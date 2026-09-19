//! OpenCode adapter — `${XDG_DATA_HOME:-~/.local/share}/opencode/`.
//!
//! ## Where sessions live
//!
//! Since 2026-01 (opencode ≥ 1.1) the canonical store is a SQLite database, `opencode.db`
//! (`$OPENCODE_DB` overrides it; non-release channels write `opencode-<channel>.db`; WAL mode):
//! `session` rows (one per session — `directory` = cwd, `title`, `model`, `agent`, cost and token
//! totals), `message` rows (`data` = the message Info JSON minus `id`/`sessionID`) and `part` rows
//! (`data` = the Part JSON minus `id`/`sessionID`/`messageID`), keyed by `message_id`. Source of
//! truth: `packages/core/src/session/sql.ts` (tables) and `packages/schema/src/v1/session.ts`
//! (payload shapes) at opencode `fee476bb` (1.18). The older JSON tree under `storage/` —
//! `session/**/ses_*.json`, `message/<sid>/<msgid>.json`, `part/<msgid>/<partid>.json` (parts keyed
//! by *messageID*) — was only ever the input to a one-shot importer that was deleted on 2026-06-02:
//! a machine that ran the import still carries a frozen copy, a fresh install has none. We read the
//! database when it exists (behind the `sqlite` cargo feature) and fall back to the JSON tree,
//! de-duplicating by session id (db wins). The payload JSON is identical in both, so one mapping
//! ([`build_messages`]) serves both sources; db-backed [`SessionRef`]s point at the `.db` file.
//!
//! ## Two storage generations (JSON tree)
//!
//! OpenCode rewrote its on-disk model. We sniff by content (presence of separate part files /
//! an inline `summary` object) rather than any version field:
//!
//! * **Inline-summary generation (older):** a message carried an AI-generated `summary`
//!   *object* `{title, body, diffs[]}` inline; there were no separate part files. We render
//!   `summary.body` (falling back to `title`) as the message text and stash `diffs` in `extra`.
//! * **Parts generation (newer):** message content lives in `part/<msgid>/<partid>.json`
//!   files, one per part. The message record holds only metadata. The richest variant.
//!   Confusingly, a *boolean* `summary: true` here is a compaction marker on an assistant
//!   message (mode/agent == "compaction"), NOT inline content — we keep it in `extra`.
//!
//! The on-disk part taxonomy (source of truth: opencode `session/message-v2.ts`) is:
//! `text` (may be `synthetic`/`ignored`), `reasoning` (carries `metadata.anthropic.signature`),
//! `tool` (with a `state` union pending|running|completed|error, each holding
//! input/output/title/metadata/error/time), `file` (file/dir/image attachment with `mime`,
//! `url`, `source`), `agent` (an `@agent` mention / sub-agent reference), `subtask` (a spawned
//! sub-session with its own prompt/agent/model), `patch`, `snapshot`, `step-start`,
//! `step-finish` (per-step cost/tokens), `retry`, `compaction`.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenCode {
    /// The legacy JSON tree (`<data>/opencode/storage`), if present.
    root: Option<PathBuf>,
    /// The canonical SQLite store, if present (and the `sqlite` feature is on).
    db: Option<PathBuf>,
}

impl OpenCode {
    pub fn new() -> Self {
        let data = data_dir();
        let root = data.as_ref().map(|d| d.join("storage")).filter(|p| p.exists());
        let db = data.as_deref().and_then(resolve_db);
        OpenCode { root, db }
    }

    /// An adapter rooted at an explicit JSON storage dir (`…/opencode/storage`) instead of `$HOME` —
    /// lets emit-verification and tests re-parse a just-written session without mutating the
    /// process environment (env writes race other threads' `getenv`). No database.
    pub fn with_root(root: PathBuf) -> Self {
        OpenCode {
            root: Some(root),
            db: None,
        }
    }

    /// An adapter over one explicit `opencode.db` (tests, alternate data dirs). No JSON tree.
    #[cfg(feature = "sqlite")]
    pub fn with_db(db: PathBuf) -> Self {
        OpenCode {
            root: None,
            db: Some(db),
        }
    }

    /// Sessions from the SQLite store, plus their ids so the JSON walk can skip duplicates.
    #[cfg(feature = "sqlite")]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        let Some(db) = &self.db else {
            return (Vec::new(), HashSet::new());
        };
        match db::discover(db) {
            Ok(refs) => {
                let ids = refs.iter().map(|r| r.id.clone()).collect();
                (refs, ids)
            }
            Err(e) => {
                eprintln!("cv: skipping {}: {e:#}", db.display());
                (Vec::new(), HashSet::new())
            }
        }
    }
    #[cfg(not(feature = "sqlite"))]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        (Vec::new(), HashSet::new())
    }

    fn message_dir(&self, sid: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("message").join(sid))
    }
    fn part_dir(&self, mid: &str) -> Option<PathBuf> {
        // Parts are keyed by messageID only: storage/part/<messageID>/<partid>.json
        self.root.as_ref().map(|r| r.join("part").join(mid))
    }
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

/// `${XDG_DATA_HOME:-~/.local/share}/opencode` — opencode's data dir (`packages/core/src/global.ts`,
/// via `xdg-basedir`).
fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("opencode"))
}

