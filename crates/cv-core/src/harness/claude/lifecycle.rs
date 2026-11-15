//! Claude's native asynchronous launch, poll, stop and task-notification evidence.
//!
//! A launch acknowledgement names the task it started; every later piece of evidence (a
//! `<task-notification>`, a `TaskOutput` poll's payload status, a `TaskStop` acknowledgement)
//! names that task again. The structured `toolUseResult` is preferred where the record kept it;
//! the model-visible wrapper text is read only at fixed positions, never searched for in output.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::ir::{Message, MessageKind};
use crate::tools::classify::{BgAction, ToolOp};
use serde_json::Value;

/// Acknowledgements that open with the wrapper sentence, and the text preceding the task id.
const LAUNCHES: &[(&str, &str)] = &[
    ("Command running in background with ID: ", ""),
    ("Command running in the background with ID: ", ""),
    ("Command was manually backgrounded by user with ID: ", ""),
    ("Command was moved to the background (ID: ", ""),
    (TIMED_OUT, "moved to the background (ID: "),
    ("Async agent launched successfully.", "\nagentId: "),
    ("Monitor started (task ", ""),
    ("Workflow launched in background. Task ID: ", ""),
];
/// `SendMessage` to an agent with no active task resumes it in the background.
const SENT: &str = "{\"success\":true,";
const TIMED_OUT: &str = "Command did not complete within its ";

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    let details = result.details.filter(|details| details.is_object());
    let controlled = result.ops.iter().find_map(|op| match &op.op {
        ToolOp::Background {
            job_id: Some(id),
            action: action @ (BgAction::Poll | BgAction::Stop),
        } => Some((id.as_ref(), *action)),
        _ => None,
    });
    if let Some((job, action)) = controlled {
        observation.acting_on = Some(job.to_owned());
        let status = match action {
            BgAction::Poll => {
                let polled = polled(job, details, result.body);
                observation.output = polled.as_ref().and_then(|(_, output)| output.clone());
                polled.map(|(status, _)| status)
            }
            _ => stopped_status(job, details, result.body),
        };
        if let Some(status) = status.filter(|status| status.is_terminal()) {
            observation.updates.push(Update {
                target: Target::Job(job.to_owned()),
                status,
                output: None,
            });
        }
        return observation;
    }
    if matches!(observation.status, Status::Failed | Status::Stopped) {
        return observation;
    }
    match launched(details, result.body) {
        Some(Some(job)) => observation.started_jobs.push(job),
        // A recognised acknowledgement whose task id cannot be read still started something.
        Some(None) => observation.status = Status::Pending,
        None => {}
    }
    observation
}

/// `Some(Some(id))`: a launch naming its task; `Some(None)`: a launch whose id is unreadable.
fn launched(details: Option<&Value>, body: &str) -> Option<Option<String>> {
    if let Some(details) = details {
        let field = |name: &str| details.get(name).and_then(Value::as_str).filter(|id| !id.is_empty());
        let asynchronous = details.get("status").and_then(Value::as_str) == Some("async_launched")
            || details.get("isAsync").and_then(Value::as_bool) == Some(true);
        let job = field("backgroundTaskId")
            .or_else(|| resumed_agent(details))
            .or_else(|| {
                asynchronous
                    .then(|| field("agentId").or_else(|| field("taskId")))
                    .flatten()
            })
            // Monitor's sidecar is exactly its task handle.
            .or_else(|| details.get("timeoutMs").and(field("taskId")));
        if let Some(job) = job {
            return Some(Some(job.to_owned()));
        }
    }
    for (opening, before_id) in LAUNCHES {
        if let Some(rest) = body.strip_prefix(opening) {
            return Some(id_after(rest, before_id));
        }
    }
    if details.is_none() && body.starts_with(SENT) {
        let sent = serde_json::from_str::<Value>(body).ok()?;
        return resumed_agent(&sent).map(|agent| Some(agent.to_owned()));
    }
    // Output read before a timeout precedes the wrapper sentence, which then opens a line. With
    // the structured result present, its missing task id above already says nothing was moved.
    if details.is_some() {
        return None;
    }
    let line = body.lines().find(|line| line.starts_with(TIMED_OUT))?;
    Some(id_after(line, "moved to the background (ID: "))
}

