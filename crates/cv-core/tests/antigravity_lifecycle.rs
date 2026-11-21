//! Synthetic native store contracts, independently encoded from the installed 1.5.9
//! descriptors. These are deliberately not labeled harness-written corpus captures.
#![cfg(feature = "sqlite")]

use cv_core::harness::{antigravity::Antigravity, Parse};
use cv_core::{Adapter, Block, Harness, MessageKind, Session, SessionRef, SubagentDemand};
use cv_testkit::World;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const PARENT: &str = "11111111-2222-4333-8444-555555555555";
const CHILD: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let next = (value & 127) as u8;
        value >>= 7;
        out.push(next | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return out;
        }
    }
}
fn number(field: u64, value: u64) -> Vec<u8> {
    [varint(field << 3), varint(value)].concat()
}
fn bytes(field: u64, value: impl AsRef<[u8]>) -> Vec<u8> {
    let value = value.as_ref();
    [varint((field << 3) | 2), varint(value.len() as u64), value.to_vec()].concat()
}
fn call(id: &str, name: &str, arguments: &str) -> Vec<u8> {
    [bytes(1, id), bytes(2, name), bytes(3, arguments)].concat()
}
fn step(ty: u64, status: u64, payload_field: u64, payload: Vec<u8>, extra: Vec<u8>) -> Vec<u8> {
    [number(1, ty), number(4, status), bytes(payload_field, payload), extra].concat()
}
fn prompt(text: &str) -> Vec<u8> {
    step(14, 3, 19, bytes(3, bytes(1, text)), vec![])
}
fn planner(calls: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = bytes(1, "Running two independent checks");
    for call in calls {
        payload.extend(bytes(7, call));
    }
    step(15, 3, 20, payload, vec![])
}
fn result(id: &str, body: &str) -> Vec<u8> {
    step(
        132,
        3,
        140,
        bytes(2, bytes(1, body)),
        bytes(5, bytes(4, call(id, "run_command", "{}"))),
    )
}
fn database(root: &Path, id: &str, rows: &[(i64, u64, u64, Vec<u8>)]) -> PathBuf {
    let dir = root.join("conversations");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.db"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (
        idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER,
        has_subtrajectory INTEGER DEFAULT 0, metadata BLOB, error_details BLOB,
        permissions BLOB, task_details BLOB, render_info BLOB,
        step_payload BLOB, step_format INTEGER DEFAULT 0
    );",
    )
    .unwrap();
    for (index, ty, status, payload) in rows {
        conn.execute(
            "INSERT INTO steps(idx,step_type,status,step_payload) VALUES (?1,?2,?3,?4)",
            rusqlite::params![index, ty, status, payload],
        )
        .unwrap();
    }
    path
}
fn log(root: &Path, id: &str, name: &str, body: &str) -> PathBuf {
    let dir = root.join("brain").join(id).join(".system_generated/logs");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}
