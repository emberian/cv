//! `cv pack` — compile a context bundle for a new task from the session corpus.
//!
//! Pipeline: **recall** (full-text + live suffix via [`cv_search::Searcher`], plus semantic when
//! embeddings exist, fused by reciprocal rank and deduped by session) → **distill** (an extractive
//! digest per recalled span: the best-matching message window plus the session's file-edit/command
//! events from the catalog — no LLM, no network) → **emit** (`md` | `prompt` | `session`).
//!
//! Streaming discipline: recall works off the index + live suffix, excerpts are pulled with a
//! windowed streaming sink under [`Demand::with_spans`], and events come from catalog queries —
//! whole sessions are never materialized.

use crate::util::{home_rel, short_id};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use cv_core::ir::{Block, Harness, Message, Role, Session};
use cv_core::lazy::Resolver;
use cv_core::scan::floor_char;
use cv_core::stream::{Demand, Flow, MessageSink};
use cv_core::EmitOptions;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// `--excerpt-bytes` default: the bytes of one message an excerpt shows. A pack is a bounded
/// bundle by design; its header states every bound below and what each one cut.
pub(crate) const EXCERPT_BYTES: usize = 1600;
/// How many distinct files to keep per source section (curation over completeness).
const MAX_FILES_PER_SOURCE: usize = 8;
/// Rows of the cross-source file rollup.
const MAX_ROLLUP: usize = 12;

pub(crate) struct PackArgs<'a> {
    pub task: &'a str,
    pub format: &'a str,
    pub harness: Option<String>,
    pub limit: usize,
    pub out: Option<PathBuf>,
    pub force: bool,
    pub thinking: cv_core::emit::ThinkingMode,
    pub json: bool,
    /// `--excerpt-bytes`: the bytes of one message an excerpt shows.
    pub excerpt_bytes: usize,
}

pub(crate) fn cmd_pack(args: PackArgs<'_>) -> Result<()> {
    let PackArgs {
        task,
        format,
        harness,
        limit,
        out,
        force,
        thinking,
        json,
        excerpt_bytes,
    } = args;
    if let (Some(path), "md" | "prompt") = (&out, format) {
        crate::util::user_file_claim(path, force)?;
    }
    let to_h = match (format, &harness) {
        ("session", Some(s)) => Some(Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?),
        ("session", None) => bail!("--format session needs --harness <harness> (e.g. --harness claude)"),
        (_, Some(_)) => bail!("--harness only applies to --format session"),
        _ => None,
    };
    let limit = limit.max(1);

    // 1. RECALL — the most relevant past sessions for the task.
    let hits = recall(task, limit)?;
    if hits.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"task": task, "format": format, "sources": [], "content": null, "path": null})
            );
        } else {
            println!("no relevant past sessions for {task:?} — nothing to pack");
        }
        return Ok(());
    }

    // 2. DISTILL — one structured digest per recalled span.
    let mut sources: Vec<PackSource> = Vec::with_capacity(hits.len());
    for hit in hits {
        match build_source(&hit, task, excerpt_bytes) {
            Ok(s) => sources.push(s),
            Err(e) => eprintln!("cv pack: skipping {} ({}): {e:#}", short_id(&hit.id), hit.harness),
        }
    }
    if sources.is_empty() {
        bail!("recall matched sessions but none could be read back from disk (stale index? run `cv index`)");
    }
    let (rollup, rollup_dropped) = rollup_files(&sources);
    let bounds = Bounds {
        excerpt_bytes,
        rollup_dropped,
    };

    // 3. EMIT.
    match format {
        "md" => write_text(
            &md_bundle(task, &sources, &rollup, &bounds),
            out,
            force,
            "context pack",
            json,
        ),
        "prompt" => write_text(
            &prompt_bundle(task, &sources, &rollup, &bounds),
            out,
            force,
            "context prompt",
            json,
        ),
        _ => emit_pack_session(
            task,
            &sources,
            &rollup,
            &bounds,
            SessionOut {
                harness: to_h.expect("validated above"),
                out,
                thinking,
                json,
            },
        ),
    }
}

// ------------------------------------------------------------------------------------------------
// 1. RECALL
// ------------------------------------------------------------------------------------------------

/// Find the `limit` most relevant sessions: full-text + live suffix (`cv_search::Searcher`) and
/// semantic (embeddings) when their store exists, fused by reciprocal rank and deduped by session.
/// Fusion weights: lexical hits on the task's own terms are higher-precision than static-embedding
/// cosine, so the full-text list counts double; semantic stays the recall-broadening channel
/// (a session ranked in BOTH lists still gets both contributions).
const FTS_WEIGHT: f32 = 2.0;
#[cfg(test)]
const SEM_WEIGHT: f32 = 1.0;

fn recall(task: &str, limit: usize) -> Result<Vec<cv_search::Hit>> {
    let fetch = limit.saturating_mul(4).max(20);
    let searcher = cv_search::Searcher;
    let mut ranked: Vec<(f32, Vec<cv_search::Hit>)> = Vec::new();

    match fts_recall_in(&searcher, &cv_search::default_tantivy_dir(), task, fetch) {
        Ok(found) => {
            for note in &found.notes {
                match note {
                    cv_search::Note::Live(s) if s.starts_with("no index yet") => {
                        eprintln!("(live: {s}; run `cv index` for instant packs)");
                    }
                    cv_search::Note::Live(s) => eprintln!("(live: {s})"),
                    cv_search::Note::Stale(s) | cv_search::Note::Unscanned(s) => eprintln!("(note: {s})"),
                    cv_search::Note::SnippetsUnavailable(_) | cv_search::Note::NoSubagents => {}
                }
            }
            if !found.hits.is_empty() {
                ranked.push((FTS_WEIGHT, found.hits));
            }
        }
        Err(e) => eprintln!("(full-text recall unavailable: {e:#})"),
    }

    Ok(fuse(ranked, limit))
}

