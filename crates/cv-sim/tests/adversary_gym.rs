//! The adversary gym: an attack suite fired at the task substrate's sensors, in two honest
//! categories.
//!
//! **Category 1 — DETECTED attacks.** Each stages an attack the substrate is supposed to catch
//! and asserts the defense fires (a warning, a quarantine, a suspect row, a rejected append).
//! Comment out the defense and the test goes red — that is the contract that keeps these from
//! rotting into vacuous green.
//!
//! **Category 2 — KNOWN-HOLE PIN TESTS** (`pin_*`). Each stages an attack that currently
//! SUCCEEDS and asserts the success. They document our admitted weaknesses so that *fixing* one
//! is a measurable event: the fix flips the assertion, and the test is renamed into a Category-1
//! defense. Each carries the hole's name, the red-team / self-audit source that found it, and
//! the intended future fix direction.
//!
//! Fixtures: cv-sim's [`FleetScenario`] generator supplies real-shaped clean fleets for
//! baselines; scratch git repos follow the `verify.rs` hermeticity convention
//! (`GIT_CONFIG_GLOBAL=/dev/null`, per-repo user config, no commit signing).

use std::path::{Path, PathBuf};
use std::process::Command;

use cv_core::task::verify::{
    heartbeat_warning, observe_revision, read_heartbeat, run_verify, VerifyHeartbeat, VerifyOptions,
};
use cv_core::task::{
    new_event, review_receipts, DebtReport, FleetStats, InboxReason, InboxRow, MergeFailure, Revision, RevisionState,
    TaskEvent, TaskEventKind, TaskProjection, TaskReducer, TaskState, TaskStore,
};
use cv_sim::{FleetScenario, Pathology};

