//! Hermes (Nous Research) adapter — `~/.hermes/state.db` (SQLite, schema v30; back-compatible to the
//! pre-v11 tables).
//!
//! See `docs/FORMATS.md`. One DB holds all sessions: a `sessions` table (metadata) and a `messages`
//! table (OpenAI-shaped rows: role, content, tool_calls JSON, reasoning, …). cwd is NOT persisted
//! (runtime-only). Multimodal content is stored with a `\x00json:` sentinel prefix.
//!
//! Historical coverage: Hermes does NOT version-gate column additions — it reconciles live tables
//! against `SCHEMA_SQL` and `ALTER TABLE ADD COLUMN`s anything missing (the Beets/sqlite-utils
//! pattern). So a database that was last opened by an older Hermes can be missing newer columns
//! entirely (`token_count`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`,
//! `codex_message_items`, `platform_message_id`, `observed`, `finish_reason`, …). We therefore probe
//! `PRAGMA table_info` and only SELECT columns that actually exist, so the parser degrades
//! gracefully instead of failing the whole session on a missing column.
//!
//! Reasoning is recorded across several provider-specific columns and we map each:
//!   * `reasoning`         — short summary text (DeepSeek/Qwen, OpenRouter summary) → Thinking.text
//!   * `reasoning_content` — provider-native scratchpad (Moonshot/Novita) → Thinking.text (appended)
//!   * `reasoning_details` — `[{type:"reasoning.summary"|"reasoning.encrypted_content"|"thinking", …}]`
//!     (OpenRouter unified) → Thinking.text (summaries) + Thinking.encrypted
//!   * `codex_reasoning_items` — `[{type:"reasoning", id, encrypted_content}]` (Codex Responses API)
//!     → Thinking.encrypted + stashed verbatim in `extra`
//!   * `codex_message_items`   — `[{type:"message", phase, content:[{type:"output_text",text}]}]`
//!     → stashed verbatim in `extra` (Codex final/commentary phases)
//!
//! Compression chains: a long conversation is split across multiple session rows linked by
//! `parent_session_id`, where the parent has `end_reason='compression'`. `parse` walks the lineage
//! root→tip and merges all messages (mirroring Hermes's own
//! `get_messages_as_conversation(include_ancestors=True)`), deduplicating the replayed first user
//! message at each compression boundary, so a resumed/compressed conversation reads as one transcript.
//! Only COMPRESSION parents are merged (`_COMPRESSION_CHILD_SQL`): a `/branch` copy
//! (`model_config._branched_from`) owns a copied transcript and stands alone, and reset
//! (`_reset_from`) / delegate sub-agent (`_delegate_from`) children are separate conversations that
//! merely record their lineage (`hermes_state_common.py:151-216`); merging them duplicated the parent
//! under every child.
//!
//! Since 2026-07 (schema v11→v30) compaction is IN PLACE under one session id
//! (`hermes_state_messages.py archive_and_compact`): the summarized rows get `active=0, compacted=1`
//! (still displayed as history), the summary is inserted as a fresh active row with
//! `_compressed_summary=1` (`display_kind='hidden'` for a standalone handoff), the carried tail is
//! column-cloned to fresh ids and its originals — like rewound rows — get `active=0, compacted=0`.
//! Hermes's display projection is `(active = 1 OR compacted = 1) ORDER BY id`
//! (`_DISPLAY_ACTIVE_CLAUSE`; "timestamps are not monotonic and would break tool-call adjacency"),
//! generation-deduped by `(role, content, timestamp, tool_call_id, tool_calls, tool_name)`. The lean
//! passes mirror that exactly; `complete` reads every row and tags `active`/`compacted` instead. A
//! summary row becomes a [`MessageKind::CompactionBoundary`] System marker + a
//! [`MessageKind::CompactionSummary`] System message linked to it by `parent_id` — the pair
//! `crate::compaction` detects for every harness.
//!
//! Kinds (`docs/INTERFACE-V2.md` §4): a `system` row is the [`MessageKind::SystemPrompt`] (the
//! resolved prompt is also `Session::system_prompt`); harness-stamped `display_kind` rows are System
//! turns typed by what they are — `model_switch` → [`MessageKind::ModelChange`],
//! `async_delegation_complete` → [`MessageKind::SubagentReturn`] (origin [`Origin::Subagent`]),
//! `hidden` / `process_complete` / `internal_notification` / `auto_continue` →
//! [`MessageKind::Notice`] — while a `steer` (a human's mid-turn message) stays a
//! [`MessageKind::Prompt`]. A foreign import (`origin_json.imported_from`) gives every message
//! [`Origin::Import`]. Lineage is first-class (`Session::lineage`: `_branched_from` → `forked_from`,
//! `_delegate_from` → `parent`, a compression rotation → `continues` / `continued_in`); everything
//! Hermes-specific rides in `extra["hermes"]` — the display sidecars (`api_content` = the verbatim
//! provider view of a user row, `display_kind`, `display_metadata`, `effect_disposition`), the
//! compaction flags, the raw reasoning columns, and the session row under
//! `extra["hermes"]["session"]`.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

/// Keys inside a message's `extra["hermes"]` bag under which the parser stashes the raw `reasoning`
/// / `reasoning_content` source columns verbatim (the folded `Thinking.text` projection is lossy —
/// it merges and dedups across columns — so the originals are kept here for a lossless emit
/// round-trip). Named after the columns. Public so `emit_hermes` can read them back.
pub const RAW_REASONING_KEY: &str = "reasoning";
pub const RAW_REASONING_CONTENT_KEY: &str = "reasoning_content";

/// Key inside the session's `extra["hermes"]` bag holding the per-session columns that have no
/// first-class IR home (`source`, `user_id`, `model_config`, `end_reason`, the aggregate token
/// counters, the listing flags, …) so `emit_hermes` can write them back. The system prompt is NOT
/// here (it is `Session::system_prompt`), nor is lineage (`Session::lineage`).
pub const SESSION_META_KEY: &str = "session";

/// Optional `messages` columns that older schemas may lack. We probe for each before SELECTing.
const OPTIONAL_MSG_COLS: &[&str] = &[
    "token_count",
    "finish_reason",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
    "codex_reasoning_items",
    "codex_message_items",
    "platform_message_id",
    "observed",
    // v11+ (2026-07, `hermes_cli/session_schema_history.py`): tool-effect classification, the
    // in-place compaction flags, and the display sidecars.
    "effect_disposition",
    "active",
    "compacted",
    "_compressed_summary",
    "api_content",
    "display_kind",
    "display_metadata",
];

/// What a `display_kind` Hermes stamped on a row IT injected (not typed by the human) makes the
/// turn: a hidden compaction handoff / diagnostic, auto-continue nudges and process notices are
/// notices; a model switch is a [`MessageKind::ModelChange`]; a delegation delivery is the
/// sub-agent's return. All read as System turns so titles, first-prompt previews and turn counts
/// stay honest. `None` for `steer` (a human mid-turn message: a prompt) and for kinds Hermes has
/// not documented (`personality_switch`, …), which keep their row role and carry the kind in
/// `extra["hermes"]["display_kind"]` only.
fn harness_display_kind(display_kind: &str) -> Option<(MessageKind, Origin)> {
    match display_kind {
        "model_switch" => Some((MessageKind::ModelChange, Origin::Harness)),
        "async_delegation_complete" => Some((MessageKind::SubagentReturn, Origin::Subagent)),
        "hidden" | "auto_continue" | "process_complete" | "internal_notification" => {
            Some((MessageKind::Notice, Origin::Harness))
        }
        _ => None,
    }
}

/// The `model_config` lineage markers (`_branched_from` / `_reset_from` / `_delegate_from`,
/// `hermes_state_common.py:151-216`).
const LINEAGE_MARKERS: &[&str] = &["_branched_from", "_reset_from", "_delegate_from"];

/// The `model_config` JSON lineage markers present on a session row, if any.
fn lineage_markers(model_config: Option<&str>) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let Some(Value::Object(cfg)) = model_config.and_then(|m| serde_json::from_str::<Value>(m).ok()) else {
        return out;
    };
    for k in LINEAGE_MARKERS {
        if let Some(v) = cfg.get(*k).filter(|v| !v.is_null()) {
            out.insert((*k).into(), v.clone());
        }
    }
    out
}

/// True if the DB has a table of this name (`system_prompts` arrived with schema v25).
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

pub struct Hermes {
    /// Every Hermes `state.db` we can read: the top-level `<home>/state.db` PLUS one per
    /// `<home>/profiles/*/state.db`. Hermes stores most sessions under the *active* profile (e.g.
    /// `profiles/hermes-x/state.db`), so a single top-level DB only sees a small slice. Each DB is
    /// opened independently per call (read-only) — `SessionRef.path` carries which DB a ref came
    /// from, so cross-profile session-id collisions never alias.
    dbs: Vec<PathBuf>,
}

impl Hermes {
    pub fn new() -> Self {
        // HERMES_HOME overrides ~/.hermes.
        let home = std::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".hermes")));
        let mut dbs = Vec::new();
        if let Some(home) = home {
            // Top-level DB (legacy / no-profile installs).
            let top = home.join("state.db");
            if top.exists() {
                dbs.push(top);
            }
            // Every `profiles/<name>/state.db` — depth-1 read_dir, so pre-update
            // `state-snapshots/**/state.db` are naturally skipped (never recurse).
            if let Ok(rd) = std::fs::read_dir(home.join("profiles")) {
                for e in rd.flatten() {
                    let p = e.path().join("state.db");
                    if p.exists() {
                        dbs.push(p); // hermes-x, hermes-google, …
                    }
                }
            }
        }
        Hermes { dbs }
    }

    /// An adapter over exactly one `state.db` — lets emit-verification and tests re-parse a
    /// just-written DB without mutating `HERMES_HOME` (process-global env, races other threads).
    pub fn with_db(db: PathBuf) -> Self {
        Hermes { dbs: vec![db] }
    }

    fn open_path(path: &PathBuf) -> Result<Connection> {
        // Read-only so we never touch the user's live DB / take a write lock.
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .with_context(|| format!("opening {}", path.display()))?;
        Ok(conn)
    }
}

