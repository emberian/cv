//! clustervision desktop app — the Tauri v2 runtime.
//!
//! This crate is a thin native shell around the *existing* static web UI (`<repo>/web/`, wired via
//! `frontendDist` in `tauri.conf.json`). It adds the things the browser-only app can't do:
//!
//!   1. **Native `local_*` reads.** `local_sessions`, `local_session`, `local_session_head`,
//!      `local_messages`, `local_events`, `local_compactions`, `local_touched`, `local_subagents`,
//!      `local_subagent` and `local_workflow_script` answer straight out of `cv_core`, with the
//!      **same JSON shapes `cvd serve` returns** — the webview's cross-origin fetch to
//!      `http://localhost` is unreliable, and a session must not look different depending on which
//!      door it came through. See the section header above `resolve` for the one deliberate
//!      difference (the session row follows `docs/INTERFACE-V2.md` §3).
//!   2. **Native LLM/ingest commands** — `distill`, `redact`, `generate`, `ingest_zip` — that call
//!      into `cv_llm` / `cv_core` directly, so LLM API keys live in the desktop process's
//!      environment (or a local `LMSTUDIO_API_BASE`) instead of being shipped into JS, and zip
//!      ingestion works without the WASM module.
//!   3. **Launches `cvd serve --port 7777` on startup** so the bundled fleet dashboard (which
//!      fetches `http://localhost:7777`) works unchanged. The child is killed on exit.
//!   4. A **native application menu** (File / Edit / View / Help) — including **File → Open zip…**
//!      which runs a native file dialog, ingests the zip in Rust, and emits the
//!      `cv://open-sessions` Tauri event the web UI listens for.
//!   5. **Window state persistence** (size/position across launches) via
//!      `tauri-plugin-window-state`.
//!
//! ### cvd-serve wiring approach
//! We spawn `cvd` via `std::process::Command` (rather than a bundled Tauri *sidecar*) — it's
//! the simplest thing that compiles and runs everywhere without an extra bundling step. We
//! search the repo's build outputs `target/{release,debug}/cvd` first, then PATH, so a plain
//! `cargo build -p cvd` in the repo is enough to make it work in dev. The handle is stored in
//! Tauri's managed state and killed on `RunEvent::Exit`. A **View → cvd serve** menu toggle lets
//! you start/stop it at runtime.
//!
//! To ship a self-contained bundle later, build `cvd` (`cargo build -p cvd --release`) and
//! either drop the binary on PATH or register it as a Tauri sidecar (see `app/README.md`).

use std::io::Read;
use std::process::{Child, Command};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::menu::{AboutMetadataBuilder, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use cv_core::{Adapter, Flow, Message, MessageSink, ParseOptions, Session, SessionRef};

/// The port the bundled web fleet-dashboard expects `cvd serve` on.
const CVD_PORT: u16 = 7777;

/// The Tauri event the web UI listens for to load freshly-ingested sessions. Payload is a JSON
/// string holding a `Session[]` array (the same IR the WASM `ingest_zip` returns).
const OPEN_SESSIONS_EVENT: &str = "cv://open-sessions";

/// The public web demo, opened from Help → Open the web demo.
const WEB_DEMO_URL: &str = "https://emberian.github.io/clustervision/";

/// Handle to the spawned `cvd serve` child, kept in Tauri managed state so we can kill it on exit
/// (and toggle it from the menu).
#[derive(Default)]
struct CvdServer(Mutex<Option<Child>>);

// ---------------------------------------------------------------------------
// Native commands — invoked from JS via `window.__TAURI__.core.invoke(name, args)`.
// ---------------------------------------------------------------------------

/// Parse a `Session` from a JSON string (the IR the web UI already speaks), mapping serde errors
/// to a readable string so the JS side gets a useful message.
fn parse_session(session_json: &str) -> Result<Session, String> {
    serde_json::from_str::<Session>(session_json).map_err(|e| format!("invalid session JSON: {e}"))
}

/// Distill a session into durable Markdown memory via `cv_llm::distill`.
///
/// Requires an LLM provider in the environment: `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, or
/// `LMSTUDIO_API_BASE` (free/offline local server). Returns the Markdown digest, or an error string.
#[tauri::command]
async fn distill(session_json: String) -> Result<String, String> {
    // `cv_llm` uses blocking reqwest; keep the async executor free by offloading to a blocking task.
    tauri::async_runtime::spawn_blocking(move || {
        let session = parse_session(&session_json)?;
        cv_llm::distill(&session, &cv_llm::DistillOptions::default()).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("distill task panicked: {e}"))?
}

/// Scrub secrets/PII from a session via `cv_core::redact`, returning the redacted session as JSON.
///
/// Pure + offline — no API key needed.
#[tauri::command]
fn redact(session_json: String) -> Result<String, String> {
    let session = parse_session(&session_json)?;
    let redacted = cv_core::redact::redact(&session);
    serde_json::to_string(&redacted).map_err(|e| format!("serializing redacted session: {e}"))
}

/// Generate the next assistant turn for a session — the generative half of looming — via
/// `cv_llm::generate`. Honors an optional `model` override; works with `LMSTUDIO_API_BASE` for
/// free local generation.
#[tauri::command]
async fn generate(session_json: String, model: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let session = parse_session(&session_json)?;
        let opts = cv_llm::GenerateOptions {
            model,
            ..Default::default()
        };
        cv_llm::generate(&session, &opts).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("generate task panicked: {e}"))?
}

/// Read a `.zip` of exported harness logs from `path`, unzip it in-memory, run
/// `cv_core::ingest::ingest_files`, and return the parsed sessions as a JSON string (a
/// `Session[]` array — the exact shape the web UI's WASM `ingest_zip` produces).
///
/// Pure + offline: no API key, and no on-disk extraction (everything is held in memory). This is
/// the native equivalent of the browser's WASM ingest path, so file-open works without WASM.
#[tauri::command]
fn ingest_zip(path: String) -> Result<String, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {path}: {e}"))?;
    ingest_zip_bytes(&bytes)
}

/// Unzip `bytes` in-memory and ingest the contained files into `Session[]` JSON. Factored out so
/// both the `ingest_zip` command and the File → Open zip… menu handler share one path.
fn ingest_zip_bytes(bytes: &[u8]) -> Result<String, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("not a readable zip: {e}"))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("reading zip entry {i}: {e}"))?;
        if !entry.is_file() {
            continue;
        }
        // Prefer the safe sanitized name; fall back to the raw name so unusual exports still ingest.
        let name = entry
            .enclosed_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name().to_string());
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("decompressing {name}: {e}"))?;
        files.push((name, buf));
    }

    let sessions = cv_core::ingest::ingest_files(files);
    serde_json::to_string(&sessions).map_err(|e| format!("serializing sessions: {e}"))
}

