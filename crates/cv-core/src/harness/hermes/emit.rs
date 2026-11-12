//! The Hermes emitter: an IR [`Session`] written into Hermes's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit into Hermes's `state.db` SQLite store (creating/appending). Mirrors the schema the Hermes
/// reader (`harness/hermes.rs`) expects: a `sessions` row + OpenAI-shaped `messages` rows
/// (role/content/tool_calls JSON/reasoning/timestamps as REAL unix secs). Multimodal content uses
/// the `\x00json:` sentinel prefix.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::Connection;

    // Reject unreadable sources before creating output; the commit guard covers later changes.
    crate::harness::wire::write::check_source_fields(session)?;
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    // `out_dir` is a directory — the Hermes home or a profile dir, which is what the adapter's
    // first root names — and the store is its `state.db`.
    let db_path = out_dir.join("state.db");
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let mut conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    write_database(session, opts, &new_id, &mut conn)?;
    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("hermes --resume {new_id}")),
    })
}

/// One Hermes FTS5 index over `messages` and its three sync triggers, as `hermes_state.py`
/// spells them. The statement text is stored verbatim in the emitted `sqlite_master`, so the
/// literal's line breaks and indentation are part of the output.
fn fts_sql(table: &str, tokenize: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING fts5(content{tokenize});
         CREATE TRIGGER IF NOT EXISTS {table}_insert AFTER INSERT ON messages BEGIN
             INSERT INTO {table}(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS {table}_delete AFTER DELETE ON messages BEGIN
             DELETE FROM {table} WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS {table}_update AFTER UPDATE ON messages BEGIN
             DELETE FROM {table} WHERE rowid = old.id;
             INSERT INTO {table}(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;"
    )
}

fn write_database(session: &Session, opts: &EmitOptions, new_id: &str, conn: &mut rusqlite::Connection) -> Result<()> {
    use rusqlite::params;
    let resolver = session.resolver();
    let tx = conn.transaction()?;

    // Bootstrap the historical core, then reconcile optional columns like Hermes itself.
    // Appending to an older store must preserve the modern fields too.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT,
            cwd TEXT,
            git_branch TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
         );",
    )
    .context("creating hermes schema")?;
    reconcile_columns(&tx)?;

    // FTS5 virtual tables + sync triggers, mirroring hermes_state.py's FTS_SQL / FTS_TRIGRAM_SQL.
    // Without these, a port into a *fresh* state.db is invisible to Hermes's search (which queries
    // `messages_fts`). Created BEFORE inserting rows so the AFTER INSERT triggers populate the
    // index. The unicode61 table is part of bundled FTS5 and always available. The trigram table
    // (CJK/substring search) is guarded: if the trigram tokenizer isn't compiled in, skip it (and
    // its triggers) rather than failing the whole emit; the unicode61 table is enough for Hermes
    // to find ASCII content.
    tx.execute_batch(&fts_sql("messages_fts", ""))
        .context("creating hermes messages_fts (FTS5)")?;
    let trigram_sql = fts_sql("messages_fts_trigram", ", tokenize='trigram'");
    if let Err(e) = tx.execute_batch(&trigram_sql) {
        eprintln!("cv: hermes trigram FTS unavailable, skipping ({e}); unicode61 FTS still active");
    }

    let started = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .timestamp_millis() as f64
        / 1000.0;
    let ended = session
        .updated_at
        .or(session.created_at)
        .map(|t| t.timestamp_millis() as f64 / 1000.0);

    // Format-complete: pull the captured per-session columns back out of the Hermes fact bag
    // (`extra["hermes"][SESSION_META_KEY]`, IR-v2 nesting) so the round-trip reproduces every
    // session-row value. Anything absent falls back to a sensible default (`source = "clustervision"`).
    use crate::harness::hermes::{RAW_REASONING_CONTENT_KEY, RAW_REASONING_KEY, SESSION_META_KEY};
    let smeta = session
        .harness_extra(Harness::Hermes)
        .and_then(|b| b.get(SESSION_META_KEY))
        .and_then(Value::as_object);
    let smeta_str = |k: &str| -> Option<&str> { smeta.and_then(|m| m.get(k)).and_then(Value::as_str) };
    let smeta_int = |k: &str| -> i64 { smeta.and_then(|m| m.get(k)).and_then(Value::as_i64).unwrap_or(0) };
    let source = smeta_str("source").unwrap_or("clustervision");
    let user_id = smeta_str("user_id");
    let model_config = smeta_str("model_config");
    // System prompt: the captured Hermes column (same-harness replay) else the first-class
    // `Session::system_prompt` (a cross-harness port carries it here into Hermes's own slot).
    let system_prompt = smeta_str("system_prompt").or(session.system_prompt.as_deref());
    let end_reason = smeta_str("end_reason");
    // First-class cwd/git → Hermes's own `cwd`/`git_branch` columns (v-recent schema), so a ported
    // session keeps its working dir (the reader probes these columns).
    let cwd = effective_cwd(session, opts).map(|p| p.to_string_lossy().into_owned());
    let git_branch = branch_of(session);

    // An explicit id replaces its transcript as well as its session row.
    tx.execute("DELETE FROM messages WHERE session_id = ?1", [new_id])?;
    let mut session_insert = tx.prepare(
        "INSERT OR REPLACE INTO sessions \
         (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
          end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
          cache_read_tokens, cache_write_tokens, reasoning_tokens, title, cwd, git_branch, display_name, origin_json, title_source, \
          profile_name, transport_profile, git_repo_root, created_source, archived, auto_archived, hidden, pinned) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, \
                 ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30)",
    )?;
    session_insert
        .execute(params![
            new_id,
            source,
            user_id,
            session.model,
            model_config,
            system_prompt,
            started,
            ended,
            end_reason,
            0,
            smeta_int("tool_call_count"),
            smeta_int("input_tokens"),
            smeta_int("output_tokens"),
            smeta_int("cache_read_tokens"),
            smeta_int("cache_write_tokens"),
            smeta_int("reasoning_tokens"),
            session.title,
            cwd,
            git_branch,
            smeta_str("display_name"),
            smeta_str("origin_json"),
            smeta_str("title_source"),
            smeta_str("profile_name"),
            smeta_str("transport_profile"),
            smeta_str("git_repo_root"),
            smeta_str("created_source"),
            smeta_int("archived"),
            smeta_int("auto_archived"),
            smeta_int("hidden"),
            smeta_int("pinned"),
        ])
        .context("inserting hermes session")?;

    let mut message_insert = tx.prepare(
        "INSERT INTO messages \
        (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, reasoning, \
         reasoning_content, reasoning_details, codex_reasoning_items, codex_message_items, \
         platform_message_id, observed, tool_call_id, tool_name, active, compacted, _compressed_summary, \
         display_kind, api_content, display_metadata, effect_disposition, message_uid, tool_call_uid, \
         absorbed_message_uids, tool_call_uids) \
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
    )?;
    // cv synthesizes a boundary when reading a summary row. Persisting it (or a source's own
    // marker linked to its summary) as another row would duplicate it and change the native replay.
    let summary_parents: std::collections::HashSet<&str> = session
        .messages
        .iter()
        .filter(|m| m.kind == MessageKind::CompactionSummary)
        .filter_map(|m| m.parent_id.as_deref())
        .collect();
    let mut base_ts = started;
    for msg in &session.messages {
        if msg.kind == MessageKind::CompactionBoundary
            && (msg.is_synthesized() || msg.id.as_deref().is_some_and(|id| summary_parents.contains(id)))
        {
            continue;
        }
        let ts = msg
            .timestamp
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|| {
                base_ts += 0.001;
                base_ts
            });

        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };

        let projected = msg
            .content
            .iter()
            .map(|block| {
                crate::emit::thinking::fate(block, Harness::Hermes, opts.thinking, &resolver).map(|fate| (block, fate))
            })
            .collect::<Result<Vec<_>>>()?;

        // Reasoning columns. For a lossless round-trip we prefer the verbatim source columns the
        // parser stashed in the Hermes fact bag (`extra["hermes"]`, IR-v2 nesting: `reasoning` /
        // `reasoning_content` / `reasoning_details` / `codex_*`) over reconstructing from the folded
        // `Thinking.text` projection — that projection merges + dedups across columns and so can't
        // recover the originals. We synthesize from Thinking blocks only when no verbatim column was
        // captured (a session built in-memory or ported from another harness).
        let hbag = msg.harness_extra(Harness::Hermes);
        let extra_str = |k: &str| -> Option<&str> { hbag.and_then(|b| b.get(k)).and_then(Value::as_str) };
        let extra_json = |k: &str| -> Option<String> { hbag.and_then(|b| b.get(k)).map(|v| v.to_string()) };

        let finish_reason = extra_str("finish_reason");
        let platform_message_id = extra_str("platform_message_id");
        let observed: i64 = if hbag.and_then(|b| b.get("observed")) == Some(&Value::Bool(true)) {
            1
        } else {
            0
        };
        // token_count: the parser routes a row's single combined count to Usage (output for
        // assistant turns, input otherwise); reverse that mapping to recover the column.
        let token_count: Option<i64> = msg.usage.as_ref().and_then(|u| {
            let v = if msg.role == Role::Assistant {
                u.output_tokens
            } else {
                u.input_tokens
            };
            v.map(|n| n as i64)
        });

        // Raw native sidecars obey the same policy as the projected Thinking blocks.
        let keep_reasoning = opts.thinking != ThinkingMode::Drop;
        let mut reasoning = keep_reasoning
            .then(|| extra_str(RAW_REASONING_KEY))
            .flatten()
            .map(std::borrow::Cow::Borrowed);
        let reasoning_content = keep_reasoning.then(|| extra_str(RAW_REASONING_CONTENT_KEY)).flatten();
        // `reasoning_details` / codex items were stashed as parsed JSON; re-serialize them verbatim.
        let mut reasoning_details = keep_reasoning.then(|| extra_json("reasoning_details")).flatten();
        let codex_reasoning_items = keep_reasoning.then(|| extra_json("codex_reasoning_items")).flatten();
        let codex_message_items = extra_json("codex_message_items");

        // Fallback synthesis only when nothing verbatim was captured: a foreign/in-memory session
        // still gets its Thinking text + encrypted blob persisted (matching the prior behaviour).
        if reasoning.is_none() && reasoning_content.is_none() && reasoning_details.is_none() {
            let mut text = String::new();
            let mut enc: Option<&str> = None;
            for (b, fate) in &projected {
                if *fate != ThinkingFate::Keep {
                    continue;
                }
                if let Block::Thinking { text: t, encrypted, .. } = b {
                    let mut first = true;
                    t.try_for_each_chunk(&resolver, 64 * 1024, |chunk| {
                        if !chunk.is_empty() {
                            if first && !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(chunk);
                            first = false;
                        }
                        Ok(())
                    })?;
                    if enc.is_none() {
                        enc = encrypted.as_deref();
                    }
                }
            }
            if !text.is_empty() {
                reasoning = Some(std::borrow::Cow::Owned(text));
            }
            if let Some(enc) = enc {
                reasoning_details =
                    Some(json!([{ "type": "reasoning.encrypted_content", "encrypted_content": enc }]).to_string());
            }
        }

        // One prepared row shape keeps native metadata identical for ordinary and tool rows.
        let active = i64::from(hbag.and_then(|b| b.get("active")) != Some(&Value::Bool(false)));
        let compacted = i64::from(hbag.and_then(|b| b.get("compacted")) == Some(&Value::Bool(true)));
        let summary = i64::from(msg.kind == MessageKind::CompactionSummary);
        let display_metadata = extra_column(hbag, "display_metadata");
        let absorbed = extra_column(hbag, "absorbed_message_uids");
        let tool_uids = extra_column(hbag, "tool_call_uids");
        let mut insert = |body: Option<&str>, calls: Option<&str>, call_id: Option<&str>, tool_name: Option<&str>| {
            message_insert
                .execute(params![
                    new_id,
                    role,
                    body,
                    calls,
                    ts,
                    token_count,
                    finish_reason,
                    reasoning,
                    reasoning_content,
                    reasoning_details,
                    codex_reasoning_items,
                    codex_message_items,
                    platform_message_id,
                    observed,
                    call_id,
                    tool_name,
                    active,
                    compacted,
                    summary,
                    extra_str("display_kind"),
                    extra_str("api_content"),
                    display_metadata,
                    extra_str("effect_disposition"),
                    extra_str("message_uid"),
                    extra_str("tool_call_uid"),
                    absorbed,
                    tool_uids,
                ])
                .context("inserting hermes message")
        };
        if msg.role == Role::Tool {
            for block in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    ..
                } = block
                {
                    // SQLite TEXT binding requires contiguous bytes for this row only.
                    let mut body = String::new();
                    content.try_for_each_chunk(&resolver, 64 * 1024, |chunk| {
                        body.push_str(chunk);
                        Ok(())
                    })?;
                    insert(Some(&body), None, Some(tool_use_id), tool_name.as_deref())?;
                }
            }
        } else {
            let content = content(&projected, &resolver)?;
            let tool_calls = tool_calls(&msg.content)?;
            insert(content.as_deref(), tool_calls.as_deref(), None, None)?;
        }
    }

    drop(message_insert);
    drop(session_insert);
    // Hermes counts active native rows, excluding archived generations and synthetic IR markers.
    update_message_count(&tx, new_id)?;
    crate::harness::wire::write::check_source_fields(session)?;
    tx.commit()?;
    Ok(())
}

