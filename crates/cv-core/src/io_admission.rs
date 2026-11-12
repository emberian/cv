//! Shared device-aware I/O admission scheduler, runtime byte-source policy (`Buffered` / `Mapped`
//! / `Slice`), physical `(object, offset)` read coalescing, and macOS `getattrlistbulk(2)` bulk
//! directory metadata enumeration.
//!
//! Design invariants:
//! 1. **External transcripts never `mmap`**: mutable harness-owned files always resolve to
//!    [`ByteSourcePolicy::Buffered`] (positioned `pread`/`read_at`) so concurrent truncation by an
//!    external harness can never fault with `SIGBUS`. [`ByteSourcePolicy::Mapped`] is reserved for
//!    local sealed immutable files (`packs/*.pack.zst` and local immutable segments).
//! 2. **Per-device I/O admission sits below adapters** and separate from CPU worker concurrency:
//!    known rotational HDDs admit `1` active sequential bulk stream per device (`max_streams = 1`),
//!    NVMe admits bounded multi-stream concurrency, and network mounts admit bounded large-range
//!    concurrency with wider read coalescing.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

/// Runtime byte-access strategy selected per source path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteSourcePolicy {
    /// Positioned buffered reads (`pread` / `seek_read`): immune to external truncation `SIGBUS`.
    Buffered,
    /// Read-only memory map: used only on local sealed immutable files (`packs/`, private spills).
    Mapped,
    /// In-memory byte slice (`wasm32` / browser-loaded archives).
    Slice,
}

/// Physical storage class governing stream concurrency and read-coalescing geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// Local NVMe / SSD flash storage.
    Nvme,
    /// Spinning rotational disk: strictly 1 sequential bulk read stream per device.
    RotationalHdd,
    /// Network filesystem (`nfs`, `smbfs`, `cifs`, `afpfs`, `webdav`, non-local mounts).
    Network,
    /// Unclassified storage: conservative bounded concurrency.
    Unknown,
}

impl DeviceClass {
    pub const fn default_policy(self) -> DevicePolicy {
        match self {
            Self::RotationalHdd => DevicePolicy {
                class: self,
                max_streams: 1,
                coalesce_gap_bytes: 128 * 1024,
                max_coalesced_bytes: 4 * 1024 * 1024,
            },
            Self::Nvme => DevicePolicy {
                class: self,
                max_streams: 8,
                coalesce_gap_bytes: 16 * 1024,
                max_coalesced_bytes: 4 * 1024 * 1024,
            },
            Self::Network => DevicePolicy {
                class: self,
                max_streams: 4,
                coalesce_gap_bytes: 256 * 1024,
                max_coalesced_bytes: 8 * 1024 * 1024,
            },
            Self::Unknown => DevicePolicy {
                class: self,
                max_streams: 4,
                coalesce_gap_bytes: 64 * 1024,
                max_coalesced_bytes: 4 * 1024 * 1024,
            },
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "hdd" | "rotational" | "rotational_hdd" => Some(Self::RotationalHdd),
            "nvme" | "ssd" | "flash" => Some(Self::Nvme),
            "net" | "network" | "nfs" | "smb" => Some(Self::Network),
            "unknown" | "auto" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// OS device identifier (`st_dev` on Unix, `0` where unavailable).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u64);

/// Admission and coalescing parameters for one physical device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicePolicy {
    pub class: DeviceClass,
    pub max_streams: usize,
    pub coalesce_gap_bytes: u64,
    pub max_coalesced_bytes: usize,
}

impl DevicePolicy {
    pub fn normalized(mut self) -> Self {
        self.max_streams = self.max_streams.max(1);
        self.max_coalesced_bytes = self.max_coalesced_bytes.max(4096);
        self
    }
}

struct DeviceSlot {
    policy: DevicePolicy,
    overridden: bool,
    active: usize,
    peak_active: usize,
    next_ticket: u64,
    serving_ticket: u64,
    waiters: VecDeque<u64>,
    owners: HashMap<std::thread::ThreadId, usize>,
}

impl DeviceSlot {
    fn new(policy: DevicePolicy, overridden: bool) -> Self {
        Self {
            policy: policy.normalized(),
            overridden,
            active: 0,
            peak_active: 0,
            next_ticket: 0,
            serving_ticket: 0,
            waiters: VecDeque::new(),
            owners: HashMap::new(),
        }
    }
}

struct DeviceLock {
    state: Mutex<DeviceSlot>,
    ready: Condvar,
}

/// Shared per-device I/O admission controller.
pub struct IoAdmission {
    devices: Mutex<HashMap<DeviceId, std::sync::Arc<DeviceLock>>>,
    sealed_roots: Mutex<Vec<PathBuf>>,
}

impl Default for IoAdmission {
    fn default() -> Self {
        Self::new()
    }
}

impl IoAdmission {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
            sealed_roots: Mutex::new(Vec::new()),
        }
    }

