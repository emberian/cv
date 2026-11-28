//! Local file versions captured by installed tool receipts.
use crate::source::FileId;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Canonical checkout root for an edited path or directory.
#[cfg(feature = "sqlite")]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CheckoutRoot {
    path: std::path::PathBuf,
    has_git: bool,
}

#[cfg(feature = "sqlite")]
impl CheckoutRoot {
    fn from_dir(dir: &Path) -> Self {
        let norm = crate::tools::classify::normalize_path(dir, None);
        if let Some(git_root) = norm.ancestors().find(|ancestor| ancestor.join(".git").exists()) {
            Self {
                path: git_root.to_path_buf(),
                has_git: true,
            }
        } else {
            Self {
                path: norm,
                has_git: false,
            }
        }
    }

    fn matches_peer_dir(&self, raw_dir: &str) -> bool {
        if raw_dir.is_empty() {
            return false;
        }
        let peer_norm = crate::tools::classify::normalize_path(Path::new(raw_dir), None);
        if peer_norm.as_os_str().is_empty() {
            return false;
        }
        let peer_git = peer_norm.ancestors().find(|ancestor| ancestor.join(".git").exists());
        match (self.has_git, peer_git) {
            // Both checkouts exist locally as git repos or worktrees: require the exact same
            // worktree root so nested submodules, sibling worktrees, and same-basename repos
            // never cross-contaminate.
            (true, Some(pg)) => pg == self.path.as_path(),
            // Local checkout is a git worktree and peer path is non-existent/remote or a subdir.
            (true, None) => peer_norm.starts_with(&self.path),
            // Synthetic / non-git local path, peer has a local .git root.
            (false, Some(pg)) => self.path.starts_with(pg),
            // Neither side has a local .git marker: component-wise prefix match on normalized path.
            (false, None) => peer_norm.starts_with(&self.path),
        }
    }
}

