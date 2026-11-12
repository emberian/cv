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

use crate::json::{ReadInput, Validation};
mod emit;
mod record;
pub use emit::{emit, emit_jsonl};

use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::json::JsonSource;
use crate::source::{Dependencies, Probe};
use crate::stream::{self, CollectSink, Flow, MessageSink};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Gemini {
    /// Every `tmp` root that exists: `~/.gemini/tmp`, then the sandbox root `~/.cache/.gemini/tmp`.
    roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Gemini = Gemini { roots: Vec::new() };

impl Gemini {
    pub fn detect() -> Self {
        let roots = crate::env::home_dir()
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

    /// Rooted at one runtime dir (a `.gemini` or `.cache/.gemini`): reads `<dir>/tmp`, and the
    /// registry beside it. For tests and the corpus; never consults the process environment.
    pub fn with_home(runtime: &Path) -> Self {
        Gemini {
            roots: vec![runtime.join("tmp")],
        }
    }
}

impl Adapter for Gemini {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if let Some(project) = chats_project(&r.path) {
            deps.directories.push(project.join(&r.id).join("tracker"));
            deps.directories.push(project.join(&r.id).join("plans"));
        }
        deps.files.extend(registry_files(&r.path).into_iter().flatten());
        Some(())
    }

    /// A `logs.json` holds a whole project's sessions: each is certified by its own entries.
    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        is_logs(&r.path).then(|| r.path.clone())
    }

    /// Every session's certificate in the `logs.json` at `store` ([`logs`]).
    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        is_logs(store).then(|| Some(logs(store).ok()?.rows.clone())).flatten()
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        if self.roots.is_empty() {
            return Ok(Default::default());
        }
        // Collect candidate file paths first (cheap walk over every root), then read+parse in
        // parallel. A single `logs.json` can expand into many sessions, so this is a flat-map.
        // An unreadable dir, and a session file that fails to read, are skipped and reported.
        let mut walk_errors = Vec::new();
        let paths: Vec<_> = self
            .roots
            .iter()
            .flat_map(|root| super::walk_dir_bulk(root, 0, usize::MAX, true, &mut walk_errors))
            .collect();
        let mut found: crate::Discovered = crate::par_discovery_flat_map(paths, |path| {
            crate::Skipped::listed_many(
                &path,
                crate::discover_cache::cached_scan_many_checked(&path, || scan_session_file(&path, Harness::Gemini)),
            )
        })
        .into_iter()
        .collect();
        found.refs = dedupe_by_id(std::mem::take(&mut found.refs));
        found.skipped.extend(walk_errors);
        Ok(found)
    }

    fn stream(&self, r: &SessionRef, opts: &stream::Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| stream_for(Harness::Gemini, r, opts, sink))
    }

    /// The per-session side files under `tmp/<project>/<session id>/`: the task tracker
    /// (`tracker/<id>.json`, `tracker_*` tools) and the plan-mode plans (`plans/*.md`).
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        side_affordances(&r.path, &r.id)
    }
}

