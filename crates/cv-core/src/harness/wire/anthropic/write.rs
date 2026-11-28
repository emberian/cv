//! The existing Anthropic block mapping with checked, bounded text/argument publication.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    lazy::Resolver,
};
use anyhow::Result;
use std::io::Write;
/// Native file references share the same tagged source shape for images and documents.
fn source_reference(source: &mut ObjectWriter<'_>, reference: &str) -> Result<()> {
    if let Some(id) = reference.strip_prefix("file:") {
        source.field("type", "file")?;
        source.field("file_id", id)
    } else {
        source.field("type", "url")?;
        source.field("url", reference)
    }
}

/// Public owned codecs retain Span descriptors; checked emitters resolve text directly.
enum LeafPublication<'a> {
    Owned,
    Checked(&'a Resolver),
}
impl LeafPublication<'_> {
    fn text(&self, writer: &mut dyn Write, text: &Text) -> Result<()> {
        match self {
            Self::Checked(resolver) => wire::text(writer, text, resolver),
            Self::Owned => match text {
                Text::Inline(value) => {
                    serde_json::to_writer(writer, value)?;
                    Ok(())
                }
                Text::Span(span) => {
                    serde_json::to_writer(writer, span)?;
                    Ok(())
                }
                Text::Source(source) => {
                    serde_json::to_writer(writer, &source.try_resolve()?)?;
                    Ok(())
                }
            },
        }
    }
}

pub(crate) fn block(writer: &mut dyn Write, b: &Block, q: &Quirks, user: bool, resolver: &Resolver) -> Result<()> {
    publish(writer, b, q, user, LeafPublication::Checked(resolver))
}

