//! GenAI Part grammar shared by Qwen and Gemini. Store wrappers, tool identities and
//! leaf demand remain with their consumers; only payload qualification differs by dialect.
use super::write::{self, ObjectWriter};
use crate::harness::store::document::Fields;
use crate::{json::JsonView, lazy::Resolver, Block, Text};
use anyhow::Result;

#[derive(Clone, Copy)]
pub(crate) enum Dialect {
    Qwen,
    Gemini,
}
pub(crate) enum Part<'a> {
    Call(JsonView<'a>),
    Response(JsonView<'a>),
    Media(Block),
    Text {
        value: JsonView<'a>,
        thought: bool,
        signature: Option<JsonView<'a>>,
    },
}
pub(crate) struct Selected<'a> {
    pub part: Option<Part<'a>>,
    /// Native Gemini turn classification depends on presence even when another field wins.
    pub response_present: bool,
}
fn payload<'a>(fields: &Fields<'a>, key: &str, dialect: Dialect) -> Result<Option<JsonView<'a>>> {
    match fields.get(key) {
        Some(value) if matches!(dialect, Dialect::Qwen) || value.is_object()? => Ok(Some(value)),
        _ => Ok(None),
    }
}
fn string(value: JsonView<'_>, key: &str) -> Result<Option<String>> {
    let Some(value) = value.get(key)? else { return Ok(None) };
    if !value.is_string()? {
        return Ok(None);
    }
    Ok(value.resolve()?.as_str().map(str::to_owned))
}
pub(crate) fn select_with_images(view: JsonView<'_>, dialect: Dialect, keep_image_data: bool) -> Result<Selected<'_>> {
    let Some(fields) = Fields::parse(
        view,
        &[
            "functionCall",
            "functionResponse",
            "inlineData",
            "fileData",
            "text",
            "thought",
            "thoughtSignature",
        ],
    )?
    else {
        return Ok(Selected {
            part: None,
            response_present: false,
        });
    };
    let part = if let Some(value) = payload(&fields, "functionCall", dialect)? {
        Some(Part::Call(value))
    } else if let Some(value) = payload(&fields, "functionResponse", dialect)? {
        Some(Part::Response(value))
    } else if let Some(value) = payload(&fields, "inlineData", dialect)? {
        let media_type = string(value, "mimeType")?;
        let data_ref = if keep_image_data {
            string(value, "data")?.map(|b64| {
                let mime = media_type.as_deref().unwrap_or("application/octet-stream");
                format!("data:{mime};base64,{b64}")
            })
        } else {
            None
        };
        Some(Part::Media(Block::Image { media_type, data_ref }))
    } else if let Some(value) = payload(&fields, "fileData", dialect)? {
        Some(Part::Media(Block::File {
            mime: string(value, "mimeType")?,
            path: None,
            source: string(value, "fileUri")?,
        }))
    } else if let Some(value) = fields.get("text") {
        Some(Part::Text {
            value,
            thought: fields.get("thought").map(JsonView::as_bool).transpose()?.flatten() == Some(true),
            signature: fields.get("thoughtSignature"),
        })
    } else {
        None
    };
    Ok(Selected {
        part,
        response_present: fields.get("functionResponse").is_some(),
    })
}
pub(crate) fn text_block(text: Text, thought: bool, signature: Option<String>) -> Block {
    if thought {
        Block::Thinking {
            text,
            signature,
            encrypted: None,
            redacted: false,
        }
    } else {
        Block::Text { text }
    }
}
/// Write an already-selected text part; the consumer chooses thought and signature publication.
pub(crate) fn text_fields(
    object: &mut ObjectWriter<'_>,
    text: &Text,
    thought: bool,
    signature: Option<&str>,
    resolver: &Resolver,
) -> Result<()> {
    object.field_with("text", |w| write::text(w, text, resolver))?;
    if thought {
        object.field("thought", true)?;
    }
    if let Some(signature) = signature {
        object.field("thoughtSignature", signature)?;
    }
    Ok(())
}
pub(crate) fn image_fields(
    object: &mut ObjectWriter<'_>,
    media_type: Option<&str>,
    data_ref: Option<&str>,
) -> Result<()> {
    if let Some((uri_mime, b64)) = data_ref
        .and_then(|r| r.strip_prefix("data:"))
        .and_then(|rest| rest.split_once(";base64,"))
    {
        let mime = if !uri_mime.is_empty() {
            Some(uri_mime)
        } else {
            media_type.filter(|m| !m.is_empty())
        };
        return object.field_with("inlineData", |w| {
            let mut data = ObjectWriter::new(w)?;
            if let Some(mime) = mime {
                data.field("mimeType", mime)?;
            }
            data.field("data", b64)?;
            data.finish()
        });
    }
    let label = data_ref
        .filter(|s| !s.is_empty())
        .or_else(|| media_type.filter(|s| !s.is_empty()));
    match label {
        Some(label) => object.field("text", format!("[image: {label}]")),
        None => object.field("text", "[image]"),
    }
}
pub(crate) fn file_fields(
    object: &mut ObjectWriter<'_>,
    mime: Option<&str>,
    path: Option<&str>,
    source: Option<&str>,
) -> Result<()> {
    object.field_with("fileData", |w| {
        let mut data = ObjectWriter::new(w)?;
        if let Some(mime) = mime {
            data.field("mimeType", mime)?;
        }
        if let Some(uri) = source.or(path) {
            data.field("fileUri", uri)?;
        }
        data.finish()
    })
}
