//! Bounded text projections shared by search and API consumers.
mod json_frames;
#[cfg(feature = "sqlite")]
pub(crate) use json_frames::write_frame_with;
mod json_string;
mod string;
use crate::ir::truncate;
use crate::{Resolver, Text};
pub use json_frames::{write_frame, write_frames, write_json_array, Counted, Framing, Prefix};
pub use json_string::{for_each_lossy_utf8_chunk, write_json_string};
pub(crate) use string::{string_prefix, string_until};

/// Append complete canonical JSON into an already demanded string without an intermediate copy.
/// A failure can leave a prefix appended; callers must discard their staged output on error.
pub fn append_json(out: &mut String, json: &crate::json::Json) -> anyhow::Result<()> {
    struct StringWriter<'a>(&'a mut String);
    impl std::io::Write for StringWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .push_str(std::str::from_utf8(bytes).map_err(std::io::Error::other)?);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    json.write_canonical(&mut StringWriter(out))
}

/// A bounded text projection: `new` selects an exact UTF-8 byte prefix; `bounded` or
/// `trimmed` selects a character-limited display label with one character of lookahead.
/// Once a byte-budget scalar does not fit, later writes cannot skip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    text: String,
    limit: usize,
    stopped: bool,
    title: Option<Title>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Title {
    chars: usize,
    nonwhite: usize,
    trim: bool,
}
impl Head {
    /// The listing title bound: every codec's title, at every demand.
    pub const TITLE: usize = 80;
    /// Character-limited label, flattened and ellipsized by `finish`.
    pub fn bounded(max: usize) -> Self {
        Self {
            text: String::new(),
            limit: max,
            stopped: false,
            title: Some(Title {
                chars: 0,
                nonwhite: 0,
                trim: false,
            }),
        }
    }
    /// Character-limited label after trimming surrounding whitespace.
    pub fn trimmed(max: usize) -> Self {
        let mut head = Self::bounded(max);
        head.title.as_mut().unwrap().trim = true;
        head
    }
    /// Characters retained, including the truncation lookahead.
    pub const fn kept(max: usize) -> usize {
        max.saturating_add(1)
    }
    pub fn push_char(&mut self, c: char) -> bool {
        if self.title.is_none() {
            return self.push(c.encode_utf8(&mut [0; 4]));
        }
        if self.done() {
            return true;
        }
        let t = self.title.as_mut().unwrap();
        if t.trim && t.chars == 0 && c.is_whitespace() {
            return false;
        }
        if t.chars < Self::kept(self.limit) {
            self.text.push(c);
            t.chars += 1;
        }
        if !c.is_whitespace() {
            t.nonwhite = t.chars;
        }
        self.done()
    }
    /// More input cannot change the resulting prefix or label.
    pub fn done(&self) -> bool {
        self.title.as_ref().map_or_else(
            || self.full(),
            |t| t.nonwhite > self.limit || (!t.trim && t.chars > self.limit && t.nonwhite > 0),
        )
    }
    pub fn is_blank(&self) -> bool {
        self.title
            .as_ref()
            .map_or_else(|| self.text.trim().is_empty(), |t| t.nonwhite == 0)
    }
    pub fn finish(&self) -> String {
        match &self.title {
            Some(t) if t.trim => truncate(&self.text.chars().take(t.nonwhite).collect::<String>(), self.limit),
            Some(_) => truncate(&self.text, self.limit),
            None => self.text.clone(),
        }
    }
    /// The bounded head of a lazy text, reading at most `max + 1` characters of it.
    pub(crate) fn of_text(
        text: &crate::lazy::Text,
        resolver: &crate::lazy::Resolver,
        max: usize,
    ) -> anyhow::Result<String> {
        let (head, _) = crate::harness::store::document::text_head(text, resolver, Self::kept(max), false)?;
        Ok(truncate(&head, max))
    }
    /// The bounded head of a JSON string; `None` for a nonstring.
    pub(crate) fn of_string(view: crate::json::JsonView<'_>, max: usize) -> anyhow::Result<Option<String>> {
        Ok(crate::projection::string_prefix(view, Self::kept(max))?.map(|head| truncate(&head, max)))
    }
    /// The bounded head of a value's canonical JSON (four bytes cover any character).
    pub fn of_json(view: crate::json::JsonView<'_>, max: usize) -> anyhow::Result<String> {
        let (head, _) = view.canonical_prefix(Self::kept(max).saturating_mul(4))?;
        Ok(truncate(&head, max))
    }
    pub fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit),
            limit,
            stopped: false,
            title: None,
        }
    }
    pub fn remaining(&self) -> usize {
        if let Some(t) = &self.title {
            return Self::kept(self.limit).saturating_sub(t.chars).saturating_mul(4);
        }
        if self.stopped {
            0
        } else {
            self.limit - self.text.len()
        }
    }
    pub fn full(&self) -> bool {
        self.title
            .as_ref()
            .map_or_else(|| self.remaining() == 0, |t| t.chars > self.limit)
    }
    pub fn push(&mut self, text: &str) -> bool {
        if self.title.is_some() {
            return text.chars().any(|c| self.push_char(c)) || self.done();
        }
        if self.full() {
            return true;
        }
        let mut end = text.len().min(self.remaining());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.stopped |= end < text.len();
        self.full()
    }
    /// Checked text demand within the remaining decoded-byte budget.
    pub fn try_push_text(&mut self, text: &Text, resolver: &Resolver) -> anyhow::Result<()> {
        if self.title.is_some() {
            return crate::stream::stopped(text.try_for_each_chunk(resolver, 4096, |part| {
                if self.push(part) {
                    Err(crate::stream::Stop.into())
                } else {
                    Ok(())
                }
            }))
            .map(|_| ());
        }
        if !self.full() {
            if let Text::Source(source) = text {
                let (prefix, truncated) = source.try_prefix_with_truncation(self.remaining())?;
                self.push(&prefix);
                self.stopped |= truncated;
                return Ok(());
            }
            let prefix = text.try_resolve_prefix(resolver, self.remaining() as u64)?;
            self.push(&prefix);
        }
        Ok(())
    }
    pub fn push_json(&mut self, json: &crate::json::Json) -> anyhow::Result<()> {
        if !self.full() {
            let (prefix, truncated) = json.canonical_prefix(self.remaining())?;
            self.push(&prefix);
            self.stopped |= truncated;
        }
        Ok(())
    }
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn len(&self) -> usize {
        self.text.len()
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.text.capacity()
    }
    pub fn into_string(self) -> String {
        self.text
    }
}

