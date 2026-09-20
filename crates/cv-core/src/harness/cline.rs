//! Cline adapter (VS Code extension) — per-task `api_conversation_history.json`.
//!
//! # Storage layout (reverse-engineered)
//!
//! Cline is a VS Code extension; its data lives in the *editor's* globalStorage, not a tidy dotdir.
//! Each editor that has Cline installed keeps a per-extension dir:
//!
//! - macOS: `~/Library/Application Support/<Editor>/User/globalStorage/saoudrizwan.claude-dev/`
//! - Linux: `~/.config/<Editor>/User/globalStorage/saoudrizwan.claude-dev/`
//! - Windows: `%APPDATA%/<Editor>/User/globalStorage/saoudrizwan.claude-dev/`
//!
//! where `<Editor>` ∈ {`Code`, `Code - Insiders`, `Cursor`, `VSCodium`}. We also probe a plain
//! `~/.cline/` (some builds mirror data there). Under each extension root, tasks live at:
//!
//! ```text
//! tasks/<taskId>/
//!   api_conversation_history.json   # array of raw Anthropic message objects (the transcript)
//!   ui_messages.json                # UI event log: [{ts, type:"say"|"ask", say/ask, text}, …]
//!   task_metadata.json              # {files_in_context[], model_usage[], …} (sometimes cwd hints)
//! ```
//!
//! `<taskId>` is a millisecond epoch timestamp string (e.g. `1717000000000`), which gives us
//! `created_at` for free.
//!
//! # `api_conversation_history.json` schema
//!
//! A JSON **array** of Anthropic Messages API message objects:
//! `{ role: "user"|"assistant", content: string | Block[] }` where each Block is one of
//! `{type:"text",text}`, `{type:"thinking",thinking,signature}`, `{type:"tool_use",id,name,input}`,
//! `{type:"tool_result",tool_use_id,content,is_error}`, `{type:"image",source}`. This maps almost
//! 1:1 onto the Claude Code block model (see `claude.rs`), so we reuse that mapping shape here.
//!
//! Cline encodes the cwd inside the **first user message**, in an `<environment_details>` block with
//! a line `# Current Working Directory (/abs/path) Files`. We extract it from there (and fall back to
//! `task_metadata.json`). A `user` turn that is *only* `tool_result` blocks is reclassified as a
//! [`Role::Tool`] turn, mirroring the Anthropic convention.
//!
//! # Roo Code
//!
//! Roo Code is a Cline fork with the **same** per-task file format under a different extension
//! namespace (`rooveterinaryinc.roo-cline` / `rooveterinaryinc.roo-code`) and `~/.roo/`. The real
//! parsing lives here as `pub` helpers; `roo.rs` reuses them with Roo's paths + `Harness::Roo`.

use super::Adapter;
use crate::ir::*;
use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The Cline VS Code extension id (its globalStorage subdir).
pub const CLINE_EXT_IDS: &[&str] = &["saoudrizwan.claude-dev"];
/// The Roo Code extension ids (it has shipped under two namespaces over its life).
pub const ROO_EXT_IDS: &[&str] = &["rooveterinaryinc.roo-cline", "rooveterinaryinc.roo-code"];

pub struct Cline {
    roots: Vec<PathBuf>,
}

impl Cline {
    pub fn new() -> Self {
        Cline {
            roots: task_roots(CLINE_EXT_IDS, ".cline"),
        }
    }
}

impl Default for Cline {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Cline {
    fn harness(&self) -> Harness {
        Harness::Cline
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_in(&self.roots, Harness::Cline))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        stream_task_dir(&r.path, &r.id, Harness::Cline, opts, sink)
    }
}

// ---------------------------------------------------------------------------
// Path discovery (shared with Roo)
// ---------------------------------------------------------------------------

