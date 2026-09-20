//! **OpenSession** — cv's own interchange format (`docs/OPENSESSION.md`), written by
//! `cv export --format json` and read back in here.
//!
//! OpenSession 0.3 *is* the IR: same keys, same snake_case, blocks tagged `type`, messages
//! carrying `kind`/`origin`. So the writer ([`document`]) is the IR's own serialization plus one
//! version marker, and the reader below is its inverse — which is the only way the spec's central
//! claim ("clustervision's in-memory IR is the reference implementation") can be checked rather
//! than asserted.
//!
//! **Discovery.** A document has no fixed home, so — exactly like the account-export adapters —
//! it is found through the sources the user registers with `cv config --add-export <path>`
//! (unioned with `$CV_EXPORTS`). Under each source (depth ≤ 2) a file counts as a document when
//! it is named `*.opensession.json`, or when it is a `*.json` whose head carries an
//! `open_session` / `openSession` key. **Opt-in**: with nothing registered, discovery is a no-op.
//!
//! **Tolerance** (spec principle 10 — "be tolerant on the way in"). The reader accepts 0.2
//! spellings beside 0.3 ones, the same way `web/components/util.js` does:
//!
//! | 0.2 | 0.3 |
//! |---|---|
//! | `openSession`, `createdAt`, `updatedAt`, `parentId`, `systemPrompt` | `open_session`, `created_at`, … |
//! | a block tagged `kind`, spelled `toolUse` / `toolResult` | a block tagged `type`: `tool_use`, `tool_result` |
//! | `toolUseId`, `isError`, `toolName`, `mediaType`, `dataRef` | `tool_use_id`, `is_error`, `tool_name`, `media_type`, `data_ref` |
//! | `inputTokens`, `cacheReadTokens`, … | `input_tokens`, `cache_read_tokens`, … |
//! | `messageKind` (the 0.2 alias, since 0.2 spent `kind` on blocks) | `kind` |
//!
//! Nothing out of vocabulary is fatal: an unknown block `type` is dropped and its name recorded in
//! the message's `extra["cv"]["unknown_blocks"]`; an unknown message `kind`/`origin` falls back to
//! the role-implied default with the original spelling kept in `extra["cv"]`; a message that
//! cannot be read at all counts toward `extra["cv"]["skipped_lines"]` like any other adapter's
//! corrupt record.
//!
//! Canonicalize-then-deserialize is deliberate: after the renames above, the document is handed to
//! the IR's **own** `Deserialize` impls, so the vocabulary of kinds, origins and block fields can
//! never drift from `ir.rs` into a second hand-written copy here.

use super::{ts_from_value, Adapter};
use crate::ir::*;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The OpenSession version cv writes — the one place it is spelled, so `docs/OPENSESSION.md`,
/// the export and the reader cannot drift apart.
///
/// It lives at the **document boundary** ([`document`]), not as a field on
/// [`Session`](crate::ir::Session): a `Session` is cv's in-memory IR and is serialized raw by
/// `cv show --json`, the MCP payloads and cvd, whose key sets are pinned by tests. A version
/// marker is a fact about a *document* someone wrote to disk or to a wire, not about a value in
/// memory — so it is added exactly where a document is produced, and stripped (ignored) where one
/// is read.
pub const VERSION: &str = "0.3";

/// The document's version key, and the 0.2 spelling still accepted on the way in.
const VERSION_KEY: &str = "open_session";
const VERSION_KEY_V02: &str = "openSession";

/// How much of a plain `*.json` file's head is read to decide whether it is an OpenSession
/// document. cv writes the version marker as the FIRST key, so this is generous; a hand-written
/// document that buries it past 64 KiB is out of scope (name it `*.opensession.json` instead).
const SNIFF_BYTES: usize = 64 * 1024;

// ── Writing: the document boundary ────────────────────────────────────────────

/// A session as an OpenSession document: the IR's own serialization, with `open_session` first.
/// Produced by [`document`]; see [`VERSION`] for why the marker lives here and not on `Session`.
#[derive(Serialize)]
pub struct Document<'a> {
    open_session: &'static str,
    #[serde(flatten)]
    session: &'a Session,
}

/// Wrap `session` as an OpenSession [`Document`] for serialization. The session should be
/// materialized (`Session::materialize`, which `Adapter::parse` already does) so its content
/// serializes as text rather than as lazy spans into a file the reader will not have.
pub fn document(session: &Session) -> Document<'_> {
    Document {
        open_session: VERSION,
        session,
    }
}

// ── The adapter ───────────────────────────────────────────────────────────────

pub struct OpenSession;

impl OpenSession {
    pub fn new() -> Self {
        OpenSession
    }
}

