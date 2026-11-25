//! The Continue history-item codec: one item view in, its message and tool results out. Only the
//! fields an output reads are selected; unrelated history and context payloads are skipped.
use super::*;

pub(super) fn reply_id(item: JsonView<'_>) -> Result<Option<String>> {
    let Some(item) = Fields::parse(item, &["message"])? else {
        return Ok(None);
    };
    let Some(message) = item
        .get("message")
        .map(|v| Fields::parse(v, &["toolCallId"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    message.string("toolCallId")
}

#[cfg(test)]
pub(super) fn has_sources(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(
            block,
            Block::ToolUse {
                input: crate::Json::Source(_),
                ..
            }
        )
    })
}

/// One `history` item: its message (`None` for an empty or message-less item) and the tool
/// results its `toolCallStates` carry. Every byte source feeds this one function; `spans` keeps
/// large strings in the source, never chooses the code. `model` is filled opportunistically from
/// the first `promptLogs[].modelTitle`/`modelProvider`, or from usage.
pub(super) fn messages(
    item: JsonView<'_>,
    spans: bool,
    model: &mut Option<String>,
) -> Result<(Option<Message>, Option<Message>)> {
    let Some(item) = Fields::parse(item, &["message", "contextItems", "promptLogs", "toolCallStates"])? else {
        return Ok((None, None));
    };
    let results = tool_states(item.get("toolCallStates"), spans)?;
    let Some(message_view) = item.get("message") else {
        return Ok((None, results));
    };
    let message = Fields::parse(message_view, &["role", "toolCallId", "usage", "content", "toolCalls"])?;
    let role_str = message.as_ref().map(|m| m.string("role")).transpose()?.flatten();
    let role = match role_str.as_deref().unwrap_or("user") {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        // Missing and unknown native roles use the user fallback; this is a lossy
        // classification, since the unrecognized role string is not retained here.
        _ => Role::User,
    };
    let mut m = Message::new(role);
    // A `system` role history item carries the system prompt.
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }

    // Opportunistically capture the model from promptLogs (assistant turns).
    if model.is_none() {
        if let Some(mut logs) = item.get("promptLogs").map(JsonView::array).transpose()?.flatten() {
            if let Some(log) = logs.next() {
                if let Some(log) = Fields::parse(log?, &["modelTitle", "modelProvider"])? {
                    let title = if log.get("modelTitle").is_some() {
                        log.string("modelTitle")?
                    } else {
                        log.string("modelProvider")?
                    };
                    if let Some(title) = title.filter(|s| !s.is_empty()) {
                        *model = Some(title);
                    }
                }
            }
        }
    }

    if let Some(message) = &message {
        if role == Role::Tool {
            // A tool result. Continue stores the output in `content`.
            m.content.push(Block::ToolResult {
                tool_use_id: message.string("toolCallId")?.unwrap_or_default(),
                content: joined_content(message.get("content"), spans)?,
                is_error: false,
                tool_name: None,
                status: Some("completed".into()),
                details: None,
                extensions: Default::default(),
            });
        } else {
            m.content = openai::blocks_from_view(message.get("content"), &openai::CONTINUE, &mut |v| v.text(spans))?;
            if role == Role::Assistant {
                if let Some(calls) = message.get("toolCalls").map(JsonView::array).transpose()?.flatten() {
                    for call in calls {
                        m.content.push(openai::tool_call_from_view(call?, spans)?);
                    }
                }
            }
        }
    }

    // Attached context items (files / snippets / docs) → File blocks (best-effort).
    if let Some(context) = item.get("contextItems").map(JsonView::array).transpose()?.flatten() {
        for context in context {
            if let Some(block) = context_item(context?)? {
                m.content.push(block);
            }
        }
    }

    // Per-request usage on assistant turns (`message.usage`, OpenAI's names): `prompt_tokens`
    // includes the cached part, which the IR keeps disjoint.
    if let Some(usage) = message.as_ref().and_then(|m| m.get("usage")) {
        if let Some(u) = Fields::parse(
            usage,
            &[
                "prompt_tokens",
                "completion_tokens",
                "prompt_tokens_details",
                "completion_tokens_details",
                "cost_cents",
                "model",
            ],
        )? {
            let number = |v: Option<JsonView<'_>>| -> Result<Option<serde_json::Number>> {
                Ok(v.map(JsonView::as_number).transpose()?.flatten())
            };
            let nested = |key: &str, inner: &str| -> Result<Option<u64>> {
                let v = u.get(key).map(|v| v.get(inner)).transpose()?.flatten();
                Ok(number(v)?.and_then(|n| n.as_u64()))
            };
            let cached = nested("prompt_tokens_details", "cached_tokens")?;
            m.usage = Some(Usage {
                input_tokens: number(u.get("prompt_tokens"))?
                    .and_then(|n| n.as_u64())
                    .map(|p| p.saturating_sub(cached.unwrap_or(0))),
                output_tokens: number(u.get("completion_tokens"))?.and_then(|n| n.as_u64()),
                cache_read_tokens: cached,
                reasoning_tokens: nested("completion_tokens_details", "reasoning_tokens")?,
                cost_usd: number(u.get("cost_cents"))?.and_then(|n| n.as_f64()).map(|c| c / 100.0),
                ..Usage::default()
            });
            m.model = u.string("model")?;
            if model.is_none() {
                model.clone_from(&m.model);
            }
        }
    }

    Ok(((!m.content.is_empty()).then_some(m), results))
}