/// Run full-text + live-suffix recall for a task description. Task prose routinely carries bare
/// colons (`~/dev/foo: bar`, `error[E0433]:`), brackets, or boolean keywords (`NOT`, `TO`) that
/// Tantivy's `QueryParser` treats as field/range syntax; when `QueryParser` rejects the raw task,
/// fall back to its sanitized literal terms, and when conjunctive matching finds nothing, relax
/// multi-word tasks to `OR`.
fn fts_recall_in(searcher: &cv_search::Searcher, dir: &Path, task: &str, fetch: usize) -> Result<cv_search::Found> {
    let filter = cv_search::SearchFilter::default();
    let initial = match searcher.search_in(dir, task, filter, fetch, cv_search::Source::Index, None) {
        Ok(found) => found,
        Err(e) if cv_search::is_invalid_text_query(&e) => {
            let lit = literal_query(task);
            if lit.is_empty() {
                return Ok(cv_search::Found {
                    hits: Vec::new(),
                    source: cv_search::Source::Index,
                    notes: Vec::new(),
                    total: 0,
                });
            }
            searcher.search_in(dir, &lit, filter, fetch, cv_search::Source::Index, None)?
        }
        Err(e) => return Err(e),
    };
    if !initial.hits.is_empty() {
        return Ok(initial);
    }
    if let Some(q) = or_query(task) {
        if let Ok(relaxed) = searcher.search_in(dir, &q, filter, fetch, cv_search::Source::Index, None) {
            if !relaxed.hits.is_empty() {
                return Ok(relaxed);
            }
        }
    }
    Ok(initial)
}

/// Strip Tantivy query syntax (`:`, `[...]`, `(...)`, leading `-`, quotes, and uppercase boolean
/// keywords like `AND`/`OR`/`NOT`/`TO`/`IN`) down to lowercase literal tokens.
fn literal_query(task: &str) -> String {
    let mut cleaned = String::with_capacity(task.len());
    for c in task.chars() {
        if c.is_alphanumeric() || c == '_' {
            for lc in c.to_lowercase() {
                cleaned.push(lc);
            }
        } else {
            cleaned.push(' ');
        }
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// English filler that would make an OR query match everything.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "our", "your", "how", "what", "when", "where", "why",
    "are", "was", "were", "has", "have", "had", "not", "but", "all",
];

/// Distinct lowercase identifier/word tokens (preserving `_`, dropping punctuation and stopwords)
/// used for relaxed `OR` recall and length-normalized excerpt window scoring.
fn query_tokens(task: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    literal_query(task)
        .split_whitespace()
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(w) && seen.insert((*w).to_string()))
        .map(str::to_string)
        .collect()
}

/// The task's distinct searchable words (lowercased so Tantivy never reads `NOT`/`TO`/`AND` as
/// operators) joined with `OR` — the relaxed second-try query. `None` when relaxing can't help.
fn or_query(task: &str) -> Option<String> {
    let words = query_tokens(task);
    (words.len() > 1).then(|| words.join(" OR "))
}

/// Weighted reciprocal-rank fusion across result lists (BM25 and cosine scores aren't comparable;
/// ranks are). Dedup by (harness, id), preferring the variant that carries a snippet/dates; then
/// near-duplicate sessions — same title in the same cwd, e.g. a templated prompt run many times —
/// collapse to their best-ranked representative (curation over completeness).
fn fuse(ranked: Vec<(f32, Vec<cv_search::Hit>)>, limit: usize) -> Vec<cv_search::Hit> {
    if limit == 0 {
        return Vec::new();
    }
    let mut fused: HashMap<(String, String), f32> = HashMap::new();
    let mut best: HashMap<(String, String), cv_search::Hit> = HashMap::new();
    for (weight, list) in ranked {
        let mut seen = HashSet::new();
        let mut rank = 0;
        for h in list {
            let key = (h.harness.clone(), h.id.clone());
            // One session contributes once per recall channel; multiple fragments must not
            // inflate its weight or push later distinct sessions down the session ranking.
            if seen.insert(key.clone()) {
                *fused.entry(key.clone()).or_insert(0.0) += weight / (60.0 + rank as f32);
                rank += 1;
            }
            match best.get_mut(&key) {
                None => {
                    best.insert(key, h);
                }
                Some(e) => {
                    // Fill gaps from the other list's variant of the same session.
                    if e.snippet.trim().is_empty() && !h.snippet.trim().is_empty() {
                        e.snippet = h.snippet;
                    }
                    e.title = e.title.take().or(h.title);
                    e.cwd = e.cwd.take().or(h.cwd);
                    e.created_at = e.created_at.or(h.created_at);
                    e.updated_at = e.updated_at.or(h.updated_at);
                }
            }
        }
    }
    let mut out: Vec<(f32, cv_search::Hit)> = best
        .into_iter()
        .map(|(key, mut h)| {
            let s = fused[&key];
            h.score = s;
            (s, h)
        })
        .collect();
    out.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.harness.cmp(&b.1.harness))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    // Near-duplicate suppression: one source per (title, cwd). Untitled sessions never collapse.
    let mut seen_titles: HashSet<(String, Option<String>)> = HashSet::new();
    let mut picked = Vec::with_capacity(limit.min(out.len()));
    for (_, h) in out {
        if let Some(t) = h.title.as_deref().filter(|t| !t.trim().is_empty()) {
            if !seen_titles.insert((t.to_lowercase(), h.cwd.clone())) {
                continue;
            }
        }
        picked.push(h);
        if picked.len() >= limit {
            break;
        }
    }
    picked
}

/// Length-normalized BM25-style lexical overlap score (scaled to fixed-point `i64` for
/// [`cv_core::api::MessageWindow`]), preserving `_` inside identifier tokens so snake_case symbols
/// match whole identifiers and long log dumps do not drown concise answers.
fn lexical_score<'a>(text: &str, needle: &str, words: impl Iterator<Item = &'a str>) -> i64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    const AVG_DL: f64 = 40.0;

    let mut tf: HashMap<&str, usize> = HashMap::new();
    let mut doc_len: usize = 0;
    for tok in text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
    {
        doc_len += 1;
        *tf.entry(tok).or_insert(0) += 1;
    }
    if doc_len == 0 {
        return 0;
    }
    let norm = 1.0 - B + B * (doc_len as f64 / AVG_DL);
    let mut score = 0.0f64;
    for word in words {
        let count = tf.get(word).copied().unwrap_or(0) as f64;
        let eff_tf = if count > 0.0 {
            count
        } else if !word.contains('_') && word.len() >= 4 && text.contains(word) {
            0.35
        } else {
            0.0
        };
        if eff_tf > 0.0 {
            score += (eff_tf * (K1 + 1.0)) / (eff_tf + K1 * norm);
        }
    }
    let trimmed_needle = needle.trim();
    if !trimmed_needle.is_empty() && text.contains(trimmed_needle) {
        score += (2.5 * (K1 + 1.0)) / (1.0 + K1 * norm);
    }
    (score * 10_000.0).round() as i64
}

// ------------------------------------------------------------------------------------------------
// 2. DISTILL
// ------------------------------------------------------------------------------------------------

