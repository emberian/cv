//! Zed editor agent-panel adapter — `threads.db` (SQLite, zstd-compressed JSON blobs).
//! **sqlite feature only** (rusqlite + `store::zst`).
//!
//! Zed keeps every agent thread in one SQLite DB under its data dir:
//!   * **macOS:**   `~/Library/Application Support/Zed/threads/threads.db`
//!   * **Linux:**   `$XDG_DATA_HOME/zed/threads/threads.db` (default `~/.local/share/zed/…`)
//!   * **Windows:** `%LOCALAPPDATA%\Zed\threads\threads.db`
//!
//! ## Schema (base table + accreted `ALTER TABLE` columns; probe before SELECTing)
//! ```sql
//! CREATE TABLE threads (
//!   id TEXT PRIMARY KEY, summary TEXT NOT NULL, updated_at TEXT NOT NULL,
//!   data_type TEXT NOT NULL, data BLOB NOT NULL,
//!   -- added later, may be absent in old DBs:
//!   parent_id TEXT, worktree_branch TEXT, folder_paths TEXT, folder_paths_order TEXT,
//!   created_at TEXT );
//! ```
//! `data_type` is `"zstd"` (blob = one zstd frame of JSON; Zed writes level 3) or `"json"` (raw).
//! `folder_paths` = workspace folders, lexicographically sorted, `\n`-joined;
//! `folder_paths_order` = `,`-joined indices restoring the user's original order.
//! Timestamps are RFC3339 with offset. `parent_id` links a **subagent** thread to its parent.
//!
//! ## Blob JSON — three generations, sniffed by `version` + message shape
//! * **legacy / agent1** (`version` `"0.1.0"`/`"0.2.0"`, or absent in the oldest files):
//!   `{version, summary, updated_at, messages:[{id:int, role:"user"|"assistant"|"system",
//!   segments:[{type:"text"|"thinking"|"RedactedThinking", …}], tool_uses:[{id,name,input}],
//!   tool_results:[{tool_use_id,is_error,content,output}], context:"…", creases:[…],
//!   is_hidden}], initial_project_snapshot, cumulative_token_usage, request_token_usage,
//!   detailed_summary_state, model:{provider,model}, completion_mode, profile, …}`.
//!   The very oldest (versionless) messages carry a plain `text` field instead of `segments`.
//!   In 0.1.0 tool results rode on the *next user* message; 0.2.0 moved them onto the assistant
//!   message that called the tool. We split them into a `Role::Tool` turn either way, so both
//!   variants converge.
//! * **modern / agent2 ACP** (`version` `"0.3.0"`): `{title, messages:[…], updated_at,
//!   detailed_summary, initial_project_snapshot, model, profile, subagent_context:
//!   {parent_thread_id, depth}, imported, …}` where each message is an *externally tagged* enum:
//!   `{"User":{id, content:[{"Text":…}|{"Mention":{uri,content}}|{"Image":{…}}]}}`,
//!   `{"Agent":{content:[{"Text":…}|{"Thinking":{text,signature}}|{"RedactedThinking":…}
//!   |{"ToolUse":{id,name,input,…}}], tool_results:{<id>:{tool_use_id,tool_name,is_error,
//!   content,output}}, reasoning_details}}`, `"Resume"`, or `{"Compaction":{…}}`.
//!
//! cwd = `initial_project_snapshot.worktree_snapshots[0].worktree_path` (falls back to the
//! `folder_paths` column); the same snapshot's `git_state` provides [`GitInfo`]. Zed records **no
//! per-message timestamps** — only thread-level created/updated.
//!
//! ## Fidelity caveats
//! * Multi-root workspaces: we take the first worktree as `cwd` (extra worktrees are only in the
//!   project snapshot, which we don't replicate into the IR).
//! * Legacy `context` (the rendered attached-files preamble) is preserved in `extra["zed"]`;
//!   modern `Mention` content (an attached file's body) is reduced to its URI as a [`Block::File`].
//! * `request_token_usage` (per-request usage) has no per-message home — only the thread-level
//!   `cumulative_token_usage` is kept (in `extra["zed"]`). A subagent's `parent_thread_id` is
//!   first-class [`Lineage::parent`]; the `Compaction` summary becomes a `CompactionSummary` turn.

mod record;

use super::store::sqlite::{open_ro, Cell, Cells, Select};
use super::wire::anthropic;
use super::Adapter;
use crate::ir::*;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Zed {
    /// `threads.db`, if it exists on disk.
    db: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Zed = Zed { db: None };

impl Zed {
    pub fn detect() -> Self {
        Zed { db: threads_db() }
    }

    /// Read an explicit directory holding `threads.db`, without changing process discovery.
    pub fn with_root(root: &Path) -> Self {
        let path = root.join("threads.db");
        Zed {
            db: path.is_file().then_some(path),
        }
    }
}

/// Resolve `…/Zed/threads/threads.db` across platforms. Returns it only if it exists, so discovery
/// degrades to empty on machines without Zed.
fn threads_db() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // XDG override first (portable, and what Zed's `paths::data_dir` honours on Linux).
    if let Some(xdg) = crate::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg).join("zed/threads/threads.db"));
    }
    if let Some(h) = crate::env::home_dir() {
        candidates.push(h.join("Library/Application Support/Zed/threads/threads.db"));
        candidates.push(h.join(".local/share/zed/threads/threads.db"));
        // Flatpak install keeps the same layout under the app sandbox.
        candidates.push(h.join(".var/app/dev.zed.Zed/data/zed/threads/threads.db"));
    }
    if let Some(local) = crate::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Zed").join("threads").join("threads.db"));
    }
    candidates.into_iter().find(|p| p.exists())
}

impl Adapter for Zed {
    fn harness(&self) -> Harness {
        Harness::Zed
    }

    /// The `threads/` dir holding `threads.db` (a root is a directory; the db itself is a
    /// catalog file-watch, see `catalog::watches_for`).
    fn roots(&self) -> Vec<PathBuf> {
        self.db
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(db) = &self.db else {
            return Ok(Default::default());
        };
        // The one database: one that cannot open or read is skipped and reported (the harness
        // lists nothing), not a discovery failure that keeps the previous listing.
        let mut found = crate::Discovered::default();
        let refs = super::store::sqlite::cached_discover(db, || open_ro(db)?.read(|conn, _| discover_db(conn, db)));
        if let Some(refs) = found.keep(db, refs) {
            found.refs = refs;
        }
        Ok(found)
    }

    /// Every thread's certificate ([`rows_conn`]).
    fn rows(&self, store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
        open_ro(store).ok()?.read(|conn, _| rows_conn(conn)).ok()
    }

    /// Zed writes `threads.db` in place.
    fn watch_policy(&self, _r: &SessionRef) -> super::WatchPolicy {
        super::WatchPolicy::Database
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let conn = open_ro(&r.path)?;
            conn.read(|conn, reads| {
                let cells = Cells::new(conn, &r.path, reads, "threads")?;
                stream_thread(conn, r, sink, &cells, opts.spans)
            })
        })
    }
}

