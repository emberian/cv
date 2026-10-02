//! Wire-format golden test: `tests/fixtures/golden-events-v1.jsonl` is a committed task event
//! log containing the v1 header record and at least one specimen of **every** `TaskEventKind`
//! variant. This test replays it through the reducer and asserts the reduced model matches the
//! committed snapshot `tests/fixtures/golden-model-v1.json`, byte for byte.
//!
//! If this test fails: the wire format or the reducer semantics changed. If that was intentional,
//! regenerate the fixtures (`GOLDEN_REGEN=1 cargo test -p clustervision-core --test wire_golden`),
//! read the diff like the wire-contract review it is, and say so in the commit. This tripwire
//! becomes a hard freeze ("existing logs must replay identically") at the first published
//! release; until then it exists to make format changes deliberate, not accidental.

use std::fs;
use std::path::PathBuf;

use cv_core::task::{TaskEvent, TaskReducer};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

const HEADER_LINE: &str = r#"{"format":"cv-task-log","v":1}"#;

/// Every wire tag the fixture must contain a specimen of. When you add a `TaskEventKind`
/// variant, add its tag here and a specimen event to the fixture — this list is the tripwire
/// that keeps the golden log exhaustive.
const ALL_EVENT_TAGS: &[&str] = &[
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

#[test]
fn golden_log_replays_identically_forever() {
    let log_path = fixtures_dir().join("golden-events-v1.jsonl");
    let snap_path = fixtures_dir().join("golden-model-v1.json");

    if std::env::var_os("GOLDEN_REGEN").is_some() {
        regenerate(&log_path, &snap_path);
    }

    let raw = fs::read_to_string(&log_path).unwrap_or_else(|e| panic!("reading {}: {e}", log_path.display()));
    let mut lines = raw.lines();
    assert_eq!(
        lines.next(),
        Some(HEADER_LINE),
        "golden log must start with the v1 header record"
    );

    let events: Vec<TaskEvent> = lines
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!(
                    "golden log line {} no longer parses as a TaskEvent ({e}).\n\
                     The wire format changed — if intentional, regenerate with GOLDEN_REGEN=1 \
                     and note it in the commit.",
                    i + 2
                )
            })
        })
        .collect();

    // Exhaustiveness: the fixture must exercise the whole event grammar, or the byte-stability
    // guarantee silently shrinks to the variants it happens to contain.
    let seen: std::collections::BTreeSet<&str> = events.iter().map(|e| e.kind.tag()).collect();
    for tag in ALL_EVENT_TAGS {
        assert!(
            seen.contains(tag),
            "golden log has no {tag:?} specimen — every TaskEventKind variant must appear"
        );
    }
    for tag in &seen {
        assert!(
            ALL_EVENT_TAGS.contains(tag),
            "golden log contains {tag:?}, which ALL_EVENT_TAGS does not list — update the roster"
        );
    }

    let model = TaskReducer::reduce(&events).unwrap_or_else(|e| {
        panic!(
            "the reducer refused the golden log ({e}).\n\
             Reducer semantics changed — if intentional, regenerate with GOLDEN_REGEN=1 and \
             note it in the commit."
        )
    });
    let mut rendered = serde_json::to_string_pretty(&model).expect("serializing reduced model");
    rendered.push('\n');

    let expected = fs::read_to_string(&snap_path).unwrap_or_else(|e| panic!("reading {}: {e}", snap_path.display()));
    assert_eq!(
        rendered, expected,
        "reduced model diverged from the committed snapshot.\n\
         The wire format or reducer semantics changed — if intentional, regenerate with \
         GOLDEN_REGEN=1, review the snapshot diff as the wire-contract change it is, and note \
         it in the commit. This becomes a hard freeze at the first published release."
    );

    // And the events themselves round-trip byte-stably under the same serde settings the store
    // writes with (serde_json, preserve_order): re-serializing each fixture line reproduces it.
    for (i, line) in raw.lines().skip(1).enumerate() {
        let ev: TaskEvent = serde_json::from_str(line).unwrap();
        let re = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            re,
            line,
            "golden log line {} does not re-serialize to itself — field order, names, or \
             skip-serializing rules changed (a wire-format change; if intentional, regenerate \
             with GOLDEN_REGEN=1 and note it in the commit).",
            i + 2
        );
    }
}