// ---------------------------------------------------------------------------
// Reading the machine's real sessions — the `local_*` commands.
//
// Every shape below is the one `cvd serve` returns for the same thing
// (`crates/cvd/src/serve.rs`), so a session is indistinguishable between the daemon's HTTP API
// and this app. The one deliberate difference is the **session row**, which follows
// `docs/INTERFACE-V2.md` §3 (`id, harness, path, cwd, title, created_at, updated_at,
// message_count, size_bytes` — what `cv ls --json` emits); cvd's listing still predates that
// rule and omits `path`/`size_bytes`, so the app's rows are a superset, never a renaming.
// ---------------------------------------------------------------------------

/// Look up one session by `harness` + id (or unique prefix), with every failure mapped to a
/// string the UI can display. Shared by every `local_*` command that names a session.
fn resolve(harness: &str, id: &str) -> Result<(SessionRef, Box<dyn Adapter>), String> {
    let want = cv_core::Harness::parse(harness).ok_or_else(|| format!("unknown harness {harness:?}"))?;
    cv_core::find(id, Some(want))
        .map_err(|e| format!("looking up session: {e:#}"))?
        .ok_or_else(|| format!("no session {id:?} for harness {harness}"))
}

/// The transcript's on-disk size, or `None` when it has been deleted since discovery.
fn size_of(r: &SessionRef) -> Option<u64> {
    std::fs::metadata(&r.path).ok().map(|m| m.len())
}

/// The one JSON shape for a session in a list — INTERFACE-V2 §3's session row, key-for-key what
/// `cv ls --json` prints (timestamps are RFC 3339 strings, null when unknown).
fn session_row(r: &SessionRef, size_bytes: Option<u64>) -> Value {
    json!({
        "id": r.id,
        "harness": r.harness.as_str(),
        "path": r.path.to_string_lossy(),
        "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy()),
        "title": r.title,
        "created_at": r.created_at.map(|t| t.to_rfc3339()),
        "updated_at": r.updated_at.map(|t| t.to_rfc3339()),
        "message_count": r.message_count,
        "size_bytes": size_bytes,
    })
}

/// Rows for `refs`, newest-first order preserved, dropping any whose transcript has been deleted
/// since discovery (the catalog's one residual lie) and then capping at `limit` — the same
/// filter-then-take order `cv ls --json` uses, so `limit` counts rows the caller actually gets.
fn session_rows(refs: &[SessionRef], limit: Option<usize>) -> Vec<Value> {
    refs.iter()
        .filter_map(|r| size_of(r).map(|size| session_row(r, Some(size))))
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

/// List the machine's real on-disk sessions as INTERFACE-V2 §3 session rows (no messages),
/// newest-first, optionally filtered by `harness` / a `cwd` substring and capped at `limit`. The
/// web UI shows these in the main list and lazily fetches the transcript via [`local_session`] /
/// [`local_messages`] when a row is opened. Native, HTTP-free twin of cvd's `/api/sessions` (the
/// webview's cross-origin fetch to `http://localhost` is unreliable).
#[tauri::command]
fn local_sessions(limit: Option<usize>, harness: Option<String>, cwd: Option<String>) -> Result<String, String> {
    // Validate the harness up front so a typo is a clear error rather than a silently empty list.
    let want = match &harness {
        Some(h) => Some(cv_core::Harness::parse(h).ok_or_else(|| format!("unknown harness {h:?}"))?),
        None => None,
    };
    // The fast catalog read (~ms when warm) — transparently escalates to a full scan when cold,
    // so the result matches what `discover_all` would return.
    let mut refs = cv_core::sessions();
    if let Some(h) = want {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .is_some_and(|c| c.to_string_lossy().contains(needle.as_str()))
        });
    }
    // Newest-first by updated_at (then created_at), missing dates sort last.
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
    serde_json::to_string(&session_rows(&refs, limit)).map_err(|e| format!("serializing session rows: {e}"))
}

/// Parse one on-disk session fully into IR and return it as JSON — the IR `Session` itself, so
/// `kind`/`origin` on every message, `system_prompt`, `lineage`, block `type` tags, tool-result
/// `details` and `usage` all arrive exactly as `cv show --json` and cvd's
/// `/api/session/{h}/{id}` emit them. `harness`/`id` come from a [`local_sessions`] row.
#[tauri::command]
fn local_session(harness: String, id: String) -> Result<String, String> {
    let (r, adapter) = resolve(&harness, &id)?;
    let session = adapter.parse(&r).map_err(|e| format!("parsing session: {e:#}"))?;
    serde_json::to_string(&session).map_err(|e| format!("serializing session: {e}"))
}

/// Counts messages as they stream past and keeps none — so a session's *head* (the
/// [`Session`] shell an adapter returns alongside the stream) costs one pass and O(1) memory
/// even on a multi-GB transcript.
#[derive(Default)]
struct HeadSink {
    total: usize,
}

impl MessageSink for HeadSink {
    fn message(&mut self, _m: Message) -> Flow {
        self.total += 1;
        Flow::Continue
    }
}

/// The session-level half of the IR without its messages: the §3 session row plus everything
/// 0.11's `Session` carries that a listing row cannot — `model`, `git`, `system_prompt`,
/// `lineage` and the harness `extra` bag — and `total`, the exact message count in the index
/// space [`local_messages`] windows over.
fn session_head_json(r: &SessionRef, adapter: &dyn Adapter) -> Result<Value, String> {
    let mut sink = HeadSink::default();
    // Lazy: giant content stays on disk as spans, and the sink drops every message anyway, so
    // peak memory is O(largest record) rather than O(transcript).
    let shell = adapter
        .stream(r, &ParseOptions::lazy(), &mut sink)
        .map_err(|e| format!("reading session head: {e:#}"))?;

    let mut row = session_row(r, size_of(r));
    let map = row.as_object_mut().expect("session_row is an object");
    // The freshly-parsed title/cwd beat the catalog's, which can be stale.
    if let Some(t) = &shell.title {
        map.insert("title".into(), json!(t));
    }
    if let Some(c) = &shell.cwd {
        map.insert("cwd".into(), json!(c.to_string_lossy()));
    }
    map.insert("total".into(), json!(sink.total));
    map.insert("model".into(), json!(shell.model));
    map.insert("git".into(), serde_json::to_value(&shell.git).unwrap_or(Value::Null));
    map.insert("system_prompt".into(), json!(shell.system_prompt));
    map.insert(
        "lineage".into(),
        serde_json::to_value(&shell.lineage).unwrap_or(Value::Null),
    );
    map.insert("extra".into(), Value::Object(shell.extra));
    Ok(row)
}

