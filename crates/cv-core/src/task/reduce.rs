//! Pure task reducer: fold [`TaskEvent`]s into a [`TaskReadModel`]. No I/O, no git, no clock —
//! the same [`TaskReducer::apply`] runs during live appends and during replay, so live state and
//! replayed state cannot diverge (replay parity).
//!
//! The machine: a base lifecycle `Open → Claimed → Done`, terminals `Abandoned`/`Superseded`,
//! `Noted` for progress (accepted on a terminal task too, marked `post_close`: terminal is about
//! state, not the record); a decision facet (`Posed`, answered by `Resolved`, provisional-then-
//! confirm the one override); a pinned STATUS. Ids are uuid v7 and idempotent: a byte-identical
//! re-append is a no-op, id reuse with different content is loud.
//!
//! The land facet retired in 0.15.0 left nine event kinds in old logs
//! ([`super::model::RETIRED_TAGS`]); they apply as no-ops, so such a log folds to the model it
//! would have without them.
//!
//! Two kinds of rule live here. [`TaskReducer::apply`] says what the log *means* and never
//! tightens: every line ever written must fold the same way forever. [`TaskReducer::admit`] says
//! what may be *written today* against the current model — the store runs it on every candidate
//! before `apply` — and may tighten between releases (a retired kind, a bare `decision` tag) without
//! touching how an old log replays. Both are pure, so CLI, MCP and HTTP get the same refusals.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{TaskEvent, TaskEventKind, TaskState};

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
    #[error("invalid {field}: {value:?}")]
    InvalidField { field: &'static str, value: String },
    #[error("event kind {tag:?} is not one this cv knows (neither live nor retired) — upgrade cv or repair the log")]
    UnknownKind { tag: String },
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

    // ── write-time rules ([`TaskReducer::admit`]) ────────────────────────
    #[error(
        "event kind {tag:?} was retired in 0.15.0 with the task land facet and is never written again \
         (an existing log that holds it still replays)"
    )]
    RetiredKind { tag: String },
    #[error(
        "task {task_id}: a decision is posed, not tagged — a `decision` tag with no options gives the decider \
         nothing to choose. Pose it:\n  cv task decide {title:?} --for <who> --default \"<the option that stands if \
         nobody speaks>\" [--option \"<alternative>\"]…\n  (`--default` is required; `--provisional` proceeds on it \
         now with a veto window; `--blocks <id>` makes a task wait on the answer)"
    )]
    BareDecisionTag { task_id: String, title: String },
}

/// A progress note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub event_id: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<super::Claim>,
    pub session_ref: Option<String>,
    /// Appended after the task reached a terminal state. Terminal is about STATE, not the record:
    /// a lane that marks `done` and then writes its evidence keeps the evidence, and the note
    /// changes nothing else. Omitted from the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub post_close: bool,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prerequisite {
    pub require_success: bool,
    pub final_only: bool,
}

/// Read model for one task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskProjection {
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub repo: Option<PathBuf>,
    pub issue: Option<String>,
    pub state: TaskState,
    pub assignee: Option<String>,
    pub opened_by: String,
    pub opened_at: DateTime<Utc>,
    pub last_event_id: String,
    pub last_ts: DateTime<Utc>,
    pub notes: Vec<Note>,
    pub superseded_by: Option<String>,
    pub abandoned_reason: Option<String>,
    /// The evidence the agent offered with its `done` — self-reported free text, never checked.
    pub done_observed: Option<String>,
    /// Who appended the `done`: the claimant whose own validation runs its report is read against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_by: Option<String>,
    /// Events applied to this task, its `opened` included: a note's cursor on a peer task.
    #[serde(default)]
    pub event_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<super::EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<String>,
    /// Only strengthened edges. Unqualified prerequisites retain their original wire representation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prerequisites: BTreeMap<String, Prerequisite>,
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
            assignee,
            ..
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
            state: TaskState::Open,
            assignee: assignee.clone(),
            opened_by: event.by.clone(),
            opened_at: event.ts,
            last_event_id: event.id.clone(),
            last_ts: event.ts,
            notes: Vec::new(),
            superseded_by: None,
            abandoned_reason: None,
            done_observed: None,
            done_by: None,
            event_count: 1,
            evidence: vec![],
            disclosure: None,
            prerequisites: Default::default(),
            tags: Vec::new(),
            blocked_by: Vec::new(),
            decision: None,
            status: None,
        }
    }

    fn touch(&mut self, event: &TaskEvent) {
        self.event_count += 1;
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
    /// Events folded before the checkpoint this reducer resumed from, by id: each one's
    /// [`event_digest`], all a later event with the same id is compared against.
    folded: HashMap<String, String>,
}