#[cfg(feature = "sqlite")]
fn open_fast_readonly_state(db_path: &Path) -> anyhow::Result<Option<rusqlite::Connection>> {
    let meta = match std::fs::metadata(db_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() == 0 {
        return Ok(None);
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(db_path, flags)?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(25));
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if ver <= 0 {
        return Ok(None);
    }
    Ok(Some(conn))
}

#[cfg(feature = "sqlite")]
fn stale_peers_in_conn(
    conn: &rusqlite::Connection,
    roots: &[CheckoutRoot],
    raw_dirs: &[std::path::PathBuf],
) -> Result<Vec<String>, crate::state::StateError> {
    use crate::state::hub::ProducerLifecycle;
    if roots.is_empty() || !conn.table_exists(None, "producers")? {
        return Ok(Vec::new());
    }
    let has_remote: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM producers WHERE producer_id <> 'local')",
        [],
        |r| r.get(0),
    )?;
    if !has_remote {
        return Ok(Vec::new());
    }

    const DEFAULT_PARTITION_MS: i64 = 300_000;
    let now_ms = chrono::Utc::now().timestamp_millis();

    let quarantined: std::collections::BTreeSet<String> = if conn.table_exists(None, "hub_quarantine")? {
        let mut stmt =
            conn.prepare_cached("SELECT DISTINCT producer_id FROM hub_quarantine WHERE resolved_at IS NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    } else {
        std::collections::BTreeSet::new()
    };

    type FrontierRow = (String, i64, i64, i64, Option<i64>, i64);
    let mut frontiers: std::collections::BTreeMap<String, Vec<FrontierRow>> = std::collections::BTreeMap::new();
    if conn.table_exists(None, "hub_frontiers")? {
        let mut stmt = conn.prepare_cached(
            "SELECT producer_id, epoch, contiguous_seq, max_received_seq, buffered_count, first_conflict_seq, updated_at
             FROM hub_frontiers",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, i64>(6)?,
                ),
            ))
        })?;
        for row in rows {
            let (pid, f) = row?;
            frontiers.entry(pid).or_default().push(f);
        }
    }

    let epoch_gen = |epoch: &str| -> u8 {
        if epoch.len() == 36 && epoch.as_bytes().get(14) == Some(&b'4') && uuid::Uuid::parse_str(epoch).is_ok() {
            0
        } else {
            1
        }
    };

    let mut prod_stmt = conn.prepare_cached(
        "SELECT producer_id, metadata_json, updated_at FROM producers WHERE producer_id <> 'local' ORDER BY producer_id",
    )?;
    let prod_rows: Vec<(String, String, i64)> = prod_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let have_heads = conn.table_exists(None, "hub_session_heads")?;
    let matches_checkout = |dir: &str| roots.iter().any(|root| root.matches_peer_dir(dir));
    let mut peers = Vec::new();

    for (place, raw_meta, prod_updated_at) in prod_rows {
        let metadata: serde_json::Value = serde_json::from_str(&raw_meta).unwrap_or(serde_json::Value::Null);
        let prod_frontiers = frontiers.get(&place);
        let latest_f = prod_frontiers.and_then(|list| list.iter().max_by_key(|(ep, ..)| (epoch_gen(ep), ep.as_str())));
        let any_gap = prod_frontiers.is_some_and(|list| {
            list.iter()
                .any(|(_, contig, max_seq, buffered, ..)| *buffered > 0 || *contig < *max_seq)
        });
        let any_conflict = quarantined.contains(&place)
            || prod_frontiers.is_some_and(|list| list.iter().any(|(.., conflict, _)| conflict.is_some()));
        let lifecycle = metadata
            .get("lifecycle")
            .and_then(|v| serde_json::from_value::<ProducerLifecycle>(v.clone()).ok());

        let is_stale = if any_conflict {
            true
        } else if let Some(ProducerLifecycle::Sealed {
            final_epoch, final_seq, ..
        }) = &lifecycle
        {
            match latest_f {
                Some((ep, contig, ..)) => ep != final_epoch || (*contig as u64) != *final_seq || any_gap,
                None => true,
            }
        } else {
            match latest_f {
                None => true,
                Some((.., f_updated_at)) => {
                    let last_contact_ms = (*f_updated_at).max(prod_updated_at);
                    any_gap || (now_ms - last_contact_ms).max(0) > DEFAULT_PARTITION_MS
                }
            }
        };
        if !is_stale {
            continue;
        }

        let enrolled = ["cwd", "repo", "repo_root"]
            .iter()
            .any(|key| metadata[*key].as_str().is_some_and(matches_checkout));
        let policy = &metadata["filter_policy"];
        let in_policy = |cwd: &Path, key: &str| {
            policy[key].as_array().is_some_and(|hashes| {
                let norm = crate::tools::classify::normalize_path(cwd, None);
                cwd.ancestors().chain(norm.ancestors()).any(|path| {
                    let hash = crate::state::hub::shipper::hash_cwd_filter_prefix(&place, &path.to_string_lossy());
                    hashes.iter().any(|value| value.as_str() == Some(hash.as_str()))
                })
            })
        };
        if enrolled
            || raw_dirs
                .iter()
                .any(|dir| in_policy(dir, "allow_cwd_hashes") && !in_policy(dir, "deny_cwd_hashes"))
        {
            peers.push(place);
            continue;
        }
        if have_heads {
            let mut heads = conn.prepare_cached(
                "SELECT DISTINCT cwd FROM hub_session_heads
                 WHERE producer_id = ?1 AND +gen_to IS NULL AND cwd IS NOT NULL",
            )?;
            for dir in heads.query_map([&place], |row| row.get::<_, String>(0))? {
                if matches_checkout(&dir?) {
                    peers.push(place);
                    break;
                }
            }
        }
    }
    Ok(peers)
}

/// Peer places with incomplete contact and recorded activity in this checkout.
#[cfg(feature = "sqlite")]
pub fn stale_peer_places(directories: &std::collections::BTreeSet<&Path>) -> anyhow::Result<Vec<String>> {
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::state::StateStore::default_store()?;
    if !store.path().is_file() {
        return Ok(Vec::new());
    }
    let roots: Vec<CheckoutRoot> = directories
        .iter()
        .map(|p| CheckoutRoot::from_dir(p))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let raw_dirs: Vec<std::path::PathBuf> = directories.iter().map(|p| p.to_path_buf()).collect();
    if crate::env::frozen() {
        return Ok(store
            .read_in(|conn| stale_peers_in_conn(conn, &roots, &raw_dirs))?
            .unwrap_or_default());
    }
    match open_fast_readonly_state(store.path())? {
        Some(conn) => Ok(stale_peers_in_conn(&conn, &roots, &raw_dirs)?),
        None => Ok(Vec::new()),
    }
}

