//! Real CLI 1.5.9 captures (`tools/harness-corpus/antigravity/generate.py`). Expectations come
//! from the scripted scenario and from a schema-less walk of the stored bytes written here,
//! never from the adapter's own decoder.
#![cfg(feature = "sqlite")]

use cv_core::harness::{antigravity::Antigravity, Parse};
use cv_core::{Adapter, Block, Harness, Message, MessageKind, Origin, Session, SessionRef};
use cv_testkit::World;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==";

type Isolated = cv_testkit::InProcess<cv_core::env::Isolated>;

fn world(label: &str) -> Isolated {
    World::in_process(label, cv_core::env::Env::enter_isolated)
}

fn capture(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/antigravity")
        .join(version)
        .join(".gemini/antigravity-cli")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// An owned copy of a capture. `exported` keeps the writer's logs and registries beside the
/// native stores; without it only `conversations/*.db` survives.
fn store(world: &Isolated, version: &str, exported: bool) -> PathBuf {
    let root = world.base.join(format!("{version}-{exported}"));
    copy_tree(&capture(version).join("conversations"), &root.join("conversations"));
    let brain = capture(version).join("brain");
    if exported && brain.is_dir() {
        copy_tree(&brain, &root.join("brain"));
    }
    root
}

fn sessions(root: &Path) -> BTreeMap<String, (SessionRef, Session)> {
    let adapter = Antigravity::with_home(root);
    adapter
        .discover()
        .unwrap()
        .into_iter()
        .map(|reference| {
            let session = adapter.parse(&reference).unwrap();
            (reference.id.clone(), (reference, session))
        })
        .collect()
}

// ---- schema-less reading of what the writer stored -------------------------------------------

fn varint(bytes: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 127) << shift;
        shift += 7;
        if byte < 128 {
            return value;
        }
    }
}

/// Length-delimited values of `field`, in order. Other wire types are skipped.
fn nested<'a>(bytes: &'a [u8], field: u64) -> Vec<&'a [u8]> {
    let (mut at, mut out) = (0, Vec::new());
    while at < bytes.len() {
        let key = varint(bytes, &mut at);
        match key & 7 {
            0 => {
                varint(bytes, &mut at);
            }
            1 => at += 8,
            5 => at += 4,
            2 => {
                let len = varint(bytes, &mut at) as usize;
                if key >> 3 == field {
                    out.push(&bytes[at..at + len]);
                }
                at += len;
            }
            other => panic!("unexpected wire type {other}"),
        }
    }
    out
}

fn string(bytes: &[u8], field: u64) -> Option<String> {
    nested(bytes, field)
        .last()
        .map(|value| String::from_utf8(value.to_vec()).unwrap())
}

struct Stored {
    index: u64,
    status: i64,
    payload: Vec<u8>,
}

