//! Native SQLite `cv prune` for Google Antigravity (`conversations/<id>.db`).
//!
//! Operates directly on intact (`status != 5`) native protobuf step rows so Gemini tool-call
//! signatures, model metadata, and native executor configuration survive round-trip verbatim:
//! - Seamlessly drops intermediate `step_type = 23` (`CHECKPOINT`) compaction walls across the
//!   kept window (`--keep`, `--window`, or full intact tail).
//! - Exorcises oversized `30.26` `CortexCheckpointSection` sludge (`all_goals`, dead
//!   `background_tasks`, stale `TrajectoryFileDiff`s) on any retained leading checkpoint, or
//!   synthesizes/rewrites step `0` with a fresh UTF-8 summary when `--replace-summary` is supplied.
//! - Snips `step_type = 132` (`CortexStepGeneric`) tool outputs larger than `min_size` outside the
//!   last `keep_last` user turns (`step_type = 14`), clearing `render_info` and stashing verbatim
//!   originals in `conversations/<new_id>.flat.jsonl` for `cv cat <new_id> <call_id>`.

use super::{
    excluded_closure_dir, live_ls_active, probe_summary_unlocked, proto, publish_staged_app_root, rel_posix,
    verify_trajectory, ExpectedStep, SummarySpec, Uncaptured, DEFAULT_CLOSURE_MAX_BYTES, SCHEMA, STEP_COLUMNS,
};
use crate::coverage::ReadCoverage;
use crate::harness::antigravity::{emit::pbtxt_escape, native, Antigravity, BrainLoc};
use crate::harness::store::{bytes::ByteSource, sqlite};
use crate::harness::Parse as _;
use crate::ir::{est_tokens_bytes, Harness, MessageKind, SessionRef, TokenKind};
use crate::publish::{Claim, StagedDir};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, types::Value, Connection, OptionalExtension as _};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Minimum raw wire size (`4 KiB`) above which `30.26` `CortexCheckpointSection` entries are
/// stripped as compaction sludge when no higher `min_size` is requested.
const DEFAULT_SLUDGE_MIN_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub enum KeepSelector {
    /// Zero-based IR message index range `[start, end)` (`cv show` / `cv prune --keep`).
    Messages(usize, Option<usize>),
    /// Native SQLite `steps.idx` range `[start, end)` (`--keep steps:A..B`).
    Steps(u64, Option<u64>),
}

#[derive(Debug, Clone)]
pub struct ColdPruneOptions {
    pub min_size: usize,
    pub keep_last: usize,
    pub drop: bool,
    pub thinking: bool,
    pub copy_resources: bool,
    pub window: Option<u64>,
    pub keep: Option<KeepSelector>,
    pub replace_summary: Option<String>,
    pub title: Option<String>,
    pub new_id: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
}

