//! Cline adapter (VS Code extension) — per-task `api_conversation_history.json`, the 3.x extension's
//! store. The CLI 3.x and the 4.x extension write the SDK store in [`super::cline_sdk`]; [`Cline`]
//! reads both.
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

use crate::json::{ReadInput, Validation};
mod prompt;
mod writer;

use super::store::document::Fields;
use super::wire::anthropic;
use super::Adapter;
use crate::ir::*;
use crate::json::{InvalidJson, JsonSource, JsonView};
use crate::source::{sqlite_sidecars, Dependencies, Probe};
use crate::stream::{Demand, Flow, MessageSink};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The Cline VS Code extension id (its globalStorage subdir).
pub const CLINE_EXT_IDS: &[&str] = &["saoudrizwan.claude-dev"];
/// The Roo Code extension ids (it has shipped under two namespaces over its life).
pub const ROO_EXT_IDS: &[&str] = &["rooveterinaryinc.roo-cline", "rooveterinaryinc.roo-code"];

pub struct Cline {
    /// Legacy per-task `tasks/` dirs (the 3.x extension).
    roots: Vec<PathBuf>,
    /// SDK `sessions/` dirs (the CLI 3.x and the 4.x extension; see [`super::cline_sdk`]).
    sdk_roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Cline = Cline {
    roots: Vec::new(),
    sdk_roots: Vec::new(),
};

impl Cline {
    pub fn detect() -> Self {
        Cline {
            roots: task_roots(CLINE_EXT_IDS, ".cline"),
            sdk_roots: crate::env::home_dir()
                .map(|h| super::cline_sdk::session_roots(&h, true))
                .unwrap_or_default(),
        }
    }

