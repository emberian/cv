//! SQLite plumbing for the database-backed stores (Cursor, Devin, Goose, Hermes, OpenClaw,
//! OpenCode, Zed). One invariant lives here so no adapter can get it wrong: **cv never modifies a
//! harness's database or WAL**, and under `--frozen` never touches `-shm` either. [`open_ro`]
//! chooses from the database's sidecars and mode:
//! - no WAL frames (`-wal` absent or empty) and no `-journal`: the original with
//!   `immutable=1`, so SQLite opens no `-wal`/`-shm` and takes no locks;
//! - live WAL (`-wal` has frames, `-shm` exists, no `-journal`, not `--frozen`): the live store
//!   opened `SQLITE_OPEN_READ_ONLY` with a short `busy_timeout` and an immediate `BEGIN DEFERRED`
//!   read transaction (`PRAGMA schema_version`), so SQLite's WAL MVCC pins a coherent snapshot
//!   without copying multi-hundred-MB stores when other sessions append to `-wal`;
//! - `--frozen`, rollback `-journal`, retained WAL without `-shm`, or genuine `SQLITE_BUSY` / lock
//!   contention on live open: a private [`ColdSnapshot`] of the database and its `-wal`/`-journal`
//!   (never `-shm`, which SQLite rebuilds in the private directory), with bounded TTL reuse so
//!   bursts under contention or `--frozen` do not copy the database repeatedly.
//!
//! Single-user adapters bracket operations with [`ReadConnection::read`]; shared multi-writer
//! stores bracket coherent point-in-time scans with [`ReadConnection::read_snapshot`] and
//! per-session reads with [`read_session`], which validates that session's own revision and schema
//! version rather than aborting when another session appends to `-wal`.

use crate::json::{ReadInput, Validation};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::Path;

mod certify;
#[cfg(test)]
pub(crate) use certify::assert_rows_move_alone;
pub(crate) use certify::{fold_value, Certificates};
mod derived;
#[cfg(test)]
pub(crate) mod export_tests;
pub(crate) mod listing;
pub(crate) use derived::derived;
#[cfg(test)]
pub(crate) use derived::DERIVE_OPENS;
mod selection;
mod snapshots;
pub(crate) use selection::Selection;

const LIVE_WAL_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);
const COLD_SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_millis(250);
const COLD_COPY_RETRIES: usize = 3;

/// A reader retains both its private backing (when needed) and the original provenance.
/// Use `read`, `read_snapshot` or `read_session` around an adapter operation, and carry `reads()`
/// into lazy source captures.
pub(crate) struct ReadConnection {
    connection: Connection,
    reads: crate::source::ReadSet,
    reads_exact_at_open: bool,
    isolated_snapshot: bool,
    _snapshot: Option<std::sync::Arc<ColdSnapshot>>,
    _scope: Option<ReadScope>,
}

impl Drop for ReadConnection {
    fn drop(&mut self) {
        if !self.connection.is_autocommit() {
            let _ = self.connection.execute_batch("ROLLBACK;");
        }
    }
}

/// Tests read the store the way cv sees it without the bracket; product code has only
/// [`ReadConnection::read`], [`ReadConnection::read_snapshot`] and [`read_session`].
#[cfg(test)]
impl std::ops::Deref for ReadConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl ReadConnection {
    #[cfg(test)]
    pub(crate) fn reads(&self) -> &crate::source::ReadSet {
        &self.reads
    }

    /// The one way to the connection for strict whole-file provenance: `operation` runs between
    /// the before and after checks of every captured path, and gets the reader's dependencies to
    /// carry into lazy captures.
    pub(crate) fn read<T>(
        &self,
        operation: impl FnOnce(&Connection, &crate::source::ReadSet) -> Result<T>,
    ) -> Result<T> {
        check_reads(&self.reads)?;
        let result = operation(&self.connection, &self.reads);
        // A mutation takes precedence over a body/SQL failure too.
        check_reads(&self.reads)?;
        result
    }

    /// Coherent point-in-time read over a shared store (discovery, per-session row certificates,
    /// derived forest scans): when pinned by WAL MVCC (`BEGIN DEFERRED`) or a private
    /// [`ColdSnapshot`], concurrent WAL appends by other sessions do not invalidate the snapshot;
    /// `immutable=1` opens have no MVCC and retain before/after file checks.
    pub(crate) fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&Connection, &crate::source::ReadSet) -> Result<T>,
    ) -> Result<T> {
        if !self.isolated_snapshot {
            return self.read(operation);
        }
        operation(&self.connection, &self.reads)
    }
}

/// Retain at an operation boundary that reads multiple sessions. Dropping the final scope or
/// reader releases cached payloads and their directories. Concurrent/nested scopes share this
/// bounded cache. Outermost scope entry/exit also reclaims marked, unlocked owners left by dead
/// processes; a fixed tiny namespace gate persists. Abrupt standalone death needs a later scope.
pub(crate) struct ReadScope {
    cache: std::sync::Arc<SnapshotCache>,
    _boundary: Option<std::sync::Arc<ReadBoundary>>,
}

struct SnapshotCache {
    state: std::sync::Mutex<SnapshotCacheState>,
    namespace: snapshots::Namespace,
    owner: std::sync::Mutex<Option<std::sync::Arc<snapshots::Owner>>>,
}

struct ReadBoundary(snapshots::Namespace);
impl Drop for ReadBoundary {
    fn drop(&mut self) {
        self.0.reap();
    }
}

thread_local! {
    // The guard may move between threads. Its last Arc drop performs exit cleanup; the
    // origin thread only retains a Weak, never a depth that must be decremented elsewhere.
    static BOUNDARY: std::cell::RefCell<std::sync::Weak<ReadBoundary>> = const { std::cell::RefCell::new(std::sync::Weak::new()) };
}

/// One private backing per store this operation copied, kept until the operation's last scope
/// drops or the store's state moves (then the next open copies it again and the old backing goes
/// once no reader holds it). Bounded by the number of distinct stores the operation opens, never by
/// bytes: a store with WAL frames is copied once per state per operation, however large (an
/// eviction by size copied a 4.2 GB store again each time another database opened). Disk is each
/// such store's DB/WAL/journal bytes once (a clone on APFS shares its blocks until the store is
/// written); memory is one small record per store.
#[derive(Default)]
struct SnapshotCacheState {
    retained: std::collections::HashMap<std::path::PathBuf, std::sync::Arc<ColdSnapshot>>,
}

pub(crate) fn read_scope() -> ReadScope {
    let mut scope = reader_scope();
    scope._boundary = Some(operation_boundary(&scope.cache.namespace));
    scope
}

