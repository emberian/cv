//! Continue adapter — `~/.continue/sessions/` (continue.dev VS Code / JetBrains extension).
//!
//! ## Storage layout
//! Root is `$CONTINUE_GLOBAL_DIR/sessions` or `~/.continue/sessions`. It holds:
//! - **`sessions.json`** — the index: a JSON array of
//!   `{sessionId, title, dateCreated, workspaceDirectory}`. `dateCreated` is a string holding a
//!   millisecond epoch (e.g. `"1700000000000"`) on older builds, or an ISO-8601 timestamp on newer
//!   ones — we accept both. This gives id + title + cwd + created for a listing without opening each
//!   transcript.
//! - **`<sessionId>.json`** — one full session per file:
//!   `{sessionId, title, workspaceDirectory, history: [Item, ...]}`.
//!
//! ## `history[]` item (a `ChatHistoryItem` in Continue)
//! `{message: Message, contextItems: [ContextItem], editorState?, promptLogs?, ...}`. We map one
//! item to one IR `Message`.
//!
//! ### `message` (Continue uses OpenAI-ish `ChatMessage`s)
//! - `role`: `"user" | "assistant" | "system" | "tool"`.
//! - `content`: a **string** OR an array of parts (`{type:"text",text}` /
//!   `{type:"imageUrl",imageUrl:{url}}`). Both shapes are handled.
//! - `toolCalls` (assistant, newer Continue): `[{id, type:"function", function:{name, arguments}}]`.
//!   `arguments` is a JSON-encoded *string* (streamed, so possibly partial) → `Block::ToolUse`.
//! - `toolCallId` (role `tool`): the id of the call this result answers → `Block::ToolResult`.
//!
//! ### `contextItems[]` (attached files / snippets / docs)
//! `{name, description, content, uri?: {type, value}}`. Best-effort → `Block::File` (path/source from
//! `uri.value` or `description`), appended to the owning message so search can see them.
//!
//! ## Tolerance
//! Missing index → glob `*.json` (minus `sessions.json`). Missing fields → skip. `content` as
//! string or array both work. `dateCreated` ms-epoch or ISO both work. Never panics.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Continue {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
}

impl Continue {
    pub fn new() -> Self {
        let root = std::env::var_os("CONTINUE_GLOBAL_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".continue")));
        let sessions = root.map(|r| r.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }
}

impl Default for Continue {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Continue {
    fn harness(&self) -> Harness {
        Harness::Continue
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.sessions.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };

        // Index entries from sessions.json, if present.
        let index = read_index(&sessions.join("sessions.json"));

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Prefer the index, but only keep entries whose `<id>.json` actually exists.
        for meta in &index {
            let path = sessions.join(format!("{}.json", meta.session_id));
            if !path.exists() {
                continue;
            }
            seen.insert(meta.session_id.clone());
            out.push(ref_from(meta, &path));
        }

        // Fold in any `<id>.json` on disk that the index didn't mention (tolerant of a missing or
        // stale index).
        if let Ok(rd) = fs::read_dir(sessions) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name == "sessions.json" || !name.ends_with(".json") {
                    continue;
                }
                let id = name.trim_end_matches(".json").to_string();
                if seen.contains(&id) {
                    continue;
                }
                let meta = peek_session(&p, &id);
                out.push(ref_from(&meta, &p));
            }
        }

        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // The whole-session parse is "stream into a CollectSink" — see [`crate::stream::collect`].
        crate::stream::collect(self, r)
    }

