//! Antigravity's native task launch, status-poll, and completion envelopes.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::ir::{Message, MessageKind};

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    if !matches!(observation.status, Status::Failed | Status::Stopped) {
        if let Some(job) = background_job(result.body) {
            // The invocation returned an acknowledgement; the named job remains unfinished.
            observation.status = Status::Complete;
            observation.started_jobs.push(job.to_owned());
        }
    }
    if let Some((job, status)) = polled_task(result.tool, result.body) {
        observation.acting_on = Some(job.to_owned());
        let status = Status::from_native(status);
        if status.is_terminal() {
            observation.updates.push(Update {
                target: Target::Job(job.to_owned()),
                status,
                output: None,
            });
        }
    }
    observation
}

pub(super) fn message(message: &Message, body: &str) -> Observation {
    if message.kind != MessageKind::InjectedContext {
        return Observation::default();
    }
    let Some((job, output)) = completion(message, body) else {
        return Observation::default();
    };
    let failed = output.lines().any(|line| {
        line.strip_prefix("Command exited with code ")
            .and_then(|code| code.trim_end_matches('.').parse::<i32>().ok())
            .is_some_and(|code| code != 0)
    });
    Observation {
        updates: vec![Update {
            target: Target::Job(job.to_owned()),
            status: if failed { Status::Failed } else { Status::Complete },
            output: Some(output.to_owned()),
        }],
        ..Default::default()
    }
}

fn background_job(out: &str) -> Option<&str> {
    let body = out
        .lines()
        .find(|line| !line.starts_with("Created At:") && !line.starts_with("Completed At:"))?;
    body.strip_prefix("Tool is running as a background task with task id: ")
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn polled_task<'a>(name: &str, out: &'a str) -> Option<(&'a str, &'a str)> {
    if name != "manage_task" {
        return None;
    }
    let mut lines = out
        .lines()
        .filter(|line| !line.starts_with("Created At:") && !line.starts_with("Completed At:"));
    Some((
        lines.next()?.strip_prefix("Task: ")?.trim(),
        lines.next()?.strip_prefix("Status: ")?.trim(),
    ))
}

fn completion<'a>(message: &'a Message, body: &'a str) -> Option<(&'a str, &'a str)> {
    let job = message.extra.get("antigravity")?.get("completed_task")?.as_str()?;
    let marker = format!("content=Task id \"{job}\" finished with result:\n");
    let (_, output) = body.split_once(&marker)?;
    Some((job, output.trim_end_matches("</SYSTEM_MESSAGE>").trim()))
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
                tool: "run_command",
                ops: &[],
                body: "Tool is running as a background task with task id: session/task-7",
                status,
                is_error,
            });
            assert_eq!(observation.status, expected);
            assert!(observation.started_jobs.is_empty());
        }
    }
}
