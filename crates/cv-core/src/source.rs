//! Primary identity and auxiliary source dependencies, with append-safe comparisons.
//!
//! A consumer captures a [`Baseline`] before it reads: the primary file's [`FileId`] and a
//! [`Stamp`] of every auxiliary path, present or absent, never a lossy aggregate. It publishes
//! the baseline with what it recorded as one [`Record`] (`version`, `baseline`, `payload`, tagged
//! `T::TAG`), in the same transaction as any output of its own; loading decodes the record
//! (this [`VERSION`], this consumer) and [`Record::admit`] is the one freshness check. Each
//! consumer owns its records: refreshing counts cannot make an older search index fresh, and a
//! missing or older record rereads once.
//!
//! A session in a shared SQLite store adds its [`Rows`] certificate: a write to the store moves
//! the database's identity for all its sessions, and a session whose certificate holds is admitted
//! without rereading ([`Record::admit_for`]).
//!
//! These are metadata guards, not content verification. Append-safe comparisons deliberately
//! exclude the primary identity: immutable-prefix contracts and sampled resume marks govern reuse
//! during growth. The before-pass inventory covers adapter-declared paths; consumers additionally
//! capture paths before dynamic reads. Complete read sets (even empty) stay distinct from
//! unrecorded ones.

use crate::ir::{Harness, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A native SQLite file by its name: what [`crate::harness::Adapter::sqlite_source`] answers
/// unless the harness permits a database under an arbitrary filename.
pub(crate) fn has_sqlite_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb"))
}

/// Whether `path` starts with the SQLite header: `None` when it cannot be read.
pub(crate) fn has_sqlite_header(path: &Path) -> Option<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0; 16];
    match file.read_exact(&mut header) {
        Ok(()) => Some(&header == b"SQLite format 3\0"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Some(false),
        Err(_) => None,
    }
}

pub(crate) fn sqlite_sidecars(path: &Path) -> [PathBuf; 2] {
    ["-wal", "-journal"].map(|suffix| {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        PathBuf::from(sidecar)
    })
}

// Preserve the path spelling: canonicalize cannot represent missing files and would hide a
// symlink's later retargeting. Relative reads belong to the cwd of the pass that consulted them.
fn absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// One file as `stat` saw it, the one identity every guard compares. It has exactly two
/// comparisons: `==` (strict: the file is unchanged) and [`FileId::admits`] (append: the same
/// file, grown or not, with its captured prefix intact). Device, inode and ctime are 0 off Unix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub len: u64,
    pub mtime_ns: u64,
    pub dev: u64,
    pub ino: u64,
    pub ctime_ns: i64,
}

impl FileId {
    /// The file at `path` now; `None` when it is absent or unreadable.
    pub fn at(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }

    pub fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let (dev, ino, ctime_ns) = (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
        );
        #[cfg(not(unix))]
        let (dev, ino, ctime_ns) = (0, 0, 0);
        Some(Self {
            len: metadata.len(),
            mtime_ns: modified.as_nanos() as u64,
            dev,
            ino,
            ctime_ns,
        })
    }

    /// The five words, little-endian: the bytes a stored identity is.
    pub fn to_bytes(self) -> [u8; 40] {
        let mut out = [0; 40];
        for (i, word) in [self.len, self.mtime_ns, self.dev, self.ino, self.ctime_ns as u64]
            .into_iter()
            .enumerate()
        {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// [`Self::to_bytes`] reversed; `None` for any other length.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 40] = bytes.try_into().ok()?;
        let word = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Some(Self {
            len: word(0),
            mtime_ns: word(1),
            dev: word(2),
            ino: word(3),
            ctime_ns: word(4) as i64,
        })
    }

    /// Append admission for an append-only source captured as `self` with sample `prefix`:
    /// `later` is this file unchanged, or the same file (device and inode) at least as long whose
    /// bytes under the captured length still sample as they did ([`Prefix::holds`]). No timing:
    /// a writer mid-`write` (new timestamps, old length) is admitted by content, and a same-length
    /// rewrite of the sampled bytes is rejected. An edit outside the samples is not detected
    /// (the limit [`crate::resume`] documents).
    pub fn admits(&self, prefix: &Prefix, later: &FileId, path: &Path) -> bool {
        self == later || (later.dev, later.ino) == (self.dev, self.ino) && later.len >= self.len && prefix.holds(path)
    }
}

/// How many bytes a [`Prefix`] samples at the head of the file and again just before its end.
pub(crate) const SAMPLE_SPAN: u64 = 4096;

/// The bytes `0..len` of one file (device, inode), sampled: FNV-1a over the first
/// [`SAMPLE_SPAN`] bytes, the [`SAMPLE_SPAN`] bytes before `len` (overlapping on a short file)
/// and `len` itself. A sample, not a digest of the whole prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Prefix {
    pub len: u64,
    pub hash: u64,
    pub dev: u64,
    pub ino: u64,
}

impl Prefix {
    /// The sample of `file`'s first `len` bytes; `None` when it is shorter or unreadable. Reads
    /// positioned, so the handle's own position never moves.
    pub fn of(file: &std::fs::File, len: u64) -> Option<Self> {
        use crate::harness::store::jsonl::ranges::read_exact_at;
        let id = FileId::of(&file.metadata().ok()?)?;
        if id.len < len {
            return None;
        }
        let mut head = vec![0u8; len.min(SAMPLE_SPAN) as usize];
        read_exact_at(file, &mut head, 0).ok()?;
        let from = len.saturating_sub(SAMPLE_SPAN);
        let mut tail = vec![0u8; (len - from) as usize];
        read_exact_at(file, &mut tail, from).ok()?;
        let hash = fnv1a(fnv1a(fnv1a(0xcbf2_9ce4_8422_2325, &head), &tail), &len.to_le_bytes());
        Some(Self {
            len,
            hash,
            dev: id.dev,
            ino: id.ino,
        })
    }

    /// The file `path` names now is this one, at least `len` long, sampling as it did.
    pub fn holds(&self, path: &Path) -> bool {
        std::fs::File::open(path).is_ok_and(|file| Self::of(&file, self.len) == Some(*self))
    }
}

/// One recorded path: its identity, or `None` when absent. A pass's append-only primary also
/// keeps its [`Prefix`] in memory, which only that pass's growth admission reads; equality,
/// hashing and the serialized record are path and identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    state: Option<FileId>,
    #[serde(skip)]
    prefix: Option<Prefix>,
    /// An append-only file whose head the reader consulted ([`ReadSet::record_append_only`]):
    /// its growth keeps the entry fresh while this sample still holds ([`FileId::admits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grows: Option<Prefix>,
    /// A declared [`Dependencies::resumable`] file: its growth (this sample still holding)
    /// leaves a mark resumable ([`Baseline::dependencies_fresh`]) but never admits a record as it
    /// stands ([`Baseline::fresh`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resumes: Option<Prefix>,
}

impl Entry {
    /// Still as recorded, or (an append-only entry) only grown.
    fn holds(&self, probe: &mut Probe) -> bool {
        let now = probe.entry(&self.path);
        now.as_ref() == Some(self)
            || self
                .grows
                .zip(self.state)
                .zip(now.and_then(|now| now.state))
                .is_some_and(|((sample, then), now)| then.admits(&sample, &now, &self.path))
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.state == other.state
    }
}
impl Eq for Entry {}
impl std::hash::Hash for Entry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.state.hash(state);
    }
}

/// The one version of everything cv stores about a session: the [`Record`] envelope, the
/// catalog (its schema stamp and discovery interpretation), the affordance view, the usage
/// projection rows, the semantic store's header, the full-text index, and every resume mark with
/// the codec checkpoint inside it. A change to what any of them means, or to a rebuildable
/// table's shape, bumps it, and every consumer rereads each session once (0.18 is 9: rc.3 wrote
/// 8, whose full-text index held only the first MiB of a persisted tool output; rc.2 wrote 7,
/// whose Cursor listings counted dropped bubbles; rc.1 wrote 6, whose discovery counted tool
/// results as turns; 0.17 wrote 4). Two byte layouts are versioned apart because they are not
/// rebuilt from transcripts: the task log (durable user data) and a watch cursor (a follower's
/// position).
pub const VERSION: u32 = 9;

/// Version identity for non-Record read state; keep the same interpretation authority.
pub(crate) fn consumer_version(consumer: impl std::fmt::Display) -> String {
    format!("v{VERSION}/{consumer}")
}

/// Exact auxiliary records, not a content digest: the inventory a pass captured before reading
/// (`entries`) and, once the pass completed, every path it read (`reads`; `None` is a before-pass
/// inventory, not a complete read set). Each is the JSON its one serializer wrote, kept as text:
/// loaded baselines hold two strings instead of every path for counts, events and search, and two
/// stamps of one inventory are equal as text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    entries: Box<serde_json::value::RawValue>,
    #[serde(default)]
    reads: Option<Box<serde_json::value::RawValue>>,
}

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        self.entries.get() == other.entries.get()
            && self.reads.as_ref().map(|r| r.get()) == other.reads.as_ref().map(|r| r.get())
    }
}
impl Eq for Stamp {}

impl Stamp {
    fn new(entries: &[Entry], reads: Option<&[Entry]>) -> Option<Self> {
        // A boxed str: retained across publication checks without the serializer's spare capacity.
        let raw = |entries: &[Entry]| serde_json::value::to_raw_value(entries).ok();
        Some(Self {
            entries: raw(entries)?,
            reads: match reads {
                Some(reads) => Some(raw(reads)?),
                None => None,
            },
        })
    }

    fn entries(&self) -> Option<Vec<Entry>> {
        serde_json::from_str(self.entries.get()).ok()
    }

    /// The recorded reads: `Some(None)` for a before-pass inventory, `None` when malformed.
    fn reads(&self) -> Option<Option<Vec<Entry>>> {
        match &self.reads {
            None => Some(None),
            Some(reads) => serde_json::from_str(reads.get()).ok().map(Some),
        }
    }

