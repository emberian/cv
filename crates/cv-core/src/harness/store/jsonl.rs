//! Strict read-only JSONL projection, sharing document source slices and navigation indexes.

use crate::json::{ReadInput, Validation};
pub(crate) mod ranges;

use super::document;
use std::fs::File;
use std::path::Path;

/// One physical JSONL line, as the record reader hands it to a codec.
pub(crate) struct Line<'v> {
    /// Absolute offset of the record after JSON whitespace trimming.
    pub at: u64,
    /// Absolute offset just past the line: after its LF, or the end of the read.
    pub end: u64,
    /// The writer finished this line (it ends in LF); only such boundaries may checkpoint.
    pub complete: bool,
    /// The Value-compatible record, or None for a blank, non-UTF-8 or rejected line.
    pub view: Option<crate::json::JsonView<'v>>,
    /// Nothing but JSON whitespace: a `None` view on any other line is a rejected record.
    pub blank: bool,
}

/// One captured source, shared by whole, resumed and bounded discovery reads. A transcript read
/// goes through [`drive`]; this boundary owns opening and identity rechecks.
pub(crate) struct Read {
    source: crate::json::JsonSource,
    file: File,
    /// Bytes through which [`Self::check_backing_at`] last verified the backing file.
    checked_through: std::sync::atomic::AtomicU64,
    /// A private decoded copy of a compressed source ([`Self::decoded`]): its lines are read
    /// instead, with the original's provenance. Its offsets are not file positions.
    decoded: Option<(std::sync::Arc<super::bytes::ByteSource>, u64)>,
}

impl ranges::At for Read {
    fn read_at(&self, bytes: &mut [u8], at: u64) -> std::io::Result<usize> {
        ranges::At::read_at(&self.file, bytes, at)
    }
}

#[cfg(test)]
thread_local! { pub(crate) static OPENED: std::cell::RefCell<Vec<std::path::PathBuf>> = const { std::cell::RefCell::new(Vec::new()) }; }

impl Read {
    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(test)]
        OPENED.with(|paths| paths.borrow_mut().push(path.to_path_buf()));
        Self::with(crate::json::JsonSource::capture(path)?)
    }

    /// A read of `source`, a capture the caller already holds (one taken with its read set).
    pub(crate) fn with(source: crate::json::JsonSource) -> anyhow::Result<Self> {
        let file = File::open(source.path())?;
        source.check_metadata(file.metadata()?)?;
        Ok(Self {
            source,
            file,
            checked_through: std::sync::atomic::AtomicU64::new(0),
            decoded: None,
        })
    }

    /// The bytes a pass reads: the source's, or its decoded copy's.
    pub(crate) fn len(&self) -> u64 {
        self.decoded.as_ref().map_or(self.source.len(), |(_, len)| *len)
    }

    /// Whether lines come from a decoded copy, whose offsets cannot resume a file.
    pub(crate) fn is_decoded(&self) -> bool {
        self.decoded.is_some()
    }

    pub(crate) fn source(&self) -> &crate::json::JsonSource {
        &self.source
    }

    /// Records read again by range from this descriptor ([`Records`]).
    pub(crate) fn records(&self) -> Records<'_> {
        Records::new(&self.source, &self.file)
    }

    /// Positioned bytes from the captured descriptor, for an adapter's compression decoder.
    pub(crate) fn reader(&self) -> anyhow::Result<impl std::io::BufRead + '_> {
        ranges::reader(
            &self.file,
            ranges::Range {
                offset: 0,
                len: self.source.len(),
            },
        )
    }

    /// Recheck the opened inode before resuming mapped bytes after a consumer callback. Called
    /// by the line passes ([`Self::check_backing_at`]), [`Self::check`] and the guard of a driven
    /// read ([`crate::stream::Guard`]): after a slow callback, the session metadata and a
    /// checkpoint. Nothing else calls it.
    pub(crate) fn check_backing(&self) -> anyhow::Result<()> {
        self.source.check_metadata(self.file.metadata()?)
    }

    /// Bytes of input between two backing checks on the hot path. A sink callback returns in
    /// microseconds on the hot path; checking the file's identity after every one of them cost a
    /// `fstat` per record (four per Codex record), a tenth of a 64 MB parse. The framer maps its
    /// input in windows, so a check once per window of consumed bytes catches a replaced or
    /// truncated file before the next mapping, and a checkpoint always forces one.
    const CHECK_EVERY: u64 = 8 << 20;

    /// A sink callback longer than this is one during which the file could plausibly have been
    /// replaced or truncated (a consumer that writes, talks to a network, or blocks); the guard
    /// checks the backing right after it. A faster callback can still mutate the source;
    /// detection then waits for CHECK_EVERY consumed bytes, a checkpoint or EOF.
    pub(crate) const SLOW_CALLBACK: std::time::Duration = std::time::Duration::from_micros(200);

    /// [`Self::check_backing`] once per [`Self::CHECK_EVERY`] bytes of consumed input, keyed by the
    /// offset the pass has reached: every line pass runs it after each visit.
    fn check_backing_at(&self, offset: u64) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let through = self.checked_through.load(Ordering::Relaxed);
        if offset < through.saturating_add(Self::CHECK_EVERY) {
            return Ok(());
        }
        self.check_backing()?;
        self.checked_through.store(offset, Ordering::Relaxed);
        Ok(())
    }

    /// Verify the captured descriptor and path, including after a codec's held EOF output.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        self.check_backing()?;
        self.source.check()
    }

    /// This source read through `backing`, its decoded copy (`len` bytes at its start): views
    /// keep the original's identity and its completion guards.
    pub(crate) fn decoded(mut self, backing: std::sync::Arc<super::bytes::ByteSource>, len: u64) -> Self {
        self.decoded = Some((backing, len));
        self
    }

    /// Whole or resumed read through the captured EOF. An unterminated record at actual EOF
    /// may be valid, but never offers a complete checkpoint boundary.
    pub(crate) fn lines(
        &self,
        from: u64,
        visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
    ) -> anyhow::Result<()> {
        // At the captured EOF (`to == len`) the snapshot flag cannot differ — nothing is held
        // back — so whole and resumed reads share one call.
        self.snapshot(from, self.len(), visit)
    }

    /// At most `limit` source bytes. A cut inside a record is not EOF: that record is withheld,
    /// even if its prefix happens to be valid JSON. Stopping the visitor does not claim EOF.
    pub(crate) fn prefix(
        &self,
        limit: u64,
        visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
    ) -> anyhow::Result<()> {
        self.visit_with_end(0, self.len().min(limit), false, visit)
    }

    /// A historical byte length intentionally acts as that snapshot's EOF, rather than a
    /// discovery budget. A valid unterminated final record remains visible, never checkpointable.
    pub(crate) fn snapshot(
        &self,
        from: u64,
        to: u64,
        visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
    ) -> anyhow::Result<()> {
        self.visit_with_end(from, to.min(self.len()), true, visit)
    }

    fn visit_with_end(
        &self,
        from: u64,
        to: u64,
        snapshot: bool,
        mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
    ) -> anyhow::Result<()> {
        let len = self.len();
        let visit = |line: Line<'_>| {
            if !snapshot && !line.complete && to < len {
                return Ok(crate::stream::Flow::Continue);
            }
            let end = line.end;
            let flow = visit(line)?;
            // After the visit's callbacks, before the framer touches its mapping again.
            self.check_backing_at(end)?;
            Ok(flow)
        };
        let read = match &self.decoded {
            Some((backing, len)) => for_each_decoded_line(&self.source, backing, *len, from, visit),
            None => for_each_line(&self.source, &self.file, from, to, visit),
        };
        self.check()?;
        read
    }
}

