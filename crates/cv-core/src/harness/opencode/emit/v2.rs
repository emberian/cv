//! Native 2.0.25 schema; one checked, contiguous TEXT row at a time.
use super::super::v2::DETAILS_KEY;
use crate::{
    emit::*,
    harness::wire::write::{self, ObjectWriter},
    ir::*,
    lazy::Resolver,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{borrow::Cow, collections::HashMap, io::Write};
use uuid::Uuid;

/// Native empty-store bootstrap: install the complete schema and record its migration frontier.
/// The SQL and IDs are one versioned unit taken from the native-written 2.0.25 corpus.
fn bootstrap(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(include_str!("schema-v2.0.25.sql"))?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut insert = tx.prepare("INSERT INTO migration (id,time_completed) VALUES (?1,?2)")?;
    for id in include_str!("migrations-v2.0.25.txt").lines() {
        insert.execute(params![id, now])?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn write_database(
    session: &Session,
    opts: &EmitOptions,
    id: &str,
    cwd: &str,
    conn: &mut Connection,
) -> Result<()> {
    let dummy = crate::harness::EmitResult::default();
    write_database_with_verify(session, opts, id, cwd, conn, &dummy, None)
}

pub(super) fn write_database_with_verify(
    session: &Session,
    opts: &EmitOptions,
    id: &str,
    cwd: &str,
    conn: &mut Connection,
    result: &crate::harness::EmitResult,
    verify: Option<crate::emit::stage::VerifyFn<'_>>,
) -> Result<()> {
    anyhow::ensure!(id.starts_with("ses"), "OpenCode 2.x session IDs must start with 'ses'");
    let message_ids: Vec<String> = session
        .messages
        .iter()
        .map(|_| format!("msg_{}", Uuid::now_v7().simple()))
        .collect();
    let mut identities = HashMap::new();
    for message in &session.messages {
        if let Some(id) = message.id.as_deref() {
            *identities.entry(id).or_insert(0usize) += 1;
        }
    }
    let remap: HashMap<&str, &str> = session
        .messages
        .iter()
        .zip(&message_ids)
        .filter_map(|(message, new)| {
            message
                .id
                .as_deref()
                .filter(|id| identities.get(id) == Some(&1))
                .map(|old| (old, new.as_str()))
        })
        .collect();
    let resolver = session.resolver();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let empty: bool = tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    if empty {
        bootstrap(&tx)?;
    }
    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));
    // Reuse native ownership for this directory, otherwise the native non-git project.
    let project: String = tx
        .query_row(
            "SELECT project_id FROM session_v2 WHERE directory=?1 LIMIT 1",
            [cwd],
            |r| r.get(0),
        )
        .optional()?
        .or(tx
            .query_row("SELECT id FROM project WHERE worktree=?1 LIMIT 1", [cwd], |r| r.get(0))
            .optional()?)
        .unwrap_or_else(|| "global".into());
    tx.execute(
        "INSERT OR IGNORE INTO project (id,worktree,time_created,time_updated,sandboxes) VALUES (?1,?2,?3,?3,'[]')",
        params![project, cwd, created],
    )?;
    let bag = session.harness_extra(Harness::OpenCode);
    let provider = super::provider_id(session, session.model.as_deref().unwrap_or_default());
    let model = session
        .model
        .as_ref()
        .map(|m| json!({"id":m,"providerID":provider}).to_string());
    let exists: bool = tx
        .query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [id], |_| Ok(true))
        .unwrap_or(false);
    if exists && !opts.force {
        let db_path = tx.path().map(std::path::PathBuf::from).unwrap_or_default();
        return Err(crate::publish::Exists {
            path: db_path,
            hint: "",
        }
        .into());
    }
    if exists {
        tx.execute("DELETE FROM session_message WHERE session_id = ?1", [id])?;
        tx.execute("DELETE FROM event_sequence WHERE aggregate_id = ?1", [id])?;
        tx.execute("DELETE FROM session_v2 WHERE id = ?1", [id])?;
    }
    tx.execute("INSERT INTO session_v2 (id,project_id,slug,directory,title,version,model,agent,time_created,time_updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![id,project,short_hash(id),cwd,session.title,env!("CARGO_PKG_VERSION"),model,"build",created,updated])?;
    if session.harness == Harness::OpenCode {
        let mut statement = tx.prepare("PRAGMA table_info(session_v2)")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(bag) = bag {
            for (key, value) in bag {
                if !columns.contains(key)
                    || matches!(
                        key.as_str(),
                        "id" | "project_id"
                            | "directory"
                            | "slug"
                            | "title"
                            | "time_created"
                            | "time_updated"
                            | "time_suspended"
                            | "resume_attempts"
                            | "time_compacting"
                            | "workspace_id"
                    )
                {
                    continue;
                }
                // Only the rewritten keys need a private copy; the rest bind the bag value as-is.
                let mut rewritten;
                let value = match key.as_str() {
                    "model" => {
                        rewritten = value.clone();
                        if let Some(object) = rewritten.as_object_mut() {
                            if let Some(model) = &session.model {
                                object.insert("id".into(), json!(model));
                            }
                        }
                        &rewritten
                    }
                    "revert" => {
                        rewritten = value.clone();
                        remap_references(&mut rewritten, &remap, &session.id, id);
                        &rewritten
                    }
                    _ => value,
                };
                let sql = format!("UPDATE session_v2 SET \"{}\"=?1 WHERE id=?2", key.replace('"', "\"\""));
                tx.execute(&sql, params![sql_value(value), id])?;
            }
        }
    }
    let results: HashMap<&str, &Block> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some((tool_use_id.as_str(), b)),
            _ => None,
        })
        .collect();
    let called = called_tool_ids(session);
    let mut insert = tx.prepare("INSERT INTO session_message (id,session_id,type,seq,time_created,time_updated,data) VALUES (?1,?2,?3,?4,?5,?5,?6)")?;
    let mut seq = 0_i64;
    let mut owners = HashMap::new();
    for message in &session.messages {
        if let Some(owner) = message
            .harness_extra(Harness::OpenCode)
            .and_then(|extra| extra.get("record_owner"))
            .and_then(Value::as_str)
        {
            *owners.entry(owner).or_insert(0usize) += 1;
        }
    }
    let mut replay_consumed = 0;
    for (index, msg) in session.messages.iter().enumerate() {
        if replay_consumed > 0 {
            replay_consumed -= 1;
            continue;
        }
        if session.harness == Harness::OpenCode && msg.id.as_deref().is_some_and(|id| identities.get(id) == Some(&1)) {
            if let Some(record) = &msg.complete_record {
                let ty = record
                    .identity
                    .get("type")
                    .and_then(Value::as_str)
                    .context("complete OpenCode row has no type")?;
                let original_id = record
                    .identity
                    .get("id")
                    .and_then(Value::as_str)
                    .context("complete OpenCode row has no id")?;
                let group = match &record.value {
                    crate::Json::Source(_)
                        if record.unchanged(Harness::OpenCode, &session.messages[index..])
                            || record.text_overlay(Harness::OpenCode, &session.messages[index..])? =>
                    {
                        Some(record.projection.len())
                    }
                    crate::Json::Owned(data) => owned_replay_data(original_id, ty, data, &session.messages[index..]),
                    _ => None,
                };
                if let Some(group) = group {
                    if !msg.extra.contains_key(crate::ir::CARRIER_KEY)
                        && replayable(msg, index, group, &session.messages, &identities, &owners)
                        && replay_model_matches(
                            &record.value,
                            &session.messages[index..index + group],
                            session.model.as_deref(),
                        )?
                    {
                        let ts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created + seq);
                        let mid = &message_ids[index];
                        let mut bytes = Vec::new();
                        write_replayed(&record.value, &mut bytes, ty, msg, opts.thinking, &resolver)?;
                        insert.execute(params![mid, id, ty, seq, ts, String::from_utf8(bytes)?])?;
                        replay_consumed = group - 1;
                        seq += 1;
                        continue;
                    }
                }
            }
            if let Some(raw) = msg.extra.get(crate::ir::CARRIER_KEY) {
                if let Some((ty, group)) = owned_replay(raw, &session.messages[index..]) {
                    if replayable(msg, index, group, &session.messages, &identities, &owners) {
                        let ts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created + seq);
                        let mid = &message_ids[index];
                        let mut bytes = Vec::new();
                        write_replayed_view(
                            crate::json::JsonView::owned(raw),
                            &mut bytes,
                            ty,
                            msg,
                            opts.thinking,
                            &resolver,
                        )?;
                        insert.execute(params![mid, id, ty, seq, ts, String::from_utf8(bytes)?])?;
                        replay_consumed = group - 1;
                        seq += 1;
                        continue;
                    }
                }
            }
        }
        if msg.role == Role::Tool && !msg.content.iter().any(|b| is_orphan_result(b, &called)) {
            continue;
        }
        let ts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created + seq);
        let mid = &message_ids[index];
        let mut bytes = Vec::new();
        let mut row = ObjectWriter::new(&mut bytes)?;
        let extra = msg.harness_extra(Harness::OpenCode);
        let ty = if msg.role == Role::Assistant || msg.role == Role::Tool {
            "assistant"
        } else if msg.role == Role::System {
            "system"
        } else {
            "user"
        };
        {
            row.field("time", json!({"created":ts,"completed":ts}))?;
            if ty == "assistant" {
                row.field(
                    "agent",
                    extra
                        .and_then(|b| b.get("agent"))
                        .and_then(Value::as_str)
                        .unwrap_or("build"),
                )?;
                row.field("model",json!({"id":msg.model.as_ref().or(session.model.as_ref()).map(String::as_str).unwrap_or("unknown"),"providerID":extra.and_then(|b|b.get("providerID")).and_then(Value::as_str).unwrap_or(provider)}))?;
                for key in [
                    "finish",
                    "rawFinish",
                    "snapshot",
                    "error",
                    "retry",
                    "providerState",
                    "metadata",
                ] {
                    if let Some(value) = extra.and_then(|b| b.get(key)) {
                        row.field(key, value)?;
                    }
                }
                row.field_with("content", |w| {
                    assistant_content(w, msg, opts, &resolver, &results, &called, ts)
                })?;
                if let Some(u) = &msg.usage {
                    row.field("tokens",json!({"input":u.input_tokens.unwrap_or(0),"output":u.output_tokens.unwrap_or(0),"reasoning":u.reasoning_tokens.unwrap_or(0),"cache":{"read":u.cache_read_tokens.unwrap_or(0),"write":u.cache_creation_tokens.unwrap_or(0)}}))?;
                    if let Some(cost) = u.cost_usd {
                        row.field("cost", cost)?;
                    }
                }
            } else {
                row.field_with("text", |w| {
                    // A single JSON string streamed from checked text leaves; no decoded body copy.
                    let mut values = Vec::new();
                    for block in &msg.content {
                        match block {
                            Block::Text { text, .. } => values.push(Cow::Borrowed(text)),
                            Block::Thinking { .. } => {
                                let fate = thinking::fate(block, Harness::OpenCode, opts.thinking, &resolver)?;
                                if let Some(text) = thinking::text_with_fate(block, fate) {
                                    values.push(text);
                                }
                            }
                            _ => {}
                        }
                    }
                    for block in &msg.content {
                        let unavailable = match block {
                            Block::Image {
                                media_type, data_ref, ..
                            } if attachment(media_type.as_deref(), data_ref.as_deref()).is_none() => Some(format!(
                                "[Image attachment unavailable: {}]",
                                data_ref.as_deref().unwrap_or("no embedded data")
                            )),
                            Block::File { mime, path, source, .. }
                                if attachment(mime.as_deref(), source.as_deref().or(path.as_deref())).is_none() =>
                            {
                                Some(format!(
                                    "[File attachment unavailable: {}]",
                                    source.as_deref().or(path.as_deref()).unwrap_or("no embedded data")
                                ))
                            }
                            _ => None,
                        };
                        if let Some(text) = unavailable {
                            values.push(Cow::Owned(text.into()));
                        }
                    }
                    write::joined_text(w, values.iter().map(|text| text.as_ref()), &resolver)
                })?;
                if let Some(metadata) = extra.and_then(|b| b.get("metadata")) {
                    row.field("metadata", metadata)?;
                }
                let files: Vec<Attachment<'_>> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Image {
                            media_type, data_ref, ..
                        } => attachment(media_type.as_deref(), data_ref.as_deref()),
                        Block::File { mime, path, source, .. } => {
                            attachment(mime.as_deref(), source.as_deref().or(path.as_deref()))
                        }
                        _ => None,
                    })
                    .collect();
                if !files.is_empty() {
                    row.field("files", files)?;
                }
            }
        }
        row.finish()?;
        insert
            .execute(params![mid, id, ty, seq, ts, String::from_utf8(bytes)?])
            .context("inserting native OpenCode 2.x message")?;
        seq += 1;
    }
    drop(insert);
    // Native migration and Bus.publish share this durable frontier. Without it, the
    // first resumed event starts at zero and can collide with imported message seqs.
    tx.execute(
        "INSERT INTO event_sequence (aggregate_id,seq,owner_id) VALUES (?1,?2,NULL)",
        params![id, seq - 1],
    )?;
    write::check_source_fields(session)?;
    if let Some(verify_fn) = verify {
        verify_fn(crate::emit::stage::StagedSource::Sqlite { conn: &tx, result })?;
    }
    tx.commit()?;
    Ok(())
}

