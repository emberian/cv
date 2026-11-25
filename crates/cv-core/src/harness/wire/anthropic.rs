//! The Anthropic Messages content shape — `text | thinking | redacted_thinking | tool_use |
//! tool_result | image | document` — read into [`Block`]s and written back. Claude Code and Cline
//! persist it verbatim; the claude.ai export, OpenClaw, Goose and Zed's legacy threads renamed a
//! key or two, which a [`Quirks`] constant names so the adapter does not re-type the shape.

#[cfg(test)]
use crate::json::{ReadInput, Validation};
pub(crate) mod write;
use crate::ir::Block;
use crate::lazy::Text;
use serde_json::Value;

/// How a store spells the shape. Every field is the Anthropic spelling in [`ANTHROPIC`].
#[derive(Debug, Clone, Copy)]
pub struct Quirks {
    /// The tool-invocation block's `type` and its input key: `("tool_use", "input")`; OpenClaw
    /// `("toolCall", "arguments")`.
    pub tool_use: (&'static str, &'static str),
    /// The thinking block's text key and signature key: `("thinking", "signature")`; OpenClaw
    /// `("thinking", "thinkingSignature")`; Zed `("text", "signature")`.
    pub thinking: (&'static str, &'static str),
    /// The redacted-thinking block's `type`: `redacted_thinking`; Goose `redactedThinking`; Zed
    /// `RedactedThinking`. `None` for a store without one — on the way out a redacted block is then
    /// written as a plain thinking block.
    pub redacted_thinking: Option<&'static str>,
    /// An image's or document's payload: Anthropic's `source {type, media_type, data|url|file_id}`
    /// object (`None`), or flat `mimeType`/`data`/`name` fields with `data` kept as a reference
    /// truncated to this many chars (OpenClaw 80, Goose 120).
    pub flat_media: Option<usize>,
    /// Drop a `text` block whose text is empty rather than keep it (Zed, the claude.ai export).
    pub skip_empty_text: bool,
    /// An empty signature string means no signature (Goose, Zed), not a present-but-empty one.
    pub empty_signature_is_none: bool,
}

/// The shape as Anthropic defines it: Claude Code, Cline, Roo.
pub const ANTHROPIC: Quirks = Quirks {
    tool_use: ("tool_use", "input"),
    thinking: ("thinking", "signature"),
    redacted_thinking: Some("redacted_thinking"),
    flat_media: None,
    skip_empty_text: false,
    empty_signature_is_none: false,
};
/// The claude.ai account export: Anthropic's shape with empty text blocks dropped.
pub const CLAUDE_AI: Quirks = Quirks {
    skip_empty_text: true,
    ..ANTHROPIC
};
/// OpenClaw: `toolCall`/`arguments`, `thinkingSignature`, flat `mimeType`/`data` images.
pub const OPENCLAW: Quirks = Quirks {
    tool_use: ("toolCall", "arguments"),
    thinking: ("thinking", "thinkingSignature"),
    redacted_thinking: None,
    flat_media: Some(80),
    ..ANTHROPIC
};
/// Goose: `redactedThinking`, flat `mimeType`/`data`/`name` media, empty signatures absent.
pub const GOOSE: Quirks = Quirks {
    redacted_thinking: Some("redactedThinking"),
    flat_media: Some(120),
    empty_signature_is_none: true,
    ..ANTHROPIC
};
/// Zed's legacy (0.1.0) thread segments: thinking text under `text`, serde-tagged `RedactedThinking`.
pub const ZED: Quirks = Quirks {
    thinking: ("text", "signature"),
    redacted_thinking: Some("RedactedThinking"),
    skip_empty_text: true,
    empty_signature_is_none: true,
    ..ANTHROPIC
};