fn world(label: &str) -> cv_testkit::InProcess<cv_core::env::Isolated> {
    World::in_process(label, cv_core::env::Env::enter_isolated)
}
fn text(session: &Session) -> String {
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| {
            if let Block::Text { text } = block {
                Some(text.try_resolve(&resolver).unwrap().into_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn native_database_only_preserves_explicit_parallel_call_ids() {
    let world = world("agy-native-identities");
    let calls = [
        call("native-A", "run_command", r#"{"CommandLine":"check A"}"#),
        call("native-B", "run_command", r#"{"CommandLine":"check B"}"#),
    ];
    let path = database(
        &world.base,
        PARENT,
        &[
            (0, 14, 3, prompt("Check both targets")),
            (1, 15, 3, planner(&calls)),
            // Results deliberately appear in the opposite order to same-name calls.
            (2, 132, 3, result("native-B", "B passed")),
            (3, 132, 3, result("native-A", "A passed")),
        ],
    );
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter.discover().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path, path);
    let session = adapter.parse(&refs[0]).unwrap();
    assert!(text(&session).contains("Check both targets"));
    let ids: Vec<_> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| {
            if let Block::ToolUse { id, .. } = b {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, ["native-A", "native-B"]);
    let ids: Vec<_> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| {
            if let Block::ToolResult { tool_use_id, .. } = b {
                Some(tool_use_id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, ["native-B", "native-A"]);
}

#[test]
fn cleared_native_payload_does_not_erase_exported_history() {
    let world = world("agy-cleared-history");
    database(&world.base, PARENT, &[(0, 14, 5, step(14, 5, 19, vec![], vec![]))]);
    log(
        &world.base,
        PARENT,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({
                "step_index":0,"type":"USER_INPUT","status":"DONE","content":"historical request survives"
            })
        ),
    );
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter.discover().unwrap();
    assert_eq!(refs.len(), 1);
    assert!(text(&adapter.parse(&refs[0]).unwrap()).contains("historical request survives"));
}

#[test]
fn root_selection_promotes_full_history_over_compact_mirror() {
    let world = world("agy-root-promotion");
    let cli = world.base.join("antigravity-cli");
    let ide = world.base.join("antigravity-ide");
    log(
        &cli,
        PARENT,
        "transcript.jsonl",
        &format!(
            "{}\n",
            json!({
                "step_index":0,"type":"USER_INPUT","content":"compact projection"
            })
        ),
    );
    let full = log(
        &ide,
        PARENT,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({
                "step_index":0,"type":"USER_INPUT","content":"full history"
            })
        ),
    );
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter.discover().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path, full);
    assert_eq!(text(&adapter.parse(&refs[0]).unwrap()), "full history");
}

#[test]
fn surviving_native_child_is_readable_without_exported_log() {
    let world = world("agy-native-child");
    let parent = log(&world.base, PARENT, "transcript_full.jsonl", "{}\n");
    let child = database(&world.base, CHILD, &[(0, 14, 3, prompt("Native worker request"))]);
    let members = world.base.join("brain").join(PARENT).join(".system_generated/swarm");
    fs::create_dir_all(&members).unwrap();
    fs::write(
        members.join("members.json"),
        json!({"members": {
            "reviewer":{"conversation_id":CHILD,"role":"review","killed":true}
        }})
        .to_string(),
    )
    .unwrap();
    let adapter = Antigravity::with_home(&world.base);
    let parent = SessionRef::new(PARENT, Harness::Antigravity, parent);
    let children = adapter
        .forest(&parent, SubagentDemand::Rich, None, &|_, _| true)
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].session.path, child);
    assert_eq!(children[0].result_status.as_deref(), Some("killed"));
    let session = adapter.parse(&children[0].session).unwrap();
    assert_eq!(session.messages[0].kind, MessageKind::Prompt);
    assert_eq!(text(&session), "Native worker request");
}

#[test]
fn cleared_database_only_history_reports_unavailable_payloads() {
    let world = world("agy-cleared-native-only");
    let path = database(&world.base, PARENT, &[(0, 15, 5, step(15, 5, 20, vec![], vec![]))]);
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
        .unwrap();
    let limitations = session.extra["cv"]["read_coverage"]["limitations"]
        .as_array()
        .expect("cleared native history must expose its coverage limits");
    assert!(limitations.iter().any(|v| v == "missing_payloads"));
    assert!(text(&session).is_empty(), "do not invent missing planner text");
}

#[test]
fn unknown_native_storage_format_cannot_look_like_complete_empty_history() {
    let world = world("agy-unknown-native-format");
    let path = database(
        &world.base,
        PARENT,
        &[(0, 14, 3, prompt("unrecognized storage representation"))],
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE steps SET step_format=999", []).unwrap();
    drop(conn);
    let adapter = Antigravity::with_home(&world.base);
    if let Ok(session) = adapter.parse(&SessionRef::new(PARENT, Harness::Antigravity, path)) {
        let limitations = session.extra["cv"]["read_coverage"]["limitations"]
            .as_array()
            .expect("unknown storage format must fail or report incomplete coverage");
        assert!(limitations
            .iter()
            .any(|v| v == "unsupported_native_fields" || v == "missing_payloads"));
    }
}

#[test]
fn same_second_replacement_cannot_attach_usage_from_another_native_revision() {
    let world = world("agy-replaced-native-revision");
    let metadata = [bytes(1, number(1, 1_700_000_000)), bytes(9, number(9, 99))].concat();
    let payload = step(15, 3, 20, bytes(1, "replacement response"), bytes(5, &metadata));
    let path = database(&world.base, PARENT, &[(0, 15, 3, payload)]);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("UPDATE steps SET metadata=?1", [metadata]).unwrap();
    drop(conn);
    log(
        &world.base,
        PARENT,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({
                "step_index":0,"type":"PLANNER_RESPONSE","status":"DONE",
                "created_at":"2023-11-14T22:13:20Z","content":"original response",
                "input_tokens":10,"output_tokens":2
            })
        ),
    );
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter.discover().unwrap();
    let session = adapter.parse(&refs[0]).unwrap();
    assert_eq!(text(&session), "original response");
    assert_eq!(
        session.messages[0].usage.as_ref().unwrap().reasoning_tokens,
        None,
        "same index and second are not sufficient revision identity"
    );
}

#[test]
fn malformed_native_protobuf_must_fail_or_report_missing_payloads() {
    for (name, payload) in [
        ("truncated_varint", vec![0x80]),
        ("length_past_eof", vec![0xa2, 0x01, 0xff, 0xff, 0xff, 0x7f]),
        (
            "overflowing_varint",
            vec![0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
        ),
    ] {
        let world = world(&format!("agy-malformed-{name}"));
        let path = database(&world.base, PARENT, &[(0, 15, 3, payload)]);
        let adapter = Antigravity::with_home(&world.base);
        if let Ok(session) = adapter.parse(&SessionRef::new(PARENT, Harness::Antigravity, path)) {
            let limitations = session.extra["cv"]["read_coverage"]["limitations"]
                .as_array()
                .expect("malformed native payload must fail or expose unavailable content");
            assert!(limitations
                .iter()
                .any(|v| v == "missing_payloads" || v == "unsupported_native_fields"));
        }
    }
}

#[test]
fn native_thinking_signature_survives_reading() {
    let world = world("agy-native-thinking");
    let response = [
        bytes(1, "The answer"),
        bytes(3, "Reasoning"),
        bytes(4, "native-signature"),
    ]
    .concat();
    let path = database(&world.base, PARENT, &[(0, 15, 3, step(15, 3, 20, response, vec![]))]);
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
        .unwrap();
    assert!(session.messages.iter().flat_map(|m| &m.content).any(|b| matches!(b,
        Block::Thinking { signature:Some(signature), .. } if signature=="native-signature")));
}

#[test]
fn future_native_step_is_explicitly_unsupported_in_complete_reads() {
    let world = world("agy-native-future");
    let path = database(
        &world.base,
        PARENT,
        &[(0, 9999, 3, step(9999, 3, 9999, bytes(1, "future payload"), vec![]))],
    );
    let adapter = Antigravity::with_home(&world.base);
    let reference = SessionRef::new(PARENT, Harness::Antigravity, path);
    let session = cv_core::stream::collect_with(&adapter, &reference, &cv_core::Demand::COMPLETE).unwrap();
    let limitations = session.extra["cv"]["read_coverage"]["limitations"]
        .as_array()
        .expect("uninterpreted native steps must remain visible in coverage");
    assert!(limitations.iter().any(|v| v == "unsupported_native_fields"));
}

#[test]
fn one_corrupt_native_database_does_not_hide_other_sessions() {
    let world = world("agy-native-partial-fleet");
    database(&world.base, PARENT, &[(0, 14, 3, prompt("readable native session"))]);
    fs::write(
        world.base.join("conversations").join(format!("{CHILD}.db")),
        b"not a SQLite database",
    )
    .unwrap();
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter
        .discover()
        .expect("one broken native store must not abort the whole fleet");
    let session = refs
        .iter()
        .find(|r| r.id == PARENT)
        .expect("healthy native session remains discoverable");
    assert_eq!(text(&adapter.parse(session).unwrap()), "readable native session");
}

fn exported_parallel_session(label: &str, planner_body: &str) -> Session {
    let world = world(label);
    let calls = [
        call("native-A", "run_command", r#"{"CommandLine":"check A"}"#),
        call("native-B", "run_command", r#"{"CommandLine":"check B"}"#),
    ];
    database(
        &world.base,
        PARENT,
        &[
            (0, 15, 3, planner(&calls)),
            (1, 132, 3, result("native-B", "B passed")),
            (2, 132, 3, result("native-A", "A passed")),
        ],
    );
    let exported = [
        json!({"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE",
        "content":planner_body,"tool_calls":[
            {"name":"run_command","args":{"CommandLine":"check A"}},
            {"name":"run_command","args":{"CommandLine":"check B"}}
        ]}),
        json!({"step_index":1,"type":"GENERIC","status":"DONE","content":"B passed"}),
        json!({"step_index":2,"type":"GENERIC","status":"DONE","content":"A passed"}),
    ]
    .into_iter()
    .map(|v| format!("{v}\n"))
    .collect::<String>();
    log(&world.base, PARENT, "transcript_full.jsonl", &exported);
    let adapter = Antigravity::with_home(&world.base);
    let reference = adapter.discover().unwrap().remove(0);
    assert!(reference.path.ends_with("transcript_full.jsonl"));
    adapter.parse(&reference).unwrap()
}

fn tool_ids(session: &Session) -> (Vec<&str>, Vec<&str>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in session.messages.iter().flat_map(|m| &m.content) {
        match block {
            Block::ToolUse { id, .. } => calls.push(id.as_str()),
            Block::ToolResult { tool_use_id, .. } => results.push(tool_use_id.as_str()),
            _ => {}
        }
    }
    (calls, results)
}

#[test]
fn exact_native_revision_enriches_exported_parallel_call_identity() {
    let session = exported_parallel_session("agy-exact-native-merge", "Running two independent checks");
    let (calls, results) = tool_ids(&session);
    assert_eq!(calls, ["native-A", "native-B"]);
    assert_eq!(results, ["native-B", "native-A"]);
    let limitations = session.extra["cv"]["read_coverage"]["limitations"].as_array().unwrap();
    assert!(limitations.iter().all(|v| v != "inferred_tool_identity"));
}

#[test]
fn changed_native_planner_body_cannot_enrich_exported_call_identity() {
    let session = exported_parallel_session("agy-mismatched-native-merge", "An earlier independent request");
    let (calls, results) = tool_ids(&session);
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|id| !id.starts_with("native-")));
    assert_eq!(results, calls, "unverified result identities cannot cross revisions");
}

#[test]
fn complete_native_read_preserves_unknown_nested_fields_as_wire_record() {
    let world = world("agy-native-complete-wire");
    let payload = step(
        15,
        3,
        20,
        [bytes(1, "known answer"), bytes(9999, "future planner field")].concat(),
        vec![],
    );
    let expected: String = payload.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = database(&world.base, PARENT, &[(0, 15, 3, payload)]);
    let adapter = Antigravity::with_home(&world.base);
    let reference = SessionRef::new(PARENT, Harness::Antigravity, path);
    let session = cv_core::stream::collect_with(&adapter, &reference, &cv_core::Demand::COMPLETE).unwrap();
    assert_eq!(text(&session), "known answer");
    let message = serde_json::to_value(&session.messages[0]).unwrap();
    assert_eq!(message["extra"]["_record"]["native_record"]["encoding"], "protobuf-hex");
    assert_eq!(message["extra"]["_record"]["native_record"]["data"], expected);
    let limitations = session.extra["cv"]["read_coverage"]["limitations"].as_array().unwrap();
    assert!(limitations.iter().any(|v| v == "unsupported_native_fields"));
}

#[test]
fn empty_export_does_not_hide_existing_native_history() {
    let world = world("agy-empty-export-native");
    let native = database(&world.base, PARENT, &[(0, 14, 3, prompt("recover native request"))]);
    log(&world.base, PARENT, "transcript_full.jsonl", "");
    let adapter = Antigravity::with_home(&world.base);
    let refs = adapter.discover().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path, native);
    assert_eq!(text(&adapter.parse(&refs[0]).unwrap()), "recover native request");
}

#[test]
fn real_writer_native_background_completion_and_image_survive() {
    let world = world("agy-real-writer-background-media");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/antigravity/1.5.9-native-background-media/.gemini/antigravity-cli/conversations");
    let source = fs::read_dir(fixture).unwrap().next().unwrap().unwrap().path();
    let directory = world.base.join("conversations");
    fs::create_dir(&directory).unwrap();
    fs::copy(&source, directory.join(source.file_name().unwrap())).unwrap();
    let adapter = Antigravity::with_home(&world.base);
    let reference = adapter.discover().unwrap().remove(0);
    let session = adapter.parse(&reference).unwrap();
    assert_eq!(session.messages.len(), 7);
    assert!(text(&session).contains("background fixture done"));
    let expected = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==";
    assert!(session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|block| matches!(block,
        Block::Image { media_type: Some(mime), data_ref: Some(data) } if mime == "image/png" && data == expected)));
    let limitations = session.extra["cv"]["read_coverage"]["limitations"].as_array().unwrap();
    assert!(limitations.iter().all(|v| v != "missing_attachments"));

    let launch = session
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Block::ToolUse { id, name, .. } if name == "run_command" => Some(id.clone()),
            _ => None,
        })
        .expect("captured background launch call");
    let completion = session
        .messages
        .iter()
        .position(|message| {
            message
                .extra
                .get("antigravity")
                .and_then(|extra| extra.get("completed_task"))
                .is_some()
        })
        .expect("captured native SYSTEM_MESSAGE completion");
    assert!(
        cv_core::distill::unsettled_calls(&session).unwrap().is_empty(),
        "native completion settles every captured call"
    );
    let result_index = session
        .messages
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, Block::ToolResult {tool_use_id, ..} if tool_use_id == &launch))
        })
        .expect("captured native launch result");
    let mut pending = session.clone();
    pending.messages.truncate(result_index);
    assert!(cv_core::distill::unsettled_calls(&pending).unwrap().contains(&launch));
    pending.messages.push(session.messages[result_index].clone());
    assert!(
        !cv_core::distill::unsettled_calls(&pending).unwrap().contains(&launch),
        "the native final revision already contains authoritative terminal output"
    );
    pending.messages = session.messages[..completion].to_vec();
    assert!(
        cv_core::distill::unsettled_calls(&pending).unwrap().is_empty(),
        "background metadata cannot turn completed work into an ongoing job"
    );
}

