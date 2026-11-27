//! A reshape carries what its caller selected: a splice into a Claude session writes each
//! system-role record back as it was, and `cv prune --plan` takes the caller's judgment of each
//! message and tool result, and `--carry-agents` / `adopt --join` carry sub-agents across
//! derived and `/resume`-split sessions. Every fixture here is invented text.

use chrono::{Duration as ChronoDuration, Utc};
use cv_testkit::World;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SRC: &str = "5e1ec7ed-0000-4000-8000-000000000001";
const SRC2: &str = "5e1ec7ed-0000-4000-8000-000000000011";
const CODEX_ID: &str = "019d0000-0000-7000-8000-000000000021";

fn rows(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every record with a `uuid` is on the chain Claude Code walks back from the newest one.
fn assert_threaded(path: &Path) {
    let rows = rows(path);
    let threaded: Vec<&Value> = rows.iter().filter(|r| r.get("uuid").is_some()).collect();
    let by: HashMap<&str, &Value> = threaded.iter().map(|r| (r["uuid"].as_str().unwrap(), *r)).collect();
    let mut at = *threaded.last().expect("a threaded record");
    let mut reached = 1;
    while let Some(parent) = at["parentUuid"].as_str() {
        at = by
            .get(parent)
            .unwrap_or_else(|| panic!("parentUuid {parent} names no record in {}", path.display()));
        reached += 1;
    }
    assert_eq!(reached, threaded.len(), "a record is off the resumed chain");
}

/// A record as Claude Code 2.1.32x writes one: both spellings of the session id, and the
/// fields a reader has no name for.
fn record_in(sid: &str, ty: &str, uuid: &str, parent: Option<&str>, at: u32, body: Value) -> Value {
    let mut v = json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": sid, "session_id": sid,
        "timestamp": format!("2026-03-01T10:00:{at:02}Z"), "cwd": "/work/proj", "userType": "external",
        "entrypoint": "cli", "version": "2.1.327", "isSidechain": false, "slug": "tidy-garden"});
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    v
}

fn record(ty: &str, uuid: &str, parent: Option<&str>, at: u32, body: Value) -> Value {
    record_in(SRC, ty, uuid, parent, at, body)
}

/// A prompt, a call and its result, then what a person and the harness put in mid-turn: a
/// message typed while the agent worked, a file attached with `@`, a hook's output, a hook that
/// printed nothing (no message), a slash command's notice; then the reply.
fn mid_turn_session(w: &World) -> PathBuf {
    let queued = "The person sent a message while you were working:\nalso water the ferns";
    w.write_session(
        SRC,
        &[
            record(
                "user",
                "u1",
                None,
                1,
                json!({"origin": {"kind": "human"}, "promptSource": "typed", "permissionMode": "default",
                "message": {"role": "user", "content": "plan the garden beds"}}),
            ),
            record(
                "assistant",
                "a1",
                Some("u1"),
                2,
                json!({"requestId": "req_1", "message": {"role": "assistant", "model": "claude-opus-5", "content": [
                {"type": "tool_use", "id": "toolu_beds", "name": "Read", "input": {"file_path": "/work/proj/beds.md"}}]}}),
            ),
            record(
                "user",
                "r1",
                Some("a1"),
                3,
                json!({"toolUseResult": {"type": "text"}, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_beds", "content": "three beds, north to south"}]}}),
            ),
            record(
                "attachment",
                "q1",
                Some("r1"),
                4,
                json!({"attachment": {"type": "queued_command", "prompt": "also water the ferns", "commandMode": "prompt",
                    "origin": {"kind": "human"}, "humanTurn": true, "source_uuid": "src-1", "delivery_id": "del-1"},
                "rendered": [{"content": queued}], "renderedInHumanTurn": [{"content": "also water the ferns"}],
                "renderedRole": "user"}),
            ),
            record(
                "attachment",
                "f1",
                Some("q1"),
                5,
                json!({"attachment": {"type": "file", "filename": "/work/proj/ferns.md", "displayPath": "ferns.md",
                    "content": {"type": "text", "file": {"filePath": "/work/proj/ferns.md", "content": "ferns like shade"}}},
                "rendered": [{"content": "Called the Read tool with: ferns.md"}, {"content": "ferns like shade"}],
                "renderedRole": "user"}),
            ),
            record(
                "attachment",
                "h0",
                Some("f1"),
                6,
                json!({"attachment": {"type": "hook_success", "hookName": "PostToolUse", "content": "", "stdout": ""}}),
            ),
            record(
                "attachment",
                "h1",
                Some("h0"),
                7,
                json!({"attachment": {"type": "hook_success", "hookName": "UserPromptSubmit", "content": "beds are mulched"},
                "rendered": [{"content": "<system-reminder>beds are mulched</system-reminder>"}], "renderedRole": "system"}),
            ),
            record(
                "system",
                "s1",
                Some("h1"),
                8,
                json!({"subtype": "local_command", "level": "info", "isMeta": false,
                "content": "<local-command-stdout>compost turned</local-command-stdout>"}),
            ),
            record(
                "assistant",
                "a2",
                Some("s1"),
                9,
                json!({"message": {"role": "assistant", "model": "claude-opus-5", "content": [
                {"type": "text", "text": "beds planned, ferns watered"}]}}),
            ),
        ],
    )
}

fn write_codex_rollout(w: &World, id: &str, lines: &[Value]) -> PathBuf {
    let dir = w.home.join(".codex/sessions/2026/03/01");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-03-01T10-00-00-{id}.jsonl"));
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    fs::write(&path, body).unwrap();
    path
}

/// What is the session's own once it is a new one: the ids and where it hangs.
fn same_record(a: &Value, b: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        for key in ["sessionId", "session_id", "parentUuid"] {
            v.as_object_mut().unwrap().remove(key);
        }
        v
    };
    strip(a) == strip(b)
}

#[test]
fn splice_into_a_claude_session_carries_system_records_as_they_were() {
    let w = World::new("splice-system-records");
    let source = mid_turn_session(&w);
    let (out, err) = w.cv_ok(&["splice", SRC, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).expect("stdout is the JSON report");
    let new_id = report["session_id"].as_str().unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    let source_rows = rows(&source);

    // In order, with the mid-turn message and the attached file where they were said.
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(
        order,
        ["u1", "a1", "r1", "q1", "f1", "h1", "s1", "a2"],
        "{err}\n{written:#?}"
    );
    // As what they were: the record itself, not a paraphrase of its text.
    for uuid in ["q1", "f1", "h1", "s1"] {
        let original = source_rows.iter().find(|r| r["uuid"] == uuid).unwrap();
        assert!(
            same_record(by[uuid], original),
            "{uuid} is not the record it was:\n{}\n{original}",
            by[uuid]
        );
    }
    assert_eq!(by["q1"]["attachment"]["origin"]["kind"], "human");
    assert_eq!(by["q1"]["attachment"]["prompt"], "also water the ferns");
    assert_eq!(by["f1"]["attachment"]["filename"], "/work/proj/ferns.md");
    // A typed prompt stays a person's typed prompt.
    assert_eq!(by["u1"]["origin"]["kind"], "human", "{}", by["u1"]);
    assert_eq!(by["u1"]["promptSource"], "typed");
    // One session: every spelling of the session id is the new one (only the continuation
    // provenance `fork-context-ref` names `parentSessionId: SRC`), and the chain is whole.
    for r in &written {
        if r.get("type").and_then(Value::as_str) == Some("fork-context-ref") {
            assert_eq!(r["parentSessionId"], SRC, "{r}");
            assert_eq!(r["sessionId"], new_id, "{r}");
        } else {
            assert!(
                !r.to_string().contains(SRC),
                "a record still names the source session: {r}"
            );
        }
        for key in ["sessionId", "session_id"] {
            if let Some(id) = r.get(key) {
                assert_eq!(id, new_id, "{r}");
            }
        }
    }
    assert_threaded(&path);
    // Nothing is reported lost, and cv reads the new session as it read the source.
    assert!(!err.contains("lossy"), "{err}");
    let shown = |id: &str| -> Vec<(String, String, String)> {
        let (out, _) = w.cv_ok(&["show", id, "--json"]);
        let v: Value = serde_json::from_str(&out).unwrap();
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let text: Vec<&str> = m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|b| b.get("text").or(b.get("content"))?.as_str())
                    .collect();
                (
                    m["kind"].as_str().unwrap_or_default().to_owned(),
                    m["origin"].as_str().unwrap_or_default().to_owned(),
                    text.join("\n"),
                )
            })
            .collect()
    };
    let (before, after) = (shown(SRC), shown(new_id));
    assert_eq!(before.len(), 8);
    assert_eq!(after, before);
}

