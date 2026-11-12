//! Integration tests for the `cv serve` HTTP endpoints (status codes, JSON shapes, query
//! parsing) — all hermetic via a temp `$HOME` + `$CLUSTERVISION_HOME` passed only to child
//! processes. (The `cv archive` tests live in `archive.rs`; the two-session fixture corpus they
//! share with these tests is in `common/mod.rs`.)

mod common;

use common::world;
use cv_core::rows::SEARCH_ROW_KEYS;
use cv_testkit::http::status as raw_status;
use cv_testkit::World;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// A third, tool-heavy session (separate from `common::write_fixtures` so the archive tests' "2
/// discovered" counts hold): 6 messages with an Edit, a failing Bash, a clean Bash, and a
/// closing text — the raw material for the messages/events/touched endpoint tests.
fn write_gamma(w: &World) {
    let lines = [
        json!({"type": "user", "uuid": "g1", "timestamp": "2026-04-01T08:00:00Z",
               "cwd": "/work/proj",
               "message": {"role": "user", "content": "please refactor the pelican module"}}),
        json!({"type": "assistant", "uuid": "g2", "timestamp": "2026-04-01T08:01:00Z",
               "message": {"role": "assistant", "content": [
                   {"type": "tool_use", "id": "t1", "name": "Edit",
                    "input": {"file_path": "src/pelican.rs", "old_string": "a", "new_string": "b"}}]}}),
        json!({"type": "user", "uuid": "g3", "timestamp": "2026-04-01T08:02:00Z",
               "message": {"role": "user", "content": [
                   {"type": "tool_result", "tool_use_id": "t1",
                    "content": "error[E0308]: mismatched types", "is_error": true}]}}),
        json!({"type": "assistant", "uuid": "g4", "timestamp": "2026-04-01T08:03:00Z",
               "message": {"role": "assistant", "content": [
                   {"type": "tool_use", "id": "t2", "name": "Bash",
                    "input": {"command": "cargo test -p pelican"}}]}}),
        json!({"type": "user", "uuid": "g5", "timestamp": "2026-04-01T08:04:00Z",
               "message": {"role": "user", "content": [
                   {"type": "tool_result", "tool_use_id": "t2",
                    "content": "all tests pass", "is_error": false}]}}),
        json!({"type": "assistant", "uuid": "g6", "timestamp": "2026-04-01T08:05:00Z",
               "message": {"role": "assistant", "content": [
                   {"type": "text", "text": "done refactoring"}]}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(w.transcript("gammasess"), body).unwrap();
}

/// A session with a compaction boundary (a `compact_boundary` system record carrying
/// `compactMetadata`), so the `/compactions` endpoint has a boundary to surface.
fn write_compacted(w: &World) {
    let lines = [
        json!({"type": "user", "uuid": "c1", "timestamp": "2026-05-01T08:00:00Z",
               "message": {"role": "user", "content": "long conversation begins"}}),
        json!({"type": "assistant", "uuid": "c2", "timestamp": "2026-05-01T08:01:00Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "working"}]}}),
        json!({"type": "system", "uuid": "c3", "subtype": "compact_boundary",
               "timestamp": "2026-05-01T08:02:00Z", "content": "Conversation compacted",
               "compactMetadata": {"trigger": "manual", "preTokens": 900000, "postTokens": 12000,
                                   "durationMs": 120000, "preCompactDiscoveredTools": ["Bash", "Read"]}}),
        // The seed of the next window: an `isCompactSummary` message whose parentUuid is the
        // boundary's uuid — its body is the summary detect() pairs and keeps.
        json!({"type": "user", "uuid": "c4", "parentUuid": "c3", "isCompactSummary": true,
               "timestamp": "2026-05-01T08:03:00Z",
               "message": {"role": "user", "content": "Summary: the user asked for X; we did Y."}}),
        json!({"type": "assistant", "uuid": "c5", "timestamp": "2026-05-01T08:04:00Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "resumed"}]}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(w.transcript("compactsess"), body).unwrap();
}

/// Plant a workflow driving-script sidecar for `alphasess` so the `/workflow/{wf}/script`
/// endpoint has something to serve: `<session>/workflows/scripts/<slug>-<wf>.js`.
fn write_workflow_script(w: &World, wf: &str, body: &str) {
    let dir = w.home.join(".claude/projects/-work-proj/alphasess/workflows/scripts");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("my-cool-flow-{wf}.js")), body).unwrap();
}

/// Build a tiny static web root (mirroring the repo's `web/`) so `serve --web` can be tested:
/// an `index.html`, a JS asset under a subdir, and a "secret" file *outside* the root that a
/// path-traversal attempt must never reach.
fn write_web_root(w: &World) -> PathBuf {
    let web = w.base.join("web");
    fs::create_dir_all(web.join("components")).unwrap();
    fs::write(web.join("index.html"), "<!doctype html><title>cv dash</title>").unwrap();
    fs::write(web.join("components/cv-forest.js"), "export const FOREST = 1;\n").unwrap();
    // A file a `../` escape would try to read; it lives beside (not under) the web root.
    fs::write(w.base.join("secret.txt"), "TOP SECRET").unwrap();
    web
}

// ───────────────────────────── serve ─────────────────────────────

fn http(port: u16, method: &str, path: &str) -> (u16, String) {
    let raw = raw_request(port, method, path, &[]);
    (raw_status(&raw), cv_testkit::http::parts(&raw).1.to_owned())
}

/// Include Content-Type when the response headers are part of the contract.
fn get_with_content_type(port: u16, path: &str) -> (u16, String, String) {
    let raw = raw_get(port, path);
    let (head, body) = cv_testkit::http::parts(&raw);
    let content_type = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.trim())
        .unwrap_or_default();
    (raw_status(&raw), content_type.to_owned(), body.to_owned())
}

fn get_json(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = http(port, "GET", path);
    let v = serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path}: non-JSON body {body:?}: {e}"));
    (status, v)
}

#[test]
fn full_session_downloads_match_native_exports_beyond_loaded_page() {
    let w = world("full-session-download");
    let rows: Vec<_> = (0..137)
        .map(|n| {
            json!({"type":"user", "uuid":format!("u{n}"),
        "message":{"role":"user","content":format!("message-{n}-λ")}})
        })
        .collect();
    w.write_session("download", &rows);
    let (port, _server) = spawn_serve(&w);
    let (status, page) = get_json(port, "/api/session/claude/download/messages?start=0&end=2");
    assert_eq!(status, 200);
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    for (format, native, content_type, extension) in [
        ("markdown", "md", "text/markdown; charset=utf-8", "md"),
        ("opensession", "json", "application/json", "opensession.json"),
    ] {
        let (expected, _) = w.cv_ok(&["export", "download", "--harness", "claude", "--format", native]);
        let raw = raw_get(port, &format!("/api/session/claude/download/export?format={format}"));
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
        let headers = headers.to_ascii_lowercase();
        assert!(headers.contains(&format!("content-type: {content_type}")));
        assert!(headers.contains(&format!(
            "content-disposition: attachment; filename=\"cv-claude-download.{extension}\""
        )));
        assert!(headers.contains("cache-control: no-store"));
        assert_eq!(body, expected, "native {native} export parity");
        assert!(
            body.contains("message-136-λ"),
            "download silently stopped at loaded prefix"
        );
    }
    assert_eq!(get_json(port, "/api/session/claude/download/export?format=html").0, 400);
    assert_eq!(
        get_json(port, "/api/session/claude/missing/export?format=markdown").0,
        404
    );
}

/// A child process killed on drop, so a failing test never leaks a server.
struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `cv serve` on a free port and wait for the listener. Returns the port and the reaper
/// keeping the child alive (drop it to kill the server).
fn spawn_serve(w: &World) -> (u16, Reaper) {
    spawn_serve_cfg(w, &[], &[])
}

/// Like [`spawn_serve`] but with extra `serve` args (e.g. `["--web", dir]`) and extra child env
/// vars (e.g. `CVD_TOKEN`).
///
/// Binds `--port 0` (the OS assigns a genuinely free ephemeral port — no bind/release/rebind race
/// under parallel tests) and parses the REAL port out of the startup banner on stderr. The child
/// is `try_wait()`ed inside the wait loop so a server that dies at startup fails the test with its
/// exit status immediately instead of as connection-reset noise later.
fn spawn_serve_cfg(w: &World, extra: &[&str], envs: &[(&str, &str)]) -> (u16, Reaper) {
    let mut command = w.cv_command();
    command
        .args(["serve", "--port", "0"])
        .args(extra)
        .envs(envs.iter().copied());
    spawn_serve_command(command)
}

/// Spawn a prepared `cv serve` command (whatever wraps it) and wait for its listener.
fn spawn_serve_command(mut command: std::process::Command) -> (u16, Reaper) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cv serve should spawn");

    // Read the banner (`cv serve on http://host:PORT — …`) off stderr on a side thread; keep
    // draining afterwards so the child never blocks on a full pipe.
    let stderr = child.stderr.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut sink);
    });
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let banner_port = |line: &str| -> Option<u16> {
        // "cv serve on http://127.0.0.1:PORT — …": take the port from the URL.
        let rest = line.split("http://").nth(1)?;
        let hostport = rest.split_whitespace().next()?;
        hostport.rsplit(':').next()?.parse().ok()
    };

    let mut reaper = Reaper(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    let port = loop {
        if let Some(status) = reaper.0.try_wait().expect("try_wait cv serve") {
            panic!("cv serve exited at startup ({status}); stderr so far: {seen:?}");
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if line.starts_with("cv serve on ") {
                    if let Some(p) = banner_port(&line) {
                        break p;
                    }
                }
                seen.push(line);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Stream closed without a banner — the exit branch above will report the status.
            }
        }
        assert!(
            Instant::now() < deadline,
            "cv serve never printed its banner; stderr so far: {seen:?}"
        );
    };
    // Keep draining stderr so the server never blocks writing logs.
    std::thread::spawn(move || while rx.recv().is_ok() {});

    // The banner prints after the bind, so the listener is up — probe once to be sure, still
    // failing fast if the process dies between banner and accept loop.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if let Some(status) = reaper.0.try_wait().expect("try_wait cv serve") {
            panic!("cv serve died after its banner ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "cv serve banner printed :{port} but it never accepts"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (port, reaper)
}

#[test]
fn serve_endpoints() {
    let w = world("serve");
    let (port, _reaper) = spawn_serve(&w);

    // health
    let (status, v) = get_json(port, "/api/health");
    assert_eq!(status, 200);
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["harnesses"].as_array().unwrap().iter().any(|h| h == "claude"), "{v}");

    // sessions: both fixtures, newest first.
    let (status, v) = get_json(port, "/api/sessions");
    assert_eq!(status, 200);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2, "{v}");
    assert_eq!(arr[0]["id"], "betasess", "newest-first: {v}");

    // THE session row (docs/INTERFACE-V2.md §3): the same keys `cv ls --json` and the desktop
    // app's `local_sessions` emit, so a consumer never has to ask which door the row came
    // through. `path` and `size_bytes` were missing here for a release.
    let mut keys: Vec<&str> = arr[0].as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
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
        "session row must match §3 exactly: {v}"
    );
    assert!(arr[0]["path"].as_str().is_some_and(|p| p.ends_with(".jsonl")), "{v}");
    assert!(arr[0]["size_bytes"].as_u64().is_some_and(|n| n > 0), "{v}");
    // One spelling of an instant on every door. serde's chrono default writes UTC as `…Z` while
    // `cv ls --json` and `/api/search` write `…+00:00`, so the same timestamp came back three
    // ways and a consumer comparing strings across doors found differences that were not there.
    for k in ["created_at", "updated_at"] {
        let t = arr[0][k].as_str().unwrap_or_default();
        assert!(
            t.ends_with("+00:00"),
            "{k} must be rfc3339 like the CLI's, got {t:?}: {v}"
        );
    }

    // limit
    let (status, v) = get_json(port, "/api/sessions?limit=1");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");

    // A malformed selector is a caller error rather than an unbounded read.
    let (status, v) = get_json(port, "/api/sessions?limit=banana");
    assert_eq!(status, 400);
    let error = v["error"].as_str().unwrap();
    assert!(error.contains("limit") && error.contains("non-negative integer"), "{v}");

    // harness filter + a typo'd harness is a clear 400.
    let (status, v) = get_json(port, "/api/sessions?harness=claude");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let (status, v) = get_json(port, "/api/sessions?harness=warpdrive");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("unknown harness"), "{v}");

    // cwd filter
    let (status, v) = get_json(port, "/api/sessions?cwd=%2Fwork%2Fproj");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let (_, v) = get_json(port, "/api/sessions?cwd=nowhere");
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");

    // one session: found / unknown id / unknown harness.
    let (status, v) = get_json(port, "/api/session/claude/alphasess");
    assert_eq!(status, 200);
    assert_eq!(v["id"], "alphasess", "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{v}");
    let (status, v) = get_json(port, "/api/session/claude/zzz-not-here");
    assert_eq!(status, 404);
    assert!(v["error"].is_string(), "{v}");
    let (status, _) = get_json(port, "/api/session/warpdrive/alphasess");
    assert_eq!(status, 400);

    // subagents of a session with none: an empty array, not an error.
    let (status, v) = get_json(port, "/api/session/claude/alphasess/subagents");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");
    let (status, _) = get_json(port, "/api/session/claude/alphasess/subagent/ghost");
    assert_eq!(status, 404);

    // Removed and unknown routes → 404; non-GET → 405; OPTIONS preflight → 204 with CORS.
    for route in [
        "/api/board/fleet",
        "/api/claims/fleet",
        "/api/who/fleet?within_secs=60",
        "/api/channels",
        "/api/definitely/not/a/route",
    ] {
        let (status, v) = get_json(port, route);
        assert_eq!(status, 404, "{route}: {v}");
        assert!(v["error"].is_string(), "{route}: {v}");
    }
    let (status, _) = http(port, "POST", "/api/health");
    assert_eq!(status, 405);
    let (status, body) = http(port, "OPTIONS", "/api/health");
    assert_eq!(status, 204);
    assert!(body.is_empty(), "{body}");
    // An allowed (local) Origin is echoed on data responses too — never a wildcard.
    let raw = raw_request(port, "GET", "/api/health", &[("Origin", "http://localhost:5173")]);
    assert!(
        raw.to_ascii_lowercase()
            .contains("access-control-allow-origin: http://localhost:5173"),
        "CORS echo missing:\n{raw}"
    );
    assert!(!raw.contains("Access-Control-Allow-Origin: *"), "{raw}");
}