/// Session-deduplicated stale-peer check for `cv hook edit` (`PreToolUse` `Edit|Write`).
///
/// Resolves canonical checkout roots strictly from the edited paths (never polluting roots with an
/// unrelated `$HOME` or `/tmp` process `cwd`), executes in a single `< 5 ms` indexed SQLite pass,
/// degrades cleanly when `state.db` is absent, `--frozen`, or locked, and records an emitted row in
/// `whispers` so the same session is advised once per `(checkout, stale_peer_set)` instead of on
/// every edit turn.
#[cfg(feature = "sqlite")]
pub fn stale_peer_advisory(
    harness: &str,
    session_id: &str,
    agent_id: Option<&str>,
    edits: &[String],
    cwd: Option<&Path>,
) -> anyhow::Result<Vec<String>> {
    if edits.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::state::StateStore::default_store()?;
    if !store.path().is_file() {
        return Ok(Vec::new());
    }
    let mut root_set = std::collections::BTreeSet::new();
    let mut raw_dirs = Vec::with_capacity(edits.len());
    for edit in edits {
        let norm = crate::tools::classify::normalize_path(Path::new(edit), cwd);
        let dir = norm.parent().unwrap_or(&norm);
        if !dir.as_os_str().is_empty() {
            raw_dirs.push(dir.to_path_buf());
            root_set.insert(CheckoutRoot::from_dir(dir));
        }
    }
    if root_set.is_empty() {
        return Ok(Vec::new());
    }
    let roots: Vec<CheckoutRoot> = root_set.into_iter().collect();
    let recipient = match agent_id.filter(|a| !a.is_empty()) {
        Some(a) if !session_id.is_empty() => format!("{session_id}:{a}"),
        _ => session_id.to_owned(),
    };
    let subject_key = roots
        .iter()
        .map(|r| r.path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");

    let evaluate = |conn: &rusqlite::Connection| -> Result<(Vec<String>, bool), crate::state::StateError> {
        let peers = stale_peers_in_conn(conn, &roots, &raw_dirs)?;
        if peers.is_empty() || recipient.is_empty() || !conn.table_exists(None, "whispers")? {
            return Ok((peers, false));
        }
        let subject_rev = blake3::hash(peers.join("\0").as_bytes()).to_hex().to_string();
        let already: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM whispers
                WHERE session_id = ?1 AND kind = 'stale_peer_edits' AND subject_key = ?2 AND subject_rev = ?3
            )",
            rusqlite::params![recipient, subject_key, subject_rev],
            |r| r.get(0),
        )?;
        Ok((peers, already))
    };

    let (peers, already_emitted) = if crate::env::frozen() {
        match store.read_in(evaluate)? {
            Some(pair) => pair,
            None => return Ok(Vec::new()),
        }
    } else {
        let Some(conn) = open_fast_readonly_state(store.path())? else {
            return Ok(Vec::new());
        };
        evaluate(&conn)?
    };

    if peers.is_empty() || already_emitted {
        return Ok(Vec::new());
    }

    if !crate::env::frozen() && !recipient.is_empty() {
        let subject_rev = blake3::hash(peers.join("\0").as_bytes()).to_hex().to_string();
        let command = format!("cv touched {}", edits[0]);
        let now_ms = chrono::Utc::now().timestamp_millis();
        // Best-effort bounded write (`25ms` busy timeout): if `state.db` is locked or read-only,
        // never block or fail `PreToolUse`; if a concurrent hook in the same session won the insert
        // race, suppress the duplicate advisory.
        if let Ok(wconn) = rusqlite::Connection::open(store.path()) {
            let _ = wconn.busy_timeout(std::time::Duration::from_millis(25));
            if let Ok(true) = wconn.table_exists(None, "whispers") {
                if let Ok(0) = wconn.execute(
                    "INSERT INTO whispers(session_id, kind, subject_key, subject_rev, command, status, harness, emitted_at)
                     SELECT ?1, 'stale_peer_edits', ?2, ?3, ?4, 'emitted', ?5, ?6
                     WHERE (SELECT COUNT(*) FROM whispers WHERE session_id = ?1) < 256
                     ON CONFLICT DO NOTHING",
                    rusqlite::params![recipient, subject_key, subject_rev, command, harness, now_ms],
                ) {
                    let raced: bool = wconn
                        .query_row(
                            "SELECT EXISTS(
                                SELECT 1 FROM whispers
                                WHERE session_id = ?1 AND kind = 'stale_peer_edits' AND subject_key = ?2 AND subject_rev = ?3
                            )",
                            rusqlite::params![recipient, subject_key, subject_rev],
                            |r| r.get(0),
                        )
                        .unwrap_or(false);
                    if raced {
                        return Ok(Vec::new());
                    }
                }
            }
        }
    }

    Ok(peers)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileVersion {
    pub path: String,
    pub blob: String,
    pub edit: bool,
    /// Half-open byte range of a uniquely located replacement in this exact blob.
    pub span: Option<(usize, usize)>,
}

