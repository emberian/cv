//! Wire types for the task substrate: the durable event grammar and derived state enums.
//!
//! The JSON shapes here are a **wire contract**: events are appended to
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` and replayed forever, so tags and field names must
//! stay stable. Additions are fine; renames are not. The round-trip and pinned-JSON tests at the
//! bottom of this file are the tripwire.
//!
//! Lineage: the revision (land-facet) grammar is ported from mission-control's
//! `ramspace-core/src/land_request.rs` pure reducer — the one module of that system worth keeping.
//! Deliberate deviations from it are documented in [`crate::task::reduce`].

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::Harness;

/// The `by` identity the cv-side git verifier stamps on verifier-only events. Not authority —
/// anyone can type it — but replay warns when a verifier-only event carries any *other* author,
/// and the periodic pass re-observes Landed revisions, so a forged land is loud and self-defeating.
pub const VERIFIER_BY: &str = "cv-verify";

/// A single durable task event. One per line of `tasks/events.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    /// Unique, time-sortable event id (uuid v7). Idempotency key: re-appending a byte-identical
    /// event is a no-op; a *different* event reusing an id is a loud error.
    pub id: String,
    /// The task this event belongs to. For `Opened`, `task_id == id`.
    pub task_id: String,
    /// When the event was appended.
    pub ts: DateTime<Utc>,
    /// Who appended it (board `from` convention: agent/session/human identifier).
    pub by: String,
    /// What happened.
    #[serde(flatten)]
    pub kind: TaskEventKind,
}

/// The event grammar. Base lifecycle events apply to every task; the land facet
/// (`RevisionProposed` onward) applies only to code tasks that propose reviewed revisions.
///
/// Verifier-only kinds (`SourceUnavailable`, `MergeFailed`, `MergedLocal`, `ReconcileFailed`,
/// `Landed`) may only be emitted by the cv-side git verifier — law 1: **observed, not attested**.
/// The store's agent-facing append path rejects them; see [`TaskEventKind::is_verifier_only`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEventKind {
    // ── base lifecycle ──────────────────────────────────────────────────────
    Opened {
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue: Option<String>,
        /// Board channel task notifications post to.
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
    },
    Claimed {
        assignee: String,
    },
    Released {},
    /// Progress note; never changes state.
    Noted {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
    },
    /// Non-code completion, with optional pointer to observable evidence.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
        /// How cv verified the completion, when a check was attached and it PASSED. Only a passing
        /// check is ever recorded — a failing check refuses the `Done`, so the task stays open and
        /// no event is written. `None` = self-report (the honest fallback), and the provenance
        /// layer labels it as such. Additive optional: pre-check logs replay unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<DoneCheck>,
    },
    Abandoned {
        reason: String,
    },
    Superseded {
        by_task: String,
    },

    // ── land facet (revision-scoped) ────────────────────────────────────────
    /// Attach a reviewed code revision. Proposing again supersedes the prior revision — that is
    /// the only cure for a refute (a REFUTE on a revision is terminal for that revision).
    RevisionProposed {
        revision: Revision,
    },
    ReviewRerouted {
        from: String,
        to: String,
    },
    ReviewPassed {
        reviewer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
        /// Advisory reviewer-independence observation (recorded, never a gate).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        independence: Option<IndependenceCheck>,
        /// Advisory reviewer-receipts observation (recorded, never a gate). Additive optional:
        /// events appended before this field existed replay unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipts: Option<ReviewReceipts>,
    },
    ReviewRefuted {
        reviewer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
        /// Advisory reviewer-receipts observation (recorded, never a gate). Additive optional.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipts: Option<ReviewReceipts>,
    },

    // ── verifier-only (law 1: observed, not attested) ───────────────────────
    SourceUnavailable {
        detail: String,
    },
    MergeFailed {
        reason: MergeFailure,
    },
    MergedLocal {
        from_sha: String,
        to_sha: String,
    },
    ReconcileFailed {
        detail: String,
    },
    Landed {
        upstream_head: String,
        observed_patch_id: String,
    },
}

impl TaskEventKind {
    /// Kinds only the git verifier may emit. Agent-facing append paths (CLI verbs other than
    /// `verify`, every MCP tool) must reject these at the store seam.
    pub fn is_verifier_only(&self) -> bool {
        matches!(
            self,
            TaskEventKind::SourceUnavailable { .. }
                | TaskEventKind::MergeFailed { .. }
                | TaskEventKind::MergedLocal { .. }
                | TaskEventKind::ReconcileFailed { .. }
                | TaskEventKind::Landed { .. }
        )
    }

