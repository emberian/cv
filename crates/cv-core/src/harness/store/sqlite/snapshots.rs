//! Private cold-WAL backings. A kernel lease, not a PID, authorizes reclamation after a
//! crashed reader. The stable gate serializes publication and removal; only versioned,
//! identity-bound owner records are reclaimable. Legacy/foreign temporary paths are untouched.

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAGIC: &str = "cv-sqlite-owners-v1";
const RECORD_LIMIT: u64 = 4096;

#[derive(Clone)]
pub(super) struct Namespace {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Identity {
    volume: u64,
    file: u128,
}

#[derive(Serialize, Deserialize)]
struct GateRecord {
    magic: String,
    root: Identity,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct OwnerRecord {
    magic: String,
    id: String,
    directory: Identity,
    lease: Identity,
}

/// Every backing retains this owner, even if its connection outlives the cache/read scope.
pub(super) struct Owner {
    namespace: Namespace,
    directory: PathBuf,
    lease_path: PathBuf,
    lease: Option<File>,
    record: OwnerRecord,
}

impl Namespace {
    pub(super) fn current() -> Self {
        // On disk beside cv's catalog when that directory is ours to write: a temporary
        // directory is often RAM-backed, and a multi-GB copy there is shared memory charged to
        // the reader's cgroup. A frozen or unwritable catalog keeps the private per-home
        // namespace in the temporary directory, which needs no writable cv state.
        let beside = crate::catalog::db_path()
            .filter(|_| !crate::env::frozen())
            .and_then(|db| Some(db.parent()?.join(MAGIC)))
            .filter(|root| {
                private_directory(root, false)
                    .and_then(|()| check_private_root(root))
                    .and_then(|()| {
                        // An existing private namespace can have become unwritable. Test an
                        // actual create, including ACLs, before selecting it over TMPDIR.
                        let probe = root.join(format!(".probe-{}", uuid::Uuid::new_v4()));
                        drop(new_file(&probe)?);
                        drop(RemoveFile(Some(probe)));
                        Ok(())
                    })
                    .is_ok()
            });
        if let Some(root) = beside {
            return Self { root };
        }
        // Separate users in a shared /tmp without depending on a writable cv catalog. The
        // namespace remains private and identity checked; this hash is a name, not authority.
        let home = crate::env::home_dir().unwrap_or_default();

        // FNV-1a gives this namespace a stable spelling; DefaultHasher does not promise that.
        let hash = home
            .as_os_str()
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
        Self {
            root: std::env::temp_dir().join(format!("cv-sqlite-owners-v1-{hash:016x}")),
        }
    }

    fn gate(&self, create: bool, blocking: bool) -> Result<Option<File>> {
        if !self.root.exists() && !create {
            return Ok(None);
        }
        if create {
            private_directory(&self.root, false)?;
        }
        let root = path_identity(&self.root, true)?;
        check_private_root(&self.root)?;
        let path = self.root.join("gate");
        if create && !path.exists() {
            // Publish a complete, already-locked gate under a new name. Never truncate or
            // replace the stable gate: two different lock inodes would split authority.
            let mut candidate = crate::publish::Staged::create(&self.root, "gate", 0o600)?;
            FileExt::lock_exclusive(candidate.file())?;
            serde_json::to_writer(
                candidate.file(),
                &GateRecord {
                    magic: MAGIC.into(),
                    root,
                },
            )?;
            match candidate.publish(&path, crate::publish::Claim::New) {
                Ok(candidate) => {
                    ensure_path(&candidate, &path)?;
                    self.ensure_root(root)?;
                    return Ok(Some(candidate));
                }
                Err(e) if e.downcast_ref::<crate::publish::Exists>().is_some() => {}
                Err(e) => return Err(e),
            }
        }
        let mut gate = existing_file(&path)?;
        if blocking {
            FileExt::lock_exclusive(&gate)?;
        } else if !FileExt::try_lock_exclusive(&gate)? {
            return Ok(None);
        }
        // A pathname may have been replaced while we waited for the lock. Do not authorize
        // publication/deletion with a lock on the old inode.
        ensure_path(&gate, &path)?;
        self.ensure_root(root)?;
        let record: GateRecord = read_record(&mut gate)?;
        anyhow::ensure!(
            record.magic == MAGIC && record.root == root,
            "unrecognized SQLite snapshot namespace"
        );
        Ok(Some(gate))
    }

