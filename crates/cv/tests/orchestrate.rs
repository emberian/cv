//! End-to-end tests for the orchestrator's instruments: `cv prompts`, `cv lanes`, `cv deferrals`.
//! Each builds a hermetic Claude session under a temp `$HOME` — a parent transcript with human
//! prompts, slash-command bookkeeping rows, an `AskUserQuestion` answer, a compaction, deferral
//! phrases in the replies and the `queue-operation` notifications the harness writes when a child
//! stops — plus a `subagents/` dir with one completed, one stranded and one running lane — and
//! drives the real binary. Mirrors `forest.rs`'s World (own `$HOME` + `$CLUSTERVISION_HOME`).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
    proj: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cv-orch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        World {
            base,
            home,
            cv_home,
            proj,
        }
    }

    fn write_session(&self, sid: &str, lines: &[serde_json::Value]) {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(self.proj.join(format!("{sid}.jsonl")), body).unwrap();
    }

    /// A directly-spawned (`Agent`-tool) sub-agent: `<sid>/subagents/agent-<id>.jsonl` + meta.
    fn write_agent(&self, sid: &str, agent_id: &str, description: &str, lines: &[serde_json::Value]) {
        let dir = self.proj.join(sid).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(dir.join(format!("agent-{agent_id}.jsonl")), body).unwrap();
        fs::write(
            dir.join(format!("agent-{agent_id}.meta.json")),
            serde_json::json!({
                "agentType": "general-purpose",
                "description": description,
                "toolUseId": format!("toolu_{agent_id}"),
                "model": "opus"
            })
            .to_string(),
        )
        .unwrap();
    }

    fn cv(&self, args: &[&str]) -> (bool, i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cv"))
            .args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLUSTERVISION_HOME", &self.cv_home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env_remove("CV_ENDPOINT")
            .output()
            .expect("cv should run");
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn cv_ok(&self, args: &[&str]) -> (String, String) {
        let (ok, code, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn user(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts, "cwd": "/work/proj",
        "message": {"role": "user", "content": text}
    })
}

fn assistant(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "message": {"role": "assistant", "model": "claude-opus-4-1-20250805", "id": format!("msg_{uuid}"),
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 1000}}
    })
}

fn assistant_tool(uuid: &str, ts: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "message": {"role": "assistant", "model": "claude-opus-4-1-20250805", "id": format!("msg_{uuid}"),
            "content": [{"type": "tool_use", "id": format!("toolu_{uuid}"), "name": name, "input": input}],
            "usage": {"input_tokens": 10, "output_tokens": 5}}
    })
}

fn tool_result(uuid: &str, ts: &str, tool_use_id: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]}
    })
}

fn subagent_stop(uuid: &str, ts: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "attachment", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "attachment": {"type": "hook_success", "hookName": "SubagentStop", "hookEvent": "SubagentStop", "content": ""}
    })
}

fn notification(ts: &str, agent_id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "queue-operation", "operation": "enqueue", "timestamp": ts, "sessionId": "s",
        "content": format!("<task-notification>\n<task-id>{agent_id}</task-id>\n<status>{status}</status>\n<summary>done</summary>\n</task-notification>")
    })
}

fn boundary(uuid: &str, ts: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "system", "subtype": "compact_boundary", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "content": "Conversation compacted",
        "compactMetadata": {"trigger": "manual", "preTokens": 900000, "durationMs": 1000}
    })
}

fn compact_summary(uuid: &str, ts: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "isCompactSummary": true, "sessionId": "s", "timestamp": ts,
        "message": {"role": "user", "content": body}
    })
}

const SID: &str = "orchsess";

