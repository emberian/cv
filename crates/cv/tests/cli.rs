//! End-to-end tests for the `cv` CLI surface: ls sorting, search fallback messaging, show
//! windowing, export formats, events/touched, redact, and error-path honesty (nonzero + stderr,
//! never a panic).
//!
//! Hermetic: every test builds its own temp `HOME` (harness discovery roots hang off the home
//! dir) and temp `CLUSTERVISION_HOME` (catalog/index/board), and only ever passes them to the
//! spawned binary's environment — no process-global `set_var`, so tests run in parallel safely.

use claude::user as user_line;
use cv_testkit::{claude, World};
use std::path::PathBuf;
use std::process::Command;
use std::{fs, str};

/// A planted secret that `cv export --redact` must scrub (matches cv_core::redact's sk- recognizer).
use cv_testkit::corpus::{self, SECRET};

/// A tag-only decision: opened assigned to `who` and tagged `decision`, with nothing posed — the
/// shape `cv task open --tags decision` wrote before it was refused; it reads as an assigned
/// action. Stamped a few seconds ahead so it sorts after anything the test just opened.
fn plant_tag_only_decision(w: &World, id: &str, title: &str, who: &str) {
    let ts = cv_testkit::now_rfc3339_at(3);
    w.append_task_log(&[
        serde_json::json!({"id": id, "task_id": id, "ts": ts, "by": "cv", "event": "opened",
            "title": title, "channel": "tasks", "assignee": who}),
        serde_json::json!({"id": format!("{}f", &id[..id.len() - 1]), "task_id": id, "ts": ts, "by": "cv",
            "event": "tagged", "tags": ["decision"]}),
    ]);
}

fn assistant_line(uuid: &str, ts: &str, blocks: serde_json::Value) -> serde_json::Value {
    claude::with(
        claude::assistant(uuid, ts, "claude-test-1", blocks),
        serde_json::json!({"cwd": "/work/proj"}),
    )
}

/// The standard two-session corpus, shared with every suite that plants it (see cv-testkit).
fn standard_corpus(w: &World) {
    corpus::standard(w);
}

/// Index of `needle`'s first occurrence, with a labeled panic when missing.
fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in:\n{hay}"))
}

// ───────────────────────────── ls ─────────────────────────────

#[test]
fn removed_rewind_from_compaction_exits_two_with_a_pointer() {
    let w = World::new("rewind-from-compaction");
    let (ok, code, _, err) = w.cv(&["rewind", "any", "--from-compaction"]);
    assert!(!ok && code == 2, "{err}");
    assert!(err.contains("--from-compaction") && err.contains("--full"), "{err}");
}

#[test]
fn removed_claurdvoyant_variables_exit_two_with_a_pointer() {
    let w = World::new("claurdvoyant-env");
    for (old, new) in [
        ("CLAURDVOYANT_HOME", "CLUSTERVISION_HOME"),
        ("CLAURDVOYANT_MAX_STALE_SECS", "CLUSTERVISION_MAX_STALE_SECS"),
    ] {
        let (ok, code, _, err) = w.cv_env(&["ls"], &[(old, "1")]);
        assert!(!ok && code == 2, "{old}: {err}");
        assert!(err.contains(old) && err.contains(new), "{err}");
    }
}

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
    // The prompt and the reply; the tool result is no turn.
    assert_eq!(rows[0]["message_count"], 2);
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

/// `text:` with no full-text index answers from the live read when it covers every session, as
/// it does here; with nothing read live (`--frozen`) there is no answer: exit 2 naming `cv index`,
/// never an empty (or, under `NOT`, a full) listing that looks like one.
#[test]
fn a_text_query_with_no_index_reads_live_or_exits_2() {
    let w = World::new("ls-text-no-index");
    standard_corpus(&w);
    let (out, _) = w.cv_ok(&["ls", "--json", "-q", "text:zebrafish"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        rows.iter().map(|r| r["id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["alphasess"]
    );
    for query in ["text:needle", "NOT text:needle"] {
        let (ok, code, out, err) = w.cv(&["--frozen", "ls", "--json", "-q", query]);
        assert!(!ok && code == 2, "{query}: exit {code}\n{out}\n{err}");
        assert!(out.trim().is_empty(), "{query}: {out}");
        assert!(err.contains("cv index"), "{query}: {err}");
    }
}

#[test]
fn ls_json_zero_limit_validates_inputs_without_creating_a_catalog() {
    let w = World::new("ls-json-zero");
    standard_corpus(&w);
    assert!(!w.cv_home.join("catalog.db").exists());
    let (out, err) = w.cv_ok(&[
        "ls",
        "--json",
        "--limit",
        "0",
        "--fresh",
        "--enrich",
        "-q",
        "text:needle AND has:plan",
    ]);
    assert_eq!(out.trim(), "[]");
    assert!(err.is_empty());
    assert!(
        !w.cv_home.join("catalog.db").exists(),
        "empty window must not discover/publish inventory"
    );
    let (_, err) = w.cv_fails(&["ls", "--json", "--limit", "0", "--harness", "invalid"]);
    assert!(err.contains("unknown harness"));
    let (_, err) = w.cv_fails(&["ls", "--json", "--limit", "0", "-q", "("]);
    assert!(!err.is_empty());
}

/// Codex sessions on the committed corpus are titled by their first prompt, never the injected
/// AGENTS.md or environment-context turn that opens every rollout: the row's `title` and
/// `display_title`, and the title of every index and live-scan hit.
#[test]
fn codex_titles_are_the_first_prompt_in_listings_and_hits() {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy(&path, &target);
            } else {
                fs::copy(&path, &target).unwrap();
            }
        }
    }
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cv-core/tests/corpus/codex");
    for version in fs::read_dir(&corpus).unwrap() {
        let version = version.unwrap().path();
        let w = World::new("lsjsonenrich-codex");
        copy(&version.join(".codex"), &w.home.join(".codex"));
        let (out, _) = w.cv_ok(&["ls", "--json", "--enrich", "--harness", "codex", "--limit", "100"]);
        let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).expect("pure JSON");
        assert!(!rows.is_empty(), "{}", version.display());
        for row in &rows {
            assert!(row["title"].is_string(), "{row}");
            assert_eq!(row["display_title"], row["title"], "{}: {row}", version.display());
        }
        let titles: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|row| {
                (
                    row["id"].as_str().unwrap().to_string(),
                    cv_core::label_from(row["title"].as_str(), None),
                )
            })
            .collect();
        let needle = rows
            .iter()
            .filter_map(|row| row["title"].as_str())
            .flat_map(str::split_whitespace)
            .find(|word| word.len() >= 5 && word.chars().all(char::is_alphanumeric))
            .expect("fixture prompt term");
        w.cv_ok(&["index"]);
        for scan in [false, true] {
            let mut args = vec!["search", "--json", "--limit", "100"];
            if scan {
                args.push("--scan");
            }
            let (out, _) = w.cv_ok(&[args.as_slice(), &[needle]].concat());
            let (hits, _) = search_json(&out);
            assert!(!hits.is_empty(), "{} scan={scan} term={needle}", version.display());
            for hit in hits {
                assert_eq!(
                    hit["title"].as_str(),
                    titles.get(hit["id"].as_str().unwrap()).map(String::as_str),
                    "{} scan={scan}",
                    version.display()
                );
            }
        }
    }
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

#[test]
fn ls_json_fresh_enumerates_a_previously_empty_project_and_drops_deleted_rows() {
    let w = World::new("ls-json-fresh-consumer");
    standard_corpus(&w);
    let empty_project = w.proj.parent().unwrap().join("-work-new-project");
    std::fs::create_dir_all(&empty_project).unwrap();
    let (out, _) = w.cv_ok(&["ls", "--json"]);
    assert_eq!(serde_json::from_str::<Vec<serde_json::Value>>(&out).unwrap().len(), 2);

    w.write_session(
        "new-project-session",
        &[serde_json::json!({
            "type":"user", "uuid":"new-u", "sessionId":"new-project-session",
            "timestamp":"2026-10-14T12:00:00Z", "cwd":"/work/new-project",
            "gitBranch":"consumer-fresh", "message":{"role":"user","content":"a new project"}
        })],
    );
    let new_path = empty_project.join("new-project-session.jsonl");
    std::fs::rename(w.proj.join("new-project-session.jsonl"), &new_path).unwrap();
    std::fs::remove_file(w.proj.join("betasess.jsonl")).unwrap();

    let (out, _) = w.cv_ok(&["ls", "--json", "--fresh", "--enrich", "--limit", "100"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(!rows.iter().any(|row| row["id"] == "betasess"), "{out}");
    let added = rows.iter().find(|row| row["id"] == "new-project-session").unwrap();
    assert_eq!(added["size_bytes"], std::fs::metadata(new_path).unwrap().len());
    assert_eq!(added["git"]["branch"], "consumer-fresh");
    assert_eq!(added["display_title"], "a new project");
}

// ───────────────────────────── search ─────────────────────────────

#[test]
fn search_fallback_messaging_and_index_path() {
    let w = World::new("search");
    standard_corpus(&w);

    // No index yet: every session is unindexed, so the live read (all of it by default) answers
    // and the footer says so; never an unasked scan.
    let (out, err) = w.cv_ok(&["search", "zebrafish"]);
    assert!(out.contains("alphases") && out.contains("no index yet"), "{out}");
    assert!(!err.contains("scanning"), "{err}");
    // --frozen reads nothing live: no index is then an error naming the cause and both cures.
    let (out, err) = w.cv_fails(&["--frozen", "search", "zebrafish"]);
    assert!(err.contains("no full-text index") && err.contains("cv index"), "{err}");
    assert!(err.contains("--scan"), "{err}");
    assert!(out.is_empty(), "{out}");
    // `--scan` asks for the live scan, which announces itself and still finds the content.
    let (out, err) = w.cv_ok(&["search", "zebrafish", "--scan"]);
    assert!(err.contains("scanning every transcript live"), "{err}");
    assert!(out.contains("alphases"), "{out}");
    assert!(out.to_lowercase().contains("zebrafish"), "snippet expected:\n{out}");

    // Build the index; search must now use it (no live-scan note) and still hit.
    let (out, _) = w.cv_ok(&["index"]);
    assert!(out.contains("indexed 2 top-level session(s)"), "{out}");
    let (out, err) = w.cv_ok(&["search", "zebrafish"]);
    assert!(!err.contains("scanning"), "indexed search must not live-scan:\n{err}");
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

    // Live-scan path (`--scan`): the note stays on stderr; stdout is exactly one JSON
    // object, `{hits, total}`, each hit with the FULL session id (the table truncates to "alphases"), the snippet the
    // table would print, and an explicit null score (a live scan has none).
    let (out, err) = w.cv_ok(&["search", "zebrafish", "--json", "--scan"]);
    assert!(err.contains("scanning every transcript live"), "{err}");
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 1), "{out}");
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

    // A live-scan miss is empty hits — valid JSON, no prose on stdout.
    let (out, _) = w.cv_ok(&["search", "xyzzyplugh", "--json", "--scan"]);
    let (rows, total) = search_json(&out);
    assert!(rows.is_empty() && total == 0, "{out}");

    // --limit bounds the hits and `total` counts every match; the "N of M" footer goes to
    // stderr, keeping stdout pure. ("the" appears in both fixture sessions on the live path.)
    let (out, err) = w.cv_ok(&["search", "the", "--json", "--limit", "1", "--scan"]);
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 2), "{out}");
    assert!(err.contains("1 of 2 matches"), "{err}");
    let (out, _) = w.cv_ok(&["search", "the", "--limit", "1", "--scan"]);
    assert!(out.contains("1 of 2 matches; use --limit for more"), "{out}");

    // Indexed path: same hit, still the full id, now with a real BM25 score and dates off
    // the index.
    w.cv_ok(&["index"]);
    let (out, err) = w.cv_ok(&["search", "zebrafish", "--json"]);
    assert!(!err.contains("scanning"), "{err}");
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 1), "{out}");
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

    // Indexed miss and non-matching --harness filter: empty hits, still valid JSON.
    let (out, _) = w.cv_ok(&["search", "xyzzyplugh", "--json"]);
    let (rows, total) = search_json(&out);
    assert!(rows.is_empty() && total == 0, "{out}");
    let (out, _) = w.cv_ok(&["search", "zebrafish", "--json", "--harness", "codex"]);
    let (rows, total) = search_json(&out);
    assert!(rows.is_empty() && total == 0, "{out}");

    // The index counts past --limit too: every match, the filter applied.
    let (out, err) = w.cv_ok(&["search", "the", "--json", "--limit", "1"]);
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 2), "{out}");
    assert!(err.contains("1 of 2 matches"), "{err}");
    let (out, _) = w.cv_ok(&["search", "the", "--json", "--limit", "1", "--harness", "claude"]);
    let (_, filtered) = search_json(&out);
    assert!(filtered <= 2, "{out}");
    let (out, _) = w.cv_ok(&["search", "the", "--limit", "5"]);
    assert!(out.contains("2 of 2 matches") && !out.contains("use --limit"), "{out}");
}

/// `cv search --json`'s stdout: `{"hits": [...], "total": M}`, as its hits and total.
fn search_json(out: &str) -> (Vec<serde_json::Value>, u64) {
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    let hits = v["hits"].as_array().expect("a hits array").clone();
    (hits, v["total"].as_u64().expect("a total"))
}

