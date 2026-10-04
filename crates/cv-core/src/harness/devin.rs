//! Devin CLI (Cognition) adapter — `~/.local/share/devin/cli/sessions.db` (SQLite, WAL).
//! **sqlite feature only.**
//!
//! Devin CLI is closed-source (github.com/CognitionAI/devin-cli is a release manifest only); this
//! schema was reverse-engineered from a live store (cli 3000.11.3, refinery schema **17** — the
//! `MAX(version)` of `refinery_schema_history`; migration names in order: `initial_schema`,
//! `add_thinking_column`, `add_prompt_history`, `add_metadata_column`, `message_forest`,
//! `add_node_metadata`, `add_shell_context`, `add_session_cogs`, `add_rendered_commits`,
//! `add_workspace_dirs`, `add_prompt_history_is_shell`, `add_app_state`,
//! `rename_permission_mode_to_agent_mode`, `tool_call_state`, `add_hidden_column`,
//! `add_session_json_metadata`, `subagent_heads`). The CLI runs and writes while we read, so the DB
//! is opened READ-ONLY and never checkpointed. The sibling `transcripts/` dir exists but was
//! observed EMPTY (purpose unknown); `app_state.json`, `session_locks/`, `logs/`, `plugins/` sit
//! beside it and are not read.
//!
//! ```sql
//! CREATE TABLE sessions (
//!   id TEXT PRIMARY KEY,               -- two-word slug, e.g. 'flying-poinsettia'
//!   working_directory TEXT NOT NULL, backend_type TEXT NOT NULL,   -- 'windsurf'
//!   model TEXT NOT NULL,               -- CAN BE '' (then the last turn's generation_model wins)
//!   agent_mode TEXT NOT NULL,          -- 'bypass' | 'normal' | 'plan' | 'accept-edits' …
//!   created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL,   -- unix SECONDS
//!   title TEXT, main_chain_id INTEGER, shell_last_seen_index INTEGER DEFAULT 0, cogs_json TEXT,
//!   workspace_dirs TEXT,               -- JSON array string, e.g. '[]'
//!   hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT);
//! CREATE TABLE message_nodes (
//!   row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
//!   node_id INTEGER NOT NULL, parent_node_id INTEGER,      -- NULL = root; UNIQUE(session_id,node_id)
//!   chat_message TEXT NOT NULL,        -- JSON
//!   created_at INTEGER NOT NULL,       -- unix seconds
//!   metadata TEXT);                    -- JSON, or the literal string 'null'
//! CREATE TABLE prompt_history (id, content, timestamp, session_id, is_shell);   -- ignored
//! CREATE TABLE rendered_commits (...); -- ignored (always empty so far)
//! CREATE TABLE app_state (key, value);  -- ignored
//! CREATE TABLE tool_call_state (session_id, tool_call_id, tool_call_json, tool_call_update_json);
//!   -- ACP ToolCall/ToolCallUpdate JSON; duplicates what the assistant message carries — ignored.
//! CREATE TABLE subagent_heads (session_id, agent_id, chain_node_id, updated_at); -- child sessions
//!   -- PK (session_id, agent_id): one CURRENT head per agent; `chain_node_id` names a chain head
//!   -- in the SAME `message_nodes` forest; `updated_at` is unix seconds.
//! ```
//!
//! ## Sub-agent chains
//!
//! Each `subagent_heads` row is a whole second conversation inside the parent's forest (verified:
//! a sidekick's 176-node chain — own `is_system_prefix` prompt, rules/skills loads, then the
//! handoff prompt and its turns — which the main-chain walk never emits). We surface one as a
//! child session id `<session_id>/<agent_id>` (`atom-telephone/sidekick`), discovered right after
//! its parent, with cwd/created_at inherited from the `sessions` row, `title` = `"[<agent_id>]
//! <parent title>"`, `updated_at` from the head row, `lineage.parent`/`agent_path` set, and
//! `extra["devin"]["agent_id"]`/`chain_head`. A `chain_node_id` naming no live node is skipped in
//! discovery and counted under the parent's `extra["devin"]["dangling_subagent_heads"]`.
//!
//! Sub-agent-specific message shapes: the child's first `user` row is the lead's handoff prompt —
//! `is_user_input: true` plus `metadata.extensions["subagent/handoff"] = true` and
//! `extensions["chisel/fusion_lead_model_uid"]` (the lead's model) — mapped Prompt/Subagent, the
//! uid carried as `extra["devin"]["lead_model"]`. Its system rows carry
//! `extensions["agent-ext/rules-loaded"]` (`{rule_paths, available_rule_paths, content_bytes,
//! is_always_on, …}` — kept whole as `rules_loaded`) and `extensions["agent-ext/skills-loaded"]`
//! (`{skills: [{name, description, path}]}` — reduced to `{name, path}` as `skills_loaded`).
//! Other `subagent/*` extensions ride on the parent's completion rows (the `sidekick` tool result
//! and the `<subagent_completion_notification>` system row): `profile_name`, `model`, `agent_id`,
//! `chain_node_id` — reduced into `extra["devin"]["subagent"]`; the tool row's `tool_call_id` is
//! also the child's `lineage.spawned_by_tool_use`.
//!
//! ## Compaction
//!
//! A context compaction writes TWO consecutive chain nodes sharing `summarized_from` = the old
//! window's head node id (still in the forest): an `assistant` node whose `content` is the summary
//! markdown, then a `system` node wrapping it ("You are continuing work from a previous
//! conversation thread…") that carries `devin-rs/summary = {source: "async_file_compactor"}`,
//! `compact/edited_files`, `compact/todo_list`, and `subagent/handoff_history` (large, a verbatim
//! copy of the handoff texts — deliberately not carried) and whose content embeds
//! `Full conversation history saved at <path>.` pointing at the sibling
//! `summaries/<agent_id>/history_<hex>.md` (~300 KB markdown of the pre-compaction history, next
//! to `sessions.db` — referenced, not read). Before the first node of each pair we emit a
//! synthetic `CompactionBoundary` message (`id = <first summary's message_id>#boundary`; the
//! summary's `parent_id` points back at it) carrying `extra["devin"]["compaction"] =
//! {summarized_from, pre_chain_head, source, history_path, edited_files, todo_list}` plus
//! `compactMetadata` (`trigger`, `preTokens` = the pair's max `num_tokens_preceding`) so the
//! shared compaction sink can pair and summarize it. A tool row's `chisel/tool_failure.reason`
//! maps to `is_error`/`status:"error"`/`details.failure_reason`, and
//! `chisel/user_question_answers` (the question dialog's answers — content begins "User answered
//! your questions:") is carried verbatim.
//!
//! Because the columns accreted over migrations, we probe `PRAGMA table_info` and only SELECT what
//! exists (`title`, `main_chain_id`, `cogs_json`, `workspace_dirs`, `hidden`, `metadata`, and
//! `message_nodes.metadata` are all later additions).
//!
//! ## The forest
//!
//! `message_nodes` is a **forest** keyed `(session_id, node_id)`. Whenever the CLI rebuilds its
//! context (compaction, re-prefixing) it writes a NEW chain of nodes that copy earlier messages
//! (same `message_id`), recording the superseded copies in the node `metadata` at
//! `extensions["compact/prior_node_ids"]`. A session's transcript is therefore the path from
//! `sessions.main_chain_id` up `parent_node_id` to a root, reversed — every node not on that chain
//! is a copy or an abandoned branch and is not emitted (the totals ride in
//! `extra["devin"]["node_count"]` / `chain_len` so the loss is visible). When `main_chain_id` is
//! NULL (older schema, or a session that never finished a turn) the head falls back to the largest
//! `node_id` and `extra["devin"]["main_chain_fallback"] = true` marks it.
//!
//! Node `metadata`: `{summarized_from: null|<node_id>, num_tokens_preceding: null|int,
//! is_system_prefix: null|true, extensions: {"compact/prior_node_ids": [ints]}}`.
//! `is_system_prefix: true` marks the system-prompt nodes (the "You are Devin…" prompt, subagent
//! profiles, parallel-tool-calls text, "You are powered by …").
//!
//! ## chat_message
//!
//! `{message_id: uuid, role: "system"|"user"|"assistant"|"tool", content: string}` plus per-role
//! fields (a nested `metadata` object — not the node's `metadata` column):
//! * **assistant**: `tool_calls: [{id: "exec_0_…#…", name, arguments: {…}, index, kind:
//!   "function"}]`, `thinking: {thinking, signature: "sealed.v1…", signature_type: "sealed"}`.
//!   `metadata`: `{num_tokens, request_id, metrics: {ttft_ms, total_time_ms, input_tokens,
//!   output_tokens, cache_read_tokens, cache_creation_tokens, tpot_ms, tokens_per_sec},
//!   finish_reason: "tool_calls"|"stop"|…, extensions: {"chisel/tool_call_content": {<id>:
//!   {toolCallId, title, status, kind, rawInput, content, _meta}}}, response_dimensions,
//!   started_generation_at, created_at (RFC3339), generation_model: "swe-2-high", telemetry:
//!   {source: "assistant", operation: "inference"}}`.
//! * **tool**: `tool_call_id` (matches `tool_calls[].id`), `content` = result text. `metadata.
//!   extensions`: `chisel/tool_call_timing {started_at, finished_at, duration_ms}`,
//!   `chisel/tool_result_meta {success, kind}`, `chisel/terminal_output {text, cwd, exit:
//!   {terminal_id, exit_code}}` (exec only — its `text` duplicates `content`, not copied),
//!   `chisel/undo [{kind, tool_name, description}]`. `telemetry.source = "tool_result"`.
//! * **user**: `metadata.is_user_input: true` marks typed prompts; `extensions
//!   ["chisel/client-message-id"]`; `metadata.created_at`.
//! * **system**: non-prefix nodes are `<system_info>…</system_info>` (with
//!   `metadata.extensions["affogato/cog-context"] = {key: "workspace/context"}`) and `<rules
//!   type="always-on">…` (CLAUDE.md/AGENTS.md rules). `telemetry.source = "system"`.
//!
//! ## IR mapping
//!
//! prefix `system` → [`MessageKind::SystemPrompt`]; other `system` and non-input `user` →
//! [`MessageKind::InjectedContext`]; `user`/`is_user_input` → Prompt/Human — or Prompt/Subagent
//! when the row is a sub-agent handoff (`subagent/handoff`, see above); `assistant` →
//! Thinking + Text + one ToolUse per `tool_calls[]` (metrics → [`Usage`], `generation_model` →
//! `model`); `tool` → [`Role::Tool`]/ToolResult (`tool_name` paired from the earlier call,
//! `is_error` = `!tool_result_meta.success`, timing/cwd/exit in `details`). A node with
//! `summarized_from` non-null is a [`MessageKind::CompactionSummary`] regardless of role (the
//! compaction writer's mark; observed on compacted system/assistant nodes). `sessions.model` ==
//! "" falls back to the last chain assistant's `generation_model` (observed on a real session). `hidden = 1` sessions are
//! still discovered, flagged in `extra["devin"]["hidden"]`.

