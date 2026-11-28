//! The one frame writer: selected messages are staged as RS-framed compact JSON, then injected
//! into an envelope's empty root array (`messages`) as serde serializes it, compact or pretty,
//! so the envelope keeps its derived schema and no message is held as a decoded value.
use crate::bytes::ByteSource;
use serde::{Deserialize, Serialize};
use serde_json::ser::{CharEscape, CompactFormatter, Formatter, PrettyFormatter};
use std::cell::Cell;
use std::fs;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::rc::Rc;

/// Maximum decoded byte length of a root key considered for frame injection.
const KEY_BYTES: usize = 128;

/// RS cannot occur unescaped in JSON; each compact message is one frame. The complete frame is
/// counted while only its first `remaining + 1` bytes are retained (`None` keeps it whole):
/// discarding still reports successful writes, so every lazy demand and source check runs.
pub fn write_frame(
    stage: &mut impl Write,
    message: &crate::Message,
    resolver: &crate::Resolver,
    remaining: Option<usize>,
) -> anyhow::Result<usize> {
    write_frame_with(stage, remaining, |mut w| message.write_json(&mut w, resolver))
}

pub(crate) fn write_frame_with(
    stage: &mut impl Write,
    remaining: Option<usize>,
    write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<usize> {
    struct Capped<'a, W> {
        stage: &'a mut W,
        count: usize,
        cap: Option<usize>,
    }
    impl<W: Write> Write for Capped<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = self
                .cap
                .map_or(bytes.len(), |cap| bytes.len().min(cap.saturating_sub(self.count)));
            if n != 0 {
                self.stage.write_all(&bytes[..n])?;
            }
            self.count = self
                .count
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("JSON size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stage.flush()
        }
    }
    let mut writer = Capped {
        stage,
        count: 0,
        cap: remaining.map(|n| n.saturating_add(1)),
    };
    writer.write_all(&[0x1e])?;
    write(&mut writer)?;
    Ok(writer.count - 1)
}

/// A writer that publishes its position, for callers whose serializer owns the writer.
pub struct Counted<W> {
    pub inner: W,
    pub position: Rc<Cell<u64>>,
    digest: Option<crate::digest::Sha256>,
}
impl<W> Counted<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            position: Rc::new(Cell::new(0)),
            digest: None,
        }
    }
    /// Capture a content digest during serialization, so publication never re-reads its stage.
    pub fn hashing(mut self) -> Self {
        self.digest = Some(crate::digest::Sha256::new());
        self
    }
    pub fn into_digest(self) -> Option<String> {
        self.digest.map(crate::digest::Sha256::finish_hex)
    }
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        if let Some(digest) = &mut self.digest {
            digest.update(&bytes[..n]);
        }
        self.position.set(
            self.position
                .get()
                .checked_add(n as u64)
                .ok_or_else(|| io::Error::other("JSON output length overflow"))?,
        );
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The output range `[start, end)` holding the first `messages` frames of a pretty envelope: an
/// archive copies it from the previous artifact instead of re-serializing a stable prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefix {
    pub start: u64,
    pub end: u64,
    pub messages: usize,
}
impl Prefix {
    pub fn valid(&self, bytes: u64, messages: usize) -> bool {
        self.messages == messages && messages > 0 && self.start < self.end && self.end <= bytes
    }
}

/// How an envelope is written around its frames.
#[derive(Default)]
pub struct Framing<'a> {
    /// serde_json's pretty format (the CLI and the archive) instead of compact (the HTTP window).
    pub pretty: bool,
    /// Copy this range of an earlier artifact as the first frames, before the staged ones.
    pub old: Option<(&'a Path, &'a Prefix)>,
    /// Record where the first `stable` frames land, for the next write's `old`.
    pub stable: Option<usize>,
    /// The frame count the stage must hold.
    pub total: Option<usize>,
}

/// Serialize `envelope`, replacing its empty root `field` array with the staged frames. The frames
/// come from checked serializers; stage before publishing.
pub fn write_json_array(
    out: &mut impl Write,
    envelope: &impl Serialize,
    field: &str,
    frames: &ByteSource,
) -> anyhow::Result<()> {
    write_frames(out, envelope, field, frames, Framing::default()).map(|_| ())
}