impl Default for OpenSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for OpenSession {
    fn harness(&self) -> Harness {
        Harness::OpenSession
    }

    fn storage_root(&self) -> Option<PathBuf> {
        super::export::export_dirs().into_iter().next()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out: Vec<SessionRef> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in document_files() {
            if let Some(r) = doc_ref(&path) {
                if seen.insert(r.id.clone()) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        read_document(&r.path)
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Every OpenSession document under the registered export sources.
fn document_files() -> Vec<PathBuf> {
    super::export::files_under(super::export::export_dirs(), is_document_file)
}

/// Is this file an OpenSession document? By name (`*.opensession.json`), or by a head sniff of a
/// plain `*.json` for the version key.
fn is_document_file(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.ends_with(".opensession.json") {
        return true;
    }
    if !name.ends_with(".json") {
        return false;
    }
    crate::lazy::read_head(p, SNIFF_BYTES)
        .is_some_and(|head| head.contains(VERSION_KEY) || head.contains(VERSION_KEY_V02))
}

/// A lightweight [`SessionRef`] for one document: metadata only, counting conversational turns
/// (user + assistant, the cross-harness contract) straight off the raw records.
fn doc_ref(path: &Path) -> Option<SessionRef> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let o = v.as_object()?;
    let message_count = o
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|m| matches!(role_word(m.get("role").and_then(Value::as_str)), "user" | "assistant"))
                .count()
        })
        .unwrap_or(0);
    Some(SessionRef {
        id: doc_id(o).unwrap_or_else(|| id_from_path(path)),
        // The REF is tagged `opensession` because that is the adapter that reads this file; the
        // parsed session keeps the harness the document names (`Session::harness`), which is the
        // fact the document carries and the round trip must preserve.
        harness: Harness::OpenSession,
        path: path.to_path_buf(),
        cwd: string_of(o, &["cwd"]).map(PathBuf::from),
        title: string_of(o, &["title"]),
        created_at: o
            .get("created_at")
            .or_else(|| o.get("createdAt"))
            .and_then(ts_from_value),
        updated_at: o
            .get("updated_at")
            .or_else(|| o.get("updatedAt"))
            .and_then(ts_from_value),
        message_count,
    })
}

fn doc_id(o: &Map<String, Value>) -> Option<String> {
    o.get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A document with no `id` of its own is identified by its file name, with the
/// `.opensession.json` / `.json` suffix removed.
fn id_from_path(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("opensession");
    name.strip_suffix(".opensession.json")
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(name)
        .to_string()
}

// ── Reading a document ────────────────────────────────────────────────────────

/// Read one OpenSession document into the IR. The inverse of [`document`].
pub fn read_document(path: &Path) -> Result<Session> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading OpenSession document {}", path.display()))?;
    let mut s = from_str(&text).with_context(|| format!("parsing OpenSession document {}", path.display()))?;
    if s.id.is_empty() {
        s.id = id_from_path(path);
    }
    // `source_path` is where cv read this session from — this document, not whatever store the
    // document's own `source_path` named on the machine that wrote it.
    s.source_path = Some(path.to_path_buf());
    Ok(s)
}

/// Read an OpenSession document from text.
pub fn from_str(text: &str) -> Result<Session> {
    let v: Value = serde_json::from_str(text).context("not JSON")?;
    let o = v.as_object().context("an OpenSession document is a JSON object")?;

    let mut messages = Vec::new();
    let mut skipped = 0u64;
    if let Some(arr) = o.get("messages").and_then(Value::as_array) {
        for m in arr {
            match message_from(m) {
                Some(m) => messages.push(m),
                None => skipped += 1,
            }
        }
    }

    let mut s = Session {
        id: doc_id(o).unwrap_or_default(),
        harness: string_of(o, &["harness"])
            .and_then(|h| harness_from(&h))
            .unwrap_or(Harness::OpenSession),
        cwd: string_of(o, &["cwd"]).map(PathBuf::from),
        title: string_of(o, &["title"]),
        created_at: o
            .get("created_at")
            .or_else(|| o.get("createdAt"))
            .and_then(ts_from_value),
        updated_at: o
            .get("updated_at")
            .or_else(|| o.get("updatedAt"))
            .and_then(ts_from_value),
        model: string_of(o, &["model"]),
        git: o
            .get("git")
            .filter(|g| g.is_object())
            .and_then(|g| serde_json::from_value::<GitInfo>(g.clone()).ok()),
        system_prompt: string_of(o, &["system_prompt", "systemPrompt"]),
        lineage: lineage_from(o.get("lineage")),
        messages,
        source_path: None,
        extra: object_of(o.get("extra")),
    };
    super::note_skipped_lines(&mut s, skipped);
    Ok(s)
}

