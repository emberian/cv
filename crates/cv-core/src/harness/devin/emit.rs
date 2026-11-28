//! The Devin CLI emitter: an IR [`Session`] written into Devin CLI's `sessions.db` SQLite store
//! (refinery schema 17, `sessions` + `message_nodes` main-chain forest).
//!
//! Registered in `crate::emit::emitter_for`. Supports same-transaction read-back verification via
//! [`super::stream_conn_verified`] so a `--strict` failure or source-provenance error rolls back
//! cleanly without leaving partial rows or empty databases behind.

use crate::emit::*;
use crate::harness::store::sqlite::table_exists;
use crate::harness::store::xdg_data_dir;
use crate::harness::{resume_in, shell_quote, EmitResult};
use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Schema 17 migration history rows matching Devin CLI 3000.11.3 (`refinery_schema_history`).
const SCHEMA_17_MIGRATIONS: &[(i32, &str)] = &[
    (1, "initial_schema"),
    (2, "add_thinking_column"),
    (3, "add_prompt_history"),
    (4, "add_metadata_column"),
    (5, "message_forest"),
    (6, "add_node_metadata"),
    (7, "add_shell_context"),
    (8, "add_session_cogs"),
    (9, "add_rendered_commits"),
    (10, "add_workspace_dirs"),
    (11, "add_prompt_history_is_shell"),
    (12, "add_app_state"),
    (13, "rename_permission_mode_to_agent_mode"),
    (14, "tool_call_state"),
    (15, "add_hidden_column"),
    (16, "add_session_json_metadata"),
    (17, "subagent_heads"),
];

/// Resolve the target SQLite database path:
/// - `Some(p)` where `p` is an existing file or ends in `.db` / `.sqlite`: `p` directly.
/// - `Some(p)` otherwise: `<p>/sessions.db`.
/// - `None`: `<xdg_data_dir>/devin/cli/sessions.db`.
pub fn resolve_db_path(out: Option<&Path>) -> Result<PathBuf> {
    match out.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => {
            let is_db_file = p.is_file()
                || p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("db") || e.eq_ignore_ascii_case("sqlite"));
            if is_db_file {
                Ok(p.to_path_buf())
            } else {
                Ok(p.join("sessions.db"))
            }
        }
        None => xdg_data_dir()
            .map(|d| d.join("devin").join("cli").join("sessions.db"))
            .context("could not resolve XDG data directory for Devin CLI sessions.db"),
    }
}

/// Build the `devin -r <id>` resume hint, adding `XDG_DATA_HOME=<dir>` when `db_path` sits under
/// an alternate `<dir>/devin/cli/sessions.db` outside the active XDG data directory.
fn build_resume_hint(db_path: &Path, new_id: &str, cwd: Option<&Path>) -> String {
    let base_cmd = format!("devin -r {}", shell_quote(new_id));
    let alt_xdg = super::xdg_root_for_db(db_path).filter(|target_xdg| {
        let active = xdg_data_dir();
        active.as_deref().is_none_or(|act| !same_dir(act, target_xdg))
    });
    let cmd = match alt_xdg {
        Some(xdg) => format!("XDG_DATA_HOME={} {base_cmd}", shell_quote(&xdg.display().to_string())),
        None => base_cmd,
    };
    resume_in(cwd, cmd)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Emit `session` into Devin CLI's `sessions.db` SQLite store.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    emit_staged(session, out_dir, opts, None)
}

pub(crate) fn emit_staged(
    session: &Session,
    out_dir: &Path,
    opts: &EmitOptions,
    verify: Option<crate::emit::stage::VerifyFn<'_>>,
) -> Result<EmitResult> {
    opts.validate_ids()?;
    crate::harness::wire::write::check_source_fields(session)?;

    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let db_path = resolve_db_path((!out_dir.as_os_str().is_empty()).then_some(out_dir))?;
    let cwd = effective_cwd(session, opts);

    let mut guard = crate::emit::stage::SqliteRollbackGuard::prepare(&db_path)?;
    let result = EmitResult {
        path: db_path.clone(),
        new_id: new_id.clone(),
        resume_hint: Some(build_resume_hint(&db_path, &new_id, cwd.as_deref())),
        ..Default::default()
    };

    let mut conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    write_database_with_verify(
        session,
        opts,
        &new_id,
        cwd.as_deref(),
        &mut conn,
        Some((&result, verify)),
    )?;
    guard.disarm();
    Ok(result)
}

