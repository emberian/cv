//! Pure task reducer: fold [`TaskEvent`]s into a [`TaskReadModel`]. No I/O, no git, no clock —
//! the same [`TaskReducer::apply`] runs during live appends and during replay, so live state and
//! replayed state cannot diverge (replay parity).
//!
//! Ported from mission-control's `ramspace-core/src/land_request.rs` (its one keeper). The land
//! facet keeps that reducer's invariants byte-for-byte where possible. **Deliberate deviations:**
//!
//! 1. Content-addressed `record_hash` ids → uuid v7 event ids. The hash chain existed to make ids
//!    unforgeable — an authority property law 3 removes. Idempotency semantics are preserved on
//!    the id: byte-identical re-append is a no-op, id reuse with different content is loud.
//! 2. `Landed` is reachable from `Ready` as well as `MergedLocal`. Nobody merges on the task's
//!    behalf here; a branch routinely lands via forge PR with no local-integration step, and
//!    "the reviewed patch is observed on upstream" is state-complete by itself.
//! 3. `MergeFailed` reasons are verification findings (observations), not merge-attempt outcomes
//!    (`attempt_id` dropped) — cv never runs a merge.
//! 4. Typed `EndpointId` → plain `String`, matching the board's `from` convention.
//!
//! On top of the ported facet sits a base lifecycle for tasks that never involve code review:
//! `Open → Claimed → Done`, terminals `Abandoned`/`Superseded`, `Noted` for progress (accepted on
//! a terminal task too, marked `post_close`: terminal is about state, not the record). The two
//! layers meet in exactly two rules: a revision may only be proposed on a non-terminal task, and
//! `Done` is refused while a revision is live (you can always kill a task, never silently
//! complete one that has unlanded reviewed code).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{
    DoneCheck, IndependenceCheck, MergeFailure, ReviewReceipts, Revision, RevisionState, TaskEvent, TaskEventKind,
    TaskState,
};

/// Errors from applying an event. During a CAS append these reject the candidate event; during
/// replay they abort loudly (a log that stops reducing is corruption, not something to skip).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ReduceError {
    #[error("event id {id} was already applied with different content")]
    EventIdCollision { id: String },
    #[error("opened event id {id} must equal its task_id {task_id}")]
    OpenIdMismatch { id: String, task_id: String },
    #[error("task {task_id} was already opened")]
    DuplicateOpen { task_id: String },
    #[error("unknown task {task_id} for event {event}")]
    UnknownTask { task_id: String, event: &'static str },
    #[error("task {task_id} in state {state} cannot apply {event}")]
    InvalidTransition {
        task_id: String,
        state: String,
        event: &'static str,
    },
    #[error("task {task_id}: {event} requires a proposed revision")]
    MissingRevision { task_id: String, event: &'static str },
    #[error("task {task_id}: reviewer {actual} is not the active reviewer {expected}")]
    ReviewerMismatch {
        task_id: String,
        expected: String,
        actual: String,
    },
    #[error("task {task_id}: cannot complete while revision {revision} is {state} (kill or land it first)")]
    DoneWithLiveRevision {
        task_id: String,
        revision: u32,
        state: String,
    },
    #[error("task {task_id}: merged to_sha {actual_to_sha} is not the reviewed sha {expected_review_sha}")]
    MergeTargetMismatch {
        task_id: String,
        expected_review_sha: String,
        actual_to_sha: String,
    },
    #[error("task {task_id}: observed patch-id {observed} does not match the reviewed revision's {expected}")]
    PatchIdMismatch {
        task_id: String,
        expected: String,
        observed: String,
    },
    #[error("invalid {field}: {value:?}")]
    InvalidField { field: &'static str, value: String },
    #[error("task {task_id} already has a posed decision (amend by posing a new task, never by rewriting)")]
    AlreadyPosed { task_id: String },
    #[error("task {task_id}: {event} requires a posed decision (this task is work, not a question — use done)")]
    MissingDecision { task_id: String, event: &'static str },
    #[error("task {task_id}: {choice:?} is not one of the posed options ({options})")]
    UnknownChoice {
        task_id: String,
        choice: String,
        options: String,
    },
    #[error("task {task_id} is a decision: answer it with resolve (--choice or --accept-default), not done")]
    DecisionNeedsResolve { task_id: String },
    #[error("task {task_id}: a provisional resolution stands on the default ({default:?}), not {choice:?} — only the decider chooses otherwise")]
    ProvisionalNotDefault {
        task_id: String,
        default: String,
        choice: String,
    },
    #[error("task {task_id}: only its assignee ({assignee}) or its opener ({opener}) can pin its STATUS, not {by}")]
    StatusNotHolder {
        task_id: String,
        assignee: String,
        opener: String,
        by: String,
    },
    #[error("task {task_id}: only the decision's assignee ({assignee}) can confirm or veto its provisional resolution, not {by}")]
    ProvisionalOverrideNotDecider {
        task_id: String,
        assignee: String,
        by: String,
    },
}

/// A recorded anomaly that did not change state (the task/revision stays actionable).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TaskIssue {
    SourceUnavailable { detail: String, event_id: String },
    MergeFailed { reason: MergeFailure, event_id: String },
    ReconcileFailed { detail: String, event_id: String },
}

impl TaskIssue {
    pub fn describe(&self) -> String {
        match self {
            TaskIssue::SourceUnavailable { detail, .. } => format!("source unavailable: {detail}"),
            TaskIssue::MergeFailed { reason, .. } => reason.describe(),
            TaskIssue::ReconcileFailed { detail, .. } => format!("reconcile failed: {detail}"),
        }
    }

    /// The event that recorded this issue (for dedup by the verifier's anti-spam rule).
    pub fn event_id(&self) -> &str {
        match self {
            TaskIssue::SourceUnavailable { event_id, .. }
            | TaskIssue::MergeFailed { event_id, .. }
            | TaskIssue::ReconcileFailed { event_id, .. } => event_id,
        }
    }
}

/// A progress note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub event_id: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    pub text: String,
    pub session_ref: Option<String>,
    /// Appended after the task reached a terminal state. Terminal is about STATE, not the record:
    /// a lane that marks `done` and then writes its evidence keeps the evidence, and the note
    /// changes nothing else. Omitted from the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub post_close: bool,
}

/// Read model for one revision of a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevisionProjection {
    pub revision: Revision,
    pub state: RevisionState,
    /// When the revision was proposed (the propose event's timestamp) — the anchor for aging
    /// AwaitingReview work: a dead reviewer is honest state nobody sees unless it ages visibly.
    pub proposed_at: DateTime<Utc>,
    /// Who appended the propose event (`by`) — the revision's author endpoint (the batting-average
    /// attribution for the stats projection). `default` so projections serialized before the
    /// field existed still deserialize (as `""`).
    #[serde(default)]
    pub proposed_by: String,
    /// The only endpoint whose verdict can advance this revision. Bound at propose time (if the
    /// revision named a reviewer) or by the first verdict; reroute is the only reassignment.
    pub active_reviewer: Option<String>,
    pub pass: Option<PassEvidence>,
    pub refute: Option<RefuteEvidence>,
    pub reroutes: Vec<RerouteEvidence>,
    /// `(from_sha, to_sha)` observed by the verifier.
    pub local_merge: Option<(String, String)>,
    /// `(upstream_head, observed_patch_id)` observed by the verifier.
    pub landed: Option<(String, String)>,
    /// When the verifier observed the land (the `Landed` event's timestamp) — the end anchor for
    /// time-to-land in the stats projection. `None` until landed. `default` so projections
    /// serialized before the field existed still deserialize.
    #[serde(default)]
    pub landed_at: Option<DateTime<Utc>>,
    pub issues: Vec<TaskIssue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassEvidence {
    pub reviewer: String,
    pub event_id: String,
    pub ts: DateTime<Utc>,
    pub session_ref: Option<String>,
    pub independence: Option<IndependenceCheck>,
    /// Advisory reviewer receipts; `None` on verdicts recorded without a reviewer session (and on
    /// events appended before the field existed — `default` keeps those deserializing).
    #[serde(default)]
    pub receipts: Option<ReviewReceipts>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefuteEvidence {
    pub reviewer: String,
    pub event_id: String,
    pub ts: DateTime<Utc>,
    pub session_ref: Option<String>,
    /// Advisory reviewer receipts, as on [`PassEvidence`].
    #[serde(default)]
    pub receipts: Option<ReviewReceipts>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RerouteEvidence {
    pub from: String,
    pub to: String,
    pub event_id: String,
}

/// What a task is *effectively* doing right now: its base state, unless a revision is live (or
/// landed), in which case the revision's state is the truth that matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "layer", content = "state")]
pub enum EffectiveState {
    Base(TaskState),
    Revision(RevisionState),
}

impl EffectiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            EffectiveState::Base(s) => s.as_str(),
            EffectiveState::Revision(s) => s.as_str(),
        }
    }
}

