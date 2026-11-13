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
//! - `usage` (assistant, cn CLI 1.5): OpenAI usage names plus `model` and `cost_cents` → [`Usage`].
//!
//! ### `toolCallStates[]` (assistant items, cn CLI 1.5)
//! The cn CLI writes no `role: "tool"` items; each assistant item carries its calls' outcomes as
//! `{toolCallId, toolCall, status, parsedArgs, output: [{content, name, description}]}`, emitted
//! right after the assistant turn as a Tool message (`status: "errored"` → `is_error`).
//!
//! ### `contextItems[]` (attached files / snippets / docs)
//! `{name, description, content, uri?: {type, value}}`. Best-effort → `Block::File` (path/source from
//! `uri.value` or `description`), appended to the owning message so search can see them.
//!
//! ## Tolerance
//! Missing index → glob `*.json` (minus `sessions.json`). Missing fields → skip. `content` as
//! string or array both work. `dateCreated` ms-epoch or ISO both work. Never panics.

use super::store::document::{Fields, InvalidStruct};
use crate::json::{InvalidJson, JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
mod record;
use super::wire::openai;
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::Result;
use chrono::{DateTime, Utc};

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Continue {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Continue = Continue { sessions: None };

impl Continue {
    pub fn detect() -> Self {
        let root = crate::env::var_os("CONTINUE_GLOBAL_DIR")
            .map(PathBuf::from)
            .or_else(|| crate::env::home_dir().map(|h| h.join(".continue")));
        let sessions = root.map(|r| r.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }

    /// Continue's store under an explicit global dir (what `CONTINUE_GLOBAL_DIR` names).
    pub fn with_root(root: &Path) -> Self {
        let sessions = Some(root.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }
}

impl Adapter for Continue {
    fn harness(&self) -> Harness {
        Harness::Continue
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.sessions.clone().into_iter().collect()
    }

    /// A session's listing title, cwd and creation time come from its `sessions.json` entry,
    /// which Continue rewrites on every save of any session: it is certified by its own entry
    /// ([`index_rows`]).
    fn dependencies(
        &self,
        r: &SessionRef,
        _probe: &mut crate::source::Probe,
        deps: &mut crate::source::Dependencies,
    ) -> Option<()> {
        deps.files.extend(self.rows_store(r));
        Some(())
    }

    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        Some(r.path.parent()?.join("sessions.json"))
    }

    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        Some(index_rows(store))
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(sessions) = &self.sessions else {
            return Ok(Default::default());
        };

        // An unreadable or undecodable `sessions.json` is reported; its files are still listed by
        // the walk below, without the index's facts. An unreadable dir or session file is skipped
        // and reported; a session file that does not decode is listed (its read is an empty,
        // counted session).
        let mut out = crate::Discovered::default();
        let index_path = sessions.join("sessions.json");
        let index = out.keep(&index_path, read_index(&index_path)).unwrap_or_default();

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Prefer the index, but only keep entries whose `<id>.json` actually exists.
        for meta in &index {
            if crate::publish::validate_output_id(&meta.session_id).is_err() {
                continue;
            }
            let path = sessions.join(format!("{}.json", meta.session_id));
            if !path.exists() {
                continue;
            }
            if !seen.insert(meta.session_id.clone()) {
                continue;
            }
            // The index has the listing facts; counting would read the whole history (C1).
            out.refs.push(ref_from(meta, &path));
        }

        // Fold in any `<id>.json` on disk that the index didn't mention (tolerant of a missing or
        // stale index).
        if let Some(rd) = out.keep(sessions, fs::read_dir(sessions).map_err(Into::into)) {
            for e in rd {
                let Some(e) = out.keep(sessions, e.map_err(Into::into)) else {
                    continue;
                };
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
                // An unindexed session's listing facts are its own file's: rescanned only when it
                // moves. Uncounted, as the scan leaves it (no catalog count is filled in here).
                let scanned = crate::discover_cache::cached_scan_checked(&p, || {
                    let scan = scan_session(&p)?;
                    let (title, cwd) = scan.map_or((None, None), |s| (s.title, s.cwd));
                    let meta = IndexMeta {
                        session_id: id,
                        title,
                        cwd,
                        created_at: None,
                    };
                    Ok(Some(ref_from(&meta, &p)))
                })
                .and_then(|r| anyhow::Context::context(r, "a session scan lists its session"));
                let Some(mut r) = out.keep(&p, scanned) else {
                    continue;
                };
                r.message_count = None;
                out.refs.push(r);
            }
        }

        Ok(out)
    }

    /// Project one accepted history item at a time, retaining body source ranges in lazy mode.
    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Continue, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let mut s = Session {
                cwd: r.cwd.clone(),
                title: r.title.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at.or_else(|| mtime(&r.path)),
                source_path: Some(r.path.clone()),
                ..Session::new(r.id.clone(), Harness::Continue)
            };
            sink.dependency(&r.path);
            if let Some(error) = sink.take_error() {
                return Err(error);
            }
            let source = JsonSource::capture(&r.path)?;
            let result = source.read(ReadInput::Document, Validation::Syntax, |root| {
                let Some(doc) = Fields::parse_struct(root, &["title", "workspaceDirectory", "history"])? else {
                    return Err(InvalidStruct("expected Continue session struct").into());
                };
                let title = record::title(doc.get("title"), true)?;
                let workspace = optional_string(doc.get("workspaceDirectory"))?;
                let history = match doc.get("history").map(JsonView::accepted_array).transpose()? {
                    Some(None) => return Err(InvalidStruct("expected Continue history array").into()),
                    array => array.flatten(),
                };
                if s.title.is_none() {
                    s.title = title;
                }
                if s.cwd.is_none() {
                    s.cwd = workspace.filter(|p| !p.is_empty()).map(|p| workspace_to_path(&p));
                }
                sink.meta(&s);
                let mut answered = std::collections::HashSet::<String>::new();
                for item in history.into_iter().flatten() {
                    let Some(item) = item? else { continue };
                    let reply_id = record::reply_id(item)?;
                    if reply_id.is_some_and(|id| answered.contains(&id)) {
                        continue;
                    }
                    let (message, results) = record::messages(item, opts.spans, &mut s.model)?;
                    if let Some(message) = message {
                        if s.system_prompt.is_none() && message.kind == MessageKind::SystemPrompt {
                            let mut text_message = message.clone();
                            text_message.try_materialize_text(&s.resolver())?;
                            let text = text_message.text();
                            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                                s.system_prompt = Some(text);
                            }
                        }
                        // A span demand checks its own source; the stream rechecks after a slow callback and
                        // at the end (`stream` module: who owns the source guarantee).
                        let started = std::time::Instant::now();
                        let flow = sink.try_message(message)?;
                        source.check_after(started)?;
                        if flow == Flow::Stop {
                            break;
                        }
                    }
                    if let Some(results) = results {
                        for block in &results.content {
                            if let Block::ToolResult { tool_use_id, .. } = block {
                                answered.insert(tool_use_id.clone());
                            }
                        }
                        let started = std::time::Instant::now();
                        let flow = sink.try_message(results)?;
                        source.check_after(started)?;
                        if flow == Flow::Stop {
                            break;
                        }
                    }
                }
                source.check()?;
                Ok(())
            });
            if let Err(error) = result {
                if !is_undecodable(&error) {
                    return Err(error);
                }
                sink.meta(&s);
                // The document is the record: one that does not decode is counted.
                super::Unparsed(1).note(&mut s);
            }
            Ok(s)
        })
    }
}

