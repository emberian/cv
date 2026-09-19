//! # cv-search
//!
//! Pure-Rust search over clustervision sessions, in two flavours:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, `text_search` queries it and returns highlighted snippets. This is a
//!    strict upgrade over a SQLite FTS5 index: real tokenization, BM25 scoring, fielded queries
//!    (`harness:claude foo`), phrase/boolean operators, and fast incremental commits.
//!
//! 2. **Semantic** via [`model2vec-rs`] (distilled *static* embeddings — tiny, fast, no ONNX
//!    runtime). `embed_all` embeds every session and persists the vectors; `semantic_search`
//!    embeds the query and does a brute-force cosine ranking. For ~1k sessions an in-memory
//!    `Vec<(id, Vec<f32>)>` scan is plenty — no ANN crate needed. Gated behind the `semantic`
//!    cargo feature (default-on, since model2vec is lightweight); a minimal FTS-only build is
//!    `cargo build -p cv-search --no-default-features`.
//!
//! Both indexes live under `$CLUSTERVISION_HOME` (or `~/.clustervision`): the tantivy index in
//! `…/tantivy/` and the embedding store in `…/embeddings.bin`.

use anyhow::Result;
use std::path::PathBuf;

pub mod fts;
#[cfg(feature = "semantic")]
pub mod semantic;

/// A single search result, shared by full-text and semantic search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Relevance score (BM25 for FTS, cosine similarity for semantic).
    pub score: f32,
    /// A representative excerpt of the matching text (generated live from the hit session for FTS).
    pub snippet: String,
    /// Unix timestamps carried straight off the FTS index / embedding store, so callers can date
    /// rows without re-discovering the corpus. `None` when the session itself had no parseable
    /// dates, or for semantic hits from an embedding store written before dates were recorded
    /// (re-run `embed_all` to backfill).
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Sub-agent provenance: when this hit is a folded-in sub-agent transcript (indexed with
    /// `cv index --subagents`), the top-level session that spawned it, the agent's own id, and the
    /// workflow run it belonged to (if any). All `None` for an ordinary top-level session, and for
    /// any hit from an index built before subagent folding (the fields default-absent on the
    /// stored doc and on deserialization of older serialized hits).
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
}

/// Root directory for cv-search's on-disk state: `$CLUSTERVISION_HOME` or `~/.clustervision`.
///
/// Back-compat: the project was once "claurdvoyant", so we still honor `$CLAURDVOYANT_HOME` and an
/// existing `~/.claurdvoyant` (kept only if the new dir hasn't been created) — so a previously-built
/// index/embedding store isn't orphaned by the rename.
pub fn home_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("CLUSTERVISION_HOME").or_else(|| std::env::var_os("CLAURDVOYANT_HOME")) {
        return PathBuf::from(v);
    }
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(".");
    };
    let new = home.join(".clustervision");
    let legacy = home.join(".claurdvoyant");
    if !new.exists() && legacy.exists() {
        legacy
    } else {
        new
    }
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    home_dir().join("tantivy")
}

/// Default location of the semantic embedding store (binary; see `semantic` module docs).
/// A legacy `embeddings.json` next to it is migrated on first query.
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.bin")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and update the full-text index at its default location. Incremental by default
/// (only changed/new sessions are rewritten; vanished ones are reaped); `rebuild = true` clears and
/// rebuilds from scratch. Returns the number of sessions discovered.
pub fn index_all(dir: impl Into<Option<PathBuf>>, rebuild: bool, subagents: bool) -> Result<usize> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir, rebuild, subagents)
}

/// Run a full-text query against the index at its default location.
pub fn text_search(dir: impl Into<Option<PathBuf>>, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::text_search(&dir, query, limit)
}

/// Discover, parse, and embed every session, persisting vectors to the default store.
#[cfg(feature = "semantic")]
pub fn embed_all(path: impl Into<Option<PathBuf>>) -> Result<usize> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::embed_all(&path)
}

/// Embed the query and rank stored session vectors by cosine similarity (default store).
#[cfg(feature = "semantic")]
pub fn semantic_search(path: impl Into<Option<PathBuf>>, query: &str, k: usize) -> Result<Vec<Hit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search(&path, query, k)
}

