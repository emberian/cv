//! Training-dataset export: render a [`Session`] into one JSONL record in a format that
//! fine-tuning toolchains ingest directly —
//!
//! - `chatml`   → `{"messages":[{"role","content"}]}` (OpenAI-style; the most portable)
//! - `sharegpt` → `{"conversations":[{"from","value"}]}`
//!
//! Unsloth Studio, TRL, and HuggingFace `datasets` load these with no bespoke adapter (Studio's
//! importer auto-detects both). One session → one line; the caller streams the corpus (so memory
//! stays O(one session)) and optionally applies [`crate::redact`] first.
//!
//! Tool calls/results are folded into the message text as fenced `tool_call` / `tool_result`
//! blocks (v1: the model learns the tool-use *pattern* in-band, and it imports as plain chatml).
//! Reasoning is kept in a `<thinking>` wrapper — it's high-signal for distilling into smaller
//! models, and a downstream filter can strip it if a given run wants answer-only SFT.

use crate::ir::{Block, Message, MessageKind, Role, Session};
use serde_json::{json, Value};

/// The training-role label for a message, or `None` when the turn was never part of what the model
/// saw or said (a harness notice, an API error, a compaction boundary, a carrier record, …) and so
/// must not appear in a distillation corpus. Keyed off [`MessageKind`], not the harness: injected
/// context (system reminders, hook output, environment context) is labelled as user INPUT — that is
/// where the model saw it on the wire — while the system prompt and a compaction summary are
/// `system`. `chat` picks the ChatML spelling, otherwise ShareGPT's.
fn training_role(m: &Message, chat: bool) -> Option<&'static str> {
    if !m.kind.is_model_visible() {
        return None;
    }
    Some(match m.kind {
        MessageKind::SystemPrompt | MessageKind::CompactionSummary => "system",
        MessageKind::InjectedContext | MessageKind::Prompt => {
            if chat {
                "user"
            } else {
                "human"
            }
        }
        MessageKind::Reply => {
            if chat {
                "assistant"
            } else {
                "gpt"
            }
        }
        MessageKind::ToolResult => "tool",
        // is_model_visible() admits nothing else; fall back to the role for safety.
        _ => match m.role {
            Role::System => "system",
            Role::User => {
                if chat {
                    "user"
                } else {
                    "human"
                }
            }
            Role::Assistant => {
                if chat {
                    "assistant"
                } else {
                    "gpt"
                }
            }
            Role::Tool => "tool",
        },
    })
}

/// Serialize a session as a ChatML record. Returns `None` if it has no non-empty turns.
pub fn to_chatml(session: &Session) -> Option<Value> {
    let resolver = session.resolver();
    let messages: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|m| {
            let role = training_role(m, true)?;
            let content = render_blocks(&m.content, &resolver);
            if content.trim().is_empty() {
                return None;
            }
            Some(json!({ "role": role, "content": content }))
        })
        .collect();
    if messages.is_empty() {
        return None;
    }
    Some(json!({ "messages": messages }))
}

/// Serialize a session as a ShareGPT record. Returns `None` if it has no non-empty turns.
pub fn to_sharegpt(session: &Session) -> Option<Value> {
    let resolver = session.resolver();
    let conversations: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|m| {
            let from = training_role(m, false)?;
            let value = render_blocks(&m.content, &resolver);
            if value.trim().is_empty() {
                return None;
            }
            Some(json!({ "from": from, "value": value }))
        })
        .collect();
    if conversations.is_empty() {
        return None;
    }
    Some(json!({ "conversations": conversations }))
}

/// Chunk size for streaming a span's content into the JSONL writer (bounds peak per giant field).
const SPAN_CHUNK: usize = 1 << 20; // 1 MiB

/// Flatten one message's blocks into a single training-text string (materializing — used by the
/// `Value`-building [`to_chatml`]/[`to_sharegpt`] and tests). Prefer [`write_chatml`] for export.
fn render_blocks(blocks: &[Block], resolver: &crate::lazy::Resolver) -> String {
    let mut out = String::new();
    render_blocks_into(blocks, resolver, &mut |s| out.push_str(s));
    out
}

