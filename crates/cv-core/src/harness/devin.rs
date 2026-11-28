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
//! discovery and counted under the parent's `extra["devin"]["dangling_subagent_heads"]`. A
//! `run_subagent` child (3000.11.3) has no `subagent_heads` row at all; its tool result row's
//! `subagent/agent_id` + `subagent/chain_node_id` name the chain instead (see `subagent_head_rows`).
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
//!   `chisel/undo [{kind, tool_name, description}]` (a `restore_file` entry's `path` →
//!   `details.restorable`). `telemetry.source = "tool_result"`.
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

mod emit;
mod record;
pub(crate) use emit::emit_staged;
pub use emit::{emit, resolve_db_path};

use super::store::sqlite::{open_ro, table_exists, Cell, Cells, Select};
use super::store::xdg_data_dir;
use super::{ts_from_epoch, Adapter, Unparsed};
use crate::ir::*;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Devin {
    /// The `~/.local/share/devin/cli` dir (candidate — may not exist).
    root: PathBuf,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Devin = Devin { root: PathBuf::new() };

impl Devin {
    pub fn detect() -> Self {
        // The literal path Devin CLI uses (even on macOS — not Application Support): the XDG data
        // dir's `devin/cli`.
        Devin {
            root: xdg_data_dir().map(|d| d.join("devin").join("cli")).unwrap_or_default(),
        }
    }

    /// The store under an explicit `devin/cli` dir (the one holding `sessions.db`).
    pub fn with_root(root: &Path) -> Self {
        Devin {
            root: root.to_path_buf(),
        }
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

impl Adapter for Devin {
    /// `agent_mode` is Devin's permission mode (`bypass`, `normal`, `accept-edits`, `plan`).
    fn session_affordances(&self, s: &Session) -> Vec<crate::affordance::Affordance> {
        match s.harness_extra(Harness::Devin).and_then(|bag| bag.get("agent_mode")) {
            Some(Value::String(mode)) => vec![crate::affordance::mode_row("agent_mode", &[mode.as_str()])],
            _ => Vec::new(),
        }
    }

    fn harness(&self) -> Harness {
        Harness::Devin
    }

    fn has_subagent_forest(&self) -> bool {
        true
    }

    fn record_forest_dependencies(&self, r: &SessionRef, guard: &mut crate::source::ReadSet) {
        guard.record_sqlite(&r.path);
    }

    /// The chains the session's `subagent_heads` table names.
    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(super::select_forest(subagent_tree(&r.path, &r.id)?, workflow, select))
    }

    fn roots(&self) -> Vec<PathBuf> {
        let installed = !self.root.as_os_str().is_empty()
            && (self.root.is_dir()
                || self.db_path().is_some()
                || self.root.parent().is_some_and(|p| p.ends_with("devin") && p.is_dir()));
        installed.then(|| self.root.clone()).into_iter().collect()
    }

    fn resume_env(&self, r: &SessionRef) -> Option<std::result::Result<super::ResumeEnv, String>> {
        const NO_STORE: &str = "no native store can be selected for this source; use the original harness/profile or port it into a supported native store";
        if r.id.contains('/') {
            return Some(Err(NO_STORE.into()));
        }
        let Ok(canonical) = r.path.canonicalize() else {
            return Some(Err(format!("{} cannot be read", r.path.display())));
        };
        let spelled = std::path::absolute(&r.path).ok();
        let target_xdg = [spelled.as_deref(), Some(canonical.as_path())]
            .into_iter()
            .flatten()
            .find_map(xdg_root_for_db)
            .filter(|xdg| {
                xdg.join("devin")
                    .join("cli")
                    .join("sessions.db")
                    .canonicalize()
                    .is_ok_and(|p| p == canonical)
            });
        let Some(target_xdg) = target_xdg else {
            return Some(Err(NO_STORE.into()));
        };
        let home_default = crate::env::home_dir().map(|h| h.join(".local").join("share"));
        let active = xdg_data_dir();
        let same = |a: &Path, b: &Path| {
            a == b
                || a.canonicalize()
                    .ok()
                    .zip(b.canonicalize().ok())
                    .is_some_and(|(x, y)| x == y)
        };
        let mut env = super::ResumeEnv::default();
        if active.as_deref().is_some_and(|act| same(act, &target_xdg)) {
            return Some(Ok(env));
        }
        if crate::env::var_os("XDG_DATA_HOME").is_some_and(|v| !v.is_empty())
            && home_default.as_deref().is_some_and(|def| same(def, &target_xdg))
        {
            env.unset.push("XDG_DATA_HOME");
            return Some(Ok(env));
        }
        env.set
            .insert("XDG_DATA_HOME".into(), target_xdg.to_string_lossy().into_owned());
        Some(Ok(env))
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(db) = self.db_path() else {
            return Ok(Default::default());
        };
        // One database: one that cannot open (locked, unreadable, not a database) is skipped.
        let mut found = crate::Discovered::default();
        let refs =
            super::store::sqlite::cached_discover(&db, || open_ro(&db)?.read(|conn, _| discover_conn(conn, &db)));
        if let Some(refs) = found.keep(&db, refs) {
            found.refs = refs;
        }
        Ok(found)
    }

    /// Every session's certificate ([`rows_conn`]).
    fn rows(&self, store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
        open_ro(store).ok()?.read(|conn, _| rows_conn(conn)).ok()
    }

    /// A sub-agent `<session_id>/<agent_id>` reads its parent session's forest and row: the
    /// parent's certificate covers it.
    fn rows_key<'a>(&self, r: &'a SessionRef) -> &'a str {
        r.id.split_once('/').map_or(&r.id, |(session, _)| session)
    }

    /// Devin writes its session database in place.
    fn watch_policy(&self, _r: &SessionRef) -> super::WatchPolicy {
        super::WatchPolicy::Database
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Devin, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let conn = open_ro(&r.path)?;
            conn.read(|conn, reads| stream_conn(conn, r, sink, reads, opts.spans))
        })
    }
}

// ---------------------------------------------------------------------------
// Schema probing + the chain walk
// ---------------------------------------------------------------------------

/// A session's node topology and (where decodable) navigation facts. Topology comes from the
/// plain columns — never the JSON — so a corrupt `chat_message` can't truncate the chain walk;
/// each node's cells are read once, through [`Cells`], and kept as arena ranges for the stream.
#[derive(Default)]
struct Forest {
    parents: HashMap<i64, Option<i64>>,
    roles: HashMap<i64, String>,
    nodes: HashMap<i64, Node>,
    navigation: HashMap<i64, record::Navigation>,
    node_meta: HashMap<i64, NodeFacts>,
    receipts: Vec<(String, i64, Option<i64>)>,
    /// Per sub-agent, the lowest node naming it and that node's tool call ([`spawning_call`]).
    spawns: HashMap<String, (i64, Option<String>)>,
}

