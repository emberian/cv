//! LM Studio desktop app adapter — local chats under `~/.lmstudio/conversations/`.
//!
//! LM Studio is a closed-source local-LLM desktop app, but it stores its chat transcripts as
//! **plaintext JSON**, one file per conversation:
//!
//! - **Transcripts:** `~/.lmstudio/conversations/<ms-epoch>.conversation.json`. The filename stem is
//!   a millisecond-epoch id (also the chat's `createdAt`). There is no `cwd` — it's a chat app, not
//!   an agent, so [`Session::cwd`] is always `None`.
//! - **Top-level fields:** `name` (user/auto title, may be `null` or `"Untitled"`), `createdAt`
//!   (ms epoch), `messages[]`, `systemPrompt`, `tokenCount`, `pinned`, `perChatPredictionConfig`
//!   (holds a per-chat `llm.prediction.systemPrompt` override under `fields[]`).
//! - **`messages[]`:** each entry is `{versions:[...], currentlySelected:<idx>}`. LM Studio keeps
//!   every regenerated/edited variant of a turn in `versions[]`; we render the `currentlySelected`
//!   one (falling back to the last).
//! - **A version** is `{type, role, ...}`:
//!     * `type:"singleStep"` (typically `role:"user"`): `content:[{type:"text",text} |
//!       {type:"file",fileIdentifier|identifier,name?,fileType,sizeBytes}]`.
//!     * `type:"multiStep"` (`role:"assistant"`): `steps:[...]` where each step `type` ∈
//!       `contentBlock` | `debugInfoBlock` | `status`.
//!         - `contentBlock`: `content:[{type:"text",text}]`. A `style.type=="thinking"` marks the
//!           block as reasoning (`<think>`); these get [`Block::Thinking`]. `genInfo` carries the
//!           model (`indexedModelIdentifier`/`identifier`, e.g. `openai/gpt-oss-120b`) and `stats`
//!           (`promptTokensCount`/`predictedTokensCount` → [`Usage`]). The `stepIdentifier` is a
//!           `"<ms-epoch>-<rand>"` string; its prefix is a usable per-step timestamp.
//!         - `debugInfoBlock` / `status`: app-internal (naming technique, prompt-processing status);
//!           ignored for content.
//!
//! Fidelity notes / variants (sniffed by content, never a version field):
//! - There are **no first-class tool-call structures** on disk. Models like gpt-oss embed tool calls
//!   inline in the assistant text via channel markers; we preserve that text verbatim (no `ToolUse`).
//! - File attachments are references only (`fileIdentifier`), not inlined bytes; images → [`Block::Image`],
//!   other files → [`Block::File`]. The actual bytes live under `~/.lmstudio/.internal/files/`.
//! - Older/newer schema drift is handled by being totally tolerant: unknown step/part types are
//!   skipped, a `content` that is a string OR an array of `{type,text}` parts both work, and a
//!   missing `~/.lmstudio` dir yields an empty discover (never a panic).

use super::Adapter;
use super::EmitResult;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct LmStudio {
    root: Option<PathBuf>,
}