/// The database opencode itself would open (`packages/core/src/database/database.ts::path`):
/// `$OPENCODE_DB` (absolute, or relative to the data dir; `:memory:` means none), else
/// `opencode.db` (release channels), else the newest `opencode-<channel>.db`.
#[cfg(feature = "sqlite")]
fn resolve_db(data: &Path) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("OPENCODE_DB") {
        if v == ":memory:" {
            return None;
        }
        let v = PathBuf::from(v);
        let p = if v.is_absolute() { v } else { data.join(v) };
        return p.is_file().then_some(p);
    }
    let main = data.join("opencode.db");
    if main.is_file() {
        return Some(main);
    }
    let mut channel: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(data)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !(name.starts_with("opencode-") && name.ends_with(".db")) {
                return None;
            }
            Some((e.metadata().ok()?.modified().ok()?, p))
        })
        .collect();
    channel.sort();
    channel.pop().map(|(_, p)| p)
}
#[cfg(not(feature = "sqlite"))]
fn resolve_db(_data: &Path) -> Option<PathBuf> {
    None
}

impl Adapter for OpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root
            .clone()
            .or_else(|| self.db.as_ref().and_then(|d| d.parent()).map(Path::to_path_buf))
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // The database first (it is the live store), then the frozen JSON tree for anything the
        // database doesn't have — a session present in both (the import left copies) appears once.
        let (mut out, seen) = self.discover_db();
        let Some(root) = &self.root else {
            return Ok(out);
        };
        let session_dir = root.join("session");
        for entry in WalkDir::new(&session_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("ses_") || !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name.trim_end_matches(".json"))
                .to_string();
            if seen.contains(&id) {
                continue;
            }
            let message_count = self.message_dir(&id).map(|d| count_turns(&d)).unwrap_or(0);
            out.push(SessionRef {
                id,
                harness: Harness::OpenCode,
                path: path.to_path_buf(),
                cwd: v.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| crate::ir::truncate(t, 80)),
                created_at: v.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: v.pointer("/time/updated").and_then(super::ts_from_value),
                message_count,
            });
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // A db-backed ref points at the `.db` file itself (like the other SQLite adapters).
        #[cfg(feature = "sqlite")]
        if db::is_db_ref(r) {
            return db::stream(r, sink);
        }
        // OpenCode stores a session as a *directory* of per-message JSON files (plus per-message
        // part files). The old `parse` slurped every message body into a `Vec` to sort it — for a
        // large session that resident-spikes the whole transcript. Here we instead:
        //   1. read the small session meta file,
        //   2. cheaply order the message files (read each only to pull its `time/created` key, then
        //      drop the body — peak is O(one message file), not the whole session),
        //   3. re-read each file in order, build its IR message(s), emit to `sink`, and drop it
        //      before touching the next.
        // `load_parts` already loads one message's parts at a time, so peak stays O(one message).
        let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
        let meta: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::OpenCode,
            cwd: meta.get("directory").and_then(Value::as_str).map(PathBuf::from),
            title: meta.get("title").and_then(Value::as_str).map(str::to_string),
            created_at: meta.pointer("/time/created").and_then(super::ts_from_value),
            updated_at: meta.pointer("/time/updated").and_then(super::ts_from_value),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
        };

        // Order message files by `time/created` (same ordering as before) while holding only the
        // sort key + path per file — never all message bodies at once.
        let mut order: Vec<(i64, PathBuf)> = Vec::new();
        if let Some(mdir) = self.message_dir(&r.id) {
            if let Ok(rd) = fs::read_dir(&mdir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let path = e.path();
                    if let Ok(t) = fs::read_to_string(&path) {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let when = v.pointer("/time/created").and_then(Value::as_i64).unwrap_or(0);
                            order.push((when, path));
                        }
                    }
                }
            }
        }
        order.sort_by_key(|(t, _)| *t);

        sink.meta(&s);

        // Re-read one message file at a time, emit, then drop before the next.
        for (_, path) in &order {
            let Ok(t) = fs::read_to_string(path) else { continue };
            let Ok(mv) = serde_json::from_str::<Value>(&t) else {
                continue;
            };
            let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
            let parts = self.load_parts(msg_id);
            for built in build_messages(&mv, &parts) {
                if s.model.is_none() {
                    s.model = built.model.clone();
                }
                if sink.message(built) == Flow::Stop {
                    return Ok(s);
                }
            }
        }

        Ok(s)
    }
}

/// Count a session's conversational turns: message records whose `role` is `user`/`assistant`.
/// The [`SessionRef::message_count`] contract is user+assistant turns only — counting directory
/// entries (the old behavior) also picked up system/summary records and any stray files, so the
/// number disagreed with every other adapter. Message files are small metadata records (the heavy
/// content lives in part files, which this never touches), so the peek stays cheap.
fn count_turns(dir: &std::path::Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let Ok(text) = fs::read_to_string(e.path()) else {
                return false;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            matches!(v.get("role").and_then(Value::as_str), Some("user") | Some("assistant"))
        })
        .count()
}

impl OpenCode {
    fn load_parts(&self, mid: &str) -> Vec<Value> {
        let Some(pdir) = self.part_dir(mid) else {
            return vec![];
        };
        let mut parts: Vec<(String, Value)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&pdir) {
            for e in rd.filter_map(|e| e.ok()) {
                let fname = e.file_name().to_string_lossy().to_string();
                if let Ok(t) = fs::read_to_string(e.path()) {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        parts.push((fname, v));
                    }
                }
            }
        }
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        parts.into_iter().map(|(_, v)| v).collect()
    }
}

