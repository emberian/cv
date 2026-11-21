//! Typed tool results: `GenericStepResult.payload`, a `google.protobuf.Any` whose
//! schema the type URL names. Only present scalar facts are projected. Text the call
//! arguments or the result already carry (a command line, a message, a summary) and
//! unbounded diffs stay in the COMPLETE wire record.
use super::{wire, File, Proto, Result, Write};

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Texts,
    /// int32/int64: a negative value is a sign-extended varint.
    Int,
    Uint,
    Flag,
    Message(&'static [Field]),
    Messages(&'static [Field]),
    /// A string field of each repeated message.
    TextOfEach(u32),
}
type Field = (u32, &'static str, Kind);

const PREFIX: &str = "type.googleapis.com/";
const TOOLS: &[(&str, &[Field])] = &[
    (
        "exa.cortex.core.tools.ReadFile",
        &[
            (1, "file_num_lines", Kind::Uint),
            (2, "file_num_bytes", Kind::Uint),
            (3, "path", Kind::Text),
            (4, "start_line", Kind::Int),
            (5, "end_line", Kind::Int),
            (6, "line_range_bytes", Kind::Uint),
        ],
    ),
    (
        "exa.cortex.core.tools.RunCommand",
        &[
            (1, "exit_code", Kind::Int),
            (2, "timed_out", Kind::Flag),
            (
                3,
                "sandbox_details",
                Kind::Message(&[(1, "ran_in_sandbox", Kind::Flag), (2, "sandbox_errors", Kind::Text)]),
            ),
            (4, "is_trusted_cli", Kind::Flag),
            (5, "shell_name", Kind::Text),
            (7, "cwd", Kind::Text),
            (8, "exit_code_unknown", Kind::Flag),
        ],
    ),
    (
        "exa.cortex.core.tools.FileEdit",
        &[
            (1, "kind", Kind::Uint),
            (2, "path", Kind::Text),
            (
                4,
                "diff_stats",
                Kind::Message(&[(1, "additions", Kind::Int), (2, "deletions", Kind::Int)]),
            ),
            (6, "is_artifact_file", Kind::Flag),
        ],
    ),
    (
        "exa.cortex.core.tools.ManageTask",
        &[(1, "action", Kind::Text), (2, "task_ids", Kind::Texts)],
    ),
    (
        "exa.cortex.core.tools.SendMessage",
        &[(1, "recipient", Kind::Text), (2, "recipient_name", Kind::Text)],
    ),
    (
        "exa.cortex.core.tools.Schedule",
        &[
            (1, "mode", Kind::Text),
            (2, "outcome", Kind::Text),
            (3, "iterations", Kind::Int),
            (4, "cancelled_by", Kind::Text),
        ],
    ),
    (
        "exa.cortex.core.tools.ManageSubagents",
        &[(1, "action", Kind::Text), (2, "conversation_ids", Kind::Texts)],
    ),
    (
        "exa.cortex.core.tools.InvokeSubagent",
        &[
            (
                1,
                "subagents",
                Kind::Messages(&[
                    (1, "type_name", Kind::Text),
                    (2, "role", Kind::Text),
                    (4, "inherit", Kind::Flag),
                    (5, "branch", Kind::Text),
                    (6, "workspace_uri", Kind::Text),
                    (9, "run_as_task", Kind::Flag),
                    (10, "share", Kind::Text),
                    (11, "fork", Kind::Flag),
                    (12, "model_option", Kind::Text),
                ]),
            ),
            (2, "conversation_ids", Kind::TextOfEach(1)),
            (3, "task_mode", Kind::Flag),
        ],
    ),
    (
        "exa.cortex.core.tools.DefineSubagent",
        &[(1, "name", Kind::Text), (4, "description", Kind::Text)],
    ),
    (
        "exa.cortex.core.tools.GenerateImage",
        &[
            (2, "image_paths", Kind::Texts),
            (3, "aspect_ratio", Kind::Text),
            (4, "image_uri", Kind::Text),
            (5, "image_num_bytes", Kind::Uint),
            (6, "model_name", Kind::Text),
            (8, "image_name", Kind::Text),
        ],
    ),
    (
        "exa.cortex.core.tools.AskQuestion",
        &[(
            1,
            "questions",
            Kind::Messages(&[
                (1, "question", Kind::Text),
                (3, "is_multi_select", Kind::Flag),
                (4, "selected_option_ids", Kind::Texts),
                (5, "write_in_response", Kind::Text),
                (6, "skipped", Kind::Flag),
            ]),
        )],
    ),
    (
        "exa.cortex.core.tools.SearchWeb",
        &[(1, "query", Kind::Text), (2, "domain", Kind::Text)],
    ),
];
/// A result that wraps an earlier typed step: the member of `gemini_coder.Step.step` set.
const STEPS: &[(u32, &str)] = &[
    (10, "code_action"),
    (13, "grep_search"),
    (14, "view_file"),
    (15, "list_directory"),
    (34, "find"),
];

/// Emits `native_payload` with the type name. Returns whether the schema is known; an
/// unknown one keeps only its name here and its bytes in the COMPLETE record.
pub(super) fn project(file: &File, any: &Proto, out: &mut impl Write) -> Result<bool> {
    let Some(url) = any.last(1) else {
        return Ok(false);
    };
    let mut name = String::new();
    let named = matches!(url.value, wire::Value::Bytes { len, .. } if len <= 256)
        && std::io::Read::read_to_string(&mut url.reader(file)?, &mut name).is_ok();
    let name = name.strip_prefix(PREFIX).unwrap_or(&name);
    out.write_all(b",\"native_payload\":{\"type\":")?;
    if named {
        serde_json::to_writer(&mut *out, name)?;
    } else {
        out.write_all(b"null")?;
    }
    let value = any.last(2).map(|value| value.message(file)).transpose()?;
    let mut known = false;
    if let Some((_, fields)) = TOOLS.iter().find(|(tool, _)| named && *tool == name) {
        known = true;
        if let Some(value) = &value {
            members(file, value, fields, out, &mut false)?;
        }
    } else if named && name == "gemini_coder.Step" {
        if let Some((_, step)) = value
            .as_ref()
            .and_then(|value| STEPS.iter().find(|(number, _)| value.last(*number).is_some()))
        {
            known = true;
            write!(out, ",\"step\":\"{step}\"")?;
        }
    }
    out.write_all(b"}")?;
    Ok(known)
}

fn members(file: &File, message: &Proto, fields: &[Field], out: &mut impl Write, first: &mut bool) -> Result<()> {
    for (number, name, kind) in fields {
        let Some(field) = message.last(*number) else {
            continue;
        };
        if !std::mem::take(first) {
            out.write_all(b",")?;
        }
        write!(out, "\"{name}\":")?;
        match kind {
            Kind::Text => field.write_json_string(file, out)?,
            Kind::Texts | Kind::TextOfEach(_) => {
                out.write_all(b"[")?;
                let mut first = true;
                for item in message.repeated(*number) {
                    let item = match kind {
                        Kind::TextOfEach(inner) => match item.message(file)?.last(*inner) {
                            Some(inner) => inner,
                            None => continue,
                        },
                        _ => item,
                    };
                    if !std::mem::take(&mut first) {
                        out.write_all(b",")?;
                    }
                    item.write_json_string(file, out)?;
                }
                out.write_all(b"]")?;
            }
            Kind::Int => write!(out, "{}", field.u64().unwrap_or(0) as i64)?,
            Kind::Uint => write!(out, "{}", field.u64().unwrap_or(0))?,
            Kind::Flag => write!(out, "{}", field.u64().unwrap_or(0) != 0)?,
            Kind::Message(inner) => {
                out.write_all(b"{")?;
                members(file, &field.message(file)?, inner, out, &mut true)?;
                out.write_all(b"}")?;
            }
            Kind::Messages(inner) => {
                out.write_all(b"[")?;
                let mut first_item = true;
                for item in message.repeated(*number) {
                    if !std::mem::take(&mut first_item) {
                        out.write_all(b",")?;
                    }
                    out.write_all(b"{")?;
                    members(file, &item.message(file)?, inner, out, &mut true)?;
                    out.write_all(b"}")?;
                }
                out.write_all(b"]")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ByteSource;
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 128 {
            out.push(value as u8 | 128);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn bytes(number: u64, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3 | 2, &mut out);
        varint(data.len() as u64, &mut out);
        out.extend_from_slice(data);
        out
    }
    fn number(number: u64, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3, &mut out);
        varint(value, &mut out);
        out
    }
    fn projection(url: &str, value: &[u8]) -> (bool, serde_json::Value) {
        let data = [bytes(1, url.as_bytes()), bytes(2, value)].concat();
        let staged = ByteSource::arena().unwrap();
        staged.append(|out| Ok(out.write_all(&data)?)).unwrap();
        let file = staged.file().unwrap();
        let any = Proto::read(&file, 0, data.len() as u64).unwrap();
        let mut out = b"{\"sentinel\":true".to_vec();
        let known = project(&file, &any, &mut out).unwrap();
        out.push(b'}');
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        (known, value["native_payload"].clone())
    }

    #[test]
    fn command_and_edit_payloads_keep_present_scalars_only() {
        let sandbox = [number(1, 1), bytes(2, b"denied write")].concat();
        let command = [
            number(1, (-2i64) as u64),
            bytes(3, &sandbox),
            bytes(5, b"zsh"),
            bytes(6, b"a command line the call arguments already hold"),
            bytes(7, b"/work"),
        ]
        .concat();
        let (known, payload) = projection("type.googleapis.com/exa.cortex.core.tools.RunCommand", &command);
        assert!(known);
        assert_eq!(
            payload,
            serde_json::json!({
                "type": "exa.cortex.core.tools.RunCommand", "exit_code": -2, "shell_name": "zsh", "cwd": "/work",
                "sandbox_details": {"ran_in_sandbox": true, "sandbox_errors": "denied write"}
            })
        );
        // A zero exit code is absent on the wire and is not asserted here either.
        let (_, payload) = projection(
            "type.googleapis.com/exa.cortex.core.tools.RunCommand",
            &bytes(7, b"/work"),
        );
        assert!(payload.get("exit_code").is_none());

        let edit = [
            number(1, 2),
            bytes(2, b"/work/src/lib.rs"),
            bytes(3, b"an unbounded diff block"),
            bytes(4, &[number(1, 12), number(2, 3)].concat()),
        ]
        .concat();
        let (known, payload) = projection("type.googleapis.com/exa.cortex.core.tools.FileEdit", &edit);
        assert!(known);
        assert_eq!(payload["diff_stats"]["additions"], 12);
        assert_eq!(payload["path"], "/work/src/lib.rs");
        assert!(payload.get("diff").is_none());
    }

    #[test]
    fn repeated_identities_and_unknown_schemas_are_distinct() {
        let results = [
            bytes(
                1,
                &[
                    bytes(1, b"self"),
                    bytes(2, b"Repo helper"),
                    number(4, 1),
                    bytes(12, b"inherit"),
                ]
                .concat(),
            ),
            bytes(2, &bytes(1, b"child-one")),
            bytes(2, &bytes(2, b"file:///log")),
            bytes(2, &bytes(1, b"child-two")),
        ]
        .concat();
        let (known, payload) = projection("type.googleapis.com/exa.cortex.core.tools.InvokeSubagent", &results);
        assert!(known);
        assert_eq!(
            payload["subagents"],
            serde_json::json!([{
                "type_name": "self",
                "role": "Repo helper",
                "inherit": true,
                "model_option": "inherit"
            }])
        );
        assert_eq!(
            payload["conversation_ids"],
            serde_json::json!(["child-one", "child-two"])
        );

        let (known, payload) = projection(
            "type.googleapis.com/exa.cortex.core.tools.DefineSubagent",
            &[bytes(1, b"verifier"), bytes(4, b"Verify output")].concat(),
        );
        assert!(known);
        assert_eq!(payload["name"], "verifier");
        assert_eq!(payload["description"], "Verify output");

        let (known, payload) = projection(
            "type.googleapis.com/exa.cortex.core.tools.GenerateImage",
            &[bytes(2, b"/tmp/a.png"), number(5, 1024), bytes(6, b"imagen")].concat(),
        );
        assert!(known);
        assert_eq!(payload["image_paths"], serde_json::json!(["/tmp/a.png"]));
        assert_eq!(payload["image_num_bytes"], 1024);
        assert_eq!(payload["model_name"], "imagen");

        let question = [bytes(1, b"Proceed?"), number(3, 0), bytes(4, b"yes")].concat();
        let (known, payload) = projection(
            "type.googleapis.com/exa.cortex.core.tools.AskQuestion",
            &bytes(1, &question),
        );
        assert!(known);
        assert_eq!(payload["questions"][0]["question"], "Proceed?");
        assert_eq!(
            payload["questions"][0]["selected_option_ids"],
            serde_json::json!(["yes"])
        );

        let (known, payload) = projection("type.googleapis.com/gemini_coder.Step", &bytes(14, b""));
        assert!(known);
        assert_eq!(payload["step"], "view_file");

        let (known, payload) = projection("type.googleapis.com/exa.cortex.core.tools.Future", &bytes(1, b"x"));
        assert!(!known);
        assert_eq!(payload, serde_json::json!({"type": "exa.cortex.core.tools.Future"}));
    }
}
