//! A reshape carries what its caller selected: a splice into a Claude session writes each
//! system-role record back as it was. Every fixture here is invented text.

use cv_testkit::World;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SRC: &str = "5e1ec7ed-0000-4000-8000-000000000001";
const SRC2: &str = "5e1ec7ed-0000-4000-8000-000000000011";
const CODEX_ID: &str = "019d0000-0000-7000-8000-000000000021";

fn rows(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every record with a `uuid` is on the chain Claude Code walks back from the newest one.
fn assert_threaded(path: &Path) {
    let rows = rows(path);
    let threaded: Vec<&Value> = rows.iter().filter(|r| r.get("uuid").is_some()).collect();
    let by: HashMap<&str, &Value> = threaded.iter().map(|r| (r["uuid"].as_str().unwrap(), *r)).collect();
    let mut at = *threaded.last().expect("a threaded record");
    let mut reached = 1;
    while let Some(parent) = at["parentUuid"].as_str() {
        at = by
            .get(parent)
            .unwrap_or_else(|| panic!("parentUuid {parent} names no record in {}", path.display()));
        reached += 1;
    }
    assert_eq!(reached, threaded.len(), "a record is off the resumed chain");
}

/// A record as Claude Code 2.1.32x writes one: both spellings of the session id, and the
/// fields a reader has no name for.
fn record_in(sid: &str, ty: &str, uuid: &str, parent: Option<&str>, at: u32, body: Value) -> Value {
    let mut v = json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": sid, "session_id": sid,
        "timestamp": format!("2026-03-01T10:00:{at:02}Z"), "cwd": "/work/proj", "userType": "external",
        "entrypoint": "cli", "version": "2.1.327", "isSidechain": false, "slug": "tidy-garden"});
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    v
}

fn record(ty: &str, uuid: &str, parent: Option<&str>, at: u32, body: Value) -> Value {
    record_in(SRC, ty, uuid, parent, at, body)
}

