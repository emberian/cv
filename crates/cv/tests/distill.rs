//! Publication must follow budget and unfinished-work checks.
use cv_testkit::{claude, World};
use serde_json::json;

fn source(w: &World, id: &str, open: bool) {
    let mut rows = vec![claude::user(id, "u", "2026-10-01T00:00:00Z", "test the handoff")];
    rows.push(claude::assistant(
        "a",
        "2026-10-01T00:00:01Z",
        "test",
        if open {
            json!([{"type":"tool_use","id":"build","name":"Bash","input":{"command":"cargo test"}}])
        } else {
            json!([{"type":"text","text":"The resulting finding is retained."}])
        },
    ));
    w.write_session(id, &rows);
}

#[test]
fn distill_budget_checks_aggregate_before_publishing() {
    let w = World::new("distill-budget");
    source(&w, "budget-source", false);
    source(&w, "budget-other", false);
    let (ok, _, pack, err) = w.cv(&["distill", "budget-source"]);
    assert!(ok, "{err}");
    let limit = pack.len().to_string();
    let output = w.base.join("derived");
    let markdown = w.base.join("handoff.md");
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "budget-source",
        "--with",
        "budget-other",
        "--max-bytes",
        &limit,
        "--session",
        "--out",
        output.to_str().unwrap(),
        "--pack",
        markdown.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(err.contains("exceeding --max-bytes"), "{err}");
    assert!(!output.exists());
    assert!(!markdown.exists());
}

#[test]
fn distill_budget_names_and_measures_what_the_target_loads() {
    let w = World::new("distill-budget-measured");
    source(&w, "measured-source", false);
    let (ok, _, pack, err) = w.cv(&["distill", "measured-source"]);
    assert!(ok, "{err}");
    let json = |args: &[&str]| -> serde_json::Value {
        let (ok, _, report, err) = w.cv(args);
        assert!(ok, "{err}");
        serde_json::from_str(&report).unwrap()
    };
    let packed = json(&["distill", "measured-source", "--json"]);
    assert_eq!(packed["budget"]["what"], "markdown pack");
    assert_eq!(packed["budget"]["bytes"], pack.len());
    // A resumable session loads the preamble and the tail's real turns, not the Markdown tail.
    let first = w.base.join("first");
    let resumed = json(&[
        "distill",
        "measured-source",
        "--session",
        "--out",
        first.to_str().unwrap(),
        "--json",
    ]);
    let what = resumed["budget"]["what"].as_str().unwrap();
    assert!(what.starts_with("resumable session content"), "{resumed}");
    let loaded = resumed["budget"]["bytes"].as_u64().unwrap();
    let transcript = std::path::PathBuf::from(resumed["emitted"]["path"].as_str().unwrap());
    let opening = std::fs::read_to_string(&transcript).unwrap();
    let opening: serde_json::Value =
        serde_json::from_str(opening.lines().find(|line| line.contains("[cv distill]")).unwrap()).unwrap();
    let prompt = opening["message"]["content"]
        .as_str()
        .map(str::len)
        .unwrap_or_else(|| opening["message"]["content"][0]["text"].as_str().unwrap().len());
    assert!(
        loaded >= prompt as u64 && prompt > packed["stats"]["pack_est_tokens"].as_u64().unwrap() as usize,
        "{resumed}"
    );
    assert_eq!(
        resumed["emitted"]["transcript_bytes"],
        std::fs::metadata(&transcript).unwrap().len()
    );
    assert!(resumed["emitted"]["transcript_bytes"].as_u64().unwrap() > loaded);
    // One byte under what would be loaded refuses and writes nothing; the exact size is accepted.
    let refused = w.base.join("refused");
    let under = (loaded - 1).to_string();
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "measured-source",
        "--session",
        "--out",
        refused.to_str().unwrap(),
        "--max-bytes",
        &under,
    ]);
    assert!(!ok);
    assert!(
        err.contains("resumable session content") && err.contains("exceeding --max-bytes"),
        "{err}"
    );
    assert!(!refused.exists());
    let exact = w.base.join("exact");
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "measured-source",
        "--session",
        "--out",
        exact.to_str().unwrap(),
        "--max-bytes",
        &loaded.to_string(),
    ]);
    assert!(ok, "{err}");
    assert!(
        err.contains("budget measured on the resumable session content"),
        "{err}"
    );
    let tokens = (resumed["budget"]["estimated_tokens"].as_u64().unwrap() - 1).to_string();
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "measured-source",
        "--session",
        "--out",
        refused.to_str().unwrap(),
        "--max-estimated-tokens",
        &tokens,
    ]);
    assert!(!ok);
    assert!(err.contains("system prompt and tools come on top"), "{err}");
    assert!(!refused.exists());
}

