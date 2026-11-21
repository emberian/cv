//! Conversation metadata beside the steps, read through the same staged wire ranges:
//! `gen_metadata.data` (field `2` = step indices, packed or repeated; field `1.19` = model
//! slug), `trajectory_metadata_blob.data` (`1.1` workspace URI, `1.2` git root, `1.4`
//! git branch, `2` created timestamp, `5` parent, `6` root, `7` workspace, `8.1`/`8.2`
//! agent type and role), with `parent_references` and `trajectory_meta` fallbacks, plus
//! `CascadeTrajectorySummary` (`conversation_summaries.raw_summary`). A `gen_metadata` row
//! also holds its whole model request (12 MiB seen); only the named scalars leave the staging file.
use super::super::{file_uri_to_path, looks_like_uuid, Annotations, SummaryRow};
use super::{sqlite, wire, File, Proto, Result, Stage};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Scalar metadata text: a slug, an id, a URI. Longer values are not metadata.
const TEXT: usize = 64 * 1024;
/// Maximum `conversation_summaries.raw_summary` protobuf blob decoded into [`RawSummary`].
pub(in super::super) const MAX_RAW_SUMMARY_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(in super::super) struct Meta {
    pub step_models: BTreeMap<u64, String>,
    pub dominant_model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub git_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub agent_type: Option<String>,
    pub agent_role: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub trajectory_type: Option<i64>,
    pub trajectory_source: Option<i64>,
}

/// Decoded `CascadeTrajectorySummary` (`conversation_summaries.raw_summary` in 1.5.13+).
#[derive(Debug, Default, Clone)]
pub(in super::super) struct RawSummary {
    pub summary: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub cwd: Option<PathBuf>,
    pub git_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub annotations: Annotations,
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub agent_type: Option<String>,
    pub agent_role: Option<String>,
    pub project_id: Option<String>,
    pub killed: Option<bool>,
    pub fork_parent_id: Option<String>,
}

#[allow(dead_code)]
pub(in super::super) fn parse_raw_summary(bytes: &[u8]) -> Option<RawSummary> {
    let mut stage = None;
    parse_raw_summary_with_stage(&mut stage, bytes)
}

pub(in super::super) fn parse_raw_summary_with_stage(stage: &mut Option<Stage>, bytes: &[u8]) -> Option<RawSummary> {
    if bytes.is_empty() || bytes.len() > MAX_RAW_SUMMARY_BYTES {
        return None;
    }
    if stage.as_ref().is_none_or(|s| s.used.get() >= Stage::RENEW) {
        *stage = Some(Stage::new().ok()?);
    }
    let st = stage.as_ref()?;
    let (at, len) = st
        .append(|out| {
            std::io::Write::write_all(out, bytes)?;
            Ok(())
        })
        .ok()?;
    let message = Proto::read(&st.file, at, len).ok()?;
    let mut out = RawSummary::default();
    raw_summary(&st.file, &message, &mut out);
    Some(out)
}

impl SummaryRow {
    pub(in super::super) const MAX_RAW_SUMMARY_BYTES: usize = MAX_RAW_SUMMARY_BYTES;

    pub(in super::super) fn merge_raw_summary(&mut self, stage: &mut Option<Stage>, bytes: &[u8]) {
        let Some(raw) = parse_raw_summary_with_stage(stage, bytes) else {
            return;
        };
        self.annotations.merge_fallback(&raw.annotations);
        if self.title.is_none() {
            self.title = self.annotations.title.clone();
        }
        if self.preview.is_none() {
            self.preview = raw.summary;
        }
        if self.cwd.is_none() {
            self.cwd = raw.cwd;
        }
        if self.git_root.is_none() {
            self.git_root = raw.git_root;
        }
        if self.git_branch.is_none() {
            self.git_branch = raw.git_branch;
        }
        if self.parent_id.is_none() {
            self.parent_id = raw.parent_id;
        }
        if self.root_id.is_none() {
            self.root_id = raw.root_id;
        }
        if self.agent_type.is_none() {
            self.agent_type = raw.agent_type;
        }
        if self.agent_role.is_none() {
            self.agent_role = raw.agent_role;
        }
        if self.project_id.is_none() {
            self.project_id = raw.project_id;
        }
        if self.created_at.is_none() {
            self.created_at = raw.created_at;
        }
        if !self.killed {
            self.killed = raw.killed.unwrap_or(false);
        }
        if self.fork_parent_id.is_none() {
            self.fork_parent_id = raw.fork_parent_id;
        }
    }
}

