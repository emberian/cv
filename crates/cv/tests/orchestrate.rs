//! End-to-end tests for the orchestrator's instruments: `cv show --kind prompt,answer` and `cv lanes`.
//! Each builds a hermetic Claude session under a temp `$HOME` — a parent transcript with human
//! prompts, slash-command bookkeeping rows, an `AskUserQuestion` answer, a compaction and the
//! `queue-operation` notifications the harness writes when a child stops — plus a `subagents/`
//! dir with one completed, one stranded and one running lane — and drives the real binary through
//! `cv_testkit::World` (own `$HOME` + `$CLUSTERVISION_HOME`).

use cv_testkit::claude::{self, compact_summary, notification, subagent_stop, tool_result, tool_use, user};
use cv_testkit::World;
use serde_json::json;

/// A directly-spawned (`Agent`-tool) sub-agent of `sid` with its meta.
fn write_agent(w: &World, sid: &str, agent_id: &str, description: &str, lines: &[serde_json::Value]) {
    let meta = json!({
        "agentType": "general-purpose",
        "description": description,
        "toolUseId": format!("toolu_{agent_id}"),
        "model": "opus"
    });
    w.write_agent(sid, agent_id, &meta, lines);
}

fn assistant(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    claude::reply(
        uuid,
        ts,
        json!([{"type": "text", "text": text}]),
        json!({"input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 1000}),
    )
}

fn assistant_tool(uuid: &str, ts: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    claude::reply(
        uuid,
        ts,
        json!([tool_use(&format!("toolu_{uuid}"), name, input)]),
        json!({"input_tokens": 10, "output_tokens": 5}),
    )
}

fn boundary(uuid: &str, ts: &str) -> serde_json::Value {
    claude::boundary(uuid, ts, "manual", 900_000)
}

const SID: &str = "orchsess";

/// The rendered `cv lanes` row of lane `id`: the line that starts with its short id. Semantic
/// checks read `--json`; this only keeps the table honest about what it shows for a lane.
fn lane_row<'a>(out: &'a str, id: &str) -> &'a str {
    out.lines()
        .find(|l| l.trim_start().starts_with(id))
        .unwrap_or_else(|| panic!("no row for {id}:\n{out}"))
}

