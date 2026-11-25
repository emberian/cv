//! Goose adapter (Block) — modern SQLite store + legacy `.jsonl` files. **sqlite feature only.**
//!
//! Goose keeps all of a user's sessions under one *data dir*:
//!   * **Linux:**   `~/.local/share/goose/sessions/`            (XDG; `$XDG_DATA_HOME/goose`)
//!   * **macOS:**   `~/Library/Application Support/Block.block.goose/sessions/`
//!   * **Windows:** `%APPDATA%\Block\Block\goose\data\sessions\`
//!   * `$GOOSE_PATH_ROOT/data/sessions/` overrides all of the above (test hook), and we also honour
//!     `$XDG_DATA_HOME` directly.
//!
//! The dir contains a modern `sessions.db` (SQLite, since the v1.10-era rewrite) and, for older
//! installs, legacy per-session `<name>.jsonl` files. New Goose imports the legacy files into the DB
//! on first run, but we read whatever is on disk: we parse the DB *and* any `.jsonl` not shadowed by
//! a same-id DB row.
//!
//! ## Modern schema (from `goose/src/session/session_manager.rs`, schema v16 at goose 1.51)
//! ```sql
//! CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TIMESTAMP);  -- MAX(version)
//! CREATE TABLE sessions (
//!   id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', description TEXT NOT NULL DEFAULT '',
//!   user_set_name BOOLEAN, session_type TEXT NOT NULL DEFAULT 'user', working_dir TEXT NOT NULL,
//!   created_at TIMESTAMP, updated_at TIMESTAMP, extension_data TEXT, total_tokens INTEGER,
//!   input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
//!   accumulated_* INTEGER, accumulated_cost REAL, schedule_id TEXT, recipe_json TEXT,
//!   user_recipe_values_json TEXT, provider_name TEXT, model_config_json TEXT, goose_mode TEXT,
//!   archived_at TIMESTAMP, project_id TEXT, parent_session_id TEXT );
//! CREATE TABLE messages (
//!   id INTEGER PK, message_id TEXT, session_id TEXT, role TEXT, content_json TEXT NOT NULL,
//!   created_timestamp INTEGER NOT NULL, timestamp TIMESTAMP, tokens INTEGER, metadata_json TEXT );
//! CREATE TABLE usage_ledger ( id, session_id, created_timestamp, model, input_tokens, output_tokens,
//!   total_tokens, cache_read_tokens, cache_write_tokens, cost REAL, cost_source, is_compaction );
//! ```
//! `created_at`/`updated_at` are stored as SQLite `TIMESTAMP` text (`YYYY-MM-DD HH:MM:SS`, UTC).
//! `messages.created_timestamp` is unix **seconds** (Goose tolerates milliseconds on read:
//! `MILLISECOND_TIMESTAMP_THRESHOLD = 10_000_000_000`, session_manager.rs:30,728 — so do we).
//!
//! Ground truth on disk: `tests/fixtures/goose/sessions-v16-1.51.0.db` was written by a goose 1.51.0
//! built from `2090ad1c` (`GOOSE_PATH_ROOT` isolated home): two `goose session import`s (a Claude
//! Code transcript, a Codex rollout), one turn against an OpenAI-compatible stub, one against a dead
//! endpoint, and a recipe whose `retry` check always fails. Facts that only the real writer settled:
//! every prompt is followed by a `userVisible: false, turnContext: true` user row holding the
//! `<turn-context>` block; the assistant row carries `inference{provider, requestedModel}` and
//! `usage{…, cacheReadTokens}`; `usage_ledger` mirrors it; a provider network error is NOT persisted
//! at all; and the retry-exhaustion message lands as a plain `text` block (not `error`) — so the
//! `error` arm below covers other producers (`from_provider_error`, ACP), not that path.
//! Two columns are **never written** at any revision: `sessions.description` (only `name` is set by
//! `create_session`/updates; Goose itself reads `name` and falls back to `description`,
//! session_manager.rs:826-833) and `messages.tokens` (every `INSERT INTO messages` lists
//! `message_id, session_id, role, content_json, created_timestamp, metadata_json`, :1942). Per-message
//! usage/model live in `metadata_json` (`MessageMetadata`, goose-provider-types
//! `conversation/message.rs:828-853`): `{userVisible, agentVisible, inference{provider,
//! requestedModel, resolvedModel, providerSessionId}, outputTokenLimitReached, steer, turnContext,
//! usage{inputTokens, outputTokens, totalTokens, cacheReadTokens, cacheWriteTokens, cost, costSource,
//! elapsedMs, timeToFirstTokenMs, isCompaction}, operations}`. Goose hides rows whose
//! `userVisible` is false from its UI; we keep them (they were in the model's context) and carry the
//! flag. Schema columns have accreted over versions, so we probe `PRAGMA table_info` and only SELECT
//! columns that exist (older DBs lack `name`, `provider_name`, `model_config_json`, `message_id`,
//! `metadata_json`, …).
//!
//! ## content_json (one row's content = a JSON array of `MessageContentBlock`, tagged `type`, camelCase;
//! goose-provider-types `conversation/message.rs:313-329`)
//! ```jsonc
//! [{"type":"text","text":"…"},
//!  {"type":"image","data":"…","mimeType":"…"},
//!  {"type":"document","data":"<base64>","mimeType":"application/pdf","name":"q3-report.pdf"},
//!  {"type":"thinking","thinking":"…","signature":"…"},
//!  {"type":"redactedThinking","data":"…"},
//!  {"type":"error","kind":"authentication|contextLengthExceeded|creditsExhausted|other","message":"…"},
//!  {"type":"toolRequest","id":"…",
//!     "toolCall":{"status":"success","value":{"name":"…","arguments":{…}}}},
//!  {"type":"toolResponse","id":"…",
//!     "toolResult":{"status":"success","value":{"content":[{"type":"text","text":"…"}],"isError":false}}}]
//! ```
//! `toolResult.value` may also be a **bare content array** (the legacy `SuccessWithContentVec` shape
//! Goose still accepts, `tool_result_serde.rs:132-146`), and result content blocks may be rmcp 3.x
//! `text|image|audio|resource|resource_link`. `toolConfirmationRequest`, `actionRequired` and
//! `systemNotification` are UI/control content: rendered as text the way Goose's own `Display`
//! prints them, kept verbatim in `extra["goose"]["control_blocks"]`, and a message that is nothing
//! but control content is a [`MessageKind::Notice`]. `frontendToolRequest` (removed 2026-08) is
//! still read from old rows.
//!
//! ## Kinds (`docs/INTERFACE-V2.md` §4)
//! A `userVisible: false` row is text the harness fed the model and never showed the person — the
//! per-turn `<turn-context>` block (`turnContext: true`), the agent-only rows compaction leaves
//! behind — so it is a System turn: [`MessageKind::CompactionSummary`] when it is the agent-only
//! summary compaction appends after flipping every earlier row to `agentVisible: false`
//! (`context_mgmt/mod.rs` `compact_messages`; Goose writes `isCompaction` only to `usage_ledger`,
//! so a `usage.isCompaction` in `metadata_json` is honoured but real stores never have it), else
//! [`MessageKind::InjectedContext`]. A row holding an `error` block is a [`MessageKind::Error`]
//! (`extra["goose"]["error"]` = `{kind, message}`). The origin of a prompt follows the session's
//! `session_type`: `scheduled` → [`Origin::Scheduler`], `sub_agent` → [`Origin::Subagent`],
//! `hidden` (Goose's own helper runs) → [`Origin::Harness`], everything else a person. Goose 1.51
//! stores NO import provenance (`goose session import` forces `session_type = user`,
//! `import_formats/mod.rs:46`), so [`Origin::Import`] is never derivable from the store today.
//! `parent_session_id` → `Session::lineage.parent`; `metadata_json.usage.cost` → `Usage::cost_usd`.
//! A fork stores no link: the messages it copied (same `message_id`) name its source, which is
//! `Session::lineage.forked_from` ([`fork_source`]).
//! Goose uses MCP-style tools: a `toolRequest` is the assistant's call (rmcp `CallToolRequestParams`
//! = `{name, arguments}`); a `toolResponse` is the result (rmcp `CallToolResult` =
//! `{content:[Content…], isError}`). On error the inner wrapper is `{"status":"error","error":"…"}`.
//! Tool requests/responses ride *inside* an assistant/user message's content array (Goose only has
//! user/assistant roles), so a tool result lives on a `user` message — we re-classify a message that
//! is purely tool responses to [`Role::Tool`] for IR fidelity.
//!
//! ## Legacy `.jsonl`
//! First line is a metadata header (`{description, working_dir, created_at, updated_at, …}`); every
//! subsequent line is one message `{id, role, created, content:[…]}` using the same `MessageContent`
//! shape as `content_json`.
//!
//! ## Fidelity caveats
//! * Goose records only user/assistant roles in the DB; there is no system message and tool turns are
//!   folded into user/assistant content (we recover [`Role::Tool`] heuristically). The MCP tool name
//!   lives on the *request*, not the *response*, so a `toolResponse` only carries `tool_name` when we
//!   can pair it to a prior request by id (we do this within a session).
//! * `model` is `model_config_json.model_name`; `provider_name` stays in the Goose bag.

use super::store::sqlite::{open_ro, Cells, Select};
use super::wire::anthropic;
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Demand, Flow, MessageSink, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod legacy;
mod record;

pub struct Goose {
    /// The `sessions/` dir, if it exists on disk.
    dir: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Goose = Goose { dir: None };

impl Goose {
    pub fn detect() -> Self {
        Goose { dir: sessions_dir() }
    }

    /// Rooted at an explicit `sessions/` dir (the one holding `sessions.db`), ignoring the process
    /// env. The corpus test reads each committed Goose version this way.
    pub fn with_sessions_dir(dir: &Path) -> Self {
        Goose {
            dir: Some(dir.to_path_buf()).filter(|d| d.exists()),
        }
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("sessions.db")).filter(|p| p.exists())
    }
}

/// Resolve Goose's `sessions/` directory across platforms / env overrides. Returns it only if it
/// exists (so discovery degrades to empty rather than erroring on machines without Goose).
fn sessions_dir() -> Option<PathBuf> {
    // 1. Explicit test/CI override used by Goose itself: `$GOOSE_PATH_ROOT/data`.
    if let Some(root) = crate::env::var_os("GOOSE_PATH_ROOT") {
        let p = PathBuf::from(root).join("data").join("sessions");
        if p.exists() {
            return Some(p);
        }
    }
    let home = crate::env::home_dir();
    // 2. XDG (honour an explicit $XDG_DATA_HOME even on macOS for portability).
    if let Some(xdg) = crate::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg).join("goose").join("sessions");
        if p.exists() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        // macOS Apple-strategy (etcetera bundle_id = "Block.block.goose").
        candidates.push(h.join("Library/Application Support/Block.block.goose/sessions"));
        // Linux XDG default.
        candidates.push(h.join(".local/share/goose/sessions"));
    }
    // Windows: `%APPDATA%\Block\goose\data\sessions` (etcetera 0.11's Windows strategy yields one
    // vendor segment); older etcetera doubled it (`Block\Block\goose`). Probe both.
    if let Some(appdata) = crate::env::var_os("APPDATA") {
        let appdata = PathBuf::from(appdata);
        candidates.push(appdata.join("Block").join("goose").join("data").join("sessions"));
        candidates.push(
            appdata
                .join("Block")
                .join("Block")
                .join("goose")
                .join("data")
                .join("sessions"),
        );
    }
    candidates.into_iter().find(|p| p.exists())
}