/// One JSONL wire format's record codec, as [`drive`] runs it: a record in, messages to the sink,
/// state carried between records. Everything around the record is the driver's.
pub(crate) trait Codec {
    /// One accepted record, starting at byte `at`.
    fn record(
        &mut self,
        view: crate::json::JsonView<'_>,
        at: u64,
        sink: &mut dyn crate::stream::MessageSink,
    ) -> anyhow::Result<crate::stream::Flow>;

    /// Nothing is held back after the last record: a checkpoint here resumes exactly.
    fn settled(&self) -> bool {
        true
    }

    /// The demand this codec's state depends on: a checkpoint keeps it, and a read at another
    /// declines it. A codec whose state is the same at every demand answers the default.
    fn demand(&self) -> crate::stream::Demand;

    /// The session the records accumulate into: the driver notes its rejected lines there before
    /// [`Self::finish`].
    fn session(&mut self) -> &mut crate::ir::Session;

    /// What a checkpoint keeps of this codec, restored by the adapter through [`restore`].
    fn state(&self) -> impl serde::Serialize + '_;

    /// The read ended (at EOF, or `stopped` by the sink): deliver what is held unless stopped,
    /// and return the session.
    fn finish(self, stopped: bool, sink: &mut dyn crate::stream::MessageSink) -> anyhow::Result<crate::ir::Session>;
}

/// What a driven read's checkpoint keeps under [`crate::stream::ADAPTER_LAYER`]. It carries no
/// version of its own: a stored checkpoint is admitted only under its mark's version
/// ([`crate::resume::version`]), which is [`crate::source::VERSION`]; a change to what a codec
/// keeps or what a record parses to bumps that.
#[derive(serde::Serialize, serde::Deserialize)]
struct Stamped<S> {
    demand: crate::stream::Demand,
    skipped: u64,
    state: S,
}

/// The codec state `from` holds; `None` (decline the resume) for another state or no state.
pub(crate) fn restore<S: serde::de::DeserializeOwned>(from: &crate::stream::Checkpoint) -> Option<S> {
    from.get::<Stamped<S>>(crate::stream::ADAPTER_LAYER)
        .map(|stamped| stamped.state)
}