fn optional_string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    let Some(view) = view else { return Ok(None) };
    if view.is_null()? {
        return Ok(None);
    }
    if !view.is_string()? || !view.accepts_value()? {
        return Err(InvalidStruct("expected optional string").into());
    }
    Ok(view.resolve()?.as_str().map(str::to_owned))
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
    opts.validate_ids()?;
    super::wire::write::check_source_fields(session)?;
    use anyhow::Context;

    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    anyhow::ensure!(
        new_id != "sessions",
        "Continue session id 'sessions' is reserved for the native index"
    );
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    // Validate an existing index before publishing the transcript. Treating a corrupt or
    // unreadable index as empty would erase metadata for every other native conversation.
    let index_path = out_dir.join("sessions.json");
    let mut entries: Vec<Value> = match fs::read(&index_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("reading Continue index {}", index_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", index_path.display())),
    };

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ── transcript: <sessionId>.json ──
    let resolver = crate::lazy::Resolver::new(session.source_path.clone());
    let title = super::wire::write::fallback_title(session, &resolver)?;
    let file_path = out_dir.join(format!("{new_id}.json"));
    let mut set = crate::publish::Set::new();
    let transcript = emit::transcript(session, &new_id, title.as_deref(), cwd_str.as_deref(), opts.thinking)?;
    super::store::stage_into(&mut set, transcript, &file_path, opts.claim())?;

    // ── index: sessions.json (merge-or-create) ──
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
    let mut stage = super::store::bytes::ByteStage::new();
    serde_json::to_writer_pretty(&mut stage, &entries)?;
    // Continue's own index: cv's update replaces it whole. Transcript and index publish as one
    // set, the index last: a crash between leaves an unlisted transcript.
    super::store::stage_into(&mut set, stage, &index_path, crate::publish::Claim::Replace)?;
    set.publish()?;

    Ok(super::EmitResult {
        path: file_path,
        new_id,
        resume_hint: None,
    })
}

