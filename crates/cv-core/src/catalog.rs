//! A persistent SQLite catalog of discovered sessions: `id → (harness, path, metadata)` — and,
//! since v0.9.12, the **hot read path** for listing the fleet.
//!
//! [`crate::find`] used to resolve one session id by calling `discover()` on **every** adapter and
//! scanning the whole fleet — O(all sessions across all harnesses) for a single `cv show <id>`. The
//! catalog turns that into an indexed lookup: [`crate::discover_all`] replaces every harness's rows
//! here, and `find` consults it first (O(1)/O(log n)), falling back to a full scan only on a miss
//! (which then re-warms the catalog).
//!
//! The same rows now also serve [`crate::sessions`], which answers `cv ls`/`timeline`/`stats`
//! without touching the fleet at all. Correctness rests on the **freshness probe**
//! ([`probe_stale`]): every full discovery records, per harness, the directories that directly
//! contain session files (plus all ancestors up to the harness root) and — for the SQLite-backed
//! harnesses — the database files themselves, each with its mtime at scan time (the `watched`
//! table). The probe re-stats those few hundred paths in milliseconds; any new/changed/vanished
//! entry marks its harness for re-discovery.
//!
//! Sibling tables: `scan_cache` is [`crate::discover_cache`]'s per-file `(mtime, size) → refs`
//! store (it owns the reads/writes; the DDL lives here so every opener sees one schema), and
//! `catalog_meta` holds the `last_full_sync` stamp behind the bounded-staleness backstop.
//!
//! Best-effort throughout: any sqlite error is swallowed and the caller falls back to scanning, so
//! correctness never depends on the catalog (a stale row is filtered by `path.exists()` at lookup).
//! Behind the `sqlite` feature (default-on); no-op fallbacks keep the scan path working without it.

