//! Lazy content: the IR holds large text as a **span** into its source file, not as owned bytes.
//!
//! A fully-parsed [`Session`](crate::ir::Session) used to *own* every byte of content
//! (`Block::Text { text: String }`, …) — 700 MB resident just to hold the IR of a transcript with
//! one 700 MB record, even if the consumer only wants its title. There's no need: the bytes live on
//! disk in a read-only transcript. So a content field is a [`Text`] that's either materialized
//! [`Inline`](Text::Inline) (small — most messages) or a [`Span`] (large), resolved **on demand**
//! against the source via a [`Resolver`] (which memory-maps the file, so even resolving touches only
//! the needed pages).
//!
//! Invariant: reading a `Span` as `&str` (via [`Deref`]) panics — you must [`resolve`](Text::resolve)
//! it against a [`Resolver`], or [`materialize`](crate::ir::Session::materialize) the session first.
//! Streaming consumers resolve per-block (peak = one field); whole-session consumers materialize
//! (peak = whole session, their choice). Below the inline threshold everything is `Inline`, where
//! `Deref`/`Display` just work.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Read at most `max_bytes` from the head of a sidecar text file (a persisted tool output, say) as
/// lossy UTF-8. `None` if the file can't be opened. Indexers use this to make text the model never
/// saw inline (only via a stub) searchable, without ever loading a whole multi-MB dump.
pub fn read_head(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(max_bytes.min(1 << 20));
    f.by_ref().take(max_bytes as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
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
}

impl Text {
    /// The inline string, or `None` if this is an unresolved span.
    pub fn inline_str(&self) -> Option<&str> {
        match self {
            Text::Inline(s) => Some(s),
            Text::Span(_) => None,
        }
    }

    pub fn is_span(&self) -> bool {
        matches!(self, Text::Span(_))
    }

    /// The span, if this is lazy content.
    pub fn as_span(&self) -> Option<&Span> {
        match self {
            Text::Span(sp) => Some(sp),
            Text::Inline(_) => None,
        }
    }

    /// Resolve to the text, borrowing the inline string or (for a span) reading it from `resolver`.
    /// Cheap for inline and for unescaped spans (borrows the mmap); allocates only to unescape.
    pub fn resolve<'a>(&'a self, resolver: &'a Resolver) -> Cow<'a, str> {
        match self {
            Text::Inline(s) => Cow::Borrowed(s),
            Text::Span(sp) => resolver.resolve(sp),
        }
    }

    /// Resolve at most the first `max_raw` raw bytes — the head of the content. Cheap and bounded:
    /// a giant paste never materializes past the cap. Inline text borrows in full (already in
    /// memory); a span reads only its prefix. Used to synthesize a short title without paying for a
    /// multi-megabyte first message.
    pub fn resolve_prefix<'a>(&'a self, resolver: &'a Resolver, max_raw: u64) -> Cow<'a, str> {
        match self {
            Text::Inline(s) => Cow::Borrowed(s),
            Text::Span(sp) => resolver.resolve_prefix(sp, max_raw),
        }
    }

    /// Length in bytes without resolving (a span's raw length — an upper bound on the unescaped len).
    pub fn len(&self) -> usize {
        match self {
            Text::Inline(s) => s.len(),
            Text::Span(sp) => sp.len as usize,
        }
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
            Text::Span(_) => {
                panic!("Text::Span read as &str without resolving — call Session::materialize() or Text::resolve(&resolver) first")
            }
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Text::Inline(s) => f.write_str(s),
            // Don't panic in Display; an unmaterialized span renders empty. Materialize for output.
            Text::Span(_) => Ok(()),
        }
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        self.inline_str() == Some(other)
    }
}
impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        self.inline_str() == Some(*other)
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

/// Resolves [`Span`]s against their source file(s). Memory-maps each source on first use (with the
/// `mmap` feature) so resolving slices pages on demand rather than reading the whole file; without
/// `mmap` (e.g. wasm32) it falls back to a positioned ranged read.
pub struct Resolver {
    primary: Option<PathBuf>,
    #[cfg(feature = "mmap")]
    maps: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<memmap2::Mmap>>>,
}