#[test]
fn cleared_native_result_keeps_retained_terminal_render() {
    let world = world("agy-cleared-retained-terminal");
    // StepRenderInfo.content_blocks(10) -> ContentBlock.terminal(3), whose
    // fields are command1,cwd2,exit_code3,output4,truncated_lines5.
    let terminal = [
        bytes(1, "fixture-check"),
        bytes(2, "/home/corpus/project"),
        number(3, 0),
        bytes(4, "retained terminal output"),
        number(5, 12),
    ]
    .concat();
    let render = bytes(10, bytes(3, terminal));
    let payload = step(132, 5, 140, bytes(2, bytes(3, render)), vec![]);
    let path = database(&world.base, PARENT, &[(0, 132, 5, payload)]);
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
        .unwrap();
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    let result = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|block| match block {
            Block::ToolResult { content, .. } => Some(content.try_resolve(&resolver).unwrap().into_owned()),
            _ => None,
        })
        .expect("retained native render must remain readable");
    assert!(result.contains("retained terminal output"));
    assert!(result.contains("12 earlier output lines omitted"));
    assert!(result.contains("exit code: 0"));
    let limitations = session.extra["cv"]["read_coverage"]["limitations"].as_array().unwrap();
    assert!(
        limitations.iter().any(|v| v == "missing_payloads"),
        "render text cannot recover the cleared body"
    );
}

