//! The Goose content codec: one validated `content_json` / `metadata_json` view in (a SQLite cell
//! from [`Cells`](crate::harness::store::sqlite::Cells), or a legacy `.jsonl` record's `content`),
//! decoded blocks and metadata facts out. The demand decides what is resolved, never which code
//! runs: `spans` keeps long text and arguments in the source, full extra resolves the native
//! sidecars, and a read with no [`Out`] (discovery's turn count) builds no composed text at all.
use super::*;
use crate::harness::store::{bytes::ByteSource, document::Fields, text};
use crate::json::{JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::lazy::Text;
use crate::stream::Tier;
use std::sync::Arc;

/// Where composed text (control notices, flattened tool results) is built: the read's source and
/// its arena. None for a read that only classifies rows.
pub(super) type Out<'a> = Option<(&'a JsonSource, &'a Arc<ByteSource>)>;

/// Text composed by `produce`: in the arena when spans are demanded, inline otherwise, nothing
/// without an [`Out`].
fn composed(out: Out<'_>, spans: bool, produce: impl FnOnce(&mut text::Builder<'_>) -> Result<()>) -> Result<Text> {
    let Some((source, arena)) = out else {
        return Ok(Text::default());
    };
    if spans {
        return text::append(source, arena, produce);
    }
    let mut quoted = Vec::new();
    text::generate_string(&mut quoted, produce)?;
    Ok(Text::Inline(serde_json::from_slice::<String>(&quoted)?))
}

/// The `metadata_json` facts a message reads: the whole object on full extra demand, else the
/// visibility flags, usage counters and inference identity.
pub(super) fn metadata(view: JsonView<'_>, rich: bool) -> Result<Value> {
    if rich {
        return Ok(view.resolve()?.into_owned());
    }
    // Each selection's keys, named once and reused by the emit loops below.
    const FLAGS: &[&str] = &["userVisible", "agentVisible", "turnContext"];
    const USAGE_NUMBERS: &[&str] = &[
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "cost",
    ];
    const INFERENCE: &[&str] = &["provider", "resolvedModel", "requestedModel"];
    let top: Vec<&str> = FLAGS.iter().copied().chain(["usage", "inference"]).collect();
    let Some(fields) = Fields::parse(view, &top)? else {
        return Ok(Value::Null);
    };
    let mut out = serde_json::Map::new();
    for key in FLAGS {
        if let Some(flag) = fields.get(key).map(|v| v.as_bool()).transpose()?.flatten() {
            out.insert((*key).into(), Value::Bool(flag));
        }
    }
    let usage_keys: Vec<&str> = USAGE_NUMBERS.iter().copied().chain(["isCompaction"]).collect();
    if let Some(usage) = fields
        .get("usage")
        .map(|v| Fields::parse(v, &usage_keys))
        .transpose()?
        .flatten()
    {
        let mut out_usage = serde_json::Map::new();
        for key in USAGE_NUMBERS {
            if let Some(number) = usage.get(key).map(|v| v.as_number()).transpose()?.flatten() {
                out_usage.insert((*key).into(), Value::Number(number));
            }
        }
        if let Some(flag) = usage.get("isCompaction").map(|v| v.as_bool()).transpose()?.flatten() {
            out_usage.insert("isCompaction".into(), Value::Bool(flag));
        }
        out.insert("usage".into(), Value::Object(out_usage));
    }
    if let Some(inference) = fields
        .get("inference")
        .map(|v| Fields::parse(v, INFERENCE))
        .transpose()?
        .flatten()
    {
        let mut out_inference = serde_json::Map::new();
        for key in INFERENCE {
            // A present non-string resolvedModel still shadows requestedModel.
            if inference.get(key).is_some() {
                out_inference.insert(
                    (*key).into(),
                    inference.string(key)?.map(Value::String).unwrap_or(Value::Null),
                );
            }
        }
        out.insert("inference".into(), Value::Object(out_inference));
    }
    Ok(Value::Object(out))
}