impl Resolver {
    /// A resolver rooted at a session's primary source (its `source_path`).
    pub fn new(primary: Option<PathBuf>) -> Self {
        Resolver {
            primary,
            #[cfg(feature = "mmap")]
            maps: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    fn path_for<'a>(&'a self, sp: &'a Span) -> Option<&'a Path> {
        sp.source.as_deref().or(self.primary.as_deref())
    }

    /// Resolve a span to its text. Borrows the mapped bytes for unescaped spans; allocates only to
    /// UTF-8-decode or JSON-unescape. (The result borrows only from `self`, so a caller-built
    /// temporary `Span` — e.g. a capped copy — works fine.)
    pub fn resolve<'a>(&'a self, sp: &Span) -> Cow<'a, str> {
        match self.raw_bytes(sp) {
            None => Cow::Borrowed(""),
            Some(bytes) => {
                if sp.escaped {
                    Cow::Owned(unescape_json_body(&bytes))
                } else {
                    match bytes {
                        Cow::Borrowed(b) => String::from_utf8_lossy(b),
                        Cow::Owned(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
                    }
                }
            }
        }
    }

    /// Resolve at most the first `max_raw` **raw** bytes of `sp` — for consumers that only need the
    /// head of a giant field (listing labels, snippet scans, embedding heads) and must never
    /// materialize the whole thing. For an escaped span the cut is backed up to the last *complete*
    /// JSON escape sequence so truncation can't corrupt the unescape; an unescaped cut may land
    /// mid-UTF-8-char, which `from_utf8_lossy` renders as a trailing replacement char.
    pub fn resolve_prefix<'a>(&'a self, sp: &Span, max_raw: u64) -> Cow<'a, str> {
        if sp.len <= max_raw {
            return self.resolve(sp);
        }
        let capped = Span {
            len: max_raw,
            ..sp.clone()
        };
        match self.raw_bytes(&capped) {
            None => Cow::Borrowed(""),
            Some(bytes) if sp.escaped => {
                let keep = complete_escape_prefix(&bytes);
                Cow::Owned(unescape_json_body(&bytes[..keep]))
            }
            Some(Cow::Borrowed(b)) => String::from_utf8_lossy(b),
            Some(Cow::Owned(b)) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
        }
    }

    /// Feed the span's text to `f` in `chunk`-byte pieces **without materializing the whole field** —
    /// for unescaped spans the pieces are borrowed straight from the mmap (only touched pages page in,
    /// and they're reclaimable). Chunk boundaries are kept on char boundaries; if `escaped`, the whole
    /// field is unescaped once (escapes can't be unescaped piecewise) then chunked. Lets a consumer
    /// (e.g. the chunked indexer) stream a 700 MB field through a 4 MB window.
    pub fn for_each_chunk(&self, sp: &Span, chunk: usize, mut f: impl FnMut(&str)) {
        let chunk = chunk.max(1);
        match self.raw_bytes(sp) {
            None => {}
            Some(bytes) if sp.escaped => {
                // Escaped: must unescape as a whole (rare for truly giant fields — those are usually
                // base64/file dumps without many escapes; unescape only allocates when there ARE
                // backslashes). Then hand out in char-boundary pieces.
                let s = unescape_json_body(&bytes);
                emit_chunks(&s, chunk, &mut f);
            }
            Some(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                emit_chunks(&s, chunk, &mut f);
            }
        }
    }

    #[cfg(feature = "mmap")]
    fn raw_bytes<'a>(&'a self, sp: &Span) -> Option<Cow<'a, [u8]>> {
        let path = self.path_for(sp)?.to_path_buf();
        {
            let mut maps = self.maps.borrow_mut();
            if !maps.contains_key(&path) {
                let m = std::fs::File::open(&path)
                    .ok()
                    .and_then(|f| unsafe { memmap2::Mmap::map(&f).ok() });
                maps.insert(path.clone(), m);
            }
        }
        let maps = self.maps.borrow();
        let mmap = maps.get(&path)?.as_ref()?;
        let start = sp.offset as usize;
        let end = start.checked_add(sp.len as usize)?;
        if end > mmap.len() {
            return None;
        }
        let slice: &[u8] = &mmap[start..end];
        // SAFETY: the backing Mmap is owned by `self.maps` for `self`'s lifetime `'a` and entries are
        // never removed, so the slice stays valid for `'a`.
        let slice: &'a [u8] = unsafe { std::mem::transmute::<&[u8], &'a [u8]>(slice) };
        Some(Cow::Borrowed(slice))
    }

    #[cfg(not(feature = "mmap"))]
    fn raw_bytes<'a>(&'a self, sp: &Span) -> Option<Cow<'a, [u8]>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path_for(sp)?;
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(sp.offset)).ok()?;
        let mut buf = vec![0u8; sp.len as usize];
        f.read_exact(&mut buf).ok()?;
        Some(Cow::Owned(buf))
    }
}

