//! The event catalog: what a session **did**, not just what was said.
//!
//! The FTS index answers "which session mentioned X"; this module answers "which session *touched*
//! file X / ran command Y / hit error Z". Every [`Block::ToolUse`]/[`Block::ToolResult`] streaming
//! past is distilled into a tiny normalized [`Event`] row (kind + tool + target), persisted into the
//! same SQLite catalog as [`crate::catalog`]'s `sessions` table. That makes `cv touched <path>`
//! an indexed lookup instead of a corpus re-parse, and is the substrate for a future `cv blame`
//! (line of code → the session whose reasoning wrote it).
//!
//! Tool names differ wildly across harnesses (claude `Edit`/`Read`/`Bash`, codex
//! `shell`/`apply_patch`/`local_shell`, gemini `run_shell_command`/`write_file`/`replace`, …), so
//! classification is heuristic: a file-path key in the input plus a read/write-flavored name, a
//! command key, or a recognizable `apply_patch` body whose per-file headers are mined for targets.
//! Unrecognized tools degrade to `kind='tool'` rather than being dropped.
//!
//! Memory discipline (the project's core value): the sink owns only the small classified rows.
//! Tool *inputs* are already-small `serde_json::Value`s; tool *result* content is never
//! materialized — under [`ParseOptions::lazy()`](crate::stream::ParseOptions::lazy) a large result
//! is an unresolved [`Span`](crate::lazy::Span) and the error `detail` is simply skipped for it.
//! Like [`crate::catalog`], the persistence side is best-effort: SQLite errors degrade to empty
//! results / no-ops, never failing the caller.

use crate::ir::{truncate, Block, Harness, Message, Session, SessionRef};
use crate::stream::{Flow, MessageSink};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Cap (chars) on a command-string target. File-path targets are never truncated.
const TARGET_MAX: usize = 300;
/// Cap (chars) on the `detail` context (e.g. the head of an error result).
const DETAIL_MAX: usize = 200;

/// Name fragments that mark a file-keyed tool as *writing* its target.
const WRITE_HINTS: &[&str] = &["edit", "write", "patch", "notebook", "replace", "create", "save"];
/// Name fragments that mark a file-keyed tool as *reading* its target.
const READ_HINTS: &[&str] = &["read", "grep", "glob", "search", "cat", "view", "list"];

/// Input keys (across harnesses) whose string value is the tool's file target.
const FILE_KEYS: &[&str] = &[
    "file_path",
    "absolute_path",
    "filePath",
    "notebook_path",
    "target_file",
    "path",
];

/// One normalized thing a session did, extracted from a message's tool blocks.
///
/// `kind` is one of: `file_edit`, `file_read`, `command`, `tool` (other/unknown), `error`
/// (a tool result flagged as failed). `target` is an absolute file path for file ops (relative
/// paths resolved against the session cwd), the command head for commands, `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Index of the message (in transcript stream order) this event came from.
    pub msg_idx: usize,
    /// Unix seconds of the message timestamp, when the harness recorded one.
    pub ts: Option<i64>,
    pub kind: &'static str,
    /// The raw tool name as the harness recorded it (`None` for an error result whose tool is unknown).
    pub tool: Option<String>,
    pub target: Option<String>,
    /// Small context, capped at [`DETAIL_MAX`] chars (e.g. the head of an error result).
    pub detail: Option<String>,
}

/// A [`MessageSink`] that classifies each message's tool blocks into [`Event`]s as it streams past.
/// Holds only the (small) accumulated rows — message content is never retained.
pub struct EventSink {
    cwd: Option<PathBuf>,
    msg_idx: usize,
    events: Vec<Event>,
}