impl Default for ColdPruneOptions {
    fn default() -> Self {
        Self {
            min_size: 2048,
            keep_last: 25,
            drop: false,
            thinking: false,
            copy_resources: false,
            window: None,
            keep: None,
            replace_summary: None,
            title: None,
            new_id: None,
            out_dir: None,
            force: false,
            dry_run: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ColdPruneResult {
    pub source_id: String,
    pub new_id: String,
    pub trajectory_id: String,
    pub new_path: Option<PathBuf>,
    pub sidecar_path: Option<PathBuf>,
    pub selected_steps: Vec<u64>,
    pub steps_written: usize,
    pub synthesized_checkpoint: bool,
    pub dropped_steps: usize,
    pub checkpoints_dropped: usize,
    pub checkpoint_sections_stripped: usize,
    pub checkpoint_bytes_saved: u64,
    pub replaced_summary: bool,
    #[serde(rename = "snipped_payloads")]
    pub pruned_count: usize,
    pub payload_bytes_saved: u64,
    pub thinking_steps_stripped: usize,
    pub thinking_bytes_saved: u64,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub tokens_freed: u64,
    pub generations_kept: usize,
    pub generations_omitted: usize,
    pub executor_rows_kept: usize,
    pub executor_rows_omitted: usize,
    pub parent_references_omitted: usize,
    pub battle_mode_rows_omitted: usize,
    pub closure_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uncaptured: Vec<Uncaptured>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub last_user_input_step_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub dry_run: bool,
}

impl ColdPruneResult {
    pub fn bytes_saved(&self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

#[derive(Clone)]
struct StepScan {
    idx: u64,
    step_type: i64,
    status: i64,
    wire_bytes: u64,
    planner_calls: Vec<String>,
    tool_result_id: Option<String>,
}

fn is_terminal_status(status: i64) -> bool {
    matches!(status, 3 | 6 | 7 | 12)
}

fn resolve_message_step_bounds(reference: &SessionRef, start: usize, end: Option<usize>) -> Result<(u64, Option<u64>)> {
    let adapter = crate::harness::rootless(Harness::Antigravity).context("Antigravity adapter unavailable")?;
    let session = adapter.parse(reference)?;
    ensure!(
        !session.messages.is_empty(),
        "source session {} has no messages",
        reference.id
    );
    let total = session.messages.len();
    let end_msg = end.unwrap_or(total).min(total);
    ensure!(
        start < end_msg,
        "--keep range {start}..{} selects zero messages (session has {total})",
        end.map_or_else(String::new, |e| e.to_string())
    );
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
    let lo = step_at(start)?;
    let hi_exclusive = if end_msg >= total {
        None
    } else {
        Some(step_at(end_msg)?)
    };
    Ok((lo, hi_exclusive))
}

fn read_source_summary_meta(
    app_data_dir: &Path,
    source_id: &str,
    db: &Path,
) -> (
    Option<String>,
    Option<PathBuf>,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    let mut title =
        super::super::read_pbtxt_title(&app_data_dir.join("annotations").join(format!("{source_id}.pbtxt")))
            .filter(|t| !t.trim().is_empty());
    let mut cwd = None;
    let mut created_at = None;
    if let Ok(meta) = native::metadata(db, false) {
        cwd = meta.cwd;
        created_at = meta.created_at;
    }
    let mut project_id = String::new();
    let sum_db = app_data_dir.join("conversation_summaries.db");
    if sum_db.is_file() {
        if let Ok(map) = super::super::read_summaries_map(&sum_db) {
            if let Some(sum) = map.get(source_id) {
                if title.is_none() {
                    title = sum.title.clone().filter(|t| !t.trim().is_empty());
                }
                if cwd.is_none() {
                    cwd.clone_from(&sum.cwd);
                }
                if let Some(pid) = &sum.project_id {
                    project_id.clone_from(pid);
                }
            }
        }
    }
    (title, cwd, project_id, created_at)
}

fn walk_prune_brain_dir(
    root: &Path,
    cur: &Path,
    dst_brain: &Path,
    r: &mut ColdPruneResult,
    index_map: &BTreeMap<u64, u64>,
    copy_resources: bool,
    closure_max_bytes: u64,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(cur)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root)?;
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            r.uncaptured.push(Uncaptured {
                path: rel_posix(rel),
                bytes: None,
                why: "symlink",
            });
            continue;
        }
        if rel.starts_with(".system_generated")
            && rel != Path::new(".system_generated")
            && !rel.starts_with(".system_generated/subagents")
        {
            continue;
        }
        if ft.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if rel == Path::new("scratch") && !copy_resources {
                r.uncaptured.push(Uncaptured {
                    path: rel_posix(rel),
                    bytes: None,
                    why: "scratch omitted unless --copy-resources",
                });
                continue;
            }
            if !rel.starts_with(".system_generated") {
                if let Some(why) = excluded_closure_dir(&name_str, &path) {
                    r.uncaptured.push(Uncaptured {
                        path: rel_posix(rel),
                        bytes: None,
                        why,
                    });
                    continue;
                }
            }
            walk_prune_brain_dir(root, &path, dst_brain, r, index_map, copy_resources, closure_max_bytes)?;
        } else if ft.is_file() {
            let target = dst_brain.join(rel);
            if rel.starts_with(".system_generated/subagents") {
                let Ok(raw) = fs::read(&path) else {
                    continue;
                };
                let Ok(mut desc) = serde_json::from_slice::<serde_json::Value>(&raw) else {
                    continue;
                };
                if let Some(idx) = desc.get("spawnStepIndex").and_then(|v| v.as_u64()) {
                    let Some(&mapped) = index_map.get(&idx) else {
                        continue;
                    };
                    desc["spawnStepIndex"] = mapped.into();
                } else if !copy_resources {
                    continue;
                }
                let encoded = serde_json::to_vec(&desc)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(target, encoded)?;
            } else {
                let len = meta.len();
                if r.closure_bytes.saturating_add(len) > closure_max_bytes {
                    r.uncaptured.push(Uncaptured {
                        path: rel_posix(rel),
                        bytes: Some(len),
                        why: "exceeds brain closure byte budget",
                    });
                    continue;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&path, target)?;
                r.closure_bytes += len;
            }
        }
    }
    Ok(())
}

pub fn prune(reference: &SessionRef, opts: &ColdPruneOptions) -> Result<ColdPruneResult> {
    if opts.window.is_some() && opts.keep.is_some() {
        bail!("--window conflicts with --keep");
    }
    let loc = BrainLoc::from_transcript(&reference.path)
        .context("expected an Antigravity conversation or brain transcript")?;
    let db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
    ensure!(
        db.is_file(),
        "Antigravity native SQLite store {} not found; `cv prune` requires an intact native store",
        db.display()
    );

    let step_bounds = match &opts.keep {
        Some(KeepSelector::Steps(s, e)) => Some((*s, *e)),
        Some(KeepSelector::Messages(s, e)) => Some(resolve_message_step_bounds(reference, *s, *e)?),
        None => None,
    };

    let new_id = opts.new_id.clone().unwrap_or_else(crate::prune::new_uuid);
    crate::publish::validate_output_id(&new_id)?;
    ensure!(
        super::super::looks_like_uuid(&new_id),
        "Antigravity conversation IDs must be UUIDs (got {new_id:?})"
    );
    ensure!(new_id != loc.id, "new id must differ from source");

    let claim = if opts.force { Claim::Replace } else { Claim::New };
    let explicit_out = opts.out_dir.as_deref();
    let target_root: Option<&Path> = if opts.dry_run {
        explicit_out
    } else if let Some(out) = explicit_out {
        Some(out)
    } else {
        if matches!(claim, Claim::Replace) && live_ls_active() {
            bail!("refusing --force replacement in active Antigravity language server store; omit --force for a fresh UUID or pass --out <dir>");
        }
        probe_summary_unlocked(&loc.app_data_dir)?;
        Some(loc.app_data_dir.as_path())
    };

    let stage = if opts.dry_run {
        None
    } else {
        let out = target_root.context("missing output directory")?;
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
        if out.exists() {
            probe_summary_unlocked(out)?;
        }
        Some(StagedDir::beside(out, "antigravity-prune")?)
    };

    let (src_title, src_cwd, project_id, src_created_at) = read_source_summary_meta(&loc.app_data_dir, &loc.id, &db);
    let final_title = opts.title.clone().or(src_title).unwrap_or_default();

    let rel_db = PathBuf::from("conversations").join(format!("{new_id}.db"));
    let rel_sidecar = PathBuf::from("conversations").join(format!("{new_id}.flat.jsonl"));
    let mut dst = if let Some(stage) = &stage {
        fs::create_dir_all(stage.path().join("conversations"))?;
        Connection::open(stage.path().join(&rel_db))?
    } else {
        Connection::open_in_memory()?
    };
    let tx = dst.transaction()?;
    tx.execute_batch(SCHEMA)?;

    let mut report = ColdPruneResult {
        source_id: loc.id.clone(),
        new_path: target_root.map(|o| o.join(&rel_db)),
        sidecar_path: None,
        new_id: new_id.clone(),
        trajectory_id: crate::prune::new_uuid(),
        selected_steps: Vec::new(),
        steps_written: 0,
        synthesized_checkpoint: false,
        dropped_steps: 0,
        checkpoints_dropped: 0,
        checkpoint_sections_stripped: 0,
        checkpoint_bytes_saved: 0,
        replaced_summary: opts.replace_summary.is_some(),
        pruned_count: 0,
        payload_bytes_saved: 0,
        thinking_steps_stripped: 0,
        thinking_bytes_saved: 0,
        before_bytes: 0,
        after_bytes: 0,
        tokens_freed: 0,
        generations_kept: 0,
        generations_omitted: 0,
        executor_rows_kept: 0,
        executor_rows_omitted: 0,
        parent_references_omitted: 0,
        battle_mode_rows_omitted: 0,
        closure_bytes: 0,
        uncaptured: Vec::new(),
        warnings: Vec::new(),
        last_user_input_step_index: None,
        cwd: src_cwd.clone(),
        dry_run: opts.dry_run,
    };

    if !opts.dry_run && explicit_out.is_none() && live_ls_active() {
        report.warnings.push(
            "published into active Antigravity store under a fresh UUID; resume via `agy --conversation <new_id>` if the running server caches its conversation list".into(),
        );
    }

    let mut stashed_lines: Vec<String> = Vec::new();
    let mut index_map: BTreeMap<u64, u64> = BTreeMap::new();
    let mut last_step_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_user_ts: Option<chrono::DateTime<chrono::Utc>> = None;

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

        let mut stripped_blob_parent = false;
        let mut blobs = src.prepare("SELECT id,data FROM trajectory_metadata_blob")?;
        for row in blobs.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
            let (id, data) = row?;
            let (rewritten, stripped) = proto::trajectory_standalone(&data, &cid, &report.new_id)?;
            stripped_blob_parent |= stripped;
            tx.execute(
                "INSERT INTO trajectory_metadata_blob VALUES(?,?)",
                params![id, rewritten],
            )?;
        }

