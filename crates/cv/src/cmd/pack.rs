//! `cv pack` — compile a context bundle for a new task from the session corpus.
//!
//! Pipeline: **recall** (full-text + semantic, fused by reciprocal rank, deduped by session) →
//! **distill** (an extractive digest per recalled span: the best-matching message window plus the
//! session's file-edit/command events from the catalog — no LLM, no network) → **emit** (`md` |
//! `prompt` | `session`).
//!
//! The digest is extractive by design: titles, dates, cwds, excerpts, and event-catalog facts are
//! all things the corpus already states, so the bundle never hallucinates. Set `CV_PACK_LLM=1`
//! (or `CV_PACK_LLM=<model-id>`) to additionally route each span through `cv-llm` for an
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
/// Searchable-haystack cap per session for the no-index live scan.
const LIVE_HAY_BYTES: usize = 256 * 1024;
/// How many distinct commands / files to keep per source section (curation over completeness).
const MAX_COMMANDS: usize = 6;
const MAX_FILES_PER_SOURCE: usize = 8;
/// Rows of the cross-source file rollup.
const MAX_ROLLUP: usize = 12;

pub(crate) fn cmd_pack(task: &str, format: &str, to: Option<String>, limit: usize, out: Option<PathBuf>) -> Result<()> {
    if !matches!(format, "md" | "prompt" | "session") {
        bail!("unknown format {format:?} (use md, prompt, or session)");
    }
    let to_h = match (format, &to) {
        ("session", Some(s)) => Some(Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?),
        ("session", None) => bail!("--format session needs --to <harness> (e.g. --to claude)"),
        (_, Some(_)) => bail!("--to only applies to --format session"),
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
        match cv_llm::available_provider() {
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
        "md" => write_text(&md_bundle(task, &sources, &rollup), out, "context pack"),
        "prompt" => write_text(&prompt_bundle(task, &sources, &rollup), out, "context prompt"),
        _ => emit_pack_session(task, &sources, &rollup, to_h.expect("validated above"), out),
    }
}

/// `CV_PACK_LLM` unset/empty/`0` → no LLM. `1`/`true` → provider default model. Anything else is
/// taken as the model id.
fn llm_from_env() -> Option<Option<String>> {
    let v = std::env::var("CV_PACK_LLM").ok()?;
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
    let fetch = (limit * 4).max(20);
    let mut ranked: Vec<(f32, Vec<cv_search::Hit>)> = Vec::new();

    if cv_search::default_tantivy_dir().exists() {
        match cv_search::text_search(None, task, fetch) {
            Ok(hits) if !hits.is_empty() => ranked.push((FTS_WEIGHT, hits)),
            Ok(_) => {
                // The index parser is conjunctive (all terms must match) — right for search,
                // too strict for a task phrase. Retry as an OR of the task's words.
                if let Some(q) = or_query(task) {
                    match cv_search::text_search(None, &q, fetch) {
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
    let mut fused: HashMap<(String, String), f32> = HashMap::new();
    let mut best: HashMap<(String, String), cv_search::Hit> = HashMap::new();
    for (weight, list) in ranked {
        for (rank, h) in list.into_iter().enumerate() {
            let key = (h.harness.clone(), h.id.clone());
            *fused.entry(key.clone()).or_insert(0.0) += weight / (60.0 + rank as f32);
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
    out.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Near-duplicate suppression: one source per (title, cwd). Untitled sessions never collapse.
    let mut seen_titles: HashSet<(String, Option<String>)> = HashSet::new();
    let mut picked = Vec::with_capacity(limit);
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

/// Builds a capped searchable head (`hay`, at most [`LIVE_HAY_BYTES`]) as messages stream past
/// (each dropped immediately), grabbing the first user text for the label fallback; stops the
/// parse once the cap is hit and a first-user label candidate exists. Stream it under
/// [`ParseOptions::lazy`] so large content arrives as spans resolved only up to the remaining
/// budget — peak per session is O(cap), never O(session).
///
/// The no-index live path for both `cv pack` ([`live_scan`]) and `cv search`
/// (`super::search::cmd_search_live`) — exactly the first-run surface where an unbounded
/// haystack used to mean multi-GB RSS.
pub(crate) struct CapSink {
    pub(crate) hay: String,
    pub(crate) first_user: Option<String>,
    resolver: Resolver,
}
impl CapSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub(crate) fn new(path: &Path) -> Self {
        CapSink {
            hay: String::new(),
            first_user: None,
            resolver: Resolver::new(Some(path.to_path_buf())),
        }
    }

    fn push_text(&mut self, text: &cv_core::Text) {
        let remaining = LIVE_HAY_BYTES.saturating_sub(self.hay.len());
        if remaining == 0 {
            return;
        }
        if let Some(sp) = text.as_span() {
            self.hay.push_str(&self.resolver.resolve_prefix(sp, remaining as u64));
        } else if let Some(t) = text.inline_str() {
            self.hay.push_str(&t[..floor_char(t, remaining.min(t.len()))]);
        }
        self.hay.push('\n');
    }
}
impl MessageSink for CapSink {
    fn message(&mut self, m: Message) -> Flow {
        if self.first_user.is_none() && m.role == Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    if let Some(t) = text.inline_str().filter(|t| !t.trim().is_empty()) {
                        self.first_user = Some(t.to_string());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            match b {
                Block::Text { text } | Block::Thinking { text, .. } => self.push_text(text),
                Block::ToolUse { name, input, .. } => {
                    if self.hay.len() < LIVE_HAY_BYTES {
                        self.hay.push_str(name);
                        self.hay.push(' ');
                        let _ = write!(self.hay, "{input}");
                        self.hay.push('\n');
                    }
                }
                Block::ToolResult { content, .. } => self.push_text(content),
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        self.hay.push_str(p);
                        self.hay.push('\n');
                    }
                }
                Block::Image { .. } => {}
            }
        }
        if self.hay.len() >= LIVE_HAY_BYTES && self.first_user.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// No-index fallback: discover and stream every session, scoring a head-capped searchable
/// haystack against the task's words. Peak memory is O(`LIVE_HAY_BYTES`), never O(session).
fn live_scan(task: &str, fetch: usize) -> Result<Vec<cv_search::Hit>> {
    let needle = task.to_lowercase();
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 2).collect();

    let mut scored: Vec<(i64, cv_search::Hit)> = Vec::new();
    for adapter in cv_core::harness::all() {
        if adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = CapSink::new(&r.path);
            let meta = match adapter.stream(&r, &ParseOptions::lazy(), &mut sink) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let low = sink.hay.to_lowercase();
            let mut score = words.iter().filter(|w| low.contains(**w)).count() as i64;
            if low.contains(&needle) {
                score += 5;
            }
            if score == 0 {
                continue;
            }
            // Snippet around the first matched word, from the original-case haystack.
            let pos = words.iter().filter_map(|w| low.find(*w)).min().unwrap_or(0);
            let lo = floor_char(&sink.hay, pos.saturating_sub(40));
            let hi = ceil_char(&sink.hay, (pos + 120).min(sink.hay.len()));
            let snippet = sink.hay[lo..hi].replace('\n', " ").trim().to_string();
            let title = cv_core::label_from(meta.title.as_deref(), sink.first_user.as_deref());
            scored.push((
                score,
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
            ));
        }
    }
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    Ok(scored.into_iter().take(fetch).map(|(_, h)| h).collect())
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
    /// Abstractive digest when `CV_PACK_LLM` routed this span through cv-llm.
    llm_digest: Option<String>,
}

fn build_source(hit: &cv_search::Hit, task: &str, llm_model: Option<&Option<String>>) -> Result<PackSource> {
    let want = Harness::parse(&hit.harness);
    let (r, adapter) =
        cv_core::find(&hit.id, want)?.with_context(|| format!("session {} no longer on disk", short_id(&hit.id)))?;

    // The excerpt window: one streamed pass under lazy options, spans resolved head-only.
    let (excerpt, excerpt_range) = best_window(adapter.as_ref(), &r, task)?;

    // Event-catalog facts (what was DONE). Ingest on the spot when stale — one streamed pass,
    // same as `cv events`.
    if cv_core::events::needs_ingest(&r, cv_core::events::file_mtime_ns(&r.path)) {
        let _ = cv_core::events::ingest_ref(&r);
    }
    let rows = cv_core::events::events_for(r.harness.as_str(), &r.id, None);
    let mut edits: BTreeMap<String, i64> = BTreeMap::new();
    let mut reads: BTreeMap<String, i64> = BTreeMap::new();
    let mut commands: Vec<String> = Vec::new();
    for e in &rows {
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

    // Optional abstractive pass over just this span (never the whole session).
    let llm_digest = llm_model.and_then(|model| {
        let mut mini = Session {
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
        let opts = cv_llm::DistillOptions {
            model: model.clone(),
            project: false,
        };
        match cv_llm::distill(&mini, &opts) {
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

/// Stream the session once (lazy spans, head-capped per message) and return a compact window of
/// `(role, trimmed text)` around the message that best matches `task`, plus its `[lo, hi)` range.
/// Same scoring as `cv recall`'s excerpt — word hits + a whole-phrase bonus — but without ever
/// materializing the session.
fn best_window(
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::ir::SessionRef,
    task: &str,
) -> Result<(Excerpt, Option<(usize, usize)>)> {
    let needle = task.to_lowercase();
    let words: Vec<String> = needle
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect();

    struct WindowSink {
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
    }
    impl MessageSink for WindowSink {
        fn message(&mut self, m: Message) -> Flow {
            let i = self.idx;
            self.idx += 1;
            if m.role == Role::Tool {
                return Flow::Continue;
            }
            let text = capped_text(&m, &self.resolver);
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
        words,
        needle,
        resolver: Resolver::new(Some(r.path.clone())),
        idx: 0,
        prev: None,
        best_score: -1, // any non-empty message seeds a window, even with zero lexical overlap
        window: Vec::new(),
        range: None,
        pending: 0,
    };
    adapter.stream(r, &ParseOptions::lazy(), &mut sink)?;
    Ok((sink.window, sink.range))
}

/// A message's text blocks, span-aware (head-resolved) and trimmed to excerpt size with code
/// fences kept intact.
fn capped_text(m: &Message, resolver: &Resolver) -> String {
    let mut s = String::new();
    for b in &m.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            match text.as_span() {
                Some(sp) => s.push_str(&resolver.resolve_prefix(sp, MSG_RESOLVE_BYTES)),
                None => {
                    if let Some(t) = text.inline_str() {
                        s.push_str(&t[..floor_char(t, t.len().min(MSG_RESOLVE_BYTES as usize))]);
                    }
                }
            }
        }
    }
    trim_excerpt(&s)
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
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        out.push_str(line);
        out.push('\n');
        if out.len() >= EXCERPT_HARD_CHARS {
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
            Some((lo, hi)) => format!("cv show {} --range {lo}-{hi}", short_id(&s.hit.id)),
            None => format!("cv show {}", short_id(&s.hit.id)),
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
        system_prompt: None,
        lineage: cv_core::ir::Lineage::default(),
    };
    crate::cmd::convert::emit_session(&session, to_h, out, EmitOptions::default())
}

/// Write a text bundle to `--out` (with a stderr note) or stdout.
fn write_text(text: &str, out: Option<PathBuf>, what: &str) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("✦ wrote {what} → {}", path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