pub fn snapshot(path: &str, edit: bool, old: Option<&str>) -> Option<FileVersion> {
    use std::io::Read;
    const LIMIT: u64 = 1024 * 1024;
    let p = Path::new(path);
    let meta = std::fs::symlink_metadata(p).ok()?;
    if !meta.is_file() || meta.len() > LIMIT {
        return None;
    }
    let before = FileId::of(&meta)?;
    let mut bytes = Vec::new();
    std::fs::File::open(p)
        .ok()?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 != before.len || FileId::at(p) != Some(before) {
        return None;
    }
    let span = old.filter(|s| !s.is_empty()).and_then(|s| {
        let text = std::str::from_utf8(&bytes).ok()?;
        let start = text.find(s)?;
        (text.rfind(s) == Some(start)).then_some((start, start + s.len()))
    });
    Some(FileVersion {
        path: path.into(),
        blob: blake3::hash(&bytes).to_hex().to_string(),
        edit,
        span,
    })
}

/// Update the local version projection in the transaction that admits this receipt.
#[cfg(feature = "sqlite")]
pub(crate) fn record(
    db: &rusqlite::Connection,
    harness: &str,
    session: &str,
    agent: &str,
    call: &str,
    body: &serde_json::Value,
) -> Result<(), crate::state::StateError> {
    use rusqlite::params;
    let files = body
        .get("files")
        .map(|v| serde_json::from_value::<Vec<FileVersion>>(v.clone()))
        .transpose()?
        .unwrap_or_default();
    match body["kind"].as_str() {
        Some("pre") if !files.is_empty() => {
            // Expiry is recorded as a monotonic horizon before the rows go, so absence older than
            // it reads as forgotten rather than never observed.
            db.execute(
                "INSERT INTO horizons(name, newest_forgotten_time)
                SELECT 'pending_file_versions', MAX(started) FROM pending_file_versions
                WHERE started < unixepoch()-86400 HAVING MAX(started) IS NOT NULL
                ON CONFLICT(name) DO UPDATE SET
                newest_forgotten_time = MAX(newest_forgotten_time, excluded.newest_forgotten_time)",
                [],
            )?;
            db.execute(
                "DELETE FROM pending_file_versions WHERE started < unixepoch()-86400",
                [],
            )?;
            for f in files {
                // A newer call from the same writer on the same path supersedes its unanswered
                // predecessor (a call whose post never arrived).
                db.execute(
                    "DELETE FROM pending_file_versions
                    WHERE harness=?1 AND session=?2 AND agent=?3 AND path=?5 AND call!=?4",
                    params![harness, session, agent, call, f.path],
                )?;
                db.execute(
                    "INSERT INTO pending_file_versions(harness,session,agent,call,path,edit,base,lo,hi)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT DO NOTHING",
                    params![
                        harness,
                        session,
                        agent,
                        call,
                        f.path,
                        f.edit,
                        f.blob,
                        f.span.map(|s| s.0),
                        f.span.map(|s| s.1)
                    ],
                )?;
                if f.edit {
                    db.execute(
                        "UPDATE pending_file_versions SET ambiguous=1
                        WHERE path=?1 AND edit=1 AND started >= unixepoch()-300
                        AND (SELECT COUNT(*) FROM pending_file_versions
                             WHERE path=?1 AND edit=1 AND started >= unixepoch()-300)>1",
                        params![f.path],
                    )?;
                }
            }
        }
        Some("post") => {
            for f in files {
                db.execute(
                    "INSERT INTO file_versions(harness,session,agent,call,path,edit,base,blob,lo,hi)
                    SELECT harness,session,agent,call,path,edit,base,?7,lo,hi FROM pending_file_versions
                    WHERE harness=?1 AND session=?2 AND agent=?3 AND call=?4 AND path=?5 AND edit=?6 AND ambiguous=0 AND (base!=?7)=edit
                    ON CONFLICT DO NOTHING",
                    params![harness, session, agent, call, f.path, f.edit, f.blob],
                )?;
            }
            db.execute(
                "DELETE FROM pending_file_versions WHERE harness=?1 AND session=?2 AND agent=?3 AND call=?4",
                params![harness, session, agent, call],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Editors whose pre-call observation is still live. Reads never enter this lookup.
#[cfg(feature = "sqlite")]
pub fn pending_editors(path: &str) -> anyhow::Result<Vec<super::TouchedSession>> {
    Ok(crate::state::StateStore::default_store()?
        .read_in(|conn| {
            if crate::state::current_schema_version(conn)? < 5 {
                return Ok(vec![]);
            }
            let mut stmt = conn.prepare(
                "SELECT DISTINCT harness,session,agent FROM pending_file_versions
                WHERE path=?1 AND edit=1 AND started >= unixepoch()-300 ORDER BY harness,session,agent LIMIT 128",
            )?;
            let rows = stmt.query_map([path], |r| {
                let harness: String = r.get(0)?;
                let session: String = r.get(1)?;
                let agent: String = r.get(2)?;
                let child = (!agent.is_empty())
                    .then(|| crate::Harness::parse(&harness).and_then(crate::harness::rootless))
                    .flatten()
                    .and_then(|adapter| adapter.agent_session_id(&agent));
                let parent_id = child.is_some().then(|| session.clone());
                Ok(super::TouchedSession {
                    harness,
                    session_id: child.unwrap_or(session),
                    parent_id,
                    agent_id: (!agent.is_empty()).then_some(agent),
                    title: None,
                    workflow: None,
                    edits: 1,
                    reads: 0,
                    last_ts: None,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?
        .unwrap_or_default())
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleRead {
    pub path: String,
    pub read_blob: String,
    pub written_blob: String,
    pub reader: String,
    pub reader_agent: String,
    pub writer_agent: String,
    pub writer: String,
    pub writer_harness: String,
}

#[cfg(feature = "sqlite")]
pub fn stale_reads(
    harness: &str,
    session: &str,
    agent: Option<&str>,
    path: Option<&str>,
) -> anyhow::Result<Vec<StaleRead>> {
    Ok(crate::state::StateStore::default_store()?
        .read_in(|conn| {
            if crate::state::current_schema_version(conn)? < 5 {
                return Ok(vec![]);
            }
            let mut stmt = conn.prepare(
                "SELECT r.path,r.blob,w.blob,w.session,w.harness,r.session,r.agent,w.agent FROM file_versions r
                JOIN file_versions w ON w.path=r.path AND w.edit=1 AND w.rowid>r.rowid
                WHERE r.harness=?1 AND r.session=?2 AND (?3 IS NULL OR r.agent=?3) AND (?4 IS NULL OR r.path=?4)
                AND r.edit=0 AND w.base!=w.blob AND w.blob!=r.blob
                AND (w.harness!=r.harness OR w.session!=r.session OR w.agent!=r.agent)
                AND NOT EXISTS (SELECT 1 FROM file_versions newer WHERE newer.path=r.path AND newer.harness=r.harness AND newer.session=r.session AND newer.agent=r.agent AND newer.rowid>r.rowid)
                AND NOT EXISTS (SELECT 1 FROM file_versions newer WHERE newer.path=w.path AND newer.edit=1 AND newer.rowid>w.rowid)
                ORDER BY r.path,r.rowid",
            )?;
            let rows = stmt.query_map(rusqlite::params![harness, session, agent, path], |r| {
                Ok(StaleRead {
                    path: r.get(0)?,
                    read_blob: r.get(1)?,
                    written_blob: r.get(2)?,
                    writer: r.get(3)?,
                    writer_harness: r.get(4)?,
                    reader: r.get(5)?,
                    reader_agent: r.get(6)?,
                    writer_agent: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?
        .unwrap_or_default())
}

#[cfg(feature = "sqlite")]
pub fn overlaps(harness: &str, session: &str, agent: Option<&str>, v: &FileVersion) -> Option<bool> {
    let span = v.span?;
    crate::state::StateStore::default_store()
        .ok()?
        .read_in(|conn| {
            let mut stmt = conn.prepare(
                "SELECT base,lo,hi FROM pending_file_versions
                WHERE harness=?1 AND session=?2 AND agent=?3 AND path=?4 AND edit=1 AND started>=unixepoch()-300
                UNION ALL SELECT base,lo,hi FROM (SELECT base,lo,hi FROM file_versions WHERE harness=?1 AND session=?2
                AND agent=?3 AND path=?4 AND edit=1 ORDER BY rowid DESC LIMIT 1)",
            )?;
            let rows = stmt.query_map(rusqlite::params![harness, session, agent.unwrap_or(""), v.path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<usize>>(1)?,
                    r.get::<_, Option<usize>>(2)?,
                ))
            })?;
            let mut same_base = false;
            for row in rows {
                let (base, lo, hi) = row?;
                if base == v.blob {
                    let (Some(lo), Some(hi)) = (lo, hi) else {
                        return Ok(None);
                    };
                    if lo < span.1 && span.0 < hi {
                        return Ok(Some(true));
                    }
                    same_base = true;
                }
            }
            Ok(same_base.then_some(false))
        })
        .ok()??
}

#[cfg(feature = "sqlite")]
pub fn observed_edits(harness: &str, session: &str) -> anyhow::Result<std::collections::BTreeSet<String>> {
    Ok(crate::state::StateStore::default_store()?
        .read_in(|conn| {
            if crate::state::current_schema_version(conn)? < 5 {
                return Ok(Default::default());
            }
            let mut stmt =
                conn.prepare("SELECT DISTINCT path FROM file_versions WHERE harness=?1 AND session=?2 AND edit=1")?;
            let rows = stmt.query_map([harness, session], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?
        .unwrap_or_default())
}

#[cfg(not(feature = "sqlite"))]
pub fn observed_edits(_harness: &str, _session: &str) -> anyhow::Result<std::collections::BTreeSet<String>> {
    Ok(Default::default())
}

/// Latest recorded file blob per path observed or produced by `(harness, session)` via receipts.
#[cfg(feature = "sqlite")]
pub fn observed_file_blobs(harness: &str, session: &str) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    Ok(crate::state::StateStore::default_store()?
        .read_in(|conn| {
            if crate::state::current_schema_version(conn)? < 5 {
                return Ok(Default::default());
            }
            let mut stmt = conn
                .prepare("SELECT path, blob FROM file_versions WHERE harness=?1 AND session=?2 ORDER BY rowid ASC")?;
            let mut out = std::collections::BTreeMap::new();
            let rows = stmt.query_map([harness, session], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (path, blob) = row?;
                out.insert(path, blob);
            }
            Ok(out)
        })?
        .unwrap_or_default())
}

#[cfg(not(feature = "sqlite"))]
pub fn observed_file_blobs(
    _harness: &str,
    _session: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    Ok(Default::default())
}

#[cfg(not(feature = "sqlite"))]
pub fn stale_peer_advisory(
    _harness: &str,
    _session_id: &str,
    _agent_id: Option<&str>,
    _edits: &[String],
    _cwd: Option<&Path>,
) -> anyhow::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stale_peer_advisory_deduplicates_per_session_and_canonicalizes_worktrees_and_symlinks() {
        // #tooth Repeated edits in one session whisper once per stale peer set; symlinked paths
        // match canonically; sibling worktrees, same-basename repos, and broad parent cwds never
        // cross-contaminate; absent/frozen state degrades in sub-millisecond time.
        let w = crate::test_world("stale-peer-advisory-tooth");
        let parent_workspace = w.base.join("workspace");
        let repo_real = parent_workspace.join("alpha").join("app");
        let repo_wt2 = parent_workspace.join("wt2").join("app");
        let repo_other_same_name = parent_workspace.join("beta").join("app");
        std::fs::create_dir_all(repo_real.join(".git")).unwrap();
        std::fs::create_dir_all(repo_real.join("src")).unwrap();
        std::fs::create_dir_all(&repo_wt2).unwrap();
        // Git worktrees carry a `.git` file (`gitdir: ...`), not a directory.
        std::fs::write(repo_wt2.join(".git"), "gitdir: /tmp/fake-gitdir\n").unwrap();
        std::fs::create_dir_all(repo_other_same_name.join(".git")).unwrap();

        #[cfg(unix)]
        let repo_symlink = w.base.join("symlinked-app");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_real, &repo_symlink).unwrap();
        #[cfg(not(unix))]
        let repo_symlink = repo_real.clone();

        let target = repo_real.join("src").join("lib.rs").to_string_lossy().into_owned();

        // 1. Cold home (no state.db): zero writes, empty result.
        assert!(stale_peer_advisory(
            "claude",
            "sess-1",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace)
        )
        .unwrap()
        .is_empty());
        assert!(!w.cv_home.join("state.db").exists());

        // 2. Enroll peers across symlink alias, sibling worktree, and same-basename unrelated repo.
        let hub = crate::state::hub::HubStore::at_home(&w.cv_home);
        hub.enroll_producer("peer-symlink", "peer-symlink", &json!({"cwd": repo_symlink}))
            .unwrap();
        hub.enroll_producer("peer-wt2", "peer-wt2", &json!({"cwd": repo_wt2}))
            .unwrap();
        hub.enroll_producer(
            "peer-same-basename",
            "peer-same-basename",
            &json!({"cwd": repo_other_same_name}),
        )
        .unwrap();

        // First edit in `sess-1` (even when `cwd` is the broad `parent_workspace` containing all 3):
        // ONLY `peer-symlink` matches `repo_real`; `peer-wt2` and `peer-same-basename` are excluded!
        let started = std::time::Instant::now();
        let first = stale_peer_advisory(
            "claude",
            "sess-1",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace),
        )
        .unwrap();
        assert_eq!(first, vec!["peer-symlink".to_string()]);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));

        // Second edit in the SAME session `sess-1`: suppressed via `whispers`!
        let second_start = std::time::Instant::now();
        let second = stale_peer_advisory(
            "claude",
            "sess-1",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace),
        )
        .unwrap();
        assert!(second.is_empty(), "duplicate whisper must be suppressed: {second:?}");
        assert!(second_start.elapsed() < std::time::Duration::from_millis(15));

        // Another session `sess-2` still receives its own single advisory.
        let other_sess = stale_peer_advisory(
            "claude",
            "sess-2",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace),
        )
        .unwrap();
        assert_eq!(other_sess, vec!["peer-symlink".to_string()]);

        // When a second peer in the same checkout partitions, `sess-1` is advised once of the new peer set.
        hub.enroll_producer("peer-second", "peer-second", &json!({"cwd": repo_real.join("src")}))
            .unwrap();
        let updated = stale_peer_advisory(
            "claude",
            "sess-1",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace),
        )
        .unwrap();
        assert_eq!(updated, vec!["peer-second".to_string(), "peer-symlink".to_string()]);
        assert!(stale_peer_advisory(
            "claude",
            "sess-1",
            None,
            std::slice::from_ref(&target),
            Some(&parent_workspace),
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn events_pending_hunks_compare_only_on_the_observed_base() {
        // #tooth Both proposals are based on the same bytes while the first call is in flight.
        let w = crate::test_world("events-pending-hunks");
        let path = w.base.join("ir.rs");
        std::fs::write(&path, "first\nsecond\nfirst\n").unwrap();
        let path = path.to_str().unwrap();
        let sample = |text: &str| snapshot(path, true, Some(text)).unwrap();
        let peer = sample("second");
        let store = crate::state::StateStore::default_store().unwrap();
        let receipt = |kind: &str, files: Vec<FileVersion>| {
            let body = json!({"kind":kind,"tool":"Edit","tool_use_id":"peer-edit","files":files});
            store
                .append_receipt_with_inbox("peer", "codex", body.as_object().unwrap().clone())
                .unwrap();
        };
        receipt("pre", vec![peer.clone()]);
        assert_eq!(pending_editors(path).unwrap().len(), 1);
        let overlap = |v: &FileVersion| overlaps("codex", "peer", None, v);
        assert_eq!(overlap(&sample("second")), Some(true));
        assert_eq!(overlap(&sample("first\nsecond")), Some(true));
        assert_eq!(overlap(&sample("\nfirst")), Some(false));
        assert_eq!(overlap(&sample("first")), None);
        std::fs::write(path, "changed second").unwrap();
        assert_eq!(overlap(&sample("second")), None);
        receipt("post", vec![sample("second")]);
        assert!(pending_editors(path).unwrap().is_empty());
    }
}
