//! Zed editor agent-panel adapter — `threads.db` (SQLite, zstd-compressed JSON blobs).
//! **sqlite feature only** (rusqlite + ruzstd).
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
//! * Legacy `context` (the rendered attached-files preamble) is preserved in `Message.extra`;
//!   modern `Mention` content (an attached file's body) is reduced to its URI as a [`Block::File`].
//! * `request_token_usage` (per-request usage) has no per-message home — only the thread-level
//!   `cumulative_token_usage` is kept (in `Session.extra`).

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Zed {
    /// `threads.db`, if it exists on disk.
    db: Option<PathBuf>,
}

impl Zed {
    pub fn new() -> Self {
        Zed { db: threads_db() }
    }
}

impl Default for Zed {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `…/Zed/threads/threads.db` across platforms. Returns it only if it exists, so discovery
/// degrades to empty on machines without Zed.
fn threads_db() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // XDG override first (portable, and what Zed's `paths::data_dir` honours on Linux).
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg).join("zed/threads/threads.db"));
    }
    if let Some(h) = dirs::home_dir() {
        candidates.push(h.join("Library/Application Support/Zed/threads/threads.db"));
        candidates.push(h.join(".local/share/zed/threads/threads.db"));
        // Flatpak install keeps the same layout under the app sandbox.
        candidates.push(h.join(".var/app/dev.zed.Zed/data/zed/threads/threads.db"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Zed").join("threads").join("threads.db"));
    }
    candidates.into_iter().find(|p| p.exists())
}

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))
}

impl Adapter for Zed {
    fn harness(&self) -> Harness {
        Harness::Zed
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.db.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(db) = &self.db else {
            return Ok(vec![]);
        };
        let conn = open_ro(db)?;
        discover_db(&conn, db)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let conn = open_ro(&r.path)?;
        stream_thread(&conn, r, sink)
    }
}

/// Columns present on a table (Zed grows `threads` via `ALTER TABLE`, so old DBs lack the newer
/// `parent_id`/`folder_paths`/`created_at` columns entirely).
fn columns(conn: &Connection, table: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            for n in rows.flatten() {
                set.insert(n);
            }
        }
    }
    set
}

fn discover_db(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let cols = columns(conn, "threads");
    let has = |c: &str| cols.contains(c);
    let sql = format!(
        "SELECT id, summary, updated_at, {created}, {folders}, {order}, data_type, data \
         FROM threads ORDER BY updated_at DESC",
        created = if has("created_at") { "created_at" } else { "NULL" },
        folders = if has("folder_paths") { "folder_paths" } else { "NULL" },
        order = if has("folder_paths_order") {
            "folder_paths_order"
        } else {
            "NULL"
        },
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1).ok().flatten(),
            row.get::<_, Option<String>>(2).ok().flatten(),
            row.get::<_, Option<String>>(3).ok().flatten(),
            row.get::<_, Option<String>>(4).ok().flatten(),
            row.get::<_, Option<String>>(5).ok().flatten(),
            row.get::<_, Option<String>>(6).ok().flatten(),
            row.get::<_, Option<Vec<u8>>>(7).ok().flatten().unwrap_or_default(),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        // One unreadable row never fails the harness.
        let Ok((id, summary, updated, created, folders, order, data_type, data)) = row else {
            continue;
        };
        // Decode the blob once per thread for the user+assistant count (Zed has no message table
        // to COUNT from) and the snapshot cwd. A corrupt blob degrades to count 0, never an error.
        let thread = decode_blob(data_type.as_deref(), &data);
        let mut count = 0usize;
        let mut cwd = None;
        if let Some(t) = &thread {
            each_ir_message(t, &mut |m| {
                if matches!(m.role, Role::User | Role::Assistant) {
                    count += 1;
                }
                Flow::Continue
            });
            cwd = snapshot_cwd(t);
        }
        let cwd = cwd.or_else(|| first_folder_path(folders.as_deref(), order.as_deref()));
        out.push(SessionRef {
            id,
            harness: Harness::Zed,
            path: db.to_path_buf(),
            cwd,
            title: summary
                .filter(|s| !s.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, 80)),
            created_at: created.as_deref().and_then(super::parse_ts),
            updated_at: updated.as_deref().and_then(super::parse_ts),
            message_count: count,
        });
    }
    Ok(out)
}

