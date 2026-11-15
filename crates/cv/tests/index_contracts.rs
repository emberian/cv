//! Index publication contracts driven through the real binary: what `cv index` may prune,
//! what a second writer does, and what a search answers while the stores move underneath it.
//!
//! Hermetic through `cv_testkit::World` (own `$HOME` + `$CLUSTERVISION_HOME`).

use cv_testkit::{claude, World};
use std::fs;

const TS: &str = "2026-06-07T12:00:00Z";

fn index_json(w: &World, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["index", "--json"];
    all.extend_from_slice(args);
    let (out, err) = w.cv_ok(&all);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("index --json: {e}\n{out}\n{err}"))
}

fn frozen_total(w: &World, query: &str) -> u64 {
    let (out, err) = w.cv_ok(&["--frozen", "search", query, "--json"]);
    let found: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}\n{err}"));
    found["total"].as_u64().unwrap()
}

/// A store root that is not there (an unmounted volume, a home restored without it) is not
/// an inventory of zero sessions: the sessions indexed from it stay searchable and return to
/// fresh when the root does. A file removed from a root that is there is still reaped.
#[test]
fn an_absent_harness_root_does_not_prune_its_indexed_sessions() {
    let w = World::new("index-absent-root");
    w.write_session(
        "kept",
        &[claude::user("kept", "u1", TS, "zqkeptword in a mounted store")],
    );
    w.write_session(
        "gone",
        &[claude::user("gone", "u1", TS, "zqgoneword removed on purpose")],
    );
    let first = index_json(&w, &["--exact"]);
    assert_eq!(
        (first["indexed"].as_u64(), first["removed"].as_u64()),
        (Some(2), Some(0))
    );
    assert_eq!(frozen_total(&w, "zqkeptword"), 1);

    let claude_home = w.home.join(".claude");
    let aside = w.base.join("unmounted-claude");
    fs::rename(&claude_home, &aside).unwrap();
    let (ok, _, out, err) = w.cv(&["index", "--json", "--exact"]);
    let pass: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}\n{err}"));
    assert_eq!(pass["removed"].as_u64(), Some(0), "an absent root pruned: {out}\n{err}");
    assert!(ok, "{err}");
    assert!(
        err.contains("absent") && err.contains("2 indexed session"),
        "the pass names what it kept and why: {err}"
    );
    assert_eq!(frozen_total(&w, "zqkeptword"), 1);
    assert_eq!(frozen_total(&w, "zqgoneword"), 1);

    fs::rename(&aside, &claude_home).unwrap();
    let (out, err) = w.cv_ok(&["index", "--json", "--exact"]);
    assert!(
        err.contains("trigram: 0 indexed, 2 unchanged"),
        "the exact index kept both: {err}"
    );
    let back: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (
            back["indexed"].as_u64(),
            back["changed"].as_u64(),
            back["removed"].as_u64()
        ),
        (Some(2), Some(0), Some(0)),
        "a returned root is the same sessions, unread: {back}"
    );

    fs::remove_file(w.transcript("gone")).unwrap();
    let reaped = index_json(&w, &[]);
    assert_eq!(reaped["removed"].as_u64(), Some(1), "{reaped}");
    assert_eq!(frozen_total(&w, "zqgoneword"), 0);
    assert_eq!(frozen_total(&w, "zqkeptword"), 1);
}
