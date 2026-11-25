//! Cold publication of a selected trajectory under a fresh identity in a private app-data root.
mod proto;
use super::{native, BrainLoc};
use crate::harness::store::{bytes::ByteSource, sqlite};
use crate::ir::{Harness, MessageKind, Session, SessionRef};
use crate::publish::{Claim, StagedDir};
use crate::rewind::{CutAt, RewindOptions};
use crate::uncompact::Plan;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, Transaction};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "
CREATE TABLE trajectory_meta(trajectory_id TEXT PRIMARY KEY,cascade_id TEXT,trajectory_type INTEGER,source INTEGER);
CREATE TABLE steps(idx INTEGER PRIMARY KEY,step_type INTEGER NOT NULL DEFAULT 0,status INTEGER NOT NULL DEFAULT 0,
 has_subtrajectory NUMERIC NOT NULL DEFAULT false,metadata BLOB,error_details BLOB,permissions BLOB,task_details BLOB,
 render_info BLOB,step_payload BLOB,step_format INTEGER NOT NULL DEFAULT 0);
CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY,data BLOB,size INTEGER NOT NULL DEFAULT 0);
CREATE TABLE trajectory_metadata_blob(id TEXT PRIMARY KEY DEFAULT 'main',data BLOB);
CREATE TABLE executor_metadata(idx INTEGER PRIMARY KEY,data BLOB);
CREATE TABLE parent_references(idx INTEGER PRIMARY KEY,data BLOB);
CREATE TABLE battle_mode_infos(idx INTEGER PRIMARY KEY,data BLOB);";

// The index stores the status name and -1 for an absent user step. The raw
// CascadeTrajectorySummary stores its enum number and omits the optional index.
const SUMMARY_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS conversation_summaries(
 conversation_id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', preview TEXT NOT NULL DEFAULT '',
 workspace_uris TEXT NOT NULL,parent_conversation_id TEXT NOT NULL DEFAULT '',agent_name TEXT NOT NULL DEFAULT '',
 source TEXT NOT NULL DEFAULT '',project_id TEXT NOT NULL DEFAULT '',last_modified_time DATETIME NOT NULL,
 killed NUMERIC NOT NULL DEFAULT false,status TEXT NOT NULL DEFAULT '',group_id TEXT NOT NULL DEFAULT '',
 step_count INTEGER NOT NULL DEFAULT 0,last_user_input_step_index INTEGER NOT NULL DEFAULT -1,
 last_user_input_time DATETIME NOT NULL,not_fully_idle NUMERIC NOT NULL DEFAULT false,
 nesting_depth INTEGER NOT NULL DEFAULT 0,raw_summary BLOB)";

#[derive(Serialize)]
pub struct Derived {
    pub source_id: String,
    pub new_id: String,
    pub trajectory_id: String,
    pub new_path: Option<PathBuf>,
    pub selected_steps: Vec<u64>,
    pub generations_kept: usize,
    pub generations_omitted: usize,
    pub executor_rows_omitted: usize,
    pub last_user_input_step_index: Option<u64>,
    pub recovery: Option<Plan>,
    pub dry_run: bool,
}

