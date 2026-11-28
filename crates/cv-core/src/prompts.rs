//! Human prompt and question-answer projection (`PromptRow`).
//!
//! Shared by `cv show --kind prompt,answer` (`cmd/session_reports.rs`) and
//! `GET /api/session/{harness}/{id}/prompts` (`api.rs`), matching the `prompt_row` contract in
//! `cv schema --json`.

use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::affordance::{classify_tool_json, Kind};
use crate::harness::Adapter;
use crate::ir::{Block, Harness, Message, MessageKind, Origin, SessionRef};
use crate::json::Json;
use crate::lazy::Resolver;
use crate::stream::{Demand, Flow};

/// One projected row of human input in a session: either a typed human prompt or the non-error
/// answer to an interactive question tool call (`AskUserQuestion`, `request_user_input`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptRow {
    /// Zero-based message index in the raw session (what `cv show --around N` takes).
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// `"prompt"` (typed by the person) or `"answer"` (a question-tool result).
    pub kind: &'static str,
    pub text: String,
}

fn is_question_tool(h: Harness, name: &str, input: &Json) -> Result<bool> {
    if matches!(classify_tool_json(h, name, input)?, Some((Kind::Question, _))) {
        return Ok(true);
    }
    let bare = name.rsplit(':').next().unwrap_or(name);
    Ok(bare == "request_user_input")
}

/// Stream `r` once and collect all human prompts and question-tool answers in transcript order.
pub fn read(adapter: &dyn Adapter, r: &SessionRef) -> Result<Vec<PromptRow>> {
    read_filtered(adapter, r, None, true, true)
}

/// Stream `r` once and keep `prompt`/`human` messages (when `prompts`) and non-error results of
/// [`Kind::Question`] tool calls paired by `tool_use_id` (when `answers`), in transcript order
/// with their raw zero-based message indices. `span` bounds raw message indices (`[start, end)`,
/// `None` end = through the last) when `--pre-compaction` narrows the session before projection.
pub fn read_filtered(
    adapter: &dyn Adapter,
    r: &SessionRef,
    span: Option<(usize, Option<usize>)>,
    prompts: bool,
    answers: bool,
) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut question_ids = HashSet::new();
    let mut idx = 0usize;
    let resolver = Resolver::new(Some(r.path.clone()));
    let mut error = None;
    adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if let Some((_, Some(e))) = span {
            if i >= e {
                return Flow::Stop;
            }
        }
        let selected = (|| -> Result<()> {
            if answers {
                for block in &m.content {
                    if let Block::ToolUse { id, name, input, .. } = block {
                        if !id.is_empty() && is_question_tool(r.harness, name, input)? {
                            question_ids.insert(id.clone());
                        }
                    }
                }
            }
            if span.is_some_and(|(s, _)| i < s) {
                return Ok(());
            }
            if prompts && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
                let mut text = String::new();
                for block in &m.content {
                    if let Block::Text { text: body, .. } = block {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        body.try_for_each_chunk(&resolver, 64 * 1024, |part| {
                            text.push_str(part);
                            Ok(())
                        })?;
                    }
                }
                if !text.trim().is_empty() {
                    rows.push(PromptRow {
                        index: i,
                        timestamp: m.timestamp,
                        kind: "prompt",
                        text,
                    });
                }
            }
            if answers {
                for block in &m.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                    {
                        if !*is_error && question_ids.contains(tool_use_id) {
                            let text = content.try_resolve(&resolver)?.into_owned();
                            if !text.trim().is_empty() {
                                rows.push(PromptRow {
                                    index: i,
                                    timestamp: m.timestamp,
                                    kind: "answer",
                                    text,
                                });
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(failure) = selected {
            error = Some(failure);
            return Flow::Stop;
        }
        Flow::Continue
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_human_prompts_and_question_answers_while_skipping_injected_and_errored_turns() {
        let dir = std::env::temp_dir().join(format!("cv-prompts-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let doc = json!({
            "open_session": "0.3",
            "harness": "claude",
            "id": "s",
            "messages": [
                {
                    "role": "user",
                    "kind": "prompt",
                    "origin": "human",
                    "timestamp": "2026-10-01T12:00:00Z",
                    "content": [{"type": "text", "text": "Build the feature"}]
                },
                {
                    "role": "user",
                    "kind": "injected_context",
                    "origin": "harness",
                    "content": [{"type": "text", "text": "<system-reminder>not human</system-reminder>"}]
                },
                {
                    "role": "assistant",
                    "kind": "reply",
                    "origin": "model",
                    "content": [
                        {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {"questions": []}},
                        {"type": "tool_use", "id": "q2", "name": "mcp:request_user_input", "input": {}},
                        {"type": "tool_use", "id": "b1", "name": "Bash", "input": {"command": "ls"}}
                    ]
                },
                {
                    "role": "tool",
                    "kind": "tool_result",
                    "origin": "harness",
                    "timestamp": "2026-10-01T12:01:00Z",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "q1", "content": "Option A", "is_error": false},
                        {"type": "tool_result", "tool_use_id": "q2", "content": "cancelled", "is_error": true},
                        {"type": "tool_result", "tool_use_id": "b1", "content": "README.md", "is_error": false}
                    ]
                }
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let r = SessionRef::new("s", Harness::OpenSession, path);
        let adapter = crate::harness::for_harness(Harness::OpenSession).unwrap();

        let all = read(adapter.as_ref(), &r).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            (all[0].index, all[0].kind, all[0].text.as_str()),
            (0, "prompt", "Build the feature")
        );
        assert_eq!(
            (all[1].index, all[1].kind, all[1].text.as_str()),
            (3, "answer", "Option A")
        );

        let only_answers = read_filtered(adapter.as_ref(), &r, Some((2, None)), true, true).unwrap();
        assert_eq!(only_answers.len(), 1);
        assert_eq!(only_answers[0].kind, "answer");

        std::fs::remove_dir_all(&dir).ok();
    }
}
