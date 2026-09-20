//! Gemini / Antigravity adapter — best-effort, multi-format.
//!
//! `~/.gemini/` holds several distinct on-disk shapes; we read every readable one:
//!
//! 1. **Chat recordings** (richest) — `~/.gemini/tmp/<projectHash>/chats/session-<ts>-<id>.json`
//!    (legacy, a single whole-file `ConversationRecord`) or `.jsonl` (modern, append-only: a metadata
//!    first line, then per-message records, `{"$set":…}` metadata patches, and `{"$rewindTo":id}`
//!    truncation markers). These carry real assistant turns, thinking, tool calls *and* tool results
//!    plus per-message token usage and model. Subagent recordings live under
//!    `chats/<parentId>/<id>.jsonl`. (See gemini-cli `core/src/services/chatRecordingService.ts`.)
//! 2. **gemini-cli checkpoints** — `~/.gemini/tmp/<hash>/checkpoint-<tag>.json`, either
//!    `{ history: Content[], authType? }` or a bare legacy `Content[]` array, where each `Content` is
//!    `{ role: "user"|"model", parts: Part[] }`. (See gemini-cli `core/src/core/logger.ts`.) These are
//!    transient (deleted on resume) so they may not be present, but we parse them when they are.
//! 3. **Readable fallback** — `~/.gemini/tmp/<hash>/logs.json`, an array of
//!    `{sessionId, messageId, type, message, timestamp}` (user prompts reliably; weak on assistant).
//!    One `logs.json` may interleave several `sessionId`s; each becomes its own IR session.
//!
//! **Roots.** gemini-cli's runtime state normally lives under `~/.gemini/`, but under the macOS
//! Seatbelt sandbox (`SANDBOX=sandbox-exec`) the profile blocks writes there, so
//! `Storage.getGlobalRuntimeDir()` routes everything to `~/.cache/.gemini/` instead
//! (`packages/core/src/config/storage.ts:90-107`, `getGlobalTempDir` `:195-197`). Both `tmp` roots
//! are scanned; a session is keyed by its id, so nothing is listed twice.
//!
//! **cwd.** A recording lives at `<runtime>/tmp/<projectIdentifier>/chats/…`, where the identifier is
//! either the legacy sha256 of the project path or (current) a short id such as `claurdvoyant`. The
//! path itself is recorded in `<runtime>/projects.json` (`{"projects": {"/abs/path": "short-id"}}`,
//! pretty-printed) and in a bare-path `.project_root` marker the registry writes into EVERY base dir
//! it manages — `<runtime>/tmp/<short-id>/.project_root` (right next to `chats/`) and
//! `<runtime>/history/<short-id>/.project_root` (`config/projectRegistry.ts` `baseDirs` +
//! `PROJECT_ROOT_FILE`, `storage.ts:283-296`). Verified on 0.46.0: a fresh project dir got all three
//! at once, the short id being the dir's basename (`proj`, `proj-1` on collision). The record's own
//! `directories[]` wins when present; otherwise the cwd is recovered from the registry — before that,
//! almost every Gemini session had `cwd: None`.
//!
//! The closed Antigravity IDE's `~/.gemini/antigravity/conversations/*.pb` are opaque (compressed /
//! no on-disk schema, no readable strings) and are *not* parsed.
//!
//! `parse_all_str` (used by the wasm ingest path) handles all three readable JSON/JSONL shapes purely,
//! with no filesystem access.

use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Gemini {
    /// Every `tmp` root that exists: `~/.gemini/tmp`, then the sandbox root `~/.cache/.gemini/tmp`.
    roots: Vec<PathBuf>,
}

impl Gemini {
    pub fn new() -> Self {
        let roots = dirs::home_dir()
            .map(|h| {
                vec![
                    h.join(".gemini").join("tmp"),
                    // `SANDBOX=sandbox-exec` runs (macOS Seatbelt) write here instead — storage.ts:90-107.
                    h.join(".cache").join(".gemini").join("tmp"),
                ]
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        Gemini { roots }
    }
}

impl Default for Gemini {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Gemini {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        if self.roots.is_empty() {
            return Ok(vec![]);
        }
        // Collect candidate file paths first (cheap walk over every root), then read+parse in
        // parallel. A single `logs.json` can expand into many sessions, so this is a flat-map.
        let paths: Vec<_> = self
            .roots
            .iter()
            .flat_map(|root| WalkDir::new(root).into_iter().filter_map(|e| e.ok()))
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect();
        let refs = crate::par_flat_map(paths, |path| {
            crate::discover_cache::cached_scan_many(&path, || scan_session_file(&path, Harness::Gemini))
        });
        Ok(dedupe_by_id(refs))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        stream_for(Harness::Gemini, r, opts, sink)
    }
}

/// Keep one [`SessionRef`] per session id across roots (a sandboxed and an unsandboxed run of the
/// same project could, in principle, both hold a copy): the most recently updated wins, and the
/// input order is otherwise preserved.
fn dedupe_by_id(refs: Vec<SessionRef>) -> Vec<SessionRef> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<SessionRef> = Vec::with_capacity(refs.len());
    for r in refs {
        match seen.get(&r.id) {
            Some(&i) => {
                if r.updated_at > out[i].updated_at {
                    out[i] = r;
                }
            }
            None => {
                seen.insert(r.id.clone(), out.len());
                out.push(r);
            }
        }
    }
    out
}

/// The project cwd a Gemini-format file belongs to, from gemini-cli's project registry.
///
/// A recording/checkpoint/log lives at `<runtime>/tmp/<projectIdentifier>/…`. The identifier is a
/// short id (`claurdvoyant`) or, for pre-registry installs, the sha256 of the project path. The
/// registry `<runtime>/projects.json` maps `{"/abs/path": "short-id"}`
/// (`config/projectRegistry.ts`), and `<runtime>/history/<id>/.project_root` holds the path too
/// (the ownership marker gemini-cli uses to self-heal a lost registry). Hash-named dirs have no
/// marker, so they stay `None` — exactly as before. Registry reads are cached per runtime root.
/// gemini-cli's own rule for user content that is NOT a prompt (`utils/sessionUtils.ts:97-105`
/// `isIgnoredUserContent`): empty, a slash command, a `?` query, or the injected `<session_context>`
/// / `<hook_context>` preamble the CLI writes as the first "user" message of every session. Such a
/// turn is kept in the transcript (it is in the record) but must not become the session title —
/// verified on 0.46.0, where a fresh session's only user turn was the `<session_context>` block.
fn is_ignored_user_content(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with('/')
        || t.starts_with('?')
        || t.starts_with("<session_context>")
        || t.starts_with("<hook_context>")
}

/// Type a `user`-typed turn by its text: a prompt the human typed stays a [`Role::User`]
/// [`MessageKind::Prompt`]; context gemini-cli itself fed the model and skips when rebuilding
/// history (`<session_context>` preambles, `<hook_context>` hook output, `/`-command and `?` echoes —
/// `isIgnoredUserContent`, `utils/sessionUtils.ts:97`) is a [`Role::System`]
/// [`MessageKind::InjectedContext`] turn (origin [`Origin::Hook`] for hook output), the same shape
/// the Claude adapter gives its attachments, so titles and first-prompt previews never see it.
fn type_user_turn(m: &mut Message) {
    let Some(text) = m.text() else { return };
    let t = text.trim_start();
    let (kind, origin) = if t.starts_with("<hook_context>") {
        (MessageKind::InjectedContext, Origin::Hook)
    } else if !t.is_empty() && is_ignored_user_content(t) {
        (MessageKind::InjectedContext, Origin::Harness)
    } else {
        (MessageKind::Prompt, Origin::Human)
    };
    if kind == MessageKind::InjectedContext {
        m.role = Role::System;
    }
    m.kind = kind;
    m.origin = origin;
}

/// The parent session of a sub-agent recording, from its path: gemini-cli writes sub-agent
/// recordings under `chats/<parentId>/<id>.jsonl` (`chatRecordingService.ts`), a level below the
/// parent's own `chats/session-*.jsonl`.
fn parent_session_from_path(path: Option<&Path>) -> Option<String> {
    let dir = path?.parent()?;
    (dir.parent()?.file_name()? == "chats")
        .then(|| dir.file_name()?.to_str().map(str::to_string))
        .flatten()
}

fn project_cwd(path: &Path) -> Option<PathBuf> {
    let comps: Vec<&std::ffi::OsStr> = path.components().map(|c| c.as_os_str()).collect();
    let tmp_at = comps.iter().position(|c| *c == "tmp")?;
    let identifier = comps.get(tmp_at + 1)?.to_str()?;
    // `<runtime>` is the parent of `tmp`: rebuild it from the leading components.
    let runtime: PathBuf = comps[..tmp_at].iter().collect();
    if runtime.as_os_str().is_empty() {
        return None;
    }
    registry_for(&runtime).get(identifier).cloned()
}

/// `short-id → project path`, one map per runtime root.
type Registry = std::sync::Arc<std::collections::HashMap<String, PathBuf>>;

/// The [`Registry`] for one runtime root, read once per process.
fn registry_for(runtime: &Path) -> Registry {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Registry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(runtime).cloned()) {
        return hit;
    }
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    // The `.project_root` markers first (`tmp/<id>/` — the dir the chats live in — then
    // `history/<id>/`), so a (newer) registry entry overrides them below. The marker is the bare
    // absolute path, no trailing newline (0.46.0), but trim defensively.
    for base in ["tmp", "history"] {
        if let Ok(entries) = fs::read_dir(runtime.join(base)) {
            for e in entries.flatten() {
                if let Ok(root) = fs::read_to_string(e.path().join(".project_root")) {
                    let root = root.trim();
                    if !root.is_empty() {
                        map.insert(e.file_name().to_string_lossy().into_owned(), PathBuf::from(root));
                    }
                }
            }
        }
    }
    if let Ok(text) = fs::read_to_string(runtime.join("projects.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(projects) = v.get("projects").and_then(Value::as_object) {
                for (abs_path, id) in projects {
                    if let Some(id) = id.as_str() {
                        map.insert(id.to_string(), PathBuf::from(abs_path));
                    }
                }
            }
        }
    }
    let arc = Arc::new(map);
    if let Ok(mut c) = cache.lock() {
        c.insert(runtime.to_path_buf(), arc.clone());
    }
    arc
}

