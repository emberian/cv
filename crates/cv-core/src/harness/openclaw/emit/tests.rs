use super::*;
use crate::{harness::wire::anthropic, Json, Message, Origin};
use std::collections::HashMap;

/// Only recorded counts are written; OpenClaw derives the total it is not given.
#[test]
fn usage_writes_recorded_counts_and_no_total() {
    let usage = crate::Usage {
        input_tokens: Some(u64::MAX),
        output_tokens: Some(1),
        ..Default::default()
    };
    let value = openclaw_usage(Some(&usage)).unwrap();
    assert_eq!(value, json!({"input": u64::MAX, "output": 1}));
    let cost_only = crate::Usage {
        cost_usd: Some(0.5),
        ..Default::default()
    };
    assert_eq!(
        openclaw_usage(Some(&cost_only)).unwrap(),
        json!({"cost": {"total": 0.5}})
    );
}

fn normalized(path: &Path) -> Result<Vec<u8>> {
    let rows: Vec<Value> = fs::read_to_string(path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let mut ids = HashMap::new();
    for value in &rows {
        if value["type"] == "session" {
            continue;
        }
        let Some(id) = value["id"].as_str() else {
            continue;
        };
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!ids.contains_key(id));
        ids.insert(id.to_owned(), format!("{:08x}", ids.len() + 1));
    }
    let mut output = Vec::new();
    for mut value in rows {
        if value["type"] != "session" {
            for field in [
                "id",
                "parentId",
                "targetId",
                "appendParentId",
                "firstKeptEntryId",
                "fromId",
            ] {
                if let Some(replacement) = value.get(field).and_then(Value::as_str).and_then(|id| ids.get(id)) {
                    value[field] = json!(replacement);
                }
            }
        }
        serde_json::to_writer(&mut output, &value)?;
        output.push(b'\n');
    }
    Ok(output)
}

/// One emitted session, frozen: the normalized transcript's and the index's length and SHA-256
/// (the test's base directory normalized), the new id and the resume hint.
fn frozen_entry(base: &Path, result: &EmitResult) -> Result<Value> {
    let digest = |bytes: Vec<u8>| {
        let bytes = String::from_utf8(bytes)
            .unwrap()
            .replace(&*base.to_string_lossy(), "$BASE");
        json!([bytes.len(), crate::digest::sha256_hex(bytes.as_bytes())])
    };
    Ok(json!({
        "transcript": digest(normalized(&result.path)?),
        "index": digest(fs::read(result.path.parent().unwrap().join("sessions.json"))?),
        "new_id": result.new_id,
        "resume_hint": result.resume_hint,
    }))
}

fn source_text(path: &Path, body: &str) -> Result<crate::Text> {
    let bytes = serde_json::to_vec(body)?;
    fs::write(path, &bytes)?;
    Json::source(path, 0, bytes.len() as u64)?.with_view(|v| Ok(v.source_text()?.unwrap()))
}