    fn ensure_root(&self, expected: Identity) -> Result<()> {
        anyhow::ensure!(
            path_identity(&self.root, true)? == expected,
            "SQLite snapshot namespace identity changed"
        );
        check_private_root(&self.root)
    }

    pub(super) fn owner(&self) -> Result<Arc<Owner>> {
        let _gate = self
            .gate(true, true)?
            .context("SQLite snapshot namespace unavailable")?;
        let id = uuid::Uuid::new_v4();
        let directory = self.root.join(id.to_string());
        private_directory(&directory, true)?;
        let mut cleanup_directory = RemoveDirectory(Some(directory.clone()));
        let lease_path = self.root.join(format!("{id}.lease"));
        let mut cleanup_lease = RemoveFile(None);
        let mut lease = new_file(&lease_path)?;
        cleanup_lease.0 = Some(lease_path.clone());
        FileExt::lock_exclusive(&lease)?;
        let record = OwnerRecord {
            magic: MAGIC.into(),
            id: id.to_string(),
            directory: path_identity(&directory, true)?,
            lease: file_identity(&lease)?,
        };
        // This is the sole commit record. No copied payload exists until it is complete and
        // the gate is released. An interrupted initialization can strand only tiny metadata.
        let mut marker = new_file(&directory.join("owner.json"))?;
        serde_json::to_writer(&mut marker, &record)?;
        marker.flush()?;
        serde_json::to_writer(&mut lease, &record)?;
        lease.flush()?;
        cleanup_directory.0 = None;
        cleanup_lease.0 = None;
        Ok(Arc::new(Owner {
            namespace: self.clone(),
            directory,
            lease_path,
            lease: Some(lease),
            record,
        }))
    }

    /// Best effort at operation boundaries. A malformed/foreign owner is never deletion
    /// authority; a live lease is never a candidate, regardless of PID reuse or scope age.
    pub(super) fn reap(&self) {
        let Ok(Some(_gate)) = self.gate(false, false) else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.root) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".lease") else {
                continue;
            };
            let Ok(id) = uuid::Uuid::parse_str(stem) else { continue };
            if stem != id.to_string() {
                continue;
            }
            let Ok(mut lease) = existing_file(&path) else { continue };
            if !matches!(FileExt::try_lock_exclusive(&lease), Ok(true)) {
                continue;
            }
            let Ok(record) = read_record::<OwnerRecord>(&mut lease) else {
                continue;
            };
            if record.magic != MAGIC || record.id != stem {
                continue;
            }
            let directory = self.root.join(stem);
            if !recognized(&lease, &path, &directory, &record) {
                continue;
            }
            if remove_owned_directory(&directory).is_err() {
                continue;
            }
            // Windows cannot remove an open lease. The namespace gate protects the gap
            // between releasing this lease and unlinking its pathname.
            drop(lease);
            let _ = fs::remove_file(path);
        }
    }
}

impl Owner {
    pub(super) fn snapshot_directory(&self) -> Result<PathBuf> {
        let directory = self.directory.join(uuid::Uuid::new_v4().to_string());
        private_directory(&directory, true)?;
        Ok(directory)
    }
}

/// An operation-owned SQLite backing directory, removed only if its identity still matches.
/// Keep this guard after its connection field so Windows closes the database before cleanup.
#[derive(Debug)]
pub(crate) struct SnapshotDirectory {
    path: PathBuf,
    identity: Identity,
}

impl SnapshotDirectory {
    pub(crate) fn retain_owned(path: PathBuf) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let identity = snapshot_identity(&path)?;
        Ok(Self { path, identity })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Unix can unlink an open backing immediately; Windows may need the owner's drop retry.
    pub(crate) fn remove_now(&self) {
        remove_snapshot_directory(&self.path, self.identity);
    }
}

impl Drop for SnapshotDirectory {
    fn drop(&mut self) {
        self.remove_now();
    }
}