fn replay_model_matches(value: &crate::Json, group: &[Message], model: Option<&str>) -> Result<bool> {
    value.with_view(|view| {
        let original = view
            .get("model")?
            .map(|v| v.get("id"))
            .transpose()?
            .flatten()
            .map(|v| v.resolve())
            .transpose()?;
        let Some(original) = original.as_ref().and_then(|v| v.as_str()) else {
            return Ok(true);
        };
        let projected = group
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant || m.kind == MessageKind::ModelChange)
            .unwrap_or(&group[0]);
        Ok(projected.model.as_deref().or(model) == Some(original))
    })
}

fn write_replayed(
    value: &crate::Json,
    writer: &mut dyn Write,
    ty: &str,
    primary: &Message,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<()> {
    value.with_view(|view| write_replayed_view(view, writer, ty, primary, mode, resolver))
}
fn write_replayed_view(
    view: crate::json::JsonView<'_>,
    writer: &mut dyn Write,
    ty: &str,
    primary: &Message,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<()> {
    let mut out = ObjectWriter::new(writer)?;
    view.for_each_entry(|key, value| {
        if matches!(key, "id" | "type" | "sessionID") {
            return Ok(());
        }
        if key == "text" && matches!(ty, "user") {
            if let Some(Block::Text { text, .. }) = primary.content.first() {
                out.field_with(key, |w| write::text(w, text, resolver))?;
            } else {
                out.field_with(key, |mut w| value.write_canonical(&mut w))?;
            }
        } else if key == "content" && ty == "assistant" && mode != ThinkingMode::Native {
            out.field_with(key, |w| {
                w.write_all(b"[")?;
                let mut first = true;
                let mut thoughts = primary.content.iter().filter(|b| matches!(b, Block::Thinking { .. }));
                for item in value.array()?.context("native assistant content is not an array")? {
                    let item = item?;
                    let reasoning = item
                        .get("type")?
                        .map(|v| v.resolve())
                        .transpose()?
                        .is_some_and(|v| v.as_str() == Some("reasoning"));
                    let fate = if reasoning && mode == ThinkingMode::Drop {
                        ThinkingFate::Drop
                    } else if reasoning {
                        let text_present = match item.get("text")? {
                            Some(value) => match value.source_range()? {
                                Some((_, len)) => len > 2,
                                None => value.resolve()?.as_str().is_some_and(|text| !text.is_empty()),
                            },
                            None => false,
                        };
                        let signature = item
                            .get("state")?
                            .map(|v| v.resolve())
                            .transpose()?
                            .is_some_and(|state| {
                                super::super::record::find_signature(Some(&state)).is_some()
                                    || state.get("signature").and_then(Value::as_str).is_some()
                            });
                        if text_present || signature {
                            thoughts
                                .next()
                                .map(|block| thinking::fate(block, Harness::OpenCode, mode, resolver))
                                .transpose()?
                                .unwrap_or(ThinkingFate::Drop)
                        } else {
                            ThinkingFate::Drop
                        }
                    } else {
                        ThinkingFate::Keep
                    };
                    if fate == ThinkingFate::Drop {
                        continue;
                    }
                    if !first {
                        w.write_all(b",")?;
                    }
                    first = false;
                    item.write_canonical(&mut &mut *w)?;
                }
                w.write_all(b"]")?;
                Ok(())
            })?;
        } else {
            out.field_with(key, |mut w| value.write_canonical(&mut w))?;
        }
        Ok(())
    })?;
    out.finish()
}

fn sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Number(n) if n.as_i64().is_some() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
        Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
        Value::Bool(v) => rusqlite::types::Value::Integer(i64::from(*v)),
        v => rusqlite::types::Value::Text(v.to_string()),
    }
}
fn remap_references(value: &mut Value, ids: &HashMap<&str, &str>, source_session: &str, new_session: &str) {
    if let Some(object) = value.as_object_mut() {
        for (key, value) in object {
            if key == "messageID" {
                if let Some(new) = value.as_str().and_then(|old| ids.get(old)) {
                    *value = json!(new);
                }
            } else if key == "sessionID" && value.as_str() == Some(source_session) {
                *value = json!(new_session);
            } else {
                remap_references(value, ids, source_session, new_session);
            }
        }
    } else if let Some(array) = value.as_array_mut() {
        for value in array {
            remap_references(value, ids, source_session, new_session);
        }
    }
}