fn operation_boundary(namespace: &snapshots::Namespace) -> std::sync::Arc<ReadBoundary> {
    BOUNDARY.with(|slot| {
        if let Some(boundary) = slot.borrow().upgrade() {
            return boundary;
        }
        namespace.reap();
        let boundary = std::sync::Arc::new(ReadBoundary(namespace.clone()));
        *slot.borrow_mut() = std::sync::Arc::downgrade(&boundary);
        boundary
    })
}

fn reader_scope() -> ReadScope {
    use std::sync::{Mutex, Weak};
    #[cfg(not(test))]
    static ACTIVE: std::sync::OnceLock<Mutex<Weak<SnapshotCache>>> = std::sync::OnceLock::new();
    // libtest runs tests concurrently in one process. A reader in another test must not
    // retain this test's backings after its final scope drops.
    #[cfg(test)]
    thread_local! {
        static ACTIVE: Mutex<Weak<SnapshotCache>> = const { Mutex::new(Weak::new()) };
    }
    #[cfg(test)]
    return ACTIVE.with(scope_from);
    #[cfg(not(test))]
    scope_from(ACTIVE.get_or_init(|| Mutex::new(Weak::new())))
}

fn scope_from(active: &std::sync::Mutex<std::sync::Weak<SnapshotCache>>) -> ReadScope {
    use std::sync::{Arc, Mutex};
    let mut active = active.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = active.upgrade().unwrap_or_else(|| {
        let cache = Arc::new(SnapshotCache {
            state: Mutex::new(SnapshotCacheState::default()),
            namespace: snapshots::Namespace::current(),
            owner: Mutex::new(None),
        });
        *active = Arc::downgrade(&cache);
        cache
    });
    ReadScope { cache, _boundary: None }
}

struct ColdSnapshot {
    reads: crate::source::ReadSet,
    birth: [u8; 32],
    created_at: std::sync::Mutex<std::time::Instant>,
    directory: std::path::PathBuf,
    directory_identity: snapshots::Identity,
    database: std::path::PathBuf,
    _owner: Option<std::sync::Arc<snapshots::Owner>>,
}

impl ColdSnapshot {
    fn reusable(&self, path: &Path, allow_ttl: bool) -> bool {
        if self.reads.fresh(&mut crate::source::Probe::default()) {
            return true;
        }
        if !allow_ttl || self.birth != crate::source::store_birth(path) {
            return false;
        }
        self.created_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            < COLD_SNAPSHOT_TTL
    }
}

impl Drop for ColdSnapshot {
    fn drop(&mut self) {
        snapshots::remove_snapshot_directory(&self.directory, self.directory_identity);
    }
}

fn record_store_reads(path: &Path) -> crate::source::ReadSet {
    let mut reads = crate::source::ReadSet::default();
    reads.record(path);
    reads.record(&sidecar(path, "-wal"));
    reads.record(&sidecar(path, "-journal"));
    reads
}

fn check_reads(reads: &crate::source::ReadSet) -> Result<()> {
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "SQLite source dependencies changed or unavailable"
    );
    Ok(())
}

/// Discovery over one database, cached in the discovery cache under the read set [`open_ro`]
/// captures (the file, its WAL and rollback journal; SHM is coordination state). An unchanged
/// store is neither reopened nor rescanned. `discover` must depend only on that database; its
/// errors are returned and never cached.
pub(crate) fn cached_discover(
    db: &Path,
    discover: impl FnOnce() -> Result<Vec<crate::SessionRef>>,
) -> Result<Vec<crate::SessionRef>> {
    let path = std::path::absolute(db)?;
    let reads = record_store_reads(&path);
    crate::discover_cache::cached_scan_many_with_reads(&path, reads, discover)
}

fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

/// One consistent view of `conn` for the reads that follow, until the returned guard drops: a
/// deferred read transaction (it writes nothing), or none when an enclosing one already holds the
/// snapshot. Navigation and payload queries of one operation must see the same history.
pub(crate) fn snapshot(conn: &Connection) -> Result<Option<rusqlite::Transaction<'_>>> {
    Ok(conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?)
}

/// Complete one adapter operation under the reader's existing before/after provenance checks.
pub(crate) fn read<T>(
    path: &Path,
    operation: impl FnOnce(&Connection, &crate::source::ReadSet) -> Result<T>,
) -> Result<T> {
    open_ro(path)?.read(operation)
}

/// Read one session from a shared multi-writer SQLite store inside a coherent WAL MVCC snapshot
/// (or [`ColdSnapshot`] fallback), then verify that *this session's* revision and schema version
/// still match. Writes and checkpoints by unrelated sessions in the same database do not fail the
/// read or trigger full-database copies.
pub(crate) fn read_session<T>(
    path: &Path,
    certify: impl Fn(&Connection) -> Result<crate::source::Rows> + Send + Sync + 'static,
    operation: impl FnOnce(
        &Connection,
        &crate::source::ReadSet,
        &std::sync::Arc<crate::json::SqliteSessionGuard>,
    ) -> Result<T>,
) -> Result<T> {
    let abs_path = std::path::absolute(path)?;
    let reader = open_ro(&abs_path)?;
    if !reader.isolated_snapshot {
        check_reads(&reader.reads)?;
    }
    let expected_schema: i64 = reader.connection.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    let expected_rows = certify(&reader.connection)?;
    let birth = crate::source::store_birth(&abs_path);
    let open_reads = reader.reads.clone();
    let exact_open = reader.reads_exact_at_open;
    let certify = std::sync::Arc::new(certify);
    let guard = crate::json::SqliteSessionGuard::new(move || {
        if exact_open && open_reads.fresh(&mut crate::source::Probe::default()) {
            return true;
        }
        if crate::source::store_birth(&abs_path) != birth {
            return false;
        }
        let Ok(now) = open_ro_fresh(&abs_path) else {
            return false;
        };
        let same_schema = now
            .connection
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .ok()
            == Some(expected_schema);
        same_schema && certify(&now.connection).ok() == Some(expected_rows)
    });
    let result = operation(&reader.connection, &reader.reads, &guard);
    guard.end_pass();
    drop(reader);
    // A mutation of this session or the store's schema takes precedence over a body/SQL failure.
    anyhow::ensure!(guard.valid(), "SQLite source dependencies changed or unavailable");
    result
}

