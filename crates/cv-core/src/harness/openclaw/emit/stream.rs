//! Existing v3 records, written from guarded leaves without payload-bearing intermediate Values.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    lazy::{Resolver, Text},
};
use std::io::Write;

pub(super) fn content(writer: &mut dyn Write, blocks: &[Block], mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    for block in blocks {
        if matches!(block, Block::ToolResult { .. }) {
            continue;
        }
        let fate = crate::emit::thinking::fate(block, Harness::OpenClaw, mode, resolver)?;
        if fate == ThinkingFate::Drop {
            continue;
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        if let Some(text) = crate::emit::thinking::text_with_fate(block, fate) {
            let mut projected = ObjectWriter::new(writer)?;
            projected.field("type", "text")?;
            projected.field_with("text", |w| wire::text(w, &text, resolver))?;
            projected.finish()?;
        } else {
            match block {
                Block::ToolUse {
                    id,
                    name,
                    input,
                    namespace: Some(namespace),
                } if namespace == crate::affordance::DEFERRED => {
                    // The adapter's own wrapper supplies OpenClaw's distinct {id,args} keys.
                    let crate::affordance::DeferredWrapper { name_key, args_key } =
                        crate::harness::openclaw::DEFERRED_WRAPPER;
                    let mut call = ObjectWriter::new(writer)?;
                    call.field("type", anthropic::OPENCLAW.tool_use.0)?;
                    call.field("id", id)?;
                    call.field("name", crate::affordance::DEFERRED)?;
                    call.field_with(anthropic::OPENCLAW.tool_use.1, |w| {
                        let mut wrapper = ObjectWriter::new(w)?;
                        wrapper.field(name_key, name)?;
                        wrapper.field_with(args_key, |w| input.write_canonical(&mut &mut *w))?;
                        wrapper.finish()
                    })?;
                    call.finish()?;
                }
                _ => anthropic::write::block(writer, block, &anthropic::OPENCLAW, false, resolver)?,
            }
        }
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn summary(writer: &mut dyn Write, msg: &crate::Message, mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    let mut texts = Vec::new();
    for block in &msg.content {
        if let Some(text) = crate::emit::thinking::text(block, Harness::OpenClaw, mode, resolver)? {
            texts.push(text);
        }
    }
    wire::joined_text(writer, texts.iter().map(|v| v.as_ref()), resolver)
}

/// The first [`Block::ToolResult`] of a Tool turn, as the native row reads it.
#[derive(Clone, Copy)]
struct NativeResult<'a> {
    id: &'a str,
    content: &'a Text,
    is_error: bool,
    name: Option<&'a str>,
    details: Option<&'a Value>,
}

fn tool_result(writer: &mut dyn Write, blocks: &[Block], timestamp: i64, resolver: &Resolver) -> Result<()> {
    let result = blocks.iter().find_map(|block| {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            details,
            ..
        } = block
        {
            Some(NativeResult {
                id: tool_use_id.as_str(),
                content,
                is_error: *is_error,
                name: tool_name.as_deref(),
                details: details.as_ref(),
            })
        } else {
            None
        }
    });
    let mut inner = ObjectWriter::new(writer)?;
    inner.field("role", "toolResult")?;
    inner.field("toolCallId", result.map(|r| r.id).unwrap_or(""))?;
    if let Some(name) = result.and_then(|r| r.name) {
        inner.field("toolName", name)?;
    }
    inner.field_with("content", |w| {
        w.write_all(b"[")?;
        let mut part = ObjectWriter::new(w)?;
        part.field("type", "text")?;
        part.field_with("text", |w| match result {
            Some(r) => wire::text(w, r.content, resolver),
            None => serde_json::to_writer(w, "").map_err(Into::into),
        })?;
        part.finish()?;
        for block in blocks.iter().filter(|block| matches!(block, Block::Image { .. })) {
            w.write_all(b",")?;
            anthropic::write::block(w, block, &anthropic::OPENCLAW, false, resolver)?;
        }
        w.write_all(b"]")?;
        Ok(())
    })?;
    inner.field("isError", result.is_some_and(|r| r.is_error))?;
    if let Some(details) = result.and_then(|r| r.details) {
        inner.field("details", details)?;
    }
    inner.field("timestamp", timestamp)?;
    inner.finish()
}

pub(super) fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    emit_after_stage(session, out_dir, opts, || Ok(()))
}