    /// Native streaming parse. A Continue session is a single JSON document
    /// (`{title, workspaceDirectory, history:[Item,…]}`), so instead of building the whole-document
    /// `Value` (which owns every `history` item body — gigabytes for a large session), we
    /// borrow-deserialize only the document *structure*: the small session-level strings by value,
    /// and `history` as a `Vec<&RawValue>` slicing the mapped/owned bytes. Each item is then turned
    /// into one small `Value` ([`history_item_to_message`]), emitted to `sink`, and dropped before
    /// the next — so peak memory is O(largest item) + the mmap (reclaimable) + the slice vec.
    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Continue,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at.or_else(|| mtime(&r.path)),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // Map (or read) the file's raw bytes; the borrowed `Doc` slices into them.
        let Some(bytes) = read_bytes(&r.path) else {
            sink.meta(&s);
            return Ok(s);
        };
        let Ok(doc) = serde_json::from_slice::<Doc>(bytes.as_ref()) else {
            sink.meta(&s);
            return Ok(s);
        };

        // Backfill session-level fields from the file itself (index may be sparse/absent).
        if s.title.is_none() {
            s.title = doc
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| crate::ir::truncate(t, 80));
        }
        if s.cwd.is_none() {
            s.cwd = doc
                .workspace_directory
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(workspace_to_path);
        }

        // Metadata is fully known now (model is only ever backfilled per-item below, and the bridge
        // sinks read it from the returned Session); hand the header to the sink before the body.
        sink.meta(&s);

        for item in doc.history {
            // ONE small Value per item — parsed, emitted, dropped before the next.
            let Ok(v) = serde_json::from_str::<Value>(item.get()) else {
                continue;
            };
            if let Some(m) = history_item_to_message(&v, &mut s.model) {
                // A `system` history item is the system prompt — the session-level fact.
                if s.system_prompt.is_none() && m.kind == MessageKind::SystemPrompt {
                    if let Some(t) = m.text() {
                        if !t.trim().is_empty() {
                            s.system_prompt = Some(t);
                        }
                    }
                }
                if sink.message(m) == Flow::Stop {
                    break;
                }
            }
        }

        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Streaming document structure (borrows the mapped/owned bytes)
// ---------------------------------------------------------------------------

/// The document *structure* of a Continue session, borrowed from the source bytes: the small
/// session-level strings by value, and `history` as raw item slices (NOT materialized into owned
/// `Value`s). Borrowing avoids building the whole-document `Value` for [`Continue::stream`].
#[derive(serde::Deserialize)]
struct Doc<'a> {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "workspaceDirectory")]
    workspace_directory: Option<String>,
    #[serde(default, borrow)]
    history: Vec<&'a RawValue>,
}

/// Owns either an mmap of `path` or its bytes read into a `Vec`, exposing `.as_ref()` as `&[u8]`.
/// `None` on any read/map error (callers treat it as an empty/unparseable session, like the old
/// `read_to_string`/`from_str` did).
enum Bytes {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Bytes::Mapped(m) => m,
            Bytes::Owned(v) => v,
        }
    }
}

/// Memory-map `path` (default `mmap` feature) so a large session's bytes are paged in on demand and
/// reclaimable; fall back to reading the whole file into a `Vec` when the feature is off or the map
/// fails (e.g. a zero-length file, where mmap is invalid).
fn read_bytes(path: &Path) -> Option<Bytes> {
    #[cfg(feature = "mmap")]
    {
        if let Ok(file) = fs::File::open(path) {
            // SAFETY: cv reads its own session store; a concurrent truncation by another process
            // could in theory cause a SIGBUS, the standard caveat for all mmap'd file reads.
            if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
                return Some(Bytes::Mapped(map));
            }
        }
    }
    fs::read(path).ok().map(Bytes::Owned)
}

// ---------------------------------------------------------------------------
// Emit (IR -> Continue native layout)
// ---------------------------------------------------------------------------

