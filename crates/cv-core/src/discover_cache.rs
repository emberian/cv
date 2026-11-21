//! A persistent cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript to extract metadata.
//! Exact cache hits require the current interpretation version, unchanged `(mtime, size)`, and
//! captured file identity, rejecting appends, timestamp-preserving replacements and rewrites. A hit returns stored [`SessionRef`]s without
//! reading transcript content; full discovery still freshly enumerates each harness's inventory.
//!
//! It's process-global and shared by `cv` and the desktop app via the `scan_cache` table in
//! the shared catalog db (see [`crate::catalog`], which owns the DDL; the rows also feed the
//! freshness probe's append detection). Reads are tolerant — a missing or unreadable cache simply
//! means everything is re-scanned (and the cache rebuilt), said once per process.
//! Opted-in adapters may continue metadata on growth under their immutable-prefix contract,
//! using independent reducer state and sampled resume guards, never stale published refs.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

pub(crate) mod read;

fn note(error: &impl std::fmt::Display) {
    crate::env::warn_once("the scan cache", error);
}

#[cfg(feature = "sqlite")]
fn note_sql(error: &rusqlite::Error) {
    if !matches!(error, rusqlite::Error::QueryReturnedNoRows) {
        note(error);
    }
}

enum Payload {
    #[cfg(feature = "sqlite")]
    Encoded { refs: String, reads: String },
    Parsed {
        refs: Vec<SessionRef>,
        reads: crate::source::ReadSet,
    },
    #[cfg(feature = "sqlite")]
    Invalid,
}

impl Payload {
    /// Decode only a demanded row. Invalid persisted metadata remains a cache miss; releasing
    /// encoded strings after either outcome avoids retaining both representations.
    #[cfg(feature = "sqlite")]
    fn parsed(&mut self) -> Option<(&mut Vec<SessionRef>, &crate::source::ReadSet)> {
        if let Self::Encoded { refs, reads } = self {
            *self = serde_json::from_str(refs)
                .and_then(|refs| serde_json::from_str(reads).map(|reads| Self::Parsed { refs, reads }))
                .inspect_err(note)
                .unwrap_or(Self::Invalid);
        }
        match self {
            Self::Parsed { refs, reads } => Some((refs, reads)),
            _ => None,
        }
    }

    fn known(&self) -> Option<(&Vec<SessionRef>, &crate::source::ReadSet)> {
        match self {
            Self::Parsed { refs, reads } => Some((refs, reads)),
            #[cfg(feature = "sqlite")]
            _ => None,
        }
    }

    #[cfg(feature = "sqlite")]
    fn encoded(&self) -> bool {
        matches!(self, Self::Encoded { .. })
    }
}

/// A metadata reducer checkpoint, kept in scan_cache rather than consumer resume_marks.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ScanContinuation {
    pub head: crate::resume::MarkHead,
    pub checkpoint: crate::stream::Checkpoint,
    /// Auxiliary identities only: the growing primary is guarded by head and before/after reads.
    pub dependencies: crate::source::ReadSet,
}

struct Entry {
    #[cfg(feature = "sqlite")]
    mtime_ns: u64,
    size: u64,
    payload: Payload,
    /// Private reducer state, independent of mutable published refs; decode only on append.
    continuation: Option<String>,
    /// Scanned this run and not yet written back to sqlite.
    #[cfg(feature = "sqlite")]
    dirty: bool,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    any_dirty: AtomicBool,
    invalidated: AtomicBool,
}

/// What discovery's cached rows mean is the one stored-format version
/// ([`crate::source::VERSION`]): bump that when discovery changes the interpretation of unchanged
/// source bytes. Unknown writer versions are filled only by ordinary source reads.
#[cfg(feature = "sqlite")]
pub(crate) const INTERPRETATION_VERSION: u32 = crate::source::VERSION;
/// Every live cache by catalog. Each environment ([`crate::env`]) retains its current catalog's;
/// other homes survive only while operations use them.
static CACHE: OnceLock<RwLock<HashMap<PathBuf, std::sync::Weak<Cache>>>> = OnceLock::new();
fn cache_for(path: &Path) -> Arc<Cache> {
    let mut active = CACHE.get_or_init(Default::default).write().unwrap();
    active.retain(|_, cache| cache.strong_count() != 0);
    active.get(path).and_then(std::sync::Weak::upgrade).unwrap_or_else(|| {
        let cache = Arc::new(load());
        active.insert(path.to_path_buf(), Arc::downgrade(&cache));
        cache
    })
}
#[cfg(all(test, feature = "sqlite"))]
fn current_cache() -> Arc<Cache> {
    read::Context::current().cache()
}
fn existing_cache(path: &Path) -> Option<Arc<Cache>> {
    let key = read::catalog_key(path);
    CACHE.get()?.read().inspect_err(note).ok()?.get(&key)?.upgrade()
}

