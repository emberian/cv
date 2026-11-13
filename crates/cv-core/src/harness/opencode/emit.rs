//! The OpenCode emitter: an IR [`Session`] written into OpenCode's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::Session;
use anyhow::Result;
use std::path::Path;
#[cfg(feature = "sqlite")]
use {
    crate::ir::{Block, Role},
    anyhow::Context,
    serde_json::{json, Map, Value},
    std::fs,
    std::path::PathBuf,
    uuid::Uuid,
};

#[cfg(feature = "sqlite")]
/// Where the `opencode.db` should live for a given `--out`/storage-root `out_dir`: the db file
/// itself if pointed at one, the data-dir sibling of a `storage/` root (the real install layout —
/// `~/.local/share/opencode/opencode.db` next to `storage/`), else `<out_dir>/opencode.db`.
fn opencode_db_path(out_dir: &Path) -> PathBuf {
    if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else if out_dir.file_name().is_some_and(|n| n == "storage") {
        out_dir
            .parent()
            .map(|p| p.join("opencode.db"))
            .unwrap_or_else(|| out_dir.join("opencode.db"))
    } else {
        out_dir.join("opencode.db")
    }
}

/// Export to OpenCode's canonical SQLite store. Existing v1 stores retain their
/// `session`/`message`/`part` layout; existing v2 and fresh stores use native 2.0.25
/// `session_v2`/`session_message`. Fresh bootstrap and row publication are atomic.
/// Tool results fold onto assistant tools; native v2 uses content[] and structured errors.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (session, out_dir, opts);
        anyhow::bail!("emit to opencode requires the `sqlite` feature (opencode.db is SQLite)");
    }
    #[cfg(feature = "sqlite")]
    {
        opts.validate_ids()?;
        // Reject unreadable sources before creating output; each store also guards its commit.
        crate::harness::wire::write::check_source_fields(session)?;
        emit_opencode_db(session, out_dir, opts)
    }
}

#[cfg(feature = "sqlite")]
fn emit_opencode_db(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::Connection;

    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| format!("ses_{}", Uuid::now_v7().simple()));
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_id = if cwd_str.is_empty() {
        "prj_global".to_string()
    } else {
        format!("prj_{}", short_hash(&cwd_str))
    };

    let db_path = opencode_db_path(out_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let empty: bool = conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    let v2: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_v2')",
        [],
        |row| row.get(0),
    )?;
    if v2 || empty {
        // Empty is the store's unknown-directory sentinel, as on the v1 path.
        v2::write_database(session, opts, &new_id, &cwd_str, &mut conn).context(
            "exporting native OpenCode 2.x store (requires complete session_v2/session_message/project schema)",
        )?;
    } else {
        let v1: bool = conn.query_row(
            "SELECT count(*)=3 FROM sqlite_master WHERE type='table' AND name IN ('session','message','part')",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            v1,
            "unsupported partial OpenCode database; initialize an empty store with OpenCode before exporting"
        );
        write_database(session, opts, &new_id, &cwd_str, &project_id, &mut conn)?;
    }
    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("opencode --session {new_id}")),
    })
}