/// Where [`searchable`] writes: complete bytes, a bounded head, or an index's chunk reducer.
/// Consumers own decoding budgets and dependency capture; block selection and separators live
/// in the walker alone.
pub trait Searchable {
    fn full(&self) -> bool {
        false
    }
    fn str(&mut self, s: &str);
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
    fn json(&mut self, json: &crate::json::Json, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;
    /// Optional consumer enrichment after the block's own projection (for example, a persisted
    /// tool output). Called only while the consumer still demands bytes.
    fn after_block(&mut self, _block: &crate::Block) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The searchable projection of one message, spelled once: text, thinking and tool-result text
/// each followed by a newline; a tool call as `name input` (canonical JSON) and a newline; a
/// file's path (else its source) and a newline; an image nothing. `check` runs before each
/// block and once at the end, and is passed to the consumer for checks during text/JSON reads.
pub fn searchable(
    blocks: &[crate::Block],
    resolver: &Resolver,
    out: &mut impl Searchable,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for block in blocks {
        check()?;
        if out.full() {
            break;
        }
        match block {
            crate::Block::Text { text, .. }
            | crate::Block::Thinking { text, .. }
            | crate::Block::ToolResult { content: text, .. } => {
                out.text(text, resolver, check)?;
                out.str("\n");
            }
            crate::Block::ToolUse { name, input, .. } => {
                out.str(name);
                out.str(" ");
                out.json(input, check)?;
                out.str("\n");
            }
            crate::Block::File { path, source, .. } => {
                if let Some(path) = path.as_deref().or(source.as_deref()) {
                    out.str(path);
                    out.str("\n");
                }
            }
            crate::Block::Image { .. } | crate::Block::Opaque { .. } => {}
        }
        if !out.full() {
            out.after_block(block)?;
        }
    }
    check()
}

/// The complete projection, as bytes (a JSON write may end inside a scalar; the whole is UTF-8).
impl Searchable for Vec<u8> {
    fn str(&mut self, s: &str) {
        self.extend_from_slice(s.as_bytes());
    }
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        text.try_for_each_chunk(resolver, 4096, |part| {
            check()?;
            self.extend_from_slice(part.as_bytes());
            Ok(())
        })
    }
    fn json(&mut self, json: &crate::json::Json, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        struct Checked<'a, 'b> {
            bytes: &'a mut Vec<u8>,
            check: &'a mut (dyn FnMut() -> anyhow::Result<()> + 'b),
        }
        impl std::io::Write for Checked<'_, '_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                (self.check)().map_err(std::io::Error::other)?;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        json.write_canonical(&mut Checked { bytes: self, check })
    }
}