/// Stream one message's blocks as training text to `emit`, resolving span content **in chunks** (so a
/// giant field is never materialized whole). Byte-identical to the old `parts.join("\n\n")`: the same
/// parts, separated by `\n\n`, with empty/redacted text turns producing no part.
fn render_blocks_into(blocks: &[Block], resolver: &crate::lazy::Resolver, emit: &mut dyn FnMut(&str)) {
    render_blocks_impl(blocks, resolver, None, emit)
}

/// Per-block redaction context for [`render_blocks_impl`]: the options plus the running stats.
type RedactCtx<'a> = (&'a crate::redact::RedactOptions, &'a mut crate::redact::RedactStats);

/// The shared block renderer behind [`render_blocks_into`] (plain) and the redacting path of
/// [`write_record`].
///
/// Without `redact`, span content is chunk-resolved straight to `emit` (a giant field is never held
/// whole). With `redact`, each text field is scrubbed **per block** — resolve (a span must be
/// scanned whole to redact it), scrub, emit, drop — so the peak is one field, and nothing is cloned
/// when a field is clean ([`scrub_cow`](crate::redact::scrub_cow) borrows). This replaced a
/// clone-the-whole-`Message`-then-redact path; only tool inputs still clone (small JSON, mutated by
/// structural scrubbing).
fn render_blocks_impl(
    blocks: &[Block],
    resolver: &crate::lazy::Resolver,
    mut redact: Option<RedactCtx<'_>>,
    emit: &mut dyn FnMut(&str),
) {
    let mut started = false;
    let sep = |started: &mut bool, emit: &mut dyn FnMut(&str)| {
        if *started {
            emit("\n\n");
        }
        *started = true;
    };
    for b in blocks {
        match b {
            Block::Text { text } => {
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = text.resolve(resolver);
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit);
                    emit(&crate::redact::scrub_cow(&s, opts, stats));
                } else if let Some(s) = text.inline_str() {
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit);
                    emit(s);
                } else if let Some(sp) = text.as_span() {
                    sep(&mut started, emit);
                    resolver.for_each_chunk(sp, SPAN_CHUNK, |c| emit(c));
                }
            }
            Block::Thinking { text, redacted, .. } => {
                if *redacted {
                    continue;
                }
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = text.resolve(resolver);
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit);
                    emit("<thinking>\n");
                    emit(&crate::redact::scrub_cow(&s, opts, stats));
                    emit("\n</thinking>");
                } else if let Some(s) = text.inline_str() {
                    if s.trim().is_empty() {
                        continue;
                    }
                    sep(&mut started, emit);
                    emit("<thinking>\n");
                    emit(s);
                    emit("\n</thinking>");
                } else if let Some(sp) = text.as_span() {
                    sep(&mut started, emit);
                    emit("<thinking>\n");
                    resolver.for_each_chunk(sp, SPAN_CHUNK, |c| emit(c));
                    emit("\n</thinking>");
                }
            }
            Block::ToolUse { name, input, .. } => {
                sep(&mut started, emit);
                emit("```tool_call\n");
                emit(name);
                emit(" ");
                let json = if let Some((opts, stats)) = redact.as_mut() {
                    // Structural scrub (string values only, keys/shape intact) needs a mutable
                    // value; tool inputs are small, so this clone is cheap.
                    let mut input = input.clone();
                    crate::redact::scrub_value(&mut input, opts, stats);
                    serde_json::to_string(&input).unwrap_or_default()
                } else {
                    serde_json::to_string(input).unwrap_or_default()
                };
                emit(&json);
                emit("\n```");
            }
            Block::ToolResult { content, is_error, .. } => {
                sep(&mut started, emit);
                emit(if *is_error {
                    "```tool_result error\n"
                } else {
                    "```tool_result\n"
                });
                if let Some((opts, stats)) = redact.as_mut() {
                    let s = content.resolve(resolver);
                    emit(&crate::redact::scrub_cow(&s, opts, stats));
                } else if let Some(s) = content.inline_str() {
                    emit(s);
                } else if let Some(sp) = content.as_span() {
                    resolver.for_each_chunk(sp, SPAN_CHUNK, |c| emit(c));
                }
                emit("\n```");
            }
            Block::Image { media_type, .. } => {
                sep(&mut started, emit);
                emit("[image");
                if let Some(m) = media_type.as_deref() {
                    emit(": ");
                    emit(m);
                }
                emit("]");
            }
            Block::File { path, mime, .. } => {
                sep(&mut started, emit);
                emit("[file: ");
                emit(path.as_deref().or(mime.as_deref()).unwrap_or("attachment"));
                emit("]");
            }
        }
    }
}