impl Default for Hermes {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Hermes {
    fn harness(&self) -> Harness {
        Harness::Hermes
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.dbs.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // Iterate every DB (top-level + each profile), opening each read-only, and concatenate the
        // per-DB discovery. Each `SessionRef` carries its own `path`, so refs from different
        // profiles stay disambiguated even when their internal session ids collide. A single
        // unreadable DB (e.g. permission-denied) is skipped rather than failing the whole scan.
        let mut out = Vec::new();
        for db in &self.dbs {
            let conn = match Self::open_path(db) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Ok(refs) = discover_conn(&conn, db) {
                out.extend(refs);
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // Route by the ref's own DB path (set at discovery time), NOT a single self.db — this is
        // what lets one Hermes adapter span many profile DBs without id collisions.
        let conn = Self::open_path(&r.path)?;
        stream_conn(&conn, r, opts, sink)
    }
}

/// Which optional columns exist on the `messages` table of this DB (for historical schemas).
fn present_msg_cols(conn: &Connection) -> HashSet<String> {
    let mut present = HashSet::new();
    if let Ok(mut stmt) = conn.prepare("PRAGMA table_info(messages)") {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            for name in rows.flatten() {
                present.insert(name);
            }
        }
    }
    present
}

/// True if `sessions` has a given column (older schemas may lack `title`, `parent_session_id`, …).
fn session_has_col(conn: &Connection, col: &str) -> bool {
    let mut stmt = match conn.prepare("PRAGMA table_info(sessions)") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    // Not a tail expression: `rows` borrows `stmt`, which must drop before the return.
    #[allow(clippy::let_and_return)]
    let found = rows.flatten().any(|n| n == col);
    found
}

/// Row shape of the per-session metadata SELECT in [`stream_conn`] (columns guarded by
/// `session_has_col`, absent ones selected as NULL): model, started_at, ended_at, title, source,
/// parent_session_id, end_reason, cwd, git_branch, last_activity_at, model_config.
type SessionMetaRow = (
    Option<String>,
    Option<f64>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
);

/// The most recent of a session's activity stamps: Hermes bumps `last_activity_at` on every turn
/// while `ended_at` stays NULL for a live session, so it is the honest `updated_at`.
fn latest_of(stamps: [Option<f64>; 3]) -> Option<DateTime<Utc>> {
    stamps
        .into_iter()
        .flatten()
        .fold(None, |acc: Option<f64>, t| Some(acc.map_or(t, |a| a.max(t))))
        .and_then(secs_to_dt)
}

/// One `SessionRef` per CONVERSATION. Excluded: delegate sub-agent runs
/// (`model_config._delegate_from` — Hermes's own picker hides them too, and cv has no sub-agent
/// forest for Hermes yet, so they are reachable only through their parent's markers; follow-up) and
/// compression ANCESTORS — a session that ended in compression and has a continuation is listed
/// once, as its tip, whose parse merges the whole chain (Hermes lists the root and resumes to the
/// tip; cv's `parse` walks upward from the ref, so the tip is the one to list). Archived and hidden
/// sessions ARE listed: Hermes keeps both resumable (`set_session_hidden`: "still resumable"; the
/// archived view is its recovery surface), and a session cv does not list is one `cv show <id>`
/// cannot reach at all — checked against a store written by Hermes's own code. Their flags ride in
/// `Session.extra[hermes_session]` (`archived`, `hidden`, `pinned`). Branch and reset children are
/// user-visible conversations and stay listed. An untitled compression tip takes its root's title,
/// as Hermes's listing does (`COALESCE(tip.title, s.title)`, `hermes_state_sessions.py:1137`).
fn discover_conn(conn: &Connection, path: &Path) -> Result<Vec<SessionRef>> {
    let expr = |col: &str| {
        if session_has_col(conn, col) {
            col.to_string()
        } else {
            "NULL".to_string()
        }
    };
    let has_parent = session_has_col(conn, "parent_session_id");
    let sql = format!(
        "SELECT id, {title}, started_at, ended_at, message_count, {cwd}, {last}, {model_config} \
         FROM sessions ORDER BY started_at DESC",
        title = expr("title"),
        cwd = expr("cwd"),
        last = expr("last_activity_at"),
        model_config = expr("model_config"),
    );
    // Compression ancestors: parents (ended in compression) that some session continues from.
    let mut compression_parents: HashSet<String> = HashSet::new();
    if session_has_col(conn, "parent_session_id") && session_has_col(conn, "end_reason") {
        if let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT p.id FROM sessions p JOIN sessions c ON c.parent_session_id = p.id \
             WHERE p.end_reason IN ('compression', 'orphaned_compression')",
        ) {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                compression_parents.extend(rows.flatten());
            }
        }
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: Option<String> = row.get(1).ok().flatten();
        let started: Option<f64> = row.get(2).ok().flatten();
        let ended: Option<f64> = row.get(3).ok().flatten();
        let count: Option<i64> = row.get(4).ok().flatten();
        let cwd: Option<String> = row.get(5).ok().flatten();
        let last: Option<f64> = row.get(6).ok().flatten();
        let model_config: Option<String> = row.get(7).ok().flatten();
        Ok((id, title, started, ended, count, cwd, last, model_config))
    })?;
    let mut out = Vec::new();
    for (id, title, started, ended, count, cwd, last, model_config) in rows.flatten() {
        if compression_parents.contains(&id) {
            continue;
        }
        if lineage_markers(model_config.as_deref()).contains_key("_delegate_from") {
            continue;
        }
        let title = title.filter(|t| !t.is_empty()).or_else(|| {
            has_parent
                .then(|| inherited_title(conn, &session_lineage_root_to_tip(conn, &id)))
                .flatten()
        });
        out.push(SessionRef {
            id,
            harness: Harness::Hermes,
            path: path.to_path_buf(),
            cwd: cwd.filter(|c| !c.is_empty()).map(PathBuf::from),
            title: title.map(|t| crate::ir::truncate(&t, 80)),
            created_at: started.and_then(secs_to_dt),
            updated_at: latest_of([ended, last, started]),
            message_count: count.unwrap_or(0).max(0) as usize,
        });
    }
    Ok(out)
}

/// Hermes titles a compression chain `COALESCE(tip.title, root.title)` (`hermes_state_sessions.py:1137`):
/// the title is carried root→tip only after the publish transaction, so an untitled tip is the normal
/// state right after a rotation (a real 0.21.3 store: `publish_compression_child` leaves the child's
/// `title` NULL). Given a root→tip `chain` (see [`session_lineage_root_to_tip`]), the nearest titled
/// ancestor, root first; `None` for a chain of one.
fn inherited_title(conn: &Connection, chain: &[String]) -> Option<String> {
    let (_, ancestors) = chain.split_last()?;
    ancestors.iter().find_map(|id| {
        conn.query_row("SELECT title FROM sessions WHERE id = ?1", [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
    })
}

/// Per-session columns with no first-class IR home. Captured into
/// `Session.extra["hermes"][SESSION_META_KEY]` on parse and written back by `emit_hermes`, so the
/// round-trip loses no session-row data. Text columns map to JSON strings; the aggregate counters to
/// JSON numbers (see [`SESSION_META_INT_COLS`]). `source` / `end_reason` are read by the metadata
/// SELECT already and passed in (so we don't re-probe them); everything here is probed independently
/// to stay graceful on older schemas. `system_prompt` is not here: it is `Session::system_prompt`.
const SESSION_META_TEXT_COLS: &[&str] = &[
    "user_id",
    "model_config",
    // v11+: where the transcript came from and how it is labelled/routed.
    "display_name",
    "origin_json",
    "title_source",
    "profile_name",
    "transport_profile",
    "git_repo_root",
];
const SESSION_META_INT_COLS: &[&str] = &[
    // Listing flags (non-zero only, like the counters): the user tucked the session away or pinned it.
    "archived",
    "hidden",
    "pinned",
    "tool_call_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
];

/// Session-level facts read from the `sessions` row beyond the first-class columns: the contents of
/// the session's `extra["hermes"]` bag (the dropped columns under [`SESSION_META_KEY`], the
/// importer's `imported_from`), the resolved system prompt, and whether the transcript is a foreign
/// import (which makes every message's origin [`Origin::Import`]).
struct SessionFacts {
    bag: serde_json::Map<String, Value>,
    system_prompt: Option<String>,
    imported: bool,
}

/// Read the [`SessionFacts`] for `id`. `source` / `end_reason` come pre-read from the metadata
/// SELECT; the rest are probed here.
fn read_session_facts(conn: &Connection, id: &str, source: Option<&str>, end_reason: Option<&str>) -> SessionFacts {
    let mut meta = serde_json::Map::new();
    if let Some(s) = source.filter(|s| !s.is_empty()) {
        meta.insert("source".into(), Value::String(s.to_string()));
    }
    if let Some(e) = end_reason.filter(|s| !s.is_empty()) {
        meta.insert("end_reason".into(), Value::String(e.to_string()));
    }
    for col in SESSION_META_TEXT_COLS {
        if !session_has_col(conn, col) {
            continue;
        }
        let v: Option<String> = conn
            .query_row(&format!("SELECT {col} FROM sessions WHERE id = ?1"), [id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten();
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            meta.insert((*col).into(), Value::String(v));
        }
    }
    for col in SESSION_META_INT_COLS {
        if !session_has_col(conn, col) {
            continue;
        }
        let v: Option<i64> = conn
            .query_row(&format!("SELECT {col} FROM sessions WHERE id = ?1"), [id], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten();
        // Only carry non-zero aggregates: a freshly-emitted DB leaves these at their DEFAULT 0, so
        // storing 0 would make the round-trip "lose" a value that was never meaningfully set. emit
        // writes back exactly the keys present here, so omitting 0 keeps emit→parse symmetric.
        if let Some(v) = v.filter(|n| *n != 0) {
            meta.insert((*col).into(), Value::Number(v.into()));
        }
    }
    // The system prompt: schema v25 hollowed `sessions.system_prompt` out into
    // `system_prompts(hash, prompt)` keyed by `system_prompt_hash` (`hermes_state_schema.py:180-199`);
    // Hermes reads `COALESCE(sp.prompt, s.system_prompt)`, so the table wins and the legacy column is
    // the fallback for a store an older Hermes wrote.
    let str_col = |sql: &str| -> Option<String> {
        conn.query_row(sql, [id], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
    };
    let mut system_prompt = None;
    if session_has_col(conn, "system_prompt_hash") && table_exists(conn, "system_prompts") {
        system_prompt = str_col(
            "SELECT sp.prompt FROM sessions s JOIN system_prompts sp ON sp.hash = s.system_prompt_hash \
             WHERE s.id = ?1",
        );
    }
    if system_prompt.is_none() && session_has_col(conn, "system_prompt") {
        system_prompt = str_col("SELECT system_prompt FROM sessions WHERE id = ?1");
    }
    let mut bag = serde_json::Map::new();
    // A foreign import (`hermes sessions import --from claude|codex`, `hermes_cli/foreign_sessions.py`)
    // records its provenance in `origin_json.imported_from{tool, path, foreign_session_id}` — the
    // very transcript cv also parses natively. Surface it in the bag so consumers can dedupe; the
    // messages themselves carry `Origin::Import`.
    let imported_from = meta
        .get("origin_json")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|origin| origin.get("imported_from").cloned())
        .filter(Value::is_object);
    let imported = imported_from.is_some();
    if let Some(imported_from) = imported_from {
        bag.insert("imported_from".into(), imported_from);
    }
    if !meta.is_empty() {
        bag.insert(SESSION_META_KEY.into(), Value::Object(meta));
    }
    SessionFacts {
        bag,
        system_prompt,
        imported,
    }
}

/// The compression continuation of `id`, if any: the child whose `parent_session_id` is `id` and
/// that carries no branch / reset / delegate marker (`_COMPRESSION_CHILD_SQL`). Only meaningful for
/// a session that ended in compression; `None` when the store does not say.
fn compression_child(conn: &Connection, id: &str) -> Option<String> {
    if !session_has_col(conn, "parent_session_id") {
        return None;
    }
    let model_config = if session_has_col(conn, "model_config") {
        "model_config"
    } else {
        "NULL"
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, {model_config} FROM sessions WHERE parent_session_id = ?1 ORDER BY started_at ASC"
        ))
        .ok()?;
    let rows = stmt
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1).ok().flatten()))
        })
        .ok()?;
    // Not a tail expression: `rows` borrows `stmt`, which must drop before the return.
    #[allow(clippy::let_and_return)]
    let found = rows
        .flatten()
        .find(|(_, mc)| lineage_markers(mc.as_deref()).is_empty())
        .map(|(child, _)| child);
    found
}