fn discover_db(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let select = Select::of(conn, "threads")
        .always(&["id", "summary", "updated_at"])
        .probed(&["created_at", "folder_paths", "folder_paths_order"])
        .always(&["data_type"]);
    // Sorting only metadata keeps every complete blob out of SQLite's temporary sorter; each
    // thread's cell is then read by key.
    let sql = format!("SELECT {} FROM threads ORDER BY updated_at DESC", select.list());
    // A thread whose row certifies as it did when its listing was last decoded keeps that
    // listing: a save decodes the saved thread, not every thread ([`Listings`]).
    let certificates = rows_conn(conn)?;
    let mut pass = super::store::sqlite::listing::Pass::<ThreadCwd>::begin(Harness::Zed, db);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        // A row without a text id is not an addressable thread (nothing could open it); the
        // rest of the row's columns are optional facts.
        let id = row.get::<_, String>(0)?;
        let summary = row.get::<_, Option<String>>(1)?;
        let updated = row.get::<_, Option<String>>(2)?;
        let created = row.get::<_, Option<String>>(3)?;
        let folders = row.get::<_, Option<String>>(4)?;
        let order = row.get::<_, Option<String>>(5)?;
        let data_type = row.get::<_, Option<String>>(6)?;
        let ThreadCwd(cwd) = pass.list(&id, certificates.get(&id).copied(), || -> Result<ThreadCwd> {
            // Each thread's cells are its own: the decoded blob is released before the next.
            let cells = Cells::new(conn, db, &crate::source::ReadSet::default(), "threads")?;
            Ok(ThreadCwd(
                thread_cell(conn, &cells, &id, data_type.as_deref())?
                    .map(|cell| cells.read(cell, listing))
                    .transpose()?
                    .flatten()
                    .flatten(),
            ))
        })?;
        out.push(SessionRef {
            cwd: cwd.or_else(|| first_folder_path(folders.as_deref(), order.as_deref())),
            title: summary
                .filter(|s| !s.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE)),
            created_at: created.as_deref().and_then(super::parse_ts),
            updated_at: updated.as_deref().and_then(super::parse_ts),
            message_count: None,
            ..SessionRef::new(id, Harness::Zed, db.to_path_buf())
        });
    }
    drop(rows);
    drop(stmt);
    pass.keep();
    Ok(out)
}

/// A thread's listing cwd: its decoded blob's snapshot cwd ([`listing`]).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ThreadCwd(Option<PathBuf>);

impl super::store::sqlite::listing::Listing for ThreadCwd {
    const TAG: &'static str = "zed-listing";
}

/// Every thread's [`Rows`](crate::source::Rows) certificate: its one `threads` row, by rowid (Zed
/// saves a thread by `INSERT OR REPLACE`, a new rowid; a `WITHOUT ROWID` table has none), every
/// column but `data` (`updated_at` moves on every save), and the byte length of the `data` blob
/// from its record header. The thread document is one compressed blob, so this is the whole read
/// short of decoding it.
fn rows_conn(conn: &Connection) -> Result<HashMap<String, crate::source::Rows>> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let rowid = if conn.prepare("SELECT rowid FROM threads LIMIT 0").is_ok() {
        "rowid"
    } else {
        "NULL"
    };
    let mut columns: Vec<String> = super::store::sqlite::columns(conn, "threads")
        .into_iter()
        .filter(|c| c != "data")
        .collect();
    columns.sort();
    let mut certificates = super::store::sqlite::Certificates::default();
    certificates.rows(
        conn,
        &format!(
            "SELECT id, {rowid}, {}, octet_length(data) FROM threads ORDER BY id",
            columns.join(", ")
        ),
    )?;
    Ok(certificates.0)
}

/// Listing needs the snapshot cwd, not a replay of the thread's message bodies.
/// The complete cell is still decoded and validated before this metadata projection.
fn listing(view: crate::json::JsonView<'_>) -> Result<Option<PathBuf>> {
    let (metadata, _) = record::header(view)?;
    Ok(snapshot_cwd(&metadata))
}

/// The thread's `data` cell, decompressed into the cells' arena: `data_type` `"zstd"` is one
/// zstd frame, `"json"` is raw, anything else is sniffed by the zstd magic. None when the row has
/// no nonempty BLOB; an error when it would not decompress.
fn thread_cell(conn: &Connection, cells: &Cells, id: &str, data_type: Option<&str>) -> Result<Option<Cell>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {} FROM threads WHERE id = ?1", cells.select("data")))?;
    let mut rows = stmt.query([id])?;
    let Some(row) = rows.next()? else { return Ok(None) };
    cells.blob(conn, "threads", "data", row, 0, |input, out| {
        let compressed = match data_type {
            Some("zstd") => true,
            Some("json") => false,
            _ => input.fill_buf()?.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
        };
        if compressed {
            std::io::copy(&mut super::store::zst::decoder(input)?, out)?;
        } else {
            std::io::copy(input, out)?;
        }
        Ok(())
    })
}