fn update_message_count(conn: &rusqlite::Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET message_count = \
        (SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND active = 1) WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Malformed JSON sidecars survive as literal strings. A string that is itself accepted JSON
/// must instead be JSON-quoted, or its next parse could change type (e.g. "null" → Null).
fn extra_column<'a>(bag: Option<&'a serde_json::Map<String, Value>>, key: &str) -> Option<std::borrow::Cow<'a, str>> {
    bag.and_then(|bag| bag.get(key)).map(|value| match value {
        Value::String(raw) if serde_json::from_str::<Value>(raw).is_err() => std::borrow::Cow::Borrowed(raw.as_str()),
        value => std::borrow::Cow::Owned(value.to_string()),
    })
}

fn reconcile_columns(conn: &rusqlite::Connection) -> Result<()> {
    use crate::harness::store::sqlite::columns;
    let sessions = columns(conn, "sessions");
    for (column, kind) in super::SESSION_META_TEXT_COLS
        .iter()
        .map(|c| (*c, "TEXT"))
        .chain(super::SESSION_META_INT_COLS.iter().map(|c| (*c, "INTEGER DEFAULT 0")))
        .chain([
            ("system_prompt", "TEXT"),
            ("parent_session_id", "TEXT"),
            ("end_reason", "TEXT"),
            ("cwd", "TEXT"),
            ("git_branch", "TEXT"),
        ])
    {
        if !sessions.contains(column) {
            conn.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {column} {kind}"))?;
        }
    }
    let messages = columns(conn, "messages");
    for column in super::OPTIONAL_MSG_COLS {
        if !messages.contains(*column) {
            let kind = match *column {
                "active" => "INTEGER DEFAULT 1",
                "compacted" | "_compressed_summary" | "observed" => "INTEGER DEFAULT 0",
                "token_count" => "INTEGER",
                _ => "TEXT",
            };
            conn.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {column} {kind}"))?;
        }
    }
    Ok(())
}