/// One session's head — everything about the session *except* its messages, in one cheap pass:
/// the §3 row plus `model`, `git`, `system_prompt`, `lineage`, `extra` and the exact message
/// `total`. This is how a paging UI shows the system prompt and the fork/parent/continuation
/// lineage 0.11 added without parsing a multi-gigabyte transcript into memory — and it is the
/// only way to get them for a harness (Claude) whose stream emits no `meta`.
#[tauri::command]
fn local_session_head(harness: String, id: String) -> Result<String, String> {
    let (r, adapter) = resolve(&harness, &id)?;
    let head = session_head_json(&r, adapter.as_ref())?;
    serde_json::to_string(&head).map_err(|e| format!("serializing session head: {e}"))
}

/// Collects the message window `[start, end)` as serialized IR while streaming — out-of-window
/// messages pass by as unmaterialized lazy handles, and the stream stops at `end` (after noting
/// whether a message exists there, the `has_more` probe). The native twin of the sink inside
/// cvd's `/messages` endpoint.
struct WindowSink {
    resolver: cv_core::Resolver,
    idx: usize,
    start: usize,
    end: Option<usize>,
    msgs: Vec<Value>,
    more: bool,
    meta: Option<Value>,
    fail: Option<String>,
}

/// The session metadata a message window carries: the same three keys cvd's `/messages` sends.
fn window_meta(s: &Session) -> Value {
    json!({
        "title": s.title,
        "model": s.model,
        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
    })
}

impl MessageSink for WindowSink {
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(window_meta(s));
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.idx;
        self.idx += 1;
        if idx < self.start {
            return Flow::Continue;
        }
        if self.end.is_some_and(|e| idx >= e) {
            self.more = true;
            return Flow::Stop;
        }
        let mut m = m;
        m.materialize(&self.resolver);
        match serde_json::to_value(&m) {
            Ok(v) => {
                self.msgs.push(v);
                Flow::Continue
            }
            Err(e) => {
                self.fail = Some(e.to_string());
                Flow::Stop
            }
        }
    }
}

/// The window `[start, end)` of a session's messages as IR JSON. Tries the seekable-session store
/// first (jump straight to message `start`'s recorded byte offset); falls back to a full stream
/// that still stops at `end`, so the tail is never read.
///
/// `want_extra` keeps each message's harness `extra` map **and** the structured
/// `Block::ToolResult::details` sidecar (both are gated on `ParseOptions::extra`, because the
/// sidecar routinely dwarfs the visible transcript) — off for bulk transcript paging, on for the
/// structure view that hunts compaction seams and for anything that renders tool-result details.
fn messages_window(
    r: &SessionRef,
    adapter: &dyn Adapter,
    start: usize,
    end: Option<usize>,
    want_extra: bool,
) -> Result<Value, String> {
    let opts = ParseOptions {
        extra: want_extra,
        ..ParseOptions::lazy()
    };
    let mut sink = WindowSink {
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        idx: start,
        start,
        end,
        msgs: Vec::new(),
        more: false,
        meta: None,
        fail: None,
    };
    // Ask for one message past the window so `has_more` is known either way.
    let probe = end.map(|e| e.saturating_add(1));
    let seeked = if start > 0 {
        cv_core::offsets::stream_range(r, start, probe, &opts, &mut sink)
            .map_err(|e| format!("range read failed: {e:#}"))?
    } else {
        false
    };
    if !seeked {
        sink.idx = 0;
        // The shell an adapter returns alongside the stream carries the session-level fields.
        // Claude's stream emits no `meta()` at all, so without this fallback a windowed read of
        // the single most common harness reports a null `session` — the shell is already in hand.
        let shell = adapter
            .stream(r, &opts, &mut sink)
            .map_err(|e| format!("streaming session: {e:#}"))?;
        if sink.meta.is_none() {
            sink.meta = Some(window_meta(&shell));
        }
    }
    if let Some(f) = sink.fail {
        return Err(f);
    }

    // Exact only when the stream ran to EOF; an empty seek-read can't tell EOF-at-start from
    // start-past-EOF (mirrors cvd's endpoint).
    let total_known = !sink.more && (!seeked || !sink.msgs.is_empty());
    Ok(json!({
        "harness": r.harness.as_str(),
        "id": r.id,
        "start": start,
        "end": start + sink.msgs.len(),
        "has_more": sink.more,
        "total_known": total_known,
        "total": total_known.then_some(sink.idx),
        // Discovery-time count, as a hint while `total` is unknown.
        "message_count": r.message_count,
        "session": sink.meta,
        "messages": sink.msgs,
    }))
}

/// The message window `[start, end)` of one on-disk session as JSON — the native equivalent of
/// cvd's `/api/session/{h}/{id}/messages?start&end&extra`, so the web UI's paged transcript works
/// without HTTP. The reply carries the window's indices, `total_known`/`total`, `has_more`, the
/// session `meta`, and the messages as IR (`kind`, `origin`, `usage`, `type`-tagged blocks).
#[tauri::command]
fn local_messages(
    harness: String,
    id: String,
    start: Option<usize>,
    end: Option<usize>,
    extra: Option<bool>,
) -> Result<String, String> {
    let start = start.unwrap_or(0);
    if end.is_some_and(|e| e < start) {
        return Err("end must be >= start".into());
    }
    let (r, adapter) = resolve(&harness, &id)?;
    let window = messages_window(&r, adapter.as_ref(), start, end, extra.unwrap_or(false))?;
    serde_json::to_string(&window).map_err(|e| format!("serializing window: {e}"))
}

/// The session's extracted tool events (file edits/reads, commands, errors) in transcript order,
/// (re)ingesting a stale catalog row on the spot — native equivalent of cvd's `/events`.
#[tauri::command]
fn local_events(harness: String, id: String, kind: Option<String>) -> Result<String, String> {
    use cv_core::events;
    let (r, _) = resolve(&harness, &id)?;
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        events::ingest_ref(&r).map_err(|e| format!("event ingest failed: {e:#}"))?;
    }
    let rows: Vec<Value> = events::events_for(r.harness.as_str(), &r.id, kind.as_deref())
        .iter()
        .map(|e| {
            json!({
                "msg_idx": e.msg_idx,
                "ts": e.ts,
                "kind": e.kind,
                "tool": e.tool,
                "target": e.target,
                "detail": e.detail,
            })
        })
        .collect();
    serde_json::to_string(&rows).map_err(|e| format!("serializing events: {e}"))
}

/// Every compaction boundary in a session, in transcript order — native equivalent of cvd's
/// `/api/session/{h}/{id}/compactions`. `index`/`summary_index` are in the same numbering
/// [`local_messages`] uses, and `pre_span` is the `[start, boundary)` range that was compacted
/// away. The summary text is kept: it is how a continued agent recovers context it can no longer
/// see, and the whole point of surfacing compaction at all.
#[tauri::command]
fn local_compactions(harness: String, id: String) -> Result<String, String> {
    let (r, _) = resolve(&harness, &id)?;
    serde_json::to_string(&compactions_json(&r)?).map_err(|e| format!("serializing compactions: {e}"))
}

