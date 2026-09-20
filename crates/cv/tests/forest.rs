//! End-to-end tests for the deep-Claude-harness surfaces: `cv compaction`, `cv workflow`,
//! `cv tools`, and `cv show --pre-compaction`. Each builds a hermetic on-disk Claude session
//! (transcript + the `<sid>/workflows/` state/scripts + `<sid>/subagents/` agent transcripts +
//! journal) under a temp `$HOME`, then drives the real binary and asserts on its output.
//!
//! Mirrors `cli.rs`'s hermetic World (own `$HOME` + `$CLUSTERVISION_HOME`, env-only, parallel-safe).

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
            "cv-forest-{tag}-{}-{}",
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

    /// Write the parent transcript at `<sid>.jsonl`.
    fn write_session(&self, sid: &str, lines: &[serde_json::Value]) {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(self.proj.join(format!("{sid}.jsonl")), body).unwrap();
    }

    /// The sidecar dir `<sid>/` next to the transcript.
    fn sidecar(&self, sid: &str) -> PathBuf {
        self.proj.join(sid)
    }

    /// Write a workflow *state* file `<sid>/workflows/wf_<run>.json` + its script.
    fn write_workflow(&self, sid: &str, run: &str, state: &serde_json::Value, script: &str) {
        let wdir = self.sidecar(sid).join("workflows");
        let sdir = wdir.join("scripts");
        fs::create_dir_all(&sdir).unwrap();
        fs::write(wdir.join(format!("{run}.json")), serde_json::to_string(state).unwrap()).unwrap();
        fs::write(sdir.join(format!("script-{run}.js")), script).unwrap();
    }

    /// Write a workflow *agent* transcript + its meta + a journal under
    /// `<sid>/subagents/workflows/<run>/`.
    fn write_workflow_agent(&self, sid: &str, run: &str, agent_id: &str, lines: &[serde_json::Value]) {
        let dir = self.sidecar(sid).join("subagents").join("workflows").join(run);
        fs::create_dir_all(&dir).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(dir.join(format!("agent-{agent_id}.jsonl")), body).unwrap();
        fs::write(
            dir.join(format!("agent-{agent_id}.meta.json")),
            r#"{"agentType":"workflow-subagent"}"#,
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

fn user(uuid: &str, parent: Option<&str>, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": "s",
        "timestamp": "2026-06-07T12:00:00Z", "cwd": "/work/proj",
        "message": {"role": "user", "content": text}
    })
}

fn assistant_tools(uuid: &str, ts: &str, blocks: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "message": {"role": "assistant", "model": "claude-test", "content": blocks}
    })
}

fn tool_use(name: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"type": "tool_use", "id": "t", "name": name, "input": input})
}

fn boundary(uuid: &str, parent: &str, trigger: &str, pre: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "system", "subtype": "compact_boundary", "uuid": uuid, "parentUuid": parent,
        "sessionId": "s", "timestamp": "2026-06-07T12:00:00Z",
        "content": "Conversation compacted",
        "compactMetadata": {"trigger": trigger, "preTokens": pre, "durationMs": 90000}
    })
}

fn compact_summary(uuid: &str, parent: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "isCompactSummary": true,
        "sessionId": "s", "timestamp": "2026-06-07T12:00:00Z",
        "message": {"role": "user", "content": body}
    })
}