fn stream_thread(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    let cols = columns(conn, "threads");
    let has = |c: &str| cols.contains(c);
    let sql = format!(
        "SELECT summary, updated_at, {created}, {parent}, {folders}, {order}, data_type, data \
         FROM threads WHERE id = ?1",
        created = if has("created_at") { "created_at" } else { "NULL" },
        parent = if has("parent_id") { "parent_id" } else { "NULL" },
        folders = if has("folder_paths") { "folder_paths" } else { "NULL" },
        order = if has("folder_paths_order") {
            "folder_paths_order"
        } else {
            "NULL"
        },
    );
    /// One optional value per column the metadata SELECT reads (the blob last).
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Vec<u8>,
    );
    let (summary, updated, created, parent, folders, order, data_type, data): Row = conn
        .query_row(&sql, [&r.id], |row| {
            Ok((
                row.get(0).ok().flatten(),
                row.get(1).ok().flatten(),
                row.get(2).ok().flatten(),
                row.get(3).ok().flatten(),
                row.get(4).ok().flatten(),
                row.get(5).ok().flatten(),
                row.get(6).ok().flatten(),
                row.get::<_, Option<Vec<u8>>>(7).ok().flatten().unwrap_or_default(),
            ))
        })
        .with_context(|| format!("zed thread {} not found", r.id))?;

    let thread = decode_blob(data_type.as_deref(), &data);

    let mut extra = serde_json::Map::new();
    let mut model = None;
    let mut git = None;
    let mut blob_title = None;
    let mut blob_updated = None;
    let mut cwd = None;
    if let Some(t) = &thread {
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
        model = t.get("model").and_then(model_string);
        thread_extras(t, &mut extra);
    } else if !data.is_empty() {
        // The row exists but its blob wouldn't decode — surface that instead of erroring, so one
        // corrupt thread never breaks listings or bulk exports.
        extra.insert("undecodable_blob".into(), Value::Bool(true));
    }
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        // Subagent linkage (also present inside the 0.3.0 blob; the column wins when both exist).
        extra.insert("parent_thread_id".into(), Value::String(p));
    }

    let s = Session {
        id: r.id.clone(),
        harness: Harness::Zed,
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
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra,
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    // All session metadata is known up front; hand it to the sink before the body.
    sink.meta(&s);

    if let Some(t) = &thread {
        each_ir_message(t, &mut |m| sink.message(m));
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Blob decoding
// ---------------------------------------------------------------------------

/// Decompress + parse one thread blob. `data_type` is `"zstd"` or `"json"`; unknown/absent values
/// are sniffed (zstd magic `28 B5 2F FD`). Any failure → `None` (tolerated by every caller).
fn decode_blob(data_type: Option<&str>, data: &[u8]) -> Option<Value> {
    let is_zstd = match data_type {
        Some("zstd") => true,
        Some("json") => false,
        _ => data.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]),
    };
    let json: std::borrow::Cow<'_, [u8]> = if is_zstd {
        std::borrow::Cow::Owned(decompress_zstd(data)?)
    } else {
        std::borrow::Cow::Borrowed(data)
    };
    serde_json::from_slice(&json).ok()
}

fn decompress_zstd(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(data).ok()?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------
// Thread-level metadata
// ---------------------------------------------------------------------------

/// `{provider, model}` → `"provider/model"` (matching the goose/hermes convention).
fn model_string(v: &Value) -> Option<String> {
    let provider = v.get("provider").and_then(Value::as_str).filter(|s| !s.is_empty());
    let model = v.get("model").and_then(Value::as_str).filter(|s| !s.is_empty());
    match (provider, model) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (None, Some(m)) => Some(m.to_string()),
        (Some(p), None) => Some(p.to_string()),
        (None, None) => None,
    }
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

/// Session-level extras shared by both generations: profile, completion mode, detailed summary,
/// cumulative token usage, subagent context, imported flag, and the raw format version.
fn thread_extras(t: &Value, extra: &mut serde_json::Map<String, Value>) {
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
        if let Some(p) = sub.get("parent_thread_id").and_then(Value::as_str) {
            extra.insert("parent_thread_id".into(), Value::String(p.to_string()));
        }
        if let Some(d) = sub.get("depth").and_then(Value::as_u64) {
            extra.insert("subagent_depth".into(), Value::Number(d.into()));
        }
    }
    if t.get("imported").and_then(Value::as_bool) == Some(true) {
        extra.insert("imported".into(), Value::Bool(true));
    }
}