// ── scratch git repos (verify.rs hermeticity convention) ─────────────────────

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cv-gym-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run git in `repo` with the machine's global/system config blanked out (no commit signing, no
/// user config leakage) — the whole hermeticity trick from the verify.rs fixtures.
fn git_run(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn tmp_repo() -> PathBuf {
    let dir = tmp_dir("git");
    git_run(&dir, &["init", "-q", "-b", "main"]);
    git_run(&dir, &["config", "user.email", "t@example.com"]);
    git_run(&dir, &["config", "user.name", "t"]);
    commit(&dir, "base.txt", "base", "base commit");
    dir
}

fn commit(repo: &Path, file: &str, content: &str, msg: &str) {
    std::fs::write(repo.join(file), content).unwrap();
    git_run(repo, &["add", "."]);
    git_run(repo, &["commit", "-q", "-m", msg]);
}

fn head(repo: &Path) -> String {
    git_out(repo, &["rev-parse", "HEAD"])
}

// ── store plumbing ───────────────────────────────────────────────────────────

fn quiet() -> VerifyOptions {
    VerifyOptions {
        quiet: true,
        ..Default::default()
    }
}

/// Append an `Opened` event and return the new task id.
fn open_in(store: &TaskStore, repo: Option<&Path>, by: &str, assignee: Option<&str>, title: &str) -> String {
    store
        .append_agent_event(new_event(
            None,
            by,
            TaskEventKind::Opened {
                title: title.into(),
                body: String::new(),
                repo: repo.map(Path::to_path_buf),
                issue: None,
                channel: "tasks".into(),
                assignee: assignee.map(String::from),
            },
        ))
        .unwrap()
        .task_id
}

/// Append a `RevisionProposed`.
fn propose(store: &TaskStore, task: &str, by: &str, revision: Revision) {
    store
        .append_agent_event(new_event(Some(task), by, TaskEventKind::RevisionProposed { revision }))
        .unwrap();
}

/// Append a `ReviewPassed` with the given receipts.
fn pass(store: &TaskStore, task: &str, reviewer: &str, receipts: Option<cv_core::task::ReviewReceipts>) {
    store
        .append_agent_event(new_event(
            Some(task),
            reviewer,
            TaskEventKind::ReviewPassed {
                reviewer: reviewer.into(),
                session_ref: None,
                independence: None,
                receipts,
            },
        ))
        .unwrap();
}

/// A hand-built revision with opaque (non-git) identity fields — enough for the reducer, the
/// stats projection, and the inbox view, none of which resolve the shas against a repo. The
/// reviewer is who the reducer will require the pass verdict to come from (the active-reviewer
/// gate).
fn fake_revision(n: u32, reviewer: &str) -> Revision {
    Revision {
        n,
        branch: format!("task/x{n}"),
        worktree: None,
        upstream: "origin/main".into(),
        base: "0".repeat(40),
        review_sha: "a".repeat(40),
        patch_id: "b".repeat(40),
        reviewer: Some(reviewer.into()),
        session_ref: None,
    }
}

/// Hand-append a raw JSONL line straight into the log — what any seat with Bash can do.
fn raw_append(store: &TaskStore, ev: &TaskEvent) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(store.dir().join("events.jsonl"))
        .unwrap();
    writeln!(f, "{}", serde_json::to_string(ev).unwrap()).unwrap();
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 1 — DETECTED attacks (the defense must fire)
// ══════════════════════════════════════════════════════════════════════════════

/// A self-consistent forged `Landed` (patch_id copied from the revision) hand-appended by a
/// non-verifier identity, against a branch that never merged. The provenance scan names the
/// forger on replay; one verify pass re-observes the branch, finds the content is NOT on
/// upstream, and records a suspect land that surfaces on the debt view.
#[test]
fn forged_landed_is_contradicted_within_one_verify_pass() {
    let repo = tmp_repo();
    let store = TaskStore::at(tmp_dir("store"));
    git_run(&repo, &["checkout", "-q", "-b", "task/forge"]);
    commit(&repo, "f.txt", "f", "feat f");
    let rev = observe_revision(&repo, "task/forge", "main", None, 1, None, None, None).unwrap();
    git_run(&repo, &["checkout", "-q", "main"]); // NOT merged — the land is a lie

    let task = open_in(&store, Some(&repo), "agent:author", None, "forged land");
    let patch_id = rev.patch_id.clone();
    propose(&store, &task, "agent:author", rev);
    pass(&store, &task, "agent:reviewer", None);

    // The forgery: a Landed line copying the revision's own patch_id, by a sneaky identity.
    let forged = new_event(
        Some(&task),
        "agent:sneaky",
        TaskEventKind::Landed {
            upstream_head: head(&repo),
            observed_patch_id: patch_id,
        },
    );
    raw_append(&store, &forged);

    // Replay folds it (self-consistent) but the provenance scan is loud about attestation.
    let outcome = store.replay().unwrap();
    assert_eq!(
        outcome.quarantined, 0,
        "self-consistent, so not quarantined — the danger"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains("attested, not observed") && w.contains("agent:sneaky")),
        "provenance warning must name the forger: {:?}",
        outcome.warnings
    );
    assert_eq!(
        outcome.model.tasks[&task].current_revision().unwrap().state,
        RevisionState::Landed,
        "the forged fold does move the revision to Landed — which is why re-observation matters"
    );

    // One verify pass re-observes Landed: content is NOT on upstream → suspect warning + row.
    let (appended, warnings) = run_verify(&store, None, &quiet()).unwrap();
    assert!(appended.is_empty(), "nothing legitimate to append: {appended:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("possible forged or rolled-back land")),
        "suspect warning must fire on the verify pass: {warnings:?}"
    );

    // Persisted to the heartbeat and surfaced as a debt suspect row.
    let hb = read_heartbeat(store.dir()).unwrap();
    assert_eq!(hb.suspect_landed.len(), 1);
    assert_eq!(hb.suspect_landed[0].task_id, task);
    let report = DebtReport::compute(&outcome.model, Some(&hb), None);
    assert_eq!(report.suspects.len(), 1, "the debt view shows the suspect land");
    assert_eq!(report.suspects[0].task_id, task);
}

/// A rubber-stamp pass — receipts show `saw_change: Some(false)` (the reviewer signed off with no
/// observable contact with the change). The stats reviewer row must count it in
/// `no_contact_passes`, and a real-shaped clean fleet must count zero (the counter is not
/// trivially always-positive).
#[test]
fn rubber_stamp_pass_is_counted() {
    // Baseline: a real-shaped fleet with rubber stamps dialed OFF reports zero no-contact passes.
    let clean = FleetScenario {
        endpoints: 4,
        reviewers: 2,
        tasks: 40,
        seed: 7,
        pathology: Pathology {
            rubber_stamp_rate: 0.0,
            ..Pathology::default()
        },
    };
    let clean_stats = clean.stats().unwrap();
    assert_eq!(
        clean_stats.reviewers.iter().map(|r| r.no_contact_passes).sum::<usize>(),
        0,
        "a fleet with no rubber stamps has zero no-contact passes"
    );

    // The attack: one pass whose receipts record no observable contact.
    let store = TaskStore::at(tmp_dir("store"));
    let task = open_in(&store, None, "agent:author", Some("agent:author"), "rubber stamp");
    propose(&store, &task, "agent:author", fake_revision(1, "agent:rubber"));
    pass(
        &store,
        &task,
        "agent:rubber",
        Some(cv_core::task::ReviewReceipts {
            saw_change: Some(false),
            ran_checks: Some(false),
            turns: Some(1),
        }),
    );

    let model = store.replay().unwrap().model;
    let stats = FleetStats::compute(&model, None, None);
    let row = stats
        .reviewers
        .iter()
        .find(|r| r.reviewer == "agent:rubber")
        .expect("the rubber-stamp reviewer has a row");
    assert_eq!(row.passes, 1);
    assert_eq!(
        row.no_contact_passes, 1,
        "saw_change=Some(false) must increment no_contact_passes"
    );
}