pub fn rewind(reference: &SessionRef, opts: &RewindOptions, recover: bool, ordinal: Option<usize>) -> Result<Derived> {
    let loc = BrainLoc::from_transcript(&reference.path)
        .context("expected an Antigravity conversation or brain transcript")?;
    let db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
    let mut reads = crate::source::ReadSet::default();
    super::record_sqlite_readset(&db, &mut reads);
    reads.record(&reference.path);
    let adapter = crate::harness::rootless(Harness::Antigravity).context("Antigravity adapter unavailable")?;
    let (session, _, grown) = crate::emit::prepare_selected(adapter, reference, 0, None)?;
    ensure!(grown.is_none(), "source grew during selection");
    use crate::coverage::ReadCoverage;
    ensure!(
        !ReadCoverage::of_session(&session).is_some_and(|c| c.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY)),
        "native and exported history disagree; select an authoritative native source before deriving"
    );
    let plan = recover
        .then(|| Plan::select(reference, &session, ordinal))
        .transpose()?;
    if let Some(plan) = &plan {
        plan.ensure_closed()?
    }
    let step_at = |i: usize| -> Result<u64> {
        let m = session.messages.get(i).context("message index outside source")?;
        let id = m.id.as_ref().or_else(|| {
            (m.kind == MessageKind::CompactionBoundary)
                .then(|| session.messages.get(i + 1))
                .flatten()
                .and_then(|m| m.id.as_ref())
        });
        id.context("message has no native step identity")?
            .parse()
            .context("invalid native step identity")
    };
    let cut = match (&opts.at, &plan) {
        (CutAt::End, Some(p)) => Some(step_at(p.boundary_message)?),
        (CutAt::End, None) => None,
        (CutAt::Message(i), None) => Some(step_at(*i)?),
        (CutAt::Commit(sha), None) => {
            let evidence = crate::rewind::commit_evidence(reference, std::slice::from_ref(sha))?;
            let hit =
                crate::rewind::CommitEvidenceSink::best(&evidence, sha).context("no recorded commit matches cut")?;
            Some(step_at(hit.msg_idx)?)
        }
        _ => anyhow::bail!("--at conflicts with --uncompact"),
    };
    let mut start = 0;
    if !recover && !opts.full {
        for (i, _) in session
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.kind == MessageKind::CompactionBoundary)
        {
            let boundary = step_at(i)?;
            if cut.is_none_or(|end| boundary <= end) {
                start = boundary;
            }
        }
    }
    let mut skip = BTreeSet::new();
    let mut required = BTreeSet::new();
    if let Some(p) = &plan {
        for i in 0..session.messages.len() {
            if p.messages.contains(&i) {
                required.insert(step_at(i)?);
            } else if i <= p.boundary_message {
                skip.insert(step_at(i)?);
            }
        }
    }
    let new_id = opts.new_id.clone().unwrap_or_else(crate::prune::new_uuid);
    crate::publish::validate_output_id(&new_id)?;
    ensure!(
        super::looks_like_uuid(&new_id),
        "Antigravity conversation IDs must be UUIDs"
    );
    ensure!(new_id != loc.id, "new id must differ from source");
    let out = opts.out_dir.as_deref();
    if !opts.dry_run && out.is_none() {
        let active = ["ANTIGRAVITY_LS_ADDRESS", "ANTIGRAVITY_CSRF_TOKEN"]
            .iter()
            .any(|key| crate::env::var_os(key).is_some_and(|v| !v.is_empty()));
        if active {
            anyhow::bail!("active Antigravity language server; use --out for an offline successor");
        }
        anyhow::bail!("cold Antigravity publication requires --out in a private offline root because the language server owns the normal index");
    }
    let stage = if opts.dry_run {
        None
    } else {
        let out = out.context("missing output directory")?;
        // The staging directory must not be walked back into the closure. Also
        // reject a symlink alias of the source root before creating anything.
        let absolute = std::path::absolute(out)?;
        if let Some(ancestor) = absolute.ancestors().find(|p| p.exists()) {
            if loc.brain_dir.exists() {
                ensure!(
                    !ancestor.canonicalize()?.starts_with(loc.brain_dir.canonicalize()?),
                    "--out is inside the source brain closure"
                );
            }
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        ensure!(!out.exists() || out.is_dir(), "--out must be an app-data directory");
        if let Some(configured) = super::non_empty_env_path("ANTIGRAVITY_APP_DATA_DIR")
            .or_else(|| super::non_empty_env_path("JETSKI_APP_DATA_DIR"))
        {
            ensure!(
                !super::same_app_dir(out, &configured),
                "--out names the configured language server root"
            );
        }
        if out.exists() {
            ensure!(
                out.canonicalize()? != loc.app_data_dir.canonicalize()?,
                "--out must be separate from the source app-data root"
            );
        }
        Some(StagedDir::beside(out, "antigravity")?)
    };
    let relative = PathBuf::from("conversations").join(format!("{new_id}.db"));
    let mut dst = if let Some(stage) = &stage {
        fs::create_dir(stage.path().join("conversations"))?;
        Connection::open(stage.path().join(&relative))?
    } else {
        Connection::open_in_memory()?
    };
    let tx = dst.transaction()?;
    tx.execute_batch(SCHEMA)?;
    let mut report = Derived {
        source_id: loc.id.clone(),
        new_path: out.map(|o| o.join(&relative)),
        new_id,
        trajectory_id: crate::prune::new_uuid(),
        selected_steps: Vec::new(),
        generations_kept: 0,
        generations_omitted: 0,
        executor_rows_omitted: 0,
        last_user_input_step_index: None,
        recovery: plan,
        dry_run: opts.dry_run,
    };
    sqlite::open_ro(&db)?.read_snapshot(|src, _| {
        ensure!(
            src.query_row("SELECT count(*) FROM trajectory_meta", [], |r| r.get::<_, u64>(0))? == 1,
            "ambiguous trajectory identity"
        );
        let (tid, cid, ty, source): (String, String, i64, i64) = src.query_row(
            "SELECT trajectory_id,cascade_id,trajectory_type,source FROM trajectory_meta",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        ensure!(cid == loc.id, "source cascade does not match conversation path");
        tx.execute(
            "INSERT INTO trajectory_meta VALUES(?,?,?,?)",
            params![report.trajectory_id, report.new_id, ty, source],
        )?;
        let mut blobs = src.prepare("SELECT id,data FROM trajectory_metadata_blob")?;
        for row in blobs.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
            let (id, data) = row?;
            tx.execute(
                "INSERT INTO trajectory_metadata_blob VALUES(?,?)",
                params![id, proto::trajectory(&data, &cid, &report.new_id)?],
            )?;
        }
        let mut steps = src.prepare("SELECT idx FROM steps ORDER BY idx")?;
        for (expected, row) in steps.query_map([], |r| r.get::<_, u64>(0))?.enumerate() {
            let idx = row?;
            ensure!(
                idx == expected as u64 || cut.is_some_and(|b| idx > b && (expected as u64) > b),
                "missing native history before step {idx}"
            );
            if idx >= start && !skip.contains(&idx) && (cut.is_none_or(|c| idx <= c) || required.contains(&idx)) {
                report.selected_steps.push(idx);
            }
        }
        let indices: BTreeMap<_, _> = report
            .selected_steps
            .iter()
            .enumerate()
            .map(|(n, &i)| (i, n as u64))
            .collect();
        ensure!(
            !indices.is_empty() && required.iter().all(|i| indices.contains_key(i)),
            "missing native steps for selected history"
        );
        let expected = copy_steps(src, &tx, &tid, &mut report)?;
        let mut gens = src.prepare("SELECT data,size FROM gen_metadata ORDER BY idx")?;
        for row in gens.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))? {
            let (data, size) = row?;
            if let Some(data) = proto::generation(&data, &indices)? {
                tx.execute(
                    "INSERT INTO gen_metadata VALUES(?,?,?)",
                    params![report.generations_kept, data, size],
                )?;
                report.generations_kept += 1;
            } else {
                report.generations_omitted += 1;
            }
        }
        report.executor_rows_omitted = src.query_row("SELECT COUNT(*) FROM executor_metadata", [], |r| r.get(0))?;
        for table in ["parent_references", "battle_mode_infos"] {
            let count: u64 = src.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
            ensure!(count == 0, "unsupported {table} references in native source");
        }
        verify_steps(&tx, &report, &expected)?;
        native::project_rows(&tx, false, -1, &ByteSource::arena()?)?;
        Ok(())
    })?;
    ensure!(
        reads.fresh(&mut Default::default()),
        "Antigravity source changed during derivation"
    );
    tx.commit()?;
    drop(dst);
    if let (Some(stage), Some(out)) = (stage, out) {
        stage_closure(&loc, stage.path(), &report, &mut reads)?;
        ensure!(
            reads.fresh(&mut Default::default()),
            "Antigravity source changed while staging artifacts"
        );
        publish(stage, out, &report, &session)?;
    }
    Ok(report)
}