/// All `tasks/` directories for a set of extension ids, across every editor's globalStorage plus a
/// plain `~/.<dotdir>/`. Only existing dirs are returned. Used by both Cline and Roo.
pub fn task_roots(ext_ids: &[&str], dotdir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for gs in global_storage_roots() {
        for ext in ext_ids {
            let tasks = gs.join(ext).join("tasks");
            if tasks.is_dir() {
                out.push(tasks);
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        let tasks = home.join(dotdir).join("tasks");
        if tasks.is_dir() {
            out.push(tasks);
        }
    }
    out
}

/// All plausible `<Editor>/User/globalStorage` roots on this platform that actually exist.
fn global_storage_roots() -> Vec<PathBuf> {
    let editors = ["Code", "Code - Insiders", "Cursor", "VSCodium"];
    let mut bases: Vec<PathBuf> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        // macOS
        bases.push(home.join("Library").join("Application Support"));
        // Linux / XDG
        bases.push(home.join(".config"));
    }
    // Windows
    if let Some(appdata) = std::env::var_os("APPDATA") {
        bases.push(PathBuf::from(appdata));
    }
    // Allow $XDG_CONFIG_HOME to override the Linux default.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        bases.push(PathBuf::from(xdg));
    }

    let mut out = Vec::new();
    for base in bases {
        for editor in editors {
            let gs = base.join(editor).join("User").join("globalStorage");
            if gs.is_dir() {
                out.push(gs);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Walk every `tasks/` root, producing one cheap `SessionRef` per `<taskId>/` dir that has an
/// `api_conversation_history.json`. Shared by Cline and Roo (the only difference is `harness`).
pub fn discover_in(roots: &[PathBuf], harness: Harness) -> Vec<SessionRef> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let history = dir.join("api_conversation_history.json");
            if !history.exists() {
                continue;
            }
            let Some(id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            if let Some(r) = scan(&dir, &id, harness) {
                out.push(r);
            }
        }
    }
    out
}

/// Cheap metadata-only scan of one task dir into a `SessionRef`.
fn scan(dir: &Path, id: &str, harness: Harness) -> Option<SessionRef> {
    let history = dir.join("api_conversation_history.json");
    let text = fs::read_to_string(&history).ok()?;
    let arr: Value = serde_json::from_str(&text).ok()?;
    let msgs = arr.as_array()?;

    let mut cwd = None;
    let mut title = None;
    let mut message_count = 0usize;
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if matches!(role, "user" | "assistant") {
            message_count += 1;
        }
        if role == "user" {
            let text = content_plain_text(m.get("content"));
            if cwd.is_none() {
                cwd = extract_cwd(&text);
            }
            if title.is_none() {
                if let Some(t) = clean_user_title(&text) {
                    title = Some(crate::ir::truncate(&t, 80));
                }
            }
        }
    }

    // created_at from the taskId (epoch millis); fall back to history mtime.
    let created_at = task_id_to_time(id);
    let updated_at = newest_mtime(dir).or(created_at);
    let created_at = created_at.or(updated_at);

    // task_metadata.json can carry a cwd hint when the first message lacked one.
    if cwd.is_none() {
        cwd = cwd_from_metadata(dir);
    }

    Some(SessionRef {
        id: id.to_string(),
        harness,
        path: dir.to_path_buf(),
        cwd,
        title,
        created_at,
        updated_at,
        message_count,
    })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Read a task dir (`api_conversation_history.json` + sidecars) into a `Session`. Tolerant: a
/// missing/empty/corrupt history yields an empty-message session rather than an error. Shared by
/// Cline and Roo.
///
/// This is the materializing twin of [`stream_task_dir`]: it streams into a [`CollectSink`] and
/// reattaches the messages, so it stays byte-identical with the streaming path (and never builds a
/// whole-document `Value`). Kept as a `pub` entry point because `roo.rs`'s emit round-trip test and
/// other callers re-parse a task dir directly.
pub fn parse_task_dir(dir: &Path, id: &str, harness: Harness) -> Result<Session> {
    let mut sink = CollectSink::default();
    let mut session = stream_task_dir(dir, id, harness, &ParseOptions::full(), &mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// Streaming core: read a task dir and emit each [`Message`] to `sink`, dropping it before the next,
/// so peak memory is O(largest message) rather than O(transcript). Returns the session metadata with
/// an **empty** `messages` vec. Shared by both Cline's and Roo's `stream`.
///
/// The OOM fix lives here: we do **not** `serde_json::from_str::<Value>` the whole
/// `api_conversation_history.json` (a single JSON document holding the message array). Instead we
/// memory-map it (or `fs::read` it when the `mmap` feature is off) and deserialize only the array
/// *structure* as `Vec<&RawValue>` — borrowing the bytes, so each message stays a cheap slice into
/// the mapped file. We then parse one message `Value` at a time and emit it, keeping at most one
/// message + the (file-backed, reclaimable) mapping + the slice vec resident.
pub fn stream_task_dir(
    dir: &Path,
    id: &str,
    harness: Harness,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let _ = opts; // Cline/Roo have no fat `extra` sidecar to gate; full fidelity either way.
    let history = dir.join("api_conversation_history.json");

    let mut session = Session {
        id: id.to_string(),
        harness,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(dir.to_path_buf()),
        extra: Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };

    // Sidecar metadata is small; resolve timestamps up front. cwd-from-metadata is only a *fallback*
    // (the first user turn's <environment_details> wins, mirroring the old parse order), so it's
    // applied after the body — kept here for the early `meta` signal only.
    let meta_cwd = cwd_from_metadata(dir);
    session.cwd = meta_cwd.clone();
    session.created_at = task_id_to_time(id);
    session.updated_at = newest_mtime(dir).or(session.created_at);
    // ui_messages.json gives the most precise created/updated and the first user turn's timestamp.
    let (ui_created, ui_updated, first_user_ts) = ui_message_times(dir);
    if let Some(c) = ui_created {
        session.created_at = Some(c);
    }
    if let Some(u) = ui_updated {
        if session.updated_at.map(|cur| u > cur).unwrap_or(true) {
            session.updated_at = Some(u);
        }
    }

    // Emit the early `meta` signal with the sidecar-derived fields (header-rendering sinks use it;
    // most ignore it). The authoritative cwd/title are folded from the transcript below and live on
    // the returned `Session`, which `collect`/`parse` reattach the messages to.
    sink.meta(&session);
    // Now clear cwd so the first user turn's <environment_details> takes priority; we re-apply the
    // metadata fallback after the body if the transcript yielded nothing.
    session.cwd = None;

    // Memory-map (or read) the history document. A missing/empty/corrupt file yields an
    // empty-message session rather than an error (tolerant, as before).
    let Some(bytes) = read_history_bytes(&history) else {
        session.cwd = meta_cwd;
        return Ok(session);
    };
    // Parse ONLY the array structure, borrowing the bytes: each item is a `&RawValue` slice, not an
    // owned message. If the document isn't a JSON array (corrupt / `{}`), tolerate it.
    let items: Vec<&RawValue> = match serde_json::from_slice(bytes.as_ref()) {
        Ok(items) => items,
        Err(_) => {
            session.cwd = meta_cwd;
            return Ok(session);
        }
    };

    let mut stamped_first_user = false;
    for item in &items {
        // ONE message Value at a time (small), parsed from the borrowed slice. Dropped at the end of
        // the loop body before the next item is touched.
        let Ok(v) = serde_json::from_str::<Value>(item.get()) else {
            continue;
        };
        if let Some(mut msg) = parse_message(&v, &mut session) {
            // Stamp the first user turn with the ui_messages first timestamp (best-effort), matching
            // the old `enrich_from_ui_messages` behavior.
            if !stamped_first_user && msg.role == Role::User {
                stamped_first_user = true;
                if let Some(ts) = first_user_ts {
                    msg.timestamp.get_or_insert(ts);
                }
            }
            if sink.message(msg) == Flow::Stop {
                break;
            }
        }
        drop(v);
    }

    // cwd fallback: the first user turn's <environment_details> wins; only if it yielded nothing do
    // we fall back to the task_metadata.json hint (matching the old parse order).
    if session.cwd.is_none() {
        session.cwd = meta_cwd;
    }

    Ok(session)
}

/// Memory-map the history file (default `mmap` feature) or fall back to a resident `fs::read`. The
/// returned `HistoryBytes` derefs to `&[u8]`; for the mmap path it is file-backed and reclaimable by
/// the OS under pressure. `None` on any I/O error (tolerant: caller yields an empty session).
fn read_history_bytes(path: &Path) -> Option<HistoryBytes> {
    #[cfg(feature = "mmap")]
    {
        let file = fs::File::open(path).ok()?;
        // SAFETY: standard mmap-of-a-file caveat — concurrent external truncation could fault. These
        // are the user's own transcript files, read-only, same as every other adapter's read.
        let map = unsafe { memmap2::Mmap::map(&file).ok()? };
        Some(HistoryBytes::Mapped(map))
    }
    #[cfg(not(feature = "mmap"))]
    {
        let buf = fs::read(path).ok()?;
        Some(HistoryBytes::Owned(buf))
    }
}

/// A byte buffer for the history document: a memory map under the `mmap` feature, otherwise an owned
/// `Vec<u8>`. Both `AsRef<[u8]>` so the `serde_json::from_slice` call is identical.
enum HistoryBytes {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    #[cfg(not(feature = "mmap"))]
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for HistoryBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            HistoryBytes::Mapped(m) => m.as_ref(),
            #[cfg(not(feature = "mmap"))]
            HistoryBytes::Owned(v) => v.as_slice(),
        }
    }
}

/// Pure parser: an `api_conversation_history.json` *array* text → `Session`. No filesystem access,
/// so it's unit-testable with a small fixture string. Shared by Cline and Roo. Drives the same
/// per-message parse as [`stream_task_dir`] (one message `Value` at a time, off `&RawValue` slices),
/// so the two paths stay byte-identical without building the whole-document `Value`.
pub fn parse_history_str(id: &str, text: &str, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let mut session = Session {
        id: id.to_string(),
        harness,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
        extra: Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };

    // Borrow the array structure as raw items; tolerate corrupt / non-array documents.
    let items: Vec<&RawValue> = match serde_json::from_str(text) {
        Ok(items) => items,
        Err(_) => return session,
    };

    for item in &items {
        let Ok(v) = serde_json::from_str::<Value>(item.get()) else {
            continue;
        };
        if let Some(msg) = parse_message(&v, &mut session) {
            session.messages.push(msg);
        }
    }
    session
}

/// Convert one Anthropic message object into an IR `Message`. Captures cwd/title from the first
/// user turn into `session` as a side effect.
fn parse_message(v: &Value, session: &mut Session) -> Option<Message> {
    let role = match v.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => return None,
    };

    let mut blocks = Vec::new();
    match v.get("content") {
        Some(Value::String(s)) => blocks.push(Block::Text { text: s.clone().into() }),
        Some(Value::Array(items)) => {
            for it in items {
                if let Some(b) = parse_block(it) {
                    blocks.push(b);
                }
            }
        }
        _ => {}
    }

    // Side effects from the first user turn: pull cwd + a clean title out of the prompt text, and
    // note the harness-injected `<task>` / `<environment_details>` wrappers (which are embedded in
    // the human's own text, so the turn stays a Prompt — we just flag that it carried them).
    let mut wraps_task = false;
    let mut wraps_env = false;
    if role == Role::User {
        let plain = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => text.inline_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        wraps_task = plain.contains("<task>");
        wraps_env = plain.contains("<environment_details>");
        if session.cwd.is_none() {
            session.cwd = extract_cwd(&plain);
        }
        if session.title.is_none() {
            if let Some(t) = clean_user_title(&plain) {
                session.title = Some(crate::ir::truncate(&t, 80));
            }
        }
    }

    // A user turn that is *only* tool_result blocks is really a Tool turn (Anthropic convention).
    let role =
        if role == Role::User && !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })) {
            Role::Tool
        } else {
            role
        };

    let mut m = Message::new(role);
    // A `system` message carries the system prompt (rare in Cline; it stores raw Anthropic turns).
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.content = blocks;
    if role == Role::System && session.system_prompt.is_none() {
        if let Some(t) = m.text() {
            if !t.trim().is_empty() {
                session.system_prompt = Some(t);
            }
        }
    }
    if role == Role::User && (wraps_task || wraps_env) {
        let bag = m.harness_extra_mut(session.harness);
        if wraps_task {
            bag.insert("wraps_task".into(), Value::Bool(true));
        }
        if wraps_env {
            bag.insert("wraps_environment_details".into(), Value::Bool(true));
        }
    }
    Some(m)
}