use crate::ir::{Harness, SessionRef};
#[cfg(not(feature = "sqlite"))]
use std::path::Path;
#[cfg(feature = "sqlite")]
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection, Row};
    use std::collections::{BTreeSet, HashSet};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// How many of the most-recently-updated session files the probe re-stats to catch in-place
    /// appends (POSIX directory mtimes don't change when a file inside merely grows).
    const TOP_K: usize = 50;

    /// A directory (or db file) whose recorded mtime is younger than this at record time is stamped
    /// `mtime_ns = 0` ("always stale"): a session file may have landed *between* discovery
    /// enumerating the directory and us stat'ing it, so the next probe re-discovers that harness
    /// once and re-records a trustworthy mtime. Self-quiescing: an actively-written harness pays
    /// one cheap re-discovery per probe until it goes idle for this long.
    const WATCH_FUDGE: Duration = Duration::from_secs(2);

    fn db_path() -> Option<PathBuf> {
        // Back-compat: also honor the legacy `$CLAURDVOYANT_HOME` / `claurdvoyant` cache dir so a
        // catalog built before the rename keeps being used.
        let dir = std::env::var_os("CLUSTERVISION_HOME")
            .or_else(|| std::env::var_os("CLAURDVOYANT_HOME"))
            .map(PathBuf::from)
            .or_else(|| {
                let cache = dirs::cache_dir()?;
                let new = cache.join("clustervision");
                let legacy = cache.join("claurdvoyant");
                Some(if !new.exists() && legacy.exists() { legacy } else { new })
            })?;
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join("catalog.db"))
    }

    /// An open connection to the shared catalog db (WAL + busy timeout, `sessions` DDL applied),
    /// for sibling catalog tables — the event catalog ([`crate::events`]) adds its own DDL on top.
    /// `None` on any error; callers degrade.
    pub(crate) fn open_db() -> Option<Connection> {
        open()
    }

    fn open() -> Option<Connection> {
        let conn = Connection::open(db_path()?).ok()?;
        // WAL + a busy timeout so concurrent cv/cvd processes don't error each other out.
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(
                 harness       TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 cwd           TEXT,
                 title         TEXT,
                 created_at    INTEGER,
                 updated_at    INTEGER,
                 message_count INTEGER,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE TABLE IF NOT EXISTS watched(
                 harness  TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 PRIMARY KEY(harness, path)
             );
             CREATE TABLE IF NOT EXISTS scan_cache(
                 path     TEXT PRIMARY KEY,
                 mtime_ns INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 refs     TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS catalog_meta(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .ok()?;
        // `sessions` predates the catalog read path, which needs sub-second timestamps to
        // reproduce discovery's ordering exactly. The original second-precision columns stay (the
        // event catalog joins them); these ride alongside. Errors ignored: a concurrent open may
        // have just added them (and a pre-migration reader simply sees NULL → second fallback).
        let has_ns: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name='updated_ns')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if !has_ns {
            let _ = conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN created_ns INTEGER;
                 ALTER TABLE sessions ADD COLUMN updated_ns INTEGER;",
            );
        }
        Some(conn)
    }

    /// `path`'s mtime in integer nanoseconds since the epoch, if stat-able.
    fn stat_mtime_ns(path: &Path) -> Option<i64> {
        let t = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(t.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64)
    }

    /// The SELECT column list [`row_to_ref`] decodes — keep the two in lockstep.
    const REF_COLUMNS: &str = "harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns";

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
    fn row_to_ref(row: &Row) -> rusqlite::Result<Option<SessionRef>> {
        let harness: String = row.get(0)?;
        let Some(harness) = Harness::parse(&harness) else {
            return Ok(None);
        };
        Ok(Some(SessionRef {
            harness,
            id: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            cwd: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            title: row.get(4)?,
            created_at: ts_col(row, 8, 5)?,
            updated_at: ts_col(row, 9, 6)?,
            message_count: row.get::<_, i64>(7)? as usize,
        }))
    }

    /// Insert (or replace) every ref through `stmt` — shared by [`sync`] and [`replace_harness`].
    fn insert_refs(tx: &rusqlite::Transaction, refs: &[SessionRef]) {
        let Ok(mut stmt) = tx.prepare(
            "INSERT OR REPLACE INTO sessions
             (harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        ) else {
            return;
        };
        for r in refs {
            let _ = stmt.execute(params![
                r.harness.as_str(),
                r.id,
                r.path.to_string_lossy(),
                r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                r.title,
                r.created_at.map(|t| t.timestamp()),
                r.updated_at.map(|t| t.timestamp()),
                r.message_count as i64,
                r.created_at.and_then(|t| t.timestamp_nanos_opt()),
                r.updated_at.and_then(|t| t.timestamp_nanos_opt()),
            ]);
        }
    }

    /// Upsert every ref in one transaction, deleting nothing — the *additive* sync for callers
    /// that know about one session (event ingest, tests). Full discovery uses [`replace_harness`]
    /// instead so vanished sessions actually disappear.
    pub fn sync(refs: &[SessionRef]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        insert_refs(&tx, refs);
        let _ = tx.commit();
    }

    /// Replace one harness's catalog rows *and* its watch entries in a single transaction — the
    /// post-discovery sync. Deleting first is what makes vanished sessions vanish from
    /// [`all_sessions`]; the transaction keeps concurrent readers from seeing the gap.
    pub(crate) fn replace_harness(h: Harness, refs: &[SessionRef], watches: &[(PathBuf, i64)]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        let _ = tx.execute("DELETE FROM sessions WHERE harness=?1", params![h.as_str()]);
        let _ = tx.execute("DELETE FROM watched WHERE harness=?1", params![h.as_str()]);
        insert_refs(&tx, refs);
        {
            let Ok(mut stmt) = tx.prepare("INSERT OR REPLACE INTO watched(harness,path,mtime_ns) VALUES(?1,?2,?3)")
            else {
                return;
            };
            for (path, mtime_ns) in watches {
                let _ = stmt.execute(params![h.as_str(), path.to_string_lossy(), mtime_ns]);
            }
        }
        let _ = tx.commit();
    }

    /// Compute the watch set for one harness from a finished discovery: the harness root, every
    /// ancestor directory between each session file and the root (so a new project/day directory
    /// anywhere in the tree bumps a watched mtime), and — for the SQLite-backed harnesses — each
    /// session's backing file itself (their "session file" is a database that grows in place, which
    /// directory mtimes never reflect). Paths that can't be stat'd are skipped: if they reappear,
    /// their parent's mtime changes. Recently-modified paths record `0` (see [`WATCH_FUDGE`]).
    pub(crate) fn watches_for(h: Harness, root: Option<&Path>, refs: &[SessionRef]) -> Vec<(PathBuf, i64)> {
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        if let Some(root) = root {
            paths.insert(root.to_path_buf());
        }
        let file_watch = matches!(
            h,
            Harness::Cursor | Harness::Goose | Harness::Hermes | Harness::Zed | Harness::OpenCode
        );
        for r in refs {
            if file_watch {
                paths.insert(r.path.clone());
            }
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
            .filter_map(|p| {
                let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
                let ns = mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64;
                let ns = match now.duration_since(mtime) {
                    Ok(age) if age >= WATCH_FUDGE => ns,
                    _ => 0, // too fresh (or clock skew): force one re-discovery next probe
                };
                Some((p, ns))
            })
            .collect()
    }

    /// The freshness probe: which harnesses need re-discovery before the catalog can be trusted?
    ///
    /// Three checks, all stat-only (a few hundred syscalls, ~ms):
    /// 1. every `watched` path (dirs + db files) — new/changed/vanished mtime ⇒ stale harness;
    /// 2. the [`TOP_K`] most-recently-updated session files that have a `scan_cache` entry —
    ///    an in-place append changes `(mtime, size)` there (active sessions are precisely the
    ///    recently-updated ones);
    /// 3. each registered adapter's `storage_root()` presence vs. whether we hold watch rows for
    ///    it — a harness installed (or wholly removed) since the last sync is stale.
    ///
    /// `None` means the catalog is unusable (sqlite error) and the caller must fall back to a full
    /// discovery. An empty Vec means "fresh, read away".
    ///
    /// Documented gaps (accepted; bounded by the [`crate::sessions`] staleness backstop):
    /// * an append to a session **outside** the top-K window, or to a session of a harness that
    ///   doesn't use the scan cache and isn't db-file-watched (e.g. grok/opencode), goes unseen
    ///   until the backstop — unless the writer rename-installs the file, which bumps the dir;
    /// * a first session appearing in a previously *empty* pre-existing subdirectory deeper than
    ///   any watched dir (no watched mtime changes);
    /// * filesystems with coarse mtime granularity can miss a same-tick change (the classic
    ///   make(1) race) — APFS/ext4 nanosecond stamps make this vanishingly rare.
    pub(crate) fn probe_stale() -> Option<Vec<Harness>> {
        let conn = open()?;
        let mut stale: Vec<Harness> = Vec::new();
        fn mark(stale: &mut Vec<Harness>, h: Harness) {
            if !stale.contains(&h) {
                stale.push(h);
            }
        }

        // 1. watched dirs + db files.
        let mut watched_harnesses: HashSet<Harness> = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT harness, path, mtime_ns FROM watched").ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })
                .ok()?;
            for (h, path, recorded) in rows.flatten() {
                let Some(h) = Harness::parse(&h) else { continue };
                watched_harnesses.insert(h);
                if stale.contains(&h) {
                    continue; // already condemned; skip the stat
                }
                match stat_mtime_ns(Path::new(&path)) {
                    Some(ns) if recorded != 0 && ns == recorded => {}
                    _ => mark(&mut stale, h), // changed, vanished, or fudge-stamped
                }
            }
        }

        // 2. top-K recently-updated session files with scan-cache stats (append detection).
        {
            let mut stmt = conn
                .prepare(
                    "SELECT s.harness, s.path, c.mtime_ns, c.size
                     FROM sessions s JOIN scan_cache c ON c.path = s.path
                     ORDER BY s.updated_at DESC LIMIT ?1",
                )
                .ok()?;
            let rows = stmt
                .query_map(params![TOP_K as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .ok()?;
            let mut seen: HashSet<String> = HashSet::new();
            for (h, path, mtime_ns, size) in rows.flatten() {
                let Some(h) = Harness::parse(&h) else { continue };
                if stale.contains(&h) || !seen.insert(path.clone()) {
                    continue;
                }
                let live = std::fs::metadata(Path::new(&path)).ok().and_then(|m| {
                    let t = m.modified().ok()?;
                    Some((t.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64, m.len() as i64))
                });
                if live != Some((mtime_ns, size)) {
                    mark(&mut stale, h);
                }
            }
        }

        // 3. harnesses that appeared (root now exists, no watch rows) or vanished outright.
        for a in crate::harness::all() {
            let h = a.harness();
            if a.storage_root().is_some() != watched_harnesses.contains(&h) {
                mark(&mut stale, h);
            }
        }

        Some(stale)
    }

    /// Every catalog row, in insertion (= discovery) order: [`replace_harness`] deletes + inserts
    /// each harness's refs in the order discovery produced them, so after a full sync `rowid`
    /// order reproduces [`crate::discover_all`]'s output order exactly — which keeps `cv ls`'s
    /// stable sorts byte-identical even across rows whose timestamps tie. `None` on any sqlite
    /// error — callers fall back to a full discovery. Note `Some(vec![])` is a valid answer (a
    /// machine with no sessions); cold-vs-empty is [`last_full_sync`]'s job.
    pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
        let conn = open()?;
        let mut stmt = conn
            .prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))
            .ok()?;
        let rows = stmt.query_map([], row_to_ref).ok()?;
        Some(rows.flatten().flatten().collect())
    }

    /// Seconds-since-epoch of the last completed full discovery, if any. Absent = cold catalog.
    pub(crate) fn last_full_sync() -> Option<i64> {
        let conn = open()?;
        conn.query_row("SELECT value FROM catalog_meta WHERE key='last_full_sync'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()?
        .parse()
        .ok()
    }

    /// Stamp "a full discovery just completed" (the [`last_full_sync`] the staleness backstop reads).
    pub(crate) fn stamp_full_sync() {
        let Some(conn) = open() else { return };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync',?1)",
            params![chrono::Utc::now().timestamp().to_string()],
        );
    }

    /// All catalog rows whose id equals `id` or starts with it, optionally constrained to one
    /// harness. Order: exact-id matches first. Empty on any error or a cold catalog.
    pub fn lookup(id: &str, harness: Option<Harness>) -> Vec<SessionRef> {
        let Some(conn) = open() else { return Vec::new() };
        // Escape LIKE metacharacters so ids containing `%`/`_` match literally (same treatment as
        // `events::sessions_touching`) — `_` is common in real session ids.
        let escaped = id.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let like = format!("{escaped}%");
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {REF_COLUMNS} FROM sessions WHERE id=?1 OR id LIKE ?2 ESCAPE '\\'
             ORDER BY (id=?1) DESC",
        )) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![id, like], row_to_ref);
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .flatten()
            .filter(|r| harness.is_none_or(|want| r.harness == want))
            .collect()
    }
}

#[cfg(feature = "sqlite")]
pub(crate) use imp::{
    all_sessions, last_full_sync, open_db, probe_stale, replace_harness, stamp_full_sync, watches_for,
};
#[cfg(feature = "sqlite")]
pub use imp::{lookup, sync};

/// No-op fallbacks when sqlite is disabled — `find`/`sessions` simply always take the scan path.
#[cfg(not(feature = "sqlite"))]
pub fn sync(_refs: &[SessionRef]) {}
#[cfg(not(feature = "sqlite"))]
pub fn lookup(_id: &str, _harness: Option<Harness>) -> Vec<SessionRef> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn replace_harness(_h: Harness, _refs: &[SessionRef], _watches: &[(std::path::PathBuf, i64)]) {}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn watches_for(_h: Harness, _root: Option<&Path>, _refs: &[SessionRef]) -> Vec<(std::path::PathBuf, i64)> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn probe_stale() -> Option<Vec<Harness>> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn last_full_sync() -> Option<i64> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn stamp_full_sync() {}