fn content(projected: &[(&Block, ThinkingFate)], resolver: &crate::lazy::Resolver) -> Result<Option<String>> {
    use crate::harness::wire::write::{text, ObjectWriter};
    if projected.iter().any(|(b, _)| matches!(b, Block::Image { .. })) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\0json:[");
        let mut first = true;
        for (block, fate) in projected {
            let value = if let Block::Text { text } = block {
                Some(std::borrow::Cow::Borrowed(text))
            } else {
                crate::emit::thinking::text_with_fate(block, *fate)
            };
            if let Some(value) = value {
                if !first {
                    bytes.push(b',');
                }
                first = false;
                let mut part = ObjectWriter::new(&mut bytes)?;
                part.field("type", "text")?;
                part.field_with("text", |w| text(w, &value, resolver))?;
                part.finish()?;
            } else if let Block::Image { data_ref, .. } = block {
                if !first {
                    bytes.push(b',');
                }
                first = false;
                serde_json::to_writer(
                    &mut bytes,
                    &json!({"type":"image_url","image_url":{"url":data_ref.as_deref().unwrap_or_default()}}),
                )?;
            }
        }
        bytes.push(b']');
        Ok(Some(String::from_utf8(bytes)?))
    } else {
        let mut body = String::new();
        for (block, fate) in projected {
            let value = if let Block::Text { text } = block {
                Some(std::borrow::Cow::Borrowed(text))
            } else {
                crate::emit::thinking::text_with_fate(block, *fate)
            };
            if let Some(value) = value {
                if !body.is_empty() {
                    body.push('\n');
                }
                value.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
                    body.push_str(chunk);
                    Ok(())
                })?;
            }
        }
        Ok((!body.is_empty()).then_some(body))
    }
}