    /// Cline's stores under an explicit home: the SDK store and a plain `.cline/tasks`.
    pub fn with_home(home: &Path) -> Self {
        let plain = home.join(".cline").join("tasks");
        Cline {
            roots: plain.is_dir().then_some(plain).into_iter().collect(),
            sdk_roots: super::cline_sdk::session_roots(home, false),
        }
    }
}

/// The side files of a Cline-family task directory, every one a freshness dependency.
pub(super) fn task_dir_dependencies(r: &SessionRef, deps: &mut Dependencies) {
    deps.files.extend(
        [
            "api_conversation_history.json",
            "ui_messages.json",
            "task_metadata.json",
            "history_item.json",
        ]
        .map(|name| r.path.join(name)),
    );
}

impl Adapter for Cline {
    fn harness(&self) -> Harness {
        Harness::Cline
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if r.path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".messages.json"))
        {
            let sid = r.id.split('/').next().unwrap_or(&r.id);
            if let Some(dir) = r.path.parent() {
                deps.files.push(dir.join(format!("{sid}.json")));
                if !r.id.contains('/') {
                    if let Some(data) = dir.parent().and_then(Path::parent) {
                        for name in super::cline_sdk::SIDE_DATABASES {
                            let db = data.join("db").join(name);
                            deps.files.extend(sqlite_sidecars(&db));
                            deps.files.push(db);
                        }
                    }
                }
            }
            return Some(());
        }
        task_dir_dependencies(r, deps);
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.iter().chain(&self.sdk_roots).cloned().collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let mut out = discover_in(&self.roots, Harness::Cline);
        super::cline_sdk::discover(&self.sdk_roots, &mut out);
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Cline, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            if super::cline_sdk::is_sdk_path(&r.path) {
                return super::cline_sdk::stream_with_options(&r.path, &r.id, opts, sink);
            }
            stream_task_dir(&r.path, &r.id, Harness::Cline, opts, sink)
        })
    }

    /// An SDK lead session reads its own rows of the store's side databases, which every session
    /// of the store writes: it is certified by them ([`super::cline_sdk::side_rows`]).
    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        super::cline_sdk::side_store(r)
    }

    fn rows_files(&self, store: &Path) -> Vec<PathBuf> {
        super::cline_sdk::SIDE_DATABASES.map(|name| store.join(name)).into()
    }

    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        super::cline_sdk::side_rows(store)
    }

    /// The SDK store's side databases: schedule fires and team runs ([`super::cline_sdk`]).
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        if super::cline_sdk::is_sdk_path(&r.path) {
            return super::cline_sdk::store_affordances(r);
        }
        Ok(Vec::new())
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
    if let Some(home) = crate::env::home_dir() {
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

    if let Some(home) = crate::env::home_dir() {
        // macOS
        bases.push(home.join("Library").join("Application Support"));
        // Linux / XDG
        bases.push(home.join(".config"));
    }
    // Windows
    if let Some(appdata) = crate::env::var_os("APPDATA") {
        bases.push(PathBuf::from(appdata));
    }
    // Allow $XDG_CONFIG_HOME to override the Linux default.
    if let Some(xdg) = crate::env::var_os("XDG_CONFIG_HOME") {
        bases.push(PathBuf::from(xdg));
    }

    let mut out = Vec::new();
    for base in bases {
        for editor in editors {
            let gs = base.join(editor).join("User").join("globalStorage");
            if gs.is_dir() && !out.contains(&gs) {
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
/// An unreadable `tasks/` dir, and a history that fails to read, are skipped and reported.
pub fn discover_in(roots: &[PathBuf], harness: Harness) -> crate::Discovered {
    let mut out = crate::Discovered::default();
    for root in roots {
        let Some(entries) = out.keep(root, fs::read_dir(root).map_err(Into::into)) else {
            continue;
        };
        for e in entries {
            let Some(e) = out.keep(root, e.map_err(Into::into)) else {
                continue;
            };
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
            // The scan reads the history, the metadata file's cwd and the three files' mtimes:
            // cached under those files' identities, rescanned only when one moves.
            let mut reads = crate::source::ReadSet::default();
            reads.record(&dir.join("ui_messages.json"));
            reads.record(&dir.join("task_metadata.json"));
            let scanned =
                crate::discover_cache::cached_scan_with_reads(&history, reads, || scan(&dir, &id, harness).map(Some));
            if let Some(found) = crate::Skipped::listed(&history, scanned) {
                out.push(found);
            }
        }
    }
    out
}

/// Whether one history item is a turn ([`Message::is_turn`]) as [`record`] reads it: a user or
/// assistant item that is not a condense summary (`isSummary`), and for a user item, not one
/// whose content is only `tool_result` blocks (a Tool turn). Reads block types, never bodies.
fn is_turn(role: &str, record: &Fields<'_>) -> Result<bool> {
    if !matches!(role, "user" | "assistant")
        || record.get("isSummary").map(|v| v.as_bool()).transpose()?.flatten() == Some(true)
    {
        return Ok(false);
    }
    if role == "assistant" {
        return Ok(true);
    }
    // `reasoning` is Cline's own thinking part; every other part is an Anthropic block.
    let only_results = anthropic::only_tool_results(record.get("content"), |part| {
        let reasoning = Fields::parse(part, &["type"])?
            .map(|f| f.string("type"))
            .transpose()?
            .flatten()
            .as_deref()
            == Some("reasoning");
        match reasoning {
            true => Ok(Some(false)),
            false => anthropic::block_kind(part, &anthropic::ANTHROPIC),
        }
    })?;
    Ok(!only_results)
}

#[cfg(test)]
static SCANNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Cheap metadata-only scan of one task dir into a `SessionRef`.
fn scan(dir: &Path, id: &str, harness: Harness) -> Result<SessionRef> {
    #[cfg(test)]
    SCANNED.lock().unwrap().push(dir.to_path_buf());
    let history = dir.join("api_conversation_history.json");
    let source = JsonSource::capture(&history)?;
    let (mut cwd, title, message_count) = source.read(ReadInput::Document, Validation::Value, |view| {
        let messages = view.array()?.context("history array")?;
        let mut cwd = None;
        let mut title = None;
        let mut message_count = 0;
        for view in messages {
            let view = view?;
            let Some(record) = Fields::parse(view, &["role", "content", "isSummary"])? else {
                continue;
            };
            let role = record.string("role")?.unwrap_or_default();
            if is_turn(&role, &record)? {
                message_count += 1;
            }
            if role == "user" && (cwd.is_none() || title.is_none()) {
                let summary = prompt::summarize(|feed| visit_text(record.get("content"), true, feed))?;
                if cwd.is_none() {
                    cwd = summary.cwd;
                }
                if title.is_none() {
                    title = summary.title;
                }
            }
        }
        Ok((cwd, title, message_count))
    })?;

    // created_at from the taskId (epoch millis); fall back to history mtime.
    let created_at = task_id_to_time(id);
    let updated_at = newest_mtime(dir).or(created_at);
    let created_at = created_at.or(updated_at);

    // task_metadata.json can carry a cwd hint when the first message lacked one.
    if cwd.is_none() {
        cwd = cwd_from_metadata(dir);
    }

    Ok(SessionRef {
        cwd,
        title,
        created_at,
        updated_at,
        message_count: Some(message_count),
        ..SessionRef::new(id.to_string(), harness, dir.to_path_buf())
    })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Test helper: [`stream_task_dir`] collected into a whole `Session`.
#[cfg(test)]
pub(crate) fn parse_task_dir(dir: &Path, id: &str, harness: Harness) -> Result<Session> {
    use crate::stream::CollectSink;
    let mut sink = CollectSink::default();
    let mut session = stream_task_dir(dir, id, harness, &Demand::RICH, &mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// Stream a task's chronological array through the shared validated source view. Bodies and
/// complete tool arguments remain source ranges in lazy mode; metadata is reduced as it arrives.
/// Full parsing owns the requested output. Shared by Cline and Roo; returns no collected messages.
pub fn stream_task_dir(
    dir: &Path,
    id: &str,
    harness: Harness,
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let history = dir.join("api_conversation_history.json");

    let mut session = Session {
        source_path: Some(dir.to_path_buf()),
        ..Session::new(id.to_string(), harness)
    };

    // Resolve sidecar timestamps without retaining their text payloads. cwd-from-metadata is only a *fallback*
    // (the first user turn's <environment_details> wins, mirroring the old parse order), so it's
    // applied after the body — kept here for the early `meta` signal only.
    for name in [
        "api_conversation_history.json",
        "task_metadata.json",
        "ui_messages.json",
        "history_item.json",
    ] {
        sink.dependency(&dir.join(name));
    }
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    let metadata = task_metadata(dir, true)?;
    let meta_cwd = metadata.cwd;
    session.model = metadata.model;
    session.cwd = meta_cwd.clone();
    session.created_at = task_id_to_time(id);
    session.updated_at = newest_mtime(dir).or(session.created_at);
    // ui_messages.json gives the most precise created/updated and the first user turn's timestamp.
    // Roo also takes its store affordances from this file: one read serves both.
    let roo = match harness {
        Harness::Roo => super::roo::ui_times(dir),
        _ => None,
    };
    let (ui_created, ui_updated, first_user_ts) = match roo {
        Some(times) => times,
        None => ui_message_times(dir)?,
    };
    if let Some(c) = ui_created {
        session.created_at = Some(c);
    }
    if let Some(u) = ui_updated {
        if session.updated_at.map(|cur| u > cur).unwrap_or(true) {
            session.updated_at = Some(u);
        }
    }

    // Roo (3.5x) writes `history_item.json` beside each task: its task-history row, which is where
    // a `new_task` child names its parent (`parentTaskId`, `rootTaskId`) and the task's mode lives.
    if let Some(hi) = optional_document(&dir.join("history_item.json"), |view| {
        Ok(Value::Object(
            Fields::parse(
                view,
                &[
                    "parentTaskId",
                    "rootTaskId",
                    "mode",
                    "apiConfigName",
                    "status",
                    "tokensIn",
                    "tokensOut",
                    "totalCost",
                    "ts",
                ],
            )?
            .map(|fields| {
                fields.project(&[
                    "parentTaskId",
                    "rootTaskId",
                    "mode",
                    "apiConfigName",
                    "status",
                    "tokensIn",
                    "tokensOut",
                    "totalCost",
                    "ts",
                ])
            })
            .transpose()?
            .unwrap_or_default(),
        ))
    })? {
        session.lineage.parent = hi.get("parentTaskId").and_then(Value::as_str).map(str::to_string);
        let bag = session.harness_extra_mut(harness);
        for k in [
            "rootTaskId",
            "mode",
            "apiConfigName",
            "status",
            "tokensIn",
            "tokensOut",
            "totalCost",
        ] {
            if let Some(v) = hi.get(k).filter(|v| !v.is_null()) {
                bag.insert(k.into(), v.clone());
            }
        }
        if session.created_at.is_none() {
            session.created_at = hi.get("ts").and_then(Value::as_i64).and_then(super::ts_from_epoch);
        }
    }

    // Emit the early `meta` signal with the sidecar-derived fields (header-rendering sinks use it;
    // most ignore it). The authoritative cwd/title are folded from the transcript below and live on
    // the returned `Session`, which `collect`/`parse` reattach the messages to.
    sink.meta(&session);
    // Now clear cwd so the first user turn's <environment_details> takes priority; we re-apply the
    // metadata fallback after the body if the transcript yielded nothing.
    session.cwd = None;

    // A task with no history file is an empty session; one that cannot be read is an error.
    let source = match JsonSource::capture(&history) {
        Ok(source) => source,
        Err(error) if super::is_not_found(&error) => {
            session.cwd = meta_cwd;
            return Ok(session);
        }
        Err(error) => return Err(error),
    };
    sink.dependency(&history);
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    let mut stamped_first_user = false;
    // An item that is not one JSON value, and a history that does not decode, are counted.
    let mut unparsed = super::Unparsed::default();
    let result = source.read(ReadInput::Document, Validation::Syntax, |items| {
        let items = items.accepted_array()?;
        for view in items.into_iter().flatten() {
            let Some(view) = view? else {
                unparsed.count();
                continue;
            };
            let (boundary, message) = record(view, &mut session, opts.spans)?;
            if let Some(boundary) = boundary {
                if sink.try_message(boundary)? == Flow::Stop {
                    break;
                }
            }
            if let Some(mut message) = message {
                if !stamped_first_user && message.role == Role::User {
                    stamped_first_user = true;
                    if let Some(ts) = first_user_ts {
                        message.timestamp.get_or_insert(ts);
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
        }
        source.check()?;
        Ok(())
    });
    if let Err(error) = result {
        if !error.is::<InvalidJson>() {
            return Err(error);
        }
        unparsed.count();
    }
    unparsed.note(&mut session);

    // cwd fallback: the first user turn's <environment_details> wins; only if it yielded nothing do
    // we fall back to the task_metadata.json hint (matching the old parse order).
    if session.cwd.is_none() {
        session.cwd = meta_cwd;
    }

    Ok(session)
}

/// Test helper: an `api_conversation_history.json` array text through the same record codec as
/// [`stream_task_dir`], over an owned view.
#[cfg(test)]
pub(crate) fn parse_history_str(id: &str, text: &str, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let mut session = Session {
        source_path,
        ..Session::new(id.to_string(), harness)
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return session;
    };
    for item in &items {
        if let Ok((boundary, message)) = record(item.into(), &mut session, false) {
            session.messages.extend(boundary.into_iter().chain(message));
        }
    }
    session
}

fn message_with_blocks(
    v: &Value,
    blocks: Vec<Block>,
    session: &mut Session,
    summary: Option<prompt::Summary>,
    system_text: Option<String>,
) -> Option<Message> {
    let role = match v.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => return None,
    };
    // Side effects from the first user turn: pull cwd + a clean title out of the prompt text, and
    // note the harness-injected `<task>` / `<environment_details>` wrappers (which are embedded in
    // the human's own text, so the turn stays a Prompt — we just flag that it carried them).
    let mut wraps_task = false;
    let mut wraps_env = false;
    if let Some(summary) = summary.filter(|_| role == Role::User) {
        wraps_task = summary.task;
        wraps_env = summary.environment;
        if session.cwd.is_none() {
            session.cwd = summary.cwd;
        }
        if session.title.is_none() {
            session.title = summary.title;
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
    m.timestamp = v.get("ts").and_then(Value::as_i64).and_then(super::ts_from_epoch);
    // A `system` message carries the system prompt (rare in Cline; it stores raw Anthropic turns).
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.content = blocks;
    if role == Role::System && session.system_prompt.is_none() {
        if let Some(t) = system_text.filter(|t| !t.trim().is_empty()) {
            session.system_prompt = Some(t);
        }
    }
    // Roo's non-destructive condense: the summary turn is `isSummary`, and every turn it replaced
    // stays in the file with `condenseParent` = the condense id (no longer sent to the model).
    if v.get("isSummary").and_then(Value::as_bool) == Some(true) {
        m.kind = MessageKind::CompactionSummary;
        m.origin = Origin::Harness;
        if let Some(id) = v.get("condenseId") {
            m.harness_extra_mut(session.harness)
                .insert("condense_id".into(), id.clone());
        }
    }
    if let Some(parent) = v.get("condenseParent").filter(|p| !p.is_null()) {
        m.harness_extra_mut(session.harness)
            .insert("condensed_into".into(), parent.clone());
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

const MESSAGE_FIELDS: &[&str] = &["role", "isSummary", "condenseId", "condenseParent", "ts", "content"];

/// Enumerate only body text views; None is an empty parsed Text block.
fn text_views<'a>(
    content: Option<JsonView<'a>>,
    discovery: bool,
    mut visit: impl FnMut(Option<JsonView<'a>>) -> Result<()>,
) -> Result<()> {
    let Some(content) = content else { return Ok(()) };
    if content.is_string()? {
        visit(Some(content))?;
    } else if let Some(parts) = content.array()? {
        for part in parts {
            let Some(part) = Fields::parse(part?, &["type", "text"])? else {
                continue;
            };
            let text = if let Some(view) = part.get("text") {
                view.is_string()?.then_some(view)
            } else {
                None
            };
            let selected = if discovery {
                text.is_some()
            } else {
                part.string("type")?.as_deref() == Some("text")
            };
            if selected {
                visit(text)?;
            }
        }
    }
    Ok(())
}
/// Feed the same joined text as text_parts (discovery) or parsed Text blocks (streaming).
fn visit_text(content: Option<JsonView<'_>>, discovery: bool, feed: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    let mut first = true;
    text_views(content, discovery, |text| {
        if !first {
            feed("\n")?;
        }
        first = false;
        if let Some(text) = text {
            text.for_each_str_chunk(|chunk| feed(chunk))?;
        }
        Ok(())
    })
}
/// The record codec, the only decoder of this format: one history item in, the condense boundary
/// it implies and its message out. Text follows [`JsonView::lazy_text`]: source text names the
/// history file it lives in (the session's own path is the task dir).
fn record(view: JsonView<'_>, session: &mut Session, spans: bool) -> Result<(Option<Message>, Option<Message>)> {
    let Some(record) = Fields::parse(view, MESSAGE_FIELDS)? else {
        return Ok((None, None));
    };
    let metadata = Value::Object(record.project(&MESSAGE_FIELDS[..MESSAGE_FIELDS.len() - 1])?);
    let boundary = condense_boundary(&metadata, session.harness);
    let content = record.get("content");
    let role = metadata.get("role").and_then(Value::as_str);
    let summary = if role == Some("user") {
        Some(prompt::summarize(|feed| visit_text(content, false, feed))?)
    } else {
        None
    };
    let system_text = if role == Some("system") && session.system_prompt.is_none() {
        let mut text = String::new();
        text_views(content, false, |view| {
            let mut started = false;
            if let Some(view) = view {
                view.for_each_str_chunk(|chunk| {
                    if !chunk.is_empty() && !started {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        started = true;
                    }
                    text.push_str(chunk);
                    Ok(())
                })?;
            }
            Ok(())
        })?;
        Some(text)
    } else {
        None
    };
    let mut blocks = Vec::new();
    if let Some(content) = content {
        if let Some(text) = content.lazy_text(spans)? {
            blocks.push(Block::Text { text });
        } else if let Some(parts) = content.array()? {
            for part in parts {
                let part = part?;
                let reasoning = Fields::parse(part, &["type", "text"])?;
                let block = if reasoning
                    .as_ref()
                    .map(|r| r.string("type"))
                    .transpose()?
                    .flatten()
                    .as_deref()
                    == Some("reasoning")
                {
                    Some(Block::Thinking {
                        text: reasoning
                            .unwrap()
                            .get("text")
                            .map(|v| v.lazy_text(spans))
                            .transpose()?
                            .flatten()
                            .unwrap_or_default(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    })
                } else {
                    anthropic::block_from_view_with(
                        part,
                        &anthropic::ANTHROPIC,
                        spans,
                        &mut |v| v.lazy_text(spans),
                        crate::harness::wire::joined_over(spans),
                    )?
                };
                if let Some(block) = block {
                    blocks.push(block);
                }
            }
        }
    }
    Ok((
        boundary,
        message_with_blocks(&metadata, blocks, session, summary, system_text),
    ))
}

/// The synthetic [`MessageKind::CompactionBoundary`] that precedes a Roo condense summary
/// (`isSummary: true`), so the shared compaction sink pairs it with the summary that follows.
fn condense_boundary(v: &Value, harness: Harness) -> Option<Message> {
    if v.get("isSummary").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut b = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
    b.timestamp = v.get("ts").and_then(Value::as_i64).and_then(super::ts_from_epoch);
    b.content = vec![Block::Text {
        text: "context condensed".into(),
    }];
    if let Some(id) = v.get("condenseId") {
        b.id = id.as_str().map(|s| format!("{s}#boundary"));
        b.harness_extra_mut(harness).insert("condense_id".into(), id.clone());
    }
    Some(b)
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
/// This is the faithful inverse of [`stream_task_dir`]: every IR [`Block`] maps back to the
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

/// The emit core for both harnesses that write this layout: Cline and Roo (`roo::emit` passes
/// `Harness::Roo`). The per-task bytes are identical; `harness` only names, in the resume hint,
/// which editor extension opens the task.
pub fn emit_with_harness(
    session: &Session,
    out_dir: &Path,
    opts: &crate::emit::EmitOptions,
    harness: Harness,
) -> Result<crate::harness::EmitResult> {
    opts.validate_ids()?;
    let extension = match harness {
        Harness::Roo => "Roo Code",
        _ => "Cline",
    };
    // taskId: an explicit override, else a ms-epoch string the parser can turn back into created_at.
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| created.timestamp_millis().to_string());

    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd.as_ref().map(|p| p.to_string_lossy().to_string());

    // Complete checked production before opening any destination transcript or sidecar.
    let stage = writer::history(session, opts, harness, cwd_str.as_deref())?;
    let task_dir = out_dir.join("tasks").join(&new_id);
    fs::create_dir_all(&task_dir).with_context(|| format!("creating {}", task_dir.display()))?;
    let history_path = task_dir.join("api_conversation_history.json");
    // The three files publish as one set, the transcript last: discovery finds a task by its
    // transcript, so a crash midway leaves nothing listed.
    let mut set = crate::publish::Set::new();

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
    let meta = serde_json::to_string_pretty(&Value::Object(meta))?;
    set.stage(&meta_path, "stage", meta.as_bytes(), opts.claim())?;

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
    let ui = serde_json::to_string_pretty(&Value::Array(ui))?;
    set.stage(&ui_path, "stage", ui.as_bytes(), opts.claim())?;
    super::store::stage_into(&mut set, stage, &history_path, opts.claim())?;
    set.publish()?;

    let resume = match &cwd {
        Some(c) => format!(
            "open {} in {extension} (task {new_id}, cwd {})",
            task_dir.display(),
            c.display()
        ),
        None => format!("open {} in {extension} (task {new_id})", task_dir.display()),
    };
    Ok(crate::harness::EmitResult {
        path: task_dir,
        new_id,
        resume_hint: Some(resume),
    })
}

// ---------------------------------------------------------------------------
// cwd / title / timestamp helpers
// ---------------------------------------------------------------------------

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
    metadata_cwd(dir).ok().flatten()
}
fn metadata_cwd(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(task_metadata(dir, false)?.cwd)
}

#[derive(Default)]
struct TaskMetadata {
    cwd: Option<PathBuf>,
    model: Option<String>,
}

fn task_metadata(dir: &Path, read_model: bool) -> Result<TaskMetadata> {
    Ok(optional_document(&dir.join("task_metadata.json"), |view| {
        let keys = ["cwd", "workspace", "workspacePath", "workspaceFolder", "cwd_path"];
        let Some(fields) = Fields::parse(
            view,
            &[
                "cwd",
                "workspace",
                "workspacePath",
                "workspaceFolder",
                "cwd_path",
                "model_usage",
            ],
        )?
        else {
            return Ok(TaskMetadata::default());
        };
        let mut metadata = TaskMetadata::default();
        for key in keys {
            if let Some(path) = fields.string(key)?.filter(|p| !p.is_empty()) {
                metadata.cwd = Some(PathBuf::from(path));
                break;
            }
        }
        if let Some(usage) = fields
            .get("model_usage")
            .filter(|_| read_model)
            .map(JsonView::array)
            .transpose()?
            .flatten()
        {
            for entry in usage {
                if let Some(entry) = Fields::parse(entry?, &["model_id"])? {
                    if let Some(model) = entry.string("model_id")?.filter(|s| !s.is_empty()) {
                        metadata.model = Some(model);
                    }
                }
            }
        }
        Ok(metadata)
    })?
    .unwrap_or_default())
}
fn optional_document<T>(path: &Path, visit: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<Option<T>> {
    let Ok(source) = JsonSource::capture(path) else {
        return Ok(None);
    };
    match source.read(ReadInput::Document, Validation::Value, visit) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is::<InvalidJson>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read timestamp hints from `ui_messages.json` (an array of `{ts: epoch_ms, type, say/ask, text}`
/// events) without materializing message bodies. Returns `(created, updated, first_user_ts)`:
///
/// - `created`: the first event's `ts` — the most precise "created" we have (overrides taskId/mtime,
///   matching the old `enrich_from_ui_messages` which unconditionally set `created_at` from it).
/// - `updated`: the last event's `ts` (the caller keeps it only if it's newer than the mtime guess).
/// - `first_user_ts`: same first `ts`, which the streaming core stamps onto the first user turn.
///
/// The three timestamp hints extracted from a task's `ui_messages.json`.
pub(super) type UiTimes = (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Reduce timestamps while skipping UI text payloads under the original whole-Value boundary.
fn ui_message_times(dir: &Path) -> Result<UiTimes> {
    Ok(optional_document(&dir.join("ui_messages.json"), |view| {
        let mut first_ts = None;
        let mut last_ts = None;
        if let Some(items) = view.array()? {
            for item in items {
                let Some(fields) = Fields::parse(item?, &["ts"])? else {
                    continue;
                };
                if let Some(ts) = fields
                    .get("ts")
                    .map(JsonView::as_number)
                    .transpose()?
                    .flatten()
                    .and_then(|n| n.as_i64())
                {
                    if let Some(dt) = DateTime::<Utc>::from_timestamp_millis(ts) {
                        first_ts.get_or_insert(dt);
                        last_ts = Some(dt);
                    }
                }
            }
        }
        Ok((first_ts, last_ts, first_ts))
    })?
    .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use crate::stream::CollectSink;

    /// Discovery rescans a task only when one of the files its scan reads moves.
    #[test]
    fn discovery_rescans_only_a_task_whose_files_moved() {
        let world = crate::test_world("cline-discovery-cache");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let root = world.base.join("tasks");
        for id in ["1717000000001", "1717000000002", "1717000000003"] {
            let dir = root.join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("api_conversation_history.json"),
                r#"[{"role":"user","content":[{"type":"text","text":"<task>hi</task>"}]}]"#,
            )
            .unwrap();
        }
        let scanned = || {
            let mut all = SCANNED.lock().unwrap();
            let mut mine: Vec<PathBuf> = all.iter().filter(|p| p.starts_with(&root)).cloned().collect();
            all.retain(|p| !p.starts_with(&root));
            mine.sort();
            mine
        };
        let first = discover_in(std::slice::from_ref(&root), Harness::Cline).refs;
        assert_eq!(scanned().len(), 3);
        let again = discover_in(std::slice::from_ref(&root), Harness::Cline).refs;
        assert!(scanned().is_empty());
        assert_eq!(format!("{first:?}"), format!("{again:?}"));
        let moved = root.join("1717000000002");
        fs::write(moved.join("task_metadata.json"), r#"{"cwd":"/meta"}"#).unwrap();
        let refs = discover_in(std::slice::from_ref(&root), Harness::Cline).refs;
        assert_eq!(scanned(), vec![moved.clone()]);
        let r = refs.iter().find(|r| r.path == moved).unwrap();
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/meta")));
    }

    #[test]
    fn editor_roots_do_not_repeat_default_xdg_stores() {
        let world = crate::test_world("cline-root-alias");
        let storage = world.home.join(".config/Code/User/globalStorage");
        fs::create_dir_all(storage.join(CLINE_EXT_IDS[0]).join("tasks")).unwrap();
        fs::create_dir_all(storage.join(ROO_EXT_IDS[0]).join("tasks")).unwrap();
        assert_eq!(global_storage_roots(), vec![storage.clone()]);
        assert_eq!(
            task_roots(CLINE_EXT_IDS, ".cline"),
            vec![storage.join(CLINE_EXT_IDS[0]).join("tasks")]
        );
        assert_eq!(
            task_roots(ROO_EXT_IDS, ".roo"),
            vec![storage.join(ROO_EXT_IDS[0]).join("tasks")]
        );
    }

    /// The task's three files are one publication: the transcript's claim fails after the
    /// metadata and UI log were claimed (a directory holds its name), and every name is left as
    /// it was, replaced files (`--force`) included.
    #[test]
    fn task_files_publish_together_or_not_at_all() {
        let world = crate::test_world("cline-emit-set");
        let mut source = Session::new("source", Harness::Claude);
        source.messages.push(Message::new(Role::User));
        for force in [false, true] {
            let out = world.base.join(format!("out-{force}"));
            let task = out.join("tasks/fixed");
            fs::create_dir_all(task.join("api_conversation_history.json")).unwrap();
            if force {
                fs::write(task.join("task_metadata.json"), "old metadata").unwrap();
                fs::write(task.join("ui_messages.json"), "old ui").unwrap();
            }
            let opts = crate::emit::EmitOptions {
                new_id: Some("fixed".into()),
                force,
                ..Default::default()
            };
            assert!(emit(&source, &out, &opts).is_err());
            assert_eq!(fs::read_dir(&task).unwrap().count(), 1 + 2 * force as usize);
            if force {
                assert_eq!(
                    fs::read_to_string(task.join("task_metadata.json")).unwrap(),
                    "old metadata"
                );
                assert_eq!(fs::read_to_string(task.join("ui_messages.json")).unwrap(), "old ui");
            }
        }
    }

    #[test]
    fn emitted_model_metadata_round_trips_for_cline_and_roo() {
        let world = crate::test_world("cline-model-metadata");
        let mut source = Session::new("source", Harness::Claude);
        source.model = Some("selected-model".into());
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text { text: "prompt".into() });
        source.messages.push(user);
        for harness in [Harness::Cline, Harness::Roo] {
            let out = world.base.join(harness.as_str());
            let opts = crate::emit::EmitOptions {
                new_id: Some("fixed".into()),
                ..Default::default()
            };
            let emitted = emit_with_harness(&source, &out, &opts, harness).unwrap();
            let session = parse_task_dir(&emitted.path, &emitted.new_id, harness).unwrap();
            assert_eq!(session.model, source.model);
            fs::write(
                emitted.path.join("task_metadata.json"),
                r#"{"model_usage":[{"model_id":"earlier"},{"model_id":"latest"},{"model_id":""}]}"#,
            )
            .unwrap();
            let session = parse_task_dir(&emitted.path, &emitted.new_id, harness).unwrap();
            assert_eq!(session.model.as_deref(), Some("latest"));
        }
    }

    #[test]
    fn transcript_timestamps_survive_tool_reclassification_and_condensation() {
        let world = crate::test_world("cline-native-times");
        let dir = world.base.join("1717000000000");
        fs::create_dir(&dir).unwrap();
        let records = json!([
            {"role":"user","ts":1717000000007_i64,"content":"prompt"},
            {"role":"assistant","ts":1717000000011_i64,"content":[{"type":"tool_use","id":"c","name":"read","input":{}}]},
            {"role":"user","ts":1717000000022_i64,"content":[{"type":"tool_result","tool_use_id":"c","content":"done"}]},
            {"role":"user","ts":1717000000033_i64,"content":"summary","isSummary":true,"condenseId":"x"}
        ]);
        let wire = serde_json::to_string(&records).unwrap();
        fs::write(dir.join("api_conversation_history.json"), &wire).unwrap();
        fs::write(dir.join("ui_messages.json"), r#"[{"ts":1717000000001}]"#).unwrap();
        for harness in [Harness::Cline, Harness::Roo] {
            let owned = parse_history_str("1717000000000", &wire, harness, None);
            assert_eq!(owned.messages[0].timestamp.unwrap().timestamp_millis(), 1717000000007);
            assert_eq!(owned.messages[2].role, Role::Tool);
            assert_eq!(owned.messages[2].timestamp.unwrap().timestamp_millis(), 1717000000022);
            assert_eq!(owned.messages[3].kind, MessageKind::CompactionBoundary);
            assert_eq!(owned.messages[3].timestamp, owned.messages[4].timestamp);
            for opts in [Demand::RICH, Demand::LEAN.with_spans(), Demand::COMPLETE] {
                let mut sink = CollectSink::default();
                stream_task_dir(&dir, "1717000000000", harness, &opts, &mut sink).unwrap();
                assert_eq!(
                    serde_json::to_value(&sink.messages).unwrap(),
                    serde_json::to_value(&owned.messages).unwrap()
                );
            }
        }
    }

    #[test]
    fn native_task_views_match_owned_messages_metadata_and_invalid_record_boundaries() {
        let world = cv_testkit::World::new("cline-source-views");
        let dir = world.base.join("1717000000000");
        fs::create_dir(&dir).unwrap();
        let history = dir.join("api_conversation_history.json");
        let body = "雪\n\"\\".repeat(8192);
        let prompt = format!("{body}<task>late selected title</task>\n# Current Working Directory (/selected) Files");
        let records = json!([
            {"role":"user","content":[{"type":"text","text":""},{"type":"text","text":prompt}]},
            {"role":"assistant","content":[
                {"type":"reasoning","text":body},
                {"type":"tool_use","id":"call","name":"read","input":{"payload":body,"n":1}},
                {"type":"text","text":"done"}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":body}]},
            {"role":"system","content":[{"type":"text","text":""},{"type":"text","text":body}]},
            {"role":"user","content":"condensed","isSummary":true,"condenseId":"c"}
        ]);
        let mut wire = serde_json::to_string(&records).unwrap();
        wire.pop();
        wire.push_str(", {\"role\":\"user\",\"content\":\"discard\",\"unknown\":1e400}, {\"role\":\"user\",\"content\":\"discard\",\"unknown\":\"\\ud800\"}]");
        fs::write(&history, &wire).unwrap();
        fs::write(
            dir.join("task_metadata.json"),
            serde_json::to_vec(&json!({"cwd":"/fallback","ignored":body})).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("ui_messages.json"),
            serde_json::to_vec(&json!([
                {"ts":1717000000001_i64,"text":body},{"ts":1717000000002_i64,"text":body}
            ]))
            .unwrap(),
        )
        .unwrap();
        for harness in [Harness::Cline, Harness::Roo] {
            let expected = parse_task_dir(&dir, "1717000000000", harness).unwrap();
            assert_eq!(expected.title.as_deref(), Some("late selected title"));
            assert_eq!(expected.cwd, Some(PathBuf::from("/selected")));
            assert_eq!(
                expected.created_at,
                DateTime::<Utc>::from_timestamp_millis(1717000000001)
            );
            assert_eq!(expected.system_prompt.as_deref(), Some(body.as_str()));
            // The two children that are not one JSON value are counted, not dropped silently.
            assert_eq!(expected.extra["cv"]["skipped_lines"], 2);
            for opts in [Demand::LEAN.with_spans(), Demand::RICH, Demand::COMPLETE] {
                let mut sink = CollectSink::default();
                let mut actual = stream_task_dir(&dir, "1717000000000", harness, &opts, &mut sink).unwrap();
                actual.messages = sink.messages;
                assert_eq!(
                    actual.messages.iter().flat_map(|m| &m.content).any(|block| {
                        matches!(
                            block,
                            Block::ToolUse {
                                input: crate::Json::Source(_),
                                ..
                            }
                        )
                    }),
                    opts.spans
                );
                if opts.spans {
                    assert!(actual.messages.iter().flat_map(|m| &m.content).any(|block| matches!(
                        block,
                        Block::Text { text: crate::Text::Source(text) } if text.path() == history.as_path()
                    )));
                }
                actual.materialize().unwrap();
                assert_eq!(
                    serde_json::to_string(&actual).unwrap(),
                    serde_json::to_string(&expected).unwrap()
                );
            }
            // Discovery retains its whole-Value acceptance, unlike per-child transcript replay.
            assert!(scan(&dir, "1717000000000", harness).is_err());
        }
        fs::write(&history, serde_json::to_vec(&records).unwrap()).unwrap();
        let r = scan(&dir, "1717000000000", Harness::Cline).unwrap();
        assert_eq!(r.title.as_deref(), Some("late selected title"));
        assert_eq!(r.cwd, Some(PathBuf::from("/selected")));
        // The prompt and the reply: the tool results, the system turn and the summary are not turns.
        assert_eq!(r.message_count, Some(2));
        fs::write(&history, "[{\"role\":\"user\",\"content\":\"ok\"},]").unwrap();
        assert!(parse_task_dir(&dir, "1717000000000", Harness::Cline)
            .unwrap()
            .messages
            .is_empty());
    }

    #[test]
    fn roo_reasoning_and_a_condense_summary_with_its_boundary() {
        let text = r#"[
            {"role":"user","content":[{"type":"text","text":"list primes"}],"ts":1791750000000,"condenseParent":"c1"},
            {"role":"assistant","content":[{"type":"reasoning","text":"think","summary":[]},{"type":"text","text":"2 3 5"}],"condenseParent":"c1"},
            {"role":"user","content":[{"type":"text","text":"summary of it"}],"ts":1791750001000,"isSummary":true,"condenseId":"c1"}
        ]"#;
        let s = parse_history_str("t", text, Harness::Roo, None);
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary
            ]
        );
        assert!(
            matches!(&s.messages[1].content[0], Block::Thinking { text, .. } if text.matches_str("think") == Some(true))
        );
        assert_eq!(
            s.messages[0]
                .harness_extra(Harness::Roo)
                .and_then(|b| b.get("condensed_into")),
            Some(&json!("c1"))
        );
        assert_eq!(s.messages[2].id.as_deref(), Some("c1#boundary"));
    }

    #[test]
    fn roo_history_item_names_the_parent_task() {
        let dir = std::env::temp_dir().join(format!("cv-roo-hi-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("api_conversation_history.json"),
            r#"[{"role":"user","content":"ok?"}]"#,
        )
        .unwrap();
        fs::write(
            dir.join("history_item.json"),
            r#"{"id":"child","parentTaskId":"lead","mode":"ask"}"#,
        )
        .unwrap();
        let s = parse_task_dir(&dir, "child", Harness::Roo).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("lead"));
        assert_eq!(
            s.harness_extra(Harness::Roo).and_then(|b| b.get("mode")),
            Some(&json!("ask"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

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
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Fix the parser bug".to_string()),
            created_at: DateTime::<Utc>::from_timestamp_millis(1717000000000),
            updated_at: DateTime::<Utc>::from_timestamp_millis(1717000005000),
            model: Some("claude-x".to_string()),
            ..Session::new("src", Harness::Cline)
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
                input: json!({ "path": "a.rs" }).into(),
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

        let cline = Cline::detect();
        let res = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).expect("emit");
        assert_eq!(res.path, out_dir.join("tasks").join(&res.new_id));

        // Re-parse via this adapter's parse path.
        let r = SessionRef::new(res.new_id.clone(), Harness::Cline, res.path.clone());
        let parsed = cline.parse(&r).expect("re-parse");
        crate::harness::assert_no_flat_keys(&parsed);

        // Roles survive: user, assistant, tool.
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(parsed.messages[2].role, Role::Tool);

        // cwd + title threaded through the first user turn / metadata.
        assert_eq!(parsed.cwd.as_deref(), Some(std::path::Path::new("/Users/me/proj")));
        assert_eq!(parsed.title.as_deref(), Some("Fix the parser bug"));

        // Assistant text + tool_use blocks survive.
        assert!(
            matches!(&parsed.messages[1].content[0], Block::Text { text } if text.matches_str("on it") == Some(true))
        );
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
