//! Authoritative local state spine (`$CLUSTERVISION_HOME/state.db`).
//!
//! `state.db` separates `cv`-owned durable facts from rebuildable derived caches (`catalog.db` and
//! search indexes). Every mutation executes under `WAL` + `synchronous=FULL` with explicit schema
//! migrations ([`schema::STATE_SCHEMA_VERSION`]) that are strictly independent of
//! [`crate::source::VERSION`].

pub mod hub;
#[cfg(unix)]
pub mod observations;
pub mod recorder;
pub mod schema;
pub mod tables;

#[cfg(test)]
mod tests;

use crate::harness::store::sqlite::SnapshotDirectory;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use recorder::{default_host_label, AppendRequest, FaultMode, RecorderServer};
pub use schema::{current_schema_version, STATE_SCHEMA_VERSION};
pub use tables::{
    CaptureInboxRow, CustodyAck, LaneEndpointBinding, LaneRun, LineageEdge, PackManifest, Producer, ReceiptRecord,
    WriterEpoch,
};

/// Default producer identifier used when a local caller appends without explicit enrollment.
pub const DEFAULT_LOCAL_PRODUCER: &str = "local";

/// Globally unique durable event coordinate `(producer_id, epoch, seq)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub producer_id: String,
    pub epoch: String,
    pub seq: u64,
}

impl EventId {
    pub fn new(producer_id: impl Into<String>, epoch: impl Into<String>, seq: u64) -> Self {
        Self {
            producer_id: producer_id.into(),
            epoch: epoch.into(),
            seq,
        }
    }

    pub fn as_tuple(&self) -> (&str, &str, u64) {
        (&self.producer_id, &self.epoch, self.seq)
    }
}

impl From<(String, String, u64)> for EventId {
    fn from((producer_id, epoch, seq): (String, String, u64)) -> Self {
        Self {
            producer_id,
            epoch,
            seq,
        }
    }
}

/// Durable commit receipt returned by [`StateStore::append`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableReceipt {
    pub event_id: EventId,
    pub local_commit_pos: i64,
    /// `true` when an existing `operation_bindings` row satisfied a retry idempotently.
    pub replayed: bool,
}

/// Typed errors surfaced by the authoritative `state.db` spine.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("refusing to mutate authoritative state.db under --frozen")]
    Frozen,

    #[error(
        "conflicting operation binding for op_id `{op_id}`: committed digest `{existing_digest}` != incoming `{incoming_digest}`"
    )]
    OperationConflict {
        op_id: String,
        existing_digest: String,
        incoming_digest: String,
    },

    #[error("state.db schema v{found} was written by a newer cv (this binary supports up to v{supported})")]
    NewerSchema { found: i64, supported: i64 },

    #[error("invalid state argument: {0}")]
    InvalidArgument(String),

    #[error("pack range publication rejected: {0}")]
    PackRangeMismatch(String),

    #[error("could not resolve CLUSTERVISION_HOME for state.db")]
    NoHome,

    #[error("state.db sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("state.db I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("state.db JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Bundled SQLite version string (`"3.51.3"` or newer).
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}

/// Bundled SQLite integer version (`3_051_003` or newer).
pub fn sqlite_version_number() -> i32 {
    rusqlite::version_number()
}

/// Read-only state connection and ownership of any private snapshot backing it.
///
/// Since 0.20, [`StateStore::open_readonly`] returns this owner rather than a bare
/// `rusqlite::Connection`. Borrowed connection operations remain available through `Deref`;
/// explicit Rust return-type annotations must use `StateReadConnection`. CLI and JSON are unchanged.
/// Mutable access to the connection owner is intentionally withheld: replacing it would detach
/// the live connection from the snapshot lifetime.
#[derive(Debug)]
pub struct StateReadConnection {
    // Rust drops fields in declaration order. Close SQLite before removing its backing files,
    // including on Windows, where SQLite does not open database handles with FILE_SHARE_DELETE.
    connection: Connection,
    _snapshot: Option<SnapshotDirectory>,
}

impl std::ops::Deref for StateReadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

/// Lightweight handle to an authoritative `state.db` location.
#[derive(Debug, Clone)]
pub struct StateStore {
    db_path: PathBuf,
    default_producer: String,
}