impl Adapter for Goose {
    fn harness(&self) -> Harness {
        Harness::Goose
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.dir.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(dir) = &self.dir else {
            return Ok(Default::default());
        };
        let mut out = crate::Discovered::default();
        let mut seen_ids = std::collections::HashSet::new();

        // Modern SQLite store: one that cannot open or read is skipped and reported.
        if let Some(db) = self.db_path() {
            let refs =
                super::store::sqlite::cached_discover(&db, || open_ro(&db)?.read(|reader, _| discover_db(reader, &db)));
            for r in out.keep(&db, refs).into_iter().flatten() {
                seen_ids.insert(r.id.clone());
                out.refs.push(r);
            }
        }

        // Legacy `.jsonl` files not already represented by a DB row of the same id.
        let legacy = out.keep(dir, list_legacy(dir)).unwrap_or_default();
        for (name, path) in legacy {
            if seen_ids.contains(&name) {
                continue;
            }
            // A legacy file is read whole (its UTF-8 inclusion contract); the ref depends on its
            // bytes and name alone, so it is cached under the file's identity.
            let found = crate::discover_cache::cached_scan_checked(&path, || legacy::discover(&name, &path).map(Some));
            if let Some(found) = crate::Skipped::listed(&path, found) {
                out.push(found);
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Goose, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            // A legacy session's `path` is the `.jsonl`; a DB session's `path` is `sessions.db`.
            if r.path.extension().is_some_and(|e| e == "jsonl") {
                return legacy::stream(&r.id, &r.path, opts, sink);
            }
            super::store::sqlite::read(&r.path, |conn, reads| {
                stream_db_opts_with_reads(conn, r, opts, sink, reads)
            })
        })
    }

    /// Every session's certificate ([`rows_conn`]); a legacy `.jsonl` session is its own file.
    fn rows(&self, store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
        open_ro(store).ok()?.read(|conn, _| rows_conn(conn)).ok()
    }

    /// The todo extension's state: `sessions.extension_data["todo.v0"].content` (a markdown list).
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        if r.path.extension().is_some_and(|e| e == "jsonl") {
            return Ok(Vec::new());
        }
        let conn = open_ro(&r.path)?;
        let data: Option<String> = conn.read(|reader, _| {
            Ok(
                reader.query_row("SELECT extension_data FROM sessions WHERE id = ?1", [&r.id], |row| {
                    row.get(0)
                })?,
            )
        })?;
        let Some(todo) = data
            .map(|d| serde_json::from_str::<Value>(&d))
            .transpose()?
            .and_then(|v| v.get("todo.v0").cloned())
        else {
            return Ok(Vec::new());
        };
        let items = crate::affordance::items_of(&todo);
        Ok(
            crate::affordance::plan_state(crate::affordance::Kind::Plan, "extension_data[todo.v0]", items)
                .into_iter()
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Modern SQLite store
// ---------------------------------------------------------------------------

/// The SQL expression for a session's title, given which columns exist. Goose only ever WRITES
/// `name` (`create_session`, the name updater — nothing touches `description`) and reads `name`
/// first, falling back to `description` (session_manager.rs:826-833). Preferring `description`
/// used to yield `None` for every modern session.
fn title_expr(has: impl Fn(&str) -> bool) -> &'static str {
    match (has("name"), has("description")) {
        (true, true) => "COALESCE(NULLIF(name, ''), NULLIF(description, ''))",
        (true, false) => "NULLIF(name, '')",
        (false, true) => "NULLIF(description, '')",
        _ => "NULL",
    }
}

fn discover_db(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let probe = Select::of(conn, "sessions");
    let title = title_expr(|c| probe.has(c));
    // Order by updated_at when present; the column always existed in the modern schema.
    let order = if probe.has("updated_at") {
        "ORDER BY updated_at DESC"
    } else {
        ""
    };
    let select = probe
        .always(&["id", title])
        .probed(&["working_dir", "created_at", "updated_at"]);
    let sql = format!("SELECT {} FROM sessions {order}", select.list());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: Option<String> = row.get(1)?;
        let wd: Option<String> = row.get(2)?;
        let created: Option<String> = row.get(3)?;
        let updated: Option<String> = row.get(4)?;
        Ok((id, title, wd, created, updated))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, wd, created, updated) = row?;
        out.push(SessionRef {
            cwd: wd.filter(|s| !s.is_empty()).map(PathBuf::from),
            title: title
                .filter(|s| !s.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE)),
            created_at: created.as_deref().and_then(parse_ts_text),
            updated_at: updated.as_deref().and_then(parse_ts_text),
            message_count: None,
            ..SessionRef::new(id, Harness::Goose, db.to_path_buf())
        });
    }
    Ok(out)
}

/// Every session's [`Rows`](crate::source::Rows) certificate: what a read of it consults besides
/// message bodies. Its whole `sessions` row (title, cwd, model, `extension_data`, lineage); per
/// message row, in rowid order, its rowid, `message_id`, role, `created_timestamp` (the read's
/// order) and the byte lengths of `content_json` and `metadata_json`; and the session it was
/// forked from ([`fork_source`]), which the earliest other holder of its first message decides.
/// Goose rewrites rows in place (a compaction's `metadata_json`, a tool request's `tool_meta`
/// merged into `content_json`, a moved `created_timestamp`) without touching the `sessions` row:
/// the lengths and timestamps catch those, an equal-length rewrite of a body escapes.
fn rows_conn(conn: &Connection) -> Result<HashMap<String, crate::source::Rows>> {
    let mut certificates = super::store::sqlite::Certificates::default();
    certificates.digest(conn, "SELECT id, * FROM sessions ORDER BY id")?;
    let has = super::store::sqlite::columns(conn, "messages");
    let column = |name: &str, expr: &str| {
        if has.contains(name) {
            expr.to_owned()
        } else {
            "NULL".to_owned()
        }
    };
    certificates.rows(
        conn,
        &format!(
            "SELECT session_id, id, {}, role, created_timestamp, octet_length(content_json), {} \
             FROM messages ORDER BY session_id, id",
            column("message_id", "message_id"),
            column("metadata_json", "octet_length(metadata_json)"),
        ),
    )?;
    let created: Vec<(String, Option<DateTime<Utc>>)> = {
        let has = super::store::sqlite::columns(conn, "sessions");
        let created = if has.contains("created_at") {
            "created_at"
        } else {
            "NULL"
        };
        let mut stmt = conn.prepare(&format!("SELECT id, {created} FROM sessions"))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        rows.map(|row| row.map(|(id, at)| (id, at.as_deref().and_then(parse_ts_text))))
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, created) in created {
        let parent = fork_source(conn, &id, created);
        certificates.fold(&id, parent.as_deref().unwrap_or("").as_bytes());
    }
    Ok(certificates.0)
}

/// The session `id` was forked from. `goose session --resume --fork` copies every message, with
/// its `message_id` and `created_timestamp`, into a new session and stores no link; so a session
/// whose first message predates the session itself is a copy, and its source is the
/// earliest-created other session holding that first message (a fork of a fork resolves to the
/// original). `goose session import` also yields an older first message, but no other session
/// holds its ids. Two indexed lookups, and only for a copied session.
fn copied_message_id(conn: &Connection, id: &str, created_at: Option<DateTime<Utc>>) -> Option<String> {
    let created_at = created_at?;
    if !Select::of(conn, "messages").has("message_id") {
        return None;
    }
    let (first, at): (String, i64) = conn
        .query_row(
            "SELECT message_id, created_timestamp FROM messages WHERE session_id = ?1 \
             AND message_id IS NOT NULL ORDER BY created_timestamp, id LIMIT 1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    if ts_from_epoch(at)?.timestamp() >= created_at.timestamp() {
        return None;
    }
    Some(first)
}

fn earliest_holder(conn: &Connection, first: &str, excluding: Option<&str>) -> Option<(DateTime<Utc>, String)> {
    let sql = if excluding.is_some() {
        "SELECT s.id,s.created_at FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.message_id=?1 AND m.session_id!=?2"
    } else {
        "SELECT s.id,s.created_at FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.message_id=?1"
    };
    let mut stmt = conn.prepare(sql).ok()?;
    let decode = |row: &rusqlite::Row<'_>| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?));
    let holders = if let Some(id) = excluding {
        stmt.query_map([first, id], decode)
    } else {
        stmt.query_map([first], decode)
    }
    .ok()?
    .flatten();
    holders
        .filter_map(|(sid, created)| Some((created.as_deref().and_then(parse_ts_text)?, sid)))
        .min()
}

// Sharing a minimum that includes the current holder is equivalent to SQL's exclusion only
// under native TEXT/BINARY ID comparisons. Custom affinities/collations keep the old query.
fn binary_holder_ids(conn: &Connection) -> bool {
    [("sessions", "id"), ("messages", "session_id")]
        .into_iter()
        .all(|(table, id)| super::store::sqlite::binary_text_columns(conn, table, &[id]).unwrap_or(false))
}

fn fork_source(conn: &Connection, id: &str, created: Option<DateTime<Utc>>) -> Option<String> {
    let first = copied_message_id(conn, id, created)?;
    let (time, parent) = earliest_holder(conn, &first, Some(id))?;
    (time < created? && parent != id).then_some(parent)
}

fn fork_source_scoped(
    conn: &Connection,
    r: &SessionRef,
    created: Option<DateTime<Utc>>,
    reads: &crate::source::ReadSet,
) -> Result<Option<String>> {
    let Some(created) = created else {
        return Ok(None);
    };
    let Some(first) = copied_message_id(conn, &r.id, Some(created)) else {
        return Ok(None);
    };
    let binary = if r.path.is_file() {
        *crate::source::inventory("goose-fork-id-contract", &r.path, reads, || {
            Ok((binary_holder_ids(conn), reads.clone(), std::mem::size_of::<bool>()))
        })?
        .0
    } else {
        binary_holder_ids(conn)
    };
    if !binary {
        return Ok(
            earliest_holder(conn, &first, Some(&r.id)).and_then(|(time, parent)| (time < created).then_some(parent))
        );
    }
    let holder = if r.path.is_file() {
        // Synthetic memo selector: only actual DB/WAL paths in reads are freshness authorities.
        let mut selector = String::with_capacity(first.len() * 2);
        use std::fmt::Write as _;
        for byte in first.as_bytes() {
            write!(&mut selector, "{byte:02x}")?;
        }
        let key = r.path.join(selector);
        let (holder, _) = crate::source::inventory("goose-fork-holder", &key, reads, || {
            let holder = earliest_holder(conn, &first, None);
            let retained = std::mem::size_of_val(&holder) + holder.as_ref().map_or(0, |(_, id)| id.capacity());
            Ok((holder, reads.clone(), retained))
        })?;
        holder
    } else {
        std::sync::Arc::new(earliest_holder(conn, &first, None))
    };
    Ok(holder
        .as_ref()
        .as_ref()
        .and_then(|(time, parent)| (*time < created && parent != &r.id).then(|| parent.clone())))
}