    /// Process-wide shared I/O admission scheduler.
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<IoAdmission> = OnceLock::new();
        GLOBAL.get_or_init(Self::new)
    }

    /// Register a local directory containing sealed immutable pack/segment files eligible for `Mapped` reads.
    pub fn register_sealed_immutable_dir(&self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        let mut roots = self.sealed_roots.lock().unwrap_or_else(|e| e.into_inner());
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }

    fn is_registered_sealed_path(&self, path: &Path) -> bool {
        let roots = self.sealed_roots.lock().unwrap_or_else(|e| e.into_inner());
        roots.iter().any(|root| path.starts_with(root))
    }

    /// Explicitly configure or override the admission policy for `dev`.
    pub fn set_device_override(&self, dev: DeviceId, policy: DevicePolicy) {
        let policy = policy.normalized();
        let lock = {
            let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(dev)
                .or_insert_with(|| {
                    std::sync::Arc::new(DeviceLock {
                        state: Mutex::new(DeviceSlot::new(policy, true)),
                        ready: Condvar::new(),
                    })
                })
                .clone()
        };
        let mut slot = lock.state.lock().unwrap_or_else(|e| e.into_inner());
        slot.policy = policy;
        slot.overridden = true;
        lock.ready.notify_all();
    }

    /// Clear any manual override for `dev`, reverting new acquisitions to detected/env policy.
    pub fn clear_device_override(&self, dev: DeviceId, fallback_class: DeviceClass) {
        let map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lock) = map.get(&dev) {
            let mut slot = lock.state.lock().unwrap_or_else(|e| e.into_inner());
            slot.overridden = false;
            slot.policy = env_override_policy(fallback_class);
            lock.ready.notify_all();
        }
    }

    fn device_lock(&self, dev: DeviceId, class_hint: Option<DeviceClass>) -> std::sync::Arc<DeviceLock> {
        let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(&dev) {
            return existing.clone();
        }
        let policy = env_override_policy(class_hint.unwrap_or(DeviceClass::Unknown));
        let arc = std::sync::Arc::new(DeviceLock {
            state: Mutex::new(DeviceSlot::new(policy, false)),
            ready: Condvar::new(),
        });
        map.insert(dev, arc.clone());
        arc
    }

    /// Effective policy currently governing `dev`.
    pub fn policy_for(&self, dev: DeviceId, class_hint: Option<DeviceClass>) -> DevicePolicy {
        let lock = self.device_lock(dev, class_hint);
        let mut slot = lock.state.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.overridden {
            if let Some(class) = class_hint {
                slot.policy = env_override_policy(class);
            }
        }
        slot.policy
    }

    /// Acquire a per-device stream permit in fair FIFO order. Blocks only I/O contenders on the
    /// same saturated device (e.g., serializing rotational HDD bulk reads to 1 stream), and is
    /// same-thread reentrant per `(self, DeviceId)` so nested sidecar/segment reads inside a
    /// visitor callback on `max_streams = 1` never self-deadlock.
    pub fn acquire(&self, dev: DeviceId, class_hint: Option<DeviceClass>) -> IoPermit {
        let tid = std::thread::current().id();
        let lock = self.device_lock(dev, class_hint);
        let mut slot = lock.state.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.overridden {
            if let Some(class) = class_hint {
                slot.policy = env_override_policy(class);
            }
        }
        if let Some(depth) = slot.owners.get_mut(&tid) {
            *depth += 1;
            return IoPermit {
                lock: lock.clone(),
                owner: tid,
            };
        }
        let ticket = slot.next_ticket;
        slot.next_ticket = slot.next_ticket.wrapping_add(1);
        slot.waiters.push_back(ticket);

        while slot.active >= slot.policy.max_streams || slot.waiters.front().copied() != Some(ticket) {
            slot = lock.ready.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
        slot.waiters.pop_front();
        slot.serving_ticket = ticket;
        slot.active += 1;
        slot.peak_active = slot.peak_active.max(slot.active);
        slot.owners.insert(tid, 1);
        drop(slot);
        // If additional stream slots remain available on this device, wake the next FIFO waiter.
        lock.ready.notify_all();
        IoPermit { lock, owner: tid }
    }

    /// Acquire admission for `path`'s underlying device and execute `f`.
    pub fn admit_path<T>(&self, path: &Path, f: impl FnOnce() -> T) -> T {
        let (dev, class) = inspect_path_device(path);
        let _permit = self.acquire(dev, Some(class));
        f()
    }

    #[cfg(test)]
    pub(crate) fn held_by_current_thread(&self, dev: DeviceId) -> bool {
        let lock = self.device_lock(dev, None);
        let held = lock
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owners
            .contains_key(&std::thread::current().id());
        held
    }

    pub fn active_streams(&self, dev: DeviceId) -> usize {
        let map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&dev)
            .map(|l| l.state.lock().unwrap_or_else(|e| e.into_inner()).active)
            .unwrap_or(0)
    }

    pub fn peak_active_streams(&self, dev: DeviceId) -> usize {
        let map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&dev)
            .map(|l| l.state.lock().unwrap_or_else(|e| e.into_inner()).peak_active)
            .unwrap_or(0)
    }

    /// Coalesce candidate `(object, offset, len)` reads using `dev`'s geometry, execute each
    /// merged physical read under per-device admission in ascending `(object, offset)` order, and
    /// scatter exact byte slices back in the caller's original request order.
    pub fn execute_coalesced_reads<Obj, F>(
        &self,
        dev: DeviceId,
        class_hint: Option<DeviceClass>,
        requests: &[ReadRequest<Obj>],
        mut read_physical: F,
    ) -> io::Result<Vec<Vec<u8>>>
    where
        Obj: Ord + Clone,
        F: FnMut(&Obj, u64, usize) -> io::Result<Vec<u8>>,
    {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let policy = self.policy_for(dev, class_hint);
        let batches = plan_coalesced_reads(requests, policy.coalesce_gap_bytes, policy.max_coalesced_bytes)?;
        let mut results = vec![Vec::new(); requests.len()];
        for batch in batches {
            if batch.len == 0 {
                continue;
            }
            let buf = {
                let _permit = self.acquire(dev, class_hint);
                read_physical(&batch.object, batch.offset, batch.len)?
            };
            if buf.len() < batch.len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "coalesced physical read returned {} bytes, expected {}",
                        buf.len(),
                        batch.len
                    ),
                ));
            }
            for member in batch.members {
                let end = member.relative_offset + member.len;
                results[member.request_index] = buf[member.relative_offset..end].to_vec();
            }
        }
        Ok(results)
    }
}

/// RAII guard holding one active stream slot on a physical device.
pub struct IoPermit {
    lock: std::sync::Arc<DeviceLock>,
    owner: std::thread::ThreadId,
}

impl Drop for IoPermit {
    fn drop(&mut self) {
        let mut slot = self.lock.state.lock().unwrap_or_else(|e| e.into_inner());
        let release_slot = match slot.owners.get_mut(&self.owner) {
            Some(depth) if *depth > 1 => {
                *depth -= 1;
                false
            }
            Some(_) => {
                slot.owners.remove(&self.owner);
                true
            }
            None => true,
        };
        if release_slot {
            slot.active = slot.active.saturating_sub(1);
            drop(slot);
            self.lock.ready.notify_all();
        }
    }
}