/// Wordings changed across releases ("Resuming agent …", "… had no active task; resumed from
/// transcript …"); the field naming the resumed agent did not.
fn resumed_agent(sent: &Value) -> Option<&str> {
    let field = |pointer: &str| {
        sent.pointer(pointer)
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    };
    field("/resumedAgentId").or_else(|| {
        field("/message")
            .is_some_and(|message| message.starts_with("Resuming agent "))
            .then(|| field("/pin/id"))
            .flatten()
    })
}

fn id_after(text: &str, marker: &str) -> Option<String> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_owned())
}

/// The polled task's own status field and output. A poll that returned says nothing by succeeding.
fn polled(job: &str, details: Option<&Value>, body: &str) -> Option<(Status, Option<String>)> {
    if let Some(details) = details.filter(|details| details.get("retrieval_status").is_some()) {
        let task = details.get("task")?;
        let status = Status::from_native(task.get("status")?.as_str()?);
        let output = task.get("output").and_then(Value::as_str).map(str::to_owned);
        return (task.get("task_id")?.as_str()? == job).then_some((status, output));
    }
    let wrapped = body.strip_prefix("<retrieval_status>")?;
    let (header, output) = match wrapped.split_once("<output>") {
        Some((header, output)) => (
            header,
            Some(output.rsplit_once("</output>").map_or(output, |(output, _)| output)),
        ),
        None => (wrapped, None),
    };
    let tag = |name| super::tag_inner(header, name).map(str::trim);
    let status = Status::from_native(tag("status")?);
    (tag("task_id")? == job).then(|| (status, output.map(|output| output.trim_matches('\n').to_owned())))
}

/// A stop acknowledgement naming this task, or the refusal that reports its terminal status.
fn stopped_status(job: &str, details: Option<&Value>, body: &str) -> Option<Status> {
    let message = details
        .and_then(|details| details.get("message"))
        .and_then(Value::as_str);
    let acknowledged = message
        .or_else(|| body.strip_prefix("{\"message\":\""))
        .and_then(|message| message.strip_prefix("Successfully stopped task: "));
    if let Some(rest) = acknowledged {
        let rest = rest.strip_prefix(job)?;
        return (rest.is_empty() || rest.starts_with([' ', '"'])).then_some(Status::Stopped);
    }
    let rest = body
        .strip_prefix("<tool_use_error>Task ")?
        .strip_prefix(job)?
        .strip_prefix(" is not running (status: ")?;
    Some(Status::from_native(rest.split_once(')')?.0))
}