/// Compatibility entry points already request an owned Value. They buffer its encoded form
/// before parsing it; hot emitters call `block` directly and never take this allocation path.
pub(super) fn owned(b: &Block, q: &Quirks, user: bool) -> Result<Option<Value>> {
    if matches!(b, Block::Opaque { .. }) || (!user && matches!(b, Block::ToolResult { .. })) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    publish(&mut bytes, b, q, user, LeafPublication::Owned)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn publish(writer: &mut dyn Write, b: &Block, q: &Quirks, user: bool, leaf: LeafPublication<'_>) -> Result<()> {
    let mut o = ObjectWriter::new(writer)?;
    match b {
        Block::Opaque { .. } => anyhow::bail!("cannot encode an opaque OpenSession block in a native turn"),
        Block::Text { text, .. } => {
            o.field("type", "text")?;
            o.field_with("text", |w| leaf.text(w, text))?;
        }
        Block::Thinking {
            text,
            signature,
            encrypted,
            redacted,
            ..
        } => match q.redacted_thinking {
            Some(t) if *redacted => {
                o.field("type", t)?;
                if let Some(enc) = encrypted {
                    o.field("data", enc)?;
                }
            }
            _ => {
                o.field("type", "thinking")?;
                o.field_with(q.thinking.0, |w| leaf.text(w, text))?;
                if let Some(sig) = signature {
                    o.field(q.thinking.1, sig)?;
                }
            }
        },
        Block::ToolUse { id, name, input, .. } => {
            o.field("type", q.tool_use.0)?;
            o.field("id", id)?;
            o.field("name", name)?;
            o.field_with(q.tool_use.1, |w| input.write_canonical(&mut &mut *w))?;
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            anyhow::ensure!(user, "tool result is a user block");
            o.field("type", "tool_result")?;
            o.field("tool_use_id", tool_use_id)?;
            o.field_with("content", |w| leaf.text(w, content))?;
            o.field("is_error", is_error)?;
        }
        Block::Image {
            media_type, data_ref, ..
        } => {
            o.field("type", "image")?;
            if q.flat_media.is_some() {
                if let Some(mt) = media_type {
                    o.field("mimeType", mt)?;
                }
                if let Some(d) = data_ref {
                    o.field("data", d)?;
                }
            } else {
                o.field_with("source", |w| {
                    let mut s = ObjectWriter::new(w)?;
                    if let Some(mt) = media_type {
                        s.field("media_type", mt)?;
                    }
                    match data_ref.as_deref() {
                        Some("base64:inline") => {
                            s.field("type", "base64")?;
                        }
                        Some(r) => source_reference(&mut s, r)?,
                        None => {}
                    }
                    s.finish()
                })?;
            }
        }
        Block::File { mime, path, source, .. } if user && q.flat_media.is_none() => {
            o.field("type", "document")?;
            if let Some(p) = path {
                o.field("title", p)?;
            }
            o.field_with("source", |w| {
                let mut s = ObjectWriter::new(w)?;
                if let Some(mt) = mime {
                    s.field("media_type", mt)?;
                }
                if let Some(reference) = source.as_deref() {
                    source_reference(&mut s, reference)?;
                }
                s.finish()
            })?;
        }
        Block::File { path, source, .. } => {
            o.field("type", "text")?;
            o.field_with("text", |w| {
                crate::harness::store::text::generate_string(w, |builder| {
                    builder.push(crate::emit::fallback::FILE_PREFIX)?;
                    builder.push(crate::emit::fallback::degraded_file_label(
                        path.as_deref(),
                        source.as_deref(),
                    ))?;
                    builder.push(crate::emit::fallback::BRACKET_SUFFIX)
                })
            })?;
        }
    }
    o.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn block_contracts_match_frozen_dialects_roles_and_leaf_publication() -> Result<()> {
        let world = cv_testkit::World::new("anthropic-wire-frozen");
        let path = world.base.join("text.json");
        let encoded = serde_json::to_vec("source \"λ\"\n")?;
        std::fs::write(&path, &encoded)?;
        let source_json = crate::Json::source(&path, 0, encoded.len() as u64)?;
        let crate::Json::Source(range) = &source_json else {
            unreachable!("captured source")
        };
        let source = Text::Source(crate::lazy::SourceText::new(range.clone()));
        let span = Text::Span(crate::lazy::Span {
            source: None,
            offset: 1,
            len: encoded.len() as u64 - 2,
            escaped: true,
        });
        let resolver = Resolver::new(Some(path));
        let content = vec![
            Block::from_text("a\"\\\nλ".into()),
            Block::from_text("".into()),
            Block::from_text(span.clone()),
            Block::from_text(source.clone()),
            Block::Thinking {
                text: "thought".into(),
                signature: Some(String::new()),
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            Block::Thinking {
                text: span.clone(),
                signature: Some("signed".into()),
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            Block::Thinking {
                text: source.clone(),
                signature: None,
                encrypted: Some("opaque".into()),
                redacted: true,
                extensions: Default::default(),
            },
            Block::Thinking {
                text: "".into(),
                signature: Some("hidden".into()),
                encrypted: None,
                redacted: true,
                extensions: Default::default(),
            },
            Block::ToolUse {
                id: "call".into(),
                name: "Read".into(),
                input: json!({"n":1.25,"s":"arg"}).into(),
                namespace: None,
                extensions: Default::default(),
            },
            Block::ToolUse {
                id: "source-call".into(),
                name: "Read".into(),
                input: source_json,
                namespace: None,
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: "result".into(),
                tool_name: None,
                is_error: true,
                status: None,
                details: None,
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "span".into(),
                content: span,
                tool_name: None,
                is_error: false,
                status: None,
                details: None,
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "source".into(),
                content: source,
                tool_name: None,
                is_error: false,
                status: None,
                details: None,
                extensions: Default::default(),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("file:image".into()),
                extensions: Default::default(),
            },
            Block::Image {
                media_type: None,
                data_ref: Some("base64:inline".into()),
                extensions: Default::default(),
            },
            Block::Image {
                media_type: Some("image/webp".into()),
                data_ref: Some("https://example.invalid/image".into()),
                extensions: Default::default(),
            },
            Block::Image {
                media_type: None,
                data_ref: None,
                extensions: Default::default(),
            },
            Block::File {
                mime: Some("text/plain".into()),
                path: Some("document λ".into()),
                source: Some("https://example.invalid/doc".into()),
                extensions: Default::default(),
            },
            Block::File {
                mime: None,
                path: None,
                source: Some("file:doc".into()),
                extensions: Default::default(),
            },
            Block::File {
                mime: None,
                path: None,
                source: None,
                extensions: Default::default(),
            },
        ];
        let mut observed = Vec::new();
        for (dialect, q) in [ANTHROPIC, CLAUDE_AI, OPENCLAW, GOOSE, ZED].into_iter().enumerate() {
            for user in [false, true] {
                for (case, b) in content.iter().enumerate() {
                    let owned = super::super::block_to(b, &q, user)?
                        .map(|v| serde_json::to_string(&v))
                        .transpose()?;
                    let mut bytes = Vec::new();
                    let (checked, error) = match block(&mut bytes, b, &q, user, &resolver) {
                        Ok(()) => (Some(String::from_utf8(bytes)?), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    observed.push(json!({"dialect":dialect,"user":user,"case":case,"owned":owned,"checked":checked,"error":error}));
                }
            }
        }
        let expected: Vec<Value> = include_str!("../../../../tests/fixtures/wire/anthropic-blocks.jsonl")
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(observed, expected);

        Ok(())
    }
}

#[cfg(test)]
mod source_failure_tests {
    use super::*;
    use crate::harness::store::JsonlStage;
    #[test]
    fn changed_source_during_block_serialization_cannot_publish() -> Result<()> {
        let world = cv_testkit::World::new("anthropic-stage-source-race");
        let source = world.base.join("source.json");
        let destination = world.base.join("destination.jsonl");
        let original = serde_json::to_vec(&"a".repeat(200_000))?;
        let changed = serde_json::to_vec(&"b".repeat(200_000))?;
        std::fs::write(&source, &original)?;
        std::fs::write(&destination, b"existing destination")?;
        let text = crate::Json::source(&source, 0, original.len() as u64)?
            .with_view(|v| v.source_text()?.ok_or_else(|| anyhow::anyhow!("source text")))?;
        struct Mutate<'a> {
            writer: &'a mut dyn Write,
            source: &'a std::path::Path,
            bytes: &'a [u8],
            done: bool,
        }
        impl Write for Mutate<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.done && bytes.len() > 1000 {
                    self.done = true;
                    let mut file = std::fs::OpenOptions::new().write(true).open(self.source)?;
                    file.write_all(self.bytes)?;
                }
                self.writer.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.writer.flush()
            }
        }
        let mut stage = JsonlStage::new();
        assert!(stage
            .push_with(|writer| block(
                &mut Mutate {
                    writer,
                    source: &source,
                    bytes: &changed,
                    done: false
                },
                &Block::from_text(text),
                &ANTHROPIC,
                false,
                &Resolver::new(None)
            ))
            .is_err());
        assert!(stage.publish(&destination, crate::publish::Claim::Replace).is_err());
        assert_eq!(std::fs::read(destination)?, b"existing destination");
        Ok(())
    }
}