fn env_override_policy(detected: DeviceClass) -> DevicePolicy {
    let class = crate::env::var("CV_IO_DEVICE_CLASS")
        .ok()
        .and_then(|v| DeviceClass::parse(&v))
        .unwrap_or(detected);
    let mut policy = class.default_policy();
    if let Ok(streams) = crate::env::var("CV_IO_MAX_STREAMS") {
        if let Ok(n) = streams.trim().parse::<usize>() {
            policy.max_streams = n.max(1);
        }
    }
    policy.normalized()
}

/// Runtime byte-source policy for `path`:
/// - `Slice` on `wasm32`.
/// - `Buffered` when `mmap` is disabled, on network mounts, or on any mutable external file.
/// - `Mapped` ONLY on local sealed immutable files (`*.pack.zst`, `$CLUSTERVISION_HOME/packs/*`,
///   `$CACHE/clustervision/{packs,segments}/*`, or registered sealed directories).
pub fn byte_source_policy(path: &Path) -> ByteSourcePolicy {
    #[cfg(target_family = "wasm")]
    {
        let _ = path;
        ByteSourcePolicy::Slice
    }
    #[cfg(not(target_family = "wasm"))]
    {
        #[cfg(not(feature = "mmap"))]
        {
            let _ = path;
            ByteSourcePolicy::Buffered
        }
        #[cfg(feature = "mmap")]
        {
            if let Ok(override_val) = crate::env::var("CV_BYTE_SOURCE_POLICY") {
                match override_val.trim().to_ascii_lowercase().as_str() {
                    "buffered" => return ByteSourcePolicy::Buffered,
                    "mapped" if is_sealed_immutable_path(path) => return ByteSourcePolicy::Mapped,
                    _ => {}
                }
            }
            if !is_sealed_immutable_path(path) {
                return ByteSourcePolicy::Buffered;
            }
            let (_dev, class) = inspect_path_device(path);
            if class == DeviceClass::Network {
                return ByteSourcePolicy::Buffered;
            }
            ByteSourcePolicy::Mapped
        }
    }
}

/// Whether `path` names a sealed immutable artifact owned by `cv` (never a mutable external
/// harness transcript).
pub fn is_sealed_immutable_path(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".pack.zst"))
    {
        return true;
    }
    if IoAdmission::global().is_registered_sealed_path(path) {
        return true;
    }
    let env = crate::env::Env::current();
    if let Some(home) = env.cv_home() {
        if path.starts_with(home.join("packs")) || path.starts_with(home.join("segments")) {
            return true;
        }
    }
    if let Some(cache) = env.cv_cache() {
        if path.starts_with(cache.join("packs")) || path.starts_with(cache.join("segments")) {
            return true;
        }
    }
    false
}

/// Inspect `path` (or its parent directory when `path` does not exist yet) to determine its OS
/// `(DeviceId, DeviceClass)`.
pub fn inspect_path_device(path: &Path) -> (DeviceId, DeviceClass) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let probe = if path.exists() {
            path
        } else {
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
        };
        let dev = std::fs::symlink_metadata(probe)
            .map(|m| DeviceId(m.dev()))
            .unwrap_or(DeviceId(0));
        let detected = detect_unix_device_class(probe, dev);
        let effective = crate::env::var("CV_IO_DEVICE_CLASS")
            .ok()
            .and_then(|v| DeviceClass::parse(&v))
            .unwrap_or(detected);
        (dev, effective)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let effective = crate::env::var("CV_IO_DEVICE_CLASS")
            .ok()
            .and_then(|v| DeviceClass::parse(&v))
            .unwrap_or(DeviceClass::Unknown);
        (DeviceId(0), effective)
    }
}

#[cfg(target_os = "macos")]
fn detect_unix_device_class(path: &Path, _dev: DeviceId) -> DeviceClass {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return DeviceClass::Unknown;
    };
    let mut sfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `c_path` is a valid NUL-terminated string and `sfs` points to writable `statfs` storage.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), sfs.as_mut_ptr()) };
    if rc != 0 {
        return DeviceClass::Unknown;
    }
    // SAFETY: `statfs` succeeded and initialized `sfs`.
    let sfs = unsafe { sfs.assume_init() };
    if (sfs.f_flags & (libc::MNT_LOCAL as u32)) == 0 {
        return DeviceClass::Network;
    }
    let fstype_bytes: &[u8] =
        // SAFETY: reinterpreting fixed-size `c_char` array as bytes to read NUL-terminated fs name.
        unsafe { std::slice::from_raw_parts(sfs.f_fstypename.as_ptr().cast::<u8>(), sfs.f_fstypename.len()) };
    let end = fstype_bytes.iter().position(|&b| b == 0).unwrap_or(fstype_bytes.len());
    let fstype = std::str::from_utf8(&fstype_bytes[..end]).unwrap_or("");
    if matches!(fstype, "nfs" | "smbfs" | "afpfs" | "webdav" | "cifs" | "sshfs") {
        return DeviceClass::Network;
    }
    if matches!(fstype, "apfs" | "hfs") {
        return DeviceClass::Nvme;
    }
    DeviceClass::Unknown
}

