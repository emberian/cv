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

/// #tooth A query naming a turn by what it did (`file stem + command + failure`) finds that turn
/// through its indexed facets (its raw text lacks `failed`), ranks it above a turn that mentions
/// each word once, and the hit's snippet is still the transcript's own text. A turn repeating
/// every word still outranks it: facets add recall and weight, not a guarantee of first place.
#[test]
fn facet_keys_match_a_failed_command_turn_while_snippets_stay_raw() {
    let w = World::new("facet-keys");
    let lines = [
        serde_json::json!({"type": "user", "uuid": "f1", "timestamp": TS, "sessionId": "facetsess",
            "message": {"role": "user", "content": "tidy the module"}}),
        serde_json::json!({"type": "assistant", "uuid": "f2", "timestamp": TS, "sessionId": "facetsess",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Edit",
                 "input": {"file_path": "crates/cv-core/src/custody.rs", "old_string": "a", "new_string": "b"}},
                {"type": "tool_use", "id": "t2", "name": "Bash",
                 "input": {"command": "cargo clippy --workspace -- -D warnings"}}]}}),
        serde_json::json!({"type": "user", "uuid": "f3", "timestamp": TS, "sessionId": "facetsess",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "is_error": true,
                 "content": "warning: unused variable `x`"}]}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(w.transcript("facetsess"), body).unwrap();
    // A decoy whose words alone cover the query once each: it talks about a failure, did nothing.
    let decoy = serde_json::json!({"type": "user", "uuid": "d1", "timestamp": TS, "sessionId": "decoysess",
        "message": {"role": "user", "content": "yesterday custody rs went fine but cargo clippy failed on another branch"}});
    fs::write(w.transcript("decoysess"), format!("{decoy}\n")).unwrap();
    w.cv_ok(&["index"]);
    let (out, err) = w.cv_ok(&["search", "custody.rs cargo clippy failed", "--json"]);
    let found: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}\n{err}"));
    let hits = found["hits"].as_array().unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"facetsess"),
        "the failed turn matches by its facets: {found}"
    );
    assert_eq!(
        ids,
        ["facetsess", "decoysess"],
        "the turn that did it outranks talk about it"
    );
    let hit = hits.iter().find(|h| h["id"] == "facetsess").unwrap();
    let snippet = hit["snippet"].as_str().unwrap();
    assert!(
        !snippet.contains("exit_") && !snippet.contains("error failed"),
        "{snippet}"
    );
    // The facet field never leaks into what a reader sees of the session.
    let (show, _) = w.cv_ok(&["show", "facetsess", "--json"]);
    assert!(!show.contains("error failed"));
}

/// #tooth A transcript rewritten in place and then grown past its old size reads like an append
/// by length alone. The index must still retract the replaced text: no ghost hit survives,
/// whether the rewrite shrank it first or replaced bytes at the same length.
#[test]
fn rewritten_then_grown_transcript_retracts_replaced_text_from_search() {
    let w = World::new("rewrite-grow");
    let line = |uuid: &str, text: &str| {
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "uuid": uuid, "timestamp": TS, "sessionId": "rwsess",
                "message": {"role": "user", "content": text}})
        )
    };
    let path = w.transcript("rwsess");
    let filler = "padding words to make the line long enough ".repeat(40);
    fs::write(
        &path,
        line("r1", &format!("quetzal {filler}")) + &line("r2", "second turn tapir"),
    )
    .unwrap();
    w.cv_ok(&["index"]);
    let total = |q: &str| {
        let (out, _) = w.cv_ok(&["search", q, "--json"]);
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["total"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(total("quetzal"), 1);
    // Same length prefix, different first word, then more turns appended.
    let rewritten = line("r1", &format!("iguanas {filler}")) + &line("r2", "second turn tapir");
    fs::write(&path, rewritten.clone() + &line("r3", "third turn okapi")).unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    w.cv_ok(&["index"]);
    assert_eq!(total("quetzal"), 0, "the replaced word is retracted");
    assert_eq!(total("iguanas"), 1);
    assert_eq!(total("okapi"), 1);
    assert_eq!(total("tapir"), 1);
    // Shrink below the old prefix, then grow past it again.
    fs::write(&path, line("r1", "short")).unwrap();
    w.cv_ok(&["index"]);
    fs::write(
        &path,
        line("r1", "short") + &line("r9", &format!("anteater {filler} {filler}")),
    )
    .unwrap();
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(bumped))
        .unwrap();
    w.cv_ok(&["index"]);
    for gone in ["iguanas", "okapi", "tapir"] {
        assert_eq!(total(gone), 0, "{gone} lingers as a ghost hit");
    }
    assert_eq!(total("anteater"), 1);
}

/// #tooth A session is found by its git branch (session metadata, on its meta doc) and a turn
/// by the short SHA its `git commit` reported, neither of which is in any message's text.
#[test]
fn git_branch_and_commit_sha_facets_find_the_session_and_the_committing_turn() {
    let w = World::new("git-facets");
    let lines = [
        serde_json::json!({"type": "user", "uuid": "g1", "timestamp": TS, "sessionId": "gitsess",
            "gitBranch": "feat/heron-otter", "message": {"role": "user", "content": "save the work"}}),
        serde_json::json!({"type": "assistant", "uuid": "g2", "timestamp": TS, "sessionId": "gitsess",
            "gitBranch": "feat/heron-otter", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "c1", "name": "Bash",
                 "input": {"command": "git checkout -b topic-kestrel && git commit -am 'tidy'"}}]}}),
        serde_json::json!({"type": "user", "uuid": "g3", "timestamp": TS, "sessionId": "gitsess",
            "gitBranch": "feat/heron-otter", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1",
                 "content": "Switched to a new branch\n[topic-kestrel 9e1d0c7] tidy\n 1 file changed"}]}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(w.transcript("gitsess"), body).unwrap();
    fs::write(
        w.transcript("othersess"),
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "uuid": "o1", "timestamp": TS, "sessionId": "othersess",
                "message": {"role": "user", "content": "nothing about version control"}})
        ),
    )
    .unwrap();
    w.cv_ok(&["index"]);
    let ids = |query: &str| -> Vec<String> {
        let (out, _) = w.cv_ok(&["search", query, "--json"]);
        let found: serde_json::Value = serde_json::from_str(&out).unwrap();
        found["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids("heron otter"), ["gitsess"], "the session branch");
    assert_eq!(ids("9e1d0c7"), ["gitsess"], "the short SHA the commit reported");
    assert_eq!(ids("kestrel"), ["gitsess"], "the branch the command created");
}
