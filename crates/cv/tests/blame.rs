//! End-to-end `cv blame`: a real (temp) git repo with controlled commit times + rebase/amend
//! history + `-L` line-scoped content-hunk attribution + `--json` + `/api/blame` HTTP route.
//! Everything lives in ONE test fn because `CLUSTERVISION_HOME` is process-global state for
//! in-process `cv_core` calls.

use cv_testkit::World;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::{fs, str};

/// 2026-06-01T12:00:00Z — the timestamp on the session's Edit message.
const EDIT_TS: i64 = 1_780_315_200;
/// The agent edits, the human commits 8 minutes later.
const COMMIT1_TS: i64 = EDIT_TS + 8 * 60;
/// A second commit two days later that no session can claim.
const COMMIT2_TS: i64 = EDIT_TS + 2 * 86_400;
/// A third squashed/rebased commit 5 days later (far outside `[commit - 6h, commit + 15m]`)
/// combining two distinct functions authored by two different sessions days earlier.
const COMMIT3_REBASED_TS: i64 = EDIT_TS + 5 * 86_400;

fn git(repo: &Path, args: &[&str], date: Option<i64>) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Index of `needle`'s first occurrence, with a labeled panic when missing.
fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in:\n{hay}"))
}

/// Run the built `cv` binary in `w` from `dir`; returns (stdout, stderr).
fn cv(w: &World, dir: &Path, args: &[&str]) -> (String, String) {
    let out = w
        .cv_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("cv should run");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn blame_end_to_end() {
    // In-process: the catalog ingest below reads `CLUSTERVISION_HOME` from the process env.
    let w = World::in_process("blame-e2e", cv_core::env::Env::enter_isolated);
    let base = w.base.clone();
    let repo_raw = base.join("repo");
    let fixtures = base.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(repo_raw.join("src")).unwrap();
    // Canonicalize so /var vs /private/var (macOS temp symlink) agrees with git's toplevel.
    let repo = repo_raw.canonicalize().unwrap();

    // --- a real repo with two commits on widget.rs at controlled times ---
    git(&repo, &["init", "-q"], None);
    git(&repo, &["config", "user.email", "t@example.com"], None);
    git(&repo, &["config", "user.name", "t"], None);
    git(&repo, &["config", "commit.gpgsign", "false"], None);
    fs::write(repo.join("src/widget.rs"), "fn widget() {}\nfn extra() {}\n").unwrap();
    git(&repo, &["add", "."], None);
    git(&repo, &["commit", "-qm", "feat: add widget"], Some(COMMIT1_TS));
    fs::write(repo.join("src/widget.rs"), "fn widget() { 2 }\nfn extra() {}\n").unwrap();
    git(&repo, &["commit", "-aqm", "chore: tweak widget"], Some(COMMIT2_TS));

    // --- a synthetic claude session that edited widget.rs in a DIFFERENT checkout ---
    let jsonl = fixtures.join("blame-e2e.jsonl");
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

    // --- a second session that edited the file hours earlier and then RAN the first commit ---
    let sha1 = git(&repo, &["rev-parse", "HEAD~1"], None);
    let short1 = &sha1[..7];
    let exact_jsonl = fixtures.join("exact-e2e.jsonl");
    let lines = [
        serde_json::json!({
            "type": "assistant", "uuid": "x1", "sessionId": "exact-e2e",
            "timestamp": "2026-06-01T07:00:00Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "e1", "name": "Edit",
                 "input": {"file_path": "/elsewhere/src/widget.rs", "old_string": "x", "new_string": "y"}}
            ]}
        }),
        serde_json::json!({
            "type": "user", "uuid": "x2", "sessionId": "exact-e2e", "timestamp": "2026-06-01T07:00:01Z",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "e1", "content": "ok", "is_error": false}
            ]}
        }),
        serde_json::json!({
            "type": "assistant", "uuid": "x3", "sessionId": "exact-e2e", "timestamp": "2026-06-01T12:08:00Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "c1", "name": "Bash",
                 "input": {"command": "git commit -qm 'feat: add widget' && git log -1 --oneline"}}
            ]}
        }),
        serde_json::json!({
            "type": "user", "uuid": "x4", "sessionId": "exact-e2e", "timestamp": "2026-06-01T12:08:01Z",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": format!("{short1} feat: add widget"), "is_error": false}
            ]}
        }),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&exact_jsonl, body).unwrap();

    let rx = cv_core::ir::SessionRef {
        id: "exact-e2e".into(),
        harness: cv_core::ir::Harness::Claude,
        path: exact_jsonl.clone(),
        cwd: Some("/elsewhere".into()),
        title: None,
        created_at: None,
        updated_at: None,
        message_count: Some(4),
    };
    cv_core::events::ingest_ref(&rx).expect("ingest exact fixture session");
    let r = cv_core::ir::SessionRef {
        id: "blame-e2e".into(),
        harness: cv_core::ir::Harness::Claude,
        path: jsonl.clone(),
        cwd: Some("/historical/checkout".into()),
        title: Some("add the widget".into()),
        created_at: None,
        updated_at: None,
        message_count: Some(3),
    };
    cv_core::events::ingest_ref(&r).expect("ingest fixture session");
    cv_core::catalog::sync(&[r.clone(), rx.clone()]);
    let edit_idx = cv_core::events::events_for("claude", "blame-e2e", Some("file_edit"))
        .unwrap()
        .first()
        .expect("edit event ingested")
        .msg_idx;

    // --- whole-file blame: both commits newest-first, only the first one matched ---
    let (out, err) = cv(&w, &repo, &["blame", "src/widget.rs"]);
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
    let (lo, hi) = ((edit_idx - 3).max(0), edit_idx + 3);
    assert!(out.contains(&format!("cv show blame-e2 --range {lo}..{hi}")), "{out}");
    assert!(
        !out.contains(&format!("--range {lo}-{hi}")),
        "old `A-B` hint must be gone:\n{out}"
    );
    assert!(out.contains("1 of 2 commit(s) matched an agent session"), "{out}");
    // CommitSha evidence outranks ContentHunk / Timed, and spells its strength explicitly.
    let exact_at = pos(&out, "commit created here");
    assert!(out.contains("exact: commit sha"), "{out}");
    assert!(out[..exact_at].contains("exact-e2"), "{out}");
    assert!(
        exact_at < pos(&out, "8m before commit"),
        "exact commit sha must rank first:\n{out}"
    );
    assert!(out.contains(&format!("cv rewind exact-e2 --at {short1}")), "{out}");
    assert_eq!(out.matches("exact-e2").count(), 3, "one match line + two hints:\n{out}");

    // --- -L: line 2 was never touched by the tweak, so only the first commit owns it ---
    let (out, err) = cv(&w, &repo, &["blame", "src/widget.rs", "-L", "2"]);
    assert!(out.contains("feat: add widget"), "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("tweak widget"), "{out}");
    assert!(out.contains("blame-e2"), "{out}");
    assert!(out.contains("8m before commit"), "{out}");

    // --- --show: the range hint indexing must line up with `cv show --range` ---
    let (out, err) = cv(&w, &repo, &["blame", "src/widget.rs", "--show"]);
    assert!(
        out.contains("conversation around the edit"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(
        out.contains("[tool_use Bash c1]"),
        "evidence msg_idx is misaligned with show --range indexing:\n{out}"
    );
    assert!(out.contains(&format!("{short1} feat: add widget")), "{out}");

    // --- Rebase / cherry-pick / squash survival + `-L a,b` line-scoped ContentHunk + `--json` + `/api/blame` ---
    // Two sessions edit `src/rebased.rs` on 2026-06-01 (5 DAYS before `COMMIT3_REBASED_TS`!),
    // plus a distractor session that touches `src/rebased.rs` 5 minutes before `COMMIT3_REBASED_TS`
    // with unrelated content (heuristic `Timed` only).
    // - `sess-alpha-hunk` writes lines 1..=2 (`fn alpha_calc() -> i32 { 41 + 1 }`)
    // - `sess-beta-hunk` writes lines 3..=4 (`fn beta_calc() -> i32 { 99 * 2 }`)
    // Then a squashed/rebased commit lands 5 days later (`COMMIT3_REBASED_TS`) with a completely new SHA.
    let alpha_jsonl = fixtures.join("sess-alpha-hunk.jsonl");
    fs::write(
        &alpha_jsonl,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant", "uuid": "ah1", "sessionId": "sess-alpha-hunk",
                "timestamp": "2026-06-01T12:00:00Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "ta1", "name": "Write",
                     "input": {"file_path": repo.join("src/rebased.rs"),
                               "content": "pub fn alpha_calc() -> i32 {\n    41 + 1\n}\n"}}
                ]}
            })
        ),
    )
    .unwrap();

    let beta_jsonl = fixtures.join("sess-beta-hunk.jsonl");
    fs::write(
        &beta_jsonl,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant", "uuid": "bh1", "sessionId": "sess-beta-hunk",
                "timestamp": "2026-06-01T13:00:00Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tb1", "name": "Edit",
                     "input": {"file_path": repo.join("src/rebased.rs"),
                               "old_string": "// placeholder",
                               "new_string": "pub fn beta_calc() -> i32 {\n    99 * 2\n}"}}
                ]}
            })
        ),
    )
    .unwrap();

    // Distractor session 5 minutes before COMMIT3_REBASED_TS (`2026-06-06T11:55:00Z`): inside `[commit - 6h, commit + 15m]`,
    // but wrote unrelated comment text that was discarded before commit.
    let timed_jsonl = fixtures.join("sess-timed-only.jsonl");
    let timed_iso = chrono::DateTime::from_timestamp(COMMIT3_REBASED_TS - 300, 0)
        .unwrap()
        .to_rfc3339();
    fs::write(
        &timed_jsonl,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant", "uuid": "tm1", "sessionId": "sess-timed-only",
                "timestamp": timed_iso,
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tt1", "name": "Edit",
                     "input": {"file_path": repo.join("src/rebased.rs"),
                               "old_string": "old",
                               "new_string": "// discarded scratch note"}}
                ]}
            })
        ),
    )
    .unwrap();

    fs::write(
        repo.join("src/rebased.rs"),
        "pub fn alpha_calc() -> i32 {\n    41 + 1\n}\npub fn beta_calc() -> i32 {\n    99 * 2\n}\n",
    )
    .unwrap();
    git(&repo, &["add", "src/rebased.rs"], None);
    git(
        &repo,
        &["commit", "-qm", "feat: squash rebased alpha and beta"],
        Some(COMMIT3_REBASED_TS),
    );
    // Amend the commit with a new timestamp (+1h) to prove rebase/amend changes commit SHA & time
    // while `ContentHunk` provenance survives intact!
    git(
        &repo,
        &["commit", "--amend", "-qm", "feat: rebased & amended alpha+beta"],
        Some(COMMIT3_REBASED_TS + 3600),
    );

    let r_alpha = cv_core::ir::SessionRef {
        id: "sess-alpha-hunk".into(),
        harness: cv_core::ir::Harness::Claude,
        path: alpha_jsonl,
        cwd: Some(repo.clone()),
        title: Some("author alpha_calc".into()),
        created_at: None,
        updated_at: None,
        message_count: Some(1),
    };
    let r_beta = cv_core::ir::SessionRef {
        id: "sess-beta-hunk".into(),
        harness: cv_core::ir::Harness::Claude,
        path: beta_jsonl,
        cwd: Some(repo.clone()),
        title: Some("author beta_calc".into()),
        created_at: None,
        updated_at: None,
        message_count: Some(1),
    };
    let r_timed = cv_core::ir::SessionRef {
        id: "sess-timed-only".into(),
        harness: cv_core::ir::Harness::Claude,
        path: timed_jsonl,
        cwd: Some(repo.clone()),
        title: Some("scratch edit".into()),
        created_at: None,
        updated_at: None,
        message_count: Some(1),
    };
    cv_core::events::ingest_ref(&r_alpha).unwrap();
    cv_core::events::ingest_ref(&r_beta).unwrap();
    cv_core::events::ingest_ref(&r_timed).unwrap();
    cv_core::catalog::sync(&[r, rx, r_alpha, r_beta, r_timed]);

    // 1) `cv blame src/rebased.rs -L 1,2 --json`: ONLY `sess-alpha-hunk` matches as `content_hunk`,
    //    outranking `sess-timed-only` (`timed`), while `sess-beta-hunk` (5 days old, wrote lines 4..=5)
    //    is excluded by `-L 1,2` line scoping!
    let (json_l12, err) = cv(&w, &repo, &["blame", "src/rebased.rs", "-L", "1,2", "--json"]);
    let rep_l12: Value =
        serde_json::from_str(&json_l12).unwrap_or_else(|e| panic!("bad JSON ({e}):\n{json_l12}\nstderr:\n{err}"));
    assert_eq!(rep_l12["in_git_repo"], true);
    assert_eq!(rep_l12["lines"], serde_json::json!([1, 2]));
    let matches_l12 = rep_l12["commits"][0]["matches"].as_array().unwrap();
    assert_eq!(matches_l12[0]["session_id"], "sess-alpha-hunk", "{rep_l12}");
    assert_eq!(matches_l12[0]["strength"], "content_hunk", "{rep_l12}");
    assert_eq!(matches_l12[0]["label"], "exact: content hunk", "{rep_l12}");
    assert!(matches_l12[0]["matched_hunks"].as_u64().unwrap() >= 1, "{rep_l12}");
    assert!(
        !matches_l12.iter().any(|m| m["session_id"] == "sess-beta-hunk"),
        "line-scoped -L 1,2 must NOT match sess-beta-hunk which only wrote lines 4..=5:\n{rep_l12}"
    );
    assert!(
        !matches_l12.iter().any(|m| m["session_id"] == "sess-timed-only"),
        "line-scoped -L 1,2 suppresses unrelated timed edits once exact content_hunk matches:\n{rep_l12}"
    );

    // Whole-file `cv blame src/rebased.rs --json` (no `-L`): `content_hunk` authors (`sess-alpha-hunk`,
    // `sess-beta-hunk`) rank strictly ahead of heuristic `timed` (`sess-timed-only`).
    let (json_whole, _) = cv(&w, &repo, &["blame", "src/rebased.rs", "--json"]);
    let rep_whole: Value = serde_json::from_str(&json_whole).unwrap();
    let matches_whole = rep_whole["commits"][0]["matches"].as_array().unwrap();
    assert_eq!(matches_whole[0]["strength"], "content_hunk", "{rep_whole}");
    assert_eq!(matches_whole[1]["strength"], "content_hunk", "{rep_whole}");
    let timed_match = matches_whole
        .iter()
        .find(|m| m["session_id"] == "sess-timed-only")
        .expect("whole-file blame includes timed fallback below content_hunk");
    assert_eq!(timed_match["strength"], "timed");
    assert_eq!(timed_match["label"], "heuristic: time window");

    // 2) `cv blame src/rebased.rs -L 4,5 --json`: flips to `sess-beta-hunk` first (`content_hunk`),
    //    and excludes `sess-alpha-hunk`!
    let (json_l45, _) = cv(&w, &repo, &["blame", "src/rebased.rs", "-L", "4,5", "--json"]);
    let rep_l45: Value = serde_json::from_str(&json_l45).unwrap();
    let matches_l45 = rep_l45["commits"][0]["matches"].as_array().unwrap();
    assert_eq!(matches_l45[0]["session_id"], "sess-beta-hunk", "{rep_l45}");
    assert_eq!(matches_l45[0]["strength"], "content_hunk");
    assert!(
        !matches_l45.iter().any(|m| m["session_id"] == "sess-alpha-hunk"),
        "line-scoped -L 4,5 must NOT match sess-alpha-hunk:\n{rep_l45}"
    );

    // 3) HTTP API `GET /api/blame?file=<abs>&lines=4,5`: serves the exact same structured provenance!
    let abs_rebased = repo.join("src/rebased.rs");
    let encoded_file = urlencoding::encode(abs_rebased.to_str().unwrap());
    let query = format!("file={encoded_file}&lines=4,5");
    let (status, api_val) = cv_core::api::route_json(&["api", "blame"], &query);
    assert_eq!(status, 200, "body: {api_val}");
    assert_eq!(
        api_val["commits"][0]["matches"][0]["session_id"], "sess-beta-hunk",
        "{api_val}"
    );
    assert_eq!(api_val["commits"][0]["matches"][0]["strength"], "content_hunk");

    // --- not a git repo: degrades to pure event-catalog mode with the same hints ---
    let (out, err) = cv(&w, &base, &["blame", "src/widget.rs"]);
    assert!(
        out.contains("not inside a git repository"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(out.contains("blame-e2"), "{out}");
    assert!(out.contains("last edit at msg"), "{out}");
    assert!(out.contains("cv show"), "{out}");

    // --- cold catalog: a clear "run cv index" hint, commits still listed ---
    let cold = World::new("blame-cold");
    let (out, err) = cv(&cold, &repo, &["blame", "src/widget.rs"]);
    assert!(err.contains("run `cv index`"), "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("(no agent session found)"), "{out}");
}
