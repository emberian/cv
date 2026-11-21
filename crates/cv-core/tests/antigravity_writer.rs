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
fn nested(bytes: &[u8], field: u64) -> Vec<&[u8]> {
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

fn by_id(session: &Session, id: u64) -> &Message {
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
    assert!(
        !refusal.contains("denied fixture\r\n"),
        "a denied command has no output"
    );
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
    assert_eq!(
        (approvals[1].1.as_str(), approvals[1].2.as_ref()),
        ("deny", Some(&denied))
    );

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
            let log = fs::read_to_string(
                root.join("brain")
                    .join(&session.id)
                    .join(".system_generated/logs/transcript_full.jsonl"),
            )
            .unwrap();
            let written: Vec<u64> = log
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["step_index"]
                        .as_u64()
                        .unwrap()
                })
                .collect();
            assert!(
                !written.contains(&2) && !written.contains(&8),
                "the writer exported neither step"
            );
            assert_eq!(sources(&session), ["exported_history", "native_store"]);
        }
        // Every stored step is one message, in step order, whichever source carried it.
        assert_eq!(
            ids(&session),
            (0..15).map(|i| i.to_string()).collect::<Vec<_>>(),
            "exported={exported}"
        );
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
            assert_eq!(
                native(by_id(&session, step))["interaction"]["decision"],
                "approved",
                "exported={exported}"
            );
        }

        let failure = by_id(&session, 8);
        assert_eq!(failure.kind, MessageKind::Error);
        assert_eq!(
            body(&session, failure),
            "UNAVAILABLE (code 503): fixture error",
            "exported={exported}"
        );

        // STOP_REASON 18 is CLIENT_STREAM_ERROR; the dropped stream has no stop reason at all.
        assert_eq!(
            body(&session, by_id(&session, 12)),
            "partial reply before interruption "
        );
        assert_eq!(
            native(by_id(&session, 12))["planner_details"]["stop"],
            "client_stream_error"
        );
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
            let log = fs::read_to_string(
                root.join("brain")
                    .join(&session.id)
                    .join(".system_generated/logs/transcript_full.jsonl"),
            )
            .unwrap();
            let first: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
            assert_eq!(
                first["step_index"], 7,
                "the writer re-created its log at the native-only resume"
            );
        }
        let expected: Vec<String> = steps.iter().map(|s| s.index.to_string()).collect();
        assert_eq!(ids(&session), expected, "exported={exported}");
        assert_eq!(expected.len(), 17);
        assert!(!limitations(&session).contains(&"unreconciled_history".to_owned()));
        assert!(session.lineage.parent.is_none());
        assert_eq!(
            session.extra["antigravity"]["root_conversation_id"],
            session.id.as_str()
        );

        assert!(body(&session, by_id(&session, 0)).contains("resume-fixture: first turn"));
        assert_eq!(body(&session, by_id(&session, 3)), "first: done.");
        assert!(
            body(&session, by_id(&session, 4)).contains("/compact"),
            "the typed command is the prompt"
        );
        let boundaries = session
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionBoundary)
            .count();
        assert_eq!(boundaries, 1);
        let summary = by_id(&session, 6);
        assert_eq!(summary.kind, MessageKind::CompactionSummary);
        assert!(body(&session, summary).contains("FIXTURE SUMMARY: the user ran one pre-compaction fixture command"));
        assert_eq!(
            native(summary)["checkpoint_details"],
            json!({
                "included_step_index_end": 6,
                "total_subagent_count": 0,
                "captured_requests": ["resume-fixture: first turn", "/compact"]
            }),
            "exported={exported}"
        );
        assert_eq!(body(&session, by_id(&session, 16)), "third: done.");
        assert_eq!(results(&session).len(), 3);
        assert!(results(&session)
            .values()
            .all(|r| r == &[(Some("completed".into()), false)]));
        reads.push(
            session
                .messages
                .iter()
                .map(|m| (m.id.clone(), m.kind))
                .collect::<Vec<_>>(),
        );
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
        assert_eq!(
            outcome[&doomed],
            [(Some("canceled".into()), false)],
            "exported={exported}"
        );
        assert_eq!(outcome[&slow], [(Some("completed".into()), false)]);
        assert_eq!(outcome[&fast], [(Some("completed".into()), false)]);
        let task = |step: u64| {
            native(by_id(&session, step))["task_details"]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
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
            [
                ("canceled_task", doomed_task.clone()),
                ("completed_task", fast_task.clone()),
                ("completed_task", slow_task.clone())
            ]
        );
        let jobs: Vec<(String, String)> = affordances(&root, &reference)
            .into_iter()
            .filter(|row| row.0 == "background_job" && row.2.is_none())
            .map(|row| (row.1, row.3))
            .collect();
        assert_eq!(
            jobs,
            [
                ("stop".to_owned(), doomed_task),
                ("complete".to_owned(), fast_task),
                ("complete".to_owned(), slow_task)
            ]
        );
        assert!(!body(&session, by_id(&session, 4)).contains("never printed\r\n"));
        assert!(
            body(&session, by_id(&session, 8)).contains("cancelled."),
            "the kill acknowledgement"
        );
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

