//! User interactions recorded on a tool step (`1.0.x`..`1.5.13`):
//! - `Step.requested_interaction (56)`: open `RequestedInteraction`
//! - `Step.permissions (133)` (or SQLite `steps.permissions`): `TrajectoryPermissions`
//! - `Step.completed_interaction (147)`: `CompletedInteraction { 1: RequestedInteraction, 2: CascadeUserInteraction }`
//!
//! Unknown future interaction fields emit an `"unsupported"` marker for `Tier::Rich`+ while
//! returning `false` so `unsupported_native_fields` remains accurate.
use super::{wire, File, Proto, Result, Write};

fn nonempty(field: wire::Field) -> bool {
    matches!(field.value, wire::Value::Bytes { len, .. } if len > 0)
}

fn write_decision(allowed: bool, out: &mut impl Write) -> Result<()> {
    write!(out, ",\"decision\":\"{}\"", if allowed { "approved" } else { "denied" })?;
    Ok(())
}

fn write_string_or_enum(
    file: &File,
    field: wire::Field,
    key: &str,
    names: &[(u64, &str)],
    out: &mut impl Write,
) -> Result<()> {
    match field.value {
        wire::Value::Bytes { len, .. } if len > 0 => {
            write!(out, ",\"{key}\":")?;
            field.write_json_string(file, out)?;
        }
        wire::Value::Varint(v) if v != 0 => {
            if let Some((_, name)) = names.iter().find(|(code, _)| *code == v) {
                write!(out, ",\"{key}\":\"{name}\"")?;
            } else {
                write!(out, ",\"{key}\":{v}")?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn project(
    file: &File,
    step: &Proto,
    side_permissions: Option<&Proto>,
    out: &mut impl Write,
) -> Result<bool> {
    let completed = step.last(147).map(|field| field.message(file)).transpose()?;
    let req = match completed.as_ref().and_then(|c| c.last(1)) {
        Some(spec) => Some(spec.message(file)?),
        None => step.last(56).map(|field| field.message(file)).transpose()?,
    };
    let resp = match completed.as_ref().and_then(|c| c.last(2)) {
        Some(r) => Some(r.message(file)?),
        None => None,
    };
    let step_perm = step.last(133).map(|field| field.message(file)).transpose()?;
    let traj_perm = step_perm.as_ref().or(side_permissions);

    let finish = |resp: &Option<Proto>, out: &mut dyn Write| -> Result<bool> {
        if resp
            .as_ref()
            .and_then(|r| r.last(24))
            .and_then(|f| f.u64())
            .unwrap_or(0)
            != 0
        {
            out.write_all(b",\"timed_out\":true")?;
        }
        out.write_all(b"}")?;
        Ok(true)
    };

    // 1. `permission` (`req.last(21)` or `traj_perm.last(2)`, `resp.last(21)` `{1: allow}`)
    let perm_req = req
        .as_ref()
        .and_then(|r| r.last(21))
        .or_else(|| traj_perm.and_then(|tp| tp.last(2)));
    let perm_resp = resp.as_ref().and_then(|r| r.last(21));
    if perm_req.is_some() || perm_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"permission\"")?;
        if let Some(request) = perm_req {
            if let Some(resource) = request.message(file)?.last(1) {
                let resource = resource.message(file)?;
                super::text(file, &resource, 1, "action", out)?;
                super::text(file, &resource, 2, "target", out)?;
            }
        }
        if let Some(answer) = perm_resp {
            let allowed = answer.message(file)?.last(1).and_then(|f| f.u64()).unwrap_or(0) != 0;
            write_decision(allowed, out)?;
        }
        return finish(&resp, out);
    }

    // 2. `file_permission` (`req.last(19)` `{1: absolute_path_uri}`, `resp.last(19)` `{1: allow, 3: absolute_path_uri}`)
    let file_req = req
        .as_ref()
        .and_then(|r| r.last(19))
        .map(|f| f.message(file))
        .transpose()?;
    let file_resp = resp
        .as_ref()
        .and_then(|r| r.last(19))
        .map(|f| f.message(file))
        .transpose()?;
    if file_req.is_some() || file_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"permission\",\"action\":\"file\"")?;
        let target = file_resp
            .as_ref()
            .and_then(|r| r.last(3))
            .filter(|f| nonempty(*f))
            .or_else(|| file_req.as_ref().and_then(|r| r.last(1)).filter(|f| nonempty(*f)))
            .or_else(|| file_req.as_ref().and_then(|r| r.last(1)))
            .or_else(|| file_resp.as_ref().and_then(|r| r.last(3)));
        if let Some(target) = target {
            out.write_all(b",\"target\":")?;
            target.write_json_string(file, out)?;
        }
        if let Some(answer) = &file_resp {
            let allowed = answer.last(1).and_then(|f| f.u64()).unwrap_or(0) != 0;
            write_decision(allowed, out)?;
        }
        return finish(&resp, out);
    }

    // 3. `run_command` (`req.last(3)`, `resp.last(5)` `{1: confirm, 2: proposed_command_line, 3: submitted_command_line}`)
    let cmd_req = req
        .as_ref()
        .and_then(|r| r.last(3))
        .map(|f| f.message(file))
        .transpose()?;
    let cmd_resp = resp
        .as_ref()
        .and_then(|r| r.last(5))
        .map(|f| f.message(file))
        .transpose()?;
    if cmd_req.is_some() || cmd_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"permission\",\"action\":\"run_command\"")?;
        let target = cmd_resp
            .as_ref()
            .and_then(|r| {
                r.last(3)
                    .filter(|f| nonempty(*f))
                    .or_else(|| r.last(2).filter(|f| nonempty(*f)))
            })
            .or_else(|| {
                cmd_req.as_ref().and_then(|r| {
                    r.last(2)
                        .filter(|f| nonempty(*f))
                        .or_else(|| r.last(1).filter(|f| nonempty(*f)))
                })
            });
        if let Some(target) = target {
            out.write_all(b",\"target\":")?;
            target.write_json_string(file, out)?;
        }
        if let Some(answer) = &cmd_resp {
            let confirmed = answer.last(1).and_then(|f| f.u64()).unwrap_or(0) != 0;
            write_decision(confirmed, out)?;
        }
        return finish(&resp, out);
    }

    // 4. `approval_interaction` (`req.last(23)`, `resp.last(23)` `{1: confirm}`)
    let app_req = req.as_ref().and_then(|r| r.last(23));
    let app_resp = resp
        .as_ref()
        .and_then(|r| r.last(23))
        .map(|f| f.message(file))
        .transpose()?;
    if app_req.is_some() || app_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"permission\",\"action\":\"approval\"")?;
        if let Some(answer) = &app_resp {
            let confirmed = answer.last(1).and_then(|f| f.u64()).unwrap_or(0) != 0;
            write_decision(confirmed, out)?;
        }
        return finish(&resp, out);
    }

    // 5. `ask_question` (`req.last(22)` `{1: questions}`, `resp.last(22)` `{1: responses, 2: cancelled}`)
    let ask_req = req
        .as_ref()
        .and_then(|r| r.last(22))
        .map(|f| f.message(file))
        .transpose()?;
    let ask_resp = resp
        .as_ref()
        .and_then(|r| r.last(22))
        .map(|f| f.message(file))
        .transpose()?;
    if ask_req.is_some() || ask_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"ask_question\"")?;
        if let Some(cancelled) = ask_resp.as_ref().and_then(|r| r.last(2)).and_then(|f| f.u64()) {
            write!(out, ",\"cancelled\":{}", cancelled != 0)?;
        }
        if let Some(source) = ask_resp
            .as_ref()
            .filter(|r| r.last(1).is_some())
            .or_else(|| ask_req.as_ref().filter(|r| r.last(1).is_some()))
        {
            super::payload::members(file, source, super::payload::ASK_QUESTION_FIELDS, out, &mut false)?;
        }
        return finish(&resp, out);
    }

    // 6. `elicitation` (`req.last(20)` `{1: server_name, 2: mode, 3: message, 5: url}`, `resp.last(20)` `{1: action}`)
    let elic_req = req
        .as_ref()
        .and_then(|r| r.last(20))
        .map(|f| f.message(file))
        .transpose()?;
    let elic_resp = resp
        .as_ref()
        .and_then(|r| r.last(20))
        .map(|f| f.message(file))
        .transpose()?;
    if elic_req.is_some() || elic_resp.is_some() {
        out.write_all(b",\"native_interaction\":{\"kind\":\"elicitation\"")?;
        if let Some(spec) = &elic_req {
            super::text(file, spec, 1, "server_name", out)?;
            if let Some(mode) = spec.last(2) {
                write_string_or_enum(file, mode, "mode", &[(1, "form"), (2, "url")], out)?;
            }
            super::text(file, spec, 3, "message", out)?;
            super::text(file, spec, 5, "url", out)?;
        }
        if let Some(answer) = &elic_resp {
            if let Some(action) = answer.last(1) {
                write_string_or_enum(
                    file,
                    action,
                    "decision",
                    &[(1, "accept"), (2, "decline"), (3, "cancel")],
                    out,
                )?;
            }
        }
        return finish(&resp, out);
    }

    let unsupported_field = if completed.is_some() {
        req.as_ref()
            .and_then(|spec| spec.fields().last().map(|f| f.number))
            .or_else(|| {
                resp.as_ref().and_then(|r| {
                    r.fields()
                        .iter()
                        .rev()
                        .find(|f| !matches!(f.number, 1 | 2 | 24 | 25))
                        .map(|f| f.number)
                })
            })
            .or(Some(147))
    } else if let Some(spec) = &req {
        spec.fields().last().map(|f| f.number).or(Some(56))
    } else if let Some(perm) = traj_perm {
        perm.fields().last().map(|f| f.number).or(Some(133))
    } else {
        None
    };
    if let Some(native_field) = unsupported_field {
        write!(
            out,
            ",\"native_interaction\":{{\"kind\":\"unsupported\",\"native_field\":{native_field}}}"
        )?;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::ByteSource;
    use super::*;

    use crate::harness::antigravity::cold::proto::{field as bytes, int as uint};

    fn projection(data: &[u8]) -> (bool, serde_json::Value) {
        let staged = ByteSource::arena().unwrap();
        staged.append(|out| Ok(out.write_all(data)?)).unwrap();
        let file = staged.file().unwrap();
        let step = Proto::read(&file, 0, data.len() as u64).unwrap();
        let mut out = b"{\"sentinel\":true".to_vec();
        let interpreted = project(&file, &step, None, &mut out).unwrap();
        out.push(b'}');
        (interpreted, serde_json::from_slice(&out).unwrap())
    }

    #[test]
    fn an_open_request_has_no_decision_and_an_answer_is_never_assumed() {
        let resource = [bytes(1, b"command"), bytes(2, b"make test")].concat();
        let open = bytes(133, &bytes(2, &bytes(1, &resource)));
        let (interpreted, val) = projection(&open);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({"kind": "permission", "action": "command", "target": "make test"})
        );
        // Open RequestedInteraction on field 56 also decodes without inventing a decision.
        let open_56 = bytes(56, &bytes(21, &bytes(1, &resource)));
        let (interpreted, val) = projection(&open_56);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({"kind": "permission", "action": "command", "target": "make test"})
        );

        let spec = bytes(1, &bytes(21, &bytes(1, &resource)));
        for (answer, decision) in [(vec![8, 1], "approved"), (bytes(4, b""), "denied")] {
            let completed = bytes(147, &[spec.clone(), bytes(2, &bytes(21, &answer))].concat());
            let (interpreted, val) = projection(&completed);
            assert!(interpreted);
            assert_eq!(val["native_interaction"]["decision"], decision);
        }
        // A completed interaction with no permission response decides nothing.
        let unanswered = bytes(147, &[spec, bytes(2, &bytes(1, b"trajectory"))].concat());
        let (interpreted, val) = projection(&unanswered);
        assert!(interpreted);
        assert!(val["native_interaction"].get("decision").is_none());

        // Unknown interaction fields emit an unsupported marker and return false.
        let (interpreted, val) = projection(&bytes(133, &bytes(5, b"question")));
        assert!(!interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({"kind": "unsupported", "native_field": 5})
        );
        let (interpreted, val) = projection(&bytes(147, &bytes(1, &bytes(99, &[]))));
        assert!(!interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({"kind": "unsupported", "native_field": 99})
        );
    }

    #[test]
    fn file_command_approval_question_and_elicitation_interactions_decode() {
        // file_permission (19)
        let file_completed = bytes(
            147,
            &[
                bytes(1, &bytes(19, &bytes(1, b"file:///work/secret.env"))),
                bytes(2, &bytes(19, &uint(1, 1))),
            ]
            .concat(),
        );
        let (interpreted, val) = projection(&file_completed);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({
                "kind": "permission",
                "action": "file",
                "target": "file:///work/secret.env",
                "decision": "approved"
            })
        );

        // run_command (req 3 / resp 5)
        let cmd_completed = bytes(
            147,
            &[
                bytes(1, &bytes(3, &bytes(2, b"rm -rf build"))),
                bytes(
                    2,
                    &[
                        bytes(
                            5,
                            &[uint(1, 1), bytes(2, b"rm -rf build"), bytes(3, b"rm -rf build/tmp")].concat(),
                        ),
                        uint(24, 1),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        let (interpreted, val) = projection(&cmd_completed);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({
                "kind": "permission",
                "action": "run_command",
                "target": "rm -rf build/tmp",
                "decision": "approved",
                "timed_out": true
            })
        );

        // approval_interaction (23)
        let approval_denied = bytes(
            147,
            &[bytes(1, &bytes(23, &[])), bytes(2, &bytes(23, &uint(1, 0)))].concat(),
        );
        let (interpreted, val) = projection(&approval_denied);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({
                "kind": "permission",
                "action": "approval",
                "decision": "denied"
            })
        );

        // ask_question (22): responses win when non-empty; cancelled falls back to asked questions.
        let opt = [bytes(1, b"opt-1"), bytes(2, b"Use SQLite")].concat();
        let asked_q = [bytes(1, b"Which store?"), bytes(2, &opt), uint(3, 0)].concat();
        let answered_q = [
            bytes(1, b"Which store?"),
            bytes(2, &opt),
            bytes(4, b"opt-1"),
            bytes(5, b"custom note"),
        ]
        .concat();
        let ask_completed = bytes(
            147,
            &[
                bytes(1, &bytes(22, &bytes(1, &asked_q))),
                bytes(2, &bytes(22, &bytes(1, &answered_q))),
            ]
            .concat(),
        );
        let (interpreted, val) = projection(&ask_completed);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({
                "kind": "ask_question",
                "questions": [{
                    "question": "Which store?",
                    "options": [{"id": "opt-1", "text": "Use SQLite"}],
                    "selected_option_ids": ["opt-1"],
                    "write_in_response": "custom note"
                }]
            })
        );

        let ask_cancelled = bytes(
            147,
            &[
                bytes(1, &bytes(22, &bytes(1, &asked_q))),
                bytes(2, &bytes(22, &uint(2, 1))),
            ]
            .concat(),
        );
        let (interpreted, val) = projection(&ask_cancelled);
        assert!(interpreted);
        assert_eq!(val["native_interaction"]["cancelled"], true);
        assert_eq!(val["native_interaction"]["questions"][0]["question"], "Which store?");

        // elicitation (20)
        let elic = bytes(
            147,
            &[
                bytes(
                    1,
                    &bytes(
                        20,
                        &[
                            bytes(1, b"github"),
                            bytes(2, b"url"),
                            bytes(3, b"Authorize OAuth"),
                            bytes(5, b"https://example.invalid/auth"),
                        ]
                        .concat(),
                    ),
                ),
                bytes(2, &bytes(20, &uint(1, 1))),
            ]
            .concat(),
        );
        let (interpreted, val) = projection(&elic);
        assert!(interpreted);
        assert_eq!(
            val["native_interaction"],
            serde_json::json!({
                "kind": "elicitation",
                "server_name": "github",
                "mode": "url",
                "message": "Authorize OAuth",
                "url": "https://example.invalid/auth",
                "decision": "accept"
            })
        );
    }
}
