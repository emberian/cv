//! Hermetic Pi discovery, datapath, and shared-IR contract regressions; no real session input or home.
use cv_core::env::Env;
use cv_core::harness::{pi::Pi, Adapter};
use cv_core::ir::{
    tool_index, Block, Harness, Message, MessageKind, Origin, Session, SessionRef, CARRIER_KEY, CV_NAMESPACE,
};
use cv_core::offsets::OFFSET_KEY;
use cv_core::stream::{Demand, Flow, MessageSink};
use cv_testkit::{jsonl, World, CLEARED_ENV};
use serde_json::{json, Value};
use std::fs;

#[test]
fn cleared_env_includes_pi_coding_agent_dir() {
    assert!(
        CLEARED_ENV.contains(&"PI_CODING_AGENT_DIR"),
        "CLEARED_ENV must strip ambient PI_CODING_AGENT_DIR"
    );
    let world = World::new("pi-cleared-cmd");
    let cmd = world.command("sh");
    let envs: Vec<_> = cmd.get_envs().collect();
    assert!(
        envs.iter().any(|(k, v)| *k == "PI_CODING_AGENT_DIR" && v.is_none()),
        "World::command must explicitly env_remove PI_CODING_AGENT_DIR"
    );
}

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
    world.scope.set("PI_CODING_AGENT_DIR", world.base.join("missing-agent"));
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
struct Collect {
    meta_before_first_msg: Option<Session>,
    meta_calls: usize,
    messages: Vec<Message>,
}

impl MessageSink for Collect {
    fn meta(&mut self, s: &Session) {
        self.meta_calls += 1;
        if self.messages.is_empty() {
            self.meta_before_first_msg = Some(s.clone());
        }
    }

    fn message(&mut self, message: Message) -> Flow {
        self.messages.push(message);
        Flow::Continue
    }
}

fn assert_nested_extra(s: &Session) {
    let namespace = |k: &str| Harness::parse(k).is_some_and(|h| h.as_str() == k) || k == CV_NAMESPACE;
    for (k, v) in &s.extra {
        assert!(namespace(k), "flat session extra key {k:?}");
        assert!(v.is_object(), "session extra[{k:?}] must be an object");
    }
    for m in &s.messages {
        for (k, v) in &m.extra {
            if k == CARRIER_KEY || k == OFFSET_KEY {
                continue;
            }
            assert!(namespace(k), "flat message extra key {k:?} on {:?}", m.id);
            assert!(v.is_object(), "message extra[{k:?}] must be an object");
        }
    }
}

#[test]
fn out_of_order_results_pair_in_shared_ir_and_keep_images_and_redacted_thinking() {
    let world = World::in_process("pi-tool-envelope", Env::enter_isolated);
    let dir = world.base.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("invented.jsonl");
    fs::write(
        &path,
        jsonl(&[
            json!({"type":"session","version":3,"id":"session","cwd":"/invented","parentSession":"parent-sess-0"}),
            json!({"type":"message","id":"calls","parentId":null,"message":{
                "role":"assistant","content":[
                    {"type":"thinking","thinking":"","thinkingSignature":"sig-r","redacted":true},
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
            }}),
        ]),
    )
    .unwrap();
    let adapter = Pi::with_roots(vec![dir]);
    let refs = adapter.discover().unwrap().refs;
    assert_eq!(refs.len(), 1);
    let mut sink = Collect::default();
    let session = adapter.stream(&refs[0], &Demand::LEAN, &mut sink).unwrap();
    assert_eq!(session.lineage.forked_from.as_deref(), Some("parent-sess-0"));
    assert_eq!(sink.meta_calls, 1, "sink.meta(&s) must be called once before messages");
    assert!(sink.meta_before_first_msg.is_some());

    // Redacted thinking preserved cleanly.
    match &sink.messages[0].content[0] {
        Block::Thinking {
            redacted,
            encrypted,
            signature,
            ..
        } => {
            assert!(*redacted, "thinking.redacted must be preserved");
            assert_eq!(encrypted.as_deref(), Some("redacted"));
            assert_eq!(signature.as_deref(), Some("sig-r"));
        }
        other => panic!("expected Thinking block, got {other:?}"),
    }

    let calls = tool_index(&sink.messages);
    assert_eq!(calls.len(), 2, "the shared tool-call index consumed both calls");
    let results: Vec<_> = sink
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolResult {
                tool_use_id,
                is_error,
                tool_name,
                details,
                ..
            } => Some((tool_use_id.as_str(), *is_error, tool_name.as_deref(), details.as_ref())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "tool results must exist in the shared IR");
    assert_eq!(results[0].0, "call-b");
    assert_eq!(results[1].0, "call-a");
    for (id, _, name, _) in &results {
        assert_eq!(calls.get(id).unwrap().name, name.unwrap());
    }
    assert!(results[0].1);
    assert!(!results[1].1);
    assert_eq!(results[0].3.unwrap(), &json!({"invented":true}));
    assert_eq!(
        sink.messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, Block::Image { .. }))
            .count(),
        1,
        "the tool-result envelope must not discard its image parts"
    );
}