/// Discovery scan of one Gemini-format file into 0..n [`SessionRef`]s, tagged as `harness`.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter (identical on-disk format). Notably,
/// checkpoints derive their id from the `checkpoint-<tag>` *filename* here — the same id
/// [`stream_for`]'s bounded path produces on parse — so discover/parse/stream all agree.
pub(crate) fn scan_session_file(path: &Path, harness: Harness) -> Vec<SessionRef> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == "logs.json" {
        if let Ok(text) = fs::read_to_string(path) {
            return parse_logs_str(&text, Some(path.to_path_buf()))
                .iter()
                .map(|s| session_ref(s, path, harness))
                .collect();
        }
    } else if is_chat_recording(path) {
        if let Ok(text) = fs::read_to_string(path) {
            if let Some(s) = parse_chat_recording(&text, harness, Some(path.to_path_buf())) {
                return vec![session_ref(&s, path, harness)];
            }
        }
    } else if is_checkpoint(&name) {
        if let Ok(text) = fs::read_to_string(path) {
            if let Some(s) = parse_checkpoint(&text, &name, Some(path.to_path_buf())) {
                return vec![session_ref(&s, path, harness)];
            }
        }
    }
    Vec::new()
}

/// Streaming parse of any Gemini-format session file, tagged as `harness` in the returned session.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter — Qwen Code is a gemini-cli fork with an
/// **identical** on-disk format rooted at `~/.qwen`, so it reuses this machinery and only re-tags.
pub(crate) fn stream_for(
    harness: Harness,
    r: &SessionRef,
    _opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    // Gemini's per-message `extra` is a few small facts (`extra[<harness>]["record_type"]` on
    // notices, `rewind_to` on a branch marker); there's no fat sidecar to gate, so `opts` doesn't
    // change the materialization here.
    let name = r
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Chat recording (legacy whole-file .json / modern append-only .jsonl): one session per
    // file, id authoritative. These are the only shapes large enough to OOM, so they get
    // the native streaming paths; the bounded shapes below stay on the materializing bridge.
    if is_chat_recording(&r.path) {
        let ext = r.path.extension().and_then(|e| e.to_str());
        if ext == Some("jsonl") {
            if let Some(mut s) = stream_jsonl_recording(&r.path, harness, sink)? {
                s.harness = harness;
                return Ok(s);
            }
        } else {
            // Legacy `.json`: a single JSON document. Memory-map it and iterate the `messages`
            // array as borrowed `RawValue`s so the whole-document `Value` is never built — peak
            // is one message + the (reclaimable) mapping. If the file isn't actually the
            // single-document shape (e.g. a `.json`-extensioned JSONL recording), fall through.
            if let Some(mut s) = stream_legacy_json_recording(&r.path, harness, sink)? {
                s.harness = harness;
                return Ok(s);
            }
            if let Some(mut s) = stream_jsonl_recording(&r.path, harness, sink)? {
                s.harness = harness;
                return Ok(s);
            }
        }
    }

    // Bounded shapes (checkpoints, logs.json): a few KB–MB, never the OOM risk. Bridge through a
    // whole-Session parse and replay its messages into the sink.
    let mut session = parse_bounded(r, &name, harness)?;
    session.harness = harness;
    let messages = std::mem::take(&mut session.messages);
    sink.meta(&session);
    for m in messages {
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    Ok(session)
}

/// Whole-`Session` parse for the bounded (non-streamed) shapes: gemini-cli checkpoints and
/// `logs.json` (many sessions per file — picks the one matching `r.id`). These are small enough
/// to materialize. Also handles the rare case where a chat-recording path didn't stream.
fn parse_bounded(r: &SessionRef, name: &str, harness: Harness) -> Result<Session> {
    let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;

    // A chat recording that the streaming paths couldn't handle (shouldn't normally happen) —
    // fall back to the pure parser so we never silently lose a session.
    if is_chat_recording(&r.path) {
        if let Some(s) = parse_chat_recording(&text, harness, Some(r.path.clone())) {
            return Ok(s);
        }
    }
    // Checkpoint: one session per file.
    if is_checkpoint(name) {
        if let Some(s) = parse_checkpoint(&text, name, Some(r.path.clone())) {
            return Ok(s);
        }
    }
    // logs.json: many sessions per file — pick out the one matching r.id.
    let sessions = parse_logs_str(&text, Some(r.path.clone()));
    if let Some(s) = sessions.into_iter().find(|s| s.id == r.id) {
        return Ok(s);
    }

    anyhow::bail!(
        "could not parse Gemini-format session {} from {}",
        r.id,
        r.path.display()
    )
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// A chat-recording file: `chats/session-*.json` or `chats/session-*.jsonl`, or a subagent recording
/// `chats/<parentId>/<id>.jsonl`. We key off the `chats/` directory in the path.
fn is_chat_recording(path: &Path) -> bool {
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    if !in_chats {
        return false;
    }
    matches!(path.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl"))
}

fn is_checkpoint(name: &str) -> bool {
    name.starts_with("checkpoint-") && name.ends_with(".json")
}

// ---------------------------------------------------------------------------
// Streaming chat recordings — the only Gemini shapes large enough to OOM.
// ---------------------------------------------------------------------------

/// Stream a *legacy whole-file* chat recording (`{sessionId, messages:[…]}` as one JSON document)
/// into `sink` without ever building the whole-document `Value`.
///
/// The file is memory-mapped (behind the `mmap` feature; a `fs::read` fallback otherwise) and a thin
/// wrapper is deserialized that keeps each element of `messages` as a borrowed
/// [`RawValue`](serde_json::value::RawValue). We then parse one raw item at a time into a [`Value`],
/// run it through [`emit_record_message`], and drop it before the next — so peak memory is one
/// message plus the (reclaimable) mapping, not the multi-GB document.
///
/// Returns `Ok(None)` if the bytes aren't the single-document shape (e.g. a `.json`-named JSONL log),
/// so the caller can fall back to the JSONL path.
fn stream_legacy_json_recording(path: &Path, harness: Harness, sink: &mut dyn MessageSink) -> Result<Option<Session>> {
    use serde::Deserialize;
    use serde_json::value::RawValue;

    /// Borrows just the metadata fields [`record_metadata`] reads, plus the message array as raw
    /// (unparsed) items. `#[serde(borrow)]` keeps everything pointing into the mapping.
    #[derive(Deserialize)]
    struct Wrapper<'a> {
        #[serde(rename = "sessionId", borrow, default)]
        session_id: Option<&'a str>,
        #[serde(rename = "startTime", borrow, default)]
        start_time: Option<&'a str>,
        #[serde(rename = "lastUpdated", borrow, default)]
        last_updated: Option<&'a str>,
        #[serde(borrow, default)]
        summary: Option<&'a str>,
        #[serde(borrow, default)]
        directories: Option<Vec<&'a str>>,
        #[serde(borrow, default)]
        messages: Option<Vec<&'a RawValue>>,
    }

    let bytes = map_file(path)?;
    // A legacy recording is a single JSON object with both `sessionId` and `messages`. If this parse
    // fails, or those keys are absent, it's not the whole-file shape — let the caller try JSONL.
    let Ok(w) = serde_json::from_slice::<Wrapper>(&bytes) else {
        return Ok(None);
    };
    let (Some(_sid), Some(raw_msgs)) = (w.session_id, &w.messages) else {
        return Ok(None);
    };

    // Rebuild a tiny metadata map (only the fields record_metadata consults) — never the document.
    let mut meta = serde_json::Map::new();
    if let Some(v) = w.session_id {
        meta.insert("sessionId".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.start_time {
        meta.insert("startTime".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.last_updated {
        meta.insert("lastUpdated".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.summary {
        meta.insert("summary".into(), Value::String(v.to_string()));
    }
    if let Some(dirs) = &w.directories {
        meta.insert(
            "directories".into(),
            Value::Array(dirs.iter().map(|d| Value::String(d.to_string())).collect()),
        );
    }

    let mut session = record_metadata(&meta, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut session_model: Option<String> = None;
    let mut title: Option<String> = None;

    for item in raw_msgs {
        // Parse just this one message; it (and its owned strings) drop at the end of the iteration.
        let Ok(rm) = serde_json::from_str::<Value>(item.get()) else {
            continue;
        };
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut session_model, &mut title, sink) == Flow::Stop {
            break;
        }
    }

    session.model = session_model;
    session.title = summary.or(title);
    Ok(Some(session))
}

/// Memory-map `path` (feature `mmap`) or read it into a `Vec<u8>` (fallback). Returns an opaque
/// byte container that derefs to `[u8]` either way, so callers don't branch on the feature.
fn map_file(path: &Path) -> Result<FileBytes> {
    #[cfg(feature = "mmap")]
    {
        let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Safety: cv reads its own on-disk transcripts; we treat the mapping as immutable bytes and
        // never alias it mutably. A concurrent external truncation is the documented mmap caveat and
        // not a case cv creates.
        let mmap = unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmapping {}", path.display()))?;
        Ok(FileBytes::Mapped(mmap))
    }
    #[cfg(not(feature = "mmap"))]
    {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(FileBytes::Owned(bytes))
    }
}

/// Byte container abstracting over an mmap (feature `mmap`) and an owned read (fallback).
enum FileBytes {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    #[cfg(not(feature = "mmap"))]
    Owned(Vec<u8>),
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            FileBytes::Mapped(m) => m,
            #[cfg(not(feature = "mmap"))]
            FileBytes::Owned(v) => v,
        }
    }
}

/// Stream a *modern* append-only JSONL chat recording into `sink` without `read_to_string`.
///
/// Reads line-by-line so no whole-document `Value` is built. The format's `$set` / `$rewindTo`
/// control records mean a later line can replace or truncate earlier messages, so we still buffer the
/// retained *raw* message records (one small [`Value`] each, superseded ones dropped on rewind) — but
/// we never reassemble them into one giant `messages` array and never hold that array alongside the
/// IR `Vec`. Peak is O(retained raw messages), which the rewind semantics make unavoidable, rather
/// than O(document) × 2.
///
/// Returns `Ok(None)` if the file is not a recording (no metadata line and no message records), so
/// the caller can fall back.
fn stream_jsonl_recording(path: &Path, harness: Harness, sink: &mut dyn MessageSink) -> Result<Option<Session>> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut replay = RecordingReplay::default();
    super::for_each_json_line(BufReader::new(file), |rec| {
        replay.ingest(rec);
        Flow::Continue
    });
    if !replay.saw_meta && replay.msgs.is_empty() {
        return Ok(None);
    }

    let mut session = record_metadata(&replay.metadata, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut session_model: Option<String> = None;
    let mut title: Option<String> = None;

    // Emit retained messages in order, dropping each raw Value as we go (so we never hold the
    // reassembled array and the IR messages at the same time).
    for id in &replay.order {
        let Some(rm) = replay.msgs.remove(id) else { continue };
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut session_model, &mut title, sink) == Flow::Stop {
            break;
        }
    }

    session.model = session_model;
    session.title = summary.or(title);
    Ok(Some(session))
}

/// Replays a *modern* append-only JSONL recording's records into its effective state: the metadata
/// map plus the retained raw message records (insertion-ordered, with `$set` replacement and
/// `$rewindTo` truncation applied). Shared by the streaming [`stream_jsonl_recording`] and the pure
/// [`parse_chat_recording`] so both apply identical control-record semantics.
#[derive(Default)]
struct RecordingReplay {
    metadata: serde_json::Map<String, Value>,
    /// Insertion order of message ids (replacement keeps the original position).
    order: Vec<String>,
    msgs: BTreeMap<String, Value>,
    /// Whether a real metadata line (sessionId + projectHash) was seen.
    saw_meta: bool,
    /// How many `$rewindTo` records were replayed (names the synthetic Branch markers).
    rewinds: usize,
}

impl RecordingReplay {
    /// Fold one JSONL record into the replay state. Non-object junk is ignored.
    fn ingest(&mut self, rec: Value) {
        if !rec.is_object() {
            return;
        }
        if let Some(rewind) = rec.get("$rewindTo").and_then(Value::as_str) {
            // Drop the rewind target and everything after it.
            if let Some(pos) = self.order.iter().position(|id| id == rewind) {
                for id in self.order.drain(pos..) {
                    self.msgs.remove(&id);
                }
            } else {
                self.order.clear();
                self.msgs.clear();
            }
            // Leave a marker where the cut happened; `emit_record_message` turns it into a
            // `MessageKind::Branch` turn (it is not a real message record: no `id` collision).
            self.rewinds += 1;
            let marker_id = format!("$rewind-{}", self.rewinds);
            self.order.push(marker_id.clone());
            self.msgs.insert(
                marker_id.clone(),
                serde_json::json!({ "id": marker_id, "type": "rewind", "rewindTo": rewind }),
            );
            return;
        }
        if let Some(set) = rec.get("$set").and_then(Value::as_object) {
            for (k, v) in set {
                if k == "messages" {
                    // Checkpoint: rebuild the whole message list.
                    self.order.clear();
                    self.msgs.clear();
                    if let Some(arr) = v.as_array() {
                        for m in arr {
                            if let Some(id) = m.get("id").and_then(Value::as_str) {
                                if !self.msgs.contains_key(id) {
                                    self.order.push(id.to_string());
                                }
                                self.msgs.insert(id.to_string(), m.clone());
                            }
                        }
                    }
                } else {
                    self.metadata.insert(k.clone(), v.clone());
                }
            }
            return;
        }
        if let Some(id) = rec.get("id").and_then(Value::as_str).map(str::to_string) {
            // A message record (must have an id and a type/content); a junk record with an id but
            // no message shape is ignored either way.
            if rec.get("type").is_some() || rec.get("content").is_some() {
                if !self.msgs.contains_key(&id) {
                    self.order.push(id.clone());
                }
                self.msgs.insert(id, rec);
            }
            return;
        }
        // Metadata line(s): a real recording's metadata carries BOTH sessionId and projectHash
        // (gemini-cli's `isPartialMetadataRecord`). This guards against mistaking a `logs.json`
        // entry (sessionId + messageId + message, no projectHash) for a recording.
        if rec.get("sessionId").is_some() && rec.get("projectHash").is_some() {
            self.saw_meta = true;
            if let Value::Object(obj) = rec {
                for (k, v) in obj {
                    self.metadata.insert(k, v);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure entry point (WASM-safe): handles all three readable JSON/JSONL shapes.
// ---------------------------------------------------------------------------

/// Parse any readable Gemini session text into IR [`Session`]s, with no filesystem access.
///
/// Sniffs the shape:
/// - a chat-recording `ConversationRecord` (`.json` object with `messages[]`, or `.jsonl` with a
///   `sessionId`+`projectHash` metadata line),
/// - a gemini-cli checkpoint (`{history:[…]}` or a bare `Content[]` array),
/// - or the `logs.json` array of `{sessionId,messageId,message,…}` entries (possibly several sessions).
pub fn parse_all_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    parse_all_str_for(Harness::Gemini, text, source_path)
}

/// [`parse_all_str`] tagged as `harness` — Qwen Code shares the format, so its harness-specific
/// facts (`extra["qwen"]`) and the session tag must say "qwen", not "gemini".
pub fn parse_all_str_for(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let mut out = parse_all_str_inner(harness, text, source_path);
    for s in &mut out {
        s.harness = harness;
    }
    out
}

fn parse_all_str_inner(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    // Chat recording? (object with sessionId+messages, or jsonl with a metadata line)
    if let Some(s) = parse_chat_recording(text, harness, source_path.clone()) {
        return vec![s];
    }

    let trimmed = text.trim_start();
    // Checkpoint object `{history:[…]}`.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("history") {
                if let Some(s) = parse_checkpoint(text, "", source_path.clone()) {
                    return vec![s];
                }
            }
        }
    }
    // Array: either a logs.json (entries have messageId/message) or a bare checkpoint Content[].
    if trimmed.starts_with('[') {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
            let looks_logs = items
                .iter()
                .take(8)
                .any(|it| it.get("messageId").is_some() && it.get("message").is_some());
            if looks_logs {
                return parse_logs_str(text, source_path);
            }
            let looks_checkpoint = items
                .iter()
                .take(8)
                .any(|it| it.get("role").is_some() && it.get("parts").is_some());
            if looks_checkpoint {
                if let Some(s) = parse_checkpoint(text, "", source_path) {
                    return vec![s];
                }
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// logs.json
// ---------------------------------------------------------------------------

/// Parse a Gemini `logs.json` from its text contents into one [`Session`] per distinct `sessionId`.
fn parse_logs_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let entries: Vec<Value> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut by_session: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for e in &entries {
        let sid = e.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        by_session.entry(sid).or_default().push(e);
    }

    let mut out = Vec::new();
    for (sid, mut items) in by_session {
        if sid.is_empty() {
            continue;
        }
        items.sort_by_key(|e| e.get("messageId").and_then(Value::as_i64).unwrap_or(0));

        let title = items
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("user"))
            .filter_map(|e| e.get("message").and_then(Value::as_str))
            .find(|t| !is_ignored_user_content(t))
            .map(|t| crate::ir::truncate(t, 80));
        let times: Vec<DateTime<Utc>> = items
            .iter()
            .filter_map(|e| e.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
            .collect();

        let mut s = Session {
            id: sid,
            harness: Harness::Gemini,
            cwd: source_path.as_deref().and_then(project_cwd),
            title,
            created_at: times.iter().min().copied(),
            updated_at: times.iter().max().copied(),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: source_path.clone(),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };
        for e in items {
            let role = match e.get("type").and_then(Value::as_str) {
                Some("assistant") | Some("model") | Some("gemini") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            let text = e.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }
            let mut m = Message::new(role);
            m.timestamp = e.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            m.content.push(Block::Text { text: text.into() });
            if role == Role::User {
                type_user_turn(&mut m);
            }
            s.messages.push(m);
        }
        out.push(s);
    }
    out
}

// ---------------------------------------------------------------------------
// Chat recordings (ConversationRecord) — legacy whole-file .json or modern .jsonl
// ---------------------------------------------------------------------------

/// Parse a gemini-cli chat recording. Handles both the legacy whole-file JSON object
/// (`{sessionId, messages:[…]}`) and the modern append-only JSONL log (metadata line + message
/// records + `$set` / `$rewindTo` control records). Returns `None` if the text is not a recording.
fn parse_chat_recording(text: &str, harness: Harness, source_path: Option<PathBuf>) -> Option<Session> {
    let trimmed = text.trim_start();

    // Legacy: a single JSON object spanning the whole file.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("sessionId") && map.contains_key("messages") {
                return Some(record_to_session(&map, harness, source_path));
            }
            // A modern recording happens to also start with `{` on its metadata first line,
            // but a *whole-file* parse of multi-line JSONL fails, so we fall through to JSONL below.
        }
    }

    // Modern JSONL: replay metadata + message records + control records.
    let mut replay = RecordingReplay::default();
    super::for_each_json_line_str(text, |rec| {
        replay.ingest(rec);
        Flow::Continue
    });

    if !replay.saw_meta && replay.msgs.is_empty() {
        return None;
    }

    // Reassemble a ConversationRecord-shaped map.
    let mut metadata = replay.metadata;
    let messages: Vec<Value> = replay.order.iter().filter_map(|id| replay.msgs.remove(id)).collect();
    metadata.insert("messages".into(), Value::Array(messages));
    Some(record_to_session(&metadata, harness, source_path))
}

/// Build the IR [`Session`] *metadata* (everything but `messages`) from a `ConversationRecord`'s
/// top-level fields. `title` is the record's `summary` when present; the per-message scan fills in a
/// first-user-turn fallback and the session `model`. `messages` is left empty — the caller streams
/// them via [`emit_record_message`].
fn record_metadata(rec: &serde_json::Map<String, Value>, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let id = rec.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let created_at = rec.get("startTime").and_then(Value::as_str).and_then(parse_ts);
    let updated_at = rec.get("lastUpdated").and_then(Value::as_str).and_then(parse_ts);
    let title = rec.get("summary").and_then(Value::as_str).map(str::to_string);

    // cwd: `directories[]` (added via /dir) may carry real absolute paths — use the first; else
    // recover the project path from gemini-cli's registry via the file's `tmp/<id>/` location.
    let cwd = rec
        .get("directories")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| source_path.as_deref().and_then(project_cwd));

    // A sub-agent recording (`kind: "subagent"`, stored under `chats/<parentId>/`) points at its
    // parent; the recording's own `kind` is a Gemini fact.
    let lineage = Lineage {
        parent: parent_session_from_path(source_path.as_deref()),
        ..Default::default()
    };
    let mut extra = serde_json::Map::new();
    if let Some(kind) = rec.get("kind").and_then(Value::as_str) {
        let mut bag = serde_json::Map::new();
        bag.insert("kind".into(), Value::String(kind.to_string()));
        extra.insert(harness.as_str().into(), Value::Object(bag));
    }

    Session {
        id,
        harness,
        cwd,
        title,
        created_at,
        updated_at,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
        extra,
        system_prompt: None,
        lineage,
    }
}

/// Translate one raw `ConversationRecord` message (`obj`) into 0..2 IR [`Message`]s and hand them to
/// `sink` (an assistant turn may also emit a trailing [`Role::Tool`] turn for its tool results).
///
/// Threads two pieces of cross-message state: `session_model` (first assistant `model` seen, becomes
/// `Session::model`) and `title` (first user turn's text, the fallback title when the record has no
/// `summary`). Returns the sink's [`Flow`] so a streaming caller can stop early.
fn emit_record_message(
    harness: Harness,
    obj: &serde_json::Map<String, Value>,
    session_model: &mut Option<String>,
    title: &mut Option<String>,
    sink: &mut dyn MessageSink,
) -> Flow {
    let mty = obj.get("type").and_then(Value::as_str).unwrap_or("user");
    let ts = obj.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let id = obj.get("id").and_then(Value::as_str).map(str::to_string);

    match mty {
        "gemini" | "model" | "assistant" => {
            let mut m = Message::new(Role::Assistant);
            m.id = id;
            m.timestamp = ts;
            // The first model seen becomes `Session::model`; a turn records its own only when it
            // differs (`Message::model` is the exception, not the rule — IR diet).
            if let Some(model) = obj.get("model").and_then(Value::as_str) {
                match session_model.as_deref() {
                    None => *session_model = Some(model.to_string()),
                    Some(m0) if m0 != model => m.model = Some(model.to_string()),
                    _ => {}
                }
            }
            m.usage = obj.get("tokens").and_then(parse_tokens);

            // Thoughts → Thinking blocks (subject + description).
            if let Some(thoughts) = obj.get("thoughts").and_then(Value::as_array) {
                for t in thoughts {
                    let subj = t.get("subject").and_then(Value::as_str).unwrap_or("");
                    let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                    let text = if subj.is_empty() {
                        desc.to_string()
                    } else if desc.is_empty() {
                        subj.to_string()
                    } else {
                        format!("**{subj}** {desc}")
                    };
                    if !text.is_empty() {
                        m.content.push(Block::Thinking {
                            text: text.into(),
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }

            // Assistant content: string, or array of Parts (text / inlineData / functionCall).
            push_content_blocks(&mut m.content, obj.get("content"));

            // Tool calls (gemini-cli ToolCallRecord) → ToolUse + paired ToolResult.
            let mut tool_results: Vec<Block> = Vec::new();
            if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
                for c in calls {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let call_id = c
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{name}-call"));
                    let input = c.get("args").cloned().unwrap_or(Value::Null);
                    m.content.push(Block::ToolUse {
                        id: call_id.clone(),
                        name: name.to_string(),
                        input,
                        namespace: None,
                    });
                    let status = c.get("status").and_then(Value::as_str);
                    let is_error = status.map(|s| s.eq_ignore_ascii_case("error")).unwrap_or(false);
                    let content = tool_result_text(c.get("result"))
                        .or_else(|| c.get("resultDisplay").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_default();
                    if !content.is_empty() {
                        tool_results.push(Block::ToolResult {
                            tool_use_id: call_id,
                            content: content.into(),
                            is_error,
                            tool_name: Some(name.to_string()),
                            status: status.map(str::to_string),
                            details: None,
                        });
                    }
                }
            }

            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
            // Emit tool results as a separate Tool-role turn (mirrors how Gemini feeds
            // functionResponses back as a user turn, but kept distinct in the IR).
            if !tool_results.is_empty() {
                let mut tm = Message::new(Role::Tool);
                tm.timestamp = ts;
                tm.content = tool_results;
                if sink.message(tm) == Flow::Stop {
                    return Flow::Stop;
                }
            }
        }
        "info" | "error" | "warning" => {
            // Harness notices (`info`/`warning`) and errors the model never answered (`error`):
            // System turns, searchable, typed by kind; the record type is a Gemini fact.
            let kind = if mty == "error" {
                MessageKind::Error
            } else {
                MessageKind::Notice
            };
            let mut m = Message::of_kind(Role::System, kind, Origin::Harness);
            m.id = id;
            m.timestamp = ts;
            m.harness_extra_mut(harness)
                .insert("record_type".into(), Value::String(mty.to_string()));
            push_content_blocks(&mut m.content, obj.get("content"));
            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
        }
        "rewind" => {
            // A `$rewindTo` control record (see `RecordingReplay::ingest`): what follows does not
            // continue what precedes. Kept as a Branch marker so a reader sees the cut; it is not a
            // message record, so it carries no id or timestamp of its own.
            let target = obj.get("rewindTo").and_then(Value::as_str).unwrap_or("");
            let mut m = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
            m.content.push(Block::Text {
                text: format!("[rewound to {target}]").into(),
            });
            let bag = m.harness_extra_mut(harness);
            bag.insert("record_type".into(), Value::String("rewind".into()));
            bag.insert("rewind_to".into(), Value::String(target.to_string()));
            if sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
        }
        _ => {
            // user (and anything else) → a human prompt, or context gemini-cli injected.
            let mut m = Message::new(Role::User);
            m.id = id;
            m.timestamp = ts;
            push_content_blocks(&mut m.content, obj.get("content"));
            type_user_turn(&mut m);
            if title.is_none() {
                *title = m
                    .text()
                    .filter(|t| !is_ignored_user_content(t))
                    .map(|t| crate::ir::truncate(&t, 80));
            }
            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
        }
    }
    Flow::Continue
}

/// Map a `ConversationRecord` (as a JSON object) onto an IR [`Session`] — the whole-`Session`
/// convenience used by the pure (no-filesystem) [`parse_all_str`] / [`parse_chat_recording`] paths.
/// Builds the metadata via [`record_metadata`] and collects messages via [`emit_record_message`].
///
/// `summary` (when present) is the title and overrides the first-user-turn fallback that the message
/// scan fills in — same precedence as before this was split for streaming.
fn record_to_session(rec: &serde_json::Map<String, Value>, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let mut session = record_metadata(rec, harness, source_path);
    let summary = session.title.take();

    let empty = vec![];
    let raw_msgs = rec.get("messages").and_then(Value::as_array).unwrap_or(&empty);

    let mut sink = CollectSink::default();
    let mut session_model: Option<String> = None;
    let mut title: Option<String> = None;
    for rm in raw_msgs {
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut session_model, &mut title, &mut sink) == Flow::Stop {
            break;
        }
    }

    session.model = session_model;
    session.title = summary.or(title);
    session.messages = sink.messages;
    session
}

/// Append IR blocks for a Gemini `content` value, which is a `PartListUnion`: a bare string, a single
/// Part object, or an array of Parts. Each Part may carry `text`, `inlineData`, `functionCall`, or
/// `functionResponse`.
fn push_content_blocks(out: &mut Vec<Block>, content: Option<&Value>) {
    let Some(content) = content else { return };
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(Block::Text { text: s.clone().into() });
            }
        }
        Value::Array(parts) => {
            for p in parts {
                push_part(out, p);
            }
        }
        Value::Object(_) => push_part(out, content),
        _ => {}
    }
}

fn push_part(out: &mut Vec<Block>, part: &Value) {
    if let Some(s) = part.as_str() {
        if !s.is_empty() {
            out.push(Block::Text {
                text: s.to_string().into(),
            });
        }
        return;
    }
    let Some(obj) = part.as_object() else { return };

    if let Some(fc) = obj.get("functionCall").and_then(Value::as_object) {
        let name = fc.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fc
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        out.push(Block::ToolUse {
            id,
            name: name.to_string(),
            input: fc.get("args").cloned().unwrap_or(Value::Null),
            namespace: None,
        });
        return;
    }
    if let Some(fr) = obj.get("functionResponse").and_then(Value::as_object) {
        let name = fr.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fr
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        let content = fr.get("response").map(value_to_text).unwrap_or_default();
        out.push(Block::ToolResult {
            tool_use_id: id,
            content: content.into(),
            is_error: false,
            tool_name: Some(name.to_string()),
            status: None,
            details: None,
        });
        return;
    }
    if let Some(inline) = obj.get("inlineData").and_then(Value::as_object) {
        out.push(Block::Image {
            media_type: inline.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: None, // we don't inline base64 bytes into the IR
        });
        return;
    }
    if let Some(fd) = obj.get("fileData").and_then(Value::as_object) {
        out.push(Block::File {
            mime: fd.get("mimeType").and_then(Value::as_str).map(str::to_string),
            path: None,
            source: fd.get("fileUri").and_then(Value::as_str).map(str::to_string),
        });
        return;
    }
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        let is_thought = obj.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if text.is_empty() {
            return;
        }
        if is_thought {
            out.push(Block::Thinking {
                text: text.to_string().into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
        } else {
            out.push(Block::Text {
                text: text.to_string().into(),
            });
        }
    }
}

/// Extract human-readable text from a `toolCall.result` (a `PartListUnion`, typically a list of
/// `{functionResponse:{response:{output}}}` parts).
fn tool_result_text(result: Option<&Value>) -> Option<String> {
    let result = result?;
    match result {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                let chunk = if let Some(fr) = p.get("functionResponse") {
                    fr.get("response").map(value_to_text).unwrap_or_default()
                } else if let Some(t) = p.get("text").and_then(Value::as_str) {
                    t.to_string()
                } else {
                    value_to_text(p)
                };
                if !chunk.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&chunk);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Value::Object(_) => {
            if let Some(fr) = result.get("functionResponse") {
                Some(fr.get("response").map(value_to_text).unwrap_or_default())
            } else {
                Some(value_to_text(result))
            }
        }
        _ => None,
    }
}

/// Turn an arbitrary JSON value into display text, unwrapping the common `{output: "…"}` wrapper.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(Value::String(o)) = map.get("output") {
                o.clone()
            } else if let Some(Value::String(e)) = map.get("error") {
                e.clone()
            } else {
                v.to_string()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_tokens(v: &Value) -> Option<Usage> {
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(Value::as_u64);
    Some(Usage {
        input_tokens: get("input"),
        output_tokens: get("output"),
        cache_read_tokens: get("cached"),
        cache_creation_tokens: None,
        reasoning_tokens: None,
        cost_usd: None,
    })
}

// ---------------------------------------------------------------------------
// gemini-cli checkpoints — `{history: Content[]}` or bare `Content[]`
// ---------------------------------------------------------------------------

/// Parse a gemini-cli checkpoint into an IR [`Session`]. The id is derived from the `checkpoint-<tag>`
/// filename when available (the `tag` is percent-encoded by gemini-cli; we decode it best-effort).
fn parse_checkpoint(text: &str, file_name: &str, source_path: Option<PathBuf>) -> Option<Session> {
    let v: Value = serde_json::from_str(text).ok()?;
    let history = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(map) => map.get("history").and_then(Value::as_array).cloned()?,
        _ => return None,
    };

    let id = checkpoint_id(file_name);
    let mut messages = Vec::new();
    let mut title: Option<String> = None;

    for content in &history {
        let Some(obj) = content.as_object() else { continue };
        let role = match obj.get("role").and_then(Value::as_str) {
            Some("model") | Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => Role::User, // gemini uses "user" for both prompts and tool responses
        };
        let mut m = Message::new(role);
        if role == Role::System {
            m.kind = MessageKind::SystemPrompt;
        }
        push_content_blocks(&mut m.content, obj.get("parts"));
        // If this "user" turn is purely tool results, retag it as a Tool turn.
        if role == Role::User
            && !m.content.is_empty()
            && m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }))
        {
            m.role = Role::Tool;
            m.kind = MessageKind::ToolResult;
            m.origin = Origin::Harness;
        } else if role == Role::User {
            type_user_turn(&mut m);
        }
        if m.role == Role::User && title.is_none() {
            title = m
                .text()
                .filter(|t| !is_ignored_user_content(t))
                .map(|t| crate::ir::truncate(&t, 80));
        }
        if !m.content.is_empty() {
            messages.push(m);
        }
    }

    if messages.is_empty() {
        return None;
    }

    Some(Session {
        id,
        harness: Harness::Gemini,
        cwd: source_path.as_deref().and_then(project_cwd),
        title,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages,
        source_path,
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    })
}

