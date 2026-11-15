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

fn hit_ids(w: &World, query: &str) -> Vec<String> {
    let (out, err) = w.cv_ok(&["--frozen", "search", query, "--json", "--limit", "100"]);
    let found: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}\n{err}"));
    let mut ids: Vec<String> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// `cv index` processes started together over one home: none corrupts the index, a process
/// that loses the writer says so, every session ends up indexed once, and the pass after
/// them finds nothing to do.
#[test]
fn concurrent_index_processes_leave_one_complete_index() {
    let w = World::new("index-concurrent");
    let ids: Vec<String> = (0..24).map(|i| format!("conc{i:02}")).collect();
    for id in &ids {
        let body = format!("zqshared zq{id} {}", "filler words ".repeat(400));
        w.write_session(id, &[claude::user(id, "u1", TS, &body)]);
    }
    let spawned: Vec<_> = (0..4)
        .map(|_| {
            w.cv_command()
                .args(["index", "--json"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut succeeded = 0;
    for child in spawned {
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{err}");
        assert!(out.status.code().is_some(), "killed by a signal: {err}");
        if out.status.success() {
            succeeded += 1;
        } else {
            assert!(
                err.contains("lock") || err.contains("another") || err.contains("retry"),
                "an index that failed beside another names the contention: {err}"
            );
        }
    }
    assert!(succeeded >= 1, "no concurrent index succeeded");
    let after = index_json(&w, &[]);
    assert_eq!(
        (
            after["indexed"].as_u64(),
            after["changed"].as_u64(),
            after["errors"].as_u64()
        ),
        (Some(24), Some(0), Some(0)),
        "{after}"
    );
    assert_eq!(hit_ids(&w, "zqshared"), ids);
    for id in &ids {
        assert_eq!(hit_ids(&w, &format!("zq{id}")), vec![id.clone()], "{id}");
    }
}

/// A transcript that shrinks, is rewritten, or is deleted and written again under the same
/// id is the session it is now: nothing of what it held before stays searchable.
#[test]
fn a_shrunk_rewritten_or_recreated_session_is_indexed_as_it_is_now() {
    let w = World::new("index-rewrites");
    let long = |word: &str| -> Vec<serde_json::Value> {
        (0..6)
            .map(|i| {
                claude::user(
                    "s",
                    &format!("u{i}"),
                    TS,
                    &format!("{word}{i} {}", "padding ".repeat(300)),
                )
            })
            .collect()
    };
    w.write_session("s", &long("zqoriginal"));
    index_json(&w, &[]);
    assert_eq!(frozen_total(&w, "zqoriginal5"), 1);

    // Shrink: the last three records are gone.
    w.write_session("s", &long("zqoriginal")[..3]);
    let shrunk = index_json(&w, &[]);
    assert_eq!(
        (shrunk["changed"].as_u64(), shrunk["removed"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(frozen_total(&w, "zqoriginal5"), 0, "a truncated tail stayed indexed");
    assert_eq!(frozen_total(&w, "zqoriginal2"), 1);

    // Rewritten longer with other words: growth of another body is not an append.
    w.write_session("s", &long("zqrewritten"));
    index_json(&w, &[]);
    assert_eq!(frozen_total(&w, "zqoriginal2"), 0, "a rewritten prefix stayed indexed");
    assert_eq!(frozen_total(&w, "zqrewritten5"), 1);

    // Deleted, indexed as gone, then recreated under the same id.
    fs::remove_file(w.transcript("s")).unwrap();
    let gone = index_json(&w, &[]);
    assert_eq!(gone["removed"].as_u64(), Some(1), "{gone}");
    assert_eq!(frozen_total(&w, "zqrewritten5"), 0);
    w.write_session("s", &long("zqreborn"));
    let reborn = index_json(&w, &[]);
    assert_eq!(
        (reborn["indexed"].as_u64(), reborn["changed"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(frozen_total(&w, "zqreborn0"), 1);
    assert_eq!(frozen_total(&w, "zqrewritten5"), 0);

    // Deleted and recreated between two passes: the pass sees only the new file.
    fs::remove_file(w.transcript("s")).unwrap();
    w.write_session("s", &long("zqsecondlife"));
    index_json(&w, &[]);
    assert_eq!(
        frozen_total(&w, "zqreborn0"),
        0,
        "the previous file's words stayed indexed"
    );
    assert_eq!(frozen_total(&w, "zqsecondlife4"), 1);
}

/// A session whose replacement cannot be read keeps the revision the index last read, the
/// pass names the session, `--strict` fails, and the next readable pass replaces it.
#[cfg(unix)]
#[test]
fn an_unreadable_replacement_keeps_the_last_indexed_revision() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("index-last-good");
    w.write_session("ok", &[claude::user("ok", "u1", TS, "zqhealthy session")]);
    let path = w.write_session("bad", &[claude::user("bad", "u1", TS, "zqlastgood revision")]);
    index_json(&w, &[]);

    fs::write(
        &path,
        cv_testkit::jsonl(&[
            claude::user("bad", "u1", TS, "zqlastgood revision"),
            claude::user("bad", "u2", TS, "zqreplacement text"),
        ]),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let (ok, _, out, err) = w.cv(&["index", "--json"]);
    let (strict_ok, _, _, strict_err) = w.cv(&["index", "--strict"]);
    let kept = frozen_total(&w, "zqlastgood");
    let healthy = frozen_total(&w, "zqhealthy");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(kept, 1, "the last good revision is gone: {out}\n{err}");
    assert_eq!(healthy, 1);
    assert!(
        !ok || out.contains("\"bad\"") || err.contains("bad"),
        "a pass that could not read a session names it: {out}\n{err}"
    );
    assert!(!strict_ok, "--strict passed over an unreadable session: {strict_err}");

    let healed = index_json(&w, &[]);
    assert_eq!(
        (healed["errors"].as_u64(), healed["removed"].as_u64()),
        (Some(0), Some(0)),
        "{healed}"
    );
    assert_eq!(frozen_total(&w, "zqreplacement"), 1);
    assert_eq!(frozen_total(&w, "zqlastgood"), 1);
}

/// A project directory that cannot be listed makes the inventory incomplete: its sessions
/// are not reaped, and the pass does not report a clean result.
#[cfg(unix)]
#[test]
fn an_unlistable_project_directory_does_not_prune_its_sessions() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("index-unlistable");
    w.write_session("here", &[claude::user("here", "u1", TS, "zqlisted session")]);
    let other = w.home.join(".claude/projects/-work-other");
    fs::create_dir_all(&other).unwrap();
    fs::write(
        other.join("hidden.jsonl"),
        cv_testkit::jsonl(&[claude::user("hidden", "u1", TS, "zqunlisted session")]),
    )
    .unwrap();
    let first = index_json(&w, &[]);
    assert_eq!(first["indexed"].as_u64(), Some(2));

    fs::set_permissions(&other, fs::Permissions::from_mode(0o000)).unwrap();
    let (ok, _, out, err) = w.cv(&["index", "--json"]);
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        frozen_total(&w, "zqunlisted"),
        1,
        "an unlistable directory pruned: {out}\n{err}"
    );
    assert!(
        !ok || err.contains("incomplete") || err.contains("could not"),
        "an incomplete inventory passed silently: {out}\n{err}"
    );
    let back = index_json(&w, &[]);
    assert_eq!(
        (back["indexed"].as_u64(), back["removed"].as_u64()),
        (Some(2), Some(0)),
        "{back}"
    );
}
