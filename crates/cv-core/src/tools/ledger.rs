//! Shared tool-call pairing, operator-ask tracking, and edit/validation state machine.

use crate::affordance::Kind;
use crate::harness::lifecycle::{Observation, ToolResult as LifecycleResult};
use crate::ir::{Harness, Message};
use crate::tools::classify::{ToolOp, ToolSemantic};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

static GATE_RES: [OnceLock<Regex>; 3] = [const { OnceLock::new() }; 3];

/// A plain single test, check, lint, or build invocation (no shell composition).
pub fn validation_command(command: &str) -> bool {
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

/// Whether `command` runs a build or test gate whose result is a verdict on the working tree.
pub fn is_gate_command(command: &str) -> bool {
    GATE_RES[0]
        .get_or_init(|| {
            Regex::new(
                r"\blake\s+(?:build|env\s+lean|test)\b|\bcargo\s+(?:build|test|nextest|check|clippy)\b|swarm-build|\bmake\b|\bpytest\b|\bnpm\s+(?:run\s+)?test\b|\bvitest\b|\bjest\b|\btsc\b|\bgo\s+test\b|clean-build|local-gates|\bctest\b|\bninja\b|\brc=",
            )
            .expect("static regex")
        })
        .is_match(command)
}

/// Whether build/test output reports a failure its tool did not flag with `is_error = true`.
pub fn gate_failed(output: &str) -> bool {
    GATE_RES[1]
        .get_or_init(|| {
            Regex::new(
                r"(?m)test result: FAILED|^error(?:\[E\d+\])?:|^FAILED\b|\b[1-9]\d* failed\b|^FAIL\b|Exit code [1-9]",
            )
            .expect("static regex")
        })
        .is_match(output)
}

/// Extract the salient trailing verdict line from a build/test gate output.
pub fn gate_verdict(output: &str) -> &str {
    let re = GATE_RES[2].get_or_init(|| {
        Regex::new(
            r"(?i)\berror\b|\bfail|\bpassed\b|\bsuccess|\bcompleted\b|\brc=\d|exit code|✔|✖|\bbuilt\b|test result|declaration uses 'sorry'",
        )
        .expect("static regex")
    });
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| re.is_match(l))
        .or_else(|| output.lines().rev().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("(no output)")
}

/// Prose files whose distinctive names are markdown headings rather than source-code definitions.
pub fn is_prose_path(path: &str) -> bool {
    let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    matches!(
        base.rsplit_once('.')
            .map_or("", |(_, e)| e)
            .to_ascii_lowercase()
            .as_str(),
        "md" | "markdown" | "txt" | "rst" | "adoc" | "org"
    )
}

#[allow(clippy::too_many_arguments)]
pub fn observe_tool_result(
    harness: Harness,
    message: Option<&Message>,
    tool: &str,
    ops: &[ToolSemantic<'static>],
    body: &str,
    status: Option<&str>,
    is_error: bool,
    details: Option<&serde_json::Value>,
) -> Observation {
    let res = LifecycleResult {
        message,
        tool,
        ops,
        body,
        status,
        is_error,
        details,
    };
    crate::harness::rootless(harness).map_or_else(|| Observation::result(&res), |a| a.tool_result_lifecycle(&res))
}

/// Open `ToolUse` call IDs and operator questions/approvals, reset at compaction boundaries.
#[derive(Default, Debug, Clone)]
pub struct CallTracker {
    pub open: HashSet<String>,
    pub asks: HashSet<String>,
}

impl CallTracker {
    pub fn clear(&mut self) {
        self.open.clear();
        self.asks.clear();
    }

    pub fn open_call(&mut self, id: &str, ops: &[ToolSemantic<'_>]) {
        if id.is_empty() {
            return;
        }
        self.open.insert(id.to_owned());
        if ops.iter().any(|s| {
            matches!(
                s.op,
                ToolOp::Affordance {
                    kind: Kind::Question | Kind::Approval,
                    ..
                }
            )
        }) {
            self.asks.insert(id.to_owned());
        }
    }

    pub fn close_call(&mut self, id: &str) {
        self.open.remove(id);
        self.asks.remove(id);
    }
}

/// Unified verification state of a trajectory's edit/gate sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationState {
    Unvalidated,
    VerifiedClean,
    UnverifiedEdit,
    FailingValidation,
    PostFailureEdit,
}

impl VerificationState {
    pub fn of(last_passed: Option<bool>, edits_after: usize) -> Self {
        match (last_passed, edits_after) {
            (None, _) => Self::Unvalidated,
            (Some(true), 0) => Self::VerifiedClean,
            (Some(true), _) => Self::UnverifiedEdit,
            (Some(false), 0) => Self::FailingValidation,
            (Some(false), _) => Self::PostFailureEdit,
        }
    }

    pub fn from_steps(steps: &[(usize, LedgerStep)]) -> Self {
        let Some(at) = steps
            .iter()
            .rposition(|(_, s)| matches!(s, LedgerStep::Validate { .. }))
        else {
            return Self::Unvalidated;
        };
        let passed = matches!(steps[at].1, LedgerStep::Validate { passed: true, .. });
        let edits = steps[at + 1..]
            .iter()
            .filter(|(_, s)| matches!(s, LedgerStep::Edit(_)))
            .count();
        Self::of(Some(passed), edits)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationRun {
    pub index: usize,
    pub tool_use_id: String,
    pub command: String,
    pub passed: bool,
}

/// O(1)-memory streaming reducer tracking open gate commands, validations, and post-run edits.
#[derive(Default, Debug, Clone)]
pub struct GateLedger {
    open: HashMap<String, String>,
    pub last: Option<ValidationRun>,
    pub edits_after: usize,
    pub edited_paths: Vec<String>,
}

impl GateLedger {
    pub fn observe_ops(&mut self, id: &str, ops: &[ToolSemantic<'_>]) {
        let mut gate = None;
        for sem in ops {
            match &sem.op {
                ToolOp::Command { cmd: Some(cmd), .. } if is_gate_command(cmd) => {
                    gate.get_or_insert_with(|| cmd.as_ref().to_owned());
                }
                ToolOp::FileEdit { path, .. } if !is_prose_path(path) => {
                    self.edits_after += 1;
                    if self.edited_paths.len() < 5 && !self.edited_paths.iter().any(|p| p == path.as_ref()) {
                        self.edited_paths.push(path.as_ref().to_owned());
                    }
                }
                _ => {}
            }
        }
        if let Some(cmd) = gate {
            self.open.insert(id.to_owned(), cmd);
        }
    }

    pub fn take_open(&mut self, id: &str) -> Option<String> {
        self.open.remove(id)
    }

    pub fn finish_gate(&mut self, index: usize, id: &str, command: String, passed: bool) {
        self.last = Some(ValidationRun {
            index,
            tool_use_id: id.to_owned(),
            command,
            passed,
        });
        self.edits_after = 0;
        self.edited_paths.clear();
    }

    pub fn state(&self) -> VerificationState {
        VerificationState::of(self.last.as_ref().map(|v| v.passed), self.edits_after)
    }
}

/// One step in the ordered edit/validation history retained for context distillation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerStep {
    Edit(String),
    Validate {
        label: String,
        verdict: String,
        passed: bool,
    },
}