/// The verifier's liveness marker is loud in all three states: absent (NEVER verified), stale
/// (older than 2x its recorded interval), and — as a no-false-alarm control — fresh (silent).
/// The debt view surfaces the NEVER state directly.
#[test]
fn stale_verifier_is_visible() {
    use chrono::{Duration, Utc};

    // Absent heartbeat: landing state has NEVER been verified — the loudest case.
    let never = heartbeat_warning(None).expect("absence must warn");
    assert!(never.contains("NEVER"), "absent heartbeat warning: {never}");

    // The debt view carries the same NEVER wording when no heartbeat exists.
    let empty = TaskReducer::reduce(&[]).unwrap();
    let report = DebtReport::compute(&empty, None, None);
    assert!(
        report.verify_warning.as_deref().unwrap_or("").contains("NEVER"),
        "debt view must say NEVER when never verified: {:?}",
        report.verify_warning
    );

    // A backdated heartbeat WITH a recorded interval, older than 2x that interval: STALE.
    let stale = VerifyHeartbeat {
        ts: Utc::now() - Duration::seconds(1000),
        tasks_checked: 3,
        events_appended: 0,
        interval_secs: Some(300),
        suspect_landed: Vec::new(),
    };
    let w = heartbeat_warning(Some(&stale)).expect("a stale heartbeat must warn");
    assert!(
        w.contains("STALE") && w.contains("periodic verifier may be dead"),
        "stale heartbeat warning: {w}"
    );

    // A fresh heartbeat within its interval is silent — no false alarm.
    let fresh = VerifyHeartbeat {
        ts: Utc::now(),
        ..stale.clone()
    };
    assert_eq!(heartbeat_warning(Some(&fresh)), None, "a fresh heartbeat must not warn");
}

/// Log tampering has three shapes with three responses: a reducer-refused interior line is
/// quarantined (counted, warned, the rest of the log survives); a torn trailing line is tolerated
/// silently (the one shape a crashed locked writer leaves); a future-version header refuses replay
/// entirely (written by a newer cv).
#[test]
fn log_tampering_is_loud() {
    use std::io::Write as _;

    // (a) Interior reducer-refused line between valid events → quarantine + warning, log survives.
    let store = TaskStore::at(tmp_dir("store"));
    let task = open_in(&store, None, "agent:a", None, "t");
    store
        .append_agent_event(new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "hi".into(),
                session_ref: None,
            },
        ))
        .unwrap();
    // A parseable event the reducer refuses (claim on an unknown task) + a valid trailer.
    let refused = new_event(
        Some("00000000-0000-7000-8000-00000000dead"),
        "agent:evil",
        TaskEventKind::Claimed {
            assignee: "agent:evil".into(),
        },
    );
    let good = new_event(
        Some(&task),
        "agent:a",
        TaskEventKind::Noted {
            text: "still here".into(),
            session_ref: None,
        },
    );
    raw_append(&store, &refused);
    raw_append(&store, &good);
    let outcome = store.replay().unwrap();
    assert_eq!(
        outcome.quarantined, 1,
        "the reducer-refused interior line is quarantined"
    );
    assert!(
        outcome.warnings.iter().any(|w| w.contains("quarantined")),
        "quarantine must warn: {:?}",
        outcome.warnings
    );
    assert_eq!(
        outcome.model.tasks[&task].notes.len(),
        2,
        "the trailing valid event still folds — one poison line does not brick the log"
    );

    // (b) A torn trailing line (no newline) is tolerated silently.
    let store2 = TaskStore::at(tmp_dir("store"));
    open_in(&store2, None, "agent:a", None, "t");
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(store2.dir().join("events.jsonl"))
            .unwrap();
        f.write_all(b"{\"id\":\"trunc").unwrap();
    }
    let outcome = store2.replay().unwrap();
    assert!(
        outcome.warnings.is_empty(),
        "a torn trailing line is tolerated: {:?}",
        outcome.warnings
    );
    assert_eq!(outcome.quarantined, 0);
    assert_eq!(outcome.events.len(), 1);

    // (c) A future log-format header (v2) → replay refuses loudly.
    let store3 = TaskStore::at(tmp_dir("store"));
    std::fs::create_dir_all(store3.dir()).unwrap();
    {
        let mut f = std::fs::File::create(store3.dir().join("events.jsonl")).unwrap();
        writeln!(f, "{{\"format\":\"cv-task-log\",\"v\":2}}").unwrap();
    }
    let err = store3.replay().unwrap_err();
    assert!(
        err.to_string().contains("newer cv"),
        "a v2 header must refuse replay: {err}"
    );
}

