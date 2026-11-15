//! Explicit Codex cell/PTY envelopes, kept separate from command stdout.
use super::ToolInput;
use crate::tools::classify::ToolOp;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct Jobs {
    owners: HashMap<String, String>,
    uncertain: HashSet<String>,
}

#[derive(Default)]
pub(super) struct Outcome {
    pub pending: bool,
    pub failed: bool,
    pub completed_call: Option<String>,
    pub result_text: Option<String>,
}

#[derive(Default)]
struct Envelope {
    running: Vec<String>,
    cell_done: bool,
    pty_exit: Option<i64>,
    failed: bool,
    uncertain: bool,
    result_text: Option<String>,
}

impl Jobs {
    pub fn pending_calls(&self) -> impl Iterator<Item = &str> {
        self.owners.values().chain(self.uncertain.iter()).map(String::as_str)
    }

    pub fn observe(&mut self, id: &str, input: &ToolInput<'_>, out: &str) -> Outcome {
        let poll = poll_job(input);
        let owner = poll
            .as_ref()
            .and_then(|job| self.owners.get(job))
            .cloned()
            .unwrap_or_else(|| id.to_owned());
        let has_process = input
            .ops
            .iter()
            .any(|op| matches!(op.op, ToolOp::Command { .. }) || op.tool.ends_with("write_stdin"));
        let envelope = envelope(out, has_process);
        for job in &envelope.running {
            self.owners.entry(job.clone()).or_insert_with(|| owner.clone());
        }
        if envelope.uncertain {
            self.uncertain.insert(owner);
        }
        let mut completed_call = None;
        if let Some(job) = poll {
            let done = if job.starts_with("cell:") {
                envelope.cell_done
            } else {
                envelope.pty_exit.is_some()
            };
            if done {
                if let Some(owner) = self.owners.remove(&job) {
                    if !self.owners.values().any(|id| id == &owner) && !self.uncertain.contains(&owner) {
                        completed_call = Some(owner);
                    }
                }
            }
        }
        Outcome {
            pending: !envelope.running.is_empty() || envelope.uncertain,
            failed: envelope.failed || envelope.pty_exit.is_some_and(|code| code != 0),
            completed_call,
            result_text: envelope.result_text,
        }
    }
}

fn poll_job(input: &ToolInput<'_>) -> Option<String> {
    let mut jobs = input.ops.iter().filter_map(|op| {
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
    result.failed = script && (out.starts_with("Script failed") || out.starts_with("Script terminated"));
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