/// The Wave-2 endpoints: windowed messages (full-stream fallback that stops at the window's
/// end), per-session events with on-the-spot ingest, and the touched lookup.
#[test]
fn serve_messages_events_touched() {
    let w = world("window");
    write_gamma(&w);
    let (port, _reaper) = spawn_serve(&w);

    // A middle window [2, 4): exactly 2 messages, indices echoed back, more beyond it, and the
    // total unknown (the stream stopped at the window's end without reaching EOF).
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=2&end=4");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["start"], 2, "{v}");
    assert_eq!(v["end"], 4, "{v}");
    let msgs = v["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(v["has_more"], true, "{v}");
    assert_eq!(v["total_known"], false, "{v}");
    assert!(v["total"].is_null(), "{v}");
    // Message 3 (the second of the window) is the Bash tool_use turn. A BLOCK is tagged `type`
    // (the word every harness uses on the wire); the MESSAGE separately carries `kind`/`origin`
    // naming what the turn is and where it came from (INTERFACE-V2 §4).
    let blocks = msgs[1]["content"].as_array().expect("content blocks");
    assert!(
        blocks.iter().any(|b| b["type"] == "tool_use" && b["name"] == "Bash"),
        "{v}"
    );
    assert!(
        blocks.iter().all(|b| b.get("kind").is_none()),
        "a block is tagged `type`, never `kind`: {v}"
    );
    assert_eq!(msgs[1]["role"], "assistant", "{v}");
    assert_eq!(msgs[1]["kind"], "reply", "{v}");
    assert_eq!(msgs[1]["origin"], "model", "{v}");
    // Its window-mate (msg 2) is the failed Edit's tool result fed back to the model.
    assert_eq!(msgs[0]["role"], "tool", "{v}");
    assert_eq!(msgs[0]["kind"], "tool_result", "{v}");
    assert!(
        msgs[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "tool_result" && b["is_error"] == true),
        "{v}"
    );

    // The whole session, kind by kind: the human prompt, the model replies, the tool results.
    let (status, all) = get_json(port, "/api/session/claude/gammasess/messages");
    assert_eq!(status, 200, "{all}");
    let kinds: Vec<&str> = all["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().expect("every message carries a kind"))
        .collect();
    assert_eq!(
        kinds,
        ["prompt", "reply", "tool_result", "reply", "tool_result", "reply"],
        "{all}"
    );
    assert_eq!(all["messages"][0]["origin"], "human", "a typed prompt: {all}");

    // A tail window: the stream reaches EOF, so the total is exact and nothing more remains.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=4");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!((v["start"].as_u64(), v["end"].as_u64()), (Some(4), Some(6)), "{v}");
    assert_eq!(v["has_more"], false, "{v}");
    assert_eq!(v["total_known"], true, "{v}");
    assert_eq!(v["total"], 6, "{v}");

    // No bounds at all: the whole session, total exact.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages");
    assert_eq!(status, 200);
    assert_eq!(v["messages"].as_array().unwrap().len(), 6, "{v}");
    assert_eq!(v["total"], 6, "{v}");

    // A window past EOF is empty, not an error.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=50&end=60");
    assert_eq!(status, 200);
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["has_more"], false, "{v}");

    // Bad windows / unknowns are clear errors.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=4&end=2");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("end"), "{v}");
    let (status, _) = get_json(port, "/api/session/claude/gammasess/messages?start=banana");
    assert_eq!(status, 400);
    let (status, _) = get_json(port, "/api/session/claude/zzz-not-here/messages");
    assert_eq!(status, 404);
    let (status, _) = get_json(port, "/api/session/warpdrive/gammasess/messages");
    assert_eq!(status, 400);

    // Events: ingested on the spot, classified rows in transcript order.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/events");
    assert_eq!(status, 200);
    let events = v.as_array().expect("events array");
    let kind_of = |k: &str| events.iter().filter(|e| e["kind"] == k).count();
    assert!(kind_of("file_edit") >= 1, "{v}");
    assert!(kind_of("command") >= 1, "{v}");
    assert!(kind_of("error") >= 1, "{v}");
    let edit = events.iter().find(|e| e["kind"] == "file_edit").unwrap();
    assert_eq!(edit["target"], "/work/proj/src/pelican.rs", "{v}");
    assert_eq!(edit["tool"], "Edit", "{v}");
    // One spelling of an instant on every door: RFC 3339, as `cv events --json` prints it.
    assert!(edit["ts"].as_str().is_some_and(|t| t.ends_with("+00:00")), "{v}");
    let errev = events.iter().find(|e| e["kind"] == "error").unwrap();
    assert!(errev["detail"].as_str().unwrap().contains("E0308"), "{v}");

    // Kind filter narrows; unknown session is a 404.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/events?kind=command");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().iter().all(|e| e["kind"] == "command"), "{v}");
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, _) = get_json(port, "/api/session/claude/zzz-not-here/events");
    assert_eq!(status, 404);

    // Touched: suffix path match finds the session; edits_only keeps it (it has an edit);
    // an untouched file is an empty array; a missing path param is a 400.
    let (status, v) = get_json(port, "/api/touched?path=src%2Fpelican.rs");
    assert_eq!(status, 200);
    let rows = v.as_array().expect("touched array");
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["session_id"], "gammasess", "{v}");
    assert_eq!(rows[0]["edits"], 1, "{v}");
    assert!(
        rows[0]["last_ts"].as_str().is_some_and(|t| t.ends_with("+00:00")),
        "{v}"
    );
    // Same keys as `cv touched --json`, provenance included: without `agent_id`/`parent_id`/
    // `workflow` a caller cannot tell a top-level run from one lane of a workflow, and the trio
    // was dropped here even though `events::Touched` has always carried it.
    for k in ["agent_id", "parent_id", "workflow"] {
        assert!(
            rows[0].as_object().unwrap().contains_key(k),
            "touched row must carry {k} like the CLI's: {v}"
        );
    }
    let (status, v) = get_json(port, "/api/touched?path=src%2Fpelican.rs&edits_only=true");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, v) = get_json(port, "/api/touched?path=src%2Fnobody.rs");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/touched");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("path"), "{v}");
}