/// Bind individual cold backings to their directory inode too: the owning tree's
/// recognition record does not authorize an eagerly dropped backing at a reused path.
pub(super) fn snapshot_identity(path: &Path) -> Result<Identity> {
    path_identity(path, true)
}

pub(super) fn remove_snapshot_directory(path: &Path, identity: Identity) {
    if path_identity(path, true).is_ok_and(|current| current == identity) {
        let _ = fs::remove_dir_all(path);
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // A contended namespace can be reaped after this owner's lease is released.
        // Dropping a reader must not wait for another process's snapshot publication.
        let Ok(Some(_gate)) = self.namespace.gate(false, false) else {
            return;
        };
        let Some(lease) = self.lease.as_ref() else { return };
        if !recognized(lease, &self.lease_path, &self.directory, &self.record) {
            return;
        }
        if remove_owned_directory(&self.directory).is_err() {
            return;
        }
        drop(self.lease.take());
        let _ = fs::remove_file(&self.lease_path);
    }
}

fn recognized(lease: &File, path: &Path, directory: &Path, record: &OwnerRecord) -> bool {
    file_identity(lease).is_ok_and(|id| id == record.lease)
        && ensure_path(lease, path).is_ok()
        && path_identity(directory, true).is_ok_and(|id| id == record.directory)
        && existing_file(&directory.join("owner.json"))
            .and_then(|mut file| read_record::<OwnerRecord>(&mut file))
            .is_ok_and(|marker| marker == *record)
}

fn remove_owned_directory(directory: &Path) -> Result<()> {
    // Keep the recognition marker until every payload has gone. If this reaper itself dies
    // midway through recursive removal, the next operation can still recognize the owner.
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == "owner.json" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_file(directory.join("owner.json"))?;
    fs::remove_dir(directory)?;
    Ok(())
}

fn read_record<T: serde::de::DeserializeOwned>(file: &mut File) -> Result<T> {
    anyhow::ensure!(
        file.metadata()?.len() <= RECORD_LIMIT,
        "oversized SQLite snapshot record"
    );
    file.seek(SeekFrom::Start(0))?;
    Ok(serde_json::from_reader(file.take(RECORD_LIMIT + 1))?)
}

fn ensure_path(file: &File, path: &Path) -> Result<()> {
    anyhow::ensure!(
        file_identity(file)? == path_identity(path, false)?,
        "SQLite snapshot lock identity changed"
    );
    Ok(())
}

fn private_directory(path: &Path, exclusive: bool) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(e) if !exclusive && e.kind() == std::io::ErrorKind::AlreadyExists => {
            path_identity(path, true)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn check_private_root(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: geteuid has no arguments or pointers and returns this process's effective UID.
        let uid = unsafe { geteuid() };
        let meta = fs::symlink_metadata(path)?;
        anyhow::ensure!(
            meta.uid() == uid && meta.mode() & 0o077 == 0,
            "SQLite snapshot namespace is not private or owned"
        );
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn existing_file(path: &Path) -> Result<File> {
    path_identity(path, false)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    ensure_path(&file, path)?;
    Ok(file)
}

struct RemoveFile(Option<PathBuf>);
impl Drop for RemoveFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}
struct RemoveDirectory(Option<PathBuf>);
impl Drop for RemoveDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(unix)]
fn path_identity(path: &Path, directory: bool) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        if directory { meta.is_dir() } else { meta.is_file() },
        "invalid SQLite snapshot path"
    );
    Ok(Identity {
        volume: meta.dev(),
        file: u128::from(meta.ino()),
    })
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok(Identity {
        volume: meta.dev(),
        file: u128::from(meta.ino()),
    })
}