#[test]
fn distill_unsettled_refuses_resume_but_allows_explicit_snapshot() {
    let w = World::new("distill-unsettled");
    source(&w, "unsettled-source", true);
    let output = w.base.join("derived");
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "unsettled-source",
        "--session",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(err.contains("1 calls without terminal evidence"), "{err}");
    assert!(err.contains("build cargo test (no terminal result)"), "{err}");
    assert!(!output.exists());
    let (ok, _, pack, err) = w.cv(&["distill", "unsettled-source"]);
    assert!(ok, "{err}");
    assert!(pack.contains("- **Unresolved calls** (1)"), "{pack}");
    // The disclosure belongs to the hand-off a successor reads first, not after other lanes' findings.
    assert!(pack.find("**Unresolved calls**").unwrap() < pack.find("## 2. Brief").unwrap());
    let (ok, _, report, err) = w.cv(&[
        "distill",
        "unsettled-source",
        "--session",
        "--out",
        output.to_str().unwrap(),
        "--allow-unsettled",
        "--json",
    ]);
    assert!(ok, "{err}");
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    // --out is a store root; the Claude emitter places main-thread transcripts in a
    // project directory beneath it. Follow the published path instead of guessing layout.
    let transcript = std::path::PathBuf::from(report["emitted"]["path"].as_str().unwrap());
    assert!(transcript.starts_with(&output), "{report}");
    let transcript = std::fs::read_to_string(transcript).unwrap();
    assert!(transcript.contains("Unresolved calls"));
    assert!(transcript.contains("`build`"));
    assert_eq!(report["unsettled"][0]["id"], "build");
}

/// Row shapes are Claude Code 2.1.x's (acknowledgement sentences, `toolUseResult` fields, poll and
/// stop payloads, notifications); every id and command is synthetic.
struct Lane {
    rows: Vec<serde_json::Value>,
}

impl Lane {
    fn new(id: &str) -> Self {
        Lane {
            rows: vec![claude::user(id, "u0", "2026-10-01T00:00:00Z", "run the gates")],
        }
    }
    fn stamp(&self) -> String {
        format!("2026-10-01T00:{:02}:{:02}Z", self.rows.len() / 60, self.rows.len() % 60)
    }
    fn call(
        &mut self,
        id: &str,
        tool: &str,
        input: serde_json::Value,
        result: &str,
        sidecar: Option<serde_json::Value>,
    ) {
        let uuid = format!("a-{id}");
        let stamp = self.stamp();
        self.rows.push(claude::assistant(
            &uuid,
            &stamp,
            "test",
            json!([claude::tool_use(id, tool, input)]),
        ));
        let mut row = claude::tool_result(&format!("r-{id}"), &self.stamp(), id, result);
        if result.starts_with("<tool_use_error>") {
            row["message"]["content"][0]["is_error"] = json!(true);
        }
        if let Some(sidecar) = sidecar {
            row["toolUseResult"] = sidecar;
        }
        self.rows.push(row);
    }
    fn bash(&mut self, id: &str, command: &str, result: &str, sidecar: Option<serde_json::Value>) {
        self.call(
            id,
            "Bash",
            json!({"command": command, "description": command}),
            result,
            sidecar,
        );
    }
    fn poll(&mut self, id: &str, task: &str, status: &str, tail: &str) {
        let body = format!(
            "<retrieval_status>{}</retrieval_status>\n\n<task_id>{task}</task_id>\n\n<task_type>local_bash</task_type>\n\n<status>{status}</status>\n\n{tail}",
            if status == "running" { "not_ready" } else { "success" }
        );
        self.call(
            id,
            "TaskOutput",
            json!({"task_id": task, "block": false, "timeout": 1000}),
            &body,
            None,
        );
    }
    fn notice(&mut self, body: String) {
        let stamp = self.stamp();
        let uuid = format!("n{}", self.rows.len());
        self.rows
            .push(json!({"type":"user","uuid":uuid,"sessionId":"s","timestamp":stamp,
            "origin":{"kind":"task-notification"},"message":{"role":"user","content":body}}));
    }
    fn report(self, w: &World, id: &str) -> (serde_json::Value, String) {
        w.write_session(id, &self.rows);
        let (ok, _, pack, err) = w.cv(&["distill", id, "--keep-last", "0"]);
        assert!(ok, "{err}");
        let pack_file = w.base.join(format!("{id}.md"));
        let (ok, _, report, err) = w.cv(&[
            "distill",
            id,
            "--keep-last",
            "0",
            "--json",
            "--pack",
            pack_file.to_str().unwrap(),
        ]);
        assert!(ok, "{err}");
        (serde_json::from_str(&report).unwrap(), pack)
    }
}

