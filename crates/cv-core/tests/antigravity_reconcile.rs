//! Export/native reconciliation against the exported shapes the installed 1.5.9 writer
//! produces: tool results under a `Created At:`/`Completed At:` header, prompts and
//! system messages inside tags with trailing metadata, second-precision timestamps.
//! Native rows are encoded here from the 1.5.9 field descriptors; all text is synthetic.
#![cfg(feature = "sqlite")]

use cv_core::coverage::ReadCoverage;
use cv_core::harness::{antigravity::Antigravity, Parse};
use cv_core::{Adapter, Block, MessageKind, Session};
use cv_testkit::World;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const ID: &str = "11111111-2222-4333-8444-555555555555";
const SECOND: u64 = 1_700_000_000;

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
/// CortexStepMetadata: created_at(1) with sub-second precision, tool_call(4), model_usage(9).
fn metadata(offset: u64, call: Option<(&str, &str)>, reasoning: Option<u64>) -> Vec<u8> {
    let mut out = bytes(1, [number(1, SECOND + offset), number(2, 987_654_321)].concat());
    if let Some((id, name)) = call {
        out.extend(bytes(4, [bytes(1, id), bytes(2, name)].concat()));
    }
    if let Some(reasoning) = reasoning {
        out.extend(bytes(9, number(9, reasoning)));
    }
    out
}
fn step(ty: u64, status: u64, payload: Option<(u64, Vec<u8>)>, metadata: Vec<u8>) -> Vec<u8> {
    let mut out = [number(1, ty), number(4, status), bytes(5, metadata)].concat();
    if let Some((field, payload)) = payload {
        out.extend(bytes(field, payload));
    }
    out
}
fn prompt(offset: u64, text: &str) -> Vec<u8> {
    let input = [bytes(3, bytes(1, text)), bytes(2, text)].concat();
    step(14, 3, Some((19, input)), metadata(offset, None, None))
}
fn planner(offset: u64, text: &str, calls: &[(&str, &str, &str)], reasoning: Option<u64>) -> Vec<u8> {
    let mut payload = bytes(1, text);
    for (id, name, arguments) in calls {
        payload.extend(bytes(7, [bytes(1, id), bytes(2, name), bytes(3, arguments)].concat()));
    }
    step(15, 3, Some((20, payload)), metadata(offset, None, reasoning))
}
fn result(offset: u64, status: u64, id: &str, body: &str) -> Vec<u8> {
    step(
        132,
        status,
        Some((140, bytes(2, bytes(1, body)))),
        metadata(offset, Some((id, "run_command")), None),
    )
}
/// CortexStepErrorMessage.error(3) -> CortexErrorDetails.short_error(2).
fn failure(offset: u64, text: &str) -> Vec<u8> {
    step(
        17,
        3,
        Some((24, bytes(3, bytes(2, text)))),
        metadata(offset, None, None),
    )
}