/// Keep one [`SessionRef`] per session id across files and roots (a sandboxed and an unsandboxed run
/// of the same project could both hold a copy, and a resumed 0.46.0 process leaves a stub): a file
/// with turns beats one without, then the most recently updated wins; input order is otherwise
/// preserved.
fn dedupe_by_id(refs: Vec<SessionRef>) -> Vec<SessionRef> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<SessionRef> = Vec::with_capacity(refs.len());
    for r in refs {
        match seen.get(&r.id) {
            Some(&i) => {
                // A file with turns beats a startup stub: a resumed 0.46.0 process writes a second
                // `session-<ts>-<id>.jsonl` for the same id holding only the `<session_context>`
                // preamble (later gemini-cli deletes it, `deleteCurrentSessionIfNotResumableAsync`),
                // and it is the NEWER file. Between equals the most recently updated wins.
                let key = |r: &SessionRef| (r.message_count.is_some_and(|n| n > 0), r.updated_at);
                if key(&r) > key(&out[i]) {
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
    type_user_turn_from(m, &text);
}

fn type_user_turn_from(m: &mut Message, text: &str) {
    let t = text.trim_start();
    let (kind, origin) = if t.starts_with("<hook_context>") {
        (MessageKind::InjectedContext, Origin::Hook)
    // Emptiness is re-tested here: `is_ignored_user_content` would take it, but an empty or
    // whitespace-only turn stays a human prompt.
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

/// The project dir a recording hangs off: the parent of its `chats/` ancestor.
fn chats_project(from: &Path) -> Option<&Path> {
    from.ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "chats"))
        .and_then(Path::parent)
}

fn project_cwd(path: &Path) -> Option<PathBuf> {
    let (runtime, identifier) = project_of(path)?;
    registry_for(&runtime).get(identifier).cloned()
}

/// `<runtime>` and the project identifier of a file under `<runtime>/tmp/<id>/`.
fn project_of(path: &Path) -> Option<(PathBuf, &str)> {
    let comps: Vec<&std::ffi::OsStr> = path.components().map(|c| c.as_os_str()).collect();
    // The INNERMOST `tmp`, not the first: the layout is `<runtime>/tmp/<id>/chats/<file>`, and a
    // runtime root can itself sit under a directory called `tmp` — `/tmp/...` on Linux, or a
    // Seatbelt/CI root. Scanning left-to-right picked that outer one, made `<runtime>` the
    // filesystem root and silently lost every cwd.
    let tmp_at = comps.iter().rposition(|c| *c == "tmp")?;
    let identifier = comps.get(tmp_at + 1)?.to_str()?;
    // `<runtime>` is the parent of `tmp`: rebuild it from the leading components.
    let runtime: PathBuf = comps[..tmp_at].iter().collect();
    if runtime.as_os_str().is_empty() {
        return None;
    }
    Some((runtime, identifier))
}

/// What [`project_cwd`] reads for `path`'s project: the registry's `projects.json` and the
/// project's two `.project_root` markers. The certificate of every session whose cwd it gives.
pub(crate) fn registry_files(path: &Path) -> Option<[PathBuf; 3]> {
    let (runtime, identifier) = project_of(path)?;
    Some([
        runtime.join("projects.json"),
        runtime.join("tmp").join(identifier).join(".project_root"),
        runtime.join("history").join(identifier).join(".project_root"),
    ])
}

/// `short-id → project path`, one map per runtime root.
type Registry = std::sync::Arc<std::collections::HashMap<String, PathBuf>>;

/// What a [`Registry`] read saw: the modification times of the three places it reads from.
/// `tmp/` and `history/` move when a project directory (and so its marker) is added or removed
/// under them; `projects.json` moves when the map is rewritten.
type RegistryStamp = [Option<std::time::SystemTime>; 3];

fn registry_stamp(runtime: &Path) -> RegistryStamp {
    ["tmp", "history", "projects.json"].map(|name| fs::metadata(runtime.join(name)).and_then(|m| m.modified()).ok())
}

/// The [`Registry`] for one runtime root, read once and re-read when its stamp moves, so a
/// long-lived process (`serve`, `mcp`, a follower) resolves projects registered after it started.
fn registry_for(runtime: &Path) -> Registry {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (RegistryStamp, Registry)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let stamp = registry_stamp(runtime);
    let hit = cache.lock().ok().and_then(|c| {
        c.get(runtime)
            .filter(|(seen, _)| *seen == stamp)
            .map(|(_, registry)| registry.clone())
    });
    if let Some(hit) = hit {
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
        c.insert(runtime.to_path_buf(), (stamp, arc.clone()));
    }
    arc
}

/// Discovery scan of one Gemini-format file into 0..n [`SessionRef`]s, tagged as `harness`.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter (identical on-disk format). Notably,
/// checkpoints derive their id from the `checkpoint-<tag>` *filename* here — the same id
/// [`stream_for`]'s bounded path produces on parse — so discover/parse/stream all agree.
pub(crate) fn scan_session_file(path: &Path, harness: Harness) -> Result<Vec<SessionRef>> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == "logs.json" {
        let cwd = project_cwd(path);
        return Ok(logs(path)?
            .sessions
            .iter()
            .map(|s| SessionRef {
                cwd: cwd.clone(),
                ..session_ref(s, path, harness)
            })
            .collect());
    } else if is_chat_recording(path) {
        let mut turns = 0usize;
        let mut sink = |m: Message| {
            turns += usize::from(matches!(m.role, Role::User | Role::Assistant));
            Flow::Continue
        };
        let r = SessionRef::new("", harness, path.to_path_buf());
        let s = stream_for(harness, &r, &stream::Demand::LEAN.with_spans(), &mut sink)?;
        let mut r = session_ref(&s, path, harness);
        r.message_count = Some(turns);
        return Ok(vec![r]);
    } else if is_checkpoint(&name) {
        let text = fs::read_to_string(path)?;
        if let Some(s) = parse_checkpoint(harness, &text, &name, Some(path.to_path_buf()))? {
            return Ok(vec![session_ref(&s, path, harness)]);
        }
    }
    Ok(Vec::new())
}

/// Streaming parse of any Gemini-format session file, tagged as `harness` in the returned session.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter — Qwen Code is a gemini-cli fork with an
/// **identical** on-disk format rooted at `~/.qwen`, so it reuses this machinery and only re-tags.
pub(crate) fn stream_for(
    harness: Harness,
    r: &SessionRef,
    opts: &stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let name = r
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Chat recording (legacy whole-file .json / modern append-only .jsonl): one session per
    // file, id authoritative. These are the only shapes large enough to OOM, so they are read
    // one message at a time; the bounded shapes below stay on the materializing bridge. A
    // `.json` that is not a single document (a `.json`-named JSONL recording) is read as JSONL.
    if is_chat_recording(&r.path) {
        let document = match r.path.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => None,
            _ => read_document(&r.path, harness, opts, sink)?,
        };
        let recording = match document {
            Some(s) => Some(s),
            None => read_jsonl(&r.path, harness, opts, sink)?,
        };
        if let Some(mut s) = recording {
            s.harness = harness;
            return Ok(s);
        }
    }

    // Bounded shapes (checkpoints, logs.json): a few KB–MB, never the OOM risk. Bridge through a
    // whole-Session parse and replay its messages into the sink.
    let mut session = parse_bounded(harness, r, &name)?;
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
/// to materialize.
fn parse_bounded(harness: Harness, r: &SessionRef, name: &str) -> Result<Session> {
    if name == "logs.json" {
        let logs = logs(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
        if let Some(s) = logs.sessions.iter().find(|s| s.id == r.id) {
            return Ok(Session {
                cwd: project_cwd(&r.path),
                ..s.clone()
            });
        }
        anyhow::bail!(
            "could not parse Gemini-format session {} from {}",
            r.id,
            r.path.display()
        )
    }
    let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
    // Checkpoint: one session per file.
    if is_checkpoint(name) {
        if let Some(s) = parse_checkpoint(harness, &text, name, Some(r.path.clone()))? {
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

/// A gemini-cli `logs.json`: every session of a project in one file.
pub(crate) fn is_logs(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "logs.json")
}

fn is_checkpoint(name: &str) -> bool {
    name.starts_with("checkpoint-") && name.ends_with(".json")
}

// ---------------------------------------------------------------------------
// Chat recordings: one reader per era, both feeding the message codec in `record.rs`.
// ---------------------------------------------------------------------------

/// Read a *legacy whole-file* chat recording (`{sessionId, messages:[…]}` as one JSON document):
/// a streaming visitor over `messages[]`, one message through the codec at a time, so the
/// document is never materialized. Mapped builds borrow it from one mapping; positioned builds
/// read it in place. `Ok(None)` when the bytes are not that shape (e.g. a `.json`-named JSONL
/// log), so the caller can try JSONL.
fn read_document(
    path: &Path,
    harness: Harness,
    opts: &stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    let source = JsonSource::capture(path)?;
    if source.len() == 0 {
        return Ok(None);
    }
    let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
    let d = record::Demand::of(harness, opts, &source, &resolver);
    // Nearly every recording is one Value-compatible document: one validating pass, and its
    // messages need no check of their own. One that is not (a field Value rejects) is read as
    // serde's typed envelope: syntax only, each message accepted on its own. Either way the
    // source view checks the descriptor's identity after the visit.
    let mut visited = false;
    let read = source.read(ReadInput::Document, Validation::Value, |view| {
        visited = true;
        document(view, Some(path), d, sink, true)
    });
    let read = match read {
        Err(error) if !visited && error.is::<crate::json::InvalidJson>() => {
            source.read(ReadInput::Document, Validation::Syntax, |view| {
                document(view, Some(path), d, sink, false)
            })
        }
        read => read,
    };
    match read {
        Err(error) if error.is::<crate::json::InvalidJson>() || error.is::<super::store::document::InvalidStruct>() => {
            Ok(None)
        }
        result => result,
    }
}

/// The document's known fields, in the order serde's derived struct reads them positionally.
const DOCUMENT: &[&str] = &[
    "sessionId",
    "startTime",
    "lastUpdated",
    "summary",
    "directories",
    "messages",
    "kind",
];

/// A whole-file recording: typed, unique known fields (strings or null, `sessionId` required,
/// `messages` an array), each message Value-compatible on its own (`accepted` when the whole
/// document already is). `None` when it is not one.
fn document(
    view: crate::json::JsonView<'_>,
    path: Option<&Path>,
    d: record::Demand<'_>,
    sink: &mut dyn MessageSink,
    accepted: bool,
) -> Result<Option<Session>> {
    let Some(fields) = super::store::document::Fields::parse_struct(view, DOCUMENT)? else {
        return Ok(None);
    };
    let mut meta = serde_json::Map::new();
    for key in ["sessionId", "startTime", "lastUpdated", "summary", "kind"] {
        let Some(value) = fields.get(key) else { continue };
        if value.is_null()? {
            continue;
        }
        if !value.is_string()? || !value.accepts_value()? {
            return Ok(None);
        }
        meta.insert(key.into(), value.resolve()?.into_owned());
    }
    if !meta.contains_key("sessionId") {
        return Ok(None);
    }
    if let Some(dirs) = fields.get("directories") {
        if !dirs.is_null()? {
            let Some(dirs) = dirs.array()? else { return Ok(None) };
            let mut directories = Vec::new();
            for dir in dirs {
                let dir = dir?;
                if !dir.is_string()? || !dir.accepts_value()? {
                    return Ok(None);
                }
                directories.push(dir.resolve()?.into_owned());
            }
            meta.insert("directories".into(), Value::Array(directories));
        }
    }
    let Some(messages) = fields
        .get("messages")
        .map(crate::json::JsonView::array)
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let mut session = record_metadata(&meta, d.harness, path.map(Path::to_path_buf));
    let summary = session.title.take();
    let mut st = EmitState::default();
    let mut published = false;
    for message in messages {
        let message = message?;
        if !accepted && !message.accepts_value()? {
            continue;
        }
        if d.decode(message, &mut st, sink, &mut published)? == Flow::Stop {
            break;
        }
    }
    session.model = st.session_model;
    session.title = summary.or(st.title);
    Ok(Some(session))
}

/// Read a *modern* append-only JSONL recording. A later record can replace or truncate earlier
/// messages, so a first pass over the lines ([`jsonl::Read::lines`](super::store::jsonl::Read::lines)) reaches
/// EOF retaining only keys and byte ranges, never a superseded message's content; the effective
/// messages are then read again by range, in order, through the codec.
///
/// `Ok(None)` when the file is not a recording (no metadata line and no message records).
fn read_jsonl(
    path: &Path,
    harness: Harness,
    opts: &stream::Demand,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    let read = super::store::jsonl::Read::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut replay = RecordingReplay::default();
    read.lines(0, |line| {
        if let Some(view) = line.view {
            replay.ingest(view)?;
        }
        Ok(Flow::Continue)
    })?;
    let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
    let mut records = read.records();
    let mut published = false;
    // The effective records are read again by range between callbacks: under the guard, a slow
    // callback's backing check stops the read before the next range is touched.
    crate::stream::guarded(sink, &read, |sink| {
        replay.emit(
            record::Demand::of(harness, opts, read.source(), &resolver),
            Some(path),
            Some(&mut records),
            sink,
            &mut published,
        )
    })
}

/// Where a retained message lives: a byte range of the source, or an owned value (records of a
/// parsed string, and the synthetic rewind markers).
#[derive(Debug, PartialEq)]
enum Body {
    Range(u64, u64),
    Owned(Value),
}

impl Body {
    fn of(view: crate::json::JsonView<'_>) -> Result<Self> {
        Ok(match view.to_json()? {
            crate::json::Json::Source(range) => Body::Range(range.offset(), range.len()),
            crate::json::Json::Owned(value) => Body::Owned(value),
        })
    }
}

/// Replays a *modern* append-only JSONL recording's records into its effective state: the metadata
/// map plus the retained message bodies (insertion-ordered, with `$set` replacement and
/// `$rewindTo` truncation applied).
#[derive(Default)]
struct RecordingReplay {
    metadata: serde_json::Map<String, Value>,
    /// Insertion order of message keys (replacement keeps the original position). A key is the
    /// record's `type` and `id` ([`replay_key`]): a resume's `$set.messages` (re-synced from the API
    /// history) can give the tool-response `user` turn the SAME id as the `gemini` turn that made the
    /// call (seen on 0.63.0), and keying by id alone let it overwrite the assistant turn.
    order: Vec<String>,
    msgs: BTreeMap<String, Body>,
    /// First insertion position for each wire ID, shared across type-qualified replay keys.
    first_positions: HashMap<String, usize>,
    /// Whether a real metadata line (sessionId + projectHash) was seen.
    saw_meta: bool,
    /// How many `$rewindTo` records were replayed (names the synthetic Branch markers).
    rewinds: usize,
}

/// A message record's replay key, `<type>\u{1f}<id>` (see [`RecordingReplay::order`]); `None`
/// without a string `id`.
fn replay_key(fields: &super::store::document::Fields<'_>) -> Result<Option<String>> {
    let Some(id) = fields.string("id")? else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}\u{1f}{id}",
        fields.string("type")?.unwrap_or_default()
    )))
}

/// The id part of a [`replay_key`].
fn key_id(key: &str) -> &str {
    key.split_once('\u{1f}').map_or(key, |(_, id)| id)
}

impl RecordingReplay {
    fn insert(&mut self, key: String, body: Body) {
        match self.msgs.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                self.first_positions
                    .entry(key_id(entry.key()).to_owned())
                    .or_insert(self.order.len());
                self.order.push(entry.key().clone());
                entry.insert(body);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(body);
            }
        }
    }

    fn clear_messages(&mut self) {
        self.order.clear();
        self.msgs.clear();
        self.first_positions.clear();
    }

    fn rewind(&mut self, target: &str) {
        if let Some(pos) = self.first_positions.get(target).copied() {
            for key in self.order.drain(pos..) {
                let id = key_id(&key);
                if self.first_positions.get(id).is_some_and(|first| *first >= pos) {
                    self.first_positions.remove(id);
                }
                self.msgs.remove(&key);
            }
        } else {
            self.clear_messages();
        }
        self.rewinds += 1;
        let marker_id = format!("$rewind-{}", self.rewinds);
        let marker = serde_json::json!({ "id": marker_id, "type": "rewind", "rewindTo": target });
        self.insert(marker_id, Body::Owned(marker));
    }

    fn note_metadata(&mut self, fields: &super::store::document::Fields<'_>) -> Result<()> {
        for key in RECORD_METADATA {
            if let Some(value) = fields.value(key)? {
                self.metadata.insert((*key).into(), value);
            }
        }
        Ok(())
    }

    /// Fold one JSONL record into the replay state. Non-object junk is ignored.
    fn ingest(&mut self, view: crate::json::JsonView<'_>) -> Result<()> {
        use super::store::document::Fields;
        let Some(rec) = Fields::parse(
            view,
            &["$rewindTo", "$set", "id", "type", "content", "sessionId", "projectHash"],
        )?
        else {
            return Ok(());
        };
        if let Some(target) = rec.string("$rewindTo")? {
            self.rewind(&target);
            return Ok(());
        }
        if let Some(set) = rec.get("$set") {
            const SET: &[&str] = &[
                "sessionId",
                "startTime",
                "lastUpdated",
                "summary",
                "directories",
                "kind",
                "messages",
            ];
            if let Some(set) = Fields::parse(set, SET)? {
                if let Some(messages) = set.get("messages") {
                    // Checkpoint: rebuild the whole message list.
                    self.clear_messages();
                    if let Some(messages) = messages.array()? {
                        for message in messages {
                            let message = message?;
                            if let Some(key) = Fields::parse(message, &["id", "type"])?
                                .map(|m| replay_key(&m))
                                .transpose()?
                                .flatten()
                            {
                                self.insert(key, Body::of(message)?);
                            }
                        }
                    }
                }
                return self.note_metadata(&set);
            }
        }
        if let Some(key) = replay_key(&rec)? {
            // A message record (must have an id and a type/content); a junk record with an id but
            // no message shape is ignored either way.
            if rec.get("type").is_some() || rec.get("content").is_some() {
                self.insert(key, Body::of(view)?);
            }
            return Ok(());
        }
        // Metadata line(s): a real recording's metadata carries BOTH sessionId and projectHash
        // (gemini-cli's `isPartialMetadataRecord`). This guards against mistaking a `logs.json`
        // entry (sessionId + messageId + message, no projectHash) for a recording.
        if rec.get("sessionId").is_some() && rec.get("projectHash").is_some() {
            self.saw_meta = true;
            if let Some(meta) = Fields::parse(view, RECORD_METADATA)? {
                self.note_metadata(&meta)?;
            }
        }
        Ok(())
    }

    /// The effective session: its metadata, then each retained message through the codec in
    /// order (`records` reads the ranged ones). `None` when nothing marked it a recording.
    fn emit(
        mut self,
        d: record::Demand<'_>,
        path: Option<&Path>,
        mut records: Option<&mut super::store::jsonl::Records<'_>>,
        sink: &mut dyn MessageSink,
        published: &mut bool,
    ) -> Result<Option<Session>> {
        if !self.saw_meta && self.msgs.is_empty() {
            return Ok(None);
        }
        let mut session = record_metadata(&self.metadata, d.harness, path.map(Path::to_path_buf));
        let summary = session.title.take();
        let mut st = EmitState::default();
        for key in &self.order {
            let flow = match self.msgs.remove(key) {
                Some(Body::Range(at, len)) => records
                    .as_deref_mut()
                    .context("ranged record without its source")?
                    .read(at, len, |view| d.decode(view, &mut st, sink, published))?,
                Some(Body::Owned(value)) => d.decode((&value).into(), &mut st, sink, published)?,
                None => continue,
            };
            if flow == Flow::Stop {
                break;
            }
        }
        session.model = st.session_model;
        session.title = summary.or(st.title);
        Ok(Some(session))
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
pub fn parse_all_str(text: &str, source_path: Option<PathBuf>) -> Result<Vec<Session>> {
    parse_all_str_for(Harness::Gemini, text, source_path)
}

/// [`parse_all_str`] tagged as `harness` — Qwen Code shares the format, so its harness-specific
/// facts (`extra["qwen"]`) and the session tag must say "qwen", not "gemini".
pub fn parse_all_str_for(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Result<Vec<Session>> {
    let mut out = parse_all_str_inner(harness, text, source_path)?;
    for s in &mut out {
        s.harness = harness;
    }
    Ok(out)
}

fn parse_all_str_inner(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Result<Vec<Session>> {
    // Parse an owned input once, then give its view to the same document/part codecs as files.
    let value = serde_json::from_str::<Value>(text).ok();
    if let Some(session) = parse_chat_recording(text, value.as_ref(), harness, source_path.clone())? {
        return Ok(vec![session]);
    }
    let Some(value) = value else { return Ok(Vec::new()) };
    // ── p15-18 ──
    let logs_shaped = value.as_array().is_some_and(|items| {
        items
            .iter()
            .take(8)
            .any(|it| it.get("messageId").is_some() && it.get("message").is_some())
    });
    if logs_shaped {
        return Ok(parse_logs(value, source_path));
    }
    let checkpoint_shaped = value.get("history").is_some()
        || value.as_array().is_some_and(|items| {
            items
                .iter()
                .take(8)
                .any(|it| it.get("role").is_some() && it.get("parts").is_some())
        });
    if checkpoint_shaped {
        return Ok(checkpoint(harness, (&value).into(), "", source_path)?
            .into_iter()
            .collect());
    }
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// logs.json
// ---------------------------------------------------------------------------

/// Parse a Gemini `logs.json` from its text contents into one [`Session`] per distinct `sessionId`.
fn parse_logs_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let value = serde_json::from_str::<Value>(text).unwrap_or_default();
    parse_logs(value, source_path)
}

/// One `logs.json`, parsed: its sessions (`cwd` unset: the registry gives it per read, see
/// [`project_cwd`]) and each session's [`Rows`](crate::source::Rows) certificate.
pub(crate) struct Logs {
    sessions: Vec<Session>,
    pub(crate) rows: std::collections::HashMap<String, crate::source::Rows>,
}

/// The `logs.json` at `path`, parsed once per file state ([`crate::source::FileId`]) in this
/// process. One file holds a whole project's sessions, so a read of each session parsed the whole
/// file again, and discovery once more; one parse now serves them all until the file moves.
/// Memory: the latest parse of each `logs.json` read (a few KB to MB each).
pub(crate) fn logs(path: &Path) -> Result<std::sync::Arc<Logs>> {
    use crate::source::FileId;
    use std::sync::{Arc, LazyLock, Mutex};
    type Memo = Mutex<std::collections::HashMap<PathBuf, (FileId, Arc<Logs>)>>;
    static MEMO: LazyLock<Memo> = LazyLock::new(Default::default);
    let lock = || MEMO.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = FileId::at(path);
    if let Some(state) = before {
        let hit = lock()
            .get(path)
            .filter(|(at, _)| *at == state)
            .map(|(_, logs)| logs.clone());
        if let Some(hit) = hit {
            return Ok(hit);
        }
    }
    #[cfg(test)]
    LOGS_PARSED.lock().unwrap().push(path.to_path_buf());
    let text = fs::read_to_string(path)?;
    let value = serde_json::from_str::<Value>(&text).unwrap_or_default();
    drop(text);
    let mut rows = std::collections::HashMap::new();
    if let Value::Array(entries) = &value {
        use crate::source::Rows;
        for (at, e) in entries.iter().enumerate() {
            let sid = e.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
            let cert = rows.entry(sid).or_insert(Rows {
                count: 0,
                high: 0,
                digest: Rows::SEED,
            });
            cert.count += 1;
            cert.high = i64::try_from(at).unwrap_or(i64::MAX);
            cert.digest = Rows::fold(cert.digest, &serde_json::to_vec(e).unwrap_or_default());
        }
    }
    let sessions = parse_logs_with(value, Some(path.to_path_buf()), |_| None);
    let logs = Arc::new(Logs { sessions, rows });
    // Kept only when the file did not move while it was read.
    if let Some(state) = before.filter(|state| FileId::at(path) == Some(*state)) {
        lock().insert(path.to_path_buf(), (state, logs.clone()));
    }
    Ok(logs)
}

#[cfg(test)]
static LOGS_PARSED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn parse_logs(value: Value, source_path: Option<PathBuf>) -> Vec<Session> {
    parse_logs_with(value, source_path, project_cwd)
}

fn parse_logs_with(value: Value, source_path: Option<PathBuf>, cwd: fn(&Path) -> Option<PathBuf>) -> Vec<Session> {
    // Consumed entry by entry: the file's tree and the built sessions never both stand complete.
    let entries = match value {
        Value::Array(entries) => entries,
        _ => Vec::new(),
    };

    let mut by_session: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in entries {
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
            .map(|t| crate::ir::truncate(t, crate::projection::Head::TITLE));
        let times: Vec<DateTime<Utc>> = items
            .iter()
            .filter_map(|e| e.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
            .collect();

        let mut s = Session {
            cwd: source_path.as_deref().and_then(cwd),
            title,
            created_at: times.iter().min().copied(),
            updated_at: times.iter().max().copied(),
            source_path: source_path.clone(),
            ..Session::new(sid, Harness::Gemini)
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

/// Parse a gemini-cli chat recording from text, with no filesystem access: the legacy whole-file
/// JSON object (`{sessionId, messages:[…]}`) or the modern append-only JSONL log (metadata line +
/// message records + `$set` / `$rewindTo` control records), through the same readers as a file.
/// `None` if the text is not a recording.
fn parse_chat_recording(
    text: &str,
    value: Option<&Value>,
    harness: Harness,
    source_path: Option<PathBuf>,
) -> Result<Option<Session>> {
    let resolver = crate::lazy::Resolver::new(None);
    let d = record::Demand {
        harness,
        source: None,
        keep_image_data: true,
        resolver: &resolver,
    };
    let mut sink = CollectSink::default();
    // A modern recording's metadata line also starts with `{`, but a whole-file parse of
    // multi-line JSONL fails, so it falls through to the line replay.
    let document = match value {
        Some(value @ Value::Object(_)) => document(value.into(), source_path.as_deref(), d, &mut sink, true)?,
        _ => None,
    };
    let mut session = match document {
        Some(session) => session,
        None => {
            let mut replay = RecordingReplay::default();
            super::store::jsonl::slice(text.as_bytes(), |line| {
                if let Some(view) = line.view {
                    replay.ingest(view)?;
                }
                Ok(Flow::Continue)
            })?;
            let Some(session) = replay.emit(d, source_path.as_deref(), None, &mut sink, &mut false)? else {
                return Ok(None);
            };
            session
        }
    };
    session.messages = sink.messages;
    Ok(Some(session))
}

/// Build the IR [`Session`] *metadata* (everything but `messages`) from a `ConversationRecord`'s
/// top-level fields. `title` is the record's `summary` when present; the per-message scan fills in a
/// first-user-turn fallback and the session `model`. `messages` is left empty — the caller streams
/// them through the message codec (`record.rs`).
const RECORD_METADATA: &[&str] = &[
    DOCUMENT[0],
    DOCUMENT[1],
    DOCUMENT[2],
    DOCUMENT[3],
    DOCUMENT[4],
    DOCUMENT[6],
];

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
        cwd,
        title,
        created_at,
        updated_at,
        source_path,
        extra,
        lineage,
        ..Session::new(id, harness)
    }
}

/// Cross-message state of one recording's emission.
#[derive(Default)]
struct EmitState {
    /// The first assistant `model` seen; becomes `Session::model`.
    session_model: Option<String>,
    /// The first prompt's text: the fallback title when the record has no `summary`.
    title: Option<String>,
    /// Tool call ids whose result was already emitted from a `toolCalls[]` entry. gemini-cli also
    /// records each tool response as a `user` record of bare `functionResponse` parts (the wire
    /// turn, `recordSyntheticMessage` in `geminiChat.ts`; 0.46.0 and later), and a resume re-syncs it
    /// into `$set.messages` once more; those copies are not new results.
    answered: std::collections::HashSet<String>,
}

// ---------------------------------------------------------------------------
// gemini-cli checkpoints — `{history: Content[]}` or bare `Content[]`
// ---------------------------------------------------------------------------

/// Parse a gemini-cli checkpoint into an IR [`Session`]. The id is derived from the `checkpoint-<tag>`
/// filename when available (the `tag` is percent-encoded by gemini-cli; we decode it best-effort).
fn parse_checkpoint(
    harness: Harness,
    text: &str,
    file_name: &str,
    source_path: Option<PathBuf>,
) -> Result<Option<Session>> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    checkpoint(harness, (&value).into(), file_name, source_path)
}

fn checkpoint(
    harness: Harness,
    view: crate::json::JsonView<'_>,
    file_name: &str,
    source_path: Option<PathBuf>,
) -> Result<Option<Session>> {
    let history = if view.is_array()? {
        Some(view)
    } else {
        view.get("history")?
    };
    let Some(history) = history.map(|v| v.array()).transpose()?.flatten() else {
        return Ok(None);
    };
    let id = checkpoint_id(file_name);
    let resolver = crate::lazy::Resolver::new(None);
    let parts_demand = record::Demand {
        harness,
        source: None,
        keep_image_data: true,
        resolver: &resolver,
    };
    let mut messages = Vec::new();
    let mut title: Option<String> = None;

    for content in history {
        let Some(obj) = super::store::document::Fields::parse(content?, &["role", "parts"])? else {
            continue;
        };
        let role = match obj.string("role")?.as_deref() {
            Some("model") | Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => Role::User, // gemini uses "user" for both prompts and tool responses
        };
        let mut m = Message::new(role);
        if role == Role::System {
            m.kind = MessageKind::SystemPrompt;
        }
        if let Some(parts) = obj.get("parts") {
            record::content_blocks(parts, parts_demand, &mut m.content)?;
        }
        // If this "user" turn is purely tool results, retag it as a Tool turn.
        if role == Role::User
            && !m.content.is_empty()
            && m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }))
        {
            m.role = Role::Tool;
            m.kind = MessageKind::ToolResult;
            m.origin = Origin::Harness;
        } else if role == Role::User {
            // The API history merges consecutive user parts into one Content, so a 0.63.0 checkpoint
            // holds the `<session_context>` preamble and the first prompt as two parts of ONE turn.
            // Split the leading injected parts off into their own InjectedContext turn.
            let lead = m
                .content
                .iter()
                .take_while(
                    |b| matches!(b, Block::Text { text } if is_ignored_user_content(text) && !text.trim().is_empty()),
                )
                .count();
            if lead > 0 && lead < m.content.len() {
                let mut ctx = Message::new(Role::User);
                ctx.content = m.content.drain(..lead).collect();
                type_user_turn(&mut ctx);
                messages.push(ctx);
            }
            type_user_turn(&mut m);
        }
        if m.role == Role::User && title.is_none() {
            title = m
                .text()
                .filter(|t| !is_ignored_user_content(t))
                .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE));
        }
        if !m.content.is_empty() {
            messages.push(m);
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    // The file is a `/chat save <tag>` point that `/chat resume <tag>` restores: a checkpoint.
    let tag = id.strip_prefix("checkpoint-").unwrap_or("").to_string();
    let mut s = Session {
        cwd: source_path.as_deref().and_then(project_cwd),
        title,
        messages,
        source_path,
        ..Session::new(id, harness)
    };
    let bag = s.harness_extra_mut(harness);
    crate::affordance::note_checkpoints(bag, 1, []);
    if let Some(cp) = bag
        .get_mut(crate::affordance::CHECKPOINTS_KEY)
        .and_then(Value::as_object_mut)
    {
        if !tag.is_empty() {
            cp.insert("tag".into(), Value::String(tag));
        }
    }
    Ok(Some(s))
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
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at, // The contract count (`SessionRef::message_count`): user + assistant turns only — a tool
        // result, an `info`/`error` notice or a `$rewindTo` branch marker is a message, not a turn.
        message_count: Some(
            s.messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count(),
        ),
        ..SessionRef::new(s.id.clone(), harness, path.to_path_buf())
    }
}

