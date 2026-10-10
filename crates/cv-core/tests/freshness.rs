//! Integration tests for the catalog read path ([`cv_core::sessions`]) and its freshness probe:
//! cold-catalog transparency, new-file / appended-file / vanished-dir detection, and the
//! staleness-backstop escape hatch.
//!
//! These tests mutate process-global env (`HOME`, `CLUSTERVISION_HOME`), so every test holds a
//! static mutex for its whole body (the `World` guard). They also `sleep` past the probe's 2s
//! watch-fudge window where a test needs recorded (non-zero) watch mtimes to be the thing that
//! detects a change.

use cv_core::ir::{Harness, SessionRef};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

/// How long the recorded-watch fudge window is (catalog's WATCH_FUDGE = 2s) plus margin.
const FUDGE: std::time::Duration = std::time::Duration::from_millis(2100);

struct World {
    base: PathBuf,
    home: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl World {
    fn new(tag: &str) -> World {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!(
            "cv-fresh-{tag}-{}-{}",
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
        std::env::set_var("HOME", &home);
        std::env::set_var("CLUSTERVISION_HOME", &cv_home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
        std::env::remove_var("CURSOR_USER_DIR");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");
        World {
            base,
            home,
            _guard: guard,
        }
    }

    fn proj_dir(&self, proj: &str) -> PathBuf {
        self.home.join(".claude/projects").join(proj)
    }

    /// Write a claude-format session of `n` user messages under project dir `proj`.
    fn write_session(&self, proj: &str, name: &str, n: usize) -> PathBuf {
        let dir = self.proj_dir(proj);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        let body: String = (0..n).map(|i| user_line(name, i)).collect();
        fs::write(&path, body).unwrap();
        path
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn user_line(sid: &str, i: usize) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user", "uuid": format!("{sid}-u{i}"), "sessionId": sid,
            "timestamp": format!("2026-01-0{}T10:0{}:00Z", (i % 9) + 1, i % 10),
            "cwd": "/work/proj",
            "message": {"role": "user", "content": format!("message {i} of {sid}")}
        })
    )
}

/// The comparable identity of a ref (timestamps excluded: the catalog stores second precision).
fn key(r: &SessionRef) -> (String, String, PathBuf, usize) {
    (r.harness.as_str().into(), r.id.clone(), r.path.clone(), r.message_count)
}

fn keys(refs: &[SessionRef]) -> Vec<(String, String, PathBuf, usize)> {
    let mut v: Vec<_> = refs.iter().map(key).collect();
    v.sort();
    v
}

fn ids(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().map(|r| r.id.clone()).collect();
    v.sort();
    v
}

/// A cold catalog must transparently full-discover (same answer as `discover_all`), and an
/// immediately-following warm read must agree exactly.
#[test]
fn cold_catalog_matches_discover_all() {
    let w = World::new("cold");
    w.write_session("-work-proj", "alphasess", 2);
    w.write_session("-work-proj", "betasess", 3);

    let cold = cv_core::sessions(); // cold: no last_full_sync stamp yet
    assert_eq!(ids(&cold), vec!["alphasess", "betasess"], "cold read = full discovery");

    let direct = cv_core::discover_all();
    let warm = cv_core::sessions();
    assert_eq!(keys(&warm), keys(&direct), "warm catalog read == discover_all");
    assert_eq!(warm.iter().map(key).count(), 2);
    let alpha = warm.iter().find(|r| r.id == "alphasess").unwrap();
    assert_eq!(alpha.harness, Harness::Claude);
    assert_eq!(alpha.message_count, 2);
    assert_eq!(alpha.cwd.as_deref(), Some(std::path::Path::new("/work/proj")));
}

/// A new session file in an already-watched directory changes that dir's mtime → probe detects it.
/// A new *project directory* changes the root's mtime → also detected.
#[test]
fn new_session_files_detected() {
    let w = World::new("new");
    w.write_session("-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE); // age past the watch fudge…
    cv_core::sessions(); // …and re-record real (non-zero) watch mtimes

    w.write_session("-work-proj", "betasess", 1); // new file in a watched dir
    w.write_session("-work-other", "gammasess", 1); // new dir under the watched root
    let refs = cv_core::sessions();
    assert_eq!(ids(&refs), vec!["alphasess", "betasess", "gammasess"]);
}

/// An in-place append doesn't change any directory mtime; the top-K recently-updated-file check
/// (against the scan cache's recorded `(mtime, size)`) must catch it.
#[test]
fn append_in_top_k_detected() {
    let w = World::new("append");
    let path = w.write_session("-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions(); // stabilize watches

    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(user_line("alphasess", 2).as_bytes()).unwrap();
    drop(f);

    let refs = cv_core::sessions();
    let alpha = refs.iter().find(|r| r.id == "alphasess").expect("alphasess present");
    assert_eq!(alpha.message_count, 3, "append must be re-scanned via the top-K probe");
}

/// Deleting a session file (dir mtime changes) or a whole project dir (watched path vanishes)
/// must drop those sessions from the read path.
#[test]
fn vanished_sessions_dropped() {
    let w = World::new("vanish");
    let alpha_path = w.write_session("-work-proj", "alphasess", 2);
    w.write_session("-work-other", "gammasess", 1);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();

    fs::remove_file(&alpha_path).unwrap(); // unlink bumps the parent dir's mtime
    let refs = cv_core::sessions();
    assert_eq!(ids(&refs), vec!["gammasess"], "deleted session must not appear");

    std::thread::sleep(FUDGE);
    cv_core::sessions();
    fs::remove_dir_all(w.proj_dir("-work-other")).unwrap(); // watched dir itself vanishes
    let refs = cv_core::sessions();
    assert_eq!(
        ids(&refs),
        Vec::<String>::new(),
        "vanished dir's sessions must not appear"
    );
}

/// The warm path really reads the catalog (a row only the catalog knows shows up), and forcing a
/// full discovery — what `cv ls --fresh` and `CLUSTERVISION_MAX_STALE_SECS=0` do — heals it.
#[test]
fn warm_path_reads_catalog_and_full_discovery_heals() {
    let w = World::new("warm");
    w.write_session("-work-proj", "alphasess", 2);
    cv_core::discover_all();
    std::thread::sleep(FUDGE);
    cv_core::sessions();

    // Plant a phantom row via the additive sync. Nothing on disk changes, so the probe stays
    // clean — only an actual catalog read can return it.
    let phantom = SessionRef {
        id: "phantom-row".into(),
        harness: Harness::Claude,
        path: w.base.join("nowhere.jsonl"),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 7,
    };
    cv_core::catalog::sync(std::slice::from_ref(&phantom));
    let refs = cv_core::sessions();
    assert!(
        refs.iter().any(|r| r.id == "phantom-row"),
        "warm sessions() must read the catalog, not re-scan: {:?}",
        ids(&refs)
    );

    // Staleness backstop at 0 = every read is a full discovery; the replace-sync drops the phantom.
    std::env::set_var("CLUSTERVISION_MAX_STALE_SECS", "0");
    let refs = cv_core::sessions();
    std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
    assert_eq!(ids(&refs), vec!["alphasess"], "full discovery heals phantom rows");

    // And discover_all() itself (the `--fresh` path) returns only what's on disk.
    cv_core::catalog::sync(std::slice::from_ref(&phantom));
    let refs = cv_core::discover_all();
    assert_eq!(ids(&refs), vec!["alphasess"]);
}