/// [`restore`] at `demand`, with the rejected lines counted before the checkpoint.
pub(crate) fn restore_at<S: serde::de::DeserializeOwned>(
    from: &crate::stream::Checkpoint,
    demand: crate::stream::Demand,
) -> Option<(S, u64)> {
    from.get::<Stamped<S>>(crate::stream::ADAPTER_LAYER)
        .filter(|stamped| stamped.demand == demand)
        .map(|stamped| (stamped.state, stamped.skipped))
}

/// Put `state` in `cp` as a driven read's checkpoint keeps it: with the `demand` it depends on and the rejected lines before it ([`restore_at`] reads it back).
pub(crate) fn stamp(
    cp: &mut crate::stream::Checkpoint,
    demand: crate::stream::Demand,
    skipped: u64,
    state: &impl serde::Serialize,
) {
    cp.put(crate::stream::ADAPTER_LAYER, &Stamped { demand, skipped, state });
}

/// A checkpoint at `offset` holding `state` as a driven read of `C` would (tests build marks).
#[cfg(test)]
pub(crate) fn checkpoint<C: Codec>(
    offset: u64,
    demand: crate::stream::Demand,
    state: &impl serde::Serialize,
) -> crate::stream::Checkpoint {
    let mut cp = crate::stream::Checkpoint::new(offset);
    stamp(&mut cp, demand, 0, state);
    cp
}

/// The one JSONL transcript read: records `from..to` of `read` (a whole read starts at 0, a
/// resumed one at its checkpoint, `to` is EOF unless a past length is asked for), each through
/// `codec`. The driver owns the rest: declining a checkpoint past EOF, counting rejected lines
/// ([`super::super::note_skipped_lines`], carried through checkpoints), offering a checkpoint
/// after every finished line the codec has [`Codec::settled`] on, the consumer guard (its
/// errors and the backing checks after a slow callback, the session metadata and a checkpoint:
/// [`crate::stream::Guard`]), and the final check after the codec's held output.
/// `Ok(None)`: `from` lies beyond the file; nothing reached `sink`.
pub(crate) fn drive<C: Codec>(
    read: &Read,
    from: crate::stream::Start<'_>,
    mut codec: C,
    sink: &mut dyn crate::stream::MessageSink,
) -> anyhow::Result<Option<crate::ir::Session>> {
    use crate::stream::{Flow, MessageSink, Start};
    let len = read.len();
    let (at, to, mut skipped) = match from {
        Start::Whole => (0, len, 0),
        Start::Prefix(to) => (0, to.min(len), 0),
        Start::Checkpoint(_) if read.is_decoded() => return Ok(None),
        Start::Checkpoint(cp) => {
            let Some((_, skipped)) = restore_at::<serde::de::IgnoredAny>(cp, codec.demand()) else {
                return Ok(None);
            };
            (cp.offset, len, skipped)
        }
    };
    if at > len {
        return Ok(None);
    }
    crate::stream::guarded(sink, read, |guard| {
        let mut stopped = false;
        read.snapshot(at, to, |line| {
            match line.view {
                Some(view) => {
                    if codec.record(view, line.at, guard)? == Flow::Stop || guard.failed() {
                        stopped = true;
                        return Ok(Flow::Stop);
                    }
                }
                None => skipped += u64::from(!line.blank),
            }
            // Only after a line its writer finished (a blank or rejected one too): a partial last
            // line is re-read whole next time.
            // The last line of the read is offered as its mark whatever the spacing.
            let settled = line.complete && !read.is_decoded() && codec.settled();
            if settled
                && (guard.wants_checkpoint(line.end) || (line.end >= to && guard.wants_final_checkpoint(line.end)))
            {
                let mut cp = crate::stream::Checkpoint::new(line.end);
                stamp(&mut cp, codec.demand(), skipped, &codec.state());
                guard.checkpoint(&mut cp);
            }
            stopped = guard.failed();
            Ok(if stopped { Flow::Stop } else { Flow::Continue })
        })?;
        super::super::note_skipped_lines(codec.session(), skipped);
        codec.finish(stopped, guard).map(Some)
    })
}

/// Pure in-memory JSONL, through the same positioned framer and Value-compatible line
/// acceptance as file reads. Views have no file provenance; demanded content stays owned.
pub(crate) fn slice(
    bytes: &[u8],
    mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    let mut frames = ranges::records(
        bytes,
        ranges::Range {
            offset: 0,
            len: bytes.len() as u64,
        },
    )?;
    while let Some(frame) = frames.next() {
        let (raw, complete) = frame?;
        let start = raw.offset as usize;
        let end = (raw.offset + raw.len) as usize;
        if in_memory(
            None,
            None,
            raw.offset,
            frames.offset(),
            complete,
            &bytes[start..end],
            &mut visit,
        )? == crate::stream::Flow::Stop
        {
            break;
        }
    }
    Ok(())
}

