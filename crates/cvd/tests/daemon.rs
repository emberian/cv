//! Integration tests for the cvd daemon binary: `sync` idempotency against a fixture corpus and
//! the `serve` HTTP endpoints (status codes, JSON shapes, query parsing) — all hermetic via a
//! temp `$HOME` + `$CLUSTERVISION_HOME` passed only to child processes.

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cvd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        fs::create_dir_all(home.join(".claude/projects/-work-proj")).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        let w = World { base, home, cv_home };
        w.write_fixtures();
        w
    }

    fn session_path(&self, name: &str) -> PathBuf {
        self.home
            .join(".claude/projects/-work-proj")
            .join(format!("{name}.jsonl"))
    }

    fn write_fixtures(&self) {
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "quokka census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen"}]}}),
        ];
        for (name, lines) in [("alphasess", &alpha[..]), ("betasess", &beta[..])] {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(self.session_path(name), body).unwrap();
        }
    }

    fn cvd_cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cvd"));
        c.current_dir(&self.base)
            .env("HOME", &self.home)
            .env_remove("CVD_TOKEN") // hermetic: a token in the ambient env must not gate tests
            .env("CLUSTERVISION_HOME", &self.cv_home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"));
        c
    }

    /// A third, tool-heavy session (separate from `write_fixtures` so the sync tests' "2
    /// discovered" counts hold): 6 messages with an Edit, a failing Bash, a clean Bash, and a
    /// closing text — the raw material for the messages/events/touched endpoint tests.
    fn write_gamma(&self) {
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
        fs::write(self.session_path("gammasess"), body).unwrap();
    }

    /// A session with a compaction boundary (a `compact_boundary` system record carrying
    /// `compactMetadata`), so the `/compactions` endpoint has a boundary to surface.
    fn write_compacted(&self) {
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
        fs::write(self.session_path("compactsess"), body).unwrap();
    }

    /// Plant a workflow driving-script sidecar for `alphasess` so the `/workflow/{wf}/script`
    /// endpoint has something to serve: `<session>/workflows/scripts/<slug>-<wf>.js`.
    fn write_workflow_script(&self, wf: &str, body: &str) {
        let dir = self
            .home
            .join(".claude/projects/-work-proj/alphasess/workflows/scripts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("my-cool-flow-{wf}.js")), body).unwrap();
    }

    /// Build a tiny static web root (mirroring the repo's `web/`) so `serve --web` can be tested:
    /// an `index.html`, a JS asset under a subdir, and a "secret" file *outside* the root that a
    /// path-traversal attempt must never reach.
    fn write_web_root(&self) -> PathBuf {
        let web = self.base.join("web");
        fs::create_dir_all(web.join("components")).unwrap();
        fs::write(web.join("index.html"), "<!doctype html><title>cv dash</title>").unwrap();
        fs::write(web.join("components/cv-forest.js"), "export const FOREST = 1;\n").unwrap();
        // A file a `../` escape would try to read; it lives beside (not under) the web root.
        fs::write(self.base.join("secret.txt"), "TOP SECRET").unwrap();
        web
    }

    /// Run a cvd subcommand to completion; returns (stdout, stderr), asserting success.
    fn cvd(&self, args: &[&str]) -> (String, String) {
        let out = self.cvd_cmd().args(args).output().expect("cvd should run");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "cvd {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        (stdout, stderr)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn count_lines(p: &Path) -> usize {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

// ───────────────────────────── sync ─────────────────────────────

#[test]
fn sync_is_idempotent() {
    let w = World::new("sync");

    // First sync archives both sessions.
    let (out, _) = w.cvd(&["sync"]);
    assert!(
        out.contains("2 archived, 0 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    assert!(w.cv_home.join("archive/claude/alphasess.json").is_file());
    assert!(w.cv_home.join("archive/claude/betasess.json").is_file());
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2);

    // Second sync: nothing changed → nothing rewritten, no duplicate catalog rows.
    let (out, _) = w.cvd(&["sync"]);
    assert!(
        out.contains("0 archived, 2 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2, "no dupes on resync");

    // ls shows each session exactly once.
    let (out, _) = w.cvd(&["ls"]);
    assert!(out.contains("2 session(s)"), "{out}");
    assert_eq!(out.matches("alphases").count(), 1, "{out}");
    assert!(out.contains("alpha adventures"), "{out}");

    // Append a turn to one session: only that one re-archives; the catalog dedupes by key.
    let mut body = fs::read_to_string(w.session_path("alphasess")).unwrap();
    body.push_str(
        &json!({"type": "user", "uuid": "u9", "timestamp": "2026-06-02T10:00:00Z",
                "message": {"role": "user", "content": "one more thing"}})
        .to_string(),
    );
    body.push('\n');
    fs::write(w.session_path("alphasess"), body).unwrap();

    let (out, _) = w.cvd(&["sync"]);
    assert!(
        out.contains("1 archived, 1 skipped (unchanged) of 2 discovered"),
        "{out}"
    );
    let (out, _) = w.cvd(&["ls"]);
    assert!(
        out.contains("2 session(s)"),
        "changed session must not duplicate:\n{out}"
    );
    assert!(out.contains("3 msgs"), "updated message count visible:\n{out}");

    // The archived JSON is the parsed IR, scrubbed of nothing — spot-check its shape.
    let archived: Value =
        serde_json::from_str(&fs::read_to_string(w.cv_home.join("archive/claude/alphasess.json")).unwrap())
            .expect("archived session is valid JSON");
    assert_eq!(archived["id"], "alphasess");
    assert_eq!(archived["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn ls_and_path_on_empty_archive() {
    let w = World::new("empty");
    let (out, _) = w.cvd(&["ls"]);
    assert!(out.contains("archive empty"), "{out}");
    let (out, _) = w.cvd(&["path"]);
    assert_eq!(out.trim(), w.cv_home.display().to_string(), "{out}");
}

// ───────────────────────────── serve ─────────────────────────────

/// A minimal HTTP/1.0-style GET over a raw socket (no client dep): returns (status, body).
fn http(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to cvd serve");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

/// Like [`http`] but also hands back the `Content-Type`, for the static-asset rules where the
/// header IS the behaviour under test.
fn get_raw(port: u16, path: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to cvd serve");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {text}"));
    let ct = text
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
        .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
        .unwrap_or_default();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, ct, body)
}

fn get_json(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = http(port, "GET", path);
    let v = serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path}: non-JSON body {body:?}: {e}"));
    (status, v)
}

/// A child process killed on drop, so a failing test never leaks a server.
struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `cvd serve` on a free port and wait for the listener. Returns the port and the reaper
/// keeping the child alive (drop it to kill the server).
fn spawn_serve(w: &World) -> (u16, Reaper) {
    spawn_serve_with(w, &[])
}

/// Like [`spawn_serve`] but with extra `serve` args (e.g. `["--web", dir]`).
fn spawn_serve_with(w: &World, extra: &[&str]) -> (u16, Reaper) {
    spawn_serve_cfg(w, extra, &[])
}

/// Like [`spawn_serve_with`] but also with extra child env vars (e.g. `CVD_TOKEN`).
///
/// Binds `--port 0` (the OS assigns a genuinely free ephemeral port — no bind/release/rebind race
/// under parallel tests) and parses the REAL port out of the startup banner on stderr. The child
/// is `try_wait()`ed inside the wait loop so a server that dies at startup fails the test with its
/// exit status immediately instead of as connection-reset noise later.
fn spawn_serve_cfg(w: &World, extra: &[&str], envs: &[(&str, &str)]) -> (u16, Reaper) {
    let mut args: Vec<String> = vec!["serve".into(), "--port".into(), "0".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut child = w
        .cvd_cmd()
        .args(&args)
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cvd serve should spawn");

    // Read the banner (`cvd serve on http://host:PORT — …`) off stderr on a side thread; keep
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
        // "cvd serve on http://127.0.0.1:PORT — …": take the port from the URL.
        let rest = line.split("http://").nth(1)?;
        let hostport = rest.split_whitespace().next()?;
        hostport.rsplit(':').next()?.parse().ok()
    };

    let mut reaper = Reaper(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    let port = loop {
        if let Some(status) = reaper.0.try_wait().expect("try_wait cvd serve") {
            panic!("cvd serve exited at startup ({status}); stderr so far: {seen:?}");
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if line.starts_with("cvd serve on ") {
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
            "cvd serve never printed its banner; stderr so far: {seen:?}"
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
        if let Some(status) = reaper.0.try_wait().expect("try_wait cvd serve") {
            panic!("cvd serve died after its banner ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "cvd serve banner printed :{port} but it never accepts"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (port, reaper)
}

#[test]
fn serve_endpoints() {
    let w = World::new("serve");
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

    // unparseable limit is ignored (no truncation), not a 500.
    let (status, v) = get_json(port, "/api/sessions?limit=banana");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");

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

    // board endpoints on an empty channel: empty arrays.
    let (status, v) = get_json(port, "/api/board/fleet");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");
    // KNOWN cv-core BUG (handed off — see crates/cv-core/tests/board_fresh_home.rs): with no
    // board/ dir at all, active_claims errors → 500. Pre-create the dir so this exercises cvd's
    // routing rather than that bug; once cv-core is fixed the pre-create becomes a no-op.
    fs::create_dir_all(w.cv_home.join("board")).unwrap();
    let (status, v) = get_json(port, "/api/claims/fleet");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/who/fleet?within_secs=60");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // unknown route → 404; non-GET → 405; OPTIONS preflight → 204 with CORS.
    let (status, v) = get_json(port, "/api/definitely/not/a/route");
    assert_eq!(status, 404);
    assert!(v["error"].is_string(), "{v}");
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
    let w = World::new("window");
    w.write_gamma();
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
    let w = World::new("wfscript");
    w.write_workflow_script("wf_abc123", "// drive the swarm\nconst phases = ['plan','build'];\n");
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
    let w = World::new("head");
    w.write_fixtures();
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
    let w = World::new("static");
    w.write_fixtures();
    let webroot = w.write_web_root();
    let (port, _reaper) = spawn_serve_with(&w, &["--web", webroot.to_str().unwrap()]);

    let (status, ct, _) = get_raw(port, "/pkg/cv_web.js");
    assert_eq!(status, 404, "a missing module is a 404, not the index");
    assert!(!ct.contains("html"), "and not served as html: {ct}");

    let (status, ct, _) = get_raw(port, "/components/cv-forest.js");
    assert_eq!(status, 200);
    assert!(ct.contains("javascript"), "{ct}");

    // A route (no file extension) still resolves to the SPA index so deep links work.
    let (status, ct, body) = get_raw(port, "/session/claude/alphasess");
    assert_eq!(status, 200);
    assert!(ct.contains("html"), "{ct}");
    assert!(body.contains("<!doctype html>"), "{body}");
}

#[test]
fn serve_compactions() {
    let w = World::new("compact");
    w.write_compacted();
    let (port, _reaper) = spawn_serve(&w);

    // The dedicated scan (built on cv_core::compaction::detect) finds the one boundary, pairs it
    // with its summary, and reports the pre-compaction span — all in `/messages` index order.
    let (status, v) = get_json(port, "/api/session/claude/compactsess/compactions");
    assert_eq!(status, 200, "{v}");
    let arr = v["compactions"].as_array().expect("compactions array");
    assert_eq!(arr.len(), 1, "{v}");
    let c = &arr[0];
    assert_eq!(c["index"], 2, "boundary is the 3rd message (idx 2): {v}");
    assert_eq!(c["trigger"], "manual", "{v}");
    assert_eq!(c["pre_tokens"], 900000, "{v}");
    assert_eq!(c["duration_ms"], 120000, "{v}");
    // The summary that seeded the next window is paired and kept.
    assert_eq!(c["summary_index"], 3, "summary is the 4th message: {v}");
    assert!(c["summary"].as_str().unwrap().contains("the user asked for X"), "{v}");
    // The pre-compaction span is [0, boundary) — what was compacted away.
    assert_eq!(c["pre_span"], json!([0, 2]), "{v}");
    assert!(c["headline"].as_str().unwrap().contains("compaction #1"), "{v}");

    // A session with no compaction → empty list, not an error.
    let (status, v) = get_json(port, "/api/session/claude/alphasess/compactions");
    assert_eq!(status, 200, "{v}");
    assert!(v["compactions"].as_array().unwrap().is_empty(), "{v}");
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
    // Lean read: no `extra` populated (the map is absent or empty for the boundary) — but `kind`
    // is first-class, so the boundary is still identifiable without opting into the harness bag.
    let extra_empty = m["extra"].is_null() || m["extra"].as_object().map(|o| o.is_empty()).unwrap_or(true);
    assert!(extra_empty, "lean read should omit extra: {m}");
    assert_eq!(m["kind"], "compaction_boundary", "{m}");
}

/// `serve --web <dir>` hosts the dashboard from `/` while keeping the JSON API at `/api/*`, with
/// path-traversal confined to the web root and an SPA `index.html` fallback for unknown paths.
#[test]
fn serve_static_web_hub() {
    let w = World::new("webhub");
    let web = w.write_web_root();
    let (port, _reaper) = spawn_serve_with(&w, &["--web", web.to_str().unwrap()]);

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
    assert!(status == 403 || status == 200, "status {status}");
    assert!(!body.contains("TOP SECRET"), "traversal leaked the secret:\n{body}");
    // Encoded traversal too.
    let (status, body) = http(port, "GET", "/%2e%2e/secret.txt");
    assert!(
        !body.contains("TOP SECRET"),
        "encoded traversal leaked:\n{body} (status {status})"
    );
}

/// CORS is an allow-list, not a wildcard: local origins (and the Tauri app's) are echoed back
/// verbatim; any other origin gets **no** `Access-Control-Allow-Origin` at all, so a hostile page
/// the user happens to visit can't read the corpus cross-origin.
#[test]
fn serve_cors_allowlist() {
    let w = World::new("cors");
    let (port, _reaper) = spawn_serve(&w);

    for origin in [
        "http://localhost:5173",
        "http://127.0.0.1:9999",
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
/// silent-empty bug), the inbox rows, and the debt report envelope with an awaiting-review row.
#[test]
fn serve_tasks_routes() {
    use cv_core::task::{model::Revision, new_event, TaskEventKind, TaskStore};

    let w = World::new("tasks");
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
                channel: "tasks".into(),
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
    // A code task with a proposed revision (fabricated shas — the event log is the fixture; no
    // git needed to serve read views), so debt's awaiting_review section has a row.
    let coded = store
        .append_agent_event(new_event(
            None,
            "human",
            TaskEventKind::Opened {
                title: "review the pelican".into(),
                body: String::new(),
                repo: Some("/tmp/pelican-repo".into()),
                issue: None,
                channel: "tasks".into(),
                assignee: None,
            },
        ))
        .expect("seed code open");
    store
        .append_agent_event(new_event(
            Some(&coded.task_id),
            "agent:b",
            TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/pelican".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: "0".repeat(40),
                    review_sha: "1".repeat(40),
                    patch_id: "b".repeat(40),
                    reviewer: Some("agent:rev".into()),
                    session_ref: None,
                },
            },
        ))
        .expect("seed propose");

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

    // Effective-state filter hits and misses (the revision layer counts).
    let (_, v) = get_json(port, "/api/tasks?state=claimed");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/tasks?state=awaiting_review");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/tasks?state=open");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 0, "{v}");

    // An unknown state string is a 400 naming the typo and the vocabulary — never a silent [].
    let (status, v) = get_json(port, "/api/tasks?state=redy");
    assert_eq!(status, 400, "{v}");
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(msg.contains("redy") && msg.contains("awaiting_review"), "{v}");

    // /api/tasks/inbox/{who}: the claim shows for its owner; `since` stays off the wire.
    let (status, v) = get_json(port, "/api/tasks/inbox/agent:a");
    assert_eq!(status, 200, "{v}");
    let inbox = v["inbox"].as_array().expect("inbox array");
    assert_eq!(inbox.len(), 1, "{v}");
    assert_eq!(inbox[0]["id"], json!(id), "{v}");
    assert_eq!(inbox[0]["reason"], "claimed_by_you", "{v}");
    assert_eq!(inbox[0]["effective_state"], "claimed", "{v}");
    assert!(inbox[0].get("since").is_none(), "since stays off the wire: {v}");
    // The bound reviewer sees the awaiting revision; a stranger sees nothing.
    let (_, v) = get_json(port, "/api/tasks/inbox/agent:rev");
    assert_eq!(v["inbox"][0]["reason"], "awaiting_your_review", "{v}");
    let (_, v) = get_json(port, "/api/tasks/inbox/agent:nobody");
    assert_eq!(v["inbox"].as_array().unwrap().len(), 0, "{v}");

    // /api/tasks/debt: no reviewed-unlanded rows yet, one awaiting-review row, and the report is
    // loud about the never-run verifier.
    let (status, v) = get_json(port, "/api/tasks/debt");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["debt"].as_array().unwrap().len(), 0, "{v}");
    let awaiting = v["awaiting_review"].as_array().expect("awaiting array");
    assert_eq!(awaiting.len(), 1, "{v}");
    assert_eq!(awaiting[0]["id"], json!(coded.task_id), "{v}");
    assert_eq!(awaiting[0]["branch"], "task/pelican", "{v}");
    assert_eq!(awaiting[0]["reviewer"], "agent:rev", "{v}");
    assert_eq!(v["suspects"].as_array().unwrap().len(), 0, "{v}");
    assert!(v["verified_as_of"].is_null(), "{v}");
    assert!(
        v["verify_warning"].as_str().unwrap_or_default().contains("NEVER"),
        "{v}"
    );
    // Repo filter empties the awaiting section for a foreign repo.
    let (_, v) = get_json(port, "/api/tasks/debt?repo=/nowhere");
    assert_eq!(v["awaiting_review"].as_array().unwrap().len(), 0, "{v}");

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
    let w = World::new("host");
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
/// credential-less OPTIONS preflight stays exempt. Both `--token` and `$CVD_TOKEN` wire it up.
#[test]
fn serve_token_auth() {
    let w = World::new("token");
    let (port, _reaper) = spawn_serve_with(&w, &["--token", "opensesame"]);

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
    let (port, _reaper2) = spawn_serve_cfg(&w, &[], &[("CVD_TOKEN", "hunter2")]);
    let raw = raw_request(port, "GET", "/api/health", &[]);
    assert_eq!(raw_status(&raw), 401, "{raw}");
    let raw = raw_request(port, "GET", "/api/health", &[("Authorization", "Bearer hunter2")]);
    assert_eq!(raw_status(&raw), 200, "{raw}");
}

/// A non-loopback bind without auth is refused outright: the corpus can contain secrets, so
/// exposure demands a token or an explicit `--insecure-expose`.
#[test]
fn serve_refuses_bare_public_bind() {
    let w = World::new("expose");
    let out = w
        .cvd_cmd()
        .args(["serve", "--host", "0.0.0.0", "--port", "0"])
        .output()
        .expect("cvd should run");
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

/// The §3 search row: a session row plus what the search contributed. `cv search --json` emits
/// exactly these keys in exactly this order, so `/api/search` must too — a consumer that cannot
/// tell the CLI's rows from the daemon's is the whole point of the contract.
const SEARCH_ROW_KEYS: [&str; 14] = [
    "id",
    "harness",
    "path",
    "cwd",
    "title",
    "created_at",
    "updated_at",
    "message_count",
    "size_bytes",
    "score",
    "snippet",
    "agent_id",
    "parent_id",
    "workflow",
];

/// `/api/search` over a world with NO full-text index — i.e. the degraded path, which is the one
/// a hermetic test can drive (building a tantivy index needs a real corpus under the *test
/// process*'s `$HOME`, which these tests deliberately don't have; the indexed path is verified
/// against the real 6,873-session archive by hand). It must still answer, in the same row shape,
/// and SAY it degraded.
#[test]
fn serve_search() {
    let w = World::new("search");
    let (port, _reaper) = spawn_serve(&w);

    // A word only `alphasess` says. One row, and the row is the §3 search row.
    let raw = raw_get(port, "/api/search?q=zebrafish");
    assert_eq!(raw_status(&raw), 200, "{raw}");
    let v = raw_json(&raw);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1, "{v}");
    assert_eq!(arr[0]["id"], "alphasess", "{v}");
    let keys: Vec<&str> = arr[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, SEARCH_ROW_KEYS, "search row must match §3 exactly: {v}");
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
    assert!(note.contains("no index"), "the degrade must be stated: {raw}");

    // An empty or missing query is a caller mistake, not "every session".
    for path in ["/api/search", "/api/search?q=", "/api/search?limit=5"] {
        let (status, v) = get_json(port, path);
        assert_eq!(status, 400, "{path}: {v}");
        assert!(v["error"].as_str().unwrap().contains("q parameter"), "{v}");
    }

    // harness: a filter, and a typo is a clear 400 (same as /api/sessions).
    let (status, v) = get_json(port, "/api/search?q=zebrafish&harness=claude");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, v) = get_json(port, "/api/search?q=zebrafish&harness=codex");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/search?q=zebrafish&harness=warpdrive");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("unknown harness"), "{v}");

    // cwd: a substring of the row's cwd, matching /api/sessions?cwd=.
    let (_, v) = get_json(port, "/api/search?q=zebrafish&cwd=%2Fwork%2Fproj");
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (_, v) = get_json(port, "/api/search?q=zebrafish&cwd=nowhere");
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // A needle both fixtures contain ("plea(se)" / "(se)venteen"), so `limit` bites — and the
    // truncation is stated rather than silently looking like the whole answer.
    let (_, v) = get_json(port, "/api/search?q=se");
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let raw = raw_get(port, "/api/search?q=se&limit=1");
    assert_eq!(raw_json(&raw).as_array().unwrap().len(), 1);
    assert!(
        header_of(&raw, "X-Cv-Search-Note")
            .unwrap_or_default()
            .contains("stopped at 1 hits"),
        "{raw}"
    );

    // A miss is an empty array, never an error.
    let (status, v) = get_json(port, "/api/search?q=pangolin");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // The degrade signal is readable cross-origin — a header a dev-server dashboard can't read is
    // not a channel.
    let raw = raw_request(
        port,
        "GET",
        "/api/search?q=zebrafish",
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
    let w = World::new("stats");
    w.write_gamma();
    let (port, _reaper) = spawn_serve(&w);

    let (status, v) = get_json(port, "/api/stats");
    assert_eq!(status, 200, "{v}");
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "sessions",
            "messages",
            "by_harness",
            "top_cwds",
            "earliest_created",
            "latest_updated"
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

/// Raw GET returning the full response text (headers + body) — for content-type / CORS assertions.
fn raw_get(port: u16, path: &str) -> String {
    raw_request(port, "GET", path, &[])
}

/// A raw request with custom headers, returning the full response text (headers + body).
/// A default local `Host` is supplied unless the caller passes their own.
fn raw_request(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut s = String::new();
    stream.read_to_string(&mut s).unwrap();
    s
}

/// The status code from a raw response's status line.
fn raw_status(raw: &str) -> u16 {
    raw.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {raw}"))
}
