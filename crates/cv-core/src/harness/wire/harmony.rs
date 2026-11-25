//! Decode OpenAI **Harmony** channel-formatted text (gpt-oss output) into IR blocks.
//!
//! gpt-oss models emit responses in Harmony's channel format —
//! `<|start|>assistant<|channel|>analysis<|message|>…<|end|>` etc. — where `analysis` is reasoning,
//! `commentary` carries tool/preamble, and `final` is the user-facing answer. Harnesses that log raw
//! gpt-oss output (LM Studio, llama.cpp, …) keep these markers as plain text. This module decodes them
//! into proper IR blocks (analysis → Thinking, commentary/tool → ToolUse, final → Text).
//!
//! Hand-rolled (no heavy `openai-harmony`/tokenizer dep) so cv-core stays lean + wasm-safe.
//!
//! ## Grammar (tolerant subset)
//!
//! A Harmony stream is a sequence of messages. Each message is, in full form:
//!
//! ```text
//! <|start|>{header}<|channel|>{channel} [to={recipient}] [<|constrain|>{type}]<|message|>{body}{stop}
//! ```
//!
//! where `{stop}` ∈ `<|end|>` | `<|return|>` | `<|call|>` (or EOF for a dangling final message).
//! The `<|start|>{header}` part is the role/author (e.g. `assistant`) and is often *absent* in logs
//! that only kept the channel framing; we don't require it. The `to={recipient}` and
//! `<|constrain|>{type}` parts are optional and only appear on tool calls.
//!
//! We parse by scanning for `<|channel|>` / `<|message|>` anchors and slicing the body up to the next
//! stop/`<|start|>`/`<|channel|>` token. Everything is best-effort and never panics: a dangling
//! `<|channel|>foo<|message|>…` with no stop token takes the rest of the string as its body.

use crate::ir::Block;

/// True if `text` looks like it contains Harmony channel markers worth decoding.
pub fn looks_like_harmony(text: &str) -> bool {
    text.contains("<|channel|>") || text.contains("<|message|>") || text.contains("<|start|>assistant")
}

// Harmony control tokens.
const T_START: &str = "<|start|>";
const T_CHANNEL: &str = "<|channel|>";
const T_MESSAGE: &str = "<|message|>";
const T_END: &str = "<|end|>";
const T_RETURN: &str = "<|return|>";
const T_CALL: &str = "<|call|>";

/// One parsed Harmony message header + body.
struct HarmonyMsg<'a> {
    channel: Option<&'a str>,
    recipient: Option<&'a str>,
    body: &'a str,
}

/// Decode Harmony channel-formatted text into IR content blocks. If no markers are present, returns
/// the text as a single [`Block::Text`] (no-op passthrough). Harmony names a call's recipient but
/// carries no call id, so each [`Block::ToolUse`] takes its id from `call_id` (the caller derives
/// one from the call's position, [`crate::ir::derived_call_id`]).
pub fn decode_content(text: &str, call_id: &mut dyn FnMut() -> String) -> Vec<Block> {
    if !looks_like_harmony(text) {
        return vec![Block::from_text(text.to_string().into())];
    }

    let mut blocks = Vec::new();
    for msg in parse_messages(text) {
        match msg.channel {
            Some("analysis") => {
                let body = msg.body.trim();
                if !body.is_empty() {
                    blocks.push(Block::Thinking {
                        text: body.to_string().into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                        extensions: Default::default(),
                    });
                }
            }
            Some("final") => {
                let body = msg.body.trim();
                if !body.is_empty() {
                    blocks.push(Block::from_text(body.to_string().into()));
                }
            }
            // Commentary (preamble/tool calls) and any unknown channel behave alike: a recipient
            // marks a tool call, else the body is plain text so we never silently drop content.
            _ => {
                if let Some(recipient) = msg.recipient {
                    blocks.push(Block::ToolUse {
                        id: call_id(),
                        name: strip_tool_prefix(recipient).to_string(),
                        input: parse_input(msg.body).into(),
                        namespace: None,
                        extensions: Default::default(),
                    });
                } else {
                    let body = msg.body.trim();
                    if !body.is_empty() {
                        blocks.push(Block::from_text(body.to_string().into()));
                    }
                }
            }
        }
    }

    // If parsing produced nothing usable (e.g. only empty bodies), fall back to passthrough so the
    // content is never lost.
    if blocks.is_empty() {
        return vec![Block::from_text(text.to_string().into())];
    }
    blocks
}