/// Whole-file projection for sidecar reducers that keep no offsets. Blank and rejected lines
/// are skipped; the same lifecycle guards used by streaming and discovery bracket the read.
pub(crate) fn for_each_record(
    path: &Path,
    mut visit: impl FnMut(crate::json::JsonView<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    Read::open(path)?.lines(0, |line| {
        if let Some(view) = line.view {
            visit(view)?;
        }
        Ok(crate::stream::Flow::Continue)
    })
}

/// Records of `from..to` in a captured JSONL source, each physical line once. This is the byte
/// source every JSONL codec shares; it is the only place the two builds differ. Mapped builds
/// borrow each line from one mapping. Positioned builds (and a failed mapping) read each small
/// line into one reused buffer and leave a giant line in place as a positioned view, so no
/// private copy of it is ever made. Either way the codec sees the same [`JsonView`] API.
///
/// Records are trimmed of JSON whitespace only (space, tab, LF, CR). Read errors end the
/// visit with an error. `file` must be the descriptor the caller checked against `source`.
fn for_each_line(
    source: &crate::json::JsonSource,
    file: &File,
    from: u64,
    to: u64,
    visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    lines(source, file, None, from, to, visit)
}

/// [`for_each_line`] over the source's private decoded bytes (a decompressed copy, `len` bytes
/// long, at the start of `backing`): views keep the original's provenance, offsets are decoded.
fn for_each_decoded_line(
    source: &crate::json::JsonSource,
    backing: &std::sync::Arc<super::bytes::ByteSource>,
    len: u64,
    from: u64,
    visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    lines(source, &backing.file()?, Some((backing, len)), from, len, visit)
}

type Decoded<'b> = Option<(&'b std::sync::Arc<super::bytes::ByteSource>, u64)>;

fn lines(
    source: &crate::json::JsonSource,
    file: &File,
    decoded: Decoded<'_>,
    from: u64,
    to: u64,
    mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    if decoded.is_none() {
        return crate::io_admission::IoAdmission::global().admit_path(source.path(), || {
            lines_inner(source, file, decoded, from, to, &mut visit)
        });
    }
    lines_inner(source, file, decoded, from, to, &mut visit)
}

fn lines_inner(
    source: &crate::json::JsonSource,
    file: &File,
    decoded: Decoded<'_>,
    from: u64,
    to: u64,
    mut visit: impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<()> {
    use crate::stream::Flow;
    #[cfg(feature = "mmap")]
    if decoded.is_some()
        || crate::io_admission::byte_source_policy(source.path()) == crate::io_admission::ByteSourcePolicy::Mapped
    {
        if let Some(mapped) = mapped_lines(source, file, decoded, from, to, &mut visit) {
            return mapped;
        }
    }
    let mut frames = ranges::records(
        file,
        ranges::Range {
            offset: from,
            len: to.saturating_sub(from),
        },
    )?;
    frames.keep_small_records(64 * 1024);
    let context = match decoded {
        Some((backing, _)) => document::range::RangeSource::decoded(source.clone(), backing)?,
        None => document::range::RangeSource::from_captured(source.clone())?,
    };
    while let Some(frame) = frames.next() {
        let (raw, complete) = frame?;
        let end = frames.offset();
        let flow = if let Some(bytes) = frames.small_record() {
            in_memory(Some(source), decoded, raw.offset, end, complete, bytes, &mut visit)?
        } else {
            let range = ranges::trim(file, raw)?;
            let at = range.offset;
            let mut visited = false;
            let blank = range.len == 0;
            let mut view = |view: crate::json::JsonView<'_>| {
                visited = true;
                visit(Line {
                    at,
                    end,
                    complete,
                    view: Some(view),
                    blank,
                })
            };
            let read = (!blank).then(|| match decoded {
                Some((backing, len)) => source.read(
                    ReadInput::Positioned(&context, at, range.len, Some((backing, len))),
                    Validation::Value,
                    &mut view,
                ),
                None => source.read(
                    ReadInput::Positioned(&context, at, range.len, None),
                    Validation::Value,
                    &mut view,
                ),
            });
            match read {
                Some(Ok(flow)) => flow,
                // A rejected record delivers the None line; its parse error does not escape.
                Some(Err(error)) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => {
                    visit(Line {
                        at,
                        end,
                        complete,
                        view: None,
                        blank,
                    })?
                }
                None => visit(Line {
                    at,
                    end,
                    complete,
                    view: None,
                    blank,
                })?,
                Some(Err(error)) => return Err(error),
            }
        };
        if flow == Flow::Stop {
            break;
        }
    }
    Ok(())
}