fn header_records(cwd: &std::path::Path, compacted: bool) -> Vec<serde_json::Value> {
    let mut records = vec![
        json!({"type":"session","version":3,"id":"header-synthetic","cwd":cwd,
            "timestamp":"2026-11-19T10:00:00.000Z","model":"header-default-model"}),
        json!({"type":"message","id":"system","parentId":null,"timestamp":"2026-11-19T10:00:01.000Z","message":{
            "role":"system","content":[{"type":"text","text":"invented system"}]}}),
        json!({"type":"message","id":"user","parentId":"system","timestamp":"2026-11-19T10:00:02.000Z","message":{
            "role":"user","content":[{"type":"text","text":"invented brass gulls"}]}}),
        json!({"type":"message","id":"assistant","parentId":"user","timestamp":"2026-11-19T10:00:03.000Z","message":{
            "role":"assistant","content":[{"type":"text","text":"invented reply"}]}}),
    ];
    if compacted {
        records.push(json!({"type":"compaction","id":"cut","parentId":"assistant",
            "timestamp":"2026-11-19T10:00:04.000Z",
            "firstKeptEntryId":"user","summary":"invented summary","tokensBefore":4096,"fromHook":true}));
    }
    records.extend([
        json!({"type":"message","id":"after","parentId":if compacted {"cut"} else {"assistant"},
            "timestamp":"2026-11-19T10:00:05.000Z",
            "message":{"role":"user","content":[{"type":"text","text":"invented follow-up"}]}}),
        json!({"type":"message","id":"result","parentId":"after","timestamp":"2026-11-19T10:00:06.000Z","message":{
            "role":"toolResult","toolCallId":"invented-call","toolName":"read",
            "content":[{"type":"text","text":"invented result"}]}}),
    ]);
    records
}

