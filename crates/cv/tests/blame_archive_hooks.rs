//! Comprehensive Phase 2 Track 2 integration suite:
//! - StateStore hash-chained `cv hook receipt` (>64 KiB multi-byte UTF-8 payloads, tamper detection,
//!   and `CV_RECEIPT_STRICT=1` fail-closed deny)
//! - StateStore `cv hook lane` (`LaneRun` PID + `proc_start_time`, `$CV_ENDPOINT` binding, exit summary)
//! - Ancestor-canonicalizing symlinked path normalization in `cv hook edit` (including not-yet-created files),
//!   `cv hook start` concise session pointers (no `cv pack` prompt injection, >2,000-message commit tail),
//!   `cv hooks show` 7-hook coverage, Codex `PostToolUse` trust hashing, and cross-harness `cv hook retired`
//! - `cv task sweep` structural-only branch matching + `issue` GitLab/Jira tracker preservation +
//!   `cv --frozen task list|show|inbox|events` read safety vs mutating exit-2 refusal
//! - `cv dataset --format openai-tools --active-branch --clean-outcome` + `cv splice` `spliced_from` `LineageEdge`

use cv_core::state::{StateError, StateStore};
use cv_testkit::claude::{self, tool_use, user, with};
use cv_testkit::World;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

fn assistant(cwd: &str, uuid: &str, ts: &str, text: &str) -> Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([{"type": "text", "text": text}]),
            json!({"input_tokens": 20, "output_tokens": 10}),
        ),
        json!({"cwd": cwd}),
    )
}

fn assistant_tool(cwd: &str, uuid: &str, ts: &str, name: &str, input: Value) -> Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([tool_use(&format!("toolu_{uuid}"), name, input)]),
            json!({"input_tokens": 10, "output_tokens": 5}),
        ),
        json!({"cwd": cwd}),
    )
}

fn tool_result(cwd: &str, uuid: &str, ts: &str, tool_use_id: &str, content: &str, is_error: bool) -> Value {
    let mut tr = claude::tool_result(uuid, ts, tool_use_id, content);
    if is_error {
        tr["message"]["content"][0]["is_error"] = json!(true);
    }
    with(tr, json!({"cwd": cwd}))
}

