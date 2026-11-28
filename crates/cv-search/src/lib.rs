//! # cv-search
//!
//! Pure-Rust search over clustervision sessions:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, [`Searcher`] queries it and returns highlighted snippets. Real
//!    tokenization, BM25 scoring, fielded queries (`harness:claude foo`), phrase/boolean
//!    operators, and fast incremental commits.
//!
//! 2. **Exact trigram substring/symbol search** (`trigram`) — non-positional `roaring` candidate
//!    postings paired with streaming exact substring verification over decoded source revisions.
//!
//! Rebuildable indexes live under [`cache_dir`] (`$CLUSTERVISION_CACHE`, else
//! `$CLUSTERVISION_HOME` when set, else `<cache_dir>/clustervision` alongside `catalog.db`),
//! separate from durable `~/.clustervision` ledgers (`tasks/`, `lanes.jsonl`, `retired.jsonl`,
//! `receipts/`).

use anyhow::Result;
use std::path::PathBuf;

pub mod fts;
pub use fts::{IndexedAnswer, PreparedIndex, QueryWork};
mod ingest;
mod session_collector;
pub mod trigram;

/// A single search result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Relevance score (BM25 for FTS, 0.0 for unranked live/exact matches).
    pub score: f32,
    /// A stored preview, or a live excerpt when a displayed full-text hit has been enriched.
    pub snippet: String,
    /// Unix timestamps carried straight off the FTS index, so callers can date rows without
    /// re-discovering the corpus. `None` when the session itself had no parseable dates.
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
            session_key: Some(r.session_selector()),
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
    let mut row = cv_core::rows::SearchRow {
        id: hit.session_key.as_deref().unwrap_or(&hit.id),
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
    .to_json_with(context);
    row["id"] = hit.id.clone().into();
    if let Some(key) = &hit.session_key {
        row["session_key"] = key.clone().into();
    }
    row
}

fn native_id(selector: &str) -> &str {
    cv_core::SessionKey::split_selector(selector).map_or(selector, |(id, _)| id)
}

/// Which path answers a search: the index (authoritative; an empty result is "no match") or a live
/// scan of every transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Index,
    Live,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Index => "index",
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

/// Accounting basis for [`SearchWork`]: logical application/index operations and bytes read,
/// distinct from OS page-cache hits or physical block faults.
pub const SEARCH_WORK_BASIS: &str = "logical_app_reads";

/// Per-query logical read-work accounting across index evaluation, discovery, and live source reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchWork {
    pub basis: String,
    pub index_docs_examined: usize,
    pub stored_previews_used: usize,
    pub discovery_sessions: usize,
    pub source_sessions_read: usize,
    pub source_bytes_read: u64,
    pub setup_micros: u64,
    pub eval_micros: u64,
}

impl Default for SearchWork {
    fn default() -> Self {
        Self {
            basis: SEARCH_WORK_BASIS.to_owned(),
            index_docs_examined: 0,
            stored_previews_used: 0,
            discovery_sessions: 0,
            source_sessions_read: 0,
            source_bytes_read: 0,
            setup_micros: 0,
            eval_micros: 0,
        }
    }
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
/// the same filter and get the same hits, source and notes.
#[derive(Default)]
pub struct Searcher;

impl Searcher {
    /// Errors are the path's own (an unusable index, an invalid query); the
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
        self.search_in_allowed(&default_tantivy_dir(), query, filter, None, limit, source, live_bytes)
    }

    /// [`search`](Self::search) further restricted to an explicit session-ID allowlist (such as
    /// the candidate set evaluated by `cv search -q <QUERY>`). Sub-agent hits in the full-text
    /// index are admitted when either their own ID or their `parent_id` is in `allowed_ids`.
    pub fn search_allowed(
        &self,
        query: &str,
        filter: SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_in_allowed(
            &default_tantivy_dir(),
            query,
            filter,
            allowed_ids,
            limit,
            source,
            live_bytes,
        )
    }

