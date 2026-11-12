//! Lazy content: the IR holds large text as a **span** into its source file, not as owned bytes.
//!
//! A fully-parsed [`Session`](crate::ir::Session) used to *own* every byte of content
//! (`Block::Text { text: String }`, …) — 700 MB resident just to hold the IR of a transcript with
//! one 700 MB record, even if the consumer only wants its title. There's no need: the bytes live on
//! disk in a read-only transcript. So a content field is a [`Text`] that's either materialized
//! [`Inline`](Text::Inline) (small — most messages) or [`Source`](Text::Source) (large): a JSON
//! string left in the read's checked capture of its file, resolved **on demand**. A [`Span`] is the
//! serialized wire form of the same thing (offsets into a file), resolved by a [`Resolver`] through
//! a checked capture too.
//!
//! Invariant: reading lazy text as `&str` (via [`Deref`]) panics — you must
//! [`try_resolve`](Text::try_resolve) it, or [`materialize`](crate::ir::Session::materialize) the
//! session first.
//! Streaming consumers resolve per-block (peak = one field); whole-session consumers materialize
//! (peak = whole session, their choice). Below the inline threshold everything is `Inline`, where
//! `Deref`/`Display` just work.

mod source;
pub use source::SourceText;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A persisted output's head as one demand found it.
#[derive(Debug, PartialEq, Eq)]
pub enum PersistedHead {
    /// No file at the path: the output was never kept, or was cleaned up.
    Absent,
    Text(String),
    /// The file exists but cannot be read here (permissions, not a regular file): a session that
    /// references it degrades to its transcript, the reason noted, rather than failing.
    Unreadable(String),
}

impl PersistedHead {
    /// The text a reader indexes for this output: its head, a note naming why it is missing, or
    /// nothing when it is absent.
    pub fn indexed_text(&self, path: &str) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Text(text) => Some(text.clone()),
            Self::Unreadable(reason) => Some(format!("[persisted output {path} unreadable: {reason}]")),
        }
    }
}

/// Read at most `max_bytes` from the head of a sidecar text file (a persisted tool output, say) as
/// lossy UTF-8, so indexers can search text the model saw only via a stub without loading a
/// whole dump. Absence and a file that cannot be opened are answers;
/// a change while it is read is an error. Identity is captured for this demand, without a
/// historical source stamp.
pub fn read_head_checked(path: &Path, max_bytes: usize) -> anyhow::Result<PersistedHead> {
    let mut text = String::new();
    let max = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    Ok(
        match read_checked(path, max, max_bytes.clamp(1, 1 << 20), &mut |piece| {
            text.push_str(piece);
            Ok(())
        })? {
            PersistedHead::Text(_) => PersistedHead::Text(text),
            other => other,
        },
    )
}

/// [`read_head_checked`] over the whole file, handed to `each` in pieces of at most `piece` bytes
/// cut on character boundaries (invalid UTF-8 replaced), so a reader takes a sidecar of any size
/// in bounded memory. On success the `Text` is empty: the text went to `each`.
pub fn read_pieces_checked(
    path: &Path,
    piece: usize,
    mut each: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<PersistedHead> {
    read_checked(path, u64::MAX, piece.max(4), &mut each)
}

fn read_checked(
    path: &Path,
    max: u64,
    piece: usize,
    each: &mut dyn FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<PersistedHead> {
    use std::io::Read;
    let path = std::path::absolute(path)?;
    let unreadable = |error: &dyn std::fmt::Display| PersistedHead::Unreadable(format!("{error:#}"));
    let captured = match crate::json::JsonSource::capture(&path) {
        Ok(captured) => captured,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(PersistedHead::Absent)
        }
        Err(error) => return Ok(unreadable(&error)),
    };
    // Absence is optional only at initial capture. Disappearance while opening/reading a source
    // already observed present is a failed demand, like any other mid-demand mutation.
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Ok(unreadable(&error)),
        Err(error) => return Err(error.into()),
    };
    captured.check_metadata(file.metadata()?)?;
    let mut left = captured.len().min(max);
    let mut buf: Vec<u8> = Vec::with_capacity(usize::try_from(left.min(piece as u64))?.saturating_add(3));
    let result = (|| -> anyhow::Result<()> {
        while left > 0 {
            let carried = buf.len();
            let take = usize::try_from(left.min(piece as u64))?;
            buf.resize(carried + take, 0);
            (&file).read_exact(&mut buf[carried..])?;
            left -= take as u64;
            // A character cut by the piece's end waits for the next piece (the last is lossy).
            let cut = if left == 0 { buf.len() } else { complete_prefix(&buf) };
            each(&String::from_utf8_lossy(&buf[..cut]))?;
            buf.drain(..cut);
        }
        Ok(())
    })();
    captured.check_metadata(file.metadata()?)?;
    captured.check()?;
    result?;
    Ok(PersistedHead::Text(String::new()))
}