impl LmStudio {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".lmstudio").join("conversations"));
        LmStudio {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for LmStudio {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for LmStudio {
    fn harness(&self) -> Harness {
        Harness::LmStudio
    }

    fn storage_root(&self) -> Option<PathBuf> {
        // Detect the install even if no conversations dir exists yet.
        self.root
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".lmstudio")).filter(|p| p.exists()))
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return Ok(vec![]),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !is_conversation_file(&path) {
                continue;
            }
            match scan(&path) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // The whole-session parse is "stream into a CollectSink" — see [`crate::stream::collect`].
        crate::stream::collect(self, r)
    }

    /// Native streaming parse. An LM Studio conversation is a single JSON document
    /// (`{name, createdAt, messages:[…], systemPrompt, perChatPredictionConfig, …}`), so rather than
    /// building the whole-document `Value` (which owns every `messages[]` turn — including every
    /// regenerated `versions[]` variant, easily gigabytes), we borrow-deserialize only the document
    /// *structure*: the small session-level fields by value (createdAt, the system-prompt sources,
    /// pinned) and `messages` as a `Vec<&RawValue>` slicing the mapped/owned bytes. Each turn is then
    /// turned into one small `Value` ([`parse_message`]), emitted to `sink`, and dropped before the
    /// next — so peak memory is O(largest turn) + the mmap (reclaimable) + the slice vec.
    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let bytes = read_bytes(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
        let doc: Doc =
            serde_json::from_slice(bytes.as_ref()).with_context(|| format!("parsing {}", r.path.display()))?;

        let created_at = doc.created_at.as_ref().and_then(super::ts_from_value);
        let updated_at = file_mtime(&r.path).or(created_at);

        let mut extra = serde_json::Map::new();
        // Preserve the per-chat / global system prompt so it isn't silently dropped. These
        // session-level fields are small, so capturing them as owned `Value`s costs nothing.
        if let Some(sp) = doc.system_prompt() {
            if !sp.trim().is_empty() {
                extra.insert("systemPrompt".into(), Value::String(sp.to_string()));
            }
        }
        if doc.pinned == Some(true) {
            extra.insert("pinned".into(), Value::Bool(true));
        }

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::LmStudio,
            cwd: None, // LM Studio is a chat app: no working directory.
            title: r.title.clone(),
            created_at,
            updated_at,
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra,
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // `model` is backfilled from the first genInfo seen while iterating turns, so hand the
        // metadata to the sink after the loop (the bridge sinks read it from the returned Session).
        let mut session_model: Option<String> = None;
        sink.meta(&s);
        for raw in doc.messages {
            // ONE small Value per turn — parsed, emitted, dropped before the next.
            let Ok(v) = serde_json::from_str::<Value>(raw.get()) else {
                continue;
            };
            if let Some(m) = parse_message(&v, &mut session_model) {
                if sink.message(m) == Flow::Stop {
                    break;
                }
            }
        }
        s.model = session_model;

        Ok(s)
    }
}

// ------------------------------------------------------------------------------------------------
// Streaming document structure (borrows the mapped/owned bytes)
// ------------------------------------------------------------------------------------------------

/// The document *structure* of an LM Studio conversation, borrowed from the source bytes: the small
/// session-level fields by value and `messages` as raw turn slices (NOT materialized into owned
/// `Value`s). Borrowing avoids building the whole-document `Value` for [`LmStudio::stream`].
#[derive(serde::Deserialize)]
struct Doc<'a> {
    #[serde(default, rename = "createdAt")]
    created_at: Option<Value>,
    #[serde(default, rename = "systemPrompt")]
    system_prompt: Option<String>,
    #[serde(default, rename = "perChatPredictionConfig")]
    per_chat_prediction_config: Option<Value>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default, borrow)]
    messages: Vec<&'a RawValue>,
}

impl Doc<'_> {
    /// The effective system prompt: the per-chat override
    /// (`perChatPredictionConfig.fields[key==llm.prediction.systemPrompt]`) takes precedence over
    /// the top-level `systemPrompt`. The borrowed counterpart of the test-only standalone
    /// `system_prompt` helper that pins this precedence rule.
    fn system_prompt(&self) -> Option<&str> {
        if let Some(fields) = self
            .per_chat_prediction_config
            .as_ref()
            .and_then(|c| c.pointer("/fields"))
            .and_then(Value::as_array)
        {
            for f in fields {
                if f.get("key").and_then(Value::as_str) == Some("llm.prediction.systemPrompt") {
                    if let Some(val) = f.get("value").and_then(Value::as_str) {
                        if !val.trim().is_empty() {
                            return Some(val);
                        }
                    }
                }
            }
        }
        self.system_prompt.as_deref().filter(|s| !s.trim().is_empty())
    }
}