/// A ~3-message excerpt window: `(role, text, cut)` per message, `cut` when the text ends at
/// `--excerpt-bytes` (marked `…`).
type Excerpt = Vec<(Role, String, bool)>;

/// One recalled session, distilled: identity off the hit, the best-matching excerpt window, and
/// the catalog's record of what the session actually DID.
struct PackSource {
    hit: cv_search::Hit,
    /// Window around the best match of the task.
    excerpt: Excerpt,
    /// 0-based `[lo, hi)` message window the excerpt came from — the `cv show --range` hint.
    excerpt_range: Option<(usize, usize)>,
    /// `path → edit count`, from `file_edit` events.
    edits: BTreeMap<String, i64>,
    /// `path → read count`, from `file_read` events.
    reads: BTreeMap<String, i64>,
    /// Distinct commands run, in first-seen order (whole, no arbitrary command count/char cap).
    commands: Vec<String>,
}

fn build_source(hit: &cv_search::Hit, task: &str, excerpt_bytes: usize) -> Result<PackSource> {
    let want = Harness::parse(&hit.harness);
    let (r, adapter) =
        cv_core::find(&hit.id, want)?.with_context(|| format!("session {} no longer on disk", short_id(&hit.id)))?;

    // Stale events ride the excerpt pass; event failure remains best effort.
    let (excerpt, excerpt_range) = best_window_and_ingest(adapter.as_ref(), &r, task, excerpt_bytes)?;
    let rows = cv_core::events::events_for(r.harness.as_str(), &r.id, None)?;
    let EventFacts { edits, reads, commands } = event_facts(&rows);

    Ok(PackSource {
        hit: hit.clone(),
        excerpt,
        excerpt_range,
        edits,
        reads,
        commands,
    })
}

#[derive(Debug, PartialEq)]
struct EventFacts {
    edits: BTreeMap<String, i64>,
    reads: BTreeMap<String, i64>,
    commands: Vec<String>,
}

fn event_facts(rows: &[cv_core::events::EventRow]) -> EventFacts {
    let mut edits: BTreeMap<String, i64> = BTreeMap::new();
    let mut reads: BTreeMap<String, i64> = BTreeMap::new();
    let mut commands: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for e in rows {
        match e.kind.as_str() {
            "file_edit" => {
                if let Some(t) = &e.target {
                    *edits.entry(t.clone()).or_insert(0) += 1;
                }
            }
            "file_read" => {
                if let Some(t) = &e.target {
                    *reads.entry(t.clone()).or_insert(0) += 1;
                }
            }
            "command" => {
                if let Some(c) = &e.target {
                    if seen.insert(c.as_str()) {
                        commands.push(c.replace('\n', " "));
                    }
                }
            }
            _ => {}
        }
    }

    EventFacts { edits, reads, commands }
}

/// Stream the session once (lazy spans, head-capped per message) and return a compact window of
/// `(role, trimmed text)` around the message that best matches `task`, plus its `[lo, hi)` range.
fn best_window_and_ingest(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    task: &str,
    limit: usize,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    if !cv_core::events::needs_ingest(r) {
        return best_window(adapter, r, task, limit);
    }
    let source = cv_core::source::Probe::default().snapshot(r);
    let mut events = cv_core::events::SpoolingSink::new(r.cwd.clone()).with_source(r.path.clone());
    let result = best_window_observing(adapter, r, task, limit, Some(&mut events))?;
    let source = source.with_reads(events.inner().dependencies());
    // The ingest rides the read the pack needed anyway; failing it costs only a later re-ingest.
    if cv_core::source::unchanged(r, &source) {
        let recorded = events
            .into_spool()
            .and_then(|spool| cv_core::events::record_spooled_with_source(r, 0, spool, &source, &Default::default()));
        if let Err(error) = recorded {
            cv_core::env::warn_once("the event catalog", format_args!("{error:#}"));
        }
    }
    Ok(result)
}

fn best_window(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::ir::SessionRef,
    task: &str,
    limit: usize,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    best_window_observing(adapter, r, task, limit, None)
}

fn best_window_observing(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    task: &str,
    limit: usize,
    events: Option<&mut cv_core::events::SpoolingSink>,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    let needle = task.to_lowercase();
    let words = query_tokens(task);

    struct WindowSink<'a> {
        events: Option<&'a mut cv_core::events::SpoolingSink>,
        events_failed: bool,
        words: Vec<String>,
        needle: String,
        resolver: Resolver,
        limit: usize,
        window: cv_core::api::MessageWindow<(Role, String, bool)>,
        error: Option<anyhow::Error>,
    }
    impl MessageSink for WindowSink<'_> {
        fn meta(&mut self, session: &Session) {
            if !self.events_failed {
                if let Some(events) = &mut self.events {
                    events.meta(session);
                }
            }
        }
        fn dependency(&mut self, path: &Path) {
            if !self.events_failed {
                if let Some(events) = &mut self.events {
                    events.dependency(path);
                }
            }
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.error.take()
        }
        fn message(&mut self, m: Message) -> Flow {
            if !self.events_failed {
                if let Some(events) = &mut self.events {
                    self.events_failed = events.observe_message(&m) == Flow::Stop;
                }
            }
            if m.role == Role::Tool {
                self.window.skip();
                return Flow::Continue;
            }
            let (text, cut) = match capped_text(&m, &self.resolver, self.limit) {
                Ok(text) => text,
                Err(error) => {
                    self.error = Some(error);
                    return Flow::Stop;
                }
            };
            if text.trim().is_empty() {
                self.window.skip();
                return Flow::Continue;
            }
            let low = text.to_lowercase();
            let score = lexical_score(&low, &self.needle, self.words.iter().map(String::as_str));
            self.window.scored((m.role, text, cut), score);
            Flow::Continue
        }
    }

    let mut sink = WindowSink {
        events,
        events_failed: false,
        words,
        needle,
        resolver: Resolver::new(Some(r.path.clone())),
        limit,
        window: cv_core::api::MessageWindow::best(),
        error: None,
    };
    adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut sink)?;
    let selected = sink.window.finish();
    let range = selected
        .first()
        .zip(selected.last())
        .map(|((lo, _), (hi, _))| (*lo, hi + 1));
    Ok((selected.into_iter().map(|(_, value)| value).collect(), range))
}