/// The `{ harness, id, compactions: [...] }` body of [`local_compactions`].
fn compactions_json(r: &SessionRef) -> Result<Value, String> {
    let boundaries = cv_core::compaction::detect(r, true).map_err(|e| format!("compaction scan failed: {e:#}"))?;
    let out: Vec<Value> = boundaries
        .iter()
        .enumerate()
        .map(|(n, c)| {
            let span = cv_core::compaction::pre_compaction_span(&boundaries, n);
            json!({
                "index": c.boundary_msg_idx,
                "summary_index": c.summary_msg_idx,
                "trigger": c.trigger,
                "pre_tokens": c.pre_tokens,
                "duration_ms": c.duration_ms,
                "summary": c.summary,
                "headline": c.headline(n + 1),
                "pre_span": span.map(|(s, e)| json!([s, e])),
            })
        })
        .collect();
    Ok(json!({
        "harness": r.harness.as_str(),
        "id": r.id,
        "compactions": out,
    }))
}

/// Sessions whose tool events touched a file (exact or suffix path match), newest first —
/// native equivalent of cvd's `/api/touched?path=&edits_only=`.
#[tauri::command]
fn local_touched(path: String, edits_only: Option<bool>) -> Result<String, String> {
    let rows: Vec<Value> = cv_core::events::sessions_touching(&path, edits_only.unwrap_or(false))
        .iter()
        .map(|t| {
            json!({
                "harness": t.harness,
                "session_id": t.session_id,
                "title": t.title,
                "edits": t.edits,
                "reads": t.reads,
                "last_ts": t.last_ts,
            })
        })
        .collect();
    serde_json::to_string(&rows).map_err(|e| format!("serializing touched rows: {e}"))
}

/// Rows for the sub-agent **forest** a session spawned: directly-spawned (`Agent`/`Task`)
/// sub-agents *and* the workflow tier one level deeper, each a §3 session row plus the sidecar
/// enrichment the forest view groups by.
fn subagent_rows(r: &SessionRef) -> Vec<Value> {
    cv_core::subagent_tree_of(r)
        .iter()
        .map(|s| {
            let mut row = session_row(&s.session, size_of(&s.session));
            let map = row.as_object_mut().expect("session_row is an object");
            map.insert("agent_id".into(), json!(s.agent_id()));
            map.insert("agent_type".into(), json!(s.agent_type));
            map.insert("description".into(), json!(s.description));
            map.insert("tool_use_id".into(), json!(s.tool_use_id));
            map.insert("workflow".into(), json!(s.workflow));
            map.insert("result_status".into(), json!(s.result_status));
            map.insert("result_summary".into(), json!(s.result_summary));
            row
        })
        .collect()
}

/// The sub-agents a session spawned, as rows — the native equivalent of cvd's
/// `/api/session/{h}/{id}/subagents`. The flat `subagents_of` this used to call missed the entire
/// workflow tier (on a heavy session, the majority of the agents) and dropped every enrichment
/// field, so the desktop forest view was structurally blind where the browser's was not.
#[tauri::command]
fn local_subagents(harness: String, id: String) -> Result<String, String> {
    let (r, _) = resolve(&harness, &id)?;
    serde_json::to_string(&subagent_rows(&r)).map_err(|e| format!("serializing subagents: {e}"))
}

/// Full parsed transcript of one sub-agent, loaded relative to its parent (sub-agents aren't in
/// the main pool). Native equivalent of cvd's `/api/session/{h}/{parent}/subagent/{agent}`;
/// `agent` may be the full transcript id or the bare `agentId` the journal uses.
#[tauri::command]
fn local_subagent(harness: String, parent: String, agent: String) -> Result<String, String> {
    let (r, adapter) = resolve(&harness, &parent)?;
    let sr = cv_core::subagent_tree_of(&r)
        .into_iter()
        .find(|s| s.session.id == agent || s.agent_id() == agent)
        .map(|s| s.session)
        .ok_or_else(|| format!("subagent {agent:?} not found under {parent}"))?;
    let session = adapter.parse(&sr).map_err(|e| format!("parsing subagent: {e:#}"))?;
    serde_json::to_string(&session).map_err(|e| format!("serializing subagent: {e}"))
}

/// The driving script Claude Code recorded for a workflow run, as `{ workflow, name, source }` —
/// native equivalent of cvd's `/api/session/{h}/{id}/workflow/{wf}/script`. Scripts live in the
/// session's sidecar tree at `<session>/workflows/scripts/<slug>-<wf>.js`; the run id (`wf_…`) is
/// matched as the filename suffix, since the slug prefix varies per workflow.
#[tauri::command]
fn local_workflow_script(harness: String, id: String, workflow: String) -> Result<String, String> {
    // Reject a path-y run id up front — it is only ever a `wf_…` token, never a path component.
    if workflow.is_empty() || workflow.contains('/') || workflow.contains("..") {
        return Err("invalid workflow id".into());
    }
    let (r, _) = resolve(&harness, &id)?;
    let path = workflow_script_path(&r, &workflow)
        .ok_or_else(|| format!("no script recorded for workflow run {workflow:?}"))?;
    let source = std::fs::read_to_string(&path).map_err(|e| format!("could not read script: {e}"))?;
    serde_json::to_string(&json!({
        "workflow": workflow,
        "name": path.file_name().and_then(|n| n.to_str()),
        "source": source,
    }))
    .map_err(|e| format!("serializing workflow script: {e}"))
}

/// The recorded script file for workflow run `wf` under session `r`, if one exists.
fn workflow_script_path(r: &SessionRef, wf: &str) -> Option<std::path::PathBuf> {
    let stem = r.path.file_stem().and_then(|s| s.to_str())?;
    let dir = r.path.parent()?.join(stem).join("workflows").join("scripts");
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.extension().and_then(|x| x.to_str()) == Some("js")
            && p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == wf || s.ends_with(&format!("-{wf}")))
    })
}

/// Report whether an LLM provider is configured (so the UI can enable/disable distill/loom).
#[derive(Serialize)]
struct ProviderInfo {
    /// `"openrouter" | "anthropic" | "lmstudio"`, or `null` if no provider is configured.
    provider: Option<String>,
    /// Convenience flag for the UI.
    available: bool,
}

/// Tell the frontend which LLM provider (if any) is wired up via the desktop process's env.
#[tauri::command]
fn provider_info() -> ProviderInfo {
    let provider = cv_llm::available_provider().map(|s| s.to_string());
    ProviderInfo {
        available: provider.is_some(),
        provider,
    }
}

// ---------------------------------------------------------------------------
// Native application menu
// ---------------------------------------------------------------------------