/// One Anthropic content block → IR `Block`. Mirrors `claude.rs::parse_block`.
fn parse_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: item.get("text").and_then(Value::as_str)?.to_string().into(),
        }),
        "thinking" => Some(Block::Thinking {
            text: item
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into(),
            signature: item.get("signature").and_then(Value::as_str).map(str::to_string),
            encrypted: None,
            redacted: false,
        }),
        "redacted_thinking" => Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: item.get("data").and_then(Value::as_str).map(str::to_string),
            redacted: true,
        }),
        "tool_use" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: item.get("input").cloned().unwrap_or(Value::Null),
            namespace: None,
        }),
        "tool_result" => Some(Block::ToolResult {
            tool_use_id: item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            content: coerce_text(item.get("content")).into(),
            is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            tool_name: None,
            status: None,
            details: None,
        }),
        "image" => {
            let source = item.get("source");
            let media_type = source
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let data_ref = source.and_then(|s| match s.get("type").and_then(Value::as_str) {
                Some("url") => s.get("url").and_then(Value::as_str).map(str::to_string),
                Some("base64") | None => s
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|_| "base64:inline".to_string()),
                Some(other) => Some(other.to_string()),
            });
            Some(Block::Image { media_type, data_ref })
        }
        _ => None,
    }
}