// ── the SQLite store ─────────────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
mod db {
    //! `opencode.db` — `session`, `message`, `part` (`packages/core/src/session/sql.ts`). Rows are
    //! hydrated back into the exact JSON the tree stored (`{...data, id, sessionID, messageID}`,
    //! as `message-v2.ts` does) and handed to the shared mapping, so the two sources can't drift.
    use super::*;
    use rusqlite::types::ValueRef;
    use rusqlite::{Connection, OpenFlags};
    use std::collections::HashMap;

    /// Session columns the IR first-classes (everything else lands in `Session::extra` verbatim).
    const FIRSTCLASS: &[&str] = &["id", "directory", "title", "time_created", "time_updated"];
    /// Columns opencode declares `{ mode: "json" }` — stored as JSON text, decoded on read.
    const JSON_COLS: &[&str] = &["model", "metadata", "revert", "permission", "summary_diffs"];

    pub(super) fn is_db_ref(r: &SessionRef) -> bool {
        r.path.extension().and_then(|e| e.to_str()) == Some("db")
    }

    fn open_ro(path: &Path) -> Result<Connection> {
        // Read-only, so we never take a write lock on the user's live (WAL-mode) database.
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", path.display()))
    }

    fn ms(v: Option<i64>) -> Option<chrono::DateTime<chrono::Utc>> {
        v.and_then(|n| super::super::ts_from_value(&Value::from(n)))
    }

    /// One [`SessionRef`] per `session` row, newest first; `message_count` is user+assistant turns
    /// (the role lives inside the `data` JSON — SQLite's built-in JSON functions read it).
    pub(super) fn discover(db: &Path) -> Result<Vec<SessionRef>> {
        let conn = open_ro(db)?;
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT session_id, COUNT(*) FROM message \
                 WHERE json_extract(data, '$.role') IN ('user', 'assistant') GROUP BY session_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for (sid, n) in rows.flatten() {
                counts.insert(sid, n.max(0) as usize);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, directory, title, time_created, time_updated FROM session ORDER BY time_updated DESC",
        )?;
        let refs = stmt
            .query_map([], |r| {
                Ok(SessionRef {
                    id: r.get(0)?,
                    harness: Harness::OpenCode,
                    path: db.to_path_buf(),
                    cwd: r.get::<_, Option<String>>(1)?.map(PathBuf::from),
                    title: r
                        .get::<_, Option<String>>(2)?
                        .filter(|t| !t.is_empty())
                        .map(|t| crate::ir::truncate(&t, 80)),
                    created_at: ms(r.get::<_, Option<i64>>(3)?),
                    updated_at: ms(r.get::<_, Option<i64>>(4)?),
                    message_count: 0,
                })
            })?
            .flatten()
            .map(|mut r| {
                r.message_count = counts.get(&r.id).copied().unwrap_or(0);
                r
            })
            .collect();
        Ok(refs)
    }

    /// One row as a JSON object keyed by column name (`NULL`s dropped, JSON columns decoded).
    fn row_to_map(cols: &[String], row: &rusqlite::Row<'_>) -> Result<Map<String, Value>> {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                ValueRef::Null => continue,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => {
                    let t = String::from_utf8_lossy(t).into_owned();
                    if JSON_COLS.contains(&c.as_str()) {
                        serde_json::from_str(&t).unwrap_or(Value::String(t))
                    } else {
                        Value::String(t)
                    }
                }
                ValueRef::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            };
            m.insert(c.clone(), v);
        }
        Ok(m)
    }

    /// Stream one db-backed session: the `session` row → [`Session`] metadata + `extra`, then each
    /// `message` row with its `part` rows hydrated and mapped one message at a time.
    pub(super) fn stream(r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
        let conn = open_ro(&r.path)?;
        let row = {
            // `SELECT *` + names, so a column that only exists in newer schemas (`metadata`,
            // 2026-05; `workspace_id`, `path`, …) is read when present and never demanded.
            let mut stmt = conn.prepare("SELECT * FROM session WHERE id = ?1")?;
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = stmt.query(rusqlite::params![r.id])?;
            match rows.next()? {
                Some(row) => row_to_map(&cols, row)?,
                None => anyhow::bail!("no opencode session {} in {}", r.id, r.path.display()),
            }
        };
        let mut extra = Map::new();
        for (k, v) in &row {
            // Keep the session's own facts (parent_id, agent, model, cost, tokens_*, summary_*,
            // share_url, version, project_id, time_archived, metadata, …) verbatim; skip blanks.
            if FIRSTCLASS.contains(&k.as_str()) || v.as_str().is_some_and(str::is_empty) {
                continue;
            }
            extra.insert(k.clone(), v.clone());
        }
        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::OpenCode,
            cwd: row.get("directory").and_then(Value::as_str).map(PathBuf::from),
            title: row
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            created_at: row.get("time_created").and_then(super::super::ts_from_value),
            updated_at: row.get("time_updated").and_then(super::super::ts_from_value),
            // `session.model` = `{id, providerID, variant?}`; the first message's model is the
            // fallback for rows written before the column existed.
            model: row
                .get("model")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra,
        };
        sink.meta(&s);

        let mut msgs = conn.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
        let mut parts = conn.prepare("SELECT id, data FROM part WHERE message_id = ?1 ORDER BY id")?;
        let mut rows = msgs.query(rusqlite::params![r.id])?;
        while let Some(row) = rows.next()? {
            let mid: String = row.get(0)?;
            let data: String = row.get(1)?;
            let Ok(Value::Object(mut mv)) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            mv.insert("id".into(), json!(mid));
            mv.insert("sessionID".into(), json!(r.id));
            let part_rows = parts.query_map(rusqlite::params![mid], |pr| {
                Ok((pr.get::<_, String>(0)?, pr.get::<_, String>(1)?))
            })?;
            let mut pv: Vec<Value> = Vec::new();
            for (pid, pdata) in part_rows.flatten() {
                if let Ok(Value::Object(mut po)) = serde_json::from_str::<Value>(&pdata) {
                    po.insert("id".into(), json!(pid));
                    po.insert("messageID".into(), json!(mid));
                    po.insert("sessionID".into(), json!(r.id));
                    pv.push(Value::Object(po));
                }
            }
            for built in build_messages(&Value::Object(mv), &pv) {
                if s.model.is_none() {
                    s.model = built.model.clone();
                }
                if sink.message(built) == Flow::Stop {
                    return Ok(s);
                }
            }
        }
        Ok(s)
    }
}

