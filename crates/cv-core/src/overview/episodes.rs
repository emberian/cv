//! Correlate successful edits and terminal validation results by tool call ID.
//! A check has to rerun the same command in the same working directory.
use crate::harness::lifecycle::{Observation, Status, ToolResult};
use crate::tools::classify::{ToolOp, ToolSemantic};
use crate::{Block, Harness, JsonView, Message, Resolver};
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Default, Serialize)]
pub struct RepairEpisodes {
    pub repairs: usize,
    pub reverted_checks: usize,
    pub recent: VecDeque<Episode>,
}

#[derive(Debug, Serialize)]
pub struct Episode {
    pub fail_msg_idx: usize,
    pub first_edit_msg_idx: usize,
    pub pass_msg_idx: usize,
    pub edits: usize,
    pub reverted_check: bool,
}

struct Replacement {
    path: String,
    before: blake3::Hash,
    after: blake3::Hash,
    replace_all: bool,
}

impl Replacement {
    fn from_input(path: &str, input: JsonView<'_>) -> Result<Option<Self>> {
        fn digest(value: JsonView<'_>) -> Result<Option<blake3::Hash>> {
            let mut hash = blake3::Hasher::new();
            let is_string = value.for_each_str_chunk(|part| {
                hash.update(part.as_bytes());
                Ok(())
            })?;
            Ok(is_string.then(|| hash.finalize()))
        }
        for (old, new) in [
            ("old_string", "new_string"),
            ("oldText", "newText"),
            ("old_str", "new_str"),
            ("TargetContent", "ReplacementContent"),
            ("oldString", "newString"),
        ] {
            if let (Some(old), Some(new)) = (input.get(old)?, input.get(new)?) {
                if let (Some(before), Some(after)) = (digest(old)?, digest(new)?) {
                    if before != after {
                        return Ok(Some(Self {
                            path: path.into(),
                            before,
                            after,
                            replace_all: input
                                .get("replace_all")?
                                .map(JsonView::as_bool)
                                .transpose()?
                                .flatten()
                                .unwrap_or(false),
                        }));
                    }
                }
            }
        }
        Ok(None)
    }
    fn reverses(&self, other: &Self) -> bool {
        self.path == other.path
            && self.replace_all == other.replace_all
            && self.before == other.after
            && self.after == other.before
    }
}

/// A plain test, check, lint, or build invocation (no shell composition).
pub fn validation(command: &str) -> bool {
    if command.len() > 4096 || command.contains(['\n', ';', '|', '&', '`', '>', '<', '$']) {
        return false;
    }
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or("").rsplit('/').next();
    match (program, words.next()) {
        (Some("pytest" | "ctest" | "jest" | "vitest" | "tsc"), _)
        | (Some("cargo"), Some("test" | "check" | "clippy" | "build" | "nextest"))
        | (Some("go"), Some("test" | "vet" | "build"))
        | (Some("npm" | "pnpm" | "yarn" | "bun" | "make"), Some("test" | "check" | "lint" | "build")) => true,
        (Some("python" | "python3"), Some("-m")) => matches!(words.next(), Some("pytest" | "unittest")),
        _ => false,
    }
}

type Check = (String, Option<String>);

struct Failure {
    index: usize,
    call_number: usize,
    completed_after_call: usize,
    last_edit_after_call: usize,
    preceding: Option<Replacement>,
    first_edit: Option<usize>,
    edits: usize,
    reverted: bool,
}

enum Action {
    Check(Check, Option<Replacement>),
    Edit(Option<Replacement>),
}

struct Call {
    number: usize,
    index: usize,
    action: Action,
    op: ToolSemantic<'static>,
}

#[derive(Default)]
pub(super) struct Detector {
    pub report: RepairEpisodes,
    calls: HashMap<String, Call>,
    failures: HashMap<Check, Failure>,
    number: usize,
    last_edit: Option<(usize, Replacement)>,
}

impl Detector {
    pub fn call(&mut self, id: &str, index: usize, input: JsonView<'_>, ops: Vec<ToolSemantic<'static>>) -> Result<()> {
        self.number += 1;
        self.calls.remove(id);
        let commit = ops.iter().any(|op| match &op.op {
            ToolOp::Command { cmd: Some(cmd), .. } => {
                crate::tools::git::git_evidence_kind(cmd) == Some(crate::tools::git::EvidenceKind::Created)
            }
            _ => false,
        });
        if commit {
            self.calls.clear();
            self.failures.clear();
            self.last_edit = None;
            return Ok(());
        }
        let single = ops.len() == 1;
        if !single && !ops.iter().all(|op| matches!(op.op, ToolOp::FileEdit { .. })) {
            return Ok(());
        }
        let Some(op) = ops.into_iter().next() else {
            return Ok(());
        };
        let action = match &op.op {
            ToolOp::Command {
                cmd: Some(cmd),
                workdir,
                background: false,
            } if validation(cmd) => {
                let preceding = self
                    .last_edit
                    .take()
                    .filter(|(n, _)| *n + 1 == self.number)
                    .map(|(_, r)| r);
                Action::Check(
                    (cmd.trim().to_owned(), workdir.as_deref().map(str::to_owned)),
                    preceding,
                )
            }
            ToolOp::FileEdit { path, .. } if single => Action::Edit(Replacement::from_input(path, input)?),
            ToolOp::FileEdit { .. } => Action::Edit(None),
            _ => return Ok(()),
        };
        self.calls.insert(
            id.into(),
            Call {
                number: self.number,
                index,
                action,
                op,
            },
        );
        Ok(())
    }