fn stream_conn(conn: &Connection, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    // session-level metadata (guard optional columns for historical schemas).
    let expr = |col: &str| {
        if session_has_col(conn, col) {
            col.to_string()
        } else {
            "NULL".to_string()
        }
    };
    let has_parent = session_has_col(conn, "parent_session_id");
    let meta_sql = format!(
        "SELECT {model}, started_at, ended_at, {title}, {source}, {parent}, {end_reason}, {cwd}, \
         {git_branch}, {last}, {model_config} FROM sessions WHERE id = ?1",
        model = expr("model"),
        title = expr("title"),
        source = expr("source"),
        parent = expr("parent_session_id"),
        end_reason = expr("end_reason"),
        cwd = expr("cwd"),
        git_branch = expr("git_branch"),
        last = expr("last_activity_at"),
        model_config = expr("model_config"),
    );
    let (model, started, ended, title, source, parent, end_reason, cwd, git_branch, last_activity, model_config): SessionMetaRow =
        conn.query_row(&meta_sql, [&r.id], |row| {
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
            ))
        })
        .unwrap_or((None, None, None, r.title.clone(), None, None, None, None, None, None, None));

    // Format-complete: capture the session columns that have no first-class IR home, so emit can
    // write them back (small metadata only — text/aggregate counters, never message bodies). The tip
    // session's row is the one whose metadata survives (lineage flattening keeps the tip).
    let SessionFacts {
        bag: mut hermes_bag,
        system_prompt,
        imported,
    } = read_session_facts(conn, &r.id, source.as_deref(), end_reason.as_deref());
    // A foreign import: every message of the session came through the importer.
    let session_origin = imported.then_some(Origin::Import);

    // Walk the compression lineage root→tip so a compressed-and-continued conversation reads as a
    // single transcript (mirrors `_resume_lineage_ids`); branches/resets/delegates stand alone.
    let chain = if has_parent {
        session_lineage_root_to_tip(conn, &r.id)
    } else {
        vec![r.id.clone()]
    };
    // Lineage, first-class: a `/branch` copy points at what it was branched from, a delegate run at
    // the session that spawned it, a compression rotation at the session it continues (whose rows
    // are merged into this transcript) and — for a session that ended in compression — at the
    // session that continued it. `_reset_from` (a fresh conversation after `/new`; nothing carried
    // over) has no IR field and stays a Hermes fact, as does an unmarked parent that was not merged.
    let markers = lineage_markers(model_config.as_deref());
    let marker = |k: &str| markers.get(k).and_then(Value::as_str).map(str::to_string);
    let mut lineage = Lineage {
        forked_from: marker("_branched_from"),
        parent: marker("_delegate_from"),
        ..Default::default()
    };
    if chain.len() > 1 {
        lineage.continues = chain.get(chain.len() - 2).cloned();
    }
    if matches!(
        end_reason.as_deref(),
        Some("compression") | Some("orphaned_compression")
    ) {
        lineage.continued_in = compression_child(conn, &r.id);
    }
    if let Some(reset) = marker("_reset_from") {
        hermes_bag.insert("_reset_from".into(), Value::String(reset));
    }
    if chain.len() == 1 && markers.is_empty() {
        if let Some(p) = parent.filter(|p| !p.is_empty()) {
            hermes_bag.insert("parent_session_id".into(), Value::String(p));
        }
    }
    let mut extra = serde_json::Map::new();
    if !hermes_bag.is_empty() {
        extra.insert(Harness::Hermes.as_str().into(), Value::Object(hermes_bag));
    }

    let s = Session {
        id: r.id.clone(),
        harness: Harness::Hermes,
        cwd: cwd
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title: title
            .filter(|t| !t.is_empty())
            .or_else(|| r.title.clone())
            .or_else(|| inherited_title(conn, &chain)),
        created_at: started.and_then(secs_to_dt).or(r.created_at),
        updated_at: latest_of([ended, last_activity, started]).or(r.updated_at),
        model,
        git: git_branch.filter(|b| !b.is_empty()).map(|branch| GitInfo {
            branch: Some(branch),
            ..Default::default()
        }),
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra,
        system_prompt,
        lineage,
    };

    let cols = present_msg_cols(conn);
    let has = |c: &str| cols.contains(c);

    // Build the SELECT defensively: required cols always present; optional cols gated by probe.
    let mut select_cols: Vec<&str> = vec![
        "id",
        "role",
        "content",
        "tool_call_id",
        "tool_calls",
        "tool_name",
        "timestamp",
    ];
    for c in OPTIONAL_MSG_COLS {
        if has(c) {
            select_cols.push(c);
        }
    }
    let select_list = select_cols.join(", ");

    // Row visibility (schema v12+): Hermes's display projection is `(active = 1 OR compacted = 1)`
    // — live rows plus the history a compaction summarized away — which drops rewound rows and the
    // superseded originals of carried tails. `complete` reads every row instead and tags the flags.
    let has_flags = has("active") && has("compacted");
    let visibility = if has_flags && !opts.complete {
        " AND (active = 1 OR compacted = 1)"
    } else {
        ""
    };
    // Display-generation dedup (`_dedupe_display_generations`): a compaction copies the protected
    // tail into each generation — same role/content/timestamp/tool fields, different id/flags — so
    // each logical message is emitted once (its first row; the copies are byte-identical).
    let dedup_generations = has("compacted") && !opts.complete;
    let mut seen_generations: HashSet<u64> = HashSet::new();

    // Hand the session-level metadata to the sink before the body (header-rendering sinks use it).
    sink.meta(&s);

    // Dedup the replayed first user message at compression boundaries — streamed: a tiny bounded
    // look-back (the trailing run of user single-texts since the last substantive assistant turn),
    // not the whole transcript, so peak stays O(one message).
    let mut dedup = ReplayDedup::default();
    'lineage: for sid in &chain {
        // `ORDER BY id`, as Hermes itself reads (`_ACTIVE_IDS_SQL`, `_fetch_conversation_rows`):
        // timestamps are not monotonic and would split a tool call from its result.
        let sql = format!("SELECT {select_list} FROM messages WHERE session_id = ?1{visibility} ORDER BY id ASC");
        let mut stmt = conn.prepare(&sql)?;
        // Map column name -> index so we can read by name regardless of which optionals exist.
        let idx = |name: &str| select_cols.iter().position(|c| *c == name);
        // Stream rows lazily: one `MsgRow` -> one `Message` at a time, emitted and dropped before
        // the next row (a single Hermes session can be large; never materialize the whole Vec).
        let mut rows = stmt.query_map([sid], |row| {
            let get_str = |name: &str| -> Option<String> {
                idx(name).and_then(|i| row.get::<_, Option<String>>(i).ok().flatten())
            };
            let get_i64 =
                |name: &str| -> Option<i64> { idx(name).and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten()) };
            Ok(MsgRow {
                id: get_i64("id").unwrap_or(0),
                role: get_str("role").unwrap_or_default(),
                content: get_str("content"),
                tool_call_id: get_str("tool_call_id"),
                tool_calls: get_str("tool_calls"),
                tool_name: get_str("tool_name"),
                timestamp: idx("timestamp").and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten()),
                token_count: get_i64("token_count"),
                finish_reason: get_str("finish_reason"),
                reasoning: get_str("reasoning"),
                reasoning_content: get_str("reasoning_content"),
                reasoning_details: get_str("reasoning_details"),
                codex_reasoning_items: get_str("codex_reasoning_items"),
                codex_message_items: get_str("codex_message_items"),
                platform_message_id: get_str("platform_message_id"),
                observed: get_i64("observed"),
                effect_disposition: get_str("effect_disposition"),
                active: get_i64("active"),
                compacted: get_i64("compacted"),
                compressed_summary: matches!(get_i64("_compressed_summary"), Some(n) if n != 0),
                api_content: get_str("api_content"),
                display_kind: get_str("display_kind"),
                display_metadata: get_str("display_metadata"),
            })
        })?;

        for row in rows.by_ref() {
            let Ok(row) = row else { continue };
            if dedup_generations && !seen_generations.insert(row.generation_key()) {
                continue;
            }
            let row_id = row.id;
            let Some(mut m) = row.into_message(session_origin) else {
                continue;
            };
            // A compaction summary row: emit the boundary marker first, then the summary linked to
            // it by `parent_id` — the `CompactionBoundary` + `CompactionSummary` pair
            // `crate::compaction` detects for every harness.
            if m.kind == MessageKind::CompactionSummary {
                let boundary_id = format!("compact-{row_id}");
                let mut boundary = Message::of_kind(Role::System, MessageKind::CompactionBoundary, m.origin);
                boundary.id = Some(boundary_id.clone());
                boundary.timestamp = m.timestamp;
                boundary.content.push(Block::Text {
                    text: "[conversation compacted]".to_string().into(),
                });
                if sink.message(boundary) == Flow::Stop {
                    break 'lineage;
                }
                m.parent_id = Some(boundary_id);
                if sink.message(m) == Flow::Stop {
                    break 'lineage;
                }
                continue;
            }
            // Dedup the replayed first user message at compression boundaries (`complete` keeps it).
            if !opts.complete && dedup.is_duplicate(&m) {
                continue;
            }
            dedup.observe(&m);
            if sink.message(m) == Flow::Stop {
                break 'lineage;
            }
        }
    }
    Ok(s)
}

/// Streaming equivalent of [`is_duplicate_replayed_user_message`]: tracks only the trailing run of
/// user single-texts since the last substantive assistant turn (the exact look-back window the
/// whole-Vec scan uses), so the streaming path stays byte-identical without retaining the session.
#[derive(Default)]
struct ReplayDedup {
    /// Single-texts of consecutive trailing user messages (most recent last). Cleared when a
    /// substantive assistant turn passes (which ends the look-back window).
    recent_user_texts: Vec<String>,
}

impl ReplayDedup {
    /// Mirror of the look-back in [`is_duplicate_replayed_user_message`].
    fn is_duplicate(&self, msg: &Message) -> bool {
        if msg.role != Role::User {
            return false;
        }
        let Some(text) = single_text(msg) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        self.recent_user_texts.iter().any(|t| t == &text)
    }

    /// Fold an *emitted* message into the look-back state.
    fn observe(&mut self, m: &Message) {
        match m.role {
            Role::User => {
                if let Some(text) = single_text(m) {
                    self.recent_user_texts.push(text);
                }
            }
            Role::Assistant => {
                // A substantive assistant turn (text or tool call) ends the look-back window.
                let has_content = m
                    .content
                    .iter()
                    .any(|b| matches!(b, Block::Text { .. } | Block::ToolUse { .. }));
                if has_content {
                    self.recent_user_texts.clear();
                }
            }
            _ => {}
        }
    }
}

