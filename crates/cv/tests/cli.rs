//! End-to-end tests for the `cv` CLI surface: ls sorting, search fallback messaging, show
//! windowing, export formats, events/touched, redact, and error-path honesty (nonzero + stderr,
//! never a panic).
//!
//! Hermetic: every test builds its own temp `HOME` (harness discovery roots hang off the home
//! dir) and temp `CLUSTERVISION_HOME` (catalog/index/board), and only ever passes them to the
//! spawned binary's environment — no process-global `set_var`, so tests run in parallel safely.

use std::path::PathBuf;
use std::process::Command;
use std::{fs, str};

/// A planted secret that `cv redact` must scrub (matches cv_core::redact's sk- recognizer).
const SECRET: &str = "sk-abcDEF1234567890ghijkl";

/// One temp world: a fake `$HOME` (with `.claude/projects` fixtures) + a `$CLUSTERVISION_HOME`.
struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cv-cli-{tag}-{}-{}",
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
        World { base, home, cv_home }
    }

    /// Write a claude-format session fixture; `name` becomes the session id.
    fn write_session(&self, name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = self
            .home
            .join(".claude/projects/-work-proj")
            .join(format!("{name}.jsonl"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(&path, body).unwrap();
        path
    }

    /// Run `cv` with this world's env; returns (status_ok, exit_code, stdout, stderr).
    fn cv(&self, args: &[&str]) -> (bool, i32, String, String) {
        self.cv_env(args, &[])
    }

    /// Like [`World::cv`] with extra env vars. The ambient `CV_ENDPOINT` is always cleared
    /// first so identity-resolution tests see exactly the environment they set.
    fn cv_env(&self, args: &[&str], extra: &[(&str, &str)]) -> (bool, i32, String, String) {
        self.cv_full(args, extra, None)
    }

    /// Like [`World::cv`], feeding `stdin` to the process.
    fn cv_stdin(&self, args: &[&str], stdin: &str) -> (bool, i32, String, String) {
        self.cv_full(args, &[], Some(stdin))
    }

    fn cv_full(&self, args: &[&str], extra: &[(&str, &str)], stdin: Option<&str>) -> (bool, i32, String, String) {
        use std::io::Write;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cv"));
        cmd.args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLUSTERVISION_HOME", &self.cv_home)
            // Linux fallbacks for dirs::cache_dir / config_dir; harmless on macOS.
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env_remove("CV_ENDPOINT")
            // Hermetic: an agent seat's own Claude config dir must not add its sessions here.
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLUSTERVISION_CLAUDE_ROOTS");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = match stdin {
            None => cmd.stdin(std::process::Stdio::null()).output().expect("cv should run"),
            Some(text) => {
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = cmd.spawn().expect("cv should spawn");
                child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
                child.wait_with_output().expect("cv should run")
            }
        };
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Seed the task log directly (the only way to plant an OLD event: the CLI stamps `now`).
    fn seed_task_log(&self, lines: &[serde_json::Value]) {
        let dir = self.cv_home.join("tasks");
        fs::create_dir_all(&dir).unwrap();
        let mut body = String::from("{\"format\":\"cv-task-log\",\"v\":1}\n");
        for l in lines {
            body.push_str(&l.to_string());
            body.push('\n');
        }
        fs::write(dir.join("events.jsonl"), body).unwrap();
    }

    /// Like `cv`, asserting success and returning (stdout, stderr).
    fn cv_ok(&self, args: &[&str]) -> (String, String) {
        let (ok, code, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// Assert the command fails with a nonzero exit and *some* explanation on stderr.
    fn cv_fails(&self, args: &[&str]) -> (String, String) {
        let (ok, code, out, err) = self.cv(args);
        assert!(!ok, "cv {args:?} unexpectedly succeeded\nstdout:\n{out}");
        assert_ne!(code, -1, "cv {args:?} died by signal (panic/abort?)\nstderr:\n{err}");
        assert!(
            !err.trim().is_empty(),
            "cv {args:?} failed silently (empty stderr)\nstdout:\n{out}"
        );
        assert!(
            !err.contains("panicked"),
            "cv {args:?} panicked instead of erroring:\n{err}"
        );
        (out, err)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn user_line(uuid: &str, ts: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "cwd": "/work/proj",
        "message": {"role": "user", "content": text}
    })
}

fn assistant_line(uuid: &str, ts: &str, blocks: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
        "cwd": "/work/proj",
        "message": {"role": "assistant", "model": "claude-test-1", "content": blocks}
    })
}

/// The standard two-session corpus:
/// - `alphasess`: created 2026-01-01, updated 2026-06-01, 3 messages, title "alpha adventures",
///   contains "zebrafish", a planted secret, and an Edit on /work/proj/src/widget.rs.
/// - `betasess`: created 2026-03-01, updated 2026-03-02, 4 messages, no title.
///
/// Orderings disagree on purpose: updated → alpha first; created → beta first; messages → beta.
fn standard_corpus(w: &World) {
    w.write_session(
        "alphasess",
        &[
            serde_json::json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            user_line("u1", "2026-01-01T10:00:00Z", "please fix the zebrafish migration"),
            assistant_line(
                "a1",
                "2026-01-02T10:00:00Z",
                serde_json::json!([
                    {"type": "text", "text": format!("on it — found a leaked key {SECRET} in the env")},
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "/work/proj/src/widget.rs", "old_string": "a", "new_string": "b"}}
                ]),
            ),
            serde_json::json!({
                "type": "user", "uuid": "u2", "sessionId": "s", "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "edited ok", "is_error": false}
                ]}
            }),
        ],
    );
    w.write_session(
        "betasess",
        &[
            user_line("b1", "2026-03-01T09:00:00Z", "how many quokkas in the census"),
            assistant_line(
                "b2",
                "2026-03-01T09:05:00Z",
                serde_json::json!([{"type": "text", "text": "seventeen quokkas"}]),
            ),
            user_line("b3", "2026-03-02T09:00:00Z", "thanks"),
            assistant_line(
                "b4",
                "2026-03-02T09:01:00Z",
                serde_json::json!([{"type": "text", "text": "anytime"}]),
            ),
        ],
    );
}

/// Index of `needle`'s first occurrence, with a labeled panic when missing.
fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in:\n{hay}"))
}

// ───────────────────────────── ls ─────────────────────────────

#[test]
fn ls_sort_modes_and_filters() {
    let w = World::new("ls");
    standard_corpus(&w);

    // Default (updated): alpha's last timestamp (06-01) beats beta's (03-02).
    let (out, _) = w.cv_ok(&["ls"]);
    assert!(out.contains("2 session(s)"), "{out}");
    assert!(pos(&out, "alphases") < pos(&out, "betasess"), "updated order:\n{out}");
    assert!(out.contains("alpha adventures"), "{out}");

    // created: beta (03-01) is newer than alpha (01-01).
    let (out, _) = w.cv_ok(&["ls", "--sort-by", "created"]);
    assert!(pos(&out, "betasess") < pos(&out, "alphases"), "created order:\n{out}");

    // messages: beta has 4, alpha 3.
    let (out, _) = w.cv_ok(&["ls", "--sort-by", "messages"]);
    assert!(pos(&out, "betasess") < pos(&out, "alphases"), "messages order:\n{out}");
    assert!(out.contains("4 msg"), "{out}");

    // explicit updated round-trips the default.
    let (out, _) = w.cv_ok(&["ls", "--sort-by", "updated"]);
    assert!(pos(&out, "alphases") < pos(&out, "betasess"), "{out}");

    // A bad sort key is a clap-level error: nonzero exit, usage on stderr, no panic.
    let (_, err) = w.cv_fails(&["ls", "--sort-by", "bogus"]);
    assert!(err.contains("bogus"), "{err}");

    // --limit truncates and says how many more exist.
    let (out, _) = w.cv_ok(&["ls", "--limit", "1"]);
    assert!(out.contains("… 1 more"), "{out}");

    // cwd filter notes how much was filtered away.
    let (out, _) = w.cv_ok(&["ls", "--cwd", "/work/proj"]);
    assert!(out.contains("2 session(s)"), "{out}");
    let (out, _) = w.cv_ok(&["ls", "--cwd", "/nowhere"]);
    assert!(out.contains("0 session(s) (of 2 discovered; filtered)"), "{out}");

    // Unknown harness is a real error.
    let (_, err) = w.cv_fails(&["ls", "--harness", "clippy9000"]);
    assert!(err.contains("unknown harness"), "{err}");
}

