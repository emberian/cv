//! Cursor IDE adapter — chat/composer history in VS Code-style `state.vscdb` (SQLite).
//!
//! See `docs/FORMATS.md`. Cursor stores conversations across two SQLite databases:
//!
//!   * **Global:** `<AppSupport>/Cursor/User/globalStorage/state.vscdb`. Newer Cursor keeps the
//!     actual chat/composer content here, in a `cursorDiskKV(key TEXT, value BLOB)` table:
//!       - `composerData:<composerId>` — one row per composer/chat thread (metadata: `name`,
//!         `createdAt`, `lastUpdatedAt`, `unifiedMode`, `_v`, plus either an inline `conversation`
//!         array (older `_v`) or a `fullConversationHeadersOnly` list of `{bubbleId,type}` pointers
//!         (newer `_v`)).
//!       - `bubbleId:<composerId>:<bubbleId>` — one row per message ("bubble"): `type` (1=user,
//!         2=assistant), `text`, `richText` (Lexical JSON), `thinking:{text}`, `toolFormerData`
//!         (a tool call+result), `tokenCount:{inputTokens,outputTokens}`.
//!   * **Per-workspace:** `<AppSupport>/Cursor/User/workspaceStorage/<hash>/state.vscdb`, with a
//!     sibling `workspace.json` giving the workspace folder URI (= the cwd). Its `ItemTable` has
//!     `composer.composerData` → `{allComposers:[{composerId,name,createdAt,lastUpdatedAt,...}]}`
//!     which links composerIds to that workspace (and hence a cwd). Older Cursor stored chat blobs
//!     in the workspace DB itself (e.g. `workbench.panel.aichat.view.aichat.chatdata`).
//!
//! Strategy: build a composerId→cwd map by scanning every workspace's `composer.composerData`,
//! then enumerate every global `composerData:` thread. Threads not linked to any workspace (common:
//! ~20% on the dev machine) are still surfaced, just with `cwd = None`. The legacy in-workspace
//! `aichat.chatdata` shape is also parsed when present so old installs aren't dropped.
//!
//! Tolerance: Cursor is closed-source and the schema churns across versions (`_v` 1..=10+ seen on
//! one machine). Anything absent is skipped gracefully — a missing table, key, or field never
//! panics and never fails the whole session.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Cursor {
    /// `<AppSupport>/Cursor/User` if it exists.
    user_dir: Option<PathBuf>,
}

impl Cursor {
    pub fn new() -> Self {
        // CURSOR_USER_DIR overrides the platform default (used by tests / non-mac).
        let user_dir = std::env::var_os("CURSOR_USER_DIR")
            .map(PathBuf::from)
            .or_else(default_user_dir)
            .filter(|p| p.exists());
        Cursor { user_dir }
    }

    fn global_db(&self) -> Option<PathBuf> {
        self.user_dir
            .as_ref()
            .map(|d| d.join("globalStorage").join("state.vscdb"))
            .filter(|p| p.exists())
    }

    fn workspace_dirs(&self) -> Vec<PathBuf> {
        let Some(ws_root) = self
            .user_dir
            .as_ref()
            .map(|d| d.join("workspaceStorage"))
            .filter(|p| p.exists())
        else {
            return vec![];
        };
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&ws_root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// Default macOS/Linux/Windows location of `Cursor/User`.
fn default_user_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"));
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Some(base.join("Cursor").join("User"))
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

fn open_ro(path: &Path) -> Result<Connection> {
    // immutable so we never lock the user's live DB or get blocked by the WAL.
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))
}

impl Adapter for Cursor {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.global_db().or_else(|| self.user_dir.clone())
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        // Map composerId -> cwd by scanning every workspace's composer.composerData. Also collect
        // metadata fallbacks (name/created) and legacy in-workspace chat threads.
        let cwd_map = self.build_cwd_map();

        if let Some(db) = self.global_db() {
            if let Ok(conn) = open_ro(&db) {
                discover_global(&conn, &db, &cwd_map, &mut out);
            }
        }