/// The most a line pass keeps mapped at once: one window, grown only to hold a line longer than
/// it. A pass over a multi-GB transcript never holds the whole file mapped, so its resident (and,
/// on Linux, cgroup-charged) mapped bytes stay bounded by the passes running, not by file size.
/// 8 MiB in production (64 KiB in tests).
#[cfg(feature = "mmap")]
pub(crate) const MAP_WINDOW: u64 = if cfg!(test) { 64 << 10 } else { 8 << 20 };

/// The record size a whole-record read buffers; at most this (and with `mmap`, strictly above it)
/// the read is served from a buffer, never a range context. (Not [`MAP_WINDOW`], which bounds how
/// much a line pass keeps mapped.)
const MAPPED_RECORD: u64 = 64 * 1024;

// Tests: the largest mapping each thread has made, the bound the mapped-bytes test asserts.
#[cfg(all(test, feature = "mmap"))]
thread_local! { pub(crate) static MAPPED_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(all(test, feature = "mmap"))]
pub(crate) fn note_mapped(len: usize) {
    MAPPED_PEAK.with(|peak| peak.set(peak.get().max(len)));
}

/// [`lines`] over windows of a mapping, each dropped before the next is mapped. `None` when the
/// first window cannot be mapped: the caller reads positioned instead.
#[cfg(feature = "mmap")]
fn mapped_lines(
    source: &crate::json::JsonSource,
    file: &File,
    decoded: Decoded<'_>,
    from: u64,
    to: u64,
    visit: &mut impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> Option<anyhow::Result<()>> {
    use crate::stream::Flow;
    let mut at = from;
    while at < to {
        // A window's complete lines end at its last LF; one holding none grows until it does.
        let mut end = to.min(at.saturating_add(MAP_WINDOW));
        let map = loop {
            let len = usize::try_from(end - at).ok()?;
            let map = match unsafe { memmap2::MmapOptions::new().offset(at).len(len).map(file) } {
                Ok(map) => map,
                Err(_) if at == from => return None,
                Err(error) => return Some(Err(error.into())),
            };
            if end == to || memchr::memrchr(b'\n', &map).is_some() {
                break map;
            }
            end = to.min(at.saturating_add(2 * (end - at)));
        };
        #[cfg(test)]
        note_mapped(map.len());
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let last = match end == to {
            true => map.len(),
            false => memchr::memrchr(b'\n', &map).map_or(map.len(), |i| i + 1),
        };
        let bytes = &map[..last];
        let mut start = 0;
        for stop in memchr::memchr_iter(b'\n', bytes).map(|i| i + 1) {
            let line = &bytes[start..stop - 1];
            let flow = in_memory(
                Some(source),
                decoded,
                at + start as u64,
                at + stop as u64,
                true,
                line,
                visit,
            );
            match flow {
                Ok(Flow::Stop) => return Some(Ok(())),
                Err(error) => return Some(Err(error)),
                Ok(Flow::Continue) => {}
            }
            start = stop;
        }
        if start < bytes.len() {
            // Only the read's last window can end without an LF: a partial last line.
            return Some(
                in_memory(
                    Some(source),
                    decoded,
                    at + start as u64,
                    to,
                    false,
                    &bytes[start..],
                    visit,
                )
                .map(|_| ()),
            );
        }
        at += last as u64;
    }
    Some(Ok(()))
}

/// Records a first [`for_each_line`] pass kept by range (a line's, or a value inside one, from
/// `JsonView::source_range`), read again from the same descriptor in any order: for formats
/// whose later records replace earlier ones, so selection must reach EOF before decoding.
/// A small range is read into one buffer. A giant one is mapped for its read alone in mapped
/// builds and left in place as a positioned view otherwise, so the file is never held mapped.
/// A record that is not UTF-8 is rejected ([`InvalidJson`](crate::json::InvalidJson)), as
/// [`for_each_line`] rejects such a line.
pub(crate) struct Records<'s> {
    source: &'s crate::json::JsonSource,
    file: &'s File,
    context: Option<document::range::RangeSource<'static>>,
    buffer: Vec<u8>,
}

impl<'s> Records<'s> {
    pub(crate) fn new(source: &'s crate::json::JsonSource, file: &'s File) -> Self {
        Self {
            source,
            file,
            context: None,
            buffer: Vec::new(),
        }
    }