/// One message's `Vec<MessageContent>`, recording tool-request names in `tools`. The tool
/// request/response pair is Goose's own (rmcp) shape; `error` and the control blocks render as
/// Goose's own `Display` prints them (message.rs:352-373); everything else is the Anthropic shape
/// under Goose's keys. A bare string (defensive; Goose stores arrays) is one text block.
pub(super) fn content(
    view: JsonView<'_>,
    tools: &mut HashMap<String, String>,
    opts: &Demand,
    out: Out<'_>,
) -> Result<Decoded> {
    let spans = opts.spans;
    let mut d = Decoded::default();
    let Some(items) = view.array()? else {
        if let Some(text) = view.lazy_text(spans)? {
            if !matches!(&text, Text::Inline(s) if s.is_empty()) {
                d.blocks.push(Block::from_text(text));
                d.substantive = true;
            }
        }
        return Ok(d);
    };
    for item in items {
        let item = item?;
        let fields = Fields::parse(item, &["type"])?;
        let ty = fields.as_ref().map(|f| f.string("type")).transpose()?.flatten();
        match ty.as_deref() {
            Some("error" | "systemNotification" | "actionRequired" | "toolConfirmationRequest") => {
                control(item, ty.as_deref(), &mut d, opts, out)?;
            }
            Some("toolRequest" | "frontendToolRequest" | "toolResponse") => {
                if let Some(block) = tool(item, tools, opts, out)? {
                    d.blocks.push(block);
                    d.substantive = true;
                }
            }
            Some(_) => {
                let joined_over = crate::harness::wire::joined_over(spans);
                let block = anthropic::block_from_view_with(
                    item,
                    &anthropic::GOOSE,
                    spans,
                    &mut |v| v.lazy_text(spans),
                    joined_over,
                )?;
                if let Some(block) = block {
                    d.blocks.push(block);
                    d.substantive = true;
                }
            }
            None => {}
        }
    }
    Ok(d)
}

/// `ErrorContent {kind, message}` (message.rs:284-287; written by `Message::with_error` when a
/// provider call gives up) makes the message an Error; the control blocks are kept verbatim on
/// full extra demand. Both render as text.
fn control(item: JsonView<'_>, ty: Option<&str>, d: &mut Decoded, opts: &Demand, out: Out<'_>) -> Result<()> {
    let fields = Fields::parse(
        item,
        &["kind", "message", "msg", "toolName", "data", "actionType", "id"],
    )?
    .context("Goose control record must be an object with a type")?;
    let (prefix, payload, suffix) = match ty {
        Some("error") => (
            format!("[error: {}] ", fields.string("kind")?.unwrap_or_else(|| "other".into())),
            fields.get("message"),
            "",
        ),
        Some("systemNotification") => ("[SystemNotification: ".into(), fields.get("msg"), "]"),
        Some("toolConfirmationRequest") => ("[ToolConfirmationRequest: ".into(), fields.get("toolName"), "]"),
        _ => {
            let data = fields.get("data").unwrap_or(item);
            let data = Fields::parse(data, &["actionType", "toolName", "message", "id"])?;
            let action = data.as_ref().map(|f| f.string("actionType")).transpose()?.flatten();
            let (prefix, key) = match action.as_deref() {
                Some("toolConfirmation") => ("[ActionRequired: ToolConfirmation for ".into(), Some("toolName")),
                Some("elicitation") => ("[ActionRequired: Elicitation - ".into(), Some("message")),
                Some("elicitationResponse") => ("[ActionRequired: ElicitationResponse for ".into(), Some("id")),
                Some("toolConfirmationResponse") => {
                    ("[ActionRequired: ToolConfirmationResponse for ".into(), Some("id"))
                }
                other => (format!("[ActionRequired: {}", other.unwrap_or("?")), None),
            };
            (prefix, key.and_then(|k| data.as_ref().and_then(|f| f.get(k))), "]")
        }
    };
    let text = composed(out, opts.spans, |b| {
        b.push(&prefix)?;
        if let Some(v) = payload {
            b.push_string(v)?;
        }
        b.push(suffix)
    })?;
    d.blocks.push(Block::from_text(text));
    if ty == Some("error") {
        if d.error.is_none() {
            d.error = Some(if opts.tier < Tier::Rich {
                Value::Object(Default::default())
            } else {
                serde_json::json!({ "kind": fields.string("kind")?.unwrap_or_else(|| "other".into()), "message": fields.string("message")?.unwrap_or_default() })
            });
        }
    } else if opts.tier >= Tier::Rich {
        d.control.push(item.resolve()?.into_owned());
    } else {
        d.control.push(Value::Null);
    }
    Ok(())
}