/// Build the native application menu (File / View / Help). Menu item IDs are matched in
/// [`handle_menu_event`].
fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // --- File
    let open_zip = MenuItem::with_id(app, "file.open_zip", "Open zip…", true, Some("CmdOrCtrl+O"))?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit clustervision"))?;
    let file = Submenu::with_items(app, "File", true, &[&open_zip, &quit])?;

    // --- Edit: without these, macOS gives the webview no Cmd+C/Cmd+V/Cmd+A — a real problem for a
    // transcript viewer where copying text is the whole point. Predefined items wire to the OS's
    // standard editing actions for the focused field/selection.
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("Undo"))?,
            &PredefinedMenuItem::redo(app, Some("Redo"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some("Cut"))?,
            &PredefinedMenuItem::copy(app, Some("Copy"))?,
            &PredefinedMenuItem::paste(app, Some("Paste"))?,
            &PredefinedMenuItem::select_all(app, Some("Select All"))?,
        ],
    )?;

    // --- View
    let reload = MenuItem::with_id(app, "view.reload", "Reload", true, Some("CmdOrCtrl+R"))?;
    let devtools = MenuItem::with_id(app, "view.devtools", "Toggle DevTools", true, Some("CmdOrCtrl+Alt+I"))?;
    let fullscreen = PredefinedMenuItem::fullscreen(app, Some("Toggle Fullscreen"))?;
    let cvd_toggle = MenuItem::with_id(app, "view.cvd_toggle", "Toggle cvd serve", true, None::<&str>)?;
    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &reload,
            &devtools,
            &fullscreen,
            &PredefinedMenuItem::separator(app)?,
            &cvd_toggle,
        ],
    )?;

    // --- Help
    let about_meta = AboutMetadataBuilder::new()
        .name(Some("clustervision"))
        .version(Some(env!("CARGO_PKG_VERSION")))
        .comments(Some(
            "Cross-harness AI session viewer with native distill, redact, and generative loom.",
        ))
        .build();
    let about = PredefinedMenuItem::about(app, Some("About clustervision"), Some(about_meta))?;
    let web_demo = MenuItem::with_id(app, "help.web_demo", "Open the web demo", true, None::<&str>)?;
    let help = Submenu::with_items(app, "Help", true, &[&about, &web_demo])?;

    Menu::with_items(app, &[&file, &edit, &view, &help])
}

/// React to a native menu click. Unknown IDs are ignored.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        "file.open_zip" => open_zip_via_dialog(app),
        "view.reload" => {
            if let Some(win) = main_window(app) {
                // Reloading the webview re-fetches frontendDist (web/index.html).
                let _ = win.eval("window.location.reload()");
            }
        }
        "view.devtools" => {
            if let Some(win) = main_window(app) {
                if win.is_devtools_open() {
                    win.close_devtools();
                } else {
                    win.open_devtools();
                }
            }
        }
        "view.cvd_toggle" => toggle_cvd_serve(app),
        "help.web_demo" => {
            // Open in the user's default browser via the shell plugin (matches the existing
            // `shell:allow-open` capability). `Shell::open` is deprecated in favor of
            // tauri-plugin-opener, but using it avoids pulling in another plugin/permission set.
            #[allow(deprecated)]
            {
                use tauri_plugin_shell::ShellExt;
                let _ = app.shell().open(WEB_DEMO_URL, None);
            }
        }
        _ => {}
    }
}

/// Get the main webview window (label `"main"`), or `None` if it's gone.
fn main_window<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    app.get_webview_window("main")
}

/// File → Open zip…: native picker → in-memory ingest → emit `cv://open-sessions` with the
/// `Session[]` JSON payload. All failures are surfaced via a native message dialog rather than
/// silently dropped.
fn open_zip_via_dialog<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    // Run the (blocking) native picker off the main thread so we never deadlock the event loop.
    std::thread::spawn(move || {
        let picked = app
            .dialog()
            .file()
            .add_filter("Harness export (.zip)", &["zip"])
            .set_title("Open a harness export zip")
            .blocking_pick_file();

        let Some(file_path) = picked else {
            return; // user cancelled
        };

        let result = file_path
            .into_path()
            .map_err(|e| format!("resolving picked path: {e}"))
            .and_then(|p| std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display())))
            .and_then(|bytes| ingest_zip_bytes(&bytes));

        match result {
            Ok(sessions_json) => {
                if let Err(e) = app.emit(OPEN_SESSIONS_EVENT, sessions_json) {
                    eprintln!("clustervision: failed to emit {OPEN_SESSIONS_EVENT}: {e}");
                }
            }
            Err(e) => {
                use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
                app.dialog()
                    .message(e)
                    .kind(MessageDialogKind::Error)
                    .title("Couldn't open zip")
                    .buttons(MessageDialogButtons::Ok)
                    .blocking_show();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// cvd serve lifecycle
// ---------------------------------------------------------------------------

/// Locate the `cvd` binary: the repo's build outputs relative to this crate's manifest dir first
/// (so a dev `cargo build -p cvd` works without installing anything), then a bare `cvd` on PATH.
fn cvd_binary() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is `.../clustervision/app/src-tauri`; the workspace target dir is
    // `.../clustervision/target`.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
        for profile in ["release", "debug"] {
            let cand = root.join("target").join(profile).join("cvd");
            if cand.exists() {
                return cand;
            }
        }
    }
    // Fall back to PATH lookup.
    std::path::PathBuf::from("cvd")
}

/// Spawn `cvd serve --port 7777`. Errors are logged but non-fatal: the rest of the UI (everything
/// except the live fleet dashboard) still works without it. Returns the child on success.
fn spawn_cvd_serve() -> Option<Child> {
    let bin = cvd_binary();
    match Command::new(&bin)
        .arg("serve")
        .arg("--port")
        .arg(CVD_PORT.to_string())
        .spawn()
    {
        Ok(child) => {
            eprintln!(
                "clustervision: launched `{} serve --port {CVD_PORT}` (pid {})",
                bin.display(),
                child.id()
            );
            Some(child)
        }
        Err(e) => {
            eprintln!(
                "clustervision: could not launch cvd serve ({}): {e}. The fleet dashboard at \
                 http://localhost:{CVD_PORT} will be unavailable (non-fatal). Build it with \
                 `cargo build -p cvd --release` or put `cvd` on PATH.",
                bin.display()
            );
            None
        }
    }
}

/// View → Toggle cvd serve: kill it if running, otherwise (re)spawn it.
fn toggle_cvd_serve<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<CvdServer>();
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("clustervision: stopped cvd serve via menu toggle.");
    } else {
        *guard = spawn_cvd_serve();
    }
}