fn unsettled(report: &serde_json::Value) -> Vec<(String, String)> {
    report["unsettled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            (
                call["id"].as_str().unwrap().to_owned(),
                call["why"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn ledger_row<'a>(pack: &'a str, id: &str) -> &'a str {
    let needle = format!("· `{id}` ");
    pack.lines()
        .find(|line| line.starts_with("- [") && line.contains(&needle))
        .unwrap_or_else(|| panic!("no ledger row for {id}:\n{pack}"))
}

const BACKGROUND: &str = "Command running in background with ID: ";

#[test]
fn claude_poll_only_history_settles_from_the_polled_status_never_from_the_poll() {
    let w = World::new("distill-claude-poll");
    let mut lane = Lane::new("polls");
    lane.bash(
        "fails",
        "cargo test -p gate",
        &format!("{BACKGROUND}b1. Output is being written to: /t/b1.output"),
        None,
    );
    lane.poll("p1", "b1", "running", "<output>\ntest result: ok. 3 passed\n</output>");
    lane.poll(
        "p2",
        "b1",
        "failed",
        "<exit_code>101</exit_code>\n\n<output>\nerror[E0308]: mismatched types\ntest result: FAILED. 1 passed; 2 failed\n</output>",
    );
    lane.bash(
        "passes",
        "cargo test -p other",
        &format!("{BACKGROUND}b2. Output is being written to: /t/b2.output"),
        None,
    );
    lane.poll(
        "p3",
        "b2",
        "completed",
        "<exit_code>0</exit_code>\n\n<output>\ntest result: ok. 9 passed; 0 failed\n</output>",
    );
    lane.bash(
        "running",
        "cargo test -p slow",
        &format!("{BACKGROUND}b3. Output is being written to: /t/b3.output"),
        None,
    );
    // A successful poll call whose payload says running, and one that names another task.
    lane.poll(
        "p4",
        "b3",
        "running",
        "<output>\n<status>completed</status>\ntest result: ok. 1 passed\n</output>",
    );
    lane.call(
        "p5",
        "TaskOutput",
        json!({"task_id": "b3", "block": true, "timeout": 1000}),
        "<retrieval_status>success</retrieval_status>\n\n<task_id>b2</task_id>\n\n<status>completed</status>",
        None,
    );
    let (report, pack) = lane.report(&w, "polls");
    assert_eq!(
        unsettled(&report),
        [("running".to_owned(), "job `b3` not seen to end".to_owned())]
    );
    // The verdict is mined against the command that started the job, from the terminal poll only.
    assert!(
        ledger_row(&pack, "fails").contains("✗ error[E0308]: mismatched types"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "passes").contains("⚑ test result: ok. 9 passed; 0 failed"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "running").contains("unfinished: no terminal result observed"),
        "{pack}"
    );
    // Consecutive polls of one task share a row; it carries the last poll's outcome.
    assert!(
        ledger_row(&pack, "p2").contains("✗ error[E0308]: mismatched types"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "p5").contains("unfinished: no terminal result observed"),
        "{pack}"
    );
    assert!(!pack.contains("1 passed"), "{pack}");
    assert!(
        !pack.contains("3 passed"),
        "a running poll's partial output is not a verdict:\n{pack}"
    );
    assert_eq!(report["stats"]["errors"], 1, "{pack}");
    assert!(pack.contains("cargo test -p gate"), "{pack}");
}

#[test]
fn claude_background_work_of_every_native_kind_is_tracked_until_it_ends() {
    let w = World::new("distill-claude-kinds");
    let mut lane = Lane::new("kinds");
    let timed_out = "Command did not complete within its 600s timeout and was moved to the background (ID: b1). Output is being written to: /t/b1.output.";
    lane.bash(
        "timeout-open",
        "make long",
        &format!("partial\n{timed_out}"),
        Some(json!({"stdout":"partial","stderr":"","interrupted":false,"backgroundTaskId":"b1","timedOutAfterMs":600000})),
    );
    lane.bash("timeout-stopped", "make longer", &timed_out.replace("b1", "b2"), None);
    lane.call(
        "stop",
        "TaskStop",
        json!({"task_id": "b2"}),
        "{\"message\":\"Successfully stopped task: b2 (make longer)\",\"task_id\":\"b2\",\"task_type\":\"local_bash\",\"command\":\"make longer\"}",
        None,
    );
    lane.bash("moved", "make docs", "Command was moved to the background (ID: b3) after running for 180s. Output is being written to: /t/b3.output.", None);
    lane.bash(
        "by-user",
        "make site",
        "Command was manually backgrounded by user with ID: b4. Output is being written to: /t/b4.output.",
        None,
    );
    let monitor = |task: &str| {
        format!("Monitor started (task {task}, expires in 30m unless the source ends first). You will be notified on each event.")
    };
    lane.call(
        "watch-expired",
        "Monitor",
        json!({"command":"tail -f a.log","description":"a"}),
        &monitor("m1"),
        None,
    );
    lane.call(
        "watch-open",
        "Monitor",
        json!({"command":"tail -f b.log","description":"b"}),
        &monitor("m2"),
        None,
    );
    let event = |task: &str, event: &str| {
        format!("<task-notification>\n<task-id>{task}</task-id>\n<summary>Monitor event: \"a\"</summary>\n<event>{event}</event>\n</task-notification>")
    };
    lane.notice(event(
        "m1",
        "[Monitor expired after 30m with no events delivered. Re-arm it if you still need the watch.]",
    ));
    lane.notice(event("m2", "line 7: compiled"));
    let agent =
        "Async agent launched successfully. (This tool result is internal metadata.)\nagentId: a1 (internal ID)";
    lane.call(
        "agent",
        "Agent",
        json!({"description":"audit","prompt":"audit it"}),
        agent,
        None,
    );
    // Agent completions are also delivered without the launching tool-use id.
    lane.notice("<task-notification>\n<task-id>a1</task-id>\n<output-file>/t/a1</output-file>\n<status>completed</status>\n<summary>Agent \"audit\" completed</summary>\n<result>done</result>\n</task-notification>".into());
    lane.call(
        "resume",
        "SendMessage",
        json!({"to":"a1","message":"one more"}),
        "{\"success\":true,\"message\":\"Resuming agent a1\",\"resumedAgentId\":\"a1\",\"pin\":{\"id\":\"a1\"}}",
        None,
    );
    lane.call(
        "workflow",
        "Workflow",
        json!({"script":"run()"}),
        "Workflow launched in background. Task ID: w1\nSummary: readers",
        Some(json!({"status":"async_launched","taskId":"w1","taskType":"local_workflow"})),
    );
    // A transcript quoted in ordinary output starts nothing.
    lane.bash(
        "quoted",
        "cv show other",
        &format!("[tool_result x] {timed_out}\n- [3] wait → {BACKGROUND}zz."),
        Some(json!({"stdout":"quoted","stderr":"","interrupted":false})),
    );
    let (report, pack) = lane.report(&w, "kinds");
    let open: Vec<_> = unsettled(&report);
    let expect = |id: &str, job: &str| (id.to_owned(), format!("job `{job}` not seen to end"));
    assert_eq!(
        open,
        [
            expect("by-user", "b4"),
            expect("moved", "b3"),
            expect("resume", "a1"),
            expect("timeout-open", "b1"),
            expect("watch-open", "m2"),
            expect("workflow", "w1"),
        ],
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "timeout-stopped").contains("stopped: no successful completion implied"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "watch-expired").contains("stopped: no successful completion implied"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "agent").contains("ended after its acknowledgement"),
        "{pack}"
    );
    for id in ["timeout-open", "moved", "by-user", "watch-open", "resume", "workflow"] {
        assert!(
            ledger_row(&pack, id).contains("unfinished: no terminal result observed"),
            "{id}\n{pack}"
        );
    }
    assert!(!ledger_row(&pack, "quoted").contains("unfinished"), "{pack}");
    let hand_off = &pack[pack
        .find("**Unresolved calls** (6)")
        .expect("hand-off lists unresolved work")..];
    assert!(
        hand_off.contains("make long · `timeout-open` · job `b1` not seen to end"),
        "{pack}"
    );
}