/// The decision facet of a task (present once a `posed` event applied): the question's options,
/// the default that stands if nobody speaks, the deadline for that, and the answer once given.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Every admissible choice, the default first.
    pub options: Vec<String>,
    #[serde(rename = "default")]
    pub default_choice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// The note event this decision was split out of, when it was (`cv task split`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub posed_at: DateTime<Utc>,
    pub posed_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    /// A provisional resolution the decider has since confirmed or vetoed: kept, never rewritten
    /// (`resolution` is then the decider's — the last non-provisional resolution wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_provisional: Option<Resolution>,
}

impl Decision {
    /// Resolved provisionally and not yet confirmed or vetoed by the decider: the decider's
    /// "made for you (veto?)" inbox group.
    pub fn awaiting_veto(&self) -> bool {
        self.resolution.as_ref().is_some_and(|r| r.provisional)
    }

    /// The options other than the default, in posed order.
    pub fn alternatives(&self) -> impl Iterator<Item = &str> {
        self.options
            .iter()
            .map(String::as_str)
            .filter(move |o| *o != self.default_choice)
    }
}

/// How a decision was answered: the literal choice (never "the default" by reference — the record
/// stays readable if the default ever reads differently), by whom, when, and any note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub choice: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `choice == default` at resolve time: the resolver accepted the proposed default.
    pub accepted_default: bool,
    /// Made on the decider's behalf by the poser, on the default, with a veto window
    /// ([`TaskEventKind::Resolved`]'s `provisional`). Omitted from the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
}

/// A task's pinned STATUS (the latest `status_set`): the current state in one place, so a relay's
/// hand-off is a field, not the newest of a hundred notes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PinnedStatus {
    pub text: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    pub event_id: String,
    /// How many times a status was pinned on this task (this one included).
    pub revisions: u32,
}

/// Read model for one task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskProjection {
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub repo: Option<PathBuf>,
    pub issue: Option<String>,
    pub channel: String,
    pub state: TaskState,
    pub assignee: Option<String>,
    pub opened_by: String,
    pub opened_at: DateTime<Utc>,
    pub last_event_id: String,
    pub last_ts: DateTime<Utc>,
    pub revisions: Vec<RevisionProjection>,
    pub notes: Vec<Note>,
    pub superseded_by: Option<String>,
    pub abandoned_reason: Option<String>,
    pub done_observed: Option<String>,
    /// How the completion was verified, when a passing check was attached to the `Done`. `None` =
    /// self-reported (the view/provenance layer labels the two distinctly).
    pub done_check: Option<DoneCheck>,
    /// The union of every `tagged` event's labels, in first-seen order. Omitted from the wire when
    /// empty, so a log that never tagged anything serializes exactly as it did before tags existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Task ids this task is waiting on (`blocked_by` events, deduplicated, first-seen order).
    /// Whether the task is blocked *now* is [`super::project::is_blocked`], computed from each
    /// blocker's current state. Omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// The decision facet, once a `posed` event applied. `None` = this task is work (kind
    /// `action`); `Some` = it is a question (kind `decision`). Omitted from the wire when absent, so
    /// every pre-decision projection serializes exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    /// The pinned STATUS, once a `status_set` applied (the latest replaces the earlier ones).
    /// Omitted from the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PinnedStatus>,
}

impl TaskProjection {
    /// Is this a decision (a question to answer) rather than an action (work to do)?
    pub fn is_decision(&self) -> bool {
        self.decision.is_some()
    }

    /// `decision` or `action`: the task's kind, derived from whether a decision was posed.
    pub fn kind(&self) -> &'static str {
        if self.is_decision() {
            "decision"
        } else {
            "action"
        }
    }

    fn opened(event: &TaskEvent, kind: &TaskEventKind) -> TaskProjection {
        let TaskEventKind::Opened {
            title,
            body,
            repo,
            issue,
            channel,
            assignee,
        } = kind
        else {
            unreachable!("opened() is only called with Opened events");
        };
        TaskProjection {
            task_id: event.task_id.clone(),
            title: title.clone(),
            body: body.clone(),
            repo: repo.clone(),
            issue: issue.clone(),
            channel: channel.clone(),
            state: TaskState::Open,
            assignee: assignee.clone(),
            opened_by: event.by.clone(),
            opened_at: event.ts,
            last_event_id: event.id.clone(),
            last_ts: event.ts,
            revisions: Vec::new(),
            notes: Vec::new(),
            superseded_by: None,
            abandoned_reason: None,
            done_observed: None,
            done_check: None,
            tags: Vec::new(),
            blocked_by: Vec::new(),
            decision: None,
            status: None,
        }
    }

    /// The current (latest) revision, if any.
    pub fn current_revision(&self) -> Option<&RevisionProjection> {
        self.revisions.last()
    }

    pub fn effective_state(&self) -> EffectiveState {
        match self.current_revision() {
            Some(rev) if !rev.state.is_terminal() || rev.state == RevisionState::Landed => {
                EffectiveState::Revision(rev.state)
            }
            _ => EffectiveState::Base(self.state),
        }
    }

    fn touch(&mut self, event: &TaskEvent) {
        self.last_event_id = event.id.clone();
        self.last_ts = event.ts;
    }
}

/// All tasks, keyed by task id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskReadModel {
    pub tasks: BTreeMap<String, TaskProjection>,
}

/// The reducer. Feed it every event in log order; ask it for the model.
#[derive(Default)]
pub struct TaskReducer {
    model: TaskReadModel,
    applied: HashMap<String, TaskEvent>,
}

