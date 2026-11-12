//! Complete JSON values with explicit owned and read-only source representations.
//!
//! Generic Serde serialization always emits the actual value, resolving a selected source value
//! when necessary. Consumers that only need projections or compact serialized length can use
//! [`Json::with_view`], [`Json::canonical_len`] and [`Json::write_canonical`] without constructing
//! a descendant value tree. Encoded argument documents use operation-owned decoded backing.
//!
//! A source view reads one byte source ([`RangeSource`]): the demanded range mapped, a line
//! already in memory, or a positioned file read in bounded windows. The canonical walker bounds
//! decoded string scratch in both builds; object key/order state grows with object width.
//! Neither limitation is hidden by this API.

mod transport;
pub(crate) use transport::{SourceDescriptor, SourceRegistry};

pub(crate) use crate::harness::store::document::range::Validation;
use crate::harness::store::document::range::{scoped, Range, RangeSource};
use crate::source::{FileId, Prefix};
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use std::borrow::Cow;
use std::fs::Metadata;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A complete JSON value. Source descriptors clone cheaply; owned values retain Value semantics.
/// Deserialization always produces `Owned`, including objects that resemble source descriptors.
/// Equality is the record replay's "unchanged" test: owned values by value, source descriptors
/// by range (the same bytes of the same captured source), never an owned value against a range.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Owned(Value),
    Source(Arc<JsonRange>),
}

/// A validated complete value range, tied to a read-only source identity. Fields are private so
/// callers cannot create an unchecked range. Normal edits, appends and replacement invalidate it.
/// Metadata checks do not provide an immutable snapshot against arbitrary external in-place
/// rewrites or prevent external truncation of a mapped file.
#[derive(Debug)]
pub struct JsonRange {
    source: JsonSource,
    offset: u64,
    len: u64,
    decoded: Option<Arc<crate::harness::store::bytes::ByteSource>>,
}

/// The same bytes of the same captured source (and the same decoded backing, by identity).
impl PartialEq for JsonRange {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.offset == other.offset
            && self.len == other.len
            && match (&self.decoded, &other.decoded) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}
impl Eq for JsonRange {}

/// Distinguish invalid content (archives historically appear empty) from unavailable or changed
/// sources, whose failures must abort consumers and cannot publish freshness.
#[derive(Debug)]
pub(crate) struct InvalidJson;
impl std::fmt::Display for InvalidJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid JSON document")
    }
}
impl std::error::Error for InvalidJson {}

/// Capture before an adapter validates its source. Only already accepted complete values may use
/// validated_range afterward; this avoids validating each giant input again at producer handoff.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct JsonSource(Arc<CapturedSource>);
/// Per-session SQLite freshness guard for shared WAL stores: while the pinned MVCC pass is live,
/// reads come from one SQLite snapshot; once the pass finishes (and whenever lazy source ranges
/// resolve later), the session's own revision and schema version are re-verified.
pub(crate) struct SqliteSessionGuard {
    in_pass: std::sync::atomic::AtomicBool,
    valid: Box<dyn Fn() -> bool + Send + Sync>,
}

impl std::fmt::Debug for SqliteSessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSessionGuard")
            .field("in_pass", &self.in_pass.load(std::sync::atomic::Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SqliteSessionGuard {
    #[cfg(feature = "sqlite")]
    pub(crate) fn new(valid: impl Fn() -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            in_pass: std::sync::atomic::AtomicBool::new(true),
            valid: Box::new(valid),
        })
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn end_pass(&self) {
        self.in_pass.store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn valid(&self) -> bool {
        if self.in_pass.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        (self.valid)()
    }
}

#[derive(Debug)]
struct CapturedSource {
    path: PathBuf,
    identity: FileId,
    /// Line-delimited logs are append-only by contract: a writer may still be adding records
    /// after capture. For them the captured prefix's sample is kept, so growth of the same file
    /// with that prefix intact is not a change ([`FileId::admits`]); truncation, replacement and
    /// rewrites of the sampled bytes still are. Readers stop at the captured length.
    prefix: Option<Prefix>,
    /// The last observation admitted against `prefix`: the same one again needs no read.
    admitted: std::sync::Mutex<Option<FileId>>,
    reads: Option<Arc<crate::source::ReadSet>>,
    sqlite_guard: Option<Arc<SqliteSessionGuard>>,
    decoded: std::sync::Mutex<Option<Arc<crate::harness::store::bytes::ByteSource>>>,
}
impl PartialEq for CapturedSource {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.identity == other.identity
            && self.reads == other.reads
            && match (&self.sqlite_guard, &other.sqlite_guard) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}
impl Eq for CapturedSource {}
impl std::hash::Hash for CapturedSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.identity.hash(state);
        self.reads.hash(state);
        self.sqlite_guard.as_ref().map(Arc::as_ptr).hash(state);
    }
}
/// Extensions of line-delimited logs that harnesses append to while a session is live.
const APPEND_ONLY_EXTENSIONS: &[&str] = &["jsonl", "ndjson"];
fn is_append_only_log(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| APPEND_ONLY_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}
fn file_id(metadata: &Metadata) -> Result<FileId> {
    anyhow::ensure!(metadata.is_file(), "JSON source is not a regular file");
    FileId::of(metadata).context("JSON source has no modification time")
}
/// Bytes for one checked JSON read. Decoded buffers retain their original source identity.
pub(crate) enum ReadInput<'a> {
    Document,
    Range(u64, u64),
    Backing(Arc<crate::harness::store::bytes::ByteSource>, u64, u64),
    Record(
        u64,
        &'a str,
        Option<(&'a Arc<crate::harness::store::bytes::ByteSource>, u64)>,
    ),
    Positioned(
        &'a RangeSource<'a>,
        u64,
        u64,
        Option<(&'a Arc<crate::harness::store::bytes::ByteSource>, u64)>,
    ),
}

impl JsonSource {
    pub(crate) fn decoded_backing(&self) -> Result<Arc<crate::harness::store::bytes::ByteSource>> {
        let mut decoded = self
            .0
            .decoded
            .lock()
            .map_err(|_| std::io::Error::other("decoded backing poisoned"))?;
        if let Some(bytes) = decoded.as_ref() {
            return Ok(bytes.clone());
        }
        let bytes = Arc::new(crate::harness::store::bytes::ByteSource::arena()?);
        *decoded = Some(bytes.clone());
        Ok(bytes)
    }

    pub(crate) fn capture(path: &Path) -> Result<Self> {
        Self::capture_with_reads(path, &crate::source::ReadSet::default())
    }
    pub(crate) fn capture_with_reads(path: &Path, reads: &crate::source::ReadSet) -> Result<Self> {
        let source = Self::capture_guarded(path, Some(Arc::new(reads.clone())), None)?;
        // The capture just stat'ed the path; only the dependencies are older than it.
        source.check_reads()?;
        Ok(source)
    }
    #[cfg(feature = "sqlite")]
    pub(crate) fn capture_with_sqlite_guard(
        path: &Path,
        reads: &crate::source::ReadSet,
        guard: Arc<SqliteSessionGuard>,
    ) -> Result<Self> {
        let source = Self::capture_guarded(path, Some(Arc::new(reads.clone())), Some(guard))?;
        source.check()?;
        Ok(source)
    }
    fn capture_guarded(
        path: &Path,
        reads: Option<Arc<crate::source::ReadSet>>,
        sqlite_guard: Option<Arc<SqliteSessionGuard>>,
    ) -> Result<Self> {
        // Preserve symlink spelling so later retargeting changes the observed identity.
        let path = std::path::absolute(path).context("locating JSON source")?;
        let (identity, prefix) = if is_append_only_log(&path) {
            let file = std::fs::File::open(&path).with_context(|| format!("reading JSON source {}", path.display()))?;
            let identity = file_id(&file.metadata()?)?;
            (
                identity,
                Some(Prefix::of(&file, identity.len).context("sampling JSON source")?),
            )
        } else {
            let metadata =
                std::fs::metadata(&path).with_context(|| format!("reading JSON source {}", path.display()))?;
            (file_id(&metadata)?, None)
        };
        Ok(Self(Arc::new(CapturedSource {
            path,
            identity,
            prefix,
            admitted: std::sync::Mutex::new(None),
            reads,
            sqlite_guard,
            decoded: std::sync::Mutex::new(None),
        })))
    }
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(guard) = &self.0.sqlite_guard {
            anyhow::ensure!(
                guard.valid(),
                "SQLite source dependencies changed or unavailable: {}",
                self.0.path.display()
            );
            return Ok(());
        }
        let metadata = std::fs::metadata(&self.0.path)
            .with_context(|| format!("reading JSON source {}", self.0.path.display()))?;
        self.check_metadata(metadata)
    }
    /// After a sink callback that began at `started` and received a descriptor of this source:
    /// recheck if the callback was slow ([`SLOW_CALLBACK`](crate::harness::store::jsonl::Read::SLOW_CALLBACK));
    /// a fast one is covered by the reader's EOF check.
    pub(crate) fn check_after(&self, started: std::time::Instant) -> Result<()> {
        if started.elapsed() < crate::harness::store::jsonl::Read::SLOW_CALLBACK {
            return Ok(());
        }
        self.check()
    }
    /// Whether the file as observed now still carries every byte captured: identical, or for an
    /// append-only log admitted as growth ([`FileId::admits`]).
    fn holds(&self, now: &FileId) -> bool {
        if *now == self.0.identity {
            return true;
        }
        let Some(prefix) = &self.0.prefix else { return false };
        let mut admitted = self.0.admitted.lock().unwrap_or_else(|poison| poison.into_inner());
        if *admitted == Some(*now) {
            return true;
        }
        let holds = self.0.identity.admits(prefix, now, &self.0.path);
        if holds {
            *admitted = Some(*now);
        }
        holds
    }
    fn check_reads(&self) -> Result<()> {
        if let Some(reads) = &self.0.reads {
            anyhow::ensure!(
                reads.fresh(&mut crate::source::Probe::default()),
                "JSON source dependencies changed or unavailable: {}",
                self.0.path.display()
            );
        }
        Ok(())
    }
    /// [`Self::capture`] without the dependency check: one stat, for a parse-time stamp.
    pub(crate) fn capture_identity(path: &Path) -> Result<Self> {
        Self::capture_guarded(path, None, None)
    }
    /// `later` is a capture of the same path that still carries every byte of this one: identical,
    /// or for an append-only log admitted as growth ([`FileId::admits`]).
    pub(crate) fn admits(&self, later: &JsonSource) -> bool {
        self.0.path == later.0.path && self.holds(&later.0.identity)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
    pub(crate) fn len(&self) -> u64 {
        self.0.identity.len
    }
    pub(crate) fn check_metadata(&self, metadata: Metadata) -> Result<()> {
        anyhow::ensure!(
            self.holds(&file_id(&metadata)?),
            "JSON source changed: {}",
            self.0.path.display()
        );
        self.check_reads()
    }
    fn range(&self, offset: u64, len: u64) -> Result<JsonRange> {
        anyhow::ensure!(len != 0, "JSON range is empty");
        anyhow::ensure!(
            offset.checked_add(len).is_some_and(|end| end <= self.0.identity.len),
            "JSON range is out of bounds"
        );
        Ok(JsonRange {
            source: self.clone(),
            offset,
            len,
            decoded: None,
        })
    }
    /// Read one view under an explicit acceptance policy. The input selects bytes and
    /// provenance; validation and the callback/error lifecycle are shared by every reader.
    pub(crate) fn read<T>(
        &self,
        input: ReadInput<'_>,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        match input {
            ReadInput::Document => {
                if self.len() == 0 {
                    return Err(InvalidJson.into());
                }
                self.range(0, self.len())?.with_validation(validation, f)
            }
            ReadInput::Range(offset, len) => self.range(offset, len)?.with_validation(validation, f),
            ReadInput::Backing(backing, offset, len) => {
                self.backing_range(backing, offset, len)?.with_validation(validation, f)
            }
            ReadInput::Record(offset, text, decoded) => {
                let range = match decoded {
                    Some((backing, len)) => self.decoded_range(backing.clone(), len, offset, text.len() as u64)?,
                    None => self.range(offset, text.len() as u64)?,
                };
                let context = RangeSource::text(Some(self.clone()), text, offset, decoded.is_none());
                range.view_unchecked(&context, validation, f)
            }
            ReadInput::Positioned(context, offset, len, decoded) => {
                let range = match decoded {
                    Some((backing, backing_len)) => self.decoded_range(backing.clone(), backing_len, offset, len)?,
                    None => {
                        anyhow::ensure!(
                            context.matches_original(self),
                            "JSON positioned context has different provenance"
                        );
                        self.range(offset, len)?
                    }
                };
                range.with_source(context, validation, f)
            }
        }
    }
    /// Only for store::text::Builder's successfully completed quoted JSON-string output.
    /// Its checked string/canonical writers guarantee strict UTF8/escape validity before this
    /// call; arbitrary decoded JSON and external source ranges must use validated readers.
    pub(crate) fn generated_string_text(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
        decoded_len: usize,
    ) -> Result<crate::lazy::Text> {
        let range = self.backing_range(backing, offset, len)?;
        if len > crate::lazy::INLINE_MAX as u64 {
            // The controlled producer has completed and strictly encoded this entire string.
            // Construct its descriptor without remapping/scanning the bytes it just emitted.
            range.check_retained_bounds()?;
            Ok(crate::lazy::Text::Source(crate::lazy::SourceText::generated(
                Arc::new(range),
                decoded_len,
            )))
        } else {
            range.with_validation(Validation::Cached, |view| {
                view.source_text()?.context("constructed JSON string")
            })
        }
    }

    fn backing_range(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        offset: u64,
        len: u64,
    ) -> Result<JsonRange> {
        self.check()?;
        let backing_len = backing.byte_len()?;
        self.decoded_range(backing, backing_len, offset, len)
    }
    fn decoded_range(
        &self,
        backing: Arc<crate::harness::store::bytes::ByteSource>,
        backing_len: u64,
        offset: u64,
        len: u64,
    ) -> Result<JsonRange> {
        anyhow::ensure!(
            len != 0 && offset.checked_add(len).is_some_and(|end| end <= backing_len),
            "backing range out of bounds"
        );
        if backing.is_arena() {
            let mut decoded = self
                .0
                .decoded
                .lock()
                .map_err(|_| std::io::Error::other("decoded backing poisoned"))?;
            if decoded.is_none() {
                *decoded = Some(backing.clone());
            }
        }
        Ok(JsonRange {
            source: self.clone(),
            offset,
            len,
            decoded: Some(backing),
        })
    }
    pub(crate) fn metadata_bytes(&self) -> usize {
        std::mem::size_of::<CapturedSource>() + 2 * std::mem::size_of::<usize>() + self.0.path.capacity()
    }
    /// Caller must have syntax AND Value-compatible scalar/depth validation for this complete
    /// range since capture. Quotes/containers are included; surrounding whitespace is excluded.
    /// Never use this for a decoded JSON string that represents a different IR argument value.
    pub(crate) fn validated_range(&self, offset: u64, len: u64) -> Result<Json> {
        self.check()?;
        Ok(Json::Source(Arc::new(self.range(offset, len)?)))
    }
}