/// Owns either an mmap of `path` or its bytes read into a `Vec`, exposing `.as_ref()` as `&[u8]`.
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

/// Memory-map `path` (default `mmap` feature) so a large conversation's bytes are paged in on demand
/// and reclaimable; fall back to reading the whole file into a `Vec` when the feature is off or the
/// map fails (e.g. a zero-length file).
fn read_bytes(path: &Path) -> std::io::Result<Bytes> {
    #[cfg(feature = "mmap")]
    {
        let file = fs::File::open(path)?;
        // SAFETY: cv reads its own session store; a concurrent truncation by another process could
        // in theory cause a SIGBUS, the standard caveat for all mmap'd file reads.
        if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
            return Ok(Bytes::Mapped(map));
        }
    }
    fs::read(path).map(Bytes::Owned)
}

// ------------------------------------------------------------------------------------------------
// Emit (port-a-sesh target): the faithful inverse of `parse`.
// ------------------------------------------------------------------------------------------------

/// Emit `session` into LM Studio's native format: a single
/// `<out_dir>/conversations/<ms-epoch>.conversation.json` file. The filename stem is the chat id
/// (also `createdAt`, ms-epoch), exactly as the parser expects.
///
/// This is the inverse of [`parse`]. Each IR [`Message`] becomes one `messages[]` turn wrapped in
/// `{versions:[<version>], currentlySelected:0}`:
/// - **User/System** → a `singleStep` version with `content` parts (`text` / `file`).
/// - **Assistant** → a `multiStep` version whose `steps[]` are `contentBlock`s: text → a plain
///   block, [`Block::Thinking`] → a block tagged `style.type=="thinking"` (the form the parser reads
///   back as reasoning), and tool calls/results are flattened to readable text blocks (LM Studio has
///   no first-class tool structures on disk, so we don't try to fabricate any). Model + usage ride
///   on the first content step's `genInfo`, and a `stepIdentifier` carries the step timestamp.
///
/// LM Studio is a chat app with no working directory, so `opts.new_cwd` / `session.cwd` are not
/// written into the file; we surface the chosen cwd only in the resume hint.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<EmitResult> {
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_ms = created.timestamp_millis();

    // The id (filename stem) is the ms-epoch createdAt unless explicitly overridden.
    let new_id = opts.new_id.clone().unwrap_or_else(|| created_ms.to_string());

    let conv_dir = out_dir.join("conversations");
    fs::create_dir_all(&conv_dir).with_context(|| format!("creating {}", conv_dir.display()))?;
    let file_path = conv_dir.join(format!("{new_id}.conversation.json"));

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        if let Some(version) = emit_version(session, msg, created) {
            messages.push(json!({
                "versions": [version],
                "currentlySelected": 0,
            }));
        }
    }

    let mut root = Map::new();
    let title = session
        .title
        .clone()
        .or_else(|| session.first_user_text())
        .map(|t| crate::ir::truncate(&t, 80))
        .unwrap_or_else(|| "Untitled".to_string());
    root.insert("name".into(), json!(title));
    root.insert("createdAt".into(), json!(created_ms));
    root.insert("messages".into(), Value::Array(messages));
    root.insert("tokenCount".into(), json!(0));
    root.insert("pinned".into(), json!(false));

    // Preserve any system prompt the source carried in `extra` (parse stashes it there).
    if let Some(sp) = session.extra.get("systemPrompt").and_then(Value::as_str) {
        root.insert("systemPrompt".into(), json!(sp));
    }

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(root))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some("open the chat in LM Studio".to_string()),
    })
}