impl TaskReducer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a full event slice (replay).
    pub fn reduce(events: &[TaskEvent]) -> Result<TaskReadModel, ReduceError> {
        let mut reducer = Self::new();
        for event in events {
            reducer.apply(event)?;
        }
        Ok(reducer.into_model())
    }

    pub fn model(&self) -> &TaskReadModel {
        &self.model
    }

    pub fn into_model(self) -> TaskReadModel {
        self.model
    }

    pub fn apply(&mut self, event: &TaskEvent) -> Result<(), ReduceError> {
        validate_event_shape(event)?;
        if let Some(previous) = self.applied.get(&event.id) {
            return if previous == event {
                Ok(()) // idempotent re-append
            } else {
                Err(ReduceError::EventIdCollision { id: event.id.clone() })
            };
        }

        match &event.kind {
            TaskEventKind::Opened { .. } => self.apply_open(event)?,
            kind => self.apply_existing(event, kind)?,
        }
        self.applied.insert(event.id.clone(), event.clone());
        Ok(())
    }

    fn apply_open(&mut self, event: &TaskEvent) -> Result<(), ReduceError> {
        if event.task_id != event.id {
            return Err(ReduceError::OpenIdMismatch {
                id: event.id.clone(),
                task_id: event.task_id.clone(),
            });
        }
        if self.model.tasks.contains_key(&event.task_id) {
            return Err(ReduceError::DuplicateOpen {
                task_id: event.task_id.clone(),
            });
        }
        self.model
            .tasks
            .insert(event.task_id.clone(), TaskProjection::opened(event, &event.kind));
        Ok(())
    }

    fn apply_existing(&mut self, event: &TaskEvent, kind: &TaskEventKind) -> Result<(), ReduceError> {
        let Some(task) = self.model.tasks.get_mut(&event.task_id) else {
            return Err(ReduceError::UnknownTask {
                task_id: event.task_id.clone(),
                event: kind.tag_static(),
            });
        };

        match kind {
            TaskEventKind::Opened { .. } => {
                return Err(ReduceError::DuplicateOpen {
                    task_id: task.task_id.clone(),
                });
            }

            // ── base lifecycle ────────────────────────────────────────────
            TaskEventKind::Claimed { assignee } => {
                require_base(task, kind, &[TaskState::Open])?;
                task.assignee = Some(assignee.clone());
                task.state = TaskState::Claimed;
            }
            TaskEventKind::Released {} => {
                require_base(task, kind, &[TaskState::Claimed])?;
                task.assignee = None;
                task.state = TaskState::Open;
            }
            TaskEventKind::Noted { text, session_ref } => {
                // The one event a terminal task accepts unconditionally: a note never changes
                // state, so refusing it only lost evidence (four lanes wrote `done` before their
                // note and lost the note). It is marked `post_close` so every surface can say so.
                task.notes.push(Note {
                    event_id: event.id.clone(),
                    by: event.by.clone(),
                    ts: event.ts,
                    text: text.clone(),
                    session_ref: session_ref.clone(),
                    post_close: task.state.is_terminal(),
                });
            }
            TaskEventKind::Done { observed, check } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                if task.decision.is_some() {
                    return Err(ReduceError::DecisionNeedsResolve {
                        task_id: task.task_id.clone(),
                    });
                }
                if let Some(rev) = task.current_revision() {
                    if !rev.state.is_terminal() {
                        return Err(ReduceError::DoneWithLiveRevision {
                            task_id: task.task_id.clone(),
                            revision: rev.revision.n,
                            state: rev.state.as_str().to_string(),
                        });
                    }
                }
                task.done_observed = observed.clone();
                task.done_check = check.clone();
                task.state = TaskState::Done;
            }
            TaskEventKind::Abandoned { reason } => {
                // Always available on a non-terminal task, live revision or not: you can always
                // kill a task.
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                task.abandoned_reason = Some(reason.clone());
                task.state = TaskState::Abandoned;
            }
            TaskEventKind::Superseded { by_task } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                task.superseded_by = Some(by_task.clone());
                task.state = TaskState::Superseded;
            }
            TaskEventKind::Tagged { tags } => {
                // Tags are bookkeeping, like notes: allowed while the task is live, never on a
                // terminal one (a done task's labels are part of the record, not a surface to edit).
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                for t in tags {
                    if !task.tags.iter().any(|have| have == t) {
                        task.tags.push(t.clone());
                    }
                }
            }
            TaskEventKind::BlockedBy { task: blocker } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                if !task.blocked_by.iter().any(|have| have == blocker) {
                    task.blocked_by.push(blocker.clone());
                }
            }

            // ── decision facet ────────────────────────────────────────────
            TaskEventKind::Posed {
                options,
                default_choice,
                deadline,
                source,
            } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                if task.decision.is_some() {
                    return Err(ReduceError::AlreadyPosed {
                        task_id: task.task_id.clone(),
                    });
                }
                task.decision = Some(Decision {
                    options: options.clone(),
                    default_choice: default_choice.clone(),
                    deadline: *deadline,
                    source: source.clone(),
                    posed_at: event.ts,
                    posed_by: event.by.clone(),
                    resolution: None,
                    superseded_provisional: None,
                });
            }
            TaskEventKind::Resolved {
                choice,
                note,
                provisional,
            } => {
                // The one place a terminal task accepts a state-bearing event, written narrowly: a
                // NON-provisional resolution, by the decision's assignee, over a resolution that is
                // provisional. Everything else on a resolved task is refused as before.
                let over_provisional = task.state == TaskState::Resolved
                    && !*provisional
                    && task.decision.as_ref().is_some_and(Decision::awaiting_veto);
                if over_provisional {
                    let assignee = task.assignee.clone().unwrap_or_default();
                    if !super::project::same_actor(&event.by, &assignee) {
                        return Err(ReduceError::ProvisionalOverrideNotDecider {
                            task_id: task.task_id.clone(),
                            assignee,
                            by: event.by.clone(),
                        });
                    }
                } else {
                    require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                }
                let task_id = task.task_id.clone();
                let Some(decision) = task.decision.as_mut() else {
                    return Err(ReduceError::MissingDecision {
                        task_id,
                        event: kind.tag_static(),
                    });
                };
                if *provisional && *choice != decision.default_choice {
                    return Err(ReduceError::ProvisionalNotDefault {
                        task_id,
                        default: decision.default_choice.clone(),
                        choice: choice.clone(),
                    });
                }
                if !decision.options.iter().any(|o| o == choice) {
                    return Err(ReduceError::UnknownChoice {
                        task_id,
                        choice: choice.clone(),
                        options: decision.options.join(" | "),
                    });
                }
                let previous = decision.resolution.replace(Resolution {
                    choice: choice.clone(),
                    by: event.by.clone(),
                    ts: event.ts,
                    event_id: event.id.clone(),
                    note: note.clone(),
                    accepted_default: *choice == decision.default_choice,
                    provisional: *provisional,
                });
                if over_provisional {
                    decision.superseded_provisional = previous;
                }
                task.state = TaskState::Resolved;
            }

            TaskEventKind::StatusSet { text } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                let holder =
                    |who: &Option<String>| who.as_deref().is_some_and(|w| super::project::same_actor(&event.by, w));
                if !holder(&task.assignee) && !holder(&Some(task.opened_by.clone())) {
                    return Err(ReduceError::StatusNotHolder {
                        task_id: task.task_id.clone(),
                        assignee: task.assignee.clone().unwrap_or_else(|| "-".into()),
                        opener: task.opened_by.clone(),
                        by: event.by.clone(),
                    });
                }
                let revisions = task.status.as_ref().map_or(0, |s| s.revisions) + 1;
                task.status = Some(PinnedStatus {
                    text: text.clone(),
                    by: event.by.clone(),
                    ts: event.ts,
                    event_id: event.id.clone(),
                    revisions,
                });
            }

            // ── land facet ────────────────────────────────────────────────
            TaskEventKind::RevisionProposed { revision } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                // A live prior revision is auto-superseded: the only cure for a refute, and the
                // only way reviewed content changes, is a new revision.
                if let Some(prev) = task.revisions.last_mut() {
                    if !prev.state.is_terminal() {
                        prev.state = RevisionState::Superseded;
                    }
                }
                task.revisions.push(RevisionProjection {
                    active_reviewer: revision.reviewer.clone(),
                    revision: revision.clone(),
                    state: RevisionState::AwaitingReview,
                    proposed_at: event.ts,
                    proposed_by: event.by.clone(),
                    landed_at: None,
                    pass: None,
                    refute: None,
                    reroutes: Vec::new(),
                    local_merge: None,
                    landed: None,
                    issues: Vec::new(),
                });
            }
            TaskEventKind::ReviewRerouted { from, to } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::AwaitingReview])?;
                require_reviewer(&task_id, rev, from)?;
                if from == to {
                    return Err(ReduceError::InvalidField {
                        field: "reroute reviewer",
                        value: to.clone(),
                    });
                }
                rev.active_reviewer = Some(to.clone());
                rev.reroutes.push(RerouteEvidence {
                    from: from.clone(),
                    to: to.clone(),
                    event_id: event.id.clone(),
                });
            }
            TaskEventKind::ReviewPassed {
                reviewer,
                session_ref,
                independence,
                receipts,
            } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::AwaitingReview])?;
                require_reviewer(&task_id, rev, reviewer)?;
                rev.active_reviewer = Some(reviewer.clone());
                rev.pass = Some(PassEvidence {
                    reviewer: reviewer.clone(),
                    event_id: event.id.clone(),
                    ts: event.ts,
                    session_ref: session_ref.clone(),
                    independence: independence.clone(),
                    receipts: receipts.clone(),
                });
                rev.state = RevisionState::Ready;
            }
            TaskEventKind::ReviewRefuted {
                reviewer,
                session_ref,
                receipts,
            } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(
                    &task_id,
                    rev,
                    kind,
                    &[RevisionState::AwaitingReview, RevisionState::Ready],
                )?;
                require_reviewer(&task_id, rev, reviewer)?;
                rev.active_reviewer = Some(reviewer.clone());
                rev.refute = Some(RefuteEvidence {
                    reviewer: reviewer.clone(),
                    event_id: event.id.clone(),
                    ts: event.ts,
                    session_ref: session_ref.clone(),
                    receipts: receipts.clone(),
                });
                rev.state = RevisionState::Refuted;
            }

            // ── verifier-only observations ────────────────────────────────
            TaskEventKind::SourceUnavailable { detail } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::Ready])?;
                rev.issues.push(TaskIssue::SourceUnavailable {
                    detail: detail.clone(),
                    event_id: event.id.clone(),
                });
            }
            TaskEventKind::MergeFailed { reason } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::Ready])?;
                rev.issues.push(TaskIssue::MergeFailed {
                    reason: reason.clone(),
                    event_id: event.id.clone(),
                });
            }
            TaskEventKind::MergedLocal { from_sha, to_sha } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::Ready])?;
                if !to_sha.eq_ignore_ascii_case(&rev.revision.review_sha) {
                    return Err(ReduceError::MergeTargetMismatch {
                        task_id,
                        expected_review_sha: rev.revision.review_sha.clone(),
                        actual_to_sha: to_sha.clone(),
                    });
                }
                rev.local_merge = Some((from_sha.clone(), to_sha.clone()));
                rev.state = RevisionState::MergedLocal;
            }
            TaskEventKind::ReconcileFailed { detail } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                require_revision_state(&task_id, rev, kind, &[RevisionState::MergedLocal])?;
                rev.issues.push(TaskIssue::ReconcileFailed {
                    detail: detail.clone(),
                    event_id: event.id.clone(),
                });
            }
            TaskEventKind::Landed {
                upstream_head,
                observed_patch_id,
            } => {
                let task_id = task.task_id.clone();
                let rev = require_revision(task, kind)?;
                // Deviation 2: Ready → Landed is legal (forge-PR lands skip local integration).
                require_revision_state(&task_id, rev, kind, &[RevisionState::Ready, RevisionState::MergedLocal])?;
                if !observed_patch_id.eq_ignore_ascii_case(&rev.revision.patch_id) {
                    return Err(ReduceError::PatchIdMismatch {
                        task_id,
                        expected: rev.revision.patch_id.clone(),
                        observed: observed_patch_id.clone(),
                    });
                }
                rev.landed = Some((upstream_head.clone(), observed_patch_id.clone()));
                rev.landed_at = Some(event.ts);
                rev.state = RevisionState::Landed;
            }
        }
        task.touch(event);
        Ok(())
    }
}