#[test]
fn ls_json_emits_the_same_rows_machine_readably() {
    let w = World::new("lsjson");
    standard_corpus(&w);

    // Stdout is exactly one JSON array — no header/footer — with one object per table row,
    // in table order (updated: alpha first), carrying the one snake_case session-row shape (INTERFACE-V2 §3).
    let (out, _) = w.cv_ok(&["ls", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    assert_eq!(rows.len(), 2, "{out}");
    assert_eq!(rows[0]["id"], "alphasess", "updated order:\n{out}");
    assert_eq!(rows[0]["harness"], "claude");
    assert_eq!(rows[0]["title"], "alpha adventures");
    assert_eq!(rows[0]["message_count"], 3);
    assert_eq!(rows[0]["cwd"], "/work/proj");
    assert!(
        rows[0]["created_at"].as_str().unwrap().starts_with("2026-01-01"),
        "{out}"
    );
    assert!(
        rows[0]["updated_at"].as_str().unwrap().starts_with("2026-06-01"),
        "{out}"
    );
    assert!(rows[0]["path"].as_str().unwrap().ends_with("alphasess.jsonl"), "{out}");
    // betasess has no title: present as an explicit null, not absent.
    assert!(rows[1]["title"].is_null(), "{out}");
    // size_bytes is always emitted (it comes free from the exists() guard's metadata call) and
    // matches the file's real length; the enrichment fields stay absent without --enrich.
    let real_size = std::fs::metadata(rows[0]["path"].as_str().unwrap()).unwrap().len();
    assert_eq!(rows[0]["size_bytes"].as_u64().unwrap(), real_size, "{out}");
    assert!(rows[0]["size_bytes"].as_u64().unwrap() > 0, "{out}");
    assert!(rows[0].get("git").is_none(), "no --enrich ⇒ no git:\n{out}");
    assert!(
        rows[0].get("display_title").is_none(),
        "no --enrich ⇒ no display_title:\n{out}"
    );

    // --limit bounds the array just like the table, with no "… N more" footer polluting stdout.
    let (out, _) = w.cv_ok(&["ls", "--json", "--limit", "1"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rows.len(), 1, "{out}");

    // Filters apply identically; an empty result is an empty array, still valid JSON.
    let (out, _) = w.cv_ok(&["ls", "--json", "--harness", "codex"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(rows.is_empty(), "{out}");
}

#[test]
fn ls_json_enrich_adds_git_branch_and_synthesized_title() {
    let w = World::new("lsjsonenrich");
    // A session with a recorded git branch and NO explicit title — so display_title must be
    // synthesized from the first real user turn, and a leading <system-reminder> block is peeled.
    w.write_session(
        "enrichsess",
        &[
            serde_json::json!({
                "type": "user", "uuid": "e1", "sessionId": "s",
                "timestamp": "2026-05-01T10:00:00Z", "cwd": "/work/proj", "gitBranch": "feature/enrich",
                "message": {"role": "user", "content":
                    "<system-reminder>be nice</system-reminder>\n\nteach me about capybaras please"}
            }),
            assistant_line(
                "e2",
                "2026-05-01T10:01:00Z",
                serde_json::json!([{"type": "text", "text": "capybaras are the largest rodents"}]),
            ),
        ],
    );

    let (out, _) = w.cv_ok(&["ls", "--json", "--enrich"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).expect("pure JSON");
    assert_eq!(rows.len(), 1, "{out}");
    // The raw catalog title stays null; display_title carries the synthesized fallback with the
    // system-reminder noise stripped.
    assert!(rows[0]["title"].is_null(), "{out}");
    assert_eq!(rows[0]["display_title"], "teach me about capybaras please", "{out}");
    // git object matches `cv show --json`'s shape (branch present).
    assert_eq!(rows[0]["git"]["branch"], "feature/enrich", "{out}");
    // size_bytes still present alongside the enrichment.
    assert!(rows[0]["size_bytes"].as_u64().unwrap() > 0, "{out}");
}

// ───────────────────────────── search ─────────────────────────────

#[test]
fn search_fallback_messaging_and_index_path() {
    let w = World::new("search");
    standard_corpus(&w);

    // No index yet: a live scan with an explicit note, that still finds the content.
    let (out, err) = w.cv_ok(&["search", "zebrafish"]);
    assert!(err.contains("no index yet"), "want live-scan note, got stderr:\n{err}");
    assert!(err.contains("scanning live"), "{err}");
    assert!(out.contains("alphases"), "{out}");
    assert!(out.to_lowercase().contains("zebrafish"), "snippet expected:\n{out}");

    // A stale legacy sqlite index gets a cleanup nudge.
    fs::write(w.cv_home.join("index.sqlite"), b"stale").unwrap();
    let (_, err) = w.cv_ok(&["search", "zebrafish"]);
    assert!(
        err.contains("legacy sqlite index no longer used"),
        "want legacy-index note, got stderr:\n{err}"
    );
    fs::remove_file(w.cv_home.join("index.sqlite")).unwrap();

    // Build the index; search must now use it (no live-scan note) and still hit.
    let (out, _) = w.cv_ok(&["index"]);
    assert!(out.contains("indexed 2 top-level session(s)"), "{out}");
    let (out, err) = w.cv_ok(&["search", "zebrafish"]);
    assert!(
        !err.contains("scanning live"),
        "indexed search must not live-scan:\n{err}"
    );
    assert!(out.contains("alphases"), "{out}");

    // The index is authoritative: a miss is a miss (with the index hint), not a fallback.
    let (out, _) = w.cv_ok(&["search", "xyzzyplugh"]);
    assert!(out.contains("no matches"), "{out}");
    assert!(out.contains("(index"), "{out}");

    // Harness filter that matches nothing still exits 0 with the no-match message.
    let (out, _) = w.cv_ok(&["search", "zebrafish", "--harness", "codex"]);
    assert!(out.contains("no matches"), "{out}");
}

#[test]
fn search_json_emits_machine_readable_hits() {
    let w = World::new("searchjson");
    standard_corpus(&w);

    // Live-scan path (no index yet): the note stays on stderr; stdout is exactly one JSON
    // array with the FULL session id (the table truncates to "alphases"), the snippet the
    // table would print, and an explicit null score (a live scan has none).
    let (out, err) = w.cv_ok(&["search", "zebrafish", "--json"]);
    assert!(err.contains("scanning live"), "{err}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0]["id"], "alphasess", "full id, not the 8-char prefix:\n{out}");
    assert_eq!(rows[0]["harness"], "claude");
    assert_eq!(rows[0]["title"], "alpha adventures");
    assert!(rows[0]["snippet"].as_str().unwrap().contains("zebrafish"), "{out}");
    assert!(rows[0]["score"].is_null(), "{out}");
    // Sub-agent provenance keys are always present — null for a top-level hit — so consumers
    // can branch on them without probing for the keys.
    for key in ["agent_id", "parent_id", "workflow"] {
        let obj = rows[0].as_object().unwrap();
        assert!(obj.contains_key(key), "provenance key {key} must be present:\n{out}");
        assert!(obj[key].is_null(), "top-level hit must have null {key}:\n{out}");
    }
    assert!(
        rows[0]["updated_at"].as_str().unwrap().starts_with("2026-06-01"),
        "{out}"
    );

    // A live-scan miss is an empty array — valid JSON, no prose on stdout.
    let (out, _) = w.cv_ok(&["search", "xyzzyplugh", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(rows.is_empty(), "{out}");

    // --limit bounds the array; the "stopped at" footer moves to stderr, keeping stdout pure.
    // ("the" appears in both fixture sessions on the live path.)
    let (out, err) = w.cv_ok(&["search", "the", "--json", "--limit", "1"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rows.len(), 1, "{out}");
    assert!(err.contains("stopped at 1"), "{err}");

    // Indexed path: same hit, still the full id, now with a real BM25 score and dates off
    // the index.
    w.cv_ok(&["index"]);
    let (out, err) = w.cv_ok(&["search", "zebrafish", "--json"]);
    assert!(!err.contains("scanning live"), "{err}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0]["id"], "alphasess", "{out}");
    assert!(rows[0]["score"].as_f64().unwrap() > 0.0, "{out}");
    // Provenance keys ride on indexed hits too (null: the fixture has no sub-agent lanes).
    let obj = rows[0].as_object().unwrap();
    for key in ["agent_id", "parent_id", "workflow"] {
        assert!(
            obj.contains_key(key) && obj[key].is_null(),
            "{key} present+null:\n{out}"
        );
    }
    assert!(
        rows[0]["updated_at"].as_str().unwrap().starts_with("2026-06-01"),
        "{out}"
    );

    // Indexed miss and non-matching --harness filter: empty arrays, still valid JSON.
    let (out, _) = w.cv_ok(&["search", "xyzzyplugh", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(rows.is_empty(), "{out}");
    let (out, _) = w.cv_ok(&["search", "zebrafish", "--json", "--harness", "codex"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(rows.is_empty(), "{out}");
}

// ───────────────────────────── show windows ─────────────────────────────

/// The 0.11 window grammar, on the one command every consumer reaches for: `--range A..B` is
/// 0-based and end-exclusive, `--first/--last/--around` are the named selectors, only one of the
/// four may be given, and every 0.10 spelling errors with a pointer at what replaced it.
#[test]
fn show_range_windowing() {
    let w = World::new("show");
    standard_corpus(&w);

    // Whole session: header + all three turns.
    let (out, _) = w.cv_ok(&["show", "alphasess"]);
    assert!(out.contains("# alpha adventures"), "{out}");
    assert!(out.contains("zebrafish migration"), "{out}");
    assert!(out.contains("[tool_use Edit t1]"), "{out}");
    assert!(out.contains("edited ok"), "{out}");

    // `A..B` is end-exclusive: 1..2 is exactly msg 1 (the assistant turn).
    let (out, _) = w.cv_ok(&["show", "alphasess", "--range", "1..2"]);
    assert!(out.contains("[tool_use Edit t1]"), "{out}");
    assert!(!out.contains("zebrafish"), "window must exclude msg 0:\n{out}");
    assert!(!out.contains("edited ok"), "window must exclude msg 2:\n{out}");

    // Open-ended tail `A..`.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--range", "2.."]);
    assert!(out.contains("edited ok"), "{out}");
    assert!(!out.contains("zebrafish"), "{out}");

    // Open-ended head `..B` — end-exclusive, so `..1` is just msg 0.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--range", "..1"]);
    assert!(out.contains("zebrafish"), "{out}");
    assert!(!out.contains("[tool_use Edit t1]"), "{out}");

    // `--first N` is the replacement for the old `-N` head window.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--first", "1"]);
    assert!(out.contains("zebrafish"), "{out}");
    assert!(!out.contains("[tool_use Edit t1]"), "{out}");
    let (out, _) = w.cv_ok(&["show", "alphasess", "--first", "2"]);
    assert!(out.contains("zebrafish") && out.contains("[tool_use Edit t1]"), "{out}");
    assert!(!out.contains("edited ok"), "{out}");

    // `--last N` counts from the end (3 messages ⇒ --last 1 is msg 2).
    let (out, _) = w.cv_ok(&["show", "alphasess", "--last", "1"]);
    assert!(out.contains("edited ok"), "{out}");
    assert!(!out.contains("zebrafish"), "{out}");
    // More than exist ⇒ the whole session, not an error.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--last", "99"]);
    assert!(out.contains("zebrafish") && out.contains("edited ok"), "{out}");

    // `--around N` with an explicit --context: 0 is the single message the old bare `N` meant.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--around", "1", "--context", "0"]);
    assert!(out.contains("[tool_use Edit t1]"), "{out}");
    assert!(!out.contains("zebrafish"), "{out}");
    assert!(!out.contains("edited ok"), "{out}");
    // …and context pulls in the neighbours either side.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--around", "1", "--context", "1"]);
    assert!(out.contains("zebrafish") && out.contains("edited ok"), "{out}");
    // --context without --around is a usage error (it has no centre to hang off).
    let (_, err) = w.cv_fails(&["show", "alphasess", "--context", "2"]);
    assert!(err.contains("--around"), "{err}");

    // `--max-bytes` truncates the render and prints the copy-pasteable continuation line.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--max-bytes", "1"]);
    assert!(
        out.contains("continue with --range"),
        "continuation line expected:\n{out}"
    );
    assert!(!out.contains("edited ok"), "budget must cut the render short:\n{out}");

    // Range entirely past the end: still exits 0, renders the header and no messages.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--range", "10..20"]);
    assert!(out.contains("# alpha adventures"), "{out}");
    assert!(!out.contains("──"), "no message blocks expected:\n{out}");

    // Inverted range is rejected.
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "9..3"]);
    assert!(err.contains("end (3) is before start (9)"), "{err}");

    // Garbage range is rejected.
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "abc"]);
    assert!(err.contains("bad range"), "{err}");

    // ── the 0.10 grammar is GONE, and each rejection names its replacement ──
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "1-2"]);
    assert!(
        err.contains("A..B") && err.contains("--first N") && err.contains("--last N"),
        "`A-B` must point at the new grammar:\n{err}"
    );
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "-1"]);
    assert!(
        err.contains("--first N") && err.contains("--last N"),
        "`-N` must point at the flags:\n{err}"
    );
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "2-"]);
    assert!(err.contains("A.."), "`N-` must point at `A..`:\n{err}");
    let (_, err) = w.cv_fails(&["show", "alphasess", "--range", "1"]);
    assert!(
        err.contains("--around 1 --context 0") && err.contains("1..2"),
        "a bare index must point at both spellings of one message:\n{err}"
    );

    // ── exactly one selector: clap refuses two at once, naming the conflict ──
    for pair in [
        ["--first", "--last"],
        ["--first", "--range"],
        ["--last", "--range"],
        ["--first", "--around"],
        ["--last", "--around"],
        ["--range", "--around"],
    ] {
        let a = if pair[0] == "--range" { "0..1" } else { "1" };
        let b = if pair[1] == "--range" { "0..1" } else { "1" };
        let args = ["show", "alphasess", pair[0], a, pair[1], b];
        let (ok, code, out, err) = w.cv(&args);
        assert!(!ok, "two selectors must be refused: cv {args:?}\n{out}");
        assert_eq!(code, 2, "clap usage errors exit 2\nstderr:\n{err}");
        assert!(
            err.contains("cannot be used with"),
            "conflict must be named: cv {args:?}\n{err}"
        );
    }

    // JSON path honors the same windows.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--json", "--range", "1..2"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "{out}");
    let (out, _) = w.cv_ok(&["show", "alphasess", "--json", "--first", "2"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{out}");
    let (out, _) = w.cv_ok(&["show", "alphasess", "--json", "--last", "1"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "{out}");
    // And a past-the-end JSON window is empty, not a panic.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--json", "--range", "10..20"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{out}");
}

/// `--thinking <native|text|drop>` means the same thing on every command that emits a session, and
/// nothing else answers to that word. The modes differ in whether a turn whose only content is
/// provider-signed reasoning survives a target that cannot hold the signature.
#[test]
fn thinking_mode_is_one_word_on_every_emitting_command() {
    let w = World::new("think");
    standard_corpus(&w);

    // Present, with the same three values, wherever a session is written out.
    for cmd in ["port", "splice", "loom", "pack"] {
        let (ok, _, out, err) = w.cv(&[cmd, "--help"]);
        assert!(ok, "cv {cmd} --help failed:\n{err}");
        for mode in ["native", "text", "drop"] {
            assert!(
                out.contains(mode),
                "cv {cmd} --help must offer --thinking {mode}:\n{out}"
            );
        }
    }

    // An unknown mode is rejected by name, listing what is valid.
    let (_, _, _, err) = w.cv(&["port", "alphasess", "--thinking", "sideways"]);
    assert!(err.contains("sideways"), "the bad value is quoted back:\n{err}");

    // And the word is NOT overloaded: prune's old boolean spelling is gone.
    let (_, code, _, err) = w.cv(&["prune", "alphasess", "--thinking"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--drop-thinking"), "{err}");
}

// ───────────────────────────── export ─────────────────────────────

#[test]
fn export_formats() {
    let w = World::new("export");
    standard_corpus(&w);

    // Markdown: header metadata + role sections + content.
    let (out, _) = w.cv_ok(&["export", "alphasess"]);
    assert!(out.contains("# alpha adventures"), "{out}");
    assert!(out.contains("- harness: claude"), "{out}");
    assert!(out.contains("- id: alphasess"), "{out}");
    assert!(out.contains("## User"), "{out}");
    assert!(out.contains("zebrafish migration"), "{out}");
    assert!(out.contains("**🔧 Edit**"), "{out}");

    // JSON: the full IR round-trips through serde — and `export` is a DOCUMENT boundary, so what
    // it writes is an OpenSession 0.3 document: the IR led by the version marker.
    let (out, _) = w.cv_ok(&["export", "alphasess", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["open_session"], "0.3", "{out}");
    assert!(
        out.trim_start().starts_with("{\n  \"open_session\""),
        "the marker leads the document:\n{out}"
    );
    assert_eq!(v["id"], "alphasess", "{out}");
    assert_eq!(v["harness"], "claude", "{out}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 3, "{out}");

    // `show --json` is the RAW IR: the marker belongs to a document, never to a Session.
    let (out, _) = w.cv_ok(&["show", "alphasess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert!(v.get("open_session").is_none(), "{out}");
    assert_eq!(v["id"], "alphasess", "{out}");

    // HTML: self-contained document with the transcript in it.
    let (out, _) = w.cv_ok(&["export", "alphasess", "--format", "html"]);
    assert!(out.contains("<html"), "{out}");
    assert!(out.contains("zebrafish"), "{out}");

    // Unknown format is an error, not a silent default.
    let (_, err) = w.cv_fails(&["export", "alphasess", "--format", "docx"]);
    assert!(err.contains("unknown format"), "{err}");
}

// ───────────────────────────── prune --json ─────────────────────────────

#[test]
fn prune_json_emits_machine_readable_report() {
    let w = World::new("prunejson");
    let big = "x".repeat(4096);
    // A realistic Claude fixture: the in-file sessionId matches the filename (prune reads the
    // source id from the lines, not the path).
    w.write_session(
        "prunesrc",
        &[
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "prunesrc", "timestamp": "2026-01-01T10:00:00Z",
                "cwd": "/work/proj",
                "message": {"role": "user", "content": "read the big file"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "prunesrc", "timestamp": "2026-01-01T10:01:00Z",
                "cwd": "/work/proj",
                "message": {"role": "assistant", "model": "claude-test-1", "content": [
                    {"type": "text", "text": "reading"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big.txt"}}
                ]}
            }),
            serde_json::json!({
                "type": "user", "uuid": "u2", "sessionId": "prunesrc", "timestamp": "2026-01-01T10:02:00Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": big, "is_error": false}
                ]}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a2", "sessionId": "prunesrc", "timestamp": "2026-01-01T10:03:00Z",
                "cwd": "/work/proj",
                "message": {"role": "assistant", "model": "claude-test-1", "content": [
                    {"type": "text", "text": "done"}
                ]}
            }),
        ],
    );
    let dir = w.home.join(".claude/projects/-work-proj");
    let source_before = fs::read_to_string(dir.join("prunesrc.jsonl")).unwrap();

    // Dry run first: stdout is ONE JSON object; nothing written means honest nulls + a note,
    // and the fixture dir is untouched.
    let (out, _) = w.cv_ok(&[
        "prune",
        "prunesrc",
        "--min-size",
        "100",
        "--keep-last",
        "0",
        "--dry-run",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    assert_eq!(v["source_id"], "prunesrc", "{out}");
    assert_eq!(v["dry_run"], true, "{out}");
    assert!(v["new_id"].is_null(), "unpinned dry-run id must be null:\n{out}");
    assert!(v["new_path"].is_null(), "{out}");
    assert!(v["sidecar_path"].is_null(), "{out}");
    assert!(v["note"].as_str().unwrap().contains("dry run"), "{out}");
    assert_eq!(v["snipped_payloads"], 1, "{out}");
    assert!(
        v["before_bytes"].as_u64().unwrap() > v["after_bytes"].as_u64().unwrap(),
        "{out}"
    );
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "dry run must write nothing");

    // A dry run with --to CAN report the id — it's caller-chosen, not minted.
    let (out, _) = w.cv_ok(&[
        "prune",
        "prunesrc",
        "--min-size",
        "100",
        "--keep-last",
        "0",
        "--dry-run",
        "--to",
        "pinnedsess",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["new_id"], "pinnedsess", "{out}");
    assert!(v["new_path"].is_null(), "still nothing written:\n{out}");

    // Real prune: full ids + real paths in the JSON; the files exist; the source is untouched.
    let (out, _) = w.cv_ok(&[
        "prune",
        "prunesrc",
        "--min-size",
        "100",
        "--keep-last",
        "0",
        "--to",
        "prunedsess",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    assert_eq!(v["source_id"], "prunesrc", "{out}");
    assert_eq!(v["new_id"], "prunedsess", "{out}");
    assert_eq!(v["harness"], "claude", "{out}");
    assert_eq!(v["dry_run"], false, "{out}");
    assert!(v["note"].is_null(), "{out}");
    assert_eq!(v["snipped_payloads"], 1, "{out}");
    assert!(v["tokens_freed"].as_u64().unwrap() > 0, "{out}");
    let new_path = PathBuf::from(v["new_path"].as_str().unwrap());
    assert!(new_path.ends_with("prunedsess.jsonl"), "{out}");
    assert!(new_path.exists(), "pruned session must exist at new_path");
    let sidecar = PathBuf::from(v["sidecar_path"].as_str().unwrap());
    assert!(sidecar.ends_with("prunedsess.flat.jsonl"), "{out}");
    assert!(sidecar.exists(), "sidecar must exist at sidecar_path");
    assert_eq!(
        fs::read_to_string(dir.join("prunesrc.jsonl")).unwrap(),
        source_before,
        "prune must never modify the source session"
    );

    // Without --json the report stays off stdout entirely (status goes to stderr).
    let (out, err) = w.cv_ok(&[
        "prune",
        "prunesrc",
        "--min-size",
        "100",
        "--keep-last",
        "0",
        "--dry-run",
    ]);
    assert!(out.trim().is_empty(), "non-json prune must keep stdout empty:\n{out}");
    assert!(err.contains("pruned"), "{err}");
}

// ───────────────────────────── rewind ─────────────────────────────

/// A keeper session (two compactions) that committed `63cef47…` mid-way, plus one sub-agent that
/// committed `9f00ba5…` — the shapes `cv rewind` exists for.
fn keeper_corpus(w: &World) -> PathBuf {
    let sid = "aaaaaaaa-0000-4000-8000-000000000001";
    let line = |ty: &str, uuid: &str, parent: Option<&str>, message: serde_json::Value| {
        serde_json::json!({"type": ty, "sessionId": sid, "uuid": uuid, "parentUuid": parent,
            "isSidechain": false, "cwd": "/work/proj", "timestamp": "2026-06-01T10:00:00Z", "message": message})
    };
    let boundary = |uuid: &str| {
        serde_json::json!({"type": "system", "subtype": "compact_boundary", "sessionId": sid, "uuid": uuid,
            "parentUuid": null, "isSidechain": false, "compactMetadata": {"trigger": "auto"}})
    };
    let path = w.write_session(
        sid,
        &[
            line("user", "u0", None, serde_json::json!({"role": "user", "content": "fix the gate"})),
            boundary("b1"),
            line("assistant", "a1", Some("b1"), serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_c", "name": "Bash",
                 "input": {"command": "git commit -am 'gate: name the project'"}}]})),
            line("user", "r1", Some("a1"), serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_c", "content": "[main 63cef47] gate: name the project"}]})),
            line("assistant", "a2", Some("r1"), serde_json::json!({"role": "assistant", "content": [
                {"type": "text", "text": "landed"}]})),
            boundary("b2"),
            line("user", "u3", Some("b2"), serde_json::json!({"role": "user", "content": "now something else"})),
        ],
    );
    let agent = w
        .home
        .join(".claude/projects/-work-proj")
        .join(sid)
        .join("subagents/agent-a7f5742c50c0b9654.jsonl");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let side = |uuid: &str, parent: Option<&str>, ty: &str, message: serde_json::Value| {
        serde_json::json!({"type": ty, "sessionId": sid, "uuid": uuid, "parentUuid": parent,
            "isSidechain": true, "agentId": "a7f5742c50c0b9654", "cwd": "/work/proj",
            "timestamp": "2026-06-01T11:00:00Z", "message": message})
    };
    let lines = [
        side(
            "s0",
            None,
            "user",
            serde_json::json!({"role": "user", "content": "review lane"}),
        ),
        side(
            "s1",
            Some("s0"),
            "assistant",
            serde_json::json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_s", "name": "Bash",
             "input": {"command": "git commit -qam fix && git rev-parse HEAD"}}]}),
        ),
        side(
            "s2",
            Some("s1"),
            "user",
            serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_s", "content": "9f00ba5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e"}]}),
        ),
        side(
            "s3",
            Some("s2"),
            "assistant",
            serde_json::json!({"role": "assistant", "content": [
            {"type": "text", "text": "done"}]}),
        ),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&agent, body).unwrap();
    path
}

fn jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn rewind_derives_the_agent_as_of_a_commit() {
    let w = World::new("rewind");
    let src = keeper_corpus(&w);
    let before = fs::read(&src).unwrap();
    let out = w.base.join("rw");
    let out_s = out.to_str().unwrap();

    // By commit sha: the cut is the tool result that printed it; the window opens at b1.
    let (stdout, err) = w.cv_ok(&[
        "rewind",
        "aaaaaaaa",
        "--at",
        "63cef473b771e07a",
        "--out",
        out_s,
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert_eq!(v["source_id"], "aaaaaaaa-0000-4000-8000-000000000001", "{stdout}");
    assert_eq!(
        (v["start_line"].as_u64(), v["cut_line"].as_u64()),
        (Some(2), Some(4)),
        "{stdout}"
    );
    assert_eq!(v["evidence"]["kind"], "created", "{stdout}");
    assert_eq!(v["omitted_lines"], 3, "{stdout}");
    let new_id = v["new_id"].as_str().unwrap().to_string();
    let new_path = PathBuf::from(v["new_path"].as_str().unwrap());
    assert_eq!(new_path, out.join(format!("{new_id}.jsonl")));
    let got = jsonl(&new_path);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|r| r["sessionId"] == new_id.as_str()), "{got:?}");
    assert!(PathBuf::from(v["provenance_path"].as_str().unwrap()).exists());
    // The resume incantation is `cv resume`'s rendering for the NEW id, in the cut's cwd.
    assert_eq!(
        v["resume"],
        serde_json::json!(["cd /work/proj", format!("claude --resume {new_id}")])
    );
    assert!(err.contains("exact: commit created here"), "{err}");
    assert!(
        err.contains("copy it there first"),
        "an --out elsewhere must say where claude looks:\n{err}"
    );
    assert_eq!(fs::read(&src).unwrap(), before, "rewind must never modify the source");

    // By message index, written to the project dir by default (where `claude --resume` looks).
    let (stdout, _) = w.cv_ok(&["rewind", "aaaaaaaa", "--at", "1", "--to", "rw-by-idx", "--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["cut_msg_idx"], 1, "{stdout}");
    assert!(w.home.join(".claude/projects/-work-proj/rw-by-idx.jsonl").exists());
    assert!(w
        .home
        .join(".claude/projects/-work-proj/rw-by-idx.rewind.json")
        .exists());

    // Dry run: nothing written, honest nulls.
    let (stdout, _) = w.cv_ok(&["rewind", "aaaaaaaa", "--full", "--dry-run", "--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        (v["dry_run"].as_bool(), v["start_line"].as_u64()),
        (Some(true), Some(1)),
        "{stdout}"
    );
    assert!(v["new_id"].is_null() && v["new_path"].is_null(), "{stdout}");

    // A sha no tool result shows: exit 1 with the reason. A bad --at: exit 2.
    let (_, err) = w.cv_fails(&["rewind", "aaaaaaaa", "--at", "deadbeef1"]);
    assert!(err.contains("deadbeef1") && err.contains("git commit"), "{err}");
    let (ok, code, _, err) = w.cv(&["rewind", "aaaaaaaa", "--at", "nope"]);
    assert!(!ok && code == 2, "{err}");
    let (ok, code, _, err) = w.cv(&["rewind", "aaaaaaaa", "--at", "99"]);
    assert!(!ok && code == 1 && err.contains("message(s)"), "{err}");
}

#[test]
fn rewind_extracts_a_subagent_as_a_standalone_session() {
    let w = World::new("rewindsub");
    keeper_corpus(&w);
    let proj = w.home.join(".claude/projects/-work-proj");

    // `agent-<id>` resolves fleet-wide (as `cv show agent-…` does); the sub-agent's own commit is
    // exact evidence; the session lands in the PARENT's project dir.
    let (stdout, err) = w.cv_ok(&[
        "rewind",
        "agent-a7f5742c",
        "--at",
        "9f00ba5",
        "--to",
        "kept-sub",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}\n{err}"));
    assert_eq!(v["subagent"], true, "{stdout}");
    assert_eq!(
        v["source_session_id"], "aaaaaaaa-0000-4000-8000-000000000001",
        "{stdout}"
    );
    assert_eq!(v["cut_line"], 3, "{stdout}");
    let got = jsonl(&proj.join("kept-sub.jsonl"));
    assert_eq!(got.len(), 3);
    assert!(got
        .iter()
        .all(|r| r["isSidechain"] == false && r.get("agentId").is_none() && r["sessionId"] == "kept-sub"));
    assert!(got[0]["parentUuid"].is_null(), "the root chain starts at a null parent");
    assert!(err.contains("sub-agent of aaaaaaaa"), "{err}");

    // `<parent> --agent <id>` and a plain transcript path reach the same transcript.
    let (stdout, _) = w.cv_ok(&["rewind", "aaaaaaaa", "--agent", "a7f57", "--dry-run", "--json"]);
    assert!(stdout.contains("\"subagent\": true"), "{stdout}");
    let path = proj.join("aaaaaaaa-0000-4000-8000-000000000001/subagents/agent-a7f5742c50c0b9654.jsonl");
    let (stdout, _) = w.cv_ok(&["rewind", path.to_str().unwrap(), "--dry-run", "--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        (v["subagent"].as_bool(), v["lines_written"].as_u64()),
        (Some(true), Some(4)),
        "{stdout}"
    );

    // The derived session is an ordinary top-level session to the rest of cv.
    let (out, _) = w.cv_ok(&["show", "kept-sub"]);
    assert!(out.contains("review lane"), "{out}");
}

// ───────────────────────────── dataset ─────────────────────────────

#[test]
fn dataset_emits_parseable_jsonl() {
    let w = World::new("dataset");
    standard_corpus(&w);

    let (out, err) = w.cv_ok(&["dataset"]);
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "one record per session:\nstdout:\n{out}\nstderr:\n{err}"
    );
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("each record is JSON");
        assert!(v["messages"].is_array(), "chatml shape: {line}");
    }
    assert!(err.contains("wrote 2 record(s)"), "{err}");

    // sharegpt shape + redaction scrubs the planted secret from the dataset too.
    let (out, _) = w.cv_ok(&["dataset", "--format", "sharegpt", "--redact"]);
    assert!(!out.contains(SECRET), "dataset --redact leaked the secret:\n{out}");
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert!(v["conversations"].is_array(), "sharegpt shape: {line}");
    }

    let (_, err) = w.cv_fails(&["dataset", "--format", "parquet"]);
    assert!(err.contains("unknown format"), "{err}");
}

// ───────────────────────────── events / touched ─────────────────────────────

#[test]
fn events_and_touched() {
    let w = World::new("events");
    standard_corpus(&w);

    // `cv events` ingests on the spot (no prior `cv index` needed).
    let (out, _) = w.cv_ok(&["events", "alphasess"]);
    assert!(out.contains("event(s) in claude:alphases"), "{out}");
    assert!(out.contains("file_edit"), "{out}");
    assert!(out.contains("/work/proj/src/widget.rs"), "{out}");

    // Kind filter: no command events in this session — friendly empty message, exit 0.
    let (out, _) = w.cv_ok(&["events", "alphasess", "--kind", "command"]);
    assert!(out.contains("no \"command\" events"), "{out}");

    // A chat-only session has no tool events at all.
    let (out, _) = w.cv_ok(&["events", "betasess"]);
    assert!(out.contains("no tool events"), "{out}");

    // touched: absolute path and repo-relative suffix both match; --edits-only keeps it.
    let (out, _) = w.cv_ok(&["touched", "/work/proj/src/widget.rs"]);
    assert!(out.contains("1 session(s) touched"), "{out}");
    assert!(out.contains("alphases"), "{out}");
    let (out, _) = w.cv_ok(&["touched", "src/widget.rs", "--edits-only"]);
    assert!(out.contains("alphases"), "{out}");
    assert!(out.contains("edit(s)"), "{out}");

    // An untouched path: explicit empty-result hint, exit 0.
    let (out, _) = w.cv_ok(&["touched", "src/nonexistent.rs"]);
    assert!(out.contains("no sessions touched"), "{out}");

    // Unknown session id is a real error.
    let (_, err) = w.cv_fails(&["events", "bogus-id"]);
    assert!(err.contains("no session matching"), "{err}");
}

// ───────────────────────────── redact ─────────────────────────────

#[test]
fn redact_scrubs_planted_secret() {
    let w = World::new("redact");
    standard_corpus(&w);

    // The secret IS in the raw transcript (sanity), and `cv show` prints it.
    let (out, _) = w.cv_ok(&["show", "alphasess"]);
    assert!(out.contains(SECRET), "fixture sanity: secret present in show:\n{out}");

    // redact md: gone from stdout, replaced by a placeholder; --stats reports it on stderr.
    let (out, err) = w.cv_ok(&["redact", "alphasess", "--stats"]);
    assert!(!out.contains(SECRET), "redact leaked the secret:\n{out}");
    assert!(out.contains("[REDACTED:api_key]"), "{out}");
    assert!(err.contains("redacted"), "{err}");
    assert!(err.contains("1 api_key"), "{err}");

    // redact json: the whole serialized session is clean.
    let (out, _) = w.cv_ok(&["redact", "alphasess", "--format", "json"]);
    assert!(!out.contains(SECRET), "redact --format json leaked the secret:\n{out}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["id"], "alphasess");

    // Unknown format is an error.
    let (_, err) = w.cv_fails(&["redact", "alphasess", "--format", "pdf"]);
    assert!(err.contains("unknown format"), "{err}");
}

// ───────────────────────────── error-path honesty ─────────────────────────────

#[test]
fn error_paths_exit_nonzero_with_stderr() {
    let w = World::new("errs");
    standard_corpus(&w);

    // `show` tries the id as a sub-agent too before giving up — its message says so.
    let (_, err) = w.cv_fails(&["show", "nonexistent-id"]);
    assert!(err.contains("no session (and no sub-agent) matching"), "{err}");

    let (_, err) = w.cv_fails(&["export", "nonexistent-id"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["tree", "nonexistent-id"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["resume", "nonexistent-id"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["diff", "alphasess", "nonexistent-id"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["show", "alphasess", "--harness", "marsrover"]);
    assert!(err.contains("unknown harness"), "{err}");

    // Every 0.10 name that 0.11.0 removed or renamed: gone, but never silently — each one exits 2
    // (a caller mistake, not a failure) with a line naming the version and the replacement.
    let removed: &[(&[&str], &str)] = &[
        // `convert` folded into `port`.
        (
            &["convert", "alphasess", "--to", "marsrover"],
            "cv port <id> --harness <harness>",
        ),
        // `query` only ever printed the field reference; that is `schema`.
        (&["query"], "cv schema"),
        // `recall` superseded by the one build-context verb. (`distill` was a 0.11 stub that pointed
        // here too; the name now means `cv distill`, the transcript reshaper.)
        (&["recall", "zebrafish"], "cv pack <task>"),
        // Fetching a tool's output is a Read, not a prune option.
        (
            &["prune", "alphasess", "--retrieve", "t1"],
            "cv cat <session> <tool_use_id>",
        ),
        // `--to`/`--to-dir` say where; the flags now say what.
        (&["port", "alphasess", "--to", "codex"], "use `--harness <harness>`"),
        (&["port", "alphasess", "--to-dir", "/tmp/nope"], "use `--cwd <dir>`"),
        // `prune --thinking` was a boolean "snip reasoning too"; `--thinking <mode>` now names what
        // an EMIT does with reasoning, and one word may not mean two things.
        (&["prune", "alphasess", "--thinking"], "use `--drop-thinking`"),
    ];
    for (args, pointer) in removed {
        let (ok, code, out, err) = w.cv(args);
        assert!(!ok, "cv {args:?} must not still work\nstdout:\n{out}");
        assert_eq!(code, 2, "cv {args:?} must exit 2 (caller mistake)\nstderr:\n{err}");
        assert!(!err.contains("panicked"), "cv {args:?} panicked:\n{err}");
        assert!(err.contains("0.11.0"), "cv {args:?} must say when it went away:\n{err}");
        assert!(err.contains(pointer), "cv {args:?} must point at {pointer:?}:\n{err}");
    }
    // The pointers are real: the replacement parses (it fails on the harness, not the grammar).
    let (_, err) = w.cv_fails(&["port", "alphasess", "--harness", "marsrover"]);
    assert!(err.contains("unknown"), "{err}");

    // A splice spec carries the same `A..B` window grammar as `--range`, split off the END of the
    // id (so a `harness:id` prefix still works): the 0.10 `<id>:A-B` points at its replacement,
    // and a non-numeric window is a bad spec, not a mystery id.
    let (_, err) = w.cv_fails(&["splice", "alphasess:1-2"]);
    assert!(err.contains("bad spec") && err.contains("<id>:A..B"), "{err}");
    let (_, err) = w.cv_fails(&["splice", "alphasess:zz..3"]);
    assert!(err.contains("bad spec"), "{err}");

    // An empty id matches every session: the ambiguity is named, not silently picked, and the
    // candidates are listed as the `harness:full-id` you can paste straight back in (§2).
    let (ok, code, out, err) = w.cv(&["events", ""]);
    assert!(!ok, "an ambiguous id must not be silently resolved:\n{out}");
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("ambiguous id"), "{err}");
    assert!(err.contains("2 candidates"), "{err}");
    assert!(
        err.contains("claude:alphasess") && err.contains("claude:betasess"),
        "candidates must be listed as harness:full-id:\n{err}"
    );
    // …and pasting one back in resolves it.
    w.cv_ok(&["events", "claude:alphasess"]);
}

// ───────────────────────────── blame on a non-file ─────────────────────────────

#[test]
fn blame_outside_repo_and_missing_file() {
    let w = World::new("blame");
    standard_corpus(&w);

    // A path that exists nowhere, outside any git repo: must not panic. Either outcome (clean
    // degradation to catalog-only mode, or a clear error) is fine — but never a panic/signal.
    let (_ok, code, out, err) = w.cv(&["blame", "/not/a/file"]);
    assert_ne!(code, -1, "cv blame died by signal\nstderr:\n{err}");
    assert!(!err.contains("panicked"), "panic in blame:\n{err}");
    assert!(
        out.contains("not inside a git repository") || !err.trim().is_empty(),
        "blame must explain itself\nstdout:\n{out}\nstderr:\n{err}"
    );
}

// ───────────────────────────── diff / tree happy paths ─────────────────────────────

#[test]
fn diff_and_tree_render() {
    let w = World::new("difftree");
    standard_corpus(&w);

    let (out, _) = w.cv_ok(&["diff", "alphasess", "betasess"]);
    assert!(out.contains("only-in-A"), "{out}");
    assert!(out.contains("0 shared"), "different sessions share no prefix:\n{out}");

    let (out, _) = w.cv_ok(&["tree", "alphasess"]);
    assert!(out.contains("# alpha adventures"), "{out}");
    assert!(out.contains("3 msg"), "{out}");
}

// ───────────────────────────── stats / timeline ─────────────────────────────

#[test]
fn stats_and_timeline() {
    let w = World::new("stats");
    standard_corpus(&w);

    let (out, _) = w.cv_ok(&["stats"]);
    assert!(out.contains("2 session(s) · 7 message(s)"), "{out}");
    assert!(out.contains("claude"), "{out}");

    let (out, _) = w.cv_ok(&["timeline"]);
    assert!(out.contains("2 session(s)"), "{out}");
    // Oldest → newest: beta's last-activity day precedes alpha's.
    assert!(pos(&out, "betasess") < pos(&out, "alphases"), "{out}");
}

#[test]
fn stats_tokens_totals_usage_per_model() {
    let w = World::new("stats-tokens");
    let reply = |uuid: &str, id: &str, model: &str| {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": "2026-03-01T09:05:00Z",
            "message": {"id": id, "role": "assistant", "model": model,
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 5, "cache_read_input_tokens": 2_000_000,
                          "cache_creation_input_tokens": 1_000, "output_tokens": 300}}
        })
    };
    w.write_session(
        "toksess",
        &[
            user_line("u1", "2026-03-01T09:00:00Z", "go"),
            reply("a1", "msg_1", "claude-opus-5-5"),
            // The same streamed response's second content-block line: one call, not two.
            reply("a2", "msg_1", "claude-opus-5-5"),
            reply("a3", "msg_2", "claude-fable-5-1"),
        ],
    );

    let (out, _) = w.cv_ok(&["stats", "--tokens"]);
    assert!(out.contains("1 repeated usage record(s) skipped"), "{out}");
    assert!(out.contains("claude-opus-5-5"), "{out}");
    assert!(out.contains("claude-fable-5-1"), "{out}");
    // 2 calls × (5 + 2,000,000 + 1,000 + 300) = 4,002,610.
    assert!(out.contains("4.0M"), "{out}");

    let (json, _) = w.cv_ok(&["stats", "--tokens", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["tokens"]["total"]["calls"], 2);
    assert_eq!(v["tokens"]["total"]["total"], 4_002_610);
    assert_eq!(v["tokens"]["total"]["uncached"], 2_610);
    assert_eq!(v["tokens"]["duplicates"], 1);

    let (json, _) = w.cv_ok(&["stats", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        v.get("tokens").is_none(),
        "without --tokens the payload stays catalog-only"
    );
}

/// Resolution by unique id prefix works across commands (find() contract the CLI leans on).
#[test]
fn id_prefix_resolution() {
    let w = World::new("prefix");
    standard_corpus(&w);

    let (out, _) = w.cv_ok(&["show", "alpha"]);
    assert!(out.contains("# alpha adventures"), "{out}");

    let (out, _) = w.cv_ok(&["show", "beta", "--harness", "claude"]);
    assert!(out.contains("quokkas"), "{out}");
}

// ───────────────────────────── extra claude roots ─────────────────────────────

/// Claude Code sessions outside `~/.claude` (a second `CLAUDE_CONFIG_DIR`, e.g. one per agent
/// seat) are listed, searched and read when named by `$CLUSTERVISION_HOME/claude-roots` (with a
/// `*` seat wildcard) or `$CLUSTERVISION_CLAUDE_ROOTS`; the default root keeps working beside them.
#[test]
fn extra_claude_roots_are_listed_and_read() {
    let w = World::new("roots");
    standard_corpus(&w);
    let seat_proj = w.home.join("seats/bonsai/claude/projects/-work-proj");
    fs::create_dir_all(&seat_proj).unwrap();
    let seat_line = user_line("s1", "2026-07-01T10:00:00Z", "the seat asks about narwhals");
    fs::write(seat_proj.join("seatsess.jsonl"), format!("{seat_line}\n")).unwrap();
    let env_proj = w.base.join("elsewhere/projects/-work-proj");
    fs::create_dir_all(&env_proj).unwrap();
    let env_line = user_line("e1", "2026-07-02T10:00:00Z", "the env root asks about axolotls");
    fs::write(env_proj.join("envsess.jsonl"), format!("{env_line}\n")).unwrap();

    // Not configured yet: the seat session is invisible, exactly as before.
    w.cv_fails(&["show", "seatsess"]);

    fs::write(w.cv_home.join("claude-roots"), "# agent seats\n~/seats/*/claude\n").unwrap();
    let (out, _) = w.cv_ok(&["show", "seatsess"]);
    assert!(out.contains("narwhals"), "{out}");
    let (out, _) = w.cv_ok(&["show", "alphasess"]);
    assert!(out.contains("zebrafish"), "the default root still works:\n{out}");
    let (out, _) = w.cv_ok(&["ls"]);
    assert!(out.contains("3 session(s)"), "{out}");
    let (out, _) = w.cv_ok(&["search", "narwhals"]);
    assert!(out.contains("seatsess"), "{out}");

    let roots = w.base.join("elsewhere");
    let (ok, _, out, err) = w.cv_env(
        &["show", "envsess"],
        &[("CLUSTERVISION_CLAUDE_ROOTS", roots.to_str().unwrap())],
    );
    assert!(ok, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("axolotls"), "{out}");
}

// ───────────────────────────── task substrate: identity + sanitizing ─────────────────────────────

/// Extract the task id `cv task open` prints on its own line (the uuid-shaped one).
fn opened_task_id(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.len() == 36 && l.chars().filter(|c| *c == '-').count() == 4)
        .unwrap_or_else(|| panic!("no task id in open output:\n{stdout}"))
        .to_string()
}

/// G4: identity is environment, not assertion — CV_ENDPOINT is the default `--from`, an explicit
/// flag still wins, and an identity-bearing verb with no resolvable identity is a hard error.
#[test]
fn task_identity_resolution_order() {
    let w = World::new("task-ident");

    // Open needs no ceremony: bare command records "cv".
    let (out, _) = w.cv_ok(&["task", "open", "first task"]);
    let id = opened_task_id(&out);
    let (json, _) = w.cv_ok(&["task", "show", &id, "--json"]);
    assert!(json.contains(r#""opened_by": "cv""#), "{json}");

    // Identity-bearing verb without CV_ENDPOINT or --from: refuse, name the cure.
    let (ok, _, _, err) = w.cv_env(&["task", "claim", &id], &[]);
    assert!(!ok, "claim without identity must fail");
    assert!(
        err.contains("set CV_ENDPOINT or pass --from"),
        "error names the cure:\n{err}"
    );

    // CV_ENDPOINT alone resolves it.
    let (ok, code, out, err) = w.cv_env(&["task", "claim", &id], &[("CV_ENDPOINT", "agent:demo")]);
    assert!(ok, "claim with CV_ENDPOINT: exit {code}\n{out}\n{err}");
    let (json, _) = w.cv_ok(&["task", "show", &id, "--json"]);
    assert!(json.contains(r#""assignee": "agent:demo""#), "{json}");

    // Explicit --from beats the env var.
    let (ok, ..) = w.cv_env(
        &["task", "release", &id, "--from", "agent:explicit"],
        &[("CV_ENDPOINT", "agent:demo")],
    );
    assert!(ok);
    let (json, _) = w.cv_ok(&["task", "show", &id, "--json", "--events"]);
    assert!(
        json.contains(r#""by": "agent:explicit""#),
        "explicit --from wins:\n{json}"
    );

    // Whitespace-only CV_ENDPOINT is no identity.
    let (ok, _, _, err) = w.cv_env(&["task", "claim", &id], &[("CV_ENDPOINT", "   ")]);
    assert!(!ok && err.contains("CV_ENDPOINT"), "{err}");
}

/// G5 + G8: an ANSI-bomb title renders stripped on every task surface, and list/inbox rows carry
/// an age column (inbox marks >24h rows with a leading marker — not testable with a fresh log,
/// covered in unit tests; here we prove the column exists).
#[test]
fn task_surfaces_sanitize_and_age() {
    let w = World::new("task-ansi");

    let bomb = "evil\u{1b}]0;pwned\u{7}title \u{1b}[31mred\nline";
    let (out, _) = w.cv_ok(&["task", "open", bomb]);
    let id = opened_task_id(&out);

    let (out, _) = w.cv_ok(&["task", "list"]);
    assert!(!out.contains('\u{1b}'), "list leaks ESC:\n{out:?}");
    assert!(out.contains("eviltitle red line"), "stripped title renders:\n{out}");
    assert!(out.contains("0s") || out.contains("1s"), "age column present:\n{out}");

    let (out, _) = w.cv_ok(&["task", "show", &id]);
    assert!(!out.contains('\u{1b}'), "show leaks ESC:\n{out:?}");

    // Assign it so the inbox has a row, then check the inbox strips too.
    let (ok, ..) = w.cv_env(&["task", "claim", &id], &[("CV_ENDPOINT", "agent:demo")]);
    assert!(ok);
    let (out, _) = w.cv_ok(&["task", "inbox", "agent:demo"]);
    assert!(!out.contains('\u{1b}'), "inbox leaks ESC:\n{out:?}");
    assert!(out.contains("claimed work (1):"), "inbox groups by reason:\n{out}");

    let (out, _) = w.cv_ok(&["task", "debt"]);
    assert!(!out.contains('\u{1b}'), "debt leaks ESC:\n{out:?}");
}

/// Branch collision at propose is a WARNING on stderr, never a block: two live tasks proposing
/// one branch in one repo usually means two agents about to trample each other.
#[test]
fn task_propose_warns_on_branch_collision() {
    let w = World::new("task-collide");
    // A tiny real repo with one feature branch over main (propose observes git, so it's real).
    let repo = w.base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        // Hermetic git: no global/system config, so no 1Password commit-signing prompts/latency.
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    git(&["checkout", "-q", "-b", "task/shared"]);
    fs::write(repo.join("b.txt"), "b").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "feat"]);

    let repo_s = repo.to_str().unwrap();
    let (out, _) = w.cv_ok(&["task", "open", "first", "--repo", repo_s]);
    let t1 = opened_task_id(&out);
    let (out, _) = w.cv_ok(&["task", "open", "second", "--repo", repo_s]);
    let t2 = opened_task_id(&out);

    // First propose: nobody else carries the branch, no warning.
    let (ok, _, _, err) = w.cv_env(
        &["task", "propose", &t1, "--branch", "task/shared", "--upstream", "main"],
        &[("CV_ENDPOINT", "agent:a")],
    );
    assert!(ok, "{err}");
    assert!(!err.contains("collision"), "first propose must not warn:\n{err}");

    // Second task proposing the SAME branch: warned on stderr, exit still 0 (never a block).
    let (ok, code, out, err) = w.cv_env(
        &["task", "propose", &t2, "--branch", "task/shared", "--upstream", "main"],
        &[("CV_ENDPOINT", "agent:b")],
    );
    assert!(ok, "collision is a warning, not a block: exit {code}\n{out}\n{err}");
    assert!(
        err.contains("WARNING") && err.contains("already carried by task") && err.contains(&t1[..8]),
        "warning names the carrier:\n{err}"
    );
    assert!(err.contains("collision"), "{err}");
}

/// Reviewer receipts + fleet stats end-to-end: a pass with a reviewer session whose transcript
/// touched the change records receipts (rendered in `show`), a session-less pass warns and
/// records none, and `cv task stats` renders both endpoints' counts with the trust footer.
#[test]
fn task_receipts_and_stats_end_to_end() {
    let w = World::new("task-receipts");
    // A tiny real repo (propose observes git).
    let repo = w.base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    git(&["checkout", "-q", "-b", "task/receipts"]);
    fs::write(repo.join("b.txt"), "b").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "feat"]);
    let repo_s = repo.to_str().unwrap();

    // The reviewer's transcript: diffed the branch and ran the tests.
    w.write_session(
        "reviewsess",
        &[
            user_line("u1", "2026-07-16T10:00:00Z", "review task/receipts please"),
            assistant_line(
                "a1",
                "2026-07-16T10:01:00Z",
                serde_json::json!([
                    {"type": "tool_use", "id": "t1", "name": "Bash",
                     "input": {"command": "git diff main...task/receipts"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test --workspace"}},
                ]),
            ),
        ],
    );

    // Task 1: propose + pass WITH the reviewer session — receipts observed.
    let (out, _) = w.cv_ok(&["task", "open", "with receipts", "--repo", repo_s]);
    let t1 = opened_task_id(&out);
    let (ok, ..) = w.cv_env(&["task", "claim", &t1], &[("CV_ENDPOINT", "agent:author")]);
    assert!(ok);
    let (ok, _, _, err) = w.cv_env(
        &[
            "task",
            "propose",
            &t1,
            "--branch",
            "task/receipts",
            "--upstream",
            "main",
        ],
        &[("CV_ENDPOINT", "agent:author")],
    );
    assert!(ok, "{err}");
    let (ok, code, out, err) = w.cv_env(
        &[
            "task",
            "pass",
            &t1,
            "--session",
            "reviewsess",
            "--from",
            "agent:reviewer",
        ],
        &[],
    );
    assert!(ok, "pass with session exited {code}\n{out}\n{err}");
    assert!(
        !err.contains("no observable contact") && !err.contains("receipts not observed"),
        "contact was in the transcript, no receipts warning expected:\n{err}"
    );

    // The receipts render on `show`.
    let (out, _) = w.cv_ok(&["task", "show", &t1]);
    assert!(
        out.contains("receipts (pass): saw change ✓, ran checks ✓, 1 turns"),
        "receipts line missing:\n{out}"
    );

    // Task 2: pass WITHOUT a session — warned, recorded as none, still never blocked.
    let (out, _) = w.cv_ok(&["task", "open", "no receipts", "--repo", repo_s]);
    let t2 = opened_task_id(&out);
    let (ok, ..) = w.cv_env(&["task", "claim", &t2], &[("CV_ENDPOINT", "agent:author")]);
    assert!(ok);
    let (ok, _, _, err) = w.cv_env(
        &[
            "task",
            "propose",
            &t2,
            "--branch",
            "task/receipts",
            "--upstream",
            "main",
        ],
        &[("CV_ENDPOINT", "agent:author")],
    );
    assert!(ok, "{err}");
    let (ok, code, _, err) = w.cv_env(&["task", "pass", &t2, "--from", "agent:reviewer"], &[]);
    assert!(ok, "session-less pass must not be blocked: exit {code}\n{err}");
    assert!(
        err.contains("no reviewer session given: review receipts not observed"),
        "missing receipts warning:\n{err}"
    );
    let (out, _) = w.cv_ok(&["task", "show", &t2]);
    assert!(
        !out.contains("receipts (pass)"),
        "no receipts recorded → no line:\n{out}"
    );

    // Fleet stats: both endpoints appear with honest counts and the trust footer.
    let (out, err) = w.cv_ok(&["task", "stats"]);
    assert!(out.contains("agent:author"), "{out}");
    assert!(out.contains("agent:reviewer"), "{out}");
    assert!(
        out.contains("computed from observed events only; landed = git-verified"),
        "{out}"
    );
    assert!(
        out.contains("verified: NEVER"),
        "no verify pass ran in this world:\n{out}"
    );
    assert!(
        err.contains("NEVER been verified"),
        "freshness warning on stderr:\n{err}"
    );
    // The reviewer row counts the session-less pass as a no-receipts pass (rubber-stamp signal).
    let reviewer_row = out.lines().find(|l| l.contains("agent:reviewer")).unwrap();
    assert!(reviewer_row.contains("2/0"), "two passes, zero refutes: {reviewer_row}");

    // And the JSON surface carries the same rows machine-readably.
    let (json, _) = w.cv_ok(&["task", "stats", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let reviewers = v["reviewers"].as_array().unwrap();
    assert_eq!(reviewers.len(), 1);
    assert_eq!(reviewers[0]["no_receipts_passes"], 1);
    assert_eq!(reviewers[0]["no_contact_passes"], 0);
    assert_eq!(reviewers[0]["passes"], 2);
}

/// G5 on the board: message bodies and senders are stripped at `cv board read`.
#[test]
fn board_read_sanitizes_and_from_defaults_to_cv_endpoint() {
    let w = World::new("board-ansi");

    let (ok, ..) = w.cv_env(
        &["board", "post", "general", "hi \u{1b}[31mthere\u{1b}]0;x\u{7}!"],
        &[("CV_ENDPOINT", "agent:board\u{1b}[0m")],
    );
    assert!(ok);
    let (out, _) = w.cv_ok(&["board", "read", "general"]);
    assert!(!out.contains('\u{1b}'), "board read leaks ESC:\n{out:?}");
    assert!(out.contains("hi there!"), "{out}");
    assert!(out.contains("agent:board"), "CV_ENDPOINT is the default sender:\n{out}");
}

// ───────────────────────── cv task ergonomics (0.12.0) ─────────────────────────

/// `--body-file`, `--tags`/`list --tag`, `--blocked-by`/`--blocks` with the blocked marker and
/// `show`'s relation lines, `note --file`, `--tsv`, `--wide`.
#[test]
fn task_open_relations_tags_and_file_bodies() {
    let w = World::new("task-ergo");
    let brief = w.base.join("brief.md");
    fs::write(
        &brief,
        "# The brief\n\nA body that is a document, not a shell string.\n",
    )
    .unwrap();

    let (out, _) = w.cv_ok(&[
        "task",
        "open",
        "FOUNDATION: the thing others wait on",
        "--tags",
        "lean,deploy",
    ]);
    let a = opened_task_id(&out);
    let (out, _) = w.cv_ok(&[
        "task",
        "open",
        "DEPENDENT: needs the foundation",
        "--body-file",
        brief.to_str().unwrap(),
        "--blocked-by",
        &a,
        "--tags",
        "lean",
    ]);
    let b = opened_task_id(&out);
    let (out, _) = w.cv_ok(&["task", "open", "UPSTREAM: blocks the foundation", "--blocks", &a]);
    let c = opened_task_id(&out);

    // The body came from the file.
    let (json, _) = w.cv_ok(&["task", "show", &b, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(t["body"].as_str().unwrap().contains("not a shell string"), "{json}");
    assert_eq!(t["tags"], serde_json::json!(["lean"]), "{json}");
    assert_eq!(t["blocked_by"], serde_json::json!([a]), "{json}");
    // `--blocks` wrote the relation on the OTHER task.
    let (json, _) = w.cv_ok(&["task", "show", &a, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["blocked_by"], serde_json::json!([c]), "{json}");
    assert_eq!(t["tags"], serde_json::json!(["lean", "deploy"]), "{json}");

    // show: tags, blocked by (with the blocker's state and title), blocks (the reverse).
    let (out, _) = w.cv_ok(&["task", "show", &a]);
    assert!(out.contains("tags:     lean, deploy"), "{out}");
    assert!(
        out.contains("blocked by: ") && out.contains("[open] UPSTREAM: blocks the foundation"),
        "{out}"
    );
    assert!(out.contains("⊘ BLOCKED"), "{out}");
    assert!(
        out.contains("blocks:   ") && out.contains("DEPENDENT: needs the foundation"),
        "{out}"
    );

    // list: blocked rows carry the marker; --tag filters; ids are rendered at a unique length.
    let (out, _) = w.cv_ok(&["task", "list"]);
    let blocked_rows = |out: &str| {
        out.lines()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_hexdigit()) && l.contains("⊘ "))
            .count()
    };
    assert_eq!(blocked_rows(&out), 2, "A and B are blocked:\n{out}");
    assert!(out.contains("⊘ = blocked"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--tag", "deploy"]);
    assert!(out.contains("FOUNDATION") && !out.contains("DEPENDENT"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list"]);
    for line in out.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_hexdigit())) {
        let shown = line.split_whitespace().next().unwrap();
        let (ok, _, _, err) = w.cv(&["task", "show", shown]);
        assert!(ok, "the prefix `list` prints must resolve in `show`: {shown}\n{err}");
    }

    // Finishing the upstream task unblocks A without another event.
    w.cv_ok(&["task", "done", &c, "--observed", "merged"]);
    let (out, _) = w.cv_ok(&["task", "show", &a]);
    assert!(!out.contains("⊘ BLOCKED"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list"]);
    assert_eq!(blocked_rows(&out), 1, "only B stays blocked:\n{out}");

    // tsv: one line per task, seven cells, full ids, no alignment.
    let (out, _) = w.cv_ok(&["task", "list", "--tsv"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    let cells: Vec<&str> = out.lines().find(|l| l.starts_with(&b)).unwrap().split('\t').collect();
    assert_eq!(cells.len(), 7, "{cells:?}");
    assert_eq!(cells[1], "open");
    assert_eq!(cells[5], "DEPENDENT: needs the foundation");
    assert_eq!(cells[6], a);

    // wide: a second line with tags, blockers and the body's first line.
    let (out, _) = w.cv_ok(&["task", "list", "--wide"]);
    assert!(
        out.contains("#lean · blocked by ") && out.contains("[open] · # The brief"),
        "{out}"
    );

    // note --file.
    let note = w.base.join("note.txt");
    fs::write(&note, "progress from a file").unwrap();
    w.cv_ok(&["task", "note", &b, "--file", note.to_str().unwrap()]);
    let (out, _) = w.cv_ok(&["task", "show", &b]);
    assert!(out.contains("progress from a file"), "{out}");
    let (ok, ..) = w.cv(&["task", "note", &b]);
    assert!(!ok, "note needs text or --file");

    // tag / block verbs after the fact; a self-block is refused; an unknown blocker is refused.
    w.cv_ok(&["task", "tag", &b, "decision"]);
    let (json, _) = w.cv_ok(&["task", "show", &b, "--json"]);
    assert!(json.contains("\"decision\""), "{json}");
    let (ok, _, _, err) = w.cv(&["task", "block", &b, "--by", &b]);
    assert!(!ok && err.contains("cannot block itself"), "{err}");
    let (ok, _, _, err) = w.cv(&["task", "open", "typo", "--blocked-by", "ffffffff"]);
    assert!(
        !ok && err.contains("no task matches"),
        "relations resolve before the open:\n{err}"
    );
    let (out, _) = w.cv_ok(&["task", "list", "--all"]);
    assert!(!out.contains("typo"), "a refused open leaves nothing behind:\n{out}");
}

/// `inbox` groups by reason, decisions first: a `decision`-tagged task assigned to you is a
/// decision owed, not work.
#[test]
fn task_inbox_groups_decisions_first() {
    let w = World::new("task-inbox");
    let (out, _) = w.cv_ok(&[
        "task",
        "open",
        "pick the enrollment rate",
        "--assignee",
        "ember",
        "--tags",
        "decision",
    ]);
    let d = opened_task_id(&out);
    w.cv_ok(&["task", "open", "write the docs", "--assignee", "ember"]);
    let (out, _) = w.cv_ok(&["task", "open", "the claimed one"]);
    let c = opened_task_id(&out);
    let (ok, ..) = w.cv_env(&["task", "claim", &c], &[("CV_ENDPOINT", "ember")]);
    assert!(ok);

    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    let dec = out.find("decisions owed (1):").expect(&out);
    let assigned = out.find("assigned actions (1):").expect(&out);
    let claimed = out.find("claimed work (1):").expect(&out);
    assert!(dec < assigned && assigned < claimed, "group order:\n{out}");
    assert!(out.contains("pick the enrollment rate"), "{out}");
    let (json, _) = w.cv_ok(&["task", "inbox", "ember", "--json"]);
    assert!(json.contains("\"decision_owed\""), "{json}");
    assert!(json.contains(&d), "{json}");
    let (out, _) = w.cv_ok(&["task", "inbox", "nobody"]);
    assert!(out.contains("(inbox empty for nobody)"), "{out}");
}

/// `sweep` observes git and the filesystem and prints candidates; it closes nothing.
#[test]
fn task_sweep_names_merged_branches_and_missing_issue_paths_without_closing() {
    let w = World::new("task-sweep");
    let repo = w.base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("HOME", &w.home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    git(&["checkout", "-q", "-b", "k-ran"]);
    fs::write(repo.join("b.txt"), "b").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "work"]);
    git(&["checkout", "-q", "main"]);
    git(&["merge", "-q", "--no-ff", "-m", "merge", "k-ran"]);
    git(&["checkout", "-q", "-b", "p3b1"]);
    fs::write(repo.join("c.txt"), "c").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "unmerged work"]);
    git(&["checkout", "-q", "main"]);
    fs::write(repo.join("plan.md"), "plan").unwrap();

    let r = repo.to_str().unwrap();
    let (out, _) = w.cv_ok(&[
        "task",
        "open",
        "K-RAN: run claim",
        "--repo",
        r,
        "--body",
        "one commit on branch `k-ran`, not pushed",
    ]);
    let merged = opened_task_id(&out);
    w.cv_ok(&[
        "task",
        "open",
        "P3B1: receiver",
        "--repo",
        r,
        "--body",
        "work is on `p3b1`",
    ]);
    w.cv_ok(&["task", "open", "PLAN: keep", "--repo", r, "--issue", "plan.md"]);
    let (out, _) = w.cv_ok(&[
        "task",
        "open",
        "GONE: issue file deleted",
        "--repo",
        r,
        "--issue",
        "docs/gone.md",
    ]);
    let gone = opened_task_id(&out);
    w.cv_ok(&[
        "task",
        "open",
        "WORDS: a body that says fix and main",
        "--repo",
        r,
        "--body",
        "fix main",
    ]);
    w.cv_ok(&[
        "task",
        "open",
        "OTHER REPO: names k-ran but lives elsewhere",
        "--repo",
        w.base.to_str().unwrap(),
        "--body",
        "k-ran",
    ]);

    let (out, _) = w.cv_ok(&["task", "sweep", "--repo", r]);
    assert!(out.contains("2 candidate(s)"), "{out}");
    assert!(
        out.contains("K-RAN: run claim") && out.contains("names branch `k-ran`, merged into main"),
        "{out}"
    );
    assert!(
        out.contains("GONE: issue file deleted") && out.contains("issue path no longer exists"),
        "{out}"
    );
    assert!(
        !out.contains("P3B1") && !out.contains("PLAN:") && !out.contains("WORDS") && !out.contains("OTHER REPO"),
        "{out}"
    );
    assert!(out.contains("nothing was closed"), "{out}");
    // Nothing changed state.
    let (json, _) = w.cv_ok(&["task", "show", &merged, "--json"]);
    assert!(json.contains("\"state\": \"open\""), "{json}");
    let (json, _) = w.cv_ok(&["task", "sweep", "--repo", r, "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|x| x["task_id"] == gone));
}

// ───────────────────────────── decisions: decide / resolve / inbox / split ─────────────────────────────

/// Replace the volatile parts of a rendering (ids, local timestamps, ages) with placeholders so
/// a golden can be compared across machines and clocks.
fn normalize(s: &str) -> String {
    let s = regex::Regex::new(r"\b[0-9a-f]{8}-[0-9a-f]{1,4}(?:-[0-9a-f]{1,4}){0,3}(?:-[0-9a-f]{1,12})?\b")
        .unwrap()
        .replace_all(s, "<id>");
    let s = regex::Regex::new(r"\b\d{4}-\d\d-\d\d \d\d:\d\d\b")
        .unwrap()
        .replace_all(&s, "<ts>");
    let s = regex::Regex::new(r"\b\d\d-\d\d \d\d:\d\d\b")
        .unwrap()
        .replace_all(&s, "<ts>");
    regex::Regex::new(r"\b\d+[smhdw] ago\b")
        .unwrap()
        .replace_all(&s, "<age> ago")
        .into_owned()
}

/// decide → inbox (decisions first, default + options on one line) → resolve. The resolver is
/// recorded, never guessed: without an identity the error names the exact command for the
/// decision's owner. `done` is the wrong verb for a question; a choice outside the options is
/// refused naming them; a resolved decision is terminal and hidden from `list`.
#[test]
fn task_decide_resolve_round_trip() {
    let w = World::new("decide");
    let (out, _) = w.cv_ok(&[
        "task", "decide", "K-PORTAL: who births the guest cell",
        "--for", "ember",
        "--default", "the concierge births it",
        "--option", "the receiver allocates it",
        "--by", "3d",
        "--body", "default = the concierge; alternative = the receiver (then F pays).",
    ]);
    let d = opened_task_id(&out);
    assert!(out.contains("posed for ember"), "{out}");

    let (out, _) = w.cv_ok(&["task", "show", &d]);
    assert!(out.contains("]  decision"), "kind on the header line:\n{out}");
    assert!(out.contains("default:  the concierge births it"), "{out}");
    assert!(out.contains("option:   the receiver allocates it"), "{out}");
    assert!(out.contains("(in 2d)") || out.contains("(in 3d)"), "deadline phrase:\n{out}");
    assert!(out.contains("resolve:  cv task resolve") && out.contains("--from ember"), "{out}");
    assert!(out.contains("tags:     decision"), "{out}");
    let (json, _) = w.cv_ok(&["task", "show", &d, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["decision"]["default"], "the concierge births it");
    assert_eq!(t["decision"]["options"].as_array().unwrap().len(), 2);
    assert!(t["decision"]["deadline"].is_string());

    // inbox: the decision group, with its default and alternative on one line.
    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    assert!(out.starts_with("decisions owed (1):"), "{out}");
    assert!(out.contains("⇒ default: the concierge births it · alt: the receiver allocates it · by "), "{out}");
    assert!(out.contains("resolve: cv task resolve <id> --accept-default --from ember"), "{out}");
    let (json, _) = w.cv_ok(&["task", "inbox", "ember", "--json"]);
    assert!(json.contains("\"decision_owed\"") && json.contains("\"default\": \"the concierge births it\""), "{json}");

    // Wrong verb, wrong option, no identity.
    let (_, err) = w.cv_fails(&["task", "done", &d]);
    assert!(err.contains("answer it with resolve"), "{err}");
    let (_, err) = w.cv_fails(&["task", "resolve", &d, "--choice", "burn it", "--from", "ember"]);
    assert!(err.contains("not one of the posed options") && err.contains("--choice \"the receiver allocates it\""), "{err}");
    let (_, err) = w.cv_fails(&["task", "resolve", &d, "--choice", "THE RECEIVER allocates it", "--from", "ember"]);
    assert!(err.contains("did you mean --choice \"the receiver allocates it\""), "{err}");
    let (_, err) = w.cv_fails(&["task", "resolve", &d, "--accept-default"]);
    assert!(
        err.contains("records WHO decided") && err.contains("--accept-default --from ember") && err.contains("export CV_ENDPOINT=ember"),
        "the cure names the owner:\n{err}"
    );
    let (_, err) = w.cv_fails(&["task", "resolve", &d]);
    assert!(err.contains("--choice") || err.contains("required"), "{err}");

    // Resolve: by the owner, accepting the default.
    let (out, _) = w.cv_ok(&["task", "resolve", &d, "--accept-default", "--from", "ember"]);
    assert!(out.contains("resolved") && out.contains("→ the concierge births it (by ember)"), "{out}");
    let (out, _) = w.cv_ok(&["task", "show", &d]);
    assert!(out.contains("[resolved]"), "{out}");
    assert!(out.contains("resolved: the concierge births it — by ember,") && out.contains("(the default)"), "{out}");
    let (json, _) = w.cv_ok(&["task", "show", &d, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["state"], "resolved");
    assert_eq!(t["decision"]["resolution"]["accepted_default"], true);
    assert_eq!(t["decision"]["resolution"]["by"], "ember");
    // Terminal: gone from list and inbox, listable by state, not resolvable twice.
    let (out, _) = w.cv_ok(&["task", "list"]);
    assert!(!out.contains("K-PORTAL"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--state", "resolved"]);
    assert!(out.contains("K-PORTAL"), "{out}");
    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    assert!(out.contains("(inbox empty for ember)"), "{out}");
    let (_, err) = w.cv_fails(&["task", "resolve", &d, "--accept-default", "--from", "ember"]);
    assert!(err.contains("resolved") && err.contains("cannot apply"), "{err}");

    // A non-default choice with a note from stdin; CV_ENDPOINT as the resolver.
    let (out, _) = w.cv_ok(&["task", "decide", "Q2", "--for", "ember", "--default", "a", "--option", "b"]);
    let d2 = opened_task_id(&out);
    let (ok, _, _, err) = w.cv_full(&["task", "resolve", &d2, "--choice", "b", "--note", "-"], &[("CV_ENDPOINT", "ember")], Some("because b\n"));
    assert!(ok, "{err}");
    let (json, _) = w.cv_ok(&["task", "show", &d2, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["decision"]["resolution"]["choice"], "b");
    assert_eq!(t["decision"]["resolution"]["accepted_default"], false);
    assert_eq!(t["decision"]["resolution"]["note"], "because b");
    // `list --decisions` / `--actions` partition.
    w.cv_ok(&["task", "open", "plain work", "--assignee", "ember"]);
    let (out, _) = w.cv_ok(&["task", "list", "--all", "--decisions"]);
    assert!(out.contains("Q2") && !out.contains("plain work"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--actions"]);
    assert!(!out.contains("Q2") && out.contains("plain work"), "{out}");
}

/// `inbox` orders decisions, then assigned actions, then claimed work; `--md` renders the whole
/// inbox as one Markdown page (bodies, the first line of each note) — pinned as a normalized golden.
#[test]
fn task_inbox_orders_decisions_first_and_renders_markdown() {
    let w = World::new("inbox-md");
    let (out, _) = w.cv_ok(&["task", "open", "the claimed one", "--body", "claimed body"]);
    let c = opened_task_id(&out);
    assert!(w.cv_env(&["task", "claim", &c], &[("CV_ENDPOINT", "ember")]).0);
    let (out, _) = w.cv_ok(&["task", "open", "write the docs", "--assignee", "ember", "--body", "First line of the action.\n\nSecond paragraph."]);
    let a = opened_task_id(&out);
    w.cv_ok(&["task", "note", &a, "a note with two lines\nsecond line hidden in md", "--from", "orchestrator:x"]);
    let (out, _) = w.cv_ok(&[
        "task", "decide", "P6: WHO MAY REFILL a purse", "--for", "ember",
        "--default", "keep open", "--option", "per-refill consent",
        "--body", "today anyone may fund any purse.", "--from", "orchestrator:x",
    ]);
    let d = opened_task_id(&out);
    w.cv_ok(&["task", "open", "legacy decision by tag", "--assignee", "ember", "--tags", "decision"]);

    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    let dec = out.find("decisions owed (2):").expect(&out);
    let assigned = out.find("assigned actions (1):").expect(&out);
    let claimed = out.find("claimed work (1):").expect(&out);
    assert!(dec < assigned && assigned < claimed, "group order:\n{out}");
    assert!(out.find("P6: WHO MAY REFILL").unwrap() < out.find("legacy decision by tag").unwrap(), "{out}");

    let (md, _) = w.cv_ok(&["task", "inbox", "ember", "--md"]);
    let got = normalize(&md);
    let expected = "\
# Inbox for ember — <ts>

2 decision(s) owed · 1 assigned action(s) · 1 claimed · 0 review(s) · 0 unlanded · 3 unread

## Decisions owed (2)

### 1. P6: WHO MAY REFILL a purse

`<id>` · open · asked <age> ago by orchestrator:x · **unread** (last: orchestrator:x)

- **default:** keep open
- alternative: per-refill consent
- resolve: `cv task resolve <id> --accept-default --from ember`

today anyone may fund any purse.

### 2. legacy decision by tag

`<id>` · open · waiting <age> ago by cv · **unread** (last: cv)

## Assigned actions (1)

### 3. write the docs

`<id>` · open · waiting <age> ago by cv · **unread** (last: orchestrator:x)

First line of the action.

Second paragraph.

notes (1):

- _orchestrator:x, <ts>:_ a note with two lines

## Claimed work (1)

### 4. the claimed one

`<id>` · claimed · waiting <age> ago by cv

claimed body

";
    assert_eq!(got, expected, "--md golden (normalized):\n{md}");
    let _ = d;
}

/// `split` turns each leading-DECIDE note into its own decision (title from the label and the
/// question, default from the `Default =`/`if silent:`/`Recommend:` clause, options from
/// `alternative =`), assigned like the parent and blocking it; the notes stay and `show` points
/// each at its decision; a second split creates nothing; mid-text DECIDEs are reported, not split.
#[test]
fn task_split_turns_decide_notes_into_decisions() {
    let w = World::new("split");
    let (out, _) = w.cv_ok(&["task", "open", "EMBER: everything only ember can do", "--assignee", "ember", "--repo", w.base.to_str().unwrap()]);
    let parent = opened_task_id(&out);
    let notes = [
        "DECIDE: move the build base from /tank (slow) to NVMe? Default if silent: keep /tank.",
        "PUSH also ~/dev/cv: one unsigned commit.",
        "DECIDE (K-PORTAL, default stands if silent): who births a guest cell — default = the concierge births it; alternative = the receiver allocates it (then F pays).",
        "DECIDE (DEOS.md §5, defaults stand if silent; the doc has the full argument): (1) quotes: REFERENCED by default; (2) web face: LOCAL ONLY.",
        "JOIN-SOLANA works. DECIDE: one enrollment per tip, raise the floor or batch.",
    ];
    for n in notes {
        w.cv_ok(&["task", "note", &parent, n, "--from", "orchestrator:x"]);
    }
    let (out, _) = w.cv_ok(&["task", "split", &parent, "--dry-run"]);
    assert!(out.contains("# 3 DECIDE note(s)") && out.contains("(dry run: nothing written)"), "{out}");
    assert!(out.contains("1 other note(s) mention DECIDE mid-text"), "{out}");
    let (list, _) = w.cv_ok(&["task", "list"]);
    assert_eq!(list.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_hexdigit())).count(), 1, "dry run wrote nothing:\n{list}");

    let (out, _) = w.cv_ok(&["task", "split", &parent, "--from", "orchestrator:x"]);
    assert!(out.contains("3 decision(s) created, each blocking"), "{out}");
    assert!(out.contains("move the build base from /tank to NVMe?"), "title from the question, parens stripped:\n{out}");
    assert!(out.contains("default: keep /tank"), "{out}");
    assert!(out.contains("K-PORTAL: who births a guest cell"), "{out}");
    assert!(out.contains("option:  the receiver allocates it (then F pays)"), "{out}");
    assert!(out.contains("DEOS.md §5: quotes: REFERENCED by default") && out.contains("default: as proposed"), "{out}");

    // The inbox: three decisions first, then the parent as an assigned action.
    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    assert!(out.starts_with("decisions owed (3):"), "{out}");
    assert!(out.contains("assigned actions (1):"), "{out}");
    assert!(out.contains("⇒ default: the concierge births it · alt: the receiver allocates it (then F pays)"), "{out}");

    // Relations and provenance: the parent is blocked by each decision; each decision carries
    // the note it came from as its source; `show` on the parent points each note at its child.
    let (json, _) = w.cv_ok(&["task", "show", &parent, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["blocked_by"].as_array().unwrap().len(), 3, "{json}");
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--brief"]);
    assert!(out.contains("⊘ BLOCKED"), "{out}");
    assert_eq!(out.matches("→ split into ").count(), 3, "{out}");
    assert!(out.contains("notes:    5 of 5 (newest last) — one line each"), "{out}");
    let (list, _) = w.cv_ok(&["task", "list", "--decisions", "--json"]);
    let ds: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap();
    assert_eq!(ds.len(), 3);
    for d in &ds {
        assert!(d["decision"]["source"].is_string(), "{d}");
        assert_eq!(d["assignee"], "ember");
        assert_eq!(d["repo"], t["repo"]);
        assert!(d["tags"].as_array().unwrap().iter().any(|x| x == "split"), "{d}");
    }

    // Idempotent: nothing new on a second run.
    let (out, _) = w.cv_ok(&["task", "split", &parent]);
    assert!(out.contains("0 decision(s) created"), "{out}");
    assert_eq!(out.matches("(already split →").count(), 3, "{out}");

    // Resolving one decision unblocks nothing yet (two remain); resolving all three unblocks.
    for d in &ds {
        w.cv_ok(&["task", "resolve", d["task_id"].as_str().unwrap(), "--accept-default", "--from", "ember"]);
    }
    let (out, _) = w.cv_ok(&["task", "show", &parent]);
    assert!(!out.contains("⊘ BLOCKED"), "{out}");
    // --notes-last / --notes-grep
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--brief", "--notes-last", "2"]);
    assert!(out.contains("notes:    2 of 5"), "{out}");
    assert!(!out.contains("DECIDE: move the build base"), "{out}");
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--notes-grep", "k-portal"]);
    assert!(out.contains("notes:    1 of 5") && out.contains("K-PORTAL"), "{out}");
}

/// `note <id> -` and `--body -` read stdin; a relative `--issue` is absolutized at open time
/// (canonicalized when it exists); handles and URLs pass through.
#[test]
fn task_note_reads_stdin_and_open_absolutizes_issue() {
    let w = World::new("stdin-paths");
    fs::write(w.base.join("PLAN.md"), "# plan\n").unwrap();
    let (ok, _, out, err) = w.cv_stdin(
        &["task", "open", "with a heredoc body", "--body", "-", "--issue", "PLAN.md"],
        "a `code span` body\nwith two lines\n",
    );
    assert!(ok, "{err}");
    let a = opened_task_id(&out);
    let (json, _) = w.cv_ok(&["task", "show", &a, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["body"], "a `code span` body\nwith two lines\n");
    let issue = t["issue"].as_str().unwrap();
    assert!(std::path::Path::new(issue).is_absolute() && issue.ends_with("PLAN.md"), "{issue}");
    assert_eq!(std::path::Path::new(issue).canonicalize().unwrap(), w.base.join("PLAN.md").canonicalize().unwrap());

    let (ok, _, _, err) = w.cv_stdin(&["task", "note", &a, "-"], "note from stdin\n");
    assert!(ok, "{err}");
    let (out, _) = w.cv_ok(&["task", "show", &a]);
    assert!(out.contains("note from stdin"), "{out}");

    let (out, _) = w.cv_ok(&["task", "open", "handle", "--issue", "#42"]);
    let b = opened_task_id(&out);
    let (json, _) = w.cv_ok(&["task", "show", &b, "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"], "#42");
    let (out, _) = w.cv_ok(&["task", "open", "url", "--issue", "https://x.y/issues/4"]);
    let c = opened_task_id(&out);
    let (json, _) = w.cv_ok(&["task", "show", &c, "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"], "https://x.y/issues/4");
    // A relative path that does not exist yet is still rooted at the cwd.
    let (out, _) = w.cv_ok(&["task", "open", "future doc", "--issue", "docs/later.md"]);
    let d = opened_task_id(&out);
    let (json, _) = w.cv_ok(&["task", "show", &d, "--json"]);
    let issue = serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"].as_str().unwrap().to_string();
    assert!(issue.starts_with(w.base.to_str().unwrap()) || issue.starts_with(&w.base.canonicalize().unwrap().display().to_string()), "{issue}");
}

fn seeded_old_and_new(w: &World) -> (String, String) {
    let old = "01900000-0000-7000-8000-000000000001".to_string(); // uuid v7 from 2024
    let new = "01a00000-0000-7000-8000-000000000002".to_string();
    w.seed_task_log(&[
        serde_json::json!({"id": old, "task_id": old, "ts": "2026-01-01T00:00:00Z", "by": "orchestrator:old",
            "event": "opened", "title": "OLD: from another project", "channel": "tasks", "assignee": "ember"}),
        serde_json::json!({"id": new, "task_id": new, "ts": chrono_now(), "by": "orchestrator:new",
            "event": "opened", "title": "NEW: today's task", "channel": "tasks", "assignee": "ember"}),
    ]);
    (old, new)
}

/// An RFC 3339 "now" for seeded events (the test crate has no chrono: derive it from cv itself).
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // days since epoch → civil date (Howard Hinnant's algorithm), UTC, minute precision is plenty
    let days = secs / 86_400;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// A bare `list`/`inbox` is scoped to the last 14 days (or tasks involving `$CV_ENDPOINT`) and
/// says how many it hid; `--all` and `--since` lift the window.
#[test]
fn task_list_and_inbox_scope_to_recent_and_all_shows_old() {
    let w = World::new("scope");
    let (old, new) = seeded_old_and_new(&w);
    let (out, _) = w.cv_ok(&["task", "list"]);
    assert!(out.contains("NEW: today") && !out.contains("OLD: from"), "{out}");
    assert!(out.trim_end().ends_with("(1 older task(s) hidden — `--all`, or `--since 90d`)"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--all"]);
    assert!(out.contains("OLD: from") && out.contains("NEW: today") && !out.contains("hidden"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--since", "2025-12-01"]);
    assert!(out.contains("OLD: from") && !out.contains("hidden"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--since", "1d"]);
    assert!(!out.contains("OLD: from") && out.contains("hidden"), "{out}");
    // The caller's own tasks escape the window.
    let (ok, _, out, _) = w.cv_env(&["task", "list"], &[("CV_ENDPOINT", "orchestrator:old")]);
    assert!(ok && out.contains("OLD: from") && !out.contains("hidden"), "{out}");
    let (ok, _, out, _) = w.cv_env(&["task", "list"], &[("CV_ENDPOINT", "web:ember")]);
    assert!(ok && out.contains("OLD: from"), "web:<who> ≡ <who>, the assignee:\n{out}");

    // Same window for the inbox.
    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    assert!(out.contains("NEW: today") && !out.contains("OLD: from"), "{out}");
    assert!(out.contains("(1 older item(s) hidden"), "{out}");
    let (out, _) = w.cv_ok(&["task", "inbox", "ember", "--all"]);
    assert!(out.contains("OLD: from") && out.contains("⏰"), "{out}");
    let (md, _) = w.cv_ok(&["task", "inbox", "ember", "--md"]);
    assert!(md.contains("1 older hidden (`--all`)"), "{md}");
    let _ = (old, new);
}

/// `--unread` keeps items whose last event is not by `<who>` (web:<who> counts as <who>);
/// `events --since` prints JSON lines, filters by kind/by/assignee, and names the next cursor;
/// `watch --assignee` is the orchestrator's view: others' events on that person's tasks.
#[test]
fn task_inbox_unread_and_events_since() {
    let w = World::new("unread-events");
    let (out, _) = w.cv_ok(&["task", "open", "A: touched by the orchestrator last", "--assignee", "ember", "--from", "orchestrator:x"]);
    let a = opened_task_id(&out);
    let (out, _) = w.cv_ok(&["task", "open", "B: ember spoke last", "--assignee", "ember", "--from", "orchestrator:x"]);
    let b = opened_task_id(&out);
    w.cv_ok(&["task", "note", &b, "on it", "--from", "ember"]);
    let (out, _) = w.cv_ok(&["task", "open", "C: web ember spoke last", "--assignee", "ember", "--from", "orchestrator:x"]);
    let c = opened_task_id(&out);
    w.cv_ok(&["task", "note", &c, "via the page", "--from", "web:ember"]);

    let (out, _) = w.cv_ok(&["task", "inbox", "ember", "--unread"]);
    assert!(out.contains("A: touched") && !out.contains("B: ember") && !out.contains("C: web"), "{out}");
    let (json, _) = w.cv_ok(&["task", "inbox", "ember", "--unread", "--json"]);
    assert!(json.contains(&a) && !json.contains(&b), "{json}");

    // events: everything, as JSON lines, cursor on stderr.
    let (out, err) = w.cv_ok(&["task", "events"]);
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(lines[0]["event"], "opened");
    assert_eq!(lines[0]["title"], "A: touched by the orchestrator last");
    assert_eq!(lines[0]["task_state"], "open");
    assert_eq!(lines[0]["assignee"], "ember");
    let last_id = lines[4]["id"].as_str().unwrap().to_string();
    assert!(err.contains(&format!("next: --since {last_id}")), "{err}");
    // since an id is exclusive; since a time is inclusive; kinds take human spellings.
    let (out, _) = w.cv_ok(&["task", "events", "--since", &last_id]);
    assert!(out.trim().is_empty(), "{out}");
    let (out, _) = w.cv_ok(&["task", "events", "--since", lines[3]["id"].as_str().unwrap()]);
    assert_eq!(out.lines().count(), 1, "{out}");
    let (out, _) = w.cv_ok(&["task", "events", "--since", "1h", "--kind", "note"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    let (out, _) = w.cv_ok(&["task", "events", "--by", "ember"]);
    assert_eq!(out.lines().count(), 2, "web:ember is ember acting through the page:\n{out}");
    let (out, _) = w.cv_ok(&["task", "events", "--not-by", "orchestrator:x", "--text"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(out.contains("noted") && out.contains("on it") && out.contains("via the page"), "{out}");
    let (out, _) = w.cv_ok(&["task", "events", "--task", &b]);
    assert_eq!(out.lines().count(), 2, "{out}");
    let (_, err) = w.cv_fails(&["task", "events", "--since", "whenever"]);
    assert!(err.contains("cannot read"), "{err}");

    // watch: what ember did on their tasks, minus the caller's own events.
    let (ok, _, out, _) = w.cv_env(&["task", "watch", "--assignee", "ember", "--since", "1d"], &[("CV_ENDPOINT", "orchestrator:x")]);
    assert!(ok);
    let rows: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(rows.iter().all(|r| r["event"] == "noted"), "{out}");
}

// ───────────────────────────── cv task serve ─────────────────────────────

/// A minimal HTTP/1.1 client over a TcpStream (the test crate has no HTTP dependency).
fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).expect("connect to cv task serve");
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

/// Start `cv task serve` on an ephemeral port in this world; returns (child, addr).
fn serve(w: &World, who: &str) -> (std::process::Child, String) {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(["task", "serve", "--bind", "127.0.0.1:0", "--assignee", who])
        .current_dir(&w.base)
        .env("HOME", &w.home)
        .env("CLUSTERVISION_HOME", &w.cv_home)
        .env_remove("CV_ENDPOINT")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cv task serve starts");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let banner = lines.next().expect("a banner line").expect("readable");
    let addr = banner
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_else(|| panic!("no address in banner: {banner}"))
        .to_string();
    // Keep draining stderr so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    (child, addr)
}

/// The page and the CLI are one store: GET the inbox for a fixture assignee (one decision, one
/// action), POST resolve / done / note / discuss, and `cv task show --brief` reflects every event
/// with the author `web:<assignee>`. The event feed route is the same query as `cv task events`.
#[test]
fn task_serve_records_the_same_events_as_the_cli() {
    let w = World::new("serve");
    let (out, _) = w.cv_ok(&["task", "decide", "PAY §7: tariff", "--for", "ember", "--default", "1 credit/unit", "--option", "free tier", "--from", "orchestrator:x"]);
    let d = opened_task_id(&out);
    let (out, _) = w.cv_ok(&["task", "open", "push dregg-infra", "--assignee", "ember", "--from", "orchestrator:x"]);
    let a = opened_task_id(&out);
    let (mut child, addr) = serve(&w, "ember");
    let result = std::panic::catch_unwind(|| {
        // The page itself is served inline; it names its own origin story.
        let (status, html) = http(&addr, "GET", "/", None);
        assert_eq!(status, 200);
        assert!(html.contains("<title>cv inbox</title>") && html.contains("/api/inbox"), "{}", &html[..200.min(html.len())]);
        assert!(!html.contains("<script src=") && !html.contains("<link rel=\"stylesheet\""), "no external resources");
        // A DNS-named Host is refused (rebinding); an IP literal is fine (the LAN case).
        let (status, _) = {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(&addr).unwrap();
            s.write_all(b"GET /api/inbox HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").unwrap();
            let mut raw = String::new();
            s.read_to_string(&mut raw).unwrap();
            (raw.split_whitespace().nth(1).unwrap_or("0").parse::<u16>().unwrap_or(0), raw)
        };
        assert_eq!(status, 403);

        let (status, body) = http(&addr, "GET", "/api/inbox?who=ember", None);
        assert_eq!(status, 200, "{body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["who"], "ember");
        assert_eq!(page["counts"]["decisions"], 1);
        assert_eq!(page["counts"]["assigned"], 1);
        let items = page["items"].as_array().unwrap();
        assert_eq!(items[0]["reason"], "decision_owed");
        assert_eq!(items[0]["decision"]["default"], "1 credit/unit");
        assert_eq!(items[0]["decision"]["alternatives"], serde_json::json!(["free tier"]));
        assert_eq!(items[1]["reason"], "assigned_open");
        // Without --assignee/?who the server's default applies.
        let (status, body) = http(&addr, "GET", "/api/inbox", None);
        assert_eq!(status, 200);
        assert!(body.contains("\"who\":\"ember\""), "{body}");

        // Act: a note, then "needs discussion", then resolve with an option, then done.
        let (status, body) = http(&addr, "POST", &format!("/api/task/{d}/note"), Some(r#"{"text":"reading PAY first"}"#));
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"by\":\"web:ember\""), "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{d}/discuss"), Some(r#"{"text":"what is a unit?"}"#));
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{d}/resolve"), Some(r#"{"choice":"free tier","note":"for October"}"#));
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"task_state\":\"resolved\""), "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{d}/resolve"), Some(r#"{"accept_default":true}"#));
        assert_eq!(status, 400, "a second resolution is refused: {body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{a}/done"), Some(r#"{"observed":"pushed"}"#));
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{a}/done"), Some("{}"));
        assert_eq!(status, 400, "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{a}/frobnicate"), Some("{}"));
        assert_eq!(status, 400, "{body}");
        let (status, body) = http(&addr, "POST", &format!("/api/task/{a}/note"), Some("not json"));
        assert_eq!(status, 400, "{body}");

        // The closed items show under the page's Resolved filter; reopen brings a NEW task.
        let (status, body) = http(&addr, "GET", "/api/inbox?who=ember", None);
        assert_eq!(status, 200);
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 0);
        assert_eq!(page["closed"].as_array().unwrap().len(), 2);
        let (status, body) = http(&addr, "POST", &format!("/api/task/{a}/reopen"), Some("{}"));
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(&addr, "GET", "/api/inbox?who=ember", None);
        assert_eq!(status, 200);
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["title"], "push dregg-infra");
        assert!(items[0]["tags"].as_array().unwrap().iter().any(|t| t == "reopened"), "{body}");

        // One task, with its history; the event feed is the same query as `cv task events`.
        let (status, body) = http(&addr, "GET", &format!("/api/task/{d}?events=1"), None);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["effective_state"], "resolved");
        assert!(v["events"].as_array().unwrap().len() >= 6, "{body}");
        let (status, body) = http(&addr, "GET", "/api/events?since=1h&kind=resolved,done", None);
        assert_eq!(status, 200);
        let rows: Vec<serde_json::Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 2, "{body}");
        assert!(rows.iter().all(|r| r["by"] == "web:ember"), "{body}");
    });
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }

    // The CLI sees exactly what the page recorded, with the web author.
    let (out, _) = w.cv_ok(&["task", "show", &d, "--brief"]);
    assert!(out.contains("[resolved]"), "{out}");
    assert!(out.contains("resolved: free tier — by web:ember,") && out.contains("· for October"), "{out}");
    assert!(out.contains("web:ember") && out.contains("reading PAY first"), "{out}");
    assert!(out.contains("NEEDS DISCUSSION: what is a unit?"), "{out}");
    assert!(out.contains("tags:     decision, discuss"), "{out}");
    let (out, _) = w.cv_ok(&["task", "show", &a, "--brief"]);
    assert!(out.contains("[done]") && out.contains("self-reported") && out.contains("(pushed)"), "{out}");
    let (json, _) = w.cv_ok(&["task", "show", &a, "--events"]);
    assert!(json.contains("\"by\": \"web:ember\""), "{json}");
    let (out, _) = w.cv_ok(&["task", "events", "--since", "1h", "--kind", "resolved,done"]);
    assert_eq!(out.lines().count(), 2, "the CLI feed and /api/events agree:\n{out}");
}
// ───────────────────────────── distill ─────────────────────────────

/// Walk `parentUuid` back from the newest threaded record, the way Claude Code loads a session on
/// resume; returns how many records the walk reaches and the root it stops at.
fn chain_walk(path: &std::path::Path) -> (usize, usize, serde_json::Value) {
    let lines: Vec<serde_json::Value> = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let threaded: Vec<&serde_json::Value> = lines.iter().filter(|v| v.get("uuid").is_some()).collect();
    let by: std::collections::HashMap<&str, &serde_json::Value> =
        threaded.iter().map(|v| (v["uuid"].as_str().unwrap(), *v)).collect();
    let mut cur = *threaded.last().unwrap();
    let mut n = 1;
    while let Some(p) = cur["parentUuid"].as_str() {
        match by.get(p) {
            Some(v) => {
                cur = v;
                n += 1;
            }
            None => break,
        }
    }
    (n, threaded.len(), cur.clone())
}

#[test]
fn distill_emits_a_whole_resumable_session_and_a_sub_agent() {
    let w = World::new("distill");
    let big = "y".repeat(9000);
    w.write_session(
        "rootsess",
        &[serde_json::json!({
            "type": "user", "uuid": "r1", "sessionId": "rootsess", "timestamp": "2026-01-01T09:00:00Z",
            "cwd": "/work/proj", "message": {"role": "user", "content": "run the lanes"}
        })],
    );
    let sub = w.home.join(".claude/projects/-work-proj/rootsess/subagents");
    fs::create_dir_all(&sub).unwrap();
    let agent = sub.join("agent-a0123456789abcdef.jsonl");
    let line = |v: serde_json::Value| {
        let mut v = v;
        v["isSidechain"] = true.into();
        v["agentId"] = "a0123456789abcdef".into();
        v["sessionId"] = "rootsess".into();
        v["cwd"] = "/work/proj".into();
        format!("{v}\n")
    };
    let body = [
        line(serde_json::json!({"type": "user", "uuid": "u0", "parentUuid": null, "timestamp": "2026-01-01T10:00:00Z",
            "message": {"role": "user", "content": "You are lane TEST. Build it on box1 and commit on lane/test."}})),
        line(serde_json::json!({"type": "assistant", "uuid": "a1", "parentUuid": "u0", "timestamp": "2026-01-01T10:01:00Z",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ssh ember@box1 'cat big.log'", "description": "Read the big log"}}]}})),
        line(serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": "a1", "timestamp": "2026-01-01T10:02:00Z",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big, "is_error": false}]}})),
        // A hook record the IR does not carry — the next turn's parent. A reshaped session that
        // kept this link would resume from the break and silently lose everything before it.
        line(serde_json::json!({"type": "attachment", "uuid": "h1", "parentUuid": "u1", "timestamp": "2026-01-01T10:02:30Z",
            "attachment": {"type": "hook_success", "hookName": "PostToolUse", "content": ""}})),
        line(serde_json::json!({"type": "assistant", "uuid": "a2", "parentUuid": "h1", "timestamp": "2026-01-01T10:03:00Z",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [
                {"type": "text", "text": "Chose a rebase: the queue wants linear ranges."},
                {"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "cd /srv/x && git commit -F msg", "description": "Commit"}}]}})),
        line(serde_json::json!({"type": "user", "uuid": "u2", "parentUuid": "a2", "timestamp": "2026-01-01T10:04:00Z",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "[lane/test 1234abcd] kernel: one", "is_error": false}]}})),
        line(serde_json::json!({"type": "assistant", "uuid": "a3", "parentUuid": "u2", "timestamp": "2026-01-01T10:05:00Z",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [{"type": "text", "text": "Committed."}],
                "usage": {"input_tokens": 5, "cache_read_input_tokens": 900000, "output_tokens": 10}}})),
    ]
    .concat();
    fs::write(&agent, &body).unwrap();
    let agent_s = agent.to_str().unwrap();

    // The pack: brief, own words, commit, host, and the big output elided.
    let (out, err) = w.cv_ok(&["distill", agent_s, "--keep-last", "1"]);
    assert!(out.contains("You are lane TEST."), "{out}");
    assert!(out.contains("Chose a rebase"), "{out}");
    assert!(out.contains("`lane/test` `1234abcd`"), "{out}");
    assert!(out.contains("`ember@box1`"), "{out}");
    assert!(!out.contains(&"y".repeat(200)), "the big output must be elided");
    assert!(err.contains("distilled"), "{err}");

    // A resumable session: every record reachable from the newest, rooted at the pack prompt.
    let (out, _) = w.cv_ok(&["distill", agent_s, "--keep-last", "1", "--session", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    let path = PathBuf::from(v["emitted"]["path"].as_str().unwrap());
    let new_id = v["emitted"]["session_id"].as_str().unwrap().to_string();
    let (reached, threaded, root) = chain_walk(&path);
    assert_eq!(reached, threaded, "a dangling parent truncates the resumed context");
    assert!(
        root["message"]["content"].as_str().unwrap().contains("[cv distill]"),
        "{root}"
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("\"isSidechain\":true"), "a main-thread session");
    assert!(
        !text.contains("900000"),
        "the source's near-limit usage must not reach the resume gate"
    );
    // The elided output comes back through the sidecar.
    let (cat, _) = w.cv_ok(&["cat", &new_id, "t1"]);
    assert!(cat.contains(&"y".repeat(9000)), "cv cat must return the elided payload");

    // A sub-agent of the root: sidechain records under the root's id, in the root's subagents/.
    let (out, _) = w.cv_ok(&[
        "distill",
        agent_s,
        "--keep-last",
        "1",
        "--agent-of",
        "rootsess",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let aid = v["emitted"]["agent_id"].as_str().unwrap();
    let apath = sub.join(format!("agent-{aid}.jsonl"));
    assert!(apath.exists() && sub.join(format!("agent-{aid}.meta.json")).exists());
    let (reached, threaded, _) = chain_walk(&apath);
    assert_eq!(reached, threaded);
    for l in fs::read_to_string(&apath).unwrap().lines() {
        let r: serde_json::Value = serde_json::from_str(l).unwrap();
        assert_eq!(r["isSidechain"], true, "{l}");
        assert_eq!(r["agentId"], aid, "{l}");
        assert_eq!(r["sessionId"], "rootsess", "{l}");
    }
    assert_eq!(
        fs::read_to_string(&agent).unwrap(),
        body,
        "distill must never modify the source"
    );
}

// ───────────────────────────── adopt ─────────────────────────────

#[test]
fn adopt_moves_a_stranded_lane_into_the_live_session() {
    let w = World::new("adopt");
    let (dead, live, aid) = ("deadsess", "livesess", "a0123456789abcdef");
    let root_line = |sid: &str| {
        serde_json::json!({"type": "user", "uuid": format!("{sid}-1"), "sessionId": sid,
            "timestamp": "2026-01-01T09:00:00Z", "cwd": "/work/proj",
            "message": {"role": "user", "content": "run the lanes"}})
    };
    w.write_session(dead, &[root_line(dead)]);
    let sub = w.home.join(".claude/projects/-work-proj/deadsess/subagents");
    fs::create_dir_all(&sub).unwrap();
    let line = |v: serde_json::Value| {
        let mut v = v;
        v["isSidechain"] = true.into();
        v["agentId"] = aid.into();
        v["sessionId"] = dead.into();
        v["cwd"] = "/work/proj".into();
        format!("{v}\n")
    };
    let body = [
        line(serde_json::json!({"type": "user", "uuid": "u0", "parentUuid": null, "timestamp": "2026-01-01T10:00:00Z",
            "message": {"role": "user", "content": "You are lane TEST."}})),
        line(serde_json::json!({"type": "assistant", "uuid": "a1", "parentUuid": "u0", "timestamp": "2026-01-01T10:01:00Z",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "grep sessionId x.jsonl"}}]}})),
        // The tool output quotes a transcript line: its sessionId is content and must survive.
        line(serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": "a1", "timestamp": "2026-01-01T10:02:00Z",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                "content": "{\"sessionId\":\"deadsess\"}", "is_error": false}]}})),
    ]
    .concat();
    let agent = sub.join(format!("agent-{aid}.jsonl"));
    fs::write(&agent, &body).unwrap();
    let meta = r#"{"agentType":"general-purpose","description":"Lane TEST","model":"opus"}"#;
    fs::write(sub.join(format!("agent-{aid}.meta.json")), meta).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    w.write_session(live, &[root_line(live)]); // the newest session: the default --into

    let (out, _) = w.cv_ok(&["adopt", "--list", dead]);
    assert!(out.contains(aid) && out.contains("Lane TEST") && out.contains("yes"), "{out}");

    let live_sub = w.home.join(".claude/projects/-work-proj/livesess/subagents");
    let (_, err) = w.cv_ok(&["adopt", &aid[..8], "--dry-run"]);
    assert!(err.contains("into livesess"), "the default target is announced: {err}");
    assert!(err.contains("restamped on 3 of 3 lines"), "{err}");
    assert!(err.contains(&format!("\"to\": \"{aid}\"")), "the SendMessage incantation: {err}");
    assert!(err.contains("cv cat deadsess"), "the sidecar reminder: {err}");
    assert!(!live_sub.exists(), "a dry run writes nothing");

    w.cv_ok(&["adopt", aid]);
    let copied = fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl"))).unwrap();
    assert_eq!(copied, body.replace("\"sessionId\":\"deadsess\"", "\"sessionId\":\"livesess\"").replace(
        "{\\\"sessionId\\\":\\\"livesess\\\"}",
        "{\\\"sessionId\\\":\\\"deadsess\\\"}"
    ));
    assert!(copied.contains("{\\\"sessionId\\\":\\\"deadsess\\\"}"), "content untouched");
    assert_eq!(fs::read_to_string(live_sub.join(format!("agent-{aid}.meta.json"))).unwrap(), meta);
    assert_eq!(fs::read_to_string(&agent).unwrap(), body, "the source is never modified");

    // A second adoption refuses (nothing written) until --force.
    fs::write(live_sub.join(format!("agent-{aid}.jsonl")), "sentinel\n").unwrap();
    let (ok, _, _, err) = w.cv(&["adopt", aid, "--from", dead, "--into", live]);
    assert!(!ok && err.contains("refusing to overwrite"), "{err}");
    assert_eq!(fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl"))).unwrap(), "sentinel\n");
    w.cv_ok(&["adopt", aid, "--from", dead, "--into", live, "--force"]);
    assert!(fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl"))).unwrap().contains("livesess"));

    // The agent now has two copies; reading it by id takes the newest instead of erroring.
    let (out, _) = w.cv_ok(&["cat", &format!("agent-{aid}"), "t1"]);
    assert!(out.contains("deadsess"), "{out}");
}