impl JsonRange {
    /// The source still holds its capture and the decoded backing still holds this range. The
    /// backing is the arena, not the source, so one source check covers both.
    pub(crate) fn check_retained_bounds(&self) -> Result<()> {
        self.source.check()?;
        if let Some(bytes) = &self.decoded {
            let len = bytes.byte_len()?;
            anyhow::ensure!(
                self.offset.checked_add(self.len).is_some_and(|end| end <= len),
                "source text backing is truncated"
            );
        }
        Ok(())
    }

    /// Captured auxiliary source dependencies, retaining original stamps and absence.
    /// Consumers merge these records into their cache guards; `path()` remains separate so
    /// append-aware consumers can handle their primary transcript identity independently.
    pub fn dependencies(&self) -> Option<&crate::source::ReadSet> {
        self.source.0.reads.as_deref()
    }
    fn child(&self, offset: u64, len: u64) -> Result<Self> {
        anyhow::ensure!(
            len != 0
                && offset >= self.offset
                && offset.checked_add(len).is_some_and(|end| end <= self.offset + self.len),
            "JSON child out of bounds"
        );
        Ok(Self {
            source: self.source.clone(),
            offset,
            len,
            decoded: self.decoded.clone(),
        })
    }

    /// Reuse this range's original capture when building checked composite text. Recapturing
    /// its path would discard the identity/dependency boundary that accepted these bytes.
    pub(crate) fn captured_source(&self) -> &JsonSource {
        &self.source
    }

    /// Original transcript provenance; decoded argument ranges refer to private backing bytes.
    pub fn path(&self) -> &Path {
        &self.source.0.path
    }
    /// Offset in the backing document (which may be a decoded argument document).
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn extent(&self) -> Range {
        Range {
            offset: self.offset,
            len: self.len,
        }
    }

    pub(crate) fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        self.with_validation(Validation::Cached, f)
    }
    /// View this range through its own byte source: the range mapped, or the decoded bytes, or a
    /// positioned read ([`RangeSource::scoped`]), with identity checked around the projection.
    fn with_validation<T>(
        &self,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        self.source.check()?;
        scoped(&self.source, self.decoded.as_deref(), self.extent(), |source| {
            self.with_source(source, validation, f)
        })
    }
    /// View this range through a byte source the caller holds, checking identity after the
    /// projection even when the callback stopped early: an intentional staged-output cap must
    /// not hide a source mutation, and a source failure takes precedence over a callback error.
    fn with_source<T>(
        &self,
        source: &RangeSource<'_>,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let result = self.view_unchecked(source, validation, f);
        source.check_identity()?;
        result
    }
    fn view_unchecked<T>(
        &self,
        source: &RangeSource<'_>,
        validation: Validation,
        f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>,
    ) -> Result<T> {
        let value = source.accept(self.extent(), validation)?.ok_or(InvalidJson)?;
        f(JsonView(View::Source(SourceView {
            source,
            owner: Some(self),
            value,
        })))
    }
}

pub(crate) struct AcceptedArray<'a>(JsonArray<'a>);
impl<'a> Iterator for AcceptedArray<'a> {
    type Item = Result<Option<JsonView<'a>>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|value| {
            let value = value?;
            Ok(value.accepts_value()?.then_some(value))
        })
    }
}

impl From<Value> for Json {
    fn from(value: Value) -> Self {
        Self::Owned(value)
    }
}
impl Default for Json {
    fn default() -> Self {
        Self::Owned(Value::Null)
    }
}
impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.resolve().map_err(serde::ser::Error::custom)?.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Owned)
    }
}
impl Json {
    /// Validate a complete JSON range before constructing a source descriptor. Missing files,
    /// overflow, truncated ranges and values rejected by serde_json Value return errors.
    pub fn source(path: impl AsRef<Path>, offset: u64, len: u64) -> Result<Self> {
        Self::source_with_reads(path, offset, len, &crate::source::ReadSet::default())
    }
    /// Validate a complete JSON range under an already captured auxiliary dependency guard.
    /// Guarded dependencies (including missing conditional files) must still be fresh.
    pub fn source_with_reads(
        path: impl AsRef<Path>,
        offset: u64,
        len: u64,
        reads: &crate::source::ReadSet,
    ) -> Result<Self> {
        let source = JsonSource::capture_with_reads(path.as_ref(), reads)?;
        let range = source.range(offset, len)?;
        let normalized = scoped(&source, None, range.extent(), |context| {
            let normalized = context
                .accept(range.extent(), Validation::Value)?
                .context("JSON range is not a valid serde_json Value")?;
            context.check_identity()?;
            Ok(normalized)
        })?;
        source.validated_range(normalized.offset, normalized.len)
    }
    /// Select a child value without materializing its descendants. Source children retain their
    /// validated range and source identity; missing keys/nonobjects remain distinct from errors.
    pub fn select(&self, key: &str) -> Result<Option<Self>> {
        self.with_view(|view| view.get(key)?.map(JsonView::to_json).transpose())
    }