impl TaskEventKind {
    /// `tag()` but usable in `ReduceError`'s `&'static str` fields.
    fn tag_static(&self) -> &'static str {
        self.tag()
    }
}

fn require_base(task: &TaskProjection, kind: &TaskEventKind, allowed: &[TaskState]) -> Result<(), ReduceError> {
    if allowed.contains(&task.state) {
        Ok(())
    } else {
        Err(ReduceError::InvalidTransition {
            task_id: task.task_id.clone(),
            state: task.state.as_str().to_string(),
            event: kind.tag_static(),
        })
    }
}

fn require_revision<'t>(
    task: &'t mut TaskProjection,
    kind: &TaskEventKind,
) -> Result<&'t mut RevisionProjection, ReduceError> {
    let task_id = task.task_id.clone();
    task.revisions.last_mut().ok_or(ReduceError::MissingRevision {
        task_id,
        event: kind.tag_static(),
    })
}

fn require_revision_state(
    task_id: &str,
    rev: &RevisionProjection,
    kind: &TaskEventKind,
    allowed: &[RevisionState],
) -> Result<(), ReduceError> {
    if allowed.contains(&rev.state) {
        Ok(())
    } else {
        Err(ReduceError::InvalidTransition {
            task_id: task_id.to_string(),
            state: rev.state.as_str().to_string(),
            event: kind.tag_static(),
        })
    }
}

/// Reviewer binding: only the active reviewer's verdict counts. If no reviewer is bound yet
/// (revision proposed without one), the first verdict binds — recorded, visible, and law 2 makes
/// independence advisory rather than the reducer's job.
fn require_reviewer(task_id: &str, rev: &RevisionProjection, actual: &str) -> Result<(), ReduceError> {
    match &rev.active_reviewer {
        None => Ok(()),
        Some(expected) if expected == actual => Ok(()),
        Some(expected) => Err(ReduceError::ReviewerMismatch {
            task_id: task_id.to_string(),
            expected: expected.clone(),
            actual: actual.to_string(),
        }),
    }
}

// ── shape validation (pure, no I/O) ──────────────────────────────────────────

fn require_nonempty(field: &'static str, value: &str) -> Result<(), ReduceError> {
    if value.trim().is_empty() {
        Err(ReduceError::InvalidField {
            field,
            value: value.to_string(),
        })
    } else {
        Ok(())
    }
}

fn require_hex(field: &'static str, value: &str, lens: &[usize]) -> Result<(), ReduceError> {
    if lens.contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ReduceError::InvalidField {
            field,
            value: value.to_string(),
        })
    }
}

/// Git object ids: sha1 (40 hex) or sha256 (64 hex) repos.
fn require_git_oid(field: &'static str, value: &str) -> Result<(), ReduceError> {
    require_hex(field, value, &[40, 64])
}

