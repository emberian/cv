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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cv"));
        cmd.args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLUSTERVISION_HOME", &self.cv_home)
            // Linux fallbacks for dirs::cache_dir / config_dir; harmless on macOS.
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env_remove("CV_ENDPOINT");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("cv should run");
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
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

    // JSON: the full IR round-trips through serde.
    let (out, _) = w.cv_ok(&["export", "alphasess", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["id"], "alphasess", "{out}");
    assert_eq!(v["harness"], "claude", "{out}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 3, "{out}");

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
        // `recall` / `distill` superseded by the one build-context verb.
        (&["recall", "zebrafish"], "cv pack <task>"),
        (&["distill", "alphasess"], "cv pack <task>"),
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
    assert!(out.contains("ClaimedByYou"), "{out}");

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
