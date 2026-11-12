//! The one byte source behind source-backed JSON: bytes in memory (a mapped window, a read line,
//! a private decode) or a positioned file, addressed by absolute [`Range`]. One structural
//! [`Cursor`] navigates either; over bytes its `fill_buf` is the whole remaining slice, so the
//! in-memory build keeps memchr speed. Offsets count consumed bytes, never underlying reads or
//! serde lookahead. Scalar/depth acceptance stays in `store::json`.

use super::super::bytes::{read_at, ByteSource};
use crate::lazy::INLINE_MAX;
use anyhow::{Context, Result};
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, Read};

const WINDOW: usize = 64 * 1024;

/// Containers longer than this (and strings over INLINE_MAX) are remembered while validating,
/// so each later structural selection (record, message, content, block) skips them in O(1).
const SKIP_MIN: u64 = 128;

/// Memoize at most 1024 insertion-ordered long endpoints, keyed by absolute offset. Reads do not
/// refresh eviction order. Nested record wrappers
/// reuse body bounds without retaining offsets for the entire history. Eviction affects only
/// scan cost, not data.
const ENDPOINTS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub offset: u64,
    pub len: u64,
}
impl Range {
    fn end(self) -> io::Result<u64> {
        self.offset
            .checked_add(self.len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source range overflows"))
    }
    /// A quoted string's interior.
    fn body(self) -> Result<Range> {
        anyhow::ensure!(self.len >= 2, "not a quoted JSON string range");
        Ok(Range {
            offset: self.offset.checked_add(1).context("JSON string offset overflows")?,
            len: self.len - 2,
        })
    }
}

/// The record arena's structural memory: long string and container endpoints found while
/// validating or scanning one source, reused by every selection over it, reset with the source.
#[derive(Default)]
pub(crate) struct Index(std::cell::RefCell<Endpoints>);
#[derive(Default)]
struct Endpoints {
    ends: BTreeMap<u64, u64>,
    order: VecDeque<u64>,
}
impl Index {
    fn get(&self, start: u64) -> Option<u64> {
        self.0.borrow().ends.get(&start).copied()
    }
    fn remember(&self, start: u64, end: u64) {
        let mut cache = self.0.borrow_mut();
        if cache.ends.contains_key(&start) {
            return;
        }
        if cache.ends.len() == ENDPOINTS {
            if let Some(old) = cache.order.pop_front() {
                cache.ends.remove(&old);
            }
        }
        cache.ends.insert(start, end);
        cache.order.push_back(start);
    }
}

enum Backing<'a> {
    /// Bytes in memory whose first byte sits at this absolute offset; `true` when they are known
    /// to be UTF-8 (a `str`), so borrowed strings and keys need no second check.
    Bytes(&'a [u8], u64, bool),
    File(File),
}

/// One source of bytes shared by all projected fields in an operation: the captured original (a
/// positioned descriptor or a window of it already in memory) or private bytes derived from it.
/// Independent cursors use positioned reads, so nested views share no seek position.
pub(crate) struct RangeSource<'a> {
    backing: Backing<'a>,
    captured: Option<crate::json::JsonSource>,
    /// The captured original itself, not private bytes derived from it.
    original: bool,
    extent: Range,
    index: Index,
}
impl<'a> RangeSource<'a> {
    /// Bytes already in memory, `base` being the absolute offset of their first byte.
    pub(crate) fn borrowed(
        captured: Option<crate::json::JsonSource>,
        bytes: &'a [u8],
        base: u64,
        original: bool,
    ) -> Self {
        Self {
            backing: Backing::Bytes(bytes, base, false),
            captured,
            original,
            extent: Range {
                offset: base,
                len: bytes.len() as u64,
            },
            index: Index::default(),
        }
    }
    /// [`Self::borrowed`] over text, whose strings need no UTF-8 check when borrowed.
    pub(crate) fn text(captured: Option<crate::json::JsonSource>, text: &'a str, base: u64, original: bool) -> Self {
        let mut source = Self::borrowed(captured, text.as_bytes(), base, original);
        source.backing = Backing::Bytes(text.as_bytes(), base, true);
        source
    }
    /// A borrowed string body as text: unchecked when the backing is known UTF-8.
    fn text_of(&self, body: &'a [u8]) -> Result<&'a str> {
        if let Backing::Bytes(_, _, true) = &self.backing {
            // SAFETY: the backing was built from a `str`, and `body` is a slice of it between
            // a string's quotes (ASCII boundaries), so it is UTF-8.
            return Ok(unsafe { std::str::from_utf8_unchecked(body) });
        }
        simdutf8::basic::from_utf8(body).map_err(|_| crate::json::InvalidJson.into())
    }
    fn of_bytes(captured: Option<crate::json::JsonSource>, bytes: &'a ByteSource, original: bool) -> Result<Self> {
        let len = bytes.byte_len()?;
        let backing = match bytes.memory_bytes() {
            Some(memory) => Backing::Bytes(memory, 0, false),
            None => Backing::File(bytes.file()?),
        };
        Ok(Self {
            backing,
            captured,
            original,
            extent: Range { offset: 0, len },
            index: Index::default(),
        })
    }
    /// Immutable operation-owned bytes need no external source identity or pathname.
    pub(crate) fn private(bytes: &'a ByteSource) -> Result<Self> {
        Self::of_bytes(None, bytes, false)
    }
    #[cfg(test)]
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_captured(crate::json::JsonSource::capture(path)?)
    }
    pub fn from_captured(captured: crate::json::JsonSource) -> Result<Self> {
        let file = File::open(captured.path()).with_context(|| format!("opening {}", captured.path().display()))?;
        Self::from_file(file, captured)
    }
    /// Reuse an already opened original-source descriptor for framing and selected reads.
    pub(crate) fn from_file(file: File, captured: crate::json::JsonSource) -> Result<Self> {
        let source = Self {
            backing: Backing::File(file),
            extent: Range {
                offset: 0,
                len: captured.len(),
            },
            captured: Some(captured),
            original: true,
            index: Index::default(),
        };
        source.check_identity()?;
        Ok(source)
    }
    /// The source's private decoded bytes, with the original's provenance.
    pub(crate) fn decoded(captured: crate::json::JsonSource, bytes: &'a ByteSource) -> Result<Self> {
        let source = Self::of_bytes(Some(captured), bytes, false)?;
        source.check_identity()?;
        Ok(source)
    }
    pub(crate) fn matches_original(&self, captured: &crate::json::JsonSource) -> bool {
        self.original && self.captured.as_ref() == Some(captured)
    }
    pub fn range(&self) -> Range {
        self.extent
    }
    pub fn check_identity(&self) -> Result<()> {
        let Some(captured) = &self.captured else { return Ok(()) };
        if let (true, Backing::File(file)) = (self.original, &self.backing) {
            captured.check_metadata(file.metadata()?)?;
        }
        captured.check()
    }
    fn contains(&self, range: Range) -> io::Result<bool> {
        Ok(range.offset >= self.extent.offset && range.end()? <= self.extent.end()?)
    }
    /// The range's bytes when they are in memory.
    pub(crate) fn borrow(&self, range: Range) -> Option<&'a [u8]> {
        let Backing::Bytes(bytes, base, _) = &self.backing else {
            return None;
        };
        let start = usize::try_from(range.offset.checked_sub(*base)?).ok()?;
        let end = start.checked_add(usize::try_from(range.len).ok()?)?;
        bytes.get(start..end)
    }
    pub fn cursor(&self, range: Range) -> io::Result<Cursor<'_>> {
        if !self.contains(range)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "range extends beyond source",
            ));
        }
        Ok(Cursor {
            backing: match &self.backing {
                Backing::Bytes(bytes, base, _) => CursorBacking::Bytes(bytes, *base),
                Backing::File(file) => CursorBacking::File(file),
            },
            index: Some(&self.index),
            end: range.end()?,
            position: range.offset,
            window: Vec::new(),
            window_start: range.offset,
            read_error: None,
        })
    }
    /// Inspect the first byte of a range without allocating an input window.
    pub fn kind(&self, range: Range) -> Result<Option<u8>> {
        if range.len == 0 {
            return Ok(None);
        }
        anyhow::ensure!(self.contains(range)?, "JSON range outside source");
        if let Some(bytes) = self.borrow(Range { len: 1, ..range }) {
            return Ok(Some(bytes[0]));
        }
        let Backing::File(file) = &self.backing else {
            unreachable!()
        };
        let mut byte = [0];
        loop {
            match read_at(file, &mut byte, range.offset) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "JSON source was truncated").into()),
                Ok(_) => return Ok(Some(byte[0])),
            }
        }
    }
    /// Whether any window of the range satisfies `found`; windows overlap by `overlap` bytes so a
    /// pattern up to that long is never cut.
    fn scan(&self, range: Range, overlap: usize, mut found: impl FnMut(&[u8]) -> bool) -> Result<bool> {
        if let Some(bytes) = self.borrow(range) {
            return Ok(found(bytes));
        }
        let mut cursor = self.cursor(range)?;
        let mut buffer = vec![0u8; WINDOW + overlap];
        let mut kept = 0;
        loop {
            let n = cursor.read(&mut buffer[kept..])?;
            if n == 0 {
                return Ok(false);
            }
            let end = kept + n;
            if found(&buffer[..end]) {
                return Ok(true);
            }
            kept = end.min(overlap);
            buffer.copy_within(end - kept..end, 0);
        }
    }
}