/// Whether a message renders to no training text (so [`to_chatml`]/[`write_chatml`] drop it) — the
/// cheap, no-resolve predicate matching [`render_blocks_into`]'s emptiness (only empty/redacted text
/// turns are empty; any tool/image/file block, or any span, makes it non-empty).
fn message_is_empty(m: &Message) -> bool {
    !m.content.iter().any(|b| match b {
        Block::Text { text } => text.is_span() || text.inline_str().is_some_and(|s| !s.trim().is_empty()),
        Block::Thinking { text, redacted, .. } => {
            !*redacted && (text.is_span() || text.inline_str().is_some_and(|s| !s.trim().is_empty()))
        }
        _ => true,
    })
}

/// Which export shape — the only difference is the array/role/content key names and the role labels.
#[derive(Clone, Copy)]
pub enum Format {
    Chatml,
    ShareGpt,
}

/// Write `session` as one JSONL record (no trailing newline) **streaming** — content (incl. giant
/// span fields) is rendered straight to `w` in chunks and JSON-escaped per chunk, so the whole record
/// is never held in memory. Returns `false` (writing nothing) when the session has no non-empty turns
/// — matching [`to_chatml`]'s `None`. With `redact`, each message is scrubbed (one materialized
/// message at a time) before rendering.
pub fn write_record<W: std::io::Write>(
    session: &Session,
    w: &mut W,
    fmt: Format,
    redact: Option<&crate::redact::RedactOptions>,
) -> std::io::Result<bool> {
    // Cheap pre-scan: skip the whole record if every exportable turn is empty (no resolve needed).
    let chat = matches!(fmt, Format::Chatml);
    let exportable = |m: &Message| training_role(m, chat).is_some() && !message_is_empty(m);
    if !session.messages.iter().any(exportable) {
        return Ok(false);
    }
    let (array_key, role_key, content_key) = match fmt {
        Format::Chatml => ("messages", "role", "content"),
        Format::ShareGpt => ("conversations", "from", "value"),
    };

    // serde_json's `Map` is a BTreeMap, so a one-shot `json!({role, content})` serializes its keys
    // in lexicographic order. Match that exactly: emit the smaller key first.
    let role_first = role_key < content_key;

    let resolver = session.resolver();
    write!(w, "{{\"{array_key}\":[")?;
    let mut first = true;
    for m in &session.messages {
        let Some(role) = training_role(m, chat) else {
            continue;
        };
        if message_is_empty(m) {
            continue;
        }
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        if role_first {
            write!(w, "{{\"{role_key}\":\"{role}\",\"{content_key}\":\"")?;
        } else {
            write!(w, "{{\"{content_key}\":\"")?;
        }
        let mut err: std::io::Result<()> = Ok(());
        {
            let mut emit = |piece: &str| {
                if err.is_ok() {
                    err = write_json_escaped(w, piece);
                }
            };
            if let Some(opts) = redact {
                // Scrub per block — resolve, scrub, emit, drop — so the peak is one field and a
                // clean message is never cloned (the old path cloned + materialized each message).
                let mut stats = crate::redact::RedactStats::default();
                render_blocks_impl(&m.content, &resolver, Some((opts, &mut stats)), &mut emit);
            } else {
                render_blocks_into(&m.content, &resolver, &mut emit);
            }
        }
        err?;
        if role_first {
            w.write_all(b"\"}")?;
        } else {
            write!(w, "\",\"{role_key}\":\"{role}\"}}")?;
        }
    }
    w.write_all(b"]}")?;
    Ok(true)
}

/// Convenience: stream a ChatML record. See [`write_record`].
pub fn write_chatml<W: std::io::Write>(session: &Session, w: &mut W) -> std::io::Result<bool> {
    write_record(session, w, Format::Chatml, None)
}