/// A complete session with: tool calls (orchestrator), TWO compactions, and one workflow run
/// (state json + script + two agent transcripts with their own tools).
fn build_world(tag: &str) -> World {
    let w = World::new(tag);
    let sid = "fsess";

    // Parent transcript: some orchestrator tools, then two compaction cycles.
    w.write_session(
        sid,
        &[
            user("u0", None, "start the work"),
            assistant_tools(
                "a0",
                "2026-06-07T12:00:01Z",
                serde_json::json!([
                    tool_use("Bash", serde_json::json!({"command": "cargo build"})),
                    tool_use("Workflow", serde_json::json!({"script": "wf"})),
                ]),
            ),
            user("u1", Some("a0"), "more"),
            // First compaction.
            boundary("b1", "u1", "manual", 980000),
            compact_summary("s1", "b1", "SUMMARY ONE: the early context"),
            user("u2", Some("s1"), "keep going"),
            assistant_tools(
                "a1",
                "2026-06-07T12:05:00Z",
                serde_json::json!([tool_use(
                    "Edit",
                    serde_json::json!({"file_path": "x.rs", "old_string": "a", "new_string": "b"})
                )]),
            ),
            // Second compaction.
            boundary("b2", "a1", "auto", 750000),
            compact_summary("s2", "b2", "SUMMARY TWO: the middle context"),
            user("u3", Some("s2"), "finish up"),
        ],
    );

    // One workflow run: 2 phases, 2 agents.
    let run = "wf_test-0001";
    w.write_workflow(
        sid,
        run,
        &serde_json::json!({
            "runId": run,
            "workflowName": "demo-workflow",
            "status": "completed",
            "summary": "demo run did the two phases",
            "defaultModel": "claude-test",
            "agentCount": 2,
            "totalTokens": 12345,
            "totalToolCalls": 7,
            "durationMs": 600000,
            "scriptPath": "/work/proj/fsess/workflows/scripts/script-wf_test-0001.js",
            "script": "export const meta = { name: 'demo-workflow' }\n// the driving script body",
            "phases": [
                {"title": "Phase-A", "detail": "build the thing"},
                {"title": "Phase-B", "detail": "verify the thing"}
            ],
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "Phase-A"},
                {"type": "workflow_phase", "index": 2, "title": "Phase-B"},
                {"type": "workflow_agent", "index": 1, "label": "builder", "phaseIndex": 1,
                 "agentId": "aBUILD", "model": "claude-test", "state": "done",
                 "tokens": 8000, "toolCalls": 5, "durationMs": 400000,
                 "promptPreview": "build it", "resultPreview": "{\"built\":true}"},
                {"type": "workflow_agent", "index": 2, "label": "verifier", "phaseIndex": 2,
                 "agentId": "aVERIFY", "model": "claude-test", "state": "error",
                 "tokens": 4345, "toolCalls": 2, "error": "found a bug",
                 "promptPreview": "verify it", "resultPreview": "{\"ok\":false}"}
            ]
        }),
        "export const meta = { name: 'demo-workflow', phases: [] }\n// the driving script body",
    );

    // The two agents' transcripts (their tool calls become per-agent histograms).
    w.write_workflow_agent(
        sid,
        run,
        "aBUILD",
        &[
            user("wu0", None, "build it"),
            assistant_tools(
                "wa0",
                "2026-06-07T12:01:00Z",
                serde_json::json!([
                    tool_use("Bash", serde_json::json!({"command": "make"})),
                    tool_use("Read", serde_json::json!({"file_path": "src/lib.rs"})),
                    tool_use(
                        "Edit",
                        serde_json::json!({"file_path": "src/lib.rs", "old_string": "x", "new_string": "y"})
                    ),
                ]),
            ),
        ],
    );
    w.write_workflow_agent(
        sid,
        run,
        "aVERIFY",
        &[
            user("vu0", None, "verify it"),
            assistant_tools(
                "va0",
                "2026-06-07T12:02:00Z",
                serde_json::json!([tool_use("Bash", serde_json::json!({"command": "cargo test"}))]),
            ),
        ],
    );

    w
}

