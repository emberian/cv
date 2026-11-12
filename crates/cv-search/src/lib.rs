//! # cv-search
//!
//! Pure-Rust search and derived columnar storage over clustervision sessions:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, [`Searcher`] queries it and returns highlighted snippets. Real
//!    tokenization, BM25 scoring, fielded queries (`harness:claude foo`), phrase/boolean
//!    operators, and fast incremental commits.
//!
//! 2. **Exact trigram substring/symbol search** (`trigram`) — non-positional `roaring` candidate
//!    postings paired with streaming exact substring verification over decoded source revisions.
//!
//! 3. **Derived columnar event segments** (`columnar_events`) — immutable `tantivy-columnar` +
//!    `tantivy-fst` + `roaring` segments with session revision/rewind visibility masks and 64-bit
//!    edit hunk fingerprints.
//!
//! Rebuildable indexes live under [`cache_dir`] (`$CLUSTERVISION_CACHE`, else
//! `$CLUSTERVISION_HOME` when set, else `<cache_dir>/clustervision` alongside `catalog.db`),
//! separate from durable `~/.clustervision` ledgers (`tasks/`, `lanes.jsonl`, `retired.jsonl`,
//! `receipts/`).

use anyhow::Result;
use std::path::PathBuf;

pub mod columnar_events;
pub mod fts;
mod ingest;
mod session_collector;
pub mod trigram;

/// A single search result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
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
        if allowed_ids.is_some_and(std::collections::HashSet::is_empty) {
            return Ok(found);
        }
        let filter = SearchFilter {
            harness: harness.map(cv_core::Harness::as_str),
            ..filter
        };
        match source {
            Source::Live => {
                let live = cv_core::scan::live_search_filtered(query, harness, limit, |r| {
                    allowed_ids.is_none_or(|ids| ids.contains(&r.id))
                        && filter.cwd.is_none_or(|needle| {
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
                let mut refs = cv_core::sessions_for(harness);
                let shared_allowed = allowed_ids.map(|ids| {
                    refs.retain(|r| ids.contains(&r.id));
                    std::sync::Arc::new(ids.clone())
                });
                let live = LiveRead::of(live_bytes)?;
                let fts::Searched {
                    hits,
                    total,
                    note,
                    suffix,
                } = fts::search_suffixed_allowed(dir, query, limit, filter, shared_allowed, live, &refs)?;
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
        }
        Ok(found)
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
        let harness = filter.harness.and_then(cv_core::Harness::parse);
        if filter.harness.is_some() && harness.is_none() {
            return Ok(Found {
                hits: Vec::new(),
                source: Source::Index,
                notes: Vec::new(),
                total: 0,
            });
        }
        let _sources = cv_core::source::read_scope();
        let refs = cv_core::sessions_for(harness);
        let live = LiveRead::of(live_bytes)?;
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
        .map(trigram::TrigramFound::into_found)
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

/// Default location of the derived columnar event store.
pub fn default_columnar_events_dir() -> PathBuf {
    cache_dir().join("events_columnar")
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

pub use fts::{BadLiveBytes, Budget, Indexed, LiveRead, Suffix, Unreadable, LIVE_BYTES_VAR};

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
            assert_eq!(default_columnar_events_dir(), expected_cache.join("events_columnar"));
        }
        {
            let _scope =
                cv_core::env::Env::from_vars([("HOME", "/u"), ("CLUSTERVISION_HOME", "/scratch/cv-home")]).enter();
            assert_eq!(home_dir(), PathBuf::from("/scratch/cv-home"));
            assert_eq!(default_tantivy_dir(), PathBuf::from("/scratch/cv-home/tantivy"));
            assert_eq!(default_trigram_dir(), PathBuf::from("/scratch/cv-home/trigram"));
            assert_eq!(
                default_columnar_events_dir(),
                PathBuf::from("/scratch/cv-home/events_columnar")
            );
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
            assert_eq!(
                default_columnar_events_dir(),
                PathBuf::from("/fast/cv-cache/events_columnar")
            );
        }
    }
}
