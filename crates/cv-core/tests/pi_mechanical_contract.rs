//! Hermetic Pi discovery and shared-IR envelope regressions; no real session input or home.
use cv_core::env::Env;
use cv_core::harness::{pi::Pi, Adapter};
use cv_core::ir::{tool_index, Block, Message};
use cv_core::stream::{Demand, Flow, MessageSink};
use cv_testkit::{jsonl, World};
use serde_json::json;
use std::fs;

#[test]
fn explicit_agent_dir_does_not_read_default_home() {
    let world = World::in_process("pi-exclusive-root", Env::enter_isolated);
    let default = world.home.join(".pi/agent/sessions");
    let other = world.base.join("other-agent");
    fs::create_dir_all(&default).unwrap();
    fs::create_dir_all(other.join("sessions")).unwrap();
    world.scope.set("PI_CODING_AGENT_DIR", &other);
    assert_eq!(Pi::detect().roots(), vec![other.join("sessions")]);
}

#[test]
fn absent_explicit_dir_does_not_fall_back_to_home() {
    let world = World::in_process("pi-missing-root", Env::enter_isolated);
    fs::create_dir_all(world.home.join(".pi/agent/sessions")).unwrap();
    world
        .scope
        .set("PI_CODING_AGENT_DIR", world.base.join("missing-agent"));
    assert!(Pi::detect().roots().is_empty());
}

#[test]
fn empty_override_uses_only_the_hermetic_default() {
    let world = World::in_process("pi-default-root", Env::enter_isolated);
    let default = world.home.join(".pi/agent/sessions");
    fs::create_dir_all(&default).unwrap();
    world.scope.set("PI_CODING_AGENT_DIR", "");
    assert_eq!(Pi::detect().roots(), vec![default]);
}

#[derive(Default)]
struct Collect(Vec<Message>);
impl MessageSink for Collect {
    fn message(&mut self, message: Message) -> Flow {
        self.0.push(message);
        Flow::Continue
    }
}

#[test]
fn out_of_order_results_pair_in_shared_ir_and_keep_images() {
    let world = World::in_process("pi-tool-envelope", Env::enter_isolated);
    let dir = world.base.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("invented.jsonl");
    fs::write(
        &path,
        jsonl(&[
            json!({"type":"session","version":3,"id":"session","cwd":"/invented"}),
            json!({"type":"message","id":"calls","parentId":null,"message":{
                "role":"assistant","content":[
                    {"type":"toolCall","id":"call-a","name":"read","arguments":{"path":"invented-a"}},
                    {"type":"toolCall","id":"call-b","name":"read","arguments":{"path":"invented-b"}}
                ]
            }}),
            json!({"type":"message","id":"result-b","parentId":"calls","message":{
                "role":"toolResult","toolCallId":"call-b","toolName":"read","isError":true,
                "details":{"invented":true},"content":[
                    {"type":"text","text":"synthetic result b"},
                    {"type":"image","mimeType":"image/png","data":"c3ludGhldGlj"}
                ]
            }}),
            json!({"type":"message","id":"result-a","parentId":"result-b","message":{
                "role":"toolResult","toolCallId":"call-a","toolName":"read","isError":false,
                "content":[{"type":"text","text":"synthetic result a"}]
            }})
        ]),
    )
    .unwrap();
    let adapter = Pi::with_roots(vec![dir]);
    let refs = adapter.discover().unwrap().refs;
    assert_eq!(refs.len(), 1);
    let mut sink = Collect::default();
    adapter
        .stream(&refs[0], &Demand::LEAN, &mut sink)
        .unwrap();
    let calls = tool_index(&sink.0);
    assert_eq!(calls.len(), 2, "the shared tool-call index consumed both calls");
    let results: Vec<_> = sink
        .0
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, is_error, tool_name, details, .. } =>
                Some((tool_use_id.as_str(), *is_error, tool_name.as_deref(), details.as_ref())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "tool results must exist in the shared IR, not only a Pi bag");
    assert_eq!(results[0].0, "call-b");
    assert_eq!(results[1].0, "call-a");
    for (id, _, name, _) in &results {
        assert_eq!(calls.get(id).unwrap().name, name.unwrap());
    }
    assert!(results[0].1);
    assert!(!results[1].1);
    assert_eq!(results[0].3.unwrap(), &json!({"invented":true}));
    assert_eq!(
        sink.0.iter().flat_map(|m| &m.content)
            .filter(|b| matches!(b, Block::Image { .. })).count(),
        1,
        "the tool-result envelope must not discard its image parts"
    );
}