/// A replay group is intact at `index`: the primary and every follower still hold a unique
/// id, and the primary owns exactly `group - 1` follower records.
fn replayable(
    msg: &Message,
    index: usize,
    group: usize,
    messages: &[Message],
    identities: &HashMap<&str, usize>,
    owners: &HashMap<&str, usize>,
) -> bool {
    messages[index..index + group]
        .iter()
        .filter_map(|m| m.id.as_deref())
        .all(|id| identities.get(id) == Some(&1))
        && msg
            .id
            .as_deref()
            .is_some_and(|id| owners.get(id).copied().unwrap_or(0) == group - 1)
}

fn owned_replay<'a>(raw: &'a Value, current: &[Message]) -> Option<(&'a str, usize)> {
    let id = raw.get("id")?.as_str()?;
    let ty = raw.get("type")?.as_str()?;
    owned_replay_data(id, ty, raw, current).map(|count| (ty, count))
}
fn owned_replay_data(id: &str, ty: &str, data: &Value, current: &[Message]) -> Option<usize> {
    let mut original = super::super::v2::build(
        id,
        ty,
        crate::json::JsonView::owned(data),
        &crate::stream::Demand::COMPLETE,
        super::super::record::Ctx::Owned,
    )
    .ok()??;
    super::super::v2::tag_complete(&mut original, id, ty);
    let text_overlay = matches!(ty, "user")
        && data
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
    (!original.is_empty()
        && original.len() <= current.len()
        && original.iter().zip(current).enumerate().all(|(index, (a, b))| {
            crate::ir::record::same_message(a, b)
                || (index == 0 && text_overlay && crate::ir::record::same_except_first_text(a, b))
        }))
    .then_some(original.len())
}

