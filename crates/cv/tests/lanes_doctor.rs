//! End-to-end tests for Wave C Track 1:
//! 1. `lanes`: `StateStore`-backed ledger (zero `lanes.jsonl`), immediate OS process exit
//!    detection (`(pid, proc_start_time, boot_id)`), recycled-PID safety, exact endpoint binding
//!    (zero description-slug guessing), and structural `open_obligations` (`stranded`) with zero
//!    English-prose regexes.
//! 2. `doctor`: post-compaction active context window vs lifetime history, true
//!    `system_and_tools_overhead`, token burn velocity, and `StoreHealthReport`.
//! 3. `LineageEdge` graph: multi-generation `A -> distilled B -> adopted C -> rewound D`, plus
//!    `spliced_from`, `retired_for`, `parent_subagent`, and `--dry-run` purity.

use cv_testkit::claude::{self, compact_summary, notification, subagent_stop, tool_result, tool_use, user};
use cv_testkit::{now_rfc3339, World};
use serde_json::{json, Value};

fn write_agent(w: &World, sid: &str, agent_id: &str, description: &str, lines: &[Value]) {
    let meta = json!({
        "agentType": "general-purpose",
        "description": description,
        "toolUseId": format!("toolu_{agent_id}"),
        "model": "opus"
    });
    w.write_agent(sid, agent_id, &meta, lines);
}

fn assistant(uuid: &str, ts: &str, text: &str) -> Value {
    claude::reply(
        uuid,
        ts,
        json!([{"type": "text", "text": text}]),
        json!({"input_tokens": 100, "output_tokens": 50}),
    )
}

fn assistant_with_usage(uuid: &str, ts: &str, text: &str, input_tokens: u64, output_tokens: u64) -> Value {
    claude::reply(
        uuid,
        ts,
        json!([{"type": "text", "text": text}]),
        json!({"input_tokens": input_tokens, "output_tokens": output_tokens}),
    )
}

fn assistant_tool(uuid: &str, ts: &str, name: &str, input: Value) -> Value {
    claude::reply(
        uuid,
        ts,
        json!([tool_use(&format!("toolu_{uuid}"), name, input)]),
        json!({"input_tokens": 10, "output_tokens": 5}),
    )
}