/// Make the target a schema-17 store, or refuse it.
///
/// A new database gets the schema and its migration history. An existing Devin store is written
/// as it stands and only at schema 17 or later: adding the columns or tables of a later migration
/// to an older store, without the history rows that record them, makes Devin's own migration fail
/// on the duplicate the next time it starts. A newer store whose columns this insert does not
/// satisfy fails the insert and rolls back.
fn ensure_schema_17(tx: &Connection) -> Result<()> {
    let has_user_tables: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%')",
        [],
        |r| r.get(0),
    )?;
    if has_user_tables {
        let version: Option<i64> = if table_exists(tx, "refinery_schema_history") {
            tx.query_row("SELECT MAX(version) FROM refinery_schema_history", [], |r| r.get(0))?
        } else {
            None
        };
        let newest = SCHEMA_17_MIGRATIONS.len() as i64;
        match version {
            Some(v) if v >= newest => return Ok(()),
            Some(v) => anyhow::bail!(
                "Devin store is at schema {v}, before {newest}: start Devin once so it migrates the store, \
                 or pass --out for a separate one"
            ),
            None => anyhow::bail!("not a Devin store (no migration history): pass --out for a separate one"),
        }
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS refinery_schema_history(
             version int4 PRIMARY KEY,
             name VARCHAR(255),
             applied_on VARCHAR(255),
             checksum VARCHAR(255));
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             working_directory TEXT NOT NULL,
             backend_type TEXT NOT NULL,
             model TEXT NOT NULL,
             agent_mode TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             last_activity_at INTEGER NOT NULL,
             title TEXT,
             main_chain_id INTEGER,
             shell_last_seen_index INTEGER DEFAULT 0,
             cogs_json TEXT,
             workspace_dirs TEXT,
             hidden INTEGER NOT NULL DEFAULT 0,
             metadata TEXT);
         CREATE TABLE IF NOT EXISTS prompt_history (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             content TEXT NOT NULL,
             timestamp INTEGER NOT NULL,
             session_id TEXT NOT NULL,
             is_shell INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS message_nodes (
             row_id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL,
             node_id INTEGER NOT NULL,
             parent_node_id INTEGER,
             chat_message TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             metadata TEXT,
             FOREIGN KEY (session_id) REFERENCES sessions(id),
             UNIQUE(session_id, node_id));
         CREATE INDEX IF NOT EXISTS idx_message_nodes_session_node
             ON message_nodes (session_id, node_id);
         CREATE TABLE IF NOT EXISTS rendered_commits (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL,
             sequence_number INTEGER NOT NULL,
             rendered_html TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             FOREIGN KEY (session_id) REFERENCES sessions(id),
             UNIQUE(session_id, sequence_number));
         CREATE TABLE IF NOT EXISTS app_state (
             key TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS tool_call_state (
             session_id TEXT NOT NULL,
             tool_call_id TEXT NOT NULL,
             tool_call_json TEXT,
             tool_call_update_json TEXT,
             PRIMARY KEY (session_id, tool_call_id),
             FOREIGN KEY (session_id) REFERENCES sessions(id));
         CREATE TABLE IF NOT EXISTS subagent_heads (
             session_id TEXT NOT NULL,
             agent_id TEXT NOT NULL,
             chain_node_id INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, agent_id),
             FOREIGN KEY (session_id) REFERENCES sessions(id));",
    )
    .context("initializing devin schema 17")?;

    let applied = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut ins = tx
        .prepare("INSERT INTO refinery_schema_history (version, name, applied_on, checksum) VALUES (?1, ?2, ?3, ?4)")?;
    for &(ver, name) in SCHEMA_17_MIGRATIONS {
        ins.execute(params![ver, name, &applied, ver.to_string()])?;
    }
    Ok(())
}

struct PendingNode {
    chat_message: String,
    created_at: i64,
    metadata: String,
}

#[derive(Default, Clone)]
struct BoundaryFacts {
    summarized_from: Option<i64>,
    pre_tokens: Option<u64>,
    source: Option<Value>,
    edited_files: Option<Value>,
    todo_list: Option<Value>,
}

fn extract_boundary_facts(msg: &Message) -> BoundaryFacts {
    let mut out = BoundaryFacts::default();
    if let Some(bag) = msg.harness_extra(Harness::Devin) {
        if let Some(comp) = bag.get("compaction").and_then(Value::as_object) {
            out.summarized_from = comp.get("summarized_from").and_then(Value::as_i64);
            out.pre_tokens = comp.get("pre_tokens").and_then(Value::as_u64);
            out.source = comp.get("source").cloned();
            out.edited_files = comp.get("edited_files").cloned();
            out.todo_list = comp.get("todo_list").cloned();
        }
        if let Some(cm) = bag.get("compactMetadata").and_then(Value::as_object) {
            if out.source.is_none() {
                out.source = cm.get("trigger").cloned();
            }
            if out.pre_tokens.is_none() {
                out.pre_tokens = cm.get("preTokens").and_then(Value::as_u64);
            }
        }
    }
    for v in msg.extra.values() {
        if let Some(cm) = v.get("compactMetadata").and_then(Value::as_object) {
            if out.source.is_none() {
                out.source = cm.get("trigger").cloned();
            }
            if out.pre_tokens.is_none() {
                out.pre_tokens = cm.get("preTokens").and_then(Value::as_u64);
            }
        }
    }
    out
}

fn is_paired_compaction_boundary(messages: &[Message], idx: usize, summary_parents: &HashSet<&str>) -> bool {
    let msg = &messages[idx];
    if msg.kind != MessageKind::CompactionBoundary {
        return false;
    }
    if msg.is_synthesized()
        || msg
            .id
            .as_deref()
            .is_some_and(|id| id.ends_with("#boundary") || summary_parents.contains(id))
    {
        return true;
    }
    messages[idx + 1..]
        .iter()
        .find(|m| m.kind != MessageKind::Carrier)
        .is_some_and(|next| next.kind == MessageKind::CompactionSummary)
}