/// Build a [`Span`] for a raw JSON string value `raw` (e.g. from `serde_json::value::RawValue`) that
/// borrows somewhere inside the record bytes `within` starting at absolute file offset `base_off`.
/// The span covers the string *body* (between the quotes); `escaped` is set if it contains a
/// backslash. `None` if `raw` isn't a JSON string or doesn't lie within `within`. Used by adapters'
/// span-producing parse paths (claude, codex).
pub fn json_string_span(raw: &RawValue, within: &[u8], base_off: u64) -> Option<Span> {
    let b = raw.get().as_bytes();
    if b.len() < 2 || b.first() != Some(&b'"') || b.last() != Some(&b'"') {
        return None;
    }
    let off = (b.as_ptr() as usize).checked_sub(within.as_ptr() as usize)?;
    if off + b.len() > within.len() {
        return None;
    }
    let body = &b[1..b.len() - 1];
    Some(Span {
        source: None,
        offset: base_off + off as u64 + 1,
        len: body.len() as u64,
        escaped: body.contains(&b'\\'),
    })
}

/// A serde_json `RawValue`, re-exported so adapters' span paths can name the type without depending
/// on the `raw_value` feature surface directly.
pub use serde_json::value::RawValue;

/// Emit `s` in `chunk`-byte pieces split on char boundaries.
fn emit_chunks(s: &str, chunk: usize, f: &mut impl FnMut(&str)) {
    let mut start = 0;
    let bytes = s.as_bytes();
    while start < s.len() {
        let mut end = (start + chunk).min(s.len());
        while end < s.len() && (bytes[end] & 0xC0) == 0x80 {
            end += 1; // don't split a UTF-8 continuation byte
        }
        f(&s[start..end]);
        start = end;
    }
}

/// The longest prefix of `raw` (a JSON string body) that ends on a *complete* escape sequence —
/// i.e. doesn't cut a `\x` or `\uXXXX` in half. Truncating to this length keeps the prefix
/// parseable by [`unescape_json_body`] instead of tripping its lossy raw fallback.
fn complete_escape_prefix(raw: &[u8]) -> usize {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] != b'\\' {
            i += 1;
            continue;
        }
        // An escape starts here: 2 bytes (`\n`, `\"`, …), 6 (`\uXXXX`), or 12 (a `\uD8xx\uDCxx`
        // surrogate pair — a cut between the halves would leave an unparseable lone surrogate).
        if raw.get(i + 1) == Some(&b'u') {
            if i + 6 > raw.len() {
                return i; // incomplete at the cut — keep everything before it
            }
            let high_surrogate = std::str::from_utf8(&raw[i + 2..i + 6])
                .ok()
                .and_then(|h| u16::from_str_radix(h, 16).ok())
                .is_some_and(|h| (0xD800..0xDC00).contains(&h));
            let needed = if high_surrogate { 12 } else { 6 };
            if i + needed > raw.len() {
                return i;
            }
            i += needed;
        } else {
            if i + 2 > raw.len() {
                return i;
            }
            i += 2;
        }
    }
    raw.len()
}