/// The harness a document names. Accepts every spelling cv itself produces: the canonical name
/// and its aliases (`Harness::parse`), plus the enum's serde spelling — which is the lowercased
/// variant name (`claudeapp`), NOT the canonical `claude-app`, and is therefore what the IR's own
/// serialization writes into a document. Unknown ⇒ `None`, and the session is an `opensession`.
fn harness_from(name: &str) -> Option<Harness> {
    Harness::parse(name).or_else(|| {
        let want = name.to_ascii_lowercase();
        Harness::ALL
            .iter()
            .copied()
            .find(|h| format!("{h:?}").to_ascii_lowercase() == want)
    })
}

fn lineage_from(v: Option<&Value>) -> Lineage {
    let Some(o) = v.and_then(Value::as_object) else {
        return Lineage::default();
    };
    let mut o = o.clone();
    alias(
        &mut o,
        &[
            ("forkedFrom", "forked_from"),
            ("parentId", "parent"),
            ("spawnedByToolUse", "spawned_by_tool_use"),
            ("continuedIn", "continued_in"),
            ("agentPath", "agent_path"),
        ],
    );
    serde_json::from_value::<Lineage>(Value::Object(o)).unwrap_or_default()
}

/// One message: canonicalize the 0.2 spellings, validate the vocabulary, then let the IR's own
/// `Deserialize` do the rest. `None` when the record is unreadable as a message at all (counted
/// as a skipped line).
fn message_from(v: &Value) -> Option<Message> {
    let mut o = v.as_object()?.clone();
    alias(
        &mut o,
        &[
            ("parentId", "parent_id"),
            ("messageKind", "kind"),
            ("message_kind", "kind"),
        ],
    );

    // Role: the four normalized roles, with the harnesses' `human` spelling accepted. Anything
    // else (including a missing role) is a system turn rather than a dropped one.
    let role = role_word(o.get("role").and_then(Value::as_str));
    o.insert("role".into(), Value::String(role.into()));

    // `kind` / `origin`: an out-of-vocabulary word is REMOVED (so the IR's role-implied default
    // applies) and remembered, rather than failing the turn. Validation goes through the enums'
    // own `Deserialize`, so this can never disagree with `ir.rs`.
    let mut notes: Vec<(String, Value)> = Vec::new();
    if let Some(word) = o.get("kind").and_then(Value::as_str).map(snake) {
        if is_known::<MessageKind>(&word) {
            o.insert("kind".into(), Value::String(word));
        } else {
            o.remove("kind");
            notes.push(("unknown_kind".into(), Value::String(word)));
        }
    }
    if let Some(word) = o.get("origin").and_then(Value::as_str).map(snake) {
        if is_known::<Origin>(&word) {
            o.insert("origin".into(), Value::String(word));
        } else {
            o.remove("origin");
            notes.push(("unknown_origin".into(), Value::String(word)));
        }
    }

    // A non-string timestamp (epoch seconds/millis) becomes the RFC3339 the IR deserializes.
    if let Some(t) = o.get("timestamp") {
        match ts_from_value(t) {
            Some(dt) if !t.is_string() => {
                o.insert("timestamp".into(), Value::String(dt.to_rfc3339()));
            }
            None => {
                o.remove("timestamp");
            }
            _ => {}
        }
    }
    if let Some(Value::Object(u)) = o.get_mut("usage") {
        alias(u, USAGE_ALIASES);
    }
    if o.get("extra").is_some_and(|e| !e.is_object()) {
        o.remove("extra");
    }

    // Content is validated block by block, so one unreadable block costs its block and not the
    // whole turn.
    let content = o.remove("content");
    let mut m: Message = serde_json::from_value(Value::Object(o)).ok()?;
    let mut unknown_blocks = Vec::new();
    if let Some(arr) = content.as_ref().and_then(Value::as_array) {
        for b in arr {
            match block_from(b) {
                Ok(block) => m.content.push(block),
                Err(what) => unknown_blocks.push(Value::String(what)),
            }
        }
    }
    if !unknown_blocks.is_empty() {
        notes.push(("unknown_blocks".into(), Value::Array(unknown_blocks)));
    }
    if !notes.is_empty() {
        let bag = m.extra.entry(CV_NAMESPACE).or_insert_with(|| Value::Object(Map::new()));
        if !bag.is_object() {
            *bag = Value::Object(Map::new());
        }
        if let Some(bag) = bag.as_object_mut() {
            bag.extend(notes);
        }
    }
    Some(m)
}