/// What [`block_from_view_with`] makes of `view` under `q`, decided from its `type` (and, for a
/// text block, whether `text` is a string) without decoding a body: `Some(true)` a tool result,
/// `Some(false)` any other block, `None` no block. What the "a user turn carrying only tool
/// results is a Tool turn" rule reads where a listing counts turns without building messages.
pub(crate) fn block_kind(view: crate::json::JsonView<'_>, q: &Quirks) -> anyhow::Result<Option<bool>> {
    use super::super::store::document::Fields;
    let Some(record) = Fields::parse(view, &["type", "text"])? else {
        return Ok(None);
    };
    let Some(ty) = record.string("type")? else {
        return Ok(None);
    };
    Ok(match ty.as_str() {
        "tool_result" => Some(true),
        "text" => match record.get("text") {
            Some(text) if text.is_string()? => {
                let mut empty = true;
                if q.skip_empty_text {
                    text.for_each_char(|_| {
                        empty = false;
                        false
                    })?;
                }
                (!(q.skip_empty_text && empty)).then_some(false)
            }
            _ => None,
        },
        "image" | "document" | "thinking" => Some(false),
        ty if ty == q.tool_use.0 || Some(ty) == q.redacted_thinking => Some(false),
        _ => None,
    })
}

/// Whether `content` is an array whose blocks are all tool results (and there is one), each
/// item kinded by `kind` as [`block_kind`] does: the "a user turn carrying only tool results is a
/// Tool turn" rule, read without building a block.
pub(crate) fn only_tool_results(
    content: Option<crate::json::JsonView<'_>>,
    mut kind: impl FnMut(crate::json::JsonView<'_>) -> anyhow::Result<Option<bool>>,
) -> anyhow::Result<bool> {
    let Some(items) = content.map(|c| c.array()).transpose()?.flatten() else {
        return Ok(false);
    };
    let (mut results, mut others) = (0, 0);
    for item in items {
        match kind(item?)? {
            Some(true) => results += 1,
            Some(false) => others += 1,
            None => {}
        }
    }
    Ok(results > 0 && others == 0)
}

/// A content block from a view, with the caller's text rule for content strings, `spans` for tool
/// arguments, and the size above which a source-backed tool-result array becomes checked joined
/// text rather than being coerced inline. Unselected fields are never decoded, in every demand.
pub(crate) fn block_from_view_with(
    view: crate::json::JsonView<'_>,
    q: &Quirks,
    spans: bool,
    text: &mut dyn FnMut(crate::json::JsonView<'_>) -> anyhow::Result<Option<Text>>,
    joined_over: u64,
) -> anyhow::Result<Option<Block>> {
    use super::super::store::document::Fields;
    let keys = [
        "type",
        "source",
        "title",
        "mimeType",
        "id",
        "name",
        "data",
        "tool_use_id",
        "is_error",
        "content",
        "text",
        q.thinking.0,
        q.thinking.1,
        q.tool_use.1,
    ];
    let Some(record) = Fields::parse(view, &keys)? else {
        return Ok(None);
    };
    let Some(ty) = record.string("type")? else {
        return Ok(None);
    };
    if ty == q.tool_use.0 {
        return Ok(Some(Block::ToolUse {
            id: record.string("id")?.unwrap_or_default(),
            name: record.string("name")?.unwrap_or_default(),
            input: record
                .get(q.tool_use.1)
                .map(|v| v.argument(spans))
                .transpose()?
                .unwrap_or_default(),
            namespace: None,
            extensions: Default::default(),
        }));
    }
    if matches!(ty.as_str(), "image" | "document") {
        if let Some(max) = q.flat_media {
            let mime = record.string("mimeType")?;
            let data = match record.get("data") {
                Some(data) if data.is_string()? => Some(data),
                _ => None,
            };
            return Ok(Some(if ty == "image" {
                let data_ref = data
                    .map(|v| -> anyhow::Result<_> {
                        let text = v.source_text()?.unwrap();
                        crate::projection::Head::of_text(&text, &crate::lazy::Resolver::new(None), max)
                    })
                    .transpose()?;
                Block::Image {
                    media_type: mime,
                    data_ref,
                    extensions: Default::default(),
                }
            } else {
                Block::File {
                    mime,
                    path: record.string("name")?,
                    source: data.map(|_| "base64:inline".into()),
                    extensions: Default::default(),
                }
            }));
        }
        let src = record
            .get("source")
            .map(|v| Fields::parse(v, &["type", "media_type", "url", "file_id", "data"]))
            .transpose()?
            .flatten();
        let media_type = src.as_ref().map(|r| r.string("media_type")).transpose()?.flatten();
        let marker = ty == "image";
        let data_ref = if let Some(src) = src {
            match src.string("type")?.as_deref() {
                Some("url") => src.string("url")?,
                Some("file") => src.string("file_id")?.map(|id| format!("file:{id}")),
                Some("base64") | None if marker => {
                    if src.get("data").map(|v| v.is_string()).transpose()?.unwrap_or(false) {
                        Some("base64:inline".into())
                    } else {
                        None
                    }
                }
                Some(other) if marker => Some(other.to_owned()),
                _ => None,
            }
        } else {
            None
        };
        return Ok(Some(if marker {
            Block::Image {
                media_type,
                data_ref,
                extensions: Default::default(),
            }
        } else {
            Block::File {
                mime: media_type,
                path: record.string("title")?,
                source: data_ref,
                extensions: Default::default(),
            }
        }));
    }
    let text_of = |key: &str, text: &mut dyn FnMut(crate::json::JsonView<'_>) -> anyhow::Result<Option<Text>>| {
        record.get(key).map(text).transpose().map(Option::flatten)
    };
    if ty == "text" {
        let Some(value) = text_of("text", text)? else {
            return Ok(None);
        };
        if q.skip_empty_text && value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(Block::from_text(value)));
    }
    if ty == "thinking" {
        let (text_key, sig_key) = q.thinking;
        return Ok(Some(Block::Thinking {
            text: text_of(text_key, text)?.unwrap_or_default(),
            signature: record
                .string(sig_key)?
                .filter(|s| !(q.empty_signature_is_none && s.is_empty())),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        }));
    }
    // Encrypted reasoning the API returns when thinking can't be shown in the clear. The plaintext
    // is intentionally absent; the opaque blob rides in `encrypted` so it is not silently lost.
    if Some(ty.as_str()) == q.redacted_thinking {
        return Ok(Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: record.string("data")?,
            redacted: true,
            extensions: Default::default(),
        }));
    }
    if ty != "tool_result" {
        return Ok(None);
    }
    // A string by the caller's rule; else the parts, which stay in the source only when the
    // array is over `joined_over` (a single part then as source text), and are otherwise owned.
    let content = match text_of("content", text)? {
        Some(content) => content,
        None => {
            let content = record.get("content");
            let large = match content {
                Some(c) => c.is_array()? && c.source_range()?.is_some_and(|(_, len)| len > joined_over),
                None => false,
            };
            let backing = super::Backing::Over(if large { joined_over } else { u64::MAX });
            super::joined(content, super::RESULT, backing, &mut |v| match large {
                true => v.source_text(),
                false => v.text(false),
            })?
        }
    };
    Ok(Some(Block::ToolResult {
        tool_use_id: record.string("tool_use_id")?.unwrap_or_default(),
        content,
        is_error: record
            .get("is_error")
            .map(|v| v.as_bool())
            .transpose()?
            .flatten()
            .unwrap_or(false),
        tool_name: None,
        status: None,
        details: None,
        extensions: Default::default(),
    }))
}