/// Walk `parent_session_id` from `session_id` up through COMPRESSION continuations only, returning
/// root→tip order — the ids a Hermes display resume materializes (`_resume_lineage_ids` /
/// `_session_lineage_root_to_tip`, `_COMPRESSION_CHILD_SQL`). A `/branch` copy (`_branched_from`)
/// owns a copied transcript and stands alone; reset (`_reset_from`) and delegate (`_delegate_from`)
/// children are separate conversations that only record their lineage; a parent that ended for any
/// reason other than compression is not a continuation either. Bounded + cycle-guarded.
fn session_lineage_root_to_tip(conn: &Connection, session_id: &str) -> Vec<String> {
    if session_id.is_empty() {
        return vec![session_id.to_string()];
    }
    let has_end_reason = session_has_col(conn, "end_reason");
    let has_model_config = session_has_col(conn, "model_config");
    let str_col = |col: &str, id: &str| -> Option<String> {
        conn.query_row(&format!("SELECT {col} FROM sessions WHERE id = ?1"), [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    };
    let mut chain: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = session_id.to_string();
    for _ in 0..100 {
        if current.is_empty() || seen.contains(&current) {
            break;
        }
        seen.insert(current.clone());
        chain.push(current.clone());
        // An explicit lineage marker on this row ends the walk: it is a branch/reset/delegate child.
        if has_model_config && !lineage_markers(str_col("model_config", &current).as_deref()).is_empty() {
            break;
        }
        let Some(parent) = str_col("parent_session_id", &current).filter(|p| !p.is_empty()) else {
            break;
        };
        // Merge only through a parent that ended in compression (the continuation needs its rows).
        if has_end_reason
            && !matches!(
                str_col("end_reason", &parent).as_deref(),
                Some("compression") | Some("orphaned_compression")
            )
        {
            break;
        }
        current = parent;
    }
    chain.reverse();
    if chain.is_empty() {
        vec![session_id.to_string()]
    } else {
        chain
    }
}

/// Mirrors `SessionDB._is_duplicate_replayed_user_message`: a compression continuation re-injects
/// the boundary's last user message as its first message; suppress that duplicate when merging.
/// The streaming equivalent is [`ReplayDedup`] (a bounded look-back over the trailing user run).
///
/// Plain text of a message iff it is exactly one Text block (the shape Hermes replays as a string).
fn single_text(m: &Message) -> Option<String> {
    if m.content.len() == 1 {
        if let Block::Text { text } = &m.content[0] {
            return Some(text.to_string());
        }
    }
    None
}

struct MsgRow {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    token_count: Option<i64>,
    finish_reason: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<String>,
    codex_reasoning_items: Option<String>,
    codex_message_items: Option<String>,
    platform_message_id: Option<String>,
    observed: Option<i64>,
    // v11+ columns (NULL / default on older schemas).
    effect_disposition: Option<String>,
    active: Option<i64>,
    compacted: Option<i64>,
    compressed_summary: bool,
    api_content: Option<String>,
    display_kind: Option<String>,
    display_metadata: Option<String>,
}

impl MsgRow {
    /// Hermes's display-generation identity (`_display_dedupe_key`): the fields a compaction clone
    /// copies byte-exact. Hashed so the dedup set costs one word per row.
    fn generation_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.role.hash(&mut h);
        self.content.hash(&mut h);
        self.timestamp.map(f64::to_bits).hash(&mut h);
        self.tool_call_id.hash(&mut h);
        self.tool_calls.hash(&mut h);
        self.tool_name.hash(&mut h);
        h.finish()
    }

    /// The IR message for this row. `session_origin` overrides every message's origin when the
    /// session as a whole came from somewhere else (a foreign import).
    fn into_message(self, session_origin: Option<Origin>) -> Option<Message> {
        let (mut role, mut kind, mut origin) = match self.role.as_str() {
            "assistant" => (Role::Assistant, MessageKind::Reply, Origin::Model),
            "tool" => (Role::Tool, MessageKind::ToolResult, Origin::Harness),
            // The system prompt, stored as a row (older Hermes; `Session::system_prompt` today).
            "system" => (Role::System, MessageKind::SystemPrompt, Origin::Harness),
            // `user`, and anything unknown.
            _ => (Role::User, MessageKind::Prompt, Origin::Human),
        };
        // Harness-injected rows (a hidden compaction handoff, auto-continue nudges, model-switch /
        // delegation / process notices) are System turns typed by what they are, never prompts.
        if let Some((k, o)) = self.display_kind.as_deref().and_then(harness_display_kind) {
            role = Role::System;
            kind = k;
            origin = o;
        }
        // A compaction summary is what seeds the next context window: never a human prompt.
        if self.compressed_summary {
            role = Role::System;
            kind = MessageKind::CompactionSummary;
            origin = Origin::Harness;
        }
        if let Some(o) = session_origin {
            origin = o;
        }
        let mut m = Message::of_kind(role, kind, origin);
        m.timestamp = self.timestamp.and_then(secs_to_dt);

        // Everything Hermes-specific goes in the `extra["hermes"]` bag, built here and attached once.
        let mut bag = serde_json::Map::new();

        // In-place compaction / rewind flags (non-default values only, so old-schema rows carry
        // nothing): `compacted` = summarized away but still displayed history; `active = false`
        // alone = a rewound or superseded row (only reachable under `complete`).
        if matches!(self.compacted, Some(c) if c != 0) {
            bag.insert("compacted".into(), Value::Bool(true));
        }
        if matches!(self.active, Some(0)) {
            bag.insert("active".into(), Value::Bool(false));
        }
        if let Some(k) = self.display_kind.as_deref().filter(|s| !s.is_empty()) {
            bag.insert("display_kind".into(), Value::String(k.to_string()));
        }
        if let Some(ed) = self.effect_disposition.as_deref().filter(|s| !s.is_empty()) {
            bag.insert("effect_disposition".into(), Value::String(ed.to_string()));
        }
        // `api_content`: the verbatim provider view of this (user) row — what was actually sent, when
        // it differs from the displayed `content` (e.g. injected context). Kept as a sidecar so the
        // block text stays the user's own words.
        if let Some(api) = self.api_content.as_deref().filter(|s| !s.is_empty()) {
            bag.insert("api_content".into(), Value::String(api.to_string()));
        }
        if let Some(raw) = self.display_metadata.as_deref().filter(|s| !s.is_empty()) {
            let v = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
            bag.insert("display_metadata".into(), v);
        }

        // Per-message token_count → Usage. Hermes records a single combined count per message; we
        // route it to output_tokens for assistant turns and input_tokens otherwise (best-effort).
        if let Some(tc) = self.token_count {
            if tc > 0 {
                let mut u = Usage::default();
                if role == Role::Assistant {
                    u.output_tokens = Some(tc as u64);
                } else {
                    u.input_tokens = Some(tc as u64);
                }
                m.usage = Some(u);
            }
        }

        if let Some(fr) = self.finish_reason.filter(|s| !s.is_empty()) {
            bag.insert("finish_reason".into(), Value::String(fr));
        }
        if let Some(pmid) = self.platform_message_id.filter(|s| !s.is_empty()) {
            bag.insert("platform_message_id".into(), Value::String(pmid));
        }
        if matches!(self.observed, Some(o) if o != 0) {
            bag.insert("observed".into(), Value::Bool(true));
        }

        // Format-complete capture: keep the raw reasoning source columns verbatim so a parse→emit
        // round-trip reproduces them byte-for-byte. The summary text we fold into `Thinking.text`
        // below is a *projection* (it merges `reasoning` + `reasoning_content` + the text entries of
        // `reasoning_details` and dedups them); reconstructing the original columns from that
        // projection is impossible, so we stash the originals here. These are small metadata strings
        // (summaries / encrypted handles), never large tool payloads. `reasoning_details` /
        // `codex_*` are already stashed (as parsed JSON) further down.
        if let Some(r) = self.reasoning.as_deref().filter(|s| !s.is_empty()) {
            bag.insert(RAW_REASONING_KEY.into(), Value::String(r.to_string()));
        }
        if let Some(r) = self.reasoning_content.as_deref().filter(|s| !s.is_empty()) {
            bag.insert(RAW_REASONING_CONTENT_KEY.into(), Value::String(r.to_string()));
        }

        // Reasoning: combine summary text from `reasoning`, `reasoning_content`, and the text
        // entries of `reasoning_details`; collect any encrypted blob (reasoning_details or codex).
        let mut reasoning_text = String::new();
        let mut push_reasoning = |s: &str| {
            let s = s.trim();
            if s.is_empty() {
                return;
            }
            if !reasoning_text.is_empty() {
                reasoning_text.push_str("\n\n");
            }
            reasoning_text.push_str(s);
        };
        if let Some(r) = self.reasoning.as_deref() {
            push_reasoning(r);
        }
        if let Some(r) = self.reasoning_content.as_deref() {
            // Avoid duplicating when identical to `reasoning` (Hermes' own dedup behaviour).
            if Some(r) != self.reasoning.as_deref() {
                push_reasoning(r);
            }
        }
        let mut encrypted: Option<String> = None;
        if let Some(raw) = self.reasoning_details.as_deref() {
            let (texts, enc) = parse_reasoning_details(raw);
            for t in texts {
                push_reasoning(&t);
            }
            encrypted = encrypted.or(enc);
            // Preserve the full structured blob for lossless round-tripping.
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                bag.insert("reasoning_details".into(), v);
            }
        }
        if let Some(raw) = self.codex_reasoning_items.as_deref() {
            encrypted = encrypted.or_else(|| extract_codex_encrypted(raw));
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                bag.insert("codex_reasoning_items".into(), v);
            }
        }
        if let Some(raw) = self.codex_message_items.as_deref() {
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                bag.insert("codex_message_items".into(), v);
            }
        }
        if !reasoning_text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: reasoning_text.into(),
                signature: None,
                encrypted,
                redacted: false,
            });
        }
        if !bag.is_empty() {
            *m.harness_extra_mut(Harness::Hermes) = bag;
        }

        // Tool result rows carry the output in `content`; the tool's name is the block's.
        if role == Role::Tool {
            m.content.push(Block::ToolResult {
                tool_use_id: self.tool_call_id.unwrap_or_default(),
                content: self.content.unwrap_or_default().into(),
                is_error: false,
                tool_name: self.tool_name,
                status: None,
                details: None,
            });
            return Some(m);
        }

        // text / multimodal content
        if let Some(raw) = &self.content {
            for b in decode_content(raw) {
                m.content.push(b);
            }
        }

        // assistant tool calls
        if let Some(tc) = &self.tool_calls {
            for b in parse_tool_calls(tc) {
                m.content.push(b);
            }
        }

        (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
    }
}

fn decode_content(raw: &str) -> Vec<Block> {
    if let Some(rest) = raw.strip_prefix(MULTIMODAL_SENTINEL) {
        match serde_json::from_str::<Value>(rest) {
            Ok(Value::Array(items)) => return items.iter().filter_map(part_to_block).collect(),
            // Dict-shaped content (provider wrappers, e.g. {"parts":[…]}). Best-effort: pull any
            // string fields out; otherwise stash nothing and fall through to the empty case.
            Ok(Value::Object(map)) => {
                if let Some(blocks) = object_content_to_blocks(&map) {
                    return blocks;
                }
            }
            _ => {}
        }
    }
    if raw.is_empty() {
        vec![]
    } else {
        vec![Block::Text {
            text: raw.to_string().into(),
        }]
    }
}