/// Stream the `(id, harness, cwd, title, body-head)` corpus by discovering + streaming every
/// session, invoking `f` once per session and **dropping the `Doc` immediately after**.
///
/// Peak memory is O(head), NOT O(session) or O(corpus). History: `build_corpus() -> Vec<Doc>`
/// held every session's full searchable body (~35 GB) and the semantic path `.clone()`d them all
/// again — ~65 GB RSS, OOM-killed. The streaming driver fixed the corpus dimension; the head cap
/// (`EMBED_HEAD_BYTES` below) fixed the per-session one, since the only consumer left is the
/// embedder and model2vec reads ≤512 tokens anyway. (The FTS indexer doesn't use this — it
/// streams chunk-wise into `fts::ChunkSink`.)
#[cfg(feature = "semantic")]
pub(crate) fn stream_corpus(mut f: impl FnMut(Doc) -> Result<()>) -> Result<usize> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, Role};

    /// How much body to keep per session. The only consumer is the embedder, and model2vec
    /// truncates each input to **512 tokens** (≈2–3 KB of text) — 64 KiB is a 20×+ safety margin,
    /// so the resulting vectors are unchanged while a multi-GB session contributes kilobytes
    /// instead of its whole transcript. Also bounds the `EMBED_BATCH` residency (128 heads ≈ 8 MB,
    /// where 128 full bodies used to be gigabytes).
    const EMBED_HEAD_BYTES: usize = 64 * 1024;

    /// Raw bytes of a lazy span resolved while hunting the first user text — the label only ever
    /// reads its first 72 chars ([`cv_core::label_from`]), so 512 bytes is plenty.
    const FIRST_USER_HEAD: u64 = 512;

    /// Builds a [`Doc`] body **head** as messages stream past, dropping each message immediately.
    /// Streams under [`ParseOptions::lazy`]: large content arrives as spans and is resolved only
    /// up to the remaining head budget, so a giant field is never materialized. Stops the parse
    /// as soon as the head is full and a first-user label candidate has been seen.
    struct DocSink {
        body: String,
        first_user_text: Option<String>,
        resolver: cv_core::Resolver,
    }
    impl MessageSink for DocSink {
        fn message(&mut self, m: Message) -> Flow {
            if self.first_user_text.is_none() && m.role == Role::User {
                // Same projection as `Message::text()` (text blocks joined by '\n'), span-aware.
                let mut s = String::new();
                for b in &m.content {
                    if let Block::Text { text } = b {
                        if !s.is_empty() {
                            s.push('\n');
                        }
                        match text.as_span() {
                            Some(sp) => s.push_str(&self.resolver.resolve_prefix(sp, FIRST_USER_HEAD)),
                            None => {
                                if let Some(t) = text.inline_str() {
                                    s.push_str(t);
                                }
                            }
                        }
                    }
                }
                if !s.trim().is_empty() {
                    self.first_user_text = Some(s);
                }
            }
            if self.body.len() < EMBED_HEAD_BYTES {
                self.append_searchable_capped(&m);
            }
            if self.body.len() >= EMBED_HEAD_BYTES && self.first_user_text.is_some() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    impl DocSink {
        /// Append one content text: inline as-is, a span resolved to at most the remaining budget.
        fn push_text(&mut self, text: &cv_core::Text) {
            if let Some(sp) = text.as_span() {
                let remaining = EMBED_HEAD_BYTES.saturating_sub(self.body.len()) as u64;
                if remaining > 0 {
                    self.body.push_str(&self.resolver.resolve_prefix(sp, remaining));
                }
            } else if let Some(s) = text.inline_str() {
                self.body.push_str(s);
            }
        }

        /// The `cv_core::stream::append_searchable` projection, span-aware and head-capped.
        fn append_searchable_capped(&mut self, m: &Message) {
            use std::fmt::Write as _;
            for b in &m.content {
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        self.push_text(text);
                        self.body.push('\n');
                    }
                    Block::ToolUse { name, input, .. } => {
                        self.body.push_str(name);
                        self.body.push(' ');
                        let _ = write!(self.body, "{input}");
                        self.body.push('\n');
                    }
                    Block::ToolResult { content, .. } => {
                        self.push_text(content);
                        self.body.push('\n');
                        // A persisted-output stub: the real output lives in a sidecar file the model
                        // only saw a pointer to — index its head too, within the same budget.
                        if let Some(p) = b.persisted_output_path() {
                            let remaining = EMBED_HEAD_BYTES.saturating_sub(self.body.len());
                            if remaining > 0 {
                                if let Some(t) = cv_core::lazy::read_head(std::path::Path::new(p), remaining) {
                                    self.body.push_str(&t);
                                    self.body.push('\n');
                                }
                            }
                        }
                    }
                    Block::File { path, source, .. } => {
                        if let Some(p) = path.as_deref().or(source.as_deref()) {
                            self.body.push_str(p);
                            self.body.push('\n');
                        }
                    }
                    Block::Image { .. } => {}
                }
            }
        }
    }

    let mut n = 0usize;
    for r in cv_core::discover_all() {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let mut sink = DocSink {
            body: String::new(),
            first_user_text: None,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
        };
        let meta = match adapter.stream(&r, &ParseOptions::lazy(), &mut sink) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness);
                continue;
            }
        };
        let title = cv_core::label_from(meta.title.as_deref(), sink.first_user_text.as_deref());
        let doc = Doc {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            title: Some(title),
            body: sink.body,
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
        };
        f(doc)?;
        n += 1;
    }
    Ok(n)
}

/// One indexable document distilled from a parsed [`cv_core::Session`], for the **semantic** path
/// ([`semantic::embed_all`]), which reads exactly these fields. The FTS path no longer goes through
/// `Doc`: it streams straight into `fts::ChunkSink`, which reads dates/path/mtime off the
/// [`cv_core::SessionRef`] instead (so they aren't duplicated here).
#[cfg(feature = "semantic")]
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub body: String,
    /// Unix-seconds session dates off the discovery [`cv_core::SessionRef`], so semantic hits
    /// carry `created_at`/`updated_at` just like FTS hits do.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}
