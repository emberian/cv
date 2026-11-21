//! End-to-end tests for the deep-Claude-harness surfaces: `cv show --kind compaction_*`, `cv workflow`,
//! `cv tools`, and `cv show --pre-compaction`. Each builds a hermetic on-disk Claude session
//! (transcript + the `<sid>/workflows/` state/scripts + `<sid>/subagents/` agent transcripts +
//! journal) under a temp `$HOME`, then drives the real binary and asserts on its output.
//!
//! Hermetic through `cv_testkit::World` (own `$HOME` + `$CLUSTERVISION_HOME`, env-only, parallel-safe).

use cv_testkit::{jsonl, World};
use std::fs;

/// A workflow *state* file `<sid>/workflows/<run>.json` + its script.
fn write_workflow(w: &World, sid: &str, run: &str, state: &serde_json::Value, script: &str) {
    let wdir = w.proj.join(sid).join("workflows");
    let sdir = wdir.join("scripts");
    fs::create_dir_all(&sdir).unwrap();
    fs::write(wdir.join(format!("{run}.json")), serde_json::to_string(state).unwrap()).unwrap();
    // The launcher's convention: `scripts/<workflowName>-wf_<run>.js` — the basename's name half
    // is the workflow's name (what ghost recovery matches an orphan against).
    let name = state["workflowName"].as_str().unwrap_or("workflow");
    fs::write(sdir.join(format!("{name}-wf_{run}.js")), script).unwrap();
}

/// A workflow *agent* transcript + its meta under `<sid>/subagents/workflows/<run>/`.
fn write_workflow_agent(w: &World, sid: &str, run: &str, agent_id: &str, lines: &[serde_json::Value]) {
    let dir = w.proj.join(sid).join("subagents").join("workflows").join(run);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("agent-{agent_id}.jsonl")), jsonl(lines)).unwrap();
    fs::write(
        dir.join(format!("agent-{agent_id}.meta.json")),
        r#"{"agentType":"workflow-subagent"}"#,
    )
    .unwrap();
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

