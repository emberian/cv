//! Which fields of a native step no decoder reads. A field counts as accounted for when
//! the projection normalizes it, or when it is listed here as carrying nothing a
//! conversation reader lacks: writer bookkeeping (timestamps after creation, execution and
//! content ids, credits, config snapshots, UI labels) or a second rendering of a value
//! that is read (`user_response` beside its items, `modified_response` beside the
//! response, a system message's `agent_message` and render info beside its text, the
//! alternative texts of one error). Everything else present on the wire makes the step
//! report `unsupported_native_fields`; COMPLETE keeps its bytes either way.
//! Field numbers are those of the 1.5.9 descriptors.
use super::{File, Proto, Result};

const STEP: &[u32] = &[1, 4, 5, 19, 20, 24, 30, 31, 114, 133, 140, 147, 148];
/// Read: created_at 1, source 3, tool_call 4, model_usage 9.
const METADATA: &[u32] = &[
    1, 3, 4, 9, // read
    5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 30, 31, 32, 33, 35, 36, 38, 39,
];
/// Read: id 1, name 2, arguments 3 or invalid arguments 4, signatures 6 and 7.
const TOOL_CALL: &[u32] = &[1, 2, 3, 4, 5, 6, 7];
/// Read: query 1, items 3, media 9.
const USER_INPUT: &[u32] = &[1, 3, 9, 2, 4, 6, 8, 12, 13, 16, 17, 18, 19];
/// Read: response 1, thinking 3-5, message ids 6 and 15, tool calls 7, duration 11,
/// stop reason 12, binary signature 14, media 20.
const PLANNER: &[u32] = &[1, 3, 4, 5, 6, 7, 11, 12, 14, 15, 20, 8];
/// Read: result 2. The arguments map repeats the call's arguments.
const GENERIC: &[u32] = &[2, 1];
/// Read: text 1, render info 3, media 4, typed payload 6, output link 7, struct 10, value 11.
const RESULT: &[u32] = &[1, 3, 4, 6, 7, 10, 11];
/// Read: message 1, media 6.
const SYSTEM_MESSAGE: &[u32] = &[1, 6, 2, 3, 4];
/// Read: session summary 5, step bounds 1/11/12/27, snapshots & captured requests 14/20/21/26/30/31.
const CHECKPOINT: &[u32] = &[
    5, 12, 14, 20, 21, 26, 30, 31, 1, 3, 9, 11, 15, 16, 17, 24, 25, 27, 28, 29,
];
/// Read: error 3.
const ERROR_MESSAGE: &[u32] = &[3, 4, 5];
/// Read: the first present of 1, 9, 3, 2, 5 and the short/full error 2 or 3.
const ERROR_DETAILS: &[u32] = &[1, 2, 3, 5, 9, 4, 6, 7, 11, 12];
/// Read: id, log, progress, description, flags, title, time.
const TASK: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8];

fn unread(message: &Proto, known: &[u32]) -> bool {
    message.fields().iter().any(|field| !known.contains(&field.number))
}

fn unread_in(file: &File, parent: &Proto, number: u32, known: &[u32]) -> Result<bool> {
    for field in parent.repeated(number) {
        if unread(&field.message(file)?, known) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `step`, of projected `kind` with member `payload`, holds a field that is
/// neither read nor listed above. Typed payloads, structured results and interactions
/// report through their own decoders.
pub(super) fn unread_fields(file: &File, step: &Proto, kind: &str, payload: Option<&Proto>) -> Result<bool> {
    if unread(step, STEP)
        || unread_in(file, step, 5, METADATA)?
        || unread_in(file, step, 31, ERROR_DETAILS)?
        || unread_in(file, step, 148, TASK)?
    {
        return Ok(true);
    }
    for metadata in step.repeated(5) {
        if unread_in(file, &metadata.message(file)?, 4, TOOL_CALL)? {
            return Ok(true);
        }
    }
    let Some(payload) = payload else {
        // A type the projection does not name; a cleared row has nothing left to read.
        return Ok(kind == "NATIVE_UNKNOWN");
    };
    Ok(match kind {
        "USER_INPUT" => unread(payload, USER_INPUT),
        "PLANNER_RESPONSE" => unread(payload, PLANNER) || unread_in(file, payload, 7, TOOL_CALL)?,
        "GENERIC" => unread(payload, GENERIC) || unread_in(file, payload, 2, RESULT)?,
        "SYSTEM_MESSAGE" => unread(payload, SYSTEM_MESSAGE),
        "CHECKPOINT" => unread(payload, CHECKPOINT),
        "ERROR_MESSAGE" => unread(payload, ERROR_MESSAGE) || unread_in(file, payload, 3, ERROR_DETAILS)?,
        _ => true,
    })
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
    fn unread_step(kind: &str, member: u32, data: &[u8]) -> bool {
        let staged = ByteSource::arena().unwrap();
        staged.append(|out| Ok(out.write_all(data)?)).unwrap();
        let file = staged.file().unwrap();
        let step = Proto::read(&file, 0, data.len() as u64).unwrap();
        let payload = step.last(member).and_then(|field| field.message(&file).ok());
        unread_fields(&file, &step, kind, payload.as_ref()).unwrap()
    }

    #[test]
    fn only_fields_no_decoder_accounts_for_are_reported() {
        // A planner step as the writer stores it: metadata with timestamps and ids,
        // response, thinking, a call with a signature.
        let metadata = [
            bytes(1, &[8, 1]),
            bytes(8, &[8, 2]),
            bytes(12, b"execution"),
            bytes(35, b"content-id"),
        ]
        .concat();
        let call = [bytes(1, b"id"), bytes(2, b"tool"), bytes(3, b"{}"), bytes(7, b"sig")].concat();
        let planner = [bytes(1, b"answer"), bytes(3, b"thinking"), bytes(7, &call)].concat();
        let step = [bytes(5, &metadata), bytes(20, &planner)].concat();
        assert!(!unread_step("PLANNER_RESPONSE", 20, &step));

        // Each of these holds content nothing reads: hook results in the metadata, raw
        // thinking, a call's original arguments, an unknown planner field, a result's data map.
        let hooked = [
            bytes(5, &[metadata.clone(), bytes(34, b"hook")].concat()),
            bytes(20, &planner),
        ]
        .concat();
        assert!(unread_step("PLANNER_RESPONSE", 20, &hooked));
        for extra in [bytes(16, b"raw thinking"), bytes(9999, b"future")] {
            let step = bytes(20, &[planner.clone(), extra].concat());
            assert!(unread_step("PLANNER_RESPONSE", 20, &step));
        }
        let rewritten = bytes(20, &bytes(7, &[call.clone(), bytes(8, b"{\"original\":1}")].concat()));
        assert!(unread_step("PLANNER_RESPONSE", 20, &rewritten));
        let result = bytes(140, &bytes(2, &[bytes(1, b"output"), bytes(5, b"data entry")].concat()));
        assert!(unread_step("GENERIC", 140, &result));
        assert!(!unread_step(
            "GENERIC",
            140,
            &bytes(140, &bytes(2, &bytes(1, b"output")))
        ));

        // A step type the projection does not name, and a cleared row with no member.
        assert!(unread_step("NATIVE_UNKNOWN", 9999, &bytes(9999, b"future step")));
        assert!(!unread_step("SYSTEM_MESSAGE", 114, &[8, 101, 32, 5]));
    }
}