use super::{note_skipped_lines, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Devin {
    /// The `~/.local/share/devin/cli` dir (candidate — may not exist).
    root: PathBuf,
}

impl Devin {
    pub fn new() -> Self {
        Devin { root: data_dir() }
    }

    /// `sessions.db` inside the root, when it exists.
    fn db_path(&self) -> Option<PathBuf> {
        if self.root.as_os_str().is_empty() {
            return None;
        }
        let p = self.root.join("sessions.db");
        p.exists().then_some(p)
    }
}

/// The literal path Devin CLI uses (even on macOS — not Application Support): the XDG data dir's
/// `devin/cli`, honouring `$XDG_DATA_HOME` when set.
fn data_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("devin").join("cli");
    }
    dirs::home_dir()
        .map(|h| h.join(".local/share/devin/cli"))
        .unwrap_or_default()
}

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))
}

impl Default for Devin {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Devin {
    fn harness(&self) -> Harness {
        Harness::Devin
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.db_path().map(|_| self.root.clone())
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(db) = self.db_path() else {
            return Ok(vec![]);
        };
        let Ok(conn) = open_ro(&db) else {
            return Ok(vec![]);
        };
        discover_conn(&conn, &db)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let conn = open_ro(&r.path)?;
        stream_conn(&conn, r, sink)
    }
}

// ---------------------------------------------------------------------------
// Schema probing + the chain walk
// ---------------------------------------------------------------------------

/// Columns present on a table (for schema-drift tolerance — the columns accreted per migration).
fn columns(conn: &Connection, table: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            for n in rows.flatten() {
                set.insert(n);
            }
        }
    }
    set
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// A session's node topology and (where decodable) roles. Topology comes from the plain columns —
/// never the JSON — so a corrupt `chat_message` can't truncate the chain walk; `roles` is filled by
/// a second query where a corrupt row is simply absent (its `json_extract` fails per-row).
#[derive(Default)]
struct Forest {
    parents: HashMap<i64, Option<i64>>,
    roles: HashMap<i64, String>,
}

fn load_forest(conn: &Connection, session_id: &str) -> Forest {
    let mut f = Forest::default();
    if let Ok(mut stmt) =
        conn.prepare("SELECT node_id, parent_node_id FROM message_nodes WHERE session_id = ?1")
    {
        if let Ok(rows) = stmt.query_map([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        }) {
            for r in rows.flatten() {
                f.parents.insert(r.0, r.1);
            }
        }
    }
    // `json_valid` first: a corrupt chat_message makes bare `json_extract` abort the whole scan,
    // which would silently drop the roles of every node past the bad row.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT node_id, CASE WHEN json_valid(chat_message) \
         THEN json_extract(chat_message, '$.role') END FROM message_nodes WHERE session_id = ?1",
    ) {
        if let Ok(rows) = stmt.query_map([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        }) {
            for r in rows.flatten() {
                if let Some(role) = r.1 {
                    f.roles.insert(r.0, role);
                }
            }
        }
    }
    f
}

/// Walk from `head` up `parent_node_id` to a root and reverse it into transcript order. Cycles
/// (shouldn't exist, but the file is live) end the walk.
fn chain_to_root(parents: &HashMap<i64, Option<i64>>, head: i64) -> Vec<i64> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = Some(head);
    while let Some(n) = cur {
        if !seen.insert(n) {
            break;
        }
        chain.push(n);
        cur = parents.get(&n).copied().flatten();
    }
    chain.reverse();
    chain
}

/// The chain head: `main_chain_id` when it names a real node, else the largest `node_id` (a
/// session that never completed a turn, or a schema without the column). `true` on the return
/// flags the fallback.
fn chain_head(f: &Forest, main_chain_id: Option<i64>) -> (Option<i64>, bool) {
    if let Some(id) = main_chain_id {
        if f.parents.contains_key(&id) {
            return (Some(id), false);
        }
    }
    (f.parents.keys().max().copied(), true)
}