impl EventSink {
    /// `cwd` is the session working directory relative file paths resolve against (usually the
    /// [`SessionRef`]'s); when `None`, the adapter's `meta` cwd is used if it arrives.
    pub fn new(cwd: Option<PathBuf>) -> Self {
        EventSink {
            cwd,
            msg_idx: 0,
            events: Vec::new(),
        }
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn into_events(self) -> Vec<Event> {
        self.events
    }
}

impl MessageSink for EventSink {
    fn meta(&mut self, s: &Session) {
        if self.cwd.is_none() {
            self.cwd = s.cwd.clone();
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        extract_into(&m, self.msg_idx, self.cwd.as_deref(), &mut self.events);
        self.msg_idx += 1;
        Flow::Continue
    }
}

/// Classify one message's tool blocks into events. `msg_idx` is its index in the transcript;
/// relative file targets resolve against `cwd`. The standalone form of what [`EventSink`] does
/// per streamed message.
pub fn extract(m: &Message, msg_idx: usize, cwd: Option<&Path>) -> Vec<Event> {
    let mut out = Vec::new();
    extract_into(m, msg_idx, cwd, &mut out);
    out
}

fn extract_into(m: &Message, msg_idx: usize, cwd: Option<&Path>, out: &mut Vec<Event>) {
    let ts = m.timestamp.map(|t| t.timestamp());
    for b in &m.content {
        match b {
            Block::ToolUse { name, input, .. } => {
                for (kind, target) in classify_tool(name, input, cwd) {
                    out.push(Event {
                        msg_idx,
                        ts,
                        kind,
                        tool: Some(name.clone()),
                        target,
                        detail: None,
                    });
                }
            }
            Block::ToolResult {
                content,
                is_error: true,
                tool_name,
                ..
            } => {
                // Never materialize a lazy span just for a 200-char detail — an unresolved span
                // (a giant failed-command dump) simply contributes no detail.
                let detail = content
                    .inline_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| truncate(s, DETAIL_MAX));
                out.push(Event {
                    msg_idx,
                    ts,
                    kind: "error",
                    tool: tool_name.clone(),
                    target: None,
                    detail,
                });
            }
            _ => {}
        }
    }
}

/// Heuristic cross-harness classification of one tool invocation into `(kind, target)` rows.
/// Usually one row; an `apply_patch` body yields one `file_edit` per patched file.
fn classify_tool(name: &str, input: &Value, cwd: Option<&Path>) -> Vec<(&'static str, Option<String>)> {
    let lname = name.to_ascii_lowercase();

    // apply_patch-style tools: the input *is* (or carries) a patch body whose per-file headers
    // are the real targets — much higher value for blame than the opaque tool row.
    if lname.contains("patch") {
        if let Some(body) = patch_body(input) {
            let files = patch_targets(body);
            if !files.is_empty() {
                return files
                    .into_iter()
                    .map(|f| ("file_edit", Some(absolutize(&f, cwd))))
                    .collect();
            }
        }
    }

    // A file-path key in the input: read/write flavor comes from the tool name; an ambiguous
    // name still records the touch as kind 'tool' with the path as target.
    if let Some(p) = file_key(input) {
        let kind = if WRITE_HINTS.iter().any(|h| lname.contains(h)) {
            "file_edit"
        } else if READ_HINTS.iter().any(|h| lname.contains(h)) {
            "file_read"
        } else {
            "tool"
        };
        return vec![(kind, Some(absolutize(p, cwd)))];
    }

    // A command key (claude Bash `command`, codex shell `command: [argv…]`, local_shell
    // `action.command`, gemini run_shell_command `command`, …).
    if let Some(cmd) = command_of(input) {
        let mut rows = vec![("command", Some(truncate(&cmd, TARGET_MAX)))];
        // codex commonly routes apply_patch *through* the shell tool; mine the patch body riding
        // in the command string so the per-file edits aren't lost behind a 'command' row.
        if cmd.contains("*** Begin Patch") || cmd.trim_start().starts_with("apply_patch") {
            rows.extend(
                patch_targets(&cmd)
                    .into_iter()
                    .map(|f| ("file_edit", Some(absolutize(&f, cwd)))),
            );
        }
        return rows;
    }

    vec![("tool", None)]
}

/// The first file-path-like string in the tool input, by key precedence.
fn file_key(input: &Value) -> Option<&str> {
    FILE_KEYS
        .iter()
        .find_map(|k| input.get(k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// The command string of a shell-ish tool input: a plain string, an argv array joined with
/// spaces, or codex local_shell's nested `action.command`.
fn command_of(input: &Value) -> Option<String> {
    let v = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .or_else(|| input.get("script"))
        .or_else(|| input.get("action").and_then(|a| a.get("command")))?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let joined: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            (!joined.is_empty()).then(|| joined.join(" "))
        }
        _ => None,
    }
}

/// The patch text of an apply_patch-style input: the input itself when it's a plain string
/// (codex `custom_tool_call`), or an obvious carrier field.
fn patch_body(input: &Value) -> Option<&str> {
    input
        .as_str()
        .or_else(|| input.get("input").and_then(Value::as_str))
        .or_else(|| input.get("patch").and_then(Value::as_str))
}

/// File paths named by a patch body. Primary: codex apply_patch headers
/// (`*** Update File: <path>` / `*** Add File:` / `*** Delete File:` / `*** Move to:`).
/// Fallback: unified-diff `+++ <path>` headers — but only when `--- ` headers are present too,
/// so an *added line* whose content begins with `++ ` can't fake a target.
fn patch_targets(patch: &str) -> Vec<String> {
    fn push(out: &mut Vec<String>, f: &str) {
        let f = f.trim();
        if !f.is_empty() && f != "/dev/null" && !out.iter().any(|x| x == f) {
            out.push(f.to_string());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let t = line.trim();
        for pre in ["*** Update File:", "*** Add File:", "*** Delete File:", "*** Move to:"] {
            if let Some(rest) = t.strip_prefix(pre) {
                push(&mut out, rest);
            }
        }
    }
    if out.is_empty() && patch.lines().any(|l| l.starts_with("--- ")) {
        for line in patch.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                push(&mut out, rest.trim().trim_start_matches("b/"));
            }
        }
    }
    out
}

/// Lexically absolutize `p` against the session cwd. Already-absolute paths pass through; with no
/// cwd the path is stored as given (still suffix-matchable by `cv touched`).
fn absolutize(p: &str, cwd: Option<&Path>) -> String {
    if Path::new(p).is_absolute() {
        return p.to_string();
    }
    let trimmed = p.trim_start_matches("./");
    match cwd {
        Some(c) => c.join(trimmed).to_string_lossy().into_owned(),
        None => trimmed.to_string(),
    }
}

/// File mtime in integer nanoseconds (0 when unreadable) — half of the `(mtime, size)`
/// incremental-ingest skip key, identical semantics to the FTS indexer's.
pub fn file_mtime_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// File size in bytes (0 when unreadable) — the other half of the `(mtime, size)` ingest key.
pub fn file_size(path: &Path) -> i64 {
    std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}

/// Whether this session source can use file **size** as its primary freshness key — the policy
/// shared by every incremental store (the FTS index, this event catalog, the message-offsets
/// store). These are the append-only JSONL transcript logs where an mtime-only check caused the
/// live re-index spiral: a pure mtime bump (touch / rsync / restore) does not mean new content,
/// while a real append grows the file. Other stores (SQLite DBs, JSON arrays/objects, and metadata
/// files) can rewrite in place at the same byte length, so they must keep mtime in the freshness
/// predicate.
pub fn size_primary_freshness(r: &SessionRef) -> bool {
    let is_jsonl = r
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"));
    is_jsonl
        && matches!(
            r.harness,
            Harness::Claude | Harness::Codex | Harness::Gemini | Harness::Qwen | Harness::Kimi | Harness::OpenClaw
        )
}

/// Provenance for a sub-agent event source: which parent session, which workflow run (if any), and
/// which agent inside it the events came from. A top-level session has none of these (`None`).
///
/// Carried on the `event_sessions` row so a stored sub-agent's events can be attributed back to the
/// orchestrator that spawned it (`cv touched` / `cv events` can then say *which* agent of *which*
/// workflow of *which* parent edited a file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The top-level session id this sub-agent was (transitively) spawned by.
    pub parent_id: Option<String>,
    /// The sub-agent's own agent id (`agentId`, the `agent-` prefix stripped).
    pub agent_id: Option<String>,
    /// The workflow run id (`wf_…`) this agent belonged to, or `None` for a directly-spawned agent.
    pub workflow: Option<String>,
}

impl Provenance {
    /// True when this carries no sub-agent attribution — i.e. an ordinary top-level session.
    pub fn is_empty(&self) -> bool {
        self.parent_id.is_none() && self.agent_id.is_none() && self.workflow.is_none()
    }
}

/// One persisted event row, as read back from the catalog.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub harness: String,
    pub session_id: String,
    pub msg_idx: i64,
    pub ts: Option<i64>,
    pub kind: String,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub detail: Option<String>,
    /// Sub-agent attribution (all `None` for a top-level session).
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub workflow: Option<String>,
}

/// One session in a `cv touched <path>` result: who touched the file, how, and when last.
#[derive(Debug, Clone)]
pub struct TouchedSession {
    pub harness: String,
    pub session_id: String,
    /// Title from the `sessions` catalog table, when that row exists.
    pub title: Option<String>,
    pub edits: i64,
    pub reads: i64,
    /// Latest event timestamp on the file in this session (unix secs).
    pub last_ts: Option<i64>,
    /// Sub-agent attribution (all `None` for a top-level session): the parent session, the
    /// workflow run, and the agent inside it whose events touched the file.
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub workflow: Option<String>,
}

