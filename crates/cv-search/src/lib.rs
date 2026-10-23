//! # cv-search
//!
//! Pure-Rust search over clustervision sessions, in two flavours:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, [`Searcher`] queries it and returns highlighted snippets. This is a
//!    strict upgrade over a SQLite FTS5 index: real tokenization, BM25 scoring, fielded queries
//!    (`harness:claude foo`), phrase/boolean operators, and fast incremental commits.
//!
//! 2. **Semantic** via [`model2vec-rs`] (distilled *static* embeddings — tiny, fast, no ONNX
//!    runtime). `embed_sessions` embeds sessions and persists the vectors; [`Searcher`]
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
mod ingest;
#[cfg(feature = "semantic")]
pub mod semantic;
#[cfg(feature = "semantic")]
mod semantic_corpus;
mod session_collector;

/// A single search result, shared by full-text and semantic search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Relevance score (BM25 for FTS, cosine similarity for semantic).
    pub score: f32,
    /// A stored preview, or a live excerpt when a displayed full-text hit has been enriched.
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
    /// A live scan's hit is the discovered session itself; its row is exact, scoreless and never
    /// a sub-agent.
    #[serde(skip)]
    pub session: Option<cv_core::SessionRef>,
}

impl From<cv_core::scan::LiveHit> for Hit {
    fn from(hit: cv_core::scan::LiveHit) -> Self {
        let r = hit.session;
        Hit {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            title: Some(hit.title),
            score: 0.0,
            snippet: hit.snippet,
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            parent_id: None,
            agent_id: None,
            workflow: None,
            session: Some(r),
        }
    }
}

/// The §3 search row for a hit, through one operation-owned catalog reader: the index's row with
/// its score and provenance, or the live scan's exact row ([`cv_core::rows::live_search_row`]).
pub fn row(hit: &Hit, context: &cv_core::rows::SearchRowContext) -> serde_json::Value {
    if let Some(session) = &hit.session {
        return cv_core::rows::live_search_row(session, hit.title.as_deref().unwrap_or_default(), &hit.snippet);
    }
    cv_core::rows::SearchRow {
        id: &hit.id,
        harness: &hit.harness,
        cwd: hit.cwd.as_deref(),
        title: hit.title.as_deref(),
        created_at: hit.created_at,
        updated_at: hit.updated_at,
        score: Some(hit.score),
        snippet: &hit.snippet,
        agent_id: hit.agent_id.as_deref(),
        parent_id: hit.parent_id.as_deref(),
        workflow: hit.workflow.as_deref(),
    }
    .to_json_with(context)
}

/// Which path answers a search: the index (authoritative; an empty result is "no match"), the
/// semantic vectors, or a live scan of every transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Index,
    Semantic,
    Live,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Index => "index",
            Source::Semantic => "semantic",
            Source::Live => "live",
        }
    }
}

/// Why an answer may be partial. Every door states these in its own words; none is an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Sessions changed since `cv index` that this search did not read live (over `--live-bytes`,
    /// unreadable, or `--frozen`): the index answered for them as it last saw them.
    Stale(String),
    /// What the search read live beside the index ([`fts::Suffix`]): the sessions, their bytes,
    /// and the `--live-bytes` budget when one was set.
    Live(String),
    /// Nothing matched, and the index holds no sub-agent transcripts (`cv index --subagents`).
    NoSubagents,
    /// Live snippets could not be generated for the shown hits; the indexed previews stand in.
    SnippetsUnavailable(String),
    /// A live scan could not read some sources or sessions; they were not searched.
    Unscanned(String),
}

/// What one search found: the hits in rank order (or discovery order, live), where they came
/// from, the degradations, and how many sessions matched in all.
pub struct Found {
    pub hits: Vec<Hit>,
    pub source: Source,
    pub notes: Vec<Note>,
    /// How many sessions matched in all; `hits` are the first `limit` of them.
    pub total: usize,
}

/// The one search operation every door renders: `cv search` and `GET /api/search` call this with
/// the same filter and get the same hits, source and notes; a long-lived service keeps one so the
/// semantic model and store stay resident.
#[derive(Default)]
pub struct Searcher {
    #[cfg(feature = "semantic")]
    semantic: SemanticSearch,
}

#[cfg(feature = "semantic")]
impl From<SemanticSearch> for Searcher {
    fn from(semantic: SemanticSearch) -> Self {
        Self { semantic }
    }
}

