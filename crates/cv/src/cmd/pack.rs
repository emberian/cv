//! `cv pack` — compile a context bundle for a new task from the session corpus.
//!
//! Pipeline: **recall** (full-text + semantic, fused by reciprocal rank, deduped by session) →
//! **distill** (an extractive digest per recalled span: the best-matching message window plus the
//! session's file-edit/command events from the catalog — no LLM, no network) → **emit** (`md` |
//! `prompt` | `session`).
//!
//! The digest is extractive by design: titles, dates, cwds, excerpts, and event-catalog facts are
//! all things the corpus already states, so the bundle never hallucinates. Set `CV_PACK_LLM=1`
//! (or `CV_PACK_LLM=<model-id>`) to additionally route each span through `llm::digest` for an
//! abstractive digest; without it, `cv pack` does zero network and loads zero models.
//!
//! Streaming discipline: recall works off the index (or a head-capped live scan), excerpts are
//! pulled with a windowed streaming sink under [`ParseOptions::lazy`], and events come from
//! catalog queries — whole sessions are never materialized.

use crate::util::{home_rel, short_id};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use cv_core::ir::{Block, Harness, Message, Role, Session};
use cv_core::lazy::Resolver;
use cv_core::scan::{ceil_char, floor_char, CapSink};
use cv_core::stream::{Flow, MessageSink, ParseOptions};
use cv_core::EmitOptions;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Per-message resolution budget for lazy spans while hunting the excerpt window — a giant pasted
/// log contributes its head, never its body.
const MSG_RESOLVE_BYTES: u64 = 4096;
/// Soft cap on one excerpt message (chars). An open code fence may run past it…
const EXCERPT_SOFT_CHARS: usize = 700;
/// …but never past this.
const EXCERPT_HARD_CHARS: usize = 1600;
/// How many distinct commands / files to keep per source section (curation over completeness).
const MAX_COMMANDS: usize = 6;
const MAX_FILES_PER_SOURCE: usize = 8;
/// Rows of the cross-source file rollup.
const MAX_ROLLUP: usize = 12;

pub(crate) fn cmd_pack(
    task: &str,
    format: &str,
    harness: Option<String>,
    limit: usize,
    out: Option<PathBuf>,
    force: bool,
    thinking: cv_core::emit::ThinkingMode,
) -> Result<()> {
    if !matches!(format, "md" | "prompt" | "session") {
        bail!("unknown format {format:?} (use md, prompt, or session)");
    }
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
        println!("no relevant past sessions for {task:?} — nothing to pack");
        return Ok(());
    }

    // Optional abstractive pass. The clap surface for pack is frozen, so the switch is an env var:
    // CV_PACK_LLM=1 (provider default model) or CV_PACK_LLM=<model-id>.
    let llm_model = llm_from_env();
    if llm_model.is_some() {
        match crate::llm::available_provider() {
            Some(p) => eprintln!("✦ CV_PACK_LLM set — distilling each span via {p}…"),
            None => bail!(
                "CV_PACK_LLM is set but no LLM provider is configured: set OPENROUTER_API_KEY, \
                 ANTHROPIC_API_KEY, or LMSTUDIO_API_BASE=local (or unset CV_PACK_LLM for the \
                 zero-network extractive digest)"
            ),
        }
    }

    // 2. DISTILL — one structured digest per recalled span.
    let mut sources: Vec<PackSource> = Vec::with_capacity(hits.len());
    for hit in hits {
        match build_source(&hit, task, llm_model.as_ref()) {
            Ok(s) => sources.push(s),
            Err(e) => eprintln!("cv pack: skipping {} ({}): {e:#}", short_id(&hit.id), hit.harness),
        }
    }
    if sources.is_empty() {
        bail!("recall matched sessions but none could be read back from disk (stale index? run `cv index`)");
    }
    let rollup = rollup_files(&sources);

    // 3. EMIT.
    match format {
        "md" => write_text(&md_bundle(task, &sources, &rollup), out, force, "context pack"),
        "prompt" => write_text(&prompt_bundle(task, &sources, &rollup), out, force, "context prompt"),
        _ => emit_pack_session(task, &sources, &rollup, to_h.expect("validated above"), out, thinking),
    }
}