/// tool_result `content` is a string or `[{type:text,text}|{type:image,…}]`. Flatten to text,
/// replacing images with a placeholder. Mirrors `claude.rs::coerce_text`.
fn coerce_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("image") => Some("[image]".to_string()),
                _ => i.get("text").and_then(Value::as_str).map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Plain concatenated text of a message's text blocks (for cwd/title sniffing in `scan`).
fn content_plain_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") | None => i.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Emitting (Cline / Roo are conversion targets)
// ---------------------------------------------------------------------------

/// Emit `session` into Cline's per-task layout under `out_dir`:
///
/// ```text
/// <out_dir>/tasks/<taskId>/
///   api_conversation_history.json   # JSON array of Anthropic message objects (the transcript)
///   ui_messages.json                # minimal UI event log (created/updated timestamps)
///   task_metadata.json              # {cwd, files_in_context[], model_usage[]} hints
/// ```
///
/// This is the faithful inverse of [`parse_history_str`]: every IR [`Block`] maps back to the
/// Anthropic content block the parser expects (`text`, `thinking`, `tool_use`, `tool_result`,
/// `image`). A [`Role::Tool`] turn is written as a `user` message whose content is *only*
/// `tool_result` blocks, which the parser reclassifies back to [`Role::Tool`].
///
/// `<taskId>` comes from `opts.new_id`; otherwise it's a millisecond-epoch string derived from the
/// session's created/updated time (the parser turns the taskId back into `created_at`). The cwd
/// (`opts.new_cwd`, else `session.cwd`) is threaded into the first user turn's
/// `<environment_details>` block (where the parser extracts it from) and into `task_metadata.json`.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult> {
    emit_with_harness(session, out_dir, opts, Harness::Cline)
}