impl Searcher {
    /// Errors are the path's own (an unusable index, an invalid query, a missing model); the
    /// caller adds the door's pointer. Filters apply before ranking. A full-text search answers
    /// from the index plus a live read of what it has not seen: all of it by default, at most
    /// `live_bytes` of source when given (else `$CV_LIVE_BYTES`, else the config's
    /// `[search] live_bytes`), nothing under `--frozen` ([`LiveRead::of`]).
    pub fn search(
        &self,
        query: &str,
        filter: SearchFilter<'_>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_in(&default_tantivy_dir(), query, filter, limit, source, live_bytes)
    }

    /// Search with an explicit full-text index location; live and semantic sources use their
    /// configured stores. This avoids changing the process environment to select an index.
    pub fn search_in(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        let mut found = Found {
            hits: Vec::new(),
            source,
            notes: Vec::new(),
            total: 0,
        };
        let harness = filter.harness.and_then(cv_core::Harness::parse);
        if filter.harness.is_some() && harness.is_none() {
            return Ok(found);
        }
        let filter = SearchFilter {
            harness: harness.map(cv_core::Harness::as_str),
            ..filter
        };
        match source {
            Source::Live => {
                let live = cv_core::scan::live_search_filtered(query, harness, limit, |r| {
                    filter.cwd.is_none_or(|needle| {
                        r.cwd
                            .as_ref()
                            .is_some_and(|path| path.to_string_lossy().contains(needle))
                    })
                })?;
                found.hits = live.hits.into_iter().map(Hit::from).collect();
                found.total = live.total;
                if let Some(first) = live.unreadable.first() {
                    found.notes.push(Note::Unscanned(format!(
                        "{} source(s) could not be read and were not scanned (first: {}: {})",
                        live.unreadable.len(),
                        first.path.display(),
                        first.why
                    )));
                }
                return Ok(found);
            }
            Source::Index => {
                let _sources = cv_core::source::read_scope();
                let refs = cv_core::sessions_for(harness);
                let live = LiveRead::of(live_bytes)?;
                let fts::Searched {
                    hits,
                    total,
                    note,
                    suffix,
                } = fts::search_suffixed(dir, query, limit, filter, live, &refs)?;
                found.hits = hits;
                found.total = total;
                if let Some(why) = note {
                    found.notes.push(Note::SnippetsUnavailable(why));
                }
                found.notes.extend(suffix.read_line().map(Note::Live));
                // Named on hits and misses: a hit list can be missing rows, not only an empty one.
                found.notes.extend(suffix.unread_line().map(Note::Stale));
                if found.hits.is_empty() && !suffix.no_index && !fts::has_subagent_docs(dir) {
                    found.notes.push(Note::NoSubagents);
                }
            }
            #[cfg(feature = "semantic")]
            Source::Semantic => (found.hits, found.total) = self.semantic.search_counted(query, limit, filter)?,
            #[cfg(not(feature = "semantic"))]
            Source::Semantic => anyhow::bail!("semantic search is not built into this cv"),
        }
        Ok(found)
    }
}

/// Root directory for cv-search's on-disk state: `$CLUSTERVISION_HOME` or `~/.clustervision`.
pub fn home_dir() -> PathBuf {
    if let Some(v) = cv_core::env::var_os("CLUSTERVISION_HOME") {
        return PathBuf::from(v);
    }
    cv_core::env::home_dir().map_or_else(|| PathBuf::from("."), |home| home.join(".clustervision"))
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    home_dir().join("tantivy")
}

/// Default location of the semantic embedding store (binary; see `semantic` module docs).
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.bin")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and update the full-text index at its default location. Incremental by default
/// (only changed/new sessions are rewritten; vanished ones are reaped); `rebuild = true` clears and
/// rebuilds from scratch. Returns what it indexed (the sessions it could not read included; `strict`
/// makes any of them an error), and the discovered refs. Each compressed
/// rollout is decoded once: discovery hands its decoded bytes to the sweep running beside it.
pub fn index_all(
    dir: impl Into<Option<PathBuf>>,
    rebuild: bool,
    subagents: bool,
    strict: bool,
) -> Result<(fts::Indexed, Vec<cv_core::SessionRef>)> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir, rebuild, subagents, strict)
}