/// One `file_edit` event joined with its session's catalog metadata — the raw material for
/// `cv blame`'s commit↔session correlation (the timestamps and cwd are what the scoring runs on).
#[derive(Debug, Clone)]
pub struct EditEvent {
    pub harness: String,
    pub session_id: String,
    pub msg_idx: i64,
    /// Unix seconds of the *message* that made the edit (`None` when the harness records none).
    pub ts: Option<i64>,
    /// The target path exactly as the event recorded it (may be from another checkout).
    pub target: Option<String>,
    /// Title from the `sessions` catalog table, when that row exists.
    pub title: Option<String>,
    /// Session working directory from the `sessions` catalog (string form).
    pub cwd: Option<String>,
    /// Session created/updated unix seconds — the weak-match window when `ts` is `None`.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[cfg(feature = "sqlite")]
mod db {
    use super::*;
    use anyhow::Result;
    use rusqlite::{params, Connection};

    /// Event-tables schema generation, stamped into the catalog's `PRAGMA user_version`.
    ///
    /// v1 (0.9.10) stored `(harness TEXT, session_id TEXT, …, target TEXT)` per event row plus a
    /// dedup expression index — measured at ~3× the data size in indexes alone. v2 interns session
    /// identity and targets into side tables and carries integers per row. The catalog is a pure
    /// cache, so "migration" is version-stamp + drop + lazy reingest (dropping `event_sync` makes
    /// every session look stale; the next `cv index` — or any on-the-spot `cv events <id>` —
    /// rebuilds), exactly like the tantivy schema self-heal.
    /// v3 (0.9.13) adds sub-agent provenance columns (`parent_id`, `agent_id`, `workflow`) to
    /// `event_sessions`, so a folded-in sub-agent transcript's events can be attributed to the
    /// orchestrator that spawned it. Same self-heal as before: bumping the version drops the event
    /// tables + `event_sync`, so everything reads stale and the next `cv index` reingests on the
    /// new schema.
    const EVENTS_SCHEMA_VERSION: i64 = 3;