/// Whether byte-exact indexed facts may replace native ID equality. Any explicit collation
/// or non-TEXT affinity conservatively retains the adapter's original native query.
pub(crate) fn binary_text_columns(conn: &Connection, table: &str, names: &[&str]) -> Result<bool> {
    use rusqlite::OptionalExtension as _;
    let ddl = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if ddl.is_none_or(|ddl| ddl.to_ascii_uppercase().contains("COLLATE")) {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT name,type FROM pragma_table_info(?1)")?;
    let columns = stmt
        .query_map([table], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names.iter().all(|name| {
        columns
            .iter()
            .any(|(column, kind)| column == name && kind.eq_ignore_ascii_case("TEXT"))
    }))
}

/// Open a harness database read-only (see the module docs).
/// Live WAL stores with `-shm` open directly with `BEGIN DEFERRED` MVCC snapshot isolation;
/// `--frozen`, rollback journals, retained WALs without `-shm`, and `SQLITE_BUSY` contention fall
/// back to a private [`ColdSnapshot`] with bounded TTL reuse.
pub(crate) fn open_ro(path: &Path) -> Result<ReadConnection> {
    open_ro_inner(path, true)
}

fn open_ro_fresh(path: &Path) -> Result<ReadConnection> {
    open_ro_inner(path, false)
}

fn open_ro_inner(path: &Path, allow_ttl_reuse: bool) -> Result<ReadConnection> {
    let path = std::path::absolute(path)?;
    let reads = record_store_reads(&path);
    let has_frames = std::fs::metadata(sidecar(&path, "-wal")).is_ok_and(|m| m.len() > 0);
    let has_journal = sidecar(&path, "-journal").exists();
    let immutable = !has_frames && !has_journal;
    if immutable {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let connection = Connection::open_with_flags(immutable_uri(&path), flags)
            .with_context(|| format!("opening {}", path.display()))?;
        check_reads(&reads)?;
        return Ok(ReadConnection {
            connection,
            reads,
            reads_exact_at_open: true,
            isolated_snapshot: false,
            _snapshot: None,
            _scope: None,
        });
    }

    let has_shm = sidecar(&path, "-shm").exists();
    if !crate::env::frozen() && has_frames && !has_journal && has_shm {
        if allow_ttl_reuse {
            if let Some(cached) = try_open_cached_cold_ro(&path, true)? {
                return Ok(cached);
            }
        }
        match open_live_wal(&path, reads) {
            Ok(conn) => return Ok(conn),
            Err(err) if is_lock_or_open_contention(&err) => {
                return open_cold_ro_inner(&path, allow_ttl_reuse, COLD_COPY_RETRIES, |_| Ok(()));
            }
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        }
    }

    open_cold_ro_inner(&path, allow_ttl_reuse, COLD_COPY_RETRIES, |_| Ok(()))
}

fn open_live_wal(path: &Path, reads: crate::source::ReadSet) -> rusqlite::Result<ReadConnection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(LIVE_WAL_BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA query_only = ON; BEGIN DEFERRED;")?;
    // Reading `schema_version` forces SQLite's `walBeginReadTransaction` immediately so the
    // connection pins its MVCC read mark (or surfaces `SQLITE_BUSY` here for fallback).
    let _: i64 = connection.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    let reads_exact_at_open = reads.fresh(&mut crate::source::Probe::default());
    Ok(ReadConnection {
        connection,
        reads,
        reads_exact_at_open,
        isolated_snapshot: true,
        _snapshot: None,
        _scope: None,
    })
}

fn is_lock_or_open_contention(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;
    let rusqlite::Error::SqliteFailure(sqlite_err, _) = err else {
        return false;
    };
    matches!(
        sqlite_err.code,
        ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::ReadOnly
            | ErrorCode::CannotOpen
            | ErrorCode::PermissionDenied
            | ErrorCode::SystemIoFailure
    ) || (sqlite_err.extended_code & 0xff) == rusqlite::ffi::SQLITE_PROTOCOL
}

fn try_open_cached_cold_ro(path: &Path, allow_ttl_reuse: bool) -> Result<Option<ReadConnection>> {
    use std::sync::Arc;
    let scope = reader_scope();
    let snapshot = {
        let cache = scope
            .cache
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot cache poisoned"))?;
        cache
            .retained
            .get(path)
            .filter(|s| s.reusable(path, allow_ttl_reuse))
            .cloned()
    };
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&snapshot.database, flags)
        .with_context(|| format!("opening {}", path.display()))?;
    let reads_exact_at_open = snapshot.reads.fresh(&mut crate::source::Probe::default());
    Ok(Some(ReadConnection {
        connection,
        reads: snapshot.reads.clone(),
        reads_exact_at_open,
        isolated_snapshot: true,
        _snapshot: Some(Arc::clone(&snapshot)),
        _scope: Some(scope),
    }))
}

#[cfg(test)]
fn open_cold_ro(path: &Path) -> Result<ReadConnection> {
    let path = std::path::absolute(path)?;
    open_cold_ro_inner(&path, true, COLD_COPY_RETRIES, |_| Ok(()))
}

#[cfg(test)]
fn open_ro_with_copy_check(path: &Path, copied: impl FnOnce(&Path) -> Result<()>) -> Result<ReadConnection> {
    let path = std::path::absolute(path)?;
    let reads = record_store_reads(&path);
    let has_frames = std::fs::metadata(sidecar(&path, "-wal")).is_ok_and(|m| m.len() > 0);
    let has_journal = sidecar(&path, "-journal").exists();
    if !has_frames && !has_journal {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let connection = Connection::open_with_flags(immutable_uri(&path), flags)
            .with_context(|| format!("opening {}", path.display()))?;
        check_reads(&reads)?;
        return Ok(ReadConnection {
            connection,
            reads,
            reads_exact_at_open: true,
            isolated_snapshot: false,
            _snapshot: None,
            _scope: None,
        });
    }
    open_cold_ro_inner(&path, true, 1, copied)
}

fn open_cold_ro_inner(
    path: &Path,
    allow_ttl_reuse: bool,
    max_attempts: usize,
    copied: impl FnOnce(&Path) -> Result<()>,
) -> Result<ReadConnection> {
    use std::sync::Arc;
    if let Some(cached) = try_open_cached_cold_ro(path, allow_ttl_reuse)? {
        return Ok(cached);
    }
    let scope = reader_scope();
    let mut copied = Some(copied);
    let mut last_error = None;
    for attempt in 0..max_attempts.max(1) {
        let reads = record_store_reads(path);
        let birth = crate::source::store_birth(path);
        let db_before = crate::source::FileId::at(path);
        let owner = {
            let mut slot = scope
                .cache
                .owner
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot owner poisoned"))?;
            if slot.is_none() {
                *slot = Some(scope.cache.namespace.owner()?);
            }
            Arc::clone(slot.as_ref().unwrap())
        };
        let directory = owner.snapshot_directory()?;
        let directory_identity = snapshots::snapshot_identity(&directory)?;
        let mut snapshot = ColdSnapshot {
            reads: reads.clone(),
            birth,
            created_at: std::sync::Mutex::new(std::time::Instant::now()),
            database: directory.join("store.db"),
            directory,
            directory_identity,
            _owner: Some(owner),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&snapshot.directory, std::fs::Permissions::from_mode(0o700))?;
        }
        // std::fs::copy uses clonefile on APFS and bounded OS copying elsewhere. Never copy
        // SHM: it contains live coordination state; SQLite rebuilds it only in our directory.
        std::fs::copy(path, &snapshot.database)?;
        for suffix in ["-wal", "-journal"] {
            let original = sidecar(path, suffix);
            if original.exists() {
                std::fs::copy(original, sidecar(&snapshot.database, suffix))?;
            }
        }
        if let Some(hook) = copied.take() {
            hook(&snapshot.database)?;
        }
        let exact = reads.fresh(&mut crate::source::Probe::default());
        let wal_append_only_on_final_retry = max_attempts > 1
            && attempt + 1 == max_attempts
            && db_before.is_some()
            && crate::source::FileId::at(path) == db_before
            && crate::source::store_birth(path) == birth
            && !sidecar(path, "-journal").exists();
        if !exact && !wal_append_only_on_final_retry {
            last_error = Some(anyhow::anyhow!("SQLite source dependencies changed or unavailable"));
            continue;
        }
        if !exact {
            snapshot.reads = record_store_reads(path);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(&snapshot.database, flags)
            .with_context(|| format!("opening {}", path.display()))?;
        let snapshot = Arc::new(snapshot);
        {
            let mut cache = scope
                .cache
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot cache poisoned"))?;
            cache.retained.insert(path.to_path_buf(), Arc::clone(&snapshot));
        }
        return Ok(ReadConnection {
            connection,
            reads: snapshot.reads.clone(),
            reads_exact_at_open: exact,
            isolated_snapshot: true,
            _snapshot: Some(snapshot),
            _scope: Some(scope),
        });
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("SQLite source dependencies changed or unavailable")))
}