// ------------------------------------------------------------------------------------------------
// writing
// ------------------------------------------------------------------------------------------------

/// An assistant turn's blocks as content: text, thinking (redacted as the store's redacted block,
/// or a plain one when it has none), tool invocations, images; a [`Block::File`] becomes a
/// `[file: …]` text marker (documents are a user-turn shape); tool results are skipped (they go
/// on the Tool turn via [`tool_result_blocks`]).
pub fn assistant_blocks(content: &[Block], q: &Quirks) -> anyhow::Result<Vec<Value>> {
    blocks_to(content, q, false)
}

/// A user turn's blocks as content: text and images natively, a [`Block::File`] as the
/// `document` attachment the reader turns back into a File (stores with flat media get the text
/// marker), a stray tool result (mixed user content) as a `tool_result` block; thinking and tool
/// invocations do not occur on user turns but map to their native shapes rather than vanishing.
#[cfg(test)]
pub fn user_blocks(content: &[Block], q: &Quirks) -> anyhow::Result<Vec<Value>> {
    blocks_to(content, q, true)
}

/// A Tool turn's results as `tool_result` blocks the reader pairs by id.
pub fn tool_result_blocks(content: &[Block]) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for block in content.iter().filter(|b| matches!(b, Block::ToolResult { .. })) {
        if let Some(value) = block_to(block, &ANTHROPIC, true)? {
            out.push(value);
        }
    }
    Ok(out)
}