#[derive(serde::Serialize)]
struct Attachment<'a> {
    data: &'a str,
    mime: &'a str,
    source: InlineSource,
}
#[derive(serde::Serialize)]
struct InlineSource {
    #[serde(rename = "type")]
    kind: &'static str,
}
fn attachment<'a>(mime: Option<&'a str>, uri: Option<&'a str>) -> Option<Attachment<'a>> {
    let (header, data) = uri?.strip_prefix("data:")?.split_once(",")?;
    let embedded_mime = header.strip_suffix(";base64")?;
    let padding = data.as_bytes().iter().rev().take_while(|b| **b == b'=').count();
    if padding > 2
        || data.len() % 4 != 0
        || !data[..data.len() - padding]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    Some(Attachment {
        data,
        mime: mime.unwrap_or(embedded_mime),
        source: InlineSource { kind: "inline" },
    })
}

fn assistant_content(
    w: &mut dyn Write,
    msg: &Message,
    opts: &EmitOptions,
    resolver: &Resolver,
    results: &HashMap<&str, &Block>,
    called: &std::collections::HashSet<&str>,
    ts: i64,
) -> Result<()> {
    w.write_all(b"[")?;
    let mut first = true;
    for block in &msg.content {
        let fate = thinking::fate(block, Harness::OpenCode, opts.thinking, resolver)?;
        if fate == ThinkingFate::Drop
            || matches!(block, Block::Opaque { .. })
            || matches!(block,Block::ToolResult{tool_use_id,..} if called.contains(tool_use_id.as_str()))
        {
            continue;
        }
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        let mut item = ObjectWriter::new(w)?;
        match block {
            Block::Text { text, .. } => {
                item.field("type", "text")?;
                item.field_with("text", |w| write::text(w, text, resolver))?;
            }
            Block::Thinking { text, signature, .. } if fate == ThinkingFate::Keep => {
                item.field("type", "reasoning")?;
                item.field_with("text", |w| write::text(w, text, resolver))?;
                if let Some(signature) = signature {
                    item.field("state", json!({"anthropic":{"signature":signature}}))?;
                }
            }
            Block::Thinking { .. } => {
                let text = thinking::text_with_fate(block, fate).unwrap();
                item.field("type", "text")?;
                item.field_with("text", |w| write::text(w, &text, resolver))?;
            }
            Block::ToolUse { id, name, input, .. } => {
                item.field("type", "tool")?;
                item.field("id", id)?;
                item.field("name", name)?;
                let result = results.get(id.as_str()).copied();
                tool_metadata(&mut item, result, ts)?;
                item.field_with("state", |w| tool_state(w, Some(input), result, resolver))?;
            }
            Block::ToolResult {
                tool_use_id, tool_name, ..
            } => {
                item.field("type", "tool")?;
                item.field("id", tool_use_id)?;
                item.field("name", tool_name.as_deref().unwrap_or("unknown"))?;
                tool_metadata(&mut item, Some(block), ts)?;
                item.field_with("state", |w| tool_state(w, None, Some(block), resolver))?;
            }
            Block::Opaque { .. } => unreachable!("opaque blocks filtered before publication"),
            // Native assistant content has no file variant. Preserve a visible URI as text.
            Block::Image { data_ref, .. } => {
                item.field("type", "text")?;
                item.field("text", data_ref.as_deref().unwrap_or("[image]"))?;
            }
            Block::File { path, source, .. } => {
                item.field("type", "text")?;
                item.field("text", source.as_deref().or(path.as_deref()).unwrap_or("[file]"))?;
            }
        }
        item.finish()?;
    }
    w.write_all(b"]")?;
    Ok(())
}
fn tool_metadata(item: &mut ObjectWriter<'_>, result: Option<&Block>, ts: i64) -> Result<()> {
    let details = match result {
        Some(Block::ToolResult { details, .. }) => details.as_ref(),
        _ => None,
    };
    let time = details
        .and_then(|d| d.get("time"))
        .filter(|t| t.get("created").and_then(Value::as_i64).is_some());
    if let Some(time) = time {
        item.field("time", time)?;
    } else {
        item.field("time", json!({"created":ts,"completed":ts}))?;
    }
    for key in ["executed", "providerState", "providerResultState"] {
        if let Some(value) = details.and_then(|d| d.get(key)) {
            item.field(key, value)?;
        }
    }
    Ok(())
}
// V2 keeps details in its clustervision envelope rather than the v1 state title/time fields;
// the v2 reader unwraps that envelope when reconstructing the tool result.
fn tool_state(
    w: &mut dyn Write,
    input: Option<&crate::Json>,
    result: Option<&Block>,
    resolver: &Resolver,
) -> Result<()> {
    let mut state = ObjectWriter::new(w)?;
    state.field_with("input", |w| match input {
        Some(input) => {
            input.with_view(|v| {
                anyhow::ensure!(v.is_object()?, "OpenCode 2.x tool arguments must be an object");
                Ok(())
            })?;
            input.write_canonical(&mut &mut *w)
        }
        None => {
            w.write_all(b"{}")?;
            Ok(())
        }
    })?;
    if let Some(Block::ToolResult {
        content,
        is_error,
        details,
        ..
    }) = result
    {
        state.field("status", if *is_error { "error" } else { "completed" })?;
        if *is_error {
            if let Some(error) = details.as_ref().and_then(|d| d.get("error")).filter(|e| {
                e.get("type").and_then(Value::as_str).is_some() && e.get("message").and_then(Value::as_str).is_some()
            }) {
                state.field("error", error)?;
            } else {
                state.field_with("error", |w| {
                    let mut error = ObjectWriter::new(w)?;
                    error.field("type", "Tool.Error")?;
                    error.field_with("message", |w| write::text(w, content, resolver))?;
                    error.finish()
                })?;
            }
        }
        state.field_with("content", |w| {
            w.write_all(b"[")?;
            let mut item = ObjectWriter::new(w)?;
            item.field("type", "text")?;
            item.field_with("text", |w| write::text(w, content, resolver))?;
            item.finish()?;
            w.write_all(b"]")?;
            Ok(())
        })?;
        if let Some(details) = details {
            state.field_with("metadata", |w| {
                let mut metadata = ObjectWriter::new(w)?;
                if let Some(native) = details.get("metadata").and_then(Value::as_object) {
                    for (key, value) in native {
                        if key != DETAILS_KEY {
                            metadata.field(key, value)?;
                        }
                    }
                }
                if details.get("metadata").is_some() || details.get("title").is_some() || details.get("time").is_some()
                {
                    metadata.field(DETAILS_KEY, details)?;
                } else if let Some(fields) = details.as_object() {
                    for (key, value) in fields {
                        metadata.field(key, value)?;
                    }
                } else {
                    metadata.field("details", details)?;
                }
                metadata.finish()
            })?;
        }
    } else {
        // No result is historical evidence, not a pending operation to execute on resume.
        state.field("status", "completed")?;
        state.field("content", json!([{"type":"text","text":""}]))?;
    }
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_v2_rows_and_fresh_bootstrap_roll_back() -> Result<()> {
        crate::harness::store::sqlite::export_tests::exercise_v2(|session, opts, conn| {
            write_database(session, opts, "ses_checked", "/synthetic/sqlite", conn)
        })
    }
    #[test]
    fn native_v2_export_fixture_and_atomic_duplicate() -> Result<()> {
        let root = std::env::var_os("CV_NATIVE_OPENCODE_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("cv-native-opencode-{}", Uuid::new_v4())));
        std::fs::create_dir_all(&root)?;
        let db = root.join("opencode.db");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db");
        std::fs::copy(fixture, &db)?;
        let mut s = Session::new("foreign", Harness::Claude);
        s.cwd = Some(root.join("project"));
        s.model = Some("fixture-model".into());
        s.title = Some("cv native v2 compatibility".into());
        let mut user = Message::new(Role::User);
        user.content.push(Block::from_text("fixture prompt".into()));
        user.content.push(Block::Image{media_type:Some("image/png".into()),data_ref:Some("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==".into()), extensions: Default::default(), });
        s.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::Thinking {
            text: "fixture reasoning".into(),
            signature: Some("fixture-signature".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        });
        assistant.content.push(Block::ToolUse {
            id: "fixture-tool".into(),
            name: "bash".into(),
            input: json!({"command":"printf fixture"}).into(),
            namespace: None,
            extensions: Default::default(),
        });
        assistant.content.push(Block::from_text("fixture answer".into()));
        s.messages.push(assistant);
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "fixture-tool".into(),
            content: "fixture output".into(),
            is_error: false,
            tool_name: Some("bash".into()),
            status: Some("completed".into()),
            details: Some(json!({"metadata":{"exit":0},"time":{"created":1700000000000_i64}})),
            extensions: Default::default(),
        });
        s.messages.push(result);
        // More rows than incidental startup events: missing native watermark cannot hide.
        for index in 0..16 {
            let mut historical = Message::new(Role::User);
            historical
                .content
                .push(Block::from_text(format!("fixture historical prompt {index}").into()));
            s.messages.push(historical);
        }
        let opts = EmitOptions {
            new_id: Some("ses_cv_native_v2".into()),
            ..Default::default()
        };
        super::super::emit(&s, &db, &opts)?;
        let conn = Connection::open(&db)?;
        let legacy: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='session'", [], |r| {
            r.get(0)
        })?;
        assert_eq!(legacy, 0);
        let rows: i64 = conn.query_row(
            "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(rows, 18);
        assert!(super::super::emit(&s, &db, &opts).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            18
        );
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM event_sequence WHERE aggregate_id='ses_cv_native_v2'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            17
        );
        let invalid = EmitOptions {
            new_id: Some("invalid-native-id".into()),
            ..Default::default()
        };
        assert!(super::super::emit(&s, &db, &invalid).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session_v2 WHERE id='invalid-native-id'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        let mut malformed = s.clone();
        if let Block::ToolUse { input, .. } = &mut malformed.messages[1].content[1] {
            *input = json!("invalid object").into();
        }
        let bad = EmitOptions {
            new_id: Some("ses_cv_failed_v2".into()),
            ..Default::default()
        };
        assert!(super::super::emit(&malformed, &db, &bad).is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session_v2 WHERE id='ses_cv_failed_v2'", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        let migrated = root.join("migrated");
        std::fs::create_dir_all(&migrated)?;
        let migrated_db = migrated.join("opencode.db");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus/opencode/1.18.20-migrated-2.0.25/.local/share/opencode/opencode.db"),
            &migrated_db,
        )?;
        let migrated_conn = Connection::open(&migrated_db)?;
        let legacy_before: i64 = migrated_conn.query_row("SELECT count(*) FROM message", [], |r| r.get(0))?;
        s.cwd = Some(migrated.join("project"));
        super::super::emit(&s, &migrated_db, &opts)?;
        assert_eq!(
            migrated_conn.query_row("SELECT count(*) FROM message", [], |r| r.get::<_, i64>(0))?,
            legacy_before
        );
        assert_eq!(
            migrated_conn.query_row(
                "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            18
        );
        drop(migrated_conn);
        if std::env::var_os("CV_NATIVE_OPENCODE_OUT").is_some() {
            use crate::harness::Adapter;
            let adapter = crate::harness::opencode::OpenCode::with_db(db.clone());
            let reference = adapter
                .discover()?
                .into_iter()
                .find(|r| r.id == "ses_cv_native_v2")
                .unwrap();
            let complete = crate::emit::parse_for_emit(&adapter, &reference, Harness::OpenCode)?;
            for (kind, mut rehome) in [("complete", complete.clone()), ("complete-edited", complete)] {
                let directory = root.join(kind);
                std::fs::create_dir_all(&directory)?;
                if kind == "complete-edited" {
                    if let Block::Text { text, .. } = &mut rehome.messages[0].content[0] {
                        *text = "fixture prompt edited".into();
                    }
                }
                let options = EmitOptions {
                    new_cwd: Some(directory.join("project")),
                    ..opts.clone()
                };
                let result = super::super::emit(&rehome, &directory, &options)?;
                let check = Connection::open(result.path)?;
                let data:String=check.query_row("SELECT data FROM session_message WHERE session_id='ses_cv_native_v2' AND type='user' ORDER BY seq LIMIT 1",[],|r|r.get(0))?;
                assert_eq!(
                    serde_json::from_str::<Value>(&data)?["files"][0]["data"],
                    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="
                );
            }

            let fresh = root.join("fresh-v2");
            std::fs::create_dir_all(&fresh)?;
            s.cwd = Some(fresh.join("project"));
            super::super::emit(&s, &fresh, &opts)?;
        }
        if std::env::var_os("CV_NATIVE_OPENCODE_OUT").is_none() {
            drop(conn);
            std::fs::remove_dir_all(root)?;
        }
        Ok(())
    }
}