// ── pure mapping (filesystem-free, so it's unit-testable for both generations) ──────────────

/// Map one OpenCode message record + its parts into IR messages. Tool results are split into a
/// trailing `Role::Tool` message (mirroring the Claude adapter) so conversions re-encode them
/// correctly. Returns 0–2 messages.
fn build_messages(mv: &Value, parts: &[Value]) -> Vec<Message> {
    let role = match mv.get("role").and_then(Value::as_str) {
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => Role::User,
    };

    let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
    let mut m = Message::new(role);
    m.id = (!msg_id.is_empty()).then(|| msg_id.to_string());
    m.parent_id = mv.get("parentID").and_then(Value::as_str).map(str::to_string);
    m.timestamp = mv.pointer("/time/created").and_then(super::ts_from_value);
    // Newer records carry `modelID`; user records nest it under `model.modelID`.
    m.model = mv
        .get("modelID")
        .and_then(Value::as_str)
        .or_else(|| mv.pointer("/model/modelID").and_then(Value::as_str))
        .map(str::to_string);
    m.usage = parse_tokens(mv.get("tokens"));
    collect_message_extra(mv, &mut m.extra);

    let mut tool_results: Vec<Block> = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        m.content.push(Block::Text {
                            text: t.to_string().into(),
                        });
                    }
                }
            }
            "reasoning" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        // Providers stash an opaque signature under
                        // `metadata.<provider>.signature` (e.g. anthropic). Preserve it so a
                        // thinking block can round-trip.
                        let signature = part
                            .pointer("/metadata/anthropic/signature")
                            .and_then(Value::as_str)
                            .or_else(|| find_signature(part.get("metadata")))
                            .map(str::to_string);
                        m.content.push(Block::Thinking {
                            text: t.to_string().into(),
                            signature,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }
            "tool" => {
                let (use_block, results) = tool_blocks(part);
                m.content.push(use_block);
                tool_results.extend(results);
            }
            "file" => {
                m.content.push(file_block(part));
            }
            "agent" => {
                // An `@agent` mention / sub-agent reference embedded in a user prompt. No IR
                // block fits; surface its name as text and keep the structured form in extra.
                if let Some(name) = part.get("name").and_then(Value::as_str) {
                    m.content.push(Block::Text {
                        text: format!("@{name}").into(),
                    });
                }
                push_extra_array(&mut m.extra, "agent_refs", part.clone());
            }
            "subtask" => {
                // A spawned sub-session (Task tool style). Render its prompt + which agent ran
                // it; keep the full record (agent/model/command/description) in extra.
                let agent = part.get("agent").and_then(Value::as_str).unwrap_or("agent");
                let prompt = part.get("prompt").and_then(Value::as_str).unwrap_or("");
                m.content.push(Block::Text {
                    text: format!("[subtask → {agent}] {prompt}").into(),
                });
                push_extra_array(&mut m.extra, "subtasks", part.clone());
            }
            "patch" => push_extra_array(&mut m.extra, "patches", part.clone()),
            "snapshot" => push_extra_array(&mut m.extra, "snapshots", part.clone()),
            "step-start" | "step-finish" => push_extra_array(&mut m.extra, "steps", part.clone()),
            "retry" => push_extra_array(&mut m.extra, "retries", part.clone()),
            "compaction" => push_extra_array(&mut m.extra, "compactions", part.clone()),
            other if !other.is_empty() => {
                // Unknown / future part type: never drop it silently — stash verbatim.
                push_extra_array(&mut m.extra, "unknown_parts", part.clone());
            }
            _ => {}
        }
    }

    // Older inline-summary generation stored no parts — fall back to the AI summary so the turn
    // isn't blank. (`summary` must be an *object* here; a boolean `summary: true` is a newer
    // compaction marker handled in collect_message_extra.)
    if m.content.is_empty() {
        let summary = mv
            .pointer("/summary/body")
            .or_else(|| mv.pointer("/summary/title"))
            .and_then(Value::as_str);
        if let Some(text) = summary {
            if !text.is_empty() {
                m.content.push(Block::Text {
                    text: text.to_string().into(),
                });
            }
        }
    }

    let mut out = Vec::new();
    if !m.content.is_empty() || !m.extra.is_empty() {
        out.push(m);
    }
    if !tool_results.is_empty() {
        let mut tm = Message::new(Role::Tool);
        tm.content = tool_results;
        out.push(tm);
    }
    out
}

