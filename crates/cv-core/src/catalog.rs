//! A persistent SQLite catalog of discovered sessions: `id → (harness, path, metadata)` — and,
//! since v0.9.12, the **hot read path** for listing the fleet.
//!
//! [`crate::find`] used to resolve one session id by calling `discover()` on **every** adapter and
//! scanning the whole fleet — O(all sessions across all harnesses) for a single `cv show <id>`. The
//! catalog turns that into an indexed lookup: [`crate::discover_all`] publishes every successful harness's rows
//! here, and `find` consults it first (O(1)/O(log n)), falling back to a full scan only on a miss
//! (which then re-warms the catalog).
//!
//! The same rows now also serve [`crate::sessions`], which answers `cv ls`/`stats`
//! without touching the fleet at all. Correctness rests on the **freshness probe**
//! ([`Reader::probe_stale`]): every full discovery records, per harness, the directories that directly
//! contain session files (plus all ancestors up to the harness root) and — for the SQLite-backed
//! harnesses — the database files and their WAL sidecars, each with its mtime at scan time (the `watched`
//! table). The probe re-stats those few hundred paths in milliseconds; any new/changed/vanished
//! entry marks its harness for re-discovery.
//!
//! Sibling tables: `scan_cache` is [`crate::discover_cache`]'s per-file `(mtime, size) → refs`
//! store (it owns the reads/writes; the DDL lives here so every opener sees one schema),
//! `catalog_meta` holds the `last_full_sync` stamp behind the bounded-staleness backstop, and
//! `skipped_sources` the sources each harness's last discovery could not open.
//!
//! Best-effort throughout: a catalog that cannot open says so once per process
//! ([`crate::env::warn_once`]) and the caller falls back to scanning, so
//! correctness never depends on the catalog (a stale row is filtered by `path.exists()` at lookup).
//! Behind the `sqlite` feature (default-on); no-op fallbacks keep the scan path working without it.

use crate::ir::{Harness, SessionRef};

#[cfg(feature = "sqlite")]
mod order;
mod reader;
pub(crate) use reader::Reader;
mod writer;
#[cfg(feature = "sqlite")]
use std::path::{Path, PathBuf};
pub use writer::Writer;

pub(crate) type PrimaryReads = std::collections::HashMap<std::path::PathBuf, crate::source::ReadSet>;

pub(crate) type Publication<'a> = (
    Harness,
    &'a [SessionRef],
    Vec<(std::path::PathBuf, Watched)>,
    &'a PrimaryReads,
);

/// SQL row loading may precede parallel discovery; source identity capture must not.
pub(crate) fn capture_primary_reads_from(a: &dyn crate::Adapter, refs: &[SessionRef]) -> PrimaryReads {
    let mut out = PrimaryReads::default();
    for r in refs {
        let path = a.resume_source(r);
        out.entry(path).or_insert_with_key(|path| {
            let mut reads = crate::source::ReadSet::default();
            reads.record(path);
            reads
        });
    }
    out
}

/// Queryable session metadata and structural counts captured alongside a completed `stream_counted`
/// pass so `model:`, `git:`, `compactions`, `agents`, `has:compacted`, and `has:subagents` resolve
/// from `catalog.db` (`Cost::Counted`) without re-parsing transcripts or walking sub-agent trees.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionQueryFacts {
    /// Representative display/session model stored in `sessions.model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model: Option<String>,
    /// Distinct lowercased model IDs across `session.model` and every streamed `message.model`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    #[serde(default)]
    pub compactions: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_count: Option<u32>,
    #[serde(default)]
    pub subagent_guard: crate::source::ReadSet,
}

/// Persisted `sessions`-table query columns read back from `catalog.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSessionFields {
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub git_remote: Option<String>,
    pub compactions: Option<u32>,
    pub subagent_count: Option<u32>,
}

/// What a complete read counted in one session: its affordances, its turns when the pass also
/// counted them, and its indexed query facts.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Counted {
    pub counts: crate::affordance::Counts,
    pub turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<SessionQueryFacts>,
}

#[cfg(feature = "sqlite")]
impl crate::source::Payload for Counted {
    const TAG: &'static str = "counts";
    const BY_ROWS: bool = true;
}

/// One watched path as discovery recorded it: its identity, an optional file's stable absence
/// (fresh until it appears), or unknown (unreadable, or modified too recently: never fresh).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) enum Watched {
    Unknown,
    Absent,
    Is(crate::source::FileId),
}

#[cfg(feature = "sqlite")]
mod imp {
    use super::order;
    use super::*;
    use crate::source::FileId;
    use rusqlite::{params, Connection, OptionalExtension, Row};
    use std::collections::{BTreeSet, HashSet};
    use std::time::{Duration, SystemTime};

    /// A directory (or db file) modified within this of being recorded is [`Watched::Unknown`]
    /// ("always stale"): a session file may have landed *between* discovery enumerating the
    /// directory and us stat'ing it, so the next probe re-discovers that harness once and
    /// re-records a trustworthy identity. Self-quiescing: an actively-written harness pays one
    /// cheap re-discovery per probe until it goes idle for this long.
    const WATCH_FUDGE: Duration = Duration::from_secs(2);

    impl Watched {
        fn value(self) -> rusqlite::types::Value {
            use rusqlite::types::Value;
            match self {
                Watched::Unknown => Value::Null,
                Watched::Absent => Value::Blob(Vec::new()),
                Watched::Is(id) => Value::Blob(id.to_bytes().into()),
            }
        }

        fn stored(bytes: Option<Vec<u8>>) -> Self {
            match bytes {
                None => Watched::Unknown,
                Some(bytes) if bytes.is_empty() => Watched::Absent,
                Some(bytes) => FileId::from_bytes(&bytes).map_or(Watched::Unknown, Watched::Is),
            }
        }
    }

    /// Path to the rebuildable session/event catalog SQLite database (`<cv_cache>/catalog.db`).
    ///
    /// Uses [`crate::env::Env::cv_cache`] (`$CLUSTERVISION_CACHE`, else `$CLUSTERVISION_HOME` when
    /// set in isolated homes/tests, else `<cache_dir>/clustervision`) so `catalog.db` never creates
    /// split-brain or 0-byte files inside durable `~/.clustervision`.
    pub(crate) fn db_path() -> Option<PathBuf> {
        let dir = crate::env::Env::current().cv_cache()?;
        if !crate::env::frozen() {
            if let Err(error) = std::fs::create_dir_all(&dir) {
                crate::env::warn_once("the catalog", format_args!("{}: {error}", dir.display()));
            }
        }
        Some(dir.join("catalog.db"))
    }

    /// An open connection to the shared catalog db (WAL + busy timeout, `sessions` DDL applied),
    /// for sibling catalog tables — the event catalog ([`crate::events`]) adds its own DDL on top.
    /// `None` on any error; callers degrade.
    pub(crate) fn open_db() -> Option<Connection> {
        open()
    }

    #[cfg(test)]
    thread_local! { static OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

    fn open() -> Option<Connection> {
        open_at(&db_path()?)
    }

    pub(crate) fn open_at(path: &Path) -> Option<Connection> {
        #[cfg(test)]
        OPEN_COUNT.with(|n| n.set(n.get() + 1));
        if crate::env::frozen() {
            // Even mode=ro can create/update WAL shared memory.
            return open_checkpoint(path);
        }
        let conn = match open_writable(path) {
            Ok(conn) => Some(conn),
            Err(error) => {
                crate::env::warn_once("the catalog", format_args!("{}: {error}", path.display()));
                open_read_only(path)
            }
        };
        if conn.is_none() {
            // Every consumer degrades to scanning; the process says so once.
            crate::env::warn_once(
                "the catalog",
                format_args!("{} cannot be opened or created", path.display()),
            );
        }
        conn
    }

    fn note_sql(error: &rusqlite::Error) {
        if !matches!(error, rusqlite::Error::QueryReturnedNoRows) {
            crate::env::warn_once("the catalog", error);
        }
    }

    /// Prefer SQLite's WAL-aware reader. A closed, checkpointed read-only copy may lack
    /// writable WAL coordination files; only that case can use the immutable reader.
    fn open_read_only(path: &Path) -> Option<Connection> {
        read_only_connection(path, "mode=ro").or_else(|| open_checkpoint(path))
    }

    fn checkpoint_only(path: &Path) -> bool {
        ["-wal", "-journal"].iter().all(|suffix| {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            match std::fs::metadata(sidecar) {
                Ok(m) => m.is_file() && m.len() == 0,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            }
        })
    }

    /// Immutable is valid only for a checkpoint, never as a way around a live WAL read error.
    /// As with frozen reads, the copy must remain read-only for the connection's lifetime.
    fn open_checkpoint(path: &Path) -> Option<Connection> {
        if !checkpoint_only(path) {
            return None;
        }
        let conn = read_only_connection(path, "immutable=1")?;
        checkpoint_only(path).then_some(conn)
    }

    fn read_only_connection(path: &Path, query: &str) -> Option<Connection> {
        use rusqlite::OpenFlags;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let file = path
            .to_string_lossy()
            .replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23");
        let version = crate::discover_cache::INTERPRETATION_VERSION.to_string();
        let conn = Connection::open_with_flags(format!("file:{file}?{query}"), flags)
            .inspect_err(note_sql)
            .ok()?;
        conn.busy_timeout(Duration::from_secs(5)).inspect_err(note_sql).ok()?;
        let stored: String = conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                [],
                |row| row.get(0),
            )
            .inspect_err(note_sql)
            .ok()?;
        let schema: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .inspect_err(note_sql)
            .ok()?;
        (stored == version && schema == SCHEMA && scoped_versions_match(&conn).unwrap_or(false)).then_some(conn)
    }

    /// Physical DDL stamp for `catalog.db` (`PRAGMA user_version`), decoupled from
    /// [`crate::source::VERSION`]: codec and event version bumps invalidate only affected
    /// harness/consumer rows via `harness_versions` instead of dropping the whole catalog.
    const SCHEMA: i64 = 11;

    #[cfg(test)]
    type ScopedVersionOverride = (std::collections::HashMap<Harness, u32>, Option<u32>);

