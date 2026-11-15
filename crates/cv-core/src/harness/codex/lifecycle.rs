//! Native Codex cell and PTY lifecycle envelopes.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::tools::classify::{ToolOp, ToolSemantic};
use serde_json::Value;

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    let acting_on = poll_job(result.ops);
    let has_process = result
        .ops
        .iter()
        .any(|op| matches!(op.op, ToolOp::Command { .. }) || op.tool.ends_with("write_stdin"));
    let envelope = envelope(result.body, has_process);
    let status = if envelope.pty_exit.is_some_and(|code| code != 0) {
        Status::Failed
    } else {
        envelope.status
    };
    if observation.status == Status::Complete {
        observation.status = status;
    }
    if let Some(job) = &acting_on {
        let done = if job.starts_with("cell:") {
            envelope.cell_done
        } else {
            envelope.pty_exit.is_some()
        };
        if done {
            observation.updates.push(Update {
                target: Target::Job(job.clone()),
                status,
                output: None,
            });
        }
    }
    observation.acting_on = acting_on;
    observation.started_jobs = envelope.running;
    observation.uncertain = envelope.uncertain;
    observation.output = envelope.result_text;
    observation
}

#[derive(Default)]
struct Envelope {
    running: Vec<String>,
    cell_done: bool,
    pty_exit: Option<i64>,
    status: Status,
    uncertain: bool,
    result_text: Option<String>,
}