#[cfg(target_os = "linux")]
fn detect_unix_device_class(path: &Path, dev: DeviceId) -> DeviceClass {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        let mut sfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `c_path` is NUL-terminated and `sfs` is valid writable memory.
        if unsafe { libc::statfs(c_path.as_ptr(), sfs.as_mut_ptr()) } == 0 {
            let sfs = unsafe { sfs.assume_init() };
            let f_type = sfs.f_type as u64;
            // NFS, SMB, CIFS, SMB2, FUSE network mounts.
            if matches!(f_type, 0x6969 | 0x517B | 0xFF53_4D42 | 0xFE53_4D42) {
                return DeviceClass::Network;
            }
        }
    }
    if dev.0 != 0 {
        // SAFETY: pure bit extraction macros for Linux `dev_t`.
        let major = unsafe { libc::major(dev.0 as libc::dev_t) };
        let minor = unsafe { libc::minor(dev.0 as libc::dev_t) };
        let sys_path = format!("/sys/dev/block/{major}:{minor}/queue/rotational");
        if let Ok(rot) = std::fs::read_to_string(sys_path) {
            return match rot.trim() {
                "1" => DeviceClass::RotationalHdd,
                "0" => DeviceClass::Nvme,
                _ => DeviceClass::Unknown,
            };
        }
    }
    DeviceClass::Unknown
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn detect_unix_device_class(_path: &Path, _dev: DeviceId) -> DeviceClass {
    DeviceClass::Unknown
}

/// Candidate byte-range read against a physical object (`pack_id`, file path, or segment id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRequest<Obj> {
    pub object: Obj,
    pub offset: u64,
    pub len: usize,
}

/// Mapping from a slice inside a [`CoalescedBatch`] back to the caller's original request index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoalescedSlice {
    pub request_index: usize,
    pub relative_offset: usize,
    pub len: usize,
}

/// One merged physical `(object, offset, len)` read covering one or more [`ReadRequest`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoalescedBatch<Obj> {
    pub object: Obj,
    pub offset: u64,
    pub len: usize,
    pub members: Vec<CoalescedSlice>,
}

/// Sort `requests` by `(object, offset)` and merge overlapping or nearby ranges separated by at
/// most `max_gap` bytes up to `max_coalesced_bytes` per physical read.
pub fn plan_coalesced_reads<Obj: Ord + Clone>(
    requests: &[ReadRequest<Obj>],
    max_gap: u64,
    max_coalesced_bytes: usize,
) -> io::Result<Vec<CoalescedBatch<Obj>>> {
    let max_coalesced_u64 = (max_coalesced_bytes.max(1)) as u64;
    let mut indexed: Vec<(usize, &ReadRequest<Obj>, u64)> = Vec::with_capacity(requests.len());
    for (idx, req) in requests.iter().enumerate() {
        let end = req
            .offset
            .checked_add(req.len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read request range overflow"))?;
        indexed.push((idx, req, end));
    }
    indexed.sort_by(|a, b| (&a.1.object, a.1.offset, a.2).cmp(&(&b.1.object, b.1.offset, b.2)));

    let mut batches: Vec<CoalescedBatch<Obj>> = Vec::new();
    for (req_idx, req, req_end) in indexed {
        if req.len == 0 {
            continue;
        }
        let can_merge = if let Some(cur) = batches.last() {
            let cur_end = cur.offset + cur.len as u64;
            if cur.object == req.object && req.offset <= cur_end.saturating_add(max_gap) {
                let merged_end = cur_end.max(req_end);
                let merged_len = merged_end - cur.offset;
                merged_len <= max_coalesced_u64 && usize::try_from(merged_len).is_ok()
            } else {
                false
            }
        } else {
            false
        };

        if can_merge {
            let cur = batches.last_mut().unwrap();
            let rel = (req.offset - cur.offset) as usize;
            let new_end = (cur.offset + cur.len as u64).max(req_end);
            cur.len = (new_end - cur.offset) as usize;
            cur.members.push(CoalescedSlice {
                request_index: req_idx,
                relative_offset: rel,
                len: req.len,
            });
        } else {
            batches.push(CoalescedBatch {
                object: req.object.clone(),
                offset: req.offset,
                len: req.len,
                members: vec![CoalescedSlice {
                    request_index: req_idx,
                    relative_offset: 0,
                    len: req.len,
                }],
            });
        }
    }
    Ok(batches)
}

/// Object type reported by [`read_dir_bulk`] without following symlinks (`lstat` / `FSOPT_NOFOLLOW`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkFileType {
    File,
    Dir,
    Symlink,
    Other,
}

/// Directory entry attributes returned in bulk by [`read_dir_bulk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryAttr {
    pub name: OsString,
    pub file_type: BulkFileType,
    pub ino: u64,
    pub dev: u64,
    pub mtime_ns: u64,
    pub ctime_ns: i64,
    pub size: u64,
    /// Per-entry errno when `getattrlistbulk` or `symlink_metadata` encountered an entry-local error.
    pub entry_error: Option<i32>,
}

/// Enumerate directory entries and their `lstat`-equivalent metadata in bulk:
/// uses `getattrlistbulk(2)` on macOS (~1 syscall per ~hundreds of entries instead of `N` `lstat`s)
/// and falls back to `std::fs::read_dir` + `symlink_metadata` on Linux/Windows.
pub fn read_dir_bulk(dir: &Path) -> io::Result<Vec<DirEntryAttr>> {
    #[cfg(target_os = "macos")]
    {
        macos_bulk::read_dir_bulk_macos(dir)
    }
    #[cfg(all(not(target_os = "macos"), not(target_family = "wasm")))]
    {
        read_dir_bulk_portable(dir)
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory enumeration is unavailable on wasm32",
        ))
    }
}