/// One node row: its creation time and its `chat_message` and `metadata` cells.
struct Node {
    created: Option<i64>,
    chat: Option<Cell>,
    meta: Option<Cell>,
}

#[cfg(test)]
thread_local! { static FOREST_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn load_forest(conn: &Connection, session_id: &str, cells: &Cells) -> Result<Forest> {
    #[cfg(test)]
    FOREST_LOADS.with(|n| n.set(n.get() + 1));
    let mut f = Forest::default();
    let mut stmt = conn.prepare("SELECT node_id, parent_node_id FROM message_nodes WHERE session_id = ?1")?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    for r in rows {
        let (node, parent) = r?;
        f.parents.insert(node, parent);
    }
    let select = Select::of(conn, "message_nodes")
        .always(&["node_id", "created_at"])
        .cell(cells, "chat_message")
        .cell(cells, "metadata");
    let (node_at, created_at) = (select.index("node_id")?, select.index("created_at")?);
    let (chat_at, meta_at) = (select.index("chat_message")?, select.index("metadata")?);
    let mut statement = conn.prepare(&format!(
        "SELECT {} FROM message_nodes WHERE session_id=?1 ORDER BY node_id",
        select.list()
    ))?;
    let mut rows = statement.query([session_id])?;
    while let Some(row) = rows.next()? {
        let node: i64 = row.get(node_at)?;
        let created = row.get::<_, Option<i64>>(created_at).ok().flatten();
        let chat = cells.cell(conn, "message_nodes", "chat_message", row, chat_at)?;
        let meta = cells.cell(conn, "message_nodes", "metadata", row, meta_at)?;
        if let Some(facts) = meta
            .map(|meta| cells.read(meta, record::facts))
            .transpose()?
            .flatten()
            .flatten()
        {
            f.node_meta.insert(node, facts);
        }
        if let Some(facts) = chat
            .map(|chat| cells.read(chat, record::navigation))
            .transpose()?
            .flatten()
        {
            if let Some(role) = &facts.role {
                f.roles.insert(node, role.clone());
            }
            if let (Some(agent), Some(head)) = (&facts.agent_id, facts.chain_node_id) {
                f.receipts.push((agent.clone(), head, created));
            }
            f.navigation.insert(node, facts);
        }
        f.nodes.insert(node, Node { created, chat, meta });
    }
    for (&node, n) in &f.navigation {
        if let Some(agent) = &n.agent_id {
            let spawn = f.spawns.entry(agent.clone()).or_insert((node, n.tool_call_id.clone()));
            if node < spawn.0 {
                *spawn = (node, n.tool_call_id.clone());
            }
        }
    }
    Ok(f)
}

type ForestKey = (PathBuf, String, Vec<Option<crate::source::FileId>>);
type SharedForests = std::sync::Mutex<HashMap<ForestKey, std::sync::Weak<(Cells, Forest)>>>;
static SHARED: std::sync::LazyLock<SharedForests> = std::sync::LazyLock::new(Default::default);
thread_local! {
    static LAST: std::cell::RefCell<Option<std::sync::Arc<(Cells, Forest)>>> = const { std::cell::RefCell::new(None) };
}

/// Forget every kept forest, as a new process starts (tests).
#[cfg(test)]
fn forget_forests() {
    SHARED.lock().unwrap().clear();
    LAST.with(|last| last.borrow_mut().take());
}

/// A session's forest with the cells it was read into, for a read at the store's current state:
/// the one a parent's or a sibling sub-agent's read of the same session loaded at this state, or
/// a fresh load. Every sub-agent reads its parent's whole forest, so a parent and its k
/// sub-agents loaded it k+1 times. Each thread keeps the last forest it read (memory: one
/// session's forest per thread), and every thread finds one another thread still keeps.
fn loaded_forest(
    conn: &Connection,
    db: &Path,
    session_id: &str,
    reads: &crate::source::ReadSet,
) -> Result<std::sync::Arc<(Cells, Forest)>> {
    use std::sync::{Arc, Weak};
    let state = || -> Vec<Option<crate::source::FileId>> {
        let mut files: Vec<PathBuf> = crate::source::sqlite_sidecars(db).into();
        files.push(db.to_path_buf());
        files.iter().map(|f| crate::source::FileId::at(f)).collect()
    };
    let key: ForestKey = (std::path::absolute(db)?, session_id.to_owned(), state());
    let shared = || SHARED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let found = shared().get(&key).and_then(Weak::upgrade);
    let loaded = match found {
        Some(loaded) => loaded,
        None => {
            let cells = Cells::new(conn, db, reads, "message_nodes")?;
            let forest = load_forest(conn, session_id, &cells)?;
            let loaded = Arc::new((cells, forest));
            // Shared only when no write landed while it loaded.
            if state() == key.2 {
                let mut shared = shared();
                shared.retain(|_, kept| kept.strong_count() > 0);
                shared.insert(key, Arc::downgrade(&loaded));
            }
            loaded
        }
    };
    LAST.with(|last| *last.borrow_mut() = Some(loaded.clone()));
    Ok(loaded)
}

/// Every session's [`Rows`](crate::source::Rows) certificate: what a read of it, or of any of its
/// sub-agent chains, consults besides message bodies. Its whole `sessions` row (the main chain id
/// among it); per `message_nodes` row, in `row_id` order, its `row_id`, node id, parent node id,
/// `created_at` and the byte lengths of `chat_message` and `metadata` (headers, never bodies); its
/// `subagent_heads` rows; and the store's schema version, which the read reports.
fn rows_conn(conn: &Connection) -> Result<HashMap<String, crate::source::Rows>> {
    let mut certificates = super::store::sqlite::Certificates::default();
    let version = if table_exists(conn, "refinery_schema_history") {
        "(SELECT MAX(version) FROM refinery_schema_history)"
    } else {
        "NULL"
    };
    certificates.digest(conn, &format!("SELECT id, {version}, * FROM sessions ORDER BY id"))?;
    let metadata = if super::store::sqlite::columns(conn, "message_nodes").contains("metadata") {
        "octet_length(metadata)"
    } else {
        "NULL"
    };
    certificates.rows(
        conn,
        &format!(
            "SELECT session_id, row_id, node_id, parent_node_id, created_at, octet_length(chat_message), {metadata} \
             FROM message_nodes ORDER BY session_id, row_id"
        ),
    )?;
    if table_exists(conn, "subagent_heads") {
        certificates.digest(
            conn,
            "SELECT session_id, agent_id, chain_node_id, updated_at FROM subagent_heads ORDER BY session_id, agent_id",
        )?;
    }
    Ok(certificates.0)
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

/// `(agent_id, chain_node_id, updated_at)` per sub-agent chain of a session: every
/// `subagent_heads` row (the table arrived late, in migration `subagent_heads`), plus every
/// sub-agent that only a tool row names. 3000.11.3's `run_subagent` writes no `subagent_heads`
/// row: its result row's `metadata.extensions` carry `subagent/agent_id` and
/// `subagent/chain_node_id` (the child chain's tail), and that is the only pointer to the chain.
fn subagent_head_rows(conn: &Connection, session_id: &str, forest: &Forest) -> Result<Vec<(String, i64, Option<i64>)>> {
    let mut out: Vec<(String, i64, Option<i64>)> = Vec::new();
    if table_exists(conn, "subagent_heads") {
        let mut stmt =
            conn.prepare("SELECT agent_id, chain_node_id, updated_at FROM subagent_heads WHERE session_id = ?1")?;
        let rows = stmt.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2).ok().flatten(),
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    // Indexed by agent: a session with many sub-agents and receipts scanned both lists per receipt.
    let listed: HashSet<&str> = out.iter().map(|(a, ..)| a.as_str()).collect();
    let mut from_tools: Vec<(String, i64, Option<i64>)> = Vec::new();
    let mut at_index: HashMap<&str, usize> = HashMap::new();
    for (agent, head, at) in &forest.receipts {
        if listed.contains(agent.as_str()) {
            continue;
        }
        match at_index.get(agent.as_str()) {
            Some(&i) if *head > from_tools[i].1 => from_tools[i] = (agent.clone(), *head, *at),
            Some(_) => {}
            None => {
                at_index.insert(agent, from_tools.len());
                from_tools.push((agent.clone(), *head, *at));
            }
        }
    }
    out.extend(from_tools);
    Ok(out)
}

/// user + assistant turns on `head`'s chain — the [`SessionRef::message_count`] contract
/// (ir.rs:793): superseded copies and abandoned branches elsewhere in the forest don't count.
fn chain_message_count(f: &Forest, head: Option<i64>) -> usize {
    head.map(|h| {
        chain_to_root(&f.parents, h)
            .iter()
            .filter(|n| {
                f.navigation.get(*n).is_some_and(|n| n.is_turn) && !f.node_meta.get(*n).is_some_and(|m| m.is_summary)
            })
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
        cwd: parent.cwd.clone(),
        title: Some(crate::ir::truncate(&title, crate::projection::Head::TITLE)),
        created_at: parent.created_at,
        updated_at: updated.and_then(ts_from_epoch).or(parent.updated_at),
        message_count: Some(chain_message_count(forest, Some(head))),
        ..SessionRef::new(format!("{session_id}/{agent}"), Harness::Devin, db.to_path_buf())
    })
}

struct DiscoverRow {
    id: String,
    wd: Option<String>,
    title: Option<String>,
    created: Option<i64>,
    last: Option<i64>,
    main_chain_id: Option<i64>,
}

fn discover_session_sql(
    conn: &Connection,
    db: &Path,
    has_subagent_heads: bool,
    has_node_meta: bool,
    row: DiscoverRow,
) -> Result<Vec<SessionRef>> {
    let DiscoverRow {
        id,
        wd,
        title,
        created,
        last,
        main_chain_id,
    } = row;
    let id = id.as_str();
    let mut parents: HashMap<i64, Option<i64>> = HashMap::new();
    {
        let mut stmt =
            conn.prepare_cached("SELECT node_id, parent_node_id FROM message_nodes WHERE session_id = ?1")?;
        let rows = stmt.query_map([id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)))?;
        for row in rows {
            let (node, parent) = row?;
            parents.insert(node, parent);
        }
    }

    let head = match main_chain_id {
        Some(h) if parents.contains_key(&h) => Some(h),
        _ => parents.keys().max().copied(),
    };

    let mut heads: Vec<(String, i64, Option<i64>)> = Vec::new();
    if has_subagent_heads {
        let mut stmt = conn
            .prepare_cached("SELECT agent_id, chain_node_id, updated_at FROM subagent_heads WHERE session_id = ?1")?;
        let rows = stmt.query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2).ok().flatten(),
            ))
        })?;
        for row in rows {
            heads.push(row?);
        }
    }
    {
        let listed: HashSet<String> = heads.iter().map(|(a, ..)| a.clone()).collect();
        let mut from_tools: Vec<(String, i64, Option<i64>)> = Vec::new();
        let mut at_index: HashMap<String, usize> = HashMap::new();
        let mut stmt = conn.prepare_cached(
            "SELECT created_at, chat_message FROM message_nodes \
             WHERE session_id = ?1 AND typeof(chat_message) = 'text' \
             AND (instr(chat_message, 'subagent/chain_node_id') > 0 OR instr(chat_message, '\\u') > 0 OR instr(chat_message, '\\/') > 0) \
             ORDER BY node_id",
        )?;
        let mut rows = stmt.query([id])?;
        while let Some(row) = rows.next()? {
            let at = row.get::<_, Option<i64>>(0).ok().flatten();
            let Ok(val) = serde_json::from_slice::<Value>(row.get_ref(1)?.as_bytes()?) else {
                continue;
            };
            if val.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let ext = val.get("metadata").and_then(|m| m.get("extensions"));
            let Some(agent) = ext.and_then(|e| e.get("subagent/agent_id")).and_then(Value::as_str) else {
                continue;
            };
            let Some(child_head) = ext
                .and_then(|e| e.get("subagent/chain_node_id"))
                .and_then(Value::as_i64)
            else {
                continue;
            };
            if listed.contains(agent) {
                continue;
            }
            match at_index.get(agent) {
                Some(&i) if child_head > from_tools[i].1 => {
                    from_tools[i] = (agent.to_string(), child_head, at);
                }
                Some(_) => {}
                None => {
                    at_index.insert(agent.to_string(), from_tools.len());
                    from_tools.push((agent.to_string(), child_head, at));
                }
            }
        }
        heads.extend(from_tools);
    }

    let parent_chain = head.map(|h| chain_to_root(&parents, h)).unwrap_or_default();
    let live_children: Vec<(String, Vec<i64>, Option<i64>)> = heads
        .into_iter()
        .filter(|(_, child_head, _)| parents.contains_key(child_head))
        .map(|(agent, child_head, updated)| (agent, chain_to_root(&parents, child_head), updated))
        .collect();

    let mut needed: HashSet<i64> = parent_chain.iter().copied().collect();
    for (_, chain, _) in &live_children {
        needed.extend(chain.iter().copied());
    }

    let mut turn_nodes: HashSet<i64> = HashSet::new();
    if !needed.is_empty() {
        let mut sorted_nodes: Vec<i64> = needed.into_iter().collect();
        sorted_nodes.sort_unstable();
        let nodes_json = serde_json::to_string(&sorted_nodes)?;
        let summary_expr = if has_node_meta {
            "CASE WHEN typeof(metadata) = 'text' AND json_valid(metadata) AND json_type(metadata) = 'object' \
             THEN (json_type(metadata, '$.summarized_from') IS NOT NULL AND json_type(metadata, '$.summarized_from') != 'null') \
             ELSE 0 END"
        } else {
            "0"
        };
        let sql = format!(
            "SELECT node_id, \
                    CASE WHEN typeof(chat_message) = 'text' AND json_valid(chat_message) AND json_type(chat_message) = 'object' \
                         THEN CASE json_extract(chat_message, '$.role') \
                              WHEN 'assistant' THEN 1 \
                              WHEN 'user' THEN (json_type(chat_message, '$.metadata.is_user_input') = 'true') \
                              ELSE 0 \
                         END \
                         ELSE 0 \
                    END AS is_turn, \
                    {summary_expr} AS is_summary \
             FROM message_nodes \
             WHERE session_id = ?1 AND node_id IN (SELECT value FROM json_each(?2))"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(rusqlite::params![id, nodes_json])?;
        while let Some(row) = rows.next()? {
            let node_id: i64 = row.get(0)?;
            let is_turn: bool = row.get::<_, i64>(1).unwrap_or(0) != 0;
            let is_summary: bool = row.get::<_, i64>(2).unwrap_or(0) != 0;
            if is_turn && !is_summary {
                turn_nodes.insert(node_id);
            }
        }
    }

    let cwd = wd.filter(|s| !s.is_empty()).map(PathBuf::from);
    let parent_title = title
        .filter(|s| !s.trim().is_empty())
        .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE));
    let created_at = created.and_then(ts_from_epoch);
    let updated_at = last.and_then(ts_from_epoch).or(created_at);
    let parent_count = parent_chain.iter().filter(|n| turn_nodes.contains(n)).count();

    let mut out = Vec::with_capacity(1 + live_children.len());
    out.push(SessionRef {
        cwd: cwd.clone(),
        title: parent_title.clone(),
        created_at,
        updated_at,
        message_count: Some(parent_count),
        ..SessionRef::new(id, Harness::Devin, db)
    });
    for (agent, chain, updated) in live_children {
        let child_title = match &parent_title {
            Some(t) => format!("[{agent}] {t}"),
            None => format!("[{agent}]"),
        };
        let child_count = chain.iter().filter(|n| turn_nodes.contains(n)).count();
        out.push(SessionRef {
            cwd: cwd.clone(),
            title: Some(crate::ir::truncate(&child_title, crate::projection::Head::TITLE)),
            created_at,
            updated_at: updated.and_then(ts_from_epoch).or(updated_at),
            message_count: Some(child_count),
            ..SessionRef::new(format!("{id}/{agent}"), Harness::Devin, db)
        });
    }
    Ok(out)
}