    pub(crate) fn read<T>(
        &mut self,
        at: u64,
        len: u64,
        f: impl FnOnce(crate::json::JsonView<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.read_with(at, len, Validation::Value, f)
    }
    /// [`Self::read`] under a chosen acceptance.
    pub(crate) fn read_with<T>(
        &mut self,
        at: u64,
        len: u64,
        validation: crate::json::Validation,
        f: impl FnOnce(crate::json::JsonView<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(
            at.checked_add(len).is_some_and(|end| end <= self.source.len()),
            "record range outside its source"
        );
        if len <= MAPPED_RECORD {
            self.buffer.resize(len as usize, 0);
            ranges::read_exact_at(self.file, &mut self.buffer, at)?;
            let text = simdutf8::basic::from_utf8(&self.buffer).map_err(|_| crate::json::InvalidJson)?;
            return self.source.read(ReadInput::Record(at, text, None), validation, f);
        }
        // Only giant reads remain: local sealed immutable files map for this read alone when
        // `mmap` is enabled; mutable external transcripts always read via positioned buffers.
        #[cfg(feature = "mmap")]
        if crate::io_admission::byte_source_policy(self.source.path()) == crate::io_admission::ByteSourcePolicy::Mapped
        {
            // SAFETY: a read-only mapping of a local sealed immutable file, dropped with this read.
            let map = unsafe {
                memmap2::MmapOptions::new()
                    .offset(at)
                    .len(usize::try_from(len)?)
                    .map(self.file)?
            };
            #[cfg(test)]
            note_mapped(map.len());
            let text = simdutf8::basic::from_utf8(&map).map_err(|_| crate::json::InvalidJson)?;
            return self.source.read(ReadInput::Record(at, text, None), validation, f);
        }
        if self.context.is_none() {
            self.context = Some(document::range::RangeSource::from_captured(self.source.clone())?);
        }
        let context = self.context.as_ref().expect("context was just made");
        self.source
            .read(ReadInput::Positioned(context, at, len, None), validation, f)
    }
}

/// One line whose bytes are in memory (mapped, or read as a small positioned record).
fn in_memory(
    source: Option<&crate::json::JsonSource>,
    decoded: Decoded<'_>,
    start: u64,
    end: u64,
    complete: bool,
    bytes: &[u8],
    visit: &mut impl FnMut(Line<'_>) -> anyhow::Result<crate::stream::Flow>,
) -> anyhow::Result<crate::stream::Flow> {
    let lead = bytes.iter().take_while(|b| super::json::is_whitespace(b)).count();
    let trimmed = super::json::trim(&bytes[lead..]);
    let at = start + lead as u64;
    let blank = trimmed.is_empty();
    if let Some(text) = (!blank).then(|| simdutf8::basic::from_utf8(trimmed).ok()).flatten() {
        let mut visited = false;
        let mut view = |view: crate::json::JsonView<'_>| {
            visited = true;
            visit(Line {
                at,
                end,
                complete,
                view: Some(view),
                blank,
            })
        };
        let read = match (source, decoded) {
            (Some(source), Some((backing, len))) => source.read(
                ReadInput::Record(at, text, Some((backing, len))),
                Validation::Value,
                view,
            ),
            (Some(source), None) => source.read(ReadInput::Record(at, text, None), Validation::Value, view),
            (None, _) => (|| {
                let context = document::range::RangeSource::text(None, text, at, false);
                let range = document::range::Range {
                    offset: at,
                    len: text.len() as u64,
                };
                match context.accept(range, Validation::Value)? {
                    Some(value) => view(crate::json::JsonView::of(&context, value)),
                    None => Err(crate::json::InvalidJson.into()),
                }
            })(),
        };
        match read {
            Err(error) if !visited && error.downcast_ref::<crate::json::InvalidJson>().is_some() => {}
            flow => return flow,
        }
    }
    visit(Line {
        at,
        end,
        complete,
        view: None,
        blank,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::json::{ReadInput, Validation};
    use crate::stream::Flow;

    /// Run the test `name` (its full path) again in a child process with core dumps off, and
    /// report whether this is that child: a regression in a test that truncates a mapped source
    /// SIGBUSes the child, never the shared runner. The parent asserts the child passed.
    #[cfg(unix)]
    pub(crate) fn in_child(name: &str) -> bool {
        const CHILD: &str = "CV_TEST_MAPPED_TRUNCATION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", name]).env(CHILD, "1");
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name} child {} failed: {status}", child.id());
                return false;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("{name} child {} timed out", child.id());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn bounded_prefix_is_not_a_valid_unterminated_eof_record() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("jsonl-bounded-eof");
        let path = world.base.join("wire.jsonl");
        let valid = r#"{"type":"user"}"#;
        std::fs::write(&path, format!("{valid} trailing\n"))?;
        let mut calls = 0;
        super::Read::open(&path)?.prefix(valid.len() as u64, |_| {
            calls += 1;
            Ok(Flow::Continue)
        })?;
        assert_eq!(
            calls, 0,
            "a budget boundary cannot validate a prefix of a rejected record"
        );
        super::Read::open(&path)?.snapshot(0, valid.len() as u64, |line| {
            assert!(line.view.is_some());
            assert!(!line.complete, "historical EOF is not a checkpoint boundary");
            calls += 1;
            Ok(Flow::Continue)
        })?;
        assert_eq!(
            calls, 1,
            "the same cut explicitly requested as a historical snapshot is EOF"
        );
        calls = 0;
        std::fs::write(&path, valid)?;
        super::Read::open(&path)?.prefix(valid.len() as u64, |line| {
            assert!(line.view.is_some());
            assert!(!line.complete);
            calls += 1;
            Ok(Flow::Continue)
        })?;
        assert_eq!(calls, 1, "actual unterminated EOF keeps the shared acceptance rule");
        Ok(())
    }

    #[test]
    fn pure_slice_and_captured_file_share_framing_and_rejected_record_positions() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("jsonl-memory-file");
        let path = world.base.join("wire.jsonl");
        let mut bytes = format!(" \r\n{{\"x\":\"{}\"}}\n{{\"x\":1e400,\"x\":0}}\n", "雪".repeat(40000)).into_bytes();
        bytes.extend_from_slice(b"\xff\n{\"partial\":true}");
        std::fs::write(&path, &bytes)?;
        let mut memory = Vec::new();
        super::slice(&bytes, |line| {
            memory.push((
                line.at,
                line.end,
                line.complete,
                line.blank,
                line.view.map(|v| v.resolve().map(|v| v.into_owned())).transpose()?,
            ));
            Ok(Flow::Continue)
        })?;
        let mut file = Vec::new();
        super::Read::open(&path)?.lines(0, |line| {
            file.push((
                line.at,
                line.end,
                line.complete,
                line.blank,
                line.view.map(|v| v.resolve().map(|v| v.into_owned())).transpose()?,
            ));
            Ok(Flow::Continue)
        })?;
        assert_eq!(memory, file);
        assert_eq!(memory.iter().filter(|row| row.4.is_none() && !row.3).count(), 2);
        assert!(!memory.last().unwrap().2);
        Ok(())
    }