#[cfg(test)]
fn stream_db_opts(conn: &Connection, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    stream_db_opts_with_reads(conn, r, opts, sink, &crate::source::ReadSet::default())
}

fn stream_db_opts_with_reads(
    conn: &Connection,
    r: &SessionRef,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    captured: &crate::source::ReadSet,
) -> Result<Session> {
    let mut reads = crate::source::ReadSet::default();
    reads.merge(captured);
    reads.record(&r.path);
    let mut wal = r.path.as_os_str().to_os_string();
    wal.push("-wal");
    reads.record(Path::new(&wal));
    let cells = Cells::new(conn, &r.path, &reads, "messages")?;
    let probe = Select::of(conn, "sessions");
    let title = title_expr(|c| probe.has(c));
    let select = probe.always(&[title]).probed(&[
        "working_dir",
        "created_at",
        "updated_at",
        "provider_name",
        "model_config_json",
        "session_type",
        "parent_session_id",
    ]);
    let sel = format!("SELECT {} FROM sessions WHERE id = ?1", select.list());
    /// One optional TEXT value per metadata column the query selects.
    type MetaRow = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let (title, wd, created, updated, provider, model_cfg, session_type, parent_id): MetaRow = conn
        .query_row(&sel, [&r.id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .with_context(|| format!("goose session {:?}", r.id))?;

    let model = recorded_model(model_cfg.as_deref());
    // The session's kind (`user`/`scheduled`/`sub_agent`/`hidden`/…, v13+) is a Goose fact and
    // decides where its prompts came from; the spawning session of a sub-agent (`parent_session_id`,
    // v15+) is first-class lineage.
    let session_type = session_type.filter(|t| !t.is_empty());
    let prompt_origin = prompt_origin(session_type.as_deref());
    let mut extra = serde_json::Map::new();
    let mut bag = serde_json::Map::new();
    if let Some(t) = session_type {
        bag.insert("session_type".into(), Value::String(t));
    }
    if let Some(provider) = provider.filter(|p| !p.is_empty()) {
        bag.insert("provider_name".into(), Value::String(provider));
    }
    if !bag.is_empty() {
        extra.insert(Harness::Goose.as_str().into(), Value::Object(bag));
    }
    let created_at = created.as_deref().and_then(parse_ts_text).or(r.created_at);
    let lineage = Lineage {
        parent: parent_id.filter(|p| !p.is_empty()),
        forked_from: fork_source_scoped(conn, r, created_at, &reads)?,
        ..Default::default()
    };

    let mut s = Session {
        cwd: wd
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title: title.filter(|t| !t.trim().is_empty()).or_else(|| r.title.clone()),
        created_at,
        updated_at: updated.as_deref().and_then(parse_ts_text).or(r.updated_at),
        model,
        source_path: Some(r.path.clone()),
        extra,
        lineage,
        ..Session::new(r.id.clone(), Harness::Goose)
    };
    // All session metadata is known up front, so hand it to the sink before the body.
    sink.meta(&s);

    let select = Select::of(conn, "messages")
        .always(&["role"])
        .cell(&cells, "content_json")
        .always(&["created_timestamp"])
        .probed(&["message_id"])
        .cell(&cells, "metadata_json");
    let (role_at, content_at, created_at) = (
        select.index("role")?,
        select.index("content_json")?,
        select.index("created_timestamp")?,
    );
    let (id_at, metadata_at) = (select.index("message_id")?, select.index("metadata_json")?);
    let sql = format!(
        "SELECT {} FROM messages WHERE session_id = ?1 ORDER BY created_timestamp, id",
        select.list()
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&r.id])?;
    let rich = opts.tier >= Tier::Rich;

    // Track tool-request names so we can label later tool responses (forward-only state).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut history = AgentHistory::default();
    // A row whose content cell is not one JSON value, or whose role is not text, is counted (it
    // still reads as it did: empty content, an empty role).
    let mut unparsed = super::Unparsed::default();
    while let Some(row) = rows.next()? {
        let metadata = match cells.cell(conn, "messages", "metadata_json", row, metadata_at)? {
            Some(cell) => cells.read(cell, |v| record::metadata(v, rich))?,
            None => None,
        };
        let decoded = match cells.cell(conn, "messages", "content_json", row, content_at)? {
            Some(cell) => cells
                .read(cell, |v| record::content(v, &mut tool_names, opts, Some(cells.out())))?
                .unwrap_or_else(|| {
                    unparsed.count();
                    Decoded::default()
                }),
            None => Decoded::default(),
        };
        let role = row.get::<_, String>(role_at).unwrap_or_else(|_| {
            unparsed.count();
            String::new()
        });
        let row = DbMsg {
            role,
            created: row.get::<_, Option<i64>>(created_at).ok().flatten(),
            id: row.get::<_, Option<String>>(id_at).ok().flatten(),
            metadata,
        };
        let Some(mut m) = row.into_message_decoded(decoded, &mut history, opts.tier >= Tier::Rich) else {
            continue;
        };
        if m.kind == MessageKind::Prompt {
            m.origin = prompt_origin;
        }
        // A compaction summary: Goose keeps no record of the compaction itself, so cv synthesizes
        // the boundary before it (no content, no id); `crate::compaction` pairs the two by
        // adjacency, as it does for every harness without a parent link.
        if m.kind == MessageKind::CompactionSummary {
            let mut boundary = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
            boundary.timestamp = m.timestamp;
            if sink.message(boundary) == Flow::Stop {
                break;
            }
        }
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    if opts.spans {
        cells.out().0.check()?;
    }
    unparsed.note(&mut s);
    Ok(s)
}

/// Where a session's prompts came from, by its `session_type` (`SessionType`,
/// session_manager.rs:47-56): a scheduler fires a `scheduled` session's recipe prompt, a parent
/// agent instructs a `sub_agent`, Goose itself composes a `hidden` helper run's prompt
/// (`tool_call_labels.rs`, `prompt_manager.rs`); `user` / `terminal` / `gateway` / `acp` are a person.
fn prompt_origin(session_type: Option<&str>) -> Origin {
    match session_type {
        Some("scheduled") => Origin::Scheduler,
        Some("sub_agent") => Origin::Subagent,
        Some("hidden") => Origin::Harness,
        _ => Origin::Human,
    }
}

struct DbMsg {
    role: String,
    created: Option<i64>,
    id: Option<String>,
    /// `messages.metadata_json` — the only place Goose records per-message usage and model.
    metadata: Option<Value>,
}

/// What the rows before this one left in the model's context, in store order: how many there were
/// and whether any is still `agentVisible`. Goose's compaction (`context_mgmt::compact_messages`)
/// flips EVERY earlier row to `agentVisible: false` and appends an agent-only summary, so a summary
/// is an agent-only user row with nothing agent-visible before it. Its `metadata_json` carries no
/// usage at all: `isCompaction` is set only on the `usage_ledger` row (verified on real 1.51.0 and
/// 1.54.0 stores). A tool-pair summary (`maybe_summarize_tool_pairs`) is also an agent-only user
/// row, but it hides only its own pair, so the rows before it are still visible.
#[derive(Default)]
struct AgentHistory {
    rows: usize,
    any_agent_visible: bool,
}

impl DbMsg {
    fn into_message_decoded(self, decoded: Decoded, history: &mut AgentHistory, rich: bool) -> Option<Message> {
        let meta = self.metadata;
        let flag = |k: &str| meta.as_ref().and_then(|m| m.get(k)).and_then(Value::as_bool);
        let agent_visible = flag("agentVisible") != Some(false);
        let compaction_summary = self.role == "user"
            && flag("userVisible") == Some(false)
            && agent_visible
            && flag("turnContext") != Some(true)
            && history.rows > 0
            && !history.any_agent_visible;
        history.rows += 1;
        history.any_agent_visible |= agent_visible;

        let mut m = build_message(&self.role, self.id, self.created.and_then(ts_from_epoch), decoded)?;
        if let Some(meta) = meta {
            apply_metadata(&mut m, meta, compaction_summary);
        }
        if !rich {
            omit_rich_details(&mut m);
        }
        Some(m)
    }
}

/// Structured result details are part of full extra demand, like the native harness bag.
fn omit_rich_details(message: &mut Message) {
    message.extra.remove(Harness::Goose.as_str());
    for block in &mut message.content {
        if let Block::ToolResult { details, .. } = block {
            *details = None;
        }
    }
}

/// Fold a row's `metadata_json` (`MessageMetadata`, message.rs:828-853) into the message: `usage.*`
/// → [`Usage`] (real per-response counts and the provider-reported `cost`; the `tokens` column is
/// never written), the inference's recorded model → `model`,
/// and the whole object under `extra["goose"]["metadata"]` so `userVisible` / `agentVisible` /
/// `isCompaction` / latency survive for consumers that care.
///
/// Visibility types the turn the way Goose itself treats it: a row with `userVisible: false` is text
/// the harness fed the model — the per-turn `<turn-context>` block (`turnContext: true`, written on
/// every prompt by 1.51's agent), the agent-only summary + continuation compaction leaves behind —
/// and Goose never shows it to the user (`session_manager.rs:742` hides `userVisible = 0`). It
/// becomes a [`Role::System`] turn of kind [`MessageKind::CompactionSummary`] (when
/// `usage.isCompaction`, or `compaction_summary`: see [`AgentHistory`]) or
/// [`MessageKind::InjectedContext`], so a user-text count or a `cv show`
/// reads as the user saw it while the model-visible context is still there. Verified on a real
/// 1.51.0 store.
fn apply_metadata(m: &mut Message, meta: Value, compaction_summary: bool) {
    let hidden = meta.get("userVisible") == Some(&Value::Bool(false));
    let compaction = compaction_summary || meta.pointer("/usage/isCompaction") == Some(&Value::Bool(true));
    if hidden && matches!(m.role, Role::User | Role::Assistant) {
        m.role = Role::System;
        m.origin = Origin::Harness;
        if compaction {
            m.kind = MessageKind::CompactionSummary;
        } else if matches!(m.kind, MessageKind::Prompt | MessageKind::Reply) {
            m.kind = MessageKind::InjectedContext;
        }
    }
    if let Some(u) = meta.get("usage") {
        let get = |k: &str| u.get(k).and_then(Value::as_u64);
        let usage = Usage {
            input_tokens: get("inputTokens"),
            output_tokens: get("outputTokens"),
            cache_read_tokens: get("cacheReadTokens"),
            cache_creation_tokens: get("cacheWriteTokens"),
            cost_usd: u.get("cost").and_then(Value::as_f64),
            ..Usage::default()
        };
        if usage.input_tokens.is_some() || usage.output_tokens.is_some() || usage.cost_usd.is_some() {
            m.usage = Some(usage);
        }
    }
    if let Some(inf) = meta.get("inference") {
        // The resolved snapshot the row records, else the requested model. Its provider stays
        // in the inference metadata; it cannot stand in for an absent model.
        let model = inf
            .get("resolvedModel")
            .or_else(|| inf.get("requestedModel"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        m.model = model.map(str::to_owned);
    }
    if meta.is_object() {
        m.harness_extra_mut(Harness::Goose).insert("metadata".into(), meta);
    }
}

/// The model's recorded name; a provider alone supplies no model fact.
fn recorded_model(model_cfg: Option<&str>) -> Option<String> {
    model_cfg
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| {
            v.get("model_name")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
        })
}

// ---------------------------------------------------------------------------
// Legacy `.jsonl`
// ---------------------------------------------------------------------------

fn list_legacy(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Some(stem) = path.file_stem() {
                out.push((stem.to_string_lossy().into_owned(), path));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Shared content decoding (modern content_json == legacy `content` array)
// ---------------------------------------------------------------------------

/// A message's decoded content: the IR blocks plus the facts that type the message — an `error`
/// block makes it a [`MessageKind::Error`]; control blocks (`systemNotification`, `actionRequired`,
/// `toolConfirmationRequest`) make it a [`MessageKind::Notice`] when they are all it holds.
#[derive(Default)]
struct Decoded {
    blocks: Vec<Block>,
    /// The first `error` block, as `{kind, message}`.
    error: Option<Value>,
    /// Every control block, verbatim (each is also rendered into `blocks` as text).
    control: Vec<Value>,
    /// Whether any block is conversational content (text, tools, thinking, media) rather than the
    /// rendering of a control block or an error.
    substantive: bool,
}

/// Build a [`Message`] from decoded content: a pure-tool-response `user` message is a
/// [`Role::Tool`] turn; an `error` block makes the message a [`MessageKind::Error`]; control
/// content alone makes it a [`MessageKind::Notice`]. The row's `metadata_json` may refine the kind
/// afterwards ([`apply_metadata`]).
fn build_message(role: &str, id: Option<String>, ts: Option<DateTime<Utc>>, decoded: Decoded) -> Option<Message> {
    let Decoded {
        blocks,
        error,
        control,
        substantive,
    } = decoded;
    if blocks.is_empty() {
        return None;
    }
    let mut ir_role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "tool" => Role::Tool,
        _ => Role::User,
    };
    // Goose stores tool results on `user` messages; if a message is *only* tool results, surface it
    // as a Tool turn so conversions re-encode it correctly.
    if ir_role == Role::User && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })) {
        ir_role = Role::Tool;
    }
    let mut m = Message::new(ir_role);
    if let Some(error) = error {
        // An error the model never answered (a provider gave up); the harness recorded it.
        m.kind = MessageKind::Error;
        m.origin = Origin::Harness;
        m.harness_extra_mut(Harness::Goose).insert("error".into(), error);
    } else if !substantive && !control.is_empty() {
        m.kind = MessageKind::Notice;
        m.origin = Origin::Harness;
    }
    if !control.is_empty() {
        m.harness_extra_mut(Harness::Goose)
            .insert("control_blocks".into(), Value::Array(control));
    }
    m.id = id;
    m.timestamp = ts;
    m.content = blocks;
    Some(m)
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Parse a JSON timestamp value: an RFC3339 string, a SQLite `TIMESTAMP` string, or unix
/// seconds/milliseconds (Goose's own reader splits them by magnitude too, session_manager.rs:728).
fn parse_ts_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_ts_text(s);
    }
    v.as_f64().and_then(ts_from_epoch)
}