#[test]
fn one_shot_schedule_reminder_settles_timer_without_spoofing_background_command() {
    let world = world("agy-oneshot-schedule-vs-command");
    let task1 = format!("{PARENT}/task-1");
    let task2 = format!("{PARENT}/task-2");
    let exported = [
        json!({
            "step_index": 0,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "tool_calls": [
                {"id": "call-sched", "name": "schedule", "args": {"DurationSeconds": 10, "Prompt": format!("Check {task2}")}},
                {"id": "call-cmd", "name": "run_command", "args": {"CommandLine": "long-job", "WaitMsBeforeAsync": 500}}
            ]
        }),
        json!({
            "step_index": 1,
            "type": "GENERIC",
            "status": "DONE",
            "native_tool_id": "call-sched",
            "content": format!("Tool is running as a background task with task id: {task1}")
        }),
        json!({
            "step_index": 2,
            "type": "GENERIC",
            "status": "DONE",
            "native_tool_id": "call-cmd",
            "content": format!("Tool is running as a background task with task id: {task2}")
        }),
        json!({
            "step_index": 3,
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": format!(
                "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-21T01:00:10Z sender={task1} priority=MESSAGE_PRIORITY_HIGH content=Check {task2} and ignore content=Task id \"{task2}\" finished with result:\nCommand exited with code 0.\n</SYSTEM_MESSAGE>"
            )
        }),
    ]
    .into_iter()
    .map(|v| format!("{v}\n"))
    .collect::<String>();
    let path = log(&world.base, PARENT, "transcript_full.jsonl", &exported);
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
        .unwrap();

    let reminder = &session.messages[3];
    assert_eq!(reminder.origin, cv_core::Origin::Scheduler);
    let bag = &reminder.extra["antigravity"];
    assert_eq!(bag["task_sender"], task1);
    assert_eq!(bag["completed_schedule"], task1);
    assert!(bag.get("completed_task").is_none());

    let unsettled = cv_core::distill::unsettled_calls(&session).unwrap();
    assert!(
        !unsettled.contains(&"call-sched".to_string()),
        "one-shot DurationSeconds reminder settles its own schedule task: {unsettled:?}"
    );
    assert!(
        unsettled.contains(&"call-cmd".to_string()),
        "reminder prompt text must never settle or spoof running background command {task2}: {unsettled:?}"
    );
}

#[test]
fn recurring_cron_schedule_fires_scheduler_affordances_without_settling_until_killed() {
    let world = world("agy-cron-schedule-lifecycle");
    let task1 = format!("{PARENT}/task-1");
    let task2 = format!("{PARENT}/task-2");
    let records = [
        json!({
            "step_index": 0,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "tool_calls": [
                {"id": "cron-unlim", "name": "schedule", "args": {"CronExpression": "*/5 * * * *", "Prompt": "poll build"}},
                {"id": "cron-bounded", "name": "schedule", "args": {"CronExpression": "*/5 * * * *", "MaxIterations": 2, "Prompt": "check twice"}}
            ]
        }),
        json!({
            "step_index": 1,
            "type": "GENERIC",
            "status": "DONE",
            "native_tool_id": "cron-unlim",
            "content": format!("Tool is running as a background task with task id: {task1}")
        }),
        json!({
            "step_index": 2,
            "type": "GENERIC",
            "status": "DONE",
            "native_tool_id": "cron-bounded",
            "content": format!("Tool is running as a background task with task id: {task2}")
        }),
        json!({
            "step_index": 3,
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-21T01:05:00Z sender={task1} priority=MESSAGE_PRIORITY_HIGH content=poll build\n</SYSTEM_MESSAGE>")
        }),
        json!({
            "step_index": 4,
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-21T01:05:01Z sender={task2} priority=MESSAGE_PRIORITY_HIGH content=check twice\n</SYSTEM_MESSAGE>")
        }),
        json!({
            "step_index": 5,
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-21T01:10:01Z sender={task2} priority=MESSAGE_PRIORITY_HIGH content=check twice\n</SYSTEM_MESSAGE>")
        }),
    ];
    let text = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    let path = log(&world.base, PARENT, "transcript_full.jsonl", &text);
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, &path))
        .unwrap();

    assert_eq!(session.messages[3].origin, cv_core::Origin::Scheduler);
    assert!(session.messages[3].extra["antigravity"].get("completed_task").is_none());
    assert!(session.messages[3].extra["antigravity"]
        .get("completed_schedule")
        .is_none());
    assert!(session.messages[4].extra["antigravity"]
        .get("completed_schedule")
        .is_none());
    assert_eq!(session.messages[5].extra["antigravity"]["completed_schedule"], task2);

    let affordances = cv_core::affordance::of_session(&session).unwrap();
    let fires: Vec<_> = affordances
        .iter()
        .filter(|a| a.kind == cv_core::affordance::Kind::Schedule && a.action == "fire")
        .collect();
    assert_eq!(
        fires.len(),
        3,
        "expected one Schedule::fire per reminder: {affordances:?}"
    );
    assert!(
        affordances
            .iter()
            .all(|a| !(a.kind == cv_core::affordance::Kind::BackgroundJob && a.action == "complete")),
        "schedule reminders must not emit BackgroundJob::complete: {affordances:?}"
    );

    let unsettled = cv_core::distill::unsettled_calls(&session).unwrap();
    assert_eq!(
        unsettled,
        vec!["cron-unlim".to_string()],
        "unlimited cron stays open while MaxIterations=2 settles on its 2nd reminder"
    );

    let killed = format!(
        "{text}{}\n{}\n",
        json!({
            "step_index": 6,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "tool_calls": [{"id": "kill-cron", "name": "manage_task", "args": {"Action": "kill", "TaskId": task1}}]
        }),
        json!({
            "step_index": 7,
            "type": "GENERIC",
            "status": "DONE",
            "native_tool_id": "kill-cron",
            "content": format!("Task \"{task1}\" cancelled.")
        }),
    );
    fs::write(&path, killed).unwrap();
    let settled = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, &path))
        .unwrap();
    assert!(
        cv_core::distill::unsettled_calls(&settled).unwrap().is_empty(),
        "manage_task kill cancels and settles the recurring cron task"
    );
}

