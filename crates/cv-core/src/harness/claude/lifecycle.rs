//! Claude's native asynchronous launch and task-notification evidence.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::ir::{Message, MessageKind};

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    if !matches!(observation.status, Status::Failed | Status::Stopped)
        && (result.body.starts_with("Command running in background with ID:")
            || result.body.starts_with("Command running in the background with ID:")
            || result.body.starts_with("Async agent launched successfully."))
    {
        observation.status = Status::Pending;
    }
    observation
}

pub(super) fn message(message: &Message, body: &str) -> Observation {
    let body = if let Some(queued) = queued_task_notice(message) {
        queued
    } else if matches!(message.kind, MessageKind::InjectedContext | MessageKind::SubagentReturn) {
        body
    } else {
        return Observation::default();
    };
    let Some((id, status, summary)) = task_notice(body) else {
        return Observation::default();
    };
    Observation {
        updates: vec![Update {
            target: Target::Call(id.to_owned()),
            status: Status::from_native(status),
            output: Some(format!("{status}: {summary}")),
        }],
        dedup: Some(crate::digest::sha256_hex(body.as_bytes())),
        ..Default::default()
    }
}

fn queued_task_notice(message: &Message) -> Option<&str> {
    let row = message.extra.get(crate::ir::CARRIER_KEY)?;
    (row.get("type")?.as_str()? == "queue-operation" && row.get("operation")?.as_str()? == "enqueue")
        .then(|| row.get("content")?.as_str())
        .flatten()
}

/// Only native injected messages reach this decoder; quoted user prose is not completion evidence.
fn task_notice(text: &str) -> Option<(&str, &str, &str)> {
    let text = text.strip_prefix("<task-notification>")?;
    let tag = |name| super::tag_inner(text, name).map(str::trim);
    Some((tag("tool-use-id")?, tag("status")?, tag("summary").unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_failure_or_cancellation_beats_launch_acknowledgement() {
        for (status, is_error, expected) in [
            (None, true, Status::Failed),
            (Some("failed"), false, Status::Failed),
            (Some("canceled"), false, Status::Stopped),
        ] {
            let observation = tool_result(&ToolResult {
                message: None,
                tool: "Bash",
                ops: &[],
                body: "Command running in background with ID: job",
                status,
                is_error,
            });
            assert_eq!(observation.status, expected);
            assert!(observation.started_jobs.is_empty());
        }
    }
}
