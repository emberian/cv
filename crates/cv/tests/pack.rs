//! End-to-end `cv pack`: a hermetic temp-HOME claude corpus with distinct topics + events, then
//! the actual binary is run against it through `cv_testkit::World` (env is only ever passed to the
//! spawned process, so tests parallelize safely).

use claude::user as user_line;
use cv_testkit::{claude, World};
use std::path::PathBuf;
use std::{fs, str};

fn assistant_line(uuid: &str, ts: &str, blocks: serde_json::Value) -> serde_json::Value {
    claude::with(
        claude::assistant(uuid, ts, "claude-test-1", blocks),
        serde_json::json!({"cwd": "/work/proj"}),
    )
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
            user_line("wombatsess", "w1", "2026-02-01T10:00:00Z", "let's design the wombat caching layer for the parser"),
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
            user_line("wombatsess", "w4", "2026-02-01T10:03:00Z", "great, ship the wombat cache"),
        ],
    );
    w.write_session(
        "quokkasess",
        &[
            user_line(
                "quokkasess",
                "q1",
                "2026-03-01T09:00:00Z",
                "how many quokkas in the census",
            ),
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
                "gammasess",
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

    // No index yet: the live-suffix read is announced, and recall still finds the right session.
    let (out, err) = w.cv_ok(&["pack", "wombat caching layer"]);
    assert!(err.contains("no index yet"), "want live-suffix note, stderr:\n{err}");
    assert!(err.contains("the index has not seen live"), "{err}");
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
    // A qualified pointer back into the full transcript uses the supported range syntax.
    assert!(out.contains("cv show claude:wombatsess --range 0..2"), "{out}");
    // The irrelevant session is NOT packed.
    assert!(
        !out.contains("quokkas"),
        "irrelevant session leaked into the pack:\n{out}"
    );
}

#[test]
fn pack_header_states_its_bounds_and_what_they_cut() {
    let w = World::new("bounds");
    pack_corpus(&w);
    let (out, _) = w.cv_ok(&["pack", "wombat caching layer"]);
    assert!(
        out.contains("Bounds: each excerpt message to 1600 bytes (`--excerpt-bytes`; 0 cut, marked …)"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["pack", "wombat caching layer", "--excerpt-bytes", "20"]);
    assert!(out.contains("to 20 bytes (`--excerpt-bytes`;"), "{out}");
    assert!(
        !out.contains("`--excerpt-bytes`; 0 cut"),
        "a 20-byte budget cuts:\n{out}"
    );
    assert!(out.contains("…"), "{out}");
    let (ok, code, _, err) = w.cv(&["pack", "wombat", "--context-bytes", "100"]);
    assert!(
        !ok && code == 2 && err.contains("unexpected argument '--context-bytes'"),
        "{code} {err}"
    );
}

#[test]
fn pack_uses_index_when_present_and_respects_limit() {
    let w = World::new("idx");
    pack_corpus(&w);

    let (out, _) = w.cv_ok(&["index", "--json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["indexed"], 3);

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
    let (out, _) = w.cv_ok(&["pack", "xyzzyplugh", "--json"]);
    assert!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["content"].is_null());

    // --out writes the bundle to a file instead of stdout.
    let dest = w.base.join("pack.md");
    let (out, _) = w.cv_ok(&[
        "pack",
        "wombat caching layer",
        "--json",
        "--out",
        dest.to_str().unwrap(),
    ]);
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["path"], dest.to_str().unwrap());
    assert!(result["content"].is_null());
    let written = fs::read_to_string(&dest).unwrap();
    assert!(written.contains("# Context pack"), "{written}");

    // 0.18 live-suffix fresh recall + bare-colon query syntax resilience: a session written AFTER
    // `cv index` is still recalled, and bare `:` / `[...]` in task text never crash QueryParser.
    w.write_session(
        "freshsess",
        &[
            serde_json::json!({"type": "ai-title", "aiTitle": "fresh unindexed fix"}),
            user_line(
                "freshsess",
                "f1",
                "2026-05-01T10:00:00Z",
                "fix bug in ~/dev/foo: platypus",
            ),
            assistant_line(
                "f2",
                "2026-05-01T10:01:00Z",
                serde_json::json!([{"type": "text", "text": "resolved the platypus colon issue"}]),
            ),
        ],
    );
    let (out, err) = w.cv_ok(&["pack", "fix bug in ~/dev/foo: platypus [E0433]"]);
    assert!(
        err.contains("the index has not seen live"),
        "live suffix should read freshsess:\n{err}"
    );
    assert!(
        out.contains("## fresh unindexed fix"),
        "fresh unindexed session must be recalled:\n{out}"
    );
}

#[test]
fn pack_json_content_matches_plain_bundle() {
    let w = World::new("json-content");
    pack_corpus(&w);
    for format in ["md", "prompt"] {
        let (plain, _) = w.cv_ok(&["pack", "wombat caching layer", "--format", format]);
        let (json, _) = w.cv_ok(&["pack", "wombat caching layer", "--format", format, "--json"]);
        let result: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(result["content"], plain);
        assert!(result["path"].is_null());
    }
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
        "--json",
        "--harness",
        "claude",
    ]);
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(result["resume_hint"].as_str().unwrap().contains("claude --resume"));
    let path = result["path"].as_str().unwrap().to_string();
    let new_id = result["id"].as_str().unwrap().to_string();
    assert!(path.ends_with(".jsonl"), "{out}");
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
        message_count: None,
    };
    use cv_core::harness::Parse;
    let parsed = cv_core::harness::claude::Claude::detect()
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

    let (_, code, _, err) = w.cv(&["pack", "x", "--format", "docx"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("docx") && err.contains("session"),
        "clap names the value and the choices:\n{err}"
    );

    // --harness is the target-harness flag, and only --format session has a target.
    let (_, err) = w.cv_fails(&["pack", "x", "--harness", "claude"]);
    assert!(err.contains("--harness only applies to --format session"), "{err}");

    let (_, err) = w.cv_fails(&["pack", "x", "--format", "session", "--harness", "marsrover"]);
    assert!(err.contains("unknown target harness"), "{err}");

    // The 0.10 `--to` spelling is a plain unknown argument now (its 0.11 tombstone left at 0.15):
    // clap's error, exit 2.
    let (ok, code, out, err) = w.cv(&["pack", "x", "--to", "claude"]);
    assert!(!ok, "`pack --to` must not still work:\n{out}");
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("unexpected argument '--to'"), "{err}");
}
