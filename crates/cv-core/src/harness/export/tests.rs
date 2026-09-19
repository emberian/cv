use super::*;
use serde_json::json;

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
    let msgs = chatgpt_messages(&conv);
    assert_eq!(msgs.len(), 2, "only the root->current_node branch");
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    assert!(msgs[0].text().unwrap().contains("how to brew"));
    assert!(msgs[1].text().unwrap().contains("soak the beans"));
    assert!(!msgs
        .iter()
        .any(|m| m.text().unwrap_or_default().contains("DEAD BRANCH")));

    let r = conv_ref(Kind::ChatGpt, std::path::Path::new("/x/conversations.json"), &conv).unwrap();
    assert_eq!(r.id, "c1");
    assert_eq!(r.harness, Harness::ChatGptExport);
    assert_eq!(r.title.as_deref(), Some("Soy Sauce"));
    assert_eq!(r.message_count, 2);
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
    let m = &chatgpt_messages(&conv)[0];
    assert!(matches!(&m.content[0], Block::ToolUse { name, .. } if name == "python"));

    let conv2 = json!({ "id": "c3", "current_node": "i1", "mapping": {
        "i1": { "id": "i1", "parent": null, "children": [],
            "message": { "author": { "role": "assistant" },
                "content": { "content_type": "multimodal_text", "parts": [ {"content_type":"image_asset_pointer","asset_pointer":"file-service://abc"}, "see image" ] } } }
    }});
    let m2 = &chatgpt_messages(&conv2)[0];
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
    let msgs = chatgpt_messages(&conv);
    assert_eq!(msgs.len(), 2);
    // the call
    let Block::ToolUse { id, name, input, .. } = &msgs[0].content[0] else {
        panic!("expected ToolUse")
    };
    assert_eq!(id, "call");
    assert_eq!(name, "browser");
    assert!(input.to_string().contains("search")); // raw content kept as input
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
    let msgs = claude_messages(&conv);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    let a = &msgs[1];
    assert!(matches!(&a.content[0], Block::Thinking { .. }));
    assert!(matches!(&a.content[1], Block::Text { .. }));
    assert!(matches!(&a.content[2], Block::ToolUse { name, .. } if name == "str_replace"));

    let r = conv_ref(Kind::Claude, std::path::Path::new("/x/conversations.json"), &conv).unwrap();
    assert_eq!(r.id, "u1");
    assert_eq!(r.harness, Harness::ClaudeExport);
    assert_eq!(r.message_count, 2);
}

#[test]
fn parse_ref_finds_conversation_in_file_and_sniffs_kind() {
    let dir = std::env::temp_dir().join(format!("cv-export-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let arr = json!([
        { "uuid": "a", "name": "first", "chat_messages": [{ "uuid":"m","sender":"human","content":[{"type":"text","text":"hi"}] }] },
        { "uuid": "b", "name": "second", "chat_messages": [{ "uuid":"m2","sender":"assistant","content":[{"type":"text","text":"yo"}] }] }
    ]);
    std::fs::write(&path, arr.to_string()).unwrap();

    // sniff signal: Claude convos have chat_messages, not a ChatGPT mapping
    let (_t, arr) = read_array(&path).unwrap();
    let first: serde_json::Value = serde_json::from_str(arr[0].get()).unwrap();
    assert!(first.get("chat_messages").is_some() && first.get("mapping").is_none());

    let r = SessionRef {
        id: "b".into(),
        harness: Harness::ClaudeExport,
        path: path.clone(),
        cwd: None,
        title: Some("second".into()),
        created_at: None,
        updated_at: None,
        message_count: 1,
    };
    let s = parse_ref(Kind::Claude, &r).unwrap();
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
    std::env::set_var("CV_EXPORTS", dirs::home_dir().unwrap().join("Downloads"));
    for kind in [Kind::ChatGpt, Kind::Claude] {
        let refs = discover_kind(kind);
        eprintln!("{:?}: {} conversation(s)", kind, refs.len());
        if let Some(r) = refs.iter().find(|r| r.message_count > 0) {
            let s = parse_ref(kind, r).unwrap();
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
    std::env::remove_var("CV_EXPORTS");
}