    /// The same inventory: equal as text, or (one side sampling a resumable file the other did
    /// not, as an inventory taken before it was declared resumable) as paths and identities.
    fn same_entries(&self, current: &Stamp) -> bool {
        self.entries.get() == current.entries.get()
            || self
                .entries()
                .zip(current.entries())
                .is_some_and(|(old, now)| old == now)
    }

    /// [`Self::matches`], except that a declared [`Dependencies::resumable`] file may have grown
    /// since this inventory sampled it: a resumed read decides from its mark what the growth
    /// revises.
    fn matches_grown(&self, current: &Stamp, probe: &mut Probe) -> bool {
        if self.entries.get() != current.entries.get() {
            let (Some(old), Some(now)) = (self.entries(), current.entries()) else {
                return false;
            };
            let grown = |old: &Entry, now: &Entry| {
                old.path == now.path
                    && (old == now
                        || old
                            .resumes
                            .zip(old.state)
                            .zip(now.state)
                            .is_some_and(|((sample, then), now)| then.admits(&sample, &now, &old.path)))
            };
            if old.len() != now.len() || !old.iter().zip(&now).all(|(old, now)| grown(old, now)) {
                return false;
            }
        }
        matches!(self.reads(), Some(Some(reads)) if reads.iter().all(|entry| entry.holds(probe)))
    }

    /// [`Self::matches`] with the files in `store` left out of both sides: a shared database's
    /// own files, which every write to any of its sessions moves.
    fn matches_except(&self, current: &Stamp, probe: &mut Probe, store: &[PathBuf]) -> bool {
        let kept =
            |entries: Vec<Entry>| -> Vec<Entry> { entries.into_iter().filter(|e| !store.contains(&e.path)).collect() };
        let (Some(old), Some(now)) = (self.entries(), current.entries()) else {
            return false;
        };
        let Some(Some(reads)) = self.reads() else {
            return false;
        };
        kept(old) == kept(now) && kept(reads).iter().all(|entry| entry.holds(probe))
    }

    /// The same inventory, and every recorded read still as recorded: the stamp of a pass whose
    /// read set is unrecorded never matches.
    fn matches(&self, current: &Stamp, probe: &mut Probe) -> bool {
        self.same_entries(current)
            && matches!(self.reads(), Some(Some(reads)) if reads.iter().all(|entry| entry.holds(probe)))
    }
}

mod scope;
pub(crate) use scope::{inventory, inventory_scoped};

/// Lifetime of a multi-session source operation. SQLite cold-store snapshots are reused while
/// this guard or any reader is alive and removed when the last owner drops. Without an outer
/// guard, standalone session reads remain safe but may repeat copying on non-cloning filesystems.
#[must_use]
pub struct ReadScope {
    _inventory: scope::Context,
    #[cfg(feature = "sqlite")]
    _sqlite: crate::harness::store::sqlite::ReadScope,
}

impl ReadScope {
    /// Run a worker under this operation's inventory owner, restoring its prior thread context
    /// even on panic. The guard remains owned by the caller for the complete operation.
    pub fn run<T>(&self, operation: impl FnOnce() -> T) -> T {
        self._inventory.run(operation)
    }
}

pub fn read_scope() -> ReadScope {
    ReadScope {
        _inventory: scope::Context::operation(),
        #[cfg(feature = "sqlite")]
        _sqlite: crate::harness::store::sqlite::read_scope(),
    }
}

/// Capture an active inventory owner for existing parallel helpers, without creating one for
/// unrelated parallel consumers. Each worker restores its previous thread-local capability.
pub(crate) fn inventory_context() -> Option<scope::Context> {
    scope::Context::current()
}

/// The metadata captured immediately before a consumer reads dynamically resolved paths.
/// Captures absence too. Repeated paths retain the first stamp; publication revalidates it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReadSet {
    entries: BTreeMap<PathBuf, Entry>,
    known: bool,
}

impl Default for ReadSet {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            known: true,
        }
    }
}

impl ReadSet {
    /// A read set that missed a dependency: it may be replayed, but cannot establish cache
    /// freshness until a complete pass replaces it.
    pub fn unrecorded() -> Self {
        Self {
            entries: BTreeMap::new(),
            known: false,
        }
    }

    pub fn record(&mut self, path: &Path) {
        self.record_as(path, false);
    }

    /// [`Self::record`]; for an `appendable` primary its first record also keeps its [`Prefix`],
    /// so [`Self::fresh_or_grown`] can admit its growth by content.
    pub(crate) fn record_as(&mut self, path: &Path, appendable: bool) {
        let Some(path) = absolute(path) else {
            self.known = false;
            return;
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(path) {
            match Probe::default().entry(slot.key()) {
                Some(mut entry) => {
                    if let Some(state) = entry.state.filter(|_| appendable) {
                        entry.prefix = std::fs::File::open(&entry.path)
                            .ok()
                            .and_then(|file| Prefix::of(&file, state.len));
                    }
                    slot.insert(entry);
                }
                None => self.known = false,
            }
        }
    }

    /// [`Self::record`] of an append-only file whose head (and length so far) the reader
    /// consulted, not its growth: an append keeps it fresh, a rewrite of its sampled bytes
    /// ([`Prefix`]) does not.
    pub fn record_append_only(&mut self, path: &Path) {
        let Some(path) = absolute(path) else {
            self.known = false;
            return;
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(path) {
            match Probe::default().entry(slot.key()) {
                Some(mut entry) => {
                    if let Some(state) = entry.state {
                        entry.grows = std::fs::File::open(&entry.path)
                            .ok()
                            .and_then(|file| Prefix::of(&file, state.len));
                    }
                    slot.insert(entry);
                }
                None => self.known = false,
            }
        }
    }

    /// The `(mtime_ns, len)` recorded for an existing `path`, the key persisted caches index by.
    pub(crate) fn signature(&self, path: &Path) -> Option<(u64, u64)> {
        let state = self.entries.get(&absolute(path)?)?.state.as_ref()?;
        Some((state.mtime_ns, state.len))
    }

    /// Paths whose original stamps this read set retains. Dependency notifications can replay
    /// these paths while guarded projections/publication continue to carry the original set.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.entries.keys().map(PathBuf::as_path)
    }

    /// Combine consumer reads, retaining the first metadata observed for each path.
    pub fn merge(&mut self, other: &Self) {
        self.known &= other.known;
        for (path, entry) in &other.entries {
            self.entries.entry(path.clone()).or_insert_with(|| entry.clone());
        }
    }

    /// Captured reads may include auxiliary sources beyond a caller's primary baseline.
    pub(crate) fn includes(&self, baseline: &Self) -> bool {
        self.known
            && baseline.known
            && baseline
                .entries
                .iter()
                .all(|(path, entry)| self.entries.get(path) == Some(entry))
    }

    /// Revalidate exactly the paths captured by this consumer, including absence and identity.
    pub fn fresh(&self, probe: &mut Probe) -> bool {
        self.known && self.entries.values().all(|entry| entry.holds(probe))
    }

    /// [`Self::fresh`], except that a primary recorded as appendable may also
    /// have grown ([`FileId::admits`]): a pass that read inside its captured length still
    /// describes that prefix.
    pub(crate) fn fresh_or_grown(&self, probe: &mut Probe) -> bool {
        self.known
            && self.entries.values().all(|entry| {
                entry.holds(probe)
                    || entry
                        .state
                        .zip(entry.prefix)
                        .zip(probe.entry(&entry.path).and_then(|now| now.state))
                        .is_some_and(|((then, prefix), now)| then.admits(&prefix, &now, &entry.path))
            })
    }
}

/// Record the original and auxiliary sources retained by one message before a consumer demands it.
pub(crate) fn session_reads(session: &crate::Session) -> ReadSet {
    let mut reads = ReadSet::default();
    let primary = session.source_path.as_deref();
    if let Some(primary) = primary {
        reads.record(primary);
    }
    for message in &session.messages {
        record_message_reads(message, primary, &mut reads);
    }
    reads
}

/// Record the original and auxiliary sources retained by one message before a consumer demands it.
pub(crate) fn record_message_reads(message: &crate::Message, primary: Option<&std::path::Path>, reads: &mut ReadSet) {
    /// A captured source records its path (unless it *is* the primary) plus its dependencies.
    fn captured(
        path: &std::path::Path,
        deps: Option<&ReadSet>,
        primary: Option<&std::path::Path>,
        reads: &mut ReadSet,
    ) {
        if primary != Some(path) {
            reads.record(path);
        }
        if let Some(deps) = deps {
            reads.merge(deps);
        }
    }
    if let Some(record) = &message.complete_record {
        if let crate::Json::Source(range) = &record.value {
            captured(range.path(), range.dependencies(), primary, reads);
        }
    }
    for block in &message.content {
        if let crate::Block::ToolUse {
            input: crate::Json::Source(range),
            ..
        } = block
        {
            captured(range.path(), range.dependencies(), primary, reads);
        }
        let text = match block {
            crate::Block::Text { text } | crate::Block::Thinking { text, .. } => Some(text),
            crate::Block::ToolResult { content, .. } => Some(content),
            _ => None,
        };
        match text {
            Some(crate::Text::Span(span)) => {
                if let Some(path) = span.source.as_ref().filter(|path| Some(path.as_path()) != primary) {
                    reads.record(path);
                }
            }
            Some(crate::Text::Source(source)) => captured(source.path(), source.dependencies(), primary, reads),
            _ => {}
        }
    }
}

/// What a consumer captured before reading one session: the primary's identity and the stamp of
/// its auxiliary paths. Compared one way: [`Baseline::fresh`] (strict) or
/// [`Baseline::dependencies_fresh`] (auxiliary only, across primary growth).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub primary: Option<FileId>,
    pub stamp: Option<Stamp>,
    /// A session in a shared SQLite store: its own rows as the adapter certified them when this
    /// baseline was published ([`Rows`]). Absent for every other source, and then never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Rows>,
}