#[test]
fn claude_notice_after_a_terminal_poll_is_one_completion() {
    let w = World::new("distill-claude-once");
    let mut lane = Lane::new("once");
    lane.bash(
        "gate",
        "cargo test -p gate",
        &format!("{BACKGROUND}b1. Output is being written to: /t/b1.output"),
        None,
    );
    lane.poll(
        "p1",
        "b1",
        "failed",
        "<exit_code>1</exit_code>\n\n<output>\nerror: linker failed\n</output>",
    );
    lane.notice("<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>gate</tool-use-id>\n<output-file>/t/b1.output</output-file>\n<status>failed</status>\n<summary>Background command \"cargo test -p gate\" failed with exit code 1</summary>\n</task-notification>".into());
    let (report, pack) = lane.report(&w, "once");
    assert!(unsettled(&report).is_empty(), "{pack}");
    assert_eq!(report["stats"]["errors"], 1, "{pack}");
    assert!(ledger_row(&pack, "gate").contains("✗ error: linker failed"), "{pack}");
}

#[test]
fn claude_wrapped_notices_and_results_beside_text_reach_the_analysis() {
    let w = World::new("distill-claude-wrapped");
    let mut lane = Lane::new("wrapped");
    let ack = |task: &str| format!("{BACKGROUND}{task}. Output is being written to: /t/{task}.output");
    let notification = |task: &str, call: &str| {
        format!("<task-notification>\n<task-id>{task}</task-id>\n<tool-use-id>{call}</tool-use-id>\n<output-file>/t/{task}.output</output-file>\n<status>completed</status>\n<summary>Background command completed (exit code 0)</summary>\n</task-notification>")
    };
    let preamble = "[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event, NOT a message from the user.\nNo human input has been received.\n\n";
    let queued = |lane: &mut Lane, mode: &str, prompt: String| {
        let stamp = lane.stamp();
        let rendered = format!("<system-reminder>\n{preamble}{prompt}\n</system-reminder>");
        lane.rows.push(
            json!({"type":"attachment","uuid":format!("q{}", lane.rows.len()),"sessionId":"s","timestamp":stamp,
            "attachment":{"type":"queued_command","prompt":prompt,"commandMode":mode},
            "rendered":[{"content":rendered}],"renderedRole":"system"}),
        );
    };
    lane.bash("turn", "make a", &ack("b1"), None);
    lane.notice(format!("{preamble}{}", notification("b1", "turn")));
    lane.bash("mid-turn", "make b", &ack("b2"), None);
    queued(&mut lane, "task-notification", notification("b2", "mid-turn"));
    // What a person typed while the lane worked is not the harness's notice, whatever it quotes.
    lane.bash("typed", "make c", &ack("b3"), None);
    queued(&mut lane, "prompt", notification("b3", "typed"));
    lane.bash("quoted", "make d", &ack("b4"), None);
    lane.rows.push(claude::user(
        "wrapped",
        "human",
        &lane.stamp(),
        &format!("it printed {}", notification("b4", "quoted")),
    ));
    let stamp = lane.stamp();
    lane.rows.push(claude::assistant(
        "a-search",
        &stamp,
        "test",
        json!([claude::tool_use(
            "search",
            "ToolSearch",
            json!({"query":"select:Monitor"})
        )]),
    ));
    lane.rows.push(json!({"type":"user","uuid":"r-search","sessionId":"s","timestamp":lane.stamp(),"message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"search","content":[{"type":"tool_reference","tool_name":"Monitor"}]},
        {"type":"text","text":"Tool loaded."}]}}));
    let (report, pack) = lane.report(&w, "wrapped");
    let open: Vec<String> = unsettled(&report).into_iter().map(|(id, _)| id).collect();
    assert_eq!(open, ["quoted", "typed"], "{pack}");
    assert!(
        ledger_row(&pack, "turn").contains("ended after its acknowledgement"),
        "{pack}"
    );
    assert!(
        ledger_row(&pack, "mid-turn").contains("ended after its acknowledgement"),
        "{pack}"
    );
    assert!(ledger_row(&pack, "search").contains("ToolSearch"), "{pack}");
}