/// Build the single `version` object for one IR message turn (the parser only reads the
/// `currentlySelected` version, so we emit exactly one). Returns `None` for empty turns.
fn emit_version(session: &Session, msg: &Message, fallback: DateTime<Utc>) -> Option<Value> {
    match msg.role {
        Role::Assistant => {
            let steps = emit_assistant_steps(session, msg, fallback);
            if steps.is_empty() {
                return None;
            }
            Some(json!({
                "type": "multiStep",
                "role": "assistant",
                "steps": steps,
            }))
        }
        // User, System, and Tool turns all become a single-step turn with content parts.
        role => {
            let content = emit_content_parts(&msg.content);
            if content.is_empty() {
                return None;
            }
            let role_str = match role {
                Role::System => "system",
                _ => "user", // Tool results have no native shape; carry them on a user turn.
            };
            Some(json!({
                "type": "singleStep",
                "role": role_str,
                "content": content,
            }))
        }
    }
}

/// Map an assistant message's blocks into LM Studio `steps[]` (`contentBlock`s). Model + usage ride
/// on the first step's `genInfo`; the `stepIdentifier` prefix carries the step timestamp.
fn emit_assistant_steps(session: &Session, msg: &Message, fallback: DateTime<Utc>) -> Vec<Value> {
    let ts = msg.timestamp.unwrap_or(fallback);
    let step_id = format!("{}-0.{}", ts.timestamp_millis(), "0000000000000000");
    let model = msg.model.as_ref().or(session.model.as_ref());

    let mut steps: Vec<Value> = Vec::new();
    let mut attached_gen = false;

    let mut push_block = |content: Value, thinking: bool, steps: &mut Vec<Value>| {
        let mut step = Map::new();
        step.insert("type".into(), json!("contentBlock"));
        step.insert("role".into(), json!("assistant"));
        step.insert("content".into(), content);
        step.insert("stepIdentifier".into(), json!(step_id));
        if thinking {
            step.insert("style".into(), json!({ "type": "thinking" }));
        }
        // Attach model + usage to the first content block, mirroring how the parser reads them.
        if !attached_gen {
            let mut gen = Map::new();
            if let Some(m) = model {
                gen.insert("indexedModelIdentifier".into(), json!(m));
            }
            if let Some(u) = &msg.usage {
                let mut stats = Map::new();
                if let Some(i) = u.input_tokens {
                    stats.insert("promptTokensCount".into(), json!(i));
                }
                if let Some(o) = u.output_tokens {
                    stats.insert("predictedTokensCount".into(), json!(o));
                }
                if !stats.is_empty() {
                    gen.insert("stats".into(), Value::Object(stats));
                }
            }
            if !gen.is_empty() {
                step.insert("genInfo".into(), Value::Object(gen));
                attached_gen = true;
            }
        }
        steps.push(Value::Object(step));
    };

    for b in &msg.content {
        match b {
            Block::Text { text } => {
                push_block(text_parts(text), false, &mut steps);
            }
            Block::Thinking { text, .. } => {
                if !text.is_empty() {
                    push_block(text_parts(text), true, &mut steps);
                }
            }
            Block::ToolUse { name, input, .. } => {
                // No native tool structure: render a readable, self-describing text block. The
                // parser reads it back as text (round-trips as content, not as a ToolUse).
                let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                push_block(text_parts(&format!("[tool call: {name} {args}]")), false, &mut steps);
            }
            Block::ToolResult { content, .. } => {
                push_block(text_parts(&format!("[tool result: {content}]")), false, &mut steps);
            }
            Block::File { .. } | Block::Image { .. } => {
                if let Some(part) = emit_file_part(b) {
                    push_block(Value::Array(vec![part]), false, &mut steps);
                }
            }
        }
    }
    steps
}

/// Map a (user/system/tool) message's blocks into a `content[]` parts array (`text` / `file`).
fn emit_content_parts(blocks: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            // Reasoning is unusual on a user turn, but keep the text rather than drop it.
            Block::Thinking { text, .. } if !text.is_empty() => out.push(json!({ "type": "text", "text": text })),
            Block::ToolResult { content, .. } => {
                out.push(json!({ "type": "text", "text": format!("[tool result: {content}]") }))
            }
            Block::ToolUse { name, input, .. } => {
                let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                out.push(json!({ "type": "text", "text": format!("[tool call: {name} {args}]") }))
            }
            Block::File { .. } | Block::Image { .. } => {
                if let Some(part) = emit_file_part(b) {
                    out.push(part);
                }
            }
            _ => {}
        }
    }
    out
}