    /// Kinds whose `by` endpoint carries semantics (keys inbox, reviewer, and land bookkeeping),
    /// matching the identity-BEARING verbs claim/release/propose/pass/refute (see
    /// [`crate::task::require_actor`]). The store's TOFU token gate ([`crate::task::identity`])
    /// applies only to these: a bound endpoint must present its token to stamp one of them, and a
    /// first-use token binds on one of them. Bookkeeping kinds (open/note/done/abandon/supersede/
    /// reroute) stay token-optional by design — impersonating them changes no lifecycle authority.
    pub fn is_identity_bearing(&self) -> bool {
        matches!(
            self,
            TaskEventKind::Claimed { .. }
                | TaskEventKind::Released {}
                | TaskEventKind::RevisionProposed { .. }
                | TaskEventKind::ReviewPassed { .. }
                | TaskEventKind::ReviewRefuted { .. }
        )
    }

    /// The wire tag for this kind (the serde `event` field), for logs and board notifications.
    pub fn tag(&self) -> &'static str {
        match self {
            TaskEventKind::Opened { .. } => "opened",
            TaskEventKind::Claimed { .. } => "claimed",
            TaskEventKind::Released {} => "released",
            TaskEventKind::Noted { .. } => "noted",
            TaskEventKind::Done { .. } => "done",
            TaskEventKind::Abandoned { .. } => "abandoned",
            TaskEventKind::Superseded { .. } => "superseded",
            TaskEventKind::RevisionProposed { .. } => "revision_proposed",
            TaskEventKind::ReviewRerouted { .. } => "review_rerouted",
            TaskEventKind::ReviewPassed { .. } => "review_passed",
            TaskEventKind::ReviewRefuted { .. } => "review_refuted",
            TaskEventKind::SourceUnavailable { .. } => "source_unavailable",
            TaskEventKind::MergeFailed { .. } => "merge_failed",
            TaskEventKind::MergedLocal { .. } => "merged_local",
            TaskEventKind::ReconcileFailed { .. } => "reconcile_failed",
            TaskEventKind::Landed { .. } => "landed",
        }
    }
}

/// How a non-code completion was verified: the kind of check cv RAN at `done` time and the
/// observed result. Recorded on a [`TaskEventKind::Done`] only when the check PASSED — this is the
/// structural difference between a self-reported completion and one cv itself observed. Extends
/// law 1 (observed, not attested) to revision-less tasks: the completion predicate is a check cv
/// executes, not free text an agent types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DoneCheck {
    /// What was checked, and against what target (the command, path, or url).
    #[serde(flatten)]
    pub kind: DoneCheckKind,
    /// A short human summary of the observed pass (e.g. `exit 0`, `exists, 42 bytes`, `200 OK`).
    pub result: String,
}

/// The kinds of completion check cv can run. Tagged on the wire by `check`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum DoneCheckKind {
    /// A shell command cv ran (in the task's repo dir if it has one, else cwd); exit 0 = pass.
    Cmd { cmd: String },
    /// A filesystem path cv confirmed exists and is non-empty.
    File { path: PathBuf },
    /// A URL cv issued a GET against; a 2xx status = pass.
    Http { url: String },
}

/// A reviewed code revision. Identity-bearing fields are `review_sha` and `patch_id` (the
/// **range** patch-id: cumulative diff from merge-base to the review sha); `branch`/`worktree`
/// are locators, never identity. Both sha and patch-id are computed by cv at propose time by
/// running git — never typed in by an agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    /// 1-based revision number within the task.
    pub n: u32,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// The ref the revision must reach to count as landed, e.g. `origin/main`.
    pub upstream: String,
    /// The merge-base of `upstream` and `review_sha` **at propose time**. Recorded so the range
    /// patch-id stays recomputable between two fixed commits forever — after a direct-FF land,
    /// a live `merge-base(upstream, sha)` would equal `sha` and the range would vanish.
    pub base: String,
    /// Full 40-hex review commit sha.
    pub review_sha: String,
    /// 40-hex range patch-id (`git diff <base> <review_sha> | git patch-id --stable`).
    pub patch_id: String,
    /// Active reviewer endpoint. Reroute is the only reassignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// The author's session (cv session id) that produced this revision, when known — the
    /// author-side input to the advisory independence check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
}