impl StateStore {
    /// Handle to `$CLUSTERVISION_HOME/state.db`.
    pub fn default_store() -> Result<Self, StateError> {
        let home = crate::env::Env::current().cv_home().ok_or(StateError::NoHome)?;
        Ok(Self::at(home.join("state.db")))
    }

    /// Handle to an explicit `state.db` file path.
    pub fn at(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            default_producer: DEFAULT_LOCAL_PRODUCER.to_string(),
        }
    }

    /// Handle to `<dir>/state.db`.
    pub fn at_dir(dir: impl AsRef<Path>) -> Self {
        Self::at(dir.as_ref().join("state.db"))
    }

    /// Override the producer identity used by [`Self::append`] when run serverless.
    pub fn with_producer(mut self, producer_id: impl Into<String>) -> Self {
        self.default_producer = producer_id.into();
        self
    }

    /// Path to the `state.db` SQLite file.
    pub fn path(&self) -> &Path {
        &self.db_path
    }

    /// Default producer id bound to this handle.
    pub fn default_producer(&self) -> &str {
        &self.default_producer
    }

    /// Advisory OS ownership lockfile coordinating [`RecorderServer`] and serverless writers.
    pub fn owner_lock_path(&self) -> PathBuf {
        sibling_with_suffix(&self.db_path, "owner.lock")
    }

    /// Restrictive Unix domain socket path for [`RecorderServer`].
    ///
    /// Falls back to a deterministic private `0700` per-UID runtime directory when `<dir>/state.sock`
    /// would approach macOS's 104-byte `SUN_LEN` limit.
    #[cfg(unix)]
    pub fn socket_path(&self) -> PathBuf {
        self.socket_path_with_temp_dir(&std::env::temp_dir())
    }

    #[cfg(unix)]
    pub(crate) fn socket_path_with_temp_dir(&self, temp_dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let direct = sibling_with_suffix(&self.db_path, "sock");
        if direct.as_os_str().as_bytes().len() <= 96 {
            return direct;
        }
        let canonical_key = normalize_path_key(&self.db_path);
        let hash = crate::digest::sha256_hex(canonical_key.as_bytes());
        let uid = unsafe { libc::geteuid() };
        let sub_dir = format!("cv-usock-{uid}");
        let file_name = format!("s-{}.sock", &hash[..24]);
        let candidate = temp_dir.join(&sub_dir).join(&file_name);
        if candidate.as_os_str().as_bytes().len() <= 96 {
            return candidate;
        }
        PathBuf::from("/tmp").join(sub_dir).join(file_name)
    }

    /// Open a migrated writable SQLite connection configured with `WAL` + `synchronous=FULL`.
    pub fn open_writable(&self) -> Result<Connection, StateError> {
        if crate::env::frozen() {
            return Err(StateError::Frozen);
        }
        if let Some(parent) = self.db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&self.db_path)?;
        set_restrictive_file_mode(&self.db_path);
        conn.busy_timeout(Duration::from_millis(5_000))?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap_or_default();
        if !mode.eq_ignore_ascii_case("wal") {
            for attempt in 0..20 {
                match conn.execute_batch("PRAGMA journal_mode = WAL;") {
                    Ok(()) => break,
                    Err(rusqlite::Error::SqliteFailure(err, _))
                        if matches!(
                            err.code,
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        ) && attempt < 19 =>
                    {
                        std::thread::sleep(Duration::from_millis(5 * (attempt + 1)));
                    }
                    Err(e) => return Err(StateError::Sqlite(e)),
                }
            }
        }
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )?;
        schema::ensure_migrated(&mut conn)?;
        Ok(conn)
    }

    /// Run an opportunistic SQLite WAL checkpoint (`PASSIVE` when `truncate == false`,
    /// `TRUNCATE` when `truncate == true`). No-ops cleanly under `--frozen` or when `state.db`
    /// has not been created yet.
    pub fn checkpoint_wal(&self, truncate: bool) -> Result<Option<(i64, i64)>, StateError> {
        if crate::env::frozen() || !self.db_path.is_file() {
            return Ok(None);
        }
        let conn = self.open_writable()?;
        let sql = if truncate {
            "PRAGMA wal_checkpoint(TRUNCATE)"
        } else {
            "PRAGMA wal_checkpoint(PASSIVE)"
        };
        let (_busy, log_frames, checkpointed_frames): (i64, i64, i64) =
            conn.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(Some((log_frames, checkpointed_frames)))
    }

    /// Open a read-only SQLite connection without migrations or application writes.
    /// Frozen reads and read-only fallbacks use private temporary snapshots, leaving source
    /// files untouched; ordinary WAL readers may update SQLite coordination sidecars.
    ///
    /// Returns `Ok(None)` when `state.db` does not exist yet or is an uninitialized 0-version file.
    pub fn open_readonly(&self) -> Result<Option<StateReadConnection>, StateError> {
        let meta = match std::fs::metadata(&self.db_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StateError::Io(e)),
        };
        if !meta.is_file() {
            return Err(StateError::Io(std::io::Error::other(format!(
                "{} is not a regular file",
                self.db_path.display()
            ))));
        }
        if meta.len() == 0 {
            return Ok(None);
        }

        let ro_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let open_folded = || -> Result<(StateReadConnection, i64), StateError> {
            let c = open_private_folded_copy(&self.db_path)?;
            let ver = schema::current_schema_version(&c)?;
            Ok((c, ver))
        };

        let (conn, ver) = if crate::env::frozen() {
            // Even a checkpointed database can change after opening. Keep a stable private
            // snapshot for the connection's lifetime, without creating or writing source SHM.
            open_folded()?
        } else {
            let direct = (|| -> Result<(Connection, i64), rusqlite::Error> {
                let c = Connection::open_with_flags(&self.db_path, ro_flags)?;
                let _ = c.busy_timeout(Duration::from_millis(5_000));
                let _ = c.execute_batch("PRAGMA query_only = ON;");
                let ver: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                Ok((c, ver))
            })();
            match direct {
                Ok((connection, ver)) => (
                    StateReadConnection {
                        connection,
                        _snapshot: None,
                    },
                    ver,
                ),
                Err(primary_err) if is_readonly_or_lock_fallback_err(&primary_err) => {
                    // Recovery and reader marks may require writes even on a read-only open.
                    // Fold a stable private copy instead, including any retained WAL/journal.
                    open_folded().map_err(|_| StateError::Sqlite(primary_err))?
                }
                Err(primary_err) => return Err(StateError::Sqlite(primary_err)),
            }
        };
        if ver > schema::STATE_SCHEMA_VERSION {
            return Err(StateError::NewerSchema {
                found: ver,
                supported: schema::STATE_SCHEMA_VERSION,
            });
        }
        if ver == 0 {
            return Ok(None);
        }
        Ok(Some(conn))
    }

    /// Execute a bounded read-only query against `state.db`.
    ///
    /// Under `--frozen`, delegates to [`crate::harness::store::sqlite::read`] so a checkpointed
    /// store without `-wal`/`-journal` frames is read in place via `immutable=1` without copying
    /// `state.db` to `TMPDIR`, while a store with active WAL/journal sidecars shares one
    /// [`crate::harness::store::sqlite::ReadScope`] snapshot.
    pub(crate) fn read_in<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StateError>,
    ) -> Result<Option<T>, StateError> {
        if crate::env::frozen() {
            let meta = match std::fs::metadata(&self.db_path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(StateError::Io(e)),
            };
            if !meta.is_file() {
                return Err(StateError::Io(std::io::Error::other(format!(
                    "{} is not a regular file",
                    self.db_path.display()
                ))));
            }
            if meta.len() == 0 {
                return Ok(None);
            }
            let mut inner_err: Option<StateError> = None;
            let res = crate::harness::store::sqlite::read(&self.db_path, |conn, _| {
                let ver = match schema::current_schema_version(conn) {
                    Ok(v) => v,
                    Err(e) => {
                        inner_err = Some(e);
                        anyhow::bail!("state schema check failed");
                    }
                };
                if ver > schema::STATE_SCHEMA_VERSION {
                    inner_err = Some(StateError::NewerSchema {
                        found: ver,
                        supported: schema::STATE_SCHEMA_VERSION,
                    });
                    anyhow::bail!("newer state schema");
                }
                if ver == 0 {
                    return Ok(None);
                }
                match f(conn) {
                    Ok(val) => Ok(Some(val)),
                    Err(e) => {
                        inner_err = Some(e);
                        anyhow::bail!("state read failed");
                    }
                }
            });
            return match res {
                Ok(val) => Ok(val),
                Err(_) if inner_err.is_some() => Err(inner_err.unwrap()),
                Err(other) => Err(StateError::Io(std::io::Error::other(other))),
            };
        }
        let Some(conn) = self.open_readonly()? else {
            return Ok(None);
        };
        f(&conn).map(Some)
    }
}

