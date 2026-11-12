//! The cybernetic layer end to end: `cv hooks install/show/remove` against a fake `$HOME`'s
//! Claude Code settings (idempotent, reversible, leaves other hooks alone), and each `cv hook
//! <name>` driven with the payload shape the Claude Code 2.1.296 hooks docs give for its event.
//! Hermetic through `cv_testkit::World`: own `$HOME` + `$CLUSTERVISION_HOME`, the real binary.

use cv_testkit::claude::{self, compact_summary, tool_use, user, with};
use cv_testkit::World;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn write_agent(w: &World, sid: &str, agent_id: &str, lines: &[serde_json::Value]) {
    let meta = json!({"agentType": "general-purpose", "description": "LANE", "toolUseId": "toolu_1"});
    w.write_agent(sid, agent_id, &meta, lines);
}

fn cv_ok(w: &World, args: &[&str]) -> String {
    w.cv_ok(args).0
}

/// `cv hook <name>` fed one payload; the hook must exit 0 whatever happens.
fn hook(w: &World, name: &str, payload: &serde_json::Value, env: &[(&str, &str)]) -> String {
    hook_full(w, &["hook", name], payload, env).0
}

/// `cv hook <name> --harness codex`, as `cv hooks install codex` writes it.
fn codex_hook(w: &World, name: &str, payload: &serde_json::Value, env: &[(&str, &str)]) -> String {
    hook_full(w, &["hook", name, "--harness", "codex"], payload, env).0
}

/// `(stdout, stderr)` of one hook call, which must exit 0.
fn hook_full(w: &World, args: &[&str], payload: &serde_json::Value, env: &[(&str, &str)]) -> (String, String) {
    let name = args[1];
    let (ok, code, out, err) = w.cv_full(args, env, Some(&payload.to_string()));
    assert!(ok, "cv hook {name} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
    (out, err)
}

fn settings_path(w: &World) -> PathBuf {
    w.home.join(".claude/settings.json")
}

fn settings(w: &World) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(settings_path(w)).unwrap()).unwrap()
}

const CWD: &str = "/work/proj";

fn assistant(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([{"type": "text", "text": text}]),
            json!({"input_tokens": 100, "output_tokens": 50}),
        ),
        json!({"cwd": CWD}),
    )
}

fn assistant_tool(uuid: &str, ts: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    with(
        claude::reply(
            uuid,
            ts,
            json!([tool_use(&format!("toolu_{uuid}"), name, input)]),
            json!({"input_tokens": 10, "output_tokens": 5}),
        ),
        json!({"cwd": CWD}),
    )
}

fn tool_result(uuid: &str, ts: &str, tool_use_id: &str, content: &str) -> serde_json::Value {
    with(claude::tool_result(uuid, ts, tool_use_id, content), json!({"cwd": CWD}))
}

fn boundary(uuid: &str, ts: &str) -> serde_json::Value {
    claude::boundary(uuid, ts, "auto", 180_412)
}

/// The common fields every event carries (the docs' "common input fields").
fn common(w: &World, sid: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": sid,
        "transcript_path": w.transcript(sid),
        "cwd": "/work/proj",
        "permission_mode": "default",
        "hook_event_name": event,
    })
}

fn context_of(out: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("hook stdout is not JSON ({e}):\n{out}"));
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no additionalContext in {out}"))
        .to_string()
}