const STEP_COLUMNS: &str = "step_type,status,has_subtrajectory,metadata,error_details,permissions,task_details,render_info,step_payload,step_format";
struct ExpectedStep {
    turn: u64,
    source_body: blake3::Hash,
    metadata: blake3::Hash,
    payload: blake3::Hash,
}
fn copy_steps(src: &Connection, tx: &Transaction<'_>, tid: &str, report: &mut Derived) -> Result<Vec<ExpectedStep>> {
    use rusqlite::types::Value;
    let mut query = src.prepare(&format!("SELECT {STEP_COLUMNS} FROM steps WHERE idx=?"))?;
    let mut expected = Vec::with_capacity(report.selected_steps.len());
    let mut turns = BTreeMap::new();
    for (i, &old) in report.selected_steps.iter().enumerate() {
        let new = i as u64;
        let row = query.query_row([old], |r| {
            (0..10)
                .map(|n| r.get::<_, Value>(n))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        ensure!(
            row[1] != Value::Integer(5) && row[9] == Value::Integer(0),
            "step {old} is cleared or has unsupported format"
        );
        let (Value::Blob(md), Value::Blob(body)) = (&row[3], &row[8]) else {
            anyhow::bail!("missing native payload at step {old}")
        };
        ensure!(proto::integer(body, 4)? != 5, "cleared embedded step {old}");
        ensure!(
            report.recovery.is_none() || (row[0] != Value::Integer(23) && proto::integer(body, 1)? != 23),
            "native checkpoint disagrees with selected history at step {old}"
        );
        let old_turn = proto::turn(md)?;
        let next = turns.len() as u64;
        let turn = *turns.entry(old_turn).or_insert(next);
        let before = proto::Reference {
            trajectory: tid,
            step: old,
            turn: old_turn,
            cascade: &report.source_id,
        };
        before.verify(md)?;
        before.verify(&proto::nested(body, 5)?)?;
        let after = proto::Reference {
            trajectory: &report.trajectory_id,
            step: new,
            turn,
            cascade: &report.new_id,
        };
        let metadata = proto::metadata(md, &after)?;
        let payload = proto::payload(body, &after, &report.source_id)?;
        expected.push(ExpectedStep {
            turn,
            source_body: proto::body_digest(body)?,
            metadata: blake3::hash(&metadata),
            payload: blake3::hash(&payload),
        });
        let task = match &row[6] {
            Value::Blob(data) => Some(proto::task(data, &report.source_id, &report.new_id)?),
            Value::Null => None,
            _ => anyhow::bail!("invalid native task details at step {old}"),
        };
        tx.execute(
            "INSERT INTO steps VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![new, row[0], row[1], row[2], metadata, row[4], row[5], task, row[7], payload, row[9]],
        )?;
        if row[0] == Value::Integer(14) {
            report.last_user_input_step_index = Some(new);
        }
    }
    Ok(expected)
}

fn verify_steps(tx: &Transaction<'_>, report: &Derived, expected: &[ExpectedStep]) -> Result<()> {
    let mut query = tx.prepare("SELECT idx,metadata,step_payload FROM steps ORDER BY idx")?;
    let mut rows = query.query([])?;
    for (idx, want) in expected.iter().enumerate() {
        let row = rows.next()?.context("staged trajectory ends early")?;
        ensure!(row.get::<_, usize>(0)? == idx, "staged trajectory is out of sequence");
        let md = row.get_ref(1)?.as_blob()?;
        let payload = row.get_ref(2)?.as_blob()?;
        let reference = proto::Reference {
            trajectory: &report.trajectory_id,
            step: idx as u64,
            turn: want.turn,
            cascade: &report.new_id,
        };
        reference.verify(md)?;
        reference.verify(&proto::nested(payload, 5)?)?;
        ensure!(
            proto::body_digest(payload)? == want.source_body,
            "source body changed at step {idx}"
        );
        ensure!(
            blake3::hash(md) == want.metadata && blake3::hash(payload) == want.payload,
            "staged metadata differs at step {idx}"
        );
    }
    ensure!(rows.next()?.is_none(), "unexpected staged rows");
    Ok(())
}

fn stage_closure(loc: &BrainLoc, stage: &Path, r: &Derived, reads: &mut crate::source::ReadSet) -> Result<()> {
    let brain = stage.join("brain").join(&r.new_id);
    fs::create_dir_all(&brain)?;
    let source = &loc.brain_dir;
    reads.record(source);
    // Do not follow links into another session or copy the source's own transcript exports.
    if source.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        let walk = walkdir::WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let p = e.path().strip_prefix(source).expect("walk rooted at brain");
                !p.starts_with(".system_generated")
                    || p == Path::new(".system_generated")
                    || p.starts_with(".system_generated/subagents")
            });
        for entry in walk {
            let e = entry?;
            if e.file_type().is_symlink() {
                continue;
            }
            reads.record(e.path());
            let rel = e.path().strip_prefix(source)?;
            let target = brain.join(rel);
            if e.file_type().is_dir() {
                fs::create_dir_all(target)?;
            } else if e.file_type().is_file() {
                if rel.starts_with(".system_generated/subagents") {
                    let mut desc: serde_json::Value = serde_json::from_slice(&fs::read(e.path())?)?;
                    if let Some(idx) = desc.get("spawnStepIndex").and_then(|v| v.as_u64()) {
                        let Ok(mapped) = r.selected_steps.binary_search(&idx) else {
                            continue;
                        };
                        desc["spawnStepIndex"] = mapped.into();
                    }
                    fs::write(target, serde_json::to_vec(&desc)?)?;
                } else {
                    fs::copy(e.path(), target)?;
                }
            }
        }
    }
    let annotations = stage.join("annotations");
    fs::create_dir(&annotations)?;
    let src = loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id));
    reads.record(&src);
    let dst = annotations.join(format!("{}.pbtxt", r.new_id));
    if src.symlink_metadata().is_ok_and(|m| m.is_file()) {
        fs::copy(src, dst)?;
    } else {
        fs::write(dst, b"")?;
    }
    Ok(())
}

