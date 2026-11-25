use super::*;
use crate::emit::frozen::Frozen;
use crate::emit::{EmitOptions, ThinkingMode};

/// The files one Cline/Roo emit publishes into the task directory.
const FILES: [&str; 3] = [
    "api_conversation_history.json",
    "task_metadata.json",
    "ui_messages.json",
];

fn fixture() -> Result<Session> {
    let mut session = Session::new("source", Harness::Claude);
    let ts = "2026-10-12T12:00:00Z".parse()?;
    session.created_at = Some(ts);
    session.updated_at = Some(ts + chrono::Duration::seconds(2));
    session.cwd = Some("/synthetic/cline λ".into());
    session.title = Some("title \"λ\"".into());
    session.model = Some("session-model".into());
    let mut user = Message::new(Role::User);
    user.content = vec![
        Block::from_text("prompt\n雪".into()),
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:img".into()),
            extensions: Default::default(),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("report λ.txt".into()),
            source: Some("https://report".into()),
            extensions: Default::default(),
        },
    ];
    let mut assistant = Message::new(Role::Assistant);
    assistant.content = vec![
        Block::Thinking {
            text: "reasoning λ\n".into(),
            signature: Some("signature".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        },
        Block::Thinking {
            text: " ".into(),
            signature: Some("signature-only".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        },
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("opaque".into()),
            redacted: true,
            extensions: Default::default(),
        },
        Block::from_text("answer \"\\\t".into()),
        Block::ToolUse {
            id: "linked".into(),
            name: "tool".into(),
            namespace: None,
            input: json!({"β":[1,true,null]}).into(),
            extensions: Default::default(),
        },
        Block::File {
            mime: None,
            path: None,
            source: Some("https://assistant/file".into()),
            extensions: Default::default(),
        },
    ];
    let mut tools = Message::new(Role::Tool);
    tools.content = vec![
        Block::from_text("not emitted in a Tool turn".into()),
        Block::ToolResult {
            tool_use_id: "linked".into(),
            content: "tool result λ".into(),
            is_error: true,
            tool_name: Some("tool".into()),
            status: None,
            details: None,
            extensions: Default::default(),
        },
    ];
    let mut empty = Message::new(Role::Assistant);
    empty.content.push(Block::Thinking {
        text: "".into(),
        signature: None,
        encrypted: None,
        redacted: false,
        extensions: Default::default(),
    });
    session.messages = vec![
        user,
        assistant,
        tools,
        Message::new(Role::System),
        empty,
        Message::new(Role::User),
    ];
    Ok(session)
}

#[test]
fn checked_cline_roo_match_frozen_pretty_all_modes() -> Result<()> {
    let world = crate::test_world("cline-checked-wire");
    let session = fixture()?;
    let mut frozen = Frozen::new("cline-roo-fixture");
    for harness in [Harness::Cline, Harness::Roo] {
        for mode in ThinkingMode::ALL {
            let options = EmitOptions {
                new_id: Some("fixed".into()),
                thinking: mode,
                ..Default::default()
            };
            let after = crate::emit::emit(
                &session,
                harness,
                &world.base.join(format!("new-{harness}-{mode}")),
                &options,
            )?;
            for name in FILES {
                let raw = fs::read(after.path.join(name))?;
                frozen.insert(
                    format!("{harness}-{mode}-{name}"),
                    json!([raw.len(), crate::digest::sha256_hex(&raw)]),
                );
                let parsed: Value = serde_json::from_slice(&raw)?;
                assert_eq!(raw, serde_json::to_vec_pretty(&parsed)?);
                assert!(!raw.ends_with(b"\n"));
                // Structural facts beside the digest, so a digest change can be read, not just
                // unfrozen: the six fixture rows and their roles, the UI log a JSON array.
                if name == "api_conversation_history.json" {
                    let rows = parsed.as_array().unwrap();
                    assert_eq!(
                        rows.iter().map(|r| r["role"].as_str().unwrap()).collect::<Vec<_>>(),
                        ["user", "assistant", "user", "system", "assistant", "user"]
                    );
                    assert!(rows[0]["content"][0]["text"].as_str().unwrap().starts_with("<task>"));
                } else if name == "task_metadata.json" {
                    assert!(parsed.is_object(), "{name}");
                } else {
                    assert!(parsed.is_array(), "{name}");
                }
            }
            assert!(after
                .resume_hint
                .as_ref()
                .unwrap()
                .contains(if harness == Harness::Roo { "Roo Code" } else { "Cline" }));
        }
    }
    frozen.check()
}

#[test]
fn checked_cline_guarded_failure_preserves_all_destination_files() -> Result<()> {
    let world = crate::test_world("cline-checked-guard");
    for text_leaf in [true, false] {
        let path = world.base.join(format!("leaf-{text_leaf}.json"));
        let raw = serde_json::to_vec(&if text_leaf {
            json!("guarded λ\n".repeat(10000))
        } else {
            json!({"payload":"guarded λ\n".repeat(10000)})
        })?;
        fs::write(&path, &raw)?;
        let json = crate::Json::source(&path, 0, raw.len() as u64)?;
        let mut session = Session::new("source", Harness::Claude);
        let mut message = Message::new(Role::Assistant);
        message.content.push(if text_leaf {
            Block::from_text(json.with_view(|v| v.source_text())?.unwrap())
        } else {
            Block::ToolUse {
                id: "id".into(),
                name: "name".into(),
                namespace: None,
                input: json,
                extensions: Default::default(),
            }
        });
        session.messages.push(message);
        let out = world.base.join(format!("out-{text_leaf}"));
        let task = out.join("tasks/fixed");
        fs::create_dir_all(&task)?;
        for name in FILES {
            fs::write(task.join(name), "original")?;
        }
        fs::write(&path, "changed source")?;
        assert!(crate::emit::emit(
            &session,
            Harness::Cline,
            &out,
            &EmitOptions {
                new_id: Some("fixed".into()),
                ..Default::default()
            }
        )
        .is_err());
        for name in FILES {
            assert_eq!(fs::read_to_string(task.join(name))?, "original");
        }
    }
    Ok(())
}