/// Build the `ToolUse` block for a `tool` part, plus the blocks that belong in the Tool message:
/// the `ToolResult` (if the state carries one) followed by any `state.attachments[]` (files/images a
/// completed tool handed back — a `read` of an image, a screenshot — same shape as a `file` part).
/// The state's `title`/`metadata`/`time` ride in the result's `details`. Tolerant of the
/// `completed`/`error`/`running`/`pending` state shapes.
fn tool_blocks(part: &Value) -> (Block, Vec<Block>) {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = part.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
    let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");

    let tool_name = (!name.is_empty()).then(|| name.clone());
    let status_field = (!status.is_empty()).then(|| status.to_string());
    let details = tool_details(part);
    let use_block = Block::ToolUse {
        id: call_id.clone(),
        name,
        input: part.pointer("/state/input").cloned().unwrap_or(Value::Null),
    };

    // A completed tool carries `output`; an error tool carries `error`.
    let result = match status {
        "completed" | "running" | "pending" => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: coerce_text(out).into(),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: details.clone(),
        }),
        "error" => {
            let content = part
                .pointer("/state/error")
                .or_else(|| part.pointer("/state/output"))
                .map(coerce_text)
                .unwrap_or_default();
            Some(Block::ToolResult {
                tool_use_id: call_id.clone(),
                content: content.into(),
                is_error: true,
                tool_name: tool_name.clone(),
                status: status_field.clone(),
                details: details.clone(),
            })
        }
        // Unknown status but an output is present — still surface it.
        _ => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: coerce_text(out).into(),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: details.clone(),
        }),
    };

    let mut out: Vec<Block> = result.into_iter().collect();
    if let Some(atts) = part.pointer("/state/attachments").and_then(Value::as_array) {
        out.extend(atts.iter().map(file_block));
    }
    (use_block, out)
}

/// `state.{title, metadata, time}` of a tool part — the tool's own label (e.g. the file read),
/// structured metadata (exit code, diff stats, …) and start/end times — as the result's `details`.
fn tool_details(part: &Value) -> Option<Value> {
    let mut d = Map::new();
    for key in ["title", "metadata", "time"] {
        if let Some(v) = part.pointer(&format!("/state/{key}")) {
            let blank = v.is_null() || v.as_object().is_some_and(|o| o.is_empty()) || v.as_str() == Some("");
            if !blank {
                d.insert(key.to_string(), v.clone());
            }
        }
    }
    (!d.is_empty()).then_some(Value::Object(d))
}