/// `CV_PACK_LLM` unset/empty/`0` → no LLM. `1`/`true` → provider default model. Anything else is
/// taken as the model id.
fn llm_from_env() -> Option<Option<String>> {
    let v = cv_core::env::var("CV_PACK_LLM").ok()?;
    let v = v.trim();
    match v {
        "" | "0" | "false" => None,
        "1" | "true" => Some(None),
        model => Some(Some(model.to_string())),
    }
}

// ------------------------------------------------------------------------------------------------
// 1. RECALL
// ------------------------------------------------------------------------------------------------

/// Find the `limit` most relevant sessions: full-text (tantivy) and semantic (embeddings) when
/// their stores exist, fused by reciprocal rank and deduped by session. Honest fallbacks: no
/// index → head-capped live scan with a stderr note; no embeddings → full-text only.
/// Fusion weights: lexical hits on the task's own terms are higher-precision than static-embedding
/// cosine, so the full-text list counts double; semantic stays the recall-broadening channel
/// (a session ranked in BOTH lists still gets both contributions).
const FTS_WEIGHT: f32 = 2.0;
const SEM_WEIGHT: f32 = 1.0;

fn recall(task: &str, limit: usize) -> Result<Vec<cv_search::Hit>> {
    let fetch = limit.saturating_mul(4).max(20);
    let mut ranked: Vec<(f32, Vec<cv_search::Hit>)> = Vec::new();

    if cv_search::default_tantivy_dir().exists() {
        // Recall consumes ranks and session IDs; it parses only the selected sources below.
        match cv_search::text_search_indexed(None, task, fetch) {
            Ok(hits) if !hits.is_empty() => ranked.push((FTS_WEIGHT, hits)),
            Ok(_) => {
                // The index parser is conjunctive (all terms must match) — right for search,
                // too strict for a task phrase. Retry as an OR of the task's words.
                if let Some(q) = or_query(task) {
                    match cv_search::text_search_indexed(None, &q, fetch) {
                        Ok(hits) if !hits.is_empty() => ranked.push((FTS_WEIGHT, hits)),
                        Ok(_) => {}
                        Err(e) => eprintln!("(full-text recall failed: {e:#})"),
                    }
                }
            }
            Err(e) => eprintln!("(full-text index unavailable: {e:#})"),
        }
    } else {
        eprintln!("(no index yet — recalling via a live scan; run `cv index` for instant packs)");
        ranked.push((FTS_WEIGHT, live_scan(task, fetch)?));
    }

    if cv_search::default_embeddings_path().exists() {
        match cv_search::semantic_search(None, task, fetch) {
            Ok(hits) => ranked.push((SEM_WEIGHT, hits)),
            Err(e) => eprintln!("(semantic recall unavailable: {e:#}; full-text only)"),
        }
    } else {
        eprintln!("(no embeddings store — full-text recall only; `cv index --semantic` adds semantic recall)");
    }

    Ok(fuse(ranked, limit))
}

/// English filler that would make an OR query match everything.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "our", "your", "how", "what", "when", "where", "why",
    "are", "was", "were", "has", "have", "had", "not", "but", "all",
];