fn summary(conn: &mut Connection, r: &Derived, session: &Session, install: impl FnOnce() -> Result<()>) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Only ordinary tables without application triggers are an offline summary
    // store. Never run foreign code as a side effect of inserting the new key.
    let foreign: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE tbl_name='conversation_summaries' AND type IN ('trigger','view'))",
        [],
        |row| row.get(0),
    )?;
    ensure!(!foreign, "unsupported offline summary schema");
    tx.execute_batch(SUMMARY_SCHEMA)?;
    let mut raw = Vec::new();
    proto::scalar(&mut raw, 2, r.selected_steps.len() as u64);
    proto::bytes(&mut raw, 4, r.trajectory_id.as_bytes());
    proto::scalar(&mut raw, 5, 1);
    if let Some(last) = r.last_user_input_step_index {
        proto::scalar(&mut raw, 16, last);
    }
    let last = r.last_user_input_step_index.map_or(-1, |n| n as i64);
    let native = session.harness_extra(Harness::Antigravity);
    let project = native
        .and_then(|v| v.get("project_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let workspace: Vec<_> = session.cwd.iter().map(|p| format!("file://{}", p.display())).collect();
    let source_time = |idx: u64| {
        session
            .messages
            .iter()
            .find(|m| m.id.as_deref().and_then(|s| s.parse().ok()) == Some(idx))
            .and_then(|m| m.timestamp)
    };
    let last_user_time = r
        .last_user_input_step_index
        .and_then(|i| source_time(r.selected_steps[i as usize]));
    let modified = r.selected_steps.last().and_then(|i| source_time(*i));
    let stamp = |t: Option<chrono::DateTime<chrono::Utc>>| {
        t.map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "0001-01-01T00:00:00Z".into())
    };
    tx.execute(
        "INSERT INTO conversation_summaries(conversation_id,title,status,step_count,last_user_input_step_index,
         not_fully_idle,raw_summary,workspace_uris,project_id,last_modified_time,last_user_input_time)
         VALUES(?,?,'CASCADE_RUN_STATUS_IDLE',?,?,0,?,?,?,?,?)",
        params![
            r.new_id,
            session.title.as_deref().unwrap_or(""),
            r.selected_steps.len(),
            last,
            raw,
            serde_json::to_string(&workspace)?,
            project,
            stamp(modified),
            stamp(last_user_time)
        ],
    )?;
    let verified: bool = tx.query_row(
        "SELECT raw_summary=? AND step_count=? AND last_user_input_step_index=? AND status='CASCADE_RUN_STATUS_IDLE'
         AND not_fully_idle=0 FROM conversation_summaries WHERE conversation_id=?",
        params![raw, r.selected_steps.len(), last, r.new_id],
        |row| row.get(0),
    )?;
    ensure!(verified, "staged summary differs from selected trajectory");
    install()?;
    tx.commit()?;
    Ok(())
}

