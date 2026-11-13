//! Retired sub-agents: an agent `cv distill --agent-of` replaced with a successor. The source
//! transcript stays resumable, so any `SendMessage` to its id (from the root or from another lane
//! that still holds it) wakes a second instance of the same lane. The authoritative `retired_agents`
//! table in `$CLUSTERVISION_HOME/state.db` records each retirement transactionally under SQLite
//! `BEGIN IMMEDIATE`; the `cv hook retired` PreToolUse body reads it to refuse every tool call the
//! retired agent makes and every `SendMessage` addressed to it, naming the successor.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retirement {
    pub ts: DateTime<Utc>,
    /// The bare agent id (`agent-` stripped, as `SendMessage` addresses it).
    pub agent_id: String,
    /// The agent that continues its work, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
    /// The root session that owns both, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A reversal: the agent is no longer retired.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undone: bool,
}

/// Authoritative retirement store path (`$CLUSTERVISION_HOME/state.db` when `sqlite` is enabled).
pub fn ledger_path() -> PathBuf {
    let base = crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(feature = "sqlite")]
    {
        base.join("state.db")
    }
    #[cfg(not(feature = "sqlite"))]
    {
        base.join("retired.jsonl")
    }
}

/// The bare id: `agent-` stripped.
pub fn bare(id: &str) -> &str {
    id.strip_prefix("agent-").unwrap_or(id)
}

fn legacy_jsonl_path() -> PathBuf {
    crate::env::Env::current()
        .cv_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("retired.jsonl")
}

/// Read legacy `$CLUSTERVISION_HOME/retired.jsonl` retirements (from pre-`state.db` installations
/// or portable builds) without mutating disk.
fn load_legacy_jsonl() -> HashMap<String, Retirement> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(legacy_jsonl_path()) else {
        return out;
    };
    for mut row in text.lines().filter_map(|l| serde_json::from_str::<Retirement>(l).ok()) {
        let id = bare(&row.agent_id).to_string();
        row.agent_id = id.clone();
        row.successor = row.successor.map(|s| bare(&s).to_string());
        if row.undone {
            out.remove(&id);
        } else {
            out.insert(id, row);
        }
    }
    out
}

/// Record `agent` as retired, continued by `successor`. Calling `retire` on an already-retired
/// agent (for example, replacing `--successor PENDING` once the replacement agent is spawned)
/// updates its retirement transactionally in place and preserves an existing `session_id` when
/// `session_id` is `None`.
#[cfg(feature = "sqlite")]
pub fn retire(agent: &str, successor: Option<&str>, session_id: Option<&str>) -> std::io::Result<Retirement> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing the retired ledger"));
    }
    let legacy = load_legacy_jsonl();
    let store = crate::state::StateStore::default_store().map_err(state_to_io)?;
    let mut conn = store.open_writable().map_err(state_to_io)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sqlite_to_io)?;

    let agent_id = bare(agent).to_string();
    let norm_succ = successor.map(|s| bare(s).to_string());
    let preserved_session = match session_id {
        Some(s) => Some(s.to_string()),
        None => {
            use rusqlite::OptionalExtension;
            let sqlite_row: Option<(Option<String>, i64)> = tx
                .query_row(
                    "SELECT session_id, undone FROM retired_agents WHERE agent_id = ?1",
                    [&agent_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sqlite_to_io)?;
            match sqlite_row {
                Some((sess, 0)) => sess,
                Some((_, _)) => None,
                None => legacy.get(&agent_id).and_then(|r| r.session_id.clone()),
            }
        }
    };

    let now = Utc::now();
    let ts_str = now.to_rfc3339();
    tx.execute(
        "INSERT INTO retired_agents(agent_id, ts, successor, session_id, undone)
         VALUES (?1, ?2, ?3, ?4, 0)
         ON CONFLICT(agent_id) DO UPDATE SET
             ts = excluded.ts,
             successor = excluded.successor,
             session_id = excluded.session_id,
             undone = 0",
        rusqlite::params![agent_id, ts_str, norm_succ, preserved_session],
    )
    .map_err(sqlite_to_io)?;

    let mut raw = legacy;
    overlay_sqlite_rows(&mut raw, &tx).map_err(sqlite_to_io)?;
    tx.commit().map_err(sqlite_to_io)?;

    if let Some(ref succ) = norm_succ {
        if !succ.eq_ignore_ascii_case("PENDING") && succ != &agent_id {
            let src_sid = format!("agent-{agent_id}");
            let dst_sid = format!("agent-{succ}");
            crate::lanes::record_lineage_edge_best_effort(
                &src_sid,
                &dst_sid,
                "retired_for",
                Some("claude"),
                None,
                serde_json::json!({
                    "agent_id": agent_id,
                    "successor": succ,
                    "session_id": preserved_session,
                }),
            );
        }
    }

    let mut resolved_map = resolve_chains(&raw);
    Ok(resolved_map.remove(&agent_id).unwrap_or(Retirement {
        ts: now,
        agent_id,
        successor: norm_succ,
        session_id: preserved_session,
        undone: false,
    }))
}