fn discover_conn(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let select = Select::of(conn, "sessions")
        .always(&["id", "working_directory"])
        .probed(&["title"])
        .always(&["created_at", "last_activity_at"])
        .probed(&["main_chain_id"]);
    let sql = format!("SELECT {} FROM sessions ORDER BY last_activity_at DESC", select.list());
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
    let has_subagent_heads = table_exists(conn, "subagent_heads");
    let has_node_meta = super::store::sqlite::columns(conn, "message_nodes").contains("metadata");
    // cached_discover already memoizes the store snapshot. Row lengths alone
    // cannot certify JSON roles, summary flags or subagent receipts.
    let mut out = Vec::new();
    for row in rows {
        let (id, wd, title, created, last, main_chain_id) = row?;
        let row = DiscoverRow {
            id: id.clone(),
            wd,
            title,
            created,
            last,
            main_chain_id,
        };
        out.extend(
            discover_session_sql(conn, db, has_subagent_heads, has_node_meta, row)
                .with_context(|| format!("listing Devin session {id}"))?,
        );
    }
    Ok(out)
}

/// A parent's completion receipt records the tool call that launched this child. The same
/// projection serves parsed lineage and the forest, including older tool-only subagents.
fn spawning_call(agent: &str, forest: &Forest) -> Option<String> {
    forest.spawns.get(agent).and_then(|(_, call)| call.clone())
}