/// Map a `file` part to a block. Genuine images become `Block::Image`; everything else
/// (text files, directories via `@path`, symbol/resource refs) becomes a first-class
/// `Block::File` reference.
fn file_block(part: &Value) -> Block {
    let mime = part.get("mime").and_then(Value::as_str).unwrap_or("");
    if mime.starts_with("image/") {
        Block::Image {
            media_type: (!mime.is_empty()).then(|| mime.to_string()),
            data_ref: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    } else {
        // `source.path` is the workspace-relative path when present; `filename`/`url` are fallbacks.
        let path = part
            .pointer("/source/path")
            .or_else(|| part.get("filename"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Block::File {
            mime: (!mime.is_empty()).then(|| mime.to_string()),
            path,
            source: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    }
}

/// Preserve message-level fields the IR has no home for, verbatim, in `Message.extra`.
fn collect_message_extra(mv: &Value, extra: &mut Map<String, Value>) {
    for key in [
        "agent",      // which agent ran this turn (Sisyphus, build, compaction, …)
        "mode",       // deprecated alias of agent; kept for old records
        "providerID", // amazon-bedrock, anthropic, …
        "cost",       // USD cost of the turn
        "finish",     // finish reason: stop | tool-calls | length | …
        "error",      // assistant error object (aborted / overflow / api error)
        "variant",    // model variant
        "structured", // structured-output result
        "format",     // requested output format
        "system",     // per-message system prompt (user records)
        "tools",      // enabled-tools map (user records)
    ] {
        if let Some(v) = mv.get(key) {
            if !v.is_null() {
                extra.insert(key.to_string(), v.clone());
            }
        }
    }
    // `path: {cwd, root}` — per-turn working dir.
    if let Some(cwd) = mv.pointer("/path/cwd").and_then(Value::as_str) {
        extra.insert("cwd".to_string(), json!(cwd));
    }
    // The assistant `error` union is tagged by `name` — ProviderAuthError, UnknownError,
    // MessageOutputLengthError, MessageAbortedError, StructuredOutputError, ContextOverflowError,
    // ContentFilterError (2026-06), APIError — with the detail under `data`. Lift the tag so
    // consumers can filter on it without unpicking the object.
    if let Some(name) = mv.pointer("/error/name").and_then(Value::as_str) {
        extra.insert("error_name".to_string(), json!(name));
    }
    // A boolean `summary: true` marks a compaction summary message (newer generation). Don't
    // confuse it with the older inline `summary` object.
    if mv.get("summary").and_then(Value::as_bool) == Some(true) {
        extra.insert("is_compaction_summary".to_string(), json!(true));
    }
    // The older inline summary's `diffs[]` are worth keeping even though we render body as text.
    if let Some(diffs) = mv.pointer("/summary/diffs") {
        if diffs.is_array() && !diffs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            extra.insert("summary_diffs".to_string(), diffs.clone());
        }
    }
    // tokens.reasoning isn't on Usage; keep it visible.
    if let Some(r) = mv.pointer("/tokens/reasoning").and_then(Value::as_u64) {
        if r > 0 {
            extra.insert("reasoning_tokens".to_string(), json!(r));
        }
    }
}

fn push_extra_array(extra: &mut Map<String, Value>, key: &str, v: Value) {
    match extra.get_mut(key) {
        Some(Value::Array(a)) => a.push(v),
        _ => {
            extra.insert(key.to_string(), Value::Array(vec![v]));
        }
    }
}

/// Look for a `signature` string anywhere one provider-level deep inside `metadata`
/// (`metadata.<provider>.signature`), without hard-coding the provider name.
fn find_signature(metadata: Option<&Value>) -> Option<&str> {
    let obj = metadata?.as_object()?;
    for v in obj.values() {
        if let Some(sig) = v.pointer("/signature").and_then(Value::as_str) {
            return Some(sig);
        }
    }
    None
}

fn coerce_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_tokens(v: Option<&Value>) -> Option<Usage> {
    let v = v?;
    Some(Usage {
        input_tokens: v.get("input").and_then(Value::as_u64),
        output_tokens: v.get("output").and_then(Value::as_u64),
        cache_read_tokens: v.pointer("/cache/read").and_then(Value::as_u64),
        cache_creation_tokens: v.pointer("/cache/write").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(json_strs: &[&str]) -> Vec<Value> {
        json_strs.iter().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    #[test]
    fn parts_generation_text_reasoning_tool_split() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"msg_1","role":"assistant","modelID":"claude","agent":"build",
                "providerID":"anthropic","cost":0.01,"finish":"tool-calls",
                "time":{"created":1767000000000},
                "tokens":{"input":10,"output":5,"reasoning":3,"cache":{"read":1,"write":2}}}"#,
        )
        .unwrap();
        let p = parts(&[
            r#"{"type":"reasoning","text":"thinking...","metadata":{"anthropic":{"signature":"SIG"}},"time":{"start":1}}"#,
            r#"{"type":"text","text":"Hello"}"#,
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List","metadata":{"exit":0}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        // assistant message + a tool-role message
        assert_eq!(out.len(), 2);
        let asst = &out[0];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!(asst.model.as_deref(), Some("claude"));
        // thinking signature preserved
        match &asst.content[0] {
            Block::Thinking { text, signature, .. } => {
                assert_eq!(text, "thinking...");
                assert_eq!(signature.as_deref(), Some("SIG"));
            }
            b => panic!("expected thinking, got {b:?}"),
        }
        assert!(matches!(&asst.content[1], Block::Text { text } if text == "Hello"));
        assert!(matches!(&asst.content[2], Block::ToolUse { name, .. } if name == "bash"));
        // message-level extras
        assert_eq!(asst.extra.get("agent").and_then(Value::as_str), Some("build"));
        assert_eq!(asst.extra.get("finish").and_then(Value::as_str), Some("tool-calls"));
        assert_eq!(asst.extra.get("reasoning_tokens").and_then(Value::as_u64), Some(3));
        // usage
        let u = asst.usage.as_ref().unwrap();
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.cache_creation_tokens, Some(2));
        // tool result became its own Tool message
        assert_eq!(out[1].role, Role::Tool);
        match &out[1].content[0] {
            Block::ToolResult {
                content,
                is_error,
                tool_use_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(content, "a\nb");
                assert!(!is_error);
                assert_eq!(tool_use_id, "c1");
                assert_eq!(tool_name.as_deref(), Some("bash"));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            b => panic!("expected tool result, got {b:?}"),
        }
    }

    #[test]
    fn tool_error_state_marks_is_error() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c","tool":"edit","state":{"status":"error","input":{},"error":"oldString not found","time":{"start":1,"end":2}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        let res = out.iter().flat_map(|m| &m.content).find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.to_string(), *is_error)),
            _ => None,
        });
        assert_eq!(res, Some(("oldString not found".to_string(), true)));
    }

    #[test]
    fn file_part_image_vs_reference() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"file","mime":"image/png","url":"file:///x.png","filename":"x.png"}"#,
            r#"{"type":"file","mime":"text/plain","url":"file:///Makefile","filename":"Makefile","source":{"type":"file","path":"Makefile"}}"#,
            r#"{"type":"file","mime":"application/x-directory","url":"file:///tools/","filename":"tools/"}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        // image -> Image block
        assert!(matches!(&m.content[0], Block::Image { media_type, .. } if media_type.as_deref()==Some("image/png")));
        // text file -> File reference, NOT an image
        assert!(matches!(&m.content[1], Block::File { path, .. } if path.as_deref()==Some("Makefile")));
        // directory -> File reference, NOT an image
        assert!(
            matches!(&m.content[2], Block::File { source, .. } if source.as_deref().unwrap_or("").contains("tools/"))
        );
    }

    #[test]
    fn agent_subtask_patch_snapshot_steps_preserved() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"agent","name":"oracle","source":{"value":"@oracle","start":0,"end":7}}"#,
            r#"{"type":"subtask","prompt":"do the thing","description":"d","agent":"build"}"#,
            r#"{"type":"patch","hash":"abc","files":["/a.rs"]}"#,
            r#"{"type":"snapshot","snapshot":"snap123"}"#,
            r#"{"type":"step-start"}"#,
            r#"{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
            r#"{"type":"retry","attempt":1,"error":{"name":"APIError"},"time":{"created":9}}"#,
            r#"{"type":"compaction","auto":false}"#,
            r#"{"type":"galaxy-brain-future-part","wat":true}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            m.extra.get("patches").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("snapshots").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("steps").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(2)
        );
        assert_eq!(
            m.extra.get("retries").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("compactions").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("agent_refs").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("subtasks").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        // unknown part type is never dropped
        assert_eq!(
            m.extra.get("unknown_parts").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        // agent + subtask also surface as text
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "@oracle")));
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("subtask → build"))));
    }

    #[test]
    fn inline_summary_generation_fallback() {
        // Older generation: no parts, inline summary object with diffs.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "summary":{"title":"Cleanup","body":"Removed debug comments.",
                "diffs":[{"file":"a.rs","before":"x","after":"y"}]}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text == "Removed debug comments."));
        // diffs preserved in extra
        assert!(out[0].extra.get("summary_diffs").is_some());
    }

    #[test]
    fn boolean_summary_is_compaction_marker_not_text() {
        // Newer generation: summary:true is a compaction flag, not inline content.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},"agent":"compaction","summary":true}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"compacted history…"}"#]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            m.extra.get("is_compaction_summary").and_then(Value::as_bool),
            Some(true)
        );
        // the boolean must NOT be rendered as a text body; the real text comes from parts
        assert!(matches!(&m.content[0], Block::Text { text } if text == "compacted history…"));
    }

    #[test]
    fn synthetic_text_still_rendered() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[r#"{"type":"text","text":"[search-mode] ...","synthetic":true}"#]);
        let out = build_messages(&mv, &p);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text.starts_with("[search-mode]")));
    }

    #[test]
    fn user_model_nested_under_model_object() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "model":{"providerID":"amazon-bedrock","modelID":"claude-opus"}}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"hi"}"#]);
        let out = build_messages(&mv, &p);
        assert_eq!(out[0].model.as_deref(), Some("claude-opus"));
    }

    /// End-to-end against on-disk fixtures for BOTH storage generations.
    #[test]
    fn parses_fixtures_both_generations() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");

        // Newer parts generation.
        let oc = OpenCode::with_root(base.join("parts_gen"));
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "one session in parts_gen fixture");
        let s = oc.parse(&refs[0]).unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "bash")));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool && !m.content.is_empty()));
        assert_eq!(s.model.as_deref(), Some("anthropic.claude-sonnet-4-5"));

        // Older inline-summary generation (message records only, no part files).
        let oc2 = OpenCode::with_root(base.join("summary_gen"));
        let refs2 = oc2.discover().unwrap();
        assert_eq!(refs2.len(), 1, "one session in summary_gen fixture");
        let s2 = oc2.parse(&refs2[0]).unwrap();
        assert!(s2
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.contains("debug comments"))));
    }

    #[test]
    fn tool_attachments_and_state_details_ride_with_the_result() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c1","tool":"read","state":{"status":"completed","input":{"filePath":"x.png"},"output":"","title":"x.png","metadata":{"preview":"…"},"time":{"start":1,"end":2},"attachments":[{"type":"file","mime":"image/png","url":"data:image/png;base64,iVBOR","filename":"x.png"},{"type":"file","mime":"text/plain","url":"file:///notes.txt","filename":"notes.txt"}]}}"#,
        ]);
        let out = build_messages(&mv, &p);
        assert_eq!(out.len(), 2);
        let tool = &out[1];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.content.len(), 3, "result + two attachments");
        match &tool.content[0] {
            Block::ToolResult { details, .. } => {
                let d = details.as_ref().expect("details");
                assert_eq!(d["title"], "x.png");
                assert_eq!(d["time"]["end"], 2);
                assert_eq!(d["metadata"]["preview"], "…");
            }
            b => panic!("expected tool result, got {b:?}"),
        }
        assert!(
            matches!(&tool.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/png"))
        );
        assert!(matches!(&tool.content[2], Block::File { path, .. } if path.as_deref() == Some("notes.txt")));
    }

    #[test]
    fn error_name_is_lifted() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},
                "error":{"name":"ContentFilterError","data":{"message":"blocked"}}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        assert_eq!(out[0].extra["error_name"], "ContentFilterError");
        assert_eq!(out[0].extra["error"]["data"]["message"], "blocked");
    }

    /// The real `opencode.db` DDL (opencode `fee476bb`, `packages/core/src/session/sql.ts`), minus
    /// the `project` foreign key. `with_metadata` adds the 2026-05 `session.metadata` column.
    #[cfg(feature = "sqlite")]
    fn make_db(path: &std::path::Path, with_metadata: bool) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        let metadata_col = if with_metadata { ", `metadata` text" } else { "" };
        conn.execute_batch(&format!(
            "CREATE TABLE `session` (`id` text PRIMARY KEY, `project_id` text NOT NULL, `parent_id` text, \
             `slug` text NOT NULL, `directory` text NOT NULL, `title` text NOT NULL, `version` text NOT NULL, \
             `share_url` text, `summary_additions` integer, `summary_deletions` integer, `summary_files` integer, \
             `summary_diffs` text, `revert` text, `permission` text, `time_created` integer NOT NULL, \
             `time_updated` integer NOT NULL, `time_compacting` integer, `time_archived` integer, \
             `workspace_id` text, `path` text, `agent` text, `model` text, `cost` real DEFAULT 0 NOT NULL, \
             `tokens_input` integer DEFAULT 0 NOT NULL, `tokens_output` integer DEFAULT 0 NOT NULL, \
             `tokens_reasoning` integer DEFAULT 0 NOT NULL, `tokens_cache_read` integer DEFAULT 0 NOT NULL, \
             `tokens_cache_write` integer DEFAULT 0 NOT NULL{metadata_col});\
             CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);\
             CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);"
        ))
        .unwrap();
        conn
    }

    #[cfg(feature = "sqlite")]
    fn seed_session(conn: &rusqlite::Connection, sid: &str, with_metadata: bool) {
        let extra_cols = if with_metadata { ", metadata" } else { "" };
        let extra_vals = if with_metadata { ", '{\"origin\":\"test\"}'" } else { "" };
        conn.execute(
            &format!(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, \
                 time_updated, agent, model, cost, tokens_input, tokens_output, tokens_cache_read{extra_cols}) VALUES \
                 (?1, 'proj', 'ses_PARENT', '', '/home/dev/proj', 'Removing debug comments', '1.15.10', \
                 1767591058589, 1767591138434, 'Sisyphus', '{{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\",\"variant\":\"default\"}}', \
                 0.27, 42987, 2055, 123762{extra_vals})"
            ),
            rusqlite::params![sid],
        )
        .unwrap();
        // Message rows: `data` = Info minus id/sessionID (exactly what opencode writes).
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591060000, 1767591060000, ?3)",
            rusqlite::params![
                "msg_user01",
                sid,
                r#"{"role":"user","time":{"created":1767591060000},"model":{"providerID":"anthropic","modelID":"claude-sonnet-4-5"},"agent":"Sisyphus"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591135919, 1767591138413, ?3)",
            rusqlite::params![
                "msg_asst01",
                sid,
                r#"{"role":"assistant","time":{"created":1767591135919,"completed":1767591138413},"parentID":"msg_user01","modelID":"claude-sonnet-4-5","providerID":"anthropic","agent":"Sisyphus","path":{"cwd":"/home/dev/proj","root":"/home/dev/proj"},"cost":0.03,"tokens":{"input":8708,"output":7,"reasoning":0,"cache":{"read":20627,"write":0}},"finish":"tool-calls"}"#
            ],
        )
        .unwrap();
        // Part rows: `data` = Part minus id/sessionID/messageID; ordered by id.
        let add = |pid: &str, mid: &str, data: &str| {
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 1, 1, ?4)",
                rusqlite::params![pid, mid, sid, data],
            )
            .unwrap();
        };
        add(
            "prt_a",
            "msg_user01",
            r#"{"type":"text","text":"remove the debug comments"}"#,
        );
        add("prt_b", "msg_asst01", r#"{"type":"step-start","snapshot":"c2e2"}"#);
        add(
            "prt_c",
            "msg_asst01",
            r#"{"type":"reasoning","text":"scanning…","metadata":{"anthropic":{"signature":"SIG"}}}"#,
        );
        add("prt_d", "msg_asst01", r#"{"type":"text","text":"Reverted."}"#);
        add(
            "prt_e",
            "msg_asst01",
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"ls","metadata":{"exit":0},"time":{"start":1,"end":2}}}"#,
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_store_discover_and_parse() {
        for with_metadata in [false, true] {
            let dir = std::env::temp_dir().join(format!("cv-opencode-db-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let db = dir.join("opencode.db");
            let conn = make_db(&db, with_metadata);
            seed_session(&conn, "ses_DB01", with_metadata);
            drop(conn);

            let oc = OpenCode::with_db(db.clone());
            let refs = oc.discover().unwrap();
            assert_eq!(refs.len(), 1);
            let r = &refs[0];
            assert_eq!(r.id, "ses_DB01");
            assert_eq!(r.path, db, "a db-backed ref points at the database");
            assert_eq!(r.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(r.title.as_deref(), Some("Removing debug comments"));
            assert_eq!(
                r.message_count, 2,
                "user + assistant turns, from the role inside `data`"
            );
            assert!(r.created_at.is_some() && r.updated_at >= r.created_at);

            let s = oc.parse(r).unwrap();
            assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-5"), "session.model.id wins");
            assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(s.extra["agent"], "Sisyphus");
            assert_eq!(s.extra["parent_id"], "ses_PARENT");
            assert_eq!(s.extra["tokens_input"], 42987);
            assert_eq!(s.extra["model"]["providerID"], "anthropic");
            assert!(!s.extra.contains_key("slug"), "blank columns stay out of extra");
            assert_eq!(
                s.extra.get("metadata").map(|m| m["origin"] == "test"),
                with_metadata.then_some(true)
            );
            // user, assistant, tool — the same mapping the JSON tree gets
            let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
            assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
            assert_eq!(
                s.messages[0].id.as_deref(),
                Some("msg_user01"),
                "row id hydrated into the record"
            );
            assert_eq!(s.messages[1].parent_id.as_deref(), Some("msg_user01"));
            assert!(
                matches!(&s.messages[1].content[0], Block::Thinking { signature, .. } if signature.as_deref() == Some("SIG"))
            );
            assert!(matches!(&s.messages[1].content[1], Block::Text { text } if text == "Reverted."));
            assert!(matches!(&s.messages[2].content[0], Block::ToolResult { content, .. } if content == "a\nb"));
            assert_eq!(s.messages[1].usage.as_ref().unwrap().cache_read_tokens, Some(20627));
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_wins_over_a_json_duplicate_and_json_still_fills_gaps() {
        // The JSON fixture holds ses_FIX01; a database holding the same id (the import left both)
        // must yield ONE ref, the db-backed one — while a JSON-only session is still discovered.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen");
        let dir = std::env::temp_dir().join(format!("cv-opencode-dedupe-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = make_db(&db, false);
        seed_session(&conn, "ses_FIX01", false);
        drop(conn);
        let oc = OpenCode {
            root: Some(base),
            db: Some(db.clone()),
        };
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "duplicate collapsed: {refs:?}");
        assert_eq!(refs[0].path, db);
        // and with a db that lacks it, the JSON copy is used
        let oc_json_only = OpenCode {
            root: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen")),
            db: None,
        };
        assert_eq!(oc_json_only.discover().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }
}
