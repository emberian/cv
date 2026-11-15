//! Bounded native protobuf projection. JSONL remains the historical authority when
//! present: native CLEARED steps are not replacements for retained exported text.
mod context;
mod identity;
mod interaction;
mod media;
mod render;
mod wire;
use super::super::store::{bytes::ByteSource, sqlite};
use super::*;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use wire::Message as Proto;

pub(super) fn open(path: &Path, complete: bool) -> Result<jsonl::Read> {
    let mut reads = ReadSet::default();
    record_sqlite_readset(path, &mut reads);
    let source = crate::json::JsonSource::capture_with_reads(path, &reads)?;
    let input = jsonl::Read::with(source)?;
    let output = Arc::new(ByteSource::arena()?);
    sqlite::open_ro(path)?.read_snapshot(|conn, _| {
        let columns = sqlite::Select::of(conn, "steps");
        anyhow::ensure!(
            columns.has("idx") && columns.has("step_payload") && columns.has("step_format"),
            "unsupported Antigravity native steps schema"
        );
        let mut query = conn.prepare("SELECT idx, step_format FROM steps ORDER BY idx")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let index: i64 = row.get(0)?;
            let format: i64 = row.get(1)?;
            anyhow::ensure!(format == 0, "unsupported Antigravity native step format {format}");
            anyhow::ensure!(index >= 0, "negative Antigravity step index");
            // Each cell is privately staged, then projected with positioned bounded
            // readers. No protobuf string or tool argument needs an owned full copy.
            let cell = ByteSource::arena()?;
            let (_, len) = cell.append(|out| {
                sqlite::with_cell_reader(conn, "steps", "step_payload", index, |reader, _| {
                    std::io::copy(reader, out)?;
                    Ok(())
                })
            })?;
            let file = cell.file()?;
            let step = Proto::read(&file, 0, len)?;
            output.append(|mut out| project(&file, &step, index as u64, complete, &mut out))?;
        }
        Ok(())
    })?;
    input.check()?;
    let len = output.byte_len()?;
    Ok(input.decoded(output, len))
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
fn project(file: &File, step: &Proto, index: u64, complete: bool, out: &mut impl Write) -> Result<()> {
    let kind = if step.last(19).is_some() {
        "USER_INPUT"
    } else if step.last(20).is_some() {
        "PLANNER_RESPONSE"
    } else if step.last(140).is_some() {
        "GENERIC"
    } else if step.last(114).is_some() {
        "SYSTEM_MESSAGE"
    } else if step.last(30).is_some() {
        "CHECKPOINT"
    } else if step.last(24).is_some() {
        "ERROR_MESSAGE"
    } else {
        "NATIVE_UNKNOWN"
    };
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
    // The selected projection does not yet interpret every native field. COMPLETE
    // retains the original wire record, but the normalized view must disclose this.
    out.write_all(b",\"native_unsupported\":true")?;
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
        if let Some(timestamp) = meta.last(1) {
            let ts = timestamp.message(file)?;
            if let Some(ts) = scalar(&ts, 1)
                .and_then(|seconds| DateTime::from_timestamp(seconds as i64, scalar(&ts, 2).unwrap_or(0) as u32))
            {
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
    interaction::project(file, step, out)?;
    match kind {
        "USER_INPUT" => {
            let input = step.last(19).unwrap().message(file)?;
            context::user(file, &input, out)?;
            text(file, &input, 1, "content", out)?;
            missing_attachments |= media::project(file, &input, 9, out)?;
        }
        "PLANNER_RESPONSE" => {
            let response = step.last(20).unwrap().message(file)?;
            text(file, &response, 1, "content", out)?;
            text(file, &response, 3, "thinking", out)?;
            text(file, &response, 4, "thinking_signature", out)?;
            if scalar(&response, 5) == Some(1) {
                out.write_all(b",\"thinking_redacted\":true")?;
            }
            calls(file, &response, out)?;
            identity::planner(file, &response, out)?;
            missing_attachments |= media::project(file, &response, 20, out)?;
        }
        "GENERIC" => {
            let generic = step.last(140).unwrap().message(file)?;
            if let Some(field) = generic.last(2) {
                let result = field.message(file)?;
                let has_content = if result.last(1).is_some() {
                    text(file, &result, 1, "content", out)?;
                    true
                } else if let Some(structure) = result.last(10) {
                    structured_content(file, structure.message(file)?, true, out)?
                } else if let Some(value) = result.last(11) {
                    structured_content(file, value.message(file)?, false, out)?
                } else {
                    false
                };
                if let Some(info) = result.last(3) {
                    let info = info.message(file)?;
                    render::agents(file, &info, out)?;
                    if !has_content {
                        render::content(file, &info, out)?;
                    }
                }
                text(file, &result, 7, "native_output_uri", out)?;
                missing_attachments |= media::project(file, &result, 4, out)?;
            }
        }
        "SYSTEM_MESSAGE" => {
            let message = step.last(114).unwrap().message(file)?;
            text(file, &message, 1, "content", out)?;
            missing_attachments |= media::project(file, &message, 6, out)?;
        }
        "CHECKPOINT" => text(file, &step.last(30).unwrap().message(file)?, 5, "content", out)?,
        "ERROR_MESSAGE" => {
            let message = step.last(24).unwrap().message(file)?;
            if let Some(error) = message.last(3) {
                let error = error.message(file)?;
                let preferred = [1, 9, 3, 2, 5].into_iter().find(|number| error.last(*number).is_some());
                if let Some(number) = preferred {
                    text(file, &error, number, "content", out)?;
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
    if complete {
        out.write_all(b",\"native_record\":{\"encoding\":\"protobuf-hex\",\"data\":\"")?;
        let mut reader = file.try_clone()?;
        use std::io::Seek;
        reader.rewind()?;
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

fn structured_content(file: &File, message: Proto, structure: bool, out: &mut impl Write) -> Result<bool> {
    let staged = ByteSource::arena()?;
    let mut supported = false;
    staged.append(|mut output| {
        supported = if structure {
            struct_json(file, &message, &mut output, 0)?
        } else {
            value_json(file, &message, &mut output, 0)?
        };
        Ok(())
    })?;
    if supported {
        out.write_all(b",\"content\":")?;
        crate::projection::write_json_string(out, staged.reader()?)?;
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
        let cell = ByteSource::arena().unwrap();
        cell.append(|out| Ok(out.write_all(data)?)).unwrap();
        let file = cell.file().unwrap();
        let message = Proto::read(&file, 0, data.len() as u64).unwrap();
        let mut output = Vec::new();
        project(&file, &message, 17, complete, &mut output).unwrap();
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
            assert_eq!(projected["native_unsupported"], true);
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
        assert_eq!(projected["media"][0]["uri"], "file:///nonexistent/cv-fixture/upload.png");
        assert_eq!(projected["native_missing_attachments"], true);
    }

    #[test]
    fn native_terminal_missing_results_and_complete_wire_are_explicit() {
        for status in [2, 3, 5, 6] {
            let data = [uint(4, status), bytes(140, &[])].concat();
            let projected = projection(&data, true);
            assert_eq!(projected["native_unsupported"], true);
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