/// Build and run the Tauri application. Called by `main.rs`.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Persist each window's size/position/etc. across launches and restore on startup.
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .manage(CvdServer::default())
        .invoke_handler(tauri::generate_handler![
            distill,
            redact,
            generate,
            ingest_zip,
            provider_info,
            local_sessions,
            local_session,
            local_session_head,
            local_messages,
            local_events,
            local_compactions,
            local_touched,
            local_subagents,
            local_subagent,
            local_workflow_script
        ])
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            // Install the native application menu.
            let menu = build_menu(app.handle())?;
            app.set_menu(menu)?;

            // Launch the fleet-state API so the bundled dashboard works unchanged.
            if let Some(child) = spawn_cvd_serve() {
                let state = app.state::<CvdServer>();
                *state.0.lock().unwrap() = Some(child);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building clustervision")
        .run(|app_handle, event| {
            // On exit, reap the cvd child so we don't leave an orphaned server on :7777.
            if let RunEvent::Exit = event {
                let state = app_handle.state::<CvdServer>();
                let child = state.0.lock().unwrap().take();
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Tests — cover the native-command *logic* (parse/ingest/redact/binary lookup). The GUI shell
// (menu, window, event loop) can't be driven headlessly, but these are the parts with real bug
// surface, and they run without a webview via plain `cargo test`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build an in-memory zip from `(name, bytes)` entries using `Stored` (no compression feature
    /// needed), returning the raw zip bytes.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// A minimal Claude `.jsonl` transcript: ingest sniffs it by `sessionId` + `type:user|assistant`.
    const CLAUDE_JSONL: &str = concat!(
        r#"{"type":"user","sessionId":"sess-abc","parentUuid":null,"message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"sess-abc","parentUuid":null,"message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
        "\n",
    );

    #[test]
    fn ingest_zip_bytes_parses_a_claude_export() {
        let zip = make_zip(&[("logs/session.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).expect("ingest should succeed");
        let sessions: Vec<Session> = serde_json::from_str(&json).expect("valid Session[] JSON");
        assert_eq!(sessions.len(), 1, "one transcript → one session");
        assert_eq!(sessions[0].harness, cv_core::Harness::Claude);
        assert!(
            !sessions[0].messages.is_empty(),
            "the session should carry its messages"
        );
    }

    #[test]
    fn ingest_zip_bytes_rejects_non_zip() {
        let err = ingest_zip_bytes(b"not a zip at all").unwrap_err();
        assert!(err.contains("not a readable zip"), "got: {err}");
    }

    #[test]
    fn ingest_zip_bytes_skips_dirs_and_unknown_files() {
        // A directory entry and an unrecognized file should yield an empty (but valid) array, no err.
        let zip = make_zip(&[("notes/", b""), ("random.txt", b"just some prose, not a transcript")]);
        let json = ingest_zip_bytes(&zip).expect("ingest should not error on unknown content");
        let sessions: Vec<Session> = serde_json::from_str(&json).unwrap();
        assert!(sessions.is_empty(), "nothing recognizable → []");
    }

    #[test]
    fn parse_session_roundtrips_and_reports_errors() {
        let zip = make_zip(&[("s.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).unwrap();
        let one = &serde_json::from_str::<Vec<Session>>(&json).unwrap()[0];
        let one_json = serde_json::to_string(one).unwrap();
        let parsed = parse_session(&one_json).expect("a session we just serialized must parse");
        assert_eq!(parsed.id, one.id);

        let err = parse_session("{ not json").unwrap_err();
        assert!(err.contains("invalid session JSON"), "got: {err}");
    }

    #[test]
    fn redact_command_runs_offline_and_returns_valid_json() {
        let zip = make_zip(&[("s.jsonl", CLAUDE_JSONL.as_bytes())]);
        let session_json = {
            let one = &serde_json::from_str::<Vec<Session>>(&ingest_zip_bytes(&zip).unwrap()).unwrap()[0];
            serde_json::to_string(one).unwrap()
        };
        let out = redact(session_json).expect("redact is pure/offline and shouldn't fail");
        // Output must be a valid Session again (so the UI can render it).
        serde_json::from_str::<Session>(&out).expect("redacted output is a valid Session");
    }

    // -----------------------------------------------------------------------
    // JSON shaping of the `local_*` reads.
    //
    // These run against a real Claude transcript written into a scratch dir and parsed by the
    // real adapter — never the machine's session store — so they pin the *shapes* the UI reads
    // (INTERFACE-V2 §3/§4) without depending on what happens to be on this laptop.
    // -----------------------------------------------------------------------

    /// A small but representative Claude transcript: the system prompt snapshot, a human prompt,
    /// a reply carrying thinking + text + a tool call with usage, the tool result with its
    /// `toolUseResult` sidecar, a compaction boundary paired with the summary that seeded the next
    /// window, and a continuation pointer (session lineage).
    const FIXTURE: &[&str] = &[
        r#"{"type":"attachment","sessionId":"fix-1","uuid":"p1","timestamp":"2026-09-19T10:00:00.000Z","attachment":{"type":"prompt_snapshot","systemPrompt":["You are Claude Code.","Be terse."]}}"#,
        r#"{"type":"user","sessionId":"fix-1","uuid":"u1","parentUuid":null,"cwd":"/w/proj","gitBranch":"main","timestamp":"2026-09-19T10:00:01.000Z","message":{"role":"user","content":"read a.rs please"}}"#,
        r#"{"type":"assistant","sessionId":"fix-1","uuid":"a1","parentUuid":"u1","timestamp":"2026-09-19T10:00:02.000Z","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"thinking","thinking":"pondering","signature":"sig-xyz"},{"type":"text","text":"reading it now"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/w/proj/a.rs"}}],"usage":{"input_tokens":120,"output_tokens":34,"cache_read_input_tokens":7,"cache_creation_input_tokens":11}}}"#,
        r#"{"type":"user","sessionId":"fix-1","uuid":"t1","parentUuid":"a1","timestamp":"2026-09-19T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"fn main() {}"}]},"toolUseResult":{"filePath":"/w/proj/a.rs","numLines":1}}"#,
        r#"{"type":"system","subtype":"compact_boundary","sessionId":"fix-1","uuid":"c1","parentUuid":"t1","timestamp":"2026-09-19T10:00:04.000Z","compactMetadata":{"trigger":"auto","preTokens":148000,"durationMs":4200}}"#,
        r#"{"type":"user","sessionId":"fix-1","uuid":"s1","parentUuid":"c1","isCompactSummary":true,"timestamp":"2026-09-19T10:00:05.000Z","message":{"role":"user","content":"Summary of everything before."}}"#,
        r#"{"type":"continued-in","sessionId":"fix-1","continuedInSessionId":"fix-2"}"#,
    ];

    /// A one-exchange sub-agent transcript (used for both tiers of the forest).
    const AGENT_JSONL: &str = concat!(
        r#"{"type":"user","sessionId":"agent-a7","uuid":"g1","timestamp":"2026-09-19T10:00:06.000Z","message":{"role":"user","content":"go look"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"agent-a7","uuid":"g2","parentUuid":"g1","timestamp":"2026-09-19T10:00:07.000Z","message":{"role":"assistant","content":[{"type":"text","text":"looked"}]}}"#,
        "\n",
    );

    /// The fixture session on disk: the transcript, its sub-agent forest (a directly-spawned agent
    /// and a workflow agent with a journaled result), and the workflow's driving script. Returns
    /// the scratch dir (kept alive for the test), a `SessionRef` for it, and the Claude adapter.
    fn fixture() -> (tempfile::TempDir, SessionRef, Box<dyn Adapter>) {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("fix-1.jsonl");
        std::fs::write(&path, FIXTURE.join("\n") + "\n").unwrap();

        let side = dir.path().join("fix-1");
        let subs = side.join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::write(subs.join("agent-a7.jsonl"), AGENT_JSONL).unwrap();
        std::fs::write(
            subs.join("agent-a7.meta.json"),
            r#"{"agentType":"Explore","description":"find the seam","toolUseId":"toolu_1"}"#,
        )
        .unwrap();

        let wf = subs.join("workflows").join("wf_99");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("agent-b8.jsonl"), AGENT_JSONL).unwrap();
        std::fs::write(
            wf.join("agent-b8.meta.json"),
            r#"{"agentType":"workflow-subagent","description":"lane 1"}"#,
        )
        .unwrap();
        std::fs::write(
            wf.join("journal.jsonl"),
            "{\"type\":\"result\",\"agentId\":\"b8\",\"result\":{\"status\":\"done\",\"summary\":\"lane 1 landed\"}}\n",
        )
        .unwrap();

        let scripts = side.join("workflows").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("my-flow-wf_99.js"), "export default async () => {};\n").unwrap();

        let r = SessionRef {
            id: "fix-1".into(),
            harness: cv_core::Harness::Claude,
            path,
            cwd: Some("/w/proj".into()),
            title: Some("fixture".into()),
            created_at: Some("2026-09-19T10:00:00Z".parse().unwrap()),
            updated_at: Some("2026-09-19T10:00:05Z".parse().unwrap()),
            message_count: 3,
        };
        let adapter = cv_core::harness::for_harness(cv_core::Harness::Claude).expect("claude adapter");
        (dir, r, adapter)
    }

    /// The object's keys, sorted — for exact key-set assertions.
    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn session_rows_are_the_interface_v2_row() {
        let (_dir, r, _a) = fixture();
        let rows = session_rows(std::slice::from_ref(&r), None);
        assert_eq!(rows.len(), 1, "an existing transcript yields one row");
        assert_eq!(
            keys(&rows[0]),
            [
                "created_at",
                "cwd",
                "harness",
                "id",
                "message_count",
                "path",
                "size_bytes",
                "title",
                "updated_at"
            ],
            "INTERFACE-V2 §3: a session row is always exactly these keys"
        );
        assert_eq!(rows[0]["harness"], "claude");
        assert_eq!(rows[0]["message_count"], 3);
        assert_eq!(
            rows[0]["size_bytes"].as_u64().unwrap(),
            std::fs::metadata(&r.path).unwrap().len(),
            "size_bytes is the transcript's real length"
        );
        // Timestamps are RFC 3339 strings, not epoch numbers or a serde struct.
        assert_eq!(rows[0]["created_at"], "2026-09-19T10:00:00+00:00");

        // A transcript deleted since discovery drops out of the listing rather than lying.
        std::fs::remove_file(&r.path).unwrap();
        assert!(session_rows(std::slice::from_ref(&r), None).is_empty());
    }

    #[test]
    fn local_session_hands_the_ui_ir_v2() {
        let (_dir, r, adapter) = fixture();
        let session = adapter.parse(&r).unwrap();
        let v = serde_json::to_value(&session).unwrap();

        // Session-level IR v2: the system prompt is a field, not a message; lineage is first-class.
        assert!(v["system_prompt"].as_str().unwrap().contains("You are Claude Code."));
        assert_eq!(v["lineage"]["continued_in"], "fix-2");
        // Harness facts nest under the harness name — no flat `extra` keys (§4).
        assert!(
            v["extra"]
                .as_object()
                .is_none_or(|e| e.keys().all(|k| k == "claude" || k == "cv")),
            "session extra is nested by harness: {:?}",
            v["extra"]
        );

        // Every message carries `kind` and `origin`, and every block is tagged by `type`.
        let msgs = v["messages"].as_array().unwrap();
        for m in msgs {
            assert!(m["kind"].is_string(), "message without a kind: {m}");
            assert!(m["origin"].is_string(), "message without an origin: {m}");
            for b in m["content"].as_array().unwrap() {
                assert!(b["type"].is_string(), "block without a `type` tag: {b}");
                assert!(b.get("kind").is_none(), "a block is tagged `type`, never `kind`: {b}");
            }
        }
        let kinds: Vec<&str> = msgs.iter().map(|m| m["kind"].as_str().unwrap()).collect();
        for want in [
            "prompt",
            "reply",
            "tool_result",
            "compaction_boundary",
            "compaction_summary",
        ] {
            assert!(kinds.contains(&want), "missing kind {want} in {kinds:?}");
        }

        // The reply: thinking/text/tool_use block types, model, and snake_case usage.
        let reply = msgs.iter().find(|m| m["kind"] == "reply").unwrap();
        assert_eq!(reply["origin"], "model");
        assert_eq!(reply["model"], "claude-opus-5");
        let types: Vec<&str> = reply["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["thinking", "text", "tool_use"]);
        assert_eq!(reply["content"][0]["signature"], "sig-xyz");
        assert_eq!(reply["content"][2]["name"], "Read");
        assert_eq!(
            keys(&reply["usage"]),
            [
                "cache_creation_tokens",
                "cache_read_tokens",
                "input_tokens",
                "output_tokens"
            ],
            "Usage is snake_case; reasoning_tokens/cost_usd are absent when the harness has none"
        );

        // A tool result's structured sidecar rides on the block as `details` (§4).
        let tool = msgs.iter().find(|m| m["kind"] == "tool_result").unwrap();
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["content"][0]["type"], "tool_result");
        assert_eq!(tool["content"][0]["details"]["filePath"], "/w/proj/a.rs");
    }

    #[test]
    fn message_windows_page_the_same_ir_the_full_parse_gives() {
        let (_dir, r, adapter) = fixture();
        let full = serde_json::to_value(adapter.parse(&r).unwrap()).unwrap();
        let all = full["messages"].as_array().unwrap();

        let w = messages_window(&r, adapter.as_ref(), 0, Some(2), false).unwrap();
        assert_eq!(
            keys(&w),
            [
                "end",
                "harness",
                "has_more",
                "id",
                "message_count",
                "messages",
                "session",
                "start",
                "total",
                "total_known"
            ],
            "the window envelope is cvd's `/messages` envelope, key for key"
        );
        assert_eq!(w["harness"], "claude");
        assert_eq!(w["start"], 0);
        assert_eq!(w["end"], 2);
        assert_eq!(w["has_more"], true, "a message exists past the window");
        assert_eq!(w["total_known"], false, "the stream stopped short of EOF");
        assert!(w["total"].is_null());
        // Claude's stream emits no `meta()`; the session shell fills it instead of a null.
        assert_eq!(w["session"]["cwd"], "/w/proj");
        assert_eq!(w["session"]["model"], "claude-opus-5");

        // A window is the same IR the full parse gives at those indices — same kinds, same blocks.
        let win = w["messages"].as_array().unwrap();
        assert_eq!(win.len(), 2);
        for (i, m) in win.iter().enumerate() {
            assert_eq!(m["kind"], all[i]["kind"]);
            assert_eq!(m["role"], all[i]["role"]);
            assert_eq!(m["origin"], all[i]["origin"]);
            assert_eq!(m["content"], all[i]["content"]);
        }

        // Reading to the end: exact totals, nothing more.
        let tail = messages_window(&r, adapter.as_ref(), 0, None, false).unwrap();
        assert_eq!(tail["has_more"], false);
        assert_eq!(tail["total_known"], true);
        assert_eq!(tail["total"].as_u64().unwrap() as usize, all.len());
        assert_eq!(tail["end"].as_u64().unwrap() as usize, all.len());
    }

    #[test]
    fn tool_result_details_ride_the_block_only_when_extra_is_asked_for() {
        let (_dir, r, adapter) = fixture();
        let details = |extra: bool| -> Value {
            let w = messages_window(&r, adapter.as_ref(), 0, None, extra).unwrap();
            w["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["kind"] == "tool_result")
                .unwrap()["content"][0]
                .clone()
        };
        // Bulk paging stays lean: the sidecar (which routinely dwarfs the transcript) is absent…
        assert!(details(false).get("details").is_none());
        // …and the structure/detail view opts in — this is the arg the app used to drop silently.
        assert_eq!(details(true)["details"]["filePath"], "/w/proj/a.rs");
    }

    #[test]
    fn session_head_carries_the_session_level_ir_without_the_messages() {
        let (_dir, r, adapter) = fixture();
        let head = session_head_json(&r, adapter.as_ref()).unwrap();
        // The §3 row, plus exactly the session-level fields a row cannot carry.
        assert_eq!(
            keys(&head),
            [
                "created_at",
                "cwd",
                "extra",
                "git",
                "harness",
                "id",
                "lineage",
                "message_count",
                "model",
                "path",
                "size_bytes",
                "system_prompt",
                "title",
                "total",
                "updated_at"
            ]
        );
        assert!(head["system_prompt"].as_str().unwrap().contains("Be terse."));
        assert_eq!(head["lineage"]["continued_in"], "fix-2");
        assert_eq!(head["git"]["branch"], "main");
        assert_eq!(head["model"], "claude-opus-5");
        assert_eq!(head["cwd"], "/w/proj");
        // `total` is the streamed index space `local_messages` windows over; `message_count` stays
        // the catalog's conversational count (they mean different things and must not be merged).
        let all = adapter.parse(&r).unwrap().messages.len();
        assert_eq!(head["total"].as_u64().unwrap() as usize, all);
        assert_eq!(head["message_count"], 3);
        assert!(head.get("messages").is_none(), "the head carries no transcript");
    }

    #[test]
    fn compactions_pair_each_boundary_with_its_summary() {
        let (_dir, r, adapter) = fixture();
        let v = compactions_json(&r).unwrap();
        assert_eq!(keys(&v), ["compactions", "harness", "id"]);
        let c = &v["compactions"][0];
        assert_eq!(
            keys(c),
            [
                "duration_ms",
                "headline",
                "index",
                "pre_span",
                "pre_tokens",
                "summary",
                "summary_index",
                "trigger"
            ]
        );
        assert_eq!(c["trigger"], "auto");
        assert_eq!(c["pre_tokens"], 148000);
        assert_eq!(c["duration_ms"], 4200);
        assert_eq!(c["summary"], "Summary of everything before.");
        // `index`/`summary_index` are in the numbering `local_messages` windows over.
        let msgs = serde_json::to_value(adapter.parse(&r).unwrap()).unwrap();
        let msgs = msgs["messages"].as_array().unwrap().clone();
        let i = c["index"].as_u64().unwrap() as usize;
        assert_eq!(msgs[i]["kind"], "compaction_boundary");
        assert_eq!(
            msgs[c["summary_index"].as_u64().unwrap() as usize]["kind"],
            "compaction_summary"
        );
        assert_eq!(c["pre_span"], json!([0, i]));
    }

    #[test]
    fn subagent_rows_cover_the_whole_forest_with_its_enrichment() {
        let (_dir, r, _a) = fixture();
        let rows = subagent_rows(&r);
        assert_eq!(rows.len(), 2, "the directly-spawned agent AND the workflow tier");
        let by_id = |id: &str| {
            rows.iter()
                .find(|v| v["agent_id"] == id)
                .unwrap_or_else(|| panic!("no {id}"))
        };

        let direct = by_id("a7");
        assert_eq!(direct["id"], "agent-a7");
        assert_eq!(direct["agent_type"], "Explore");
        assert_eq!(direct["description"], "find the seam");
        assert_eq!(direct["tool_use_id"], "toolu_1");
        assert!(direct["workflow"].is_null());
        // A sub-agent row is a session row too, so the UI can open it like any other session.
        assert!(direct["path"].as_str().unwrap().ends_with("agent-a7.jsonl"));
        assert!(direct["size_bytes"].as_u64().unwrap() > 0);

        let wf = by_id("b8");
        assert_eq!(
            wf["workflow"], "wf_99",
            "the workflow tier the flat lookup used to miss"
        );
        assert_eq!(wf["result_status"], "done");
        assert_eq!(wf["result_summary"], "lane 1 landed");
    }

    #[test]
    fn workflow_scripts_are_found_by_run_id_suffix() {
        let (_dir, r, _a) = fixture();
        let p = workflow_script_path(&r, "wf_99").expect("the recorded script");
        assert_eq!(p.file_name().unwrap(), "my-flow-wf_99.js");
        assert!(workflow_script_path(&r, "wf_00").is_none(), "unknown run → no script");
    }

    #[test]
    fn cvd_binary_resolves_to_a_cvd_path() {
        // Either a built binary under target/{release,debug}/cvd, or the bare PATH fallback — but it
        // must always end in the executable name so the spawn targets the right thing.
        let p = cvd_binary();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("cvd"));
    }
}