    #[cfg(test)]
    thread_local! {
        static SCOPED_VERSION_OVERRIDE: std::cell::RefCell<Option<ScopedVersionOverride>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Current codec revision for `h`, tracked per harness in `harness_versions.codec_v`.
    pub fn harness_codec_version(h: Harness) -> u32 {
        #[cfg(test)]
        if let Some(v) =
            SCOPED_VERSION_OVERRIDE.with(|slot| slot.borrow().as_ref().and_then(|(m, _)| m.get(&h).copied()))
        {
            return v;
        }
        let _ = h;
        crate::source::VERSION
    }

    /// Current event-extractor revision, tracked in `harness_versions.events_v`.
    pub fn events_codec_version() -> u32 {
        #[cfg(test)]
        if let Some(v) = SCOPED_VERSION_OVERRIDE.with(|slot| slot.borrow().as_ref().and_then(|(_, ev)| *ev)) {
            return v;
        }
        crate::source::VERSION
    }

    #[cfg(test)]
    pub(crate) fn with_scoped_versions<R>(
        codec_overrides: &[(Harness, u32)],
        events_override: Option<u32>,
        f: impl FnOnce() -> R,
    ) -> R {
        struct Guard(Option<ScopedVersionOverride>);
        impl Drop for Guard {
            fn drop(&mut self) {
                SCOPED_VERSION_OVERRIDE.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let map = codec_overrides.iter().copied().collect();
        let prev = SCOPED_VERSION_OVERRIDE.with(|slot| slot.replace(Some((map, events_override))));
        let _guard = Guard(prev);
        f()
    }

    fn scoped_versions_match(conn: &Connection) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare("SELECT harness, codec_v, events_v FROM harness_versions")?;
        let stored: std::collections::HashMap<String, (i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;
        let want_events = events_codec_version() as i64;
        Ok(Harness::ALL.iter().all(|h| {
            let want_codec = harness_codec_version(*h) as i64;
            stored.get(h.as_str()) == Some(&(want_codec, want_events))
        }))
    }

    fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    fn escape_like_prefix(dir: &str) -> String {
        let mut out = String::with_capacity(dir.len() + 4);
        for ch in dir.chars() {
            if matches!(ch, '\\' | '%' | '_') {
                out.push('\\');
            }
            out.push(ch);
        }
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push('%');
        out
    }

    fn invalidate_events_for_harness(tx: &rusqlite::Transaction, h: Harness) -> rusqlite::Result<()> {
        let name = h.as_str();
        tx.execute(
            "DELETE FROM source_dependencies WHERE consumer='events' AND harness=?1",
            [name],
        )?;
        if table_exists(tx, "event_sessions")? {
            if table_exists(tx, "events")? {
                tx.execute(
                    "DELETE FROM events WHERE session IN (SELECT id FROM event_sessions WHERE harness=?1)",
                    [name],
                )?;
            }
            tx.execute("DELETE FROM event_sessions WHERE harness=?1", [name])?;
        }
        Ok(())
    }

    /// Evict all cached rows belonging to `h` without touching any other harness's tables.
    fn invalidate_harness_in(tx: &rusqlite::Transaction, h: Harness) -> rusqlite::Result<()> {
        let name = h.as_str();
        let mut paths = BTreeSet::new();
        for sql in [
            "SELECT path FROM sessions WHERE harness=?1",
            "SELECT path FROM primary_sources WHERE harness=?1",
            "SELECT path FROM watched WHERE harness=?1",
        ] {
            let mut stmt = tx.prepare(sql)?;
            for p in stmt.query_map([name], |r| r.get::<_, String>(0))? {
                paths.insert(p?);
            }
        }
        if let Some(adapter) = crate::harness::for_harness(h) {
            for root in adapter.roots() {
                paths.insert(root.to_string_lossy().into_owned());
            }
        }
        for p in &paths {
            let prefix = escape_like_prefix(p);
            tx.execute(
                "DELETE FROM scan_cache WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
                params![p, prefix],
            )?;
        }
        tx.execute("DELETE FROM sessions WHERE harness=?1", [name])?;
        tx.execute("DELETE FROM watched WHERE harness=?1", [name])?;
        tx.execute("DELETE FROM primary_sources WHERE harness=?1", [name])?;
        tx.execute("DELETE FROM resume_marks WHERE harness=?1", [name])?;
        tx.execute("DELETE FROM skipped_sources WHERE harness=?1", [name])?;
        tx.execute("DELETE FROM source_dependencies WHERE harness=?1", [name])?;
        invalidate_events_for_harness(tx, h)?;
        Ok(())
    }

    fn open_writable(path: &Path) -> rusqlite::Result<Connection> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let schema: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let version = crate::discover_cache::INTERPRETATION_VERSION.to_string();
        // A stamp certifies the complete CREATE phase. Current catalogs whose scoped per-harness
        // `(codec_v, events_v)` also match take the read-only fast path without a writer lock.
        if schema == SCHEMA {
            let old: Option<String> = conn
                .query_row(
                    "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if old.as_deref() == Some(&version) && scoped_versions_match(&conn)? {
                configure_journal(&conn)?;
                return Ok(conn);
            }
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let schema: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let mut rebuilt = false;
        let mut scoped_cache_invalidated = false;
        let want_events = events_codec_version() as i64;
        if schema != SCHEMA {
            let tables = tx
                .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rebuilt = !tables.is_empty();
            for table in tables {
                tx.execute_batch(&format!("DROP TABLE \"{}\";", table.replace('"', "\"\"")))?;
            }
            tx.execute_batch(SCHEMA_SQL)?;
            for h in Harness::ALL {
                tx.execute(
                    "INSERT OR REPLACE INTO harness_versions(harness, codec_v, events_v) VALUES(?1, ?2, ?3)",
                    params![h.as_str(), harness_codec_version(h) as i64, want_events],
                )?;
            }
        } else {
            let stored: std::collections::HashMap<String, (i64, i64)> = {
                let mut stmt = tx.prepare("SELECT harness, codec_v, events_v FROM harness_versions")?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
                    .collect::<rusqlite::Result<_>>()?;
                rows
            };
            for h in Harness::ALL {
                let want_codec = harness_codec_version(h) as i64;
                match stored.get(h.as_str()).copied() {
                    Some((c, e)) if c == want_codec && e == want_events => {}
                    Some((c, _)) if c != want_codec => {
                        invalidate_harness_in(&tx, h)?;
                        scoped_cache_invalidated = true;
                        tx.execute(
                            "INSERT OR REPLACE INTO harness_versions(harness, codec_v, events_v) VALUES(?1, ?2, ?3)",
                            params![h.as_str(), want_codec, want_events],
                        )?;
                    }
                    Some((_, _)) => {
                        invalidate_events_for_harness(&tx, h)?;
                        tx.execute(
                            "INSERT OR REPLACE INTO harness_versions(harness, codec_v, events_v) VALUES(?1, ?2, ?3)",
                            params![h.as_str(), want_codec, want_events],
                        )?;
                    }
                    None => {
                        invalidate_harness_in(&tx, h)?;
                        scoped_cache_invalidated = true;
                        tx.execute(
                            "INSERT OR REPLACE INTO harness_versions(harness, codec_v, events_v) VALUES(?1, ?2, ?3)",
                            params![h.as_str(), want_codec, want_events],
                        )?;
                    }
                }
            }
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let changed = old.as_deref() != Some(&version);
        if changed {
            tx.execute("DELETE FROM watched", [])?;
            tx.execute("DELETE FROM primary_sources", [])?;
            tx.execute("DELETE FROM catalog_meta WHERE key='last_full_sync'", [])?;
            tx.execute(
                "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('discovery_version',?1)",
                [&version],
            )?;
            if old.is_some() || rebuilt {
                tx.execute(
                    "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('rediscovery_notice',?1)",
                    [&version],
                )?;
            }
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA};"))?;
        tx.commit()?;
        configure_journal(&conn)?;
        if changed || scoped_cache_invalidated {
            crate::discover_cache::invalidate_interpretation(path);
        }
        if rebuilt {
            if let Err(error) = conn.execute_batch("VACUUM;") {
                crate::env::warn_once("catalog compaction", error);
            }
        }
        Ok(conn)
    }

    // WAL is an accelerator too. A simultaneous first opener can hold the default-journal
    // schema lock while another switches modes; keep its usable connection and retry next open.
    fn configure_journal(conn: &Connection) -> rusqlite::Result<()> {
        match conn.execute_batch("PRAGMA journal_mode=WAL;") {
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                crate::env::warn_once("catalog WAL mode", &error);
                Ok(())
            }
            result => result,
        }
    }

    const SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS sessions(
                 harness        TEXT NOT NULL,
                 id             TEXT NOT NULL,
                 path           TEXT NOT NULL,
                 cwd            TEXT,
                 title          TEXT,
                 created_at     INTEGER,
                 updated_at     INTEGER,
                 message_count  INTEGER,
                 created_ns     INTEGER,
                 updated_ns     INTEGER,
                 writer_version TEXT,
                 model          TEXT,
                 git_branch     TEXT,
                 git_remote     TEXT,
                 compactions    INTEGER,
                 subagent_count INTEGER,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE INDEX IF NOT EXISTS idx_sessions_model ON sessions(model);
             CREATE INDEX IF NOT EXISTS idx_sessions_git_branch ON sessions(git_branch);
             CREATE TABLE IF NOT EXISTS harness_versions(
                 harness  TEXT PRIMARY KEY,
                 codec_v  INTEGER NOT NULL,
                 events_v INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS watched(
                 harness  TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 id       BLOB,
                 PRIMARY KEY(harness, path)
             );
             CREATE TABLE IF NOT EXISTS primary_sources(
                 harness TEXT NOT NULL, path TEXT NOT NULL, reads TEXT NOT NULL,
                 PRIMARY KEY(harness,path)
             );
             CREATE TABLE IF NOT EXISTS scan_cache(
                 path     TEXT PRIMARY KEY,
                 mtime_ns INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 refs     TEXT NOT NULL,
                 interpretation_version INTEGER,
                 source_reads TEXT,
                 continuation TEXT
             );
             CREATE TABLE IF NOT EXISTS catalog_meta(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS resume_marks(
                 harness TEXT NOT NULL,
                 id      TEXT NOT NULL,
                 path    TEXT NOT NULL,
                 offset  INTEGER NOT NULL,
                 hash    INTEGER NOT NULL,
                 version TEXT NOT NULL,
                 state   TEXT NOT NULL,
                 identity TEXT,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE TABLE IF NOT EXISTS source_dependencies(
                 consumer TEXT NOT NULL, harness TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL,
                 record BLOB NOT NULL, PRIMARY KEY(consumer, harness, id, path)
             );
             CREATE TABLE IF NOT EXISTS skipped_sources(
                 harness TEXT NOT NULL,
                 path    TEXT NOT NULL,
                 why     TEXT NOT NULL,
                 PRIMARY KEY(harness, path)
             );";

    fn watch_is_fresh(path: &Path, recorded: Watched) -> bool {
        match std::fs::metadata(path) {
            Ok(md) => matches!(recorded, Watched::Is(id) if FileId::of(&md) == Some(id)),
            Err(e) => recorded == Watched::Absent && e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// The sessions column list, in order: [`row_to_ref`] decodes it and `insert_refs` names it —
    /// keep all three in lockstep.
    pub(super) const REF_COLUMNS: &str =
        "harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns";

    /// A timestamp from the nanosecond column at `ns_idx`, falling back to the second-precision
    /// column at `s_idx` (rows written before the `_ns` migration, or dates outside the i64-ns
    /// range — ±~292 years of 1970, where `timestamp_nanos_opt` stores NULL).
    fn ts_col(row: &Row, ns_idx: usize, s_idx: usize) -> rusqlite::Result<Option<chrono::DateTime<chrono::Utc>>> {
        if let Some(ns) = row.get::<_, Option<i64>>(ns_idx)? {
            return Ok(Some(chrono::DateTime::from_timestamp_nanos(ns)));
        }
        Ok(row
            .get::<_, Option<i64>>(s_idx)?
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)))
    }

    /// One row (in [`REF_COLUMNS`] order) → one `SessionRef`. `Ok(None)` for rows whose harness
    /// this binary doesn't know (a newer binary's rows — skipped, never deleted by us).
    pub(super) fn row_to_ref(row: &Row) -> rusqlite::Result<Option<SessionRef>> {
        row_to_ref_projected(row, [false, false])
    }

    // Preserve cell type/UTF8 acceptance even when a hit overrides the catalog hint. Borrow the
    // native SQLite cell for validation; do not allocate a String that output will discard.
    fn optional_hint(row: &Row, index: usize, overridden: bool) -> rusqlite::Result<Option<String>> {
        let value = row.get_ref(index)?;
        if matches!(value, rusqlite::types::ValueRef::Null) {
            return Ok(None);
        }
        let text = value
            .as_str()
            .map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error)))?;
        Ok((!overridden).then(|| text.to_owned()))
    }

    fn row_to_ref_projected(row: &Row, overrides: [bool; 2]) -> rusqlite::Result<Option<SessionRef>> {
        let harness: String = row.get(0)?;
        let Some(harness) = Harness::parse(&harness) else {
            return Ok(None);
        };
        Ok(Some(SessionRef {
            cwd: optional_hint(row, 3, overrides[0])?.map(PathBuf::from),
            title: optional_hint(row, 4, overrides[1])?,
            created_at: ts_col(row, 8, 5)?,
            updated_at: ts_col(row, 9, 6)?,
            message_count: row.get::<_, Option<i64>>(7)?.map(|n| n as usize),
            ..SessionRef::new(
                row.get::<_, String>(1)?,
                harness,
                PathBuf::from(row.get::<_, String>(2)?),
            )
        }))
    }

    /// Upsert every ref in one transaction, deleting nothing — the *additive* sync for callers
    /// that know about one session (event ingest, tests). A known row is rewritten in place and a
    /// new one follows its harness's rows ([`order::upsert`]). Full discovery uses
    /// [`Reader::replace_harness`] instead so vanished sessions actually disappear.
    pub fn sync(refs: &[SessionRef]) {
        if crate::env::frozen() {
            return;
        }
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction().inspect_err(note_sql) else {
            return;
        };
        if refs
            .iter()
            .try_for_each(|r| order::upsert(&tx, r))
            .inspect_err(note_sql)
            .is_ok()
            && refs
                .iter()
                .map(|r| r.harness)
                .collect::<HashSet<_>>()
                .into_iter()
                .all(|h| publish_writer_versions(&tx, h).inspect_err(note_sql).is_ok())
        {
            if let Err(error) = tx.commit() {
                note_sql(&error);
            }
        }
    }

    /// Bracket known primary sources before discovery. New uncached paths stay unknown until
    /// a later scoped pass; never bless an identity sampled after its metadata was read.
    pub(super) fn primary_refs_in(conn: &Connection, h: Option<Harness>) -> Vec<SessionRef> {
        // Source selection uses only harness, ID and path. Do not decode title/cwd/time metadata
        // that discovery will replace; this inventory exists solely to bracket primary reads.
        let sql = if h.is_some() {
            "SELECT harness,id,path FROM sessions WHERE harness=?1 ORDER BY rowid"
        } else {
            "SELECT harness,id,path FROM sessions WHERE ?1 IS NULL ORDER BY rowid"
        };
        let Ok(mut stmt) = conn.prepare(sql).inspect_err(note_sql) else {
            return Default::default();
        };
        let Ok(rows) = stmt
            .query_map([h.map(Harness::as_str)], |row| {
                let name: String = row.get(0)?;
                let Some(harness) = Harness::parse(&name) else {
                    return Ok(None);
                };
                Ok(Some(SessionRef {
                    cwd: None,
                    title: None,
                    created_at: None,
                    updated_at: None,
                    message_count: None,
                    ..SessionRef::new(
                        row.get::<_, String>(1)?,
                        harness,
                        PathBuf::from(row.get::<_, String>(2)?),
                    )
                }))
            })
            .inspect_err(note_sql)
        else {
            return Default::default();
        };
        rows.filter_map(Result::transpose)
            .collect::<rusqlite::Result<Vec<_>>>()
            .inspect_err(note_sql)
            .unwrap_or_default()
    }

    /// Replace one harness's catalog rows *and* its watch entries in a single transaction — the
    /// post-discovery sync. Vanished sessions' rows are deleted and only changed rows are
    /// written ([`order::publish`]); the transaction keeps concurrent readers from seeing a gap.
    #[cfg(test)]
    pub(crate) fn replace_harness(
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, Watched)],
        before: &PrimaryReads,
    ) {
        if let Some(mut conn) = open() {
            replace_harness_in(&mut conn, h, refs, watches, before);
        }
    }

    #[cfg(test)]
    pub(super) fn replace_harness_in(
        conn: &mut Connection,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, Watched)],
        before: &PrimaryReads,
    ) {
        let sources = prepare_sources(h, refs, before);
        replace_prepared_in(conn, h, refs, watches, &sources);
    }