/// `models` selects the per-step model rows: a header needs them, discovery does not.
pub(in super::super) fn read(db_path: &Path, models: bool) -> Result<Meta> {
    sqlite::open_ro(db_path)?.read_snapshot(|conn, _| {
        let mut out = Meta::default();
        let mut stage = Stage::new()?;
        // A row that does not decode is skipped; a row that cannot be read is an error.
        let mut cell = |table: &str, rowid: i64| -> Result<Option<(File, Proto)>> {
            if stage.used.get() >= Stage::RENEW {
                stage = Stage::new()?;
            }
            let (at, len) = stage.cell(conn, table, "data", rowid)?;
            let file = stage.file.try_clone()?;
            Ok(Proto::read(&file, at, len).ok().map(|message| (file, message)))
        };
        if models && sqlite::table_exists(conn, "gen_metadata") {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            let mut first_model = None;
            let mut query = conn.prepare("SELECT rowid FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx")?;
            let mut rows = query.query([])?;
            while let Some(row) = rows.next()? {
                let Some((file, message)) = cell("gen_metadata", row.get(0)?)? else {
                    continue;
                };
                let Some(model) = message
                    .repeated(1)
                    .filter_map(|chat| chat.message(&file).ok())
                    .filter_map(|chat| trimmed(&file, chat.last(19)?))
                    .last()
                else {
                    continue;
                };
                let steps: Vec<u64> = message
                    .repeated(2)
                    .flat_map(|field| match field.value {
                        wire::Value::Varint(step) => vec![step],
                        wire::Value::Bytes { .. } => field.varints(&file, TEXT).unwrap_or_default(),
                        _ => Vec::new(),
                    })
                    .collect();
                first_model.get_or_insert_with(|| model.clone());
                *counts.entry(model.clone()).or_default() += steps.len().max(1);
                for step in steps {
                    out.step_models.insert(step, model.clone());
                }
            }
            out.dominant_model = counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(model, _)| model)
                .or(first_model);
        }
        let mut has_trajectory_blob = false;
        if sqlite::table_exists(conn, "trajectory_metadata_blob") {
            use rusqlite::OptionalExtension as _;
            let rowid: Option<i64> = conn
                .query_row(
                    "SELECT rowid FROM trajectory_metadata_blob WHERE id = 'main' AND data IS NOT NULL LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(rowid) = rowid {
                has_trajectory_blob = true;
                if let Some((file, message)) = cell("trajectory_metadata_blob", rowid)? {
                    trajectory(&file, &message, &mut out);
                }
            }
        }
        if !has_trajectory_blob || out.parent_id.is_none() {
            let pr_cols = sqlite::Select::of(conn, "parent_references");
            if pr_cols.has("data") {
                use rusqlite::OptionalExtension as _;
                let sql = if pr_cols.has("idx") {
                    "SELECT rowid FROM parent_references WHERE data IS NOT NULL ORDER BY idx LIMIT 1"
                } else {
                    "SELECT rowid FROM parent_references WHERE data IS NOT NULL LIMIT 1"
                };
                let rowid: Option<i64> = conn.query_row(sql, [], |row| row.get(0)).optional()?;
                if let Some(rowid) = rowid {
                    if let Some((file, message)) = cell("parent_references", rowid)? {
                        // `CortexTrajectoryReference`: `cascade_id` (3) names the on-disk session UUID
                        // when present; `trajectory_id` (1) is the fallback when UUID-shaped.
                        let cascade = message
                            .last(3)
                            .and_then(|f| trimmed(&file, f))
                            .filter(|s| looks_like_uuid(s));
                        let traj = message
                            .last(1)
                            .and_then(|f| trimmed(&file, f))
                            .filter(|s| looks_like_uuid(s));
                        if let Some(parent) = cascade.or(traj) {
                            out.parent_id = Some(parent);
                        }
                    }
                }
            }
        }
        if !has_trajectory_blob {
            let tm_cols = sqlite::Select::of(conn, "trajectory_meta");
            if tm_cols.has("trajectory_type") || tm_cols.has("source") {
                use rusqlite::types::ValueRef;
                use rusqlite::OptionalExtension as _;
                let tt_col = if tm_cols.has("trajectory_type") {
                    "trajectory_type"
                } else {
                    "NULL"
                };
                let src_col = if tm_cols.has("source") { "source" } else { "NULL" };
                let sql = format!("SELECT {tt_col}, {src_col} FROM trajectory_meta LIMIT 1");
                let as_i64 = |v: ValueRef<'_>| match v {
                    ValueRef::Integer(i) => Some(i),
                    ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse::<i64>().ok(),
                    _ => None,
                };
                if let Some((tt, ts)) = conn
                    .query_row(&sql, [], |row| Ok((as_i64(row.get_ref(0)?), as_i64(row.get_ref(1)?))))
                    .optional()?
                {
                    out.trajectory_type = tt;
                    out.trajectory_source = ts;
                }
            }
        }
        Ok(out)
    })
}

fn text(file: &File, field: wire::Field) -> Option<String> {
    field.string(file, TEXT).ok().filter(|text| !text.is_empty())
}
fn trimmed(file: &File, field: wire::Field) -> Option<String> {
    let text = text(file, field)?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}
