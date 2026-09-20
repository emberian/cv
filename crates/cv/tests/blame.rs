//! End-to-end `cv blame`: a real (temp) git repo with controlled commit times + a temp
//! CLUSTERVISION_HOME whose catalog is fed by ingesting a synthetic claude session — then the
//! actual binary is run against it. Everything lives in ONE test fn because CLUSTERVISION_HOME is
//! process-global state for the in-process cv-core calls (parallel test fns would race it).

use std::path::Path;
use std::process::Command;
use std::{fs, str};

/// 2026-06-01T12:00:00Z — the timestamp on the session's Edit message.
const EDIT_TS: i64 = 1_780_315_200;
/// The agent edits, the human commits 8 minutes later.
const COMMIT1_TS: i64 = EDIT_TS + 8 * 60;
/// A second commit two days later that no session can claim.
const COMMIT2_TS: i64 = EDIT_TS + 2 * 86_400;

fn git(repo: &Path, args: &[&str], date: Option<i64>) {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(args);
    if let Some(d) = date {
        let d = format!("{d} +0000");
        c.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
    }
    let out = c.output().expect("git should run");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run the built `cv` binary with the temp catalog home; returns (stdout, stderr).
fn cv(home: &Path, dir: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cv"))
        .args(args)
        .current_dir(dir)
        .env("CLUSTERVISION_HOME", home)
        .output()
        .expect("cv should run");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn blame_end_to_end() {
    let base = std::env::temp_dir().join(format!(
        "cv-blame-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = base.join("home");
    let repo_raw = base.join("repo");
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::create_dir_all(repo_raw.join("src")).unwrap();
    // Canonicalize so /var vs /private/var (macOS temp symlink) agrees with git's toplevel.
    let repo = repo_raw.canonicalize().unwrap();

    // --- a real repo with two commits at controlled times ---
    git(&repo, &["init", "-q"], None);
    git(&repo, &["config", "user.email", "t@example.com"], None);
    git(&repo, &["config", "user.name", "t"], None);
    git(&repo, &["config", "commit.gpgsign", "false"], None);
    fs::write(repo.join("src/widget.rs"), "fn widget() {}\nfn extra() {}\n").unwrap();
    git(&repo, &["add", "."], None);
    git(&repo, &["commit", "-qm", "feat: add widget"], Some(COMMIT1_TS));
    fs::write(repo.join("src/widget.rs"), "fn widget() { 2 }\nfn extra() {}\n").unwrap();
    git(&repo, &["commit", "-aqm", "chore: tweak widget"], Some(COMMIT2_TS));

    // --- a synthetic claude session that edited the same file in a DIFFERENT checkout ---
    // (cwd /historical/checkout: only the repo-relative suffix can match it — the robust path.)
    let jsonl = home.join("sessions/blame-e2e.jsonl");
    let lines = [
        serde_json::json!({
            "type": "user", "uuid": "u0", "sessionId": "blame-e2e",
            "message": {"role": "user", "content": "please add the widget"}
        }),
        serde_json::json!({
            "type": "assistant", "uuid": "a1", "sessionId": "blame-e2e",
            "timestamp": "2026-06-01T12:00:00Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "adding the widget now"},
                {"type": "tool_use", "id": "t1", "name": "Edit",
                 "input": {"file_path": "src/widget.rs", "old_string": "x", "new_string": "fn widget() {}"}}
            ]}
        }),
        serde_json::json!({
            "type": "user", "uuid": "u2", "sessionId": "blame-e2e",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false}
            ]}
        }),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&jsonl, body).unwrap();

    std::env::set_var("CLUSTERVISION_HOME", &home);
    let r = cv_core::ir::SessionRef {
        id: "blame-e2e".into(),
        harness: cv_core::ir::Harness::Claude,
        path: jsonl.clone(),
        cwd: Some("/historical/checkout".into()),
        title: Some("add the widget".into()),
        created_at: None,
        updated_at: None,
        message_count: 3,
    };
    cv_core::events::ingest_ref(&r).expect("ingest fixture session");
    cv_core::catalog::sync(std::slice::from_ref(&r));
    // The edit's msg_idx as the catalog recorded it — asserted against, never assumed.
    let edit_idx = cv_core::events::events_for("claude", "blame-e2e", Some("file_edit"))
        .first()
        .expect("edit event ingested")
        .msg_idx;
    std::env::remove_var("CLUSTERVISION_HOME");

    // --- whole-file blame: both commits newest-first, only the first one matched ---
    let (out, err) = cv(&home, &repo, &["blame", "src/widget.rs"]);
    assert!(out.contains("chore: tweak widget"), "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("feat: add widget"), "stdout:\n{out}");
    assert!(
        out.find("tweak widget").unwrap() < out.find("add widget").unwrap(),
        "newest commit must print first:\n{out}"
    );
    assert!(out.contains("(no agent session found)"), "{out}");
    assert!(out.contains("blame-e2"), "{out}");
    assert!(out.contains(&format!("edit at msg {edit_idx}")), "{out}");
    assert!(out.contains("8m before commit"), "{out}");
    assert!(
        out.contains("other checkout"),
        "suffix-only match should be labeled:\n{out}"
    );
    // The hint is copy-pasteable into the 0.11 window grammar: `--range A..B`, never `A-B`.
    let (lo, hi) = ((edit_idx - 3).max(0), edit_idx + 3);
    assert!(out.contains(&format!("cv show blame-e2 --range {lo}..{hi}")), "{out}");
    assert!(
        !out.contains(&format!("--range {lo}-{hi}")),
        "old `A-B` hint must be gone:\n{out}"
    );
    assert!(out.contains("1 of 2 commit(s) matched an agent session"), "{out}");

    // --- -L: line 2 was never touched by the tweak, so only the first commit owns it ---
    let (out, err) = cv(&home, &repo, &["blame", "src/widget.rs", "-L", "2"]);
    assert!(out.contains("feat: add widget"), "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("tweak widget"), "{out}");
    assert!(out.contains("blame-e2"), "{out}");
    assert!(out.contains("8m before commit"), "{out}");

    // --- --show: the range hint indexing must line up with `cv show --range` ---
    // The rendered window around msg `edit_idx` must contain the Edit tool_use itself.
    let (out, err) = cv(&home, &repo, &["blame", "src/widget.rs", "--show"]);
    assert!(
        out.contains("conversation around the edit"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        out.contains("[tool_use Edit t1]"),
        "event msg_idx is misaligned with show --range indexing:\n{out}"
    );
    assert!(out.contains("adding the widget now"), "{out}");

    // --- not a git repo: degrades to pure event-catalog mode with the same hints ---
    let (out, err) = cv(&home, &base, &["blame", "src/widget.rs"]);
    assert!(
        out.contains("not inside a git repository"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(out.contains("blame-e2"), "{out}");
    assert!(out.contains("last edit at msg"), "{out}");
    assert!(out.contains("cv show"), "{out}");

    // --- cold catalog: a clear "run cv index" hint, commits still listed ---
    let home2 = base.join("home2");
    fs::create_dir_all(&home2).unwrap();
    let (out, err) = cv(&home2, &repo, &["blame", "src/widget.rs"]);
    assert!(err.contains("run `cv index`"), "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("(no agent session found)"), "{out}");

    fs::remove_dir_all(&base).ok();
}
