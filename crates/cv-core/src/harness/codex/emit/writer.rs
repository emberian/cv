//! Codex's text and argument fields written directly into the shared JSONL stage.
use crate::harness::{
    store::{text, JsonlStage},
    wire::write::{append_text, text as write_text},
};
use crate::{emit::ThinkingMode, lazy::Resolver, Harness, Json, Message, Text};
use anyhow::Result;
use std::io::Write;

/// The one Codex envelope, `{"type":kind,"timestamp":…,"payload":…}`, with the payload written
/// in place.
pub(super) fn record(
    lines: &mut JsonlStage,
    timestamp: &str,
    kind: &str,
    payload: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    lines.push_with(|writer| {
        writer.write_all(b"{\"type\":")?;
        serde_json::to_writer(&mut *writer, kind)?;
        writer.write_all(b",\"timestamp\":")?;
        serde_json::to_writer(&mut *writer, timestamp)?;
        writer.write_all(b",\"payload\":")?;
        payload(writer)?;
        writer.write_all(b"}")?;
        Ok(())
    })
}
/// [`record`] with an owned payload value.
pub(super) fn value(lines: &mut JsonlStage, timestamp: &str, kind: &str, payload: &serde_json::Value) -> Result<()> {
    record(lines, timestamp, kind, |writer| {
        Ok(serde_json::to_writer(writer, payload)?)
    })
}
pub(super) fn message_string(
    writer: &mut dyn Write,
    message: &Message,
    resolver: &Resolver,
    mode: ThinkingMode,
) -> Result<()> {
    text::generate_string(writer, |builder| {
        let mut started = false;
        for block in &message.content {
            if let Some(text) = crate::emit::thinking::text(block, Harness::Codex, mode, resolver)? {
                if started {
                    builder.push("\n")?;
                }
                started |= append_text(builder, &text, resolver)?;
            }
        }
        Ok(())
    })
}
pub(super) fn string(writer: &mut dyn Write, value: &Text, resolver: &Resolver, prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return write_text(writer, value, resolver);
    }
    text::generate_string(writer, |builder| {
        builder.push(prefix)?;
        append_text(builder, value, resolver)?;
        Ok(())
    })
}
pub(super) fn argument(writer: &mut dyn Write, value: &Json) -> Result<()> {
    text::generate_string(writer, |builder| value.with_view(|view| builder.push_json(view)))
}

/// Whether `(name, value)` is a Codex freeform `custom_tool_call` (`exec` or `apply_patch`
/// carrying a raw string input, or cross-harness `{"raw": "..."}`).
pub(super) fn is_custom_tool_input(name: &str, value: &Json) -> Result<bool> {
    if !matches!(name, "exec" | "apply_patch") {
        return Ok(false);
    }
    value.with_view(|view| {
        if view.is_string()? {
            return Ok(true);
        }
        if !view.is_object()? {
            return Ok(false);
        }
        let mut count = 0usize;
        let mut raw_str = false;
        view.for_each_entry(|k, v| {
            count += 1;
            if k == "raw" && v.is_string()? {
                raw_str = true;
            }
            Ok(())
        })?;
        Ok(count == 1 && raw_str)
    })
}

/// Write a `custom_tool_call`'s `input` JSON string directly (unwrapping `{"raw": "..."}` when present).
pub(super) fn custom_input(writer: &mut dyn Write, value: &Json) -> Result<()> {
    value.with_view(|view| {
        if view.is_string()? {
            let mut w = &mut *writer;
            return view.write_canonical(&mut w);
        }
        if let Some(raw) = view.get("raw")? {
            if raw.is_string()? {
                let mut w = &mut *writer;
                return raw.write_canonical(&mut w);
            }
        }
        argument(writer, value)
    })
}
#[allow(clippy::too_many_arguments)]
pub(super) fn message(
    lines: &mut JsonlStage,
    timestamp: &str,
    role: &str,
    content_type: &str,
    event_type: Option<&str>,
    message: &Message,
    resolver: &Resolver,
    mode: ThinkingMode,
) -> Result<()> {
    let mut texts = message
        .content
        .iter()
        .filter_map(|block| crate::emit::thinking::text(block, Harness::Codex, mode, resolver).transpose());
    // A twinned turn of one inline text (or none) is written straight from that string.
    let first = texts.next().transpose()?;
    let only = match (&first, texts.next().transpose()?) {
        (Some(value), None) => value.inline_str(),
        (None, None) => Some(""),
        _ => None,
    };
    match only.filter(|_| event_type.is_some()) {
        Some(only) => twins(lines, timestamp, role, content_type, event_type, &|writer| {
            Ok(serde_json::to_writer(writer, only)?)
        }),
        None => twins(lines, timestamp, role, content_type, event_type, &|writer| {
            message_string(writer, message, resolver, mode)
        }),
    }
}
pub(super) fn text(lines: &mut JsonlStage, timestamp: &str, value: &Text, resolver: &Resolver) -> Result<()> {
    twins(
        lines,
        timestamp,
        "assistant",
        "output_text",
        Some("agent_message"),
        &|writer| match value.inline_str() {
            Some(inline) => Ok(serde_json::to_writer(writer, inline)?),
            None => string(writer, value, resolver, ""),
        },
    )
}
/// A text turn as Codex's legacy pair: the `response_item` message and, when `event_type` names
/// one, its `event_msg` twin, both carrying the same `body` string.
fn twins(
    lines: &mut JsonlStage,
    timestamp: &str,
    role: &str,
    content_type: &str,
    event_type: Option<&str>,
    body: &impl Fn(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    record(lines, timestamp, "response_item", |writer| {
        writer.write_all(b"{\"type\":\"message\",\"role\":")?;
        serde_json::to_writer(&mut *writer, role)?;
        writer.write_all(b",\"content\":[{\"type\":")?;
        serde_json::to_writer(&mut *writer, content_type)?;
        writer.write_all(b",\"text\":")?;
        body(writer)?;
        writer.write_all(b"}]}")?;
        Ok(())
    })?;
    if let Some(event_type) = event_type {
        record(lines, timestamp, "event_msg", |writer| {
            writer.write_all(b"{\"type\":")?;
            serde_json::to_writer(&mut *writer, event_type)?;
            writer.write_all(b",\"message\":")?;
            body(writer)?;
            writer.write_all(b"}")?;
            Ok(())
        })?;
    }
    Ok(())
}