/// One content block. `Err(type)` names a block that could not be read, for the caller's note.
fn block_from(v: &Value) -> std::result::Result<Block, String> {
    let o = v.as_object().ok_or_else(|| "<non-object>".to_string())?;
    // A block has a `type` in 0.3 and a `kind` in 0.2; `toolUse` snakes to `tool_use`.
    let tag = o
        .get("type")
        .or_else(|| o.get("kind"))
        .and_then(Value::as_str)
        .map(snake)
        .ok_or_else(|| "<untagged>".to_string())?;

    let mut o = o.clone();
    o.insert("type".into(), Value::String(tag.clone()));
    alias(
        &mut o,
        &[
            ("toolUseId", "tool_use_id"),
            ("isError", "is_error"),
            ("toolName", "tool_name"),
            ("mediaType", "media_type"),
            ("dataRef", "data_ref"),
        ],
    );

    // Required fields get a default rather than failing the block: a tool call with no recorded
    // id is still a tool call (spec principle 8 — lossy but honest).
    match tag.as_str() {
        "text" | "thinking" => default_text(&mut o, "text"),
        "tool_use" => {
            default_text(&mut o, "id");
            default_text(&mut o, "name");
            o.entry("input").or_insert(Value::Null);
        }
        "tool_result" => {
            default_text(&mut o, "tool_use_id");
            if let Some(c) = o.get("content") {
                if let Some(flat) = flatten_text(c) {
                    o.insert("content".into(), Value::String(flat));
                }
            }
            default_text(&mut o, "content");
        }
        // A `file` block's media type was `mediaType` in some 0.2 writers; the IR calls it `mime`.
        "file" => {
            alias(&mut o, &[("media_type", "mime")]);
        }
        "image" => {}
        _ => return Err(tag),
    }
    serde_json::from_value::<Block>(Value::Object(o)).map_err(|_| tag)
}

// ── Small shared rules ────────────────────────────────────────────────────────

const USAGE_ALIASES: &[(&str, &str)] = &[
    ("inputTokens", "input_tokens"),
    ("outputTokens", "output_tokens"),
    ("cacheReadTokens", "cache_read_tokens"),
    ("cacheCreationTokens", "cache_creation_tokens"),
    ("reasoningTokens", "reasoning_tokens"),
    ("costUsd", "cost_usd"),
];

/// Copy `from` to `to` for each pair, when the document spelled it the old way and not the new
/// one. Never removes the old key: an unknown field is ignored by the IR's `Deserialize` anyway,
/// and keeping it costs nothing.
fn alias(o: &mut Map<String, Value>, pairs: &[(&str, &str)]) {
    for (from, to) in pairs {
        if !o.contains_key(*to) {
            if let Some(v) = o.get(*from).cloned() {
                o.insert((*to).to_string(), v);
            }
        }
    }
}

/// The four normalized roles as the IR spells them; `human` is the spelling several stores use,
/// and anything else — including a missing role — is a system turn.
fn role_word(role: Option<&str>) -> &'static str {
    match role.map(|r| r.to_ascii_lowercase()).as_deref() {
        Some("user") | Some("human") => "user",
        Some("assistant") => "assistant",
        Some("tool") => "tool",
        _ => "system",
    }
}

/// Does this word name a variant of `T`? Asked of the IR's own enums so the accepted vocabulary
/// is always exactly what `ir.rs` declares.
fn is_known<T: serde::de::DeserializeOwned>(word: &str) -> bool {
    serde_json::from_value::<T>(Value::String(word.to_string())).is_ok()
}

/// `toolUse` → `tool_use`, `InjectedContext` → `injected_context`, `TOOL_RESULT` →
/// `tool_result`. The same normalization the browser reader does (`web/components/util.js`).
fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_wordy = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if prev_wordy {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_wordy = false;
        } else if c == ' ' || c == '-' || c == '\t' {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_wordy = false;
        } else {
            out.push(c);
            prev_wordy = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// The first of `keys` present as a non-empty string.
fn string_of(o: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| o.get(*k))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn object_of(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Ensure `key` holds something the IR can read as text: absent or non-textual becomes `""`.
/// (A span object is textual — `Text` deserializes one — so it passes through untouched.)
fn default_text(o: &mut Map<String, Value>, key: &str) {
    let ok = o.get(key).is_some_and(|v| v.is_string() || v.is_object());
    if !ok {
        o.insert(key.to_string(), Value::String(String::new()));
    }
}

/// Flatten a non-string tool-result `content` (the Anthropic-style array of parts, or a
/// `{"text": …}` wrapper) into text. `None` leaves the value alone — notably a lazy span object,
/// which has an `offset` rather than a `text`.
fn flatten_text(v: &Value) -> Option<String> {
    match v {
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string).or_else(|| flatten_text(p)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Object(o) if !o.contains_key("offset") => o.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
