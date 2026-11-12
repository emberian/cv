//! `cv archive sync|ls|path` and `cv watch` — the durable, harness-agnostic store (what `cvd` was).
//!
//! The archive is a per-harness directory of serialized IR sessions plus an idempotent JSONL
//! catalog, so a user can centralize agent logs from many machines/harnesses for search and
//! safekeeping. `sync` snapshots once and exits; `watch` follows live activity and re-archives
//! each session as it changes, built on cv-core's live-watch engine.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::{home_rel, truncate};
use cv_core::watch::{EventKind, Filter, Watcher};
use cv_core::{Harness, Session, SessionRef};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util::{parse_harness, short_id};

mod reuse;

/// One row of the catalog (`catalog.jsonl`): the archived copy as a session row, whose `path` is
/// the serialized session JSON (relative to the archive home) and whose `message_count` is its
/// turns, plus how many messages the archive wrote and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "Line")]
pub(crate) struct CatalogEntry {
    #[serde(flatten)]
    pub session: SessionRef,
    pub messages_written: usize,
    pub archived_at: DateTime<Utc>,
}

/// A catalog line as stored. A line 0.17 wrote has no `messages_written`: its `message_count`
/// was that raw count, so it reads as written with its turns not counted.
#[derive(Deserialize)]
struct Line {
    #[serde(flatten)]
    session: SessionRef,
    messages_written: Option<usize>,
    archived_at: DateTime<Utc>,
}

impl From<Line> for CatalogEntry {
    fn from(mut line: Line) -> Self {
        let messages_written = match line.messages_written {
            Some(n) => n,
            None => line.session.message_count.take().unwrap_or(0),
        };
        CatalogEntry {
            session: line.session,
            messages_written,
            archived_at: line.archived_at,
        }
    }
}

impl CatalogEntry {
    fn key(&self) -> (String, String) {
        (self.session.harness.as_str().into(), self.session.id.clone())
    }
}

/// Whether a `store` call actually wrote (the session was new/changed) or was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreOutcome {
    Archived,
    Skipped,
}

/// Handle to the archive root (`$CLUSTERVISION_HOME` or `~/.clustervision`, overridable by `--home`).
pub(crate) struct Archive {
    home: PathBuf,
    /// Only schedules cleanup; never cached catalog contents. Another writer's compaction (a
    /// size decrease) resets the schedule, and every pass reads the actual file under the lock.
    compaction_check: Cell<CompactionCheck>,
    #[cfg(test)]
    catalog_bytes_scanned: Cell<u64>,
}

#[derive(Clone, Copy, Default)]
struct CompactionCheck {
    last_size: u64,
    next_size: u64,
}

