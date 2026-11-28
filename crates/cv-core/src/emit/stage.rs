//! Pre-publication staging for [`crate::emit`]: materialize or transact target output on the
//! destination filesystem in isolation, run native structural checks and read-back fidelity
//! verification against the exact staged state, and publish to `out_dir` (or `COMMIT` the SQLite
//! transaction) only after verification succeeds.

use crate::harness::store::bytes::ByteSource;
use crate::harness::EmitResult;
use crate::publish::{Claim, Set};
use anyhow::{Context, Result};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// What a staged emitter hands the pre-publication verifier before touching live destination names
/// or committing a shared SQLite database transaction.
pub(crate) enum StagedSource<'a> {
    /// Staged files inside an isolated temporary directory on the destination filesystem;
    /// `result.path` names the staged primary file or directory so the target's own adapter reads
    /// the exact bytes about to be published, and `destination` the live name that primary will
    /// take (nothing has been created there yet).
    Files {
        result: &'a EmitResult,
        destination: &'a Path,
    },
    /// Uncommitted rows inside a `BEGIN IMMEDIATE` transaction on the target SQLite database;
    /// triggers, autoincrement `rowid`s, and schema migrations have already fired on `conn`.
    #[cfg(feature = "sqlite")]
    Sqlite {
        conn: &'a rusqlite::Connection,
        result: &'a EmitResult,
    },
}

/// Callback invoked by a staged emitter after all transforms and native checks complete, strictly
/// before publication or transaction commit. Returning `Err` aborts publication / rolls back.
pub(crate) type VerifyFn<'a> = &'a mut dyn FnMut(StagedSource<'_>) -> Result<()>;

/// An isolated hidden staging directory (`.cv-stage-<tag>-<uuid>`) created on the destination's
/// own filesystem (never `std::env::temp_dir()`, avoiding cross-volume `EXDEV` hazards, and never
/// creating `out_dir` itself when `out_dir` does not yet exist). Removed recursively on drop.
pub(crate) struct StageWorkspace {
    root: PathBuf,
}

impl StageWorkspace {
    pub(crate) fn beside(out_dir: &Path, tag: &str) -> Result<Self> {
        let name = format!(".cv-stage-{tag}-{}", Uuid::new_v4());
        // Prefer `out_dir`'s parent when it already exists (same volume, zero touch to `out_dir`),
        // else walk `out_dir.ancestors()` for the deepest existing directory on that volume.
        let mut candidates: Vec<&Path> = Vec::new();
        if let Some(parent) = out_dir.parent().filter(|p| !p.as_os_str().is_empty() && p.is_dir()) {
            candidates.push(parent);
        }
        for anc in out_dir.ancestors() {
            let dir = if anc.as_os_str().is_empty() {
                Path::new(".")
            } else {
                anc
            };
            if dir.is_dir() && !candidates.contains(&dir) {
                candidates.push(dir);
            }
        }
        if candidates.is_empty() {
            candidates.push(Path::new("."));
        }
        let mut last_err = None;
        for base in candidates {
            let root = base.join(&name);
            match create_private_dir(&root) {
                Ok(()) => return Ok(Self { root }),
                Err(e) => last_err = Some((base.to_path_buf(), e)),
            }
        }
        let (base, err) = last_err.context("no candidate directory for staging workspace")?;
        Err(err).with_context(|| format!("creating staging workspace in {}", base.display()))
    }

    pub(crate) fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel.as_ref())
    }

    pub(crate) fn write_bytes(&self, rel: impl AsRef<Path>, bytes: &[u8]) -> Result<PathBuf> {
        let dst = self.path(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, bytes).with_context(|| format!("writing staged {}", dst.display()))?;
        Ok(dst)
    }

    pub(crate) fn write_source(&self, rel: impl AsRef<Path>, source: &ByteSource) -> Result<PathBuf> {
        let dst = self.path(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&dst).with_context(|| format!("creating staged {}", dst.display()))?;
        let mut reader = BufReader::with_capacity(256 * 1024, source.reader()?);
        std::io::copy(&mut reader, &mut out).with_context(|| format!("copying staged {}", dst.display()))?;
        Ok(dst)
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

impl Drop for StageWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Publish an already-finished [`ByteSource`] to `path` with `claim`, creating parent directories
/// only at commit time.
pub(crate) fn publish_source(source: &ByteSource, path: &Path, claim: Claim) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let reader = BufReader::with_capacity(256 * 1024, source.reader()?);
    crate::publish::publish_reader(path, "stage", reader, claim)?;
    Ok(())
}

