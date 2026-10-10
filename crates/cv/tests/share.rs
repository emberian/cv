//! End-to-end tests for `cv share`: the self-contained HTML artifact, redaction default-on,
//! `--no-redact` opt-out + warning, default output path, and self-containment (no external
//! src/href). Hermetic — same temp-HOME pattern as `tests/cli.rs`, with its own helpers so the
//! shared file stays untouched.

use std::path::PathBuf;
use std::process::Command;
use std::{fs, str};

/// A planted secret that the default redaction pass must scrub.
const SECRET: &str = "sk-abcDEF1234567890ghijkl";
/// Planted conversation text that must survive into the artifact.
const PLANTED: &str = "the quokka census migration plan";

struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cv-share-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        fs::create_dir_all(home.join(".claude/projects/-work-proj")).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        World { base, home, cv_home }
    }

    fn cv(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cv"))
            .args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLUSTERVISION_HOME", &self.cv_home)
            // Hermetic: an agent seat's own Claude config dir must not add its sessions here.
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLUSTERVISION_CLAUDE_ROOTS")
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .output()
            .expect("cv should run");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn cv_ok(&self, args: &[&str]) -> (String, String) {
        let (ok, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} failed\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

/// A claude-format session with a title, thinking, a tool round-trip, and a planted secret.
fn share_corpus(w: &World) {
    let lines = [
        serde_json::json!({"type": "ai-title", "aiTitle": "quokka quest"}),
        serde_json::json!({
            "type": "user", "uuid": "u1", "sessionId": "s", "timestamp": "2026-02-01T10:00:00Z",
            "cwd": "/work/proj",
            "message": {"role": "user", "content": PLANTED}
        }),
        serde_json::json!({
            "type": "assistant", "uuid": "a1", "sessionId": "s", "timestamp": "2026-02-01T10:01:00Z",
            "cwd": "/work/proj",
            "message": {"role": "assistant", "model": "claude-test-1", "content": [
                {"type": "thinking", "thinking": "hmm, the census <tables> need care", "signature": "sig"},
                {"type": "text", "text": format!("found a leaked key {SECRET} in the env, scrubbing")},
                {"type": "tool_use", "id": "t1", "name": "Bash",
                 "input": {"command": format!("export API_KEY={SECRET} && ./migrate")}}
            ]}
        }),
        serde_json::json!({
            "type": "user", "uuid": "u2", "sessionId": "s", "timestamp": "2026-02-01T10:02:00Z",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": format!("migrated 7 quokkas; key was {SECRET}"), "is_error": false}
            ]}
        }),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(w.home.join(".claude/projects/-work-proj/sharesess.jsonl"), body).unwrap();
}

/// No `src=`/`href=` attribute may point at the network. (Fragment anchors and data: are fine.)
fn assert_no_external_urls(html: &str) {
    let lower = html.to_lowercase();
    for needle in [
        "src=\"http",
        "src='http",
        "href=\"http",
        "href='http",
        "src=\"//",
        "href=\"//",
        "@import",
        "url(http",
    ] {
        assert!(
            !lower.contains(needle),
            "external reference {needle:?} found in artifact"
        );
    }
}

/// Cheap structural well-formedness: doctype, closing html, balanced key containers.
fn assert_well_formed(html: &str) {
    assert!(html.starts_with("<!doctype html"), "missing doctype");
    assert!(html.trim_end().ends_with("</html>"), "unterminated document");
    for tag in ["section", "details", "header", "main", "footer", "pre", "summary"] {
        let open = html.matches(&format!("<{tag}")).count();
        let close = html.matches(&format!("</{tag}>")).count();
        assert_eq!(open, close, "unbalanced <{tag}>: {open} open vs {close} close");
    }
}

#[test]
fn share_redacts_by_default() {
    let w = World::new("redact");
    share_corpus(&w);

    let (_, err) = w.cv_ok(&["share", "sharesess", "--out", "shared.html"]);
    // no scare-warning on the safe default path, but a redaction summary
    assert!(!err.contains("NOT be scrubbed"), "stderr: {err}");
    assert!(err.contains("redacted"), "stderr: {err}");

    let html = fs::read_to_string(w.base.join("shared.html")).expect("artifact exists");
    assert_well_formed(&html);
    assert_no_external_urls(&html);

    // planted conversation text survives; planted secret does not (text, tool input, tool result)
    assert!(html.contains(PLANTED), "planted text missing");
    assert!(!html.contains(SECRET), "secret leaked into artifact");
    assert!(html.contains("[REDACTED:api_key]"), "no redaction placeholder");

    // badges: header shield + footer phrase
    assert!(html.contains("🛡 redacted"), "header badge missing");
    assert!(html.contains("this transcript was redacted"), "footer badge missing");
    assert!(!html.contains("unredacted"));

    // flagship structure: title, harness badge, collapsible thinking + tool call, inline assets
    assert!(html.contains("quokka quest"), "session title missing");
    assert!(html.contains(">claude</span>"), "harness badge missing");
    assert!(html.contains("fold think"), "thinking not collapsible");
    assert!(html.contains("fold tool tool-use"), "tool call not collapsible");
    assert!(html.contains("fold tool tool-res"), "tool result not collapsible");
    assert!(
        html.contains("<style>") && html.contains("<script>"),
        "inline assets missing"
    );
    // session content is escaped, never raw markup
    assert!(html.contains("&lt;tables&gt;"), "content not escaped");
    // footer credits the emitting version
    assert!(html.contains(&format!("clustervision</strong> v{}", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn share_no_redact_keeps_secret_and_warns() {
    let w = World::new("noredact");
    share_corpus(&w);

    let (_, err) = w.cv_ok(&["share", "sharesess", "--no-redact", "--out", "raw.html"]);
    assert!(err.contains("--no-redact"), "missing warning: {err}");
    assert!(err.contains("NOT be scrubbed"), "missing warning: {err}");

    let html = fs::read_to_string(w.base.join("raw.html")).unwrap();
    assert_well_formed(&html);
    assert_no_external_urls(&html);
    assert!(html.contains(SECRET), "--no-redact must keep the secret");
    assert!(!html.contains("[REDACTED:"));
    // honest badges
    assert!(html.contains("⚠ unredacted"), "header badge missing");
    assert!(html.contains("shared without redaction"), "footer badge missing");
    assert!(!html.contains("this transcript was redacted"));
}

#[test]
fn share_default_output_path_is_id_html() {
    let w = World::new("defaultout");
    share_corpus(&w);

    w.cv_ok(&["share", "sharesess"]);
    let path = w.base.join("sharesess.html");
    assert!(path.exists(), "default ./<id>.html not written");
    let html = fs::read_to_string(path).unwrap();
    assert_well_formed(&html);
    assert!(html.contains(PLANTED));
}

#[test]
fn share_unknown_session_fails_cleanly() {
    let w = World::new("missing");
    share_corpus(&w);
    let (ok, _, err) = w.cv(&["share", "nope-no-such-session"]);
    assert!(!ok, "should fail on unknown id");
    assert!(err.contains("no session matching"), "stderr: {err}");
    assert!(!err.contains("panicked"), "panic instead of error: {err}");
}
