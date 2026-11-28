//! 📋 The task substrate — durable, replayable dispatch objects for agent fleets.
//!
//! Tasks let a fleet *commit to work* and let a human see what has been said about it. A task
//! is a durable object with a replay-derived lifecycle; a decision is a task with a posed
//! question. The log is the fleet's one record: follow it with `cv task events --since <id>`.
//!
//! # The three laws
//!
//! 1. **cv never claims to have observed what it has not.** A completion is the agent's own
//!    word: `Done { observed }` is recorded and labeled self-reported, and nothing in cv turns it
//!    into a verified fact. What git can say about a task is a `sweep` candidate, never a close.
//! 2. **No authority machinery.** No seats, grants, tokens or debts. Identity (`CV_ENDPOINT`,
//!    `--from`) is recorded, never authenticated.
//! 3. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
//!    design-notes graveyard, next to the 200k-line system this replaced.
//!
//! Until 0.15.0 a **land facet** (reviewed code revisions, a git verifier, TOFU tokens,
//! reviewer receipts, completion checks) lived here too. Its nine event kinds are
//! [`model::RETIRED_TAGS`]: a log that holds them still replays, as state-neutral events.
//!
//! Storage is a single append-only event log (`$CLUSTERVISION_HOME/tasks/events.jsonl`) under a
//! crash-safe flock.

pub mod claims;
pub use claims::{AllOf, Applicability, Claim, ClaimKind, Condition, EvidenceRef, FileDigests};
pub mod inbox;
pub mod model;
pub mod ops;
pub mod project;
pub mod reduce;
pub mod stall;
pub mod store;
pub mod views;
pub mod witness;

pub use inbox::{
    deadline_phrase, decisions_from_notes, inbox_page, inbox_page_brief, DecisionView, InboxCounts, InboxItem,
    InboxPage, NoteView, ResolutionView,
};
pub use model::{RetiredEvent, TaskEvent, TaskEventKind, TaskState, LIVE_TAGS, RETIRED_TAGS};
pub use ops::{Answer, DecisionSpec, EventFilter, Since};
pub use project::{
    age_short, blocks, effective_display, in_scope, inbox, involves, is_blocked, list, parse_deadline, parse_duration,
    parse_since, resolve_id, same_actor, unique_prefix_len, uuid_v7_timestamp, InboxEntry, InboxReason, TaskFilter,
    DECISION_TAG, DISCUSS_TAG, STATE_VOCABULARY,
};
pub use reduce::{Decision, Note, PinnedStatus, ReduceError, Resolution, TaskProjection, TaskReadModel, TaskReducer};
pub use store::{fold, fold_readonly, new_event, replay, Folded, ReplayOutcome, TaskStore};
pub use views::TaskRow;

/// The tests' `Opened` kernel: a titled plain task with no repo, issue, channel and no assignee
/// unless named.
#[cfg(test)]
pub(crate) fn opened_kind(title: &str, assignee: Option<&str>) -> TaskEventKind {
    TaskEventKind::Opened {
        title: title.into(),
        body: String::new(),
        repo: None,
        issue: None,
        channel: None,
        assignee: assignee.map(str::to_string),
    }
}

/// The fleet identity convention (audit G4): the spawner sets `CV_ENDPOINT=agent:<pane>` in each
/// agent's environment, and cv front-ends derive their default `--from` here — the *bare*
/// command records the right actor. This is environment, not assertion: nothing verifies the
/// string (law 2, no authority machinery); it only stops a forgotten flag from silently
/// detaching an agent from its own inbox and misattributing the durable record forever.
pub fn default_endpoint() -> Option<String> {
    let v = crate::env::var("CV_ENDPOINT").ok()?;
    let v = v.trim();

    (!v.is_empty()).then(|| v.to_string())
}

/// Identity resolution (G4) for verbs where the actor is bookkeeping, not semantics
/// (open/note/done/abandon/supersede): explicit actor wins, else the spawner-set
/// `CV_ENDPOINT`, else the surface's deliberate default sink — `"cv"` for CLI chat-grade verbs
/// (a human at a shell owes no ceremony), `"agent"` for MCP (the legacy anonymous sink).
pub fn actor(explicit: Option<String>, default_sink: &str) -> String {
    explicit
        .or_else(default_endpoint)
        .unwrap_or_else(|| default_sink.to_string())
}

/// Identity resolution for identity-BEARING verbs (claim/release/status/resolve), whose endpoint
/// string keys inbox and decision semantics: no resolvable identity is an error naming the cure,
/// never a silent fall-through to a shared sink. `flag` is the surface's spelling of the explicit
/// parameter (`--from` for the CLI, `` `from` `` for MCP), so the error names the exact knob the
/// caller has.
pub fn require_actor(explicit: Option<String>, flag: &str) -> anyhow::Result<String> {
    explicit
        .or_else(default_endpoint)
        .ok_or_else(|| anyhow::anyhow!("set CV_ENDPOINT or pass {flag}; identity-bearing events must record who acted"))
}

/// What one agent-facing append produced: the durable event, the task's new state, and the
/// advisory warnings to surface.
#[derive(Debug)]
pub struct AppendOutcome {
    pub event: TaskEvent,
    /// The task's state at this append's commit boundary, before any later concurrent writes.
    pub effective_state: Option<String>,
    /// The caller's carried-in advisory warnings — the set MCP ships in its JSON reply.
    pub warnings: Vec<String>,
}

