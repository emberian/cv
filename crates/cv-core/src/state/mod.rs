//! Authoritative local state spine (`$CLUSTERVISION_HOME/state.db`).
//!
//! `state.db` separates `cv`-owned durable facts from rebuildable derived caches (`catalog.db` and
//! search indexes). Every mutation executes under `WAL` + `synchronous=FULL` with explicit schema
//! migrations ([`schema::STATE_SCHEMA_VERSION`]) that are strictly independent of
//! [`crate::source::VERSION`].

pub mod hub;
pub mod recorder;
pub mod schema;
pub mod tables;

#[cfg(test)]
mod tests;

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
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )?;
        schema::ensure_migrated(&mut conn)?;
        Ok(conn)
    }

    /// Open a strictly read-only SQLite connection without creating directories or mutating disk.
    ///
    /// Returns `Ok(None)` when `state.db` does not exist yet or is an uninitialized 0-version file.
    pub fn open_readonly(&self) -> Result<Option<Connection>, StateError> {
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
        let wal_path = path_with_suffix(&self.db_path, "-wal");
        let wal_nonempty = || std::fs::metadata(&wal_path).is_ok_and(|m| m.len() > 0);
        let conn = if crate::env::frozen() {
            // A plain `SQLITE_OPEN_READ_ONLY` open of a WAL-mode database creates `-shm` (and an
            // empty `-wal`) whenever they are absent, which `--frozen` forbids. So:
            // - checkpointed store (no WAL frames): `immutable=1` opens nothing but the database;
            // - live store (`-wal` has frames and its `-shm` exists): the WAL-aware reader reuses
            //   the existing coordination files and creates none;
            // - retained WAL without `-shm` (a copied or crashed store): read a private folded
            //   copy, never recovering the WAL next to the original.
            if !wal_nonempty() {
                let uri = immutable_sqlite_uri(&self.db_path);
                Connection::open_with_flags(uri, ro_flags | OpenFlags::SQLITE_OPEN_URI)?
            } else if path_with_suffix(&self.db_path, "-shm").exists() {
                Connection::open_with_flags(&self.db_path, ro_flags)?
            } else {
                open_private_folded_copy(&self.db_path, &wal_path)?
            }
        } else {
            match Connection::open_with_flags(&self.db_path, ro_flags) {
                Ok(c) => c,
                Err(primary_err) => {
                    // Only fall back to `immutable=1` when no non-empty `-wal` sidecar exists (e.g. a
                    // checkpointed store on a read-only mount where SQLite cannot create `-shm`).
                    if wal_nonempty() {
                        return Err(StateError::Sqlite(primary_err));
                    }
                    let uri = immutable_sqlite_uri(&self.db_path);
                    Connection::open_with_flags(uri, ro_flags | OpenFlags::SQLITE_OPEN_URI)
                        .map_err(|_| StateError::Sqlite(primary_err))?
                }
            }
        };
        let _ = conn.busy_timeout(Duration::from_millis(2_000));
        let ver = schema::current_schema_version(&conn)?;
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

/// Read a store whose `-wal` holds frames but whose `-shm` is gone without recovering it in place:
/// copy the database and WAL into a private directory, fold the WAL into that copy, and hand back
/// an `immutable=1` connection over it. The copy is unlinked once the connection holds it open
/// (where the platform refuses to unlink an open file it is left for the OS temp cleaner).
fn open_private_folded_copy(db_path: &Path, wal_path: &Path) -> Result<Connection, StateError> {
    let dir = std::env::temp_dir().join(format!("cv-state-frozen-{}", uuid::Uuid::new_v4().simple()));
    #[cfg(unix)]
    ensure_private_dir_0700(&dir)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join("state.db");
    let opened = (|| -> Result<Connection, StateError> {
        std::fs::copy(db_path, &copy)?;
        std::fs::copy(wal_path, path_with_suffix(&copy, "-wal"))?;
        set_restrictive_file_mode(&copy);
        {
            let fold = Connection::open(&copy)?;
            let _mode: String = fold.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(immutable_sqlite_uri(&copy), flags)?;
        // Touch the file so the connection holds it open before the directory entry goes away.
        let _: i64 = conn.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
        Ok(conn)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    opened
}

fn immutable_sqlite_uri(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut uri = String::from("file:");
    for b in abs.to_string_lossy().as_bytes() {
        match *b {
            b'#' => uri.push_str("%23"),
            b'?' => uri.push_str("%3f"),
            b'%' => uri.push_str("%25"),
            b' ' => uri.push_str("%20"),
            ch => uri.push(ch as char),
        }
    }
    uri.push_str("?immutable=1");
    uri
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
