//! `show --kind files` and `GET /api/session/{harness}/{id}/files`: one timeline, verified against
//! a real (temp) git repository and checkout.
use cv_testkit::{claude, World};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

/// 2026-01-01T00:00:00Z, the first touch.
const T0: i64 = 1_767_225_600;

fn git(repo: &Path, args: &[&str], date: Option<i64>) {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(args);
    if let Some(d) = date {
        let d = format!("{d} +0000");
        c.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(T0 + secs, 0).unwrap().to_rfc3339()
}

fn call(n: i64, id: &str, name: &str, input: Value) -> Value {
    claude::assistant(
        &format!("a{n}"),
        &ts(n),
        "m",
        json!([claude::tool_use(id, name, input)]),
    )
}

fn ok(n: i64, id: &str) -> Value {
    claude::tool_result(&format!("r{n}"), &ts(n), id, "done")
}

/// The session and repository: a write superseded by an edit that was committed, an edit with no
/// known base whose replacement was committed, a read, a failed edit, and a write left on disk.
fn scene(w: &World) -> std::path::PathBuf {
    let repo = w.base.canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"], None);
    for (k, v) in [
        ("user.name", "T"),
        ("user.email", "t@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", k, v], None);
    }
    let a = repo.join("src/a.rs");
    let replacement = "let important_value = compute_answer();";
    let mut prompt = claude::user("files-view", "u0", &ts(0), "go");
    prompt["cwd"] = json!(repo);
    let mut failed = claude::tool_result("r9", &ts(9), "t5", "old_string not found");
    failed["message"]["content"][0]["is_error"] = json!(true);
    w.write_session(
        "files-view",
        &[
            prompt,
            call(
                1,
                "t1",
                "Write",
                json!({"file_path": a, "content": "fn a() {}\nfn b() {}\n"}),
            ),
            ok(2, "t1"),
            call(
                3,
                "t2",
                "Edit",
                json!({"file_path": a, "old_string": "fn b() {}", "new_string": "fn b() -> u8 {\n    7\n}"}),
            ),
            ok(4, "t2"),
            call(
                5,
                "t3",
                "Edit",
                json!({"file_path": repo.join("src/c.rs"), "old_string": "todo!()", "new_string": replacement}),
            ),
            ok(6, "t3"),
            call(7, "t4", "Read", json!({"file_path": a})),
            ok(8, "t4"),
            call(
                8,
                "t5",
                "Edit",
                json!({"file_path": a, "old_string": "nope", "new_string": "never"}),
            ),
            failed,
            call(
                10,
                "t6",
                "Write",
                json!({"file_path": repo.join("notes.txt"), "content": "on disk only\n"}),
            ),
            ok(11, "t6"),
        ],
    );
    std::fs::write(&a, "fn a() {}\nfn b() -> u8 {\n    7\n}\n").unwrap();
    std::fs::write(repo.join("src/c.rs"), format!("fn c() {{\n    {replacement}\n}}\n")).unwrap();
    git(&repo, &["add", "src"], None);
    git(&repo, &["commit", "-qm", "agent work"], Some(T0 + 600));
    std::fs::write(repo.join("notes.txt"), "on disk only\n").unwrap();
    repo
}

fn rows(w: &World, extra: &[&str]) -> Value {
    let mut args = vec!["show", "files-view", "--kind", "files", "--json"];
    args.extend(extra);
    serde_json::from_str(&w.cv_ok(&args).0).unwrap()
}

#[test]
fn files_view_verifies_touches_against_git_disk_and_the_transcript() {
    // #tooth Each verification level appears once, on the touch that earns it, through both doors.
    let w = World::in_process("files-view", cv_core::env::Env::enter_isolated);
    scene(&w);
    let all = rows(&w, &[]);
    let got: Vec<(u64, &str, &str, Value, &str)> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["msg_idx"].as_u64().unwrap(),
                r["op"].as_str().unwrap(),
                r["path"].as_str().unwrap(),
                r["succeeded"].clone(),
                r["verification"]["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "write", "src/a.rs", json!(true), "reconstructed"),
            (3, "edit", "src/a.rs", json!(true), "verified_git"),
            (5, "edit", "src/c.rs", json!(true), "verified_git"),
            (7, "read", "src/a.rs", json!(true), "unverified"),
            (9, "edit", "src/a.rs", json!(false), "unverified"),
            (11, "write", "notes.txt", json!(true), "verified_disk"),
        ],
        "{all:#}"
    );
    assert_eq!(all[1]["lines_changed"], json!([3, 1]));
    assert_eq!(all[1]["tool_use_id"], "t2");
    assert_eq!(all[1]["verification"]["blob"].as_str().unwrap().len(), 40);
    assert_eq!(all[1]["verification"]["commit"], all[2]["verification"]["commit"]);
    assert_eq!(all[5]["verification"]["blake3"].as_str().unwrap().len(), 64);

    let (schema, _) = w.cv_ok(&["schema", "--json"]);
    let schema: Value = serde_json::from_str(&schema).unwrap();
    let keys: Vec<&str> = schema["file_touch"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    for row in all.as_array().unwrap() {
        let mut have: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = keys.clone();
        have.sort_unstable();
        want.sort_unstable();
        assert_eq!(have, want);
    }

    let path = &["api", "session", "claude", "files-view", "files"];
    for (flags, query) in [
        (vec![], ""),
        (vec!["--last", "2"], "last=2"),
        (vec!["--around", "5", "--context", "1"], "around=5&context=1"),
    ] {
        let (status, body) = cv_core::api::route_json(path, query);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, rows(&w, &flags), "{query}");
    }
    assert_eq!(cv_core::api::route_json(path, "last=1&first=1").0, 400);

    let (text, _) = w.cv_ok(&["show", "files-view", "--kind", "files"]);
    assert!(text.contains("edit  ok  src/a.rs  +3 -1  git "), "{text}");
    assert!(text.contains("edit  ERR src/a.rs"), "{text}");
    assert!(text.contains("notes.txt  +1 -0  disk"), "{text}");
}

#[test]
fn files_view_keeps_the_recorded_commit_after_later_changes_and_skips_disk_when_frozen() {
    // #tooth A later commit rewrites src/a.rs: the edit stays verified at the commit that held
    // it, not at HEAD. Frozen, the checkout is never read: the disk-only write is reconstructed.
    let w = World::in_process("files-view-later", cv_core::env::Env::enter_isolated);
    let repo = scene(&w);
    let head = |repo: &Path| {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let recorded = head(&repo);
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["commit", "-qam", "human rewrite"], Some(T0 + 7200));
    assert_ne!(head(&repo), recorded);

    let verification = |all: &Value, i: usize| all[i]["verification"].clone();
    let all = rows(&w, &[]);
    assert_eq!(verification(&all, 1)["kind"], "verified_git", "{all:#}");
    assert_eq!(verification(&all, 1)["commit"], recorded.as_str());
    assert_eq!(verification(&all, 5)["kind"], "verified_disk");

    let frozen = rows(&w, &["--frozen"]);
    assert_eq!(verification(&frozen, 1), verification(&all, 1));
    assert_eq!(verification(&frozen, 2), verification(&all, 2));
    assert_eq!(verification(&frozen, 5)["kind"], "reconstructed", "{frozen:#}");
}