fn checkpoint_id(file_name: &str) -> String {
    let tag = file_name
        .strip_prefix("checkpoint-")
        .and_then(|s| s.strip_suffix(".json"))
        .unwrap_or("");
    if tag.is_empty() {
        return "checkpoint".to_string();
    }
    let decoded = percent_encoding::percent_decode_str(tag)
        .decode_utf8()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| tag.to_string());
    format!("checkpoint-{decoded}")
}

// ---------------------------------------------------------------------------
// SessionRef helpers
// ---------------------------------------------------------------------------

fn session_ref(s: &Session, path: &Path, harness: Harness) -> SessionRef {
    SessionRef {
        id: s.id.clone(),
        harness,
        path: path.to_path_buf(),
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: s.messages.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/gemini/{}", env!("CARGO_MANIFEST_DIR"), name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn parses_logs_json_grouped_by_session() {
        let text = fixture("logs.json");
        let mut sessions = parse_all_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[1].id, "sess-b");
        assert_eq!(sessions[1].harness, Harness::Gemini);
    }

    #[test]
    fn parses_legacy_chat_recording() {
        let text = fixture("session_legacy.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        assert_eq!(s.model.as_deref(), Some("gemini-3-flash-preview"));
        assert_eq!(
            s.title.as_deref(),
            Some("Investigated whether brorb is a useful meditation aid.")
        );
        assert!(s.created_at.is_some() && s.updated_at.is_some());

        // user, gemini(with thinking+text+tooluse), tool(result), gemini(text), info(system)
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
            ]
        );
        assert!(
            s.messages.iter().all(|m| m.model.is_none()),
            "every turn used the session model, so none repeats it"
        );

        let asst = &s.messages[1];
        assert!(asst.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(asst.content.iter().any(|b| matches!(b, Block::Text { .. })));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert!(asst.usage.is_some());

        // tool result carried in the following Tool turn
        let tool = &s.messages[2];
        let got = tool
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("meditation timer")));
        assert!(got, "tool result text should be extracted");

        // info message kept as a System notice, its record type a Gemini fact
        let info = &s.messages[4];
        assert_eq!(info.role, Role::System);
        assert_eq!(info.harness_extra(Harness::Gemini).unwrap()["record_type"], "info");
        assert_nested_extra(s, Harness::Gemini);
    }

    fn kinds(s: &Session) -> Vec<(Role, MessageKind, Origin)> {
        s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect()
    }

    /// Every `extra` key, on the session and on each message, is the harness's own: no flat keys.
    fn assert_nested_extra(s: &Session, h: Harness) {
        for k in s.extra.keys() {
            assert_eq!(k, h.as_str(), "flat session extra key {k:?}");
        }
        for (i, m) in s.messages.iter().enumerate() {
            for k in m.extra.keys() {
                assert_eq!(k, h.as_str(), "flat extra key {k:?} on message {i}");
            }
        }
    }

    #[test]
    fn user_turn_kinds_and_notices() {
        // gemini-cli writes context of its own as `user` records — `isIgnoredUserContent`
        // (`utils/sessionUtils.ts:97`): slash commands, `?` queries, the `<session_context>` preamble,
        // `<hook_context>` hook output — and `info` / `warning` / `error` records.
        let text = concat!(
            r#"{"sessionId":"k1","projectHash":"h","startTime":"2026-03-01T00:00:00.000Z","lastUpdated":"2026-03-01T00:00:09.000Z","kind":"main"}"#,
            "\n",
            r#"{"id":"u1","timestamp":"2026-03-01T00:00:01.000Z","type":"user","content":"<session_context>\nToday is Sunday.\n</session_context>"}"#,
            "\n",
            r#"{"id":"u2","timestamp":"2026-03-01T00:00:02.000Z","type":"user","content":"<hook_context>\nlint: ok\n</hook_context>"}"#,
            "\n",
            r#"{"id":"u3","timestamp":"2026-03-01T00:00:03.000Z","type":"user","content":"/memory show"}"#,
            "\n",
            r#"{"id":"u4","timestamp":"2026-03-01T00:00:04.000Z","type":"user","content":"?what does this do"}"#,
            "\n",
            r#"{"id":"u5","timestamp":"2026-03-01T00:00:05.000Z","type":"user","content":"fix the build"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:06.000Z","type":"gemini","model":"gemini-3-pro","content":"On it."}"#,
            "\n",
            r#"{"id":"i1","timestamp":"2026-03-01T00:00:07.000Z","type":"info","content":"Session compacted."}"#,
            "\n",
            r#"{"id":"w1","timestamp":"2026-03-01T00:00:08.000Z","type":"warning","content":"Rate limited; retrying."}"#,
            "\n",
            r#"{"id":"e1","timestamp":"2026-03-01T00:00:09.000Z","type":"error","content":"429 Resource exhausted"}"#,
            "\n",
        );
        let sessions = parse_all_str(text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::System, MessageKind::InjectedContext, Origin::Hook),
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Error, Origin::Harness),
            ]
        );
        assert_eq!(
            s.title.as_deref(),
            Some("fix the build"),
            "only the human prompt titles"
        );
        assert_eq!(s.first_user_text().as_deref(), Some("fix the build"));
        let record_type = |i: usize| s.messages[i].harness_extra(Harness::Gemini).unwrap()["record_type"].clone();
        assert_eq!(record_type(6), "info");
        assert_eq!(record_type(7), "warning");
        assert_eq!(record_type(8), "error");
        assert!(s.messages[4].extra.is_empty(), "a plain prompt carries no Gemini facts");
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "main");
        assert!(s.lineage.is_empty());
        assert_nested_extra(s, Harness::Gemini);
    }

    #[test]
    fn message_model_only_when_it_differs_from_the_session() {
        let text = concat!(
            r#"{"sessionId":"m1","projectHash":"h","startTime":"2026-03-01T00:00:00.000Z","lastUpdated":"2026-03-01T00:00:03.000Z","kind":"main"}"#,
            "\n",
            r#"{"id":"u1","timestamp":"2026-03-01T00:00:01.000Z","type":"user","content":"hi"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:02.000Z","type":"gemini","model":"gemini-3-pro","content":"hello"}"#,
            "\n",
            r#"{"id":"g2","timestamp":"2026-03-01T00:00:03.000Z","type":"gemini","model":"gemini-3-flash","content":"(fallback) hello"}"#,
            "\n",
        );
        let s = &parse_all_str(text, None)[0];
        assert_eq!(s.model.as_deref(), Some("gemini-3-pro"));
        assert_eq!(s.messages[1].model, None);
        assert_eq!(s.messages[2].model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn parses_modern_jsonl_with_rewind_and_set() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "11112222-3333-4444-5555-666677778888");
        // $set provided a summary used as title
        assert_eq!(s.title.as_deref(), Some("Listed the project files."));

        // The $rewindTo a3 dropped the first a3, then a3 was re-appended with new text.
        let a3s: Vec<&Message> = s.messages.iter().filter(|m| m.id.as_deref() == Some("a3")).collect();
        assert_eq!(a3s.len(), 1, "rewind should have de-duplicated a3");
        assert!(a3s[0]
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("exactly two files"))));
        // The cut itself is visible: one Branch marker, right before the re-appended a3.
        let branches: Vec<usize> = s
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.kind == MessageKind::Branch)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(branches.len(), 1, "{:?}", kinds(s));
        let b = &s.messages[branches[0]];
        assert_eq!((b.role, b.origin), (Role::System, Origin::Harness));
        assert_eq!(b.id, None, "a control record is not a message record");
        assert_eq!(b.harness_extra(Harness::Gemini).unwrap()["rewind_to"], "a3");
        assert_eq!(b.harness_extra(Harness::Gemini).unwrap()["record_type"], "rewind");
        assert_eq!(s.messages[branches[0] + 1].id.as_deref(), Some("a3"));
        assert!(s.messages.iter().all(|m| m.model.is_none()), "one model throughout");
        assert_nested_extra(s, Harness::Gemini);

        // tool call + result present
        assert!(s.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "list_directory"))));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool
            && m.content
                .iter()
                .any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("main.py")))));
    }

    #[test]
    fn parses_checkpoint_history() {
        let text = fixture("checkpoint.json");
        let s = parse_checkpoint(&text, "checkpoint-my%20tag.json", None).expect("checkpoint");
        assert_eq!(s.id, "checkpoint-my tag");
        // user, model(text+thinking+tooluse), tool(result), model(text)
        let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
        let model_turn = &s.messages[1];
        assert!(model_turn.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(model_turn
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "calculator")));
        // the tool turn carries the functionResponse output
        assert!(s.messages[2]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content == "4")));
    }

    #[test]
    fn parses_legacy_array_checkpoint_via_parse_all() {
        let text = fixture("checkpoint_legacy_array.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(s.title.as_deref(), Some("hi"));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_all_str("not json at all", None).is_empty());
        assert!(parse_all_str("{}", None).is_empty());
        assert!(parse_all_str("[]", None).is_empty());
    }

    // ── on-disk streaming paths ──────────────────────────────────────────────
    //
    // `is_chat_recording` keys off a `chats/` path component, so these write each fixture into a
    // unique temp `chats/` dir, then drive the real `Adapter::parse` (= `stream` → `collect`) and the
    // native `Adapter::stream`, asserting both reproduce the pure `parse_all_str` Session byte-for-
    // byte (Session has no `PartialEq`, so we compare its JSON serialization).

    fn chats_fixture(name: &str) -> (PathBuf, PathBuf) {
        let text = fixture(name);
        let dir = std::env::temp_dir().join(format!(
            "cv-gemini-stream-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let chats = dir.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(name);
        fs::write(&file, text).unwrap();
        (dir, file)
    }

    fn as_json(s: &Session) -> Value {
        serde_json::to_value(s).unwrap()
    }

    /// Drive the on-disk `parse` and `stream` for a chat-recording fixture and assert both equal the
    /// pure `parse_all_str` Session. `source_path` differs (on-disk paths carry provenance), so we
    /// blank it before comparing.
    fn assert_stream_matches_pure(name: &str) {
        let (dir, file) = chats_fixture(name);
        let id = parse_all_str(&fixture(name), None)
            .into_iter()
            .next()
            .expect("fixture parses to one session")
            .id;
        let r = SessionRef {
            id: id.clone(),
            harness: Harness::Gemini,
            path: file.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let adapter = Gemini { roots: vec![] };

        // Reference: the pure parser, with source_path set to the on-disk file for an apples-to-
        // apples comparison.
        let reference = parse_all_str(&fixture(name), Some(file.clone()));
        let reference = reference.into_iter().next().unwrap();

        let via_parse = adapter.parse(&r).expect("on-disk parse");
        assert_eq!(
            as_json(&via_parse),
            as_json(&reference),
            "parse() must stay byte-identical to the pure parser for {name}"
        );

        // Native stream → CollectSink must reassemble the same Session.
        let mut sink = CollectSink::default();
        let mut streamed = adapter
            .stream(&r, &ParseOptions::full(), &mut sink)
            .expect("on-disk stream");
        assert!(streamed.messages.is_empty(), "stream returns empty messages");
        streamed.messages = sink.messages;
        assert_eq!(
            as_json(&streamed),
            as_json(&reference),
            "stream() must reassemble the same Session for {name}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn streams_legacy_json_recording_on_disk() {
        // The mmap + RawValue single-document path.
        assert_stream_matches_pure("session_legacy.json");
    }

    #[test]
    fn streams_modern_jsonl_recording_on_disk() {
        // The line-buffered JSONL replay path (with $set / $rewindTo).
        assert_stream_matches_pure("session_modern.jsonl");
    }

    #[test]
    fn stream_honors_stop_early() {
        // A sink returning Stop after the first message halts the legacy-JSON streaming path without
        // emitting the rest.
        let (dir, file) = chats_fixture("session_legacy.json");
        let r = SessionRef {
            id: "9aeb2942-7c46-47b7-aded-13772d4d4e63".into(),
            harness: Harness::Gemini,
            path: file,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let adapter = Gemini { roots: vec![] };
        let mut seen = 0usize;
        let mut sink = |_m: Message| {
            seen += 1;
            Flow::Stop
        };
        let _ = adapter.stream(&r, &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A fake gemini-cli runtime root (`<runtime>/tmp/<id>/chats/<recording>` + registry files).
    fn runtime_fixture(tag: &str, project_id: &str, recording: &str) -> (PathBuf, PathBuf) {
        let runtime = std::env::temp_dir().join(format!(
            "cv-gemini-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let chats = runtime.join("tmp").join(project_id).join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(recording);
        fs::write(&file, fixture(recording)).unwrap();
        (runtime, file)
    }

    #[test]
    fn discovers_both_runtime_roots_and_dedupes_by_id() {
        // `~/.gemini/tmp` and the Seatbelt root `~/.cache/.gemini/tmp` are both scanned; the same
        // session id under both roots is listed once.
        let (rt_a, _) = runtime_fixture("root-a", "proj", "session_modern.jsonl");
        let (rt_b, _) = runtime_fixture("root-b", "proj", "session_modern.jsonl");
        let (rt_c, _) = runtime_fixture("root-c", "proj", "session_legacy.json");
        let adapter = Gemini {
            roots: vec![rt_a.join("tmp"), rt_b.join("tmp"), rt_c.join("tmp")],
        };
        let refs = adapter.discover().unwrap();
        let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "11112222-3333-4444-5555-666677778888",
                "9aeb2942-7c46-47b7-aded-13772d4d4e63"
            ],
            "two distinct sessions across three roots; the duplicate collapsed"
        );
        assert_eq!(adapter.storage_root(), Some(rt_a.join("tmp")));
        for d in [rt_a, rt_b, rt_c] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn cwd_comes_from_the_project_registry() {
        // `projects.json` maps path → short id; `history/<id>/.project_root` is the fallback marker.
        let (runtime, file) = runtime_fixture("registry", "claurdvoyant", "session_modern.jsonl");
        fs::write(
            runtime.join("projects.json"),
            r#"{"projects": {"/Users/u/pug/claurdvoyant": "claurdvoyant"}}"#,
        )
        .unwrap();
        let (runtime2, file2) = runtime_fixture("marker", "gemtest", "session_legacy.json");
        fs::create_dir_all(runtime2.join("history").join("gemtest")).unwrap();
        fs::write(
            runtime2.join("history").join("gemtest").join(".project_root"),
            "/Users/u/cvrt8/gemtest\n",
        )
        .unwrap();
        // hash-named dirs (pre-registry) have no mapping → None, as before
        let (runtime3, file3) = runtime_fixture("hash", &"ab".repeat(32), "session_modern.jsonl");

        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        let refs2 = scan_session_file(&file2, Harness::Gemini);
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/cvrt8/gemtest")));
        let refs3 = scan_session_file(&file3, Harness::Gemini);
        assert_eq!(refs3[0].cwd, None);

        // the streamed session carries it too (the sink's `meta` sees the cwd)
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        // a record's own `directories[]` still wins (unchanged behaviour): covered by
        // `parses_legacy_chat_recording` fixtures carrying no registry at all.
        for d in [runtime, runtime2, runtime3] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn registry_files_in_their_real_0_46_shapes_recover_cwd() {
        // Fixtures mirror what gemini-cli 0.46.0 wrote for a fresh project dir on 2026-09-19:
        // `projects.json` pretty-printed with a nested `projects` map, and an identical bare-path
        // `.project_root` (no trailing newline) in BOTH `tmp/<id>/` and `history/<id>/`.
        let (runtime, file) = runtime_fixture("real-shapes", "gemtest-proj", "session_modern.jsonl");
        fs::write(runtime.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        fs::create_dir_all(runtime.join("history").join("gemtest-proj")).unwrap();
        fs::write(
            runtime.join("history").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));

        // The marker beside the chats is enough on its own (a runtime root with no registry file
        // and no history dir — e.g. one the CLI is still populating).
        let (runtime2, file2) = runtime_fixture("tmp-marker-only", "solo", "session_legacy.json");
        fs::write(
            runtime2.join("tmp").join("solo").join(".project_root"),
            "/Users/u/work/solo",
        )
        .unwrap();
        let refs2 = scan_session_file(&file2, Harness::Gemini);
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/work/solo")));
        for d in [runtime, runtime2] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn real_0_46_recording_keeps_the_context_preamble_but_never_titles_from_it() {
        // Written by gemini-cli 0.46.0 on 2026-09-19 (paths neutralized): a metadata line, then ONE
        // `$set` carrying the whole `messages` array, whose only user turn is the injected
        // `<session_context>` preamble. gemini-cli ignores such content when rebuilding history
        // (`isIgnoredUserContent`), so it must not become the title; the turn itself stays.
        let (runtime, file) = runtime_fixture("real-0-46", "gemtest-proj", "session_0_46_session_context.jsonl");
        fs::write(runtime.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "ad7102ea-0b4f-4822-94f7-f3bbf67f1614");
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));
        assert_eq!(refs[0].title, None, "the <session_context> preamble is not a prompt");

        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(s.title, None);
        assert_eq!(sink.messages.len(), 1, "the preamble turn is still in the transcript");
        let m = &sink.messages[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::InjectedContext, Origin::Harness),
            "gemini-cli fed the model that block; nobody typed it"
        );
        assert!(m.text().unwrap().starts_with("<session_context>"));
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "main");
        assert!(s.lineage.is_empty(), "a main recording has no parent");
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn subagent_recording_points_at_its_parent() {
        // A sub-agent recording lives under `chats/<parentId>/<id>.jsonl` with `kind: "subagent"`
        // (`chatRecordingService.ts`); its parent is the recording one level up.
        let (runtime, _) = runtime_fixture("subagent", "proj", "session_modern.jsonl");
        let parent_id = "11112222-3333-4444-5555-666677778888";
        let child_dir = runtime.join("tmp").join("proj").join("chats").join(parent_id);
        fs::create_dir_all(&child_dir).unwrap();
        let child = child_dir.join("aaaa0000-0000-0000-0000-000000000001.jsonl");
        let text = fixture("session_modern.jsonl")
            .replacen(
                "11112222-3333-4444-5555-666677778888",
                "aaaa0000-0000-0000-0000-000000000001",
                1,
            )
            .replacen(r#""kind":"main""#, r#""kind":"subagent""#, 1);
        fs::write(&child, text).unwrap();

        let refs = scan_session_file(&child, Harness::Gemini);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "aaaa0000-0000-0000-0000-000000000001");
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(
            s.lineage,
            Lineage {
                parent: Some(parent_id.into()),
                ..Default::default()
            }
        );
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "subagent");
        // the parent's own recording, one level up, has no parent
        let parent_refs = scan_session_file(&runtime.join("tmp/proj/chats/session_modern.jsonl"), Harness::Gemini);
        let mut sink = CollectSink::default();
        let p = adapter
            .stream(&parent_refs[0], &ParseOptions::full(), &mut sink)
            .unwrap();
        assert!(p.lineage.is_empty());
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn rewrite_siblings_are_not_sessions() {
        // `rewriteConversationFile` (chatRecordingService.ts:575-640) leaves `<file>.unreadable-<ms>`
        // and writes through `<file>.tmp-<pid>`; neither is a recording.
        let (runtime, file) = runtime_fixture("siblings", "proj", "session_modern.jsonl");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(file.with_extension("jsonl.unreadable-1700000000000"), &text).unwrap();
        fs::write(file.with_extension("jsonl.tmp-4242"), &text).unwrap();
        let adapter = Gemini {
            roots: vec![runtime.join("tmp")],
        };
        let refs = adapter.discover().unwrap();
        assert_eq!(
            refs.len(),
            1,
            "only the real recording is discovered: {:?}",
            refs.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        assert!(refs[0].path.ends_with("session_modern.jsonl"));
        let _ = fs::remove_dir_all(&runtime);
    }
}