pub(super) fn emit_after_stage(
    session: &Session,
    out_dir: &Path,
    opts: &EmitOptions,
    after_stage: impl FnOnce() -> Result<()>,
) -> Result<EmitResult> {
    wire::check_source_fields(session)?;
    let resolver = session.resolver();
    // Match explicit-title precedence, first-user selection, whitespace filtering and truncation.
    let label = wire::fallback_title(session, &resolver)?;
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let agents_root = crate::emit::out_dir_within(out_dir, "agents");
    let sessions_dir = agents_root.join("main/sessions");
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut lines = JsonlStage::new();
    #[cfg(feature = "sqlite")]
    let sqlite_target = detect_sqlite_target(out_dir, &agents_root);
    #[cfg(feature = "sqlite")]
    let mut sql_events: Vec<String> = Vec::new();
    let mut push_event = |write: &mut dyn FnMut(&mut dyn Write) -> Result<()>| -> Result<()> {
        #[cfg(feature = "sqlite")]
        if sqlite_target.is_some() {
            let mut buf = Vec::new();
            write(&mut buf)?;
            sql_events.push(String::from_utf8(buf)?);
            return Ok(());
        }
        lines.push_with(|w| write(w))
    };
    let native = super::replay::Native::new(session, &new_id);
    push_event(&mut |w| {
        if native.header(session, w, &new_id, &cwd_str)? {
            return Ok(());
        }
        let mut header = ObjectWriter::new(w)?;
        header.field("type", "session")?;
        header.field("version", 4)?;
        header.field("id", &new_id)?;
        header.field("timestamp", &created_iso)?;
        if let Some(parent) = &session.lineage.forked_from {
            header.field("parentSession", parent)?;
        }
        if !cwd_str.is_empty() {
            header.field("cwd", &cwd_str)?;
        }
        header.finish()
    })?;
    if let Some(title) = session
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty() && !native.has_title_record(session))
    {
        push_event(&mut |w| {
            let mut row = ObjectWriter::new(w)?;
            row.field("type", "session_info")?;
            row.field("id", native.generated_id("title"))?;
            row.field("parentId", Value::Null)?;
            row.field("timestamp", &created_iso)?;
            row.field("name", title)?;
            row.finish()
        })?;
    }
    let mut parent: Option<String> = None;
    let mut index = 0;
    while index < session.messages.len() {
        let msg = &session.messages[index];
        let paired = msg.kind == MessageKind::CompactionBoundary
            && msg.id.is_some()
            && session
                .messages
                .get(index + 1)
                .is_some_and(|next| next.kind == MessageKind::CompactionSummary && next.parent_id == msg.id);
        let summary_message = if paired { &session.messages[index + 1] } else { msg };
        let mut consumed = if paired { 2 } else { 1 };
        let ts = msg.timestamp.unwrap_or(created);
        let control = match msg.kind {
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary => Some("compaction"),
            MessageKind::Branch => Some("reset"),
            MessageKind::ModelChange => Some("model_change"),
            _ => None,
        };
        let mut remaining = msg.content.as_slice();
        let mut first_part = true;
        let mut part_index = 0;
        loop {
            let split = if msg.role == Role::Tool && control.is_none() {
                remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| matches!(block, Block::ToolResult { .. }))
                    .nth(1)
                    .map(|(i, _)| i)
            } else {
                None
            };
            let (blocks, rest) = if let Some(split) = split {
                let (a, b) = remaining.split_at(split);
                (a, Some(b))
            } else {
                (remaining, None)
            };
            let entry_id = if first_part {
                native.entry_id(msg, index)
            } else {
                native.generated_id(&format!("part:{index}:{part_index}"))
            };
            let row_parent = if first_part {
                native.parent(msg, &parent)
            } else {
                parent.clone()
            };
            let mut replayed = false;
            push_event(&mut |w| {
                if first_part {
                    if let Some(group) = native.record(&session.messages[index..], w, opts.thinking, &resolver)? {
                        consumed = group;
                        replayed = true;
                        return Ok(());
                    }
                }
                let mut row = ObjectWriter::new(w)?;
                row.field("type", control.unwrap_or("message"))?;
                row.field("id", &entry_id)?;
                row.field("parentId", &row_parent)?;
                row.field("timestamp", ts.to_rfc3339_opts(SecondsFormat::Millis, true))?;
                match control {
                    Some("compaction") => {
                        if msg.kind == MessageKind::CompactionSummary || paired {
                            row.field_with("summary", |w| summary(w, summary_message, opts.thinking, &resolver))?;
                        }
                    }
                    Some("reset") => row.field("reason", "reset")?,
                    Some("model_change") => {
                        row.field("provider", Value::Null)?;
                        row.field("modelId", msg.model.as_ref().or(session.model.as_ref()))?;
                    }
                    _ => row.field_with("message", |w| {
                        if msg.role == Role::Tool {
                            return tool_result(w, blocks, ts.timestamp_millis(), &resolver);
                        }
                        let mut inner = ObjectWriter::new(w)?;
                        inner.field(
                            "role",
                            match msg.role {
                                Role::User => "user",
                                Role::System => "system",
                                _ => "assistant",
                            },
                        )?;
                        inner.field_with("content", |w| content(w, &msg.content, opts.thinking, &resolver))?;
                        inner.field("timestamp", ts.timestamp_millis())?;
                        if msg.role == Role::Assistant {
                            if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                                inner.field("model", model)?;
                            }
                            if let Some(usage) = openclaw_usage(msg.usage.as_ref()) {
                                inner.field("usage", usage)?;
                            }
                        }
                        inner.finish()
                    })?,
                }
                row.finish()
            })?;
            // Id-less replayed records retain their own links and do not introduce an outer
            // entry id to parent the next message; generated or identified records do.
            if !replayed || msg.id.is_some() {
                parent = Some(entry_id);
            }
            if replayed {
                break;
            }
            let Some(rest) = rest else { break };
            remaining = rest;
            first_part = false;
            part_index += 1;
        }
        index += consumed;
    }
    drop(push_event);
    let session_key = canonical_session_key(session, &new_id);
    #[cfg(feature = "sqlite")]
    if let Some(db_path) = sqlite_target {
        after_stage()?;
        return emit_to_sqlite(
            session,
            &db_path,
            opts,
            &new_id,
            &session_key,
            label.as_deref(),
            &cwd_str,
            created,
            &sql_events,
        );
    }
    // All source demands complete before opening the transcript or changing its index.
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    let index_path = sessions_dir.join("sessions.json");
    // cv's update replaces OpenClaw's index whole, so an index it cannot read as one is refused,
    // never replaced by one holding only this session.
    let refused = || {
        format!(
            "reading the OpenClaw index {}; refusing to replace it",
            index_path.display()
        )
    };
    // The index is claimed as read: the file this update was computed from, or none.
    let (read, mut index): (Option<crate::source::FileId>, Map<String, Value>) = match fs::File::open(&index_path) {
        Ok(mut file) => {
            let id = crate::source::FileId::of(&file.metadata().with_context(refused)?);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes).with_context(refused)?;
            (id, serde_json::from_slice(&bytes).with_context(refused)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, Map::new()),
        Err(e) => return Err(e).with_context(refused),
    };
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(label) = label {
        entry.insert("label".into(), json!(label));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    if session_key != new_id {
        index.remove(&new_id);
    }
    index.insert(session_key, Value::Object(entry));
    let mut index_stage = crate::harness::store::bytes::ByteStage::new();
    serde_json::to_writer_pretty(&mut index_stage, &index)?;
    // The transcript and its index entry publish as one set, the index last: a crash between
    // leaves an unlisted transcript, never an entry naming a missing one.
    let mut set = crate::publish::Set::new();
    lines.stage_into(&mut set, &file_path, opts.claim())?;
    crate::harness::store::stage_into(&mut set, index_stage, &index_path, crate::publish::Claim::Replace)?;
    after_stage()?;
    // Another writer's index update since the read would be lost under ours: refuse, publish nothing.
    if crate::source::FileId::at(&index_path) != read {
        anyhow::bail!(
            "the OpenClaw index {} changed while cv updated it; rerun",
            index_path.display()
        );
    }
    set.publish()?;
    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}