/// The one place the two builds differ for a range of a captured source: mapped builds map that
/// range alone (or borrow the decoded bytes); positioned builds read it in windows.
pub(crate) fn scoped<T>(
    captured: &crate::json::JsonSource,
    decoded: Option<&ByteSource>,
    range: Range,
    f: impl FnOnce(&RangeSource<'_>) -> Result<T>,
) -> Result<T> {
    #[cfg(feature = "mmap")]
    if decoded.is_some()
        || crate::io_admission::byte_source_policy(captured.path()) == crate::io_admission::ByteSourcePolicy::Mapped
    {
        let mut callback = Some(f);
        if let Some(bytes) = decoded {
            if let Some(result) = bytes.with_bytes(|bytes| {
                let start = usize::try_from(range.offset)?;
                let end = start
                    .checked_add(usize::try_from(range.len)?)
                    .context("backing range overflow")?;
                let bytes = bytes.get(start..end).context("backing range out of bounds")?;
                callback.take().unwrap()(&RangeSource::borrowed(
                    Some(captured.clone()),
                    bytes,
                    range.offset,
                    false,
                ))
            })? {
                return result;
            }
        }
        let file = match decoded {
            Some(bytes) => bytes.file()?,
            None => File::open(captured.path())?,
        };
        if decoded.is_none() {
            captured.check_metadata(file.metadata()?)?;
        }
        let map = if range.len == 0 {
            None
        } else {
            // SAFETY: a read-only mapping of a private decoded spill or local sealed immutable file,
            // dropped with this scope.
            Some(unsafe {
                memmap2::MmapOptions::new()
                    .offset(range.offset)
                    .len(usize::try_from(range.len)?)
                    .map(&file)
                    .context("mapping source")?
            })
        };
        #[cfg(test)]
        crate::harness::store::jsonl::note_mapped(range.len as usize);
        let bytes: &[u8] = map.as_deref().unwrap_or(&[]);
        let result = callback.take().unwrap()(&RangeSource::borrowed(
            Some(captured.clone()),
            bytes,
            range.offset,
            decoded.is_none(),
        ));
        if decoded.is_none() {
            captured.check_metadata(file.metadata()?)?;
        }
        return result;
    }
    // Positioned RangeSource: mutable external transcripts never mmap (SIGBUS-immune on concurrent
    // truncation), addressing the capture by absolute offset through bounded read windows.
    let _ = range;
    let source = match decoded {
        Some(bytes) => RangeSource::decoded(captured.clone(), bytes)?,
        None => RangeSource::from_captured(captured.clone())?,
    };
    f(&source)
}

enum CursorBacking<'a> {
    Bytes(&'a [u8], u64),
    File(&'a File),
}
pub(crate) struct Cursor<'a> {
    backing: CursorBacking<'a>,
    index: Option<&'a Index>,
    end: u64,
    position: u64,
    window: Vec<u8>,
    window_start: u64,
    read_error: Option<io::ErrorKind>,
}
impl<'a> Cursor<'a> {
    #[cfg(test)]
    pub fn bytes(bytes: &'a [u8], base: u64) -> io::Result<Self> {
        let range = Range {
            offset: base,
            len: bytes.len() as u64,
        };
        Ok(Self {
            backing: CursorBacking::Bytes(bytes, base),
            index: None,
            end: range.end()?,
            position: base,
            window: Vec::new(),
            window_start: base,
            read_error: None,
        })
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.fill_buf()?.first().copied())
    }
    pub fn whitespace(&mut self) -> io::Result<()> {
        loop {
            let bytes = self.fill_buf()?;
            let n = bytes
                .iter()
                .take_while(|b| crate::harness::store::json::is_whitespace(b))
                .count();
            if n == 0 {
                return Ok(());
            }
            self.consume(n);
        }
    }
    #[cfg(test)]
    pub fn borrow_range(&self, range: Range) -> Option<&[u8]> {
        let (bytes, base) = match &self.backing {
            CursorBacking::Bytes(b, base) => (*b, *base),
            CursorBacking::File(_) => (self.window.as_slice(), self.window_start),
        };
        let start = usize::try_from(range.offset.checked_sub(base)?).ok()?;
        let end = start.checked_add(usize::try_from(range.len).ok()?)?;
        bytes.get(start..end)
    }
    fn remembered(&self, start: u64) -> io::Result<Option<u64>> {
        // A narrower child cursor never jumps beyond its own accepted extent.
        Ok(self
            .index
            .and_then(|index| index.get(start))
            .filter(|&end| end > start && end <= self.end))
    }
    fn remember(&self, start: u64, min: u64) {
        if let Some(index) = self.index {
            if self.position - start > min {
                index.remember(start, self.position);
            }
        }
    }
    /// Only structural navigation; the caller validates grammar and scalar acceptance once at
    /// the document/record boundary. Escapes may span windows without decoding their payload.
    pub fn string(&mut self) -> io::Result<Range> {
        let start = self.position();
        if self.peek()? != Some(b'"') {
            return Err(invalid("expected JSON string"));
        }
        if let Some(end) = self.remembered(start)? {
            self.position = end;
            return Ok(Range {
                offset: start,
                len: end - start,
            });
        }
        self.consume(1);
        // Only quotes can end a string. Carry slash parity across refill boundaries so
        // dense escaped bodies remain vectorized rather than visiting every backslash.
        // An all-backslash window extends the previous run, so XOR its parity. Any intervening
        // non-backslash ends that run; a quote inherits prior parity only when its entire
        // window prefix is backslashes (slashes == n).
        let mut trailing_odd = false;
        loop {
            let bytes = self.fill_buf()?;
            if bytes.is_empty() {
                return Err(invalid("unterminated JSON string"));
            }
            let Some(n) = memchr::memchr(b'"', bytes) else {
                let trailing = bytes.iter().rev().take_while(|&&b| b == b'\\').count();
                trailing_odd = if trailing == bytes.len() {
                    trailing_odd ^ (trailing % 2 != 0)
                } else {
                    trailing % 2 != 0
                };
                let n = bytes.len();
                self.consume(n);
                continue;
            };
            let slashes = bytes[..n].iter().rev().take_while(|&&b| b == b'\\').count();
            let escaped = (slashes % 2 != 0) ^ (slashes == n && trailing_odd);
            self.consume(n + 1);
            if !escaped {
                self.remember(start, INLINE_MAX as u64);
                return Ok(Range {
                    offset: start,
                    len: self.position - start,
                });
            }
            trailing_odd = false;
        }
    }

    pub fn value(&mut self) -> io::Result<Range> {
        self.whitespace()?;
        let start = self.position();
        match self.peek()? {
            Some(b'"') => return self.string(),
            Some(b'{' | b'[') => {
                if let Some(end) = self.remembered(start)? {
                    self.position = end;
                    return Ok(Range {
                        offset: start,
                        len: end - start,
                    });
                }
                self.consume(1);
                let mut depth = 1usize;
                while depth != 0 {
                    let bytes = self.fill_buf()?;
                    if bytes.is_empty() {
                        return Err(invalid("unterminated JSON container"));
                    }
                    // Walk the window for structure; a string is skipped whole by `string`.
                    let mut i = 0;
                    let mut quote = false;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'"' => {
                                quote = true;
                                break;
                            }
                            b'{' | b'[' => depth = depth.checked_add(1).ok_or_else(|| invalid("nesting overflow"))?,
                            b'}' | b']' => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                    self.consume(i);
                    if quote {
                        self.string()?;
                    }
                }
                self.remember(start, SKIP_MIN);
            }
            Some(_) => loop {
                let bytes = self.fill_buf()?;
                let n = bytes
                    .iter()
                    .take_while(|b| !crate::harness::store::json::is_whitespace(b) && !b",]}".contains(b))
                    .count();
                if n == 0 {
                    break;
                }
                self.consume(n);
            },
            None => return Err(invalid("expected JSON value")),
        }
        if self.position == start {
            return Err(invalid("expected JSON value"));
        }
        Ok(Range {
            offset: start,
            len: self.position - start,
        })
    }
}
impl BufRead for Cursor<'_> {
    #[inline]
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self.backing {
            CursorBacking::Bytes(bytes, base) => {
                Ok(&bytes[(self.position - base) as usize..(self.end - base) as usize])
            }
            CursorBacking::File(_) if self.position == self.end => Ok(&[]),
            CursorBacking::File(file) => {
                let consumed = usize::try_from(self.position - self.window_start).unwrap_or(usize::MAX);
                if consumed < self.window.len() {
                    return Ok(&self.window[consumed..]);
                }
                self.window_start = self.position;
                let n = (self.end - self.position).min(WINDOW as u64) as usize;
                self.window.resize(n, 0);
                let n = loop {
                    match read_at(file, &mut self.window, self.position) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            self.read_error = Some(e.kind());
                            return Err(e);
                        }
                        Ok(n) => break n,
                    }
                };
                if n == 0 {
                    self.read_error = Some(io::ErrorKind::UnexpectedEof);
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source was truncated"));
                }
                self.window.truncate(n);
                Ok(&self.window)
            }
        }
    }
    #[inline]
    fn consume(&mut self, n: usize) {
        debug_assert!(n as u64 <= self.end - self.position, "cursor consumed beyond range");
        self.position += n as u64;
    }
}
impl Read for Cursor<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let bytes = self.fill_buf()?;
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        self.consume(n);
        Ok(n)
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
/// A positioned range read in windows for one validation pass: the byte source behind
/// [`validate_value`](super::super::json::validate_value) when the bytes are not in memory.
/// Indexes are relative to the range; a read failure is remembered and ends the pass.
struct Windowed<'s> {
    source: &'s RangeSource<'s>,
    range: Range,
    buf: Vec<u8>,
    /// Relative index of `buf[0]`.
    start: usize,
    error: Option<io::Error>,
}
impl<'s> Windowed<'s> {
    fn new(source: &'s RangeSource<'s>, range: Range) -> Self {
        Self {
            source,
            range,
            buf: Vec::new(),
            start: 0,
            error: None,
        }
    }
    /// Make `i..i+len` resident when it exists; the window slides to start at `i`.
    fn load(&mut self, i: usize, len: usize) -> bool {
        if self.error.is_some() {
            return false;
        }
        let end = match i.checked_add(len) {
            Some(end) if end as u64 <= self.range.len => end,
            _ => return false,
        };
        if i >= self.start && end <= self.start + self.buf.len() {
            return true;
        }
        let want = (self.range.len - i as u64).min(WINDOW.max(len) as u64) as usize;
        let window = Range {
            offset: self.range.offset + i as u64,
            len: want as u64,
        };
        self.buf.clear();
        let read = self
            .source
            .cursor(window)
            .and_then(|mut cursor| cursor.read_to_end(&mut self.buf));
        match read {
            Ok(n) if n == want => {
                self.start = i;
                true
            }
            Ok(_) => {
                self.error = Some(io::Error::new(io::ErrorKind::UnexpectedEof, "source was truncated"));
                false
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }
}
impl super::super::json::ValidationBytes for Windowed<'_> {
    fn get(&mut self, i: usize) -> Option<u8> {
        self.load(i, 1).then(|| self.buf[i - self.start])
    }
    fn slice(&mut self, i: usize, len: usize) -> Option<&[u8]> {
        self.load(i, len)
            .then(|| &self.buf[i - self.start..i - self.start + len])
    }
    fn utf8(&mut self, i: usize, end: usize) -> bool {
        if i >= self.start && end <= self.start + self.buf.len() {
            return simdutf8::basic::from_utf8(&self.buf[i - self.start..end - self.start]).is_ok();
        }
        let body = Range {
            offset: self.range.offset + i as u64,
            len: (end - i) as u64,
        };
        match self
            .source
            .cursor(body)
            .and_then(|mut cursor| super::super::jsonl::ranges::valid_utf8(&mut cursor))
        {
            Ok(valid) => valid,
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }
    fn number(&mut self, i: usize, end: usize, strict: bool) -> bool {
        if let Some(token) = self.slice(i, end - i) {
            return super::super::json::number_accepted(token, strict);
        }
        false
    }
}

/// How a range is accepted before it is viewed. `Cached`: accepted already, at capture.
#[derive(Clone, Copy)]
pub(crate) enum Validation {
    Cached,
    /// Exactly `serde_json::Value`'s language.
    Value,
    /// JSON grammar and UTF-8 only; typed consumers accept their known fields themselves.
    Syntax,
}

impl<'a> RangeSource<'a> {
    /// Accept `range` under `validation`, returning the value's range without its surrounding
    /// JSON whitespace, or None when it is not accepted. The one validator runs over the bytes in
    /// memory, or over the positioned range in windows; it indexes long fields as it goes.
    pub(crate) fn accept(&self, range: Range, validation: Validation) -> Result<Option<Range>> {
        let strict = match validation {
            Validation::Cached => return Ok(Some(range)),
            Validation::Value => true,
            Validation::Syntax => false,
        };
        Ok(self.validated(range, strict, 0)?.map(|(start, end)| Range {
            offset: range.offset + start as u64,
            len: (end - start) as u64,
        }))
    }
    /// [`validate_value`](super::super::json::validate_value) over the range's bytes, remembering
    /// every long string and container as it closes (the index is bounded; nothing is collected).
    fn validated(&self, range: Range, strict: bool, depth: usize) -> Result<Option<(usize, usize)>> {
        anyhow::ensure!(self.contains(range)?, "JSON range outside source");
        let span = |kind: u8, start: usize, end: usize| {
            let (start, end) = (start as u64, end as u64);
            if end - start > if kind == b'"' { INLINE_MAX as u64 } else { SKIP_MIN } {
                self.index.remember(range.offset + start, range.offset + end);
            }
        };
        if let Some(bytes) = self.borrow(range) {
            return Ok(if let Backing::Bytes(_, _, true) = &self.backing {
                super::super::json::validate_value(&mut super::super::json::Text(bytes), strict, depth, span)
            } else {
                super::super::json::validate_value(&mut &bytes[..], strict, depth, span)
            });
        }
        let mut windowed = Windowed::new(self, range);
        let accepted = super::super::json::validate_value(&mut windowed, strict, depth, span);
        if let Some(error) = windowed.error {
            return Err(error.into());
        }
        Ok(accepted)
    }

    /// Scalar/depth validation of a value already accepted by grammar: Value's finite numbers,
    /// paired surrogates and nesting limit, from `depth` levels already open.
    pub fn accepts_value_at_depth(&self, range: Range, depth: usize) -> Result<bool> {
        Ok(self.validated(range, true, depth)?.is_some())
    }
    /// Explicit materialization is for demanded typed metadata/output, not structural traversal.
    pub fn materialize<T: serde::de::DeserializeOwned>(&self, range: Range) -> Result<T> {
        match self.borrow(range) {
            Some(bytes) => Ok(serde_json::from_slice(bytes)?),
            None => Ok(serde_json::from_reader(self.cursor(range)?)?),
        }
    }
    /// The one string decoder: an unescaped string in memory is visited whole and borrowed;
    /// anything else goes through the body codec in bounded windows. Stops on callback error.
    /// The enclosing operation checks source identity even after an intentional early stop.
    pub fn string_chunks(&self, string: Range, visit: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        anyhow::ensure!(string.len >= 2, "not a JSON string");
        if let Some(bytes) = self.borrow(string) {
            let body = &bytes[1..bytes.len() - 1];
            if !body.contains(&b'\\') {
                return visit(self.text_of(body)?);
            }
        } else if string.len <= INLINE_MAX as u64 + 2 {
            let decoded: String = self.materialize(string)?;
            return visit(&decoded);
        }
        let mut cursor = self.cursor(string.body()?)?;
        let mut callback_error = None;
        let decoded = super::super::json::decode_body(&mut cursor, &mut |part| {
            callback_error = visit(part).err();
            callback_error.is_none()
        });
        if let Some(kind) = cursor.read_error {
            return Err(io::Error::new(kind, "reading JSON string").into());
        }
        if let Some(error) = callback_error {
            return Err(error);
        }
        decoded
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("validated JSON string failed decoding"))
    }
    /// A key as text: borrowed when it is in memory without escapes, decoded otherwise.
    fn key(&self, string: Range) -> Result<Cow<'a, str>> {
        if let Some(bytes) = self.borrow(string) {
            let body = &bytes[1..bytes.len() - 1];
            if !body.contains(&b'\\') {
                return Ok(Cow::Borrowed(self.text_of(body)?));
            }
        }
        Ok(Cow::Owned(self.materialize(string)?))
    }
    pub fn array(&self, range: Range) -> Result<Option<ArrayRanges<'_>>> {
        let mut cursor = self.cursor(range)?;
        cursor.whitespace()?;
        if cursor.peek()? != Some(b'[') {
            return Ok(None);
        }
        cursor.consume(1);
        Ok(Some(ArrayRanges { cursor, done: false }))
    }
    pub fn object(&self, range: Range) -> Result<Option<ObjectRanges<'_>>> {
        let mut cursor = self.cursor(range)?;
        cursor.whitespace()?;
        if cursor.peek()? != Some(b'{') {
            return Ok(None);
        }
        cursor.consume(1);
        Ok(Some(ObjectRanges { cursor, done: false }))
    }
    /// Visit each requested field of an object in wire order (a later duplicate visits again, so
    /// the last visit wins). False for a nonobject. Keys obey Value's final-duplicate rule; no
    /// ignored key or descendant payload is retained.
    pub fn select_each(
        &self,
        range: Range,
        keys: &[&str],
        unique: bool,
        visit: &mut dyn FnMut(usize, Range) -> Result<()>,
    ) -> Result<bool> {
        let Some(entries) = self.object(range)? else {
            return Ok(false);
        };
        // An escaped ASCII byte needs at most six encoded bytes; surrogate pairs encode one
        // four-byte UTF-8 scalar in twelve bytes. A longer key cannot equal a requested name.
        let key_limit = (keys.iter().map(|key| key.len()).max().unwrap_or(0) as u64)
            .saturating_mul(6)
            .saturating_add(2);
        for entry in entries {
            let (key, value) = entry?;
            if unique && !self.accepts_value_at_depth(key, 0)? {
                return Err(super::InvalidStruct("invalid object key").into());
            }
            if key.len > key_limit {
                continue;
            }
            let key = self.key(key)?;
            for (i, requested) in keys.iter().enumerate() {
                if key == *requested {
                    visit(i, value)?;
                }
            }
        }
        Ok(true)
    }
    /// An object's entries as Value keeps them: each key at its first position, with its final
    /// value. None for a nonobject. Narrow objects dedupe by scanning; wide ones by a position map.
    /// Retains all decoded keys (borrowing when possible); prefer get/select_each for selected keys.
    pub fn entries(&self, range: Range) -> Result<Option<Entries<'a>>> {
        let Some(iter) = self.object(range)? else {
            return Ok(None);
        };
        let mut entries: Entries<'a> = Vec::new();
        let mut positions: Option<std::collections::HashMap<String, usize>> = None;
        for entry in iter {
            let (key, value) = entry?;
            let key = self.key(key)?;
            if entries.len() == 16 && positions.is_none() {
                positions = Some(
                    entries
                        .iter()
                        .enumerate()
                        .map(|(i, (k, _))| (k.to_string(), i))
                        .collect(),
                );
            }
            let existing = match &positions {
                Some(positions) => positions.get(key.as_ref()).copied(),
                None => entries.iter().position(|(k, _)| *k == key),
            };
            match existing {
                Some(i) => entries[i].1 = value,
                None => {
                    if let Some(positions) = &mut positions {
                        positions.insert(key.to_string(), entries.len());
                    }
                    entries.push((key, value));
                }
            }
        }
        Ok(Some(entries))
    }
    /// Conservative printable-ASCII presence without decoding a retained JSON string.
    /// False proves absence; escaped backslashes may create harmless false positives.
    pub fn string_may_contain_ascii(&self, string: Range, byte: u8) -> Result<bool> {
        anyhow::ensure!((0x20..=0x7e).contains(&byte), "expected printable ASCII");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut escape = [
            b'\\',
            b'u',
            b'0',
            b'0',
            HEX[(byte >> 4) as usize],
            HEX[(byte & 15) as usize],
        ];
        let upper = {
            escape[5].make_ascii_uppercase();
            let upper = escape;
            escape[5].make_ascii_lowercase();
            upper
        };
        self.scan(string, 5, |raw| {
            memchr::memchr(byte, raw).is_some()
                || memchr::memmem::find(raw, &escape).is_some()
                || memchr::memmem::find(raw, &upper).is_some()
        })
    }
    /// Scan with constant state and final-key-wins semantics, including discarded duplicate keys.
    pub fn get(&self, range: Range, key: &str) -> Result<Option<Range>> {
        let mut found = None;
        self.select_each(range, &[key], false, &mut |_, value| {
            found = Some(value);
            Ok(())
        })?;
        Ok(found)
    }
}