fn stamp(offset: u64) -> String {
    chrono::DateTime::from_timestamp((SECOND + offset) as i64, 0)
        .unwrap()
        .to_rfc3339()
}
fn exported(index: u64, offset: u64, ty: &str, status: &str, fields: Value) -> Value {
    let mut record = json!({
        "step_index": index, "type": ty, "status": status, "created_at": stamp(offset),
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    record
}
fn exported_prompt(index: u64, offset: u64, text: &str) -> Value {
    let content = format!(
        "<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: {}.\n</ADDITIONAL_METADATA>",
        stamp(offset)
    );
    exported(
        index,
        offset,
        "USER_INPUT",
        "DONE",
        json!({"source":"USER_EXPLICIT","content":content}),
    )
}
fn exported_result(index: u64, offset: u64, status: &str, body: &str) -> Value {
    let content = format!(
        "Created At: {}\nCompleted At: {}\n{body}",
        stamp(offset),
        stamp(offset + 1)
    );
    exported(
        index,
        offset,
        "GENERIC",
        status,
        json!({"source":"MODEL","content":content}),
    )
}

fn read(label: &str, native: &[(i64, Vec<u8>)], export: &[Value]) -> Session {
    let world = World::in_process(label, cv_core::env::Env::enter_isolated);
    let root: &Path = &world.base;
    let conversations = root.join("conversations");
    fs::create_dir_all(&conversations).unwrap();
    let conn = rusqlite::Connection::open(conversations.join(format!("{ID}.db"))).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    for (index, payload) in native {
        conn.execute(
            "INSERT INTO steps(idx, step_payload) VALUES (?1, ?2)",
            rusqlite::params![index, payload],
        )
        .unwrap();
    }
    drop(conn);
    let logs = root.join("brain").join(ID).join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let body: String = export.iter().map(|record| format!("{record}\n")).collect();
    fs::write(logs.join("transcript_full.jsonl"), body).unwrap();
    let adapter = Antigravity::with_home(root);
    let reference = adapter.discover().unwrap().remove(0);
    assert!(reference.path.ends_with("transcript_full.jsonl"));
    adapter.parse(&reference).unwrap()
}

fn limitations(session: &Session) -> std::collections::BTreeSet<String> {
    ReadCoverage::of_session(session).unwrap().limitations
}
fn kinds(session: &Session) -> Vec<MessageKind> {
    session.messages.iter().map(|m| m.kind).collect()
}
fn texts(session: &Session) -> Vec<String> {
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.try_resolve(&resolver).unwrap().into_owned()),
            _ => None,
        })
        .collect()
}
fn result_ids(session: &Session) -> Vec<&str> {
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

const CALLS: [(&str, &str, &str); 2] = [
    ("native-A", "run_command", r#"{"CommandLine":"check A"}"#),
    ("native-B", "run_command", r#"{"CommandLine":"check B"}"#),
];
fn exported_planner(index: u64, offset: u64, text: &str, calls: bool) -> Value {
    let mut fields = json!({"source":"MODEL","content":text});
    if calls {
        fields["tool_calls"] = json!([
            {"name":"run_command","args":{"CommandLine":"check A"}},
            {"name":"run_command","args":{"CommandLine":"check B"}}
        ]);
    }
    exported(index, offset, "PLANNER_RESPONSE", "DONE", fields)
}

#[test]
fn exported_result_headers_do_not_hide_native_tool_identity() {
    // Results complete in the opposite order to their same-name calls.
    let session = read(
        "agy-reconcile-result-header",
        &[
            (0, planner(0, "Running two checks", &CALLS, None)),
            (1, result(1, 3, "native-B", "B passed")),
            (2, result(2, 3, "native-A", "A passed")),
        ],
        &[
            exported_planner(0, 0, "Running two checks", true),
            exported_result(1, 1, "DONE", "B passed"),
            exported_result(2, 2, "DONE", "A passed"),
        ],
    );
    assert_eq!(result_ids(&session), ["native-B", "native-A"]);
    assert!(!limitations(&session).contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
}

#[test]
fn another_revision_of_a_result_keeps_its_call_but_verifies_nothing_beside_it() {
    // Same index, type and second; the exported body is not the native body.
    let session = read(
        "agy-reconcile-result-revision",
        &[
            (0, planner(0, "Running two checks", &CALLS, None)),
            (1, result(1, 3, "native-B", "passed")),
            (2, failure(2, "only the native store has this step")),
        ],
        &[
            exported_planner(0, 0, "Running two checks", true),
            exported_result(1, 1, "DONE", "passed after a retry"),
        ],
    );
    // The verified planner issued the call this step answers; call order would say A.
    assert_eq!(result_ids(&session), ["native-B"]);
    assert_eq!(kinds(&session), [MessageKind::Reply, MessageKind::ToolResult]);
    assert!(limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));
}

#[test]
fn a_result_in_another_second_is_not_the_native_step() {
    let session = read(
        "agy-reconcile-result-second",
        &[
            (0, planner(0, "Running two checks", &CALLS, None)),
            (1, result(1, 3, "native-B", "B passed")),
        ],
        &[
            exported_planner(0, 0, "Running two checks", true),
            exported_result(1, 5, "DONE", "B passed"),
        ],
    );
    assert_eq!(result_ids(&session), ["native-A"], "paired by call order");
    assert!(limitations(&session).contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
}

#[test]
fn a_failed_step_is_verified_by_its_exported_error_text() {
    // GENERIC failures carry CortexErrorDetails at Step.error(31); the export renders
    // other prose as content and the writer's error text under `error`.
    let failed = [
        number(1, 132),
        number(4, 7),
        bytes(5, metadata(1, Some(("native-B", "run_command")), None)),
        bytes(140, bytes(2, [])),
        bytes(31, [bytes(2, "target missing"), bytes(3, "target missing")].concat()),
    ]
    .concat();
    let mut exported_failure = exported_result(1, 1, "ERROR", "Encountered error in step execution: target missing");
    exported_failure["error"] = json!("target missing");
    let session = read(
        "agy-reconcile-error",
        &[
            (0, planner(0, "Running two checks", &CALLS, None)),
            (1, failed),
            (2, failure(2, "interrupted")),
            (3, failure(3, "native only")),
        ],
        &[
            exported_planner(0, 0, "Running two checks", true),
            exported_failure,
            // ERROR_MESSAGE exports its short error as `error`, without content.
            exported(
                2,
                2,
                "ERROR_MESSAGE",
                "DONE",
                json!({"source":"SYSTEM","error":"interrupted"}),
            ),
        ],
    );
    assert_eq!(
        kinds(&session),
        [
            MessageKind::Reply,
            MessageKind::ToolResult,
            MessageKind::Error,
            MessageKind::Error
        ]
    );
    assert!(!limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));
}

#[test]
fn a_tagged_exported_prompt_verifies_the_native_tail() {
    let session = read(
        "agy-reconcile-prompt-tail",
        &[
            (0, prompt(0, "Summarize the build log")),
            (1, planner(1, "The build passed", &[], None)),
        ],
        &[exported_prompt(0, 0, "Summarize the build log")],
    );
    assert_eq!(kinds(&session), [MessageKind::Prompt, MessageKind::Reply]);
    assert_eq!(texts(&session)[1], "The build passed");
    assert!(!limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));
}

#[test]
fn a_native_step_missing_inside_the_export_is_read_in_step_order() {
    let native = [
        (0, prompt(0, "Run the migration")),
        (1, failure(1, "model quota exhausted, retrying")),
        (2, planner(2, "Migration finished", &[], None)),
    ];
    let session = read(
        "agy-reconcile-interior",
        &native,
        &[
            exported_prompt(0, 0, "Run the migration"),
            exported_planner(2, 2, "Migration finished", false),
        ],
    );
    assert_eq!(
        kinds(&session),
        [MessageKind::Prompt, MessageKind::Error, MessageKind::Reply]
    );
    assert_eq!(texts(&session)[1], "model quota exhausted, retrying");
    assert!(!limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));

    // The same gap beside a different revision of the following step is disclosed, not merged.
    let session = read(
        "agy-reconcile-interior-divergent",
        &native,
        &[
            exported_prompt(0, 0, "Run the migration"),
            exported_planner(2, 2, "An answer from another revision", false),
        ],
    );
    assert_eq!(kinds(&session), [MessageKind::Prompt, MessageKind::Reply]);
    assert!(limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));
}

