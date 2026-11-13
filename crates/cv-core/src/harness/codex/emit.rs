//! The Codex emitter: an IR [`Session`] written into Codex's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::fidelity::unrepresentable_checked;
mod writer;
use crate::emit::*;
use crate::harness::store::JsonlStage;
use crate::harness::EmitResult;
use crate::ir::{Block, GitInfo, Harness, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    opts.validate_ids()?;
    crate::emit::validate_native_source(session, Harness::Codex, opts.thinking)?;
    let resolver = session.resolver();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    // Which tool calls exist at all, so a result whose call was pruned away can be paired below.
    let called = called_tool_ids(session);
    let mut custom_calls = std::collections::HashSet::<String>::new();
    // `session_meta.cwd` (and `turn_context.cwd`) are REQUIRED by Codex's decoder (`SessionMeta.cwd:
    // PathBuf`, no default): a meta line without one is undecodable, the thread id can't be read,
    // and resume fails with "failed to parse thread ID from rollout file". A session with no known
    // cwd (some harnesses never record one) lands in the home dir rather than nowhere.
    let cwd = effective_cwd(session, opts)
        .or_else(crate::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd_str = cwd.to_string_lossy().into_owned();

    let created = session.created_at.unwrap_or_else(Utc::now);
    // Path: out_dir/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl — in LOCAL time and with no
    // zone suffix, exactly as `RolloutFileName::render` writes it (`OffsetDateTime::now_local()`):
    // Codex's `RolloutFileName::parse` requires a 19-char timestamp with byte 19 == `-` before the
    // uuid, so a `…T11-04-49Z-<uuid>` name (what we used to write) is invisible to every
    // filename-based lookup (thread-id resolution, migration, `builder_from_items`) and the thread
    // only resumed while its sqlite row happened to exist.
    let local = created.with_timezone(&chrono::Local);
    let date_dir = out_dir
        .join(local.format("%Y").to_string())
        .join(local.format("%m").to_string())
        .join(local.format("%d").to_string());
    fs::create_dir_all(&date_dir).with_context(|| format!("creating {}", date_dir.display()))?;
    let stamp = local.format("%Y-%m-%dT%H-%M-%S").to_string();
    let file_path = date_dir.join(format!("rollout-{stamp}-{new_id}.jsonl"));

    let ts_str = created.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines = JsonlStage::new();

    // session_meta first line.
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    // `session_id` is the root-thread id (Codex backfills it from `id` on read; explicit is clearer).
    meta.insert("session_id".into(), json!(new_id));
    meta.insert("timestamp".into(), json!(ts_str));
    meta.insert("cwd".into(), json!(cwd_str));
    meta.insert("source".into(), json!("cli"));
    meta.insert("thread_source".into(), json!("user"));
    // We write the pre-paginated layout (`response_item` message + `event_msg` user/agent twins);
    // say so — Codex ≥ 0.147 keys its persistence policy and ordinal handling off `history_mode`,
    // defaulting to `legacy` when absent, and the legacy→paginated migration canonicalizes it.
    meta.insert("history_mode".into(), json!("legacy"));
    meta.insert("originator".into(), json!(super::CV_ORIGINATOR));
    // `SessionMeta.cli_version: String` has no serde default: without the key Codex's reader
    // rejects the rollout ("does not start with session metadata"). It names the Codex CLI that
    // wrote the file, which cv is not: carry a Codex source's recorded version, else Codex's own
    // spelling of unknown, `""` (its `SessionMeta` default, which `state::extract` skips).
    let cli_version = session
        .harness_extra(Harness::Codex)
        .and_then(|bag| bag.get("cli_version"))
        .and_then(Value::as_str)
        .unwrap_or("");
    meta.insert("cli_version".into(), json!(cli_version));
    // Codex persists this field into `state_5.sqlite.threads.model_provider` when it first
    // discovers the rollout. Omitting it creates an empty provider entry which the TUI then
    // cannot resume (`Model provider `` not found`), even though `turn_context.model` is valid.
    // The Codex target uses its standard OpenAI provider; the model id remains independently
    // carried by `turn_context` below.
    meta.insert("model_provider".into(), json!("openai"));
    // Session-level system prompt → Codex's own `base_instructions {text}` slot (the parser reads it
    // back into `Session::system_prompt`), so a ported session keeps the instructions it ran under.
    if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
        meta.insert("base_instructions".into(), json!({ "text": sp }));
    }
    if let Some(g) = &session.git {
        meta.insert("git".into(), codex_git(g));
    }
    writer::value(&mut lines, &ts_str, "session_meta", &Value::Object(meta))?;

    // The model rides in a `turn_context` record (per-turn config: model/cwd/approval/sandbox) —
    // that's the ONLY place Codex's own parser (and ours, `apply_turn_context`) reads a model id
    // from. (`session_meta.model_provider` is the *provider* name, not a model id; stuffing the
    // model there was silently dropped on re-parse.)
    // `TurnContextItem` requires `cwd`, `approval_policy`, `sandbox_policy`, `model` and `summary`
    // (no serde defaults); a `{cwd, model}` record fails `decode_rollout_line` and is silently
    // skipped on resume, so the model never actually came back. `on-request` + `workspace-write`
    // are Codex's own defaults (both decode with no further fields).
    if let Some(model) = &session.model {
        writer::value(
            &mut lines,
            &ts_str,
            "turn_context",
            &json!({
                "turn_id": Uuid::now_v7().to_string(),
                "cwd": cwd_str,
                "approval_policy": "on-request",
                "sandbox_policy": { "type": "workspace-write" },
                "model": model,
                "summary": "auto",
            }),
        )?;
    }

    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts_str.clone());

        // A compaction pair → Codex's own top-level `compacted` record (the parser reads it back as
        // a CompactionBoundary, plus a CompactionSummary when `message` is non-empty). The boundary
        // carries no text; the summary carries the seed text — both become `compacted` records so the
        // boundary survives even without a summary.
        if matches!(
            msg.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary
        ) {
            writer::record(&mut lines, &ts, "compacted", |out| {
                out.write_all(b"{\"message\":")?;
                if msg.kind == MessageKind::CompactionSummary {
                    writer::message_string(out, msg, &resolver, opts.thinking)?;
                } else {
                    serde_json::to_writer(&mut *out, "")?;
                }
                out.write_all(b"}")?;
                Ok(())
            })?;
            continue;
        }

        match msg.role {
            Role::System => {
                writer::message(
                    &mut lines,
                    &ts,
                    "developer",
                    "input_text",
                    None,
                    msg,
                    &resolver,
                    opts.thinking,
                )?;
            }
            Role::User => {
                writer::message(
                    &mut lines,
                    &ts,
                    "user",
                    "input_text",
                    Some("user_message"),
                    msg,
                    &resolver,
                    opts.thinking,
                )?;
            }
            Role::Assistant => {
                let first_record = lines.record_count();
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match crate::emit::thinking::fate(b, Harness::Codex, opts.thinking, &resolver)? {
                        ThinkingFate::Drop => continue,
                        fate @ (ThinkingFate::AsText | ThinkingFate::Placeholder) => {
                            let text = crate::emit::thinking::text_with_fate(b, fate).unwrap();
                            writer::text(&mut lines, &ts, &text, &resolver)?;
                            continue;
                        }
                        ThinkingFate::Keep => {}
                    }
                    match b {
                        Block::Text { text } => {
                            writer::text(&mut lines, &ts, text, &resolver)?;
                        }
                        Block::Thinking { text, encrypted, .. } => {
                            let summary = msg
                                .harness_extra(Harness::Codex)
                                .and_then(|extra| extra.get("reasoning_summary"))
                                .and_then(Value::as_str);
                            let present = match summary {
                                Some(summary) => !summary.is_empty(),
                                None => crate::emit::thinking::first_character(text, &resolver, false)?.is_some(),
                            };
                            writer::record(&mut lines, &ts, "response_item", |out| {
                                out.write_all(b"{\"type\":\"reasoning\",\"summary\":[")?;
                                if present {
                                    out.write_all(b"{\"type\":\"summary_text\",\"text\":")?;
                                    if let Some(summary) = summary {
                                        serde_json::to_writer(&mut *out, summary)?;
                                    } else {
                                        writer::string(out, text, &resolver, "")?;
                                    }
                                    out.write_all(b"}")?;
                                }
                                out.write_all(b"]")?;
                                if let Some(encrypted) = encrypted {
                                    out.write_all(b",\"encrypted_content\":")?;
                                    serde_json::to_writer(&mut *out, encrypted)?;
                                }
                                out.write_all(b"}")?;
                                Ok(())
                            })?;
                        }
                        Block::ToolUse {
                            id,
                            name,
                            namespace,
                            input,
                        } => {
                            let is_custom = writer::is_custom_tool_input(name, input)?;
                            if is_custom {
                                custom_calls.insert(id.clone());
                            }
                            writer::record(&mut lines, &ts, "response_item", |out| {
                                if is_custom {
                                    out.write_all(b"{\"type\":\"custom_tool_call\",\"call_id\":")?;
                                } else {
                                    out.write_all(b"{\"type\":\"function_call\",\"call_id\":")?;
                                }
                                serde_json::to_writer(&mut *out, id)?;
                                out.write_all(b",\"name\":")?;
                                serde_json::to_writer(&mut *out, name)?;
                                if let Some(namespace) = namespace {
                                    out.write_all(b",\"namespace\":")?;
                                    serde_json::to_writer(&mut *out, namespace)?;
                                }
                                if is_custom {
                                    out.write_all(b",\"input\":")?;
                                    writer::custom_input(out, input)?;
                                } else {
                                    out.write_all(b",\"arguments\":")?;
                                    writer::argument(out, input)?;
                                }
                                out.write_all(b"}")?;
                                Ok(())
                            })?;
                        }
                        Block::File { path, source, .. } => {
                            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                            writer::value(
                                &mut lines,
                                &ts,
                                "event_msg",
                                &json!({
                                    "type": "agent_message",
                                    "message": format!("[file: {label}]"),
                                }),
                            )?;
                        }
                        Block::ToolResult { .. } | Block::Image { .. } => {}
                    }
                }
                // Per-turn token usage → Codex's own `token_count` event, the ONLY record its parser
                // (and ours, `codex::apply_token_count`) reads usage from: it attaches
                // `info.last_token_usage` to the assistant message the event trails. Without it a
                // ported session read back `usage: 393 → 0`. Only when the turn actually wrote
                // records Codex reads back: trailing nothing (or only an empty `reasoning` item,
                // which its reader drops) the event would synthesize a bare usage-only turn.
                if lines.record_count() > first_record
                    && !unrepresentable_checked(Harness::Codex, opts.thinking, msg, &resolver)?
                {
                    if let Some(info) = codex_token_usage(msg.usage.as_ref())? {
                        writer::value(
                            &mut lines,
                            &ts,
                            "event_msg",
                            &json!({ "type": "token_count", "info": { "last_token_usage": info } }),
                        )?;
                    }
                }
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    {
                        // `FunctionCallOutputPayload` deserializes ONLY a string or a content-item
                        // array (and never serializes a `success` flag) — Codex itself has no
                        // persisted error bit. The object form `{content, success:false}` we used
                        // to write is undecodable: the line was dropped on resume, orphaning its
                        // `function_call`. Keep the error visible in the text instead; cv's own
                        // Codex parser recognizes the `[error] ` prefix and restores `is_error`.
                        // Codex pairs an output with the call above it and logs "Orphan function
                        // call output for call id" for one it cannot pair — which a pruned session
                        // produces, since its first turn can be a result whose call was snipped.
                        // Emit a call shell first so the pair is well-formed and the output's text
                        // still reaches the model, rather than dropping the turn.
                        if is_orphan_result(b, &called) {
                            let orphan_custom = matches!(tool_name.as_deref(), Some("exec" | "apply_patch"));
                            if orphan_custom {
                                custom_calls.insert(tool_use_id.clone());
                            }
                            let shell = if orphan_custom {
                                json!({
                                    "type": "custom_tool_call",
                                    "call_id": tool_use_id,
                                    "name": tool_name.clone().unwrap_or_else(|| "unknown".into()),
                                    "input": "",
                                })
                            } else {
                                json!({
                                    "type": "function_call",
                                    "call_id": tool_use_id,
                                    "name": tool_name.clone().unwrap_or_else(|| "unknown".into()),
                                    "arguments": "{}",
                                })
                            };
                            writer::value(&mut lines, &ts, "response_item", &shell)?;
                        }
                        let is_custom_out = custom_calls.contains(tool_use_id.as_str());
                        writer::record(&mut lines, &ts, "response_item", |out| {
                            if is_custom_out {
                                out.write_all(b"{\"type\":\"custom_tool_call_output\",\"call_id\":")?;
                            } else {
                                out.write_all(b"{\"type\":\"function_call_output\",\"call_id\":")?;
                            }
                            serde_json::to_writer(&mut *out, tool_use_id)?;
                            if let Some(name) = tool_name {
                                out.write_all(b",\"name\":")?;
                                serde_json::to_writer(&mut *out, name)?;
                            }
                            if let Some(namespace) = msg
                                .harness_extra(Harness::Codex)
                                .and_then(|bag| bag.get("namespace"))
                                .and_then(Value::as_str)
                            {
                                out.write_all(b",\"namespace\":")?;
                                serde_json::to_writer(&mut *out, namespace)?;
                            }
                            out.write_all(b",\"output\":")?;
                            writer::string(out, content, &resolver, if *is_error { "[error] " } else { "" })?;
                            out.write_all(b"}")?;
                            Ok(())
                        })?;
                    }
                }
            }
        }
    }

    lines.publish(&file_path, opts.claim())?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("codex resume {new_id}")),
    })
}