/// The staleness disclosure: once the corpus has outrun the index, a hit list must say so (a
/// non-empty answer can still be missing rows), a miss must say so, and --json moves it to
/// stderr so stdout stays pure JSON — `cv mcp` relays stderr as its own content block.
#[test]
fn search_discloses_index_staleness() {
    let w = World::new("searchstale");
    standard_corpus(&w);
    w.cv_ok(&["index"]);

    // Fresh index: no disclosure on either surface.
    let (_out, err) = w.cv_ok(&["search", "zebrafish", "--json"]);
    assert!(!err.contains("behind"), "{err}");
    let (out, _) = w.cv_ok(&["search", "zebrafish"]);
    assert!(!out.contains("behind"), "{out}");

    // Grow one session's transcript: the index now trails one file.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(w.transcript("alphasess"))
            .unwrap();
        writeln!(
            f,
            "{}",
            user_line(
                "new-project-session",
                "u9",
                "2026-06-02T00:00:00Z",
                "more zebrafish tail work"
            )
        )
        .unwrap();
    }

    // The grown suffix is read live: the hit list finds the new text and says what it read; with
    // no --live-bytes nothing bounds the read, so the footer names no budget…
    let (out, _) = w.cv_ok(&["search", "tail"]);
    assert!(out.contains("alphases") && out.contains("read 1 session(s)"), "{out}");
    assert!(!out.contains("behind") && !out.contains("budget"), "{out}");
    // …--json keeps stdout pure and puts the footer on stderr…
    let (out, err) = w.cv_ok(&["search", "tail", "--json"]);
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 1), "{out}");
    assert!(err.contains("read 1 session(s)"), "{err}");
    // …a budget too small for the delta leaves it to the index, and a hit list or a miss says so…
    for query in ["zebrafish", "xyzzyplugh"] {
        let (out, _) = w.cv_ok(&["search", query, "--live-bytes", "10"]);
        assert!(
            out.contains("behind for 1 session") && out.contains("over the --live-bytes budget"),
            "{out}"
        );
    }
    let (out, _) = w.cv_ok(&["search", "tail", "--live-bytes", "10"]);
    assert!(out.contains("no matches"), "{out}");
    // …and --frozen answers from the index alone and says so.
    let (out, _) = w.cv_ok(&["--frozen", "search", "tail"]);
    assert!(out.contains("no matches") && out.contains("--frozen"), "{out}");
    // A host sets its default budget in the config ([search] live_bytes); the footer names it…
    let config = w.home.join(".config/clustervision/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "[search]\nlive_bytes = 10\n").unwrap();
    let (out, _) = w.cv_ok(&["search", "zebrafish"]);
    assert!(out.contains("over the config search.live_bytes budget"), "{out}");
    // …$CV_LIVE_BYTES overrides it (`all` lifts it), and --live-bytes overrides both…
    let (ok, _, out, err) = w.cv_env(&["search", "zebrafish"], &[("CV_LIVE_BYTES", "20")]);
    assert!(ok && out.contains("over the CV_LIVE_BYTES budget"), "{out}{err}");
    let (ok, _, out, err) = w.cv_env(&["search", "tail"], &[("CV_LIVE_BYTES", "all")]);
    assert!(
        ok && out.contains("read 1 session(s)") && !out.contains("budget"),
        "{out}{err}"
    );
    let (ok, _, out, err) = w.cv_env(
        &["search", "tail", "--live-bytes", "1048576"],
        &[("CV_LIVE_BYTES", "10")],
    );
    assert!(ok && out.contains("of the 1.0 MiB --live-bytes budget"), "{out}{err}");
    // …and a malformed variable is a usage error, not a silent fallback.
    let (ok, code, _, err) = w.cv_env(&["search", "tail"], &[("CV_LIVE_BYTES", "50MB")]);
    assert!(!ok && code == 2 && err.contains("CV_LIVE_BYTES=\"50MB\""), "{err}");
    std::fs::remove_file(&config).unwrap();

    // A fresh pass quiets it again.
    w.cv_ok(&["index"]);
    let (_, err) = w.cv_ok(&["search", "zebrafish", "--json"]);
    assert!(!err.contains("behind") && !err.contains("live:"), "{err}");
}

/// A sandbox that denies writes to the cv home (here the home made read-only, as a deny-write
/// profile makes it) still searches, lists and shows from the index and catalog it finds there.
/// Text appended after indexing is found by the live read of the suffix, which writes nothing;
/// `--frozen` answers from the index alone. Nothing announces a scan.
#[cfg(unix)]
#[test]
fn read_only_home_answers_from_the_index() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("searchro");
    standard_corpus(&w);
    w.cv_ok(&["index"]);
    let mut lines = fs::read_to_string(w.transcript("betasess")).unwrap();
    lines.push_str(&format!(
        "{}\n",
        user_line("new-project-session", "b9", "2026-03-02T09:00:00Z", "zqlatecomer")
    ));
    fs::write(w.transcript("betasess"), lines).unwrap();
    let set_mode = |mode: u32| {
        let mut dirs = vec![w.cv_home.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path.clone());
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                } else {
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o666)).unwrap();
                }
            }
        }
        fs::set_permissions(&w.cv_home, fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o555);
    let found = w.cv(&["search", "zebrafish", "--json"]);
    let late = w.cv(&["search", "zqlatecomer"]);
    let frozen = w.cv(&["--frozen", "search", "zqlatecomer"]);
    let listed = w.cv(&["ls", "--json"]);
    let shown = w.cv(&["show", "alphasess"]);
    set_mode(0o755);

    let (ok, _, out, err) = found;
    assert!(ok, "{err}");
    let (rows, total) = search_json(&out);
    assert_eq!((rows.len(), total), (1, 1), "{out}");
    assert_eq!(rows[0]["id"], "alphasess");
    assert!(
        rows[0]["score"].as_f64().is_some(),
        "an indexed hit carries its score: {out}"
    );
    assert!(!err.contains("scanning") && !err.contains("unusable"), "{err}");
    let (ok, _, out, err) = late;
    assert!(
        ok && out.contains("betases") && out.contains("read 1 session(s)") && !err.contains("scanning"),
        "the live suffix answered, not a scan:\n{out}\n{err}"
    );
    let (ok, _, out, err) = frozen;
    assert!(
        ok && out.contains("no matches"),
        "the index alone answered:\n{out}\n{err}"
    );
    let (ok, _, out, err) = listed;
    assert!(ok, "{err}");
    assert_eq!(
        serde_json::from_str::<Vec<serde_json::Value>>(&out).unwrap().len(),
        2,
        "{out}"
    );
    let (ok, _, out, err) = shown;
    assert!(ok && out.contains("zebrafish"), "{out}\n{err}");
}