#[test]
fn an_export_that_starts_late_keeps_the_native_head() {
    let session = read(
        "agy-reconcile-head",
        &[
            (0, prompt(0, "First request")),
            (1, planner(1, "First answer", &[], None)),
            (2, prompt(2, "Second request")),
        ],
        &[exported_prompt(2, 2, "Second request")],
    );
    assert_eq!(
        kinds(&session),
        [MessageKind::Prompt, MessageKind::Reply, MessageKind::Prompt]
    );
    assert_eq!(texts(&session)[..2], ["First request", "First answer"]);
}

#[test]
fn a_step_exported_while_running_neither_matches_nor_blocks_reconciliation() {
    let session = read(
        "agy-reconcile-running",
        &[
            (0, planner(0, "Running two checks", &CALLS, None)),
            (1, result(1, 3, "native-A", "final output of the finished task")),
            (2, failure(2, "interrupted")),
        ],
        &[
            exported_planner(0, 0, "Running two checks", true),
            exported_result(1, 1, "RUNNING", "Tool is running as a background task"),
        ],
    );
    assert_eq!(
        kinds(&session),
        [MessageKind::Reply, MessageKind::ToolResult, MessageKind::Error]
    );
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    let Block::ToolResult { content, .. } = &session.messages[1].content[0] else {
        panic!("tool result expected");
    };
    assert!(
        content
            .try_resolve(&resolver)
            .unwrap()
            .contains("running as a background task"),
        "the exported revision stays authoritative"
    );
    assert_eq!(result_ids(&session), ["native-A"]);
    assert!(!limitations(&session).contains(ReadCoverage::UNRECONCILED_HISTORY));
}

#[test]
fn identical_bodies_at_two_indices_keep_their_own_native_usage() {
    let session = read(
        "agy-reconcile-repeated-body",
        &[
            (0, planner(0, "Done.", &[], Some(11))),
            (1, prompt(0, "again")),
            (2, planner(0, "Done.", &[], Some(22))),
        ],
        &[
            exported_planner(0, 0, "Done.", false),
            exported_prompt(1, 0, "again"),
            exported_planner(2, 0, "Done.", false),
        ],
    );
    let reasoning: Vec<_> = session
        .messages
        .iter()
        .filter_map(|m| m.usage.as_ref()?.reasoning_tokens)
        .collect();
    assert_eq!(reasoning, [11, 22]);
}

#[test]
fn a_native_result_answers_an_exported_call_whose_planner_row_was_cleared() {
    // The cleared planner row keeps no call ids; the cancelled result exists only natively.
    let session = read(
        "agy-reconcile-cleared-planner",
        &[
            (0, prompt(0, "Check the target")),
            (1, step(15, 5, Some((20, vec![])), metadata(1, None, None))),
            (2, result(2, 6, "native-X", "cancelled")),
            (3, planner(3, "Stopped", &[], None)),
        ],
        &[
            exported_prompt(0, 0, "Check the target"),
            exported(
                1,
                1,
                "PLANNER_RESPONSE",
                "DONE",
                json!({"content":"Checking","tool_calls":[{"name":"run_command","args":{}}]}),
            ),
            exported_planner(3, 3, "Stopped", false),
        ],
    );
    let call = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|block| match block {
            Block::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(result_ids(&session), [call], "the result must not be orphaned");
    assert!(limitations(&session).contains(ReadCoverage::INFERRED_TOOL_IDENTITY));
}