/// Walk the text, emitting one [`HarmonyMsg`] per `<|message|>` body found. Tolerant: a `<|message|>`
/// without a preceding `<|channel|>` still yields a channel-less message; a body without a stop token
/// runs to the next structural token (or EOF).
fn parse_messages(text: &str) -> impl Iterator<Item = HarmonyMsg<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= text.len() {
            return None;
        }
        // Find the next message body anchor.
        let msg_rel = text[pos..].find(T_MESSAGE)?;
        let msg_at = pos + msg_rel;

        // The header region is everything between the previous cut (`pos`) and this `<|message|>`.
        // Within it, locate the last `<|channel|>` (the one that owns this message), if any.
        let header = &text[pos..msg_at];
        let (channel, recipient) = parse_header(header);

        // Body starts after the `<|message|>` token.
        let body_start = msg_at + T_MESSAGE.len();

        // Body ends at the earliest of: a stop token, a new `<|start|>`, or a new `<|channel|>`.
        let body_end = earliest_stop(&text[body_start..])
            .map(|(rel, _)| body_start + rel)
            .unwrap_or(text.len());

        // Advance before yielding; no intermediate message roster is retained.
        pos = advance_past_stop(text, body_end);
        Some(HarmonyMsg {
            channel,
            recipient,
            body: &text[body_start..body_end],
        })
    })
}

/// Find the earliest body-terminating token in `s`, returning its byte offset and which token it was.
/// Terminators: `<|end|>`, `<|return|>`, `<|call|>`, and a new `<|start|>` / `<|channel|>` (which
/// begin the next message even without an explicit stop).
fn earliest_stop(s: &str) -> Option<(usize, &'static str)> {
    let mut pos = 0;
    while let Some(relative) = s[pos..].find("<|") {
        pos += relative;
        for token in [T_END, T_RETURN, T_CALL, T_START, T_CHANNEL] {
            if s[pos..].starts_with(token) {
                return Some((pos, token));
            }
        }
        pos += 2;
    }
    None
}

/// Given the offset where a body ended in `text`, return the position to resume header-scanning from.
/// If the body ended on an explicit stop token (`<|end|>`/`<|return|>`/`<|call|>`) we skip past it;
/// if it ended because the next message started (`<|start|>`/`<|channel|>`) we leave that token in
/// place so it's seen as the next header.
fn advance_past_stop(text: &str, body_end: usize) -> usize {
    let rest = &text[body_end..];
    for tok in [T_END, T_RETURN, T_CALL] {
        if rest.starts_with(tok) {
            return body_end + tok.len();
        }
    }
    // Otherwise the cut was at a <|start|> / <|channel|> (or EOF): resume right here.
    if body_end >= text.len() {
        text.len()
    } else {
        body_end
    }
}

/// Parse a header region (the text between a cut point and `<|message|>`) into `(channel, recipient)`.
/// Uses the *last* `<|channel|>` in the region, so a leading `<|start|>assistant` is ignored. The
/// channel token's value runs up to the next control token; a `to=<recipient>` and `<|constrain|>`
/// may follow on the same line.
fn parse_header(header: &str) -> (Option<&str>, Option<&str>) {
    // Locate the channel marker that governs this message (the last one before <|message|>).
    let chan_val = match header.rfind(T_CHANNEL) {
        Some(i) => &header[i + T_CHANNEL.len()..],
        None => {
            // No channel marker. There may still be a recipient (e.g. `... to=functions.x`).
            return (None, extract_recipient(header));
        }
    };

    // The channel name is the first whitespace-delimited word; the rest may hold `to=` / constrain.
    let mut channel: Option<&str> = None;
    let trimmed = chan_val.trim_start();
    if !trimmed.is_empty() {
        let name_end = trimmed
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(trimmed.len());
        let name = &trimmed[..name_end];
        if !name.is_empty() {
            channel = Some(name);
        }
    }

    (channel, extract_recipient(header))
}

/// Extract a tool recipient from a header fragment: the token following `to=`, stopped at whitespace
/// or a `<` (start of the next control token like `<|constrain|>` / `<|message|>`).
fn extract_recipient(frag: &str) -> Option<&str> {
    let idx = frag.find("to=")?;
    let after = &frag[idx + 3..];
    let after = after.trim_start();
    if after.is_empty() {
        return None;
    }
    let end = after
        .find(|c: char| c.is_whitespace() || c == '<')
        .unwrap_or(after.len());
    let recipient = &after[..end];
    if recipient.is_empty() {
        None
    } else {
        Some(recipient)
    }
}

/// Strip a leading namespace like `functions.` from a tool recipient (`functions.get_weather` →
/// `get_weather`). Only the conventional `functions.` prefix is stripped; other namespaces are kept.
fn strip_tool_prefix(recipient: &str) -> &str {
    recipient.strip_prefix("functions.").unwrap_or(recipient)
}