fn location(text: &str) -> PathBuf {
    file_uri_to_path(text).unwrap_or_else(|| PathBuf::from(text))
}
fn parse_timestamp(file: &File, field: wire::Field) -> Option<DateTime<Utc>> {
    let timestamp = field.message(file).ok()?;
    let seconds = timestamp
        .last(1)
        .and_then(|f| f.u64())
        .and_then(|s| i64::try_from(s).ok())
        .filter(|s| *s > 0)?;
    let nanos = timestamp
        .last(2)
        .and_then(|f| f.u64())
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n < 1_000_000_000)
        .unwrap_or(0);
    DateTime::from_timestamp(seconds, nanos)
}
fn parse_workspace(
    file: &File,
    field: wire::Field,
    cwd: &mut Option<PathBuf>,
    git_root: &mut Option<PathBuf>,
    git_branch: &mut Option<String>,
) {
    let Ok(workspace) = field.message(file) else {
        return;
    };
    for part in workspace.fields() {
        let Some(value) = trimmed(file, *part) else {
            continue;
        };
        match part.number {
            1 if cwd.is_none() => *cwd = Some(location(&value)),
            2 if git_root.is_none() => *git_root = Some(location(&value)),
            4 if git_branch.is_none() => *git_branch = Some(value),
            _ => {}
        }
    }
}

/// Fields are applied in wire order: the first workspace, git root and branch stand.
fn trajectory(file: &File, message: &Proto, out: &mut Meta) {
    for field in message.fields() {
        match field.number {
            1 => parse_workspace(file, *field, &mut out.cwd, &mut out.git_root, &mut out.git_branch),
            2 => {
                if let Some(ts) = parse_timestamp(file, *field) {
                    out.created_at = Some(ts);
                }
            }
            5 => out.parent_id = text(file, *field).or(out.parent_id.take()),
            6 => out.root_id = text(file, *field).or(out.root_id.take()),
            7 if out.cwd.is_none() => out.cwd = text(file, *field).and_then(|uri| file_uri_to_path(&uri)),
            8 => {
                let Ok(spec) = field.message(file) else {
                    continue;
                };
                for part in spec.fields() {
                    match (part.number, text(file, *part)) {
                        (1, Some(value)) => out.agent_type = Some(value),
                        (2, Some(value)) => out.agent_role = Some(value),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn raw_summary(file: &File, message: &Proto, out: &mut RawSummary) {
    for field in message.fields() {
        match field.number {
            1 => out.summary = trimmed(file, *field).or(out.summary.take()),
            7 => {
                if let Some(ts) = parse_timestamp(file, *field) {
                    out.created_at = Some(ts);
                }
            }
            9 => parse_workspace(file, *field, &mut out.cwd, &mut out.git_root, &mut out.git_branch),
            15 => {
                let Ok(ann) = field.message(file) else {
                    continue;
                };
                for part in ann.fields() {
                    match part.number {
                        1 => {
                            if let Some(val) = text(file, *part).filter(|s| !s.trim().is_empty()) {
                                out.annotations.title = Some(val);
                            }
                        }
                        3 => {
                            if let Some(tag) = trimmed(file, *part) {
                                out.annotations.tags.push(tag);
                            }
                        }
                        4 => {
                            if let Some(v) = part.u64() {
                                out.annotations.archived = Some(v != 0);
                            }
                        }
                        6 => {
                            if let Some(v) = part.u64() {
                                out.annotations.starred = Some(v != 0);
                            }
                        }
                        12 => {
                            if let Some(v) = part.u64() {
                                out.annotations.pinned = Some(v != 0);
                            }
                        }
                        14 => {
                            out.annotations.active_profile =
                                trimmed(file, *part).or(out.annotations.active_profile.take())
                        }
                        15 => out.annotations.group_id = trimmed(file, *part).or(out.annotations.group_id.take()),
                        _ => {}
                    }
                }
            }
            17 => {
                let Ok(traj) = field.message(file) else {
                    continue;
                };
                for part in traj.fields() {
                    match part.number {
                        1 => parse_workspace(file, *part, &mut out.cwd, &mut out.git_root, &mut out.git_branch),
                        2 if out.created_at.is_none() => out.created_at = parse_timestamp(file, *part),
                        5 => out.parent_id = trimmed(file, *part).or(out.parent_id.take()),
                        6 => out.root_id = trimmed(file, *part).or(out.root_id.take()),
                        7 if out.cwd.is_none() => {
                            out.cwd = text(file, *part).and_then(|uri| file_uri_to_path(&uri));
                        }
                        8 => {
                            let Ok(spec) = part.message(file) else {
                                continue;
                            };
                            for sp in spec.fields() {
                                match (sp.number, trimmed(file, *sp)) {
                                    (1, Some(val)) => out.agent_type = Some(val),
                                    (2, Some(val)) => out.agent_role = Some(val),
                                    _ => {}
                                }
                            }
                        }
                        18 => out.project_id = trimmed(file, *part).or(out.project_id.take()),
                        _ => {}
                    }
                }
            }
            23 => {
                if let Some(v) = field.u64() {
                    out.killed = Some(v != 0);
                }
            }
            24 => out.fork_parent_id = trimmed(file, *field).or(out.fork_parent_id.take()),
            _ => {}
        }
    }
}
