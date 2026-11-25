//! Cursor IDE composer and bubble cells: one decoder over [`Cells`] views for discovery and every
//! demand. `spans` picks how long text and arguments are held, never which code runs.
use super::*;
use crate::harness::store::{document::Fields, sqlite::Cells, text};
use crate::json::{Json, JsonArray, JsonView};
use crate::lazy::Text;

/// A composer's session metadata (the fields [`global_session`] reads) and its authoritative
/// conversation: `fullConversationHeadersOnly` pointers when that is an array, else `conversation`.
pub(super) struct Composer<'a> {
    pub metadata: Value,
    pub rows: Option<JsonArray<'a>>,
    /// Whether `rows` are `{bubbleId}` pointers to external bubble cells.
    pub external: bool,
}

pub(super) fn composer(view: JsonView<'_>) -> Result<Composer<'_>> {
    let fields = Fields::parse(
        view,
        &[
            "name",
            "createdAt",
            "lastUpdatedAt",
            "subagentInfo",
            "unifiedMode",
            "modelConfig",
            "fullConversationHeadersOnly",
            "conversation",
        ],
    )?;
    let Some(fields) = fields else {
        return Ok(Composer {
            metadata: Value::Null,
            rows: None,
            external: false,
        });
    };
    let mut metadata = Map::new();
    if let Some(name) = fields.string("name")? {
        metadata.insert("name".into(), name.into());
    }
    for key in ["createdAt", "lastUpdatedAt", "unifiedMode"] {
        if let Some(value) = fields.get(key) {
            metadata.insert(key.into(), scalar(value)?);
        }
    }
    for (key, names) in [
        (
            "subagentInfo",
            &["parentComposerId", "toolCallId", "subagentTypeName"][..],
        ),
        ("modelConfig", &["modelName"][..]),
    ] {
        if let Some(selected) = fields.get(key).map(|v| Fields::parse(v, names)).transpose()?.flatten() {
            let mut object = Map::new();
            for name in names {
                if let Some(value) = selected.string(name)? {
                    object.insert((*name).into(), value.into());
                }
            }
            metadata.insert(key.into(), Value::Object(object));
        }
    }
    let headers = fields
        .get("fullConversationHeadersOnly")
        .map(|v| v.array())
        .transpose()?
        .flatten();
    let external = headers.is_some();
    let rows = match headers {
        Some(rows) => Some(rows),
        None => fields.get("conversation").map(|v| v.array()).transpose()?.flatten(),
    };
    Ok(Composer {
        metadata: Value::Object(metadata),
        rows,
        external,
    })
}

fn scalar(view: JsonView<'_>) -> Result<Value> {
    if view.is_object()? || view.is_array()? {
        return Ok(Value::Null);
    }
    Ok(view.resolve()?.into_owned())
}

/// Text built from views: kept in the store's arena when `spans` is demanded, else inline.
fn built(cells: &Cells, spans: bool, build: impl FnOnce(&mut text::Builder<'_>) -> Result<()>) -> Result<Text> {
    if spans {
        let (source, arena) = cells.out();
        return text::append(source, arena, build);
    }
    let mut quoted = Vec::new();
    text::generate_string(&mut quoted, build)?;
    Ok(Text::Inline(serde_json::from_slice(&quoted)?))
}

/// A JSON string's decoded contents as a cell of its own, to be read as the document it holds.
/// None for a nonstring or an empty string (malformed as JSON, so ignored).
fn embedded(view: JsonView<'_>, cells: &Cells) -> Result<Option<crate::harness::store::sqlite::Cell>> {
    if !view.is_string()? {
        return Ok(None);
    }
    cells.append(|out| {
        view.for_each_str_chunk(|part| Ok(out.write_all(part.as_bytes())?))?;
        Ok(())
    })
}

/// Cursor accepts any valid JSON document in an encoded argument, unlike OpenAI's object-only
/// arguments. None for a nonstring or invalid JSON.
fn encoded_argument(view: JsonView<'_>, cells: &Cells, spans: bool) -> Result<Option<Json>> {
    let Some(cell) = embedded(view, cells)? else {
        return Ok(None);
    };
    cells.read(cell, |v| v.argument(spans))
}

/// Plain text of a Lexical editor state (`richText`, an object or its JSON encoding): text nodes
/// concatenated, a newline between block children. None when it is not one or is blank.
fn lexical(view: JsonView<'_>, cells: &Cells, spans: bool) -> Result<Option<Text>> {
    fn walk(node: JsonView<'_>, out: &mut text::Builder<'_>) -> Result<bool> {
        let Some(fields) = Fields::parse(node, &["text", "children"])? else {
            return Ok(false);
        };
        if let Some(text) = fields.get("text") {
            out.push_string(text)?;
        }
        let children = fields.get("children");
        if let Some(array) = children.map(|v| v.array()).transpose()?.flatten() {
            let mut array = array.peekable();
            while let Some(child) = array.next() {
                let has_children = walk(child?, out)?;
                if array.peek().is_some() && has_children {
                    out.push("\n")?;
                }
            }
        }
        Ok(children.is_some())
    }
    let build = |view: JsonView<'_>| -> Result<Option<Text>> {
        let Some(root) = view.get("root")? else { return Ok(None) };
        let text = built(cells, spans, |out| walk(root, out).map(|_| ()))?;
        Ok(
            (!crate::harness::store::document::text_is_blank(&text, &crate::lazy::Resolver::new(None))?)
                .then_some(text),
        )
    };
    if !view.is_string()? {
        return build(view);
    }
    match embedded(view, cells)? {
        Some(cell) => Ok(cells.read(cell, build)?.flatten()),
        None => Ok(None),
    }
}

/// A tool result: a string as text, any other non-null value as its compact JSON.
fn result_text(view: JsonView<'_>, cells: &Cells, spans: bool) -> Result<Option<Text>> {
    if view.is_null()? {
        return Ok(None);
    }
    match view.lazy_text(spans)? {
        Some(text) => Ok(Some(text)),
        None => built(cells, spans, |out| out.push_json(view)).map(Some),
    }
}

/// One bubble: `type` 1 is the user, anything else the assistant. None for an empty bubble.
/// `ordinal` is the bubble's place in its composer's conversation: a tool record naming no call id
/// gets one derived from it (`cv:cursor:<ordinal>`; a bubble holds at most one call).
pub(super) fn bubble(view: JsonView<'_>, cells: &Cells, spans: bool, ordinal: usize) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(
        view,
        &[
            "type",
            "bubbleId",
            "createdAt",
            "thinking",
            "text",
            "richText",
            "toolFormerData",
            "modelInfo",
            "modelType",
            "model",
            "tokenCount",
            "unifiedMode",
        ],
    )?
    else {
        return Ok(None);
    };
    let user = fields.get("type").map(scalar).transpose()?.and_then(|v| v.as_i64()) == Some(1);
    let mut message = Message::new(if user { Role::User } else { Role::Assistant });
    message.id = fields.string("bubbleId")?;
    message.timestamp = fields.value("createdAt")?.as_ref().and_then(bubble_timestamp);
    if let Some(thinking) = fields
        .get("thinking")
        .map(|v| Fields::parse(v, &["text", "signature"]))
        .transpose()?
        .flatten()
    {
        let text = thinking
            .get("text")
            .map(|v| v.lazy_text(spans))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let signature = thinking.string("signature")?.filter(|s| !s.is_empty());
        if !text.is_empty() || signature.is_some() {
            message.content.push(Block::Thinking {
                text,
                signature,
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            });
        }
    }
    if let Some(text) = fields
        .get("text")
        .map(|v| v.lazy_text(spans))
        .transpose()?
        .flatten()
        .filter(|text| !text.is_empty())
    {
        message.content.push(Block::from_text(text));
    } else if let Some(rich) = fields.get("richText") {
        if let Some(text) = lexical(rich, cells, spans)? {
            message.content.push(Block::from_text(text));
        }
    }
    if let Some(tool) = fields.get("toolFormerData") {
        // A tool call is emitted even for a non-object toolFormerData.
        let tf = Fields::parse(
            tool,
            &[
                "name",
                "toolCallId",
                "modelCallId",
                "rawArgs",
                "params",
                "status",
                "error",
                "result",
                "additionalData",
            ],
        )?;
        let get = |key| tf.as_ref().and_then(|f| f.get(key));
        let text = |key| get(key).map(|view| view.string()).transpose().map(Option::flatten);
        let name = text("name")?.unwrap_or_else(|| "tool".into());
        let id = match text("toolCallId")? {
            Some(id) => id,
            None => match text("modelCallId")? {
                Some(id) => id,
                None => crate::ir::derived_call_id("cursor", ordinal),
            },
        };
        // The model-issued `rawArgs`, else the normalized `params` (decoded, or as stored).
        let mut input = None;
        if let Some(raw) = get("rawArgs") {
            input = encoded_argument(raw, cells, spans)?;
        }
        if input.is_none() {
            if let Some(params) = get("params") {
                input = Some(match encoded_argument(params, cells, spans)? {
                    Some(input) => input,
                    None => params.argument(spans)?,
                });
            }
        }
        message.content.push(Block::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.unwrap_or_else(|| Value::Null.into()),
            namespace: None,
            extensions: Default::default(),
        });
        let status = text("status")?.unwrap_or_default();
        let error = get("error");
        // A terminal call can complete as an invocation while its command fails: 3.24.12
        // records status=completed and additionalData.status=error.
        let command_failed = get("additionalData")
            .map(|v| v.get("status"))
            .transpose()?
            .flatten()
            .map(|view| view.string())
            .transpose()?
            .flatten()
            .is_some_and(|s| s.eq_ignore_ascii_case("error"));
        let is_error = status.eq_ignore_ascii_case("error")
            || command_failed
            || error.map(|v| v.is_null()).transpose()? == Some(false);
        let mut content = get("result")
            .map(|v| result_text(v, cells, spans))
            .transpose()?
            .flatten();
        if content.is_none() {
            content = error.map(|v| result_text(v, cells, spans)).transpose()?.flatten();
        }
        if let Some(content) = content {
            message.content.push(Block::ToolResult {
                tool_use_id: id,
                content,
                is_error,
                tool_name: Some(name),
                status: (!status.is_empty()).then_some(status),
                details: None,
                extensions: Default::default(),
            });
        }
    }
    // `modelInfo.modelName` (newer) or a bare `modelType`/`model`: the first present wins.
    let model = fields
        .get("modelInfo")
        .map(|v| v.get("modelName"))
        .transpose()?
        .flatten()
        .or(fields.get("modelType"))
        .or(fields.get("model"));
    message.model = model
        .map(|view| view.string())
        .transpose()?
        .flatten()
        .filter(|s| !s.is_empty());
    if let Some(tokens) = fields
        .get("tokenCount")
        .map(|v| Fields::parse(v, &["inputTokens", "outputTokens"]))
        .transpose()?
        .flatten()
    {
        let count = |key| -> Result<Option<u64>> { Ok(tokens.value(key)?.and_then(|v| v.as_u64())) };
        let (input, output) = (count("inputTokens")?, count("outputTokens")?);
        if input.is_some() || output.is_some() {
            message.usage = Some(Usage {
                input_tokens: input,
                output_tokens: output,
                ..Usage::default()
            });
        }
    }
    // A bubble's own `unifiedMode` overrides the composer's (on the session bag) for this message
    // only: a mode switched mid-thread.
    if let Some(mode) = fields.get("unifiedMode") {
        let metadata = unified_mode_metadata(&scalar(mode)?);
        if !metadata.is_empty() {
            message.harness_extra_mut(Harness::Cursor).extend(metadata);
        }
    }
    Ok((!message.content.is_empty() || !message.extra.is_empty()).then_some(message))
}