        // Legacy: old Cursor kept chat blobs inside the workspace DB itself.
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let cwd = workspace_cwd(&ws);
            if let Ok(conn) = open_ro(&db) {
                discover_legacy_workspace(&conn, &db, cwd.as_deref(), &mut out);
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // `path` is the DB the thread lives in. Legacy refs carry a `legacy:` id prefix.
        let conn = open_ro(&r.path)?;
        if let Some(legacy_id) = r.id.strip_prefix("legacy:") {
            return stream_legacy(&conn, r, legacy_id, sink);
        }
        stream_global(&conn, r, sink)
    }
}

impl Cursor {
    /// composerId -> cwd, harvested from every workspace's `composer.composerData.allComposers`.
    fn build_cwd_map(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let Some(cwd) = workspace_cwd(&ws) else {
                continue;
            };
            let Ok(conn) = open_ro(&db) else { continue };
            let Some(raw) = item_table_get(&conn, "composer.composerData") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if let Some(all) = v.get("allComposers").and_then(Value::as_array) {
                for c in all {
                    if let Some(id) = c.get("composerId").and_then(Value::as_str) {
                        map.entry(id.to_string()).or_insert_with(|| cwd.clone());
                    }
                }
            }
        }
        map
    }
}

/// Read a single ItemTable value (TEXT or BLOB) as a UTF-8 string.
fn item_table_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        // value may be stored as TEXT or BLOB depending on version.
        match row.get::<_, String>(0) {
            Ok(s) => Ok(s),
            Err(_) => row
                .get::<_, Vec<u8>>(0)
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        }
    })
    .ok()
}

/// Read a cursorDiskKV value as a UTF-8 string.
fn disk_kv_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM cursorDiskKV WHERE key = ?1",
        [key],
        |row| match row.get::<_, String>(0) {
            Ok(s) => Ok(s),
            Err(_) => row
                .get::<_, Vec<u8>>(0)
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        },
    )
    .ok()
}

/// Resolve the cwd for a workspace dir from its sibling `workspace.json` `folder` URI.
fn workspace_cwd(ws: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(ws.join("workspace.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    // `folder` is a single-root folder URI; multi-root workspaces use `workspace` (a .json path) —
    // we only resolve the simple single-folder case to a cwd.
    let uri = v.get("folder").and_then(Value::as_str)?;
    file_uri_to_path(uri)
}

/// `file:///Users/x/p%20q` -> `/Users/x/p q`. Returns None for non-file URIs.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an authority component if present (file://host/path); keep the leading '/'.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => rest,
    };
    Some(PathBuf::from(percent_decode(path)))
}

/// Minimal percent-decoder (Cursor URIs use %XX for spaces etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn ms_to_dt(ms: i64) -> Option<chrono::DateTime<Utc>> {
    if ms <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms).single()
}

// ---------------------------------------------------------------------------
// Global (cursorDiskKV) format
// ---------------------------------------------------------------------------

fn discover_global(conn: &Connection, db: &Path, cwd_map: &HashMap<String, PathBuf>, out: &mut Vec<SessionRef>) {
    // Bail quietly if this DB has no cursorDiskKV table (older layout).
    let mut stmt = match conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'") {
        Ok(s) => s,
        Err(_) => return,
    };
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value: String = match row.get::<_, String>(1) {
            Ok(s) => s,
            Err(_) => row
                .get::<_, Vec<u8>>(1)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default(),
        };
        Ok((key, value))
    });
    let Ok(rows) = rows else { return };

    for (key, value) in rows.flatten() {
        let Some(composer_id) = key.strip_prefix("composerData:") else {
            continue;
        };
        let Ok(d) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        let count = conversation_len(&d);
        // Skip threads that have no messages at all (Cursor leaves empty composer shells around).
        if count == 0 {
            continue;
        }
        let created = d.get("createdAt").and_then(Value::as_i64);
        let updated = d.get("lastUpdatedAt").and_then(Value::as_i64).or(created);
        let title = d
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| crate::ir::truncate(s, 80));
        out.push(SessionRef {
            id: composer_id.to_string(),
            harness: Harness::Cursor,
            path: db.to_path_buf(),
            cwd: cwd_map.get(composer_id).cloned(),
            title,
            created_at: created.and_then(ms_to_dt),
            updated_at: updated.and_then(ms_to_dt),
            message_count: count,
        });
    }
}