        // Scan intact (`status != 5`) native steps once to plan windowing, checkpoint splicing, and
        // tool-call/result pairing.
        let mut scan_stmt = src.prepare(
            "SELECT idx, step_type, status, step_format, metadata, COALESCE(length(render_info), 0), step_payload
             FROM steps WHERE status != 5 ORDER BY idx",
        )?;
        let mut intact: Vec<StepScan> = Vec::new();
        let mut checkpoint_templates: BTreeMap<u64, (Vec<u8>, Vec<u8>)> = BTreeMap::new();
        for row in scan_stmt.query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<Vec<u8>>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<Vec<u8>>>(6)?,
            ))
        })? {
            let (idx, step_type, status, format, md_opt, render_len, body_opt) = row?;
            ensure!(format == 0, "unsupported Antigravity step_format {format} at step {idx}");
            let (Some(md), Some(body)) = (md_opt, body_opt) else {
                continue;
            };
            if proto::integer(&body, 4)? == 5 {
                continue;
            }
            let wire_bytes = md.len() as u64 + body.len() as u64 + render_len.max(0) as u64;
            report.before_bytes = report.before_bytes.saturating_add(wire_bytes);
            if step_type == 23 {
                checkpoint_templates.insert(idx, (md.clone(), body.clone()));
            }
            let planner_calls = if step_type == 15 {
                proto::planner_tool_ids(&body)?
            } else {
                Vec::new()
            };
            let tool_result_id = if step_type != 14 && step_type != 15 && step_type != 23 && step_type != 101 {
                proto::step_tool_id(&md, &body)?
            } else {
                None
            };
            intact.push(StepScan {
                idx,
                step_type,
                status,
                wire_bytes,
                planner_calls,
                tool_result_id,
            });
        }
        ensure!(
            !intact.is_empty(),
            "no intact (non-cleared) native steps in {}",
            db.display()
        );

        // Trim any trailing non-terminal (`RUNNING` / `PENDING` / `GENERATING` / `WAITING`) steps at
        // the live tail so the successor always lands in a settled `IDLE` state.
        let mut settled_len = intact.len();
        while settled_len > 0 && !is_terminal_status(intact[settled_len - 1].status) {
            settled_len -= 1;
        }
        if settled_len < intact.len() {
            report.warnings.push(format!(
                "omitted {} trailing unsettled in-flight step(s) from live tail",
                intact.len() - settled_len
            ));
            intact.truncate(settled_len);
        }
        ensure!(!intact.is_empty(), "all native steps are unsettled");

        // Determine `[lo_pos, hi_pos)` inside `intact`.
        let (mut lo_pos, mut hi_pos) = if let Some((lo_step, hi_step_excl)) = step_bounds {
            let min_intact = intact.first().unwrap().idx;
            ensure!(
                lo_step >= min_intact,
                "requested step {lo_step} precedes earliest intact native step {min_intact} in {} (earlier steps were cleared by Antigravity)",
                db.display()
            );
            let s = intact.partition_point(|st| st.idx < lo_step);
            let e = match hi_step_excl {
                Some(hi) => intact.partition_point(|st| st.idx < hi),
                None => intact.len(),
            };
            ensure!(s < e, "--keep selected zero intact native steps");
            (s, e)
        } else if let Some(budget_tokens) = opts.window {
            let mut acc_tokens = 0u64;
            let mut s = intact.len();
            let mut overshoot = false;
            for i in (0..intact.len()).rev() {
                if intact[i].step_type == 23 {
                    continue;
                }
                let est = est_tokens_bytes(intact[i].wire_bytes as usize, TokenKind::Text);
                if acc_tokens.saturating_add(est) > budget_tokens {
                    if acc_tokens == 0 {
                        s = i;
                        overshoot = true;
                    }
                    break;
                }
                acc_tokens += est;
                s = i;
            }
            if s == intact.len() {
                s = 0;
            }
            // Snap backward to a user/system turn so a windowed tail never opens mid-tool-call.
            while s > 0 && !matches!(intact[s].step_type, 14 | 23) {
                s -= 1;
            }
            if overshoot {
                report.warnings.push(format!(
                    "newest turn alone exceeds --window {budget_tokens}; kept newest turn"
                ));
            }
            (s, intact.len())
        } else {
            // Default without --keep/--window: if step 0 was cleared, start at the first intact
            // checkpoint (if any) so we don't open on an orphaned pre-checkpoint fragment.
            let s = if intact[0].idx > 0 {
                intact.iter().position(|st| st.step_type == 23).unwrap_or(0)
            } else {
                0
            };
            (s, intact.len())
        };

        // Ensure the selected window never opens on an orphaned tool result (`step_type = 132` etc.)
        // or assistant reply without its prompt/checkpoint anchor.
        if opts.replace_summary.is_some() {
            while lo_pos < hi_pos && intact[lo_pos].step_type == 23 {
                lo_pos += 1;
            }
        }
        if lo_pos < hi_pos && !matches!(intact[lo_pos].step_type, 14 | 23 | 101) {
            if let Some(back) = (0..lo_pos)
                .rev()
                .find(|&i| matches!(intact[i].step_type, 14 | 23 | 101))
            {
                lo_pos = back;
            } else if let Some(fwd) = (lo_pos..hi_pos).find(|&i| matches!(intact[i].step_type, 14 | 23 | 101)) {
                lo_pos = fwd;
            }
        }
        ensure!(lo_pos < hi_pos, "no turn boundary found in selected native window");

        // Ensure every `PLANNER_RESPONSE` tool_call inside `[lo_pos..hi_pos)` has its closing result:
        // pull in immediate closing tool results past `hi_pos` when available, else trim an
        // unfinished trailing planner batch at EOF.
        let mut open_calls: BTreeSet<String> = BTreeSet::new();
        let mut last_planner_pos: Option<usize> = None;
        for (offset, st) in intact[lo_pos..hi_pos].iter().enumerate() {
            if st.step_type == 15 && !st.planner_calls.is_empty() {
                last_planner_pos = Some(lo_pos + offset);
                for cid in &st.planner_calls {
                    open_calls.insert(cid.clone());
                }
            }
            if let Some(done_id) = &st.tool_result_id {
                open_calls.remove(done_id);
            }
        }
        while !open_calls.is_empty() && hi_pos < intact.len() {
            let next = &intact[hi_pos];
            if matches!(next.step_type, 14 | 15 | 23) {
                break;
            }
            if let Some(done_id) = &next.tool_result_id {
                open_calls.remove(done_id);
            }
            hi_pos += 1;
        }
        if !open_calls.is_empty() {
            if let Some(p_pos) = last_planner_pos.filter(|&p| p > lo_pos) {
                report.warnings.push(format!(
                    "trimmed trailing incomplete tool-call batch at native step {} ({} unsettled call(s))",
                    intact[p_pos].idx,
                    open_calls.len()
                ));
                hi_pos = p_pos;
            }
        }

        // Build final kept native step list, omitting intermediate `step_type = 23` compaction walls!
        // Only retain a source `step_type = 23` if `replace_summary` is `None` AND it sits at the
        // very start of `[lo_pos..hi_pos)`.
        let mut kept_positions: Vec<usize> = Vec::with_capacity(hi_pos - lo_pos);
        for (pos, st) in intact.iter().enumerate().take(hi_pos).skip(lo_pos) {
            if st.step_type == 23 {
                if opts.replace_summary.is_none() && kept_positions.is_empty() {
                    kept_positions.push(pos);
                } else {
                    report.checkpoints_dropped += 1;
                }
            } else {
                kept_positions.push(pos);
            }
        }
        ensure!(
            !kept_positions.is_empty() || opts.replace_summary.is_some(),
            "selection holds no non-checkpoint steps"
        );
        report.dropped_steps = intact.len().saturating_sub(kept_positions.len());

        // Identify the `keep_last` user-turn cutoff (`step_type = 14`): steps at or after the Nth
        // newest `USER_INPUT` keep all tool payloads and thinking verbatim.
        let user_positions_in_kept: Vec<usize> = kept_positions
            .iter()
            .enumerate()
            .filter(|(_, &pos)| intact[pos].step_type == 14)
            .map(|(k_idx, _)| k_idx)
            .collect();
        let verbatim_from_kept_idx = if opts.keep_last == 0 {
            kept_positions.len()
        } else if user_positions_in_kept.len() <= opts.keep_last {
            0
        } else {
            user_positions_in_kept[user_positions_in_kept.len() - opts.keep_last]
        };

        let sludge_threshold = opts.min_size.max(DEFAULT_SLUDGE_MIN_BYTES);
        let mut expected: Vec<ExpectedStep> = Vec::with_capacity(kept_positions.len() + 1);
        let mut turns: BTreeMap<u64, u64> = BTreeMap::new();
        let mut next_new_idx = 0u64;

        // If `--replace-summary` is supplied, write clean `step_type = 23` checkpoint at `idx = 0`.
        if let Some(summary_text) = &opts.replace_summary {
            let first_selected_idx = kept_positions
                .first()
                .map(|&p| intact[p].idx)
                .unwrap_or_else(|| intact.last().unwrap().idx);
            let tpl = checkpoint_templates
                .range(..=first_selected_idx)
                .next_back()
                .or_else(|| checkpoint_templates.iter().next())
                .map(|(_, pair)| (pair.0.as_slice(), pair.1.as_slice()));
            let after = proto::Reference {
                trajectory: &report.trajectory_id,
                step: 0,
                turn: 0,
                cascade: &report.new_id,
            };
            let (md, body, cp_saved, cp_stripped) =
                proto::checkpoint_step(tpl, summary_text, &after, &report.source_id)?;
            report.synthesized_checkpoint = true;
            report.checkpoint_bytes_saved += cp_saved;
            report.checkpoint_sections_stripped += cp_stripped;
            report.after_bytes += (md.len() + body.len()) as u64;
            expected.push(ExpectedStep {
                turn: 0,
                source_body: proto::body_digest(&body)?,
                metadata: blake3::hash(&md),
                payload: blake3::hash(&body),
            });
            tx.execute(
                "INSERT INTO steps(idx,step_type,status,has_subtrajectory,metadata,step_payload,step_format)
                 VALUES(0,23,3,0,?,?,0)",
                params![md, body],
            )?;
            next_new_idx = 1;
        }

        let mut query = src.prepare(&format!("SELECT {STEP_COLUMNS} FROM steps WHERE idx=?"))?;
        for (k_idx, &pos) in kept_positions.iter().enumerate() {
            let old = intact[pos].idx;
            let new = next_new_idx;
            next_new_idx += 1;
            report.selected_steps.push(old);
            index_map.insert(old, new);

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
                bail!("missing native payload at step {old}");
            };
            ensure!(proto::integer(body, 4)? != 5, "cleared embedded step {old}");

            let old_turn = proto::turn(md)?;
            let turn_floor = u64::from(report.synthesized_checkpoint);
            let next_turn = turns.len() as u64 + turn_floor;
            let turn = *turns.entry(old_turn).or_insert(next_turn);
            let before = proto::Reference {
                trajectory: &tid,
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
            let mut payload = proto::payload(body, &after, &report.source_id)?;
            let mut render_col = row[7].clone();

            let step_type = match row[0] {
                Value::Integer(n) => n,
                _ => 0,
            };
            let in_prunable_prefix = k_idx < verbatim_from_kept_idx;

            if step_type == 23 {
                let (rewritten, cp_saved, cp_stripped) =
                    proto::prune_existing_checkpoint_body(&payload, None, sludge_threshold)?;
                payload = rewritten;
                report.checkpoint_bytes_saved += cp_saved;
                report.checkpoint_sections_stripped += cp_stripped;
            } else if step_type == 132 && in_prunable_prefix {
                let (rewritten, snipped_opt, extra_saved, clear_render) = proto::snip_generic_step(
                    &metadata,
                    &payload,
                    old,
                    opts.min_size,
                    &report.new_id,
                    opts.drop,
                )?;
                let render_col_len = match &render_col {
                    Value::Blob(b) if clear_render => b.len() as u64,
                    _ => 0,
                };
                if clear_render {
                    render_col = Value::Null;
                }
                if let Some(snipped) = snipped_opt {
                    report.pruned_count += 1;
                    report.payload_bytes_saved += snipped.bytes_saved + render_col_len;
                    if !opts.drop {
                        stashed_lines.push(crate::prune::sidecar_line(
                            &snipped.call_id,
                            &snipped.tool_name,
                            &snipped.input,
                            &snipped.original,
                        ));
                    }
                } else if extra_saved > 0 || render_col_len > 0 {
                    report.payload_bytes_saved += extra_saved + render_col_len;
                }
                payload = rewritten;
            } else if step_type == 15 && in_prunable_prefix && opts.thinking {
                let (rewritten, saved) = proto::drop_planner_thinking(&payload)?;
                if saved > 0 {
                    report.thinking_steps_stripped += 1;
                    report.thinking_bytes_saved += saved;
                }
                payload = rewritten;
            }

            if let Ok(Some(ts_raw)) = proto::unique(&metadata, 1, 2) {
                if let Ok(secs) = proto::integer(&ts_raw, 1) {
                    let nanos = proto::integer(&ts_raw, 2).unwrap_or(0);
                    if let (Ok(s), Ok(n)) = (i64::try_from(secs), u32::try_from(nanos)) {
                        if let Some(dt) = chrono::DateTime::from_timestamp(s, n) {
                            last_step_ts = Some(dt);
                            if step_type == 14 {
                                last_user_ts = Some(dt);
                            }
                        }
                    }
                }
            }

            let render_after_len = match &render_col {
                Value::Blob(b) => b.len() as u64,
                _ => 0,
            };
            report.after_bytes += metadata.len() as u64 + payload.len() as u64 + render_after_len;

            expected.push(ExpectedStep {
                turn,
                source_body: proto::body_digest(&payload)?,
                metadata: blake3::hash(&metadata),
                payload: blake3::hash(&payload),
            });
            let task = match &row[6] {
                Value::Blob(data) => Some(proto::task(data, &report.source_id, &report.new_id)?),
                Value::Null => None,
                _ => bail!("invalid native task details at step {old}"),
            };
            tx.execute(
                "INSERT INTO steps VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    new, row[0], row[1], row[2], metadata, row[4], row[5], task, render_col, payload, row[9]
                ],
            )?;
            if step_type == 14 {
                report.last_user_input_step_index = Some(new);
            }
        }
        report.steps_written = expected.len();

        let mut gens = src.prepare("SELECT data,size FROM gen_metadata ORDER BY idx")?;
        for row in gens.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))? {
            let (data, size) = row?;
            if let Some(data) = proto::generation(&data, &index_map)? {
                tx.execute(
                    "INSERT INTO gen_metadata VALUES(?,?,?)",
                    params![report.generations_kept, data, size],
                )?;
                report.generations_kept += 1;
            } else {
                report.generations_omitted += 1;
            }
        }

        let total_executor: usize = src.query_row("SELECT COUNT(*) FROM executor_metadata", [], |r| r.get(0))?;
        let exec_zero: Option<Option<Vec<u8>>> = src
            .query_row("SELECT data FROM executor_metadata WHERE idx = 0", [], |r| {
                r.get::<_, Option<Vec<u8>>>(0)
            })
            .optional()?;
        if let Some(data) = exec_zero {
            tx.execute("INSERT INTO executor_metadata VALUES(0, ?)", params![data])?;
            report.executor_rows_kept = 1;
        }
        report.executor_rows_omitted = total_executor.saturating_sub(report.executor_rows_kept);

        report.parent_references_omitted = src.query_row("SELECT COUNT(*) FROM parent_references", [], |r| r.get(0))?;
        if report.parent_references_omitted > 0 || stripped_blob_parent {
            report.warnings.push(format!(
                "omitted {} parent_references row(s) to materialize standalone root successor",
                report.parent_references_omitted
            ));
        }
        report.battle_mode_rows_omitted = src.query_row("SELECT COUNT(*) FROM battle_mode_infos", [], |r| r.get(0))?;
        if report.battle_mode_rows_omitted > 0 {
            report.warnings.push(format!(
                "omitted {} battle_mode_infos row(s) from standalone successor",
                report.battle_mode_rows_omitted
            ));
        }

        verify_trajectory(&tx, &report.trajectory_id, &report.new_id, &expected)?;
        native::project_rows(&tx, false, -1, &ByteSource::arena()?)?;
        Ok(())
    })?;

    report.tokens_freed = est_tokens_bytes(report.bytes_saved() as usize, TokenKind::Text);
    tx.commit()?;
    drop(dst);

    if !stashed_lines.is_empty() && !opts.drop {
        report.sidecar_path = target_root.map(|o| o.join(&rel_sidecar));
    }

    if let (Some(stage), Some(out)) = (stage, target_root) {
        if !stashed_lines.is_empty() && !opts.drop {
            let mut sc = fs::File::create(stage.path().join(&rel_sidecar))?;
            for line in &stashed_lines {
                writeln!(sc, "{line}")?;
            }
            sc.flush()?;
        }

        let dst_brain = stage.path().join("brain").join(&report.new_id);
        fs::create_dir_all(&dst_brain)?;
        if loc.brain_dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            walk_prune_brain_dir(
                &loc.brain_dir,
                &loc.brain_dir,
                &dst_brain,
                &mut report,
                &index_map,
                opts.copy_resources,
                DEFAULT_CLOSURE_MAX_BYTES,
            )?;
        }

        let annotations = stage.path().join("annotations");
        fs::create_dir_all(&annotations)?;
        let dst_pbtxt = annotations.join(format!("{}.pbtxt", report.new_id));
        if let Some(custom_title) = &opts.title {
            let body = if custom_title.trim().is_empty() {
                String::new()
            } else {
                format!("title: \"{}\"\n", pbtxt_escape(custom_title))
            };
            fs::write(&dst_pbtxt, body.as_bytes())?;
        } else {
            let src_pbtxt = loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id));
            if src_pbtxt.symlink_metadata().is_ok_and(|m| m.is_file()) {
                fs::copy(src_pbtxt, dst_pbtxt)?;
            } else {
                fs::write(dst_pbtxt, b"")?;
            }
        }

        let spec = SummarySpec {
            new_id: &report.new_id,
            trajectory_id: &report.trajectory_id,
            title: &final_title,
            step_count: report.steps_written,
            last_user_input_step_index: report.last_user_input_step_index,
            cwd: src_cwd.as_deref(),
            project_id: &project_id,
            modified: last_step_ts.or(src_created_at),
            last_user_time: last_user_ts.or(last_step_ts).or(src_created_at),
            replace: matches!(claim, Claim::Replace),
        };

        let verify_id = report.new_id.clone();
        let verify_rel_db = rel_db.clone();
        let expect_messages = report.steps_written > 0;
        publish_staged_app_root(stage, out, &spec, claim, |root| {
            let agy = Antigravity::with_home(root);
            let sref = SessionRef::new(&verify_id, Harness::Antigravity, root.join(&verify_rel_db));
            let reparsed = agy
                .parse(&sref)
                .context("re-parsing pruned Antigravity SQLite trajectory")?;
            ensure!(
                reparsed.id == verify_id,
                "reparsed id mismatch ({} != {verify_id})",
                reparsed.id
            );
            if expect_messages {
                ensure!(
                    !reparsed.messages.is_empty(),
                    "reparsed pruned Antigravity trajectory holds zero messages"
                );
            }
            if let Some(cov) = ReadCoverage::of_session(&reparsed) {
                ensure!(
                    !cov.limitations.contains(ReadCoverage::UNRECONCILED_HISTORY),
                    "pruned Antigravity trajectory reported unreconciled history: {:?}",
                    cov.limitations
                );
            }
            Ok(())
        })?;
    }

    Ok(report)
}
