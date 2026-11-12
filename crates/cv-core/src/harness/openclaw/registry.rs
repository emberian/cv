//! The state dir's registries beside the transcripts: the cron registry and the sub-agent
//! registry, read as a session's `schedule`/`state` and `subagent_spawn`/`state` rows
//! ([`crate::harness::Adapter::store_affordances`]).
//!
//! Both are keyed by **session key** (`agent:<agentId>:<kind>:<…>`), never by session id, so the
//! session's key is looked up first (`sessions.json` beside a JSONL transcript, `session_windows`
//! in the sqlite store). Layout by era, under the state dir (`agents/`'s parent):
//!
//! - **JSONL era (2026.1.29):** `cron/jobs.json` `{version, jobs: [{id, agentId, name, enabled,
//!   createdAtMs, schedule: {kind: at|cron|every, atMs|at|expr|everyMs}, sessionTarget, payload:
//!   {kind, message}, state: {lastRunAtMs, lastStatus}}]}` (+ `cron/runs/<jobId>.jsonl`, the run
//!   log), and `subagents/runs.json` `{version, runs: {<runId>: {childSessionKey,
//!   requesterSessionKey, task, label?, createdAt, endedAt?, outcome: {status}}}}`.
//! - **sqlite era (2026.10.1):** `state/openclaw.sqlite` `cron_jobs(job_json, state_json, …)`
//!   (`job_json` is the old `jobs.json` entry, plus `owner: {sessionKey}` for a job an agent
//!   made) and `subagent_runs(requester_session_key, controller_session_key, payload_json)`
//!   (`payload_json`: `task`, `label`, `execution.outcome.status`, `completion.resultText`).
//!
//! A job belongs to a session when the session made it (`owner.sessionKey`,
//! `scheduledToolPolicy.ownerSessionKey`), when the session is one of its runs
//! (`agent:<a>:cron:<jobId>[:run:<id>]`), or when it fires into the session (`sessionTarget:
//! "main"` and the key `agent:<agentId>:main`). A 1.29 job records no maker, so its maker's `cron`
//! tool call is its only trace there. A sub-agent run belongs to the session that requested or
//! controls it; the child already names its parent through `Lineage::parent`.

use crate::affordance::{Affordance, Kind};
use crate::ir::{truncate, SessionRef};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// The state dir's position above either store layout: self, `sessions|agent`, `<agent>`, `agents`,
/// then `<state>` — so both `<state>/agents/<agent>/sessions/<sid>.jsonl` and
/// `<state>/agents/<agent>/agent/<db>.sqlite` keep it at the same ancestor index.
const STATE_ANCESTOR: usize = 4;

/// Every registry row that belongs to `r`.
pub(super) fn affordances(r: &SessionRef) -> Result<Vec<Affordance>> {
    let Some(state) = r.path.ancestors().nth(STATE_ANCESTOR) else {
        return Ok(Vec::new());
    };
    let Some(key) = session_key(r)? else {
        return Ok(Vec::new());
    };
    let (jobs, runs) = if super::is_sqlite_store_path(&r.path) {
        sqlite_registries(state, &key)?
    } else {
        (jsonl_jobs(state, &key)?, jsonl_runs(state, &key)?)
    };
    let mut out: Vec<Affordance> = jobs.iter().filter(|j| job_belongs(j, &key)).map(job_row).collect();
    out.extend(runs.iter().filter(|(by, _)| by.contains(&key)).map(|(_, p)| run_row(p)));
    Ok(out)
}

