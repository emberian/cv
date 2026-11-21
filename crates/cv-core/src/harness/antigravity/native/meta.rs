//! Conversation metadata beside the steps, read through the same staged wire ranges:
//! `gen_metadata.data` (field `2` = step indices, packed or repeated; field `1.19` = model
//! slug) and `trajectory_metadata_blob.data` (`1.1` workspace URI, `1.2` git root, `1.4`
//! git branch, `2` created timestamp, `5` parent, `6` root, `7` workspace, `8.1`/`8.2`
//! agent type and role). A `gen_metadata` row also holds its whole model request (12 MiB
//! seen); only the named scalars leave the staging file.
use super::super::file_uri_to_path;
use super::{sqlite, wire, File, Proto, Result, Stage};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Scalar metadata text: a slug, an id, a URI. Longer values are not metadata.
const TEXT: usize = 64 * 1024;

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
                if let Some((file, message)) = cell("trajectory_metadata_blob", rowid)? {
                    trajectory(&file, &message, &mut out);
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

/// Fields are applied in wire order: the first workspace, git root and branch stand.
fn trajectory(file: &File, message: &Proto, out: &mut Meta) {
    for field in message.fields() {
        match field.number {
            1 => {
                let Ok(workspace) = field.message(file) else {
                    continue;
                };
                for part in workspace.fields() {
                    let Some(value) = trimmed(file, *part) else {
                        continue;
                    };
                    match part.number {
                        1 if out.cwd.is_none() => out.cwd = Some(location(&value)),
                        2 if out.git_root.is_none() => out.git_root = Some(location(&value)),
                        4 if out.git_branch.is_none() => out.git_branch = Some(value),
                        _ => {}
                    }
                }
            }
            2 => {
                let Ok(timestamp) = field.message(file) else {
                    continue;
                };
                let seconds = timestamp
                    .last(1)
                    .and_then(|field| field.u64())
                    .and_then(|seconds| i64::try_from(seconds).ok())
                    .filter(|seconds| *seconds > 0);
                let nanos = timestamp
                    .last(2)
                    .and_then(|field| field.u64())
                    .and_then(|nanos| u32::try_from(nanos).ok())
                    .unwrap_or(0);
                if let Some(seconds) = seconds {
                    out.created_at = DateTime::from_timestamp(seconds, nanos);
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