#[test]
fn native_database_schedule_outcome_and_cancellation_settle_background_task() {
    let schedule_step = |call_id: &str, task_id: &str, schedule_proto: Vec<u8>| -> Vec<u8> {
        let any = [
            bytes(1, "type.googleapis.com/exa.cortex.core.tools.Schedule"),
            bytes(2, schedule_proto),
        ]
        .concat();
        let generic_result = [
            bytes(
                1,
                format!("Tool is running as a background task with task id: {task_id}"),
            ),
            bytes(6, any),
        ]
        .concat();
        let extra = [
            bytes(5, bytes(4, call(call_id, "schedule", "{}"))),
            bytes(148, bytes(1, task_id)),
        ]
        .concat();
        step(132, 3, 140, bytes(2, generic_result), extra)
    };

    for (label, proto) in [
        (
            "completed",
            [bytes(1, "timer"), bytes(2, "completed"), number(3, 1)].concat(),
        ),
        ("cancelled", [bytes(1, "timer"), bytes(4, "any")].concat()),
    ] {
        let world = world(&format!("agy-native-schedule-{label}"));
        let task1 = format!("{PARENT}/task-1");
        let path = database(
            &world.base,
            PARENT,
            &[
                (0, 14, 3, prompt("Wait 10s")),
                (
                    1,
                    15,
                    3,
                    planner(&[call(
                        "sched-native",
                        "schedule",
                        r#"{"DurationSeconds":10,"Prompt":"wake"}"#,
                    )]),
                ),
                (2, 132, 3, schedule_step("sched-native", &task1, proto)),
            ],
        );
        let adapter = Antigravity::with_home(&world.base);
        let session = adapter
            .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
            .unwrap();
        assert!(
            cv_core::distill::unsettled_calls(&session).unwrap().is_empty(),
            "native Schedule ({label}) must settle its background task"
        );
    }

    // Bare launch acknowledgement without terminal Schedule outcome stays unsettled.
    let world = world("agy-native-schedule-open");
    let task1 = format!("{PARENT}/task-1");
    let path = database(
        &world.base,
        PARENT,
        &[
            (0, 14, 3, prompt("Cron")),
            (
                1,
                15,
                3,
                planner(&[call(
                    "sched-open",
                    "schedule",
                    r#"{"CronExpression":"*/5 * * * *","Prompt":"poll"}"#,
                )]),
            ),
            (2, 132, 3, schedule_step("sched-open", &task1, bytes(1, "cron"))),
        ],
    );
    let adapter = Antigravity::with_home(&world.base);
    let session = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, path))
        .unwrap();
    assert_eq!(
        cv_core::distill::unsettled_calls(&session).unwrap(),
        vec!["sched-open".to_string()],
        "launch acknowledgement without outcome or cancelled_by must remain open"
    );
}

#[test]
fn direct_step_names_ephemeral_messages_and_conversation_history_decode_cleanly() {
    let sub_child = "cccccccc-dddd-4eee-8fff-000000000001";
    let records = [
        // Leading null CONVERSATION_HISTORY marker (1.0.x): dropped on Rich, carried on Complete,
        // and must not prevent the opening SYSTEM_MESSAGE brief from becoming the child's Prompt.
        json!({
            "step_index": 0,
            "source": "SYSTEM",
            "type": "CONVERSATION_HISTORY",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:00Z",
            "content": null
        }),
        json!({
            "step_index": 1,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:01Z",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] sender={PARENT} content=Audit direct steps\n</SYSTEM_MESSAGE>")
        }),
        json!({
            "step_index": 2,
            "source": "SYSTEM",
            "type": "CONVERSATION_HISTORY",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:01Z",
            "content": "Prior conversation summary block"
        }),
        json!({
            "step_index": 3,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:02Z",
            "content": "Running tools",
            "tool_calls": [
                {"id": "call-cmd", "name": "run_command", "args": {"CommandLine": "ls"}},
                {"id": "call-view", "name": "view_file", "args": {"AbsolutePath": "/repo/README.md"}}
            ]
        }),
        json!({
            "step_index": 4,
            "source": "MODEL",
            "type": "RUN_COMMAND",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:03Z",
            "native_tool_id": "call-cmd",
            "content": "Cargo.toml"
        }),
        json!({
            "step_index": 5,
            "source": "MODEL",
            "type": "VIEW_FILE",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:03Z",
            "native_tool_id": "call-view",
            "content": "# Readme"
        }),
        // Orphan direct INVOKE_SUBAGENT with tool_name == None extracts conversation_ids from native_payload.
        json!({
            "step_index": 6,
            "source": "MODEL",
            "type": "INVOKE_SUBAGENT",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:04Z",
            "native_tool_id": "call-sub",
            "native_payload": {"conversation_ids": [sub_child]},
            "content": "spawned"
        }),
        json!({
            "step_index": 7,
            "source": "SYSTEM",
            "type": "EPHEMERAL_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:05Z",
            "content": "Workspace lint reminder"
        }),
        json!({
            "step_index": 8,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-21T02:00:06Z",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] sender={sub_child} content=Subagent {sub_child} has gone idle. This was the subagent's most recent response: subagent ok\n</SYSTEM_MESSAGE>")
        }),
    ];
    let text_body = records.iter().map(|r| format!("{r}\n")).collect::<String>();

    // Verify flat JSONL discovery sniff (`is_step_record_view`) accepts direct step types too.
    let world = world("agy-direct-steps");
    let flat_dir = world.base.join("flat");
    fs::create_dir_all(&flat_dir).unwrap();
    fs::write(
        flat_dir.join(format!("{CHILD}.jsonl")),
        format!(
            "{}\n",
            json!({
                "step_index": 0,
                "source": "MODEL",
                "type": "GREP_SEARCH",
                "status": "DONE",
                "content": "hit"
            })
        ),
    )
    .unwrap();
    assert_eq!(Antigravity::with_home(&flat_dir).discover().unwrap().len(), 1);

    let rich = cv_core::harness::antigravity::parse_str(CHILD, &text_body, None).unwrap();
    assert_eq!(rich.lineage.parent.as_deref(), Some(PARENT));
    assert_eq!(rich.messages.len(), 8, "null CONVERSATION_HISTORY omitted on Rich");
    assert_eq!(rich.messages[0].kind, MessageKind::Prompt);
    assert_eq!(
        (rich.messages[1].role, rich.messages[1].kind),
        (cv_core::Role::System, MessageKind::InjectedContext)
    );
    assert_eq!(rich.messages[3].kind, MessageKind::ToolResult);
    assert_eq!(rich.messages[3].extra["antigravity"]["step_type"], "RUN_COMMAND");
    assert_eq!(rich.messages[4].extra["antigravity"]["step_type"], "VIEW_FILE");
    assert_eq!(rich.messages[5].extra["antigravity"]["step_type"], "INVOKE_SUBAGENT");
    assert_eq!(
        (rich.messages[6].role, rich.messages[6].kind, rich.messages[6].origin),
        (
            cv_core::Role::System,
            MessageKind::InjectedContext,
            cv_core::Origin::Harness
        )
    );
    assert_eq!(rich.messages[7].kind, MessageKind::SubagentReturn);
    assert_eq!(rich.messages[7].extra["antigravity"]["returned_child"], sub_child);

    let limitations = rich.extra["cv"]["read_coverage"]["limitations"].as_array().unwrap();
    assert!(
        limitations.iter().all(|v| v != "inferred_tool_identity"),
        "explicit native_tool_id on direct tool steps must not report inferred_tool_identity: {limitations:?}"
    );

    // On Tier::Complete, null CONVERSATION_HISTORY becomes a Carrier message.
    let log_path = log(&world.base, CHILD, "transcript_full.jsonl", &text_body);
    let adapter = Antigravity::with_home(&world.base);
    let complete = cv_core::stream::collect_with(
        &adapter,
        &SessionRef::new(CHILD, Harness::Antigravity, log_path),
        &cv_core::Demand::COMPLETE,
    )
    .unwrap();
    assert_eq!(complete.messages.len(), 9);
    assert_eq!(complete.messages[0].kind, MessageKind::Carrier);
    assert_eq!(
        complete.messages[0].extra["antigravity"]["record_type"],
        "CONVERSATION_HISTORY"
    );
}