/// Emit `session` into Continue's native layout under `out_dir` (the `sessions` directory):
///   `out_dir/<sessionId>.json`     — the transcript `{sessionId, title, workspaceDirectory, history}`
///   `out_dir/sessions.json`        — the index array (merged: our entry added/updated)
///
/// This is the faithful inverse of [`Continue::parse`] / [`Continue::discover`]:
/// - Each IR [`Message`] becomes one `history` item `{message, contextItems}`.
/// - `message` is the OpenAI-ish `ChatMessage` (`role`, string-or-array `content`, assistant
///   `toolCalls[]` with `arguments` as a JSON-encoded *string*, `role:"tool"` + `toolCallId`).
/// - `Block::File`/`Block::Image` on a turn become `contextItems[]` (so the parser's
///   `context_item_to_block` recovers them as `File` blocks); inline images also go in `content`.
///
/// The parser backfills `cwd`/`title` from the transcript itself, so writing the index is for
/// `discover()` fidelity, not correctness; we still write it.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<super::EmitResult> {
    use anyhow::Context;

    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ── transcript: <sessionId>.json ──
    let title = session
        .title
        .clone()
        .or_else(|| session.first_user_text())
        .map(|t| crate::ir::truncate(&t, 80));

    let mut history: Vec<Value> = Vec::new();
    for msg in &session.messages {
        history.push(history_item(msg, session.model.as_deref()));
    }

    let mut transcript = Map::new();
    transcript.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        transcript.insert("title".into(), json!(t));
    }
    if let Some(c) = &cwd_str {
        transcript.insert("workspaceDirectory".into(), json!(c));
    }
    transcript.insert("history".into(), Value::Array(history));

    let file_path = out_dir.join(format!("{new_id}.json"));
    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(transcript))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    // ── index: sessions.json (merge-or-create) ──
    let index_path = out_dir.join("sessions.json");
    let mut entries: Vec<Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    // Drop any stale entry for this id, then append a fresh one.
    entries.retain(|e| e.get("sessionId").and_then(Value::as_str) != Some(new_id.as_str()));
    let created = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(chrono::Utc::now);
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        entry.insert("title".into(), json!(t));
    }
    // `dateCreated` is a string holding millisecond epoch (the older-build shape, which the parser
    // accepts alongside ISO).
    entry.insert("dateCreated".into(), json!(created.timestamp_millis().to_string()));
    if let Some(c) = &cwd_str {
        entry.insert("workspaceDirectory".into(), json!(c));
    }
    entries.push(Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Array(entries))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(super::EmitResult {
        path: file_path,
        new_id,
        resume_hint: None,
    })
}

/// Build one `history` item (`{message, contextItems}`) from an IR message.
fn history_item(msg: &Message, session_model: Option<&str>) -> Value {
    let role_str = match msg.role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::User => "user",
    };

    let mut message = Map::new();
    message.insert("role".into(), json!(role_str));

    let mut context_items: Vec<Value> = Vec::new();

    match msg.role {
        Role::Tool => {
            // A tool result: Continue stores output in `content`, keyed by `toolCallId`.
            let (tool_use_id, content) = tool_result_of(&msg.content);
            message.insert("toolCallId".into(), json!(tool_use_id));
            message.insert("content".into(), json!(content));
        }
        _ => {
            // content: array of text/image parts (the array shape the parser handles).
            let mut parts: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => {
                        parts.push(json!({ "type": "text", "text": text }));
                    }
                    Block::Thinking { text, .. } => {
                        // Continue's ChatMessage has no thinking part; preserve as text so it's
                        // searchable and survives a round-trip as content.
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    Block::Image { data_ref, .. } => {
                        if let Some(url) = data_ref {
                            parts.push(json!({
                                "type": "imageUrl",
                                "imageUrl": { "url": url },
                            }));
                        }
                    }
                    Block::ToolUse { id, name, input, .. } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                // `arguments` is a JSON-encoded *string*.
                                "arguments": serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            },
                        }));
                    }
                    Block::File { path, source, mime } => {
                        context_items.push(file_context_item(path, source, mime));
                    }
                    Block::ToolResult { .. } => {}
                }
            }
            message.insert("content".into(), Value::Array(parts));
            if msg.role == Role::Assistant && !tool_calls.is_empty() {
                message.insert("toolCalls".into(), Value::Array(tool_calls));
            }
        }
    }

    let mut item = Map::new();
    item.insert("message".into(), Value::Object(message));
    item.insert("contextItems".into(), Value::Array(context_items));
    // Carry the model so the parser can backfill it (assistant turns).
    if msg.role == Role::Assistant {
        if let Some(model) = msg.model.as_deref().or(session_model) {
            item.insert("promptLogs".into(), json!([{ "modelTitle": model }]));
        }
    }
    Value::Object(item)
}