fn write_database_with_verify(
    session: &Session,
    opts: &EmitOptions,
    new_id: &str,
    cwd: Option<&Path>,
    conn: &mut Connection,
    verify_ctx: Option<(&EmitResult, Option<crate::emit::stage::VerifyFn<'_>>)>,
) -> Result<()> {
    let resolver = session.resolver();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    ensure_schema_17(&tx)?;

    let exists: bool = tx
        .query_row("SELECT 1 FROM sessions WHERE id = ?1", [new_id], |_| Ok(true))
        .unwrap_or(false);
    if exists && !opts.force {
        let db_path = tx.path().map(PathBuf::from).unwrap_or_default();
        return Err(crate::publish::Exists {
            path: db_path,
            hint: "",
        }
        .into());
    }
    if exists {
        for table in [
            "message_nodes",
            "subagent_heads",
            "tool_call_state",
            "prompt_history",
            "rendered_commits",
        ] {
            if table_exists(&tx, table) {
                tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [new_id])?;
            }
        }
        tx.execute("DELETE FROM sessions WHERE id = ?1", [new_id])?;
    }

    let first_msg_ts = session.messages.iter().find_map(|m| m.timestamp);
    let last_msg_ts = session.messages.iter().rev().find_map(|m| m.timestamp);
    let now = Utc::now();
    let created_secs = session
        .created_at
        .or(first_msg_ts)
        .or(session.updated_at)
        .unwrap_or(now)
        .timestamp()
        .max(1);
    let updated_secs = session
        .updated_at
        .or(last_msg_ts)
        .or(session.created_at)
        .unwrap_or(now)
        .timestamp()
        .max(created_secs);

    let sbag = session.harness_extra(Harness::Devin);
    let backend_type = sbag
        .and_then(|b| b.get("backend_type"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("windsurf");
    let agent_mode = sbag
        .and_then(|b| b.get("agent_mode"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("normal");
    let hidden = i64::from(sbag.and_then(|b| b.get("hidden")).and_then(Value::as_bool) == Some(true));
    let workspace_dirs = sbag
        .and_then(|b| b.get("workspace_dirs"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "[]".to_string());
    let cogs_json = sbag
        .and_then(|b| b.get("cogs_json"))
        .filter(|v| !v.is_null())
        .map(Value::to_string);
    let mut session_meta_val = sbag.and_then(|b| b.get("metadata")).filter(|v| !v.is_null()).cloned();
    if let Some((df, dk)) = crate::lineage::raw_cv_derived_info(session).filter(|(id, _)| *id != new_id) {
        let mut cv_obj = Map::new();
        cv_obj.insert("derived_from".into(), json!(df));
        if let Some(kind) = dk {
            cv_obj.insert("derived_kind".into(), json!(kind));
        }
        match &mut session_meta_val {
            Some(Value::Object(map)) => {
                map.insert("cv".into(), Value::Object(cv_obj));
            }
            None => {
                session_meta_val = Some(json!({ "cv": cv_obj }));
            }
            _ => {}
        }
    }
    let session_meta = session_meta_val.map(|v| v.to_string());

    let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let model_str = session.model.as_deref().unwrap_or("");

    let nodes = build_nodes(session, opts, created_secs, &resolver)?;
    let main_chain_id = (!nodes.is_empty()).then_some(nodes.len() as i64);

    tx.execute(
        "INSERT INTO sessions \
         (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, \
          title, main_chain_id, shell_last_seen_index, cogs_json, workspace_dirs, hidden, metadata) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13)",
        params![
            new_id,
            cwd_str,
            backend_type,
            model_str,
            agent_mode,
            created_secs,
            updated_secs,
            session.title.as_deref(),
            main_chain_id,
            cogs_json,
            workspace_dirs,
            hidden,
            session_meta,
        ],
    )
    .context("inserting devin session row")?;

    if !nodes.is_empty() {
        let mut node_insert = tx.prepare(
            "INSERT INTO message_nodes \
             (session_id, node_id, parent_node_id, chat_message, created_at, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (idx, node) in nodes.iter().enumerate() {
            let node_id = (idx + 1) as i64;
            let parent_node_id = (idx > 0).then_some(idx as i64);
            node_insert
                .execute(params![
                    new_id,
                    node_id,
                    parent_node_id,
                    &node.chat_message,
                    node.created_at,
                    &node.metadata,
                ])
                .context("inserting devin message_nodes row")?;
        }
    }

    crate::harness::wire::write::check_source_fields(session)?;
    if let Some((result, Some(verify_fn))) = verify_ctx {
        verify_fn(crate::emit::stage::StagedSource::Sqlite { conn: &tx, result })?;
    }
    tx.commit()?;
    Ok(())
}

fn build_nodes(
    session: &Session,
    opts: &EmitOptions,
    default_secs: i64,
    resolver: &crate::lazy::Resolver,
) -> Result<Vec<PendingNode>> {
    let mut nodes: Vec<PendingNode> = Vec::new();
    let has_system_prompt_msg = session.messages.iter().any(|m| m.kind == MessageKind::SystemPrompt);

    if !has_system_prompt_msg {
        if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            let chat = json!({
                "message_id": Uuid::new_v4().to_string(),
                "role": "system",
                "content": sp,
            });
            let meta = json!({
                "summarized_from": Value::Null,
                "num_tokens_preceding": Value::Null,
                "is_system_prefix": true,
            });
            nodes.push(PendingNode {
                chat_message: chat.to_string(),
                created_at: default_secs,
                metadata: meta.to_string(),
            });
        }
    }

    let summary_parents: HashSet<&str> = session
        .messages
        .iter()
        .filter(|m| m.kind == MessageKind::CompactionSummary)
        .filter_map(|m| m.parent_id.as_deref())
        .collect();

    let mut pending_boundary: Option<BoundaryFacts> = None;
    let mut in_summary_run = false;
    let mut active_group_sf: Option<i64> = None;
    let mut last_finished_sf: Option<i64> = None;
    let mut synthetic_sf_counter: i64 = 0;

    for (idx, msg) in session.messages.iter().enumerate() {
        let bookkeeping =
            msg.kind == MessageKind::Carrier || (msg.is_carrier() && msg.content.is_empty() && !msg.is_synthesized());
        if bookkeeping {
            continue;
        }
        if is_paired_compaction_boundary(&session.messages, idx, &summary_parents) {
            pending_boundary = Some(extract_boundary_facts(msg));
            in_summary_run = false;
            continue;
        }

        let projected = msg
            .content
            .iter()
            .map(|b| crate::emit::thinking::fate(b, Harness::Devin, opts.thinking, resolver).map(|f| (b, f)))
            .collect::<Result<Vec<_>>>()?;

        if !msg.content.is_empty() && !projected.iter().any(|(b, f)| block_survives_in_devin(msg.role, b, *f)) {
            continue;
        }

        let mbag = msg.harness_extra(Harness::Devin);
        let (summarized_from, boundary_for_node) = if msg.kind == MessageKind::CompactionSummary {
            let explicit_sf = mbag
                .and_then(|b| b.get("summarized_from"))
                .and_then(Value::as_i64)
                .or_else(|| pending_boundary.as_ref().and_then(|bf| bf.summarized_from));
            let sf = if in_summary_run {
                match explicit_sf {
                    Some(esf) if Some(esf) != active_group_sf => esf,
                    _ => active_group_sf.unwrap_or(0),
                }
            } else {
                let chosen = match explicit_sf {
                    Some(esf) if Some(esf) != last_finished_sf => esf,
                    _ => {
                        let cand = nodes.len() as i64;
                        if cand > 0 && Some(cand) != last_finished_sf {
                            cand
                        } else {
                            synthetic_sf_counter -= 1;
                            synthetic_sf_counter
                        }
                    }
                };
                in_summary_run = true;
                chosen
            };
            active_group_sf = Some(sf);
            (Some(sf), pending_boundary.take())
        } else {
            if in_summary_run {
                last_finished_sf = active_group_sf.take();
                in_summary_run = false;
            }
            pending_boundary = None;
            (None, None)
        };

        let created_secs = msg.timestamp.map(|t| t.timestamp().max(1)).unwrap_or(default_secs);
        let created_rfc3339 = msg.timestamp.map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true));

        let node_meta_str = build_node_metadata(msg, mbag, summarized_from, boundary_for_node.as_ref());

        if msg.role == Role::Tool {
            let tool_blocks: Vec<&Block> = msg
                .content
                .iter()
                .filter(|b| matches!(b, Block::ToolResult { .. }))
                .collect();
            if tool_blocks.is_empty() {
                let chat = build_tool_chat_empty(msg, created_rfc3339.as_deref(), mbag);
                nodes.push(PendingNode {
                    chat_message: chat.to_string(),
                    created_at: created_secs,
                    metadata: node_meta_str,
                });
            } else {
                for (t_idx, b) in tool_blocks.into_iter().enumerate() {
                    let mid = tool_turn_message_id(msg.id.as_deref(), t_idx);
                    let chat = build_tool_chat(&mid, b, created_rfc3339.as_deref(), mbag, resolver)?;
                    nodes.push(PendingNode {
                        chat_message: chat,
                        created_at: created_secs,
                        metadata: if t_idx == 0 {
                            node_meta_str.clone()
                        } else {
                            "null".to_string()
                        },
                    });
                }
            }
            continue;
        }

        if msg.role == Role::User {
            let tool_blocks: Vec<&Block> = msg
                .content
                .iter()
                .filter(|b| matches!(b, Block::ToolResult { .. }))
                .collect();
            if !tool_blocks.is_empty() {
                let has_other = projected
                    .iter()
                    .any(|(b, f)| !matches!(b, Block::ToolResult { .. }) && block_survives_in_devin(Role::User, b, *f));
                for (t_idx, b) in tool_blocks.into_iter().enumerate() {
                    let mid = if !has_other && t_idx == 0 {
                        msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string())
                    } else {
                        match msg.id.as_deref() {
                            Some(id) => format!("{id}#tool{t_idx}"),
                            None => Uuid::new_v4().to_string(),
                        }
                    };
                    let chat = build_tool_chat(&mid, b, created_rfc3339.as_deref(), None, resolver)?;
                    nodes.push(PendingNode {
                        chat_message: chat,
                        created_at: created_secs,
                        metadata: "null".to_string(),
                    });
                }
                if !has_other {
                    continue;
                }
            }
        }

        let chat = build_turn_chat(
            session,
            msg,
            &projected,
            created_rfc3339.as_deref(),
            mbag,
            boundary_for_node.as_ref(),
            resolver,
        )?;
        nodes.push(PendingNode {
            chat_message: chat,
            created_at: created_secs,
            metadata: node_meta_str,
        });
    }

    Ok(nodes)
}

fn block_survives_in_devin(role: Role, block: &Block, fate: ThinkingFate) -> bool {
    match block {
        Block::Text { .. } => true,
        Block::Thinking { .. } => fate != ThinkingFate::Drop,
        Block::ToolUse { .. } => role == Role::Assistant,
        Block::ToolResult { .. } => matches!(role, Role::Tool | Role::User),
        Block::Image { .. } | Block::File { .. } | Block::Opaque { .. } => false,
    }
}

fn build_node_metadata(
    msg: &Message,
    mbag: Option<&Map<String, Value>>,
    summarized_from: Option<i64>,
    boundary: Option<&BoundaryFacts>,
) -> String {
    let is_prefix = msg.kind == MessageKind::SystemPrompt;
    let ntp = mbag
        .and_then(|b| b.get("num_tokens_preceding"))
        .and_then(Value::as_u64)
        .or_else(|| boundary.and_then(|bf| bf.pre_tokens));
    let prior_ids = mbag.and_then(|b| b.get("prior_node_ids")).cloned();

    if !is_prefix && summarized_from.is_none() && ntp.is_none() && prior_ids.is_none() {
        return "null".to_string();
    }

    let mut meta = Map::new();
    meta.insert(
        "summarized_from".into(),
        summarized_from.map_or(Value::Null, |v| json!(v)),
    );
    meta.insert("num_tokens_preceding".into(), ntp.map_or(Value::Null, |v| json!(v)));
    meta.insert(
        "is_system_prefix".into(),
        if is_prefix { Value::Bool(true) } else { Value::Null },
    );
    if let Some(prior) = prior_ids {
        meta.insert("extensions".into(), json!({ "compact/prior_node_ids": prior }));
    }
    Value::Object(meta).to_string()
}

fn tool_turn_message_id(base: Option<&str>, idx: usize) -> String {
    match (base.filter(|s| !s.is_empty()), idx) {
        (Some(id), 0) => id.to_string(),
        (Some(id), n) => format!("{id}#{n}"),
        (None, _) => Uuid::new_v4().to_string(),
    }
}

fn build_tool_chat_empty(msg: &Message, created_rfc3339: Option<&str>, mbag: Option<&Map<String, Value>>) -> Value {
    let mut mmeta = Map::new();
    if let Some(ts) = created_rfc3339 {
        mmeta.insert("created_at".into(), json!(ts));
    }
    copy_common_mmeta(mbag, &mut mmeta);
    let mut chat = Map::new();
    chat.insert(
        "message_id".into(),
        json!(msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string())),
    );
    chat.insert("role".into(), json!("tool"));
    chat.insert("tool_call_id".into(), json!(""));
    chat.insert("content".into(), json!(""));
    if !mmeta.is_empty() {
        chat.insert("metadata".into(), Value::Object(mmeta));
    }
    Value::Object(chat)
}