/// Every session's certificate in the JSONL registries a transcript in `dir` reads: exactly what
/// its read takes from them. From `dir/sessions.json`: the key [`session_key`] finds (the first
/// entry naming it, in the parsed map's order) and, as [`super::JsonKeys`] reads the document,
/// its own entry's key and `spawnedBy` and the session that spawner key names now; from the state
/// dir's `cron/jobs.json` and `subagents/runs.json`, the jobs and runs that belong to its key
/// ([`jsonl_jobs`], [`jsonl_runs`]); a registry that fails to read, as the failure. Keyed by the
/// session ids `sessions.json` names and the transcripts' file names.
pub(super) fn jsonl_rows(dir: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
    use crate::source::Rows;
    let state = dir.ancestors().nth(STATE_ANCESTOR - 1)?;
    let index_path = dir.join("sessions.json");
    let index = shared_json(&index_path);
    let keys = super::JsonKeys::read(&index_path);
    let entries = match &index {
        Ok(Some(index)) => index.as_object(),
        _ => None,
    };
    // The sessions `sessions.json` names, and every transcript's by its file name (a session it
    // does not name reads no entry: that absence is its certificate).
    let on_disk: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(super::is_primary_transcript_name))
        .map(|e| super::session_id_from_filename(&e.path()))
        .collect();
    let mut ids: std::collections::BTreeSet<&str> = keys.sessions.keys().map(String::as_str).collect();
    ids.extend(
        entries
            .into_iter()
            .flatten()
            .filter_map(|(_, e)| e.get("sessionId").and_then(Value::as_str)),
    );
    ids.extend(on_disk.iter().map(String::as_str));
    let fold = |digest: u64, part: &[u8]| Rows::fold(Rows::fold(digest, &(part.len() as u64).to_le_bytes()), part);
    let json = |value: &Value| serde_json::to_vec(value).unwrap_or_default();
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let mut digest = Rows::SEED;
        let key = match &index {
            Err(error) => {
                digest = fold(digest, format!("error {error:#}").as_bytes());
                None
            }
            Ok(None) => None,
            Ok(Some(_)) => match entries {
                None => {
                    digest = fold(digest, b"not an object");
                    None
                }
                Some(entries) => entries
                    .iter()
                    .find(|(_, e)| e.get("sessionId").and_then(Value::as_str) == Some(id))
                    .map(|(k, _)| k.clone()),
            },
        };
        digest = fold(digest, key.as_deref().unwrap_or("\0").as_bytes());
        if let Some((own, spawned_by)) = keys.sessions.get(id) {
            digest = fold(digest, own.as_bytes());
            if let Some(by) = spawned_by {
                digest = fold(digest, by.as_bytes());
                digest = fold(digest, keys.preferred.get(by).map_or(&b"\0"[..], |s| s.as_bytes()));
            }
        }
        if let Some(key) = &key {
            match jsonl_jobs(state, key) {
                Ok(jobs) => jobs.iter().for_each(|job| digest = fold(digest, &json(job))),
                Err(error) => digest = fold(digest, format!("error {error:#}").as_bytes()),
            }
            match jsonl_runs(state, key) {
                Ok(runs) => runs.iter().for_each(|(_, run)| digest = fold(digest, &json(run))),
                Err(error) => digest = fold(digest, format!("error {error:#}").as_bytes()),
            }
        }
        out.insert(
            id.to_owned(),
            Rows {
                count: 1,
                high: 0,
                digest,
            },
        );
    }
    Some(out)
}

/// The session's key: the `sessions.json` entry naming it, or its `session_windows` row.
fn session_key(r: &SessionRef) -> Result<Option<String>> {
    if super::is_sqlite_store_path(&r.path) {
        return sqlite_session_key(r);
    }
    let Some(parent) = r.path.parent() else { return Ok(None) };
    let Some(index) = shared_json(&parent.join("sessions.json"))? else {
        return Ok(None);
    };
    let index = index
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("sessions.json is not an object"))?;
    Ok(index
        .iter()
        .find(|(_, e)| e.get("sessionId").and_then(Value::as_str) == Some(r.id.as_str()))
        .map(|(k, _)| k.clone()))
}