/// Map a `Block::File` to a Continue `contextItem` (`{name, description, uri:{type, value}}`) that
/// the parser's `context_item_to_block` round-trips back into a `File` block.
fn file_context_item(path: &Option<String>, source: &Option<String>, mime: &Option<String>) -> Value {
    let mut ci = Map::new();
    match (path, source) {
        (Some(p), _) => {
            ci.insert("name".into(), json!(file_name_of(p)));
            ci.insert("description".into(), json!(p));
            ci.insert("uri".into(), json!({ "type": "file", "value": p }));
        }
        (None, Some(s)) => {
            ci.insert("name".into(), json!(s));
            ci.insert("description".into(), json!(s));
            ci.insert("uri".into(), json!({ "type": "url", "value": s }));
        }
        (None, None) => {
            ci.insert("name".into(), json!("file"));
        }
    }
    if let Some(m) = mime {
        ci.insert("content".into(), json!(""));
        ci.insert("mime".into(), json!(m));
    }
    Value::Object(ci)
}

/// Basename of a path-ish string, for a `contextItem` name.
fn file_name_of(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// Pull `(toolCallId, content)` from a Tool turn's first ToolResult block.
fn tool_result_of(content: &[Block]) -> (String, String) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id, content, ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string());
        }
    }
    (String::new(), String::new())
}

// ---------------------------------------------------------------------------
// Index (sessions.json)
// ---------------------------------------------------------------------------

struct IndexMeta {
    session_id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
}

/// Read `sessions.json` (array of `{sessionId, title, dateCreated, workspaceDirectory}`). Empty vec
/// on any error.
fn read_index(path: &Path) -> Vec<IndexMeta> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let session_id = e.get("sessionId").and_then(Value::as_str)?.to_string();
            if session_id.is_empty() {
                return None;
            }
            Some(IndexMeta {
                session_id,
                title: e
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, 80)),
                cwd: e
                    .get("workspaceDirectory")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path),
                created_at: e.get("dateCreated").and_then(parse_date),
            })
        })
        .collect()
}

/// Build a `SessionRef` from index metadata + the on-disk transcript path.
fn ref_from(meta: &IndexMeta, path: &Path) -> SessionRef {
    let updated = mtime(path);
    SessionRef {
        id: meta.session_id.clone(),
        harness: Harness::Continue,
        path: path.to_path_buf(),
        cwd: meta.cwd.clone(),
        title: meta.title.clone(),
        created_at: meta.created_at.or(updated),
        updated_at: updated,
        message_count: count_history(path),
    }
}

/// Peek a `<id>.json` for title/cwd when there's no index row for it.
fn peek_session(path: &Path, id: &str) -> IndexMeta {
    let mut meta = IndexMeta {
        session_id: id.to_string(),
        title: None,
        cwd: None,
        created_at: None,
    };
    let Ok(text) = fs::read_to_string(path) else {
        return meta;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return meta;
    };
    meta.title = v
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(|t| crate::ir::truncate(t, 80));
    meta.cwd = v
        .get("workspaceDirectory")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(workspace_to_path);
    meta
}