#[cfg(feature = "sqlite")]
pub(crate) fn invalidate_interpretation(path: &Path) {
    if let Some(cache) = existing_cache(path) {
        cache.invalidated.store(true, Ordering::Release);
    }
}

/// Drop this process's scan-cache entries, as a new process starts: later scans see only the
/// rows the catalog holds.
#[cfg(test)]
pub(crate) fn forget_process_cache() {
    let context = read::Context::current();
    if let Some(cache) = context.path().and_then(existing_cache) {
        cache.entries.write().unwrap().clear();
        cache.any_dirty.store(false, Ordering::Relaxed);
    }
}

fn clear_invalidated(cache: &Cache) {
    if cache.invalidated.load(Ordering::Acquire) {
        let mut entries = cache.entries.write().unwrap();
        if cache.invalidated.swap(false, Ordering::AcqRel) {
            entries.clear();
            cache.any_dirty.store(false, Ordering::Release);
        }
    }
}

/// Start empty. Persisted payloads enter this existing cache only on exact keyed demand.
fn load() -> Cache {
    Cache {
        entries: Default::default(),
        any_dirty: AtomicBool::new(false),
        invalidated: AtomicBool::new(false),
    }
}

#[cfg(feature = "sqlite")]
fn load_entry(conn: &rusqlite::Connection, path: &Path, key: (u64, u64)) -> Option<Entry> {
    let mut statement = conn.prepare_cached("SELECT refs,source_reads,continuation FROM scan_cache WHERE path=?1 AND mtime_ns=?2 AND size=?3 AND interpretation_version=?4").inspect_err(note_sql).ok()?;
    let (refs, reads, continuation) = statement
        .query_row(
            rusqlite::params![
                path.to_string_lossy(),
                key.0 as i64,
                key.1 as i64,
                INTERPRETATION_VERSION
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .inspect_err(note_sql)
        .ok()?;
    Some(Entry {
        mtime_ns: key.0,
        size: key.1,
        payload: Payload::Encoded { refs, reads },
        continuation,
        dirty: false,
    })
}

#[cfg(feature = "sqlite")]
fn load_previous(conn: &rusqlite::Connection, path: &Path, size: u64) -> Option<(u64, String)> {
    conn.prepare_cached("SELECT size,continuation FROM scan_cache WHERE path=?1 AND size<?2 AND interpretation_version=?3 AND continuation IS NOT NULL").inspect_err(note_sql).ok()?
        .query_row(rusqlite::params![path.to_string_lossy(),size as i64,INTERPRETATION_VERSION], |row| Ok((row.get::<_, i64>(0)? as u64,row.get(1)?))).inspect_err(note_sql).ok()
}

#[cfg(feature = "sqlite")]
fn admit_loaded(
    cache: &Cache,
    path: &Path,
    entry: Entry,
    reads: &crate::source::ReadSet,
) -> Option<(Vec<SessionRef>, crate::source::ReadSet)> {
    let mut entries = cache.entries.write().unwrap();
    let entry = entries.entry(path.to_path_buf()).or_insert(entry);
    entry
        .payload
        .known()
        .and_then(|(refs, captured)| captured.includes(reads).then(|| (refs.clone(), captured.clone())))
}

/// The persisted key a capture of `path` would use now.
#[cfg(all(test, feature = "sqlite"))]
pub(crate) fn stat_key(path: &Path) -> Option<(u64, u64)> {
    let mut reads = crate::source::ReadSet::default();
    reads.record(path);
    reads.signature(path)
}

/// Return cached refs for `path` when its captured identity (and every auxiliary read) is
/// unchanged; otherwise run `scan`, cache any non-empty result, and return it. One file may map
/// to several sessions (e.g. Gemini logs).
///
/// The capture happens **before** the scan, so a write racing the scan leaves a too-old stamp in
/// the cache, which misses next time and re-scans. The safe direction.
#[cfg(test)]
pub(crate) fn cached_scan_many<F>(path: &Path, scan: F) -> Vec<SessionRef>
where
    F: FnOnce() -> Vec<SessionRef>,
{
    let Ok(refs) =
        cached_scan_inner::<std::convert::Infallible, _>(path, None, Default::default(), |_| Ok((scan(), None)));
    refs
}

/// Single-result metadata scan with auxiliary identities captured before the caller reads them.
/// The existing primary cache retains and revalidates the complete baseline, including absence.
/// A failed scan is the caller's error and leaves the cache as it was.
pub(crate) fn cached_scan_with_reads<F>(
    path: &Path,
    reads: crate::source::ReadSet,
    scan: F,
) -> anyhow::Result<Option<SessionRef>>
where
    F: FnOnce() -> anyhow::Result<Option<SessionRef>>,
{
    cached_scan_inner(path, None, reads, |_| Ok((scan()?.into_iter().collect(), None)))
        .map(|refs: Vec<SessionRef>| refs.into_iter().next())
}

/// [`cached_scan_many_checked`] with auxiliary identities captured before the caller reads them, over a
/// scan that can fail (its error is returned, nothing cached).
#[cfg(feature = "sqlite")]
pub(crate) fn cached_scan_many_with_reads<F>(
    path: &Path,
    reads: crate::source::ReadSet,
    scan: F,
) -> anyhow::Result<Vec<SessionRef>>
where
    F: FnOnce() -> anyhow::Result<Vec<SessionRef>>,
{
    cached_scan_inner(path, None, reads, |_| Ok((scan()?, None)))
}

/// Opt-in append metadata state. Exact hits retain their existing guards; a continuation is
/// eligible only under the adapter's immutable-prefix append contract and sampled MarkHead. A
/// failed scan is the caller's error and leaves the cache as it was.
pub(crate) fn cached_scan_continued<F>(path: &Path, version: &str, scan: F) -> anyhow::Result<Option<SessionRef>>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> anyhow::Result<(Option<SessionRef>, Option<ScanContinuation>)>,
{
    cached_scan_continued_with_reads(path, version, Default::default(), scan)
}

/// [`cached_scan_continued`] with auxiliary dependencies captured before exact-hit lookup.
pub(crate) fn cached_scan_continued_with_reads<F>(
    path: &Path,
    version: &str,
    reads: crate::source::ReadSet,
    scan: F,
) -> anyhow::Result<Option<SessionRef>>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> anyhow::Result<(Option<SessionRef>, Option<ScanContinuation>)>,
{
    cached_scan_inner(path, Some(version), reads, |from| {
        let (reference, continuation) = scan(from)?;
        Ok((reference.into_iter().collect(), continuation))
    })
    .map(|refs: Vec<SessionRef>| refs.into_iter().next())
}

/// The one scan-cache path. A scan's error comes straight back, before anything is cached:
/// the cache never has to tell a failed scan's empty from a source that holds no session.
fn cached_scan_inner<E, F>(
    path: &Path,
    version: Option<&str>,
    reads: crate::source::ReadSet,
    scan: F,
) -> Result<Vec<SessionRef>, E>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> Result<(Vec<SessionRef>, Option<ScanContinuation>), E>,
{
    let context = read::Context::current();
    let mut refs = cached_scan_captured(path, version, reads, scan, &context)?;
    context.refill_turns(&mut refs);
    Ok(refs)
}

/// The process entry's capture of `path`, when it covers every read in `reads`.
fn in_process_candidate(
    cache: &Cache,
    path: &Path,
    reads: &crate::source::ReadSet,
) -> Option<(Vec<SessionRef>, crate::source::ReadSet)> {
    let entries = cache.entries.read().unwrap();
    entries
        .get(path)
        .and_then(|entry| entry.payload.known())
        .and_then(|(refs, captured)| captured.includes(reads).then(|| (refs.clone(), captured.clone())))
}

/// A candidate's refs when its capture is still fresh, else nothing: a stale capture falls
/// through to a scan.
fn fresh_candidate(candidate: Option<(Vec<SessionRef>, crate::source::ReadSet)>) -> Option<Vec<SessionRef>> {
    candidate
        .filter(|(_, captured)| captured.fresh(&mut crate::source::Probe::default()))
        .map(|(refs, _)| refs)
}

/// An exact-demand row loaded from sqlite and admitted into the process cache, when still
/// fresh: only a path with no process entry is loaded, and a concurrent in-process scan/count
/// update always wins over the older SQL snapshot.
#[cfg(feature = "sqlite")]
fn fresh_loaded(
    cache: &Cache,
    context: &read::Context,
    path: &Path,
    key: (u64, u64),
    reads: &crate::source::ReadSet,
) -> Option<Vec<SessionRef>> {
    // Do not reload an existing process entry, even when stale or invalid: a fresh scan replaces it.
    if cache.entries.read().unwrap().contains_key(path) {
        return None;
    }
    #[cfg(test)]
    context.note_lookup();
    let loaded = context.with_connection(|conn| load_entry(conn, path, key)).flatten();
    // A lazy standalone open may migrate interpretation; clear before retaining this scan.
    clear_invalidated(cache);
    let mut entry = loaded?;
    #[cfg(test)]
    context.note_row();
    // Decode outside locks, then choose the actual current process entry.
    entry.payload.parsed();
    fresh_candidate(admit_loaded(cache, path, entry, reads))
}

fn cached_scan_captured<E, F>(
    path: &Path,
    version: Option<&str>,
    mut reads: crate::source::ReadSet,
    scan: F,
    context: &read::Context,
) -> Result<Vec<SessionRef>, E>
where
    F: FnOnce(Option<&crate::stream::Checkpoint>) -> Result<(Vec<SessionRef>, Option<ScanContinuation>), E>,
{
    // One capture decides freshness: the path's full identity, inside the read set (with its
    // prefix sample when it may grow under a continuation). Its `(mtime, size)` only keys rows.
    reads.record_as(path, version.is_some());
    #[cfg_attr(not(feature = "sqlite"), allow(unused_variables))]
    let Some(key @ (_, size)) = reads.signature(path) else {
        return Ok(scan(None)?.0);
    };
    let cache = context.cache();
    clear_invalidated(&cache);
    // Exact hit in this process.
    if let Some(refs) = fresh_candidate(in_process_candidate(&cache, path, &reads)) {
        return Ok(refs);
    }
    // Exact demand persisted by an earlier run.
    #[cfg(feature = "sqlite")]
    if let Some(refs) = fresh_loaded(&cache, context, path, key, &reads) {
        return Ok(refs);
    }
    // The continuation a re-opened pass may resume from: the in-process entry's, or — for a
    // continuation demand with no process entry — sqlite's, which a newer process row overrides.
    #[allow(unused_mut)] // Persistence may replace the in-process candidate with a demanded row.
    let mut previous = version.and_then(|_| {
        let entries = cache.entries.read().unwrap();
        entries
            .get(path)
            .and_then(|entry| entry.continuation.as_ref().map(|state| (entry.size, state.clone())))
    });
    #[cfg(feature = "sqlite")]
    if version.is_some() && !cache.entries.read().unwrap().contains_key(path) {
        #[cfg(test)]
        context.note_lookup();
        previous = context
            .with_connection(|conn| load_previous(conn, path, size))
            .flatten();
        // As with exact demand, a newer in-process row wins over an older SQL snapshot.
        if let Some(entry) = cache.entries.read().unwrap().get(path) {
            previous = entry.continuation.as_ref().map(|state| (entry.size, state.clone()));
        }
    }
    // A continuation may end past its row's size: a pass that raced an append read the bytes
    // its own capture held. The head's identity and sampled bytes are what admit it, also at
    // the row's own size (a writer mid-`write`: new timestamps, old length).
    let prior = previous.and_then(|(old_size, text)| {
        if old_size > size {
            return None;
        }
        let prior: ScanContinuation = serde_json::from_str(&text).inspect_err(note).ok()?;
        (prior.dependencies.fresh(&mut crate::source::Probe::default())
            && prior.head.admit(path, version?, &prior.checkpoint).is_some())
        .then_some(prior)
    });
    if let Some(prior) = &prior {
        reads.merge(&prior.dependencies);
    }
    // The scan itself, against a resumed checkpoint when one was admitted.
    let (refs, mut continuation) = scan(prior.as_ref().map(|prior| &prior.checkpoint))?;
    if let Some(state) = &mut continuation {
        if let Some(prior) = &prior {
            state.dependencies.merge(&prior.dependencies);
        }
        reads.merge(&state.dependencies);
    }
    // A primary that only grew during the pass (`FileId::admits`) still holds what it read;
    // refusing that state restarted every pass over a live transcript from byte 0.
    let continuation = continuation
        .filter(|state| {
            Some(state.head.version.as_str()) == version
                && state.head.offset() == state.checkpoint.offset
                && reads.fresh_or_grown(&mut crate::source::Probe::default())
                && state.dependencies.fresh(&mut crate::source::Probe::default())
        })
        .and_then(|state| serde_json::to_string(&state).inspect_err(note).ok());
    // Only cache hits — empty results are cheap to recompute (a failed name/content check that never
    // read the file) and caching them would bloat the table with every non-transcript path.
    // Publish into this process (and mark for write-back) under one lock held only for the insert.
    if !refs.is_empty() {
        // Keep filesystem probes outside the cache lock. A pass raced by anything but growth of
        // its append-only primary publishes nothing, and one raced by growth never displaces a
        // newer row; its caller still gets its result.
        let mut probe = crate::source::Probe::default();
        let grown = version.is_some() && !reads.fresh(&mut probe);
        if grown && !reads.fresh_or_grown(&mut probe) {
            return Ok(refs);
        }
        let mut entries = cache.entries.write().unwrap();
        if grown && entries.get(path).is_some_and(|entry| entry.size > size) {
            return Ok(refs);
        }
        entries.insert(
            path.to_path_buf(),
            Entry {
                #[cfg(feature = "sqlite")]
                mtime_ns: key.0,
                size,
                payload: Payload::Parsed {
                    refs: refs.clone(),
                    reads,
                },
                continuation,
                #[cfg(feature = "sqlite")]
                dirty: true,
            },
        );
        cache.any_dirty.store(true, Ordering::Relaxed);
    }
    Ok(refs)
}

/// Original before-read metadata, never a fresh post-discovery stamp.
#[cfg(feature = "sqlite")]
pub(crate) fn captured_reads(path: &Path) -> Option<crate::source::ReadSet> {
    let cache = read::Context::current().cached()?;
    {
        let entries = cache.entries.read().inspect_err(note).ok()?;
        let e = entries.get(path)?;
        if let Some((_, reads)) = e.payload.known() {
            return Some(reads.clone());
        }
        if !e.payload.encoded() {
            return None;
        }
    }
    let mut entries = cache.entries.write().inspect_err(note).ok()?;
    let (_, reads) = entries.get_mut(path)?.payload.parsed()?;
    Some(reads.clone())
}

/// Single-result convenience over [`cached_scan_many_checked`].
#[cfg(test)]
pub(crate) fn cached_scan<F>(path: &Path, scan: F) -> Option<SessionRef>
where
    F: FnOnce() -> Option<SessionRef>,
{
    cached_scan_many(path, || scan().into_iter().collect())
        .into_iter()
        .next()
}

/// A single-result scan that can fail: its error comes back to the caller, nothing
/// cached, for discovery to report the source as skipped.
pub(crate) fn cached_scan_checked<F>(path: &Path, scan: F) -> anyhow::Result<Option<SessionRef>>
where
    F: FnOnce() -> anyhow::Result<Option<SessionRef>>,
{
    cached_scan_with_reads(path, Default::default(), scan)
}

/// [`cached_scan_many_checked`] over a scan that can fail (see [`cached_scan_checked`]).
pub(crate) fn cached_scan_many_checked<F>(path: &Path, scan: F) -> anyhow::Result<Vec<SessionRef>>
where
    F: FnOnce() -> anyhow::Result<Vec<SessionRef>>,
{
    cached_scan_inner(path, None, Default::default(), |_| Ok((scan()?, None)))
}

fn persistence_cache(path: &Path, prune: bool) -> Option<Arc<Cache>> {
    let cache = existing_cache(path)?;
    (cache.any_dirty.load(Ordering::Relaxed) || prune).then_some(cache)
}

/// Flush freshly-scanned entries to the catalog db in one transaction. With `prune`, also drop
/// rows whose files no longer exist so the table doesn't grow without bound — pass it after a
/// *full* [`crate::discover_all`] (the stat-per-row cost belongs on the slow path; per-harness
/// probe refreshes skip it). A failed flush means a re-scan next run, and one stderr line.
pub(crate) fn persist(prune: bool) {
    if crate::env::frozen() {
        return;
    }
    if persistence_cache(read::Context::current().path().unwrap_or(Path::new("")), prune).is_some() {
        #[cfg(feature = "sqlite")]
        read::Context::current().with_connection_mut(|conn| persist_in(conn, prune));
    }
}

#[cfg(feature = "sqlite")]
pub(crate) fn persist_in(conn: &mut rusqlite::Connection, prune: bool) {
    if crate::env::frozen() {
        return;
    }
    let Some(cache) = persistence_cache(Path::new(conn.path().unwrap_or("")), prune) else {
        return;
    };
    clear_invalidated(&cache);
    if let Err(e) = flush(conn, &cache, prune) {
        // The rows stay dirty for the next flush; this process says once that they did not land.
        crate::env::warn_once("the scan cache", e);
    }
}

#[cfg(feature = "sqlite")]
fn flush(conn: &mut rusqlite::Connection, cache: &Cache, prune: bool) -> rusqlite::Result<()> {
    // Full discovery still checks every persisted path, including deferred or malformed
    // payloads. Pruning therefore keeps its existing completeness semantics.
    let mut prune_paths: std::collections::HashSet<String> = if prune {
        let mut stmt = conn.prepare("SELECT path FROM scan_cache")?;
        let paths = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        paths
    } else {
        Default::default()
    };
    let mut g = cache.entries.write().unwrap();
    if prune {
        // Match the previous flush-then-prune behavior for newly scanned files that
        // vanished before publication, checking each persisted/dirty path only once.
        prune_paths.extend(
            g.iter()
                .filter(|(_, e)| e.dirty)
                .map(|(path, _)| path.to_string_lossy().into_owned()),
        );
    }
    let vanished: Vec<_> = prune_paths.into_iter().filter(|p| !Path::new(p).exists()).collect();
    if vanished.is_empty() && !g.values().any(|entry| entry.dirty) {
        return Ok(());
    }
    let tx = conn.transaction()?;
    let mut written = Vec::new();
    {
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs,interpretation_version,source_reads,continuation) VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
        for (path, e) in g.iter().filter(|(_, e)| e.dirty) {
            let Some((refs, reads)) = e.payload.known() else {
                continue;
            };
            let Ok(refs) = serde_json::to_string(refs) else {
                continue;
            };
            let Ok(reads) = serde_json::to_string(reads) else {
                continue;
            };
            stmt.execute(rusqlite::params![
                path.to_string_lossy(),
                e.mtime_ns as i64,
                e.size as i64,
                refs,
                INTERPRETATION_VERSION,
                reads,
                e.continuation
            ])?;
            written.push(path.clone());
        }
    }
    for path in &vanished {
        tx.execute("DELETE FROM scan_cache WHERE path=?1", [path])?;
    }
    tx.commit()?;
    // A failed transaction retains dirty flags for retry rather than losing its writes.
    for path in written {
        if let Some(entry) = g.get_mut(&path) {
            entry.dirty = false;
        }
    }
    for path in vanished {
        g.remove(Path::new(&path));
    }
    cache
        .any_dirty
        .store(g.values().any(|entry| entry.dirty), Ordering::Relaxed);
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    #[test]
    fn failed_scan_cache_read_reports_and_retries_after_repair() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let path = Path::new("fixture.jsonl");
        assert!(load_entry(&conn, path, (1, 2)).is_none());
        assert!(!crate::env::warn_once("the scan cache", "already reported"));
        conn.execute_batch(
            "CREATE TABLE scan_cache(path,mtime_ns,size,interpretation_version,refs,source_reads,continuation)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_cache VALUES (?1,1,2,?2,'[]','[]',NULL)",
            rusqlite::params![path.to_string_lossy(), INTERPRETATION_VERSION],
        )
        .unwrap();
        assert!(load_entry(&conn, path, (1, 2)).is_some());
    }

    #[test]
    fn auxiliary_scan_baselines_invalidate_changes_and_preserve_raced_stamps() {
        let world = crate::test_world("scan-auxiliary-baselines");
        let path = world.base.join("source");
        let sidecar = world.base.join("state");
        std::fs::write(&path, "unchanged primary").unwrap();
        let baseline = || {
            let mut reads = crate::source::ReadSet::default();
            reads.record(&sidecar);
            reads
        };
        let reference = |id| SessionRef::new(id, crate::Harness::Claude, &path);
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Ok(Some(reference("absent"))))
                .unwrap()
                .unwrap()
                .id,
            "absent"
        );
        cached_scan_with_reads(&path, baseline(), || panic!("unchanged absent sidecar must hit")).unwrap();
        persist(false);
        current_cache().entries.write().unwrap().clear();
        cached_scan_with_reads(&path, baseline(), || panic!("persisted auxiliary baseline must hit")).unwrap();
        std::fs::write(&sidecar, "new title").unwrap();
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Ok(Some(reference("present"))))
                .unwrap()
                .unwrap()
                .id,
            "present"
        );
        std::fs::write(&sidecar, "before race").unwrap();
        cached_scan_with_reads(&path, baseline(), || {
            std::fs::write(&sidecar, "after race").unwrap();
            Ok(Some(reference("raced")))
        })
        .unwrap();
        assert!(!captured_reads(&path)
            .unwrap()
            .fresh(&mut crate::source::Probe::default()));
        assert_eq!(
            cached_scan_with_reads(&path, baseline(), || Ok(Some(reference("refreshed"))))
                .unwrap()
                .unwrap()
                .id,
            "refreshed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn continuation_checks_auxiliary_identity_and_does_not_publish_raced_passes() {
        let w = crate::test_world("scan-continuation-freshness");
        let path = w.base.join("s.jsonl");
        let aux = w.base.join("dependency");
        std::fs::write(&path, "first\n").unwrap();
        std::fs::write(&aux, "one").unwrap();
        let version = "test-discovery/1";
        let r = SessionRef::new("s", crate::Harness::Claude, &path);
        let state = || {
            let cp = crate::stream::Checkpoint::new(std::fs::metadata(&path).unwrap().len());
            let mut dependencies = crate::source::ReadSet::default();
            dependencies.record(&aux);
            ScanContinuation {
                head: crate::resume::MarkHead::of(&path, &cp, version).unwrap(),
                checkpoint: cp,
                dependencies,
            }
        };
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none());
            Ok((Some(r.clone()), Some(state())))
        })
        .unwrap();
        cached_scan_continued(&path, version, |_| {
            panic!("unchanged primary and dependency must hit process cache")
        })
        .unwrap();
        persist(false);
        current_cache().entries.write().unwrap().clear();
        cached_scan_continued(&path, version, |_| {
            panic!("persisted exact hit must retain auxiliary guards")
        })
        .unwrap();
        // A dependency-only change forces complete metadata discovery, even with identical
        // primary bytes and no opportunity for append continuation.
        std::fs::write(&aux, "replacement auxiliary").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none());
            Ok((Some(r.clone()), Some(state())))
        })
        .unwrap();
        cached_scan_continued(&path, version, |_| panic!("refreshed auxiliary must hit")).unwrap();
        std::fs::write(&path, "first\nsecond\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert_eq!(prior.unwrap().offset, 6);
            Ok((Some(r.clone()), Some(state())))
        })
        .unwrap();
        std::fs::write(&aux, "two").unwrap();
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_none(), "changed auxiliary rejects append authority");
            Ok((Some(r.clone()), Some(state())))
        })
        .unwrap();
        let cache = current_cache();
        let old_size = cache.entries.read().unwrap()[&path].size;
        std::fs::write(&path, "first\nsecond\nthird\nfourth\n").unwrap();
        cached_scan_continued(&path, version, |prior| {
            assert!(prior.is_some());
            let continuation = state();
            std::fs::write(&aux, "new").unwrap();
            Ok((Some(r.clone()), Some(continuation)))
        })
        .unwrap();
        assert_eq!(
            cache.entries.read().unwrap()[&path].size,
            old_size,
            "raced dependency cannot bless a new cache tuple"
        );
        let result = cached_scan_continued(&path, version, |_| {
            std::fs::write(&path, "first\nsecond\nthird\nfourth\nfifth\n").unwrap();
            let newer = cached_scan_continued(&path, version, |_| {
                Ok((
                    Some(SessionRef {
                        message_count: Some(9),
                        ..r.clone()
                    }),
                    Some(state()),
                ))
            })
            .unwrap()
            .unwrap();
            assert_eq!(newer.message_count, Some(9));
            Ok((
                Some(SessionRef {
                    message_count: Some(1),
                    ..r.clone()
                }),
                Some(state()),
            ))
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.message_count, Some(1));
        assert_eq!(
            cache.entries.read().unwrap()[&path].payload.known().unwrap().0[0].message_count,
            Some(9),
            "older raced primary pass cannot replace a newer installed row"
        );
    }

    #[test]
    fn lazy_payload_preserves_demand_guards_and_malformed_fallback() {
        let w = crate::test_world("scan-lazy");
        let mut conn = crate::catalog::open_db().unwrap();
        let paths: Vec<_> = ["valid", "invalid", "stale", "invalid-reads"]
            .map(|name| w.base.join(name))
            .into();
        let mut expected = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(path, "content").unwrap();
            let r = SessionRef {
                harness: crate::Harness::Claude,
                id: format!("row-{i}"),
                path: path.clone(),
                cwd: None,
                title: Some("metadata".into()),
                created_at: None,
                updated_at: None,
                message_count: None,
            };
            let mut reads = crate::source::ReadSet::default();
            reads.record(path);
            let (mtime, size) = stat_key(path).unwrap();
            conn.execute(
                "INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads,interpretation_version) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![path.to_string_lossy(), mtime as i64, size as i64,
                    if i == 0 || i == 3 { serde_json::to_string(std::slice::from_ref(&r)).unwrap() } else { "invalid".into() },
                    if i == 3 { "invalid".into() } else { serde_json::to_string(&reads).unwrap() }, INTERPRETATION_VERSION],
            ).unwrap();
            expected.push((r, reads));
        }
        let cache = current_cache();
        clear_invalidated(&cache);
        assert!(
            cache.entries.read().unwrap().is_empty(),
            "no unrelated persisted payload is loaded"
        );
        assert_eq!(
            cached_scan(&paths[0], || panic!("valid persisted row must hit"))
                .unwrap()
                .id,
            expected[0].0.id
        );
        assert_eq!(captured_reads(&paths[0]).unwrap(), expected[0].1);
        assert!(
            !cache.entries.read().unwrap().contains_key(&paths[1]),
            "unrelated malformed row stays unloaded"
        );
        assert!(captured_reads(&paths[1]).is_none());
        assert!(cached_scan_many(&paths[1], Vec::new).is_empty());
        assert!(matches!(
            cache.entries.read().unwrap()[&paths[1]].payload,
            Payload::Invalid
        ));
        let repaired = cached_scan(&paths[1], || Some(expected[1].0.clone())).unwrap();
        assert_eq!(repaired.id, expected[1].0.id);
        std::fs::write(&paths[2], "appended content").unwrap();
        let fresh = cached_scan(&paths[2], || {
            assert!(
                !cache.entries.read().unwrap().contains_key(&paths[2]),
                "tuple mismatch avoids persisted payload loading"
            );
            Some(SessionRef {
                message_count: Some(3),
                ..expected[2].0.clone()
            })
        })
        .unwrap();
        assert_eq!(fresh.message_count, Some(3));
        assert!(cached_scan_many(&paths[3], Vec::new).is_empty());
        assert!(captured_reads(&paths[3]).is_none(), "invalid ReadSet must also miss");
        assert_eq!(
            cached_scan(&paths[3], || Some(expected[3].0.clone())).unwrap().id,
            expected[3].0.id
        );
        persist_in(&mut conn, false);
        for (i, path) in paths.iter().enumerate() {
            let mut entry = load_entry(&conn, path, stat_key(path).unwrap()).unwrap();
            let (refs, reads) = entry.payload.parsed().unwrap();
            assert_eq!(refs[0].message_count, [None, None, Some(3), None][i]);
            assert!(reads.fresh(&mut crate::source::Probe::default()));
        }
        std::fs::remove_file(&paths[1]).unwrap();
        persist_in(&mut conn, true);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1",
                [paths[1].to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn interpretation_upgrade_rejects_persisted_and_process_scan_rows() {
        let w = crate::test_world("scan-interpretation");
        let path = w.base.join("unchanged.jsonl");
        std::fs::write(&path, "unchanged").unwrap();
        let old = SessionRef {
            harness: crate::Harness::Codex,
            id: "old".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: Some(4),
        };
        assert_eq!(cached_scan(&path, || Some(old.clone())).unwrap().id, "old");
        persist(false);
        let conn = crate::catalog::open_db().unwrap();
        conn.execute(
            "UPDATE scan_cache SET interpretation_version=?1",
            [INTERPRETATION_VERSION - 1],
        )
        .unwrap();
        conn.execute(
            "UPDATE catalog_meta SET value=?1 WHERE key='discovery_version'",
            [(INTERPRETATION_VERSION - 1).to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync','1');
            INSERT OR REPLACE INTO watched(harness,path,id) VALUES('gemini','old-watch',X'');",
        )
        .unwrap();
        let conn = crate::catalog::open_db().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM watched", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(crate::catalog::last_full_sync().is_none());
        // The first full discovery after the upgrade announces itself, once.
        let reader = crate::catalog::Reader::open().unwrap();
        assert!(reader.take_rediscovery_notice());
        assert!(!reader.take_rediscovery_notice());
        assert!(
            load_entry(&conn, &path, stat_key(&path).unwrap()).is_none(),
            "old persisted interpretation ignored"
        );
        let current = SessionRef {
            id: "current".into(),
            message_count: Some(2),
            ..old
        };
        assert_eq!(
            cached_scan(&path, || Some(current)).unwrap().id,
            "current",
            "process entry invalidated despite unchanged bytes"
        );
        assert_eq!(
            cached_scan(&path, || panic!("current scan must stay cached"))
                .unwrap()
                .message_count,
            Some(2)
        );
        persist(false);
        assert_eq!(
            conn.query_row(
                "SELECT interpretation_version FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
            INTERPRETATION_VERSION
        );
        // Old binaries omit the added nullable field on replacement: their row remains stale.
        conn.execute(
            "INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs) VALUES(?1,1,1,'[]')",
            [path.to_string_lossy()],
        )
        .unwrap();
        assert!(load_entry(&conn, &path, stat_key(&path).unwrap()).is_none());
    }
    #[test]
    fn failed_flush_retries_and_prunes_newly_vanished_entries() {
        let w = crate::test_world("scan-persist-retry");
        let path = w.base.join("retry.jsonl");
        std::fs::write(&path, "data").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "retry".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: Some(1),
        };
        let conn = crate::catalog::open_db().unwrap();
        cached_scan(&path, || Some(r.clone()));
        conn.execute_batch(
            "CREATE TRIGGER reject_cache BEFORE INSERT ON scan_cache BEGIN SELECT RAISE(ABORT,'retry'); END;",
        )
        .unwrap();
        persist(false);
        assert!(current_cache().entries.read().unwrap().get(&path).unwrap().dirty);
        conn.execute_batch("DROP TRIGGER reject_cache;").unwrap();
        persist(false);
        assert!(!current_cache().entries.read().unwrap().get(&path).unwrap().dirty);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let vanished = w.base.join("newly-vanished.jsonl");
        std::fs::write(&vanished, "data").unwrap();
        cached_scan(&vanished, || {
            Some(SessionRef {
                path: vanished.clone(),
                ..r
            })
        });
        std::fs::remove_file(&vanished).unwrap();
        // Include a malformed persisted row: pruning must not rely only on loaded entries.
        conn.execute("INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads) VALUES('missing-malformed',1,1,'broken','broken')", []).unwrap();
        persist(true);
        assert!(!current_cache().entries.read().unwrap().contains_key(&vanished));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1 OR path='missing-malformed'",
                [vanished.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn discovery_mutation_keeps_original_identity_and_forces_rescan() {
        let w = crate::test_world("scan-mutation");
        let path = w.base.join("raced.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "old".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: Some(1),
        };
        cached_scan(&path, || {
            std::fs::write(&path, "after!").unwrap();
            Some(r.clone())
        })
        .unwrap();
        let reads = captured_reads(&path).unwrap();
        crate::catalog::replace_harness(
            crate::Harness::Claude,
            std::slice::from_ref(&r),
            &[],
            &Default::default(),
        );
        let conn = crate::catalog::open_db().unwrap();
        let json: String = conn
            .query_row(
                "SELECT reads FROM primary_sources WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        let published: crate::source::ReadSet = serde_json::from_str(&json).unwrap();
        assert_eq!(published, reads, "catalog must publish the original captured identity");
        assert!(
            !reads.fresh(&mut crate::source::Probe::default()),
            "publication must retain the before-read identity"
        );
        let fresh = cached_scan(&path, || Some(SessionRef { id: "new".into(), ..r })).unwrap();
        assert_eq!(fresh.id, "new");
        persist(false);
        let mut loaded = load_entry(&conn, &path, stat_key(&path).unwrap()).unwrap();
        assert!(loaded
            .payload
            .parsed()
            .unwrap()
            .1
            .fresh(&mut crate::source::Probe::default()));
    }
}