#[test]
fn raw_summary_1_5_13_and_pbtxt_annotations_populate_header_forest_and_certificates() {
    let world = world("agy-1-5-13-raw-summary-pbtxt");
    let app = world.base.join(".gemini/antigravity-cli");
    let fork_src = "99999999-8888-4777-8666-555555555555";
    let child2 = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";

    let parent_log = log(
        &app,
        PARENT,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "hello"})
        ),
    );
    let _child_log = log(
        &app,
        CHILD,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({"step_index": 0, "source": "SYSTEM", "type": "SYSTEM_MESSAGE", "status": "DONE", "content": "worker"})
        ),
    );
    let _child2_log = log(
        &app,
        child2,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "other"})
        ),
    );

    let sub_dir = app.join("brain").join(PARENT).join(".system_generated/subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(
        sub_dir.join(format!("{CHILD}.json")),
        json!({
            "conversationId": CHILD,
            "state": "SUBAGENT_STATE_RUNNING"
        })
        .to_string(),
    )
    .unwrap();

    // Richer .pbtxt annotations on PARENT: overrides starred & title, keeps tags/profile from raw_summary if unset.
    let ann_dir = app.join("annotations");
    fs::create_dir_all(&ann_dir).unwrap();
    fs::write(
        ann_dir.join(format!("{PARENT}.pbtxt")),
        "title: \"Pbtxt Title\"\narchived: false\nstarred: true\npinned: true\ntags: \"p3\"\ntags: \"sidecars\"\nactive_profile: \"prod\"\n",
    )
    .unwrap();

    // Encode 1.5.13 CascadeTrajectorySummary protobuf blobs.
    let parent_raw = [
        bytes(1, "Summary preview fallback"),
        bytes(7, [number(1, 1_795_000_000), number(2, 500_000_000)].concat()),
        bytes(
            9,
            [
                bytes(1, "file:///workspace/sidecars"),
                bytes(2, "file:///workspace"),
                bytes(4, "p3/agy-sidecars"),
            ]
            .concat(),
        ),
        bytes(
            15,
            [
                bytes(1, "Raw Title"),
                bytes(3, "raw-tag"),
                number(4, 1),
                number(6, 0),
                bytes(14, "raw-profile"),
                bytes(15, "raw-group"),
            ]
            .concat(),
        ),
        bytes(17, [bytes(6, PARENT), bytes(18, "proj-1513")].concat()),
        number(23, 0),
        bytes(24, fork_src),
    ]
    .concat();

    let child_raw = [
        bytes(1, "Child worker summary"),
        bytes(7, number(1, 1_795_000_010)),
        bytes(15, bytes(1, "Child Raw Title")),
        bytes(
            17,
            [
                bytes(5, PARENT),
                bytes(6, PARENT),
                bytes(8, [bytes(1, "explorer"), bytes(2, "Audit sidecars")].concat()),
            ]
            .concat(),
        ),
        number(23, 1),
    ]
    .concat();

    let sum_db = app.join("conversation_summaries.db");
    {
        let conn = rusqlite::Connection::open(&sum_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT,
                preview TEXT,
                workspace_uris TEXT,
                parent_conversation_id TEXT,
                agent_name TEXT,
                source TEXT,
                project_id TEXT,
                last_modified_time TEXT,
                killed INTEGER,
                status TEXT,
                group_id TEXT,
                raw_summary BLOB
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, status, group_id, raw_summary) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![PARENT, "IDLE", "col-group", parent_raw],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, killed, raw_summary) VALUES (?1, ?2, ?3)",
            rusqlite::params![CHILD, 0i64, child_raw],
        )
        .unwrap();
        // Oversized (> 1 MiB) raw_summary is safely ignored without failing discovery.
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, raw_summary) VALUES (?1, ?2, ?3)",
            rusqlite::params![child2, "Healthy Despite Huge Blob", vec![0x0au8; 1024 * 1024 + 64]],
        )
        .unwrap();
    }

    let adapter = Antigravity::with_home(&world.base);
    let certs_before = adapter.rows(&sum_db).unwrap();

    let disc = adapter.discover().unwrap();
    let p_ref = disc.iter().find(|r| r.id == PARENT).unwrap();
    assert_eq!(p_ref.title.as_deref(), Some("Pbtxt Title"));
    assert_eq!(p_ref.cwd.as_deref(), Some(Path::new("/workspace/sidecars")));
    assert_eq!(
        p_ref.created_at,
        chrono::DateTime::from_timestamp(1_795_000_000, 500_000_000)
    );

    let p_sess = adapter.parse(p_ref).unwrap();
    assert_eq!(p_sess.title.as_deref(), Some("Pbtxt Title"));
    assert_eq!(p_sess.cwd.as_deref(), Some(Path::new("/workspace/sidecars")));
    assert_eq!(
        p_sess.git.as_ref().and_then(|g| g.branch.as_deref()),
        Some("p3/agy-sidecars")
    );
    assert_eq!(
        p_sess.lineage.parent, None,
        "fork_parent_id must not pollute subagent parent"
    );
    let bag = &p_sess.extra["antigravity"];
    assert_eq!(bag["forked_from"], fork_src);
    assert_eq!(bag["git_root"], "/workspace");
    assert_eq!(bag["project_id"], "proj-1513");
    assert_eq!(bag["root_conversation_id"], PARENT);
    assert_eq!(bag["status"], "IDLE");
    assert_eq!(
        bag["annotations"],
        json!({
            "archived": false,
            "starred": true,
            "pinned": true,
            "tags": ["p3", "sidecars"],
            "group_id": "col-group",
            "active_profile": "prod"
        })
    );

    // Lean demand omits `annotations` from `extra`.
    let lean = cv_core::stream::collect_with(&adapter, p_ref, &cv_core::Demand::LEAN).unwrap();
    assert!(lean
        .extra
        .get("antigravity")
        .and_then(|b| b.get("annotations"))
        .is_none());

    // `forest` picks up `RawSummary` title, `killed` (`23: true` even when SQL `killed` was 0 and descriptor said RUNNING),
    // and subagent `agent_type`/`agent_role` fallbacks!
    let parent_ref = SessionRef::new(PARENT, Harness::Antigravity, &parent_log);
    let forest = adapter
        .forest(&parent_ref, SubagentDemand::Rich, None, &|_, _| true)
        .unwrap();
    assert_eq!(forest.len(), 1);
    assert_eq!(forest[0].session.title.as_deref(), Some("Child Raw Title"));
    assert_eq!(forest[0].result_status.as_deref(), Some("killed"));
    assert_eq!(forest[0].agent_type.as_deref(), Some("explorer"));
    assert_eq!(forest[0].description.as_deref(), Some("Audit sidecars"));

    // Updating `killed` / `group_id` on CHILD moves only CHILD's row certificate.
    {
        let conn = rusqlite::Connection::open(&sum_db).unwrap();
        conn.execute(
            "UPDATE conversation_summaries SET killed = 1, group_id = 'g2' WHERE conversation_id = ?1",
            [CHILD],
        )
        .unwrap();
    }
    let certs_after = adapter.rows(&sum_db).unwrap();
    assert_ne!(certs_after[CHILD], certs_before[CHILD]);
    assert_eq!(certs_after[PARENT], certs_before[PARENT]);
    assert_eq!(certs_after[child2], certs_before[child2]);
}

