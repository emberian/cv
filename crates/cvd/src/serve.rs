//! `cvd serve` — a tiny local HTTP server exposing the fleet's state as JSON.
//!
//! A static browser dashboard polls these endpoints live, turning `cvd` into the fleet's live hub.
//! Point `--web <dir>` at the `web/` frontend and `cvd serve` *also* hosts that dashboard from `/`,
//! so a single command is a complete, self-contained hub (API at `/api/*`, UI everywhere else). We
//! deliberately use a lightweight *synchronous* server ([`tiny_http`]) with a small worker-thread
//! pool rather than an async stack — the workload is a handful of pollers, and simplicity wins.
//!
//! The transcript corpus is sensitive (it can contain secrets), so the API is locked to local
//! callers: the `Host` header must name this machine (defeats DNS rebinding), and CORS is echoed
//! only for an allow-list of local origins (the served dashboard, `localhost`/`127.0.0.1` dev
//! servers, and the Tauri app) — never `*`. An optional bearer token (`--token` / `$CVD_TOKEN`)
//! additionally gates every `/api/*` request; `OPTIONS` preflight is answered with a 204.

use anyhow::Result;
use cv_core::{Flow, Harness, Message, MessageSink, ParseOptions, Session};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

/// What one request handler needs to know beyond the request itself.
struct Ctx {
    /// The configured bind host, accepted in `Host`/`Origin` checks alongside the loopback names.
    bind_host: String,
    web_root: Option<PathBuf>,
    /// When set, every `/api/*` request must carry `Authorization: Bearer <token>`.
    token: Option<String>,
}

/// Run the HTTP server until the process is killed. Never returns under normal operation.
///
/// When `web_root` is `Some(dir)`, any request that doesn't match an `/api/*` route is served as a
/// static file from `dir` (with `index.html` for `/`), so the same process hosts both the JSON API
/// and the browser dashboard.
pub fn run(host: &str, port: u16, web_root: Option<PathBuf>, token: Option<String>) -> Result<()> {
    let server = Server::http((host, port)).map_err(|e| anyhow::anyhow!("failed to bind {host}:{port}: {e}"))?;
    // `--port 0` binds an ephemeral port; the banner below must carry the REAL bound port so
    // callers (test harnesses, scripts) can parse the address instead of racing to guess a free
    // one. For a fixed port this is the same number.
    let port = server.server_addr().to_ip().map_or(port, |a| a.port());
    // Canonicalize the web root once so per-request path-traversal checks are cheap and robust.
    let web_root = web_root.and_then(|d| match d.canonicalize() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("cvd serve: --web {} is unreadable ({e}); serving API only", d.display());
            None
        }
    });
    match &web_root {
        Some(dir) => eprintln!(
            "cvd serve on http://{host}:{port} — dashboard at /, API at /api/* (web root {})",
            dir.display()
        ),
        None => eprintln!("cvd serve on http://{host}:{port} — API at /api/* (no --web; JSON only)"),
    }

    let server = Arc::new(server);
    let ctx = Arc::new(Ctx {
        bind_host: host.to_string(),
        web_root,
        token,
    });
    let mut workers = Vec::new();
    // A handful of workers is plenty for dashboard polling; they share the listener.
    for _ in 0..4 {
        let server = Arc::clone(&server);
        let ctx = Arc::clone(&ctx);
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                // A panicking handler must cost one response, not a worker for the process's life.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(request, &ctx)));
                if outcome.is_err() {
                    eprintln!("cvd serve: request handler panicked; worker continuing");
                }
            }
        }));
    }
    for w in workers {
        // If a worker panics we keep serving on the others, but join here so `run` blocks forever.
        let _ = w.join();
    }
    Ok(())
}