/// A message's text blocks, span-aware, as an excerpt of at most `limit` bytes: the whole text
/// when it fits, else its first `limit` bytes with any open code fence closed and `…` appended
/// (`true`). No more than `limit + 1` bytes are resolved, so a giant pasted log is never read whole.
fn capped_text(m: &Message, resolver: &Resolver, limit: usize) -> Result<(String, bool)> {
    let cap = limit.saturating_add(1);
    let mut s = String::new();
    let mut more = false;
    for b in &m.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            let remaining = cap.saturating_sub(s.len());
            if remaining == 0 {
                more = true;
                break;
            }
            let prefix = text.try_resolve_prefix(resolver, remaining as u64)?;
            s.push_str(&prefix[..floor_char(&prefix, prefix.len().min(remaining))]);
        }
    }
    Ok(trim_excerpt(&s, limit, more || s.len() > limit))
}

/// Trim to `limit` bytes when `cut` (or longer): the head, cut on a character boundary, an open
/// code fence closed explicitly so the result renders as markdown, then `…`.
fn trim_excerpt(s: &str, limit: usize, cut: bool) -> (String, bool) {
    let s = s.trim();
    if !cut && s.len() <= limit {
        return (s.to_string(), false);
    }
    let mut out = s[..floor_char(s, s.len().min(limit))].trim_end().to_string();
    if out.is_empty() {
        // Nothing visible within the limit (a whitespace-padded blob): no excerpt line at all.
        return (out, true);
    }
    let fences = out.lines().filter(|line| line.trim_start().starts_with("```")).count();
    if fences % 2 == 1 {
        out.push_str("\n```");
    }
    out.push_str("\n…");
    (out, true)
}

/// Cross-source rollup: `(path, sessions touching it, edits, reads)` ranked by how many sources
/// it appears in, then by edit count — "the files that keep appearing".
fn rollup_files(sources: &[PackSource]) -> (Vec<(String, usize, i64, i64)>, usize) {
    let mut acc: BTreeMap<String, (HashSet<usize>, i64, i64)> = BTreeMap::new();
    for (si, s) in sources.iter().enumerate() {
        for (p, n) in &s.edits {
            let e = acc.entry(p.clone()).or_default();
            e.0.insert(si);
            e.1 += n;
        }
        for (p, n) in &s.reads {
            let e = acc.entry(p.clone()).or_default();
            e.0.insert(si);
            e.2 += n;
        }
    }
    let mut rows: Vec<(String, usize, i64, i64)> =
        acc.into_iter().map(|(p, (sess, e, r))| (p, sess.len(), e, r)).collect();
    rows.sort_by_key(|r| std::cmp::Reverse((r.1, r.2, r.3)));
    let dropped = rows.len().saturating_sub(MAX_ROLLUP);
    rows.truncate(MAX_ROLLUP);
    (rows, dropped)
}

/// The bundle's bounds and what the whole bundle cut under them.
struct Bounds {
    excerpt_bytes: usize,
    rollup_dropped: usize,
}

/// The header line naming every bound of a pack and what each cut, e.g. `Bounds: each excerpt
/// message to 1600 bytes (--excerpt-bytes; 2 cut, marked …), …`.
fn bounds_line(sources: &[PackSource], bounds: &Bounds) -> String {
    let excerpts_cut: usize = sources
        .iter()
        .map(|s| s.excerpt.iter().filter(|(_, _, cut)| *cut).count())
        .sum();
    let files_dropped: usize = sources.iter().map(files_dropped).sum();
    format!(
        "Bounds: each excerpt message to {} bytes (`--excerpt-bytes`; {excerpts_cut} cut, marked …); \
         {MAX_FILES_PER_SOURCE} files per source ({files_dropped} left out); {MAX_ROLLUP} rollup files ({} left out).",
        bounds.excerpt_bytes, bounds.rollup_dropped
    )
}

/// Distinct files a source touched that [`files_line`] leaves out.
fn files_dropped(s: &PackSource) -> usize {
    let distinct = s.edits.len() + s.reads.keys().filter(|p| !s.edits.contains_key(*p)).count();
    distinct.saturating_sub(MAX_FILES_PER_SOURCE)
}

// ------------------------------------------------------------------------------------------------
// 3. EMIT
// ------------------------------------------------------------------------------------------------

/// `YYYY-MM-DD` off the hit's dates, or a dashed placeholder.
fn hit_date(hit: &cv_search::Hit) -> String {
    hit.updated_at
        .or(hit.created_at)
        .and_then(|t| crate::util::fmt_local_ts(t, "%Y-%m-%d"))
        .unwrap_or_else(|| "----------".into())
}

fn hit_cwd(hit: &cv_search::Hit) -> String {
    hit.cwd
        .as_deref()
        .map(|c| home_rel(Path::new(c)))
        .unwrap_or_else(|| "(no cwd)".into())
}