/// Length of the `history` array, for `message_count`. 0 on any error.
fn count_history(path: &Path) -> usize {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    v.get("history").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// history[] -> IR
// ---------------------------------------------------------------------------

/// Convert one `history` item into an IR `Message`. `None` for an empty/message-less item. `model`
/// is filled opportunistically from a `promptLogs[].modelTitle`/`modelProvider` if present.
fn history_item_to_message(item: &Value, model: &mut Option<String>) -> Option<Message> {
    let message = item.get("message")?;
    let role_str = message.get("role").and_then(Value::as_str).unwrap_or("user");
    let role = match role_str {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    };

    let mut m = Message::new(role);
    // A `system` role history item carries the system prompt.
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }

    // Opportunistically capture the model from promptLogs (assistant turns).
    if model.is_none() {
        if let Some(mt) = item
            .get("promptLogs")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|l| l.get("modelTitle").or_else(|| l.get("modelProvider")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *model = Some(mt.to_string());
        }
    }

    match role {
        Role::Tool => {
            // A tool result. Continue stores the output in `content`.
            let content = coerce_content_text(message.get("content"));
            let tool_use_id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            m.content.push(Block::ToolResult {
                tool_use_id,
                content: content.into(),
                is_error: false,
                tool_name: None,
                status: Some("completed".into()),
                details: None,
            });
        }
        _ => {
            push_content(&mut m, message.get("content"));
            if role == Role::Assistant {
                if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
                    for c in calls {
                        let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        let func = c.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let input = parse_arguments(func.and_then(|f| f.get("arguments")));
                        m.content.push(Block::ToolUse {
                            id,
                            name,
                            input,
                            namespace: None,
                        });
                    }
                }
            }
        }
    }

    // Attached context items (files / snippets / docs) → File blocks (best-effort).
    if let Some(items) = item.get("contextItems").and_then(Value::as_array) {
        for ci in items {
            if let Some(b) = context_item_to_block(ci) {
                m.content.push(b);
            }
        }
    }

    (!m.content.is_empty()).then_some(m)
}

/// Push `message.content` (string or `[Part]`) onto a message as Text/Image blocks.
fn push_content(m: &mut Message, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                m.content.push(Block::Text { text: s.clone().into() });
            }
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(b) = part_to_block(p) {
                    m.content.push(b);
                }
            }
        }
        _ => {}
    }
}

/// One `type`-tagged content part → a Block.
fn part_to_block(p: &Value) -> Option<Block> {
    let ty = p.get("type").and_then(Value::as_str)?;
    match ty {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            (!text.is_empty()).then_some(Block::Text { text: text.into() })
        }
        "imageUrl" => {
            let url = p
                .get("imageUrl")
                .and_then(|iu| iu.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Block::Image {
                media_type: None,
                data_ref: url,
            })
        }
        _ => None,
    }
}

