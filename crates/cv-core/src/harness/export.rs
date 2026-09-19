//! **Account data-export** adapters — the archives you download from ChatGPT ("Export data") and
//! Claude.ai. Both pack *many* conversations into one big JSON file (`conversations.json`, often split
//! `conversations-000.json … -NNN.json`), so unlike the per-session harnesses one file yields many
//! [`SessionRef`]s. Two shapes, sniffed by content (never a filename):
//!
//! - **ChatGPT export** (`Harness::ChatGptExport`): each conversation has a `mapping` — a DAG of
//!   `node_id -> {parent, children[], message}` — with `current_node` as the active leaf. We walk the
//!   root→`current_node` chain (the live branch). A message has `author.role`, `content.{content_type,
//!   parts|text}`, and `recipient` (≠`all` ⇒ a tool call). `default_model_slug` is the model.
//! - **Claude.ai export** (`Harness::ClaudeExport`): each conversation has `chat_messages[]`, linear
//!   via `parent_message_uuid`; `sender` ∈ human/assistant; `content[]` blocks are
//!   text/thinking/tool_use/tool_result.
//!
//! Distinct from `claude` (Claude Code JSONL), `claude-app`/`chatgpt-app` (desktop apps). Exports have
//! no fixed home, so discovery scans `$CV_EXPORTS` (`:`-separated dirs/files) or, by default,
//! `~/Downloads` — for `conversations*.json` files. Set `CV_EXPORTS` to a tighter dir if scanning
//! Downloads is slow.

use super::{ts_from_value, Adapter};
use crate::ir::*;
use anyhow::{Context, Result};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ChatGpt,
    Claude,
}

impl Kind {
    fn harness(self) -> Harness {
        match self {
            Kind::ChatGpt => Harness::ChatGptExport,
            Kind::Claude => Harness::ClaudeExport,
        }
    }
}

// ── Adapters (thin wrappers over the shared engine, one per export shape) ──────

pub struct ChatGptExport;
impl ChatGptExport {
    pub fn new() -> Self {
        ChatGptExport
    }
}
impl Default for ChatGptExport {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ClaudeExport;
impl ClaudeExport {
    pub fn new() -> Self {
        ClaudeExport
    }
}
impl Default for ClaudeExport {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for ChatGptExport {
    fn harness(&self) -> Harness {
        Harness::ChatGptExport
    }
    fn storage_root(&self) -> Option<PathBuf> {
        export_dirs().into_iter().next()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::ChatGpt))
    }
    fn parse(&self, r: &SessionRef) -> Result<Session> {
        parse_ref(Kind::ChatGpt, r)
    }
}