    pub fn result(
        &mut self,
        m: &Message,
        block: &Block,
        index: usize,
        harness: Harness,
        resolver: &Resolver,
    ) -> Result<()> {
        let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            status,
            details,
            ..
        } = block
        else {
            return Ok(());
        };
        let Some(call) = self.calls.remove(tool_use_id) else {
            return Ok(());
        };
        let adapter = crate::harness::rootless(harness);
        let body = if !is_error && adapter.is_some_and(|a| a.lifecycle_body_required()) {
            content.try_resolve_prefix(resolver, 8192)?
        } else {
            "".into()
        };
        let result = ToolResult {
            message: Some(m),
            tool: &call.op.tool,
            ops: std::slice::from_ref(&call.op),
            body: &body,
            status: status.as_deref(),
            is_error: *is_error,
            details: details.as_ref(),
        };
        let outcome = adapter.map_or_else(|| Observation::result(&result), |a| a.tool_result_lifecycle(&result));
        if outcome.uncertain || !outcome.started_jobs.is_empty() {
            return Ok(());
        }
        match (call.action, outcome.status) {
            (Action::Check(check, preceding), Status::Failed) => {
                self.failures.entry(check).or_insert(Failure {
                    index,
                    call_number: call.number,
                    completed_after_call: self.number,
                    last_edit_after_call: self.number,
                    preceding,
                    first_edit: None,
                    edits: 0,
                    reverted: false,
                });
            }
            (Action::Edit(replacement), Status::Complete) => {
                for fail in self
                    .failures
                    .values_mut()
                    .filter(|f| f.completed_after_call < call.number)
                {
                    fail.last_edit_after_call = self.number;
                    fail.first_edit.get_or_insert(call.index);
                    fail.edits += 1;
                    fail.reverted = fail.edits == 1
                        && call.number == fail.call_number + 1
                        && replacement
                            .as_ref()
                            .zip(fail.preceding.as_ref())
                            .is_some_and(|(a, b)| a.reverses(b));
                }
                self.last_edit = replacement.map(|r| (call.number, r));
            }
            (Action::Check(check, _), Status::Complete) => {
                // A concurrent pass started before the failure or edit completed.
                if self
                    .failures
                    .get(&check)
                    .is_some_and(|f| f.last_edit_after_call >= call.number)
                {
                    return Ok(());
                }
                if let Some(fail) = self.failures.remove(&check) {
                    if let Some(edit) = fail.first_edit {
                        if fail.reverted {
                            self.report.reverted_checks += 1
                        } else {
                            self.report.repairs += 1
                        }
                        super::recent(
                            &mut self.report.recent,
                            Episode {
                                fail_msg_idx: fail.index,
                                first_edit_msg_idx: edit,
                                pass_msg_idx: index,
                                edits: fail.edits,
                                reverted_check: fail.reverted,
                            },
                            12,
                        );
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Json, Role, Session};
    use serde_json::json;

    fn invoke(s: &mut Session, name: &str, args: serde_json::Value, fail: bool) {
        let id = format!("call-{}", s.messages.len());
        let mut call = Message::new(Role::Assistant);
        call.id = Some(id.clone());
        call.content.push(Block::ToolUse {
            id: id.clone(),
            name: name.into(),
            input: Json::from(args),
            namespace: None,
            extensions: Default::default(),
        });
        let mut result = Message::new(Role::Tool);
        result.id = Some(format!("result-{id}"));
        result.content.push(Block::ToolResult {
            tool_use_id: id,
            tool_name: Some(name.into()),
            content: "".into(),
            is_error: fail,
            status: None,
            details: None,
            extensions: Default::default(),
        });
        s.messages.extend([call, result]);
    }

    #[test]
    fn overview_repairs_exclude_verbatim_reverts_replays_and_commit_boundaries() -> Result<()> {
        // #tooth Only an observed terminal result for the same validation completes the cycle.
        let mut s = Session::new("episodes", Harness::Claude);
        let check = |s: &mut Session, fail| invoke(s, "Bash", json!({"command":"pytest tests/checksum.py"}), fail);
        let edit = |s: &mut Session, old, new| {
            invoke(
                s,
                "Edit",
                json!({"file_path":"src/checksum.py","old_string":old,"new_string":new}),
                false,
            )
        };
        check(&mut s, true);
        edit(&mut s, "bug", "fix");
        invoke(&mut s, "Bash", json!({"command":"pytest tests/other.py"}), false);
        check(&mut s, false);
        s.messages.extend(s.messages.clone());
        edit(&mut s, "fix", "mutation");
        check(&mut s, true);
        edit(&mut s, "mutation", "fix");
        check(&mut s, false);
        check(&mut s, true);
        edit(&mut s, "fix", "new");
        invoke(&mut s, "Bash", json!({"command":"git commit -am fix"}), false);
        check(&mut s, false);
        check(&mut s, true);
        invoke(
            &mut s,
            "Edit",
            json!({"file_path":"x","old_string":"y","new_string":"z"}),
            true,
        );
        check(&mut s, false);
        let opts = crate::overview::OverviewOptions {
            command_limit: 0,
            ..Default::default()
        };
        let report = crate::overview::from_session(&s, &opts)?.repair_episodes;
        assert_eq!((report.repairs, report.reverted_checks), (1, 1));
        assert_eq!(report.recent.len(), 2);
        assert_eq!(
            (
                report.recent[0].fail_msg_idx,
                report.recent[0].first_edit_msg_idx,
                report.recent[0].pass_msg_idx
            ),
            (1, 2, 7)
        );
        assert!(report.recent[1].reverted_check);
        Ok(())
    }
}