/// IR [`Usage`](crate::ir::Usage) → Codex's `last_token_usage` block, reversing
/// [`codex::parse_usage`]'s renames (`cache_read_tokens` → `cached_input_tokens`,
/// `cache_creation_tokens` → `cache_write_input_tokens`, `reasoning_tokens` →
/// `reasoning_output_tokens`). Only the fields actually present are written, so a Codex source
/// round-trips with the same key set. `None` when there is nothing Codex's parser would read back
/// (it requires at least one of input / output / cached-input).
fn codex_token_usage(usage: Option<&crate::ir::Usage>) -> Result<Option<Value>> {
    let Some(u) = usage else { return Ok(None) };
    let mut m = Map::new();
    // The IR's `input_tokens` excludes the cache counts; Codex's includes them.
    let input = u
        .input_tokens
        .map(|i| {
            i.checked_add(u.cache_read_tokens.unwrap_or(0))
                .and_then(|n| n.checked_add(u.cache_creation_tokens.unwrap_or(0)))
                .context("Codex input token count overflows u64")
        })
        .transpose()?;
    for (key, val) in [
        ("input_tokens", input),
        ("cached_input_tokens", u.cache_read_tokens),
        ("cache_write_input_tokens", u.cache_creation_tokens),
        ("output_tokens", u.output_tokens),
        ("reasoning_output_tokens", u.reasoning_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return Ok(None);
    }
    // Codex's own totals are input + output (cached input is a *subset* of its `input_tokens`, not
    // an addend — 35507 + 55 = 35562 in a real 0.155 record whose cached count was 34432).
    if let (Some(input), Some(output)) = (input, u.output_tokens) {
        let total = input
            .checked_add(output)
            .context("Codex total token count overflows u64")?;
        m.insert("total_tokens".into(), json!(total));
    }
    Ok(Some(Value::Object(m)))
}

fn codex_git(g: &GitInfo) -> Value {
    let mut m = Map::new();
    if let Some(b) = &g.branch {
        m.insert("branch".into(), json!(b));
    }
    if let Some(c) = &g.commit {
        m.insert("commit_hash".into(), json!(c));
    }
    if let Some(r) = &g.remote {
        m.insert("repository_url".into(), json!(r));
    }
    Value::Object(m)
}

#[cfg(test)]
mod staged_tests {
    use super::*;
    #[test]
    fn native_namespaced_tool_calls_and_named_results_round_trip() -> Result<()> {
        use crate::harness::{Adapter, Parse};
        use crate::ir::{Message, SessionRef};
        let world = crate::test_world("codex-tool-identity-wire");
        let mut session = Session::new("source", Harness::Codex);
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "call".into(),
            name: "send_message".into(),
            namespace: Some("collaboration".into()),
            input: json!({"target":"worker","message":"payload λ"}).into(),
        });
        let mut result = Message::new(Role::Tool);
        result
            .harness_extra_mut(Harness::Codex)
            .insert("namespace".into(), json!("collaboration"));
        result.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            content: "delivered".into(),
            is_error: false,
            tool_name: Some("send_message".into()),
            // A Codex output record has no status of its own, so none survives the round trip.
            status: None,
            details: None,
        });
        session.messages = vec![call, result];
        let emitted = emit(
            &session,
            &world.base,
            &EmitOptions {
                new_id: Some("tool-identity".into()),
                ..Default::default()
            },
        )?;
        let adapter = super::super::Codex::with_home(&world.home);
        let parsed = adapter.parse(&SessionRef::new("tool-identity", adapter.harness(), &emitted.path))?;
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(
            serde_json::to_value(&parsed.messages[0].content)?,
            serde_json::to_value(&session.messages[0].content)?
        );
        assert_eq!(
            serde_json::to_value(&parsed.messages[1].content)?,
            serde_json::to_value(&session.messages[1].content)?
        );
        assert_eq!(
            parsed.messages[1]
                .harness_extra(Harness::Codex)
                .and_then(|bag| bag.get("namespace")),
            Some(&json!("collaboration"))
        );
        Ok(())
    }

    #[test]
    fn usage_wire_rejects_overflow_and_keeps_partial_counts_partial() {
        for usage in [
            crate::ir::Usage {
                input_tokens: Some(u64::MAX),
                cache_read_tokens: Some(1),
                ..Default::default()
            },
            crate::ir::Usage {
                input_tokens: Some(u64::MAX),
                output_tokens: Some(1),
                ..Default::default()
            },
        ] {
            assert!(codex_token_usage(Some(&usage))
                .unwrap_err()
                .to_string()
                .contains("overflows u64"));
        }
        let value = codex_token_usage(Some(&crate::ir::Usage {
            output_tokens: Some(3),
            ..Default::default()
        }))
        .unwrap()
        .unwrap();
        assert_eq!(value["output_tokens"], 3);
        assert!(value.get("input_tokens").is_none() && value.get("total_tokens").is_none());
    }

    #[test]
    fn custom_tool_calls_and_raw_wrapped_inputs_round_trip() -> Result<()> {
        use crate::harness::{Adapter, Parse};
        use crate::ir::{Message, SessionRef};
        let world = crate::test_world("codex-custom-tool-wire");
        let mut session = Session::new("source", Harness::Codex);
        let script = "const r = await tools.exec_command({ cmd: \"cargo check\" });\ntext(r.output);";
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch";

        let mut c1 = Message::new(Role::Assistant);
        c1.content.push(Block::ToolUse {
            id: "call_exec".into(),
            name: "exec".into(),
            namespace: Some("functions".into()),
            input: json!(script).into(),
        });
        let mut r1 = Message::new(Role::Tool);
        r1.content.push(Block::ToolResult {
            tool_use_id: "call_exec".into(),
            content: "Script failed\nWall time 0.1 seconds".into(),
            is_error: true,
            tool_name: Some("exec".into()),
            status: None,
            details: None,
        });

        let mut c2 = Message::new(Role::Assistant);
        c2.content.push(Block::ToolUse {
            id: "call_patch".into(),
            name: "apply_patch".into(),
            namespace: None,
            input: json!({ "raw": patch }).into(),
        });
        let mut r2 = Message::new(Role::Tool);
        r2.content.push(Block::ToolResult {
            tool_use_id: "call_patch".into(),
            content: "Success.".into(),
            is_error: false,
            tool_name: Some("apply_patch".into()),
            status: None,
            details: None,
        });

        session.messages = vec![c1, r1, c2, r2];
        let emitted = emit(
            &session,
            &world.base,
            &EmitOptions {
                new_id: Some("custom-tools".into()),
                ..Default::default()
            },
        )?;
        let raw_text = fs::read_to_string(&emitted.path)?;
        let payload_types: Vec<String> = raw_text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "response_item")
            .filter_map(|v| v["payload"]["type"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            payload_types,
            vec![
                "custom_tool_call",
                "custom_tool_call_output",
                "custom_tool_call",
                "custom_tool_call_output",
            ]
        );

        let adapter = super::super::Codex::with_home(&world.home);
        let parsed = adapter.parse(&SessionRef::new("custom-tools", adapter.harness(), &emitted.path))?;
        assert_eq!(parsed.messages.len(), 4);
        assert_eq!(
            serde_json::to_value(&parsed.messages[0].content)?,
            json!([{
                "type": "tool_use",
                "id": "call_exec",
                "name": "exec",
                "namespace": "functions",
                "input": script,
            }])
        );
        assert_eq!(
            serde_json::to_value(&parsed.messages[1].content)?,
            json!([{
                "type": "tool_result",
                "tool_use_id": "call_exec",
                "content": "Script failed\nWall time 0.1 seconds",
                "is_error": true,
                "tool_name": "exec",
            }])
        );
        assert_eq!(
            serde_json::to_value(&parsed.messages[2].content)?,
            json!([{
                "type": "tool_use",
                "id": "call_patch",
                "name": "apply_patch",
                "input": patch,
            }])
        );
        Ok(())
    }
}