/// A `contextItem` → a `File` block (best-effort). The path/source is taken from `uri.value` (a file
/// path or URL) and falls back to `description`/`name`.
fn context_item(item: JsonView<'_>) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(item, &["name", "description", "uri"])? else {
        return Ok(None);
    };
    let uri = item
        .get("uri")
        .map(|v| Fields::parse(v, &["type", "value"]))
        .transpose()?
        .flatten();
    let uri_val = uri
        .as_ref()
        .map(|u| u.string("value"))
        .transpose()?
        .flatten()
        .filter(|s| !s.is_empty());
    let uri_type = uri.as_ref().map(|u| u.string("type")).transpose()?.flatten();
    let desc = item.string("description")?.filter(|s| !s.is_empty());
    let name = item.string("name")?.filter(|s| !s.is_empty());
    // Nothing identifiable → skip.
    if uri_val.is_none() && desc.is_none() && name.is_none() {
        return Ok(None);
    }
    // `uri.type == "file"` → a filesystem path; otherwise treat the value as an opaque source/URL.
    let (path, source) = match (uri_val, uri_type.as_deref()) {
        (Some(v), Some("file")) => (Some(v), None),
        (Some(v), _) => (None, Some(v)),
        (None, _) => (None, desc.or(name)),
    };
    Ok(Some(Block::File {
        mime: None,
        path,
        source,
        extensions: Default::default(),
    }))
}

/// A string by `text(spans)`; parts' `text` joined, owned (a single part stays a range).
fn joined_content(content: Option<JsonView<'_>>, spans: bool) -> Result<crate::Text> {
    use crate::harness::wire;
    wire::joined(content, wire::TEXT, wire::Backing::Inline, &mut |v| v.text(spans))
}
fn tool_states(states: Option<JsonView<'_>>, spans: bool) -> Result<Option<Message>> {
    let Some(states) = states.map(JsonView::array).transpose()?.flatten() else {
        return Ok(None);
    };
    let mut message = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
    for state in states {
        let Some(state) = Fields::parse(state?, &["output", "status", "toolCallId", "toolCall"])? else {
            continue;
        };
        let Some(output) = state.get("output") else {
            continue;
        };
        if !output.is_array()? {
            continue;
        }
        let content = crate::harness::wire::joined(
            Some(output),
            crate::harness::wire::Parts {
                text: "content",
                image: None,
            },
            crate::harness::wire::Backing::Inline,
            &mut |v| v.text(spans),
        )?;
        let status = state.string("status")?;
        let call = state
            .get("toolCall")
            .map(|v| Fields::parse(v, &["function"]))
            .transpose()?
            .flatten();
        let function = call
            .and_then(|c| c.get("function"))
            .map(|v| Fields::parse(v, &["name"]))
            .transpose()?
            .flatten();
        message.content.push(Block::ToolResult {
            tool_use_id: state.string("toolCallId")?.unwrap_or_default(),
            content,
            is_error: status.as_deref() == Some("errored"),
            tool_name: function.map(|f| f.string("name")).transpose()?.flatten(),
            status,
            details: None,
            extensions: Default::default(),
        });
    }
    Ok((!message.content.is_empty()).then_some(message))
}

/// Titles retain only the published prefix, while checking nonempty whitespace over the full text.
pub(super) fn title(view: Option<JsonView<'_>>, strict: bool) -> Result<Option<String>> {
    let Some(view) = view else { return Ok(None) };
    if view.is_null()? {
        return Ok(None);
    }
    if !view.is_string()? {
        return if strict {
            Err(InvalidStruct("expected optional title string").into())
        } else {
            Ok(None)
        };
    }
    if strict && !view.accepts_value()? {
        return Err(InvalidStruct("invalid title string").into());
    }
    let mut head = crate::projection::Head::bounded(crate::projection::Head::TITLE);
    view.for_each_str_chunk(|chunk| {
        head.push(chunk);
        Ok(())
    })?;
    Ok((!head.is_blank()).then(|| head.finish()))
}
