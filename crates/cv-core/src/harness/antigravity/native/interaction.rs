//! Permission interactions recorded on a tool step. Field numbers are those CLI 1.5.9
//! writes: Step.requested_interaction(133).permission(2) while a request is open or was
//! approved, Step.completed_interaction(147) with the request spec(1).permission(21) and
//! the response(2).permission(21) whose field 1 is the allow flag. Other interaction
//! kinds stay uninterpreted wire, disclosed by the step's unsupported-field coverage.
use super::{File, Proto, Result, Write};

pub(super) fn project(file: &File, step: &Proto, out: &mut impl Write) -> Result<()> {
    let completed = step.last(147).map(|field| field.message(file)).transpose()?;
    let asked = match &completed {
        Some(completed) => match completed.last(1) {
            Some(spec) => spec.message(file)?.last(21),
            None => None,
        },
        None => None,
    };
    let requested = match step.last(133) {
        Some(field) => field.message(file)?.last(2),
        None => None,
    };
    let Some(request) = asked.or(requested) else {
        return Ok(());
    };
    out.write_all(b",\"native_interaction\":{\"kind\":\"permission\"")?;
    if let Some(resource) = request.message(file)?.last(1) {
        let resource = resource.message(file)?;
        super::text(file, &resource, 1, "action", out)?;
        super::text(file, &resource, 2, "target", out)?;
    }
    let answer = match completed.as_ref().and_then(|completed| completed.last(2)) {
        Some(response) => response.message(file)?.last(21),
        None => None,
    };
    if let Some(answer) = answer {
        let allowed = answer.message(file)?.last(1).and_then(|field| field.u64()).unwrap_or(0) != 0;
        write!(out, ",\"decision\":\"{}\"", if allowed { "approved" } else { "denied" })?;
    }
    out.write_all(b"}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ByteSource;
    use super::*;

    fn bytes(number: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for mut value in [u64::from(number << 3 | 2), data.len() as u64] {
            while value >= 128 {
                out.push(value as u8 | 128);
                value >>= 7;
            }
            out.push(value as u8);
        }
        out.extend_from_slice(data);
        out
    }

    fn projection(data: &[u8]) -> serde_json::Value {
        let staged = ByteSource::arena().unwrap();
        staged.append(|out| Ok(out.write_all(data)?)).unwrap();
        let file = staged.file().unwrap();
        let step = Proto::read(&file, 0, data.len() as u64).unwrap();
        let mut out = b"{\"sentinel\":true".to_vec();
        project(&file, &step, &mut out).unwrap();
        out.push(b'}');
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn an_open_request_has_no_decision_and_an_answer_is_never_assumed() {
        let resource = [bytes(1, b"command"), bytes(2, b"make test")].concat();
        let open = bytes(133, &bytes(2, &bytes(1, &resource)));
        assert_eq!(
            projection(&open)["native_interaction"],
            serde_json::json!({"kind": "permission", "action": "command", "target": "make test"})
        );
        let spec = bytes(1, &bytes(21, &bytes(1, &resource)));
        for (answer, decision) in [(vec![8, 1], "approved"), (bytes(4, b""), "denied")] {
            let completed = bytes(147, &[spec.clone(), bytes(2, &bytes(21, &answer))].concat());
            assert_eq!(projection(&completed)["native_interaction"]["decision"], decision);
        }
        // A completed interaction with no permission response decides nothing.
        let unanswered = bytes(147, &[spec, bytes(2, &bytes(1, b"trajectory"))].concat());
        assert!(projection(&unanswered)["native_interaction"].get("decision").is_none());
        // Another interaction kind is not a permission.
        assert!(projection(&bytes(133, &bytes(5, b"question")))
            .get("native_interaction")
            .is_none());
    }
}