/// Why a verification pass could not observe the revision as merged/landed. These are
/// **verification findings** (observations about the world), not merge-attempt outcomes —
/// cv never runs a merge on anyone's behalf.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MergeFailure {
    NonFastForward {},
    MissingWorktree {},
    MissingBranch {},
    BranchTipChanged {},
    GitFailed { detail: String },
}

impl MergeFailure {
    pub fn describe(&self) -> String {
        match self {
            MergeFailure::NonFastForward {} => "upstream moved; not fast-forwardable".to_string(),
            MergeFailure::MissingWorktree {} => "worktree missing".to_string(),
            MergeFailure::MissingBranch {} => "branch missing".to_string(),
            MergeFailure::BranchTipChanged {} => "branch tip no longer the reviewed sha".to_string(),
            MergeFailure::GitFailed { detail } => format!("git failed: {detail}"),
        }
    }
}

/// Advisory reviewer-independence observation, read from transcripts at pass time (law 2).
/// `None` fields mean "could not determine" — recorded and warned about, never a gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndependenceCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub independent: Option<bool>,
}

/// Advisory reviewer-receipts observation, read from the reviewer's transcript at verdict time
/// (law 2's posture: observed and recorded, never a gate). Every field is `Option` because every
/// field is an honest observation: `None` means "could not determine", never a guess.
///
/// These are a **heuristic signal, not proof**: structural observations over tool inputs that raise
/// the cost of a forged review, never guarantee *review*. `saw_change` requires the reviewer to
/// have actually engaged the change (a repo-path read or a real `git diff/show/log`), not merely
/// quoted its sha — but it stays gameable by an adversary who pointlessly opens a repo file. The
/// bar is raised past the trivial echo, not made a guarantee.
///
/// Semantics when recorded (a reviewer session id was provided):
/// - all fields `None`: the session was named but could not be found/read — undetermined.
/// - `saw_change`: the reviewer's engagement with the change, over an evidence hierarchy in
///   `crate::task::scan_session` — `Some(true)` for structural contact (a content read under the
///   repo, or a `git diff`/`git show`/`git log` invocation); `None` when only the change's identity
///   (sha/branch/repo path) appears as a bare argument (the `echo <sha>` case — undetermined, not a
///   pass); `Some(false)` when a revision existed and the scan saw no contact at all.
/// - `ran_checks`: whether any tool input runs a known test/build command in command position
///   ([`crate::task::CHECK_COMMAND_PATTERNS`], not merely quoted). Heuristic and deliberately narrow.
/// - `turns`: the number of assistant messages in the session — a cheap effort signal.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewReceipts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saw_change: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
}

/// Base lifecycle state of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Claimed,
    Done,
    Abandoned,
    Superseded,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Abandoned | TaskState::Superseded)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
            TaskState::Abandoned => "abandoned",
            TaskState::Superseded => "superseded",
        }
    }
}

/// Land-facet state of a revision. Ported from mission-control's `LandRequestState`:
/// `MergedLocal` is actionable, **not** terminal — a local merge is not a land until the
/// verifier observes the reviewed patch on the upstream ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionState {
    AwaitingReview,
    Ready,
    Refuted,
    Superseded,
    MergedLocal,
    Landed,
}

impl RevisionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RevisionState::Refuted | RevisionState::Superseded | RevisionState::Landed
        )
    }

    pub fn is_actionable(self) -> bool {
        matches!(self, RevisionState::Ready | RevisionState::MergedLocal)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RevisionState::AwaitingReview => "awaiting_review",
            RevisionState::Ready => "ready",
            RevisionState::Refuted => "refuted",
            RevisionState::Superseded => "superseded",
            RevisionState::MergedLocal => "merged_local",
            RevisionState::Landed => "landed",
        }
    }
}

/// Model family behind a harness, for the advisory independence check. Multi-model harnesses
/// (IDE extensions, local runners) return `None` — undeterminable at harness granularity;
/// `task::independence_check` then falls back to reading the session's per-message `model` ids
/// and mapping them through [`model_family`].
pub fn harness_family(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::Claude | Harness::ClaudeApp | Harness::ClaudeExport => Some("anthropic"),
        Harness::Codex | Harness::ChatGptApp | Harness::ChatGptExport => Some("openai"),
        Harness::Gemini => Some("google"),
        Harness::Grok => Some("xai"),
        Harness::Qwen => Some("alibaba"),
        Harness::Kimi | Harness::KimiCode => Some("moonshot"),
        Harness::Hermes => Some("nous"),
        // Model-agnostic multiplexers: could be running anything. `OpenSession` joins them
        // because a document names the harness it came from — by the time a session is one, the
        // family is whatever that harness was, and the wrapper itself says nothing.
        Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::OpenSession => None,
    }
}