/// A span cut through the middle keeps the records inside it and re-links the cut.
#[test]
fn a_spliced_window_keeps_the_system_records_inside_it() {
    let w = World::new("splice-system-window");
    mid_turn_session(&w);
    // Messages 3..8 are the mid-turn message, the file, the hook output, the notice, the reply.
    let spec = format!("{SRC}:3..8");
    let (out, err) = w.cv_ok(&["splice", &spec, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let order: Vec<String> = rows(&path)
        .iter()
        .filter_map(|r| Some(r.get("uuid")?.as_str()?.to_owned()))
        .collect();
    assert_eq!(order, ["q1", "f1", "h1", "s1", "a2"], "{err}");
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");
}

/// Multi-span splice where Span 1 ends in `_record` attachments/notices (and also when Span 1 is
/// ONLY `_record`s) keeps every trailing record on Span 2's backward `parentUuid` walk.
#[test]
fn multi_span_splice_ending_in_system_records_keeps_parent_chain_whole() {
    let w = World::new("splice-multi-span-carrier-tail");
    mid_turn_session(&w);
    w.write_session(
        SRC2,
        &[
            record_in(
                SRC2,
                "user",
                "b_u1",
                None,
                10,
                json!({"message": {"role": "user", "content": "now plant the herbs"}}),
            ),
            record_in(
                SRC2,
                "assistant",
                "b_a1",
                Some("b_u1"),
                11,
                json!({"message": {"role": "assistant", "model": "claude-opus-5", "content": [
                    {"type": "text", "text": "herbs planted"}]}}),
            ),
        ],
    );

    // Case 1: Span 1 ends at `s1` (`0..7` = `u1, a1, r1, q1, f1, h1, s1`), Span 2 starts at root `b_u1`.
    let s1 = format!("{SRC}:0..7");
    let s2 = format!("{SRC2}:0..2");
    let (out, err) = w.cv_ok(&["splice", &s1, &s2, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(
        order,
        ["u1", "a1", "r1", "q1", "f1", "h1", "s1", "b_u1", "b_a1"],
        "{err}"
    );
    let b_u1 = written.iter().find(|r| r["uuid"] == "b_u1").unwrap();
    assert_eq!(
        b_u1["parentUuid"], "s1",
        "Span 2 root must link to Span 1's last carrier"
    );
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");

    // Case 2: Span 1 consists ONLY of `_record` messages (`3..7` = `q1, f1, h1, s1`).
    let only_carriers = format!("{SRC}:3..7");
    let (out, err) = w.cv_ok(&["splice", &only_carriers, &s2, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    assert_eq!(order, ["q1", "f1", "h1", "s1", "b_u1", "b_a1"], "{err}");
    assert_threaded(&path);
}

#[test]
fn mixed_codex_and_claude_splice_preserves_provenance_records_and_system_prompt() {
    let w = World::new("splice-mixed-provenance");
    mid_turn_session(&w);
    write_codex_rollout(
        &w,
        CODEX_ID,
        &[
            json!({"timestamp": "2026-03-01T09:59:00Z", "type": "session_meta",
                "payload": {"id": CODEX_ID, "cwd": "/work/proj", "base_instructions": {"text": "Follow garden house style."}}}),
            json!({"timestamp": "2026-03-01T09:59:01Z", "type": "response_item",
                "payload": {"type": "message", "role": "developer",
                    "content": [{"type": "input_text", "text": "<environment_context>zone 8b</environment_context>"}]}}),
            json!({"timestamp": "2026-03-01T09:59:02Z", "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "prepare soil"}]}}),
            json!({"timestamp": "2026-03-01T09:59:03Z", "type": "compacted",
                "payload": {"message": "soil prepared summary"}}),
        ],
    );

    // 1. Mixed `codex + claude -> --session`: keeps Span 1's foreign InjectedContext + system_prompt
    //    AND keeps Span 2's verbatim Claude `_record`s (`q1`, `f1`, `h1`, `s1`) and turn extras!
    let codex_all = format!("codex:{CODEX_ID}");
    let (out, err) = w.cv_ok(&["splice", &codex_all, SRC, "--session", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    assert!(
        written.iter().any(|r| r["attachment"]["type"] == "prompt_snapshot"
            && r["attachment"]["systemPrompt"][0] == "Follow garden house style."),
        "foreign system_prompt snapshot should be emitted: {written:#?}"
    );
    assert!(
        written.iter().any(|r| r["attachment"]["type"] == "injected_context"),
        "foreign Codex developer context should emit via SystemFate::Special"
    );
    assert!(
        written.iter().any(|r| r["isCompactSummary"] == true),
        "foreign Codex compaction summary should emit via SystemFate::Special"
    );
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    assert_eq!(by["q1"]["attachment"]["prompt"], "also water the ferns");
    assert_eq!(by["u1"]["promptSource"], "typed");
    assert_threaded(&path);
    assert!(!err.contains("lossy"), "{err}");

    // 2. Foreign `codex -> --harness claude --out <dir>`: emits foreign system_prompt + Special kinds.
    let out_dir = w.base.join("claude-out");
    let (_, err) = w.cv_ok(&[
        "splice",
        &codex_all,
        "--harness",
        "claude",
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert!(!err.contains("lossy"), "{err}");
    let emitted = fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            p.is_dir().then_some(p)
        })
        .flat_map(|d| fs::read_dir(d).unwrap())
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .expect("emitted claude jsonl");
    let emitted_rows = rows(&emitted);
    assert!(emitted_rows
        .iter()
        .any(|r| r["attachment"]["type"] == "prompt_snapshot"));
    assert!(emitted_rows.iter().any(|r| r["subtype"] == "compact_boundary"));
    assert_threaded(&emitted);
}

#[test]
fn splice_export_json_omits_carrier_records_and_obeys_extra_namespaces() {
    let w = World::new("splice-export-json-clean");
    mid_turn_session(&w);
    let spec = format!("{SRC}:0..8");
    let (out, _) = w.cv_ok(&["splice", &spec, "--export", "json"]);
    let session: cv_core::Session = serde_json::from_str(&out).expect("valid Session JSON");
    let check_bag = |extra: &serde_json::Map<String, Value>, ctx: &str| {
        for (k, v) in extra {
            assert!(
                (k == cv_core::ir::CV_NAMESPACE || cv_core::Harness::parse(k).is_some()) && v.is_object(),
                "{ctx}: flat or non-namespace key {k:?} = {v:?}"
            );
        }
    };
    check_bag(&session.extra, "session.extra");
    for (i, m) in session.messages.iter().enumerate() {
        assert!(
            !m.extra.contains_key(cv_core::ir::CARRIER_KEY),
            "message {i} exported JSON must not leak _record: {:?}",
            m.extra
        );
        check_bag(&m.extra, &format!("message[{i}].extra"));
    }
}

#[test]
fn spliced_claude_compaction_boundary_reports_zero_spurious_fidelity_deltas() {
    let w = World::new("splice-compact-boundary-fidelity");
    w.write_session(
        SRC,
        &[
            record(
                "user",
                "u1",
                None,
                1,
                json!({"message": {"role": "user", "content": "start work"}}),
            ),
            record(
                "system",
                "cb1",
                None,
                2,
                json!({"subtype": "compact_boundary", "logicalParentUuid": "u1", "level": "info",
                    "content": "Conversation compacted", "compactMetadata": {"trigger": "auto", "preTokens": 120000}}),
            ),
            record(
                "user",
                "cs1",
                Some("cb1"),
                3,
                json!({"isCompactSummary": true, "message": {"role": "user", "content": "Compacted summary"}}),
            ),
            record(
                "assistant",
                "a1",
                Some("cs1"),
                4,
                json!({"message": {"role": "assistant", "model": "claude-opus-5",
                    "content": [{"type": "text", "text": "continuing after compaction"}]}}),
            ),
        ],
    );
    let (out, err) = w.cv_ok(&["splice", SRC, "--session", "--json"]);
    assert!(
        !err.contains("lossy"),
        "unexpected fidelity delta on compact_boundary:\n{err}"
    );
    assert!(!err.contains("timestamps supplied by target"), "{err}");
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let path = Path::new(report["path"].as_str().unwrap()).to_path_buf();
    let written = rows(&path);
    assert!(written
        .iter()
        .any(|r| r["uuid"] == "cb1" && r["subtype"] == "compact_boundary"));
}

// ───────────────────────────── prune: the count it prints ─────────────────────────────

#[test]
fn prune_warns_on_inherited_usage_without_boundary_and_revive_estimate_rewrites_it() {
    const SID: &str = "5e1ec7ed-0000-4000-8000-000000000040";
    let w = World::new("prune-inherited-40");
    let usage = |ctx: u64| {
        json!({"input_tokens": 100, "cache_read_input_tokens": ctx - 100,
        "cache_creation_input_tokens": 0, "output_tokens": 30})
    };
    let record = |ty: &str, uuid: &str, parent: Option<&str>, content: Value, usage: Option<Value>| {
        let mut v = json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": SID,
            "timestamp": "2026-11-01T08:00:00Z", "cwd": "/work/proj",
            "message": {"role": ty, "model": "claude-opus-5", "content": content}});
        if let Some(usage) = usage {
            v["message"]["usage"] = usage;
        }
        v
    };
    w.write_session(
        SID,
        &[
            record("user", "u1", None, json!("folded summary context"), None),
            record(
                "assistant",
                "a1",
                Some("u1"),
                json!([{"type": "text", "text": "noted"}]),
                Some(usage(922_155)),
            ),
            record("user", "u2", Some("a1"), json!("ship the fix"), None),
            record(
                "assistant",
                "a2",
                Some("u2"),
                json!([{"type": "text", "text": "shipping"}]),
                Some(usage(976_850)),
            ),
        ],
    );

    // Default `cv prune --dry-run` warns about implausible inherited overhead and names `--revive-estimate`.
    let (dry_out, dry_err) = w.cv_ok(&["prune", SID, "--dry-run", "--json"]);
    let dry_report: Value = serde_json::from_str(dry_out.trim()).unwrap();
    assert!(
        dry_report["inherited_overhead_tokens"]
            .as_u64()
            .is_some_and(|n| n > 900_000),
        "{dry_report}"
    );
    assert!(
        dry_err.contains("--revive-estimate") && dry_err.contains("fixed-overhead tokens above visible turns"),
        "{dry_err}"
    );
    assert!(
        !dry_err.contains("no recorded usage required an estimated-window rewrite"),
        "{dry_err}"
    );

    // Passing `--revive-estimate` rewrites 922k and 976k down to ~21k-25k.
    let (est_out, est_err) = w.cv_ok(&["prune", SID, "--revive-estimate", "--json"]);
    let est_report: Value = serde_json::from_str(est_out.trim()).unwrap();
    assert_eq!(est_report["revived"]["usage_records_rewritten"], 2, "{est_report}");
    assert_eq!(est_report["revived"]["recorded_tokens_before"], 976_850, "{est_report}");
    let after = est_report["revived"]["recorded_tokens_after"].as_u64().unwrap();
    assert!((20_000..25_000).contains(&after), "{est_report}");
    assert_eq!(est_report["resume_reads_tokens_after"], after, "{est_report}");
    assert!(est_report.get("inherited_overhead_tokens").is_none(), "{est_report}");
    assert!(est_err.contains("2 usage record(s)"), "{est_err}");

    // `--revive-estimate` conflicts with `--no-revive`.
    let (ok, code, _, conflict_err) = w.cv(&["prune", SID, "--revive-estimate", "--no-revive"]);
    assert!(!ok && code == 2, "{conflict_err}");
}

/// A session whose context shrank before the prune: an early request recorded 900k, the last
/// one 500k. Revive rewrites the records above its estimate; the harness sizes a resume by the
/// LAST record. The report names both, and skips a trailing thinking-only record removed by
/// `--drop-thinking`.
#[test]
fn prune_says_which_recorded_size_a_resume_reads_and_what_revive_rewrote() {
    const SID: &str = "5e1ec7ed-0000-4000-8000-000000000003";
    let w = World::new("prune-count");
    let usage = |context: u64| {
        json!({"input_tokens": 100, "cache_read_input_tokens": context - 300,
        "cache_creation_input_tokens": 200, "output_tokens": 50})
    };
    let record = |ty: &str, uuid: &str, parent: Option<&str>, content: Value, usage: Option<Value>| {
        let mut v = json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": SID,
            "timestamp": "2026-03-03T08:00:00Z", "cwd": "/work/proj",
            "message": {"role": ty, "model": "claude-opus-5", "content": content}});
        if let Some(usage) = usage {
            v["message"]["usage"] = usage;
        }
        v
    };
    let source = w.write_session(
        SID,
        &[
            record("user", "u1", None, json!("count the seedlings"), None),
            record(
                "assistant",
                "a1",
                Some("u1"),
                json!([{"type": "text", "text": "counting"}]),
                Some(usage(900_000)),
            ),
            record("user", "u2", Some("a1"), json!("and the trays?"), None),
            record(
                "assistant",
                "a2",
                Some("u2"),
                json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls trays"}}]),
                Some(usage(499_000)),
            ),
            record(
                "user",
                "r1",
                Some("a2"),
                json!([{"type": "tool_result", "tool_use_id": "t1", "content": "tray ".repeat(2000)}]),
                None,
            ),
            record(
                "assistant",
                "a3",
                Some("r1"),
                json!([{"type": "text", "text": "twelve trays"}]),
                Some(usage(500_000)),
            ),
            record(
                "assistant",
                "a4_think",
                Some("a3"),
                json!([{"type": "thinking", "thinking": "double-checking count", "signature": "sig1"}]),
                Some(usage(510_000)),
            ),
        ],
    );

    // Without `--drop-thinking`, `a4_think` survives and is the last request (510k).
    let (out, err) = w.cv_ok(&["prune", SID, "--keep-last", "0", "--min-size", "1024", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["resume_reads_tokens_before"], 510_000, "{report}");
    assert_eq!(report["resume_reads_tokens_after"], 510_000, "{report}");
    assert_eq!(report["revived"]["usage_records_rewritten"], 1, "{report}");
    assert_eq!(report["revived"]["recorded_tokens_before"], 900_000, "{report}");
    let estimate = report["revived"]["recorded_tokens_after"].as_u64().unwrap();
    assert!((510_000..900_000).contains(&estimate), "{report}");
    assert!(
        err.contains(
            "1 usage record(s) recorded above the post-prune estimate were rewritten to it: highest recorded 900k"
        ),
        "{err}"
    );
    assert!(
        err.contains(
            "resume reads the last request's recorded size (input + cache read + cache creation): 510k, unchanged"
        ),
        "{err}"
    );

    // With `--drop-thinking`, `a4_think` is emptied and removed; `a3` (500k) is now the last request.
    let (out2, err2) = w.cv_ok(&[
        "prune",
        SID,
        "--keep-last",
        "0",
        "--min-size",
        "1024",
        "--drop-thinking",
        "--json",
    ]);
    let report2: Value = serde_json::from_str(out2.trim()).unwrap();
    assert_eq!(report2["resume_reads_tokens_before"], 500_000, "{report2}");
    assert_eq!(report2["resume_reads_tokens_after"], 500_000, "{report2}");
    assert!(
        err2.contains(
            "resume reads the last request's recorded size (input + cache read + cache creation): 500k, unchanged"
        ),
        "{err2}"
    );
    let path = source.with_file_name(format!("{}.jsonl", report2["new_id"].as_str().unwrap()));
    let written = rows(&path);
    assert!(!written.iter().any(|r| r["uuid"] == "a4_think"));
    let last = written.into_iter().rfind(|r| r.get("uuid").is_some()).unwrap();
    assert_eq!(last["uuid"], "a3");
    let u = &last["message"]["usage"];
    let sum = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .map(|k| u[*k].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(sum, 500_000);
}

// ───────────────────────────── prune --plan: the caller's selection ─────────────────────────────

const PLAN: &str = "5e1ec7ed-0000-4000-8000-000000000002";

fn turn(ty: &str, uuid: &str, parent: Option<&str>, content: Value) -> Value {
    json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": PLAN, "timestamp": "2026-03-02T09:00:00Z",
        "cwd": "/work/proj", "message": {"role": ty, "model": "claude-opus-5", "content": content}})
}
fn call(uuid: &str, parent: &str, id: &str) -> Value {
    turn(
        "assistant",
        uuid,
        Some(parent),
        json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("survey {id}")}}]),
    )
}
fn result(uuid: &str, parent: &str, id: &str, text: &str) -> Value {
    let mut v = turn(
        "user",
        uuid,
        Some(parent),
        json!([{"type": "tool_result", "tool_use_id": id, "content": text}]),
    );
    v["toolUseResult"] = json!({"stdout": text});
    v
}

/// The person's message: two typed lines, a pasted block of forty, one typed line.
fn typed_with_paste() -> String {
    let paste: String = (1..=40).map(|i| format!("pasted row {i}\n")).collect();
    format!("please sort the seed list\nkeep the heirlooms first\n{paste}that is the whole list")
}
fn rows_text() -> String {
    (1..=60).map(|i| format!("row {i}\n")).collect()
}

/// Two turns. The first reads four results and has a file attached and a hook's output; the
/// second reads one.
fn planned_session(w: &World) -> PathBuf {
    let long = "soil sample ".repeat(400);
    let mut file = json!({"type": "attachment", "uuid": "f1", "parentUuid": "r4", "sessionId": PLAN,
        "timestamp": "2026-03-02T09:00:00Z", "cwd": "/work/proj",
        "attachment": {"type": "file", "filename": "/work/proj/ferns.md", "displayPath": "ferns.md"},
        "rendered": [{"content": "Called the Read tool with: ferns.md"}, {"content": "ferns like shade"}],
        "renderedRole": "user"});
    file["attachment"]["content"] = json!({"type": "text"});
    let hook = json!({"type": "attachment", "uuid": "h1", "parentUuid": "f1", "sessionId": PLAN,
        "timestamp": "2026-03-02T09:00:00Z", "cwd": "/work/proj",
        "attachment": {"type": "hook_success", "hookName": "PostToolUse", "content": "status line refreshed"},
        "rendered": [{"content": "<system-reminder>status line refreshed</system-reminder>"}]});
    w.write_session(
        PLAN,
        &[
            turn("user", "u1", None, json!(typed_with_paste())),
            call("a1", "u1", "t_keep"),
            result("r1", "a1", "t_keep", "the beds face south"),
            call("a2", "r1", "t_ptr"),
            result("r2", "a2", "t_ptr", &long),
            call("a3", "r2", "t_exc"),
            result("r3", "a3", "t_exc", &rows_text()),
            call("a4", "r3", "t_drop"),
            result("r4", "a4", "t_drop", "nothing found in the shed"),
            file,
            hook,
            turn(
                "assistant",
                "a5",
                Some("h1"),
                json!([{"type": "text", "text": "sorted, heirlooms first"}]),
            ),
            turn("user", "u2", Some("a5"), json!("and the bulbs?")),
            call("a6", "u2", "t_bulbs"),
            result("r6", "a6", "t_bulbs", "forty bulbs in the cellar"),
            turn(
                "assistant",
                "a7",
                Some("r6"),
                json!([{"type": "text", "text": "forty bulbs, cellar"}]),
            ),
        ],
    )
}

fn write_plan(w: &World, name: &str, plan: Value) -> String {
    let path = w.base.join(name);
    fs::write(&path, plan.to_string()).unwrap();
    path.to_str().unwrap().to_owned()
}

#[test]
fn prune_plan_carries_the_callers_selection_per_message_and_tool_result() {
    let w = World::new("plan-selection");
    let source = planned_session(&w);
    let plan = write_plan(
        &w,
        "plan.json",
        json!({
            "results": {
                "t_keep": {"action": "keep"},
                "t_ptr": {"action": "pointer"},
                "t_exc": {"action": "excerpt", "lines": ["2-3", "58-"]},
                "t_drop": {"action": "drop"},
                "t_bulbs": {"action": "pointer"}
            },
            "messages": {
                // Their typed lines stay; the paste between them goes behind the marker.
                "u1": {"action": "excerpt", "lines": ["1-2", "43-"]},
                "f1": {"action": "pointer"},
                "h1": {"action": "drop"}
            }
        }),
    );
    let (out, err) = w.cv_ok(&["prune", PLAN, "--plan", &plan, "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let p = &report["plan"];
    assert_eq!(
        (
            &p["keep"],
            &p["pointer"],
            &p["excerpt"],
            &p["results_dropped"],
            &p["records_dropped"]
        ),
        (&json!(1), &json!(3), &json!(2), &json!(1), &json!(3)),
        "{report}"
    );
    let new_id = report["new_id"].as_str().unwrap();
    let path = source.with_file_name(format!("{new_id}.jsonl"));
    let written = rows(&path);
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    let order: Vec<&str> = written.iter().filter_map(|r| r.get("uuid")?.as_str()).collect();
    // The dropped hook record is gone, and the dropped result with its call and their records.
    assert_eq!(
        order,
        ["u1", "a1", "r1", "a2", "r2", "a3", "r3", "f1", "a5", "u2", "a6", "r6", "a7"],
        "{err}"
    );
    assert!(!fs::read_to_string(&path).unwrap().contains("t_drop"));
    assert_threaded(&path);

    let marker = |text: &str, id: &str| {
        text.starts_with(&format!("[PRUNED id={id} ")) && text.contains(&format!("cv cat {new_id} {id}"))
    };
    // keep: verbatim.
    assert_eq!(by["r1"]["message"]["content"][0]["content"], "the beds face south");
    // pointer: the marker and nothing else.
    let pointer = by["r2"]["message"]["content"][0]["content"].as_str().unwrap();
    assert!(marker(pointer, "t_ptr") && !pointer.contains('\n'), "{pointer}");
    // excerpt by lines: the named lines verbatim, in order, the rest behind the marker.
    let excerpt = by["r3"]["message"]["content"][0]["content"].as_str().unwrap();
    let (head, kept) = excerpt.split_once('\n').unwrap();
    assert!(marker(head, "t_exc"), "{head}");
    assert_eq!(
        kept,
        "[… lines 2-3]\nrow 2\nrow 3\n[… lines 58-]\nrow 58\nrow 59\nrow 60"
    );
    // A person's message: their own words verbatim, the pasted block pointered.
    let typed = by["u1"]["message"]["content"].as_str().unwrap();
    let (head, kept) = typed.split_once('\n').unwrap();
    assert!(marker(head, "u1"), "{head}");
    assert_eq!(
        kept,
        "[… lines 1-2]\nplease sort the seed list\nkeep the heirlooms first\n[… lines 43-]\nthat is the whole list"
    );
    // An attachment: what the model is sent goes behind markers; the record stays what it was.
    for (i, item) in by["f1"]["rendered"].as_array().unwrap().iter().enumerate() {
        assert!(marker(item["content"].as_str().unwrap(), &format!("f1#r{i}")), "{item}");
    }
    assert_eq!(by["f1"]["attachment"]["filename"], "/work/proj/ferns.md");

    // Nothing is lost: `cv cat` reopens every original.
    let cat = |id: &str| w.cv_ok(&["cat", new_id, id]).0;
    assert_eq!(cat("t_ptr").trim_end(), "soil sample ".repeat(400).trim_end());
    assert_eq!(cat("t_exc").trim_end(), rows_text().trim_end());
    assert_eq!(cat("t_drop").trim_end(), "nothing found in the shed");
    assert!(cat("t_drop#call").contains("survey t_drop"));
    assert_eq!(cat("u1").trim_end(), typed_with_paste());
    assert_eq!(cat("f1#r1").trim_end(), "ferns like shade");
    assert!(cat("h1").contains("status line refreshed"));

    // The one rule checked for the caller: a warning, and the session is written all the same.
    assert_eq!(
        p["rests_on_nothing"],
        json!([{"reply": "a7", "results": 1}]),
        "{report}"
    );
    assert!(
        err.contains("reply a7 rests on nothing kept: its turn's 1 tool result(s) were all dropped or pointered"),
        "{err}"
    );
    assert!(!err.contains("reply a5"), "the first turn kept a result: {err}");
}

#[test]
fn a_plan_that_does_not_fit_is_refused_whole_and_no_plan_is_todays_default() {
    let w = World::new("plan-refusals");
    let source = planned_session(&w);
    let before = fs::read_dir(source.parent().unwrap()).unwrap().count();
    let bad = write_plan(
        &w,
        "bad.json",
        json!({
            "results": {
                "t_exc": {"action": "excerpt", "lines": ["61-70"]},
                "t_ptr": {"action": "excerpt", "lines": ["1-5"]},
                "t_keep": {"action": "keep"}
            },
            "messages": {"nope": {"action": "drop"}, "a1": {"action": "drop"}, "u2": {"action": "excerpt"}}
        }),
    );
    let (ok, code, _, err) = w.cv(&["prune", PLAN, "--plan", &bad]);
    assert!(!ok && code == 2, "{err}");
    for problem in [
        "t_exc: line 61 is past the payload's 60 line(s)",
        "t_ptr: line 5 is past the payload's 1 line(s)",
        "nope: no record with this uuid in the session",
        "t_keep: planned as keep, but the plan drops the record that holds it or its call",
        "u2: an excerpt names what it keeps",
    ] {
        assert!(err.contains(problem), "missing {problem:?} in:\n{err}");
    }
    assert_eq!(
        fs::read_dir(source.parent().unwrap()).unwrap().count(),
        before,
        "nothing written"
    );

    // No plan: size and age decide, as they did. A person's words are not a payload.
    let (out, _) = w.cv_ok(&["prune", PLAN, "--keep-last", "0", "--min-size", "1024", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(report["plan"].is_null(), "{report}");
    let path = source.with_file_name(format!("{}.jsonl", report["new_id"].as_str().unwrap()));
    let written = rows(&path);
    let content = |uuid: &str| {
        let r = written.iter().find(|r| r["uuid"] == uuid).unwrap();
        r["message"]["content"].clone()
    };
    assert_eq!(content("u1"), json!(typed_with_paste()));
    assert!(content("r2")[0]["content"]
        .as_str()
        .unwrap()
        .starts_with("[PRUNED id=t_ptr "));
    assert_eq!(content("r1")[0]["content"], "the beds face south");
    assert_eq!(written.len(), 16);
}

#[test]
fn plan_handles_image_only_messages_mixed_result_blocks_crlf_overlaps_and_filtered_warnings() {
    const EDGE: &str = "5e1ec7ed-0000-4000-8000-000000000004";
    let w = World::new("plan-edge-cases");
    let rec = |ty: &str, uuid: &str, parent: Option<&str>, content: Value| {
        json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": EDGE,
            "timestamp": "2026-03-04T09:00:00Z", "cwd": "/work/proj",
            "message": {"role": ty, "model": "claude-opus-5", "content": content}})
    };
    let mut multi_result = rec(
        "user",
        "r_multi",
        Some("a_multi"),
        json!([
            {"type": "tool_result", "tool_use_id": "t_drop_1", "content": "discarded result"},
            {"type": "tool_result", "tool_use_id": "t_keep_2", "content": "kept result"}
        ]),
    );
    multi_result["toolUseResult"] = json!({"stdout": "kept mirror payload"});

    let mut api_err = rec(
        "assistant",
        "a_err",
        Some("r_err"),
        json!([{"type": "text", "text": "API Error: 529 Overloaded"}]),
    );
    api_err["isApiErrorMessage"] = json!(true);
    api_err["message"]["model"] = json!("<synthetic>");

    let crlf_text = (1..=8).map(|i| format!("crlf line {i}\r\n")).collect::<String>();
    let source = w.write_session(
        EDGE,
        &[
            // Turn 0 (will be windowed out in the `--keep` check below):
            rec("user", "u_old", None, json!("old turn")),
            rec(
                "assistant",
                "a_old_call",
                Some("u_old"),
                json!([{"type": "tool_use", "id": "t_old", "name": "Bash", "input": {"command": "echo old"}}]),
            ),
            rec(
                "user",
                "r_old",
                Some("a_old_call"),
                json!([{"type": "tool_result", "tool_use_id": "t_old", "content": "old output"}]),
            ),
            rec(
                "assistant",
                "a_old_reply",
                Some("r_old"),
                json!([{"type": "text", "text": "old conclusion"}]),
            ),
            // Turn 1: image-only user message + thinking-only assistant + parallel [drop, keep] + CRLF excerpt + API error:
            rec(
                "user",
                "u_img",
                Some("a_old_reply"),
                json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}]),
            ),
            rec(
                "assistant",
                "a_think",
                Some("u_img"),
                json!([{"type": "thinking", "thinking": "inspecting image", "signature": "sig"}]),
            ),
            rec(
                "assistant",
                "a_multi",
                Some("a_think"),
                json!([
                    {"type": "tool_use", "id": "t_drop_1", "name": "Bash", "input": {"command": "one"}},
                    {"type": "tool_use", "id": "t_keep_2", "name": "Bash", "input": {"command": "two"}}
                ]),
            ),
            multi_result,
            rec(
                "assistant",
                "a_crlf",
                Some("r_multi"),
                json!([{"type": "tool_use", "id": "t_crlf", "name": "Read", "input": {"file_path": "dos.txt"}}]),
            ),
            rec(
                "user",
                "r_crlf",
                Some("a_crlf"),
                json!([{"type": "tool_result", "tool_use_id": "t_crlf", "content": crlf_text}]),
            ),
            // Turn 2: tool result pointered, followed only by a `<synthetic>` API error (not a reply!):
            rec("user", "u_err", Some("r_crlf"), json!("retry check")),
            rec(
                "assistant",
                "a_err_call",
                Some("u_err"),
                json!([{"type": "tool_use", "id": "t_err", "name": "Bash", "input": {"command": "fail"}}]),
            ),
            rec(
                "user",
                "r_err",
                Some("a_err_call"),
                json!([{"type": "tool_result", "tool_use_id": "t_err", "content": "transient"}]),
            ),
            api_err,
        ],
    );

    // Excerpt on an image-only message is still refused.
    let bad_img = write_plan(
        &w,
        "bad-img.json",
        json!({"messages": {"u_img": {"action": "excerpt", "lines": ["1"]}}}),
    );
    let (ok, code, _, err) = w.cv(&["prune", EDGE, "--plan", &bad_img]);
    assert!(
        !ok && code == 2 && err.contains("u_img: an excerpt needs a text payload"),
        "{err}"
    );

    // Pointer on image-only message, [drop, keep] preserving kept toolUseResult, CRLF + overlapping
    // lines merged without `\r` or duplicates, `--drop-thinking` not inflating `plan.records_dropped`,
    // and windowed-out (`u_old..a_old_reply`) + `<synthetic>` (`a_err`) excluded from `rests_on_nothing`.
    let good = write_plan(
        &w,
        "good-edge.json",
        json!({
            "results": {
                "t_old": {"action": "pointer"},
                "t_drop_1": {"action": "drop"},
                "t_keep_2": {"action": "keep"},
                "t_crlf": {"action": "excerpt", "lines": ["3-5", "1-3", "7-8"]},
                "t_err": {"action": "pointer"}
            },
            "messages": {
                "u_img": {"action": "precis", "precis": "diagram of south beds"}
            }
        }),
    );
    let (out, err) = w.cv_ok(&[
        "prune",
        EDGE,
        "--plan",
        &good,
        "--keep",
        "4..",
        "--drop-thinking",
        "--keep-last",
        "0",
        "--json",
    ]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let p = &report["plan"];
    // `a_think` was emptied by `--drop-thinking`, NOT by `--plan`, so `records_dropped` stays 0!
    assert_eq!(p["records_dropped"], 0, "{report}");
    assert_eq!(p["results_dropped"], 1, "{report}");
    assert_eq!(p["precis"], 1, "{report}");
    // Neither windowed-out `a_old_reply` nor synthetic API error `a_err` triggers `rests_on_nothing`!
    assert_eq!(p["rests_on_nothing"], json!([]), "{report}\n{err}");

    let new_id = report["new_id"].as_str().unwrap();
    let path = source.with_file_name(format!("{new_id}.jsonl"));
    let written = rows(&path);
    let by: HashMap<&str, &Value> = written
        .iter()
        .filter_map(|r| Some((r.get("uuid")?.as_str()?, r)))
        .collect();
    assert!(!by.contains_key("a_think"), "thinking-only record removed");
    assert_threaded(&path);

    // Fix #5: image-only `u_img` was replaced with `[PRUNED id=u_img#b0 ...]\ndiagram of south beds`,
    // and `cv cat` recovers the original image block JSON!
    let img_text = by["u_img"]["message"]["content"][0]["text"].as_str().unwrap();
    assert!(
        img_text.starts_with("[PRUNED id=u_img#b0 ") && img_text.ends_with("\ndiagram of south beds"),
        "{img_text}"
    );
    let cat_img = w.cv_ok(&["cat", new_id, "u_img#b0"]).0;
    assert!(cat_img.contains("iVBORw0KGgo="), "{cat_img}");

    // Fix #6: `r_multi` keeps only `t_keep_2`, and its `toolUseResult` stays `{"stdout": "kept mirror payload"}`
    // (NOT overwritten by a `t_drop_1#tur` string marker!).
    let r_multi_blocks = by["r_multi"]["message"]["content"].as_array().unwrap();
    assert_eq!(r_multi_blocks.len(), 1);
    assert_eq!(r_multi_blocks[0]["tool_use_id"], "t_keep_2");
    assert_eq!(by["r_multi"]["toolUseResult"], json!({"stdout": "kept mirror payload"}));

    // Fix #7: overlapping `["3-5", "1-3", "7-8"]` merged into `1-5` + `7-8` with zero `\r`s or duplicates!
    let crlf_out = by["r_crlf"]["message"]["content"][0]["content"].as_str().unwrap();
    assert!(!crlf_out.contains('\r'), "stray CR in {crlf_out:?}");
    let (_, kept_crlf) = crlf_out.split_once('\n').unwrap();
    assert_eq!(
        kept_crlf,
        "[… lines 1-5]\ncrlf line 1\ncrlf line 2\ncrlf line 3\ncrlf line 4\ncrlf line 5\n[… lines 7-8]\ncrlf line 7\ncrlf line 8"
    );
}

// ───────────────────────────── --carry-agents ─────────────────────────────

/// A session with one background agent (a typed one on its own model) and one workflow agent.
/// Each derived session takes them along under its own id: the spawn record verbatim, the
/// transcript restamped, so an agent re-entered after the move is still itself.
#[test]
fn carry_agents_takes_spawn_records_and_transcripts_to_the_new_session() {
    const SID: &str = "5e1ec7ed-0000-4000-8000-000000000004";
    let w = World::new("carry-agents");
    let line = |ty: &str, uuid: &str, parent: Option<&str>, content: Value| {
        json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": SID, "timestamp": "2026-03-04T07:00:00Z",
            "cwd": "/work/proj", "message": {"role": ty, "model": "claude-opus-5", "content": content}})
    };
    let source = w.write_session(
        SID,
        &[
            line("user", "u1", None, json!("survey the orchard")),
            line(
                "assistant",
                "a1",
                Some("u1"),
                json!([{"type": "text", "text": "an agent is on it"}]),
            ),
            line("user", "u2", Some("a1"), json!("and the hedges")),
            line(
                "assistant",
                "a2",
                Some("u2"),
                json!([{"type": "text", "text": "noted"}]),
            ),
        ],
    );
    let meta = json!({"agentType": "Explore", "model": "sonnet", "description": "survey the orchard",
        "toolUseId": "toolu_spawn"});
    let agent_line = |uuid: &str, parent: Option<&str>, role: &str, text: &str| {
        json!({"type": role, "uuid": uuid, "parentUuid": parent, "sessionId": SID, "agentId": "a0123456789abcdef",
            "isSidechain": true, "timestamp": "2026-03-04T07:00:01Z", "cwd": "/work/proj",
            "message": {"role": role, "content": text}})
    };
    w.write_agent(
        SID,
        "a0123456789abcdef",
        &meta,
        &[
            agent_line("b1", None, "user", "count the apple trees, then wait"),
            agent_line("b2", Some("b1"), "assistant", "forty apple trees"),
        ],
    );
    let run = w.proj.join(SID).join("subagents/workflows/run-7");
    fs::create_dir_all(&run).unwrap();
    fs::write(
        run.join("agent-w1.meta.json"),
        r#"{"agentType":"builder","model":"opus"}"#,
    )
    .unwrap();

    let carried_to = |new_id: &str, what: &str| {
        let sub = w.proj.join(new_id).join("subagents");
        let kept: Value =
            serde_json::from_str(&fs::read_to_string(sub.join("agent-a0123456789abcdef.meta.json")).unwrap())
                .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(kept, meta, "{what}: the spawn record is carried verbatim");
        assert_eq!(
            fs::read_to_string(sub.join("workflows/run-7/agent-w1.meta.json")).unwrap(),
            r#"{"agentType":"builder","model":"opus"}"#,
            "{what}: a workflow agent keeps its run folder"
        );
        let lines = rows(&sub.join("agent-a0123456789abcdef.jsonl"));
        assert_eq!(lines.len(), 2, "{what}");
        for l in &lines {
            assert_eq!(l["sessionId"], new_id, "{what}: restamped");
            assert_eq!(l["agentId"], "a0123456789abcdef", "{what}");
        }
        assert_eq!(
            lines[0]["message"]["content"], "count the apple trees, then wait",
            "{what}: the brief"
        );
    };
    let report = |args: &[&str]| -> Value {
        let (out, err) = w.cv_ok(args);
        assert!(
            err.contains("agents:      2 spawn record(s) copied, 1 transcript(s) copied → "),
            "{err}"
        );
        serde_json::from_str(out.trim()).unwrap()
    };

    let pruned = report(&["prune", SID, "--carry-agents", "--json"]);
    assert_eq!(pruned["carried_agents"]["metas"], 2, "{pruned}");
    carried_to(pruned["new_id"].as_str().unwrap(), "prune");

    let rewound = report(&["rewind", SID, "--at", "1", "--carry-agents", "--json"]);
    carried_to(rewound["new_id"].as_str().unwrap(), "rewind");

    let (head, tail) = (format!("{SID}:..2"), format!("{SID}:2.."));
    let spliced = report(&["splice", &head, &tail, "--session", "--carry-agents", "--json"]);
    assert_eq!(
        spliced["carried_agents"]["duplicates"], 0,
        "one source, named twice: {spliced}"
    );
    carried_to(spliced["session_id"].as_str().unwrap(), "splice");

    // `meta`: the spawn records alone. Without the flag nothing is carried, as before.
    let (out, _) = w.cv_ok(&["prune", SID, "--carry-agents=meta", "--json"]);
    let lean: Value = serde_json::from_str(out.trim()).unwrap();
    let sub = w.proj.join(lean["new_id"].as_str().unwrap()).join("subagents");
    assert!(sub.join("agent-a0123456789abcdef.meta.json").is_file());
    assert!(!sub.join("agent-a0123456789abcdef.jsonl").exists());
    let (out, _) = w.cv_ok(&["prune", SID, "--json"]);
    let plain: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(!w.proj.join(plain["new_id"].as_str().unwrap()).exists());
    assert!(plain.get("carried_agents").is_none(), "{plain}");
    // The source's agents are untouched.
    let agent = w.proj.join(SID).join("subagents/agent-a0123456789abcdef.jsonl");
    let original = rows(&agent);
    assert!(original.iter().all(|l| l["sessionId"] == SID));
    assert!(source.is_file());

    // The bare flag never takes the id after it.
    let (out, _) = w.cv_ok(&["prune", "--carry-agents", SID, "--json"]);
    let first: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(first["carried_agents"]["transcripts"], 1, "{first}");
    // A dry run whose id is not pinned names no folder it would write.
    let (out, err) = w.cv_ok(&["prune", SID, "--carry-agents", "--dry-run", "--json"]);
    let dry: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        (&dry["carried_agents"]["dir"], &dry["carried_agents"]["metas"]),
        (&Value::Null, &json!(2)),
        "{dry}"
    );
    assert!(
        err.contains("2 spawn record(s) copied, 1 transcript(s) copied → the new session's subagents/"),
        "{err}"
    );
    // A sub-agent has no agents of its own: a usage error before anything is written.
    let before = fs::read_dir(&w.proj).unwrap().count();
    let (ok, code, _, err) = w.cv(&["prune", "agent-a0123456789abcdef", "--carry-agents"]);
    assert!(
        !ok && code == 2 && err.contains("is itself a sub-agent"),
        "{code} {err}"
    );
    assert_eq!(fs::read_dir(&w.proj).unwrap().count(), before);

    // An agent mid-append: its unfinished last record is left out, and the line says so.
    let mut live = fs::OpenOptions::new().append(true).open(&agent).unwrap();
    std::io::Write::write_all(&mut live, br#"{"type":"assistant","uuid":"b3","parentUu"#).unwrap();
    let (out, err) = w.cv_ok(&["prune", SID, "--carry-agents", "--json"]);
    let cut: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(cut["carried_agents"]["unfinished"], 1, "{cut}");
    assert!(
        err.contains(
            "2 spawn record(s) copied, 1 transcript(s) copied, 1 transcript(s) carried up to an unfinished last record → "
        ),
        "{err}"
    );
    let carried = w
        .proj
        .join(cut["new_id"].as_str().unwrap())
        .join("subagents/agent-a0123456789abcdef.jsonl");
    assert_eq!(rows(&carried).len(), 2, "whole records only");
    assert!(fs::read_to_string(&carried).unwrap().ends_with("}\n"));
}