/// Reverse a retirement transactionally. Returns whether `agent` was retired.
#[cfg(feature = "sqlite")]
pub fn undo(agent: &str) -> std::io::Result<bool> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing the retired ledger"));
    }
    let legacy = load_legacy_jsonl();
    let store = crate::state::StateStore::default_store().map_err(state_to_io)?;
    let mut conn = store.open_writable().map_err(state_to_io)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sqlite_to_io)?;

    let agent_id = bare(agent);
    let ts_str = Utc::now().to_rfc3339();
    let updated = tx
        .execute(
            "UPDATE retired_agents
             SET ts = ?2, successor = NULL, session_id = NULL, undone = 1
             WHERE agent_id = ?1 AND undone = 0",
            rusqlite::params![agent_id, ts_str],
        )
        .map_err(sqlite_to_io)?;
    if updated > 0 {
        tx.commit().map_err(sqlite_to_io)?;
        return Ok(true);
    }

    use rusqlite::OptionalExtension;
    let exists_in_sqlite: Option<i64> = tx
        .query_row("SELECT 1 FROM retired_agents WHERE agent_id = ?1", [agent_id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sqlite_to_io)?;
    if exists_in_sqlite.is_none() && legacy.contains_key(agent_id) {
        tx.execute(
            "INSERT INTO retired_agents(agent_id, ts, successor, session_id, undone)
             VALUES (?1, ?2, NULL, NULL, 1)",
            rusqlite::params![agent_id, ts_str],
        )
        .map_err(sqlite_to_io)?;
        tx.commit().map_err(sqlite_to_io)?;
        return Ok(true);
    }

    tx.commit().map_err(sqlite_to_io)?;
    Ok(false)
}

#[cfg(feature = "sqlite")]
fn raw_current() -> HashMap<String, Retirement> {
    let mut out = load_legacy_jsonl();
    let Ok(store) = crate::state::StateStore::default_store() else {
        return out;
    };
    let Ok(Some(conn)) = store.open_readonly() else {
        return out;
    };
    let _ = overlay_sqlite_rows(&mut out, &conn);
    out
}

