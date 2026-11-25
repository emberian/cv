//! Wire types for the task substrate: the durable event grammar and derived state enums.
//!
//! The JSON shapes here are a **wire contract**: events are appended to
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` and replayed forever, so tags and field names must
//! stay stable. Additions are fine; renames are not. The round-trip and pinned-JSON tests at the
//! bottom of this file are the tripwire.
//!
//! Kinds cv no longer writes are not deleted from the grammar: they read as
//! [`TaskEventKind::Retired`] — the raw object, kept losslessly so an old log replays and
//! re-serializes byte for byte — and the reducer applies them as no-ops.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// The event grammar: the base lifecycle, the decision facet, the pinned STATUS, and the
/// retired kinds an old log may still hold.
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
        /// Retired in 0.15.0 with the coordination board (the channel task events were mirrored
        /// to). Read through so a pre-0.15.0 line re-serializes to its own bytes; never written.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
    },
    Claimed {
        assignee: String,
    },
    Released {},
    /// Progress note; never changes state.
    Noted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<super::Claim>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
    },
    /// Completion, self-reported and labeled so, with an optional pointer to the evidence the
    /// agent offers. cv records the word; it never claims to have checked it.
    Done {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        evidence: Vec<super::EvidenceRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disclosure: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
        /// Retired in 0.15.0 (the `--check-*` completion checks). Read through as raw JSON so a
        /// log written before then re-serializes to its own bytes; never written, never read by
        /// the reducer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<Value>,
    },
    Abandoned {
        reason: String,
    },
    Superseded {
        by_task: String,
    },
    /// Add tags (deduplicated, case-sensitive labels such as `decision`, `deploy`, `lean`). Tags are
    /// additive: a task's tag set is the union of every `tagged` event on it.
    Tagged {
        tags: Vec<String>,
    },
    /// Record that this task cannot proceed until `task` reaches a terminal state. The relation
    /// is stored on the BLOCKED task; "X blocks Y" is written as a `blocked_by` on Y. Whether a
    /// task is blocked *now* is computed at read time from the blocker's current state, never
    /// stored — so a blocker finishing or being abandoned unblocks without another event.
    BlockedBy {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        require_success: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        final_only: bool,
        task: String,
    },
    /// Pose a decision on this task: from here on the task is of kind `decision` — something an
    /// assignee *answers* (with `resolved`) rather than does. `options` always contains `default`;
    /// `deadline` is when the default stands if nobody speaks; `source` is the event id of the
    /// note this decision was split out of (`cv task split`), when it was. One `posed` per task:
    /// amending the options is a new decision, never a silent rewrite under a resolver.
    Posed {
        options: Vec<String>,
        #[serde(rename = "default")]
        default_choice: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Answer a posed decision. `choice` must be one of the posed options (the default included).
    /// Terminal (`TaskState::Resolved`). Identity-bearing: WHO resolved is the fact that matters,
    /// so the resolver is recorded from an explicit identity, never a shared sink.
    ///
    /// `provisional`: the poser (an orchestrator) resolved it ON ITS DEFAULT on the decider's
    /// behalf, with a veto window — the task is resolved and work proceeds, and the decider's
    /// inbox lists it under "made for you (veto?)" until they answer. The ONE state-bearing event a
    /// terminal task accepts is a non-provisional `resolved` by the decision's assignee over a
    /// provisional resolution (a confirmation or a veto). Omitted from the wire when false.
    Resolved {
        choice: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        provisional: bool,
    },

    /// Pin the task's current STATUS: the relay hand-off field (a generation's tip, what is
    /// merged, what runs, what is next). REPLACES the pinned status in the projection — the
    /// history stays in the events. Allowed on a live task, by its assignee or its opener;
    /// refused on a terminal one (a status is state-bearing, unlike a note). Identity-bearing.
    StatusSet {
        text: String,
    },

    // ── retired (0.15.0): read, never written, applied as a no-op ───────────
    /// An event whose `event` tag is not one of the kinds above. The land facet's nine kinds
    /// ([`RETIRED_TAGS`]) land here from logs written before 0.15.0 and replay as state-neutral
    /// events; any other tag is refused by the reducer. The object is kept whole, so the line
    /// re-serializes to its own bytes.
    #[serde(untagged)]
    Retired(RetiredEvent),
}

/// The wire tags of every live [`TaskEventKind`], the image of [`TaskEventKind::tag`] over the
/// whole grammar. Fixture tests hold golden logs to this roster, so a new variant adds its tag
/// here (the round-trip test fails loud until it does).
pub const LIVE_TAGS: &[&str] = &[
    "opened",
    "claimed",
    "released",
    "noted",
    "done",
    "abandoned",
    "superseded",
    "tagged",
    "blocked_by",
    "posed",
    "resolved",
    "status_set",
];

/// The wire tags of the land facet retired in 0.15.0: reviewed code revisions, review
/// verdicts, and the git verifier's observations. Kept so a log that holds them still replays
/// and so a tool that names one can be told what happened to it.
pub const RETIRED_TAGS: &[&str] = &[
    "revision_proposed",
    "review_rerouted",
    "review_passed",
    "review_refuted",
    "source_unavailable",
    "merge_failed",
    "merged_local",
    "reconcile_failed",
    "landed",
];

/// The raw object of a retired event: its `event` tag plus whatever payload it carried.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RetiredEvent(pub serde_json::Map<String, Value>);

impl RetiredEvent {
    /// The `event` tag as written (empty when the line carried none).
    pub fn tag_str(&self) -> &str {
        self.0.get("event").and_then(Value::as_str).unwrap_or("")
    }

    /// Is this one of the kinds cv itself used to write ([`RETIRED_TAGS`])? Anything else is a
    /// kind this cv does not know, and the reducer refuses it.
    pub fn is_known(&self) -> bool {
        RETIRED_TAGS.contains(&self.tag_str())
    }
}

impl TaskEventKind {
    /// A kind cv no longer writes: agent-facing append paths refuse it at the store seam; replay
    /// applies it as a no-op.
    pub fn is_retired(&self) -> bool {
        matches!(self, TaskEventKind::Retired(_))
    }

    /// The wire tag for this kind (the serde `event` field), for logs and board notifications.
    /// A retired kind reports its own tag; an unknown one reports `unknown`.
    pub fn tag(&self) -> &'static str {
        match self {
            TaskEventKind::Opened { .. } => "opened",
            TaskEventKind::Claimed { .. } => "claimed",
            TaskEventKind::Released {} => "released",
            TaskEventKind::Noted { .. } => "noted",
            TaskEventKind::Done { .. } => "done",
            TaskEventKind::Abandoned { .. } => "abandoned",
            TaskEventKind::Superseded { .. } => "superseded",
            TaskEventKind::Tagged { .. } => "tagged",
            TaskEventKind::BlockedBy { .. } => "blocked_by",
            TaskEventKind::Posed { .. } => "posed",
            TaskEventKind::Resolved { .. } => "resolved",
            TaskEventKind::StatusSet { .. } => "status_set",
            TaskEventKind::Retired(r) => RETIRED_TAGS
                .iter()
                .copied()
                .find(|t| *t == r.tag_str())
                .unwrap_or("unknown"),
        }
    }
}

/// Lifecycle state of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Claimed,
    Done,
    Abandoned,
    Superseded,
    /// A posed decision that was answered (`resolved`). Terminal, like `Done` is for work.
    Resolved,
}

impl TaskState {
    /// Every state, in lifecycle order: the vocabulary `--state` accepts and `as_str` spells.
    pub const ALL: [TaskState; 6] = [
        TaskState::Open,
        TaskState::Claimed,
        TaskState::Done,
        TaskState::Abandoned,
        TaskState::Superseded,
        TaskState::Resolved,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Abandoned | TaskState::Superseded | TaskState::Resolved
        )
    }

    /// The state's one spelling, the same as its serde name.
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
            TaskState::Abandoned => "abandoned",
            TaskState::Superseded => "superseded",
            TaskState::Resolved => "resolved",
        }
    }
}

impl std::str::FromStr for TaskState {
    type Err = String;

    /// One of [`TaskState::ALL`] by its spelling; an unknown word names the whole vocabulary.
    fn from_str(s: &str) -> Result<Self, String> {
        TaskState::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown state {s:?} (expected one of {})",
                    TaskState::ALL.map(TaskState::as_str).join("|")
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_is_spelled_once() {
        for state in TaskState::ALL {
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
            assert_eq!(state.as_str().parse::<TaskState>(), Ok(state));
        }
        assert!("redy".parse::<TaskState>().unwrap_err().contains("open|claimed|done"));
    }

    fn event(kind: TaskEventKind) -> TaskEvent {
        TaskEvent {
            id: "0198c0de-0000-7000-8000-000000000001".to_string(),
            task_id: "0198c0de-0000-7000-8000-000000000000".to_string(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: "agent:test".to_string(),
            kind,
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
                channel: None,
                assignee: None,
            },
            TaskEventKind::Claimed { assignee: "a".into() },
            TaskEventKind::Released {},
            TaskEventKind::Noted {
                claim: None,
                text: "n".into(),
                session_ref: Some("s".into()),
            },
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: None,
                check: None,
            },
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: Some("exit 0".into()),
                check: None,
            },
            TaskEventKind::Abandoned { reason: "r".into() },
            TaskEventKind::Superseded { by_task: "t2".into() },
            TaskEventKind::Tagged {
                tags: vec!["decision".into()],
            },
            TaskEventKind::BlockedBy {
                require_success: false,
                final_only: false,
                task: "t3".into(),
            },
            TaskEventKind::Posed {
                options: vec!["keep".into(), "delete".into()],
                default_choice: "keep".into(),
                deadline: Some("2026-10-03T00:00:00Z".parse().unwrap()),
                source: Some("0198c0de-0000-7000-8000-0000000000aa".into()),
            },
            TaskEventKind::Posed {
                options: vec!["keep".into()],
                default_choice: "keep".into(),
                deadline: None,
                source: None,
            },
            TaskEventKind::Resolved {
                choice: "delete".into(),
                note: Some("why".into()),
                provisional: false,
            },
            TaskEventKind::Resolved {
                choice: "keep".into(),
                note: None,
                provisional: true,
            },
            TaskEventKind::StatusSet {
                text: "tip abc123; next: merge B".into(),
            },
        ];
        let mut tags = std::collections::BTreeSet::new();
        for kind in kinds {
            let ev = event(kind);
            let json = serde_json::to_string(&ev).unwrap();
            let back: TaskEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(ev, back, "round-trip mismatch for {json}");
            assert!(!back.kind.is_retired(), "a live kind must not read as retired: {json}");
            tags.insert(back.kind.tag());
        }
        assert_eq!(
            tags,
            LIVE_TAGS.iter().copied().collect::<std::collections::BTreeSet<_>>(),
            "every live kind's tag is in LIVE_TAGS, and LIVE_TAGS names no kind this list leaves out"
        );
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

        // The decision facet: `default` is the wire name (the Rust field is `default_choice`).
        let ev = event(TaskEventKind::Posed {
            options: vec!["a".into(), "b".into()],
            default_choice: "a".into(),
            deadline: None,
            source: None,
        });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "posed");
        assert_eq!(v["default"], "a");
        assert_eq!(v["options"], serde_json::json!(["a", "b"]));
        assert!(v.get("deadline").is_none() && v.get("source").is_none(), "{v}");

        // Tag helper stays in sync with serde, for every kind (the hand map must spell what
        // `rename_all` writes).
        for (kind, tag) in [
            (
                TaskEventKind::Opened {
                    title: "t".into(),
                    body: String::new(),
                    repo: None,
                    issue: None,
                    channel: None,
                    assignee: None,
                },
                "opened",
            ),
            (TaskEventKind::Claimed { assignee: "a".into() }, "claimed"),
            (TaskEventKind::Released {}, "released"),
            (
                TaskEventKind::Noted {
                    claim: None,
                    text: "n".into(),
                    session_ref: None,
                },
                "noted",
            ),
            (
                TaskEventKind::Done {
                    evidence: Vec::new(),
                    disclosure: None,
                    observed: None,
                    check: None,
                },
                "done",
            ),
            (TaskEventKind::Abandoned { reason: "r".into() }, "abandoned"),
            (TaskEventKind::Superseded { by_task: "b".into() }, "superseded"),
            (TaskEventKind::Tagged { tags: vec![] }, "tagged"),
            (
                TaskEventKind::BlockedBy {
                    require_success: false,
                    final_only: false,
                    task: "b".into(),
                },
                "blocked_by",
            ),
            (
                TaskEventKind::Posed {
                    options: vec!["a".into()],
                    default_choice: "a".into(),
                    deadline: None,
                    source: None,
                },
                "posed",
            ),
            (
                TaskEventKind::Resolved {
                    choice: "a".into(),
                    note: None,
                    provisional: false,
                },
                "resolved",
            ),
            (TaskEventKind::StatusSet { text: "s".into() }, "status_set"),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&event(kind.clone())).unwrap()).unwrap();
            assert_eq!(v["event"], tag);
            assert_eq!(kind.tag(), tag);
        }
    }

    /// A retired land-facet line reads as `Retired` with its own tag, keeps every byte it
    /// carried, and re-serializes to itself; a tag this cv never wrote reads as `Retired` too but
    /// reports `unknown`, so the reducer can refuse it.
    #[test]
    fn retired_kinds_read_losslessly_and_unknown_tags_are_not_mistaken_for_them() {
        let landed = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"cv-verify","event":"landed","upstream_head":"eeee","observed_patch_id":"cccc"}"#;
        let ev: TaskEvent = serde_json::from_str(landed).unwrap();
        assert!(ev.kind.is_retired());
        assert_eq!(ev.kind.tag(), "landed");
        let TaskEventKind::Retired(ref r) = ev.kind else {
            panic!("{ev:?}")
        };
        assert!(r.is_known());
        assert_eq!(r.0["upstream_head"], "eeee");
        assert_eq!(serde_json::to_string(&ev).unwrap(), landed, "byte-stable replay");

        let alien = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"x","event":"teleported","where":"away"}"#;
        let ev: TaskEvent = serde_json::from_str(alien).unwrap();
        let TaskEventKind::Retired(ref r) = ev.kind else {
            panic!("{ev:?}")
        };
        assert!(!r.is_known());
        assert_eq!(ev.kind.tag(), "unknown");
    }

    /// The retired `check` field on `done`: a log written with it re-serializes to its own bytes,
    /// a log written without it stays without it, and new events never carry it.
    #[test]
    fn done_check_field_is_read_through_only() {
        let old = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"agent:test","event":"done","observed":"trust me"}"#;
        let ev: TaskEvent = serde_json::from_str(old).unwrap();
        assert!(matches!(ev.kind, TaskEventKind::Done { check: None, .. }), "{ev:?}");
        assert_eq!(serde_json::to_string(&ev).unwrap(), old);

        let checked = r#"{"id":"0198c0de-0000-7000-8000-000000000001","task_id":"0198c0de-0000-7000-8000-000000000000","ts":"2026-07-16T12:00:00Z","by":"agent:test","event":"done","observed":"exit 0","check":{"check":"cmd","cmd":"cargo test","result":"exit 0"}}"#;
        let ev: TaskEvent = serde_json::from_str(checked).unwrap();
        assert!(!ev.kind.is_retired(), "a done with a retired field is still a done");
        assert_eq!(serde_json::to_string(&ev).unwrap(), checked, "byte-stable replay");
    }
}
