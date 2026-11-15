//! Antigravity's native task launch, status-poll, and completion envelopes.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::ir::{Message, MessageKind};

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    let native_task = result
        .message
        .and_then(|message| message.extra.get("antigravity"))
        .and_then(|extra| extra.get("task_details"))
        .filter(|task| task.get("kind").and_then(serde_json::Value::as_str) == Some("background_task"))
        .and_then(|task| task.get("id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    if let Some(job) = native_task {
        // Native task identity survives when AGY rewrites an acknowledgement with final output.
        // Backgrounding alone says nothing about liveness; terminal status plus the native
        // command-exit envelope can settle this job before its later notification is delivered.
        observation.started_jobs.push(job.to_owned());
        let exit = result
            .body
            .strip_prefix("Command exited with code ")
            .and_then(|body| body.split_once(".\n\nCommand output:\n"))
            .and_then(|(code, _)| code.parse::<i32>().ok());
        let terminal = match observation.status {
            Status::Failed | Status::Stopped => Some(observation.status),
            Status::Complete if result.status.is_some() => {
                exit.map(|code| if code == 0 { Status::Complete } else { Status::Failed })
            }
            _ => None,
        };
        if let Some(status) = terminal {
            observation.updates.push(Update {
                target: Target::Job(job.to_owned()),
                status,
                output: None,
            });
        }
    } else if !matches!(observation.status, Status::Failed | Status::Stopped) {
        if let Some(job) = background_job(result.body) {
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
    fn native_task_identity_requires_explicit_completion_evidence() {
        let mut message = Message::of_kind(
            crate::ir::Role::Tool,
            MessageKind::ToolResult,
            crate::ir::Origin::Harness,
        );
        message.extra.insert(
            "antigravity".into(),
            serde_json::json!({"task_details":{"kind":"background_task", "id":"session/task-7"}}),
        );
        for (body, status, expected) in [
            (
                "Tool is running as a background task with task id: session/task-7",
                Some("completed"),
                None,
            ),
            (
                "Command exited with code 0.\n\nCommand output:\nok",
                Some("completed"),
                Some(Status::Complete),
            ),
            (
                "Command exited with code 1.\n\nCommand output:\nfailed",
                Some("completed"),
                Some(Status::Failed),
            ),
            (
                "Command exited with code 0.\n\nCommand output:\nok",
                Some("running"),
                None,
            ),
            (
                "stdout:\nCommand exited with code 0.\n\nCommand output:\nok",
                Some("completed"),
                None,
            ),
        ] {
            let observation = tool_result(&ToolResult {
                message: Some(&message),
                tool: "run_command",
                ops: &[],
                body,
                status,
                is_error: false,
            });
            assert_eq!(observation.started_jobs, ["session/task-7"]);
            assert_eq!(
                observation.updates.first().map(|update| update.status),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn terminal_failure_or_cancellation_beats_launch_acknowledgement() {
        for (status, is_error, expected) in [
            (None, true, Status::Failed),
            (Some("failed"), false, Status::Failed),
            (Some("canceled"), false, Status::Stopped),
        ] {
            let observation = tool_result(&ToolResult {
                message: None,
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