fn tool_calls(blocks: &[Block]) -> Result<Option<String>> {
    use crate::harness::wire::write::ObjectWriter;
    let mut bytes = vec![b'['];
    let mut first = true;
    for block in blocks {
        let rewrapped = crate::affordance::rewrap_deferred(super::DEFERRED_WRAPPER, block)?;
        let block = rewrapped.as_ref().unwrap_or(block);
        if let Block::ToolUse { id, name, input, .. } = block {
            if !first {
                bytes.push(b',');
            }
            first = false;
            let mut call = ObjectWriter::new(&mut bytes)?;
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
    }
    bytes.push(b']');
    Ok((!first).then(|| String::from_utf8(bytes)).transpose()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parsed(connection: &rusqlite::Connection, options: &crate::stream::Demand) -> Result<Session> {
        super::super::parse_conn_with(
            connection,
            &crate::SessionRef::new("fixed", Harness::Hermes, connection.path().expect("file store")),
            options,
        )
    }

    fn message_values(session: &Session) -> Value {
        Value::Array(
            session
                .messages
                .iter()
                .map(|m| {
                    json!({
                        "role":m.role,"kind":m.kind,"origin":m.origin,"content":m.content,
                        "timestamp":m.timestamp,"usage":m.usage,"extra":m.extra
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn modern_native_exports_preserve_display_compaction_identity_and_sparse_metadata() -> Result<()> {
        let store = cv_testkit::temp_dir("hermes-export-source");
        let source = rusqlite::Connection::open(store.join("state.db"))?;
        source.execute_batch(include_str!("../../../tests/fixtures/hermes/schema-v30.sql"))?;
        reconcile_columns(&source)?;
        source.execute_batch(
            r#"
            INSERT INTO sessions(id,source,started_at,title,display_name,origin_json,title_source,
                profile_name,transport_profile,git_repo_root,created_source,archived,auto_archived,hidden,pinned)
            VALUES('fixed','cli',1000,'Native','Label','{"imported_from":{"tool":"codex"}}',
                'manual','profile','transport','/repo','oneshot',1,1,1,1);
            INSERT INTO messages(session_id,role,content,timestamp,active,message_uid)
                VALUES('fixed','user','rewound',1001,0,'u0');
            INSERT INTO messages(session_id,role,content,timestamp,active,compacted,message_uid)
                VALUES('fixed','assistant','archived reply',1002,0,1,'a0');
            INSERT INTO messages(session_id,role,content,timestamp,api_content,display_metadata,
                message_uid,absorbed_message_uids)
                VALUES('fixed','user','prompt',1003,'provider prompt','{malformed','u1','["u0"]');
            INSERT INTO messages(session_id,role,content,timestamp,display_metadata)
                VALUES('fixed','user','string metadata',1003.5,'"null"');
            INSERT INTO messages(session_id,role,content,timestamp,_compressed_summary)
                VALUES('fixed','user','summary',1004,1);
            INSERT INTO messages(session_id,role,content,timestamp,tool_call_id,tool_name,
                effect_disposition,message_uid,tool_call_uid,reasoning,reasoning_content,
                reasoning_details,codex_reasoning_items,codex_message_items,observed,token_count)
                VALUES('fixed','tool','result',1005,'call','terminal','effect','t1','occurrence',
                    'private summary','private scratchpad','[{"type":"thinking","text":"details"}]',
                    '[{"encrypted_content":"private encrypted"}]','[{"type":"message"}]',1,17);
            INSERT INTO messages(session_id,role,content,timestamp,display_kind,display_metadata,tool_call_uids)
                VALUES('fixed','user','model changed',1006,'model_switch','{"model":"next"}',
                    '{"call":"occurrence"}');
        "#,
        )?;
        let complete = crate::stream::Demand::COMPLETE;
        let session = parsed(&source, &complete)?;
        assert_eq!(
            session.messages.len(),
            8,
            "one synthetic boundary accompanies the seven native rows"
        );
        let world = crate::test_world("hermes-modern-export");
        for (name, old_schema) in [("fresh", false), ("historical", true)] {
            let directory = world.base.join(name);
            std::fs::create_dir_all(&directory)?;
            let path = directory.join("state.db");
            if old_schema {
                let db = rusqlite::Connection::open(&path)?;
                db.execute_batch(include_str!("../../../tests/fixtures/hermes/schema-old.sql"))?;
                db.execute_batch(
                    "INSERT INTO sessions(id,source,started_at) VALUES('other','cli',1); \
                    INSERT INTO messages(session_id,role,content,timestamp) VALUES('other','user','untouched',1);",
                )?;
            }
            let options = EmitOptions {
                new_id: Some("fixed".into()),
                ..Default::default()
            };
            emit(&session, &directory, &options)?;
            // Replacing a requested id must replace its rows, rather than accumulating copies.
            emit(&session, &directory, &options)?;
            let output = rusqlite::Connection::open(&path)?;
            let replay = parsed(&output, &complete)?;
            assert_eq!(message_values(&replay), message_values(&session), "{name}");
            assert_eq!(replay.extra, session.extra, "{name}");
            let count: i64 =
                output.query_row("SELECT message_count FROM sessions WHERE id='fixed'", [], |r| r.get(0))?;
            assert_eq!(count, 5, "Hermes's native counter includes only active rows");
            let native_rows: i64 =
                output.query_row("SELECT COUNT(*) FROM messages WHERE session_id='fixed'", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(native_rows, 7);
            let display = parsed(&output, &crate::stream::Demand::RICH)?;
            assert_eq!(
                message_values(&display),
                message_values(&parsed(&source, &crate::stream::Demand::RICH)?)
            );
            if old_schema {
                let other: String =
                    output.query_row("SELECT content FROM messages WHERE session_id='other'", [], |r| {
                        r.get(0)
                    })?;
                assert_eq!(other, "untouched");
            }
        }
        Ok(())
    }

    #[test]
    fn thinking_drop_removes_native_sidecars_on_assistant_and_tool_rows() -> Result<()> {
        let mut session = Session::new("source", Harness::Hermes);
        for role in [Role::Assistant, Role::Tool] {
            let mut message = crate::Message::new(role);
            message.content.push(Block::Thinking {
                text: "private".into(),
                signature: None,
                encrypted: Some("private encrypted".into()),
                redacted: false,
            });
            if role == Role::Tool {
                message.content.push(Block::ToolResult {
                    tool_use_id: "call".into(),
                    content: "public result".into(),
                    is_error: false,
                    tool_name: Some("terminal".into()),
                    status: None,
                    details: None,
                });
            } else {
                message.content.push(Block::Text {
                    text: "public answer".into(),
                });
            }
            *message.harness_extra_mut(Harness::Hermes) = serde_json::from_value(json!({
                "reasoning":"private summary","reasoning_content":"private scratchpad",
                "reasoning_details":[{"type":"thinking","text":"private details"}],
                "codex_reasoning_items":[{"encrypted_content":"private encrypted"}],
                "codex_message_items":[{"type":"message"}]
            }))?;
            session.messages.push(message);
        }
        let store = crate::test_world("hermes-thinking-drop");
        let mut output = rusqlite::Connection::open(store.base.join("state.db"))?;
        write_database(
            &session,
            &EmitOptions {
                thinking: ThinkingMode::Drop,
                ..Default::default()
            },
            "fixed",
            &mut output,
        )?;
        let retained: i64 = output.query_row(
            "SELECT COUNT(*) FROM messages WHERE reasoning IS NOT NULL \
            OR reasoning_content IS NOT NULL OR reasoning_details IS NOT NULL OR codex_reasoning_items IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(retained, 0);
        let replay = parsed(&output, &crate::stream::Demand::RICH)?;
        assert_eq!(replay.messages.len(), 2);
        assert!(replay
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .all(|b| !matches!(b, Block::Thinking { .. })));
        assert!(
            matches!(&replay.messages[0].content[0], Block::Text { text } if text.matches_str("public answer") == Some(true))
        );
        assert!(replay.messages.iter().all(|m| m
            .harness_extra(Harness::Hermes)
            .unwrap()
            .contains_key("codex_message_items")));
        Ok(())
    }

    #[test]
    fn checked_transactional_rows_match_frozen_and_roll_back() -> Result<()> {
        crate::harness::store::sqlite::export_tests::exercise_frozen(
            Harness::Hermes,
            "hermes-export",
            emit,
            |session, options, connection| write_database(session, options, "fixed", connection),
        )
    }

    #[test]
    fn hermes_preserves_long_data_uri_images_without_truncation() -> Result<()> {
        use crate::harness::{hermes::Hermes, Adapter, Parse};
        use crate::ir::Message;
        let world = crate::test_world("hermes-long-data-uri");
        let long_b64 = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=".repeat(12);
        let data_uri = format!("data:image/png;base64,{long_b64}");
        assert!(data_uri.len() > 400);

        let mut s = Session::new("src", Harness::Hermes);
        let mut u = Message::new(Role::User);
        u.content = vec![
            Block::Text {
                text: "check diagram".into(),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some(data_uri.clone()),
            },
        ];
        s.messages.push(u);

        let res = emit(
            &s,
            &world.base,
            &EmitOptions {
                new_id: Some("img-hermes".into()),
                ..Default::default()
            },
        )?;
        let hermes = Hermes::with_db(res.path);
        let refs = hermes.discover()?;
        let parsed_first = hermes.parse(&refs[0])?;
        assert!(parsed_first.messages[0].content.iter().any(|b| matches!(
            b,
            Block::Image { data_ref: Some(r), .. } if *r == data_uri && !r.contains('…')
        )));

        // Re-emitting a parsed Hermes session must keep the full data URI in SQLite too.
        let out2 = world.base.join("rehome");
        let res2 = emit(
            &parsed_first,
            &out2,
            &EmitOptions {
                new_id: Some("img-hermes-2".into()),
                ..Default::default()
            },
        )?;
        let hermes2 = Hermes::with_db(res2.path);
        let parsed_second = hermes2.parse(&hermes2.discover()?[0])?;
        assert!(parsed_second.messages[0].content.iter().any(|b| matches!(
            b,
            Block::Image { data_ref: Some(r), .. } if *r == data_uri
        )));
        Ok(())
    }
}