/// `file:<path>?immutable=1`, with the URI's reserved characters percent-encoded.
fn immutable_uri(path: &Path) -> String {
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%').add(b'?').add(b'#').add(b' ');
    #[cfg(unix)]
    let encoded = {
        use std::os::unix::ffi::OsStrExt;
        percent_encoding::percent_encode(path.as_os_str().as_bytes(), RESERVED).to_string()
    };
    #[cfg(not(unix))]
    let encoded = {
        let text = format!("/{}", path.to_string_lossy().replace('\\', "/"));
        percent_encoding::utf8_percent_encode(&text, RESERVED).to_string()
    };
    format!("file:{encoded}?immutable=1")
}

/// Copy a snapshot's immutable TEXT/BLOB cell into an operation-owned arena using bounded
/// scratch. Table and column are adapter-owned identifiers, never user selectors. Blob access
/// may be unavailable for virtual tables; the fallback retains one encoded cell in SQLite.
pub(crate) fn append_cell(
    conn: &Connection,
    table: &str,
    column: &str,
    rowid: i64,
    arena: &super::bytes::ByteSource,
) -> Result<(u64, u64)> {
    if let Ok(mut blob) = conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, true) {
        return arena.append(|writer| {
            let mut buffer = [0u8; super::IO_CHUNK];
            loop {
                let read = std::io::Read::read(&mut blob, &mut buffer)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buffer[..read])?;
            }
            Ok(())
        });
    }
    conn.query_row(
        &format!("SELECT {column} FROM {table} WHERE rowid = ?1"),
        [rowid],
        |row| {
            Ok(arena.append(|writer| {
                match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                        writer.write_all(bytes)?
                    }
                    _ => anyhow::bail!("SQLite source cell is not TEXT or BLOB"),
                }
                Ok(())
            }))
        },
    )?
}

/// A store's JSON cells as views: rows already are the byte source. Each cell a read selects is
/// copied once into the source's operation-owned arena (an incremental read by rowid where the
/// table allows one, else the row's own bytes), validated once, and handed to the format's one
/// decoder as a [`JsonView`](crate::json::JsonView). The demand decides what the decoder
/// resolves, never which code runs.
pub(crate) struct Cells {
    source: crate::json::JsonSource,
    arena: std::sync::Arc<super::bytes::ByteSource>,
    incremental: bool,
}

/// One cell's bytes in a [`Cells`] arena.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cell(u64, u64);

impl Cells {
    /// Cells of `table` in the database at `path`, guarded by the reader's dependencies.
    pub(crate) fn new(conn: &Connection, path: &Path, reads: &crate::source::ReadSet, table: &str) -> Result<Self> {
        let source = crate::json::JsonSource::capture_with_reads(path, reads)?;
        Ok(Self {
            arena: source.decoded_backing()?,
            source,
            incremental: incremental_utf8_table(conn, table),
        })
    }

    /// Cells of `table` for a single session in a shared multi-writer SQLite store, guarded by
    /// that session's revision and schema guard ([`read_session`]).
    pub(crate) fn for_session(
        conn: &Connection,
        path: &Path,
        reads: &crate::source::ReadSet,
        guard: &std::sync::Arc<crate::json::SqliteSessionGuard>,
        table: &str,
    ) -> Result<Self> {
        let source = crate::json::JsonSource::capture_with_sqlite_guard(path, reads, std::sync::Arc::clone(guard))?;
        Ok(Self {
            arena: source.decoded_backing()?,
            source,
            incremental: incremental_utf8_table(conn, table),
        })
    }

    /// What a query selects for `column` so [`Cells::cell`] can read it from columns `i` and
    /// `i + 1`: the rowid and the cell's type when reads are incremental, else the cell itself.
    pub(crate) fn select(&self, column: &str) -> String {
        if self.incremental {
            format!("rowid, typeof({column})")
        } else {
            format!("{column}, NULL")
        }
    }

    /// The cell `select(column)` put at column `i` of `row`. None for a cell that is not TEXT,
    /// is empty or is not UTF-8 (UTF-8 is checked when it is read).
    pub(crate) fn cell(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
    ) -> Result<Option<Cell>> {
        use rusqlite::types::ValueRef;
        let (offset, len) = if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("text") {
                return Ok(None);
            }
            append_cell(conn, table, column, row.get(i)?, &self.arena)?
        } else {
            let ValueRef::Text(bytes) = row.get_ref(i)? else {
                return Ok(None);
            };
            self.arena.append(|writer| Ok(writer.write_all(bytes)?))?
        };
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// The BLOB `select(column)` put at column `i` of `row`, passed through `decode` (a
    /// decompressor, say) into the arena: the decoded bytes are the cell. None for a cell that is
    /// not a BLOB or is empty; a `decode` failure is an error.
    pub(crate) fn blob(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
        decode: impl FnOnce(&mut dyn std::io::BufRead, &mut dyn std::io::Write) -> Result<()>,
    ) -> Result<Option<Cell>> {
        use rusqlite::types::ValueRef;
        let (offset, len) = if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("blob") {
                return Ok(None);
            }
            let blob = conn.blob_open(rusqlite::DatabaseName::Main, table, column, row.get(i)?, true)?;
            if blob.is_empty() {
                return Ok(None);
            }
            let mut blob = std::io::BufReader::with_capacity(super::IO_CHUNK, blob);
            self.arena.append(|writer| decode(&mut blob, writer))?
        } else {
            let ValueRef::Blob(mut bytes) = row.get_ref(i)? else {
                return Ok(None);
            };
            if bytes.is_empty() {
                return Ok(None);
            }
            self.arena.append(|writer| decode(&mut bytes, writer))?
        };
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// A TEXT cell holding a plain string (not JSON), copied into the arena as one JSON string so
    /// the decoder views it like any other string. None for a cell that is not TEXT, is empty or
    /// is not UTF-8.
    pub(crate) fn text(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
    ) -> Result<Option<Cell>> {
        let mut valid = false;
        let (offset, len) = self.arena.append(|writer| {
            super::text::generate_string(writer, |builder| {
                valid = self.chunks(conn, table, column, row, i, |chunk| builder.push(chunk))?;
                Ok(())
            })
        })?;
        Ok((valid && len > 2).then_some(Cell(offset, len)))
    }

