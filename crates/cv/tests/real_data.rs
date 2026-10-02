//! Acceptance against real data on the machine that built these commands — the orchestrating
//! session of 2026-09-30/10-01 (`0c315aee…`, ~164 sub-agents) and the live task store. These are
//! `#[ignore]`d: they run only when asked (`cargo test --test real_data -- --ignored`) and FAIL
//! when the data is not there, so they can never pass by skipping. The hermetic suites
//! (`orchestrate.rs`, `cli.rs`) are the gate; this file is the evidence the gate was aimed at
//! something real.

use std::path::PathBuf;
use std::process::Command;

const SESSION: &str = "0c315aee";

fn session_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    home.join(".claude/projects/-Users-ember-dev-breadstuffs/0c315aee-e07a-499c-8bb4-fa36730510ca.jsonl")
}

fn cv(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(args)
        .output()
        .expect("cv runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn require_real_session() {
    let p = session_path();
    assert!(p.exists(), "the real session is not on this machine: {}", p.display());
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_prompts_are_the_38_human_lines_and_6_answers() {
    require_real_session();
    let (code, out) = cv(&["prompts", SESSION, "--json"]);
    assert_eq!(code, 0);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let prompts = rows.iter().filter(|r| r["kind"] == "prompt").count();
    let answers = rows.iter().filter(|r| r["kind"] == "answer").count();
    // The session is still being written to: these are the counts at the time the command was
    // built (38 / 6), as lower bounds.
    assert!(prompts >= 38 && answers >= 6, "{prompts} prompts, {answers} answers");
    assert!(rows[0]["text"].as_str().unwrap().starts_with("ok we have 13m to burn"));
    assert!(!rows
        .iter()
        .any(|r| r["text"].as_str().unwrap().starts_with("<command-name>")));
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_lanes_census_matches_the_transcripts() {
    require_real_session();
    let (code, out) = cv(&["lanes", SESSION, "--json"]);
    assert_eq!(code, 0);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(rows.len() >= 164, "{} lanes", rows.len());
    let running = rows.iter().filter(|r| r["status"] == "running").count();
    // `returned` = finished with no stop recorded (a lost notification): done, not running.
    let completed = rows
        .iter()
        .filter(|r| r["status"] == "completed" || r["status"] == "returned")
        .count();
    assert!(completed >= 150, "{completed} completed");
    // A live orchestrator keeps spawning; what must hold is that "running" is not the whole
    // forest (the lost-notification class would make it so after a harness restart).
    assert!(running < rows.len() / 2, "{running} running of {}", rows.len());
    // Every lane parsed its model and spent tokens.
    assert!(rows
        .iter()
        .all(|r| r["model"].as_str().is_some_and(|m| m.starts_with("claude-"))));
    assert!(rows.iter().all(|r| r["tokens"]["total"].as_u64().unwrap() > 0));
}

#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_deferrals_cross_reference_the_live_task_store() {
    require_real_session();
    let (_, out) = cv(&["deferrals", SESSION, "--open-tasks", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(rows.len() >= 40, "{} deferrals", rows.len());
    let matched = rows.iter().filter(|r| r["matched"].is_object()).count();
    assert!(matched >= 20, "{matched} matched");
    assert!(rows.iter().any(|r| r["phrase"] == "after FINAL"));
}

// ───────────────────────────── 0.13: decisions, the inbox, the feed, the page ─────────────────────────────

/// The task `cv task split` was built against: ember's consolidated "everything only ember can
/// do" task, which carried the day's DECIDE notes.
const EMBER_TASK: &str = "01a0f54c-d1dd";

fn require_real_store() {
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let log = home.join(".clustervision/tasks/events.jsonl");
    assert!(
        log.exists(),
        "the real task store is not on this machine: {}",
        log.display()
    );
}

/// After `cv task split 01a0f54c-d1dd` ran (the acceptance step), ember's inbox leads with the
/// split decisions, each carrying its default; the Markdown page renders them.
#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_inbox_ember_lists_the_split_decisions() {
    require_real_store();
    let (code, out) = cv(&["task", "show", EMBER_TASK, "--json"]);
    assert_eq!(code, 0, "{out}");
    let parent: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        parent["blocked_by"].as_array().is_some_and(|b| b.len() >= 8),
        "the parent is blocked by its split decisions: {}",
        parent["blocked_by"]
    );
    let (code, out) = cv(&["task", "inbox", "ember", "--json"]);
    assert_eq!(code, 0, "{out}");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let decisions: Vec<&serde_json::Value> = entries.iter().filter(|e| e["reason"] == "decision_owed").collect();
    assert!(decisions.len() >= 8, "{} decisions owed", decisions.len());
    assert!(
        decisions.iter().all(|e| e["task"]["decision"]["default"].is_string()),
        "every split decision carries a default"
    );
    assert!(decisions
        .iter()
        .any(|e| e["task"]["title"].as_str().unwrap().starts_with("K-PORTAL: who births")));
    let (code, out) = cv(&["task", "inbox", "ember", "--md"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("# Inbox for ember — "), "{}", &out[..60.min(out.len())]);
    assert!(
        out.contains("## Decisions owed (") && out.contains("- **default:** "),
        "{out}"
    );
    let (code, out) = cv(&["task", "inbox", "ember"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("decisions owed ("), "{out}");
}

/// The feed over the real log: every line is a JSON event with its task's title.
#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_events_since_a_day_parse_as_json_lines() {
    require_real_store();
    let (code, out) = cv(&["task", "events", "--since", "2d"]);
    assert_eq!(code, 0);
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert!(!rows.is_empty(), "no events in two days?");
    assert!(rows.iter().all(|r| r["event"].is_string() && r["title"].is_string()));
    let last = rows.last().unwrap()["id"].as_str().unwrap();
    let (code, out) = cv(&["task", "events", "--since", last]);
    assert_eq!(code, 0);
    assert!(out.trim().is_empty(), "an id cursor is exclusive: {out}");
}

/// `cv task serve` over the real store, curled: the page's inbox for ember is the CLI's.
#[test]
#[ignore = "real data on ember's laptop; run with --ignored"]
fn real_serve_curls_ember_inbox() {
    use std::io::{BufRead, BufReader};
    require_real_store();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(["task", "serve", "--bind", "127.0.0.1:0", "--assignee", "ember"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("serve starts");
    let banner = BufReader::new(child.stderr.take().unwrap())
        .lines()
        .next()
        .unwrap()
        .unwrap();
    let addr = banner
        .split("http://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap()
        .to_string();
    let curl = Command::new("curl")
        .args(["-sS", &format!("http://{addr}/api/inbox?who=ember")])
        .output()
        .expect("curl runs");
    let _ = child.kill();
    let _ = child.wait();
    let body = String::from_utf8_lossy(&curl.stdout);
    let page: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(page["who"], "ember");
    assert!(page["counts"]["decisions"].as_u64().unwrap() >= 8, "{}", page["counts"]);
    let (code, out) = cv(&["task", "inbox", "ember", "--json"]);
    assert_eq!(code, 0);
    let cli: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(
        page["items"].as_array().unwrap().len(),
        cli.len(),
        "the page and the CLI list the same inbox"
    );
}