/// A `content[]` array holding a single `text` part.
fn text_parts(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Map a [`Block::File`] / [`Block::Image`] into LM Studio's `{type:"file", ...}` part (a reference,
/// matching how the parser reads `identifier`/`fileIdentifier`, `name`, `fileType`).
fn emit_file_part(b: &Block) -> Option<Value> {
    let mut part = Map::new();
    part.insert("type".into(), json!("file"));
    match b {
        Block::Image { media_type, data_ref } => {
            part.insert("fileType".into(), json!("image"));
            if let Some(r) = data_ref {
                part.insert("identifier".into(), json!(r));
                part.insert("name".into(), json!(r));
            }
            if let Some(mt) = media_type {
                part.insert("mimeType".into(), json!(mt));
            }
        }
        Block::File { mime, path, source } => {
            if let Some(mt) = mime {
                part.insert("fileType".into(), json!(mt));
            }
            if let Some(s) = source {
                part.insert("identifier".into(), json!(s));
            }
            if let Some(p) = path {
                part.insert("name".into(), json!(p));
            }
        }
        _ => return None,
    }
    Some(Value::Object(part))
}

/// Files we treat as conversations: `*.conversation.json` (the real format) and, defensively, any
/// `*.json` directly in the conversations dir.
fn is_conversation_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.ends_with(".conversation.json") || name.ends_with(".json"),
        None => false,
    }
}

/// Build a cheap [`SessionRef`] without parsing every step.
fn scan(path: &Path) -> Result<Option<SessionRef>> {
    let text = fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text)?;

    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.trim_end_matches(".conversation.json")
                .trim_end_matches(".json")
                .to_string()
        })
        .unwrap_or_default();

    let title = chat_title(&v);
    let created_at = v.get("createdAt").and_then(super::ts_from_value);
    let updated_at = file_mtime(path).or(created_at);
    let message_count = v
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);

    Ok(Some(SessionRef {
        id,
        harness: Harness::LmStudio,
        path: path.to_path_buf(),
        cwd: None,
        title,
        created_at,
        updated_at,
        message_count,
    }))
}

/// Title: the explicit `name` (when meaningful), else the first user text. LM Studio writes
/// `"Untitled"` / `"Untitled - Branched"` for un-named chats, so we treat those as absent.
fn chat_title(v: &Value) -> Option<String> {
    if let Some(name) = v.get("name").and_then(Value::as_str) {
        let t = name.trim();
        if !t.is_empty() && t != "Untitled" && !t.starts_with("Untitled -") {
            return Some(crate::ir::truncate(t, 80));
        }
    }
    // Fall back to the first user text part.
    let messages = v.get("messages").and_then(Value::as_array)?;
    for msg in messages {
        let version = selected_version(msg)?;
        if version.get("role").and_then(Value::as_str) == Some("user") {
            let t = collect_text_parts(version.get("content"));
            if !t.trim().is_empty() {
                return Some(crate::ir::truncate(&t, 80));
            }
        }
    }
    None
}