/// The certificate of one session's rows in a database shared by many sessions
/// ([`crate::harness::Adapter::rows`]): how many rows its read consults, the highest rowid among
/// them (the high-water mark), and a digest of the small rows and flags beside them (session
/// metadata, lineage, visibility). A write to the store moves the database's identity for every
/// session in it; a session whose certificate is unchanged is admitted without rereading. Like a
/// resume mark's samples, it is not a content digest: an in-place edit of a message body that
/// changes no count, rowid, flag or session row escapes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rows {
    pub count: u64,
    pub high: i64,
    pub digest: u64,
}

impl Rows {
    /// Fold `bytes` into a certificate digest (FNV-1a), for adapters building one.
    pub fn fold(digest: u64, bytes: &[u8]) -> u64 {
        fnv1a(digest, bytes)
    }
    /// The digest a certificate starts from.
    pub const SEED: u64 = 0xcbf2_9ce4_8422_2325;
}

impl Baseline {
    /// The captured primary's length; 0 when it was absent.
    pub fn primary_len(&self) -> u64 {
        self.primary.map_or(0, |id| id.len)
    }

    /// Retain both declared inventory and conditional reads with their original identities.
    pub(crate) fn all_reads(&self) -> Option<ReadSet> {
        let stamp = self.stamp.as_ref()?;
        let mut reads = ReadSet::default();
        for entry in stamp.entries()?.into_iter().chain(stamp.reads()?.into_iter().flatten()) {
            reads.entries.entry(entry.path.clone()).or_insert(entry);
        }
        Some(reads)
    }

    /// Recorded consumer reads, for a resumed consumer to retain reads made by its old prefix.
    pub fn reads(&self) -> Option<ReadSet> {
        let entries = self.stamp.as_ref()?.reads()??;
        Some(ReadSet {
            entries: entries.into_iter().map(|entry| (entry.path.clone(), entry)).collect(),
            known: true,
        })
    }

    /// Auxiliary inventories and all recorded reads still match, independently of primary growth;
    /// a [`Dependencies::resumable`] file may have grown, which only a resumed read may admit.
    pub fn dependencies_fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.stamp
            .as_ref()
            .zip(current.stamp.as_ref())
            .is_some_and(|(old, now)| old.matches_grown(now, probe))
    }

    /// Strict: the primary was captured and is unchanged, and every dependency is as recorded.
    pub fn fresh(&self, current: &Self, probe: &mut Probe) -> bool {
        self.primary.is_some()
            && self.primary == current.primary
            && self
                .stamp
                .as_ref()
                .zip(current.stamp.as_ref())
                .is_some_and(|(old, now)| old.matches(now, probe))
    }

    /// A session in a store shared by many whose files moved (another session was written): the
    /// store's own files aside, every dependency is as recorded, and `r`'s rows certify as they
    /// did ([`Rows`]). The store is `r`'s primary (a SQLite database, a `logs.json`) or a shared
    /// side file of it ([`crate::harness::Adapter::rows_store`]), when its primary must be as it
    /// was.
    pub fn rows_fresh(&self, r: &SessionRef, current: &Self, probe: &mut Probe) -> bool {
        let (Some(rows), Some(_), Some(old), Some(now)) =
            (self.rows, current.primary, self.stamp.as_ref(), current.stamp.as_ref())
        else {
            return false;
        };
        let Some(shared) = probe.rows_store(r) else {
            return false;
        };
        // A store that is not the session's primary certifies its slice of a shared side file;
        // the primary itself must be as it was. (A primary store's replacement is in its
        // certificate: [`store_birth`].)
        let primary = probe.primary_path(r);
        if absolute(&shared) != absolute(&primary) && self.primary != current.primary {
            return false;
        }
        let store = match probe.adapters.get(&r.harness) {
            Some(Some(adapter)) => store_files(adapter.as_ref(), &shared),
            _ => return false,
        };
        let Some(store) = store.iter().map(|p| absolute(p)).collect::<Option<Vec<_>>>() else {
            return false;
        };
        old.matches_except(now, probe, &store) && probe.rows(r) == Some(rows)
    }

    /// Attach the read set captured by this consumer's complete pass, never another consumer's.
    pub fn with_reads(&self, reads: &ReadSet) -> Self {
        let stamp = (|| {
            if !reads.known {
                return None;
            }
            let stamp = self.stamp.as_ref()?;
            let entries = stamp.entries()?;
            let reads: Vec<Entry> = reads.entries.values().cloned().collect();
            Stamp::new(&entries, Some(&reads))
        })();
        Self {
            primary: self.primary,
            stamp,
            rows: self.rows,
        }
    }
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// What one consumer records about a session beside the baseline it read it at.
pub trait Payload: Serialize + serde::de::DeserializeOwned {
    /// The consumer's name: written into every record and checked when one loads.
    const TAG: &'static str;
    /// Whether the payload is derived from the session's own rows alone, so a SQLite session's
    /// [`Rows`] certificate may admit it across writes to other sessions of its store. A
    /// database-wide projection is not.
    const BY_ROWS: bool = false;
}

/// The one envelope of everything cv stores about a session: the [`VERSION`] it was written
/// under, the [`Baseline`] its source was read at and the consumer's payload, serialized by
/// [`Record::encode`] alone and admitted by [`Record::admit`] alone. A record of another version
/// or another consumer does not decode; one whose source moved does not admit.
#[derive(Debug, Clone)]
pub struct Record<T> {
    pub version: u32,
    pub baseline: Baseline,
    pub payload: T,
}

/// The one writer of a [`Record`]'s bytes: JSON `{version, tag, baseline, payload}`.
fn encode<T: Payload>(version: u32, baseline: &Baseline, payload: &T) -> Vec<u8> {
    serde_json::to_vec(&WireOut {
        version,
        tag: T::TAG,
        baseline,
        payload,
    })
    .expect("a record serializes")
}

#[derive(Serialize)]
struct WireOut<'a, T> {
    version: u32,
    tag: &'a str,
    baseline: &'a Baseline,
    payload: &'a T,
}

#[derive(Deserialize)]
struct WireIn<T> {
    version: u32,
    tag: String,
    baseline: Baseline,
    payload: T,
}

impl<T: Payload> Record<T> {
    pub fn new(baseline: Baseline, payload: T) -> Self {
        Self {
            version: VERSION,
            baseline,
            payload,
        }
    }

    /// The record's bytes.
    pub fn encode(&self) -> Vec<u8> {
        encode(self.version, &self.baseline, &self.payload)
    }

    /// The record `bytes` hold, when they are one of this version and this consumer.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let wire: WireIn<T> = serde_json::from_slice(bytes).ok()?;
        (wire.version == VERSION && wire.tag == T::TAG).then_some(Self {
            version: wire.version,
            baseline: wire.baseline,
            payload: wire.payload,
        })
    }

    /// THE admission: the payload, while the source is still the one this record was read at
    /// (`now`, captured before this read: [`Baseline::fresh`]).
    pub fn admit(&self, now: &Baseline, probe: &mut Probe) -> Option<&T> {
        (self.version == VERSION && self.baseline.fresh(now, probe)).then_some(&self.payload)
    }

    /// [`Self::admit`], or, for a payload of `r`'s own rows ([`Payload::BY_ROWS`]) in a shared
    /// SQLite store, its unchanged [`Rows`] certificate ([`Baseline::rows_fresh`]).
    pub fn admit_for(&self, r: &SessionRef, now: &Baseline, probe: &mut Probe) -> Option<&T> {
        self.admit(now, probe).or_else(|| {
            (T::BY_ROWS && self.version == VERSION && self.baseline.rows_fresh(r, now, probe)).then_some(&self.payload)
        })
    }
}

/// What an adapter declares a session reads besides its primary
/// ([`crate::harness::Adapter::dependencies`]): `files` are recorded present or absent by their
/// own identity (a directory listed here moves when an entry appears or goes, without recording
/// its entries); `directories` are enumerated one level and every entry recorded.
#[derive(Default)]
pub struct Dependencies {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    /// Append-only files the read consults whole, whose growth can revise what it emitted before:
    /// recorded like `files`, with a [`Prefix`] sample, so their growth keeps a resume mark usable
    /// (the adapter's resumed read declines when the growth revises its prefix) but never admits a
    /// record as it stands.
    pub resumable: Vec<PathBuf>,
}

/// The store file's identity, folded into each of its sessions' certificates: device, inode and
/// birth time. Writes to a store keep its file; a store replaced by another file (a copy, a
/// restore) is another store whose rows nothing here has read, even when the copy holds the same
/// bytes (so no SQLite header field tells them apart) and the filesystem reuses the freed inode
/// (ext4 does; the birth time then differs). Where the filesystem keeps no birth time, device and
/// inode alone.
fn store_birth(path: &Path) -> [u8; 32] {
    let mut out = [0u8; 32];
    let Ok(metadata) = std::fs::metadata(path) else {
        return out;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        out[..8].copy_from_slice(&metadata.dev().to_le_bytes());
        out[8..16].copy_from_slice(&metadata.ino().to_le_bytes());
    }
    let born = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    out[16..].copy_from_slice(&born.to_le_bytes());
    out
}

/// A shared SQLite store, and the certificates of its sessions ([`Probe::rows`]).
type Store = (Harness, PathBuf);
type Certified = Option<std::sync::Arc<HashMap<String, Rows>>>;

/// File and immediate-directory probes shared within one operation. It never descends recursively.
#[derive(Default)]
pub struct Probe {
    entries: HashMap<PathBuf, Observation>,
    directories: HashMap<PathBuf, Option<std::sync::Arc<Vec<PathBuf>>>>,
    // Adapters resolved once per operation: detection stats roots, so not once per session.
    adapters: HashMap<Harness, Option<Box<dyn crate::harness::Adapter>>>,
    // Shared stores certified once per operation ([`Probe::rows`]).
    rows: HashMap<Store, Certified>,
}