#[test]
fn swarm_members_board_deliveries_and_lineage_are_read() {
    let world = world("agy-writer-swarm");
    let root = store(&world, "1.5.9-swarm", true);
    let (coordinator, registry) = members(&root);
    let member = |name: &str| registry[name]["conversation_id"].as_str().unwrap().to_owned();
    let (alpha, beta, gamma) = (member("alpha"), member("beta"), member("gamma"));
    assert_eq!(member("coordinator"), coordinator);
    assert_eq!(registry["gamma"]["added_by"], "beta");
    let read = sessions(&root);
    assert_eq!(read.len(), 4);

    // The coordinator is a member of its own registry, never its own child.
    let (reference, session) = &read[&coordinator];
    let forest = cv_core::subagent_tree_of(reference).unwrap();
    let mut children: Vec<_> = forest.iter().map(|agent| agent.session.id.clone()).collect();
    children.sort();
    let mut expected = vec![alpha.clone(), beta.clone(), gamma.clone()];
    expected.sort();
    assert_eq!(children, expected);
    let state = |id: &str| {
        forest
            .iter()
            .find(|a| a.session.id == id)
            .unwrap()
            .result_status
            .clone()
    };
    assert_eq!(state(&gamma).as_deref(), Some("killed"));
    assert_eq!(state(&alpha), None);

    let lanes = cv_core::lanes::lanes_of(reference).unwrap();
    assert_eq!(lanes.len(), 3);
    let lane_of = |id: &str| lanes.iter().find(|l| l.session_id == id).unwrap();
    for (id, want_status, want_text) in [
        (&alpha, "waiting", "alpha final reply"),
        (&beta, "waiting", "beta integrated gamma"),
        (&gamma, "killed", "gamma final reply"),
    ] {
        let lane = lane_of(id);
        assert_eq!(lane.status.as_str(), want_status, "{id}");
        assert!(!lane.is_running() && !lane.is_done() && !lane.stranded, "{lane:?}");
        assert_eq!(lane.last_text.as_deref(), Some(want_text), "{id}");
    }
    let overview = cv_core::overview::read(reference, &cv_core::overview::OverviewOptions::default()).unwrap();
    assert_eq!(overview.workers.len(), 3);
    for (id, want_status, want_text) in [
        (&alpha, "waiting", "alpha final reply"),
        (&beta, "waiting", "beta integrated gamma"),
        (&gamma, "killed", "gamma final reply"),
    ] {
        let worker = overview.workers.iter().find(|w| &w.session_id == id).unwrap();
        assert_eq!(worker.status.as_str(), want_status, "{id}");
        assert_eq!(worker.last_text.as_deref(), Some(want_text), "{id}");
    }

    let described = session.extra["antigravity"]["subagents"].as_object().unwrap();
    assert!(!described.contains_key(&coordinator));
    for (name, id) in [("alpha", &alpha), ("beta", &beta), ("gamma", &gamma)] {
        let entry = &described[id];
        assert_eq!(entry["member"], name);
        assert_eq!(entry["killed"], registry[name]["killed"]);
        assert_eq!(entry["wake_mode"], registry[name]["wake_mode"]);
        assert_eq!(entry["phase"], registry[name]["phase"]);
        assert_eq!(entry.get("added_by"), registry[name].get("added_by"));
    }
    // Each `swarm add` result names the conversation it created: the call is the spawn.
    let steps = stored(&root, &coordinator);
    assert_eq!(
        described[&alpha]["call_id"],
        call_with(&steps, "alpha-fixture").as_str()
    );
    assert_eq!(described[&beta]["call_id"], call_with(&steps, "beta-fixture").as_str());
    assert!(described[&gamma].get("call_id").is_none(), "beta added gamma");
    let nested = read[&beta].1.extra["antigravity"]["subagents"].as_object().unwrap();
    assert_eq!(
        nested[&gamma]["call_id"],
        call_with(&stored(&root, &beta), "gamma-fixture").as_str()
    );

    // A board delivery is not a child's final return, whoever the writer names as sender.
    let board: Vec<u64> = session
        .messages
        .iter()
        .filter_map(|m| native(m).get("board_message")?.as_u64())
        .collect();
    assert!(!board.is_empty());
    for (_, session) in read.values() {
        for message in &session.messages {
            assert_ne!(message.kind, MessageKind::SubagentReturn, "{}", body(session, message));
            if body(session, message).contains("[swarm] new board message #") {
                assert_eq!(
                    (message.kind, message.origin),
                    (MessageKind::InjectedContext, Origin::Harness)
                );
                assert!(native(message)["board_message"].is_u64());
            }
        }
    }
    let rows = affordances(&root, reference);
    assert_eq!(rows.iter().filter(|row| row.0 == "subagent_spawn").count(), 2);
    assert_eq!(rows.iter().filter(|row| row.0 == "subagent_return").count(), 0);

    // The writer records the coordinator as every worker's parent, including the helper's.
    for exported in [true, false] {
        let root = store(&world, "1.5.9-swarm", exported);
        let read = sessions(&root);
        let alpha_call = call_with(&stored(&root, &coordinator), "alpha-fixture");
        let beta_call = call_with(&stored(&root, &coordinator), "beta-fixture");
        let gamma_call = call_with(&stored(&root, &beta), "gamma-fixture");
        assert!(read[&coordinator].1.lineage.parent.is_none());
        for (name, id, want_path, want_call) in [
            ("alpha", &alpha, "/coordinator/alpha", alpha_call.as_str()),
            ("beta", &beta, "/coordinator/beta", beta_call.as_str()),
            ("gamma", &gamma, "/coordinator/beta/gamma", gamma_call.as_str()),
        ] {
            let (reference, session) = &read[id];
            assert_eq!(
                session.lineage.parent.as_deref(),
                Some(coordinator.as_str()),
                "{name} exported={exported}"
            );
            assert_eq!(
                session.lineage.spawned_by_tool_use.as_deref(),
                Some(want_call),
                "{name} exported={exported}"
            );
            if exported {
                assert_eq!(
                    session.lineage.agent_path.as_deref(),
                    Some(want_path),
                    "{name} exported={exported}"
                );
                let child_bag = &session.extra["antigravity"];
                assert_eq!(child_bag["member"], name);
                assert_eq!(child_bag["wake_mode"], registry[name]["wake_mode"]);
                assert_eq!(child_bag["phase"], registry[name]["phase"]);
                assert_eq!(child_bag["killed"], registry[name]["killed"]);
                assert_eq!(child_bag.get("added_by"), registry[name].get("added_by"));
                if name == "gamma" {
                    assert_eq!(child_bag["added_by_id"], beta.as_str());
                } else {
                    assert!(child_bag.get("added_by_id").is_none());
                }
            } else {
                assert_eq!(session.lineage.agent_path, None);
            }
            // The assignment is the worker's prompt; the board notice before it is not.
            let prompts: Vec<_> = session
                .messages
                .iter()
                .filter(|m| m.kind == MessageKind::Prompt)
                .collect();
            assert_eq!(prompts.len(), 1, "{name} exported={exported}");
            assert_eq!(prompts[0].origin, Origin::Subagent);
            let assignment = body(session, prompts[0]);
            assert!(assignment.contains(&format!("{name}-fixture:")), "{assignment}");
            if exported {
                let title = reference.title.as_deref().unwrap();
                assert!(!title.contains("not actually sent by the user"), "{title}");
            }
        }
    }
}