// ---------------------------------------------------------------------------
// Messages — one walker for both generations (sniffed per message, not per thread,
// so a hand-migrated or mixed blob still parses)
// ---------------------------------------------------------------------------

/// Convert every thread message to IR and feed it to `f`, stopping on [`Flow::Stop`]. This is the
/// single conversion both `discover` (counting) and `stream` (emitting) run, so the
/// `message_count` contract holds by construction.
fn each_ir_message(t: &Value, f: &mut dyn FnMut(Message) -> Flow) {
    let Some(messages) = t.get("messages").and_then(Value::as_array) else {
        return;
    };
    // Tool-call ids → names, so legacy tool_results (which carry no name) get labeled.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in messages {
        if legacy_message(m, &mut tool_names, f) == Flow::Stop {
            return;
        }
        if modern_message(m, f) == Flow::Stop {
            return;
        }
    }
}

/// Legacy (agent1) message: an object with a string `role`. Emits the main turn, then its
/// `tool_results` as a separate [`Role::Tool`] turn. Non-legacy shapes are ignored.
fn legacy_message(m: &Value, tool_names: &mut HashMap<String, String>, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    let Some(role) = m.get("role").and_then(Value::as_str) else {
        return Flow::Continue;
    };
    let role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    let mut msg = Message::new(role);
    msg.id = m.get("id").and_then(Value::as_i64).map(|i| i.to_string());

    // segments[] (0.1.0+) or a plain `text` field (the oldest, versionless shape).
    if let Some(segments) = m.get("segments").and_then(Value::as_array) {
        for seg in segments {
            if let Some(b) = segment_to_block(seg) {
                msg.content.push(b);
            }
        }
    } else if let Some(text) = m.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            msg.content.push(Block::Text {
                text: text.to_string().into(),
            });
        }
    }

    for tu in m.get("tool_uses").and_then(Value::as_array).into_iter().flatten() {
        let id = tu.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let name = tu.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if !id.is_empty() && !name.is_empty() {
            tool_names.insert(id.clone(), name.clone());
        }
        msg.content.push(Block::ToolUse {
            id,
            name,
            input: tu.get("input").cloned().unwrap_or(Value::Null),
            namespace: None,
        });
    }

    // `context` is the rendered attached-files preamble Zed prepends to the request. When it's the
    // *only* content (an attachment-only user turn), surface it as the text (mirroring Zed's own
    // 0.2.0→0.3.0 upgrade); otherwise preserve it in extra.
    if let Some(ctx) = m.get("context").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        if msg.content.is_empty() {
            msg.content.push(Block::Text {
                text: ctx.to_string().into(),
            });
        } else {
            msg.extra.insert("context".into(), Value::String(ctx.to_string()));
        }
    }
    if m.get("is_hidden").and_then(Value::as_bool) == Some(true) {
        msg.extra.insert("is_hidden".into(), Value::Bool(true));
    }
    if let Some(creases) = m.get("creases").and_then(Value::as_array).filter(|c| !c.is_empty()) {
        msg.extra.insert("creases".into(), Value::Array(creases.clone()));
    }

    if !msg.content.is_empty() && f(msg) == Flow::Stop {
        return Flow::Stop;
    }

    // tool_results → one Role::Tool turn (0.2.0 keeps them on the assistant message; 0.1.0 put
    // them on the next user message — either way they become the same Tool turn here).
    let results = m.get("tool_results").and_then(Value::as_array);
    let blocks: Vec<Block> = results
        .into_iter()
        .flatten()
        .filter_map(|tr| tool_result_block(tr, tool_names))
        .collect();
    if !blocks.is_empty() {
        let mut tm = Message::new(Role::Tool);
        tm.content = blocks;
        return f(tm);
    }
    Flow::Continue
}

