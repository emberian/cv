//! The Claude Code adapter reads transcripts from more than one `projects/` directory: the default
//! `~/.claude/projects`, `$CLAUDE_CONFIG_DIR/projects`, each entry of `$CLUSTERVISION_CLAUDE_ROOTS`,
//! and each line of `$CLUSTERVISION_HOME/claude-roots` (an entry may hold `*` segments).
//!
//! These tests mutate process-global env (`HOME`, `CLUSTERVISION_HOME`, `CLAUDE_CONFIG_DIR`,
//! `CLUSTERVISION_CLAUDE_ROOTS`), so every test holds a static mutex for its whole body (the
//! `World` guard), the same way `freshness.rs` does.

use cv_core::harness::claude::Claude;
use cv_core::ir::{Harness, SessionRef};
use cv_core::Adapter;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

/// The catalog's recorded-watch fudge window (2s) plus margin; see `freshness.rs`.
const FUDGE: std::time::Duration = std::time::Duration::from_millis(2100);

struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl World {
    fn new(tag: &str) -> World {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!(
            "cv-roots-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("CLUSTERVISION_HOME", &cv_home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");
        std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
        std::env::remove_var("CURSOR_USER_DIR");
        World {
            base,
            home,
            cv_home,
            _guard: guard,
        }
    }

    fn default_projects(&self) -> PathBuf {
        self.home.join(".claude/projects")
    }
}

impl Drop for World {
    fn drop(&mut self) {
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLUSTERVISION_CLAUDE_ROOTS");
        fs::remove_dir_all(&self.base).ok();
    }
}

/// Write a one-message claude session `<projects>/<proj>/<sid>.jsonl`.
fn write_session(projects: &Path, proj: &str, sid: &str) -> PathBuf {
    write_session_at(projects, proj, sid, "2026-01-01T10:00:00Z")
}

/// [`write_session`] with its one message stamped `ts`.
fn write_session_at(projects: &Path, proj: &str, sid: &str, ts: &str) -> PathBuf {
    let dir = projects.join(proj);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    let line = serde_json::json!({
        "type": "user", "uuid": format!("{sid}-u0"), "sessionId": sid, "timestamp": ts,
        "cwd": "/work/proj",
        "message": {"role": "user", "content": format!("hello from {sid}")}
    });
    fs::write(&path, format!("{line}\n")).unwrap();
    path
}

fn claude_ids(refs: &[SessionRef]) -> Vec<String> {
    let mut v: Vec<String> = refs
        .iter()
        .filter(|r| r.harness == Harness::Claude)
        .map(|r| r.id.clone())
        .collect();
    v.sort();
    v
}

fn discovered() -> Vec<String> {
    claude_ids(&Claude::new().discover().unwrap())
}

fn join(paths: &[PathBuf]) -> std::ffi::OsString {
    std::env::join_paths(paths).unwrap()
}

/// With nothing configured, the adapter reads `~/.claude/projects` exactly as before, and that is
/// its storage root.
#[test]
fn default_root_still_works() {
    let w = World::new("default");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");

    assert_eq!(discovered(), vec!["defaultsess"]);
    assert_eq!(Claude::new().storage_root(), Some(w.default_projects()));
}

/// `CLUSTERVISION_CLAUDE_ROOTS` is a path list; an entry may be a projects dir itself or a Claude
/// config dir that holds `projects/`.
#[test]
fn env_roots_are_discovered() {
    let w = World::new("env");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");
    let projects_dir = w.base.join("elsewhere/projects");
    let config_dir = w.base.join("otherconfig");
    write_session(&projects_dir, "-work-proj", "projdirsess");
    write_session(&config_dir.join("projects"), "-work-proj", "configdirsess");
    std::env::set_var("CLUSTERVISION_CLAUDE_ROOTS", join(&[projects_dir, config_dir]));

    assert_eq!(discovered(), vec!["configdirsess", "defaultsess", "projdirsess"]);
    assert_eq!(
        Claude::new().storage_root(),
        Some(w.default_projects()),
        "the default root stays first"
    );
}