#[test]
fn compaction_lists_boundaries_and_summaries() {
    let w = build_world("compaction");
    let (out, _) = w.cv_ok(&["compaction", "fsess"]);
    assert!(out.contains("compacted 2 time(s)"), "got:\n{out}");
    assert!(out.contains("manual"), "trigger surfaced:\n{out}");
    assert!(out.contains("auto"), "second trigger surfaced:\n{out}");
    assert!(out.contains("SUMMARY ONE"), "first summary head:\n{out}");

    // --summaries prints full text of both.
    let (full, _) = w.cv_ok(&["compaction", "fsess", "--summaries"]);
    assert!(full.contains("SUMMARY ONE: the early context"));
    assert!(full.contains("SUMMARY TWO: the middle context"));

    // JSON carries pre_compaction_span.
    let (js, _) = w.cv_ok(&["compaction", "fsess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert!(v[0].get("pre_compaction_span").is_some(), "span in json:\n{js}");
    assert_eq!(v[0]["trigger"], "manual");
    assert_eq!(v[0]["pre_tokens"], 980000);
}

#[test]
fn show_pre_compaction_windows_the_lost_span() {
    let w = build_world("precompact");
    // The first boundary is at msg 3 (u0,a0,u1 precede it → span 0..3, end-exclusive).
    let (out, err) = w.cv_ok(&["show", "fsess", "--pre-compaction", "1"]);
    assert!(err.contains("pre-compaction #1 of 2"), "banner:\n{err}");
    assert!(
        err.contains("messages 0..3"),
        "computed span (the 0.11 `A..B` grammar):\n{err}"
    );
    assert!(!err.contains("messages 0-3"), "the old `A-B` span must be gone:\n{err}");
    // The pre-span body holds the first user turn, not the post-compaction "finish up".
    assert!(out.contains("start the work"), "pre-span body:\n{out}");
    assert!(
        !out.contains("finish up"),
        "must not leak post-compaction content:\n{out}"
    );

    // A second boundary's span exists; an out-of-range one errors cleanly.
    let (_, err2) = w.cv_ok(&["show", "fsess", "--pre-compaction", "2"]);
    assert!(err2.contains("pre-compaction #2 of 2"), "{err2}");
    let (ok, _, _, err3) = w.cv(&["show", "fsess", "--pre-compaction", "9"]);
    assert!(!ok && err3.contains("no compaction #9"), "{err3}");
}

#[test]
fn workflow_renders_phase_tree_and_lists() {
    let w = build_world("workflow");

    // List: one workflow, the right shape.
    let (list, _) = w.cv_ok(&["workflow", "fsess"]);
    assert!(list.contains("1 workflow(s)"), "got:\n{list}");
    assert!(list.contains("demo-workflow"), "{list}");
    assert!(list.contains("2 phase(s)") && list.contains("2 agent(s)"), "{list}");

    // The phase tree: phases, agents under each, outcomes, totals.
    let (tree, _) = w.cv_ok(&["workflow", "fsess", "wf_test"]);
    assert!(tree.contains("workflow demo-workflow"), "{tree}");
    assert!(tree.contains("12,345 tokens"), "grouped token total:\n{tree}");
    assert!(tree.contains("phase 1 · Phase-A"), "{tree}");
    assert!(tree.contains("build the thing"), "phase detail:\n{tree}");
    assert!(tree.contains("builder"), "agent label:\n{tree}");
    assert!(
        tree.contains("✓ builder") && tree.contains("✗ verifier"),
        "state glyphs:\n{tree}"
    );
    assert!(tree.contains("found a bug"), "agent error surfaced:\n{tree}");
    assert!(tree.contains("\"built\":true"), "result preview:\n{tree}");

    // --script prints the driving script.
    let (scr, _) = w.cv_ok(&["workflow", "fsess", "wf_test", "--script"]);
    assert!(scr.contains("the driving script body"), "script body:\n{scr}");

    // JSON round-trips the structured workflow.
    let (js, _) = w.cv_ok(&["workflow", "fsess", "wf_test", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v["name"], "demo-workflow");
    assert_eq!(v["phases"].as_array().unwrap().len(), 2);
    assert_eq!(v["phases"][0]["agents"][0]["agent_id"], "aBUILD");
}

#[test]
fn tools_cross_agent_queries() {
    let w = build_world("tools");

    // Aggregate across orchestrator + the 2 workflow agents.
    let (agg, _) = w.cv_ok(&["tools", "fsess"]);
    assert!(agg.contains("orchestrator + forest"), "{agg}");
    // Bash used by orchestrator(1) + builder(1) + verifier(1) = 3.
    assert!(agg.contains("Bash"), "{agg}");

    // Which agents used Bash: all three.
    let (which, _) = w.cv_ok(&["tools", "fsess", "--tool", "Bash"]);
    assert!(which.contains("3 agent(s) used \"Bash\""), "got:\n{which}");

    // One agent's histogram: the builder ran Bash+Read+Edit.
    let (one, _) = w.cv_ok(&["tools", "fsess", "--agent", "aBUILD"]);
    assert!(one.contains("tools used by aBUILD"), "{one}");
    assert!(
        one.contains("Bash") && one.contains("Read") && one.contains("Edit"),
        "{one}"
    );

    // Workflow-restricted per-agent breakdown (prefix resolves to the full run id).
    let (across, _) = w.cv_ok(&["tools", "fsess", "--workflow", "wf_test", "--across"]);
    assert!(across.contains("aBUILD") && across.contains("aVERIFY"), "{across}");
    assert!(
        !across.contains("orchestrator"),
        "workflow view excludes orchestrator:\n{across}"
    );

    // Timeline is chronological and tagged by agent.
    let (tl, _) = w.cv_ok(&["tools", "fsess", "--timeline"]);
    assert!(tl.contains("tool call(s) (chronological)"), "{tl}");
    assert!(tl.contains("orch") && tl.contains("aBUILD"), "agents tagged:\n{tl}");
}
