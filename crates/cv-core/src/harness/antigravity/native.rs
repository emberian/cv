//! Bounded native protobuf projection. JSONL remains the historical authority when
//! present: native CLEARED steps are not replacements for retained exported text.
mod account;
mod context;
mod identity;
mod interaction;
mod media;
mod meta;
mod payload;
mod render;
mod wire;
use super::super::store::{bytes::ByteSource, sqlite};
use super::*;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use wire::Message as Proto;

pub(super) use meta::read as metadata;
pub(super) use wire::varint as read_varint;

pub(super) fn open(path: &Path, complete: bool) -> Result<jsonl::Read> {
    read(path, complete, -1)
}

/// The first `rows` steps only: what discovery needs to title a session.
pub(super) fn head(path: &Path, rows: u32) -> Result<jsonl::Read> {
    read(path, false, i64::from(rows))
}

/// Validates the native `steps` schema for discovery: `Err` when `path` is not a valid SQLite
/// database or lacks `steps(idx, step_payload, step_format)`, `Ok(false)` when `steps` holds
/// zero rows, and `Ok(true)` when at least one step row is present.
pub(super) fn has_steps(path: &Path) -> Result<bool> {
    use rusqlite::OptionalExtension as _;
    sqlite::open_ro(path)?.read_snapshot(|conn, _| {
        let _: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
        let columns = sqlite::Select::of(conn, "steps");
        anyhow::ensure!(
            columns.has("idx") && columns.has("step_payload") && columns.has("step_format"),
            "unsupported Antigravity native steps schema"
        );
        let row: Option<i64> = conn
            .query_row("SELECT 1 FROM steps LIMIT 1", [], |r| r.get(0))
            .optional()?;
        Ok(row.is_some())
    })
}

#[derive(Default, Clone, Copy)]
struct SideRow {
    step_type: Option<u64>,
    status: Option<u64>,
    metadata_len: i64,
    error_len: i64,
    permissions_len: i64,
    task_len: i64,
    render_len: i64,
}

#[derive(Default)]
struct SideCells {
    step_type: Option<u64>,
    status: Option<u64>,
    metadata: Option<Proto>,
    error: Option<Proto>,
    permissions: Option<Proto>,
    task: Option<Proto>,
    render: Option<Proto>,
}

fn read_side_proto(
    stage: &Stage,
    conn: &rusqlite::Connection,
    column: &str,
    rowid: i64,
    len: i64,
) -> Result<Option<Proto>> {
    if len <= 0 {
        return Ok(None);
    }
    let (at, byte_len) = stage.cell(conn, "steps", column, rowid)?;
    if byte_len == 0 {
        return Ok(None);
    }
    Ok(Some(Proto::read(&stage.file, at, byte_len)?))
}

fn read(path: &Path, complete: bool, limit: i64) -> Result<jsonl::Read> {
    let mut reads = ReadSet::default();
    record_sqlite_readset(path, &mut reads);
    let source = crate::json::JsonSource::capture_with_reads(path, &reads)?;
    let input = jsonl::Read::with(source)?;
    let output = Arc::new(ByteSource::arena()?);
    sqlite::open_ro(path)?.read_snapshot(|conn, _| project_rows(conn, complete, limit, &output))?;
    input.check()?;
    let len = output.byte_len()?;
    Ok(input.decoded(output, len))
}

/// Project rows through the caller's snapshot, including a successor before commit.
pub(super) fn project_rows(conn: &rusqlite::Connection, complete: bool, limit: i64, output: &ByteSource) -> Result<()> {
    let mut stage = Stage::new()?;
    let columns = sqlite::Select::of(conn, "steps");
    anyhow::ensure!(
        columns.has("idx") && columns.has("step_payload") && columns.has("step_format"),
        "unsupported Antigravity native steps schema"
    );
    let [meta_len, err_len, perm_len, task_len, render_len] = [
        "metadata",
        "error_details",
        "permissions",
        "task_details",
        "render_info",
    ]
    .map(|col| {
        if columns.has(col) {
            format!("COALESCE(length({col}), 0)")
        } else {
            "0".into()
        }
    });
    let select = columns.probed(&["step_type", "status"]).always(&[
        "rowid",
        "idx",
        "step_format",
        &meta_len,
        &err_len,
        &perm_len,
        &task_len,
        &render_len,
        "step_payload IS NOT NULL",
    ]);
    // A negative LIMIT is no limit.
    let sql = format!("SELECT {} FROM steps ORDER BY idx LIMIT ?1", select.list());
    let mut query = conn.prepare(&sql)?;
    let mut rows = query.query([limit])?;
    output.append(|mut out| {
        let opt_u64 = |row: &rusqlite::Row<'_>, idx: usize| -> rusqlite::Result<Option<u64>> {
            use rusqlite::types::ValueRef;
            Ok(match row.get_ref(idx)? {
                ValueRef::Integer(v) => u64::try_from(v).ok(),
                ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|s| s.trim().parse::<u64>().ok()),
                _ => None,
            })
        };
        while let Some(row) = rows.next()? {
            let side_row = SideRow {
                step_type: opt_u64(row, 0)?,
                status: opt_u64(row, 1)?,
                metadata_len: row.get(5)?,
                error_len: row.get(6)?,
                permissions_len: row.get(7)?,
                task_len: row.get(8)?,
                render_len: row.get(9)?,
            };
            let rowid: i64 = row.get(2)?;
            let index: i64 = row.get(3)?;
            let format: i64 = row.get(4)?;
            let has_payload: bool = row.get(10)?;
            anyhow::ensure!(format == 0, "unsupported Antigravity native step format {format}");
            anyhow::ensure!(index >= 0, "negative Antigravity step index");
            // Each cell is privately staged, then projected with positioned bounded
            // readers. Normal rows with `step_payload` do zero extra `stage.cell` reads.
            if stage.used.get() >= Stage::RENEW {
                stage = Stage::new()?;
            }
            let (cell, step) = if has_payload {
                let cell = stage.cell(conn, "steps", "step_payload", rowid)?;
                let step = Proto::read(&stage.file, cell.0, cell.1)?;
                (cell, step)
            } else {
                ((stage.used.get(), 0), Proto::default())
            };
            let mut side = SideCells {
                step_type: side_row.step_type,
                status: side_row.status,
                ..SideCells::default()
            };
            if step.last(5).is_none() {
                side.metadata = read_side_proto(&stage, conn, "metadata", rowid, side_row.metadata_len)?;
            }
            if step.last(31).is_none() {
                side.error = read_side_proto(&stage, conn, "error_details", rowid, side_row.error_len)?;
            }
            if step.last(56).is_none() && step.last(133).is_none() && step.last(147).is_none() {
                side.permissions = read_side_proto(&stage, conn, "permissions", rowid, side_row.permissions_len)?;
            }
            if step.last(148).is_none() {
                side.task = read_side_proto(&stage, conn, "task_details", rowid, side_row.task_len)?;
            }
            if side_row.render_len > 0 && !step_has_content_or_render(&stage.file, &step)? {
                side.render = read_side_proto(&stage, conn, "render_info", rowid, side_row.render_len)?;
            }
            project(&stage, &step, &side, cell, index as u64, complete, &mut out)?;
        }
        Ok(())
    })?;
    Ok(())
}