/// A `*` segment matches every entry of the directory before it, and is re-expanded on every
/// discover, so a seat created after the adapter was built is still found.
#[test]
fn wildcard_root_finds_every_seat_including_later_ones() {
    let w = World::new("wild");
    let seats = w.base.join("seats");
    write_session(&seats.join("one/claude/projects"), "-work-proj", "seatonesess");
    write_session(&seats.join("two/claude/projects"), "-work-proj", "seattwosess");
    fs::write(seats.join("ports.lock"), b"").unwrap(); // a plain file beside the seats is skipped
    fs::create_dir_all(seats.join("noclaude")).unwrap(); // a seat with no claude dir is skipped
    std::env::set_var("CLUSTERVISION_CLAUDE_ROOTS", seats.join("*").join("claude"));

    let adapter = Claude::new();
    assert_eq!(
        claude_ids(&adapter.discover().unwrap()),
        vec!["seatonesess", "seattwosess"]
    );

    write_session(&seats.join("three/claude/projects"), "-work-proj", "seatthreesess");
    assert_eq!(
        claude_ids(&adapter.discover().unwrap()),
        vec!["seatonesess", "seatthreesess", "seattwosess"],
        "a seat added later is found without rebuilding the adapter"
    );
}

/// Every `*` segment of an entry expands, so a layout that nests instances under each agent
/// (`seats/*/instances/*/claude`) is covered by one line.
#[test]
fn every_wildcard_segment_expands() {
    let w = World::new("wild2");
    let seats = w.base.join("seats");
    write_session(
        &seats.join("codex/instances/c1/claude/projects"),
        "-work-proj",
        "instonesess",
    );
    write_session(
        &seats.join("codex/instances/c2/claude/projects"),
        "-work-proj",
        "insttwosess",
    );
    write_session(
        &seats.join("kimi/instances/k1/claude/projects"),
        "-work-proj",
        "instkimisess",
    );
    write_session(&seats.join("codex/claude/projects"), "-work-proj", "flatsess"); // not this entry's layout
    std::env::set_var("CLUSTERVISION_CLAUDE_ROOTS", seats.join("*/instances/*/claude"));

    assert_eq!(discovered(), vec!["instkimisess", "instonesess", "insttwosess"]);
}

/// `$CLUSTERVISION_HOME/claude-roots` holds one entry per line; blank lines and `#` comments are
/// skipped, surrounding whitespace is trimmed, and a leading `~` is the home dir.
#[test]
fn roots_file_is_read() {
    let w = World::new("file");
    write_session(&w.home.join("seats/alpha/claude/projects"), "-work-proj", "filesess");
    write_session(&w.base.join("plain/projects"), "-work-proj", "plainsess");
    let roots = format!(
        "# helm seats\n\n  ~/seats/*/claude  \n{}\n# {}\n",
        w.base.join("plain").display(),
        w.base.join("commented-out").display()
    );
    fs::write(w.cv_home.join("claude-roots"), roots).unwrap();
    write_session(&w.base.join("commented-out/projects"), "-work-proj", "hiddensess");

    assert_eq!(discovered(), vec!["filesess", "plainsess"]);
}

/// Claude Code's own `CLAUDE_CONFIG_DIR` is read, and — being where the current Claude Code
/// writes — it is the storage root (the default conversion target) when set.
#[test]
fn claude_config_dir_is_read() {
    let w = World::new("ccd");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");
    let cfg = w.base.join("seatconfig");
    write_session(&cfg.join("projects"), "-work-proj", "configsess");
    std::env::set_var("CLAUDE_CONFIG_DIR", &cfg);

    assert_eq!(discovered(), vec!["configsess", "defaultsess"]);
    assert_eq!(Claude::new().storage_root(), Some(cfg.join("projects")));
}

/// Roots that do not exist (or are not directories) are ignored — never an error, and they do
/// not displace the default storage root.
#[test]
fn missing_roots_are_ignored() {
    let w = World::new("missing");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");
    let a_file = w.base.join("not-a-dir");
    fs::write(&a_file, b"x").unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", w.base.join("no-such-config"));
    std::env::set_var(
        "CLUSTERVISION_CLAUDE_ROOTS",
        join(&[
            w.base.join("no-such-root"),
            a_file,
            w.base.join("no-such-seats/*/claude"),
        ]),
    );
    fs::write(w.cv_home.join("claude-roots"), "/definitely/not/here\n").unwrap();

    assert_eq!(discovered(), vec!["defaultsess"]);
    assert_eq!(Claude::new().storage_root(), Some(w.default_projects()));
}