/// The workflow driving-script endpoint: matches a run id by filename suffix, 404s for an absent
/// run, and rejects a path-y run id.
#[test]
fn serve_workflow_script() {
    let w = world("wfscript");
    write_workflow_script(
        &w,
        "wf_abc123",
        "// drive the swarm\nconst phases = ['plan','build'];\n",
    );
    let (port, _reaper) = spawn_serve(&w);

    // Found by run-id suffix (filename is `my-cool-flow-wf_abc123.js`).
    let (status, v) = get_json(port, "/api/session/claude/alphasess/workflow/wf_abc123/script");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["workflow"], "wf_abc123", "{v}");
    assert_eq!(v["name"], "my-cool-flow-wf_abc123.js", "{v}");
    assert!(v["source"].as_str().unwrap().contains("drive the swarm"), "{v}");

    // A run with no recorded script → 404.
    let (status, v) = get_json(port, "/api/session/claude/alphasess/workflow/wf_missing/script");
    assert_eq!(status, 404, "{v}");
    assert!(v["error"].is_string(), "{v}");

    // A path-y / traversal run id is a clean 400, never a filesystem read.
    let (status, v) = http(
        port,
        "GET",
        "/api/session/claude/alphasess/workflow/..%2F..%2Fetc/script",
    );
    assert_eq!(status, 400, "{v}");

    // Unknown session id is a 404.
    let (status, _) = get_json(port, "/api/session/claude/zzz-nope/workflow/wf_abc123/script");
    assert_eq!(status, 404);
}

/// The `/compactions` scan: every boundary with its metadata, plus `extra=1` on `/messages`
/// surfacing the boundary's `compactMetadata` in-band.
/// `/head` answers what a session knows about ITSELF, which a windowed read structurally cannot:
/// the window stops once it is full, so a session-level fact recorded later in the transcript is
/// never reached. It also pins the §3 row keys, so this door and `cv ls --json` agree.
#[test]
fn serve_session_head() {
    let w = world("head");
    let (port, _reaper) = spawn_serve(&w);

    let (status, v) = get_json(port, "/api/session/claude/alphasess/head");
    assert_eq!(status, 200, "{v}");
    for k in [
        "id",
        "harness",
        "path",
        "cwd",
        "title",
        "created_at",
        "updated_at",
        "message_count",
        "size_bytes",
        "model",
        "git",
        "system_prompt",
        "lineage",
        "total",
    ] {
        assert!(v.as_object().unwrap().contains_key(k), "head must carry {k}: {v}");
    }
    assert_eq!(v["id"], "alphasess", "{v}");
    // `total` is the exact streamed message count — the index space `/messages` windows — and is
    // not the discovery-time `message_count`, which counts records on disk.
    assert!(v["total"].as_u64().is_some(), "{v}");

    let (status, _) = get_json(port, "/api/session/claude/zzz-nope/head");
    assert_eq!(status, 404);
}

/// A deep link is a ROUTE and falls back to the SPA index; a missing ASSET is a 404. Serving
/// `index.html` under an asset's name made the browser reject it with "Expected a
/// JavaScript-or-Wasm module script but the server responded with a MIME type of text/html",
/// which reads like a server misconfiguration rather than a missing file.
#[test]
fn serve_static_missing_asset_is_404_not_the_index() {
    let w = world("static");
    let webroot = write_web_root(&w);
    let (port, _reaper) = spawn_serve_cfg(&w, &["--web", webroot.to_str().unwrap()], &[]);

    let (status, ct, _) = get_with_content_type(port, "/pkg/cv_web.js");
    assert_eq!(status, 404, "a missing module is a 404, not the index");
    assert!(!ct.contains("html"), "and not served as html: {ct}");

    let (status, ct, _) = get_with_content_type(port, "/components/cv-forest.js");
    assert_eq!(status, 200);
    assert!(ct.contains("javascript"), "{ct}");

    // A route (no file extension) still resolves to the SPA index so deep links work.
    let (status, ct, body) = get_with_content_type(port, "/session/claude/alphasess");
    assert_eq!(status, 200);
    assert!(ct.contains("html"), "{ct}");
    assert!(body.contains("<!doctype html>"), "{body}");
}

#[test]
fn serve_compactions() {
    let w = world("compact");
    write_compacted(&w);
    let (port, _reaper) = spawn_serve(&w);

    // The dedicated scan (built on cv_core::compaction::detect) finds the one boundary, pairs it
    // with its summary, and reports the pre-compaction span — all in `/messages` index order.
    let (status, v) = get_json(port, "/api/session/claude/compactsess/compactions");
    assert_eq!(status, 200, "{v}");
    // A bare array with the SAME field names `cv show --kind compaction_boundary --json` emits — this endpoint used to
    // wrap it in `{harness, id, compactions}` and rename the boundary to `index` and the span to
    // `pre_span`, so the same fact had two names depending on the door you asked.
    let arr = v.as_array().expect("compactions array");
    assert_eq!(arr.len(), 1, "{v}");
    let c = &arr[0];
    assert_eq!(c["boundary_msg_idx"], 2, "boundary is the 3rd message (idx 2): {v}");
    assert_eq!(c["trigger"], "manual", "{v}");
    assert_eq!(c["pre_tokens"], 900000, "{v}");
    assert_eq!(c["duration_ms"], 120000, "{v}");
    // The summary that seeded the next window is paired and kept.
    assert_eq!(c["summary_msg_idx"], 3, "summary is the 4th message: {v}");
    assert!(c["summary"].as_str().unwrap().contains("the user asked for X"), "{v}");
    // The pre-compaction span is [0, boundary) — what was compacted away.
    assert_eq!(c["pre_compaction_span"], json!([0, 2]), "{v}");
    assert!(c["headline"].as_str().unwrap().contains("compaction #1"), "{v}");

    // A session with no compaction → empty list, not an error.
    let (status, v) = get_json(port, "/api/session/claude/alphasess/compactions");
    assert_eq!(status, 200, "{v}");
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, _) = get_json(port, "/api/session/claude/zzz-nope/compactions");
    assert_eq!(status, 404);

    // `extra=1` on the windowed read keeps the boundary's subtype + compactMetadata in-band;
    // without it the lean read omits them. Harness facts NEST under the harness name
    // (`extra["claude"][…]`, INTERFACE-V2 §4) — never flat — while the shared concept the
    // boundary IS has moved out of `extra` entirely and onto the message's `kind`.
    let (status, v) = get_json(port, "/api/session/claude/compactsess/messages?extra=1");
    assert_eq!(status, 200, "{v}");
    let m = &v["messages"].as_array().unwrap()[2];
    assert_eq!(m["kind"], "compaction_boundary", "{v}");
    assert_eq!(m["extra"]["claude"]["subtype"], "compact_boundary", "{v}");
    assert_eq!(m["extra"]["claude"]["compactMetadata"]["trigger"], "manual", "{v}");
    assert_eq!(m["extra"]["claude"]["compactMetadata"]["preTokens"], 900000, "{v}");
    assert!(
        m["extra"].get("subtype").is_none() && m["extra"].get("compactMetadata").is_none(),
        "harness facts must not sit flat at the top of extra: {v}"
    );
    // The summary that seeds the next window is its own kind, also independent of `extra`.
    let summary = &v["messages"].as_array().unwrap()[3];
    assert_eq!(summary["kind"], "compaction_summary", "{v}");

    let (status, v) = get_json(port, "/api/session/claude/compactsess/messages");
    assert_eq!(status, 200);
    let m = &v["messages"].as_array().unwrap()[2];
    // Lean read: no `extra` on the wire at all (an empty bag is skipped) — but `kind` is
    // first-class, so the boundary is still identifiable without opting into the harness bag.
    assert_eq!(m.get("extra"), None, "lean read should omit extra: {m}");
    assert_eq!(m["kind"], "compaction_boundary", "{m}");
}

/// `serve --web <dir>` hosts the dashboard from `/` while keeping the JSON API at `/api/*`, with
/// path-traversal confined to the web root and an SPA `index.html` fallback for unknown paths.
#[test]
fn serve_static_web_hub() {
    let w = world("webhub");
    let web = write_web_root(&w);
    let (port, _reaper) = spawn_serve_cfg(&w, &["--web", web.to_str().unwrap()], &[]);

    // `/` serves index.html with an HTML content type.
    let (status, body) = http(port, "GET", "/");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("cv dash"), "index served at /: {body}");

    // A nested asset is served verbatim with a JS content type (check the raw headers).
    let (status, body) = http(port, "GET", "/components/cv-forest.js");
    assert_eq!(status, 200);
    assert!(body.contains("FOREST = 1"), "{body}");
    let raw = raw_get(port, "/components/cv-forest.js");
    assert!(
        raw.to_ascii_lowercase().contains("content-type: text/javascript"),
        "JS content-type missing:\n{raw}"
    );

    // The JSON API still works under the same server, and *does* carry CORS for a local origin.
    let (status, v) = get_json(port, "/api/health");
    assert_eq!(status, 200);
    assert_eq!(v["ok"], true, "{v}");
    let raw = raw_request(port, "GET", "/api/health", &[("Origin", "http://127.0.0.1:9999")]);
    assert!(
        raw.to_ascii_lowercase()
            .contains("access-control-allow-origin: http://127.0.0.1:9999"),
        "API CORS missing:\n{raw}"
    );

    // An unknown (non-asset) path falls back to the SPA index, not a 404.
    let (status, body) = http(port, "GET", "/forest/some/deep/link");
    assert_eq!(status, 200, "SPA fallback: {body}");
    assert!(body.contains("cv dash"), "{body}");

    // Path traversal cannot escape the web root: the sibling secret is unreachable.
    let (status, body) = http(port, "GET", "/../secret.txt");
    assert_eq!(status, 403, "path traversal must be refused: {body}");
    assert!(!body.contains("TOP SECRET"), "traversal leaked the secret:\n{body}");
    // Encoded traversal too.
    for path in [
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
        "/%2e%2e%2fsecret.txt",
        "/x/../../secret.txt",
    ] {
        let (status, body) = http(port, "GET", path);
        assert!(!body.contains("TOP SECRET"), "{path} leaked:\n{body} (status {status})");
    }
    let secret = web.parent().unwrap().join("secret.txt");
    let (status, body) = http(
        port,
        "GET",
        &format!("/{}", secret.display().to_string().replace('/', "%2f")),
    );
    assert!(
        !body.contains("TOP SECRET"),
        "absolute segment leaked:\n{body} (status {status})"
    );
}