/// The length of `bytes` without a trailing UTF-8 sequence its lead byte says is incomplete.
fn complete_prefix(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let at = bytes.len() - back;
        let b = bytes[at];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let need = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if need > back { at } else { bytes.len() };
    }
    bytes.len()
}

/// Content at or below this many bytes is stored inline; larger content becomes a lazy [`Span`].
/// Small on purpose: most messages are well under it, so typical sessions are untouched and only
/// genuinely large fields (file dumps, pasted blobs, base64) go lazy.
pub const INLINE_MAX: usize = 4096;

/// A span of bytes in a source file, resolved on demand. `offset`/`len` cover the **raw** bytes; if
/// the source is a JSON string body, `escaped` marks that the slice needs JSON-unescaping on resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Source file. `None` ⇒ the owning [`Session`](crate::ir::Session)'s `source_path` (the common
    /// case); set only for adapters whose content lives in a sidecar (codex/grok update streams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    pub offset: u64,
    pub len: u64,
    /// The raw slice is a JSON string body needing unescaping (`true`) or literal text (`false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escaped: bool,
}

/// A piece of content text: materialized inline (small) or a lazy [`Span`] (large). See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    Inline(String),
    Span(Span),
    Source(SourceText),
}

impl Text {
    /// The inline string, or `None` if this is an unresolved span.
    pub fn inline_str(&self) -> Option<&str> {
        match self {
            Text::Inline(s) => Some(s),
            Text::Span(_) | Text::Source(_) => None,
        }
    }

    /// Whether reading this text requires a checked source demand.
    pub fn is_lazy(&self) -> bool {
        matches!(self, Text::Span(_) | Text::Source(_))
    }

    /// Inline equality without resolving: `None` means a source demand is required.
    pub fn matches_str(&self, other: &str) -> Option<bool> {
        self.inline_str().map(|text| text == other)
    }

    /// The span, when this is one (`Text::Source` is lazy with no span).
    pub fn as_span(&self) -> Option<&Span> {
        match self {
            Text::Span(sp) => Some(sp),
            Text::Inline(_) | Text::Source(_) => None,
        }
    }

    /// Resolve demanded text, reporting unavailable, changed or unreadable sources.
    pub fn try_resolve<'a>(&'a self, resolver: &'a Resolver) -> anyhow::Result<Cow<'a, str>> {
        match self {
            Text::Inline(s) => Ok(Cow::Borrowed(s)),
            Text::Span(sp) => resolver.try_resolve(sp),
            Text::Source(source) => Ok(Cow::Owned(source.try_resolve()?)),
        }
    }

    /// A decoded-byte prefix for checked lazy text; inline text is borrowed whole.
    pub fn try_resolve_prefix<'a>(&'a self, resolver: &'a Resolver, max: u64) -> anyhow::Result<Cow<'a, str>> {
        match self {
            Text::Inline(s) => Ok(Cow::Borrowed(s)),
            Text::Span(sp) => resolver.try_resolve_prefix(sp, max),
            Text::Source(source) => Ok(Cow::Owned(
                source.try_prefix(usize::try_from(max).unwrap_or(usize::MAX))?,
            )),
        }
    }