fn fixture(base: &Path) -> Result<Session> {
    let mut session = Session::new("source", Harness::Claude);
    let ts = "2026-10-12T12:00:00Z".parse()?;
    session.created_at = Some(ts);
    session.updated_at = Some(ts + chrono::Duration::seconds(5));
    session.title = Some("title \"λ\"".into());
    session.cwd = Some("/synthetic/openclaw".into());
    session.model = Some("native-model".into());
    let input = json!({"z":"guarded λ\n".repeat(1000),"a":[1,true,null]});
    let bytes = serde_json::to_vec(&input)?;
    let path = base.join("input.json");
    fs::write(&path, &bytes)?;
    let input = Json::source(&path, 0, bytes.len() as u64)?;
    let mut user = Message::new(Role::User);
    user.content = vec![
        Block::Text {
            text: source_text(&base.join("prompt.json"), &"prompt λ\n".repeat(1000))?,
        },
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:image".into()),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("report λ.txt".into()),
            source: None,
        },
    ];
    let mut assistant = Message::new(Role::Assistant);
    assistant.usage = Some(crate::Usage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        cache_read_tokens: Some(2),
        cache_creation_tokens: Some(1),
        cost_usd: Some(0.1),
        ..Default::default()
    });
    assistant.content = vec![
        Block::Thinking {
            text: source_text(&base.join("thinking.json"), &"reasoning λ\n".repeat(1000))?,
            signature: Some("sig".into()),
            encrypted: None,
            redacted: false,
        },
        Block::Thinking {
            text: " ".into(),
            signature: Some("signature-only".into()),
            encrypted: None,
            redacted: false,
        },
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("opaque".into()),
            redacted: true,
        },
        Block::Text {
            text: "answer \"\\\t".into(),
        },
        Block::ToolUse {
            id: "deferred".into(),
            name: "inner".into(),
            namespace: Some(crate::affordance::DEFERRED.into()),
            input: input.clone(),
        },
        Block::ToolUse {
            id: "ordinary".into(),
            name: "ordinary".into(),
            namespace: None,
            input,
        },
        Block::Image {
            media_type: None,
            data_ref: None,
        },
        Block::ToolResult {
            tool_use_id: "ignored".into(),
            content: "assistant result is omitted".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        },
    ];
    let mut tool = Message::new(Role::Tool);
    tool.content = vec![Block::ToolResult {
        tool_use_id: "deferred".into(),
        content: source_text(&base.join("result.json"), &"result λ\n".repeat(1000))?,
        is_error: false,
        tool_name: Some("inner".into()),
        status: Some("completed".into()),
        details: None,
    }];
    let mut summary = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
    summary.content = vec![
        Block::Text { text: "".into() },
        Block::Text {
            text: "summary\nλ".into(),
        },
        Block::Thinking {
            text: "summary reasoning".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        },
    ];
    let mut branch = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
    branch.content.push(Block::Text {
        text: "unused branch body".into(),
    });
    let mut model = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
    model.model = Some("changed-model".into());
    let mut empty = Message::new(Role::Assistant);
    empty.content.push(Block::Thinking {
        text: "".into(),
        signature: None,
        encrypted: None,
        redacted: false,
    });
    session.messages = vec![
        user,
        assistant,
        tool,
        summary,
        branch,
        model,
        empty,
        Message::new(Role::System),
        Message::new(Role::Tool),
    ];
    Ok(session)
}

#[test]
fn checked_openclaw_matches_frozen_records_index_and_all_modes() -> Result<()> {
    let world = crate::test_world("openclaw-checked-wire");
    let session = fixture(&world.base)?;
    let mut frozen = crate::emit::frozen::Frozen::new("openclaw-fixture");
    for mode in ThinkingMode::ALL {
        let options = EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let root = world.base.join(format!("new-{mode}/agents"));
        fs::create_dir_all(root.join("main/sessions"))?;
        fs::write(
            root.join("main/sessions/sessions.json"),
            serde_json::to_vec(&json!({"untouched":{"label":"keep"},"fixed":{"old":"replace"}}))?,
        )?;
        let after = crate::emit::emit(&session, Harness::OpenClaw, &root, &options)?;
        frozen.insert(mode.to_string(), frozen_entry(&world.base, &after)?);
    }
    frozen.check()
}

#[test]
fn checked_openclaw_borrowed_wrapper_matches_replay_definition_and_none() -> Result<()> {
    let world = crate::test_world("openclaw-checked-deferred");
    let session = fixture(&world.base)?;
    let Block::ToolUse { input, .. } = &session.messages[1].content[4] else {
        unreachable!()
    };
    for namespace in [None, Some("other"), Some(crate::affordance::DEFERRED)] {
        let block = Block::ToolUse {
            id: "call".into(),
            name: "inner".into(),
            input: input.clone(),
            namespace: namespace.map(str::to_owned),
        };
        let rewrapped = crate::affordance::rewrap_deferred(crate::harness::openclaw::DEFERRED_WRAPPER, &block)?;
        assert_eq!(rewrapped.is_some(), namespace == Some(crate::affordance::DEFERRED));
        let expected = anthropic::assistant_blocks(&[rewrapped.unwrap_or(block.clone())], &anthropic::OPENCLAW)?;
        let mut bytes = Vec::new();
        stream::content(&mut bytes, &[block], ThinkingMode::Native, &session.resolver())?;
        assert_eq!(bytes, serde_json::to_vec(&expected)?);
    }
    Ok(())
}