/// CORS is an allow-list, not a wildcard: local origins (and the Tauri app's) are echoed back
/// verbatim; any other origin gets **no** `Access-Control-Allow-Origin` at all, so a hostile page
/// the user happens to visit can't read the corpus cross-origin.
#[test]
fn serve_cors_allowlist() {
    let w = world("cors");
    let (port, _reaper) = spawn_serve(&w);

    for origin in [
        "http://localhost:5173",
        "http://127.0.0.1:9999",
        "http://[::1]:9999",
        "tauri://localhost",
        "http://tauri.localhost",
    ] {
        let raw = raw_request(port, "GET", "/api/health", &[("Origin", origin)]);
        assert_eq!(raw_status(&raw), 200, "{raw}");
        assert!(
            raw.to_ascii_lowercase()
                .contains(&format!("access-control-allow-origin: {origin}")),
            "origin {origin} not echoed:\n{raw}"
        );
    }

    // Foreign origins — including lookalikes — get no ACAO header (the browser withholds the body).
    for origin in [
        "https://evil.example",
        "http://localhost.evil.example",
        "http://127.0.0.1.evil.example:7777",
        "http://localhost:garbage",
        "http://localhost:1234/path",
        "http://localhost:99999",
        "http://[::1]evil.example",
        "http://[::1]:1234/path",
    ] {
        let raw = raw_request(port, "GET", "/api/health", &[("Origin", origin)]);
        assert!(
            !raw.to_ascii_lowercase().contains("access-control-allow-origin"),
            "origin {origin} must get no ACAO:\n{raw}"
        );
    }

    // Preflight mirrors the same allow-list.
    let raw = raw_request(port, "OPTIONS", "/api/health", &[("Origin", "http://localhost:5173")]);
    assert_eq!(raw_status(&raw), 204, "{raw}");
    assert!(
        raw.to_ascii_lowercase()
            .contains("access-control-allow-origin: http://localhost:5173"),
        "{raw}"
    );
    let raw = raw_request(port, "OPTIONS", "/api/health", &[("Origin", "https://evil.example")]);
    assert_eq!(raw_status(&raw), 204, "{raw}");
    assert!(
        !raw.to_ascii_lowercase().contains("access-control-allow-origin"),
        "{raw}"
    );
}

// ───────────────────────────── tasks API ─────────────────────────────

/// Seed the fixture task store directly through cv-core (the same store the daemon serves), then
/// exercise every `/api/tasks*` route end-to-end: the shared row shapes (timestamps on this
/// surface, `since` off the wire), state filtering, the 400 on an unknown state string (the
/// silent-empty bug), the inbox rows, and the 410 pointer where the debt/stats routes were.
#[test]
fn serve_tasks_routes() {
    use cv_core::task::{new_event, TaskEventKind, TaskStore};

    let w = world("tasks");
    let store = TaskStore::at(w.cv_home.join("tasks"));
    let opened = store
        .append_agent_event(new_event(
            None,
            "human",
            TaskEventKind::Opened {
                title: "wire the flux capacitor".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("agent:a".into()),
            },
        ))
        .expect("seed open");
    let id = opened.task_id.clone();
    store
        .append_agent_event(new_event(
            Some(&id),
            "agent:a",
            TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        ))
        .expect("seed claim");
    // A second, unassigned task so the state filter has a hit and a miss.
    store
        .append_agent_event(new_event(
            None,
            "human",
            TaskEventKind::Opened {
                title: "review the pelican".into(),
                body: String::new(),
                repo: Some("/tmp/pelican-repo".into()),
                issue: None,
                channel: None,
                assignee: None,
            },
        ))
        .expect("seed second open");

    let (port, _reaper) = spawn_serve(&w);

    // /api/tasks: the shared TaskRow shape — this surface carries timestamps.
    let (status, v) = get_json(port, "/api/tasks");
    assert_eq!(status, 200, "{v}");
    let rows = v["tasks"].as_array().expect("tasks array");
    assert_eq!(rows.len(), 2, "{v}");
    let row = rows.iter().find(|r| r["id"] == json!(id)).expect("claimed row");
    assert_eq!(row["title"], "wire the flux capacitor", "{v}");
    assert_eq!(row["effective_state"], "claimed", "{v}");
    assert_eq!(row["assignee"], "agent:a", "{v}");
    assert!(
        row["opened_at"].is_string() && row["last_ts"].is_string(),
        "HTTP rows carry timestamps: {v}"
    );
    assert!(v["warnings"].as_array().is_some_and(|w| w.is_empty()), "{v}");
    // `detail=1` adds the inbox page's backlog columns; the plain row keeps its shape.
    assert!(row.get("notes").is_none() && row.get("body").is_none(), "{v}");
    let (status, v) = get_json(port, "/api/tasks?detail=1");
    assert_eq!(status, 200, "{v}");
    let row = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap();
    assert_eq!(row["opened_by"], "human", "{v}");
    assert_eq!(
        (
            row["notes"].as_u64(),
            row["decision"].as_bool(),
            row["blocked"].as_bool()
        ),
        (Some(0), Some(false), Some(false)),
        "{v}"
    );
    assert_eq!(row["body"], "", "{v}");

    // State filter hits and misses.
    let (_, v) = get_json(port, "/api/tasks?state=claimed");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/tasks?state=open");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/tasks?state=done");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 0, "{v}");

    // An unknown state string is a 400 naming the typo and the vocabulary — never a silent [].
    let (status, v) = get_json(port, "/api/tasks?state=redy");
    assert_eq!(status, 400, "{v}");
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(msg.contains("redy") && msg.contains("claimed"), "{v}");

    // /api/tasks/inbox/{who}: the claim shows for its owner, as the one inbox row.
    let (status, v) = get_json(port, "/api/tasks/inbox/agent:a");
    assert_eq!(status, 200, "{v}");
    let inbox = v["inbox"].as_array().expect("inbox array");
    assert_eq!(inbox.len(), 1, "{v}");
    assert_eq!(inbox[0]["id"], json!(id), "{v}");
    assert_eq!(inbox[0]["reason"], "claimed_by_you", "{v}");
    assert_eq!(inbox[0]["effective_state"], "claimed", "{v}");
    assert!(inbox[0]["since"].is_string() && inbox[0]["age"].is_string(), "{v}");
    // A stranger sees nothing.
    let (_, v) = get_json(port, "/api/tasks/inbox/agent:nobody");
    assert_eq!(v["inbox"].as_array().unwrap().len(), 0, "{v}");

    // Retired land-facet routes 404 cleanly.
    for route in ["/api/tasks/debt", "/api/tasks/stats"] {
        let (status, v) = get_json(port, route);
        assert_eq!(status, 404, "{route}: {v}");
    }

    // /api/task/{id}: the full projection resolves by unique prefix (uuid v7 ids from the same
    // millisecond share a long time prefix, so take most of it).
    let (status, v) = get_json(port, &format!("/api/task/{}", &id[..30]));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["task"]["task_id"], json!(id), "{v}");
    assert_eq!(v["effective_state"], "claimed", "{v}");
}

/// The Host header must name this machine (DNS-rebinding guard): loopback names in any spelling
/// pass, a rebound domain is a 403 before any route logic runs.
#[test]
fn serve_host_validation() {
    let w = world("host");
    let (port, _reaper) = spawn_serve(&w);

    for host in [
        format!("127.0.0.1:{port}"),
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ] {
        let raw = raw_request(port, "GET", "/api/health", &[("Host", &host)]);
        assert_eq!(raw_status(&raw), 200, "host {host}:\n{raw}");
    }

    for host in ["evil.example", "evil.example:7777", "127.0.0.1.evil.example"] {
        let raw = raw_request(port, "GET", "/api/health", &[("Host", host)]);
        assert_eq!(raw_status(&raw), 403, "host {host} must be rejected:\n{raw}");
        assert!(!raw.contains("\"ok\""), "{raw}");
    }
}

/// Bearer-token auth on `/api/*`: 401 without (or with the wrong) token, 200 with it, and the
/// credential-less OPTIONS preflight stays exempt. Both `--token` and `$CV_SERVE_TOKEN` wire it up.
#[test]
fn serve_token_auth() {
    let w = world("token");
    let (port, _reaper) = spawn_serve_cfg(&w, &["--token", "opensesame"], &[]);

    let raw = raw_request(port, "GET", "/api/sessions", &[]);
    assert_eq!(raw_status(&raw), 401, "{raw}");
    assert!(!raw.contains("alphasess"), "401 must not leak data:\n{raw}");
    let raw = raw_request(port, "GET", "/api/sessions", &[("Authorization", "Bearer wrong")]);
    assert_eq!(raw_status(&raw), 401, "{raw}");

    let raw = raw_request(port, "GET", "/api/sessions", &[("Authorization", "Bearer opensesame")]);
    assert_eq!(raw_status(&raw), 200, "{raw}");
    assert!(raw.contains("alphasess"), "{raw}");

    let raw = raw_request(port, "OPTIONS", "/api/sessions", &[]);
    assert_eq!(raw_status(&raw), 204, "preflight is exempt:\n{raw}");

    // Same gate via the environment variable.
    let (port, _reaper2) = spawn_serve_cfg(&w, &[], &[("CV_SERVE_TOKEN", "hunter2")]);
    let raw = raw_request(port, "GET", "/api/health", &[]);
    assert_eq!(raw_status(&raw), 401, "{raw}");
    let raw = raw_request(port, "GET", "/api/health", &[("Authorization", "Bearer hunter2")]);
    assert_eq!(raw_status(&raw), 200, "{raw}");
}