impl Archive {
    /// Resolve the archive home: explicit `--home`, else `$CLUSTERVISION_HOME`, else `~/.clustervision`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(p) = explicit {
            p
        } else if let Some(env) = cv_core::env::var_os("CLUSTERVISION_HOME") {
            PathBuf::from(env)
        } else {
            let base = crate::util::dirs_home().context("could not determine home directory")?;
            base.join(".clustervision")
        };
        Ok(Archive {
            home,
            compaction_check: Cell::new(CompactionCheck::default()),
            #[cfg(test)]
            catalog_bytes_scanned: Cell::new(0),
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn catalog_path(&self) -> PathBuf {
        self.home.join("catalog.jsonl")
    }

    /// Serialize catalog writers across processes (parallel `sync` / `watch` on one archive): an
    /// exclusive advisory flock, released when the returned handle drops. It lives on a dedicated
    /// lockfile rather than the catalog itself, because compaction rename-replaces the catalog and
    /// a lock on the replaced inode would guard nothing.
    fn lock_catalog(&self) -> Result<fs::File> {
        let path = self.home.join("catalog.jsonl.lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false) // the lockfile is a pure rendezvous point; contents are irrelevant
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.lock().with_context(|| format!("locking {}", path.display()))?;
        Ok(f)
    }

    /// Relative (to home) path where a session's JSON lives.
    fn rel_session_path(harness: Harness, id: &str) -> PathBuf {
        PathBuf::from("archive")
            .join(harness.as_str())
            .join(format!("{}.json", archive_stem(id)))
    }

    fn session_path(&self, harness: Harness, id: &str) -> PathBuf {
        self.home.join(Self::rel_session_path(harness, id))
    }

    /// Read the catalog, deduped by (harness, id) with the newest `archived_at` winning.
    pub fn load_catalog(&self) -> Result<Vec<CatalogEntry>> {
        let path = self.catalog_path();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self::catalog_entries(&raw))
    }

    fn catalog_entries(raw: &str) -> Vec<CatalogEntry> {
        let mut map: BTreeMap<(String, String), CatalogEntry> = BTreeMap::new();
        for (index, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<CatalogEntry>(line) {
                Ok(entry) => {
                    let key = entry.key();
                    match map.get(&key) {
                        Some(prev) if prev.archived_at >= entry.archived_at => {}
                        _ => {
                            map.insert(key, entry);
                        }
                    }
                }
                Err(e) => eprintln!("cv archive: skipping corrupt catalog line {}: {e}", index + 1),
            }
        }
        map.into_values().collect()
    }

    /// Record an entry by **appending** one line — O(1) per archive instead of rewriting the whole
    /// catalog every time (which made N archives O(N²)). `load_catalog` already dedups by key with
    /// the newest `archived_at` winning, so superseded lines are harmless until we compact. When the
    /// raw line count grows well past the number of unique entries, rewrite once to reclaim space.
    #[cfg(test)]
    fn upsert_catalog(&self, entry: CatalogEntry) -> Result<()> {
        // Held across the append *and* the possible compaction: a compactor's read-rewrite-rename
        // would otherwise silently drop a line another process appended in between.
        let _lock = self.lock_catalog()?;
        self.upsert_catalog_locked(entry)
    }

    fn upsert_catalog_locked(&self, entry: CatalogEntry) -> Result<()> {
        let path = self.catalog_path();
        let line = serde_json::to_string(&entry)?;
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {} for append", path.display()))?;
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.maybe_compact_catalog()?;
        Ok(())
    }

    /// Check for dead catalog rows only after its bytes have doubled since the last check, or its
    /// size fell (another writer compacted). This makes a growing sync's scans geometric in total
    /// bytes instead of reading/parsing the whole catalog after each session. Content is always
    /// read fresh; this schedule affects cleanup timing only. The first append in a new process
    /// checks once, and a successful compaction starts the schedule from its smaller output.
    /// Caller must hold the catalog lock (see [`Self::lock_catalog`]).
    fn maybe_compact_catalog(&self) -> Result<()> {
        let path = self.catalog_path();
        let size = fs::metadata(&path)
            .with_context(|| format!("stat'ing {}", path.display()))?
            .len();
        let check = self.compaction_check.get();
        if size >= check.last_size && size < check.next_size {
            self.compaction_check.set(CompactionCheck {
                last_size: size,
                ..check
            });
            return Ok(());
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        #[cfg(test)]
        self.catalog_bytes_scanned
            .set(self.catalog_bytes_scanned.get() + raw.len() as u64);
        let raw_lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
        let entries = Self::catalog_entries(&raw);
        // Compact only once we're carrying ~2x dead weight (and past a small floor).
        if raw_lines < 64 || raw_lines < entries.len() * 2 {
            self.schedule_compaction_check(raw.len() as u64);
            return Ok(());
        }
        let mut staged = cv_core::publish::Staged::beside(&path, "archive-catalog", 0o666)?;
        {
            let mut f = std::io::BufWriter::new(staged.file());
            for e in &entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.flush()?;
        }
        staged.publish(&path, cv_core::publish::Claim::Replace)?;
        let size = fs::metadata(&path)
            .with_context(|| format!("stat'ing {}", path.display()))?
            .len();
        self.schedule_compaction_check(size);
        Ok(())
    }

    fn schedule_compaction_check(&self, size: u64) {
        self.compaction_check.set(CompactionCheck {
            last_size: size,
            next_size: size.saturating_mul(2).max(1),
        });
    }
}

/// Preserve ordinary IDs while encoding bytes that can change path structure. Escape `%`
/// too, so a native `a/b` ID cannot collide with a literal `a%2Fb` ID.
fn archive_stem(id: &str) -> String {
    use std::fmt::Write as _;
    let mut stem = String::new();
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            stem.push(byte as char);
        } else {
            write!(stem, "%{byte:02X}").expect("writing to String");
        }
    }
    stem
}

