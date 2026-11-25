//! Checked native fields; structural metadata keeps the existing writer's layout.
use super::*;
use crate::{
    harness::{
        store::bytes::ByteStage,
        wire::write::{text, ObjectWriter},
    },
    ir_json::PrettyJson,
    lazy::Resolver,
};
use std::io::{BufWriter, Write};

/// The checked transcript bytes, staged for publication.
pub(super) fn transcript(
    session: &Session,
    id: &str,
    title: Option<&str>,
    cwd: Option<&str>,
    mode: crate::emit::ThinkingMode,
) -> Result<ByteStage> {
    let resolver = Resolver::new(session.source_path.clone());
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        let mut root = ObjectWriter::new(&mut pretty)?;
        root.field("sessionId", id)?;
        if let Some(title) = title {
            root.field("title", title)?;
        }
        if let Some(cwd) = cwd {
            root.field("workspaceDirectory", cwd)?;
        }
        root.field_with("history", |w| {
            w.write_all(b"[")?;
            let mut first = true;
            for msg in &session.messages {
                let mut write = |result| {
                    if !first {
                        w.write_all(b",")?;
                    }
                    first = false;
                    item(w, msg, session.model.as_deref(), &resolver, mode, result)
                };
                let mut results = msg.content.iter().filter_map(|block| match block {
                    Block::ToolResult {
                        tool_use_id, content, ..
                    } if msg.role == Role::Tool => Some((tool_use_id.as_str(), content)),
                    _ => None,
                });
                match results.next() {
                    // One native item per result, each answering its call by `toolCallId`.
                    Some(first) => {
                        write(Some(first))?;
                        for result in results {
                            write(Some(result))?;
                        }
                    }
                    // A native tool message answers one call by `toolCallId`; a tool turn with no
                    // result has none to name, and an empty id is not one. Fail closed.
                    None => {
                        anyhow::ensure!(
                            msg.role != Role::Tool,
                            "Continue cannot hold a tool turn with no tool result ({}): it has no toolCallId",
                            msg.id.as_deref().unwrap_or("no id")
                        );
                        write(None)?;
                    }
                }
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
        root.finish()?;
    }
    Ok(stage.into_inner().map_err(|e| e.into_error())?)
}
/// Continue's OpenAI usage counts include cache reads in the prompt total.
pub(super) fn native_usage(usage: &Usage, model: Option<&str>) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    if let Some(input) = usage.input_tokens {
        let prompt = input
            .checked_add(usage.cache_read_tokens.unwrap_or(0))
            .ok_or_else(|| anyhow::anyhow!("Continue prompt token count overflows u64"))?;
        out.insert("prompt_tokens".into(), json!(prompt));
    }
    if let Some(output) = usage.output_tokens {
        out.insert("completion_tokens".into(), json!(output));
    }
    if let Some(cached) = usage.cache_read_tokens {
        out.insert("prompt_tokens_details".into(), json!({"cached_tokens":cached}));
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        out.insert(
            "completion_tokens_details".into(),
            json!({"reasoning_tokens":reasoning}),
        );
    }
    if let Some(cost) = usage.cost_usd {
        let cents = cost * 100.0;
        anyhow::ensure!(
            cost.is_finite() && cents.is_finite(),
            "Continue cost cannot be represented as finite cost_cents"
        );
        out.insert("cost_cents".into(), json!(cents));
    }
    if let Some(model) = model {
        out.insert("model".into(), json!(model));
    }
    Ok(out)
}

fn item(
    w: &mut dyn Write,
    msg: &Message,
    model: Option<&str>,
    resolver: &Resolver,
    mode: crate::emit::ThinkingMode,
    result: Option<(&str, &crate::lazy::Text)>,
) -> Result<()> {
    let mut item = ObjectWriter::new(w)?;
    item.field_with("message", |w| {
        let mut message = ObjectWriter::new(w)?;
        message.field(
            "role",
            match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            },
        )?;
        if let (Role::Tool, Some((id, content))) = (msg.role, result) {
            message.field("toolCallId", id)?;
            message.field_with("content", |w| text(w, content, resolver))?;
        } else {
            message.field_with("content", |w| {
                w.write_all(b"[")?;
                let mut first = true;
                for b in &msg.content {
                    let value = if mode == crate::emit::ThinkingMode::Native {
                        match b {
                            Block::Text { text, .. } => Some(std::borrow::Cow::Borrowed(text)),
                            Block::Thinking { text, .. }
                                if crate::emit::thinking::first_character(text, resolver, false)?.is_some() =>
                            {
                                Some(std::borrow::Cow::Borrowed(text))
                            }
                            _ => None,
                        }
                    } else {
                        crate::emit::thinking::text(b, Harness::Continue, mode, resolver)?
                    };
                    if let Some(value) = value {
                        if !first {
                            w.write_all(b",")?;
                        }
                        first = false;
                        let mut part = ObjectWriter::new(w)?;
                        part.field("type", "text")?;
                        part.field_with("text", |w| text(w, &value, resolver))?;
                        part.finish()?;
                    } else if let Block::Image {
                        data_ref: Some(url), ..
                    } = b
                    {
                        if !first {
                            w.write_all(b",")?;
                        }
                        first = false;
                        serde_json::to_writer(&mut *w, &json!({"type":"imageUrl","imageUrl":{"url":url}}))?;
                    }
                }
                w.write_all(b"]")?;
                Ok(())
            })?;
            // The calls are collected once; the `toolCalls` field is written only when there is one.
            let calls: Vec<_> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input, .. } if msg.role == Role::Assistant => Some((id, name, input)),
                    _ => None,
                })
                .collect();
            if !calls.is_empty() {
                message.field_with("toolCalls", |w| {
                    w.write_all(b"[")?;
                    let mut first = true;
                    for (id, name, input) in &calls {
                        if !first {
                            w.write_all(b",")?;
                        }
                        first = false;
                        let mut call = ObjectWriter::new(w)?;
                        call.field("id", id)?;
                        call.field("type", "function")?;
                        call.field_with("function", |w| {
                            let mut f = ObjectWriter::new(w)?;
                            f.field("name", name)?;
                            f.field_with("arguments", |w| {
                                crate::harness::store::text::generate_string(w, |builder| {
                                    input.with_view(|view| builder.push_json(view))
                                })
                            })?;
                            f.finish()
                        })?;
                        call.finish()?;
                    }
                    w.write_all(b"]")?;
                    Ok(())
                })?;
            }
        }
        if msg.role == Role::Assistant {
            if let Some(usage) = &msg.usage {
                message.field("usage", native_usage(usage, msg.model.as_deref())?)?;
            }
        }
        message.finish()
    })?;
    item.field_with("contextItems", |w| {
        w.write_all(b"[")?;
        let mut first = true;
        // Grouped tool results each become one native item, with no context items.
        // Checking the role before traversing avoids rescanning R blocks for R items.
        if msg.role != Role::Tool {
            for b in &msg.content {
                if let Block::File { path, source, mime, .. } = b {
                    if !first {
                        w.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut *w, &file_context_item(path, source, mime))?;
                }
            }
        }
        w.write_all(b"]")?;
        Ok(())
    })?;
    if msg.role == Role::Assistant {
        if let Some(model) = msg.model.as_deref().or(model) {
            item.field("promptLogs", json!([{"modelTitle":model}]))?;
        }
    }
    item.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cn_usage_cost_and_partial_fields_roundtrip_without_invented_counts() -> Result<()> {
        let world = crate::test_world("continue_usage_cost");
        let store = world.home.join(".continue");
        fs::create_dir_all(store.join("sessions"))?;
        let path = store.join("sessions/source.json");
        let body = "answer 雪\n\"\\".repeat(10_000);
        let doc = json!({"sessionId":"source","history":[
            {"message":{"role":"assistant","content":body,"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":3},"model":"native-model","cost_cents":2}}},
            {"message":{"role":"assistant","content":"partial","usage":{"prompt_tokens_details":{"cached_tokens":11},"cost_cents":0}}},
            {"message":{"role":"assistant","content":"empty usage","usage":{}}}
        ]});
        fs::write(&path, serde_json::to_vec(&doc)?)?;
        let adapter = crate::harness::continuedev::Continue::with_root(&store);
        let session = crate::stream::collect_with(
            &adapter,
            &crate::SessionRef::new("source", Harness::Continue, &path),
            &crate::stream::Demand::LEAN.with_spans(),
        )?;
        let opts = crate::emit::EmitOptions {
            new_id: Some("copy".into()),
            force: true, // this test emits the same pinned id again
            ..Default::default()
        };
        let (emitted, report) = crate::emit::emit_report(&session, Harness::Continue, &store.join("sessions"), &opts)?;
        assert!(!report
            .deltas
            .iter()
            .any(|delta| delta.what == "usage" || delta.what == "usage cost"));
        let reparsed = crate::stream::collect_with(
            &adapter,
            &crate::SessionRef::new("copy", Harness::Continue, &emitted.path),
            &crate::stream::Demand::RICH,
        )?;
        assert_eq!(reparsed.messages.len(), session.messages.len());
        for (source, copied) in session.messages.iter().zip(&reparsed.messages) {
            assert_eq!(
                serde_json::to_value(&source.usage)?,
                serde_json::to_value(&copied.usage)?
            );
            assert_eq!(source.model, copied.model);
        }
        let native: Value = serde_json::from_slice(&fs::read(&emitted.path)?)?;
        assert_eq!(native["history"][0]["message"]["usage"]["prompt_tokens"], 100);
        assert_eq!(native["history"][0]["message"]["usage"]["cost_cents"], 2.0);
        assert!(native["history"][1]["message"]["usage"].get("prompt_tokens").is_none());
        assert_eq!(native["history"][2]["message"]["usage"], json!({}));
        let mut unsupported = session.clone();
        unsupported.messages[0].usage.as_mut().unwrap().cache_creation_tokens = Some(17);
        let (_, report) = crate::emit::emit_report(&unsupported, Harness::Continue, &store.join("sessions"), &opts)?;
        let loss = report
            .deltas
            .iter()
            .find(|delta| delta.what == "usage cache creation tokens")
            .expect("native cache-write absence is reported");
        assert!(loss.expected);
        assert!(!report
            .deltas
            .iter()
            .any(|delta| delta.what == "usage" || delta.what == "usage cost"));
        let before = fs::read(&emitted.path)?;
        let index_before = fs::read(store.join("sessions/sessions.json"))?;
        let mut overflow = session.clone();
        overflow.messages[0].usage.as_mut().unwrap().input_tokens = Some(u64::MAX);
        overflow.messages[0].usage.as_mut().unwrap().cache_read_tokens = Some(1);
        let error = super::super::emit(&overflow, &store.join("sessions"), &opts).unwrap_err();
        assert!(error.to_string().contains("overflows u64"));
        overflow.messages[0].usage.as_mut().unwrap().input_tokens = Some(60);
        overflow.messages[0].usage.as_mut().unwrap().cost_usd = Some(f64::MAX);
        let error = super::super::emit(&overflow, &store.join("sessions"), &opts).unwrap_err();
        assert!(error.to_string().contains("finite cost_cents"));
        assert_eq!(fs::read(emitted.path)?, before);
        assert_eq!(fs::read(store.join("sessions/sessions.json"))?, index_before);
        Ok(())
    }

    #[test]
    fn cn_grouped_tool_results_emit_every_call_and_body() -> Result<()> {
        let world = crate::test_world("continue_grouped_results");
        let store = world.home.join(".continue");
        fs::create_dir_all(store.join("sessions"))?;
        let path = store.join("sessions/source.json");
        let first = "first 雪\n\"\\".repeat(10_000);
        let second = "second λ\n\"\\".repeat(10_000);
        let doc = json!({"sessionId":"source","history":[{
            "message":{"role":"assistant","content":"calls","toolCalls":[
                {"id":"one","function":{"name":"Read","arguments":"{}"}},
                {"id":"two","function":{"name":"Read","arguments":"{}"}}
            ]},
            "toolCallStates":[
                {"toolCallId":"one","status":"done","output":[{"content":first}]},
                {"toolCallId":"two","status":"done","output":[{"content":second}]}
            ]
        }]});
        fs::write(&path, serde_json::to_vec(&doc)?)?;
        let adapter = crate::harness::continuedev::Continue::with_root(&store);
        let r = crate::SessionRef::new("source", Harness::Continue, &path);
        let session = crate::stream::collect_with(&adapter, &r, &crate::stream::Demand::LEAN.with_spans())?;
        let grouped = session.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert_eq!(grouped.content.len(), 2);
        let opts = crate::emit::EmitOptions {
            new_id: Some("copy".into()),
            force: true, // this test emits the same pinned id again
            ..Default::default()
        };
        let emitted = super::super::emit(&session, &store.join("sessions"), &opts)?;
        let reparsed = crate::stream::collect_with(
            &adapter,
            &crate::SessionRef::new("copy", Harness::Continue, &emitted.path),
            &crate::stream::Demand::RICH,
        )?;
        let outputs = reparsed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult {
                    tool_use_id, content, ..
                } => Some((tool_use_id.as_str(), content.inline_str().unwrap())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs, vec![("one", first.as_str()), ("two", second.as_str())]);
        let native: Value = serde_json::from_slice(&fs::read(&emitted.path)?)?;
        assert_eq!(native["history"].as_array().unwrap().len(), 3);
        let before = fs::read(&emitted.path)?;
        fs::write(&path, b"changed source")?;
        assert!(super::super::emit(&session, &store.join("sessions"), &opts).is_err());
        assert_eq!(fs::read(emitted.path)?, before);
        Ok(())
    }

    #[test]
    fn checked_native_writer_matches_owned_oracle_and_preserves_changed_source() -> Result<()> {
        let world = crate::test_world("checked-continuedev");
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
        let mut lazy = crate::emit::parse_for_emit(&adapter, &reference, Harness::Continue)?;
        lazy.messages[0].content.extend([
            Block::File {
                path: Some("/synthetic/input.txt".into()),
                source: None,
                mime: Some("text/plain".into()),
                extensions: Default::default(),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("https://synthetic/image.png".into()),
                extensions: Default::default(),
            },
        ]);
        lazy.messages[1].content.push(Block::Thinking {
            text: "".into(),
            signature: Some("empty-signature".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        });
        let mut frozen = crate::emit::frozen::Frozen::new("continuedev-emit");
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
            frozen.insert(format!("{mode:?}"), json!([new.len(), crate::digest::sha256_hex(&new)]));
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
        let world = crate::test_world("late-argument-Continue");
        let source = world.base.join("arguments.json");
        fs::write(&source, br#"{"path":"old"}"#)?;
        let input = crate::Json::source(&source, 0, fs::metadata(&source)?.len())?;
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        session.created_at =
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
        let mut user = Message::new(Role::User);
        user.content.push(Block::from_text("inline prompt".into()));
        session.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Read".into(),
            input,
            namespace: None,
            extensions: Default::default(),
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
    fn dropped_source_failure_matches_direct_owned_error_contract() -> Result<()> {
        let world = crate::test_world("dropped-source-Continue");
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
            extensions: Default::default(),
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
        // Drop does not demand the unreadable span, so the checked writer still emits.
        fs::write(&path, b"short")?;
        if let Block::Thinking { text, .. } = &mut session.messages[0].content[0] {
            *text = crate::Text::Span(crate::lazy::Span {
                source: Some(path.clone()),
                offset: 0,
                len: 99,
                escaped: true,
            });
        }
        let new = super::super::emit(&session, &dest, &opts)?;
        let bytes = fs::read(&new.path)?;
        let mut frozen = crate::emit::frozen::Frozen::new("continuedev-emit-drop-span");
        frozen.insert("drop", json!([bytes.len(), crate::digest::sha256_hex(&bytes)]));
        frozen.check()?;
        assert_eq!(fs::read(&emitted.path)?, before);
        assert_eq!(fs::read(index).ok(), index_before);
        Ok(())
    }
    #[test]
    fn ignored_user_argument_source_failure_preserves_transcript_and_index() -> Result<()> {
        let world = crate::test_world("ignored-continue-arguments");
        let path = world.base.join("arguments.json");
        fs::write(&path, b"{}")?;
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        let mut user = Message::new(Role::User);
        user.content.push(Block::ToolUse {
            id: "ignored".into(),
            name: "Read".into(),
            input: crate::Json::source(&path, 0, 2)?,
            namespace: None,
            extensions: Default::default(),
        });
        session.messages.push(user);
        let dest = world.base.join("output");
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let emitted = super::super::emit(&session, &dest, &opts)?;
        let before = fs::read(&emitted.path)?;
        let index = dest.join("sessions.json");
        let index_before = fs::read(&index)?;
        fs::remove_file(path)?;
        assert!(super::super::emit(&session, &dest, &opts).is_err());
        assert_eq!(fs::read(emitted.path)?, before);
        assert_eq!(fs::read(index)?, index_before);
        Ok(())
    }

    #[test]
    fn wide_grouped_tool_results_preserve_each_item_and_empty_context() -> Result<()> {
        let world = crate::test_world("continue-wide-grouped-results");
        for count in [1, 128, 8192] {
            let mut session = Session::new("source", Harness::Continue);
            let mut tool = Message::new(Role::Tool);
            let mut expected = Vec::new();
            for index in 0..count {
                let id = format!("call-{index}");
                let body = format!("result-{index}\nλ");
                expected.push(json!({"message":{"role":"tool","toolCallId":id,"content":body},"contextItems":[]}));
                tool.content.push(Block::ToolResult {
                    tool_use_id: id,
                    content: body.into(),
                    is_error: false,
                    tool_name: None,
                    status: None,
                    details: None,
                    extensions: Default::default(),
                });
            }
            // Tool attachments/prose were already omitted by the native writer; the
            // shortcut must preserve that policy rather than copy user context rules.
            tool.content.push(Block::File {
                path: Some("ignored.txt".into()),
                source: None,
                mime: None,
                extensions: Default::default(),
            });
            tool.content.push(Block::from_text("ignored prose".into()));
            session.messages.push(tool);
            let path = world.base.join(format!("wide-{count}.json"));
            let stage = transcript(&session, "copy", None, None, crate::emit::ThinkingMode::Native)?;
            crate::harness::store::publish_stage(stage, &path, crate::publish::Claim::New)?;
            let actual: Value = serde_json::from_slice(&fs::read(path)?)?;
            assert_eq!(actual, json!({"sessionId":"copy","history":expected}));
        }
        Ok(())
    }

    /// A tool turn with no result has no `toolCallId` to write: the emit fails and publishes nothing.
    #[test]
    fn tool_turn_without_result_fails_closed() -> Result<()> {
        let world = crate::test_world("continue-tool-without-result");
        let mut session = Session::new("source", Harness::Continue);
        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::from_text("prose only".into()));
        session.messages.push(tool);
        let dest = world.base.join("output");
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let err = emit(&session, &dest, &opts).unwrap_err();
        assert!(format!("{err:#}").contains("toolCallId"), "{err:#}");
        assert!(!dest.join("fixed.json").exists());
        Ok(())
    }
}