mod emit;

/// Map a `Block::File` to a Continue `contextItem` (`{name, description, uri:{type, value}}`) that
/// the parser's `record::context_item` round-trips back into a `File` block.
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

// ---------------------------------------------------------------------------
// Index (sessions.json)
// ---------------------------------------------------------------------------

struct IndexMeta {
    session_id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
}

/// A document that read but did not decode (as opposed to one that could not be read).
fn is_undecodable(error: &anyhow::Error) -> bool {
    error.is::<InvalidJson>() || error.is::<InvalidStruct>()
}

/// Read `sessions.json` (array of `{sessionId, title, dateCreated, workspaceDirectory}`); a
/// non-array document is an empty index, a read or decode failure an error.
fn read_index(path: &Path) -> Result<Vec<IndexMeta>> {
    JsonSource::capture(path)?.read(ReadInput::Document, Validation::Value, |root| {
        let mut out = Vec::new();
        if let Some(entries) = root.array()? {
            for entry in entries {
                let Some(fields) = Fields::parse(entry?, &["sessionId", "title", "dateCreated", "workspaceDirectory"])?
                else {
                    continue;
                };
                let Some(session_id) = fields.string("sessionId")?.filter(|s| !s.is_empty()) else {
                    continue;
                };
                out.push(IndexMeta {
                    session_id,
                    title: record::title(fields.get("title"), false)?,
                    cwd: fields
                        .string("workspaceDirectory")?
                        .filter(|p| !p.is_empty())
                        .map(|p| workspace_to_path(&p)),
                    created_at: fields.value("dateCreated")?.as_ref().and_then(parse_date),
                });
            }
        }
        Ok(out)
    })
}