/// Best-effort extraction from a dict-shaped (non-array) decoded content payload.
fn object_content_to_blocks(map: &serde_json::Map<String, Value>) -> Option<Vec<Block>> {
    // Common shape: {"parts": [ {"text": "…"} | {"type":…} ]}
    if let Some(Value::Array(parts)) = map.get("parts") {
        let blocks: Vec<Block> = parts
            .iter()
            .filter_map(|p| {
                part_to_block(p).or_else(|| {
                    p.get("text").and_then(Value::as_str).map(|t| Block::Text {
                        text: t.to_string().into(),
                    })
                })
            })
            .collect();
        if !blocks.is_empty() {
            return Some(blocks);
        }
    }
    None
}

fn part_to_block(part: &Value) -> Option<Block> {
    match part.get("type").and_then(Value::as_str)? {
        // OpenAI chat (`text`) and Responses (`input_text`/`output_text`) text parts.
        "text" | "input_text" | "output_text" => Some(Block::Text {
            text: part.get("text").and_then(Value::as_str)?.to_string().into(),
        }),
        // `image_url` parts hold either `{image_url:{url}}` or, for `input_image`, `{image_url:"…"}`.
        "image_url" | "input_image" => {
            let url = part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .or_else(|| part.get("image_url").and_then(Value::as_str));
            Some(Block::Image {
                media_type: None,
                data_ref: url.map(|u| crate::ir::truncate(u, 120)),
            })
        }
        _ => None,
    }
}

/// Hermes `tool_calls` is an OpenAI-shaped array: `[{id,type,function:{name,arguments}}]`.
fn parse_tool_calls(raw: &str) -> Vec<Block> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    items
        .iter()
        .map(|tc| {
            let id = tc.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .or_else(|| tc.pointer("/function/arguments").cloned())
                .unwrap_or(Value::Null);
            Block::ToolUse {
                id,
                name,
                input,
                namespace: None,
            }
        })
        .collect()
}

/// `reasoning_details` is an OpenRouter-unified array. Each item may carry summary/thinking text
/// (`reasoning.summary`, `thinking`, `redacted_thinking`) or an encrypted blob
/// (`reasoning.encrypted_content`). Returns (collected text summaries, first encrypted blob).
fn parse_reasoning_details(raw: &str) -> (Vec<String>, Option<String>) {
    let mut texts = Vec::new();
    let mut encrypted = None;
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return (texts, encrypted);
    };
    for it in &items {
        if encrypted.is_none() {
            if let Some(enc) = it.get("encrypted_content").and_then(Value::as_str) {
                encrypted = Some(enc.to_string());
            }
        }
        // Summary/thinking text lives under one of several keys (provider-dependent).
        let text = it
            .get("summary")
            .or_else(|| it.get("thinking"))
            .or_else(|| it.get("content"))
            .or_else(|| it.get("text"))
            .and_then(Value::as_str);
        if let Some(t) = text {
            if !t.is_empty() {
                texts.push(t.to_string());
            }
        }
    }
    (texts, encrypted)
}

/// `codex_reasoning_items` is `[{type:"reasoning", id, encrypted_content}]`.
fn extract_codex_encrypted(raw: &str) -> Option<String> {
    let Value::Array(items) = serde_json::from_str::<Value>(raw).ok()? else {
        return None;
    };
    items
        .iter()
        .find_map(|it| it.get("encrypted_content").and_then(Value::as_str).map(str::to_string))
}

fn secs_to_dt(s: f64) -> Option<DateTime<Utc>> {
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    Utc.timestamp_millis_opt((s * 1000.0) as i64).single()
}

/// Whole-`Session` convenience over [`stream_conn`] for tests: stream into a [`CollectSink`] and
/// reattach the messages. The production path goes through `Adapter::parse` → `stream::collect`.
#[cfg(test)]
fn parse_conn(conn: &Connection, r: &SessionRef) -> Result<Session> {
    parse_conn_with(conn, r, &ParseOptions::full())
}