#[cfg(not(target_family = "wasm"))]
#[allow(dead_code)]
fn read_dir_bulk_portable(dir: &Path) -> io::Result<Vec<DirEntryAttr>> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let entry = item?;
        let name = entry.file_name();
        match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => {
                let ft = meta.file_type();
                let file_type = if ft.is_symlink() {
                    BulkFileType::Symlink
                } else if ft.is_file() {
                    BulkFileType::File
                } else if ft.is_dir() {
                    BulkFileType::Dir
                } else {
                    BulkFileType::Other
                };
                #[cfg(unix)]
                let (ino, dev, mtime_ns, ctime_ns, size) = {
                    use std::os::unix::fs::MetadataExt;
                    let m_sec = meta.mtime().max(0) as u64;
                    let m_nsec = meta.mtime_nsec().clamp(0, 999_999_999) as u64;
                    let mtime_ns = m_sec.saturating_mul(1_000_000_000).saturating_add(m_nsec);
                    let ctime_ns = (meta.ctime() as i128)
                        .saturating_mul(1_000_000_000)
                        .saturating_add(meta.ctime_nsec() as i128)
                        .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                    let size = if file_type == BulkFileType::File { meta.len() } else { 0 };
                    (meta.ino(), meta.dev(), mtime_ns, ctime_ns, size)
                };
                #[cfg(not(unix))]
                let (ino, dev, mtime_ns, ctime_ns, size) = {
                    let mtime_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .and_then(|d| u64::try_from(d.as_nanos()).ok())
                        .unwrap_or(0);
                    let size = if file_type == BulkFileType::File { meta.len() } else { 0 };
                    (0u64, 0u64, mtime_ns, 0i64, size)
                };
                entries.push(DirEntryAttr {
                    name,
                    file_type,
                    ino,
                    dev,
                    mtime_ns,
                    ctime_ns,
                    size,
                    entry_error: None,
                });
            }
            Err(err) => {
                entries.push(DirEntryAttr {
                    name,
                    file_type: BulkFileType::Other,
                    ino: 0,
                    dev: 0,
                    mtime_ns: 0,
                    ctime_ns: 0,
                    size: 0,
                    entry_error: err.raw_os_error().or(Some(1)),
                });
            }
        }
    }
    Ok(entries)
}

#[cfg(target_os = "macos")]
mod macos_bulk {
    use super::{BulkFileType, DirEntryAttr};
    use std::ffi::{CString, OsStr, OsString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const FSOPT_NOFOLLOW: u64 = 0x0000_0001;

    const ATTR_CMN_NAME: u32 = 0x0000_0001;
    const ATTR_CMN_DEVID: u32 = 0x0000_0002;
    const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
    const ATTR_CMN_CHGTIME: u32 = 0x0000_0800;
    const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

    const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

    // XNU `enum vtype` (`<sys/vnode.h>`).
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AttributeSet {
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AttrReference {
        attr_dataoffset: i32,
        attr_length: u32,
    }

    unsafe extern "C" {
        fn getattrlistbulk(
            dirfd: libc::c_int,
            attr_list: *mut AttrList,
            attr_buf: *mut libc::c_void,
            attr_buf_size: libc::size_t,
            options: u64,
        ) -> libc::c_int;
    }

    struct FdGuard(libc::c_int);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            // SAFETY: closes our privately opened directory descriptor once.
            unsafe {
                let _ = libc::close(self.0);
            }
        }
    }