/// `tracker/*.json` as the session's plan and each `plans/*.md` as one plan row (its first
/// heading), from the session's directory beside `chats/`.
fn side_affordances(recording: &Path, id: &str) -> Result<Vec<crate::affordance::Affordance>> {
    use crate::affordance::{item_of, json_files, plan_state, Affordance, Kind};
    let Some(project) = chats_project(recording) else {
        return Ok(Vec::new());
    };
    let dir = project.join(id);
    let mut out = Vec::new();
    let items: Vec<_> = json_files(&dir.join("tracker"))?
        .iter()
        .filter_map(|(_, v)| item_of(v))
        .collect();
    out.extend(plan_state(Kind::Plan, "tracker/", items));
    let mut plans: Vec<PathBuf> = super::side_entries(&dir.join("plans"))?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    plans.sort();
    for p in plans {
        let text = fs::read_to_string(&p)?;
        let head = text
            .lines()
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("plan.md");
        out.push(Affordance::store(
            Kind::Plan,
            format!("plans/{name}"),
            truncate(head, 120),
            Vec::new(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn checked_complete() -> stream::Demand {
        stream::Demand {
            spans: true,
            ..stream::Demand::COMPLETE
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/gemini/{}", env!("CARGO_MANIFEST_DIR"), name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    /// A document is serde's typed envelope: unknown fields need only be JSON, known ones are
    /// unique strings (or null), the positional form works, and each message is accepted on its
    /// own, whether or not the whole document is Value-compatible.
    #[test]
    fn document_acceptance_is_the_typed_envelope_with_independent_messages() {
        let world = crate::test_world("gemini-document-acceptance");
        let path = world.base.join("source.json");
        let large = serde_json::to_string(&"x\nβ".repeat(6000)).unwrap();
        let message = format!("{{\"type\":\"gemini\",\"id\":\"a\",\"toolCalls\":[{{\"name\":\"custom\",\"args\":{{\"payload\":{large}}}}}]}}");
        let child_poison = r#"{"type":"user","id":"bad","content":"poison","ignored":"\ud800"}"#;
        let mut cases = vec![
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{child_poison},{message}],\"ignored\":1e400}}"),
                true,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{child_poison},{message}]}}"),
                true,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"ignored\":\"\\ud800\"}}"),
                true,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"ignored\":1,\"ignored\":2}}"),
                true,
            ),
            (
                format!("[\"source\",null,null,\"summary\",[],[{message}],\"main\"]"),
                true,
            ),
            (format!("[\"source\",null,null,\"summary\",[],[{message}]]"), true),
            (
                format!("[\"source\",null,null,\"summary\",[],[{message}],\"main\",0]"),
                false,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"session\\u0049d\":\"source\",\"messages\":[{message}]}}"),
                false,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"messages\":[]}}"),
                false,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"directories\":[\"ok\",false]}}"),
                false,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"directories\":[\"\\ud800\"]}}"),
                false,
            ),
            (
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"summary\":\"\\ud800\"}}"),
                false,
            ),
            ("".into(), false),
            ("not JSON".into(), false),
            ("null".into(), false),
            (r#"{"sessionId":"source","messages":null}"#.into(), false),
        ];
        for field in ["sessionId", "startTime", "lastUpdated", "summary", "kind"] {
            cases.push((
                format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"{field}\":false}}"),
                false,
            ));
        }
        for (i, (text, accepted)) in cases.into_iter().enumerate() {
            fs::write(&path, &text).unwrap();
            for opts in [stream::Demand::RICH, stream::Demand::LEAN.with_spans()] {
                let mut sink = CollectSink::default();
                let session = read_document(&path, Harness::Gemini, &opts, &mut sink).unwrap();
                assert_eq!(session.is_some(), accepted, "case {i}");
                if accepted {
                    let ids: Vec<_> = sink.messages.iter().map(|m| m.id.as_deref()).collect();
                    assert_eq!(ids, [Some("a")], "case {i}: only the poisoned message is dropped");
                }
            }
        }
    }

    fn json_inputs(session: &Session) -> Vec<&crate::Json> {
        session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { input, .. } => Some(input),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn native_json_arguments_keep_real_values_and_exact_ranges() {
        let world = crate::test_world("gemini-json-inputs");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let big = serde_json::to_string(&"x\nβ".repeat(crate::lazy::INLINE_MAX)).unwrap();
        let args = [
            format!("{{\"payload\":{big},\"duplicate\":1,\"dupl\\u0069cate\":2}}"),
            format!("[{big},null,true,3]"),
            big,
            "true".into(),
            "null".into(),
            "2.5".into(),
            format!("\"{}\"", "x".repeat(crate::lazy::INLINE_MAX - 2)),
            format!("\"{}\"", "x".repeat(crate::lazy::INLINE_MAX - 1)),
        ];
        let mut records: Vec<String> = args.iter().enumerate().map(|(i, arg)| format!(
            "{{\"type\":\"gemini\",\"id\":\"a{i}\",\"toolCalls\":[{{\"name\":\"custom\",\"id\":\"c{i}\",\"args\": {arg} }}]}}"
        )).collect();
        records.push(format!("{{\"type\":\"gemini\",\"id\":\"parts\",\"content\":[{{\"functionCall\":{{\"name\":\"custom\",\"args\": {} }} }}]}}", args[0]));
        records.push(r#"{"type":"gemini","id":"missing","toolCalls":[{"name":"custom"}]}"#.into());
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-source.jsonl"
            } else {
                "session-source.json"
            });
            let text = if modern {
                format!(
                    "{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\r\n \t{}\r\n",
                    records.join("\r\n \t")
                )
            } else {
                format!(
                    "{{\"sessionId\":\"source\",\"messages\":[ {} ]}}",
                    records.join(",\n \t")
                )
            };
            fs::write(&path, &text).unwrap();
            let adapter = Gemini { roots: vec![] };
            let reference = SessionRef::new("source", Harness::Gemini, path.clone());
            let pure = parse_all_str(&text, Some(path.clone())).unwrap().remove(0);
            assert!(json_inputs(&pure).iter().all(|j| matches!(j, crate::Json::Owned(_))));
            for opts in [
                stream::Demand::RICH,
                stream::Demand::LEAN,
                stream::Demand::COMPLETE,
                checked_complete(),
                stream::Demand::LEAN.with_spans(),
                stream::Demand::RICH.with_spans(),
            ] {
                let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
                let inputs = json_inputs(&actual);
                assert_eq!(inputs.len(), args.len() + 2);
                for (i, input) in inputs.iter().enumerate() {
                    let expected_source =
                        opts.spans && (i == args.len() || (i < args.len() && args[i].len() > crate::lazy::INLINE_MAX));
                    assert_eq!(
                        matches!(input, crate::Json::Source(_)),
                        expected_source,
                        "mode {opts:?}, input {i}"
                    );
                    if let crate::Json::Source(range) = input {
                        let actual_raw = &text[range.offset() as usize..(range.offset() + range.len()) as usize];
                        assert_eq!(actual_raw, &args[if i == args.len() { 0 } else { i }]);
                    }
                }
                actual.materialize().unwrap();
                assert_eq!(as_json(&actual), as_json(&pure));
            }
        }
    }

    #[test]
    fn native_json_arguments_survive_set_replacement_rewind_and_invalid_lines() {
        let world = crate::test_world("gemini-json-replay");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-source.jsonl");
        let large = serde_json::json!({"payload": "x\nβ".repeat(5000)});
        let record = |id: &str| serde_json::json!({"type":"gemini","id":id,"content":[{"functionCall":{"name":"custom","id":id,"args":large}}]});
        let lines = [
            serde_json::json!({"sessionId":"source","projectHash":"x","future":large}),
            serde_json::json!({"$set":{"future":large,"summary":"retained summary"}}),
            record("old"),
            serde_json::json!({"$set":{"messages":[record("keep"),record("replace"),record("drop")]}}),
            serde_json::json!({"type":"user","id":"keep","content":"same id, separate kind"}),
            serde_json::json!({"$rewindTo":"drop"}),
            record("replace"),
            record("after"),
        ];
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(format!(" \t{line}\r\n").as_bytes());
            bytes.extend_from_slice(b"invalid JSON\n\xff\n{\"$set\":{\"messages\":[]},\"invalid\":\"\\ud800\"}\n");
        }
        bytes.extend_from_slice(
            "\u{2003}{\"type\":\"user\",\"id\":\"unicode-whitespace\",\"content\":\"keep portable trim\"}\u{2003}\n"
                .as_bytes(),
        );
        fs::write(&path, bytes).unwrap();
        let adapter = Gemini { roots: vec![] };
        let reference = SessionRef::new("source", Harness::Gemini, path);
        let expected = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH).unwrap();
        let mut actual = crate::stream::collect_with(&adapter, &reference, &stream::Demand::LEAN.with_spans()).unwrap();
        assert_eq!(json_inputs(&actual).len(), 3);
        assert!(json_inputs(&actual).iter().all(|j| matches!(j, crate::Json::Source(_))));
        actual.materialize().unwrap();
        assert_eq!(as_json(&actual), as_json(&expected));
    }

    #[test]
    fn selected_composite_results_and_thoughts_match_owned_and_keep_source_guards() {
        let world = crate::test_world("gemini-selected-results");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let body = "escaped\n\"quote\" β😀".repeat(7000);
        let assistant = serde_json::json!({"type":"gemini","id":"a","content":[{"text":"duplicate thought","thought":true}],"thoughts":[{"subject":"subject","description":body},{"subject":"","description":body},{"subject":body,"description":""},null],"toolCalls":[
            {"id":"composite","name":"custom","args":{},"status":"success","result":[null,{"text":""},{"text":body},{"functionResponse":{"response":{"output":body}}},{"future":body}]},
            {"id":"canonical","name":"custom","status":"success","result":{"z":body,"a":1}},
            {"id":"empty","name":"custom","status":"error","result":{"functionResponse":null},"resultDisplay":"unused"}
        ]});
        let adapter = Gemini { roots: vec![] };
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-selected.jsonl"
            } else {
                "session-selected.json"
            });
            let bytes = if modern {
                format!("{{\"sessionId\":\"selected\"}}\n{assistant}\n")
            } else {
                serde_json::json!({"sessionId":"selected","messages":[assistant]}).to_string()
            };
            fs::write(&path, &bytes).unwrap();
            let reference = SessionRef::new("selected", Harness::Gemini, path.clone());
            let expected = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH).unwrap();
            let mut actual =
                crate::stream::collect_with(&adapter, &reference, &stream::Demand::LEAN.with_spans()).unwrap();
            assert!(matches!(
                actual.messages[0].content[0],
                Block::Thinking {
                    text: crate::lazy::Text::Source(_),
                    ..
                }
            ));
            actual.materialize().unwrap();
            assert_eq!(as_json(&actual), as_json(&expected));
            let mut retained =
                crate::stream::collect_with(&adapter, &reference, &stream::Demand::LEAN.with_spans()).unwrap();
            fs::write(&path, b"{}").unwrap();
            assert!(retained.materialize().is_err());
            for at in [1, 2] {
                fs::write(&path, &bytes).unwrap();
                let mut calls = 0;
                let mut sink = crate::stream::TryFnSink::new(|_| {
                    calls += 1;
                    if calls == at {
                        fs::write(&path, b"{}").unwrap();
                        anyhow::bail!("consumer cap");
                    }
                    Ok(Flow::Continue)
                });
                let error = adapter
                    .stream(&reference, &stream::Demand::LEAN.with_spans(), &mut sink)
                    .unwrap_err();
                // The shared guard preserves the first failure: a fast callback can report
                // its own error before a backing check. Neither case may deliver another turn.
                let diagnostic = format!("{error:#}");
                assert!(
                    diagnostic.contains("changed") || diagnostic.contains("consumer cap"),
                    "{diagnostic}"
                );
                assert_eq!(calls, at);
            }
        }
    }

    #[test]
    fn native_json_source_mutation_stops_before_paired_result_and_at_completion() {
        let world = crate::test_world("gemini-json-errors");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        struct Delete {
            path: PathBuf,
            at: usize,
            calls: usize,
        }
        impl MessageSink for Delete {
            fn message(&mut self, _: Message) -> Flow {
                self.calls += 1;
                if self.calls == self.at {
                    fs::remove_file(&self.path).unwrap();
                    // The contract: a callback slow enough to mutate the source is rechecked on
                    // return; a fast one is the EOF check's.
                    std::thread::sleep(crate::harness::store::jsonl::Read::SLOW_CALLBACK * 10);
                }
                Flow::Continue
            }
        }
        let large = serde_json::json!({"payload":"x".repeat(6000)});
        let assistant = serde_json::json!({"type":"gemini","id":"a","toolCalls":[{"id":"c","name":"custom","args":large,"status":"success","result":"done"}]});
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-source.jsonl"
            } else {
                "session-source.json"
            });
            let user = serde_json::json!({"type":"user","id":"u","content":"later"});
            let text = if modern {
                format!("{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\n{assistant}\n{user}\n")
            } else {
                serde_json::json!({"sessionId":"source","messages":[assistant,user]}).to_string()
            };
            for at in [1, 3] {
                fs::write(&path, &text).unwrap();
                let mut sink = Delete {
                    path: path.clone(),
                    at,
                    calls: 0,
                };
                let result = Gemini { roots: vec![] }.stream(
                    &SessionRef::new("source", Harness::Gemini, path.clone()),
                    &stream::Demand::LEAN.with_spans(),
                    &mut sink,
                );
                assert!(result.is_err());
                assert_eq!(sink.calls, at);
            }
        }
    }

    #[test]
    fn native_json_arguments_feed_consumers_and_fail_when_detached_source_changes() {
        let world = crate::test_world("gemini-json-consumers");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-source.jsonl");
        let tools = [
            ("read_file", serde_json::json!({"absolute_path":"/tmp/a.rs"})),
            ("run_shell_command", serde_json::json!({"command":"echo hello"})),
            (
                "write_todos",
                serde_json::json!({"todos":[{"description":"ship", "status":"pending"}]}),
            ),
        ];
        let text = "{\"sessionId\":\"source\",\"projectHash\":\"x\"}\n".to_owned() + &tools.into_iter().enumerate().map(|(i, (name, mut args))| {
            args.as_object_mut().unwrap().insert("ignored_payload".into(), Value::String("x\nβ".repeat(6000)));
            serde_json::json!({"type":"gemini","id":i.to_string(),"toolCalls":[{"id":i.to_string(),"name":name,"args":args}]}).to_string() + "\n"
        }).collect::<String>();
        fs::write(&path, text).unwrap();
        let adapter = Gemini { roots: vec![] };
        let reference = SessionRef::new("source", Harness::Gemini, path.clone());
        let full = crate::stream::collect_with(&adapter, &reference, &stream::Demand::RICH).unwrap();
        let mut expected = crate::events::EventSink::new(None);
        for message in &full.messages {
            expected.try_message(message.clone()).unwrap();
        }
        let mut actual = crate::events::EventSink::new(None);
        adapter
            .stream(&reference, &stream::Demand::LEAN.with_spans(), &mut actual)
            .unwrap();
        assert_eq!(actual.into_events().unwrap(), expected.into_events().unwrap());
        assert_eq!(
            serde_json::to_value(crate::affordance::read(&adapter, &reference).unwrap()).unwrap(),
            serde_json::to_value(crate::affordance::of_session(&full).unwrap()).unwrap()
        );
        let lazy = crate::stream::collect_with(&adapter, &reference, &stream::Demand::LEAN.with_spans()).unwrap();
        assert!(json_inputs(&lazy).iter().all(|j| matches!(j, crate::Json::Source(_))));
        fs::remove_file(path).unwrap();
        assert!(json_inputs(&lazy)[0].resolve().is_err());
        let mut events = crate::events::EventSink::new(None);
        assert!(events.try_message(lazy.messages[0].clone()).is_err());
    }

    /// One `logs.json` holds three sessions: discovery, every session's read and the store's
    /// certificates share one parse per file state; an entry appended to one session moves its
    /// certificate alone, and the others read exactly as before (their cwd from the registry).
    #[test]
    fn a_logs_json_append_moves_one_certificate_and_parses_once_per_state() {
        let world = crate::test_world("gemini-logs-rows");
        let runtime = world.base.join(".gemini");
        let file = runtime.join("tmp/proj/logs.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(runtime.join("projects.json"), r#"{"projects":{"/work/proj":"proj"}}"#).unwrap();
        let entry = |sid: &str, id: i64, text: &str| {
            serde_json::json!({"sessionId":sid,"messageId":id,"type":"user","message":text,
                "timestamp":"2026-01-01T00:00:00Z"})
        };
        let mut entries: Vec<Value> = ["a", "b", "c"]
            .iter()
            .flat_map(|sid| {
                [
                    entry(sid, 0, &format!("{sid} first")),
                    entry(sid, 1, &format!("{sid} second")),
                ]
            })
            .collect();
        fs::write(&file, serde_json::to_string_pretty(&entries).unwrap()).unwrap();
        let parsed = || {
            let mut all = LOGS_PARSED.lock().unwrap();
            let mine = all.iter().filter(|p| **p == file).count();
            all.retain(|p| *p != file);
            mine
        };
        let gemini = Gemini::with_home(&runtime);
        let read = |r: &SessionRef| serde_json::to_string(&gemini.parse(r).unwrap()).unwrap();
        let refs = gemini.discover().unwrap().refs;
        assert_eq!(refs.len(), 3);
        let before = gemini.rows(&file).unwrap();
        let reads: Vec<String> = refs.iter().map(read).collect();
        assert_eq!(parsed(), 1, "discovery, three reads and the certificates");
        assert!(refs.iter().all(|r| r.cwd.as_deref() == Some(Path::new("/work/proj"))));
        entries.push(entry("b", 2, "b third"));
        fs::write(&file, serde_json::to_string_pretty(&entries).unwrap()).unwrap();
        let after = gemini.rows(&file).unwrap();
        for (r, old) in refs.iter().zip(&reads) {
            if r.id == "b" {
                assert_ne!(after[&r.id], before[&r.id]);
                assert_ne!(read(r), *old);
            } else {
                assert_eq!(after[&r.id], before[&r.id], "{}", r.id);
                assert_eq!(read(r), *old, "{}", r.id);
            }
        }
        assert_eq!(parsed(), 1, "one parse of the new state");
    }

    #[test]
    fn parses_logs_json_grouped_by_session() {
        let text = fixture("logs.json");
        let mut sessions = parse_all_str(&text, None).unwrap();
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
        let sessions = parse_all_str(&text, None).unwrap();
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
            s.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .all(|m| m.model.as_deref() == Some("gemini-3-flash-preview")),
            "every assistant record states its model"
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
        let text = fixture("session_ignored_user_content.jsonl");
        let sessions = parse_all_str(&text, None).unwrap();
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
    fn every_turn_states_the_model_its_record_names() {
        let text = fixture("session_model_fallback.jsonl");
        let s = &parse_all_str(&text, None).unwrap()[0];
        assert_eq!(s.model.as_deref(), Some("gemini-3-pro"));
        assert_eq!(s.messages[1].model.as_deref(), Some("gemini-3-pro"));
        assert_eq!(s.messages[2].model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn windowed_reads_keep_the_model() {
        let _world = crate::test_world("gemini-windowed");
        let home = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/gemini/0.63.0/.gemini");
        crate::harness::assert_windowed_models(&Gemini::with_home(&home));
    }

    #[test]
    fn rewind_index_matches_linear_first_wire_id_oracle_on_long_history() {
        let mut replay = RecordingReplay::default();
        let mut order = Vec::<String>::new();
        let mut bodies = BTreeMap::<String, Body>::new();
        let insert = |replay: &mut RecordingReplay,
                      order: &mut Vec<String>,
                      bodies: &mut BTreeMap<String, Body>,
                      key: String| {
            if !bodies.contains_key(&key) {
                order.push(key.clone());
            }
            bodies.insert(key.clone(), Body::Range(0, 1));
            replay.insert(key, Body::Range(0, 1));
        };
        for index in 0..4096 {
            insert(
                &mut replay,
                &mut order,
                &mut bodies,
                format!("user\u{1f}prefix-{index}"),
            );
        }
        for turn in 1..=128 {
            let target = format!("tail-{turn}");
            for kind in ["user", "gemini"] {
                insert(&mut replay, &mut order, &mut bodies, format!("{kind}\u{1f}{target}"));
            }
            // Replacement must keep the first occurrence's index.
            insert(&mut replay, &mut order, &mut bodies, format!("user\u{1f}{target}"));
            let position = order.iter().position(|key| key_id(key) == target).unwrap();
            for key in order.drain(position..) {
                bodies.remove(&key);
            }
            let marker_id = format!("$rewind-{turn}");
            let marker = serde_json::json!({"id":marker_id,"type":"rewind","rewindTo":target});
            order.push(marker_id.clone());
            bodies.insert(marker_id, Body::Owned(marker));
            replay.rewind(&target);
        }
        assert_eq!(replay.order, order);
        assert_eq!(replay.msgs, bodies);
        assert_eq!(replay.first_positions.len(), order.len());
        for (id, position) in &replay.first_positions {
            assert_eq!(key_id(&order[*position]), id);
        }
        replay.clear_messages();
        assert!(replay.first_positions.is_empty());
        replay.insert("user\u{1f}prefix-0".into(), Body::Range(0, 1));
        replay.rewind("prefix-0");
        assert_eq!(replay.order, ["$rewind-129"]);
        replay.rewind("missing");
        assert_eq!(replay.order, ["$rewind-130"]);
    }

    #[test]
    fn replay_controls_keep_their_precedence() {
        let mut replay = RecordingReplay::default();
        let ingest = |replay: &mut RecordingReplay, record: Value| replay.ingest((&record).into()).unwrap();
        ingest(
            &mut replay,
            serde_json::json!({"$set": {
                "messages": [
                    {"id":"same", "type":"user", "content":"obsolete"},
                    {"id":"same", "type":"user", "content":"kept"},
                    {"type":"user", "content":"missing id"}
                ],
                "summary":"checkpoint summary"
            }}),
        );
        assert_eq!(replay.order, ["user\u{1f}same"]);
        assert_eq!(
            replay.msgs["user\u{1f}same"],
            Body::Owned(serde_json::json!({"id":"same", "type":"user", "content":"kept"}))
        );
        // A rewind wins over a `$set` in the same record.
        ingest(
            &mut replay,
            serde_json::json!({"$rewindTo":"same", "$set":{"summary":"ignored"}}),
        );
        assert_eq!(replay.metadata["summary"], "checkpoint summary");
        // A non-object `$set` leaves a message record a message.
        ingest(
            &mut replay,
            serde_json::json!({"$set":false,"id":"fallback","type":"user","content":"kept"}),
        );
        assert!(replay.msgs.contains_key("user\u{1f}fallback"));
        ingest(
            &mut replay,
            serde_json::json!({"$set":{"messages":false,"summary":null}}),
        );
        assert!(replay.order.is_empty());
        assert!(replay.msgs.is_empty());
        assert_eq!(replay.metadata["summary"], Value::Null);
    }

    #[test]
    fn parses_modern_jsonl_with_rewind_and_set() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_all_str(&text, None).unwrap();
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
        assert!(
            s.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .all(|m| m.model.as_deref() == Some("gemini-3-pro")),
            "one model throughout, stated on every assistant record"
        );
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
        let s = parse_checkpoint(Harness::Gemini, &text, "checkpoint-my%20tag.json", None)
            .unwrap()
            .expect("checkpoint");
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
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content.matches_str("4") == Some(true))));
    }

    #[test]
    fn a_project_registered_after_the_first_lookup_resolves() {
        let world = crate::test_world("gemini-registry-refresh");
        let runtime = world.base.join("runtime");
        // Each change gets its own whole-second mtime: a filesystem with a coarse clock could
        // otherwise stamp two steps alike.
        let at = |path: &Path, second: u64| {
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + second);
            fs::File::open(path).unwrap().set_modified(when).unwrap();
        };
        let mark = |id: &str, root: &str, second: u64| {
            let dir = runtime.join("tmp").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".project_root"), root).unwrap();
            at(&runtime.join("tmp"), second);
        };
        mark("first", "/work/first", 1);
        assert_eq!(registry_for(&runtime).get("first"), Some(&PathBuf::from("/work/first")));
        mark("second", "/work/second", 2);
        assert_eq!(
            registry_for(&runtime).get("second"),
            Some(&PathBuf::from("/work/second"))
        );
        fs::write(runtime.join("projects.json"), r#"{"projects":{"/work/moved":"first"}}"#).unwrap();
        at(&runtime.join("projects.json"), 3);
        assert_eq!(registry_for(&runtime).get("first"), Some(&PathBuf::from("/work/moved")));
    }

    /// Qwen writes gemini-cli's checkpoint shape; what cv derives from one belongs in Qwen's bag
    /// and on Qwen's messages, never Gemini's.
    #[test]
    fn a_qwen_checkpoint_keeps_its_facts_under_qwen() {
        let text = fixture("checkpoint.json");
        let s = parse_checkpoint(Harness::Qwen, &text, "checkpoint-q.json", None)
            .unwrap()
            .expect("checkpoint");
        assert_eq!(s.harness, Harness::Qwen);
        assert!(s.extra.contains_key(Harness::Qwen.as_str()), "{:?}", s.extra.keys());
        assert!(!s.extra.contains_key(Harness::Gemini.as_str()), "{:?}", s.extra.keys());
        for m in &s.messages {
            assert!(!m.extra.contains_key(Harness::Gemini.as_str()), "{:?}", m.extra);
        }
    }

    #[test]
    fn parses_legacy_array_checkpoint_via_parse_all() {
        let text = fixture("checkpoint_legacy_array.json");
        let sessions = parse_all_str(&text, None).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(s.title.as_deref(), Some("hi"));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_all_str("not json at all", None).unwrap().is_empty());
        assert!(parse_all_str("{}", None).unwrap().is_empty());
        assert!(parse_all_str("[]", None).unwrap().is_empty());
    }

    // ── on-disk streaming paths ──────────────────────────────────────────────
    //
    // `is_chat_recording` keys off a `chats/` path component, so these write each fixture into a
    // unique temp `chats/` dir, then drive the real `Adapter::parse` (= `stream` → `collect`) and the
    // native `Adapter::stream`, asserting both reproduce the pure `parse_all_str` Session byte-for-
    // byte (Session has no `PartialEq`, so we compare its JSON serialization).

    fn chats_fixture(name: &str) -> (cv_testkit::World, PathBuf) {
        let world = cv_testkit::World::new("gemini-stream");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(name);
        fs::write(&file, fixture(name)).unwrap();
        (world, file)
    }

    fn as_json(s: &Session) -> Value {
        serde_json::to_value(s).unwrap()
    }

    /// Drive the on-disk `parse` and `stream` for a chat-recording fixture and assert both equal the
    /// pure `parse_all_str` Session. `source_path` differs (on-disk paths carry provenance), so we
    /// blank it before comparing.
    fn assert_stream_matches_pure(name: &str) {
        let (_world, file) = chats_fixture(name);
        let id = parse_all_str(&fixture(name), None)
            .unwrap()
            .into_iter()
            .next()
            .expect("fixture parses to one session")
            .id;
        let r = SessionRef::new(id.clone(), Harness::Gemini, file.clone());
        let adapter = Gemini { roots: vec![] };

        // Reference: the pure parser, with source_path set to the on-disk file for an apples-to-
        // apples comparison.
        let reference = parse_all_str(&fixture(name), Some(file.clone())).unwrap();
        let reference = reference.into_iter().next().unwrap();

        let via_parse = adapter.parse(&r).expect("on-disk parse");
        assert_eq!(
            as_json(&via_parse),
            as_json(&reference),
            "parse() must stay byte-identical to the pure parser for {name}"
        );

        // Native stream and explicit checked complete preparation retain the existing codec.
        for opts in [stream::Demand::RICH, stream::Demand::COMPLETE, checked_complete()] {
            let mut sink = CollectSink::default();
            let mut streamed = adapter.stream(&r, &opts, &mut sink).expect("on-disk stream");
            assert!(streamed.messages.is_empty(), "stream returns empty messages");
            streamed.messages = sink.messages;
            streamed.materialize().unwrap();
            assert_eq!(
                serde_json::to_vec(&streamed).unwrap(),
                serde_json::to_vec(&reference).unwrap(),
                "stream() must reassemble the same Session for {name}, {opts:?}"
            );
        }
    }

    #[test]
    fn streams_legacy_json_recording_on_disk() {
        // The mmap + RawValue single-document path.
        assert_stream_matches_pure("session_legacy.json");
    }

    #[test]
    fn legacy_stream_preserves_escaped_metadata_and_kind() {
        let (_world, file) = chats_fixture("session_legacy.json");
        let text = r#"{"sessionId":"escaped\"id","startTime":"2026-01-01T00:00:00Z","summary":"line one\nline two","directories":["/tmp/quoted\"path"],"kind":"subagent","messages":[{"type":"user","id":"u","content":"hello"}]}"#;
        fs::write(&file, text).unwrap();
        let mut sink = CollectSink::default();
        // Call this path directly: a fallback would hide the borrowing failure.
        let mut streamed = read_document(&file, Harness::Gemini, &stream::Demand::RICH, &mut sink)
            .unwrap()
            .expect("escaped strings are a legacy recording");
        streamed.messages = sink.messages;
        assert_eq!(
            as_json(&streamed),
            as_json(&parse_all_str(text, Some(file.clone())).unwrap().remove(0))
        );
    }

    #[test]
    fn streams_modern_jsonl_recording_on_disk() {
        // The line-buffered JSONL replay path (with $set / $rewindTo).
        assert_stream_matches_pure("session_modern.jsonl");
    }

    #[test]
    fn lazy_legacy_content_resolves_identically() {
        let (_world, file) = chats_fixture("session_legacy.json");
        let giant = "giant output \n\t\"\\🌙".repeat(2000);
        let input = serde_json::json!({
            "sessionId": "lazy", "projectHash": "x", "kind": "subagent", "summary": "escaped\nsummary",
            "messages": [
                {"type":"user", "id":"u", "content":[{"text":giant}, {"text":format!("<hook_context>{giant}")}]},
                {"type":"gemini", "id":"a", "content":[{"text":giant,"thought":true},{"text":giant}],
                 "toolCalls":[{"id":"c","name":"shell","args":{"command":"ls"},"status":"success",
                     "result":[{"functionResponse":{"response":{"output":giant}}}]}]},
                {"type":"user","id":"t","content":[{"functionResponse":{"id":"c","name":"shell","response":{"output":giant}}}]},
                {"type":"warning","id":"w","content":giant},
                {"type":"user","id":"white","content":format!("{}<session_context>hidden marker", " ".repeat(9000))}
            ]
        });
        let text = serde_json::to_string(&input).unwrap();
        fs::write(&file, &text).unwrap();
        let mut sink = CollectSink::default();
        let mut s = Gemini { roots: vec![] }
            .stream(
                &SessionRef::new("lazy", Harness::Gemini, file.clone()),
                &stream::Demand::LEAN.with_spans(),
                &mut sink,
            )
            .unwrap();
        s.messages = sink.messages;
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.is_lazy())));
        s.materialize().unwrap();
        assert_eq!(
            as_json(&s),
            as_json(&parse_all_str(&text, Some(file.clone())).unwrap().remove(0))
        );
    }

    #[test]
    fn raw_jsonl_replay_and_lazy_fields_match_pure() {
        let (_world, file) = chats_fixture("session_modern.jsonl");
        let giant = "escaped\n\t\"\\🌙".repeat(1200);
        let lines = [
            serde_json::json!({"sessionId":"replay","projectHash":"x","directories":["/tmp/q\"uote"]}),
            serde_json::json!({"type":"user","id":"u","content":"superseded"}),
            serde_json::json!({"type":"gemini","id":"same","content":giant}),
            serde_json::json!({"type":"user","id":"same","content":"same id different kind"}),
            serde_json::json!({"$set":{"summary":"new\nsummary","messages":[
                {"type":"user","id":"u","content":giant},
                {"type":"gemini","id":"same","content":giant},
                {"type":"user","id":"same","content":"paired"}]}}),
            serde_json::json!({"type":"gemini","id":"same","content":"replacement","toolCalls":[
                {"id":"a","name":"shell","status":"success","result":{"text":"json object text"}},
                {"id":"b","name":"shell","status":"success","result":[{"text":"first"},{"text":giant}]},
                {"id":"c","name":"shell","status":"success","resultDisplay":giant}]}),
            serde_json::json!({"$rewindTo":"same"}),
            serde_json::json!({"type":"user","id":"after","content":[{"functionCall":null,"text":giant}]}),
        ];
        for end in [6, lines.len()] {
            let text = lines[..end].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
            fs::write(&file, &text).unwrap();
            let reference = parse_all_str(&text, Some(file.clone())).unwrap().remove(0);
            for opts in [
                stream::Demand::RICH,
                stream::Demand::LEAN.with_spans(),
                stream::Demand::COMPLETE,
                checked_complete(),
            ] {
                let mut sink = CollectSink::default();
                let mut s = Gemini { roots: vec![] }
                    .stream(
                        &SessionRef::new("replay", Harness::Gemini, file.clone()),
                        &opts,
                        &mut sink,
                    )
                    .unwrap();
                s.messages = sink.messages;
                s.materialize().unwrap();
                assert_eq!(as_json(&s), as_json(&reference));
            }
            let refs = scan_session_file(&file, Harness::Gemini).unwrap();
            assert_eq!(refs.len(), 1);
            assert_eq!(
                refs[0].message_count,
                Some(
                    reference
                        .messages
                        .iter()
                        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                        .count()
                )
            );
        }
    }

    #[test]
    fn raw_replay_rejects_invalid_values_before_navigation() {
        let (_world, file) = chats_fixture("session_modern.jsonl");
        let deep = format!("{}0{}", "[".repeat(127), "]".repeat(127));
        for unknown in ["1e400", r#""\ud800""#, r#""\udc00""#, &deep] {
            let text = format!(
                "{{\"sessionId\":\"validation\",\"projectHash\":\"x\"}}\n\
                 {{\"type\":\"user\",\"id\":\"u\",\"content\":\"keep\"}}\n\
                 {{\"type\":\"gemini\",\"id\":\"a\",\"content\":\"keep suffix\"}}\n\
                 {{\"type\":\"user\",\"id\":\"u\",\"content\":\"poison\",\"unknown\":{unknown},\"unknown\":0}}\n\
                 {{\"$rewindTo\":\"u\",\"unknown\":{unknown}}}\n\
                 {{\"$set\":{{\"messages\":[]}},\"unknown\":{unknown}}}"
            );
            fs::write(&file, &text).unwrap();
            let reference = parse_all_str(&text, Some(file.clone())).unwrap().remove(0);
            assert_eq!(reference.messages.len(), 2);
            for opts in [
                stream::Demand::LEAN.with_spans(),
                stream::Demand::RICH,
                stream::Demand::COMPLETE,
                checked_complete(),
            ] {
                let mut sink = CollectSink::default();
                let mut streamed = read_jsonl(&file, Harness::Gemini, &opts, &mut sink).unwrap().unwrap();
                streamed.messages = sink.messages;
                streamed.materialize().unwrap();
                assert_eq!(as_json(&streamed), as_json(&reference), "invalid field {unknown}");
            }
        }
        // Legacy streaming has always rejected individual invalid records, independently of
        // the whole-file wrapper's depth. The lazy path must match that full native path.
        let text = format!(
            r#"{{"sessionId":"validation","messages":[{{"type":"user","id":"u","content":"keep"}},{{"type":"user","id":"bad","content":"{}\ud800"}}]}}"#,
            "x".repeat(9000)
        );
        fs::write(&file, text).unwrap();
        let mut sessions = Vec::new();
        for opts in [
            stream::Demand::LEAN.with_spans(),
            stream::Demand::RICH,
            stream::Demand::COMPLETE,
            checked_complete(),
        ] {
            let mut sink = CollectSink::default();
            let mut streamed = read_document(&file, Harness::Gemini, &opts, &mut sink)
                .unwrap()
                .unwrap();
            streamed.messages = sink.messages;
            streamed.materialize().unwrap();
            assert_eq!(streamed.messages.len(), 1);
            sessions.push(as_json(&streamed));
        }
        assert!(sessions.windows(2).all(|s| s[0] == s[1]));
    }

    #[test]
    fn stream_honors_stop_early() {
        // A sink returning Stop after the first message halts the legacy-JSON streaming path without
        // emitting the rest.
        let (_world, file) = chats_fixture("session_legacy.json");
        let r = SessionRef::new("9aeb2942-7c46-47b7-aded-13772d4d4e63", Harness::Gemini, file);
        let adapter = Gemini { roots: vec![] };
        let mut seen = 0usize;
        let mut sink = |_m: Message| {
            seen += 1;
            Flow::Stop
        };
        let _ = adapter.stream(&r, &stream::Demand::RICH, &mut sink).unwrap();
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
    }

    /// A fake gemini-cli runtime root (`<runtime>/tmp/<id>/chats/<recording>` + registry files).
    fn runtime_fixture(tag: &str, project_id: &str, recording: &str) -> (cv_testkit::World, PathBuf) {
        let world = cv_testkit::World::new(&format!("gemini-{tag}"));
        let chats = world.base.join("tmp").join(project_id).join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(recording);
        fs::write(&file, fixture(recording)).unwrap();
        (world, file)
    }

    #[test]
    fn discovers_both_runtime_roots_and_dedupes_by_id() {
        // `~/.gemini/tmp` and the Seatbelt root `~/.cache/.gemini/tmp` are both scanned; the same
        // session id under both roots is listed once.
        let (rt_a, _) = runtime_fixture("root-a", "proj", "session_modern.jsonl");
        let (rt_b, _) = runtime_fixture("root-b", "proj", "session_modern.jsonl");
        let (rt_c, _) = runtime_fixture("root-c", "proj", "session_legacy.json");
        let (a, b, c) = (rt_a.base.clone(), rt_b.base.clone(), rt_c.base.clone());
        let adapter = Gemini {
            roots: vec![a.join("tmp"), b.join("tmp"), c.join("tmp")],
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
        assert_eq!(adapter.roots().into_iter().next(), Some(rt_a.base.join("tmp")));
    }

    /// A runtime root that itself lives under a directory named `tmp` — `/tmp/...` on Linux, a
    /// Seatbelt root, a CI temp dir — must still resolve. Reading the components left-to-right
    /// matched that outer `tmp`, so `<runtime>` became `/` and every cwd came back `None`.
    #[test]
    fn runtime_root_under_a_tmp_directory_still_resolves_cwd() {
        let world = cv_testkit::World::new("gemini-outer");
        let outer = world.base.join("outer");
        let runtime = outer.join("tmp").join("gemini-runtime");
        let chats = runtime.join("tmp").join("proj-id").join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join("session_modern.jsonl");
        fs::write(&file, fixture("session_modern.jsonl")).unwrap();
        fs::write(
            runtime.join("projects.json"),
            r#"{"projects": {"/Users/u/work/proj": "proj-id"}}"#,
        )
        .unwrap();

        let refs = scan_session_file(&file, Harness::Gemini).unwrap();
        assert_eq!(
            refs[0].cwd.as_deref(),
            Some(Path::new("/Users/u/work/proj")),
            "the innermost `tmp` is the runtime's own"
        );
    }

    #[test]
    fn cwd_comes_from_the_project_registry() {
        // `projects.json` maps path → short id; `history/<id>/.project_root` is the fallback marker.
        let (runtime, file) = runtime_fixture("registry", "claurdvoyant", "session_modern.jsonl");
        fs::write(
            runtime.base.join("projects.json"),
            r#"{"projects": {"/Users/u/pug/claurdvoyant": "claurdvoyant"}}"#,
        )
        .unwrap();
        let (runtime2, file2) = runtime_fixture("marker", "gemtest", "session_legacy.json");
        fs::create_dir_all(runtime2.base.join("history").join("gemtest")).unwrap();
        fs::write(
            runtime2.base.join("history").join("gemtest").join(".project_root"),
            "/Users/u/cvrt8/gemtest\n",
        )
        .unwrap();
        // hash-named dirs (pre-registry) have no mapping → None, as before
        let (_runtime3, file3) = runtime_fixture("hash", &"ab".repeat(32), "session_modern.jsonl");

        let refs = scan_session_file(&file, Harness::Gemini).unwrap();
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        let refs2 = scan_session_file(&file2, Harness::Gemini).unwrap();
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/cvrt8/gemtest")));
        // session_legacy.json parses to user, assistant, tool, assistant, info-notice: three turns.
        assert_eq!(
            refs2[0].message_count,
            Some(3),
            "user + assistant turns, not every message"
        );
        let refs3 = scan_session_file(&file3, Harness::Gemini).unwrap();
        assert_eq!(refs3[0].cwd, None);

        // the streamed session carries it too (the sink's `meta` sees the cwd)
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &stream::Demand::RICH, &mut sink).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        // a record's own `directories[]` still wins (unchanged behaviour): covered by
        // `parses_legacy_chat_recording` fixtures carrying no registry at all.
    }

    #[test]
    fn registry_files_in_their_real_0_46_shapes_recover_cwd() {
        // Fixtures mirror what gemini-cli 0.46.0 wrote for a fresh project dir on 2026-09-19:
        // `projects.json` pretty-printed with a nested `projects` map, and an identical bare-path
        // `.project_root` (no trailing newline) in BOTH `tmp/<id>/` and `history/<id>/`.
        let (runtime, file) = runtime_fixture("real-shapes", "gemtest-proj", "session_modern.jsonl");
        fs::write(runtime.base.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.base.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        fs::create_dir_all(runtime.base.join("history").join("gemtest-proj")).unwrap();
        fs::write(
            runtime.base.join("history").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini).unwrap();
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));

        // The marker beside the chats is enough on its own (a runtime root with no registry file
        // and no history dir — e.g. one the CLI is still populating).
        let (runtime2, file2) = runtime_fixture("tmp-marker-only", "solo", "session_legacy.json");
        fs::write(
            runtime2.base.join("tmp").join("solo").join(".project_root"),
            "/Users/u/work/solo",
        )
        .unwrap();
        let refs2 = scan_session_file(&file2, Harness::Gemini).unwrap();
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/work/solo")));
    }

    #[test]
    fn real_0_46_recording_keeps_the_context_preamble_but_never_titles_from_it() {
        // Written by gemini-cli 0.46.0 on 2026-09-19 (paths neutralized): a metadata line, then ONE
        // `$set` carrying the whole `messages` array, whose only user turn is the injected
        // `<session_context>` preamble. gemini-cli ignores such content when rebuilding history
        // (`isIgnoredUserContent`), so it must not become the title; the turn itself stays.
        let (runtime, file) = runtime_fixture("real-0-46", "gemtest-proj", "session_0_46_session_context.jsonl");
        fs::write(runtime.base.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.base.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "ad7102ea-0b4f-4822-94f7-f3bbf67f1614");
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));
        assert_eq!(refs[0].title, None, "the <session_context> preamble is not a prompt");

        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &stream::Demand::RICH, &mut sink).unwrap();
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
    }

    #[test]
    fn subagent_recording_points_at_its_parent() {
        // A sub-agent recording lives under `chats/<parentId>/<id>.jsonl` with `kind: "subagent"`
        // (`chatRecordingService.ts`); its parent is the recording one level up.
        let (runtime, _) = runtime_fixture("subagent", "proj", "session_modern.jsonl");
        let parent_id = "11112222-3333-4444-5555-666677778888";
        let child_dir = runtime.base.join("tmp").join("proj").join("chats").join(parent_id);
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

        let refs = scan_session_file(&child, Harness::Gemini).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "aaaa0000-0000-0000-0000-000000000001");
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &stream::Demand::RICH, &mut sink).unwrap();
        assert_eq!(
            s.lineage,
            Lineage {
                parent: Some(parent_id.into()),
                ..Default::default()
            }
        );
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "subagent");
        // the parent's own recording, one level up, has no parent
        let parent_refs = scan_session_file(
            &runtime.base.join("tmp/proj/chats/session_modern.jsonl"),
            Harness::Gemini,
        )
        .unwrap();
        let mut sink = CollectSink::default();
        let p = adapter
            .stream(&parent_refs[0], &stream::Demand::RICH, &mut sink)
            .unwrap();
        assert!(p.lineage.is_empty());
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
            roots: vec![runtime.base.join("tmp")],
        };
        let refs = adapter.discover().unwrap();
        assert_eq!(
            refs.len(),
            1,
            "only the real recording is discovered: {:?}",
            refs.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        assert!(refs[0].path.ends_with("session_modern.jsonl"));
    }

    /// The 0.63.0 resume shape: a `$set.messages` re-sync from the API history carries the
    /// assistant turn's thoughts and calls as parts AND as `thoughts[]`/`toolCalls[]`, a `user`
    /// tool-response turn sharing the assistant's id, and the original tool-response record.
    #[test]
    fn resynced_history_neither_doubles_nor_overwrites_turns() {
        let call = r#"{"id":"c1","name":"read_file","args":{"file_path":"a.txt"},"status":"success","timestamp":"2026-10-11T00:00:02.000Z","result":[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"alpha"}}}]}"#;
        let fr = r#"[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"alpha"}}}]"#;
        let gem = format!(
            r#"{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"gemini","content":[{{"text":"thinking","thought":true}},{{"functionCall":{{"id":"c1","name":"read_file","args":{{}}}}}}],"thoughts":[{{"subject":"","description":"thinking","timestamp":"2026-10-11T00:00:02.000Z"}}],"toolCalls":[{call}],"model":"gemini-2.5-flash"}}"#
        );
        let text = [
            r#"{"sessionId":"s1","projectHash":"h","startTime":"2026-10-11T00:00:00.000Z","lastUpdated":"2026-10-11T00:00:00.000Z","kind":"main"}"#.to_string(),
            r#"{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{"text":"read a.txt"}]}"#.to_string(),
            gem.clone(),
            format!(r#"{{"id":"t1","timestamp":"2026-10-11T00:00:03.000Z","type":"user","content":{fr}}}"#),
            r#"{"id":"g2","timestamp":"2026-10-11T00:00:04.000Z","type":"gemini","content":"alpha","model":"gemini-2.5-flash"}"#.to_string(),
            format!(
                r#"{{"$set":{{"messages":[{{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{{"text":"read a.txt"}}]}},{gem},{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"user","content":{fr}}},{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"user","content":{fr}}},{{"id":"t1","timestamp":"2026-10-11T00:00:03.000Z","type":"user","content":{fr}}},{{"id":"g2","timestamp":"2026-10-11T00:00:04.000Z","type":"gemini","content":[{{"text":"alpha"}}],"model":"gemini-2.5-flash"}}]}}}}"#
            ),
        ]
        .join("\n");
        let s = &parse_all_str(&text, None).unwrap()[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        let reply = &s.messages[1];
        let n = |f: fn(&Block) -> bool| reply.content.iter().filter(|b| f(b)).count();
        assert_eq!(
            n(|b| matches!(b, Block::Thinking { .. })),
            1,
            "thought part and thoughts[] are one"
        );
        assert_eq!(
            n(|b| matches!(b, Block::ToolUse { .. })),
            1,
            "functionCall part and toolCalls[] are one"
        );
        assert_eq!(s.messages[2].content.len(), 1, "one result, not three");
    }

    /// A `user` record is a tool-response turn only when its content is a nonempty array whose
    /// every part has a `functionResponse` key, whatever that key's value; `displayContent` is a
    /// string, an object's `text`, or the concatenated strings and `text`s of a part list.
    #[test]
    fn tool_response_turns_and_display_text_follow_the_part_list_rules() {
        let response =
            serde_json::json!({"functionResponse":{"id":"call","name":"read","response":{"output":"result"}}});
        let cases = [
            (Value::Null, vec![]),
            (serde_json::json!([]), vec![]),
            (serde_json::json!([response]), vec![Role::Tool]),
            (serde_json::json!([response, {"text":"mixed prompt"}]), vec![Role::User]),
            (serde_json::json!(["", response]), vec![Role::User]),
            (
                serde_json::json!([{ "functionResponse":null, "text":"presence is authoritative" }]),
                vec![Role::Tool],
            ),
            (
                serde_json::json!([{ "functionResponse":false, "functionCall":{"id":"call","name":"read","args":{}} }]),
                vec![Role::Tool],
            ),
            (response.clone(), vec![Role::User]),
        ];
        for (content, roles) in cases {
            let record = serde_json::json!({"id":"u", "type":"user", "content":content});
            let text = serde_json::json!({"sessionId":"source", "messages":[record]}).to_string();
            let session = parse_all_str(&text, None).unwrap().remove(0);
            let actual: Vec<_> = session.messages.iter().map(|m| m.role).collect();
            assert_eq!(actual, roles, "content={content}");
        }
        let body = "display é\n\"quoted\" ";
        for (display, expected) in [
            (Value::String(body.into()), Some(body.to_string())),
            (
                serde_json::json!({"text":body,"ignored":"not display"}),
                Some(body.into()),
            ),
            (
                serde_json::json!(["prefix", {"text":body}, null, {"text":false}, ["ignored"], "suffix"]),
                Some(format!("prefix{body}suffix")),
            ),
            (serde_json::json!(["", {"text":""}]), None),
            (serde_json::json!({"text":false}), None),
            (Value::Null, None),
        ] {
            let record = serde_json::json!({"id":"u", "type":"user", "content":"prompt", "displayContent":display});
            let text = serde_json::json!({"sessionId":"source", "messages":[record]}).to_string();
            let session = parse_all_str(&text, None).unwrap().remove(0);
            let actual = session.messages[0]
                .harness_extra(Harness::Gemini)
                .and_then(|bag| bag.get("display_text"))
                .and_then(Value::as_str)
                .map(str::to_string);
            assert_eq!(actual, expected, "display={display}");
        }
    }

    #[test]
    fn subagent_call_hook_context_and_display_text() {
        let text = [
            r#"{"sessionId":"s2","projectHash":"h","startTime":"2026-10-11T00:00:00.000Z","lastUpdated":"2026-10-11T00:00:00.000Z","kind":"main"}"#,
            r#"{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{"text":"Reply with exactly: hi"},{"text":"<hook_context>be brief</hook_context>"}],"displayContent":[{"text":"/hello"}]}"#,
            r#"{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"gemini","content":"","toolCalls":[{"id":"a1","name":"invoke_agent","args":{"agent_name":"codebase_investigator"},"status":"success","agentId":"child-1","timestamp":"2026-10-11T00:00:03.000Z","result":[{"functionResponse":{"id":"a1","name":"invoke_agent","response":{"output":"done"}}}]}]}"#,
        ]
        .join("\n");
        let s = &parse_all_str(&text, None).unwrap()[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::System, MessageKind::InjectedContext, Origin::Hook),
                (Role::Assistant, MessageKind::SubagentSpawn, Origin::Model),
                (Role::Tool, MessageKind::SubagentReturn, Origin::Subagent),
            ]
        );
        let ex = s.messages[0].harness_extra(Harness::Gemini).unwrap();
        assert_eq!(ex["display_text"], "/hello");
        assert_eq!(
            s.messages[2].harness_extra(Harness::Gemini).unwrap()["subagent_ids"][0],
            "child-1"
        );
        assert_eq!(s.title.as_deref(), Some("Reply with exactly: hi"));
    }

    #[test]
    fn a_startup_stub_never_shadows_the_session_it_shares_an_id_with() {
        let mk = |n: usize, mins: i64| {
            let mut r = SessionRef::new("same", Harness::Gemini, format!("/x/{n}.jsonl"));
            r.message_count = Some(n);
            r.updated_at = Some(chrono::DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(mins));
            r
        };
        let kept = dedupe_by_id(vec![mk(4, 1), mk(0, 2)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message_count, Some(4), "the newer stub lost");
        assert_eq!(
            dedupe_by_id(vec![mk(2, 1), mk(3, 2)])[0].message_count,
            Some(3),
            "newer real copy wins"
        );
    }

    #[test]
    fn a_merged_checkpoint_turn_splits_the_preamble_from_the_prompt() {
        let text = r#"{"history":[{"role":"user","parts":[{"text":"<session_context>\nctx\n</session_context>"},{"text":"Say saved."}]},{"role":"model","parts":[{"text":"saved"}]}]}"#;
        let s = parse_checkpoint(Harness::Gemini, text, "checkpoint-x.json", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            kinds(&s),
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.title.as_deref(), Some("Say saved."));
    }
}