#[cfg(feature = "sqlite")]
fn overlay_sqlite_rows(out: &mut HashMap<String, Retirement>, conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("SELECT agent_id, ts, successor, session_id, undone FROM retired_agents")?;
    let rows = stmt.query_map([], |r| {
        let agent_id: String = r.get(0)?;
        let ts_raw: String = r.get(1)?;
        let ts = ts_raw
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let undone = r.get::<_, i64>(4)? != 0;
        Ok((
            agent_id.clone(),
            Retirement {
                ts,
                agent_id,
                successor: r.get(2)?,
                session_id: r.get(3)?,
                undone,
            },
        ))
    })?;
    for (id, row) in rows.flatten() {
        if row.undone {
            out.remove(&id);
        } else {
            out.insert(id, row);
        }
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
fn state_to_io(err: crate::state::StateError) -> std::io::Error {
    match err {
        crate::state::StateError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_to_io(err: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

// ───────────────────────── Portable Non-SQLite Fallback ─────────────────────────

#[cfg(not(feature = "sqlite"))]
fn append_fallback(row: &Retirement) -> std::io::Result<()> {
    if crate::env::frozen() {
        return Err(std::io::Error::other("--frozen forbids writing the retired ledger"));
    }
    use std::io::Write;
    let path = ledger_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(row).map_err(std::io::Error::other)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())
}

#[cfg(not(feature = "sqlite"))]
pub fn retire(agent: &str, successor: Option<&str>, session_id: Option<&str>) -> std::io::Result<Retirement> {
    let agent_id = bare(agent).to_string();
    let session_id = session_id
        .map(str::to_string)
        .or_else(|| raw_current().get(&agent_id).and_then(|prev| prev.session_id.clone()));
    let row = Retirement {
        ts: Utc::now(),
        agent_id,
        successor: successor.map(|s| bare(s).to_string()),
        session_id,
        undone: false,
    };
    append_fallback(&row)?;
    Ok(lookup(&row.agent_id).unwrap_or(row))
}

#[cfg(not(feature = "sqlite"))]
pub fn undo(agent: &str) -> std::io::Result<bool> {
    let agent = bare(agent);
    let was = raw_current().contains_key(agent);
    if was {
        append_fallback(&Retirement {
            ts: Utc::now(),
            agent_id: agent.to_string(),
            successor: None,
            session_id: None,
            undone: true,
        })?;
    }
    Ok(was)
}

#[cfg(not(feature = "sqlite"))]
fn raw_current() -> HashMap<String, Retirement> {
    load_legacy_jsonl()
}

/// Every agent retired now, by bare id, with transitive `A -> B -> C` successor chains resolved
/// to the live terminal successor (cycle-guarded). Empty when there is no ledger.
pub fn current() -> HashMap<String, Retirement> {
    resolve_chains(&raw_current())
}

fn resolve_chains(raw: &HashMap<String, Retirement>) -> HashMap<String, Retirement> {
    let mut out = HashMap::with_capacity(raw.len());
    for (id, mut row) in raw.iter().map(|(k, v)| (k.clone(), v.clone())) {
        let mut resolved = row.successor.clone();
        if let Some(mut cur) = resolved.clone() {
            let mut seen = HashSet::new();
            seen.insert(id.clone());
            while seen.insert(cur.clone()) {
                let Some(next_row) = raw.get(&cur) else {
                    break;
                };
                match &next_row.successor {
                    Some(next) if !seen.contains(next) => {
                        resolved = Some(next.clone());
                        cur = next.clone();
                    }
                    Some(_) | None => {
                        resolved = None;
                        break;
                    }
                }
            }
            if resolved.as_deref() == Some(id.as_str()) {
                resolved = None;
            }
        }
        row.successor = resolved;
        out.insert(id, row);
    }
    out
}

/// The retirement of `agent`, if it is retired now.
pub fn lookup(agent: &str) -> Option<Retirement> {
    current().remove(bare(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retire_lookup_undo_round_trip() {
        let _world = crate::test_world("retired-ledger");
        assert!(lookup("a1").is_none());
        retire("agent-a1", Some("agent-b2"), Some("root")).unwrap();
        let r = lookup("a1").expect("retired");
        assert_eq!(r.successor.as_deref(), Some("b2"));
        assert_eq!(r.session_id.as_deref(), Some("root"));
        assert!(
            lookup("agent-a1").is_some(),
            "the agent- prefix addresses the same agent"
        );
        assert!(lookup("b2").is_none(), "the successor is not retired");
        assert!(undo("a1").unwrap());
        assert!(lookup("a1").is_none());
        assert!(!undo("a1").unwrap(), "undoing twice reports it was not retired");
        retire("a3", None, None).unwrap();
        assert!(lookup("a3").is_some());

        #[cfg(feature = "sqlite")]
        {
            let home = crate::env::Env::current().cv_home().unwrap();
            assert!(
                home.join("state.db").exists() && !home.join("retired.jsonl").exists(),
                "retirements are backed by authoritative state.db without .jsonl sidecars"
            );
        }
    }

    #[test]
    fn transitive_chains_pending_in_place_update_and_cycle_guard() {
        let _world = crate::test_world("retired-chains");
        // Start with PENDING, then update in place without --undo; session_id is preserved.
        retire("agent-a", Some("PENDING"), Some("root-1")).unwrap();
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("PENDING"));
        retire("a", Some("agent-b"), None).unwrap();
        let a = lookup("a").unwrap();
        assert_eq!(a.successor.as_deref(), Some("b"));
        assert_eq!(a.session_id.as_deref(), Some("root-1"));

        // Transitive A -> B -> C -> D resolves every retired hop to live D.
        retire("b", Some("PENDING"), None).unwrap();
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("PENDING"));
        retire("b", Some("c"), None).unwrap();
        retire("c", Some("d"), None).unwrap();
        let all = current();
        assert_eq!(all["a"].successor.as_deref(), Some("d"));
        assert_eq!(all["b"].successor.as_deref(), Some("d"));
        assert_eq!(all["c"].successor.as_deref(), Some("d"));
        assert!(!all.contains_key("d"));

        // Terminal retirement without a successor clears stale pointers to dead intermediates.
        retire("d", None, None).unwrap();
        assert_eq!(lookup("a").unwrap().successor, None);
        assert!(undo("d").unwrap());
        assert_eq!(lookup("a").unwrap().successor.as_deref(), Some("d"));

        // Cycle guard: X -> Y -> X terminates cleanly with no live successor.
        retire("x", Some("y"), None).unwrap();
        retire("y", Some("x"), None).unwrap();
        assert_eq!(lookup("x").unwrap().successor, None);
        assert_eq!(lookup("y").unwrap().successor, None);
        retire("self", Some("self"), None).unwrap();
        assert_eq!(lookup("self").unwrap().successor, None);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn concurrent_retirements_and_frozen_mode_are_safe() {
        let dir = std::env::temp_dir().join(format!("cv-retired-conc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        std::thread::scope(|scope| {
            for i in 0..16 {
                let d = dir.clone();
                scope.spawn(move || {
                    let _env = crate::env::Env::from_vars([("CLUSTERVISION_HOME", d.as_os_str())]).enter();
                    retire(&format!("agent-w{i}"), Some("PENDING"), Some("sess-root")).unwrap();
                    retire(&format!("w{i}"), Some(&format!("succ-{i}")), None).unwrap();
                });
            }
        });

        {
            let _env = crate::env::Env::from_vars([
                ("CLUSTERVISION_HOME", dir.as_os_str()),
                ("CV_FROZEN", std::ffi::OsStr::new("1")),
            ])
            .enter();
            let all = current();
            assert_eq!(all.len(), 16);
            for i in 0..16 {
                let r = &all[&format!("w{i}")];
                assert_eq!(r.successor.as_deref(), Some(format!("succ-{i}").as_str()));
                assert_eq!(r.session_id.as_deref(), Some("sess-root"));
            }
            assert!(retire("w99", None, None).is_err());
            assert!(undo("w0").is_err());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_retired_jsonl_coexists_with_sqlite_under_frozen_chain_and_undo() {
        let world = crate::test_world("retired-legacy-jsonl-upgrade");
        let jsonl = world.cv_home.join("retired.jsonl");
        std::fs::write(
            &jsonl,
            concat!(
                "{\"ts\":\"2026-06-01T10:00:00Z\",\"agent_id\":\"agent-leg1\",\"successor\":\"agent-leg2\",\"session_id\":\"root-leg\"}\n",
                "{\"ts\":\"2026-06-01T10:05:00Z\",\"agent_id\":\"leg-reverted\",\"successor\":\"x\"}\n",
                "{\"ts\":\"2026-06-01T10:06:00Z\",\"agent_id\":\"leg-reverted\",\"undone\":true}\n",
            ),
        )
        .unwrap();

        // 1. Even under `--frozen` with NO `state.db` yet, legacy `retired.jsonl` entries surface!
        {
            let _frozen = crate::env::Env::from_vars([
                ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
                ("CV_FROZEN", std::ffi::OsStr::new("1")),
            ])
            .enter();
            let leg = lookup("leg1").expect("legacy leg1 retired");
            assert_eq!(leg.successor.as_deref(), Some("leg2"));
            assert_eq!(leg.session_id.as_deref(), Some("root-leg"));
            assert!(lookup("leg-reverted").is_none());
            assert!(!world.cv_home.join("state.db").exists());
        }

        // 2. Retiring `leg2 -> leg3` in `state.db` resolves transitive chain `leg1 -> leg2 -> leg3`
        //    without mutating `retired.jsonl`!
        let before_jsonl = std::fs::read(&jsonl).unwrap();
        retire("leg2", Some("leg3"), None).unwrap();
        assert_eq!(lookup("leg1").unwrap().successor.as_deref(), Some("leg3"));

        // 3. Updating `leg1` without `session_id` preserves `session_id` from `retired.jsonl`, and
        //    `undo` on a legacy-only entry records a tombstone in `state.db` without touching `retired.jsonl`.
        std::fs::write(
            &jsonl,
            concat!(
                "{\"ts\":\"2026-06-01T10:00:00Z\",\"agent_id\":\"agent-leg1\",\"successor\":\"agent-leg2\",\"session_id\":\"root-leg\"}\n",
                "{\"ts\":\"2026-06-01T10:10:00Z\",\"agent_id\":\"leg-only\",\"successor\":\"succ-z\",\"session_id\":\"root-z\"}\n",
            ),
        )
        .unwrap();
        let expected_jsonl = std::fs::read(&jsonl).unwrap();
        assert_ne!(before_jsonl, expected_jsonl);
        assert!(lookup("leg-only").is_some());
        assert!(undo("leg-only").unwrap());
        assert!(lookup("leg-only").is_none());
        assert!(!undo("leg-only").unwrap(), "second undo returns false");

        retire("leg1", Some("leg4"), None).unwrap();
        let updated_leg1 = lookup("leg1").unwrap();
        assert_eq!(updated_leg1.successor.as_deref(), Some("leg4"));
        assert_eq!(updated_leg1.session_id.as_deref(), Some("root-leg"));
        assert_eq!(std::fs::read(&jsonl).unwrap(), expected_jsonl);
    }
}