/// A `contextItem` → a `File` block (best-effort). The path/source is taken from `uri.value` (a file
/// path or URL) and falls back to `description`/`name`.
fn context_item_to_block(ci: &Value) -> Option<Block> {
    let uri_val = ci
        .get("uri")
        .and_then(|u| u.get("value"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let desc = ci.get("description").and_then(Value::as_str).filter(|s| !s.is_empty());
    let name = ci.get("name").and_then(Value::as_str).filter(|s| !s.is_empty());

    let uri_type = ci
        .get("uri")
        .and_then(|u| u.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Nothing identifiable → skip.
    if uri_val.is_none() && desc.is_none() && name.is_none() {
        return None;
    }

    // `uri.type == "file"` → a filesystem path; otherwise treat the value as an opaque source/URL.
    let (path, source) = match (uri_val, uri_type) {
        (Some(v), "file") => (Some(v.to_string()), None),
        (Some(v), _) => (None, Some(v.to_string())),
        (None, _) => (None, desc.or(name).map(str::to_string)),
    };

    Some(Block::File {
        mime: None,
        path,
        source,
    })
}

/// Flatten `content` (string or `[Part]`) to plain text (text parts joined by newline).
fn coerce_content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `arguments` is a JSON-encoded *string* (possibly streamed/partial). Parse it; null/empty ⇒ `{}`.
/// A non-empty but unparseable string is stashed under `_raw` so nothing is lost.
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(parsed) => parsed,
            Err(_) => {
                let mut map = serde_json::Map::new();
                map.insert("_raw".into(), Value::String(s.clone()));
                Value::Object(map)
            }
        },
        Some(Value::Object(_)) => v.cloned().unwrap(),
        _ => Value::Object(Default::default()),
    }
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Continue's `workspaceDirectory` may be a `file://` URI or a plain path. Normalize to a path.
fn workspace_to_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("file://") {
        // file:///Users/... -> /Users/... (drop the empty authority's leading slash group).
        PathBuf::from(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Parse a `dateCreated` value: a number or numeric-string of **milliseconds** since epoch, or an
/// ISO-8601 string.
fn parse_date(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => n.as_i64().and_then(ms_to_dt),
        Value::String(s) => {
            let s = s.trim();
            if let Ok(ms) = s.parse::<i64>() {
                return ms_to_dt(ms);
            }
            super::parse_ts(s)
        }
        _ => None,
    }
}

fn ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/continue")
    }

    #[test]
    fn date_parsing_ms_and_iso() {
        let ms = parse_date(&Value::String("1700000000000".into())).unwrap();
        assert_eq!(ms.timestamp_millis(), 1700000000000);
        let num = parse_date(&Value::Number(1700000000000i64.into())).unwrap();
        assert_eq!(num.timestamp_millis(), 1700000000000);
        let iso = parse_date(&Value::String("2026-05-29T12:00:00Z".into())).unwrap();
        assert_eq!(iso.to_rfc3339(), "2026-05-29T12:00:00+00:00");
        assert!(parse_date(&Value::Null).is_none());
    }

    #[test]
    fn workspace_uri_and_plain() {
        assert_eq!(workspace_to_path("/Users/me/proj"), PathBuf::from("/Users/me/proj"));
        assert_eq!(
            workspace_to_path("file:///Users/me/proj"),
            PathBuf::from("/Users/me/proj")
        );
    }

    #[test]
    fn user_string_and_assistant_toolcall() {
        let mut model = None;
        let user: Value =
            serde_json::from_str(r#"{"message":{"role":"user","content":"hello"},"contextItems":[]}"#).unwrap();
        let m = history_item_to_message(&user, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let asst: Value = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":"let me check","toolCalls":[
                {"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&asst, &mut model).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.text().as_deref(), Some("let me check"));
        match m.content.iter().find(|b| matches!(b, Block::ToolUse { .. })) {
            Some(Block::ToolUse { id, name, input, .. }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "a.rs");
            }
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn content_array_text_and_image() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":[
                {"type":"text","text":"look at this"},
                {"type":"imageUrl","imageUrl":{"url":"data:image/png;base64,AAA"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("look at this"));
        assert!(m.content.iter().any(|b| matches!(
            b,
            Block::Image { data_ref, .. } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        )));
    }

    #[test]
    fn kind_and_origin_across_roles() {
        let mut model = None;
        let mk = |line: &str, model: &mut Option<String>| {
            history_item_to_message(&serde_json::from_str::<Value>(line).unwrap(), model).unwrap()
        };
        let u = mk(r#"{"message":{"role":"user","content":"hi"}}"#, &mut model);
        assert_eq!((u.kind, u.origin), (MessageKind::Prompt, Origin::Human));
        let a = mk(r#"{"message":{"role":"assistant","content":"yo"}}"#, &mut model);
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        let t = mk(
            r#"{"message":{"role":"tool","content":"out","toolCallId":"c1"}}"#,
            &mut model,
        );
        assert_eq!((t.kind, t.origin), (MessageKind::ToolResult, Origin::Harness));
        let sys = mk(r#"{"message":{"role":"system","content":"you are terse"}}"#, &mut model);
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );
        // No flat extra keys were introduced.
        for m in [&u, &a, &t, &sys] {
            for k in m.extra.keys() {
                assert!(k == "continue" || k == "_record", "unexpected extra key {k}");
            }
        }
    }

    #[test]
    fn tool_result_message() {
        let mut model = None;
        let v: Value =
            serde_json::from_str(r#"{"message":{"role":"tool","content":"file contents here","toolCallId":"call_1"}}"#)
                .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id, content, ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents here");
            }
            _ => panic!("expected tool_result"),
        }
    }

    #[test]
    fn context_items_become_files() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":"check these"},"contextItems":[
                {"name":"main.rs","description":"src/main.rs","content":"fn main(){}","uri":{"type":"file","value":"/proj/src/main.rs"}},
                {"name":"Docs","description":"react docs","content":"...","uri":{"type":"url","value":"https://react.dev"}}
            ]}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        let files: Vec<_> = m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::File { path, source, .. } => Some((path.clone(), source.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0.as_deref(), Some("/proj/src/main.rs"));
        assert_eq!(files[1].1.as_deref(), Some("https://react.dev"));
    }

    #[test]
    fn arguments_garbage_preserved() {
        let v = parse_arguments(Some(&Value::String("{partial".into())));
        assert_eq!(v["_raw"], "{partial");
        assert_eq!(parse_arguments(None), Value::Object(Default::default()));
    }

    #[test]
    fn parses_fixture_session() {
        let dir = fixtures();
        let path = dir.join("11111111-1111-1111-1111-111111111111.json");
        if !path.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Continue,
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Continue { sessions: None }.parse(&r).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::File { .. }))));
    }

    #[test]
    fn emit_round_trips() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);

        // Build a small session: user, assistant w/ a tool call, a tool result.
        let mut session = Session {
            id: "src".into(),
            harness: Harness::Continue,
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Roundtrip".into()),
            created_at: None,
            updated_at: None,
            model: Some("gpt-4o".into()),
            git: None,
            messages: Vec::new(),
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "please read a.rs".into(),
        });
        session.messages.push(user);

        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text { text: "on it".into() });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.rs" }),
            namespace: None,
        });
        session.messages.push(asst);

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        session.messages.push(tool);

        let opts = crate::emit::EmitOptions {
            new_id: Some("emit-test-id".into()),
            new_cwd: None,
            ..Default::default()
        };
        let res = emit(&session, &dir, &opts).unwrap();
        assert_eq!(res.new_id, "emit-test-id");
        assert!(res.path.exists());

        // Re-parse via this adapter.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Continue,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Continue { sessions: None }.parse(&r).unwrap();

        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[0].text().as_deref(), Some("please read a.rs"));
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert!(parsed.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert_eq!(parsed.messages[2].role, Role::Tool);
        assert!(parsed.messages[2].content.iter().any(|b| matches!(
            b,
            Block::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_1" && content == "fn main() {}"
        )));

        // The index should also discover it.
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        assert!(refs.iter().any(|r| r.id == "emit-test-id"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovers_from_index() {
        let dir = fixtures();
        if !dir.join("sessions.json").exists() {
            return;
        }
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        let r = refs
            .iter()
            .find(|r| r.id == "11111111-1111-1111-1111-111111111111")
            .expect("indexed session discovered");
        assert_eq!(r.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(r.message_count >= 3);
        assert!(r.created_at.is_some());
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first message ends the parse with exactly one message seen, never materializing the rest.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-stream-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        fs::write(
            &path,
            r#"{"sessionId":"s","title":"T","workspaceDirectory":"/w","history":[
                {"message":{"role":"user","content":"one"}},
                {"message":{"role":"assistant","content":"two"}},
                {"message":{"role":"user","content":"three"}}
            ]}"#,
        )
        .unwrap();

        let r = SessionRef {
            id: "s".into(),
            harness: Harness::Continue,
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };

        // A sink that records every message it sees and stops after the first.
        struct StopFirst {
            seen: Vec<String>,
        }
        impl MessageSink for StopFirst {
            fn message(&mut self, m: Message) -> Flow {
                self.seen.push(m.text().unwrap_or_default());
                Flow::Stop
            }
        }
        let mut sink = StopFirst { seen: Vec::new() };
        let s = Continue { sessions: None }
            .stream(&r, &ParseOptions::full(), &mut sink)
            .unwrap();

        // Only the first message was ever materialized; the returned Session is metadata-only.
        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/w")));

        let _ = fs::remove_dir_all(&dir);
    }
}