/// Write `s` to `w` as the *body* of a JSON string (no surrounding quotes), JSON-escaped exactly as
/// `serde_json` would. Escaping is context-free, so escaping each chunk and concatenating equals
/// escaping the whole — keeping streamed output byte-identical to a one-shot `serde_json::to_string`.
fn write_json_escaped<W: std::io::Write>(w: &mut W, s: &str) -> std::io::Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    let quoted = serde_json::to_string(s).map_err(std::io::Error::other)?;
    // `quoted` is `"…escaped…"`; write the body between the quotes.
    w.write_all(&quoted.as_bytes()[1..quoted.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Message};

    fn msg(role: Role, blocks: Vec<Block>) -> Message {
        Message {
            id: None,
            parent_id: None,
            role,
            timestamp: None,
            model: None,
            content: blocks,
            usage: None,
            extra: serde_json::Map::new(),
            kind: crate::ir::MessageKind::for_role(role),
            origin: crate::ir::Origin::for_role(role),
        }
    }

    fn session(messages: Vec<Message>) -> Session {
        Session {
            id: "t".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages,
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        }
    }

    #[test]
    fn chatml_maps_roles_and_folds_tools() {
        let s = session(vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: "fix the bug".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking {
                        text: "check the log".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::ToolUse {
                        id: "1".into(),
                        name: "Bash".into(),
                        input: json!({"cmd": "grep x"}),
                        namespace: None,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "1".into(),
                    content: "found it".into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: None,
                }],
            ),
        ]);
        let v = to_chatml(&s).expect("non-empty");
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].as_str().unwrap().contains("<thinking>"));
        assert!(msgs[1]["content"].as_str().unwrap().contains("```tool_call"));
        assert_eq!(msgs[2]["role"], "tool");
        assert!(msgs[2]["content"].as_str().unwrap().contains("found it"));
    }

    #[test]
    fn empty_session_is_none() {
        assert!(to_chatml(&session(vec![])).is_none());
        assert!(to_chatml(&session(vec![msg(Role::User, vec![Block::Text { text: "  ".into() }])])).is_none());
    }

    /// The per-block redacting renderer must produce exactly what "redact the whole session first,
    /// then render plain" produces — the old clone-and-materialize path it replaced.
    #[test]
    fn redacted_record_equals_redact_then_render() {
        let s = session(vec![
            msg(
                Role::User,
                vec![Block::Text {
                    text: "my key is sk-abcDEF1234567890ghijkl ok".into(),
                }],
            ),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking {
                        text: "they pasted sk-zzzzzzzzzzzzzzzzzzzz hmm".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::ToolUse {
                        id: "1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "export TOKEN=plain", "auth": "Bearer abcdef1234567890XYZ"}),
                        namespace: None,
                    },
                ],
            ),
            msg(
                Role::Tool,
                vec![Block::ToolResult {
                    tool_use_id: "1".into(),
                    content: "leaked ghp_0123456789abcdefABCDEF0123456789abcd done".into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: None,
                }],
            ),
            // A clean message: must render identically (and untouched) under redaction.
            msg(
                Role::User,
                vec![Block::Text {
                    text: "thanks, looks good".into(),
                }],
            ),
        ]);
        let opts = crate::redact::RedactOptions::default();

        let (scrubbed_session, _) = crate::redact::redact_with(&s, &opts);
        let mut expect = Vec::new();
        write_record(&scrubbed_session, &mut expect, Format::Chatml, None).unwrap();

        let mut got = Vec::new();
        write_record(&s, &mut got, Format::Chatml, Some(&opts)).unwrap();

        let got = String::from_utf8(got).unwrap();
        assert_eq!(got, String::from_utf8(expect).unwrap());
        assert!(got.contains("[REDACTED:api_key]"));
        assert!(!got.contains("sk-abcDEF1234567890ghijkl"));
        assert!(!got.contains("ghp_0123456789abcdef"));
        assert!(got.contains("thanks, looks good"));
    }

    #[test]
    fn sharegpt_uses_from_value() {
        let s = session(vec![msg(Role::User, vec![Block::Text { text: "hi".into() }])]);
        let v = to_sharegpt(&s).unwrap();
        assert_eq!(v["conversations"][0]["from"], "human");
        assert_eq!(v["conversations"][0]["value"], "hi");
    }
}