/// A non-loopback bind without auth is refused outright: the corpus can contain secrets, so
/// exposure demands a token or an explicit `--insecure-expose`.
#[test]
fn serve_refuses_bare_public_bind() {
    let w = world("expose");
    let out = w
        .cv_command()
        .args(["serve", "--host", "0.0.0.0", "--port", "0"])
        .output()
        .expect("cv should run");
    assert!(!out.status.success(), "bare 0.0.0.0 bind must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("refusing"), "{stderr}");
    assert!(stderr.contains("--insecure-expose"), "{stderr}");
}

// ───────────────────────── search & stats ─────────────────────────

/// The value of one response header, lowercased-name match, from a raw response.
fn header_of(raw: &str, name: &str) -> Option<String> {
    raw.lines()
        .take_while(|l| !l.trim().is_empty())
        .find(|l| {
            l.to_ascii_lowercase()
                .starts_with(&format!("{}:", name.to_ascii_lowercase()))
        })
        .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
}

/// The JSON body of a raw response.
fn raw_json(raw: &str) -> Value {
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("non-JSON body {body:?}: {e}"))
}

/// `/api/search` over a world with NO full-text index: every session is unindexed, so the live
/// read answers (all of it unless `live_bytes` bounds it) and the note says so; `scan=1` asks for the live scan
/// instead, which answers in the same row shape and SAYS it is a scan.
#[test]
fn serve_search() {
    let w = world("search");
    let (port, _reaper) = spawn_serve(&w);

    // No index: the live read finds it, ranked, and the note names what was read and the cure.
    let raw = raw_get(port, "/api/search?q=zebrafish");
    assert_eq!(raw_status(&raw), 200, "{raw}");
    let v = raw_json(&raw);
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{v}");
    assert!(v[0]["score"].as_f64().is_some(), "{v}");
    assert_eq!(header_of(&raw, "X-Cv-Search-Source").as_deref(), Some("index"), "{raw}");
    let note = header_of(&raw, "X-Cv-Search-Note").unwrap_or_default();
    assert!(note.contains("no index yet"), "{raw}");
    // A zero budget reads nothing live, and says what it left out.
    let raw = raw_get(port, "/api/search?q=zebrafish&live_bytes=0");
    assert_eq!(raw_json(&raw), serde_json::json!([]), "{raw}");
    let note = header_of(&raw, "X-Cv-Search-Note").unwrap_or_default();
    assert!(note.contains("not indexed") && note.contains("cv index"), "{raw}");

    // A word only `alphasess` says. One row, and the row is the §3 search row.
    let raw = raw_get(port, "/api/search?scan=1&q=zebrafish");
    assert_eq!(raw_status(&raw), 200, "{raw}");
    let v = raw_json(&raw);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1, "{v}");
    assert_eq!(arr[0]["id"], "alphasess", "{v}");
    // The §3 contract is the key SET (a JSON object has no order), not the wire order.
    let mut keys: Vec<&str> = arr[0].as_object().unwrap().keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = SEARCH_ROW_KEYS.to_vec();
    keys.sort_unstable();
    expected.sort_unstable();
    assert_eq!(keys, expected, "search row must match §3 exactly: {v}");
    // The session-row half is real (it came from a ref, not the index) and the search half is
    // filled the way a scoreless scan fills it.
    assert!(arr[0]["path"].as_str().is_some_and(|p| p.ends_with(".jsonl")), "{v}");
    assert!(arr[0]["size_bytes"].as_u64().is_some_and(|n| n > 0), "{v}");
    assert!(arr[0]["score"].is_null(), "a live scan ranks nothing: {v}");
    assert!(
        arr[0]["snippet"].as_str().is_some_and(|s| s.contains("zebrafish")),
        "{v}"
    );
    assert!(arr[0]["agent_id"].is_null() && arr[0]["workflow"].is_null(), "{v}");

    // …and it says which door answered, so the UI can tell the user the result is partial.
    assert_eq!(header_of(&raw, "X-Cv-Search-Source").as_deref(), Some("live"), "{raw}");
    let note = header_of(&raw, "X-Cv-Search-Note").unwrap_or_default();
    assert!(
        note.contains("scanned every transcript live"),
        "the scan must be stated: {raw}"
    );

    // An empty or missing query is a caller mistake, not "every session".
    for path in ["/api/search", "/api/search?q=", "/api/search?limit=5"] {
        let (status, v) = get_json(port, path);
        assert_eq!(status, 400, "{path}: {v}");
        assert!(v["error"].as_str().unwrap().contains("q parameter"), "{v}");
    }

    // Live scan has no context projection; never silently search the exchange instead.
    for kind in ["context", "all"] {
        let (status, v) = get_json(port, &format!("/api/search?scan=1&kind={kind}&q=zebrafish"));
        assert_eq!(status, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("exchange only"), "{v}");
    }

    // harness: a filter, and a typo is a clear 400 (same as /api/sessions).
    let (status, v) = get_json(port, "/api/search?scan=1&q=zebrafish&harness=claude");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, v) = get_json(port, "/api/search?scan=1&q=zebrafish&harness=codex");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/search?scan=1&q=zebrafish&harness=warpdrive");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("unknown harness"), "{v}");

    // cwd: a substring of the row's cwd, matching /api/sessions?cwd=.
    let (_, v) = get_json(port, "/api/search?scan=1&q=zebrafish&cwd=%2Fwork%2Fproj");
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/search?scan=1&q=zebrafish&cwd=nowhere");
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // A needle both fixtures contain ("plea(se)" / "(se)venteen"), so `limit` bites — and the
    // truncation is stated rather than silently looking like the whole answer.
    let (_, v) = get_json(port, "/api/search?scan=1&q=se");
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let raw = raw_get(port, "/api/search?scan=1&q=se&limit=1");
    assert_eq!(raw_json(&raw).as_array().unwrap().len(), 1);
    assert!(
        header_of(&raw, "X-Cv-Search-Note")
            .unwrap_or_default()
            .contains("1 of 2 matches"),
        "{raw}"
    );
    assert_eq!(header_of(&raw, "X-Cv-Search-Total").as_deref(), Some("2"), "{raw}");

    // A miss is an empty array, never an error.
    let (status, v) = get_json(port, "/api/search?scan=1&q=pangolin");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // The degrade signal is readable cross-origin — a header a dev-server dashboard can't read is
    // not a channel.
    let raw = raw_request(
        port,
        "GET",
        "/api/search?scan=1&q=zebrafish",
        &[("Origin", "http://localhost:5173")],
    );
    let exposed = header_of(&raw, "Access-Control-Expose-Headers").unwrap_or_default();
    assert!(
        exposed.contains("X-Cv-Search-Source") && exposed.contains("X-Cv-Search-Note"),
        "{raw}"
    );
}

/// `/api/stats` — the corpus-wide numbers `cv stats --json` computes, over the whole archive
/// rather than over whatever the dashboard happened to have open.
#[test]
fn serve_stats() {
    let w = world("stats");
    write_gamma(&w);
    let (port, _reaper) = spawn_serve(&w);

    let (status, v) = get_json(port, "/api/stats");
    assert_eq!(status, 200, "{v}");
    // The §3 contract is the key SET (a JSON object has no order), not the wire order.
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "by_harness",
            "earliest_created",
            "latest_updated",
            "messages",
            "sessions",
            "top_cwds",
            "uncounted"
        ],
        "stats payload must match `cv stats --json` exactly: {v}"
    );
    assert_eq!(v["sessions"], 3, "{v}");
    assert_eq!(v["by_harness"]["claude"], 3, "{v}");

    // The totals are the fleet's, not one session's: cross-check against /api/sessions' own rows.
    let (_, sessions) = get_json(port, "/api/sessions");
    let rows = sessions.as_array().unwrap();
    let total: u64 = rows.iter().map(|r| r["message_count"].as_u64().unwrap_or(0)).sum();
    assert_eq!(v["messages"].as_u64(), Some(total), "{v}");

    // top_cwds: home-relative cwd + a session count, ranked.
    let top = v["top_cwds"].as_array().unwrap();
    assert_eq!(top[0]["cwd"], "/work/proj", "{v}");
    assert_eq!(top[0]["sessions"], 3, "{v}");
    // RFC 3339 spans, oldest creation to newest activity.
    assert!(
        v["earliest_created"]
            .as_str()
            .is_some_and(|s| s.starts_with("2026-01-01")),
        "{v}"
    );
    assert!(
        v["latest_updated"]
            .as_str()
            .is_some_and(|s| s.starts_with("2026-04-01")),
        "{v}"
    );

    // `q` is cv's query calculus, exactly as `cv stats -q` takes it.
    let (_, v) = get_json(port, "/api/stats?q=harness%3Aclaude");
    assert_eq!(v["sessions"], 3, "{v}");
    let (_, v) = get_json(port, "/api/stats?q=harness%3Acodex");
    assert_eq!(v["sessions"], 0, "{v}");
    assert_eq!(v["messages"], 0, "{v}");
    assert!(v["by_harness"].as_object().unwrap().is_empty(), "{v}");
    assert!(
        v["earliest_created"].is_null(),
        "an empty match is nulls, not an error: {v}"
    );
    // A term that needs the parsed transcript, not just the catalog row.
    let (_, v) = get_json(port, "/api/stats?q=tool%3Aedit");
    assert_eq!(v["sessions"], 1, "only gammasess edits a file: {v}");

    // A malformed query is a 400 quoting the core's own message, pointed at `cv schema`.
    let (status, v) = get_json(port, "/api/stats?q=msgs%3E");
    assert_eq!(status, 400, "{v}");
    let msg = v["error"].as_str().unwrap();
    assert!(msg.contains("msgs") && msg.contains("cv schema"), "{v}");
}