fn canonical_session_key(session: &Session, new_id: &str) -> String {
    if let Some(existing) = session
        .extra
        .get("openclaw")
        .and_then(|v| v.get("session_key"))
        .and_then(Value::as_str)
        .filter(|k| k.starts_with("agent:"))
    {
        if new_id == session.id {
            return existing.to_string();
        }
        if let Some(prefix) = existing.strip_suffix(&session.id) {
            if prefix.ends_with(':') {
                return format!("{prefix}{new_id}");
            }
        }
    }
    format!("agent:main:{new_id}")
}

#[cfg(feature = "sqlite")]
fn detect_sqlite_target(out_dir: &Path, agents_root: &Path) -> Option<std::path::PathBuf> {
    if out_dir.extension().and_then(|e| e.to_str()) == Some("sqlite") {
        return Some(out_dir.to_path_buf());
    }
    for candidate in [
        agents_root.join("main/agent/openclaw-agent.sqlite"),
        out_dir.join("agent/openclaw-agent.sqlite"),
        out_dir.join("openclaw-agent.sqlite"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let agent_dir = agents_root.join("main/agent");
    if let Ok(entries) = fs::read_dir(&agent_dir) {
        let mut dbs: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("openclaw-agent") && n.ends_with(".sqlite"))
            })
            .collect();
        dbs.sort();
        if let Some(first) = dbs.into_iter().next() {
            return Some(first);
        }
    }
    None
}