/// An index that exists but cannot be opened fails at once, naming its path, the cause and both
/// ways forward; it never turns into a scan of every transcript.
#[test]
fn unusable_index_fails_fast_with_the_cause() {
    let w = World::new("searchbad");
    standard_corpus(&w);
    let index = w.cv_home.join("tantivy");
    fs::create_dir_all(&index).unwrap();
    fs::write(index.join("meta.json"), b"{not json").unwrap();
    let started = std::time::Instant::now();
    let (out, err) = w.cv_fails(&["search", "zebrafish"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("unusable") && err.contains(&*index.to_string_lossy()),
        "{err}"
    );
    assert!(err.contains("cv index") && err.contains("--scan"), "{err}");
    assert!(!err.contains("scanning"), "{err}");
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

/// `--thinking <native|text|drop|portable>` means the same thing on every command that emits a
/// session, and nothing else answers to that word. `rewind` writes Claude only and offers the two
/// it can honor (`native`, `portable`), says so in its help, and refuses the others. The modes differ in whether a turn whose only content is
/// provider-signed reasoning survives a target that cannot hold the signature.
#[test]
fn thinking_mode_is_one_word_on_every_emitting_command() {
    let w = World::new("think");
    standard_corpus(&w);

    // Present, with the same three values, wherever a session is written out.
    for cmd in ["port", "splice", "pack"] {
        let (ok, _, out, err) = w.cv(&[cmd, "--help"]);
        assert!(ok, "cv {cmd} --help failed:\n{err}");
        for mode in ["native", "text", "drop"] {
            assert!(
                out.contains(mode),
                "cv {cmd} --help must offer --thinking {mode}:\n{out}"
            );
        }
    }

    let (ok, _, out, err) = w.cv(&["rewind", "--help"]);
    assert!(ok, "cv rewind --help failed:\n{err}");
    for mode in ["native", "portable"] {
        assert!(
            out.contains(mode),
            "cv rewind --help must offer --thinking {mode}:\n{out}"
        );
    }
    assert!(
        out.contains("`text` and `drop` are"),
        "rewind's help names the values it refuses:\n{out}"
    );
    let (_, code, _, err) = w.cv(&["rewind", "alphasess", "--thinking", "drop"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("drop"), "the refused value is quoted back:\n{err}");

    // An unknown mode is rejected by name, listing what is valid.
    let (_, _, _, err) = w.cv(&["port", "alphasess", "--thinking", "sideways"]);
    assert!(err.contains("sideways"), "the bad value is quoted back:\n{err}");

    // And the word is NOT overloaded: prune's old boolean spelling is gone.
    let (_, code, _, err) = w.cv(&["prune", "alphasess", "--thinking"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--drop-thinking"), "{err}");
}

/// `--interval` is seconds as a number on both pollers; a negative one is a usage error.
#[test]
fn interval_is_seconds_and_negative_is_a_usage_error() {
    let w = World::new("interval");
    standard_corpus(&w);
    for cmd in ["scry", "watch"] {
        let (_, code, _, err) = w.cv(&[cmd, "--interval=-1"]);
        assert_eq!(code, 2, "cv {cmd}: {err}");
        assert!(err.contains("--interval"), "cv {cmd}: {err}");
    }
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
    assert!(out.contains("## user"), "{out}");
    assert!(out.contains("zebrafish migration"), "{out}");
    assert!(out.contains("**🔧 Edit**"), "{out}");
    // Explicit whole-range export keeps the window renderer as an independent output oracle.
    let (windowed, _) = w.cv_ok(&["export", "alphasess", "--format", "md", "--range", "0.."]);
    assert_eq!(out, windowed);
    let giant = "🌙\\\"\n".repeat(16384);
    let giant_path = w.write_session(
        "giant-export",
        &[user_line("giant-export", "g", "2026-01-01T10:00:00Z", &giant)],
    );
    let (full, _) = w.cv_ok(&["export", "giant-export", "--format", "md"]);
    let (windowed, _) = w.cv_ok(&["export", "giant-export", "--format", "md", "--range", "0.."]);
    assert_eq!(full, windowed);
    // Claude rejects malformed individual records without rejecting an otherwise valid session.
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(giant_path)
        .unwrap()
        .write_all(b"\xff\n")
        .unwrap();
    // Pin Claude so this assertion exercises its malformed-record policy explicitly.
    let (with_invalid_row, _) = w.cv_ok(&["export", "giant-export", "--format", "md", "--harness", "claude"]);
    assert_eq!(with_invalid_row, full);

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

    // HTML left `export` for `share` (the streaming, redacting renderer): the old spelling
    // points there and exits 2.
    let (ok, code, _, err) = w.cv(&["export", "alphasess", "--format", "html"]);
    assert!(!ok && code == 2, "{err}");
    assert!(err.contains("cv share <id>"), "{err}");

    // Unknown format is an error, not a silent default.
    let (_, err) = w.cv_fails(&["export", "alphasess", "--format", "docx"]);
    assert!(err.contains("unknown format"), "{err}");
}

// ───────────────────────────── prune --json ─────────────────────────────

/// `--drop-thinking` omits old reasoning outright: a thinking-only record goes, its child is
/// relinked, the transcript only shrinks, and `cv cat` still returns the dropped block.
#[test]
fn prune_drop_thinking_omits_blocks_and_keeps_them_retrievable() {
    let w = World::new("prunedropthinking");
    let record = |uuid: &str, parent: Option<&str>, role: &str, content: serde_json::Value| {
        serde_json::json!({"type": role, "uuid": uuid, "parentUuid": parent, "sessionId": "thinksrc",
            "timestamp": "2026-01-01T10:00:00Z", "cwd": "/work/proj",
            "message": {"role": role, "model": "claude-opus-5", "content": content}})
    };
    let reasoning = "weighing the options ".repeat(40);
    let source = w.write_session(
        "thinksrc",
        &[
            record("u1", None, "user", serde_json::json!("plan it")),
            record(
                "a1",
                Some("u1"),
                "assistant",
                serde_json::json!([
                {"type": "thinking", "thinking": reasoning, "signature": "s".repeat(600)}]),
            ),
            record(
                "a2",
                Some("a1"),
                "assistant",
                serde_json::json!([{"type": "text", "text": "planned"}]),
            ),
            record("u2", Some("a2"), "user", serde_json::json!("go on")),
            record(
                "a3",
                Some("u2"),
                "assistant",
                serde_json::json!([{"type": "text", "text": "going"}]),
            ),
        ],
    );
    let (out, _) = w.cv_ok(&["prune", "thinksrc", "--drop-thinking", "--keep-last", "1", "--json"]);
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let new_id = report["new_id"].as_str().unwrap();
    let path = source.with_file_name(format!("{new_id}.jsonl"));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.len() < fs::read_to_string(&source).unwrap().len());
    assert!(!text.contains("\"thinking\"") && !text.contains("[PRUNED"), "{text}");
    assert!(!text.contains("\"uuid\":\"a1\""), "the emptied record is removed");
    let (reached, threaded, _) = chain_walk(&path);
    assert_eq!(reached, threaded, "a dangling parent truncates the resumed context");
    let (cat, _) = w.cv_ok(&["cat", new_id, "a1#think0"]);
    assert!(cat.contains("weighing the options"), "{cat}");
}

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
    let (out, human) = w.cv_ok(&[
        "prune",
        "prunesrc",
        "--min-size",
        "100",
        "--keep-last",
        "0",
        "--dry-run",
        "--json",
    ]);
    assert!(human.contains("token equivalents removed (byte estimate)"), "{human}");
    assert!(
        human.contains("no recorded usage required an estimated-window rewrite"),
        "{human}"
    );
    assert!(!human.contains("resume gate will let you back in"), "{human}");
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

/// `--to` names a session that may already exist, or `<id>.flat`, another session's sidecar.
/// Either is refused (exit 2, nothing written); `--force` replaces by rename, never in place.
#[test]
fn prune_to_an_existing_session_is_refused_unless_forced() {
    let w = World::new("pruneexisting");
    let line = |uuid: &str, sid: &str, content: serde_json::Value| {
        serde_json::json!({"type": "user", "uuid": uuid, "sessionId": sid, "timestamp": "2026-01-01T10:00:00Z",
            "cwd": "/work/proj", "message": {"role": "user", "content": content}})
    };
    let big = "x".repeat(4096);
    w.write_session(
        "prunesrc",
        &[
            line("u1", "prunesrc", "go".into()),
            line(
                "u2",
                "prunesrc",
                serde_json::json!([{"type": "tool_result", "tool_use_id": "t1", "content": big}]),
            ),
        ],
    );
    let other = w.write_session("othersess", &[line("o1", "othersess", "keep me".into())]);
    let dir = other.parent().unwrap().to_owned();
    let prune = |to: &str, force: bool| {
        let mut args = vec!["prune", "prunesrc", "--min-size", "100", "--keep-last", "0", "--to", to];
        if force {
            args.push("--force");
        }
        w.cv(&args)
    };
    let listing = || {
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let other_before = fs::read(&other).unwrap();
    let names_before = listing();
    let (_, code, _, err) = prune("othersess", false);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read(&other).unwrap(), other_before);
    assert_eq!(listing(), names_before, "a refused prune writes nothing");

    let (ok, _, _, err) = prune("pruned", false);
    assert!(ok, "{err}");
    let sidecar = dir.join("pruned.flat.jsonl");
    let sidecar_before = fs::read(&sidecar).unwrap();
    let names_before = listing();
    let (_, code, _, err) = prune("pruned.flat", false);
    assert_eq!(code, 2, "{err}");
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        sidecar_before,
        "the sidecar's payloads stay retrievable"
    );
    assert_eq!(listing(), names_before);

    #[cfg(unix)]
    let held = {
        use std::os::unix::fs::MetadataExt;
        (fs::File::open(&other).unwrap(), fs::metadata(&other).unwrap().ino())
    };
    let (ok, _, _, err) = prune("othersess", true);
    assert!(ok, "{err}");
    assert!(fs::read_to_string(&other).unwrap().contains("[PRUNED"));
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let (mut reader, inode) = held;
        assert_ne!(
            fs::metadata(&other).unwrap().ino(),
            inode,
            "--force renames, never truncates in place"
        );
        let mut old = Vec::new();
        reader.read_to_end(&mut old).unwrap();
        assert_eq!(old, other_before, "a reader of the replaced file keeps its bytes");
    }
    assert!(!listing().iter().any(|n| n.starts_with(".cv-")), "{:?}", listing());
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

    let (empty, report) = w.cv_ok(&["dataset", "--limit", "0"]);
    assert!(
        empty.is_empty(),
        "a zero record limit must not publish the first record: {empty}"
    );
    assert!(report.contains("wrote 0 record(s)"), "{report}");

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

#[test]
fn dataset_file_publication_matches_stdout_and_preserves_transcript_aliases() {
    let w = World::new("dataset-file-publication");
    standard_corpus(&w);
    let (expected, _) = w.cv_ok(&["dataset"]);
    let output = w.base.join("training.jsonl");
    fs::write(&output, b"previous artifact").unwrap();
    let (_, code, _, error) = w.cv(&["dataset", "--file", output.to_str().unwrap()]);
    assert_eq!(code, 2, "{error}");
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(fs::read(&output).unwrap(), b"previous artifact");
    // `--out` is a directory everywhere else; the old spelling points at `--file`.
    let (_, code, _, error) = w.cv(&["dataset", "--out", output.to_str().unwrap()]);
    assert_eq!(code, 2, "{error}");
    assert!(error.contains("--file"), "{error}");
    let (stdout, _) = w.cv_ok(&["dataset", "--file", output.to_str().unwrap(), "--force"]);
    assert!(stdout.is_empty());
    assert_eq!(fs::read(&output).unwrap(), expected.as_bytes());

    // Even --force never replaces one of the dataset's own source transcripts.
    let source = w.transcript("alphasess");
    let original = fs::read(&source).unwrap();
    let (_, error) = w.cv_fails(&["dataset", "--limit", "1", "--file", source.to_str().unwrap(), "--force"]);
    assert!(error.contains("aliases source transcript"), "{error}");
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(w.cv_ok(&["dataset"]).0, expected);

    #[cfg(unix)]
    {
        let hardlink = w.base.join("source-hardlink.jsonl");
        fs::hard_link(&source, &hardlink).unwrap();
        let symlink = w.base.join("source-symlink.jsonl");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        for alias in [&hardlink, &symlink] {
            let (_, error) = w.cv_fails(&["dataset", "--file", alias.to_str().unwrap(), "--force"]);
            assert!(error.contains("aliases source transcript"), "{error}");
            assert_eq!(fs::read(alias).unwrap(), original);
        }
        assert!(symlink.is_symlink());
    }
}

/// `--force` replaces the name: a symlink there becomes the new file (its target untouched), the
/// old inode's other hard links keep the old bytes, and the replaced file's permissions carry over.
#[cfg(unix)]
#[test]
fn dataset_file_publication_replaces_the_name_not_the_linked_file() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let w = World::new("dataset-file-links");
    standard_corpus(&w);
    let (expected, _) = w.cv_ok(&["dataset"]);
    let target = w.base.join("target.jsonl");
    fs::write(&target, b"previous artifact").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let hardlink = w.base.join("output-hardlink.jsonl");
    fs::hard_link(&target, &hardlink).unwrap();
    let output = w.base.join("output-symlink.jsonl");
    symlink(&target, &output).unwrap();
    w.cv_ok(&["dataset", "--file", output.to_str().unwrap(), "--force"]);
    assert!(!output.is_symlink());
    assert_eq!(fs::read(&output).unwrap(), expected.as_bytes());
    assert_eq!(fs::metadata(&output).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read(&target).unwrap(), b"previous artifact");
    w.cv_ok(&["dataset", "--file", target.to_str().unwrap(), "--force"]);
    assert_eq!(fs::read(&target).unwrap(), expected.as_bytes());
    assert_eq!(fs::read(&hardlink).unwrap(), b"previous artifact");
}

/// Files the user names (`distill --pack`, `share --out`) are refused when taken (exit 2,
/// untouched) and replaced whole with `--force`.
#[test]
fn named_output_files_are_refused_unless_forced() {
    let w = World::new("named-outputs");
    standard_corpus(&w);
    let pack = w.base.join("pack.md");
    let html = w.base.join("shared.html");
    for (path, args) in [
        (&pack, vec!["distill", "alphasess", "--pack"]),
        (&html, vec!["share", "alphasess", "--out"]),
    ] {
        fs::write(path, b"keep me").unwrap();
        let mut argv = args.clone();
        argv.push(path.to_str().unwrap());
        let (_, code, _, error) = w.cv(&argv);
        assert_eq!(code, 2, "{argv:?}: {error}");
        assert!(error.contains("--force"), "{error}");
        assert_eq!(fs::read(path).unwrap(), b"keep me", "{argv:?}");
        argv.push("--force");
        let (ok, _, _, error) = w.cv(&argv);
        assert!(ok, "{argv:?}: {error}");
        assert_ne!(fs::read(path).unwrap(), b"keep me", "{argv:?}");
    }
    assert!(!fs::read_dir(&w.base)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".cv-")));
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

// ───────────────────────────── export --redact ─────────────────────────────

#[test]
fn export_redact_scrubs_planted_secret() {
    let w = World::new("redact");
    standard_corpus(&w);

    // The secret IS in the raw transcript (sanity), and `cv show` prints it.
    let (out, _) = w.cv_ok(&["show", "alphasess"]);
    assert!(out.contains(SECRET), "fixture sanity: secret present in show:\n{out}");

    // md: gone from stdout, replaced by a placeholder; the per-class tally is on stderr.
    let (out, err) = w.cv_ok(&["export", "alphasess", "--redact"]);
    assert!(!out.contains(SECRET), "export --redact leaked the secret:\n{out}");
    assert!(out.contains("[REDACTED:api_key]"), "{out}");
    assert!(out.contains("## user"), "still the export rendering: {out}");
    assert!(err.contains("redacted") && err.contains("1 api_key"), "{err}");

    // json: the whole OpenSession document is clean, and still the document (marker led).
    let (out, err) = w.cv_ok(&["export", "alphasess", "--format", "json", "--redact"]);
    assert!(
        !out.contains(SECRET),
        "export --format json --redact leaked the secret:\n{out}"
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["open_session"], "0.3", "{out}");
    assert_eq!(v["id"], "alphasess");
    assert!(err.contains("1 api_key"), "{err}");

    // Without the flag nothing is scrubbed and nothing is tallied.
    let (out, err) = w.cv_ok(&["export", "alphasess"]);
    assert!(out.contains(SECRET), "{out}");
    assert!(!err.contains("redacted"), "{err}");

    // The removed command errors with the replacement, exit 2.
    let (ok, code, _, err) = w.cv(&["redact", "alphasess", "--stats"]);
    assert!(!ok && code == 2, "{err}");
    assert!(err.contains("cv export <id> --redact"), "{err}");
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

    let (_, err) = w.cv_fails(&["show", "nonexistent-id", "--tree"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["resume", "nonexistent-id"]);
    assert!(err.contains("no session matching"), "{err}");

    let (_, err) = w.cv_fails(&["show", "alphasess", "--harness", "marsrover"]);
    assert!(err.contains("unknown harness"), "{err}");

    // Every name this release removed: gone, but never silently — each one exits 2 (a caller
    // mistake, not a failure) with a line naming the version and the replacement. The 0.11
    // tombstones left at 0.15; these leave at the next release.
    let removed: &[(&[&str], &str)] = &[
        // `timeline` was `ls` with the shown window reversed.
        (&["timeline", "--limit", "5"], "cv ls --sort-by updated --asc"),
        // `redact` was `export` with the scrubber in front.
        (&["redact", "alphasess", "--format", "json"], "cv export <id> --redact"),
        // `diff` was a role+text prefix compare; export both and use a real diff.
        (&["diff", "alphasess", "betasess"], "cv export <id> --format md"),
        // `export --format html` had a second, non-streaming renderer; `share` is the one.
        (&["export", "alphasess", "--format", "html"], "cv share <id>"),
        // `adopt --list/--orphans` were lanes tables.
        (&["adopt", "--list", "alphasess"], "cv lanes alphasess"),
        (&["adopt", "--orphans"], "cv lanes --recent <N> --unfinished"),
        // `formats check` needed a checkout: it is the test; `census` is plain `formats`.
        (
            &["formats", "check"],
            "cargo test -p clustervision-core --test formats_manifest",
        ),
        (&["formats", "census"], "cv formats"),
        // `fork` was `splice <id>:..N`; `loom` was a two-span `splice`.
        (
            &["fork", "alphasess", "--at", "2", "--session"],
            "cv splice <id>:..N --session",
        ),
        (
            &["loom", "alphasess", "--at", "2", "--graft", "betasess", "--from", "1"],
            "cv splice <base>:..N <other>:M..",
        ),
        // `tree`, `prompts` and `compaction` were views of one session: `show` flags now.
        (&["tree", "alphasess"], "cv show <id> --tree"),
        (&["prompts", "alphasess", "--json"], "cv show <id> --kind prompt,answer"),
        (
            &["compaction", "alphasess", "--summaries"],
            "cv show <id> --kind compaction_boundary,compaction_summary",
        ),
    ];
    for (args, pointer) in removed {
        let (ok, code, out, err) = w.cv(args);
        assert!(!ok, "cv {args:?} must not still work\nstdout:\n{out}");
        assert_eq!(code, 2, "cv {args:?} must exit 2 (caller mistake)\nstderr:\n{err}");
        assert!(!err.contains("panicked"), "cv {args:?} panicked:\n{err}");
        assert!(
            err.contains("0.15.0") || err.contains("is now"),
            "cv {args:?} must say when it went away:\n{err}"
        );
        assert!(err.contains(pointer), "cv {args:?} must point at {pointer:?}:\n{err}");
    }
    // The 0.11 tombstones are plain unknown names now: clap's error, still exit 2, no pointer.
    let (ok, code, _, err) = w.cv(&["convert", "alphasess"]);
    assert!(!ok && code == 2 && err.contains("unrecognized subcommand"), "{err}");
    let (ok, code, _, err) = w.cv(&["prune", "alphasess", "--retrieve", "t1"]);
    assert!(!ok && code == 2 && err.contains("unexpected argument"), "{err}");
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

    // A path that exists nowhere, outside any git repo: blame degrades to catalog-only mode,
    // says so first, and finds no session that edited it.
    let (ok, code, out, err) = w.cv(&["blame", "/not/a/file"]);
    assert!(
        ok && code == 0,
        "blame degrades, never fails\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        out.starts_with("(not inside a git repository — event-catalog matches only, no commit correlation)\n"),
        "blame names its mode first:\n{out}"
    );
    // Nothing was indexed in this world, so the catalog has no edit events to match.
    assert!(
        out.contains("no edit events found — run `cv index` first"),
        "no session edited it:\n{out}"
    );
}

// ───────────────────────────── show --tree ─────────────────────────────

#[test]
fn show_tree_renders() {
    let w = World::new("tree");
    standard_corpus(&w);

    let (out, _) = w.cv_ok(&["show", "alphasess", "--tree"]);
    assert!(out.contains("# alpha adventures"), "{out}");
    assert!(out.contains("3 msg"), "{out}");
    w.write_session(
        "cycle-tree",
        &[
            claude::with(
                user_line("cycle-tree", "c1", "2026-01-01T10:00:00Z", "first-cycle"),
                serde_json::json!({"parentUuid":"c2"}),
            ),
            claude::with(
                user_line("cycle-tree", "c2", "2026-01-01T10:01:00Z", "second-cycle"),
                serde_json::json!({"parentUuid":"c1"}),
            ),
        ],
    );
    let (out, _) = w.cv_ok(&["show", "cycle-tree", "--tree"]);
    assert!(out.contains("2 msg"), "{out}");
    assert!(out.contains("• user  first-cycle"), "{out}");
    assert!(out.contains("• user  second-cycle"), "{out}");
}

// ───────────────────────────── stats / ls --asc ─────────────────────────────

#[test]
fn stats_and_ls_asc() {
    let w = World::new("stats");
    standard_corpus(&w);

    let (out, _) = w.cv_ok(&["stats"]);
    assert!(out.contains("2 session(s) · 6 message(s)"), "{out}");
    assert!(out.contains("claude"), "{out}");
    // `ls` is newest first; `--asc` prints the same rows oldest → newest (the old `timeline` feed).
    let (out, _) = w.cv_ok(&["ls"]);
    assert!(pos(&out, "alphases") < pos(&out, "betasess"), "{out}");
    let (out, _) = w.cv_ok(&["ls", "--sort-by", "updated", "--asc"]);
    assert!(out.contains("2 session(s)"), "{out}");
    assert!(pos(&out, "betasess") < pos(&out, "alphases"), "{out}");
    // `--asc` flips the order of the shown window, never its membership: `--limit 1` is still
    // the newest session, printed alone.
    let (out, _) = w.cv_ok(&["ls", "--asc", "--limit", "1", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0]["id"], "alphasess", "{out}");

    // The removed command errors with the replacement, exit 2.
    let (ok, code, _, err) = w.cv(&["timeline"]);
    assert!(!ok && code == 2, "{err}");
    assert!(err.contains("cv ls --sort-by updated --asc"), "{err}");
}

#[test]
fn stats_reports_uncounted_sessions_beside_the_total() {
    let w = World::new("stats-uncounted");
    standard_corpus(&w);
    let (out, _) = w.cv_ok(&["stats"]);
    assert!(out.contains("2 session(s) · 6 message(s)\n"), "{out}");
    // A Codex listing defers its count: the sum stays honest and says what it left out.
    let dir = w.home.join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    let id = "019a0000-0000-7000-8000-0000000000aa";
    let rollout = [
        serde_json::json!({"timestamp": "2026-01-01T10:00:00Z", "type": "session_meta",
            "payload": {"id": id, "cwd": "/work/proj", "timestamp": "2026-01-01T10:00:00Z"}}),
        serde_json::json!({"timestamp": "2026-01-01T10:00:01Z", "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
    ];
    std::fs::write(
        dir.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        cv_testkit::jsonl(&rollout),
    )
    .unwrap();
    let (out, _) = w.cv_ok(&["stats"]);
    assert!(out.contains("3 session(s) · 6 message(s) · 1 not counted yet"), "{out}");
    let (out, _) = w.cv_ok(&["stats", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (v["messages"].as_u64(), v["uncounted"].as_u64()),
        (Some(6), Some(1)),
        "{out}"
    );
    // The table prints a dash for it, and `--sort-by messages` ranks it after every counted row.
    let (out, _) = w.cv_ok(&["ls", "--sort-by", "messages"]);
    assert!(out.contains("– msg"), "{out}");
    assert!(pos(&out, "betasess") < pos(&out, "alphases"), "{out}");
    assert!(pos(&out, "alphases") < pos(&out, "019a0000"), "{out}");
    let (out, _) = w.cv_ok(&["ls", "--json", "--harness", "codex"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(rows[0]["message_count"].is_null(), "{out}");
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
            user_line("toksess", "u1", "2026-03-01T09:00:00Z", "go"),
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
    let seat_line = user_line("seatsess", "s1", "2026-07-01T10:00:00Z", "the seat asks about narwhals");
    fs::write(seat_proj.join("seatsess.jsonl"), format!("{seat_line}\n")).unwrap();
    let env_proj = w.base.join("elsewhere/projects/-work-proj");
    fs::create_dir_all(&env_proj).unwrap();
    let env_line = user_line(
        "envsess",
        "e1",
        "2026-07-02T10:00:00Z",
        "the env root asks about axolotls",
    );
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
    w.cv_ok(&["index"]);
    let (out, _) = w.cv_ok(&["search", "narwhals"]);
    assert!(out.contains("seatsess"), "the index covers the extra root:\n{out}");

    let roots = w.base.join("elsewhere");
    let (ok, _, out, err) = w.cv_env(
        &["show", "envsess"],
        &[("CLUSTERVISION_CLAUDE_ROOTS", roots.to_str().unwrap())],
    );
    assert!(ok, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("axolotls"), "{out}");
}

// ───────────────────────────── task substrate: identity + sanitizing ─────────────────────────────

/// The id of the one task titled `title`, read from the task projection
/// (`task list --all --json`), not from the human line `open`/`decide` print.
fn task_id_titled(w: &World, title: &str) -> String {
    let (json, _) = w.cv_ok(&["task", "list", "--all", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let ids: Vec<&str> = tasks
        .iter()
        .filter(|t| t["title"] == title)
        .map(|t| t["task_id"].as_str().expect("task_id"))
        .collect();
    assert_eq!(ids.len(), 1, "one task titled {title:?}:\n{json}");
    ids[0].to_string()
}

/// G4: identity is environment, not assertion — CV_ENDPOINT is the default `--from`, an explicit
/// flag still wins, and an identity-bearing verb with no resolvable identity is a hard error.
#[test]
fn task_identity_resolution_order() {
    let w = World::new("task-ident");

    // Open needs no ceremony: bare command records "cv".
    let (out, _) = w.cv_ok(&["task", "open", "first task"]);
    let id = task_id_titled(&w, "first task");
    assert!(out.lines().any(|l| l.trim() == id), "open prints its id:\n{out}");
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
    w.cv_ok(&["task", "open", bomb]);
    let id = task_id_titled(&w, bomb);

    let (out, _) = w.cv_ok(&["task", "list"]);
    assert!(!out.contains('\u{1b}'), "list leaks ESC:\n{out:?}");
    assert!(out.contains("eviltitle red line"), "stripped title renders:\n{out}");
    // The age column, read from the script format (`id state assignee repo age title blocked_by`).
    let (tsv, _) = w.cv_ok(&["task", "list", "--tsv"]);
    let row: Vec<&str> = tsv
        .lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .find(|f| f[0] == id)
        .unwrap_or_else(|| panic!("no row for {id}:\n{tsv}"));
    let age = row[4];
    assert!(
        age.len() >= 2
            && age[..age.len() - 1].bytes().all(|b| b.is_ascii_digit())
            && age.ends_with(['s', 'm', 'h', 'd']),
        "age is a count and a unit: {age:?} in {row:?}"
    );
    assert!(!row[5].contains('\u{1b}'), "tsv title leaks ESC: {row:?}");

    let (out, _) = w.cv_ok(&["task", "show", &id]);
    assert!(!out.contains('\u{1b}'), "show leaks ESC:\n{out:?}");

    // Assign it so the inbox has a row, then check the inbox strips too.
    let (ok, ..) = w.cv_env(&["task", "claim", &id], &[("CV_ENDPOINT", "agent:demo")]);
    assert!(ok);
    let (out, _) = w.cv_ok(&["task", "inbox", "agent:demo"]);
    assert!(!out.contains('\u{1b}'), "inbox leaks ESC:\n{out:?}");
    assert!(out.contains("claimed work (1):"), "inbox groups by reason:\n{out}");
}

/// The land facet's verbs and flags went away in 0.15.0: each old spelling errors with a pointer
/// and exit 2, and `done` labels itself self-reported.
#[test]
fn task_land_facet_verbs_and_flags_are_removed_with_pointers() {
    let w = World::new("task-land-removed");
    w.cv_ok(&["task", "open", "finish it"]);
    let id = task_id_titled(&w, "finish it");
    let removed: &[&[&str]] = &[
        &["task", "propose", &id, "--branch", "x"],
        &["task", "reroute", &id, "--to", "y"],
        &["task", "pass", &id],
        &["task", "refute", &id],
        &["task", "verify", "--all"],
        &["task", "debt"],
        &["task", "stats"],
        &["task", "claim", &id, "--from", "a", "--token", "t"],
        &["task", "done", &id, "--check-cmd", "true"],
        &["task", "split", &id, "--dry-run"],
        &["task", "watch", "ember"],
    ];
    for args in removed {
        let (ok, code, out, err) = w.cv(args);
        assert!(!ok, "cv {args:?} must not still work\nstdout:\n{out}");
        assert_eq!(code, 2, "cv {args:?} must exit 2 (caller mistake)\nstderr:\n{err}");
        assert!(
            err.contains("removed in 0.15.0"),
            "cv {args:?} must say when it went away:\n{err}"
        );
        assert!(
            err.contains(" — ") && err.matches('`').count() >= 2,
            "cv {args:?} must point at a replacement:\n{err}"
        );
    }
    let (out, _) = w.cv_ok(&["task", "list", "--all"]);
    assert!(
        out.contains("open"),
        "none of the refused commands wrote anything:\n{out}"
    );
    w.cv_ok(&["task", "done", &id, "--observed", "tests green in sess-1"]);
    let (out, _) = w.cv_ok(&["task", "show", &id]);
    assert!(
        out.contains("done:     self-reported") && out.contains("tests green in sess-1"),
        "a completion is labeled as the agent's own word:\n{out}"
    );
}

/// `cv board` went away in 0.15.0: every spelling errors with a pointer at the task log, exit 2.
#[test]
fn board_is_removed_with_a_pointer() {
    let w = World::new("board-removed");
    for args in [
        &["board", "post", "general", "hi"][..],
        &["board", "read", "general"],
        &["board", "channels"],
    ] {
        let (ok, code, out, err) = w.cv(args);
        assert!(!ok, "cv {args:?} must not still work\nstdout:\n{out}");
        assert_eq!(code, 2, "cv {args:?} must exit 2\nstderr:\n{err}");
        assert!(
            err.contains("removed in 0.15.0") && err.contains("cv task events"),
            "{err}"
        );
    }
    assert!(!w.cv_home.join("board").exists(), "nothing wrote a board dir");
}

// ───────────────────────── cv task ergonomics (0.12.0) ─────────────────────────

/// `--body-file`, `--tags`/`list --tag`, `--blocked-by`/`--blocks` with the blocked marker and
/// `show`'s relation lines, `note --file`, `--tsv`.
#[test]
fn task_open_relations_tags_and_file_bodies() {
    let w = World::new("task-ergo");
    let brief = w.base.join("brief.md");
    fs::write(
        &brief,
        "# The brief\n\nA body that is a document, not a shell string.\n",
    )
    .unwrap();

    w.cv_ok(&[
        "task",
        "open",
        "FOUNDATION: the thing others wait on",
        "--tags",
        "lean,deploy",
    ]);
    let a = task_id_titled(&w, "FOUNDATION: the thing others wait on");
    w.cv_ok(&[
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
    let b = task_id_titled(&w, "DEPENDENT: needs the foundation");
    w.cv_ok(&["task", "open", "UPSTREAM: blocks the foundation", "--blocks", &a]);
    let c = task_id_titled(&w, "UPSTREAM: blocks the foundation");

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

    // `--wide` went away in 0.15.0; the flag points at `show --brief`.
    let (ok, code, _, err) = w.cv(&["task", "list", "--wide"]);
    assert!(!ok && code == 2, "{err}");
    assert!(
        err.contains("removed in 0.15.0") && err.contains("show <id> --brief"),
        "{err}"
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
    w.cv_ok(&["task", "tag", &b, "deploy"]);
    let (json, _) = w.cv_ok(&["task", "show", &b, "--json"]);
    assert!(json.contains("\"deploy\""), "{json}");
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
    w.cv_ok(&[
        "task",
        "open",
        "K-RAN: run claim",
        "--repo",
        r,
        "--body",
        "one commit on branch `k-ran`, not pushed",
    ]);
    let merged = task_id_titled(&w, "K-RAN: run claim");
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
    w.cv_ok(&[
        "task",
        "open",
        "GONE: issue file deleted",
        "--repo",
        r,
        "--issue",
        "docs/gone.md",
    ]);
    let gone = task_id_titled(&w, "GONE: issue file deleted");
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

/// `inbox` orders decisions, then assigned actions, then claimed work; `--md` renders the whole
/// inbox as one Markdown page (bodies, the first line of each note) — pinned as a normalized golden.
#[test]
fn task_inbox_orders_decisions_first_and_renders_markdown() {
    let w = World::new("inbox-md");
    w.cv_ok(&["task", "open", "the claimed one", "--body", "claimed body"]);
    let c = task_id_titled(&w, "the claimed one");
    assert!(w.cv_env(&["task", "claim", &c], &[("CV_ENDPOINT", "ember")]).0);
    w.cv_ok(&[
        "task",
        "open",
        "write the docs",
        "--assignee",
        "ember",
        "--body",
        "First line of the action.\n\nSecond paragraph.",
    ]);
    let a = task_id_titled(&w, "write the docs");
    w.cv_ok(&[
        "task",
        "note",
        &a,
        "a note with two lines\nsecond line hidden in md",
        "--from",
        "orchestrator:x",
    ]);
    w.cv_ok(&[
        "task",
        "decide",
        "P6: WHO MAY REFILL a purse",
        "--for",
        "ember",
        "--default",
        "keep open",
        "--option",
        "per-refill consent",
        "--body",
        "today anyone may fund any purse.",
        "--from",
        "orchestrator:x",
    ]);
    let d = task_id_titled(&w, "P6: WHO MAY REFILL a purse");
    plant_tag_only_decision(
        &w,
        "01a0f000-0000-7000-8000-00000000000b",
        "tagged, never posed",
        "ember",
    );

    let (out, _) = w.cv_ok(&["task", "inbox", "ember"]);
    let dec = out.find("decisions owed (1):").expect(&out);
    let assigned = out.find("assigned actions (2):").expect(&out);
    let claimed = out.find("claimed work (1):").expect(&out);
    assert!(dec < assigned && assigned < claimed, "group order:\n{out}");
    assert!(assigned < out.find("tagged, never posed").unwrap(), "{out}");

    let (md, _) = w.cv_ok(&["task", "inbox", "ember", "--md"]);
    let got = normalize(&md);
    let expected = "\
# Inbox for ember — <ts>

1 decision(s) owed · 2 assigned action(s) · 1 claimed · 3 unread

## Decisions owed (1)

### 1. P6: WHO MAY REFILL a purse

`<id>` · open · asked <age> ago by orchestrator:x · **unread** (last: orchestrator:x)

- **default:** keep open
- alternative: per-refill consent
- resolve: `cv task resolve <id> --accept-default --from ember`

today anyone may fund any purse.

## Assigned actions (2)

### 2. write the docs

`<id>` · open · waiting <age> ago by cv · **unread** (last: orchestrator:x)

First line of the action.

Second paragraph.

notes (1):

- _orchestrator:x, <ts>:_ a note with two lines

### 3. tagged, never posed

`<id>` · open · waiting <age> ago by cv · **unread** (last: cv)

## Claimed work (1)

### 4. the claimed one

`<id>` · claimed · waiting <age> ago by cv

claimed body

";
    assert_eq!(got, expected, "--md golden (normalized):\n{md}");
    let _ = d;
}

/// `show --notes-last N` keeps the newest N notes, `--notes-grep` the matching ones; both say how
/// many of the total they kept.
#[test]
fn task_show_notes_last_and_grep() {
    let w = World::new("notes-filter");
    w.cv_ok(&[
        "task",
        "open",
        "EMBER: everything only ember can do",
        "--assignee",
        "ember",
    ]);
    let parent = task_id_titled(&w, "EMBER: everything only ember can do");
    for n in [
        "move the build base from /tank (slow) to NVMe?",
        "PUSH also ~/dev/cv: one unsigned commit.",
        "K-PORTAL: who births a guest cell",
        "JOIN-SOLANA works.",
    ] {
        w.cv_ok(&["task", "note", &parent, n, "--from", "orchestrator:x"]);
    }
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--brief"]);
    assert!(out.contains("notes:    4 of 4 (newest last) — one line each"), "{out}");
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--brief", "--notes-last", "2"]);
    assert!(out.contains("notes:    2 of 4"), "{out}");
    assert!(
        !out.contains("move the build base") && out.contains("JOIN-SOLANA"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["task", "show", &parent, "--notes-grep", "k-portal"]);
    assert!(out.contains("notes:    1 of 4") && out.contains("K-PORTAL"), "{out}");
}

/// `note <id> -` and `--body -` read stdin; a relative `--issue` is absolutized at open time
/// (canonicalized when it exists); handles and URLs pass through.
#[test]
fn task_note_reads_stdin_and_open_absolutizes_issue() {
    let w = World::new("stdin-paths");
    fs::write(w.base.join("PLAN.md"), "# plan\n").unwrap();
    let (ok, _, _, err) = w.cv_stdin(
        &[
            "task",
            "open",
            "with a heredoc body",
            "--body",
            "-",
            "--issue",
            "PLAN.md",
        ],
        "a `code span` body\nwith two lines\n",
    );
    assert!(ok, "{err}");
    let a = task_id_titled(&w, "with a heredoc body");
    let (json, _) = w.cv_ok(&["task", "show", &a, "--json"]);
    let t: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(t["body"], "a `code span` body\nwith two lines\n");
    let issue = t["issue"].as_str().unwrap();
    assert!(
        std::path::Path::new(issue).is_absolute() && issue.ends_with("PLAN.md"),
        "{issue}"
    );
    assert_eq!(
        std::path::Path::new(issue).canonicalize().unwrap(),
        w.base.join("PLAN.md").canonicalize().unwrap()
    );

    let (ok, _, _, err) = w.cv_stdin(&["task", "note", &a, "-"], "note from stdin\n");
    assert!(ok, "{err}");
    let (out, _) = w.cv_ok(&["task", "show", &a]);
    assert!(out.contains("note from stdin"), "{out}");

    w.cv_ok(&["task", "open", "handle", "--issue", "#42"]);
    let b = task_id_titled(&w, "handle");
    let (json, _) = w.cv_ok(&["task", "show", &b, "--json"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"],
        "#42"
    );
    w.cv_ok(&["task", "open", "url", "--issue", "https://x.y/issues/4"]);
    let c = task_id_titled(&w, "url");
    let (json, _) = w.cv_ok(&["task", "show", &c, "--json"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"],
        "https://x.y/issues/4"
    );
    // A relative path that does not exist yet is still rooted at the cwd.
    w.cv_ok(&["task", "open", "future doc", "--issue", "docs/later.md"]);
    let d = task_id_titled(&w, "future doc");
    let (json, _) = w.cv_ok(&["task", "show", &d, "--json"]);
    let issue = serde_json::from_str::<serde_json::Value>(&json).unwrap()["issue"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        issue.starts_with(w.base.to_str().unwrap())
            || issue.starts_with(&w.base.canonicalize().unwrap().display().to_string()),
        "{issue}"
    );
}

fn seeded_old_and_new(w: &World) -> (String, String) {
    let old = "01900000-0000-7000-8000-000000000001".to_string(); // uuid v7 from 2024
    let new = "01a00000-0000-7000-8000-000000000002".to_string();
    w.seed_task_log(&[
        serde_json::json!({"id": old, "task_id": old, "ts": "2026-01-01T00:00:00Z", "by": "orchestrator:old",
            "event": "opened", "title": "OLD: from another project", "channel": "tasks", "assignee": "ember"}),
        serde_json::json!({"id": new, "task_id": new, "ts": cv_testkit::now_rfc3339(), "by": "orchestrator:new",
            "event": "opened", "title": "NEW: today's task", "channel": "tasks", "assignee": "ember"}),
    ]);
    (old, new)
}

/// A bare `list`/`inbox` is scoped to the last 14 days (or tasks involving `$CV_ENDPOINT`) and
/// says how many it hid; `--all` and `--since` lift the window.
#[test]
fn task_list_and_inbox_scope_to_recent_and_all_shows_old() {
    let w = World::new("scope");
    let (old, new) = seeded_old_and_new(&w);
    let (out, _) = w.cv_ok(&["task", "list"]);
    assert!(out.contains("NEW: today") && !out.contains("OLD: from"), "{out}");
    assert!(
        out.trim_end()
            .ends_with("(1 older task(s) hidden — `--all`, or `--since 90d`)"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["task", "list", "--all"]);
    assert!(
        out.contains("OLD: from") && out.contains("NEW: today") && !out.contains("hidden"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["task", "list", "--since", "2025-12-01"]);
    assert!(out.contains("OLD: from") && !out.contains("hidden"), "{out}");
    let (out, _) = w.cv_ok(&["task", "list", "--since", "1d"]);
    assert!(!out.contains("OLD: from") && out.contains("hidden"), "{out}");
    // The caller's own tasks escape the window.
    let (ok, _, out, _) = w.cv_env(&["task", "list"], &[("CV_ENDPOINT", "orchestrator:old")]);
    assert!(ok && out.contains("OLD: from") && !out.contains("hidden"), "{out}");
    let (ok, _, out, _) = w.cv_env(&["task", "list"], &[("CV_ENDPOINT", "web:ember")]);
    assert!(
        ok && out.contains("OLD: from"),
        "web:<who> ≡ <who>, the assignee:\n{out}"
    );

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
    w.cv_ok(&[
        "task",
        "open",
        "A: touched by the orchestrator last",
        "--assignee",
        "ember",
        "--from",
        "orchestrator:x",
    ]);
    let a = task_id_titled(&w, "A: touched by the orchestrator last");
    w.cv_ok(&[
        "task",
        "open",
        "B: ember spoke last",
        "--assignee",
        "ember",
        "--from",
        "orchestrator:x",
    ]);
    let b = task_id_titled(&w, "B: ember spoke last");
    w.cv_ok(&["task", "note", &b, "on it", "--from", "ember"]);
    w.cv_ok(&[
        "task",
        "open",
        "C: web ember spoke last",
        "--assignee",
        "ember",
        "--from",
        "orchestrator:x",
    ]);
    let c = task_id_titled(&w, "C: web ember spoke last");
    w.cv_ok(&["task", "note", &c, "via the page", "--from", "web:ember"]);

    let (out, _) = w.cv_ok(&["task", "inbox", "ember", "--unread"]);
    assert!(
        out.contains("A: touched") && !out.contains("B: ember") && !out.contains("C: web"),
        "{out}"
    );
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
    assert_eq!(
        out.lines().count(),
        2,
        "web:ember is ember acting through the page:\n{out}"
    );
    let (out, _) = w.cv_ok(&["task", "events", "--not-by", "orchestrator:x", "--text"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(
        out.contains("noted") && out.contains("on it") && out.contains("via the page"),
        "{out}"
    );
    let (out, _) = w.cv_ok(&["task", "events", "--task", &b]);
    assert_eq!(out.lines().count(), 2, "{out}");
    // An unknown kind is a usage error naming the known tags, not an empty feed.
    let (_, code, _, err) = w.cv(&["task", "events", "--kind", "note,bogus"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("bogus") && err.contains("status_set"), "{err}");
    let (_, err) = w.cv_fails(&["task", "events", "--since", "whenever"]);
    assert!(err.contains("cannot read"), "{err}");

    // What ember did on their tasks, minus the caller's own events: `events` with two flags.
    let (ok, _, out, _) = w.cv_env(
        &[
            "task",
            "events",
            "--assignee",
            "ember",
            "--not-by",
            "orchestrator:x",
            "--since",
            "1d",
        ],
        &[("CV_ENDPOINT", "orchestrator:x")],
    );
    assert!(ok);
    let rows: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(rows.iter().all(|r| r["event"] == "noted"), "{out}");
    // `watch` was that query under a second name; the name errors with the spelling.
    let (ok, code, _, err) = w.cv(&["task", "watch", "--assignee", "ember"]);
    assert!(!ok && code == 2, "{err}");
    assert!(err.contains("removed in 0.15.0") && err.contains("--not-by"), "{err}");
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

    // `--model` rewrites the new agent's meta `model`; it needs `--agent-of` (exit 2 without).
    let (out, _) = w.cv_ok(&[
        "distill",
        agent_s,
        "--agent-of",
        "rootsess",
        "--model",
        "inherit",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let meta = sub.join(format!(
        "agent-{}.meta.json",
        v["emitted"]["agent_id"].as_str().unwrap()
    ));
    let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(meta).unwrap()).unwrap();
    assert_eq!(meta["model"], "inherit", "{meta}");
    let (ok, code, _, _) = w.cv(&["distill", agent_s, "--session", "--model", "opus"]);
    assert!(!ok && code == 2);

    // A fork is a splice of the verbatim prefix, to the same two destinations. `..2` strands `t1`
    // (its result is message 2) and the emitter drops the call, so the session resumes whole.
    let spec = format!("{agent_s}:..2");
    let (out, _) = w.cv_ok(&["splice", &spec, "--session", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("stdout must be pure JSON");
    let path = PathBuf::from(v["path"].as_str().unwrap());
    let (reached, threaded, root) = chain_walk(&path);
    assert_eq!(reached, threaded);
    assert_eq!(root["uuid"], "u0", "{root}");
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("\"t1\"") && !text.contains("\"isSidechain\":true"),
        "{text}"
    );
    let (out, _) = w.cv_ok(&["splice", &spec, "--agent-of", "rootsess", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let aid = v["agent_id"].as_str().unwrap();
    let apath = sub.join(format!("agent-{aid}.jsonl"));
    assert!(apath.exists() && sub.join(format!("agent-{aid}.meta.json")).exists());
    for l in fs::read_to_string(&apath).unwrap().lines() {
        let r: serde_json::Value = serde_json::from_str(l).unwrap();
        assert!(
            r["isSidechain"] == true && r["agentId"] == aid && r["sessionId"] == "rootsess",
            "{l}"
        );
    }
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

    // Finding the agents is `lanes`' job: one session, or the project's recent sessions.
    let (out, _) = w.cv_ok(&["lanes", dead]);
    assert!(out.contains(&aid[..8]) && out.contains("Lane TEST"), "{out}");
    let (out, _) = w.cv_ok(&["lanes", "--recent", "5", "--unfinished", "--cwd", "/work/proj"]);
    assert!(out.contains("SESSION"), "rows are tagged with their session: {out}");
    assert!(out.contains("deadsess") && out.contains(&aid[..8]), "{out}");
    assert!(out.contains("cv adopt <agent-id>"), "the rescue hint: {out}");
    let (out, _) = w.cv_ok(&[
        "lanes",
        "--recent",
        "5",
        "--unfinished",
        "--cwd",
        "/work/proj",
        "--json",
    ]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(
        rows.iter()
            .any(|r| r["parent_id"] == "deadsess" && r["agent_id"] == aid),
        "{out}"
    );
    // The removed flags error with the replacement, exit 2.
    let (ok, code, _, err) = w.cv(&["adopt", "--list", dead]);
    assert!(!ok && code == 2 && err.contains("cv lanes deadsess"), "{err}");
    let (ok, code, _, err) = w.cv(&["adopt", "--orphans"]);
    assert!(
        !ok && code == 2 && err.contains("cv lanes --recent <N> --unfinished"),
        "{err}"
    );

    let live_sub = w.home.join(".claude/projects/-work-proj/livesess/subagents");
    let (_, err) = w.cv_ok(&["adopt", &aid[..8], "--dry-run"]);
    assert!(err.contains("into livesess"), "the default target is announced: {err}");
    assert!(err.contains("restamped on 3 of 3 lines"), "{err}");
    assert!(
        err.contains(&format!("\"to\": \"{aid}\"")),
        "the SendMessage incantation: {err}"
    );
    assert!(err.contains("cv cat deadsess"), "the sidecar reminder: {err}");
    assert!(!live_sub.exists(), "a dry run writes nothing");

    let (ok, code, _, err) = w.cv(&["adopt", &aid[..8], aid, "--from", dead, "--into", live]);
    assert!(!ok && code == 2 && err.contains("selected more than once"), "{err}");
    assert!(
        !live_sub.exists(),
        "duplicate selections must refuse before copying any agent"
    );
    assert_eq!(fs::read_to_string(&agent).unwrap(), body);

    w.cv_ok(&["adopt", aid]);
    let copied = fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl"))).unwrap();
    assert_eq!(
        copied,
        body.replace("\"sessionId\":\"deadsess\"", "\"sessionId\":\"livesess\"")
            .replace(
                "{\\\"sessionId\\\":\\\"livesess\\\"}",
                "{\\\"sessionId\\\":\\\"deadsess\\\"}"
            )
    );
    assert!(
        copied.contains("{\\\"sessionId\\\":\\\"deadsess\\\"}"),
        "content untouched"
    );
    assert_eq!(
        fs::read_to_string(live_sub.join(format!("agent-{aid}.meta.json"))).unwrap(),
        meta
    );
    assert_eq!(
        fs::read_to_string(&agent).unwrap(),
        body,
        "the source is never modified"
    );

    // A second adoption refuses (nothing written) until --force.
    fs::write(live_sub.join(format!("agent-{aid}.jsonl")), "sentinel\n").unwrap();
    let (ok, _, _, err) = w.cv(&["adopt", aid, "--from", dead, "--into", live]);
    assert!(!ok && err.contains("refusing to overwrite"), "{err}");
    assert_eq!(
        fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl"))).unwrap(),
        "sentinel\n"
    );
    w.cv_ok(&["adopt", aid, "--from", dead, "--into", live, "--force"]);
    assert!(fs::read_to_string(live_sub.join(format!("agent-{aid}.jsonl")))
        .unwrap()
        .contains("livesess"));

    // `--model` rewrites the copied meta's `model` and keeps the rest of it.
    w.cv_ok(&[
        "adopt", aid, "--from", dead, "--into", live, "--force", "--model", "sonnet",
    ]);
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(live_sub.join(format!("agent-{aid}.meta.json"))).unwrap()).unwrap();
    assert_eq!(
        (m["model"].as_str(), m["description"].as_str()),
        (Some("sonnet"), Some("Lane TEST"))
    );

    // The agent now has two copies; reading it by id takes the newest instead of erroring.
    let (out, _) = w.cv_ok(&["cat", &format!("agent-{aid}"), "t1"]);
    assert!(out.contains("deadsess"), "{out}");
}

/// The dead session lives in an agent seat's config dir (named by `claude-roots`), and is the most
/// recently written file of the project; the live session is under `~/.claude`. Adopt finds the
/// agent across roots, and the default target is the newest session OTHER than the one being
/// rescued from, in whichever root holds it.
#[test]
fn adopt_reaches_extra_claude_roots_and_never_targets_the_dead_session() {
    let w = World::new("adopt-roots");
    let (dead, live, aid) = ("deadsess", "livesess", "afedcba9876543210");
    let root_line = |sid: &str| {
        serde_json::json!({"type": "user", "uuid": format!("{sid}-1"), "sessionId": sid,
            "timestamp": "2026-01-01T09:00:00Z", "cwd": "/work/proj",
            "message": {"role": "user", "content": "run the lanes"}})
    };
    w.write_session(live, &[root_line(live)]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let seat_proj = w.home.join("seats/bonsai/claude/projects/-work-proj");
    let sub = seat_proj.join(dead).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        seat_proj.join(format!("{dead}.jsonl")),
        format!("{}\n", root_line(dead)),
    )
    .unwrap();
    let mut l = serde_json::json!({"type": "user", "uuid": "u0", "parentUuid": null, "timestamp": "2026-01-01T10:00:00Z",
        "cwd": "/work/proj", "message": {"role": "user", "content": "You are lane SEAT."}});
    l["isSidechain"] = true.into();
    l["agentId"] = aid.into();
    l["sessionId"] = dead.into();
    fs::write(sub.join(format!("agent-{aid}.jsonl")), format!("{l}\n")).unwrap();

    // The seat root is not configured yet: the agent is invisible.
    let (ok, _, _, err) = w.cv(&["adopt", aid, "--dry-run"]);
    assert!(!ok && err.contains("no sub-agent"), "{err}");

    fs::write(w.cv_home.join("claude-roots"), "~/seats/*/claude\n").unwrap();
    let (_, err) = w.cv_ok(&["adopt", aid, "--dry-run"]);
    assert!(
        err.contains("into livesess"),
        "the dead session is newer but is never the target: {err}"
    );
    w.cv_ok(&["adopt", aid]);
    let copied = w
        .home
        .join(".claude/projects/-work-proj/livesess/subagents")
        .join(format!("agent-{aid}.jsonl"));
    assert!(fs::read_to_string(copied)
        .unwrap()
        .contains("\"sessionId\":\"livesess\""));
    let (out, _) = w.cv_ok(&["lanes", dead]);
    assert!(out.contains(&aid[..8]), "a seat session lists by id too: {out}");
}

// ───────────────────────────── tools --timeline → cat ─────────────────────────────

/// Every timeline entry carries its call's id, and that id is what `cv cat` takes: on screen and
/// in `--json`.
#[test]
fn timeline_ids_reach_cat() {
    let w = World::new("timeline-ids");
    standard_corpus(&w);
    let (out, _) = w.cv_ok(&["tools", "alphasess", "--timeline", "--json"]);
    let tl: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(tl.len(), 1, "{out}");
    let id = tl[0]["tool_use_id"].as_str().unwrap();
    assert_eq!(id, "t1");
    let (screen, _) = w.cv_ok(&["tools", "alphasess", "--timeline"]);
    assert!(screen.contains(" t1 "), "{screen}");
    let (out, _) = w.cv_ok(&["cat", "alphasess", id]);
    assert_eq!(out.trim(), "edited ok");
    let (out, _) = w.cv_ok(&["cat", "alphasess", id, "--input"]);
    assert!(out.contains("/work/proj/src/widget.rs"), "{out}");

    // Timeline mode composes the same workflow/tool selectors as histogram mode.
    for (run, agent) in [("wf_first", "first-agent"), ("wf_second", "second-agent")] {
        let states = w.proj.join("alphasess/workflows");
        fs::create_dir_all(&states).unwrap();
        fs::write(
            states.join(format!("{run}.json")),
            serde_json::json!({"runId":run,"workflowName":format!("{run}-name")}).to_string(),
        )
        .unwrap();
        let dir = w.proj.join("alphasess/subagents/workflows").join(run);
        fs::create_dir_all(&dir).unwrap();
        let calls = serde_json::json!([
            {"type":"tool_use","id":format!("{agent}-edit"),"name":"Edit","input":{"file_path":"selected.rs","old_string":"a","new_string":"b"}},
            {"type":"tool_use","id":format!("{agent}-read"),"name":"Read","input":{"file_path":"selected.rs"}}
        ]);
        fs::write(
            dir.join(format!("agent-{agent}.jsonl")),
            cv_testkit::jsonl(&[assistant_line(agent, "2026-01-03T10:00:00Z", calls)]),
        )
        .unwrap();
    }
    let (out, _) = w.cv_ok(&[
        "tools",
        "alphasess",
        "--timeline",
        "--workflow",
        "wf_f",
        "--tool",
        "Edit",
        "--agent",
        "first",
        "--json",
    ]);
    let selected: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(selected.len(), 1, "{out}");
    assert_eq!(selected[0]["agent"], "first-agent");
    assert_eq!(selected[0]["tool_use_id"], "first-agent-edit");
    let (across, _) = w.cv_ok(&["tools", "alphasess", "--across", "--json"]);
    let all: serde_json::Value = serde_json::from_str(&across).unwrap();
    let expected = all["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["agent"] == "first-agent")
        .unwrap();
    let (one, _) = w.cv_ok(&["tools", "alphasess", "--agent", "first", "--workflow", "wf_f", "--json"]);
    assert_eq!(&serde_json::from_str::<serde_json::Value>(&one).unwrap(), expected);
    let (out, _) = w.cv_ok(&["tools", "alphasess", "--workflow", "wf_f", "--tool", "Edit", "--json"]);
    let selected: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["agent"], "first-agent");
    assert_eq!(selected[0]["calls"], 1);
    let (out, _) = w.cv_ok(&["tools", "alphasess", "--timeline", "--tool", "Read", "--json"]);
    let reads: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(reads.len(), 2, "{out}");
    assert!(reads.iter().all(|event| event["tool"] == "Read"));
    let (_, error) = w.cv_fails(&["tools", "alphasess", "--timeline", "--workflow", "absent"]);
    assert!(error.contains("no workflow"), "{error}");

    // One `--agent` grammar on every view: an exact id, else a unique prefix; several matches or
    // none exit 2 and list the candidates, like a session id.
    let twin = serde_json::json!([{"type":"tool_use","id":"twin-read","name":"Read","input":{"file_path":"t.rs"}}]);
    fs::write(
        w.proj
            .join("alphasess/subagents/workflows/wf_first/agent-first-twin.jsonl"),
        cv_testkit::jsonl(&[assistant_line("first-twin", "2026-01-03T10:00:00Z", twin)]),
    )
    .unwrap();
    for extra in [&["--json"][..], &["--timeline", "--json"][..]] {
        let mut args = vec!["tools", "alphasess", "--agent", "first"];
        args.extend_from_slice(extra);
        let (_, code, _, error) = w.cv(&args);
        assert_eq!(code, 2, "{args:?}: {error}");
        assert!(error.contains("first-agent") && error.contains("first-twin"), "{error}");
        let mut args = vec!["tools", "alphasess", "--agent", "first-a"];
        args.extend_from_slice(extra);
        w.cv_ok(&args);
        let mut args = vec!["tools", "alphasess", "--agent", "nobody"];
        args.extend_from_slice(extra);
        let (_, code, _, error) = w.cv(&args);
        assert_eq!(code, 2, "{args:?}: {error}");
    }

    // Two views are not silently collapsed into one.
    for args in [
        &["tools", "alphasess", "--agent", "first-agent", "--tool", "Edit"][..],
        &["tools", "alphasess", "--across", "--agent", "first-agent"][..],
        &["tools", "alphasess", "--across", "--timeline"][..],
    ] {
        let (_, code, _, error) = w.cv(args);
        assert_eq!(code, 2, "{args:?}: {error}");
    }
}

#[test]
fn doctor_labels_payload_estimates_and_recorded_usage_without_changing_accounting() {
    let w = World::new("doctor-accounting-labels");
    w.write_session(
        "accounting",
        &[
            user_line("accounting", "u", "2026-01-01T10:00:00Z", "hello"),
            claude::with(
                assistant_line(
                    "a",
                    "2026-01-01T10:01:00Z",
                    serde_json::json!([
                        {"type":"thinking", "thinking":"", "signature":"s".repeat(350)},
                        {"type":"text", "text":"reply"}
                    ]),
                ),
                serde_json::json!({"message":{"usage":{
                    "input_tokens":100, "cache_read_input_tokens":59900, "output_tokens":20
                }}}),
            ),
        ],
    );
    let (human, _) = w.cv_ok(&["doctor", "accounting"]);
    assert!(
        human.contains("Transcript content by source (138 estimated token equivalents)"),
        "{human}"
    );
    assert!(human.contains("totals include history across compactions"), "{human}");
    assert!(
        human.contains("thinking is mostly signature bytes; requests carry them, and most models count them"),
        "{human}"
    );
    assert!(
        human.contains("First prompt: 60.0k recorded (includes conversation; heuristic proxy for fixed overhead)"),
        "{human}"
    );
    assert!(human.contains("Peak prompt:  60.0k recorded in transcript"), "{human}");
    assert!(!human.contains("before any conversation"), "{human}");
    assert!(!human.contains("lower the thinking budget"), "{human}");
    let (json, _) = w.cv_ok(&["doctor", "accounting", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["fixed_overhead_est"], 60000);
    assert_eq!(report["peak_context"], 60000);
    assert_eq!(report["conversational"]["thinking"], 110);
    assert_eq!(report["conversational"]["total"], 138);
}

#[test]
fn prune_child_id_and_path_select_child_and_emit_discoverable_standalone() {
    let w = World::new("prune-child-source");
    let parent = w.write_session(
        "prune-parent",
        &[user_line(
            "prune-parent",
            "parent-u",
            "2026-01-01T00:00:00Z",
            "parent-only",
        )],
    );
    let child_dir = parent.parent().unwrap().join("prune-parent/subagents");
    fs::create_dir_all(&child_dir).unwrap();
    let child = child_dir.join("agent-prune-child.jsonl");
    let rows = [
        serde_json::json!({"type":"user", "sessionId":"prune-parent", "session_id":"prune-parent", "isSidechain":true,"agentId":"prune-child","uuid":"c0","message":{"role":"user","content":"child-only"}}),
        serde_json::json!({"type":"assistant", "sessionId":"prune-parent", "isSidechain":true,"agentId":"prune-child","uuid":"c1","parentUuid":"c0","message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"fixture"}}]}}),
        serde_json::json!({"type":"user", "sessionId":"prune-parent", "isSidechain":true,"agentId":"prune-child","uuid":"c2","parentUuid":"c1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"x".repeat(4096)}]}}),
    ];
    fs::write(&child, rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
    let parent_before = fs::read(&parent).unwrap();
    let child_before = fs::read(&child).unwrap();
    for (source, target) in [
        ("claude:agent-prune-child", "pruned-child-id"),
        (child.to_str().unwrap(), "pruned-child-path"),
    ] {
        let (out, _) = w.cv_ok(&[
            "prune",
            source,
            "--keep-last",
            "0",
            "--min-size",
            "100",
            "--to",
            target,
            "--json",
        ]);
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["source_id"], "agent-prune-child");
        assert_eq!(report["parent_session_id"], "prune-parent");
        assert_eq!(report["snipped_payloads"], 1);
        let path = PathBuf::from(report["new_path"].as_str().unwrap());
        assert_eq!(path.parent(), parent.parent());
        for row in fs::read_to_string(&path).unwrap().lines() {
            let row: serde_json::Value = serde_json::from_str(row).unwrap();
            assert_eq!(row["sessionId"], target);
            if let Some(id) = row.get("session_id") {
                assert_eq!(id, target);
            }
            assert_eq!(row["isSidechain"], false);
            assert!(row.get("agentId").is_none());
        }
        let (shown, _) = w.cv_ok(&["show", &format!("claude:{target}")]);
        assert!(shown.contains("child-only"));
        assert!(!shown.contains("parent-only"));
    }
    assert_eq!(fs::read(&parent).unwrap(), parent_before);
    assert_eq!(fs::read(&child).unwrap(), child_before);
    let (_, code, _, _) = w.cv(&["prune", child.to_str().unwrap(), "--to", "prune-parent"]);
    assert_ne!(code, 0);
    assert_eq!(fs::read(&parent).unwrap(), parent_before);
}

#[test]
fn rewind_validates_explicit_harness_for_direct_paths_before_writing() {
    let w = World::new("rewind-path-harness");
    standard_corpus(&w);
    let source = w.transcript("alphasess");
    let original = fs::read(&source).unwrap();
    let output = w.base.join("derived");
    let (ok, code, _, error) = w.cv(&[
        "rewind",
        source.to_str().unwrap(),
        "--harness",
        "codex",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert!(!ok && code == 2, "{error}");
    assert!(error.contains("Claude Code sessions only (got codex)"), "{error}");
    assert!(!output.exists());
    assert_eq!(fs::read(&source).unwrap(), original);
    let (ok, _, _, error) = w.cv(&["rewind", source.to_str().unwrap(), "--harness", "unknown-harness"]);
    assert!(!ok && error.contains("unknown-harness"), "{error}");
}

#[test]
fn splice_rejects_invalid_exports_before_source_reads_or_generation() {
    let w = World::new("splice-export-validation");
    let (ok, _, stdout, error) = w.cv(&["splice", "nonexistent-source", "--export", "yaml", "--generate"]);
    assert!(!ok, "{error}");
    assert!(error.contains("unknown export format \"yaml\""), "{error}");
    assert!(
        stdout.is_empty(),
        "failed export must not publish a composition report: {stdout}"
    );
}

/// Claude Code can record foreign models behind a proxy. Replay compatibility cannot be
/// authenticated by a model label: explicit Claude, foreign, and missing labels get one policy.
#[test]
fn claude_portable_rewind_and_port_handle_proxy_records() {
    let w = World::new("claude-portable-proxy");
    let ids = ["Bash:0#hex", "Bash_0_hex", "cv_tool_0", "Bash#0:hex"];
    let mut rows = vec![user_line("prune-parent", "u", "2026-01-01T00:00:00Z", "continue")];
    for (index, model) in [Some("gpt-6.1-sol"), Some("claude-proxy"), None]
        .into_iter()
        .enumerate()
    {
        let mut message = serde_json::json!({"role":"assistant","content":[
            {"type":"thinking","thinking":format!("reason {index}"),"signature":"foreign"},
            {"type":"redacted_thinking","data":"foreign"},
            {"type":"text","text":"keep Bash:0#hex literally"}
        ]});
        if let Some(model) = model {
            message["model"] = model.into();
        }
        rows.push(serde_json::json!({"type":"assistant","uuid":format!("a{index}"),"parentUuid":if index==0 {"u".into()} else {format!("a{}",index-1)},"message":message}));
    }
    rows.push(serde_json::json!({"type":"assistant","uuid":"calls","parentUuid":"a2","message":{"role":"assistant","content": ids.iter().map(|id|serde_json::json!({"type":"tool_use","id":id,"name":"Bash","input":{"command":"true","literal":"Bash:0#hex"}})).collect::<Vec<_>>()}}));
    rows.push(serde_json::json!({"type":"user","uuid":"results","parentUuid":"calls","message":{"role":"user","content":ids.iter().map(|id|serde_json::json!({"type":"tool_result","tool_use_id":id,"content":"Bash:0#hex"})).collect::<Vec<_>>()}}));
    let source = w.write_session("proxy-source", &rows);
    let before = fs::read(&source).unwrap();
    for command in ["rewind", "port"] {
        let out = w.base.join(command);
        let selector = if command == "rewind" {
            source.to_str().unwrap()
        } else {
            "claude:proxy-source"
        };
        let mut args = vec![command, selector, "--out", out.to_str().unwrap()];
        if command == "rewind" {
            args.extend(["--to", "derived"]);
        } else {
            args.extend(["--harness", "claude", "--strict"]);
        }
        w.cv_ok(&args);
        let output = if command == "rewind" {
            out.join("derived.jsonl")
        } else {
            fn find(dir: &std::path::Path) -> PathBuf {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        return find(&path);
                    }
                    if path.extension().is_some_and(|e| e == "jsonl") {
                        return path;
                    }
                }
                panic!("no emitted JSONL")
            }
            find(&out)
        };
        let records: Vec<serde_json::Value> = fs::read_to_string(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let blocks: Vec<_> = records
            .iter()
            .filter_map(|r| r.pointer("/message/content").and_then(|c| c.as_array()))
            .flatten()
            .collect();
        assert!(blocks
            .iter()
            .all(|b| b["type"] != "thinking" && b["type"] != "redacted_thinking"));
        assert!(
            blocks
                .iter()
                .all(|b| !b["text"].as_str().is_some_and(|t| t.starts_with("[thinking"))),
            "reasoning without plaintext is removed, not replaced by placeholder text"
        );
        for index in 0..3 {
            assert!(blocks.iter().any(|b| b["text"] == format!("reason {index}")));
        }
        let calls: Vec<_> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        let results: Vec<_> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| b["tool_use_id"].as_str().unwrap())
            .collect();
        assert_eq!(calls, results);
        assert_eq!(calls.iter().collect::<std::collections::HashSet<_>>().len(), 4);
        assert_eq!(calls[1], ids[1]);
        assert_eq!(calls[2], ids[2]);
        assert!(calls
            .iter()
            .all(|id| id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')));
        let native_out = w.base.join(format!("{command}-native"));
        let mut native = vec![
            command,
            selector,
            "--out",
            native_out.to_str().unwrap(),
            "--thinking",
            "native",
        ];
        if command == "rewind" {
            native.extend(["--to", "native-derived"]);
        }
        w.cv_ok(&native);
    }
    assert_eq!(fs::read(source).unwrap(), before);
}

/// A session whose every recorded model is `claude-*` keeps its signed reasoning by default:
/// the signatures came from Anthropic, and rewriting them would only add noise to the resume.
#[test]
fn claude_rewind_and_port_default_to_native_for_genuine_claude_sessions() {
    let w = World::new("claude-native-default");
    let thinking = [
        serde_json::json!({"type":"thinking","thinking":"","signature":"sig-a"}),
        serde_json::json!({"type":"thinking","thinking":"plan","signature":"sig-b"}),
    ];
    let mut rows = vec![user_line("proxy-source", "u", "2026-01-01T00:00:00Z", "continue")];
    for (index, block) in thinking.iter().enumerate() {
        let parent = if index == 0 {
            "u".to_string()
        } else {
            format!("a{}", index - 1)
        };
        rows.push(
            serde_json::json!({"type":"assistant","uuid":format!("a{index}"),"parentUuid":parent,
            "message":{"role":"assistant","model":"claude-opus-5","content":[block]}}),
        );
    }
    rows.push(serde_json::json!({"type":"assistant","uuid":"syn","parentUuid":"a1",
        "message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}));
    rows.push(serde_json::json!({"type":"assistant","uuid":"call","parentUuid":"syn","message":{"role":"assistant","model":"claude-opus-5",
        "content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"true"}}]}}));
    rows.push(
        serde_json::json!({"type":"user","uuid":"result","parentUuid":"call","message":{"role":"user",
        "content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"ok"}]}}),
    );
    let source = w.write_session("genuine-source", &rows);
    let rewound = w.base.join("rewind");
    let out = w.cv_ok(&[
        "rewind",
        source.to_str().unwrap(),
        "--out",
        rewound.to_str().unwrap(),
        "--to",
        "derived",
        "--json",
    ]);
    let report: serde_json::Value = serde_json::from_str(&out.0).unwrap();
    assert_eq!(report["thinking"], "native");
    let written: Vec<serde_json::Value> = fs::read_to_string(rewound.join("derived.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kept: Vec<_> = written
        .iter()
        .filter_map(|r| r.pointer("/message/content/0"))
        .filter(|b| b["type"] == "thinking")
        .cloned()
        .collect();
    assert_eq!(kept, thinking);
    let ported = w.base.join("port");
    w.cv_ok(&[
        "port",
        "claude:genuine-source",
        "--out",
        ported.to_str().unwrap(),
        "--harness",
        "claude",
    ]);
    let mut signatures = Vec::new();
    for entry in fs::read_dir(&ported).unwrap() {
        for file in fs::read_dir(entry.unwrap().path()).unwrap() {
            let body = fs::read_to_string(file.unwrap().path()).unwrap();
            for line in body.lines() {
                let record: serde_json::Value = serde_json::from_str(line).unwrap();
                for block in record
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                {
                    if block["type"] == "thinking" {
                        signatures.push(block["signature"].clone());
                    }
                }
            }
        }
    }
    assert_eq!(signatures, ["sig-a", "sig-b"]);
}

/// A writer appending to a session while `cv` reads it is the normal state of a live transcript:
/// `show` and `doctor` read the prefix they captured, and listing keeps the session.
#[test]
fn show_and_doctor_read_a_session_its_writer_is_appending_to() {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    let w = World::new("live-writer");
    let turn = |i: usize| {
        let ts = format!("2026-01-01T10:{:02}:{:02}Z", (i / 60) % 60, i % 60);
        [
            user_line(
                "livesess",
                &format!("u{i}"),
                &ts,
                &format!("prompt {i} {}", "context ".repeat(600)),
            ),
            assistant_line(
                &format!("a{i}"),
                &ts,
                serde_json::json!([{"type": "text", "text": format!("reply {i} {}", "detail ".repeat(600))}]),
            ),
        ]
    };
    let path = w.write_session("livesess", &(0..300).flat_map(turn).collect::<Vec<_>>());
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            for i in 300.. {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                for line in turn(i) {
                    // Two writes: readers see a partial last line between them.
                    let line = format!("{line}\n");
                    let (head, tail) = line.split_at(line.len() / 2);
                    file.write_all(head.as_bytes()).unwrap();
                    file.write_all(tail.as_bytes()).unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        // Stops the writer however this closure exits, so a failed assertion cannot hang the scope.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop = Stop(&stop);
        for _ in 0..3 {
            let (prompts, _) = w.cv_ok(&["show", "livesess", "--kind", "prompt"]);
            assert!(prompts.contains("prompt 299 "), "the captured prefix is shown");
            w.cv_ok(&["doctor", "livesess"]);
            let (rows, _) = w.cv_ok(&["ls", "--json"]);
            assert!(rows.contains("\"livesess\""), "{rows}");
        }
    });
}

/// An unreadable persisted output fails the whole session's publication; repair retries it.
#[test]
fn index_json_rejects_unreadable_tool_output_and_retries_after_repair() {
    let w = World::new("index-unreadable-output");
    let output = w.base.join("elsewhere").join("tool-results").join("big.txt");
    fs::create_dir_all(&output).unwrap(); // Deterministic failed read, including as root.
    let stub = format!(
        "<persisted-output>\nOutput too large (66KB). Full output saved to: {}\n\nPreview (first 2KB):\nzqpreview\n</persisted-output>",
        output.display()
    );
    w.write_session(
        "degraded",
        &[
            user_line("degraded", "u1", "2026-01-01T10:00:00Z", "run the big command"),
            claude::tool_result("u2", "2026-01-01T10:01:00Z", "t1", &stub),
        ],
    );
    let (stdout, error) = w.cv_fails(&["index", "--json", "--strict"]);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        error.contains("degraded") && error.contains(output.to_str().unwrap()),
        "{error}"
    );
    fs::remove_dir(&output).unwrap();
    fs::write(&output, "zqrestored").unwrap();
    let (json, _) = w.cv_ok(&["index", "--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["indexed"], 1, "{report}");
    assert_eq!(report["errors"], 0, "{report}");
    let (hits, _) = w.cv_ok(&["search", "zqrestored"]);
    assert!(hits.contains("degraded"), "{hits}");
}

/// The skip policy's second channel, under an injected failure: a project directory cv cannot
/// read hides its sessions, and `cv doctor` names it (INTERFACE-V2 §8: the store has bytes, the
/// listing has zero), while a healthy store records nothing.
#[cfg(unix)]
#[test]
fn an_unreadable_project_dir_is_named_by_doctor_not_silently_empty() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("skipped-source-doctor");
    w.write_session("visible", &[user_line("visible", "u", "2026-01-01T10:00:00Z", "hello")]);
    let (healthy, _) = w.cv_ok(&["doctor", "visible", "--json"]);
    let healthy: serde_json::Value = serde_json::from_str(&healthy).unwrap();
    assert!(healthy.get("skipped_sources").is_none(), "{healthy}");

    let locked = w.home.join(".claude/projects/-work-locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(
        locked.join("hidden.jsonl"),
        cv_testkit::jsonl(&[user_line("visible", "h", "2026-01-01T11:00:00Z", "unseen")]),
    )
    .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = fs::read_dir(&locked).is_err();
    let run = || {
        let (ls, ls_err) = w.cv_ok(&["ls", "--json", "--fresh"]);
        let (doctor, _) = w.cv_ok(&["doctor", "visible", "--json"]);
        let (human, _) = w.cv_ok(&["doctor", "visible"]);
        (ls, ls_err, doctor, human)
    };
    let (ls, ls_err, doctor, human) = run();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if !denied {
        return; // a superuser reads through mode 000: there is no failure to inject
    }
    assert!(ls.contains("visible") && !ls.contains("hidden"), "{ls}");
    assert!(ls_err.contains("1 source(s) could not be read"), "{ls_err}");
    let doctor: serde_json::Value = serde_json::from_str(&doctor).unwrap();
    let skipped = doctor["skipped_sources"].as_array().expect("the doctor row");
    assert_eq!(skipped.len(), 1, "{doctor}");
    assert_eq!(skipped[0]["harness"], "claude");
    assert_eq!(skipped[0]["path"], locked.to_str().unwrap());
    assert!(
        human.contains("Unreadable sources: 1 (the store has bytes, the listing has zero)"),
        "{human}"
    );

    // Readable again, the next discovery replaces the row: nothing is skipped.
    w.cv_ok(&["ls", "--fresh"]);
    let (doctor, _) = w.cv_ok(&["doctor", "visible", "--json"]);
    let doctor: serde_json::Value = serde_json::from_str(&doctor).unwrap();
    assert!(doctor.get("skipped_sources").is_none(), "{doctor}");
}

/// The skip policy's third channel under an injected failure: a catalog that is not a database
/// (a truncated or overwritten file) degrades ONCE. Every consumer that opens it in one process
/// falls back to scanning, the listing is the same, and stderr says so in exactly one line.
#[test]
fn a_garbled_catalog_degrades_once_per_process() {
    let w = World::new("garbled-catalog-warns-once");
    w.write_session("listed", &[user_line("listed", "u", "2026-01-01T10:00:00Z", "hello")]);
    let (healthy, healthy_err) = w.cv_ok(&["ls", "--json"]);
    assert!(!healthy_err.contains("unavailable"), "{healthy_err}");
    let catalog = w.cv_home.join("catalog.db");
    for sidecar in ["catalog.db-wal", "catalog.db-shm"] {
        let _ = fs::remove_file(w.cv_home.join(sidecar));
    }
    fs::write(&catalog, b"SQLite format 3\0 truncated").unwrap();
    let (degraded, err) = w.cv_ok(&["ls", "--json"]);
    assert_eq!(degraded, healthy, "scanning lists what the catalog listed");
    let warnings: Vec<_> = err.lines().filter(|l| l.contains("unavailable")).collect();
    assert_eq!(warnings.len(), 1, "{err}");
    assert!(
        warnings[0].starts_with("cv: the catalog unavailable, continuing without it:"),
        "{err}"
    );
}

// Frozen reads are process-level policy: exercise the real CLI and its complete store tree.
fn frozen_store_snapshot(root: &std::path::Path) -> Vec<(PathBuf, std::time::SystemTime, Vec<u8>)> {
    let mut out = Vec::new();
    if root.exists() {
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(frozen_store_snapshot(&path));
            } else {
                out.push((
                    path.clone(),
                    fs::metadata(&path).unwrap().modified().unwrap(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn frozen_show_leaves_unknown_counts_while_default_certifies() {
    for frozen in [false, true] {
        let w = World::new("frozen-counts");
        let dir = w.home.join(".codex/sessions/2026/01/01");
        fs::create_dir_all(&dir).unwrap();
        let id = "019a0000-0000-7000-8000-0000000000aa";
        fs::write(
            dir.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
            cv_testkit::jsonl(&[
                serde_json::json!({"timestamp": "2026-01-01T10:00:00Z", "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/proj", "timestamp": "2026-01-01T10:00:00Z"}}),
                serde_json::json!({"timestamp": "2026-01-01T10:00:01Z", "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
            ]),
        )
        .unwrap();
        let count = || {
            let (out, _) = w.cv_ok(&["ls", "--json", "--harness", "codex"]);
            serde_json::from_str::<serde_json::Value>(&out).unwrap()[0]["message_count"].clone()
        };
        assert!(count().is_null());
        let before = frozen_store_snapshot(&w.cv_home);
        let mut args = vec!["show", id, "--json"];
        if frozen {
            args.push("--frozen");
        }
        w.cv_ok(&args);
        if frozen {
            assert_eq!(before, frozen_store_snapshot(&w.cv_home));
        }
        assert_eq!(
            count(),
            if frozen {
                serde_json::Value::Null
            } else {
                serde_json::json!(1)
            }
        );
    }
}

#[test]
fn frozen_reads_preserve_every_store_and_stream_stale_events() {
    let w = World::new("frozen-stores");
    standard_corpus(&w);
    w.cv_ok(&["index"]);
    // Prime the usage tables, then change a target so returning stale event rows cannot pass.
    w.cv_ok(&["stats", "--tokens", "--json"]);
    let path = w.proj.join("alphasess.jsonl");
    let body = fs::read_to_string(&path)
        .unwrap()
        .replace("widget.rs", "frozen-refresh.rs");
    fs::write(path, body).unwrap();
    let before = frozen_store_snapshot(&w.cv_home);
    assert!(before.iter().any(|(p, _, _)| p.ends_with("catalog.db")));
    for args in [
        vec!["ls", "--json", "--fresh"],
        vec!["show", "alphasess", "--json"],
        vec!["events", "alphasess", "--json"],
        vec!["events", "alphasess", "--kind", "file_edit", "--json"],
        vec!["search", "zebrafish", "--json"],
        vec!["search", "zebrafish", "--scan", "--json"],
        vec!["stats", "--json"],
        vec!["stats", "--tokens", "--json"],
    ] {
        let mut frozen = vec!["--frozen"];
        frozen.extend_from_slice(&args);
        let (out, _) = w.cv_ok(&frozen);
        assert_eq!(before, frozen_store_snapshot(&w.cv_home), "{args:?}");
        if args[0] == "events" {
            let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
            assert!(!rows.is_empty(), "stale events must stream: {out}");
            assert_eq!(rows[0]["target"], "/work/proj/src/frozen-refresh.rs", "{out}");
        }
    }
    let (frozen, _) = w.cv_ok(&["events", "alphasess", "--json", "--frozen"]);
    let (ordinary, _) = w.cv_ok(&["events", "alphasess", "--json"]);
    assert_eq!(frozen, ordinary);
    // Fresh catalog rows remain readable under the flag, too.
    assert_eq!(ordinary, w.cv_ok(&["--frozen", "events", "alphasess", "--json"]).0);
}

#[test]
fn frozen_env_and_cold_home_never_create_stores() {
    let w = World::new("frozen-cold");
    standard_corpus(&w);
    let cold = w.cv_home.join("absent");
    let env = [("CV_FROZEN", "1"), ("CLUSTERVISION_HOME", cold.to_str().unwrap())];
    for args in [
        vec!["ls", "--json"],
        vec!["show", "alphasess", "--json"],
        vec!["events", "alphasess", "--json"],
        vec!["search", "zebrafish", "--scan"],
        vec!["stats", "--tokens", "--json"],
    ] {
        let (ok, _, out, err) = w.cv_env(&args, &env);
        assert!(ok, "{args:?}: {out} {err}");
        assert!(!cold.exists(), "{args:?} created the catalog directory");
    }
}

#[test]
fn frozen_writer_commands_exit_two_before_resolving_or_writing() {
    let w = World::new("frozen-writers");
    let before = frozen_store_snapshot(&w.cv_home);
    for args in [
        vec!["index"],
        vec!["hooks", "install"],
        vec!["hooks", "remove"],
        vec!["prune", "missing"],
        vec!["rewind", "missing", "--at", "0"],
        vec!["adopt", "missing"],
        vec!["port", "missing", "--harness", "claude"],
        vec!["task", "list"],
        vec!["archive", "sync"],
        vec!["watch"],
        vec!["config", "--add-export", "/absent"],
    ] {
        let mut frozen = vec!["--frozen"];
        frozen.extend_from_slice(&args);
        let (ok, code, _, err) = w.cv(&frozen);
        assert!(
            !ok && code == 2 && err.contains("--frozen") && err.contains("writes"),
            "{args:?}: {code} {err}"
        );
        assert_eq!(before, frozen_store_snapshot(&w.cv_home));
    }
    let (ok, code, _, err) = w.cv_env(&["index"], &[("CV_FROZEN", "1")]);
    assert!(!ok && code == 2 && err.contains("--frozen"), "{err}");
    w.cv_ok(&["ls", "--frozen", "--json"]);
    w.cv_ok(&["hooks", "show", "--frozen", "--json"]);
}

#[test]
fn frozen_events_match_catalog_order_and_keep_every_event() {
    let w = World::new("frozen-event-order");
    w.write_session("eventorder", &[
        user_line("eventorder", "u1", "2026-01-01T10:00:00Z", "fix it"),
        assistant_line("a1", "2026-01-01T10:00:01Z", serde_json::json!([
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/work/a"}},
            {"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": "/work/a", "old_string": "a", "new_string": "b"}},
            {"type": "tool_use", "id": "t3", "name": "Edit", "input": {"file_path": "/work/a", "old_string": "b", "new_string": "c"}},
            {"type": "tool_use", "id": "t4", "name": "Bash", "input": {"command": "true"}}
        ])),
    ]);
    let (frozen, _) = w.cv_ok(&["--frozen", "events", "eventorder", "--json"]);
    let (ordinary, _) = w.cv_ok(&["events", "eventorder", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&ordinary).unwrap();
    // Both edits of /work/a are rows: no dedup on (message, kind, target).
    assert_eq!(rows.len(), 4, "{ordinary}");
    assert_eq!(frozen, ordinary);
}

#[test]
fn frozen_catalog_does_not_migrate_or_touch_live_wal() {
    let w = World::new("frozen-catalog-open");
    standard_corpus(&w);
    w.cv_ok(&["ls", "--json"]);
    let conn = rusqlite::Connection::open(w.cv_home.join("catalog.db")).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE frozen_probe(value); INSERT INTO frozen_probe VALUES(1);",
    )
    .unwrap();
    let before = frozen_store_snapshot(&w.cv_home);
    w.cv_ok(&["--frozen", "ls", "--json"]);
    let (events, _) = w.cv_ok(&["--frozen", "events", "alphasess", "--json"]);
    assert!(events.contains("file_edit"), "{events}");
    assert_eq!(before, frozen_store_snapshot(&w.cv_home));
    // A catalog of an older interpretation must not be upgraded by a frozen read.
    conn.execute_batch("UPDATE catalog_meta SET value='0' WHERE key='discovery_version';")
        .unwrap();
    drop(conn);
    let before = frozen_store_snapshot(&w.cv_home);
    w.cv_ok(&["--frozen", "ls", "--json"]);
    w.cv_ok(&["--frozen", "events", "alphasess", "--json"]);
    assert_eq!(before, frozen_store_snapshot(&w.cv_home));
}

#[test]
fn archive_ls_names_each_skipped_catalog_line() {
    let w = World::new("archive-corrupt-catalog");
    fs::write(w.cv_home.join("catalog.jsonl"), "\n{broken\n").unwrap();
    let (_, error) = w.cv_ok(&["archive", "ls"]);
    assert!(error.contains("skipping corrupt catalog line 2"), "{error}");
}

/// The policy reaches all three reshape doors and must not mistake legacy command files for
/// deleted skills. JSON and provenance at the default keep their existing shape.
#[test]
fn cli_skills_policy_on_prune_rewind_and_port() {
    use serde_json::{json, Value};
    let w = World::new("skills-policy");
    let commands = w.home.join(".claude/commands/a");
    fs::create_dir_all(&commands).unwrap();
    fs::write(
        commands.join("b.md"),
        "---\ndescription: current command\n---\n\nCurrent command.\n",
    )
    .unwrap();
    let source = w.write_session("skills-source", &[
        user_line("skills-source", "u", "2026-01-01T00:00:00Z", "continue"),
        json!({"type":"attachment", "uuid":"r", "parentUuid":"u", "isSidechain":false,
            "cwd":"/work/proj", "attachment":{"type":"invoked_skills", "skills":[
                {"name":"a:b", "path":"userSettings:a:b", "content":"Base directory for this skill: /skills/a:b\n\nCurrent command."},
                {"name":"gone", "path":"userSettings:gone", "content":"Gone."}
            ]}, "rendered":[{"content":"Old restored text"}], "renderedRole":"system"}),
        claude::with(user_line("skills-source", "u2", "2026-01-01T00:01:00Z", "next"), json!({"parentUuid":"r"})),
    ]);
    let before = fs::read(&source).unwrap();
    for command in ["prune", "rewind", "port"] {
        for policy in ["keep", "drop-stale", "drop"] {
            let out = w.base.join(format!("{command}-{policy}"));
            let new_id = format!("derived-{command}-{policy}");
            let mut args = vec![command, "claude:skills-source", "--skills", policy];
            if command == "prune" {
                args.extend(["--to", &new_id, "--json"]);
            } else {
                args.extend(["--out", out.to_str().unwrap(), "--thinking", "native"]);
                if command == "rewind" {
                    args.extend(["--to", &new_id, "--full", "--json"]);
                } else {
                    args.push("--no-context");
                }
            }
            if command != "port" && policy != "keep" {
                let mut dry = args.clone();
                dry.push("--dry-run");
                let (stdout, _) = w.cv_ok(&dry);
                let report: Value = serde_json::from_str(&stdout).unwrap();
                assert!(report["dry_run"].as_bool().unwrap());
                assert!(report["new_path"].is_null());
                assert_eq!(report["skills"]["dropped"], if policy == "drop" { 2 } else { 1 });
            }
            let (stdout, _) = w.cv_ok(&args);
            let output = if command == "port" {
                // This fixture has one cwd and port writes one project subdirectory.
                let project = fs::read_dir(&out).unwrap().next().unwrap().unwrap().path();
                fs::read_dir(project)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
                    .unwrap()
            } else {
                let report: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(report.get("skills").is_some(), policy != "keep");
                if policy != "keep" {
                    assert_eq!(report["skills"]["dropped"], if policy == "drop" { 2 } else { 1 });
                }
                if command == "rewind" {
                    let receipt: Value =
                        serde_json::from_slice(&fs::read(report["provenance_path"].as_str().unwrap()).unwrap())
                            .unwrap();
                    assert_eq!(receipt.get("skills").is_some(), policy != "keep");
                }
                PathBuf::from(report["new_path"].as_str().unwrap())
            };
            let records: Vec<Value> = fs::read_to_string(output)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let restored = records
                .iter()
                .find(|r| r.pointer("/attachment/type") == Some(&json!("invoked_skills")))
                .unwrap();
            let names: Vec<_> = restored["attachment"]["skills"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                match policy {
                    "keep" => vec!["a:b", "gone"],
                    "drop-stale" => vec!["a:b"],
                    _ => vec![],
                }
            );
            assert_eq!(restored.get("rendered").is_some(), policy == "keep");
        }
    }
    assert_eq!(fs::read(source).unwrap(), before);
    for command in ["prune", "rewind", "port"] {
        let (_, code, _, _) = w.cv(&[command, "claude:skills-source", "--skills", "invalid"]);
        assert_eq!(code, 2);
    }
    let (_, code, _, error) = w.cv(&["port", "claude:skills-source", "--harness", "codex", "--skills", "drop"]);
    assert_eq!(code, 2);
    assert!(error.contains("requires a Claude target"));
}