fn tool_use(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
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
                    tool_use("t1", "Bash", serde_json::json!({"command": "cargo build"})),
                    tool_use("t2", "Workflow", serde_json::json!({"script": "wf"})),
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
                    "t3",
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
    write_workflow(
        &w,
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
            "scriptPath": "/work/proj/fsess/workflows/scripts/demo-workflow-wf_test-0001.js",
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
    write_workflow_agent(
        &w,
        sid,
        run,
        "aBUILD",
        &[
            user("wu0", None, "build it"),
            assistant_tools(
                "wa0",
                "2026-06-07T12:01:00Z",
                serde_json::json!([
                    tool_use("b1", "Bash", serde_json::json!({"command": "make"})),
                    tool_use("b2", "Read", serde_json::json!({"file_path": "src/lib.rs"})),
                    tool_use(
                        "b3",
                        "Edit",
                        serde_json::json!({"file_path": "src/lib.rs", "old_string": "x", "new_string": "y"})
                    ),
                ]),
            ),
        ],
    );
    write_workflow_agent(
        &w,
        sid,
        run,
        "aVERIFY",
        &[
            user("vu0", None, "verify it"),
            assistant_tools(
                "va0",
                "2026-06-07T12:02:00Z",
                serde_json::json!([tool_use("v1", "Bash", serde_json::json!({"command": "cargo test"}))]),
            ),
        ],
    );

    w
}

#[test]
fn show_kind_compaction_lists_boundaries_and_summaries() {
    let w = build_world("compaction");
    let (out, _) = w.cv_ok(&["show", "fsess", "--kind", "compaction_boundary,compaction_summary"]);
    assert!(out.contains("compacted 2 time(s)"), "got:\n{out}");
    assert!(out.contains("manual"), "trigger surfaced:\n{out}");
    assert!(out.contains("auto"), "second trigger surfaced:\n{out}");
    assert!(out.contains("SUMMARY ONE"), "first summary head:\n{out}");

    // --summaries prints full text of both.
    let (full, _) = w.cv_ok(&[
        "show",
        "fsess",
        "--kind",
        "compaction_boundary,compaction_summary",
        "--summaries",
    ]);
    assert!(full.contains("SUMMARY ONE: the early context"));
    assert!(full.contains("SUMMARY TWO: the middle context"));

    // JSON carries pre_compaction_span.
    let (js, _) = w.cv_ok(&[
        "show",
        "fsess",
        "--kind",
        "compaction_boundary,compaction_summary",
        "--json",
    ]);
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

    // A flag that acts on one run is refused where there is no run, not dropped.
    for args in [
        &["workflow", "fsess", "--script"][..],
        &["workflow", "fsess", "--results"][..],
        &["workflow", "fsess", "--revive"][..],
        &["workflow", "fsess", "--follow"][..],
        &["workflow", "fsess", "wf_test", "--revive-all"][..],
        &["workflow", "fsess", "wf_test", "--revive", "--json"][..],
        &["workflow", "fsess", "wf_test", "--revive", "--follow"][..],
    ] {
        let (_, code, _, err) = w.cv(args);
        assert_eq!(code, 2, "{args:?}: {err}");
    }

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

/// A background agent outlives an in-place `/resume`: its next records go to the resumed session's
/// `subagents/`, starting from the parent of the last record in the first session's copy, with no
/// meta beside them. Timestamps are recent (`< 300s` ago) so `cv lanes sess-first` cannot coast on
/// `RETURNED_QUIET_SECS` and must merge `<task-notification>` from `sess-resumed`.
fn split_by_resume(tag: &str) -> World {
    use chrono::{Duration, SecondsFormat, Utc};
    use cv_testkit::claude::with;
    let w = World::new(tag);
    let (first, resumed, agent, spawn) = ("sess-first", "sess-resumed", "a5f1170c0de000001", "toolu_spawn");
    let now = Utc::now();
    let ts = |secs_ago: i64| (now - Duration::seconds(secs_ago)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let rec = |base: serde_json::Value, sid: &str, uuid: &str, parent: Option<&str>| {
        with(
            base,
            serde_json::json!({"sessionId": sid, "uuid": uuid, "parentUuid": parent}),
        )
    };
    let side = |r: serde_json::Value| with(r, serde_json::json!({"isSidechain": true, "agentId": agent}));
    let user = |sid: &str, uuid: &str, parent: Option<&str>, at: &str, content: serde_json::Value| {
        rec(
            serde_json::json!({"type": "user", "timestamp": at, "cwd": "/work/proj",
                "message": {"role": "user", "content": content}}),
            sid,
            uuid,
            parent,
        )
    };
    let said = |sid: &str, uuid: &str, parent: &str, at: &str, blocks: serde_json::Value| {
        rec(assistant_tools(uuid, at, blocks), sid, uuid, Some(parent))
    };
    let text = |t: &str| serde_json::json!([{"type": "text", "text": t}]);
    let result = |id: &str, out: &str| serde_json::json!([{"type": "tool_result", "tool_use_id": id, "content": out}]);
    let note = format!(
        "<task-notification>\n<task-id>{agent}</task-id>\n<tool-use-id>{spawn}</tool-use-id>\n<status>completed</status>\n<summary>Agent \"survey the rivers\" completed</summary>\n</task-notification>"
    );

    w.write_session(
        first,
        &[
            user(first, "o-u1", None, &ts(60), "start the survey".into()),
            said(
                first,
                "o-a1",
                "o-u1",
                &ts(59),
                serde_json::json!([tool_use(
                    spawn,
                    "Agent",
                    serde_json::json!({"description": "survey the rivers", "prompt": "count the rivers",
                        "subagent_type": "general-purpose", "run_in_background": true})
                )]),
            ),
            user(
                first,
                "o-r1",
                Some("o-a1"),
                &ts(58),
                result(spawn, &format!("Async agent launched successfully.\nagentId: {agent}")),
            ),
            said(first, "o-a2", "o-r1", &ts(57), text("launched the survey")),
            serde_json::json!({"type": "last-prompt", "leafUuid": "o-a2", "sessionId": first}),
        ],
    );
    w.write_agent(
        first,
        agent,
        &serde_json::json!({"agentType": "general-purpose", "description": "survey the rivers",
            "toolUseId": spawn, "spawnDepth": 1}),
        &[
            side(user(first, "x-u1", None, &ts(58), "count the rivers".into())),
            side(said(
                first,
                "x-a1",
                "x-u1",
                &ts(56),
                serde_json::json!([tool_use(
                    "toolu_x1",
                    "Bash",
                    serde_json::json!({"command": "ls rivers"})
                )]),
            )),
            side(user(
                first,
                "x-r1",
                Some("x-a1"),
                &ts(55),
                result("toolu_x1", "ochrebank"),
            )),
            side(said(
                first,
                "x-a2",
                "x-r1",
                &ts(54),
                serde_json::json!([{"type": "text", "text": "the first river is ochrebank"},
                    tool_use("toolu_x2", "Bash", serde_json::json!({"command": "wc rivers"}))]),
            )),
            serde_json::json!({"type": "api-request", "sessionId": first, "timestamp": ts(54)}),
        ],
    );

    w.write_session(
        resumed,
        &[
            user(resumed, "n-u1", None, &ts(30), "here is where we were".into()),
            said(resumed, "n-a1", "n-u1", &ts(29), text("understood")),
            user(resumed, "n-u2", Some("n-a1"), &ts(28), "first try".into()),
            serde_json::json!({"type": "last-prompt", "leafUuid": "n-u2", "sessionId": resumed}),
            user(resumed, "n-u2b", Some("n-a1"), &ts(25), "second try".into()),
            said(resumed, "n-a2", "n-u2b", &ts(24), text("answering the second try")),
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "sessionId": resumed,
                "timestamp": ts(5), "content": note}),
            serde_json::json!({"type": "queue-operation", "operation": "remove", "sessionId": resumed,
                "timestamp": ts(5), "content": note}),
            rec(
                serde_json::json!({"type": "attachment", "timestamp": ts(5),
                    "attachment": {"type": "queued_command", "prompt": note, "commandMode": "task-notification"},
                    "rendered": [{"content": note}], "renderedInHumanTurn": true}),
                resumed,
                "n-q1",
                Some("n-a2"),
            ),
            said(resumed, "n-a3", "n-q1", &ts(4), text("the survey came back")),
            serde_json::json!({"type": "last-prompt", "leafUuid": "n-a3", "sessionId": resumed}),
        ],
    );
    let tail = [
        side(rec(
            serde_json::json!({"type": "attachment", "timestamp": ts(20),
                "attachment": {"type": "hook_success", "hookName": "PostToolUse", "hookEvent": "PostToolUse", "content": ""}}),
            resumed,
            "y-h1",
            Some("x-a2"),
        )),
        side(user(
            resumed,
            "y-r2",
            Some("x-a2"),
            &ts(19),
            result("toolu_x2", "2 rivers"),
        )),
        side(said(
            resumed,
            "y-a3",
            "y-r2",
            &ts(6),
            text("two rivers: ochrebank and viridianford"),
        )),
    ];
    let dir = w.proj.join(resumed).join("subagents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("agent-{agent}.jsonl")), jsonl(&tail)).unwrap();
    w
}

#[test]
fn an_agent_split_by_a_resume_reads_whole_from_either_session() {
    let w = split_by_resume("resume-split");
    let ids = |out: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect()
    };
    let whole = ["x-u1", "x-a1", "x-r1", "x-a2", "y-r2", "y-a3"];

    // By its id, from the session that spawned it, and from the one it continued in: one agent,
    // its spawn's call and meta, where it continues from, and no false `(adopted)` banner.
    for (i, args) in [
        vec!["show", "agent-a5f1170c0de000001", "--json"],
        vec!["show", "sess-first", "--agent", "a5f1170c0de000001", "--json"],
        vec!["show", "sess-resumed", "--agent", "a5f1170c0de000001", "--json"],
    ]
    .into_iter()
    .enumerate()
    {
        let (out, err) = w.cv_ok(&args);
        if i == 0 {
            assert!(!err.contains("(adopted)"), "split agent is not adopted:\n{err}");
        }
        assert_eq!(ids(&out), whole, "{args:?}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["lineage"]["continues"], "sess-first", "{args:?}");
        assert_eq!(v["lineage"]["spawned_by_tool_use"], "toolu_spawn", "{args:?}");
        assert_eq!(v["extra"]["claude"]["agent_type"], "general-purpose", "{args:?}");
    }

    // Each session's forest lists it under its spawn's description, ending where it ended, and
    // `cv lanes sess-first` sees `<task-notification>` from `sess-resumed` even within 5 minutes.
    for sid in ["sess-first", "sess-resumed"] {
        let (subs, _) = w.cv_ok(&["show", sid, "--subagents"]);
        assert!(subs.contains("survey the rivers"), "{sid}:\n{subs}");
        assert!(subs.contains("viridianford"), "{sid}:\n{subs}");
        let (lanes, _) = w.cv_ok(&["lanes", sid]);
        assert!(lanes.contains("1 completed"), "{sid}:\n{lanes}");
        let (unfinished, _) = w.cv_ok(&["lanes", sid, "--unfinished", "--json"]);
        let rows: serde_json::Value = serde_json::from_str(&unfinished).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 0, "{sid} unfinished:\n{unfinished}");
    }
}

#[test]
fn split_agent_reentry_and_quoted_uuid_negative_controls() {
    use chrono::{Duration, SecondsFormat, Utc};
    use cv_testkit::claude::with;
    let w = split_by_resume("resume-reentry");
    let (first, resumed, agent) = ("sess-first", "sess-resumed", "a5f1170c0de000001");
    let now = Utc::now();
    let ts = |secs_ago: i64| (now - Duration::seconds(secs_ago)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let side = |sid: &str, ty: &str, uuid: &str, parent: Option<&str>, at: &str, content: serde_json::Value| {
        with(
            serde_json::json!({"type": ty, "timestamp": at, "cwd": "/work/proj",
                "isSidechain": true, "agentId": agent,
                "message": {"role": ty, "model": "claude-test", "content": content}}),
            serde_json::json!({"sessionId": sid, "uuid": uuid, "parentUuid": parent}),
        )
    };

    // Sibling session quoting `"y-a3"` and `"x-a2"` in tool results must NEVER be chained into `agent`.
    w.write_session("sess-decoy", &[user("d-u1", None, "decoy")]);
    let decoy_dir = w.proj.join("sess-decoy").join("subagents");
    fs::create_dir_all(&decoy_dir).unwrap();
    let decoy_lines = [side(
        "sess-decoy",
        "user",
        "d-1",
        None,
        &ts(90),
        serde_json::json!("log snippet quoting \"x-a2\" and \"y-a3\" verbatim"),
    )];
    fs::write(decoy_dir.join(format!("agent-{agent}.jsonl")), jsonl(&decoy_lines)).unwrap();

    // Now simulate re-entry `sess-first -> sess-resumed -> sess-first`: append `z-u4`, `z-a4` onto
    // `sess-first/subagents/agent-<id>.jsonl` with `parentUuid: "y-a3"`.
    let reentered = [
        side(
            first,
            "user",
            "z-u4",
            Some("y-a3"),
            &ts(3),
            serde_json::json!("name the third tributary"),
        ),
        side(
            first,
            "assistant",
            "z-a4",
            Some("z-u4"),
            &ts(2),
            serde_json::json!([{"type": "text", "text": "amberbrook joins downstream"}]),
        ),
    ];
    let first_agent = w
        .proj
        .join(first)
        .join("subagents")
        .join(format!("agent-{agent}.jsonl"));
    let mut f = fs::OpenOptions::new().append(true).open(&first_agent).unwrap();
    std::io::Write::write_all(&mut f, jsonl(&reentered).as_bytes()).unwrap();
    drop(f);

    let expected = ["x-u1", "x-a1", "x-r1", "x-a2", "y-r2", "y-a3", "z-u4", "z-a4"];
    for sid in [first, resumed] {
        let (out, _) = w.cv_ok(&["show", sid, "--agent", agent, "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let got: Vec<&str> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(
            got, expected,
            "from {sid} (re-entry must interleave B between A's two runs)"
        );
        assert_eq!(v["lineage"]["continues"], resumed, "final run in A continued from B");
        assert!(!out.contains("log snippet quoting"), "decoy must never chain");
    }
}