/// Parse a tool-call body into JSON. Harmony bodies are usually a JSON object (sometimes preceded by
/// a `<|constrain|>json` hint, already stripped from the body here). If it isn't valid JSON we keep
/// the raw string as a JSON string value rather than failing.
fn parse_input(body: &str) -> serde_json::Value {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode with numbered call ids, as a caller deriving them from position would.
    fn decode(text: &str) -> Vec<Block> {
        let mut n = 0;
        decode_content(text, &mut || {
            n += 1;
            format!("call-{n}")
        })
    }

    #[test]
    fn recipient_before_channel_is_a_tool_call() {
        let text = "<|start|>assistant to=functions.read<|channel|>commentary<|message|>{\"path\":\"a\"}<|call|>";
        let blocks = decode(text);
        assert!(matches!(&blocks[0], Block::ToolUse { name, .. } if name == "read"));
        if let Block::ToolUse { input, .. } = &blocks[0] {
            assert_eq!(input.resolve().unwrap()["path"], "a");
        }
    }

    #[test]
    fn stop_scan_matches_each_terminator_and_preserves_unknown_controls() {
        for token in [T_END, T_RETURN, T_CALL, T_START, T_CHANNEL] {
            let body = format!("é<|unknown|>body{token}later<|end|>");
            assert_eq!(earliest_stop(&body), Some(("é<|unknown|>body".len(), token)));
        }
        assert_eq!(earliest_stop("é<|unknown|><|message|>body"), None);
        let text = "<|channel|>final<|message|>body<|end|>".repeat(256);
        assert_eq!(parse_messages(&text).count(), 256);
        assert_eq!(decode(&text).len(), 256);
    }

    #[test]
    fn multi_channel_decodes_in_order() {
        let text = concat!(
            "<|start|>assistant<|channel|>analysis<|message|>I should check the weather.<|end|>",
            "<|start|>assistant<|channel|>commentary to=functions.get_weather <|constrain|>json<|message|>{\"location\":\"SF\"}<|call|>",
            "<|start|>assistant<|channel|>final<|message|>It is sunny in SF.<|return|>",
        );
        let blocks = decode(text);
        assert_eq!(blocks.len(), 3, "expected thinking + tooluse + text");

        match &blocks[0] {
            Block::Thinking { text, .. } => assert_eq!(text.inline_str(), Some("I should check the weather.")),
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &blocks[1] {
            Block::ToolUse { name, input, id, .. } => {
                assert_eq!(name, "get_weather", "functions. prefix stripped");
                assert_eq!(id, "call-1", "the id comes from the caller, never the text");
                assert_eq!(input.resolve().unwrap()["location"], "SF");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &blocks[2] {
            Block::Text { text, .. } => assert_eq!(text.inline_str(), Some("It is sunny in SF.")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn no_markers_passes_through_as_single_text() {
        let text = "Just a normal answer with no Harmony framing.";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { text: t, .. } => assert_eq!(t.inline_str(), Some(text)),
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(!looks_like_harmony(text));
    }

    #[test]
    fn dangling_channel_without_stop_does_not_panic() {
        // No stop token at all: the body runs to EOF.
        let text = "<|channel|>final<|message|>partial answer with no end token";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { text, .. } => assert_eq!(text.inline_str(), Some("partial answer with no end token")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn dangling_channel_no_message_is_safe() {
        // A channel marker but no <|message|> body: nothing to decode → passthrough, no panic.
        let text = "<|start|>assistant<|channel|>analysis";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Block::Text { .. }));
    }

    #[test]
    fn analysis_then_final_two_blocks() {
        let text = concat!(
            "<|channel|>analysis<|message|>think think<|end|>",
            "<|channel|>final<|message|>the answer<|return|>",
        );
        let blocks = decode(text);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], Block::Thinking { .. }));
        assert!(matches!(blocks[1], Block::Text { .. }));
    }

    #[test]
    fn tool_call_with_non_json_body_keeps_raw_string() {
        let text = "<|channel|>commentary to=functions.run <|message|>not json here<|call|>";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "run");
                assert_eq!(
                    input.resolve().unwrap().as_ref(),
                    &serde_json::Value::String("not json here".into())
                );
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn plain_commentary_without_recipient_becomes_text() {
        let text = "<|channel|>commentary<|message|>Let me work through this.<|end|><|channel|>final<|message|>Done.<|return|>";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Text { text, .. } => assert_eq!(text.inline_str(), Some("Let me work through this.")),
            other => panic!("expected Text preamble, got {other:?}"),
        }
        match &blocks[1] {
            Block::Text { text, .. } => assert_eq!(text.inline_str(), Some("Done.")),
            other => panic!("expected final Text, got {other:?}"),
        }
    }

    #[test]
    fn multibyte_content_is_char_boundary_safe() {
        let text = "<|channel|>final<|message|>café 日本語 🎉 emoji<|return|>";
        let blocks = decode(text);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { text, .. } => assert_eq!(text.inline_str(), Some("café 日本語 🎉 emoji")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn empty_string_is_safe() {
        let blocks = decode("");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Block::Text { .. }));
    }
}