    /// Borrow owned data or materialize the selected complete source value. This is deliberately
    /// explicit: there is no implicit Deref that builds a giant tree or substitutes missing data.
    pub fn resolve(&self) -> Result<Cow<'_, Value>> {
        match self {
            Self::Owned(value) => Ok(Cow::Borrowed(value)),
            Self::Source(range) => range.with_view(|view| Ok(Cow::Owned(view.resolve()?.into_owned()))),
        }
    }
    /// Resolve before mutation. A failed read leaves the source descriptor intact.
    pub fn make_owned(&mut self) -> Result<&mut Value> {
        if let Self::Source(_) = self {
            *self = Self::Owned(self.resolve()?.into_owned());
        }
        match self {
            Self::Owned(value) => Ok(value),
            Self::Source(_) => unreachable!(),
        }
    }
    /// A scoped view cannot outlive its backing mapping/buffer. All source read/validation errors
    /// are distinct from a missing field; a changed source also rejects the completed operation.
    pub fn with_view<T>(&self, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        match self {
            Self::Owned(value) => f(JsonView(View::Owned(value))),
            Self::Source(range) => range.with_view(f),
        }
    }
    /// Byte length of Value's compact serialization, preserving insertion order and duplicates'
    /// final values at their first key position. Does not materialize descendant strings.
    pub fn canonical_len(&self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// A compact JSON byte prefix ending on a Unicode scalar boundary, plus whether the complete
    /// value was truncated. Source validation and the final identity check still apply. No more
    /// than max_bytes of output are retained; the prefix need not itself be complete JSON.
    pub fn canonical_prefix(&self, max_bytes: usize) -> Result<(String, bool)> {
        self.with_view(|view| view.canonical_prefix(max_bytes))
    }
    /// Emit the exact compact JSON that serde_json serializes from the complete Value.
    ///
    /// An I/O or source-change error may occur after a prefix has been written. Callers must
    /// discard/reset staged output on Err and must not publish consumer freshness for that pass.
    /// Generic Serialize instead resolves the selected value, as serializers cannot stream pieces
    /// of one string. This explicit writer keeps decoded leaf scratch bounded.
    pub fn write_canonical(&self, writer: &mut impl Write) -> Result<()> {
        self.with_view(|view| view.write_canonical(writer))
    }
}

#[derive(Clone, Copy)]
enum View<'a> {
    Owned(&'a Value),
    Source(SourceView<'a>),
}
/// A value range of one byte source. `owner` is the accepted range it was selected from, when it
/// has one (a view over bytes with no captured source cannot become a `Json::Source`).
#[derive(Clone, Copy)]
struct SourceView<'a> {
    source: &'a RangeSource<'a>,
    owner: Option<&'a JsonRange>,
    value: Range,
}
/// A typed projection of an already validated complete JSON value, scoped by Json::with_view.
#[derive(Clone, Copy)]
pub struct JsonView<'a>(View<'a>);
impl<'a> From<&'a Value> for JsonView<'a> {
    fn from(value: &'a Value) -> Self {
        Self(View::Owned(value))
    }
}
impl<'a> JsonView<'a> {
    #[cfg(any(feature = "sqlite", test))]
    pub(crate) fn owned(value: &'a Value) -> Self {
        Self(View::Owned(value))
    }
    /// A view of `value` within `source`, with no accepted range to own children.
    pub(crate) fn of(source: &'a RangeSource<'a>, value: Range) -> Self {
        Self(View::Source(SourceView {
            source,
            owner: None,
            value,
        }))
    }
    fn at(self, value: Range) -> Self {
        match self.0 {
            View::Source(source) => Self(View::Source(SourceView { value, ..source })),
            View::Owned(_) => unreachable!(),
        }
    }

