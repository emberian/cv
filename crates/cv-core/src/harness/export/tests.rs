use super::*;
use crate::emit::frozen::Frozen;
use crate::json::{ReadInput, Validation};
use crate::stream::{CollectSink, Demand, Tier};
use serde_json::{json, Value};

/// A read session, frozen: its length and SHA-256, the temporary archive path aside.
fn digest(session: &Session) -> Value {
    let mut session = session.clone();
    session.source_path = None;
    let bytes = serde_json::to_vec(&session).unwrap();
    json!([bytes.len(), crate::digest::sha256_hex(&bytes)])
}

/// A discovered ref, frozen: every field but the temporary archive path.
fn reference(r: &SessionRef) -> Value {
    let mut value = serde_json::to_value(r).unwrap();
    value.as_object_mut().unwrap().remove("path");
    value
}

/// One conversation as cv reads it: alone in an archive, discovered, then streamed in full.
fn read(kind: Kind, conv: &Value) -> (SessionRef, Session) {
    let dir = std::env::temp_dir().join(format!("cv-export-read-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    std::fs::write(&path, serde_json::to_vec(&json!([conv])).unwrap()).unwrap();
    let r = discover_kind_in(kind, vec![path]).remove(0);
    let mut sink = CollectSink::default();
    let mut session = native::stream(kind, &r, &Demand::RICH, &mut sink).unwrap();
    session.messages = sink.messages;
    std::fs::remove_dir_all(dir).unwrap();
    (r, session)
}

#[test]
fn chatgpt_walks_mapping_active_branch() {
    let conv = json!({
        "id": "c1", "title": "Soy Sauce", "default_model_slug": "gpt-4o",
        "create_time": 1_700_000_000.0, "update_time": 1_700_000_100.0,
        "current_node": "n2",
        "mapping": {
            "root": { "id": "root", "parent": null, "children": ["n1"], "message": null },
            "n1": { "id": "n1", "parent": "root", "children": ["n2"],
                "message": { "author": { "role": "user" }, "content": { "content_type": "text", "parts": ["how to brew?"] }, "create_time": 1_700_000_001.0, "recipient": "all" } },
            "n2": { "id": "n2", "parent": "n1", "children": [],
                "message": { "author": { "role": "assistant" }, "content": { "content_type": "text", "parts": ["soak the beans"] }, "create_time": 1_700_000_002.0, "recipient": "all" } },
            // an off-branch sibling that current_node does NOT reach — must be excluded
            "n9": { "id": "n9", "parent": "n1", "children": [],
                "message": { "author": { "role": "assistant" }, "content": { "content_type": "text", "parts": ["DEAD BRANCH"] }, "recipient": "all" } }
        }
    });
    let (r, session) = read(Kind::ChatGpt, &conv);
    let msgs = session.messages;
    assert_eq!(msgs.len(), 2, "only the root->current_node branch");
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    assert!(msgs[0].text().unwrap().contains("how to brew"));
    assert!(msgs[1].text().unwrap().contains("soak the beans"));
    assert!(!msgs
        .iter()
        .any(|m| m.text().unwrap_or_default().contains("DEAD BRANCH")));

    assert_eq!(r.id, "c1");
    assert_eq!(r.harness, Harness::ChatGptExport);
    assert_eq!(r.title.as_deref(), Some("Soy Sauce"));
    assert_eq!(r.message_count, Some(2));
    assert!(r.created_at.is_some());
}

#[test]
fn chatgpt_tool_call_and_image() {
    let conv = json!({
        "id": "c2", "current_node": "t1",
        "mapping": {
            "t1": { "id": "t1", "parent": null, "children": [],
                "message": { "author": { "role": "assistant" }, "recipient": "python",
                    "content": { "content_type": "code", "text": "print(1)" } } },
        }
    });
    let m = &read(Kind::ChatGpt, &conv).1.messages[0];
    assert!(matches!(&m.content[0], Block::ToolUse { name, .. } if name == "python"));

    let conv2 = json!({ "id": "c3", "current_node": "i1", "mapping": {
        "i1": { "id": "i1", "parent": null, "children": [],
            "message": { "author": { "role": "assistant" },
                "content": { "content_type": "multimodal_text", "parts": [ {"content_type":"image_asset_pointer","asset_pointer":"file-service://abc"}, "see image" ] } } }
    }});
    let m2 = &read(Kind::ChatGpt, &conv2).1.messages[0];
    assert!(m2
        .content
        .iter()
        .any(|b| matches!(b, Block::Image { data_ref: Some(p), .. } if p == "file-service://abc")));
    assert!(m2
        .content
        .iter()
        .any(|b| matches!(b, Block::Text { text } if text.to_string().contains("see image"))));
}

#[test]
fn chatgpt_tool_call_links_to_result() {
    // assistant(recipient=browser, code) → tool(author.name=browser, result), linked by name+order.
    let conv = json!({ "id": "c4", "current_node": "r1", "mapping": {
        "call": { "id": "call", "parent": null, "children": ["r1"],
            "message": { "author": { "role": "assistant" }, "recipient": "browser",
                "content": { "content_type": "code", "text": "search(\"x\")" } } },
        "r1": { "id": "r1", "parent": "call", "children": [],
            "message": { "author": { "role": "tool", "name": "browser" }, "recipient": "all",
                "content": { "content_type": "tether_quote", "text": "the answer is 42" } } }
    }});
    let msgs = read(Kind::ChatGpt, &conv).1.messages;
    assert_eq!(msgs.len(), 2);
    // the call
    let Block::ToolUse { id, name, input, .. } = &msgs[0].content[0] else {
        panic!("expected ToolUse")
    };
    assert_eq!(id, "call");
    assert_eq!(name, "browser");
    assert!(serde_json::to_string(input).unwrap().contains("search")); // raw content kept as input
                                                                       // the result, wired back to the call id by tool name
    let Block::ToolResult {
        tool_use_id,
        content,
        tool_name,
        ..
    } = &msgs[1].content[0]
    else {
        panic!("expected ToolResult")
    };
    assert_eq!(tool_use_id, "call", "result linked to the matching call");
    assert_eq!(tool_name.as_deref(), Some("browser"));
    assert!(content.to_string().contains("the answer is 42"));
}

#[test]
fn kind_and_origin_chatgpt_and_claude() {
    // ChatGPT: user prompt, assistant reply, a recipient!=all tool call (still a reply), tool result.
    let conv = json!({ "id": "k", "current_node": "res", "mapping": {
        "u": { "id": "u", "parent": null, "children": ["call"],
            "message": { "author": { "role": "user" }, "content": { "content_type": "text", "parts": ["hi"] }, "recipient": "all" } },
        "call": { "id": "call", "parent": "u", "children": ["res"],
            "message": { "author": { "role": "assistant" }, "recipient": "python", "content": { "content_type": "code", "text": "print(1)" } } },
        "res": { "id": "res", "parent": "call", "children": [],
            "message": { "author": { "role": "tool", "name": "python" }, "recipient": "all", "content": { "content_type": "execution_output", "text": "1" } } }
    }});
    let msgs = read(Kind::ChatGpt, &conv).1.messages;
    assert_eq!((msgs[0].kind, msgs[0].origin), (MessageKind::Prompt, Origin::Human));
    assert_eq!((msgs[1].kind, msgs[1].origin), (MessageKind::Reply, Origin::Model));
    assert_eq!(
        (msgs[2].kind, msgs[2].origin),
        (MessageKind::ToolResult, Origin::Harness)
    );
    for m in &msgs {
        for k in m.extra.keys() {
            assert!(k == "chatgpt-export" || k == "_record", "unexpected extra key {k}");
        }
    }

    // Claude.ai export: human prompt, assistant reply.
    let cc = json!({ "uuid": "c", "chat_messages": [
        { "uuid": "m1", "sender": "human", "content": [{ "type": "text", "text": "q" }] },
        { "uuid": "m2", "sender": "assistant", "content": [{ "type": "text", "text": "a" }] }
    ]});
    let cm = read(Kind::Claude, &cc).1.messages;
    assert_eq!((cm[0].kind, cm[0].origin), (MessageKind::Prompt, Origin::Human));
    assert_eq!((cm[1].kind, cm[1].origin), (MessageKind::Reply, Origin::Model));
}

#[test]
fn claude_export_blocks() {
    let conv = json!({
        "uuid": "u1", "name": "Refactor chat", "created_at": "2026-01-01T00:00:00Z",
        "chat_messages": [
            { "uuid": "m1", "sender": "human", "parent_message_uuid": null, "text": "fix it",
              "content": [{ "type": "text", "text": "fix it" }] },
            { "uuid": "m2", "sender": "assistant", "parent_message_uuid": "m1", "text": "",
              "content": [
                  { "type": "thinking", "thinking": "let me think" },
                  { "type": "text", "text": "here's the fix" },
                  { "type": "tool_use", "id": "tu1", "name": "str_replace", "input": {"path":"a.rs"} }
              ] }
        ]
    });
    let (r, session) = read(Kind::Claude, &conv);
    let msgs = session.messages;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    let a = &msgs[1];
    assert!(matches!(&a.content[0], Block::Thinking { .. }));
    assert!(matches!(&a.content[1], Block::Text { .. }));
    assert!(matches!(&a.content[2], Block::ToolUse { name, .. } if name == "str_replace"));

    assert_eq!(r.id, "u1");
    assert_eq!(r.harness, Harness::ClaudeExport);
    assert_eq!(r.message_count, Some(2));
}

#[test]
fn read_finds_conversation_in_file_and_sniffs_kind() {
    let dir = std::env::temp_dir().join(format!("cv-export-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let arr = json!([
        { "uuid": "a", "name": "first", "chat_messages": [{ "uuid":"m","sender":"human","content":[{"type":"text","text":"hi"}] }] },
        { "uuid": "b", "name": "second", "chat_messages": [{ "uuid":"m2","sender":"assistant","content":[{"type":"text","text":"yo"}] }] }
    ]);
    std::fs::write(&path, arr.to_string()).unwrap();

    // Claude convos have chat_messages, not a ChatGPT mapping: discovery sniffs the kind.
    assert!(discover_kind_in(Kind::ChatGpt, vec![path.clone()]).is_empty());
    assert_eq!(discover_kind_in(Kind::Claude, vec![path.clone()]).len(), 2);

    let r = SessionRef {
        title: Some("second".into()),
        message_count: Some(1),
        ..SessionRef::new("b", Harness::ClaudeExport, path.clone())
    };
    let mut sink = CollectSink::default();
    let mut s = native::stream(Kind::Claude, &r, &Demand::RICH, &mut sink).unwrap();
    s.messages = sink.messages;
    assert_eq!(s.id, "b");
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, Role::Assistant);
    assert!(s.messages[0].text().unwrap().contains("yo"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Real exports in ~/Downloads (or $CV_EXPORTS) — ignored by default (machine-specific).
#[test]
#[ignore]
fn real_downloads_exports_parse() {
    // discovery is opt-in; point it at Downloads for this machine-specific probe.
    let env = crate::env::Env::from_process().enter();
    env.set("CV_EXPORTS", dirs::home_dir().unwrap().join("Downloads"));
    for kind in [Kind::ChatGpt, Kind::Claude] {
        let refs = discover_kind_under(kind, export_dirs());
        eprintln!("{:?}: {} conversation(s)", kind, refs.len());
        if let Some(r) = refs.iter().find(|r| r.message_count.is_some_and(|n| n > 0)) {
            let mut sink = CollectSink::default();
            let mut s = native::stream(kind, r, &Demand::RICH, &mut sink).unwrap();
            s.messages = sink.messages;
            let tools = s
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, Block::ToolUse { .. } | Block::ToolResult { .. }))
                .count();
            eprintln!(
                "  parsed {} -> {} msgs ({} tool blocks), model {:?}",
                &r.id[..8.min(r.id.len())],
                s.messages.len(),
                tools,
                s.model
            );
            assert!(!s.messages.is_empty());
        }
    }
}

// Full-session composition, not a count-only or selected-prefix heap assertion. The routine
// fixture crosses INLINE_MAX (4 KiB) and positioned windows (64 KiB) without making every
// workspace run pay for repeated 32 MiB serialization and hashing in debug builds.
#[test]
fn account_archive_full_output_matches_frozen() {
    account_archive_full_output_oracle(2 << 20);
}

#[test]
#[ignore = "32 MiB full-session composition control; run explicitly after relevant reader changes"]
fn giant_account_archive_full_output_matches_frozen() {
    account_archive_full_output_oracle(32 << 20);
}

fn account_archive_full_output_oracle(payload_bytes: usize) {
    use std::io::Write;
    let mut frozen = Frozen::new(if payload_bytes > 2 << 20 {
        "export-archive-giant"
    } else {
        "export-archive"
    });

    #[derive(Default)]
    struct HashOutput {
        digest: crate::digest::Sha256,
        bytes: usize,
    }
    impl Write for HashOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.digest.update(bytes);
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn source_hash(path: &Path) -> (usize, String) {
        let mut hash = HashOutput::default();
        std::io::copy(&mut std::fs::File::open(path).unwrap(), &mut hash).unwrap();
        (hash.bytes, hash.digest.finish_hex())
    }
    fn output(session: &Session) -> (usize, String) {
        let mut hash = HashOutput::default();
        serde_json::to_writer(&mut hash, session).unwrap();
        (hash.bytes, hash.digest.finish_hex())
    }

    let dir = std::env::temp_dir().join(format!("cv-export-full-output-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    // Mixed Unicode and JSON escapes force real decoding; the final turn proves EOF coverage.
    let pattern = "ordinary é😀\n\"quoted\"\t\\end ";
    let body = pattern.repeat(payload_bytes / pattern.len());
    for kind in [Kind::Claude, Kind::ChatGpt] {
        let conv = match kind {
            Kind::Claude => json!({"uuid":"selected", "name":"complete archive", "chat_messages":[
                {"uuid":"u","sender":"human","text":"prompt"},
                {"uuid":"a","sender":"assistant","content":[{"type":"text","text":body}]},
                {"uuid":"final","sender":"human","text":"final EOF marker"}
            ]}),
            Kind::ChatGpt => json!({"conversation_id":"selected","title":"complete archive",
            "current_node":"final","mapping":{
                "u":{"parent":null,"message":{"author":{"role":"user"},"content":{"parts":["prompt"]}}},
                "a":{"parent":"u","message":{"author":{"role":"assistant"},"content":{"parts":[body]}}},
                "final":{"parent":"a","message":{"author":{"role":"user"},"content":{"parts":["final EOF marker"]}}},
                "dead":{"parent":"u","message":{"author":{"role":"assistant"},"content":{"parts":["excluded branch"]}}}
            }}),
        };
        let unrelated = match kind {
            Kind::Claude => json!({"uuid":"unrelated","chat_messages":[]}),
            Kind::ChatGpt => json!({"conversation_id":"unrelated","mapping":{}}),
        };
        serde_json::to_writer(std::fs::File::create(&path).unwrap(), &json!([unrelated, conv])).unwrap();
        let before = source_hash(&path);
        let refs = discover_kind_in(kind, vec![path.clone()]);
        assert_eq!(refs.len(), 2);
        let expected_ref = refs.into_iter().find(|r| r.id == "selected").unwrap();
        assert_eq!(expected_ref.message_count, Some(3));
        for opts in [Demand::LEAN.with_spans(), Demand::LEAN, Demand::RICH, Demand::COMPLETE] {
            let mut sink = CollectSink::default();
            let mut actual = native::stream(kind, &expected_ref, &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(
                actual.messages[1].content.iter().any(|block| matches!(
                    block,
                    Block::Text {
                        text: crate::lazy::Text::Span(_) | crate::lazy::Text::Source(_)
                    }
                )),
                opts.spans,
                "fixture must exercise deferred text when requested"
            );
            actual.materialize().unwrap();
            assert_eq!(actual.messages.len(), 3);
            assert_eq!(actual.messages.last().unwrap().text().unwrap(), "final EOF marker");
            assert!(output(&actual).0 > payload_bytes);
            frozen.insert(format!("{kind:?} {}", mode(&opts)), digest(&actual));
        }
        assert_eq!(source_hash(&path), before);
    }
    frozen.check().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

fn mode(opts: &Demand) -> &'static str {
    match (opts.spans, opts.tier == Tier::Complete, opts.tier >= Tier::Facts) {
        (true, _, _) => "lazy",
        (false, true, _) => "complete",
        (false, false, true) => "full",
        (false, false, false) => "bulk",
    }
}

#[test]
fn native_archive_matches_frozen_across_modes_and_invalidation() {
    use crate::stream::{Flow, MessageSink};
    let mut frozen = Frozen::new("export-native");
    struct Stop;
    impl MessageSink for Stop {
        fn message(&mut self, _: Message) -> Flow {
            Flow::Stop
        }
    }
    let dir = std::env::temp_dir().join(format!("cv-export-native-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let body = "a\n\"é😀".repeat(8192);
    let conversations = [
        (
            Kind::Claude,
            json!({"uuid":"claude", "name":"", "chat_messages":[
                {"uuid":"u","sender":"human","content":[{"type":"text","text":body}]},
                {"uuid":"a","parent_message_uuid":"u","sender":"assistant","content":[
                    {"type":"thinking","thinking":"","text":body},
                    {"type":"tool_use","id":"t","name":"read","input":{"a":1,"payload":body}},
                    {"type":"tool_result","tool_use_id":"t","text":body},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":body}},
                    {"type":"document","title":"a","source":{"type":"file","file_id":"file1"}}
                ]}, {"sender":"human","text":"fallback"}, {"sender":"other","text":"system"}
            ]}),
        ),
        (
            Kind::ChatGpt,
            json!({"conversation_id":"chatgpt", "mapping":{
                "0":{"id":"u","parent":null,"message":{"author":{"role":"user"},"content":{"parts":[body]}}},
                "1":{"id":"t","parent":"0","message":{"author":{"role":"assistant"},"recipient":"python","content":{"text":body}}},
                "2":{"id":"r","parent":"1","message":{"author":{"role":"tool","name":"python"},"content":{"text":body}}},
                "3":{"id":"a","parent":"2","message":{"author":{"role":"assistant"},"content":{"parts":["","two",{"asset_pointer":"file-service://a"}]}}},
                "4":{"id":"s","parent":"3","message":{"author":{"role":"system"},"content":{"text":"context"}}}
            }}),
        ),
    ];
    for (kind, conv) in conversations {
        let prefix = match kind {
            Kind::Claude => json!({"uuid":"unrelated","chat_messages":[],"unknown":body}),
            Kind::ChatGpt => json!({"id":"unrelated","mapping":{},"unknown":body}),
        };
        std::fs::write(&path, serde_json::to_vec(&json!([prefix, conv])).unwrap()).unwrap();
        let id = conv.get("uuid").or_else(|| conv.get("conversation_id")).unwrap();
        let expected_ref = discover_kind_in(kind, vec![path.clone()])
            .into_iter()
            .find(|r| id == r.id.as_str())
            .unwrap();
        frozen.insert(format!("{kind:?} ref"), reference(&expected_ref));
        let mut title = None;
        for opts in [Demand::LEAN.with_spans(), Demand::RICH, Demand::COMPLETE] {
            let mut sink = CollectSink::default();
            let mut actual = native::stream(kind, &expected_ref, &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(
                actual.messages.iter().flat_map(|m| &m.content).any(|block| {
                    matches!(
                        block,
                        Block::ToolUse {
                            input: crate::json::Json::Source(_),
                            ..
                        }
                    )
                }),
                opts.spans
            );
            actual.materialize().unwrap();
            title = actual.title.clone();
            frozen.insert(format!("{kind:?} {}", mode(&opts)), digest(&actual));
        }
        let mut published = None;
        let mut mutate = |message: Message| {
            let input = message.content.iter().find(|block| {
                matches!(
                    block,
                    Block::ToolUse {
                        input: crate::json::Json::Source(_),
                        ..
                    }
                )
            });
            if let Some(Block::ToolUse { input, .. }) = input {
                published = Some(input.clone());
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
                crate::stream::Flow::Stop
            } else {
                crate::stream::Flow::Continue
            }
        };
        assert!(
            native::stream(kind, &expected_ref, &Demand::LEAN.with_spans(), &mut mutate).is_err(),
            "source mutation after publication must reject completion"
        );
        assert!(
            published.unwrap().resolve().is_err(),
            "published descriptor must retain its captured identity"
        );
        assert_eq!(
            native::stream(kind, &expected_ref, &Demand::LEAN.with_spans(), &mut Stop)
                .unwrap()
                .title,
            title
        );
        // A same-size replacement with a new inode must invalidate descriptors and metadata.
        let changed = serde_json::to_string(&json!([conv]))
            .unwrap()
            .replace(&expected_ref.id, "changed");
        let replacement = dir.join("replacement.json");
        std::fs::write(&replacement, changed).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let fresh = discover_kind_in(kind, vec![path.clone()]);
        assert_eq!(fresh[0].id, "changed");
        assert!(native::stream(kind, &expected_ref, &Demand::LEAN.with_spans(), &mut Stop).is_err());
    }
    frozen.check().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_archive_keeps_raw_value_acceptance_and_duplicate_precedence() {
    let mut frozen = Frozen::new("export-acceptance");
    let dir = std::env::temp_dir().join(format!("cv-export-validation-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let valid = r#"{"uuid":"same","chat_messages":[{"sender":"human","text":"ok"}]}"#;
    for invalid in ["1e400", "\"\\ud800\""] {
        let bad = format!("{{\"uuid\":\"same\",\"chat_messages\":[],\"unknown\":{invalid},\"unknown\":0}}");
        std::fs::write(&path, format!("[{bad},{valid}]")).unwrap();
        // Invalid first conversation prevents sniffed discovery; explicit reads skip it.
        assert!(discover_kind_in(Kind::Claude, vec![path.clone()]).is_empty());
        let r = SessionRef::new("same", Harness::ClaudeExport, path.clone());
        let mut sink = CollectSink::default();
        let mut actual = native::stream(Kind::Claude, &r, &Demand::LEAN.with_spans(), &mut sink).unwrap();
        actual.messages = sink.messages;
        actual.materialize().unwrap();
        assert_eq!(actual.messages.len(), 1);
        frozen.insert(invalid, digest(&actual));
    }
    frozen.check().unwrap();
    let duplicate = r#"[{"conversation_id":false,"uuid":"hidden","chat_messages":[]},{"uuid":"old","\u0075uid":"new","chat_messages":[]}]"#;
    std::fs::write(&path, duplicate).unwrap();
    let refs = discover_kind_in(Kind::Claude, vec![path.clone()]);
    assert_eq!(refs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["new"]);
    let roles = json!({"uuid":"roles","chat_messages":[
        {"sender":"human"},{"sender":"user"},{"sender":"assistant"},{"sender":"tool"}
    ]});
    std::fs::write(&path, serde_json::to_vec(&json!([roles])).unwrap()).unwrap();
    let refs = discover_kind_in(Kind::Claude, vec![path.clone()]);
    assert_eq!(refs[0].message_count, Some(2));
    std::fs::write(&path, "[]").unwrap();
    assert!(discover_kind_in(Kind::Claude, vec![path.clone()]).is_empty());
    // An archive that does not decode is reported, not silently empty.
    std::fs::write(&path, "").unwrap();
    let found = discover_kind_in(Kind::Claude, vec![path.clone()]);
    assert!(found.is_empty());
    assert_eq!(found.skipped.iter().map(|s| &s.path).collect::<Vec<_>>(), [&path]);
    let r = SessionRef::new("missing", Harness::ClaudeExport, path.clone());
    assert!(native::stream(
        Kind::Claude,
        &r,
        &Demand::LEAN.with_spans(),
        &mut CollectSink::default()
    )
    .unwrap_err()
    .to_string()
    .contains("conversation missing not found"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cached_export_discovery_preserves_the_callers_path_spelling() {
    let relative_dir = PathBuf::from(format!("cv-export-path-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&relative_dir).unwrap();
    let relative = relative_dir.join("conversations.json");
    let absolute = std::env::current_dir().unwrap().join(&relative);
    std::fs::write(&relative, r#"[{"uuid":"same","chat_messages":[]}]"#).unwrap();
    let first = discover_kind_in(Kind::Claude, vec![relative.clone()]);
    let second = discover_kind_in(Kind::Claude, vec![absolute.clone()]);
    assert_eq!(first[0].path, relative);
    assert_eq!(second[0].path, absolute);
    std::fs::remove_dir_all(relative_dir).unwrap();
}

#[test]
fn native_chatgpt_fallback_keeps_insertion_order_and_final_duplicate_value() {
    let mut frozen = Frozen::new("export-order");
    let dir = std::env::temp_dir().join(format!("cv-export-order-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let archive = r#"[{"id":"ordered","mapping":{
        "z":{"id":"discarded","message":{"author":{"role":"user"},"content":{"parts":["old"]}}},
        "a":{"id":"second","message":{"author":{"role":"assistant"},"recipient":"python","content":{"z":1,"a":2,"text":"run"}}},
        "\u007a":{"id":"first","message":{"author":{"role":"user"},"content":{"parts":["replacement"]}}}
    }}]"#;
    std::fs::write(&path, archive).unwrap();
    let r = SessionRef::new("ordered", Harness::ChatGptExport, path.clone());
    for opts in [Demand::LEAN.with_spans(), Demand::RICH, Demand::COMPLETE] {
        let mut sink = CollectSink::default();
        let mut actual = native::stream(Kind::ChatGpt, &r, &opts, &mut sink).unwrap();
        actual.messages = sink.messages;
        actual.materialize().unwrap();
        assert_eq!(
            actual.messages.iter().map(|m| m.id.as_deref()).collect::<Vec<_>>(),
            [Some("first"), Some("second")]
        );
        frozen.insert(mode(&opts), digest(&actual));
    }
    frozen.check().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn multipart_chatgpt_read_keeps_lazy_payloads_and_breaks_parent_cycles() -> Result<()> {
    let world = crate::test_world("account-multipart-cycle");
    let path = world.base.join("conversations.json");
    let body = "λ\n\"escaped\"".repeat(20000);
    let conversation = json!({"conversation_id":"multi","title":"explicit","current_node":"reply","mapping":{
        "user":{"id":"user","parent":null,"message":{"author":{"role":"user"},"content":{"parts":["",body,body,""]}}},
        "reply":{"id":"reply","parent":"user","message":{"author":{"role":"assistant"},"content":{"parts":[body,"",body]}}}
    }});
    std::fs::write(&path, serde_json::to_vec(&json!([conversation]))?)?;
    let reference = discover_kind_in(Kind::ChatGpt, vec![path.clone()]).remove(0);
    let mut sink = crate::stream::CollectSink::default();
    let mut actual = native::stream(Kind::ChatGpt, &reference, &Demand::LEAN.with_spans(), &mut sink)?;
    actual.messages = sink.messages;
    assert!(
        actual.messages.iter().all(|m| matches!(
            &m.content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        )),
        "joined ordinary bodies remain checked backing"
    );
    actual.materialize()?;
    let mut frozen = Frozen::new("export-multipart");
    frozen.insert("lazy", digest(&actual));
    frozen.check()?;
    let cycle = json!({"conversation_id":"cycle","current_node":"reply","mapping":{
        "reply":{"id":"reply","parent":"reply","message":{"author":{"role":"assistant"},"content":{"parts":["one reply"]}}}
    }});
    std::fs::write(&path, serde_json::to_vec(&json!([cycle]))?)?;
    let refs = discover_kind_in(Kind::ChatGpt, vec![path.clone()]);
    assert_eq!(refs[0].message_count, Some(1));
    let mut sink = crate::stream::CollectSink::default();
    native::stream(Kind::ChatGpt, &refs[0], &Demand::LEAN.with_spans(), &mut sink)?;
    assert_eq!(
        sink.messages.len(),
        1,
        "cycles must not synthesize100,000 duplicate replies"
    );
    Ok(())
}

#[test]
fn wide_chatgpt_parts_preserve_filtered_join_and_source_guard() -> Result<()> {
    let world = crate::test_world("account-wide-parts");
    let path = world.base.join("parts.json");
    let mut parts = Vec::new();
    for i in 0..8192 {
        parts.push(match i % 3 {
            0 => json!(""),
            1 => json!({"ignored":"not a string part"}),
            _ => json!(format!("part-{i} λ\n")),
        });
    }
    let expected = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
    std::fs::write(&path, serde_json::to_vec(&json!({"parts":parts}))?)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let resolver = crate::lazy::Resolver::new(None);
    let text = source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
        let fields = crate::harness::store::document::Fields::parse(view, &["parts"])?;
        native::chatgpt_text(fields.as_ref(), true, Some(&source), &resolver)
    })?;
    assert!(matches!(text, crate::Text::Source(_)));
    assert_eq!(text.try_resolve(&resolver)?.as_ref(), expected);
    std::fs::write(&path, b"changed")?;
    assert!(text.try_resolve(&resolver).is_err());
    Ok(())
}