fn segment_to_block(seg: &Value) -> Option<Block> {
    match seg.get("type").and_then(Value::as_str)? {
        "text" => {
            let text = seg.get("text").and_then(Value::as_str)?.to_string();
            (!text.is_empty()).then(|| Block::Text { text: text.into() })
        }
        "thinking" => Some(Block::Thinking {
            text: seg.get("text").and_then(Value::as_str).unwrap_or("").to_string().into(),
            signature: seg
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            encrypted: None,
            redacted: false,
        }),
        // serde tag without a rename ⇒ the variant name itself.
        "RedactedThinking" => Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: seg.get("data").and_then(Value::as_str).map(str::to_string),
            redacted: true,
        }),
        _ => None,
    }
}

/// One legacy/modern tool result → a [`Block::ToolResult`]. The legacy shape has no `tool_name`;
/// we recover it from the paired `tool_uses` entry. `output` (the tool's structured return)
/// rides in `details`.
fn tool_result_block(tr: &Value, tool_names: &HashMap<String, String>) -> Option<Block> {
    let id = tr.get("tool_use_id").and_then(Value::as_str)?.to_string();
    let tool_name = tr
        .get("tool_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| tool_names.get(&id).cloned());
    Some(Block::ToolResult {
        content: tool_result_content(tr.get("content")).into(),
        is_error: tr.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        tool_use_id: id,
        tool_name,
        status: None,
        details: tr.get("output").filter(|o| !o.is_null()).cloned(),
    })
}

/// `LanguageModelToolResultContent`: a plain string, `{"Text": "…"}` (case-tolerant), or
/// `{"Image": {…}}` (flattened to a marker; the IR doesn't inline image bytes).
fn tool_result_content(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(s) = v.get("Text").or_else(|| v.get("text")).and_then(Value::as_str) {
        return s.to_string();
    }
    if v.get("Image").or_else(|| v.get("image")).is_some() {
        return "[image]".to_string();
    }
    if v.is_null() {
        return String::new();
    }
    v.to_string()
}

/// Modern (agent2 / 0.3.0) message: the externally tagged `Message` enum. `"Resume"` markers are
/// skipped; `Compaction` summaries become a flagged system turn. Non-modern shapes are ignored.
fn modern_message(m: &Value, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    if let Some(user) = m.get("User") {
        let mut msg = Message::new(Role::User);
        msg.id = user.get("id").and_then(Value::as_str).map(str::to_string);
        for item in user.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(b) = user_content_block(item) {
                msg.content.push(b);
            }
        }
        if !msg.content.is_empty() {
            return f(msg);
        }
        return Flow::Continue;
    }
    if let Some(agent) = m.get("Agent") {
        let mut msg = Message::new(Role::Assistant);
        let mut tool_names: HashMap<String, String> = HashMap::new();
        for item in agent.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(b) = agent_content_block(item) {
                if let Block::ToolUse { id, name, .. } = &b {
                    if !id.is_empty() && !name.is_empty() {
                        tool_names.insert(id.clone(), name.clone());
                    }
                }
                msg.content.push(b);
            }
        }
        if let Some(rd) = agent.get("reasoning_details").filter(|v| !v.is_null()) {
            msg.extra.insert("reasoning_details".into(), rd.clone());
        }
        // tool_results is an id-keyed map ({"<tool_use_id>": {…}}), ordered by insertion.
        let blocks: Vec<Block> = agent
            .get("tool_results")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
            .filter_map(|tr| tool_result_block(tr, &tool_names))
            .collect();
        if (!msg.content.is_empty() || !msg.extra.is_empty()) && f(msg) == Flow::Stop {
            return Flow::Stop;
        }
        if !blocks.is_empty() {
            let mut tm = Message::new(Role::Tool);
            tm.content = blocks;
            return f(tm);
        }
        return Flow::Continue;
    }
    if let Some(comp) = m.get("Compaction") {
        // CompactionInfo::Summary(text) — the context-compaction summary that replaced history.
        if let Some(text) = comp
            .get("Summary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            let mut msg = Message::new(Role::System);
            msg.content.push(Block::Text {
                text: text.to_string().into(),
            });
            msg.extra.insert("compaction".into(), Value::Bool(true));
            return f(msg);
        }
    }
    // "Resume" markers and unknown variants carry no transcript content.
    Flow::Continue
}

