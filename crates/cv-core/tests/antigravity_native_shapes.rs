//! Native row shapes that are common in real 1.5.9 stores and absent from the earlier
//! synthetic cases: cleared rows that keep only `Step.type`, and typed tool results.
//! Rows are encoded here from the 1.5.9 field descriptors; all text is synthetic.
#![cfg(feature = "sqlite")]

use cv_core::coverage::ReadCoverage;
use cv_core::harness::{antigravity::Antigravity, Parse};
use cv_core::{Adapter, Block, MessageKind, Session};
use cv_testkit::World;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const ID: &str = "11111111-2222-4333-8444-555555555555";

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
fn call(id: &str, name: &str) -> Vec<u8> {
    [bytes(1, id), bytes(2, name), bytes(3, "{}")].concat()
}
/// Step: type(1), status(4), metadata(5){created_at(1), tool_call(4)}, then its member.
fn step(ty: u64, status: u64, tool_call: Option<&[u8]>, member: Option<(u64, Vec<u8>)>) -> Vec<u8> {
    let mut metadata = bytes(1, number(1, 1_700_000_000));
    if let Some(tool_call) = tool_call {
        metadata.extend(bytes(4, tool_call));
    }
    let mut out = [number(1, ty), number(4, status), bytes(5, metadata)].concat();
    if let Some((field, payload)) = member {
        out.extend(bytes(field, payload));
    }
    out
}
fn planner(calls: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = calls.iter().flat_map(|call| bytes(7, call)).collect();
    step(15, 3, None, Some((20, [bytes(1, "Working"), payload].concat())))
}
/// GenericStepResult: result(1), payload(6) = Any{type_url(1), value(2)}.
fn typed_result(call: &[u8], body: &str, url: &str, value: Vec<u8>) -> Vec<u8> {
    let any = [bytes(1, format!("type.googleapis.com/{url}")), bytes(2, value)].concat();
    let result = [bytes(1, body), bytes(6, any)].concat();
    step(132, 3, Some(call), Some((140, bytes(2, result))))
}

fn store(root: &Path, rows: &[Vec<u8>]) {
    let conversations = root.join("conversations");
    fs::create_dir_all(&conversations).unwrap();
    let conn = rusqlite::Connection::open(conversations.join(format!("{ID}.db"))).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    for (index, payload) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO steps(idx, step_payload) VALUES (?1, ?2)",
            rusqlite::params![index as i64, payload],
        )
        .unwrap();
    }
}
fn read(root: &Path) -> Session {
    let adapter = Antigravity::with_home(root);
    let reference = adapter.discover().unwrap().remove(0);
    adapter.parse(&reference).unwrap()
}
fn results(session: &Session) -> Vec<(&str, Option<&Value>)> {
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            Block::ToolResult {
                tool_use_id, details, ..
            } => Some((tool_use_id.as_str(), details.as_ref())),
            _ => None,
        })
        .collect()
}

#[test]
fn cleared_rows_without_a_step_member_keep_their_kind_and_tool_identity() {
    let world = World::in_process("agy-shapes-cleared", cv_core::env::Env::enter_isolated);
    let run = call("native-A", "run_command");
    store(
        &world.base,
        &[
            step(14, 3, None, Some((19, bytes(3, bytes(1, "Start"))))),
            // Compaction cleared these rows: no checkpoint, system-message or generic member.
            step(23, 5, None, None),
            planner(std::slice::from_ref(&run)),
            step(132, 5, Some(&run), None),
            step(101, 5, None, None),
        ],
    );
    let session = read(&world.base);
    assert_eq!(
        session.messages.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [
            MessageKind::Prompt,
            MessageKind::CompactionBoundary,
            MessageKind::CompactionSummary,
            MessageKind::Reply,
            MessageKind::ToolResult,
            MessageKind::InjectedContext,
        ]
    );
    assert_eq!(results(&session)[0].0, "native-A");
    let coverage = ReadCoverage::of_session(&session).unwrap();
    assert!(coverage.limitations.contains(ReadCoverage::MISSING_PAYLOADS));
    assert!(!coverage.limitations.contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
}

#[test]
fn typed_tool_results_keep_their_scalar_facts_and_every_spawned_child() {
    let world = World::in_process("agy-shapes-payload", cv_core::env::Env::enter_isolated);
    let run = call("native-run", "run_command");
    let spawn = call("native-spawn", "invoke_subagent");
    let children = [
        "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee1",
        "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee2",
    ];
    let command = [number(1, 2), bytes(6, "make test"), bytes(7, "/work")].concat();
    let spawned: Vec<u8> = children.iter().flat_map(|id| bytes(2, bytes(1, id))).collect();
    store(
        &world.base,
        &[
            planner(&[run.clone(), spawn.clone()]),
            typed_result(&run, "2 tests failed", "exa.cortex.core.tools.RunCommand", command),
            typed_result(&spawn, "started", "exa.cortex.core.tools.InvokeSubagent", spawned),
        ],
    );
    let session = read(&world.base);
    let results = results(&session);
    assert_eq!(
        results[0].1.unwrap()["payload"],
        json!({"type": "exa.cortex.core.tools.RunCommand", "exit_code": 2, "cwd": "/work"})
    );
    let subagents = &session.extra["antigravity"]["subagents"];
    for child in children {
        assert_eq!(subagents[child]["call_id"], "native-spawn", "{subagents}");
    }

    // The same rows beside their export: a verified result keeps its typed facts.
    let logs = world.base.join("brain").join(ID).join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let stamp = "2023-11-14T22:13:20Z";
    let export = [
        json!({"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE","created_at":stamp,"content":"Working",
            "tool_calls":[{"name":"run_command","args":{}},{"name":"invoke_subagent","args":{}}]}),
        json!({"step_index":1,"type":"GENERIC","status":"DONE","created_at":stamp,
            "content":format!("Created At: {stamp}\nCompleted At: {stamp}\n2 tests failed")}),
    ];
    fs::write(
        logs.join("transcript_full.jsonl"),
        export.iter().map(|record| format!("{record}\n")).collect::<String>(),
    )
    .unwrap();
    let session = read(&world.base);
    assert!(session.source_path.as_ref().unwrap().ends_with("transcript_full.jsonl"));
    let merged = self::results(&session);
    assert_eq!(merged[0].0, "native-run");
    assert_eq!(merged[0].1.unwrap()["payload"]["exit_code"], 2);
}