    fn read_unaligned<T: Copy>(record: &[u8], cursor: &mut usize) -> io::Result<T> {
        let size = std::mem::size_of::<T>();
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "getattrlistbulk field offset overflow"))?;
        let bytes = record.get(*cursor..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "getattrlistbulk record truncated before expected attribute",
            )
        })?;
        *cursor = end;
        // SAFETY: `bytes` has exact length `size_of::<T>()`, `T: Copy`, and `read_unaligned` handles 4-byte packing.
        Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
    }

    pub(super) fn read_dir_bulk_macos(dir: &Path) -> io::Result<Vec<DirEntryAttr>> {
        let c_path =
            CString::new(dir.as_os_str().as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `c_path` is a valid NUL-terminated path; `O_DIRECTORY | O_CLOEXEC` prevents non-dir opens & fd leaks.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = FdGuard(fd);

        let mut attr_list = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_ERROR
                | ATTR_CMN_NAME
                | ATTR_CMN_DEVID
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_MODTIME
                | ATTR_CMN_CHGTIME
                | ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_DATALENGTH,
            forkattr: 0,
        };

        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();

        loop {
            // SAFETY: `fd` is an open directory fd, `attr_list` is valid `AttrList`, `buf` is 64 KiB writable storage.
            let count = unsafe {
                getattrlistbulk(
                    fd,
                    &mut attr_list,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    FSOPT_NOFOLLOW,
                )
            };
            if count < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Fall back cleanly if a synthetic/FUSE filesystem does not implement `getattrlistbulk`.
                if matches!(
                    err.raw_os_error(),
                    Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL)
                ) {
                    return super::read_dir_bulk_portable(dir);
                }
                return Err(err);
            }
            if count == 0 {
                break;
            }

            let mut entry_offset = 0usize;
            for _ in 0..(count as usize) {
                let len_slice = buf.get(entry_offset..entry_offset + 4).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "getattrlistbulk entry header out of buffer bounds",
                    )
                })?;
                let rec_len = u32::from_ne_bytes(len_slice.try_into().unwrap()) as usize;
                if rec_len < 4 + std::mem::size_of::<AttributeSet>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("getattrlistbulk entry record length {rec_len} too small"),
                    ));
                }
                let next_entry = entry_offset.checked_add(rec_len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "getattrlistbulk entry length overflow")
                })?;
                let record = buf.get(entry_offset..next_entry).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "getattrlistbulk entry extends past buffer")
                })?;
                entry_offset = next_entry;

                if let Some(entry) = parse_bulk_record(record)? {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }

    pub(super) fn parse_bulk_record(record: &[u8]) -> io::Result<Option<DirEntryAttr>> {
        let mut cursor = 4usize;
        let returned: AttributeSet = read_unaligned(record, &mut cursor)?;

        let entry_err = if (returned.commonattr & ATTR_CMN_ERROR) != 0 {
            let code: u32 = read_unaligned(record, &mut cursor)?;
            (code != 0).then_some(code as i32)
        } else {
            None
        };

        let mut name_os = OsString::new();
        if (returned.commonattr & ATTR_CMN_NAME) != 0 {
            let name_field_pos = cursor;
            let name_ref: AttrReference = read_unaligned(record, &mut cursor)?;
            let data_start = isize::try_from(name_field_pos)
                .ok()
                .and_then(|base| base.checked_add(name_ref.attr_dataoffset as isize))
                .and_then(|pos| usize::try_from(pos).ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid ATTR_CMN_NAME dataoffset"))?;
            let data_end = data_start
                .checked_add(name_ref.attr_length as usize)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ATTR_CMN_NAME length overflow"))?;
            let raw_name = record
                .get(data_start..data_end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ATTR_CMN_NAME out of record bounds"))?;
            let nul = raw_name.iter().position(|&b| b == 0).unwrap_or(raw_name.len());
            name_os = OsStr::from_bytes(&raw_name[..nul]).to_os_string();
        }

        if name_os == "." || name_os == ".." {
            return Ok(None);
        }

        if let Some(code) = entry_err {
            return Ok(Some(DirEntryAttr {
                name: name_os,
                file_type: BulkFileType::Other,
                ino: 0,
                dev: 0,
                mtime_ns: 0,
                ctime_ns: 0,
                size: 0,
                entry_error: Some(code),
            }));
        }

        if name_os.is_empty() {
            return Ok(None);
        }

        let dev = if (returned.commonattr & ATTR_CMN_DEVID) != 0 {
            let raw_dev: libc::dev_t = read_unaligned(record, &mut cursor)?;
            raw_dev as u64
        } else {
            0
        };

        let file_type = if (returned.commonattr & ATTR_CMN_OBJTYPE) != 0 {
            let vtype: u32 = read_unaligned(record, &mut cursor)?;
            match vtype {
                VREG => BulkFileType::File,
                VDIR => BulkFileType::Dir,
                VLNK => BulkFileType::Symlink,
                _ => BulkFileType::Other,
            }
        } else {
            BulkFileType::Other
        };

        let mtime_ns = if (returned.commonattr & ATTR_CMN_MODTIME) != 0 {
            let ts: libc::timespec = read_unaligned(record, &mut cursor)?;
            let sec = ts.tv_sec.max(0) as u64;
            let nsec = ts.tv_nsec.clamp(0, 999_999_999) as u64;
            sec.saturating_mul(1_000_000_000).saturating_add(nsec)
        } else {
            0
        };

        let ctime_ns = if (returned.commonattr & ATTR_CMN_CHGTIME) != 0 {
            let ts: libc::timespec = read_unaligned(record, &mut cursor)?;
            (ts.tv_sec as i128)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as i128)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64
        } else {
            0
        };

        let ino = if (returned.commonattr & ATTR_CMN_FILEID) != 0 {
            read_unaligned::<u64>(record, &mut cursor)?
        } else {
            0
        };

        let size = if (returned.fileattr & ATTR_FILE_DATALENGTH) != 0 {
            let len: libc::off_t = read_unaligned(record, &mut cursor)?;
            if file_type == BulkFileType::File {
                len.max(0) as u64
            } else {
                0
            }
        } else {
            0
        };

        Ok(Some(DirEntryAttr {
            name: name_os,
            file_type,
            ino,
            dev,
            mtime_ns,
            ctime_ns,
            size,
            entry_error: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn byte_source_policy_keeps_external_transcripts_buffered_and_local_packs_mapped() {
        let world = crate::test_world("io-admission-policy");
        let external_transcript = world.home.join(".claude/projects/p/session.jsonl");
        std::fs::create_dir_all(external_transcript.parent().unwrap()).unwrap();
        std::fs::write(&external_transcript, "{}\n").unwrap();

        // External mutable transcript MUST always use Buffered so concurrent truncation never SIGBUSes.
        assert_eq!(byte_source_policy(&external_transcript), ByteSourcePolicy::Buffered);

        let sealed_pack = crate::env::Env::current()
            .cv_home()
            .unwrap()
            .join("packs/pack-001.pack.zst");
        std::fs::create_dir_all(sealed_pack.parent().unwrap()).unwrap();
        std::fs::write(&sealed_pack, b"sealed").unwrap();

        #[cfg(all(feature = "mmap", not(target_family = "wasm")))]
        assert_eq!(byte_source_policy(&sealed_pack), ByteSourcePolicy::Mapped);
        #[cfg(all(not(feature = "mmap"), not(target_family = "wasm")))]
        assert_eq!(byte_source_policy(&sealed_pack), ByteSourcePolicy::Buffered);
    }

    #[test]
    fn hdd_admission_enforces_single_sequential_stream_while_nvme_allows_parallel_streams() {
        let admission = Arc::new(IoAdmission::new());
        let hdd_dev = DeviceId(0xDEAD_0001);
        let nvme_dev = DeviceId(0xFA57_0002);
        admission.set_device_override(hdd_dev, DeviceClass::RotationalHdd.default_policy());
        admission.set_device_override(
            nvme_dev,
            DevicePolicy {
                class: DeviceClass::Nvme,
                max_streams: 4,
                coalesce_gap_bytes: 16 * 1024,
                max_coalesced_bytes: 4 * 1024 * 1024,
            },
        );

        // 8 worker threads contend simultaneously on the rotational HDD device: peak active MUST be 1.
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let adm = admission.clone();
                let bar = barrier.clone();
                s.spawn(move || {
                    bar.wait();
                    let _permit = adm.acquire(hdd_dev, None);
                    assert_eq!(
                        adm.active_streams(hdd_dev),
                        1,
                        "rotational HDD exceeded 1 active stream"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(4));
                });
            }
        });
        assert_eq!(admission.peak_active_streams(hdd_dev), 1);
        assert_eq!(admission.active_streams(hdd_dev), 0);

        // 4 worker threads on NVMe reach parallel active streams up to `max_streams = 4`.
        let nvme_barrier_enter = Arc::new(Barrier::new(4));
        let nvme_barrier_hold = Arc::new(Barrier::new(4));
        std::thread::scope(|s| {
            for _ in 0..4 {
                let adm = admission.clone();
                let enter = nvme_barrier_enter.clone();
                let hold = nvme_barrier_hold.clone();
                s.spawn(move || {
                    enter.wait();
                    let _permit = adm.acquire(nvme_dev, None);
                    hold.wait();
                });
            }
        });
        assert_eq!(admission.peak_active_streams(nvme_dev), 4);
        assert_eq!(admission.active_streams(nvme_dev), 0);
    }

    #[test]
    fn coalescer_merges_adjacent_and_overlapping_reads_in_offset_order_and_restores_caller_order() {
        let mut object_a = Vec::with_capacity(2048);
        for i in 0..2048u32 {
            object_a.push(((i * 13 + 7) & 0xFF) as u8);
        }
        let object_b: Vec<u8> = (0..512u32).map(|i| ((i * 29 + 3) & 0xFF) as u8).collect();

        // Out-of-order, overlapping, gap-within-threshold, and max-size-exceeding requests.
        let requests = vec![
            ReadRequest {
                object: "obj-b",
                offset: 100,
                len: 50,
            },
            ReadRequest {
                object: "obj-a",
                offset: 300,
                len: 80,
            },
            ReadRequest {
                object: "obj-a",
                offset: 100,
                len: 120, // 100..220; gap to 300 is 80 <= 128 => merges with 300..380!
            },
            ReadRequest {
                object: "obj-a",
                offset: 150,
                len: 40, // 150..190 overlaps inside 100..380!
            },
            ReadRequest {
                object: "obj-a",
                offset: 1200,
                len: 100, // gap from 380 is 820 > 128 => separate physical read!
            },
        ];

        let admission = IoAdmission::new();
        let dev = DeviceId(42);
        admission.set_device_override(
            dev,
            DevicePolicy {
                class: DeviceClass::RotationalHdd,
                max_streams: 1,
                coalesce_gap_bytes: 128,
                max_coalesced_bytes: 1024,
            },
        );

        let mut physical_calls = Vec::new();
        let slices = admission
            .execute_coalesced_reads(dev, None, &requests, |obj, off, len| {
                physical_calls.push((*obj, off, len));
                let backing = if *obj == "obj-a" { &object_a } else { &object_b };
                Ok(backing[off as usize..off as usize + len].to_vec())
            })
            .unwrap();

        // 5 logical requests coalesced into 3 sorted physical reads:
        // ("obj-a", 100, 280), ("obj-a", 1200, 100), ("obj-b", 100, 50).
        assert_eq!(
            physical_calls,
            vec![("obj-a", 100, 280), ("obj-a", 1200, 100), ("obj-b", 100, 50)]
        );
        assert_eq!(slices[0], &object_b[100..150]);
        assert_eq!(slices[1], &object_a[300..380]);
        assert_eq!(slices[2], &object_a[100..220]);
        assert_eq!(slices[3], &object_a[150..190]);
        assert_eq!(slices[4], &object_a[1200..1300]);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn read_dir_bulk_matches_symlink_metadata_across_symlinks_unicode_and_large_directories() {
        let world = cv_testkit::World::new("read-dir-bulk");
        let dir = world.base.join("bulk-dir");
        std::fs::create_dir_all(&dir).unwrap();

        // Subdirectory, regular files (including Unicode name), valid symlink, and dangling symlink.
        std::fs::create_dir(dir.join("nested-dir")).unwrap();
        std::fs::write(dir.join("alpha.jsonl"), vec![b'x'; 1234]).unwrap();
        std::fs::write(dir.join("unicode-λ-雪-🦀.json"), b"{\"ok\":true}").unwrap();
        for idx in 0..550 {
            std::fs::write(dir.join(format!("item-{idx:04}.txt")), format!("payload-{idx}")).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("alpha.jsonl"), dir.join("valid-link")).unwrap();
            std::os::unix::fs::symlink(dir.join("missing-target"), dir.join("dangling-link")).unwrap();
        }

        let mut bulk = read_dir_bulk(&dir).unwrap();
        let mut portable = read_dir_bulk_portable(&dir).unwrap();
        bulk.sort_by(|a, b| a.name.cmp(&b.name));
        portable.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(bulk.len(), portable.len());
        assert!(bulk.len() >= 553);
        for (actual, expected) in bulk.iter().zip(portable.iter()) {
            assert_eq!(actual.name, expected.name);
            assert_eq!(
                actual.file_type, expected.file_type,
                "type mismatch for {:?}",
                actual.name
            );
            assert_eq!(actual.ino, expected.ino, "ino mismatch for {:?}", actual.name);
            assert_eq!(actual.dev, expected.dev, "dev mismatch for {:?}", actual.name);
            assert_eq!(
                actual.mtime_ns, expected.mtime_ns,
                "mtime mismatch for {:?}",
                actual.name
            );
            assert_eq!(
                actual.ctime_ns, expected.ctime_ns,
                "ctime mismatch for {:?}",
                actual.name
            );
            assert_eq!(actual.size, expected.size, "size mismatch for {:?}", actual.name);
            assert_eq!(actual.entry_error, None);
        }

        #[cfg(unix)]
        {
            let dangling = bulk.iter().find(|e| e.name == "dangling-link").unwrap();
            assert_eq!(dangling.file_type, BulkFileType::Symlink);
            assert_eq!(dangling.size, 0);
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn concurrent_external_transcript_truncation_uses_buffered_policy_and_never_mmaps() {
        let world = crate::test_world("io-admission-concurrent-truncate");
        let transcript = world.home.join(".claude/projects/p/mutable.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();

        let line = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}\n", "x".repeat(4096));
        std::fs::write(&transcript, line.repeat(256)).unwrap();

        #[cfg(feature = "mmap")]
        crate::harness::store::jsonl::MAPPED_PEAK.with(|p| p.set(0));

        let read = crate::harness::store::jsonl::Read::open(&transcript).unwrap();
        let truncator = std::fs::OpenOptions::new().write(true).open(&transcript).unwrap();

        let mut seen = 0usize;
        let res = read.lines(0, |_| {
            seen += 1;
            if seen == 2 {
                // Concurrent external truncation mid-pass: would SIGBUS a live mmap!
                truncator.set_len(0).unwrap();
            }
            Ok(crate::stream::Flow::Continue)
        });
        // Must return a clean error on truncated read, never SIGBUS or map external pages.
        assert!(res.is_err());
        #[cfg(feature = "mmap")]
        assert_eq!(
            crate::harness::store::jsonl::MAPPED_PEAK.with(|p| p.get()),
            0,
            "mutable external transcript must never be memory-mapped"
        );
    }

    #[test]
    fn reentrant_same_thread_acquire_never_deadlocks_on_single_stream_device() {
        let admission_a = IoAdmission::new();
        let admission_b = IoAdmission::new();
        let dev = DeviceId(0xBEEF_0001);
        admission_a.set_device_override(dev, DeviceClass::RotationalHdd.default_policy());
        admission_b.set_device_override(dev, DeviceClass::RotationalHdd.default_policy());

        // Nested LIFO acquisition on `max_streams = 1`: active stream count stays 1, never deadlocks.
        {
            let p1 = admission_a.acquire(dev, None);
            assert_eq!(admission_a.active_streams(dev), 1);
            let p2 = admission_a.acquire(dev, None);
            assert_eq!(admission_a.active_streams(dev), 1);
            let p3 = admission_a.acquire(dev, None);
            assert_eq!(admission_a.active_streams(dev), 1);

            // Independent `IoAdmission` instance has its own `(self, DeviceId)` reentrancy scope.
            let pb = admission_b.acquire(dev, None);
            assert_eq!(admission_b.active_streams(dev), 1);
            drop(pb);
            assert_eq!(admission_b.active_streams(dev), 0);

            drop(p3);
            assert_eq!(admission_a.active_streams(dev), 1);
            drop(p2);
            assert_eq!(admission_a.active_streams(dev), 1);
            drop(p1);
            assert_eq!(admission_a.active_streams(dev), 0);
        }

        // Out-of-order drop (`drop(outer)` before `drop(inner)`) also keeps the physical slot held
        // until the last permit on that thread drops.
        let outer = admission_a.acquire(dev, None);
        let inner = admission_a.acquire(dev, None);
        drop(outer);
        assert_eq!(admission_a.active_streams(dev), 1);
        drop(inner);
        assert_eq!(admission_a.active_streams(dev), 0);
        assert_eq!(admission_a.peak_active_streams(dev), 1);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn jsonl_lines_allow_parallel_callbacks_and_nested_sidecar_reads_on_hdd() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let world = crate::test_world("io-admission-jsonl-hdd");
        let dir = world.home.join(".claude/projects/hdd-test");
        std::fs::create_dir_all(&dir).unwrap();
        let primary = dir.join("primary.jsonl");
        let sidecar = dir.join("sidecar.jsonl");
        std::fs::write(&primary, "{\"k\":1}\n{\"k\":2}\n{\"k\":3}\n").unwrap();
        std::fs::write(&sidecar, "{\"side\":true}\n").unwrap();

        let (dev, orig_class) = inspect_path_device(&primary);
        struct ResetOverride(DeviceId, DeviceClass);
        impl Drop for ResetOverride {
            fn drop(&mut self) {
                IoAdmission::global().clear_device_override(self.0, self.1);
            }
        }
        IoAdmission::global().set_device_override(dev, DeviceClass::RotationalHdd.default_policy());
        let _guard = ResetOverride(dev, orig_class);

        let barrier = Arc::new(Barrier::new(8));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..8 {
                let bar = barrier.clone();
                let active = in_flight.clone();
                let peak = max_in_flight.clone();
                let primary_path = primary.clone();
                let sidecar_path = sidecar.clone();
                s.spawn(move || {
                    bar.wait();
                    let reader = crate::harness::store::jsonl::Read::open(&primary_path).unwrap();
                    let mut line_idx = 0usize;
                    reader
                        .lines(0, |_| {
                            let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(cur, Ordering::SeqCst);
                            if line_idx == 0 {
                                // Nested sidecar JSONL read on the same `max_streams = 1` device!
                                let nested = crate::harness::store::jsonl::Read::open(&sidecar_path).unwrap();
                                let mut nested_count = 0usize;
                                nested
                                    .lines(0, |_| {
                                        nested_count += 1;
                                        Ok(crate::stream::Flow::Continue)
                                    })
                                    .unwrap();
                                assert_eq!(nested_count, 1);
                                std::thread::sleep(std::time::Duration::from_millis(3));
                            }
                            line_idx += 1;
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok(crate::stream::Flow::Continue)
                        })
                        .unwrap();
                    assert_eq!(line_idx, 3);
                });
            }
        });

        assert!(
            max_in_flight.load(Ordering::SeqCst) > 1,
            "device admission must serialize physical reads, not CPU-side visitor callbacks"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bulk_record_preserves_nameless_per_entry_error() {
        // Synthesize a raw `getattrlistbulk` record where `ATTR_CMN_ERROR` (`0x2000_0000`) is set
        // to `EACCES` and `ATTR_CMN_NAME` (`0x0000_0001`) was NOT populated by the kernel.
        let mut record = Vec::new();
        let rec_len: u32 = 4 + 20 + 4;
        record.extend_from_slice(&rec_len.to_ne_bytes());
        record.extend_from_slice(&0x2000_0000u32.to_ne_bytes()); // commonattr = ATTR_CMN_ERROR only
        record.extend_from_slice(&0u32.to_ne_bytes()); // volattr
        record.extend_from_slice(&0u32.to_ne_bytes()); // dirattr
        record.extend_from_slice(&0u32.to_ne_bytes()); // fileattr
        record.extend_from_slice(&0u32.to_ne_bytes()); // forkattr
        record.extend_from_slice(&(libc::EACCES as u32).to_ne_bytes()); // error code

        let parsed = macos_bulk::parse_bulk_record(&record)
            .unwrap()
            .expect("per-entry ATTR_CMN_ERROR without ATTR_CMN_NAME must not be dropped");
        assert!(parsed.name.is_empty());
        assert_eq!(parsed.entry_error, Some(libc::EACCES));
    }
}