/// The digest of an event's JSON: equal events have equal digests, so a resumed reducer still
/// tells an idempotent re-append from an id collision.
pub(super) fn event_digest(event: &TaskEvent) -> String {
    let mut digest = crate::digest::Sha256::new();
    digest.update(&serde_json::to_vec(event).expect("a task event serializes"));
    digest.finish_hex()
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

    /// The state a checkpoint keeps: the model and every applied event's digest.
    pub(super) fn checkpoint(&self) -> (TaskReadModel, std::collections::BTreeMap<String, String>) {
        let mut applied: std::collections::BTreeMap<String, String> = self
            .folded
            .iter()
            .map(|(id, digest)| (id.clone(), digest.clone()))
            .collect();
        applied.extend(self.applied.iter().map(|(id, event)| (id.clone(), event_digest(event))));
        (self.model.clone(), applied)
    }

    /// A reducer standing where [`Self::checkpoint`] was taken.
    pub(super) fn resume(model: TaskReadModel, applied: std::collections::BTreeMap<String, String>) -> Self {
        Self {
            model,
            applied: HashMap::new(),
            folded: applied.into_iter().collect(),
        }
    }

    pub fn into_model(self) -> TaskReadModel {
        self.model
    }

    /// May `event` be WRITTEN now, given the model so far? Replay never asks this — an old log is
    /// folded by [`apply`](Self::apply) alone — so a rule here tightens what new writers may say
    /// without changing what any existing line means. The store runs it on every candidate of an
    /// agent-facing append, which is why CLI, MCP and HTTP cannot disagree about it.
    pub fn admit(&self, event: &TaskEvent) -> Result<(), ReduceError> {
        match &event.kind {
            TaskEventKind::Retired(r) if r.is_known() => Err(ReduceError::RetiredKind {
                tag: r.tag_str().to_string(),
            }),
            TaskEventKind::Retired(r) => Err(ReduceError::UnknownKind {
                tag: r.tag_str().to_string(),
            }),
            TaskEventKind::Opened { channel: Some(_), .. } => Err(ReduceError::InvalidField {
                field: "channel",
                value: "removed in 0.15.0 (read-through only)".into(),
            }),
            TaskEventKind::Noted { claim: Some(c), .. } if !c.admissible() => Err(ReduceError::InvalidField {
                field: "claim",
                value: "invalid scope, condition, or claim size".into(),
            }),
            TaskEventKind::Done {
                evidence, disclosure, ..
            } if evidence.len() > 64
                || disclosure.as_ref().is_some_and(|s| s.len() > 8192)
                || serde_json::to_vec(evidence).is_ok_and(|v| v.len() > 65536) =>
            {
                Err(ReduceError::InvalidField {
                    field: "evidence",
                    value: "completion evidence exceeds admission limit".into(),
                })
            }
            TaskEventKind::Done { check: Some(_), .. } => Err(ReduceError::InvalidField {
                field: "check",
                value: "removed in 0.15.0 (read-through only)".into(),
            }),
            TaskEventKind::Superseded { by_task } if *by_task == event.task_id => Err(ReduceError::InvalidField {
                field: "by_task",
                value: "a task cannot supersede itself".into(),
            }),
            // One way to pose a decision: a `decision` tag on a task with nothing posed made an
            // inbox card with no options and no buttons (two such cards, 10-01). The tag is what
            // `pose` wrote before 0.15.0 and what hands wrote before `decide` existed; old logs
            // keep it, new writers pose.
            TaskEventKind::Tagged { tags } if tags.iter().any(|t| t == super::project::DECISION_TAG) => {
                match self.model.tasks.get(&event.task_id) {
                    Some(task) if task.decision.is_none() => Err(ReduceError::BareDecisionTag {
                        task_id: task.task_id.clone(),
                        title: task.title.clone(),
                    }),
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
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
        if let Some(previous) = self.folded.get(&event.id) {
            return if *previous == event_digest(event) {
                Ok(())
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
                event: kind.tag(),
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
            TaskEventKind::Noted {
                text,
                session_ref,
                claim,
            } => {
                // The one event a terminal task accepts unconditionally: a note never changes
                // state, so refusing it only lost evidence (four lanes wrote `done` before their
                // note and lost the note). It is marked `post_close` so every surface can say so.
                task.notes.push(Note {
                    event_id: event.id.clone(),
                    by: event.by.clone(),
                    ts: event.ts,
                    text: text.clone(),
                    session_ref: session_ref.clone(),
                    claim: claim.clone(),
                    post_close: task.state.is_terminal(),
                });
            }
            TaskEventKind::Done {
                observed,
                evidence,
                disclosure,
                ..
            } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                if task.decision.is_some() {
                    return Err(ReduceError::DecisionNeedsResolve {
                        task_id: task.task_id.clone(),
                    });
                }
                task.evidence = evidence.clone();
                task.disclosure = disclosure.clone();
                task.done_observed = observed.clone();
                task.done_by = Some(event.by.clone());
                task.state = TaskState::Done;
            }
            TaskEventKind::Abandoned { reason } => {
                // Always available on a non-terminal task: you can always kill a task.
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
            TaskEventKind::BlockedBy {
                task: blocker,
                require_success,
                final_only,
            } => {
                require_base(task, kind, &[TaskState::Open, TaskState::Claimed])?;
                if *require_success || *final_only {
                    let edge = task.prerequisites.entry(blocker.clone()).or_insert(Prerequisite {
                        require_success: false,
                        final_only: false,
                    });
                    edge.require_success |= *require_success;
                    edge.final_only |= *final_only;
                }
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
                        event: kind.tag(),
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
                if !task
                    .assignee
                    .as_deref()
                    .is_some_and(|who| super::project::same_actor(&event.by, who))
                    && !super::project::same_actor(&event.by, &task.opened_by)
                {
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

            // ── retired kinds (0.15.0) ────────────────────────────────────
            TaskEventKind::Retired(r) => {
                // A kind cv itself used to write replays as a no-op: the log keeps the record, the
                // model derives nothing from it (not even `last_ts`). A tag cv never wrote is a
                // different matter — refused, so a log from a newer cv is loud, never misread.
                if !r.is_known() {
                    return Err(ReduceError::UnknownKind {
                        tag: r.tag_str().to_string(),
                    });
                }
                return Ok(());
            }
        }
        task.touch(event);
        Ok(())
    }
}

fn require_base(task: &TaskProjection, kind: &TaskEventKind, allowed: &[TaskState]) -> Result<(), ReduceError> {
    if allowed.contains(&task.state) {
        Ok(())
    } else {
        Err(ReduceError::InvalidTransition {
            task_id: task.task_id.clone(),
            state: task.state.as_str().to_string(),
            event: kind.tag(),
        })
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

fn validate_event_shape(event: &TaskEvent) -> Result<(), ReduceError> {
    require_nonempty("id", &event.id)?;
    require_nonempty("task_id", &event.task_id)?;
    require_nonempty("by", &event.by)?;
    match &event.kind {
        TaskEventKind::Opened { title, assignee, .. } => {
            require_nonempty("title", title)?;
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
        TaskEventKind::BlockedBy { task, .. } => {
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
        TaskEventKind::Retired(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR: &str = "agent:author";
    const OTHER: &str = "agent:other";

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
                    channel: None,
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

    // ── ported roster (names kept from mission-control) ──────────────────

    #[test]
    fn write_admission_refuses_retired_payloads_unknown_kinds_and_self_supersession_without_changing_replay() {
        let mut log = Log::new();
        let id = log.open(AUTHOR);
        let mut historical_open = log.events[0].clone();
        if let TaskEventKind::Opened { channel, .. } = &mut historical_open.kind {
            *channel = Some("old board".into());
        }
        let mut reducer = TaskReducer::new();
        assert!(matches!(
            reducer.admit(&historical_open),
            Err(ReduceError::InvalidField { field: "channel", .. })
        ));
        reducer.apply(&historical_open).unwrap();
        let historical_done = TaskEvent {
            id: "old-done".into(),
            task_id: id.clone(),
            ts: historical_open.ts,
            by: AUTHOR.into(),
            kind: TaskEventKind::Done {
                evidence: vec![],
                disclosure: None,
                observed: None,
                check: Some(serde_json::json!({"cmd":"old check"})),
            },
        };
        assert!(matches!(
            reducer.admit(&historical_done),
            Err(ReduceError::InvalidField { field: "check", .. })
        ));
        reducer.apply(&historical_done).unwrap();
        assert_eq!(
            reducer.model().tasks[&id].state,
            TaskState::Done,
            "historical replay remains supported"
        );
        let alien = TaskEvent {
            id: "alien".into(),
            task_id: id.clone(),
            ts: historical_open.ts,
            by: AUTHOR.into(),
            kind: TaskEventKind::Retired(super::super::model::RetiredEvent(
                serde_json::from_value(serde_json::json!({"event":"not-a-known-kind"})).unwrap(),
            )),
        };
        assert!(matches!(reducer.admit(&alien), Err(ReduceError::UnknownKind { .. })));
        let mut reducer = TaskReducer::new();
        reducer.apply(&historical_open).unwrap();
        let self_supersede = TaskEvent {
            id: "self".into(),
            task_id: id.clone(),
            ts: historical_open.ts,
            by: AUTHOR.into(),
            kind: TaskEventKind::Superseded { by_task: id.clone() },
        };
        assert!(matches!(
            reducer.admit(&self_supersede),
            Err(ReduceError::InvalidField { field: "by_task", .. })
        ));
        assert_eq!(reducer.model().tasks[&id].state, TaskState::Open);
        reducer.apply(&self_supersede).unwrap();
        assert_eq!(
            reducer.model().tasks[&id].state,
            TaskState::Superseded,
            "do not retroactively change old records"
        );
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
            kind: crate::task::opened_kind("t", None),
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
                claim: None,
                text: "halfway".into(),
                session_ref: None,
            },
        );
        log.push(
            &task,
            OTHER,
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
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
                evidence: Vec::new(),
                disclosure: None,
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
                    evidence: Vec::new(),
                    disclosure: None,
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
                    claim: None,
                    text: "before".into(),
                    session_ref: None,
                },
            );
            for k in close {
                log.push(&task, AUTHOR, k);
            }
            let before = log.reduce().unwrap().tasks[&task].clone();
            assert_eq!(before.state.as_str(), name);
            log.push(
                &task,
                OTHER,
                TaskEventKind::Noted {
                    claim: None,
                    text: "the evidence, after the close".into(),
                    session_ref: None,
                },
            );
            let after = log.reduce().unwrap().tasks[&task].clone();
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
            claim: None,
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
                kind: crate::task::opened_kind("a decision", Some(DECIDER)),
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
        assert_eq!(m.tasks[&task].state.as_str(), "claimed", "a status never moves state");
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
                evidence: Vec::new(),
                disclosure: None,
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

    /// The land facet's retired kinds replay as no-ops — a log written before 0.15.0 folds to the
    /// same model as if they were never there, and none of them gates `done` — while a tag this cv
    /// never wrote is refused (a log from a newer cv is loud, never misread).
    #[test]
    fn retired_kinds_are_no_ops_and_unknown_kinds_are_refused() {
        let mut log = Log::new();
        let task = log.open(AUTHOR);
        let before = log.reduce().unwrap();
        let retired = |tag: &str| -> TaskEventKind {
            serde_json::from_value(serde_json::json!({"event": tag, "reviewer": "r", "detail": "d"})).unwrap()
        };
        for tag in crate::task::model::RETIRED_TAGS {
            log.push(&task, "cv-verify", retired(tag));
        }
        let after = log.reduce().unwrap();
        assert_eq!(after, before, "nine retired events change nothing, not even last_ts");
        log.push(
            &task,
            AUTHOR,
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: Some("my word".into()),
                check: None,
            },
        );
        assert_eq!(log.reduce().unwrap().tasks[&task].state, TaskState::Done);
        log.push(&task, AUTHOR, retired("teleported"));
        assert!(matches!(
            log.reduce().unwrap_err(),
            ReduceError::UnknownKind { ref tag } if tag == "teleported"
        ));
    }

    /// Write-time rules are not replay rules: a bare `decision` tag is refused by `admit` on a task
    /// with nothing posed, admitted once a decision is posed, and `apply` folds the old shape
    /// (`tagged decision` before `posed`, or alone) exactly as it always did. A retired kind is
    /// refused by `admit` and a no-op under `apply`.
    #[test]
    fn admit_refuses_what_replay_still_folds() {
        let mut log = Log::new();
        let work = log.open(AUTHOR);
        let mut r = TaskReducer::new();
        for ev in &log.events {
            r.apply(ev).unwrap();
        }
        let bare = TaskEvent {
            id: "00000000-0000-7000-8000-0000000000aa".into(),
            task_id: work.clone(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: AUTHOR.into(),
            kind: TaskEventKind::Tagged {
                tags: vec!["deploy".into(), "decision".into()],
            },
        };
        let err = r.admit(&bare).unwrap_err();
        assert!(
            matches!(&err, ReduceError::BareDecisionTag { title, .. } if title == "demo task"),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("cv task decide \"demo task\"") && msg.contains("--default"),
            "{msg}"
        );
        // Replay of the same event: the log keeps what was written.
        r.apply(&bare).unwrap();
        assert_eq!(r.model().tasks[&work].tags, ["deploy", "decision"]);

        // Posed first: the tag is admitted (a decision may be tagged anything).
        let posed = log.push(
            &work,
            AUTHOR,
            TaskEventKind::Posed {
                options: vec!["a".into()],
                default_choice: "a".into(),
                deadline: None,
                source: None,
            },
        );
        let mut r2 = TaskReducer::new();
        r2.apply(&log.events[0]).unwrap();
        r2.apply(&posed).unwrap();
        r2.admit(&bare).unwrap();
        // A tag without `decision` is nobody's business.
        let plain = TaskEvent {
            kind: TaskEventKind::Tagged {
                tags: vec!["lean".into()],
            },
            ..bare.clone()
        };
        r.admit(&plain).unwrap();

        let retired: TaskEvent = serde_json::from_str(&format!(
            r#"{{"id":"00000000-0000-7000-8000-0000000000bb","task_id":"{work}","ts":"2026-07-16T12:00:00Z","by":"cv-verify","event":"landed","upstream_head":"e"}}"#
        ))
        .unwrap();
        assert!(matches!(r.admit(&retired), Err(ReduceError::RetiredKind { tag }) if tag == "landed"));
        r.apply(&retired).unwrap();
        assert_eq!(r.model().tasks[&work].state, TaskState::Open);
    }
}
