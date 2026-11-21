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

fn read(path: &Path, complete: bool, limit: i64) -> Result<jsonl::Read> {
    let mut reads = ReadSet::default();
    record_sqlite_readset(path, &mut reads);
    let source = crate::json::JsonSource::capture_with_reads(path, &reads)?;
    let input = jsonl::Read::with(source)?;
    let output = Arc::new(ByteSource::arena()?);
    let mut stage = Stage::new()?;
    sqlite::open_ro(path)?.read_snapshot(|conn, _| {
        let columns = sqlite::Select::of(conn, "steps");
        anyhow::ensure!(
            columns.has("idx") && columns.has("step_payload") && columns.has("step_format"),
            "unsupported Antigravity native steps schema"
        );
        // A negative LIMIT is no limit.
        let mut query = conn.prepare("SELECT rowid, idx, step_format FROM steps ORDER BY idx LIMIT ?1")?;
        let mut rows = query.query([limit])?;
        output.append(|mut out| {
            while let Some(row) = rows.next()? {
                let rowid: i64 = row.get(0)?;
                let index: i64 = row.get(1)?;
                let format: i64 = row.get(2)?;
                anyhow::ensure!(format == 0, "unsupported Antigravity native step format {format}");
                anyhow::ensure!(index >= 0, "negative Antigravity step index");
                // Each cell is privately staged, then projected with positioned bounded
                // readers. No protobuf string or tool argument needs an owned full copy.
                if stage.used.get() >= Stage::RENEW {
                    stage = Stage::new()?;
                }
                let cell = stage.cell(conn, "steps", "step_payload", rowid)?;
                let step = Proto::read(&stage.file, cell.0, cell.1)?;
                project(&stage, &step, cell, index as u64, complete, &mut out)?;
            }
            Ok(())
        })?;
        Ok(())
    })?;
    input.check()?;
    let len = output.byte_len()?;
    Ok(input.decoded(output, len))
}