/// The same directory named several ways (config dir vs projects dir, a symlink) is read once.
#[test]
fn a_root_named_twice_is_read_once() {
    let w = World::new("dedupe");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");
    std::env::set_var("CLAUDE_CONFIG_DIR", w.home.join(".claude"));
    let mut roots = vec![w.default_projects(), w.home.join(".claude")];
    #[cfg(unix)]
    {
        let link = w.base.join("link-to-claude");
        std::os::unix::fs::symlink(w.home.join(".claude"), &link).unwrap();
        roots.push(link);
    }
    std::env::set_var("CLUSTERVISION_CLAUDE_ROOTS", join(&roots));

    assert_eq!(discovered(), vec!["defaultsess"], "one directory, one listing");
    assert_eq!(Claude::new().storage_root(), Some(w.default_projects()));
}

/// Two roots can hold the same session id — e.g. a transcript copied into a second config dir to
/// resume it there, then continued. Both copies are listed; resolving the id (exact or by prefix)
/// picks the most recently updated copy instead of an arbitrary one or an "ambiguous" error.
#[test]
fn same_id_in_two_roots_resolves_to_the_newest_copy() {
    let w = World::new("dupid");
    write_session_at(&w.default_projects(), "-work-proj", "dupsess", "2026-01-01T10:00:00Z");
    let seat = w.base.join("seat/projects");
    let newer = write_session_at(&seat, "-work-proj", "dupsess", "2026-02-01T10:00:00Z");
    std::env::set_var("CLUSTERVISION_CLAUDE_ROOTS", &seat);

    assert_eq!(discovered(), vec!["dupsess", "dupsess"], "both copies are listed");
    // First call: cold catalog → full scan path. Second: the warm catalog path.
    for pass in ["cold", "warm"] {
        let (r, _) = cv_core::find("dupsess", None).unwrap().expect("found");
        assert_eq!(r.path, newer, "{pass}: exact id resolves to the newest copy");
        let (r, _) = cv_core::find("dupse", None).unwrap().expect("found by prefix");
        assert_eq!(r.path, newer, "{pass}: a prefix of one duplicated id is not ambiguous");
    }
}

/// The catalog's freshness probe watches every root (a new project under an extra root shows up
/// on the next fast read) and re-checks the root list itself: a root that appeared since the last
/// sync — a new seat under a `*`, a line added to the roots file — also triggers re-discovery.
/// Each change is made after the watches settle, so no fudge-stamped mtime can be what finds it.
#[test]
fn catalog_sees_new_sessions_under_extra_roots() {
    let w = World::new("fresh");
    write_session(&w.default_projects(), "-work-proj", "defaultsess");
    let extra = w.base.join("extra/projects");
    write_session(&extra, "-work-proj", "extrasess");
    let seats = w.base.join("seats");
    write_session(&seats.join("one/claude/projects"), "-work-proj", "seatonesess");
    std::env::set_var(
        "CLUSTERVISION_CLAUDE_ROOTS",
        join(&[extra.clone(), seats.join("*/claude")]),
    );

    cv_core::discover_all();
    std::thread::sleep(FUDGE); // age past the watch fudge…
    cv_core::sessions(); // …and re-record real (non-zero) watch mtimes
    assert_eq!(
        claude_ids(&cv_core::sessions()),
        vec!["defaultsess", "extrasess", "seatonesess"]
    );

    write_session(&extra, "-work-new", "newprojsess"); // new project dir
    assert_eq!(
        claude_ids(&cv_core::sessions()),
        vec!["defaultsess", "extrasess", "newprojsess", "seatonesess"],
        "a new project directory under an extra root is seen"
    );

    std::thread::sleep(FUDGE);
    cv_core::sessions();
    write_session(&seats.join("two/claude/projects"), "-work-proj", "seattwosess");
    assert_eq!(
        claude_ids(&cv_core::sessions()),
        vec!["defaultsess", "extrasess", "newprojsess", "seatonesess", "seattwosess"],
        "a new seat under a `*` root is seen"
    );

    std::thread::sleep(FUDGE);
    cv_core::sessions();
    let later = w.base.join("later");
    write_session(&later.join("projects"), "-work-proj", "latersess");
    fs::write(w.cv_home.join("claude-roots"), format!("{}\n", later.display())).unwrap();
    assert_eq!(
        claude_ids(&cv_core::sessions()),
        vec![
            "defaultsess",
            "extrasess",
            "latersess",
            "newprojsess",
            "seatonesess",
            "seattwosess"
        ],
        "a root added to the roots file is seen"
    );
}
