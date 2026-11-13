//! The Qwen Code emitter: an IR [`Session`] written as Qwen Code 0.25's session JSONL, the shape
//! [`super`] reads first. Registered in `crate::emit::emitter_for`.
//!
//! The file is `<projects>/<sanitized cwd>/chats/<sessionId>.jsonl`, one `ChatRecord` per line
//! (`{uuid, parentUuid, sessionId, timestamp, type, subtype?, provenance, cwd, version?, gitBranch?,
//! message?, model?, usageMetadata?, toolCallResult?, systemPayload?}`), each record's
//! `parentUuid` the previous record's `uuid`. `out_dir` is `~/.qwen/projects` (the adapter's first
//! root), `~/.qwen/tmp` (the first root of a home that only ever held the gemini-cli shape; the
//! file goes to the sibling `projects/`, the only place 0.25 looks), or a Qwen home itself
//! (`--out ~/.qwen`, or a scratch dir: the file goes to its `projects/`).

use crate::emit::*;
use crate::harness::wire::genai;
use crate::harness::{
    store::JsonlStage,
    wire::write::{self as wire, ObjectWriter},
};
use crate::lazy::{Resolver, Text};
use std::{borrow::Cow, io::Write};

use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// cv's Qwen project-directory spelling: replace each non-ASCII-alphanumeric character with
/// one dash (`/home/u/p` → `-home-u-p`). This is lossy: the session record retains the exact cwd,
/// and unique session IDs keep colliding directory spellings from overwriting one another.
fn sanitize_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `<home>/projects`, from the projects root, its `tmp/` sibling, or the home itself.
///
/// `--out` naming convention (same as lmstudio's `conversations`): the flag may name the CLI's
/// store root or the store dir inside it; accept either and write where the CLI reads.
fn projects_dir(out_dir: &Path) -> PathBuf {
    match out_dir.file_name().and_then(|n| n.to_str()) {
        Some("projects") => out_dir.to_path_buf(),
        Some("tmp") => out_dir.parent().unwrap_or(out_dir).join("projects"),
        _ => out_dir.join("projects"),
    }
}

/// Borrowed parts after the existing per-block thinking projection.
enum Part<'a> {
    Text(Cow<'a, Text>),
    Thinking { text: &'a Text, signature: Option<&'a str> },
    Other(&'a Block),
}
fn parts<'a>(content: &'a [Block], mode: ThinkingMode, resolver: &Resolver) -> Result<Vec<Part<'a>>> {
    let mut out = Vec::new();
    for block in content {
        let fate = crate::emit::thinking::fate(block, Harness::Qwen, mode, resolver)?;
        if fate == ThinkingFate::Drop {
            continue;
        }
        let text = match block {
            Block::Text { text } => Some(Cow::Borrowed(text)),
            _ => crate::emit::thinking::text_with_fate(block, fate),
        };
        if let Some(text) = text {
            if crate::emit::thinking::first_character(&text, resolver, false)?.is_some() {
                out.push(Part::Text(text));
            }
            continue;
        }
        match block {
            Block::Thinking { text, signature, .. } => {
                if crate::emit::thinking::first_character(text, resolver, false)?.is_some() {
                    out.push(Part::Thinking {
                        text,
                        signature: signature.as_deref(),
                    });
                }
            }
            Block::Image { .. } | Block::File { .. } | Block::ToolUse { .. } => out.push(Part::Other(block)),
            _ => {}
        }
    }
    Ok(out)
}
fn write_parts(writer: &mut dyn Write, parts: &[Part<'_>], resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    for (i, part) in parts.iter().enumerate() {
        if i != 0 {
            writer.write_all(b",")?;
        }
        let mut row = ObjectWriter::new(writer)?;
        match part {
            Part::Text(text) => genai::text_fields(&mut row, text, false, None, resolver)?,
            Part::Thinking { text, signature } => {
                genai::text_fields(&mut row, text, true, signature.as_deref(), resolver)?
            }
            Part::Other(Block::Image { media_type, data_ref }) => {
                genai::image_fields(&mut row, media_type.as_deref(), data_ref.as_deref())?
            }
            Part::Other(Block::File { mime, path, source }) => {
                genai::file_fields(&mut row, mime.as_deref(), path.as_deref(), source.as_deref())?
            }
            Part::Other(Block::ToolUse {
                id,
                name,
                input,
                namespace,
            }) => row.field_with("functionCall", |w| {
                let mut call = ObjectWriter::new(w)?;
                call.field("id", id)?;
                let deferred = namespace.as_deref() == Some(super::TOOL_CALL);
                call.field("name", if deferred { super::TOOL_CALL } else { name.as_str() })?;
                call.field_with("args", |w| {
                    if deferred {
                        let mut args = ObjectWriter::new(w)?;
                        args.field("name", name)?;
                        args.field_with("arguments", |w| input.write_canonical(&mut &mut *w))?;
                        args.finish()
                    } else {
                        input.write_canonical(&mut &mut *w)
                    }
                })?;
                call.finish()
            })?,
            Part::Other(_) => unreachable!("selected part"),
        }
        row.finish()?;
    }
    writer.write_all(b"]")?;
    Ok(())
}
fn projected_texts<'a>(message: &'a Message, mode: ThinkingMode, resolver: &Resolver) -> Result<Vec<Cow<'a, Text>>> {
    message
        .content
        .iter()
        .filter_map(
            |b| match crate::emit::thinking::text(b, Harness::Qwen, mode, resolver) {
                Ok(Some(text)) => Some(Ok(text)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            },
        )
        .collect()
}