fn stream_thread(
    conn: &Connection,
    r: &SessionRef,
    sink: &mut dyn MessageSink,
    cells: &Cells,
    spans: bool,
) -> Result<Session> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let select = Select::of(conn, "threads")
        .always(&["summary", "updated_at"])
        .probed(&["created_at", "parent_id", "folder_paths", "folder_paths_order"])
        .always(&["data_type"])
        .probed(&["worktree_branch"]);
    let sql = format!("SELECT {} FROM threads WHERE id = ?1", select.list());
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&r.id])?;
    let row = rows.next()?.with_context(|| format!("zed thread {} not found", r.id))?;
    let summary = row.get::<_, Option<String>>(0).ok().flatten();
    let updated = row.get::<_, Option<String>>(1).ok().flatten();
    let created = row.get::<_, Option<String>>(2).ok().flatten();
    let parent = row.get::<_, Option<String>>(3).ok().flatten();
    let folders = row.get::<_, Option<String>>(4).ok().flatten();
    let order = row.get::<_, Option<String>>(5).ok().flatten();
    let data_type = row.get::<_, Option<String>>(6).ok().flatten();
    let branch = row.get::<_, Option<String>>(7).ok().flatten().filter(|s| !s.is_empty());
    // The whole document is validated before any message is emitted, ignored fields included.
    // A store error is the read's; a blob that will not decompress reads as undecodable below.
    let cell = match thread_cell(conn, cells, &r.id, data_type.as_deref()) {
        Err(error) if error.is::<rusqlite::Error>() => return Err(error),
        cell => cell,
    };
    let thread = match &cell {
        Ok(Some(cell)) => cells.read(*cell, |view| {
            let (metadata, messages) = record::header(view)?;
            Ok((metadata, messages.map(|v| v.to_json()).transpose()?))
        })?,
        _ => None,
    };

    let mut zed = serde_json::Map::new();
    let mut model = None;
    let mut git = None;
    let mut blob_title = None;
    let mut blob_updated = None;
    let mut cwd = None;
    // The parent thread of a subagent: from the 0.3.0 blob's `subagent_context`, overridden by the
    // `parent_id` column when both name one. First-class [`Lineage`], not an extra key.
    let mut parent_thread_id: Option<String> = None;
    if let Some((t, _)) = &thread {
        // `title` (0.3.0) / `summary` (legacy) — same meaning, renamed across generations.
        blob_title = t
            .get("title")
            .or_else(|| t.get("summary"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        blob_updated = t.get("updated_at").and_then(super::ts_from_value);
        cwd = snapshot_cwd(t);
        git = snapshot_git(t);
        let (name, provider) = t.get("model").map(model_and_provider).unwrap_or_default();
        model = name;
        if let Some(p) = provider {
            zed.insert("provider".into(), Value::String(p));
        }
        thread_extras(t, &mut zed, &mut parent_thread_id);
    } else if !matches!(cell, Ok(None)) {
        // The row has a blob that wouldn't decode — surface that instead of erroring, so one
        // corrupt thread never breaks listings or bulk exports.
        zed.insert("undecodable_blob".into(), Value::Bool(true));
    }
    let undecodable = zed.contains_key("undecodable_blob");
    if let Some(branch) = branch {
        git.get_or_insert_with(GitInfo::default).branch.get_or_insert(branch);
    }
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        parent_thread_id = Some(p);
    }

    let mut s = Session {
        cwd: cwd
            .or_else(|| first_folder_path(folders.as_deref(), order.as_deref()))
            .or_else(|| r.cwd.clone()),
        title: blob_title
            .or_else(|| summary.filter(|s| !s.trim().is_empty()))
            .or_else(|| r.title.clone()),
        created_at: created.as_deref().and_then(super::parse_ts).or(r.created_at),
        updated_at: blob_updated
            .or_else(|| updated.as_deref().and_then(super::parse_ts))
            .or(r.updated_at),
        model,
        git,
        source_path: Some(r.path.clone()),
        lineage: Lineage {
            parent: parent_thread_id,
            ..Lineage::default()
        },
        ..Session::new(r.id.clone(), Harness::Zed)
    };
    if !zed.is_empty() {
        let bag = s.harness_extra_mut(Harness::Zed);
        for (k, v) in zed {
            bag.insert(k, v);
        }
    }
    // The thread's one record, undecodable, is counted as well as marked.
    if undecodable {
        super::Unparsed(1).note(&mut s);
    }
    // All session metadata is known up front; hand it to the sink before the body.
    sink.meta(&s);
    if let Some((_, Some(messages))) = &thread {
        messages.with_view(|view| record::messages(view, cells.out().0, spans, &mut |m| sink.message(m)))?;
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Thread-level metadata
// ---------------------------------------------------------------------------

/// `{provider, model}` → the model, and the provider beside it (`extra["zed"]["provider"]`): a
/// thread that names only its provider has no model.
fn model_and_provider(v: &Value) -> (Option<String>, Option<String>) {
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    (field("model"), field("provider"))
}

/// First worktree path of `initial_project_snapshot` — the cwd the thread was created against.
fn snapshot_cwd(t: &Value) -> Option<PathBuf> {
    t.pointer("/initial_project_snapshot/worktree_snapshots/0/worktree_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Git state of the first worktree snapshot.
fn snapshot_git(t: &Value) -> Option<GitInfo> {
    let g = t.pointer("/initial_project_snapshot/worktree_snapshots/0/git_state")?;
    let pick = |k: &str| {
        g.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let info = GitInfo {
        branch: pick("current_branch"),
        commit: pick("head_sha"),
        remote: pick("remote_url"),
        ..Default::default()
    };
    (info.branch.is_some() || info.commit.is_some() || info.remote.is_some()).then_some(info)
}

/// First *ordered* workspace folder from the `folder_paths` (`\n`-joined, lexicographic) +
/// `folder_paths_order` (`,`-joined original indices) columns.
fn first_folder_path(folders: Option<&str>, order: Option<&str>) -> Option<PathBuf> {
    let folders = folders.filter(|s| !s.is_empty())?;
    let paths: Vec<&str> = folders.split('\n').collect();
    let first = order
        .and_then(|o| o.split(',').next())
        .and_then(|ix| ix.trim().parse::<usize>().ok())
        .and_then(|ix| paths.get(ix))
        .copied()
        .or_else(|| paths.first().copied())?;
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// Session-level facts shared by both generations, written into `extra["zed"]`: profile,
/// completion mode, detailed summary, cumulative token usage, subagent depth, imported flag, and
/// the raw format version. A subagent's `parent_thread_id` is first-class lineage, so it is
/// returned via `parent` rather than stashed in the bag.
fn thread_extras(t: &Value, extra: &mut serde_json::Map<String, Value>, parent: &mut Option<String>) {
    if let Some(v) = t.get("version").and_then(Value::as_str) {
        extra.insert("thread_version".into(), Value::String(v.to_string()));
    }
    for key in ["profile", "completion_mode"] {
        if let Some(v) = t.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            extra.insert(key.into(), Value::String(v.to_string()));
        }
    }
    // Legacy `detailed_summary_state: {"Generated": {"text": …}}` / modern `detailed_summary: "…"`.
    let detailed = t
        .pointer("/detailed_summary_state/Generated/text")
        .or_else(|| t.get("detailed_summary"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    if let Some(d) = detailed {
        extra.insert("detailed_summary".into(), Value::String(d.to_string()));
    }
    if let Some(u) = t.get("cumulative_token_usage").and_then(Value::as_object) {
        if u.values().any(|v| v.as_u64().unwrap_or(0) > 0) {
            extra.insert("cumulative_token_usage".into(), Value::Object(u.clone()));
        }
    }
    // 0.3.0 subagent threads: {parent_thread_id, depth}.
    if let Some(sub) = t.get("subagent_context").filter(|v| !v.is_null()) {
        if let Some(p) = sub
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *parent = Some(p.to_string());
        }
        if let Some(d) = sub.get("depth").and_then(Value::as_u64) {
            extra.insert("subagent_depth".into(), Value::Number(d.into()));
        }
    }
    if t.get("imported").and_then(Value::as_bool) == Some(true) {
        extra.insert("imported".into(), Value::Bool(true));
    }
}

/// Whole-`Session` convenience over [`stream_thread`] for tests: stream into a CollectSink.
#[cfg(test)]
fn parse_thread(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let cells = Cells::new(
        conn,
        &tests::db_path(conn),
        &crate::source::ReadSet::default(),
        "threads",
    )?;
    let mut s = stream_thread(conn, r, &mut sink, &cells, false)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    /// The current full schema (base table + every ALTER-added column).
    const SCHEMA: &str = "
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            data_type TEXT NOT NULL,
            data BLOB NOT NULL,
            parent_id TEXT,
            worktree_branch TEXT,
            folder_paths TEXT,
            folder_paths_order TEXT,
            created_at TEXT
        );
    ";

    /// The original schema, before any ALTER TABLE (no parent_id/folder_paths/created_at).
    const SCHEMA_OLD: &str = "
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            data_type TEXT NOT NULL,
            data BLOB NOT NULL
        );
    ";

    /// The current schema with the table as WITHOUT ROWID — the shape production must also read.
    fn without_rowid_schema() -> String {
        SCHEMA.replace("        );", "        ) WITHOUT ROWID;")
    }

    /// A file-backed store (a cell's byte source is a file) that lives as long as the value.
    struct Db {
        conn: Connection,
        _world: cv_testkit::World,
    }
    impl std::ops::Deref for Db {
        type Target = Connection;
        fn deref(&self) -> &Connection {
            &self.conn
        }
    }

    fn mk(schema: &str) -> Db {
        let world = cv_testkit::World::new("zed-db");
        let c = Connection::open(world.base.join("threads.db")).unwrap();
        c.execute_batch(schema).unwrap();
        Db { conn: c, _world: world }
    }

    pub(super) fn db_path(conn: &Connection) -> PathBuf {
        PathBuf::from(conn.path().expect("a file-backed test store"))
    }

    fn discover(conn: &Connection) -> Vec<SessionRef> {
        discover_db(conn, &db_path(conn)).unwrap()
    }

    fn zstd_blob(json: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(json).unwrap();
        zstd::encode_all(bytes.as_slice(), 1).unwrap()
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Zed, PathBuf::from(":memory:"))
    }

    /// A representative legacy (0.2.0) thread: user text, assistant thinking+text+tool call with
    /// its result on the same message, project snapshot, model, profile.
    pub(super) fn legacy_thread() -> Value {
        serde_json::json!({
            "version": "0.2.0",
            "summary": "Fix the flaky test",
            "updated_at": "2025-08-08T21:08:04.712610Z",
            "messages": [
                {
                    "id": 0, "role": "user",
                    "segments": [{"type": "text", "text": "why is test_foo flaky?"}],
                    "tool_uses": [], "tool_results": [],
                    "context": "<context>attached file foo.rs</context>",
                    "creases": [], "is_hidden": false
                },
                {
                    "id": 1, "role": "assistant",
                    "segments": [
                        {"type": "thinking", "text": "let me look", "signature": "sig1"},
                        {"type": "text", "text": "Checking the test now."}
                    ],
                    "tool_uses": [{"id": "toolu_1", "name": "read_file", "input": {"path": "foo.rs"}}],
                    "tool_results": [{
                        "tool_use_id": "toolu_1", "is_error": false,
                        "content": {"Text": "fn test_foo() {}"},
                        "output": {"lines": 1}
                    }],
                    "context": "", "creases": [], "is_hidden": false
                },
                {
                    "id": 2, "role": "assistant",
                    "segments": [{"type": "text", "text": "It races on the shared tempdir."}],
                    "tool_uses": [], "tool_results": [],
                    "context": "", "creases": [], "is_hidden": false
                }
            ],
            "initial_project_snapshot": {
                "worktree_snapshots": [{
                    "worktree_path": "/Users/u/proj",
                    "git_state": {
                        "remote_url": "git@github.com:u/proj",
                        "head_sha": "abc123",
                        "current_branch": "main",
                        "diff": ""
                    }
                }]
            },
            "cumulative_token_usage": {"input_tokens": 10, "output_tokens": 20},
            "request_token_usage": [],
            "detailed_summary_state": {"Generated": {"text": "We fixed a race."}},
            "model": {"provider": "zed.dev", "model": "claude-sonnet-4-thinking"},
            "completion_mode": "burn",
            "tool_use_limit_reached": false,
            "profile": "write"
        })
    }

    /// A representative modern (0.3.0) thread: tagged-enum messages, Mention, tool_results map,
    /// Resume marker, subagent_context.
    pub(super) fn modern_thread() -> Value {
        serde_json::json!({
            "version": "0.3.0",
            "title": "Refactor the parser",
            "updated_at": "2026-06-10T19:57:44.766848Z",
            "messages": [
                {"User": {"id": "um-1", "content": [
                    {"Text": "split this module"},
                    {"Mention": {"uri": "file:///Users/u/proj/src/lib.rs", "content": "mod a;"}}
                ]}},
                "Resume",
                {"Agent": {
                    "content": [
                        {"Thinking": {"text": "plan the split", "signature": null}},
                        {"Text": "Splitting now."},
                        {"ToolUse": {"id": "toolu_9", "name": "edit_file", "raw_input": "{}",
                                     "input": {"path": "src/lib.rs"}, "is_input_complete": true}}
                    ],
                    "tool_results": {
                        "toolu_9": {"tool_use_id": "toolu_9", "tool_name": "edit_file",
                                     "is_error": true, "content": {"Text": "permission denied"},
                                     "output": null}
                    },
                    "reasoning_details": null
                }}
            ],
            "detailed_summary": null,
            "initial_project_snapshot": {
                "worktree_snapshots": [{
                    "worktree_path": "/Users/u/proj",
                    "git_state": {"remote_url": null, "head_sha": null, "current_branch": "dev", "diff": ""}
                }]
            },
            "cumulative_token_usage": {"input_tokens": 0, "output_tokens": 0},
            "request_token_usage": {},
            "model": {"provider": "openai", "model": "gpt-5-mini"},
            "profile": "write",
            "imported": false,
            "subagent_context": {"parent_thread_id": "parent-1", "depth": 1}
        })
    }

    fn insert(conn: &Connection, id: &str, summary: &str, data_type: &str, data: &[u8]) {
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, folder_paths, folder_paths_order, created_at) \
             VALUES (?1, ?2, '2025-08-08T21:08:04.712610+00:00', ?3, ?4, NULL, NULL, '2025-08-01T00:00:00+00:00')",
            rusqlite::params![id, summary, data_type, data],
        )
        .unwrap();
    }

    #[test]
    fn discover_reads_metadata_without_decoding_messages() {
        let c = mk(SCHEMA);
        insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
        // A second, newer thread with workspace folders but an empty summary.
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, folder_paths, folder_paths_order, created_at) \
             VALUES ('t2', '', '2026-06-10T19:57:44.766848+00:00', 'zstd', ?1, \
                     '/Users/u/aaa' || char(10) || '/Users/u/proj', '1,0', '2026-06-10T19:57:44.766848+00:00')",
            [zstd_blob(&modern_thread())],
        )
        .unwrap();

        record::DECODED_MESSAGES.with(|n| n.set(0));
        let refs = discover(&c);
        assert_eq!(record::DECODED_MESSAGES.with(|n| n.get()), 0);
        assert_eq!(refs.len(), 2);
        // Ordered by updated_at DESC.
        assert_eq!(refs[0].id, "t2");
        assert_eq!(refs[1].id, "t1");

        let t1 = &refs[1];
        assert_eq!(t1.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(t1.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert!(t1.created_at.unwrap() < t1.updated_at.unwrap());
        assert_eq!(t1.message_count, None, "exact counts come from complete reads");

        let t2 = &refs[0];
        assert_eq!(t2.title, None, "empty summary must not become a title");
        // Snapshot cwd wins over folder_paths.
        assert_eq!(t2.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(t2.message_count, None);
        for reference in &refs {
            parse_thread(&c, reference).unwrap();
        }
        assert!(
            record::DECODED_MESSAGES.with(|n| n.get()) > 0,
            "full reads still decode messages"
        );
    }

    #[test]
    fn complete_reads_refill_counts_and_database_changes_invalidate_them() -> Result<()> {
        let world = crate::test_world("zed-count-refill");
        let db = world.base.join("threads.db");
        {
            let writer = Connection::open(&db)?;
            writer.execute_batch(SCHEMA)?;
            insert(&writer, "thread", "Native title", "zstd", &zstd_blob(&legacy_thread()));
        }
        let adapter = Zed::with_root(&world.base);
        let reference = adapter.discover()?.remove(0);
        assert_eq!(reference.message_count, None);
        crate::catalog::sync(std::slice::from_ref(&reference));
        let complete = crate::stream::stream_counted(
            &adapter,
            &reference,
            &Demand::FACTS.with_spans(),
            &mut crate::stream::Discard,
            crate::stream::Pass::RECORD,
        )?;
        let turns = complete.counted.unwrap().turns;
        assert_eq!(turns, 3);
        record::DECODED_MESSAGES.with(|n| n.set(0));
        assert_eq!(adapter.discover()?[0].message_count, Some(turns));
        assert_eq!(
            record::DECODED_MESSAGES.with(|n| n.get()),
            0,
            "count reuse cannot replay bodies"
        );
        let stored = crate::catalog::Reader::open().unwrap().all_sessions().unwrap();
        assert_eq!(stored[0].message_count, Some(turns));
        // A live WAL changes the source even before the database file is checkpointed.
        let writer = Connection::open(&db)?;
        writer.execute_batch("PRAGMA journal_mode=WAL;")?;
        let mut changed = legacy_thread();
        changed["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role":"user","text":"next turn"}));
        writer.execute("UPDATE threads SET data=?1 WHERE id='thread'", [zstd_blob(&changed)])?;
        let unknown = adapter.discover()?.remove(0);
        assert_eq!(unknown.message_count, None);
        assert_eq!(unknown.cwd, reference.cwd);
        assert_eq!(unknown.title, reference.title);
        assert_eq!(record::DECODED_MESSAGES.with(|n| n.get()), 0);
        let complete = crate::stream::stream_counted(
            &adapter,
            &unknown,
            &Demand::FACTS.with_spans(),
            &mut crate::stream::Discard,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(complete.counted.unwrap().turns, 4);
        assert_eq!(adapter.discover()?[0].message_count, Some(4));
        Ok(())
    }

    #[test]
    fn parses_legacy_thread() {
        let c = mk(SCHEMA);
        insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
        let s = parse_thread(&c, &sref("t1")).unwrap();
        // Every Zed thread fact is nested under `extra["zed"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);

        assert_eq!(s.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-thinking"));
        let git = s.git.as_ref().unwrap();
        assert_eq!(git.branch.as_deref(), Some("main"));
        assert_eq!(git.commit.as_deref(), Some("abc123"));
        assert_eq!(git.remote.as_deref(), Some("git@github.com:u/proj"));
        // Blob updated_at preferred; created_at from the column.
        assert!(s.created_at.is_some() && s.updated_at.is_some());
        let bag = s.harness_extra(Harness::Zed).expect("zed bag");
        assert_eq!(bag.get("profile").and_then(Value::as_str), Some("write"));
        assert_eq!(bag.get("completion_mode").and_then(Value::as_str), Some("burn"));
        assert_eq!(bag.get("thread_version").and_then(Value::as_str), Some("0.2.0"));
        assert_eq!(
            bag.get("detailed_summary").and_then(Value::as_str),
            Some("We fixed a race.")
        );
        assert!(bag.contains_key("cumulative_token_usage"));
        // Nested only: no flat facts leaked to the top level.
        for k in s.extra.keys() {
            assert!(
                k == "zed" || k == "_record",
                "unexpected top-level session extra key {k}"
            );
        }

        // user, assistant(+tool call), Tool, assistant.
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("why is test_foo flaky?"));
        assert_eq!(
            s.messages[0]
                .harness_extra(Harness::Zed)
                .and_then(|b| b.get("context"))
                .and_then(Value::as_str),
            Some("<context>attached file foo.rs</context>")
        );
        assert!(
            !s.messages[0].extra.contains_key("context"),
            "context nests under the zed bag"
        );
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::Reply, Origin::Model)
        );
        assert_eq!(
            (s.messages[2].kind, s.messages[2].origin),
            (MessageKind::ToolResult, Origin::Harness)
        );
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(&a.content[0], Block::Thinking { text, signature, .. }
            if text.matches_str("let me look") == Some(true) && signature.as_deref() == Some("sig1")));
        assert!(
            matches!(&a.content[1], Block::Text { text , .. } if text.matches_str("Checking the test now.") == Some(true))
        );
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, input , ..}
            if id == "toolu_1" && name == "read_file"
            && input.resolve().unwrap().get("path").and_then(Value::as_str) == Some("foo.rs")));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, details, .. }
            if tool_use_id == "toolu_1" && content.matches_str("fn test_foo() {}") == Some(true) && !is_error
            && tool_name.as_deref() == Some("read_file")
            && details.as_ref().and_then(|d| d.get("lines")).and_then(Value::as_i64) == Some(1))
        );
        assert_eq!(s.messages[3].text().as_deref(), Some("It races on the shared tempdir."));
    }

    #[test]
    fn parses_modern_thread() {
        let c = mk(SCHEMA);
        insert(&c, "t2", "", "zstd", &zstd_blob(&modern_thread()));
        let s = parse_thread(&c, &sref("t2")).unwrap();
        crate::harness::assert_no_flat_keys(&s);

        assert_eq!(s.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(s.model.as_deref(), Some("gpt-5-mini"), "the model is the model");
        assert_eq!(s.harness_extra(Harness::Zed).unwrap()["provider"], "openai");
        // A thread naming only its provider has no model; the provider is never promoted to one.
        assert_eq!(
            model_and_provider(&serde_json::json!({"provider": "zed.dev", "model": ""})),
            (None, Some("zed.dev".into()))
        );
        assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("dev"));
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(
            s.harness_extra(Harness::Zed)
                .and_then(|b| b.get("subagent_depth"))
                .and_then(Value::as_u64),
            Some(1)
        );
        // All-zero usage is not stashed.
        assert!(s
            .harness_extra(Harness::Zed)
            .is_none_or(|b| !b.contains_key("cumulative_token_usage")));

        // user, assistant, Tool ("Resume" skipped).
        assert_eq!(s.messages.len(), 3);
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!(u.id.as_deref(), Some("um-1"));
        assert_eq!(u.text().as_deref(), Some("split this module"));
        assert!(matches!(&u.content[1], Block::File { source: Some(uri), .. }
            if uri == "file:///Users/u/proj/src/lib.rs"));
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(
            matches!(&a.content[0], Block::Thinking { text, .. } if text.matches_str("plan the split") == Some(true))
        );
        assert!(matches!(&a.content[1], Block::Text { text , .. } if text.matches_str("Splitting now.") == Some(true)));
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, .. } if id == "toolu_9" && name == "edit_file"));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, .. }
            if tool_use_id == "toolu_9" && content.matches_str("permission denied") == Some(true) && *is_error
            && tool_name.as_deref() == Some("edit_file"))
        );
    }

    #[test]
    fn v010_tool_results_on_next_user_message_become_a_tool_turn() {
        // 0.1.0 carried tool_results on the user message *after* the assistant call.
        let thread = serde_json::json!({
            "version": "0.1.0",
            "summary": "old",
            "updated_at": "2025-05-01T00:00:00Z",
            "messages": [
                {"id": 0, "role": "assistant",
                 "segments": [{"type": "text", "text": "calling"}],
                 "tool_uses": [{"id": "c1", "name": "shell", "input": {"cmd": "ls"}}],
                 "tool_results": []},
                {"id": 1, "role": "user", "segments": [],
                 "tool_uses": [],
                 "tool_results": [{"tool_use_id": "c1", "is_error": false,
                                    "content": {"Text": "file_a"}, "output": null}]}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "old", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 2, "no empty user turn; results become a Tool turn");
        assert_eq!(s.messages[0].role, Role::Assistant);
        assert_eq!(s.messages[1].role, Role::Tool);
        assert!(
            matches!(&s.messages[1].content[0], Block::ToolResult { tool_name, content, .. }
            if tool_name.as_deref() == Some("shell") && content.matches_str("file_a") == Some(true))
        );
    }

    #[test]
    fn versionless_text_messages_parse() {
        // The oldest blobs have no `version` and messages carry `text` instead of `segments`.
        let thread = serde_json::json!({
            "summary": "ancient",
            "updated_at": "2025-03-01T00:00:00Z",
            "messages": [
                {"id": 0, "role": "user", "text": "hello from the past"},
                {"id": 1, "role": "assistant", "text": "still parses"}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "ancient", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello from the past"));
        assert_eq!(s.messages[1].text().as_deref(), Some("still parses"));
        assert!(s
            .harness_extra(Harness::Zed)
            .and_then(|b| b.get("thread_version"))
            .is_none());
    }

    #[test]
    fn metadata_sort_preserves_ties_and_without_rowid_fallback() {
        for schema in [SCHEMA.to_string(), without_rowid_schema()] {
            let c = mk(&schema);
            let data = zstd_blob(&legacy_thread());
            for id in ["z", "a", "m"] {
                insert(&c, id, id, "zstd", &data);
            }
            let expected: Vec<String> = c
                .prepare("SELECT id, data FROM threads ORDER BY updated_at DESC")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let actual = discover(&c);
            assert_eq!(
                actual.iter().map(|r| &r.id).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            for r in actual {
                assert_eq!(r.message_count, None);
                assert_eq!(parse_thread(&c, &r).unwrap().messages.len(), 4);
            }
            for text in [
                "not a blob".to_string(),
                serde_json::to_string(&legacy_thread()).unwrap(),
            ] {
                c.execute(
                    "UPDATE threads SET data = ?1, data_type = 'json' WHERE id = 'a'",
                    [&text],
                )
                .unwrap();
                assert_eq!(
                    c.query_row("SELECT typeof(data) FROM threads WHERE id = 'a'", [], |row| row
                        .get::<_, String>(0))
                        .unwrap(),
                    "text"
                );
                let unreadable = parse_thread(&c, &sref("a")).unwrap();
                assert!(unreadable.messages.is_empty());
                assert!(unreadable.harness_extra(Harness::Zed).is_none());
                let listed = discover(&c).into_iter().find(|r| r.id == "a").unwrap();
                assert_eq!(listed.message_count, None);
                assert_eq!(listed.cwd, None);
            }
        }
    }

    #[test]
    fn read_snapshot_allows_wal_writer_and_keeps_both_passes_consistent() {
        let world = cv_testkit::World::new("zed-snapshot");
        let path = world.base.join("threads.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute_batch(SCHEMA).unwrap();
        insert(&writer, "t", "old", "zstd", &zstd_blob(&legacy_thread()));
        let reader = open_ro(&path).unwrap();
        struct UpdatingSink<'a> {
            writer: &'a Connection,
            messages: Vec<Message>,
        }
        impl MessageSink for UpdatingSink<'_> {
            fn meta(&mut self, _: &Session) {
                self.writer
                    .execute(
                        "UPDATE threads SET data = ?1 WHERE id = 't'",
                        [zstd_blob(&modern_thread())],
                    )
                    .unwrap();
                self.writer
                    .execute(
                        "UPDATE threads SET summary = ?1 WHERE id = 't'",
                        [uuid::Uuid::new_v4().to_string()],
                    )
                    .unwrap();
            }
            fn message(&mut self, message: Message) -> Flow {
                self.messages.push(message);
                Flow::Continue
            }
        }
        let mut sink = UpdatingSink {
            writer: &writer,
            messages: Vec::new(),
        };
        let cells = Cells::new(&reader, &path, &crate::source::ReadSet::default(), "threads").unwrap();
        let session = stream_thread(&reader, &sref("t"), &mut sink, &cells, false).unwrap();
        assert_eq!(session.model.as_deref(), Some("claude-sonnet-4-thinking"));
        assert_eq!(sink.messages.len(), 4);
        assert_eq!(sink.messages[0].text().as_deref(), Some("why is test_foo flaky?"));
        // A live WAL store is read through a private snapshot (cv never touches its -shm), so
        // even an unchecked second pass sees the snapshot, not the writer's later commit.
        assert_eq!(
            parse_thread(&reader, &sref("t")).unwrap().model.as_deref(),
            Some("claude-sonnet-4-thinking")
        );
        // The internal SQLite snapshot remains consistent, but publication through the real
        // adapter must reject a source changed by a callback rather than recapture its identity.
        assert!(reader.read(|conn, _| parse_thread(conn, &sref("t"))).is_err());
        let adapter = Zed::with_root(&world.base);
        let mut reference = sref("t");
        reference.path = path;
        let failure = adapter.stream(&reference, &Demand::LEAN, &mut sink).unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"));
    }

    #[test]
    fn json_data_type_rows_parse_uncompressed() {
        let c = mk(SCHEMA);
        let raw = serde_json::to_vec(&legacy_thread()).unwrap();
        insert(&c, "t", "json row", "json", &raw);
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 4);
    }

    #[test]
    fn corrupt_blob_is_reported_by_discovery_and_annotated_by_parse() {
        let c = mk(SCHEMA);
        insert(&c, "bad", "corrupt one", "zstd", b"\x28\xb5\x2f\xfdnot really zstd");
        insert(&c, "good", "fine one", "zstd", &zstd_blob(&legacy_thread()));

        assert!(
            discover_db(&c, &db_path(&c)).is_err(),
            "a failed projection is not an empty cwd"
        );

        // parse returns the row's metadata + a flag, never an error.
        let s = parse_thread(&c, &sref("bad")).unwrap();
        assert!(s.messages.is_empty());
        assert_eq!(
            s.harness_extra(Harness::Zed).and_then(|b| b.get("undecodable_blob")),
            Some(&Value::Bool(true))
        );
        assert_eq!(s.extra["cv"]["skipped_lines"], 1);
        assert_eq!(s.title.as_deref(), Some("corrupt one"));
    }

    /// A document that is not exactly one JSON value (invalid UTF-8 in an ignored field, a
    /// truncated zstd frame, trailing bytes) is undecodable as a whole: no prefix is emitted.
    #[test]
    fn invalid_documents_never_emit_a_prefix() {
        let c = mk(SCHEMA);
        let json = b"{\"messages\":[{\"role\":\"user\",\"text\":\"ok\"}],\"ignored\":\"\xff\"}";
        let valid = br#"{"messages":[{"role":"user","text":"ok"}]}"#;
        let truncated = zstd::encode_all(valid.as_slice(), 1).unwrap();
        let cases: [(&str, &str, Vec<u8>); 4] = [
            ("utf8", "json", json.to_vec()),
            ("utf8z", "zstd", zstd::encode_all(json.as_slice(), 1).unwrap()),
            ("cut", "zstd", truncated[..truncated.len() - 1].to_vec()),
            (
                "trailing",
                "json",
                br#"{"messages":[{"role":"user","text":"ok"}]} { }"#.to_vec(),
            ),
        ];
        for (id, data_type, data) in &cases {
            insert(&c, id, id, data_type, data);
        }
        assert!(discover_db(&c, &db_path(&c)).is_err());
        for (id, _, _) in &cases {
            let s = parse_thread(&c, &sref(id)).unwrap();
            assert!(s.messages.is_empty(), "{id}");
            assert_eq!(
                s.harness_extra(Harness::Zed).and_then(|b| b.get("undecodable_blob")),
                Some(&Value::Bool(true))
            );
        }
    }

    /// Long text, thinking, tool arguments and a generated result stay in the decoded cell when
    /// spans are demanded, outlive the read and are rejected once the store changes.
    #[test]
    fn long_payloads_stay_in_the_cell_and_the_store_guards_them() {
        let world = cv_testkit::World::new("zed-cell-guard");
        let path = world.base.join("threads.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute_batch(SCHEMA).unwrap();
        let large = "雪\n\"\\".repeat(2 * crate::lazy::INLINE_MAX);
        let value = serde_json::json!({"messages":[
            {"role":"user","text":large},
            {"Agent":{"content":[{"Thinking":{"text":large}},{"ToolUse":{"id":"t","name":"shell","input":{"body":large}}}],
                "tool_results":{"t":{"tool_use_id":"t","content":{"b":large,"a":1}}}}},
        ]});
        insert(&writer, "t", "t", "zstd", &zstd_blob(&value));
        let zed = Zed::with_root(&world.base);
        let mut reference = sref("t");
        reference.path = path;
        let full = crate::stream::collect_with(&zed, &reference, &Demand::RICH).unwrap();
        let lazy = crate::stream::collect_with(&zed, &reference, &Demand::LEAN.with_spans()).unwrap();
        assert_eq!(
            serde_json::to_value(&full).unwrap(),
            serde_json::to_value(&lazy).unwrap()
        );
        let resolver = crate::lazy::Resolver::new(None);
        let Block::Text { text, .. } = &lazy.messages[0].content[0] else {
            panic!("text")
        };
        let Block::Thinking { text: thinking, .. } = &lazy.messages[1].content[0] else {
            panic!("thinking")
        };
        let Block::ToolUse { input, .. } = &lazy.messages[1].content[1] else {
            panic!("tool input")
        };
        let Block::ToolResult { content, .. } = &lazy.messages[2].content[0] else {
            panic!("result")
        };
        assert!(matches!(text, crate::lazy::Text::Source(_)));
        assert!(matches!(thinking, crate::lazy::Text::Source(_)));
        assert!(matches!(input, crate::json::Json::Source(_)));
        assert!(matches!(content, crate::lazy::Text::Source(_)));
        assert_eq!(text.try_resolve(&resolver).unwrap().as_ref(), large);
        assert_eq!(
            content.try_resolve(&resolver).unwrap().as_ref(),
            value["messages"][1]["Agent"]["tool_results"]["t"]["content"].to_string()
        );
        writer
            .execute("UPDATE threads SET summary = 'changed' WHERE id = 't'", [])
            .unwrap();
        assert!(text.try_resolve(&resolver).is_err());
        assert!(input.resolve().is_err());
        assert!(content.try_resolve(&resolver).is_err());
    }

    #[test]
    fn empty_thread_parses_to_zero_messages() {
        let thread = serde_json::json!({
            "version": "0.2.0",
            "summary": "New Thread",
            "updated_at": "2025-08-08T21:06:36.188213Z",
            "messages": []
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "New Thread", "zstd", &zstd_blob(&thread));
        let refs = discover(&c);
        assert_eq!(refs[0].message_count, None);
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert!(s.messages.is_empty());
        assert_eq!(s.title.as_deref(), Some("New Thread"));
    }

    #[test]
    fn old_schema_without_added_columns_still_works() {
        let c = mk(SCHEMA_OLD);
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data) \
             VALUES ('t', 'pre-alter', '2025-07-05T02:22:05.419868+00:00', 'zstd', ?1)",
            [zstd_blob(&legacy_thread())],
        )
        .unwrap();
        let refs = discover(&c);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].message_count, None);
        assert!(refs[0].created_at.is_none(), "no created_at column in old schema");
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
    }

    #[test]
    fn worktree_branch_fills_missing_snapshot_branch() {
        let c = mk(SCHEMA);
        for (id, thread, expected) in [
            ("missing", serde_json::json!({"messages":[]}), "isolated-branch"),
            (
                "snapshot",
                serde_json::json!({"initial_project_snapshot":{"worktree_snapshots":[{"git_state":{"current_branch":"snapshot-branch","head_sha":"commit"}}]},"messages":[]}),
                "snapshot-branch",
            ),
        ] {
            insert(&c, id, "branch", "json", &serde_json::to_vec(&thread).unwrap());
            c.execute("UPDATE threads SET worktree_branch='isolated-branch' WHERE id=?1", [id])
                .unwrap();
            let session = parse_thread(&c, &sref(id)).unwrap();
            assert_eq!(session.git.unwrap().branch.as_deref(), Some(expected));
        }
    }

    #[test]
    fn parent_id_column_links_subagent_threads() {
        let c = mk(SCHEMA);
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, parent_id) \
             VALUES ('child', 'sub work', '2026-06-10T00:00:00+00:00', 'zstd', ?1, 'parent-thread')",
            [zstd_blob(&legacy_thread())],
        )
        .unwrap();
        let s = parse_thread(&c, &sref("child")).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-thread"));
    }

    /// The modern `Compaction` variant becomes a `CompactionSummary` turn (no flat marker).
    #[test]
    fn modern_compaction_becomes_compaction_summary() {
        let thread = serde_json::json!({
            "version": "0.3.0",
            "title": "compacted",
            "updated_at": "2026-06-10T00:00:00Z",
            "messages": [
                {"User": {"id": "u", "content": [{"Text": "hi"}]}},
                {"Compaction": {"Summary": "we did a lot of things earlier"}}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "compacted", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        let comp = s
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::CompactionSummary)
            .expect("compaction summary");
        assert_eq!(comp.role, Role::System);
        assert_eq!(comp.origin, Origin::Harness);
        assert!(comp.text().unwrap().contains("earlier"));
        assert!(
            !comp.extra.contains_key("compaction"),
            "kind carries it, not a flat key"
        );
    }

    /// A hidden legacy user turn ("Continue where you left off") is harness-injected context.
    #[test]
    fn hidden_user_turn_is_injected_context() {
        let thread = serde_json::json!({
            "version": "0.2.0",
            "summary": "resumed",
            "updated_at": "2026-06-10T00:00:00Z",
            "messages": [
                {"id": 0, "role": "user", "is_hidden": true,
                 "segments": [{"type": "text", "text": "Continue where you left off"}]},
                {"id": 1, "role": "assistant", "segments": [{"type": "text", "text": "ok"}]}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "resumed", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!((u.kind, u.origin), (MessageKind::InjectedContext, Origin::Harness));
        assert_eq!(
            u.harness_extra(Harness::Zed).and_then(|b| b.get("is_hidden")),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn folder_paths_order_restores_primary_folder() {
        // Lexicographic paths "/a\n/z" with order "1,0" → the user's first folder is "/z".
        assert_eq!(
            first_folder_path(Some("/a\n/z"), Some("1,0")).as_deref(),
            Some(Path::new("/z"))
        );
        assert_eq!(
            first_folder_path(Some("/a\n/z"), None).as_deref(),
            Some(Path::new("/a"))
        );
        assert_eq!(first_folder_path(Some(""), None), None);
        assert_eq!(first_folder_path(None, Some("0")), None);
        // An out-of-range order index degrades to the first path.
        assert_eq!(
            first_folder_path(Some("/a"), Some("7")).as_deref(),
            Some(Path::new("/a"))
        );
    }

    #[test]
    fn stream_equals_parse_on_synthetic_db() {
        let _world = crate::test_world("zed-stream-parse");
        // The corpus-wide invariant, asserted here on a fixture so it runs without real data:
        // parse() and collect(stream()) are the same bytes.
        let dir = cv_testkit::temp_dir("zed-parity");
        let db = dir.join("threads.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
            insert(&c, "t2", "", "zstd", &zstd_blob(&modern_thread()));
        }
        let zed = Zed { db: Some(db.clone()) };
        let refs = zed.discover().unwrap();
        assert_eq!(refs.len(), 2);
        for r in &refs {
            let parsed = zed.parse(r).unwrap();
            let mut sink = crate::stream::CollectSink::default();
            let mut streamed = zed.stream(r, &crate::stream::Demand::RICH, &mut sink).unwrap();
            streamed.messages = sink.messages;
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&streamed).unwrap(),
                "parse != collect(stream) for {}",
                r.id
            );
            assert_eq!(r.message_count, None, "discovery leaves turns uncounted for {}", r.id);
            assert!(parsed
                .messages
                .iter()
                .any(|m| matches!(m.role, Role::User | Role::Assistant)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save of one thread rediscovers the store decoding that thread alone: every other
    /// thread keeps the listing its unchanged row certifies, in this process or (its memo gone,
    /// as in the next `cv` process) from the catalog.
    #[test]
    fn a_save_decodes_only_the_saved_thread_at_discovery() {
        let world = crate::test_world("zed-listing");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let db = world.base.join("threads.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        for id in ["t1", "t2", "t3"] {
            insert(&c, id, id, "zstd", &zstd_blob(&legacy_thread()));
        }
        let zed = Zed::with_root(&world.base);
        let decoded = || super::super::store::sqlite::listing::DECODED.with(|n| n.replace(0));
        let listing = || {
            let mut refs = zed.discover().unwrap().refs;
            refs.sort_by(|a, b| a.id.cmp(&b.id));
            serde_json::to_string(&refs).unwrap()
        };
        let first = listing();
        assert_eq!(decoded(), 3);
        let save = |id: &str, at: &str| {
            c.execute(
                "INSERT OR REPLACE INTO threads (id, summary, updated_at, data_type, data, folder_paths, \
                 folder_paths_order, created_at) SELECT id, summary, ?2, data_type, data, folder_paths, \
                 folder_paths_order, created_at FROM threads WHERE id = ?1",
                rusqlite::params![id, at],
            )
            .unwrap();
        };
        save("t2", "2030-01-01T00:00:00+00:00");
        let saved = listing();
        assert_eq!(decoded(), 1);
        assert_ne!(saved, first);
        super::super::store::sqlite::listing::forget();
        save("t3", "2030-01-02T00:00:00+00:00");
        listing();
        assert_eq!(decoded(), 1);
    }

    /// A save of one thread (its row replaced, as Zed writes it) moves its certificate alone, with
    /// and without rowids, and every other thread reads exactly as before.
    #[test]
    fn rows_certify_each_thread_and_move_only_with_its_row() {
        for without_rowid in [false, true] {
            let world = crate::test_world("zed-rows");
            let db = world.base.join("threads.db");
            {
                let c = Connection::open(&db).unwrap();
                c.execute_batch(&if without_rowid {
                    without_rowid_schema()
                } else {
                    SCHEMA.to_string()
                })
                .unwrap();
                insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
                insert(&c, "t2", "", "zstd", &zstd_blob(&modern_thread()));
                insert(&c, "t3", "Third", "json", modern_thread().to_string().as_bytes());
            }
            let zed = Zed::with_root(&world.base);
            super::super::store::sqlite::assert_rows_move_alone(&zed, &db, "t2", |c| {
                let mut thread = modern_thread();
                thread["title"] = "zqcertified".into();
                c.execute(
                    "INSERT OR REPLACE INTO threads (id, summary, updated_at, data_type, data, folder_paths, \
                     folder_paths_order, created_at) SELECT id, summary, '2030-01-01T00:00:00+00:00', data_type, ?2, \
                     folder_paths, folder_paths_order, created_at FROM threads WHERE id = ?1",
                    rusqlite::params!["t2", zstd_blob(&thread)],
                )
                .unwrap();
            });
        }
    }

    #[test]
    fn every_demand_resolves_the_same_thread_with_and_without_rowid() {
        let world = cv_testkit::World::new("zed-checked-stream");
        let path = world.base.join("threads.db");
        for without_rowid in [false, true] {
            let schema = if without_rowid {
                without_rowid_schema()
            } else {
                SCHEMA.to_string()
            };
            let writer = Connection::open(&path).unwrap();
            writer.execute_batch("DROP TABLE IF EXISTS threads;").unwrap();
            writer.execute_batch(&schema).unwrap();
            for (id, value) in [("legacy", legacy_thread()), ("modern", modern_thread())] {
                insert(&writer, id, id, "zstd", &zstd_blob(&value));
            }
            insert(
                &writer,
                "invalid",
                "invalid",
                "json",
                br#"{"messages":[{"role":"user","text":"ok"}],"ignored":"\ud800"}"#,
            );
            let zed = Zed { db: Some(path.clone()) };
            for id in ["legacy", "modern", "invalid"] {
                let mut reference = sref(id);
                reference.path = path.clone();
                let expected = zed.parse(&reference).unwrap();
                for options in [
                    Demand::LEAN.with_spans(),
                    Demand::FACTS.with_spans(),
                    Demand::RICH.with_spans(),
                    Demand {
                        spans: true,
                        ..Demand::COMPLETE
                    },
                ] {
                    let mut sink = crate::stream::CollectSink::default();
                    let mut actual = zed.stream(&reference, &options, &mut sink).unwrap();
                    actual.messages = sink.messages;
                    assert_eq!(
                        serde_json::to_vec(&actual).unwrap(),
                        serde_json::to_vec(&expected).unwrap(),
                        "id={id} without_rowid={without_rowid}"
                    );
                }
            }
        }
    }

    #[test]
    fn checked_stream_retained_ranges_reject_wal_changes() {
        let world = cv_testkit::World::new("zed-checked-wal");
        let path = world.base.join("threads.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute_batch(SCHEMA).unwrap();
        let body = "large checked body\n".repeat(4096);
        let value = serde_json::json!({"messages":[{"role":"user","text":body}]});
        insert(&writer, "t", "t", "zstd", &zstd_blob(&value));
        let zed = Zed { db: Some(path.clone()) };
        let mut reference = sref("t");
        reference.path = path;
        let mut sink = crate::stream::CollectSink::default();
        zed.stream(&reference, &Demand::LEAN.with_spans(), &mut sink).unwrap();
        let Block::Text { text, .. } = &sink.messages[0].content[0] else {
            panic!("text")
        };
        assert!(matches!(text, crate::lazy::Text::Source(_)));
        assert_eq!(
            text.try_resolve(&crate::lazy::Resolver::new(None)).unwrap().as_ref(),
            body
        );
        writer
            .execute("UPDATE threads SET summary = 'changed' WHERE id = 't'", [])
            .unwrap();
        assert!(text.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
    }

    #[test]
    fn missing_db_discovers_nothing() {
        let zed = Zed { db: None };
        assert!(zed.discover().unwrap().is_empty());
        assert!(zed.roots().is_empty());
    }

    /// Smoke over the real local Zed install (skipped in CI; run with --ignored --nocapture).
    #[test]
    #[ignore = "reads the live ~/Library/Application Support/Zed/threads/threads.db"]
    fn real_zed_threads_discover_and_parse() {
        let zed = Zed::detect();
        let Some(root) = zed.roots().into_iter().next() else {
            eprintln!("no Zed install here; nothing to do");
            return;
        };
        let refs = zed.discover().unwrap();
        eprintln!("zed: {} thread(s) in {}", refs.len(), root.display());
        for r in &refs {
            let s = zed.parse(r).unwrap();
            crate::harness::assert_no_flat_keys(&s);
            eprintln!(
                "  {} [{}] {:?} cwd={:?} msgs={} (count={})",
                &r.id[..8.min(r.id.len())],
                s.model.as_deref().unwrap_or("?"),
                s.label(),
                s.cwd,
                s.messages.len(),
                r.message_count.map_or("-".to_string(), |n| n.to_string())
            );
        }
    }
}