fn step_has_content_or_render(file: &File, step: &Proto) -> Result<bool> {
    let nonempty = |f: wire::Field| matches!(f.value, wire::Value::Bytes { len, .. } if len > 0);
    if let Some(generic) = step.last(140) {
        if let Some(result) = generic.message(file)?.last(2) {
            let result = result.message(file)?;
            if result.last(1).is_some()
                || result.last(3).is_some()
                || result.last(10).is_some()
                || result.last(11).is_some()
            {
                return Ok(true);
            }
        }
    }
    if let Some(mcp) = step.last(47) {
        let mcp = mcp.message(file)?;
        if mcp.last(3).is_some() || mcp.last(9).is_some() {
            return Ok(true);
        }
    }
    for (member, content_fields) in [
        (19u32, &[1u32, 2, 3][..]),
        (20, &[1, 3]),
        (114, &[1]),
        (30, &[5]),
        (24, &[3]),
        (28, &[4, 21]),
        (14, &[4, 9]),
        (15, &[1, 2, 3]),
        (10, &[26]),
        (98, &[5]),
        (13, &[3]),
        (34, &[11, 14]),
        (37, &[3, 9, 12]),
        (40, &[2]),
        (42, &[5]),
        (12, &[2]),
        (103, &[1]),
        (111, &[1]),
    ] {
        if let Some(sub) = step.last(member) {
            let sub = sub.message(file)?;
            if content_fields.iter().any(|&n| sub.last(n).is_some_and(nonempty)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The `invoke_subagent` or `swarm` (`action: "add"`) call that created each of `children`,
/// from the result that names the conversation (`InvokeSubagent.results[].conversation_id`,
/// direct `CortexStepInvokeSubagent` `step.last(143)`, or `swarm`'s `{"status":"added",...}` JSON).
/// The registry's `spawnStepIndex` is not that step: parallel spawns all record the last result.
pub(super) fn spawn_calls(path: &Path, children: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut calls = BTreeMap::new();
    if children.is_empty() {
        return Ok(calls);
    }
    sqlite::open_ro(path)?.read_snapshot(|conn, _| {
        if !sqlite::table_exists(conn, "steps") {
            return Ok(());
        }
        let has_metadata_col = sqlite::Select::of(conn, "steps").has("metadata");
        // SQLite finds the rows that mention a child; only those are staged and decoded.
        let mentions = vec!["instr(step_payload, CAST(? AS BLOB)) > 0"; children.len()].join(" OR ");
        let sql = if has_metadata_col {
            format!(
                "SELECT rowid, COALESCE(length(metadata), 0) FROM steps WHERE step_payload IS NOT NULL AND ({mentions}) ORDER BY idx"
            )
        } else {
            format!("SELECT rowid, 0 FROM steps WHERE step_payload IS NOT NULL AND ({mentions}) ORDER BY idx")
        };
        let mut query = conn.prepare(&sql)?;
        let rows: Vec<(i64, i64)> = query
            .query_map(rusqlite::params_from_iter(children.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let stage = Stage::new()?;
        let file = &stage.file;
        let record_direct_invoke = |inv: &Proto, id: &str, calls: &mut BTreeMap<String, String>| -> Result<()> {
            let text = |message: &Proto, number: u32| message.last(number).and_then(|f| f.string(file, 256).ok());
            let mut matched = false;
            for created in inv.repeated(10) {
                if let Some(child) = text(&created.message(file)?, 1).filter(|s| !s.is_empty()) {
                    matched = true;
                    if children.contains(&child.as_str()) {
                        calls.entry(child).or_insert_with(|| id.to_owned());
                    }
                }
            }
            if !matched {
                if let Some(child) = text(inv, 5).filter(|s| !s.is_empty()) {
                    if children.contains(&child.as_str()) {
                        calls.entry(child).or_insert_with(|| id.to_owned());
                    }
                }
            }
            Ok(())
        };
        for (row, meta_len) in rows {
            let (at, len) = stage.cell(conn, "steps", "step_payload", row)?;
            let step = Proto::read(file, at, len)?;
            let inner = |message: &Proto, number: u32| message.last(number).map(|f| f.message(file)).transpose();
            let text = |message: &Proto, number: u32| message.last(number).and_then(|f| f.string(file, 256).ok());
            let metadata = match inner(&step, 5)? {
                Some(m) => Some(m),
                None => read_side_proto(&stage, conn, "metadata", row, meta_len)?,
            };
            let Some(call) = metadata
                .as_ref()
                .map(|m| inner(m, 4))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            let Some(id) = text(&call, 1).filter(|s| !s.is_empty()) else {
                continue;
            };
            let tool = text(&call, 2);
            if let Some(inv) = inner(&step, 143)? {
                if tool.as_deref().is_none_or(|t| t == "invoke_subagent") {
                    record_direct_invoke(&inv, &id, &mut calls)?;
                }
                continue;
            }
            let Some(tool) = tool else {
                continue;
            };
            if !matches!(tool.as_str(), "invoke_subagent" | "swarm") {
                continue;
            }
            let Some(result) = inner(&step, 140)?
                .map(|generic| inner(&generic, 2))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            if tool == "swarm" {
                if let Some(body) = result.last(1).and_then(|f| f.string(file, 64 * 1024).ok()) {
                    for child in super::extract_swarm_added_ids(&body) {
                        if children.contains(&child.as_str()) {
                            calls.entry(child).or_insert_with(|| id.clone());
                        }
                    }
                }
                continue;
            }
            let Some(any) = inner(&result, 6)? else {
                continue;
            };
            let Some(type_url) = text(&any, 1) else {
                continue;
            };
            let Some(spawned) = inner(&any, 2)? else {
                continue;
            };
            if type_url.ends_with("/exa.cortex.core.tools.InvokeSubagent") {
                for created in spawned.repeated(2) {
                    if let Some(child) = text(&created.message(file)?, 1) {
                        if children.contains(&child.as_str()) {
                            calls.entry(child).or_insert_with(|| id.clone());
                        }
                    }
                }
            } else if type_url.ends_with("/gemini_coder.Step") {
                if let Some(inv) = inner(&spawned, 143)? {
                    record_direct_invoke(&inv, &id, &mut calls)?;
                }
            }
        }
        Ok(())
    })?;
    Ok(calls)
}

/// Private staging for native cells and for text assembled from them. One unlinked file
/// serves many rows: a file per row cost an open, an unlink and a close for each of a
/// session's thousands of steps. Ranges are immutable once appended.
pub(super) struct Stage {
    arena: ByteSource,
    file: File,
    used: std::cell::Cell<u64>,
}
impl Stage {
    /// Staged bytes after which the next row starts a new file, releasing the old one.
    const RENEW: u64 = 64 << 20;
    /// Maximum single SQLite BLOB cell staged into disk scratch (guards hostile `zeroblob`s).
    const MAX_CELL_BYTES: u64 = 256 << 20;

    fn new() -> Result<Self> {
        let arena = ByteSource::arena()?;
        let file = arena.file()?;
        Ok(Self {
            arena,
            file,
            used: Default::default(),
        })
    }
    fn append(&self, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<(u64, u64)> {
        let range = self.arena.append(write)?;
        self.used.set(range.0 + range.1);
        Ok(range)
    }
    fn cell(&self, conn: &rusqlite::Connection, table: &str, column: &str, rowid: i64) -> Result<(u64, u64)> {
        self.append(|out| {
            sqlite::with_cell_reader(conn, table, column, rowid, |reader, len| {
                anyhow::ensure!(
                    len as u64 <= Self::MAX_CELL_BYTES,
                    "native SQLite cell in {table}.{column} exceeds {} MiB limit",
                    Self::MAX_CELL_BYTES >> 20
                );
                let copied = std::io::copy(&mut reader.take(Self::MAX_CELL_BYTES + 1), out)?;
                anyhow::ensure!(
                    copied <= Self::MAX_CELL_BYTES,
                    "native SQLite cell in {table}.{column} exceeds {} MiB limit",
                    Self::MAX_CELL_BYTES >> 20
                );
                Ok(())
            })
        })
    }
}

fn timestamp(ts: &Proto) -> Option<DateTime<Utc>> {
    let seconds = scalar(ts, 1).and_then(|s| i64::try_from(s).ok())?;
    let nanos = match scalar(ts, 2) {
        Some(n) => u32::try_from(n).ok().filter(|&n| n < 1_000_000_000)?,
        None => 0,
    };
    DateTime::from_timestamp(seconds, nanos)
}

fn scalar(p: &Proto, field: u32) -> Option<u64> {
    p.last(field).and_then(|f| f.u64())
}
fn text(file: &File, p: &Proto, number: u32, key: &str, out: &mut impl Write) -> Result<()> {
    if let Some(field) = p.last(number) {
        write!(out, ",\"{key}\":")?;
        field.write_json_string(file, out)?;
    }
    Ok(())
}
fn calls(file: &File, p: &Proto, out: &mut impl Write) -> Result<()> {
    out.write_all(b",\"tool_calls\":[")?;
    for (i, field) in p.repeated(7).enumerate() {
        if i != 0 {
            out.write_all(b",")?;
        }
        let call = field.message(file)?;
        out.write_all(b"{\"native\":true")?;
        text(file, &call, 1, "id", out)?;
        text(file, &call, 2, "name", out)?;
        if call.last(3).is_some() {
            text(file, &call, 3, "arguments", out)?;
        } else {
            text(file, &call, 4, "arguments", out)?;
        }
        out.write_all(b"}")?;
    }
    out.write_all(b"]")?;
    Ok(())
}
fn nonempty(field: wire::Field) -> bool {
    matches!(field.value, wire::Value::Bytes { len, .. } if len > 0)
}

fn write_preferred_text(file: &File, p: &Proto, fields: &[u32], key: &str, out: &mut impl Write) -> Result<bool> {
    let selected = fields
        .iter()
        .find_map(|&n| p.last(n).filter(|f| nonempty(*f)))
        .or_else(|| fields.iter().find_map(|&n| p.last(n)));
    if let Some(field) = selected {
        write!(out, ",\"{key}\":")?;
        field.write_json_string(file, out)?;
        return Ok(true);
    }
    Ok(false)
}

fn nested_field(file: &File, root: &Proto, path: &[u32]) -> Result<Option<wire::Field>> {
    let Some((&last, parents)) = path.split_last() else {
        return Ok(None);
    };
    let mut owned;
    let mut current = root;
    for &num in parents {
        let Some(field) = current.last(num) else {
            return Ok(None);
        };
        if !matches!(field.value, wire::Value::Bytes { .. }) {
            return Ok(None);
        }
        owned = field.message(file)?;
        current = &owned;
    }
    Ok(current.last(last))
}

fn list_directory(stage: &Stage, dir: &Proto, out: &mut impl Write) -> Result<bool> {
    if dir.last(1).is_none() && dir.last(2).is_none() && dir.last(3).is_none() {
        return Ok(false);
    }
    let file = &stage.file;
    let (at, len) = stage.append(|mut output| {
        output.write_all(b"{")?;
        let mut has_path = false;
        if let Some(path) = dir.last(1) {
            output.write_all(b"\"path\":")?;
            path.write_json_string(file, &mut output)?;
            has_path = true;
        }
        output.write_all(if has_path { b",\"entries\":[" } else { b"\"entries\":[" })?;
        if dir.last(3).is_some() {
            for (i, item) in dir.repeated(3).enumerate() {
                if i != 0 {
                    output.write_all(b",")?;
                }
                let entry = item.message(file)?;
                output.write_all(b"{")?;
                let mut first = true;
                if let Some(name) = entry.last(1) {
                    first = false;
                    output.write_all(b"\"name\":")?;
                    name.write_json_string(file, &mut output)?;
                }
                if let Some(is_dir) = entry.last(2).and_then(|f| f.u64()) {
                    if !std::mem::take(&mut first) {
                        output.write_all(b",")?;
                    }
                    write!(output, "\"is_dir\":{}", is_dir != 0)?;
                }
                if let Some(size) = entry.last(4).and_then(|f| f.u64()) {
                    if !std::mem::take(&mut first) {
                        output.write_all(b",")?;
                    }
                    write!(output, "\"size_bytes\":{size}")?;
                }
                output.write_all(b"}")?;
            }
        } else {
            for (i, child) in dir.repeated(2).enumerate() {
                if i != 0 {
                    output.write_all(b",")?;
                }
                child.write_json_string(file, &mut output)?;
            }
        }
        output.write_all(b"]}")?;
        Ok(())
    })?;
    out.write_all(b",\"content\":")?;
    crate::projection::write_json_string(out, wire::range(file, at, len)?)?;
    out.write_all(b",\"native_structured_result\":")?;
    std::io::copy(&mut wire::range(file, at, len)?, out)?;
    Ok(true)
}

fn project(
    stage: &Stage,
    step: &Proto,
    side: &SideCells,
    cell: (u64, u64),
    index: u64,
    complete: bool,
    out: &mut impl Write,
) -> Result<()> {
    use payload::Kind;
    let file = &stage.file;
    // The `step` member set names the type. A cleared row can lose its member and keep
    // only `Step.type` (CortexStepType) or SQLite `steps.step_type`.
    const KINDS: [(u32, u64, &str); 25] = [
        (19, 14, "USER_INPUT"),
        (20, 15, "PLANNER_RESPONSE"),
        (140, 132, "GENERIC"),
        (114, 101, "SYSTEM_MESSAGE"),
        (30, 23, "CHECKPOINT"),
        (24, 17, "ERROR_MESSAGE"),
        (28, 21, "RUN_COMMAND"),
        (14, 8, "VIEW_FILE"),
        (15, 9, "LIST_DIRECTORY"),
        (10, 5, "CODE_ACTION"),
        (23, 0, "WRITE_TO_FILE"),
        (98, 86, "FILE_CHANGE"),
        (13, 7, "GREP_SEARCH"),
        (34, 25, "FIND"),
        (37, 28, "COMMAND_STATUS"),
        (40, 31, "READ_URL_CONTENT"),
        (42, 33, "SEARCH_WEB"),
        (47, 38, "MCP_TOOL"),
        (62, 51, "LIST_RESOURCES"),
        (63, 52, "READ_RESOURCE"),
        (12, 2, "FINISH"),
        (143, 127, "INVOKE_SUBAGENT"),
        (154, 138, "ASK_QUESTION"),
        (103, 90, "EPHEMERAL_MESSAGE"),
        (111, 98, "CONVERSATION_HISTORY"),
    ];
    let declared = scalar(step, 1).or(side.step_type);
    let (kind, payload) = KINDS
        .iter()
        .find_map(|(member, _, kind)| step.last(*member).map(|payload| (*kind, Some(payload))))
        .or_else(|| {
            let (_, _, kind) = KINDS.iter().find(|(_, ty, _)| *ty != 0 && declared == Some(*ty))?;
            Some((*kind, None))
        })
        .unwrap_or(("NATIVE_UNKNOWN", None));
    let payload = payload.map(|payload| payload.message(file)).transpose()?;
    let status = match scalar(step, 4).or(side.status).unwrap_or(0) {
        1 => "PENDING",
        2 => "RUNNING",
        3 => "DONE",
        4 => "INVALID",
        5 => "CLEARED",
        6 => "CANCELED",
        7 => "ERROR",
        8 => "GENERATING",
        9 => "WAITING",
        11 => "QUEUED",
        12 => "INTERRUPTED",
        _ => "UNSPECIFIED",
    };
    write!(
        out,
        "{{\"step_index\":{index},\"type\":\"{kind}\",\"status\":\"{status}\",\"native_store\":true"
    )?;
    // COMPLETE retains the wire record; the normalized view discloses what it left unread.
    let mut unsupported = account::unread_fields(file, step, kind, payload.as_ref())?
        || account::unread_side(file, side.metadata.as_ref(), side.error.as_ref(), side.task.as_ref())?;
    if status == "CLEARED" {
        out.write_all(b",\"native_cleared\":true")?;
    }
    let step_meta = step.last(5).map(|f| f.message(file)).transpose()?;
    if let Some(meta) = step_meta.as_ref().or(side.metadata.as_ref()) {
        let source = match scalar(meta, 3).unwrap_or(0) {
            2 => "MODEL",
            3 => "USER_IMPLICIT",
            4 => "USER_EXPLICIT",
            5 => "SYSTEM",
            6 => "SYSTEM_SDK",
            _ => "UNSPECIFIED",
        };
        write!(out, ",\"source\":\"{source}\"")?;
        if let Some(ts_field) = meta.last(1) {
            if let Some(ts) = timestamp(&ts_field.message(file)?) {
                write!(out, ",\"created_at\":{}", serde_json::to_string(&ts.to_rfc3339())?)?;
            }
        }
        if let Some(field) = meta.last(4) {
            let call = field.message(file)?;
            text(file, &call, 1, "native_tool_id", out)?;
            text(file, &call, 2, "native_tool_name", out)?;
        }
        if let Some(field) = meta.last(9) {
            let usage = field.message(file)?;
            for (number, key) in [
                (2, "input_tokens"),
                (3, "output_tokens"),
                (4, "cache_creation_tokens"),
                (5, "cache_read_tokens"),
                (9, "reasoning_tokens"),
            ] {
                if let Some(value) = scalar(&usage, number) {
                    write!(out, ",\"{key}\":{value}")?;
                }
            }
        }
    }
    let mut has_step_error = false;
    let step_error = step.last(31).map(|f| f.message(file)).transpose()?;
    if let Some(error) = step_error.as_ref().or(side.error.as_ref()) {
        if let Some(number) = [1, 9, 3, 2, 5].into_iter().find(|number| error.last(*number).is_some()) {
            text(file, error, number, "error", out)?;
            has_step_error = true;
        }
    }
    let step_task = step.last(148).map(|f| f.message(file)).transpose()?;
    if let Some(details) = step_task.as_ref().or(side.task.as_ref()) {
        context::task(file, details, out)?;
    }
    let mut missing_attachments = false;
    let has_interaction =
        step.last(56).is_some() || step.last(133).is_some() || step.last(147).is_some() || side.permissions.is_some();
    let interpreted = interaction::project(file, step, side.permissions.as_ref(), out)?;
    unsupported |= !interpreted && has_interaction;
    let mut has_content = false;
    let mut has_embedded_render = false;
    match (kind, &payload) {
        ("USER_INPUT", Some(input)) => {
            context::user(file, input, out)?;
            if input.last(1).is_some() {
                text(file, input, 1, "content", out)?;
                has_content = true;
            }
            missing_attachments |= media::project(file, input, &[9], &[6], out)?;
        }
        ("PLANNER_RESPONSE", Some(response)) => {
            if response.last(1).is_some() {
                text(file, response, 1, "content", out)?;
                has_content = true;
            }
            text(file, response, 3, "thinking", out)?;
            text(file, response, 4, "thinking_signature", out)?;
            if scalar(response, 5) == Some(1) {
                out.write_all(b",\"thinking_redacted\":true")?;
            }
            calls(file, response, out)?;
            identity::planner(file, response, out)?;
            missing_attachments |= media::project(file, response, &[20], &[], out)?;
        }
        ("GENERIC", Some(generic)) => {
            if let Some(field) = generic.last(2) {
                let result = field.message(file)?;
                has_content = if result.last(1).is_some() {
                    text(file, &result, 1, "content", out)?;
                    if let Some(structure) = result.last(10) {
                        unsupported |= !structured_result(stage, structure.message(file)?, true, out)?;
                    } else if let Some(value) = result.last(11) {
                        unsupported |= !structured_result(stage, value.message(file)?, false, out)?;
                    }
                    true
                } else if let Some(structure) = result.last(10) {
                    let ok = structured_content(stage, structure.message(file)?, true, out)?;
                    unsupported |= !ok;
                    ok
                } else if let Some(value) = result.last(11) {
                    let ok = structured_content(stage, value.message(file)?, false, out)?;
                    unsupported |= !ok;
                    ok
                } else {
                    false
                };
                if let Some(info) = result.last(3) {
                    has_embedded_render = true;
                    let info = info.message(file)?;
                    render::agents(file, &info, out)?;
                    if !has_content {
                        has_content = render::content(stage, &info, out)?;
                    }
                }
                text(file, &result, 7, "native_output_uri", out)?;
                if let Some(any) = result.last(6) {
                    unsupported |= !payload::project(file, &any.message(file)?, out)?;
                }
                missing_attachments |= media::project(file, &result, &[4], &[], out)?;
            }
        }
        ("SYSTEM_MESSAGE", Some(message)) => {
            if message.last(1).is_some() {
                text(file, message, 1, "content", out)?;
                has_content = true;
            }
            missing_attachments |= media::project(file, message, &[6], &[], out)?;
        }
        ("CHECKPOINT", Some(checkpoint)) => {
            if checkpoint.last(5).is_some() {
                text(file, checkpoint, 5, "content", out)?;
                has_content = true;
            }
            context::checkpoint(file, checkpoint, out)?;
        }
        ("ERROR_MESSAGE", Some(message)) => {
            if let Some(error) = message.last(3) {
                let error = error.message(file)?;
                let preferred = [1, 9, 3, 2, 5].into_iter().find(|number| error.last(*number).is_some());
                if let Some(number) = preferred {
                    text(file, &error, number, "content", out)?;
                    has_content = true;
                }
                // The export's `error` is the short or full error, whichever text is preferred.
                if !has_step_error {
                    if let Some(err_num) = [2, 3].into_iter().find(|number| error.last(*number).is_some()) {
                        text(file, &error, err_num, "error", out)?;
                    }
                }
            }
        }
        ("RUN_COMMAND", Some(cmd)) => {
            let combined = nested_field(file, cmd, &[21, 1])?;
            let stdout = cmd.last(4);
            if let Some(body) = combined
                .filter(|f| nonempty(*f))
                .or_else(|| stdout.filter(|f| nonempty(*f)))
                .or(combined)
                .or(stdout)
            {
                out.write_all(b",\"content\":")?;
                body.write_json_string(file, out)?;
                has_content = true;
            }
            payload::write_typed(
                file,
                "CortexStepRunCommand",
                cmd,
                &[
                    (6, "exit_code", Kind::Int),
                    (2, "cwd", Kind::Text),
                    (31, "shell_name", Kind::Text),
                    (37, "timed_out", Kind::Flag),
                ],
                out,
            )?;
        }
        ("VIEW_FILE", Some(view)) => {
            has_content = write_preferred_text(file, view, &[4, 9], "content", out)?;
            payload::write_typed(
                file,
                "CortexStepViewFile",
                view,
                &[
                    (1, "path", Kind::Text),
                    (2, "start_line", Kind::Int),
                    (3, "end_line", Kind::Int),
                    (11, "file_num_lines", Kind::Uint),
                    (12, "file_num_bytes", Kind::Uint),
                ],
                out,
            )?;
            if view.last(15).is_some() || view.last(14).is_some() {
                missing_attachments |= media::project(file, view, &[15], &[14], out)?;
            }
        }
        ("LIST_DIRECTORY", Some(dir)) => {
            has_content = list_directory(stage, dir, out)?;
        }
        ("CODE_ACTION", Some(action)) => {
            if let Some(desc) = action.last(26).filter(|f| nonempty(*f)) {
                out.write_all(b",\"content\":")?;
                desc.write_json_string(file, out)?;
                has_content = true;
            }
            let mut path_field = None;
            for candidate in [&[2u32, 1, 8][..], &[1, 2, 2, 5], &[1, 1, 4, 5]] {
                if let Some(found) = nested_field(file, action, candidate)?.filter(|f| nonempty(*f)) {
                    path_field = Some(found);
                    break;
                }
            }
            out.write_all(b",\"native_payload\":{\"type\":\"CortexStepCodeAction\"")?;
            if let Some(path_field) = path_field {
                out.write_all(b",\"path\":")?;
                path_field.write_json_string(file, out)?;
            }
            payload::members(
                file,
                action,
                &[
                    (28, "diff_stats", Kind::Message(payload::DIFF_STATS_FIELDS)),
                    (21, "is_artifact_file", Kind::Flag),
                ],
                out,
                &mut false,
            )?;
            out.write_all(b"}")?;
        }
        ("WRITE_TO_FILE", Some(write_step)) => {
            payload::write_typed(
                file,
                "CortexStepWriteToFile",
                write_step,
                &[(1, "path", Kind::Text), (4, "file_created", Kind::Flag)],
                out,
            )?;
        }
        ("FILE_CHANGE", Some(change)) => {
            if let Some(instruction) = change.last(5).filter(|f| nonempty(*f)) {
                out.write_all(b",\"content\":")?;
                instruction.write_json_string(file, out)?;
                has_content = true;
            }
            payload::write_typed(file, "CortexStepFileChange", change, &[(1, "path", Kind::Text)], out)?;
        }
        ("GREP_SEARCH", Some(grep)) => {
            if grep.last(3).is_some() {
                text(file, grep, 3, "content", out)?;
                has_content = true;
            }
            if !has_step_error {
                if let Some(err) = grep.last(5).filter(|f| nonempty(*f)) {
                    out.write_all(b",\"error\":")?;
                    err.write_json_string(file, out)?;
                }
            }
            payload::write_typed(
                file,
                "CortexStepGrepSearch",
                grep,
                &[(1, "query", Kind::Text), (7, "total_results", Kind::Uint)],
                out,
            )?;
        }
        ("FIND", Some(find)) => {
            has_content = write_preferred_text(file, find, &[11, 14], "content", out)?;
            if !has_step_error {
                if let Some(err) = find.last(8).filter(|f| nonempty(*f)) {
                    out.write_all(b",\"error\":")?;
                    err.write_json_string(file, out)?;
                }
            }
            payload::write_typed(
                file,
                "CortexStepFind",
                find,
                &[(1, "pattern", Kind::Text), (7, "total_results", Kind::Uint)],
                out,
            )?;
        }
        ("COMMAND_STATUS", Some(status_step)) => {
            has_content = write_preferred_text(file, status_step, &[9, 3, 12], "content", out)?;
            payload::write_typed(
                file,
                "CortexStepCommandStatus",
                status_step,
                &[(1, "command_id", Kind::Text), (5, "exit_code", Kind::Int)],
                out,
            )?;
        }
        ("READ_URL_CONTENT", Some(url_step)) => {
            if let Some(doc_text) = nested_field(file, url_step, &[2, 2])? {
                out.write_all(b",\"content\":")?;
                doc_text.write_json_string(file, out)?;
                has_content = true;
            }
            payload::write_typed(
                file,
                "CortexStepReadUrlContent",
                url_step,
                &[(1, "url", Kind::Text), (3, "resolved_url", Kind::Text)],
                out,
            )?;
        }
        ("SEARCH_WEB", Some(search)) => {
            if search.last(5).is_some() {
                text(file, search, 5, "content", out)?;
                has_content = true;
            }
            payload::write_typed(
                file,
                "CortexStepSearchWeb",
                search,
                &[(1, "query", Kind::Text), (3, "domain", Kind::Text)],
                out,
            )?;
        }
        ("MCP_TOOL", Some(mcp)) => {
            if mcp.last(3).is_some() {
                text(file, mcp, 3, "content", out)?;
                has_content = true;
            }
            if let Some(info) = mcp.last(9) {
                has_embedded_render = true;
                let info = info.message(file)?;
                render::agents(file, &info, out)?;
                if !has_content {
                    has_content = render::content(stage, &info, out)?;
                }
            }
            text(file, mcp, 8, "native_output_uri", out)?;
            payload::write_typed(file, "CortexStepMcpTool", mcp, &[(1, "server_name", Kind::Text)], out)?;
            if mcp.last(6).is_some() || mcp.last(5).is_some() {
                missing_attachments |= media::project(file, mcp, &[6], &[5], out)?;
            }
        }
        ("LIST_RESOURCES", Some(res)) => {
            payload::write_typed(
                file,
                "CortexStepListResources",
                res,
                &[(1, "server_name", Kind::Text), (2, "uri", Kind::Text)],
                out,
            )?;
        }
        ("READ_RESOURCE", Some(res)) => {
            payload::write_typed(
                file,
                "CortexStepReadResource",
                res,
                &[(1, "server_name", Kind::Text), (2, "uri", Kind::Text)],
                out,
            )?;
        }
        ("FINISH", Some(finish)) => {
            if finish.last(2).is_some() {
                text(file, finish, 2, "content", out)?;
                has_content = true;
            }
        }
        ("INVOKE_SUBAGENT", Some(inv)) => {
            let mut cids = Vec::new();
            for item in inv.repeated(10) {
                if let Some(cid) = item.message(file)?.last(1).filter(|f| nonempty(*f)) {
                    cids.push(cid);
                }
            }
            if cids.is_empty() {
                if let Some(cid) = inv.last(5).filter(|f| nonempty(*f)) {
                    cids.push(cid);
                }
            }
            out.write_all(b",\"native_payload\":{\"type\":\"CortexStepInvokeSubagent\"")?;
            if !cids.is_empty() {
                out.write_all(b",\"conversation_ids\":[")?;
                for (i, cid) in cids.iter().enumerate() {
                    if i != 0 {
                        out.write_all(b",")?;
                    }
                    cid.write_json_string(file, out)?;
                }
                out.write_all(b"]")?;
            }
            payload::members(file, inv, &[(11, "task_mode", Kind::Flag)], out, &mut false)?;
            out.write_all(b"}")?;
        }
        ("ASK_QUESTION", Some(ask)) => {
            payload::write_typed(file, "CortexStepAskQuestion", ask, payload::ASK_QUESTION_FIELDS, out)?;
        }
        ("EPHEMERAL_MESSAGE", Some(message)) => {
            if message.last(1).is_some() {
                text(file, message, 1, "content", out)?;
                has_content = true;
            }
            missing_attachments |= media::project(file, message, &[2, 4], &[], out)?;
        }
        ("CONVERSATION_HISTORY", Some(history)) if history.last(1).is_some() => {
            text(file, history, 1, "content", out)?;
            has_content = true;
        }
        _ => {}
    }
    if !has_content && !has_embedded_render {
        if let Some(info) = &side.render {
            render::agents(file, info, out)?;
            render::content(stage, info, out)?;
        }
    }
    if status == "CLEARED" {
        out.write_all(b",\"native_missing_payloads\":true")?;
    }
    if missing_attachments {
        out.write_all(b",\"native_missing_attachments\":true")?;
    }
    if unsupported {
        out.write_all(b",\"native_unsupported\":true")?;
    }
    if complete {
        out.write_all(b",\"native_record\":{\"encoding\":\"protobuf-hex\",\"data\":\"")?;
        let mut reader = wire::range(file, cell.0, cell.1)?;
        let mut bytes = [0u8; 8192];
        let digits = b"0123456789abcdef";
        let mut hex = [0u8; 16384];
        loop {
            let n = reader.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            for (byte, pair) in bytes[..n].iter().zip(hex.as_chunks_mut::<2>().0) {
                pair[0] = digits[(byte >> 4) as usize];
                pair[1] = digits[(byte & 15) as usize];
            }
            out.write_all(&hex[..n * 2])?;
        }
        out.write_all(b"\"}")?;
    }
    out.write_all(b"}\n")?;
    Ok(())
}

fn stage_structured(stage: &Stage, message: Proto, structure: bool) -> Result<(bool, u64, u64)> {
    let file = &stage.file;
    let mut supported = false;
    let (at, len) = stage.append(|mut output| {
        supported = if structure {
            struct_json(file, &message, &mut output, 0)?
        } else {
            value_json(file, &message, &mut output, 0)?
        };
        Ok(())
    })?;
    Ok((supported, at, len))
}

fn structured_content(stage: &Stage, message: Proto, structure: bool, out: &mut impl Write) -> Result<bool> {
    let (supported, at, len) = stage_structured(stage, message, structure)?;
    if supported {
        out.write_all(b",\"content\":")?;
        crate::projection::write_json_string(out, wire::range(&stage.file, at, len)?)?;
    }
    Ok(supported)
}

fn structured_result(stage: &Stage, message: Proto, structure: bool, out: &mut impl Write) -> Result<bool> {
    let (supported, at, len) = stage_structured(stage, message, structure)?;
    if supported {
        out.write_all(b",\"native_structured_result\":")?;
        std::io::copy(&mut wire::range(&stage.file, at, len)?, out)?;
    }
    Ok(supported)
}

fn struct_json(file: &File, message: &Proto, out: &mut impl Write, depth: usize) -> Result<bool> {
    anyhow::ensure!(depth < 64, "native structured result nesting limit exceeded");
    out.write_all(b"{")?;
    for (i, field) in message.repeated(1).enumerate() {
        if i != 0 {
            out.write_all(b",")?;
        }
        let entry = field.message(file)?;
        match entry.last(1) {
            Some(key) => key.write_json_string(file, out)?,
            None => out.write_all(b"\"\"")?,
        }
        out.write_all(b":")?;
        match entry.last(2) {
            Some(value) => {
                if !value_json(file, &value.message(file)?, out, depth + 1)? {
                    return Ok(false);
                }
            }
            None => out.write_all(b"null")?,
        }
    }
    out.write_all(b"}")?;
    Ok(true)
}

/// google.protobuf.Value and genai.Value share fields 1..6. The latter's content
/// and function variants cannot be represented as ordinary JSON; retain their wire
/// record and mark the projection incomplete instead of manufacturing null values.
fn value_json(file: &File, message: &Proto, out: &mut impl Write, depth: usize) -> Result<bool> {
    anyhow::ensure!(depth < 64, "native structured result nesting limit exceeded");
    let Some(field) = message.fields().last().copied() else {
        out.write_all(b"null")?;
        return Ok(true);
    };
    match (field.number, field.value) {
        (1, wire::Value::Varint(_)) => out.write_all(b"null")?,
        (2, wire::Value::Fixed64(bits)) => {
            let number = f64::from_bits(bits);
            if !number.is_finite() {
                return Ok(false);
            }
            serde_json::to_writer(out, &number)?;
        }
        (3, wire::Value::Bytes { .. }) => field.write_json_string(file, out)?,
        (4, wire::Value::Varint(value)) => out.write_all(if value == 0 { b"false" } else { b"true" })?,
        (5, wire::Value::Bytes { .. }) => return struct_json(file, &field.message(file)?, out, depth + 1),
        (6, wire::Value::Bytes { .. }) => {
            out.write_all(b"[")?;
            let list = field.message(file)?;
            for (i, value) in list.repeated(1).enumerate() {
                if i != 0 {
                    out.write_all(b",")?;
                }
                if !value_json(file, &value.message(file)?, out, depth + 1)? {
                    return Ok(false);
                }
            }
            out.write_all(b"]")?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 128 {
            out.push(value as u8 | 128);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn bytes(number: u64, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3 | 2, &mut out);
        varint(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }

    fn uint(number: u64, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3, &mut out);
        varint(value, &mut out);
        out
    }

    fn projection(data: &[u8], complete: bool) -> serde_json::Value {
        // An earlier row in the same staging file: ranges, not the file, bound a cell.
        let stage = Stage::new().unwrap();
        stage.append(|out| Ok(out.write_all(b"earlier row")?)).unwrap();
        let cell = stage.append(|out| Ok(out.write_all(data)?)).unwrap();
        let message = Proto::read(&stage.file, cell.0, cell.1).unwrap();
        let mut output = Vec::new();
        project(&stage, &message, &SideCells::default(), cell, 17, complete, &mut output).unwrap();
        serde_json::from_slice(&output).unwrap()
    }

    #[test]
    fn native_media_and_missing_payloads_are_distinct() {
        let existing = std::env::current_exe().unwrap();
        let existing = existing.to_str().unwrap();
        let uri_media = [bytes(1, b"image/png"), bytes(5, existing.as_bytes())].concat();
        let inline_media = [bytes(1, b"image/png"), bytes(2, b"image bytes")].concat();
        for (field, media_field) in [(19, 9), (20, 20), (114, 6)] {
            let content = [bytes(1, b"caption"), bytes(media_field, &uri_media)].concat();
            let projected = projection(&bytes(field, &content), false);
            assert_eq!(projected["media"][0]["uri"], existing);
            assert_eq!(projected["media"][0]["mime_type"], "image/png");
            assert!(projected.get("native_missing_attachments").is_none());
            assert!(projected.get("native_missing_payloads").is_none());
            let projected = projection(&bytes(field, &bytes(media_field, &inline_media)), false);
            assert!(projected.get("native_missing_attachments").is_none());
            // Inline media is read: nothing on this step is left uninterpreted.
            assert!(projected.get("native_unsupported").is_none());
            assert!(projected.get("native_missing_payloads").is_none());
        }
        let result = [bytes(1, b"output"), bytes(4, &uri_media)].concat();
        let projected = projection(&bytes(140, &bytes(2, &result)), false);
        assert_eq!(projected["media"][0]["uri"], existing);

        // A linked file that is gone: its inline copy is read; with no copy the step says so.
        let gone = b"file:///nonexistent/cv-fixture/upload.png";
        let both = [bytes(1, b"image/png"), bytes(2, b"image bytes"), bytes(5, gone)].concat();
        let projected = projection(&bytes(19, &bytes(9, &both)), false);
        assert_eq!(projected["media"][0]["uri"], "data:image/png;base64,aW1hZ2UgYnl0ZXM=");
        assert!(projected.get("native_missing_attachments").is_none());
        let link_only = [bytes(1, b"image/png"), bytes(5, gone)].concat();
        let projected = projection(&bytes(19, &bytes(9, &link_only)), false);
        assert_eq!(
            projected["media"][0]["uri"],
            "file:///nonexistent/cv-fixture/upload.png"
        );
        assert_eq!(projected["native_missing_attachments"], true);
    }

    #[test]
    fn native_terminal_missing_results_and_complete_wire_are_explicit() {
        for status in [2, 3, 5, 6] {
            let data = [uint(4, status), bytes(140, &[])].concat();
            let projected = projection(&data, true);
            // An empty result holds no field a decoder skipped.
            assert!(projected.get("native_unsupported").is_none());
            assert_eq!(projected.get("native_missing_payloads").is_some(), status == 5);
            assert_eq!(projected["native_record"]["encoding"], "protobuf-hex");
            let expected: String = data.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(projected["native_record"]["data"], expected);
        }
    }

    #[test]
    fn native_structured_results_preserve_json_and_disclose_unsupported_values() {
        let list = [bytes(1, &bytes(3, b"hello\nworld")), bytes(1, &uint(4, 1))].concat();
        let entry = [bytes(1, b"values"), bytes(2, &bytes(6, &list))].concat();
        let data = bytes(140, &bytes(2, &bytes(10, &bytes(1, &entry))));
        let projected = projection(&data, false);
        let structured: serde_json::Value = serde_json::from_str(projected["content"].as_str().unwrap()).unwrap();
        assert_eq!(structured, serde_json::json!({"values": ["hello\nworld", true]}));
        assert!(projected.get("native_missing_payloads").is_none());

        // genai.Value.content is a richer message, not JSON null.
        let data = bytes(140, &bytes(2, &bytes(11, &bytes(7, &[]))));
        let projected = projection(&data, false);
        assert!(projected.get("native_missing_payloads").is_none());
        assert_eq!(projected["native_unsupported"], true);
        assert!(projected.get("content").is_none());

        // When result.text(1) is also present, 10/11 project into native_structured_result
        // and still propagate unsupported genai.Value variants.
        let both = bytes(
            140,
            &bytes(2, &[bytes(1, b"text summary"), bytes(10, &bytes(1, &entry))].concat()),
        );
        let projected = projection(&both, false);
        assert_eq!(projected["content"], "text summary");
        assert_eq!(
            projected["native_structured_result"],
            serde_json::json!({"values": ["hello\nworld", true]})
        );
        assert!(projected.get("native_unsupported").is_none());

        let both_unsupported = bytes(
            140,
            &bytes(2, &[bytes(1, b"text summary"), bytes(11, &bytes(7, &[]))].concat()),
        );
        let projected = projection(&both_unsupported, false);
        assert_eq!(projected["content"], "text summary");
        assert!(projected.get("native_structured_result").is_none());
        assert_eq!(projected["native_unsupported"], true);
    }

    #[test]
    fn generic_native_error_details_are_not_lost() {
        let data = [uint(4, 7), bytes(140, &[]), bytes(31, &bytes(3, b"Command failed"))].concat();
        let projected = projection(&data, false);
        assert_eq!(projected["type"], "GENERIC");
        assert_eq!(projected["status"], "ERROR");
        assert_eq!(projected["error"], "Command failed");
    }

    #[test]
    fn invalid_native_arguments_remain_available() {
        let call = [bytes(1, b"call-id"), bytes(2, b"tool"), bytes(4, b"{broken")].concat();
        let data = bytes(20, &bytes(7, &call));
        let projected = projection(&data, false);
        assert_eq!(projected["tool_calls"][0]["arguments"], "{broken");
    }

    #[test]
    fn native_error_uses_nested_error_details() {
        let details = [bytes(1, b"User facing failure"), bytes(3, b"Internal details")].concat();
        let projected = projection(&bytes(24, &bytes(3, &details)), false);
        assert_eq!(projected["content"], "User facing failure");
        assert_eq!(projected["error"], "Internal details");
    }

    #[test]
    fn explicit_rowid_cell_cap_and_bad_timestamps_are_safe() {
        let world = crate::test_world("antigravity-native-bounds");
        let path = world.base.join("nonpk.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Non-INTEGER PRIMARY KEY `idx INT` assigns rowid=1 to idx=0.
        conn.execute_batch(
            "CREATE TABLE steps(idx INT, step_format INT, step_payload BLOB);
             CREATE TABLE gen_metadata(idx INT, data BLOB);",
        )
        .unwrap();
        let prompt = bytes(19, &bytes(1, b"rowid zero index"));
        conn.execute("INSERT INTO steps VALUES(0, 0, ?1)", [&prompt]).unwrap();
        let model = [bytes(1, &bytes(19, b"gemini-4-pro")), uint(2, 0)].concat();
        conn.execute("INSERT INTO gen_metadata VALUES(0, ?1)", [&model])
            .unwrap();
        drop(conn);

        let read = open(&path, false).unwrap();
        let line: serde_json::Value = serde_json::from_slice(&read.decoded_bytes().unwrap()).unwrap();
        assert_eq!(line["step_index"], 0);
        assert_eq!(line["content"], "rowid zero index");
        let meta = metadata(&path, true).unwrap();
        assert_eq!(meta.dominant_model.as_deref(), Some("gemini-4-pro"));

        // Oversized zeroblob(>256 MiB) is rejected before copying into scratch.
        let huge = world.base.join("huge.db");
        let conn = rusqlite::Connection::open(&huge).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_format INTEGER, step_payload BLOB);
             INSERT INTO steps VALUES(0, 0, zeroblob(270000000));",
        )
        .unwrap();
        drop(conn);
        let err = open(&huge, false).err().expect("oversized cell must fail");
        assert!(format!("{err:#}").contains("exceeds 256 MiB limit"), "{err:#}");

        // Overflowing timestamp nanos omit created_at instead of truncating mod 2^32.
        let bad_ts = bytes(5, &bytes(1, &[uint(1, 1_700_000_000), uint(2, 1 << 32)].concat()));
        let projected = projection(&[bad_ts, bytes(19, &bytes(1, b"ok"))].concat(), false);
        assert!(projected.get("created_at").is_none());
    }

    /// A history's temporary files do not grow with its steps, and each step's projection,
    /// its retained wire record included, is the one it has alone.
    #[test]
    fn a_native_history_is_staged_in_a_fixed_number_of_files() {
        let list = [bytes(1, &bytes(3, b"hello\nworld")), bytes(1, &uint(4, 1))].concat();
        let entry = [bytes(1, b"values"), bytes(2, &bytes(6, &list))].concat();
        let structured = bytes(140, &bytes(2, &bytes(10, &bytes(1, &entry))));
        let rendered = bytes(140, &bytes(2, &bytes(3, &bytes(2, b"rendered markdown"))));
        let prompt = bytes(19, &bytes(1, b"a prompt"));
        let steps = |n: usize| -> Vec<Vec<u8>> {
            (0..n)
                .map(|i| [uint(4, 3), [&structured, &rendered, &prompt][i % 3].clone()].concat())
                .collect()
        };
        let read = |tag: &str, steps: &[Vec<u8>]| {
            let world = crate::test_world(tag);
            let path = world.base.join("session.db");
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_format INTEGER, step_payload BLOB);")
                .unwrap();
            for (idx, payload) in steps.iter().enumerate() {
                conn.execute(
                    "INSERT INTO steps VALUES(?1, 0, ?2)",
                    rusqlite::params![idx as i64, payload],
                )
                .unwrap();
            }
            drop(conn);
            let before = super::super::super::store::bytes::spills_created();
            let read = open(&path, true).unwrap();
            let created = super::super::super::store::bytes::spills_created() - before;
            let lines: Vec<serde_json::Value> = read
                .decoded_bytes()
                .unwrap()
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            (created, lines)
        };
        let (few_files, few) = read("antigravity-native-few", &steps(3));
        let many_steps = steps(60);
        let (many_files, many) = read("antigravity-native-many", &many_steps);
        assert_eq!(few.len(), 3);
        assert_eq!(many.len(), 60);
        assert_eq!(many_files, few_files, "temporary files grew with the step count");
        for (index, (line, data)) in many.iter().zip(&many_steps).enumerate() {
            let mut alone = projection(data, true);
            alone["step_index"] = index.into();
            assert_eq!(line, &alone, "step {index}");
            // The retained wire record is the step's own range of the shared staging file.
            let expected: String = data.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(line["native_record"]["data"], expected, "step {index}");
        }
        assert_eq!(many[1]["content"], "rendered markdown");
        assert_eq!(many[2]["content"], "a prompt");
    }

    #[test]
    fn native_unknown_step_format_is_rejected() {
        let world = crate::test_world("antigravity-native-format");
        let path = world.base.join("session.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_format INTEGER, step_payload BLOB);
            INSERT INTO steps VALUES(0, 1, X'');",
        )
        .unwrap();
        drop(conn);
        let error = match open(&path, false) {
            Ok(_) => panic!("unsupported encoding accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("unsupported Antigravity native step format 1"));
    }

    #[test]
    fn direct_gemini_coder_steps_project_cleanly_without_unread_fields() {
        // RUN_COMMAND (28)
        let run = bytes(
            28,
            &[
                bytes(2, b"/work"),
                bytes(4, b"stdout fallback"),
                uint(6, 0),
                bytes(21, &bytes(1, b"combined output")),
                bytes(31, b"zsh"),
                uint(37, 1),
            ]
            .concat(),
        );
        let p = projection(&run, false);
        assert_eq!(p["type"], "RUN_COMMAND");
        assert_eq!(p["content"], "combined output");
        assert_eq!(
            p["native_payload"],
            serde_json::json!({
                "type": "CortexStepRunCommand",
                "exit_code": 0,
                "cwd": "/work",
                "shell_name": "zsh",
                "timed_out": true
            })
        );
        assert!(p.get("native_unsupported").is_none());

        // VIEW_FILE (14) with Media (15) + ImageData (14)
        let view = bytes(
            14,
            &[
                bytes(1, b"file:///work/a.rs"),
                uint(2, 10),
                uint(3, 25),
                bytes(4, b"fn main() {}"),
                uint(11, 100),
                uint(12, 2048),
                bytes(14, &[bytes(1, b"aGVsbG8="), bytes(2, b"image/png")].concat()),
                bytes(15, &[bytes(1, b"image/jpeg"), bytes(2, b"fo")].concat()),
            ]
            .concat(),
        );
        let p = projection(&view, false);
        assert_eq!(p["type"], "VIEW_FILE");
        assert_eq!(p["content"], "fn main() {}");
        assert_eq!(p["native_payload"]["path"], "file:///work/a.rs");
        assert_eq!(p["native_payload"]["start_line"], 10);
        assert_eq!(p["native_payload"]["end_line"], 25);
        assert_eq!(p["native_payload"]["file_num_lines"], 100);
        assert_eq!(p["native_payload"]["file_num_bytes"], 2048);
        assert_eq!(p["media"].as_array().unwrap().len(), 2);
        assert!(p.get("native_unsupported").is_none());

        // LIST_DIRECTORY (15): results (3) and children (2)
        let entry_a = [bytes(1, b"src"), uint(2, 1)].concat();
        let entry_b = [bytes(1, b"Cargo.toml"), uint(2, 0), uint(4, 512)].concat();
        let dir = bytes(
            15,
            &[bytes(1, b"/work"), bytes(3, &entry_a), bytes(3, &entry_b)].concat(),
        );
        let p = projection(&dir, false);
        assert_eq!(p["type"], "LIST_DIRECTORY");
        let expected_dir = serde_json::json!({
            "path": "/work",
            "entries": [
                {"name": "src", "is_dir": true},
                {"name": "Cargo.toml", "is_dir": false, "size_bytes": 512}
            ]
        });
        assert_eq!(p["native_structured_result"], expected_dir);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(p["content"].as_str().unwrap()).unwrap(),
            expected_dir
        );
        assert!(p.get("native_unsupported").is_none());

        let dir_children = bytes(15, &[bytes(1, b"/work"), bytes(2, b"a.rs"), bytes(2, b"b.rs")].concat());
        let p = projection(&dir_children, false);
        assert_eq!(
            p["native_structured_result"],
            serde_json::json!({"path": "/work", "entries": ["a.rs", "b.rs"]})
        );

        // CODE_ACTION (10): all 3 nested path locations
        for path_nested in [
            bytes(2, &bytes(1, &bytes(8, b"file:///work/p1.rs"))),
            bytes(1, &bytes(2, &bytes(2, &bytes(5, b"file:///work/p2.rs")))),
            bytes(1, &bytes(1, &bytes(4, &bytes(5, b"file:///work/p3.rs")))),
        ] {
            let ca = bytes(
                10,
                &[
                    path_nested,
                    uint(21, 1),
                    bytes(26, b"Refactor parser"),
                    bytes(28, &[uint(1, 5), uint(2, 2)].concat()),
                ]
                .concat(),
            );
            let p = projection(&ca, false);
            assert_eq!(p["type"], "CODE_ACTION");
            assert_eq!(p["content"], "Refactor parser");
            assert!(p["native_payload"]["path"]
                .as_str()
                .unwrap()
                .starts_with("file:///work/p"));
            assert_eq!(
                p["native_payload"]["diff_stats"],
                serde_json::json!({"additions": 5, "deletions": 2})
            );
            assert_eq!(p["native_payload"]["is_artifact_file"], true);
            assert!(p.get("native_unsupported").is_none());
        }

        // WRITE_TO_FILE (23), FILE_CHANGE (98), GREP_SEARCH (13), FIND (34), COMMAND_STATUS (37)
        let wtf = projection(
            &bytes(23, &[bytes(1, b"file:///work/new.rs"), uint(4, 1)].concat()),
            false,
        );
        assert_eq!(wtf["type"], "WRITE_TO_FILE");
        assert_eq!(wtf["native_payload"]["path"], "file:///work/new.rs");
        assert_eq!(wtf["native_payload"]["file_created"], true);
        assert!(wtf.get("native_unsupported").is_none());
        // Unspecified step_type 0 on an empty row must remain NATIVE_UNKNOWN, not WRITE_TO_FILE.
        assert_eq!(projection(&uint(1, 0), false)["type"], "NATIVE_UNKNOWN");

        let fc = projection(
            &bytes(
                98,
                &[bytes(1, b"file:///work/mod.rs"), bytes(5, b"Add imports")].concat(),
            ),
            false,
        );
        assert_eq!(fc["type"], "FILE_CHANGE");
        assert_eq!(fc["content"], "Add imports");
        assert_eq!(fc["native_payload"]["path"], "file:///work/mod.rs");
        assert!(fc.get("native_unsupported").is_none());

        let grep = projection(
            &bytes(
                13,
                &[
                    bytes(1, b"needle"),
                    bytes(3, b"a.rs:1:needle"),
                    bytes(5, b"warn"),
                    uint(7, 1),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(grep["type"], "GREP_SEARCH");
        assert_eq!(grep["content"], "a.rs:1:needle");
        assert_eq!(grep["error"], "warn");
        assert_eq!(grep["native_payload"]["query"], "needle");
        assert_eq!(grep["native_payload"]["total_results"], 1);
        assert!(grep.get("native_unsupported").is_none());

        let find = projection(
            &bytes(
                34,
                &[
                    bytes(1, b"*.rs"),
                    bytes(8, b"permission denied"),
                    bytes(14, b"src/lib.rs"),
                    uint(7, 1),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(find["type"], "FIND");
        assert_eq!(find["content"], "src/lib.rs");
        assert_eq!(find["error"], "permission denied");
        assert_eq!(find["native_payload"]["pattern"], "*.rs");
        assert!(find.get("native_unsupported").is_none());

        let cs = projection(
            &bytes(37, &[bytes(1, b"cmd-1"), uint(5, 0), bytes(12, b"delta line")].concat()),
            false,
        );
        assert_eq!(cs["type"], "COMMAND_STATUS");
        assert_eq!(cs["content"], "delta line");
        assert_eq!(cs["native_payload"]["command_id"], "cmd-1");
        assert_eq!(cs["native_payload"]["exit_code"], 0);
        assert!(cs.get("native_unsupported").is_none());

        // READ_URL_CONTENT (40), SEARCH_WEB (42), MCP_TOOL (47), LIST_RESOURCES (62), READ_RESOURCE (63)
        let url_p = projection(
            &bytes(
                40,
                &[
                    bytes(1, b"https://a.example"),
                    bytes(2, &bytes(2, b"page text")),
                    bytes(3, b"https://b.example"),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(url_p["type"], "READ_URL_CONTENT");
        assert_eq!(url_p["content"], "page text");
        assert_eq!(url_p["native_payload"]["url"], "https://a.example");
        assert_eq!(url_p["native_payload"]["resolved_url"], "https://b.example");
        assert!(url_p.get("native_unsupported").is_none());

        let sw = projection(
            &bytes(
                42,
                &[
                    bytes(1, b"rust"),
                    bytes(3, b"doc.rust-lang.org"),
                    bytes(5, b"Rust docs"),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(sw["type"], "SEARCH_WEB");
        assert_eq!(sw["content"], "Rust docs");
        assert_eq!(sw["native_payload"]["query"], "rust");
        assert_eq!(sw["native_payload"]["domain"], "doc.rust-lang.org");
        assert!(sw.get("native_unsupported").is_none());

        let mcp = projection(
            &bytes(
                47,
                &[
                    bytes(1, b"srv"),
                    bytes(5, &[bytes(1, b"aGVsbG8="), bytes(2, b"image/png")].concat()),
                    bytes(8, b"file:///tmp/mcp.txt"),
                    bytes(9, &bytes(2, b"rendered mcp")),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(mcp["type"], "MCP_TOOL");
        assert_eq!(mcp["content"], "rendered mcp");
        assert_eq!(mcp["native_output_uri"], "file:///tmp/mcp.txt");
        assert_eq!(mcp["native_payload"]["server_name"], "srv");
        assert_eq!(mcp["media"][0]["uri"], "data:image/png;base64,aGVsbG8=");
        assert!(mcp.get("native_unsupported").is_none());

        let lr = projection(
            &bytes(62, &[bytes(1, b"srv"), bytes(2, b"mcp://srv/list")].concat()),
            false,
        );
        assert_eq!(lr["type"], "LIST_RESOURCES");
        assert_eq!(lr["native_payload"]["server_name"], "srv");
        assert_eq!(lr["native_payload"]["uri"], "mcp://srv/list");
        assert!(lr.get("native_unsupported").is_none());

        let rr = projection(
            &bytes(63, &[bytes(1, b"srv"), bytes(2, b"mcp://srv/item")].concat()),
            false,
        );
        assert_eq!(rr["type"], "READ_RESOURCE");
        assert_eq!(rr["native_payload"]["uri"], "mcp://srv/item");
        assert!(rr.get("native_unsupported").is_none());

        // FINISH (12), INVOKE_SUBAGENT (143), ASK_QUESTION (154), EPHEMERAL_MESSAGE (103), CONVERSATION_HISTORY (111)
        let fin = projection(&bytes(12, &bytes(2, b"All tasks done")), false);
        assert_eq!(fin["type"], "FINISH");
        assert_eq!(fin["content"], "All tasks done");
        assert!(fin.get("native_unsupported").is_none());

        let inv = projection(
            &bytes(143, &[bytes(10, &bytes(1, b"child-1")), uint(11, 1)].concat()),
            false,
        );
        assert_eq!(inv["type"], "INVOKE_SUBAGENT");
        assert_eq!(
            inv["native_payload"]["conversation_ids"],
            serde_json::json!(["child-1"])
        );
        assert_eq!(inv["native_payload"]["task_mode"], true);
        assert!(inv.get("native_unsupported").is_none());

        let inv_single = projection(&bytes(143, &bytes(5, b"child-single")), false);
        assert_eq!(
            inv_single["native_payload"]["conversation_ids"],
            serde_json::json!(["child-single"])
        );

        let ask = projection(&bytes(154, &bytes(1, &bytes(1, b"Which branch?"))), false);
        assert_eq!(ask["type"], "ASK_QUESTION");
        assert_eq!(ask["native_payload"]["questions"][0]["question"], "Which branch?");
        assert!(ask.get("native_unsupported").is_none());

        let eph = projection(
            &bytes(
                103,
                &[
                    bytes(1, b"transient hint"),
                    bytes(2, &[bytes(1, b"image/png"), bytes(2, b"x")].concat()),
                    bytes(4, &[bytes(1, b"image/png"), bytes(2, b"y")].concat()),
                ]
                .concat(),
            ),
            false,
        );
        assert_eq!(eph["type"], "EPHEMERAL_MESSAGE");
        assert_eq!(eph["content"], "transient hint");
        assert_eq!(eph["media"].as_array().unwrap().len(), 2);
        assert!(eph.get("native_unsupported").is_none());

        let hist = projection(&bytes(111, &bytes(1, b"prior context")), false);
        assert_eq!(hist["type"], "CONVERSATION_HISTORY");
        assert_eq!(hist["content"], "prior context");
        assert!(hist.get("native_unsupported").is_none());
    }

    #[test]
    fn sqlite_side_columns_recover_cleared_rows_and_direct_spawn_calls_match() {
        let world = crate::test_world("antigravity-side-columns");
        let path = world.base.join("side.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER,
                status INTEGER,
                metadata BLOB,
                error_details BLOB,
                permissions BLOB,
                task_details BLOB,
                render_info BLOB,
                step_payload BLOB,
                step_format INTEGER DEFAULT 0
            );",
        )
        .unwrap();

        let call = [bytes(1, b"call-side"), bytes(2, b"run_command")].concat();
        let meta = [
            bytes(1, &uint(1, 1_700_000_100)),
            uint(3, 5),
            bytes(4, &call),
            bytes(9, &uint(2, 42)),
        ]
        .concat();
        let err = bytes(3, b"side error text");
        let perm = bytes(2, &bytes(1, &[bytes(1, b"command"), bytes(2, b"make")].concat()));
        let task = [bytes(1, b"sess/task-9"), uint(6, 1)].concat();
        let render = bytes(2, b"retained side render");

        // Row 0: stripped/cleared row with NULL step_payload; everything comes from side columns.
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, metadata, error_details, permissions, task_details, render_info, step_payload)
             VALUES (0, 21, 5, ?1, ?2, ?3, ?4, ?5, NULL)",
            rusqlite::params![meta, err, perm, task, render],
        )
        .unwrap();

        // Row 1: direct INVOKE_SUBAGENT (143) with metadata in side column only.
        let spawn_meta = bytes(
            4,
            &[bytes(1, b"call-direct-143"), bytes(2, b"invoke_subagent")].concat(),
        );
        let direct_inv = bytes(143, &bytes(10, &bytes(1, b"child-143-uuid")));
        conn.execute(
            "INSERT INTO steps(idx, step_type, status, metadata, step_payload)
             VALUES (1, 127, 3, ?1, ?2)",
            rusqlite::params![spawn_meta, direct_inv],
        )
        .unwrap();
        drop(conn);

        let read = open(&path, false).unwrap();
        let lines: Vec<serde_json::Value> = read
            .decoded_bytes()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "RUN_COMMAND");
        assert_eq!(lines[0]["status"], "CLEARED");
        assert_eq!(lines[0]["native_cleared"], true);
        assert_eq!(lines[0]["native_missing_payloads"], true);
        assert_eq!(lines[0]["native_tool_id"], "call-side");
        assert_eq!(lines[0]["native_tool_name"], "run_command");
        assert_eq!(lines[0]["input_tokens"], 42);
        assert_eq!(lines[0]["error"], "side error text");
        assert_eq!(lines[0]["native_task_details"]["id"], "sess/task-9");
        assert_eq!(lines[0]["native_interaction"]["action"], "command");
        assert_eq!(lines[0]["content"], "retained side render");
        assert!(lines[0].get("native_unsupported").is_none());

        let found = spawn_calls(&path, &["child-143-uuid", "other"]).unwrap();
        assert_eq!(found.get("child-143-uuid").map(String::as_str), Some("call-direct-143"));
    }
}