    pub(super) fn replace_prepared_in(
        conn: &mut Connection,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, Watched)],
        sources: &SourceRows,
    ) {
        if crate::env::frozen() {
            return;
        }
        // Reserve the writer before reading old rows; deferred upgrades cannot wait safely.
        let Ok(tx) = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .inspect_err(note_sql)
        else {
            return;
        };
        if publish_harness_in(&tx, h, refs, watches, sources)
            .inspect_err(note_sql)
            .is_ok()
        {
            if let Err(error) = tx.commit() {
                note_sql(&error);
            }
        }
    }

    #[cfg(test)]
    fn matches_sessions_in<'a>(
        conn: &Connection,
        refs: impl Iterator<Item = &'a SessionRef>,
    ) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))?;
        let rows = stmt.query_map([], row_to_ref)?;
        let mut refs = refs;
        for row in rows {
            let Some(a) = row? else { return Ok(false) };
            let Some(b) = refs.next() else { return Ok(false) };
            if !order::same_row(&a, b) {
                return Ok(false);
            }
        }
        Ok(refs.next().is_none())
    }

    #[cfg(test)]
    fn matches_sessions<'a>(refs: impl Iterator<Item = &'a SessionRef>) -> bool {
        open()
            .and_then(|conn| matches_sessions_in(&conn, refs).inspect_err(note_sql).ok())
            .unwrap_or(false)
    }

    /// Compare and publish the full ordered output in one SQL snapshot. Cache guards are
    /// prepared before taking the writer lock, preserving the cache-to-SQL lock order.
    #[cfg(test)]
    fn publish_all(rows: &[Publication<'_>]) {
        if let Some(mut reader) = Reader::open() {
            reader.publish_all(rows);
        }
    }

    pub(super) fn publish_all_in(conn: &mut Connection, rows: &[Publication<'_>], sources: &[SourceRows]) {
        if crate::env::frozen() {
            return;
        }
        // Reserve the writer before reading old rows; deferred upgrades cannot wait safely.
        let Ok(tx) = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .inspect_err(note_sql)
        else {
            return;
        };
        for ((h, refs, watches, _), sources) in rows.iter().zip(sources) {
            if publish_harness_in(&tx, *h, refs, watches, sources)
                .inspect_err(note_sql)
                .is_err()
            {
                return;
            }
        }
        if stamp_full_sync_in(&tx).inspect_err(note_sql).is_ok() {
            if let Err(error) = tx.commit() {
                note_sql(&error);
            }
        }
    }

    /// Reconcile existing watch/guard rows without rewriting equal values.
    fn reconcile_rows(
        tx: &rusqlite::Transaction,
        h: Harness,
        table: &str,
        value: &str,
        rows: &std::collections::HashMap<String, rusqlite::types::Value>,
    ) -> rusqlite::Result<()> {
        let mut stmt = tx.prepare(&format!("SELECT path,{value} FROM {table} WHERE harness=?1"))?;
        let old: std::collections::HashMap<String, rusqlite::types::Value> = stmt
            .query_map([h.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for path in old.keys().filter(|path| !rows.contains_key(*path)) {
            tx.execute(
                &format!("DELETE FROM {table} WHERE harness=?1 AND path=?2"),
                params![h.as_str(), path],
            )?;
        }
        let mut insert = tx.prepare(&format!(
            "INSERT OR REPLACE INTO {table}(harness,path,{value}) VALUES(?1,?2,?3)"
        ))?;
        for (path, val) in rows.iter().filter(|(path, val)| old.get(*path) != Some(*val)) {
            insert.execute(params![h.as_str(), path, val])?;
        }
        Ok(())
    }

    pub(super) type SourceRows = std::collections::HashMap<String, rusqlite::types::Value>;

    pub(super) fn prepare_sources(h: Harness, refs: &[SessionRef], before: &PrimaryReads) -> SourceRows {
        // Resolve cache guards before acquiring the SQLite writer: cache persistence takes
        // the cache lock first, so reversing that order here could deadlock.
        let mut source_rows = std::collections::HashMap::new();
        let adapter = crate::harness::for_harness(h);
        let mut seen = HashSet::new();
        for r in refs {
            if !seen.insert(&r.path) {
                continue;
            }
            let primary = adapter.as_ref().map_or_else(|| r.path.clone(), |a| a.resume_source(r));
            let reads = before
                .get(&primary)
                .cloned()
                .or_else(|| crate::discover_cache::captured_reads(&primary));
            if let Some(reads) = reads {
                if let Ok(json) = serde_json::to_string(&reads) {
                    source_rows.insert(
                        r.path.to_string_lossy().into_owned(),
                        rusqlite::types::Value::Text(json),
                    );
                }
            }
        }
        source_rows
    }

    fn publish_harness_in(
        tx: &rusqlite::Transaction,
        h: Harness,
        refs: &[SessionRef],
        watches: &[(PathBuf, Watched)],
        source_rows: &SourceRows,
    ) -> rusqlite::Result<()> {
        order::publish(tx, h, refs)?;
        publish_writer_versions(tx, h)?;
        let watch_rows = watches
            .iter()
            .map(|(path, watched)| (path.to_string_lossy().into_owned(), watched.value()))
            .collect();
        reconcile_rows(tx, h, "primary_sources", "reads", source_rows)?;
        reconcile_rows(tx, h, "watched", "id", &watch_rows)
    }

    /// Compute the watch set for one harness from a finished discovery: the harness roots, every
    /// ancestor directory between each session file and its root (so a new project/day directory
    /// anywhere in the tree changes a watched identity), and — for a session the adapter writes in
    /// place ([`crate::harness::WatchPolicy::Database`]) — its backing file and its WAL (in-place writes change neither directory mtimes
    /// nor necessarily the main database). Optional CLI metadata and absent WALs remain watched.
    /// Other paths that can't be stat'd are unknown, as are recently-modified paths
    /// (see [`WATCH_FUDGE`]).
    pub(crate) fn watches_for(
        adapter: &dyn crate::harness::Adapter,
        roots: &[PathBuf],
        refs: &[SessionRef],
    ) -> Vec<(PathBuf, Watched)> {
        let mut paths: BTreeSet<PathBuf> = roots.iter().cloned().collect();
        let mut optional_files = BTreeSet::new();
        for r in refs {
            if let Some(metadata) = adapter.metadata_file(r) {
                optional_files.insert(metadata.clone());
                paths.insert(metadata);
            }
            let sqlite_watch = adapter.watch_policy(r) == crate::harness::WatchPolicy::Database;
            if sqlite_watch {
                paths.insert(r.path.clone());
            }
            if sqlite_watch && adapter.sqlite_source(r).unwrap_or(true) {
                let mut wal = r.path.as_os_str().to_os_string();
                wal.push("-wal");
                let wal = PathBuf::from(wal);
                optional_files.insert(wal.clone());
                paths.insert(wal);
            }
            let root = roots.iter().map(PathBuf::as_path).find(|rt| r.path.starts_with(rt));
            let mut d = r.path.parent();
            while let Some(dir) = d {
                paths.insert(dir.to_path_buf());
                match root {
                    // Still strictly below the root: keep walking up.
                    Some(rt) if dir != rt && dir.starts_with(rt) => d = dir.parent(),
                    // Reached the root, or the path lives outside it: stop.
                    _ => break,
                }
            }
        }
        let now = SystemTime::now();
        paths
            .into_iter()
            .map(|p| {
                let md = match std::fs::metadata(&p) {
                    Ok(md) => md,
                    Err(e) if optional_files.contains(&p) && e.kind() == std::io::ErrorKind::NotFound => {
                        return (p, Watched::Absent);
                    }
                    Err(_) => return (p, Watched::Unknown),
                };
                // Too fresh (or clock skew): force one re-discovery next probe.
                let settled = md
                    .modified()
                    .ok()
                    .and_then(|mtime| now.duration_since(mtime).ok())
                    .is_some_and(|age| age >= WATCH_FUDGE);
                let watched = FileId::of(&md).filter(|_| settled);
                (p, watched.map_or(Watched::Unknown, Watched::Is))
            })
            .collect()
    }

    /// Stat-only freshness across watched directories/DB sidecars, every primary source,
    /// and newly installed roots. A shared Probe deduplicates primary paths and compares full
    /// identities, including inode/ctime on Unix. Missing or legacy primary baselines are stale.
    /// Unknown new noncached sources bootstrap through one later scoped discovery; the baseline
    /// must precede that read. SQLite failure returns None for full-discovery fallback.
    ///
    /// A first session in a previously empty deeper directory can still escape the directory
    /// watch set until the time backstop. Primary metadata checks do not hash file contents.
    pub(super) fn probe_stale_in(conn: &Connection) -> Option<Vec<Harness>> {
        let mut stale: Vec<Harness> = Vec::new();
        fn mark(stale: &mut Vec<Harness>, h: Harness) {
            if !stale.contains(&h) {
                stale.push(h);
            }
        }

        // 1. watched dirs + db files.
        let mut watched_harnesses: HashSet<Harness> = HashSet::new();
        let mut watched_paths: HashSet<(Harness, String)> = HashSet::new();
        {
            let mut stmt = conn
                .prepare("SELECT harness, path, id FROM watched")
                .inspect_err(note_sql)
                .ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        Watched::stored(r.get(2)?),
                    ))
                })
                .inspect_err(note_sql)
                .ok()?;
            for row in rows {
                let (h, path, recorded) = row.inspect_err(note_sql).ok()?;
                let Some(h) = Harness::parse(&h) else { continue };
                watched_harnesses.insert(h);
                watched_paths.insert((h, path.clone()));
                if stale.contains(&h) {
                    continue; // already condemned; skip the stat
                }
                if !watch_is_fresh(Path::new(&path), recorded) {
                    mark(&mut stale, h); // changed, vanished, or fudge-stamped
                }
            }
        }

        // Every primary source, with one identity-aware metadata probe shared across paths.
        {
            let mut stmt = conn.prepare("SELECT DISTINCT s.harness,s.path,p.reads FROM sessions s LEFT JOIN primary_sources p ON p.harness=s.harness AND p.path=s.path").inspect_err(note_sql).ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .inspect_err(note_sql)
                .ok()?;
            let mut probe = crate::source::Probe::default();
            for row in rows {
                let (h, _, json) = row.inspect_err(note_sql).ok()?;
                let Some(h) = Harness::parse(&h) else { continue };
                if stale.contains(&h) {
                    continue;
                }
                let reads = json.and_then(|s| {
                    serde_json::from_str::<crate::source::ReadSet>(&s)
                        .inspect_err(|error| {
                            crate::env::warn_once("the catalog", error);
                        })
                        .ok()
                });
                if !reads.is_some_and(|reads| reads.fresh(&mut probe)) {
                    mark(&mut stale, h);
                }
            }
        }

        // 3. harnesses that appeared (root now exists, no watch rows) or vanished outright, and
        //    existing roots with no watch row of their own (appeared since the last sync).
        for a in crate::harness::all() {
            let h = a.harness();
            if stale.contains(&h) {
                continue;
            }
            let unwatched_root = || {
                a.roots()
                    .iter()
                    .any(|r| r.exists() && !watched_paths.contains(&(h, r.to_string_lossy().into_owned())))
            };
            if !a.roots().is_empty() != watched_harnesses.contains(&h) || unwatched_root() {
                mark(&mut stale, h);
            }
        }

        Some(stale)
    }

    /// Every catalog row, in discovery order: each harness's rows sit in its own rowid span in
    /// the order its last publication listed them ([`order`]), so `rowid` order reproduces
    /// [`crate::discover_all`]'s output order exactly, after a full or a scoped publication —
    /// which keeps `cv ls`'s stable sorts byte-identical even across rows whose timestamps tie. `None` on any sqlite
    /// error — callers fall back to a full discovery. Note `Some(vec![])` is a valid answer (a
    /// machine with no sessions); cold-vs-empty is [`Reader::last_full_sync`]'s job.
    pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
        Reader::open()?.all_sessions()
    }

    pub(super) fn all_sessions_in(conn: &Connection) -> Option<Vec<SessionRef>> {
        let mut stmt = conn
            .prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))
            .inspect_err(note_sql)
            .ok()?;
        let rows = stmt.query_map([], row_to_ref).inspect_err(note_sql).ok()?;
        rows.filter_map(Result::transpose)
            .collect::<rusqlite::Result<Vec<_>>>()
            .inspect_err(note_sql)
            .ok()
    }

    /// Seconds-since-epoch of the last completed full discovery, if any. Absent = cold catalog.
    /// Consume the notice a discovery-version upgrade left; exactly one caller sees `true`.
    pub(super) fn take_rediscovery_notice_in(conn: &Connection) -> bool {
        if crate::env::frozen() {
            return false;
        }
        conn.execute("DELETE FROM catalog_meta WHERE key='rediscovery_notice'", [])
            .is_ok_and(|n| n > 0)
    }

    pub(super) fn last_full_sync_in(conn: &Connection) -> Option<i64> {
        conn.query_row("SELECT value FROM catalog_meta WHERE key='last_full_sync'", [], |r| {
            r.get::<_, String>(0)
        })
        .inspect_err(note_sql)
        .ok()?
        .parse()
        .inspect_err(|error| {
            crate::env::warn_once("the catalog", error);
        })
        .ok()
    }

    #[cfg(test)]
    fn last_full_sync() -> Option<i64> {
        Reader::open()?.last_full_sync()
    }

    /// Stamp "a full discovery just completed" (the [`Reader::last_full_sync`] the staleness backstop reads).
    fn stamp_full_sync_in(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync',?1)",
            params![chrono::Utc::now().timestamp().to_string()],
        )?;
        Ok(())
    }

    use crate::source::Payload as _;

    /// Whether `h` never records a sub-agent forest (`Adapter::forest` is always empty).
    fn has_zero_subagent_forest(h: Harness) -> bool {
        match h {
            Harness::Claude
            | Harness::Codex
            | Harness::Hermes
            | Harness::Devin
            | Harness::Antigravity
            | Harness::Kimi
            | Harness::KimiCode => false,
            Harness::Grok
            | Harness::OpenCode
            | Harness::Gemini
            | Harness::OpenClaw
            | Harness::Cursor
            | Harness::Qwen
            | Harness::LmStudio
            | Harness::Cline
            | Harness::Roo
            | Harness::Continue
            | Harness::Goose
            | Harness::Zed
            | Harness::ChatGptExport
            | Harness::ClaudeExport
            | Harness::OpenSession => true,
        }
    }

    /// Capture sub-agent count and an exact filesystem guard for `r` on demand (when a query
    /// touches `agents` or `has:subagents`), without holding a SQLite writer lock.
    pub(crate) fn populate_subagent_facts(r: &SessionRef, facts: &mut SessionQueryFacts) {
        let mut guard = crate::source::ReadSet::default();
        if has_zero_subagent_forest(r.harness) {
            facts.subagent_count = Some(0);
            facts.subagent_guard = guard;
            return;
        }
        match r.harness {
            Harness::Claude => {
                let stem = r.path.with_extension("");
                let subagents = stem.join("subagents");
                let wf_subagents = subagents.join("workflows");
                let workflows = stem.join("workflows");
                for p in [&stem, &subagents, &wf_subagents, &workflows] {
                    guard.record(p);
                }
                if let Ok(entries) = std::fs::read_dir(&wf_subagents) {
                    for entry in entries.flatten() {
                        guard.record(&entry.path());
                    }
                }
            }
            Harness::Codex => {
                let ledger_path = crate::lanes::ledger_path();
                guard.record(&ledger_path);
                let home = crate::env::Env::current()
                    .cv_home()
                    .unwrap_or_else(|| PathBuf::from("."));
                let state_db = home.join("state.db");
                guard.record(&state_db);
                for sidecar in crate::source::sqlite_sidecars(&state_db) {
                    guard.record(&sidecar);
                }
                let ledger = crate::lanes::ledger_index();
                let mut visited = HashSet::from([r.id.clone()]);
                let mut queue = std::collections::VecDeque::from([r.id.clone()]);
                while let Some(sid) = queue.pop_front() {
                    for &at in ledger.rows_of(&sid) {
                        let row = &ledger.rows[at];
                        if let Some(child_path) = row.agent_transcript_path.as_deref() {
                            guard.record(child_path);
                        }
                        if !row.agent_id.is_empty() && visited.insert(row.agent_id.clone()) {
                            queue.push_back(row.agent_id.clone());
                        }
                    }
                }
            }
            Harness::Hermes | Harness::Devin => {
                guard.record(&r.path);
                for sidecar in crate::source::sqlite_sidecars(&r.path) {
                    guard.record(&sidecar);
                }
            }
            Harness::Antigravity => {
                let sys_dir = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == ".system_generated"));
                let sub_dir = sys_dir
                    .map(|sys| sys.join("subagents"))
                    .unwrap_or_else(|| r.path.join(".system_generated").join("subagents"));
                let brain_root = sys_dir
                    .and_then(|sys| sys.parent())
                    .and_then(|brain| brain.parent())
                    .or_else(|| r.path.parent());
                guard.record(&sub_dir);
                if let Ok(entries) = std::fs::read_dir(&sub_dir) {
                    for entry in entries.flatten() {
                        let desc_path = entry.path();
                        guard.record(&desc_path);
                        if desc_path.extension().and_then(|x| x.to_str()) != Some("json") {
                            continue;
                        }
                        let cid = std::fs::read_to_string(&desc_path)
                            .ok()
                            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                            .and_then(|val| {
                                val.get("conversationId")
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned)
                            })
                            .or_else(|| desc_path.file_stem().and_then(|s| s.to_str()).map(str::to_owned));
                        if let (Some(root), Some(cid)) = (brain_root, cid) {
                            if !cid.is_empty() && !cid.contains('/') && !cid.contains('\\') && cid != ".." {
                                let child_logs = root.join(&cid).join(".system_generated").join("logs");
                                guard.record(&child_logs);
                                for name in ["transcript_full.jsonl", "transcript.jsonl", "overview.txt"] {
                                    guard.record(&child_logs.join(name));
                                }
                            }
                        }
                    }
                }
            }
            Harness::Kimi => {
                guard.record(&r.path);
                let sub_dir = r.path.join("subagents");
                guard.record(&sub_dir);
                if let Ok(entries) = std::fs::read_dir(&sub_dir) {
                    for entry in entries.flatten() {
                        guard.record(&entry.path());
                    }
                }
            }
            Harness::KimiCode => {
                let sdir = r
                    .path
                    .ancestors()
                    .find(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("session_"))
                    })
                    .unwrap_or(&r.path);
                guard.record(&sdir.join("state.json"));
                let agents_dir = sdir.join("agents");
                guard.record(&agents_dir);
                if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                    for entry in entries.flatten() {
                        guard.record(&entry.path());
                    }
                }
            }
            Harness::Grok
            | Harness::OpenCode
            | Harness::Gemini
            | Harness::OpenClaw
            | Harness::Cursor
            | Harness::Qwen
            | Harness::LmStudio
            | Harness::Cline
            | Harness::Roo
            | Harness::Continue
            | Harness::Goose
            | Harness::Zed
            | Harness::ChatGptExport
            | Harness::ClaudeExport
            | Harness::OpenSession => unreachable!("handled above"),
        }
        match crate::subagent_tree_filtered_of(r, crate::SubagentDemand::Sessions, None, |_, _| true) {
            Ok(agents) => {
                for agent in &agents {
                    guard.record(&agent.session.path);
                }
                if let Ok(count) = u32::try_from(agents.len()) {
                    if guard.fresh(&mut crate::source::Probe::default()) {
                        facts.subagent_count = Some(count);
                        facts.subagent_guard = guard;
                        return;
                    }
                }
                facts.subagent_count = None;
                facts.subagent_guard = crate::source::ReadSet::unrecorded();
            }
            Err(_) => {
                facts.subagent_count = None;
                facts.subagent_guard = crate::source::ReadSet::unrecorded();
            }
        }
    }

    pub(crate) fn note_counted_with_source(
        r: &SessionRef,
        source: &crate::source::Baseline,
        turns: usize,
        facts: Option<SessionQueryFacts>,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        note_counts_with_source(r, source, Some(turns), facts, counts);
    }

    /// Record counts against the dependencies captured before their complete source pass.
    pub fn note_affordances_with_source(
        r: &SessionRef,
        source: &crate::source::Baseline,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        note_counts_with_source(r, source, None, None, counts);
    }

    fn note_counts_with_source(
        r: &SessionRef,
        source: &crate::source::Baseline,
        turns: Option<usize>,
        facts: Option<SessionQueryFacts>,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        if crate::env::frozen() {
            return;
        }
        if !db_path().is_some_and(|p| p.exists()) {
            return;
        }
        if !crate::source::unchanged(r, source) {
            return;
        }
        let Some(mut conn) = open() else { return };
        note_counts_in(&mut conn, r, source, turns, facts, counts);
    }

    /// The caller verified the captured source before publishing on this existing connection.
    /// The output and its independent freshness baseline still commit in one transaction.
    pub(crate) fn note_counts_in(
        conn: &mut Connection,
        r: &SessionRef,
        source: &crate::source::Baseline,
        turns: Option<usize>,
        mut facts: Option<SessionQueryFacts>,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        if crate::env::frozen() {
            return;
        }
        let Ok(turns) = turns.map(u32::try_from).transpose() else {
            return;
        };
        let path = r.path.to_string_lossy();
        // Validate the old record before taking a writer lock. Recheck it after BEGIN IMMEDIATE:
        // a racing publication cannot lend its provenance to our skip decision.
        let stored = count_record_in(conn, r);
        let old_decoded = stored.as_deref().and_then(crate::source::Record::<Counted>::decode);
        if let Some(ref mut new_f) = facts {
            if new_f.subagent_count.is_none() {
                if has_zero_subagent_forest(r.harness) {
                    new_f.subagent_count = Some(0);
                    new_f.subagent_guard = crate::source::ReadSet::default();
                } else if let Some(old_f) = old_decoded.as_ref().and_then(|old| old.payload.facts.as_ref()) {
                    if old_f.subagent_count.is_some()
                        && old_f.subagent_guard.fresh(&mut crate::source::Probe::default())
                    {
                        new_f.subagent_count = old_f.subagent_count;
                        new_f.subagent_guard = old_f.subagent_guard.clone();
                    }
                }
            }
        }
        let keep = old_decoded.as_ref().is_some_and(|old| {
            turns.is_none_or(|turns| old.payload.turns == Some(turns))
                && facts
                    .as_ref()
                    .is_none_or(|new_f| old.payload.facts.as_ref() == Some(new_f))
                && old.admit_for(r, source, &mut crate::source::Probe::default()).is_some()
        });
        if keep && turns.is_none() && facts.is_none() {
            return;
        }
        if facts.is_none() {
            if let Some(old_f) = old_decoded.and_then(|old| old.payload.facts) {
                // Preserve enriched facts across affordance-only publications on an unchanged source.
                facts = Some(old_f);
            }
        }
        let counted = Counted {
            counts: counts(),
            turns,
            facts,
        };
        let Ok(tx) = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .inspect_err(note_sql)
        else {
            return;
        };
        let current = count_record_in(&tx, r);
        // Partial affordance publication must not replace a concurrent enriched complete pass.
        if turns.is_none() && current != stored {
            return;
        }
        let mut probe = crate::source::Probe::default();
        let mut write = || -> anyhow::Result<()> {
            if !(keep && current == stored) {
                crate::source::publish_in(&tx, Counted::TAG, r, source, &counted, &mut probe)?;
                anyhow::ensure!(
                    count_record_in(&tx, r).is_some(),
                    "source changed before count publication"
                );
            } else {
                anyhow::ensure!(
                    crate::source::unchanged_with(r, &probe.primary_path(r), source, &mut probe),
                    "source changed before count refill"
                );
            }
            if let Some(turns) = turns {
                tx.execute(
                    "UPDATE sessions SET message_count=?1 WHERE harness=?2 AND id=?3 AND path=?4 AND message_count IS NULL",
                    params![turns as i64, r.harness.as_str(), r.id, path],
                )?;
            }
            if let Some(f) = &counted.facts {
                tx.execute(
                    "UPDATE sessions SET model=?1, git_branch=?2, git_remote=?3, compactions=?4, subagent_count=?5 WHERE harness=?6 AND id=?7 AND path=?8",
                    params![
                        f.primary_model,
                        f.git_branch,
                        f.git_remote,
                        i64::from(f.compactions),
                        f.subagent_count.map(i64::from),
                        r.harness.as_str(),
                        r.id,
                        path,
                    ],
                )?;
            }
            Ok(())
        };
        if write().is_ok() {
            if let Err(error) = tx.commit() {
                note_sql(&error);
            }
        }
    }

    /// Read persisted indexed query columns (`model`, `git_branch`, `git_remote`, `compactions`,
    /// `subagent_count`) from `catalog.db`'s `sessions` row for `r`.
    pub fn session_indexed_fields(r: &SessionRef) -> Option<IndexedSessionFields> {
        let conn = open()?;
        conn.query_row(
            "SELECT model, git_branch, git_remote, compactions, subagent_count FROM sessions WHERE harness=?1 AND id=?2 AND path=?3",
            params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            |row| {
                Ok(IndexedSessionFields {
                    model: row.get(0)?,
                    git_branch: row.get(1)?,
                    git_remote: row.get(2)?,
                    compactions: row.get::<_, Option<i64>>(3)?.and_then(|n| u32::try_from(n).ok()),
                    subagent_count: row.get::<_, Option<i64>>(4)?.and_then(|n| u32::try_from(n).ok()),
                })
            },
        )
        .optional()
        .inspect_err(note_sql)
        .ok()
        .flatten()
    }

    /// `r`'s counts record, its bytes as stored: compared across BEGIN IMMEDIATE, decoded by
    /// the caller. No catalog transaction spans a source read.
    pub(super) fn count_record_in(conn: &Connection, r: &SessionRef) -> Option<Vec<u8>> {
        conn.prepare_cached(
            "SELECT record FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        )
        .inspect_err(note_sql)
        .ok()?
        .query_row(
            params![Counted::TAG, r.harness.as_str(), r.id, r.path.to_string_lossy()],
            |row| row.get(0),
        )
        .inspect_err(note_sql)
        .ok()
    }

    /// Apply observations collected by existing discovery reads, under the publication's writer lock.
    fn publish_writer_versions(tx: &rusqlite::Transaction, h: Harness) -> rusqlite::Result<()> {
        for (id, path, version) in crate::discover_cache::read::Context::current().writer_versions(h) {
            let old: Option<String> = tx
                .query_row(
                    "SELECT writer_version FROM sessions WHERE harness=?1 AND id=?2 AND path=?3",
                    params![h.as_str(), id, path],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let mut newest = old;
            crate::writer_version::observe(&mut newest, version.as_deref());
            tx.execute(
                "UPDATE sessions SET writer_version=?4 WHERE harness=?1 AND id=?2 AND path=?3 AND writer_version IS NOT ?4",
                params![h.as_str(), id, path, newest.as_deref().unwrap_or("")],
            )?;
        }
        Ok(())
    }

    /// Replace each listed harness's skipped sources with the ones its discovery just reported
    /// ([`crate::harness::Discovered::skipped`]). A healthy fleet with none recorded writes nothing.
    pub(super) fn record_skipped_in(
        conn: &mut Connection,
        rows: &[(Harness, &[crate::Skipped])],
    ) -> rusqlite::Result<()> {
        if crate::env::frozen() {
            return Ok(());
        }
        let recorded: i64 = conn.query_row("SELECT COUNT(*) FROM skipped_sources", [], |r| r.get(0))?;
        if recorded == 0 && rows.iter().all(|(_, skipped)| skipped.is_empty()) {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (h, skipped) in rows {
            tx.execute("DELETE FROM skipped_sources WHERE harness=?1", [h.as_str()])?;
            for s in *skipped {
                tx.execute(
                    "INSERT OR REPLACE INTO skipped_sources(harness,path,why) VALUES(?1,?2,?3)",
                    params![h.as_str(), s.path.to_string_lossy(), s.why],
                )?;
            }
        }
        tx.commit()
    }

    /// The sources the last discovery of each harness found but could not open, by harness and
    /// path: catalog knowledge only, as `cv doctor` shows it. Empty on a catalog without any.
    pub fn skipped_sources() -> Vec<(Harness, crate::Skipped)> {
        let Some(conn) = open() else { return Vec::new() };
        let Ok(mut stmt) = conn.prepare("SELECT harness,path,why FROM skipped_sources ORDER BY harness,path") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(h, path, why)| {
                Some((
                    Harness::parse(&h)?,
                    crate::Skipped {
                        path: PathBuf::from(path),
                        why,
                    },
                ))
            })
            .collect()
    }

    /// Catalog knowledge only: no transcript probes or backfill. NULL/empty/unsupported versions
    /// count as unknown; the maximum is historical evidence, not a claim of current source validity.
    pub fn writer_versions() -> Vec<crate::writer_version::Observed> {
        let mut result = crate::writer_version::empty_observations();
        let Some(conn) = open() else { return result };
        // The roster's names, spelled into the statement: harness names are fixed identifiers.
        let names: Vec<String> = result.iter().map(|o| format!("'{}'", o.harness)).collect();
        let sql = format!(
            "SELECT harness,writer_version FROM sessions WHERE harness IN ({})",
            names.join(",")
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return result;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        }) else {
            return result;
        };
        for (h, version) in rows.flatten() {
            if let Some(observed) = result.iter_mut().find(|o| o.harness == h) {
                observed.sessions += 1;
                if !crate::writer_version::observe(&mut observed.newest, version.as_deref()) {
                    observed.unknown += 1;
                }
            }
        }
        result
    }

    /// Every recorded affordance count, loaded once for a sweep (`cv ls -q has:plan`, `cv index`).
    pub struct AffordanceTable {
        records: crate::source::Table<Counted>,
    }

    impl AffordanceTable {
        pub fn load() -> AffordanceTable {
            AffordanceTable {
                records: crate::source::Table::load(Counted::TAG),
            }
        }

        /// `r`'s counts when a full read by this view recorded them and they are still fresh;
        /// `None` ("not counted") otherwise. Stats the session once.
        pub fn counts(&self, r: &SessionRef) -> Option<crate::affordance::Counts> {
            self.counts_with_source(r, &crate::source::Probe::default().snapshot(r))
        }

        /// Use a shared operation's dependency probe rather than stat'ing shared files again.
        pub fn counts_with_source(
            &self,
            r: &SessionRef,
            source: &crate::source::Baseline,
        ) -> Option<crate::affordance::Counts> {
            self.counts_with_probe(r, source, &mut crate::source::Probe::default())
        }

        pub fn counts_with_probe(
            &self,
            r: &SessionRef,
            source: &crate::source::Baseline,
            probe: &mut crate::source::Probe,
        ) -> Option<crate::affordance::Counts> {
            Some(self.records.admit(r, source, probe)?.counts.clone())
        }

        pub(crate) fn counted_record_with_probe(
            &self,
            r: &SessionRef,
            source: &crate::source::Baseline,
            probe: &mut crate::source::Probe,
        ) -> Option<(crate::source::Baseline, Counted)> {
            let record = self.records.get(r)?;
            let payload = record.admit_for(r, source, probe)?.clone();
            Some((record.baseline.clone(), payload))
        }
    }

    /// UTF-8 lexicographic successor of a literal prefix for SQLite's binary text index.
    /// A suffix of maximal scalars carries into the preceding scalar; empty/all-maximal
    /// prefixes need no upper bound. Skip the surrogate gap, which Rust strings cannot contain.
    fn prefix_end(prefix: &str) -> Option<String> {
        for (offset, ch) in prefix.char_indices().rev() {
            if ch != char::MAX {
                let next = if ch == '\u{d7ff}' {
                    '\u{e000}'
                } else {
                    char::from_u32(ch as u32 + 1)?
                };
                let mut end = prefix[..offset].to_owned();
                end.push(next);
                return Some(end);
            }
        }
        None
    }

    pub(super) fn exact_session_in(
        conn: &Connection,
        id: &str,
        harness: Option<Harness>,
        overrides: [bool; 2],
    ) -> Option<SessionRef> {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {REF_COLUMNS} FROM sessions WHERE id=?1 AND (?2 IS NULL OR harness=?2) ORDER BY rowid"
            ))
            .inspect_err(note_sql)
            .ok()?;
        let mut rows = stmt
            .query_map(params![id, harness.map(Harness::as_str)], |row| {
                row_to_ref_projected(row, overrides)
            })
            .inspect_err(note_sql)
            .ok()?;
        // Unknown newer harness rows are skipped, as in lookup; the first known exact copy wins.
        rows.find_map(|row| row.inspect_err(note_sql).ok().flatten())
    }

    /// All catalog rows whose id equals `id` or starts with it, optionally constrained to one
    /// harness. Order: exact-id matches first. Empty on any error or a cold catalog.
    pub fn lookup(id: &str, harness: Option<Harness>) -> Vec<SessionRef> {
        let Some(conn) = open() else { return Vec::new() };
        let upper = prefix_end(id);
        // Binary prefix ranges use idx_sessions_id; LIKE's case-folded comparison would scan
        // the entire table despite that index, and disagree with literal Rust ID matching.
        let range = if upper.is_some() {
            "id>=?1 AND id<?2"
        } else {
            "id>=?1 AND ?2 IS NULL"
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {REF_COLUMNS} FROM sessions
             WHERE {range} AND (?3 IS NULL OR harness=?3)
             ORDER BY (id=?1) DESC",
        )) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![id, upper, harness.map(Harness::as_str)], row_to_ref);
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten().flatten().filter(|r| r.id.starts_with(id)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::UNIX_EPOCH;

        fn rootless(h: Harness) -> &'static dyn crate::harness::Adapter {
            crate::harness::rootless(h).unwrap()
        }

        fn db_ref(harness: Harness, path: PathBuf) -> SessionRef {
            SessionRef {
                harness,
                id: "session".into(),
                path,
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: None,
            }
        }

        #[test]
        fn locked_wal_switch_keeps_a_usable_catalog_and_retries() {
            let world = crate::test_world("catalog-journal-lock");
            let path = world.cv_home.join("journal.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE fixture(value); INSERT INTO fixture VALUES(1);")
                .unwrap();
            let reader = Connection::open(&path).unwrap();
            reader.execute_batch("BEGIN; SELECT * FROM fixture;").unwrap();
            configure_journal(&conn).unwrap();
            assert!(!crate::env::warn_once("catalog WAL mode", "already reported"));
            assert_eq!(
                conn.query_row("SELECT value FROM fixture", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            drop(reader);
            configure_journal(&conn).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "wal"
            );
        }

        #[test]
        fn failed_schema_creation_never_stamps_or_drops_the_previous_catalog() {
            let world = crate::test_world("catalog-create-error");
            let path = world.base.join("catalog.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version=3; CREATE TABLE preserved(x); INSERT INTO preserved VALUES (7); CREATE VIEW sessions AS SELECT 1;").unwrap();
            assert!(open_writable(&path).is_err());
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                3
            );
            assert_eq!(
                conn.query_row("SELECT x FROM preserved", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                7
            );
            conn.execute_batch("DROP VIEW sessions").unwrap();
            let rebuilt = open_writable(&path).unwrap();
            assert_eq!(
                rebuilt
                    .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                SCHEMA
            );
            assert!(rebuilt.prepare("SELECT * FROM sessions").is_ok());
        }

        #[test]
        fn search_rows_share_one_exact_catalog_reader_and_keep_first_copy_metadata() {
            let w = crate::test_world("search-row-catalog-owner");
            let first_path = w.base.join("first");
            let newer_path = w.base.join("newer");
            std::fs::write(&first_path, "old").unwrap();
            std::fs::write(&newer_path, "newer").unwrap();
            let mut first = db_ref(Harness::Claude, first_path.clone());
            first.id = "hit".into();
            first.message_count = Some(7);
            let mut newer = first.clone();
            newer.path = newer_path;
            newer.message_count = Some(9);
            newer.updated_at = chrono::DateTime::from_timestamp(1_700_000_000, 0);
            let mut prefix = first.clone();
            prefix.id = "hit-longer".into();
            sync(&[first.clone(), newer, prefix]);
            let count = || OPEN_COUNT.with(std::cell::Cell::get);
            let before = count();
            let context = crate::rows::SearchRowContext::new();
            assert_eq!(count(), before, "empty response contexts do not initialize a catalog");
            let make = |id, harness| crate::rows::SearchRow {
                id,
                harness,
                cwd: Some("/indexed"),
                title: Some("indexed title"),
                created_at: None,
                updated_at: None,
                score: Some(1.0),
                snippet: "snippet",
                agent_id: None,
                parent_id: None,
                workflow: None,
            };
            let row = make("hit", "claude").to_json_with(&context);
            assert_eq!(row["path"], first_path.to_string_lossy().as_ref());
            assert_eq!(row["message_count"], 7);
            assert_eq!(row["size_bytes"], 3);
            assert_eq!(row["title"], "indexed title");
            assert_eq!(row["cwd"], "/indexed");
            assert_eq!(make("hit", "claude").to_json_with(&context), row);
            assert!(
                make("hi", "claude").to_json_with(&context)["path"].is_null(),
                "a prefix is not an indexed exact hit"
            );
            assert!(make("hit", "codex").to_json_with(&context)["path"].is_null());
            assert_eq!(
                count() - before,
                1,
                "one catalog/schema initialization for all shaped hits"
            );
            assert_eq!(
                lookup("hit", Some(Harness::Claude))
                    .into_iter()
                    .find(|r| r.id == "hit")
                    .unwrap()
                    .path,
                first.path
            );
            assert_eq!(
                row.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
                crate::rows::SEARCH_ROW_KEYS
            );
        }

        #[test]
        fn search_projection_omits_overridden_hints_but_keeps_fallback_and_invalid_copy_policy() {
            let w = crate::test_world("search-row-projection");
            let mut first = db_ref(Harness::Claude, w.base.join("first"));
            first.title = Some("catalog custom title".repeat(4096));
            first.cwd = Some(PathBuf::from("/catalog/path".repeat(4096)));
            first.created_at = chrono::DateTime::from_timestamp(1_700_000_000, 0);
            let mut second = first.clone();
            second.path = w.base.join("second");
            second.message_count = Some(9);
            sync(&[first.clone(), second]);
            let conn = open().unwrap();
            let projected = exact_session_in(&conn, "session", Some(Harness::Claude), [true, true]).unwrap();
            assert!(
                projected.title.is_none() && projected.cwd.is_none(),
                "overridden hints were owned"
            );
            let full = exact_session_in(&conn, "session", Some(Harness::Claude), [false, false]).unwrap();
            assert_eq!(full.title, first.title);
            assert_eq!(full.cwd, first.cwd);
            let row = crate::rows::SearchRow {
                id: "session",
                harness: "claude",
                title: Some("index title"),
                cwd: Some("/index"),
                created_at: Some(i64::MAX),
                updated_at: Some(0),
                score: None,
                snippet: "snippet",
                agent_id: None,
                parent_id: None,
                workflow: None,
            };
            let output = row.to_json_with(&crate::rows::SearchRowContext::new());
            assert_eq!(output["title"], "index title");
            assert_eq!(output["cwd"], "/index");
            assert_eq!(
                output["created_at"],
                first.created_at.unwrap().to_rfc3339(),
                "invalid index date lost fallback"
            );
            assert_eq!(output["updated_at"], "1970-01-01T00:00:00+00:00");
            for expression in ["X'ff'", "CAST(X'ff' AS TEXT)"] {
                conn.execute(
                    &format!("UPDATE sessions SET title={expression} WHERE path=?1"),
                    [first.path.to_string_lossy()],
                )
                .unwrap();
                let expected = lookup("session", Some(Harness::Claude))
                    .into_iter()
                    .find(|r| r.id == "session")
                    .unwrap();
                let actual = exact_session_in(&conn, "session", Some(Harness::Claude), [true, true]).unwrap();
                assert_eq!(
                    actual.path, expected.path,
                    "overridden invalid hint changed copy acceptance"
                );
                assert_eq!(actual.message_count, Some(9));
            }
        }

        #[test]
        fn primary_inventory_omits_metadata_and_preserves_source_selection() {
            let w = crate::test_world("catalog-primary-inventory");
            let refs: Vec<_> = crate::harness::all()
                .into_iter()
                .enumerate()
                .map(|(i, a)| {
                    let mut r = db_ref(a.harness(), w.base.join(format!("sessions/{i}/session")));
                    r.id = "session".into();
                    r.title = Some("unused title".repeat(1024));
                    r.cwd = Some(w.base.join("unused-cwd"));
                    r.message_count = Some(123);
                    r
                })
                .collect();
            sync(&refs);
            let reader = Reader::open().unwrap();
            let projected = reader.primary_sessions();
            assert_eq!(projected.len(), refs.len());
            for (full, lean) in refs.iter().zip(&projected) {
                assert_eq!(
                    (&full.id, full.harness, &full.path),
                    (&lean.id, lean.harness, &lean.path)
                );
                assert!(lean.title.is_none() && lean.cwd.is_none());
                assert_eq!(lean.message_count, None);
                let a = crate::harness::for_harness(full.harness).unwrap();
                assert_eq!(a.resume_source(full), a.resume_source(lean));
                let conn = open().unwrap();
                let scoped = primary_refs_in(&conn, Some(full.harness));
                assert_eq!(scoped.len(), 1);
                assert_eq!(scoped[0].path, full.path);
            }
        }

        #[test]
        fn primary_inventory_preserves_empty_unknown_and_error_fallbacks() {
            let w = crate::test_world("catalog-primary-errors");
            let conn = open().unwrap();
            assert!(primary_refs_in(&conn, None).is_empty());
            let r = db_ref(Harness::Claude, w.base.join("session.jsonl"));
            sync(std::slice::from_ref(&r));
            // Newer binaries' unknown rows are skipped before decoding identity columns,
            // matching the full row reader even if their column representation differs.
            conn.execute(
                "INSERT INTO sessions(harness,id,path) VALUES('future-harness',X'01',X'02')",
                [],
            )
            .unwrap();
            let projected = primary_refs_in(&conn, None);
            let full = all_sessions_in(&conn).unwrap();
            assert_eq!(projected.len(), 1);
            assert_eq!(projected[0].path, full[0].path);
            assert!(primary_refs_in(&conn, Some(Harness::Codex)).is_empty());
            conn.execute_batch("DROP TABLE sessions").unwrap();
            assert!(primary_refs_in(&conn, None).is_empty());
            assert!(primary_refs_in(&conn, Some(Harness::Claude)).is_empty());
        }

        /// A catalog of another schema (an older cv's, stamped below [`SCHEMA`]) is dropped whole
        /// and rebuilt at open, with the rediscovery notice; a second open keeps what it holds.
        #[test]
        fn another_schema_is_rebuilt_once() {
            let w = crate::test_world("catalog-other-schema");
            let path = w.cv_home.join("old-catalog.db");
            Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "PRAGMA user_version = 3;
                     CREATE TABLE sessions(harness TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL,
                         PRIMARY KEY(harness, id, path));
                     INSERT INTO sessions VALUES('claude', 'old', '/old.jsonl');
                     CREATE TABLE event_sync(harness TEXT);
                     CREATE TABLE catalog_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO catalog_meta VALUES('discovery_version', '1');",
                )
                .unwrap();
            let conn = open_writable(&path).expect("the rebuild opens");
            let count = |conn: &Connection, sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
            assert_eq!(count(&conn, "PRAGMA user_version"), SCHEMA);
            assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 0);
            assert_eq!(
                count(&conn, "SELECT count(*) FROM sqlite_master WHERE name='event_sync'"),
                0
            );
            assert_eq!(
                count(
                    &conn,
                    "SELECT count(*) FROM catalog_meta WHERE key='rediscovery_notice'"
                ),
                1
            );
            conn.execute(
                "INSERT INTO sessions(harness, id, path, created_ns) VALUES('claude', 'new', '/new.jsonl', 1)",
                [],
            )
            .unwrap();
            drop(conn);
            let conn = open_writable(&path).unwrap();
            assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 1);
        }

        #[test]
        fn skipped_sources_replace_per_harness_and_a_healthy_fleet_writes_nothing() {
            let _w = crate::test_world("catalog-skipped-sources");
            let skip = |p: &str| crate::Skipped {
                path: PathBuf::from(p),
                why: format!("{p} is locked"),
            };
            let mut conn = open().unwrap();
            record_skipped_in(&mut conn, &[(Harness::Claude, &[]), (Harness::Codex, &[])]).unwrap();
            let changes = conn.total_changes();
            record_skipped_in(&mut conn, &[(Harness::Claude, &[])]).unwrap();
            assert_eq!(
                conn.total_changes(),
                changes,
                "nothing recorded, nothing reported: no write"
            );
            record_skipped_in(
                &mut conn,
                &[
                    (Harness::Claude, &[skip("/a"), skip("/b")]),
                    (Harness::Codex, &[skip("/c")]),
                ],
            )
            .unwrap();
            assert_eq!(skipped_sources().len(), 3);
            // A later discovery of one harness replaces its rows and leaves the other's.
            record_skipped_in(&mut conn, &[(Harness::Claude, &[skip("/b")])]).unwrap();
            assert_eq!(
                skipped_sources(),
                [(Harness::Claude, skip("/b")), (Harness::Codex, skip("/c"))]
            );
            record_skipped_in(&mut conn, &[(Harness::Claude, &[]), (Harness::Codex, &[])]).unwrap();
            assert!(skipped_sources().is_empty());
        }

        #[test]
        fn a_skipped_source_write_that_fails_keeps_the_old_list_and_warns_once() {
            let _w = crate::test_world("catalog-skipped-sources-refused");
            let skip = |p: &str| crate::Skipped {
                path: PathBuf::from(p),
                why: format!("{p} is locked"),
            };
            let mut conn = open().unwrap();
            record_skipped_in(&mut conn, &[(Harness::Claude, &[skip("/old")])]).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER refuse_skipped BEFORE INSERT ON skipped_sources BEGIN SELECT RAISE(ABORT,'refused'); END;",
            )
            .unwrap();
            let mut reader = super::super::reader::Reader::open().unwrap();
            reader.record_skipped(&[(Harness::Claude, &[skip("/new")])]);
            assert_eq!(skipped_sources(), [(Harness::Claude, skip("/old"))]);
            assert!(
                !crate::env::warn_once(super::super::reader::SKIPPED_LIST, "already reported"),
                "the refused write was reported"
            );
        }

        #[test]
        fn lookup_matches_literal_case_sensitive_prefixes() {
            let w = crate::test_world("catalog-literal-prefix");
            let ids = [
                "Alpha",
                "alpha",
                "a_%\\literal",
                "aXYliteral",
                "λ_session",
                "λ_other",
                "a\0one",
                "a\0two",
                "a\u{d7ff}tail",
                "a\u{e000}tail",
                "a\u{10ffff}tail",
                "\u{10ffff}tail",
                "\u{10ffff}\u{10ffff}tail",
            ];
            let refs: Vec<_> = ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| {
                    let mut r = db_ref(Harness::Claude, w.base.join(format!("{i}.jsonl")));
                    r.id = id.into();
                    r
                })
                .collect();
            let mut foreign = refs[0].clone();
            foreign.harness = Harness::Codex;
            foreign.path = w.base.join("foreign.jsonl");
            sync(&refs);
            sync(&[foreign]);
            for prefix in [
                "A",
                "a",
                "a_%\\",
                "λ_",
                "λ_s",
                "",
                "a\0",
                "a\u{d7ff}",
                "a\u{e000}",
                "a\u{10ffff}",
                "\u{10ffff}",
                "\u{10ffff}\u{10ffff}",
                "missing",
            ] {
                let actual: std::collections::BTreeSet<_> = lookup(prefix, Some(Harness::Claude))
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                let expected = refs
                    .iter()
                    .filter(|r| r.id.starts_with(prefix))
                    .map(|r| r.id.clone())
                    .collect();
                assert_eq!(actual, expected, "prefix {prefix:?}");
            }
            assert_eq!(lookup("Alpha", None).len(), 2);
        }

        #[test]
        fn reader_warm_sessions_initialize_catalog_once() {
            let w = crate::test_world("reader-warm");
            let path = w.home.join(".claude/projects/-reader/alpha.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"alpha\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n").unwrap();
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for dir in path
                .parent()
                .unwrap()
                .ancestors()
                .take_while(|dir| dir.starts_with(&w.home))
            {
                std::fs::File::open(dir)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let discovered = crate::discover_all();
            assert_eq!(discovered.len(), 1);
            let count = || OPEN_COUNT.with(std::cell::Cell::get);
            let before = count();
            assert!(Reader::open().unwrap().last_full_sync().is_some());
            assert!(Reader::open().unwrap().probe_stale().unwrap().is_empty());
            let legacy = Reader::open().unwrap().all_sessions().unwrap();
            assert_eq!(count() - before, 3, "prior three-open read recipe");
            let before = count();
            let (current, full) = crate::sessions_impl();
            assert!(!full, "verified catalog read must stay on the warm route");
            assert_eq!(
                count() - before,
                1,
                "one owned operation connection/schema initialization"
            );
            assert_eq!(
                serde_json::to_value(current).unwrap(),
                serde_json::to_value(legacy).unwrap()
            );
            // Initial discovery has already warmed the process-local metadata cache. A first-use
            // scan remains demand-lazy and may independently open it; SQLite-only refreshes must
            // not bulk-load unrelated transcript metadata just to save an initialization.
            use std::io::Write;
            writeln!(std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
                "{{\"type\":\"user\",\"uuid\":\"u2\",\"sessionId\":\"alpha\",\"message\":{{\"role\":\"user\",\"content\":\"second\"}}}}"
            ).unwrap();
            let before = count();
            let (refreshed, full) = crate::sessions_impl();
            assert!(!full, "one changed primary uses scoped refresh");
            assert_eq!(
                count() - before,
                1,
                "capture, publication and cache persist reuse the operation connection"
            );
            assert_eq!(
                refreshed[0].message_count,
                Some(2),
                "a Claude listing recounts on growth"
            );
            crate::stream::stream_counted(
                &crate::harness::claude::Claude::detect(),
                &refreshed[0],
                &crate::stream::Demand::FACTS.with_spans(),
                &mut crate::stream::Discard,
                crate::stream::Pass::RECORD,
            )
            .unwrap();
            let exact = crate::sessions_impl().0;
            assert_eq!(exact[0].message_count, Some(2));
            assert_eq!(
                serde_json::to_value(&exact).unwrap(),
                serde_json::to_value(crate::discover_all()).unwrap()
            );
        }

        /// A home the process may not write (a sandbox's deny-write rule) still serves its catalog:
        /// the listing comes off the existing rows on the warm route, never a full discovery.
        #[cfg(unix)]
        #[test]
        fn read_only_home_serves_the_catalog_without_discovery() {
            use std::os::unix::fs::PermissionsExt;
            let w = crate::test_world("reader-read-only");
            let path = w.home.join(".claude/projects/-reader/alpha.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"alpha\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n").unwrap();
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for dir in path
                .parent()
                .unwrap()
                .ancestors()
                .take_while(|dir| dir.starts_with(&w.home))
            {
                std::fs::File::open(dir)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let discovered = crate::discover_all();
            // A live connection retains WAL coordination. A mode=ro reader must observe it,
            // including commits that an immutable connection would silently ignore.
            let writer = open_db().unwrap();
            let set_mode = |mode: u32| {
                for entry in std::fs::read_dir(&w.cv_home).unwrap() {
                    let path = entry.unwrap().path();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o666)).unwrap();
                }
                std::fs::set_permissions(&w.cv_home, std::fs::Permissions::from_mode(mode)).unwrap();
            };
            set_mode(0o555);
            let read = std::panic::catch_unwind(crate::sessions_impl);
            set_mode(0o755);
            let (listed, full) = read.unwrap();
            assert!(!full, "a read-only catalog with WAL coordination serves the warm route");
            assert_eq!(
                serde_json::to_value(listed).unwrap(),
                serde_json::to_value(&discovered).unwrap()
            );
            drop(writer);
            set_mode(0o555);
            let read = std::panic::catch_unwind(crate::sessions_impl);
            set_mode(0o755);
            let (listed, full) = read.unwrap();
            assert!(!full, "a closed read-only checkpoint also serves the warm route");
            assert_eq!(
                serde_json::to_value(listed).unwrap(),
                serde_json::to_value(&discovered).unwrap()
            );
        }

        #[test]
        fn checkpoint_reader_rejects_wal_and_journal_contents() {
            let w = crate::test_world("reader-checkpoint");
            let r = db_ref(Harness::Claude, w.base.join("recorded"));
            sync(std::slice::from_ref(&r));
            let path = db_path().unwrap();
            assert!(open_checkpoint(&path).is_some());
            let writer = open_db().unwrap();
            writer
                .execute_batch("PRAGMA wal_autocheckpoint=0; UPDATE sessions SET writer_version='9.0.0';")
                .unwrap();
            assert!(open_checkpoint(&path).is_none(), "never ignore committed WAL frames");
            let reader = open_read_only(&path).unwrap();
            let observed: String = reader
                .query_row("SELECT writer_version FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(observed, "9.0.0");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // A copied WAL without shared memory cannot coordinate in a read-only
                // directory. Returning the older main-file row here would lose evidence.
                let copy = w.base.join("uncoordinated");
                std::fs::create_dir(&copy).unwrap();
                let copied = copy.join("catalog.db");
                for name in ["catalog.db", "catalog.db-wal"] {
                    std::fs::copy(w.cv_home.join(name), copy.join(name)).unwrap();
                    std::fs::set_permissions(copy.join(name), std::fs::Permissions::from_mode(0o444)).unwrap();
                }
                if crate::publish::tests::read_only(&copy, true) {
                    let rejected = open_read_only(&copied).is_none();
                    crate::publish::tests::read_only(&copy, false);
                    assert!(rejected, "an unreadable live WAL must not serve the checkpoint");
                }
            }
            drop(reader);
            drop(writer);
            assert!(open_checkpoint(&path).is_some());
            for suffix in ["-wal", "-journal"] {
                let sidecar = w.cv_home.join(format!("catalog.db{suffix}"));
                std::fs::write(&sidecar, b"unreadable recovery data").unwrap();
                assert!(
                    open_checkpoint(&path).is_none(),
                    "{suffix} must not fall back to immutable"
                );
                std::fs::remove_file(sidecar).unwrap();
            }
        }

        #[test]
        fn reader_operations_follow_catalog_switch_and_replacement() {
            let w = crate::test_world("reader-path");
            let a = db_ref(Harness::Claude, w.base.join("first"));
            sync(std::slice::from_ref(&a));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, a.path);
            std::fs::rename(w.cv_home.join("catalog.db"), w.cv_home.join("old-catalog.db")).unwrap();
            let b = db_ref(Harness::Claude, w.base.join("replacement"));
            sync(std::slice::from_ref(&b));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, b.path);
            let next = w.base.join("next-catalog");
            w.scope.set("CLUSTERVISION_HOME", &next);
            let c = db_ref(Harness::Claude, w.base.join("switched"));
            sync(std::slice::from_ref(&c));
            assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].path, c.path);
        }

        #[test]
        fn reader_statement_errors_remain_fallbacks_without_held_transactions() {
            let _w = crate::test_world("reader-errors");
            let reader = Reader::open().unwrap();
            let conn = open().unwrap();
            // Another writer can change schema while this operation owner is idle.
            conn.execute_batch("BEGIN IMMEDIATE; DROP TABLE watched; COMMIT;")
                .unwrap();
            assert!(reader.probe_stale().is_none());
            conn.execute_batch("DROP TABLE catalog_meta;").unwrap();
            assert!(reader.last_full_sync().is_none());
            conn.execute_batch("DROP TABLE sessions;").unwrap();
            assert!(reader.all_sessions().is_none());
        }

        #[test]
        fn reader_preload_does_not_capture_primary_identity_early() {
            let w = crate::test_world("reader-before-source");
            let path = w.base.join("session.jsonl");
            std::fs::write(&path, "before").unwrap();
            let r = db_ref(Harness::Claude, path.clone());
            sync(std::slice::from_ref(&r));
            let known = Reader::open().unwrap().all_sessions().unwrap();
            std::fs::write(&path, "loaded").unwrap();
            let adapter = crate::harness::for_harness(Harness::Claude).unwrap();
            let captured = capture_primary_reads_from(adapter.as_ref(), &known);
            assert!(captured[&path].fresh(&mut crate::source::Probe::default()));
            std::fs::write(&path, "racing").unwrap();
            assert!(!captured[&path].fresh(&mut crate::source::Probe::default()));
        }

        #[test]
        fn publication_waits_for_existing_writer_before_comparing_and_replacing() {
            use std::sync::mpsc;
            use std::time::Duration;
            let mut failures = Vec::new();
            for full in [false, true] {
                let w = crate::test_world("publication-held-writer");
                let r = db_ref(Harness::Claude, w.base.join("source"));
                let mut publishing = open().unwrap();
                let mut blocking = open().unwrap();
                let lock = blocking
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                lock.execute("INSERT INTO catalog_meta(key,value) VALUES('held-writer','1')", [])
                    .unwrap();
                let (started, entered) = mpsc::channel();
                let (finished, result) = mpsc::channel();
                let worker = std::thread::spawn(crate::env::carry(move || {
                    let refs = [r];
                    started.send(()).unwrap();
                    if full {
                        let before = PrimaryReads::default();
                        let rows = [(Harness::Claude, refs.as_slice(), Vec::new(), &before)];
                        publish_all_in(&mut publishing, &rows, &[SourceRows::default()]);
                    } else {
                        replace_prepared_in(&mut publishing, Harness::Claude, &refs, &[], &SourceRows::default());
                    }
                    finished
                        .send(matches_sessions_in(&publishing, refs.iter()).unwrap())
                        .unwrap();
                }));
                entered.recv_timeout(Duration::from_secs(10)).unwrap();
                // A publication must wait for the held writer rather than abandon its rows
                // when upgrading a deferred read snapshot. Release even on negative controls.
                let early = result.recv_timeout(Duration::from_millis(100)).ok();
                lock.commit().unwrap();
                let stored = early.unwrap_or_else(|| result.recv_timeout(Duration::from_secs(10)).unwrap());
                worker.join().unwrap();
                if early.is_some() || !stored {
                    failures.push((full, early, stored));
                }
            }
            assert!(failures.is_empty(), "publication did not wait and commit: {failures:?}");
        }

        #[test]
        fn publication_preserves_full_tie_order_and_failed_harness_rows() {
            let w = crate::test_world("publication-order");
            let mut a = db_ref(Harness::Claude, w.base.join("a"));
            a.id = "a".into();
            let mut b = db_ref(Harness::Gemini, w.base.join("b"));
            b.id = "b".into();
            let mut failed = db_ref(Harness::Devin, w.base.join("failed"));
            failed.id = "failed".into();
            sync(&[a.clone(), b.clone()]);
            assert!(matches_sessions([&a, &b].into_iter()));
            assert!(!matches_sessions([&b, &a].into_iter()), "ties retain adapter order");
            let before = PrimaryReads::default();
            let publish = |bs: &[SessionRef]| {
                publish_all(&[
                    (Harness::Claude, std::slice::from_ref(&a), Vec::new(), &before),
                    (Harness::Gemini, bs, Vec::new(), &before),
                ])
            };
            b.message_count = Some(1);
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&a, &b].into_iter()));
            // Intervening additive publication after enumeration must defeat the skip.
            let mut intervening = b.clone();
            intervening.message_count = Some(909);
            sync(&[intervening]);
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&a, &b].into_iter()));
            sync(std::slice::from_ref(&failed));
            publish(std::slice::from_ref(&b));
            // A harness whose discovery failed keeps its rows, in its registration place.
            assert!(matches_sessions([&a, &b, &failed].into_iter()));
            let mut new = b.clone();
            new.id = "new".into();
            publish(&[b.clone(), new]);
            publish(std::slice::from_ref(&b));
            assert!(matches_sessions([&a, &b, &failed].into_iter()));
            let conn = open().unwrap();
            conn.execute(
                "INSERT INTO sessions(harness,id,path,message_count) VALUES('future-harness','future','future',0)",
                [],
            )
            .unwrap();
            publish(std::slice::from_ref(&b));
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM sessions WHERE harness='future-harness'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
        }

        #[test]
        fn one_changed_session_writes_one_row() {
            let w = crate::test_world("publication-one-change");
            let session = |i: usize| {
                let mut r = db_ref(Harness::Hermes, w.base.join("state.db"));
                r.id = format!("s{i:04}");
                r
            };
            let mut refs: Vec<SessionRef> = (0..200).map(session).collect();
            let before = PrimaryReads::default();
            let publish = |refs: &[SessionRef]| {
                order::WRITES.with(|n| n.set(0));
                replace_harness(Harness::Hermes, refs, &[], &before);
                assert!(matches_sessions(refs.iter()), "rowid order is the listing's");
                order::WRITES.with(std::cell::Cell::get)
            };
            assert_eq!(publish(&refs), 200);
            assert_eq!(publish(&refs), 0, "an unchanged harness writes nothing");
            refs[100].message_count = Some(7);
            assert_eq!(publish(&refs), 1, "one session's metadata");
            refs.insert(0, session(1000));
            assert_eq!(
                publish(&refs),
                1,
                "a new session listed first (Hermes lists newest first)"
            );
            refs.push(session(1001));
            assert_eq!(publish(&refs), 1, "a new session listed last");
            refs.insert(77, session(1002));
            assert_eq!(publish(&refs), 1, "a new session listed in the middle");
            refs.remove(150);
            assert_eq!(publish(&refs), 1, "a vanished session");
            // Sessions landing in one gap, over and over: the gap widens instead of the harness
            // being rewritten, so the total stays far below one rewrite per insert.
            let mut total = 0;
            for i in 0..64 {
                refs.insert(50, session(2000 + i));
                total += publish(&refs);
            }
            assert!(total < 64 * 8, "{total} writes for 64 inserts into one gap");
        }

        #[test]
        fn a_scoped_publication_keeps_its_harness_in_registration_order() {
            let w = crate::test_world("publication-scoped-order");
            let mut a = db_ref(Harness::Claude, w.base.join("a"));
            a.id = "a".into();
            let mut b = db_ref(Harness::Gemini, w.base.join("b"));
            b.id = "b".into();
            let before = PrimaryReads::default();
            publish_all(&[
                (Harness::Claude, std::slice::from_ref(&a), Vec::new(), &before),
                (Harness::Gemini, std::slice::from_ref(&b), Vec::new(), &before),
            ]);
            a.message_count = Some(3);
            replace_harness(Harness::Claude, std::slice::from_ref(&a), &[], &before);
            assert!(
                matches_sessions([&a, &b].into_iter()),
                "a refreshed harness keeps its place"
            );
        }

        #[test]
        fn publication_failure_rolls_back_rows_guards_and_full_sync_stamp() {
            let w = crate::test_world("publication-abort");
            let path = w.base.join("session.jsonl");
            std::fs::write(&path, "before").unwrap();
            let a = db_ref(Harness::Claude, path.clone());
            let mut before = PrimaryReads::default();
            let mut reads = crate::source::ReadSet::default();
            reads.record(&path);
            before.insert(path.clone(), reads.clone());
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&a),
                vec![(path.clone(), Watched::Absent)],
                &before,
            )]);
            let conn = open().unwrap();
            conn.execute("UPDATE catalog_meta SET value='1' WHERE key='last_full_sync'", [])
                .unwrap();
            let guard: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            let mut changed = a.clone();
            changed.message_count = Some(2);
            std::fs::write(&path, "after!").unwrap();
            let mut newer = crate::source::ReadSet::default();
            newer.record(&path);
            before.insert(path.clone(), newer);
            // A changed row is rewritten in place, so the publication aborts on its update.
            conn.execute_batch("CREATE TRIGGER reject_publication BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT,'publication'); END;").unwrap();
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&changed),
                vec![(path.clone(), Watched::Unknown)],
                &before,
            )]);
            assert!(matches_sessions(std::iter::once(&a)));
            assert_eq!(last_full_sync(), Some(1));
            let unchanged: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(unchanged, guard);
            assert_eq!(
                conn.query_row("SELECT id FROM watched WHERE path=?1", [path.to_string_lossy()], |r| {
                    r.get::<_, Option<Vec<u8>>>(0)
                })
                .unwrap(),
                Some(Vec::new())
            );
            conn.execute_batch("DROP TRIGGER reject_publication;").unwrap();
            // Restoring original captured proof must not bless the intervening file rewrite.
            before.insert(path.clone(), reads.clone());
            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&changed),
                vec![(path.clone(), Watched::Unknown)],
                &before,
            )]);
            let published: String = conn
                .query_row(
                    "SELECT reads FROM primary_sources WHERE path=?1",
                    [path.to_string_lossy()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<crate::source::ReadSet>(&published).unwrap(),
                reads
            );
            assert!(!reads.fresh(&mut crate::source::Probe::default()));
        }

        #[test]
        fn arbitrary_opencode_database_names_watch_native_wal() {
            let w = crate::test_world("catalog-opencode-arbitrary-db");
            let path = w.base.join("custom-store");
            std::fs::write(&path, b"SQLite format 3\0native database bytes").unwrap();
            let r = db_ref(Harness::OpenCode, path.clone());
            let watches = watches_for(
                rootless(r.harness),
                std::slice::from_ref(&w.base),
                std::slice::from_ref(&r),
            );
            assert!(watches.iter().any(|(p, _)| p == &w.base.join("custom-store-wal")));
            let legacy = w.base.join("ses_old.json");
            std::fs::write(&legacy, "{}").unwrap();
            let r = db_ref(Harness::OpenCode, legacy);
            assert!(!watches_for(rootless(r.harness), &[], &[r])
                .iter()
                .any(|(p, _)| p == &w.base.join("ses_old.json-wal")));
        }

        #[test]
        fn watches_track_cursor_cli_metadata_without_database_changes() {
            let world = crate::test_world("catalog-cursor-cli-meta");
            let path = world.base.join("store.db");
            std::fs::write(&path, []).unwrap();
            let mut r = db_ref(Harness::Cursor, path);
            r.id = "cli:session".into();
            let metadata = world.base.join("meta.json");
            let watches = watches_for(rootless(Harness::Cursor), std::slice::from_ref(&world.base), &[r]);
            let absent = watches.iter().find(|(path, _)| path == &metadata).unwrap().1;
            assert_eq!(absent, Watched::Absent);
            assert!(watch_is_fresh(&metadata, absent));
            std::fs::write(&metadata, r#"{"cwd":"/scratch/project"}"#).unwrap();
            assert!(!watch_is_fresh(&metadata, absent));
            let present = Watched::Is(FileId::at(&metadata).unwrap());
            assert!(watch_is_fresh(&metadata, present));
            std::fs::File::open(&metadata)
                .unwrap()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(123))
                .unwrap();
            assert!(!watch_is_fresh(&metadata, present));
            std::fs::remove_file(&metadata).unwrap();
            assert!(!watch_is_fresh(&metadata, present));
        }

        #[test]
        fn watches_track_absent_wal_and_its_appearance_and_removal() {
            let w = crate::test_world("catalog-absent-wal");
            let path = w.base.join("openclaw-agent.sqlite");
            std::fs::write(&path, []).unwrap();
            let wal = w.base.join("openclaw-agent.sqlite-wal");
            let r = db_ref(Harness::OpenClaw, path.clone());
            let watches = watches_for(rootless(Harness::OpenClaw), std::slice::from_ref(&w.base), &[r]);
            assert!(
                watches.iter().any(|(p, _)| *p == path),
                "OpenClaw SQLite is file-watched"
            );
            let absent = watches.iter().find(|(p, _)| *p == wal).unwrap().1;
            assert_eq!(absent, Watched::Absent);
            assert!(watch_is_fresh(&wal, absent));

            std::fs::write(&wal, "wal").unwrap();
            assert!(
                !watch_is_fresh(&wal, absent),
                "WAL appearance invalidates its absent watch"
            );
            let present = Watched::Is(FileId::at(&wal).unwrap());
            assert!(watch_is_fresh(&wal, present));
            std::fs::remove_file(&wal).unwrap();
            assert!(
                !watch_is_fresh(&wal, present),
                "WAL removal invalidates its present watch"
            );
            assert!(!watch_is_fresh(&wal, Watched::Unknown), "unknown is not stable absence");

            let jsonl = db_ref(Harness::OpenClaw, w.base.join("legacy.jsonl"));
            let watches = watches_for(rootless(Harness::OpenClaw), &[], std::slice::from_ref(&jsonl));
            assert!(
                !watches.iter().any(|(p, _)| *p == jsonl.path),
                "legacy JSONL keeps the bounded file probe"
            );
        }

        #[test]
        fn wal_only_write_invalidates_database_watch_set() {
            let w = crate::test_world("catalog-live-wal");
            let path = w.base.join("sessions.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE messages(text TEXT);",
            )
            .unwrap();
            let wal = w.base.join("sessions.db-wal");
            // Age the files so the test exercises actual WAL freshness, not WATCH_FUDGE.
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for p in [&path, &wal, &w.base] {
                std::fs::File::open(p)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let db_sig = crate::source::FileId::at(&path);
            let r = db_ref(Harness::OpenCode, path.clone());
            let watches = watches_for(rootless(Harness::OpenCode), std::slice::from_ref(&w.base), &[r]);
            assert!(watches.iter().all(|(p, stamp)| watch_is_fresh(p, *stamp)));
            conn.execute("INSERT INTO messages VALUES ('new message')", []).unwrap();
            assert_eq!(crate::source::FileId::at(&path), db_sig, "write stayed in WAL");
            let changed: Vec<_> = watches.iter().filter(|(p, stamp)| !watch_is_fresh(p, *stamp)).collect();
            assert_eq!(changed.len(), 1);
            assert_eq!(
                changed[0].0, wal,
                "only WAL invalidates the otherwise unchanged watch set"
            );
        }

        #[test]
        fn db_path_honors_clustervision_cache_without_touching_durable_home() {
            let w = crate::test_world("catalog-cache-override");
            let durable = w.base.join("durable-home");
            let cache = w.base.join("dedicated-cache");
            w.scope.set("CLUSTERVISION_HOME", &durable);
            w.scope.set("CLUSTERVISION_CACHE", &cache);
            let resolved = db_path().expect("db_path");
            assert_eq!(resolved, cache.join("catalog.db"));
            assert!(
                !durable.join("catalog.db").exists(),
                "durable CLUSTERVISION_HOME must never gain a split-brain catalog.db when CLUSTERVISION_CACHE is set"
            );
        }

        /// A counts record of another version or consumer, or with a malformed known count,
        /// does not decode: a kind an older view did not know must not read as absent.
        #[test]
        fn counts_records_of_another_version_or_shape_are_not_counted() {
            use crate::source::{Baseline, Record};
            let mut counts = crate::affordance::Counts::new();
            counts.insert(crate::affordance::Kind::Approval, u32::MAX);
            let record = Record::new(
                Baseline {
                    primary: None,
                    stamp: None,
                    rows: None,
                },
                Counted {
                    counts,
                    turns: Some(3),
                    facts: None,
                },
            );
            let bytes = record.encode();
            assert_eq!(Record::<Counted>::decode(&bytes).unwrap().payload, record.payload);
            let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            for (key, value) in [
                ("version", serde_json::json!(crate::source::VERSION - 1)),
                ("tag", serde_json::json!("events")),
            ] {
                let mut other = wire.clone();
                other[key] = value;
                assert!(
                    Record::<Counted>::decode(&serde_json::to_vec(&other).unwrap()).is_none(),
                    "{key}"
                );
            }
            for value in [
                serde_json::json!(4294967296_u64),
                serde_json::json!(-1),
                serde_json::json!("1"),
                serde_json::Value::Null,
            ] {
                wire["payload"]["counts"]["approval"] = value;
                assert!(Record::<Counted>::decode(&serde_json::to_vec(&wire).unwrap()).is_none());
            }
        }

        #[test]
        fn scoped_codec_bump_evicts_only_target_harness_rows() {
            let w = crate::test_world("catalog-scoped-codec-bump");
            let claude_path = w.base.join("claude.jsonl");
            let codex_path = w.base.join("codex.jsonl");
            std::fs::write(&claude_path, "{}\n").unwrap();
            std::fs::write(&codex_path, "{}\n").unwrap();

            let mut claude_ref = db_ref(Harness::Claude, claude_path.clone());
            claude_ref.id = "claude-1".into();
            let mut codex_ref = db_ref(Harness::Codex, codex_path.clone());
            codex_ref.id = "codex-1".into();

            let mut claude_reads = PrimaryReads::default();
            let mut cr = crate::source::ReadSet::default();
            cr.record(&claude_path);
            claude_reads.insert(claude_path.clone(), cr);

            let mut codex_reads = PrimaryReads::default();
            let mut xr = crate::source::ReadSet::default();
            xr.record(&codex_path);
            codex_reads.insert(codex_path.clone(), xr);

            publish_all(&[
                (
                    Harness::Claude,
                    std::slice::from_ref(&claude_ref),
                    vec![(claude_path.clone(), Watched::Is(FileId::at(&claude_path).unwrap()))],
                    &claude_reads,
                ),
                (
                    Harness::Codex,
                    std::slice::from_ref(&codex_ref),
                    vec![(codex_path.clone(), Watched::Is(FileId::at(&codex_path).unwrap()))],
                    &codex_reads,
                ),
            ]);
            let sync_before = last_full_sync();
            assert!(sync_before.is_some());

            // Seed scan_cache + counts for both harnesses so we prove cross-table isolation.
            {
                let mut conn = open().unwrap();
                for (h, r, p) in [
                    (Harness::Claude, &claude_ref, &claude_path),
                    (Harness::Codex, &codex_ref, &codex_path),
                ] {
                    conn.execute(
                        "INSERT INTO scan_cache(path, mtime_ns, size, refs, interpretation_version) VALUES(?1, 1, 3, '[]', ?2)",
                        params![p.to_string_lossy(), crate::discover_cache::INTERPRETATION_VERSION],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT INTO resume_marks(harness, id, path, offset, hash, version, state) VALUES(?1, ?2, ?3, 0, 0, 'v', '{}')",
                        params![h.as_str(), r.id, p.to_string_lossy()],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT INTO skipped_sources(harness, path, why) VALUES(?1, ?2, 'err')",
                        params![h.as_str(), p.to_string_lossy()],
                    )
                    .unwrap();
                    let snap = crate::source::Probe::default()
                        .snapshot(r)
                        .with_reads(&crate::source::ReadSet::default());
                    note_counts_in(&mut conn, r, &snap, Some(2), None, crate::affordance::Counts::new);
                }
            }

            with_scoped_versions(&[(Harness::Codex, crate::source::VERSION + 1)], None, || {
                let conn = open().unwrap();
                let count_for = |table: &str, h: Harness| -> i64 {
                    conn.query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE harness=?1"),
                        [h.as_str()],
                        |r| r.get(0),
                    )
                    .unwrap()
                };
                for table in [
                    "sessions",
                    "watched",
                    "primary_sources",
                    "resume_marks",
                    "skipped_sources",
                    "source_dependencies",
                ] {
                    assert_eq!(count_for(table, Harness::Codex), 0, "Codex {table} must be evicted");
                    assert_eq!(count_for(table, Harness::Claude), 1, "Claude {table} must survive");
                }
                let codex_scan: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM scan_cache WHERE path=?1",
                        [codex_path.to_string_lossy()],
                        |r| r.get(0),
                    )
                    .unwrap();
                let claude_scan: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM scan_cache WHERE path=?1",
                        [claude_path.to_string_lossy()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(codex_scan, 0, "Codex scan_cache row must be evicted");
                assert_eq!(claude_scan, 1, "Claude scan_cache row must survive");
                assert_eq!(last_full_sync(), sync_before, "last_full_sync must be preserved");
            });
        }

        #[test]
        fn scoped_events_bump_evicts_events_only_preserving_sessions_and_counts() {
            let w = crate::test_world("catalog-scoped-events-bump");
            let claude_path = w.base.join("claude.jsonl");
            std::fs::write(&claude_path, "{}\n").unwrap();
            let mut claude_ref = db_ref(Harness::Claude, claude_path.clone());
            claude_ref.id = "claude-ev".into();
            let mut reads = PrimaryReads::default();
            let mut cr = crate::source::ReadSet::default();
            cr.record(&claude_path);
            reads.insert(claude_path.clone(), cr);

            publish_all(&[(
                Harness::Claude,
                std::slice::from_ref(&claude_ref),
                vec![(claude_path.clone(), Watched::Is(FileId::at(&claude_path).unwrap()))],
                &reads,
            )]);
            {
                let mut conn = open().unwrap();
                let snap = crate::source::Probe::default()
                    .snapshot(&claude_ref)
                    .with_reads(&crate::source::ReadSet::default());
                note_counts_in(
                    &mut conn,
                    &claude_ref,
                    &snap,
                    Some(4),
                    None,
                    crate::affordance::Counts::new,
                );
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS event_sessions(id INTEGER PRIMARY KEY, harness TEXT NOT NULL, session_id TEXT NOT NULL);
                     CREATE TABLE IF NOT EXISTS events(session INTEGER NOT NULL, msg_idx INTEGER NOT NULL, kind TEXT NOT NULL);",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO event_sessions(id, harness, session_id) VALUES(1, 'claude', 'claude-ev')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO events(session, msg_idx, kind) VALUES(1, 0, 'compaction')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO source_dependencies(consumer, harness, id, path, record) VALUES('events', 'claude', 'claude-ev', ?1, X'00')",
                    [claude_path.to_string_lossy()],
                )
                .unwrap();
            }

            with_scoped_versions(&[], Some(crate::source::VERSION + 1), || {
                let conn = open().unwrap();
                let sessions_n: i64 = conn
                    .query_row("SELECT COUNT(*) FROM sessions WHERE harness='claude'", [], |r| r.get(0))
                    .unwrap();
                let counts_n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM source_dependencies WHERE consumer='counts' AND harness='claude'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let ev_deps_n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM source_dependencies WHERE consumer='events'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let ev_sess_n: i64 = conn
                    .query_row("SELECT COUNT(*) FROM event_sessions", [], |r| r.get(0))
                    .unwrap();
                let ev_rows_n: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
                assert_eq!(sessions_n, 1, "sessions must survive events_v bump");
                assert_eq!(counts_n, 1, "counts records must survive events_v bump");
                assert_eq!(ev_deps_n, 0, "events dependencies must be evicted");
                assert_eq!(ev_sess_n, 0, "event_sessions must be evicted");
                assert_eq!(ev_rows_n, 0, "events rows must be evicted");
            });
        }

        #[test]
        fn session_indexed_fields_persist_and_survive_catalog_republish() {
            let w = crate::test_world("catalog-indexed-fields");
            let p1 = w.base.join("s1.jsonl");
            let p2 = w.base.join("s2.jsonl");
            std::fs::write(&p1, "{}\n").unwrap();
            std::fs::write(&p2, "{}\n").unwrap();

            let mut r1 = db_ref(Harness::Gemini, p1.clone());
            r1.id = "g1".into();
            let mut r2 = db_ref(Harness::Gemini, p2.clone());
            r2.id = "g2".into();

            let mut reads = PrimaryReads::default();
            for p in [&p1, &p2] {
                let mut rs = crate::source::ReadSet::default();
                rs.record(p);
                reads.insert(p.clone(), rs);
            }
            publish_all(&[(Harness::Gemini, &[r1.clone(), r2.clone()], Vec::new(), &reads)]);

            let snap = crate::source::Probe::default()
                .snapshot(&r1)
                .with_reads(&crate::source::ReadSet::default());
            let facts = SessionQueryFacts {
                primary_model: Some("gemini-2.5-pro".into()),
                models: vec!["gemini-2.5-flash".into(), "gemini-2.5-pro".into()],
                git_branch: Some("feat/spine".into()),
                git_remote: Some("git@github.com:org/cv.git".into()),
                compactions: 3,
                subagent_count: None,
                subagent_guard: crate::source::ReadSet::default(),
            };
            note_counted_with_source(&r1, &snap, 5, Some(facts), crate::affordance::Counts::new);

            let expected = IndexedSessionFields {
                model: Some("gemini-2.5-pro".into()),
                git_branch: Some("feat/spine".into()),
                git_remote: Some("git@github.com:org/cv.git".into()),
                compactions: Some(3),
                subagent_count: Some(0),
            };
            assert_eq!(session_indexed_fields(&r1), Some(expected.clone()));

            // Republish with a prepended session to exercise order::publish / gap moves.
            let p0 = w.base.join("s0.jsonl");
            std::fs::write(&p0, "{}\n").unwrap();
            let mut r0 = db_ref(Harness::Gemini, p0.clone());
            r0.id = "g0".into();
            let mut rs0 = crate::source::ReadSet::default();
            rs0.record(&p0);
            reads.insert(p0, rs0);
            replace_harness(Harness::Gemini, &[r0, r1.clone(), r2], &[], &reads);
            assert_eq!(
                session_indexed_fields(&r1),
                Some(expected),
                "indexed query columns must survive catalog order republish"
            );
        }

        #[test]
        fn codex_and_antigravity_subagent_guards_invalidate_when_pending_child_transcript_appears() {
            // 1. Codex: `SubagentStart` in `lanes.jsonl` names `child_path` BEFORE `child_path`
            //    exists on disk. Later creating `child_path` without modifying `lanes.jsonl` or
            //    `parent.jsonl` MUST invalidate `subagent_guard` and refresh `subagent_count` 0 -> 1!
            let w_codex = crate::test_world("codex-pending-child-guard");
            let parent_path = w_codex.base.join("codex-parent.jsonl");
            std::fs::write(
                &parent_path,
                serde_json::json!({"type": "session_meta", "payload": {"id": "codex-parent"}}).to_string() + "\n",
            )
            .unwrap();
            let child_path = w_codex.base.join("codex-child.jsonl");
            assert!(!child_path.exists());

            crate::lanes::append_lane_event(&crate::lanes::LaneEvent {
                ts: chrono::Utc::now(),
                event: crate::lanes::LaneEventKind::Start,
                session_id: "codex-parent".into(),
                agent_id: "codex-child".into(),
                agent_type: Some("worker".into()),
                transcript_path: Some(parent_path.clone()),
                agent_transcript_path: Some(child_path.clone()),
                cwd: None,
                last_text: None,
            })
            .unwrap();

            let codex_ref = SessionRef::new("codex-parent", Harness::Codex, &parent_path);
            let mut codex_facts = SessionQueryFacts::default();
            populate_subagent_facts(&codex_ref, &mut codex_facts);
            assert_eq!(codex_facts.subagent_count, Some(0));
            assert!(codex_facts.subagent_guard.fresh(&mut crate::source::Probe::default()));

            // Now Codex flushes `child_path` WITHOUT touching `parent_path` or `lanes.jsonl`!
            std::fs::write(
                &child_path,
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": "codex-child", "parent_thread_id": "codex-parent"}
                })
                .to_string()
                    + "\n",
            )
            .unwrap();
            assert!(
                !codex_facts.subagent_guard.fresh(&mut crate::source::Probe::default()),
                "Codex subagent_guard must invalidate when pending child transcript appears"
            );
            populate_subagent_facts(&codex_ref, &mut codex_facts);
            assert_eq!(codex_facts.subagent_count, Some(1));
            assert!(codex_facts.subagent_guard.fresh(&mut crate::source::Probe::default()));

            // 2. Antigravity: `<parent_brain>/.system_generated/subagents/<cid>.json` exists BEFORE
            //    `<app_data_dir>/brain/<cid>/.system_generated/logs/transcript.jsonl` is created.
            //    Creating the child transcript later without touching parent or `<cid>.json` MUST
            //    invalidate `subagent_guard` and refresh `subagent_count` 0 -> 1!
            let w_agy = crate::test_world("agy-pending-child-guard");
            let app_data = w_agy.base.join("antigravity");
            let parent_logs = app_data
                .join("brain")
                .join("agy-parent")
                .join(".system_generated")
                .join("logs");
            let parent_sub_dir = app_data
                .join("brain")
                .join("agy-parent")
                .join(".system_generated")
                .join("subagents");
            std::fs::create_dir_all(&parent_logs).unwrap();
            std::fs::create_dir_all(&parent_sub_dir).unwrap();

            let agy_parent_log = parent_logs.join("transcript.jsonl");
            std::fs::write(
                &agy_parent_log,
                serde_json::json!({
                    "step_index": 0,
                    "source": "USER_EXPLICIT",
                    "type": "USER_INPUT",
                    "content": "hello parent"
                })
                .to_string()
                    + "\n",
            )
            .unwrap();
            std::fs::write(
                parent_sub_dir.join("agy-child.json"),
                serde_json::json!({"conversationId": "agy-child"}).to_string(),
            )
            .unwrap();

            let agy_ref = SessionRef::new("agy-parent", Harness::Antigravity, &agy_parent_log);
            let mut agy_facts = SessionQueryFacts::default();
            populate_subagent_facts(&agy_ref, &mut agy_facts);
            assert_eq!(agy_facts.subagent_count, Some(0));
            assert!(agy_facts.subagent_guard.fresh(&mut crate::source::Probe::default()));

            // Create child transcript under `<app_data>/brain/agy-child/.system_generated/logs/`
            // WITHOUT touching `agy_parent_log` or `agy-child.json`!
            let child_logs = app_data
                .join("brain")
                .join("agy-child")
                .join(".system_generated")
                .join("logs");
            std::fs::create_dir_all(&child_logs).unwrap();
            std::fs::write(
                child_logs.join("transcript.jsonl"),
                serde_json::json!({
                    "step_index": 0,
                    "source": "USER_EXPLICIT",
                    "type": "USER_INPUT",
                    "content": "hello child"
                })
                .to_string()
                    + "\n",
            )
            .unwrap();

            assert!(
                !agy_facts.subagent_guard.fresh(&mut crate::source::Probe::default()),
                "Antigravity subagent_guard must invalidate when pending child transcript appears"
            );
            populate_subagent_facts(&agy_ref, &mut agy_facts);
            assert_eq!(agy_facts.subagent_count, Some(1));
            assert!(agy_facts.subagent_guard.fresh(&mut crate::source::Probe::default()));
        }
    }
}