#[test]
fn carry_agents_emits_uniform_report_even_when_source_has_zero_agents() {
    const SID: &str = "5e1ec7ed-0000-4000-8000-000000000044";
    let w = World::new("carry-agents-empty");
    w.write_session(
        SID,
        &[
            json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": SID,
                "timestamp": "2026-03-04T07:00:00Z", "cwd": "/work/proj",
                "message": {"role": "user", "content": "solo prompt"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SID,
                "timestamp": "2026-03-04T07:00:01Z", "cwd": "/work/proj",
                "message": {"role": "assistant", "model": "claude-opus-5",
                "content": [{"type": "text", "text": "solo reply"}]}}),
        ],
    );

    let (out, _) = w.cv_ok(&["prune", SID, "--carry-agents", "--dry-run", "--json"]);
    let dry: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(dry["carried_agents"].is_object(), "{dry}");
    assert_eq!(dry["carried_agents"]["dir"], Value::Null, "{dry}");
    assert_eq!(dry["carried_agents"]["metas"], 0, "{dry}");
    assert_eq!(dry["carried_agents"]["transcripts"], 0, "{dry}");

    let (out, _) = w.cv_ok(&["prune", SID, "--carry-agents", "--json"]);
    let live: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(live["carried_agents"].is_object(), "{live}");
    assert!(live["carried_agents"]["dir"].is_string(), "{live}");
    assert_eq!(live["carried_agents"]["metas"], 0, "{live}");
    assert_eq!(live["carried_agents"]["transcripts"], 0, "{live}");
}