/// The `invoke_subagent` call that created each of `children`, from the typed result
/// that names the conversation (`InvokeSubagent.results[].conversation_id`). The registry's
/// `spawnStepIndex` is not that step: parallel spawns all record the last result.
pub(super) fn spawn_calls(path: &Path, children: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut calls = BTreeMap::new();
    if children.is_empty() {
        return Ok(calls);
    }
    sqlite::open_ro(path)?.read_snapshot(|conn, _| {
        if !sqlite::table_exists(conn, "steps") {
            return Ok(());
        }
        // SQLite finds the rows that mention a child; only those are staged and decoded.
        let mentions = vec!["instr(step_payload, CAST(? AS BLOB)) > 0"; children.len()].join(" OR ");
        let mut query = conn.prepare(&format!(
            "SELECT rowid FROM steps WHERE step_payload IS NOT NULL AND ({mentions}) ORDER BY idx"
        ))?;
        let rows: Vec<i64> = query
            .query_map(rusqlite::params_from_iter(children.iter()), |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let stage = Stage::new()?;
        let file = &stage.file;
        for row in rows {
            let (at, len) = stage.cell(conn, "steps", "step_payload", row)?;
            let step = Proto::read(file, at, len)?;
            let inner = |message: &Proto, number: u32| message.last(number).map(|f| f.message(file)).transpose();
            let text = |message: &Proto, number: u32| message.last(number).and_then(|f| f.string(file, 256).ok());
            let Some(call) = inner(&step, 5)?
                .map(|metadata| inner(&metadata, 4))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            let (Some(id), Some("invoke_subagent")) = (text(&call, 1), text(&call, 2).as_deref()) else {
                continue;
            };
            let Some(result) = inner(&step, 140)?
                .map(|generic| inner(&generic, 2))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            let Some(any) = inner(&result, 6)? else {
                continue;
            };
            if !text(&any, 1).is_some_and(|url| url.ends_with("/exa.cortex.core.tools.InvokeSubagent")) {
                continue;
            }
            let Some(spawned) = inner(&any, 2)? else {
                continue;
            };
            for created in spawned.repeated(2) {
                if let Some(child) = text(&created.message(file)?, 1) {
                    if children.contains(&child.as_str()) {
                        calls.entry(child).or_insert_with(|| id.clone());
                    }
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
fn project(
    stage: &Stage,
    step: &Proto,
    cell: (u64, u64),
    index: u64,
    complete: bool,
    out: &mut impl Write,
) -> Result<()> {
    let file = &stage.file;
    // The `step` member set names the type. A cleared row can lose its member and keep
    // only `Step.type` (CortexStepType): its kind, order and tool identity still stand.
    const KINDS: [(u32, u64, &str); 6] = [
        (19, 14, "USER_INPUT"),
        (20, 15, "PLANNER_RESPONSE"),
        (140, 132, "GENERIC"),
        (114, 101, "SYSTEM_MESSAGE"),
        (30, 23, "CHECKPOINT"),
        (24, 17, "ERROR_MESSAGE"),
    ];
    let declared = scalar(step, 1);
    let (kind, payload) = KINDS
        .iter()
        .find_map(|(member, _, kind)| step.last(*member).map(|payload| (*kind, Some(payload))))
        .or_else(|| {
            let (_, _, kind) = KINDS.iter().find(|(_, ty, _)| declared == Some(*ty))?;
            Some((*kind, None))
        })
        .unwrap_or(("NATIVE_UNKNOWN", None));
    let payload = payload.map(|payload| payload.message(file)).transpose()?;
    let status = match scalar(step, 4).unwrap_or(0) {
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
    let mut unsupported = account::unread_fields(file, step, kind, payload.as_ref())?;
    if status == "CLEARED" {
        out.write_all(b",\"native_cleared\":true")?;
    }
    if let Some(field) = step.last(5) {
        let meta = field.message(file)?;
        let source = match scalar(&meta, 3).unwrap_or(0) {
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
    if let Some(error) = step.last(31) {
        let error = error.message(file)?;
        if let Some(number) = [1, 9, 3, 2, 5].into_iter().find(|number| error.last(*number).is_some()) {
            text(file, &error, number, "error", out)?;
        }
    }
    if let Some(details) = step.last(148) {
        context::task(file, &details.message(file)?, out)?;
    }
    let mut missing_attachments = false;
    // An interaction of a kind other than a permission request stays uninterpreted.
    let interpreted = interaction::project(file, step, out)?;
    unsupported |= !interpreted && (step.last(133).is_some() || step.last(147).is_some());
    match (kind, &payload) {
        ("USER_INPUT", Some(input)) => {
            context::user(file, input, out)?;
            text(file, input, 1, "content", out)?;
            missing_attachments |= media::project(file, input, 9, out)?;
        }
        ("PLANNER_RESPONSE", Some(response)) => {
            text(file, response, 1, "content", out)?;
            text(file, response, 3, "thinking", out)?;
            text(file, response, 4, "thinking_signature", out)?;
            if scalar(response, 5) == Some(1) {
                out.write_all(b",\"thinking_redacted\":true")?;
            }
            calls(file, response, out)?;
            identity::planner(file, response, out)?;
            missing_attachments |= media::project(file, response, 20, out)?;
        }
        ("GENERIC", Some(generic)) => {
            if let Some(field) = generic.last(2) {
                let result = field.message(file)?;
                let has_content = if result.last(1).is_some() {
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
                    let info = info.message(file)?;
                    render::agents(file, &info, out)?;
                    if !has_content {
                        render::content(stage, &info, out)?;
                    }
                }
                text(file, &result, 7, "native_output_uri", out)?;
                if let Some(any) = result.last(6) {
                    unsupported |= !payload::project(file, &any.message(file)?, out)?;
                }
                missing_attachments |= media::project(file, &result, 4, out)?;
            }
        }
        ("SYSTEM_MESSAGE", Some(message)) => {
            text(file, message, 1, "content", out)?;
            missing_attachments |= media::project(file, message, 6, out)?;
        }
        ("CHECKPOINT", Some(checkpoint)) => {
            text(file, checkpoint, 5, "content", out)?;
            context::checkpoint(file, checkpoint, out)?;
        }
        ("ERROR_MESSAGE", Some(message)) => {
            if let Some(error) = message.last(3) {
                let error = error.message(file)?;
                let preferred = [1, 9, 3, 2, 5].into_iter().find(|number| error.last(*number).is_some());
                if let Some(number) = preferred {
                    text(file, &error, number, "content", out)?;
                }
                // The export's `error` is the short or full error, whichever text is preferred.
                if step.last(31).is_none() {
                    if let Some(err_num) = [2, 3].into_iter().find(|number| error.last(*number).is_some()) {
                        text(file, &error, err_num, "error", out)?;
                    }
                }
            }
        }
        _ => {}
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
        project(&stage, &message, cell, 17, complete, &mut output).unwrap();
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
}