/// Number of messages in a composerData record, across both shapes.
fn conversation_len(d: &Value) -> usize {
    if let Some(h) = d.get("fullConversationHeadersOnly").and_then(Value::as_array) {
        return h.len();
    }
    d.get("conversation")
        .and_then(Value::as_array)
        .map(|c| c.len())
        .unwrap_or(0)
}

fn stream_global(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    let key = format!("composerData:{}", r.id);
    let raw = disk_kv_get(conn, &key).with_context(|| format!("composerData:{} not found", r.id))?;
    let d: Value = serde_json::from_str(&raw).context("composerData JSON")?;

    let created = d.get("createdAt").and_then(Value::as_i64);
    let updated = d.get("lastUpdatedAt").and_then(Value::as_i64).or(created);
    let title = d
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| r.title.clone());

    let mut session = Session {
        id: r.id.clone(),
        harness: Harness::Cursor,
        cwd: r.cwd.clone(),
        title,
        created_at: created.and_then(ms_to_dt).or(r.created_at),
        updated_at: updated.and_then(ms_to_dt).or(r.updated_at),
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    // Sub-agent lineage: a composer spawned as a sub-agent records `subagentInfo`
    // (`parentComposerId`, the spawning `toolCallId`, the `subagentTypeName` nickname).
    if let Some(si) = d.get("subagentInfo").filter(|v| !v.is_null()) {
        if let Some(p) = si
            .get("parentComposerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.parent = Some(p.to_string());
        }
        if let Some(tc) = si.get("toolCallId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            session.lineage.spawned_by_tool_use = Some(tc.to_string());
        }
        if let Some(name) = si
            .get("subagentTypeName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.agent_path = Some(name.to_string());
        }
    }
    // `unifiedMode` (chat/agent/edit) has no first-class IR home. The composer's is a *session*
    // fact — the mode the thread was opened in — so it lands in the session bag; a bubble that
    // carries its own overrides it for that message only (see `bubble_to_message`), which is how
    // Cursor records a mode switched mid-thread.
    if let Some(mode) = d.get("unifiedMode").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        session
            .harness_extra_mut(Harness::Cursor)
            .insert("unified_mode".into(), Value::String(mode.to_string()));
    }

    // Session model: the latest message's model (whole-Vec `rev().find_map` originally). Tracked
    // incrementally here — each emitted message overwrites it when it carries a model — so the last
    // one wins without retaining the body. Falls back to the composer's modelConfig below.
    let mut last_model: Option<String> = None;
    let mut emit = |sink: &mut dyn MessageSink, m: Message| -> Flow {
        if m.model.is_some() {
            last_model = m.model.clone();
        }
        sink.message(m)
    };

    // Newer shape: headers point at external bubble rows (each a separate DB row, fetched lazily so
    // one bubble is resident at a time). Older shape: inline conversation array.
    if let Some(headers) = d.get("fullConversationHeadersOnly").and_then(Value::as_array) {
        for h in headers {
            let Some(bubble_id) = h.get("bubbleId").and_then(Value::as_str) else {
                continue;
            };
            let bkey = format!("bubbleId:{}:{}", r.id, bubble_id);
            let Some(braw) = disk_kv_get(conn, &bkey) else {
                continue;
            };
            let Ok(bv) = serde_json::from_str::<Value>(&braw) else {
                continue;
            };
            if let Some(m) = bubble_to_message(&bv) {
                if emit(sink, m) == Flow::Stop {
                    break;
                }
            }
        }
    } else if let Some(conv) = d.get("conversation").and_then(Value::as_array) {
        for bv in conv {
            if let Some(m) = bubble_to_message(bv) {
                if emit(sink, m) == Flow::Stop {
                    break;
                }
            }
        }
    }

    session.model = last_model.or_else(|| {
        d.pointer("/modelConfig/modelName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });

    Ok(session)
}

/// Convert one Cursor "bubble" (message) object into an IR [`Message`].
/// `type`: 1 = user, 2 = assistant. Returns None for an entirely empty bubble.
fn bubble_to_message(b: &Value) -> Option<Message> {
    let btype = b.get("type").and_then(Value::as_i64).unwrap_or(0);
    let role = match btype {
        1 => Role::User,
        2 => Role::Assistant,
        _ => Role::Assistant,
    };
    let mut m = Message::new(role);
    m.id = b.get("bubbleId").and_then(Value::as_str).map(str::to_string);

    // Thinking block (assistant): {text: "..."} (and sometimes signature).
    if let Some(th) = b.get("thinking") {
        let text = th.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let signature = th
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        if !text.is_empty() || signature.is_some() {
            m.content.push(Block::Thinking {
                text: text.into(),
                signature,
                encrypted: None,
                redacted: false,
            });
        }
    }

    // Primary text. Fall back to richText (Lexical JSON) if `text` is empty.
    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
    if !text.is_empty() {
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
    } else if let Some(rt) = b.get("richText") {
        if let Some(extracted) = lexical_plain_text(rt) {
            if !extracted.trim().is_empty() {
                m.content.push(Block::Text { text: extracted.into() });
            }
        }
    }

    // A tool call + its result, stored together in `toolFormerData`.
    if let Some(tf) = b.get("toolFormerData") {
        push_tool_former(&mut m, tf);
    }

    // Model on assistant bubbles: `modelInfo.modelName` (newer) or a bare `modelType`/`model`.
    if let Some(model) = b
        .pointer("/modelInfo/modelName")
        .or_else(|| b.get("modelType"))
        .or_else(|| b.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        m.model = Some(model.to_string());
    }
    if let Some(tc) = b.get("tokenCount") {
        let input = tc.get("inputTokens").and_then(Value::as_u64);
        let output = tc.get("outputTokens").and_then(Value::as_u64);
        if input.is_some() || output.is_some() {
            m.usage = Some(Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
                cost_usd: None,
            });
        }
    }
    // A bubble's own `unifiedMode` overrides the composer's (which `stream_global` puts on the
    // session bag) for this message only — a mode switched mid-thread.
    if let Some(mode) = b.get("unifiedMode").and_then(Value::as_str) {
        if !mode.is_empty() {
            m.harness_extra_mut(Harness::Cursor)
                .insert("unified_mode".into(), Value::String(mode.to_string()));
        }
    }

    (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
}

/// `toolFormerData` carries a single tool invocation and (usually) its result:
/// `{name, toolCallId, params|rawArgs (JSON string), result (JSON string), status, error}`.
fn push_tool_former(m: &mut Message, tf: &Value) {
    let name = tf.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
    let id = tf
        .get("toolCallId")
        .and_then(Value::as_str)
        .or_else(|| tf.get("modelCallId").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    // Arguments: prefer the model-issued `rawArgs`, fall back to normalized `params`.
    let input = tf
        .get("rawArgs")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .or_else(|| {
            tf.get("params")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
        })
        .or_else(|| tf.get("params").cloned())
        .unwrap_or(Value::Null);
    let tool_name = name.clone();
    m.content.push(Block::ToolUse {
        id: id.clone(),
        name,
        input,
        namespace: None,
    });

    // Result: a JSON string (or nested obj) plus a status/error flag.
    let status = tf.get("status").and_then(Value::as_str).unwrap_or("");
    let is_error = status.eq_ignore_ascii_case("error") || tf.get("error").map(|e| !e.is_null()).unwrap_or(false);
    let result_text = tf
        .get("result")
        .and_then(|r| match r {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
        .or_else(|| {
            tf.get("error").and_then(|e| match e {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            })
        });
    if let Some(content) = result_text {
        m.content.push(Block::ToolResult {
            tool_use_id: id,
            content: content.into(),
            is_error,
            tool_name: Some(tool_name),
            status: (!status.is_empty()).then(|| status.to_string()),
            details: None,
        });
    }
}

/// Extract concatenated plain text from a Lexical editor-state JSON tree (Cursor's `richText`).
/// The tree is `{root:{children:[{children:[{text,type:"text"},...]}]}}`. Best-effort; returns
/// None if it doesn't look like a Lexical state.
fn lexical_plain_text(rt: &Value) -> Option<String> {
    // richText may be either a JSON object or a JSON-encoded string.
    let owned;
    let v = if let Some(s) = rt.as_str() {
        owned = serde_json::from_str::<Value>(s).ok()?;
        &owned
    } else {
        rt
    };
    let root = v.get("root")?;
    let mut out = String::new();
    collect_lexical_text(root, &mut out);
    (!out.is_empty()).then_some(out)
}

fn collect_lexical_text(node: &Value, out: &mut String) {
    if let Some(t) = node.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for (i, child) in children.iter().enumerate() {
            collect_lexical_text(child, out);
            // paragraph break between top-level block children
            if i + 1 < children.len() && child.get("children").is_some() {
                out.push('\n');
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy in-workspace format (`workbench.panel.aichat.view.aichat.chatdata`)
// ---------------------------------------------------------------------------

/// Legacy key holding an array of chat tabs in the workspace ItemTable.
const LEGACY_CHAT_KEY: &str = "workbench.panel.aichat.view.aichat.chatdata";

fn discover_legacy_workspace(conn: &Connection, db: &Path, cwd: Option<&Path>, out: &mut Vec<SessionRef>) {
    let Some(raw) = item_table_get(conn, LEGACY_CHAT_KEY) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let Some(tabs) = v.get("tabs").and_then(Value::as_array) else {
        return;
    };
    for tab in tabs {
        let Some(tab_id) = tab.get("tabId").and_then(Value::as_str) else {
            continue;
        };
        let bubbles = tab.get("bubbles").and_then(Value::as_array);
        let count = bubbles.map(|b| b.len()).unwrap_or(0);
        if count == 0 {
            continue;
        }
        let title = tab
            .get("chatTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| crate::ir::truncate(s, 80));
        out.push(SessionRef {
            id: format!("legacy:{tab_id}"),
            harness: Harness::Cursor,
            path: db.to_path_buf(),
            cwd: cwd.map(Path::to_path_buf),
            title,
            created_at: None,
            updated_at: None,
            message_count: count,
        });
    }
}

fn stream_legacy(conn: &Connection, r: &SessionRef, tab_id: &str, sink: &mut dyn MessageSink) -> Result<Session> {
    let raw = item_table_get(conn, LEGACY_CHAT_KEY).context("legacy chatdata not found")?;
    let v: Value = serde_json::from_str(&raw).context("legacy chatdata JSON")?;
    let tabs = v
        .get("tabs")
        .and_then(Value::as_array)
        .context("legacy chatdata has no tabs")?;
    let tab = tabs
        .iter()
        .find(|t| t.get("tabId").and_then(Value::as_str) == Some(tab_id))
        .with_context(|| format!("legacy tab {tab_id} not found"))?;

    let title = tab
        .get("chatTitle")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| r.title.clone());
    let session = Session {
        id: r.id.clone(),
        harness: Harness::Cursor,
        cwd: r.cwd.clone(),
        title,
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    sink.meta(&session);
    if let Some(bubbles) = tab.get("bubbles").and_then(Value::as_array) {
        for b in bubbles {
            if let Some(m) = legacy_bubble_to_message(b) {
                if sink.message(m) == Flow::Stop {
                    break;
                }
            }
        }
    }
    Ok(session)
}

/// Legacy bubbles use a string `type` ("user"/"ai") and a `text` field.
fn legacy_bubble_to_message(b: &Value) -> Option<Message> {
    let role = match b.get("type").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("ai") | Some("assistant") => Role::Assistant,
        _ => Role::Assistant,
    };
    let mut m = Message::new(role);
    if let Some(text) = b.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
    }
    (!m.content.is_empty()).then_some(m)
}

/// Whole-`Session` convenience over [`stream_global`] for tests: stream into a [`CollectSink`].
#[cfg(test)]
fn parse_global(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_global(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

/// Whole-`Session` convenience over [`stream_legacy`] for tests.
#[cfg(test)]
fn parse_legacy(conn: &Connection, r: &SessionRef, tab_id: &str) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_legacy(conn, r, tab_id, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory state.vscdb resembling the global cursorDiskKV layout.
    fn mk_global() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);",
        )
        .unwrap();
        conn
    }

    fn put(conn: &Connection, key: &str, value: &str) {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Cursor,
            path: PathBuf::from(":memory:"),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    #[test]
    fn parses_headers_only_with_external_bubbles() {
        let conn = mk_global();
        let cid = "comp-1";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-1","name":"My Chat","createdAt":1700000000000,"lastUpdatedAt":1700000050000,"unifiedMode":"agent","_v":3,
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"How do I write a test?"}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b2"),
            r#"{"bubbleId":"b2","type":2,"text":"Here is how.","thinking":{"text":"let me think"},
                "unifiedMode":"chat",
                "tokenCount":{"inputTokens":10,"outputTokens":5},
                "toolFormerData":{"name":"run_terminal_cmd","toolCallId":"t1","status":"completed",
                  "rawArgs":"{\"command\":\"cargo test\"}","result":"{\"output\":\"ok\"}"}}"#,
        );

        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "comp-1");
        assert_eq!(out[0].title.as_deref(), Some("My Chat"));
        assert_eq!(out[0].message_count, 2);
        assert!(out[0].created_at.is_some());

        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.title.as_deref(), Some("My Chat"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("How do I write a test?"));

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // thinking block present
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { text, .. } if text == "let me think")));
        // text block present
        assert_eq!(a.text().as_deref(), Some("Here is how."));
        // tool use + result
        let tu = a.content.iter().find_map(|b| match b {
            Block::ToolUse { name, input, id, .. } => Some((name.clone(), input.clone(), id.clone())),
            _ => None,
        });
        let (name, input, tid) = tu.expect("tool use");
        assert_eq!(name, "run_terminal_cmd");
        assert_eq!(tid, "t1");
        assert_eq!(input.get("command").and_then(Value::as_str), Some("cargo test"));
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { tool_use_id, content, is_error, .. }
            if tool_use_id == "t1" && content.contains("ok") && !is_error)));
        // usage mapped
        assert_eq!(a.usage.as_ref().unwrap().input_tokens, Some(10));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(5));

        // kind/origin: user bubble is a prompt, assistant bubble a reply.
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        // `unifiedMode` rides nested under `extra["cursor"]`, never flat. The COMPOSER's mode is a
        // session fact (the mode the thread was opened in); a bubble's own overrides it per
        // message — here b2 switched to "chat" while the thread is an "agent" thread, and b1
        // (which carries no mode of its own) gets no message-level key at all.
        assert_eq!(
            s.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("agent"),
            "the composer's mode lands on the session bag"
        );
        assert_eq!(
            a.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("chat"),
            "the bubble's own mode overrides for that message"
        );
        assert!(s.messages[0]
            .harness_extra(Harness::Cursor)
            .is_none_or(|b| !b.contains_key("unified_mode")));
        // No flat keys survive anywhere: every top-level key is a namespace (the harness bag, cv's
        // own `cv` bag, or the `_record` carrier). See `docs/INTERFACE-V2.md` §4.
        assert!(
            !a.extra.contains_key("unifiedMode"),
            "no flat keys besides the harness bag"
        );
        crate::harness::assert_no_flat_keys(&s);
    }

    #[test]
    fn subagent_info_populates_lineage() {
        let conn = mk_global();
        let cid = "comp-sub";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-sub","_v":3,
                "subagentInfo":{"parentComposerId":"comp-parent","toolCallId":"tool_7","subagentTypeName":"explore"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"look"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("comp-parent"));
        assert_eq!(s.lineage.spawned_by_tool_use.as_deref(), Some("tool_7"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("explore"));
    }

    #[test]
    fn extracts_model_from_bubble_and_composer() {
        let conn = mk_global();
        let cid = "comp-model";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-model","_v":3,"modelConfig":{"modelName":"gpt-5","maxMode":true},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"answer","modelInfo":{"modelName":"claude-4.5-sonnet-thinking"}}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        // Per-message model from modelInfo.modelName; session model prefers the assistant's.
        assert_eq!(s.messages[0].model.as_deref(), Some("claude-4.5-sonnet-thinking"));
        assert_eq!(s.model.as_deref(), Some("claude-4.5-sonnet-thinking"));
    }

    #[test]
    fn session_model_falls_back_to_composer_config() {
        let conn = mk_global();
        let cid = "comp-cfg";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-cfg","_v":3,"modelConfig":{"modelName":"gpt-5"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"hi"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn parses_inline_conversation_old_shape() {
        let conn = mk_global();
        let cid = "comp-old";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-old","createdAt":1700000000000,"_v":1,
                "conversation":[{"bubbleId":"x1","type":1,"text":"hi"},{"bubbleId":"x2","type":2,"text":"hello"}]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_count, 2);
        // no name -> title falls back to None at discover; label() uses first user text.
        assert!(out[0].title.is_none());

        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
        assert_eq!(s.messages[1].text().as_deref(), Some("hello"));
    }

    #[test]
    fn empty_thread_is_skipped_in_discover() {
        let conn = mk_global();
        put(
            &conn,
            "composerData:empty",
            r#"{"composerId":"empty","_v":1,"conversation":[]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn richtext_fallback_when_text_empty() {
        let conn = mk_global();
        let cid = "comp-rt";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-rt","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        // text empty, richText holds a Lexical tree.
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"","richText":"{\"root\":{\"children\":[{\"children\":[{\"text\":\"from rich text\",\"type\":\"text\"}],\"type\":\"paragraph\"}],\"type\":\"root\"}}"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("from rich text"));
    }

    #[test]
    fn tool_error_marks_result_as_error() {
        let conn = mk_global();
        let cid = "comp-err";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-err","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"oops",
                "toolFormerData":{"name":"read_file","toolCallId":"t9","status":"error","params":"{\"path\":\"/x\"}","error":"not found"}}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        let tr = s.messages[0].content.iter().find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = tr.expect("tool result");
        assert!(is_error);
        assert!(content.contains("not found"));
    }

    #[test]
    fn missing_table_does_not_panic() {
        // A DB with no cursorDiskKV table at all (older layout) must yield no rows, not error.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT, value BLOB)", [])
            .unwrap();
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn legacy_workspace_chatdata() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)", [])
            .unwrap();
        let blob = r#"{"tabs":[{"tabId":"tab-1","chatTitle":"Old Tab",
            "bubbles":[{"type":"user","text":"legacy question"},{"type":"ai","text":"legacy answer"}]}]}"#;
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![LEGACY_CHAT_KEY, blob],
        )
        .unwrap();

        let mut out = Vec::new();
        discover_legacy_workspace(&conn, Path::new(":memory:"), Some(Path::new("/tmp/proj")), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "legacy:tab-1");
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/tmp/proj")));
        assert_eq!(out[0].message_count, 2);

        let mut r = sref("legacy:tab-1");
        r.cwd = Some(PathBuf::from("/tmp/proj"));
        let s = parse_legacy(&conn, &r, "tab-1").unwrap();
        assert_eq!(s.title.as_deref(), Some("Old Tab"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("legacy question"));
        assert_eq!(s.messages[1].role, Role::Assistant);
    }

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/proj"),
            Some(PathBuf::from("/Users/x/proj"))
        );
        assert_eq!(
            file_uri_to_path("file:///Users/x/Application%20Support"),
            Some(PathBuf::from("/Users/x/Application Support"))
        );
        assert_eq!(file_uri_to_path("vscode-remote://foo/bar"), None);
    }
}