/// The shape an in-place `/resume` leaves when an agent was mid-turn: the new session's folder
/// holds a file that begins with the agent's next assistant record and has no meta; the brief,
/// the earlier records and the meta are in the old session's folder. `cv adopt --join` puts the
/// old records first and copies the meta, supports unique agent prefixes, exits 2 on unmatched or
/// ambiguous specs, and a repeat changes nothing.
#[test]
fn adopt_join_puts_a_moved_agents_earlier_records_before_what_the_new_session_holds() {
    const OLD: &str = "5e1ec7ed-0000-4000-8000-000000000005";
    const NEW: &str = "5e1ec7ed-0000-4000-8000-000000000006";
    const AGENT: &str = "a0123456789abcdef";
    const OTHER: &str = "a0999999999999999";
    let w = World::new("adopt-join");
    for sid in [OLD, NEW] {
        w.write_session(
            sid,
            &[
                json!({"type": "user", "uuid": format!("{sid}-u"), "parentUuid": null, "sessionId": sid,
                "timestamp": "2026-03-05T07:00:00Z", "cwd": "/work/proj",
                "message": {"role": "user", "content": "survey the orchard"}}),
            ],
        );
    }
    let line = |uuid: &str, parent: Option<&str>, sid: &str, aid: &str, role: &str, text: &str| {
        json!({"type": role, "uuid": uuid, "parentUuid": parent, "sessionId": sid, "agentId": aid,
            "isSidechain": true, "timestamp": "2026-03-05T07:00:01Z", "cwd": "/work/proj",
            "message": {"role": role, "content": text}})
    };
    let meta = json!({"agentType": "Explore", "model": "sonnet", "description": "count the trees"});
    w.write_agent(
        OLD,
        AGENT,
        &meta,
        &[
            line("b1", None, OLD, AGENT, "user", "count the apple trees, then wait"),
            line("b2", Some("b1"), OLD, AGENT, "assistant", "counting the first row"),
        ],
    );
    w.write_agent(
        OLD,
        OTHER,
        &json!({"agentType": "Explore", "description": "other lane"}),
        &[line("o1", None, OLD, OTHER, "user", "other lane")],
    );

    // Unmatched and ambiguous explicit agent specs exit 2 before writing anything.
    let (ok, code, _, err) = w.cv(&["adopt", "--join", "nonexistent", "--from", OLD, "--into", NEW]);
    assert!(!ok && code == 2 && err.contains("no sub-agent"), "{code} {err}");
    let (ok, code, _, err) = w.cv(&["adopt", "--join", "a0", "--from", OLD, "--into", NEW]);
    assert!(!ok && code == 2 && err.contains("matches 2 agents"), "{code} {err}");
    assert!(!w.proj.join(NEW).exists());

    // What the agent wrote after the move: no meta beside it.
    let moved = w.proj.join(NEW).join("subagents");
    fs::create_dir_all(&moved).unwrap();
    let tail = cv_testkit::jsonl(&[
        line("b3", Some("b2"), NEW, AGENT, "assistant", "forty apple trees"),
        line("b4", Some("b3"), NEW, AGENT, "user", "and the pears"),
    ]);
    let transcript = moved.join(format!("agent-{AGENT}.jsonl"));
    fs::write(&transcript, &tail).unwrap();

    // Unique prefix `a01234` narrows the join to `AGENT` alone (leaving `OTHER` uncopied).
    let (out, err) = w.cv_ok(&["adopt", "--join", "a01234", "--from", OLD, "--into", NEW, "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["carried_agents"]["joined"], 1, "{report}");
    assert_eq!(report["carried_agents"]["metas"], 1, "{report}");
    assert!(!moved.join(format!("agent-{OTHER}.jsonl")).exists());
    // The line says what happened, first, and never leads with a count of nothing.
    let said = err.lines().find(|l| l.starts_with("✦ joined")).expect("the join line");
    let (_, did) = said.split_once(": ").unwrap();
    let (did, to) = did.rsplit_once(" → ").unwrap();
    assert_eq!(
        did,
        "1 spawn record(s) copied, 1 transcript(s) joined (the source's records first, then what the session already held)"
    );
    assert!(!did.contains("0 "), "{said}");
    assert!(to.ends_with("subagents"), "{said}");
    let joined = rows(&transcript);
    let ids: Vec<&str> = joined.iter().map(|r| r["uuid"].as_str().unwrap()).collect();
    assert_eq!(ids, ["b1", "b2", "b3", "b4"], "the old records go first");
    assert!(joined.iter().all(|r| r["sessionId"] == NEW && r["agentId"] == AGENT));
    assert_eq!(joined[0]["message"]["content"], "count the apple trees, then wait");
    assert!(
        fs::read_to_string(&transcript).unwrap().ends_with(&tail),
        "what the new session held is byte for byte where it was"
    );
    let kept: Value =
        serde_json::from_str(&fs::read_to_string(moved.join(format!("agent-{AGENT}.meta.json"))).unwrap()).unwrap();
    assert_eq!(kept, meta);

    let before = fs::read_to_string(&transcript).unwrap();
    let (out, err) = w.cv_ok(&["adopt", "--join", "a01234", "--from", OLD, "--into", NEW, "--json"]);
    assert!(err.contains(": 2 file(s) already there, left as they are → "), "{err}");
    let again: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        (&again["carried_agents"]["joined"], &again["carried_agents"]["already"]),
        (&json!(0), &json!(2)),
        "{again}"
    );
    assert_eq!(fs::read_to_string(&transcript).unwrap(), before);
    // The old session's folder is read, never written.
    let old = rows(&w.proj.join(OLD).join(format!("subagents/agent-{AGENT}.jsonl")));
    assert!(old.len() == 2 && old.iter().all(|r| r["sessionId"] == OLD));
}

/// One transcript the carry refuses must not cost the others. Three agents in name order, the
/// middle one holding a whole line that is not a record: the session is written and the command
/// exits 0, the first and third are carried, the middle one keeps its spawn record and is named.
/// `adopt --join`, whose whole job is the carry, exits 1.
#[test]
fn a_refused_agent_transcript_is_skipped_and_named_and_the_others_are_carried() {
    const SID: &str = "5e1ec7ed-0000-4000-8000-000000000007";
    let w = World::new("carry-refused");
    w.write_session(
        SID,
        &[
            json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": SID,
            "timestamp": "2026-03-06T07:00:00Z", "cwd": "/work/proj",
            "message": {"role": "user", "content": "survey the orchard"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SID,
            "timestamp": "2026-03-06T07:00:01Z", "cwd": "/work/proj",
            "message": {"role": "assistant", "model": "claude-opus-5", "content": [{"type": "text", "text": "on it"}]}}),
        ],
    );
    let ids = ["a000000000000001", "a000000000000002", "a000000000000003"];
    for id in ids {
        let path = w.write_agent(
            SID,
            id,
            &json!({"agentType": "Explore", "description": id}),
            &[
                json!({"type": "user", "uuid": "b1", "parentUuid": null, "sessionId": SID, "agentId": id,
                "isSidechain": true, "message": {"role": "user", "content": "count the trees"}}),
            ],
        );
        if id == ids[1] {
            let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
            std::io::Write::write_all(&mut file, b"not a record\n").unwrap();
        }
    }
    let (ok, code, out, err) = w.cv(&["prune", SID, "--carry-agents", "--json"]);
    assert!(ok && code == 0, "the session is written: {code}\n{err}");
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    let c = &report["carried_agents"];
    assert_eq!((&c["metas"], &c["transcripts"]), (&json!(3), &json!(2)), "{report}");
    assert_eq!(c["refused"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(c["refused"][0]["agent"], ids[1], "{report}");
    assert!(
        c["refused"][0]["why"]
            .as_str()
            .unwrap()
            .contains("is not one JSON value"),
        "{report}"
    );
    assert!(
        err.contains("agents:      3 spawn record(s) copied, 2 transcript(s) copied, 1 transcript(s) refused → "),
        "{err}"
    );
    assert!(
        err.contains(&format!("  ⚠ agent {} not carried: the record at byte ", ids[1])),
        "{err}"
    );
    let new_id = report["new_id"].as_str().unwrap();
    let sub = w.proj.join(new_id).join("subagents");
    for id in [ids[0], ids[2]] {
        assert_eq!(rows(&sub.join(format!("agent-{id}.jsonl"))).len(), 1, "{id}");
    }
    assert!(!sub.join(format!("agent-{}.jsonl", ids[1])).exists());
    assert!(
        sub.join(format!("agent-{}.meta.json", ids[1])).is_file(),
        "its spawn record still goes"
    );
    assert_eq!(fs::read_dir(&sub).unwrap().count(), 5);

    // The join after the fact: everything else is already there, and the refusal is its failure.
    let (ok, code, out, err) = w.cv(&["adopt", "--join", "--from", SID, "--into", new_id, "--json"]);
    assert!(!ok && code == 1, "{code}\n{err}");
    let again: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(again["carried_agents"]["already"], 5, "{again}");
    assert_eq!(again["carried_agents"]["refused"][0]["agent"], ids[1], "{again}");
    assert!(
        err.contains(": 1 transcript(s) refused, 5 file(s) already there, left as they are → "),
        "{err}"
    );
    assert!(err.contains(&format!("  ⚠ agent {} not carried: ", ids[1])), "{err}");
    assert_eq!(fs::read_dir(&sub).unwrap().count(), 5, "a repeat changes nothing");
}

/// `#35 × #37` composition:
/// 1. `S1 -> /resume S2` splits a running sub-agent (`S2` has no `.meta.json` and records
///    `<task-notification>` + `SubagentStop` within the last 60s, `< 300s` stale threshold).
/// 2. `cv prune S2 --carry-agents --json` from the unjoined split tail `S2` materializes the full
///    root-to-tip `[b1, b2, b3, b4]` transcript (`parentUuid: null` at `b1`) + `S1`'s `.meta.json`!
/// 3. `cv adopt --join --from S1 --into S2` prepends `S1` into `S2`, and `Beside::next_run` still
///    advances `S1 -> S2` so `cv lanes S1 --unfinished --json` reports 0 unfinished!
#[test]
fn resume_split_composes_with_carry_agents_adopt_join_and_lanes() {
    const S1: &str = "5e1ec7ed-0000-4000-8000-000000000051";
    const S2: &str = "5e1ec7ed-0000-4000-8000-000000000052";
    const AGENT: &str = "a7777777777777777";
    let w = World::new("split-carry-compose");
    let now = Utc::now();
    let ts = |secs_ago: i64| (now - ChronoDuration::seconds(secs_ago)).to_rfc3339();

    w.write_session(
        S1,
        &[
            json!({"type": "user", "uuid": "s1-u1", "parentUuid": null, "sessionId": S1,
                "timestamp": ts(50), "cwd": "/work/proj",
                "message": {"role": "user", "content": "spawn worker"}}),
            json!({"type": "assistant", "uuid": "s1-a1", "parentUuid": "s1-u1", "sessionId": S1,
            "timestamp": ts(49), "cwd": "/work/proj",
            "message": {"role": "assistant", "model": "claude-opus-5", "content": [
                {"type": "tool_use", "id": "toolu_bg", "name": "Agent",
                 "input": {"description": "audit orchard", "prompt": "count trees", "run_in_background": true}}
            ]}}),
            json!({"type": "user", "uuid": "s1-r1", "parentUuid": "s1-a1", "sessionId": S1,
            "timestamp": ts(48), "cwd": "/work/proj",
            "toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": AGENT},
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_bg",
                 "content": format!("Async agent launched (agentId: {AGENT}).")}
            ]}}),
        ],
    );
    w.write_session(
        S2,
        &[
            json!({"type": "user", "uuid": "s2-u1", "parentUuid": null, "sessionId": S2,
                "timestamp": ts(25), "cwd": "/work/proj",
                "message": {"role": "user", "content": format!(
                    "<task-notification>\n<task-id>{AGENT}</task-id>\n<status>completed</status>\n<result>all 42 trees counted</result>\n</task-notification>"
                )}}),
            json!({"type": "assistant", "uuid": "s2-a1", "parentUuid": "s2-u1", "sessionId": S2,
                "timestamp": ts(20), "cwd": "/work/proj",
                "message": {"role": "assistant", "model": "claude-opus-5",
                "content": [{"type": "text", "text": "worker finished"}]}}),
        ],
    );

    let meta = json!({"agentType": "Explore", "model": "sonnet", "description": "audit orchard"});
    w.write_agent(
        S1,
        AGENT,
        &meta,
        &[
            json!({"type": "user", "uuid": "b1", "parentUuid": null, "sessionId": S1, "agentId": AGENT,
                "isSidechain": true, "timestamp": ts(47), "cwd": "/work/proj",
                "message": {"role": "user", "content": "count trees"}}),
            json!({"type": "assistant", "uuid": "b2", "parentUuid": "b1", "sessionId": S1, "agentId": AGENT,
                "isSidechain": true, "timestamp": ts(40), "cwd": "/work/proj",
                "message": {"role": "assistant", "content": "counting..."}}),
        ],
    );
    let s2_sub = w.proj.join(S2).join("subagents");
    fs::create_dir_all(&s2_sub).unwrap();
    fs::write(
        s2_sub.join(format!("agent-{AGENT}.jsonl")),
        cv_testkit::jsonl(&[
            json!({"type": "user", "uuid": "b3", "parentUuid": "b2", "sessionId": S2, "agentId": AGENT,
                "isSidechain": true, "timestamp": ts(35), "cwd": "/work/proj",
                "message": {"role": "user", "content": "finish count"}}),
            json!({"type": "assistant", "uuid": "b4", "parentUuid": "b3", "sessionId": S2, "agentId": AGENT,
                "isSidechain": true, "timestamp": ts(30), "cwd": "/work/proj",
                "message": {"role": "assistant", "content": "all 42 trees counted"}}),
            json!({"type": "api-request", "sessionId": S2, "timestamp": ts(29)}),
            json!({"type": "attachment", "uuid": "b5", "parentUuid": "b4", "sessionId": S2, "agentId": AGENT,
                "isSidechain": true, "timestamp": ts(28),
                "attachment": {"type": "hook_success", "hookEvent": "SubagentStop"}}),
        ]),
    )
    .unwrap();

    // Carrying from the unjoined `/resume` tail `S2` brings `[b1..b4]` + `S1`'s `.meta.json`!
    let (out, _) = w.cv_ok(&["prune", S2, "--carry-agents", "--json"]);
    let pruned: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(pruned["carried_agents"]["metas"], 1, "{pruned}");
    assert_eq!(pruned["carried_agents"]["transcripts"], 1, "{pruned}");
    let s3 = pruned["new_id"].as_str().unwrap();
    let s3_sub = w.proj.join(s3).join("subagents");
    let s3_meta: Value =
        serde_json::from_str(&fs::read_to_string(s3_sub.join(format!("agent-{AGENT}.meta.json"))).unwrap()).unwrap();
    assert_eq!(s3_meta, meta);
    let s3_rows = rows(&s3_sub.join(format!("agent-{AGENT}.jsonl")));
    let s3_uuids: Vec<&str> = s3_rows.iter().filter_map(|r| r["uuid"].as_str()).collect();
    assert_eq!(s3_uuids, ["b1", "b2", "b3", "b4", "b5"]);
    assert!(s3_rows[0]["parentUuid"].is_null());

    // Now run `adopt --join --from S1 --into S2`: `S1` still advances to `S2`, so `cv lanes S1`
    // sees `S2`'s completion even with timestamps < 60s old!
    w.cv_ok(&["adopt", "--join", "--from", S1, "--into", S2, "--json"]);
    let (lanes_out, _) = w.cv_ok(&["lanes", S1, "--unfinished", "--json"]);
    let lanes: Value = serde_json::from_str(lanes_out.trim()).unwrap();
    assert_eq!(lanes.as_array().unwrap().len(), 0, "S1 advances to joined S2: {lanes}");
}