/// A prompt, a call and its result, then what a person and the harness put in mid-turn: a
/// message typed while the agent worked, a file attached with `@`, a hook's output, a hook that
/// printed nothing (no message), a slash command's notice; then the reply.
fn mid_turn_session(w: &World) -> PathBuf {
    let queued = "The person sent a message while you were working:\nalso water the ferns";
    w.write_session(
        SRC,
        &[
            record(
                "user",
                "u1",
                None,
                1,
                json!({"origin": {"kind": "human"}, "promptSource": "typed", "permissionMode": "default",
                "message": {"role": "user", "content": "plan the garden beds"}}),
            ),
            record(
                "assistant",
                "a1",
                Some("u1"),
                2,
                json!({"requestId": "req_1", "message": {"role": "assistant", "model": "claude-opus-5", "content": [
                {"type": "tool_use", "id": "toolu_beds", "name": "Read", "input": {"file_path": "/work/proj/beds.md"}}]}}),
            ),
            record(
                "user",
                "r1",
                Some("a1"),
                3,
                json!({"toolUseResult": {"type": "text"}, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_beds", "content": "three beds, north to south"}]}}),
            ),
            record(
                "attachment",
                "q1",
                Some("r1"),
                4,
                json!({"attachment": {"type": "queued_command", "prompt": "also water the ferns", "commandMode": "prompt",
                    "origin": {"kind": "human"}, "humanTurn": true, "source_uuid": "src-1", "delivery_id": "del-1"},
                "rendered": [{"content": queued}], "renderedInHumanTurn": [{"content": "also water the ferns"}],
                "renderedRole": "user"}),
            ),
            record(
                "attachment",
                "f1",
                Some("q1"),
                5,
                json!({"attachment": {"type": "file", "filename": "/work/proj/ferns.md", "displayPath": "ferns.md",
                    "content": {"type": "text", "file": {"filePath": "/work/proj/ferns.md", "content": "ferns like shade"}}},
                "rendered": [{"content": "Called the Read tool with: ferns.md"}, {"content": "ferns like shade"}],
                "renderedRole": "user"}),
            ),
            record(
                "attachment",
                "h0",
                Some("f1"),
                6,
                json!({"attachment": {"type": "hook_success", "hookName": "PostToolUse", "content": "", "stdout": ""}}),
            ),
            record(
                "attachment",
                "h1",
                Some("h0"),
                7,
                json!({"attachment": {"type": "hook_success", "hookName": "UserPromptSubmit", "content": "beds are mulched"},
                "rendered": [{"content": "<system-reminder>beds are mulched</system-reminder>"}], "renderedRole": "system"}),
            ),
            record(
                "system",
                "s1",
                Some("h1"),
                8,
                json!({"subtype": "local_command", "level": "info", "isMeta": false,
                "content": "<local-command-stdout>compost turned</local-command-stdout>"}),
            ),
            record(
                "assistant",
                "a2",
                Some("s1"),
                9,
                json!({"message": {"role": "assistant", "model": "claude-opus-5", "content": [
                {"type": "text", "text": "beds planned, ferns watered"}]}}),
            ),
        ],
    )
}

fn write_codex_rollout(w: &World, id: &str, lines: &[Value]) -> PathBuf {
    let dir = w.home.join(".codex/sessions/2026/03/01");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-03-01T10-00-00-{id}.jsonl"));
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    fs::write(&path, body).unwrap();
    path
}

/// What is the session's own once it is a new one: the ids and where it hangs.
fn same_record(a: &Value, b: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        for key in ["sessionId", "session_id", "parentUuid"] {
            v.as_object_mut().unwrap().remove(key);
        }
        v
    };
    strip(a) == strip(b)
}

#[test]
fn splice_into_a_claude_session_carries_system_records_as_they_were() {
    let w = World::new("splice-system-records");
    let source = mid_turn_session(&w);
    let (out, err) = w.cv_ok(&["splice", SRC, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).expect("stdout is the JSON report");
    let new_id = report["session_id"].as_str().unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    let source_rows = rows(&source);

    // In order, with the mid-turn message and the attached file where they were said.
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(
        order,
        ["u1", "a1", "r1", "q1", "f1", "h1", "s1", "a2"],
        "{err}\n{written:#?}"
    );
    // As what they were: the record itself, not a paraphrase of its text.
    for uuid in ["q1", "f1", "h1", "s1"] {
        let original = source_rows.iter().find(|r| r["uuid"] == uuid).unwrap();
        assert!(
            same_record(by[uuid], original),
            "{uuid} is not the record it was:\n{}\n{original}",
            by[uuid]
        );
    }
    assert_eq!(by["q1"]["attachment"]["origin"]["kind"], "human");
    assert_eq!(by["q1"]["attachment"]["prompt"], "also water the ferns");
    assert_eq!(by["f1"]["attachment"]["filename"], "/work/proj/ferns.md");
    // A typed prompt stays a person's typed prompt.
    assert_eq!(by["u1"]["origin"]["kind"], "human", "{}", by["u1"]);
    assert_eq!(by["u1"]["promptSource"], "typed");
    // One session: every spelling of the id is the new one, and the chain is whole.
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(SRC), "a record still names the source session");
    for r in &written {
        for key in ["sessionId", "session_id"] {
            if let Some(id) = r.get(key) {
                assert_eq!(id, new_id, "{r}");
            }
        }
    }
    assert_threaded(&path);
    // Nothing is reported lost, and cv reads the new session as it read the source.
    assert!(!err.contains("lossy"), "{err}");
    let shown = |id: &str| -> Vec<(String, String, String)> {
        let (out, _) = w.cv_ok(&["show", id, "--json"]);
        let v: Value = serde_json::from_str(&out).unwrap();
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let text: Vec<&str> = m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|b| b.get("text").or(b.get("content"))?.as_str())
                    .collect();
                (
                    m["kind"].as_str().unwrap_or_default().to_owned(),
                    m["origin"].as_str().unwrap_or_default().to_owned(),
                    text.join("\n"),
                )
            })
            .collect()
    };
    let (before, after) = (shown(SRC), shown(new_id));
    assert_eq!(before.len(), 8);
    assert_eq!(after, before);
}