fn blocks_to(content: &[Block], q: &Quirks, user: bool) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for block in content {
        if let Some(value) = block_to(block, q, user)? {
            out.push(value);
        }
    }
    Ok(out)
}

fn block_to(b: &Block, q: &Quirks, user: bool) -> anyhow::Result<Option<Value>> {
    write::owned(b, q, user)
}

/// An image rebuilt from its retained reference, using the same grammar as streamed output.
pub fn image_block(media_type: &Option<String>, data_ref: &Option<String>, q: &Quirks) -> Value {
    write::owned(
        &Block::Image {
            media_type: media_type.clone(),
            data_ref: data_ref.clone(),
            extensions: Default::default(),
        },
        q,
        false,
    )
    .expect("image fields serialize infallibly")
    .expect("image is an assistant block")
}

/// The owned entry point, for tests: a `Value` through the one view codec, `(&Value).into()`.
#[cfg(test)]
pub(crate) fn block_from(item: &Value, q: &Quirks) -> Option<Block> {
    block_from_view_with(item.into(), q, false, &mut |v| v.text(false), u64::MAX)
        .expect("an owned value has no source reads")
}

/// Every block of a `content` array; a bare string is one text block.
#[cfg(test)]
fn blocks_from(content: Option<&Value>, q: &Quirks) -> Vec<Block> {
    match content {
        Some(Value::String(s)) => vec![Block::from_text(s.clone().into())],
        Some(Value::Array(items)) => items.iter().filter_map(|i| block_from(i, q)).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_anthropic_block() {
        let items = json!([
            {"type": "text", "text": ""},
            {"type": "thinking", "thinking": "hm", "signature": ""},
            {"type": "redacted_thinking", "data": "BLOB"},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "x"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "ok"}, {"type": "image"}], "is_error": true},
            {"type": "image", "source": {"type": "file", "file_id": "f9", "media_type": "image/png"}},
            {"type": "document", "title": "a.pdf", "source": {"type": "url", "url": "https://x/a.pdf", "media_type": "application/pdf"}},
            {"type": "server_tool_use"}
        ]);
        let b = blocks_from(Some(&items), &ANTHROPIC);
        assert_eq!(b.len(), 7, "unknown types are skipped, empty text is kept");
        assert!(matches!(&b[0], Block::Text { text , .. } if text.is_empty()));
        assert!(matches!(&b[1], Block::Thinking { signature: Some(s), redacted: false, .. } if s.is_empty()));
        assert!(matches!(&b[2], Block::Thinking { encrypted: Some(e), redacted: true, .. } if e == "BLOB"));
        assert!(matches!(&b[3], Block::ToolUse { name, .. } if name == "Read"));
        assert!(
            matches!(&b[4], Block::ToolResult { content, is_error: true, .. } if content.matches_str("ok\n[image]") == Some(true))
        );
        assert!(matches!(&b[5], Block::Image { data_ref: Some(r), .. } if r == "file:f9"));
        assert!(
            matches!(&b[6], Block::File { path: Some(p), source: Some(s), .. } if p == "a.pdf" && s == "https://x/a.pdf")
        );
    }

    #[test]
    fn quirks_rename_keys() {
        let oc = json!([
            {"type": "toolCall", "id": "c", "name": "exec", "arguments": {"cmd": "ls"}},
            {"type": "thinking", "thinking": "t", "thinkingSignature": "S"},
            {"type": "image", "mimeType": "image/png", "data": "0123456789"}
        ]);
        let b = blocks_from(Some(&oc), &OPENCLAW);
        assert!(matches!(&b[0], Block::ToolUse { input, .. } if input.resolve().unwrap()["cmd"] == "ls"));
        assert!(matches!(&b[1], Block::Thinking { signature: Some(s), .. } if s == "S"));
        assert!(
            matches!(&b[2], Block::Image { media_type: Some(m), data_ref: Some(d) , .. } if m == "image/png" && d == "0123456789")
        );
        let zed = json!([{"type": "text", "text": ""}, {"type": "thinking", "text": "z", "signature": ""}, {"type": "RedactedThinking", "data": "D"}]);
        let b = blocks_from(Some(&zed), &ZED);
        assert_eq!(b.len(), 2, "Zed drops empty text");
        assert!(matches!(&b[0], Block::Thinking { text, signature: None, .. } if text.matches_str("z") == Some(true)));
        assert!(matches!(&b[1], Block::Thinking { redacted: true, .. }));
        let goose = json!([{"type": "redactedThinking", "data": "D"}, {"type": "document", "name": "n.pdf", "mimeType": "application/pdf", "data": "AAAA"}]);
        let b = blocks_from(Some(&goose), &GOOSE);
        assert!(matches!(&b[0], Block::Thinking { redacted: true, .. }));
        assert!(
            matches!(&b[1], Block::File { path: Some(p), source: Some(s), .. } if p == "n.pdf" && s == "base64:inline")
        );
    }

    #[test]
    fn writes_what_it_reads() {
        let blocks = vec![
            Block::from_text("hi".into()),
            Block::Thinking {
                text: "".into(),
                signature: None,
                encrypted: Some("BLOB".into()),
                redacted: true,
                extensions: Default::default(),
            },
            Block::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: json!({"path": "x"}).into(),
                namespace: None,
                extensions: Default::default(),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("base64:inline".into()),
                extensions: Default::default(),
            },
            Block::File {
                mime: None,
                path: Some("a.pdf".into()),
                source: Some("file:f9".into()),
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ];
        let a = assistant_blocks(&blocks, &ANTHROPIC).unwrap();
        assert_eq!(a.len(), 5, "tool results are not assistant content");
        assert_eq!(a[1]["type"], "redacted_thinking");
        assert_eq!(a[4]["type"], "text", "a file on an assistant turn is a marker");
        let u = user_blocks(&blocks, &ANTHROPIC).unwrap();
        assert_eq!(u[4]["type"], "document");
        assert_eq!(u[5]["type"], "tool_result");
        let back = blocks_from(Some(&Value::Array(u)), &ANTHROPIC);
        assert_eq!(back.len(), blocks.len());
        assert!(
            matches!(&back[4], Block::File { path: Some(p), source: Some(s), .. } if p == "a.pdf" && s == "file:f9")
        );
        // OpenClaw has no redacted block: the thinking block is written plain, under its keys.
        let oc = assistant_blocks(&blocks, &OPENCLAW).unwrap();
        assert_eq!(oc[1]["type"], "thinking");
        assert_eq!(oc[2]["type"], "toolCall");
        assert_eq!(oc[2]["arguments"]["path"], "x");
        assert_eq!(oc[3]["mimeType"], "image/png");
        assert_eq!(tool_result_blocks(&blocks).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod selected_media_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn owned_codec_returns_changed_text_errors_instead_of_panicking() -> anyhow::Result<()> {
        let world = crate::test_world("anthropic-owned-source-error");
        let path = world.base.join("text.json");
        let raw = serde_json::to_vec(&"large text ".repeat(10000))?;
        std::fs::write(&path, &raw)?;
        let text = crate::json::JsonSource::capture(&path)?
            .read(ReadInput::Range(0, raw.len() as u64), Validation::Value, |view| {
                view.source_text()
            })?
            .unwrap();
        std::fs::write(&path, b"changed source")?;
        for block in [
            Block::from_text(text.clone()),
            Block::Thinking {
                text: text.clone(),
                signature: None,
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: text,
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ] {
            assert!(block_to(&block, &ANTHROPIC, true).is_err());
        }
        Ok(())
    }
    #[test]
    fn wide_composite_results_preserve_empty_parts_image_precedence_and_guard() -> anyhow::Result<()> {
        let world = crate::test_world("wire-wide-result");
        let path = world.base.join("result.json");
        let mut parts = Vec::new();
        for i in 0..8192 {
            parts.push(match i % 4 {
                0 => json!({"text":""}),
                1 => json!({"ignored":"not a text part"}),
                2 => json!({"type":"image_url","text":"ignored image text"}),
                _ => json!({"text":format!("part-{i} λ\n")}),
            });
        }
        let value = json!({"type":"tool_result","tool_use_id":"call","content":parts});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let block = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        assert_eq!(
            serde_json::to_value(&block)?,
            serde_json::to_value(block_from(&value, &ANTHROPIC))?
        );
        std::fs::write(&path, b"changed")?;
        if let Block::ToolResult { content, .. } = block {
            assert!(content.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
        } else {
            panic!("tool result");
        }
        Ok(())
    }

    #[test]
    fn composite_result_projection_matches_owned_codec_without_joined_body_ownership() -> anyhow::Result<()> {
        let world = crate::test_world("wire-composite-result");
        let path = world.base.join("result.json");
        let body = "λ\n\"result\"".repeat(20000);
        let value = json!({"type":"tool_result","tool_use_id":"call","is_error":true,"content":[{"text":""},{"text":body,"ignored":body},{"type":"image_url","text":"ignored image text"},{"text":""},{"text":body}],"ignored":body});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let block = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        assert!(matches!(
            &block,
            Block::ToolResult {
                content: Text::Source(_),
                ..
            }
        ));
        let mut session = crate::Session::new("result", crate::Harness::Claude);
        let mut message = crate::Message::new(crate::Role::Tool);
        message.content.push(block);
        session.messages.push(message);
        session.materialize()?;
        assert_eq!(
            serde_json::to_value(&session.messages[0].content[0])?,
            serde_json::to_value(block_from(&value, &ANTHROPIC))?
        );
        let block = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                block_from_view_with(view, &ANTHROPIC, true, &mut |v| v.text(true), 0)
            })?
            .unwrap();
        std::fs::write(&path, b"changed after selected join")?;
        if let Block::ToolResult { content, .. } = block {
            assert!(content.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
        }
        Ok(())
    }
    #[test]
    fn flat_media_projection_matches_owned_codec_with_large_ignored_payloads() -> anyhow::Result<()> {
        let world = crate::test_world("wire-flat-media");
        let path = world.base.join("part.json");
        for quirks in [OPENCLAW, GOOSE] {
            for ty in ["image", "document"] {
                for data in [json!(""), json!("λ\n\"".repeat(20000)), json!(false)] {
                    let value = json!({"type":ty,"mimeType":"image/png","data":data,"name":"attachment","unknown":"x".repeat(200000)});
                    std::fs::write(&path, serde_json::to_vec(&value)?)?;
                    let source = crate::json::JsonSource::capture(&path)?;
                    let selected = source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                        block_from_view_with(view, &quirks, true, &mut |v| v.text(true), 0)
                    })?;
                    assert_eq!(
                        serde_json::to_value(selected)?,
                        serde_json::to_value(block_from(&value, &quirks))?
                    );
                }
            }
        }
        Ok(())
    }
}