    /// Drop pre-v2 event tables (their indexes go with them) and stamp the version.
    ///
    /// Two triggers, both checked on every open so the schema self-heals:
    /// * `user_version < 2` — a v1/unversioned catalog (the `sessions` table never changed shape
    ///   and is versionless `CREATE IF NOT EXISTS`; the event tables own the version stamp);
    /// * a v1-*shaped* `events` table (it has a `harness` column) even under a v2 stamp — an old
    ///   0.9.10 binary re-created v1 tables in a v2 catalog. (The common old-binary case is
    ///   harmless without this: v2 drops `idx_events_dedup`, so the old binary's
    ///   `CREATE UNIQUE INDEX … ON events(harness,…)` DDL fails against the v2 table and it
    ///   degrades to events-disabled — its designed no-catalog path, no corruption possible. The
    ///   shape check covers the race where an old binary lands v1 tables *first*.)
    fn migrate(conn: &Connection) -> Option<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).ok()?;
        let v1_shaped: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('events') WHERE name='harness')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if version >= EVENTS_SCHEMA_VERSION && !v1_shaped {
            return Some(());
        }
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS events;
             DROP TABLE IF EXISTS event_sync;
             DROP TABLE IF EXISTS event_sessions;
             DROP TABLE IF EXISTS event_targets;
             PRAGMA user_version = {EVENTS_SCHEMA_VERSION};"
        ))
        .ok()?;
        // The drop frees most of the file's pages but SQLite never returns them to the OS on its
        // own — reclaim once, opportunistically (a busy db just keeps its free pages for reuse).
        let _ = conn.execute_batch("VACUUM;");
        Some(())
    }

    /// Open the shared catalog db and ensure the (v2) event tables exist. `None` on any error.
    ///
    /// Schema notes:
    /// * `event_sessions` is a dedicated intern table rather than a reuse of the `sessions`
    ///   rowid: `sessions` rows are `INSERT OR REPLACE`d on every sync, which deletes+reinserts
    ///   and so *changes* the rowid — events keyed on it would silently orphan. Its PK is also
    ///   `(harness, id, path)`, so one logical session at two paths would have two rowids.
    ///   `UNIQUE(harness, session_id)` here is stable across syncs and tiny (~2.4k rows).
    /// * `event_targets` interns the distinct target strings (~54k of ~145k events); event rows
    ///   carry a nullable integer. Re-ingest deletes a session's event rows but leaves its
    ///   interned targets — orphans are unreferenced cache rows, reclaimed only by the next
    ///   schema migration. Suffix matching scans `event_targets` instead of every event row.
    /// * No dedup index: [`record`] is DELETE-by-session + batch INSERT in one transaction, so
    ///   cross-session duplicates are impossible by construction, and intra-batch duplicates are
    ///   deduped in memory there.
    /// * `idx_events_session` serves [`events_for`] and `record`'s delete; `idx_events_target`
    ///   joins event rows back from a matched target. v1's `idx_events_kind_ts` indexed
    ///   `(kind, ts)` which no query ever drove with a kind-prefix lookup — dropped.
    fn open() -> Option<Connection> {
        let conn = crate::catalog::open_db()?;
        migrate(&conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_sessions(
                 id         INTEGER PRIMARY KEY,
                 harness    TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 parent_id  TEXT,
                 agent_id   TEXT,
                 workflow   TEXT,
                 UNIQUE(harness, session_id)
             );
             CREATE TABLE IF NOT EXISTS event_targets(
                 id     INTEGER PRIMARY KEY,
                 target TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS events(
                 session   INTEGER NOT NULL,
                 msg_idx   INTEGER NOT NULL,
                 ts        INTEGER,
                 kind      TEXT NOT NULL,
                 tool      TEXT,
                 target_id INTEGER,
                 detail    TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_events_session ON events(session);
             CREATE INDEX IF NOT EXISTS idx_events_target ON events(target_id);
             CREATE TABLE IF NOT EXISTS event_sync(
                 harness  TEXT NOT NULL,
                 id       TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 size     INTEGER,
                 PRIMARY KEY(harness, id, path)
             );",
        )
        .ok()?;
        // Lazy migration: a pre-`size` `event_sync` (the CREATE above is a no-op on it) gets the
        // column added in place. Old rows read back a NULL size, which [`sync_fresh`] treats as
        // stale — each session re-ingests once and is stamped with its size; no wholesale drop.
        let has_size: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('event_sync') WHERE name='size')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if !has_size {
            conn.execute_batch("ALTER TABLE event_sync ADD COLUMN size INTEGER;")
                .ok()?;
        }
        Some(conn)
    }

    /// The event-catalog freshness predicate over a stored `(mtime_ns, size)` stamp, mirroring the
    /// FTS indexer's policy ([`size_primary_freshness`]): for append-only transcript logs an
    /// unchanged size means no new content even when mtimes were mass-bumped (touch / rsync /
    /// restore — the whole-corpus re-parse trigger); rewriteable stores must match on mtime too.
    /// A missing stored size (a pre-`size` catalog row) or a 0 size (unreadable file) is never
    /// fresh, so those re-ingest — best-effort-cache semantics: errors mean re-ingest, never skip.
    fn sync_fresh(r: &SessionRef, stored: Option<(i64, Option<i64>)>, mtime_ns: i64, size: i64) -> bool {
        stored.is_some_and(|(stored_mtime, stored_size)| {
            stored_size == Some(size)
                && size != 0
                && (size_primary_freshness(r) || (stored_mtime == mtime_ns && mtime_ns != 0))
        })
    }

    /// Whether `r` needs an event (re)ingest: its file `(mtime, size)` signature differs from the
    /// `event_sync` row (or there is none). `false` when the catalog can't be opened — events
    /// degrade silently rather than re-streaming the corpus into a broken db forever.
    ///
    /// Opens a fresh connection per call — fine for a one-off check (`cv events <id>`), wrong for
    /// a corpus sweep: bulk callers use [`SyncTable`] (one query for the whole skip-table).
    pub fn needs_ingest(r: &SessionRef, mtime_ns: i64) -> bool {
        let Some(conn) = open() else { return false };
        let stored: Option<(i64, Option<i64>)> = conn
            .query_row(
                "SELECT mtime_ns, size FROM event_sync WHERE harness=?1 AND id=?2 AND path=?3",
                params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        !sync_fresh(r, stored, mtime_ns, file_size(&r.path))
    }

    /// The whole `event_sync` skip-table, loaded with **one** connection + query, for bulk passes
    /// that check every discovered session ([`ingest_all`], the FTS indexer's event ride-along).
    /// The per-session [`needs_ingest`] opens a connection — and re-runs the catalog DDL — per
    /// call, which a ~2,400-session sweep pays ~2,400 times.
    ///
    /// Answers exactly like [`needs_ingest`], including the degrade: when the catalog can't be
    /// opened, every check returns `false` (don't re-stream the corpus into a broken db).
    /// `(harness, id, path) → (mtime_ns, size)` — one loaded `event_sync` stamp per session
    /// (size is `None` on rows written before the size column existed).
    type SyncRows = std::collections::HashMap<(String, String, String), (i64, Option<i64>)>;

    pub struct SyncTable {
        /// The loaded skip-table; `None` when the catalog is unavailable.
        rows: Option<SyncRows>,
    }

    impl SyncTable {
        pub fn load() -> SyncTable {
            let load = || -> Option<SyncRows> {
                let conn = open()?;
                let mut stmt = conn
                    .prepare("SELECT harness, id, path, mtime_ns, size FROM event_sync")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ),
                            (row.get::<_, i64>(3)?, row.get::<_, Option<i64>>(4)?),
                        ))
                    })
                    .ok()?;
                Some(rows.flatten().collect())
            };
            SyncTable { rows: load() }
        }

        /// Same answer [`needs_ingest`] would give for `r`, from the preloaded table. Takes the
        /// file `size` the caller already stat'ed (bulk sweeps have it from `file_sig`) instead of
        /// re-stat'ing per row.
        pub fn needs_ingest(&self, r: &SessionRef, mtime_ns: i64, size: i64) -> bool {
            let Some(rows) = &self.rows else { return false };
            let key = (
                r.harness.as_str().to_string(),
                r.id.clone(),
                r.path.to_string_lossy().into_owned(),
            );
            !sync_fresh(r, rows.get(&key).copied(), mtime_ns, size)
        }
    }

    /// Persist `events` as the complete event set for session `r` (one transaction: delete prior
    /// rows, insert, stamp `event_sync` with the `(mtime_ns, size)` signature — captured by the
    /// caller **before** streaming, so content appended mid-parse reads as stale next time).
    /// Idempotent — a re-ingest of an unchanged session rewrites identical rows. Best-effort: any
    /// sqlite error leaves the previous state and returns quietly.
    ///
    /// Dedup invariant (what replaced v1's unique expression index): the DELETE-by-session +
    /// batch-INSERT shape makes cross-session duplicates impossible by construction, and
    /// duplicates *within* this batch — same `(msg_idx, kind, target)`, matching the old index's
    /// `COALESCE(target,'')` semantics — are dropped by the in-memory set below.
    pub fn record(r: &SessionRef, events: &[Event], mtime_ns: i64, size: i64) {
        record_with(r, events, mtime_ns, size, &Provenance::default());
    }

    /// [`record`], additionally stamping sub-agent `prov`enance onto the `event_sessions` row so the
    /// stored events are attributable to the orchestrator that spawned the agent. A top-level
    /// session passes an empty [`Provenance`] (this is exactly what [`record`] does); the columns
    /// stay NULL and existing queries are unaffected.
    pub fn record_with(r: &SessionRef, events: &[Event], mtime_ns: i64, size: i64, prov: &Provenance) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        let harness = r.harness.as_str();
        let _ = tx.execute(
            "INSERT OR IGNORE INTO event_sessions(harness, session_id) VALUES(?1,?2)",
            params![harness, r.id],
        );
        // Stamp provenance on every record (idempotent re-ingest keeps it current); top-level
        // sessions write NULLs, which leaves the existing top-level rows exactly as before.
        let _ = tx.execute(
            "UPDATE event_sessions SET parent_id=?3, agent_id=?4, workflow=?5
             WHERE harness=?1 AND session_id=?2",
            params![harness, r.id, prov.parent_id, prov.agent_id, prov.workflow],
        );
        let Ok(session) = tx.query_row(
            "SELECT id FROM event_sessions WHERE harness=?1 AND session_id=?2",
            params![harness, r.id],
            |row| row.get::<_, i64>(0),
        ) else {
            return;
        };
        let _ = tx.execute("DELETE FROM events WHERE session=?1", params![session]);
        {
            let Ok(mut intern) = tx.prepare("INSERT OR IGNORE INTO event_targets(target) VALUES(?1)") else {
                return;
            };
            let Ok(mut intern_id) = tx.prepare("SELECT id FROM event_targets WHERE target=?1") else {
                return;
            };
            let Ok(mut insert) = tx.prepare(
                "INSERT INTO events(session,msg_idx,ts,kind,tool,target_id,detail)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
            ) else {
                return;
            };
            let mut seen: std::collections::HashSet<(usize, &str, &str)> = std::collections::HashSet::new();
            let mut targets: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
            for e in events {
                if !seen.insert((e.msg_idx, e.kind, e.target.as_deref().unwrap_or(""))) {
                    continue;
                }
                let target_id: Option<i64> = match e.target.as_deref() {
                    None => None,
                    Some(t) => match targets.get(t) {
                        Some(&id) => Some(id),
                        None => {
                            let _ = intern.execute(params![t]);
                            let Ok(id) = intern_id.query_row(params![t], |row| row.get(0)) else {
                                return;
                            };
                            targets.insert(t, id);
                            Some(id)
                        }
                    },
                };
                let _ = insert.execute(params![
                    session,
                    e.msg_idx as i64,
                    e.ts,
                    e.kind,
                    e.tool,
                    target_id,
                    e.detail,
                ]);
            }
        }
        let _ = tx.execute(
            "INSERT OR REPLACE INTO event_sync(harness,id,path,mtime_ns,size) VALUES(?1,?2,?3,?4,?5)",
            params![harness, r.id, r.path.to_string_lossy(), mtime_ns, size],
        );
        let _ = tx.commit();
    }

    /// (Re)ingest events for one session: stream it (lazily — large content stays on disk) through
    /// an [`EventSink`] and persist the rows. Returns the number of events extracted. The
    /// `(mtime, size)` stamp is captured **before** the streamed pass, so anything appended while
    /// we were reading still shows as stale next time.
    pub fn ingest_ref(r: &SessionRef) -> Result<usize> {
        let adapter =
            crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
        let (mtime_ns, size) = crate::offsets::file_sig(&r.path);
        let mut sink = EventSink::new(r.cwd.clone());
        adapter.stream(r, &crate::stream::ParseOptions::lazy(), &mut sink)?;
        let events = sink.into_events();
        record(r, &events, mtime_ns, size);
        Ok(events.len())
    }

    /// (Re)ingest events for one sub-agent transcript, stamping `prov`enance on its catalog row so
    /// the rows are attributable to the orchestrator that spawned it. Same streaming discipline as
    /// [`ingest_ref`] (lazy parse → large content stays on disk); returns the events extracted.
    pub fn ingest_subagent(r: &SessionRef, prov: &Provenance) -> Result<usize> {
        let adapter =
            crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
        let (mtime_ns, size) = crate::offsets::file_sig(&r.path);
        let mut sink = EventSink::new(r.cwd.clone());
        adapter.stream(r, &crate::stream::ParseOptions::lazy(), &mut sink)?;
        let events = sink.into_events();
        record_with(r, &events, mtime_ns, size, prov);
        Ok(events.len())
    }

    /// (Re)ingest events for every discovered session, skipping unchanged ones (by the
    /// `event_sync` `(mtime, size)` signature) unless `force`. The standalone entry point for users who don't run the tantivy
    /// indexer (which otherwise ingests events as a ride-along). Returns sessions (re)ingested.
    ///
    /// With `subagents`, also fold each top-level Claude session's whole sub-agent **forest**
    /// (`crate::subagent_tree_of`) into the catalog: every Task/Workflow agent's transcript is
    /// streamed through the same `EventSink` and stored with provenance (parent / workflow / agent),
    /// so `cv touched`/`cv events` see what the agents did — not just the orchestrator. Off by
    /// default (the forest can be hundreds of transcripts / ~900 MB).
    pub fn ingest_all(force: bool, subagents: bool) -> Result<usize> {
        let mut n = 0usize;
        let sync = SyncTable::load();
        for r in crate::discover_all() {
            let (mtime_ns, size) = crate::offsets::file_sig(&r.path);
            let top_stale = sync.needs_ingest(&r, mtime_ns, size);
            if force || top_stale {
                match ingest_ref(&r) {
                    Ok(_) => n += 1,
                    Err(e) => {
                        eprintln!("cv: event ingest failed for {} ({}): {e:#}", r.id, r.harness)
                    }
                }
            }
            if subagents {
                n += ingest_forest(&r, &sync, force);
            }
        }
        Ok(n)
    }

    /// Fold the sub-agent forest of one top-level session into the catalog, skipping unchanged
    /// transcripts (by the `event_sync` `(mtime, size)` signature) unless `force`. Best-effort per
    /// agent: an unreadable sub-agent transcript is skipped, not fatal. Returns the number of
    /// agents (re)ingested.
    fn ingest_forest(parent: &SessionRef, sync: &SyncTable, force: bool) -> usize {
        let mut n = 0usize;
        for sub in crate::subagent_tree_of(parent) {
            let sr = &sub.session;
            let (mtime_ns, size) = crate::offsets::file_sig(&sr.path);
            if !force && !sync.needs_ingest(sr, mtime_ns, size) {
                continue;
            }
            let prov = Provenance {
                parent_id: Some(parent.id.clone()),
                agent_id: Some(sub.agent_id().to_string()),
                workflow: sub.workflow.clone(),
            };
            match ingest_subagent(sr, &prov) {
                Ok(_) => n += 1,
                Err(_) => continue,
            }
        }
        n
    }

    /// All persisted events of one session in transcript order, optionally filtered by kind.
    /// Empty on any error or an un-ingested session.
    pub fn events_for(harness: &str, session_id: &str, kind: Option<&str>) -> Vec<EventRow> {
        let Some(conn) = open() else { return Vec::new() };
        let Ok(mut stmt) = conn.prepare(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, e.kind, e.tool, t.target, e.detail,
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN event_targets t ON t.id = e.target_id
             WHERE es.harness=?1 AND es.session_id=?2 AND (?3 IS NULL OR e.kind=?3)
             ORDER BY e.msg_idx, e.kind",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![harness, session_id, kind], |row| {
            Ok(EventRow {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                msg_idx: row.get(2)?,
                ts: row.get(3)?,
                kind: row.get(4)?,
                tool: row.get(5)?,
                target: row.get(6)?,
                detail: row.get(7)?,
                parent_id: row.get(8)?,
                agent_id: row.get(9)?,
                workflow: row.get(10)?,
            })
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// THE query: sessions whose file events touched `path`, newest activity first.
    ///
    /// `path` is lexically absolutized against the current directory, and *also* matched by
    /// suffix (`…/<path>`), so `cv touched src/ir.rs` finds `/repo/src/ir.rs` from anywhere.
    /// `edits_only` keeps only sessions with at least one `file_edit`. Titles come from a join
    /// against the `sessions` catalog. Empty on any error or a cold catalog.
    ///
    /// The leading-`%` LIKE can never use an index; since v2 it scans `event_targets` (the
    /// distinct targets, ~⅓ of the event rows) and joins matches back through `idx_events_target`
    /// instead of scanning every event row.
    pub fn sessions_touching(path: &str, edits_only: bool) -> Vec<TouchedSession> {
        let Some(conn) = open() else { return Vec::new() };
        let abs = absolutize(path, std::env::current_dir().ok().as_deref());
        // Escape LIKE metacharacters in the user's path, then match any target ending in /<path>.
        let escaped = path
            .trim_start_matches("./")
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let suffix = if escaped.starts_with('/') {
            format!("%{escaped}")
        } else {
            format!("%/{escaped}")
        };
        let having = if edits_only { "HAVING edits > 0" } else { "" };
        // The IN-subquery (not a join) pins the plan: one scan over the distinct targets, then
        // matching event rows through idx_events_target — never a scan of all events.
        let sql = format!(
            "SELECT es.harness, es.session_id,
                    SUM(e.kind='file_edit') AS edits,
                    SUM(e.kind='file_read') AS reads,
                    MAX(e.ts) AS last_ts,
                    MAX(s.title) AS title,
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN sessions s ON s.harness = es.harness AND s.id = es.session_id
             WHERE e.kind IN ('file_edit','file_read')
               AND e.target_id IN (SELECT id FROM event_targets
                                   WHERE target = ?1 OR target LIKE ?2 ESCAPE '\\')
             GROUP BY es.harness, es.session_id
             {having}
             ORDER BY last_ts DESC, es.session_id"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![abs, suffix], |row| {
            Ok(TouchedSession {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                edits: row.get(2)?,
                reads: row.get(3)?,
                last_ts: row.get(4)?,
                title: row.get(5)?,
                parent_id: row.get(6)?,
                agent_id: row.get(7)?,
                workflow: row.get(8)?,
            })
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Every `file_edit` event on one file, with session metadata riding along, oldest first.
    ///
    /// Matched two ways, mirroring [`sessions_touching`]: exactly by `abs` (the file's absolute
    /// path in the *current* checkout) and by repo-relative suffix `rel` (target equals `rel` or
    /// ends with `/<rel>`), so edits recorded by a session whose checkout lived at a different
    /// path still surface. The LEFT JOIN is grouped per event row because a session may be
    /// cataloged at more than one path. Empty on any error or a cold catalog.
    pub fn edits_touching_file(abs: &str, rel: &str) -> Vec<EditEvent> {
        let Some(conn) = open() else { return Vec::new() };
        let escaped = rel.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let suffix = format!("%/{escaped}");
        // IN-subquery for the same plan-pinning reason as [`sessions_touching`]; the extra join
        // back to event_targets only resolves the matched rows' target strings by rowid.
        let Ok(mut stmt) = conn.prepare(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, t.target,
                    MAX(s.title), MAX(s.cwd), MAX(s.created_at), MAX(s.updated_at)
             FROM events e
             JOIN event_targets t ON t.id = e.target_id
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN sessions s ON s.harness = es.harness AND s.id = es.session_id
             WHERE e.kind = 'file_edit'
               AND e.target_id IN (SELECT id FROM event_targets
                                   WHERE target = ?1 OR target = ?2 OR target LIKE ?3 ESCAPE '\\')
             GROUP BY es.harness, es.session_id, e.msg_idx, e.ts, t.target
             ORDER BY e.ts, e.msg_idx",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![abs, rel, suffix], |row| {
            Ok(EditEvent {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                msg_idx: row.get(2)?,
                ts: row.get(3)?,
                target: row.get(4)?,
                title: row.get(5)?,
                cwd: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Whether *any* events have ever been ingested — lets callers distinguish "no sessions
    /// touched this file" from "nobody ran `cv index` yet" and hint accordingly.
    pub fn catalog_has_events() -> bool {
        let Some(conn) = open() else { return false };
        conn.query_row("SELECT 1 FROM events LIMIT 1", [], |_| Ok(())).is_ok()
    }
}

#[cfg(feature = "sqlite")]
pub use db::{
    catalog_has_events, edits_touching_file, events_for, ingest_all, ingest_ref, ingest_subagent, needs_ingest, record,
    record_with, sessions_touching, SyncTable,
};

/// No-op fallbacks without sqlite: extraction still works (the sink is pure), persistence and
/// queries degrade to nothing — mirroring [`crate::catalog`].
#[cfg(not(feature = "sqlite"))]
mod db_stub {
    use super::*;
    use anyhow::Result;

    pub fn needs_ingest(_r: &SessionRef, _mtime_ns: i64) -> bool {
        false
    }
    /// No-op twin of the sqlite [`SyncTable`]: nothing is ever stale.
    pub struct SyncTable;
    impl SyncTable {
        pub fn load() -> SyncTable {
            SyncTable
        }
        pub fn needs_ingest(&self, _r: &SessionRef, _mtime_ns: i64, _size: i64) -> bool {
            false
        }
    }
    pub fn record(_r: &SessionRef, _events: &[Event], _mtime_ns: i64, _size: i64) {}
    pub fn record_with(_r: &SessionRef, _events: &[Event], _mtime_ns: i64, _size: i64, _prov: &Provenance) {}
    pub fn ingest_ref(_r: &SessionRef) -> Result<usize> {
        Ok(0)
    }
    pub fn ingest_subagent(_r: &SessionRef, _prov: &Provenance) -> Result<usize> {
        Ok(0)
    }
    pub fn ingest_all(_force: bool, _subagents: bool) -> Result<usize> {
        Ok(0)
    }
    pub fn events_for(_harness: &str, _session_id: &str, _kind: Option<&str>) -> Vec<EventRow> {
        Vec::new()
    }
    pub fn sessions_touching(_path: &str, _edits_only: bool) -> Vec<TouchedSession> {
        Vec::new()
    }
    pub fn edits_touching_file(_abs: &str, _rel: &str) -> Vec<EditEvent> {
        Vec::new()
    }
    pub fn catalog_has_events() -> bool {
        false
    }
}
#[cfg(not(feature = "sqlite"))]
pub use db_stub::{
    catalog_has_events, edits_touching_file, events_for, ingest_all, ingest_ref, ingest_subagent, needs_ingest, record,
    record_with, sessions_touching, SyncTable,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role};
    use serde_json::json;

    fn msg(blocks: Vec<Block>) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content = blocks;
        m
    }

    fn tool_use(name: &str, input: Value) -> Block {
        Block::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input,
            namespace: None,
        }
    }

    #[test]
    fn claude_style_tools_classify_and_resolve_relative_paths() {
        let cwd = Path::new("/repo");
        let m = msg(vec![
            tool_use(
                "Edit",
                json!({"file_path": "src/ir.rs", "old_string": "a", "new_string": "b"}),
            ),
            tool_use("Read", json!({"file_path": "/etc/hosts"})),
            tool_use("Bash", json!({"command": "cargo test -p cv-core"})),
            tool_use("Glob", json!({"pattern": "**/*.rs", "path": "crates"})),
        ]);
        let ev = extract(&m, 7, Some(cwd));
        assert_eq!(ev.len(), 4);
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("file_edit", Some("/repo/src/ir.rs"))
        );
        assert_eq!(ev[0].msg_idx, 7);
        assert_eq!((ev[1].kind, ev[1].target.as_deref()), ("file_read", Some("/etc/hosts")));
        assert_eq!(
            (ev[2].kind, ev[2].target.as_deref()),
            ("command", Some("cargo test -p cv-core"))
        );
        assert_eq!(
            (ev[3].kind, ev[3].target.as_deref()),
            ("file_read", Some("/repo/crates"))
        );
        assert_eq!(ev[0].tool.as_deref(), Some("Edit"));
    }

    #[test]
    fn codex_style_shell_and_local_shell_become_commands() {
        // codex `shell`: argv array.
        let m = msg(vec![tool_use("shell", json!({"command": ["bash", "-lc", "ls -la"]}))]);
        let ev = extract(&m, 0, None);
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("command", Some("bash -lc ls -la"))
        );

        // codex `local_shell`: nested action.command.
        let m = msg(vec![tool_use(
            "local_shell",
            json!({"action": {"type": "exec", "command": ["echo", "hi"]}, "status": "completed"}),
        )]);
        let ev = extract(&m, 0, None);
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("command", Some("echo hi")));
    }

    #[test]
    fn apply_patch_body_yields_per_file_edits() {
        let patch = "*** Begin Patch\n\
                     *** Update File: crates/cv-core/src/ir.rs\n\
                     @@\n-old\n+new\n\
                     *** Add File: docs/notes.md\n\
                     +hello\n\
                     *** End Patch";
        // Plain-string input (codex custom_tool_call).
        let m = msg(vec![tool_use("apply_patch", json!(patch))]);
        let ev = extract(&m, 0, Some(Path::new("/repo")));
        assert_eq!(ev.len(), 2);
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("file_edit", Some("/repo/crates/cv-core/src/ir.rs"))
        );
        assert_eq!(
            (ev[1].kind, ev[1].target.as_deref()),
            ("file_edit", Some("/repo/docs/notes.md"))
        );

        // The same patch routed through the shell tool: command row + per-file edits.
        let m = msg(vec![tool_use("shell", json!({"command": ["apply_patch", patch]}))]);
        let ev = extract(&m, 0, Some(Path::new("/repo")));
        assert_eq!(ev[0].kind, "command");
        let edits: Vec<_> = ev.iter().filter(|e| e.kind == "file_edit").collect();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].target.as_deref(), Some("/repo/crates/cv-core/src/ir.rs"));
    }

    #[test]
    fn unified_diff_headers_need_minus_lines_to_count() {
        // `+++` headers count only alongside `---` ones…
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(patch_targets(diff), vec!["src/lib.rs".to_string()]);
        // …so an *added line* starting with `++ ` can't fake a target.
        let not_a_diff = "*** Begin Patch\n*** Update File: real.rs\n++++ fake.rs\n*** End Patch";
        assert_eq!(patch_targets(not_a_diff), vec!["real.rs".to_string()]);
        assert!(patch_targets("+++ orphan.rs\n").is_empty());
    }

    #[test]
    fn ambiguous_file_tool_and_unknown_tool_degrade_gracefully() {
        // File key but a name that hints neither read nor write → 'tool' with the path target.
        let m = msg(vec![tool_use("Mystery", json!({"path": "x.txt"}))]);
        let ev = extract(&m, 0, Some(Path::new("/w")));
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("tool", Some("/w/x.txt")));
        // No file key, no command → bare 'tool' row.
        let m = msg(vec![tool_use("web_search", json!({"query": "rust"}))]);
        let ev = extract(&m, 0, None);
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("tool", None));
    }

    #[test]
    fn gemini_style_names_classify() {
        let m = msg(vec![
            tool_use("write_file", json!({"file_path": "out.txt", "content": "hi"})),
            tool_use("read_file", json!({"absolute_path": "/g/in.txt"})),
            tool_use("run_shell_command", json!({"command": "make build"})),
            tool_use(
                "replace",
                json!({"file_path": "/g/edit.txt", "old_string": "a", "new_string": "b"}),
            ),
        ]);
        let ev = extract(&m, 0, Some(Path::new("/g")));
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("file_edit", Some("/g/out.txt")));
        assert_eq!((ev[1].kind, ev[1].target.as_deref()), ("file_read", Some("/g/in.txt")));
        assert_eq!((ev[2].kind, ev[2].target.as_deref()), ("command", Some("make build")));
        assert_eq!(
            (ev[3].kind, ev[3].target.as_deref()),
            ("file_edit", Some("/g/edit.txt"))
        );
    }

    #[test]
    fn error_results_emit_error_events_without_materializing_spans() {
        let mut m = Message::new(Role::Tool);
        m.content = vec![
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "command failed: no such file".into(),
                is_error: true,
                tool_name: Some("Bash".into()),
                status: None,
                details: None,
            },
            // A giant failed dump left lazy on disk: the event still lands, detail is skipped.
            Block::ToolResult {
                tool_use_id: "t2".into(),
                content: crate::lazy::Text::Span(crate::lazy::Span {
                    source: None,
                    offset: 0,
                    len: 50_000_000,
                    escaped: false,
                }),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            },
            // Non-error results emit nothing.
            Block::ToolResult {
                tool_use_id: "t3".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ];
        let ev = extract(&m, 3, None);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "error");
        assert_eq!(ev[0].detail.as_deref(), Some("command failed: no such file"));
        assert_eq!(ev[0].tool.as_deref(), Some("Bash"));
        assert_eq!(ev[1].kind, "error");
        assert_eq!(ev[1].detail, None);
    }

    #[test]
    fn long_commands_and_details_are_capped() {
        let long_cmd = "x".repeat(1000);
        let m = msg(vec![tool_use("Bash", json!({"command": long_cmd}))]);
        let ev = extract(&m, 0, None);
        assert!(ev[0].target.as_ref().unwrap().chars().count() <= TARGET_MAX);

        let mut m = Message::new(Role::Tool);
        m.content = vec![Block::ToolResult {
            tool_use_id: "t".into(),
            content: "e".repeat(1000).into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        }];
        let ev = extract(&m, 0, None);
        assert!(ev[0].detail.as_ref().unwrap().chars().count() <= DETAIL_MAX);
    }

    /// `CLUSTERVISION_HOME` is process-global state — the temp-home tests serialize on this.
    #[cfg(feature = "sqlite")]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// End-to-end against a real temp catalog db: stream a claude JSONL fixture through
    /// `ingest_ref`, then read events back with `events_for` and `sessions_touching`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn ingest_and_query_roundtrip_in_temp_catalog() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("cv-events-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("CLUSTERVISION_HOME", &home);

        let sdir = home.join("sessions");
        std::fs::create_dir_all(&sdir).unwrap();
        let path = sdir.join("evt-e2e.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u0", "sessionId": "evt-e2e",
                "message": {"role": "user", "content": "please fix the parser"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "evt-e2e",
                "timestamp": "2026-06-01T12:00:00Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "src/parser.rs", "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test"}}
                ]}
            }),
            serde_json::json!({
                "type": "user", "uuid": "u2", "sessionId": "evt-e2e",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2",
                     "content": "error[E0308]: mismatched types", "is_error": true}
                ]}
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let r = SessionRef {
            id: "evt-e2e".into(),
            harness: crate::ir::Harness::Claude,
            path: path.clone(),
            cwd: Some("/repo".into()),
            title: Some("fix the parser".into()),
            created_at: None,
            updated_at: None,
            message_count: 2,
        };

        let n = ingest_ref(&r).unwrap();
        assert!(n >= 3, "expected at least edit+command+error events, got {n}");

        // Freshly ingested → no longer needs ingest at the same mtime; a re-ingest is idempotent.
        assert!(!needs_ingest(&r, file_mtime_ns(&r.path)));
        ingest_ref(&r).unwrap();

        let rows = events_for("claude", "evt-e2e", None);
        assert_eq!(rows.len(), n, "re-ingest must not duplicate rows");
        let edit = rows.iter().find(|e| e.kind == "file_edit").unwrap();
        assert_eq!(edit.target.as_deref(), Some("/repo/src/parser.rs"));
        assert_eq!(edit.tool.as_deref(), Some("Edit"));
        assert_eq!(edit.ts, Some(1780315200)); // 2026-06-01T12:00:00Z
        let cmd = rows.iter().find(|e| e.kind == "command").unwrap();
        assert_eq!(cmd.target.as_deref(), Some("cargo test"));
        let err = rows.iter().find(|e| e.kind == "error").unwrap();
        assert!(err.detail.as_deref().unwrap().contains("E0308"));

        // Kind filter narrows.
        assert_eq!(events_for("claude", "evt-e2e", Some("command")).len(), 1);

        // Freshness keys on (mtime, size), size-primary for this append-only jsonl: a pure mtime
        // bump (byte-identical rewrite) is still fresh — the mass-mtime-bump condition that used
        // to re-parse the whole corpus — while a real append reads stale.
        let content = std::fs::read(&path).unwrap();
        std::fs::write(&path, &content).unwrap();
        assert!(
            !needs_ingest(&r, file_mtime_ns(&r.path)),
            "a pure mtime bump (size unchanged) must not force an event re-ingest"
        );
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({"type": "user", "uuid": "u3", "sessionId": "evt-e2e",
                                   "message": {"role": "user", "content": "and one more thing"}})
            )
            .unwrap();
        }
        assert!(
            needs_ingest(&r, file_mtime_ns(&r.path)),
            "a real append (size grew) must read stale"
        );
        ingest_ref(&r).unwrap();
        assert!(!needs_ingest(&r, file_mtime_ns(&r.path)));

        // The touched query finds the session by absolute path and by suffix.
        let touched = sessions_touching("/repo/src/parser.rs", false);
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "evt-e2e");
        assert_eq!(touched[0].edits, 1);
        assert_eq!(touched[0].reads, 0);
        let by_suffix = sessions_touching("src/parser.rs", true);
        assert_eq!(by_suffix.len(), 1, "suffix match + edits_only should still hit");
        // A file nobody touched stays quiet.
        assert!(sessions_touching("src/nonexistent.rs", false).is_empty());

        // blame's edit query: abs + repo-relative-suffix matching, session metadata joined in.
        crate::catalog::sync(std::slice::from_ref(&r));
        assert!(catalog_has_events());
        let edits = edits_touching_file("/repo/src/parser.rs", "src/parser.rs");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].session_id, "evt-e2e");
        assert_eq!(edits[0].msg_idx, 1);
        assert_eq!(edits[0].ts, Some(1780315200));
        assert_eq!(edits[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(edits[0].title.as_deref(), Some("fix the parser"));
        // Suffix-only match from a *different* checkout path still hits.
        let edits = edits_touching_file("/elsewhere/src/parser.rs", "src/parser.rs");
        assert_eq!(edits.len(), 1);
        assert!(edits_touching_file("/repo/src/other.rs", "src/other.rs").is_empty());

        std::env::remove_var("CLUSTERVISION_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Migration e2e: a v1-shaped (0.9.10) catalog opened by v2 code is dropped wholesale —
    /// old rows AND `event_sync` (so everything reads as stale) — then lazily reingested; the
    /// version stamp prevents re-drops; and a v1 table snuck back in by an old binary under a
    /// v2 stamp self-heals on the next open.
    #[cfg(feature = "sqlite")]
    #[test]
    fn v1_catalog_migrates_by_drop_and_lazy_reingest() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("cv-events-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("CLUSTERVISION_HOME", &home);

        const V1_DDL: &str = "CREATE TABLE events(
                 harness    TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 msg_idx    INTEGER NOT NULL,
                 ts         INTEGER,
                 kind       TEXT NOT NULL,
                 tool       TEXT,
                 target     TEXT,
                 detail     TEXT
             );
             CREATE UNIQUE INDEX idx_events_dedup
                 ON events(harness, session_id, msg_idx, kind, COALESCE(target,''));
             INSERT INTO events VALUES('claude','mig-e2e',1,NULL,'file_edit','Edit','/old/v1.rs',NULL);";

        // A v1-shaped catalog as 0.9.10 left it: v1 events + a fresh event_sync stamp.
        let sdir = home.join("sessions");
        std::fs::create_dir_all(&sdir).unwrap();
        let path = sdir.join("mig-e2e.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "assistant", "uuid": "a0", "sessionId": "mig-e2e",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "t1", "name": "Edit",
                         "input": {"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}}
                    ]}
                })
            ),
        )
        .unwrap();
        let mtime = file_mtime_ns(&path);
        {
            let conn = rusqlite::Connection::open(home.join("catalog.db")).unwrap();
            conn.execute_batch(&format!(
                "{V1_DDL}
                 CREATE TABLE event_sync(
                     harness TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL,
                     mtime_ns INTEGER NOT NULL, PRIMARY KEY(harness, id, path));
                 INSERT INTO event_sync VALUES('claude','mig-e2e','{}',{mtime});",
                path.display()
            ))
            .unwrap();
        }

        let r = SessionRef {
            id: "mig-e2e".into(),
            harness: crate::ir::Harness::Claude,
            path: path.clone(),
            cwd: Some("/repo".into()),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };

        // First v2 open (inside needs_ingest) migrates: v1 rows gone, sync stamp gone → stale.
        assert!(
            needs_ingest(&r, mtime),
            "v1 event_sync stamps must not survive migration"
        );
        assert!(!catalog_has_events(), "v1 event rows must not survive migration");

        // The normal lazy path then rebuilds cleanly on the v2 schema.
        let n = ingest_ref(&r).unwrap();
        assert_eq!(n, 1);
        assert!(!needs_ingest(&r, mtime));
        let rows = events_for("claude", "mig-e2e", None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target.as_deref(), Some("/repo/src/lib.rs"));
        assert_eq!(sessions_touching("src/lib.rs", true).len(), 1);

        // The stamp stuck, so the next open did NOT re-drop (the ingested rows are still there).
        {
            let conn = rusqlite::Connection::open(home.join("catalog.db")).unwrap();
            let v: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
            assert_eq!(v, 3);

            // Now simulate an old 0.9.10 binary recreating v1 tables under the v2 stamp.
            conn.execute_batch(&format!("DROP TABLE events; {V1_DDL}")).unwrap();
        }
        // Shape detection self-heals on open: full reset, stale again, reingest works.
        assert!(
            !catalog_has_events(),
            "a v1-shaped table under a v2 stamp must be dropped"
        );
        assert!(needs_ingest(&r, mtime));
        ingest_ref(&r).unwrap();
        assert_eq!(events_for("claude", "mig-e2e", None).len(), 1);

        std::env::remove_var("CLUSTERVISION_HOME");
        std::fs::remove_dir_all(&home).ok();
    }
}