fn stored(root: &Path, id: &str) -> Vec<Stored> {
    let conn = rusqlite::Connection::open(root.join("conversations").join(format!("{id}.db"))).unwrap();
    let mut query = conn
        .prepare("SELECT idx, status, step_payload FROM steps ORDER BY idx")
        .unwrap();
    let rows = query
        .query_map([], |row| {
            Ok(Stored {
                index: row.get::<_, i64>(0)? as u64,
                status: row.get(1)?,
                payload: row.get(2)?,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// `(call id, tool, arguments)` for every call the stored planner steps issued:
/// Step.planner_response(20).tool_calls(7).{id(1), name(2), arguments_json(3)}.
fn stored_calls(steps: &[Stored]) -> Vec<(String, String, String)> {
    steps
        .iter()
        .flat_map(|step| nested(&step.payload, 20))
        .flat_map(|planner| nested(planner, 7))
        .map(|call| {
            (
                string(call, 1).unwrap(),
                string(call, 2).unwrap(),
                string(call, 3).unwrap_or_default(),
            )
        })
        .collect()
}

fn call_with(steps: &[Stored], needle: &str) -> String {
    let calls = stored_calls(steps);
    let mut found = calls.iter().filter(|(_, _, arguments)| arguments.contains(needle));
    let id = found
        .next()
        .unwrap_or_else(|| panic!("no stored call mentions {needle}"))
        .0
        .clone();
    assert!(found.next().is_none(), "{needle} names more than one stored call");
    id
}

// ---- reading the IR --------------------------------------------------------------------------

fn body(session: &Session, message: &Message) -> String {
    let resolver = cv_core::lazy::Resolver::new(session.source_path.clone());
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.try_resolve(&resolver).unwrap().into_owned()),
            Block::ToolResult { content, .. } => Some(content.try_resolve(&resolver).unwrap().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ids(session: &Session) -> Vec<String> {
    session.messages.iter().filter_map(|m| m.id.clone()).collect()
}

fn by_id<'a>(session: &'a Session, id: u64) -> &'a Message {
    let id = id.to_string();
    session
        .messages
        .iter()
        .find(|m| m.id.as_deref() == Some(&id))
        .unwrap_or_else(|| panic!("step {id} is missing from the read"))
}

fn native(message: &Message) -> &Value {
    message.extra.get("antigravity").unwrap_or(&Value::Null)
}

/// `(status, is_error)` of each result, by the call it answers.
fn results(session: &Session) -> BTreeMap<String, Vec<(Option<String>, bool)>> {
    let mut out: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for block in session.messages.iter().flat_map(|m| &m.content) {
        if let Block::ToolResult {
            tool_use_id,
            status,
            is_error,
            ..
        } = block
        {
            out.entry(tool_use_id.clone())
                .or_default()
                .push((status.clone(), *is_error));
        }
    }
    out
}

fn limitations(session: &Session) -> Vec<String> {
    session.extra["cv"]["read_coverage"]["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn sources(session: &Session) -> Vec<String> {
    session.extra["cv"]["read_coverage"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// `(kind, action, tool_use_id, summary)` rows of the affordance view.
fn affordances(root: &Path, reference: &SessionRef) -> Vec<(String, String, Option<String>, String)> {
    let adapter = Antigravity::with_home(root);
    cv_core::affordance::read(&adapter, reference)
        .unwrap()
        .into_iter()
        .map(|row| {
            let row = serde_json::to_value(row).unwrap();
            (
                row["kind"].as_str().unwrap().to_owned(),
                row["action"].as_str().unwrap().to_owned(),
                row["tool_use_id"].as_str().map(str::to_owned),
                row["summary"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn only(map: BTreeMap<String, (SessionRef, Session)>) -> (SessionRef, Session) {
    assert_eq!(map.len(), 1);
    map.into_values().next().unwrap()
}

// ---- permissions -----------------------------------------------------------------------------

#[test]
fn permission_request_decision_and_tool_status_are_read() {
    let world = world("agy-writer-permissions");
    let root = store(&world, "1.5.9-permissions", false);
    let read = sessions(&root);
    assert_eq!(read.len(), 2);
    let (asked, headless): (Vec<_>, Vec<_>) = read
        .values()
        .partition(|(_, session)| body(session, &session.messages[0]).contains("two gated"));
    let (reference, session) = asked[0];
    let steps = stored(&root, &session.id);
    let approved = call_with(&steps, "approved fixture");
    let denied = call_with(&steps, "denied fixture");
    // CORTEX_STEP_STATUS: 3 DONE, 7 ERROR, 6 CANCELED.
    assert_eq!(steps.iter().map(|s| s.status).collect::<Vec<_>>(), [3, 3, 3, 3, 7]);

    let outcome = results(session);
    assert_eq!(outcome[&approved], [(Some("completed".into()), false)]);
    assert_eq!(outcome[&denied], [(Some("error".into()), true)]);
    assert!(body(session, by_id(session, 2)).contains("approved fixture\r\n"));
    let refusal = body(session, by_id(session, 4));
    assert!(refusal.contains("user denied permission to run command"));
    assert!(!refusal.contains("denied fixture\r\n"), "a denied command has no output");
    assert_eq!(
        native(by_id(session, 2))["interaction"],
        json!({"kind": "permission", "action": "command", "target": "printf 'approved fixture\\n'", "decision": "approved"})
    );
    assert_eq!(
        native(by_id(session, 4))["interaction"],
        json!({"kind": "permission", "action": "command", "target": "printf 'denied fixture\\n'", "decision": "denied"})
    );
    // The approval row is the interaction's own (no call of its own); the denial row is
    // the refused call's.
    let approvals: Vec<_> = affordances(&root, reference)
        .into_iter()
        .filter(|row| row.0 == "approval")
        .collect();
    assert_eq!(approvals.len(), 2);
    assert_eq!((approvals[0].1.as_str(), &approvals[0].2), ("approve", &None));
    assert_eq!(approvals[0].3, "command printf 'approved fixture\\n'");
    assert_eq!((approvals[1].1.as_str(), approvals[1].2.as_ref()), ("deny", Some(&denied)));

    // Print mode cannot ask: the writer cancels the gated step and records no interaction.
    let (reference, session) = headless[0];
    let steps = stored(&root, &session.id);
    let gated = call_with(&steps, "headless fixture");
    assert_eq!(steps.last().unwrap().status, 6);
    assert_eq!(results(session)[&gated], [(Some("canceled".into()), false)]);
    let last = session.messages.last().unwrap();
    assert!(native(last).get("interaction").is_none());
    assert!(!body(session, last).contains("Command exited"));
    assert!(affordances(&root, reference).iter().all(|row| row.0 != "approval"));
    assert!(cv_core::distill::unsettled_calls(session).unwrap().is_empty());
}

// ---- interruption, retry, upstream errors -----------------------------------------------------

#[test]
fn steps_absent_from_the_export_are_read_from_the_store() {
    let world = world("agy-writer-interruption");
    for exported in [true, false] {
        let root = store(&world, "1.5.9-interruption", exported);
        let (_, session) = only(sessions(&root));
        let steps = stored(&root, &session.id);
        let interrupted = call_with(&steps, "interrupted fixture finished");
        let retried = call_with(&steps, "retried fixture finished");
        assert_eq!(steps.len(), 15);
        if exported {
            let log = fs::read_to_string(root.join("brain").join(&session.id).join(".system_generated/logs/transcript_full.jsonl")).unwrap();
            let written: Vec<u64> = log
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()["step_index"].as_u64().unwrap())
                .collect();
            assert!(!written.contains(&2) && !written.contains(&8), "the writer exported neither step");
            assert_eq!(sources(&session), ["exported_history", "native_store"]);
        }
        // Every stored step is one message, in step order, whichever source carried it.
        assert_eq!(ids(&session), (0..15).map(|i| i.to_string()).collect::<Vec<_>>(), "exported={exported}");
        assert!(!limitations(&session).contains(&"unreconciled_history".to_owned()));

        let outcome = results(&session);
        assert_eq!(outcome.len(), 2, "two calls, each answered once");
        assert_eq!(outcome[&interrupted], [(Some("canceled".into()), false)]);
        assert_eq!(outcome[&retried], [(Some("completed".into()), false)]);
        let cancelled = body(&session, by_id(&session, 2));
        assert!(!cancelled.contains("interrupted fixture finished\r\n") && !cancelled.contains("Command exited"));
        assert!(body(&session, by_id(&session, 5)).contains("retried fixture finished\r\n"));
        // Both commands were approved through ACP before they ran; the export has no such field.
        for step in [2, 5] {
            assert_eq!(native(by_id(&session, step))["interaction"]["decision"], "approved", "exported={exported}");
        }

        let failure = by_id(&session, 8);
        assert_eq!(failure.kind, MessageKind::Error);
        assert!(body(&session, failure).contains("fixture error"));

        // STOP_REASON 18 is CLIENT_STREAM_ERROR; the dropped stream has no stop reason at all.
        assert_eq!(body(&session, by_id(&session, 12)), "partial reply before interruption ");
        assert_eq!(native(by_id(&session, 12))["planner_details"]["stop"], "client_stream_error");
        assert_eq!(native(by_id(&session, 12))["interrupted"], true);
        assert!(native(by_id(&session, 10))["planner_details"].get("stop").is_none());
        assert_eq!(native(by_id(&session, 14))["planner_details"]["stop"], "stop_pattern");
        assert!(native(by_id(&session, 14)).get("interrupted").is_none());
        assert!(cv_core::distill::unsettled_calls(&session).unwrap().is_empty());
    }
}

// ---- compaction and resume --------------------------------------------------------------------

#[test]
fn a_resumed_conversation_keeps_the_history_its_recreated_export_lacks() {
    let world = world("agy-writer-resume");
    let mut reads = Vec::new();
    for exported in [true, false] {
        let root = store(&world, "1.5.9-compaction-resume", exported);
        let (_, session) = only(sessions(&root));
        let steps = stored(&root, &session.id);
        if exported {
            let log = fs::read_to_string(root.join("brain").join(&session.id).join(".system_generated/logs/transcript_full.jsonl")).unwrap();
            let first: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
            assert_eq!(first["step_index"], 7, "the writer re-created its log at the native-only resume");
        }
        let expected: Vec<String> = steps.iter().map(|s| s.index.to_string()).collect();
        assert_eq!(ids(&session), expected, "exported={exported}");
        assert_eq!(expected.len(), 17);
        assert!(!limitations(&session).contains(&"unreconciled_history".to_owned()));
        assert!(session.lineage.parent.is_none());
        assert_eq!(session.extra["antigravity"]["root_conversation_id"], session.id.as_str());

        assert_eq!(body(&session, by_id(&session, 0)).contains("resume-fixture: first turn"), true);
        assert_eq!(body(&session, by_id(&session, 3)), "first: done.");
        assert!(body(&session, by_id(&session, 4)).contains("/compact"), "the typed command is the prompt");
        let boundaries = session
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionBoundary)
            .count();
        assert_eq!(boundaries, 1);
        let summary = by_id(&session, 6);
        assert_eq!(summary.kind, MessageKind::CompactionSummary);
        assert!(body(&session, summary).contains("FIXTURE SUMMARY: the user ran one pre-compaction fixture command"));
        assert_eq!(body(&session, by_id(&session, 16)), "third: done.");
        assert_eq!(results(&session).len(), 3);
        assert!(results(&session).values().all(|r| r == &[(Some("completed".into()), false)]));
        reads.push(session.messages.iter().map(|m| (m.id.clone(), m.kind)).collect::<Vec<_>>());
    }
    assert_eq!(reads[0], reads[1], "both sources describe one conversation");
}

// ---- background tasks -------------------------------------------------------------------------

#[test]
fn killed_and_finished_background_tasks_settle_distinctly() {
    let world = world("agy-writer-background");
    for exported in [true, false] {
        let root = store(&world, "1.5.9-background-tasks", exported);
        let (reference, session) = only(sessions(&root));
        let steps = stored(&root, &session.id);
        let slow = call_with(&steps, "slow fixture done");
        let fast = call_with(&steps, "fast fixture done");
        let doomed = call_with(&steps, "never printed");
        // The store's final revision of the three launches: DONE, DONE, CANCELED.
        assert_eq!(steps[2..5].iter().map(|s| s.status).collect::<Vec<_>>(), [3, 3, 6]);

        let outcome = results(&session);
        assert_eq!(outcome[&doomed], [(Some("canceled".into()), false)], "exported={exported}");
        assert_eq!(outcome[&slow], [(Some("completed".into()), false)]);
        assert_eq!(outcome[&fast], [(Some("completed".into()), false)]);
        let task = |step: u64| native(by_id(&session, step))["task_details"]["id"].as_str().unwrap().to_owned();
        let (slow_task, fast_task, doomed_task) = (task(2), task(3), task(4));
        assert!(slow_task.ends_with("/task-2") && fast_task.ends_with("/task-3") && doomed_task.ends_with("/task-4"));

        // Notifications arrive in completion order: the kill, the 2 s task, the 4 s task.
        let delivered: Vec<(&str, String)> = session
            .messages
            .iter()
            .filter_map(|m| {
                ["canceled_task", "completed_task"]
                    .into_iter()
                    .find_map(|key| Some((key, native(m).get(key)?.as_str()?.to_owned())))
            })
            .collect();
        assert_eq!(
            delivered,
            [("canceled_task", doomed_task.clone()), ("completed_task", fast_task.clone()), ("completed_task", slow_task.clone())]
        );
        let jobs: Vec<(String, String)> = affordances(&root, &reference)
            .into_iter()
            .filter(|row| row.0 == "background_job" && row.2.is_none())
            .map(|row| (row.1, row.3))
            .collect();
        assert_eq!(
            jobs,
            [("stop".to_owned(), doomed_task), ("complete".to_owned(), fast_task), ("complete".to_owned(), slow_task)]
        );
        assert!(!body(&session, by_id(&session, 4)).contains("never printed\r\n"));
        assert!(body(&session, by_id(&session, 8)).contains("cancelled."), "the kill acknowledgement");
        assert!(cv_core::distill::unsettled_calls(&session).unwrap().is_empty());

        // Before any notification: the killed launch is already settled; in the exported
        // history the other two are still only acknowledgements.
        let mut early = session.clone();
        early.messages.truncate(11);
        let open = cv_core::distill::unsettled_calls(&early).unwrap();
        assert!(!open.contains(&doomed), "exported={exported}");
        assert_eq!(open.contains(&slow) && open.contains(&fast), exported);
    }
}

// ---- swarm ------------------------------------------------------------------------------------

fn members(root: &Path) -> (String, Value) {
    let brain = root.join("brain");
    for entry in fs::read_dir(&brain).unwrap() {
        let entry = entry.unwrap();
        let registry = entry.path().join(".system_generated/swarm/members.json");
        if registry.is_file() {
            let value: Value = serde_json::from_str(&fs::read_to_string(registry).unwrap()).unwrap();
            return (entry.file_name().into_string().unwrap(), value["members"].clone());
        }
    }
    panic!("capture has no swarm registry")
}


// ---- invoked subagents ------------------------------------------------------------------------


// ---- prompt media ----------------------------------------------------------------------------


fn len(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for mut number in [(field << 3) | 2, value.len() as u64] {
        while number >= 128 {
            out.push(number as u8 | 128);
            number >>= 7;
        }
        out.push(number as u8);
    }
    out.extend_from_slice(value);
    out
}