#[cfg(feature = "sqlite")]
pub(crate) use imp::{all_sessions, db_path, open_at, open_db, watches_for};

#[cfg(feature = "sqlite")]
pub use imp::{
    events_codec_version, harness_codec_version, lookup, note_affordances_with_source, session_indexed_fields,
    skipped_sources, sync, writer_versions, AffordanceTable,
};

#[cfg(feature = "sqlite")]
pub(crate) use imp::{note_counted_with_source, populate_subagent_facts};

#[cfg(not(feature = "sqlite"))]
pub(crate) fn populate_subagent_facts(_r: &SessionRef, _facts: &mut SessionQueryFacts) {}

#[cfg(all(feature = "sqlite", test))]
pub(crate) use imp::replace_harness;

#[cfg(all(feature = "sqlite", test))]
pub(crate) fn last_full_sync() -> Option<i64> {
    Reader::open()?.last_full_sync()
}

/// No-op fallbacks when sqlite is disabled — `find`/`sessions` simply always take the scan path.
#[cfg(not(feature = "sqlite"))]
pub fn sync(_refs: &[SessionRef]) {}
#[cfg(not(feature = "sqlite"))]
pub fn harness_codec_version(_h: Harness) -> u32 {
    crate::source::VERSION
}
#[cfg(not(feature = "sqlite"))]
pub fn events_codec_version() -> u32 {
    crate::source::VERSION
}
#[cfg(not(feature = "sqlite"))]
pub fn session_indexed_fields(_r: &SessionRef) -> Option<IndexedSessionFields> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub fn note_affordances_with_source(
    _r: &SessionRef,
    _source: &crate::source::Baseline,
    _counts: impl FnOnce() -> crate::affordance::Counts,
) {
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn note_counted_with_source(
    _r: &SessionRef,
    _source: &crate::source::Baseline,
    _turns: usize,
    _facts: Option<SessionQueryFacts>,
    _counts: impl FnOnce() -> crate::affordance::Counts,
) {
}
/// Without a catalog nothing is ever counted: every `has:<kind>` parses.
#[cfg(not(feature = "sqlite"))]
pub struct AffordanceTable;
#[cfg(not(feature = "sqlite"))]
impl AffordanceTable {
    pub fn load() -> AffordanceTable {
        AffordanceTable
    }
    pub fn counts(&self, _r: &SessionRef) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_source(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Baseline,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_probe(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Baseline,
        _probe: &mut crate::source::Probe,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub(crate) fn counted_record_with_probe(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Baseline,
        _probe: &mut crate::source::Probe,
    ) -> Option<(crate::source::Baseline, Counted)> {
        None
    }
}

impl AffordanceTable {
    /// Whether `r` has no fresh counts (a sweep re-reads it to fill them): without the catalog,
    /// nothing is ever counted.
    pub fn stale(&self, r: &SessionRef) -> bool {
        self.counts(r).is_none()
    }
}
#[cfg(not(feature = "sqlite"))]
pub fn skipped_sources() -> Vec<(Harness, crate::Skipped)> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub fn writer_versions() -> Vec<crate::writer_version::Observed> {
    crate::writer_version::empty_observations()
}
#[cfg(not(feature = "sqlite"))]
pub fn lookup(_id: &str, _harness: Option<Harness>) -> Vec<SessionRef> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn watches_for(
    _adapter: &dyn crate::harness::Adapter,
    _roots: &[std::path::PathBuf],
    _refs: &[SessionRef],
) -> Vec<(std::path::PathBuf, Watched)> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
    None
}

#[cfg(all(test, feature = "sqlite"))]
mod counts_tests;
#[cfg(all(test, feature = "sqlite"))]
mod writer_version_tests;