/// Write the pretty session envelope used by custody and JSON export around staged messages.
pub fn write_session_document(
    writer: &mut impl Write,
    session: &crate::Session,
    spool: &ByteSource,
    total: usize,
    document: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    let framing = Framing {
        pretty: true,
        total: Some(total),
        ..Framing::default()
    };
    if document {
        write_frames(
            writer,
            &crate::harness::opensession::document(session),
            "messages",
            spool,
            framing,
        )?;
    } else {
        write_frames(writer, session, "messages", spool, framing)?;
    }
    Ok(())
}

/// [`write_json_array`] with the pretty and archive options; returns the recorded [`Prefix`].
/// Stage output before publishing: a missing field or mismatched total can fail after envelope
/// bytes have already been written to out.
pub fn write_frames(
    out: &mut impl Write,
    envelope: &impl Serialize,
    field: &str,
    frames: &ByteSource,
    framing: Framing<'_>,
) -> anyhow::Result<Option<Prefix>> {
    anyhow::ensure!(field.len() <= KEY_BYTES, "JSON frame field exceeds key budget");
    let mut counted = Counted::new(out);
    let position = counted.position.clone();
    let mut array = Array {
        field,
        frames: Some(frames.reader()?),
        framing,
        position,
        depth: 0,
        key: String::new(),
        reading_key: false,
        selected: false,
        injected: false,
        count: 0,
        start: None,
        prefix: None,
    };
    if array.framing.pretty {
        let mut formatter = Shell {
            inner: PrettyFormatter::new(),
            array: &mut array,
        };
        envelope.serialize(&mut serde_json::Serializer::with_formatter(
            &mut counted,
            &mut formatter,
        ))?;
    } else {
        let mut formatter = Shell {
            inner: CompactFormatter,
            array: &mut array,
        };
        envelope.serialize(&mut serde_json::Serializer::with_formatter(
            &mut counted,
            &mut formatter,
        ))?;
    }
    anyhow::ensure!(array.injected, "JSON frame field is absent or not a root array");
    Ok(array.prefix)
}

struct Array<'a> {
    field: &'a str,
    frames: Option<Box<dyn Read + 'a>>,
    framing: Framing<'a>,
    position: Rc<Cell<u64>>,
    depth: usize,
    key: String,
    reading_key: bool,
    selected: bool,
    injected: bool,
    count: usize,
    start: Option<u64>,
    prefix: Option<Prefix>,
}

impl Array<'_> {
    /// Absorb one decoded key fragment while reading a root key, up to the key budget.
    fn absorb_key(&mut self, part: &str) {
        if self.reading_key {
            if self.key.len().saturating_add(part.len()) <= KEY_BYTES {
                self.key.push_str(part);
            } else {
                self.reading_key = false;
            }
        }
    }
    fn record_end(&mut self) {
        if self.framing.stable == Some(self.count) {
            self.prefix = self.start.map(|start| Prefix {
                start,
                end: self.position.get(),
                messages: self.count,
            });
        }
    }
    fn inject<F: Formatter, W: ?Sized + Write>(&mut self, inner: &mut F, w: &mut W) -> io::Result<()> {
        if let Some((path, prefix)) = self.framing.old {
            inner.begin_array_value(w, true)?;
            self.start = Some(self.position.get());
            let mut old = fs::File::open(path)?;
            old.seek(SeekFrom::Start(prefix.start))?;
            let expected = prefix.end - prefix.start;
            if io::copy(&mut old.take(expected), w)? != expected {
                return Err(io::Error::other("archive stable prefix truncated"));
            }
            self.count = prefix.messages;
            self.record_end();
            inner.end_array_value(w)?;
        }
        let frames = self
            .frames
            .take()
            .ok_or_else(|| io::Error::other("JSON frames already consumed"))?;
        let mut frames = io::BufReader::with_capacity(64 * 1024, frames);
        while !frames.fill_buf()?.is_empty() {
            if frames.fill_buf()?[0] != 0x1e {
                return Err(io::Error::other("JSON spool has no frame boundary"));
            }
            frames.consume(1);
            inner.begin_array_value(w, self.count == 0)?;
            self.start.get_or_insert(self.position.get());
            if self.framing.pretty {
                // Only whitespace changes: the compact frame was accepted and serialized by Message.
                let mut output = &mut *w;
                copy_frame(&mut frames, &mut crate::PrettyJson::new(&mut output, 2))?;
            } else {
                copy_frame(&mut frames, w)?;
            }
            self.count += 1;
            self.record_end();
            inner.end_array_value(w)?;
        }
        if self.framing.total.is_some_and(|total| total != self.count) {
            return Err(io::Error::other("JSON spool message count mismatch"));
        }
        if self
            .framing
            .stable
            .is_some_and(|stable| stable > 0 && self.prefix.is_none())
        {
            return Err(io::Error::other("archive checkpoint has no serialized prefix"));
        }
        Ok(())
    }
}