#[test]
fn install_is_idempotent_show_reports_and_remove_restores_the_file() {
    let w = World::new("install");
    fs::create_dir_all(w.home.join(".claude")).unwrap();
    let theirs = serde_json::json!({
        "theme": "dark",
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/notify"}]}],
                  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]}
    });
    fs::write(settings_path(&w), serde_json::to_string_pretty(&theirs).unwrap()).unwrap();

    let out = cv_ok(&w, &["hooks", "install", "claude"]);
    assert!(out.contains("installed 6 entries"), "{out}");
    let once = fs::read_to_string(settings_path(&w)).unwrap();
    let s = settings(&w);
    assert_eq!(s["theme"], "dark");
    assert_eq!(
        s["hooks"]["Stop"][0]["hooks"][0]["command"], "/usr/bin/notify",
        "theirs survives"
    );
    assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "Bash", "theirs stays first");
    assert_eq!(s["hooks"]["PreToolUse"][1]["matcher"], "Edit|Write");
    let cmd = s["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        cmd.ends_with("/cv hook compact --harness claude"),
        "absolute binary path: {cmd}"
    );
    assert_eq!(s["hooks"]["SessionStart"][1]["matcher"], "startup|resume");
    assert_eq!(s["hooks"]["SubagentStart"][0]["hooks"][0]["async"], true);
    assert_eq!(s["hooks"]["SubagentStop"][0]["hooks"][0]["async"], true);
    assert!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with(" hook decisions --harness claude"));
    let skill = w.home.join(".claude/skills/cv/SKILL.md");
    assert!(
        fs::read_to_string(&skill).unwrap().starts_with("---\nname: cv\n"),
        "skill copied"
    );

    // Idempotent: the second run writes nothing and the bytes are identical.
    let out = cv_ok(&w, &["hooks", "install", "claude"]);
    assert!(out.contains("already installed"), "{out}");
    assert_eq!(fs::read_to_string(settings_path(&w)).unwrap(), once);

    let out = cv_ok(&w, &["hooks", "show", "claude", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["installed"].as_array().unwrap().len(), 6);
    assert_eq!(v["missing"].as_array().unwrap().len(), 0);
    assert_eq!(v["would_change"], false);
    assert_eq!(v["skill_present"], true);
    let out = cv_ok(&w, &["hooks", "show", "claude"]);
    assert!(
        out.contains("✓ SessionStart") && out.contains("would write nothing"),
        "{out}"
    );

    // Remove: exactly ours go; the file equals what was there before.
    let out = cv_ok(&w, &["hooks", "remove", "claude"]);
    assert!(out.contains("removed 6 entries"), "{out}");
    assert_eq!(settings(&w), theirs);
    assert!(!skill.exists(), "skill removed");
    let out = cv_ok(&w, &["hooks", "remove", "claude"]);
    assert!(out.contains("nothing written"), "{out}");

    // Project scope writes under the cwd, and unsupported harnesses error.
    let out = cv_ok(&w, &["hooks", "install", "claude", "--scope", "project"]);
    assert!(out.contains("installed"), "{out}");
    assert!(w.base.join(".claude/settings.json").exists());
    assert!(w.base.join(".claude/skills/cv/SKILL.md").exists());
    let (ok, code, _, err) = w.cv(&["hooks", "install", "gemini"]);
    assert!(!ok && code == 1, "unknown harness errors");
    assert!(err.contains("no verified hook contract"), "{err}");
}