fn user_content_block(item: &Value) -> Option<Block> {
    if let Some(text) = item.get("Text").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| Block::Text {
            text: text.to_string().into(),
        });
    }
    if let Some(mention) = item.get("Mention") {
        // The mention's resolved `content` (an attached file's whole body) is dropped — the URI is
        // the durable reference.
        let uri = match mention.get("uri") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return None,
        };
        return Some(Block::File {
            mime: None,
            path: None,
            source: Some(uri),
        });
    }
    if let Some(img) = item.get("Image") {
        // LanguageModelImage: {source: <base64>, size}.
        return Some(Block::Image {
            media_type: None,
            data_ref: img
                .get("source")
                .and_then(Value::as_str)
                .map(|s| crate::ir::truncate(s, 120)),
        });
    }
    None
}

fn agent_content_block(item: &Value) -> Option<Block> {
    if let Some(text) = item.get("Text").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| Block::Text {
            text: text.to_string().into(),
        });
    }
    if let Some(th) = item.get("Thinking") {
        return Some(Block::Thinking {
            text: th.get("text").and_then(Value::as_str).unwrap_or("").to_string().into(),
            signature: th
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            encrypted: None,
            redacted: false,
        });
    }
    if let Some(data) = item.get("RedactedThinking").and_then(Value::as_str) {
        return Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: Some(data.to_string()),
            redacted: true,
        });
    }
    if let Some(tu) = item.get("ToolUse") {
        return Some(Block::ToolUse {
            id: tu.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: tu.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: tu.get("input").cloned().unwrap_or(Value::Null),
            namespace: None,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Whole-`Session` convenience over [`stream_thread`] for tests: stream into a CollectSink.
#[cfg(test)]
fn parse_thread(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_thread(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn mk(schema: &str) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(schema).unwrap();
        c
    }

    fn zstd_blob(json: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(json).unwrap();
        ruzstd::encoding::compress_to_vec(bytes.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Zed,
            path: PathBuf::from(":memory:"),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    /// A representative legacy (0.2.0) thread: user text, assistant thinking+text+tool call with
    /// its result on the same message, project snapshot, model, profile.
    fn legacy_thread() -> Value {
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
    fn modern_thread() -> Value {
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
    fn discover_reads_metadata_and_counts() {
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

        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        // Ordered by updated_at DESC.
        assert_eq!(refs[0].id, "t2");
        assert_eq!(refs[1].id, "t1");

        let t1 = &refs[1];
        assert_eq!(t1.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(t1.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert!(t1.created_at.unwrap() < t1.updated_at.unwrap());
        // user + assistant turns only: 1 user + 2 assistant (the Tool turn doesn't count).
        assert_eq!(t1.message_count, 3);

        let t2 = &refs[0];
        assert_eq!(t2.title, None, "empty summary must not become a title");
        // Snapshot cwd wins over folder_paths.
        assert_eq!(t2.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        // 1 user + 1 assistant ("Resume" and the Tool turn don't count).
        assert_eq!(t2.message_count, 2);
    }

    #[test]
    fn parses_legacy_thread() {
        let c = mk(SCHEMA);
        insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
        let s = parse_thread(&c, &sref("t1")).unwrap();

        assert_eq!(s.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(s.model.as_deref(), Some("zed.dev/claude-sonnet-4-thinking"));
        let git = s.git.as_ref().unwrap();
        assert_eq!(git.branch.as_deref(), Some("main"));
        assert_eq!(git.commit.as_deref(), Some("abc123"));
        assert_eq!(git.remote.as_deref(), Some("git@github.com:u/proj"));
        // Blob updated_at preferred; created_at from the column.
        assert!(s.created_at.is_some() && s.updated_at.is_some());
        assert_eq!(s.extra.get("profile").and_then(Value::as_str), Some("write"));
        assert_eq!(s.extra.get("completion_mode").and_then(Value::as_str), Some("burn"));
        assert_eq!(s.extra.get("thread_version").and_then(Value::as_str), Some("0.2.0"));
        assert_eq!(
            s.extra.get("detailed_summary").and_then(Value::as_str),
            Some("We fixed a race.")
        );
        assert!(s.extra.contains_key("cumulative_token_usage"));

        // user, assistant(+tool call), Tool, assistant.
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("why is test_foo flaky?"));
        assert_eq!(
            s.messages[0].extra.get("context").and_then(Value::as_str),
            Some("<context>attached file foo.rs</context>")
        );
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(&a.content[0], Block::Thinking { text, signature, .. }
            if text == "let me look" && signature.as_deref() == Some("sig1")));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "Checking the test now."));
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, input , ..}
            if id == "toolu_1" && name == "read_file"
            && input.get("path").and_then(Value::as_str) == Some("foo.rs")));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, details, .. }
            if tool_use_id == "toolu_1" && content == "fn test_foo() {}" && !is_error
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

        assert_eq!(s.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(s.model.as_deref(), Some("openai/gpt-5-mini"));
        assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("dev"));
        assert_eq!(
            s.extra.get("parent_thread_id").and_then(Value::as_str),
            Some("parent-1")
        );
        assert_eq!(s.extra.get("subagent_depth").and_then(Value::as_u64), Some(1));
        // All-zero usage is not stashed.
        assert!(!s.extra.contains_key("cumulative_token_usage"));

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
        assert!(matches!(&a.content[0], Block::Thinking { text, .. } if text == "plan the split"));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "Splitting now."));
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, .. } if id == "toolu_9" && name == "edit_file"));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, .. }
            if tool_use_id == "toolu_9" && content == "permission denied" && *is_error
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
            if tool_name.as_deref() == Some("shell") && content == "file_a")
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
        assert!(s.extra.get("thread_version").is_none());
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
    fn corrupt_blob_is_tolerated_everywhere() {
        let c = mk(SCHEMA);
        insert(&c, "bad", "corrupt one", "zstd", b"\x28\xb5\x2f\xfdnot really zstd");
        insert(&c, "good", "fine one", "zstd", &zstd_blob(&legacy_thread()));

        // discover lists both; the corrupt one just has no count/cwd.
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        let bad = refs.iter().find(|r| r.id == "bad").unwrap();
        assert_eq!(bad.message_count, 0);
        assert_eq!(bad.title.as_deref(), Some("corrupt one"));

        // parse returns the row's metadata + a flag, never an error.
        let s = parse_thread(&c, &sref("bad")).unwrap();
        assert!(s.messages.is_empty());
        assert_eq!(s.extra.get("undecodable_blob"), Some(&Value::Bool(true)));
        assert_eq!(s.title.as_deref(), Some("corrupt one"));
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
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs[0].message_count, 0);
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
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].message_count, 3);
        assert!(refs[0].created_at.is_none(), "no created_at column in old schema");
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
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
        assert_eq!(
            s.extra.get("parent_thread_id").and_then(Value::as_str),
            Some("parent-thread")
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
        // The corpus-wide invariant, asserted here on a fixture so it runs without real data:
        // parse() and collect(stream()) are the same bytes.
        let dir = std::env::temp_dir().join(format!("cv-zed-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
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
            let mut streamed = zed.stream(r, &crate::stream::ParseOptions::full(), &mut sink).unwrap();
            streamed.messages = sink.messages;
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&streamed).unwrap(),
                "parse != collect(stream) for {}",
                r.id
            );
            // And the discover contract: message_count == parsed user+assistant turns.
            let ua = parsed
                .messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count();
            assert_eq!(r.message_count, ua, "message_count contract for {}", r.id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_db_discovers_nothing() {
        let zed = Zed { db: None };
        assert!(zed.discover().unwrap().is_empty());
        assert!(zed.storage_root().is_none());
    }

    /// Smoke over the real local Zed install (skipped in CI; run with --ignored --nocapture).
    #[test]
    #[ignore = "reads the live ~/Library/Application Support/Zed/threads/threads.db"]
    fn real_zed_threads_discover_and_parse() {
        let zed = Zed::new();
        let Some(root) = zed.storage_root() else {
            eprintln!("no Zed install here; nothing to do");
            return;
        };
        let refs = zed.discover().unwrap();
        eprintln!("zed: {} thread(s) in {}", refs.len(), root.display());
        for r in &refs {
            let s = zed.parse(r).unwrap();
            eprintln!(
                "  {} [{}] {:?} cwd={:?} msgs={} (count={})",
                &r.id[..8.min(r.id.len())],
                s.model.as_deref().unwrap_or("?"),
                s.label(),
                s.cwd,
                s.messages.len(),
                r.message_count
            );
        }
    }
}