/// The one agent-facing append path shared by the CLI and MCP: append the event through the
/// store's CAS and report the task state from that same commit boundary.
pub fn append(
    store: &TaskStore,
    task_id: Option<&str>,
    from: &str,
    kind: TaskEventKind,
    warnings: Vec<String>,
) -> anyhow::Result<AppendOutcome> {
    let mut committed = store.append_committed(vec![new_event(task_id, from, kind)])?;
    let event = committed.events.pop().expect("one committed event");
    let proj = committed.model.tasks.get(&event.task_id);
    Ok(AppendOutcome {
        effective_state: proj.map(effective_display),
        event,
        warnings,
    })
}

/// [`append`] for several events the caller built with [`new_event`], appended as ONE unit
/// ([`TaskStore::append_agent_events`]): all candidates are validated before writing. Later events may name the task
/// the FIRST one opens (`cv task decide`: the `opened` id must be known before the
/// `tagged`/`posed`/`resolved` that follow it are built). The outcome's `effective_state` is the
/// LAST event's task after the batch; `events` holds all of them in append order.
pub fn append_events(
    store: &TaskStore,
    candidates: Vec<TaskEvent>,
    warnings: Vec<String>,
) -> anyhow::Result<BatchOutcome> {
    if candidates.is_empty() {
        return Ok(BatchOutcome {
            events: candidates,
            effective_state: None,
            effective_states: Default::default(),
            warnings,
            replay_warnings: store.replay()?.warnings,
        });
    }
    let committed = store.append_committed(candidates)?;
    let events = committed.events;
    let mut effective_states = std::collections::BTreeMap::new();
    for event in &events {
        if let Some(task) = committed.model.tasks.get(&event.task_id) {
            effective_states
                .entry(event.task_id.clone())
                .or_insert_with(|| effective_display(task));
        }
    }
    let effective_state = events.last().and_then(|e| effective_states.get(&e.task_id)).cloned();
    Ok(BatchOutcome {
        events,
        effective_state,
        effective_states,
        warnings,
        replay_warnings: Vec::new(),
    })
}

/// What one batch append produced (see [`append_events`]).
#[derive(Debug)]
pub struct BatchOutcome {
    pub events: Vec<TaskEvent>,
    /// The last event's task, after the batch.
    pub effective_state: Option<String>,
    /// Each touched task's state after the whole batch, captured under the append lock.
    pub effective_states: std::collections::BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub replay_warnings: Vec<String>,
}

use std::path::PathBuf;

/// Root dir for the task log: `$CLUSTERVISION_HOME/tasks` (or `~/.clustervision/tasks`).
pub fn tasks_dir() -> PathBuf {
    crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tasks")
}

#[cfg(test)]
mod tests {
    use super::default_endpoint;

    #[test]
    fn append_reports_the_committed_batch_states_and_preserves_empty_read_warnings() {
        use super::*;
        let world = cv_testkit::World::new("task-committed-reports");
        let store = TaskStore::at(world.base.join("tasks"));
        let open = |title: &str| new_event(None, "agent:author", opened_kind(title, None));
        let first = open("first");
        let second = open("second");
        let claim = new_event(
            Some(&first.task_id),
            "agent:worker",
            TaskEventKind::Claimed {
                assignee: "agent:worker".into(),
            },
        );
        let candidates = vec![first.clone(), second.clone(), claim];
        let report = append_events(&store, candidates.clone(), vec!["caller advisory".into()]).unwrap();
        assert_eq!(report.events, candidates);
        assert_eq!(report.effective_state.as_deref(), Some("claimed"));
        assert_eq!(report.effective_states[&first.task_id], "claimed");
        assert_eq!(report.effective_states[&second.task_id], "open");
        assert_eq!(report.effective_states.len(), 2);
        assert_eq!(report.warnings, ["caller advisory"]);
        assert!(report.replay_warnings.is_empty());

        let done = append(
            &store,
            Some(&first.task_id),
            "agent:worker",
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: Some("completed".into()),
                check: None,
            },
            Vec::new(),
        )
        .unwrap();
        assert_eq!(done.effective_state.as_deref(), Some("done"));
        assert_eq!(report.effective_states[&first.task_id], "claimed");
        let model = store.replay().unwrap().model;
        assert_eq!(effective_display(&model.tasks[&first.task_id]), "done");
        assert_eq!(effective_display(&model.tasks[&second.task_id]), "open");

        use std::io::Write;
        let path = store.dir().join("events.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"malformed historical row\n")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let empty = append_events(&store, Vec::new(), Vec::new()).unwrap();
        assert!(empty.events.is_empty());
        assert!(empty.effective_state.is_none());
        assert!(empty.effective_states.is_empty());
        assert_eq!(empty.replay_warnings.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(append(
            &store,
            Some(&second.task_id),
            "agent:author",
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: None,
                check: None,
            },
            Vec::new()
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn default_endpoint_reads_cv_endpoint_trimmed_nonempty() {
        let env = crate::env::Env::from_vars::<&str, &str>([]).enter();
        assert_eq!(default_endpoint(), None, "unset → no identity");

        env.set("CV_ENDPOINT", "agent:demo");
        assert_eq!(default_endpoint().as_deref(), Some("agent:demo"));

        env.set("CV_ENDPOINT", "  agent:padded \n");
        assert_eq!(default_endpoint().as_deref(), Some("agent:padded"), "trimmed");

        env.set("CV_ENDPOINT", "   ");
        assert_eq!(default_endpoint(), None, "whitespace-only → no identity");

        env.set("CV_ENDPOINT", "");
        assert_eq!(default_endpoint(), None, "empty → no identity");
    }
}