/// `cv doctor` says whether the hooks are installed (project scope here) and the policy in force.
#[test]
fn doctor_reports_hook_install_state_and_policy() {
    let w = World::new("doctor-hooks");
    let out = cv_ok(&w, &["hooks", "install", "claude", "--scope", "project"]);
    assert!(out.contains("installed"), "{out}");
    w.write_session(
        "docsess",
        &[
            user("docsess", "u0", "2026-10-01T10:00:00Z", "hi"),
            assistant("a0", "2026-10-01T10:00:05Z", "hello"),
        ],
    );
    let (ok, _, out, err) = w.cv_env(&["doctor", "docsess"], &[("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "70")]);
    assert!(ok, "{err}");
    assert!(out.contains("Hooks:       6/6 in"), "{out}");
    assert!(out.contains("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=70"), "{out}");
    assert!(out.contains("autoCompactEnabled unset (default on)"), "{out}");
}

#[test]
fn each_hook_answers_the_docs_payload_for_its_event() {
    let w = World::new("bodies");
    // alphasess: edited a file, made a commit, has a title-worthy first prompt.
    w.write_session(
        "alphasess",
        &[
            serde_json::json!({"type": "ai-title", "aiTitle": "fix the widget gate", "sessionId": "s"}),
            user("alphasess", "u0", "2026-10-01T10:00:00Z", "fix the widget gate"),
            assistant_tool(
                "a0",
                "2026-10-01T10:00:05Z",
                "Edit",
                serde_json::json!({"file_path": "/work/proj/src/widget.rs", "old_string": "a", "new_string": "b"}),
            ),
            tool_result("r0", "2026-10-01T10:00:06Z", "toolu_a0", "edited ok"),
            assistant_tool(
                "a1",
                "2026-10-01T10:00:07Z",
                "Bash",
                serde_json::json!({"command": "git commit -am 'gate fix'"}),
            ),
            tool_result(
                "r1",
                "2026-10-01T10:00:08Z",
                "toolu_a1",
                "[main 63cef47] gate fix\n 1 file changed",
            ),
            assistant("a2", "2026-10-01T10:00:09Z", "landed."),
        ],
    );
    // hooksess: the session the hooks run in — compacted once, one lane still running.
    w.write_session(
        "hooksess",
        &[
            user("hooksess", "u0", "2026-10-01T11:00:00Z", "start the swarm"),
            assistant("a0", "2026-10-01T11:00:05Z", "starting"),
            boundary("b1", "2026-10-01T11:30:00Z"),
            compact_summary("s1", "2026-10-01T11:30:01Z", "SUMMARY: the early context"),
            user("hooksess", "u1", "2026-10-01T11:31:00Z", "carry on"),
        ],
    );
    write_agent(
        &w,
        "hooksess",
        "aaa1",
        &[
            user("hooksess", "x0", "2026-10-01T11:00:10Z", "You are lane A."),
            assistant_tool(
                "x1",
                "2026-10-01T11:00:20Z",
                "Bash",
                serde_json::json!({"command": "cargo build"}),
            ),
            tool_result("x2", "2026-10-01T11:01:00Z", "toolu_x1", "ok"),
            // Ends mid tool call: `running` until something records a stop.
            assistant_tool(
                "x3",
                "2026-10-01T11:02:00Z",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
        ],
    );
    w.write_session("elsewhere", &[
        serde_json::json!({"type": "user", "uuid": "e0", "sessionId": "s", "timestamp": "2026-10-01T12:00:00Z", "cwd": "/other",
            "message": {"role": "user", "content": "unrelated"}}),
    ]);
    // The catalog and the edit index are what the hooks read; populate them as a user would.
    cv_ok(&w, &["ls"]);
    cv_ok(&w, &["events", "alphasess"]);

    // ── compact: SessionStart with source=compact ──
    let out = hook(
        &w,
        "compact",
        &with(
            common(&w, "hooksess", "SessionStart"),
            serde_json::json!({"source": "compact"}),
        ),
        &[],
    );
    let ctx = context_of(&out);
    assert!(
        ctx.contains("context compacted (auto, 180,412 tokens) at message 2 — compaction #1"),
        "{ctx}"
    );
    assert!(ctx.contains("`cv show hooksess --pre-compaction 1`"), "{ctx}");
    assert!(ctx.contains("`cv rewind hooksess --at 1`"), "{ctx}");
    // A session whose transcript has no boundary yet: honest, still a pointer.
    let out = hook(
        &w,
        "compact",
        &with(
            common(&w, "alphasess", "SessionStart"),
            serde_json::json!({"source": "compact"}),
        ),
        &[],
    );
    assert!(context_of(&out).contains("has not recorded the boundary yet"), "{out}");

    // ── start: SessionStart with source=startup, from hooksess, in /work/proj ──
    let out = hook(
        &w,
        "start",
        &with(
            common(&w, "hooksess", "SessionStart"),
            serde_json::json!({"source": "startup"}),
        ),
        &[],
    );
    let ctx = context_of(&out);
    assert!(ctx.starts_with("cv: 1 earlier session(s) in this directory"), "{ctx}");
    assert!(
        ctx.contains("- alphases · ") && ctx.contains("fix the widget gate") && ctx.contains("last commit 63cef47"),
        "{ctx}"
    );
    assert!(!ctx.contains("hooksess"), "the running session is not listed:\n{ctx}");
    assert!(
        !ctx.contains("elsewher"),
        "another directory's session is not listed:\n{ctx}"
    );
    assert!(
        ctx.chars().count() <= 10_000,
        "the start pack fits the harness's 10,000-char hook context, by the bound on each line:\n{ctx}"
    );
    // Nothing to say, nothing printed.
    let out = hook(
        &w,
        "start",
        &with(
            common(&w, "x", "SessionStart"),
            serde_json::json!({"source": "startup", "cwd": "/nowhere"}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "");

    // ── lane: SubagentStart then SubagentStop (the docs' shapes: agent-prefixed id on start, bare on stop) ──
    let out = hook(
        &w,
        "lane",
        &with(
            common(&w, "hooksess", "SubagentStart"),
            serde_json::json!({"agent_id": "agent-aaa1", "agent_type": "general-purpose"}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "", "async hook prints nothing");
    let before = cv_ok(&w, &["lanes", "hooksess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert_eq!(v[0]["status"], "running", "{before}");
    let out = hook(
        &w,
        "lane",
        &with(
            common(&w, "hooksess", "SubagentStop"),
            serde_json::json!({
        "stop_hook_active": false, "agent_id": "aaa1", "agent_type": "general-purpose",
        "agent_transcript_path": w.proj.join("hooksess/subagents/agent-aaa1.jsonl"),
        "last_assistant_message": "Lane A is done.\nmore", "background_tasks": [], "session_crons": []}),
        ),
        &[],
    );
    assert_eq!(out.trim(), "");
    let lane_runs = cv_core::state::StateStore::at_dir(&w.cv_home)
        .list_lane_runs_for_session("hooksess")
        .unwrap();
    assert_eq!(lane_runs.len(), 1);
    assert_eq!(lane_runs[0].agent_id.as_deref(), Some("aaa1"), "agent- prefix stripped");
    assert!(
        lane_runs[0]
            .agent_transcript_path
            .as_deref()
            .unwrap()
            .ends_with("subagents/agent-aaa1.jsonl"),
        "derived on start"
    );
    assert!(lane_runs[0].exited_at.is_some());
    assert_eq!(lane_runs[0].last_text.as_deref(), Some("Lane A is done."));
    let after = cv_ok(&w, &["lanes", "hooksess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v[0]["status"], "stopped", "{after}");
    assert_eq!(v[0]["status_source"], "hook_ledger", "{after}");

    // ── decisions: UserPromptSubmit — silent with nothing owed, one line otherwise ──
    let prompt = with(
        common(&w, "hooksess", "UserPromptSubmit"),
        serde_json::json!({"prompt": "hi"}),
    );
    assert_eq!(hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:me")]).trim(), "");
    cv_ok(
        &w,
        &[
            "task",
            "decide",
            "which door",
            "--for",
            "lane:me",
            "--default",
            "left",
            "--option",
            "right",
        ],
    );
    let out = hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:me")]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(
        context_of(&out).contains("1 decision(s) owed to lane:me — `cv task inbox`"),
        "{out}"
    );
    assert_eq!(
        hook(&w, "decisions", &prompt, &[("CV_ENDPOINT", "lane:other")]).trim(),
        "",
        "not owed to this endpoint"
    );

    // ── edit: PreToolUse Edit on a file a live session (alphasess, written seconds ago) edited ──
    let edit = |sid: &str, file: &str| {
        with(
            common(&w, sid, "PreToolUse"),
            serde_json::json!({
            "tool_name": "Edit", "tool_use_id": "toolu_01ABC123",
            "tool_input": {"file_path": file, "old_string": "a", "new_string": "b"}}),
        )
    };
    let out = hook(&w, "edit", &edit("hooksess", "/work/proj/src/widget.rs"), &[]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("edited by live session alphases (0 min ago)"),
        "{reason}"
    );
    assert!(reason.contains("`cv show alphases --last 10`"), "{reason}");
    // A relative path resolves against the hook's cwd.
    let out = hook(&w, "edit", &edit("hooksess", "src/widget.rs"), &[]);
    assert!(out.contains("\"ask\""), "{out}");
    // The editing session itself, and a file nobody edited: allow by silence.
    assert_eq!(
        hook(&w, "edit", &edit("alphasess", "/work/proj/src/widget.rs"), &[]).trim(),
        ""
    );
    assert_eq!(
        hook(&w, "edit", &edit("hooksess", "/work/proj/src/other.rs"), &[]).trim(),
        ""
    );

    // Codex rejects `ask`; a real conflict must use its supported blocking decision.
    let codex_patch = with(
        common(&w, "codex-current", "PreToolUse"),
        json!({
            "tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch\n*** Add File: src/untracked.rs\n+new\n*** Update File: src/widget.rs\n@@\n-a\n+b\n*** End Patch"}
        }),
    );
    let out = codex_hook(&w, "edit", &codex_patch, &[]);
    let output: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(output["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("widget.rs"));

    // Garbage on stdin never fails the harness.
    let (ok, _, out, err) = w.cv_stdin(&["hook", "edit"], "not json");
    assert!(ok && out.is_empty(), "{out}");
    assert!(err.contains("not the hook JSON"), "{err}");
}

#[test]
fn codex_install_scopes_preserve_unrelated_hooks_and_are_reversible() {
    let w = World::new("codex-hooks-install");
    for (scope, root) in [("user", w.home.clone()), ("project", w.base.clone())] {
        let path = root.join(".codex/hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let theirs = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify.sh"}]}]}});
        fs::write(&path, theirs.to_string()).unwrap();
        cv_ok(&w, &["hooks", "install", "codex", "--scope", scope]);
        let once = fs::read(&path).unwrap();
        let out = cv_ok(&w, &["hooks", "install", "codex", "--scope", scope]);
        assert!(out.contains("nothing written"), "{out}");
        assert_eq!(fs::read(&path).unwrap(), once);
        let shown: serde_json::Value =
            serde_json::from_str(&cv_ok(&w, &["hooks", "show", "codex", "--scope", scope, "--json"])).unwrap();
        assert_eq!(shown["installed"].as_array().unwrap().len(), 6);
        assert_eq!(shown["would_change"], false);
        assert!(shown["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["event"] == "PreToolUse" && row["matcher"] == "apply_patch"));
        assert!(shown["installed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["command"].as_str().unwrap().ends_with(" --harness codex")));
        cv_ok(&w, &["hooks", "remove", "codex", "--scope", scope]);
        let back: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(back, theirs);
        assert!(!root.join(".codex/skills/cv/SKILL.md").exists());
    }
}

#[test]
fn codex_real_hook_payloads_compose_with_the_recorded_rollouts() {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy(&path, &target);
            } else {
                fs::copy(&path, &target).unwrap();
                // The edit guard counts another session as live only if its transcript changed in
                // the last 30 minutes. On macOS `fs::copy` clones and keeps the source's mtime,
                // so the copied rollouts are "live" by the checkout's age unless we say so.
                fs::File::options()
                    .write(true)
                    .open(&target)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()))
                    .unwrap();
            }
        }
    }
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cv-core/tests/corpus/codex/0.163.0");
    let w = World::new("codex-real-hooks");
    copy(&corpus.join(".codex"), &w.home.join(".codex"));
    cv_ok(&w, &["index"]);
    cv_ok(
        &w,
        &[
            "task",
            "decide",
            "fixture choice",
            "--for",
            "corpus:hooks",
            "--default",
            "left",
            "--option",
            "right",
        ],
    );
    let mut observed = [false; 5];
    let mut parent = None;
    for line in fs::read_to_string(corpus.join("hooks.jsonl")).unwrap().lines() {
        let capture: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut input = capture["input"].clone();
        for field in ["transcript_path", "agent_transcript_path"] {
            if let Some(path) = input[field]
                .as_str()
                .and_then(|p| p.strip_prefix("/home/corpus/.codex/"))
            {
                input[field] = json!(w.home.join(".codex").join(path));
            }
        }
        let (name, index) = match input["hook_event_name"].as_str().unwrap() {
            "SessionStart" if input["source"] == "compact" => ("compact", 0),
            "SessionStart" => ("start", 1),
            "SubagentStart" | "SubagentStop" => ("lane", 2),
            "UserPromptSubmit" => ("decisions", 3),
            "PreToolUse" => ("edit", 4),
            other => panic!("unexpected recorded hook {other}"),
        };
        let output = codex_hook(&w, name, &input, &[("CV_ENDPOINT", "corpus:hooks")]);
        match name {
            "compact" => assert!(context_of(&output).contains("compaction #1")),
            "start" => assert!(context_of(&output).contains("earlier session(s)")),
            "decisions" => assert!(context_of(&output).contains("1 decision(s) owed")),
            "edit" => assert_eq!(
                serde_json::from_str::<serde_json::Value>(&output).unwrap()["hookSpecificOutput"]["permissionDecision"],
                "deny"
            ),
            "lane" => {
                assert!(output.is_empty());
                if input["hook_event_name"] == "SubagentStop" {
                    parent = Some(input["transcript_path"].as_str().unwrap().to_string());
                }
            }
            _ => unreachable!(),
        }
        observed[index] = true;
    }
    assert!(observed.into_iter().all(|seen| seen));
    let parent_tp = parent.expect("SubagentStop recorded");
    let state_store = cv_core::state::StateStore::at_dir(&w.cv_home);
    let stopped_run = {
        let conn = rusqlite::Connection::open(state_store.path()).unwrap();
        let sid: String = conn
            .query_row(
                "SELECT session_id FROM lane_runs WHERE exited_at IS NOT NULL LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        state_store
            .list_lane_runs_for_session(&sid)
            .unwrap()
            .into_iter()
            .find(|r| r.exited_at.is_some())
            .unwrap()
    };
    assert_eq!(stopped_run.transcript_path.as_deref(), Some(parent_tp.as_str()));
    let selector = format!("codex:{}", stopped_run.session_id);
    let lanes: Vec<serde_json::Value> = serde_json::from_str(&cv_ok(&w, &["lanes", &selector, "--json"])).unwrap();
    assert!(lanes.iter().any(|lane| {
        lane["agent_id"].as_str() == stopped_run.agent_id.as_deref()
            && lane["status"] == "stopped"
            && lane["status_source"] == "hook_ledger"
    }));
}

/// Codex's `[hooks.state]` in `config.toml`, as its startup hooks review writes it.
fn write_codex_trust(w: &World, hashes: &[(String, String)]) {
    let mut text = String::from("[hooks.state]\n");
    for (key, hash) in hashes {
        text.push_str(&format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\n"));
    }
    fs::write(w.home.join(".codex/config.toml"), text).unwrap();
}

fn trust_rows(w: &World, scope: &str) -> Vec<(String, String, String)> {
    let shown: serde_json::Value =
        serde_json::from_str(&cv_ok(w, &["hooks", "show", "codex", "--scope", scope, "--json"])).unwrap();
    shown["installed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["trust_key"].as_str().unwrap().to_string(),
                row["current_hash"].as_str().unwrap().to_string(),
                row["trust"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn codex_trust_is_read_from_the_store_codex_writes() {
    let w = World::new("codex-trust");
    // doctor wants a session to report on; its hooks lines do not depend on it.
    w.write_session(
        "docsess",
        &[
            user("docsess", "u0", "2026-10-01T10:00:00Z", "hi"),
            assistant("a0", "2026-10-01T10:00:05Z", "hello"),
        ],
    );
    // A fresh install is untrusted, and install says so with the two ways out.
    let out = cv_ok(&w, &["hooks", "install", "codex"]);
    assert!(out.contains("Codex trust: 6 untrusted"), "{out}");
    assert!(out.contains("startup hooks review"), "{out}");
    assert!(out.contains("--dangerously-bypass-hook-trust"), "{out}");
    let rows = trust_rows(&w, "user");
    assert_eq!(rows.len(), 6);
    // Codex keys by the canonical path (`/var/…` is `/private/var/…` on macOS).
    let hooks_json = fs::canonicalize(w.home.join(".codex/hooks.json"))
        .unwrap()
        .display()
        .to_string();
    assert!(rows.iter().all(|(_, _, t)| t == "untrusted"), "{rows:?}");
    assert!(
        rows.iter()
            .any(|(k, _, _)| *k == format!("{hooks_json}:pre_tool_use:0:0")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(k, _, _)| *k == format!("{hooks_json}:session_start:1:0")),
        "{rows:?}"
    );
    assert!(
        rows.iter().all(|(_, h, _)| h.starts_with("sha256:") && h.len() == 71),
        "{rows:?}"
    );
    let (ok, _, out, _) = w.cv(&["doctor", "docsess"]);
    assert!(ok);
    assert!(
        out.contains("Codex hooks: 6/6 in ~/.codex/hooks.json (User), 6 untrusted"),
        "{out}"
    );
    assert!(out.contains("Hooks:       not installed"), "{out}");

    // A settings file that is not JSON is unreadable, with its path, not "not installed".
    let project = w.base.join(".codex/hooks.json");
    fs::create_dir_all(project.parent().unwrap()).unwrap();
    fs::write(&project, "{ not json").unwrap();
    let (_, _, out, _) = w.cv(&["doctor", "docsess"]);
    assert!(
        out.contains("6 untrusted") && out.contains("/.codex/hooks.json (Project) unreadable (key must be a string"),
        "{out}"
    );
    let (_, _, out, _) = w.cv(&["doctor", "docsess", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let broken = &doc["hooks"]["codex"][1];
    assert!(
        broken["entries"].is_null() && broken["error"].as_str().unwrap().starts_with("key must be a string"),
        "{broken}"
    );
    fs::remove_file(&project).unwrap();

    // Trust recorded for every key: trusted; a reinstall writes nothing and says so.
    write_codex_trust(
        &w,
        &rows.iter().map(|(k, h, _)| (k.clone(), h.clone())).collect::<Vec<_>>(),
    );
    assert!(trust_rows(&w, "user").iter().all(|(_, _, t)| t == "trusted"));
    let out = cv_ok(&w, &["hooks", "install", "codex"]);
    assert!(
        out.contains("nothing written") && out.contains("Codex trust: trusted"),
        "{out}"
    );
    let (_, _, out, _) = w.cv(&["doctor", "docsess"]);
    assert!(
        out.contains("Codex hooks: 6/6 in ~/.codex/hooks.json (User), trusted"),
        "{out}"
    );
    let doc: serde_json::Value = serde_json::from_str(&cv_ok(&w, &["doctor", "docsess", "--json"])).unwrap();
    assert_eq!(doc["hooks"]["codex"][0]["trust"].as_array().unwrap().len(), 6);
    assert!(doc["hooks"]["codex"][0]["trust"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["trust"] == "trusted"));

    // One entry trusted at another hash (a moved binary, an edited matcher): modified, skipped again.
    write_codex_trust(
        &w,
        &rows
            .iter()
            .map(|(k, h, _)| {
                let h = if k.ends_with(":pre_tool_use:0:0") {
                    "sha256:stale".to_string()
                } else {
                    h.clone()
                };
                (k.clone(), h)
            })
            .collect::<Vec<_>>(),
    );
    let rows = trust_rows(&w, "user");
    assert_eq!(rows.iter().filter(|(_, _, t)| t == "modified").count(), 1, "{rows:?}");
    assert_eq!(rows.iter().filter(|(_, _, t)| t == "trusted").count(), 5, "{rows:?}");
    let (_, _, out, _) = w.cv(&["doctor", "docsess"]);
    assert!(
        out.contains("(User), 1 modified since trusted — Codex skips them"),
        "{out}"
    );
    let out = cv_ok(&w, &["hooks", "show", "codex"]);
    assert!(
        out.contains("hook edit --harness codex · modified") && out.contains("hook start --harness codex · trusted"),
        "{out}"
    );
    assert!(out.contains("Codex trust: 1 modified since trusted"), "{out}");

    // Project scope: its own keys under ./.codex/hooks.json, and the extra project-trust step.
    let out = cv_ok(&w, &["hooks", "install", "codex", "--scope", "project"]);
    assert!(out.contains("6 untrusted") && out.contains("trusted project"), "{out}");
    let project = fs::canonicalize(w.base.join(".codex/hooks.json"))
        .unwrap()
        .display()
        .to_string();
    assert!(trust_rows(&w, "project")
        .iter()
        .all(|(k, _, t)| k.starts_with(&project) && t == "untrusted"));
}

/// The real-binary gate for trust: with `CV_CODEX_BIN=/path/to/codex`, ask that Codex's
/// `app-server` for `hooks/list` over the file cv wrote and compare keys, hashes and states.
/// Without the variable the test passes without running; CI sets it to a pinned release.
#[test]
fn codex_trust_matches_the_real_binary() {
    let Some(codex) = std::env::var_os("CV_CODEX_BIN") else {
        eprintln!("CV_CODEX_BIN unset: skipping the real-binary trust check");
        return;
    };
    use std::io::Write;
    use std::process::{Command, Stdio};
    let w = World::new("codex-trust-real");
    cv_ok(&w, &["hooks", "install", "codex"]);
    let rows = trust_rows(&w, "user");
    // Trust five as Codex would, leave one at a stale hash: expect trusted ×5, modified ×1.
    write_codex_trust(
        &w,
        &rows
            .iter()
            .map(|(k, h, _)| {
                let h = if k.ends_with(":user_prompt_submit:0:0") {
                    "sha256:stale".to_string()
                } else {
                    h.clone()
                };
                (k.clone(), h)
            })
            .collect::<Vec<_>>(),
    );
    let mut child = Command::new(&codex)
        .arg("app-server")
        .env("CODEX_HOME", w.home.join(".codex"))
        .env("HOME", &w.home)
        .current_dir(&w.base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("codex app-server");
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{{"name":"cv-test","version":"0"}}}}}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"initialized"}}"#).unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"hooks/list","params":{{"cwds":[{}]}}}}"#,
        serde_json::to_string(&w.base).unwrap()
    )
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reported = std::collections::HashMap::new();
    for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
        let msg: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        if msg["id"] != 2 {
            continue;
        }
        for entry in msg["result"]["data"].as_array().expect("hooks/list result") {
            for hook in entry["hooks"].as_array().unwrap() {
                reported.insert(
                    hook["key"].as_str().unwrap().to_string(),
                    (
                        hook["currentHash"].as_str().unwrap().to_string(),
                        hook["trustStatus"].as_str().unwrap().to_string(),
                    ),
                );
            }
        }
        break;
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let rows = trust_rows(&w, "user");
    assert_eq!(rows.len(), 6);
    for (key, hash, trust) in &rows {
        let (their_hash, their_trust) = reported.get(key).unwrap_or_else(|| panic!("codex did not list {key}"));
        assert_eq!(their_hash, hash, "{key}");
        assert_eq!(their_trust, trust, "{key}");
    }
    assert_eq!(rows.iter().filter(|(_, _, t)| t == "modified").count(), 1);
}

/// The live effect, with no model call: `codex exec` runs SessionStart and UserPromptSubmit hooks
/// before it asks the model anything, and reports each as `hook: <Event> Completed`. Untrusted
/// (a fresh install) Codex skips every one; `--dangerously-bypass-hook-trust` runs them. The model
/// request itself fails unauthenticated, which is why the run is read until both lines appear (or
/// a deadline) and then killed. Opt-in on `CV_CODEX_BIN`, like the trust check.
#[test]
fn codex_hooks_run_in_the_real_binary_only_with_trust() {
    let Some(codex) = std::env::var_os("CV_CODEX_BIN") else {
        eprintln!("CV_CODEX_BIN unset: skipping the real-binary hook run");
        return;
    };
    use std::process::{Command, Stdio};
    let w = World::new("codex-hooks-real");
    cv_ok(&w, &["hooks", "install", "codex"]);
    let run = |bypass: bool| -> String {
        let log = w.base.join(if bypass {
            "exec-bypass.log"
        } else {
            "exec-untrusted.log"
        });
        let out = fs::File::create(&log).unwrap();
        let mut cmd = Command::new(&codex);
        cmd.args(["exec", "--skip-git-repo-check"]);
        if bypass {
            cmd.arg("--dangerously-bypass-hook-trust");
        }
        let mut child = cmd
            .arg("hello")
            .env("CODEX_HOME", w.home.join(".codex"))
            .env("HOME", &w.home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .current_dir(&w.base)
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .expect("codex exec");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let text = fs::read_to_string(&log).unwrap_or_default();
            let hooked =
                text.contains("hook: SessionStart Completed") && text.contains("hook: UserPromptSubmit Completed");
            // Untrusted: Codex reaches the model request (and its retries) without a hook line.
            let reached_model = text.contains("Reconnecting... 2/");
            let exited = child.try_wait().unwrap().is_some();
            if (bypass && hooked) || (!bypass && reached_model) || exited || std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return fs::read_to_string(&log).unwrap_or_default();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };
    let untrusted = run(false);
    assert!(!untrusted.contains("hook: "), "untrusted hooks ran:\n{untrusted}");
    let bypassed = run(true);
    assert!(
        bypassed.contains("hook: SessionStart Completed") && bypassed.contains("hook: UserPromptSubmit Completed"),
        "hooks did not complete under --dangerously-bypass-hook-trust:\n{bypassed}"
    );
    assert!(
        !bypassed.contains("Failed") && !bypassed.contains("Blocked"),
        "{bypassed}"
    );
}

/// A Codex rollout under the world's `~/.codex/sessions`: a `session_meta` header (with
/// `parent_thread_id` for a child thread) and one `apply_patch` of `file`, in the record shapes
/// Codex 0.163.0 writes (`tests/fixtures/codex/direct_patch.jsonl`).
fn codex_rollout(w: &World, id: &str, parent: Option<&str>, file: &str) -> PathBuf {
    let dir = w.home.join(".codex/sessions/2026/10/19");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-10-19T10-00-00-{id}.jsonl"));
    let mut meta = json!({"id": id, "session_id": parent.unwrap_or(id), "timestamp": "2026-10-19T10:00:00.000Z",
        "cwd": "/work/proj", "originator": "codex_exec", "cli_version": "0.163.0", "source": "exec"});
    if let Some(parent) = parent {
        meta["parent_thread_id"] = json!(parent);
    }
    let call = format!("call_{id}");
    let patch = format!("*** Begin Patch\n*** Update File: {file}\n@@\n-a\n+b\n*** End Patch");
    let lines = [
        json!({"timestamp": "2026-10-19T10:00:00.000Z", "ordinal": 0, "type": "session_meta", "payload": meta}),
        json!({"timestamp": "2026-10-19T10:00:01.000Z", "ordinal": 1, "type": "response_item", "payload": {
            "type": "function_call", "id": format!("fc_{id}"), "name": "exec_command", "call_id": call,
            "arguments": json!({"cmd": format!("apply_patch <<'PATCH'\n{patch}\nPATCH"), "workdir": "/work/proj"}).to_string()}}),
        json!({"timestamp": "2026-10-19T10:00:02.000Z", "ordinal": 2, "type": "event_msg", "payload": {
            "type": "item_completed", "thread_id": id, "item": {"type": "FileChange", "id": call, "status": "completed",
            "changes": {format!("/work/proj/{file}"): {"type": "update", "unified_diff": "@@ -1 +1 @@\n-a\n+b\n", "move_path": null}}}}}),
        json!({"timestamp": "2026-10-19T10:00:03.000Z", "ordinal": 3, "type": "response_item", "payload": {
            "type": "function_call_output", "id": format!("fco_{id}"), "call_id": call, "output": "Success. Updated the following files:\nM shared.txt\n"}}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn codex_edit_guard_exempts_the_root_threads_lineage_and_denies_strangers() {
    let w = World::new("codex-lineage");
    let parent = "01a14d90-0000-7000-8000-000000000001";
    let child = "01a14d90-0000-7000-8000-000000000002";
    let stranger = "01a14d90-0000-7000-8000-000000000003";
    let parent_path = codex_rollout(&w, parent, None, "shared.txt");
    let child_path = codex_rollout(&w, child, Some(parent), "shared.txt");
    let stranger_path = codex_rollout(&w, stranger, None, "other.txt");
    cv_ok(&w, &["index"]);
    let patch = |file: &str| format!("*** Begin Patch\n*** Update File: {file}\n@@\n-b\n+c\n*** End Patch");
    let input = |sid: &str, path: &PathBuf, file: &str| {
        json!({"session_id": sid, "transcript_path": path, "cwd": "/work/proj", "hook_event_name": "PreToolUse",
            "permission_mode": "bypassPermissions", "tool_name": "apply_patch", "tool_input": {"command": patch(file)}})
    };
    // All three rollouts were just written: every editor is live. The child edited shared.txt
    // moments ago, yet the parent patching it is not a conflict, and neither is the child patching
    // what its root edited.
    assert_eq!(
        codex_hook(&w, "edit", &input(parent, &parent_path, "shared.txt"), &[]),
        ""
    );
    assert_eq!(
        codex_hook(&w, "edit", &input(child, &child_path, "shared.txt"), &[]),
        ""
    );
    // An unrelated root thread is denied, as before.
    let out = codex_hook(&w, "edit", &input(parent, &parent_path, "other.txt"), &[]);
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains(&format!("codex:{stranger}")));
    let out = codex_hook(&w, "edit", &input(stranger, &stranger_path, "shared.txt"), &[]);
    assert!(out.contains("\"deny\""), "{out}");
    // A child the catalog has not indexed yet is still placed by the lane ledger's start row.
    let late = "01a14d90-0000-7000-8000-000000000004";
    let late_path = codex_rollout(&w, late, Some(parent), "shared.txt");
    codex_hook(
        &w,
        "lane",
        &json!({"session_id": parent, "transcript_path": parent_path, "hook_event_name": "SubagentStart",
            "agent_id": late, "agent_type": "default", "cwd": "/work/proj"}),
        &[],
    );
    assert_eq!(codex_hook(&w, "edit", &input(late, &late_path, "shared.txt"), &[]), "");
}

/// `$CV_HOOK_HARNESS` (0.17's Codex entries) was removed: it exits 2 with the reinstall pointer.
#[test]
fn removed_hook_harness_variable_exits_two_with_a_pointer() {
    let w = World::new("codex-hook-harness-env");
    let (ok, code, _, err) = w.cv_full(&["hook", "edit"], &[("CV_HOOK_HARNESS", "codex")], Some("{}"));
    assert!(!ok && code == 2, "{err}");
    assert!(
        err.contains("CV_HOOK_HARNESS") && err.contains("cv hooks install codex"),
        "{err}"
    );
}

/// Past the hooks' 8 MiB read budget the compact hook reads the transcript's tail, and its text
/// says how much of how large a transcript it read, and that the index is left to `cv show`.
#[test]
fn the_compact_hook_names_what_its_budget_left_unread() {
    let w = World::new("compact-budget");
    let big = "word ".repeat(2 * 1024 * 1024);
    w.write_session(
        "bigsess",
        &[
            user("bigsess", "u0", "2026-10-01T10:00:00Z", &big),
            boundary("b0", "2026-10-01T10:01:00Z"),
        ],
    );
    let out = hook(
        &w,
        "compact",
        &with(common(&w, "bigsess", "SessionStart"), json!({"source": "compact"})),
        &[],
    );
    let ctx = context_of(&out);
    assert!(
        ctx.contains("context compacted (auto, 180,412 tokens; the hook read the last 8.0 MB of this 10.0 MB transcript, so the index is not given)"),
        "{ctx}"
    );
    assert!(ctx.contains("--kind compaction_boundary,compaction_summary"), "{ctx}");
}