#[test]
fn dead_os_pid_exits_running_immediately_and_recycled_pid_is_safe() {
    let w = World::new("lanes-os-pid");
    let sid = "pidsess01";
    let now = now_rfc3339();

    w.write_session(sid, &[user(sid, "u0", &now, "spawn workers")]);

    // 1. Spawn a real short-lived OS process, capture its (pid, proc_start_time) while alive, then
    //    wait for it to exit so the PID is genuinely dead right now (< 1s old transcript, zero
    //    600s quiet-window wait).
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 0.25"])
        .spawn()
        .expect("spawn short-lived child");
    let dead_pid = child.id() as i64;
    let dead_start = cv_core::state::tables::proc_start_time_of(dead_pid);
    let _ = child.wait();

    write_agent(
        &w,
        sid,
        "deadproc",
        "DEADPROC: crashed mid-tool",
        &[
            user(sid, "d0", &now, "work"),
            assistant_tool("d1", &now, "Bash", json!({"command": "cargo test"})),
        ],
    );

    // 2. Sub-agent with recycled PID: points at our OWN live PID, but with a mismatched
    //    `proc_start_time` simulating OS PID reuse.
    write_agent(
        &w,
        sid,
        "recycled",
        "RECYCLED: stale pid reused by OS",
        &[
            user(sid, "r0", &now, "work"),
            assistant_tool("r1", &now, "Read", json!({"file_path": "/work/proj/a.rs"})),
        ],
    );

    // 3. Sub-agent with our genuine live `(pid, proc_start_time, boot_id)` — stays running.
    write_agent(
        &w,
        sid,
        "liveproc",
        "LIVEPROC: genuinely alive",
        &[
            user(sid, "l0", &now, "work"),
            assistant_tool("l1", &now, "Read", json!({"file_path": "/work/proj/b.rs"})),
        ],
    );

    let (self_pid, self_start) = cv_core::state::tables::current_process_identity();
    let boot_id = cv_core::lanes::current_boot_id();
    let store = cv_core::state::StateStore::at_dir(&w.cv_home);
    let started_ms = chrono::Utc::now().timestamp_millis();

    store
        .record_lane_start(&cv_core::state::LaneRun {
            run_id: format!("claude:{sid}:deadproc"),
            session_id: sid.into(),
            agent_id: Some("deadproc".into()),
            agent_type: Some("general-purpose".into()),
            harness: Some("claude".into()),
            pid: Some(dead_pid),
            proc_start_time: dead_start,
            boot_id: boot_id.clone(),
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: started_ms,
            exited_at: None,
            exit_status: None,
            last_text: None,
        })
        .unwrap();

    store
        .record_lane_start(&cv_core::state::LaneRun {
            run_id: format!("claude:{sid}:recycled"),
            session_id: sid.into(),
            agent_id: Some("recycled".into()),
            agent_type: Some("general-purpose".into()),
            harness: Some("claude".into()),
            pid: self_pid,
            proc_start_time: Some(self_start.unwrap_or(1000).wrapping_add(777_777)),
            boot_id: boot_id.clone(),
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: started_ms,
            exited_at: None,
            exit_status: None,
            last_text: None,
        })
        .unwrap();

    store
        .record_lane_start(&cv_core::state::LaneRun {
            run_id: format!("claude:{sid}:liveproc"),
            session_id: sid.into(),
            agent_id: Some("liveproc".into()),
            agent_type: Some("general-purpose".into()),
            harness: Some("claude".into()),
            pid: self_pid,
            proc_start_time: self_start,
            boot_id,
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: started_ms,
            exited_at: None,
            exit_status: None,
            last_text: None,
        })
        .unwrap();

    assert!(
        !w.cv_home.join("lanes.jsonl").exists(),
        "StateStore migration must never write flat lanes.jsonl"
    );

    let (json_out, _) = w.cv_ok(&["lanes", sid, "--json"]);
    let rows: Vec<Value> = serde_json::from_str(&json_out).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();

    let dead = by_id("deadproc");
    assert_eq!(dead["status"], "stopped", "{dead}");
    assert_eq!(dead["status_source"], "process_exit", "{dead}");
    assert_eq!(dead["stranded"], true, "{dead}");
    assert_eq!(dead["open_obligations"]["open_tool_calls"], 1, "{dead}");

    if self_start.is_some() {
        let rec = by_id("recycled");
        assert_eq!(
            rec["status"], "stopped",
            "recycled PID must never be misidentified as alive: {rec}"
        );
        assert_eq!(rec["status_source"], "process_exit", "{rec}");
        assert_eq!(rec["stranded"], true, "{rec}");
    }

    let live = by_id("liveproc");
    assert_eq!(live["status"], "running", "{live}");
    assert_eq!(live["stranded"], false, "{live}");
}