/// `(agent_id, chain_node_id, updated_at)` per `subagent_heads` row for a session (empty when the
/// table is absent — it arrived late, in migration `subagent_heads`).
fn subagent_head_rows(conn: &Connection, session_id: &str) -> Vec<(String, i64, Option<i64>)> {
    if !table_exists(conn, "subagent_heads") {
        return Vec::new();
    }
    let mut stmt = match conn.prepare(
        "SELECT agent_id, chain_node_id, updated_at FROM subagent_heads WHERE session_id = ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let Ok(rows) = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2).ok().flatten(),
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// user + assistant turns on `head`'s chain — the [`SessionRef::message_count`] contract
/// (ir.rs:793): superseded copies and abandoned branches elsewhere in the forest don't count.
fn chain_message_count(f: &Forest, head: Option<i64>) -> usize {
    head.map(|h| {
        chain_to_root(&f.parents, h)
            .iter()
            .filter(|n| matches!(f.roles.get(*n).map(String::as_str), Some("user" | "assistant")))
            .count()
    })
    .unwrap_or(0)
}

/// One child [`SessionRef`] per live `subagent_heads` row: id `<session_id>/<agent_id>`, listed
/// right after its parent, with cwd/created_at inherited from the parent row and `updated_at`
/// from the head row (the head moves as the agent works). A head whose `chain_node_id` is no
/// longer a live node is skipped (counted under the parent's
/// `extra["devin"]["dangling_subagent_heads"]` at parse time).
fn child_ref(
    db: &Path,
    session_id: &str,
    agent: &str,
    head: i64,
    updated: Option<i64>,
    forest: &Forest,
    parent: &SessionRef,
) -> Option<SessionRef> {
    if !forest.parents.contains_key(&head) {
        return None;
    }
    let title = match &parent.title {
        Some(t) => format!("[{agent}] {t}"),
        None => format!("[{agent}]"),
    };
    Some(SessionRef {
        id: format!("{session_id}/{agent}"),
        harness: Harness::Devin,
        path: db.to_path_buf(),
        cwd: parent.cwd.clone(),
        title: Some(crate::ir::truncate(&title, 80)),
        created_at: parent.created_at,
        updated_at: updated.and_then(secs_to_dt).or(parent.updated_at),
        message_count: chain_message_count(forest, Some(head)),
    })
}

fn discover_conn(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let cols = columns(conn, "sessions");
    let has = |c: &str| cols.contains(c);
    let sel = |c: &str| if has(c) { c.to_string() } else { "NULL".to_string() };
    let sql = format!(
        "SELECT id, working_directory, {}, created_at, last_activity_at, {} \
         FROM sessions ORDER BY last_activity_at DESC",
        sel("title"),
        sel("main_chain_id"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1).ok().flatten(),
            row.get::<_, Option<String>>(2).ok().flatten(),
            row.get::<_, Option<i64>>(3).ok().flatten(),
            row.get::<_, Option<i64>>(4).ok().flatten(),
            row.get::<_, Option<i64>>(5).ok().flatten(),
        ))
    })?;
    let mut out = Vec::new();
    for (id, wd, title, created, last, main_chain_id) in rows.flatten() {
        // One forest load serves the parent's count AND its sub-agent children's.
        let forest = load_forest(conn, &id);
        let (head, _) = chain_head(&forest, main_chain_id);
        let parent = SessionRef {
            id: id.clone(),
            harness: Harness::Devin,
            path: db.to_path_buf(),
            cwd: wd.filter(|s| !s.is_empty()).map(PathBuf::from),
            title: title
                .filter(|s| !s.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, 80)),
            created_at: created.and_then(secs_to_dt),
            updated_at: last.and_then(secs_to_dt).or_else(|| created.and_then(secs_to_dt)),
            message_count: chain_message_count(&forest, head),
        };
        out.push(parent.clone());
        for (agent, head, updated) in subagent_head_rows(conn, &id) {
            if let Some(r) = child_ref(db, &id, &agent, head, updated, &forest, &parent) {
                out.push(r);
            }
        }
    }
    Ok(out)
}

/// The sub-agent chains a session's `subagent_heads` table names — the same rows [`discover_conn`]
/// turns into `<session_id>/<agent_id>` child refs — as the forest [`crate::subagent_tree_of`]
/// surfaces. `agent_id` is the child's `agent_type`; Devin records no per-agent description or
/// spawning tool_use id.
pub fn subagent_tree(db: &Path, session_id: &str) -> Vec<crate::harness::claude::SubagentInfo> {
    let Ok(conn) = open_ro(db) else {
        return Vec::new();
    };
    let cols = columns(&conn, "sessions");
    let sel = |c: &str| if cols.contains(c) { c.to_string() } else { "NULL".to_string() };
    let (wd, title, created, last): (Option<String>, Option<String>, Option<i64>, Option<i64>) = conn
        .query_row(
            &format!(
                "SELECT working_directory, {}, created_at, last_activity_at FROM sessions WHERE id = ?1",
                sel("title")
            ),
            [session_id],
            |row| {
                Ok((
                    row.get(0).ok().flatten(),
                    row.get(1).ok().flatten(),
                    row.get(2).ok().flatten(),
                    row.get(3).ok().flatten(),
                ))
            },
        )
        .unwrap_or_default();
    let parent = SessionRef {
        id: session_id.to_string(),
        harness: Harness::Devin,
        path: db.to_path_buf(),
        cwd: wd.filter(|s| !s.is_empty()).map(PathBuf::from),
        title,
        created_at: created.and_then(secs_to_dt),
        updated_at: last.and_then(secs_to_dt),
        message_count: 0,
    };
    let forest = load_forest(&conn, session_id);
    let mut out = Vec::new();
    for (agent, head, updated) in subagent_head_rows(&conn, session_id) {
        if let Some(session) = child_ref(db, session_id, &agent, head, updated, &forest, &parent) {
            out.push(crate::harness::claude::SubagentInfo {
                session,
                agent_type: Some(agent),
                description: None,
                tool_use_id: None,
                workflow: None,
                result_status: None,
                result_summary: None,
            });
        }
    }
    out
}

/// Row shape of the session metadata SELECT (columns probed — absent ones come back NULL):
/// working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title,
/// main_chain_id, cogs_json, workspace_dirs, hidden, metadata.
type SessionRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

fn stream_conn(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    // Child ids split on the FIRST '/': `<session_id>/<agent_id>` — slugs never contain one.
    let (session_id, agent_id) = match r.id.split_once('/') {
        Some((s, a)) => (s, Some(a)),
        None => (r.id.as_str(), None),
    };
    stream_session(conn, session_id, agent_id, r, sink)
}