/// Handle one request: any error becomes a JSON error response.
fn handle(request: Request, ctx: &Ctx) {
    // DNS-rebinding guard: a hostile page can point its own domain at 127.0.0.1 and fetch us
    // same-origin, but it can't forge the Host header — reject anything that isn't our own name.
    if !host_allowed(header_value(&request, "Host").as_deref(), &ctx.bind_host) {
        let _ = request.respond(json_response(403, &json!({"error": "forbidden host"}), None));
        return;
    }
    // Echoed on API responses only when the Origin is on the local allow-list; never `*`.
    let origin = allowed_origin(header_value(&request, "Origin").as_deref(), &ctx.bind_host);

    let method = request.method().clone();
    let raw_url = request.url().to_string();

    // CORS preflight: a 204, CORS-tagged only for allowed origins. Unauthenticated by design —
    // browsers strip credentials from preflights, and it discloses nothing.
    if method == Method::Options {
        let _ = request.respond(no_content(origin.as_deref()));
        return;
    }

    // Split off the query string, then normalise the path (strip trailing slash, decode segments).
    let (path, query) = match raw_url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_url.as_str(), ""),
    };
    let trimmed = path.trim_end_matches('/');
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            urlencoding::decode(s)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| s.to_string())
        })
        .collect();

    // Only GET (besides the OPTIONS handled above) is supported.
    if method != Method::Get {
        let _ = request.respond(json_response(
            405,
            &json!({"error": "method not allowed"}),
            origin.as_deref(),
        ));
        return;
    }

    // Anything outside `/api/*` is a static-dashboard request when a web root is configured.
    let is_api = segments.first().map(|s| s == "api").unwrap_or(false);
    if !is_api {
        if let Some(root) = &ctx.web_root {
            let _ = request.respond(static_file(root, &segments));
            return;
        }
    }

    if let Some(token) = &ctx.token {
        let authed = header_value(&request, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .is_some_and(|t| t == *token);
        if !authed {
            let _ = request.respond(json_response(
                401,
                &json!({"error": "missing or invalid bearer token"}),
                origin.as_deref(),
            ));
            return;
        }
    }

    let (status, body) = route(&segments, query);
    let _ = request.respond(json_response(status, &body, origin.as_deref()));
}

/// First value of a (case-insensitively named) request header, if present.
fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// The hostname part of a `Host` header or URL authority: strips a `:port`, unbrackets IPv6.
fn host_name(authority: &str) -> &str {
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.rsplit_once(':').map(|(name, _)| name).unwrap_or(authority),
    }
}

/// A name this server answers to: the loopback names, or the configured bind host.
fn is_local_name(name: &str, bind_host: &str) -> bool {
    name == "127.0.0.1" || name == "::1" || name.eq_ignore_ascii_case("localhost") || name == bind_host
}

/// Whether a configured bind host is a loopback name/address.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Whether the `Host` header names this machine. An explicitly non-loopback bind is an opt-in to
/// remote clients whose Host we can't predict, so the check only bites on loopback binds.
fn host_allowed(host_header: Option<&str>, bind_host: &str) -> bool {
    if !is_loopback_host(bind_host) {
        return true;
    }
    host_header.is_some_and(|h| is_local_name(host_name(h), bind_host))
}

/// The specific `Origin` to echo in `Access-Control-Allow-Origin`, if it's on the allow-list:
/// the Tauri app's origins, or an http(s) origin on a local name (any port — a dev dashboard on
/// e.g. `http://localhost:5173` is same-machine).
fn allowed_origin(origin: Option<&str>, bind_host: &str) -> Option<String> {
    let o = origin?;
    if o == "tauri://localhost" || o == "http://tauri.localhost" {
        return Some(o.to_string());
    }
    let authority = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"))?;
    is_local_name(host_name(authority), bind_host).then(|| o.to_string())
}

