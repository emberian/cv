//! A reshape carries what its caller selected: a splice into a Claude session writes each
//! system-role record back as it was, and `cv prune --plan` takes the caller's judgment of each
//! message and tool result. Every fixture here is invented text.

use cv_testkit::World;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const SRC: &str = "5e1ec7ed-0000-4000-8000-000000000001";

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
fn record(ty: &str, uuid: &str, parent: Option<&str>, at: u32, body: Value) -> Value {
    let mut v = json!({"type": ty, "uuid": uuid, "parentUuid": parent, "sessionId": SRC, "session_id": SRC,
        "timestamp": format!("2026-03-01T10:00:{at:02}Z"), "cwd": "/work/proj", "userType": "external",
        "entrypoint": "cli", "version": "2.1.327", "isSidechain": false, "slug": "tidy-garden"});
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    v
}

/// A prompt, a call and its result, then what a person and the harness put in mid-turn: a
/// message typed while the agent worked, a file attached with `@`, a hook's output, a hook that
/// printed nothing (no message), a slash command's notice; then the reply.
fn mid_turn_session(w: &World) -> std::path::PathBuf {
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
    // One session: every spelling of the id is the new one, and the chain is whole.
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(SRC), "a record still names the source session");
    for r in &written {
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
fn planned_session(w: &World) -> std::path::PathBuf {
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
            "results": {"t_exc": {"action": "excerpt", "lines": ["61-70"]}, "t_keep": {"action": "keep"}},
            "messages": {"nope": {"action": "drop"}, "a1": {"action": "drop"}, "u2": {"action": "excerpt"}}
        }),
    );
    let (ok, code, _, err) = w.cv(&["prune", PLAN, "--plan", &bad]);
    assert!(!ok && code == 2, "{err}");
    for problem in [
        "t_exc: line 61 is past the payload's 60 line(s)",
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

// ───────────────────────────── prune: the count it prints ─────────────────────────────

/// A session whose context shrank before the prune: an early request recorded 900k, the last
/// one 500k. Revive rewrites the records above its estimate; the harness sizes a resume by the
/// LAST record. The report names both, and what each number counted.
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
        ],
    );
    let (out, err) = w.cv_ok(&["prune", SID, "--keep-last", "0", "--min-size", "1024", "--json"]);
    let report: Value = serde_json::from_str(out.trim()).unwrap();
    // What a resume reads: the last request's input + cache read + cache creation.
    assert_eq!(report["resume_reads_tokens_before"], 500_000, "{report}");
    assert_eq!(report["resume_reads_tokens_after"], 500_000, "{report}");
    // What revive rewrote: the one record above its estimate, which is not the last.
    assert_eq!(report["revived"]["usage_records_rewritten"], 1, "{report}");
    assert_eq!(report["revived"]["recorded_tokens_before"], 900_000, "{report}");
    let estimate = report["revived"]["recorded_tokens_after"].as_u64().unwrap();
    assert!((500_000..900_000).contains(&estimate), "{report}");
    assert!(
        err.contains(
            "1 usage record(s) recorded above the post-prune estimate were rewritten to it: highest recorded 900k"
        ),
        "{err}"
    );
    assert!(
        err.contains(
            "resume reads the last request's recorded size (input + cache read + cache creation): 500k, unchanged"
        ),
        "{err}"
    );
    // The new transcript agrees with the report.
    let path = source.with_file_name(format!("{}.jsonl", report["new_id"].as_str().unwrap()));
    let last = rows(&path).into_iter().rfind(|r| r["uuid"] == "a3").unwrap();
    let u = &last["message"]["usage"];
    let sum = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .map(|k| u[*k].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(sum, 500_000);
}