    #[test]
    fn lifecycle_rechecks_a_source_even_after_visitor_stop() -> anyhow::Result<()> {
        let world = cv_testkit::World::new("jsonl-stop-recheck");
        let path = world.base.join("wire.jsonl");
        std::fs::write(&path, "{}\n{}\n")?;
        let read = super::Read::open(&path)?;
        let result = read.lines(0, |_| {
            let replacement = path.with_extension("replacement");
            std::fs::write(&replacement, "{}\n")?;
            std::fs::rename(&replacement, &path)?;
            Ok(Flow::Stop)
        });
        assert!(
            result.is_err(),
            "early discovery stop must reject replacement (immutable-prefix appends remain admissible)"
        );
        Ok(())
    }

    /// Invalid UTF-8 is not one JSON value: every record reader rejects it as `InvalidJson`
    /// (inside a string or key, between tokens, alone, as a cut scalar, past the in-memory limit),
    /// so no reader needs a UTF-8 pass of its own before it parses.
    #[test]
    fn invalid_utf8_is_not_one_json_value_to_any_record_reader() {
        let world = cv_testkit::World::new("jsonl-invalid-utf8");
        let valid = br#"{"type":"user","sessionId":"s"}"#;
        let mut large = br#"{"a":""#.to_vec();
        large.extend(std::iter::repeat_n(b'x', 70_000));
        large.extend(b"\xff\"}");
        for (name, bad) in [
            (
                "string",
                br#"{"a":""#.iter().chain(b"\xff\"}").copied().collect::<Vec<_>>(),
            ),
            ("key", b"{\"\xff\":1}".to_vec()),
            ("between", b"{\"a\":1\xff}".to_vec()),
            ("alone", b"\xff".to_vec()),
            ("cut-scalar", b"{\"a\":\"\xe2\x82\"}".to_vec()),
            ("large", large.clone()),
        ] {
            let path = world.base.join(format!("{name}.jsonl"));
            let bytes = [&valid[..], b"\n", &bad, b"\n", valid, b"\n"].concat();
            std::fs::write(&path, &bytes).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let mut lines = Vec::new();
            super::for_each_line(&source, &file, 0, source.len(), |line| {
                lines.push((line.blank, line.view.is_some()));
                Ok(Flow::Continue)
            })
            .unwrap();
            assert_eq!(lines, [(false, true), (false, false), (false, true)], "framer, {name}");
            let (at, len) = (valid.len() as u64 + 1, bad.len() as u64);
            let invalid = |result: anyhow::Result<()>| result.is_err_and(|e| e.is::<crate::json::InvalidJson>());
            assert!(
                invalid(source.read(ReadInput::Range(at, len), Validation::Value, |_| Ok(()))),
                "value, {name}"
            );
            assert!(
                invalid(source.read(ReadInput::Range(at, len), Validation::Syntax, |_| Ok(()))),
                "syntax, {name}"
            );
            let mut records = super::Records::new(&source, &file);
            let read = records.read_with(at, len, Validation::Syntax, |_| Ok(()));
            assert!(invalid(read), "records, {name}");
            let derived = crate::derive::transcript(
                &source,
                &file,
                "n",
                crate::harness::claude::IDENTITY,
                &mut std::io::sink(),
            );
            assert!(invalid(derived.map(|_| ())), "derive, {name}");
        }
    }

