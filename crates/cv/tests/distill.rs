//! Publication must follow budget and unfinished-work checks.
use cv_testkit::{claude, World};
use serde_json::json;

fn source(w: &World, id: &str, open: bool) {
    let mut rows = vec![claude::user(id, "u", "2026-10-01T00:00:00Z", "test the handoff")];
    rows.push(claude::assistant(
        "a",
        "2026-10-01T00:00:01Z",
        "test",
        if open {
            json!([{"type":"tool_use","id":"build","name":"Bash","input":{"command":"cargo test"}}])
        } else {
            json!([{"type":"text","text":"The resulting finding is retained."}])
        },
    ));
    w.write_session(id, &rows);
}

#[test]
fn distill_budget_checks_aggregate_before_publishing() {
    let w = World::new("distill-budget");
    source(&w, "budget-source", false);
    source(&w, "budget-other", false);
    let (ok, _, pack, err) = w.cv(&["distill", "budget-source"]);
    assert!(ok, "{err}");
    let limit = pack.len().to_string();
    let output = w.base.join("derived");
    let markdown = w.base.join("handoff.md");
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "budget-source",
        "--with",
        "budget-other",
        "--max-bytes",
        &limit,
        "--session",
        "--out",
        output.to_str().unwrap(),
        "--pack",
        markdown.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(err.contains("exceeding --max-bytes"), "{err}");
    assert!(!output.exists());
    assert!(!markdown.exists());
}

#[test]
fn distill_unsettled_refuses_resume_but_allows_explicit_snapshot() {
    let w = World::new("distill-unsettled");
    source(&w, "unsettled-source", true);
    let output = w.base.join("derived");
    let (ok, _, _, err) = w.cv(&[
        "distill",
        "unsettled-source",
        "--session",
        "--out",
        output.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(err.contains("1 calls without terminal results"), "{err}");
    assert!(!output.exists());
    let (ok, _, pack, err) = w.cv(&["distill", "unsettled-source"]);
    assert!(ok, "{err}");
    assert!(pack.contains("## Unsettled calls"));
    let (ok, _, report, err) = w.cv(&[
        "distill",
        "unsettled-source",
        "--session",
        "--out",
        output.to_str().unwrap(),
        "--allow-unsettled",
        "--json",
    ]);
    assert!(ok, "{err}");
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    // --out is a store root; the Claude emitter places main-thread transcripts in a
    // project directory beneath it. Follow the published path instead of guessing layout.
    let transcript = std::path::PathBuf::from(report["emitted"]["path"].as_str().unwrap());
    assert!(transcript.starts_with(&output), "{report}");
    let transcript = std::fs::read_to_string(transcript).unwrap();
    assert!(transcript.contains("Unsettled calls"));
    assert!(transcript.contains("`build`"));
}