/// An object's keys (borrowed when in memory without escapes) and value ranges, deduplicated.
pub(crate) type Entries<'a> = Vec<(Cow<'a, str>, Range)>;

pub(crate) struct ArrayRanges<'a> {
    cursor: Cursor<'a>,
    done: bool,
}
impl Iterator for ArrayRanges<'_> {
    type Item = io::Result<Range>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            self.cursor.whitespace()?;
            if self.cursor.peek()? == Some(b']') {
                self.done = true;
                return Ok(None);
            }
            let value = self.cursor.value()?;
            self.cursor.whitespace()?;
            match self.cursor.peek()? {
                Some(b',') => self.cursor.consume(1),
                Some(b']') => self.done = true,
                _ => return Err(invalid("unvalidated JSON array delimiter")),
            }
            Ok(Some(value))
        })();
        match result {
            Ok(value) => value.map(Ok),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
pub(crate) struct ObjectRanges<'a> {
    cursor: Cursor<'a>,
    done: bool,
}
impl Iterator for ObjectRanges<'_> {
    type Item = io::Result<(Range, Range)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            self.cursor.whitespace()?;
            if self.cursor.peek()? == Some(b'}') {
                self.done = true;
                return Ok(None);
            }
            let key = self.cursor.string()?;
            self.cursor.whitespace()?;
            if self.cursor.peek()? != Some(b':') {
                return Err(invalid("unvalidated JSON field delimiter"));
            }
            self.cursor.consume(1);
            let value = self.cursor.value()?;
            self.cursor.whitespace()?;
            match self.cursor.peek()? {
                Some(b',') => self.cursor.consume(1),
                Some(b'}') => self.done = true,
                _ => return Err(invalid("unvalidated JSON object delimiter")),
            }
            Ok(Some((key, value)))
        })();
        match result {
            Ok(value) => value.map(Ok),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;

    #[test]
    fn bounded_validation_matches_value_and_navigation_keeps_final_escaped_keys() {
        let path = std::env::temp_dir().join(format!("cv-range-validation-{}", uuid::Uuid::new_v4()));
        let mut cases = [
            "null",
            "true",
            "0",
            "-0",
            "1e400",
            "1e-400",
            "18446744073709551616",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\ud83c\\udf19\"",
            "\"\\\\ud800\"",
            "{\"x\":1e400,\"x\":0}",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
        }
        cases.extend([
            format!("\"{}\\ud800\"", "\\u0061".repeat(WINDOW)),
            format!("\"{}é😀\"", "a".repeat(WINDOW - 2)),
            format!("1.{}9", "0".repeat(WINDOW * 2)),
            format!("1e-{}", "9".repeat(WINDOW * 2)),
        ]);
        for case in cases {
            std::fs::write(&path, &case).unwrap();
            let source = RangeSource::open(&path).unwrap();
            let expected = serde_json::from_str::<serde_json::Value>(&case).is_ok();
            assert_eq!(
                source.accept(source.range(), Validation::Value).unwrap().is_some(),
                expected,
                "{} bytes",
                case.len()
            );
            let bytes = std::fs::read(&path).unwrap();
            let memory = RangeSource::borrowed(None, &bytes, 0, false);
            assert_eq!(
                memory.accept(memory.range(), Validation::Value).unwrap().is_some(),
                expected,
                "in memory: {} bytes",
                case.len()
            );
        }
        let document = r#" {"z":1,"a":[null,{"x":"tail"}],"\u007a":2} "#;
        std::fs::write(&path, document).unwrap();
        let bytes = document.as_bytes().to_vec();
        for source in [
            RangeSource::open(&path).unwrap(),
            RangeSource::borrowed(None, &bytes, 0, false),
        ] {
            let value = source.accept(source.range(), Validation::Value).unwrap().unwrap();
            assert_eq!(
                value,
                Range {
                    offset: 1,
                    len: document.len() as u64 - 2
                }
            );
            let z = source.get(value, "z").unwrap().unwrap();
            assert_eq!(
                source.materialize::<serde_json::Value>(z).unwrap(),
                serde_json::json!(2)
            );
            assert!(source.get(value, "missing").unwrap().is_none());
            let a = source.get(value, "a").unwrap().unwrap();
            let values = source
                .array(a)
                .unwrap()
                .unwrap()
                .map(|range| source.materialize::<serde_json::Value>(range.unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(values, [serde_json::Value::Null, serde_json::json!({"x":"tail"})]);
            let keys = source
                .entries(value)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|(key, _)| key.into_owned())
                .collect::<Vec<_>>();
            assert_eq!(keys, ["z", "a"]);
        }
        std::fs::write(&path, b"{\"ignored\":\"\xff\"}").unwrap();
        let invalid = RangeSource::open(&path).unwrap();
        assert!(invalid.accept(invalid.range(), Validation::Syntax).unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    #[test]
    fn cached_endpoint_never_extends_a_requested_range() {
        let path = std::env::temp_dir().join(format!("cv-range-cache-{}", uuid::Uuid::new_v4()));
        let text = format!("  \"{}\"  ", "\\n\\\"".repeat(WINDOW));
        std::fs::write(&path, &text).unwrap();
        let source = RangeSource::open(&path).unwrap();
        let expected = source.accept(source.range(), Validation::Value).unwrap().unwrap();
        assert!(source.index.get(expected.offset).is_some());
        assert_eq!(source.cursor(source.range()).unwrap().value().unwrap(), expected);
        assert_eq!(source.cursor(source.range()).unwrap().value().unwrap(), expected);
        let bounded = Range {
            offset: expected.offset,
            len: expected.len - 1,
        };
        assert!(source.cursor(bounded).unwrap().string().is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_late_refill_failure_is_a_read_error_not_an_invalid_scalar_or_success() {
        let path = std::env::temp_dir().join(format!("cv-range-refill-{}", uuid::Uuid::new_v4()));
        let encoded = serde_json::to_string(&"x".repeat(WINDOW * 3)).unwrap();
        std::fs::write(&path, &encoded).unwrap();
        let source = RangeSource::open(&path).unwrap();
        assert!(source.accept(source.range(), Validation::Value).unwrap().is_some());
        let mut callbacks = 0;
        let result = source.string_chunks(source.range(), &mut |_| {
            callbacks += 1;
            if callbacks == 1 {
                std::fs::write(&path, "x").unwrap();
            }
            Ok(())
        });
        assert!(callbacks > 0);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(source.check_identity().is_err());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_roster_keeps_original_ranges_in_both_backings() {
        let body = "雪\\".repeat(4000);
        let encoded = format!(
            "{{\"id\":1,\"\\u0069d\":2,\"ignored{}\":null,\"body\":{}}}",
            "x".repeat(20000),
            serde_json::to_string(&body).unwrap()
        );
        let path = std::env::temp_dir().join(format!("cv-range-fields-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, &encoded).unwrap();
        let bytes = encoded.as_bytes().to_vec();
        for source in [
            RangeSource::open(&path).unwrap(),
            RangeSource::borrowed(None, &bytes, 0, false),
        ] {
            let range = source.accept(source.range(), Validation::Value).unwrap().unwrap();
            let mut fields = [None; 4];
            assert!(source
                .select_each(range, &["id", "id", "missing", "body"], false, &mut |i, value| {
                    fields[i] = Some(value);
                    Ok(())
                })
                .unwrap());
            let mut visits = 0;
            assert!(source
                .select_each(range, &["id", "body"], true, &mut |_, _| {
                    visits += 1;
                    Ok(())
                })
                .unwrap());
            assert_eq!(visits, 3, "a duplicate key visits again; the typed roster rejects it");
            assert_eq!(fields[0], fields[1]);
            assert!(fields[2].is_none());
            assert_eq!(source.materialize::<u64>(fields[0].unwrap()).unwrap(), 2);
            let mut decoded = String::new();
            source
                .string_chunks(fields[3].unwrap(), &mut |part| {
                    decoded.push_str(part);
                    Ok(())
                })
                .unwrap();
            assert_eq!(decoded, body);
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn quote_search_preserves_slash_parity_across_windows() {
        let path = std::env::temp_dir().join(format!("cv-range-parity-{}", uuid::Uuid::new_v4()));
        for body in [
            r"\u0061".repeat(WINDOW),
            r"\\".repeat(WINDOW + 1),
            "a".repeat(WINDOW - 2) + r#"\"after"#,
            "a".repeat(WINDOW - 3) + r#"\\"#,
            "a".repeat(WINDOW - 1) + &r"\\".repeat(WINDOW) + r#"\"after"#,
        ] {
            let encoded = format!("\"{body}\" null");
            let string_len = encoded.len() - 5;
            std::fs::write(&path, &encoded).unwrap();
            let source = RangeSource::open(&path).unwrap();
            let range = source.cursor(source.range()).unwrap().string().unwrap();
            assert_eq!(
                range,
                Range {
                    offset: 0,
                    len: string_len as u64
                }
            );
            assert!(serde_json::from_str::<String>(&encoded[..string_len]).is_ok());
            let mut bytes = Cursor::bytes(encoded.as_bytes(), 0).unwrap();
            assert_eq!(bytes.string().unwrap(), range);
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn byte_and_positioned_cursors_agree_across_escaped_window_boundaries() {
        let path = std::env::temp_dir().join(format!("cv-range-cursor-{}", uuid::Uuid::new_v4()));
        let values = [
            serde_json::json!("a".repeat(WINDOW - 2) + "\\\"é😀"),
            serde_json::json!({"x":[1,true,null,"end"]}),
            serde_json::json!(-1.25e30),
        ];
        let encoded = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join(" \n");
        std::fs::write(&path, &encoded).unwrap();
        let source = RangeSource::open(&path).unwrap();
        let mut file = source.cursor(source.range()).unwrap();
        let mut bytes = Cursor::bytes(encoded.as_bytes(), 0).unwrap();
        for expected in values {
            let fr = file.value().unwrap();
            let br = bytes.value().unwrap();
            assert_eq!(fr, br);
            assert_eq!(file.position(), bytes.position());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(bytes.borrow_range(br).unwrap()).unwrap(),
                expected
            );
            let mut scalar = source.cursor(fr).unwrap();
            assert_eq!(
                serde_json::from_reader::<_, serde_json::Value>(&mut scalar).unwrap(),
                expected
            );
        }
        file.whitespace().unwrap();
        assert_eq!(file.peek().unwrap(), None);
        assert_eq!(source.kind(source.range()).unwrap(), Some(b'"'));
        let first = source.cursor(source.range()).unwrap().value().unwrap();
        let mut chunks = 0;
        let error = source
            .string_chunks(first, &mut |_| {
                chunks += 1;
                anyhow::bail!("stop after first bounded chunk")
            })
            .unwrap_err();
        assert_eq!(chunks, 1);
        assert!(error.to_string().contains("stop after first"));
        source.check_identity().unwrap();
        std::fs::write(&path, "truncated").unwrap();
        assert!(source.check_identity().is_err());
        let mut truncated = source.cursor(source.range()).unwrap();
        assert!(truncated.read_to_end(&mut Vec::new()).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn container_scan_and_endpoint_eviction_preserve_records() {
        let body = "a\\\"x".repeat(2048);
        let values = (0..ENDPOINTS * 3)
            .map(|n| serde_json::json!({"n":n,"body":body}))
            .collect::<Vec<_>>();
        let text = serde_json::to_string(&values).unwrap();
        let source = RangeSource::borrowed(None, text.as_bytes(), 0, false);
        let range = source.accept(source.range(), Validation::Value).unwrap().unwrap();
        assert!(source.index.0.borrow().ends.len() <= ENDPOINTS);
        let items = source
            .array(range)
            .unwrap()
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(items.len(), values.len());
        for (item, expected) in items.into_iter().zip(&values) {
            let n = source.get(item, "n").unwrap().unwrap();
            assert_eq!(source.materialize::<serde_json::Value>(n).unwrap(), expected["n"]);
            let body = source.get(item, "body").unwrap().unwrap();
            assert_eq!(source.materialize::<serde_json::Value>(body).unwrap(), expected["body"]);
            assert!(source.index.0.borrow().ends.len() <= ENDPOINTS);
        }
    }
}

#[cfg(test)]
mod syntax_tests {
    use super::*;
    use serde::Deserialize;

    fn oracle(bytes: &[u8]) -> bool {
        let mut parser = serde_json::Deserializer::from_slice(bytes);
        serde::de::IgnoredAny::deserialize(&mut parser)
            .and_then(|_| parser.end())
            .is_ok()
            && std::str::from_utf8(bytes).is_ok()
    }

    #[test]
    fn a_source_truncated_during_validation_is_a_read_error_not_a_rejection() {
        let path = std::env::temp_dir().join(format!("cv-syntax-io-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("[null,\"{}\"]", "a".repeat(WINDOW * 3))).unwrap();
        let source = RangeSource::open(&path).unwrap();
        let mut windowed = Windowed::new(&source, source.range());
        assert_eq!(
            super::super::super::json::ValidationBytes::get(&mut windowed, 0),
            Some(b'[')
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();
        assert!(super::super::super::json::validate_value(&mut windowed, true, 0, |_, _, _| {}).is_none());
        assert!(windowed.error.is_some());
        assert!(source.accept(source.range(), Validation::Value).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounded_string_grammar_matches_ignored_any_without_value_narrowing() {
        let path = std::env::temp_dir().join(format!("cv-range-syntax-{}", uuid::Uuid::new_v4()));
        let mut cases = vec![
            br#"{"a":"x","b":[true,null,-1.2e3],"a":2}"#.to_vec(),
            br#"{"\ud800":"\udc00","a":1e9999}"#.to_vec(),
            br#"["\ud800x","\udc00","\ud800\u0041"]"#.to_vec(),
            br#"{"x" "y"}"#.to_vec(),
            br#"["a""b"]"#.to_vec(),
            br#"[true,]"#.to_vec(),
            br#"{1:"x"}"#.to_vec(),
            br#""abc\q""#.to_vec(),
            br#""abc\u00xz""#.to_vec(),
            b"\"invalid\xff\"".to_vec(),
            b"\"raw\ncontrol\"".to_vec(),
            b"\"unterminated".to_vec(),
        ];
        for delta in 0..16 {
            let pad = "a".repeat(WINDOW - 16 + delta);
            for tail in [
                r"\ud800",
                r"\udc00",
                r"\ud800\udc00",
                r"\ud800x",
                r#"\\\""#,
                "é😀",
                r"\u000x",
                "\\",
            ] {
                cases.push(format!("[\"{pad}{tail}\",null]").into_bytes());
            }
        }
        for token in [b"null".as_slice(), b"true", b"1.0e10", br#""\ud800""#, br#"{"a":[1]}"#] {
            for end in 0..=token.len() {
                cases.push(token[..end].to_vec());
            }
        }
        for bytes in cases {
            std::fs::write(&path, &bytes).unwrap();
            let source = RangeSource::open(&path).unwrap();
            assert_eq!(
                source.accept(source.range(), Validation::Syntax).unwrap().is_some(),
                oracle(&bytes),
                "{} bytes: {:?}",
                bytes.len(),
                &bytes[bytes.len().saturating_sub(32)..]
            );
            let memory = RangeSource::borrowed(None, &bytes, 0, false);
            assert_eq!(
                memory.accept(memory.range(), Validation::Syntax).unwrap().is_some(),
                oracle(&bytes),
                "in memory, {} bytes",
                bytes.len()
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