/// Update the index using an already discovered session inventory.
pub fn index_sessions(
    dir: impl Into<Option<PathBuf>>,
    refs: impl AsRef<[cv_core::SessionRef]>,
    rebuild: bool,
    subagents: bool,
) -> Result<usize> {
    fts::index_sessions(
        &dir.into().unwrap_or_else(default_tantivy_dir),
        refs,
        rebuild,
        subagents,
    )
}

/// Distinguish an invalid full-text expression from an unavailable index. Callers may
/// degrade storage/setup failures to live search, but a rejected query is a caller error.
pub fn is_invalid_text_query(error: &anyhow::Error) -> bool {
    error.is::<tantivy::query::QueryParserError>()
}

/// Match several full-text needles using one index-reader snapshot. The outer result reports
/// index setup errors; each returned result independently reports that needle's query error.
pub fn text_search_ids_batch(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
) -> Result<Vec<Result<std::collections::HashSet<String>>>> {
    fts::text_search_ids_batch(&dir.into().unwrap_or_else(default_tantivy_dir), queries)
}

/// [`text_search_ids_batch`] plus the live suffix the index has not seen (see
/// [`fts::text_search_ids_suffixed`]), as `live` allows.
pub fn text_search_ids_suffixed(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
    live: LiveRead,
    refs: &[cv_core::SessionRef],
) -> Result<(Vec<Result<std::collections::HashSet<String>>>, fts::Suffix)> {
    let _sources = cv_core::source::read_scope();
    fts::text_search_ids_suffixed(&dir.into().unwrap_or_else(default_tantivy_dir), queries, live, refs)
}

/// Rank sessions with stored previews; no source transcripts are read.
pub fn text_search_indexed(dir: impl Into<Option<PathBuf>>, query: &str, limit: usize) -> Result<Vec<Hit>> {
    fts::text_search_indexed(&dir.into().unwrap_or_else(default_tantivy_dir), query, limit)
}

/// Filters evaluated before ranking sessions. Cwd matching is a case-sensitive substring.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchFilter<'a> {
    pub harness: Option<&'a str>,
    pub cwd: Option<&'a str>,
    /// Which text a full-text query matches; semantic vectors embed the exchange alone.
    pub scope: Scope,
}

/// Which of a session's text a full-text query matches. A session's own exchange by default:
/// what the person asked, what the model replied and ran, what came back
/// ([`cv_core::MessageKind::is_exchange`]). The context around it (injected instruction files,
/// system prompts, skill bodies, hook output, notices, compaction summaries) is the same in many
/// sessions and would dilute every answer, so it is matched only when asked for, and turns a fork
/// copied from its source are matched in the source alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Exchange,
    Context,
    All,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "exchange" => Some(Scope::Exchange),
            "context" => Some(Scope::Context),
            "all" => Some(Scope::All),
            _ => None,
        }
    }
    /// Whether a turn of `kind` is in this scope.
    pub fn holds(self, kind: cv_core::MessageKind) -> bool {
        match self {
            Scope::Exchange => kind.is_exchange(),
            Scope::Context => !kind.is_exchange(),
            Scope::All => true,
        }
    }
}

/// Refresh semantic vectors using the same inventory as the full-text pass.
#[cfg(feature = "semantic")]
pub fn embed_sessions(path: impl Into<Option<PathBuf>>, refs: &[cv_core::SessionRef]) -> Result<usize> {
    semantic::embed_sessions(&path.into().unwrap_or_else(default_embeddings_path), refs)
}

pub use fts::{BadLiveBytes, Budget, Indexed, LiveRead, Suffix, Unreadable, LIVE_BYTES_VAR};
/// Explicit lazy semantic state for a long-lived service. One-shot search remains stateless.
#[cfg(feature = "semantic")]
pub use semantic::{embedded_chars, SemanticSearch};

/// Embed the query and rank stored session vectors by cosine similarity (default store).
#[cfg(feature = "semantic")]
pub fn semantic_search(path: impl Into<Option<PathBuf>>, query: &str, k: usize) -> Result<Vec<Hit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search(&path, query, k)
}

#[cfg(feature = "semantic")]
pub(crate) use semantic_corpus::{read_corpus_doc, Doc};

#[cfg(all(test, feature = "probe-heap"))]
#[global_allocator]
static PROBE_ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