fn header_records(cwd: &std::path::Path, compacted: bool) -> Vec<serde_json::Value> {
    let mut records = vec![
        json!({"type":"session","version":3,"id":"header-synthetic","cwd":cwd,
            "timestamp":"2026-11-19T10:00:00.000Z"}),
        json!({"type":"message","id":"system","parentId":null,"message":{
            "role":"system","content":[{"type":"text","text":"invented system"}]}}),
        json!({"type":"message","id":"user","parentId":"system","message":{
            "role":"user","content":[{"type":"text","text":"invented brass gulls"}]}}),
        json!({"type":"message","id":"assistant","parentId":"user","message":{
            "role":"assistant","content":[{"type":"text","text":"invented reply"}]}}),
    ];
    if compacted {
        records.push(json!({"type":"compaction","id":"cut","parentId":"assistant",
            "firstKeptEntryId":"user","summary":"invented summary"}));
    }
    records.extend([
        json!({"type":"message","id":"after","parentId":if compacted {"cut"} else {"assistant"},
            "message":{"role":"user","content":[{"type":"text","text":"invented follow-up"}]}}),
        json!({"type":"message","id":"result","parentId":"after","message":{
            "role":"toolResult","toolCallId":"invented-call","toolName":"read",
            "content":[{"type":"text","text":"invented result"}]}}),
    ]);
    records
}

#[test]
fn discovery_keeps_native_header_metadata_and_conversational_count() {
    let world = World::in_process("pi-header-discovery", Env::enter_isolated);
    let dir = world.base.join("sessions");
    let cwd = world.base.join("invented-project");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("synthetic.jsonl");
    let bytes = jsonl(&header_records(&cwd, true));
    fs::write(&path, &bytes).unwrap();
    let refs = Pi::with_roots(vec![dir]).discover().unwrap().refs;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, "header-synthetic");
    assert_eq!(refs[0].cwd.as_deref(), Some(cwd.as_path()), "discovery must retain native cwd");
    let created = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z").unwrap().to_utc();
    assert_eq!(refs[0].created_at, Some(created));
    assert_eq!(refs[0].message_count, Some(3), "only user and assistant entries are conversational turns");
    assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
}

#[test]
fn projected_and_complete_reads_keep_header_metadata_outside_the_message_chain() {
    use cv_core::ir::{Harness, MessageKind, SessionRef};
    for compacted in [false, true] {
        let world = World::in_process("pi-header-stream", Env::enter_isolated);
        let dir = world.base.join("sessions");
        let cwd = world.base.join("invented-project");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synthetic.jsonl");
        let bytes = jsonl(&header_records(&cwd, compacted));
        fs::write(&path, &bytes).unwrap();
        // Do not let discovery metadata hide a stream that forgot to read the native header.
        let r = SessionRef::new("header-synthetic", Harness::Pi, path.clone());
        let adapter = Pi::with_roots(vec![dir]);
        for complete in [false, true] {
            let mut sink = Collect::default();
            let session = adapter.stream(&r, if complete { &Demand::COMPLETE } else { &Demand::LEAN }, &mut sink).unwrap();
            assert_eq!(session.cwd.as_deref(), Some(cwd.as_path()), "stream must retain native cwd");
            let created = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z").unwrap().to_utc();
            assert_eq!(session.created_at, Some(created));
            assert_eq!(sink.0.iter().filter(|m| m.id.as_deref() == Some("user")).count(), 1);
            if compacted && !complete {
                assert_eq!(sink.0[0].kind, MessageKind::CompactionBoundary);
                assert!(!sink.0.iter().any(|m| m.id.as_deref() == Some("system")));
            }
        }
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
    }
}