/// The locked compare-and-swap claim at scale: 32 threads race to claim each of 8 tasks through
/// the real store. Exactly one claim wins per task; every loser gets a clean, named rejection —
/// never a panic, never a corrupt append.
#[test]
fn claim_race_has_one_winner_at_scale() {
    let store = TaskStore::at(tmp_dir("store"));
    let tasks: Vec<String> = (0..8)
        .map(|i| open_in(&store, None, "human:sim", None, &format!("task {i}")))
        .collect();

    let winners: Vec<Vec<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..32)
            .map(|w| {
                let store = store.clone();
                let tasks = tasks.clone();
                s.spawn(move || {
                    let who = format!("agent:w{w:02}");
                    tasks
                        .iter()
                        .filter(|t| {
                            store
                                .append_agent_event(new_event(
                                    Some(t),
                                    &who,
                                    TaskEventKind::Claimed { assignee: who.clone() },
                                ))
                                .is_ok()
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for t in &tasks {
        let n = winners.iter().flatten().filter(|w| *w == t).count();
        assert_eq!(n, 1, "task {t} must have exactly one winning claim, got {n}");
    }
    assert_eq!(
        winners.iter().map(Vec::len).sum::<usize>(),
        8,
        "8 tasks → 8 winning claims total"
    );

    // The log agrees: every task is Claimed exactly once.
    let model = store.replay().unwrap().model;
    for t in &tasks {
        assert_eq!(model.tasks[t].state, TaskState::Claimed);
    }

    // A late claim on an already-won task is a clean, named rejection — not a panic, not a corrupt
    // append.
    let err = store
        .append_agent_event(new_event(
            Some(&tasks[0]),
            "agent:late",
            TaskEventKind::Claimed {
                assignee: "agent:late".into(),
            },
        ))
        .unwrap_err();
    assert!(
        err.to_string().contains("event rejected"),
        "a loser sees a clean rejection: {err}"
    );
}

/// The store seam holds law 1: every verifier-only kind pushed through the agent-facing append
/// path is rejected, and nothing lands in the log.
#[test]
fn verifier_only_kinds_rejected_from_agent_paths() {
    let store = TaskStore::at(tmp_dir("store"));
    let task = open_in(&store, None, "agent:a", None, "t");

    let verifier_only = [
        TaskEventKind::SourceUnavailable { detail: "gone".into() },
        TaskEventKind::MergeFailed {
            reason: MergeFailure::NonFastForward {},
        },
        TaskEventKind::MergedLocal {
            from_sha: "a".repeat(40),
            to_sha: "b".repeat(40),
        },
        TaskEventKind::ReconcileFailed { detail: "boom".into() },
        TaskEventKind::Landed {
            upstream_head: "c".repeat(40),
            observed_patch_id: "d".repeat(40),
        },
    ];
    for kind in verifier_only {
        assert!(
            kind.is_verifier_only(),
            "{} must be classified verifier-only",
            kind.tag()
        );
        let tag = kind.tag();
        let err = store
            .append_agent_event(new_event(Some(&task), "agent:sneaky", kind))
            .unwrap_err();
        assert!(
            err.to_string().contains("verifier-only"),
            "agent append of '{tag}' must be rejected at the store seam: {err}"
        );
    }
    // The seam held: only the Opened event is in the log.
    assert_eq!(store.replay().unwrap().events.len(), 1);
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 2 — KNOWN-HOLE PIN TESTS (the attack currently SUCCEEDS)
// ══════════════════════════════════════════════════════════════════════════════

/// Serializes the env-mutating pin tests: `find_cheap` resolves session ids through
/// process-global `HOME`/`CLUSTERVISION_HOME`, so any test that plants a discoverable transcript
/// holds this for its whole body (the freshness.rs convention).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A task with one proposed revision whose `review_sha` is the contact needle the reviewer scan
/// looks for.
fn task_with_revision(repo: &str, branch: &str, review_sha: &str) -> TaskProjection {
    let ts: chrono::DateTime<chrono::Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
    let open = TaskEvent {
        id: "00000000-0000-7000-8000-000000000001".into(),
        task_id: "00000000-0000-7000-8000-000000000001".into(),
        ts,
        by: "h".into(),
        kind: TaskEventKind::Opened {
            title: "t".into(),
            body: String::new(),
            repo: Some(PathBuf::from(repo)),
            issue: None,
            channel: "tasks".into(),
            assignee: None,
        },
    };
    let propose = TaskEvent {
        id: "00000000-0000-7000-8000-000000000002".into(),
        task_id: open.task_id.clone(),
        ts,
        by: "agent:author".into(),
        kind: TaskEventKind::RevisionProposed {
            revision: Revision {
                n: 1,
                branch: branch.into(),
                worktree: None,
                upstream: "origin/main".into(),
                base: "0".repeat(40),
                review_sha: review_sha.into(),
                patch_id: "b".repeat(40),
                reviewer: Some("agent:r".into()),
                session_ref: None,
            },
        },
    };
    let model = TaskReducer::reduce(&[open, propose]).unwrap();
    model.tasks.values().next().unwrap().clone()
}

/// Plant a Claude-format reviewer transcript discoverable at `HOME/.claude/projects/<enc>/<sid>.jsonl`
/// (the adapter walks depth 2) whose single assistant turn issues one Bash tool call.
fn plant_claude_review_session(home: &Path, sid: &str, cwd: &str, tool_command: &str) {
    let proj = home.join(".claude/projects/-repo-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let mut body = String::new();
    body.push_str(
        &serde_json::json!({
            "type": "user", "uuid": "u1", "sessionId": sid,
            "timestamp": "2026-07-16T12:00:00Z", "cwd": cwd,
            "message": {"role": "user", "content": "please review"}
        })
        .to_string(),
    );
    body.push('\n');
    body.push_str(
        &serde_json::json!({
            "type": "assistant", "uuid": "a0", "sessionId": sid,
            "timestamp": "2026-07-16T12:01:00Z",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [
                {"type": "tool_use", "id": "t0", "name": "Bash", "input": {"command": tool_command}}
            ]}
        })
        .to_string(),
    );
    body.push('\n');
    std::fs::write(proj.join(format!("{sid}.jsonl")), body).unwrap();
}

/// DEFENSE (was `pin_goodhart_saw_change_is_currently_gameable`). `saw_change` no longer reads a
/// bare substring: `task/mod.rs::scan_session` runs an evidence hierarchy in command position, so a
/// reviewer who merely `echo`s the review sha — quoting the change's identity with no read and no
/// `git diff/show/log` — lands in the `Mention` tier, which is `saw_change: None` (undetermined),
/// NOT `Some(true)`. The trivial echo forgery is refused.
///
/// Source: codex red-team + self-audit found the substring hole; this closes it. See the honest
/// residual in `receipts_remain_a_heuristic_by_design` — a determined adversary who pointlessly
/// opens a repo file still passes, so this is a raised bar, not a guarantee.
#[test]
fn saw_change_rejects_bare_sha_echo() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = tmp_dir("home7");
    std::env::set_var("HOME", &home);
    std::env::set_var("CLUSTERVISION_HOME", tmp_dir("cvhome7"));
    std::env::set_var("CLUSTERVISION_MAX_STALE_SECS", "0"); // force full discovery — deterministic
    std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
    std::env::remove_var("CURSOR_USER_DIR");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");

    let review_sha = "a".repeat(40);
    let t = task_with_revision("/repo/proj", "task/demo", &review_sha);

    // The forgery: a single Bash call, `echo <review-sha>`. No diff, no file read under the repo —
    // the reviewer demonstrably never looked at the change, only quoted its sha.
    plant_claude_review_session(&home, "reviewsess", "/repo/proj", &format!("echo {review_sha}"));

    let receipts = review_receipts(&t, Some("reviewsess")).expect("a session id was given");
    assert_ne!(
        receipts.saw_change,
        Some(true),
        "echoing the review sha must NOT satisfy the engagement heuristic — the bare mention is \
         undetermined, not a pass"
    );
    assert_eq!(
        receipts.saw_change, None,
        "the bare-mention tier is recorded as undetermined (None), distinct from an observed \
         no-contact rubber stamp (Some(false))"
    );
}

/// DEFENSE (the positive half). A reviewer who actually reads a file UNDER the repo — the
/// structural `Engaged` tier — reads `saw_change: Some(true)`. This is what the echo forgery lacks:
/// a content read whose target is in the change's tree, not just its sha as an argument.
#[test]
fn saw_change_accepts_genuine_file_read() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = tmp_dir("home7b");
    std::env::set_var("HOME", &home);
    std::env::set_var("CLUSTERVISION_HOME", tmp_dir("cvhome7b"));
    std::env::set_var("CLUSTERVISION_MAX_STALE_SECS", "0");
    std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
    std::env::remove_var("CURSOR_USER_DIR");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");

    let review_sha = "a".repeat(40);
    let t = task_with_revision("/repo/proj", "task/demo", &review_sha);

    // Genuine engagement: `cat` a file under the reviewed repo — a content read in command position.
    plant_claude_review_session(&home, "reviewsess", "/repo/proj", "cat /repo/proj/src/lib.rs");

    let receipts = review_receipts(&t, Some("reviewsess")).expect("a session id was given");
    assert_eq!(
        receipts.saw_change,
        Some(true),
        "a content read of a file under the reviewed repo is structural engagement evidence"
    );
}

/// HONEST RESIDUAL (by design). The engagement heuristic raises the cost of a forged review past
/// the trivial echo, but it does not — and cannot cheaply — prove the reviewer understood the
/// change. A determined adversary who pointlessly opens ANY file under the repo (here an unrelated
/// one) still reads `saw_change: Some(true)`. This is acknowledged, not a hole to pin: the bar is
/// raised, not a guarantee. The stronger future step is computing the changed-file set and
/// requiring the read to target it (or diff CONTENT to enter the reviewer's context).
#[test]
fn receipts_remain_a_heuristic_by_design() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = tmp_dir("home7c");
    std::env::set_var("HOME", &home);
    std::env::set_var("CLUSTERVISION_HOME", tmp_dir("cvhome7c"));
    std::env::set_var("CLUSTERVISION_MAX_STALE_SECS", "0");
    std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
    std::env::remove_var("CURSOR_USER_DIR");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");

    let review_sha = "a".repeat(40);
    let t = task_with_revision("/repo/proj", "task/demo", &review_sha);

    // The residual game: open a file under the repo that has nothing to do with the change. It is a
    // read under the repo, so it passes — the heuristic cannot tell purposeful from pointless.
    plant_claude_review_session(&home, "reviewsess", "/repo/proj", "cat /repo/proj/UNRELATED.md");

    let receipts = review_receipts(&t, Some("reviewsess")).expect("a session id was given");
    assert_eq!(
        receipts.saw_change,
        Some(true),
        "pointlessly opening a repo file still passes — the acknowledged residual; the bar is \
         raised past echo, not a guarantee"
    );
}

/// DEFENSE (was a pinned hole). The `by` field is still a self-asserted string, but an endpoint may
/// now opt in to a TOFU per-endpoint token (`CV_TOKEN`/`--token`): the first token seen binds the
/// endpoint (`endpoint -> sha256(token)` in `tasks/endpoints.json`), and thereafter every
/// identity-bearing event stamped as that endpoint must present the matching token or the append is
/// REJECTED at the store seam. This makes impersonation of an opted-in endpoint detectable — a seat
/// running as agent:attacker can no longer append `by: "agent:victim"` once the victim has bound a
/// token, without holding that token.
///
/// Source: codex red-team ("env strings are not identities") — blessed fix: short-lived per-worker
/// credentials, no seats. This asserts (a) an unbound endpoint's events still apply (backward
/// compat), (b) after binding, a tokenless / wrong-token append as that endpoint is rejected, and
/// (c) an append WITH the bound token applies.
#[test]
fn identity_impersonation_is_rejected_when_endpoint_is_bound() {
    let dir = tmp_dir("store");

    // (a) Backward compat: the victim binds its endpoint on first use with a token. The bind itself
    // is a normal, successful claim — TOFU never blocks the first appearance.
    let victim = TaskStore::at(&dir).with_token(Some("victim-secret".into()));
    let task = open_in(&victim, None, "agent:victim", Some("agent:victim"), "victim work");
    victim
        .append_agent_event(new_event(
            Some(&task),
            "agent:victim",
            TaskEventKind::Claimed {
                assignee: "agent:victim".into(),
            },
        ))
        .expect("first-use token binds the endpoint and the claim applies");

    // (b) The attacker, holding NO token, stamps an identity-bearing event as the (now bound)
    // victim: rejected at the store seam, nothing lands.
    let before = TaskStore::at(&dir).replay().unwrap().events.len();
    let attacker = TaskStore::at(&dir); // no token
    let err = attacker
        .append_agent_event(new_event(Some(&task), "agent:victim", TaskEventKind::Released {}))
        .expect_err("a bound endpoint requires its token — a tokenless append is rejected");
    assert!(
        err.to_string().contains("token-bound"),
        "the rejection names the cause: {err}"
    );

    // …and the same with a WRONG token is rejected too (impersonation, not just a missing flag).
    let wrong = TaskStore::at(&dir).with_token(Some("guessed-wrong".into()));
    let err = wrong
        .append_agent_event(new_event(Some(&task), "agent:victim", TaskEventKind::Released {}))
        .expect_err("a wrong token is rejected");
    assert!(err.to_string().contains("does not match"), "{err}");

    assert_eq!(
        TaskStore::at(&dir).replay().unwrap().events.len(),
        before,
        "neither forged append wrote anything"
    );

    // (c) The genuine victim, holding the bound token, still appends normally.
    victim
        .append_agent_event(new_event(Some(&task), "agent:victim", TaskEventKind::Released {}))
        .expect("the real victim, with its token, appends normally");
    let outcome = TaskStore::at(&dir).replay().unwrap();
    assert!(outcome.warnings.is_empty(), "clean log: {:?}", outcome.warnings);
    assert_eq!(
        outcome.model.tasks[&task].state,
        TaskState::Open,
        "the victim's own release applied (claim → release)"
    );
}

/// DESIGN NOTE (deliberate, not a hole). An endpoint that never presented a token stays fully
/// trusted: solo CLI use and the human-at-a-shell path keep working with no token and no binding.
/// TOFU closes the hole for opt-in endpoints only — the durable record has no seats, and an
/// unbound `by` is taken at face value by design (law 3). `phoenix` opts fleet workers in by
/// minting `CV_TOKEN` at spawn beside `CV_ENDPOINT`.
#[test]
fn pin_identity_unbound_is_trusted_by_design() {
    let store = TaskStore::at(tmp_dir("store")); // no token anywhere
    let task = open_in(&store, None, "agent:solo", Some("agent:solo"), "solo work");
    // A full identity-bearing lifecycle with no token at all: every append applies, no warning.
    store
        .append_agent_event(new_event(
            Some(&task),
            "agent:solo",
            TaskEventKind::Claimed {
                assignee: "agent:solo".into(),
            },
        ))
        .expect("an unbound endpoint needs no token — backward compatible");
    propose(&store, &task, "agent:solo", fake_revision(1, "agent:solo"));
    pass(&store, &task, "agent:solo", None);

    let outcome = store.replay().unwrap();
    assert!(
        outcome.warnings.is_empty(),
        "unbound = trusted by design: no impersonation machinery fires. Got: {:?}",
        outcome.warnings
    );
    assert!(outcome.events.iter().all(|e| e.by == "agent:solo"));
}

/// DEFENSE (formerly `pin_done_completion_is_self_reported`, codex red-team item 3). A non-code
/// task's completion can now be OBSERVED, not just attested: `cv task done --check-cmd/-file/-http`
/// makes cv RUN a completion predicate. A failing check REFUSES the done (the task stays open); a
/// passing check records HOW it was verified ([`DoneCheck`]) and the completion carries provenance
/// `checked`, not self-report. This extends law 1 (observed, not attested) from code revisions to
/// revision-less tasks.
///
/// The honest fallback is KEPT DELIBERATELY: a checkless done is still self-report — some tasks
/// have no runnable completion predicate, and pretending otherwise would be the real hole. What
/// changed is that self-report is now a *choice with an alternative*, and it is LABELED as such,
/// never silently equal to an observed completion.
#[test]
fn done_with_a_check_is_verified_not_attested() {
    use cv_core::task::{provenance, CheckSpec, Provenance};

    let store = TaskStore::at(tmp_dir("store"));
    let task = open_in(
        &store,
        None,
        "agent:worker",
        Some("agent:worker"),
        "write the design doc",
    );
    store
        .append_agent_event(new_event(
            Some(&task),
            "agent:worker",
            TaskEventKind::Claimed {
                assignee: "agent:worker".into(),
            },
        ))
        .unwrap();

    // A FAILING check refuses the done. The surface (CLI/MCP) runs the check FIRST and appends only
    // on pass; a nonzero exit errors, so nothing is written and the task stays open. Hermetic: the
    // `false` builtin, no repo dir.
    let refused = CheckSpec::Cmd("false".into()).run(None);
    assert!(refused.is_err(), "a nonzero --check-cmd must refuse the done");
    assert_eq!(
        store.replay().unwrap().model.tasks[&task].state,
        TaskState::Claimed,
        "the refused done left the task open — completion was NOT taken on the agent's word"
    );

    // A PASSING check makes the completion observed. cv ran `true`, records the DoneCheck, and the
    // projection carries it; the provenance is `checked`, distinct from self-report.
    let check = CheckSpec::Cmd("true".into()).run(None).unwrap();
    assert_eq!(check.result, "exit 0");
    store
        .append_agent_event(new_event(
            Some(&task),
            "agent:worker",
            TaskEventKind::Done {
                observed: Some(check.result.clone()),
                check: Some(check.clone()),
            },
        ))
        .unwrap();
    let outcome = store.replay().unwrap();
    let t = &outcome.model.tasks[&task];
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(
        t.done_check.as_ref(),
        Some(&check),
        "the observed check is recorded on the projection"
    );
    let prov = Provenance::checked(Some(t.last_ts));
    assert_eq!(prov.source, provenance::SOURCE_CHECKED);
    assert_ne!(
        prov.source,
        provenance::SOURCE_SELF_REPORT,
        "a checked completion is observed, not self-reported"
    );

    // DESIGN CHOICE, kept deliberately (not a hole): a CHECKLESS done is still self-report — the
    // honest fallback for a task with no runnable completion predicate. It is LABELED self-report,
    // never silently equal to an observed completion.
    let task2 = open_in(
        &store,
        None,
        "agent:worker",
        Some("agent:worker"),
        "think hard about the problem",
    );
    store
        .append_agent_event(new_event(
            Some(&task2),
            "agent:worker",
            TaskEventKind::Done {
                observed: Some("trust me".into()),
                check: None,
            },
        ))
        .unwrap();
    let outcome = store.replay().unwrap();
    let t2 = &outcome.model.tasks[&task2];
    assert_eq!(t2.state, TaskState::Done);
    assert!(t2.done_check.is_none(), "a checkless done carries no observed check");
    assert_eq!(
        Provenance::self_reported(Some(t2.last_ts)).source,
        provenance::SOURCE_SELF_REPORT,
        "the honest fallback stays LABELED self-report"
    );
}

/// KNOWN HOLE (pinned). When a verdict lands on a task, the assignee learns of it only by PULLING
/// their inbox. There is no push: no event is appended to wake them, no file is written, nothing
/// beyond an optional board post. Wake latency is bounded only by when the assignee next looks —
/// unbounded in the worst case.
///
/// Source: codex red-team item 7 (session-start pull only).
///
/// Future fix that FLIPS this: an empty-to-nonempty wake — the substrate signals the assignee the
/// instant their inbox gains an obligation — then assert the wake artifact exists and rename this
/// into a defense.
#[test]
fn pin_wake_latency_is_unbounded() {
    let store = TaskStore::at(tmp_dir("store"));
    let task = open_in(&store, None, "human:pm", Some("agent:worker"), "ship it");
    store
        .append_agent_event(new_event(
            Some(&task),
            "agent:worker",
            TaskEventKind::Claimed {
                assignee: "agent:worker".into(),
            },
        ))
        .unwrap();
    propose(&store, &task, "agent:worker", fake_revision(1, "agent:reviewer"));

    // Before the verdict: the worker owes no unlanded work yet.
    let before_model = store.replay().unwrap();
    let before_events = before_model.events.len();
    let before = InboxRow::compute(&before_model.model, "agent:worker");
    assert!(
        !before.iter().any(|r| r.reason == InboxReason::YourUnlandedWork),
        "no unlanded-work obligation before the verdict: {:?}",
        before.iter().map(|r| r.reason).collect::<Vec<_>>()
    );

    // The reviewer's verdict lands on the task.
    pass(&store, &task, "agent:reviewer", None);

    // The verdict created an obligation — the worker's inbox level changed (unlanded work appeared).
    let model = store.replay().unwrap().model;
    let after = InboxRow::compute(&model, "agent:worker");
    assert!(
        after
            .iter()
            .any(|r| r.reason == InboxReason::YourUnlandedWork && r.id == task),
        "the verdict put unlanded work in the worker's inbox: {:?}",
        after.iter().map(|r| r.reason).collect::<Vec<_>>()
    );

    // But nothing WOKE the worker: the only new artifact is the verdict event itself — no extra
    // notification event was appended to reach the assignee.
    let after_events = store.replay().unwrap().events.len();
    assert_eq!(
        after_events,
        before_events + 1,
        "exactly one new event (the verdict) — PINNED HOLE: no wake/notification event exists"
    );

    // Re-reading the inbox any number of times returns the same obligation, unchanged: nothing
    // consumes it, because nothing was ever pushed. Pull is the only mechanism.
    let reread = InboxRow::compute(&store.replay().unwrap().model, "agent:worker");
    assert_eq!(
        reread.len(),
        after.len(),
        "PINNED HOLE: the obligation persists across reads with no consumer — wake is pull-only. \
         A future empty-to-nonempty wake would flip this pin."
    );
    assert!(reread
        .iter()
        .any(|r| r.id == task && r.reason == InboxReason::YourUnlandedWork));
}