enum Observation {
    File(FileId),
    // Metadata exists, but its timestamp cannot form a freshness identity.
    Unidentified(u64),
    Absent,
    Unavailable,
}

impl Probe {
    fn observation(&mut self, path: &Path) -> &Observation {
        self.entries
            .entry(path.to_path_buf())
            .or_insert_with(|| match std::fs::metadata(path) {
                Ok(md) => FileId::of(&md).map_or(Observation::Unidentified(md.len()), Observation::File),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Observation::Absent
                }
                Err(_) => Observation::Unavailable,
            })
    }

    fn entry(&mut self, path: &Path) -> Option<Entry> {
        let path = absolute(path)?;
        let state = match self.observation(&path) {
            Observation::File(id) => Some(*id),
            Observation::Absent => None,
            Observation::Unidentified(_) | Observation::Unavailable => return None,
        };
        Some(Entry {
            path,
            state,
            prefix: None,
            grows: None,
            resumes: None,
        })
    }

    /// Whether `path` exists, recording its identity for this operation; `None` when unreadable.
    pub fn exists(&mut self, path: &Path) -> Option<bool> {
        Some(self.entry(path)?.state.is_some())
    }

    /// Metadata size captured once per path in this probe. Missing/unreadable paths return None;
    /// a readable path without a usable freshness timestamp still has a size.
    pub fn size(&mut self, path: &Path) -> Option<u64> {
        match self.observation(&absolute(path)?) {
            Observation::File(id) => Some(id.len),
            Observation::Unidentified(len) => Some(*len),
            Observation::Absent | Observation::Unavailable => None,
        }
    }

    /// The entries of directory `path`, read once per operation; an absent directory is empty.
    pub fn list(&mut self, path: &Path) -> Option<std::sync::Arc<Vec<PathBuf>>> {
        let path = absolute(path)?;
        self.directories
            .entry(path.clone())
            .or_insert_with(|| match std::fs::read_dir(&path) {
                Ok(rd) => rd
                    .map(|e| e.ok().map(|e| e.path()))
                    .collect::<Option<Vec<_>>>()
                    .map(std::sync::Arc::new),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(std::sync::Arc::new(Vec::new())),
                Err(_) => None,
            })
            .clone()
    }

    /// [`primary_path`] with this operation's adapter registry.
    pub fn primary_path(&mut self, r: &SessionRef) -> PathBuf {
        self.adapters
            .entry(r.harness)
            .or_insert_with(|| crate::harness::for_harness(r.harness))
            .as_ref()
            .map_or_else(|| r.path.clone(), |adapter| adapter.resume_source(r))
    }

    fn dependencies_from(&mut self, r: &SessionRef, primary: &Path) -> Option<Stamp> {
        Stamp::new(&self.dependency_entries(r, primary)?, None)
    }

    // Publication already has typed captured records; collect fresh entries without a second
    // serialized stamp and parser allocation. The same inventory serves serialized checkpoints.
    fn dependency_entries(&mut self, r: &SessionRef, primary: &Path) -> Option<Vec<Entry>> {
        // Native SQLite identities retain sidecars; an absent metadata file is still recorded,
        // because creating it can supply a previously missing cwd or title.
        let (sqlite, metadata) = {
            let adapter = self
                .adapters
                .entry(r.harness)
                .or_insert_with(|| crate::harness::for_harness(r.harness));
            let adapter = adapter.as_ref();
            (
                adapter.map_or_else(|| Some(has_sqlite_extension(&r.path)), |a| a.sqlite_source(r)),
                adapter.and_then(|a| a.metadata_file(r)),
            )
        };
        let mut deps = Dependencies::default();
        if sqlite? {
            deps.files.extend(sqlite_sidecars(&r.path));
        }
        deps.files.extend(metadata);
        // The adapter is taken out of the registry while it probes through `self`.
        let adapter = self
            .adapters
            .remove(&r.harness)
            .unwrap_or_else(|| crate::harness::for_harness(r.harness));
        let declared = adapter
            .as_ref()
            .map_or(Some(()), |adapter| adapter.dependencies(r, self, &mut deps));
        self.adapters.insert(r.harness, adapter);
        declared?;
        let Dependencies {
            files,
            directories,
            resumable,
        } = deps;
        let mut entries = BTreeMap::new();
        for dir in directories {
            entries.insert(dir.clone(), self.entry(&dir)?.state);
            for file in self.list(&dir)?.iter() {
                if primary != r.path && absolute(file)? == absolute(primary)? {
                    continue;
                }
                let state = self.entry(file)?.state;
                entries.insert(file.clone(), state);
            }
        }
        for file in files {
            // A directory adapter's append file belongs to the primary signature and sampled
            // checkpoint guard. Keeping it in the auxiliary set would reject every append.
            // Preserve existing shared-database inventories when the logical path is the primary.
            if primary != r.path && absolute(&file)? == absolute(primary)? {
                continue;
            }
            let state = self.entry(&file)?.state;
            entries.insert(file, state);
        }
        let mut samples = HashMap::new();
        for file in resumable {
            let state = self.entry(&file)?.state;
            let file = absolute(&file)?;
            if let Some(state) = state {
                // A file that moved between the stat and the sample is recorded exact.
                let sample = std::fs::File::open(&file).ok().and_then(|f| Prefix::of(&f, state.len));
                samples.insert(file.clone(), sample);
            }
            entries.insert(file, state);
        }
        entries
            .into_iter()
            .map(|(path, state)| {
                let path = if path.is_absolute() { path } else { absolute(&path)? };
                Some(Entry {
                    resumes: samples.remove(&path).flatten(),
                    path,
                    state,
                    prefix: None,
                    grows: None,
                })
            })
            .collect()
    }

    pub fn snapshot(&mut self, r: &SessionRef) -> Baseline {
        let primary = self.primary_path(r);
        self.snapshot_from(r, &primary)
    }

    /// Capture the concrete adapter's append source while retaining its logical session key
    /// and all other auxiliary inputs. Path selection must not perform unrecorded source reads.
    pub fn snapshot_from(&mut self, r: &SessionRef, primary: &Path) -> Baseline {
        Baseline {
            primary: self.entry(primary).and_then(|entry| entry.state),
            stamp: self.dependencies_from(r, primary),
            rows: None,
        }
    }

    /// The store whose certificate covers `r` ([`crate::harness::Adapter::rows_store`]).
    pub fn rows_store(&mut self, r: &SessionRef) -> Option<PathBuf> {
        self.adapters
            .entry(r.harness)
            .or_insert_with(|| crate::harness::for_harness(r.harness))
            .as_ref()?
            .rows_store(r)
    }

    /// `r`'s [`Rows`] certificate, when its adapter keeps it in a store shared by many sessions
    /// ([`crate::harness::Adapter::rows_store`]: a SQLite database, a gemini-cli `logs.json`, a
    /// Kimi Code `state.json`): every session of that store is certified at once, from one
    /// consistent read, once per store state in this process (the state is the store's, WAL's and
    /// journal's identities, the ones a write moves) and once per probe.
    pub fn rows(&mut self, r: &SessionRef) -> Option<Rows> {
        type State = Vec<Option<FileId>>;
        type Stores = std::sync::Mutex<HashMap<Store, (State, Certified)>>;
        static STORES: std::sync::LazyLock<Stores> = std::sync::LazyLock::new(Default::default);
        let shared = self.rows_store(r)?;
        let store = (r.harness, absolute(&shared)?);
        if !self.rows.contains_key(&store) {
            let adapter = self.adapters.get(&r.harness).and_then(Option::as_ref);
            let certified = adapter.and_then(|adapter| {
                let files = store_files(adapter.as_ref(), &shared);
                let state = || -> State { files.iter().map(|f| FileId::at(f)).collect() };
                let before = state();
                let cached = STORES
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&store)
                    .filter(|(at, _)| *at == before)
                    .map(|(_, certified)| certified.clone());
                cached.unwrap_or_else(|| {
                    // A session's own store carries its birth; a shared side file's slice is
                    // certified by content, whatever file holds it (side files are commonly
                    // replaced by an atomic rename on every write).
                    let born = (shared == r.path).then(|| store_birth(&store.1));
                    let certified = adapter.rows(&shared).map(|mut rows| {
                        if let Some(born) = born {
                            for certificate in rows.values_mut() {
                                certificate.digest = Rows::fold(certificate.digest, &born);
                            }
                        }
                        std::sync::Arc::new(rows)
                    });
                    // Kept only when no write landed while certifying.
                    if state() == before {
                        STORES
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(store.clone(), (before, certified.clone()));
                    }
                    certified
                })
            });
            self.rows.insert(store.clone(), certified);
        }
        let key = match self.adapters.get(&r.harness) {
            Some(Some(adapter)) => adapter.rows_key(r),
            _ => &r.id,
        };
        self.rows.get(&store)?.as_ref()?.get(key).copied()
    }
}

/// The files whose identities are a shared store's state ([`Probe::rows`]): each of its
/// [`crate::harness::Adapter::rows_files`] with its SQLite sidecars.
fn store_files(adapter: &dyn crate::harness::Adapter, store: &Path) -> Vec<PathBuf> {
    adapter
        .rows_files(store)
        .into_iter()
        .flat_map(|file| {
            let mut files: Vec<PathBuf> = sqlite_sidecars(&file).into();
            files.push(file);
            files
        })
        .collect()
}

/// The built-in adapter's actual append file, which may differ from a directory session key.
pub fn primary_path(r: &SessionRef) -> PathBuf {
    crate::harness::for_harness(r.harness).map_or_else(|| r.path.clone(), |adapter| adapter.resume_source(r))
}