/// The one chain walk / node map / meta path behind [`Adapter::stream`] for both a session and
/// each of its sub-agent children: same forest, same [`sessions`] row — `agent` only changes which
/// head is walked (`subagent_heads.chain_node_id` instead of `main_chain_id`) and what lineage /
/// extra the emitted session carries.
fn stream_session(
    conn: &Connection,
    session_id: &str,
    agent: Option<&str>,
    r: &SessionRef,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let cols = columns(conn, "sessions");
    let has = |c: &str| cols.contains(c);
    let sel = |c: &str| if has(c) { c.to_string() } else { "NULL".to_string() };
    let sql = format!(
        "SELECT working_directory, backend_type, model, agent_mode, created_at, last_activity_at, \
         {}, {}, {}, {}, {}, {} FROM sessions WHERE id = ?1",
        sel("title"),
        sel("main_chain_id"),
        sel("cogs_json"),
        sel("workspace_dirs"),
        sel("hidden"),
        sel("metadata"),
    );
    let (wd, backend, model, agent_mode, created, last, title, main_chain_id, cogs, wdirs, hidden, smeta): SessionRow =
        conn.query_row(&sql, [session_id], |row| {
            Ok((
                row.get(0).ok().flatten(),
                row.get(1).ok().flatten(),
                row.get(2).ok().flatten(),
                row.get(3).ok().flatten(),
                row.get(4).ok().flatten(),
                row.get(5).ok().flatten(),
                row.get(6).ok().flatten(),
                row.get(7).ok().flatten(),
                row.get(8).ok().flatten(),
                row.get(9).ok().flatten(),
                row.get(10).ok().flatten(),
                row.get(11).ok().flatten(),
            ))
        })
        .unwrap_or_default();

    // The chosen chain (see the module doc): head → root, reversed. For a child the head comes
    // from `subagent_heads.chain_node_id` — a head naming no live node leaves the chain empty
    // (counted as dangling rather than falling back, which would pick some other chain's tail).
    let forest = load_forest(conn, session_id);
    let heads = subagent_head_rows(conn, session_id);
    let (head, fell_back, dangling_head) = match agent {
        None => {
            let (head, fb) = chain_head(&forest, main_chain_id);
            (head, fb, None)
        }
        Some(a) => match heads.iter().find(|(id, _, _)| id == a) {
            None => anyhow::bail!("no devin sub-agent {a:?} in session {session_id:?}"),
            Some((_, head, _)) => (
                forest.parents.contains_key(head).then_some(*head),
                false,
                Some(*head),
            ),
        },
    };
    let chain = head.map(|h| chain_to_root(&forest.parents, h)).unwrap_or_default();

    // For a child, the tool call that spawned it: the parent's `tool` row carrying
    // `subagent/agent_id` — its `tool_call_id` names the spawning call. One indexed query;
    // absent on sessions without a completion row.
    let spawned_by = agent.and_then(|a| {
        conn.query_row(
            "SELECT json_extract(chat_message, '$.tool_call_id') FROM message_nodes \
             WHERE session_id = ?1 AND json_valid(chat_message) \
             AND json_extract(chat_message, '$.role') = 'tool' \
             AND json_extract(chat_message, '$.metadata.extensions.\"subagent/agent_id\"') = ?2",
            rusqlite::params![session_id, a],
            |row| row.get::<_, String>(0),
        )
        .ok()
    });

    // Per-chain-node metadata column (added in `add_node_metadata` — probe before selecting).
    let node_meta: HashMap<i64, Value> = if columns(conn, "message_nodes").contains("metadata") {
        let mut m = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT node_id, metadata FROM message_nodes WHERE session_id = ?1",
        ) {
            if let Ok(rows) = stmt.query_map([session_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            }) {
                for (nid, raw) in rows.flatten() {
                    // The column holds JSON, or the literal string 'null'.
                    if let Some(v) = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                        if v.is_object() {
                            m.insert(nid, v);
                        }
                    }
                }
            }
        }
        m
    } else {
        HashMap::new()
    };

    // A compaction writes TWO chain nodes sharing one `summarized_from` (the old window's head —
    // still in the forest): an assistant node whose content is the summary markdown, and a system
    // node wrapping it ("You are continuing work…") that carries the sidecar extensions
    // (`devin-rs/summary`, `compact/edited_files`, `compact/todo_list`, `subagent/handoff_history`)
    // plus a "Full conversation history saved at <path>." line pointing at
    // `summaries/<agent>/history_<hex>.md`. Gather the per-`summarized_from` facts now (≤2 node
    // bodies per compaction) so the synthetic boundary can carry them before the first summary
    // node is emitted. `handoff_history` is deliberately dropped — it duplicates the handoffs.
    let mut compaction_facts: HashMap<i64, serde_json::Map<String, Value>> = HashMap::new();
    for &nid in &chain {
        let Some(sf) = node_meta
            .get(&nid)
            .and_then(|m| m.get("summarized_from"))
            .and_then(Value::as_i64)
        else {
            continue;
        };
        let entry = compaction_facts.entry(sf).or_insert_with(|| {
            let mut o = serde_json::Map::new();
            o.insert("summarized_from".into(), json!(sf));
            o.insert("pre_chain_head".into(), json!(sf));
            o
        });
        if let Some(ntp) = node_meta
            .get(&nid)
            .and_then(|m| m.get("num_tokens_preceding"))
            .and_then(Value::as_u64)
        {
            // The window's token cost: the larger `num_tokens_preceding` of the pair.
            let cur = entry.get("pre_tokens").and_then(Value::as_u64).unwrap_or(0);
            entry.insert("pre_tokens".into(), json!(cur.max(ntp)));
        }
        let raw: Option<String> = conn
            .query_row(
                "SELECT chat_message FROM message_nodes WHERE session_id = ?1 AND node_id = ?2",
                rusqlite::params![session_id, nid],
                |row| row.get(0),
            )
            .ok();
        let Some(raw) = raw else { continue };
        let Ok(chat) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let ext = chat.get("metadata").and_then(|m| m.get("extensions"));
        let get = |k: &str| ext.and_then(|e| e.get(k));
        if let Some(src) = get("devin-rs/summary")
            .and_then(|v| v.get("source"))
            .filter(|v| !v.is_null())
        {
            entry.insert("source".into(), src.clone());
        }
        if let Some(paths) = get("compact/edited_files").and_then(|v| v.get("paths")) {
            entry.insert("edited_files".into(), paths.clone());
        }
        if let Some(todos) = get("compact/todo_list").filter(|v| !v.is_null()) {
            entry.insert("todo_list".into(), todos.clone());
        }
        if let Some(content) = chat.get("content").and_then(Value::as_str) {
            // `Full conversation history saved at <path>.` — the sentence's trailing period is
            // not part of the path.
            if let Some(p) = content
                .split("Full conversation history saved at ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(|s| s.trim_end_matches('.'))
                .filter(|s| !s.is_empty())
            {
                entry.insert("history_path".into(), json!(p));
            }
        }
    }

    // The system prompt = the chain's `is_system_prefix` nodes' contents, in order. (Content is
    // fetched lazily per node here — only the prefix nodes' bodies are read ahead of the stream.)
    let mut prefix: Vec<String> = Vec::new();
    for &nid in &chain {
        let is_prefix = node_meta
            .get(&nid)
            .and_then(|m| m.get("is_system_prefix"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_prefix && forest.roles.get(&nid).is_some_and(|r| r == "system") {
            let content: Option<String> = conn
                .query_row(
                    "SELECT CASE WHEN json_valid(chat_message) \
                     THEN json_extract(chat_message, '$.content') END FROM message_nodes \
                     WHERE session_id = ?1 AND node_id = ?2",
                    rusqlite::params![session_id, nid],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            if let Some(content) = content.filter(|c| !c.is_empty()) {
                prefix.push(content);
            }
        }
    }

    // `sessions.model` can be '' (observed): then the model lives only on the turns — take the
    // LAST chain assistant's `metadata.generation_model`.
    let mut model = model.filter(|m| !m.is_empty());
    if model.is_none() {
        for &nid in chain.iter().rev() {
            if forest.roles.get(&nid).is_some_and(|r| r == "assistant") {
                let g: Option<String> = conn
                    .query_row(
                        "SELECT CASE WHEN json_valid(chat_message) \
                         THEN json_extract(chat_message, '$.metadata.generation_model') END \
                         FROM message_nodes WHERE session_id = ?1 AND node_id = ?2",
                        rusqlite::params![session_id, nid],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                if let Some(g) = g.filter(|s| !s.is_empty()) {
                    model = Some(g);
                    break;
                }
            }
        }
    }

    let schema_version: Option<i64> = if table_exists(conn, "refinery_schema_history") {
        conn.query_row("SELECT MAX(version) FROM refinery_schema_history", [], |row| {
            row.get(0)
        })
        .ok()
        .flatten()
    } else {
        None
    };

    // The session's own fact bag (everything harness-specific nests under "devin").
    let mut extra = serde_json::Map::new();
    {
        let mut bag = serde_json::Map::new();
        let put = |bag: &mut serde_json::Map<String, Value>, k: &str, v: Option<Value>| {
            if let Some(v) = v {
                bag.insert(k.into(), v);
            }
        };
        put(&mut bag, "backend_type", backend.filter(|s| !s.is_empty()).map(Value::String));
        put(&mut bag, "agent_mode", agent_mode.filter(|s| !s.is_empty()).map(Value::String));
        if let Some(w) = &wdirs {
            put(
                &mut bag,
                "workspace_dirs",
                Some(serde_json::from_str(w).unwrap_or_else(|_| json!(w))),
            );
        }
        if let Some(h) = hidden {
            bag.insert("hidden".into(), Value::Bool(h != 0));
        }
        put(&mut bag, "main_chain_id", main_chain_id.map(|i| json!(i)));
        put(&mut bag, "schema_version", schema_version.map(|v| json!(v)));
        bag.insert("node_count".into(), json!(forest.parents.len()));
        bag.insert("chain_len".into(), json!(chain.len()));
        if fell_back && !chain.is_empty() {
            bag.insert("main_chain_fallback".into(), Value::Bool(true));
        }
        for (k, raw) in [("cogs_json", cogs), ("metadata", smeta)] {
            if let Some(v) = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                if !v.is_null() {
                    bag.insert(k.into(), v);
                }
            }
        }
        match agent {
            // Parent: list its live sub-agent heads and count the dangling ones (a head whose
            // chain_node_id names no live node — its child is skipped in discovery).
            None => {
                let mut live = Vec::new();
                let mut dangling = 0u64;
                for (agent_id, chain_node_id, _) in &heads {
                    if forest.parents.contains_key(chain_node_id) {
                        live.push(json!({"agent_id": agent_id, "chain_node_id": chain_node_id}));
                    } else {
                        dangling += 1;
                    }
                }
                if !live.is_empty() {
                    bag.insert("subagent_heads".into(), Value::Array(live));
                }
                if dangling > 0 {
                    bag.insert("dangling_subagent_heads".into(), json!(dangling));
                }
            }
            // Child: which head it was parsed from — not its own subagent_heads copy.
            Some(a) => {
                bag.insert("agent_id".into(), json!(a));
                if let Some(h) = dangling_head {
                    bag.insert("chain_head".into(), json!(h));
                    if head.is_none() {
                        bag.insert("dangling_chain_head".into(), Value::Bool(true));
                    }
                }
            }
        }
        extra.insert(Harness::Devin.as_str().into(), Value::Object(bag));
    }

    // Children: title "[agent] <parent title>", updated_at from the head row (which moves with
    // the agent's work), lineage pointing at the parent session.
    let head_updated = agent.and_then(|a| {
        heads
            .iter()
            .find(|(id, _, _)| id == a)
            .and_then(|(_, _, u)| *u)
            .and_then(secs_to_dt)
    });
    let title = match (agent, title.filter(|t| !t.trim().is_empty())) {
        (Some(a), Some(t)) => Some(format!("[{a}] {t}")),
        (Some(a), None) => Some(format!("[{a}]")),
        (None, t) => t.or_else(|| r.title.clone()),
    };
    let mut s = Session {
        id: r.id.clone(),
        harness: Harness::Devin,
        cwd: wd
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title,
        created_at: created.and_then(secs_to_dt).or(r.created_at),
        updated_at: head_updated.or_else(|| last.and_then(secs_to_dt)).or(r.updated_at),
        model,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra,
        system_prompt: (!prefix.is_empty()).then(|| prefix.join("\n\n")),
        lineage: match agent {
            Some(a) => crate::ir::Lineage {
                parent: Some(session_id.to_string()),
                agent_path: Some(a.to_string()),
                spawned_by_tool_use: spawned_by,
                ..Default::default()
            },
            None => crate::ir::Lineage::default(),
        },
    };
    sink.meta(&s);

    // One chain node → one message, streamed: fetch each node's `chat_message` only as it is
    // emitted, so peak is one message, not the whole transcript.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut last_msg_id: Option<String> = None;
    let mut last_sf: Option<i64> = None;
    let mut skipped = 0u64;
    let mut stmt = conn.prepare(
        "SELECT chat_message, created_at FROM message_nodes WHERE session_id = ?1 AND node_id = ?2",
    )?;
    for &nid in &chain {
        let row: Option<(String, i64)> = stmt
            .query_row(rusqlite::params![session_id, nid], |row| Ok((row.get(0)?, row.get(1)?)))
            .ok();
        let Some((raw, node_created)) = row else {
            skipped += 1;
            continue;
        };
        let Ok(chat) = serde_json::from_str::<Value>(&raw) else {
            skipped += 1;
            continue;
        };
        let mut m = node_to_message(nid, node_created, &chat, node_meta.get(&nid), &mut tool_names);

        // The first node of a compaction pair: emit a synthetic boundary right before it and
        // point the summary's parent_id at the boundary (compaction.rs pairs them by uuid).
        let sf = node_meta
            .get(&nid)
            .and_then(|n| n.get("summarized_from"))
            .and_then(Value::as_i64);
        let mut parent = last_msg_id.clone();
        if let Some(sf) = sf {
            if last_sf != Some(sf) {
                last_sf = Some(sf);
                let bid = format!("{}#boundary", m.id.clone().unwrap_or_else(|| format!("node{nid}")));
                let mut b = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
                b.id = Some(bid.clone());
                b.timestamp = m.timestamp;
                b.content = vec![Block::Text {
                    text: "context compacted".into(),
                }];
                {
                    let bag = b.harness_extra_mut(Harness::Devin);
                    if let Some(facts) = compaction_facts.get(&sf) {
                        bag.insert("compaction".into(), Value::Object(facts.clone()));
                        // The sink reads compactMetadata off the boundary (any harness bag):
                        // trigger/preTokens/durationMs — what Devin records, in its words.
                        let mut cm = serde_json::Map::new();
                        if let Some(src) = facts.get("source") {
                            cm.insert("trigger".into(), src.clone());
                        }
                        if let Some(ntp) = facts.get("pre_tokens") {
                            cm.insert("preTokens".into(), ntp.clone());
                        }
                        if !cm.is_empty() {
                            bag.insert("compactMetadata".into(), Value::Object(cm));
                        }
                    }
                }
                if sink.message(b) == Flow::Stop {
                    break;
                }
                parent = Some(bid);
            }
            // The full-history path belongs on the summary node too, not just the boundary.
            if let Some(hp) = compaction_facts.get(&sf).and_then(|f| f.get("history_path")) {
                m.harness_extra_mut(Harness::Devin)
                    .insert("history_path".into(), hp.clone());
            }
        }
        m.parent_id = parent;
        if m.id.is_some() {
            last_msg_id = m.id.clone();
        }
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    note_skipped_lines(&mut s, skipped);
    Ok(s)
}

// ---------------------------------------------------------------------------
// One chain node → one Message
// ---------------------------------------------------------------------------

fn node_to_message(
    node_id: i64,
    node_created: i64,
    chat: &Value,
    node_meta: Option<&Value>,
    tool_names: &mut HashMap<String, String>,
) -> Message {
    let role = chat.get("role").and_then(Value::as_str).unwrap_or("");
    // The message's own metadata object (inside `chat_message`, distinct from the node column).
    let mmeta = chat.get("metadata").cloned().unwrap_or(Value::Null);
    let ext = mmeta.get("extensions").cloned().unwrap_or(Value::Null);
    let is_prefix = node_meta
        .and_then(|m| m.get("is_system_prefix"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_user_input = mmeta.get("is_user_input").and_then(Value::as_bool).unwrap_or(false);
    // The lead's brief to a sub-agent is a `user` prompt typed by another agent.
    let is_handoff = ext
        .get("subagent/handoff")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut m = match (role, is_prefix, is_user_input) {
        ("system", true, _) => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
        ("system", false, _) => {
            Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness)
        }
        ("user", _, true) if is_handoff => {
            Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent)
        }
        ("user", _, true) => Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human),
        // A `user` row the human didn't type is harness-injected context (same modeling Goose
        // uses for `userVisible: false` rows).
        ("user", _, false) => Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness),
        ("assistant", _, _) => Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model),
        ("tool", _, _) => Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness),
        _ => Message::of_kind(Role::System, MessageKind::Notice, Origin::Harness),
    };
    // Compaction marks the node, whatever the role it copied into (observed live: system nodes
    // carrying `subagent/handoff_history`/`compact/*`/`devin-rs/summary` extension sidecars).
    if node_meta.and_then(|n| n.get("summarized_from")).is_some_and(|v| !v.is_null()) {
        m.kind = MessageKind::CompactionSummary;
    }

    m.id = chat
        .get("message_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    m.timestamp = mmeta
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(super::parse_ts)
        .or_else(|| secs_to_dt(node_created));

    let content = chat.get("content").and_then(Value::as_str).unwrap_or("");
    match role {
        "assistant" => {
            if let Some(th) = chat.get("thinking") {
                let text = th.get("thinking").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    m.content.push(Block::Thinking {
                        text: text.to_string().into(),
                        signature: th.get("signature").and_then(Value::as_str).map(str::to_string),
                        encrypted: None,
                        redacted: false,
                    });
                }
            }
            if !content.is_empty() {
                m.content.push(Block::Text {
                    text: content.to_string().into(),
                });
            }
            if let Some(calls) = chat.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let input = c.get("arguments").cloned().unwrap_or(Value::Null);
                    if !id.is_empty() && !name.is_empty() {
                        tool_names.insert(id.clone(), name.clone());
                    }
                    m.content.push(Block::ToolUse {
                        id,
                        name,
                        input,
                        namespace: None,
                    });
                }
            }
            m.model = mmeta
                .get("generation_model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if let Some(metrics) = mmeta.get("metrics") {
                let get = |k: &str| metrics.get(k).and_then(Value::as_u64);
                let u = Usage {
                    input_tokens: get("input_tokens"),
                    output_tokens: get("output_tokens"),
                    cache_read_tokens: get("cache_read_tokens"),
                    cache_creation_tokens: get("cache_creation_tokens"),
                    ..Default::default()
                };
                if u.input_tokens.is_some() || u.output_tokens.is_some() {
                    m.usage = Some(u);
                }
            }
        }
        "tool" => {
            let tool_use_id = chat
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let success = ext
                .get("chisel/tool_result_meta")
                .and_then(|v| v.get("success"))
                .and_then(Value::as_bool);
            // `chisel/tool_failure` is a failure even without a tool_result_meta verdict.
            let failure_reason = ext
                .get("chisel/tool_failure")
                .and_then(|v| v.get("reason"))
                .and_then(Value::as_str);
            let mut details = serde_json::Map::new();
            if let Some(r) = failure_reason {
                details.insert("failure_reason".into(), json!(r));
            }
            if let Some(term) = ext.get("chisel/terminal_output") {
                if let Some(cwd) = term.get("cwd") {
                    details.insert("cwd".into(), cwd.clone());
                }
                for (src, dst) in [("exit_code", "exit_code"), ("terminal_id", "terminal_id")] {
                    if let Some(v) = term.get("exit").and_then(|e| e.get(src)) {
                        details.insert(dst.into(), v.clone());
                    }
                }
            }
            if let Some(d) = ext
                .get("chisel/tool_call_timing")
                .and_then(|t| t.get("duration_ms"))
            {
                details.insert("duration_ms".into(), d.clone());
            }
            if let Some(k) = ext
                .get("chisel/tool_result_meta")
                .and_then(|v| v.get("kind"))
            {
                details.insert("kind".into(), k.clone());
            }
            m.content.push(Block::ToolResult {
                tool_name: tool_names.get(&tool_use_id).cloned(),
                tool_use_id,
                content: content.to_string().into(),
                is_error: success == Some(false) || failure_reason.is_some(),
                status: if failure_reason.is_some() {
                    Some("error".to_string())
                } else {
                    success.map(|ok| if ok { "completed" } else { "error" }.to_string())
                },
                details: (!details.is_empty()).then(|| Value::Object(details)),
            });
        }
        _ => {
            if !content.is_empty() {
                m.content.push(Block::Text {
                    text: content.to_string().into(),
                });
            }
        }
    }

    // Harness facts that have no first-class IR home: the node's position in the forest and the
    // metadata both layers record. Only present keys are written.
    {
        let bag = m.harness_extra_mut(Harness::Devin);
        bag.insert("node_id".into(), json!(node_id));
        for k in ["finish_reason", "request_id", "telemetry", "num_tokens"] {
            if let Some(v) = mmeta.get(k).filter(|v| !v.is_null()) {
                bag.insert(k.into(), v.clone());
            }
        }
        if let Some(nm) = node_meta {
            if let Some(v) = nm.get("num_tokens_preceding").filter(|v| !v.is_null()) {
                bag.insert("num_tokens_preceding".into(), v.clone());
            }
            if let Some(v) = nm.get("summarized_from").filter(|v| !v.is_null()) {
                bag.insert("summarized_from".into(), v.clone());
            }
            if let Some(v) = nm
                .get("extensions")
                .and_then(|e| e.get("compact/prior_node_ids"))
                .filter(|v| !v.is_null())
            {
                bag.insert("prior_node_ids".into(), v.clone());
            }
        }
        if let Some(v) = ext.get("affogato/cog-context").filter(|v| !v.is_null()) {
            bag.insert("cog_context".into(), v.clone());
        }
        // The tool row that answers an in-band question dialog (and `cv prompts` reads).
        if let Some(v) = ext
            .get("chisel/user_question_answers")
            .filter(|v| !v.is_null())
        {
            bag.insert("user_question_answers".into(), v.clone());
        }
        // Which sub-agent a completion row is about: the parent's `sidekick` tool result and the
        // `<subagent_completion_notification>` system row both carry these.
        {
            let mut sub = serde_json::Map::new();
            for (k, dst) in [
                ("subagent/profile_name", "profile_name"),
                ("subagent/model", "model"),
                ("subagent/agent_id", "agent_id"),
                ("subagent/chain_node_id", "chain_node_id"),
            ] {
                if let Some(v) = ext.get(k).filter(|v| !v.is_null()) {
                    sub.insert(dst.into(), v.clone());
                }
            }
            if !sub.is_empty() {
                bag.insert("subagent".into(), Value::Object(sub));
            }
        }
        // Sub-agent wiring: the lead's model uid on a handoff prompt, and the rules/skills the
        // child's system prefix says it loaded (agent-ext/*).
        if let Some(v) = ext
            .get("chisel/fusion_lead_model_uid")
            .filter(|v| !v.is_null())
        {
            bag.insert("lead_model".into(), v.clone());
        }
        if let Some(v) = ext.get("agent-ext/rules-loaded").filter(|v| !v.is_null()) {
            bag.insert("rules_loaded".into(), v.clone());
        }
        if let Some(skills) = ext
            .get("agent-ext/skills-loaded")
            .and_then(|v| v.get("skills"))
            .and_then(Value::as_array)
        {
            let reduced: Vec<Value> = skills
                .iter()
                .map(|s| {
                    let mut o = serde_json::Map::new();
                    for k in ["name", "path"] {
                        if let Some(v) = s.get(k) {
                            o.insert(k.into(), v.clone());
                        }
                    }
                    Value::Object(o)
                })
                .collect();
            bag.insert("skills_loaded".into(), Value::Array(reduced));
        }
        // The ACP view of the calls, reduced to title/kind/status — `content`/`rawInput`/`_meta`
        // duplicate the ToolUse block and the result row.
        if let Some(tcc) = ext.get("chisel/tool_call_content").and_then(Value::as_object) {
            let mut reduced = serde_json::Map::new();
            for (id, v) in tcc {
                let mut o = serde_json::Map::new();
                for k in ["title", "kind", "status"] {
                    if let Some(val) = v.get(k) {
                        o.insert(k.into(), val.clone());
                    }
                }
                reduced.insert(id.clone(), Value::Object(o));
            }
            bag.insert("tool_call_content".into(), Value::Object(reduced));
        }
    }
    m
}