/// Shared emit core, parameterized by `harness` so `roo.rs` can reuse it with `Harness::Roo`.
///
/// The on-disk per-task format is byte-for-byte identical for Cline and Roo (the harness only
/// differs at parse time / in which storage root it lives under), so `harness` is accepted for API
/// symmetry and resume-hint flavor but doesn't change the emitted bytes.
pub fn emit_with_harness(
    session: &Session,
    out_dir: &Path,
    opts: &crate::emit::EmitOptions,
    harness: Harness,
) -> Result<crate::harness::EmitResult> {
    let _ = harness;
    // taskId: an explicit override, else a ms-epoch string the parser can turn back into created_at.
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| created.timestamp_millis().to_string());

    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd.as_ref().map(|p| p.to_string_lossy().to_string());

    let task_dir = out_dir.join("tasks").join(&new_id);
    fs::create_dir_all(&task_dir).with_context(|| format!("creating {}", task_dir.display()))?;

    // ── api_conversation_history.json ──
    let mut history: Vec<Value> = Vec::new();
    let mut first_user_seen = false;
    for msg in &session.messages {
        match msg.role {
            Role::User => {
                // Thread the cwd through the first user turn's <environment_details>, mirroring
                // where the parser reads it back from.
                let inject = if !first_user_seen {
                    first_user_seen = true;
                    cwd_str.as_deref()
                } else {
                    None
                };
                let mut blocks = emit_blocks(&msg.content);
                if let Some(cwd) = inject {
                    // Prepend a <task> title block (when we have one) so titles round-trip, then
                    // append the environment_details so cwd extraction finds it.
                    if let Some(title) = &session.title {
                        blocks.insert(0, json!({ "type": "text", "text": format!("<task>{title}</task>") }));
                    }
                    blocks.push(json!({
                        "type": "text",
                        "text": format!(
                            "<environment_details>\n# Current Working Directory ({cwd}) Files\n</environment_details>"
                        ),
                    }));
                }
                history.push(json!({ "role": "user", "content": Value::Array(blocks) }));
            }
            Role::Assistant => {
                history.push(json!({
                    "role": "assistant",
                    "content": Value::Array(emit_blocks(&msg.content)),
                }));
            }
            Role::Tool => {
                // A tool turn is a `user` message of only tool_result blocks; the parser
                // reclassifies it back to Role::Tool.
                history.push(json!({
                    "role": "user",
                    "content": Value::Array(emit_tool_result_blocks(&msg.content)),
                }));
            }
            Role::System => {
                history.push(json!({
                    "role": "system",
                    "content": Value::Array(emit_blocks(&msg.content)),
                }));
            }
        }
    }
    let history_path = task_dir.join("api_conversation_history.json");
    fs::write(&history_path, serde_json::to_string_pretty(&Value::Array(history))?)
        .with_context(|| format!("writing {}", history_path.display()))?;

    // ── task_metadata.json (cwd + model hints; sidecar the parser falls back to for cwd) ──
    let mut meta = Map::new();
    if let Some(c) = &cwd_str {
        meta.insert("cwd".into(), json!(c));
    }
    meta.insert("files_in_context".into(), json!([]));
    if let Some(model) = &session.model {
        meta.insert("model_usage".into(), json!([{ "model_id": model }]));
    }
    let meta_path = task_dir.join("task_metadata.json");
    fs::write(&meta_path, serde_json::to_string_pretty(&Value::Object(meta))?)
        .with_context(|| format!("writing {}", meta_path.display()))?;

    // ── ui_messages.json (minimal event log carrying created/updated timestamps) ──
    let created_ms = created.timestamp_millis();
    let updated_ms = session
        .updated_at
        .or(session.created_at)
        .unwrap_or(created)
        .timestamp_millis();
    let mut ui: Vec<Value> = Vec::new();
    if let Some(title) = &session.title {
        ui.push(json!({ "ts": created_ms, "type": "say", "say": "task", "text": title }));
    } else {
        ui.push(json!({ "ts": created_ms, "type": "say", "say": "task", "text": "" }));
    }
    if updated_ms != created_ms {
        ui.push(json!({ "ts": updated_ms, "type": "say", "say": "completion_result", "text": "" }));
    }
    let ui_path = task_dir.join("ui_messages.json");
    fs::write(&ui_path, serde_json::to_string_pretty(&Value::Array(ui))?)
        .with_context(|| format!("writing {}", ui_path.display()))?;

    let resume = match &cwd {
        Some(c) => format!(
            "open {} in Cline (task {new_id}, cwd {})",
            task_dir.display(),
            c.display()
        ),
        None => format!("open {} in Cline (task {new_id})", task_dir.display()),
    };
    Ok(crate::harness::EmitResult {
        path: task_dir,
        new_id,
        resume_hint: Some(resume),
    })
}