/// Stage an already-finished [`ByteSource`] into a [`Set`] for `path`.
pub(crate) fn stage_source_into(set: &mut Set, source: &ByteSource, path: &Path, claim: Claim) -> Result<()> {
    let reader = BufReader::with_capacity(256 * 1024, source.reader()?);
    set.stage(path, "stage", reader, claim)
}

/// Guard for SQLite emission: if a transaction aborts before [`Self::disarm`] and this operation
/// exclusively created `db_path` (with zero committed user tables remaining after rollback),
/// remove the database file and any newly created empty parent directories so a failed emit leaves
/// zero trace on disk. Pre-existing or concurrently populated databases are never unlinked.
#[cfg(feature = "sqlite")]
pub(crate) struct SqliteRollbackGuard {
    db_path: PathBuf,
    created: Option<crate::source::FileId>,
    created_dirs: Vec<PathBuf>,
    committed: bool,
}

#[cfg(feature = "sqlite")]
impl SqliteRollbackGuard {
    pub(crate) fn prepare(db_path: &Path) -> Result<Self> {
        let mut created_dirs = Vec::new();
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut cur = Some(parent);
            while let Some(dir) = cur {
                if dir.as_os_str().is_empty() || dir.exists() {
                    break;
                }
                created_dirs.push(dir.to_path_buf());
                cur = dir.parent();
            }
            if !created_dirs.is_empty() {
                fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let created = match fs::OpenOptions::new().write(true).create_new(true).open(db_path) {
            Ok(f) => crate::source::FileId::of(
                &f.metadata()
                    .with_context(|| format!("statting {}", db_path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(e) => return Err(e).with_context(|| format!("creating {}", db_path.display())),
        };
        Ok(Self {
            db_path: db_path.to_path_buf(),
            created,
            created_dirs,
            committed: false,
        })
    }

    pub(crate) fn disarm(&mut self) {
        self.committed = true;
    }
}

#[cfg(feature = "sqlite")]
fn same_created_inode(created: crate::source::FileId, current: crate::source::FileId) -> bool {
    #[cfg(unix)]
    {
        (current.dev, current.ino) == (created.dev, created.ino)
    }
    #[cfg(not(unix))]
    {
        current.ctime_ns == created.ctime_ns
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_is_unpopulated(db_path: &Path, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = rusqlite::Connection::open_with_flags(db_path, flags) else {
        return false;
    };
    conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

#[cfg(feature = "sqlite")]
impl Drop for SqliteRollbackGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(created) = self.created {
            let can_unlink = crate::source::FileId::at(&self.db_path)
                .is_some_and(|cur| same_created_inode(created, cur) && sqlite_is_unpopulated(&self.db_path, cur.len));
            if can_unlink {
                let _ = fs::remove_file(&self.db_path);
                for suffix in ["-wal", "-shm", "-journal"] {
                    let mut os = self.db_path.as_os_str().to_os_string();
                    os.push(suffix);
                    let _ = fs::remove_file(PathBuf::from(os));
                }
                for dir in &self.created_dirs {
                    let _ = fs::remove_dir(dir);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// #tooth Concurrent creation or population of `db_path` prevents rollback unlink, while an
    /// aborted fresh emit still leaves zero database or parent directory behind.
    #[test]
    fn tooth_sqlite_rollback_guard_preserves_concurrently_populated_db() {
        let tmp = std::env::temp_dir().join(format!("cv-stage-guard-{}", Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();

        // 1. Fresh rolled-back DB is cleanly unlinked along with created parent dirs.
        let fresh_db = tmp.join("fresh/sub/state.db");
        {
            let _guard = SqliteRollbackGuard::prepare(&fresh_db).unwrap();
            let mut conn = rusqlite::Connection::open(&fresh_db).unwrap();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            tx.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY);")
                .unwrap();
            drop(tx);
        }
        assert!(!fresh_db.exists());
        assert!(!tmp.join("fresh").exists());

        // 2. If another writer populates our newly created 0-byte DB before drop, never unlink it.
        let raced_db = tmp.join("raced/state.db");
        {
            let _guard = SqliteRollbackGuard::prepare(&raced_db).unwrap();
            let peer = rusqlite::Connection::open(&raced_db).unwrap();
            peer.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY); INSERT INTO sessions VALUES ('live');")
                .unwrap();
        }
        assert!(raced_db.exists(), "concurrently populated DB must survive rollback");
        let _ = fs::remove_dir_all(&tmp);
    }
}