/// A cwd for logs/listings: `~`-relative, or `-` when unknown.
fn pretty_cwd(cwd: Option<&Path>) -> String {
    cwd.map(home_rel).unwrap_or_else(|| "-".to_string())
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// `cv archive sync`: archive every session the index sweep inventories, once, then exit. The
/// sweep is the one corpus walk (compressed rollouts decoded once, the index refreshed on the
/// way); a home whose index cannot be written falls back to plain discovery.
pub(crate) fn cmd_sync(archive: &Archive) -> Result<()> {
    let refs = match cv_search::index_all(None, false, false, false) {
        Ok((_, refs)) => refs,
        Err(e) => {
            eprintln!("cv archive: index sweep unavailable ({e:#}); discovering directly");
            cv_core::discover_all()
        }
    };
    let total = refs.len();
    let mut archived = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        match archive.store_ref(adapter.as_ref(), &r) {
            Ok(pass) => match pass.outcome {
                StoreOutcome::Archived => archived += 1,
                StoreOutcome::Skipped => skipped += 1,
            },
            Err(e) => {
                failed += 1;
                eprintln!("cv archive: failed to archive {} {}: {e:#}", r.harness, r.id);
            }
        }
    }

    if failed > 0 {
        eprintln!("cv archive: {failed} session(s) failed (logged above)");
    }
    println!("sync complete: {archived} archived, {skipped} skipped (unchanged) of {total} discovered");
    println!("archive: {}", archive.home().display());
    Ok(())
}

/// `cv watch`: follow live activity and archive sessions as they change, forever.
pub(crate) fn cmd_watch(archive: &Archive, interval: f64, harness: Option<String>, cwd: Option<String>) -> Result<()> {
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    // Emit existing sessions on the first poll so a fresh watcher archives what's already there
    // before following live activity (`sync` is the dedicated one-shot, but watch is self-sufficient).
    let mut watcher = Watcher::new(filter, true);
    let interval = crate::util::poll_interval(interval)?;
    let mut wake = cv_core::watch::wake::Wake::new(interval)?;

    eprintln!(
        "cv watch: watching (interval {}s) -> {}",
        interval.as_secs_f64(),
        archive.home().display()
    );

    // Not `Watcher::run` (which never returns and takes the closure by value): a manual loop keeps
    // ownership of `archive` and reads identically.
    // What each accepted pass left on disk: the archived session and its resume state. A source
    // the watcher finds unchanged is offered again only when one of these moved, the same
    // condition under which `store_ref` would not skip it.
    let mut outputs: std::collections::HashMap<(Harness, String), [cv_core::source::FileId; 2]> =
        std::collections::HashMap::new();
    loop {
        let mut fatal = None;
        let mut accepted = Vec::new();
        watcher.offer_sources(
            |r| {
                outputs
                    .get(&(r.harness, r.id.clone()))
                    .is_some_and(|ids| archive.outputs(r).as_ref() == Some(ids))
            },
            |kind, r, previous| match archive_event(archive, r, kind, previous) {
                Ok(pass) => {
                    if pass.is_some() {
                        accepted.push(r.clone());
                    }
                    pass
                }
                Err(error) => {
                    fatal = Some(error);
                    None
                }
            },
        );
        for r in accepted {
            match archive.outputs(&r) {
                Some(ids) => outputs.insert((r.harness, r.id), ids),
                None => outputs.remove(&(r.harness, r.id)),
            };
        }
        if let Some(error) = fatal {
            return Err(error);
        }
        watcher.watch_dirs(&mut wake);
        wake.wait(Some(interval))?;
    }
}

/// Archive in one consumer read; failure leaves the watcher's source position retryable.
fn archive_event(
    archive: &Archive,
    r: &SessionRef,
    kind: EventKind,
    previous: usize,
) -> Result<Option<(usize, cv_core::watch::ReadState)>> {
    let adapter = cv_core::harness::rootless(r.harness).with_context(|| {
        format!(
            "archive cannot read {}: no adapter for {} in this build",
            r.id, r.harness
        )
    })?;
    let pass = match archive.store_ref(adapter, r) {
        Ok(pass) => pass,
        Err(e) => {
            eprintln!("[archive-fail] {} {}: {e:#}", r.harness, short_id(&r.id));
            return Ok(None);
        }
    };
    match pass.outcome {
        StoreOutcome::Archived => {
            let verb = match kind {
                EventKind::New => "new",
                EventKind::Updated => "archived",
            };
            eprintln!(
                "[{verb}] {} {}  +{} msgs  {}",
                r.harness,
                short_id(&r.id),
                pass.total.saturating_sub(previous),
                pretty_cwd(r.cwd.as_deref())
            );
        }
        StoreOutcome::Skipped => {}
    }
    Ok(Some((pass.total, pass.read)))
}