#[test]
fn structural_obligations_drive_stranded_with_zero_prose_regexes() {
    let w = World::new("lanes-structural-obligations");
    let sid = "structsess";

    w.write_session(
        sid,
        &[
            user(sid, "u0", "2026-10-01T10:00:00Z", "dispatch"),
            notification("2026-10-01T10:05:00Z", "prosewait", "completed"),
            notification("2026-10-01T10:05:01Z", "opentool", "completed"),
            notification("2026-10-01T10:05:02Z", "taskoblig", "completed"),
            assistant("a0", "2026-10-01T10:06:00Z", "acknowledged"),
        ],
    );

    // 1. Waiting English prose with ZERO structural obligations -> NOT stranded.
    write_agent(
        &w,
        sid,
        "prosewait",
        "PROSEWAIT: says waiting on notifications",
        &[
            user(sid, "pw0", "2026-10-01T10:00:10Z", "go"),
            assistant(
                "pw1",
                "2026-10-01T10:04:50Z",
                "Build is mirrored. Waiting on notifications. I will poll when ready.",
            ),
            subagent_stop("pw2", "2026-10-01T10:04:55Z"),
        ],
    );

    // 2. Cheerful "All done!" prose WITH an unmatched ToolUse -> STRANDED.
    write_agent(
        &w,
        sid,
        "opentool",
        "OPENTOOL: unmatched tool call",
        &[
            user(sid, "ot0", "2026-10-01T10:00:10Z", "go"),
            assistant_tool("ot1", "2026-10-01T10:02:00Z", "Bash", json!({"command": "cargo build"})),
            assistant("ot2", "2026-10-01T10:04:50Z", "All done, everything shipped cleanly."),
            subagent_stop("ot3", "2026-10-01T10:04:55Z"),
        ],
    );

    // 3. Clean transcript bound to `lane:w3` holding an open claimed task + unanswered decision -> STRANDED.
    write_agent(
        &w,
        sid,
        "taskoblig",
        "TASKOBLIG: holds claimed task and decision",
        &[
            user(sid, "to0", "2026-10-01T10:00:10Z", "go"),
            assistant_tool(
                "to1",
                "2026-10-01T10:01:00Z",
                "Bash",
                json!({"command": "export CV_ENDPOINT=lane:w3; echo ok"}),
            ),
            tool_result("to2", "2026-10-01T10:01:05Z", "toolu_to1", "ok"),
            assistant("to3", "2026-10-01T10:04:50Z", "Parked cleanly."),
            subagent_stop("to4", "2026-10-01T10:04:55Z"),
        ],
    );

    // Open & claim a task for `lane:w3`, and pose an unanswered decision owed by `lane:w3`.
    w.cv_ok(&["task", "open", "implement feature X", "--assignee", "lane:w3"]);
    let (ls_json, _) = w.cv_ok(&["task", "ls", "--json"]);
    let tasks: Vec<Value> = serde_json::from_str(&ls_json).unwrap();
    let task_id = tasks[0]["id"].as_str().unwrap().to_string();
    w.cv_ok(&["task", "claim", &task_id, "--from", "lane:w3"]);

    w.cv_ok(&[
        "task",
        "decide",
        "Which wire format?",
        "--for",
        "lane:w3",
        "--default",
        "cbor",
        "--option",
        "json",
        "--from",
        "orchestrator:root",
    ]);
    let (dec_ls, _) = w.cv_ok(&["task", "ls", "--json"]);
    let all_tasks: Vec<Value> = serde_json::from_str(&dec_ls).unwrap();
    let dec_id = all_tasks.iter().find(|t| t["title"] == "Which wire format?").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (lanes_json, _) = w.cv_ok(&["lanes", sid, "--json"]);
    let rows: Vec<Value> = serde_json::from_str(&lanes_json).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();

    let pw = by_id("prosewait");
    assert_eq!(
        pw["stranded"], false,
        "prose alone must never mark a lane stranded: {pw}"
    );
    assert_eq!(
        pw["open_obligations"],
        json!({
            "unreturned_subagents": 0,
            "open_tool_calls": 0,
            "open_claimed_tasks": 0,
            "unanswered_decisions": 0,
        }),
        "{pw}"
    );

    let ot = by_id("opentool");
    assert_eq!(ot["stranded"], true, "{ot}");
    assert_eq!(ot["open_obligations"]["open_tool_calls"], 1, "{ot}");

    let to = by_id("taskoblig");
    assert_eq!(to["stranded"], true, "{to}");
    assert_eq!(to["open_obligations"]["open_claimed_tasks"], 1, "{to}");
    assert_eq!(to["open_obligations"]["unanswered_decisions"], 1, "{to}");

    let (human_out, _) = w.cv_ok(&["lanes", sid]);
    assert!(human_out.contains("1 claimed task(s)"), "{human_out}");
    assert!(human_out.contains("1 unanswered decision(s)"), "{human_out}");

    // Resolve the decision and close the claimed task -> `taskoblig` is no longer stranded!
    w.cv_ok(&["task", "resolve", &dec_id, "--accept-default", "--from", "lane:w3"]);
    w.cv_ok(&["task", "close", &task_id, "--observed", "shipped", "--from", "lane:w3"]);

    let (lanes_after, _) = w.cv_ok(&["lanes", sid, "--json"]);
    let rows_after: Vec<Value> = serde_json::from_str(&lanes_after).unwrap();
    let to_after = rows_after.iter().find(|r| r["agent_id"] == "taskoblig").unwrap();
    assert_eq!(to_after["stranded"], false, "{to_after}");
}