// ---- invoked subagents ------------------------------------------------------------------------

#[test]
fn an_idle_child_notice_is_its_return_to_the_parent() {
    let world = world("agy-writer-subagents");
    for exported in [true, false] {
        let root = store(&world, "1.5.9-subagents", exported);
        let read = sessions(&root);
        assert_eq!(read.len(), 3);
        let (reference, parent) = read
            .values()
            .find(|(_, session)| session.lineage.parent.is_none())
            .unwrap();
        let steps = stored(&root, &parent.id);
        let child = |marker: &str| {
            read.values()
                .find(|(_, s)| s.messages.iter().any(|m| body(s, m).contains(marker)) && s.id != parent.id)
                .map(|(_, s)| s.id.clone())
                .unwrap()
        };
        let (one, two) = (child("child-one-fixture"), child("child-two-fixture"));
        for id in [&one, &two] {
            assert_eq!(read[id].1.lineage.parent.as_deref(), Some(parent.id.as_str()));
        }
        let returns: Vec<(String, String)> = parent
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::SubagentReturn)
            .map(|m| {
                assert_eq!(m.origin, Origin::Subagent);
                let raw = body(parent, m);
                let delivered = cv_core::harness::delivered_body(Harness::Antigravity, m, &raw)
                    .unwrap()
                    .to_owned();
                (native(m)["returned_child"].as_str().unwrap().to_owned(), delivered)
            })
            .collect();
        // Child one goes idle after its reply and again after the parent's list call;
        // child two after its delayed reply.
        assert!(returns.len() >= 2, "exported={exported}: {returns:?}");
        assert!(returns
            .iter()
            .any(|(id, text)| id == &one && text == "child one final result"));
        assert!(returns
            .iter()
            .any(|(id, text)| id == &two && text.starts_with("child two ")));
        let rows = affordances(&root, reference);
        assert_eq!(
            rows.iter().filter(|row| row.0 == "subagent_return").count(),
            returns.len()
        );
        if exported {
            let spawned = parent.extra["antigravity"]["subagents"].as_object().unwrap();
            let one_call = call_with(&steps, "child-one-fixture");
            let two_call = call_with(&steps, "child-two-fixture");
            assert_eq!(spawned[&one]["call_id"], one_call.as_str());
            assert_eq!(spawned[&two]["call_id"], two_call.as_str());
            assert_eq!(
                read[&one].1.lineage.spawned_by_tool_use.as_deref(),
                Some(one_call.as_str())
            );
            assert_eq!(
                read[&two].1.lineage.spawned_by_tool_use.as_deref(),
                Some(two_call.as_str())
            );
            let lanes = cv_core::lanes::lanes_of(reference).unwrap();
            assert_eq!(lanes.len(), 2);
            for lane in &lanes {
                assert_eq!(lane.status.as_str(), "completed", "{lane:?}");
                assert!(lane.is_done() && !lane.stranded, "{lane:?}");
            }
            assert_eq!(
                reference.title.as_deref(),
                Some("parent-fixture: start two fixture children")
            );
            assert_eq!(
                read[&one].0.title.as_deref(),
                Some("child-one-fixture: reply with your result.")
            );
        } else {
            assert_eq!(
                read[&one].1.lineage.spawned_by_tool_use.as_deref(),
                Some(call_with(&steps, "child-one-fixture").as_str())
            );
            assert_eq!(
                read[&two].1.lineage.spawned_by_tool_use.as_deref(),
                Some(call_with(&steps, "child-two-fixture").as_str())
            );
        }
    }
}

