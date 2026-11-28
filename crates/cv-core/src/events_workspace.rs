//! Local file versions captured by installed tool receipts.
use crate::source::FileId;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Peer places with incomplete contact and recorded activity in this checkout.
#[cfg(feature = "sqlite")]
pub fn stale_peer_places(directories: &std::collections::BTreeSet<&Path>) -> anyhow::Result<Vec<String>> {
    use crate::state::{
        hub::{AuthorityQueryContext, HubStore, PlaceState},
        StateStore,
    };
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let hub = HubStore::default_hub()?;
    let authority = hub.compute_authority(&AuthorityQueryContext::default())?;
    let stale: std::collections::BTreeSet<_> = authority
        .places
        .into_iter()
        .filter(|p| matches!(p.state, PlaceState::Partitioned | PlaceState::Unknown))
        .map(|p| p.place_id)
        .collect();
    if stale.is_empty() {
        return Ok(Vec::new());
    }
    let roots: std::collections::BTreeSet<_> = directories
        .iter()
        .map(|p| {
            p.ancestors()
                .find(|ancestor| ancestor.join(".git").exists())
                .unwrap_or(p)
        })
        .collect();
    let store = StateStore::default_store()?;
    let matches_checkout = |dir: &str| !dir.is_empty() && roots.iter().any(|root| Path::new(dir).starts_with(root));
    let peers = store.read_in(|conn| {
        use rusqlite::OptionalExtension;
        let have_heads = conn.table_exists(None, "hub_session_heads")?;
        let mut peers = Vec::new();
        for place in stale {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT metadata_json FROM producers WHERE producer_id = ?1",
                    [&place],
                    |row| row.get(0),
                )
                .optional()?;
            let metadata: serde_json::Value = serde_json::from_str(raw.as_deref().unwrap_or("null"))?;
            let enrolled = ["cwd", "repo", "repo_root"]
                .iter()
                .any(|key| metadata[*key].as_str().is_some_and(matches_checkout));
            let policy = &metadata["filter_policy"];
            let in_policy = |cwd: &Path, key: &str| {
                policy[key].as_array().is_some_and(|hashes| {
                    cwd.ancestors().any(|path| {
                        let hash = crate::state::hub::shipper::hash_cwd_filter_prefix(&place, &path.to_string_lossy());
                        hashes.iter().any(|value| value.as_str() == Some(hash.as_str()))
                    })
                })
            };
            if enrolled
                || directories
                    .iter()
                    .any(|dir| in_policy(dir, "allow_cwd_hashes") && !in_policy(dir, "deny_cwd_hashes"))
            {
                peers.push(place);
                continue;
            }
            if have_heads {
                let mut heads = conn.prepare_cached(
                    "SELECT DISTINCT cwd FROM hub_session_heads
                     WHERE producer_id = ?1 AND gen_to IS NULL AND cwd IS NOT NULL",
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
    })?;
    Ok(peers.unwrap_or_default())
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

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use serde_json::json;

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