#[cfg(feature = "sqlite")]
fn sqlite_session_key(r: &SessionRef) -> Result<Option<String>> {
    use rusqlite::OptionalExtension as _;
    let conn = super::open_ro(&r.path)?;
    conn.read(|conn, _| {
        Ok(conn
            .query_row(
                "SELECT session_key FROM session_windows WHERE session_id = ?1",
                [&r.id],
                |row| row.get(0),
            )
            .optional()?)
    })
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_session_key(_r: &SessionRef) -> Result<Option<String>> {
    anyhow::bail!("SQLite support is required for the session registry")
}

/// A registry file's parse, shared while the file stats as it did when it was parsed. Every
/// session's affordances consult the same `sessions.json`, `cron/jobs.json` and
/// `subagents/runs.json`; parsing them per session made a pass over n sessions cost
/// n × the registries. Errors are not kept: the next ask reads and reports again.
fn shared_json(path: &Path) -> Result<Option<std::sync::Arc<Value>>> {
    use crate::source::FileId;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    type Memo = HashMap<PathBuf, (FileId, Arc<Value>)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    let before = FileId::at(path);
    if let Some(before) = before {
        if let Some((_, hit)) = memo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .filter(|(seen, _)| *seen == before)
        {
            return Ok(Some(Arc::clone(hit)));
        }
    }
    let Some(value) = crate::affordance::side_json(path)? else {
        return Ok(None);
    };
    let value = Arc::new(value);
    // A write during the read stores nothing: the next ask parses the file as it then stands.
    if let Some(before) = before.filter(|before| FileId::at(path) == Some(*before)) {
        memo.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (before, Arc::clone(&value)));
    }
    Ok(Some(value))
}

/// The cron jobs that belong to `key`, in registry order.
fn jsonl_jobs(state: &Path, key: &str) -> Result<Vec<Value>> {
    let Some(registry) = shared_json(&state.join("cron").join("jobs.json"))? else {
        return Ok(Vec::new());
    };
    Ok(match registry.get("jobs") {
        Some(Value::Array(jobs)) => jobs.iter().filter(|j| job_belongs(j, key)).cloned().collect(),
        _ => Vec::new(),
    })
}

/// A sub-agent run: the keys it belongs to (requester, controller) and its record.
type Run = (Vec<String>, Value);