    /// Visit the UTF-8 chunks of the TEXT cell `select(column)` put at column `i` of `row`
    /// without copying it anywhere. False for a cell that is not TEXT or is not UTF-8.
    pub(crate) fn chunks(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        row: &rusqlite::Row<'_>,
        i: usize,
        mut visit: impl FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        use rusqlite::types::ValueRef;
        if self.incremental {
            if row.get_ref(i + 1)?.as_str_or_null()? != Some("text") {
                return Ok(false);
            }
            return with_cell_reader(conn, table, column, row.get(i)?, |reader, _| {
                for_each_utf8(reader, &mut visit)
            });
        }
        let ValueRef::Text(bytes) = row.get_ref(i)? else {
            return Ok(false);
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return Ok(false);
        };
        visit(text)?;
        Ok(true)
    }

    /// The captured store and the arena its cells are copied into, where a decoder composes text
    /// that is checked against the store like the cells themselves.
    pub(crate) fn out(&self) -> (&crate::json::JsonSource, &std::sync::Arc<super::bytes::ByteSource>) {
        (&self.source, &self.arena)
    }

    /// The cell at `rowid` of `table.column`, TEXT or BLOB bytes as stored, for a store that
    /// locates its rows itself (by key). None when empty.
    pub(crate) fn row(&self, conn: &Connection, table: &str, column: &str, rowid: i64) -> Result<Option<Cell>> {
        let (offset, len) = append_cell(conn, table, column, rowid, &self.arena)?;
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// Bytes a format derives as a cell of their own: a BLOB in the store's own encoding rule, or
    /// a JSON string holding a JSON document. None when empty.
    pub(crate) fn append(&self, write: impl FnOnce(&mut dyn std::io::Write) -> Result<()>) -> Result<Option<Cell>> {
        let (offset, len) = self.arena.append(write)?;
        Ok((len != 0).then_some(Cell(offset, len)))
    }

    /// Recheck the store and the reader's dependencies (once, when a read ends).
    pub(crate) fn check(&self) -> Result<()> {
        self.source.check()
    }

    /// Decode `cell` with `f`. None when the cell is not exactly one JSON value; every other
    /// error is a source failure.
    pub(crate) fn read<T>(
        &self,
        cell: Cell,
        f: impl for<'a> FnOnce(crate::json::JsonView<'a>) -> Result<T>,
    ) -> Result<Option<T>> {
        match self.source.read(
            ReadInput::Backing(self.arena.clone(), cell.0, cell.1),
            Validation::Value,
            f,
        ) {
            Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => Ok(None),
            result => result.map(Some),
        }
    }
}

/// Incremental TEXT bytes use the database's encoding. Ordinary UTF-8 rowid tables support
/// bounded reads; views, virtual/WITHOUT ROWID tables and UTF-16 stores retain their typed reader.
pub(crate) fn incremental_utf8_table(conn: &Connection, table: &str) -> bool {
    let utf8 = conn
        .query_row("PRAGMA encoding", [], |row| row.get::<_, String>(0))
        .ok()
        .as_deref()
        == Some("UTF-8");
    utf8 && conn
        .query_row(&format!("PRAGMA main.table_list('{table}')"), [], |row| {
            Ok(row.get::<_, String>(2)? == "table" && !row.get::<_, bool>(4)?)
        })
        .unwrap_or(false)
}

/// Borrow a read-only cell handle while the caller's active statement/snapshot is still alive.
/// Adapter-owned table/column identifiers only. Unlike selecting a payload column, opening this
/// handle does not ask sqlite3_step to materialize the entire TEXT/BLOB value.
pub(crate) fn with_cell_reader<T>(
    conn: &Connection,
    table: &str,
    column: &str,
    rowid: i64,
    read: impl FnOnce(&mut dyn std::io::Read, usize) -> Result<T>,
) -> Result<T> {
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, true)?;
    let len = blob.len();
    read(&mut blob, len)
}

/// Strict streaming UTF-8 acceptance, including scalars crossing native read windows. False is
/// a typed-string rejection, not a read failure; callers discard any unpublished prefix output.
pub(crate) fn for_each_utf8(reader: &mut dyn std::io::Read, visit: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    crate::utf8::read(reader, crate::utf8::Verdict::Reject, visit)
}

/// A SELECT list built against the schema this database has, every entry named so a reader asks
/// [`Select::at`] for its index instead of counting positions (a [`Cells`] cell takes two). Every
/// SQLite store grows its schema by `ALTER TABLE`, so a probed column the table lacks reads as
/// `NULL` and one query serves every version.
pub(crate) struct Select {
    columns: HashSet<String>,
    list: Vec<String>,
    names: Vec<String>,
}

impl Select {
    /// A list over `table`, whose columns are probed once.
    pub(crate) fn of(conn: &Connection, table: &str) -> Self {
        Self {
            columns: columns(conn, table),
            list: Vec::new(),
            names: Vec::new(),
        }
    }

    /// Whether the table has `column`.
    pub(crate) fn has(&self, column: &str) -> bool {
        self.columns.contains(column)
    }

    /// Each of `exprs` as written: columns every schema has, or expressions, named by their text.
    pub(crate) fn always(mut self, exprs: &[&str]) -> Self {
        for expr in exprs {
            self.list.push((*expr).to_string());
            self.names.push((*expr).to_string());
        }
        self
    }

    /// Each of `columns`, or `NULL` where the table lacks it.
    pub(crate) fn probed(mut self, columns: &[&str]) -> Self {
        for column in columns {
            self.list
                .push(if self.has(column) { column } else { "NULL" }.to_string());
            self.names.push((*column).to_string());
        }
        self
    }

    /// Each of `columns` the table has; an absent one is not selected and [`Self::at`] answers
    /// `None` for it.
    pub(crate) fn present(mut self, columns: &[&str]) -> Self {
        for column in columns.iter().filter(|c| self.columns.contains(**c)) {
            self.list.push((*column).to_string());
            self.names.push((*column).to_string());
        }
        self
    }