#[test]
fn checked_openclaw_title_policy_matches_frozen_explicit_and_meaningful_prompt() -> Result<()> {
    let world = crate::test_world("openclaw-title-policy");
    let mut frozen = crate::emit::frozen::Frozen::new("openclaw-title-policy");
    for title in [None, Some(""), Some("   "), Some("explicit λ")] {
        // Frozen labels keep the raw title; directory names only get a filesystem-safe spelling
        // of it, so a whitespace-only title does not name a whitespace-led directory.
        let dir_title = |title: Option<&str>| title.unwrap_or("none").replace([' ', '\t', '\n', '\r'], "_");
        for meaningful in [false, true] {
            let mut session = Session::new("source", Harness::Claude);
            session.created_at = Some("2026-10-12T12:00:00Z".parse()?);
            session.updated_at = session.created_at;
            session.title = title.map(str::to_owned);
            let mut first = Message::new(Role::User);
            first.content = vec![
                Block::Text { text: "".into() },
                Block::Text { text: " \n".into() },
                Block::Text {
                    text: if meaningful {
                        "λ\n".repeat(50).into()
                    } else {
                        "".into()
                    },
                },
            ];
            let mut second = Message::new(Role::User);
            second.content.push(Block::Text {
                text: "later meaningful prompt".into(),
            });
            first.content.push(Block::Thinking {
                text: "user reasoning".into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
            session.messages = vec![first, second];
            for mode in ThinkingMode::ALL {
                let options = EmitOptions {
                    new_id: Some("fixed".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let suffix = format!("{}-{meaningful}-{mode}", title.unwrap_or("none"));
                let after = emit(
                    &session,
                    &world.base.join(format!("new-{}-{meaningful}-{mode}", dir_title(title))),
                    &options,
                )?;
                frozen.insert(suffix, frozen_entry(&world.base, &after)?["index"].clone());
            }
        }
    }
    frozen.check()
}

#[test]
fn checked_openclaw_source_failure_preserves_transcript_and_index_even_for_ignored_fields() -> Result<()> {
    let world = crate::test_world("openclaw-source-guard");
    for ignored in [false, true] {
        let mut session = fixture(&world.base)?;
        if ignored {
            session.messages[1].kind = MessageKind::Branch;
        }
        let out = world.base.join(format!("out-{ignored}"));
        let dir = out.join("agents/main/sessions");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("fixed.jsonl"), "old transcript")?;
        fs::write(dir.join("sessions.json"), r#"{"old":{"label":"index"}}"#)?;
        fs::write(world.base.join("input.json"), "changed source")?;
        assert!(emit(
            &session,
            &out,
            &EmitOptions {
                new_id: Some("fixed".into()),
                thinking: ThinkingMode::Drop,
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(fs::read_to_string(dir.join("fixed.jsonl"))?, "old transcript");
        assert_eq!(
            fs::read_to_string(dir.join("sessions.json"))?,
            r#"{"old":{"label":"index"}}"#
        );
    }
    Ok(())
}

/// An index cv cannot read as one is refused, never replaced by one naming only this session;
/// the transcript is not published either, and one it would replace keeps its bytes.
#[test]
fn an_unreadable_index_is_refused_and_nothing_is_published() -> Result<()> {
    let world = crate::test_world("openclaw-unreadable-index");
    let session = fixture(&world.base)?;
    for (case, index) in [
        ("broken", Some("{broken index")),
        ("array", Some("[]")),
        ("directory", None),
    ] {
        for force in [false, true] {
            let out = world.base.join(format!("out-{case}-{force}"));
            let dir = out.join("agents/main/sessions");
            fs::create_dir_all(&dir)?;
            match index {
                Some(index) => fs::write(dir.join("sessions.json"), index)?,
                None => fs::create_dir(dir.join("sessions.json"))?,
            }
            if force {
                fs::write(dir.join("fixed.jsonl"), "old transcript")?;
            }
            let options = EmitOptions {
                new_id: Some("fixed".into()),
                force,
                ..Default::default()
            };
            let error = format!("{:#}", emit(&session, &out, &options).unwrap_err());
            assert!(error.contains("refusing to replace it"), "{case}: {error}");
            if let Some(index) = index {
                assert_eq!(fs::read_to_string(dir.join("sessions.json"))?, index);
            }
            match force {
                true => assert_eq!(fs::read_to_string(dir.join("fixed.jsonl"))?, "old transcript"),
                false => assert!(!dir.join("fixed.jsonl").exists()),
            }
            assert_eq!(fs::read_dir(&dir)?.count(), 1 + force as usize, "{case}");
        }
    }
    Ok(())
}

#[test]
fn checked_openclaw_late_source_failure_cannot_publish_staged_records() -> Result<()> {
    use std::io::Write;
    let world = crate::test_world("openclaw-source-race");
    let session = fixture(&world.base)?;
    let source = world.base.join("input.json");
    let replacement = world.base.join("replacement.json");
    fs::copy(&source, &replacement)?;
    let destination = world.base.join("existing.jsonl");
    fs::write(&destination, "original transcript")?;
    struct Replace<'a> {
        writer: &'a mut dyn Write,
        source: &'a Path,
        replacement: &'a Path,
        done: bool,
    }
    impl Write for Replace<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.done && bytes.len() > 1000 {
                fs::rename(self.replacement, self.source)?;
                self.done = true;
            }
            self.writer.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut stage = JsonlStage::new();
    let error = stage
        .push_with(|writer| {
            stream::content(
                &mut Replace {
                    writer,
                    source: &source,
                    replacement: &replacement,
                    done: false,
                },
                &session.messages[1].content[4..5],
                ThinkingMode::Native,
                &session.resolver(),
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("source changed"), "{error}");
    assert!(stage.publish(&destination, crate::publish::Claim::Replace).is_err());
    assert_eq!(fs::read_to_string(&destination)?, "original transcript");
    Ok(())
}

#[test]
fn checked_openclaw_complete_preparation_preserves_ir_wire_report_and_late_failure() -> Result<()> {
    use crate::stream::Demand;
    let world = crate::test_world("openclaw-complete-preparation");
    let source = world.base.join("source.jsonl");
    let rows = [
        json!({"type":"session","version":4,"cwd":"/synthetic","timestamp":"2026-10-12T12:00:00Z","parentSession":"parent"}),
        json!({"type":"session_info","id":"info","name":"explicit"}),
        json!({"type":"message","id":"u","message":{"role":"user","content":"λ\n".repeat(10000),"provenance":{"kind":"internal_system","sourceTool":"cron"}}}),
        json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant","model":"m","content":[{"type":"thinking","thinking":"reasoning".repeat(1000),"thinkingSignature":"sig"},{"type":"toolCall","id":"call","name":"run","arguments":{"command":"echo λ"}},{"type":"unknown","text":"ignored"}],"provider":"p","usage":{"input":3,"output":2,"cost":{"total":0.1}},"diagnostics":{"rich":true}}}),
        json!({"type":"message","id":"t","message":{"role":"toolResult","toolCallId":"call","content":[{"text":"first"},{"type":"other","text":"λ".repeat(10000)}],"details":{"structured":true}}}),
        json!({"type":"message","message":{"role":"system","content":"owned system prompt"}}),
        json!({"type":"message","message":{"role":"custom","content":"custom body","display":true}}),
        json!({"type":"future_record","id":"future","payload":{"unknown":[true,"keep all"]}}),
        json!({"type":"label","id":"label","targetId":"u","label":"first"}),
    ];
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, &row)?;
        bytes.push(b'\n');
    }
    fs::write(&source, &bytes)?;
    let reference = crate::SessionRef::new("complete", Harness::OpenClaw, &source);
    let adapter = crate::harness::for_harness(Harness::OpenClaw).unwrap();
    let owned = crate::stream::collect_with(adapter.as_ref(), &reference, &Demand::COMPLETE)?;
    let checked = crate::emit::parse_for_emit(adapter.as_ref(), &reference, Harness::OpenClaw)?;
    assert!(checked.messages.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        Block::Text {
            text: crate::Text::Source(_)
        }
    ))));
    let mut materialized = checked.clone();
    materialized.materialize()?;
    assert_eq!(serde_json::to_value(&owned)?, serde_json::to_value(&materialized)?);
    assert!(owned.messages.iter().any(|m| m.kind == MessageKind::Carrier));
    for mode in ThinkingMode::ALL {
        let checked = crate::emit::parse_for_emit(adapter.as_ref(), &reference, Harness::OpenClaw)?;
        let opts = EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let (before, before_report) = crate::emit::emit_report(
            &owned,
            Harness::OpenClaw,
            &world.base.join(format!("owned-{mode}")),
            &opts,
        )?;
        let out = world.base.join(format!("checked-{mode}"));
        let (after, after_report) = crate::emit::emit_report(&checked, Harness::OpenClaw, &out, &opts)?;
        assert_eq!(normalized(&before.path)?, normalized(&after.path)?);
        assert_eq!(
            serde_json::to_value(before_report)?,
            serde_json::to_value(after_report)?
        );
        let index = after.path.parent().unwrap().join("sessions.json");
        assert_eq!(
            fs::read(before.path.parent().unwrap().join("sessions.json"))?,
            fs::read(&index)?
        );
        let transcript = fs::read(&after.path)?;
        let index_bytes = fs::read(&index)?;
        fs::write(&source, b"late replacement")?;
        assert!(crate::emit::emit_report(&checked, Harness::OpenClaw, &out, &opts).is_err());
        assert_eq!(fs::read(&after.path)?, transcript);
        assert_eq!(fs::read(&index)?, index_bytes);
        fs::write(&source, &bytes)?;
    }
    Ok(())
}

#[test]
fn tool_export_preserves_every_result_structured_details_and_images() -> Result<()> {
    let world = crate::test_world("openclaw-tool-export");
    let mut source = Session::new("tools", Harness::Claude);
    source.created_at = Some("2026-10-14T12:00:00Z".parse()?);
    let mut tool = Message::new(Role::Tool);
    let body = source_text(&world.base.join("body.json"), &"selected result λ\n".repeat(5000))?;
    tool.content = vec![
        Block::ToolResult {
            tool_use_id: "a".into(),
            content: body,
            is_error: false,
            tool_name: Some("first".into()),
            status: None,
            details: Some(json!({"patch":{"path":"file","lines":[1,2]}})),
        },
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("https://example.test/image".into()),
        },
        Block::ToolResult {
            tool_use_id: "b".into(),
            content: "second result".into(),
            is_error: true,
            tool_name: Some("second".into()),
            status: None,
            details: Some(json!({"failure":"rejected"})),
        },
    ];
    source.messages.push(tool);
    let result = emit(&source, &world.base.join("output"), &EmitOptions::default())?;
    let adapter = crate::harness::for_harness(Harness::OpenClaw).unwrap();
    let reference = crate::SessionRef::new(&result.new_id, Harness::OpenClaw, &result.path);
    let parsed = crate::stream::collect_with(adapter.as_ref(), &reference, &crate::stream::Demand::RICH)?;
    assert_eq!(parsed.messages.len(), 2);
    assert!(
        matches!(&parsed.messages[0].content[0], Block::ToolResult { tool_use_id, tool_name, is_error:false, details:Some(details), content, .. }
        if tool_use_id=="a" && tool_name.as_deref()==Some("first") && details==&json!({"patch":{"path":"file","lines":[1,2]}})
        && content.inline_str()==Some("selected result λ\n".repeat(5000).as_str()))
    );
    assert!(
        matches!(&parsed.messages[0].content[1], Block::Image { data_ref:Some(value), .. } if value=="https://example.test/image")
    );
    assert!(
        matches!(&parsed.messages[1].content[0], Block::ToolResult { tool_use_id, is_error:true, details:Some(details), content, .. }
        if tool_use_id=="b" && details==&json!({"failure":"rejected"}) && content.inline_str()==Some("second result"))
    );
    assert_eq!(parsed.messages[1].parent_id, parsed.messages[0].id);
    Ok(())
}

#[test]
fn native_complete_export_preserves_records_branch_links_and_one_compaction_pair() -> Result<()> {
    use crate::stream::Demand;
    let world = crate::test_world("openclaw-native-complete-export");
    let original =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openclaw/branchy-v4.jsonl"))?;
    let mut rows = original
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows[0]["futureHeader"] = json!({"preserve":true});
    rows[1]["message"]["futureUser"] = json!({"retain":"metadata"});
    rows[2]["message"]["content"].as_array_mut().unwrap().extend([
        json!({"type":"thinking","thinking":"guarded reasoning λ\n".repeat(1000),"thinkingSignature":"sig"}),
        json!({"type":"futureBlock","payload":{"retain":true}}),
    ]);
    rows[3]["message"]["diagnostics"] = json!({"inactive":"preserve"});
    rows.push(json!({"type":"future_record","id":"future","parentId":"a3","appendMode":"side","payload":{"large":"retained λ\n".repeat(1000)}}));
    rows.push(json!({"type":"label","id":"label","parentId":"a3","targetId":"u1","label":"keep target"}));
    let path = world.base.join("source.jsonl");
    let mut bytes = Vec::new();
    for row in &rows {
        serde_json::to_writer(&mut bytes, row)?;
        bytes.push(b'\n');
    }
    fs::write(&path, &bytes)?;
    let adapter = crate::harness::for_harness(Harness::OpenClaw).unwrap();
    let reference = crate::SessionRef::new("source", Harness::OpenClaw, &path);
    let source = crate::emit::parse_for_emit(adapter.as_ref(), &reference, Harness::OpenClaw)?;
    assert!(source.messages.iter().any(|m| m.complete_record.is_some()));
    // Text mode retains native reasoning when this target can hold it — the assumption the
    // per-mode expected rows below are built on, so it is pinned once, not per iteration.
    assert_eq!(
        crate::emit::thinking_fate_with_presence(Harness::OpenClaw, ThinkingMode::Text, true, Some("sig"), None, false),
        ThinkingFate::Keep
    );
    for mode in ThinkingMode::ALL {
        let result = emit(
            &source,
            &world.base.join(format!("output-{mode}")),
            &EmitOptions {
                new_id: Some("rehomed".into()),
                thinking: mode,
                ..Default::default()
            },
        )?;
        let actual = fs::read_to_string(&result.path)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            actual.len(),
            rows.len(),
            "one original row, including a single compaction record, must emit once"
        );
        let identities: HashMap<String, String> = actual
            .iter()
            .zip(&rows)
            .skip(1)
            .filter_map(|(a, b)| Some((a["id"].as_str()?.into(), b["id"].as_str()?.into())))
            .collect();
        for (index, (mut actual, expected)) in actual.into_iter().zip(&rows).enumerate() {
            let mut expected = expected.clone();
            if index == 0 {
                expected["id"] = json!("rehomed");
            } else {
                for field in [
                    "id",
                    "parentId",
                    "targetId",
                    "appendParentId",
                    "firstKeptEntryId",
                    "fromId",
                ] {
                    if let Some(old) = actual
                        .get(field)
                        .and_then(Value::as_str)
                        .and_then(|id| identities.get(id))
                    {
                        actual[field] = json!(old);
                    }
                }
                if mode != ThinkingMode::Native {
                    if let Some(parts) = expected.pointer_mut("/message/content").and_then(Value::as_array_mut) {
                        if mode == ThinkingMode::Drop {
                            parts.retain(|part| part["type"] != "thinking");
                        }
                    }
                }
            }
            assert_eq!(actual, expected, "row {index} mode {mode}");
        }
        let emitted = crate::SessionRef::new(&result.new_id, Harness::OpenClaw, &result.path);
        let parsed = crate::stream::collect_with(adapter.as_ref(), &emitted, &Demand::RICH)?;
        assert_eq!(
            parsed
                .messages
                .iter()
                .filter(|m| m.kind == MessageKind::CompactionBoundary)
                .count(),
            1
        );
        assert_eq!(
            parsed
                .messages
                .iter()
                .filter(|m| m.kind == MessageKind::CompactionSummary)
                .count(),
            1
        );
        assert!(!parsed
            .messages
            .iter()
            .any(|m| m.text().is_some_and(|text| text == "second answer (side branch)")));
    }
    // Actual IR edits must win over the retained raw record.
    let mut edited = source.clone();
    let user = edited
        .messages
        .iter_mut()
        .find(|m| m.id.as_deref() == Some("u1"))
        .unwrap();
    user.content[0] = Block::Text {
        text: "edited prompt".into(),
    };
    let result = emit(&edited, &world.base.join("edited"), &EmitOptions::default())?;
    let emitted = crate::SessionRef::new(&result.new_id, Harness::OpenClaw, &result.path);
    let parsed = crate::stream::collect_with(adapter.as_ref(), &emitted, &Demand::RICH)?;
    assert!(parsed
        .messages
        .iter()
        .any(|m| m.text().as_deref() == Some("edited prompt")));
    let mut owned = source.clone();
    owned.materialize()?;
    // The owned codec uses captured envelopes to reject metadata edits as well as body edits.
    let mut owned_edit = owned.clone();
    owned_edit
        .messages
        .iter_mut()
        .find(|m| m.id.as_deref() == Some("u1"))
        .unwrap()
        .timestamp = Some("2026-10-13T01:02:03Z".parse()?);
    let changed = emit(&owned_edit, &world.base.join("owned-edited"), &EmitOptions::default())?;
    let changed_ref = crate::SessionRef::new(&changed.new_id, Harness::OpenClaw, &changed.path);
    let reparsed = crate::stream::collect_with(adapter.as_ref(), &changed_ref, &Demand::RICH)?;
    assert!(reparsed
        .messages
        .iter()
        .any(|m| m.timestamp == Some("2026-10-13T01:02:03Z".parse().unwrap())));
    fs::write(&path, "changed source")?;
    assert!(emit(&source, &world.base.join("late-failure"), &EmitOptions::default()).is_err());
    fs::remove_file(&path)?;
    let owned_result = emit(&owned, &world.base.join("owned-source-gone"), &EmitOptions::default())?;
    let owned_rows = fs::read_to_string(owned_result.path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(owned_rows.len(), rows.len());
    assert_eq!(owned_rows[0]["futureHeader"], rows[0]["futureHeader"]);
    assert_eq!(owned_rows[1]["message"]["futureUser"], rows[1]["message"]["futureUser"]);
    assert_eq!(
        owned_rows[3]["message"]["diagnostics"],
        rows[3]["message"]["diagnostics"]
    );
    Ok(())
}

#[test]
fn fixed_target_identity_reproduces_openclaw_corpus_bytes() -> Result<()> {
    let world = crate::test_world("openclaw-repeatable-emit");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/openclaw/2026.1.29");
    let id = "28aa9586-8613-4ec8-bf4d-02549281e1f2";
    let reference = crate::SessionRef::new(
        id,
        Harness::OpenClaw,
        root.join(format!("agents/main/sessions/{id}.jsonl")),
    );
    let adapter = crate::harness::openclaw::OpenClaw::with_state_dir(&root);
    let source = crate::emit::parse_for_emit(&adapter, &reference, Harness::OpenClaw)?;
    let options = EmitOptions {
        new_id: Some("repeatable-target".into()),
        ..Default::default()
    };
    let first = emit(&source, &world.base.join("first"), &options)?;
    let second = emit(&source, &world.base.join("second"), &options)?;
    assert_eq!(
        crate::digest::sha256_hex(&fs::read(first.path)?),
        crate::digest::sha256_hex(&fs::read(second.path)?),
        "a fixed destination identity must produce the same transcript"
    );
    Ok(())
}

/// The index is claimed as it was read: another writer's update (or a new index) between the
/// read and the publish refuses the emit, and neither file is published.
#[test]
fn an_index_changed_after_the_read_is_refused_and_nothing_is_published() -> Result<()> {
    let world = crate::test_world("openclaw-claimed-index");
    let session = fixture(&world.base)?;
    for prior in [Some(r#"{"old":{"label":"index"}}"#), None] {
        let out = world.base.join(format!("out-{}", prior.is_some()));
        let dir = out.join("agents/main/sessions");
        fs::create_dir_all(&dir)?;
        if let Some(prior) = prior {
            fs::write(dir.join("sessions.json"), prior)?;
        }
        let other = r#"{"other":{"label":"a concurrent update"}}"#;
        let options = EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let error = stream::emit_after_stage(&session, &out, &options, || {
            Ok(fs::write(dir.join("sessions.json"), other)?)
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed while cv updated it"), "{error}");
        assert_eq!(fs::read_to_string(dir.join("sessions.json"))?, other);
        assert!(!dir.join("fixed.jsonl").exists());
        assert_eq!(fs::read_dir(&dir)?.count(), 1);
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
#[test]
fn emits_into_modern_openclaw_sqlite_store_and_round_trips() -> Result<()> {
    use crate::harness::{openclaw::OpenClaw, Adapter, Parse};
    let world = crate::test_world("openclaw-sqlite-emit");
    let agents = world.base.join("agents");
    let agent_dir = agents.join("main/agent");
    fs::create_dir_all(&agent_dir)?;
    let db = agent_dir.join("openclaw-agent.sqlite");
    {
        let conn = rusqlite::Connection::open(&db)?;
        conn.execute_batch(include_str!(
            "../../../../tests/fixtures/openclaw/transcript-tables.sql"
        ))?;
    }
    let mut session = fixture(&world.base)?;
    session.title = Some("Modern SQLite Window".into());
    let opts = EmitOptions {
        new_id: Some("sql-win-1".into()),
        ..Default::default()
    };
    let res = crate::emit::emit(&session, Harness::OpenClaw, &agents, &opts)?;
    assert_eq!(res.path, db);

    // Refuses overwrite without `force: true`, succeeds with `force: true`.
    assert!(crate::emit::emit(&session, Harness::OpenClaw, &agents, &opts).is_err());
    crate::emit::emit(
        &session,
        Harness::OpenClaw,
        &agents,
        &EmitOptions { force: true, ..opts },
    )?;

    let oc = OpenClaw::with_state_dir(&world.base);
    let refs = oc.discover()?;
    let found = refs.iter().find(|r| r.id == "sql-win-1").expect("discovered window");
    assert_eq!(found.title.as_deref(), Some("Modern SQLite Window"));
    let back = oc.parse(found)?;
    assert_eq!(
        back.extra["openclaw"]["session_key"].as_str(),
        Some("agent:main:sql-win-1")
    );
    assert!(!back.messages.is_empty());
    Ok(())
}