#[test]
fn live_search_filters_cwd_before_spending_the_result_budget() {
    let w = world("live-filter-budget");
    for id in ["alphasess", "betasess"] {
        w.write_session(id, &[json!({"type":"user", "uuid":id, "cwd":format!("/work/{id}"), "message":{"role":"user", "content":"sharedneedle"}})]);
    }
    let (port, _server) = spawn_serve(&w);
    let (_, first) = get_json(port, "/api/search?scan=1&q=sharedneedle&limit=1");
    let target = if first[0]["id"] == "alphasess" {
        "betasess"
    } else {
        "alphasess"
    };
    let (status, filtered) = get_json(
        port,
        &format!("/api/search?scan=1&q=sharedneedle&limit=1&cwd=/work/{target}"),
    );
    assert_eq!(status, 200, "{filtered}");
    assert_eq!(
        filtered.as_array().unwrap().len(),
        1,
        "nonmatching hit exhausted live limit: {filtered}"
    );
    assert_eq!(filtered[0]["id"], target);
    assert_eq!(get_json(port, "/api/search?scan=1&q=sharedneedle&limit=0").1, json!([]));
}

#[test]
fn malformed_numeric_read_selectors_error_before_expanding_the_read() {
    let w = world("numeric-read-selectors");
    let (port, _server) = spawn_serve(&w);
    for path in [
        "/api/sessions?limit=bad",
        "/api/sessions?limit=",
        "/api/search?q=needle&limit=-1",
        "/api/search?q=needle&limit=",
        "/api/session/claude/alphasess/messages?end=",
        "/api/session/claude/missing/messages?start=bad",
    ] {
        let (status, error) = get_json(port, path);
        assert_eq!(status, 400, "{path}: {error}");
    }
    assert_eq!(get_json(port, "/api/sessions?limit=0").1, json!([]));
    assert_eq!(
        get_json(port, "/api/session/claude/alphasess/messages?start=0&end=0").1["messages"],
        json!([])
    );
    w.cv_ok(&["index"]);
    let (status, error) = get_json(port, "/api/search?q=%22unterminated");
    assert_eq!(status, 400, "invalid FTS query was silently retried live: {error}");
    assert_eq!(get_json(port, "/api/search?q=unmatchableword").1, json!([]));
}

#[test]
fn request_headers_are_bounded_and_buffering_preserves_body_and_pipeline() {
    let w = world("header-bounds");
    let (port, _server) = spawn_serve(&w);
    for fields in [
        format!("X-Large: {}\r\n", "x".repeat(9000)),
        "X-Small: x\r\n".repeat(129),
        format!("X-Block: {}\r\n", "x".repeat(7000)).repeat(10),
    ] {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            stream,
            "GET /api/sessions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{fields}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        assert_eq!(raw_status(&raw), 431, "oversized headers accepted: {raw}");
    }
    // A single buffered packet contains headers, body and the following request. The body
    // bytes must remain in the same sequential reader, rather than disappear in header scratch.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4\r\n\r\nbodyGET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    // Count full status LINES (`HTTP/1.1 200 OK\r\n`), so a body that happens to quote a bare
    // "HTTP/1.1 200" is never counted as a response.
    assert_eq!(
        raw.matches("HTTP/1.1 200 OK\r\n").count(),
        2,
        "buffered body/pipeline lost: {raw}"
    );
}

#[test]
fn request_body_framing_rejects_ambiguous_or_unsupported_headers() {
    let w = world("request-framing");
    let (port, _server) = spawn_serve(&w);
    for headers in [
        vec![("Content-Length", "garbage")],
        vec![("Content-Length", "+0")],
        vec![("Content-Length", "0"), ("Content-Length", "1")],
        vec![("Content-Length", "0"), ("Transfer-Encoding", "chunked")],
        vec![("Transfer-Encoding", "gzip")],
        vec![("Transfer-Encoding", "gzip, chunked")],
        vec![("Transfer-Encoding", "chunked"), ("Transfer-Encoding", "chunked")],
    ] {
        let raw = raw_request(port, "GET", "/api/sessions", &headers);
        assert_eq!(raw_status(&raw), 400, "ambiguous framing accepted {headers:?}: {raw}");
    }
    for headers in [
        vec![("Content-Length", "0")],
        vec![("Content-Length", "0"), ("Content-Length", "0")],
    ] {
        assert_eq!(raw_status(&raw_request(port, "GET", "/api/sessions", &headers)), 200);
    }
    // Valid chunked framing remains accepted and decoded through the ordinary request path.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET /api/sessions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nTransfer-Encoding: Chunked\r\nConnection: close\r\n\r\n0\r\n\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert_eq!(raw_status(&raw), 200, "{raw}");
}

/// Every pipelined request passes the Host check on its own: an allowed first request does not
/// vouch for the next one on the same connection.
#[test]
fn pipelined_requests_each_pass_the_host_check() {
    let w = world("pipelined-host");
    let (port, _server) = spawn_serve(&w);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\nGET /api/health HTTP/1.1\r\nHost: evil.example\r\n\r\nGET /api/health HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let statuses: Vec<&str> = raw
        .match_indices("HTTP/1.1 ")
        .map(|(at, _)| &raw[at + 9..at + 12])
        .collect();
    assert_eq!(statuses, ["200", "403", "403"], "{raw}");
}

/// The server's connection cap (`http::Limits::connections`' default): filling it with idle
/// connections must shed the next newcomer, not queue it.
const CONNECTION_CAP: usize = 64;