/// `cv archive ls`: what's in the archive, newest activity first.
pub(crate) fn cmd_ls(archive: &Archive) -> Result<()> {
    let mut entries = archive.load_catalog()?;
    if entries.is_empty() {
        println!("(archive empty — run `cv archive sync`)");
        println!("archive: {}", archive.home().display());
        return Ok(());
    }
    entries.sort_by(|a, b| {
        b.session
            .updated_at
            .unwrap_or(b.archived_at)
            .cmp(&a.session.updated_at.unwrap_or(a.archived_at))
    });

    for e in &entries {
        let title = e.session.title.clone().unwrap_or_else(|| "(untitled)".to_string());
        let archived = e.archived_at.format("%Y-%m-%d %H:%M");
        println!(
            "{:<9} {:<10} {:>4} msgs  {}  [{}]  {}",
            e.session.harness.as_str(),
            short_id(&e.session.id),
            e.session
                .message_count
                .map_or_else(|| "-".to_string(), |n| n.to_string()),
            archived,
            pretty_cwd(e.session.cwd.as_deref()),
            truncate(&title.replace('\n', " "), 60),
        );
    }
    println!("\n{} session(s) in {}", entries.len(), archive.home().display());
    Ok(())
}

/// `cv archive path`: print the archive location.
pub(crate) fn cmd_path(archive: &Archive) -> Result<()> {
    println!("{}", archive.home().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_event_retries_a_failed_source_then_reads_without_a_detected_store() {
        let world = cv_testkit::World::in_process("archive-detached", cv_core::env::Env::enter_isolated);
        let archive = Archive::resolve(Some(world.base.join("archive"))).unwrap();
        let path = world.base.join("detached.jsonl");
        fs::create_dir(&path).unwrap();
        let r = SessionRef::new("detached", Harness::Claude, &path);
        assert!(archive_event(&archive, &r, EventKind::New, 0).unwrap().is_none());
        fs::remove_dir(&path).unwrap();
        fs::write(
            &path,
            r#"{"type":"user","uuid":"u","sessionId":"detached","message":{"role":"user","content":"hello"}}
"#,
        )
        .unwrap();
        assert!(archive_event(&archive, &r, EventKind::New, 0).unwrap().is_some());
        assert_eq!(archive.load_catalog().unwrap().len(), 1);
    }

    #[test]
    fn archive_ids_are_single_components_without_encoding_collisions() {
        assert_eq!(archive_stem("normal-session_1.2"), "normal-session_1.2");
        for id in ["../escape", "/absolute", "a/b", "a\\b", "a%2Fb", "月", ""] {
            let path = Archive::rel_session_path(Harness::Claude, id);
            assert_eq!(path.parent().unwrap(), Path::new("archive/claude"));
            assert_eq!(path.components().count(), 3);
        }
        assert_ne!(archive_stem("a/b"), archive_stem("a%2Fb"));
        let a = archive();
        let mut session = Session::new("../../escape", Harness::Claude);
        session.messages.push(cv_core::Message::new(cv_core::Role::User));
        let mut recorded = entry(&session.id, 1);
        recorded.session.path = Archive::rel_session_path(session.harness, &session.id);
        a.upsert_catalog(recorded).unwrap();
        let catalog = a.load_catalog().unwrap();
        assert_eq!(catalog[0].session.id, "../../escape");
        assert_eq!(catalog[0].session.path.parent().unwrap(), Path::new("archive/claude"));
        fs::remove_dir_all(a.home()).unwrap();
    }

    fn archive() -> Archive {
        let home = std::env::temp_dir().join(format!("cv-archive-test-{}", uuid::Uuid::new_v4()));
        Archive::resolve(Some(home)).unwrap()
    }

    fn entry(id: &str, revision: i64) -> CatalogEntry {
        CatalogEntry {
            session: SessionRef {
                title: Some(format!("revision {revision}")),
                message_count: Some(1),
                ..SessionRef::new(id, Harness::Claude, format!("archive/claude/{id}.json"))
            },
            messages_written: 1,
            archived_at: DateTime::from_timestamp(revision, 0).unwrap(),
        }
    }

    /// A 0.17 line's `message_count` was the raw count: it reads as `messages_written`, with the
    /// turns not counted; a current line round-trips.
    #[test]
    fn catalog_lines_of_both_shapes_read() {
        let old = r#"{"harness":"claude","id":"s","title":"t","message_count":48,"archived_at":"2026-10-11T02:24:54Z","path":"archive/claude/s.json"}"#;
        let read: CatalogEntry = serde_json::from_str(old).unwrap();
        assert_eq!((read.messages_written, read.session.message_count), (48, None));
        assert_eq!(read.session.path, Path::new("archive/claude/s.json"));
        let line = serde_json::to_string(&entry("s", 1)).unwrap();
        assert_eq!(
            line,
            r#"{"id":"s","harness":"claude","path":"archive/claude/s.json","title":"revision 1","message_count":1,"messages_written":1,"archived_at":"1970-01-01T00:00:01Z"}"#
        );
        let back: CatalogEntry = serde_json::from_str(&line).unwrap();
        assert_eq!((back.messages_written, back.session.message_count), (1, Some(1)));
    }

    #[test]
    fn catalog_scan_bytes_grow_linearly_across_many_archives() {
        let a = archive();
        for i in 0..1024 {
            a.upsert_catalog(entry(&format!("session-{i:04}"), i)).unwrap();
        }
        let size = fs::metadata(a.catalog_path()).unwrap().len();
        let scanned = a.catalog_bytes_scanned.get();
        eprintln!("1024 catalog appends: {scanned} scanned bytes, {size} final bytes");
        assert!(scanned <= 4 * size, "scanned {scanned} bytes for a {size}-byte catalog");
        assert_eq!(a.load_catalog().unwrap().len(), 1024);
        // Every append remains visible immediately, even when no compaction check ran.
        assert_eq!(fs::read_to_string(a.catalog_path()).unwrap().lines().count(), 1024);
        fs::remove_dir_all(a.home()).unwrap();
    }

    #[test]
    fn independent_catalog_writers_keep_each_others_updates() {
        let a = archive();
        let home = a.home().to_path_buf();
        let writers: Vec<_> = (0..2)
            .map(|writer| {
                let home = home.clone();
                std::thread::spawn(move || {
                    let a = Archive::resolve(Some(home)).unwrap();
                    for revision in 0..256 {
                        let id = format!("writer-{writer}-{}", revision % 8);
                        a.upsert_catalog(entry(&id, revision)).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let entries = a.load_catalog().unwrap();
        assert_eq!(entries.len(), 16);
        for e in entries {
            let slot: i64 = e.session.id.rsplit('-').next().unwrap().parse().unwrap();
            assert_eq!(e.archived_at.timestamp(), 248 + slot);
        }
        // Repeated updates still reclaim dead rows as growth crosses a check boundary.
        assert!(fs::read_to_string(a.catalog_path()).unwrap().lines().count() < 128);
        fs::remove_dir_all(a.home()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_compaction_keeps_the_append_and_retries_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let a = archive();
        fs::create_dir_all(a.home()).unwrap();
        let raw: String = (0..64)
            .map(|i| format!("{}\n", serde_json::to_string(&entry("same", i)).unwrap()))
            .collect();
        fs::write(a.catalog_path(), raw).unwrap();
        // The append writes the existing catalog; the compaction's staged file cannot be created.
        drop(a.lock_catalog().unwrap());
        let dir = a.catalog_path().parent().unwrap().to_owned();
        let writable = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let failed = a.upsert_catalog(entry("same", 64));
        fs::set_permissions(&dir, writable).unwrap();
        assert!(failed.is_err());
        assert_eq!(a.load_catalog().unwrap()[0].archived_at.timestamp(), 64);
        assert_eq!(fs::read_to_string(a.catalog_path()).unwrap().lines().count(), 65);
        a.upsert_catalog(entry("same", 65)).unwrap();
        assert_eq!(a.load_catalog().unwrap()[0].archived_at.timestamp(), 65);
        assert_eq!(fs::read_to_string(a.catalog_path()).unwrap().lines().count(), 1);
        fs::remove_dir_all(a.home()).unwrap();
    }
}