/// A span cut through the middle keeps the records inside it and re-links the cut.
#[test]
fn a_spliced_window_keeps_the_system_records_inside_it() {
    let w = World::new("splice-system-window");
    mid_turn_session(&w);
    // Messages 3..8 are the mid-turn message, the file, the hook output, the notice, the reply.
    let spec = format!("{SRC}:3..8");
    let (out, err) = w.cv_ok(&["splice", &spec, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let order: Vec<String> = rows(&path)
        .iter()
        .filter_map(|r| Some(r.get("uuid")?.as_str()?.to_owned()))
        .collect();
    assert_eq!(order, ["q1", "f1", "h1", "s1", "a2"], "{err}");
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");
}

/// Multi-span splice where Span 1 ends in `_record` attachments/notices (and also when Span 1 is
/// ONLY `_record`s) keeps every trailing record on Span 2's backward `parentUuid` walk.
#[test]
fn multi_span_splice_ending_in_system_records_keeps_parent_chain_whole() {
    let w = World::new("splice-multi-span-carrier-tail");
    mid_turn_session(&w);
    w.write_session(
        SRC2,
        &[
            record_in(
                SRC2,
                "user",
                "b_u1",
                None,
                10,
                json!({"message": {"role": "user", "content": "now plant the herbs"}}),
            ),
            record_in(
                SRC2,
                "assistant",
                "b_a1",
                Some("b_u1"),
                11,
                json!({"message": {"role": "assistant", "model": "claude-opus-5", "content": [
                    {"type": "text", "text": "herbs planted"}]}}),
            ),
        ],
    );

    // Case 1: Span 1 ends at `s1` (`0..7` = `u1, a1, r1, q1, f1, h1, s1`), Span 2 starts at root `b_u1`.
    let s1 = format!("{SRC}:0..7");
    let s2 = format!("{SRC2}:0..2");
    let (out, err) = w.cv_ok(&["splice", &s1, &s2, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(
        order,
        ["u1", "a1", "r1", "q1", "f1", "h1", "s1", "b_u1", "b_a1"],
        "{err}"
    );
    let b_u1 = written.iter().find(|r| r["uuid"] == "b_u1").unwrap();
    assert_eq!(
        b_u1["parentUuid"], "s1",
        "Span 2 root must link to Span 1's last carrier"
    );
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");

    // Case 2: Span 1 consists ONLY of `_record` messages (`3..7` = `q1, f1, h1, s1`).
    let only_carriers = format!("{SRC}:3..7");
    let (out, err) = w.cv_ok(&["splice", &only_carriers, &s2, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(order, ["q1", "f1", "h1", "s1", "b_u1", "b_a1"], "{err}");
    assert_threaded(&path);
}

#[test]
fn mixed_codex_and_claude_splice_preserves_provenance_records_and_system_prompt() {
    let w = World::new("splice-mixed-provenance");
    mid_turn_session(&w);
    write_codex_rollout(
        &w,
        CODEX_ID,
        &[
            json!({"timestamp": "2026-03-01T09:59:00Z", "type": "session_meta",
                "payload": {"id": CODEX_ID, "cwd": "/work/proj", "base_instructions": {"text": "Follow garden house style."}}}),
            json!({"timestamp": "2026-03-01T09:59:01Z", "type": "response_item",
                "payload": {"type": "message", "role": "developer",
                    "content": [{"type": "input_text", "text": "<environment_context>zone 8b</environment_context>"}]}}),
            json!({"timestamp": "2026-03-01T09:59:02Z", "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "prepare soil"}]}}),
            json!({"timestamp": "2026-03-01T09:59:03Z", "type": "compacted",
                "payload": {"message": "soil prepared summary"}}),
        ],
    );

    // 1. Mixed `codex + claude -> --session`: keeps Span 1's foreign InjectedContext + system_prompt
    //    AND keeps Span 2's verbatim Claude `_record`s (`q1`, `f1`, `h1`, `s1`) and turn extras!
    let codex_all = format!("codex:{CODEX_ID}");
    let (out, err) = w.cv_ok(&["splice", &codex_all, SRC, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    assert!(
        written.iter().any(|r| r["attachment"]["type"] == "prompt_snapshot"
            && r["attachment"]["systemPrompt"][0] == "Follow garden house style."),
        "foreign system_prompt snapshot should be emitted: {written:#?}"
    );
    assert!(
        written.iter().any(|r| r["attachment"]["type"] == "injected_context"),
        "foreign Codex developer context should emit via SystemFate::Special"
    );
    assert!(
        written.iter().any(|r| r["isCompactSummary"] == true),
        "foreign Codex compaction summary should emit via SystemFate::Special"
    );
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    assert_eq!(by["q1"]["attachment"]["prompt"], "also water the ferns");
    assert_eq!(by["u1"]["promptSource"], "typed");
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");

    // 2. Foreign `codex -> --harness claude --out <dir>`: emits foreign system_prompt + Special kinds.
    let out_dir = w.base.join("claude-out");
    let (_, err) = w.cv_ok(&[
        "splice",
        &codex_all,
        "--harness",
        "claude",
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert!(!err.contains("lossy"), "{err}");
    let emitted = fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            p.is_dir().then_some(p)
        })
        .flat_map(|d| fs::read_dir(d).unwrap())
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .expect("emitted claude jsonl");
    let emitted_rows = rows(&emitted);
    assert!(emitted_rows
        .iter()
        .any(|r| r["attachment"]["type"] == "prompt_snapshot"));
    assert!(emitted_rows.iter().any(|r| r["subtype"] == "compact_boundary"));
    assert_threaded(&emitted);
}

#[test]
fn splice_export_json_omits_carrier_records_and_obeys_extra_namespaces() {
    let w = World::new("splice-export-json-clean");
    mid_turn_session(&w);
    let spec = format!("{SRC}:0..8");
    let (out, _) = w.cv_ok(&["splice", &spec, "--export", "json"]);
    let session: cv_core::Session = serde_json::from_str(&out).expect("valid Session JSON");
    let check_bag = |extra: &serde_json::Map<String, Value>, ctx: &str| {
        for (k, v) in extra {
            assert!(
                (k == cv_core::ir::CV_NAMESPACE || cv_core::Harness::parse(k).is_some()) && v.is_object(),
                "{ctx}: flat or non-namespace key {k:?} = {v:?}"
            );
        }
    };
    check_bag(&session.extra, "session.extra");
    for (i, m) in session.messages.iter().enumerate() {
        assert!(
            !m.extra.contains_key(cv_core::ir::CARRIER_KEY),
            "message {i} exported JSON must not leak _record: {:?}",
            m.extra
        );
        check_bag(&m.extra, &format!("message[{i}].extra"));
    }
}

#[test]
fn spliced_claude_compaction_boundary_reports_zero_spurious_fidelity_deltas() {
    let w = World::new("splice-compact-boundary-fidelity");
    w.write_session(
        SRC,
        &[
            record(
                "user",
                "u1",
                None,
                1,
                json!({"message": {"role": "user", "content": "start work"}}),
            ),
            record(
                "system",
                "cb1",
                None,
                2,
                json!({"subtype": "compact_boundary", "logicalParentUuid": "u1", "level": "info",
                    "content": "Conversation compacted", "compactMetadata": {"trigger": "auto", "preTokens": 120000}}),
            ),
            record(
                "user",
                "cs1",
                Some("cb1"),
                3,
                json!({"isCompactSummary": true, "message": {"role": "user", "content": "Compacted summary"}}),
            ),
            record(
                "assistant",
                "a1",
                Some("cs1"),
                4,
                json!({"message": {"role": "assistant", "model": "claude-opus-5",
                    "content": [{"type": "text", "text": "continuing after compaction"}]}}),
            ),
        ],
    );
    let (out, err) = w.cv_ok(&["splice", SRC, "--session", "--json"]);
    assert!(
        !err.contains("lossy"),
        "unexpected fidelity delta on compact_boundary:\n{err}"
    );
    assert!(!err.contains("timestamps supplied by target"), "{err}");
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    assert!(written
        .iter()
        .any(|r| r["uuid"] == "cb1" && r["subtype"] == "compact_boundary"));
}