    /// The pair `cells` reads `column` from ([`Cells::select`]; `NULL` where the table lacks it),
    /// named `column` at the index [`Cells::cell`] takes.
    pub(crate) fn cell(self, cells: &Cells, column: &str) -> Self {
        let names = [column.to_string(), format!("{column}#type")];
        self.cell_named(cells, column, names)
    }

    /// [`Self::cell`] with both positions named by the caller (a reader that reads the pair's
    /// rowid or type itself).
    pub(crate) fn cell_named(mut self, cells: &Cells, column: &str, names: [String; 2]) -> Self {
        self.list
            .push(cells.select(if self.has(column) { column } else { "NULL" }));
        self.names.extend(names);
        self
    }

    /// Where `name` reads from; `None` for a name the list does not hold.
    pub(crate) fn at(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Where `name`, which this list always selects, reads from.
    pub(crate) fn index(&self, name: &str) -> Result<usize> {
        self.at(name)
            .with_context(|| format!("{name} is not in the SELECT list"))
    }

    /// The comma-joined list, for `SELECT {list} FROM …`.
    pub(crate) fn list(&self) -> String {
        self.list.join(", ")
    }
}

/// The columns present on `table` (`PRAGMA table_info`). Every SQLite-backed harness grows its
/// schema by `ALTER TABLE` across versions, so an adapter probes before it selects; an unknown
/// table yields the empty set.
pub(crate) fn columns(conn: &Connection, table: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            set.extend(rows.flatten());
        }
    }
    set
}

/// Whether the database has a table named `name`.
pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Every file in a store directory: name, inode, size, mtime and bytes (`-shm` included).
#[cfg(test)]
pub(crate) fn store_state(dir: &Path) -> Vec<(std::ffi::OsString, u64, u64, std::time::SystemTime, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            #[cfg(unix)]
            let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
            #[cfg(not(unix))]
            let inode = 0;
            let bytes = if metadata.is_file() {
                std::fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (
                entry.file_name(),
                inode,
                metadata.len(),
                metadata.modified().unwrap(),
                bytes,
            )
        })
        .collect();
    files.sort();
    files
}

/// A running harness: WAL mode, `sql`'s frames not checkpointed, its connection kept open.
#[cfg(test)]
pub(crate) fn live_store(database: &Path, sql: &str) -> Connection {
    let writer = Connection::open(database).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    writer.execute_batch(sql).unwrap();
    writer
}

#[cfg(test)]
mod tests {
    use super::*;

    static SNAPSHOT_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn rollback_reader_cannot_create_wal_sidecars_after_a_mode_change() {
        let (directory, database) = fixture();
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1);")
            .unwrap();
        let reader = open_ro(&database).unwrap();
        let result = reader.read(|connection, _| {
            // The harness switches mode after the reader's before-operation guard. Closing
            // its connection checkpoints the WAL and removes its coordination sidecars.
            let writer = Connection::open(&database)?;
            writer.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO t VALUES(2);")?;
            drop(writer);
            let before = store_state(&directory);
            let _: rusqlite::Result<i64> = connection.query_row("SELECT count(*) FROM t", [], |row| row.get(0));
            assert!(
                store_state(&directory) == before,
                "read-only connection wrote harness sidecars"
            );
            Ok(())
        });
        assert!(result.is_err(), "changed source must also fail freshness");
        drop(reader);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn fixture() -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!("cv-cold-wal-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("sessions.db");
        (directory, database)
    }

    #[test]
    fn dropping_replaced_cold_snapshot_preserves_different_directory() {
        let (directory, database) = fixture();
        std::fs::write(&database, "owned snapshot").unwrap();
        let snapshot = ColdSnapshot {
            reads: crate::source::ReadSet::default(),
            birth: [0; 32],
            created_at: std::sync::Mutex::new(std::time::Instant::now()),
            directory_identity: snapshots::snapshot_identity(&directory).unwrap(),
            directory: directory.clone(),
            database,
            _owner: None,
        };
        let saved = directory.with_extension("saved");
        std::fs::rename(&directory, &saved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let different = directory.join("different-owner");
        std::fs::write(&different, "retained").unwrap();
        drop(snapshot);
        assert_eq!(std::fs::read_to_string(&different).unwrap(), "retained");
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(saved).unwrap();
    }

    #[test]
    fn incremental_text_reads_keep_utf8_acceptance_and_native_table_constraints() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE cells(value TEXT); CREATE TABLE other(id TEXT PRIMARY KEY, value TEXT) WITHOUT ROWID; CREATE VIEW projected AS SELECT * FROM cells;")?;
        assert!(incremental_utf8_table(&conn, "cells"));
        assert!(!incremental_utf8_table(&conn, "other"));
        assert!(!incremental_utf8_table(&conn, "projected"));
        let body = format!("{}🌙{}\0end", "x".repeat(65535), "界".repeat(30000));
        conn.execute("INSERT INTO cells(value) VALUES(?1)", [&body])?;
        let decoded = with_cell_reader(&conn, "cells", "value", 1, |reader, len| {
            assert_eq!(len, body.len());
            let mut decoded = String::new();
            assert!(for_each_utf8(reader, |part| {
                decoded.push_str(part);
                Ok(())
            })?);
            Ok(decoded)
        })?;
        assert_eq!(decoded, body);
        for bytes in [b"invalid\xff".as_slice(), b"truncated\xf0\x9f".as_slice()] {
            conn.execute("INSERT INTO cells(value) VALUES(CAST(?1 AS TEXT))", [bytes])?;
            assert!(!with_cell_reader(
                &conn,
                "cells",
                "value",
                conn.last_insert_rowid(),
                |reader, _| for_each_utf8(reader, |_| Ok(()))
            )?);
        }
        let utf16 = Connection::open_in_memory()?;
        utf16.execute_batch("PRAGMA encoding='UTF-16le'; CREATE TABLE cells(value TEXT)")?;
        assert!(!incremental_utf8_table(&utf16, "cells"));
        Ok(())
    }

    fn expire_cached_snapshot(path: &Path) {
        let path = std::path::absolute(path).unwrap();
        let scope = reader_scope();
        let cache = scope.cache.state.lock().unwrap();
        if let Some(snapshot) = cache.retained.get(&path) {
            *snapshot.created_at.lock().unwrap() =
                std::time::Instant::now() - COLD_SNAPSHOT_TTL - std::time::Duration::from_secs(1);
        }
    }