/// Unix seconds → datetime (the node/session `created_at` columns).
fn secs_to_dt(s: i64) -> Option<DateTime<Utc>> {
    if s <= 0 {
        return None;
    }
    Utc.timestamp_opt(s, 0).single()
}

// ---------------------------------------------------------------------------
// Tests — `tests/fixtures/devin/sessions-v17-3000.11.3.db` (tools/harness-fixtures/devin/)
// ---------------------------------------------------------------------------

/// Whole-`Session` convenience over [`stream_conn`] for tests.
#[cfg(test)]
fn parse_db(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_conn(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/devin/sessions-v17-3000.11.3.db")
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Devin,
            path: fixture(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn dbag(s: &Session) -> &serde_json::Map<String, Value> {
        s.harness_extra(Harness::Devin).expect("extra[\"devin\"]")
    }

    fn roles(s: &Session) -> Vec<(Role, MessageKind, Origin)> {
        s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect()
    }

    #[test]
    fn discovers_sessions_and_subagent_children_with_chain_only_counts() {
        let conn = open_ro(&fixture()).unwrap();
        let refs = discover_conn(&conn, &fixture()).unwrap();
        // Alpha, its sidekick child, beta — the child sits right after its parent, and the
        // dangling 'ghost' head is skipped entirely.
        assert_eq!(
            refs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["fixture-alpha", "fixture-alpha/sidekick", "fixture-beta"]
        );
        let a = &refs[0];
        assert_eq!(a.cwd.as_deref(), Some(Path::new("/work/alpha")));
        assert_eq!(a.title.as_deref(), Some("Fixture session alpha"));
        assert!(a.created_at.is_some() && a.updated_at.is_some());
        // user+assistant ON the main chain only: the superseded copies and the two tool
        // results don't count. Chain = user(12) + assistant(13) + assistant(16) = 3.
        assert_eq!(a.message_count, 3);

        let sub = &refs[1];
        assert_eq!(sub.title.as_deref(), Some("[sidekick] Fixture session alpha"));
        assert_eq!(sub.cwd, a.cwd);
        assert_eq!(sub.created_at, a.created_at);
        assert_eq!(sub.updated_at, secs_to_dt(1_759_600_000 + 200));
        // The child's own chain: handoff user + 5 assistant (incl. the compaction summary)
        // = 6 (its tool + system rows don't).
        assert_eq!(sub.message_count, 6);

        // Beta: nodes 0 (system), 1 (user), 2 (corrupt), 3 (assistant) → 2.
        assert_eq!(refs[2].message_count, 2);
    }

    #[test]
    fn parses_a_subagent_child_chain_with_its_own_lineage() {
        let conn = open_ro(&fixture()).unwrap();
        let s = parse_db(&conn, &sref("fixture-alpha/sidekick")).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("fixture-alpha"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("sidekick"));
        assert_eq!(s.title.as_deref(), Some("[sidekick] Fixture session alpha"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work/alpha")));
        let bag = dbag(&s);
        assert_eq!(bag["agent_id"], "sidekick");
        assert_eq!(bag["chain_head"], json!(29));
        assert!(bag.get("subagent_heads").is_none());
        // The child's chain: 2 prefix + rules + skills + handoff + 2 turns + compaction pair
        // + a failed-edit turn = 13 nodes, 14 messages (the synthetic boundary inserts one).
        assert_eq!(s.messages.len(), 14);
        assert_eq!(bag["chain_len"], json!(13));
        // The child has its OWN system prompt, not the parent's.
        let sp = s.system_prompt.as_deref().unwrap();
        assert!(sp.starts_with("You are the Sidekick"));
        assert!(!sp.contains("You are Devin, an interactive"));
        // Model: sessions.model is non-empty — the child's turns still carry their own.
        assert_eq!(
            s.messages[5].model.as_deref(),
            Some("swe-2-medium"),
            "child turns keep their own generation_model"
        );
        // The handoff row is a Subagent-typed prompt carrying the lead's model uid.
        let handoff = &s.messages[4];
        assert_eq!(handoff.role, Role::User);
        assert_eq!(handoff.kind, MessageKind::Prompt);
        assert_eq!(handoff.origin, Origin::Subagent);
        assert_eq!(
            handoff.harness_extra(Harness::Devin).unwrap()["lead_model"],
            "claude-fable-5-1-medium"
        );
        // Rules loaded is the whole object; skills reduced to name/path.
        let rules = &s.messages[2];
        assert_eq!(
            rules.harness_extra(Harness::Devin).unwrap()["rules_loaded"]["rule_paths"],
            json!(["/work/alpha/CLAUDE.md"])
        );
        let skills = &s.messages[3];
        assert_eq!(
            skills.harness_extra(Harness::Devin).unwrap()["skills_loaded"],
            json!([{"name": "repo-hygiene", "path": "/skills/repo-hygiene/SKILL.md"}])
        );
        crate::harness::assert_no_flat_keys(&s);
    }

    #[test]
    fn subagent_tree_lists_the_live_head_and_skips_the_dangling_one() {
        let subs = subagent_tree(&fixture(), "fixture-alpha");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].session.id, "fixture-alpha/sidekick");
        assert_eq!(subs[0].agent_type.as_deref(), Some("sidekick"));
        assert_eq!(subs[0].session.message_count, 6);
        // ...and the parent records the ghost head.
        let conn = open_ro(&fixture()).unwrap();
        let s = parse_db(&conn, &sref("fixture-alpha")).unwrap();
        assert_eq!(dbag(&s)["dangling_subagent_heads"], json!(1));
        assert_eq!(
            dbag(&s)["subagent_heads"],
            json!([{"agent_id": "sidekick", "chain_node_id": 29}])
        );
        // Parsing the dangling child yields an empty transcript marked dangling — never some
        // other chain's tail; an agent id with no head row errors.
        let g = parse_db(&conn, &sref("fixture-alpha/ghost")).unwrap();
        assert!(g.messages.is_empty());
        assert_eq!(dbag(&g)["dangling_chain_head"], json!(true));
        assert!(parse_db(&conn, &sref("fixture-alpha/nobody")).is_err());
    }

    #[test]
    fn a_summarized_from_pair_yields_one_boundary_with_the_compaction_facts() {
        let conn = open_ro(&fixture()).unwrap();
        let s = parse_db(&conn, &sref("fixture-alpha/sidekick")).unwrap();
        // Exactly one boundary for the one summarized_from value, right before the pair.
        let b = &s.messages[8];
        assert_eq!(b.kind, MessageKind::CompactionBoundary);
        assert_eq!(b.role, Role::System);
        let bid = b.id.as_deref().unwrap();
        assert!(bid.ends_with("#boundary"));
        let bag = b.harness_extra(Harness::Devin).unwrap();
        let comp = &bag["compaction"];
        assert_eq!(comp["summarized_from"], json!(24));
        assert_eq!(comp["pre_chain_head"], json!(24));
        assert_eq!(comp["source"], "async_file_compactor");
        assert_eq!(comp["history_path"], "/work/summaries/sidekick/history_0123abcd.md");
        assert_eq!(
            comp["edited_files"],
            json!(["/work/alpha/src/lib.rs", "/work/alpha/README.md"])
        );
        assert!(comp.get("todo_list").is_some());
        // What the compaction sink reads off the boundary.
        assert_eq!(bag["compactMetadata"]["trigger"], "async_file_compactor");
        assert_eq!(bag["compactMetadata"]["preTokens"], json!(48000));

        // The pair: assistant summary first (parent_id → the boundary), then the system wrapper.
        let sum = &s.messages[9];
        assert_eq!(sum.kind, MessageKind::CompactionSummary);
        assert_eq!(sum.role, Role::Assistant);
        assert_eq!(sum.parent_id.as_deref(), Some(bid));
        let cont = &s.messages[10];
        assert_eq!(cont.kind, MessageKind::CompactionSummary);
        assert_eq!(cont.role, Role::System);
        assert_eq!(
            cont.harness_extra(Harness::Devin).unwrap()["history_path"],
            "/work/summaries/sidekick/history_0123abcd.md"
        );

        // cv compaction sees exactly one boundary, paired, with the summary text.
        let found = crate::compaction::detect_in_session(&s, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary_msg_idx, Some(9));
        assert_eq!(found[0].trigger.as_deref(), Some("async_file_compactor"));
        assert_eq!(found[0].pre_tokens, Some(48000));
        assert!(found[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("Summary of prior work"));

        // chisel/tool_failure → is_error + status + details.failure_reason (no tool_result_meta).
        let t = &s.messages[12];
        match &t.content[0] {
            Block::ToolResult { is_error, status, details, tool_name, .. } => {
                assert!(*is_error);
                assert_eq!(status.as_deref(), Some("error"));
                assert_eq!(details.as_ref().unwrap()["failure_reason"], "ValidationError");
                assert_eq!(tool_name.as_deref(), Some("edit"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parses_the_main_chain_and_drops_superseded_nodes() {
        let conn = open_ro(&fixture()).unwrap();
        let s = parse_db(&conn, &sref("fixture-alpha")).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work/alpha")));
        assert_eq!(s.model.as_deref(), Some("fusion-test-model"));
        assert_eq!(s.title.as_deref(), Some("Fixture session alpha"));
        // The 7 prefix nodes' contents joined into the session-level system prompt.
        let sp = s.system_prompt.as_deref().unwrap();
        assert!(sp.starts_with("You are Devin"));
        assert!(sp.contains("You are powered by SWE-2 High."));
        assert_eq!(sp.matches("\n\n").count(), 6);
        assert_eq!(
            roles(&s),
            vec![
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::SystemPrompt, Origin::Harness),
                (Role::System, MessageKind::InjectedContext, Origin::Harness), // <system_info>
                (Role::System, MessageKind::InjectedContext, Origin::Harness), // <rules>
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        // parent_id chains through emitted message_ids.
        let ids: Vec<Option<&str>> = s.messages.iter().map(|m| m.id.as_deref()).collect();
        for i in 1..s.messages.len() {
            assert_eq!(s.messages[i].parent_id.as_deref(), ids[i - 1]);
        }
        // The first three nodes (the abandoned pre-prefix chain) and the sidekick's chain
        // (nodes 17-29) are NOT emitted.
        assert_eq!(dbag(&s)["node_count"], json!(30));
        assert_eq!(dbag(&s)["chain_len"], json!(14));
        assert_eq!(dbag(&s)["main_chain_id"], json!(16));
        assert!(dbag(&s).get("main_chain_fallback").is_none());
        assert_eq!(dbag(&s)["agent_mode"], "bypass");
        assert_eq!(dbag(&s)["backend_type"], "windsurf");
        assert_eq!(dbag(&s)["hidden"], false);
        assert_eq!(dbag(&s)["schema_version"], json!(17));

        // The injected <system_info> node carries its cog context.
        let sys_info = &s.messages[7];
        assert_eq!(
            sys_info.harness_extra(Harness::Devin).unwrap()["cog_context"]["key"],
            "workspace/context"
        );
        // The copied nodes record what they supersede.
        assert_eq!(
            sys_info.harness_extra(Harness::Devin).unwrap()["prior_node_ids"],
            json!([0])
        );

        // Assistant turn 1: Thinking + Text + two exec ToolUse blocks, metrics → Usage.
        let a = &s.messages[10];
        assert!(matches!(&a.content[0], Block::Thinking { signature, .. } if signature.as_deref() == Some("sealed.v1.fixture")));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "I'll check both."));
        let tool_ids: Vec<&str> = a
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, .. } if name == "exec" => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ids.len(), 2);
        assert_eq!(a.model.as_deref(), Some("swe-2-high"));
        let u = a.usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (Some(100), Some(42), Some(200))
        );
        assert_eq!(
            a.harness_extra(Harness::Devin).unwrap()["tool_call_content"][tool_ids[0]]["title"],
            "Ran ls"
        );

        // Tool results: paired names, is_error from success, exit details.
        let t1 = &s.messages[11];
        let t2 = &s.messages[12];
        for (t, err) in [(t1, false), (t2, true)] {
            assert!(matches!(
                &t.content[0],
                Block::ToolResult { tool_name, is_error, .. }
                    if tool_name.as_deref() == Some("exec") && *is_error == err
            ));
        }
        match &t1.content[0] {
            Block::ToolResult { details, status, .. } => {
                assert_eq!(details.as_ref().unwrap()["exit_code"], json!(0));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            _ => unreachable!(),
        }
        assert!(s.cv_extra().is_none(), "no skipped lines on a clean chain");
        crate::harness::assert_no_flat_keys(&s);
    }

    #[test]
    fn null_main_chain_falls_back_and_corrupt_nodes_are_counted() {
        let conn = open_ro(&fixture()).unwrap();
        let s = parse_db(&conn, &sref("fixture-beta")).unwrap();
        let bag = dbag(&s);
        assert_eq!(bag["main_chain_fallback"], json!(true));
        assert_eq!(bag["hidden"], json!(true));
        // sessions.model == '' → the last chain assistant's generation_model.
        assert_eq!(s.model.as_deref(), Some("swe-2-high"));
        // Chain = nodes 0,1,2,3; node 2's chat_message is corrupt and skipped.
        assert_eq!(bag["node_count"], json!(4));
        assert_eq!(bag["chain_len"], json!(4));
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.cv_extra().unwrap()["skipped_lines"], json!(1));
        assert_eq!(
            s.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::System, Role::User, Role::Assistant]
        );
        crate::harness::assert_no_flat_keys(&s);
    }
}
