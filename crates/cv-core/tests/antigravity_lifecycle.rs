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