/// Unescape a JSON string *body* (the bytes between the quotes) into its text, by re-quoting and
/// letting `serde_json` parse it — correct for all JSON escapes. Falls back to a lossy decode.
fn unescape_json_body(raw: &[u8]) -> String {
    if !raw.contains(&b'\\') {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut quoted = Vec::with_capacity(raw.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(raw);
    quoted.push(b'"');
    serde_json::from_slice::<String>(&quoted).unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrips_as_string() {
        let t = Text::from("hello");
        assert_eq!(&*t, "hello");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"hello\"");
        let back: Text = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(back, Text::Inline("hello".into()));
    }

    #[test]
    fn unescape_handles_escapes() {
        assert_eq!(unescape_json_body(b"a\\nb"), "a\nb");
        assert_eq!(unescape_json_body(b"plain"), "plain");
        assert_eq!(unescape_json_body(b"quote \\\" mark"), "quote \" mark");
    }

    #[test]
    fn complete_escape_prefix_never_splits_an_escape() {
        // Plain text: the whole slice survives.
        assert_eq!(complete_escape_prefix(b"hello"), 5);
        // Complete escapes survive whole.
        assert_eq!(complete_escape_prefix(b"a\\nb"), 4);
        assert_eq!(complete_escape_prefix(b"a\\u0041b"), 8);
        // A trailing half escape is cut *before* the backslash.
        assert_eq!(complete_escape_prefix(b"abc\\"), 3);
        assert_eq!(complete_escape_prefix(b"abc\\u00"), 3);
        // A high surrogate keeps its partner or is dropped entirely.
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD1E"), 13);
        assert_eq!(complete_escape_prefix(b"x\\uD834"), 1);
        assert_eq!(complete_escape_prefix(b"x\\uD834\\uDD"), 1);
        // …and every kept prefix actually unescapes cleanly.
        for raw in [&b"abc\\"[..], b"abc\\u00", b"x\\uD834", b"a\\nb\\"] {
            let keep = complete_escape_prefix(raw);
            let requoted = [b"\"", &raw[..keep], b"\""].concat();
            assert!(
                serde_json::from_slice::<String>(&requoted).is_ok(),
                "prefix of {raw:?} must stay parseable"
            );
        }
    }

    /// `resolve_prefix` reads only the head of a span — including backing an escaped cut up to a
    /// complete escape so the head unescapes exactly like the full resolve's head would.
    #[test]
    fn resolve_prefix_caps_and_respects_escapes() {
        let dir = std::env::temp_dir().join(format!("cv-lazy-prefix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.txt");
        // Raw JSON string body: "line1\nline2…" with the escape right at the cut boundary.
        std::fs::write(&path, b"line one\\nline two and much more after").unwrap();
        let resolver = Resolver::new(Some(path.clone()));

        let span = |len: u64, escaped: bool| Span {
            source: None,
            offset: 0,
            len,
            escaped,
        };
        let full_len = std::fs::metadata(&path).unwrap().len();

        // Unescaped: plain byte cap.
        assert_eq!(resolver.resolve_prefix(&span(full_len, false), 8), "line one");
        // Cap ≥ len: identical to a full resolve.
        assert_eq!(
            resolver.resolve_prefix(&span(full_len, true), full_len + 10),
            resolver.resolve(&span(full_len, true))
        );
        // Escaped, cut mid-`\n` (raw bytes 9 ends between '\' and 'n'): backs up to byte 8.
        assert_eq!(resolver.resolve_prefix(&span(full_len, true), 9), "line one");
        // Escaped, cut after the complete escape: the newline survives.
        assert_eq!(resolver.resolve_prefix(&span(full_len, true), 14), "line one\nline");

        std::fs::remove_dir_all(&dir).ok();
    }
}
