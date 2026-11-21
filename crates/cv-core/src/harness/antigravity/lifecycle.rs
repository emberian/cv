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
            Status::Complete if result.status.is_some() => exit
                .map(|code| if code == 0 { Status::Complete } else { Status::Failed })
                .or_else(|| schedule_terminal(result)),
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
    if let Some(job) = message
        .extra
        .get("antigravity")
        .and_then(|native| native.get("completed_schedule"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Observation {
            updates: vec![Update {
                target: Target::Job(job.to_owned()),
                status: Status::Complete,
                output: None,
            }],
            ..Default::default()
        };
    }
    let Some((job, canceled, output)) = completion(message, body) else {
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
            status: match (canceled, failed) {
                (true, _) => Status::Stopped,
                (false, true) => Status::Failed,
                (false, false) => Status::Complete,
            },
            output: Some(output.to_owned()),
        }],
        ..Default::default()
    }
}

pub(super) fn background_job(out: &str) -> Option<&str> {
    let body = out
        .lines()
        .find(|line| !line.starts_with("Created At:") && !line.starts_with("Completed At:"))?;
    body.strip_prefix("Tool is running as a background task with task id: ")
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn schedule_terminal(result: &ToolResult<'_>) -> Option<Status> {
    let payload = result
        .details
        .and_then(|d| d.get("payload"))
        .or_else(|| {
            result
                .message
                .and_then(|m| m.extra.get("antigravity"))
                .and_then(|extra| extra.get("payload").or_else(|| extra.get("native_payload")))
        })
        .filter(|p| p.get("type").and_then(serde_json::Value::as_str) == Some("exa.cortex.core.tools.Schedule"))?;
    let cancelled_by = payload
        .get("cancelled_by")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let outcome = payload
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    if cancelled_by.is_some() || matches!(outcome, Some("cancelled" | "canceled" | "stopped" | "aborted")) {
        return Some(Status::Stopped);
    }
    if matches!(outcome, Some("failed" | "error")) {
        return Some(Status::Failed);
    }
    if outcome.is_none() && background_job(result.body).is_some() {
        return None;
    }
    Some(Status::Complete)
}

fn polled_task<'a>(name: &str, out: &'a str) -> Option<(&'a str, &'a str)> {
    if name != "manage_task" {
        return None;
    }
    let mut lines = out
        .lines()
        .filter(|line| !line.starts_with("Created At:") && !line.starts_with("Completed At:"));
    let first = lines.next()?;
    // A kill is acknowledged by naming the task it cancelled.
    if let Some(job) = first
        .strip_prefix("Task \"")
        .and_then(|rest| rest.strip_suffix("\" cancelled."))
    {
        return Some((job, "cancelled"));
    }
    Some((
        first.strip_prefix("Task: ")?.trim(),
        lines.next()?.strip_prefix("Status: ")?.trim(),
    ))
}

/// The task a native message reports ended, whether it was canceled, and its result.
fn completion<'a>(message: &'a Message, body: &'a str) -> Option<(&'a str, bool, &'a str)> {
    // The adapter tags a cancellation notice `canceled_task` and a finish `completed_task`.
    let native = message.extra.get("antigravity")?;
    let job = native
        .get("completed_task")
        .or_else(|| native.get("canceled_task"))?
        .as_str()?;
    let delivered = super::unwrap_system_message_body(body)?;
    let rest = delivered.strip_prefix(&format!("Task id \"{job}\" "))?;
    let (canceled, output) = if let Some(output) = strip_result_header(rest, "finished with result:") {
        (false, output)
    } else if let Some(output) = strip_result_header(rest, "was canceled with result:") {
        (true, output)
    } else if rest == "was canceled" {
        (true, "")
    } else {
        return None;
    };
    Some((job, canceled, output.trim_end_matches("</SYSTEM_MESSAGE>").trim()))
}