    /// [`search_allowed`](Self::search_allowed) paired with [`SearchWork`] accounting.
    pub fn search_allowed_with_work(
        &self,
        query: &str,
        filter: SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<(Found, SearchWork)> {
        self.search_in_allowed_with_work(
            &default_tantivy_dir(),
            query,
            filter,
            allowed_ids,
            limit,
            source,
            live_bytes,
        )
    }

    /// Search with an explicit full-text index location; live sources use their configured stores.
    /// This avoids changing the process environment to select an index.
    pub fn search_in(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_in_allowed(dir, query, filter, None, limit, source, live_bytes)
    }

    /// [`search_in`](Self::search_in) with an optional session-ID allowlist.
    #[allow(clippy::too_many_arguments)]
    pub fn search_in_allowed(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_in_allowed_with_work(dir, query, filter, allowed_ids, limit, source, live_bytes)
            .map(|(found, _)| found)
    }

    /// [`search_in_allowed`](Self::search_in_allowed) paired with [`SearchWork`] accounting.
    #[allow(clippy::too_many_arguments)]
    pub fn search_in_allowed_with_work(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        source: Source,
        live_bytes: Option<u64>,
    ) -> Result<(Found, SearchWork)> {
        let mut found = Found {
            hits: Vec::new(),
            source,
            notes: Vec::new(),
            total: 0,
        };
        let mut work = SearchWork::default();
        let harness = filter.harness.and_then(cv_core::Harness::parse);
        if filter.harness.is_some() && harness.is_none() {
            return Ok((found, work));
        }
        if allowed_ids.is_some_and(std::collections::HashSet::is_empty) {
            return Ok((found, work));
        }
        let filter = SearchFilter {
            harness: harness.map(cv_core::Harness::as_str),
            ..filter
        };
        match source {
            Source::Live => {
                let started = std::time::Instant::now();
                let mut discovery_sessions = 0usize;
                let mut admitted_paths: Vec<PathBuf> = Vec::new();
                let live = cv_core::scan::live_search_filtered(query, harness, limit, |r| {
                    discovery_sessions += 1;
                    let keep = allowed_ids.is_none_or(|ids| ids.contains(&r.id) || ids.contains(&r.session_selector()))
                        && filter.cwd.is_none_or(|needle| {
                            r.cwd
                                .as_ref()
                                .is_some_and(|path| path.to_string_lossy().contains(needle))
                        });
                    if keep {
                        admitted_paths.push(r.path.clone());
                    }
                    keep
                })?;
                let unreadable_paths: std::collections::HashSet<&std::path::Path> =
                    live.unreadable.iter().map(|s| s.path.as_path()).collect();
                let mut counted_paths: std::collections::HashSet<&std::path::Path> = std::collections::HashSet::new();
                let mut source_bytes_read = 0u64;
                for p in &admitted_paths {
                    if !unreadable_paths.contains(p.as_path()) && counted_paths.insert(p.as_path()) {
                        source_bytes_read =
                            source_bytes_read.saturating_add(std::fs::metadata(p).map_or(0, |m| m.len()));
                    }
                }
                work = SearchWork {
                    basis: SEARCH_WORK_BASIS.to_owned(),
                    index_docs_examined: 0,
                    stored_previews_used: 0,
                    discovery_sessions,
                    source_sessions_read: admitted_paths.len().saturating_sub(live.unreadable.len()),
                    source_bytes_read,
                    setup_micros: 0,
                    eval_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                };
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
                return Ok((found, work));
            }
            Source::Index => {
                let _sources = cv_core::source::read_scope();
                let shared_allowed = allowed_ids.map(|ids| std::sync::Arc::new(ids.clone()));
                let live = LiveRead::of(live_bytes)?;
                let fts::Searched {
                    hits,
                    total,
                    note,
                    suffix,
                    work: searched_work,
                } = fts::search_suffixed_allowed(dir, query, limit, filter, shared_allowed, live, || {
                    let mut refs = cv_core::try_sessions_for(harness)?;
                    if let Some(ids) = allowed_ids {
                        refs.retain(|r| ids.contains(&r.id) || ids.contains(&r.session_selector()));
                    }
                    Ok(refs)
                })?;
                found.hits = hits;
                found.total = total;
                work = searched_work;
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
        }
        Ok((found, work))
    }

    /// Exact code/symbol/substring search backed by the trigram candidate index plus live tail
    /// verification against matching decoded source revisions.
    pub fn search_exact(
        &self,
        query: &str,
        filter: SearchFilter<'_>,
        case_sensitive: bool,
        limit: usize,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_exact_in_allowed(
            &default_trigram_dir(),
            query,
            filter,
            case_sensitive,
            None,
            limit,
            live_bytes,
        )
    }

    /// [`search_exact`](Self::search_exact) with explicit trigram index location and session allowlist.
    #[allow(clippy::too_many_arguments)]
    pub fn search_exact_in_allowed(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        case_sensitive: bool,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        live_bytes: Option<u64>,
    ) -> Result<Found> {
        self.search_exact_in_allowed_with_work(dir, query, filter, case_sensitive, allowed_ids, limit, live_bytes)
            .map(|(found, _)| found)
    }

    /// [`search_exact_in_allowed`](Self::search_exact_in_allowed) paired with [`SearchWork`] accounting.
    #[allow(clippy::too_many_arguments)]
    pub fn search_exact_in_allowed_with_work(
        &self,
        dir: &std::path::Path,
        query: &str,
        filter: SearchFilter<'_>,
        case_sensitive: bool,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        limit: usize,
        live_bytes: Option<u64>,
    ) -> Result<(Found, SearchWork)> {
        let harness = filter.harness.and_then(cv_core::Harness::parse);
        if filter.harness.is_some() && harness.is_none() {
            return Ok((
                Found {
                    hits: Vec::new(),
                    source: Source::Index,
                    notes: Vec::new(),
                    total: 0,
                },
                SearchWork::default(),
            ));
        }
        let _sources = cv_core::source::read_scope();
        let disc_started = std::time::Instant::now();
        let refs = cv_core::sessions_for(harness);
        let live = LiveRead::of(live_bytes)?;
        let disc_micros = disc_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        trigram::search(
            dir,
            query,
            trigram::TrigramOptions {
                filter: SearchFilter {
                    harness: harness.map(cv_core::Harness::as_str),
                    ..filter
                },
                case_sensitive,
                live,
                limit,
            },
            &refs,
            allowed_ids,
        )
        .map(|mut tf| {
            tf.work.setup_micros = tf.work.setup_micros.saturating_add(disc_micros);
            tf.into_found_with_work()
        })
    }
}

/// Durable state directory: `$CLUSTERVISION_HOME` or `~/.clustervision`.
pub fn home_dir() -> PathBuf {
    cv_core::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Root directory for cv-search's rebuildable on-disk indexes (`$CLUSTERVISION_CACHE`, else
/// `$CLUSTERVISION_HOME` when set, else `<cache_dir>/clustervision`).
pub fn cache_dir() -> PathBuf {
    cv_core::env::cv_cache().unwrap_or_else(home_dir)
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    cache_dir().join("tantivy")
}

/// Default location of the derived trigram substring index.
pub fn default_trigram_dir() -> PathBuf {
    cache_dir().join("trigram")
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

/// Match a single full-text needle against the index (`Scope::Exchange`).
pub fn text_search_ids(dir: impl Into<Option<PathBuf>>, query: &str) -> Result<std::collections::HashSet<String>> {
    text_search_ids_scoped(dir, query, Scope::Exchange)
}

/// Match a single full-text needle in `scope` against the index.
pub fn text_search_ids_scoped(
    dir: impl Into<Option<PathBuf>>,
    query: &str,
    scope: Scope,
) -> Result<std::collections::HashSet<String>> {
    fts::text_search_ids_scoped(&dir.into().unwrap_or_else(default_tantivy_dir), query, scope)
}

/// Match several full-text needles using one index-reader snapshot. The outer result reports
/// index setup errors; each returned result independently reports that needle's query error.
pub fn text_search_ids_batch(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
) -> Result<Vec<Result<std::collections::HashSet<String>>>> {
    text_search_ids_batch_scoped(dir, queries, Scope::Exchange)
}

/// [`text_search_ids_batch`] evaluated in an explicit [`Scope`].
pub fn text_search_ids_batch_scoped(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
    scope: Scope,
) -> Result<Vec<Result<std::collections::HashSet<String>>>> {
    fts::text_search_ids_batch_scoped(&dir.into().unwrap_or_else(default_tantivy_dir), queries, scope)
}

/// [`text_search_ids_batch`] plus the live suffix the index has not seen (see
/// [`fts::text_search_ids_suffixed`]), as `live` allows.
pub fn text_search_ids_suffixed(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
    live: LiveRead,
    refs: &[cv_core::SessionRef],
) -> Result<(Vec<Result<std::collections::HashSet<String>>>, fts::Suffix)> {
    text_search_ids_suffixed_scoped(dir, queries, Scope::Exchange, live, refs)
}

/// [`text_search_ids_suffixed`] evaluated in an explicit [`Scope`].
pub fn text_search_ids_suffixed_scoped(
    dir: impl Into<Option<PathBuf>>,
    queries: &[&str],
    scope: Scope,
    live: LiveRead,
    refs: &[cv_core::SessionRef],
) -> Result<(Vec<Result<std::collections::HashSet<String>>>, fts::Suffix)> {
    let _sources = cv_core::source::read_scope();
    fts::text_search_ids_suffixed_scoped(
        &dir.into().unwrap_or_else(default_tantivy_dir),
        queries,
        scope,
        live,
        refs,
    )
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
    /// Which text a full-text or trigram query matches.
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

pub use cv_core::{BadThreads, THREADS_VAR};
pub use fts::{BadLiveBytes, Budget, Indexed, LiveRead, Suffix, Unreadable, LIVE_BYTES_VAR};

/// Bounded over-fetch budget before [`collapse_hit_chains`] / [`collapse_found_hits`] so derived
/// copies dropped during chain collapsing do not starve the caller's requested `limit`.
pub fn chain_fetch_limit(limit: usize, all: bool) -> usize {
    if all || limit == 0 {
        limit
    } else {
        limit
            .saturating_mul(2)
            .max(limit.saturating_add(4))
            .min(limit.saturating_add(16))
    }
}

/// Resolve an indexed identity exactly. Offline searches can use committed custody and recorded
/// catalog metadata, including the last known message count of a vanished transcript.
fn chain_reference(harness: &str, id: &str, live: bool) -> Option<cv_core::SessionRef> {
    let h = cv_core::Harness::parse(harness)?;
    let exact = |r: &cv_core::SessionRef| r.id == id || native_id(&r.id) == id || r.session_selector() == id;
    if live {
        return cv_core::find(id, Some(h)).ok()?.map(|(r, _)| r).filter(exact);
    }
    let mut archived = cv_core::custody::matching_backings(id, Some(h))
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.reference)
        .filter(exact);
    if let Some(r) = archived.next() {
        return archived.next().is_none().then_some(r);
    }
    let mut cached = cv_core::catalog::lookup(id, Some(h)).into_iter().filter(exact);
    let r = cached.next()?;
    cached.next().is_none().then_some(r)
}

fn chain_parent(id: &str, r: Option<&cv_core::SessionRef>, live: bool) -> Option<String> {
    let parent = r
        .filter(|r| live || cv_core::custody::is_pack_ref(r))
        .and_then(cv_core::SessionRef::derived_from)
        .or_else(|| cv_core::lineage::state_derived_from(id))?;
    Some(match cv_core::SessionKey::split_selector(id) {
        Some((_, profile)) if cv_core::SessionKey::split_selector(&parent).is_none() => format!("{parent}@{profile}"),
        _ => parent,
    })
}

/// Collapse connected derived-session chains (`distill`, `splice`, `rewind`, `port`, `continues`)
/// across `hits` down to each chain's fullest representative, keeping sub-agent hits distinct and
/// respecting `live_allowed`. Offline searches use custody, catalog metadata and recorded lineage edges.
pub fn collapse_hit_chains(hits: Vec<Hit>, live_allowed: bool) -> (Vec<Hit>, Vec<cv_core::lineage::CollapsedCopy>) {
    if hits.len() <= 1 {
        return (hits, Vec::new());
    }
    // An index-only HTTP request can arrive in a writable server process.
    let _scope = (!live_allowed && !cv_core::env::frozen())
        .then(|| cv_core::env::Env::current().as_ref().clone().with_frozen().enter());
    let refs: Vec<Option<cv_core::SessionRef>> = hits
        .iter()
        .map(|h| {
            if h.agent_id.is_some() {
                return None;
            }
            if let Some(r) = &h.session {
                return Some(r.clone());
            }
            let sel = h.session_key.as_deref().unwrap_or(&h.id);
            chain_reference(&h.harness, sel, live_allowed)
        })
        .collect();
    let derived: Vec<Option<String>> = hits
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            if h.agent_id.is_some() {
                return None;
            }
            chain_parent(
                h.session_key.as_deref().unwrap_or(&h.id),
                refs[idx].as_ref(),
                live_allowed,
            )
        })
        .collect();
    if derived.iter().all(Option::is_none) {
        return (hits, Vec::new());
    }
    let counts: Vec<Option<usize>> = refs
        .iter()
        .map(|r| {
            if live_allowed {
                r.as_ref().and_then(cv_core::lineage::effective_message_count)
            } else {
                r.as_ref().and_then(|s| s.message_count)
            }
        })
        .collect();
    let sizes: Vec<Option<u64>> = refs
        .iter()
        .map(|r| {
            if live_allowed {
                r.as_ref()
                    .and_then(|s| std::fs::metadata(&s.path).ok().map(|m| m.len()))
            } else {
                None
            }
        })
        .collect();

    let indexed: Vec<(usize, Hit)> = hits.into_iter().enumerate().collect();
    let (kept, collapsed) = cv_core::lineage::collapse_derived_chains_with_lookup(
        indexed,
        |(idx, h)| cv_core::lineage::ChainMeta {
            harness: h.harness.clone(),
            id: if h.agent_id.is_some() {
                format!("subagent:{idx}:{}", h.id)
            } else {
                h.session_key.as_ref().unwrap_or(&h.id).clone()
            },
            derived_from: derived[*idx].clone(),
            message_count: counts[*idx],
            updated_at: h
                .updated_at
                .or_else(|| refs[*idx].as_ref().and_then(|r| r.updated_at.map(|t| t.timestamp()))),
            size_bytes: sizes[*idx],
        },
        |harness, pid| {
            let reference = chain_reference(harness, pid, live_allowed);
            chain_parent(pid, reference.as_ref(), live_allowed)
        },
    );
    (kept.into_iter().map(|(_, h)| h).collect(), collapsed)
}

/// Apply [`collapse_hit_chains`] (unless `all` is set) and truncate `found` to `limit`, adjusting
/// `found.total` by the number of collapsed duplicate copies.
pub fn collapse_found_hits(found: &mut Found, limit: usize, all: bool, live_allowed: bool) {
    if all {
        found.hits.truncate(limit);
        return;
    }
    let hits = std::mem::take(&mut found.hits);
    let (mut kept, collapsed) = collapse_hit_chains(hits, live_allowed);
    found.total = found.total.saturating_sub(collapsed.len());
    kept.truncate(limit);
    found.hits = kept;
}

#[cfg(all(test, feature = "probe-heap"))]
#[global_allocator]
static PROBE_ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_cache_paths_honor_clustervision_cache_and_default_outside_durable_home() {
        {
            let _scope = cv_core::env::Env::from_vars([("HOME", "/u"), ("XDG_CACHE_HOME", "/u/.cache")]).enter();
            let expected_cache = cv_core::env::cache_dir().unwrap().join("clustervision");
            assert_eq!(home_dir(), PathBuf::from("/u/.clustervision"));
            assert_eq!(default_tantivy_dir(), expected_cache.join("tantivy"));
            assert_eq!(default_trigram_dir(), expected_cache.join("trigram"));
        }
        {
            let _scope =
                cv_core::env::Env::from_vars([("HOME", "/u"), ("CLUSTERVISION_HOME", "/scratch/cv-home")]).enter();
            assert_eq!(home_dir(), PathBuf::from("/scratch/cv-home"));
            assert_eq!(default_tantivy_dir(), PathBuf::from("/scratch/cv-home/tantivy"));
            assert_eq!(default_trigram_dir(), PathBuf::from("/scratch/cv-home/trigram"));
        }
        {
            let _scope = cv_core::env::Env::from_vars([
                ("HOME", "/u"),
                ("CLUSTERVISION_HOME", "/durable/cv-home"),
                ("CLUSTERVISION_CACHE", "/fast/cv-cache"),
            ])
            .enter();
            assert_eq!(home_dir(), PathBuf::from("/durable/cv-home"));
            assert_eq!(default_tantivy_dir(), PathBuf::from("/fast/cv-cache/tantivy"));
            assert_eq!(default_trigram_dir(), PathBuf::from("/fast/cv-cache/trigram"));
        }
    }

    #[test]
    fn collapse_found_hits_deduplicates_derived_chains_and_adjusts_total() {
        assert_eq!(chain_fetch_limit(0, false), 0);
        assert_eq!(chain_fetch_limit(5, true), 5);
        assert_eq!(chain_fetch_limit(5, false), 10);
        assert_eq!(chain_fetch_limit(20, false), 36);

        let dir = std::env::temp_dir().join(format!("cv-search-collapse-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = cv_core::env::Env::from_vars([("CLUSTERVISION_HOME", dir.to_str().unwrap())]).enter();

        let state = cv_core::state::StateStore::at_dir(&dir);
        state
            .record_lineage_edge(&cv_core::state::LineageEdge {
                src_session_id: "root-orig".into(),
                dst_session_id: "child-copy".into(),
                edge_kind: "distilled_from".into(),
                harness: Some("claude".into()),
                tool_use_id: None,
                metadata: serde_json::json!({}),
                created_at: 1_700_000_000,
            })
            .unwrap();

        let make_hit = |id: &str, msgs: usize| Hit {
            harness: "claude".into(),
            id: id.into(),
            session_key: None,
            score: 1.0,
            title: Some("Chain".into()),
            cwd: None,
            created_at: None,
            updated_at: Some(1_700_000_000),
            snippet: "needle".into(),
            agent_id: None,
            parent_id: None,
            workflow: None,
            session: Some({
                let mut r = cv_core::SessionRef::new(id, cv_core::Harness::Claude, dir.join(format!("{id}.jsonl")));
                r.message_count = Some(msgs);
                r
            }),
        };

        let mut twin = make_hit("root-orig", 20);
        let qualify = |mut hit: Hit, suffix: char| {
            hit.session_key = Some(format!("{}@{}", hit.id, suffix.to_string().repeat(64)));
            hit
        };
        twin = qualify(twin, 'b');
        let (kept, dropped) = collapse_hit_chains(
            vec![
                qualify(make_hit("child-copy", 2), 'a'),
                qualify(make_hit("root-orig", 12), 'a'),
                twin,
            ],
            false,
        );
        assert_eq!(kept.len(), 2, "the other profile keeps its root");
        assert_eq!(dropped.len(), 1);
        assert_eq!(kept[0].id, "root-orig");

        let mut indexed = [make_hit("child-copy", 0), make_hit("root-orig", 0)];
        for hit in &mut indexed {
            hit.session = None;
        }
        assert_eq!(collapse_hit_chains(indexed.into(), false).0.len(), 1);
        assert!(
            !dir.join("catalog.db").exists(),
            "index-only lineage triggered discovery"
        );

        let mut found = Found {
            hits: vec![make_hit("child-copy", 2), make_hit("root-orig", 12)],
            total: 2,
            source: Source::Index,
            notes: Vec::new(),
        };
        collapse_found_hits(&mut found, 10, false, false);
        assert_eq!(found.total, 1);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].id, "root-orig");

        std::fs::remove_dir_all(&dir).ok();
    }
}