#[cfg(feature = "sqlite")]
#[allow(clippy::too_many_arguments)]
fn emit_to_sqlite(
    session: &Session,
    db_path: &Path,
    opts: &EmitOptions,
    new_id: &str,
    session_key: &str,
    label: Option<&str>,
    cwd_str: &str,
    created: chrono::DateTime<Utc>,
    events: &[String],
) -> Result<EmitResult> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut conn = rusqlite::Connection::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS session_nodes (
           session_key TEXT NOT NULL PRIMARY KEY, current_session_id TEXT NOT NULL,
           entry_json TEXT NOT NULL, legacy_acp_migration_json TEXT,
           entry_valid INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, status TEXT,
           created_at INTEGER, parent_session_key TEXT, spawned_by TEXT, fork_source_session_key TEXT,
           fork_source_session_id TEXT, fork_source_entry_id TEXT, label TEXT, display_name TEXT,
           category TEXT, icon TEXT, pinned_at INTEGER, archived_at INTEGER, last_read_at INTEGER,
           last_interaction_at INTEGER, last_activity_at INTEGER) STRICT;
         CREATE TABLE IF NOT EXISTS session_windows (
           session_id TEXT NOT NULL PRIMARY KEY, session_key TEXT NOT NULL, previous_session_id TEXT,
           reason TEXT, session_scope TEXT NOT NULL DEFAULT 'conversation', created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL, transcript_updated_at INTEGER DEFAULT NULL,
           transcript_observed_at INTEGER DEFAULT NULL, started_at INTEGER, ended_at INTEGER, status TEXT,
           chat_type TEXT, channel TEXT, account_id TEXT, primary_conversation_id TEXT,
           model_provider TEXT, model TEXT, agent_harness_id TEXT, parent_session_key TEXT,
           spawned_by TEXT, display_name TEXT,
           FOREIGN KEY (session_key) REFERENCES session_nodes(session_key) ON DELETE CASCADE) STRICT;
         CREATE TABLE IF NOT EXISTS transcript_events (
           session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT,
           created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq),
           FOREIGN KEY (session_id) REFERENCES session_windows(session_id) ON DELETE CASCADE) STRICT;",
    )
    .with_context(|| format!("initializing OpenClaw SQLite schema in {}", db_path.display()))?;

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let exists: bool = tx
        .query_row("SELECT 1 FROM session_windows WHERE session_id = ?1", [new_id], |_| {
            Ok(true)
        })
        .unwrap_or(false);
    if exists && !opts.force {
        return Err(crate::publish::Exists {
            path: db_path.to_path_buf(),
            hint: "",
        }
        .into());
    }
    let created_ms = created.timestamp_millis();
    let updated_ms = epoch_ms(session.updated_at.or(session.created_at));
    let spawned_by = session
        .extra
        .get("openclaw")
        .and_then(|v| v.get("spawned_by"))
        .and_then(Value::as_str);
    let mut entry_obj = Map::new();
    entry_obj.insert("sessionId".into(), json!(new_id));
    if let Some(label) = label {
        entry_obj.insert("label".into(), json!(label));
    }
    if !cwd_str.is_empty() {
        entry_obj.insert("cwd".into(), json!(cwd_str));
    }
    entry_obj.insert("updatedAt".into(), json!(updated_ms));
    let entry_json = serde_json::to_string(&entry_obj)?;

    if exists {
        tx.execute("DELETE FROM transcript_events WHERE session_id = ?1", [new_id])?;
    }
    tx.execute(
        "INSERT INTO session_nodes (session_key, current_session_id, entry_json, entry_valid, updated_at, created_at, spawned_by, label, display_name)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(session_key) DO UPDATE SET
           current_session_id = excluded.current_session_id,
           entry_json = excluded.entry_json,
           entry_valid = 1,
           updated_at = excluded.updated_at,
           spawned_by = COALESCE(excluded.spawned_by, session_nodes.spawned_by),
           label = COALESCE(excluded.label, session_nodes.label),
           display_name = COALESCE(excluded.display_name, session_nodes.display_name)",
        rusqlite::params![session_key, new_id, entry_json, updated_ms, created_ms, spawned_by, label],
    )?;
    tx.execute(
        "INSERT INTO session_windows (session_id, session_key, created_at, updated_at, transcript_updated_at, model, spawned_by, display_name)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7)
         ON CONFLICT(session_id) DO UPDATE SET
           session_key = excluded.session_key,
           updated_at = excluded.updated_at,
           transcript_updated_at = excluded.transcript_updated_at,
           model = excluded.model,
           spawned_by = excluded.spawned_by,
           display_name = excluded.display_name",
        rusqlite::params![
            new_id,
            session_key,
            created_ms,
            updated_ms,
            session.model.as_deref(),
            spawned_by,
            label
        ],
    )?;
    {
        let mut insert_ev = tx.prepare(
            "INSERT INTO transcript_events (session_id, seq, event_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (seq, ev) in events.iter().enumerate() {
            insert_ev.execute(rusqlite::params![new_id, seq as i64, ev, created_ms])?;
        }
    }
    tx.commit()?;
    Ok(EmitResult {
        path: db_path.to_path_buf(),
        new_id: new_id.to_string(),
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}