/// A bounded head: decoding stops once the budget is spent.
impl Searchable for Head {
    fn full(&self) -> bool {
        Head::full(self)
    }
    fn str(&mut self, s: &str) {
        self.push(s);
    }
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        crate::stream::stopped(text.try_for_each_chunk(resolver, 4096, |part| {
            check()?;
            self.push(part);
            if Head::full(self) {
                Err(crate::stream::Stop.into())
            } else {
                Ok(())
            }
        }))
        .map(|_| ())
    }
    fn json(&mut self, json: &crate::json::Json, _: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.push_json(json)
    }
}

/// Complete searchable projection with cooperative cancellation between text chunks and
/// canonical JSON writes. The joined output is demanded one message at a time.
pub fn searchable_text_checked(
    message: &crate::Message,
    resolver: &Resolver,
    mut check: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    searchable(&message.content, resolver, &mut bytes, &mut check)?;
    Ok(String::from_utf8(bytes)?)
}

/// Bytes kept past `limit` as [`searchable_prefix_checked`]'s truncation witness: room for one
/// more UTF8 scalar, so an exact fit stays within `limit` and reads as untruncated.
const WITNESS_BYTES: usize = 4;

/// Exact decoded byte prefix of the searchable message projection, with a truncation witness.
/// Demanded text uses checked chunks; JSON uses the canonical bounded writer. No later block
/// is demanded after the prefix is established.
pub fn searchable_prefix(
    message: &crate::Message,
    resolver: &Resolver,
    limit: usize,
) -> anyhow::Result<(String, bool)> {
    searchable_prefix_checked(message, resolver, limit, || Ok(()))
}

