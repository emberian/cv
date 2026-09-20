//! **Door parity**: the same noun, asked through the CLI and through the daemon, must come back
//! the same shape.
//!
//! cv answers the same questions through several doors — `cv <cmd> --json`, `cvd serve`'s HTTP API,
//! the MCP tools (generated from the CLI), and the desktop app's native commands. A consumer should
//! never have to ask *which door did this row come through?* before reading it, which is what
//! `docs/INTERFACE-V2.md` §3 means by one shape per noun.
//!
//! Every one of these was a real bug, shipped, and found by hand rather than by a test:
//!
//! * `/api/sessions` omitted `path` and `size_bytes`, so its rows were not the §3 session row.
//! * `/api/touched` and `/api/session/…/events` dropped the `agent_id`/`parent_id`/`workflow`
//!   trio, so a caller could not tell a top-level run from one lane of a workflow.
//! * `/api/session/…/compactions` wrapped its list in `{harness, id, compactions}` — the only list
//!   in the API shaped differently from its siblings — and renamed `boundary_msg_idx` to `index`
//!   and `pre_compaction_span` to `pre_span`.
//! * The same instant came back `…Z` from one route and `…+00:00` from another and from the CLI.
//!
//! This test is the thing that notices next time. It deliberately compares **key sets**, not
//! values: the two doors read the same store at slightly different moments, and the point is the
//! contract, not the contents. A door may ADD a field (the daemon computes `headline` for the
//! dashboard); it may not rename or drop one.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A throwaway HOME with one Claude session that exercises every noun we compare: a prompt, an
/// assistant turn with a tool call, a tool result (so `events` has something to extract), and a
/// compaction boundary with its summary (so `compactions` is non-empty).
struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new() -> World {
        let base = std::env::temp_dir().join(format!(
            "cv-parity-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&cv_home).unwrap();

        let lines = [
            r#"{"type":"user","uuid":"m1","sessionId":"paritysess","cwd":"/work/proj","gitBranch":"main","timestamp":"2026-04-01T08:00:00Z","message":{"role":"user","content":"fix the parser"}}"#.to_string(),
            r#"{"type":"assistant","uuid":"m2","parentUuid":"m1","sessionId":"paritysess","timestamp":"2026-04-01T08:01:00Z","message":{"role":"assistant","model":"claude-test","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/parser.rs"}}]}}"#.to_string(),
            r#"{"type":"user","uuid":"m3","parentUuid":"m2","sessionId":"paritysess","timestamp":"2026-04-01T08:02:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"toolUseResult":{"filePath":"src/parser.rs"}}"#.to_string(),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"m4","sessionId":"paritysess","timestamp":"2026-04-01T08:03:00Z","content":"Conversation compacted","compactMetadata":{"trigger":"manual","preTokens":900000,"postTokens":1200,"durationMs":1000}}"#.to_string(),
            r#"{"type":"user","uuid":"m5","parentUuid":"m4","sessionId":"paritysess","isCompactSummary":true,"timestamp":"2026-04-01T08:04:00Z","message":{"role":"user","content":"Summary: we fixed the parser."}}"#.to_string(),
        ];
        std::fs::write(proj.join("paritysess.jsonl"), lines.join("\n") + "\n").unwrap();
        World { base, home, cv_home }
    }

    fn cmd(&self, exe: &str) -> Command {
        let mut c = Command::new(exe);
        c.current_dir(&self.base)
            .env("HOME", &self.home)
            .env_remove("CVD_TOKEN")
            .env("CLUSTERVISION_HOME", &self.cv_home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"));
        c
    }

    /// `cv` lives beside `cvd` in the same target dir; Cargo only hands a test the bin paths of its
    /// OWN package, so resolve the sibling rather than guessing a profile directory.
    fn cv_exe(&self) -> PathBuf {
        let cvd = PathBuf::from(env!("CARGO_BIN_EXE_cvd"));
        cvd.with_file_name(if cfg!(windows) { "cv.exe" } else { "cv" })
    }

    fn cli(&self, args: &[&str]) -> Option<Value> {
        let out = self.cmd(self.cv_exe().to_str().unwrap()).args(args).output().ok()?;
        serde_json::from_slice(&out.stdout).ok()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `cvd serve` on an OS-assigned port and read the real port out of its banner.
fn spawn_serve(w: &World) -> (u16, Reaper) {
    let mut child = w
        .cmd(env!("CARGO_BIN_EXE_cvd"))
        .args(["serve", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cvd serve should spawn");
    let mut err = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while err.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                let line = String::from_utf8_lossy(&buf).to_string();
                if let Some(p) = line.rsplit(':').next().and_then(|s| {
                    s.trim_end_matches(|c: char| !c.is_ascii_digit())
                        .split_whitespace()
                        .last()
                        .and_then(|d| d.parse::<u16>().ok())
                }) {
                    let _ = tx.send(p);
                }
                buf.clear();
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("cvd should announce its port");
    (port, Reaper(child))
}

fn api(port: u16, path: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
            let mut raw = Vec::new();
            if s.read_to_end(&mut raw).is_ok() {
                let text = String::from_utf8_lossy(&raw);
                if let Some((_, body)) = text.split_once("\r\n\r\n") {
                    if let Ok(v) = serde_json::from_str::<Value>(body) {
                        return v;
                    }
                }
            }
        }
        if Instant::now() > deadline {
            panic!("GET {path} never returned JSON");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The key set of a row: the first element for a list, the object itself otherwise.
fn row_keys(v: &Value) -> Vec<String> {
    let row = match v {
        Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    let mut k: Vec<String> = row.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    k.sort();
    k
}

#[test]
fn cli_and_daemon_answer_the_same_nouns_the_same_way() {
    let w = World::new();
    let (port, _reaper) = spawn_serve(&w);
    let id = "paritysess";

    // (noun, CLI argv, API path, keys the daemon may ADD without it being a divergence)
    let cases: &[(&str, &[&str], String, &[&str])] = &[
        ("session row", &["ls", "--json"], "/api/sessions".into(), &[]),
        (
            "events",
            &["events", id, "--json"],
            format!("/api/session/claude/{id}/events"),
            &[],
        ),
        (
            "compactions",
            &["compaction", id, "--json"],
            format!("/api/session/claude/{id}/compactions"),
            // The daemon renders a headline for the dashboard. Adding is fine; renaming is not.
            &["headline"],
        ),
    ];

    for (noun, argv, path, may_add) in cases {
        let cli = w
            .cli(argv)
            .unwrap_or_else(|| panic!("`cv {}` produced no JSON", argv.join(" ")));
        let apiv = api(port, path);
        let (a, b) = (row_keys(&cli), row_keys(&apiv));
        assert!(
            !a.is_empty(),
            "the {noun} fixture produced no CLI row — fix the fixture"
        );
        let missing: Vec<&String> = a.iter().filter(|k| !b.contains(k)).collect();
        let extra: Vec<&String> = b
            .iter()
            .filter(|k| !a.contains(k) && !may_add.contains(&k.as_str()))
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{noun}: the daemon and the CLI disagree.\n  only in cv:  {missing:?}\n  only in api: {extra:?}\n\
             A door may ADD a field; it may not rename or drop one (docs/INTERFACE-V2.md §3)."
        );
    }

    // Timestamps: one spelling everywhere. serde's chrono default writes `…Z`, `to_rfc3339()`
    // writes `…+00:00`, and for a release the same instant came back both ways.
    let cli_row = w.cli(&["ls", "--json"]).expect("ls json");
    let api_row = api(port, "/api/sessions");
    for (door, v) in [("cv ls --json", &cli_row), ("/api/sessions", &api_row)] {
        let t = v[0]["created_at"].as_str().unwrap_or_default();
        assert!(
            t.ends_with("+00:00"),
            "{door} must spell UTC as `+00:00` like every other door, got {t:?}"
        );
    }

    // A list is a list. `/api/…/compactions` was once the only one wrapped in an object.
    for path in [
        "/api/sessions",
        &format!("/api/session/claude/{id}/events"),
        &format!("/api/session/claude/{id}/compactions"),
    ] {
        assert!(
            api(port, path).is_array(),
            "{path} must be a bare array, like every other list this API serves"
        );
    }
}