    /// Cold WAL snapshots (used under `--frozen`, missing `-shm`, or `SQLITE_BUSY`) are private,
    /// reused within an operation, removed after it, and write nothing in the store directory
    /// (`-shm` bytes included).
    #[test]
    fn live_wal_snapshot_is_private_reused_and_writes_nothing_in_the_store() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let _frozen = crate::env::Env::current().as_ref().clone().with_frozen().enter();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let before = super::store_state(&directory);
        assert!(std::fs::metadata(sidecar(&database, "-wal")).unwrap().len() > 0);
        let first = open_ro(&database).unwrap();
        assert_eq!(
            first
                .read(|r, _| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            7
        );
        let second = open_ro(&database).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            first._snapshot.as_ref().unwrap(),
            second._snapshot.as_ref().unwrap()
        ));
        assert!(second.execute("INSERT INTO t VALUES(8)", []).is_err());
        let private_directory = first._snapshot.as_ref().unwrap().directory.clone();
        let scope = read_scope();
        drop(first);
        drop(second);
        for _ in 0..32 {
            let reader = open_ro(&database).unwrap();
            assert_eq!(reader._snapshot.as_ref().unwrap().directory, private_directory);
            reader
                .read(|r, _| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap();
        }
        assert!(private_directory.exists());
        drop(scope);
        assert!(!private_directory.exists());
        assert_eq!(super::store_state(&directory), before);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retained_backing_keeps_lease_after_its_connection_and_cache_finish() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x);");
        let reader = open_cold_ro(&database).unwrap();
        let backing = std::sync::Arc::clone(reader._snapshot.as_ref().unwrap());
        let private = backing.directory.clone();
        drop(reader); // Its cache/scope are gone, but the backing retains its own owner Arc.
        snapshots::Namespace::current().reap();
        assert!(private.exists());
        drop(backing);
        assert!(!private.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn interleaved_databases_reuse_active_snapshots_and_cleanup_operation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x);");
        let second_database = directory.join("other.db");
        let _second_writer = live_store(&second_database, "CREATE TABLE t(x);");
        let scope = read_scope();
        // Model another libtest thread retaining a scope across this test's cleanup.
        let unrelated = std::thread::spawn(read_scope).join().unwrap();
        let first = open_cold_ro(&database).unwrap();
        let first_backing = first._snapshot.as_ref().unwrap().directory.clone();
        let second = open_cold_ro(&second_database).unwrap();
        let second_backing = second._snapshot.as_ref().unwrap().directory.clone();
        let again = open_ro_with_copy_check(&database, |_| panic!("active A snapshot duplicated")).unwrap();
        assert_eq!(again._snapshot.as_ref().unwrap().directory, first_backing);
        drop(first);
        drop(again);
        assert!(first_backing.exists());
        drop(second);
        assert!(second_backing.exists());
        drop(scope);
        assert!(!first_backing.exists());
        assert!(!second_backing.exists());
        drop(unrelated);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A copied store lives for the operation however many other databases it opens: a WAL
    /// store is copied once per state, never again because another store was copied after it.
    #[test]
    fn a_copied_store_outlives_every_other_open_in_its_operation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let others: Vec<_> = (0..4)
            .map(|i| {
                let other = directory.join(format!("other-{i}.db"));
                (live_store(&other, "CREATE TABLE t(x);"), other)
            })
            .collect();
        let scope = read_scope();
        let first = open_cold_ro(&database)
            .unwrap()
            ._snapshot
            .as_ref()
            .unwrap()
            .directory
            .clone();
        for (_, other) in &others {
            drop(open_cold_ro(other).unwrap());
            let again = open_ro_with_copy_check(&database, |_| panic!("an unchanged store was copied again")).unwrap();
            assert_eq!(again._snapshot.as_ref().unwrap().directory, first);
        }
        drop(scope);
        assert!(!first.exists());
        drop(others);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cached_snapshot_rebuilds_after_source_change() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let scope = read_scope();
        let first = open_cold_ro(&database).unwrap();
        let old_backing = first._snapshot.as_ref().unwrap().directory.clone();
        let old_reads = first.reads().clone();
        drop(first);
        writer.execute("UPDATE t SET x=8", []).unwrap();
        expire_cached_snapshot(&database);
        let mut copied = false;
        let second = open_ro_with_copy_check(&database, |_| {
            copied = true;
            Ok(())
        })
        .unwrap();
        assert!(copied);
        assert!(!old_backing.exists());
        assert!(!old_reads.fresh(&mut crate::source::Probe::default()));
        assert_eq!(
            second
                .read(|r, _| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            8
        );
        drop(second);
        drop(scope);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn live_wal_uses_mvcc_deferred_snapshot_without_copying_db() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(
            &database,
            "CREATE TABLE t(s TEXT, x INT); INSERT INTO t VALUES('a', 7);",
        );
        let non_shm = |dir: &Path| -> Vec<_> {
            super::store_state(dir)
                .into_iter()
                .filter(|entry| !entry.0.to_string_lossy().ends_with("-shm"))
                .collect()
        };
        let before = non_shm(&directory);
        let reader = open_ro(&database).unwrap();
        assert!(reader._snapshot.is_none(), "live WAL should use MVCC, not ColdSnapshot");
        assert!(!reader.is_autocommit(), "BEGIN DEFERRED must pin the WAL read mark");
        assert_eq!(non_shm(&directory), before, ".db and -wal must be untouched");

        // Concurrent writer appends to another session AND passive-checkpoints while reader holds MVCC.
        writer
            .execute_batch("INSERT INTO t VALUES('b', 99); PRAGMA wal_checkpoint(PASSIVE);")
            .unwrap();
        let seen: Vec<(String, i64)> = reader
            .read_snapshot(|conn, _| {
                let mut stmt = conn.prepare("SELECT s, x FROM t ORDER BY s")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(seen, vec![("a".to_string(), 7)]);
        drop(reader);

        // Next reader sees the newly committed row without copying either.
        let next = open_ro(&database).unwrap();
        assert!(next._snapshot.is_none());
        let count: i64 = next
            .read_snapshot(|conn, _| Ok(conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 2);
        drop(next);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn busy_or_frozen_falls_back_to_cold_snapshot_with_bounded_ttl_reuse() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        // 1. Genuine SQLITE_BUSY via EXCLUSIVE lock holder: falls back to ColdSnapshot and reuses
        //    the single copy across 10 back-to-back opens within TTL.
        let locker = Connection::open(&database).unwrap();
        locker
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; \
                 CREATE TABLE t(x); INSERT INTO t VALUES(42); COMMIT; BEGIN EXCLUSIVE; INSERT INTO t VALUES(43);",
            )
            .unwrap();
        let scope = read_scope();
        let first = open_ro(&database).unwrap();
        let first_dir = first
            ._snapshot
            .as_ref()
            .expect("SQLITE_BUSY must snapshot")
            .directory
            .clone();
        assert_eq!(
            first
                .read_snapshot(|c, _| Ok(c.query_row("SELECT x FROM t", [], |r| r.get::<_, i64>(0))?))
                .unwrap(),
            42
        );
        drop(first);
        for _ in 0..10 {
            let again = open_ro(&database).unwrap();
            assert_eq!(
                again._snapshot.as_ref().unwrap().directory,
                first_dir,
                "10 back-to-back calls under SQLITE_BUSY must reuse the TTL ColdSnapshot"
            );
        }
        drop(scope);
        drop(locker);
        std::fs::remove_dir_all(directory).unwrap();

        // 2. `--frozen` with moving `-wal`: 10 back-to-back calls reuse one snapshot within TTL.
        let (directory, database) = fixture();
        let _frozen = crate::env::Env::current().as_ref().clone().with_frozen().enter();
        let writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(0);");
        let scope = read_scope();
        let initial = open_ro(&database).unwrap();
        let snap_dir = initial._snapshot.as_ref().unwrap().directory.clone();
        drop(initial);
        for i in 1..=10 {
            writer.execute("INSERT INTO t VALUES(?1)", [i]).unwrap();
            let reader = open_ro(&database).unwrap();
            assert_eq!(
                reader._snapshot.as_ref().unwrap().directory,
                snap_dir,
                "moving -wal within TTL must not copy the DB 10 times"
            );
        }
        expire_cached_snapshot(&database);
        let refreshed = open_ro(&database).unwrap();
        assert_ne!(refreshed._snapshot.as_ref().unwrap().directory, snap_dir);
        assert_eq!(
            refreshed
                .read_snapshot(|c, _| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?))
                .unwrap(),
            11
        );
        drop(refreshed);
        drop(scope);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// An isolated home whose catalog directory is `<root>/catalog`.
    fn catalog_home() -> (std::path::PathBuf, crate::env::Scope) {
        let root = std::env::temp_dir().join(format!("cv-derived-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("home")).unwrap();
        let scope = crate::env::Env::from_vars([
            ("HOME", root.join("home")),
            ("CLUSTERVISION_HOME", root.join("catalog")),
        ])
        .enter();
        (root, scope)
    }

    /// A cold WAL copy is staged under cv's catalog directory, not the temporary directory,
    /// and is gone once its reader and scope are.
    #[test]
    fn cold_copy_lands_beside_the_catalog_and_is_removed_after_the_pass() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (root, _env) = catalog_home();
        let (directory, database) = fixture();
        let _writer = live_store(&database, "CREATE TABLE t(x); INSERT INTO t VALUES(7);");
        let scope = read_scope();
        let mut staged = None;
        let reader = open_ro_with_copy_check(&database, |copied| {
            staged = Some(copied.to_path_buf());
            Ok(())
        })
        .unwrap();
        let staged = staged.unwrap();
        assert!(
            staged.starts_with(root.join("catalog").join("cv-sqlite-owners-v1")),
            "{}",
            staged.display()
        );
        assert!(staged.exists());
        drop(reader);
        drop(scope);
        assert!(!staged.exists());
        assert!(!staged.parent().unwrap().exists());
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_race_rejects_source_change_and_removes_private_backing() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = live_store(&database, "CREATE TABLE t(x);");
        let mut private = None;
        let result = open_ro_with_copy_check(&database, |copied| {
            private = copied.parent().map(Path::to_path_buf);
            writer.execute("INSERT INTO t VALUES(9)", [])?;
            Ok(())
        });
        assert!(result.err().unwrap().to_string().contains("dependencies changed"));
        assert!(!private.unwrap().exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A WAL-mode store without frames (checkpointed while its harness runs, or closed) is read
    /// in place with `immutable=1`: no copy, no `-wal`/`-shm` created, nothing changed.
    #[test]
    fn frameless_wal_reads_the_original_immutably_and_writes_nothing() {
        let (directory, database) = fixture();
        let writer = live_store(
            &database,
            "CREATE TABLE t(x); INSERT INTO t VALUES(7); PRAGMA wal_checkpoint(TRUNCATE);",
        );
        assert!(sidecar(&database, "-shm").exists());
        let read = || {
            let before = super::store_state(&directory);
            let reader = open_ro_with_copy_check(&database, |_| panic!("a frameless WAL was copied")).unwrap();
            assert!(reader._snapshot.is_none());
            assert_eq!(
                reader
                    .read(|r, _| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                    .unwrap(),
                7
            );
            assert!(reader.execute("INSERT INTO t VALUES(8)", []).is_err());
            drop(reader);
            assert_eq!(super::store_state(&directory), before);
        };
        read();
        drop(writer);
        assert!(!sidecar(&database, "-wal").exists() && !sidecar(&database, "-shm").exists());
        read();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_shm_snapshot_preserves_wal_only_rows_and_rejects_mutation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = Connection::open(&database).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x); PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO t VALUES(7);").unwrap();
        // Simulate a retained/crashed WAL without its coordination sidecar. The writer remains
        // alive solely to demonstrate why immutable=1 would incorrectly omit this committed row.
        std::fs::remove_file(sidecar(&database, "-shm")).unwrap();
        let reader = open_ro(&database).unwrap();
        assert_eq!(
            reader
                .read(|r, _| Ok(r.query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            1
        );
        assert!(!sidecar(&database, "-shm").exists());
        let immutable = Connection::open_with_flags(
            format!("file:{}?immutable=1", database.display()),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert_eq!(
            immutable
                .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(immutable);
        let failure = reader
            .read::<()>(|_, _| {
                writer.execute("INSERT INTO t VALUES(8)", [])?;
                anyhow::bail!("body failure")
            })
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"));
        assert!(crate::json::JsonSource::capture_with_reads(&database, reader.reads()).is_err());
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn append_text_and_blob_cells_share_one_backing() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE payload(data); INSERT INTO payload VALUES('text'); INSERT INTO payload VALUES(X'626C6F62'); INSERT INTO payload VALUES(NULL);").unwrap();
        let arena = super::super::bytes::ByteSource::arena().unwrap();
        assert_eq!(append_cell(&connection, "payload", "data", 1, &arena).unwrap(), (0, 4));
        assert_eq!(append_cell(&connection, "payload", "data", 2, &arena).unwrap(), (4, 4));
        assert!(append_cell(&connection, "payload", "data", 3, &arena).is_err());
        let mut output = Vec::new();
        arena.copy_to(&mut output).unwrap();
        assert_eq!(output, b"textblob");
    }

    #[test]
    fn probes_and_read_only_open() {
        let dir = std::env::temp_dir().join(format!("cv-store-sqlite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE sessions (id TEXT, title TEXT); INSERT INTO sessions VALUES ('a', 't');")
                .unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert!(table_exists(&ro, "sessions"));
        assert!(!table_exists(&ro, "messages"));
        let cols = columns(&ro, "sessions");
        assert!(cols.contains("id") && cols.contains("title") && cols.len() == 2);
        assert!(columns(&ro, "nope").is_empty());
        // Read-only means a write is refused, not performed.
        assert!(ro.execute("INSERT INTO sessions VALUES ('b', 'u')", []).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