    /// Child records retain their original independent Value-compatible acceptance boundary.
    pub(crate) fn accepted_array(self) -> Result<Option<AcceptedArray<'a>>> {
        Ok(self.array()?.map(AcceptedArray))
    }
    pub(crate) fn accepts_value(self) -> Result<bool> {
        self.accepts_value_at_depth(0)
    }
    pub(crate) fn accepts_value_at_depth(self, depth: usize) -> Result<bool> {
        match self.0 {
            View::Owned(_) => Ok(true),
            View::Source(source) => source.source.accepts_value_at_depth(source.value, depth),
        }
    }
    pub(crate) fn select_fields(self, keys: &[&str]) -> Result<Option<Vec<Option<Self>>>> {
        let mut fields = vec![None; keys.len()];
        let complete = self.select_each(keys, false, &mut |i, value| {
            fields[i] = Some(value);
            Ok(())
        })?;
        Ok(complete.then_some(fields))
    }
    /// Visit requested fields; the last visit of a key wins. False for a nonobject.
    pub(crate) fn select_each(
        self,
        keys: &[&str],
        unique: bool,
        visit: &mut dyn FnMut(usize, Self) -> Result<()>,
    ) -> Result<bool> {
        match self.0 {
            View::Source(source) => source
                .source
                .select_each(source.value, keys, unique, &mut |i, value| visit(i, self.at(value))),
            View::Owned(Value::Object(object)) => {
                for (i, key) in keys.iter().enumerate() {
                    if let Some(value) = object.get(*key) {
                        visit(i, Self(View::Owned(value)))?;
                    }
                }
                Ok(true)
            }
            View::Owned(_) => Ok(false),
        }
    }
    pub(crate) fn source_range(self) -> Result<Option<(u64, u64)>> {
        Ok(match self.0 {
            View::Owned(_) => None,
            View::Source(source) => Some((source.value.offset, source.value.len)),
        })
    }
    /// Retain the selected string independently of a session's implicit primary path.
    pub fn source_text(self) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        if self
            .source_range()?
            .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            if let Json::Source(range) = self.to_json()? {
                return Ok(Some(crate::lazy::Text::Source(crate::lazy::SourceText::new(range))));
            }
        }
        self.text(false)
    }
    /// Transcript text by the codec's span rule: a string whose *decoded* length exceeds
    /// INLINE_MAX stays in the source when `spans` is demanded, and anything else is inline.
    /// Both builds keep it as checked [`SourceText`](crate::lazy::SourceText) in its file, held
    /// to the read's capture (same file, only grown). None for a nonstring.
    pub(crate) fn lazy_text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        self.text(spans && self.exceeds_inline()?)
    }
    /// [`lazy_text`](Self::lazy_text) for a store whose strings live in many files or cells:
    /// text that stays in the source is a checked source text naming its own bytes, never a span
    /// of the session's primary file.
    pub(crate) fn lazy_source_text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        if !self.is_string()? {
            return Ok(None);
        }
        if spans && self.exceeds_inline()? {
            return self.source_text();
        }
        self.text(false)
    }
    /// The span rule: a string whose decoded length exceeds INLINE_MAX.
    fn exceeds_inline(self) -> Result<bool> {
        let Some((_, len)) = self.source_range()? else {
            return Ok(false);
        };
        if len - 2 <= crate::lazy::INLINE_MAX as u64 {
            return Ok(false);
        }
        // A JSON escape is at most six bytes per decoded byte: only near-threshold escaped
        // strings need their decoded length, and those are at most 24 KiB of input.
        if len - 2 > 6 * crate::lazy::INLINE_MAX as u64 {
            return Ok(true);
        }
        let mut decoded = 0;
        self.for_each_char(|c| {
            decoded += c.len_utf8();
            decoded <= crate::lazy::INLINE_MAX
        })?;
        Ok(decoded > crate::lazy::INLINE_MAX)
    }
    /// Visit a string's decoded characters until `visit` returns false; false for a nonstring.
    pub(crate) fn for_each_char(self, mut visit: impl FnMut(char) -> bool) -> Result<bool> {
        Ok(crate::stream::stopped(self.for_each_str_chunk(|chunk| {
            for c in chunk.chars() {
                if !visit(c) {
                    return Err(crate::stream::Stop.into());
                }
            }
            Ok(())
        }))?
        .unwrap_or(true))
    }
    /// A string, inline or, with `spans` and a body over [`INLINE_MAX`](crate::lazy::INLINE_MAX),
    /// left in its source as checked [`SourceText`](crate::lazy::SourceText): the session's
    /// primary transcript like any other file, cell or decoded backing.
    pub(crate) fn text(self, spans: bool) -> Result<Option<crate::lazy::Text>> {
        let View::Source(source) = self.0 else {
            let View::Owned(value) = self.0 else { unreachable!() };
            return Ok(value.as_str().map(|s| crate::lazy::Text::Inline(s.to_owned())));
        };
        if source.source.kind(source.value)? != Some(b'"') {
            return Ok(None);
        }
        let primary = source.owner.is_some_and(|owner| owner.decoded.is_none());
        if spans && !primary {
            return self.source_text();
        }
        if spans && source.value.len - 2 > crate::lazy::INLINE_MAX as u64 {
            if let Json::Source(range) = self.to_json()? {
                return Ok(Some(crate::lazy::Text::Source(crate::lazy::SourceText::new(range))));
            }
        }
        Ok(Some(crate::lazy::Text::Inline(
            source.source.materialize(source.value)?,
        )))
    }
    pub(crate) fn argument(self, spans: bool) -> Result<Json> {
        if spans
            && self
                .source_range()?
                .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            self.to_json()
        } else {
            Ok(Json::Owned(self.resolve()?.into_owned()))
        }
    }
    /// Decode OpenAI's encoded argument document into a private bounded backing. Invalid,
    /// partial and scalar arguments retain the original string representation.
    pub(crate) fn encoded_argument(self) -> Result<Json> {
        if self.is_null()? {
            return Ok(serde_json::json!({}).into());
        }
        if !self.is_string()? {
            return self.to_json();
        }
        if !self
            .source_range()?
            .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
        {
            return Ok(crate::harness::wire::openai::parse_arguments(Some(self.resolve()?.as_ref())).into());
        }
        let original = self.to_json()?;
        let Json::Source(range) = &original else {
            return Ok(crate::harness::wire::openai::parse_arguments(Some(self.resolve()?.as_ref())).into());
        };
        let bytes = range.source.decoded_backing()?;
        let mut blank = true;
        let (offset, len) = bytes.append(|writer| {
            self.for_each_str_chunk(|part| {
                blank &= part.trim().is_empty();
                writer.write_all(part.as_bytes())?;
                Ok(())
            })?;
            Ok(())
        })?;
        if blank {
            return Ok(serde_json::json!({}).into());
        }
        match range
            .source
            .read(ReadInput::Backing(bytes, offset, len), Validation::Value, |view| {
                if view.is_object()? || view.is_array()? {
                    Ok(Some(view.to_json()?))
                } else {
                    Ok(None)
                }
            }) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Ok(original),
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => Ok(original),
            Err(error) => Err(error),
        }
    }
    /// A nonobject or missing field is None; backing-source failures occur before this view exists.
    pub fn get(self, key: &str) -> Result<Option<Self>> {
        Ok(match self.0 {
            View::Owned(value) => value.get(key).map(|value| Self(View::Owned(value))),
            View::Source(source) => source.source.get(source.value, key)?.map(|value| self.at(value)),
        })
    }
    /// Conservative printable-ASCII presence without decoding a retained JSON string.
    /// False proves absence; escaped backslashes may create harmless false positives.
    pub(crate) fn string_may_contain_ascii(self, byte: u8) -> Result<bool> {
        match self.0 {
            View::Owned(value) => {
                anyhow::ensure!((0x20..=0x7e).contains(&byte), "expected printable ASCII");
                Ok(value.as_str().is_some_and(|s| s.as_bytes().contains(&byte)))
            }
            View::Source(source) => {
                if source.source.kind(source.value)? != Some(b'"') {
                    return Ok(false);
                }
                source.source.string_may_contain_ascii(source.value, byte)
            }
        }
    }
    fn kind(self) -> Result<Option<u8>> {
        let View::Source(source) = self.0 else { unreachable!() };
        source.source.kind(source.value)
    }
    /// Own a selected JSON string; a missing string type remains distinct from a read failure.
    pub(crate) fn string(self) -> Result<Option<String>> {
        Ok(self.text(false)?.and_then(|text| match text {
            crate::Text::Inline(text) => Some(text),
            _ => None,
        }))
    }
    pub fn is_string(self) -> Result<bool> {
        match self.0 {
            View::Owned(value) => Ok(value.is_string()),
            View::Source(_) => Ok(self.kind()? == Some(b'"')),
        }
    }
    /// A weaker [`Self::is_string`]: an unreadable source counts as absence, for a type probe of a
    /// field the codec skips rather than fails on.
    pub fn may_be_string(self) -> bool {
        self.is_string().unwrap_or(false)
    }
    pub fn is_object(self) -> Result<bool> {
        match self.0 {
            View::Owned(value) => Ok(value.is_object()),
            View::Source(_) => Ok(self.kind()? == Some(b'{')),
        }
    }
    pub fn is_array(self) -> Result<bool> {
        match self.0 {
            View::Owned(value) => Ok(value.is_array()),
            View::Source(_) => Ok(self.kind()? == Some(b'[')),
        }
    }
    pub fn is_null(self) -> Result<bool> {
        match self.0 {
            View::Owned(value) => Ok(value.is_null()),
            View::Source(_) => Ok(self.kind()? == Some(b'n')),
        }
    }
    pub fn as_bool(self) -> Result<Option<bool>> {
        match self.0 {
            View::Owned(value) => Ok(value.as_bool()),
            View::Source(_) => Ok(match self.kind()? {
                Some(b't') => Some(true),
                Some(b'f') => Some(false),
                _ => None,
            }),
        }
    }
    pub fn as_number(self) -> Result<Option<Number>> {
        match self.0 {
            View::Owned(value) => Ok(value.as_number().cloned()),
            View::Source(source) => {
                if matches!(self.kind()?, Some(b'-' | b'0'..=b'9')) {
                    Ok(Some(source.source.materialize(source.value)?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    /// Chronological array traversal holds one descriptor, not a vector of all children.
    pub fn array(self) -> Result<Option<JsonArray<'a>>> {
        Ok(match self.0 {
            View::Owned(value) => value.as_array().map(|array| JsonArray(Array::Owned(array.iter()))),
            View::Source(source) => source
                .source
                .array(source.value)?
                .map(|array| JsonArray(Array::Source { array, source })),
        })
    }
    /// Visit normalized object entries in Value's insertion order. Structural key state grows
    /// with object width; descendant payloads are borrowed, not decoded into a tree.
    pub fn for_each_entry(self, mut f: impl FnMut(&str, Self) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(Value::Object(object)) => {
                for (key, value) in object {
                    f(key, Self(View::Owned(value)))?;
                }
            }
            View::Owned(_) => return Ok(false),
            View::Source(source) => {
                let Some(entries) = source.source.entries(source.value)? else {
                    return Ok(false);
                };
                for (key, value) in entries {
                    f(&key, self.at(value))?;
                }
            }
        }
        Ok(true)
    }
    /// Visit decoded string fragments with bounded source scratch. Returns false for a nonstring.
    /// Errors are explicit; callback effects must be discarded if this operation returns Err.
    pub fn for_each_str_chunk(self, mut f: impl FnMut(&str) -> Result<()>) -> Result<bool> {
        match self.0 {
            View::Owned(value) => match value.as_str() {
                Some(text) => {
                    f(text)?;
                    Ok(true)
                }
                None => Ok(false),
            },
            View::Source(source) => {
                if source.source.kind(source.value)? != Some(b'"') {
                    return Ok(false);
                }
                source.source.string_chunks(source.value, &mut f)?;
                Ok(true)
            }
        }
    }
    /// Own a descriptor for this complete child. Source values copy only range metadata, while
    /// already owned values explicitly clone their selected data.
    pub fn to_json(self) -> Result<Json> {
        match self.0 {
            View::Owned(value) => Ok(Json::Owned(value.clone())),
            // The containing scoped operation performs the final identity check; child
            // descriptors do not need another stat for each field within that operation.
            View::Source(source) => match source.owner {
                Some(owner) => Ok(Json::Source(Arc::new(
                    owner.child(source.value.offset, source.value.len)?,
                ))),
                None => Ok(Json::Owned(self.resolve()?.into_owned())),
            },
        }
    }
    /// Materialize only this selected view. Source strings/containers may allocate their full data.
    pub fn resolve(self) -> Result<Cow<'a, Value>> {
        match self.0 {
            View::Owned(value) => Ok(Cow::Borrowed(value)),
            View::Source(source) => Ok(Cow::Owned(source.source.materialize(source.value)?)),
        }
    }
    pub fn canonical_len(self) -> Result<u64> {
        let mut counter = CountWriter(0);
        self.write_canonical(&mut counter)?;
        Ok(counter.0)
    }
    /// Same bounded output and truncation contract as Json::canonical_prefix.
    pub fn canonical_prefix(self, max_bytes: usize) -> Result<(String, bool)> {
        self.formatted_prefix(max_bytes, false)
    }
    /// A bounded Unicode-complete prefix of serde_json's default pretty representation.
    /// The containing scoped view retains its normal final source-identity check.
    pub fn pretty_prefix(self, max_bytes: usize) -> Result<(String, bool)> {
        self.formatted_prefix(max_bytes, true)
    }
    fn formatted_prefix(self, max_bytes: usize, pretty: bool) -> Result<(String, bool)> {
        let mut writer = PrefixWriter {
            bytes: Vec::new(),
            limit: max_bytes,
            stopped: false,
        };
        let result = if pretty {
            self.write_canonical(&mut crate::PrettyJson::new(&mut writer, 0))
        } else {
            self.write_canonical(&mut writer)
        };
        if !writer.stopped {
            result?;
        }
        Ok((String::from_utf8(writer.bytes)?, writer.stopped))
    }
    /// Canonicalize with source-identity-specific replacements selected from this capture.
    /// Replacements never change acceptance: the producer must validate the complete original.
    /// The replacement [`Self::write_canonical_replacing`] would write for this exact node.
    pub(crate) fn replacement(self, replacements: &[(Json, Value)]) -> Result<Option<&Value>> {
        let View::Source(SourceView {
            owner: Some(owner),
            value,
            ..
        }) = self.0
        else {
            return Ok(None);
        };
        for (original, replacement) in replacements {
            if let Json::Source(range) = original {
                let same_backing = match (&range.decoded, &owner.decoded) {
                    (None, None) => true,
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    _ => false,
                };
                if Arc::ptr_eq(&range.source.0, &owner.source.0) && same_backing && value == range.extent() {
                    return Ok(Some(replacement));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn write_canonical_replacing(
        self,
        replacements: &[(Json, Value)],
        writer: &mut impl Write,
    ) -> Result<()> {
        match self.0 {
            View::Owned(value) => serde_json::to_writer(writer, value).map_err(Into::into),
            View::Source(source) => write_source(source, replacements, writer),
        }
    }

    /// Same partial-output-on-error contract as Json::write_canonical.
    pub fn write_canonical(self, writer: &mut impl Write) -> Result<()> {
        self.write_canonical_replacing(&[], writer)
    }
}

enum Array<'a> {
    Owned(std::slice::Iter<'a, Value>),
    Source {
        array: crate::harness::store::document::range::ArrayRanges<'a>,
        source: SourceView<'a>,
    },
}
/// A scoped chronological array iterator. No source payloads or child roster are owned.
/// Items are fallible so file-backed cursor implementations can propagate read errors.
pub struct JsonArray<'a>(Array<'a>);
impl<'a> Iterator for JsonArray<'a> {
    type Item = Result<JsonView<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Array::Owned(array) => array.next().map(|value| Ok(JsonView(View::Owned(value)))),
            Array::Source { array, source } => array.next().map(|value| {
                value
                    .map(|value| JsonView(View::Source(SourceView { value, ..*source })))
                    .map_err(Into::into)
            }),
        }
    }
}

struct PrefixWriter {
    bytes: Vec<u8>,
    limit: usize,
    stopped: bool,
}
impl Write for PrefixWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let remaining = self.limit - self.bytes.len();
        let mut end = bytes.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let needed = self.bytes.len() + end;
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .max(32)
                .saturating_mul(2)
                .min(self.limit)
                .max(needed);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(&bytes[..end]);
        if end != bytes.len() {
            self.stopped = true;
            return Err(std::io::Error::other("canonical JSON prefix complete"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct CountWriter(u64);
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// One container walk; only scalar access touches the bytes.
fn write_source(view: SourceView<'_>, replacements: &[(Json, Value)], writer: &mut impl Write) -> Result<()> {
    let value = JsonView(View::Source(view));
    if let Some(replacement) = value.replacement(replacements)? {
        serde_json::to_writer(writer, replacement)?;
        return Ok(());
    }
    match view.source.kind(view.value)? {
        Some(b'{') => {
            writer.write_all(b"{")?;
            let mut first = true;
            value.for_each_entry(|key, child| {
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                child.write_canonical_replacing(replacements, writer)
            })?;
            writer.write_all(b"}")?;
        }
        Some(b'[') => {
            writer.write_all(b"[")?;
            for (i, child) in value.array()?.context("invalid validated JSON array")?.enumerate() {
                if i != 0 {
                    writer.write_all(b",")?;
                }
                child?.write_canonical_replacing(replacements, writer)?;
            }
            writer.write_all(b"]")?;
        }
        Some(b'"') => {
            if let Some(bytes) = view.source.borrow(view.value) {
                // Preserve the in-memory fast path: only Unicode and optional slash escapes need
                // decoding to match serde's canonical spelling. Accepted bytes can pass through.
                if !bytes.contains(&b'\\')
                    || (memchr::memmem::find(bytes, b"\\u").is_none() && memchr::memmem::find(bytes, b"\\/").is_none())
                {
                    writer.write_all(bytes)?;
                    return Ok(());
                }
            }
            writer.write_all(b"\"")?;
            let mut scratch = Vec::new();
            view.source.string_chunks(view.value, &mut |text: &str| {
                scratch.clear();
                serde_json::to_writer(&mut scratch, text)?;
                writer.write_all(&scratch[1..scratch.len() - 1])?;
                Ok(())
            })?;
            writer.write_all(b"\"")?;
        }
        Some(b'-' | b'0'..=b'9') => {
            let number: Number = view.source.materialize(view.value)?;
            serde_json::to_writer(writer, &number)?;
        }
        Some(b't') => writer.write_all(b"true")?,
        Some(b'f') => writer.write_all(b"false")?,
        Some(b'n') => writer.write_all(b"null")?,
        _ => anyhow::bail!("invalid validated JSON scalar"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_replacements_require_same_capture_and_exact_range() {
        let world = cv_testkit::World::new("json-canonical-replacements");
        let raw = r#"{"z":"first","a":{"content":"old","content":"final"},"z":"last","n":1.50}"#;
        let left = world.base.join("left.json");
        let right = world.base.join("right.json");
        std::fs::write(&left, raw).unwrap();
        std::fs::write(&right, raw).unwrap();
        let left = Json::source(&left, 0, raw.len() as u64).unwrap();
        let right = Json::source(&right, 0, raw.len() as u64).unwrap();
        let selected = left.select("a").unwrap().unwrap().select("content").unwrap().unwrap();
        let replacements = vec![(selected, Value::String("replacement".into()))];
        let mut actual = Vec::new();
        left.with_view(|view| view.write_canonical_replacing(&replacements, &mut actual))
            .unwrap();
        let mut expected: Value = serde_json::from_str(raw).unwrap();
        expected["a"]["content"] = Value::String("replacement".into());
        assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        actual.clear();
        right
            .with_view(|view| view.write_canonical_replacing(&replacements, &mut actual))
            .unwrap();
        assert_eq!(
            actual,
            serde_json::to_vec(&serde_json::from_str::<Value>(raw).unwrap()).unwrap()
        );
    }

    #[cfg(not(feature = "mmap"))]
    #[test]
    fn positioned_context_preserves_capture_and_failure_precedence() {
        let first = Fixture::new(br#"{"known":1,"ignored":"x"}"#);
        let second = Fixture::new(br#"{"known":1,"ignored":"x"}"#);
        let source = JsonSource::capture(&first.path).unwrap();
        let other = JsonSource::capture(&second.path).unwrap();
        let context = RangeSource::from_captured(source.clone()).unwrap();
        assert_eq!(
            source
                .read(
                    ReadInput::Positioned(&context, 0, source.len(), None),
                    Validation::Value,
                    |view| { Ok(view.get("known")?.unwrap().resolve()?.as_u64()) }
                )
                .unwrap(),
            Some(1)
        );
        assert!(other
            .read(
                ReadInput::Positioned(&context, 0, other.len(), None),
                Validation::Value,
                |_| Ok(())
            )
            .is_err());
        let error = source
            .read(
                ReadInput::Positioned(&context, 0, source.len(), None),
                Validation::Value,
                |_| -> Result<()> {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&first.path)?
                        .write_all(b" ")?;
                    anyhow::bail!("intentional callback stop")
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("source changed"));
    }

    #[test]
    fn escaped_string_chunks_match_serde_and_propagate_callback_failure() {
        for prefix in [0, 1, 7, 65530, 65531, 65532, 65533, 65534, 65535, 65536] {
            let wanted = format!(
                "{}😀\n\t\r\u{8}\u{c}\\\"/é{}",
                "x".repeat(prefix),
                "tail😀\n".repeat(if prefix < 65530 { 1 } else { 20000 })
            );
            let encoded = serde_json::to_string(&wanted).unwrap();
            let source = RangeSource::borrowed(None, encoded.as_bytes(), 0, false);
            let mut actual = String::new();
            source
                .string_chunks(source.range(), &mut |chunk| {
                    assert!(chunk.len() <= 65536);
                    actual.push_str(chunk);
                    Ok(())
                })
                .unwrap();
            assert_eq!(actual, wanted);
            assert!(source
                .string_chunks(source.range(), &mut |_| anyhow::bail!("sink failure"))
                .unwrap_err()
                .to_string()
                .contains("sink failure"));
        }
    }

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let dir = std::env::temp_dir().join(format!("cv-json-foundation-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("source.json");
            std::fs::write(&path, bytes).unwrap();
            Self { dir, path }
        }
        fn value(&self) -> Json {
            Json::source(&self.path, 0, std::fs::metadata(&self.path).unwrap().len()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }
    fn canonical(value: &Json) -> Vec<u8> {
        let mut output = Vec::new();
        value.write_canonical(&mut output).unwrap();
        assert_eq!(value.canonical_len().unwrap(), output.len() as u64);
        output
    }

    fn log_fixture(name: &str, bytes: &[u8]) -> (Fixture, PathBuf) {
        let fixture = Fixture::new(b"");
        let path = fixture.dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        (fixture, path)
    }

    #[test]
    fn append_only_log_grown_after_capture_still_reads_its_stable_prefix() {
        let (_fixture, path) = log_fixture("live.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
        let source = JsonSource::capture(&path).unwrap();
        let captured = source.len();
        let file = std::fs::File::open(&path).unwrap();
        let mut writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut writer, b"{\"c\":3}\n{\"partial\":").unwrap();
        source.check().unwrap();
        source.check_metadata(file.metadata().unwrap()).unwrap();
        let value = source.validated_range(0, 7).unwrap();
        assert_eq!(canonical(&value), br#"{"a":1}"#);
        assert_eq!(source.len(), captured);
        let range = JsonSource::capture(&path).unwrap();
        range.check().unwrap();
    }

    #[test]
    fn append_only_log_truncated_or_rewritten_after_capture_is_still_a_source_change() {
        for (label, mutate) in [
            (
                "truncate",
                (|p: &Path| std::fs::write(p, b"{\"a\":1}\n").unwrap()) as fn(&Path),
            ),
            ("same-length rewrite", |p| {
                std::fs::write(p, b"{\"z\":9}\n{\"b\":2}\n").unwrap();
                std::fs::File::open(p)
                    .unwrap()
                    .set_modified(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap();
            }),
            ("replace with a larger file", |p| {
                let next = p.with_extension("next");
                std::fs::write(&next, b"{\"z\":9}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
                std::fs::rename(&next, p).unwrap();
            }),
        ] {
            let (_fixture, path) = log_fixture("live.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
            let source = JsonSource::capture(&path).unwrap();
            mutate(&path);
            let error = source.check().unwrap_err();
            assert!(error.to_string().contains("JSON source changed"), "{label}: {error}");
        }
    }

    /// Append admission is decided by content, never by waiting: a log seen mid-write (new
    /// timestamps at its captured length, as Linux's write path shows) and one grown by half a
    /// record are admitted; a same-length rewrite of a sampled byte is not. An edit outside the
    /// samples is the documented limit (`crate::resume`), not detected.
    #[cfg(unix)]
    #[test]
    fn append_only_log_admission_is_decided_by_content() {
        use std::io::Write;
        let body: String = (0..400)
            .map(|i| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(40)))
            .collect();
        let (_fixture, path) = log_fixture("live.jsonl", body.as_bytes());
        let source = JsonSource::capture(&path).unwrap();
        let file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH).unwrap();
        source.check().unwrap();
        (&file).write_all(b"{\"half\":").unwrap();
        source.check().unwrap();
        let grown = std::fs::read(&path).unwrap();
        let rewrite = |at: usize| {
            let mut bytes = grown.clone();
            bytes[at] = b'y';
            std::fs::write(&path, &bytes).unwrap();
        };
        for at in [body.len() - 3, 3] {
            rewrite(at);
            assert!(
                source.check().unwrap_err().to_string().contains("JSON source changed"),
                "byte {at}"
            );
        }
        rewrite(body.len() / 2);
        source.check().unwrap();
    }

    #[test]
    fn whole_document_growth_is_still_a_source_change() {
        let (_fixture, path) = log_fixture("doc.json", b"{\"a\":1}");
        let source = JsonSource::capture(&path).unwrap();
        std::fs::write(&path, b"{\"a\":1} ").unwrap();
        assert!(source.check().unwrap_err().to_string().contains("JSON source changed"));
    }

    #[test]
    fn public_source_guard_factory_retains_absence_and_rejects_stale_dependencies() {
        let fixture = Fixture::new(br#"{"arg":true}"#);
        let sidecar = fixture.dir.join("conditional-wal");
        let mut reads = crate::source::ReadSet::default();
        reads.record(&sidecar);
        let value = Json::source_with_reads(&fixture.path, 0, 12, &reads).unwrap();
        let Json::Source(range) = &value else {
            panic!("checked source")
        };
        let mut consumer = crate::source::ReadSet::default();
        consumer.merge(range.dependencies().unwrap());
        assert!(consumer.fresh(&mut crate::source::Probe::default()));
        std::fs::write(&sidecar, "adopted dependency").unwrap();
        assert!(!consumer.fresh(&mut crate::source::Probe::default()));
        assert!(value.canonical_prefix(1).is_err());
        assert!(Json::source_with_reads(&fixture.path, 0, 12, &reads).is_err());
        reads = crate::source::ReadSet::default();
        reads.record(&sidecar);
        assert!(Json::source_with_reads(&fixture.path, 0, 12, &reads).is_ok());
        assert!(Json::source_with_reads(&fixture.path, 1, 3, &reads).is_err());
    }

    #[test]
    fn canonical_short_escape_strings_match_serde_value() {
        let bodies = [
            r#"plain☺ "#,
            r#"a\n\r\t\b\f\"\\z"#,
            r#"a\/b"#,
            r#"a\u0000\u000A\u0022\u005c\u0061\uD83D\uDE00"#,
            r#"\\u0041\\/"#,
        ];
        for body in bodies {
            let raw = format!(r#"{{"z":-0,"a":["{body}",{{"x":"{body}"}}],"z":2}}"#);
            let fixture = Fixture::new(raw.as_bytes());
            let expected: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(canonical(&fixture.value()), serde_json::to_vec(&expected).unwrap());
        }
    }

    #[test]
    fn typed_document_projection_preserves_derive_duplicate_and_sequence_rules() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Doc {
            #[serde(default)]
            body: Option<Value>,
        }
        let mut cases = vec![
            r#"{"body":null,"body":0}"#.to_owned(),
            r#"{"body":0,"\u0062ody":0}"#.to_owned(),
            r#"{"ignored":1,"ignored":2,"body":0}"#.to_owned(),
            r#"{"ignored\uD800":0}"#.to_owned(),
            r#"{"body":1e400}"#.to_owned(),
            r#"{"ignored":1e400,"body":0}"#.to_owned(),
            "[]".to_owned(),
            "[null]".to_owned(),
            "[0,1]".to_owned(),
        ];
        cases.push(format!(r#"{{"{}\uD800":0}}"#, "x".repeat(100_000)));
        for depth in 125..=129 {
            cases.push(format!(r#"{{"body":{}0{}}}"#, "[".repeat(depth), "]".repeat(depth)));
        }
        for bytes in cases {
            let fixture = Fixture::new(bytes.as_bytes());
            let source = JsonSource::capture(&fixture.path).unwrap();
            let actual = source.read(ReadInput::Document, Validation::Syntax, |view| {
                let fields =
                    crate::harness::store::document::Fields::parse_struct(view, &["body"])?.context("not a struct")?;
                if let Some(body) = fields.get("body") {
                    anyhow::ensure!(body.accepts_value_at_depth(1)?, "invalid body");
                }
                Ok(())
            });
            assert_eq!(actual.is_ok(), serde_json::from_str::<Doc>(&bytes).is_ok(), "{bytes}");
            if bytes.contains("ignored\\uD800") || bytes.contains("\\u0062ody") || bytes == "[0,1]" {
                assert!(actual
                    .unwrap_err()
                    .downcast_ref::<crate::harness::store::document::InvalidStruct>()
                    .is_some());
            }
        }
    }

    #[test]
    fn document_projection_preserves_escaped_keys_final_values_and_complete_arguments() {
        let body = "雪\"\\".repeat(4000);
        let bytes = format!(
            " \n{{\"ignored{}\":null,\"id\":\"old\",\"\\u0069d\":\"last\",\"body\":{},\"input\":{{\"payload\":{},\"n\":2}}}} \n",
            "x".repeat(20000),
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&body).unwrap()
        );
        let fixture = Fixture::new(bytes.as_bytes());
        let source = JsonSource::capture(&fixture.path).unwrap();
        source
            .read(ReadInput::Document, Validation::Value, |view| {
                let fields =
                    crate::harness::store::document::Fields::parse(view, &["id", "id", "body", "input"])?.unwrap();
                assert_eq!(fields.string("id")?.as_deref(), Some("last"));
                let text = fields.get("body").unwrap().text(true)?.unwrap();
                assert!(
                    matches!(text, crate::lazy::Text::Source(_)),
                    "large text must remain in its source"
                );
                assert_eq!(text.try_resolve(&crate::lazy::Resolver::new(None)).unwrap(), body);
                let argument = fields.get("input").unwrap().argument(true)?;
                assert!(matches!(argument, Json::Source(_)));
                assert_eq!(
                    argument.resolve()?.get("payload").and_then(Value::as_str),
                    Some(body.as_str())
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn archive_whole_grammar_and_independent_value_budgets_match_raw_children() {
        let nested = format!("{}0{}", "[".repeat(126), "]".repeat(126));
        let fixture = Fixture::new(
            format!("[{{\"bad\":1e400}},{{\"bad\":\"\\ud800\"}},{{\"ok\":{nested}}},{{\"ok\":1}}]").as_bytes(),
        );
        let source = JsonSource::capture(&fixture.path).unwrap();
        assert!(source.read(ReadInput::Document, Validation::Value, |_| Ok(())).is_err());
        let syntax = source
            .read(ReadInput::Document, Validation::Syntax, |view| {
                view.accepted_array()?
                    .unwrap()
                    .map(|v| v.map(|v| v.is_some()))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(syntax, [false, false, true, true]);
        let accepted = source
            .read(ReadInput::Document, Validation::Syntax, |array| {
                let array = array.accepted_array()?;
                array
                    .unwrap()
                    .map(|value| value.map(|v| v.is_some()))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(accepted, [false, false, true, true]);
        for bytes in [b"[{\"ok\":1},]".as_slice(), b"[{\"ok\":1},\"\xff\"]"] {
            let fixture = Fixture::new(bytes);
            let source = JsonSource::capture(&fixture.path).unwrap();
            let mut published = false;
            let error = source
                .read(ReadInput::Document, Validation::Syntax, |view| {
                    let _ = view.accepted_array()?;
                    published = true;
                    Ok(())
                })
                .unwrap_err();
            assert!(error.is::<InvalidJson>());
            assert!(!published);
        }
    }

    #[test]
    fn owned_wire_and_mutation_are_value_transparent() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Json>();
        let values = [
            Value::Null,
            json!(true),
            json!(-7),
            json!(1.25),
            json!(["x", null, {"a": 1}]),
            json!({"Source": {"path": "missing", "offset": 0, "len": 100}, "Owned": null}),
        ];
        for value in values {
            let expected = serde_json::to_vec(&value).unwrap();
            let mut wrapped = Json::from(value.clone());
            assert!(matches!(wrapped.resolve().unwrap(), Cow::Borrowed(_)));
            assert_eq!(serde_json::to_vec(&wrapped).unwrap(), expected);
            assert_eq!(canonical(&wrapped), expected);
            let decoded: Json = serde_json::from_slice(&expected).unwrap();
            assert!(matches!(decoded, Json::Owned(_)));
            assert_eq!(*decoded.resolve().unwrap(), value);
            *wrapped.make_owned().unwrap() = json!("mutated");
            assert_eq!(canonical(&wrapped), br#""mutated""#);
        }
    }

    #[test]
    fn source_writer_and_selected_serde_match_independent_value_oracle() {
        let mut cases: Vec<String> = [
            "null",
            "true",
            "false",
            "-0",
            "1.234e+10",
            "1e-400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""literal UTF8 é\n\r\t\b\f\/\\\"\u0000\u001f\ud83c\udf19""#,
            r#"{"z":0,"a":1,"z":{"b":2,"a":3,"b":4},"a":[true,null,"\u0041"],"":5}"#,
            r#"{"\u0061":1,"b":2,"a":3,"\u0062":4}"#,
            "[[],{},[1,2],\"\",{\"x\": [false, -0, 1e2]}]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for split in 0..=12 {
            cases.push(format!(
                "{{\"prefix\":\"{}\\ud83c\\udf19é\\n{}\",\"tail\":1}}",
                "x".repeat(65_536 - split),
                "\\\\".repeat(32_769)
            ));
        }
        // Large escaped controls exercise the serializer's worst-case per-window expansion.
        cases.push(format!("\"{}\"", "\\u0000\\t\\n".repeat(20_000)));
        for input in cases {
            let oracle: Value = serde_json::from_str(&input).unwrap();
            let expected = serde_json::to_vec(&oracle).unwrap();
            let fixture = Fixture::new(input.as_bytes());
            let value = fixture.value();
            assert_eq!(canonical(&value), expected);
            assert_eq!(serde_json::to_vec(&value).unwrap(), expected);
            assert_eq!(*value.resolve().unwrap(), oracle);
        }
    }

    #[test]
    fn source_acceptance_matches_value_including_overwritten_invalid_values() {
        let mut cases: Vec<Vec<u8>> = [
            "",
            " ",
            "[] trailing",
            "[1,]",
            "{",
            "00",
            "1e400",
            "-1e400",
            r#""\ud800""#,
            r#""\udc00""#,
            r#"{"a":"\ud800","a":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#""\ud800\u0041""#,
            r#""\q""#,
            "\"raw\ncontrol\"",
            " \n true \r ",
            r#""\\ud800""#,
        ]
        .into_iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        cases.push(b"{\"unused\":\"\xff\"}".to_vec());
        cases.push(format!("\"{}\\ud800\"", "x".repeat(131_072)).into_bytes());
        cases.push("9".repeat(2000).into_bytes());
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes());
            cases.push(format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth)).into_bytes());
        }
        for input in cases {
            let expected = serde_json::from_slice::<Value>(&input).is_ok();
            let fixture = Fixture::new(&input);
            if let Ok(text) = std::str::from_utf8(&input) {
                let source = JsonSource::capture(&fixture.path).unwrap();
                assert_eq!(
                    source
                        .read(ReadInput::Record(0, text, None), Validation::Value, |_| Ok(()))
                        .is_ok(),
                    expected,
                    "in memory {input:?}"
                );
            }
            assert_eq!(
                Json::source(&fixture.path, 0, input.len() as u64).is_ok(),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn scoped_projections_match_owned_and_preserve_duplicate_order() {
        let input = r#"{"z":{"n":1.5,"b":false},"a":[true,null,"\ud83c\udf19"],"z":{"n":2,"b":true},"s":"head\n尾"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned = Json::from(serde_json::from_str::<Value>(input).unwrap());
        for value in [owned, fixture.value()] {
            let (keys, strings) = value
                .with_view(|view| {
                    assert!(view.get("missing")?.is_none());
                    let z = view.get("z")?.unwrap();
                    assert_eq!(z.get("n")?.unwrap().as_number()?, Some(Number::from(2)));
                    assert_eq!(z.get("b")?.unwrap().as_bool()?, Some(true));
                    assert!(z.get("n")?.unwrap().get("anything")?.is_none());
                    let array = view.get("a")?.unwrap().array()?.unwrap().collect::<Result<Vec<_>>>()?;
                    assert_eq!(array.len(), 3);
                    assert_eq!(array[0].as_bool()?, Some(true));
                    assert!(array[1].is_null()?);
                    assert!(view.is_object()?);
                    assert!(view.get("a")?.unwrap().is_array()?);
                    assert!(array[2].is_string()?);
                    assert!(!array[1].is_string()?);
                    assert!(!array[1].for_each_str_chunk(|_| panic!("nonstring callback"))?);
                    let mut keys = Vec::new();
                    view.for_each_entry(|key, _| {
                        keys.push(key.to_string());
                        Ok(())
                    })?;
                    let mut strings = String::new();
                    array[2].for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    view.get("s")?.unwrap().for_each_str_chunk(|part| {
                        strings.push_str(part);
                        Ok(())
                    })?;
                    Ok((keys, strings))
                })
                .unwrap();
            assert_eq!(keys, ["z", "a", "s"]);
            assert_eq!(strings, "🌙head\n尾");
        }
    }

    #[test]
    fn exact_range_bounds_and_read_errors_never_become_null_or_missing() {
        let body = b" {\"value\":\"range\"} \n";
        // Keep non-JSON prefix/suffix outside the selected range.
        let fixture = Fixture::new(&[b"junk".as_slice(), body.as_slice(), b"trailing".as_slice()].concat());
        let mut value = Json::source(&fixture.path, 4, body.len() as u64).unwrap();
        assert_eq!(canonical(&value), br#"{"value":"range"}"#);
        assert!(Json::source(&fixture.path, 0, 4).is_err());
        assert!(Json::source(&fixture.path, u64::MAX, 2).is_err());
        assert!(Json::source(&fixture.path, 0, u64::MAX).is_err());
        assert!(Json::source(&fixture.path, 1000, 1).is_err());
        assert!(Json::source(fixture.dir.join("absent"), 0, 1).is_err());
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.resolve().is_err());
        assert!(value.with_view(|view| Ok(view.get("absent")?.is_none())).is_err());
        assert!(value.canonical_len().is_err());
        assert!(serde_json::to_vec(&value).is_err());
        assert!(value.make_owned().is_err());
        assert!(matches!(value, Json::Source(_)));
    }

    #[test]
    fn source_identity_rejects_append_replacement_and_mutation_during_projection() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":1} "#).unwrap();
        assert!(value.canonical_len().is_err());
        let value = fixture.value();
        value
            .with_view(|view| {
                assert_eq!(view.get("a")?.unwrap().as_number()?, Some(Number::from(1)));
                // Preserve length so mapped bytes remain valid; a deliberate mtime makes the check
                // deterministic on filesystems with coarse timestamp granularity.
                std::fs::write(&fixture.path, br#"{"a":2} "#)?;
                std::fs::File::open(&fixture.path)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
                Ok(())
            })
            .expect_err("post-operation source change must reject its output");
        let value = fixture.value();
        let replacement = fixture.dir.join("replacement");
        std::fs::write(&replacement, br#"{"a":3} "#).unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::rename(replacement, &fixture.path).unwrap();
        #[cfg(unix)]
        assert!(value.canonical_len().is_err()); // Same size/mtime; inode distinguishes replacement.
        drop(value);
        let mut value = fixture.value();
        let expected = value.resolve().unwrap().into_owned();
        assert_eq!(*value.make_owned().unwrap(), expected);
        std::fs::remove_file(&fixture.path).unwrap();
        assert_eq!(*value.resolve().unwrap(), expected); // Explicit ownership survives source loss.
    }

    #[test]
    fn selected_children_share_validated_identity_and_borrowed_owned_views() {
        let input = br#" {"ignored":"large","wrapper":{"n":7,"list":[{"a":"first"},{"a":"last"}]}} "#;
        let fixture = Fixture::new(input);
        let snapshot = JsonSource::capture(&fixture.path).unwrap();
        let memory = RangeSource::borrowed(None, input, 0, false);
        assert_eq!(
            memory.accept(memory.range(), Validation::Value).unwrap(),
            Some(Range {
                offset: 1,
                len: input.len() as u64 - 2
            })
        );
        let value = snapshot.validated_range(1, input.len() as u64 - 2).unwrap();
        let wrapper = value.select("wrapper").unwrap().unwrap();
        let child = wrapper
            .with_view(|view| view.get("list")?.unwrap().array()?.unwrap().nth(1).unwrap()?.to_json())
            .unwrap();
        assert_eq!(canonical(&child), br#"{"a":"last"}"#);
        assert_eq!(canonical(&wrapper.select("n").unwrap().unwrap()), b"7");
        assert!(wrapper.select("missing").unwrap().is_none());
        assert!(matches!(&child, Json::Source(range) if range.path() == fixture.path && range.offset() > 1));
        let public_value = fixture.value();
        assert_eq!(
            canonical(&public_value.select("wrapper").unwrap().unwrap()),
            canonical(&wrapper)
        );
        let owned: Value = serde_json::from_slice(input).unwrap();
        assert!(matches!(JsonView::from(&owned).resolve().unwrap(), Cow::Borrowed(_)));
        assert_eq!(
            canonical(
                &JsonView::from(&owned)
                    .get("wrapper")
                    .unwrap()
                    .unwrap()
                    .to_json()
                    .unwrap()
            ),
            canonical(&wrapper)
        );
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(child.resolve().is_err());
        assert!(snapshot.validated_range(1, input.len() as u64 - 2).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_restored_mtime_and_symlink_retargeting() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(br#"{"a":1}"#);
        let original_time = std::fs::metadata(&fixture.path).unwrap().modified().unwrap();
        let value = fixture.value();
        std::fs::write(&fixture.path, br#"{"a":2}"#).unwrap();
        std::fs::File::open(&fixture.path)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        assert!(value.resolve().is_err()); // Unix change time captures the restored mtime edit.
        let other = fixture.dir.join("other");
        std::fs::write(&other, br#"{"a":3}"#).unwrap();
        let link = fixture.dir.join("link");
        symlink(&fixture.path, &link).unwrap();
        let value = Json::source(&link, 0, 7).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        assert!(value.resolve().is_err());
    }

    #[test]
    fn direct_source_prefix_matches_owned_value_semantics() {
        let inputs = [
            serde_json::to_string(&"é🌙\\\n尾".repeat(200_000)).unwrap(),
            format!("[1,2,3,{}]", serde_json::to_string(&"tail".repeat(500_000)).unwrap()),
            r#"{"z":"first","a":[1,true,"é🌙"],"z":"last","tail":null}"#.to_string(),
        ];
        for input in inputs {
            let fixture = Fixture::new(input.as_bytes());
            let source = fixture.value();
            let owned = Json::from(serde_json::from_str::<Value>(&input).unwrap());
            for limit in [0, 1, 2, 5, 17, 65, 4096] {
                assert_eq!(
                    source.canonical_prefix(limit).unwrap(),
                    owned.canonical_prefix(limit).unwrap()
                );
            }
            std::fs::remove_file(&fixture.path).unwrap();
            assert!(source.canonical_prefix(0).is_err());
        }
    }

    #[test]
    fn canonical_prefix_is_bounded_unicode_complete_and_reports_truncation() {
        let input = r#"{"z":"é🌙尾\n","a":[true,1.25],"z":"é🌙尾\n","last":"tail"}"#;
        let fixture = Fixture::new(input.as_bytes());
        let owned: Value = serde_json::from_str(input).unwrap();
        for pretty in [false, true] {
            let expected = if pretty {
                serde_json::to_string_pretty(&owned).unwrap()
            } else {
                serde_json::to_string(&owned).unwrap()
            };
            for value in [Json::from(owned.clone()), fixture.value()] {
                for cap in 0..=expected.len() + 4 {
                    let (prefix, truncated) = value
                        .with_view(|view| {
                            if pretty {
                                view.pretty_prefix(cap)
                            } else {
                                view.canonical_prefix(cap)
                            }
                        })
                        .unwrap();
                    let mut end = cap.min(expected.len());
                    while !expected.is_char_boundary(end) {
                        end -= 1;
                    }
                    assert_eq!(prefix, expected[..end]);
                    assert_eq!(truncated, cap < expected.len());
                    assert!(prefix.capacity() <= cap);
                }
                let (prefix, truncated) = value
                    .with_view(|view| {
                        if pretty {
                            view.pretty_prefix(usize::MAX)
                        } else {
                            view.canonical_prefix(usize::MAX)
                        }
                    })
                    .unwrap();
                assert_eq!(prefix, expected);
                assert!(!truncated);
            }
        }
        let value = fixture.value();
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.canonical_prefix(0).is_err());
    }

    #[test]
    fn prefix_stop_cannot_mask_a_source_change() {
        let fixture = Fixture::new(br#"{"a":1}"#);
        let value = fixture.value();
        let error = value
            .with_view(|view| {
                let result = view.canonical_prefix(0)?;
                std::fs::File::open(&fixture.path)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
                Ok(result)
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
        let value = fixture.value();
        let error = value
            .with_view::<()>(|_| {
                std::fs::File::open(&fixture.path)?
                    .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
    }

    #[test]
    fn encoded_arguments_keep_backing_children_transport_and_original_identity() {
        let inner = format!(r#" {{"z":"{}","a":1,"z":"last"}} "#, "\\n".repeat(800_000));
        let encoded = serde_json::to_vec(&inner).unwrap();
        let fixture = Fixture::new(&encoded);
        let value = fixture.value().with_view(|view| view.encoded_argument()).unwrap();
        assert!(canonical(&value) == serde_json::to_vec(&serde_json::from_str::<Value>(&inner).unwrap()).unwrap());
        let child = value.select("z").unwrap().unwrap();
        let mut transported = value.clone();
        let mut registry = SourceRegistry::default();
        let descriptor = registry.detach(&mut transported).unwrap();
        drop(value);
        let restored = registry.restore(descriptor).unwrap();
        assert_eq!(canonical(&restored), br#"{"z":"last","a":1}"#);
        assert_eq!(canonical(&child), br#""last""#);
        let error = restored
            .with_view::<()>(|_| {
                std::fs::File::open(&fixture.path)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
                anyhow::bail!("intentional callback stop")
            })
            .unwrap_err();
        assert!(error.to_string().contains("JSON source changed"));
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(restored.resolve().is_err());
        assert!(child.canonical_len().is_err());
    }

    #[test]
    fn memory_backing_ranges_do_not_read_prefix_suffix_or_recapture_provenance() {
        let fixture = Fixture::new(b"original provenance");
        let source = JsonSource::capture(&fixture.path).unwrap();
        let body = br#"{"p":[0,{"n":42}]}"#;
        let bytes = [b"prefix ".as_slice(), body.as_slice(), b" suffix".as_slice()].concat();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::read(bytes.as_slice()).unwrap());
        let child = source
            .read(
                ReadInput::Backing(backing, 7, body.len() as u64),
                Validation::Syntax,
                |view| {
                    view.get("p")?
                        .unwrap()
                        .array()?
                        .unwrap()
                        .nth(1)
                        .unwrap()?
                        .get("n")?
                        .unwrap()
                        .to_json()
                },
            )
            .unwrap();
        assert_eq!(canonical(&child), b"42");
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(child.resolve().is_err());
    }

    #[test]
    fn same_arena_unescaped_windows_copy_before_append_write() {
        const CHILD: &str = "CV_TEST_SAME_ARENA_APPEND_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "json::tests::same_arena_unescaped_windows_copy_before_append_write",
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "same-arena append child failed: {status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    // A Darwin vnode deadlock can remain uninterruptible even after kill.
                    // Do not block the test runner waiting for that kernel fault to unwind.
                    let _ = child.try_wait();
                    panic!("same-arena append child {} stalled for 30 seconds", child.id());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let fixture = Fixture::new(b"compressed provenance placeholder");
        let captured = JsonSource::capture(&fixture.path).unwrap();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let padding = " ".repeat(crate::lazy::INLINE_MAX + 1);
        for argument in [
            "true".to_owned(),
            format!("{padding}true"),
            format!("{padding}{{}}"),
            format!("{padding}[]"),
            padding,
        ] {
            let outer = serde_json::to_vec(&serde_json::json!({"arguments":argument})).unwrap();
            let (offset, len) = backing
                .append(|out| {
                    out.write_all(&outer)?;
                    Ok(())
                })
                .unwrap();
            let value = captured
                .read(
                    ReadInput::Backing(backing.clone(), offset, len),
                    Validation::Value,
                    |view| view.get("arguments")?.unwrap().encoded_argument(),
                )
                .unwrap();
            let expected = crate::harness::wire::openai::parse_arguments(Some(&serde_json::Value::String(argument)));
            assert_eq!(value.resolve().unwrap().as_ref(), &expected);
        }
        // Tiny raw scalars and giant unescaped JSON strings both use borrowed writer fast paths.
        // The destination is the same arena vnode as the mapped input, exercising the Darwin
        // deadlock that producer-owned input buffers and escaped chunks did not reproduce.
        for body in [serde_json::json!(true), serde_json::json!("x".repeat(3 * 64 * 1024))] {
            let wire = serde_json::to_vec(&body).unwrap();
            let (offset, len) = backing
                .append(|out| {
                    out.write_all(&wire)?;
                    Ok(())
                })
                .unwrap();
            let (copy_offset, copy_len) = captured
                .read(
                    ReadInput::Backing(backing.clone(), offset, len),
                    Validation::Value,
                    |view| backing.append(|mut out| view.write_canonical(&mut out)),
                )
                .unwrap();
            let copied = captured
                .read(
                    ReadInput::Backing(backing.clone(), copy_offset, copy_len),
                    Validation::Value,
                    |view| view.to_json(),
                )
                .unwrap();
            assert_eq!(canonical(&copied), wire);
        }
    }

    #[test]
    fn nested_encoded_document_uses_same_arena_without_read_append_deadlock() {
        let inner = format!("{{\"payload\":\"{}\"}}", "x".repeat(9000));
        let outer = serde_json::to_vec(&serde_json::json!({"arguments":inner})).unwrap();
        let fixture = Fixture::new(b"compressed provenance placeholder");
        let captured = JsonSource::capture(&fixture.path).unwrap();
        let backing = Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let (offset, len) = backing
            .append(|writer| {
                writer.write_all(&outer)?;
                Ok(())
            })
            .unwrap();
        let value = captured
            .read(
                ReadInput::Backing(backing.clone(), offset, len),
                Validation::Syntax,
                |view| view.get("arguments")?.unwrap().encoded_argument(),
            )
            .unwrap();
        let Json::Source(range) = &value else {
            panic!("decoded source")
        };
        assert!(Arc::ptr_eq(range.decoded.as_ref().unwrap(), &backing));
        assert!(range.offset >= len);
        assert!(canonical(&value) == inner.as_bytes());
        assert!(backing.byte_len().unwrap() > len);
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(value.canonical_len().is_err());
    }

    #[test]
    fn decoded_arguments_share_one_append_arena_and_transport_owner() {
        let inner = [
            format!("{{\"p\":\"{}\"}}", "a".repeat(9000)),
            format!("{{\"p\":\"{}\"}}", "b".repeat(9000)),
        ];
        let fixture = Fixture::new(&serde_json::to_vec(&inner).unwrap());
        let values = fixture
            .value()
            .with_view(|view| {
                view.array()?
                    .unwrap()
                    .map(|view| view?.encoded_argument())
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        let (Json::Source(first), Json::Source(second)) = (&values[0], &values[1]) else {
            panic!("source arguments")
        };
        assert!(Arc::ptr_eq(
            first.decoded.as_ref().unwrap(),
            second.decoded.as_ref().unwrap()
        ));
        assert!(first.offset < second.offset);
        for (value, expected) in values.iter().zip(&inner) {
            assert!(canonical(value) == expected.as_bytes());
        }
        let mut registry = SourceRegistry::default();
        let descriptors = values
            .into_iter()
            .map(|mut value| registry.detach(&mut value).unwrap())
            .collect::<Vec<_>>();
        for (descriptor, expected) in descriptors.into_iter().zip(&inner) {
            assert!(canonical(&registry.restore(descriptor).unwrap()) == expected.as_bytes());
        }
    }

    #[test]
    fn encoded_arguments_preserve_invalid_scalar_blank_and_strict_value_semantics() {
        for inner in [
            format!("{{\"x\":\"{}\"}}", "x".repeat(9000)),
            format!("{}0{}{}", "[".repeat(130), "]".repeat(130), " ".repeat(9000)),
            format!("{{\"x\":\"\\ud800\",\"p\":\"{}\"}}", "x".repeat(9000)),
            format!("{{\"x\":1e999,\"p\":\"{}\"}}", "x".repeat(9000)),
            format!("42{}", " ".repeat(9000)),
            " ".repeat(9000),
            format!("{{\"x\":\"{}", "x".repeat(9000)),
        ] {
            let fixture = Fixture::new(&serde_json::to_vec(&inner).unwrap());
            let value = fixture.value().with_view(|view| view.encoded_argument()).unwrap();
            assert_eq!(
                canonical(&value),
                serde_json::to_vec(&crate::harness::wire::openai::structured_arguments(&inner)).unwrap()
            );
        }
    }

    struct FailingWriter {
        remaining: usize,
        written: usize,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected writer failure"));
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            self.written += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn writer_and_string_reducer_errors_are_propagated() {
        let input = format!("{{\"a\":\"{}\"}}", "\\n".repeat(100_000));
        let fixture = Fixture::new(input.as_bytes());
        for value in [
            Json::from(serde_json::from_str::<Value>(&input).unwrap()),
            fixture.value(),
        ] {
            let mut writer = FailingWriter {
                remaining: 100,
                written: 0,
            };
            let error = value.write_canonical(&mut writer).unwrap_err();
            assert!(error.to_string().contains("injected writer failure"));
            assert_eq!(writer.written, 100);
            let mut calls = 0;
            let error = value
                .with_view(|view| {
                    view.get("a")?.unwrap().for_each_str_chunk(|_| {
                        calls += 1;
                        anyhow::bail!("injected reducer failure")
                    })
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert!(error.to_string().contains("injected reducer failure"));
        }
    }
}