#[test]
fn doctor_separates_post_compaction_active_window_from_history_and_reports_store_health() {
    let w = World::new("doctor-window-and-store");
    let sid = "docsess01";

    let big_pre_tool_output = "X".repeat(40_000);
    let summary_seed = "S".repeat(2_400);
    let post_user = "U".repeat(600);

    w.write_session(
        sid,
        &[
            user(sid, "m1", "2026-10-01T10:00:00Z", "investigate build"),
            claude::reply(
                "m2",
                "2026-10-01T10:00:05Z",
                json!([tool_use("toolu_pre", "Bash", json!({"command": "cargo build -vv"}))]),
                json!({"input_tokens": 55_000, "output_tokens": 200}),
            ),
            tool_result("m3", "2026-10-01T10:00:10Z", "toolu_pre", &big_pre_tool_output),
            claude::boundary("b1", "2026-10-01T10:01:00Z", "auto", 60_000),
            compact_summary("s1", "2026-10-01T10:01:01Z", &summary_seed),
            user(sid, "m4", "2026-10-01T10:02:00Z", &post_user),
            assistant_with_usage("m5", "2026-10-01T10:02:05Z", "Post-compaction reply 1.", 22_000, 120),
            user(sid, "m6", "2026-10-01T10:03:00Z", "read small config"),
            claude::reply(
                "m7",
                "2026-10-01T10:03:05Z",
                json!([tool_use(
                    "toolu_post",
                    "Read",
                    json!({"file_path": "/work/proj/Cargo.toml"})
                )]),
                json!({"input_tokens": 23_800, "output_tokens": 80}),
            ),
            tool_result(
                "m8",
                "2026-10-01T10:03:06Z",
                "toolu_post",
                "[package]\nname = \"demo\"\n",
            ),
        ],
    );

    // Seed authoritative `state.db` capture_inbox + a columnar `.clev` segment file so
    // `StoreHealthReport` inspects populated stores and indexes.
    {
        let store = cv_core::state::StateStore::at_dir(&w.cv_home);
        store
            .append(
                "doc-health-seed-1",
                "sha256:seed1",
                &json!({"kind": "probe", "ok": true}),
            )
            .unwrap();
    }
    let clev_dir = w.cv_home.join("events_columnar");
    std::fs::create_dir_all(&clev_dir).unwrap();
    std::fs::write(clev_dir.join("seg-0001.clev"), b"CVCLEV\x01\x00dummy").unwrap();

    let (doc_json, _) = w.cv_ok(&["doctor", sid, "--json"]);
    let doc: Value = serde_json::from_str(&doc_json).unwrap();

    // Active window vs lifetime history checks.
    let aw = &doc["active_window"];
    let hist = &doc["history"];
    assert_eq!(aw["from_compaction"], true, "{aw}");
    assert!(
        aw["conversational"]["tool_results"].as_u64().unwrap() < 500,
        "pre-compaction 40KB Bash output must not pollute active_window: {aw}"
    );
    assert!(
        hist["conversational"]["tool_results"].as_u64().unwrap() > 10_000,
        "lifetime history must retain pre-compaction tool bloat: {hist}"
    );
    let aw_tools: Vec<&str> = aw["by_tool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert_eq!(aw_tools, vec!["Read"], "{aw}");
    let hist_tools: Vec<&str> = hist["by_tool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert!(hist_tools.contains(&"Bash") && hist_tools.contains(&"Read"), "{hist}");

    let first_pt = aw["first_prompt_tokens"].as_u64().unwrap();
    let recon = aw["reconstructed_visible_turn_tokens"].as_u64().unwrap();
    let overhead = aw["system_and_tools_overhead"].as_u64().unwrap();
    assert_eq!(first_pt, 22_000);
    assert!(recon > 900, "summary + post_user visible tokens: {aw}");
    assert_eq!(overhead, 22_000 - recon);
    assert!(overhead < 22_000, "overhead subtracts visible turn content: {aw}");
    assert_eq!(aw["current_context"], 23_800);
    assert_eq!(aw["token_burn_per_turn"], 1_800);

    // Store & system health checks.
    let sh = &doc["store_health"];
    assert!(sh["sqlite_version"].as_str().unwrap().starts_with("3."), "{sh}");
    assert_eq!(sh["state_db"]["quick_check"], "ok", "{sh}");
    assert!(sh["state_db"]["bytes"].as_u64().unwrap() > 0, "{sh}");
    assert_eq!(sh["state_db"]["capture_inbox_backlog"], 1, "{sh}");
    assert_eq!(sh["catalog_db"]["quick_check"], "ok", "{sh}");
    assert!(sh["catalog_db"]["session_rows"].as_u64().unwrap() >= 1, "{sh}");
    assert_eq!(sh["search_indexes"]["columnar_events"]["clev_segments"], 1, "{sh}");
    let dev_class = sh["storage"]["device_class"].as_str().unwrap();
    assert!(["nvme", "hdd", "network", "unknown"].contains(&dev_class), "{sh}");
    assert!(sh["storage"]["free_bytes"].as_u64().unwrap() > 0, "{sh}");

    // Human output separates Active context window and Lifetime history.
    let (human, _) = w.cv_ok(&["doctor", sid]);
    assert!(
        human.contains("Active context window (post-compaction slice:"),
        "{human}"
    );
    assert!(human.contains("System + tools:"), "{human}");
    assert!(human.contains("Burn velocity:"), "{human}");
    assert!(human.contains("Lifetime history:"), "{human}");
    assert!(human.contains("Store health: sqlite"), "{human}");
}

#[test]
fn multi_generation_lineage_edges_across_distill_adopt_rewind_splice_and_retire() {
    let w = World::new("lineage-multi-gen");
    let sid_a = "11111111-1111-4111-8111-111111111111";
    let sid_c = "33333333-3333-4333-8333-333333333333";

    w.write_session(
        sid_a,
        &[
            user(sid_a, "a0", "2026-10-01T10:00:00Z", "design lineage graph"),
            assistant(
                "a1",
                "2026-10-01T10:00:10Z",
                "We will record directed LineageEdges in state.db.",
            ),
            user(sid_a, "a2", "2026-10-01T10:00:20Z", "verify multi-hop queries"),
            assistant(
                "a3",
                "2026-10-01T10:00:30Z",
                "BFS traversal over lineage_edges handles cycles safely.",
            ),
        ],
    );
    w.write_session(
        sid_c,
        &[
            user(sid_c, "c0", "2026-10-01T11:00:00Z", "host session C"),
            assistant("c1", "2026-10-01T11:00:10Z", "ready to adopt"),
            user(sid_c, "c2", "2026-10-01T11:00:20Z", "turn 2 before rewind"),
            assistant("c3", "2026-10-01T11:00:30Z", "turn 2 reply"),
        ],
    );

    let store = cv_core::state::StateStore::at_dir(&w.cv_home);

    // `--dry-run` (and stdout-only `distill`) MUST NEVER write LineageEdges!
    w.cv_ok(&["distill", sid_a, "--json"]);
    w.cv_ok(&["rewind", sid_c, "--at", "1", "--dry-run", "--json"]);
    assert!(
        store.lineage_descendants(sid_a).unwrap().is_empty(),
        "stdout-only distill must not write lineage edges"
    );
    assert!(
        store.lineage_descendants(sid_c).unwrap().is_empty(),
        "dry-run rewind must not write lineage edges"
    );

    // Hop 1: `A -> distilled agent-B` (`distill --into A --keep-source`).
    let (dist_json, _) = w.cv_ok(&["distill", sid_a, "--into", sid_a, "--keep-source", "--json"]);
    let dist_val: Value = serde_json::from_str(&dist_json).unwrap();
    let agent_b_bare = dist_val["emitted"]["agent_id"].as_str().unwrap().to_string();
    let node_b = format!("agent-{agent_b_bare}");

    // Hop 2: `agent-B -> adopted_into C` (`cv adopt <B> --from A --into C`).
    w.cv_ok(&["adopt", &agent_b_bare, "--from", sid_a, "--into", sid_c, "--json"]);

    // Hop 3: `C -> rewound_from D` (`cv rewind C --at 1`).
    let (rw_json, _) = w.cv_ok(&["rewind", sid_c, "--at", "1", "--json"]);
    let rw_val: Value = serde_json::from_str(&rw_json).unwrap();
    let sid_d = rw_val["new_id"].as_str().unwrap().to_string();

    // Verify 3-hop chain `sid_a -> node_b -> sid_c -> sid_d` in StateStore!
    let desc_a = store.lineage_descendants(sid_a).unwrap();
    assert!(
        desc_a
            .iter()
            .any(|e| e.src_session_id == sid_a && e.dst_session_id == node_b && e.edge_kind == "distilled_from"),
        "{desc_a:?}"
    );
    assert!(
        desc_a
            .iter()
            .any(|e| e.src_session_id == node_b && e.dst_session_id == sid_c && e.edge_kind == "adopted_into"),
        "{desc_a:?}"
    );
    assert!(
        desc_a
            .iter()
            .any(|e| e.src_session_id == sid_c && e.dst_session_id == sid_d && e.edge_kind == "rewound_from"),
        "{desc_a:?}"
    );

    let anc_d = store.lineage_ancestors(&sid_d).unwrap();
    let anc_srcs: Vec<&str> = anc_d.iter().map(|e| e.src_session_id.as_str()).collect();
    assert!(
        anc_srcs.contains(&sid_c) && anc_srcs.contains(&node_b.as_str()) && anc_srcs.contains(&sid_a),
        "ancestors of D must traverse C -> B -> A: {anc_d:?}"
    );

    // Also verify `spliced_from` and `retired_for` edges!
    let (sp_json, _) = w.cv_ok(&["splice", &format!("{sid_a}:..2"), "--session", "--json"]);
    let sp_val: Value = serde_json::from_str(&sp_json).unwrap();
    let sid_spliced = sp_val["session_id"].as_str().unwrap().to_string();
    let desc_after_splice = store.lineage_descendants(sid_a).unwrap();
    assert!(
        desc_after_splice
            .iter()
            .any(|e| e.src_session_id == sid_a && e.dst_session_id == sid_spliced && e.edge_kind == "spliced_from"),
        "{desc_after_splice:?}"
    );

    w.cv_ok(&["retire", &agent_b_bare, "--successor", "succ9999"]);
    let desc_b = store.lineage_descendants(&node_b).unwrap();
    assert!(
        desc_b.iter().any(|e| e.src_session_id == node_b
            && e.dst_session_id == "agent-succ9999"
            && e.edge_kind == "retired_for"),
        "{desc_b:?}"
    );

    // Verify `cv doctor <D> --json` surfaces multi-generation lineage ancestors!
    let (doc_d_json, _) = w.cv_ok(&["doctor", &sid_d, "--json"]);
    let doc_d: Value = serde_json::from_str(&doc_d_json).unwrap();
    let doc_anc: Vec<&str> = doc_d["lineage"]["ancestors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["src_session_id"].as_str().unwrap())
        .collect();
    assert!(
        doc_anc.contains(&sid_c) && doc_anc.contains(&node_b.as_str()) && doc_anc.contains(&sid_a),
        "{doc_d}"
    );

    // Verify `cv lanes <C> --json` surfaces lineage on the adopted sub-agent `agent-B`!
    let (lanes_c_json, _) = w.cv_ok(&["lanes", sid_c, "--json"]);
    let lanes_c: Vec<Value> = serde_json::from_str(&lanes_c_json).unwrap();
    let lane_b = lanes_c.iter().find(|r| r["agent_id"] == agent_b_bare).unwrap();
    assert!(
        lane_b["lineage"]["ancestors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["src_session_id"] == sid_a),
        "{lane_b}"
    );
}

#[test]
fn frozen_lanes_resolves_state_tasks_with_zero_filesystem_mutations() {
    let w = World::new("lanes-frozen-purity");
    let sid = "frozensess01";
    let now = now_rfc3339();

    w.write_session(sid, &[user(sid, "u0", &now, "coordinate frozen check")]);
    let export_cmd = "export CV_ENDPOINT=lane:frozen-worker && echo ok";
    write_agent(
        &w,
        sid,
        "fz1",
        "frozen worker",
        &[
            user(sid, "f0", &now, "claim and pause"),
            assistant_tool("f1", &now, "Bash", json!({"command": export_cmd})),
            tool_result("f2", &now, "toolu_f1", "ok"),
            assistant("f3", &now, "Done for now."),
            subagent_stop("f4", &now),
        ],
    );

    // Seed `state.db` (`lane_runs` via `cv hook lane` + `task_events` via `cv task open|claim`).
    {
        let payload = json!({
            "hook_event_name": "SubagentStart",
            "session_id": sid,
            "agent_id": "agent-fz1",
            "agent_type": "general-purpose",
            "cwd": w.proj.display().to_string(),
        });
        let (ok, code, out, err) = w.cv_full(&["hook", "lane"], &[], Some(&payload.to_string()));
        assert!(ok, "cv hook lane exited {code}: {out} {err}");
    }
    w.cv_ok(&["task", "open", "Frozen obligation", "--assignee", "lane:frozen-worker"]);
    let (ls_out, _) = w.cv_ok(&["task", "ls", "--json"]);
    let ls_rows: Vec<Value> = serde_json::from_str(&ls_out).unwrap();
    let task_id = ls_rows[0]["id"].as_str().unwrap().to_string();
    w.cv_ok(&["task", "claim", &task_id, "--from", "lane:frozen-worker"]);

    // Checkpoint & truncate `state.db` so `-wal` and `-shm` are absent before `--frozen`.
    {
        let store = cv_core::state::StateStore::at_dir(&w.cv_home);
        let conn = store.open_writable().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    assert!(
        !w.cv_home.join("state.db-wal").exists() && !w.cv_home.join("state.db-shm").exists(),
        "precondition: WAL/SHM sidecars absent after checkpoint+close"
    );

    fn snapshot_tree(root: &std::path::Path) -> Vec<(String, u64)> {
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Ok(meta) = entry.metadata() {
                        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
                        entries.push((rel, meta.len()));
                    }
                }
            }
        }
        entries.sort();
        entries
    }

    let before = snapshot_tree(&w.cv_home);

    let (lanes_out, _) = w.cv_ok(&["--frozen", "lanes", sid, "--json"]);
    let rows: Vec<Value> = serde_json::from_str(&lanes_out).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["open_obligations"]["open_claimed_tasks"], 1, "{rows:?}");
    assert_eq!(rows[0]["stranded"], true, "{rows:?}");

    let (lanes_tasks_out, _) = w.cv_ok(&["--frozen", "lanes", sid, "--tasks", "--json"]);
    let rows_tasks: Vec<Value> = serde_json::from_str(&lanes_tasks_out).unwrap();
    assert_eq!(rows_tasks[0]["tasks"].as_array().unwrap().len(), 1, "{rows_tasks:?}");

    let after = snapshot_tree(&w.cv_home);
    assert!(
        !w.cv_home.join("state.db-shm").exists() && !w.cv_home.join("state.db-wal").exists(),
        "--frozen lanes must never create state.db-shm or state.db-wal"
    );
    assert_eq!(
        before, after,
        "--frozen lanes must not create or mutate any file in CLUSTERVISION_HOME"
    );
}

#[test]
fn hook_lane_exit_coalesces_with_append_stop_and_child_session_runs_bind() {
    let w = World::new("lanes-hook-coalesce-and-child-sid");
    let sid = "coalsess01";
    let now = now_rfc3339();

    w.write_session(sid, &[user(sid, "u0", &now, "spawn lanes")]);

    // 1. Clean `SubagentStop` where `hook_lane` calls `record_lane_exit(..., Some("stopped"))`
    //    followed by `append_lane_event(Stop)` on a non-report turn (ends in tool_result, 0 open
    //    obligations): must coalesce into ONE `lane_runs` row, resolve `hook_ledger` (not
    //    `process_exit`), and keep `stranded == false`!
    write_agent(
        &w,
        sid,
        "cleanhook",
        "clean hook stop",
        &[
            user(sid, "c0", &now, "run check"),
            assistant_tool("c1", &now, "Bash", json!({"command": "true"})),
            tool_result("c2", &now, "toolu_c1", ""),
        ],
    );

    // 2. Child run recorded under the child's own `session.id` (`format!("{sid}/agent-childsid")`,
    //    `agent_id = None`) with a dead OS PID + bound `lane_endpoints`: must resolve both the
    //    bound endpoint (`endpoint_source == "state"`) and immediate `process_exit`!
    let mut short_proc = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let dead_pid = short_proc.id() as i64;
    let dead_start = cv_core::state::tables::proc_start_time_of(dead_pid);
    let _ = short_proc.wait();

    write_agent(
        &w,
        sid,
        "childsid",
        "child-session-keyed run",
        &[
            user(sid, "s0", &now, "run"),
            assistant_tool("s1", &now, "Bash", json!({"command": "echo hi"})),
        ],
    );

    let store = cv_core::state::StateStore::at_dir(&w.cv_home);
    let now_ms = chrono::Utc::now().timestamp_millis() + 2_000;
    store
        .record_lane_start(&cv_core::state::LaneRun {
            run_id: "run-cleanhook-1".into(),
            session_id: sid.into(),
            agent_id: Some("cleanhook".into()),
            agent_type: Some("general-purpose".into()),
            harness: Some("claude".into()),
            pid: None,
            proc_start_time: None,
            boot_id: None,
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: now_ms - 1_000,
            exited_at: None,
            exit_status: None,
            last_text: None,
        })
        .unwrap();
    store
        .record_lane_exit("run-cleanhook-1", now_ms, Some("stopped"), Some("finished"))
        .unwrap();
    {
        let payload = json!({
            "hook_event_name": "SubagentStop",
            "session_id": sid,
            "agent_id": "cleanhook",
            "agent_type": "general-purpose",
            "last_assistant_message": "hook stop",
            "cwd": w.proj.display().to_string(),
        });
        let (ok, code, out, err) = w.cv_full(&["hook", "lane"], &[], Some(&payload.to_string()));
        assert!(ok, "cv hook lane exited {code}: {out} {err}");
    }

    let runs_for_parent = store.list_lane_runs_for_session(sid).unwrap();
    let cleanhook_runs: Vec<_> = runs_for_parent
        .iter()
        .filter(|r| r.agent_id.as_deref() == Some("cleanhook"))
        .collect();
    assert_eq!(
        cleanhook_runs.len(),
        1,
        "append_lane_event(Stop) must coalesce onto the run closed by record_lane_exit: {cleanhook_runs:?}"
    );
    assert_eq!(cleanhook_runs[0].exit_status.as_deref(), Some("hook_stop"));

    // Record child-session-keyed run (`agent_id: None`, `session_id: "{sid}/agent-childsid"`).
    let child_session_id = format!("{sid}/agent-childsid");
    store
        .record_lane_start(&cv_core::state::LaneRun {
            run_id: "run-childsid-1".into(),
            session_id: child_session_id,
            agent_id: None,
            agent_type: None,
            harness: Some("claude".into()),
            pid: Some(dead_pid),
            proc_start_time: dead_start,
            boot_id: cv_core::lanes::current_boot_id(),
            transcript_path: None,
            agent_transcript_path: None,
            cwd: None,
            started_at: now_ms - 500,
            exited_at: None,
            exit_status: None,
            last_text: None,
        })
        .unwrap();
    store.bind_lane_endpoint("run-childsid-1", "lane:child-bound").unwrap();

    let (out_json, _) = w.cv_ok(&["lanes", sid, "--json"]);
    let rows: Vec<Value> = serde_json::from_str(&out_json).unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r["agent_id"] == id).unwrap().clone();

    let ch = by_id("cleanhook");
    assert_eq!(ch["status"], "stopped", "{ch}");
    assert_eq!(ch["status_source"], "hook_ledger", "{ch}");
    assert_eq!(
        ch["stranded"], false,
        "clean hook_stop with zero open obligations must NOT be marked crashed/stranded: {ch}"
    );

    let cs = by_id("childsid");
    assert_eq!(cs["endpoint"], "lane:child-bound", "{cs}");
    assert_eq!(cs["endpoint_source"], "state", "{cs}");
    assert_eq!(cs["status"], "stopped", "{cs}");
    assert_eq!(cs["status_source"], "process_exit", "{cs}");
    assert_eq!(cs["stranded"], true, "{cs}");
}