fn strip_result_header<'a>(rest: &'a str, header: &str) -> Option<&'a str> {
    let tail = rest.strip_prefix(header)?;
    if tail.is_empty() {
        Some("")
    } else {
        tail.strip_prefix('\n').or_else(|| tail.strip_prefix("\r\n"))
    }
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
                details: None,
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
    fn a_cancellation_notice_stops_its_job_and_a_completion_notice_completes_it() {
        // Bodies as CLI 1.5.9 wrote them for a killed and a finished background command.
        for (key, body, expected) in [
            (
                "canceled_task",
                "[Message] timestamp=2026-11-15T02:27:15Z sender=session/task-4 priority=MESSAGE_PRIORITY_LOW content=Task id \"session/task-4\" was canceled with result:\nTool execution was canceled\nLog: file:///tmp/task-4.log",
                Status::Stopped,
            ),
            (
                "completed_task",
                "[Message] timestamp=2026-11-15T02:27:16Z sender=session/task-4 priority=MESSAGE_PRIORITY_HIGH content=Task id \"session/task-4\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nok",
                Status::Complete,
            ),
        ] {
            let mut delivered = Message::of_kind(
                crate::ir::Role::System,
                MessageKind::InjectedContext,
                crate::ir::Origin::Harness,
            );
            delivered
                .extra
                .insert("antigravity".into(), serde_json::json!({key: "session/task-4"}));
            let observation = message(&delivered, body);
            assert_eq!(observation.updates.len(), 1, "{key}");
            assert_eq!(observation.updates[0].status, expected);
            assert!(matches!(&observation.updates[0].target, Target::Job(job) if job == "session/task-4"));
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
                details: None,
            });
            assert_eq!(observation.status, expected);
            assert!(observation.started_jobs.is_empty());
        }
    }

    #[test]
    fn cancellation_is_evidence_only_where_it_names_the_task() {
        // Real shapes: manage_task's kill acknowledgement and the native canceled message.
        let kill = |body: &str| {
            tool_result(&ToolResult {
                message: None,
                tool: "manage_task",
                ops: &[],
                body,
                status: Some("completed"),
                is_error: false,
                details: None,
            })
        };
        let named = kill("Created At: 2026-11-13T15:37:47-08:00\nTask \"session/task-7\" cancelled.");
        assert_eq!(named.acting_on.as_deref(), Some("session/task-7"));
        assert_eq!(named.updates.len(), 1);
        assert_eq!(named.updates[0].status, Status::Stopped);
        for body in [
            "Task cancelled.",
            "Cancelled.",
            "Task: session/task-7\nStatus: RUNNING\nLog: file:///x",
        ] {
            assert!(kill(body).updates.is_empty(), "{body}");
        }
        let mut notice = Message::of_kind(
            crate::ir::Role::System,
            MessageKind::InjectedContext,
            crate::ir::Origin::Harness,
        );
        notice.extra.insert(
            "antigravity".into(),
            serde_json::json!({"completed_task":"session/task-7"}),
        );
        let wrap = |content: &str| {
            format!("<SYSTEM_MESSAGE>\n[Message] sender=session/task-7 content={content}\n</SYSTEM_MESSAGE>")
        };
        let canceled = message(
            &notice,
            &wrap("Task id \"session/task-7\" was canceled with result:\nTool execution was canceled\nLog: file:///x"),
        );
        assert_eq!(canceled.updates.len(), 1);
        assert_eq!(canceled.updates[0].status, Status::Stopped);
        let finished = message(
            &notice,
            &wrap("Task id \"session/task-7\" finished with result:\nCommand exited with code 0."),
        );
        assert_eq!(finished.updates[0].status, Status::Complete);
        for content in [
            "Task id \"session/task-8\" was canceled with result:\nx",
            "Task id \"session/task-7\" is running",
        ] {
            assert!(message(&notice, &wrap(content)).updates.is_empty(), "{content}");
        }
    }
}