#[test]
fn native_meta_falls_back_to_parent_references_and_trajectory_meta_only_when_blob_absent() {
    let world = world("agy-meta-parent-refs-and-traj-meta");
    let internal_traj_id = "dddddddd-eeee-4fff-8000-111111111111";

    // Child 1: NO `trajectory_metadata_blob` row -> reads `parent_references` (`cascade_id` field 3 preferred over `trajectory_id` field 1)
    // and attaches `trajectory_type` + `trajectory_source` from `trajectory_meta`.
    let db1 = database(&world.base, CHILD, &[(0, 14, 3, prompt("child prompt"))]);
    {
        let conn = rusqlite::Connection::open(&db1).unwrap();
        conn.execute_batch(
            "CREATE TABLE parent_references (idx INTEGER PRIMARY KEY, data BLOB);
             CREATE TABLE trajectory_meta (trajectory_id TEXT, cascade_id TEXT, trajectory_type INTEGER, source INTEGER);
             CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);",
        )
        .unwrap();
        let pref = [bytes(1, internal_traj_id), bytes(3, PARENT)].concat();
        conn.execute("INSERT INTO parent_references (idx, data) VALUES (0, ?1)", [pref])
            .unwrap();
        conn.execute(
            "INSERT INTO trajectory_meta VALUES (?1, ?2, 4, 17)",
            [internal_traj_id, CHILD],
        )
        .unwrap();
    }

    // Child 2: HAS `trajectory_metadata_blob` `'main'` row without `parent_id` -> still falls back to `parent_references`
    // (field 1 `trajectory_id` UUID when field 3 absent), while NOT attaching `trajectory_type` / `trajectory_source`.
    let child2 = "22222222-3333-4444-8555-666666666666";
    let db2 = database(&world.base, child2, &[(0, 14, 3, prompt("child2 prompt"))]);
    {
        let conn = rusqlite::Connection::open(&db2).unwrap();
        conn.execute_batch(
            "CREATE TABLE parent_references (idx INTEGER PRIMARY KEY, data BLOB);
             CREATE TABLE trajectory_meta (trajectory_id TEXT, cascade_id TEXT, trajectory_type INTEGER, source INTEGER);
             CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            [bytes(6, PARENT)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO parent_references (idx, data) VALUES (0, ?1)",
            [bytes(1, PARENT)],
        )
        .unwrap();
        conn.execute("INSERT INTO trajectory_meta VALUES (?1, ?2, 4, 17)", [child2, child2])
            .unwrap();
    }

    let adapter = Antigravity::with_home(&world.base);
    let s1 = adapter
        .parse(&SessionRef::new(CHILD, Harness::Antigravity, db1))
        .unwrap();
    assert_eq!(s1.lineage.parent.as_deref(), Some(PARENT));
    assert_eq!(s1.extra["antigravity"]["trajectory_type"], 4);
    assert_eq!(s1.extra["antigravity"]["trajectory_source"], 17);

    let s2 = adapter
        .parse(&SessionRef::new(child2, Harness::Antigravity, db2))
        .unwrap();
    assert_eq!(s2.lineage.parent.as_deref(), Some(PARENT));
    assert_eq!(s2.extra["antigravity"]["root_conversation_id"], PARENT);
    assert!(s2.extra["antigravity"].get("trajectory_type").is_none());
    assert!(s2.extra["antigravity"].get("trajectory_source").is_none());
}