fn build_tool_chat(
    message_id: &str,
    block: &Block,
    created_rfc3339: Option<&str>,
    mbag: Option<&Map<String, Value>>,
    resolver: &crate::lazy::Resolver,
) -> Result<String> {
    let Block::ToolResult {
        tool_use_id,
        content,
        is_error,
        details,
        ..
    } = block
    else {
        unreachable!("build_tool_chat expects Block::ToolResult");
    };

    let mut body = String::new();
    content.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        body.push_str(chunk);
        Ok(())
    })?;

    let mut ext = Map::new();
    let mut result_meta = Map::new();
    result_meta.insert("success".into(), Value::Bool(!*is_error));

    if let Some(Value::Object(d)) = details {
        if let Some(kind) = d.get("kind").filter(|v| !v.is_null()) {
            result_meta.insert("kind".into(), kind.clone());
        }
        if let Some(reason) = d.get("failure_reason").and_then(Value::as_str) {
            ext.insert("chisel/tool_failure".into(), json!({ "reason": reason }));
        }
        let mut term = Map::new();
        if let Some(cwd) = d.get("cwd").filter(|v| !v.is_null()) {
            term.insert("cwd".into(), cwd.clone());
        }
        let mut exit = Map::new();
        for k in ["exit_code", "terminal_id"] {
            if let Some(v) = d.get(k).filter(|v| !v.is_null()) {
                exit.insert(k.into(), v.clone());
            }
        }
        if !exit.is_empty() {
            term.insert("exit".into(), Value::Object(exit));
        }
        if !term.is_empty() {
            ext.insert("chisel/terminal_output".into(), Value::Object(term));
        }
        if let Some(dur) = d.get("duration_ms").filter(|v| !v.is_null()) {
            ext.insert("chisel/tool_call_timing".into(), json!({ "duration_ms": dur }));
        }
        if let Some(paths) = d.get("restorable").and_then(Value::as_array) {
            let undo: Vec<Value> = paths
                .iter()
                .map(|p| json!({ "kind": "restore_file", "path": p }))
                .collect();
            if !undo.is_empty() {
                ext.insert("chisel/undo".into(), Value::Array(undo));
            }
        }
    }
    ext.insert("chisel/tool_result_meta".into(), Value::Object(result_meta));
    copy_bag_extensions(mbag, &mut ext);

    let mut mmeta = Map::new();
    if let Some(ts) = created_rfc3339 {
        mmeta.insert("created_at".into(), json!(ts));
    }
    copy_common_mmeta(mbag, &mut mmeta);
    if !ext.is_empty() {
        mmeta.insert("extensions".into(), Value::Object(ext));
    }

    let mut chat = Map::new();
    chat.insert("message_id".into(), json!(message_id));
    chat.insert("role".into(), json!("tool"));
    chat.insert("tool_call_id".into(), json!(tool_use_id));
    chat.insert("content".into(), json!(body));
    chat.insert("metadata".into(), Value::Object(mmeta));
    Ok(Value::Object(chat).to_string())
}