/// Idle connections cannot lock the server out: past the connection cap a newcomer is answered
/// 503 at once, never queued behind them, and each idle one is closed at the first-byte deadline.
#[test]
fn idle_connections_are_shed_then_expire() {
    let w = world("idle-shed");
    let (port, _server) = spawn_serve(&w);
    std::thread::sleep(Duration::from_millis(200)); // let the readiness probe's connection end
    let idle: Vec<TcpStream> = (0..CONNECTION_CAP)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let raw = raw_get(port, "/api/health");
    assert_eq!(raw_status(&raw), 503, "{raw}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shedding waited {:?}",
        started.elapsed()
    );
    for mut stream in idle {
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut rest = Vec::new();
        assert!(
            matches!(stream.read_to_end(&mut rest), Ok(0)),
            "an idle connection got {rest:?}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "idle connections held {:?}",
        started.elapsed()
    );
    assert_eq!(get_json(port, "/api/health").0, 200);
}

/// Accept errors are survived: with the descriptor limit far below the connections offered,
/// accept fails with EMFILE, and the loop backs off and keeps serving once descriptors free up.
#[cfg(unix)]
#[test]
fn accept_errors_back_off_and_the_server_keeps_serving() {
    let w = world("emfile");
    let template = w.cv_command();
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("ulimit -n 48 && exec \"$0\" serve --port 0")
        .arg(template.get_program())
        .env_clear()
        .envs(template.get_envs().filter_map(|(k, v)| Some((k, v?))));
    if let Some(dir) = template.get_current_dir() {
        command.current_dir(dir);
    }
    let (port, mut server) = spawn_serve_command(command);
    let flood: Vec<TcpStream> = (0..80)
        .filter_map(|_| TcpStream::connect(("127.0.0.1", port)).ok())
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    drop(flood);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(server.0.try_wait().unwrap().is_none(), "cv serve exited under EMFILE");
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut raw = String::new();
        let sent = write!(
            stream,
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        if sent.is_ok() && stream.read_to_string(&mut raw).is_ok() && raw.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(Instant::now() < deadline, "the accept loop never recovered: {raw:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A client that vanishes mid-request or mid-response costs its own connection only.
#[test]
fn abandoned_requests_and_responses_leave_the_server_serving() {
    let w = world("abandoned");
    let rows: Vec<_> = (0..2000)
        .map(|n| json!({"type":"user", "uuid":format!("u{n}"), "message":{"role":"user","content":"x".repeat(500)}}))
        .collect();
    w.write_session("big", &rows);
    let (port, _server) = spawn_serve(&w);
    for request in [
        "GET /api/sess".to_string(),
        "POST /api/task/x/note HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{".to_string(),
        "GET /api/session/claude/big/export?format=markdown HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_string(),
    ] {
        for _ in 0..8 {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
        }
    }
    assert_eq!(get_json(port, "/api/health").0, 200);
    assert_eq!(get_json(port, "/api/session/claude/big/messages?start=0&end=1").0, 200);
}

/// Raw GET returning the full response text (headers + body) — for content-type / CORS assertions.
fn raw_get(port: u16, path: &str) -> String {
    raw_request(port, "GET", path, &[])
}

/// A raw request with custom headers, returning the full response text (headers + body).
/// A default local `Host` is supplied unless the caller passes their own.
fn raw_request(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    cv_testkit::http::raw_request(&format!("127.0.0.1:{port}"), method, path, headers, None)
}

// ───────────────────────────── the inbox on cv serve ─────────────────────────────

/// A POST with a body and headers, returning the raw response.
fn raw_post(port: u16, path: &str, body: &str, headers: &[(&str, &str)]) -> String {
    cv_testkit::http::raw_request(&format!("127.0.0.1:{port}"), "POST", path, headers, Some(body))
}

/// What `cv task serve` served moved onto `cv serve`: the page (at `/inbox`, and at `/` with no
/// `--web`), its reads, and its writes — which record the same event the CLI would, as
/// `web:<who>`, and only as JSON from a local origin (a cross-site form or `text/plain` fetch
/// needs no preflight, so it must be refused, not merely denied the response).
#[test]
fn serve_carries_the_inbox_and_guards_its_writes() {
    use cv_core::task::{new_event, TaskEventKind, TaskStore};
    let w = world("inbox");
    let store = TaskStore::at(w.cv_home.join("tasks"));
    let opened = store
        .append_agent_event(new_event(
            None,
            "human",
            TaskEventKind::Opened {
                title: "tune the theremin".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("ember".into()),
            },
        ))
        .expect("seed open");
    let id = opened.task_id.clone();
    let (port, _reaper) = spawn_serve_cfg(&w, &["--who", "ember"], &[]);

    for path in ["/", "/inbox"] {
        let (status, ct, body) = get_with_content_type(port, path);
        assert_eq!(status, 200, "{path}");
        assert!(
            ct.contains("html") && body.contains("<title>cv inbox</title>"),
            "{path}: {ct}"
        );
    }
    let (status, v) = get_json(port, "/api/inbox");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["who"], "ember", "the server's --who is the default inbox: {v}");
    let (status, v) = get_json(port, "/api/backlog");
    assert_eq!(status, 404, "{v}");

    let path = format!("/api/task/{id}/note");
    let body = r#"{"who":"ember","text":"a little flat"}"#;
    // Not JSON → refused before it touches the store.
    let raw = raw_post(port, &path, body, &[("Content-Type", "text/plain")]);
    assert_eq!(raw_status(&raw), 403, "{raw}");
    // JSON from a foreign origin → refused.
    let raw = raw_post(
        port,
        &path,
        body,
        &[("Content-Type", "application/json"), ("Origin", "https://evil.example")],
    );
    assert_eq!(raw_status(&raw), 403, "{raw}");
    let raw = raw_post(port, &path, body, &[("Content-Type", "application/json-invalid")]);
    assert_eq!(raw_status(&raw), 403, "lookalike MIME type was accepted: {raw}");
    // A valid JSON prefix cannot hide an unfinished declared body.
    let mut short = TcpStream::connect(("127.0.0.1", port)).unwrap();
    short.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(short, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{body}").unwrap();
    short.shutdown(std::net::Shutdown::Write).unwrap();
    let mut raw = String::new();
    short.read_to_string(&mut raw).unwrap();
    assert_eq!(raw_status(&raw), 400, "unfinished body was accepted: {raw}");
    assert_eq!(raw_json(&raw)["error"], "unreadable body");
    let mut oversized = body.to_string();
    oversized.push_str(&" ".repeat((1 << 20) + 1 - oversized.len()));
    let raw = raw_post(port, &path, &oversized, &[("Content-Type", "application/json")]);
    assert_eq!(raw_status(&raw), 413, "truncated JSON prefix was accepted: {raw}");
    let (_, _, events) = get_with_content_type(port, "/api/events");
    assert_eq!(
        events.lines().count(),
        1,
        "rejected writes changed the task store: {events}"
    );
    // The exact boundary and a legal MIME parameter remain accepted.
    let mut boundary = body.to_string();
    boundary.push_str(&" ".repeat((1 << 20) - boundary.len()));
    let raw = raw_post(
        port,
        &path,
        &boundary,
        &[("Content-Type", "Application/JSON; charset=utf-8")],
    );
    assert_eq!(raw_status(&raw), 200, "legal boundary body was rejected: {raw}");
    // JSON from the page's own origin → recorded as web:<who>.
    let raw = raw_post(
        port,
        &path,
        body,
        &[
            ("Content-Type", "application/json"),
            ("Origin", &format!("http://127.0.0.1:{port}")),
        ],
    );
    assert_eq!(raw_status(&raw), 200, "{raw}");
    let v = raw_json(&raw);
    assert_eq!(v["by"], "web:ember", "{v}");

    // The events feed is the same query as `cv task events`: JSON lines, the note last.
    let (status, ct, lines) = get_with_content_type(port, "/api/events?since=1h");
    assert_eq!(status, 200);
    assert!(ct.contains("ndjson"), "{ct}");
    let last: Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
    assert_eq!(last["by"], "web:ember", "{lines}");

    // POST anywhere else is still a 405.
    let raw = raw_post(port, "/api/sessions", "{}", &[("Content-Type", "application/json")]);
    assert_eq!(raw_status(&raw), 405, "{raw}");
}

/// A GET or POST with a JSON content type; returns (status, body).
fn json_http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let raw = cv_testkit::http::raw_request(
        &format!("127.0.0.1:{port}"),
        method,
        path,
        &[("Content-Type", "application/json")],
        Some(body.unwrap_or("")),
    );
    (raw_status(&raw), cv_testkit::http::parts(&raw).1.to_owned())
}

/// The page and the CLI are one store: GET the inbox for a fixture assignee (one decision, one
/// action), POST resolve / done / note / discuss, and `cv task show --brief` reflects every event
/// with the author `web:<assignee>`. The event feed route is the same query as `cv task events`.
#[test]
fn the_inbox_records_the_same_events_as_the_cli() {
    let w = World::new("serve-cli-agreement");
    w.cv_ok(&[
        "task",
        "decide",
        "PAY §7: tariff",
        "--for",
        "ember",
        "--default",
        "1 credit/unit",
        "--option",
        "free tier",
        "--from",
        "orchestrator:x",
    ]);
    let titled = |title: &str| -> String {
        let (json, _) = w.cv_ok(&["task", "list", "--all", "--json"]);
        let tasks: Vec<Value> = serde_json::from_str(&json).unwrap();
        let ids: Vec<&str> = tasks
            .iter()
            .filter(|t| t["title"] == title)
            .map(|t| t["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids.len(), 1, "one task titled {title:?}:\n{json}");
        ids[0].to_string()
    };
    let d = titled("PAY §7: tariff");
    w.cv_ok(&[
        "task",
        "open",
        "push dregg-infra",
        "--assignee",
        "ember",
        "--from",
        "orchestrator:x",
    ]);
    let a = titled("push dregg-infra");
    let (port, _reaper) = spawn_serve_cfg(&w, &["--who", "ember"], &[]);
    // The page itself is served inline; it names its own origin story.
    let (status, html) = json_http(port, "GET", "/", None);
    assert_eq!(status, 200);
    assert!(
        html.contains("<title>cv inbox</title>") && html.contains("/api/inbox"),
        "{}",
        &html[..200.min(html.len())]
    );
    assert!(
        !html.contains("<script src=") && !html.contains("<link rel=\"stylesheet\""),
        "no external resources"
    );
    // A DNS-named Host is refused (rebinding).
    let raw = cv_testkit::http::raw_request(
        &format!("127.0.0.1:{port}"),
        "GET",
        "/api/inbox",
        &[("Host", "evil.example")],
        None,
    );
    assert_eq!(raw_status(&raw), 403);

    let (status, body) = json_http(port, "GET", "/api/inbox?who=ember", None);
    assert_eq!(status, 200, "{body}");
    let page: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(page["who"], "ember");
    assert_eq!(page["counts"]["decisions"], 1);
    assert_eq!(page["counts"]["assigned"], 1);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items[0]["reason"], "decision_owed");
    assert_eq!(items[0]["decision"]["default"], "1 credit/unit");
    assert_eq!(items[0]["decision"]["alternatives"], json!(["free tier"]));
    assert_eq!(items[1]["reason"], "assigned_open");
    // Without ?who the server's --who applies.
    let (status, body) = json_http(port, "GET", "/api/inbox", None);
    assert_eq!(status, 200);
    assert!(body.contains("\"who\":\"ember\""), "{body}");

    // Act: a note, then "needs discussion", then resolve with an option, then done.
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{d}/note"),
        Some(r#"{"text":"reading PAY first"}"#),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"by\":\"web:ember\""), "{body}");
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{d}/discuss"),
        Some(r#"{"text":"what is a unit?"}"#),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{d}/resolve"),
        Some(r#"{"choice":"free tier","note":"for October"}"#),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"task_state\":\"resolved\""), "{body}");
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{d}/resolve"),
        Some(r#"{"accept_default":true}"#),
    );
    assert_eq!(status, 400, "a second resolution is refused: {body}");
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{a}/done"),
        Some(r#"{"observed":"pushed"}"#),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = json_http(port, "POST", &format!("/api/task/{a}/done"), Some("{}"));
    assert_eq!(status, 400, "{body}");
    let (status, body) = json_http(port, "POST", &format!("/api/task/{a}/frobnicate"), Some("{}"));
    assert_eq!(status, 400, "{body}");
    let (status, body) = json_http(port, "POST", &format!("/api/task/{a}/note"), Some("not json"));
    assert_eq!(status, 400, "{body}");

    // The closed items show under the page's Resolved filter; unknown verbs (including reopen) return 400.
    let (status, body) = json_http(port, "GET", "/api/inbox?who=ember", None);
    assert_eq!(status, 200);
    let page: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 0);
    assert_eq!(page["closed"].as_array().unwrap().len(), 2);
    let (status, body) = json_http(port, "POST", &format!("/api/task/{a}/reopen"), Some("{}"));
    assert_eq!(status, 400, "{body}");

    // One task, with its history; the event feed is the same query as `cv task events`.
    let (status, body) = json_http(port, "GET", &format!("/api/task/{d}?events=1"), None);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["effective_state"], "resolved");
    assert!(v["events"].as_array().unwrap().len() >= 6, "{body}");
    let (status, body) = json_http(port, "GET", "/api/events?since=1h&kind=resolved,done", None);
    assert_eq!(status, 200);
    let rows: Vec<Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "{body}");
    assert!(rows.iter().all(|r| r["by"] == "web:ember"), "{body}");

    // The CLI sees exactly what the page recorded, with the web author.
    let (out, _) = w.cv_ok(&["task", "show", &d, "--brief"]);
    assert!(out.contains("[resolved]"), "{out}");
    assert!(
        out.contains("resolved: free tier — by web:ember,") && out.contains("· for October"),
        "{out}"
    );
    assert!(out.contains("web:ember") && out.contains("reading PAY first"), "{out}");
    assert!(out.contains("NEEDS DISCUSSION: what is a unit?"), "{out}");
    assert!(
        out.contains("tags:     discuss"),
        "a decision is posed, never tagged:\n{out}"
    );
    let (out, _) = w.cv_ok(&["task", "show", &a, "--brief"]);
    assert!(
        out.contains("[done]") && out.contains("self-reported") && out.contains("(pushed)"),
        "{out}"
    );
    let (json, _) = w.cv_ok(&["task", "show", &a, "--events"]);
    assert!(json.contains("\"by\": \"web:ember\""), "{json}");
    let (out, _) = w.cv_ok(&["task", "events", "--since", "1h", "--kind", "resolved,done"]);
    assert_eq!(out.lines().count(), 2, "the CLI feed and /api/events agree:\n{out}");
}

/// The provisional decision lifecycle on the CLI: a provisional resolution reads as "made for
/// you (veto?)", the poser cannot pick a non-default, a veto supersedes (keeping the provisional
/// one), and confirming through the page records `web:<who>` with the provisional kept.
#[test]
fn task_provisional_resolution_veto_and_confirm() {
    let w = World::new("provisional");
    let (out, _) = w.cv_ok(&[
        "task",
        "decide",
        "build base: /tank or NVMe",
        "--for",
        "ember",
        "--default",
        "keep /tank",
        "--option",
        "move to NVMe",
        "--provisional",
        "--from",
        "orchestrator:o",
    ]);
    let titled = |title: &str| -> String {
        let (json, _) = w.cv_ok(&["task", "list", "--all", "--json"]);
        let tasks: Vec<Value> = serde_json::from_str(&json).unwrap();
        let ids: Vec<&str> = tasks
            .iter()
            .filter(|t| t["title"] == title)
            .map(|t| t["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids.len(), 1, "one task titled {title:?}:\n{json}");
        ids[0].to_string()
    };
    let d = titled("build base: /tank or NVMe");
    assert!(
        out.contains("resolved") && out.contains("provisionally on the default"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    assert!(
        out.starts_with("made for you (veto?) (1):"),
        "not owed, its own group:\n{out}"
    );
    assert!(
        out.contains("⇒ made: keep /tank — by orchestrator:o · veto to: move to NVMe"),
        "{out}"
    );
    assert!(out.contains("cv task resolve <id> --confirm --from ember"), "{out}");
    let (json, _) = w.cv_ok(&["task", "inbox", "ember", "--json"]);
    let inbox: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(inbox["counts"]["provisional"], 1, "{json}");
    let item = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == d.as_str());
    assert_eq!(item.map(|i| &i["reason"]), Some(&json!("provisional")), "{json}");
    let (out, _) = w.cv_ok(&["task", "show", &d]);
    assert!(out.contains("[resolved]") && out.contains("PROVISIONAL"), "{out}");
    assert!(out.contains("veto?:    cv task resolve"), "{out}");

    // The poser cannot pick a non-default provisionally; a third party cannot override.
    w.cv_ok(&[
        "task",
        "decide",
        "Q2",
        "--for",
        "ember",
        "--default",
        "a",
        "--option",
        "b",
        "--from",
        "orchestrator:o",
    ]);
    let d2 = titled("Q2");
    let (_, err) = w.cv_fails(&[
        "task",
        "resolve",
        &d2,
        "--choice",
        "b",
        "--provisional",
        "--from",
        "orchestrator:o",
    ]);
    assert!(err.contains("stands on the default"), "{err}");
    let (_, err) = w.cv_fails(&[
        "task",
        "resolve",
        &d,
        "--choice",
        "move to NVMe",
        "--from",
        "lane:someone",
    ]);
    assert!(err.contains("only the decision's assignee (ember)"), "{err}");
    let (_, err) = w.cv_fails(&["task", "resolve", &d2, "--confirm", "--from", "ember"]);
    assert!(err.contains("no provisional resolution to confirm"), "{err}");
    // No identity: the cure names the decision's owner, never a guess.
    let (_, err) = w.cv_fails(&["task", "resolve", &d2, "--accept-default"]);
    assert!(
        err.contains("records WHO decided") && err.contains("--accept-default --from ember"),
        "{err}"
    );

    // The decider vetoes: a new resolution replaces the provisional one, which is kept.
    let (out, _) = w.cv_ok(&["task", "resolve", &d, "--choice", "move to NVMe", "--from", "ember"]);
    assert!(out.contains("→ move to NVMe (by ember)"), "{out}");
    let (out, _) = w.cv_ok(&["task", "show", &d]);
    assert!(out.contains("provisionally: keep /tank — by orchestrator:o"), "{out}");
    assert!(
        out.contains("resolved: move to NVMe — by ember") && !out.contains("PROVISIONAL"),
        "{out}"
    );
    let (json, _) = w.cv_ok(&["task", "show", &d, "--json"]);
    let t: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["decision"]["resolution"]["choice"], "move to NVMe");
    assert!(t["decision"]["resolution"].get("provisional").is_none(), "{json}");
    assert_eq!(t["decision"]["superseded_provisional"]["provisional"], true, "{json}");
    // Vetoed, it is no longer made for ember: not an open item, counted as closed.
    let (json, _) = w.cv_ok(&["task", "inbox", "ember", "--json"]);
    let inbox: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(inbox["counts"]["provisional"], 0, "{json}");
    assert!(
        inbox["items"].as_array().unwrap().iter().all(|i| i["id"] != d.as_str()),
        "{json}"
    );
    assert!(
        inbox["closed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == d.as_str()),
        "{json}"
    );
    // A second answer is refused as it always was.
    let (_, err) = w.cv_fails(&["task", "resolve", &d, "--accept-default", "--from", "ember"]);
    assert!(err.contains("cannot apply resolved"), "{err}");

    // Confirm through the page.
    w.cv_ok(&[
        "task",
        "decide",
        "Q3",
        "--for",
        "ember",
        "--default",
        "x",
        "--option",
        "y",
        "--provisional",
        "--from",
        "orchestrator:o",
    ]);
    let d3 = titled("Q3");
    let (port, _reaper) = spawn_serve_cfg(&w, &["--who", "ember"], &[]);
    let (status, body) = json_http(port, "GET", "/api/inbox?who=ember", None);
    assert_eq!(status, 200, "{body}");
    let page: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(page["counts"]["provisional"], 1, "{body}");
    assert_eq!(page["counts"]["decisions"], 1, "Q2 is still owed; Q3 is not: {body}");
    let made: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["reason"] == "provisional")
        .collect();
    assert_eq!(made.len(), 1, "{body}");
    assert_eq!(made[0]["title"], "Q3");
    let (status, body) = json_http(
        port,
        "POST",
        &format!("/api/task/{d3}/resolve"),
        Some(r#"{"confirm":true}"#),
    );
    assert_eq!(status, 200, "{body}");
    let (json, _) = w.cv_ok(&["task", "show", &d3, "--json"]);
    let t: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["decision"]["resolution"]["by"], "web:ember");
    assert_eq!(t["decision"]["resolution"]["choice"], "x");
    assert_eq!(t["decision"]["superseded_provisional"]["by"], "orchestrator:o");
}

#[test]
fn lanes_task_enrichment_reports_degraded_replay_and_skips_empty_forests() {
    let w = world("lanes-task-warnings");
    w.write_agent(
        "betasess",
        "warning-lane",
        &json!({"agentType": "general-purpose", "description": "useful lane"}),
        &[
            json!({"type": "user", "uuid": "lane-u", "timestamp": "2026-01-01T10:00:00Z",
            "message": {"role": "user", "content": "work"}}),
        ],
    );
    let log = w.cv_home.join("tasks/events.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, "{malformed task event}\n").unwrap();
    let (port, _reaper) = spawn_serve(&w);
    let (status, body) = get_json(port, "/api/lanes?session=betasess");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["counts"]["total"], 1, "{body}");
    assert_eq!(body["lanes"][0]["description"], "useful lane", "{body}");
    assert!(!body["warnings"].as_array().unwrap().is_empty(), "{body}");
    fs::remove_file(&log).unwrap();
    fs::create_dir(&log).unwrap(); // task replay now fails, rather than warning and continuing
    let (status, body) = get_json(port, "/api/lanes?session=betasess");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["counts"]["total"], 1, "{body}");
    assert!(
        body["warnings"][0]
            .as_str()
            .unwrap()
            .contains("task enrichment unavailable"),
        "{body}"
    );
    let (status, body) = get_json(port, "/api/lanes?session=alphasess");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["counts"]["total"], 0, "{body}");
    assert!(
        body["warnings"].as_array().unwrap().is_empty(),
        "empty forest does not consult broken task log: {body}"
    );
    w.cv_ok(&["lanes", "alphasess", "--tasks", "--json"]);
    // All of the fixture's lanes are not done, so filtering done leaves no task join to read.
    w.cv_ok(&["lanes", "betasess", "--done", "--tasks", "--json"]);
    let (summary, _) = w.cv_ok(&["lanes", "betasess", "--done", "--tasks"]);
    assert!(
        summary.contains("1 sub-agents"),
        "forest total preserved despite empty status selection: {summary}"
    );
    assert!(summary.contains("showing 0 done"), "filter scope preserved: {summary}");
    assert!(
        summary.contains("(none match)"),
        "filtered-empty message preserved: {summary}"
    );
}

#[test]
fn malformed_header_fields_fail_before_routing() {
    let w = world("header-grammar");
    let (port, _server) = spawn_serve(&w);
    for (name, value) in [
        ("", "x"),
        ("X(Bad)", "x"),
        ("X-Bad", "bad\0value"),
        ("X-Bad", "bad\x7fvalue"),
        ("X-Bad", "bad\x0bvalue"),
    ] {
        let response = raw_request(port, "GET", "/api/health", &[(name, value)]);
        assert_eq!(raw_status(&response), 400, "{name:?}={value:?}: {response}");
    }
    let response = raw_request(port, "GET", "/api/health", &[("X-Valid_Token", "\tvalid\tvalue\t")]);
    assert_eq!(raw_status(&response), 200, "{response}");
}

#[test]
fn duplicate_host_fields_are_rejected_before_routing() {
    let w = world("duplicate-host");
    let (port, _server) = spawn_serve(&w);
    for hosts in [
        [("Host", "localhost"), ("host", "evil.example")],
        [("Host", "localhost"), ("Host", "localhost")],
    ] {
        let response = raw_request(port, "GET", "/api/health", &hosts);
        assert_eq!(raw_status(&response), 400, "{response}");
    }
}

#[test]
fn malformed_local_host_authorities_are_rejected() {
    let w = world("host-authority");
    let (port, _server) = spawn_serve(&w);
    for authority in [
        "localhost:garbage",
        "localhost:99999",
        "[::1]evil.example",
        "[::1]:1/path",
        "localhost:",
    ] {
        let response = raw_request(port, "GET", "/api/health", &[("Host", authority)]);
        assert_eq!(raw_status(&response), 403, "{authority}: {response}");
    }
    for authority in ["localhost", "localhost:1234", "[::1]", "[::1]:1234"] {
        let response = raw_request(port, "GET", "/api/health", &[("Host", authority)]);
        assert_eq!(raw_status(&response), 200, "{authority}: {response}");
    }
}