pub(super) fn message(message: &Message, body: &str) -> Observation {
    let body = if let Some(queued) = queued_task_notice(message) {
        queued
    } else if matches!(message.kind, MessageKind::InjectedContext | MessageKind::SubagentReturn) {
        body
    } else {
        return Observation::default();
    };
    // A queued command is a notification only when the harness queued it as one; its record
    // then holds the notice itself beside the reminder the model was shown.
    let attachment = message.extra.get("claude").and_then(|bag| bag.get("attachment"));
    if attachment.is_some_and(|attachment| {
        attachment
            .get("commandMode")
            .is_some_and(|mode| mode != "task-notification")
    }) {
        return Observation::default();
    }
    let body = attachment
        .and_then(|attachment| attachment.get("prompt")?.as_str())
        .unwrap_or(body);
    let Some(notice) = notification(body).and_then(task_notice) else {
        return Observation::default();
    };
    // The task id finds the launch that registered it; the tool-use id covers a launch whose
    // acknowledgement did not name its task. The reducer settles an open call once.
    let targets = [
        notice.task.map(|id| Target::Job(id.to_owned())),
        notice.call.map(|id| Target::Call(id.to_owned())),
    ];
    Observation {
        updates: targets
            .into_iter()
            .flatten()
            .map(|target| Update {
                target,
                status: notice.status,
                output: Some(notice.output.clone()),
            })
            .collect(),
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

/// The notification in a native injected message: bare, or (Claude Code 2.1.3xx) after the
/// harness's "not user input" preamble, where the tag opens a line.
fn notification(body: &str) -> Option<&str> {
    let body = body.trim_start();
    let body = body.strip_prefix("<system-reminder>").map_or(body, str::trim_start);
    if body.starts_with(NOTICE) {
        return Some(body);
    }
    let rest = body.strip_prefix("[SYSTEM NOTIFICATION - NOT USER INPUT]")?;
    Some(&rest[rest.find(&format!("\n{NOTICE}"))? + 1..])
}

const NOTICE: &str = "<task-notification>";

struct Notice<'a> {
    task: Option<&'a str>,
    call: Option<&'a str>,
    status: Status,
    output: String,
}

/// Only native injected messages reach this decoder; quoted user prose is not completion evidence.
fn task_notice(text: &str) -> Option<Notice<'_>> {
    let text = text.strip_prefix(NOTICE)?;
    let text = text
        .split_once("</task-notification>")
        .map_or(text, |(notice, _)| notice);
    let tag = |name| {
        super::tag_inner(text, name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let (task, call) = (tag("task-id"), tag("tool-use-id"));
    let summary = tag("summary").unwrap_or("");
    if let Some(status) = tag("status") {
        return (task.is_some() || call.is_some()).then(|| Notice {
            task,
            call,
            status: Status::from_native(status),
            output: format!("{status}: {summary}"),
        });
    }
    // A monitor's expiry is its only terminal notice and carries no status tag. The event is
    // the harness's bracketed sentence alone; an expired watch claims nothing about its subject.
    let event = tag("event")?;
    (summary.starts_with("Monitor event: ") && event.starts_with("[Monitor expired after ") && event.ends_with(']'))
        .then(|| Notice {
            task,
            call,
            status: Status::Stopped,
            output: event.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::classify::ToolSemantic;
    use serde_json::json;

    fn observe(tool: &str, ops: &[ToolSemantic<'static>], body: &str, details: Option<&Value>) -> Observation {
        tool_result(&ToolResult {
            message: None,
            tool,
            ops,
            body,
            status: None,
            is_error: body.starts_with("<tool_use_error>"),
            details,
        })
    }

    fn background(tool: &'static str, job: &'static str, action: BgAction) -> [ToolSemantic<'static>; 1] {
        [ToolSemantic {
            tool: tool.into(),
            op: ToolOp::Background {
                job_id: Some(job.into()),
                action,
            },
        }]
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
                tool: "Bash",
                ops: &[],
                body: "Command running in background with ID: job",
                status,
                is_error,
                details: None,
            });
            assert_eq!(observation.status, expected);
            assert!(observation.started_jobs.is_empty());
        }
    }

    #[test]
    fn every_native_launch_acknowledgement_names_its_task() {
        for (tool, body) in [
            ("Bash", "Command running in background with ID: b1. Output is being written to: /t/b1.output."),
            ("Bash", "Command was manually backgrounded by user with ID: b1. Output is being written to: /t."),
            ("Bash", "Command was moved to the background (ID: b1) after running for 180s. Output is"),
            ("Bash", "Command was moved to the background (ID: b1) so that a message that arrived"),
            ("Bash", "Command did not complete within its 600s timeout and was moved to the background (ID: b1). Output"),
            ("Bash", "partial output\nCommand did not complete within its 60s timeout and was moved to the background (ID: b1). Output"),
            ("Agent", "Async agent launched successfully. (internal metadata)\nagentId: b1 (internal ID)"),
            ("Monitor", "Monitor started (task b1, expires in 30m unless the source ends first)."),
            ("Workflow", "Workflow launched in background. Task ID: b1\nSummary: readers"),
            ("SendMessage", "{\"success\":true,\"message\":\"Resuming agent b1\",\"resumedAgentId\":\"b1\",\"pin\":{}}"),
            ("SendMessage", "{\"success\":true,\"message\":\"Resuming agent b\",\"pin\":{\"id\":\"b1\",\"name\":\"b1\"}}"),
            ("SendMessage", "{\"success\":true,\"message\":\"Agent \\\"b1\\\" had no active task; resumed from transcript in the background with your message.\",\"resumedAgentId\":\"b1\",\"pin\":{\"id\":\"b1\"}}"),
        ] {
            let observation = observe(tool, &[], body, None);
            assert_eq!(observation.started_jobs, ["b1"], "{body}");
            assert_eq!(observation.status, Status::Complete, "{body}");
        }
        for details in [
            json!({"stdout":"partial","backgroundTaskId":"b1","timedOutAfterMs":600000}),
            json!({"status":"async_launched","isAsync":true,"agentId":"b1"}),
            json!({"status":"async_launched","taskId":"b1","taskType":"local_workflow"}),
            json!({"taskId":"b1","timeoutMs":1000,"persistent":false}),
            json!({"success":true,"message":"Resuming agent b1","resumedAgentId":"b1"}),
            json!({"success":true,"message":"Resuming agent b","pin":{"id":"b1"}}),
        ] {
            assert_eq!(
                observe("Bash", &[], "unrelated", Some(&details)).started_jobs,
                ["b1"],
                "{details}"
            );
        }
    }

    #[test]
    fn quoted_acknowledgements_and_queued_messages_start_nothing() {
        for body in [
            "[tool_result t1] Command did not complete within its 600s timeout and was moved to the background (ID: b1).",
            "- [7] wait → Command was moved to the background (ID: b1) after running for 180s.",
            "{\"success\":true,\"message\":\"Message queued for delivery to b1 at its next tool round.\",\"pin\":{\"id\":\"b1\"}}",
        ] {
            let observation = observe("Bash", &[], body, None);
            assert!(observation.started_jobs.is_empty(), "{body}");
            assert_eq!(observation.status, Status::Complete, "{body}");
        }
        // The structured result of a finished command outranks a wrapper sentence in its output.
        let finished = json!({"stdout":"x\nCommand did not complete within its 5s timeout and was moved to the background (ID: b1).","interrupted":false});
        let body = finished["stdout"].as_str().unwrap();
        assert!(observe("Bash", &[], body, Some(&finished)).started_jobs.is_empty());
        // An acknowledgement whose id is cut off still started something.
        assert_eq!(
            observe("Agent", &[], "Async agent launched successfully.", None).status,
            Status::Pending
        );
    }

    #[test]
    fn poll_settles_only_from_the_payload_status_of_the_polled_task() {
        let poll = background("TaskOutput", "b1", BgAction::Poll);
        let text = |status: &str, tail: &str| {
            format!("<retrieval_status>success</retrieval_status>\n\n<task_id>b1</task_id>\n\n<task_type>local_bash</task_type>\n\n<status>{status}</status>\n\n{tail}")
        };
        for (body, expected) in [
            (
                text("completed", "<exit_code>0</exit_code>\n\n<output>\nok\n</output>"),
                Some(Status::Complete),
            ),
            (
                text("failed", "<exit_code>101</exit_code>\n\n<output>\nerror\n</output>"),
                Some(Status::Failed),
            ),
            (text("killed", ""), Some(Status::Stopped)),
            (text("running", "<output>\n<status>completed</status>\n</output>"), None),
            ("<retrieval_status>timeout</retrieval_status>".to_owned(), None),
            (text("completed", "").replace("<task_id>b1<", "<task_id>other<"), None),
            (format!("stdout first\n{}", text("completed", "")), None),
        ] {
            let observation = observe("TaskOutput", &poll, &body, None);
            assert_eq!(observation.acting_on.as_deref(), Some("b1"));
            assert_eq!(
                observation.updates.first().map(|update| update.status),
                expected,
                "{body}"
            );
            assert_eq!(observation.status, Status::Complete);
        }
        for (details, expected) in [
            (
                json!({"retrieval_status":"success","task":{"task_id":"b1","status":"failed","exitCode":1}}),
                Some(Status::Failed),
            ),
            (
                json!({"retrieval_status":"not_ready","task":{"task_id":"b1","status":"running"}}),
                None,
            ),
            (json!({"retrieval_status":"timeout","task":null}), None),
            (
                json!({"retrieval_status":"success","task":{"task_id":"other","status":"completed"}}),
                None,
            ),
        ] {
            // The structured task outranks model-visible text that claims completion.
            let observation = observe("TaskOutput", &poll, &text("completed", ""), Some(&details));
            let got = observation.updates.first().map(|update| update.status);
            assert_eq!(got, expected, "{details}");
        }
        let failed_poll = observe(
            "TaskOutput",
            &poll,
            "<tool_use_error>No task found with ID: b1</tool_use_error>",
            None,
        );
        assert!(failed_poll.updates.is_empty());
        assert_eq!(failed_poll.status, Status::Failed);
    }

    #[test]
    fn stop_acknowledgement_names_the_stopped_task() {
        let stop = background("TaskStop", "b1", BgAction::Stop);
        let stopped = |observation: Observation| observation.updates.first().map(|update| update.status);
        let ack = "{\"message\":\"Successfully stopped task: b1 (sleep 600)\",\"task_id\":\"b1\",\"task_type\":\"local_bash\"}";
        assert_eq!(stopped(observe("TaskStop", &stop, ack, None)), Some(Status::Stopped));
        let details = json!({"message":"Successfully stopped task: b1 (sleep 600)","task_id":"b1"});
        assert_eq!(
            stopped(observe("TaskStop", &stop, "", Some(&details))),
            Some(Status::Stopped)
        );
        assert_eq!(
            stopped(observe(
                "TaskStop",
                &stop,
                "<tool_use_error>Task b1 is not running (status: killed)</tool_use_error>",
                None
            )),
            Some(Status::Stopped)
        );
        assert_eq!(
            stopped(observe(
                "TaskStop",
                &stop,
                "<tool_use_error>Task b1 is not running (status: completed)</tool_use_error>",
                None
            )),
            Some(Status::Complete)
        );
        for body in [
            "{\"message\":\"Successfully stopped task: b10 (other)\",\"task_id\":\"b10\"}",
            "<tool_use_error>No task found with ID: b1. Running background agents: b1 (lane)</tool_use_error>",
            "{\"message\":\"Stop requested\"}",
        ] {
            assert_eq!(stopped(observe("TaskStop", &stop, body, None)), None, "{body}");
        }
    }

    #[test]
    fn notices_target_the_task_then_the_call_and_only_an_expiry_ends_a_monitor() {
        let notice = |body: &str| {
            let message = Message::of_kind(
                crate::ir::Role::User,
                MessageKind::InjectedContext,
                crate::ir::Origin::Harness,
            );
            super::message(&message, body)
        };
        let wrapped = notice("[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event, NOT a message from the user.\n\n<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>t1</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>");
        assert_eq!(wrapped.updates.len(), 2);
        assert!(wrapped.updates.iter().all(|update| update.status == Status::Complete));
        // Text after the notice, and a tag quoted mid-line or without the preamble, decide nothing.
        let trailing = notice("<task-notification>\n<task-id>b1</task-id>\n<summary>Monitor event: \"x\"</summary>\n<event>tick</event>\n</task-notification>\n<status>completed</status>");
        assert!(trailing.updates.is_empty());
        for quoted in [
            "The lane printed <task-notification>\n<task-id>b1</task-id>\n<status>completed</status>",
            "[SYSTEM NOTIFICATION - NOT USER INPUT] see <task-notification><task-id>b1</task-id><status>completed</status>",
            "note\n<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>",
        ] {
            assert!(notice(quoted).updates.is_empty(), "{quoted}");
        }
        let mut typed = Message::of_kind(
            crate::ir::Role::System,
            MessageKind::InjectedContext,
            crate::ir::Origin::Harness,
        );
        typed.extra.insert(
            "claude".into(),
            json!({"attachment_type":"queued_command","attachment":{"commandMode":"prompt"}}),
        );
        let pasted = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        assert!(super::message(&typed, pasted).updates.is_empty());
        typed.extra.insert(
            "claude".into(),
            json!({"attachment_type":"queued_command","attachment":{"commandMode":"task-notification"}}),
        );
        assert_eq!(super::message(&typed, pasted).updates.len(), 1);
        // The record's own notice is read in place of the reminder rendered around it.
        typed.extra.insert(
            "claude".into(),
            json!({"attachment":{"commandMode":"task-notification","prompt":pasted}}),
        );
        assert_eq!(
            super::message(&typed, "<system-reminder>\nrendered otherwise\n</system-reminder>")
                .updates
                .len(),
            1
        );
        let reminder = format!("<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\nNot a user message.\n\n{pasted}\n</system-reminder>");
        assert_eq!(notice(&reminder).updates.len(), 1);
        let both = notice("<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>t1</tool-use-id>\n<status>failed</status>\n<summary>exit 1</summary>\n</task-notification>");
        assert!(matches!(&both.updates[0].target, Target::Job(id) if id == "b1"));
        assert!(matches!(&both.updates[1].target, Target::Call(id) if id == "t1"));
        assert!(both.updates.iter().all(|update| update.status == Status::Failed));
        let task_only = notice("<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n<summary>Agent done</summary>\n</task-notification>");
        assert_eq!(task_only.updates.len(), 1);
        assert!(matches!(&task_only.updates[0].target, Target::Job(id) if id == "a1"));
        let line = |event: &str| {
            notice(&format!("<task-notification>\n<task-id>b1</task-id>\n<summary>Monitor event: \"build\"</summary>\n<event>{event}</event>\n</task-notification>"))
        };
        assert!(line("compiled 3 of 9").updates.is_empty());
        assert!(line("log: [Monitor expired after 30m]").updates.is_empty());
        let expired =
            line("[Monitor expired after 30m with no events delivered. Re-arm it if you still need the watch.]");
        assert_eq!(expired.updates.len(), 1);
        assert_eq!(expired.updates[0].status, Status::Stopped);
    }
}