/// The parent: two human prompts, a `/compact` with its bookkeeping rows, an AskUserQuestion
/// answer, two replies, three lane notifications, and a human prompt after the compaction.
fn build_world(tag: &str) -> World {
    let w = World::new(tag);
    w.write_session(
        SID,
        &[
            user(SID, "u0", "2026-10-01T10:00:00Z", "start the swarm please"),
            assistant_tool("a0", "2026-10-01T10:00:05Z", "AskUserQuestion", serde_json::json!({"questions": [{"question": "Where?"}]})),
            tool_result("r0", "2026-10-01T10:01:00Z", "toolu_a0", "The user answered: \"Where?\"=\"persvati\""),
            assistant("a1", "2026-10-01T10:02:00Z", "Noted. The ResourceTargetAdmission externalKind exhaustiveness check is queued for the standing integrator right after FINAL-1 lands."),
            user(SID, "u1", "2026-10-01T10:03:00Z", "/compact"),
            user(SID, "u2", "2026-10-01T10:03:00Z", "<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"),
            user(SID, "u3", "2026-10-01T10:03:30Z", "<local-command-stdout>Compacted</local-command-stdout>"),
            boundary("b1", "2026-10-01T10:03:31Z"),
            compact_summary("s1", "2026-10-01T10:03:32Z", "SUMMARY: the early context"),
            user(SID, "u4", "2026-10-01T10:04:00Z", "hi :) we /compacted. carry on"),
            notification("2026-10-01T10:05:00Z", "aaa1", "completed"),
            notification("2026-10-01T10:06:00Z", "bbb2", "completed"),
            assistant("a2", "2026-10-01T10:07:00Z", "The hermes tariff reconciliation is an inconsistency to settle in one later lane, not tonight."),
            user(SID, "u5", "2026-10-01T10:08:00Z", "ok"),
        ],
    );
    // aaa1: finished cleanly.
    write_agent(
        &w,
        SID,
        "aaa1",
        "LANE-A: the first lane",
        &[
            user(SID, "x0", "2026-10-01T10:00:10Z", "You are lane A."),
            assistant_tool(
                "x1",
                "2026-10-01T10:00:20Z",
                "Bash",
                serde_json::json!({"command": "export CV_ENDPOINT=lane:lane-a; cargo build"}),
            ),
            tool_result("x2", "2026-10-01T10:01:00Z", "toolu_x1", "ok"),
            assistant(
                "x3",
                "2026-10-01T10:04:50Z",
                "Lane A is done.\nThe branch is pushed and the tests pass.",
            ),
            subagent_stop("x4", "2026-10-01T10:04:55Z"),
        ],
    );
    // bbb2: stopped on a promise — the strand class.
    write_agent(
        &w,
        SID,
        "bbb2",
        "LANE-B: waits on a build",
        &[
            user(SID, "y0", "2026-10-01T10:00:30Z", "You are lane B."),
            assistant_tool(
                "y1",
                "2026-10-01T10:00:40Z",
                "Bash",
                serde_json::json!({"command": "ssh hbox swarm-build lake build", "run_in_background": true}),
            ),
            tool_result("y2", "2026-10-01T10:00:41Z", "toolu_y1", "started"),
            assistant(
                "y3",
                "2026-10-01T10:05:50Z",
                "Build is mirrored on hbox. Waiting on notifications.",
            ),
            subagent_stop("y4", "2026-10-01T10:05:55Z"),
        ],
    );
    // ccc3: still running (no stop, no notification), mid tool call.
    write_agent(
        &w,
        SID,
        "ccc3",
        "LANE-C: still at it",
        &[
            user(SID, "z0", "2026-10-01T10:01:00Z", "You are lane C."),
            assistant("z1", "2026-10-01T10:01:10Z", "Reading first."),
            assistant_tool(
                "z2",
                "2026-10-01T10:09:00Z",
                "Read",
                serde_json::json!({"file_path": "/work/proj/src/lib.rs"}),
            ),
        ],
    );
    // ddd4: finished — the transcript ends in a final report — but no SubagentStop and no
    // <task-notification> ever recorded it (the harness restarted). It must not read as running.
    write_agent(
        &w,
        SID,
        "ddd4",
        "LANE-D: lost notification",
        &[
            user(SID, "q0", "2026-10-01T10:01:30Z", "You are lane D."),
            assistant_tool(
                "q1",
                "2026-10-01T10:01:40Z",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
            tool_result("q2", "2026-10-01T10:05:00Z", "toolu_q1", "ok"),
            assistant(
                "q3",
                "2026-10-01T10:06:00Z",
                "Lane D report.\nAll 12 tests pass; nothing left to do.",
            ),
        ],
    );
    w
}

#[test]
fn show_kind_prompt_lists_only_the_person_and_the_answers() {
    let w = build_world("prompts");
    let (out, _) = w.cv_ok(&["show", SID, "--kind", "prompt,answer"]);
    assert!(out.contains("4 typed, 1 answered"), "{out}");
    assert!(out.contains("[0] ") && out.contains("start the swarm please"), "{out}");
    assert!(
        out.contains("answer\nThe user answered: \"Where?\"=\"persvati\""),
        "{out}"
    );
    assert!(
        out.contains("/compact\n"),
        "the typed slash command is the person's:\n{out}"
    );
    assert!(
        !out.contains("<command-name>"),
        "slash-command bookkeeping rows are harness notices:\n{out}"
    );
    assert!(!out.contains("<local-command-stdout>"), "{out}");
    assert!(
        !out.contains("SUMMARY:"),
        "the compaction summary is not a prompt:\n{out}"
    );
    assert!(out.contains("carry on") && out.contains("\nok\n"), "{out}");

    let (json, _) = w.cv_ok(&["show", SID, "--kind", "prompt,answer", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["prompt", "answer", "prompt", "prompt", "prompt"], "{json}");
    assert_eq!(rows[1]["index"], 2);
    assert!(
        rows[0]["timestamp"]
            .as_str()
            .unwrap()
            .starts_with("2026-10-01T10:00:00"),
        "{json}"
    );

    // --pre-compaction: only what came before the boundary.
    let (json, err) = w.cv_ok(&["show", SID, "--kind", "prompt,answer", "--pre-compaction", "--json"]);
    assert!(err.contains("pre-compaction #1 of 1"), "{err}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let texts: Vec<&str> = rows.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert!(
        texts.contains(&"start the swarm please") && texts.contains(&"/compact"),
        "{texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("carry on")),
        "post-compaction prompt excluded: {texts:?}"
    );

    // One kind of the pair narrows the rows; a window narrows them too.
    let kinds_of = |args: &[&str]| -> Vec<String> {
        let (json, _) = w.cv_ok(args);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        rows.iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(kinds_of(&["show", SID, "--kind", "prompt", "--json"]), ["prompt"; 4]);
    assert_eq!(kinds_of(&["show", SID, "--kind", "answer", "--json"]), ["answer"]);
    assert_eq!(
        kinds_of(&["show", SID, "--kind", "prompt,answer", "--range", "0..2", "--json"]),
        ["prompt", "answer"]
    );
    assert_eq!(
        kinds_of(&["show", SID, "--kind", "prompt,answer", "--first", "3", "--json"]),
        ["prompt", "answer", "prompt"]
    );
    // A kind with no view, two views at once, or `--summaries` off the compaction view: exit 2.
    for args in [
        &["show", SID, "--kind", "reply"][..],
        &["show", SID, "--kind", "prompt,compaction_boundary"],
        &["show", SID, "--kind", "prompt", "--summaries"],
    ] {
        let (ok, code, _, err) = w.cv(args);
        assert!(!ok && code == 2 && err.contains("--kind"), "{args:?}: {err}");
    }
}

#[test]
fn show_kind_post_filter_last_max_bytes_subagents_and_question_pairing() {
    let w = build_world("kind-post-filter");
    // Session with 2 prompts, 1 real AskUserQuestion answer, and 30 trailing Bash calls (one of
    // whose outputs starts with `"The user answered"` — must NOT be mistaken for an answer).
    let tail_sid = "22222222-3333-4444-5555-666666666666";
    let mut records = vec![
        user(tail_sid, "t0", "2026-10-01T10:00:00Z", "first directive"),
        assistant_tool(
            "ta0",
            "2026-10-01T10:00:05Z",
            "AskUserQuestion",
            serde_json::json!({"questions": [{"question": "Target?"}]}),
        ),
        tool_result("tr0", "2026-10-01T10:00:10Z", "toolu_ta0", "ship it"),
        user(tail_sid, "t1", "2026-10-01T10:00:20Z", "second directive"),
    ];
    for i in 0..25 {
        let aid = format!("tb{i}");
        let rid = format!("trb{i}");
        let out = if i == 0 {
            "The user answered: fake shell log line"
        } else {
            "build step ok"
        };
        records.push(assistant_tool(
            &aid,
            "2026-10-01T10:01:00Z",
            "Bash",
            serde_json::json!({"command": "cargo check"}),
        ));
        records.push(tool_result(&rid, "2026-10-01T10:01:01Z", &format!("toolu_{aid}"), out));
    }
    w.write_session(tail_sid, &records);

    // `--last 2` slices AFTER `--kind` projection (returns 2 rows, not 0 from the 50 Bash turns).
    let (json, _) = w.cv_ok(&["show", tail_sid, "--kind", "prompt,answer", "--last", "2", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let texts: Vec<&str> = rows.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["ship it", "second directive"], "{json}");

    // `--around ID` composes with post-filtered `--kind` views: `t1` is at raw message index 3
    // (projected row 2), and an intervening assistant turn (`ta0` at index 1) anchors surrounding rows.
    let (json, _) = w.cv_ok(&[
        "show",
        tail_sid,
        "--kind",
        "prompt,answer",
        "--around",
        "t1",
        "--context",
        "0",
        "--json",
    ]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 1, "{json}");
    assert_eq!(rows[0]["text"], "second directive");

    let (json, _) = w.cv_ok(&[
        "show",
        tail_sid,
        "--kind",
        "prompt,answer",
        "--around",
        "ta0",
        "--context",
        "1",
        "--json",
    ]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let texts: Vec<&str> = rows.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["first directive", "ship it"], "{json}");

    // `--max-bytes` bounds output cleanly and emits a `--range` continuation hint.
    let (json, err) = w.cv_ok(&[
        "show",
        tail_sid,
        "--kind",
        "prompt,answer",
        "--max-bytes",
        "180",
        "--json",
    ]);
    let page1: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(page1.len(), 1, "{json}");
    assert!(err.contains("continue with --range 1.."), "{err}");

    let (text_out, _) = w.cv_ok(&["show", tail_sid, "--kind", "prompt,answer", "--max-bytes", "200"]);
    assert!(text_out.contains("first directive"), "{text_out}");
    assert!(text_out.contains("continue with --range "), "{text_out}");

    // Bare sub-agent id, `agent-<id>`, and `<parent> --agent <sub>` all work with `--kind`.
    for args in [
        &["show", "aaa1", "--kind", "prompt,answer", "--json"][..],
        &["show", "agent-aaa1", "--kind", "prompt,answer", "--json"],
        &["show", SID, "--agent", "aaa1", "--kind", "prompt,answer", "--json"],
    ] {
        let (json, _) = w.cv_ok(args);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(rows.len(), 1, "{args:?}: {json}");
        assert_eq!(rows[0]["text"], "You are lane A.");
    }
}

#[test]
fn lanes_table_status_tokens_and_the_strand_class() {
    let w = build_world("lanes");
    let (out, _) = w.cv_ok(&["lanes", SID]);
    assert!(
        out.contains("4 sub-agents: 1 running · 2 completed (1 returned without a stop record) · 0 other · 1 STRANDED"),
        "{out}"
    );
    // The lost-notification lane: `returned`, its report on the ↩ line, counted as done, and
    // explained in the footer.
    assert!(lane_row(&out, "ddd4").contains("returned"), "{out}");
    assert!(out.contains("↩ All 12 tests pass; nothing left to do."), "{out}");
    assert!(out.contains("the harness lost the notification"), "{out}");
    // Launch order, one row per lane, the telling columns present.
    let a = out.find("aaa1").unwrap();
    let b = out.find("bbb2").unwrap();
    let c = out.find("ccc3").unwrap();
    assert!(a < b && b < c, "launch order:\n{out}");
    assert!(out.contains("opus-4-1"), "model short form:\n{out}");
    assert!(
        out.contains("↩ The branch is pushed and the tests pass."),
        "last line of a finished lane's return:\n{out}"
    );
    assert!(
        out.contains("↪ Read · /work/proj/src/lib.rs"),
        "last tool of a running lane:\n{out}"
    );
    assert!(
        out.contains("STRANDED") && out.contains("→ resume: SendMessage to bbb2"),
        "{out}"
    );

    let (out, _) = w.cv_ok(&["lanes", SID, "--stranded"]);
    assert!(
        out.contains("bbb2") && !out.contains("aaa1") && !out.contains("ccc3"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["lanes", SID, "--running"]);
    assert!(out.contains("ccc3") && !out.contains("aaa1"), "{out}");
    assert!(!out.contains("ddd4"), "a returned lane is not running:\n{out}");
    let (out, _) = w.cv_ok(&["lanes", SID, "--done"]);
    assert!(
        out.contains("aaa1") && out.contains("ddd4") && !out.contains("ccc3") && !out.contains("bbb2"),
        "{out}"
    );
    // --since: a window on activity (the fixture is long past; a huge window keeps it, a tiny
    // one empties it).
    let (out, _) = w.cv_ok(&["lanes", SID, "--since", "9000h"]);
    assert!(out.contains("4 sub-agents") && out.contains("active since"), "{out}");
    let (out, _) = w.cv_ok(&["lanes", SID, "--since", "1s", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), json!([]));
    let (ok, _, _, err) = w.cv(&["lanes", SID, "--since", "soon"]);
    assert!(!ok && err.contains("--since takes a duration"), "{err}");

    let (json, _) = w.cv_ok(&["lanes", SID, "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 4);
    let d = rows.iter().find(|r| r["agent_id"] == "ddd4").unwrap();
    assert_eq!(d["status"], "returned");
    assert_eq!(d["status_source"], "transcript");
    assert_eq!(d["stranded"], false);
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();
    let a = by_id("aaa1");
    // The last assistant turn is 10:04:50; the 10:04:55 stop hook is bookkeeping.
    assert_eq!(a["duration_ms"], 280_000, "duration from first to last turn");
    assert_eq!(a["status"], "completed");
    assert_eq!(a["status_source"], "task_notification");
    assert_eq!(a["stranded"], false);
    // Two usage-bearing assistant rows with distinct message ids: 100+10 input, 50+5 output,
    // 1000 cache read → total 1165; only Bash is a tool call, while the text turn contributes usage.
    assert_eq!(a["tokens"]["total"], 1165, "{a}");
    assert_eq!(a["tool_calls"], 1);
    let b = by_id("bbb2");
    assert_eq!(
        b["status"], "completed",
        "the harness said completed; the text says waiting"
    );
    assert_eq!(b["stranded"], true);
    assert!(b["last_text"].as_str().unwrap().ends_with("Waiting on notifications."));
    let c = by_id("ccc3");
    assert_eq!(c["status"], "running");
    assert_eq!(c["status_source"], "transcript");
    assert_eq!(c["last_tool"], "Read · /work/proj/src/lib.rs");
}

/// `cv lanes --tasks` joins each lane to the task store: lane A by the `CV_ENDPOINT` its own tool
/// call exported (exact), lane B by its description's leading token (`LANE-B:` → `lane:lane-b`),
/// lane C holds nothing. Each task shows its short id, state, title and last note's first line;
/// `--json` carries `endpoint`, `endpoint_source` and `tasks`.
#[test]
fn lanes_tasks_join_by_exported_endpoint_then_description() {
    let w = build_world("lanes-tasks");
    let open = |title: &str, who: &str| -> String {
        w.cv_ok(&["task", "open", title, "--assignee", who]);
        let (out, _) = w.cv_ok(&["task", "list", "--json"]);
        let tasks: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        let matches: Vec<_> = tasks.iter().filter(|task| task["title"] == title).collect();
        assert_eq!(matches.len(), 1, "one newly opened task with title {title:?}");
        // Both `task list --json` (TaskRow) and the lane's joined task row use `id`.
        matches[0]["id"].as_str().expect("task id").to_owned()
    };
    let a = open("A: the first lane's task", "lane:lane-a");
    w.cv_ok(&[
        "task",
        "note",
        &a,
        "halfway: the parser is done\nsecond line",
        "--from",
        "lane:lane-a",
    ]);
    open("B: the stranded lane's task", "lane:lane-b");
    open("someone else's", "lane:zzz");

    let (out, _) = w.cv_ok(&["lanes", SID, "--tasks"]);
    assert!(out.contains("⚑ lane:lane-a: 1 task(s)"), "{out}");
    assert!(
        out.contains("[open] A: the first lane's task · halfway: the parser is done"),
        "{out}"
    );
    assert!(!out.contains("second line"), "only the last note's first line:\n{out}");
    assert!(out.contains("⚑ lane:lane-b (by description): 1 task(s)"), "{out}");
    assert!(!out.contains("someone else's"), "{out}");
    assert!(
        out.contains("⚑ no endpoint"),
        "lane C exported nothing and matches nothing:\n{out}"
    );
    let (plain, _) = w.cv_ok(&["lanes", SID]);
    assert!(!plain.contains('⚑'), "no join without --tasks:\n{plain}");

    let (json, _) = w.cv_ok(&["lanes", SID, "--tasks", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();
    let la = by_id("aaa1");
    assert_eq!(la["endpoint"], "lane:lane-a");
    assert_eq!(la["endpoint_source"], "transcript");
    assert_eq!(la["tasks"][0]["id"], a.as_str());
    assert_eq!(la["tasks"][0]["last_note"], "halfway: the parser is done");
    let lb = by_id("bbb2");
    assert_eq!(lb["endpoint_source"], "description");
    assert_eq!(lb["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(by_id("ccc3")["tasks"], serde_json::json!([]));
    // Without --tasks the transcript endpoint is still reported; no tasks key.
    let (json, _) = w.cv_ok(&["lanes", SID, "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let la = rows.iter().find(|r| r["agent_id"] == "aaa1").unwrap();
    assert_eq!(la["endpoint"], "lane:lane-a");
    assert!(la.get("tasks").is_none(), "{la}");
}

/// A Claude Code API-error notice, in the record shape session 0c315aee's kills left (content
/// redacted to the notice text; `quotaLimits` carried only on the rate limit).
fn api_error(
    uuid: &str,
    ts: &str,
    text: &str,
    error: &str,
    status: Option<u64>,
    resets_at: Option<i64>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts, "isSidechain": true,
        "message": {"id": format!("e-{uuid}"), "model": "<synthetic>", "role": "assistant", "type": "message",
            "stop_reason": "stop_sequence", "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}},
        "error": error, "isApiErrorMessage": true
    });
    if let Some(s) = status {
        v["apiErrorStatus"] = s.into();
    }
    if let Some(r) = resets_at {
        v["quotaLimits"] = serde_json::json!({"status": "rejected", "resetsAt": r, "rateLimitType": "five_hour"});
    }
    v
}

/// `cv lanes` names why a lane died: the session-limit 429 is `failed:rate-limited` with its
/// reset, `Prompt is too long` is `failed:context`, a lane resumed after a limit is no longer
/// failed, and a failure with no notice stays plain `failed`. The summary counts the causes.
#[test]
fn lanes_failure_cause_from_the_transcript() {
    let w = World::new("lanes-cause");
    w.write_session(
        SID,
        &[
            user(SID, "u0", "2026-10-01T19:00:00Z", "go"),
            notification("2026-10-01T20:10:30Z", "eee5", "failed"),
            notification("2026-10-01T19:30:30Z", "fff6", "failed"),
            notification("2026-10-01T20:10:31Z", "ggg7", "failed"),
            notification("2026-10-01T21:30:00Z", "ggg7", "completed"),
            notification("2026-10-01T20:00:00Z", "hhh8", "failed"),
            assistant("a0", "2026-10-01T22:00:00Z", "ok"),
        ],
    );
    let run = |id: &str, desc: &str, tail: Vec<serde_json::Value>| {
        let mut lines = vec![
            user(SID, &format!("{id}0"), "2026-10-01T19:00:10Z", "You are a lane."),
            assistant_tool(
                &format!("{id}1"),
                "2026-10-01T19:00:20Z",
                "Bash",
                serde_json::json!({"command": "cargo build"}),
            ),
            tool_result(&format!("{id}2"), "2026-10-01T19:01:00Z", &format!("toolu_{id}1"), "ok"),
        ];
        lines.extend(tail);
        write_agent(&w, SID, id, desc, &lines);
    };
    run(
        "eee5",
        "LANE-E: killed by the session limit",
        vec![api_error(
            "e3",
            "2026-10-01T20:10:20Z",
            "You've hit your session limit · resets 9pm (America/New_York)",
            "rate_limit",
            Some(429),
            Some(1_790_902_800),
        )],
    );
    run(
        "fff6",
        "LANE-F: the standing integrator",
        vec![api_error(
            "f3",
            "2026-10-01T19:30:04Z",
            "Prompt is too long",
            "invalid_request",
            None,
            None,
        )],
    );
    run(
        "ggg7",
        "LANE-G: resumed after the limit",
        vec![
            api_error(
                "g3",
                "2026-10-01T20:10:21Z",
                "You've hit your session limit · resets 9pm (America/New_York)",
                "rate_limit",
                Some(429),
                Some(1_790_902_800),
            ),
            user(
                SID,
                "g4",
                "2026-10-01T21:10:00Z",
                "The coordinator sent a message: resume.",
            ),
            assistant("g5", "2026-10-01T21:29:00Z", "Resumed and finished; report written."),
        ],
    );
    run("hhh8", "LANE-H: died quietly", vec![]);

    let (out, _) = w.cv_ok(&["lanes", SID]);
    assert!(lane_row(&out, "eee5").contains("failed:rate-limited"), "{out}");
    assert!(
        out.contains("✗ You've hit your session limit · resets 9pm (America/New_York) (resets "),
        "{out}"
    );
    assert!(
        out.contains("resume with one message after the reset: SendMessage to eee5"),
        "{out}"
    );
    assert!(lane_row(&out, "fff6").contains("failed:context"), "{out}");
    assert!(out.contains("✗ Prompt is too long → it cannot be resumed"), "{out}");
    assert!(
        lane_row(&out, "ggg7").contains("completed"),
        "a resumed lane is not dead:\n{out}"
    );
    let h = lane_row(&out, "hhh8");
    assert!(
        h.contains("failed") && !h.contains("failed:"),
        "no notice: plain failed:\n{out}"
    );
    assert!(
        out.contains("4 sub-agents: 0 running · 1 completed · 3 other (1 context · 1 rate-limited)"),
        "{out}"
    );

    let (json, _) = w.cv_ok(&["lanes", SID, "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();
    let e = by_id("eee5");
    assert_eq!(e["status"], "failed");
    assert_eq!(e["failure_cause"], "rate-limited");
    assert_eq!(e["resets_at"], "2026-10-02T01:00:00Z");
    assert_eq!(by_id("fff6")["status"], "failed");
    assert_eq!(by_id("fff6")["failure_cause"], "context");
    assert_eq!(by_id("ggg7")["status"], "completed");
    assert!(by_id("ggg7").get("failure_cause").is_none());
    assert_eq!(by_id("hhh8")["status"], "failed");
    assert!(by_id("hhh8").get("failure_cause").is_none());
}

/// A lane's endpoint comes from its OWN shell exports only. A lane that briefs a child (an `Agent`
/// prompt carrying `export CV_ENDPOINT=lane:child`) or writes that brief to a file is not the
/// child — scanning the whole tool input used to join it to the child's tasks. Its own later Bash
/// export wins; a lane with only the brief has no endpoint from the transcript.
#[test]
fn lanes_endpoint_comes_from_the_lanes_own_shell_exports_only() {
    let w = World::new("lanes-endpoint");
    w.write_session(
        SID,
        &[
            user(SID, "u0", "2026-10-01T10:00:00Z", "go"),
            notification("2026-10-01T10:30:00Z", "ppp1", "completed"),
            notification("2026-10-01T10:30:01Z", "qqq2", "completed"),
            assistant("a0", "2026-10-01T10:31:00Z", "ok"),
        ],
    );
    let brief = "You are lane CHILD. First: export CV_ENDPOINT=lane:child; cv task claim 01a0";
    write_agent(
        &w,
        SID,
        "ppp1",
        "PARENT: briefs a child, then works",
        &[
            user(SID, "p0", "2026-10-01T10:00:10Z", "You are the parent lane."),
            assistant_tool(
                "p1",
                "2026-10-01T10:00:20Z",
                "Agent",
                serde_json::json!({"description": "child lane", "prompt": brief}),
            ),
            tool_result("p2", "2026-10-01T10:01:00Z", "toolu_p1", "spawned"),
            assistant_tool(
                "p3",
                "2026-10-01T10:02:00Z",
                "Write",
                serde_json::json!({"file_path": "/tmp/brief.md", "content": brief}),
            ),
            tool_result("p4", "2026-10-01T10:02:10Z", "toolu_p3", "ok"),
            assistant_tool(
                "p5",
                "2026-10-01T10:03:00Z",
                "Bash",
                serde_json::json!({"command": "export CV_ENDPOINT=lane:parent; cv task claim 01a1"}),
            ),
            tool_result("p6", "2026-10-01T10:03:10Z", "toolu_p5", "ok"),
            assistant("p7", "2026-10-01T10:29:00Z", "Done."),
        ],
    );
    write_agent(
        &w,
        SID,
        "qqq2",
        "ONLYBRIEF: spawns and stops",
        &[
            user(SID, "q0", "2026-10-01T10:00:10Z", "You spawn one child."),
            assistant_tool(
                "q1",
                "2026-10-01T10:00:20Z",
                "Agent",
                serde_json::json!({"description": "child lane", "prompt": brief}),
            ),
            tool_result("q2", "2026-10-01T10:01:00Z", "toolu_q1", "spawned"),
            assistant("q3", "2026-10-01T10:29:00Z", "Spawned; done."),
        ],
    );
    let open = |title: &str, who: &str| {
        w.cv_ok(&["task", "open", title, "--assignee", who]);
    };
    open("the child's task", "lane:child");
    open("the parent's task", "lane:parent");

    let (json, _) = w.cv_ok(&["lanes", SID, "--tasks", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();
    let parent = by_id("ppp1");
    assert_eq!(parent["endpoint"], "lane:parent", "{parent}");
    assert_eq!(parent["endpoint_source"], "transcript");
    assert_eq!(parent["tasks"].as_array().unwrap().len(), 1, "{parent}");
    assert_eq!(parent["tasks"][0]["title"], "the parent's task");
    let only = by_id("qqq2");
    assert!(only.get("endpoint").is_none(), "a brief is not an export: {only}");
    assert_eq!(only["tasks"], serde_json::json!([]));
    let (out, _) = w.cv_ok(&["lanes", SID, "--tasks"]);
    assert!(!out.contains("lane:child"), "no lane is the child:\n{out}");
}

#[test]
fn universal_subagent_addressing_ls_prefix_and_port_json() {
    let w = build_world("universal-addr");
    // Same-second UUIDv7 sessions share their first 15 chars: `cv ls` grows the ID prefix.
    let v7_a = "019c1234-5678-7aaa-8000-000000000001";
    let v7_b = "019c1234-5678-7bbb-8000-000000000002";
    w.write_session(v7_a, &[user(v7_a, "va0", "2026-10-02T10:00:00Z", "first v7")]);
    w.write_session(v7_b, &[user(v7_b, "vb0", "2026-10-02T10:00:01Z", "second v7")]);
    let (ls_out, _) = w.cv_ok(&["ls"]);
    assert!(
        ls_out.contains("019c1234-5678-7a") && ls_out.contains("019c1234-5678-7b"),
        "same-second UUIDv7 rows disambiguate in cv ls:\n{ls_out}"
    );

    // Long sub-agent hex id renders `agent-` + 8 hex chars (never `agent-ae`), and bare
    // `<agent-id>` / `<parent>/<agent>` work across show, distill (--into), splice (--into), rewind.
    let long_agent = "ae84249a11223344";
    write_agent(
        &w,
        SID,
        long_agent,
        "LANE-LONG: sub-agent with 16-hex id",
        &[
            user(SID, "la0", "2026-10-01T10:00:10Z", "Investigate short_id."),
            assistant_tool(
                "la1",
                "2026-10-01T10:00:20Z",
                "Bash",
                serde_json::json!({"command": "cargo check"}),
            ),
            tool_result("la2", "2026-10-01T10:00:30Z", "toolu_la1", "ok"),
            assistant("la3", "2026-10-01T10:00:40Z", "Done checking."),
        ],
    );

    let (show_slash, _) = w.cv_ok(&["show", &format!("{SID}/{long_agent}"), "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&show_slash).unwrap();
    assert_eq!(parsed["id"], format!("agent-{long_agent}"));

    // Bare `<agent-id>` in distill + `--into` alias on `--agent-of`, plus `agent-ae84249a` short_id.
    let (dist_json, dist_err) = w.cv_ok(&["distill", long_agent, "--into", SID, "--keep-source", "--json"]);
    assert!(
        dist_err.contains("agent-ae84249a"),
        "short_id keeps 8 hex chars past agent-:\n{dist_err}"
    );
    let dist_val: serde_json::Value = serde_json::from_str(&dist_json).unwrap();
    assert!(dist_val["emitted"]["agent_id"].as_str().is_some(), "{dist_json}");

    // Bare `<agent-id>` in splice + `--into` alias on `--agent-of`.
    let (sp_json, _) = w.cv_ok(&["splice", &format!("{long_agent}:..2"), "--into", SID, "--json"]);
    let sp_val: serde_json::Value = serde_json::from_str(&sp_json).unwrap();
    assert!(sp_val["agent_id"].as_str().is_some(), "{sp_json}");

    // Bare `<agent-id>` in rewind.
    let (rw_json, _) = w.cv_ok(&["rewind", long_agent, "--dry-run", "--json"]);
    let rw_val: serde_json::Value = serde_json::from_str(&rw_json).unwrap();
    assert_eq!(rw_val["subagent"], true, "{rw_json}");

    // `cv port --json` emits structured JSON on stdout.
    let port_out = w.base.join("ported");
    let (port_json, _) = w.cv_ok(&["port", SID, "--out", port_out.to_str().unwrap(), "--json"]);
    let port_val: serde_json::Value = serde_json::from_str(&port_json).unwrap();
    assert_eq!(port_val["harness"], "claude", "{port_json}");
    assert!(port_val["path"].as_str().is_some(), "{port_json}");
    assert!(port_val["id"].as_str().is_some(), "{port_json}");
}

#[test]
fn task_list_json_emits_task_row_summaries_and_ls_close_aliases() {
    let w = World::new("task-row-summary");
    w.cv_ok(&[
        "task",
        "open",
        "compact task row",
        "--body",
        "huge task body that should not bloat task list --json",
        "--assignee",
        "lane:a",
        "--from",
        "orchestrator:root",
    ]);
    // `cv task ls --json` aliases `list` and emits TaskRow summaries without body/notes arrays.
    let (ls_json, _) = w.cv_ok(&["task", "ls", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&ls_json).unwrap();
    assert_eq!(rows.len(), 1, "{ls_json}");
    let id = rows[0]["id"].as_str().expect("TaskRow id").to_string();
    assert!(rows[0].get("body").is_none(), "TaskRow omits body:\n{ls_json}");
    assert!(rows[0].get("notes").is_none(), "TaskRow omits notes:\n{ls_json}");

    w.cv_ok(&["task", "note", &id, "progress update", "--from", "lane:a"]);

    // `--detail` opts into full TaskProjection JSON.
    let (detail_json, _) = w.cv_ok(&["task", "list", "--json", "--detail"]);
    let full: Vec<serde_json::Value> = serde_json::from_str(&detail_json).unwrap();
    assert_eq!(full[0]["task_id"], id);
    assert!(full[0]["body"].as_str().unwrap().contains("huge task body"));
    assert_eq!(full[0]["notes"].as_array().unwrap().len(), 1);

    // `cv task close` aliases `done`.
    w.cv_ok(&["task", "close", &id, "--observed", "verified", "--from", "lane:a"]);
    let (all_json, _) = w.cv_ok(&["task", "ls", "--all", "--json"]);
    let closed: Vec<serde_json::Value> = serde_json::from_str(&all_json).unwrap();
    assert_eq!(closed[0]["effective_state"], "done", "{all_json}");
}