/// Route a decoded path + raw query string to a `(status, json)` pair. Never panics.
fn route(segments: &[String], query: &str) -> (u16, Value) {
    let parts: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    match parts.as_slice() {
        ["api", "health"] => ok(json!({
            "ok": true,
            "harnesses": Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        })),

        ["api", "sessions"] => sessions(query),

        ["api", "session", harness, id] => session(harness, id),

        ["api", "session", harness, id, "messages"] => messages(harness, id, query),

        ["api", "session", harness, id, "events"] => session_events(harness, id, query),

        ["api", "session", harness, id, "compactions"] => compactions(harness, id),

        ["api", "touched"] => touched(query),

        ["api", "session", harness, id, "subagents"] => subagents(harness, id),

        ["api", "session", harness, parent, "subagent", agent] => subagent(harness, parent, agent),

        ["api", "session", harness, id, "workflow", wf, "script"] => workflow_script(harness, id, wf),

        ["api", "board", channel] => board(channel, query),

        ["api", "board", channel, "unanswered"] => board_unanswered(channel, query),

        ["api", "claims", channel] => claims(channel),

        ["api", "who", channel] => who(channel, query),

        ["api", "channels"] => match cv_core::board::channels() {
            Ok(c) => ok(json!(c)),
            Err(e) => err(500, &e.to_string()),
        },

        ["api", "tasks"] => tasks(query),

        ["api", "tasks", "debt"] => tasks_debt(query),

        ["api", "tasks", "stats"] => tasks_stats(query),

        ["api", "tasks", "inbox", who] => tasks_inbox(who),

        ["api", "task", id] => task_one(id, false),

        ["api", "task", id, "events"] => task_one(id, true),

        _ => err(404, "not found"),
    }
}