/// `git patch-id` output is always 40 hex.
fn require_patch_id(field: &'static str, value: &str) -> Result<(), ReduceError> {
    require_hex(field, value, &[40])
}

fn validate_revision(revision: &Revision) -> Result<(), ReduceError> {
    if revision.n == 0 {
        return Err(ReduceError::InvalidField {
            field: "revision.n",
            value: "0".to_string(),
        });
    }
    require_nonempty("revision.branch", &revision.branch)?;
    require_nonempty("revision.upstream", &revision.upstream)?;
    require_git_oid("revision.base", &revision.base)?;
    require_git_oid("revision.review_sha", &revision.review_sha)?;
    require_patch_id("revision.patch_id", &revision.patch_id)?;
    if let Some(reviewer) = &revision.reviewer {
        require_nonempty("revision.reviewer", reviewer)?;
    }
    Ok(())
}

fn validate_event_shape(event: &TaskEvent) -> Result<(), ReduceError> {
    require_nonempty("id", &event.id)?;
    require_nonempty("task_id", &event.task_id)?;
    require_nonempty("by", &event.by)?;
    match &event.kind {
        TaskEventKind::Opened {
            title,
            channel,
            assignee,
            ..
        } => {
            require_nonempty("title", title)?;
            require_nonempty("channel", channel)?;
            if let Some(a) = assignee {
                require_nonempty("assignee", a)?;
            }
            Ok(())
        }
        TaskEventKind::Claimed { assignee } => require_nonempty("assignee", assignee),
        TaskEventKind::Released {} => Ok(()),
        TaskEventKind::Noted { text, .. } => require_nonempty("text", text),
        TaskEventKind::Done { .. } => Ok(()),
        TaskEventKind::Abandoned { reason } => require_nonempty("reason", reason),
        TaskEventKind::Superseded { by_task } => require_nonempty("by_task", by_task),
        TaskEventKind::Tagged { tags } => {
            if tags.is_empty() {
                return Err(ReduceError::InvalidField {
                    field: "tags",
                    value: "[]".into(),
                });
            }
            for t in tags {
                require_nonempty("tags[]", t)?;
                if t.chars().any(|c| c.is_whitespace() || c == ',') {
                    return Err(ReduceError::InvalidField {
                        field: "tags[]",
                        value: t.clone(),
                    });
                }
            }
            Ok(())
        }
        TaskEventKind::BlockedBy { task } => {
            require_nonempty("task", task)?;
            if task == &event.task_id {
                return Err(ReduceError::InvalidField {
                    field: "task",
                    value: format!("{task} (a task cannot block itself)"),
                });
            }
            Ok(())
        }
        TaskEventKind::Posed {
            options,
            default_choice,
            ..
        } => {
            require_nonempty("default", default_choice)?;
            for o in options {
                require_nonempty("options[]", o)?;
            }
            if !options.iter().any(|o| o == default_choice) {
                return Err(ReduceError::InvalidField {
                    field: "options",
                    value: format!(
                        "{} (the default {default_choice:?} must be one of them)",
                        options.join(" | ")
                    ),
                });
            }
            let mut seen = std::collections::HashSet::new();
            if let Some(dup) = options.iter().find(|o| !seen.insert(o.as_str())) {
                return Err(ReduceError::InvalidField {
                    field: "options",
                    value: format!("{dup:?} appears twice"),
                });
            }
            Ok(())
        }
        TaskEventKind::Resolved { choice, .. } => require_nonempty("choice", choice),
        TaskEventKind::StatusSet { text } => require_nonempty("text", text),
        TaskEventKind::RevisionProposed { revision } => validate_revision(revision),
        TaskEventKind::ReviewRerouted { from, to } => {
            require_nonempty("from", from)?;
            require_nonempty("to", to)
        }
        TaskEventKind::ReviewPassed { reviewer, .. } => require_nonempty("reviewer", reviewer),
        TaskEventKind::ReviewRefuted { reviewer, .. } => require_nonempty("reviewer", reviewer),
        TaskEventKind::SourceUnavailable { detail } => require_nonempty("detail", detail),
        TaskEventKind::MergeFailed { .. } => Ok(()),
        TaskEventKind::MergedLocal { from_sha, to_sha } => {
            require_git_oid("from_sha", from_sha)?;
            require_git_oid("to_sha", to_sha)?;
            if from_sha.eq_ignore_ascii_case(to_sha) {
                return Err(ReduceError::InvalidField {
                    field: "merge edge",
                    value: format!("{from_sha}..{to_sha}"),
                });
            }
            Ok(())
        }
        TaskEventKind::ReconcileFailed { detail } => require_nonempty("detail", detail),
        TaskEventKind::Landed {
            upstream_head,
            observed_patch_id,
        } => {
            require_git_oid("upstream_head", upstream_head)?;
            require_patch_id("observed_patch_id", observed_patch_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR: &str = "agent:author";
    const REVIEWER: &str = "agent:reviewer";
    const OTHER: &str = "agent:other";

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    struct Log {
        events: Vec<TaskEvent>,
        n: u64,
    }

    impl Log {
        fn new() -> Self {
            Log {
                events: Vec::new(),
                n: 0,
            }
        }

        fn next_id(&mut self) -> String {
            self.n += 1;
            format!("00000000-0000-7000-8000-{:012}", self.n)
        }

        fn push(&mut self, task_id: &str, by: &str, kind: TaskEventKind) -> TaskEvent {
            let id = self.next_id();
            let ev = TaskEvent {
                id,
                task_id: task_id.to_string(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind,
            };
            self.events.push(ev.clone());
            ev
        }

        fn open(&mut self, by: &str) -> String {
            let id = self.next_id();
            let ev = TaskEvent {
                id: id.clone(),
                task_id: id.clone(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind: TaskEventKind::Opened {
                    title: "demo task".into(),
                    body: String::new(),
                    repo: Some(PathBuf::from("/tmp/repo")),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: None,
                },
            };
            self.events.push(ev);
            id
        }

        fn reduce(&self) -> Result<TaskReadModel, ReduceError> {
            TaskReducer::reduce(&self.events)
        }
    }

    fn revision(n: u32, reviewer: Option<&str>) -> Revision {
        Revision {
            n,
            branch: format!("task/demo-r{n}"),
            worktree: None,
            upstream: "origin/main".into(),
            base: sha('0'),
            review_sha: sha(char::from_digit(n, 10).unwrap()),
            patch_id: sha('b'),
            reviewer: reviewer.map(String::from),
            session_ref: None,
        }
    }

    fn propose(log: &mut Log, task: &str, n: u32) {
        log.push(
            task,
            AUTHOR,
            TaskEventKind::RevisionProposed {
                revision: revision(n, Some(REVIEWER)),
            },
        );
    }

    fn pass(log: &mut Log, task: &str, reviewer: &str) -> TaskEvent {
        log.push(
            task,
            reviewer,
            TaskEventKind::ReviewPassed {
                reviewer: reviewer.to_string(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
        )
    }

    // ── ported roster (names kept from mission-control) ──────────────────

    #[test]
    fn happy_path_is_replay_equal_and_local_merge_is_not_landed() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            "verifier",
            TaskEventKind::MergedLocal {
                from_sha: sha('e'),
                to_sha: sha('1'),
            },
        );

        // Live fold == replay fold.
        let mut live = TaskReducer::new();
        for ev in &log.events {
            live.apply(ev).unwrap();
        }
        let replayed = log.reduce().unwrap();
        assert_eq!(live.model(), &replayed);

        // Local merge is NOT landed.
        let t = &replayed.tasks[&task];
        let rev = t.current_revision().unwrap();
        assert_eq!(rev.state, RevisionState::MergedLocal);
        assert!(rev.state.is_actionable() && !rev.state.is_terminal());
        assert!(rev.landed.is_none());
        assert_eq!(
            t.effective_state(),
            EffectiveState::Revision(RevisionState::MergedLocal)
        );

        // Then the verifier observes the land; only now is it terminal.
        log.push(
            &task,
            "verifier",
            TaskEventKind::Landed {
                upstream_head: sha('f'),
                observed_patch_id: sha('b'),
            },
        );
        let m = log.reduce().unwrap();
        let rev = m.tasks[&task].current_revision().unwrap().clone();
        assert_eq!(rev.state, RevisionState::Landed);
        assert!(rev.state.is_terminal());
        assert_eq!(rev.landed, Some((sha('f'), sha('b'))));
    }

    #[test]
    fn source_and_merge_failures_leave_ready_actionable() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            "verifier",
            TaskEventKind::SourceUnavailable {
                detail: "worktree gone".into(),
            },
        );
        log.push(
            &task,
            "verifier",
            TaskEventKind::MergeFailed {
                reason: MergeFailure::NonFastForward {},
            },
        );
        let m = log.reduce().unwrap();
        let rev = m.tasks[&task].current_revision().unwrap();
        assert_eq!(rev.state, RevisionState::Ready);
        assert!(rev.state.is_actionable());
        assert_eq!(rev.issues.len(), 2);
    }

    #[test]
    fn refute_after_pass_is_terminal_and_cannot_be_cured() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            REVIEWER,
            TaskEventKind::ReviewRefuted {
                reviewer: REVIEWER.into(),
                session_ref: None,
                receipts: None,
            },
        );
        let m = log.reduce().unwrap();
        assert_eq!(m.tasks[&task].current_revision().unwrap().state, RevisionState::Refuted);

        // A later pass on the refuted revision fails closed.
        pass(&mut log, &task, REVIEWER);
        let err = log.reduce().unwrap_err();
        assert!(
            matches!(err, ReduceError::InvalidTransition { event, .. } if event == "review_passed"),
            "{err:?}"
        );
    }

    #[test]
    fn reroute_changes_the_only_reviewer_whose_verdict_can_apply() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        log.push(
            &task,
            REVIEWER,
            TaskEventKind::ReviewRerouted {
                from: REVIEWER.into(),
                to: OTHER.into(),
            },
        );

        // Old reviewer's verdict no longer applies.
        pass(&mut log, &task, REVIEWER);
        let err = log.reduce().unwrap_err();
        assert!(matches!(err, ReduceError::ReviewerMismatch { .. }), "{err:?}");
        log.events.pop();

        // New reviewer's does.
        pass(&mut log, &task, OTHER);
        let m = log.reduce().unwrap();
        let rev = m.tasks[&task].current_revision().unwrap();
        assert_eq!(rev.state, RevisionState::Ready);
        assert_eq!(rev.pass.as_ref().unwrap().reviewer, OTHER);
        assert_eq!(rev.reroutes.len(), 1);
    }

    #[test]
    fn supersession_is_terminal_before_ref_movement() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Superseded {
                by_task: "other-task".into(),
            },
        );
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.state, TaskState::Superseded);
        assert!(t.state.is_terminal());

        // Nothing further applies to a superseded task.
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn reconcile_failure_stays_merged_local_and_blocks_relanding() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            "verifier",
            TaskEventKind::MergedLocal {
                from_sha: sha('e'),
                to_sha: sha('1'),
            },
        );
        log.push(
            &task,
            "verifier",
            TaskEventKind::ReconcileFailed {
                detail: "torn log tail".into(),
            },
        );
        let m = log.reduce().unwrap();
        let rev = m.tasks[&task].current_revision().unwrap();
        assert_eq!(rev.state, RevisionState::MergedLocal);
        assert_eq!(rev.issues.len(), 1);

        // A second MergedLocal cannot re-apply from MergedLocal.
        log.push(
            &task,
            "verifier",
            TaskEventKind::MergedLocal {
                from_sha: sha('e'),
                to_sha: sha('1'),
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn unknown_request_and_wrong_open_id_fail_closed() {
        let mut log = Log::new();
        log.push(
            "no-such-task",
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::UnknownTask { .. }));
        log.events.clear();

        // An Opened event whose id != task_id is refused.
        let ev = TaskEvent {
            id: "00000000-0000-7000-8000-000000000abc".into(),
            task_id: "00000000-0000-7000-8000-000000000def".into(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: AUTHOR.into(),
            kind: TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: "tasks".into(),
                assignee: None,
            },
        };
        assert!(matches!(
            TaskReducer::reduce(&[ev]).unwrap_err(),
            ReduceError::OpenIdMismatch { .. }
        ));
    }

    #[test]
    fn exact_duplicate_is_idempotent_but_id_reuse_is_loud() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        let claim = log.push(
            &task,
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );

        // Byte-identical re-append: no-op.
        let mut reducer = TaskReducer::new();
        for ev in &log.events {
            reducer.apply(ev).unwrap();
        }
        reducer.apply(&claim).unwrap();
        assert_eq!(reducer.model().tasks[&task].state, TaskState::Claimed);

        // Same id, different content: loud.
        let mut forged = claim.clone();
        forged.kind = TaskEventKind::Claimed { assignee: OTHER.into() };
        assert!(matches!(
            reducer.apply(&forged).unwrap_err(),
            ReduceError::EventIdCollision { .. }
        ));
    }

    #[test]
    fn merge_and_landed_evidence_must_match_reviewed_content() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);

        // MergedLocal whose to_sha is not the reviewed sha: refused.
        log.push(
            &task,
            "verifier",
            TaskEventKind::MergedLocal {
                from_sha: sha('e'),
                to_sha: sha('9'),
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::MergeTargetMismatch { .. }
        ));
        log.events.pop();

        // Landed whose patch-id is not the reviewed revision's: refused.
        log.push(
            &task,
            "verifier",
            TaskEventKind::Landed {
                upstream_head: sha('f'),
                observed_patch_id: sha('9'),
            },
        );
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::PatchIdMismatch { .. }));
    }

    // ── new roster (base lifecycle + facet interplay) ─────────────────────

    #[test]
    fn base_lifecycle_happy_path() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        log.push(&task, OTHER, TaskEventKind::Claimed { assignee: OTHER.into() });
        log.push(
            &task,
            OTHER,
            TaskEventKind::Noted {
                text: "halfway".into(),
                session_ref: None,
            },
        );
        log.push(
            &task,
            OTHER,
            TaskEventKind::Done {
                observed: Some("screenshot.png".into()),
                check: None,
            },
        );
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.state, TaskState::Done);
        assert_eq!(t.assignee.as_deref(), Some(OTHER));
        assert_eq!(t.notes.len(), 1);
        assert_eq!(t.done_observed.as_deref(), Some("screenshot.png"));
        assert_eq!(t.effective_state(), EffectiveState::Base(TaskState::Done));

        // Claim races: a second Claimed on a claimed task is refused (CAS at the store makes
        // this "first writer wins").
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn done_is_blocked_while_a_revision_is_live() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Claimed {
                assignee: AUTHOR.into(),
            },
        );
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);

        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::DoneWithLiveRevision { .. }
        ));
        log.events.pop();

        // But you can always kill it.
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Abandoned {
                reason: "wrong approach".into(),
            },
        );
        assert_eq!(log.reduce().unwrap().tasks[&task].state, TaskState::Abandoned);
        log.events.pop();

        // And once the revision lands, Done goes through.
        log.push(
            &task,
            "verifier",
            TaskEventKind::Landed {
                upstream_head: sha('f'),
                observed_patch_id: sha('b'),
            },
        );
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        let m = log.reduce().unwrap();
        assert_eq!(m.tasks[&task].state, TaskState::Done);
    }

    #[test]
    fn reproposal_supersedes_the_prior_revision() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        log.push(
            &task,
            REVIEWER,
            TaskEventKind::ReviewRefuted {
                reviewer: REVIEWER.into(),
                session_ref: None,
                receipts: None,
            },
        );
        propose(&mut log, &task, 2);
        pass(&mut log, &task, REVIEWER);

        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.revisions.len(), 2);
        assert_eq!(t.revisions[0].state, RevisionState::Refuted); // terminal stays terminal
        assert_eq!(t.revisions[1].state, RevisionState::Ready);
        assert_eq!(t.effective_state(), EffectiveState::Revision(RevisionState::Ready));

        // A live (non-terminal) revision gets auto-superseded by a re-proposal.
        propose(&mut log, &task, 3);
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.revisions[1].state, RevisionState::Superseded);
        assert_eq!(t.revisions[2].state, RevisionState::AwaitingReview);
    }

    #[test]
    fn landed_is_reachable_directly_from_ready() {
        // Deviation 2: a forge-PR land has no MergedLocal step; observation is state-complete.
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        propose(&mut log, &task, 1);
        pass(&mut log, &task, REVIEWER);
        log.push(
            &task,
            "verifier",
            TaskEventKind::Landed {
                upstream_head: sha('f'),
                observed_patch_id: sha('b'),
            },
        );
        let m = log.reduce().unwrap();
        let rev = m.tasks[&task].current_revision().unwrap();
        assert_eq!(rev.state, RevisionState::Landed);
        assert!(rev.local_merge.is_none());
    }

    fn posed(options: &[&str], default: &str) -> TaskEventKind {
        TaskEventKind::Posed {
            options: options.iter().map(|s| s.to_string()).collect(),
            default_choice: default.into(),
            deadline: None,
            source: None,
        }
    }

    /// The decision facet: posing makes a task a decision; resolving answers it with one of the
    /// posed options and is terminal; `done` is refused on a decision; a second pose is refused.
    #[test]
    fn decision_pose_resolve_round_trip_and_refusals() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        assert!(!log.reduce().unwrap().tasks[&task].is_decision());
        log.push(&task, AUTHOR, posed(&["keep", "delete"], "keep"));
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert!(t.is_decision());
        assert_eq!(t.kind(), "decision");
        assert_eq!(t.state, TaskState::Open, "posing does not change the base state");
        let d = t.decision.as_ref().unwrap();
        assert_eq!(d.default_choice, "keep");
        assert_eq!(d.alternatives().collect::<Vec<_>>(), vec!["delete"]);
        assert_eq!(d.posed_by, AUTHOR);
        assert!(d.resolution.is_none());

        // done is the wrong verb for a question.
        log.push(
            &task,
            OTHER,
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::DecisionNeedsResolve { .. }
        ));
        log.events.pop();

        // A choice outside the posed options is refused.
        log.push(
            &task,
            OTHER,
            TaskEventKind::Resolved {
                choice: "burn it".into(),
                note: None,
                provisional: false,
            },
        );
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::UnknownChoice { .. }));
        log.events.pop();

        // A second pose is refused: amendments are new decisions.
        log.push(&task, AUTHOR, posed(&["keep", "delete", "archive"], "keep"));
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::AlreadyPosed { .. }));
        log.events.pop();

        // Resolving with a non-default option: terminal, literal choice, accepted_default false.
        let ev = log.push(
            &task,
            OTHER,
            TaskEventKind::Resolved {
                choice: "delete".into(),
                note: Some("the twin goes".into()),
                provisional: false,
            },
        );
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.state, TaskState::Resolved);
        assert!(t.state.is_terminal());
        assert_eq!(t.effective_state().as_str(), "resolved");
        let r = t.decision.as_ref().unwrap().resolution.as_ref().unwrap();
        assert_eq!(
            (r.choice.as_str(), r.by.as_str(), r.accepted_default),
            ("delete", OTHER, false)
        );
        assert_eq!(r.event_id, ev.id);
        assert_eq!(r.note.as_deref(), Some("the twin goes"));

        // Nothing state-bearing applies after a resolution (terminal) — a tag included.
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Tagged {
                tags: vec!["late".into()],
            },
        );
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
    }

    /// `resolved` on a plain task is refused (there is nothing to choose from), and a pose whose
    /// default is not among its options — or that lists an option twice — never enters the log.
    #[test]
    fn resolve_needs_a_pose_and_a_pose_needs_its_default_among_the_options() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        log.push(
            &task,
            OTHER,
            TaskEventKind::Resolved {
                choice: "yes".into(),
                note: None,
                provisional: false,
            },
        );
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::MissingDecision { .. }));
        log.events.pop();
        log.push(&task, AUTHOR, posed(&["a", "b"], "c"));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidField { field: "options", .. }
        ));
        log.events.pop();
        log.push(&task, AUTHOR, posed(&["a", "a"], "a"));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidField { field: "options", .. }
        ));
        log.events.pop();
        log.push(&task, AUTHOR, posed(&["a"], ""));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidField { field: "default", .. }
        ));
        log.events.pop();
        // Accepting the default is recorded as the literal choice.
        log.push(&task, AUTHOR, posed(&["a", "b"], "a"));
        log.push(
            &task,
            OTHER,
            TaskEventKind::Resolved {
                choice: "a".into(),
                note: None,
                provisional: false,
            },
        );
        let m = log.reduce().unwrap();
        let r = m.tasks[&task].decision.as_ref().unwrap().resolution.as_ref().unwrap();
        assert!(r.accepted_default);
        assert_eq!(r.choice, "a");
    }

    /// A note on a terminal task appends (marked `post_close`) and never changes the task's state —
    /// for every terminal state. Tags stay refused: they change views, not the record.
    #[test]
    fn post_close_note_appends_and_never_changes_effective_state() {
        let terminals: Vec<(&str, Vec<TaskEventKind>)> = vec![
            (
                "done",
                vec![TaskEventKind::Done {
                    observed: None,
                    check: None,
                }],
            ),
            ("abandoned", vec![TaskEventKind::Abandoned { reason: "r".into() }]),
            (
                "superseded",
                vec![TaskEventKind::Superseded {
                    by_task: "other".into(),
                }],
            ),
            (
                "resolved",
                vec![
                    posed(&["a", "b"], "a"),
                    TaskEventKind::Resolved {
                        choice: "b".into(),
                        note: None,
                        provisional: false,
                    },
                ],
            ),
        ];
        for (name, close) in terminals {
            let mut log = Log::new();
            let task = log.open(AUTHOR);
            log.push(
                &task,
                AUTHOR,
                TaskEventKind::Noted {
                    text: "before".into(),
                    session_ref: None,
                },
            );
            for k in close {
                log.push(&task, AUTHOR, k);
            }
            let before = log.reduce().unwrap().tasks[&task].clone();
            assert_eq!(before.effective_state().as_str(), name);
            log.push(
                &task,
                OTHER,
                TaskEventKind::Noted {
                    text: "the evidence, after the close".into(),
                    session_ref: None,
                },
            );
            let after = log.reduce().unwrap().tasks[&task].clone();
            assert_eq!(after.effective_state(), before.effective_state(), "{name}: state moved");
            assert_eq!(after.state, before.state, "{name}");
            assert_eq!(after.decision, before.decision, "{name}: the resolution is untouched");
            assert_eq!(after.notes.len(), 2, "{name}");
            assert!(!after.notes[0].post_close, "{name}: a live note is not post-close");
            assert!(after.notes[1].post_close, "{name}: the late note is marked");
            assert_eq!(after.notes[1].by, OTHER);
            // Tags are still refused on a terminal task.
            log.push(
                &task,
                AUTHOR,
                TaskEventKind::Tagged {
                    tags: vec!["late".into()],
                },
            );
            assert!(
                matches!(log.reduce().unwrap_err(), ReduceError::InvalidTransition { .. }),
                "{name}: tags stay refused"
            );
        }
    }

    /// `post_close` is wire-additive: a live note serializes exactly as before the field existed,
    /// and a note recorded before it existed deserializes as live.
    #[test]
    fn post_close_is_wire_additive() {
        let n = Note {
            event_id: "e".into(),
            by: "b".into(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            text: "t".into(),
            session_ref: None,
            post_close: false,
        };
        let v = serde_json::to_value(&n).unwrap();
        assert!(v.get("post_close").is_none(), "{v}");
        let back: Note = serde_json::from_value(v).unwrap();
        assert!(!back.post_close);
        let late = Note { post_close: true, ..n };
        assert_eq!(serde_json::to_value(&late).unwrap()["post_close"], true);
    }

    /// Provisional resolutions: the poser resolves on the default (state `resolved`, provisional);
    /// the decider confirms or vetoes it with a non-provisional resolve, which REPLACES it (the
    /// provisional one is kept); a third party cannot override; a second non-provisional resolve
    /// is refused as before; a provisional choice must be the default.
    #[test]
    fn provisional_resolution_is_overridden_only_by_the_decider() {
        const DECIDER: &str = "ember";
        let resolved = |choice: &str, provisional: bool| TaskEventKind::Resolved {
            choice: choice.into(),
            note: None,
            provisional,
        };
        let setup = || {
            let mut log = Log::new();
            let id = log.next_id();
            log.events.push(TaskEvent {
                id: id.clone(),
                task_id: id.clone(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: AUTHOR.into(),
                kind: TaskEventKind::Opened {
                    title: "a decision".into(),
                    body: String::new(),
                    repo: None,
                    issue: None,
                    channel: "tasks".into(),
                    assignee: Some(DECIDER.into()),
                },
            });
            log.push(&id, AUTHOR, posed(&["keep", "delete"], "keep"));
            (log, id)
        };

        // A provisional choice other than the default is refused.
        let (mut log, task) = setup();
        log.push(&task, AUTHOR, resolved("delete", true));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::ProvisionalNotDefault { .. }
        ));
        log.events.pop();

        // The poser resolves provisionally on the default: resolved, awaiting veto.
        log.push(&task, AUTHOR, resolved("keep", true));
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.state, TaskState::Resolved);
        let d = t.decision.as_ref().unwrap();
        assert!(d.awaiting_veto());
        assert!(d.resolution.as_ref().unwrap().provisional);
        assert_eq!(d.resolution.as_ref().unwrap().by, AUTHOR);

        // A third party cannot override; nor can the poser re-resolve, provisionally or not.
        for (by, prov) in [(OTHER, false), (AUTHOR, false), (AUTHOR, true), (DECIDER, true)] {
            log.push(&task, by, resolved("delete", prov));
            assert!(log.reduce().is_err(), "{by} provisional={prov} must be refused");
            log.events.pop();
        }
        log.push(&task, OTHER, resolved("delete", false));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::ProvisionalOverrideNotDecider { .. }
        ));
        log.events.pop();

        // The decider vetoes (through the web inbox: `web:ember` is ember): the veto replaces the
        // provisional resolution, which is kept.
        let veto = log.push(&task, "web:ember", resolved("delete", false));
        let m = log.reduce().unwrap();
        let t = &m.tasks[&task];
        assert_eq!(t.state, TaskState::Resolved);
        let d = t.decision.as_ref().unwrap();
        assert!(!d.awaiting_veto());
        let r = d.resolution.as_ref().unwrap();
        assert_eq!(
            (r.choice.as_str(), r.provisional, r.event_id.as_str()),
            ("delete", false, veto.id.as_str())
        );
        let p = d.superseded_provisional.as_ref().unwrap();
        assert_eq!(
            (p.choice.as_str(), p.by.as_str(), p.provisional),
            ("keep", AUTHOR, true)
        );

        // A second non-provisional resolve is refused as it always was.
        log.push(&task, DECIDER, resolved("keep", false));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
        log.events.pop();

        // Confirming is the same event with the provisional choice.
        let (mut log, task) = setup();
        log.push(&task, AUTHOR, resolved("keep", true));
        log.push(&task, DECIDER, resolved("keep", false));
        let m = log.reduce().unwrap();
        let d = m.tasks[&task].decision.as_ref().unwrap();
        assert!(!d.awaiting_veto());
        assert!(d.resolution.as_ref().unwrap().accepted_default);
        assert!(d.superseded_provisional.is_some());

        // An ordinary (non-provisional) resolution is final: the decider cannot re-resolve it.
        let (mut log, task) = setup();
        log.push(&task, DECIDER, resolved("keep", false));
        log.push(&task, DECIDER, resolved("delete", false));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
    }

    /// A pinned STATUS replaces the previous one (history stays in the events), may be set by the
    /// assignee or the opener only, and is refused on a terminal task.
    #[test]
    fn status_replaces_is_held_by_assignee_or_opener_and_refused_when_terminal() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        let status = |t: &str| TaskEventKind::StatusSet { text: t.into() };
        log.push(&task, AUTHOR, status("gen 1: tip aaa"));
        log.push(&task, OTHER, TaskEventKind::Claimed { assignee: OTHER.into() });
        log.push(&task, OTHER, status("gen 2: tip bbb; next: merge C"));
        let m = log.reduce().unwrap();
        let s = m.tasks[&task].status.as_ref().unwrap();
        assert_eq!(
            (s.text.as_str(), s.by.as_str(), s.revisions),
            ("gen 2: tip bbb; next: merge C", OTHER, 2)
        );
        assert_eq!(
            m.tasks[&task].effective_state().as_str(),
            "claimed",
            "a status never moves state"
        );
        // The opener still may; a third party may not.
        log.push(&task, AUTHOR, status("gen 3"));
        log.reduce().unwrap();
        log.push(&task, "agent:stranger", status("hijack"));
        assert!(matches!(log.reduce().unwrap_err(), ReduceError::StatusNotHolder { .. }));
        log.events.pop();
        // Terminal: refused (state-bearing), unlike a note.
        log.push(
            &task,
            OTHER,
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        log.push(&task, OTHER, status("after the end"));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::InvalidTransition { .. }
        ));
        log.events.pop();
        assert_eq!(
            log.reduce().unwrap().tasks[&task].status.as_ref().unwrap().text,
            "gen 3"
        );
    }
}