fn build_turn_chat(
    session: &Session,
    msg: &Message,
    projected: &[(&Block, ThinkingFate)],
    created_rfc3339: Option<&str>,
    mbag: Option<&Map<String, Value>>,
    boundary: Option<&BoundaryFacts>,
    resolver: &crate::lazy::Resolver,
) -> Result<String> {
    let role_str = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => unreachable!("Role::Tool handled prior to build_turn_chat"),
    };

    let mut content_text = String::new();
    for &(block, fate) in projected {
        let val = match block {
            Block::Text { text, .. } => Some(std::borrow::Cow::Borrowed(text)),
            Block::Thinking { .. } => crate::emit::thinking::text_with_fate(block, fate),
            _ => None,
        };
        if let Some(val) = val {
            let mut local_any = false;
            val.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
                if !chunk.is_empty() {
                    if !local_any && !content_text.is_empty() {
                        content_text.push('\n');
                    }
                    local_any = true;
                    content_text.push_str(chunk);
                }
                Ok(())
            })?;
        }
    }

    let mut mmeta = Map::new();
    if let Some(ts) = created_rfc3339 {
        mmeta.insert("created_at".into(), json!(ts));
    }
    copy_common_mmeta(mbag, &mut mmeta);

    let mut ext = Map::new();
    copy_bag_extensions(mbag, &mut ext);

    if let Some(bf) = boundary {
        if let Some(src) = &bf.source {
            ext.entry("devin-rs/summary".to_string())
                .or_insert_with(|| json!({ "source": src }));
        }
        if let Some(files) = &bf.edited_files {
            ext.entry("compact/edited_files".to_string())
                .or_insert_with(|| json!({ "paths": files }));
        }
        if let Some(todos) = &bf.todo_list {
            ext.entry("compact/todo_list".to_string())
                .or_insert_with(|| todos.clone());
        }
    }

    if msg.role == Role::User {
        let is_user_input = matches!(msg.kind, MessageKind::Prompt | MessageKind::CompactionSummary);
        mmeta.insert("is_user_input".into(), Value::Bool(is_user_input));
        if msg.origin == Origin::Subagent {
            ext.insert("subagent/handoff".into(), Value::Bool(true));
        }
    }

    let mut thinking_obj: Option<Value> = None;
    let mut tool_calls: Vec<Value> = Vec::new();

    if msg.role == Role::Assistant {
        let mut th_text = String::new();
        let mut first_sig: Option<&str> = None;
        for &(block, fate) in projected {
            if fate != ThinkingFate::Keep {
                continue;
            }
            if let Block::Thinking { text, signature, .. } = block {
                let mut local_any = false;
                text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
                    if !chunk.is_empty() {
                        if !local_any && !th_text.is_empty() {
                            th_text.push_str("\n\n");
                        }
                        local_any = true;
                        th_text.push_str(chunk);
                    }
                    Ok(())
                })?;
                if first_sig.is_none() {
                    first_sig = signature.as_deref();
                }
            }
        }
        if !th_text.is_empty() || first_sig.is_some() {
            let mut th = Map::new();
            th.insert("thinking".into(), json!(th_text));
            if let Some(sig) = first_sig {
                th.insert("signature".into(), json!(sig));
                th.insert("signature_type".into(), json!("sealed"));
            }
            thinking_obj = Some(Value::Object(th));
        }

        for &(block, _) in projected {
            if let Block::ToolUse { id, name, input, .. } = block {
                let mut raw_args = Vec::new();
                input.write_canonical(&mut raw_args)?;
                let args_val: Value =
                    serde_json::from_slice(&raw_args).context("parsing canonical tool_use arguments")?;
                let idx = tool_calls.len();
                tool_calls.push(json!({
                    "id": id,
                    "name": name,
                    "arguments": args_val,
                    "index": idx,
                    "kind": "function",
                }));
            }
        }

        if let Some(model) = msg
            .model
            .as_deref()
            .or(session.model.as_deref())
            .filter(|m| !m.is_empty())
        {
            mmeta.insert("generation_model".into(), json!(model));
        }
        if let Some(u) = &msg.usage {
            let mut metrics = Map::new();
            if let Some(v) = u.input_tokens {
                metrics.insert("input_tokens".into(), json!(v));
            }
            if let Some(v) = u.output_tokens {
                metrics.insert("output_tokens".into(), json!(v));
            }
            if let Some(v) = u.cache_read_tokens {
                metrics.insert("cache_read_tokens".into(), json!(v));
            }
            if let Some(v) = u.cache_creation_tokens {
                metrics.insert("cache_creation_tokens".into(), json!(v));
            }
            if metrics.is_empty() {
                metrics.insert("output_tokens".into(), json!(u.reasoning_tokens.unwrap_or(0)));
            }
            mmeta.insert("metrics".into(), Value::Object(metrics));
        }
    }

    if !ext.is_empty() {
        mmeta.insert("extensions".into(), Value::Object(ext));
    }

    let mut chat = Map::new();
    chat.insert(
        "message_id".into(),
        json!(msg
            .id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string())),
    );
    chat.insert("role".into(), json!(role_str));
    chat.insert("content".into(), json!(content_text));
    if let Some(th) = thinking_obj {
        chat.insert("thinking".into(), th);
    }
    if !tool_calls.is_empty() {
        chat.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if !mmeta.is_empty() {
        chat.insert("metadata".into(), Value::Object(mmeta));
    }
    Ok(Value::Object(chat).to_string())
}