/// Model family from a model **id** (the per-message `model` field), for sessions whose harness
/// is multi-model. Dumb case-insensitive prefix match on the major providers' id conventions;
/// unknown ids are `None` — recorded as undetermined, never guessed.
pub fn model_family(model_id: &str) -> Option<&'static str> {
    let m = model_id.trim().to_ascii_lowercase();
    const PREFIXES: [(&str, &str); 8] = [
        ("claude", "anthropic"),
        ("gpt", "openai"),
        ("gemini", "google"),
        ("grok", "xai"),
        ("qwen", "alibaba"),
        ("kimi", "moonshot"),
        ("deepseek", "deepseek"),
        ("llama", "meta"),
    ];
    for (prefix, family) in PREFIXES {
        if m.starts_with(prefix) {
            return Some(family);
        }
    }
    // OpenAI's o-series: "o1", "o3-mini", "o4" — an 'o' followed by a digit.
    if m.starts_with('o') && m.as_bytes().get(1).is_some_and(u8::is_ascii_digit) {
        return Some("openai");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: TaskEventKind) -> TaskEvent {
        TaskEvent {
            id: "0198c0de-0000-7000-8000-000000000001".to_string(),
            task_id: "0198c0de-0000-7000-8000-000000000000".to_string(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: "agent:test".to_string(),
            kind,
        }
    }

    fn revision() -> Revision {
        Revision {
            n: 1,
            branch: "task/demo".to_string(),
            worktree: None,
            upstream: "origin/main".to_string(),
            base: "0".repeat(40),
            review_sha: "a".repeat(40),
            patch_id: "b".repeat(40),
            reviewer: Some("agent:reviewer".to_string()),
            session_ref: None,
        }
    }

    #[test]
    fn every_kind_round_trips() {
        let kinds = vec![
            TaskEventKind::Opened {
                title: "t".into(),
                body: "b".into(),
                repo: Some(PathBuf::from("/tmp/x")),
                issue: Some("#1".into()),
                channel: "tasks".into(),
                assignee: None,
            },
            TaskEventKind::Claimed { assignee: "a".into() },
            TaskEventKind::Released {},
            TaskEventKind::Noted {
                text: "n".into(),
                session_ref: Some("s".into()),
            },
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
            TaskEventKind::Done {
                observed: Some("exit 0".into()),
                check: Some(DoneCheck {
                    kind: DoneCheckKind::Cmd {
                        cmd: "cargo test".into(),
                    },
                    result: "exit 0".into(),
                }),
            },
            TaskEventKind::Abandoned { reason: "r".into() },
            TaskEventKind::Superseded { by_task: "t2".into() },
            TaskEventKind::RevisionProposed { revision: revision() },
            TaskEventKind::ReviewRerouted {
                from: "a".into(),
                to: "b".into(),
            },
            TaskEventKind::ReviewPassed {
                reviewer: "b".into(),
                session_ref: None,
                independence: Some(IndependenceCheck {
                    author_family: Some("anthropic".into()),
                    reviewer_family: Some("openai".into()),
                    independent: Some(true),
                }),
                receipts: Some(ReviewReceipts {
                    saw_change: Some(true),
                    ran_checks: Some(false),
                    turns: Some(14),
                }),
            },
            TaskEventKind::ReviewRefuted {
                reviewer: "b".into(),
                session_ref: None,
                receipts: Some(ReviewReceipts {
                    saw_change: None,
                    ran_checks: None,
                    turns: Some(2),
                }),
            },
            TaskEventKind::SourceUnavailable { detail: "gone".into() },
            TaskEventKind::MergeFailed {
                reason: MergeFailure::GitFailed { detail: "boom".into() },
            },
            TaskEventKind::MergeFailed {
                reason: MergeFailure::NonFastForward {},
            },
            TaskEventKind::MergedLocal {
                from_sha: "c".repeat(40),
                to_sha: "a".repeat(40),
            },
            TaskEventKind::ReconcileFailed { detail: "d".into() },
            TaskEventKind::Landed {
                upstream_head: "d".repeat(40),
                observed_patch_id: "b".repeat(40),
            },
        ];
        for kind in kinds {
            let ev = event(kind);
            let json = serde_json::to_string(&ev).unwrap();
            let back: TaskEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(ev, back, "round-trip mismatch for {json}");
        }
    }

    /// The wire contract: tags and core field names, pinned. If this test fails you are
    /// breaking replay of existing logs — add fields instead of renaming.
    #[test]
    fn wire_tags_are_pinned() {
        let ev = event(TaskEventKind::Claimed {
            assignee: "agent:a".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "claimed");
        assert_eq!(v["task_id"], "0198c0de-0000-7000-8000-000000000000");
        assert_eq!(v["by"], "agent:test");
        assert_eq!(v["assignee"], "agent:a");

        let ev = event(TaskEventKind::MergeFailed {
            reason: MergeFailure::BranchTipChanged {},
        });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "merge_failed");
        assert_eq!(v["reason"]["kind"], "branch_tip_changed");

        let ev = event(TaskEventKind::RevisionProposed { revision: revision() });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "revision_proposed");
        assert_eq!(v["revision"]["review_sha"], "a".repeat(40));
        assert_eq!(v["revision"]["upstream"], "origin/main");

        // Tag helper stays in sync with serde.
        for (kind, tag) in [
            (TaskEventKind::Released {}, "released"),
            (
                TaskEventKind::Landed {
                    upstream_head: "d".repeat(40),
                    observed_patch_id: "b".repeat(40),
                },
                "landed",
            ),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&event(kind.clone())).unwrap()).unwrap();
            assert_eq!(v["event"], tag);
            assert_eq!(kind.tag(), tag);
        }
    }

    #[test]
    fn verifier_only_partition_is_exact() {
        let verifier_only = [
            TaskEventKind::SourceUnavailable { detail: String::new() },
            TaskEventKind::MergeFailed {
                reason: MergeFailure::MissingBranch {},
            },
            TaskEventKind::MergedLocal {
                from_sha: String::new(),
                to_sha: String::new(),
            },
            TaskEventKind::ReconcileFailed { detail: String::new() },
            TaskEventKind::Landed {
                upstream_head: String::new(),
                observed_patch_id: String::new(),
            },
        ];
        let agent_facing = [
            TaskEventKind::Opened {
                title: String::new(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: String::new(),
                assignee: None,
            },
            TaskEventKind::Claimed {
                assignee: String::new(),
            },
            TaskEventKind::Released {},
            TaskEventKind::Noted {
                text: String::new(),
                session_ref: None,
            },
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
            TaskEventKind::Abandoned { reason: String::new() },
            TaskEventKind::Superseded { by_task: String::new() },
            TaskEventKind::RevisionProposed { revision: revision() },
            TaskEventKind::ReviewRerouted {
                from: String::new(),
                to: String::new(),
            },
            TaskEventKind::ReviewPassed {
                reviewer: String::new(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
            TaskEventKind::ReviewRefuted {
                reviewer: String::new(),
                session_ref: None,
                receipts: None,
            },
        ];
        for k in &verifier_only {
            assert!(k.is_verifier_only(), "{} should be verifier-only", k.tag());
        }
        for k in &agent_facing {
            assert!(!k.is_verifier_only(), "{} should be agent-facing", k.tag());
        }
    }

    /// The identity-bearing partition (the TOFU token gate's scope) is exactly the five verbs whose
    /// endpoint keys inbox/reviewer/land semantics — claim/release/propose/pass/refute.
    #[test]
    fn identity_bearing_partition_is_exact() {
        let identity_bearing = [
            TaskEventKind::Claimed {
                assignee: String::new(),
            },
            TaskEventKind::Released {},
            TaskEventKind::RevisionProposed { revision: revision() },
            TaskEventKind::ReviewPassed {
                reviewer: String::new(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
            TaskEventKind::ReviewRefuted {
                reviewer: String::new(),
                session_ref: None,
                receipts: None,
            },
        ];
        let bookkeeping = [
            TaskEventKind::Opened {
                title: String::new(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: String::new(),
                assignee: None,
            },
            TaskEventKind::Noted {
                text: String::new(),
                session_ref: None,
            },
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
            TaskEventKind::Abandoned { reason: String::new() },
            TaskEventKind::Superseded { by_task: String::new() },
            TaskEventKind::ReviewRerouted {
                from: String::new(),
                to: String::new(),
            },
        ];
        for k in &identity_bearing {
            assert!(k.is_identity_bearing(), "{} should be identity-bearing", k.tag());
        }
        for k in &bookkeeping {
            assert!(
                !k.is_identity_bearing(),
                "{} should be bookkeeping (token-optional)",
                k.tag()
            );
        }
    }

    #[test]
    fn harness_family_maps_majors_and_declines_multiplexers() {
        assert_eq!(harness_family(Harness::Claude), Some("anthropic"));
        assert_eq!(harness_family(Harness::Codex), Some("openai"));
        assert_eq!(harness_family(Harness::Gemini), Some("google"));
        assert_eq!(harness_family(Harness::Cursor), None);
        assert_eq!(harness_family(Harness::Goose), None);
    }

    #[test]
    fn model_family_maps_id_prefixes_and_declines_unknowns() {
        assert_eq!(model_family("claude-sonnet-4-5-20250929"), Some("anthropic"));
        assert_eq!(model_family("Claude-Opus-4"), Some("anthropic"), "case-insensitive");
        assert_eq!(model_family("  gpt-5.1-codex "), Some("openai"), "trimmed");
        assert_eq!(model_family("o3-mini"), Some("openai"), "o-series");
        assert_eq!(model_family("o1"), Some("openai"));
        assert_eq!(model_family("gemini-2.5-pro"), Some("google"));
        assert_eq!(model_family("grok-4"), Some("xai"));
        assert_eq!(model_family("qwen3-coder-plus"), Some("alibaba"));
        assert_eq!(model_family("kimi-k2"), Some("moonshot"));
        assert_eq!(model_family("deepseek-v3"), Some("deepseek"));
        assert_eq!(model_family("llama-3.3-70b"), Some("meta"));
        // Not guessed: bare 'o' words, empty, unknown vendors.
        assert_eq!(model_family("opus"), None, "'o' + non-digit is not o-series");
        assert_eq!(model_family(""), None);
        assert_eq!(model_family("mistral-large"), None);
    }

    /// Wire additivity of `receipts`: a pre-receipts pass/refute event (no `receipts` key)
    /// deserializes to `receipts: None` AND re-serializes without the key (byte-stable replay of
    /// old logs), while a receipts-bearing event round-trips with the key.
    #[test]
    fn receipts_field_is_wire_additive() {
        // The exact shape old logs carry (also what a pre-receipts cv wrote).
        let old = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"agent:test","event":"review_passed","reviewer":"b"}"#;
        let ev: TaskEvent = serde_json::from_str(old).unwrap();
        let TaskEventKind::ReviewPassed { ref receipts, .. } = ev.kind else {
            panic!("wrong kind: {ev:?}")
        };
        assert_eq!(receipts, &None, "absent key reads as None");
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            old,
            "an old event must re-serialize to its own bytes — receipts is skip-when-none"
        );

        // A new event with receipts round-trips, and the key appears only when present.
        let ev = event(TaskEventKind::ReviewRefuted {
            reviewer: "b".into(),
            session_ref: None,
            receipts: Some(ReviewReceipts {
                saw_change: Some(false),
                ran_checks: None,
                turns: Some(3),
            }),
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: TaskEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["receipts"]["saw_change"], false);
        assert_eq!(v["receipts"]["turns"], 3);
        assert!(
            v["receipts"].get("ran_checks").is_none(),
            "undetermined sub-observations stay off the wire: {json}"
        );
    }

    /// Wire additivity of the Done `check`: a pre-check `done` event (no `check` key) reads as
    /// `check: None` and re-serializes to its own bytes (byte-stable replay of old logs), while a
    /// checked `done` round-trips carrying its flattened `check` tag and result.
    #[test]
    fn done_check_field_is_wire_additive() {
        // What a pre-check cv wrote for a self-reported completion.
        let old = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"agent:test","event":"done","observed":"trust me"}"#;
        let ev: TaskEvent = serde_json::from_str(old).unwrap();
        let TaskEventKind::Done { ref check, .. } = ev.kind else {
            panic!("wrong kind: {ev:?}")
        };
        assert_eq!(check, &None, "absent key reads as None (still self-report)");
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            old,
            "an old done must re-serialize to its own bytes — check is skip-when-none"
        );

        // A checked done round-trips; the flattened kind tag and result appear on the wire.
        let ev = event(TaskEventKind::Done {
            observed: Some("exists, 42 bytes".into()),
            check: Some(DoneCheck {
                kind: DoneCheckKind::File {
                    path: PathBuf::from("/repo/design.md"),
                },
                result: "exists, 42 bytes".into(),
            }),
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: TaskEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["check"]["check"], "file",
            "kind is flattened under the outer check key"
        );
        assert_eq!(v["check"]["path"], "/repo/design.md");
        assert_eq!(v["check"]["result"], "exists, 42 bytes");
    }
}