/// Every session's certificate in the index at `store`: what discovery takes from its first entry
/// there (`title`, `workspaceDirectory`, `dateCreated`), or that it has none (an unreadable or
/// undecodable index is no entry, as discovery reads it), for every `<id>.json` beside it.
fn index_rows(store: &Path) -> std::collections::HashMap<String, crate::source::Rows> {
    use crate::source::Rows;
    let index = read_index(store).unwrap_or_default();
    let unindexed = Rows {
        count: 0,
        high: 0,
        digest: Rows::SEED,
    };
    let mut rows: std::collections::HashMap<String, Rows> = std::collections::HashMap::new();
    if let Some(dir) = store.parent().and_then(|dir| fs::read_dir(dir).ok()) {
        for name in dir.flatten().filter_map(|e| e.file_name().into_string().ok()) {
            if name != "sessions.json" && name.ends_with(".json") {
                rows.insert(name.trim_end_matches(".json").to_string(), unindexed);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for meta in &index {
        if !seen.insert(meta.session_id.as_str()) {
            continue;
        }
        let entry = serde_json::json!([meta.title, meta.cwd, meta.created_at]);
        rows.insert(
            meta.session_id.clone(),
            Rows {
                count: 1,
                high: 0,
                digest: Rows::fold(Rows::SEED, &serde_json::to_vec(&entry).unwrap_or_default()),
            },
        );
    }
    rows
}

/// Build a `SessionRef` from index metadata + the on-disk transcript path.
fn ref_from(meta: &IndexMeta, path: &Path) -> SessionRef {
    let updated = mtime(path);
    SessionRef {
        cwd: meta.cwd.clone(),
        title: meta.title.clone(),
        created_at: meta.created_at.or(updated),
        updated_at: updated,
        // Uncounted (`SessionRef::new`): counting would read the whole history (C1).
        ..SessionRef::new(meta.session_id.clone(), Harness::Continue, path.to_path_buf())
    }
}

/// Discovery keeps the listing metadata of an unindexed session, never its history or a count
/// (counting means reading every history item: `None` until a full read certifies it). The
/// document is still validated whole, as Value. Duplicate fields retain their last value,
/// including null/wrong-type replacements.
#[derive(Default)]
struct Scan {
    title: Option<String>,
    cwd: Option<PathBuf>,
}

/// `None` for a document that does not decode; an error for one that cannot be read.
fn scan_session(path: &Path) -> Result<Option<Scan>> {
    let scanned = JsonSource::capture(path)?.read(ReadInput::Document, Validation::Value, |root| {
        let mut scan = Scan::default();
        if let Some(fields) = Fields::parse(root, &["title", "workspaceDirectory"])? {
            scan.title = record::title(fields.get("title"), false)?;
            scan.cwd = fields
                .string("workspaceDirectory")?
                .filter(|p| !p.is_empty())
                .map(|p| workspace_to_path(&p));
        }
        Ok(scan)
    });
    match scanned {
        Ok(scan) => Ok(Some(scan)),
        Err(error) if is_undecodable(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// history[] -> IR
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Parse an in-memory Continue session JSON document (used by [`crate::ingest`], wasm-safe).
pub(crate) fn parse_str(id: &str, text: &str) -> Result<Session> {
    use crate::harness::store::document::range::RangeSource;
    let source = RangeSource::text(None, text, 0, false);
    let Some(val) = source.accept(source.range(), Validation::Syntax)? else {
        anyhow::bail!("empty Continue session document");
    };
    let root = JsonView::of(&source, val);
    let Some(doc) = Fields::parse_struct(root, &["sessionId", "title", "workspaceDirectory", "history"])? else {
        return Err(InvalidStruct("expected Continue session struct").into());
    };
    let resolved_id = doc
        .string("sessionId")?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.to_owned());
    let title = record::title(doc.get("title"), true)?;
    let workspace = optional_string(doc.get("workspaceDirectory"))?;
    let history = match doc.get("history").map(JsonView::accepted_array).transpose()? {
        Some(None) => return Err(InvalidStruct("expected Continue history array").into()),
        array => array.flatten(),
    };
    let mut s = Session {
        cwd: workspace.filter(|p| !p.is_empty()).map(|p| workspace_to_path(&p)),
        title,
        ..Session::new(resolved_id, Harness::Continue)
    };
    let mut collect = crate::stream::CollectSink::default();
    {
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Continue, &mut collect);
        spawns.meta(&s);
        let mut answered = std::collections::HashSet::<String>::new();
        for item in history.into_iter().flatten() {
            let Some(item) = item? else { continue };
            let reply_id = record::reply_id(item)?;
            if reply_id.is_some_and(|id| answered.contains(&id)) {
                continue;
            }
            let (message, results) = record::messages(item, false, &mut s.model)?;
            if let Some(message) = message {
                if s.system_prompt.is_none() && message.kind == MessageKind::SystemPrompt {
                    let mut text_message = message.clone();
                    text_message.try_materialize_text(&s.resolver())?;
                    if let Some(t) = text_message.text().filter(|t| !t.trim().is_empty()) {
                        s.system_prompt = Some(t);
                    }
                }
                if spawns.try_message(message)? == Flow::Stop {
                    break;
                }
            }
            if let Some(results) = results {
                for block in &results.content {
                    if let Block::ToolResult { tool_use_id, .. } = block {
                        answered.insert(tool_use_id.clone());
                    }
                }
                if spawns.try_message(results)? == Flow::Stop {
                    break;
                }
            }
        }
    }
    s.messages = collect.messages;
    Ok(s)
}

/// Continue's `workspaceDirectory` may be a `file://` URI (including `file://localhost/...`,
/// Windows `file:///C:/...`, and percent-encoded UTF-8 like `%20`) or a plain path. Normalize to a path.
fn workspace_to_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("file://") {
        let path_part = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode_utf8(path_part);
        PathBuf::from(normalize_windows_drive_slash(&decoded))
    } else {
        PathBuf::from(normalize_windows_drive_slash(s))
    }
}

fn percent_decode_utf8(s: &str) -> String {
    if !s.as_bytes().contains(&b'%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
}

fn normalize_windows_drive_slash(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 3
        && b[0] == b'/'
        && b[1].is_ascii_alphabetic()
        && b[2] == b':'
        && (b.len() == 3 || b[3] == b'/' || b[3] == b'\\')
    {
        &s[1..]
    } else {
        s
    }
}

/// Parse a `dateCreated` value: a number or numeric-string of **milliseconds** since epoch, or an
/// ISO-8601 string.
fn parse_date(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => n.as_i64().and_then(ts_from_epoch),
        Value::String(s) => {
            let s = s.trim();
            if let Ok(ms) = s.parse::<i64>() {
                return ts_from_epoch(ms);
            }
            super::parse_ts(s)
        }
        _ => None,
    }
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

    #[test]
    fn export_preserves_existing_files_when_index_is_unusable() {
        let world = crate::test_world("continue-index-export-error");
        let dir = world.base.join("sessions");
        fs::create_dir(&dir).unwrap();
        let transcript = dir.join("fixed.json");
        fs::write(&transcript, "original transcript").unwrap();
        let index = dir.join("sessions.json");
        let session = Session::new("source", Harness::Continue);
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        for unusable in ["{broken", "{}", "null"] {
            fs::write(&index, unusable).unwrap();
            assert!(emit(&session, &dir, &opts).is_err());
            assert_eq!(fs::read_to_string(&index).unwrap(), unusable);
            assert_eq!(fs::read_to_string(&transcript).unwrap(), "original transcript");
        }
        let reserved = crate::emit::EmitOptions {
            new_id: Some("sessions".into()),
            ..Default::default()
        };
        fs::write(&index, "[]").unwrap();
        assert!(emit(&session, &dir, &reserved).is_err());
        assert_eq!(fs::read_to_string(&index).unwrap(), "[]");
        fs::write(&index, r#"[{"sessionId":"other","custom":{"retain":true}}]"#).unwrap();
        // `fixed.json` is taken: refused without force, replaced (and indexed) with it.
        assert!(emit(&session, &dir, &opts)
            .unwrap_err()
            .downcast_ref::<crate::publish::Exists>()
            .is_some());
        assert_eq!(fs::read_to_string(&transcript).unwrap(), "original transcript");
        let forced = crate::emit::EmitOptions { force: true, ..opts };
        emit(&session, &dir, &forced).unwrap();
        let merged: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        assert_eq!(merged[0]["custom"]["retain"], true);
        assert_eq!(merged[1]["sessionId"], "fixed");
    }
    use crate::harness::Parse;

    /// The history-item codec over an owned value, no spans.
    fn history_item_to_message(item: &Value, model: &mut Option<String>) -> Option<Message> {
        record::messages(item.into(), false, model).unwrap().0
    }

    /// Source-backed items with spans resolve to exactly what the owned value gives.
    #[test]
    fn history_items_resolve_alike_from_source_and_value_and_header_acceptance() {
        #[derive(serde::Deserialize)]
        struct OldDoc<'a> {
            #[serde(default)]
            title: Option<String>,
            #[serde(default, rename = "workspaceDirectory")]
            workspace: Option<String>,
            #[serde(default, borrow)]
            history: Vec<&'a serde_json::value::RawValue>,
        }
        let world = cv_testkit::World::new("continue-view-oracle");
        let path = world.base.join("s.json");
        let body = "雪\n\"\\".repeat(8192);
        let items = json!([
            {"message":{"role":"user","content":[{"text":""},{"type":"ignored","text":body},{"type":"imageUrl","imageUrl":{"url":"file:///image"}}]},
                "contextItems":[{"name":"a","description":"file","content":body,"uri":{"type":"file","value":"a.rs"}}]},
            {"message":{"role":"assistant","content":body,
                "toolCalls":[{"id":"a","function":{"name":"read","arguments":{"payload":body}}},{"id":"b","function":{"name":"edit","arguments":serde_json::to_string(&json!({"payload":body})).unwrap()}}],
                "usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40,"ignored":body},"model":"m","cost_cents":2,"ignored":body}},
                "promptLogs":[{"modelTitle":null,"modelProvider":"suppressed","ignored":body}],
                "toolCallStates":[{"toolCallId":"a","status":"errored","toolCall":{"function":{"name":"read"}},"output":[{"content":body,"ignored":body}]}]},
            {"message":{"role":"tool","toolCallId":"a","content":[{"text":""},{"text":body}]}},
            {"message":null,"contextItems":[{"name":"still a file","content":body}]},
            {"message":{"role":"system","content":body}}
        ]);
        fs::write(&path, serde_json::to_vec(&items).unwrap()).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        source
            .read(ReadInput::Document, Validation::Value, |root| {
                let mut expected_model = None;
                let mut actual_model = None;
                for (view, owned) in root.array()?.unwrap().zip(items.as_array().unwrap()) {
                    let expected = record::messages(owned.into(), false, &mut expected_model)?;
                    let mut actual = record::messages(view?, true, &mut actual_model)?;
                    if let Some(message) = &actual.0 {
                        if owned.pointer("/message/toolCalls").is_some() {
                            assert!(record::has_sources(message));
                        }
                    }
                    let resolver = crate::lazy::Resolver::new(Some(path.clone()));
                    for message in [&mut actual.0, &mut actual.1].into_iter().flatten() {
                        message.materialize(&resolver)?;
                    }
                    assert_eq!(
                        serde_json::to_string(&actual).unwrap(),
                        serde_json::to_string(&expected).unwrap()
                    );
                    assert_eq!(actual_model, expected_model);
                }
                Ok(())
            })
            .unwrap();
        for text in [
            "{}",
            "[]",
            "[null,null,[]]",
            "[\"title\",\"/cwd\",[]]",
            "[null,null,[],0]",
            "{\"title\":null,\"history\":[]}",
            "{\"title\":1,\"history\":[]}",
            "{\"title\":\"a\",\"title\":\"b\",\"history\":[]}",
            "{\"history\":null}",
            "{\"ignored\":1e400,\"history\":[{\"message\":{\"content\":\"yes\"}}]}",
            "{\"title\":\"\\ud800\",\"history\":[]}",
            "{\"ignored\":\"\\ud800\",\"history\":[]}",
        ] {
            fs::write(&path, text).unwrap();
            let reference = SessionRef::new("s", Harness::Continue, path.clone());
            let mut sink = crate::stream::CollectSink::default();
            let actual = Continue { sessions: None }
                .stream(&reference, &Demand::LEAN.with_spans(), &mut sink)
                .unwrap();
            match serde_json::from_str::<OldDoc<'_>>(text) {
                Ok(old) => {
                    let mut model = None;
                    let expected: Vec<_> = old
                        .history
                        .iter()
                        .filter_map(|v| serde_json::from_str::<Value>(v.get()).ok())
                        .filter_map(|v| history_item_to_message(&v, &mut model))
                        .collect();
                    assert_eq!(sink.messages.len(), expected.len(), "{text}");
                    assert_eq!(
                        actual.title,
                        old.title
                            .filter(|t| !t.trim().is_empty())
                            .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE)),
                        "{text}"
                    );
                    assert_eq!(
                        actual.cwd,
                        old.workspace.filter(|w| !w.is_empty()).map(|w| workspace_to_path(&w)),
                        "{text}"
                    );
                }
                Err(_) => {
                    assert!(sink.messages.is_empty(), "{text}");
                    assert!(actual.title.is_none(), "{text}");
                }
            }
        }
    }

    #[test]
    fn encoded_arguments_survive_minimum_message_deferred_staging() {
        let world = crate::test_world("continue-encoded-minimum-staging");
        let path = world.base.join("session.json");
        let argument = serde_json::to_string(&json!({"payload":"x".repeat(8192)})).unwrap();
        let history = (0..3).map(|i| json!({"message":{"role":"assistant","usage":{"model":(i == 2).then_some("present")},"toolCalls":[{"id":i.to_string(),"function":{"name":"read","arguments":argument}}]}})).collect::<Vec<_>>();
        fs::write(&path, serde_json::to_vec(&json!({"history":history})).unwrap()).unwrap();
        let reference = SessionRef::new("fixture", Harness::Continue, &path);
        let adapter = Continue { sessions: None };
        let full = adapter.parse(&reference).unwrap();
        let mut expected = Vec::new();
        crate::dataset::write_record(&full, &mut expected, crate::dataset::Format::Chatml, None).unwrap();
        expected.push(b'\n');
        for minimum in [1, 2, 3] {
            let staged = crate::dataset::stage_record(
                &adapter,
                &reference,
                crate::dataset::Format::Chatml,
                None,
                minimum,
                None,
                &Default::default(),
            )
            .unwrap()
            .unwrap();
            let mut actual = Vec::new();
            staged.publish(&mut actual).unwrap();
            assert!(actual == expected);
        }
        assert!(crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            4,
            None,
            &Default::default()
        )
        .unwrap()
        .is_none());
        for (query, matched) in [("model:present", true), ("model:absent", false)] {
            let query = crate::SessionQuery::parse(query).unwrap();
            let staged = crate::dataset::stage_record(
                &adapter,
                &reference,
                crate::dataset::Format::Chatml,
                None,
                0,
                Some(&query),
                &Default::default(),
            )
            .unwrap();
            assert_eq!(staged.is_some(), matched);
            if let Some(staged) = staged {
                let mut actual = Vec::new();
                staged.publish(&mut actual).unwrap();
                assert!(actual == expected);
            }
        }
    }

    #[test]
    fn published_arguments_reject_source_changes_after_a_stopping_callback() {
        let world = cv_testkit::World::new("continue-source-change");
        let path = world.base.join("s.json");
        fs::write(&path, serde_json::to_vec(&json!({"history":[{"message":{"role":"assistant","toolCalls":[{"id":"a","function":{"name":"read","arguments":{"payload":"x".repeat(8192)}}}]}}]})).unwrap()).unwrap();
        let reference = SessionRef::new("s", Harness::Continue, path.clone());
        let mut published = None;
        let mut mutate = |message: Message| {
            if let Some(input) = message.content.iter().find_map(|block| match block {
                Block::ToolUse {
                    input: crate::Json::Source(_),
                    ..
                } => {
                    if let Block::ToolUse { input, .. } = block {
                        Some(input)
                    } else {
                        None
                    }
                }
                _ => None,
            }) {
                published = Some(input.clone());
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
                Flow::Stop
            } else {
                Flow::Continue
            }
        };
        assert!(Continue { sessions: None }
            .stream(&reference, &Demand::LEAN.with_spans(), &mut mutate)
            .is_err());
        assert!(published.unwrap().resolve().is_err());
    }

    #[test]
    fn cn_tool_call_states_become_tool_results_and_usage_is_read() {
        let dir = cv_testkit::temp_dir("continue-cn");
        let path = dir.join("s.json");
        fs::write(&path, r#"{"sessionId":"s","history":[
            {"message":{"role":"user","content":"read a"},"contextItems":[]},
            {"message":{"role":"assistant","content":"","toolCalls":[{"id":"c1","type":"function","function":{"name":"Read","arguments":"{}"}}],
              "usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40},"model":"glm","cost_cents":2}},
             "contextItems":[],
             "toolCallStates":[{"toolCallId":"c1","toolCall":{"function":{"name":"Read"}},"status":"errored",
               "output":[{"content":"no such file","name":"Tool Result"}]}]},
            {"message":{"role":"tool","content":"no such file","toolCallId":"c1"},"contextItems":[]}
        ]}"#).unwrap();
        let r = SessionRef::new("s", Harness::Continue, path);
        let s = Continue { sessions: None }.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MessageKind::Prompt, MessageKind::Reply, MessageKind::ToolResult]
        );
        let u = s.messages[1].usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (Some(60), Some(40), Some(7))
        );
        assert_eq!(s.model.as_deref(), Some("glm"));
        match &s.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                is_error,
                tool_name,
                ..
            } => {
                assert_eq!(
                    (tool_use_id.as_str(), *is_error, tool_name.as_deref()),
                    ("c1", true, Some("Read"))
                )
            }
            b => panic!("{b:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

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
                assert_eq!(input.resolve().unwrap()["path"], "a.rs");
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
                assert_eq!(content.inline_str(), Some("file contents here"));
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
    fn parses_fixture_session() {
        let dir = fixtures();
        let path = dir.join("11111111-1111-1111-1111-111111111111.json");
        assert!(path.exists(), "missing committed fixture {}", path.display());
        let r = SessionRef::new("11111111-1111-1111-1111-111111111111", Harness::Continue, path.clone());
        let s = Continue { sessions: None }.parse(&r).unwrap();
        crate::harness::assert_no_flat_keys(&s);
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
        let dir = cv_testkit::temp_dir("continue-emit");

        // Build a small session: user, assistant w/ a tool call, a tool result.
        let mut session = Session {
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Roundtrip".into()),
            model: Some("gpt-4o".into()),
            ..Session::new("src", Harness::Continue)
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
            input: json!({ "path": "a.rs" }).into(),
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
        let r = SessionRef::new(res.new_id.clone(), Harness::Continue, res.path.clone());
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
                if tool_use_id == "call_1" && content.matches_str("fn main() {}") == Some(true)
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
        let index = dir.join("sessions.json");
        assert!(index.exists(), "missing committed fixture {}", index.display());
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
        assert_eq!(r.message_count, None, "uncounted until a full read");
        assert!(r.created_at.is_some());
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first message ends the parse with exactly one message seen, never materializing the rest.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        let dir = cv_testkit::temp_dir("continue-stream");
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

        let r = SessionRef::new("s", Harness::Continue, path.clone());

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
            .stream(&r, &Demand::RICH, &mut sink)
            .unwrap();

        // Only the first message was ever materialized; the returned Session is metadata-only.
        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/w")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_scan_matches_whole_value_metadata() {
        let _world = crate::test_world("continue-scan");
        let dir = cv_testkit::temp_dir("continue-scan");
        let path = dir.join("s.json");
        let mut cases = vec![
            r#"{"title":"escaped\n\uD834\uDD1E","workspaceDirectory":"file:///p","history":[null,{},42,{"message":{"role":"tool"}}]}"#.to_owned(),
            r#"{"title":"old","title":null,"workspaceDirectory":"/old","workspaceDirectory":42,"history":[1,2],"history":false}"#.to_owned(),
            r#"{"title":42,"title":"new","history":null,"history":[1,2,3]}"#.to_owned(),
            r#"{"history":{"nested":[1,2]},"title":"   "}"#.to_owned(),
            "[1,2,3]".to_owned(), "null".to_owned(), "42".to_owned(),
            r#"{"title":"early","history":[1,{"content":"bad\uD800"}]}"#.to_owned(),
            r#"{"history":[1],"ignored":"bad\uDC00"}"#.to_owned(),
            r#"{"history":[1e999]}"#.to_owned(),
            r#"{"history":[1],"history":[2,}"#.to_owned(),
            r#"{"history":[1]} trailing"#.to_owned(),
        ];
        for depth in 123..131 {
            cases.push(format!(
                r#"{{"history":[{}0{}]}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        // Large discarded bodies and nested context are validated, not retained in a history tree.
        cases.push(
            serde_json::json!({
                "title": "metadata", "history": (0..24).map(|_| serde_json::json!({
                    "message": { "content": "payload\\\n".repeat(16_384) },
                    "contextItems": [{"content": "unused".repeat(4096)}],
                })).collect::<Vec<_>>()
            })
            .to_string(),
        );
        for text in cases {
            fs::write(&path, &text).unwrap();
            let expected = serde_json::from_str::<Value>(&text).ok().map(|v| {
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, crate::projection::Head::TITLE));
                let cwd = v
                    .get("workspaceDirectory")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path);
                (title, cwd)
            });
            let actual = scan_session(&path).unwrap().map(|s| (s.title, s.cwd));
            assert_eq!(actual, expected, "case prefix: {:?}", &text[..text.len().min(120)]);
        }
        fs::write(&path, b"{\"history\":[\"\xff\"]}").unwrap();
        assert!(scan_session(&path).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovery_scan_preserves_index_precedence_and_unindexed_fallback() {
        let _world = crate::test_world("continue-scan-index");
        let dir = cv_testkit::temp_dir("continue-index");
        fs::write(
            dir.join("sessions.json"),
            r#"[
            {"sessionId":"indexed","title":"index title","workspaceDirectory":"/index","dateCreated":"1700000000000"},
            {"sessionId":"indexed","title":"duplicate entry"},
            {"sessionId":"missing","title":"missing"}
        ]"#,
        )
        .unwrap();
        fs::write(
            dir.join("indexed.json"),
            r#"{"title":"file title","workspaceDirectory":"/file","history":[1,2]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("extra.json"),
            r#"{"title":"extra title","workspaceDirectory":"file:///extra","history":[1,null,{}]}"#,
        )
        .unwrap();
        fs::write(dir.join("broken.json"), r#"{"title":"partial","history":[1,}"#).unwrap();
        let refs = Continue {
            sessions: Some(dir.clone()),
        }
        .discover()
        .unwrap();
        assert_eq!(refs.len(), 3);
        let indexed = refs.iter().find(|r| r.id == "indexed").unwrap();
        assert_eq!(indexed.title.as_deref(), Some("index title"));
        assert_eq!(indexed.cwd.as_deref(), Some(Path::new("/index")));
        assert_eq!(indexed.created_at.unwrap().timestamp_millis(), 1700000000000);
        assert_eq!(indexed.message_count, None);
        let extra = refs.iter().find(|r| r.id == "extra").unwrap();
        assert_eq!(extra.title.as_deref(), Some("extra title"));
        assert_eq!(extra.cwd.as_deref(), Some(Path::new("/extra")));
        assert_eq!(extra.created_at, extra.updated_at);
        assert_eq!(extra.message_count, None);
        let broken = refs.iter().find(|r| r.id == "broken").unwrap();
        assert!(broken.title.is_none());
        assert_eq!(broken.message_count, None);
        // The undecodable file reads as an empty session that counts its one record.
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let read = adapter.parse(broken).unwrap();
        assert!(read.messages.is_empty());
        assert_eq!(read.extra["cv"]["skipped_lines"], 1);
        // An undecodable index is reported; the walk still lists every file, without its facts.
        fs::write(dir.join("sessions.json"), "{broken").unwrap();
        let found = adapter.discover().unwrap();
        assert_eq!(found.refs.len(), 3);
        assert_eq!(found.skipped.len(), 1);
        assert_eq!(found.skipped[0].path, dir.join("sessions.json"));
        assert!(found.refs.iter().find(|r| r.id == "indexed").unwrap().title.as_deref() == Some("file title"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_to_path_decodes_percent_escapes_and_normalizes_windows_drives() {
        assert_eq!(
            workspace_to_path("file:///Users/me/My%20Project%2B1"),
            PathBuf::from("/Users/me/My Project+1")
        );
        assert_eq!(
            workspace_to_path("file://localhost/home/dev/%E9%9B%AA"),
            PathBuf::from("/home/dev/雪")
        );
        assert_eq!(
            workspace_to_path("file:///C:/Users/dev/My%20Repo"),
            PathBuf::from("C:/Users/dev/My Repo")
        );
        assert_eq!(workspace_to_path("/d:/work/project"), PathBuf::from("d:/work/project"));
    }
}