fn run_keys(p: &Value) -> Vec<String> {
    ["requesterSessionKey", "controllerSessionKey"]
        .iter()
        .filter_map(|k| p.get(*k).and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn jsonl_runs(state: &Path, key: &str) -> Result<Vec<Run>> {
    let Some(registry) = shared_json(&state.join("subagents").join("runs.json"))? else {
        return Ok(Vec::new());
    };
    let Some(Value::Object(runs)) = registry.get("runs") else {
        return Ok(Vec::new());
    };
    // Only this session's runs need ordering. Filtering before sorting avoids ordering
    // the full global registry again for every requested session.
    let mut out: Vec<Run> = runs
        .values()
        .filter_map(|p| {
            let keys = run_keys(p);
            keys.iter().any(|by| by == key).then(|| (keys, p.clone()))
        })
        .collect();
    out.sort_by_key(|(_, p)| p.get("createdAt").and_then(Value::as_i64).unwrap_or(0));
    Ok(out)
}

#[cfg(feature = "sqlite")]
fn sqlite_registries(state: &Path, key: &str) -> Result<(Vec<Value>, Vec<Run>)> {
    let path = state.join("state").join("openclaw.sqlite");
    if !path.try_exists()? {
        return Ok(Default::default());
    }
    let all = sqlite_rows(&path)?;
    let mut jobs = Vec::new();
    for (job, state) in &all.jobs {
        if !job_belongs(job, key) {
            continue;
        }
        // Read as text and parsed for a belonging job only, as before: another job's state
        // cannot fail this one.
        let state: Value = serde_json::from_str(rusqlite::types::ValueRef::from(state).as_str()?)?;
        let mut job = job.clone();
        if let Some(object) = job.as_object_mut() {
            object.insert("state".into(), state);
        }
        jobs.push(job);
    }
    let runs = all
        .runs
        .iter()
        .filter(|(keys, _)| keys.iter().any(|by| by == key))
        .cloned()
        .collect();
    Ok((jobs, runs))
}

/// Every `cron_jobs` row (its parsed job and raw state cell) and `subagent_runs` row of one store, in
/// query order.
#[cfg(feature = "sqlite")]
#[derive(Default)]
struct Rows {
    jobs: Vec<(Value, rusqlite::types::Value)>,
    runs: Vec<Run>,
}

/// [`Rows`] of the store at `path`, kept while the store and its WAL and journal stat as they did
/// when they were read (the pattern of Hermes's delegate scan). Each session's affordances ask; a
/// scan per session parsed every job and run once per session of the store.
#[cfg(feature = "sqlite")]
fn sqlite_rows(path: &Path) -> Result<std::sync::Arc<Rows>> {
    use super::super::store::sqlite::table_exists;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    type Memo = HashMap<PathBuf, (crate::source::ReadSet, Arc<Rows>)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    let conn = super::open_ro(path)?;
    conn.read(|conn, reads| {
        let hit = memo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .filter(|(seen, _)| seen.fresh(&mut crate::source::Probe::default()))
            .map(|(_, rows)| Arc::clone(rows));
        if let Some(rows) = hit {
            return Ok(rows);
        }
        let mut all = Rows::default();
        if table_exists(conn, "cron_jobs") {
            let mut stmt =
                conn.prepare("SELECT job_json, state_json FROM cron_jobs ORDER BY sort_order, updated_at, job_id")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let job: Value = serde_json::from_str(row.get_ref(0)?.as_str()?)?;
                let state: rusqlite::types::Value = row.get(1)?;
                all.jobs.push((job, state));
            }
        }
        if table_exists(conn, "subagent_runs") {
            let mut stmt = conn.prepare("SELECT payload_json FROM subagent_runs ORDER BY created_at, run_id")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let payload: Value = serde_json::from_str(row.get_ref(0)?.as_str()?)?;
                all.runs.push((run_keys(&payload), payload));
            }
        }
        let all = Arc::new(all);
        memo.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (reads.clone(), Arc::clone(&all)));
        Ok(all)
    })
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_registries(_state: &Path, _key: &str) -> Result<(Vec<Value>, Vec<Run>)> {
    anyhow::bail!("SQLite support is required for the side registries")
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn job_belongs(job: &Value, key: &str) -> bool {
    let owner = ["/owner/sessionKey", "/scheduledToolPolicy/ownerSessionKey"]
        .iter()
        .any(|p| str_at(job, p) == Some(key));
    let id = str_at(job, "/id").unwrap_or("");
    // `agent:<agentId>:<kind>:<…>`: the agent, the kind, and the first field after it.
    let fields: Vec<&str> = key.split(':').collect();
    let (agent, kind, rest) = (fields.get(1).copied(), fields.get(2).copied(), fields.get(3).copied());
    let run = kind == Some("cron") && !id.is_empty() && rest == Some(id);
    let main = str_at(job, "/sessionTarget") == Some("main")
        && kind == Some("main")
        && agent.is_some()
        && str_at(job, "/agentId") == agent;
    owner || run || main
}

/// The public summary's bound.
const SUMMARY: usize = 120;
/// What a [`Head`](crate::projection::Head) of the summary keeps: its bound and one lookahead.
fn summary_prefix<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .flat_map(str::chars)
        .take(crate::projection::Head::kept(SUMMARY))
        .collect()
}