/// Replay the task log; on success hand the outcome to `f`. Log warnings ride every response so
/// a corrupted coordination log is visible from any consumer.
fn with_tasks(f: impl FnOnce(&cv_core::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match cv_core::task::replay() {
        Ok(outcome) => f(&outcome),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/tasks?state=&assignee=&repo=&all=` — task projections, oldest first.
fn tasks(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    with_tasks(|outcome| {
        let filter = cv_core::task::TaskFilter {
            state: params.get("state"),
            assignee: params.get("assignee"),
            repo: params.get("repo").map(std::path::PathBuf::from),
            include_terminal: params.get("all").is_some_and(|v| v == "1" || v == "true"),
        };
        // An unknown state string is a clear 400 (naming the vocabulary), not a silent [].
        let tasks: Vec<cv_core::task::TaskRow> = match cv_core::task::list(&outcome.model, &filter) {
            Ok(tasks) => tasks.into_iter().map(cv_core::task::TaskRow::full).collect(),
            Err(e) => return err(400, &e),
        };
        ok(json!({ "tasks": tasks, "warnings": outcome.warnings }))
    })
}

/// `GET /api/task/{id}` (full projection) / `GET /api/task/{id}/events` (raw history).
fn task_one(id: &str, events: bool) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match cv_core::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        if events {
            let history: Vec<&cv_core::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
            ok(json!({ "events": history, "warnings": outcome.warnings }))
        } else {
            let t = &outcome.model.tasks[&id];
            ok(json!({
                "task": t,
                "effective_state": cv_core::task::effective_display(t),
                "warnings": outcome.warnings,
            }))
        }
    })
}

/// `GET /api/tasks/debt?repo=` — reviewed-but-unlanded work, grouped by repo, oldest first,
/// plus the verifier heartbeat (`verified_as_of` / `verify_warning`) and any suspect lands
/// (revisions recorded Landed whose content the verifier no longer observes on upstream) —
/// the debt view is only as honest as the verifier is alive.
fn tasks_debt(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    with_tasks(|outcome| {
        let want = params.get("repo").map(std::path::PathBuf::from);
        let hb = cv_core::task::verify::read_heartbeat(&cv_core::task::tasks_dir());
        let report = cv_core::task::DebtReport::compute(&outcome.model, hb.as_ref(), want.as_deref());
        // This surface has always shipped an explicit `suspect` marker on every row (`false` on
        // debt rows, `true` on suspect rows — the dashboard keys off it); the MCP shape doesn't.
        let rows: Vec<cv_core::task::DebtRow> = report
            .debt
            .into_iter()
            .map(|r| cv_core::task::DebtRow {
                suspect: Some(false),
                ..r
            })
            .collect();
        let suspects: Vec<cv_core::task::SuspectRow> = report
            .suspects
            .iter()
            .map(cv_core::task::SuspectRow::from_suspect)
            .collect();
        ok(json!({
            "debt": rows,
            "awaiting_review": report.awaiting_review,
            "suspects": suspects,
            "verified_as_of": report.verified_as_of,
            "verify_warning": report.verify_warning,
            "warnings": outcome.warnings,
        }))
    })
}

/// `GET /api/tasks/stats?repo=` — fleet batting averages: per-endpoint and per-reviewer outcome
/// counts over the replayed model, annotated with verifier freshness. Computed from observed
/// events only (landed = git-verified); informational, never a gate.
fn tasks_stats(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    with_tasks(|outcome| {
        let want = params.get("repo").map(std::path::PathBuf::from);
        let hb = cv_core::task::verify::read_heartbeat(&cv_core::task::tasks_dir());
        let stats = cv_core::task::FleetStats::compute(&outcome.model, hb.as_ref(), want.as_deref());
        let mut v = match serde_json::to_value(&stats) {
            Ok(v) => v,
            Err(e) => return err(500, &e.to_string()),
        };
        v["warnings"] = json!(outcome.warnings);
        ok(v)
    })
}

/// `GET /api/tasks/inbox/{who}` — what needs this endpoint.
fn tasks_inbox(who: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let entries = cv_core::task::InboxRow::compute(&outcome.model, who);
        ok(json!({ "inbox": entries, "warnings": outcome.warnings }))
    })
}

/// `GET /api/sessions?harness=&cwd=&limit=` — session metadata, filtered + newest-first.
fn sessions(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let harness = params.get("harness");
    let cwd = params.get("cwd");
    let limit = params.get("limit").and_then(|l| l.parse::<usize>().ok());

    // Validate harness up front so a typo is a clear 400 rather than silently empty.
    let harness = match harness {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            None => return err(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };

    // The fast catalog read (~ms when warm); transparently escalates to a full scan when cold.
    let mut refs = cv_core::sessions();
    if let Some(h) = harness {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .map(|c| c.to_string_lossy().contains(needle.as_str()))
                .unwrap_or(false)
        });
    }
    // Newest activity first (updated_at, falling back to created_at).
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
    if let Some(n) = limit {
        refs.truncate(n);
    }

    let out: Vec<Value> = refs
        .iter()
        .map(|r| {
            // The ONE session-row shape (docs/INTERFACE-V2.md §3) — the same keys `cv ls --json`
            // and the desktop app's `local_sessions` emit. `path` and `size_bytes` were missing
            // here, so a consumer could not tell the daemon's rows from the CLI's without special
            // casing, which is exactly what §3 exists to prevent.
            json!({
                "id": r.id,
                "harness": r.harness.as_str(),
                "path": r.path.to_string_lossy(),
                "cwd": r.cwd.as_ref().map(|c| c.to_string_lossy()),
                "title": r.title,
                "created_at": r.created_at,
                "updated_at": r.updated_at,
                "message_count": r.message_count,
                "size_bytes": std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0),
            })
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}` — the full parsed session, or 404.
fn session(harness: &str, id: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, adapter))) => match adapter.parse(&r) {
            Ok(session) => match serde_json::to_value(&session) {
                Ok(v) => ok(v),
                Err(e) => err(500, &e.to_string()),
            },
            Err(e) => err(500, &format!("parse failed: {e:#}")),
        },
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// Collects the message window `[start, end)` as serialized IR, never holding more than the
/// window: out-of-window messages pass by as unmaterialized lazy handles, and the stream stops
/// at `end` (after noting whether a message exists there — the `has_more` probe).
struct WindowSink {
    resolver: cv_core::Resolver,
    /// Index of the next message to arrive (pre-set to `start` on the seek path, 0 on the full).
    idx: usize,
    start: usize,
    /// Exclusive window end; `None` streams to EOF.
    end: Option<usize>,
    msgs: Vec<Value>,
    /// A message at `end` exists (so the window isn't the tail of the session).
    more: bool,
    /// Session metadata the stream delivered (codex meta / the seek store's snapshot / the
    /// parse-bridge), when any did — claude's native stream emits none.
    meta: Option<Value>,
    fail: Option<String>,
}

impl MessageSink for WindowSink {
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(json!({
                "title": s.title,
                "model": s.model,
                "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
            }));
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.idx;
        self.idx += 1;
        if idx < self.start {
            return Flow::Continue; // before the window: skip without touching content bytes
        }
        if self.end.is_some_and(|e| idx >= e) {
            self.more = true;
            return Flow::Stop; // at the window's end: note the existence, read no further
        }
        let mut m = m;
        m.materialize(&self.resolver); // resolve lazy spans for just this message
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

/// `GET /api/session/{harness}/{id}/messages?start=&end=&extra=` — the window `[start, end)` of the
/// session's messages as IR JSON, without parsing the rest of a huge transcript when possible.
///
/// First tries the seekable-session store ([`cv_core::offsets::stream_range`]: jump straight to
/// message `start`'s recorded byte offset); with no/stale offsets it falls back to a full stream
/// that still **stops at `end`** (the `cv show --range` pattern), so the tail is never read.
/// The reply carries the window's actual indices, `total_known` (+ `total` when the stream
/// reached EOF so the count is exact), and `has_more`.
///
/// `extra=1` additionally keeps each message's harness `extra` map (e.g. `compactMetadata` on a
/// compaction boundary) — the structure view pages through with this on to find the seams.
fn messages(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    let params = Query::parse(query);
    // Window bounds must parse cleanly — a typo'd index is a clear 400, not a silent full read.
    let idx_param = |key: &str| -> Result<Option<usize>, (u16, Value)> {
        match params.get(key) {
            None => Ok(None),
            Some(v) => v
                .parse::<usize>()
                .map(Some)
                .map_err(|_| err(400, &format!("{key} must be a non-negative integer, got {v:?}"))),
        }
    };
    let start = match idx_param("start") {
        Ok(v) => v.unwrap_or(0),
        Err(e) => return e,
    };
    let end = match idx_param("end") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if end.is_some_and(|e| e < start) {
        return err(400, "end must be >= start");
    }
    // `extra=1` keeps each message's harness-specific `extra` map (subtype, compactMetadata, the
    // toolUseResult sidecar, …). Off by default — the lean windowed read is for bulk transcript
    // paging — but a structure view (compaction boundaries live in `extra.compactMetadata`) opts in.
    let want_extra = params
        .get("extra")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let (r, adapter) = match cv_core::find(id, Some(h)) {
        Ok(Some(hit)) => hit,
        Ok(None) => return err(404, "session not found"),
        Err(e) => return err(500, &e.to_string()),
    };

    // Lazy spans (so unread giant fields aren't materialized) plus, when asked, the `extra` maps.
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
    // Ask the stream for one message past the window so `has_more` is known either way.
    let probe = end.map(|e| e.saturating_add(1));
    let seeked = if start > 0 {
        match cv_core::offsets::stream_range(&r, start, probe, &opts, &mut sink) {
            Ok(seeked) => seeked,
            Err(e) => return err(500, &format!("range read failed: {e:#}")),
        }
    } else {
        false
    };
    if !seeked {
        sink.idx = 0; // full stream counts from the top (skipping pre-window messages cheaply)
        if let Err(e) = adapter.stream(&r, &opts, &mut sink) {
            return err(500, &format!("stream failed: {e:#}"));
        }
    }
    if let Some(f) = sink.fail {
        return err(500, &f);
    }

    // The count is exact when the stream ran to EOF. The one blind spot: a seek-read that
    // delivered nothing can't distinguish "EOF exactly at start" from "start past EOF".
    let total_known = !sink.more && (!seeked || !sink.msgs.is_empty());
    ok(json!({
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

/// `GET /api/session/{harness}/{id}/events?kind=` — the session's extracted tool events
/// (file edits/reads, commands, errors) from the event catalog, in transcript order. A stale
/// or missing catalog row is (re)ingested on the spot, exactly like `cv events`.
fn session_events(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    let kind = Query::parse(query).get("kind");
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => {
            use cv_core::events;
            if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
                if let Err(e) = events::ingest_ref(&r) {
                    return err(500, &format!("event ingest failed: {e:#}"));
                }
            }
            let out: Vec<Value> = events::events_for(r.harness.as_str(), &r.id, kind.as_deref())
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
            ok(json!(out))
        }
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/session/{harness}/{id}/compactions` — every compaction boundary in the session, in
/// transcript order, each as
/// `{ index, summary_index, ts, trigger, pre_tokens, duration_ms, summary, pre_span: [start,end] }`.
///
/// Built on [`cv_core::compaction::detect`], which streams the whole transcript (complete coverage,
/// memory-safe even on a multi-GB session) and — crucially — pairs each `compact_boundary` with the
/// `isCompactSummary` message that seeds the next window, keeping that **summary text** (the
/// load-bearing artifact: how a continued agent recovers the context it can no longer see). The
/// `pre_span` is the `[start, boundary)` message range that was compacted away — what to read to
/// retrieve the pre-compaction context. `index`/`summary_index` are in the same numbering
/// `/messages` uses.
fn compactions(harness: &str, id: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    let r = match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => r,
        Ok(None) => return err(404, "session not found"),
        Err(e) => return err(500, &e.to_string()),
    };
    // Keep the summary text — it's the whole point of surfacing compaction (retrieving the
    // pre-compaction context). `detect` holds only the small per-boundary rows + bounded summaries.
    let boundaries = match cv_core::compaction::detect(&r, true) {
        Ok(b) => b,
        Err(e) => return err(500, &format!("compaction scan failed: {e:#}")),
    };
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
    ok(json!({
        "harness": r.harness.as_str(),
        "id": r.id,
        "compactions": out,
    }))
}

/// `GET /api/touched?path=&edits_only=` — sessions whose tool events touched a file (exact or
/// suffix path match), newest activity first. Empty on a cold catalog, like `cv touched`.
fn touched(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(path) = params.get("path") else {
        return err(400, "missing path parameter");
    };
    let edits_only = params
        .get("edits_only")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let out: Vec<Value> = cv_core::events::sessions_touching(&path, edits_only)
        .iter()
        .map(|t| {
            // Same keys as `cv touched --json`, including the sub-agent provenance trio the
            // struct has always carried: without it a caller cannot tell whether the session that
            // touched a file was a top-level run or one lane of a workflow.
            json!({
                "harness": t.harness,
                "session_id": t.session_id,
                "title": t.title,
                "edits": t.edits,
                "reads": t.reads,
                "last_ts": t.last_ts,
                "agent_id": t.agent_id,
                "parent_id": t.parent_id,
                "workflow": t.workflow,
            })
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/subagents` — lightweight refs for the sub-agents this session
/// spawned (e.g. Claude Code Task sub-agents), newest first. Empty array if none.
fn subagents(harness: &str, id: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => {
            // The full forest (directly-spawned + Workflow sub-agents), each enriched with its
            // meta sidecar (agent type / task / spawning tool_use) and the workflow's journaled
            // outcome — the flat `subagents_of` would miss the entire workflow tier.
            let out: Vec<Value> = cv_core::subagent_tree_of(&r)
                .iter()
                .map(|s| {
                    json!({
                        "id": s.session.id,
                        "agent_id": s.agent_id(),
                        "harness": s.session.harness.as_str(),
                        "cwd": s.session.cwd.as_ref().map(|c| c.to_string_lossy()),
                        "title": s.session.title,
                        "updated_at": s.session.updated_at,
                        "created_at": s.session.created_at,
                        "message_count": s.session.message_count,
                        "agent_type": s.agent_type,
                        "description": s.description,
                        "tool_use_id": s.tool_use_id,
                        "workflow": s.workflow,
                        "result_status": s.result_status,
                        "result_summary": s.result_summary,
                    })
                })
                .collect();
            ok(json!(out))
        }
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/session/{harness}/{parent}/subagent/{agent}` — the full parsed transcript of one
/// sub-agent (sub-agents aren't in the main pool, so they're loaded relative to their parent).
fn subagent(harness: &str, parent: &str, agent: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(parent, Some(h)) {
        Ok(Some((r, adapter))) => {
            // Resolve across the whole forest (incl. Workflow agents) by full id or bare agentId.
            match cv_core::subagent_tree_of(&r)
                .into_iter()
                .find(|s| s.session.id == agent || s.agent_id() == agent)
                .map(|s| s.session)
            {
                Some(sr) => match adapter.parse(&sr) {
                    Ok(session) => match serde_json::to_value(&session) {
                        Ok(v) => ok(v),
                        Err(e) => err(500, &e.to_string()),
                    },
                    Err(e) => err(500, &format!("parse failed: {e:#}")),
                },
                None => err(404, "subagent not found"),
            }
        }
        Ok(None) => err(404, "parent session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/session/{harness}/{id}/workflow/{wf}/script` — the driving script Claude Code recorded
/// for a workflow run, as `{ workflow, name, source }`. Scripts live in the session's sidecar tree
/// at `<session>/workflows/scripts/<slug>-<wf>.js`; we match the run id (`wf_…`) as the filename
/// suffix (the slug prefix varies per workflow). 404 if there's no script for that run.
fn workflow_script(harness: &str, id: &str, wf: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    // Reject a path-y run id up front — it's only ever a `wf_…` token, never a path component.
    if wf.contains('/') || wf.contains("..") || wf.is_empty() {
        return err(400, "invalid workflow id");
    }
    let r = match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => r,
        Ok(None) => return err(404, "session not found"),
        Err(e) => return err(500, &e.to_string()),
    };
    // `<dir>/<stem>/workflows/scripts/` next to the transcript file.
    let Some(stem) = r.path.file_stem().and_then(|s| s.to_str()) else {
        return err(404, "session has no sidecar tree");
    };
    let scripts_dir = match r.path.parent() {
        Some(d) => d.join(stem).join("workflows").join("scripts"),
        None => return err(404, "session has no sidecar tree"),
    };
    // The filename is `<some-slug>-<wf>.js`; match by the run-id suffix.
    let found = std::fs::read_dir(&scripts_dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("js"))
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == wf || s.ends_with(&format!("-{wf}")))
            })
    });
    let Some(path) = found else {
        return err(404, "no script recorded for that workflow run");
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => ok(json!({
            "workflow": wf,
            "name": path.file_name().and_then(|n| n.to_str()),
            "source": source,
        })),
        Err(e) => err(500, &format!("could not read script: {e}")),
    }
}

/// `GET /api/board/{channel}?since=&limit=` — board messages.
fn board(channel: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let since = params.get("since");
    let limit = params.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(0); // 0 == unlimited in cv_core::board::read.
    match cv_core::board::read(channel, since.as_deref(), limit) {
        Ok(msgs) => ok(json!(msgs)),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/board/{channel}/unanswered?within_secs=` — requests with zero replies, oldest
/// first, each with its age in seconds (default window 24h).
fn board_unanswered(channel: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let within = params
        .get("within_secs")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(86_400);
    match cv_core::board::unanswered_requests(channel, Duration::from_secs(within)) {
        Ok(rows) => {
            let out: Vec<Value> = rows
                .iter()
                .map(|(m, age)| json!({ "message": m, "age_secs": age.num_seconds() }))
                .collect();
            ok(json!(out))
        }
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/claims/{channel}` — active leases as `{key, owner, expires_at}`.
fn claims(channel: &str) -> (u16, Value) {
    match cv_core::board::active_claims(channel) {
        Ok(claims) => {
            let out: Vec<Value> = claims
                .into_iter()
                .map(|(key, owner, expires_at)| json!({ "key": key, "owner": owner, "expires_at": expires_at }))
                .collect();
            ok(json!(out))
        }
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/who/{channel}?within_secs=` — recently-present agents.
fn who(channel: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let within = params
        .get("within_secs")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(cv_core::board::WHO_WINDOW_SECS);
    match cv_core::board::who(channel, Duration::from_secs(within)) {
        Ok(names) => ok(json!(names)),
        Err(e) => err(500, &e.to_string()),
    }
}

// --- small helpers ---------------------------------------------------------

/// A trivially-parsed query string. Avoids pulling a URL crate for our handful of flat params.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(q: &str) -> Self {
        let pairs = q
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let dec = |s: &str| {
                    urlencoding::decode(&s.replace('+', " "))
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| s.to_string())
                };
                (dec(k), dec(v))
            })
            .collect();
        Query(pairs)
    }

    /// First non-empty value for `key`, if any.
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }
}

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

/// Serve a static file from the configured web root for a non-`/api` request.
///
/// `segments` is the already-decoded path (so `["web", "components", "cv-forest.js"]`). An empty
/// path maps to `index.html`. The resolved path is canonicalized and must stay inside the (already
/// canonical) root — any traversal (`..`, symlink escape, absolute injection) yields a 403. A
/// missing file falls back to `index.html` (so client-side deep links work), and only a missing
/// index is a 404. The `Content-Type` is inferred from the extension; static assets are not
/// CORS-tagged (same-origin) — only the JSON API is.
fn static_file(root: &Path, segments: &[String]) -> Response<std::io::Cursor<Vec<u8>>> {
    let index = root.join("index.html");
    let resolved = if segments.is_empty() {
        index.clone()
    } else {
        // Reject obviously hostile components before touching the filesystem.
        if segments.iter().any(|s| s == ".." || s.contains('\0')) {
            return text_response(403, "text/plain; charset=utf-8", b"forbidden".to_vec());
        }
        let mut p = root.to_path_buf();
        for s in segments {
            p.push(s);
        }
        p
    };

    // Canonicalize and confine to the root (defeats symlink escapes too). On a miss, serve the SPA
    // index so deep links resolve client-side.
    let serve_path = match resolved.canonicalize() {
        Ok(c) if c.starts_with(root) && c.is_file() => c,
        _ => match index.canonicalize() {
            Ok(c) if c.starts_with(root) && c.is_file() => c,
            _ => return text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
        },
    };

    match std::fs::read(&serve_path) {
        Ok(bytes) => text_response(200, content_type_for(&serve_path), bytes),
        Err(_) => text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

/// Guess a `Content-Type` from a file extension — enough for the dashboard's asset set.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A raw-bytes response with a `Content-Type` and no CORS headers (static assets are same-origin).
fn text_response(status: u16, content_type: &str, body: Vec<u8>) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut resp = Response::from_data(body).with_status_code(status);
    resp.add_header(header("Content-Type", content_type));
    resp
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

/// Build a JSON response, CORS-tagged for `origin` when one was allowed.
fn json_response(status: u16, body: &Value, origin: Option<&str>) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut resp = Response::from_data(data).with_status_code(status);
    for h in cors_headers(origin) {
        resp.add_header(h);
    }
    resp.add_header(header("Content-Type", "application/json"));
    resp
}

/// A 204 No Content, CORS-tagged for `origin` when one was allowed — for `OPTIONS` preflight.
fn no_content(origin: Option<&str>) -> Response<std::io::Empty> {
    let mut resp = Response::empty(204);
    for h in cors_headers(origin) {
        resp.add_header(h);
    }
    resp
}

/// CORS headers echoing the one allowed `origin` — an unlisted origin gets none (the browser then
/// refuses to share the response with it). `Vary: Origin` keeps caches from mixing the two.
fn cors_headers(origin: Option<&str>) -> Vec<Header> {
    let mut headers = vec![header("Vary", "Origin")];
    if let Some(o) = origin {
        headers.push(header("Access-Control-Allow-Origin", o));
        headers.push(header("Access-Control-Allow-Methods", "GET, OPTIONS"));
        headers.push(header("Access-Control-Allow-Headers", "Authorization, Content-Type"));
    }
    headers
}

fn header(field: &str, value: &str) -> Header {
    // These are all static, valid headers; the parse cannot fail in practice.
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"X-Invalid"[..], &b"1"[..]).unwrap())
}