/// Copy one frame's bytes, up to the next RS or the end of the stage.
fn copy_frame(frames: &mut impl BufRead, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
    loop {
        let bytes = frames.fill_buf()?;
        let end = bytes.iter().position(|byte| *byte == 0x1e).unwrap_or(bytes.len());
        if end == 0 {
            return Ok(());
        }
        out.write_all(&bytes[..end])?;
        frames.consume(end);
    }
}

/// The envelope's own formatter, with the root `field` array hooked.
struct Shell<'a, 'f, F> {
    inner: F,
    array: &'a mut Array<'f>,
}

impl<F: Formatter> Formatter for &mut Shell<'_, '_, F> {
    fn begin_object<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.array.depth += 1;
        self.inner.begin_object(out)
    }
    fn end_object<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.array.depth -= 1;
        self.inner.end_object(out)
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, out: &mut W, first: bool) -> io::Result<()> {
        self.array.reading_key = self.array.depth == 1;
        self.array.key.clear();
        self.inner.begin_object_key(out, first)
    }
    fn end_object_key<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.array.selected = self.array.reading_key && self.array.key == self.array.field;
        self.array.reading_key = false;
        self.inner.end_object_key(out)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.inner.begin_object_value(out)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.inner.end_object_value(out)
    }
    fn write_string_fragment<W: ?Sized + Write>(&mut self, out: &mut W, part: &str) -> io::Result<()> {
        self.array.absorb_key(part);
        self.inner.write_string_fragment(out, part)
    }
    fn write_char_escape<W: ?Sized + Write>(&mut self, out: &mut W, escape: CharEscape) -> io::Result<()> {
        if self.array.reading_key {
            // Match decoded key spelling, including escaped quotes; dropping an escape
            // could alias a different key to the requested array field.
            let character = match &escape {
                CharEscape::Quote => '"',
                CharEscape::ReverseSolidus => '\\',
                CharEscape::Solidus => '/',
                CharEscape::Backspace => '\u{08}',
                CharEscape::FormFeed => '\u{0c}',
                CharEscape::LineFeed => '\n',
                CharEscape::CarriageReturn => '\r',
                CharEscape::Tab => '\t',
                CharEscape::AsciiControl(byte) => char::from(*byte),
            };
            let mut encoded = [0u8; 4];
            self.array.absorb_key(character.encode_utf8(&mut encoded));
        }
        self.inner.write_char_escape(out, escape)
    }
    fn begin_array<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        let inject = self.array.depth == 1 && self.array.selected;
        self.array.depth += 1;
        self.inner.begin_array(out)?;
        if inject {
            if self.array.injected {
                return Err(io::Error::other("duplicate staged JSON array"));
            }
            self.array.injected = true;
            self.array.inject(&mut self.inner, out)?;
        }
        Ok(())
    }
    fn begin_array_value<W: ?Sized + Write>(&mut self, out: &mut W, first: bool) -> io::Result<()> {
        if self.array.depth == 2 && self.array.selected && self.array.injected {
            return Err(io::Error::other("staged envelope array must be empty"));
        }
        self.inner.begin_array_value(out, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.inner.end_array_value(out)
    }
    fn end_array<W: ?Sized + Write>(&mut self, out: &mut W) -> io::Result<()> {
        if self.array.depth == 2 {
            self.array.selected = false;
        }
        self.array.depth -= 1;
        self.inner.end_array(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn framed(values: &[serde_json::Value]) -> ByteSource {
        let mut bytes = Vec::new();
        for value in values {
            bytes.push(0x1e);
            bytes.extend(serde_json::to_vec(value).unwrap());
        }
        ByteSource::read(bytes.as_slice()).unwrap()
    }

    #[test]
    fn escaped_root_keys_match_decoded_names_without_aliasing_other_fields() {
        let frames = framed(&[json!(1)]);
        for key in ["mes\"sages", "mes\\sages", "mes\nsages", "mes\u{01}sages"] {
            let mut envelope = serde_json::Map::new();
            envelope.insert(key.into(), json!([]));
            envelope.insert("messages".into(), json!([]));
            let mut actual = Vec::new();
            write_json_array(&mut actual, &envelope, "messages", &frames).unwrap();
            let mut expected = envelope.clone();
            expected.insert("messages".into(), json!([1]));
            assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
            actual.clear();
            write_json_array(&mut actual, &envelope, key, &frames).unwrap();
            let mut expected = envelope.clone();
            expected.insert(key.into(), json!([1]));
            assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        }
    }

    #[test]
    fn injected_root_array_preserves_schema_and_nested_arrays_compact_and_pretty() {
        let envelope = json!({"head":{"messages":[1]},"messages":[],"tail":[3]});
        let frames = framed(&[json!({"text":"月🌙é\n\"", "n":[1,{"a":[]}]}), json!(2)]);
        let mut expected = envelope.clone();
        expected["messages"] = json!([{"text":"月🌙é\n\"", "n":[1,{"a":[]}]},2]);
        let mut actual = Vec::new();
        write_json_array(&mut actual, &envelope, "messages", &frames).unwrap();
        assert_eq!(actual, serde_json::to_vec(&expected).unwrap());
        let mut actual = Vec::new();
        let framing = Framing {
            pretty: true,
            total: Some(2),
            ..Framing::default()
        };
        write_frames(&mut actual, &envelope, "messages", &frames, framing).unwrap();
        assert_eq!(actual, serde_json::to_vec_pretty(&expected).unwrap());
        let framing = Framing {
            total: Some(3),
            ..Framing::default()
        };
        assert!(write_frames(&mut Vec::new(), &envelope, "messages", &frames, framing).is_err());
        assert!(write_json_array(&mut Vec::new(), &json!({"messages":[1]}), "messages", &frames).is_err());
        assert!(write_json_array(&mut Vec::new(), &json!({"other":[]}), "messages", &frames).is_err());
        assert!(write_json_array(
            &mut Vec::new(),
            &json!({"messages":[]}),
            "messages",
            &ByteSource::read(b"1".as_slice()).unwrap()
        )
        .is_err());
    }

    #[test]
    fn stable_prefix_is_recorded_and_copied_from_the_old_artifact() {
        let world = crate::test_world("frames-stable-prefix");
        let envelope = json!({"id":"s","messages":[],"title":"t"});
        let frames = framed(&[json!({"a":1}), json!({"b":[2]}), json!("c")]);
        let mut first = Vec::new();
        let framing = Framing {
            pretty: true,
            stable: Some(2),
            total: Some(3),
            ..Framing::default()
        };
        let prefix = write_frames(&mut first, &envelope, "messages", &frames, framing)
            .unwrap()
            .unwrap();
        assert_eq!(prefix.messages, 2);
        assert!(prefix.valid(first.len() as u64, 2));
        let old = world.base.join("old.json");
        fs::write(&old, &first).unwrap();
        let mut second = Vec::new();
        let framing = Framing {
            pretty: true,
            old: Some((&old, &prefix)),
            stable: Some(2),
            total: Some(3),
        };
        let again = write_frames(&mut second, &envelope, "messages", &framed(&[json!("c")]), framing)
            .unwrap()
            .unwrap();
        assert_eq!(second, first);
        assert_eq!((again.start, again.end), (prefix.start, prefix.end));
        let framing = Framing {
            pretty: true,
            stable: Some(1),
            total: Some(0),
            ..Framing::default()
        };
        assert!(write_frames(&mut Vec::new(), &envelope, "messages", &framed(&[]), framing).is_err());
    }

    #[test]
    fn frame_stage_holds_one_compact_serialization_capped_to_the_remaining_budget() {
        let mut message = crate::Message::new(crate::Role::Assistant);
        message.content.push(crate::Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            namespace: None,
            input: json!({"quoted": "🌙\n\\\"", "nested": [{"a": []}, true, 12]}).into(),
            extensions: Default::default(),
        });
        let expected = serde_json::to_vec(&message).unwrap();
        let resolver = crate::Resolver::new(None);
        let mut stage = Vec::new();
        assert_eq!(
            write_frame(&mut stage, &message, &resolver, None).unwrap(),
            expected.len()
        );
        assert_eq!(stage[0], 0x1e);
        assert_eq!(stage[1..], expected);
        let mut capped = Vec::new();
        assert_eq!(
            write_frame(&mut capped, &message, &resolver, Some(7)).unwrap(),
            expected.len()
        );
        assert_eq!(capped.len(), 8);
    }
}