/// Map IR blocks → Anthropic content blocks, exactly as [`parse_block`] reads them. Tool results
/// are skipped here (they live on their own [`Role::Tool`] turn via [`emit_tool_result_blocks`]).
fn emit_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
            } => {
                if *redacted {
                    let mut m = Map::new();
                    m.insert("type".into(), json!("redacted_thinking"));
                    if let Some(enc) = encrypted {
                        m.insert("data".into(), json!(enc));
                    }
                    out.push(Value::Object(m));
                } else {
                    let mut m = Map::new();
                    m.insert("type".into(), json!("thinking"));
                    m.insert("thinking".into(), json!(text));
                    if let Some(sig) = signature {
                        m.insert("signature".into(), json!(sig));
                    }
                    out.push(Value::Object(m));
                }
            }
            Block::ToolUse { id, name, input, .. } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            Block::Image { media_type, data_ref } => {
                let mut source = Map::new();
                if let Some(mt) = media_type {
                    source.insert("media_type".into(), json!(mt));
                }
                match data_ref.as_deref() {
                    Some("base64:inline") | None => {
                        source.insert("type".into(), json!("base64"));
                    }
                    Some(r) => {
                        source.insert("type".into(), json!("url"));
                        source.insert("url".into(), json!(r));
                    }
                }
                out.push(json!({ "type": "image", "source": Value::Object(source) }));
            }
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            // Tool results don't belong on a non-Tool turn; emitted separately.
            Block::ToolResult { .. } => {}
        }
    }
    out
}

/// Map a [`Role::Tool`] turn's blocks → `tool_result` content blocks the parser pairs by id.
fn emit_tool_result_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } = b
        {
            out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// cwd / title / timestamp helpers
// ---------------------------------------------------------------------------

/// Pull the cwd out of Cline's `<environment_details>` block. The relevant line looks like:
/// `# Current Working Directory (/Users/me/proj) Files`. We take the text inside the parens.
pub fn extract_cwd(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# Current Working Directory") {
            let rest = rest.trim_start();
            let inner = rest.strip_prefix('(')?;
            let end = inner.find(')')?;
            let path = inner[..end].trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

/// Strip Cline's `<task>…</task>` / `<environment_details>…</environment_details>` wrappers to get a
/// clean human title from the first user message. Returns the first non-empty line of the remaining
/// prose, or `None` if nothing usable is left.
pub fn clean_user_title(text: &str) -> Option<String> {
    // Prefer the explicit `<task>…</task>` payload if present.
    if let Some(inner) = between(text, "<task>", "</task>") {
        let t = inner.trim();
        if !t.is_empty() {
            return first_line(t);
        }
    }
    // Otherwise drop the environment_details block and take the first prose line.
    let stripped = strip_block(text, "<environment_details>", "</environment_details>");
    first_line(stripped.trim())
}

fn between<'a>(hay: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = hay.find(open)? + open.len();
    let end = hay[start..].find(close)? + start;
    Some(&hay[start..end])
}

fn strip_block(hay: &str, open: &str, close: &str) -> String {
    if let (Some(s), Some(e)) = (hay.find(open), hay.find(close)) {
        if e >= s {
            let mut out = String::from(&hay[..s]);
            out.push_str(&hay[e + close.len()..]);
            return out;
        }
    }
    hay.to_string()
}

fn first_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// `<taskId>` is a millisecond epoch timestamp string. Convert it to a UTC time, when it parses.
fn task_id_to_time(id: &str) -> Option<DateTime<Utc>> {
    let ms: i64 = id.parse().ok()?;
    // Sanity range: a 13-digit ms epoch lands roughly 2001–2286.
    DateTime::<Utc>::from_timestamp_millis(ms)
}

/// Newest mtime among the task dir's known files (history + sidecars).
fn newest_mtime(dir: &Path) -> Option<DateTime<Utc>> {
    let mut newest: Option<DateTime<Utc>> = None;
    for name in [
        "api_conversation_history.json",
        "ui_messages.json",
        "task_metadata.json",
    ] {
        let p = dir.join(name);
        if let Ok(meta) = fs::metadata(&p) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    let dt = DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64);
                    if newest.map(|n| dt > n).unwrap_or(true) {
                        newest = Some(dt);
                    }
                }
            }
        }
    }
    newest
}