#[cfg(test)]
fn parse_conn_with(conn: &Connection, r: &SessionRef, opts: &ParseOptions) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_conn(conn, r, opts, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Schema matching hermes_state.py SCHEMA_VERSION 14.
    const SCHEMA_V14: &str = "
        CREATE TABLE sessions (
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
            title TEXT
        );
        CREATE TABLE messages (
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
        );
    ";

    /// An older (pre-v14-ish) schema: only the columns Hermes had before the reasoning/codex/token
    /// additions. Exercises the column-probing degradation path.
    const SCHEMA_OLD: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            model TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            message_count INTEGER DEFAULT 0,
            title TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL
        );
    ";

    fn mk_conn(schema: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        conn
    }

    /// A message's `extra["hermes"]` bag (the only place a Hermes message fact may live).
    fn bag(m: &Message) -> &serde_json::Map<String, Value> {
        m.harness_extra(Harness::Hermes).expect("extra[\"hermes\"]")
    }

    /// The session-row facts, `extra["hermes"]["session"]`.
    fn smeta(s: &Session) -> &Value {
        &s.harness_extra(Harness::Hermes).expect("extra[\"hermes\"]")[SESSION_META_KEY]
    }

    /// Every `extra` key, on the session and on each message, is `hermes` (or the carrier's
    /// `_record`): no flat harness keys anywhere.
    /// The shared IR-v2 nesting invariant ([`crate::harness::assert_no_flat_keys`]), tightened for
    /// Hermes: the only namespace a Hermes session may carry is `hermes` (plus cv's own `cv` bag
    /// and the `_record` carrier), so a fact leaking into some other harness's bag also fails.
    fn assert_nested_extra(s: &Session) {
        crate::harness::assert_no_flat_keys(s);
        let allowed = |k: &str| {
            k == Harness::Hermes.as_str() || k == crate::ir::CV_NAMESPACE || k == crate::harness::claude::CARRIER_KEY
        };
        for k in s.extra.keys() {
            assert!(allowed(k), "flat session extra key {k:?}");
        }
        for (i, m) in s.messages.iter().enumerate() {
            for k in m.extra.keys() {
                assert!(allowed(k), "flat extra key {k:?} on message {i}");
            }
        }
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Hermes,
            path: PathBuf::from(":memory:"),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        parent: Option<&str>,
        end_reason: Option<&str>,
        started: f64,
        ended: Option<f64>,
    ) {
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, ended_at, end_reason, title) \
             VALUES (?1, 'cli', 'nous/hermes-4', ?2, ?3, ?4, ?5, 'T')",
            rusqlite::params![id, parent, started, ended, end_reason],
        )
        .unwrap();
    }

    #[test]
    fn parses_basic_text_conversation() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user','Hello',1001.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, token_count, finish_reason) \
             VALUES ('s1','assistant','Hi there!',1002.0, 42, 'stop')",
            [],
        )
        .unwrap();

        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("Hello"));
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::Reply, Origin::Model)
        );
        assert_eq!(s.messages[1].text().as_deref(), Some("Hi there!"));
        // token_count → Usage.output_tokens for assistant.
        assert_eq!(s.messages[1].usage.as_ref().unwrap().output_tokens, Some(42));
        assert_eq!(bag(&s.messages[1])["finish_reason"], "stop");
        assert_eq!(s.model.as_deref(), Some("nous/hermes-4"));
        assert!(s.messages[0].extra.is_empty(), "a plain prompt carries no Hermes facts");
        assert_nested_extra(&s);
    }

    #[test]
    fn system_row_is_the_system_prompt() {
        // Older Hermes stored the system prompt as a `system` row; it is `SystemPrompt`, not a notice,
        // and the session-level copy comes from the column.
        let conn = mk_conn(SCHEMA_V14);
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, system_prompt) VALUES ('s', 'cli', 1000.0, 'You are Hermes')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','system','You are Hermes',1000.5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','user','hi',1001.0)",
            [],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are Hermes"));
        assert_eq!(s.messages[0].role, Role::System);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::SystemPrompt, Origin::Harness)
        );
        assert_eq!(s.messages[1].kind, MessageKind::Prompt);
    }

    #[test]
    fn decodes_multimodal_sentinel() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:[{\"type\":\"text\",\"text\":\"see this\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,AAAA\"}}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 1);
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text == "see this"));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/png")));
    }

    #[test]
    fn decodes_input_image_string_form() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        // input_image with image_url as a bare string (Responses API form).
        let content = "\u{0}json:[{\"type\":\"input_text\",\"text\":\"hi\"},{\"type\":\"input_image\",\"image_url\":\"data:image/jpeg;base64,ZZZ\"}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text == "hi"));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/jpeg")));
    }

    #[test]
    fn decodes_dict_shaped_content() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:{\"parts\":[{\"text\":\"hi\"}]}";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
    }

    #[test]
    fn parses_tool_call_and_result() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let tc = "[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','',?1,1001.0)",
            [tc],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp) \
             VALUES ('s1','tool','{\"ok\":true}','c1','web_search',1002.0)",
            [],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        let tu = &s.messages[0].content[0];
        assert!(matches!(tu, Block::ToolUse { name, id, .. } if name == "web_search" && id == "c1"));
        if let Block::ToolUse { input, .. } = tu {
            assert_eq!(input.get("q").and_then(Value::as_str), Some("x"));
        }
        let tr = &s.messages[1].content[0];
        assert!(
            matches!(tr, Block::ToolResult { tool_use_id, tool_name, .. } if tool_use_id == "c1" && tool_name.as_deref() == Some("web_search")),
            "the tool name lives on the block, not in extra"
        );
        assert_eq!(s.messages[1].role, Role::Tool);
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::ToolResult, Origin::Harness)
        );
        assert!(s.messages[1].extra.is_empty());
    }

    #[test]
    fn maps_all_reasoning_variants() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let details = "[{\"type\":\"reasoning.summary\",\"summary\":\"summarised\"},{\"type\":\"reasoning.encrypted_content\",\"encrypted_content\":\"ENC123\"}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, reasoning, reasoning_content, reasoning_details) \
             VALUES ('s1','assistant','answer',1001.0,'short summary','native scratchpad',?1)",
            [details],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let think = s.messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                Block::Thinking { text, encrypted, .. } => Some((text.clone(), encrypted.clone())),
                _ => None,
            })
            .unwrap();
        assert!(think.0.contains("short summary"));
        assert!(think.0.contains("native scratchpad"));
        assert!(think.0.contains("summarised"));
        assert_eq!(think.1.as_deref(), Some("ENC123"));
        // structured blob and the raw source columns preserved in the Hermes bag
        let b = bag(&s.messages[0]);
        assert!(b.contains_key("reasoning_details"));
        assert_eq!(b[RAW_REASONING_KEY], "short summary");
        assert_eq!(b[RAW_REASONING_CONTENT_KEY], "native scratchpad");
        assert_nested_extra(&s);
    }

    #[test]
    fn reasoning_content_not_duplicated_when_equal() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, reasoning, reasoning_content) \
             VALUES ('s1','assistant','a',1001.0,'same','same')",
            [],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let text = s.messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                Block::Thinking { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(text, "same");
    }

    #[test]
    fn maps_codex_items() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let cri = "[{\"type\":\"reasoning\",\"id\":\"rs_a\",\"encrypted_content\":\"BLOB\"}]";
        let cmi = "[{\"type\":\"message\",\"phase\":\"final_answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"Done\"}]}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, codex_reasoning_items, codex_message_items) \
             VALUES ('s1','assistant','Done',1001.0,?1,?2)",
            [cri, cmi],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let enc = s.messages[0].content.iter().find_map(|b| match b {
            Block::Thinking { encrypted, .. } => encrypted.clone(),
            _ => None,
        });
        assert_eq!(enc.as_deref(), Some("BLOB"));
        assert!(bag(&s.messages[0]).contains_key("codex_reasoning_items"));
        assert!(bag(&s.messages[0]).contains_key("codex_message_items"));
    }

    #[test]
    fn platform_message_id_and_observed() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, platform_message_id, observed) \
             VALUES ('s1','user','hi',1001.0,'abc-123',1)",
            [],
        )
        .unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(bag(&s.messages[0])["platform_message_id"], "abc-123");
        assert_eq!(bag(&s.messages[0])["observed"], Value::Bool(true));
    }

    #[test]
    fn walks_compression_chain_and_dedups_replayed_user() {
        let conn = mk_conn(SCHEMA_V14);
        // root ended via compression at t=2000; child created after, replays the last user msg.
        insert_session(&conn, "root", None, Some("compression"), 1000.0, Some(2000.0));
        insert_session(&conn, "child", Some("root"), None, 2001.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','first prompt',1001.0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','assistant','first answer',1002.0)", []).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','second prompt',1003.0)",
            [],
        )
        .unwrap();
        // child replays 'second prompt' then continues.
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','user','second prompt',2002.0)", []).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','assistant','second answer',2003.0)", []).unwrap();

        // Parsing the CHILD should walk up to the root and merge, deduping the replayed user msg.
        let s = parse_conn(&conn, &sref("child")).unwrap();
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["first prompt", "first answer", "second prompt", "second answer"]
        );
    }

    #[test]
    fn old_schema_missing_columns_still_parses() {
        let conn = mk_conn(SCHEMA_OLD);
        conn.execute(
            "INSERT INTO sessions (id, source, model, started_at, title) VALUES ('s1','cli','m',1000.0,'T')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user','hello old',1001.0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','',?1,1002.0)",
            ["[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]"]).unwrap();
        // Must not error despite missing reasoning/token/codex/parent columns.
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello old"));
        assert!(matches!(&s.messages[1].content[0], Block::ToolUse { name, .. } if name == "f"));
        // discover also works on the old schema.
        let refs = discover_conn(&conn, &PathBuf::from(":memory:")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "s1");
    }

    /// Branch-don't-fork: the adapter must discover sessions in BOTH the top-level `state.db` AND
    /// every `profiles/<name>/state.db`. Regression guard for the bug where `Hermes::new()` bound a
    /// single `<home>/state.db` and left the active profile's sessions (2782 on the real install)
    /// invisible. Synthesizes a minimal `~/.hermes`-shaped tree under a temp `HERMES_HOME`.
    #[test]
    fn discovers_top_level_and_profile_dbs() {
        // Mutating process env (HERMES_HOME) is not thread-safe; serialize against the other
        // env-touching tests in this binary.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let home = std::env::temp_dir().join(format!("cv-hermes-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("profiles/hermes-x")).unwrap();
        // A pre-update snapshot dir at a DEEPER level must NOT be picked up (depth-1 read_dir only).
        std::fs::create_dir_all(home.join("profiles/state-snapshots/old")).unwrap();

        // helper: create a state.db with one named session.
        let mk_db = |path: &std::path::Path, sid: &str| {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(SCHEMA_V14).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, source, model, started_at, message_count, title) \
                 VALUES (?1,'cli','nous/hermes-4',1000.0,1,?2)",
                rusqlite::params![sid, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1,'user','hi',1001.0)",
                [sid],
            )
            .unwrap();
        };
        mk_db(&home.join("state.db"), "top-session");
        mk_db(&home.join("profiles/hermes-x/state.db"), "profile-session");
        // A snapshot DB deeper than depth-1 — must stay hidden.
        mk_db(&home.join("profiles/state-snapshots/old/state.db"), "snapshot-session");

        // Point the adapter at our synthetic home and discover.
        std::env::set_var("HERMES_HOME", &home);
        let hermes = Hermes::new();
        // Two readable DBs: top-level + the one profile (snapshot is depth-2, skipped).
        assert_eq!(hermes.dbs.len(), 2, "should bind top-level + profile DB only");
        let refs = hermes.discover().unwrap();
        std::env::remove_var("HERMES_HOME");

        let ids: HashSet<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains("top-session"), "top-level session must be discovered");
        assert!(
            ids.contains("profile-session"),
            "profile session must be discovered (the bug)"
        );
        assert!(
            !ids.contains("snapshot-session"),
            "depth-2 snapshot DB must NOT be discovered"
        );

        // parse() must route by the ref's own DB path, so the profile session parses from its DB.
        let pref = refs.iter().find(|r| r.id == "profile-session").unwrap();
        let s = hermes.parse(pref).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Schema v30 (2026-09, `hermes_state_common.py:239`): the v14 tables plus the in-place
    /// compaction flags, display sidecars, `system_prompts`, cwd/git columns and listing flags.
    ///
    /// A *subset* of the real schema 30 — column ORDER and types match
    /// `tests/fixtures/hermes/state-v30.db` (`sqlite3 … '.schema sessions'`), but the gateway,
    /// billing, handoff and compression-cooldown columns the adapter never reads are omitted.
    /// Every column this adapter SELECTs or a test INSERTs must be here; add it in its real
    /// position when one is missing.
    const SCHEMA_V30: &str = "
        CREATE TABLE system_prompts (hash TEXT PRIMARY KEY, prompt TEXT NOT NULL);
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            display_name TEXT,
            -- Provenance of an imported session (`hermes sessions import` writes
            -- `origin_json.imported_from`); real schema 30 puts it right here, after
            -- `display_name` — verified against tests/fixtures/hermes/state-v30.db.
            origin_json TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            system_prompt_hash TEXT,
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
            cwd TEXT,
            git_branch TEXT,
            git_repo_root TEXT,
            title TEXT,
            title_source TEXT,
            last_activity_at REAL,
            profile_name TEXT,
            archived INTEGER NOT NULL DEFAULT 0,
            hidden INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            effect_disposition TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0,
            _compressed_summary INTEGER NOT NULL DEFAULT 0,
            active INTEGER NOT NULL DEFAULT 1,
            compacted INTEGER NOT NULL DEFAULT 0,
            api_content TEXT,
            display_kind TEXT,
            display_metadata TEXT,
            display_identity BLOB,
            display_order INTEGER
        );
    ";

    /// Insert a v30 message row with explicit flags; returns nothing (ids are AUTOINCREMENT in order).
    fn insert_v30(conn: &Connection, sid: &str, role: &str, content: &str, ts: f64, active: i64, compacted: i64) {
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, active, compacted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![sid, role, content, ts, active, compacted],
        )
        .unwrap();
    }

    fn texts(s: &Session) -> Vec<String> {
        s.messages.iter().filter_map(|m| m.text()).collect()
    }

    #[test]
    fn orders_by_id_not_timestamp() {
        // Hermes reads `ORDER BY id`: timestamps are not monotonic and would split a tool call from
        // its result (hermes_state_messages.py:42).
        let conn = mk_conn(SCHEMA_V30);
        insert_session(&conn, "s", None, None, 1000.0, None);
        insert_v30(&conn, "s", "user", "later stamp, first row", 2000.0, 1, 0);
        insert_v30(&conn, "s", "assistant", "earlier stamp, second row", 1500.0, 1, 0);
        let s = parse_conn(&conn, &sref("s")).unwrap();
        assert_eq!(texts(&s), vec!["later stamp, first row", "earlier stamp, second row"]);
    }

    #[test]
    fn in_place_compaction_display_view() {
        // archive_and_compact: summarized rows → active=0,compacted=1 (still history); the summary is
        // a fresh active row with _compressed_summary=1; the carried tail is cloned (same content +
        // timestamp, new id) and superseded originals / rewound rows get active=0,compacted=0.
        let conn = mk_conn(SCHEMA_V30);
        insert_session(&conn, "s", None, None, 1000.0, None);
        insert_v30(&conn, "s", "user", "q1", 1001.0, 0, 1); // id 1: summarized away
        insert_v30(&conn, "s", "assistant", "a1", 1002.0, 0, 1); // id 2
        insert_v30(&conn, "s", "user", "q2", 1003.0, 0, 1); // id 3: older generation of the carried tail
        insert_v30(&conn, "s", "user", "typo", 1003.5, 0, 0); // id 4: rewound — never displayed
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, active, compacted, _compressed_summary, display_kind) \
             VALUES ('s', 'user', '[CONTEXT COMPACTION] earlier turns…', 1004.0, 1, 0, 1, 'hidden')",
            [],
        )
        .unwrap(); // id 5: the summary
        insert_v30(&conn, "s", "user", "q2", 1003.0, 1, 0); // id 6: live clone of id 3
        insert_v30(&conn, "s", "assistant", "a2", 1005.0, 1, 0); // id 7

        // Lean: the display projection, generation-deduped, summary as boundary + summary.
        let s = parse_conn(&conn, &sref("s")).unwrap();
        assert_eq!(
            texts(&s),
            vec![
                "q1",
                "a1",
                "q2",
                "[conversation compacted]",
                "[CONTEXT COMPACTION] earlier turns…",
                "a2"
            ]
        );
        assert!(
            !texts(&s).contains(&"typo".to_string()),
            "rewound rows are not displayed"
        );
        assert_eq!(bag(&s.messages[0])["compacted"], Value::Bool(true));
        assert_eq!(s.messages[0].role, Role::User, "summarized-away history keeps its role");
        assert_eq!(s.messages[0].kind, MessageKind::Prompt);
        let boundary = &s.messages[3];
        assert_eq!(boundary.role, Role::System);
        assert_eq!(
            (boundary.kind, boundary.origin),
            (MessageKind::CompactionBoundary, Origin::Harness)
        );
        assert!(boundary.extra.is_empty(), "the boundary is its kind; no flat subtype");
        let summary = &s.messages[4];
        assert_eq!(summary.role, Role::System);
        assert_eq!(
            (summary.kind, summary.origin),
            (MessageKind::CompactionSummary, Origin::Harness)
        );
        assert_eq!(summary.parent_id, boundary.id, "summary links to its boundary");
        assert_eq!(bag(summary)["display_kind"], "hidden");
        assert!(!bag(summary).contains_key("isCompactSummary"), "the kind carries it");
        // the shared detector sees one compaction with its summary
        let found = crate::compaction::detect_in_session(&s, true);
        assert_eq!(found.len(), 1);
        assert!(found[0].summary.as_deref().is_some_and(|t| t.contains("earlier turns")));
        assert_nested_extra(&s);

        // Complete: every row, flags tagged, no dedup.
        let c = parse_conn_with(&conn, &sref("s"), &ParseOptions::complete()).unwrap();
        assert_eq!(c.messages.len(), 8, "7 rows + 1 boundary marker");
        let typo = c.messages.iter().find(|m| m.text().as_deref() == Some("typo")).unwrap();
        assert_eq!(bag(typo)["active"], Value::Bool(false));
        assert!(!bag(typo).contains_key("compacted"));
        assert_eq!(
            c.messages.iter().filter(|m| m.text().as_deref() == Some("q2")).count(),
            2
        );
    }

    #[test]
    fn system_prompt_resolves_via_system_prompts_table() {
        // v25 hollowed `sessions.system_prompt` out into `system_prompts(hash, prompt)`.
        let conn = mk_conn(SCHEMA_V30);
        conn.execute(
            "INSERT INTO system_prompts (hash, prompt) VALUES ('h1', 'You are Hermes')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, system_prompt, system_prompt_hash) VALUES ('s', 'cli', 1000.0, NULL, 'h1')",
            [],
        )
        .unwrap();
        insert_v30(&conn, "s", "user", "hi", 1001.0, 1, 0);
        let s = parse_conn(&conn, &sref("s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are Hermes"));
        assert!(
            !smeta(&s).as_object().unwrap().contains_key("system_prompt"),
            "the system prompt is first-class, not a session fact"
        );
        // the old direct column still wins on a v14 DB
        let old = mk_conn(SCHEMA_V14);
        old.execute(
            "INSERT INTO sessions (id, source, started_at, system_prompt) VALUES ('s', 'cli', 1000.0, 'legacy prompt')",
            [],
        )
        .unwrap();
        old.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s','user','hi',1001.0)",
            [],
        )
        .unwrap();
        let s = parse_conn(&old, &sref("s")).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("legacy prompt"));
    }

    #[test]
    fn branches_and_delegates_stand_alone_but_compression_still_merges() {
        let conn = mk_conn(SCHEMA_V30);
        // /branch: parent ended 'branched'; the child owns a COPY of the transcript.
        insert_session(&conn, "p", None, Some("branched"), 1000.0, Some(1500.0));
        insert_v30(&conn, "p", "user", "shared prompt", 1001.0, 1, 0);
        conn.execute(
            "INSERT INTO sessions (id, source, parent_session_id, model_config, started_at) \
             VALUES ('b', 'cli', 'p', '{\"_branched_from\":\"p\",\"yolo_mode\":false}', 1600.0)",
            [],
        )
        .unwrap();
        insert_v30(&conn, "b", "user", "shared prompt", 1001.0, 1, 0); // the copied row
        insert_v30(&conn, "b", "assistant", "branch answer", 1601.0, 1, 0);
        let b = parse_conn(&conn, &sref("b")).unwrap();
        assert_eq!(
            texts(&b),
            vec!["shared prompt", "branch answer"],
            "no parent merge, no duplicate"
        );
        assert_eq!(
            b.lineage,
            Lineage {
                forked_from: Some("p".into()),
                ..Default::default()
            }
        );
        assert!(
            !b.harness_extra(Harness::Hermes)
                .unwrap()
                .contains_key("parent_session_id"),
            "a marked parent is already in the lineage"
        );
        assert_eq!(b.model.as_deref(), None, "markers never leak into the model name");

        // delegate sub-agent run: separate conversation whose parent spawned it.
        conn.execute(
            "INSERT INTO sessions (id, source, parent_session_id, model_config, started_at) \
             VALUES ('d', 'cli', 'p', '{\"_delegate_from\":\"p\"}', 1700.0)",
            [],
        )
        .unwrap();
        insert_v30(&conn, "d", "user", "delegate task", 1701.0, 1, 0);
        let d = parse_conn(&conn, &sref("d")).unwrap();
        assert_eq!(texts(&d), vec!["delegate task"]);
        assert_eq!(
            d.lineage,
            Lineage {
                parent: Some("p".into()),
                ..Default::default()
            }
        );

        // reset child: a fresh conversation; the marker is a Hermes fact only.
        conn.execute(
            "INSERT INTO sessions (id, source, parent_session_id, model_config, started_at) \
             VALUES ('r', 'cli', 'p', '{\"_reset_from\":\"p\"}', 1800.0)",
            [],
        )
        .unwrap();
        insert_v30(&conn, "r", "user", "fresh", 1801.0, 1, 0);
        let r = parse_conn(&conn, &sref("r")).unwrap();
        assert!(r.lineage.is_empty(), "{:?}", r.lineage);
        assert_eq!(r.harness_extra(Harness::Hermes).unwrap()["_reset_from"], "p");

        // compression continuation: parent ended in compression → merged root→tip, and the store's
        // pointers survive both ways.
        insert_session(&conn, "c0", None, Some("compression"), 2000.0, Some(2500.0));
        insert_v30(&conn, "c0", "user", "long ago", 2001.0, 1, 0);
        insert_session(&conn, "c1", Some("c0"), None, 2600.0, None);
        insert_v30(&conn, "c1", "assistant", "continued", 2601.0, 1, 0);
        let c1 = parse_conn(&conn, &sref("c1")).unwrap();
        assert_eq!(texts(&c1), vec!["long ago", "continued"]);
        assert_eq!(c1.lineage.continues.as_deref(), Some("c0"));
        assert_eq!(c1.lineage.continued_in, None);
        let c0 = parse_conn(&conn, &sref("c0")).unwrap();
        assert_eq!(c0.lineage.continued_in.as_deref(), Some("c1"));
        assert_eq!(c0.lineage.continues, None);
        assert!(
            !c1.harness_extra(Harness::Hermes)
                .unwrap()
                .contains_key("parent_session_id"),
            "a compression parent is in the lineage, not the bag"
        );
    }

    #[test]
    fn discovery_mirrors_hermes_listing() {
        let conn = mk_conn(SCHEMA_V30);
        insert_session(&conn, "root", None, Some("compression"), 1000.0, Some(1500.0));
        insert_session(&conn, "tip", Some("root"), None, 1600.0, None);
        conn.execute("UPDATE sessions SET title = NULL WHERE id = 'tip'", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, archived) VALUES ('arch', 'cli', 1700.0, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, hidden) VALUES ('hid', 'cli', 1800.0, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, parent_session_id, model_config) \
             VALUES ('del', 'cli', 1900.0, 'tip', '{\"_delegate_from\":\"tip\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, cwd, last_activity_at, ended_at) \
             VALUES ('plain', 'cli', 2000.0, '/work/proj', 5000.0, 4000.0)",
            [],
        )
        .unwrap();
        let refs = discover_conn(&conn, &PathBuf::from(":memory:")).unwrap();
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["plain", "hid", "arch", "tip"],
            "tip lists (root is its compression ancestor) and so do archived/hidden (Hermes keeps \
             them resumable); the delegate run does not"
        );
        assert_eq!(
            refs.iter().find(|r| r.id == "tip").unwrap().title.as_deref(),
            Some("T"),
            "an untitled tip is titled by its compression root"
        );
        let plain = &refs[0];
        assert_eq!(plain.cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(plain.updated_at, secs_to_dt(5000.0), "last_activity_at beats ended_at");
        // a v14 DB is listed exactly as before (no listing columns → no filters)
        let old = mk_conn(SCHEMA_V14);
        insert_session(&old, "a", None, None, 1000.0, None);
        assert_eq!(discover_conn(&old, &PathBuf::from(":memory:")).unwrap().len(), 1);
    }

    #[test]
    fn session_cwd_git_and_activity_are_first_class() {
        let conn = mk_conn(SCHEMA_V30);
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, ended_at, last_activity_at, cwd, git_branch, git_repo_root, profile_name) \
             VALUES ('s', 'cli', 1000.0, 2000.0, 3000.0, '/work/proj', 'main', '/work/proj', 'hermes-x')",
            [],
        )
        .unwrap();
        insert_v30(&conn, "s", "user", "hi", 1001.0, 1, 0);
        let s = parse_conn(&conn, &sref("s")).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(s.git.as_ref().and_then(|g| g.branch.as_deref()), Some("main"));
        assert_eq!(s.updated_at, secs_to_dt(3000.0));
        assert_eq!(smeta(&s)["git_repo_root"], "/work/proj");
        assert_eq!(smeta(&s)["profile_name"], "hermes-x");
        assert_nested_extra(&s);
    }

    #[test]
    fn display_kind_notices_and_sidecars() {
        let conn = mk_conn(SCHEMA_V30);
        insert_session(&conn, "s", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, display_kind) \
             VALUES ('s', 'user', 'Model switched to x', 1001.0, 'model_switch')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, display_kind) \
             VALUES ('s', 'user', 'keep going but faster', 1002.0, 'steer')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, api_content, effect_disposition, display_metadata) \
             VALUES ('s', 'user', 'what the user typed', 1003.0, '[context] what the user typed', 'observed', '{\"k\":1}')",
            [],
        )
        .unwrap();
        for (kind, text) in [
            ("hidden", "[diagnostic] provider latency 900ms"),
            ("auto_continue", "continue"),
            ("process_complete", "job 7 finished"),
            ("internal_notification", "context 80% full"),
            ("async_delegation_complete", "delegate 7f3c finished"),
            ("personality_switch", "now terse"),
        ] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp, display_kind) VALUES ('s', 'user', ?1, 1004.0, ?2)",
                [text, kind],
            )
            .unwrap();
        }
        let s = parse_conn(&conn, &sref("s")).unwrap();
        let m = &s.messages[0];
        assert_eq!(m.role, Role::System, "harness-injected notice");
        assert_eq!((m.kind, m.origin), (MessageKind::ModelChange, Origin::Harness));
        assert_eq!(bag(m)["display_kind"], "model_switch");
        let steer = &s.messages[1];
        assert_eq!(steer.role, Role::User, "a human steer stays a user turn");
        assert_eq!((steer.kind, steer.origin), (MessageKind::Prompt, Origin::Human));
        assert_eq!(bag(steer)["display_kind"], "steer");
        let u = &s.messages[2];
        assert_eq!(u.role, Role::User);
        assert_eq!(
            u.text().as_deref(),
            Some("what the user typed"),
            "block text is the user's words"
        );
        assert_eq!(bag(u)["api_content"], "[context] what the user typed");
        assert_eq!(bag(u)["effect_disposition"], "observed");
        assert_eq!(bag(u)["display_metadata"]["k"], 1);
        // every documented harness display_kind, by kind
        let kinds: Vec<(Role, MessageKind, Origin)> =
            s.messages[3..].iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::SubagentReturn, Origin::Subagent),
                // undocumented: keeps its row role, carries the kind in the bag only
                (Role::User, MessageKind::Prompt, Origin::Human),
            ]
        );
        assert_eq!(bag(&s.messages[8])["display_kind"], "personality_switch");
        assert_eq!(
            s.first_user_text().as_deref(),
            Some("keep going but faster"),
            "notices never become the title"
        );
        assert_nested_extra(&s);
    }

    #[test]
    fn imported_session_messages_carry_import_origin() {
        // `hermes sessions import` records `origin_json.imported_from`; every message of such a
        // session came through the importer, whatever its role.
        let conn = mk_conn(SCHEMA_V30);
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, origin_json) VALUES ('f', 'codex-cli', 1000.0, \
             '{\"imported_from\": {\"tool\": \"codex-cli\", \"path\": \"/r.jsonl\", \"foreign_session_id\": \"x1\"}}')",
            [],
        )
        .unwrap();
        insert_v30(&conn, "f", "user", "hello", 1001.0, 1, 0);
        insert_v30(&conn, "f", "assistant", "hi", 1002.0, 1, 0);
        let f = parse_conn(&conn, &sref("f")).unwrap();
        assert_eq!(f.messages.len(), 2);
        assert!(
            f.messages.iter().all(|m| m.origin == Origin::Import),
            "{:?}",
            f.messages
        );
        assert_eq!(f.messages[0].kind, MessageKind::Prompt);
        assert_eq!(f.messages[1].kind, MessageKind::Reply);
        let hb = f.harness_extra(Harness::Hermes).unwrap();
        assert_eq!(hb["imported_from"]["tool"], "codex-cli");
        assert_eq!(hb["imported_from"]["foreign_session_id"], "x1");
        assert_eq!(hb[SESSION_META_KEY]["source"], "codex-cli");
        assert!(!f.extra.contains_key("imported_from"), "nested, never flat");
        assert_nested_extra(&f);
        assert_nested_extra(&f);
    }

    #[test]
    fn discover_orders_and_counts() {
        let conn = mk_conn(SCHEMA_V14);
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, message_count, title) VALUES ('a','cli',1000.0,3,'A')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, message_count, title) VALUES ('b','cli',2000.0,5,'B')",
            [],
        )
        .unwrap();
        let refs = discover_conn(&conn, &PathBuf::from(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        // ordered by started_at DESC
        assert_eq!(refs[0].id, "b");
        assert_eq!(refs[0].message_count, 5);
        assert_eq!(refs[1].id, "a");
    }

    /// `tests/fixtures/hermes/state-v30.db` was written by Hermes's OWN store code (hermes-agent
    /// `6d8a8bebf7`, `SCHEMA_VERSION = 30`) through `SessionDB` — `create_session`, `append_message`,
    /// `append_messages_batch`, `archive_and_compact` (in place, `tail_count=2`),
    /// `publish_compression_child` (rotation), `update_system_prompt`, `promote_to_session_reset`,
    /// `set_session_{title,archived,hidden}`, `append_delegation_delivery`, and
    /// `hermes_cli.foreign_sessions.import_foreign_session` — never raw SQL; only the derived FTS
    /// tables/triggers were dropped to keep it small (cv never reads them). Ten sessions: A compacted
    /// in place (+ steer / hidden / async_delegation_complete rows), R root with a `/branch` child B,
    /// a reset child S and a delegate child D, a rotation chain C1→C2, X archived, Y hidden, and F, a
    /// Claude Code import. Every assertion below was first checked against Hermes's own views
    /// (`list_sessions_rich`, `get_messages(include_compacted=True)`, `get_messages_as_conversation`).
    #[test]
    fn real_v30_store_written_by_hermes() {
        const A: &str = "20260919_161227_a657eb";
        const R: &str = "20260919_161227_3b223a";
        const B: &str = "20260919_161227_3584d9";
        const S: &str = "20260919_161227_804285";
        const D: &str = "20260919_161227_65066f";
        const C1: &str = "20260919_161227_f36517";
        const C2: &str = "20260919_161227_bbce2d";
        const X: &str = "20260919_161227_12996c";
        const Y: &str = "20260919_161227_f94c97";
        const F: &str = "20260919_161227_212153";
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes/state-v30.db");
        let conn = Hermes::open_path(&path).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 30);

        // Discovery: every conversation once — the same six Hermes's picker lists, plus the archived
        // and hidden ones it keeps resumable; not the delegate run, not the merged compression root.
        let refs = discover_conn(&conn, &path).unwrap();
        let ids: HashSet<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        for want in [A, R, B, S, C2, X, Y, F] {
            assert!(ids.contains(want), "{want} is listed");
        }
        assert!(
            !ids.contains(D),
            "a delegate sub-agent run is not a top-level conversation"
        );
        assert!(!ids.contains(C1), "a compression ancestor lists as its tip");
        let by_id = |id: &str| refs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id(A).title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(by_id(B).title.as_deref(), Some("Design the cache (branch)"));
        assert_eq!(
            by_id(C2).title.as_deref(),
            Some("Long migration"),
            "an untitled rotation tip is titled by its root, as Hermes lists it"
        );
        assert!(by_id(F)
            .title
            .as_deref()
            .unwrap()
            .starts_with("Imported from Claude Code"));
        assert_eq!(by_id(A).cwd, Some(PathBuf::from("/tmp/proj")));
        assert_eq!(
            by_id(A).message_count,
            8,
            "Hermes's message_count is the ACTIVE row count"
        );

        // A: the display view — each carried message once, in id order, the summary as a system turn.
        let a = parse_conn(&conn, &sref(A)).unwrap();
        let t = texts(&a);
        assert_eq!(a.messages.len(), 13, "{t:?}");
        // Index by message (the tool-result turn has no text, so `t` is one shorter).
        let txt = |i: usize| a.messages[i].text().unwrap_or_default();
        assert_eq!(txt(0), "fix the flaky test in tests/sched.py");
        assert_eq!(txt(1), "Let me look at the test.");
        assert_eq!(a.messages[2].role, Role::Tool);
        assert!(txt(5).starts_with("[CONTEXT COMPACTION"), "{}", txt(5));
        assert_eq!(
            t.iter().filter(|x| x.as_str() == "ok do it").count(),
            1,
            "the carried tail is not duplicated"
        );
        assert_eq!(txt(6), "ok do it");
        assert_eq!(txt(8), "now run the suite");
        // Every turn typed: prompt / reply / tool result / the compaction pair / steer / notices /
        // the delegation return.
        let kinds: Vec<(Role, MessageKind, Origin)> = a.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        use MessageKind as K;
        use Origin as O;
        use Role as R_;
        assert_eq!(
            kinds,
            vec![
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::Tool, K::ToolResult, O::Harness),
                (R_::Assistant, K::Reply, O::Model),
                (R_::System, K::CompactionBoundary, O::Harness),
                (R_::System, K::CompactionSummary, O::Harness),
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::User, K::Prompt, O::Human),
                (R_::Assistant, K::Reply, O::Model),
                (R_::User, K::Prompt, O::Human),     // steer
                (R_::System, K::Notice, O::Harness), // hidden diagnostic
                (R_::System, K::SubagentReturn, O::Subagent),
            ]
        );
        assert_eq!(
            a.messages[5].parent_id, a.messages[4].id,
            "summary links to its boundary"
        );
        assert_eq!(bag(&a.messages[5])["display_kind"], "hidden");
        assert_eq!(
            bag(&a.messages[0])["compacted"],
            true,
            "summarized-away history is flagged"
        );
        assert_eq!(bag(&a.messages[10])["display_kind"], "steer");
        assert_eq!(bag(&a.messages[12])["display_kind"], "async_delegation_complete");
        assert_eq!(a.model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
        assert!(
            a.messages.iter().all(|m| m.model.is_none()),
            "no per-message model to record"
        );
        assert_eq!(a.cwd, Some(PathBuf::from("/tmp/proj")));
        let sp = a.system_prompt.as_deref().unwrap();
        assert!(
            sp.ends_with("Context was compacted once."),
            "system prompt resolved via system_prompts: {sp}"
        );
        assert!(a.lineage.is_empty(), "{:?}", a.lineage);
        assert_nested_extra(&a);
        // complete: the superseded originals of the carried tail are present, tagged as rewind rows.
        let ac = parse_conn_with(&conn, &sref(A), &ParseOptions::complete()).unwrap();
        assert!(ac.messages.len() > a.messages.len());
        // (Flags are recorded as non-default values only: a superseded original carries
        // `active: false` and no `compacted` key — the same shape as a rewound row.)
        assert!(ac.messages.iter().any(|m| {
            m.harness_extra(Harness::Hermes)
                .is_some_and(|b| b.get("active") == Some(&Value::Bool(false)) && !b.contains_key("compacted"))
        }));
        assert_nested_extra(&ac);

        // C2: the rotation chain reads root→tip, titled by the root, the carried tail once; the
        // store's pointers survive both ways.
        let c2 = parse_conn(&conn, &sref(C2)).unwrap();
        assert_eq!(c2.title.as_deref(), Some("Long migration"));
        let t = texts(&c2);
        assert_eq!(t[0], "step 0: migrate table t0");
        assert_eq!(t.iter().filter(|x| x.as_str() == "migrated t3").count(), 1);
        assert!(t.iter().any(|x| x.starts_with("[CONTEXT COMPACTION")));
        assert_eq!(t.last().map(String::as_str), Some("migrated t4"));
        assert_eq!(
            c2.lineage,
            Lineage {
                continues: Some(C1.into()),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_conn(&conn, &sref(C1)).unwrap().lineage,
            Lineage {
                continued_in: Some(C2.into()),
                ..Default::default()
            }
        );
        assert_eq!(
            c2.system_prompt.as_deref(),
            Some("You are Hermes, a helpful coding agent. Working dir: /tmp/proj.")
        );
        assert_nested_extra(&c2);

        // Lineage: branch / reset / delegate children stand alone with their markers.
        let b = parse_conn(&conn, &sref(B)).unwrap();
        assert_eq!(
            b.messages.len(),
            4,
            "a branch owns its copied transcript; never merged with R"
        );
        assert_eq!(b.lineage.forked_from.as_deref(), Some(R));
        assert_eq!(b.lineage.parent, None);
        let s = parse_conn(&conn, &sref(S)).unwrap();
        assert!(s.lineage.is_empty(), "a reset has no IR lineage field: {:?}", s.lineage);
        assert_eq!(s.harness_extra(Harness::Hermes).unwrap()["_reset_from"], R);
        assert_eq!(s.messages.len(), 2);
        let d = parse_conn(&conn, &sref(D)).unwrap();
        assert_eq!(d.lineage.parent.as_deref(), Some(R));
        assert_eq!(d.lineage.forked_from, None);

        // Flags and provenance.
        assert_eq!(smeta(&parse_conn(&conn, &sref(X)).unwrap())["archived"], 1);
        assert_eq!(smeta(&parse_conn(&conn, &sref(Y)).unwrap())["hidden"], 1);
        let f = parse_conn(&conn, &sref(F)).unwrap();
        let fb = f.harness_extra(Harness::Hermes).unwrap();
        assert_eq!(fb["imported_from"]["tool"], "claude-code");
        assert_eq!(smeta(&f)["source"], "claude-code");
        assert_eq!(texts(&f)[0], "Reply with exactly: ONE");
        assert_eq!(f.messages.len(), 4);
        assert!(
            f.messages.iter().all(|m| m.origin == Origin::Import),
            "every message of an import came through the importer"
        );
        assert_eq!(f.messages[0].kind, MessageKind::Prompt);
        assert_eq!(f.messages[1].kind, MessageKind::Reply);
        assert_nested_extra(&f);
    }
}