fn sibling_with_suffix(db_path: &Path, suffix: &str) -> PathBuf {
    let stem = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("state");
    let file_name = format!("{stem}.{suffix}");
    match db_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

#[cfg(unix)]
fn normalize_path_key(path: &Path) -> String {
    if let Some(parent) = path.parent() {
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            if let Some(name) = path.file_name() {
                return canon_parent.join(name).to_string_lossy().into_owned();
            }
        }
    }
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn is_readonly_or_lock_fallback_err(err: &rusqlite::Error) -> bool {
    let Some(sqlite_err) = err.sqlite_error() else {
        return false;
    };
    matches!(
        sqlite_err.code,
        rusqlite::ErrorCode::DatabaseBusy
            | rusqlite::ErrorCode::DatabaseLocked
            | rusqlite::ErrorCode::ReadOnly
            | rusqlite::ErrorCode::CannotOpen
            | rusqlite::ErrorCode::PermissionDenied
            | rusqlite::ErrorCode::SystemIoFailure
    ) || (sqlite_err.extended_code & 0xff) == rusqlite::ffi::SQLITE_PROTOCOL
}

fn copy_optional_sidecar(src: &Path, dst: &Path) -> std::io::Result<bool> {
    match std::fs::copy(src, dst) {
        Ok(bytes) => {
            set_restrictive_file_mode(dst);
            Ok(bytes > 0)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn private_temp_base() -> PathBuf {
    crate::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Read a stable store snapshot, including any WAL/journal frames, without touching source SHM or recovering in
/// place: copy the database and sidecars into a private `0700` directory, fold them into that copy
/// (`0600` so read-only `0444` source files still fold cleanly), and hand back an `immutable=1`
/// `query_only` connection over it. Unlink immediately where allowed; the owner retries removal
/// after the connection closes on platforms that prohibit deleting an open database.
fn open_private_folded_copy(db_path: &Path) -> Result<StateReadConnection, StateError> {
    open_private_folded_copy_with_check(db_path, || {})
}

fn open_private_folded_copy_with_check(
    db_path: &Path,
    mut copied: impl FnMut(),
) -> Result<StateReadConnection, StateError> {
    // A writer between the copies makes one attempt unusable. Each attempt samples the store
    // again, so many short waits (about a quarter second in total) find a quiet moment more
    // often than a few long ones: a busy store costs a reader time rather than its answer.
    const MAX_ATTEMPTS: u32 = 32;
    let wal_src = path_with_suffix(db_path, "-wal");
    let journal_src = path_with_suffix(db_path, "-journal");

    for attempt in 0..MAX_ATTEMPTS {
        let dir = private_temp_base().join(format!("cv-state-frozen-{}", uuid::Uuid::new_v4().simple()));
        #[cfg(unix)]
        ensure_private_dir_0700(&dir)?;
        #[cfg(not(unix))]
        std::fs::create_dir_all(&dir)?;
        let snapshot = SnapshotDirectory::retain_owned(dir).map_err(std::io::Error::other)?;
        let copy = snapshot.path().join("state.db");
        let copy_wal = path_with_suffix(&copy, "-wal");
        let copy_journal = path_with_suffix(&copy, "-journal");

        let identities = || [db_path, wal_src.as_path(), journal_src.as_path()].map(crate::source::FileId::at);
        let before = identities();
        let wal_before = wal_generation(&wal_src);

        let unstable = std::cell::Cell::new(false);
        let opened = (|| -> Result<Connection, StateError> {
            std::fs::copy(db_path, &copy)?;
            set_restrictive_file_mode(&copy);
            let copied_wal = copy_optional_sidecar(&wal_src, &copy_wal)?;
            let copied_journal = copy_optional_sidecar(&journal_src, &copy_journal)?;

            copied();
            // Never accept an unstable copy, including on the final retry.
            if !snapshot_copy_is_consistent(&before, &identities(), wal_before, &wal_src, &copy_wal) {
                unstable.set(true);
                return Err(StateError::Io(std::io::Error::other(
                    "state.db or its sidecars changed during snapshot copy",
                )));
            }

            if copied_wal || copied_journal {
                let fold = Connection::open(&copy)?;
                let _mode: String = fold.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
            }
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
            let conn = Connection::open_with_flags(immutable_sqlite_uri(&copy), flags)?;
            let _ = conn.busy_timeout(Duration::from_millis(5_000));
            let _ = conn.execute_batch("PRAGMA query_only = ON;");
            // Validate the private database before handing ownership to the caller.
            let _: i64 = conn.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
            Ok(conn)
        })();
        match opened {
            Ok(connection) => {
                // Preserve Unix's crash-safe unlink while the fd keeps the snapshot alive.
                // Windows rejects unlinking SQLite's open handle; ownership retries on close.
                snapshot.remove_now();
                return Ok(StateReadConnection {
                    connection,
                    _snapshot: Some(snapshot),
                });
            }
            Err(err) => {
                if attempt + 1 < MAX_ATTEMPTS && (unstable.get() || before != identities()) {
                    std::thread::sleep(Duration::from_millis(1 << attempt.min(3)));
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(StateError::Io(std::io::Error::other("failed to snapshot state.db")))
}

/// The 32-byte header of a write-ahead log: its salts and checkpoint sequence change whenever
/// SQLite restarts the log. `None` when there is no log or it has no complete header.
fn wal_generation(wal: &Path) -> Option<[u8; 32]> {
    use std::io::Read;
    let mut header = [0u8; 32];
    std::fs::File::open(wal).ok()?.read_exact(&mut header).ok()?;
    Some(header)
}

/// Whether the copy taken between `before` and `after` opens as one committed state.
///
/// With a write-ahead log, every page a checkpoint has moved into the database is still in the
/// log until the log restarts, and frames are only ever appended to it. A database copied first
/// and a log copied after it, both within one log generation, therefore recover to a committed
/// state however many commits or checkpoint writes landed in between: recovery stops at the last
/// whole commit and reads every checkpointed page from the log. Only a restart (a changed header)
/// or a replaced database file breaks that. Without a log nothing protects a torn database copy,
/// so every file must be unchanged.
fn snapshot_copy_is_consistent(
    before: &[Option<crate::source::FileId>; 3],
    after: &[Option<crate::source::FileId>; 3],
    wal_before: Option<[u8; 32]>,
    wal_src: &Path,
    copy_wal: &Path,
) -> bool {
    let (Some(db_before), Some(db_after)) = (before[0], after[0]) else {
        return false;
    };
    if before[2] != after[2] {
        return false;
    }
    match (wal_before, wal_generation(wal_src)) {
        (Some(generation), Some(now)) => {
            generation == now
                && wal_generation(copy_wal) == Some(generation)
                && (db_before.dev, db_before.ino) == (db_after.dev, db_after.ino)
        }
        (None, None) => before == after,
        _ => false,
    }
}

fn immutable_sqlite_uri(path: &Path) -> String {
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%').add(b'?').add(b'#').add(b' ');
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    #[cfg(unix)]
    let encoded = {
        use std::os::unix::ffi::OsStrExt;
        percent_encoding::percent_encode(abs.as_os_str().as_bytes(), RESERVED).to_string()
    };
    #[cfg(not(unix))]
    let encoded = {
        let text = format!("/{}", abs.to_string_lossy().replace('\\', "/"));
        percent_encoding::utf8_percent_encode(&text, RESERVED).to_string()
    };
    format!("file:{encoded}?immutable=1")
}

pub(crate) fn set_restrictive_file_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Ensure a private `0700` directory owned by the current effective UID (`#[cfg(unix)]`).
#[cfg(unix)]
pub(crate) fn ensure_private_dir_0700(dir: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)?;
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() {
        return Err(StateError::Io(std::io::Error::other(format!(
            "socket runtime path {} is not a directory",
            dir.display()
        ))));
    }
    let euid = unsafe { libc::geteuid() };
    if md.uid() != euid {
        return Err(StateError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("socket runtime dir {} not owned by uid {euid}", dir.display()),
        )));
    }
    if md.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod uri_tests {
    use super::*;

    #[test]
    fn frozen_safe_readonly_opens_non_ascii_state_db_path() {
        let world = cv_testkit::World::new("state-non-ascii");
        let dir = world.base.join("δοκιμή-日本語");
        std::fs::create_dir_all(&dir).unwrap();
        let store = StateStore::at_dir(&dir);
        store.append("op:1", "dig1", &serde_json::json!({"ok": true})).unwrap();

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(immutable_sqlite_uri(&dir.join("state.db")), flags).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM capture_inbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