/// The system prompt over a whole `Value`: the per-chat override (`perChatPredictionConfig.fields[
/// key==llm.prediction.systemPrompt]`) takes precedence over the top-level `systemPrompt`. The
/// streaming parser uses the borrowed [`Doc::system_prompt`] instead; this whole-`Value` form is
/// retained as the reference the unit test pins the precedence rule against.
#[cfg(test)]
fn system_prompt(v: &Value) -> Option<String> {
    if let Some(fields) = v.pointer("/perChatPredictionConfig/fields").and_then(Value::as_array) {
        for f in fields {
            if f.get("key").and_then(Value::as_str) == Some("llm.prediction.systemPrompt") {
                if let Some(val) = f.get("value").and_then(Value::as_str) {
                    if !val.trim().is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    v.get("systemPrompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Pick the `currentlySelected` version of a message turn, falling back to the last / first.
fn selected_version(msg: &Value) -> Option<&Value> {
    let versions = msg.get("versions").and_then(Value::as_array)?;
    if versions.is_empty() {
        return None;
    }
    let idx = msg
        .get("currentlySelected")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .filter(|i| *i < versions.len())
        .unwrap_or(versions.len() - 1);
    versions.get(idx)
}

/// Parse one message turn into an IR [`Message`]. Updates `session_model` with the first model seen.
fn parse_message(msg: &Value, session_model: &mut Option<String>) -> Option<Message> {
    let version = selected_version(msg)?;
    let role = match version.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);

    match version.get("type").and_then(Value::as_str) {
        // Assistant turns: a list of steps (contentBlock / debugInfoBlock / status).
        Some("multiStep") => {
            if let Some(steps) = version.get("steps").and_then(Value::as_array) {
                for step in steps {
                    parse_step(step, &mut m, session_model);
                }
            }
        }
        // User (and any other single-step) turns: content parts directly on the version.
        _ => {
            push_content_parts(version.get("content"), &mut m, false);
        }
    }

    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// Parse one assistant `step`. Only `contentBlock` carries content; others are app-internal.
fn parse_step(step: &Value, m: &mut Message, session_model: &mut Option<String>) {
    if step.get("type").and_then(Value::as_str) != Some("contentBlock") {
        return; // debugInfoBlock / status — nothing to render.
    }

    // Model + usage live on genInfo.
    if let Some(gen) = step.get("genInfo") {
        if session_model.is_none() {
            if let Some(model) = gen
                .get("indexedModelIdentifier")
                .or_else(|| gen.get("identifier"))
                .and_then(Value::as_str)
            {
                let model = model.to_string();
                if m.model.is_none() {
                    m.model = Some(model.clone());
                }
                *session_model = Some(model);
            }
        }
        if m.usage.is_none() {
            if let Some(u) = parse_usage(gen.get("stats")) {
                m.usage = Some(u);
            }
        }
    }

    // Per-step timestamp from the stepIdentifier prefix ("<ms-epoch>-<rand>").
    if m.timestamp.is_none() {
        if let Some(ts) = step.get("stepIdentifier").and_then(Value::as_str).and_then(step_id_ts) {
            m.timestamp = Some(ts);
        }
    }

    // A `style.type == "thinking"` block is chain-of-thought reasoning.
    let is_thinking = step.pointer("/style/type").and_then(Value::as_str) == Some("thinking");
    push_content_parts(step.get("content"), m, is_thinking);
}

/// Append text/file/image parts from a `content` array (or bare string) onto `m`.
/// When `as_thinking`, text parts become [`Block::Thinking`] instead of [`Block::Text`].
fn push_content_parts(content: Option<&Value>, m: &mut Message, as_thinking: bool) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                push_text(m, s.clone(), as_thinking);
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                push_text(m, t.to_string(), as_thinking);
                            }
                        }
                    }
                    Some("file") => push_file(m, part),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn push_text(m: &mut Message, text: String, as_thinking: bool) {
    if as_thinking {
        // Explicit reasoning steps (`style.type=="thinking"`) stay a single Thinking block.
        m.content.push(Block::Thinking {
            text: text.into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
    } else if crate::harmony::looks_like_harmony(&text) {
        // gpt-oss (common in LM Studio) embeds Harmony channel markers in plain assistant text:
        // analysis → Thinking, commentary tool calls → ToolUse, final → Text. Decode them into
        // first-class IR blocks instead of leaving the raw `<|channel|>…` markers in the text.
        m.content.extend(crate::harmony::decode_content(&text));
    } else {
        m.content.push(Block::Text { text: text.into() });
    }
}

/// A `{type:"file",...}` part. `fileType:"image"` → [`Block::Image`]; otherwise [`Block::File`].
/// The reference is `identifier`/`fileIdentifier` (bytes live under `.lmstudio/.internal/files/`).
fn push_file(m: &mut Message, part: &Value) {
    let data_ref = part
        .get("identifier")
        .or_else(|| part.get("fileIdentifier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = part.get("name").and_then(Value::as_str).map(str::to_string);
    let file_type = part.get("fileType").and_then(Value::as_str);

    if file_type == Some("image") {
        m.content.push(Block::Image {
            media_type: None,
            data_ref: data_ref.or(name),
        });
    } else {
        m.content.push(Block::File {
            mime: file_type.map(str::to_string),
            path: name,
            source: data_ref,
        });
    }
}

/// Concatenate `{type:"text"}` parts of a `content` value into a single string (for titles).
fn collect_text_parts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Map LM Studio's `genInfo.stats` to IR [`Usage`].
fn parse_usage(stats: Option<&Value>) -> Option<Usage> {
    let stats = stats?;
    let input = stats.get("promptTokensCount").and_then(Value::as_u64);
    let output = stats.get("predictedTokensCount").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
        cost_usd: None,
    })
}

/// A `stepIdentifier` is `"<ms-epoch>-<rand>"`; mine its prefix as a timestamp.
fn step_id_ts(id: &str) -> Option<DateTime<Utc>> {
    let prefix = id.split('-').next()?;
    let ms: i64 = prefix.parse().ok()?;
    // Sanity bound: between ~2010 and ~2100 (ms epoch).
    if !(1_200_000_000_000..=4_000_000_000_000).contains(&ms) {
        return None;
    }
    DateTime::from_timestamp_millis(ms)
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lmstudio")
            .join(name)
    }

    #[test]
    fn parses_user_assistant_thinking_and_usage() {
        let path = fixture("sample.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        assert_eq!(r.id, "sample");
        assert_eq!(r.title.as_deref(), Some("Hello there"));
        assert_eq!(r.message_count, 2);

        let s = LmStudio { root: None }.parse(&r).unwrap();
        assert!(s.cwd.is_none(), "lmstudio chats have no cwd");
        assert_eq!(s.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(s.messages.len(), 2);

        // user turn
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!(u.text().as_deref(), Some("Hello there"));

        // assistant turn: thinking block (from style.thinking) then text, plus model+usage.
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(a.content[0], Block::Thinking { .. }));
        match &a.content[0] {
            Block::Thinking { text, .. } => assert_eq!(text, "the user said hi"),
            _ => unreachable!(),
        }
        assert_eq!(a.text().as_deref(), Some("Hi! How can I help?"));
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-120b"));
        let usage = a.usage.as_ref().expect("usage present");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));
        assert!(a.timestamp.is_some(), "timestamp from stepIdentifier");
    }

    #[test]
    fn empty_messages_and_untitled_yield_no_title() {
        let v: Value = serde_json::from_str(r#"{"name":"Untitled","createdAt":1754424586190,"messages":[]}"#).unwrap();
        assert!(chat_title(&v).is_none());
    }

    #[test]
    fn per_chat_system_prompt_overrides_top_level() {
        let v: Value = serde_json::from_str(
            r#"{"systemPrompt":"global","perChatPredictionConfig":{"fields":[{"key":"llm.prediction.systemPrompt","value":"tsundere"}]}}"#,
        )
        .unwrap();
        assert_eq!(system_prompt(&v).as_deref(), Some("tsundere"));
    }

    #[test]
    fn user_file_image_part_becomes_image_block() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"look"},{"type":"file","name":"image.png","identifier":"123 - 4.png","fileType":"image","sizeBytes":100}]}]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert!(matches!(m.content[1], Block::Image { .. }));
        match &m.content[1] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("123 - 4.png")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn selects_currently_selected_version() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":1,"versions":[
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v0"}]},
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v1"}]}
            ]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("v1"));
    }

    #[test]
    fn missing_dir_discovers_empty() {
        let a = LmStudio { root: None };
        assert!(a.discover().unwrap().is_empty());
    }

    #[test]
    fn step_id_timestamp_parses_prefix() {
        let ts = step_id_ts("1754425094719-0.022738884687463323").unwrap();
        assert_eq!(ts.timestamp_millis(), 1754425094719);
        assert!(step_id_ts("garbage").is_none());
    }

    #[test]
    fn emit_round_trips_through_parse() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        // Build a small session: a user turn and an assistant turn with thinking + text.
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "Hello there".to_string().into(),
        });
        let mut assistant = Message::new(Role::Assistant);
        assistant.model = Some("openai/gpt-oss-120b".to_string());
        assistant.content.push(Block::Thinking {
            text: "the user said hi".to_string().into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        assistant.content.push(Block::Text {
            text: "Hi! How can I help?".to_string().into(),
        });
        assistant.usage = Some(Usage {
            input_tokens: Some(12),
            output_tokens: Some(34),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
        });

        let session = Session {
            id: "orig".to_string(),
            harness: Harness::LmStudio,
            cwd: None,
            title: Some("Greeting".to_string()),
            created_at: DateTime::from_timestamp_millis(1_754_425_094_719),
            updated_at: None,
            model: Some("openai/gpt-oss-120b".to_string()),
            git: None,
            messages: vec![user, assistant],
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_dir = std::env::temp_dir().join(format!("cv-lmstudio-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&out_dir);

        let result = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).unwrap();
        assert!(result.path.exists(), "emitted file exists");

        // Re-parse the emitted file with this adapter.
        let r = scan(&result.path).unwrap().unwrap();
        let parsed = LmStudio { root: None }.parse(&r).unwrap();

        assert_eq!(parsed.messages.len(), 2, "both turns survive");
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(
            parsed.messages[0].text().as_deref(),
            Some("Hello there"),
            "first user text survives"
        );

        let a = &parsed.messages[1];
        assert!(
            matches!(a.content[0], Block::Thinking { .. }),
            "thinking block round-trips via style.type==thinking"
        );
        assert_eq!(a.text().as_deref(), Some("Hi! How can I help?"));
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-120b"));
        let usage = a.usage.as_ref().expect("usage round-trips");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn gpt_oss_harmony_text_decodes_into_blocks() {
        // gpt-oss output stored by LM Studio as raw Harmony-framed assistant text should be decoded
        // into Thinking + ToolUse + Text instead of a single text block full of `<|channel|>` markers.
        let path = fixture("harmony.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        let s = LmStudio { root: None }.parse(&r).unwrap();
        assert_eq!(s.messages.len(), 2);

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // No raw Harmony markers should survive in the decoded blocks.
        assert_eq!(a.content.len(), 3, "thinking + tooluse + text");

        match &a.content[0] {
            Block::Thinking { text, .. } => {
                assert!(text.contains("get_weather"));
                assert!(!text.contains("<|channel|>"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &a.content[1] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "San Francisco, CA");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &a.content[2] {
            Block::Text { text } => {
                assert!(text.contains("sunny"));
                assert!(!text.contains("<|"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // Model + usage still parsed from genInfo.
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(68));
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first turn ends the parse with exactly one message seen, never materializing the rest.
    /// Session-level extras (the system prompt) are still captured up front.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-stream-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.conversation.json");
        fs::write(
            &path,
            r#"{"name":"T","createdAt":1754425094719,"systemPrompt":"be terse","messages":[
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"one"}]}]},
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"two"}]}]},
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"three"}]}]}
            ]}"#,
        )
        .unwrap();

        let r = scan(&path).unwrap().unwrap();

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
        let s = LmStudio { root: None }
            .stream(&r, &ParseOptions::full(), &mut sink)
            .unwrap();

        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        // Session-level extras survive without being subject to early-stop.
        assert_eq!(s.extra.get("systemPrompt").and_then(Value::as_str), Some("be terse"));

        let _ = fs::remove_dir_all(&dir);
    }
}
