//! Claude Code's hook payload (checked against the 2.1.296 hooks reference): `session_id` is the
//! top-level session even inside a sub-agent, whose `agent_id` names it; an `Edit`/`Write`/
//! `NotebookEdit` call writes its `file_path` or `notebook_path`.

use crate::hooks::{Agent, Event, Payload};
use crate::ir::{Harness, SessionRef};
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) fn hook_event(v: &Value) -> Result<Option<Event>> {
    let p = Payload::decode(v)?;
    let transcript = p.transcript();
    let agent = p.agent_id.as_deref().map(|id| {
        let id = id.strip_prefix("agent-").unwrap_or(id).to_string();
        Agent {
            transcript: p.agent_transcript().or_else(|| {
                transcript
                    .as_deref()
                    .and_then(super::sidecar::subagents_dir)
                    .map(|d| d.join(format!("agent-{id}.jsonl")))
            }),
            id,
            kind: p.agent_type.clone(),
            last_message: p.last_assistant_message.clone(),
        }
    });
    let mut edits = Vec::new();
    if let Some(tool) = &p.tool_input {
        let name = p.tool_name.as_deref().unwrap_or("Edit");
        crate::tools::classify::visit_tool_ops(
            Some(Harness::Claude),
            name,
            crate::json::JsonView::from(tool),
            crate::tools::classify::ClassifyOpts::COUNTS,
            |sem| {
                if let crate::tools::classify::ToolOp::FileEdit { path, .. } = sem.op {
                    edits.push(path.into_owned());
                }
            },
        )?;
        edits.sort();
        edits.dedup();
    }
    Ok(Some(Event {
        harness: Harness::Claude,
        session: SessionRef::new(p.session_id, Harness::Claude, transcript.unwrap_or_default()),
        event: p.hook_event_name,
        cwd: p.cwd,
        agent,
        edits,
    }))
}

/// `subagents/agent-<id>.jsonl` beside the parent, else the `Workflow` run's copy.
pub(super) fn agent_transcript(parent: &Path, agent_session: &str, workflow: Option<&str>) -> Option<PathBuf> {
    let bare = agent_session.strip_prefix("agent-")?;
    let dir = super::sidecar::subagents_dir(parent)?;
    let direct = dir.join(format!("agent-{bare}.jsonl"));
    if direct.exists() {
        return Some(direct);
    }
    Some(
        dir.join("workflows")
            .join(workflow?)
            .join(format!("agent-{bare}.jsonl")),
    )
}