/// Bounded searchable projection with cooperative checks at block and decoded text chunks.
pub fn searchable_prefix_checked(
    message: &crate::Message,
    resolver: &Resolver,
    limit: usize,
    mut check: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<(String, bool)> {
    // The witness bytes distinguish an exact fit from the next UTF8 scalar not fitting.
    let mut head = Head {
        text: String::with_capacity(limit.saturating_add(WITNESS_BYTES).min(64 * 1024)),
        limit: limit.saturating_add(WITNESS_BYTES),
        stopped: false,
        title: None,
    };
    searchable(&message.content, resolver, &mut head, &mut check)?;
    let mut text = head.into_string();
    let truncated = text.len() > limit;
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    Ok((text, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_searchable_projection_matches_owned_text_and_interrupts_chunks() {
        let mut message = crate::Message::new(crate::Role::Assistant);
        message.content = vec![
            crate::Block::from_text("λ ordinary\n".into()),
            crate::Block::Thinking {
                text: "thinking".into(),
                signature: None,
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            crate::Block::ToolUse {
                id: "call".into(),
                name: "tool".into(),
                input: serde_json::json!({"z":"λ", "a":2}).into(),
                namespace: None,
                extensions: Default::default(),
            },
            crate::Block::ToolResult {
                tool_use_id: "call".into(),
                content: "result".into(),
                is_error: false,
                tool_name: None,
                details: None,
                status: None,
                extensions: Default::default(),
            },
        ];
        let mut expected = String::new();
        message.append_searchable(&mut expected).unwrap();
        assert_eq!(
            searchable_text_checked(&message, &Resolver::new(None), || Ok(())).unwrap(),
            expected
        );
        message.content = vec![crate::Block::from_text("λ".repeat(32 * 1024).into())];
        let mut checks = 0;
        let error = searchable_text_checked(&message, &Resolver::new(None), || {
            checks += 1;
            anyhow::ensure!(checks < 4, "cancelled inside payload");
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled inside payload");
        assert_eq!(checks, 4);
    }

    #[test]
    fn searchable_prefix_checks_escaped_span_and_retained_text() {
        let dir = cv_testkit::temp_dir("searchable-escaped-prefix");
        let path = dir.join("text.json");
        let text = "月🌙é\n\"\\".repeat(4000);
        let raw = serde_json::to_vec(&text).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let json = crate::Json::source(&path, 0, raw.len() as u64).unwrap();
        let retained = json.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
        let span = Text::Span(crate::lazy::Span {
            source: Some(path.clone()),
            offset: 1,
            len: raw.len() as u64 - 2,
            escaped: true,
        });
        let resolver = Resolver::new(None);
        for body in [span, retained] {
            let mut message = crate::Message::new(crate::Role::Assistant);
            message.content.push(crate::Block::from_text(body));
            let oracle = format!("{text}\n");
            for cap in [0, 1, 2, 3, 4, 7, 16000, oracle.len(), oracle.len() + 4] {
                let (actual, truncated) = searchable_prefix(&message, &resolver, cap).unwrap();
                let mut end = oracle.len().min(cap);
                while !oracle.is_char_boundary(end) {
                    end -= 1;
                }
                assert_eq!(actual, oracle[..end]);
                assert_eq!(truncated, oracle.len() > cap);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn searchable_prefix_matches_owned_projection_at_every_unicode_boundary() {
        let mut message = crate::Message::new(crate::Role::Assistant);
        message.content = vec![
            crate::Block::from_text("月🌙é\n".into()),
            crate::Block::ToolUse {
                id: "c".into(),
                name: "tool".into(),
                input: serde_json::json!({"z":"🌙\n\"\\", "a":2}).into(),
                namespace: None,
                extensions: Default::default(),
            },
            crate::Block::ToolResult {
                tool_use_id: "c".into(),
                tool_name: None,
                content: "done月".into(),
                is_error: false,
                details: None,
                status: None,
                extensions: Default::default(),
            },
        ];
        let mut oracle = String::new();
        message.append_searchable(&mut oracle).unwrap();
        for cap in 0..=oracle.len() + 4 {
            let (actual, truncated) = searchable_prefix(&message, &Resolver::new(None), cap).unwrap();
            assert_eq!(
                searchable_prefix_checked(&message, &Resolver::new(None), cap, || Ok(())).unwrap(),
                (actual.clone(), truncated)
            );
            let mut end = oracle.len().min(cap);
            while !oracle.is_char_boundary(end) {
                end -= 1;
            }
            assert_eq!(actual, oracle[..end], "cap={cap}");
            assert_eq!(truncated, oracle.len() > cap);
        }
    }

    #[test]
    fn source_text_prefix_preserves_unicode_cutoff_and_zero_budget() {
        let dir = cv_testkit::temp_dir("source-text-head");
        let path = dir.join("source.json");
        let decoded = format!("abé{}", "x".repeat(20_000));
        std::fs::write(&path, serde_json::to_vec(&decoded).unwrap()).unwrap();
        let value = crate::json::Json::source(&path, 0, std::fs::metadata(&path).unwrap().len()).unwrap();
        let text = value.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
        let Text::Source(source) = &text else {
            panic!("retained source")
        };
        assert_eq!(source.try_prefix_with_truncation(0).unwrap(), (String::new(), true));
        assert_eq!(source.try_prefix_with_truncation(3).unwrap(), ("ab".into(), true));
        assert_eq!(
            source.try_prefix_with_truncation(decoded.len()).unwrap(),
            (decoded.clone(), false)
        );
        let mut head = Head::new(3);
        head.try_push_text(&text, &Resolver::new(None)).unwrap();
        head.push("\n");
        assert_eq!(head.as_str(), "ab");
        assert!(head.full());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checked_text_head_propagates_selected_source_failure() {
        let resolver = Resolver::new(None);
        let text = Text::Span(crate::lazy::Span {
            source: None,
            offset: 0,
            len: 20,
            escaped: false,
        });
        let mut head = Head::new(8);
        assert!(head.try_push_text(&text, &resolver).is_err());
        assert!(head.is_empty());
        let mut full = Head::new(0);
        full.try_push_text(&text, &resolver).unwrap();
        let mut inline = Head::new(3);
        inline.try_push_text(&Text::from("abé"), &resolver).unwrap();
        inline.push("z");
        assert_eq!(inline.as_str(), "ab");
    }

    #[test]
    fn prefix_cannot_skip_a_scalar_that_does_not_fit() {
        let mut head = Head::new(3);
        head.push("ab");
        head.push("é");
        head.push("x");
        assert_eq!(head.as_str(), "ab");
        assert!(head.full());
        assert_eq!(head.capacity(), 3);
    }

    #[test]
    fn canonical_json_and_following_text_share_one_prefix_budget() {
        let json = crate::json::Json::from(serde_json::json!({"arg":"é\n\""}));
        let expected = format!("tool {}\ntail", json.resolve().unwrap());
        for cap in 0..=expected.len() {
            let mut head = Head::new(cap);
            head.push("tool ");
            head.push_json(&json).unwrap();
            head.push("\ntail");
            let mut end = cap.min(expected.len());
            while !expected.is_char_boundary(end) {
                end -= 1;
            }
            assert_eq!(head.as_str(), &expected[..end], "cap {cap}");
            assert_eq!(head.capacity(), cap);
        }
    }
}