    /// Length in bytes without resolving (a span's raw length — an upper bound on the unescaped len).
    pub fn len(&self) -> usize {
        match self {
            Text::Inline(s) => s.len(),
            Text::Span(sp) => usize::try_from(sp.len).unwrap_or(usize::MAX),
            Text::Source(source) => source.raw_len(),
        }
    }

    pub fn try_for_each_chunk(
        &self,
        resolver: &Resolver,
        chunk: usize,
        mut f: impl FnMut(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        match self {
            Text::Inline(text) => {
                let mut part = text.as_str();
                while !part.is_empty() {
                    let mut end = part.len().min(chunk.max(1));
                    while !part.is_char_boundary(end) {
                        end += 1;
                    }
                    f(&part[..end])?;
                    part = &part[end..];
                }
                Ok(())
            }
            Text::Span(span) => resolver.try_for_each_chunk(span, chunk, f),
            Text::Source(source) => source.try_for_each_chunk(chunk, f),
        }
    }
    pub fn try_decoded_len(&self, resolver: &Resolver) -> anyhow::Result<usize> {
        match self {
            Text::Inline(text) => Ok(text.len()),
            Text::Source(source) => source.try_decoded_len(),
            Text::Span(span) => resolver.try_resolved_len(span),
        }
    }
    /// Validate source identity, bounds, and JSON string framing without chunk-decoding an
    /// unread block (such as `Thinking` when `!keep_tail_thinking`).
    pub(crate) fn validate_unread(&self, resolver: &Resolver) -> anyhow::Result<()> {
        match self {
            Text::Inline(_) => Ok(()),
            Text::Source(source) => source.validate_backing(),
            Text::Span(span) => resolver.source_text(span)?.validate_backing(),
        }
    }
    /// Validate and return the byte length used for token estimation of an unread block:
    /// skips escape-decoding `Text::Span` while preserving exact `SourceText` counts.
    pub(crate) fn est_unread_len(&self, resolver: &Resolver) -> anyhow::Result<usize> {
        match self {
            Text::Inline(text) => Ok(text.len()),
            Text::Source(source) => source.try_decoded_len(),
            Text::Span(span) => {
                resolver.source_text(span)?.validate_backing()?;
                Ok(usize::try_from(span.len).unwrap_or(usize::MAX))
            }
        }
    }
    pub fn with_skipped_prefix(mut self, bytes: usize) -> anyhow::Result<Self> {
        match &mut self {
            Text::Inline(text) => {
                *text = text
                    .get(bytes..)
                    .ok_or_else(|| anyhow::anyhow!("text prefix past end or invalid UTF8 boundary"))?
                    .to_owned();
            }
            Text::Source(source) => {
                let prefix = source.try_prefix(bytes.saturating_add(3))?;
                anyhow::ensure!(
                    prefix.get(bytes..).is_some(),
                    "source text prefix past end or invalid UTF8 boundary"
                );
                source.skip = source
                    .skip
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow::anyhow!("text prefix overflow"))?;
            }
            Text::Span(_) => anyhow::bail!("a decoded prefix needs source text, not a wire-form span"),
        }
        Ok(self)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Text {
    fn default() -> Self {
        Text::Inline(String::new())
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Text::Inline(s)
    }
}
impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Text::Inline(s.to_string())
    }
}

/// Read ergonomics: an inline `Text` derefs to its `str`. **Panics on an unresolved `Span`** — the
/// caller must `resolve`/`materialize` first. Inline content (the common case) is unaffected.
impl Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Text::Inline(s) => s,
            Text::Span(_) | Text::Source(_) => {
                panic!("Text::Span read as &str without resolving — call Session::materialize() or Text::try_resolve(&resolver) first")
            }
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Text::Inline(s) => f.write_str(s),
            // Formatting cannot resolve or report a source error. Refuse an unresolved value.
            Text::Span(_) | Text::Source(_) => Err(fmt::Error),
        }
    }
}