/// A captured source still matches at publication time. Errors never establish freshness.
pub fn unchanged(r: &SessionRef, baseline: &Baseline) -> bool {
    unchanged_from(r, &primary_path(r), baseline)
}

/// Publication guard against the concrete adapter's source path.
pub fn unchanged_from(r: &SessionRef, primary: &Path, baseline: &Baseline) -> bool {
    unchanged_with(r, primary, baseline, &mut Probe::default())
}

/// [`unchanged_from`] on the publishing transaction's one probe: its rows' shared files are
/// stat'ed once, after every pass it publishes.
pub(crate) fn unchanged_with(r: &SessionRef, primary: &Path, baseline: &Baseline, probe: &mut Probe) -> bool {
    admitted_with(r, primary, baseline, false, probe).is_some()
}

/// [`unchanged_from`] for a pass over an append-only primary's prefix: the primary may also have
/// grown (the same file, no shorter), its read bytes held by the append-only entry `baseline`'s
/// reads keep for it ([`ReadSet::record_append_only`]); every other input as captured. The
/// primary's length now, when admitted.
pub fn grown_from(r: &SessionRef, primary: &Path, baseline: &Baseline) -> Option<u64> {
    admitted_with(r, primary, baseline, true, &mut Probe::default())
}

fn admitted_with(r: &SessionRef, primary: &Path, baseline: &Baseline, grows: bool, probe: &mut Probe) -> Option<u64> {
    let then = baseline.primary?;
    let now = probe.entry(primary).and_then(|entry| entry.state)?;
    let grown = now != then;
    if grown && !(grows && (now.dev, now.ino) == (then.dev, then.ino) && now.len >= then.len) {
        return None;
    }
    let stamp = baseline.stamp.as_ref()?;
    let current = probe.dependency_entries(r, primary)?;
    // Directory enumeration is complete. Only metadata probes are needed for dynamic reads;
    // release path lists before parsing the captured inventory's owned paths.
    probe.directories.clear();
    let (entries, reads) = (stamp.entries()?, stamp.reads()?);
    let same = entries == current;
    drop(entries);
    drop(current);
    // A grown primary is admitted only where the read set samples it as append-only.
    let reads = reads.unwrap_or_default();
    let sampled = !grown
        || absolute(primary).is_some_and(|path| reads.iter().any(|entry| entry.path == path && entry.grows.is_some()));
    (same && sampled && reads.iter().all(|entry| entry.holds(probe))).then_some(now.len)
}

pub type Update = (SessionRef, Baseline);

type Key = (String, String, String);
fn key(r: &SessionRef) -> Key {
    (
        r.harness.as_str().into(),
        r.id.clone(),
        r.path.to_string_lossy().into_owned(),
    )
}

/// A consumer's records, loaded once. Records of another version or consumer, and sessions
/// without one, reread once; even an empty complete read set has a record, so an old consumer
/// cannot silently certify unresolved paths.
pub struct Table<T> {
    rows: HashMap<Key, Record<T>>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self { rows: HashMap::new() }
    }
}

impl<T: Payload> Table<T> {
    /// `r`'s record, for [`Baseline::dependencies_fresh`] across growth and resume decisions.
    pub fn get(&self, r: &SessionRef) -> Option<&Record<T>> {
        self.rows.get(&key(r))
    }

    /// [`Record::admit_for`] of `r`'s record.
    pub fn admit(&self, r: &SessionRef, now: &Baseline, probe: &mut Probe) -> Option<&T> {
        self.get(r)?.admit_for(r, now, probe)
    }

    /// The records of `consumer` (`T::TAG`, or a tagged instance such as one index's `fts:<id>`).
    #[cfg(feature = "sqlite")]
    pub fn load(consumer: &str) -> Self {
        open().map_or_else(Self::default, |conn| Self::load_in(&conn, consumer))
    }

    /// Load alongside SQL consumer rows in the caller's read transaction.
    #[cfg(feature = "sqlite")]
    pub(crate) fn load_in(conn: &rusqlite::Connection, consumer: &str) -> Self {
        let load = || -> Option<_> {
            let mut stmt = conn
                .prepare("SELECT harness,id,path,record FROM source_dependencies WHERE consumer=?1")
                .ok()?;
            let rows = stmt
                .query_map([consumer], |row| {
                    Ok(((row.get(0)?, row.get(1)?, row.get(2)?), row.get::<_, Vec<u8>>(3)?))
                })
                .ok()?;
            Some(
                rows.flatten()
                    .filter_map(|(key, record)| Some((key, Record::decode(&record)?)))
                    .collect(),
            )
        };
        Self {
            rows: load().unwrap_or_default(),
        }
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn load(_consumer: &str) -> Self {
        Self::default()
    }
}

/// The catalog connection: `source_dependencies` is part of its migration.
#[cfg(feature = "sqlite")]
fn open() -> Option<rusqlite::Connection> {
    crate::catalog::open_db()
}

/// `r`'s record of `consumer`, read from the caller's statement snapshot.
#[cfg(feature = "sqlite")]
pub(crate) fn record_in<T: Payload>(conn: &rusqlite::Connection, consumer: &str, r: &SessionRef) -> Option<Record<T>> {
    let (h, id, path) = key(r);
    let record: Vec<u8> = conn
        .query_row(
            "SELECT record FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
            rusqlite::params![consumer, h, id, path],
            |row| row.get(0),
        )
        .ok()?;
    Record::decode(&record)
}

/// Whether `r`'s record of `consumer` holds `payload` and still admits against `now`.
#[cfg(feature = "sqlite")]
pub(crate) fn fresh_in<T: Payload + PartialEq>(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    now: &Baseline,
    payload: &T,
) -> bool {
    record_in::<T>(conn, consumer, r)
        .is_some_and(|record| record.payload == *payload && record.admit_for(r, now, &mut Probe::default()).is_some())
}

/// A 64-bit digest of `bytes`, for a payload that binds output the consumer keeps elsewhere.
pub(crate) fn digest(bytes: &[u8]) -> u64 {
    fnv1a(0xcbf2_9ce4_8422_2325, bytes)
}

/// Publish `payload` in the same transaction as the consumer's own SQL output. A failure,
/// returned with its reason, must roll back both. Only a complete pass (its read set recorded)
/// over a source still unchanged publishes; `probe` is the transaction's one, created after its
/// passes.
#[cfg(feature = "sqlite")]
pub(crate) fn publish_in<T: Payload>(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    baseline: &Baseline,
    payload: &T,
    probe: &mut Probe,
) -> anyhow::Result<()> {
    if crate::env::frozen() {
        return Ok(());
    }
    let (h, id, path) = key(r);
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        rusqlite::params![consumer, h, id, path],
    )?;
    let complete = baseline
        .stamp
        .as_ref()
        .is_some_and(|stamp| matches!(stamp.reads(), Some(Some(_))));
    // A shared store's certificate is taken before the identity check: the store unchanged since
    // the capture means the certificate describes the rows this pass read.
    let rows = if T::BY_ROWS { probe.rows(r) } else { None };
    if complete && unchanged_with(r, &probe.primary_path(r), baseline, probe) {
        let baseline = Baseline {
            rows,
            ..baseline.clone()
        };
        conn.execute(
            "INSERT INTO source_dependencies(consumer,harness,id,path,record) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![consumer, h, id, path, encode(VERSION, &baseline, payload)],
        )?;
    }
    Ok(())
}