/// The sub-agent chains a session's `subagent_heads` table names — the same rows [`discover_conn`]
/// turns into `<session_id>/<agent_id>` child refs — as the forest [`crate::subagent_tree_of`]
/// surfaces. `agent_id` is the child's `agent_type`; a completion receipt provides its spawning
/// tool call. Devin records no per-agent description.
pub fn subagent_tree(db: &Path, session_id: &str) -> Result<Vec<crate::AgentRef>> {
    // Child refs address a chain within the root session, not another sessions row. The native
    // store attaches every subagent head to the root; a child has no separately nested forest.
    if session_id.contains('/') {
        return Ok(Vec::new());
    }
    // Every listed session's forest, derived once per db state and kept in cv's catalog: a
    // forest fold over an unchanged store neither copies nor opens it.
    let forests = super::store::sqlite::derived(db, Harness::Devin, |conn, db| {
        let listed = super::store::sqlite::cached_discover(db, || discover_conn(conn, db))?;
        Ok(Forests(
            listed
                .into_iter()
                .filter(|r| !r.id.contains('/'))
                .filter_map(|r| Some((r.id.clone(), forest_conn(conn, db, &r.id).ok()?)))
                .collect::<HashMap<String, Vec<crate::AgentRef>>>(),
        ))
    })?;
    if let Some(forest) = forests.payload.0.get(session_id) {
        return Ok(forest.clone());
    }
    open_ro(db)?.read(|conn, _| forest_conn(conn, db, session_id))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Forests(HashMap<String, Vec<crate::AgentRef>>);

impl crate::source::Payload for Forests {
    const TAG: &'static str = "devin-forests";
}

/// The subagent heads of `session_id`, each with the chain it names.
fn forest_conn(conn: &Connection, db: &Path, session_id: &str) -> Result<Vec<crate::AgentRef>> {
    let select = Select::of(conn, "sessions")
        .always(&["working_directory"])
        .probed(&["title"])
        .always(&["created_at", "last_activity_at"]);
    let (wd, title, created, last): (Option<String>, Option<String>, Option<i64>, Option<i64>) = conn.query_row(
        &format!("SELECT {} FROM sessions WHERE id = ?1", select.list()),
        [session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let parent = SessionRef {
        cwd: wd.filter(|s| !s.is_empty()).map(PathBuf::from),
        title,
        created_at: created.and_then(ts_from_epoch),
        updated_at: last.and_then(ts_from_epoch),
        ..SessionRef::new(session_id.to_string(), Harness::Devin, db.to_path_buf())
    };
    let cells = Cells::new(conn, db, &crate::source::ReadSet::default(), "message_nodes")?;
    let forest = load_forest(conn, session_id, &cells)?;
    let mut out = Vec::new();
    for (agent, head, updated) in subagent_head_rows(conn, session_id, &forest)? {
        if let Some(session) = child_ref(db, session_id, &agent, head, updated, &forest, &parent) {
            out.push(crate::AgentRef {
                session,
                tool_use_id: spawning_call(&agent, &forest),
                agent_type: Some(agent),
                description: None,
                workflow: None,
                result_status: None,
                result_summary: None,
            });
        }
    }
    Ok(out)
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

/// Session-wide navigation state has fixed size per node, independent of metadata payload size.
#[derive(Clone, Copy, Default)]
struct NodeFacts {
    is_summary: bool,
    summarized_from: Option<i64>,
    num_tokens_preceding: Option<u64>,
    is_system_prefix: bool,
}

/// When `db_path` sits at `<xdg>/devin/cli/sessions.db`, return `<xdg>`.
pub(super) fn xdg_root_for_db(db_path: &Path) -> Option<PathBuf> {
    let cli = db_path.parent().filter(|_| db_path.ends_with("sessions.db"))?;
    let devin = cli.parent().filter(|_| cli.ends_with("cli"))?;
    devin
        .parent()
        .filter(|_| devin.ends_with("devin"))
        .map(Path::to_path_buf)
}

pub(crate) fn stream_conn_verified(
    conn: &Connection,
    r: &SessionRef,
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let guard = crate::json::SqliteSessionGuard::new(|| false);
    let result = crate::stream::checked(sink, |sink| {
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Devin, sink);
        let (session_id, agent_id) = match r.id.split_once('/') {
            Some((s, a)) => (s, Some(a)),
            None => (r.id.as_str(), None),
        };
        stream_session_with_guard(
            conn,
            session_id,
            agent_id,
            r,
            &mut spawns,
            &crate::source::ReadSet::default(),
            opts.spans,
            Some(&guard),
        )
    });
    guard.end_pass();
    result
}

fn stream_conn(
    conn: &Connection,
    r: &SessionRef,
    sink: &mut dyn MessageSink,
    reads: &crate::source::ReadSet,
    spans: bool,
) -> Result<Session> {
    // Child ids split on the FIRST '/': `<session_id>/<agent_id>` — slugs never contain one.
    let (session_id, agent_id) = match r.id.split_once('/') {
        Some((s, a)) => (s, Some(a)),
        None => (r.id.as_str(), None),
    };
    stream_session_with_guard(conn, session_id, agent_id, r, sink, reads, spans, None)
}

/// The one chain walk / node map / meta path behind [`Adapter::stream`] for both a session and
/// each of its sub-agent children: same forest, same [`sessions`] row — `agent` only changes which
/// head is walked (`subagent_heads.chain_node_id` instead of `main_chain_id`) and what lineage /
/// extra the emitted session carries.
#[allow(clippy::too_many_arguments)]
fn stream_session_with_guard(
    conn: &Connection,
    session_id: &str,
    agent: Option<&str>,
    r: &SessionRef,
    sink: &mut dyn MessageSink,
    reads: &crate::source::ReadSet,
    spans: bool,
    in_tx_guard: Option<&std::sync::Arc<crate::json::SqliteSessionGuard>>,
) -> Result<Session> {
    let _snapshot = if in_tx_guard.is_none() {
        super::store::sqlite::snapshot(conn)?
    } else {
        None
    };
    let select = Select::of(conn, "sessions")
        .always(&[
            "working_directory",
            "backend_type",
            "model",
            "agent_mode",
            "created_at",
            "last_activity_at",
        ])
        .probed(&[
            "title",
            "main_chain_id",
            "cogs_json",
            "workspace_dirs",
            "hidden",
            "metadata",
        ]);
    let sql = format!("SELECT {} FROM sessions WHERE id = ?1", select.list());
    let (wd, backend, model, agent_mode, created, last, title, main_chain_id, cogs, wdirs, hidden, smeta): SessionRow =
        conn.query_row(&sql, [session_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })
        .with_context(|| format!("devin session {session_id:?}"))?;

    // The chosen chain (see the module doc): head → root, reversed. For a child the head comes
    // from `subagent_heads.chain_node_id` — a head naming no live node leaves the chain empty
    // (counted as dangling rather than falling back, which would pick some other chain's tail).
    let loaded = match in_tx_guard {
        None => loaded_forest(conn, &r.path, session_id, reads)?,
        Some(guard) => {
            let cells = Cells::for_session(conn, &r.path, reads, guard, "message_nodes")?;
            let forest = load_forest(conn, session_id, &cells)?;
            std::sync::Arc::new((cells, forest))
        }
    };
    let (cells, forest) = (&loaded.0, &loaded.1);
    let heads = subagent_head_rows(conn, session_id, forest)?;
    let (head, fell_back, dangling_head) = match agent {
        None => {
            let (head, fb) = chain_head(forest, main_chain_id);
            (head, fb, None)
        }
        Some(a) => match heads.iter().find(|(id, _, _)| id == a) {
            None => anyhow::bail!("no devin sub-agent {a:?} in session {session_id:?}"),
            Some((_, head, _)) => (forest.parents.contains_key(head).then_some(*head), false, Some(*head)),
        },
    };
    let chain = head.map(|h| chain_to_root(&forest.parents, h)).unwrap_or_default();

    // For a child, the tool call that spawned it: the parent's `tool` row carrying
    // `subagent/agent_id` — its `tool_call_id` names the spawning call. The forest already
    // projected these receipts; absent on sessions without a completion row.
    let spawned_by = agent.and_then(|a| spawning_call(a, forest));

    // The forest's bounded metadata projection serves compaction, prefix and turn counts.
    let node_meta = &forest.node_meta;
    // Gather each compaction pair's facts before its first summary is emitted.
    let mut compaction_facts: HashMap<i64, serde_json::Map<String, Value>> = HashMap::new();
    for &nid in &chain {
        let Some(sf) = node_meta.get(&nid).and_then(|m| m.summarized_from) else {
            continue;
        };
        let entry = compaction_facts.entry(sf).or_insert_with(|| {
            let mut o = serde_json::Map::new();
            o.insert("summarized_from".into(), json!(sf));
            o.insert("pre_chain_head".into(), json!(sf));
            o
        });
        if let Some(ntp) = node_meta.get(&nid).and_then(|m| m.num_tokens_preceding) {
            // The window's token cost: the larger `num_tokens_preceding` of the pair.
            let cur = entry.get("pre_tokens").and_then(Value::as_u64).unwrap_or(0);
            entry.insert("pre_tokens".into(), json!(cur.max(ntp)));
        }
        if let Some(chat) = forest.nodes.get(&nid).and_then(|n| n.chat) {
            if let Some(facts) = cells.read(chat, record::compaction)? {
                entry.extend(facts);
            }
        }
    }

    // The system prompt = the chain's `is_system_prefix` nodes' contents, in order. (Content is
    // fetched lazily per node here — only the prefix nodes' bodies are read ahead of the stream.)
    let mut prefix: Vec<String> = Vec::new();
    for &nid in &chain {
        let is_prefix = node_meta.get(&nid).map(|m| m.is_system_prefix).unwrap_or(false);
        if is_prefix && forest.roles.get(&nid).is_some_and(|r| r == "system") {
            let content = match forest.nodes.get(&nid).and_then(|n| n.chat) {
                Some(chat) => cells.read(chat, |v| record::string(v.get("content")?))?.flatten(),
                None => None,
            };
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
                let g = forest.navigation.get(&nid).and_then(|n| n.model.clone());
                if let Some(g) = g.filter(|s| !s.is_empty()) {
                    model = Some(g);
                    break;
                }
            }
        }
    }

    let schema_version: Option<i64> = if table_exists(conn, "refinery_schema_history") {
        conn.query_row("SELECT MAX(version) FROM refinery_schema_history", [], |row| row.get(0))
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
        put(
            &mut bag,
            "backend_type",
            backend.filter(|s| !s.is_empty()).map(Value::String),
        );
        put(
            &mut bag,
            "agent_mode",
            agent_mode.filter(|s| !s.is_empty()).map(Value::String),
        );
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
        let mut cv_derived_pair: Option<(String, Option<String>)> = None;
        for (k, raw) in [("cogs_json", cogs), ("metadata", smeta)] {
            if let Some(mut v) = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                if k == "metadata" && agent.is_none() {
                    if let Some(obj) = v.as_object_mut() {
                        if let Some(cv) = obj.remove("cv") {
                            if let Some(df) = cv
                                .get("derived_from")
                                .and_then(Value::as_str)
                                .filter(|d| !d.is_empty() && *d != session_id)
                            {
                                let dk = cv.get("derived_kind").and_then(Value::as_str).map(str::to_owned);
                                cv_derived_pair = Some((df.to_owned(), dk));
                            }
                        }
                        if obj.is_empty() {
                            v = Value::Null;
                        }
                    }
                }
                if !v.is_null() {
                    bag.insert(k.into(), v);
                }
            }
        }
        if let Some((df, dk)) = cv_derived_pair {
            let mut cv_map = serde_json::Map::new();
            cv_map.insert("derived_from".into(), json!(df));
            if let Some(kind) = dk {
                cv_map.insert("derived_kind".into(), json!(kind));
            }
            extra.insert(crate::ir::CV_NAMESPACE.into(), Value::Object(cv_map));
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
            .and_then(ts_from_epoch)
    });
    let title = match (agent, title.filter(|t| !t.trim().is_empty())) {
        (Some(a), Some(t)) => Some(format!("[{a}] {t}")),
        (Some(a), None) => Some(format!("[{a}]")),
        (None, t) => t.or_else(|| r.title.clone()),
    };
    let mut s = Session {
        cwd: wd
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title,
        created_at: created.and_then(ts_from_epoch).or(r.created_at),
        updated_at: head_updated.or_else(|| last.and_then(ts_from_epoch)).or(r.updated_at),
        model,
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
        ..Session::new(r.id.clone(), Harness::Devin)
    };
    sink.meta(&s);

    // One chain node → one message, streamed: fetch each node's `chat_message` only as it is
    // emitted, so peak is one message, not the whole transcript.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut last_msg_id: Option<String> = None;
    let mut last_sf: Option<i64> = None;
    let mut unparsed = Unparsed::default();
    for &nid in &chain {
        let Some(Node {
            created: Some(created),
            chat: Some(chat),
            meta,
        }) = forest.nodes.get(&nid)
        else {
            unparsed.count();
            continue;
        };
        let carried = match meta {
            Some(meta) => cells
                .read(*meta, |v| {
                    if v.is_object()? {
                        record::carried(v)
                    } else {
                        Ok(Vec::new())
                    }
                })?
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let facts = node_meta.get(&nid);
        let decoded = cells.read(*chat, |v| {
            record::node(v, nid, *created, facts, carried, &mut tool_names, spans)
        })?;
        let Some(mut m) = decoded else {
            unparsed.count();
            continue;
        };

        // The first node of a compaction pair: emit a synthetic boundary right before it and
        // point the summary's parent_id at the boundary (compaction.rs pairs them by uuid).
        let sf = node_meta.get(&nid).and_then(|n| n.summarized_from);
        let mut parent = last_msg_id.clone();
        if let Some(sf) = sf {
            if last_sf != Some(sf) {
                last_sf = Some(sf);
                let bid = format!("{}#boundary", m.id.clone().unwrap_or_else(|| format!("node{nid}")));
                let mut b = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
                b.id = Some(bid.clone());
                b.timestamp = m.timestamp;
                b.content = vec![Block::from_text("context compacted".into())];
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
    unparsed.note(&mut s);
    Ok(s)
}

// ---------------------------------------------------------------------------
// Tests — `tests/fixtures/devin/sessions-v17-3000.11.3.db` (tools/harness-fixtures/devin/)
// ---------------------------------------------------------------------------

/// Whole-`Session` convenience over [`stream_conn`] for tests.
#[cfg(test)]
fn parse_db(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_conn(conn, r, &mut sink, &crate::source::ReadSet::default(), false)?;
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
        SessionRef::new(id, Harness::Devin, fixture())
    }

    /// The corpus store: a node appended to one session moves its certificate (and its
    /// sub-agents', which read its forest) alone; every other session and sub-agent reads exactly
    /// as before.
    #[test]
    fn corpus_rows_certify_each_session_and_move_only_with_its_rows() {
        let world = crate::test_world("devin-corpus-rows");
        let db = world.base.join("sessions.db");
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/devin");
        for version in std::fs::read_dir(&corpus).unwrap() {
            std::fs::copy(version.unwrap().path().join(".local/share/devin/cli/sessions.db"), &db).unwrap();
            let adapter = Devin::with_root(&world.base);
            assert!(
                adapter.discover().unwrap().refs.iter().any(|r| r.id.contains('/')),
                "sub-agents"
            );
            let leaf: String = Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT session_id FROM message_nodes ORDER BY session_id LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            super::super::store::sqlite::assert_rows_move_alone(&adapter, &db, &leaf, |c| {
                c.execute(
                    "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) \
                     SELECT ?1, max(node_id) + 1, NULL, '{\"role\":\"user\",\"content\":\"zqcertified\"}', 4102444800 \
                     FROM message_nodes WHERE session_id = ?1",
                    [&leaf],
                )
                .unwrap();
            });
        }
    }

    #[test]
    fn adapter_rejects_source_mutated_by_metadata_callback() {
        let world = cv_testkit::World::new("devin-checked-read");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path).unwrap();
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let adapter = Devin::with_root(&world.base);
        let reference = adapter.discover().unwrap().remove(0);
        struct MutatingSink<'a>(&'a Connection);
        impl MessageSink for MutatingSink<'_> {
            fn meta(&mut self, _: &Session) {
                self.0.execute_batch("CREATE TABLE callback_mutation(value);").unwrap();
            }
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
        }
        let failure = adapter
            .stream(&reference, &Demand::LEAN, &mut MutatingSink(&writer))
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"), "{failure:#}");
    }

    #[test]
    fn long_text_stays_in_the_cell_and_the_store_guards_it() -> Result<()> {
        let world = crate::test_world("devin-cell-guard");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let adapter = Devin::with_root(&world.base);
        let writer = Connection::open(&path)?;
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE);")?;
        let reference = adapter.discover()?.remove(0);
        let before = std::fs::read(&path)?;
        writer.execute(
            "UPDATE message_nodes SET chat_message=json_set(chat_message,'$.content',?1) WHERE session_id=?2",
            rusqlite::params!["changed", reference.id],
        )?;
        assert_eq!(std::fs::read(&path)?, before);
        writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        writer.execute(
            "UPDATE message_nodes SET chat_message=json_set(chat_message,'$.content',?1) WHERE session_id=?2",
            rusqlite::params!["large body ".repeat(10000), reference.id],
        )?;
        let inline = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
        let mut large = crate::stream::collect_with(&adapter, &reference, &Demand::RICH.with_spans())?;
        let source = |s: &Session| {
            s.messages.iter().any(|m| {
                m.content.iter().any(|b| {
                    matches!(
                        b,
                        Block::Text {
                            text: crate::Text::Source(_),
                            ..
                        }
                    )
                })
            })
        };
        assert!(source(&large) && !source(&inline));
        writer.execute(
            "UPDATE message_nodes SET created_at=created_at+1 WHERE session_id=?1",
            [&reference.id],
        )?;
        assert!(large.materialize().is_err());
        Ok(())
    }

    #[test]
    fn prefix_and_model_fallback_skip_giant_ignored_cells() -> Result<()> {
        let world = crate::test_world("devin-native-navigation");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let writer = Connection::open(&path)?;
        let payload = "ignored λ\nbody".repeat(100_000);
        let chat = json!({"role":"assistant","message_id":"giant","content":payload,
            "metadata":{"generation_model":"fallback-model","telemetry":{"ignored":payload}}});
        writer.execute(
            "UPDATE message_nodes SET chat_message=?1, metadata=?2 \
            WHERE session_id='fixture-beta' AND node_id=3",
            rusqlite::params![
                chat.to_string(),
                json!({"is_system_prefix":false,"ignored":payload}).to_string()
            ],
        )?;
        let prefix = json!({"role":"system","content":"You are a checked prefix.",
            "metadata":{"ignored":payload}});
        writer.execute(
            "UPDATE message_nodes SET chat_message=?1, metadata=?2 \
            WHERE session_id='fixture-beta' AND node_id=0",
            rusqlite::params![
                prefix.to_string(),
                json!({"is_system_prefix":true,"ignored":payload}).to_string()
            ],
        )?;
        writer.execute("UPDATE sessions SET model='' WHERE id='fixture-beta'", [])?;
        let adapter = Devin::with_root(&world.base);
        let refs = adapter.discover()?;
        let reference = refs.iter().find(|r| r.id == "fixture-beta").unwrap();
        for options in [Demand::RICH, Demand::RICH.with_spans(), Demand::FACTS.with_spans()] {
            let s = crate::stream::collect_with(&adapter, reference, &options)?;
            assert_eq!(s.model.as_deref(), Some("fallback-model"));
            assert_eq!(s.system_prompt.as_deref(), Some("You are a checked prefix."));
        }
        Ok(())
    }

    #[test]
    fn a_view_without_incremental_cells_reads_its_rows() -> Result<()> {
        let world = crate::test_world("devin-view-fallback");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let adapter = Devin::with_root(&world.base);
        let table = adapter.discover()?;
        let parsed = |r: &SessionRef| crate::stream::collect_with(&adapter, r, &Demand::RICH).unwrap();
        let expected: Vec<Value> = table.iter().map(|r| serde_json::to_value(parsed(r)).unwrap()).collect();
        let writer = Connection::open(&path)?;
        writer.execute_batch(
            "ALTER TABLE message_nodes RENAME TO stored_nodes; \
            CREATE VIEW message_nodes AS SELECT * FROM stored_nodes;",
        )?;
        let view = adapter.discover()?;
        assert_eq!(serde_json::to_value(&view.refs)?, serde_json::to_value(&table.refs)?);
        for (r, expected) in view.iter().zip(expected) {
            assert_eq!(serde_json::to_value(parsed(r))?, expected, "{}", r.id);
        }
        Ok(())
    }

    #[test]
    fn cells_that_are_not_json_text_are_skipped_rows() -> Result<()> {
        let world = crate::test_world("devin-cell-types");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let writer = Connection::open(&path)?;
        let adapter = Devin::with_root(&world.base);
        let reference = SessionRef::new("fixture-beta", Harness::Devin, &path);
        let base = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
        for (cell, skipped) in [
            (
                "CAST('{\"role\":\"assistant\",\"content\":\"blob body\"}' AS BLOB)",
                true,
            ),
            ("CAST(X'FF' AS TEXT)", true),
            ("'{invalid json'", true),
            ("''", true),
            ("'null'", false),
        ] {
            writer.execute_batch(&format!(
                "UPDATE message_nodes SET chat_message={cell} \
                WHERE session_id='fixture-beta' AND node_id=3;"
            ))?;
            for options in [Demand::RICH, Demand::RICH.with_spans()] {
                let s = crate::stream::collect_with(&adapter, &reference, &options)?;
                let noted = |s: &Session| {
                    s.cv_extra()
                        .and_then(|cv| cv.get("skipped_lines")?.as_u64())
                        .unwrap_or(0)
                };
                assert_eq!(base.messages.len() - s.messages.len(), usize::from(skipped), "{cell}");
                assert_eq!(noted(&s) - noted(&base), u64::from(skipped), "{cell}");
            }
        }
        Ok(())
    }

    #[test]
    fn corrupt_abandoned_rows_do_not_hide_tool_only_subagent_heads() -> Result<()> {
        let world = crate::test_world("devin-corrupt-tool-head");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let conn = Connection::open(&path)?;
        conn.execute("DELETE FROM subagent_heads WHERE session_id='fixture-alpha'", [])?;
        conn.execute_batch(
            "INSERT INTO message_nodes(session_id,node_id,chat_message,created_at) \
            VALUES('fixture-alpha',-2,'{broken',1000);",
        )?;
        let receipt = json!({"role":"tool","content":"child complete","tool_call_id":"spawn-call",
            "metadata":{"extensions":{"subagent/agent_id":"sidekick","subagent/chain_node_id":29}}});
        conn.execute(
            "INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at) \
            VALUES('fixture-alpha',30,16,?1,2000)",
            [receipt.to_string().replace("subagent/", r"subagent\/")],
        )?;
        // #tooth A cyclic child still visits each real node only once.
        assert_eq!(chain_to_root(&HashMap::from([(1, Some(2)), (2, Some(1))]), 2), [1, 2]);
        let refs = discover_conn(&conn, &path)?;
        let child = refs.iter().find(|r| r.id == "fixture-alpha/sidekick").unwrap();
        assert_eq!(child.message_count, Some(5));
        let tree = subagent_tree(&path, "fixture-alpha").unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].tool_use_id.as_deref(), Some("spawn-call"));
        let parsed = parse_db(&conn, child)?;
        assert_eq!(parsed.lineage.parent.as_deref(), Some("fixture-alpha"));
        assert_eq!(parsed.lineage.spawned_by_tool_use.as_deref(), Some("spawn-call"));
        assert_eq!(parsed.messages.len(), 14);
        assert!(parsed
            .system_prompt
            .as_deref()
            .unwrap()
            .starts_with("You are the Sidekick"));
        Ok(())
    }

    fn dbag(s: &Session) -> &serde_json::Map<String, Value> {
        s.harness_extra(Harness::Devin).expect("extra[\"devin\"]")
    }

    fn roles(s: &Session) -> Vec<(Role, MessageKind, Origin)> {
        s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect()
    }

    #[test]
    fn discovery_counts_only_canonical_prompts_and_replies_on_each_chain() -> Result<()> {
        let world = crate::test_world("devin-canonical-discovery-count");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path)?;
        let writer = Connection::open(&path)?;
        // A user-role injected node is not a human prompt. The fixture also carries assistant
        // compaction summaries, which are not model replies.
        writer.execute(
            "UPDATE message_nodes SET chat_message=json_set(chat_message,'$.metadata.is_user_input',false) \
            WHERE session_id='fixture-beta' AND node_id=1",
            [],
        )?;
        let adapter = Devin::with_root(&world.base);
        for reference in adapter.discover()? {
            let parsed = crate::stream::collect_with(&adapter, &reference, &Demand::RICH)?;
            assert_eq!(
                reference.message_count,
                Some(parsed.messages.iter().filter(|m| m.is_turn()).count()),
                "{}",
                reference.id
            );
        }
        let tree = subagent_tree(&path, "fixture-alpha").unwrap();
        assert_eq!(tree[0].session.message_count, Some(5));
        Ok(())
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
        assert_eq!(a.message_count, Some(3));

        let sub = &refs[1];
        assert_eq!(sub.title.as_deref(), Some("[sidekick] Fixture session alpha"));
        assert_eq!(sub.cwd, a.cwd);
        assert_eq!(sub.created_at, a.created_at);
        assert_eq!(sub.updated_at, ts_from_epoch(1_759_600_000i64 + 200));
        // The child's own chain: handoff user + 5 assistant (incl. the compaction summary)
        // = 5 conversational turns (the assistant summary is not a reply).
        assert_eq!(sub.message_count, Some(5));

        // Beta: nodes 0 (system), 1 (user), 2 (corrupt), 3 (assistant) → 2.
        assert_eq!(refs[2].message_count, Some(2));
    }

    /// A session and its sub-agents, read one after another at one store state, load the
    /// session's forest once, and read as each did with a load of its own.
    #[test]
    fn a_parent_and_its_sub_agents_load_the_forest_once() {
        let world = crate::test_world("devin-forest-once");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path).unwrap();
        let adapter = Devin::with_root(&world.base);
        let refs: Vec<SessionRef> = adapter
            .discover()
            .unwrap()
            .refs
            .into_iter()
            .filter(|r| r.id.split('/').next() == Some("fixture-alpha"))
            .collect();
        assert!(refs.len() > 1, "a parent and its sub-agents");
        let read = |r: &SessionRef| {
            serde_json::to_string(&crate::stream::collect_with(&adapter, r, &Demand::RICH).unwrap()).unwrap()
        };
        FOREST_LOADS.with(|n| n.set(0));
        let shared: Vec<String> = refs.iter().map(read).collect();
        assert_eq!(FOREST_LOADS.with(|n| n.replace(0)), 1);
        for (r, shared) in refs.iter().zip(&shared) {
            forget_forests();
            assert_eq!(read(r), *shared, "{}: a read with its own forest", r.id);
        }
        assert_eq!(FOREST_LOADS.with(|n| n.replace(0)), refs.len());
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
    fn subagent_forests_reuse_one_database_record_and_refresh_after_writes() {
        let world = crate::test_world("devin-forest-record");
        let path = world.base.join("sessions.db");
        std::fs::copy(fixture(), &path).unwrap();
        let opens = || super::super::store::sqlite::DERIVE_OPENS.with(std::cell::Cell::get);
        let before = opens();
        let first = subagent_tree(&path, "fixture-alpha").unwrap();
        assert!(!first.is_empty());
        assert_eq!(opens(), before + 1);
        subagent_tree(&path, "fixture-beta").unwrap();
        assert_eq!(opens(), before + 1, "second parent does not open the database");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute(
                "UPDATE sessions SET working_directory='/changed' WHERE id='fixture-alpha'",
                [],
            )
            .unwrap();
        let changed = subagent_tree(&path, "fixture-alpha").unwrap();
        assert_eq!(changed[0].session.cwd.as_deref(), Some(Path::new("/changed")));
        assert_eq!(opens(), before + 2);
    }

    #[test]
    fn subagent_tree_lists_the_live_head_and_skips_the_dangling_one() {
        let subs = subagent_tree(&fixture(), "fixture-alpha").unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].session.id, "fixture-alpha/sidekick");
        assert_eq!(subs[0].agent_type.as_deref(), Some("sidekick"));
        assert_eq!(subs[0].session.message_count, Some(5));
        assert!(subagent_tree(&fixture(), &subs[0].session.id).unwrap().is_empty());
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

        // `cv show --kind compaction_boundary` sees exactly one boundary, paired, with the summary text.
        let found = crate::compaction::detect_in_session(&s, true).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary_msg_idx, Some(9));
        assert_eq!(found[0].trigger.as_deref(), Some("async_file_compactor"));
        assert_eq!(found[0].pre_tokens, Some(48000));
        assert!(found[0].summary.as_deref().unwrap().contains("Summary of prior work"));

        // chisel/tool_failure → is_error + status + details.failure_reason (no tool_result_meta).
        let t = &s.messages[12];
        match &t.content[0] {
            Block::ToolResult {
                is_error,
                status,
                details,
                tool_name,
                ..
            } => {
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
        assert!(
            matches!(&a.content[0], Block::Thinking { signature, .. } if signature.as_deref() == Some("sealed.v1.fixture"))
        );
        assert!(
            matches!(&a.content[1], Block::Text { text , .. } if text.matches_str("I'll check both.") == Some(true))
        );
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