/// Serializes as a plain JSON string when inline (so a materialized `Session` round-trips
/// byte-identically), or as the span object when not. Whole-session JSON paths
/// [`materialize`](crate::ir::Session::materialize) first, so spans don't reach serialization there.
impl Serialize for Text {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Inline(t) => s.serialize_str(t),
            Text::Span(sp) => sp.serialize(s),
            Text::Source(source) => s.serialize_str(&source.try_resolve().map_err(serde::ser::Error::custom)?),
        }
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string (inline text) or a span object")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Text, E> {
                Ok(Text::Inline(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Text, E> {
                Ok(Text::Inline(v))
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Text, A::Error> {
                let sp = Span::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Text::Span(sp))
            }
        }
        d.deserialize_any(V)
    }
}

/// The identity a session's primary source had when its spans were parsed (or, for a resolver
/// built before the parse, when the resolver was built). Spans carry offsets, not identities; this
/// is what lets a resolver tell a file that only grew from one that was replaced.
#[derive(Clone, Debug)]
pub struct SourceStamp(crate::json::JsonSource);
impl SourceStamp {
    /// `None` when `path` cannot be stat'ed now; resolution then reports that failure itself.
    pub fn capture(path: &Path) -> Option<Self> {
        crate::json::JsonSource::capture_identity(path).ok().map(Self)
    }
    /// Whether this stamps `path` (spelled however the caller spells it).
    pub fn is_for(&self, path: &Path) -> bool {
        std::path::absolute(path).is_ok_and(|path| path == self.0.path())
    }
}

/// Resolves wire-form [`Span`]s: each demand reads the span's JSON string (its body plus the two
/// quotes around it) as checked [`SourceText`] in a capture of its source file.
///
/// The primary source is held to its [`SourceStamp`]: a demand fails unless the file is the
/// stamped one, unchanged or only appended to. That catches a file replaced under its name (a new
/// inode) or rewritten at the same length after the parse, on the first demand. An in-place
/// rewrite that also grows the file keeps the inode and passes, the same limit resume marks
/// document.
pub struct Resolver {
    primary: Option<PathBuf>,
    stamp: Option<SourceStamp>,
    /// One capture per source path, taken by the first demand; later demands check it instead of
    /// capturing again, and a capture replacing it must admit it (the file only grew).
    sources: std::cell::RefCell<std::collections::HashMap<PathBuf, crate::json::JsonSource>>,
}

impl Resolver {
    /// A resolver rooted at a session's primary source (its `source_path`), stamped now. Build it
    /// before the parse whose spans it resolves; after a parse, use [`crate::Session::resolver`],
    /// which carries the parse-time stamp.
    pub fn new(primary: Option<PathBuf>) -> Self {
        let stamp = primary.as_deref().and_then(SourceStamp::capture);
        Self::stamped(primary, stamp)
    }

    /// A resolver whose primary is held to `stamp` (none: the primary is not held to anything).
    pub fn stamped(primary: Option<PathBuf>, stamp: Option<SourceStamp>) -> Self {
        Resolver {
            primary,
            stamp,
            sources: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// The checked source text a span names: its string in the resolver's capture of the span's
    /// file when that still holds the span, else a fresh capture held to the parse-time stamp and
    /// to the capture it replaces.
    fn source_text(&self, sp: &Span) -> anyhow::Result<SourceText> {
        use anyhow::Context;
        let path = sp.source.as_deref().or(self.primary.as_deref());
        let path = std::path::absolute(path.context("Text span has no source path")?)?;
        // The span covers a JSON string's body; its value includes the quotes on either side.
        let offset = sp.offset.checked_sub(1).context("Text span has no opening quote")?;
        let len = sp.len.checked_add(2).context("Text span length overflows")?;
        let fits = |source: &crate::json::JsonSource| offset.checked_add(len).is_some_and(|end| end <= source.len());
        let held = self.sources.borrow().get(&path).cloned();
        let source = match held.as_ref().filter(|source| fits(source)) {
            Some(source) => {
                source.check()?;
                source.clone()
            }
            None => {
                let source = crate::json::JsonSource::capture(&path)?;
                if let Some(stamp) = self.stamp.as_ref().filter(|stamp| stamp.0.path() == source.path()) {
                    anyhow::ensure!(
                        stamp.0.admits(&source),
                        "Text source {} was replaced or rewritten after it was parsed",
                        path.display()
                    );
                }
                if let Some(first) = &held {
                    anyhow::ensure!(first.admits(&source), "JSON source changed: {}", path.display());
                }
                anyhow::ensure!(fits(&source), "Text span is outside source {}", path.display());
                self.sources.borrow_mut().insert(path, source.clone());
                source
            }
        };
        let crate::json::Json::Source(range) = source.validated_range(offset, len)? else {
            anyhow::bail!("Text span range is not source-backed");
        };
        Ok(SourceText::new(range))
    }

    pub fn try_resolve<'a>(&'a self, sp: &Span) -> anyhow::Result<Cow<'a, str>> {
        Ok(Cow::Owned(self.source_text(sp)?.try_resolve()?))
    }