/// The task's distinct searchable words joined with OR — the relaxed second-try query.
/// `None` when relaxing can't help (zero or one usable word).
fn or_query(task: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let words: Vec<String> = task
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .filter(|w| {
            let low = w.to_lowercase();
            w.len() > 2 && !STOPWORDS.contains(&low.as_str()) && seen.insert(low)
        })
        .collect();
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

/// No-index fallback: discover and stream every session, scoring a head-capped searchable
/// haystack against the task's words. Peak memory is O(the cap), never O(session).
fn live_scan(task: &str, fetch: usize) -> Result<Vec<cv_search::Hit>> {
    let adapters = cv_core::harness::all();
    live_scan_adapters(task, fetch, adapters.iter().map(|adapter| adapter.as_ref()))
}

fn live_scan_adapters<'a>(
    task: &str,
    fetch: usize,
    adapters: impl IntoIterator<Item = &'a dyn cv_core::Adapter>,
) -> Result<Vec<cv_search::Hit>> {
    let needle = task.to_lowercase();
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 2).collect();

    if fetch == 0 {
        return Ok(Vec::new());
    }
    let mut scored = BTreeMap::new();
    let mut ordinal = 0usize;
    for adapter in adapters {
        if adapter.roots().is_empty() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = CapSink::new(&r.path);
            let meta = adapter
                .try_stream(&r, &ParseOptions::lazy(), &mut sink)
                .with_context(|| format!("scanning {}:{} for pack", r.harness, r.id))?;
            let low = sink.hay.as_str().to_lowercase();
            let mut score = words.iter().filter(|w| low.contains(**w)).count() as i64;
            if low.contains(&needle) {
                score += 5;
            }
            if score == 0 {
                continue;
            }
            let rank = (std::cmp::Reverse(score), ordinal);
            ordinal += 1;
            // Equal scores retain discovery order, as in the original stable sort.
            if scored.len() == fetch && scored.last_key_value().is_some_and(|(worst, _)| rank >= *worst) {
                continue;
            }
            // Lowercasing can expand UTF-8 characters; map the match back to source bytes.
            let folded_pos = words.iter().filter_map(|w| low.find(*w)).min().unwrap_or(0);
            let pos = cv_core::scan::lowercase_source_range(sink.hay.as_str(), folded_pos..folded_pos).start;
            let lo = floor_char(sink.hay.as_str(), pos.saturating_sub(40));
            let hi = ceil_char(sink.hay.as_str(), (pos + 120).min(sink.hay.len()));
            let snippet = sink.hay.as_str()[lo..hi].replace('\n', " ").trim().to_string();
            let title = cv_core::label_from(meta.title.as_deref(), sink.first_prompt.as_deref());
            scored.insert(
                rank,
                cv_search::Hit {
                    id: r.id.clone(),
                    harness: r.harness.as_str().to_string(),
                    cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
                    title: Some(title),
                    score: score as f32,
                    snippet,
                    created_at: r.created_at.map(|t| t.timestamp()),
                    updated_at: r.updated_at.map(|t| t.timestamp()),
                    parent_id: None,
                    agent_id: None,
                    workflow: None,
                },
            );
            if scored.len() > fetch {
                scored.pop_last();
            }
        }
    }
    Ok(scored.into_values().collect())
}

// ------------------------------------------------------------------------------------------------
// 2. DISTILL
// ------------------------------------------------------------------------------------------------

/// A ~3-message excerpt window: `(role, trimmed text)` per message.
type Excerpt = Vec<(Role, String)>;

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
    /// Distinct commands run, in first-seen order (capped).
    commands: Vec<String>,
    /// Abstractive digest when `CV_PACK_LLM` routed this span through `llm::digest`.
    llm_digest: Option<String>,
}

