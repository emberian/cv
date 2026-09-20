//! End-to-end `cv pack`: a hermetic temp-HOME claude corpus with distinct topics + events, then
//! the actual binary is run against it (same pattern as tests/cli.rs — env is only ever passed to
//! the spawned process, so tests parallelize safely).

use std::path::PathBuf;
use std::process::Command;
use std::{fs, str};

/// One temp world: a fake `$HOME` (with `.claude/projects` fixtures) + a `$CLUSTERVISION_HOME`.
struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cv-pack-{tag}-{}-{}",
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

    fn write_session(&self, name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = self
            .home
            .join(".claude/projects/-work-proj")
            .join(format!("{name}.jsonl"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(&path, body).unwrap();
        path
    }

    fn cv(&self, args: &[&str]) -> (bool, i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cv"))
            .args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLUSTERVISION_HOME", &self.cv_home)
            .env_remove("CV_PACK_LLM") // tests must exercise the zero-network digest
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .output()
            .expect("cv should run");
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn cv_ok(&self, args: &[&str]) -> (String, String) {
        let (ok, code, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    fn cv_fails(&self, args: &[&str]) -> (String, String) {
        let (ok, code, out, err) = self.cv(args);
        assert!(!ok, "cv {args:?} unexpectedly succeeded\nstdout:\n{out}");
        assert_ne!(code, -1, "cv {args:?} died by signal (panic/abort?)\nstderr:\n{err}");
        assert!(!err.contains("panicked"), "cv {args:?} panicked:\n{err}");
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

/// Three tiny sessions with distinct topics:
/// - `wombatsess` ("wombat cache design"): the wombat caching layer — edits src/cache.rs, runs
///   `cargo test -p cache`, and carries a fenced code block in the relevant excerpt.
/// - `quokkasess`: quokka census chatter, no tool use.
/// - `gammasess`: parser work touching src/parser.rs (a different file, so the rollup has shape).
fn pack_corpus(w: &World) {
    w.write_session(
        "wombatsess",
        &[
            serde_json::json!({"type": "ai-title", "aiTitle": "wombat cache design"}),
            user_line("w1", "2026-02-01T10:00:00Z", "let's design the wombat caching layer for the parser"),
            assistant_line(
                "w2",
                "2026-02-01T10:01:00Z",
                serde_json::json!([
                    {"type": "text", "text": "the wombat cache should be keyed by mtime:\n```rust\nstruct WombatCache { keyed_by_mtime: bool }\n```"},
                    {"type": "tool_use", "id": "wt1", "name": "Edit",
                     "input": {"file_path": "/work/proj/src/cache.rs", "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "wt2", "name": "Bash",
                     "input": {"command": "cargo test -p cache"}}
                ]),
            ),
            serde_json::json!({
                "type": "user", "uuid": "w3", "sessionId": "s", "timestamp": "2026-02-01T10:02:00Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "wt1", "content": "edited ok", "is_error": false}
                ]}
            }),
            user_line("w4", "2026-02-01T10:03:00Z", "great, ship the wombat cache"),
        ],
    );
    w.write_session(
        "quokkasess",
        &[
            user_line("q1", "2026-03-01T09:00:00Z", "how many quokkas in the census"),
            assistant_line(
                "q2",
                "2026-03-01T09:05:00Z",
                serde_json::json!([{"type": "text", "text": "seventeen quokkas"}]),
            ),
        ],
    );
    w.write_session(
        "gammasess",
        &[
            user_line(
                "g1",
                "2026-04-01T09:00:00Z",
                "the parser chokes on empty caching headers",
            ),
            assistant_line(
                "g2",
                "2026-04-01T09:05:00Z",
                serde_json::json!([
                    {"type": "text", "text": "fixed the parser caching header handling"},
                    {"type": "tool_use", "id": "gt1", "name": "Edit",
                     "input": {"file_path": "/work/proj/src/parser.rs", "old_string": "x", "new_string": "y"}}
                ]),
            ),
        ],
    );
}

// ───────────────────────────── md format (default) ─────────────────────────────

#[test]
fn pack_md_no_index_fallback_and_content() {
    let w = World::new("md");
    pack_corpus(&w);

    // No index yet: the live-scan fallback is announced, and recall still finds the right session.
    let (out, err) = w.cv_ok(&["pack", "wombat caching layer"]);
    assert!(err.contains("no index yet"), "want live-scan note, stderr:\n{err}");
    assert!(err.contains("live scan"), "{err}");
    assert!(
        err.contains("no embeddings store"),
        "honest FTS-only note expected:\n{err}"
    );

    // Bundle framing.
    assert!(out.contains("# Context pack: wombat caching layer"), "{out}");
    // The relevant session is a source section, with harness/date/cwd in the header.
    assert!(out.contains("## wombat cache design (claude, 2026-02-01"), "{out}");
    assert!(out.contains("/work/proj"), "{out}");
    // The matched excerpt survives, code fence intact.
    assert!(out.contains("keyed by mtime"), "excerpt expected:\n{out}");
    assert!(out.contains("```rust"), "code block must be preserved:\n{out}");
    assert!(out.contains("struct WombatCache"), "{out}");
    // Event-catalog facts: the file edit and the command.
    assert!(out.contains("Files touched:"), "{out}");
    assert!(out.contains("/work/proj/src/cache.rs"), "{out}");
    assert!(out.contains("Commands run:"), "{out}");
    assert!(out.contains("cargo test -p cache"), "{out}");
    // The cross-source rollup section exists and ranks the edited file.
    assert!(out.contains("## Files that keep appearing"), "{out}");
    assert!(out.contains("edit(s)"), "{out}");
    // A pointer back into the full transcript.
    assert!(out.contains("cv show wombatse"), "{out}");
    // The irrelevant session is NOT packed.
    assert!(
        !out.contains("quokkas"),
        "irrelevant session leaked into the pack:\n{out}"
    );
}

#[test]
fn pack_uses_index_when_present_and_respects_limit() {
    let w = World::new("idx");
    pack_corpus(&w);

    let (out, _) = w.cv_ok(&["index"]);
    assert!(out.contains("indexed 3 top-level session(s)"), "{out}");

    // Indexed recall: no live-scan note.
    let (out, err) = w.cv_ok(&["pack", "wombat caching layer"]);
    assert!(!err.contains("live scan"), "indexed pack must not live-scan:\n{err}");
    assert!(out.contains("## wombat cache design"), "{out}");
    assert!(out.contains("src/cache.rs"), "{out}");

    // "caching" appears in two sessions; --limit 1 keeps only the best source section.
    let (out, _) = w.cv_ok(&["pack", "caching", "--limit", "1"]);
    let sections = out.matches("\n## ").count() + usize::from(out.starts_with("## "));
    // One source + (possibly) the rollup section.
    assert!(
        sections <= 2,
        "--limit 1 must keep at most one source section (+rollup):\n{out}"
    );

    // A task matching nothing: friendly empty result, exit 0.
    let (out, _) = w.cv_ok(&["pack", "xyzzyplugh"]);
    assert!(out.contains("no relevant past sessions"), "{out}");

    // --out writes the bundle to a file instead of stdout.
    let dest = w.base.join("pack.md");
    let (out, err) = w.cv_ok(&["pack", "wombat caching layer", "--out", dest.to_str().unwrap()]);
    assert!(out.trim().is_empty(), "--out must not also print the bundle:\n{out}");
    assert!(err.contains("wrote context pack"), "{err}");
    let written = fs::read_to_string(&dest).unwrap();
    assert!(written.contains("# Context pack"), "{written}");
}

// ───────────────────────────── prompt format ─────────────────────────────

#[test]
fn pack_prompt_differs_from_md() {
    let w = World::new("prompt");
    pack_corpus(&w);

    let (md, _) = w.cv_ok(&["pack", "wombat caching layer"]);
    let (prompt, _) = w.cv_ok(&["pack", "wombat caching layer", "--format", "prompt"]);

    // Second-person system-prompt shape.
    assert!(
        prompt.contains("You are picking up work on: wombat caching layer"),
        "{prompt}"
    );
    assert!(prompt.contains("Prior context from earlier sessions"), "{prompt}");
    assert!(prompt.contains("You touched:"), "{prompt}");
    assert!(prompt.contains("You ran:"), "{prompt}");
    // Not the md bundle shape.
    assert!(!prompt.contains("# Context pack"), "{prompt}");
    assert!(!prompt.contains("## Files that keep appearing"), "{prompt}");
    assert_ne!(md, prompt);
    // Same recalled substance though.
    assert!(prompt.contains("wombat cache design"), "{prompt}");
    assert!(prompt.contains("src/cache.rs"), "{prompt}");
}

// ───────────────────────────── session format ─────────────────────────────

#[test]
fn pack_session_emits_resumable_claude_session_that_round_trips() {
    let w = World::new("session");
    pack_corpus(&w);

    // --format session requires --harness, and the error spells the fix out.
    let (_, err) = w.cv_fails(&["pack", "wombat caching layer", "--format", "session"]);
    assert!(
        err.contains("--format session needs --harness <harness>") && err.contains("--harness claude"),
        "{err}"
    );

    // Emit into the (temp-HOME) claude storage root, like `cv port` does.
    let (out, _) = w.cv_ok(&[
        "pack",
        "wombat caching layer",
        "--format",
        "session",
        "--harness",
        "claude",
    ]);
    assert!(out.contains("✦ wrote "), "{out}");
    assert!(out.contains("claude --resume"), "resume hint expected:\n{out}");

    // Pull the written path + new id out of the "✦ wrote <path> (<id>)" line.
    let wrote = out.lines().find(|l| l.contains("✦ wrote")).unwrap();
    let path = wrote
        .trim_start_matches("✦ wrote ")
        .split(" (")
        .next()
        .unwrap()
        .to_string();
    let new_id = wrote.rsplit(" (").next().unwrap().trim_end_matches(')').to_string();
    assert!(path.ends_with(".jsonl"), "{wrote}");
    assert!(PathBuf::from(&path).exists(), "emitted file missing: {path}");

    // Round-trip: the target adapter re-parses what pack emitted.
    let sref = cv_core::ir::SessionRef {
        id: new_id.clone(),
        harness: cv_core::ir::Harness::Claude,
        path: PathBuf::from(&path),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };
    use cv_core::Adapter;
    let parsed = cv_core::harness::claude::Claude::new()
        .parse(&sref)
        .expect("claude adapter must re-parse the emitted pack session");
    assert_eq!(parsed.messages.len(), 2, "user framing + assistant ack");
    let user_text = parsed.messages[0].text().unwrap_or_default();
    assert!(user_text.contains("wombat caching layer"), "{user_text}");
    assert!(
        user_text.contains("# Context pack"),
        "bundle must ride in the user turn"
    );
    assert!(user_text.contains("src/cache.rs"), "{user_text}");
    let ack = parsed.messages[1].text().unwrap_or_default();
    assert!(ack.contains("Ready to continue"), "{ack}");

    // And the emitted session is discoverable + showable through the CLI itself.
    let (out, _) = w.cv_ok(&["show", &new_id]);
    assert!(out.contains("Context pack"), "{out}");
}

// ───────────────────────────── argument validation ─────────────────────────────

#[test]
fn pack_rejects_bad_arguments() {
    let w = World::new("args");
    pack_corpus(&w);

    let (_, err) = w.cv_fails(&["pack", "x", "--format", "docx"]);
    assert!(err.contains("unknown format"), "{err}");

    // --harness is the target-harness flag, and only --format session has a target.
    let (_, err) = w.cv_fails(&["pack", "x", "--harness", "claude"]);
    assert!(err.contains("--harness only applies to --format session"), "{err}");

    let (_, err) = w.cv_fails(&["pack", "x", "--format", "session", "--harness", "marsrover"]);
    assert!(err.contains("unknown target harness"), "{err}");

    // The 0.10 `--to` spelling is gone, with a pointer at --harness (exit 2: a caller mistake).
    let (ok, code, out, err) = w.cv(&["pack", "x", "--to", "claude"]);
    assert!(!ok, "`pack --to` must not still work:\n{out}");
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("renamed in 0.11.0") && err.contains("--harness <harness>"),
        "{err}"
    );
}