    /// The decoded length, without retaining the text.
    pub fn try_resolved_len(&self, sp: &Span) -> anyhow::Result<usize> {
        self.source_text(sp)?.try_decoded_len()
    }

    /// At most `max` decoded bytes from the head, cut at a character boundary.
    pub fn try_resolve_prefix<'a>(&'a self, sp: &Span, max: u64) -> anyhow::Result<Cow<'a, str>> {
        let cap = usize::try_from(max).unwrap_or(usize::MAX);
        Ok(Cow::Owned(self.source_text(sp)?.try_prefix(cap)?))
    }

    /// Bounded callbacks over the decoded text (see [`SourceText::try_for_each_chunk`]). An error
    /// can follow earlier callbacks: discard staged output and never publish freshness on Err.
    pub fn try_for_each_chunk(
        &self,
        sp: &Span,
        chunk: usize,
        f: impl FnMut(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.source_text(sp)?.try_for_each_chunk(chunk, f)
    }
}

/// A serde_json `RawValue`, re-exported so adapters' span paths can name the type without depending
/// on the `raw_value` feature surface directly.
pub use serde_json::value::RawValue;

#[cfg(test)]
mod tests {
    #[test]
    fn unresolved_span_display_and_comparison_require_resolution() {
        use std::fmt::Write;
        let span = super::Text::Span(super::Span {
            source: None,
            offset: 1,
            len: 3,
            escaped: false,
        });
        assert_eq!(span.matches_str(""), None);
        assert_eq!(span.matches_str("abc"), None);
        assert!(write!(String::new(), "{span}").is_err());
        let inline = super::Text::from("abc");
        assert_eq!(inline.matches_str("abc"), Some(true));
        assert_eq!(inline.matches_str(""), Some(false));
        assert_eq!(inline.to_string(), "abc");
    }

    #[test]
    fn public_span_operations_reject_overflowing_offsets() {
        let text = super::Text::Span(super::Span {
            source: None,
            offset: u64::MAX,
            len: 1,
            escaped: false,
        });
        assert!(text.with_skipped_prefix(1).is_err());
    }

    use super::*;