#[test]
fn compaction_selection_matches_rewind_splice_and_distill_across_five_boundaries() {
    // #tooth: earlier genuine turns survive later summaries. A later prompt is
    // reported even when a historical selection omits it from the successor.
    let w = World::new("compaction-selection");
    let mut source = Vec::new();
    let mut parent = None;
    for n in 1..=5 {
        let human = format!("human-{n}");
        let boundary = format!("boundary-{n}");
        let summary = format!("summary-{n}");
        let hook = format!("hook-{n}");
        source.extend([
            record("user",&human,parent.as_deref(),n,json!({"message":{"role":"user","content":format!("instruction {n}")}})),
            record("system",&boundary,Some(&human),n,json!({"subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}})),
            record("user",&summary,Some(&boundary),n,json!({"isCompactSummary":true,"message":{"role":"user","content":"generated summary"}})),
            record("attachment",&hook,Some(&summary),n,json!({"attachment":{"type":"hook_success","hookEvent":"SessionStart","hookName":"SessionStart:compact"},"rendered":[{"content":"compact hook"}]})),
        ]);
        parent = Some(hook);
    }
    source.push(record(
        "user",
        "later",
        parent.as_deref(),
        6,
        json!({"message":{"role":"user","content":"later correction"}}),
    ));
    let path = w.write_session(SRC, &source);
    let path = path.to_str().unwrap();
    let read = |args: &[&str]| -> Value {
        let (out, _) = w.cv_ok(args);
        serde_json::from_str(&out).unwrap()
    };
    let rewind = read(&[
        "rewind",
        path,
        "--uncompact",
        "--json",
        "--out",
        w.base.to_str().unwrap(),
    ]);
    let written = Path::new(rewind["new_path"].as_str().unwrap());
    assert_threaded(written);
    let ids = rows(written).into_iter().map(|r| r["uuid"].clone()).collect::<Vec<_>>();
    assert_eq!(
        ids,
        json!(["human-1", "human-2", "human-3", "human-4", "human-5"])
            .as_array()
            .unwrap()
            .clone()
    );
    let splice = read(&["splice", path, "--uncompact", "--export", "json"]);
    let distill = read(&["distill", path, "--uncompact", "--json"]);
    let plan = &rewind["recovery"];
    assert_eq!(plan["pre_compaction"], 5);
    assert_eq!(plan["excluded_prompts"][0]["text"], "later correction");
    assert_eq!(plan, &splice["extra"]["cv"]["recovery"][0]);
    assert_eq!(plan, &distill["recovery"]);
    assert_eq!(plan, &read(&["splice", path, "--uncompact", "--json"])["recovery"][0]);
    let first = read(&["distill", path, "--pre-compaction", "1", "--uncompact", "--json"]);
    assert_eq!(first["recovery"]["messages"], json!([0]));
    assert_eq!(first["recovery"]["excluded_prompts"].as_array().unwrap().len(), 5);
}
