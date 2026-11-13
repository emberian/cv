use super::*;
use crate::{
    emit::{
        thinking::{fate, text_with_fate},
        ThinkingMode,
    },
    harness::{
        store::{bytes::ByteStage, publish_stage, text::generate_string},
        wire::write::{append_text, text, ObjectWriter},
    },
    ir_json::PrettyJson,
    lazy::Resolver,
};
use std::borrow::Cow;
use std::io::{BufWriter, Write};

/// A projected block, and whether it renders as a thinking step (the step's `style: thinking`).
struct Projected<'a> {
    block: Cow<'a, Block>,
    thinking: bool,
}

fn projected<'a>(block: &'a Block, mode: ThinkingMode, resolver: &Resolver) -> Result<Option<Projected<'a>>> {
    if let Block::Thinking { text, .. } = block {
        let fate = fate(block, Harness::LmStudio, mode, resolver)?;
        if let Some(value) = text_with_fate(block, fate) {
            return Ok(Some(Projected {
                block: match value {
                    Cow::Borrowed(_) => Cow::Borrowed(block),
                    Cow::Owned(text) => Cow::Owned(Block::Text { text }),
                },
                thinking: false,
            }));
        }
        if fate == crate::emit::ThinkingFate::Drop {
            return Ok(None);
        }
        if crate::emit::thinking::first_character(text, resolver, false)?.is_none() {
            return Ok(None);
        }
        return Ok(Some(Projected {
            block: Cow::Borrowed(block),
            thinking: true,
        }));
    }
    Ok(Some(Projected {
        block: Cow::Borrowed(block),
        thinking: false,
    }))
}
fn part(w: &mut dyn Write, block: &Block, resolver: &Resolver) -> Result<()> {
    match block {
        Block::Text { text: value } | Block::Thinking { text: value, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| text(w, value, resolver))?;
            o.finish()?;
        }
        Block::ToolUse { name, input, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| {
                generate_string(w, |b| {
                    b.push("[tool call: ")?;
                    b.push(name)?;
                    b.push(" ")?;
                    input.with_view(|v| b.push_json(v))?;
                    b.push("]")
                })
            })?;
            o.finish()?;
        }
        Block::ToolResult { content, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| {
                generate_string(w, |b| {
                    b.push("[tool result: ")?;
                    append_text(b, content, resolver)?;
                    b.push("]")
                })
            })?;
            o.finish()?;
        }
        _ => {
            if let Some(part) = emit_file_part(block) {
                serde_json::to_writer(&mut *w, &part)?;
            }
        }
    }
    Ok(())
}
fn parts(w: &mut dyn Write, block: &Block, resolver: &Resolver) -> Result<()> {
    w.write_all(b"[")?;
    part(w, block, resolver)?;
    w.write_all(b"]")?;
    Ok(())
}
fn blocks<'a>(msg: &'a Message, mode: ThinkingMode, resolver: &Resolver) -> Result<Vec<Projected<'a>>> {
    let mut blocks = msg
        .content
        .iter()
        .filter_map(|b| match projected(b, mode, resolver) {
            Ok(Some(b)) => Some(Ok(b)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<Result<Vec<_>>>()?;
    blocks.retain(|p| {
        !matches!(p.block.as_ref(), Block::File { .. } | Block::Image { .. }) || emit_file_part(&p.block).is_some()
    });
    Ok(blocks)
}

fn native_usage(msg: &Message) -> bool {
    msg.role == Role::Assistant
        && msg
            .usage
            .as_ref()
            .is_some_and(|usage| usage.input_tokens.is_some() || usage.output_tokens.is_some())
}

fn represented(msg: &Message, blocks: &[Projected<'_>]) -> bool {
    !blocks.is_empty() || native_usage(msg)
}

fn write_version(
    w: &mut dyn Write,
    session: &Session,
    msg: &Message,
    created: chrono::DateTime<Utc>,
    resolver: &Resolver,
    blocks: &[Projected<'_>],
) -> Result<()> {
    let mut version = ObjectWriter::new(w)?;
    version.field(
        "type",
        if msg.role == Role::Assistant {
            "multiStep"
        } else {
            "singleStep"
        },
    )?;
    // LM Studio has no tool role: tool results use a user turn with bracketed text parts.
    version.field(
        "role",
        if msg.role == Role::Assistant {
            "assistant"
        } else if msg.role == Role::System {
            "system"
        } else {
            "user"
        },
    )?;
    if msg.role == Role::Assistant {
        version.field_with("steps", |w| {
            w.write_all(b"[")?;
            let ts = msg.timestamp.unwrap_or(created);
            let step_id = format!("{}-0.0000000000000000", ts.timestamp_millis());
            let mut attached = false;
            for i in 0..blocks.len().max(usize::from(native_usage(msg))) {
                if i != 0 {
                    w.write_all(b",")?;
                }
                let mut step = ObjectWriter::new(w)?;
                step.field("type", "contentBlock")?;
                step.field("role", "assistant")?;
                step.field_with("content", |w| match blocks.get(i) {
                    Some(p) => parts(w, &p.block, resolver),
                    None => {
                        w.write_all(b"[]")?;
                        Ok(())
                    }
                })?;
                step.field("stepIdentifier", &step_id)?;
                if blocks.get(i).is_some_and(|p| p.thinking) {
                    step.field("style", json!({"type":"thinking"}))?;
                }
                if !attached {
                    let mut gen = Map::new();
                    if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                        gen.insert("indexedModelIdentifier".into(), json!(model));
                    }
                    if let Some(u) = &msg.usage {
                        let mut stats = Map::new();
                        if let Some(n) = u.input_tokens {
                            stats.insert("promptTokensCount".into(), json!(n));
                        }
                        if let Some(n) = u.output_tokens {
                            stats.insert("predictedTokensCount".into(), json!(n));
                        }
                        if !stats.is_empty() {
                            gen.insert("stats".into(), Value::Object(stats));
                        }
                    }
                    if !gen.is_empty() {
                        step.field("genInfo", gen)?;
                        attached = true;
                    }
                }
                step.finish()?;
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
    } else {
        version.field_with("content", |w| {
            w.write_all(b"[")?;
            for (i, p) in blocks.iter().enumerate() {
                if i != 0 {
                    w.write_all(b",")?;
                }
                part(w, &p.block, resolver)?;
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
    }
    version.finish()?;
    Ok(())
}

fn native_versions(message: &Message) -> Option<(usize, usize)> {
    let bag = message.harness_extra(Harness::LmStudio)?;
    let selected = usize::try_from(bag.get("currentlySelected")?.as_u64()?).ok()?;
    let count = usize::try_from(bag.get("version_count")?.as_u64()?).ok()?;
    (selected < count && message.kind != MessageKind::Branch).then_some((selected, count))
}

pub(super) fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<EmitResult> {
    opts.validate_ids()?;
    super::super::wire::write::check_source_fields(session)?;
    let resolver = Resolver::new(session.source_path.clone());
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| created.timestamp_millis().to_string());
    // `--out` naming convention (same as qwen's `projects_dir`): the flag may name the CLI's
    // store root or the store dir inside it; accept either and write where the CLI reads.
    let conversations = crate::emit::out_dir_within(out_dir, "conversations");
    let file_path = conversations.join(format!("{new_id}.conversation.json"));
    let title = super::super::wire::write::fallback_title(session, &resolver)?.unwrap_or_else(|| "Untitled".into());
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        let mut root = ObjectWriter::new(&mut pretty)?;
        root.field("name", title)?;
        root.field("createdAt", created.timestamp_millis())?;
        root.field_with("messages", |w| {
            w.write_all(b"[")?;
            let mut first = true;
            let mut messages = session.messages.iter().peekable();
            while let Some(msg) = messages.next() {
                let native = native_versions(msg);
                let (selected, count) = native.unwrap_or((0, 1));
                let mut group = std::collections::BTreeMap::new();
                group.insert(selected, msg);
                if native.is_some() {
                    while let Some(alternate) = messages.peek() {
                        let index = alternate
                            .harness_extra(Harness::LmStudio)
                            .and_then(|bag| bag.get("variant"))
                            .and_then(Value::as_u64)
                            .and_then(|index| usize::try_from(index).ok());
                        let Some(index) = index.filter(|index| *index < count && !group.contains_key(index)) else {
                            break;
                        };
                        if alternate.kind != MessageKind::Branch
                            || alternate
                                .harness_extra(Harness::LmStudio)
                                .and_then(|bag| bag.get("orphaned_variant"))
                                .and_then(Value::as_bool)
                                == Some(true)
                        {
                            break;
                        }
                        group.insert(index, messages.next().unwrap());
                    }
                }
                let group = group
                    .into_iter()
                    .map(|(index, message)| Ok((index, (message, blocks(message, opts.thinking, &resolver)?))))
                    .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
                if group.values().all(|(message, blocks)| !represented(message, blocks)) {
                    continue;
                }
                if !first {
                    w.write_all(b",")?;
                }
                first = false;
                let mut turn = ObjectWriter::new(w)?;
                turn.field_with("versions", |w| {
                    w.write_all(b"[")?;
                    for index in 0..count {
                        if index != 0 {
                            w.write_all(b",")?;
                        }
                        match group.get(&index) {
                            Some((message, blocks)) if represented(message, blocks) => {
                                write_version(w, session, message, created, &resolver, blocks)?
                            }
                            // Omitted or unsupported versions keep their native index without
                            // inventing visible text or changing the selected version.
                            _ => serde_json::to_writer(
                                &mut *w,
                                &json!({"type":"singleStep","role":"user","content":[]}),
                            )?,
                        }
                    }
                    w.write_all(b"]")?;
                    Ok(())
                })?;
                turn.field("currentlySelected", selected)?;
                turn.finish()?;
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
        // No `tokenCount`: LM Studio records the last generation's prompt token count there, a
        // fact the IR does not hold, and writing 0 claimed an empty context.
        root.field("pinned", false)?;
        if let Some(sp) = &session.system_prompt {
            root.field("systemPrompt", sp)?;
        }
        root.finish()?;
    }
    let stage = stage.into_inner().map_err(|e| e.into_error())?;
    fs::create_dir_all(file_path.parent().unwrap())?;
    publish_stage(stage, &file_path, opts.claim())?;
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some("open the chat in LM Studio".into()),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::frozen::Frozen;

    /// One emitted conversation, frozen: its length and SHA-256.
    fn digest(bytes: &[u8]) -> Value {
        json!([bytes.len(), crate::digest::sha256_hex(bytes)])
    }
    #[test]
    fn primary_adapter_root_and_explicit_conversations_emit_into_discoverable_store() -> Result<()> {
        let world = crate::test_world("lmstudio_root_composition");
        let home = world.home.join(".lmstudio");
        let conversations = home.join("conversations");
        fs::create_dir_all(&conversations)?;
        let adapter = LmStudio::with_root(&conversations);
        assert_eq!(adapter.roots(), vec![home]);
        let mut session = Session::new("source", Harness::LmStudio);
        let mut prompt = Message::new(Role::User);
        prompt.content.push(Block::Text {
            text: "visible in the native store".into(),
        });
        session.messages.push(prompt);
        for (root, id) in [
            (adapter.roots()[0].clone(), "primary"),
            (conversations.clone(), "explicit"),
        ] {
            let result = super::super::emit(
                &session,
                &root,
                &crate::emit::EmitOptions {
                    new_id: Some(id.into()),
                    ..Default::default()
                },
            )?;
            assert_eq!(result.path, conversations.join(format!("{id}.conversation.json")));
            assert!(adapter
                .discover()?
                .iter()
                .any(|reference| reference.id == id && reference.path == result.path));
        }
        assert!(!conversations.join("conversations").exists());
        Ok(())
    }

    #[test]
    fn usage_only_selected_and_alternate_completions_roundtrip_without_text() -> Result<()> {
        let world = crate::test_world("lmstudio_empty_completions");
        let path = world.home.join("source.conversation.json");
        let completion = |model: &str, stats: Value, timestamp: &str| {
            json!({"type":"multiStep","role":"assistant","steps":[{
                "type":"contentBlock","stepIdentifier":timestamp,"content":[],
                "genInfo":{"indexedModelIdentifier":model,"stats":stats}
            }]})
        };
        fs::write(
            &path,
            serde_json::to_vec(&json!({"createdAt":1730000000000_i64,"messages":[
                {"currentlySelected":0,"versions":[{"role":"user","content":"try again"}]},
                {"currentlySelected":1,"versions":[
                    completion("alternate", json!({"promptTokensCount":12,"predictedTokensCount":2}), "1730000001000-alt"),
                    completion("selected", json!({"promptTokensCount":9,"predictedTokensCount":0}), "1730000002000-selected"),
                    completion("ignored", json!({"unrelated":42}), "1730000003000-empty")
                ]}
            ]}))?,
        )?;
        let adapter = LmStudio::with_root(&world.home);
        let reference = SessionRef::new("source", Harness::LmStudio, &path);
        let complete = crate::stream::collect_with(&adapter, &reference, &Demand::COMPLETE)?;
        assert_eq!(complete.messages.len(), 3);
        assert_eq!(complete.model.as_deref(), Some("selected"));
        let selected = &complete.messages[1];
        assert!(selected.content.is_empty());
        assert_eq!(selected.usage.as_ref().unwrap().input_tokens, Some(9));
        assert_eq!(selected.usage.as_ref().unwrap().output_tokens, Some(0));
        assert_eq!(selected.timestamp.unwrap().timestamp_millis(), 1730000002000);
        let alternate = &complete.messages[2];
        assert_eq!(alternate.kind, MessageKind::Branch);
        assert_eq!(alternate.model.as_deref(), Some("alternate"));
        assert!(alternate.content.is_empty());
        assert_eq!(alternate.usage.as_ref().unwrap().output_tokens, Some(2));
        for options in [Demand::RICH, Demand::LEAN] {
            let visible = crate::stream::collect_with(&adapter, &reference, &options)?;
            assert_eq!(visible.messages.len(), 2);
            assert!(visible.messages[1].content.is_empty());
            assert_eq!(
                serde_json::to_value(&visible.messages[1].usage)?,
                serde_json::to_value(&selected.usage)?
            );
        }
        let emitted = super::super::emit(&complete, &world.home.join("output"), &Default::default())?;
        let native: Value = serde_json::from_slice(&fs::read(&emitted.path)?)?;
        assert_eq!(native["messages"].as_array().unwrap().len(), 2);
        assert_eq!(native["messages"][1]["currentlySelected"], 1);
        let versions = native["messages"][1]["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0]["steps"][0]["content"], json!([]));
        assert_eq!(versions[1]["steps"][0]["content"], json!([]));
        assert_eq!(versions[0]["steps"][0]["genInfo"]["stats"]["predictedTokensCount"], 2);
        assert_eq!(versions[1]["steps"][0]["genInfo"]["stats"]["predictedTokensCount"], 0);
        let reparsed = crate::stream::collect_with(
            &adapter,
            &SessionRef::new("copy", Harness::LmStudio, &emitted.path),
            &Demand::COMPLETE,
        )?;
        assert_eq!(reparsed.model, complete.model);
        assert_eq!(
            serde_json::to_value(reparsed.messages)?,
            serde_json::to_value(complete.messages)?
        );
        // Empty cache-only IR usage has no corresponding native stats field.
        let mut unsupported = Message::new(Role::Assistant);
        unsupported.usage = Some(Usage {
            cache_read_tokens: Some(4),
            ..Default::default()
        });
        assert!(!represented(&unsupported, &[]));
        Ok(())
    }

    #[test]
    fn complete_native_versions_roundtrip_selected_indices_roles_and_models() -> Result<()> {
        let world = crate::test_world("lmstudio_versions_roundtrip");
        let path = world.home.join("source.conversation.json");
        let payload = "edited 雪\n\"\\".repeat(10_000);
        let assistant = |text: &str, model: &str| {
            json!({"type":"multiStep","role":"assistant","steps":[{
                "type":"contentBlock","stepIdentifier":"1730000000000-random",
                "content":[{"type":"text","text":text}],
                "genInfo":{"identifier":model,"stats":{"promptTokensCount":8,"predictedTokensCount":3}}
            }]})
        };
        let doc = json!({"name":"versions","createdAt":1730000000000_i64,"messages":[
            {"currentlySelected":1,"versions":[
                {"role":"user","content":"old user"},
                {"role":"user","content":payload},
                null,
                {"role":"system","content":"alternate system"}
            ]},
            {"currentlySelected":99,"versions":[
                assistant("alternate reply", "alternate-model"),
                {"role":"invalid","content":"unparsed"},
                assistant(&payload, "selected-model")
            ]}
        ]});
        fs::write(&path, serde_json::to_vec(&doc)?)?;
        let adapter = LmStudio::with_root(&world.home);
        let r = SessionRef::new("source", Harness::LmStudio, &path);
        let opts = Demand {
            spans: true,
            ..Demand::COMPLETE
        };
        let session = crate::stream::collect_with(&adapter, &r, &opts)?;
        assert_eq!(session.messages.len(), 5);
        assert_eq!(session.messages[4].model.as_deref(), Some("alternate-model"));
        let emitted = super::super::emit(
            &session,
            &world.home.join("output"),
            &crate::emit::EmitOptions {
                new_id: Some("copy".into()),
                ..Default::default()
            },
        )?;
        let native: Value = serde_json::from_slice(&fs::read(&emitted.path)?)?;
        assert_eq!(
            native["messages"].as_array().unwrap().len(),
            2,
            "alternates stay versions of their native turn"
        );
        assert_eq!(native["messages"][0]["currentlySelected"], 1);
        assert_eq!(native["messages"][0]["versions"].as_array().unwrap().len(), 4);
        assert_eq!(native["messages"][0]["versions"][3]["role"], "system");
        assert_eq!(
            native["messages"][1]["currentlySelected"], 2,
            "invalid selection fell back to the last version"
        );
        let copied = SessionRef::new("copy", Harness::LmStudio, &emitted.path);
        let reparsed = crate::stream::collect_with(&adapter, &copied, &Demand::COMPLETE)?;
        let mut expected = session.clone();
        expected.materialize()?;
        assert_eq!(
            serde_json::to_value(&reparsed.messages)?,
            serde_json::to_value(&expected.messages)?
        );
        assert_eq!(reparsed.model, expected.model);
        let visible = crate::stream::collect_with(&adapter, &copied, &Demand::RICH)?;
        assert_eq!(visible.messages.len(), 2);
        assert_eq!(visible.messages[0].text().as_deref(), Some(payload.as_str()));
        assert_eq!(visible.messages[1].text().as_deref(), Some(payload.as_str()));
        let before = fs::read(&emitted.path)?;
        fs::write(&path, b"changed source")?;
        assert!(super::super::emit(
            &session,
            &world.home.join("output"),
            &crate::emit::EmitOptions {
                new_id: Some("copy".into()),
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(fs::read(emitted.path)?, before);
        Ok(())
    }

    #[test]
    fn an_unparsed_selected_turn_cannot_attach_its_alternates_to_the_previous_turn() -> Result<()> {
        let world = crate::test_world("lmstudio_orphan_group");
        let path = world.home.join("source.conversation.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({"messages":[
                {"currentlySelected":0,"versions":[{"role":"user","content":"primary"},{"role":"user","content":"own alternate"}]},
                {"currentlySelected":0,"versions":[{"role":"invalid","content":"not a parsed primary"},{"role":"user","content":"orphan alternate"}]}
            ]}))?,
        )?;
        let adapter = LmStudio::with_root(&world.home);
        let session = crate::stream::collect_with(
            &adapter,
            &SessionRef::new("source", Harness::LmStudio, &path),
            &Demand::COMPLETE,
        )?;
        assert_eq!(session.messages.len(), 3);
        assert_eq!(
            session.messages[2].harness_extra(Harness::LmStudio).unwrap()["orphaned_variant"],
            true
        );
        let emitted = super::super::emit(&session, &world.home.join("output"), &Default::default())?;
        let native: Value = serde_json::from_slice(&fs::read(emitted.path)?)?;
        assert_eq!(native["messages"].as_array().unwrap().len(), 2);
        assert_eq!(native["messages"][0]["versions"].as_array().unwrap().len(), 2);
        assert_eq!(native["messages"][1]["versions"].as_array().unwrap().len(), 1);
        assert_eq!(
            native["messages"][1]["versions"][0]["content"][0]["text"],
            "orphan alternate"
        );
        Ok(())
    }

    #[test]
    fn malformed_or_orphaned_variant_metadata_falls_back_to_independent_turns() -> Result<()> {
        let world = crate::test_world("lmstudio_variant_fallback");
        let mut session = Session::new("source", Harness::LmStudio);
        let mut primary = Message::new(Role::User);
        primary.content.push(Block::Text { text: "primary".into() });
        primary
            .harness_extra_mut(Harness::LmStudio)
            .insert("currentlySelected".into(), json!(5));
        primary
            .harness_extra_mut(Harness::LmStudio)
            .insert("version_count".into(), json!(2));
        let mut orphan = Message::new(Role::Assistant);
        orphan.kind = MessageKind::Branch;
        orphan.content.push(Block::Text { text: "orphan".into() });
        orphan
            .harness_extra_mut(Harness::LmStudio)
            .insert("variant".into(), json!(1));
        session.messages = vec![primary, orphan];
        let emitted = super::super::emit(&session, &world.home, &Default::default())?;
        let native: Value = serde_json::from_slice(&fs::read(emitted.path)?)?;
        assert_eq!(native["messages"].as_array().unwrap().len(), 2);
        assert!(native["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|turn| turn["currentlySelected"] == 0 && turn["versions"].as_array().unwrap().len() == 1));
        Ok(())
    }

    #[test]
    fn checked_native_writer_matches_frozen_output_and_preserves_changed_source() -> Result<()> {
        let world = crate::test_world("checked-lmstudio");
        let path = world.base.join("source.jsonl");
        let mut rows = [
            json!({"type":"user","message":{"role":"user","content":[{"type":"thinking","thinking":"user thought","signature":"user-sig"},{"type":"text","text":" \n"},{"type":"text","text":"λ prompt".repeat(3000)}]}}),
            json!({"type":"assistant","message":{"role":"assistant","model":"test-model","usage":{"input_tokens":7,"output_tokens":3},"content":[{"type":"thinking","thinking":"reasoning \nλ".repeat(3000),"signature":"sig"},{"type":"tool_use","id":"call","name":"Read","input":{"z":2,"a":"input"}},{"type":"text","text":"answer"}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"result \nλ".repeat(3000)}]}}),
        ];
        for row in &mut rows {
            row["timestamp"] = json!("2026-10-13T12:00:00Z");
        }
        fs::write(
            &path,
            rows.iter()
                .map(|v| serde_json::to_string(v).unwrap() + "\n")
                .collect::<String>(),
        )?;
        let adapter = crate::harness::claude::Claude::detect();
        let reference = crate::SessionRef::new("synthetic", Harness::Claude, &path);
        let mut lazy = crate::emit::parse_for_emit(&adapter, &reference, Harness::LmStudio)?;
        lazy.messages[0].content.extend([
            Block::File {
                path: Some("/synthetic/input.txt".into()),
                source: None,
                mime: Some("text/plain".into()),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("https://synthetic/image.png".into()),
            },
        ]);
        lazy.messages[1].content.push(Block::Thinking {
            text: "".into(),
            signature: Some("empty-signature".into()),
            encrypted: None,
            redacted: false,
        });
        for message in &mut lazy.messages[..2] {
            message.content.push(Block::Thinking {
                text: " \n\t ".into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
        }
        let mut frozen = Frozen::new("lmstudio-emit");
        for mode in [
            crate::emit::ThinkingMode::Native,
            crate::emit::ThinkingMode::Text,
            crate::emit::ThinkingMode::Drop,
        ] {
            let opts = crate::emit::EmitOptions {
                new_id: Some("fixed".into()),
                thinking: mode,
                ..Default::default()
            };
            let new = super::super::emit(&lazy, &world.base.join(format!("new-{mode:?}")), &opts)?;
            let new = fs::read(new.path)?;
            frozen.insert(format!("{mode:?}"), digest(&new));
        }
        frozen.check()?;
        let dest = world.base.join("mutation");
        fs::create_dir_all(&dest)?;
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let first = super::super::emit(&lazy, &dest, &opts)?;
        let before = fs::read(&first.path)?;
        fs::write(&path, b"changed")?;
        assert!(super::super::emit(&lazy, &dest, &opts).is_err());
        assert_eq!(fs::read(first.path)?, before);
        Ok(())
    }
    #[test]
    fn late_argument_source_failure_preserves_published_files() -> Result<()> {
        let world = crate::test_world("late-argument-LmStudio");
        let source = world.base.join("arguments.json");
        fs::write(&source, br#"{"path":"old"}"#)?;
        let input = crate::Json::source(&source, 0, fs::metadata(&source)?.len())?;
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        session.created_at =
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "inline prompt".into(),
        });
        session.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Read".into(),
            input,
            namespace: None,
        });
        session.messages.push(assistant);
        let dest = world.base.join("output");
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let result = super::super::emit(&session, &dest, &opts)?;
        let transcript = fs::read(&result.path)?;
        let index = dest.join("sessions.json");
        let index_before = fs::read(&index).ok();
        fs::write(&source, br#"{"path":"new"}"#)?;
        assert!(super::super::emit(&session, &dest, &opts).is_err());
        assert_eq!(fs::read(result.path)?, transcript);
        assert_eq!(fs::read(index).ok(), index_before);
        Ok(())
    }
    #[test]
    fn dropped_source_failure_preserves_published_files_and_drop_never_reads_thinking() -> Result<()> {
        let world = crate::test_world("dropped-source-LmStudio");
        let path = world.base.join("thinking.json");
        fs::write(&path, serde_json::to_vec(&"private thought".repeat(3000))?)?;
        let text = crate::Json::source(&path, 0, fs::metadata(&path)?.len())?
            .with_view(|v| v.source_text()?.ok_or_else(|| anyhow::anyhow!("text source")))?;
        assert!(matches!(text, crate::Text::Source(_)));
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        session.created_at =
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Thinking {
            text,
            signature: None,
            encrypted: None,
            redacted: false,
        });
        session.messages.push(message);
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            thinking: crate::emit::ThinkingMode::Drop,
            force: true, // this test emits the same pinned id again
            ..Default::default()
        };
        let dest = world.base.join("new");
        let emitted = super::super::emit(&session, &dest, &opts)?;
        let before = fs::read(&emitted.path)?;
        let index = dest.join("sessions.json");
        let index_before = fs::read(&index).ok();
        fs::remove_file(&path)?;
        assert!(super::super::emit(&session, &dest, &opts).is_err());
        assert_eq!(fs::read(&emitted.path)?, before);
        assert_eq!(fs::read(&index).ok(), index_before);
        // Drop never demands the thinking text, so an unreadable span is never read.
        fs::write(&path, b"short")?;
        if let Block::Thinking { text, .. } = &mut session.messages[0].content[0] {
            *text = crate::Text::Span(crate::lazy::Span {
                source: Some(path.clone()),
                offset: 0,
                len: 99,
                escaped: true,
            });
        }
        assert!(session.clone().materialize().is_err());
        let new = super::super::emit(&session, &dest, &opts)?;
        let mut frozen = Frozen::new("lmstudio-emit-drop");
        frozen.insert("unreadable span", digest(&fs::read(new.path)?));
        frozen.check()?;
        assert_eq!(fs::read(&emitted.path)?, before);
        assert_eq!(fs::read(index).ok(), index_before);
        Ok(())
    }
}