fn poll_job(ops: &[ToolSemantic<'_>]) -> Option<String> {
    let mut jobs = ops.iter().filter_map(|op| {
        let ToolOp::Background { job_id: Some(id), .. } = &op.op else {
            return None;
        };
        match op.tool.rsplit(['.', '_']).next().unwrap_or(&op.tool) {
            "wait" => Some(format!("cell:{id}")),
            _ if op.tool.ends_with("write_stdin") => Some(format!("pty:{id}")),
            _ => None,
        }
    });
    let first = jobs.next()?;
    jobs.next().is_none().then_some(first)
}

/// Read only wrapper header lines. A command printing a lifecycle-looking line in stdout
/// cannot create or finish a job. Code Mode may print structured exec_command results as JSON.
fn envelope(out: &str, has_process: bool) -> Envelope {
    let mut result = Envelope::default();
    let script = out.starts_with("Script ");
    let (header, body) = out
        .split_once("\nOutput:\n")
        .or_else(|| out.split_once("\nFinal output:\n"))
        .unwrap_or((out, ""));
    let native_header =
        script || out.starts_with("Chunk ID:") || out.starts_with("Wall time:") || out.starts_with("Process ");
    for line in header.lines().filter(|_| native_header) {
        if let Some(id) = line.strip_prefix("Script running with cell ID ") {
            result.running.push(format!("cell:{}", id.trim()));
        } else if let Some(id) = line.strip_prefix("Process running with session ID ") {
            result.running.push(format!("pty:{}", id.trim()));
        } else if let Some(code) = line.strip_prefix("Process exited with code ") {
            result.pty_exit = code.trim().parse().ok();
        }
    }
    result.cell_done = script
        && ["Script completed", "Script failed", "Script terminated"]
            .iter()
            .any(|s| out.starts_with(s));
    result.status = if out.starts_with("Script failed") {
        Status::Failed
    } else if out.starts_with("Script terminated") {
        Status::Stopped
    } else {
        Status::Complete
    };
    if script || has_process {
        // Direct exec_command returns JSON; functions.exec prints one JSON item per line.
        let metadata = if script {
            body
        } else if out.starts_with('{') {
            out
        } else {
            ""
        };
        for line in metadata.lines().filter(|line| line.starts_with('{')) {
            if let Some((session, exit, output)) = process_metadata(line) {
                if let Some(exit) = exit {
                    result.pty_exit = Some(exit);
                    result.result_text = output;
                } else if let Some(session) = session {
                    result.running.push(format!("pty:{session}"));
                } else {
                    result.uncertain = true;
                }
            }
        }
    }
    result
}

/// Prefer complete JSON. For a bounded prefix, parse metadata preceding `output` without
/// decoding a potentially huge escaped output string. Never search inside command stdout.
fn process_metadata(text: &str) -> Option<(Option<String>, Option<i64>, Option<String>)> {
    let mut fields = serde_json::Map::new();
    let mut incomplete_output = false;
    if let Ok(Value::Object(object)) = serde_json::from_str(text) {
        fields = object;
    } else {
        let mut rest = text.strip_prefix('{')?.trim_start();
        loop {
            let mut key = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            let key_name = key.next()?.ok()?;
            rest = rest[key.byte_offset()..].trim_start().strip_prefix(':')?.trim_start();
            if key_name == "output" {
                incomplete_output = true;
                break;
            }
            let mut value = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            let parsed = value.next()?.ok()?;
            fields.insert(key_name, parsed);
            rest = rest[value.byte_offset()..].trim_start();
            if !rest.starts_with(',') {
                break;
            }
            rest = rest[1..].trim_start();
        }
    }
    if !fields.contains_key("wall_time_seconds") && !fields.contains_key("chunk_id") {
        return incomplete_output.then_some((None, None, None));
    }
    let session = fields.get("session_id").and_then(|id| match id {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let exit = fields.get("exit_code").and_then(Value::as_i64);
    let output = match fields.remove("output") {
        Some(Value::String(output)) => Some(output),
        _ => None,
    };
    Some((session, exit, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::classify::BgAction;

    #[test]
    fn cell_wait_distinguishes_completion_failure_and_termination() {
        let ops = [ToolSemantic {
            tool: "wait".into(),
            op: ToolOp::Background {
                job_id: Some("53".into()),
                action: BgAction::Poll,
            },
        }];
        for (body, expected) in [
            (
                "Script completed\nWall time: 0.1 seconds\nOutput:\nok",
                Status::Complete,
            ),
            ("Script failed\nWall time: 0.1 seconds\nOutput:\nerror", Status::Failed),
            ("Script terminated\nWall time: 0.1 seconds\nOutput:\n", Status::Stopped),
        ] {
            let result = ToolResult {
                message: None,
                tool: "wait",
                ops: &ops,
                body,
                status: None,
                is_error: false,
                details: None,
            };
            let observation = tool_result(&result);
            assert_eq!(observation.status, expected);
            assert_eq!(observation.acting_on.as_deref(), Some("cell:53"));
            assert_eq!(observation.updates.len(), 1);
            assert!(matches!(&observation.updates[0].target, Target::Job(id) if id == "cell:53"));
            assert_eq!(observation.updates[0].status, expected);
        }
    }

    #[test]
    fn native_termination_preserves_explicit_invocation_status() {
        for (status, is_error, expected) in [
            (Some("running"), false, Status::Pending),
            (Some("failed"), false, Status::Failed),
            (None, true, Status::Failed),
        ] {
            let result = ToolResult {
                message: None,
                tool: "exec",
                ops: &[],
                body: "Script terminated",
                status,
                is_error,
                details: None,
            };
            assert_eq!(tool_result(&result).status, expected);
        }
    }

    #[test]
    fn nonzero_process_exit_remains_failed() {
        let ops = [ToolSemantic {
            tool: "write_stdin".into(),
            op: ToolOp::Background {
                job_id: Some("71".into()),
                action: BgAction::Poll,
            },
        }];
        let result = ToolResult {
            message: None,
            tool: "write_stdin",
            ops: &ops,
            body: "Process exited with code 1\nOutput:\nerror",
            status: None,
            is_error: false,
            details: None,
        };
        let observation = tool_result(&result);
        assert_eq!(observation.status, Status::Failed);
        assert_eq!(observation.updates.len(), 1);
        assert_eq!(observation.updates[0].status, Status::Failed);
    }
}