#[test]
fn discovery_keeps_native_header_metadata_title_timestamps_and_projected_turn_count() {
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
    assert_eq!(refs[0].cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(refs[0].title.as_deref(), Some("invented brass gulls"));
    let created = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z")
        .unwrap()
        .to_utc();
    let updated = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:06Z")
        .unwrap()
        .to_utc();
    assert_eq!(refs[0].created_at, Some(created));
    assert_eq!(refs[0].updated_at, Some(updated));
    assert_eq!(refs[0].message_count, Some(3));
    assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
}

#[test]
fn projected_and_complete_reads_keep_header_system_prompt_and_compaction_contracts() {
    for compacted in [false, true] {
        let world = World::in_process("pi-header-stream", Env::enter_isolated);
        let dir = world.base.join("sessions");
        let cwd = world.base.join("invented-project");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synthetic.jsonl");
        let bytes = jsonl(&header_records(&cwd, compacted));
        fs::write(&path, &bytes).unwrap();
        let r = SessionRef::new("header-synthetic", Harness::Pi, path.clone());
        let adapter = Pi::with_roots(vec![dir]);
        for complete in [false, true] {
            let mut sink = Collect::default();
            let demand = if complete { &Demand::COMPLETE } else { &Demand::LEAN };
            let mut session = adapter.stream(&r, demand, &mut sink).unwrap();
            assert_eq!(session.cwd.as_deref(), Some(cwd.as_path()));
            assert_eq!(session.title.as_deref(), Some("invented brass gulls"));
            assert_eq!(session.model.as_deref(), Some("header-default-model"));
            let created = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z")
                .unwrap()
                .to_utc();
            let updated = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:06Z")
                .unwrap()
                .to_utc();
            assert_eq!(session.created_at, Some(created));
            assert_eq!(session.updated_at, Some(updated));
            assert_eq!(sink.meta_calls, 1);

            if !compacted || complete {
                let sys = sink
                    .messages
                    .iter()
                    .find(|m| m.id.as_deref() == Some("system"))
                    .expect("system message present");
                assert_eq!(
                    sys.kind,
                    MessageKind::SystemPrompt,
                    "system role must be SystemPrompt, not Notice"
                );
                assert_eq!(session.system_prompt.as_deref(), Some("invented system"));
            }
            if compacted && !complete {
                assert_eq!(sink.messages[0].kind, MessageKind::CompactionBoundary);
                assert!(sink.messages[0].is_synthesized());
                assert_eq!(sink.messages[0].id.as_deref(), Some("cut"));
                assert_eq!(sink.messages[1].kind, MessageKind::CompactionSummary);
                assert_eq!(sink.messages[1].id, None, "summary must not duplicate boundary id");
                assert_eq!(sink.messages[1].parent_id.as_deref(), Some("cut"));
                assert!(!sink.messages.iter().any(|m| m.id.as_deref() == Some("system")));

                let boundaries = cv_core::compaction::detect(&r, true).unwrap();
                assert_eq!(boundaries.len(), 1);
                assert_eq!(boundaries[0].boundary_uuid.as_deref(), Some("cut"));
                assert_eq!(boundaries[0].summary_msg_idx, Some(1));
                assert_eq!(boundaries[0].pre_tokens, Some(4096));
                assert_eq!(boundaries[0].trigger.as_deref(), Some("hook"));
                assert_eq!(boundaries[0].summary.as_deref(), Some("invented summary"));

                let ctx_rows = cv_core::context::read(&adapter, &r).unwrap();
                assert_eq!(ctx_rows.len(), 1);
                assert_eq!(ctx_rows[0].kind, "compaction_boundary");
                assert_eq!(ctx_rows[0].pre_compaction_tokens, Some(4096));
            }
            session.messages = sink.messages;
            assert_nested_extra(&session);
        }
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
    }
}

#[test]
fn discovery_message_count_matches_full_read_is_turn_across_all_exclusions() {
    let world = World::in_process("pi-count-parity", Env::enter_isolated);
    let dir = world.base.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("complex.jsonl");
    fs::write(
        &path,
        jsonl(&[
            json!({"type":"session","version":3,"id":"count-parity","cwd":"/work"}),
            json!({"type":"session_info","id":"si","parentId":"count-parity","name":"Named Parity Session"}),
            // Pre-compaction dropped turn:
            json!({"type":"message","id":"dropped-pre","parentId":"si","message":{"role":"user","content":"dropped pre-compaction"}}),
            // Kept pre-compaction turns:
            json!({"type":"message","id":"kept-u1","parentId":"dropped-pre","message":{"role":"user","content":"kept user 1"}}),
            json!({"type":"message","id":"kept-a1","parentId":"kept-u1","message":{"role":"assistant","content":[{"type":"text","text":"kept reply 1"}]}}),
            // Abandoned branch off kept-a1 (2 turns that must NOT be counted):
            json!({"type":"message","id":"dead-u","parentId":"kept-a1","message":{"role":"user","content":"dead prompt"}}),
            json!({"type":"message","id":"dead-a","parentId":"dead-u","message":{"role":"assistant","content":[{"type":"text","text":"dead reply"}]}}),
            // Compaction keeping from kept-u1:
            json!({"type":"compaction","id":"c1","parentId":"kept-a1","firstKeptEntryId":"kept-u1","summary":"sum"}),
            // Post-compaction turn that will be null-dropped by context_edit:
            json!({"type":"message","id":"edit-dropped","parentId":"c1","message":{"role":"user","content":"will be dropped by edit"}}),
            // Failed assistant messages (stopReason: error and errorMessage):
            json!({"type":"message","id":"err-1","parentId":"edit-dropped","message":{"role":"assistant","stopReason":"error","content":[]}}),
            json!({"type":"message","id":"err-2","parentId":"err-1","message":{"role":"assistant","stopReason":"stop","errorMessage":"boom","content":[]}}),
            // Surviving post-compaction prompt + reply:
            json!({"type":"message","id":"post-u","parentId":"err-2","message":{"role":"user","content":"live post prompt"}}),
            json!({"type":"message","id":"post-a","parentId":"post-u","message":{"role":"assistant","content":[{"type":"text","text":"live post reply"}]}}),
            // Context edit dropping `edit-dropped`:
            json!({"type":"context_edit","id":"ce1","parentId":"post-a","targetId":"edit-dropped","replacement":null}),
        ]),
    )
    .unwrap();

    let adapter = Pi::with_roots(vec![dir]);
    let refs = adapter.discover().unwrap().refs;
    assert_eq!(refs.len(), 1);
    let r = &refs[0];
    assert_eq!(r.title.as_deref(), Some("Named Parity Session"));
    // Exactly 4 turns survive: kept-u1, kept-a1, post-u, post-a.
    assert_eq!(r.message_count, Some(4));

    for demand in [
        Demand::LEAN,
        Demand::LEAN.with_spans(),
        Demand::USAGE,
        Demand::FACTS,
        Demand::RICH,
    ] {
        let s = cv_core::stream::collect_with(&adapter, r, &demand).unwrap();
        let full_turns = s.messages.iter().filter(|m| m.is_turn()).count();
        assert_eq!(r.message_count, Some(full_turns), "mismatch at {demand:?}");
    }
}

#[test]
fn crlf_unterminated_eof_malformed_lines_offsets_and_lazy_spans() {
    let world = World::in_process("pi-crlf-spans", Env::enter_isolated);
    let dir = world.base.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("crlf-spans.jsonl");

    let big_user = "Uλ".repeat(40_000); // > 64 KiB
    let big_reply = "Aπ".repeat(40_000); // > 64 KiB
    let big_arg = "X".repeat(70_000); // > 64 KiB
    let big_result = "RΩ".repeat(40_000); // > 64 KiB

    let lines = [
        json!({"type":"session","version":3,"id":"crlf-s","cwd":"/work"}).to_string(),
        json!({"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":big_user}}).to_string(),
        "   {corrupt mid-file json line".to_string(),
        json!({"type":"message","id":"m2","parentId":"m1","message":{
            "role":"assistant",
            "model":"pi-model-1",
            "content":[
                {"type":"text","text":big_reply},
                {"type":"toolCall","id":"tc1","name":"bash","arguments":{"cmd":big_arg}}
            ]
        }})
        .to_string(),
        json!({"type":"message","id":"m3","parentId":"m2","message":{
            "role":"toolResult","toolCallId":"tc1","toolName":"bash","isError":false,
            "content":[{"type":"text","text":big_result}]
        }})
        .to_string(),
        json!({"type":"message","id":"m4","parentId":"m3","message":{"role":"user","content":"original tail"}})
            .to_string(),
        // No trailing newline on the final line!
        json!({"type":"context_edit","id":"e1","parentId":"m4","targetId":"m4","replacement":{"content":"edited tail"}})
            .to_string(),
    ];
    let raw = lines.join("\r\n");
    assert!(!raw.ends_with('\n'));
    fs::write(&path, raw.as_bytes()).unwrap();

    let adapter = Pi::with_roots(vec![dir]);
    let r = &adapter.discover().unwrap().refs[0];
    assert_eq!(r.message_count, Some(3), "m1 (user), m2 (assistant), m4 (edited user)");

    let spanned = cv_core::stream::collect_with(&adapter, r, &Demand::LEAN.with_spans().with_offsets()).unwrap();
    assert_eq!(
        spanned
            .cv_extra()
            .and_then(|c| c.get("skipped_lines"))
            .and_then(Value::as_u64),
        Some(1),
        "corrupt line counted in extra.cv.skipped_lines"
    );
    assert_eq!(spanned.model.as_deref(), Some("pi-model-1"));
    assert_eq!(spanned.messages.len(), 4);

    // Unedited entries stamp cv_byte_offset; edited entry (m4) omits it.
    assert!(spanned.messages[0].extra.contains_key(OFFSET_KEY));
    assert!(spanned.messages[1].extra.contains_key(OFFSET_KEY));
    assert!(spanned.messages[2].extra.contains_key(OFFSET_KEY));
    assert!(!spanned.messages[3].extra.contains_key(OFFSET_KEY));
    assert_eq!(spanned.messages[3].text().as_deref(), Some("edited tail"));

    // >64 KiB text and tool arguments stay lazy (`Text::Source` / `Json::Source`).
    assert!(spanned.messages[0].content[0].text().unwrap().is_lazy());
    assert!(spanned.messages[1].content[0].text().unwrap().is_lazy());
    match &spanned.messages[1].content[1] {
        Block::ToolUse { input, .. } => assert!(matches!(input, cv_core::Json::Source(_))),
        other => panic!("expected ToolUse, got {other:?}"),
    }
    match &spanned.messages[2].content[0] {
        Block::ToolResult { content, .. } => assert!(content.is_lazy(), "ToolResult >64 KiB must stay lazy"),
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // Mutating the backing file invalidates lazy source spans cleanly instead of returning corrupt text.
    let mut clone_ok = spanned.clone();
    clone_ok.materialize().unwrap();
    assert_eq!(clone_ok.messages[0].text().as_deref(), Some(big_user.as_str()));

    fs::write(&path, b"{}\n").unwrap();
    let mut clone_mutated = spanned;
    assert!(
        clone_mutated.materialize().is_err(),
        "mutated source must reject lazy span resolution"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_subdirectory_is_reported_in_discovered_skipped() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let world = World::in_process("pi-walk-skip", Env::enter_isolated);
    let root = world.base.join("sessions");
    let locked = root.join("locked-subdir");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let discovered = Pi::with_roots(vec![root]).discover().unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(discovered.skipped.len(), 1, "{:?}", discovered.skipped);
    assert_eq!(discovered.skipped[0].path, locked);
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
    assert_eq!(sink.messages.len(), bodies.len());
    sink.messages
}

#[test]
fn assistant_metadata_keeps_accounting_and_provider_facts_without_complete() {
    let usage = json!({"input":7,"output":3,"cacheRead":0,"cacheWrite":2,"reasoning":1,
        "cost":{"total":0.125,"inventedBucket":0.025},"contextUsage":{"invented":true}});
    let messages = assistant_messages(&[json!({"role":"assistant","model":"synthetic-model",
        "content":[{"type":"text","text":"invented reply"}],"usage":usage,
        "api":"synthetic-api","provider":"synthetic-provider","responseModel":"synthetic-response-model",
        "responseId":"synthetic-response","stopReason":"toolUse","diagnostics":{"invented":true}})]);
    let message = &messages[0];
    let u = message
        .usage
        .as_ref()
        .expect("assistant usage must reach the shared IR");
    assert_eq!((u.input_tokens, u.output_tokens), (Some(7), Some(3)));
    assert_eq!((u.cache_read_tokens, u.cache_creation_tokens), (Some(0), Some(2)));
    assert_eq!(u.reasoning_tokens, Some(1));
    assert_eq!(u.cost_usd, Some(0.125));
    assert_eq!(message.model.as_deref(), Some("synthetic-model"));
    assert_eq!((message.kind, message.origin), (MessageKind::Reply, Origin::Model));
    let bag = &message.extra["pi"];
    assert_eq!(bag["provider"], "synthetic-provider");
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
    let messages = assistant_messages(&[
        json!({"role":"assistant","content":[],"stopReason":"error","usage":{"input":0,"output":0}}),
        json!({"role":"assistant","content":[],"stopReason":"stop","errorMessage":"synthetic failure",
            "errorCode":"invented-code","errorType":"invented-type"}),
    ]);
    for message in &messages {
        assert_eq!((message.kind, message.origin), (MessageKind::Error, Origin::Harness));
        assert!(!message.extra.contains_key("openclaw"));
    }
    let usage = messages[0]
        .usage
        .as_ref()
        .expect("zero accounting is present, not missing");
    assert_eq!((usage.input_tokens, usage.output_tokens), (Some(0), Some(0)));
    assert_eq!(messages[0].extra["pi"]["error"], json!({}));
    assert!(messages[1].usage.is_none());
    assert_eq!(
        messages[1].extra["pi"]["error"],
        json!({"error_message":"synthetic failure","error_code":"invented-code","error_type":"invented-type"})
    );
}