#[cfg(feature = "sqlite")]
fn write_database(
    session: &Session,
    opts: &EmitOptions,
    new_id: &str,
    cwd_str: &str,
    project_id: &str,
    conn: &mut rusqlite::Connection,
) -> Result<()> {
    use rusqlite::params;
    let resolver = session.resolver();
    let tx = conn.transaction()?;
    // FK enforcement stays off (rusqlite's default) so a fresh db needs no `project` row and an
    // append into a real store isn't rejected for the synthetic project_id. Create the reader's
    // schema when absent (`IF NOT EXISTS` no-ops against a real store; our INSERTs name only columns
    // both schemas share, so any extra real columns keep their defaults).
    // Match native lookup indexes even in older cv-created stores. Without these,
    // reading M messages with P global parts performs M whole-part-table scans.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            model TEXT,
            cost REAL NOT NULL DEFAULT 0,
            tokens_input INTEGER NOT NULL DEFAULT 0,
            tokens_output INTEGER NOT NULL DEFAULT 0,
            tokens_reasoning INTEGER NOT NULL DEFAULT 0,
            tokens_cache_read INTEGER NOT NULL DEFAULT 0,
            tokens_cache_write INTEGER NOT NULL DEFAULT 0,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS message_session_time_created_id_idx
             ON message (session_id, time_created, id);
         CREATE INDEX IF NOT EXISTS part_message_id_id_idx ON part (message_id, id);",
    )
    .context("creating opencode schema")?;

    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));

    // Session row. `model` is `{id, providerID}` (the reader reads `model.id`); `title` is NOT NULL.
    let model_json = session
        .model
        .as_deref()
        .map(|m| json!({ "id": m, "providerID": "clustervision" }).to_string());
    let mut session_insert = tx.prepare(
        "INSERT INTO session \
         (id, project_id, slug, directory, title, version, model, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    session_insert
        .execute(params![
            new_id,
            project_id,
            short_hash(new_id),
            cwd_str,
            session.title.as_deref().unwrap_or_default(),
            env!("CARGO_PKG_VERSION"),
            model_json,
            created,
            updated,
        ])
        .context("inserting opencode session")?;

    // Pre-index tool results by tool_use_id so each folds onto its assistant's `tool` part.
    let mut tool_results: std::collections::HashMap<&str, &Block> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult { tool_use_id, .. } = b {
                    tool_results.insert(tool_use_id.as_str(), b);
                }
            }
        }
    }

    let called = called_tool_ids(session);

    let mut message_insert = tx.prepare(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut part_insert = tx.prepare("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
    let base = created;
    let mut seq: u64 = 0;
    for msg in &session.messages {
        // Tool turns are folded into the assistant's tool parts; no standalone message. The
        // exception is a turn carrying an ORPHAN result ([`called_tool_ids`]): nothing folds it, so
        // it rides in a message of its own or it is lost outright.
        if msg.role == Role::Tool && !msg.content.iter().any(|b| is_orphan_result(b, &called)) {
            continue;
        }
        let role = match msg.role {
            Role::Assistant | Role::Tool => "assistant",
            Role::System => "system",
            _ => "user",
        };
        // Monotonic id + time so the reader's `ORDER BY time_created, id` == emission order.
        let mid = format!("msg_{seq:08}_{}", Uuid::now_v7().simple());
        let mts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(base + seq as i64);

        let mut mdata = Map::new();
        mdata.insert("role".into(), json!(role));
        mdata.insert("time".into(), json!({ "created": mts }));
        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
            mdata.insert("modelID".into(), json!(model));
        }
        // Usage → `tokens` + `cost` (the reader's `parse_usage` reads both off the message record).
        if let Some(u) = &msg.usage {
            let mut tokens = Map::new();
            tokens.insert("input".into(), json!(u.input_tokens.unwrap_or(0)));
            tokens.insert("output".into(), json!(u.output_tokens.unwrap_or(0)));
            tokens.insert("reasoning".into(), json!(u.reasoning_tokens.unwrap_or(0)));
            tokens.insert(
                "cache".into(),
                json!({ "read": u.cache_read_tokens.unwrap_or(0), "write": u.cache_creation_tokens.unwrap_or(0) }),
            );
            mdata.insert("tokens".into(), Value::Object(tokens));
            if let Some(c) = u.cost_usd {
                mdata.insert("cost".into(), json!(c));
            }
        }
        message_insert
            .execute(params![mid, new_id, mts, mts, Value::Object(mdata).to_string()])
            .context("inserting opencode message")?;

        let mut pidx: u64 = 0;
        let mut write_part = |part: String| -> Result<()> {
            let pid = format!("prt_{seq:08}_{pidx:04}_{}", Uuid::now_v7().simple());
            part_insert
                .execute(params![pid, mid, new_id, mts, mts, part])
                .context("inserting opencode part")?;
            pidx += 1;
            Ok(())
        };

        for b in &msg.content {
            let fate = crate::emit::thinking::fate(b, crate::Harness::OpenCode, opts.thinking, &resolver)?;
            if fate == ThinkingFate::Drop {
                continue;
            }
            let mut bytes = Vec::new();
            let mut part = crate::harness::wire::write::ObjectWriter::new(&mut bytes)?;
            match b {
                Block::Text { text } => {
                    part.field("type", "text")?;
                    part.field_with("text", |w| crate::harness::wire::write::text(w, text, &resolver))?;
                }
                Block::Thinking { text, signature, .. } if fate == ThinkingFate::Keep => {
                    part.field("type", "reasoning")?;
                    part.field_with("text", |w| crate::harness::wire::write::text(w, text, &resolver))?;
                    if let Some(sig) = signature {
                        part.field("metadata", json!({"anthropic":{"signature":sig}}))?;
                    }
                }
                Block::Thinking { .. } => {
                    let text = crate::emit::thinking::text_with_fate(b, fate).unwrap();
                    part.field("type", "text")?;
                    part.field_with("text", |w| crate::harness::wire::write::text(w, &text, &resolver))?;
                }
                Block::ToolUse { id, name, input, .. } => {
                    part.field("type", "tool")?;
                    part.field("callID", id)?;
                    part.field("tool", name)?;
                    part.field_with("state", |w| {
                        tool_state(w, Some(input), tool_results.get(id.as_str()).copied(), &resolver)
                    })?;
                }
                Block::Image { media_type, data_ref } => {
                    part.field("type", "file")?;
                    if let Some(mt) = media_type {
                        part.field("mime", mt)?;
                    }
                    if let Some(url) = data_ref {
                        part.field("url", url)?;
                    }
                }
                Block::File { mime, path, source } => {
                    part.field("type", "file")?;
                    if let Some(mt) = mime {
                        part.field("mime", mt)?;
                    }
                    if let Some(url) = source.as_deref().or(path.as_deref()) {
                        part.field("url", url)?;
                    }
                }
                Block::ToolResult {
                    tool_use_id, tool_name, ..
                } if !called.contains(tool_use_id.as_str()) => {
                    part.field("type", "tool")?;
                    part.field("callID", tool_use_id)?;
                    part.field("tool", tool_name.as_deref().unwrap_or_default())?;
                    part.field_with("state", |w| tool_state(w, None, Some(b), &resolver))?;
                }
                Block::ToolResult { .. } => continue,
            }
            part.finish()?;
            write_part(String::from_utf8(bytes)?)?;
        }
        seq += 1;
    }

    drop(part_insert);
    drop(message_insert);
    drop(session_insert);
    // Revalidate after source reads and before committing any staged database changes.
    crate::harness::wire::write::check_source_fields(session)?;
    tx.commit()?;
    Ok(())
}