/// `path (3 edits)` / `path (read)` listing for one source, edits first, capped.
fn files_line(s: &PackSource) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for (p, n) in &s.edits {
        if parts.len() >= MAX_FILES_PER_SOURCE {
            break;
        }
        let times = if *n == 1 { "1 edit".into() } else { format!("{n} edits") };
        parts.push(format!("`{p}` ({times})"));
    }
    for p in s.reads.keys() {
        if parts.len() >= MAX_FILES_PER_SOURCE {
            break;
        }
        if !s.edits.contains_key(p) {
            parts.push(format!("`{p}` (read)"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The `md` format: a CLAUDE.md-style bundle you can paste into project memory.
fn md_bundle(task: &str, sources: &[PackSource], rollup: &[(String, usize, i64, i64)], bounds: &Bounds) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Context pack: {task}\n");
    let _ = writeln!(
        out,
        "Compiled by `cv pack` from {} past session(s) · {}. Extractive — every line below comes \
         from a real transcript or the event catalog.\n",
        sources.len(),
        Utc::now().format("%Y-%m-%d")
    );
    let _ = writeln!(out, "{}\n", bounds_line(sources, bounds));

    for s in sources {
        let title = s.hit.title.as_deref().unwrap_or("(untitled)");
        let _ = writeln!(
            out,
            "## {title} ({}, {}, {})\n",
            s.hit.harness,
            hit_date(&s.hit),
            hit_cwd(&s.hit)
        );
        if !s.excerpt.is_empty() {
            let _ = writeln!(out, "Key excerpt:\n");
            for (role, text, _) in &s.excerpt {
                let _ = writeln!(out, "**{}:**", cv_core::render::role_label(*role));
                let _ = writeln!(out, "{text}\n");
            }
        }
        if let Some(files) = files_line(s) {
            let _ = writeln!(out, "Files touched: {files}\n");
        }
        if !s.commands.is_empty() {
            let cmds: Vec<String> = s.commands.iter().map(|c| format!("`{c}`")).collect();
            let _ = writeln!(out, "Commands run: {}\n", cmds.join(", "));
        }
        let more = match s.excerpt_range {
            Some((lo, hi)) => format!("cv show {}:{} --range {lo}..{hi}", s.hit.harness, s.hit.id),
            None => format!("cv show {}:{}", s.hit.harness, s.hit.id),
        };
        let _ = writeln!(out, "(full context: `{more}`)\n");
    }

    if !rollup.is_empty() {
        let _ = writeln!(out, "## Files that keep appearing\n");
        for (p, sess, e, r) in rollup {
            let mut counts: Vec<String> = Vec::new();
            if *e > 0 {
                counts.push(format!("{e} edit(s)"));
            }
            if *r > 0 {
                counts.push(format!("{r} read(s)"));
            }
            let _ = writeln!(out, "- `{p}` — {sess} session(s), {}", counts.join(", "));
        }
    }
    out
}

/// The `prompt` format: the same content shaped as a system prompt (second person).
fn prompt_bundle(task: &str, sources: &[PackSource], rollup: &[(String, usize, i64, i64)], bounds: &Bounds) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "You are picking up work on: {task}\n");
    let _ = writeln!(
        out,
        "Prior context from earlier sessions: you have worked on this ground before. What follows \
         is recalled from {} of your past session(s).\n",
        sources.len()
    );
    let _ = writeln!(out, "{}\n", bounds_line(sources, bounds));
    for (i, s) in sources.iter().enumerate() {
        let title = s.hit.title.as_deref().unwrap_or("(untitled)");
        let _ = writeln!(
            out,
            "{}. In \"{title}\" ({}, {}, in {}):",
            i + 1,
            s.hit.harness,
            hit_date(&s.hit),
            hit_cwd(&s.hit)
        );
        for (role, text, _) in &s.excerpt {
            let _ = writeln!(out, "   {} said:", cv_core::render::role_label(*role));
            for line in text.lines() {
                let _ = writeln!(out, "   > {line}");
            }
        }
        if let Some(files) = files_line(s) {
            let _ = writeln!(out, "   You touched: {files}");
        }
        if !s.commands.is_empty() {
            let cmds: Vec<String> = s.commands.iter().map(|c| format!("`{c}`")).collect();
            let _ = writeln!(out, "   You ran: {}", cmds.join(", "));
        }
        out.push('\n');
    }
    if !rollup.is_empty() {
        let files: Vec<String> = rollup.iter().map(|(p, ..)| format!("`{p}`")).collect();
        let _ = writeln!(
            out,
            "The files that kept appearing across these sessions: {}.\n",
            files.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "Treat this as your own working memory: prefer the locations, commands, and decisions \
         established above instead of re-deriving them. Verify paths against the current tree \
         before editing."
    );
    out
}

/// Where and how `--format session` writes: the target harness, `--out`, `--thinking`, `--json`.
struct SessionOut {
    harness: Harness,
    out: Option<PathBuf>,
    thinking: cv_core::emit::ThinkingMode,
    json: bool,
}

/// The `session` format: a synthetic resumable session in the target harness — one user message
/// framing the task with the full bundle, plus a short assistant acknowledgment so the resumed
/// harness opens on a clean turn boundary. Emitted through the same `cv_core::emit` path
/// `cv port` uses (prints the written path + resume hint).
fn emit_pack_session(
    task: &str,
    sources: &[PackSource],
    rollup: &[(String, usize, i64, i64)],
    bounds: &Bounds,
    target: SessionOut,
) -> Result<()> {
    let SessionOut {
        harness: to_h,
        out,
        thinking,
        json,
    } = target;
    let bundle = md_bundle(task, sources, rollup, bounds);
    let now = Utc::now();

    let mut context = Message::of_kind(
        Role::System,
        cv_core::ir::MessageKind::InjectedContext,
        cv_core::ir::Origin::Harness,
    );
    context.timestamp = Some(now);
    context.content.push(Block::Text {
        text: format!("Relevant context from earlier sessions, compiled by `cv pack`:\n\n{bundle}").into(),
    });

    let mut prompt = Message::of_kind(Role::User, cv_core::ir::MessageKind::Prompt, cv_core::ir::Origin::Human);
    prompt.timestamp = Some(now);
    prompt.content.push(Block::Text {
        text: format!("I'm picking up this task: {task}").into(),
    });

    let session = Session {
        cwd: std::env::current_dir().ok(),
        title: Some(format!("context pack: {}", cv_core::ir::truncate(task, 60))),
        created_at: Some(now),
        updated_at: Some(now),
        messages: vec![context, prompt],
        ..Session::new(
            format!("pack-{}", now.timestamp()),
            Harness::parse("opensession").expect("opensession"),
        )
    };
    crate::cmd::port::emit_session_output(
        &session,
        to_h,
        out,
        EmitOptions {
            thinking,
            ..Default::default()
        },
        json,
    )
    .map(drop)
}

/// Write a text bundle to `--out` (with a stderr note) or stdout.
fn write_text(text: &str, out: Option<PathBuf>, force: bool, what: &str, json: bool) -> Result<()> {
    match out {
        Some(path) => {
            crate::util::write_user_file(&path, text.as_bytes(), force)?;
            if json {
                println!("{}", serde_json::json!({"path": path, "content": null}));
            } else {
                eprintln!("✦ wrote {what} → {}", path.display());
            }
        }
        None if json => println!("{}", serde_json::json!({"path": null, "content": text})),
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::Adapter;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountedAdapter {
        inner: Box<dyn cv_core::Adapter>,
        calls: AtomicUsize,
        mutate: Option<PathBuf>,
        dependency: Option<PathBuf>,
    }
    impl cv_core::Adapter for CountedAdapter {
        fn harness(&self) -> Harness {
            self.inner.harness()
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<cv_core::Discovered> {
            Ok(Default::default())
        }
        fn stream(&self, r: &cv_core::SessionRef, options: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            struct Counter<'a> {
                sink: &'a mut dyn MessageSink,
                calls: &'a AtomicUsize,
            }
            impl MessageSink for Counter<'_> {
                fn meta(&mut self, s: &Session) {
                    self.sink.meta(s);
                }
                fn dependency(&mut self, path: &Path) {
                    self.sink.dependency(path);
                }
                fn take_error(&mut self) -> Option<anyhow::Error> {
                    self.sink.take_error()
                }
                fn message(&mut self, m: Message) -> Flow {
                    self.calls.fetch_add(1, Ordering::Relaxed);
                    self.sink.message(m)
                }
            }
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            let result = self.inner.try_stream(
                r,
                options,
                &mut Counter {
                    sink,
                    calls: &self.calls,
                },
            )?;
            if let Some(path) = &self.mutate {
                use std::io::Write;
                std::fs::OpenOptions::new().append(true).open(path)?.write_all(b" ")?;
            }
            Ok(result)
        }
    }
    fn stale(path: &Path) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
    }
    fn rows(r: &cv_core::SessionRef) -> Vec<cv_core::events::EventRow> {
        cv_core::events::events_for(r.harness.as_str(), &r.id, None).unwrap()
    }

    #[test]
    fn shared_pack_pass_matches_two_pass_excerpt_rows_and_facts_with_cold_and_warm_counts() -> Result<()> {
        let world = crate::test_world("pack-shared-real");
        let path = world.base.join("real.opensession.json");
        let mut session = Session::new("real", Harness::OpenSession);
        session.cwd = Some(world.base.clone());
        for (role, text, tool, input) in [
            (Role::User, "before", "Read", serde_json::json!({"file_path":"file.rs"})),
            (
                Role::Assistant,
                "chosen task",
                "Edit",
                serde_json::json!({"file_path":"file.rs", "old_string":"before", "new_string":"after"}),
            ),
            (
                Role::User,
                "after",
                "Bash",
                serde_json::json!({"command":"cargo check"}),
            ),
        ] {
            let mut m = Message::new(role);
            m.content.push(Block::Text { text: text.into() });
            m.content.push(Block::ToolUse {
                id: "call".into(),
                name: tool.into(),
                namespace: None,
                input: input.into(),
            });
            session.messages.push(m);
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&cv_core::harness::opensession::document(&session))?,
        )?;
        let r = cv_core::SessionRef::new("real", Harness::OpenSession, &path);
        let mut adapter = CountedAdapter {
            inner: cv_core::harness::for_harness(Harness::OpenSession).unwrap(),
            calls: Default::default(),
            mutate: None,
            dependency: None,
        };
        let expected = best_window(&adapter, &r, "chosen", EXCERPT_BYTES)?;
        // The old independent ingestion traversal, through the same real adapter.
        let source = cv_core::source::Probe::default().snapshot(&r);
        let mut events = cv_core::events::SpoolingSink::new(r.cwd.clone()).with_source(r.path.clone());
        adapter.try_stream(&r, &Demand::LEAN.with_spans(), &mut events)?;
        let source = source.with_reads(events.inner().dependencies());
        cv_core::events::record_spooled_with_source(&r, 0, events.into_spool()?, &source, &Default::default())?;
        assert_eq!(adapter.calls.load(Ordering::Relaxed), 2 * session.messages.len());
        let expected_rows = format!("{:?}", rows(&r));
        let expected_facts = event_facts(&rows(&r));
        assert!(!expected_facts.reads.is_empty());
        assert!(!expected_facts.edits.is_empty());
        assert!(!expected_facts.commands.is_empty());
        stale(&path);
        adapter.calls.store(0, Ordering::Relaxed);
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen", EXCERPT_BYTES)?, expected);
        assert_eq!(adapter.calls.load(Ordering::Relaxed), session.messages.len());
        assert_eq!(format!("{:?}", rows(&r)), expected_rows);
        assert_eq!(event_facts(&rows(&r)), expected_facts);
        assert!(!cv_core::events::needs_ingest(&r));
        let conn = rusqlite::Connection::open(world.cv_home.join("catalog.db"))?;
        conn.execute_batch("CREATE TABLE pack_event_writes(n INTEGER); INSERT INTO pack_event_writes VALUES(0); CREATE TRIGGER count_pack_events AFTER INSERT ON events BEGIN UPDATE pack_event_writes SET n=n+1; END;")?;
        adapter.calls.store(0, Ordering::Relaxed);
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen", EXCERPT_BYTES)?, expected);
        assert_eq!(adapter.calls.load(Ordering::Relaxed), session.messages.len());
        assert_eq!(event_facts(&rows(&r)), expected_facts);

        assert_eq!(
            conn.query_row("SELECT n FROM pack_event_writes", [], |row| row.get::<_, i64>(0))?,
            0
        );
        // Make the next candidate event rows observably different from the published baseline.
        let changed = std::fs::read_to_string(&path)?.replace("file.rs", "changed.rs");
        std::fs::write(&path, changed)?;
        // Mid-pass primary/dependency changes do not replace previously published rows.
        let dependency = world.base.join("aux");
        std::fs::write(&dependency, "baseline")?;
        for mutation in [&path, &dependency] {
            stale(&path);
            adapter.dependency = Some(dependency.clone());
            adapter.mutate = Some(mutation.clone());
            assert_eq!(best_window_and_ingest(&adapter, &r, "chosen", EXCERPT_BYTES)?, expected);
            assert_eq!(format!("{:?}", rows(&r)), expected_rows);
            assert!(cv_core::events::needs_ingest(&r));
        }
        adapter.mutate = None;
        // Publication failure rolls back rows without failing the successful excerpt.
        let conn = rusqlite::Connection::open(world.cv_home.join("catalog.db"))?;
        conn.execute_batch(
            "CREATE TRIGGER reject_pack_events BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'pack failure'); END;",
        )?;
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen", EXCERPT_BYTES)?, expected);
        assert_eq!(format!("{:?}", rows(&r)), expected_rows);
        assert!(cv_core::events::needs_ingest(&r));
        std::fs::write(&path, b"{broken transcript")?;
        assert!(best_window_and_ingest(&adapter, &r, "chosen", EXCERPT_BYTES).is_err());
        assert_eq!(format!("{:?}", rows(&r)), expected_rows);
        Ok(())
    }

    #[test]
    fn optional_event_extraction_failure_keeps_excerpt_and_never_publishes_partial_rows() -> Result<()> {
        let world = crate::test_world("pack-event-failure");
        let path = world.base.join("source");
        std::fs::write(&path, "source")?;
        let r = cv_core::SessionRef::new("event-failure", Harness::OpenSession, &path);
        let payload = world.base.join("removed.json");
        std::fs::write(&payload, b"{\"file_path\":\"late\"}")?;
        let input = cv_core::Json::source(&payload, 0, std::fs::metadata(&payload)?.len())?;
        std::fs::remove_file(&payload)?;
        let mut good = Message::new(Role::Assistant);
        good.content.push(Block::Text {
            text: "chosen excerpt".into(),
        });
        good.content.push(Block::ToolUse {
            id: "good".into(),
            name: "Read".into(),
            namespace: None,
            input: serde_json::json!({"file_path":"good"}).into(),
        });
        let mut bad = Message::new(Role::Tool);
        bad.content.push(Block::ToolUse {
            id: "bad".into(),
            name: "Read".into(),
            namespace: None,
            input,
        });
        let mut following = Message::new(Role::User);
        following.content.push(Block::Text {
            text: "tail after failed events".into(),
        });
        let fixture = ExcerptFixture(vec![good, bad, following]);
        let expected = best_window(&fixture, &r, "chosen", EXCERPT_BYTES)?;
        assert_eq!(best_window_and_ingest(&fixture, &r, "chosen", EXCERPT_BYTES)?, expected);
        assert!(rows(&r).is_empty());
        let mut unreadable = Message::new(Role::Assistant);
        unreadable.content.push(Block::Text {
            text: cv_core::Text::Span(cv_core::lazy::Span {
                source: Some(payload),
                offset: 0,
                len: 4,
                escaped: false,
            }),
        });
        assert!(best_window_and_ingest(&ExcerptFixture(vec![unreadable]), &r, "chosen", EXCERPT_BYTES).is_err());
        assert!(rows(&r).is_empty());
        assert!(cv_core::events::needs_ingest(&r));
        Ok(())
    }

    struct ExcerptFixture(Vec<Message>);
    impl cv_core::Adapter for ExcerptFixture {
        fn harness(&self) -> Harness {
            Harness::OpenSession
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<cv_core::Discovered> {
            Ok(Default::default())
        }
        fn stream(&self, r: &cv_core::SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
            for message in &self.0 {
                if sink.message(message.clone()) == Flow::Stop {
                    break;
                }
            }
            Ok(Session::new(&r.id, r.harness))
        }
    }
    #[test]
    fn pack_excerpt_inline_span_and_source_match_with_bounded_prefix() -> Result<()> {
        let world = cv_testkit::World::new("pack-excerpt-source");
        let path = world.base.join("source.opensession.json");
        let body = format!("chosen λ {} omitted-tail", "padding ".repeat(128 * 1024));
        let mut original = Session::new("source", Harness::Claude);
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Text {
            text: body.clone().into(),
        });
        original.messages.push(message.clone());
        let bytes = serde_json::to_vec(&cv_core::harness::opensession::document(&original))?;
        let offset = bytes
            .windows(body.len())
            .position(|part| part == body.as_bytes())
            .unwrap();
        std::fs::write(&path, bytes)?;
        let reference = cv_core::SessionRef::new("source", Harness::OpenSession, &path);
        let source_text = cv_core::Json::source(&path, offset as u64 - 1, body.len() as u64 + 2)?
            .with_view(|view| view.source_text()?.context("source string"))?;
        assert!(matches!(&source_text, cv_core::Text::Source(_)));
        let mut span = message.clone();
        span.content = vec![Block::Text {
            text: cv_core::Text::Span(cv_core::lazy::Span {
                source: Some(path.clone()),
                offset: offset as u64,
                len: body.len() as u64,
                escaped: false,
            }),
        }];
        let mut source = message.clone();
        source.content = vec![Block::Text { text: source_text }];
        let resolver = Resolver::new(Some(path.clone()));
        let expected_text = capped_text(&message, &resolver, EXCERPT_BYTES)?;
        assert!(!expected_text.0.contains("omitted-tail"));
        assert!(expected_text.1 && expected_text.0.len() <= EXCERPT_BYTES + "\n…".len());
        let expected_window = best_window(&ExcerptFixture(vec![message]), &reference, "chosen", EXCERPT_BYTES)?;
        for selected in [&span, &source] {
            assert_eq!(capped_text(selected, &resolver, EXCERPT_BYTES)?, expected_text);
            assert_eq!(
                best_window(
                    &ExcerptFixture(vec![selected.clone()]),
                    &reference,
                    "chosen",
                    EXCERPT_BYTES
                )?,
                expected_window
            );
        }
        std::fs::write(&path, b"changed")?;
        assert!(capped_text(&source, &resolver, EXCERPT_BYTES).is_err());
        assert!(best_window(&ExcerptFixture(vec![source]), &reference, "chosen", EXCERPT_BYTES).is_err());
        Ok(())
    }

    #[test]
    fn literal_and_or_queries_sanitize_colons_brackets_and_boolean_keywords() {
        assert_eq!(
            literal_query("fix bug in ~/dev/foo: bar [E0433] --flag NOT TO AND"),
            "fix bug in dev foo bar e0433 flag not to and"
        );
        assert_eq!(literal_query("::: [] ()"), "");
        assert_eq!(
            or_query("tantivy chunked indexing").as_deref(),
            Some("tantivy OR chunked OR indexing")
        );
        assert_eq!(or_query("tantivy"), None);
        assert_eq!(or_query("a of in"), None); // short words dropped
                                               // Punctuation stripped, lowercased, dupes collapsed.
        assert_eq!(or_query("Fix fix, the (parser)!").as_deref(), Some("fix OR parser"));
    }

    #[test]
    fn event_facts_keeps_all_distinct_commands_whole_without_magic_caps() {
        let rows: Vec<cv_core::events::EventRow> = (0..10)
            .map(|i| cv_core::events::EventRow {
                harness: "claude".into(),
                session_id: "s".into(),
                msg_idx: i,
                ts: None,
                kind: "command".into(),
                tool: Some("Bash".into()),
                target: Some(format!(
                    "cargo test --package clustervision-suite-{i} -- --nocapture {}",
                    "x".repeat(120)
                )),
                detail: None,
                agent_id: None,
                parent_id: None,
                workflow: None,
            })
            .collect();
        let facts = event_facts(&rows);
        assert_eq!(facts.commands.len(), 10, "all 10 distinct commands kept");
        assert!(facts.commands.iter().all(|c| c.len() > 150), "no 90-char truncation");
    }

    #[test]
    fn trim_excerpt_keeps_code_fences_closed() {
        // Short text passes through untouched.
        assert_eq!(trim_excerpt("hello", EXCERPT_BYTES, false), ("hello".into(), false));

        // A fence open at the limit is closed, and the cut is marked.
        let body = format!("intro\n```rust\n{}\n", "let x = 1;\n".repeat(400));
        let (out, cut) = trim_excerpt(&body, EXCERPT_BYTES, false);
        assert!(cut && out.len() <= EXCERPT_BYTES + 8, "len {}", out.len());
        assert_eq!(out.matches("```").count() % 2, 0, "unbalanced fences in:\n{out}");
        assert!(out.ends_with('…'), "{out}");

        // Prose cuts at the limit the caller chose.
        let (out, cut) = trim_excerpt(&"word ".repeat(500), 700, false);
        assert!(cut && out.len() <= 700 + 4, "len {}", out.len());
        // A text exactly at the limit is whole; one the caller knows continues is cut.
        assert!(!trim_excerpt(&"x".repeat(700), 700, false).1);
        assert!(trim_excerpt(&"x".repeat(700), 700, true).1);
    }

    fn hit(id: &str, snippet: &str) -> cv_search::Hit {
        cv_search::Hit {
            id: id.into(),
            harness: "claude".into(),
            cwd: None,
            title: None,
            score: 1.0,
            snippet: snippet.into(),
            created_at: None,
            updated_at: None,
            parent_id: None,
            agent_id: None,
            workflow: None,
            session: None,
        }
    }

    #[test]
    fn fuse_dedups_and_prefers_double_ranked() {
        // "b" appears in both lists (lower ranks), "a" tops one list only.
        let fts = vec![hit("a", "fts snippet"), hit("b", "")];
        let sem = vec![hit("b", "sem snippet"), hit("c", "")];
        let fused = fuse(vec![(1.0, fts), (1.0, sem)], 10);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].id, "b", "double-ranked session must win");
        // The empty FTS snippet was backfilled from the semantic variant.
        assert_eq!(fused[0].snippet, "sem snippet");
    }

    #[test]
    fn fuse_weights_lexical_over_semantic_and_collapses_dup_titles() {
        // FTS top hit must beat a semantic-only top hit under the 2:1 weights.
        let fts = vec![hit("relevant", "")];
        let sem = vec![hit("vibes", "")];
        let fused = fuse(vec![(FTS_WEIGHT, fts), (SEM_WEIGHT, sem)], 10);
        assert_eq!(fused[0].id, "relevant");

        // Same title + cwd → one representative; untitled sessions never collapse.
        let titled = |id: &str, title: &str| {
            let mut h = hit(id, "");
            h.title = Some(title.into());
            h
        };
        let sem = vec![
            titled("t1", "Repo summarizer"),
            titled("t2", "repo summarizer"), // case-insensitive dup
            hit("u1", ""),
            hit("u2", ""), // both untitled, both kept
        ];
        let fused = fuse(vec![(SEM_WEIGHT, sem)], 10);
        let ids: Vec<&str> = fused.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["t1", "u1", "u2"]);
    }
    #[test]
    fn fusion_counts_channels_once_and_breaks_equal_scores_deterministically() {
        let duplicated = vec![hit("a", ""), hit("a", "metadata"), hit("b", "")];
        let unique = vec![hit("a", ""), hit("b", "")];
        let compare = |fts| fuse(vec![(1.0, fts), (1.0, vec![hit("b", "")])], 10);
        let actual = compare(duplicated);
        let expected = compare(unique);
        assert_eq!(
            actual.iter().map(|h| (&h.id, h.score)).collect::<Vec<_>>(),
            expected.iter().map(|h| (&h.id, h.score)).collect::<Vec<_>>()
        );
        assert_eq!(actual[0].id, "b");
        assert_eq!(actual[1].snippet, "metadata");
        let ties = fuse(vec![(1.0, vec![hit("z", "")]), (1.0, vec![hit("a", "")])], 1);
        assert_eq!(ties[0].id, "a");
        assert!(fuse(vec![(1.0, vec![hit("a", "")])], 0).is_empty());
    }

    #[test]
    fn pack_message_budget_is_shared_across_blocks_and_hard_line_cuts_close_fences() -> Result<()> {
        let mut message = Message::new(Role::Assistant);
        message.content = (0..20)
            .map(|_| Block::Text {
                text: " ".repeat(EXCERPT_BYTES).into(),
            })
            .collect();
        message.content.push(Block::Text {
            text: "outside the message budget".into(),
        });
        assert_eq!(
            capped_text(&message, &Resolver::new(None), EXCERPT_BYTES)?,
            (String::new(), true)
        );
        for body in ["x".repeat(1500), "雪".repeat(500)] {
            let source = format!("{}\n```rust\n{body}\nstill inside\n```\n", "intro ".repeat(100));
            let (actual, cut) = trim_excerpt(&source, EXCERPT_BYTES, false);
            assert!(cut && actual.len() <= EXCERPT_BYTES + 8, "{}", actual.len());
            assert_eq!(actual.matches("```").count() % 2, 0, "{actual}");
            assert!(actual.ends_with('…'));
        }
        Ok(())
    }

    #[test]
    fn context_pack_hints_use_the_current_range_syntax_and_full_identity() {
        let source = PackSource {
            hit: hit("abcdef0123456789", ""),
            excerpt: Vec::new(),
            excerpt_range: Some((4, 9)),
            edits: BTreeMap::new(),
            reads: BTreeMap::new(),
            commands: Vec::new(),
        };
        let bounds = Bounds {
            excerpt_bytes: EXCERPT_BYTES,
            rollup_dropped: 0,
        };
        let bundle = md_bundle("task", &[source], &[], &bounds);
        assert!(bundle.contains("Bounds: each excerpt message to 1600 bytes (`--excerpt-bytes`; 0 cut"));
        assert!(bundle.contains("cv show claude:abcdef0123456789 --range 4..9"));
        assert!(!bundle.contains("--range 4-9"));
    }

    #[test]
    fn lexical_score_preserves_underscores_and_normalizes_by_document_length() {
        let words = query_tokens("fix crate::cache_invalidation_bug() in store");
        assert_eq!(words, vec!["fix", "crate", "cache_invalidation_bug", "store"]);

        let focused = "we fixed cache_invalidation_bug inside store";
        let verbose = format!("cache_invalidation_bug store {}", "unrelated filler token ".repeat(140));
        let needle = "fix crate::cache_invalidation_bug() in store";
        let focused_score = lexical_score(focused, needle, words.iter().map(String::as_str));
        let verbose_score = lexical_score(&verbose, needle, words.iter().map(String::as_str));
        assert!(
            focused_score > verbose_score,
            "length normalization must prefer concise match ({focused_score}) over 420-word log dump ({verbose_score})"
        );

        // Snake-case identifier must not falsely match a longer distinct identifier.
        let partial_only = "unrelated prefix_cache_invalidation_bug_suffix only";
        let exact_only = "isolated cache_invalidation_bug match";
        assert!(
            lexical_score(exact_only, needle, words.iter().map(String::as_str))
                > lexical_score(partial_only, needle, words.iter().map(String::as_str))
        );
        assert_eq!(
            lexical_score(partial_only, needle, ["cache_invalidation_bug"].into_iter()),
            0
        );
    }
}