#[test]
fn state_store_receipt_chain_handles_over_64kib_multibyte_tamper_detection_and_strict_deny() {
    let w = World::new("receipt-chain-64k");

    // >72 KiB of 4-byte UTF-8 emojis ("🦀🎉🔥") so any naive 64 KiB byte slice would split mid-codepoint!
    let huge_utf8 = "🦀🎉🔥".repeat(7_000);
    assert!(huge_utf8.len() > 72 * 1024);

    let pre_payload = json!({
        "session_id": "rcpt-sess",
        "transcript_path": w.transcript("rcpt-sess"),
        "cwd": "/work/proj",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_big_1",
        "tool_input": {"file_path": "/work/proj/out.txt", "content": huge_utf8},
    });
    let (ok, code, out, err) = w.cv_full(
        &["hook", "receipt", "--harness", "claude"],
        &[],
        Some(&pre_payload.to_string()),
    );
    assert!(ok, "PreToolUse receipt exited {code}: {err}");
    assert_eq!(out.trim(), "", "successful receipt is silent on stdout");

    let post_payload = json!({
        "session_id": "rcpt-sess",
        "transcript_path": w.transcript("rcpt-sess"),
        "cwd": "/work/proj",
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_big_1",
        "tool_input": {"file_path": "/work/proj/out.txt"},
        "tool_response": {"ok": true, "echo": "🦀".repeat(20_000)},
    });
    let (ok, code, out, err) = w.cv_full(
        &["hook", "receipt", "--harness", "claude"],
        &[],
        Some(&post_payload.to_string()),
    );
    assert!(ok, "PostToolUse receipt exited {code}: {err}");
    assert_eq!(out.trim(), "");

    // Legacy flat `$CLUSTERVISION_HOME/receipts/` must NOT be created.
    assert!(
        !w.cv_home.join("receipts").exists(),
        "legacy flat receipts/ directory must not exist"
    );

    let store = StateStore::at_dir(&w.cv_home);
    let receipts = store.receipts_for_session("rcpt-sess").unwrap();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].seq, 1);
    assert_eq!(receipts[0].prev_hash, "");
    assert_eq!(receipts[1].seq, 2);
    assert_eq!(
        receipts[1].prev_hash, receipts[0].receipt_hash,
        "second receipt must chain onto first receipt_hash"
    );
    assert_eq!(store.verify_receipt_chain("rcpt-sess").unwrap(), 2);
    assert!(
        store.inbox_count().unwrap() >= 2,
        "capture_inbox must record receipt envelopes"
    );

    // Tamper with `body_json` of seq 1 directly in SQLite: `verify_receipt_chain` MUST detect tampering!
    {
        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE receipts SET body_json = '{\"forged\":true}' WHERE session_id = 'rcpt-sess' AND seq = 1",
            [],
        )
        .unwrap();
    }
    let tamper_err = store
        .verify_receipt_chain("rcpt-sess")
        .expect_err("forged receipt body must fail chain verification");
    assert!(
        matches!(&tamper_err, StateError::InvalidArgument(msg) if msg.contains("receipt chain tampered at seq 1")),
        "expected tampered error at seq 1, got: {tamper_err:?}"
    );

    // `CV_RECEIPT_STRICT=1`: if `state.db` cannot be written (e.g. replaced by a directory),
    // `PreToolUse` MUST fail closed with `permissionDecision: "deny"`.
    let w_strict = World::new("receipt-strict-deny");
    fs::create_dir_all(w_strict.cv_home.join("state.db")).unwrap();
    let strict_in = json!({
        "session_id": "strict-sess",
        "cwd": "/work/proj",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "toolu_s1",
        "tool_input": {"command": "echo hi"},
    });
    let (ok, _, out, _) = w_strict.cv_full(
        &["hook", "receipt", "--harness", "claude"],
        &[("CV_RECEIPT_STRICT", "1")],
        Some(&strict_in.to_string()),
    );
    assert!(ok, "hook exits 0 with JSON deny decision");
    let decision: Value = serde_json::from_str(out.trim()).expect("strict deny JSON on stdout");
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn hook_lane_records_lane_run_pid_endpoint_and_exit_in_state_store() {
    let w = World::new("hook-lane-statestore");
    let start_payload = json!({
        "session_id": "parent-sess-1",
        "transcript_path": w.transcript("parent-sess-1"),
        "cwd": "/work/proj",
        "hook_event_name": "SubagentStart",
        "agent_id": "agent-lane42",
        "agent_type": "general-purpose",
    });
    let (ok, _, out, err) = w.cv_full(
        &["hook", "lane", "--harness", "claude"],
        &[("CV_ENDPOINT", "lane:worker-42")],
        Some(&start_payload.to_string()),
    );
    assert!(ok && out.trim().is_empty(), "err: {err}");

    let store = StateStore::at_dir(&w.cv_home);
    let runs = store.list_lane_runs_for_session("parent-sess-1").unwrap();
    assert_eq!(runs.len(), 1, "SubagentStart coalesces into a single LaneRun: {runs:?}");
    let run = &runs[0];
    assert!(
        run.run_id.starts_with("parent-sess-1:lane42:"),
        "unexpected run_id: {}",
        run.run_id
    );
    assert_eq!(run.agent_id.as_deref(), Some("lane42"));
    assert!(run.exited_at.is_none());
    assert!(run.pid.is_some(), "parent/payload PID captured: {run:?}");
    assert!(
        run.proc_start_time.is_some(),
        "OS proc_start_time captured alongside PID: {run:?}"
    );
    let endpoints = store.endpoints_for_run(&run.run_id).unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].endpoint, "lane:worker-42");
    let bound = store
        .latest_run_for_endpoint("lane:worker-42")
        .unwrap()
        .expect("CV_ENDPOINT bound in lane_endpoints");
    assert_eq!(bound.run_id, run.run_id);

    let stop_payload = json!({
        "session_id": "parent-sess-1",
        "transcript_path": w.transcript("parent-sess-1"),
        "cwd": "/work/proj",
        "hook_event_name": "SubagentStop",
        "agent_id": "lane42",
        "agent_type": "general-purpose",
        "last_assistant_message": "All checks green.\nextra detail",
    });
    let (ok, _, out, err) = w.cv_full(
        &["hook", "lane", "--harness", "claude"],
        &[("CV_ENDPOINT", "lane:worker-42")],
        Some(&stop_payload.to_string()),
    );
    assert!(ok && out.trim().is_empty(), "err: {err}");

    let runs_after = store.list_lane_runs_for_session("parent-sess-1").unwrap();
    assert_eq!(
        runs_after.len(),
        1,
        "SubagentStop coalesces onto the started LaneRun: {runs_after:?}"
    );
    let stopped = store.get_lane_run(&run.run_id).unwrap().unwrap();
    assert_eq!(stopped.exit_status.as_deref(), Some("hook_stop"));
    assert_eq!(stopped.last_text.as_deref(), Some("All checks green."));
    assert!(stopped.exited_at.is_some());
}