impl Adapter for ClaudeExport {
    fn harness(&self) -> Harness {
        Harness::ClaudeExport
    }
    fn storage_root(&self) -> Option<PathBuf> {
        export_dirs().into_iter().next()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::Claude))
    }
    fn parse(&self, r: &SessionRef) -> Result<Session> {
        parse_ref(Kind::Claude, r)
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Where to look for export archives — the registered sources from the config file
/// (`cv config --add-export <path>`; see [`crate::config`]), unioned with `$CV_EXPORTS` (a
/// `:`-separated ad-hoc list). **Opt-in**: with nothing registered and the env unset, export discovery
/// is a no-op, so the default `cv ls`/`discover_all` never pays to scan (account exports are huge —
/// enumerating thousands of conversations across multi-MB files takes seconds).
fn export_dirs() -> Vec<PathBuf> {
    let mut dirs = crate::config::load().exports;
    if let Ok(spec) = std::env::var("CV_EXPORTS") {
        dirs.extend(spec.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    dirs.retain(|p| p.exists());
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Every `conversations*.json` under the export dirs (depth ≤ 2; a dir entry is scanned, a file entry
/// is taken directly).
fn export_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in export_dirs() {
        if root.is_file() {
            if is_conversations_file(&root) {
                out.push(root);
            }
            continue;
        }
        // root dir + one level of subdirs (an extracted export is usually a subfolder)
        for depth1 in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let p = depth1.path();
            if p.is_file() && is_conversations_file(&p) {
                out.push(p);
            } else if p.is_dir() {
                for depth2 in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let q = depth2.path();
                    if q.is_file() && is_conversations_file(&q) {
                        out.push(q);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_conversations_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "conversations.json" || (n.starts_with("conversations") && n.ends_with(".json")))
}

/// Read a file's top-level conversation array as borrowed raw slices (no per-conversation
/// materialization until we pick one).
fn read_array(path: &Path) -> Result<(String, Vec<Box<RawValue>>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading export {}", path.display()))?;
    // Tolerate either a bare array or a single object; non-arrays yield nothing.
    let arr: Vec<Box<RawValue>> = serde_json::from_str(&text).unwrap_or_default();
    Ok((text, arr))
}

/// Enumerate every conversation of `kind` across all export files as lightweight [`SessionRef`]s.
/// Reads each file once: parse the array, sniff the kind from its first element, then enumerate.
/// Dedups by conversation id (keeping the first) — overlapping exports (e.g. a loose
/// `conversations-000.json` plus an extracted full archive) commonly repeat the same conversation.
fn discover_kind(kind: Kind) -> Vec<SessionRef> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in export_files() {
        let Ok((_text, arr)) = read_array(&path) else { continue };
        let is_kind = arr
            .first()
            .and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok())
            .is_some_and(|v| match kind {
                Kind::ChatGpt => v.get("mapping").is_some(),
                Kind::Claude => v.get("chat_messages").is_some(),
            });
        if !is_kind {
            continue;
        }
        for raw in &arr {
            let Ok(v) = serde_json::from_str::<Value>(raw.get()) else {
                continue;
            };
            if let Some(r) = conv_ref(kind, &path, &v) {
                if seen.insert(r.id.clone()) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// Build a [`SessionRef`] for one conversation `v` (metadata only — counts message turns cheaply).
fn conv_ref(kind: Kind, path: &Path, v: &Value) -> Option<SessionRef> {
    let id = conv_id(v)?;
    let title = match kind {
        Kind::ChatGpt => v.get("title").and_then(Value::as_str),
        Kind::Claude => v.get("name").and_then(Value::as_str),
    }
    .filter(|s| !s.is_empty())
    .map(str::to_string);
    let created = v
        .get("create_time")
        .or_else(|| v.get("created_at"))
        .and_then(ts_from_value);
    let updated = v
        .get("update_time")
        .or_else(|| v.get("updated_at"))
        .and_then(ts_from_value);
    let message_count = match kind {
        Kind::ChatGpt => chatgpt_active_chain(v)
            .iter()
            .filter(|n| convo_role(n.get("message")).is_some_and(|r| r == Role::User || r == Role::Assistant))
            .count(),
        Kind::Claude => v
            .get("chat_messages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|m| matches!(m.get("sender").and_then(Value::as_str), Some("human" | "assistant")))
                    .count()
            })
            .unwrap_or(0),
    };
    Some(SessionRef {
        id,
        harness: kind.harness(),
        path: path.to_path_buf(),
        cwd: None,
        title,
        created_at: created,
        updated_at: updated,
        message_count,
    })
}

fn conv_id(v: &Value) -> Option<String> {
    v.get("conversation_id")
        .or_else(|| v.get("id"))
        .or_else(|| v.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ── Parse one conversation ──────────────────────────────────────────────────

fn parse_ref(kind: Kind, r: &SessionRef) -> Result<Session> {
    let (_text, arr) = read_array(&r.path)?;
    let conv = arr
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw.get()).ok())
        .find(|v| conv_id(v).as_deref() == Some(r.id.as_str()))
        .with_context(|| format!("conversation {} not found in {}", r.id, r.path.display()))?;
    let mut s = Session {
        id: r.id.clone(),
        harness: kind.harness(),
        cwd: None,
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: conv
            .get("default_model_slug")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        git: None,
        messages: match kind {
            Kind::ChatGpt => chatgpt_messages(&conv),
            Kind::Claude => claude_messages(&conv),
        },
        source_path: Some(r.path.clone()),
        extra: Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    if s.title.is_none() {
        s.title = s.first_user_text().map(|t| truncate(&t, 80));
    }
    Ok(s)
}

/// The active branch (root → `current_node`) of a ChatGPT conversation's `mapping` DAG, root-first.
/// Falls back to mapping order when `current_node` is absent. Shared by message-building and the
/// metadata count so they agree (dead/regenerated branches are excluded).
fn chatgpt_active_chain(conv: &Value) -> Vec<&Value> {
    let Some(mapping) = conv.get("mapping").and_then(Value::as_object) else {
        return vec![];
    };
    let Some(cur) = conv.get("current_node").and_then(Value::as_str) else {
        return mapping.values().collect();
    };
    let mut chain: Vec<&Value> = Vec::new();
    let mut node_id = Some(cur.to_string());
    let mut guard = 0;
    while let Some(nid) = node_id {
        guard += 1;
        if guard > 100_000 {
            break;
        }
        let Some(node) = mapping.get(&nid) else { break };
        chain.push(node);
        node_id = node.get("parent").and_then(Value::as_str).map(str::to_string);
    }
    chain.reverse();
    chain
}

/// ChatGPT: walk the active branch of the `mapping` DAG into ordered turns. Stateful, because tool
/// calls and their results are separate turns linked only by tool *name* + order (ChatGPT carries no
/// explicit `tool_use_id`): we remember the last call per tool name and wire each result back to it.
fn chatgpt_messages(conv: &Value) -> Vec<Message> {
    let mut out = Vec::new();
    let mut last_call: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for node in chatgpt_active_chain(conv) {
        let Some(msg) = node.get("message") else { continue };
        let Some(role) = convo_role(Some(msg)) else { continue };
        let node_id = node.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let recipient = msg.get("recipient").and_then(Value::as_str).unwrap_or("all");
        let text = chatgpt_content_text(msg.get("content"));

        let mut content = Vec::new();
        if role == Role::Assistant && recipient != "all" {
            // A tool call. `name` = the tool (recipient); `input` = the raw content object (the call
            // payload — code/query string — kept verbatim, lossless).
            content.push(Block::ToolUse {
                id: node_id.clone(),
                name: recipient.to_string(),
                input: msg.get("content").cloned().unwrap_or(Value::Null),
                namespace: None,
            });
            last_call.insert(recipient.to_string(), node_id.clone());
        } else if role == Role::Tool {
            // A tool result. Link it to the most recent call of the same tool (author.name).
            let tool = msg
                .get("author")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            content.push(Block::ToolResult {
                tool_use_id: last_call.get(tool).cloned().unwrap_or_default(),
                content: text.clone().into(),
                is_error: false,
                tool_name: (!tool.is_empty()).then(|| tool.to_string()),
                status: None,
                details: None,
            });
            for ptr in chatgpt_image_pointers(msg.get("content")) {
                content.push(Block::Image {
                    media_type: None,
                    data_ref: Some(ptr),
                });
            }
        } else {
            if !text.trim().is_empty() {
                content.push(Block::Text { text: text.into() });
            }
            for ptr in chatgpt_image_pointers(msg.get("content")) {
                content.push(Block::Image {
                    media_type: None,
                    data_ref: Some(ptr),
                });
            }
        }
        if content.is_empty() {
            continue;
        }
        let mut m = Message::new(role);
        m.id = (!node_id.is_empty()).then(|| node_id.clone());
        m.parent_id = node.get("parent").and_then(Value::as_str).map(str::to_string);
        m.timestamp = msg.get("create_time").and_then(ts_from_value);
        m.content = content;
        out.push(m);
    }
    out
}

/// Role of a ChatGPT mapping node's `message` (None if no message / unknown author).
fn convo_role(msg: Option<&Value>) -> Option<Role> {
    let role = msg?.get("author")?.get("role")?.as_str()?;
    Some(role_from_str(role))
}

fn role_from_str(s: &str) -> Role {
    match s {
        "user" | "human" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::System,
    }
}

/// Flatten a ChatGPT `content` object's text (parts joined; `text` for code/exec; ignores image parts).
fn chatgpt_content_text(content: Option<&Value>) -> String {
    let Some(c) = content else { return String::new() };
    if let Some(parts) = c.get("parts").and_then(Value::as_array) {
        return parts.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join("\n");
    }
    // code / execution_output / tether_* carry `text`.
    c.get("text").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Image asset pointers in a ChatGPT `multimodal_text` content (`{content_type:image_asset_pointer,
/// asset_pointer:"file-service://…"}` parts).
fn chatgpt_image_pointers(content: Option<&Value>) -> Vec<String> {
    let Some(parts) = content.and_then(|c| c.get("parts")).and_then(Value::as_array) else {
        return vec![];
    };
    parts
        .iter()
        .filter_map(|p| p.as_object())
        .filter_map(|o| o.get("asset_pointer").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Claude.ai export: `chat_messages[]` (already in order) → IR turns.
fn claude_messages(conv: &Value) -> Vec<Message> {
    let Some(msgs) = conv.get("chat_messages").and_then(Value::as_array) else {
        return vec![];
    };
    msgs.iter()
        .filter_map(|m| {
            let role = role_from_str(m.get("sender").and_then(Value::as_str)?);
            let content = claude_content_blocks(m);
            if content.is_empty() {
                return None;
            }
            let mut msg = Message::new(role);
            msg.id = m.get("uuid").and_then(Value::as_str).map(str::to_string);
            msg.parent_id = m.get("parent_message_uuid").and_then(Value::as_str).map(str::to_string);
            msg.timestamp = m.get("created_at").and_then(ts_from_value);
            msg.content = content;
            Some(msg)
        })
        .collect()
}

/// A Claude.ai message's `content[]` blocks → IR blocks (text/thinking/tool_use/tool_result), with the
/// flat `text` field as a fallback when there are no structured blocks.
fn claude_content_blocks(m: &Value) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(blocks) = m.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        out.push(Block::Text { text: t.into() });
                    }
                }
                Some("thinking") => {
                    if let Some(t) = b.get("thinking").or_else(|| b.get("text")).and_then(Value::as_str) {
                        out.push(Block::Thinking {
                            text: t.into(),
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
                Some("tool_use") => out.push(Block::ToolUse {
                    id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                    name: b.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    input: b.get("input").cloned().unwrap_or(Value::Null),
                    namespace: None,
                }),
                Some("tool_result") => out.push(Block::ToolResult {
                    tool_use_id: b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    content: claude_tool_result_text(b).into(),
                    is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    tool_name: None,
                    status: None,
                    details: None,
                }),
                _ => {}
            }
        }
    }
    // Fallback to the flattened `text` field if no structured blocks produced content.
    if out.is_empty() {
        if let Some(t) = m.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            out.push(Block::Text { text: t.into() });
        }
    }
    out
}

fn claude_tool_result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => b.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
    }
}

#[cfg(test)]
mod tests;