/// The parent: two human prompts, a `/compact` with its bookkeeping rows, an AskUserQuestion
/// answer, two replies with deferral phrases (one matching a task, one not), three lane
/// notifications, and a human prompt after the compaction.
fn build_world(tag: &str) -> World {
    let w = World::new(tag);
    w.write_session(
        SID,
        &[
            user("u0", "2026-10-01T10:00:00Z", "start the swarm please"),
            assistant_tool("a0", "2026-10-01T10:00:05Z", "AskUserQuestion", serde_json::json!({"questions": [{"question": "Where?"}]})),
            tool_result("r0", "2026-10-01T10:01:00Z", "toolu_a0", "The user answered: \"Where?\"=\"persvati\""),
            assistant("a1", "2026-10-01T10:02:00Z", "Noted. The ResourceTargetAdmission externalKind exhaustiveness check is queued for the standing integrator right after FINAL-1 lands."),
            user("u1", "2026-10-01T10:03:00Z", "/compact"),
            user("u2", "2026-10-01T10:03:00Z", "<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"),
            user("u3", "2026-10-01T10:03:30Z", "<local-command-stdout>Compacted</local-command-stdout>"),
            boundary("b1", "2026-10-01T10:03:31Z"),
            compact_summary("s1", "2026-10-01T10:03:32Z", "SUMMARY: the early context"),
            user("u4", "2026-10-01T10:04:00Z", "hi :) we /compacted. carry on"),
            notification("2026-10-01T10:05:00Z", "aaa1", "completed"),
            notification("2026-10-01T10:06:00Z", "bbb2", "completed"),
            assistant("a2", "2026-10-01T10:07:00Z", "The hermes tariff reconciliation is an inconsistency to settle in one later lane, not tonight."),
            user("u5", "2026-10-01T10:08:00Z", "ok"),
        ],
    );
    // aaa1: finished cleanly.
    w.write_agent(
        SID,
        "aaa1",
        "LANE-A: the first lane",
        &[
            user("x0", "2026-10-01T10:00:10Z", "You are lane A."),
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
    w.write_agent(
        SID,
        "bbb2",
        "LANE-B: waits on a build",
        &[
            user("y0", "2026-10-01T10:00:30Z", "You are lane B."),
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
    w.write_agent(
        SID,
        "ccc3",
        "LANE-C: still at it",
        &[
            user("z0", "2026-10-01T10:01:00Z", "You are lane C."),
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
    w.write_agent(
        SID,
        "ddd4",
        "LANE-D: lost notification",
        &[
            user("q0", "2026-10-01T10:01:30Z", "You are lane D."),
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
fn prompts_lists_only_the_person_and_the_answers() {
    let w = build_world("prompts");
    let (out, _) = w.cv_ok(&["prompts", SID]);
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

    let (json, _) = w.cv_ok(&["prompts", SID, "--json"]);
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
    let (json, err) = w.cv_ok(&["prompts", SID, "--pre-compaction", "--json"]);
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
    assert!(out.contains("ddd4      returned"), "{out}");
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
    assert!(
        out.contains("4m45s") || out.contains("4m"),
        "duration from first to last turn:\n{out}"
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
    let (out, _) = w.cv_ok(&["lanes", SID, "--since", "1s"]);
    assert!(out.contains("no sub-agents") || out.contains("0 sub-agents"), "{out}");
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
    assert_eq!(a["status"], "completed");
    assert_eq!(a["status_source"], "task_notification");
    assert_eq!(a["stranded"], false);
    // Two usage-bearing assistant rows with distinct message ids: 100+10 input, 50+5 output,
    // 1000 cache read → total 1165; two tool calls counted? one (Bash) — the text turn has none.
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

#[test]
fn deferrals_find_the_phrases_and_open_tasks_gates_on_unmatched() {
    let w = build_world("deferrals");
    let (out, _) = w.cv_ok(&["deferrals", SID]);
    assert!(out.contains("found"), "{out}");
    for phrase in [
        "\"queued\"",
        "\"after FINAL\"",
        "\"when X lands\"",
        "\"later lane\"",
        "\"not tonight\"",
    ] {
        assert!(out.contains(phrase), "missing {phrase}:\n{out}");
    }
    assert!(out.contains("[3] ") && out.contains("[10] "), "message indices:\n{out}");
    assert!(
        !out.contains("MATCHED"),
        "no cross-reference without --open-tasks:\n{out}"
    );

    // --since skips the earlier reply entirely.
    let (out, _) = w.cv_ok(&["deferrals", SID, "--since", "10"]);
    assert!(!out.contains("after FINAL") && out.contains("later lane"), "{out}");

    // No tasks yet: everything is UNMATCHED and the exit status says so.
    let (ok, code, out, _) = w.cv(&["deferrals", SID, "--open-tasks"]);
    assert!(!ok && code == 1, "unmatched deferrals exit 1: code={code}\n{out}");
    assert!(out.contains("UNMATCHED") && !out.contains("MATCHED   "), "{out}");

    // A task sharing three significant words with the first deferral matches it; the second
    // stays unmatched, so the gate still fails.
    w.cv_ok(&[
        "task",
        "open",
        "Make ResourceTargetAdmission.externalKind exhaustive after the integrator pass",
        "--body",
        "the standing integrator owns this",
    ]);
    let (ok, code, out, _) = w.cv(&["deferrals", SID, "--open-tasks"]);
    assert!(!ok && code == 1, "{out}");
    assert!(out.contains("MATCHED   ") && out.contains("exhaustive"), "{out}");
    assert!(out.contains("(shared: "), "{out}");
    assert!(out.contains("UNMATCHED"), "the hermes one has no task:\n{out}");

    // Cover the second with a task: the gate passes.
    w.cv_ok(&[
        "task",
        "open",
        "HERMES-TARIFF: hermes tariff reconciliation inconsistency",
    ]);
    let (ok, code, out, _) = w.cv(&["deferrals", SID, "--open-tasks"]);
    assert!(ok, "every deferral has a task → exit 0: code={code}\n{out}");
    assert!(out.contains("0 UNMATCHED"), "{out}");

    let (json, _) = w.cv_ok(&["deferrals", SID, "--open-tasks", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(rows.iter().all(|r| r["matched"].is_object()), "{json}");
    // 50 chars either side of the match, the match itself, and two ellipses.
    assert!(
        rows[0]["context"].as_str().unwrap().chars().count() <= 100 + "queued".len() + 2,
        "{json}"
    );
}

/// `cv lanes --tasks` joins each lane to the task store: lane A by the `CV_ENDPOINT` its own tool
/// call exported (exact), lane B by its description's leading token (`LANE-B:` → `lane:lane-b`),
/// lane C holds nothing. Each task shows its short id, state, title and last note's first line;
/// `--json` carries `endpoint`, `endpoint_source` and `tasks`.
#[test]
fn lanes_tasks_join_by_exported_endpoint_then_description() {
    let w = build_world("lanes-tasks");
    let open = |title: &str, who: &str| -> String {
        let (out, _) = w.cv_ok(&["task", "open", title, "--assignee", who]);
        out.lines().last().unwrap().trim().to_string()
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
            user("u0", "2026-10-01T19:00:00Z", "go"),
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
            user(&format!("{id}0"), "2026-10-01T19:00:10Z", "You are a lane."),
            assistant_tool(
                &format!("{id}1"),
                "2026-10-01T19:00:20Z",
                "Bash",
                serde_json::json!({"command": "cargo build"}),
            ),
            tool_result(&format!("{id}2"), "2026-10-01T19:01:00Z", &format!("toolu_{id}1"), "ok"),
        ];
        lines.extend(tail);
        w.write_agent(SID, id, desc, &lines);
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
            user("g4", "2026-10-01T21:10:00Z", "The coordinator sent a message: resume."),
            assistant("g5", "2026-10-01T21:29:00Z", "Resumed and finished; report written."),
        ],
    );
    run("hhh8", "LANE-H: died quietly", vec![]);

    let (out, _) = w.cv_ok(&["lanes", SID]);
    assert!(out.contains("eee5      failed:rate-limited"), "{out}");
    assert!(
        out.contains("✗ You've hit your session limit · resets 9pm (America/New_York) (resets "),
        "{out}"
    );
    assert!(
        out.contains("resume with one message after the reset: SendMessage to eee5"),
        "{out}"
    );
    assert!(out.contains("fff6      failed:context"), "{out}");
    assert!(out.contains("✗ Prompt is too long → it cannot be resumed"), "{out}");
    assert!(
        out.contains("ggg7      completed"),
        "a resumed lane is not dead:\n{out}"
    );
    assert!(out.contains("hhh8      failed "), "no notice: plain failed:\n{out}");
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
    assert_eq!(by_id("fff6")["failure_cause"], "context");
    assert!(by_id("ggg7").get("failure_cause").is_none());
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
            user("u0", "2026-10-01T10:00:00Z", "go"),
            notification("2026-10-01T10:30:00Z", "ppp1", "completed"),
            notification("2026-10-01T10:30:01Z", "qqq2", "completed"),
            assistant("a0", "2026-10-01T10:31:00Z", "ok"),
        ],
    );
    let brief = "You are lane CHILD. First: export CV_ENDPOINT=lane:child; cv task claim 01a0";
    w.write_agent(
        SID,
        "ppp1",
        "PARENT: briefs a child, then works",
        &[
            user("p0", "2026-10-01T10:00:10Z", "You are the parent lane."),
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
    w.write_agent(
        SID,
        "qqq2",
        "ONLYBRIEF: spawns and stops",
        &[
            user("q0", "2026-10-01T10:00:10Z", "You spawn one child."),
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