#[cfg(unix)]
#[test]
fn ancestor_canonicalizing_path_normalizer_catches_symlinked_edit_conflicts_before_file_exists() {
    let w = World::new("symlink-edit-guard");
    let real_ws = w.base.join("real_ws");
    fs::create_dir_all(real_ws.join("src")).unwrap();
    let link_ws = w.base.join("link_ws");
    std::os::unix::fs::symlink(&real_ws, &link_ws).unwrap();

    // Target file `src/future_mod.rs` intentionally DOES NOT EXIST on disk yet!
    let via_symlink = link_ws.join("src/future_mod.rs");
    let via_real = real_ws.join("src/future_mod.rs");
    assert!(!via_real.exists());

    // Session A (`live-a`) proposed/wrote `via_symlink` moments ago.
    w.write_session(
        "live-a",
        &[
            user("live-a", "u0", "2026-10-01T10:00:00Z", "create future_mod"),
            assistant_tool(
                &link_ws.to_string_lossy(),
                "a0",
                "2026-10-01T10:00:05Z",
                "Write",
                json!({"file_path": via_symlink, "content": "pub fn future() {}\n"}),
            ),
        ],
    );
    w.cv_ok(&["ls"]);
    w.cv_ok(&["events", "live-a"]);

    // Session B (`live-b`) tries to edit `via_real` (resolved real path, file still nonexistent):
    // ancestor canonicalization resolves both `link_ws/src/future_mod.rs` and `real_ws/src/future_mod.rs`
    // to `<canonical_real_ws>/src/future_mod.rs` and catches the live conflict!
    let edit_in = json!({
        "session_id": "live-b",
        "transcript_path": w.transcript("live-b"),
        "cwd": via_real.parent().unwrap(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_b1",
        "tool_input": {"file_path": via_real, "content": "pub fn colliding() {}\n"},
    });
    let (ok, _, out, err) = w.cv_full(
        &["hook", "edit", "--harness", "claude"],
        &[],
        Some(&edit_in.to_string()),
    );
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
    assert!(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("live-a"),
        "{v}"
    );
}

#[test]
fn hooks_show_reports_seven_hooks_codex_post_tool_use_start_over_2000_msgs_and_cross_harness_retired() {
    let w = World::new("hooks-seven-and-retired");

    // 1) `cv hooks show claude` (human & `--json`) reports all 7 hooks (`compact`, `start`, `lane`,
    //    `decisions`, `edit`, `retired`, `receipt`).
    let (human_show, _) = w.cv_ok(&["hooks", "show", "claude"]);
    for name in ["compact", "start", "lane", "decisions", "edit", "retired", "receipt"] {
        assert!(
            human_show.contains(&format!("hook {name}")),
            "human `hooks show` missing {name}:\n{human_show}"
        );
    }
    let (json_show, _) = w.cv_ok(&["hooks", "show", "claude", "--json"]);
    let show_val: Value = serde_json::from_str(&json_show).unwrap();
    let hook_names: Vec<&str> = show_val["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["hook"].as_str().unwrap())
        .collect();
    assert_eq!(
        hook_names,
        vec!["compact", "start", "lane", "decisions", "edit", "retired", "receipt"]
    );

    // 2) `cv hooks install codex` + `--retired` + `--receipts` installs all 9 entries including `PostToolUse` (`receipt`),
    //    and `hooks show codex --json` computes `post_tool_use` trust keys cleanly.
    w.cv_ok(&["hooks", "install", "codex"]);
    w.cv_ok(&["hooks", "install", "codex", "--retired"]);
    w.cv_ok(&["hooks", "install", "codex", "--receipts"]);
    let (codex_json, _) = w.cv_ok(&["hooks", "show", "codex", "--json"]);
    let codex_val: Value = serde_json::from_str(&codex_json).unwrap();
    let installed = codex_val["installed"].as_array().unwrap();
    assert_eq!(installed.len(), 9, "{codex_val}");
    assert!(
        installed.iter().any(|r| {
            r["event"] == "PostToolUse"
                && r["trust_key"].as_str().unwrap().contains(":post_tool_use:")
                && r["command"].as_str().unwrap().contains("hook receipt --harness codex")
        }),
        "Codex PostToolUse receipt hook must be reported with post_tool_use trust key:\n{codex_val}"
    );

    // 3) `hook start`: >2,050-message session with a commit near the end is STILL found (no 2,000-msg bailout),
    //    and `hook start` emits concise structured session pointers with NO `cv pack` prompt injection!
    let mut long_lines = Vec::with_capacity(2_060);
    long_lines.push(json!({"type": "ai-title", "aiTitle": "deep marathon", "sessionId": "longsess"}));
    for i in 0..2_050 {
        long_lines.push(user("longsess", &format!("u{i}"), "2026-10-01T10:00:00Z", "step"));
    }
    long_lines.push(assistant_tool(
        "/work/marathon",
        "ac1",
        "2026-10-01T11:00:00Z",
        "Bash",
        json!({"command": "git commit -m 'marathon ship'"}),
    ));
    long_lines.push(tool_result(
        "/work/marathon",
        "rc1",
        "2026-10-01T11:00:01Z",
        "toolu_ac1",
        "[main deadbee] marathon ship\n 1 file changed",
        false,
    ));
    // Ensure `/work/marathon` cwd is stamped on first user message so catalog indexes cwd `/work/marathon`.
    long_lines[1] = with(long_lines[1].clone(), json!({"cwd": "/work/marathon"}));
    w.write_session("longsess", &long_lines);
    w.cv_ok(&["ls"]);

    let start_in = json!({
        "session_id": "fresh-sess",
        "cwd": "/work/marathon",
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let (ok, _, start_out, _) = w.cv_full(
        &["hook", "start", "--harness", "claude"],
        &[],
        Some(&start_in.to_string()),
    );
    assert!(ok);
    let start_json: Value = serde_json::from_str(start_out.trim()).unwrap();
    let start_ctx = start_json["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(
        start_ctx.contains("last commit deadbee"),
        "commit past 2,000 messages must be surfaced:\n{start_ctx}"
    );
    assert!(
        !start_ctx.contains("cv pack"),
        "hook_start must never inject `cv pack` prompt bloat:\n{start_ctx}"
    );

    // 4) Cross-harness `hook retired`: retiring `old-lane` blocks both:
    //    - the retired Codex thread's own `exec_command` calls (`session_id == "old-lane"`), and
    //    - another session's Codex `send_input` / Code Mode `exec` / `Task(resume)` addressing `old-lane`!
    w.cv_ok(&["retire", "old-lane", "--successor", "new-lane"]);
    for (caller_sid, tool_name, tool_input) in [
        ("old-lane", "exec_command", json!({"cmd": "cargo build"})),
        ("root-sess", "send_input", json!({"id": "old-lane", "chars": "y\n"})),
        (
            "root-sess",
            "exec",
            json!("await tools.send_input({ id: 'old-lane', message: 'ping' });"),
        ),
        (
            "root-sess",
            "Task",
            json!({"resume": "agent-old-lane", "prompt": "continue"}),
        ),
    ] {
        let retired_in = json!({
            "session_id": caller_sid,
            "cwd": "/work/marathon",
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
        });
        let (ok, _, out, _) = w.cv_full(
            &["hook", "retired", "--harness", "codex"],
            &[],
            Some(&retired_in.to_string()),
        );
        assert!(ok);
        let dec: Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("tool {tool_name} ({caller_sid}) was not blocked ({e}): {out:?}"));
        assert_eq!(dec["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            dec["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("new-lane"),
            "{dec}"
        );
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn task_sweep_structural_branches_issue_tracker_refs_and_frozen_task_gates() {
    let w = World::new("task-sweep-and-frozen");
    let repo = w.base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    // Create merged branches whose names are ordinary English words ("feature", "refactor", "polish")
    // plus one explicit structural branch ("feat/widget-v2").
    git(&repo, &["branch", "feature"]);
    git(&repo, &["branch", "refactor"]);
    git(&repo, &["branch", "polish"]);
    git(&repo, &["branch", "feat/widget-v2"]);

    // Task 1: English prose title containing "Feature", "refactor", "polish", and GitLab nested issue ref.
    let (open1, _) = w.cv_ok(&[
        "task",
        "open",
        "Feature refactor and polish for auth/login",
        "--repo",
        repo.to_str().unwrap(),
        "--issue",
        "group/subgroup/repo#42",
    ]);
    let id1 = open1.lines().last().unwrap().trim().to_string();

    // Task 2: Explicit `branch:feat/widget-v2` tag and Jira `PROJ-123` issue ref.
    let (open2, _) = w.cv_ok(&[
        "task",
        "open",
        "Ship widget v2",
        "--repo",
        repo.to_str().unwrap(),
        "--tags",
        "branch:feat/widget-v2",
        "--issue",
        "PROJ-123",
    ]);
    let id2 = open2.lines().last().unwrap().trim().to_string();

    // Verify `issue` strings were NEVER mangled into `<cwd>/group/subgroup/repo#42` or `<cwd>/PROJ-123`.
    let (show1, _) = w.cv_ok(&["task", "show", &id1, "--json"]);
    let t1: Value = serde_json::from_str(&show1).unwrap();
    assert_eq!(t1["issue"], "group/subgroup/repo#42", "{t1}");

    let (show2, _) = w.cv_ok(&["task", "show", &id2, "--json"]);
    let t2: Value = serde_json::from_str(&show2).unwrap();
    assert_eq!(t2["issue"], "PROJ-123", "{t2}");

    // `cv task sweep --repo <repo> --json`:
    // - Task 1 MUST NOT be flagged (neither English prose words nor GitLab `group/subgroup/repo#42` are local files/branches!).
    // - Task 2 MUST be flagged for merged structural branch `feat/widget-v2` (and NOT for `PROJ-123`).
    let (sweep_out, _) = w.cv_ok(&["task", "sweep", "--repo", repo.to_str().unwrap(), "--json"]);
    let candidates: Vec<Value> = serde_json::from_str(&sweep_out).unwrap();
    assert!(
        !candidates.iter().any(|c| c["task_id"] == id1),
        "English prose words and GitLab tracker handle must never trigger sweep:\n{sweep_out}"
    );
    let hit2 = candidates
        .iter()
        .find(|c| c["task_id"] == id2)
        .unwrap_or_else(|| panic!("expected structural branch:feat/widget-v2 hit in:\n{sweep_out}"));
    assert_eq!(
        hit2["reasons"],
        json!(["names branch `feat/widget-v2`, merged into main"])
    );

    // Addendum #10: Read-only `cv --frozen task list|show|inbox|events` MUST succeed (exit 0)
    // AND never create `state.db-shm`, `state.db-wal`, or `tasks/state.json` on disk, while
    // mutating `task` subcommands MUST fail closed with exit 2!
    {
        let conn = rusqlite::Connection::open(w.cv_home.join("state.db")).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let _ = fs::remove_file(w.cv_home.join("state.db-wal"));
    let _ = fs::remove_file(w.cv_home.join("state.db-shm"));
    let _ = fs::remove_file(w.cv_home.join("tasks/state.json"));
    fn snapshot_tree(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        for entry in fs::read_dir(dir).unwrap() {
            let e = entry.unwrap();
            let p = e.path();
            if p.is_dir() {
                snapshot_tree(root, &p, out);
            } else if p.is_file() {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut before_frozen = std::collections::BTreeMap::new();
    snapshot_tree(&w.cv_home, &w.cv_home, &mut before_frozen);
    w.cv_ok(&["--frozen", "task", "list", "--json"]);
    w.cv_ok(&["--frozen", "task", "show", &id1, "--json"]);
    w.cv_ok(&["--frozen", "task", "inbox", "cv", "--json"]);
    w.cv_ok(&["--frozen", "task", "events"]);
    let mut after_frozen = std::collections::BTreeMap::new();
    snapshot_tree(&w.cv_home, &w.cv_home, &mut after_frozen);
    assert_eq!(
        after_frozen, before_frozen,
        "--frozen task read commands must not create -wal/-shm or mutate $CLUSTERVISION_HOME"
    );

    // Note: `task sweep` only reads git + task fold? Wait: `writes_store` marks non-list/show/inbox/events
    // `TaskCmd` variants as refused or let's verify `open` & `claim` exit 2!
    for bad_args in [
        &["--frozen", "task", "open", "forbidden"][..],
        &["--frozen", "task", "claim", &id1, "--from", "lane:x"][..],
    ] {
        let (ok, code, _, err) = w.cv(bad_args);
        assert!(
            !ok && code == 2,
            "expected exit 2 for {bad_args:?}, got code={code} err={err}"
        );
    }
}

#[test]
fn dataset_openai_tools_flags_and_splice_lineage_edge_in_state_store() {
    let w = World::new("dataset-and-splice-lineage");

    // Session 1 (`clean-tool-sess`): clean tool call + result + final assistant answer.
    w.write_session(
        "clean-tool-sess",
        &[
            user("clean-tool-sess", "u1", "2026-10-01T10:00:00Z", "run echo"),
            assistant_tool(
                "/work/ds",
                "a1",
                "2026-10-01T10:00:01Z",
                "Bash",
                json!({"command": "echo hi"}),
            ),
            tool_result("/work/ds", "u2", "2026-10-01T10:00:02Z", "toolu_a1", "hi\n", false),
            assistant("/work/ds", "a2", "2026-10-01T10:00:03Z", "It printed hi."),
        ],
    );

    // Session 2 (`dirty-err-sess`): ends in an errored tool result (dirty outcome).
    w.write_session(
        "dirty-err-sess",
        &[
            user("dirty-err-sess", "du1", "2026-10-01T10:05:00Z", "run broken"),
            assistant_tool(
                "/work/ds",
                "da1",
                "2026-10-01T10:05:01Z",
                "Bash",
                json!({"command": "false"}),
            ),
            tool_result(
                "/work/ds",
                "du2",
                "2026-10-01T10:05:02Z",
                "toolu_da1",
                "exit status 1",
                true,
            ),
        ],
    );

    let (ds_out, _) = w.cv_ok(&[
        "dataset",
        "--format",
        "openai-tools",
        "--active-branch",
        "--clean-outcome",
        "--min-messages",
        "2",
    ]);
    let rows: Vec<Value> = ds_out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "--clean-outcome must keep clean-tool-sess and filter out dirty-err-sess:\n{ds_out}"
    );
    let msgs = rows[0]["messages"].as_array().unwrap();
    assert!(
        msgs.iter().any(|m| m["content"] == "It printed hi."),
        "retained record must be clean-tool-sess:\n{}",
        rows[0]
    );
    assert!(
        msgs.iter().any(|m| m["tool_calls"].is_array()),
        "openai-tools format must emit structured tool_calls:\n{}",
        rows[0]
    );
    assert!(
        msgs.iter().any(|m| m["role"] == "tool"),
        "openai-tools format must emit role='tool' messages:\n{}",
        rows[0]
    );

    // `cv splice clean-tool-sess:0..4 --session --json`: emits a resumable session AND records
    // a `spliced_from` `LineageEdge` in `StateStore` (`clean-tool-sess` -> `<minted_id>`).
    let (splice_out, _) = w.cv_ok(&["splice", "clean-tool-sess:0..4", "--session", "--json"]);
    let splice_val: Value = serde_json::from_str(&splice_out).unwrap();
    let minted_id = splice_val["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing session_id in splice JSON: {splice_val}"));

    let store = StateStore::at_dir(&w.cv_home);
    let ancestors = store.lineage_ancestors(minted_id).unwrap();
    assert!(
        ancestors.iter().any(|e| {
            e.src_session_id == "clean-tool-sess" && e.dst_session_id == minted_id && e.edge_kind == "spliced_from"
        }),
        "expected spliced_from LineageEdge from clean-tool-sess -> {minted_id}, got: {ancestors:?}"
    );
}