/// Best-effort cwd from `task_metadata.json`. Different Cline/Roo versions key it differently
/// (`cwd`, `workspace`, `workspacePath`, …) — probe the common ones.
fn cwd_from_metadata(dir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dir.join("task_metadata.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    for key in ["cwd", "workspace", "workspacePath", "workspaceFolder", "cwd_path"] {
        if let Some(p) = v.get(key).and_then(Value::as_str) {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

/// Read timestamp hints from `ui_messages.json` (an array of `{ts: epoch_ms, type, say/ask, text}`
/// events) without materializing message bodies. Returns `(created, updated, first_user_ts)`:
///
/// - `created`: the first event's `ts` — the most precise "created" we have (overrides taskId/mtime,
///   matching the old `enrich_from_ui_messages` which unconditionally set `created_at` from it).
/// - `updated`: the last event's `ts` (the caller keeps it only if it's newer than the mtime guess).
/// - `first_user_ts`: same first `ts`, which the streaming core stamps onto the first user turn.
///
/// `(created_at, updated_at, last_api_start)` extracted from a task's `ui_messages.json`.
type UiTimes = (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Small file (one entry per UI event, no transcript bodies), so a plain `from_str::<Value>` here is
/// fine — the OOM concern is `api_conversation_history.json`, not this sidecar.
fn ui_message_times(dir: &Path) -> UiTimes {
    let Ok(text) = fs::read_to_string(dir.join("ui_messages.json")) else {
        return (None, None, None);
    };
    let Ok(arr) = serde_json::from_str::<Value>(&text) else {
        return (None, None, None);
    };
    let Some(items) = arr.as_array() else {
        return (None, None, None);
    };

    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    for it in items {
        if let Some(ts) = it.get("ts").and_then(Value::as_i64) {
            if let Some(dt) = DateTime::<Utc>::from_timestamp_millis(ts) {
                first_ts.get_or_insert(dt);
                last_ts = Some(dt);
            }
        }
    }
    // created and first_user_ts are both the first event's ts (as the old code used `f` for both).
    (first_ts, last_ts, first_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cline/");
        std::fs::read_to_string(format!("{path}{name}")).unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn parses_anthropic_history_fixture() {
        let s = parse_history_str(
            "1717000000000",
            &fixture("api_conversation_history.json"),
            Harness::Cline,
            None,
        );
        assert_eq!(s.harness, Harness::Cline);
        // cwd extracted from the <environment_details> of the first user turn.
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/Users/me/proj")));
        // title is the clean <task> text, not the environment_details noise.
        assert_eq!(s.title.as_deref(), Some("Fix the parser bug"));

        // user, assistant, tool-result(user→Tool).
        assert_eq!(s.messages.len(), 3);

        let user = &s.messages[0];
        assert_eq!(user.role, Role::User);
        // The first user turn carries <task>/<environment_details>; it stays a Prompt, flagged.
        assert_eq!((user.kind, user.origin), (MessageKind::Prompt, Origin::Human));
        let bag = user.harness_extra(Harness::Cline).expect("cline bag");
        assert_eq!(bag.get("wraps_task"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(
            bag.get("wraps_environment_details"),
            Some(&serde_json::Value::Bool(true))
        );
        for k in user.extra.keys() {
            assert!(k == "cline" || k == "_record", "unexpected top-level extra key {k}");
        }

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!((asst.kind, asst.origin), (MessageKind::Reply, Origin::Model));
        assert!(matches!(&asst.content[0], Block::Thinking { signature: Some(sig), .. } if sig == "SIG=="));
        assert!(matches!(&asst.content[1], Block::Text { .. }));
        assert!(matches!(&asst.content[2], Block::ToolUse { name, .. } if name == "read_file"));

        // tool_result-only user line reclassified as Tool.
        let tool = &s.messages[2];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!((tool.kind, tool.origin), (MessageKind::ToolResult, Origin::Harness));
        match &tool.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert!(content.contains("fn main"));
                assert!(!is_error);
            }
            o => panic!("expected tool_result, got {o:?}"),
        }
    }

    #[test]
    fn task_id_epoch_ms_to_time() {
        // `parse_task_dir` derives created_at from the taskId via this helper.
        assert_eq!(
            task_id_to_time("1717000000000"),
            DateTime::<Utc>::from_timestamp_millis(1717000000000)
        );
        assert_eq!(task_id_to_time("not-a-number"), None);
    }

    #[test]
    fn extract_cwd_from_env_details() {
        let text = "<task>do x</task>\n<environment_details>\n# Current Working Directory (/Users/me/proj) Files\na.rs\n</environment_details>";
        assert_eq!(extract_cwd(text), Some(PathBuf::from("/Users/me/proj")));
        assert_eq!(extract_cwd("no cwd here"), None);
    }

    #[test]
    fn clean_title_prefers_task_block() {
        let text = "<task>Refactor the IR\nmore detail</task>\n<environment_details>noise</environment_details>";
        assert_eq!(clean_user_title(text).as_deref(), Some("Refactor the IR"));
        // Without a <task> block, fall back to first prose line after stripping env details.
        let text2 = "Just do the thing\n<environment_details>noise</environment_details>";
        assert_eq!(clean_user_title(text2).as_deref(), Some("Just do the thing"));
    }

    #[test]
    fn tolerates_corrupt_and_non_array() {
        assert!(parse_history_str("x", "not json", Harness::Cline, None)
            .messages
            .is_empty());
        assert!(parse_history_str("x", "{}", Harness::Cline, None).messages.is_empty());
    }

    #[test]
    fn emit_round_trips_through_parser() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        // Build a small Session: user, assistant (text + tool_use), tool result.
        let mut session = Session {
            id: "src".to_string(),
            harness: Harness::Cline,
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Fix the parser bug".to_string()),
            created_at: DateTime::<Utc>::from_timestamp_millis(1717000000000),
            updated_at: DateTime::<Utc>::from_timestamp_millis(1717000005000),
            model: Some("claude-x".to_string()),
            git: None,
            messages: Vec::new(),
            source_path: None,
            extra: Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        let mut user = Message::new(Role::User);
        user.content = vec![Block::Text {
            text: "please fix it".to_string().into(),
        }];
        session.messages.push(user);

        let mut asst = Message::new(Role::Assistant);
        asst.content = vec![
            Block::Text {
                text: "on it".to_string().into(),
            },
            Block::ToolUse {
                id: "toolu_1".to_string(),
                name: "read_file".to_string(),
                input: json!({ "path": "a.rs" }),
                namespace: None,
            },
        ];
        session.messages.push(asst);

        let mut tool = Message::new(Role::Tool);
        tool.content = vec![Block::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            content: "fn main() {}".to_string().into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        }];
        session.messages.push(tool);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_dir = std::env::temp_dir().join(format!("cv-cline-emit-{}-{}", std::process::id(), n));

        let cline = Cline::new();
        let res = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).expect("emit");
        assert_eq!(res.path, out_dir.join("tasks").join(&res.new_id));

        // Re-parse via this adapter's parse path.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Cline,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = cline.parse(&r).expect("re-parse");

        // Roles survive: user, assistant, tool.
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(parsed.messages[2].role, Role::Tool);

        // cwd + title threaded through the first user turn / metadata.
        assert_eq!(parsed.cwd.as_deref(), Some(std::path::Path::new("/Users/me/proj")));
        assert_eq!(parsed.title.as_deref(), Some("Fix the parser bug"));

        // Assistant text + tool_use blocks survive.
        assert!(matches!(&parsed.messages[1].content[0], Block::Text { text } if text == "on it"));
        assert!(matches!(
            &parsed.messages[1].content[1],
            Block::ToolUse { id, name, .. } if id == "toolu_1" && name == "read_file"
        ));

        // Tool result survives with id + content.
        match &parsed.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert!(content.contains("fn main"));
                assert!(!is_error);
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn content_can_be_bare_string() {
        let text = r#"[{"role":"user","content":"hello there"}]"#;
        let s = parse_history_str("x", text, Harness::Cline, None);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello there"));
    }
}