fn copy_common_mmeta(mbag: Option<&Map<String, Value>>, mmeta: &mut Map<String, Value>) {
    let Some(bag) = mbag else { return };
    for k in ["finish_reason", "request_id", "telemetry", "num_tokens"] {
        if let Some(v) = bag.get(k).filter(|v| !v.is_null()) {
            mmeta.insert(k.into(), v.clone());
        }
    }
}

fn copy_bag_extensions(mbag: Option<&Map<String, Value>>, ext: &mut Map<String, Value>) {
    let Some(bag) = mbag else { return };
    for (src, dst) in [
        ("cog_context", "affogato/cog-context"),
        ("user_question_answers", "chisel/user_question_answers"),
        ("lead_model", "chisel/fusion_lead_model_uid"),
        ("rules_loaded", "agent-ext/rules-loaded"),
        ("tool_call_content", "chisel/tool_call_content"),
    ] {
        if let Some(v) = bag.get(src).filter(|v| !v.is_null()) {
            ext.insert(dst.into(), v.clone());
        }
    }
    if let Some(skills) = bag.get("skills_loaded").and_then(Value::as_array) {
        ext.insert("agent-ext/skills-loaded".into(), json!({ "skills": skills }));
    }
    if let Some(sub) = bag.get("subagent").and_then(Value::as_object) {
        for (src, dst) in [
            ("profile_name", "subagent/profile_name"),
            ("model", "subagent/model"),
            ("agent_id", "subagent/agent_id"),
            ("chain_node_id", "subagent/chain_node_id"),
        ] {
            if let Some(v) = sub.get(src).filter(|v| !v.is_null()) {
                ext.insert(dst.into(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Devin;
    use super::*;
    use crate::harness::{Adapter, Parse};
    use crate::ir::{SessionRef, Usage};

    #[test]
    fn claude_to_devin_strict_verified_preserves_full_turn_and_session_fidelity() -> Result<()> {
        let world = crate::test_world("devin-emit-claude-strict");
        let ts0: chrono::DateTime<Utc> = "2026-11-20T10:00:00Z".parse()?;
        let ts1: chrono::DateTime<Utc> = "2026-11-20T10:00:05Z".parse()?;
        let ts2: chrono::DateTime<Utc> = "2026-11-20T10:00:08Z".parse()?;
        let ts3: chrono::DateTime<Utc> = "2026-11-20T10:00:12Z".parse()?;

        let mut s = Session::new("claude-src-1", Harness::Claude);
        s.title = Some("Ported Claude investigation".into());
        s.cwd = Some(PathBuf::from("/work/devin-port"));
        s.model = Some("claude-sonnet-4-6".into());
        s.system_prompt = Some("You are a careful systems engineer.".into());
        s.created_at = Some(ts0);
        s.updated_at = Some(ts3);

        let mut u1 = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
        u1.id = Some("m-user-1".into());
        u1.timestamp = Some(ts0);
        u1.content
            .push(Block::from_text("Inspect Cargo.toml and run check.".into()));

        let mut a1 = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
        a1.id = Some("m-asst-1".into());
        a1.timestamp = Some(ts1);
        a1.model = Some("claude-sonnet-4-6".into());
        a1.usage = Some(Usage {
            input_tokens: Some(320),
            output_tokens: Some(95),
            cache_read_tokens: Some(1024),
            cache_creation_tokens: Some(128),
            ..Default::default()
        });
        a1.content = vec![
            Block::Thinking {
                text: "Let's read Cargo.toml and run cargo check.".into(),
                signature: Some("sealed.sig.alpha".into()),
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            Block::from_text("Checking both now.".into()),
            Block::ToolUse {
                id: "toolu_read".into(),
                name: "Read".into(),
                input: json!({"file_path": "/work/devin-port/Cargo.toml"}).into(),
                namespace: None,
                extensions: Default::default(),
            },
            Block::ToolUse {
                id: "toolu_bash".into(),
                name: "Bash".into(),
                input: json!({"command": "cargo check"}).into(),
                namespace: None,
                extensions: Default::default(),
            },
        ];

        let mut tr = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        tr.id = Some("m-tool-1".into());
        tr.timestamp = Some(ts2);
        tr.content = vec![
            Block::ToolResult {
                tool_use_id: "toolu_read".into(),
                tool_name: Some("Read".into()),
                content: "[package]\nname = \"demo\"\n".into(),
                is_error: false,
                status: Some("completed".into()),
                details: Some(json!({"duration_ms": 4})),
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "toolu_bash".into(),
                tool_name: Some("Bash".into()),
                content: "error: missing semicolon".into(),
                is_error: true,
                status: Some("error".into()),
                details: Some(json!({"cwd": "/work/devin-port", "exit_code": 101, "duration_ms": 250})),
                extensions: Default::default(),
            },
        ];

        let mut a2 = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
        a2.id = Some("m-asst-2".into());
        a2.timestamp = Some(ts3);
        a2.model = Some("claude-sonnet-4-6".into());
        a2.usage = Some(Usage {
            input_tokens: Some(450),
            output_tokens: Some(40),
            cache_read_tokens: Some(1344),
            cache_creation_tokens: Some(0),
            ..Default::default()
        });
        a2.content.push(Block::from_text(
            "Cargo.toml is valid; cargo check found one syntax error.".into(),
        ));

        s.messages = vec![u1, a1, tr, a2];

        let out_dir = world.base.join("devin-cli");
        let (res, warnings) = emit_verified(
            &s,
            Harness::Devin,
            &out_dir,
            &EmitOptions {
                new_id: Some("ported-alpha".into()),
                strict: true,
                thinking: ThinkingMode::Native,
                ..Default::default()
            },
        )?;
        assert_eq!(res.path, out_dir.join("sessions.db"));
        assert!(
            res.resume_hint.as_deref().unwrap().contains("devin -r ported-alpha"),
            "resume hint: {:?}",
            res.resume_hint
        );
        // Only expected tool-result details / target-supplied split tool timestamp notes, zero unexpected losses.
        assert!(
            warnings.iter().all(|w| w.contains("expected")),
            "warnings: {warnings:?}"
        );

        let adapter = Devin::with_root(&out_dir);
        let discovered = adapter.discover()?;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "ported-alpha");
        assert_eq!(discovered[0].title.as_deref(), Some("Ported Claude investigation"));
        assert_eq!(discovered[0].cwd.as_deref(), Some(Path::new("/work/devin-port")));
        assert_eq!(discovered[0].message_count, Some(3));

        let reparsed = adapter.parse(&discovered[0])?;
        assert_eq!(
            reparsed.system_prompt.as_deref(),
            Some("You are a careful systems engineer.")
        );
        assert_eq!(reparsed.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(reparsed.title.as_deref(), Some("Ported Claude investigation"));
        assert_eq!(reparsed.cwd.as_deref(), Some(Path::new("/work/devin-port")));
        assert_eq!(
            reparsed.harness_extra(Harness::Devin).unwrap()["schema_version"],
            json!(17)
        );

        let reparsed_asst = reparsed
            .messages
            .iter()
            .find(|m| m.id.as_deref() == Some("m-asst-1"))
            .unwrap();
        assert_eq!(reparsed_asst.usage, s.messages[1].usage);
        assert!(matches!(
            &reparsed_asst.content[0],
            Block::Thinking { text, signature, .. }
                if text.matches_str("Let's read Cargo.toml and run cargo check.") == Some(true)
                    && signature.as_deref() == Some("sealed.sig.alpha")
        ));

        let tool_results: Vec<_> = reparsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 2);
        match &tool_results[0].content[0] {
            Block::ToolResult {
                is_error, tool_name, ..
            } => {
                assert!(!is_error);
                assert_eq!(tool_name.as_deref(), Some("Read"));
            }
            _ => panic!("expected first ToolResult"),
        }
        match &tool_results[1].content[0] {
            Block::ToolResult {
                is_error,
                tool_name,
                details,
                ..
            } => {
                assert!(*is_error);
                assert_eq!(tool_name.as_deref(), Some("Bash"));
                assert_eq!(details.as_ref().unwrap()["exit_code"], json!(101));
                assert_eq!(details.as_ref().unwrap()["cwd"], json!("/work/devin-port"));
            }
            _ => panic!("expected second ToolResult"),
        }
        Ok(())
    }

    #[test]
    fn devin_to_devin_round_trip_preserves_compaction_without_doubling_boundaries() -> Result<()> {
        let world = crate::test_world("devin-roundtrip-compaction");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/devin/sessions-v17-3000.11.3.db");
        let src_adapter = Devin::detect();
        // `fixture-alpha/sidekick` has 2 prefix nodes, rules, skills, subagent handoff prompt,
        // tool calls, a compaction summary pair (summarized_from = 24), and a failed tool result!
        let src = crate::stream::collect_with(
            &src_adapter,
            &SessionRef::new("fixture-alpha/sidekick", Harness::Devin, &fixture),
            &crate::stream::Demand::COMPLETE,
        )?;
        let src_boundaries = src
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionBoundary)
            .count();
        let src_summaries = src
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionSummary)
            .count();
        assert_eq!(src_boundaries, 1);
        assert_eq!(src_summaries, 2);

        let out_db = world.base.join("custom-out.sqlite");
        let (res, warnings) = emit_verified(
            &src,
            Harness::Devin,
            &out_db,
            &EmitOptions {
                new_id: Some("rehomed-sidekick".into()),
                strict: true,
                thinking: ThinkingMode::Native,
                parsed_at: ParsedAt::Complete,
                ..Default::default()
            },
        )?;
        assert_eq!(res.path, out_db);
        assert!(
            warnings.iter().all(|w| w.contains("expected")),
            "warnings: {warnings:?}"
        );

        let reparsed = src_adapter.parse(&SessionRef::new("rehomed-sidekick", Harness::Devin, &out_db))?;
        let dst_boundaries = reparsed
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionBoundary)
            .count();
        let dst_summaries = reparsed
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::CompactionSummary)
            .count();
        assert_eq!(dst_boundaries, 1, "compaction boundary must not double on round-trip");
        assert_eq!(dst_summaries, 2);
        assert_eq!(reparsed.messages.len(), src.messages.len());
        assert_eq!(reparsed.system_prompt, src.system_prompt);

        let handoff = reparsed
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::Prompt)
            .unwrap();
        assert_eq!(handoff.origin, Origin::Subagent, "subagent handoff origin survives");

        let compactions = crate::compaction::detect_in_session(&reparsed, true)?;
        assert_eq!(compactions.len(), 1);
        assert_eq!(compactions[0].trigger.as_deref(), Some("async_file_compactor"));
        assert_eq!(compactions[0].pre_tokens, Some(48000));
        Ok(())
    }

    #[test]
    fn id_collision_refuses_without_force_and_rolls_back_strict_failures_intact() -> Result<()> {
        let world = crate::test_world("devin-collision-and-rollback");
        let out_dir = world.base.join("store");

        let make_session = |title: &str, body: &str| {
            let mut s = Session::new("src", Harness::Claude);
            s.title = Some(title.into());
            s.cwd = Some(PathBuf::from("/work/proj"));
            s.model = Some("swe-2-high".into());
            let mut u = Message::new(Role::User);
            u.content.push(Block::from_text(body.into()));
            s.messages.push(u);
            s
        };

        let first = make_session("Original title", "original prompt");
        let pinned = EmitOptions {
            new_id: Some("pinned-slug".into()),
            ..Default::default()
        };
        let res = emit(&first, &out_dir, &pinned)?;

        // 1. Duplicate ID without `--force` fails with `publish::Exists`, leaving original untouched.
        let second = make_session("Replacement title", "replacement prompt");
        let err = emit(&second, &out_dir, &pinned).unwrap_err();
        assert!(
            err.downcast_ref::<crate::publish::Exists>().is_some(),
            "expected publish::Exists, got {err:#}"
        );
        let adapter = Devin::with_root(&out_dir);
        let kept = adapter.parse(&SessionRef::new("pinned-slug", Harness::Devin, &res.path))?;
        assert_eq!(kept.title.as_deref(), Some("Original title"));
        assert_eq!(kept.messages[0].text().as_deref(), Some("original prompt"));

        // 2. `--force` + `--strict` failure rolls back inside the same `BEGIN IMMEDIATE` tx!
        let mut lossy = make_session("Broken overwrite", "will fail strict");
        let mut bad_tool = Message::new(Role::Tool);
        bad_tool.content = vec![
            Block::from_text("unrepresentable tool-role prose".into()),
            Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ];
        lossy.messages.push(bad_tool);

        assert!(emit_verified(
            &lossy,
            Harness::Devin,
            &out_dir,
            &EmitOptions {
                new_id: Some("pinned-slug".into()),
                force: true,
                strict: true,
                ..Default::default()
            },
        )
        .is_err());

        let still_kept = adapter.parse(&SessionRef::new("pinned-slug", Harness::Devin, &res.path))?;
        assert_eq!(still_kept.title.as_deref(), Some("Original title"));
        assert_eq!(still_kept.messages.len(), 1);
        assert_eq!(still_kept.messages[0].text().as_deref(), Some("original prompt"));

        // 3. `--force` with valid replacement succeeds and replaces the chain cleanly.
        emit(&second, &out_dir, &EmitOptions { force: true, ..pinned })?;
        let replaced = adapter.parse(&SessionRef::new("pinned-slug", Harness::Devin, &res.path))?;
        assert_eq!(replaced.title.as_deref(), Some("Replacement title"));
        assert_eq!(replaced.messages.len(), 1);
        assert_eq!(replaced.messages[0].text().as_deref(), Some("replacement prompt"));

        // 4. Fresh directory + `--strict` failure leaves zero `.db` or directory behind.
        let fresh_fail = world.base.join("fresh-rolled-back");
        assert!(emit_verified(
            &lossy,
            Harness::Devin,
            &fresh_fail,
            &EmitOptions {
                new_id: Some("never-created".into()),
                strict: true,
                ..Default::default()
            },
        )
        .is_err());
        assert!(!fresh_fail.exists(), "aborted fresh emit must clean up empty db + dir");
        Ok(())
    }

    // #tooth A store Devin has not migrated to this schema is refused as it stands: no column or
    // table of a later migration is added behind its migration history.
    #[test]
    fn an_older_or_unrecognized_store_is_refused_and_left_as_it_was() -> Result<()> {
        let world = crate::test_world("devin-older-store");
        let mut session = Session::new("src", Harness::Claude);
        let mut prompt = Message::new(Role::User);
        prompt.content.push(Block::from_text("hello".into()));
        session.messages.push(prompt);

        let schema_of = |db: &Path| -> Result<Vec<String>> {
            let conn = Connection::open(db)?;
            let mut stmt = conn.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        for (name, history) in [("at-16", Some(16)), ("empty-history", Some(0)), ("no-history", None)] {
            let dir = world.base.join(name);
            std::fs::create_dir_all(&dir)?;
            let db = dir.join("sessions.db");
            {
                let conn = Connection::open(&db)?;
                // Schema 16: `sessions.metadata` exists, `subagent_heads` does not yet.
                conn.execute_batch(
                    "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, \
                     backend_type TEXT NOT NULL, model TEXT NOT NULL, agent_mode TEXT NOT NULL, \
                     created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL, title TEXT, \
                     main_chain_id INTEGER, hidden INTEGER NOT NULL DEFAULT 0);
                     CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
                     node_id INTEGER NOT NULL, parent_node_id INTEGER, chat_message TEXT NOT NULL, \
                     created_at INTEGER NOT NULL);",
                )?;
                if let Some(versions) = history {
                    conn.execute_batch(
                        "CREATE TABLE refinery_schema_history(version int4 PRIMARY KEY, name VARCHAR(255), \
                         applied_on VARCHAR(255), checksum VARCHAR(255));",
                    )?;
                    for &(version, name) in &SCHEMA_17_MIGRATIONS[..versions] {
                        conn.execute(
                            "INSERT INTO refinery_schema_history VALUES (?1, ?2, '', '')",
                            params![version, name],
                        )?;
                    }
                }
            }
            let before = schema_of(&db)?;
            let err = emit(&session, &dir, &EmitOptions::default()).unwrap_err();
            assert!(format!("{err:#}").contains("--out"), "{name}: {err:#}");
            assert_eq!(schema_of(&db)?, before, "{name}: the refused store was altered");
            let conn = Connection::open(&db)?;
            let sessions: i64 = conn.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?;
            assert_eq!(sessions, 0, "{name}");
        }
        Ok(())
    }
}