fn publish(stage: StagedDir, out: &Path, r: &Derived, session: &Session) -> Result<()> {
    if !out.exists() {
        summary(
            &mut Connection::open(stage.path().join("conversation_summaries.db"))?,
            r,
            session,
            || Ok(()),
        )?;
        return stage.publish(out, Claim::New);
    }
    // Existing offline root: reserve the summary key before claiming fresh files.
    // The discovery row commits last. Never replace a sibling conversation or index.
    for dir in ["annotations", "brain", "conversations"] {
        let path = out.join(dir);
        if path.exists() {
            ensure!(
                path.symlink_metadata()?.is_dir(),
                "unsafe destination {}",
                path.display()
            );
        } else {
            fs::create_dir(&path)?;
        }
    }
    let index = out.join("conversation_summaries.db");
    if let Ok(m) = index.symlink_metadata() {
        ensure!(m.is_file(), "unsafe destination index");
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = out.join(format!("conversation_summaries.db{suffix}"));
        if let Ok(m) = sidecar.symlink_metadata() {
            ensure!(m.is_file(), "unsafe destination index sidecar");
        }
    }
    let mut conn = Connection::open(index)?;
    let brain = PathBuf::from("brain").join(&r.new_id);
    let annotations = PathBuf::from("annotations").join(format!("{}.pbtxt", r.new_id));
    let db = PathBuf::from("conversations").join(format!("{}.db", r.new_id));
    let mut installed = Vec::new();
    let result = summary(&mut conn, r, session, || {
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = out.join("conversations").join(format!("{}.db{suffix}", r.new_id));
            crate::publish::refuse_existing(&sidecar, "")?;
        }
        crate::publish::refuse_existing(&out.join(&brain), "")?;
        // Claim the directory exclusively, then install its files without following symlinks.
        fs::create_dir(out.join(&brain))?;
        installed.push(brain.clone());
        for e in walkdir::WalkDir::new(stage.path().join(&brain)).min_depth(1) {
            let e = e?;
            let dest = out.join(e.path().strip_prefix(stage.path())?);
            if e.file_type().is_dir() {
                fs::create_dir(dest)?;
            } else {
                fs::hard_link(e.path(), dest)?;
            }
        }
        for path in [annotations, db] {
            fs::hard_link(stage.path().join(&path), out.join(&path))?;
            installed.push(path);
        }
        Ok(())
    });
    if result.is_err() {
        for p in installed.into_iter().rev() {
            if p == brain {
                let _ = fs::remove_dir_all(out.join(p));
            } else {
                let _ = fs::remove_file(out.join(p));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests;