/// Rewrites both fixture files from the canonical specimen log below. The specimen construction
/// is code, so regeneration always uses the current wire types — the point of the committed
/// files is that the *repo history* then pins the bytes.
fn regenerate(log_path: &std::path::Path, snap_path: &std::path::Path) {
    use chrono::{DateTime, Utc};
    use cv_core::task::{
        DoneCheck, DoneCheckKind, IndependenceCheck, MergeFailure, ReviewReceipts, Revision, TaskEventKind,
    };

    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }
    let ts = |minute: u32| -> DateTime<Utc> { format!("2026-07-16T12:{minute:02}:00Z").parse().unwrap() };
    let id = |n: u64| format!("00000000-0000-7000-8000-{n:012}");

    let mut events: Vec<TaskEvent> = Vec::new();
    let mut n = 0u64;
    let mut push = |events: &mut Vec<TaskEvent>, task_id: &str, by: &str, minute: u32, kind: TaskEventKind| -> String {
        n += 1;
        let event_id = id(n);
        let task_id = if task_id.is_empty() {
            event_id.clone()
        } else {
            task_id.to_string()
        };
        events.push(TaskEvent {
            id: event_id.clone(),
            task_id,
            ts: ts(minute),
            by: by.to_string(),
            kind,
        });
        event_id
    };

    // Task A: the full happy path — claim, note, propose, reroute, pass, then every verifier
    // observation on the way to landed, then done.
    let a = push(
        &mut events,
        "",
        "agent:owner",
        0,
        TaskEventKind::Opened {
            title: "land the range patch-id gate".into(),
            body: "port the gate onto multi-commit review debts".into(),
            repo: Some("/repo/alpha".into()),
            issue: Some("#42".into()),
            channel: "tasks".into(),
            assignee: Some("agent:author".into()),
        },
    );
    push(
        &mut events,
        &a,
        "agent:author",
        1,
        TaskEventKind::Claimed {
            assignee: "agent:author".into(),
        },
    );
    push(
        &mut events,
        &a,
        "agent:author",
        2,
        TaskEventKind::Noted {
            text: "gate wired; tests running".into(),
            session_ref: Some("sess-alpha-001".into()),
        },
    );
    push(
        &mut events,
        &a,
        "agent:author",
        3,
        TaskEventKind::RevisionProposed {
            revision: Revision {
                n: 1,
                branch: "issue/42-gate".into(),
                worktree: Some("/wt/alpha".into()),
                upstream: "origin/main".into(),
                base: hex('a'),
                review_sha: hex('b'),
                patch_id: hex('c'),
                reviewer: Some("agent:reviewer".into()),
                session_ref: Some("sess-alpha-001".into()),
            },
        },
    );
    push(
        &mut events,
        &a,
        "agent:reviewer",
        4,
        TaskEventKind::ReviewRerouted {
            from: "agent:reviewer".into(),
            to: "agent:other".into(),
        },
    );
    push(
        &mut events,
        &a,
        "agent:other",
        5,
        TaskEventKind::ReviewPassed {
            reviewer: "agent:other".into(),
            session_ref: Some("sess-review-002".into()),
            independence: Some(IndependenceCheck {
                author_family: Some("anthropic".into()),
                reviewer_family: Some("openai".into()),
                independent: Some(true),
            }),
            receipts: Some(ReviewReceipts {
                saw_change: Some(true),
                ran_checks: Some(true),
                turns: Some(14),
            }),
        },
    );
    push(
        &mut events,
        &a,
        "cv-verify",
        6,
        TaskEventKind::SourceUnavailable {
            detail: "worktree /wt/alpha vanished".into(),
        },
    );
    push(
        &mut events,
        &a,
        "cv-verify",
        7,
        TaskEventKind::MergeFailed {
            reason: MergeFailure::NonFastForward {},
        },
    );
    push(
        &mut events,
        &a,
        "cv-verify",
        8,
        TaskEventKind::MergedLocal {
            from_sha: hex('d'),
            to_sha: hex('b'),
        },
    );
    push(
        &mut events,
        &a,
        "cv-verify",
        9,
        TaskEventKind::ReconcileFailed {
            detail: "post-merge fetch of origin failed".into(),
        },
    );
    push(
        &mut events,
        &a,
        "cv-verify",
        10,
        TaskEventKind::Landed {
            upstream_head: hex('e'),
            observed_patch_id: hex('c'),
        },
    );
    push(
        &mut events,
        &a,
        "agent:author",
        11,
        TaskEventKind::Done {
            observed: Some("observed on origin/main by cv-verify".into()),
            // A checked completion: cv ran the predicate and recorded the observed pass (pins the
            // DoneCheck wire shape into the golden log).
            check: Some(DoneCheck {
                kind: DoneCheckKind::Cmd {
                    cmd: "cargo test -p demo".into(),
                },
                result: "exit 0".into(),
            }),
        },
    );

    // Task B: claim/release churn, a refuted revision, abandonment.
    let b = push(
        &mut events,
        "",
        "agent:owner",
        12,
        TaskEventKind::Opened {
            title: "refuted lane".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            assignee: None,
        },
    );
    push(
        &mut events,
        &b,
        "agent:author",
        13,
        TaskEventKind::Claimed {
            assignee: "agent:author".into(),
        },
    );
    push(&mut events, &b, "agent:author", 14, TaskEventKind::Released {});
    push(
        &mut events,
        &b,
        "agent:other",
        15,
        TaskEventKind::Claimed {
            assignee: "agent:other".into(),
        },
    );
    push(
        &mut events,
        &b,
        "agent:other",
        16,
        TaskEventKind::RevisionProposed {
            revision: Revision {
                n: 1,
                branch: "issue/7-fix".into(),
                worktree: None,
                upstream: "origin/main".into(),
                base: hex('1'),
                review_sha: hex('2'),
                patch_id: hex('3'),
                reviewer: Some("agent:reviewer".into()),
                session_ref: None,
            },
        },
    );
    push(
        &mut events,
        &b,
        "agent:reviewer",
        17,
        TaskEventKind::ReviewRefuted {
            reviewer: "agent:reviewer".into(),
            session_ref: Some("sess-review-003".into()),
            // A no-contact refute: pins that undetermined sub-observations stay off the wire.
            receipts: Some(ReviewReceipts {
                saw_change: Some(false),
                ran_checks: None,
                turns: Some(2),
            }),
        },
    );
    push(
        &mut events,
        &b,
        "agent:other",
        18,
        TaskEventKind::Abandoned {
            reason: "refuted; superseding approach chosen".into(),
        },
    );

    // Task C: opened, then superseded by task A.
    let c = push(
        &mut events,
        "",
        "agent:owner",
        19,
        TaskEventKind::Opened {
            title: "duplicate of the gate task".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            assignee: None,
        },
    );
    push(
        &mut events,
        &c,
        "agent:owner",
        20,
        TaskEventKind::Tagged {
            tags: vec!["decision".into(), "gate".into()],
        },
    );
    push(
        &mut events,
        &c,
        "agent:owner",
        21,
        TaskEventKind::BlockedBy { task: a.clone() },
    );
    push(
        &mut events,
        &c,
        "agent:owner",
        22,
        TaskEventKind::Superseded { by_task: a.clone() },
    );

    // Task D: a decision — opened for a human, posed with a default, a deadline and a source
    // note, then resolved with a non-default choice (pins the decision facet's wire shape and
    // the `resolved` terminal state).
    let d = push(
        &mut events,
        "",
        "orchestrator:demo",
        23,
        TaskEventKind::Opened {
            title: "K-PORTAL: who births a traveller's GUEST cell".into(),
            body: "default = the concierge births it; alternative = the receiver allocates it".into(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            assignee: Some("ember".into()),
        },
    );
    push(
        &mut events,
        &d,
        "orchestrator:demo",
        24,
        TaskEventKind::Tagged {
            tags: vec!["decision".into()],
        },
    );
    push(
        &mut events,
        &d,
        "orchestrator:demo",
        25,
        TaskEventKind::Posed {
            options: vec!["the concierge births it".into(), "the receiver allocates it".into()],
            default_choice: "the concierge births it".into(),
            deadline: Some("2026-07-20T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            source: Some("00000000-0000-7000-8000-000000000004".into()),
        },
    );
    push(
        &mut events,
        &d,
        "ember",
        26,
        TaskEventKind::Resolved {
            choice: "the receiver allocates it".into(),
            note: Some("F can afford the arrivals".into()),
            provisional: false,
        },
    );

    let mut log = String::from(HEADER_LINE);
    log.push('\n');
    for ev in &events {
        log.push_str(&serde_json::to_string(ev).unwrap());
        log.push('\n');
    }
    let model = TaskReducer::reduce(&events).expect("specimen log must reduce");
    let mut snap = serde_json::to_string_pretty(&model).unwrap();
    snap.push('\n');

    fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    fs::write(log_path, log).unwrap();
    fs::write(snap_path, snap).unwrap();
}