/// A `toolRequest` is the assistant's call (rmcp `CallToolRequestParams` = `{name, arguments}`;
/// the error wrapper `{"status":"error","error":"…"}` is a degenerate call); a `toolResponse` is
/// the result (rmcp `CallToolResult` = `{content:[Content…], isError}`, or the bare content array
/// of the legacy `SuccessWithContentVec`, tool_result_serde.rs:132-146). rmcp's
/// `structuredContent` and `_meta` (summon's `delegate` puts the child's `subagent_session_id`
/// there) ride in `details` on full extra demand.
fn tool(view: JsonView<'_>, tools: &mut HashMap<String, String>, opts: &Demand, out: Out<'_>) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(view, &["type", "id", "toolCall", "toolResult"])? else {
        return Ok(None);
    };
    let id = item.string("id")?.unwrap_or_default();
    if item.string("type")?.as_deref() != Some("toolResponse") {
        let Some(call) = item
            .get("toolCall")
            .map(|v| Fields::parse(v, &["status", "error", "value"]))
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        if call.string("status")?.as_deref() == Some("error") {
            let error = string_view(call.get("error"))?;
            let input = match out {
                Some((source, arena)) => {
                    let (offset, len) = arena.append(|mut writer| {
                        writer.write_all(b"{\"error\":")?;
                        match error {
                            Some(v) => v.write_canonical(&mut writer)?,
                            None => writer.write_all(b"\"\"")?,
                        }
                        writer.write_all(b"}")?;
                        Ok(())
                    })?;
                    source.read(ReadInput::Backing(arena.clone(), offset, len), Validation::Value, |v| {
                        v.argument(opts.spans)
                    })?
                }
                None => crate::json::Json::default(),
            };
            return Ok(Some(Block::ToolUse {
                id,
                name: String::new(),
                input,
                namespace: None,
                extensions: Default::default(),
            }));
        }
        let Some(value_view) = call.get("value") else {
            return Ok(None);
        };
        let value = Fields::parse(value_view, &["name", "arguments"])?;
        let name = value
            .as_ref()
            .map(|f| f.string("name"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let input = value
            .as_ref()
            .and_then(|f| f.get("arguments"))
            .map(|v| v.argument(opts.spans))
            .transpose()?
            .unwrap_or_default();
        if !id.is_empty() && !name.is_empty() {
            tools.insert(id.clone(), name.clone());
        }
        return Ok(Some(Block::ToolUse {
            id,
            name,
            input,
            namespace: None,
            extensions: Default::default(),
        }));
    }
    let Some(result_view) = item.get("toolResult") else {
        return Ok(None);
    };
    let result = Fields::parse(result_view, &["status", "error", "value"])?;
    let value = result.as_ref().and_then(|f| f.get("value"));
    let fields = value
        .map(|v| Fields::parse(v, &["content", "isError", "structuredContent", "_meta"]))
        .transpose()?
        .flatten();
    let mut details = serde_json::Map::new();
    if opts.tier >= Tier::Rich {
        if let Some(fields) = &fields {
            for key in ["structuredContent", "_meta"] {
                if let Some(v) = fields.value(key)? {
                    details.insert(key.into(), v);
                }
            }
        }
    }
    let error = result
        .as_ref()
        .map(|f| f.string("status"))
        .transpose()?
        .flatten()
        .as_deref()
        == Some("error");
    let content = if error {
        string_view(result.as_ref().and_then(|f| f.get("error")))?
            .map(|v| v.lazy_text(opts.spans))
            .transpose()?
            .flatten()
            .unwrap_or_default()
    } else {
        let content = if value.map(|v| v.is_array()).transpose()?.unwrap_or(false) {
            value
        } else {
            fields.as_ref().and_then(|f| f.get("content"))
        };
        match content {
            Some(v) => flatten_result(v, opts.spans, out)?,
            None => Text::default(),
        }
    };
    let is_error = error
        || fields
            .as_ref()
            .and_then(|f| f.get("isError"))
            .map(|v| v.as_bool())
            .transpose()?
            .flatten()
            .unwrap_or(false);
    Ok(Some(Block::ToolResult {
        tool_use_id: id.clone(),
        content,
        is_error,
        tool_name: tools.get(&id).cloned(),
        status: None,
        details: (!details.is_empty()).then_some(Value::Object(details)),
        extensions: Default::default(),
    }))
}

/// An rmcp `CallToolResult.content` array as text. rmcp 3.x content blocks are
/// `text | image | audio | resource | resource_link` (snake_case): embedded resources contribute
/// their text (or a `[blob: <mime>]` marker), links their uri.
fn flatten_result(view: JsonView<'_>, spans: bool, out: Out<'_>) -> Result<Text> {
    let Some(items) = view.array()? else {
        return Ok(view.lazy_text(spans)?.unwrap_or_default());
    };
    composed(out, spans, |builder| {
        let mut first = true;
        for item in items {
            let item = item?;
            let Some(fields) = Fields::parse(item, &["type", "text", "mimeType", "resource", "uri"])? else {
                continue;
            };
            let ty = fields.string("type")?;
            let (text, marker) = match ty.as_deref() {
                Some("text") => (string_view(fields.get("text"))?, None),
                Some(kind @ ("image" | "audio")) => (
                    None,
                    Some(format!(
                        "[{kind}: {}]",
                        fields.string("mimeType")?.unwrap_or_else(|| kind.to_owned())
                    )),
                ),
                Some("resource_link") => (
                    None,
                    Some(format!(
                        "[resource: {}]",
                        fields.string("uri")?.unwrap_or_else(|| "?".into())
                    )),
                ),
                Some("resource") => {
                    let resource = fields.get("resource").unwrap_or(item);
                    let resource = Fields::parse(resource, &["text", "mimeType"])?;
                    let text = string_view(resource.as_ref().and_then(|f| f.get("text")))?;
                    let marker = if text.is_none() {
                        Some(format!(
                            "[blob: {}]",
                            resource
                                .as_ref()
                                .map(|f| f.string("mimeType"))
                                .transpose()?
                                .flatten()
                                .unwrap_or_else(|| "binary".into())
                        ))
                    } else {
                        None
                    };
                    (text, marker)
                }
                _ => continue,
            };
            if text.is_none() && marker.is_none() {
                continue;
            }
            if !first {
                builder.push("\n")?;
            }
            first = false;
            if let Some(text) = text {
                builder.push_string(text)?;
            }
            if let Some(marker) = marker {
                builder.push(&marker)?;
            }
        }
        Ok(())
    })
}

fn string_view(view: Option<JsonView<'_>>) -> Result<Option<JsonView<'_>>> {
    match view {
        Some(view) if view.is_string()? => Ok(Some(view)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(content: &Value, opts: &Demand) -> Decoded {
        content_of(content, &mut HashMap::new(), opts)
    }
    fn content_of(content: &Value, tools: &mut HashMap<String, String>, opts: &Demand) -> Decoded {
        super::content(content.into(), tools, opts, None).unwrap()
    }

    #[test]
    fn an_unread_text_demand_classifies_like_the_stream() {
        // Discovery decodes without an Out: composed text is empty, every block and fact remains.
        let content = serde_json::json!([
            {"type":"systemNotification","msg":"m"},
            {"type":"toolResponse","id":"t","toolResult":{"value":[{"type":"text","text":"a"},{"type":"audio"}]}},
            {"type":"error","kind":"other","message":"e"}
        ]);
        let d = decode(&content, &Demand::LEAN.with_spans());
        assert_eq!(d.blocks.len(), 3);
        assert!(d.substantive && d.error.is_some() && d.control.len() == 1);
        assert!(d.blocks.iter().all(|b| match b {
            Block::Text { text, .. } | Block::ToolResult { content: text, .. } => text.is_empty(),
            _ => false,
        }));
    }
}