/// Invalidate old baselines before changing consumer output. Failure must prevent that output
/// write: otherwise an older stamp could certify newer output after dependencies revert.
#[cfg(feature = "sqlite")]
pub fn invalidate(consumer: &str, refs: &[SessionRef]) -> anyhow::Result<()> {
    anyhow::ensure!(!crate::env::frozen(), "the catalog is frozen");
    if refs.is_empty() {
        return Ok(());
    }
    let mut conn = open().ok_or_else(|| anyhow::anyhow!("the catalog cannot be opened"))?;
    let tx = conn.transaction()?;
    for r in refs {
        let (h, id, path) = key(r);
        tx.execute(
            "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
            rusqlite::params![consumer, h, id, path],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(not(feature = "sqlite"))]
pub fn invalidate(_consumer: &str, _refs: &[SessionRef]) -> anyhow::Result<()> {
    Ok(())
}

/// Publish after a consumer whose output lives outside the catalog commits it (its payload is
/// `T::default()`). Revalidation makes concurrent edits stale; old baselines must already have
/// been invalidated with [`invalidate`]. SQL consumers publish atomically through `publish_in`.
/// Each row publishes on its own: the rows returned failed, with the reason, and only their
/// sessions are read again next pass; the rest commit together.
#[cfg(feature = "sqlite")]
pub fn update<T: Payload + Default>(consumer: &str, updates: &[Update]) -> Vec<(SessionRef, String)> {
    if crate::env::frozen() {
        return updates
            .iter()
            .map(|(r, _)| (r.clone(), "--frozen forbids updating source baselines".into()))
            .collect();
    }
    if updates.is_empty() {
        return Vec::new();
    }
    let mut failed = Vec::new();
    let mut publish = || -> anyhow::Result<()> {
        let mut conn = open().ok_or_else(|| anyhow::anyhow!("the catalog cannot be opened"))?;
        let mut tx = conn.transaction()?;
        let mut probe = Probe::default();
        for (r, baseline) in updates {
            let row = tx.savepoint().map_err(anyhow::Error::from).and_then(|row| {
                publish_in(&row, consumer, r, baseline, &T::default(), &mut probe)?;
                Ok(row.commit()?)
            });
            if let Err(e) = row {
                failed.push((r.clone(), format!("{e:#}")));
            }
        }
        Ok(tx.commit()?)
    };
    match publish() {
        Ok(()) => failed,
        Err(e) => updates.iter().map(|(r, _)| (r.clone(), format!("{e:#}"))).collect(),
    }
}

#[cfg(not(feature = "sqlite"))]
pub fn update<T: Payload + Default>(_consumer: &str, _updates: &[Update]) -> Vec<(SessionRef, String)> {
    Vec::new()
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    /// A consumer whose output lives elsewhere: its records carry nothing but the baseline.
    #[derive(Default, Serialize, Deserialize)]
    struct Done;
    impl Payload for Done {
        const TAG: &'static str = "done";
    }

    #[cfg(unix)]
    #[test]
    fn metadata_size_reuse_does_not_certify_unidentified_sources() {
        let world = crate::test_world("probe-size-identity");
        let path = world.base.join("source");
        std::fs::write(&path, "one").unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
            )
            .unwrap();
        let mut probe = Probe::default();
        assert_eq!(probe.size(&path), Some(3));
        assert_eq!(probe.exists(&path), None);
        assert!(probe.entry(&path).is_none());
        std::fs::write(&path, "longer").unwrap();
        assert_eq!(probe.size(&path), Some(3));
        assert_eq!(Probe::default().size(&path), Some(6));
        std::fs::remove_file(&path).unwrap();
        let mut absent = Probe::default();
        assert_eq!(absent.size(&path), None);
        assert_eq!(absent.exists(&path), Some(false));
    }

    #[test]
    fn arbitrary_opencode_database_names_retain_wal_and_journal_guards() {
        let w = crate::test_world("source-opencode-arbitrary-db");
        let path = w.base.join("custom-store");
        std::fs::write(&path, b"SQLite format 3\0remaining database bytes").unwrap();
        let r = SessionRef::new("session", Harness::OpenCode, &path);
        let opencode = crate::harness::rootless(Harness::OpenCode).unwrap();
        assert_eq!(opencode.sqlite_source(&r), Some(true));
        let before = Probe::default().snapshot(&r);
        assert!(unchanged(&r, &before));
        for sidecar in sqlite_sidecars(&path) {
            std::fs::write(&sidecar, "changed native state").unwrap();
            assert!(!unchanged(&r, &before));
            std::fs::remove_file(sidecar).unwrap();
            assert!(unchanged(&r, &before));
        }
        let missing = SessionRef::new("missing", Harness::OpenCode, w.base.join("missing"));
        assert_eq!(opencode.sqlite_source(&missing), None);
        assert!(Probe::default().snapshot(&missing).stamp.is_none());
        let legacy = w.base.join("ses_old.json");
        std::fs::write(&legacy, "{}").unwrap();
        assert_eq!(
            opencode.sqlite_source(&SessionRef::new("old", Harness::OpenCode, legacy)),
            Some(false)
        );
    }

    #[test]
    fn grok_dependency_kind_projects_only_exact_subagents_and_keeps_acceptance() {
        let world = crate::test_world("grok-dependency-kind");
        let group = world.base.join("sessions");
        let session = group.join("child");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("chat_history.jsonl"), "").unwrap();
        let reference = crate::SessionRef::new("child", crate::Harness::Grok, session.clone());
        let large = "ignored payload".repeat(1024);
        let cases = [
            serde_json::json!({"session_kind":"subagent", "ignored":large}).to_string(),
            "{\"session_kind\":\"subagent\",\"session_kind\":false}".into(),
            "{\"session_kind\":\"subagentx\"}".into(),
            "null".into(),
            "{\"session_kind\":\"subagent\",\"ignored\":\"\\ud800\"}".into(),
        ];
        for text in cases {
            std::fs::write(session.join("summary.json"), &text).unwrap();
            let old = serde_json::from_str::<serde_json::Value>(&text).ok();
            let stamp = super::Probe::default().snapshot(&reference).stamp;
            assert_eq!(stamp.is_some(), old.is_some());
            if let Some(stamp) = stamp {
                let expected = old.unwrap().get("session_kind").and_then(serde_json::Value::as_str) == Some("subagent");
                assert_eq!(
                    stamp.entries().unwrap().iter().any(|entry| entry.path == group),
                    expected
                );
            }
        }
    }

    #[test]
    fn impossible_child_absence_invalidates_when_parent_becomes_directory() {
        let world = crate::test_world("not-directory-absence");
        let parent = world.base.join("parent");
        let child = parent.join("child");
        std::fs::write(&parent, "a file").unwrap();
        let mut reads = ReadSet::default();
        reads.record(&child);
        assert!(reads.fresh(&mut Probe::default()));
        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&child, "now present").unwrap();
        assert!(!reads.fresh(&mut Probe::default()));
    }

    fn tuple(baseline: &Baseline) -> Option<(u64, u64)> {
        baseline.primary.map(|id| (id.len, id.mtime_ns))
    }

    #[cfg(unix)]
    #[test]
    fn primary_identity_rejects_restored_tuple_rewrites_and_replacements() {
        let w = crate::test_world("primary-identity");
        // Include the canonical delimiter in the filename to exercise JSON escaping.
        let path = w.base.join("plain,\"entries\":.jsonl");
        std::fs::write(&path, "old body").unwrap();
        let r = SessionRef::new("s", Harness::Claude, &path);
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        for consumer in ["counts", "events", "fts", "semantic"] {
            update::<Done>(consumer, &[(r.clone(), before.clone())]);
            assert!(Table::<Done>::load(consumer)
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&before, &mut Probe::default())));
        }
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "new body").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let now = Probe::default().snapshot(&r);
        assert_eq!(tuple(&before), tuple(&now), "length and mtime restored");
        assert!(!before.fresh(&now, &mut Probe::default()));
        assert!(!unchanged(&r, &before));
        assert!(
            before.dependencies_fresh(&now, &mut Probe::default()),
            "auxiliary prefix authority excludes primary identity"
        );
        for consumer in ["counts", "events", "fts", "semantic"] {
            let table = Table::<Done>::load(consumer);
            assert!(!table
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&now, &mut Probe::default())));
            assert!(table.get(&r).is_some_and(|old| old.baseline.primary == before.primary
                && old.baseline.dependencies_fresh(&now, &mut Probe::default())));
        }
        // A publication racing the restored-tuple write cannot establish freshness.
        update::<Done>("counts", &[(r.clone(), before.clone())]);
        assert!(!Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&now, &mut Probe::default())));
        let now = now.with_reads(&ReadSet::default());
        update::<Done>("counts", &[(r.clone(), now.clone())]);
        assert!(Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&now, &mut Probe::default())));
        assert!(!Table::<Done>::load("events")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&now, &mut Probe::default())));
        // Replacement with equal bytes and restored timestamps is a different primary too.
        let replacement = w.base.join("replacement");
        std::fs::write(&replacement, "new body").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = Probe::default().snapshot(&r);
        assert_eq!(tuple(&now), tuple(&replaced));
        assert!(!now.fresh(&replaced, &mut Probe::default()));
        assert!(!unchanged(&r, &now));
        assert!(!Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&replaced, &mut Probe::default())));
    }

    #[test]
    fn records_verify_version_tag_and_shape_before_anything_admits() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Row(String);
        impl Payload for Row {
            const TAG: &'static str = "row";
        }
        let w = crate::test_world("record-verify");
        let path = w.base.join("primary.jsonl");
        std::fs::write(&path, "body").unwrap();
        let r = SessionRef::new("s", Harness::Claude, &path);
        let now = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let stamp = now.stamp.as_ref().unwrap();
        assert!(stamp.matches(stamp, &mut Probe::default()));
        let unrecorded = Probe::default().snapshot(&r).stamp.unwrap();
        assert!(
            !unrecorded.matches(stamp, &mut Probe::default()),
            "an unrecorded read set never matches"
        );
        assert!(
            stamp.matches(&unrecorded, &mut Probe::default()),
            "a complete empty read set matches either"
        );
        let bytes = Record::new(now.clone(), Row("row".into())).encode();
        let stored = Record::<Row>::decode(&bytes).unwrap();
        assert_eq!(stored.admit(&now, &mut Probe::default()), Some(&Row("row".into())));
        assert_eq!(stored.baseline.primary, now.primary);
        assert!(Record::<Row>::decode(&bytes[..bytes.len() - 1]).is_none());
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (pointer, value) in [
            ("/version", serde_json::json!(VERSION - 1)),
            ("/tag", serde_json::json!("other")),
            ("/payload", serde_json::json!(1)),
            ("/baseline/stamp/entries", serde_json::json!({"version": 1})),
        ] {
            let mut other = wire.clone();
            *other.pointer_mut(pointer).unwrap() = value;
            let decoded = Record::<Row>::decode(&serde_json::to_vec(&other).unwrap());
            assert!(
                decoded.is_none_or(|record| record.admit(&now, &mut Probe::default()).is_none()),
                "{pointer}"
            );
        }
        let mut absent = wire.clone();
        absent["baseline"]["primary"] = serde_json::Value::Null;
        let absent = Record::<Row>::decode(&serde_json::to_vec(&absent).unwrap()).unwrap();
        assert!(absent.admit(&now, &mut Probe::default()).is_none());
    }

    #[test]
    fn sqlite_journal_creation_rewrite_and_removal_invalidate_inventory() {
        let world = crate::test_world("source-sqlite-journal");
        let db = world.base.join("sessions.db");
        std::fs::write(&db, "fixture").unwrap();
        let journal = world.base.join("sessions.db-journal");
        let r = SessionRef::new("s", Harness::Hermes, &db);
        let absent = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let stamp = absent.stamp.as_ref().unwrap();
        let mut entries = stamp.entries().unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.path == journal && entry.state.is_none()));
        entries.retain(|entry| entry.path != journal);
        let legacy = Baseline {
            stamp: Stamp::new(&entries, stamp.reads().unwrap().as_deref()),
            ..absent.clone()
        };
        assert!(
            !unchanged(&r, &legacy),
            "old inventories reread once without a version change"
        );
        assert!(unchanged(&r, &absent));
        std::fs::write(&journal, "before").unwrap();
        assert!(!unchanged(&r, &absent));
        let present = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(present.primary, absent.primary);
        assert!(unchanged(&r, &present));
        std::fs::write(&journal, "after!").unwrap();
        assert!(!unchanged(&r, &present), "same-size journal rewrite");
        let rewritten = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::remove_file(&journal).unwrap();
        assert!(!unchanged(&r, &rewritten));
    }

    #[test]
    fn typed_publication_matches_serialized_inventory_and_preserves_wire() {
        let world = crate::test_world("source-typed-publication");
        let storage = world.base.join("storage");
        for dir in ["session/project", "message/sid", "part/mid"] {
            std::fs::create_dir_all(storage.join(dir)).unwrap();
        }
        let path = storage.join("session/project/sid.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(storage.join("message/sid/header.json"), r#"{"id":"mid"}"#).unwrap();
        let part = storage.join("part/mid/part.json");
        std::fs::write(&part, "before").unwrap();
        let r = SessionRef::new("sid", Harness::OpenCode, path);
        let absent = world.base.join("conditional");
        let mut reads = ReadSet::default();
        reads.record(&absent);
        let before = Probe::default().snapshot(&r).with_reads(&reads);
        let stamp = before.stamp.as_ref().unwrap();
        assert_eq!(
            stamp.entries.get(),
            serde_json::to_string(&stamp.entries().unwrap()).unwrap()
        );
        let reference = |captured: &Baseline| {
            let mut probe = Probe::default();
            let now = probe.snapshot(&r);
            let old = captured.stamp.as_ref().unwrap();
            let fresh = now.stamp.as_ref().unwrap();
            now.primary == captured.primary
                && old.entries() == fresh.entries()
                && old.reads().unwrap().as_ref().is_none_or(|reads| {
                    reads
                        .iter()
                        .all(|entry| probe.entry(&entry.path).as_ref() == Some(entry))
                })
        };
        assert!(reference(&before));
        assert!(unchanged(&r, &before));
        std::fs::write(&absent, "created").unwrap();
        assert_eq!(unchanged(&r, &before), reference(&before));
        assert!(!unchanged(&r, &before));
        std::fs::remove_file(&absent).unwrap();
        std::fs::write(&part, "after!").unwrap();
        assert_eq!(unchanged(&r, &before), reference(&before));
        assert!(!unchanged(&r, &before));
        let updated = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_eq!(unchanged(&r, &updated), reference(&updated));
        assert!(!unchanged(&r, &updated));
        let populated = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::remove_file(storage.join("part/mid/new.json")).unwrap();
        std::fs::remove_file(&part).unwrap();
        std::fs::remove_dir(storage.join("part/mid")).unwrap();
        assert_eq!(unchanged(&r, &populated), reference(&populated));
        assert!(
            !unchanged(&r, &populated),
            "directory removal changes captured membership"
        );
    }

    #[test]
    fn cursor_cli_metadata_creation_and_rewrite_invalidate_consumers() {
        let world = crate::test_world("cursor-cli-source-metadata");
        let path = world.base.join("store.db");
        std::fs::write(&path, "database").unwrap();
        let r = crate::SessionRef::new("cli:session", crate::Harness::Cursor, &path);
        let before = super::Probe::default()
            .snapshot(&r)
            .with_reads(&super::ReadSet::default());
        let metadata = world.base.join("meta.json");
        assert!(super::unchanged(&r, &before));
        std::fs::write(&metadata, r#"{"title":"first"}"#).unwrap();
        assert!(!super::unchanged(&r, &before));
        let present = super::Probe::default()
            .snapshot(&r)
            .with_reads(&super::ReadSet::default());
        std::fs::write(&metadata, r#"{"title":"rewritten title"}"#).unwrap();
        assert!(!super::unchanged(&r, &present));
        let ide = crate::SessionRef::new("composer", crate::Harness::Cursor, &path);
        let cursor = crate::harness::rootless(crate::Harness::Cursor).unwrap();
        assert!(cursor.metadata_file(&ide).is_none());
    }

    #[test]
    fn selected_directory_primary_grows_without_blessing_metadata_changes() {
        let w = crate::test_world("source-directory-primary");
        let session = w.base.join("session");
        let agent = session.join("agents/agent-child");
        std::fs::create_dir_all(&agent).unwrap();
        let wire = agent.join("wire.jsonl");
        let state = session.join("state.json");
        std::fs::write(&wire, "prefix\n").unwrap();
        std::fs::write(&state, "{}").unwrap();
        let r = crate::SessionRef::new("session/agent-child", crate::Harness::KimiCode, &agent);
        let before = super::Probe::default()
            .snapshot_from(&r, &wire)
            .with_reads(&super::ReadSet::default());
        assert_eq!(before.primary, super::FileId::at(&wire));
        std::fs::write(&wire, "prefix\nappend\n").unwrap();
        let grown = super::Probe::default().snapshot_from(&r, &wire);
        assert_ne!(before.primary, grown.primary);
        assert!(before.dependencies_fresh(&grown, &mut super::Probe::default()));
        assert!(!before.fresh(&grown, &mut super::Probe::default()));
        assert!(!super::unchanged_from(&r, &wire, &before));
        std::fs::write(&state, "{\"title\":\"edited\"}").unwrap();
        let changed = super::Probe::default().snapshot_from(&r, &wire);
        assert!(!before.dependencies_fresh(&changed, &mut super::Probe::default()));
        // Missing controls are stamped too: later creation must invalidate the baseline.
        std::fs::remove_file(&state).unwrap();
        let absent = super::Probe::default()
            .snapshot_from(&r, &wire)
            .with_reads(&super::ReadSet::default());
        std::fs::write(&state, "{}").unwrap();
        assert!(!super::unchanged_from(&r, &wire, &absent));
    }

    #[test]
    fn serialized_consumer_reads_survive_growth_and_reject_dependency_changes() {
        let w = crate::test_world("source-read-state");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = crate::SessionRef::new("test", crate::Harness::Codex, &path);
        let extra = w.base.join("external.txt");
        let mut reads = super::ReadSet::default();
        reads.record(&extra);
        let captured = super::Probe::default().snapshot(&r).with_reads(&reads);
        let restored: super::Baseline = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
        assert!(restored.fresh(&super::Probe::default().snapshot(&r), &mut super::Probe::default()));
        std::fs::write(&path, "primary grows").unwrap();
        let now = super::Probe::default().snapshot(&r);
        assert!(!restored.fresh(&now, &mut super::Probe::default()));
        assert!(restored.dependencies_fresh(&now, &mut super::Probe::default()));
        let mut suffix = super::ReadSet::default();
        suffix.merge(&restored.reads().unwrap());
        assert!(now.with_reads(&suffix).fresh(&now, &mut super::Probe::default()));
        std::fs::write(&extra, "created").unwrap();
        assert!(!restored.dependencies_fresh(&super::Probe::default().snapshot(&r), &mut super::Probe::default()));
        suffix.merge(&super::ReadSet::unrecorded());
        assert!(now.with_reads(&suffix).stamp.is_none());
    }
    use super::*;

    #[test]
    fn relative_reads_remain_bound_to_the_capture_directory() {
        // cwd belongs to the process, so exercise the change in a child instead of racing the
        // workspace test runner's other adapters and file operations.
        const CHILD_ROOT: &str = "CV_SOURCE_RELATIVE_READS_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let r = SessionRef {
                harness: Harness::Codex,
                id: "relative".into(),
                path: root.join("primary.jsonl"),
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: None,
            };
            let base = Probe::default().snapshot(&r);
            let mut reads = ReadSet::default();
            reads.record(Path::new("external.txt"));
            assert!(reads.entries.keys().all(|path| path.is_absolute()));
            let captured = base.with_reads(&reads);
            update::<Done>("relative", &[(r.clone(), captured.clone())]);
            std::env::set_current_dir(root.join("other")).unwrap();
            assert!(unchanged(&r, &captured));
            assert!(Table::<Done>::load("relative")
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
            // A different same-named file in the new cwd is irrelevant to the original read.
            std::fs::write("external.txt", "unrelated").unwrap();
            assert!(Table::<Done>::load("relative")
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
            std::fs::write(root.join("external.txt"), "after!").unwrap();
            assert!(!unchanged(&r, &captured));
            assert!(!Table::<Done>::load("relative")
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
            return;
        }
        let w = crate::test_world("relative-source-reads");
        std::fs::write(w.base.join("primary.jsonl"), "primary").unwrap();
        std::fs::write(w.base.join("external.txt"), "before").unwrap();
        std::fs::create_dir(w.base.join("other")).unwrap();
        let status = w
            .command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "source::tests::relative_reads_remain_bound_to_the_capture_directory",
                "--nocapture",
            ])
            .env(CHILD_ROOT, &w.base)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn dynamic_reads_capture_absence_and_reject_changes_during_a_pass() {
        let w = crate::test_world("dynamic-reads");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let base = Probe::default().snapshot(&r);
        let extra = w.base.join("external.txt");
        let mut reads = ReadSet::default();
        reads.record(&extra);
        let before = base.with_reads(&reads);
        update::<Done>("dynamic", &[(r.clone(), before.clone())]);
        assert!(Table::<Done>::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
        std::fs::write(&extra, "before").unwrap();
        reads.record(&extra); // Keep the first capture, even after the dependent read changed.
        assert!(!unchanged(&r, &base.with_reads(&reads)));
        assert!(!Table::<Done>::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
        update::<Done>("dynamic", &[(r.clone(), before)]);
        assert!(!Table::<Done>::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
        let mut reads = ReadSet::default();
        reads.record(&extra);
        update::<Done>("dynamic", &[(r.clone(), base.with_reads(&reads))]);
        assert!(Table::<Done>::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
        std::fs::write(&extra, "after!").unwrap();
        assert!(!Table::<Done>::load("dynamic")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
        assert_eq!(Probe::default().snapshot(&r).primary, base.primary);
        assert!(!Table::<Done>::load("other")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&base, &mut Probe::default())));
    }

    #[test]
    fn unrecorded_and_older_records_reread_once() {
        #[derive(Default, Serialize, Deserialize)]
        struct Upgrade;
        impl Payload for Upgrade {
            const TAG: &'static str = "upgrade";
        }
        let w = crate::test_world("read-set-upgrade");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let r = SessionRef::new("test", Harness::Codex, path);
        let base = Probe::default().snapshot(&r);
        update::<Upgrade>("upgrade", &[(r.clone(), base.clone())]);
        assert!(
            Table::<Upgrade>::load("upgrade").get(&r).is_none(),
            "an unrecorded read set never publishes"
        );
        let complete = base.with_reads(&ReadSet::default());
        update::<Upgrade>("upgrade", &[(r.clone(), complete.clone())]);
        assert!(Table::<Upgrade>::load("upgrade")
            .admit(&r, &base, &mut Probe::default())
            .is_some());
        let conn = open().unwrap();
        conn.execute(
            "UPDATE source_dependencies SET record=CAST(json_set(CAST(record AS TEXT),'$.version',?1) AS BLOB) WHERE consumer='upgrade'",
            [VERSION - 1],
        )
        .unwrap();
        assert!(
            Table::<Upgrade>::load("upgrade").get(&r).is_none(),
            "an older record rereads"
        );
    }

    #[test]
    fn dependency_baselines_are_per_consumer_and_revalidated_at_publication() {
        let w = crate::test_world("source-baselines");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s1".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let mut probe = Probe::default();
        let before = probe.snapshot(&r).with_reads(&ReadSet::default());
        let other = SessionRef {
            id: "s2".into(),
            ..r.clone()
        };
        assert_eq!(
            probe.snapshot(&other).with_reads(&ReadSet::default()).stamp,
            before.stamp
        );
        assert_eq!(
            probe.entries.len(),
            3,
            "one main DB and absent WAL/journal probes shared by both sessions"
        );
        assert!(!Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&before, &mut Probe::default())));
        update::<Done>("counts", &[(r.clone(), before.clone())]);
        assert!(Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&before, &mut Probe::default())));
        assert!(!Table::<Done>::load("events")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&before, &mut Probe::default())));

        std::fs::write(w.base.join("sessions.db-wal"), "new WAL").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(after.primary, before.primary);
        assert_ne!(after.stamp, before.stamp);
        assert!(!unchanged(&r, &before));
        update::<Done>("counts", &[(r.clone(), before)]);
        assert!(
            !Table::<Done>::load("counts")
                .get(&r)
                .is_some_and(|old| old.baseline.fresh(&after, &mut Probe::default())),
            "a racing change cannot publish freshness"
        );
        update::<Done>("counts", &[(r.clone(), after.clone())]);
        assert!(Table::<Done>::load("counts")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&after, &mut Probe::default())));
        assert!(!Table::<Done>::load("events")
            .get(&r)
            .is_some_and(|old| old.baseline.fresh(&after, &mut Probe::default())));
    }
    #[test]
    fn scoped_children_track_wire_state_and_opencode_parts() {
        let w = crate::test_world("source-children");
        let session = w.base.join("session_test");
        std::fs::create_dir_all(session.join("agents/main")).unwrap();
        std::fs::write(session.join("state.json"), "before").unwrap();
        let wire = session.join("agents/main/wire.jsonl");
        std::fs::write(&wire, "line1\n").unwrap();
        let r = SessionRef {
            harness: Harness::KimiCode,
            id: "test".into(),
            path: session.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(&wire, "line1\nline2\n").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        // The finalized-step adapter now uses wire.jsonl as its concrete append primary.
        assert_ne!(before.primary, after.primary);
        assert!(before.dependencies_fresh(&after, &mut Probe::default()));
        std::fs::write(session.join("state.json"), "after!").unwrap();
        assert_ne!(
            after.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp
        );

        let storage = w.base.join("storage");
        for dir in ["session/project", "message/sid", "part/mid"] {
            std::fs::create_dir_all(storage.join(dir)).unwrap();
        }
        let path = storage.join("session/project/sid.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(storage.join("message/sid/noncanonical.json"), r#"{"id":"mid"}"#).unwrap();
        let part = storage.join("part/mid/pid.json");
        std::fs::write(&part, "before").unwrap();
        let r = SessionRef {
            harness: Harness::OpenCode,
            id: "sid".into(),
            path,
            ..r
        };
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(&part, "after!").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        assert_eq!(before.primary, after.primary);
        assert_ne!(before.stamp, after.stamp, "part-only same-size edit");
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_ne!(
            after.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp,
            "part membership"
        );
        // The stream uses the part root for an absent message ID too. Dependency caching must
        // retain that default, even when the message filename could suggest a different ID.
        std::fs::write(storage.join("message/sid/noncanonical.json"), r#"{"role":"user"}"#).unwrap();
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        std::fs::write(storage.join("part/default.json"), "new").unwrap();
        assert_ne!(
            before.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp,
            "missing IDs still track their default part directory"
        );
    }

    #[test]
    fn counts_publish_captured_primary_and_a_concurrent_edit_stays_stale() {
        let w = crate::test_world("source-counts-publish");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        crate::catalog::open_db().unwrap();
        let source = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        crate::catalog::note_affordances_with_source(&r, &source, || {
            // Even an edit restoring the original tuple must not bless stale consumer output.
            std::fs::write(&path, "after!").unwrap();
            #[cfg(unix)]
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            #[cfg(not(unix))]
            let _ = modified;
            crate::affordance::Counts::new()
        });
        assert!(crate::catalog::AffordanceTable::load().stale(&r));
    }
    #[test]
    fn failed_stamp_publication_cannot_reuse_an_older_baseline() {
        let w = crate::test_world("source-publication-failure");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let conn = crate::catalog::open_db().unwrap();
        let before = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        assert!(!crate::catalog::AffordanceTable::load().stale(&r));
        let wal = w.base.join("sessions.db-wal");
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        conn.execute_batch(
            "CREATE TRIGGER reject_stamp BEFORE INSERT ON source_dependencies
            WHEN NEW.consumer='counts' BEGIN SELECT RAISE(FAIL,'stamp rejected'); END;",
        )
        .unwrap();
        crate::catalog::note_affordances_with_source(&r, &after, || {
            [(crate::affordance::Kind::Plan, 1)].into_iter().collect()
        });
        std::fs::remove_file(&wal).unwrap();
        assert_eq!(
            before.stamp,
            Probe::default().snapshot(&r).with_reads(&ReadSet::default()).stamp
        );
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed stamp insert rolls back new counts too"
        );
        conn.execute_batch("DROP TRIGGER reject_stamp;").unwrap();
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        conn.execute_batch(
            "CREATE TRIGGER reject_invalidation BEFORE DELETE ON source_dependencies
            WHEN OLD.consumer='counts' BEGIN SELECT RAISE(FAIL,'delete rejected'); END;",
        )
        .unwrap();
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
        let mut called = false;
        crate::catalog::note_affordances_with_source(&r, &after, || {
            called = true;
            crate::affordance::Counts::new()
        });
        assert!(called);
        std::fs::remove_file(&wal).unwrap();
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed invalidation rolls back consumer output mutation"
        );
    }

    #[test]
    fn a_folded_agents_stamp_declares_the_parents_tool_results_by_identity() {
        // From akapug's PR #25: one agent row carried 2,198 of its parent's spilled files while its
        // parse had read 25. The agent declares the directory once; the files it read are in its
        // read set.
        let world = crate::test_world("folded-agent-tool-results");
        let project = world.base.join("claude").join("projects").join("-slug");
        let session = project.join("sess");
        let subs = session.join("subagents");
        let spilled = session.join("tool-results");
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::create_dir_all(&spilled).unwrap();
        std::fs::write(session.with_extension("jsonl"), "{}\n").unwrap();
        for name in ["x.txt", "y.txt", "z.txt"] {
            std::fs::write(spilled.join(name), "spilled tool output").unwrap();
        }
        let agent_path = subs.join("agent-a1.jsonl");
        std::fs::write(&agent_path, "{}\n").unwrap();

        let parent = SessionRef::new("sess", Harness::Claude, session.with_extension("jsonl"));
        let agent = SessionRef::new("agent-a1", Harness::Claude, &agent_path);
        let parent_entries = Probe::default().dependency_entries(&parent, &parent.path).unwrap();
        let agent_entries = Probe::default().dependency_entries(&agent, &agent.path).unwrap();

        assert!(
            parent_entries.iter().any(|e| e.path == spilled.join("x.txt")),
            "the parent still inventories its own tool-results: {parent_entries:?}"
        );
        assert!(
            agent_entries.iter().any(|e| e.path == spilled),
            "the agent declares the parent's tool-results directory: {agent_entries:?}"
        );
        assert!(
            !agent_entries
                .iter()
                .any(|e| e.path.starts_with(&spilled) && e.path != spilled),
            "the agent must not copy the parent's file inventory: {agent_entries:?}"
        );

        // A file appearing there still invalidates the agent: the directory's own identity moves.
        let before = Probe::default().snapshot(&agent).with_reads(&ReadSet::default());
        std::fs::write(spilled.join("new.txt"), "later output").unwrap();
        assert!(!unchanged(&agent, &before), "a new spilled file invalidates the agent");
        std::fs::remove_file(spilled.join("new.txt")).unwrap();

        // A rewrite of a file the agent read invalidates it too, carried by the read set.
        let mut reads = ReadSet::default();
        reads.record(&spilled.join("x.txt"));
        let read_before = Probe::default().snapshot(&agent).with_reads(&reads);
        std::fs::write(spilled.join("x.txt"), "rewritten tool output").unwrap();
        assert!(
            !unchanged(&agent, &read_before),
            "a read file's rewrite invalidates the agent"
        );
    }
}