// ---- prompt media ----------------------------------------------------------------------------

#[test]
fn a_prompt_image_survives_the_loss_of_its_stored_upload() {
    let world = world("agy-writer-media");
    let root = store(&world, "1.5.9-prompt-media", false);
    let (_, session) = only(sessions(&root));
    let steps = stored(&root, &session.id);
    // UserInput(19).media(9): mime(1), inline bytes(2) and the writer's stored copy(5).
    let media = nested(nested(&steps[0].payload, 19)[0], 9);
    assert_eq!(media.len(), 1);
    let upload = string(media[0], 5).unwrap();
    assert!(upload.contains("/.user_uploaded/") && !Path::new(&upload).exists());
    assert!(!nested(media[0], 2).is_empty());

    let prompt = by_id(&session, 0);
    assert_eq!(body(&session, prompt), "inline-image fixture prompt");
    let images: Vec<_> = prompt
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Image { media_type, data_ref } => Some((media_type.as_deref(), data_ref.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(images, [(Some("image/png"), Some(PIXEL))]);
    assert!(!limitations(&session).contains(&"missing_attachments".to_owned()));

    // Linked files are references into the workspace, not payloads the store owns.
    let linked = by_id(&session, 2);
    assert_eq!(body(&session, linked), "linked-attachment fixture prompt");
    assert!(linked.content.iter().all(|block| matches!(block, Block::Text { .. })));
    let scope: Vec<&str> = native(linked)["scope_items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["kind"], "file");
            item["uri"].as_str().unwrap().rsplit('/').next().unwrap()
        })
        .collect();
    assert_eq!(scope, ["linked.png", "notes.txt"]);
    // The ACP server advertises no audio support and stores nothing for an audio block.
    let audio = by_id(&session, 4);
    assert_eq!(body(&session, audio), "audio fixture prompt");
    assert_eq!(audio.content.len(), 1);
}

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

/// Not a capture: one stored prompt whose media names a file and carries no bytes.
#[test]
fn a_linked_attachment_missing_at_read_time_is_a_coverage_limitation() {
    let world = world("agy-writer-missing-attachment");
    let attachment = world.base.join("upload.png");
    let media = [len(1, b"image/png"), len(5, attachment.to_str().unwrap().as_bytes())].concat();
    let input = [len(3, &len(1, b"see attachment")), len(9, &media)].concat();
    let payload = [vec![8, 14, 32, 3], len(19, &input)].concat();
    let directory = world.base.join("conversations");
    fs::create_dir_all(&directory).unwrap();
    let conn = rusqlite::Connection::open(directory.join("11111111-2222-4333-8444-555555555555.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER,
         step_payload BLOB, step_format INTEGER DEFAULT 0);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps(idx,step_type,status,step_payload) VALUES (0,14,3,?1)",
        [payload],
    )
    .unwrap();
    drop(conn);
    let missing = only(sessions(&world.base)).1;
    assert!(limitations(&missing).contains(&"missing_attachments".to_owned()));
    assert!(missing.messages[0]
        .content
        .iter()
        .any(|block| matches!(block, Block::Image { data_ref: Some(path), .. } if Path::new(path) == attachment)));
    fs::write(&attachment, b"present").unwrap();
    let present = only(sessions(&world.base)).1;
    assert!(!limitations(&present).contains(&"missing_attachments".to_owned()));
}

#[test]
fn the_forest_names_the_call_that_spawned_each_child_or_none() {
    let world = world("agy-writer-forest-calls");
    // Children of `invoke_subagent`: the forest's call is the one the session records.
    let root = store(&world, "1.5.9-subagents", true);
    let read = sessions(&root);
    let mut linked = 0;
    for (reference, session) in read.values() {
        let Some(described) = session.extra["antigravity"]["subagents"].as_object() else {
            continue;
        };
        let stored_calls: Vec<String> = stored_calls(&stored(&root, &reference.id))
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        for agent in cv_core::subagent_tree_of(reference).unwrap() {
            let call = agent.tool_use_id.expect("the store names the spawning call");
            assert_eq!(described[&agent.session.id]["call_id"], call.as_str());
            assert!(stored_calls.contains(&call), "{call} is not a call the writer stored");
            linked += 1;
        }
    }
    assert!(linked >= 2, "the capture has two children");

    // Swarm members: direct `swarm add` children get the coordinator's call ID; helpers added
    // by another worker (`gamma`, `added_by: "beta"`) keep `tool_use_id: None` on the coordinator's
    // `AgentRef`.
    let root = store(&world, "1.5.9-swarm", true);
    let (coordinator, registry) = members(&root);
    let gamma = registry["gamma"]["conversation_id"].as_str().unwrap();
    let read = sessions(&root);
    let (reference, _) = &read[&coordinator];
    let calls: Vec<String> = stored_calls(&stored(&root, &coordinator))
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let mut direct_linked = 0;
    for agent in cv_core::subagent_tree_of(reference).unwrap() {
        if agent.session.id == gamma {
            assert_eq!(agent.tool_use_id, None, "coordinator did not issue gamma's swarm add");
        } else {
            let call = agent.tool_use_id.expect("coordinator swarm add names child");
            assert!(calls.contains(&call), "{call} is not a call the writer stored");
            direct_linked += 1;
        }
    }
    assert_eq!(direct_linked, 2);
}

#[test]
fn a_resumed_conversation_is_titled_by_its_first_prompt_in_both_layouts() {
    let world = world("agy-writer-resume-title");
    for exported in [true, false] {
        let root = store(&world, "1.5.9-compaction-resume", exported);
        let (reference, session) = only(sessions(&root));
        let first = body(&session, by_id(&session, 0));
        assert!(first.contains("resume-fixture: first turn"));
        for title in [reference.title.as_deref(), session.title.as_deref()] {
            let title = title.unwrap_or_else(|| panic!("untitled, exported={exported}"));
            assert!(
                title.contains("resume-fixture: first turn"),
                "exported={exported}: {title:?} is not the step 0 prompt"
            );
        }
    }
}

/// The capture keeps only the store. Its exported layout is rebuilt here the way the writer
/// renders the first prompt: the request in its envelope, the upload as a linked file.
#[test]
fn an_exported_prompt_whose_upload_is_gone_reads_the_image_the_store_kept() {
    let world = world("agy-writer-media-exported");
    let root = store(&world, "1.5.9-prompt-media", false);
    let (_, native_only) = only(sessions(&root));
    let steps = stored(&root, &native_only.id);
    let media = nested(nested(&steps[0].payload, 19)[0], 9);
    let upload = string(media[0], 5).unwrap();
    assert!(!Path::new(upload.trim_start_matches("file://")).exists());
    // Step.metadata(5).created_at(1).seconds(1), a varint.
    let created = nested(nested(&steps[0].payload, 5)[0], 1)[0];
    assert_eq!(created[0], 8);
    let seconds = varint(created, &mut 1) as i64;
    let stamp = chrono::DateTime::from_timestamp(seconds, 0).unwrap().to_rfc3339();
    let record = json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE",
        "created_at": stamp,
        "content": "<USER_REQUEST>\ninline-image fixture prompt\n</USER_REQUEST>",
        "media": [{"mime_type": "image/png", "uri": upload}],
    });
    let logs = root.join("brain").join(&native_only.id).join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join("transcript_full.jsonl"), format!("{record}\n")).unwrap();

    let (reference, session) = only(sessions(&root));
    assert!(reference.path.ends_with("transcript_full.jsonl"));
    let prompt = by_id(&session, 0);
    assert_eq!(body(&session, prompt), "inline-image fixture prompt");
    let images: Vec<_> = prompt
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Image { media_type, data_ref } => Some((media_type.as_deref(), data_ref.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(images, [(Some("image/png"), Some(PIXEL))]);
    assert!(!limitations(&session).contains(&"missing_attachments".to_owned()));
    assert_eq!(
        ids(&session),
        ids(&native_only),
        "both layouts describe one conversation"
    );

    // An export that is another revision of the prompt keeps its own link and says so.
    let record = json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE",
        "created_at": stamp,
        "content": "<USER_REQUEST>\na different request\n</USER_REQUEST>",
        "media": [{"mime_type": "image/png", "uri": upload}],
    });
    fs::write(logs.join("transcript_full.jsonl"), format!("{record}\n")).unwrap();
    let (_, session) = only(sessions(&root));
    assert!(body(&session, by_id(&session, 0)).contains("a different request"));
    assert!(limitations(&session).contains(&"missing_attachments".to_owned()));
}

#[test]
fn unsupported_native_fields_names_only_captures_with_a_field_nothing_reads() {
    let world = world("agy-writer-unsupported");
    let reports = |version: &str, exported: bool| -> Vec<bool> {
        let root = store(&world, version, exported);
        sessions(&root)
            .values()
            .map(|(_, session)| limitations(session).contains(&"unsupported_native_fields".to_owned()))
            .collect()
    };
    // Permissions, cancellation, background tasks, swarm members, children, prompt media,
    // native background media and compaction checkpoints are all interpreted: none of the
    // 8 real-writer captures leaves a field unread.
    for version in [
        "1.5.9-permissions",
        "1.5.9-interruption",
        "1.5.9-background-tasks",
        "1.5.9-swarm",
        "1.5.9-subagents",
        "1.5.9-prompt-media",
        "1.5.9-native-background-media",
        "1.5.9-compaction-resume",
    ] {
        for exported in [false, true] {
            assert!(
                reports(version, exported).iter().all(|reported| !reported),
                "{version} exported={exported}"
            );
        }
    }
}