    /// JSON whitespace is space, tab, LF and CR. A line holding only U+2003 (EM SPACE), short or
    /// past the in-memory limit, is a rejected record to every reader of a transcript (and so
    /// refused by every writer of a derived one), and a line of JSON whitespace is blank to all.
    #[test]
    fn every_path_trims_json_whitespace_only() {
        let world = cv_testkit::World::new("jsonl-json-whitespace");
        let record =
            r#"{"type":"user","uuid":"a","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"hi"}}"#;
        for (name, filler, blank) in [
            ("em", "\u{2003}".to_owned(), false),
            ("em-large", "\u{2003}".repeat(40_000), false),
            ("ascii", " \t\r".to_owned(), true),
            ("ascii-large", " \t".repeat(70_000), true),
        ] {
            let path = world.base.join(format!("{name}.jsonl"));
            let text = format!("{record}\n{filler}\n");
            std::fs::write(&path, &text).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let file = std::fs::File::open(&path).unwrap();

            let mut codec = Vec::new();
            super::for_each_line(&source, &file, 0, source.len(), |line| {
                codec.push((line.blank, line.view.is_some()));
                Ok(Flow::Continue)
            })
            .unwrap();
            assert_eq!(codec, [(false, true), (blank, false)], "codec, {name}");

            let lines = 1 + usize::from(!blank);
            match crate::derive::transcript(
                &source,
                &file,
                "n",
                crate::harness::claude::IDENTITY,
                &mut std::io::sink(),
            ) {
                Ok(counts) => assert!(blank && counts.lines == 1, "restamp, {name}: {counts:?}"),
                Err(error) => assert!(
                    !blank
                        && error.is::<crate::json::InvalidJson>()
                        && error.to_string().contains(&format!("byte {}", record.len() + 1)),
                    "restamp, {name}: {error:#}"
                ),
            }
            assert_eq!(crate::restamp::restamp(&text, "n").lines, lines, "restamp text, {name}");
            assert_eq!(
                crate::harness::for_each_json_line_str(&text, |_| Flow::Continue),
                u64::from(!blank),
                "line reader, {name}"
            );

            let verified = crate::harness::claude::emit::verify_claude_resumable(&path, None);
            assert_eq!(verified.is_ok(), blank, "verify, {name}: {verified:?}");

            let opts = crate::prune::PruneOptions {
                new_id: Some(format!("{name}-pruned")),
                ..Default::default()
            };
            match crate::prune::prune_session(crate::Harness::Claude, &path, &opts) {
                Ok(pruned) => {
                    let output = std::fs::read_to_string(&pruned.new_path).unwrap();
                    assert!(blank && output.lines().count() == 1, "prune, {name}: {output:?}");
                }
                Err(error) => {
                    assert!(
                        !blank && error.is::<crate::json::InvalidJson>(),
                        "prune, {name}: {error:#}"
                    );
                    assert!(error.to_string().contains("line 2"), "prune, {name}: {error:#}");
                    assert!(
                        !world.base.join(format!("{name}-pruned.jsonl")).exists(),
                        "prune, {name}"
                    );
                }
            }
        }
    }

    /// A writer appending half records at a time while passes read: every pass reads to its
    /// captured length without error, each record it yields is whole, a trailing partial line is
    /// held back (no view, not complete), and the source still checks at the end. No sleeps.
    #[test]
    fn a_pass_beside_a_writer_of_half_records_never_errors_or_yields_a_partial() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let world = cv_testkit::World::new("jsonl-live-writer");
        let path = world.base.join("live.jsonl");
        let record = |i: usize| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "p".repeat(i % 300));
        std::fs::write(&path, (0..200).map(record).collect::<String>()).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                for i in 200..20_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let line = record(i);
                    let (head, tail) = line.split_at(line.len() / 2);
                    file.write_all(head.as_bytes()).unwrap();
                    file.write_all(tail.as_bytes()).unwrap();
                }
            });
            for _ in 0..20 {
                let source = crate::json::JsonSource::capture(&path).unwrap();
                let file = std::fs::File::open(&path).unwrap();
                source.check_metadata(file.metadata().unwrap()).unwrap();
                let mut records = Vec::new();
                super::for_each_line(&source, &file, 0, source.len(), |line| {
                    match line.view {
                        Some(view) => {
                            assert!(line.complete, "a record is whole");
                            records.push(view.source_range()?.unwrap());
                        }
                        None => assert!(!line.complete && line.end == source.len(), "only a held-back tail"),
                    }
                    Ok(Flow::Continue)
                })
                .unwrap();
                let bytes = std::fs::read(&path).unwrap();
                for (n, (at, len)) in records.into_iter().enumerate() {
                    assert_eq!(
                        &bytes[at as usize..(at + len) as usize],
                        record(n).trim_end().as_bytes()
                    );
                }
                source.check_metadata(file.metadata().unwrap()).unwrap();
                source.check().unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
        });
    }
}