#[cfg(windows)]
fn path_identity(path: &Path, directory: bool) -> Result<Identity> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.file_attributes() & 0x400 == 0 && if directory { meta.is_dir() } else { meta.is_file() },
        "invalid SQLite snapshot path"
    );
    // BACKUP_SEMANTICS opens directories; OPEN_REPARSE_POINT prevents following a substitution.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x02000000 | 0x00200000)
        .open(path)?;
    file_identity(&file)
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileIdInfo {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let mut info = FileIdInfo { volume: 0, id: [0; 16] };
    // SAFETY: the live file handle and writable, correctly sized FILE_ID_INFO remain valid
    // for this synchronous call. FileIdInfo (18) uses 128-bit IDs, including on ReFS.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FileIdInfo>() as u32,
        )
    };
    anyhow::ensure!(
        ok != 0,
        "SQLite snapshot file identity: {}",
        std::io::Error::last_os_error()
    );
    Ok(Identity {
        volume: info.volume,
        file: u128::from_le_bytes(info.id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> (Scratch, Namespace) {
        let directory = std::env::temp_dir().join(format!("cv-lease-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let namespace = Namespace {
            root: directory.join("owners"),
        };
        (Scratch(directory), namespace)
    }
    #[test]
    fn owned_snapshot_directory_cleanup_preserves_a_replacement() {
        let (scratch, _) = fixture();
        let path = scratch.0.join("backing");
        fs::create_dir(&path).unwrap();
        let owned = SnapshotDirectory::retain_owned(path.clone()).unwrap();
        let moved = scratch.0.join("moved-backing");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("replacement"), b"keep").unwrap();
        drop(owned);
        assert_eq!(fs::read(path.join("replacement")).unwrap(), b"keep");
        assert!(moved.exists());
    }

    /// Beside a writable catalog, private; a frozen environment keeps the temporary directory.
    #[test]
    fn namespace_stages_beside_the_catalog_unless_frozen() {
        let (scratch, _) = fixture();
        let env = crate::env::Env::from_vars([
            ("HOME", scratch.0.join("home")),
            ("CLUSTERVISION_HOME", scratch.0.join("catalog")),
        ]);
        let frozen = env.clone().with_frozen();
        let beside = {
            let _scope = env.enter();
            Namespace::current().root
        };
        assert_eq!(beside, scratch.0.join("catalog").join(MAGIC));
        check_private_root(&beside).unwrap();
        let _scope = frozen.enter();
        assert!(Namespace::current().root.starts_with(std::env::temp_dir()));
        assert!(!Namespace::current().root.starts_with(&scratch.0));
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_existing_catalog_namespace_falls_back_to_temporary() {
        use std::os::unix::fs::PermissionsExt;
        let (scratch, _) = fixture();
        let _env = crate::env::Env::from_vars([
            ("HOME", scratch.0.join("home")),
            ("CLUSTERVISION_HOME", scratch.0.join("catalog")),
        ])
        .enter();
        let root = Namespace::current().root;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        let writable = new_file(&root.join("permission-probe")).is_ok();
        let selected = Namespace::current().root;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        if !writable {
            // A privileged test runner may bypass directory permissions.
            assert!(selected.starts_with(std::env::temp_dir()));
            assert_ne!(selected, root);
        }
    }
    fn abandon(owner: Arc<Owner>) {
        let mut owner = Arc::try_unwrap(owner).ok().unwrap();
        drop(owner.lease.take()); // Model process death: release the kernel lease, bypass cleanup.
    }
    fn payload(owner: &Owner) -> PathBuf {
        let directory = owner.snapshot_directory().unwrap();
        let file = directory.join("store.db");
        fs::write(&file, b"owned payload").unwrap();
        file
    }

    #[test]
    fn leases_keep_live_owners_and_reclaim_only_abandoned_payloads() {
        let (_scratch, namespace) = fixture();
        let live = namespace.owner().unwrap();
        let live_payload = payload(&live);
        let dead = namespace.owner().unwrap();
        let dead_payload = payload(&dead);
        let dead_path = dead.lease_path.clone();
        abandon(dead);
        namespace.reap();
        assert!(live_payload.exists());
        assert!(!dead_payload.exists());
        assert!(!dead_path.exists());
        for _ in 0..20 {
            payload(&live);
        }
        assert_eq!(
            fs::read_dir(&namespace.root)
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "lease"))
                .count(),
            1
        );
        drop(live);
        assert_eq!(fs::read_dir(&namespace.root).unwrap().count(), 1); // Stable gate only.
    }

    #[test]
    fn malformed_unmarked_and_replaced_owners_are_not_deletion_authority() {
        let (_scratch, namespace) = fixture();
        let dead = namespace.owner().unwrap();
        let directory = dead.directory.clone();
        let lease = dead.lease_path.clone();
        abandon(dead);
        fs::write(&lease, b"not an owner record").unwrap();
        namespace.reap();
        assert!(directory.exists());
        let dead = namespace.owner().unwrap();
        let directory = dead.directory.clone();
        abandon(dead);
        fs::remove_file(directory.join("owner.json")).unwrap();
        namespace.reap();
        assert!(directory.exists());
        let dead = namespace.owner().unwrap();
        let directory = dead.directory.clone();
        abandon(dead);
        let renamed = directory.with_extension("original");
        fs::rename(&directory, &renamed).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::copy(renamed.join("owner.json"), directory.join("owner.json")).unwrap();
        namespace.reap();
        assert!(directory.exists());
        assert!(renamed.exists());
        let foreign = namespace.root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("store.db"), b"foreign").unwrap();
        namespace.reap();
        assert!(foreign.join("store.db").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_owners_leases_and_namespaces_are_never_followed() {
        use std::os::unix::fs::symlink;
        let (scratch, namespace) = fixture();
        let dead = namespace.owner().unwrap();
        let directory = dead.directory.clone();
        let lease_path = dead.lease_path.clone();
        abandon(dead);
        let target = scratch.0.join("foreign");
        fs::rename(&directory, &target).unwrap();
        symlink(&target, &directory).unwrap();
        namespace.reap();
        assert!(target.exists());
        fs::remove_file(&directory).unwrap();
        fs::rename(&target, &directory).unwrap();
        let saved = lease_path.with_extension("saved");
        fs::rename(&lease_path, &saved).unwrap();
        symlink(&saved, &lease_path).unwrap();
        namespace.reap();
        assert!(directory.exists());
        let linked = Namespace {
            root: scratch.0.join("linked"),
        };
        symlink(&namespace.root, &linked.root).unwrap();
        linked.reap();
        assert!(linked.owner().is_err());
        assert!(directory.exists());
    }

    #[test]
    fn gate_excludes_reaper_during_publication_and_rejects_replaced_lock_handle() {
        let (_scratch, namespace) = fixture();
        let gate = namespace.gate(true, true).unwrap().unwrap();
        // Publication is in progress. A concurrent operation boundary does not stall or
        // claim even a fully recognizable, currently unlocked record behind this gate.
        let id = uuid::Uuid::new_v4();
        let directory = namespace.root.join(id.to_string());
        private_directory(&directory, true).unwrap();
        let path = namespace.root.join(format!("{id}.lease"));
        let mut lease = new_file(&path).unwrap();
        let record = OwnerRecord {
            magic: MAGIC.into(),
            id: id.to_string(),
            directory: path_identity(&directory, true).unwrap(),
            lease: file_identity(&lease).unwrap(),
        };
        serde_json::to_writer(&mut lease, &record).unwrap();
        serde_json::to_writer(new_file(&directory.join("owner.json")).unwrap(), &record).unwrap();
        drop(lease);
        namespace.reap();
        assert!(directory.exists());
        drop(gate);
        namespace.reap();
        assert!(!directory.exists());
        let old = existing_file(&namespace.root.join("gate")).unwrap();
        fs::rename(namespace.root.join("gate"), namespace.root.join("old-gate")).unwrap();
        new_file(&namespace.root.join("gate")).unwrap();
        FileExt::lock_exclusive(&old).unwrap();
        assert!(ensure_path(&old, &namespace.root.join("gate")).is_err());
        assert!(namespace.owner().is_err());
    }

    #[test]
    fn namespace_replacement_cannot_reuse_the_old_gate_inode() {
        let (scratch, namespace) = fixture();
        let gate = namespace.gate(true, true).unwrap().unwrap();
        let identity = path_identity(&namespace.root, true).unwrap();
        let saved = scratch.0.join("saved");
        fs::rename(&namespace.root, &saved).unwrap();
        private_directory(&namespace.root, true).unwrap();
        fs::rename(saved.join("gate"), namespace.root.join("gate")).unwrap();
        assert!(ensure_path(&gate, &namespace.root.join("gate")).is_ok());
        assert!(namespace.ensure_root(identity).is_err());
        drop(gate);
        assert!(namespace.owner().is_err());
    }

    #[test]
    fn interrupted_payload_removal_preserves_recognition_until_payloads_are_gone() {
        let (_scratch, namespace) = fixture();
        let dead = namespace.owner().unwrap();
        let first = payload(&dead);
        let second = payload(&dead);
        let directory = dead.directory.clone();
        abandon(dead);
        // A terminated remover may have removed some files/subdirectories already. The
        // owner marker is outside those payload subtrees and remains replay authority.
        fs::remove_file(&first).unwrap();
        namespace.reap();
        assert!(!second.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn nested_boundary_moves_between_threads_and_origin_can_start_again() {
        let (_scratch, namespace) = fixture();
        let outer = super::super::operation_boundary(&namespace);
        let nested = super::super::operation_boundary(&namespace);
        assert!(Arc::ptr_eq(&outer, &nested));
        let dead = namespace.owner().unwrap();
        let dead_payload = payload(&dead);
        abandon(dead);
        drop(outer);
        assert!(dead_payload.exists()); // No per-nested-scope scan.
        std::thread::spawn(move || drop(nested)).join().unwrap();
        assert!(!dead_payload.exists());
        let dead = namespace.owner().unwrap();
        let dead_payload = payload(&dead);
        abandon(dead);
        let next = super::super::operation_boundary(&namespace);
        assert!(!dead_payload.exists()); // Expired origin Weak is not a stuck depth counter.
        drop(next);
    }

    #[test]
    fn another_threads_long_poll_does_not_suppress_finished_handler_reclamation() {
        let (_scratch, namespace) = fixture();
        let long_poll = super::super::operation_boundary(&namespace);
        let namespace = namespace.clone();
        std::thread::spawn(move || {
            let ordinary = super::super::operation_boundary(&namespace);
            let dead = namespace.owner().unwrap();
            let dead_payload = payload(&dead);
            abandon(dead);
            drop(ordinary);
            assert!(!dead_payload.exists());
        })
        .join()
        .unwrap();
        drop(long_poll);
    }

    #[cfg(unix)]
    #[test]
    fn permissive_namespace_cannot_create_or_reap() {
        use std::os::unix::fs::PermissionsExt;
        let (_scratch, namespace) = fixture();
        let dead = namespace.owner().unwrap();
        let file = payload(&dead);
        abandon(dead);
        fs::set_permissions(&namespace.root, fs::Permissions::from_mode(0o755)).unwrap();
        namespace.reap();
        assert!(file.exists());
        assert!(namespace.owner().is_err());
        fs::set_permissions(&namespace.root, fs::Permissions::from_mode(0o700)).unwrap();
        namespace.reap();
        assert!(!file.exists());
    }

    #[test]
    fn child_lease_entry() {
        let Some(root) = std::env::var_os("CV_SQLITE_LEASE_TEST_ROOT") else {
            return;
        };
        let namespace = Namespace { root: root.into() };
        let owner = namespace.owner().unwrap();
        let file = payload(&owner);
        fs::write(
            namespace.root.join("ready"),
            file.as_os_str().to_string_lossy().as_bytes(),
        )
        .unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn cross_process_kernel_lease_survives_reaper_and_reclaims_after_kill() {
        let (_scratch, namespace) = fixture();
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "harness::store::sqlite::snapshots::tests::child_lease_entry",
                    "--nocapture",
                ])
                .env("CV_SQLITE_LEASE_TEST_ROOT", &namespace.root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !namespace.root.join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ready = fs::read_to_string(namespace.root.join("ready"));
        if ready.is_err() {
            let _ = child.0.kill();
            let _ = child.0.wait();
            panic!("child did not publish lease");
        }
        let file = PathBuf::from(ready.unwrap());
        namespace.reap();
        assert!(file.exists());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        namespace.reap();
        assert!(!file.exists());
    }
}