/// Parse a textual timestamp: RFC3339 (`2024-01-01T12:00:00Z`) or SQLite (`2024-01-01 12:00:00`).
fn parse_ts_text(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(dt) = super::parse_ts(s) {
        return Some(dt);
    }
    // SQLite `CURRENT_TIMESTAMP` form (space separator, UTC, no offset).
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&ndt));
        }
    }
    None
}

/// Whole-`Session` convenience over [`stream_db_opts`] for tests: stream into a [`CollectSink`].
#[cfg(test)]
fn parse_db(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_db_opts(conn, r, &Demand::RICH, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

/// Whole-`Session` convenience over the legacy stream for tests.
#[cfg(test)]
fn parse_legacy(name: &str, path: &Path) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = legacy::stream(name, path, &Demand::RICH, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goose schema v16 (session_manager.rs, goose 1.51.0) — the columns as created today.
    const SCHEMA: &str = "
        CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
        INSERT INTO schema_version (version) VALUES (16);
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL DEFAULT '',
            description TEXT NOT NULL DEFAULT '',
            user_set_name BOOLEAN DEFAULT FALSE,
            session_type TEXT NOT NULL DEFAULT 'user',
            working_dir TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            extension_data TEXT DEFAULT '{}',
            total_tokens INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            accumulated_total_tokens INTEGER,
            accumulated_input_tokens INTEGER,
            accumulated_output_tokens INTEGER,
            accumulated_cache_read_tokens INTEGER,
            accumulated_cache_write_tokens INTEGER,
            accumulated_cost REAL,
            schedule_id TEXT,
            recipe_json TEXT,
            user_recipe_values_json TEXT,
            provider_name TEXT,
            model_config_json TEXT,
            goose_mode TEXT NOT NULL DEFAULT 'auto',
            archived_at TIMESTAMP,
            project_id TEXT,
            parent_session_id TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id TEXT,
            session_id TEXT NOT NULL REFERENCES sessions(id),
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            created_timestamp INTEGER NOT NULL,
            timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            tokens INTEGER,
            metadata_json TEXT
        );
        CREATE TABLE usage_ledger (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            created_timestamp INTEGER NOT NULL,
            model TEXT,
            input_tokens INTEGER,
            output_tokens INTEGER,
            total_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            cost REAL,
            cost_source TEXT,
            is_compaction INTEGER DEFAULT 0
        );
    ";

    /// An older schema lacking `description`, `provider_name`, `model_config_json`, `message_id`,
    /// `tokens` — exercises the column-probing degradation path.
    const SCHEMA_OLD: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL DEFAULT '',
            working_dir TEXT NOT NULL,
            created_at TIMESTAMP,
            updated_at TIMESTAMP
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            created_timestamp INTEGER NOT NULL
        );
    ";

    /// A store file holding `schema`; Goose reads cells against the file they live in.
    fn mk(schema: &str) -> (cv_testkit::World, Connection) {
        let (world, dir) = tempdir();
        let c = Connection::open(dir.join("sessions.db")).unwrap();
        c.execute_batch(schema).unwrap();
        (world, c)
    }

    /// A message's `extra["goose"]` bag (the only place a Goose message fact may live).
    fn bag(m: &Message) -> &serde_json::Map<String, Value> {
        m.harness_extra(Harness::Goose).expect("extra[\"goose\"]")
    }

    /// Every `extra` key, on the session and on each message, is `goose` (or cv's own namespace,
    /// on a turn cv synthesized): the shared IR-v2 nesting invariant
    /// ([`crate::harness::assert_no_flat_keys`]), tightened to goose's one namespace.
    fn assert_nested_extra(s: &Session) {
        crate::harness::assert_no_flat_keys(s);
        for k in s.extra.keys() {
            assert_eq!(k, Harness::Goose.as_str(), "flat session extra key {k:?}");
        }
        for (i, m) in s.messages.iter().enumerate() {
            for k in m.extra.keys() {
                let cv = k == crate::ir::CV_NAMESPACE && m.is_synthesized();
                assert!(
                    cv || k == Harness::Goose.as_str(),
                    "flat extra key {k:?} on message {i}"
                );
            }
        }
    }

    fn kinds(s: &Session) -> Vec<(Role, MessageKind, Origin)> {
        s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect()
    }

    fn at(c: &Connection) -> PathBuf {
        PathBuf::from(c.path().expect("file store"))
    }

    fn sref(c: &Connection, id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Goose, at(c))
    }

    /// Every corpus version: a row written to one session moves its certificate alone, and every
    /// other session reads exactly as before ([`super::super::store::sqlite::assert_rows_move_alone`]).
    #[test]
    fn corpus_rows_certify_each_session_and_move_only_with_its_rows() {
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/goose");
        let mut versions: Vec<_> = std::fs::read_dir(&corpus).unwrap().map(|e| e.unwrap().path()).collect();
        versions.sort();
        assert!(!versions.is_empty());
        for version in versions {
            let world = crate::test_world("goose-corpus-rows");
            let dir = world.base.join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("sessions.db");
            std::fs::copy(version.join("sessions/sessions.db"), &db).unwrap();
            // A session whose messages no other session holds: no fork resolution consults it.
            let leaf: String = Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT s.id FROM sessions s WHERE EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id) \
                     AND NOT EXISTS (SELECT 1 FROM messages m JOIN messages o ON o.message_id = m.message_id \
                     AND o.session_id != m.session_id WHERE m.session_id = s.id) ORDER BY s.id LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            super::super::store::sqlite::assert_rows_move_alone(&Goose::with_sessions_dir(&dir), &db, &leaf, |c| {
                c.execute(
                    "INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) \
                     VALUES ('zq', ?1, 'user', '[{\"type\":\"text\",\"text\":\"zqcertified\"}]', 4102444800, '{}')",
                    [&leaf],
                )
                .unwrap();
            });
        }
    }

    #[test]
    fn windowed_reads_keep_the_model() {
        let _world = crate::test_world("goose-windowed");
        let sessions = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/goose/1.54.0/sessions");
        crate::harness::assert_windowed_models(&Goose::with_sessions_dir(&sessions));
    }

    #[test]
    fn provider_without_model_stays_in_the_bag() {
        let (_world, c) = mk(SCHEMA);
        c.execute(
            "INSERT INTO sessions (id, working_dir, provider_name) VALUES ('s', '/fixture', 'anthropic')",
            [],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s")).unwrap();
        assert!(s.model.is_none());
        assert_eq!(s.harness_extra(Harness::Goose).unwrap()["provider_name"], "anthropic");
        let mut m = Message::new(Role::Assistant);
        apply_metadata(&mut m, serde_json::json!({"inference":{"provider":"anthropic"}}), false);
        assert!(m.model.is_none());
        assert_eq!(bag(&m)["metadata"]["inference"]["provider"], "anthropic");
    }

    #[test]
    fn parses_modern_db_text_and_tools() {
        let (_world, c) = mk(SCHEMA);
        c.execute(
            "INSERT INTO sessions (id, description, working_dir, created_at, updated_at, provider_name, model_config_json) \
             VALUES ('s1','Build a thing','/home/u/proj','2024-01-01 12:00:00','2024-01-01 12:05:00','anthropic','{\"model_name\":\"claude-sonnet-4\"}')",
            [],
        ).unwrap();
        // user text
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) \
             VALUES ('s1','user','[{\"type\":\"text\",\"text\":\"Hello\"}]',1704110400)",
            [],
        )
        .unwrap();
        // assistant: thinking + text + tool request
        let asst = r#"[{"type":"thinking","thinking":"let me think","signature":"sig"},{"type":"text","text":"Working on it"},{"type":"toolRequest","id":"call_1","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"ls"}}}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','assistant',?1,1704110401)",
            [asst],
        ).unwrap();
        // tool response carried on a user message
        let toolresp = r#"[{"type":"toolResponse","id":"call_1","toolResult":{"status":"success","value":{"content":[{"type":"text","text":"file1\nfile2"}],"isError":false}}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,1704110402)",
            [toolresp],
        ).unwrap();

        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert_eq!(s.title.as_deref(), Some("Build a thing"));
        assert_eq!(s.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/proj"));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4"));
        assert!(s.created_at.is_some());
        assert_eq!(s.messages.len(), 3);

        // user
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("Hello"));

        // assistant: thinking, text, tooluse
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(
            matches!(&a.content[0], Block::Thinking { text, signature, .. } if text.matches_str("let me think") == Some(true) && signature.as_deref() == Some("sig"))
        );
        assert!(matches!(&a.content[1], Block::Text { text , .. } if text.matches_str("Working on it") == Some(true)));
        assert!(
            matches!(&a.content[2], Block::ToolUse { name, id, input , ..} if name == "shell" && id == "call_1" && input.resolve().unwrap().get("command").and_then(Value::as_str) == Some("ls"))
        );

        // tool response reclassified to Role::Tool, paired name recovered
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, tool_name, is_error, .. }
            if tool_use_id == "call_1" && content.matches_str("file1\nfile2") == Some(true) && tool_name.as_deref() == Some("shell") && !is_error)
        );
    }

    /// The store a real goose 1.51.0 wrote (see the module doc for how). Every assertion here is a
    /// fact observed on disk, not a fixture we authored.
    #[test]
    fn real_1_51_0_store_round_trips() {
        let path = format!(
            "{}/tests/fixtures/goose/sessions-v16-1.51.0.db",
            env!("CARGO_MANIFEST_DIR")
        );
        let c = open_ro(Path::new(&path)).unwrap();
        let version: i64 = c
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 16);

        // Discovery: four sessions, titled by `name`, cwd from `working_dir`, real timestamps.
        let refs = discover_db(&c, Path::new(&path)).unwrap();
        assert_eq!(refs.len(), 4);
        let by_id = |id: &str| refs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(
            by_id("20260919_6").title.as_deref(),
            Some("Parser refactor session"),
            "`goose session import` titles from the Claude transcript's ai-title"
        );
        assert_eq!(by_id("20260919_6").cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(by_id("20260919_3").title.as_deref(), Some("stubturn"));
        assert!(by_id("20260919_3")
            .created_at
            .is_some_and(|t| t.to_rfc3339().starts_with("2026-09-19T20:16")));

        // A stub-provider turn: prompt, the injected `<turn-context>` row (hidden from the user →
        // System / injected context), the reply with real usage; the session model is
        // the bare model and the reply records the same shape from its inference metadata.
        let s = parse_db(&c, &sref(&c, "20260919_3")).unwrap();
        assert_eq!(s.model.as_deref(), Some("stub-model"));
        assert_eq!(
            kinds(&s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("say pong"));
        assert_eq!(
            bag(&s.messages[0]).keys().collect::<Vec<_>>(),
            vec!["metadata"],
            "a prompt carries only its metadata_json"
        );
        assert_eq!(bag(&s.messages[1])["metadata"]["turnContext"], true);
        assert!(s.messages[1].text().unwrap().starts_with("<turn-context>"));
        let a = &s.messages[2];
        assert_eq!(a.model.as_deref(), Some("stub-model"), "the row's inference metadata");
        let u = a.usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cost_usd),
            (Some(324), Some(17), Some(100), None)
        );
        assert_eq!(bag(a)["metadata"]["inference"]["provider"], "openai");
        assert_eq!(s.harness_extra(Harness::Goose).unwrap()["session_type"], "user");
        assert!(s.lineage.is_empty());
        assert_nested_extra(&s);

        // An imported Claude Code transcript (cv's own `rich_blocks.jsonl` fixture, run through
        // `goose session import`): tool calls and results keep their names, thinking survives. The
        // store records no provenance (`session_type` is forced to `user`), so its prompts read as
        // a person's — `Origin::Import` is not derivable from a goose 1.51 store.
        let s = parse_db(&c, &sref(&c, "20260919_6")).unwrap();
        assert_eq!(s.messages[0].origin, Origin::Human);
        assert!(s.harness_extra(Harness::Goose).unwrap()["session_type"] == "user");
        let tool_names: Vec<&str> = s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_names, vec!["Edit"], "{tool_names:?}");
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        let results: Vec<&Block> = s
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .flat_map(|m| &m.content)
            .collect();
        assert!(!results.is_empty());
        assert!(results
            .iter()
            .all(|b| matches!(b, Block::ToolResult { tool_name: Some(_), .. })));

        // Retry exhaustion: goose 1.51.0 persists the message as a plain text block — a reply, not
        // an `error` block, so it is not a `MessageKind::Error`.
        let s = parse_db(&c, &sref(&c, "20260919_5")).unwrap();
        let last = s.messages.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(last.kind, MessageKind::Reply);
        assert!(
            matches!(&last.content[0], Block::Text { text , .. } if text.starts_with("Maximum retry attempts (1) exceeded"))
        );
        assert_nested_extra(&s);
        // The dead-endpoint session recorded no assistant row at all.
        let s = parse_db(&c, &sref(&c, "20260919_4")).unwrap();
        assert!(s.messages.iter().all(|m| m.role != Role::Assistant));
    }

    #[test]
    fn compaction_summary_is_the_agent_only_row_after_everything_went_agent_invisible() {
        // What real 1.51.0/1.54.0 compaction writes (tests/corpus/goose): every earlier row flipped to
        // agentVisible:false, then an agent-only summary, continuation and preserved prompt, none
        // carrying `usage`. A tool-pair summary is agent-only too, but earlier rows stay visible.
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('c','/p')", [])
            .unwrap();
        let rows = [
            ("user", "prompt one", r#"{"userVisible":true,"agentVisible":false}"#),
            ("assistant", "reply one", r#"{"userVisible":true,"agentVisible":false}"#),
            (
                "user",
                "# Conversation Summary",
                r#"{"userVisible":false,"agentVisible":true}"#,
            ),
            (
                "assistant",
                "Your context was compacted.",
                r#"{"userVisible":false,"agentVisible":true}"#,
            ),
            ("user", "prompt two", r#"{"userVisible":false,"agentVisible":true}"#),
            ("assistant", "reply two", r#"{"userVisible":true,"agentVisible":true}"#),
            (
                "user",
                "A call to shell was made",
                r#"{"userVisible":false,"agentVisible":true}"#,
            ),
        ];
        for (i, (role, text, meta)) in rows.iter().enumerate() {
            let content = serde_json::json!([{"type": "text", "text": text}]).to_string();
            c.execute(
                "INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) \
                 VALUES (?1, 'c', ?2, ?3, ?4, ?5)",
                rusqlite::params![format!("m{i}"), role, content, 1_704_110_400 + i as i64, meta],
            )
            .unwrap();
        }
        let s = parse_db(&c, &sref(&c, "c")).unwrap();
        let k: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            k,
            vec![
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::InjectedContext,
                MessageKind::InjectedContext,
                MessageKind::Reply,
                MessageKind::InjectedContext,
            ],
            "one boundary, and the later tool-pair summary is not a compaction"
        );
    }

    #[test]
    fn fork_lineage_normalizes_seconds_and_milliseconds() {
        for milliseconds in [false, true] {
            let (_world, conn) = mk(SCHEMA);
            for (id, created) in [
                ("source", "2024-01-01 00:00:00"),
                ("fork", "2024-01-03 00:00:00"),
                ("later-fork", "2024-01-04 00:00:00"),
            ] {
                conn.execute(
                    "INSERT INTO sessions (id,working_dir,created_at) VALUES (?1,'/repo',?2)",
                    rusqlite::params![id, created],
                )
                .unwrap();
                let at = 1704153600_i64 * if milliseconds { 1000 } else { 1 };
                conn.execute("INSERT INTO messages (session_id,message_id,role,content_json,created_timestamp) VALUES (?1,'shared','user',?2,?3)", rusqlite::params![id,r#"[{"type":"text","text":"copied"}]"#,at]).unwrap();
            }
            let created = parse_ts_text("2024-01-03 00:00:00");
            assert_eq!(fork_source(&conn, "fork", created).as_deref(), Some("source"));
            assert_eq!(
                fork_source(&conn, "later-fork", parse_ts_text("2024-01-04 00:00:00")).as_deref(),
                Some("source")
            );
            assert!(fork_source(&conn, "source", parse_ts_text("2024-01-01 00:00:00")).is_none());
        }
    }

    #[test]
    fn fork_scoped_custom_nocase_ids_preserve_sql_current_holder_exclusion() -> Result<()> {
        let world = crate::test_world("goose-nocase-fork-scope");
        let path = world.base.join("sessions.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(&SCHEMA.replace(
            "session_id TEXT NOT NULL REFERENCES sessions(id)",
            "session_id TEXT COLLATE NOCASE NOT NULL REFERENCES sessions(id)",
        ))?;
        for (id, at) in [
            ("CASE", "2024-01-01 00:00:00"),
            ("origin", "2024-01-02 00:00:00"),
            ("case", "2024-01-03 00:00:00"),
        ] {
            conn.execute(
                "INSERT INTO sessions(id,working_dir,created_at) VALUES (?1,'/synthetic',?2)",
                rusqlite::params![id, at],
            )?;
            conn.execute("INSERT INTO messages(session_id,message_id,role,content_json,created_timestamp) VALUES (?1,'shared','user','[]',1703980800)",[id])?;
        }
        assert!(!binary_holder_ids(&conn));
        let mut reads = crate::source::ReadSet::default();
        reads.record(&path);
        let created = parse_ts_text("2024-01-03 00:00:00");
        let _scope = crate::source::read_scope();
        for _ in 0..2 {
            let oracle = fork_source(&conn, "case", created);
            assert_eq!(oracle.as_deref(), Some("origin"));
            assert_eq!(
                fork_source_scoped(&conn, &SessionRef::new("case", Harness::Goose, &path), created, &reads)?,
                oracle
            );
        }
        Ok(())
    }

    #[test]
    fn fork_family_holder_is_shared_within_a_read_scope_and_fresh_between_scopes() -> Result<()> {
        let world = crate::test_world("goose-fork-family-scope");
        let path = world.base.join("sessions.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        for i in 0..80 {
            conn.execute(
                "INSERT INTO sessions(id,working_dir,created_at) VALUES (?1,'/synthetic',?2)",
                rusqlite::params![
                    format!("family-{i:03}"),
                    format!("2024-01-{:02} 00:00:00", i / 24 + 1) + &format!(".{:03}", i)
                ],
            )?;
            conn.execute("INSERT INTO messages(session_id,message_id,role,content_json,created_timestamp) VALUES (?1,'shared','user','[{\"type\":\"text\",\"text\":\"copied\"}]',1703980800)",
                [format!("family-{i:03}")])?;
        }
        drop(conn);
        let adapter = Goose::with_sessions_dir(&world.base);
        let read = |i| -> Result<Session> {
            let reference = SessionRef::new(format!("family-{i:03}"), Harness::Goose, &path);
            crate::stream::collect_with(&adapter, &reference, &Demand::RICH)
        };
        {
            let _scope = crate::source::read_scope();
            for i in 1..80 {
                assert_eq!(read(i)?.lineage.forked_from.as_deref(), Some("family-000"));
            }
        }
        for i in 1..4 {
            assert_eq!(read(i)?.lineage.forked_from.as_deref(), Some("family-000"));
        }
        // Exercise the real multi-session consumer without a test-provided source scope.
        assert!(!crate::source::inventory_scoped());
        let references: Vec<_> = (1..80)
            .map(|i| SessionRef::new(format!("family-{i:03}"), Harness::Goose, &path))
            .collect();
        let stats = crate::stats::TokenStats::compute(&references)?;
        assert_eq!(stats.sessions, 79);
        assert_eq!(stats.failed, 0);
        assert!(!crate::source::inventory_scoped());
        // A later operation observes changed holder metadata rather than keeping the old minimum.
        let conn = Connection::open(&path)?;
        conn.execute(
            "UPDATE sessions SET created_at='2023-01-01 00:00:00' WHERE id='family-001'",
            [],
        )?;
        drop(conn);
        {
            let _scope = crate::source::read_scope();
            assert_eq!(read(2)?.lineage.forked_from.as_deref(), Some("family-001"));
        }
        Ok(())
    }

    #[test]
    fn title_prefers_the_written_name_column() {
        // Goose only ever writes `name`; `description` is a never-written default '' (v16 DDL).
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, name, working_dir, updated_at) VALUES ('n','Fix flaky test','/p','2026-09-01 00:00:00')", []).unwrap();
        c.execute("INSERT INTO sessions (id, description, working_dir, updated_at) VALUES ('d','Only description','/p','2026-08-01 00:00:00')", []).unwrap();
        c.execute(
            "INSERT INTO sessions (id, working_dir, updated_at) VALUES ('e','/p','2026-07-01 00:00:00')",
            [],
        )
        .unwrap();
        let refs = discover_db(&c, &at(&c)).unwrap();
        let by_id = |id: &str| refs.iter().find(|r| r.id == id).unwrap().title.clone();
        assert_eq!(by_id("n").as_deref(), Some("Fix flaky test"));
        assert_eq!(
            by_id("d").as_deref(),
            Some("Only description"),
            "description still a fallback"
        );
        assert_eq!(by_id("e"), None, "empty strings are not titles");
        assert_eq!(
            parse_db(&c, &sref(&c, "n")).unwrap().title.as_deref(),
            Some("Fix flaky test")
        );
    }

    #[test]
    fn metadata_json_yields_usage_model_and_flags() {
        let (_world, c) = mk(SCHEMA);
        c.execute(
            "INSERT INTO sessions (id, name, working_dir, session_type, parent_session_id, provider_name, model_config_json) \
             VALUES ('s1','t','/x','sub_agent','root-1','anthropic','{\"model_name\":\"claude-sonnet-4-5\"}')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) \
             VALUES ('m0','s1','user','[{\"type\":\"text\",\"text\":\"subtask\"}]',1704110399,'{\"userVisible\":true,\"agentVisible\":true}')",
            [],
        )
        .unwrap();
        let meta = r#"{"userVisible":true,"agentVisible":true,"inference":{"provider":"anthropic","requestedModel":"claude-sonnet-4-5","resolvedModel":"claude-sonnet-4-5-20250929"},"usage":{"inputTokens":1200,"outputTokens":85,"totalTokens":1285,"cacheReadTokens":900,"cacheWriteTokens":0,"cost":0.0041,"costSource":"provider_reported","elapsedMs":2310,"timeToFirstTokenMs":410}}"#;
        c.execute(
            "INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) VALUES ('m1','s1','assistant','[{\"type\":\"text\",\"text\":\"hi\"}]',1704110400,?1)",
            [meta],
        )
        .unwrap();
        let hidden = r#"{"userVisible":false,"agentVisible":true,"usage":{"inputTokens":10,"outputTokens":2,"isCompaction":true}}"#;
        c.execute(
            "INSERT INTO messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) VALUES ('m2','s1','assistant','[{\"type\":\"text\",\"text\":\"summary\"}]',1704110401,?1)",
            [hidden],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert_eq!(s.harness_extra(Harness::Goose).unwrap()["session_type"], "sub_agent");
        assert_eq!(
            s.lineage,
            Lineage {
                parent: Some("root-1".into()),
                ..Default::default()
            }
        );
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-5"));
        // prompt / reply / the compaction pair (boundary synthesized before the agent-only summary)
        assert_eq!(
            kinds(&s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Subagent),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::CompactionSummary, Origin::Harness),
            ]
        );
        let m = &s.messages[1];
        let u = m.usage.as_ref().expect("usage from metadata_json");
        assert_eq!((u.input_tokens, u.output_tokens), (Some(1200), Some(85)));
        assert_eq!((u.cache_read_tokens, u.cache_creation_tokens), (Some(900), Some(0)));
        assert_eq!(u.cost_usd, Some(0.0041), "provider-reported cost is first-class");
        assert_eq!(
            m.model.as_deref(),
            Some("claude-sonnet-4-5-20250929"),
            "the resolved snapshot differs from the session model, so it is recorded"
        );
        assert_eq!(bag(m)["metadata"]["usage"]["costSource"], "provider_reported");
        let (boundary, h) = (&s.messages[2], &s.messages[3]);
        assert_eq!(h.parent_id, boundary.id);
        assert_eq!(
            bag(h)["metadata"]["userVisible"],
            false,
            "hidden rows are kept, flagged"
        );
        assert_eq!(bag(h)["metadata"]["usage"]["isCompaction"], true);
        assert!(h.model.is_none(), "no inference → no model");
        assert_eq!(crate::compaction::detect_in_session(&s, true).unwrap().len(), 1);
        assert_nested_extra(&s);
    }

    #[test]
    fn prompt_origin_follows_session_type() {
        let (_world, c) = mk(SCHEMA);
        for (id, ty) in [("u", "user"), ("s", "scheduled"), ("h", "hidden"), ("a", "acp")] {
            c.execute(
                "INSERT INTO sessions (id, working_dir, session_type) VALUES (?1, '/x', ?2)",
                [id, ty],
            )
            .unwrap();
            c.execute(
                "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES (?1,'user','[{\"type\":\"text\",\"text\":\"go\"}]',1)",
                [id],
            )
            .unwrap();
        }
        let origin = |id: &str| parse_db(&c, &sref(&c, id)).unwrap().messages[0].origin;
        assert_eq!(origin("u"), Origin::Human);
        assert_eq!(origin("s"), Origin::Scheduler);
        assert_eq!(origin("h"), Origin::Harness);
        assert_eq!(origin("a"), Origin::Human);
    }

    #[test]
    fn error_and_document_blocks_map() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        let content = r#"[{"type":"error","kind":"contextLengthExceeded","message":"Maximum retry attempts (3) exceeded."},{"type":"document","data":"cGRmLWJ5dGVz","mimeType":"application/pdf","name":"q3-report.pdf"}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','assistant',?1,1)",
            [content],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        let m = &s.messages[0];
        assert_eq!(m.role, Role::Assistant);
        assert_eq!((m.kind, m.origin), (MessageKind::Error, Origin::Harness));
        assert_eq!(bag(m)["error"]["kind"], "contextLengthExceeded");
        assert_eq!(bag(m)["error"]["message"], "Maximum retry attempts (3) exceeded.");
        assert_eq!(m.content.len(), 2, "neither block is dropped");
        assert!(
            matches!(&m.content[0], Block::Text { text , .. } if text.matches_str("[error: contextLengthExceeded] Maximum retry attempts (3) exceeded.") == Some(true))
        );
        assert!(matches!(&m.content[1], Block::File { mime, path, source , .. }
                if mime.as_deref() == Some("application/pdf") && path.as_deref() == Some("q3-report.pdf") && source.as_deref() == Some("base64:inline")));
        assert_nested_extra(&s);
    }

    #[test]
    fn control_blocks_are_notices() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        let rows = [
            // a bare notification → a notice, rendered as Goose prints it
            (
                "user",
                r#"[{"type":"systemNotification","notificationType":"progressMessage","msg":"Loading extension developer"}]"#,
            ),
            // a bare confirmation request → a notice
            (
                "assistant",
                r#"[{"type":"toolConfirmationRequest","id":"c1","toolName":"developer__shell","arguments":{"command":"rm -rf x"},"prompt":null}]"#,
            ),
            // an action-required elicitation → a notice
            (
                "user",
                r#"[{"type":"actionRequired","data":{"actionType":"elicitation","id":"e1","message":"Which branch?","requestedSchema":{}}}]"#,
            ),
            // real content alongside a control block keeps its kind
            (
                "assistant",
                r#"[{"type":"text","text":"I need to run a command."},{"type":"actionRequired","data":{"actionType":"toolConfirmation","id":"c2","toolName":"developer__shell","arguments":{},"prompt":null}}]"#,
            ),
        ];
        for (i, (role, content)) in rows.iter().enumerate() {
            c.execute(
                "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1', ?1, ?2, ?3)",
                rusqlite::params![role, content, i as i64 + 1],
            )
            .unwrap();
        }
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert_eq!(
            kinds(&s),
            vec![
                (Role::User, MessageKind::Notice, Origin::Harness),
                (Role::Assistant, MessageKind::Notice, Origin::Harness),
                (Role::User, MessageKind::Notice, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "[SystemNotification: Loading extension developer]",
                "[ToolConfirmationRequest: developer__shell]",
                "[ActionRequired: Elicitation - Which branch?]",
                "I need to run a command.\n[ActionRequired: ToolConfirmation for developer__shell]",
            ]
        );
        assert_eq!(bag(&s.messages[0])["control_blocks"][0]["type"], "systemNotification");
        assert_eq!(
            bag(&s.messages[3])["control_blocks"][0]["data"]["actionType"],
            "toolConfirmation"
        );
        assert_nested_extra(&s);
    }

    #[test]
    fn bare_array_results_and_rmcp3_content_flatten() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        // legacy `SuccessWithContentVec`: `value` is the content array itself, with rmcp 3 blocks
        let resp = r#"[{"type":"toolResponse","id":"c1","toolResult":{"status":"success","value":[{"type":"text","text":"ok"},{"type":"resource","resource":{"uri":"file:///tmp/a.txt","mimeType":"text/plain","text":"hello"}},{"type":"resource_link","uri":"file:///tmp/b.bin","name":"b"},{"type":"audio","data":"…","mimeType":"audio/wav"}]}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,1704110400123)",
            [resp],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::ToolResult { content, is_error, .. }
                if content.matches_str("ok\nhello\n[resource: file:///tmp/b.bin]\n[audio: audio/wav]") == Some(true) && !is_error));
        // a millisecond `created_timestamp` is read as milliseconds (Goose's own reader divides it
        // down to whole seconds for display; the IR keeps what the column holds)
        assert_eq!(
            m.timestamp.map(|t| t.to_rfc3339()),
            Some("2024-01-01T12:00:00.123+00:00".to_string())
        );
    }

    #[test]
    fn parses_tool_error_response() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        let resp = r#"[{"type":"toolResponse","id":"c9","toolResult":{"status":"error","error":"boom"}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,100)",
            [resp],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert!(
            matches!(&s.messages[0].content[0], Block::ToolResult { is_error, content, .. } if *is_error && content.matches_str("boom") == Some(true))
        );
    }

    #[test]
    fn delegate_result_keeps_the_child_session_id_in_details() {
        // summon's `delegate` result as Goose 1.51.0/1.54.0 write it (tests/corpus/goose).
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        let resp = r#"[{"type":"toolResponse","id":"c1","toolResult":{"status":"success","value":{"resultType":"complete","content":[{"type":"text","text":"ok"}],"isError":false,"_meta":{"subagent_session_id":"20261011_7"}}}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,100)",
            [resp],
        )
        .unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        let Block::ToolResult { details, content, .. } = &s.messages[0].content[0] else {
            panic!("a tool result")
        };
        assert_eq!(content.inline_str(), Some("ok"));
        assert_eq!(
            details.as_ref().and_then(|d| d.pointer("/_meta/subagent_session_id")),
            Some(&Value::from("20261011_7"))
        );
    }

    #[test]
    fn metadata_listing_refills_complete_counts_for_sqlite_and_legacy_sources() -> Result<()> {
        let w = crate::test_world("goose-deferred-counts");
        let dir = w.home.join(".local/share/goose/sessions");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("sessions.db");
        let writer = Connection::open(&path)?;
        writer.execute_batch(SCHEMA)?;
        writer.execute(
            "INSERT INTO sessions(id,description,working_dir) VALUES ('native','Native','/native')",
            [],
        )?;
        writer.execute_batch("INSERT INTO messages(session_id,role,content_json,created_timestamp) VALUES ('native','user','[{\"type\":\"text\",\"text\":\"question\"}]',1),('native','assistant','[{\"type\":\"text\",\"text\":\"answer\"}]',2)")?;
        let legacy = dir.join("legacy.jsonl");
        let first = "{\"description\":\"Legacy\",\"working_dir\":\"/legacy\"}\n{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"question\"}]}\n";
        std::fs::write(&legacy, first)?;
        let refs = crate::sessions_for(Some(Harness::Goose));
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().all(|r| r.message_count.is_none()));
        let adapter = Goose::with_sessions_dir(&dir);
        for reference in &refs {
            crate::stream::stream_counted(
                &adapter,
                reference,
                &Demand::FACTS.with_spans(),
                &mut crate::stream::Discard,
                crate::stream::Pass::RECORD,
            )?;
        }
        let check = |refs: &[SessionRef], native, legacy| {
            assert_eq!(refs.iter().find(|r| r.id == "native").unwrap().message_count, native);
            assert_eq!(refs.iter().find(|r| r.id == "legacy").unwrap().message_count, legacy);
        };
        check(&crate::sessions_for(Some(Harness::Goose)), Some(2), Some(1));
        let refreshed = crate::discover_all();
        check(&refreshed, Some(2), Some(1));
        for mut reference in refreshed {
            reference.message_count = None;
            assert_eq!(
                serde_json::to_value(&reference)?,
                serde_json::to_value(refs.iter().find(|old| old.id == reference.id).unwrap())?
            );
        }
        writer.execute("UPDATE sessions SET description='Renamed' WHERE id='native'", [])?;
        let changed = crate::sessions_for(Some(Harness::Goose));
        check(&changed, None, Some(1));
        assert_eq!(
            changed.iter().find(|r| r.id == "native").unwrap().title.as_deref(),
            Some("Renamed")
        );
        std::fs::write(
            &legacy,
            format!("{first}{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"next\"}}]}}\n"),
        )?;
        check(&crate::sessions_for(Some(Harness::Goose)), None, None);
        Ok(())
    }

    #[test]
    fn discovery_defers_counts_and_streams_preserve_shapes_visibility_and_corruption() {
        let (_world, conn) = mk(SCHEMA);
        let giant = "雪λ\n".repeat(20000);
        let mut cases = vec![
            ("user", "null".to_owned(), None),
            ("assistant", "[]".to_owned(), None),
            (
                "user",
                r#"["scalar",1,null,{"type":"text","text":"accepted"}]"#.to_owned(),
                None,
            ),
            ("user", r#""bare prompt""#.to_owned(), None),
            ("user", r#""""#.to_owned(), None),
            (
                "assistant",
                r#"[{"type":"toolResponse","toolResult":null}]"#.to_owned(),
                None,
            ),
            (
                "user",
                r#"[{"type":"toolResponse","toolResult":null}]"#.to_owned(),
                None,
            ),
            ("user", r#"[{"type":"toolResponse"}]"#.to_owned(), None),
            (
                "user",
                r#"[{"type":"text","text":""}]"#.to_owned(),
                Some(r#"{"userVisible":0}"#),
            ),
            (
                "user",
                r#"[{"type":"text","text":"x"}]"#.to_owned(),
                Some(r#"{"userVisible":false}"#),
            ),
            (
                "assistant",
                r#"[{"type":"text","text":"x"}]"#.to_owned(),
                Some(r#"{"userVisible":false,"userVisible":true}"#),
            ),
            (
                "user",
                r#"[{"type":"text","text":"x"}]"#.to_owned(),
                Some(r#"{"userVisible":true,"userVisible":false}"#),
            ),
            (
                "user",
                r#"[{"type":"text","text":"x","ignored":1e999}]"#.to_owned(),
                None,
            ),
            (
                "user",
                r#"[{"type":"text","text":"x","ignored":"\ud800"}]"#.to_owned(),
                None,
            ),
            (
                "assistant",
                r#"[{"type":"error"},{"type":"text","text":"also text"}]"#.to_owned(),
                None,
            ),
            (
                "other",
                r#"[{"type":"text","text":"unknown role defaults user"}]"#.to_owned(),
                Some("malformed"),
            ),
            (
                "assistant",
                r#"[{"type":"text","type":"toolResponse","toolResult":null}]"#.to_owned(),
                None,
            ),
        ];
        for ty in [
            "systemNotification",
            "actionRequired",
            "toolConfirmationRequest",
            "error",
            "thinking",
            "redactedThinking",
            "image",
            "document",
            "tool_use",
            "tool_result",
        ] {
            cases.push((
                "assistant",
                serde_json::json!([{"type":ty,"text":giant,"data":giant,"ignored":giant}]).to_string(),
                None,
            ));
        }
        for call in [
            serde_json::json!(null),
            serde_json::json!({"status":"error","error":giant}),
            serde_json::json!({"value":null}),
            serde_json::json!({"value":{"arguments":{"body":giant}}}),
        ] {
            cases.push((
                "assistant",
                serde_json::json!([{"type":"toolRequest","toolCall":call}]).to_string(),
                None,
            ));
        }
        for (i, (role, body, metadata)) in cases.iter().enumerate() {
            let id = format!("shape-{i}");
            conn.execute("INSERT INTO sessions(id,working_dir) VALUES (?1,'/synthetic')", [&id])
                .unwrap();
            conn.execute("INSERT INTO messages(session_id,role,content_json,created_timestamp,metadata_json) VALUES (?1,?2,?3,1,?4)",
                rusqlite::params![id,role,body,metadata]).unwrap();
        }
        for reference in discover_db(&conn, &at(&conn)).unwrap() {
            let parsed = parse_db(&conn, &sref(&conn, &reference.id)).unwrap();
            assert_eq!(reference.message_count, None);
            let mut sink = crate::stream::CollectSink::default();
            stream_db_opts(&conn, &reference, &Demand::LEAN.with_spans(), &mut sink).unwrap();
            assert_eq!(
                sink.messages.iter().filter(|m| m.is_turn()).count(),
                parsed.messages.iter().filter(|m| m.is_turn()).count(),
                "{}",
                reference.id
            );
        }
    }

    #[test]
    fn discovery_keeps_known_sessions_when_message_count_schema_is_unavailable() -> Result<()> {
        let (_world, conn) = mk(SCHEMA);
        conn.execute("INSERT INTO sessions(id,working_dir) VALUES ('known','/synthetic')", [])?;
        conn.execute("DROP TABLE messages", [])?;
        for incomplete in [None, Some("CREATE TABLE messages(session_id TEXT,content_json TEXT)")] {
            if let Some(schema) = incomplete {
                conn.execute_batch(schema)?;
            }
            let refs = discover_db(&conn, &at(&conn))?;
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].id, "known");
            assert_eq!(refs[0].message_count, None);
        }
        Ok(())
    }

    #[test]
    fn discover_db_orders_without_reading_message_counts() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, description, working_dir, updated_at) VALUES ('a','A','/x','2024-01-01 00:00:00')", []).unwrap();
        c.execute("INSERT INTO sessions (id, description, working_dir, updated_at) VALUES ('b','B','/y','2024-02-01 00:00:00')", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','user','[{\"type\":\"text\",\"text\":\"hi\"}]',1)", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','assistant','[{\"type\":\"text\",\"text\":\"yo\"}]',2)", []).unwrap();
        // A user row that is only a tool result is a Tool turn; a corrupt cell is not a turn. Neither
        // is counted (`SessionRef::message_count`: user + assistant turns).
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','user','[{\"type\":\"toolResponse\",\"id\":\"t1\",\"toolResult\":{\"status\":\"success\",\"value\":{\"content\":[]}}}]',3)", []).unwrap();
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','user','not json',4)",
            [],
        )
        .unwrap();
        let refs = discover_db(&c, &at(&c)).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].id, "b"); // updated_at DESC
        assert_eq!(refs[1].id, "a");
        assert_eq!(
            refs[1].message_count, None,
            "discovery does not reduce the messages table"
        );
        assert_eq!(refs[0].title.as_deref(), Some("B"));
        assert_eq!(
            parse_db(&c, &refs[1])
                .unwrap()
                .messages
                .iter()
                .filter(|m| m.is_turn())
                .count(),
            2
        );
    }

    #[test]
    fn old_schema_missing_columns_still_parses() {
        let (_world, c) = mk(SCHEMA_OLD);
        c.execute(
            "INSERT INTO sessions (id, name, working_dir) VALUES ('s1','My Session','/p')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user','[{\"type\":\"text\",\"text\":\"old\"}]',1704110400)", []).unwrap();
        let refs = discover_db(&c, &at(&c)).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].title.as_deref(), Some("My Session")); // falls back to `name`
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("old"));
        assert_eq!(s.title.as_deref(), Some("My Session"));
        assert!(s.model.is_none());
    }

    #[test]
    fn parses_legacy_jsonl() {
        let (_world, dir) = tempdir();
        let path = dir.join("20240101_120000.jsonl");
        let content = concat!(
            "{\"description\":\"legacy chat\",\"working_dir\":\"/home/u/old\",\"created_at\":\"2024-01-01T12:00:00Z\",\"updated_at\":\"2024-01-01T12:01:00Z\"}\n",
            "{\"id\":\"m1\",\"role\":\"user\",\"created\":1704110400,\"content\":[{\"type\":\"text\",\"text\":\"Hi\"}]}\n",
            "{\"id\":\"m2\",\"role\":\"assistant\",\"created\":1704110401,\"content\":[{\"type\":\"text\",\"text\":\"Hello!\"},{\"type\":\"toolRequest\",\"id\":\"t1\",\"toolCall\":{\"status\":\"success\",\"value\":{\"name\":\"developer__shell\",\"arguments\":{\"command\":\"pwd\"}}}}]}\n",
            "{\"id\":\"m3\",\"role\":\"user\",\"created\":1704110402,\"content\":[{\"type\":\"toolResponse\",\"id\":\"t1\",\"toolResult\":{\"status\":\"success\",\"value\":{\"content\":[{\"type\":\"text\",\"text\":\"/home/u/old\"}]}}}]}\n"
        );
        std::fs::write(&path, content).unwrap();

        // discover (header-only)
        let r = legacy::discover("20240101_120000", &path).unwrap();
        assert_eq!(r.title.as_deref(), Some("legacy chat"));
        assert_eq!(r.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/old"));
        assert_eq!(
            r.message_count, None,
            "discovery reads metadata without reducing messages"
        );
        assert!(r.created_at.is_some());

        // full parse
        let s = parse_legacy("20240101_120000", &path).unwrap();
        assert_eq!(s.title.as_deref(), Some("legacy chat"));
        assert_eq!(s.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/old"));
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages.iter().filter(|m| m.is_turn()).count(), 2);
        assert_eq!(
            kinds(&s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
            ]
        );
        assert!(matches!(&s.messages[1].content[1], Block::ToolUse { name, .. } if name == "developer__shell"));
        // tool response reclassified + name paired
        assert_eq!(s.messages[2].role, Role::Tool);
        assert!(
            matches!(&s.messages[2].content[0], Block::ToolResult { tool_name, content, .. }
            if tool_name.as_deref() == Some("developer__shell") && content.matches_str("/home/u/old") == Some(true))
        );
    }

    /// Legacy discovery reads a whole file; an unchanged file is not read again, a new legacy
    /// session is read alone.
    #[test]
    fn unchanged_legacy_files_are_not_rescanned() -> Result<()> {
        let world = crate::test_world("goose-legacy-cache");
        let dir = world.base.join("sessions");
        std::fs::create_dir_all(&dir)?;
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let write = |name: &str| {
            std::fs::write(
                dir.join(format!("{name}.jsonl")),
                "{\"description\":\"legacy\",\"working_dir\":\"/w\"}\n{\"role\":\"user\",\"created\":1,\"content\":[]}\n",
            )
        };
        for i in 0..4 {
            write(&format!("s{i}"))?;
        }
        let scanned = || {
            let mut all = legacy::SCANNED.lock().unwrap();
            let mine = all.iter().filter(|p| p.starts_with(&dir)).count();
            all.retain(|p| !p.starts_with(&dir));
            mine
        };
        let adapter = Goose::with_sessions_dir(&dir);
        let first = adapter.discover()?;
        assert_eq!((first.refs.len(), scanned()), (4, 4));
        let again = adapter.discover()?;
        assert_eq!((again.refs.len(), scanned()), (4, 0), "unchanged files are not reread");
        assert_eq!(serde_json::to_value(&first.refs)?, serde_json::to_value(&again.refs)?);
        write("s4")?;
        assert_eq!((adapter.discover()?.refs.len(), scanned()), (5, 1));
        Ok(())
    }

    #[test]
    fn legacy_without_metadata_header_is_tolerant() {
        // A header that's actually a message (no description) — we still read messages, no panic.
        let (_world, dir) = tempdir();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"role\":\"user\",\"created\":1,\"content\":[{\"type\":\"text\",\"text\":\"only msg\"}]}\n",
        )
        .unwrap();
        let s = parse_legacy("s", &path).unwrap();
        // first line consumed as header; since it has no `working_dir`/`description`, fields are None.
        assert!(s.title.is_none());
        assert_eq!(s.messages.len(), 0);
    }

    #[test]
    fn redacted_thinking_maps() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", [])
            .unwrap();
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','assistant','[{\"type\":\"redactedThinking\",\"data\":\"BLOB\"}]',1)",
            [],
        ).unwrap();
        let s = parse_db(&c, &sref(&c, "s1")).unwrap();
        assert!(
            matches!(&s.messages[0].content[0], Block::Thinking { redacted, encrypted, .. } if *redacted && encrypted.as_deref() == Some("BLOB"))
        );
    }

    #[test]
    fn empty_dir_discovers_nothing() {
        let g = Goose { dir: None };
        assert!(g.discover().unwrap().is_empty());
        assert!(g.roots().is_empty());
    }

    fn projected_session(conn: &Connection, r: &SessionRef, opts: &Demand) -> Session {
        let mut sink = crate::stream::CollectSink::default();
        let mut session = stream_db_opts(conn, r, opts, &mut sink).unwrap();
        session.messages = sink.messages;
        session.materialize().unwrap();
        session
    }

    // Demand explicitly reserves native sidecars and structured result details for full
    // extra demand. Lean output must retain every first-class field and ordinary content block.
    fn without_rich_message_fields(mut session: Session) -> Value {
        for message in &mut session.messages {
            message.extra.remove(Harness::Goose.as_str());
            for block in &mut message.content {
                if let Block::ToolResult { details, .. } = block {
                    *details = None;
                }
            }
        }
        serde_json::to_value(session).unwrap()
    }

    #[test]
    fn long_text_stays_in_the_cell_and_lean_demands_drop_only_rich_fields() {
        let (_world, c) = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('large','/scratch')", [])
            .unwrap();
        let giant = "é\n\"\\".repeat(20_000);
        let content = serde_json::json!([
            {"type":"text","text": giant},
            {"type":"toolRequest","id":"call","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":giant}}}},
            {"type":"toolResponse","id":"call","toolResult":{"status":"success","value":{"content":[{"type":"text","text":giant},{"type":"audio","mimeType":"audio/wav"}],"structuredContent":{"stdout":"out"}}}},
            {"type":"systemNotification","msg":giant},
            {"type":"unknown","discarded":giant}
        ]);
        let metadata = serde_json::json!({"userVisible":false,"turnContext":true,"usage":{"inputTokens":9,"cost":0.25,"discarded":giant},"inference":{"provider":"p","resolvedModel":"m"},"operations":[giant]});
        c.execute("INSERT INTO messages (session_id,role,content_json,created_timestamp,metadata_json) VALUES ('large','user',?1,1,?2)", rusqlite::params![content.to_string(),metadata.to_string()]).unwrap();
        let r = sref(&c, "large");
        let mut sink = crate::stream::CollectSink::default();
        stream_db_opts(&c, &r, &Demand::LEAN.with_spans(), &mut sink).unwrap();
        let m = &sink.messages[0];
        assert!(matches!(
            &m.content[0],
            Block::Text {
                text: crate::lazy::Text::Source(_),
                ..
            }
        ));
        assert!(matches!(
            &m.content[1],
            Block::ToolUse {
                input: crate::json::Json::Source(_),
                ..
            }
        ));
        assert!(matches!(
            &m.content[2],
            Block::ToolResult {
                content: crate::lazy::Text::Source(_),
                details: None,
                ..
            }
        ));
        assert_eq!((m.kind, m.model.as_deref()), (MessageKind::InjectedContext, Some("m")));
        assert_eq!(
            m.usage.as_ref().map(|u| (u.input_tokens, u.cost_usd)),
            Some((Some(9), Some(0.25)))
        );
        assert!(m.extra.is_empty());
        let lazy = projected_session(&c, &r, &Demand::LEAN.with_spans());
        let full = projected_session(&c, &r, &Demand::RICH);
        assert_eq!(
            serde_json::to_value(lazy).unwrap(),
            without_rich_message_fields(full.clone())
        );
        let tool = &full.messages[0].content[2];
        assert!(
            matches!(tool, Block::ToolResult { content, details: Some(_), .. } if content.matches_str(&format!("{giant}\n[audio: audio/wav]")) == Some(true))
        );
        assert_eq!(bag(&full.messages[0])["metadata"]["operations"][0], giant.as_str());
    }

    #[test]
    fn checked_projection_rejects_sidecar_creation_at_early_stop() {
        let (_world, dir) = tempdir();
        let path = dir.join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO sessions (id,description,working_dir) VALUES ('guard','guard','/scratch')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id,role,content_json,created_timestamp) VALUES ('guard','user',?1,1)",
            [serde_json::json!([{"type":"text","text":"x".repeat(crate::lazy::INLINE_MAX + 1)}]).to_string()],
        )
        .unwrap();
        struct Stop {
            wal: PathBuf,
            message: Option<Message>,
        }
        impl MessageSink for Stop {
            fn message(&mut self, message: Message) -> Flow {
                self.message = Some(message);
                std::fs::write(&self.wal, b"changed sidecar").unwrap();
                Flow::Stop
            }
        }
        let mut stop = Stop {
            wal: dir.join("sessions.db-wal"),
            message: None,
        };
        let r = SessionRef::new("guard", Harness::Goose, path);
        assert!(stream_db_opts(&conn, &r, &Demand::LEAN.with_spans(), &mut stop).is_err());
        let mut message = stop.message.unwrap();
        assert!(message.materialize(&crate::lazy::Resolver::new(None)).is_err());
        drop(conn);
    }

    #[test]
    fn checked_projection_invalid_body_retains_compaction_history() {
        let (_world, dir) = tempdir();
        let path = dir.join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO sessions (id,description,working_dir) VALUES ('history','history','/scratch')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO messages (session_id,role,content_json,created_timestamp,metadata_json) VALUES ('history','user','not JSON',1,'{\"agentVisible\":false}')", []).unwrap();
        conn.execute("INSERT INTO messages (session_id,role,content_json,created_timestamp,metadata_json) VALUES ('history','user','[{\"type\":\"text\",\"text\":\"summary\"}]',2,'{\"userVisible\":false,\"agentVisible\":true}')", []).unwrap();
        let r = SessionRef::new("history", Harness::Goose, path);
        let full = projected_session(&conn, &r, &Demand::RICH);
        assert_eq!(full.messages.len(), 2);
        assert_eq!(full.messages[1].kind, MessageKind::CompactionSummary);
        assert_eq!(
            serde_json::to_value(projected_session(&conn, &r, &Demand::LEAN.with_spans())).unwrap(),
            without_rich_message_fields(projected_session(&conn, &r, &Demand::RICH))
        );
        assert_eq!(
            kinds(&projected_session(&conn, &r, &Demand::FACTS.with_spans())),
            kinds(&full)
        );
        drop(conn);
    }

    /// A fresh directory per call: a clock stamp collides between parallel tests (macOS clocks
    /// tick in microseconds), and two tests then share one `sessions.db`.
    fn tempdir() -> (cv_testkit::World, PathBuf) {
        let world = cv_testkit::World::new("goose");
        let dir = world.base.clone();
        (world, dir)
    }
}