fn usage_metadata(m: &Message) -> Result<Option<Value>> {
    let Some(u) = m.usage.as_ref() else { return Ok(None) };
    let cached = u.cache_read_tokens.unwrap_or(0);
    let input = u
        .input_tokens
        .map(|i| i.checked_add(cached).context("Qwen prompt token count overflows u64"))
        .transpose()?;
    let mut out = Map::new();
    if let Some(i) = input {
        out.insert("promptTokenCount".into(), json!(i));
    }
    if let Some(o) = u.output_tokens {
        out.insert("candidatesTokenCount".into(), json!(o));
    }
    if let Some(r) = u.reasoning_tokens {
        out.insert("thoughtsTokenCount".into(), json!(r));
    }
    if let Some(cached) = u.cache_read_tokens {
        out.insert("cachedContentTokenCount".into(), json!(cached));
    }
    if let (Some(input), Some(output)) = (input, u.output_tokens) {
        let total = input
            .checked_add(output)
            .context("Qwen total token count overflows u64")?;
        out.insert("totalTokenCount".into(), json!(total));
    }
    Ok(Some(Value::Object(out)))
}

/// The subtype a model-facing, harness-written user record carries: the one it was read with
/// (a Qwen→Qwen port), else one named for its origin.
fn injected_subtype(m: &Message) -> String {
    if let Some(s) = m
        .harness_extra(Harness::Qwen)
        .and_then(|e| e.get("subtype"))
        .and_then(Value::as_str)
    {
        return s.to_string();
    }
    match m.origin {
        Origin::Scheduler => "cron",
        Origin::Subagent => "agent_message",
        _ => "notification",
    }
    .to_string()
}

/// Appends records, each parented on the one before.
struct Writer<'a> {
    session_id: &'a str,
    cwd: String,
    version: Option<String>,
    branch: Option<String>,
    last_uuid: Option<String>,
    fallback_ts: String,
    lines: JsonlStage,
    forked_from: Option<&'a str>,
}