/// The `state` object of an OpenCode `tool` part: the call's `input`, plus — when the session
/// paired one with it — the result's status, output-or-error and structured `details`.
/// This v1 layout hoists title/time into state and folds other details into metadata;
/// `emit/v2.rs::tool_state` uses the v2 details envelope instead.
///
/// `input` is `None` for an **orphan** result (see [`emit_opencode_db`]): the call itself is not in
/// this session, so there are no arguments to record, only what came back.
#[cfg(feature = "sqlite")]
fn tool_state(
    writer: &mut dyn std::io::Write,
    input: Option<&crate::Json>,
    result: Option<&Block>,
    resolver: &crate::lazy::Resolver,
) -> Result<()> {
    use crate::harness::wire::write::{text, Object, ObjectWriter};
    let mut state = ObjectWriter::new(writer)?;
    if let Some(input) = input {
        state.field_with("input", |mut w| input.write_canonical(&mut w))?;
    }
    let Some(Block::ToolResult {
        content,
        is_error,
        details,
        ..
    }) = result
    else {
        state.field("status", "completed")?;
        return state.finish();
    };
    state.field("status", if *is_error { "error" } else { "completed" })?;
    state.field_with(if *is_error { "error" } else { "output" }, |w| {
        text(w, content, resolver)
    })?;
    if let Some(details) = details {
        let empty = Map::new();
        let initial = details.get("metadata").and_then(Value::as_object).unwrap_or(&empty);
        let mut meta = Object::new(initial);
        let mut present = !initial.is_empty();
        if let Value::Object(fields) = details {
            for (key, value) in fields {
                match key.as_str() {
                    "title" | "time" => state.field(key, value)?,
                    "metadata" if value.is_object() => {}
                    _ => {
                        meta.set(key, value);
                        present = true;
                    }
                }
            }
        } else {
            meta.set("details", details);
            present = true;
        }
        if present {
            state.field_with("metadata", |w| meta.write(w, |_, _, _| Ok(false)))?;
        }
    }
    state.finish()
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    #[test]
    fn unknown_cwd_stays_empty_and_complete_verification_is_declared() -> Result<()> {
        let world = crate::test_world("opencode-unknown-cwd");
        let mut session = Session::new("source", crate::Harness::Claude);
        let mut message = crate::Message::new(Role::User);
        message.content.push(Block::Text { text: "hello".into() });
        session.messages.push(message);
        let (result, report) = crate::emit::emit_report(
            &session,
            crate::Harness::OpenCode,
            &world.base.join("out"),
            &EmitOptions {
                parsed_at: ParsedAt::Complete,
                ..Default::default()
            },
        )?;
        let conn = rusqlite::Connection::open(&result.path)?;
        let cwd: String = conn.query_row("SELECT directory FROM session_v2 WHERE id=?1", [&result.new_id], |r| {
            r.get(0)
        })?;
        assert!(cwd.is_empty());
        assert!(report
            .deltas
            .iter()
            .any(|d| d.what == "verification demand" && d.expected));
        Ok(())
    }

    #[test]
    fn exported_v1_store_uses_lookup_indexes_with_unrelated_rows() -> Result<()> {
        let mut connection = rusqlite::Connection::open_in_memory()?;
        let session = Session::new("source", crate::Harness::Claude);
        write_database(
            &session,
            &EmitOptions::default(),
            "selected",
            "/fixture",
            "project",
            &mut connection,
        )?;
        for count in [1, 16, 256] {
            for index in 0..count {
                let id = format!("unrelated-{count}-{index}");
                connection.execute("INSERT INTO message VALUES(?1,'unrelated',1,1,'{}')", [&id])?;
                connection.execute("INSERT INTO part VALUES(?1,?1,'unrelated',1,1,'{}')", [&id])?;
            }
            for (sql,index) in [
                ("EXPLAIN QUERY PLAN SELECT id,rowid,typeof(data) FROM message WHERE session_id='selected' ORDER BY time_created,id","message_session_time_created_id_idx"),
                ("EXPLAIN QUERY PLAN SELECT rowid,id,typeof(data) FROM part WHERE message_id='selected-message' ORDER BY id","part_message_id_id_idx"),
            ] {
                let details: Vec<String> = connection.prepare(sql)?.query_map([],|row|row.get(3))?.collect::<rusqlite::Result<_>>()?;
                assert!(details.iter().any(|detail|detail.contains("SEARCH")&&detail.contains(index)),"unrelated rows {count}: {details:?}");
                assert!(!details.iter().any(|detail|detail.contains("SCAN")||detail.contains("TEMP B-TREE")),"lookup must not scan/sort unrelated rows: {details:?}");
            }
        }
        Ok(())
    }

    #[test]
    fn reused_v1_session_id_rejects_without_altering_history() -> Result<()> {
        let mut connection = rusqlite::Connection::open_in_memory()?;
        let mut session = Session::new("original", crate::Harness::Claude);
        let mut message = crate::Message::new(Role::User);
        message.content.push(Block::Text {
            text: "original body".into(),
        });
        session.messages.push(message);
        let options = EmitOptions::default();
        write_database(&session, &options, "fixed", "/fixture", "project", &mut connection)?;
        let before = crate::harness::store::sqlite::export_tests::logical(&connection, crate::Harness::OpenCode)?;
        session.title = Some("replacement".into());
        assert!(write_database(&session, &options, "fixed", "/changed", "project", &mut connection).is_err());
        assert_eq!(
            before,
            crate::harness::store::sqlite::export_tests::logical(&connection, crate::Harness::OpenCode)?
        );
        Ok(())
    }

    #[test]
    fn checked_transactional_rows_match_frozen_and_roll_back() -> Result<()> {
        crate::harness::store::sqlite::export_tests::exercise_frozen(
            crate::Harness::OpenCode,
            "opencode-export",
            emit,
            |session, options, connection| {
                write_database(
                    session,
                    options,
                    options.new_id.as_deref().unwrap_or("fixed"),
                    "/synthetic/sqlite",
                    &format!("prj_{}", short_hash("/synthetic/sqlite")),
                    connection,
                )
            },
        )
    }
}

#[cfg(feature = "sqlite")]
mod v2;

#[cfg(all(test, feature = "sqlite"))]
mod complete_tests;