fn assistant_messages(bodies: &[serde_json::Value]) -> Vec<Message> {
    let world = World::in_process("pi-assistant-metadata", Env::enter_isolated);
    let dir = world.base.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let mut entries = vec![json!({"type":"session","version":3,"id":"synthetic-session"})];
    for (i, body) in bodies.iter().enumerate() {
        let parent = (i > 0).then(|| format!("assistant-{}", i - 1));
        entries.push(json!({"type":"message","id":format!("assistant-{i}"),
            "parentId":parent,"message":body}));
    }
    fs::write(dir.join("synthetic.jsonl"), jsonl(&entries)).unwrap();
    let adapter = Pi::with_roots(vec![dir]);
    let refs = adapter.discover().unwrap().refs;
    assert_eq!(refs.len(), 1);
    let mut sink = Collect::default();
    adapter.stream(&refs[0], &Demand::LEAN, &mut sink).unwrap();
    assert_eq!(sink.0.len(), bodies.len());
    sink.0
}

#[test]
fn assistant_metadata_keeps_accounting_and_provider_facts_without_complete() {
    use cv_core::ir::{MessageKind, Origin};
    let usage = json!({"input":7,"output":3,"cacheRead":0,"cacheWrite":2,"reasoning":1,
        "cost":{"total":0.125,"inventedBucket":0.025},"contextUsage":{"invented":true}});
    let messages = assistant_messages(&[json!({"role":"assistant","model":"synthetic-model",
        "content":[{"type":"text","text":"invented reply"}],"usage":usage,
        "api":"synthetic-api","provider":"synthetic-provider","responseModel":"synthetic-response-model",
        "responseId":"synthetic-response","stopReason":"toolUse","diagnostics":{"invented":true}})]);
    let message = &messages[0];
    let u = message.usage.as_ref().expect("assistant usage must reach the shared IR");
    assert_eq!((u.input_tokens, u.output_tokens), (Some(7), Some(3)));
    assert_eq!((u.cache_read_tokens, u.cache_creation_tokens), (Some(0), Some(2)));
    assert_eq!(u.reasoning_tokens, Some(1));
    assert_eq!(u.cost_usd, Some(0.125));
    assert_eq!(message.model.as_deref(), Some("synthetic-model"));
    assert_eq!((message.kind, message.origin), (MessageKind::Reply, Origin::Model));
    let bag = &message.extra["pi"];
    assert_eq!(bag["provider"], "synthetic-provider", "provider facts must survive ordinary reads");
    assert_eq!(bag["api"], "synthetic-api");
    assert_eq!(bag["response_model"], "synthetic-response-model");
    assert_eq!(bag["response_id"], "synthetic-response");
    assert_eq!(bag["stop_reason"], "toolUse");
    assert_eq!(bag["diagnostics"], json!({"invented":true}));
    assert_eq!(bag["usage_raw"], usage);
    assert!(bag.get("native").is_none());
    assert!(!message.extra.contains_key("openclaw"));
}

#[test]
fn assistant_provider_errors_are_not_counted_as_model_replies() {
    use cv_core::ir::{MessageKind, Origin};
    let messages = assistant_messages(&[
        json!({"role":"assistant","content":[],"stopReason":"error","usage":{"input":0,"output":0}}),
        json!({"role":"assistant","content":[],"stopReason":"stop","errorMessage":"synthetic failure",
            "errorCode":"invented-code","errorType":"invented-type"}),
    ]);
    for message in &messages {
        assert_eq!((message.kind, message.origin), (MessageKind::Error, Origin::Harness),
            "assistant provider failures must not become successful model replies");
        assert!(!message.extra.contains_key("openclaw"));
    }
    let usage = messages[0].usage.as_ref().expect("zero accounting is present, not missing");
    assert_eq!((usage.input_tokens, usage.output_tokens), (Some(0), Some(0)));
    assert_eq!(messages[0].extra["pi"]["error"], json!({}));
    assert!(messages[1].usage.is_none(), "absent usage must not be invented as zero");
    assert_eq!(messages[1].extra["pi"]["error"], json!({"error_message":"synthetic failure",
        "error_code":"invented-code","error_type":"invented-type"}));
}