impl Writer<'_> {
    fn push(
        &mut self,
        ts: Option<String>,
        ty: &str,
        fields: impl FnOnce(&mut ObjectWriter<'_>) -> Result<()>,
    ) -> Result<()> {
        let uuid = Uuid::new_v4().to_string();
        let first = self.lines.is_empty();
        self.lines.push_with(|writer| {
            let mut row = ObjectWriter::new(writer)?;
            row.field("uuid", &uuid)?;
            row.field("parentUuid", &self.last_uuid)?;
            row.field("sessionId", self.session_id)?;
            row.field("timestamp", ts.as_deref().unwrap_or(&self.fallback_ts))?;
            row.field("type", ty)?;
            row.field("cwd", &self.cwd)?;
            if let Some(version) = &self.version {
                row.field("version", version)?;
            }
            if let Some(branch) = &self.branch {
                row.field("gitBranch", branch)?;
            }
            fields(&mut row)?;
            if first {
                if let Some(from) = self.forked_from {
                    row.field("forkedFrom", json!({ "sessionId": from }))?;
                }
            }
            row.finish()
        })?;
        self.last_uuid = Some(uuid);
        Ok(())
    }
    fn system(
        &mut self,
        ts: Option<String>,
        subtype: &str,
        payload: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<()> {
        self.push(ts, "system", |row| {
            row.field("subtype", subtype)?;
            row.field("provenance", "system")?;
            row.field_with("systemPayload", payload)
        })
    }
    fn user(
        &mut self,
        ts: Option<String>,
        subtype: Option<&str>,
        provenance: &str,
        parts: &[Part<'_>],
        resolver: &Resolver,
    ) -> Result<()> {
        self.push(ts, "user", |row| {
            if let Some(subtype) = subtype {
                row.field("subtype", subtype)?;
            }
            row.field("provenance", provenance)?;
            row.field_with("message", |w| message_parts(w, "user", parts, resolver))
        })
    }
}
fn message_parts(writer: &mut dyn Write, role: &str, parts: &[Part<'_>], resolver: &Resolver) -> Result<()> {
    let mut message = ObjectWriter::new(writer)?;
    message.field("role", role)?;
    message.field_with("parts", |w| write_parts(w, parts, resolver))?;
    message.finish()
}

/// Emit Qwen Code 0.25 session JSONL under `out_dir` (see the module doc for what it may be).
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    opts.validate_ids()?;
    crate::emit::validate_native_source(session, Harness::Qwen, opts.thinking)?;
    let resolver = session.resolver();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .context("a Qwen Code session is filed under its cwd; this session has none (pass --cwd)")?;
    let chats = projects_dir(out_dir).join(sanitize_cwd(&cwd_str)).join("chats");
    let path = chats.join(format!("{new_id}.jsonl"));

    let stamp = |t: Option<chrono::DateTime<Utc>>| t.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
    let mut w = Writer {
        session_id: &new_id,
        cwd: cwd_str.clone(),
        version: session
            .harness_extra(Harness::Qwen)
            .and_then(|e| e.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string),
        branch: branch_of(session),
        last_uuid: None,
        fallback_ts: stamp(session.created_at.or(session.updated_at))
            .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        lines: JsonlStage::new(),
        forked_from: session.lineage.forked_from.as_deref(),
    };

    let msgs = &session.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        let ts = stamp(m.timestamp);
        i += 1;
        match (m.role, m.kind) {
            // Qwen rebuilds its system prompt at launch; session messages cannot represent it.
            (_, MessageKind::SystemPrompt) => {}
            (Role::Assistant, _) => {
                let parts = parts(&m.content, opts.thinking, &resolver)?;
                if parts.is_empty() {
                    continue;
                }
                w.push(ts, "assistant", |row| {
                    row.field("provenance", "assistant_output")?;
                    if let Some(model) = m.model.as_ref().or(session.model.as_ref()) {
                        row.field("model", model)?;
                    }
                    row.field_with("message", |w| message_parts(w, "model", &parts, &resolver))?;
                    if let Some(usage) = usage_metadata(m)? {
                        row.field("usageMetadata", usage)?;
                    }
                    Ok(())
                })?;
            }
            (Role::Tool, _) => {
                for block in &m.content {
                    let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = block
                    else {
                        continue;
                    };
                    let key = if *is_error { "error" } else { "output" };
                    let name = tool_name.as_deref().unwrap_or("tool");
                    let status = if *is_error { "error" } else { "success" };
                    w.push(ts.clone(), "tool_result", |row| {
                        row.field("provenance", "tool_result")?;
                        row.field_with("message", |writer| {
                            let mut message = ObjectWriter::new(writer)?;
                            message.field("role", "user")?;
                            message.field_with("parts", |writer| {
                                writer.write_all(b"[")?;
                                let mut part = ObjectWriter::new(writer)?;
                                part.field_with("functionResponse", |writer| {
                                    let mut response = ObjectWriter::new(writer)?;
                                    response.field("id", tool_use_id)?;
                                    response.field("name", name)?;
                                    response.field_with("response", |writer| {
                                        let mut content_row = ObjectWriter::new(writer)?;
                                        content_row.field_with(key, |w| wire::text(w, content, &resolver))?;
                                        content_row.finish()
                                    })?;
                                    response.finish()
                                })?;
                                part.finish()?;
                                writer.write_all(b"]")?;
                                Ok(())
                            })?;
                            message.finish()
                        })?;
                        row.field(
                            "toolCallResult",
                            json!({ "callId": tool_use_id, "status": status, "resultDisplay": "" }),
                        )?;
                        Ok(())
                    })?;
                }
            }
            (Role::User, MessageKind::Prompt) => {
                let parts = parts(&m.content, opts.thinking, &resolver)?;
                if !parts.is_empty() {
                    w.user(ts, None, "real_user", &parts, &resolver)?;
                }
            }
            (_, MessageKind::CompactionBoundary) => {
                let next = msgs.get(i).filter(|n| n.kind == MessageKind::CompactionSummary);
                let compressed = next.map(|n| parts(&n.content, opts.thinking, &resolver)).transpose()?;
                if next.is_some() {
                    i += 1;
                }
                w.system(ts, "chat_compression", |writer| {
                    let mut payload = ObjectWriter::new(writer)?;
                    payload.field_with("info", |writer| {
                        let mut info = ObjectWriter::new(writer)?;
                        if let Some(extra) = m.harness_extra(Harness::Qwen) {
                            for (key, value) in extra {
                                if key != "subtype" {
                                    info.field(key, value)?;
                                }
                            }
                        }
                        info.finish()
                    })?;
                    if let Some(parts) = &compressed {
                        payload.field_with("compressedHistory", |writer| {
                            writer.write_all(b"[")?;
                            message_parts(writer, "user", parts, &resolver)?;
                            writer.write_all(b"]")?;
                            Ok(())
                        })?;
                    }
                    payload.finish()
                })?;
            }
            (_, MessageKind::ModelChange) => {
                if let Some(model) = &m.model {
                    w.system(ts, "session_model", |writer| {
                        let mut payload = ObjectWriter::new(writer)?;
                        payload.field("model", model)?;
                        payload.finish()
                    })?;
                }
            }
            (_, MessageKind::Error) => {
                let texts = projected_texts(m, opts.thinking, &resolver)?;
                w.system(ts, "turn_result", |writer| {
                    let mut payload = ObjectWriter::new(writer)?;
                    payload.field_with("error", |writer| {
                        let mut error = ObjectWriter::new(writer)?;
                        error.field_with("message", |w| {
                            wire::joined_text(w, texts.iter().map(Cow::as_ref), &resolver)
                        })?;
                        error.finish()
                    })?;
                    payload.finish()
                })?;
            }
            (_, MessageKind::Branch) => w.system(ts, "rewind", |writer| {
                writer.write_all(b"{}")?;
                Ok(())
            })?,
            (_, MessageKind::Notice) => {
                let texts = projected_texts(m, opts.thinking, &resolver)?;
                let mut present = false;
                for text in &texts {
                    if crate::emit::thinking::first_character(text, &resolver, false)?.is_some() {
                        present = true;
                        break;
                    }
                }
                if present {
                    w.system(ts, "notification", |writer| {
                        let mut payload = ObjectWriter::new(writer)?;
                        payload.field_with("text", |w| {
                            wire::joined_text(w, texts.iter().map(Cow::as_ref), &resolver)
                        })?;
                        payload.finish()
                    })?;
                }
            }
            // What remains arrives as model-facing injected input. Naming each kind keeps a
            // future MessageKind from silently degrading to this record shape.
            (
                Role::System,
                MessageKind::Prompt
                | MessageKind::Reply
                | MessageKind::ToolResult
                | MessageKind::InjectedContext
                | MessageKind::CompactionSummary
                | MessageKind::SubagentSpawn
                | MessageKind::SubagentReturn
                | MessageKind::Carrier,
            )
            | (
                Role::User,
                MessageKind::Reply
                | MessageKind::ToolResult
                | MessageKind::InjectedContext
                | MessageKind::CompactionSummary
                | MessageKind::SubagentSpawn
                | MessageKind::SubagentReturn
                | MessageKind::Carrier,
            ) => {
                let parts = parts(&m.content, opts.thinking, &resolver)?;
                if !parts.is_empty() {
                    let subtype = injected_subtype(m);
                    w.user(ts, Some(&subtype), "system", &parts, &resolver)?;
                }
            }
        }
    }
    if let Some(title) = &session.title {
        w.system(stamp(session.updated_at), "custom_title", |writer| {
            let mut payload = ObjectWriter::new(writer)?;
            payload.field("title", title)?;
            payload.finish()
        })?;
    }
    // Complete all checked record production before creating or opening the destination.
    fs::create_dir_all(&chats).with_context(|| format!("creating {}", chats.display()))?;
    if w.lines.is_empty() {
        // The original Qwen writer emits one blank line even for an empty session.
        crate::publish::publish_reader(&path, "stage", &b"\n"[..], opts.claim())?;
    } else {
        w.lines.publish(&path, opts.claim())?;
    }
    Ok(EmitResult {
        path,
        new_id: new_id.clone(),
        resume_hint: Some(crate::harness::resume_in(
            cwd.as_deref(),
            format!("qwen --resume {}", crate::harness::shell_quote(&new_id)),
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::qwen::Qwen;
    use crate::harness::{Adapter, Parse};

    #[test]
    fn usage_wire_preserves_absence_and_rejects_overflow() {
        let mut message = Message::new(Role::Assistant);
        message.usage = Some(crate::ir::Usage {
            output_tokens: Some(2),
            ..Default::default()
        });
        let value = usage_metadata(&message).unwrap().unwrap();
        assert_eq!(value, json!({"candidatesTokenCount":2}));
        message.usage.as_mut().unwrap().cache_read_tokens = Some(0);
        assert_eq!(usage_metadata(&message).unwrap().unwrap()["cachedContentTokenCount"], 0);
        message.usage.as_mut().unwrap().input_tokens = Some(u64::MAX);
        assert!(usage_metadata(&message)
            .unwrap_err()
            .to_string()
            .contains("overflows u64"));
        message.usage.as_mut().unwrap().cache_read_tokens = Some(1);
        assert!(usage_metadata(&message)
            .unwrap_err()
            .to_string()
            .contains("overflows u64"));
    }

    fn corpus_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/qwen/0.25.1/.qwen")
    }

    /// What a session says, turn by turn: role, kind, text, and each call's and result's tool.
    fn shape(s: &Session) -> Vec<(Role, MessageKind, String, Vec<String>)> {
        s.messages
            .iter()
            .map(|m| {
                let tools = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { name, .. } => Some(format!("use:{name}")),
                        Block::ToolResult {
                            tool_name, is_error, ..
                        } => Some(format!("result:{}:{is_error}", tool_name.as_deref().unwrap_or("?"))),
                        _ => None,
                    })
                    .collect();
                (m.role, m.kind, m.text().unwrap_or_default(), tools)
            })
            .collect()
    }

    /// Every session Qwen Code 0.25.1 wrote in the corpus, emitted into a fresh Qwen home and read
    /// back by the same reader through `discover`: the same turns, and no unexpected fidelity loss.
    #[test]
    fn corpus_sessions_round_trip_through_the_reader() {
        let src = Qwen::with_home(&corpus_home());
        let refs = src.discover().unwrap();
        assert!(refs.len() >= 20, "corpus sessions: {}", refs.len());
        let home = std::env::temp_dir().join(format!("cv-qwen-emit-{}", Uuid::new_v4()));
        for r in &refs {
            let s = src.parse(r).unwrap();
            let opts = EmitOptions {
                strict: true,
                ..EmitOptions::default()
            };
            let (res, _) = crate::emit::emit_verified(&s, Harness::Qwen, &home, &opts)
                .unwrap_or_else(|e| panic!("{}: {e:#}", r.id));
            assert!(res.path.starts_with(home.join("projects/-home-corpus-project/chats")));
            let back = Qwen::with_home(&home);
            let found = back
                .discover()
                .unwrap()
                .into_iter()
                .find(|x| x.id == res.new_id)
                .unwrap_or_else(|| panic!("{}: emitted session not discovered", r.id));
            let again = back.parse(&found).unwrap();
            assert_eq!(shape(&again), shape(&s), "{}", r.id);
            assert_eq!(again.cwd, s.cwd);
            assert_eq!(again.model, s.model);
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn writes_under_projects_from_a_home_its_tmp_or_projects() {
        let home = Path::new("/q/.qwen");
        for out in [home.to_path_buf(), home.join("tmp"), home.join("projects")] {
            assert_eq!(projects_dir(&out), home.join("projects"));
        }
        assert_eq!(sanitize_cwd("/home/u/my proj.x"), "-home-u-my-proj-x");
    }
}

#[cfg(test)]
mod staged_tests {
    use super::*;
    #[test]
    fn first_record_fork_and_empty_session_shape() -> Result<()> {
        let world = cv_testkit::World::new("qwen-stage");
        let mut session = Session::new("source", Harness::Qwen);
        session.cwd = Some("/synthetic/qwen".into());
        session.lineage.forked_from = Some("parent-session".into());
        let opts = EmitOptions {
            new_id: Some("synthetic-qwen".into()),
            force: true, // this test emits the same pinned id again
            ..Default::default()
        };
        let empty = emit(&session, &world.base, &opts)?;
        assert_eq!(fs::read(&empty.path)?, b"\n");
        for role in [Role::User, Role::Assistant] {
            let mut message = Message::new(role);
            message.content.push(Block::Text {
                text: "escaped\"\\\nλ".into(),
            });
            session.messages.push(message);
        }
        let result = emit(&session, &world.base, &opts)?;
        let body = fs::read_to_string(&result.path)?;
        assert!(body.ends_with('\n'));
        let records: Vec<Value> = body
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["forkedFrom"]["sessionId"], "parent-session");
        assert!(records[1].get("forkedFrom").is_none());
        assert_eq!(records[1]["parentUuid"], records[0]["uuid"]);
        assert_eq!(records[0]["message"]["parts"][0]["text"], "escaped\"\\\nλ");
        Ok(())
    }
}
