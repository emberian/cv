//! Native Codex cell and PTY lifecycle envelopes.
use crate::harness::lifecycle::{Observation, Status, Target, ToolResult, Update};
use crate::tools::classify::{ToolOp, ToolSemantic};
use serde_json::Value;

pub(super) fn tool_result(result: &ToolResult<'_>) -> Observation {
    let mut observation = Observation::result(result);
    let steps = steps(result.ops);
    // A process this invocation starts beside a poll is its own, not the polled job's child.
    let acting_on = poll_job(result.ops).filter(|_| !steps.iter().any(|step| matches!(step, Step::Run)));
    let envelope = envelope(result.body, !steps.is_empty());
    // One native result per process call, in call order, lets each be read against its own
    // call. A result that names a session must name the one its call polled.
    let aligned = !envelope.results.is_empty()
        && envelope.results.len() == steps.len()
        && steps
            .iter()
            .zip(&envelope.results)
            .all(|(step, native)| match (step, &native.session) {
                (Step::Poll(polled), Some(session)) => polled == session,
                _ => true,
            });
    let own: Vec<&Native> = if aligned {
        let own = steps.iter().zip(&envelope.results);
        own.filter(|(step, _)| matches!(step, Step::Run))
            .map(|(_, native)| native)
            .collect()
    } else {
        envelope.results.iter().collect()
    };
    let polls = steps.iter().filter(|step| matches!(step, Step::Poll(_))).count();
    // An exit belongs to this invocation's own command only where no polled process could
    // have produced it: a poll's result carries the polled process's exit, never the call's.
    let own_exit = own
        .iter()
        .rev()
        .find_map(|native| native.exit)
        .filter(|_| aligned || polls == 0);
    let status = match envelope.header_exit.or(own_exit) {
        Some(code) if code != 0 => Status::Failed,
        _ => envelope.status,
    };
    if observation.status == Status::Complete {
        observation.status = status;
    }
    if aligned {
        for (step, native) in steps.iter().zip(&envelope.results) {
            if let (Step::Poll(polled), Some(code)) = (step, native.exit) {
                observation.updates.push(Update {
                    target: Target::Job(format!("pty:{polled}")),
                    status: if code == 0 { Status::Complete } else { Status::Failed },
                    output: native.output.clone(),
                });
            }
        }
    }
    if let Some(job) = &acting_on {
        let done = if job.starts_with("cell:") {
            envelope.cell_done
        } else {
            envelope.header_exit.is_some()
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
    let outputs: Vec<&str> = own.iter().filter_map(|native| native.output.as_deref()).collect();
    observation.output = (!outputs.is_empty()).then(|| outputs.join("\n"));
    observation
}

/// A process call inside one invocation, in call order.
enum Step {
    Run,
    Poll(String),
}

/// One native `exec_command`/`write_stdin` result object.
struct Native {
    session: Option<String>,
    exit: Option<i64>,
    output: Option<String>,
}

#[derive(Default)]
struct Envelope {
    running: Vec<String>,
    cell_done: bool,
    /// `Process exited with code N` in a native text header: the polled process's own exit.
    header_exit: Option<i64>,
    results: Vec<Native>,
    status: Status,
    uncertain: bool,
}

fn steps(ops: &[ToolSemantic<'_>]) -> Vec<Step> {
    ops.iter()
        .filter_map(|op| match &op.op {
            ToolOp::Command { .. } => Some(Step::Run),
            ToolOp::Background { job_id: Some(id), .. } if op.tool.ends_with("write_stdin") => {
                Some(Step::Poll(id.to_string()))
            }
            _ => None,
        })
        .collect()
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
            result.header_exit = code.trim().parse().ok();
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
        for line in metadata.lines().filter(|line| line.starts_with(['{', '['])) {
            let before = result.results.len();
            match serde_json::from_str::<Value>(line) {
                Ok(value) => natives(value, 0, &mut result.results),
                Err(_) => match bounded_metadata(line) {
                    Some(Some(native)) => result.results.push(native),
                    Some(None) => result.uncertain = true,
                    None => {}
                },
            }
            for native in &result.results[before..] {
                match (&native.session, native.exit) {
                    (Some(session), None) => result.running.push(format!("pty:{session}")),
                    (None, None) => result.uncertain = true,
                    _ => {}
                }
            }
        }
    }
    result
}

/// Native result objects in document order. A script may print them bare or inside its own
/// wrapping (`Promise.allSettled` items, indexed rows); the wrapping decides nothing.
fn natives(value: Value, depth: usize, found: &mut Vec<Native>) {
    match value {
        Value::Object(fields) if fields.contains_key("wall_time_seconds") || fields.contains_key("chunk_id") => {
            found.push(native(fields));
        }
        Value::Object(fields) if depth < 3 => {
            for (_, value) in fields {
                natives(value, depth + 1, found);
            }
        }
        Value::Array(items) if depth < 3 => {
            for value in items {
                natives(value, depth + 1, found);
            }
        }
        _ => {}
    }
}

fn native(mut fields: serde_json::Map<String, Value>) -> Native {
    Native {
        session: fields.get("session_id").and_then(|id| match id {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }),
        exit: fields.get("exit_code").and_then(Value::as_i64),
        output: match fields.remove("output") {
            Some(Value::String(output)) => Some(output),
            _ => None,
        },
    }
}

/// A line cut short of valid JSON: parse metadata preceding `output` without decoding a
/// potentially huge escaped output string. Never search inside command stdout.
/// `Some(None)`: a result whose identity the cut hides.
fn bounded_metadata(text: &str) -> Option<Option<Native>> {
    let mut fields = serde_json::Map::new();
    let mut incomplete_output = false;
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
    if !fields.contains_key("wall_time_seconds") && !fields.contains_key("chunk_id") {
        return incomplete_output.then_some(None);
    }
    Some(Some(native(fields)))
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

    fn script(calls: &[(&'static str, Option<&'static str>)]) -> Vec<ToolSemantic<'static>> {
        calls
            .iter()
            .map(|(tool, session)| ToolSemantic {
                tool: (*tool).into(),
                op: match session {
                    Some(session) => ToolOp::Background {
                        job_id: Some((*session).into()),
                        action: BgAction::Poll,
                    },
                    None => ToolOp::Command {
                        cmd: Some("ps".into()),
                        workdir: None,
                        background: false,
                    },
                },
            })
            .collect()
    }

    fn observe(ops: &[ToolSemantic<'static>], lines: &[&str]) -> Observation {
        let body = format!(
            "Script completed\nWall time 7.2 seconds\nOutput:\n\n{}",
            lines.join("\n")
        );
        tool_result(&ToolResult {
            message: None,
            tool: "exec",
            ops,
            body: &body,
            status: None,
            is_error: false,
            details: None,
        })
    }

    const RUNNING: &str =
        r#"{"chunk_id":"a","wall_time_seconds":5.0,"session_id":66592,"original_token_count":0,"output":""}"#;
    const PS_OK: &str =
        r#"{"chunk_id":"b","wall_time_seconds":1.9,"exit_code":0,"original_token_count":9,"output":"63 lean"}"#;
    const BUILD_FAILED: &str = r#"{"chunk_id":"c","wall_time_seconds":0.1,"exit_code":2,"original_token_count":3,"output":"error: build failed"}"#;

    #[test]
    fn another_commands_exit_never_ends_the_polled_process() {
        // Real shape: `write_stdin(66592)` then `exec_command(ps)` in one script.
        let ops = script(&[("write_stdin", Some("66592")), ("exec_command", None)]);
        let observation = observe(&ops, &[RUNNING, PS_OK]);
        assert!(observation.updates.is_empty(), "the polled process is still running");
        assert!(
            observation.acting_on.is_none(),
            "a session the script's own command starts is this call's"
        );
        assert_eq!(observation.started_jobs, ["pty:66592"]);
        assert_eq!(observation.status, Status::Complete);
        assert_eq!(observation.output.as_deref(), Some("63 lean"));
        // The polled process's own failure is its update, not the invocation's status.
        let observation = observe(&ops, &[BUILD_FAILED, PS_OK]);
        assert_eq!(observation.updates.len(), 1);
        assert!(matches!(&observation.updates[0].target, Target::Job(id) if id == "pty:66592"));
        assert_eq!(observation.updates[0].status, Status::Failed);
        assert_eq!(observation.updates[0].output.as_deref(), Some("error: build failed"));
        assert_eq!(observation.status, Status::Complete);
        assert_eq!(observation.output.as_deref(), Some("63 lean"));
    }

    #[test]
    fn compound_polls_settle_only_when_every_result_aligns_with_its_call() {
        let ops = script(&[("write_stdin", Some("66592")), ("write_stdin", Some("7"))]);
        let exited = |observation: &Observation| -> Vec<(String, Status)> {
            let updates = observation.updates.iter();
            updates
                .map(|update| match &update.target {
                    Target::Job(id) | Target::Call(id) => (id.clone(), update.status),
                })
                .collect()
        };
        let other_running = RUNNING.replace("66592", "7");
        assert_eq!(
            exited(&observe(&ops, &[RUNNING, BUILD_FAILED])),
            [("pty:7".to_owned(), Status::Failed)]
        );
        assert_eq!(
            exited(&observe(&ops, &[PS_OK, &other_running])),
            [("pty:66592".to_owned(), Status::Complete)]
        );
        // The same items inside a script's own wrapping.
        let settled = format!(r#"{{"i":0,"result":{{"status":"fulfilled","value":{PS_OK}}}}}"#);
        let wrapped_running = format!(r#"{{"status":"fulfilled","value":{other_running}}}"#);
        let wrapped = observe(&ops, &[&settled, &wrapped_running]);
        assert_eq!(exited(&wrapped), [("pty:66592".to_owned(), Status::Complete)]);
        assert_eq!(wrapped.started_jobs, ["pty:7"]);
        // Fewer or more results than calls, or a session where another was polled: nothing is
        // attributed.
        for lines in [
            vec![PS_OK],
            vec![other_running.as_str(), PS_OK],
            vec![PS_OK, PS_OK, PS_OK],
        ] {
            let observation = observe(&ops, &lines);
            assert!(observation.updates.is_empty(), "{lines:?}");
            assert_eq!(observation.status, Status::Complete, "{lines:?}");
        }
        // A line cut before its identity is still that call's result; what it hides stays open.
        let cut = observe(
            &ops,
            &[PS_OK, r#"{"chunk_id":"d","wall_time_seconds":1.0,"output":"cut"#],
        );
        assert_eq!(exited(&cut), [("pty:66592".to_owned(), Status::Complete)]);
        assert!(cut.uncertain);
        let hidden = observe(&ops, &[PS_OK, r#"{"output":"identity hidden"#]);
        assert!(hidden.updates.is_empty());
        assert!(hidden.uncertain);
    }

    #[test]
    fn every_own_commands_output_reaches_facts_and_the_last_exit_is_the_status() {
        let ops = script(&[("exec_command", None), ("exec_command", None)]);
        let observation = observe(&ops, &[PS_OK, BUILD_FAILED]);
        assert_eq!(observation.status, Status::Failed);
        assert_eq!(observation.output.as_deref(), Some("63 lean\nerror: build failed"));
        assert!(observation.updates.is_empty());
    }
}