    struct CheckedTestDir(PathBuf);
    impl CheckedTestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cv-checked-text-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for CheckedTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn checked_demands_reject_missing_truncated_and_overflowing_sources() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        let resolver = Resolver::new(Some(path.clone()));
        let mut span = Span {
            offset: 0,
            len: 5,
            escaped: false,
            source: None,
        };
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 1).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        std::fs::write(&path, b"abc").unwrap();
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 0).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        span.offset = u64::MAX;
        span.len = 1;
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(Resolver::new(None).try_resolve(&span).is_err());
        std::fs::write(&path, b"\"\"").unwrap();
        span.offset = 1;
        span.len = 0;
        assert_eq!(resolver.try_resolve(&span).unwrap(), "");
        assert_eq!(resolver.try_resolve_prefix(&span, 0).unwrap(), "");
        let mut calls = 0;
        resolver
            .try_for_each_chunk(&span, 1, |_| {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 0);
    }

    #[test]
    fn checked_callbacks_keep_owned_text_and_reject_mutation_before_more_reads() {
        for escaped in [false, true] {
            let dir = CheckedTestDir::new();
            let path = dir.path().join("text");
            // A span covers a JSON string's body; `X` stands in for the decoded newline unescaped.
            let (raw, decoded) = if escaped {
                ("body\\n".repeat(40_000), b'\n')
            } else {
                ("bodyX".repeat(40_000), b'X')
            };
            std::fs::write(&path, format!("\"{raw}\"")).unwrap();
            let resolver = Resolver::new(Some(path.clone()));
            let span = Span {
                offset: 1,
                len: raw.len() as u64,
                escaped,
                source: None,
            };
            let mut calls = 0;
            let error = resolver
                .try_for_each_chunk(&span, 4096, |text| {
                    calls += 1;
                    std::fs::write(&path, b"changed").unwrap();
                    // The current callback remains readable after its mapped file is truncated.
                    assert_eq!(text.as_bytes().iter().filter(|&&b| b == decoded).count(), 819);
                    anyhow::bail!("callback sentinel")
                })
                .unwrap_err();
            assert_eq!(calls, 1);
            assert!(!error.to_string().contains("callback sentinel"), "{error:#}");
        }
    }

    #[test]
    fn checked_optional_head_distinguishes_absence_from_read_failure() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        assert_eq!(read_head_checked(&path, 8).unwrap(), PersistedHead::Absent);
        assert!(matches!(
            read_head_checked(dir.path(), 8).unwrap(),
            PersistedHead::Unreadable(_)
        ));
        std::fs::write(&path, b"a\xffb").unwrap();
        for cap in 0..5 {
            assert_eq!(
                read_head_checked(&path, cap).unwrap(),
                PersistedHead::Text(String::from_utf8_lossy(&b"a\xffb"[..cap.min(3)]).into_owned())
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway; everyone else gets a noted, unreadable output.
            if std::fs::File::open(&path).is_err() {
                let head = read_head_checked(&path, 8).unwrap();
                assert!(
                    matches!(&head, PersistedHead::Unreadable(reason) if reason.contains("ermission")),
                    "{head:?}"
                );
                assert!(head
                    .indexed_text("/p")
                    .unwrap()
                    .starts_with("[persisted output /p unreadable: "));
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn checked_demands_match_independent_large_string_oracle_and_source_overrides() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        let content = "é🌙 quote\" slash\\ newline\n\u{2003}".repeat(4000);
        let quoted = serde_json::to_string(&content).unwrap();
        let raw = &quoted.as_bytes()[1..quoted.len() - 1];
        let mut file = b"prefix\"".to_vec();
        file.extend_from_slice(raw);
        file.extend_from_slice(b"\"suffix");
        std::fs::write(&path, &file).unwrap();
        let span = Span {
            source: Some(path.clone()),
            offset: 7,
            len: raw.len() as u64,
            escaped: true,
        };
        let resolver = Resolver::new(Some(dir.path().join("wrong-primary")));
        assert_eq!(resolver.try_resolve(&span).unwrap(), content);
        for chunk in [1, 2, 13, 65536] {
            let mut output = String::new();
            resolver
                .try_for_each_chunk(&span, chunk, |part| {
                    assert!(part.len() <= chunk + 3);
                    output.push_str(part);
                    Ok(())
                })
                .unwrap();
            assert_eq!(output, content);
        }
        // A malformed escape is an error, not a lossy fallback: the body is no JSON string.
        let late_path = dir.path().join("late");
        let mut malformed = b"\"".to_vec();
        malformed.extend_from_slice(raw);
        malformed.extend_from_slice(b"\\ud83c\"");
        std::fs::write(&late_path, &malformed).unwrap();
        let span = Span {
            source: Some(late_path),
            offset: 1,
            len: malformed.len() as u64 - 2,
            escaped: true,
        };
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_for_each_chunk(&span, 8192, |_| Ok(())).is_err());
        // Literal Unicode whitespace cut mid-scalar must remain whitespace, including portable.
        let whitespace_path = dir.path().join("whitespace");
        let whitespace = format!("{}wanted", "\u{2003}".repeat(10_000));
        std::fs::write(&whitespace_path, format!("\"{whitespace}\"")).unwrap();
        let span = Span {
            source: Some(whitespace_path),
            offset: 1,
            len: whitespace.len() as u64,
            escaped: false,
        };
        let prefix = resolver.try_resolve_prefix(&span, 1024).unwrap();
        assert_eq!(prefix.len(), 1023);
        assert!(prefix.chars().all(char::is_whitespace));
    }

    #[test]
    fn a_file_replaced_after_the_parse_fails_the_first_demand_and_growth_does_not() {
        use crate::ir::{Block, Harness, SessionRef};
        use std::io::Write;
        let world = cv_testkit::World::new("lazy-parse-stamp");
        let path = world.proj.join("stamped.jsonl");
        let line = |fill: &str| {
            serde_json::json!({"type": "user", "uuid": fill, "sessionId": "stamped",
                "message": {"role": "user", "content": fill.repeat(10_000)}})
            .to_string()
                + "\n"
        };
        let parse = || {
            let adapter = crate::harness::claude::Claude::with_config_dir(&world.proj);
            let reference = SessionRef::new("stamped", Harness::Claude, path.clone());
            crate::stream::collect_with(&adapter, &reference, &crate::stream::Demand::LEAN.with_spans()).unwrap()
        };
        let first_text = |session: &crate::Session| match &session.messages[0].content[0] {
            Block::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        std::fs::write(&path, line("A")).unwrap();
        let session = parse();
        assert!(first_text(&session).is_lazy());
        let replacement = world.base.join("replacement");
        std::fs::write(&replacement, line("B")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let error = session.clone().materialize().unwrap_err();
        assert!(error.to_string().contains("JSON source changed"), "{error:#}");
        let resolver = session.resolver();
        assert!(first_text(&session).try_resolve(&resolver).is_err());
        // An unguarded streaming sink's resolver is built before the parse: same answer.
        let early = Resolver::new(Some(path.clone()));
        let session = parse();
        std::fs::write(&replacement, line("C")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(first_text(&session).try_resolve(&early).is_err());
        // A live transcript only grows: appending after the parse still resolves.
        std::fs::write(&path, line("A")).unwrap();
        let mut session = parse();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(line("D").as_bytes())
            .unwrap();
        session.materialize().unwrap();
        assert_eq!(&*first_text(&session), "A".repeat(10_000));
    }

    #[test]
    fn checked_mapping_rejects_replaced_path_after_first_demand() {
        let dir = CheckedTestDir::new();
        let path = dir.path().join("text");
        std::fs::write(&path, b"\"before\"").unwrap();
        let resolver = Resolver::new(Some(path.clone()));
        let span = Span {
            source: None,
            offset: 1,
            len: 6,
            escaped: false,
        };
        assert_eq!(resolver.try_resolve(&span).unwrap(), "before");
        // Later demands on the path reuse the first demand's capture.
        let held = resolver.sources.borrow().values().next().cloned().unwrap();
        assert_eq!(resolver.try_resolved_len(&span).unwrap(), 6);
        assert_eq!(resolver.sources.borrow().len(), 1);
        assert_eq!(&held, resolver.sources.borrow().values().next().unwrap());
        drop(held);
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"\"after!\"").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(resolver.try_resolve(&span).is_err());
        assert!(resolver.try_resolved_len(&span).is_err());
        assert!(resolver.try_resolve_prefix(&span, 2).is_err());
        assert!(resolver.try_for_each_chunk(&span, 2, |_| Ok(())).is_err());
        assert_eq!(Resolver::new(Some(path)).try_resolve(&span).unwrap(), "after!");
    }

    #[test]
    fn inline_roundtrips_as_string() {
        let t = Text::from("hello");
        assert_eq!(&*t, "hello");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"hello\"");
        let back: Text = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(back, Text::Inline("hello".into()));
    }
}