fn build_source(hit: &cv_search::Hit, task: &str, llm_model: Option<&Option<String>>) -> Result<PackSource> {
    let want = Harness::parse(&hit.harness);
    let (r, adapter) =
        cv_core::find(&hit.id, want)?.with_context(|| format!("session {} no longer on disk", short_id(&hit.id)))?;

    // Stale events ride the excerpt pass; event failure remains best effort.
    let (excerpt, excerpt_range) = best_window_and_ingest(adapter.as_ref(), &r, task)?;
    let rows = cv_core::events::events_for(r.harness.as_str(), &r.id, None);
    let EventFacts { edits, reads, commands } = event_facts(&rows);

    // Optional abstractive pass over just this span (never the whole session).
    let llm_digest = llm_model.and_then(|model| {
        let mut mini = Session {
            source_stamp: None,
            id: hit.id.clone(),
            harness: r.harness,
            cwd: hit.cwd.as_ref().map(PathBuf::from),
            title: hit.title.clone(),
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: None,
            extra: Default::default(),
            principal: None,
            system_prompt: None,
            lineage: cv_core::ir::Lineage::default(),
        };
        for (role, text) in &excerpt {
            let mut m = Message::new(*role);
            m.content.push(Block::Text {
                text: text.clone().into(),
            });
            mini.messages.push(m);
        }
        let opts = crate::llm::DigestOptions {
            model: model.clone(),
            project: false,
        };
        match crate::llm::digest(&mini, &opts) {
            Ok(d) => Some(d.trim().to_string()),
            Err(e) => {
                eprintln!(
                    "cv pack: LLM digest failed for {} ({e:#}); keeping the extractive digest",
                    short_id(&hit.id)
                );
                None
            }
        }
    });

    Ok(PackSource {
        hit: hit.clone(),
        excerpt,
        excerpt_range,
        edits,
        reads,
        commands,
        llm_digest,
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
                    // A whole shell pipeline isn't memory; its head is.
                    let c = cv_core::ir::truncate(c, 90);
                    if commands.len() < MAX_COMMANDS && !commands.contains(&c) {
                        commands.push(c);
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
/// Same scoring as `cv recall`'s excerpt — word hits + a whole-phrase bonus — but without ever
/// materializing the session.
fn best_window_and_ingest(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    task: &str,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    if !cv_core::events::needs_ingest(r) {
        return best_window(adapter, r, task);
    }
    let source = cv_core::source::Probe::default().snapshot(r);
    let mut events = cv_core::events::SpoolingSink::new(r.cwd.clone());
    let result = best_window_observing(adapter, r, task, Some(&mut events))?;
    let source = source.with_reads(events.inner().dependencies());
    if cv_core::source::unchanged(r, &source) {
        if let Ok(spool) = events.into_spool() {
            let _ = cv_core::events::record_spooled_with_source(r, 0, spool, &source, &Default::default());
        }
    }
    Ok(result)
}

fn best_window(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::ir::SessionRef,
    task: &str,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    best_window_observing(adapter, r, task, None)
}

fn best_window_observing(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    task: &str,
    events: Option<&mut cv_core::events::SpoolingSink>,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    let needle = task.to_lowercase();
    let words: Vec<String> = needle
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect();

    struct WindowSink<'a> {
        events: Option<&'a mut cv_core::events::SpoolingSink>,
        events_failed: bool,
        words: Vec<String>,
        needle: String,
        resolver: Resolver,
        idx: usize,
        /// Last non-empty conversational message seen: `(idx, role, trimmed text)`.
        prev: Option<(usize, Role, String)>,
        best_score: i64,
        window: Vec<(Role, String)>,
        range: Option<(usize, usize)>,
        /// How many more following messages the current best window still wants.
        pending: usize,
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
            let i = self.idx;
            self.idx += 1;
            if m.role == Role::Tool {
                return Flow::Continue;
            }
            let text = match capped_text(&m, &self.resolver) {
                Ok(text) => text,
                Err(error) => {
                    self.error = Some(error);
                    return Flow::Stop;
                }
            };
            if text.trim().is_empty() {
                return Flow::Continue;
            }
            let low = text.to_lowercase();
            let mut score = self.words.iter().filter(|w| low.contains(w.as_str())).count() as i64;
            if low.contains(&self.needle) {
                score += 5;
            }
            if score > self.best_score {
                self.best_score = score;
                self.window.clear();
                let mut lo = i;
                if let Some((pi, pr, pt)) = self.prev.take() {
                    self.window.push((pr, pt));
                    lo = pi;
                }
                self.window.push((m.role, text.clone()));
                self.range = Some((lo, i + 1));
                self.pending = 1;
            } else if self.pending > 0 {
                self.window.push((m.role, text.clone()));
                if let Some((_, hi)) = &mut self.range {
                    *hi = i + 1;
                }
                self.pending -= 1;
            }
            self.prev = Some((i, m.role, text));
            Flow::Continue
        }
    }

    let mut sink = WindowSink {
        events,
        events_failed: false,
        words,
        needle,
        resolver: Resolver::new(Some(r.path.clone())),
        idx: 0,
        prev: None,
        best_score: -1, // any non-empty message seeds a window, even with zero lexical overlap
        window: Vec::new(),
        range: None,
        pending: 0,
        error: None,
    };
    adapter.try_stream(r, &ParseOptions::lazy(), &mut sink)?;
    Ok((sink.window, sink.range))
}

/// A message's text blocks, span-aware (head-resolved) and trimmed to excerpt size with code
/// fences kept intact.
fn capped_text(m: &Message, resolver: &Resolver) -> Result<String> {
    let mut s = String::new();
    for b in &m.content {
        if let Block::Text { text } = b {
            if s.len() >= MSG_RESOLVE_BYTES as usize {
                break;
            }
            if !s.is_empty() {
                s.push('\n');
            }
            let remaining = MSG_RESOLVE_BYTES as usize - s.len();
            if remaining == 0 {
                break;
            }
            let prefix = text.try_resolve_prefix(resolver, remaining as u64)?;
            s.push_str(&prefix[..floor_char(&prefix, prefix.len().min(remaining))]);
        }
    }
    Ok(trim_excerpt(&s))
}

/// Trim to ~[`EXCERPT_SOFT_CHARS`], but never cut inside a fenced code block: an open fence
/// extends the budget up to [`EXCERPT_HARD_CHARS`], then is closed explicitly. Line-oriented, so
/// fences stay at line starts and the result renders as markdown.
fn trim_excerpt(s: &str) -> String {
    let s = s.trim();
    if s.len() <= EXCERPT_SOFT_CHARS {
        return s.to_string();
    }
    let mut out = String::new();
    let mut in_fence = false;
    for line in s.lines() {
        let line = if line.len() > EXCERPT_HARD_CHARS {
            // A single pathological line (minified JSON, …) — head only.
            &line[..floor_char(line, 400)]
        } else {
            line
        };
        let remaining = EXCERPT_HARD_CHARS.saturating_sub(out.len());
        let prefix = &line[..floor_char(line, line.len().min(remaining))];
        if prefix.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        out.push_str(prefix);
        out.push('\n');
        if prefix.len() < line.len() || out.len() >= EXCERPT_HARD_CHARS {
            if in_fence {
                out.push_str("```\n");
            }
            out.push('…');
            return out;
        }
        if out.len() >= EXCERPT_SOFT_CHARS && !in_fence {
            out.push('…');
            return out;
        }
    }
    out.trim_end().to_string()
}

/// Cross-source rollup: `(path, sessions touching it, edits, reads)` ranked by how many sources
/// it appears in, then by edit count — "the files that keep appearing".
fn rollup_files(sources: &[PackSource]) -> Vec<(String, usize, i64, i64)> {
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
    rows.truncate(MAX_ROLLUP);
    rows
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
fn md_bundle(task: &str, sources: &[PackSource], rollup: &[(String, usize, i64, i64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Context pack: {task}\n");
    let _ = writeln!(
        out,
        "Compiled by `cv pack` from {} past session(s) · {}. Extractive — every line below comes \
         from a real transcript or the event catalog.\n",
        sources.len(),
        Utc::now().format("%Y-%m-%d")
    );

    for s in sources {
        let title = s.hit.title.as_deref().unwrap_or("(untitled)");
        let _ = writeln!(
            out,
            "## {title} ({}, {}, {})\n",
            s.hit.harness,
            hit_date(&s.hit),
            hit_cwd(&s.hit)
        );
        if let Some(d) = &s.llm_digest {
            let _ = writeln!(out, "{d}\n");
        }
        if !s.excerpt.is_empty() {
            let _ = writeln!(out, "Key excerpt:\n");
            for (role, text) in &s.excerpt {
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
fn prompt_bundle(task: &str, sources: &[PackSource], rollup: &[(String, usize, i64, i64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "You are picking up work on: {task}\n");
    let _ = writeln!(
        out,
        "Prior context from earlier sessions: you have worked on this ground before. What follows \
         is recalled from {} of your past session(s).\n",
        sources.len()
    );
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
        if let Some(d) = &s.llm_digest {
            for line in d.lines() {
                let _ = writeln!(out, "   {line}");
            }
        }
        for (role, text) in &s.excerpt {
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

/// The `session` format: a synthetic resumable session in the target harness — one user message
/// framing the task with the full bundle, plus a short assistant acknowledgment so the resumed
/// harness opens on a clean turn boundary. Emitted through the same `cv_core::emit` path
/// `cv convert` uses (prints the written path + resume hint).
fn emit_pack_session(
    task: &str,
    sources: &[PackSource],
    rollup: &[(String, usize, i64, i64)],
    to_h: Harness,
    out: Option<PathBuf>,
    thinking: cv_core::emit::ThinkingMode,
) -> Result<()> {
    let bundle = md_bundle(task, sources, rollup);
    let now = Utc::now();

    let mut user = Message::new(Role::User);
    user.timestamp = Some(now);
    user.content.push(Block::Text {
        text: format!(
            "I'm picking up this task: {task}\n\nBefore we start, here is the relevant context \
             from my earlier sessions, compiled by `cv pack`:\n\n{bundle}"
        )
        .into(),
    });

    let mut ack = Message::new(Role::Assistant);
    ack.timestamp = Some(now);
    let key_files: Vec<String> = rollup.iter().take(5).map(|(p, ..)| format!("`{p}`")).collect();
    let files_note = if key_files.is_empty() {
        String::new()
    } else {
        format!(" The files that kept appearing are {}.", key_files.join(", "))
    };
    ack.content.push(Block::Text {
        text: format!(
            "Understood — I've reviewed the prior context for \"{task}\" ({} earlier \
             session(s)).{files_note} Ready to continue from there.",
            sources.len()
        )
        .into(),
    });

    let session = Session {
        source_stamp: None,
        id: format!("pack-{}", now.timestamp()),
        harness: to_h,
        cwd: std::env::current_dir().ok(),
        title: Some(format!("context pack: {}", cv_core::ir::truncate(task, 60))),
        created_at: Some(now),
        updated_at: Some(now),
        model: None,
        git: None,
        messages: vec![user, ack],
        source_path: None,
        extra: Default::default(),
        principal: None,
        system_prompt: None,
        lineage: cv_core::ir::Lineage::default(),
    };
    crate::cmd::port::emit_session(
        &session,
        to_h,
        out,
        EmitOptions {
            thinking,
            ..Default::default()
        },
    )
    .map(drop)
}

/// Write a text bundle to `--out` (with a stderr note) or stdout.
fn write_text(text: &str, out: Option<PathBuf>, force: bool, what: &str) -> Result<()> {
    match out {
        Some(path) => {
            crate::util::write_user_file(&path, text.as_bytes(), force)?;
            eprintln!("✦ wrote {what} → {}", path.display());
        }
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
        fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(
            &self,
            r: &cv_core::SessionRef,
            options: &ParseOptions,
            sink: &mut dyn MessageSink,
        ) -> Result<Session> {
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
        cv_core::events::events_for(r.harness.as_str(), &r.id, None)
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
        let expected = best_window(&adapter, &r, "chosen")?;
        // The old independent ingestion traversal, through the same real adapter.
        let source = cv_core::source::Probe::default().snapshot(&r);
        let mut events = cv_core::events::SpoolingSink::new(r.cwd.clone());
        adapter.try_stream(&r, &ParseOptions::lazy(), &mut events)?;
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
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen")?, expected);
        assert_eq!(adapter.calls.load(Ordering::Relaxed), session.messages.len());
        assert_eq!(format!("{:?}", rows(&r)), expected_rows);
        assert_eq!(event_facts(&rows(&r)), expected_facts);
        assert!(!cv_core::events::needs_ingest(&r));
        let conn = rusqlite::Connection::open(world.cv_home.join("catalog.db"))?;
        conn.execute_batch("CREATE TABLE pack_event_writes(n INTEGER); INSERT INTO pack_event_writes VALUES(0); CREATE TRIGGER count_pack_events AFTER INSERT ON events BEGIN UPDATE pack_event_writes SET n=n+1; END;")?;
        adapter.calls.store(0, Ordering::Relaxed);
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen")?, expected);
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
            assert_eq!(best_window_and_ingest(&adapter, &r, "chosen")?, expected);
            assert_eq!(format!("{:?}", rows(&r)), expected_rows);
            assert!(cv_core::events::needs_ingest(&r));
        }
        adapter.mutate = None;
        // Publication failure rolls back rows without failing the successful excerpt.
        let conn = rusqlite::Connection::open(world.cv_home.join("catalog.db"))?;
        conn.execute_batch(
            "CREATE TRIGGER reject_pack_events BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'pack failure'); END;",
        )?;
        assert_eq!(best_window_and_ingest(&adapter, &r, "chosen")?, expected);
        assert_eq!(format!("{:?}", rows(&r)), expected_rows);
        assert!(cv_core::events::needs_ingest(&r));
        std::fs::write(&path, b"{broken transcript")?;
        assert!(best_window_and_ingest(&adapter, &r, "chosen").is_err());
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
        let expected = best_window(&fixture, &r, "chosen")?;
        assert_eq!(best_window_and_ingest(&fixture, &r, "chosen")?, expected);
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
        assert!(best_window_and_ingest(&ExcerptFixture(vec![unreadable]), &r, "chosen").is_err());
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
        fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(&self, r: &cv_core::SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
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
        let expected_text = capped_text(&message, &resolver)?;
        assert!(!expected_text.contains("omitted-tail"));
        assert!(expected_text.len() < MSG_RESOLVE_BYTES as usize);
        let expected_window = best_window(&ExcerptFixture(vec![message]), &reference, "chosen")?;
        for selected in [&span, &source] {
            assert_eq!(capped_text(selected, &resolver)?, expected_text);
            assert_eq!(
                best_window(&ExcerptFixture(vec![selected.clone()]), &reference, "chosen")?,
                expected_window
            );
        }
        std::fs::write(&path, b"changed")?;
        assert!(capped_text(&source, &resolver).is_err());
        assert!(best_window(&ExcerptFixture(vec![source]), &reference, "chosen").is_err());
        Ok(())
    }

    struct RecallFixture(Vec<(String, String)>);
    impl cv_core::Adapter for RecallFixture {
        fn harness(&self) -> Harness {
            Harness::OpenSession
        }
        fn roots(&self) -> Vec<PathBuf> {
            vec![PathBuf::from("recall-fixture")]
        }
        fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
            Ok(self
                .0
                .iter()
                .map(|(id, _)| {
                    let mut reference = cv_core::SessionRef::from_path(Path::new(id), self.harness());
                    reference.id = id.clone();
                    reference
                })
                .collect())
        }
        fn stream(&self, r: &cv_core::SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            let (_, text) = self.0.iter().find(|(id, _)| id == &r.id).unwrap();
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text {
                text: text.clone().into(),
            });
            sink.message(message);
            Ok(Session::new(&r.id, r.harness))
        }
    }

    #[test]
    fn pack_live_recall_matches_full_stable_ranking_for_every_limit() -> Result<()> {
        let fixture = RecallFixture(
            (0..128)
                .map(|i| {
                    (
                        format!("session-{i}"),
                        match i % 4 {
                            0 => "alpha beta".into(),
                            1 => "alpha".into(),
                            2 => "beta alpha".into(),
                            _ => "irrelevant".into(),
                        },
                    )
                })
                .collect(),
        );
        // Independent oracle: retain every match and apply the original stable full sort.
        let mut expected: Vec<_> = fixture
            .0
            .iter()
            .filter_map(|(id, text)| {
                let score = match text.as_str() {
                    "alpha beta" => 7,
                    "alpha" => 1,
                    "beta alpha" => 2,
                    _ => return None,
                };
                Some((score, id.as_str()))
            })
            .collect();
        expected.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        for limit in [0, 1, 2, 7, 32, 96, 128] {
            let actual = live_scan_adapters("alpha beta", limit, [&fixture as &dyn cv_core::Adapter])?;
            assert_eq!(
                actual.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
                expected.iter().take(limit).map(|(_, id)| *id).collect::<Vec<_>>()
            );
            assert_eq!(
                actual.iter().map(|hit| hit.score as i64).collect::<Vec<_>>(),
                expected.iter().take(limit).map(|(score, _)| *score).collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[test]
    fn pack_live_recall_unicode_snippet_uses_original_source_offsets() -> Result<()> {
        let source = format!("{}target {}", "İ".repeat(200), "end ".repeat(100));
        let fixture = RecallFixture(vec![("unicode".into(), source.clone())]);
        let actual = live_scan_adapters("target", 1, [&fixture as &dyn cv_core::Adapter])?;
        let pos = source.find("target").unwrap();
        assert_eq!(actual[0].snippet, source[pos - 40..pos + 120].trim());
        assert!(actual[0].snippet.contains("target"));
        for source in ["plain text", "İΣ ΟΣ target", "Ⱥİ target", "λİ target"] {
            let folded = source.to_lowercase();
            for (offset, _) in source.char_indices() {
                let prefix = &source[..offset];
                assert_eq!(
                    cv_core::scan::lowercase_source_range(
                        source,
                        prefix.to_lowercase().len()..prefix.to_lowercase().len()
                    )
                    .start,
                    offset
                );
            }
            assert_eq!(
                cv_core::scan::lowercase_source_range(source, folded.len()..folded.len()).start,
                source.len()
            );
        }
        Ok(())
    }

    #[test]
    fn or_query_relaxes_multiword_tasks_only() {
        assert_eq!(
            or_query("tantivy chunked indexing").as_deref(),
            Some("tantivy OR chunked OR indexing")
        );
        assert_eq!(or_query("tantivy"), None);
        assert_eq!(or_query("a of in"), None); // short words dropped
                                               // Punctuation stripped, dupes (case-insensitive) collapsed.
        assert_eq!(or_query("Fix fix, the (parser)!").as_deref(), Some("Fix OR parser"));
    }

    #[test]
    fn trim_excerpt_keeps_code_fences_closed() {
        // Short text passes through untouched.
        assert_eq!(trim_excerpt("hello"), "hello");

        // A fence that opens before the soft cap stays open past it and gets closed at the hard cap.
        let body = format!("intro\n```rust\n{}\n", "let x = 1;\n".repeat(400));
        let out = trim_excerpt(&body);
        assert!(out.len() <= EXCERPT_HARD_CHARS + 16, "len {}", out.len());
        let fences = out.matches("```").count();
        assert_eq!(fences % 2, 0, "unbalanced fences in:\n{out}");
        assert!(out.ends_with('…'), "{out}");

        // Plain prose cuts at the soft cap.
        let prose = "word ".repeat(500);
        let out = trim_excerpt(&prose);
        assert!(out.len() < EXCERPT_SOFT_CHARS + 16, "len {}", out.len());
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
                text: " ".repeat(MSG_RESOLVE_BYTES as usize).into(),
            })
            .collect();
        message.content.push(Block::Text {
            text: "outside the message budget".into(),
        });
        assert!(capped_text(&message, &Resolver::new(None))?.is_empty());
        for body in ["x".repeat(1500), "雪".repeat(500)] {
            let source = format!("{}\n```rust\n{body}\nstill inside\n```\n", "intro ".repeat(100));
            let actual = trim_excerpt(&source);
            assert!(actual.len() <= EXCERPT_HARD_CHARS + 16, "{}", actual.len());
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
            llm_digest: None,
        };
        let bundle = md_bundle("task", &[source], &[]);
        assert!(bundle.contains("cv show claude:abcdef0123456789 --range 4..9"));
        assert!(!bundle.contains("--range 4-9"));
    }
}
