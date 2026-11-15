//! User-supplied context and background-task metadata. Display labels remain
//! metadata; only actual text/content/model-facing fields enter the user body.
use super::{wire, File, Proto, Result, Write};

pub(super) fn user(file: &File, input: &Proto, out: &mut impl Write) -> Result<()> {
    out.write_all(b",\"native_content\":[")?;
    let mut first = true;
    for item in input.repeated(3) {
        let item = item.message(file)?;
        let Some(field) = item.fields().iter().rev().find(|f| matches!(f.number, 1 | 2)) else {
            continue;
        };
        if field.number == 1 {
            component(file, *field, out, &mut first)?;
        } else if let Some(scope) = scope(&field.message(file)?) {
            let value = scope.message(file)?;
            match scope.number {
                11 => {
                    if let Some(text) = value.last(1) {
                        component(file, text, out, &mut first)?;
                    }
                }
                21 => {
                    for number in [3, 5] {
                        if let Some(text) = value.last(number) {
                            component(file, text, out, &mut first)?;
                        }
                    }
                }
                22 => {
                    if let Some(text) = value.last(2) {
                        component(file, text, out, &mut first)?;
                    }
                }
                27 => {
                    if let Some(info) = value.last(1) {
                        if let Some(text) = info.message(file)?.last(2) {
                            component(file, text, out, &mut first)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // A command with no expansion (`/compact`) leaves every item empty. What the
    // person typed is then the whole prompt; with item text it would repeat mentions.
    if first {
        if let Some(typed) = input.last(2).filter(|field| nonempty(*field)) {
            component(file, typed, out, &mut first)?;
        }
    }
    out.write_all(b"],\"native_scope_items\":[")?;
    let mut first = true;
    for item in input.repeated(3) {
        let item = item.message(file)?;
        let Some(field) = item
            .fields()
            .iter()
            .rev()
            .find(|f| matches!(f.number, 1 | 2))
            .filter(|f| f.number == 2)
        else {
            continue;
        };
        if let Some(scope) = scope(&field.message(file)?) {
            if !first {
                out.write_all(b",")?;
            }
            first = false;
            scope_json(file, scope, out)?;
        }
    }
    out.write_all(b"]")?;
    Ok(())
}

fn scope(message: &Proto) -> Option<wire::Field> {
    message
        .fields()
        .iter()
        .rev()
        .find(|f| (1..=30).contains(&f.number) && f.number != 5)
        .copied()
}

fn nonempty(field: wire::Field) -> bool {
    matches!(field.value, wire::Value::Bytes { len, .. } if len > 0)
}

fn component(file: &File, field: wire::Field, out: &mut impl Write, first: &mut bool) -> Result<()> {
    if !nonempty(field) {
        return Ok(());
    }
    if !*first {
        out.write_all(b",")?;
    }
    *first = false;
    field.write_json_string(file, out)
}

fn strings(file: &File, message: &Proto, fields: &[(u32, &str)], out: &mut impl Write) -> Result<()> {
    for (number, name) in fields {
        super::text(file, message, *number, name, out)?;
    }
    Ok(())
}

fn numbers(message: &Proto, fields: &[(u32, &str)], out: &mut impl Write) -> Result<()> {
    for (number, name) in fields {
        if let Some(value) = message.last(*number).and_then(|f| f.u64()) {
            write!(out, ",\"{name}\":{value}")?;
        }
    }
    Ok(())
}

fn range(file: &File, message: &Proto, out: &mut impl Write) -> Result<()> {
    strings(file, message, &[(1, "uri")], out)?;
    numbers(message, &[(2, "start_line"), (3, "end_line")], out)
}

fn scope_json(file: &File, field: wire::Field, out: &mut impl Write) -> Result<()> {
    let name = match field.number {
        1 => "file",
        2 => "directory",
        10 => "file_line_range",
        11 => "text_block",
        18 => "rule",
        19 => "mcp_resource",
        21 => "browser_code_block",
        22 => "browser_text",
        27 => "slash_command",
        28 => "dynamic_context",
        29 => "link",
        _ => "unsupported",
    };
    write!(out, "{{\"kind\":\"{name}\",\"native_field\":{}", field.number)?;
    let value = field.message(file)?;
    match field.number {
        1 | 2 => {
            strings(file, &value, &[(1, "path"), (5, "uri")], out)?;
            numbers(&value, &[(3, "num_files"), (4, "num_bytes")], out)?;
        }
        10 => range(file, &value, out)?,
        11 => {
            strings(file, &value, &[(3, "label")], out)?;
            if let Some(location) = value.last(2) {
                out.write_all(b",\"file_line_range\":{\"kind\":\"file_line_range\"")?;
                range(file, &location.message(file)?, out)?;
                out.write_all(b"}")?;
            }
        }
        18 => strings(file, &value, &[(1, "path"), (2, "name"), (3, "description")], out)?,
        19 => strings(
            file,
            &value,
            &[
                (1, "uri"),
                (2, "name"),
                (3, "description"),
                (4, "mime_type"),
                (5, "server_name"),
            ],
            out,
        )?,
        21 => {
            strings(file, &value, &[(1, "url"), (2, "title"), (6, "page_id")], out)?;
            numbers(&value, &[(4, "language")], out)?;
        }
        22 => strings(file, &value, &[(1, "url"), (3, "page_id")], out)?,
        27 => {
            if let Some(info) = value.last(1) {
                let info = info.message(file)?;
                strings(
                    file,
                    &info,
                    &[
                        (1, "name"),
                        (4, "icon"),
                        (5, "definition_path"),
                        (6, "display_name"),
                        (7, "logo"),
                    ],
                    out,
                )?;
                numbers(&info, &[(3, "command_type")], out)?;
            }
            out.write_all(b",\"argument_values\":{")?;
            for (index, entry) in value.repeated(2).enumerate() {
                if index != 0 {
                    out.write_all(b",")?;
                }
                let entry = entry.message(file)?;
                if let Some(key) = entry.last(1) {
                    key.write_json_string(file, out)?;
                } else {
                    out.write_all(b"\"\"")?;
                }
                out.write_all(b":")?;
                if let Some(value) = entry.last(2) {
                    value.write_json_string(file, out)?;
                } else {
                    out.write_all(b"\"\"")?;
                }
            }
            out.write_all(b"}")?;
        }
        28 => strings(
            file,
            &value,
            &[
                (1, "provider_label"),
                (2, "item_value"),
                (3, "item_label"),
                (4, "uri"),
                (5, "icon_uri"),
                (6, "description"),
            ],
            out,
        )?,
        29 => strings(
            file,
            &value,
            &[
                (1, "url"),
                (2, "title"),
                (3, "description"),
                (4, "display_label"),
                (5, "provider"),
            ],
            out,
        )?,
        _ => {}
    }
    out.write_all(b"}")?;
    Ok(())
}

pub(super) fn task(file: &File, details: &Proto, out: &mut impl Write) -> Result<()> {
    out.write_all(b",\"native_task_details\":{\"kind\":\"background_task\"")?;
    strings(
        file,
        details,
        &[
            (1, "id"),
            (2, "log_uri"),
            (3, "progress"),
            (4, "description"),
            (7, "title"),
        ],
        out,
    )?;
    for (number, name) in [(5, "requires_input_approval"), (6, "is_daemon")] {
        if let Some(value) = details.last(number).and_then(|f| f.u64()) {
            write!(out, ",\"{name}\":{}", value != 0)?;
        }
    }
    if let Some(timestamp) = details.last(8) {
        let timestamp = timestamp.message(file)?;
        let seconds = timestamp.last(1).and_then(|f| f.u64()).unwrap_or(0) as i64;
        let nanos = timestamp.last(2).and_then(|f| f.u64()).unwrap_or(0);
        let timestamp = chrono::DateTime::from_timestamp(seconds, u32::try_from(nanos)?)
            .ok_or_else(|| anyhow::anyhow!("invalid native background task timestamp"))?;
        write!(
            out,
            ",\"moved_to_background_at\":{}",
            serde_json::to_string(&timestamp.to_rfc3339())?
        )?;
    }
    out.write_all(b"}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ByteSource;
    use super::*;

    fn bytes(number: u32, data: &[u8]) -> Vec<u8> {
        fn varint(mut value: u64, out: &mut Vec<u8>) {
            while value >= 128 {
                out.push(value as u8 | 128);
                value >>= 7;
            }
            out.push(value as u8);
        }
        let mut out = Vec::new();
        varint((number << 3 | 2) as u64, &mut out);
        varint(data.len() as u64, &mut out);
        out.extend_from_slice(data);
        out
    }

    fn projection(data: &[u8], is_task: bool) -> serde_json::Value {
        let staged = ByteSource::arena().unwrap();
        staged.append(|out| Ok(out.write_all(data)?)).unwrap();
        let file = staged.file().unwrap();
        let message = Proto::read(&file, 0, data.len() as u64).unwrap();
        let mut out = b"{\"sentinel\":true".to_vec();
        if is_task {
            task(&file, &message, &mut out).unwrap();
        } else {
            user(&file, &message, &mut out).unwrap();
        }
        out.push(b'}');
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn user_scope_keeps_model_text_separate_from_display_and_arguments() {
        let info = [
            bytes(1, b"research"),
            bytes(2, b"Investigate carefully"),
            bytes(6, b"Display only"),
        ]
        .concat();
        let args = [bytes(1, b"topic"), bytes(2, b"bounded IO")].concat();
        let slash = [bytes(1, &info), bytes(2, &args)].concat();
        let input = [
            bytes(3, &bytes(1, b"Please help")),
            bytes(3, &bytes(2, &bytes(27, &slash))),
        ]
        .concat();
        let output = projection(&input, false);
        assert_eq!(
            output["native_content"],
            serde_json::json!(["Please help", "Investigate carefully"])
        );
        assert_eq!(
            output["native_scope_items"][0]["argument_values"]["topic"],
            "bounded IO"
        );
        assert_eq!(output["native_scope_items"][0]["display_name"], "Display only");
        assert!(output["native_scope_items"][0].get("model_facing_text").is_none());
    }

    #[test]
    fn a_command_without_expansion_keeps_the_typed_prompt() {
        // Written by CLI 1.5.9 for `/compact`: the typed text, an empty text item and a
        // slash-command scope with a name and no model-facing body.
        let slash = bytes(1, &[bytes(1, b"compact"), vec![24, 1]].concat());
        let input = [
            bytes(2, b"/compact"),
            bytes(3, &bytes(2, &bytes(27, &slash))),
            bytes(3, &bytes(1, b"")),
        ]
        .concat();
        let output = projection(&input, false);
        assert_eq!(output["native_content"], serde_json::json!(["/compact"]));
        assert_eq!(output["native_scope_items"][0]["name"], "compact");

        // Item text wins: the typed form of a mention would repeat its path.
        let input = [bytes(2, b"read this@[/work/a.rs]"), bytes(3, &bytes(1, b"read this"))].concat();
        assert_eq!(
            projection(&input, false)["native_content"],
            serde_json::json!(["read this"])
        );
    }

    #[test]
    fn file_scope_is_metadata_and_text_scope_is_content() {
        let file = bytes(5, b"file:///work/source.rs");
        let text = [bytes(1, b"selected code"), bytes(3, b"display label")].concat();
        let input = [
            bytes(3, &bytes(2, &bytes(1, &file))),
            bytes(3, &bytes(2, &bytes(11, &text))),
        ]
        .concat();
        let output = projection(&input, false);
        assert_eq!(output["native_content"], serde_json::json!(["selected code"]));
        assert_eq!(output["native_scope_items"][0]["uri"], "file:///work/source.rs");
        assert!(output["native_scope_items"][1].get("content").is_none());
    }

    #[test]
    fn task_details_keep_explicit_metadata_without_inventing_completion() {
        let input = [
            bytes(1, b"task-id"),
            bytes(2, b"file:///tmp/task.log"),
            bytes(3, b"Working"),
            bytes(4, b"Check build"),
            bytes(7, b"Build"),
            vec![40, 1, 48, 1],
            bytes(8, &[8, 1]),
        ]
        .concat();
        let output = projection(&input, true);
        let task = &output["native_task_details"];
        assert_eq!(task["id"], "task-id");
        assert_eq!(task["progress"], "Working");
        assert_eq!(task["requires_input_approval"], true);
        assert_eq!(task["is_daemon"], true);
        assert_eq!(task["moved_to_background_at"], "1970-01-01T00:00:01+00:00");
        assert!(task.get("status").is_none());
    }
}