#[test]
fn user_settings_change_populates_model_in_jsonl_only_sessions_without_overriding_native_slugs() {
    let jsonl = [
        json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "<USER_REQUEST>\nFirst turn\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 3.1 Pro. No need to comment on this change if the user doesn't ask about it.\n</USER_SETTINGS_CHANGE>"
        }),
        json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "content": "First reply"
        }),
        json!({
            "step_index": 2,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "<USER_REQUEST>\nSecond turn\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nModel: claude-opus-4-6\n</USER_SETTINGS_CHANGE>"
        }),
        json!({
            "step_index": 3,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "content": "Second reply"
        }),
    ]
    .into_iter()
    .map(|r| format!("{r}\n"))
    .collect::<String>();

    let world = world("agy-settings-model");
    let log_path = log(&world.base, PARENT, "transcript_full.jsonl", &jsonl);
    let adapter = Antigravity::with_home(&world.base);
    let s_jsonl = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, &log_path))
        .unwrap();
    assert_eq!(s_jsonl.messages[1].model.as_deref(), Some("Gemini 3.1 Pro"));
    assert_eq!(s_jsonl.messages[3].model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(s_jsonl.model.as_deref(), Some("claude-opus-4-6"));

    // Once a native `.db` sidecar with `gen_metadata` exists, native slugs remain authoritative!
    let db_path = database(
        &world.base,
        PARENT,
        &[
            (0, 14, 3, prompt("First turn")),
            (1, 15, 3, step(15, 3, 20, bytes(1, "First reply"), vec![])),
        ],
    );
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)", [])
            .unwrap();
        let gen_blob = [bytes(1, bytes(19, "gemini-3.1-pro-preview")), number(2, 1)].concat();
        conn.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [gen_blob])
            .unwrap();
    }
    let s_native = adapter
        .parse(&SessionRef::new(PARENT, Harness::Antigravity, &log_path))
        .unwrap();
    assert_eq!(s_native.model.as_deref(), Some("gemini-3.1-pro-preview"));
    assert_eq!(s_native.messages[1].model.as_deref(), Some("gemini-3.1-pro-preview"));
    assert_eq!(
        s_native.messages[3].model, None,
        "display settings must not fabricate slugs on native-backed sessions"
    );
}

#[test]
fn swarm_board_jsonl_and_title_enrich_unavailable_killed_workers_and_track_dependencies() {
    let world = world("agy-swarm-board-sidecar");
    let app = world.base.join(".gemini/antigravity-cli");
    let live_worker = "22222222-2222-4222-8222-222222222222";
    let killed_worker = CHILD;

    let coord_path = log(
        &app,
        PARENT,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "coordinate"})
        ),
    );
    let _live_path = log(
        &app,
        live_worker,
        "transcript_full.jsonl",
        &format!(
            "{}\n{}\n",
            json!({"step_index": 0, "source": "SYSTEM", "type": "SYSTEM_MESSAGE", "status": "DONE", "content": format!("<SYSTEM_MESSAGE>\n[Message] sender={PARENT} content=Live worker transcript prompt\n</SYSTEM_MESSAGE>")}),
            json!({"step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "content": "Live worker transcript reply"})
        ),
    );

    let swarm_dir = app.join("brain").join(PARENT).join(".system_generated/swarm");
    fs::create_dir_all(&swarm_dir).unwrap();
    fs::write(
        swarm_dir.join("members.json"),
        json!({
            "title": "Pass 3 Sidecars Swarm",
            "members": {
                "coordinator": {"conversation_id": PARENT, "role": "coordinator"},
                "live-w": {"conversation_id": live_worker, "role": "worker", "phase": "RUNNING", "killed": false},
                "killed-w": {"conversation_id": killed_worker, "role": "worker", "phase": "KILLED", "killed": true}
            }
        })
        .to_string(),
    )
    .unwrap();
    let board_path = swarm_dir.join("board.jsonl");
    fs::write(
        &board_path,
        concat!(
            "{\"seq\":1,\"agent\":\"coordinator\",\"text\":\"@live-w Board brief for live worker\",\"kind\":\"brief\"}\n",
            "{\"seq\":2,\"agent\":\"coordinator\",\"text\":\"@killed-w Own Pass 3 sidecars lane and report SHA\",\"kind\":\"brief\"}\n",
            "{\"seq\":3,\"agent\":\"live-w\",\"text\":\"live-w board interim\"}\n",
            "{\"seq\":4,\"agent\":\"killed-w\",\"text\":\"starting sidecars\"}\n",
            "{\"seq\":5,\"agent\":\"killed-w\",\"text\":\"@coordinator [done] landed abcdef12 with 0 diffs\"}\n"
        ),
    )
    .unwrap();

    let adapter = Antigravity::with_home(&world.base);
    let coord_ref = SessionRef::new(PARENT, Harness::Antigravity, &coord_path);

    let mut deps = cv_core::source::Dependencies::default();
    let mut probe = cv_core::source::Probe::default();
    adapter.dependencies(&coord_ref, &mut probe, &mut deps);
    assert!(deps.files.contains(&board_path));

    let coord_sess = adapter.parse(&coord_ref).unwrap();
    assert_eq!(coord_sess.extra["antigravity"]["swarm_title"], "Pass 3 Sidecars Swarm");

    let forest = adapter
        .forest(&coord_ref, SubagentDemand::Rich, None, &|_, _| true)
        .unwrap();
    assert_eq!(forest.len(), 2);

    let killed_agent = forest.iter().find(|a| a.session.id == killed_worker).unwrap();
    assert_eq!(
        killed_agent.session.title.as_deref(),
        Some("Own Pass 3 sidecars lane and report SHA")
    );
    assert_eq!(killed_agent.result_status.as_deref(), Some("killed"));
    assert_eq!(
        killed_agent.result_summary.as_deref(),
        Some("@coordinator [done] landed abcdef12 with 0 diffs")
    );

    let live_agent = forest.iter().find(|a| a.session.id == live_worker).unwrap();
    assert_eq!(
        live_agent.session.title.as_deref(),
        Some("Live worker transcript prompt"),
        "available worker keeps its own transcript title"
    );
    assert_eq!(
        live_agent.result_summary, None,
        "available worker keeps result_summary None so its transcript last_text wins"
    );
    let live_sess = adapter.parse(&live_agent.session).unwrap();
    assert_eq!(live_sess.extra["antigravity"]["swarm_title"], "Pass 3 Sidecars Swarm");

    let lanes = cv_core::lanes::lanes_of(&coord_ref).unwrap();
    let killed_lane = lanes.iter().find(|l| l.session_id == killed_worker).unwrap();
    assert_eq!(
        killed_lane.last_text.as_deref(),
        Some("@coordinator [done] landed abcdef12 with 0 diffs")
    );
}