/// `corpus-ping · at 2026-10-11T20:48:42Z · Reply with the word pong. · last ok`.
fn job_row(job: &Value) -> Affordance {
    let when = match str_at(job, "/schedule/kind") {
        Some("cron") => str_at(job, "/schedule/expr").map(|s| summary_prefix([s])),
        Some("every") => job
            .pointer("/schedule/everyMs")
            .and_then(Value::as_i64)
            .map(|ms| format!("every {}s", ms / 1000)),
        Some("at") => str_at(job, "/schedule/at")
            .map(|a| summary_prefix(["at ", a]))
            .or_else(|| {
                job.pointer("/schedule/atMs")
                    .and_then(Value::as_i64)
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|t| format!("at {}", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
            }),
        _ => None,
    };
    let name = str_at(job, "/displayName").or_else(|| str_at(job, "/name"));
    let message = str_at(job, "/payload/message").or_else(|| str_at(job, "/payload/kind"));
    let last = str_at(job, "/state/lastStatus").map(|s| summary_prefix(["last ", s]));
    let disabled = (job.get("enabled").and_then(Value::as_bool) == Some(false)).then_some("disabled");
    let parts: Vec<&str> = [name, when.as_deref(), message, last.as_deref(), disabled]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    let prefix = summary_prefix(
        parts
            .iter()
            .enumerate()
            .flat_map(|(i, part)| [if i == 0 { "" } else { " · " }, *part]),
    );
    Affordance::store(Kind::Schedule, "cron", truncate(&prefix, SUMMARY), Vec::new())
}

/// `ok-reply-check · Reply with the single word ok. · ok → ok`.
fn run_row(p: &Value) -> Affordance {
    let who = str_at(p, "/label")
        .or_else(|| str_at(p, "/childSessionKey"))
        .unwrap_or("");
    let task = str_at(p, "/task").unwrap_or("");
    let status = str_at(p, "/execution/outcome/status").or_else(|| str_at(p, "/outcome/status"));
    let mut parts = vec![who, " · ", task];
    if let Some(s) = status {
        parts.extend([" · ", s]);
    }
    if let Some(res) = str_at(p, "/completion/resultText") {
        parts.extend([" → ", res]);
    }
    let summary = summary_prefix(parts);
    let timestamp = p
        .get("createdAt")
        .and_then(Value::as_i64)
        .and_then(chrono::DateTime::from_timestamp_millis);
    Affordance {
        timestamp,
        ..Affordance::store(
            Kind::SubagentSpawn,
            "subagents",
            truncate(&summary, SUMMARY),
            Vec::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// The three ways a job belongs to a session: it made the job, it is one of the job's runs, or
    /// the job fires into it.
    #[test]
    fn a_job_belongs_to_its_owner_its_runs_and_its_main_target() {
        let made = json!({"id": "j1", "owner": {"sessionKey": "agent:main:explicit:s1"}, "sessionTarget": "isolated"});
        assert!(job_belongs(&made, "agent:main:explicit:s1"));
        assert!(job_belongs(&made, "agent:main:cron:j1"));
        assert!(job_belongs(&made, "agent:main:cron:j1:run:r9"));
        assert!(!job_belongs(&made, "agent:main:cron:j2"));
        assert!(!job_belongs(&made, "agent:main:main"));
        let heartbeat = json!({"id": "hb", "agentId": "main", "sessionTarget": "main"});
        assert!(job_belongs(&heartbeat, "agent:main:main"));
        assert!(!job_belongs(&heartbeat, "agent:other:main"));
        let every = json!({"name": "hb", "schedule": {"kind": "every", "everyMs": 1_800_000}, "payload": {"kind": "heartbeat"}});
        assert_eq!(job_row(&every).summary, "hb · every 1800s · heartbeat");
    }

    #[test]
    fn jsonl_run_order_and_ownership_match_global_sort_with_many_unrelated_runs() -> anyhow::Result<()> {
        let world = crate::test_world("openclaw-owned-run-order");
        fs::create_dir_all(world.base.join("subagents"))?;
        let key = "agent:main:explicit:requested";
        let mut runs = serde_json::Map::new();
        for i in 0..5000 {
            runs.insert(
                format!("other-{i}"),
                json!({"requesterSessionKey":"unrelated",
            "createdAt":5000-i,"task":"unrelated"}),
            );
        }
        for (id, payload) in [
            ("late", json!({"requesterSessionKey":key,"createdAt":9})),
            ("early", json!({"controllerSessionKey":key,"createdAt":-9})),
            ("tie-a", json!({"requesterSessionKey":key,"createdAt":0})),
            (
                "tie-b",
                json!({"requesterSessionKey":"other","controllerSessionKey":key,"createdAt":0}),
            ),
            (
                "invalid",
                json!({"requesterSessionKey":false,"controllerSessionKey":1,"createdAt":-100}),
            ),
            ("missing-time", json!({"requesterSessionKey":key,"createdAt":"invalid"})),
        ] {
            runs.insert(id.into(), payload);
        }
        let mut expected: Vec<Run> = runs.values().cloned().map(|p| (run_keys(&p), p)).collect();
        expected.sort_by_key(|(_, p)| p.get("createdAt").and_then(Value::as_i64).unwrap_or(0));
        expected.retain(|(keys, _)| keys.iter().any(|by| by == key));
        fs::write(world.base.join("subagents/runs.json"), json!({"runs":runs}).to_string())?;
        let actual = jsonl_runs(&world.base, key).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 5);
        Ok(())
    }

    #[test]
    fn registry_summaries_match_full_rendering_for_large_unicode_fields() {
        let giant = "雪λ\n".repeat(50000);
        for name in ["", "short", giant.as_str()] {
            for payload in ["", "short", giant.as_str()] {
                let job = json!({"name":name,"schedule":{"kind":"at","at":giant},
                    "payload":{"message":payload},"state":{"lastStatus":giant},"enabled":false});
                let expected = [
                    name.to_owned(),
                    format!("at {giant}"),
                    payload.to_owned(),
                    format!("last {giant}"),
                    "disabled".into(),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
                assert_eq!(job_row(&job).summary, truncate(&expected, 120));
                let run = json!({"label":name,"task":payload,"outcome":{"status":giant},
                    "completion":{"resultText":giant},"createdAt":1704067200000_i64});
                assert_eq!(
                    run_row(&run).summary,
                    truncate(&format!("{name} · {payload} · {giant} → {giant}"), 120)
                );
                assert_eq!(
                    run_row(&run).timestamp,
                    chrono::DateTime::from_timestamp_millis(1704067200000)
                );
            }
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_filters_match_payload_ownership_and_keep_row_order() {
        let state = std::env::temp_dir().join(format!("cv-openclaw-registry-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(state.join("state")).unwrap();
        let conn = rusqlite::Connection::open(state.join("state/openclaw.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE cron_jobs(job_json TEXT, state_json TEXT, sort_order INT, updated_at INT, job_id TEXT);\
             CREATE TABLE subagent_runs(payload_json TEXT, created_at INT, run_id TEXT, requester_session_key TEXT, controller_session_key TEXT);",
        ).unwrap();
        let mut jobs = vec![
            json!({"id":"j1", "owner":{"sessionKey":"agent:main:explicit:s1"}}),
            json!({"id":"hb", "agentId":"main", "sessionTarget":"main"}),
            json!({"id":"j2", "scheduledToolPolicy":{"ownerSessionKey":"agent:main:explicit:s1"}}),
            json!({"id":"", "owner":{"sessionKey":""}}),
            json!({"owner":{"sessionKey":1}, "agentId":1, "sessionTarget":"main"}),
            json!({"id":"orphan", "owner":{"sessionKey":"another-session"}}),
        ];
        let mut runs = vec![
            json!({"requesterSessionKey":"agent:main:explicit:s1", "task":"requested"}),
            json!({"controllerSessionKey":"agent:main:explicit:s1", "task":"controlled"}),
            json!({"requesterSessionKey":1, "controllerSessionKey":false}),
            json!({"requesterSessionKey":"another-session", "task":"unrelated"}),
        ];
        let raw_jobs = [
            r#"{"id":"duplicate-owner","owner":{"sessionKey":"other"},"owner":{"sessionKey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"duplicate-session","owner":{"sessionKey":"other","sessionKey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"duplicate-agent","sessionTarget":"main","agentId":"other","agentId":"main"}"#,
            r#"{"id":"escaped-owner","\u006fwner":{"session\u004bey":"agent:main:explicit:s1"}}"#,
            r#"{"id":"nested-unrelated","payload":{"owner":{"sessionKey":"agent:main:explicit:s1"}}}"#,
        ];
        let raw_runs = [
            r#"{"requesterSessionKey":"other","requesterSessionKey":"agent:main:explicit:s1"}"#,
            r#"{"controllerSessionKey":"other","controllerSessionKey":"agent:main:explicit:s1"}"#,
            r#"{"requesterSession\u004bey":"agent:main:explicit:s1"}"#,
            r#"{"payload":{"requesterSessionKey":"agent:main:explicit:s1"}}"#,
        ];
        let job_text: Vec<String> = jobs
            .iter()
            .map(Value::to_string)
            .chain(raw_jobs.iter().map(|s| s.to_string()))
            .collect();
        let run_text: Vec<String> = runs
            .iter()
            .map(Value::to_string)
            .chain(raw_runs.iter().map(|s| s.to_string()))
            .collect();
        jobs.extend(raw_jobs.iter().map(|s| serde_json::from_str::<Value>(s).unwrap()));
        runs.extend(raw_runs.iter().map(|s| serde_json::from_str::<Value>(s).unwrap()));
        for (i, job) in job_text.iter().enumerate() {
            conn.execute(
                "INSERT INTO cron_jobs VALUES (?1, ?2, ?3, 0, ?4)",
                rusqlite::params![job, r#"{"lastStatus":"ok"}"#, i, format!("job{i}")],
            )
            .unwrap();
        }
        for (i, run) in run_text.iter().enumerate() {
            // A relational projection disagrees with JSON. Preserve the original JSON-based
            // ownership contract instead of losing relevant runs or accepting unrelated ones.
            conn.execute(
                "INSERT INTO subagent_runs VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![run, i, format!("run{i}"), "wrong-projection"],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO cron_jobs VALUES ('malformed', '{}', 99, 0, 'bad')", [])
            .unwrap();
        conn.execute("INSERT INTO subagent_runs VALUES ('malformed', 99, 'bad', '', '')", [])
            .unwrap();
        assert!(sqlite_registries(&state, "agent:main:explicit:s1").is_err());
        conn.execute("DELETE FROM cron_jobs WHERE job_id = 'bad'", []).unwrap();
        assert!(sqlite_registries(&state, "agent:main:explicit:s1").is_err());
        conn.execute("DELETE FROM subagent_runs WHERE run_id = 'bad'", [])
            .unwrap();
        for key in [
            "agent:main:explicit:s1",
            "agent:main:main",
            "agent:other:main",
            "agent:main:cron:j1:run:r9",
            "1",
            "",
        ] {
            let (actual_jobs, actual_runs) = sqlite_registries(&state, key).unwrap();
            let expected_jobs: Vec<Value> = jobs
                .iter()
                .filter(|j| job_belongs(j, key))
                .cloned()
                .map(|mut j| {
                    j.as_object_mut()
                        .unwrap()
                        .insert("state".into(), json!({"lastStatus":"ok"}));
                    j
                })
                .collect();
            let expected_runs: Vec<Run> = runs
                .iter()
                .filter(|r| run_keys(r).iter().any(|k| k == key))
                .cloned()
                .map(|p| (run_keys(&p), p))
                .collect();
            assert_eq!(actual_jobs, expected_jobs, "{key}");
            assert_eq!(actual_runs, expected_runs, "{key}");
        }
        fs::remove_dir_all(state).unwrap();
    }

    /// Every session of an agent asks for the same registries: each is parsed once per file
    /// state, and a rewrite is read.
    #[test]
    fn registries_are_parsed_once_per_file_state() -> Result<()> {
        let world = crate::test_world("openclaw-registry-shared");
        let state = world.base.join("state");
        let sessions = state.join("agents/main/sessions");
        fs::create_dir_all(&sessions)?;
        fs::create_dir_all(state.join("cron"))?;
        fs::create_dir_all(state.join("subagents"))?;
        let mut index = serde_json::Map::new();
        for i in 0..3 {
            index.insert(
                format!("agent:main:main:{i}"),
                serde_json::json!({"sessionId": format!("s{i}")}),
            );
            fs::write(sessions.join(format!("s{i}.jsonl")), "")?;
        }
        fs::write(sessions.join("sessions.json"), Value::Object(index).to_string())?;
        fs::write(
            state.join("cron/jobs.json"),
            r#"{"jobs":[{"id":"j","name":"ping","owner":{"sessionKey":"agent:main:main:1"}}]}"#,
        )?;
        let runs = |label: &str| {
            fs::write(
                state.join("subagents/runs.json"),
                format!(
                    r#"{{"runs":{{"r":{{"requesterSessionKey":"agent:main:main:2","task":"t","label":"{label}"}}}}}}"#
                ),
            )
        };
        runs("first")?;
        let rows = |i: usize| {
            let r = SessionRef::new(
                format!("s{i}"),
                crate::Harness::OpenClaw,
                sessions.join(format!("s{i}.jsonl")),
            );
            affordances(&r).map(|rows| rows.iter().map(|a| a.summary.clone()).collect::<Vec<_>>())
        };
        let parsed = |name: &str| shared_json(&state.join(name)).map(|v| v.unwrap());
        assert_eq!(rows(0)?, Vec::<String>::new());
        assert_eq!(rows(1)?, vec!["ping".to_string()]);
        assert_eq!(rows(2)?, vec!["first · t".to_string()]);
        let (jobs, before) = (parsed("cron/jobs.json")?, parsed("subagents/runs.json")?);
        rows(0)?;
        assert!(
            std::sync::Arc::ptr_eq(&jobs, &parsed("cron/jobs.json")?),
            "unchanged: not reparsed"
        );
        assert!(std::sync::Arc::ptr_eq(&before, &parsed("subagents/runs.json")?));
        runs("second")?;
        assert!(
            !std::sync::Arc::ptr_eq(&before, &parsed("subagents/runs.json")?),
            "a rewrite is read"
        );
        assert_eq!(rows(2)?, vec!["second · t".to_string()]);
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_registry_rows_are_read_once_per_store_state() -> Result<()> {
        let world = crate::test_world("openclaw-registry-sqlite");
        let state = world.base.join("state");
        let path = state.join("state/openclaw.sqlite");
        fs::create_dir_all(path.parent().unwrap())?;
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(
            r#"CREATE TABLE cron_jobs(job_id TEXT, job_json TEXT, state_json, sort_order INT, updated_at INT);
               CREATE TABLE subagent_runs(run_id TEXT, payload_json TEXT, created_at INT);
               INSERT INTO cron_jobs VALUES('a', '{"id":"a","name":"mine","owner":{"sessionKey":"k1"}}', '{"lastStatus":"ok"}', 0, 0);
               INSERT INTO cron_jobs VALUES('b', '{"id":"b","name":"theirs","owner":{"sessionKey":"k2"}}', NULL, 1, 0);
               INSERT INTO subagent_runs VALUES('r', '{"requesterSessionKey":"k2","task":"t"}', 0);"#,
        )?;
        let (jobs, runs) = sqlite_registries(&state, "k1")?;
        assert_eq!(jobs.len(), 1, "another job's NULL state does not fail this one");
        assert_eq!(jobs[0]["state"]["lastStatus"], "ok");
        assert!(runs.is_empty());
        assert!(
            sqlite_registries(&state, "k2").is_err(),
            "its own NULL state still fails, as before"
        );
        let first = sqlite_rows(&path)?;
        assert!(
            std::sync::Arc::ptr_eq(&first, &sqlite_rows(&path)?),
            "unchanged: not reread"
        );
        conn.execute(
            "INSERT INTO subagent_runs VALUES('s', '{\"requesterSessionKey\":\"k1\",\"task\":\"u\"}', 1)",
            [],
        )?;
        assert!(!std::sync::Arc::ptr_eq(&first, &sqlite_rows(&path)?), "a write is read");
        assert_eq!(sqlite_registries(&state, "k1")?.1.len(), 1);
        Ok(())
    }
}
