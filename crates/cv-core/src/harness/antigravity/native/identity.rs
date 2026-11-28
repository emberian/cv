//! Planner and tool-call identity metadata. Binary signatures stay explicitly
//! encoded bytes; they are never substituted for the planner's text signature.
use super::wire::{Field, Message as Proto};
use anyhow::Result;
use std::fs::File;
use std::io::Write;

pub(super) fn planner(file: &File, response: &Proto, out: &mut impl Write) -> Result<()> {
    out.write_all(b",\"native_planner_details\":{")?;
    let mut first = true;
    text(file, response, 6, "message_id", out, &mut first)?;
    text(file, response, 15, "provider_message_id", out, &mut first)?;
    if let Some(reason) = response.last(12).and_then(|field| field.u64()) {
        key("stop_reason", out, &mut first)?;
        write!(out, "{}", reason as i32)?;
        if let Some(name) = stop_name(reason) {
            key("stop", out, &mut first)?;
            write!(out, "\"{name}\"")?;
        }
    }
    if let Some(duration) = response.last(11) {
        let duration = duration.message(file)?;
        key("thinking_duration", out, &mut first)?;
        let seconds = duration.last(1).and_then(|field| field.u64()).unwrap_or(0) as i64;
        let nanos = duration.last(2).and_then(|field| field.u64()).unwrap_or(0) as i32;
        write!(out, "{{\"seconds\":{seconds},\"nanos\":{nanos}}}")?;
    }
    if let Some(signature) = response.last(14) {
        key("thinking_signature", out, &mut first)?;
        signature_bytes(file, signature, out)?;
    }
    if response.last(7).is_some() {
        key("tool_calls", out, &mut first)?;
        out.write_all(b"[")?;
        for (i, call) in response.repeated(7).enumerate() {
            if i > 0 {
                out.write_all(b",")?;
            }
            let call = call.message(file)?;
            out.write_all(b"{")?;
            let mut first = true;
            text(file, &call, 1, "id", out, &mut first)?;
            text(file, &call, 2, "name", out, &mut first)?;
            text(file, &call, 6, "thought_signature", out, &mut first)?;
            if let Some(signature) = call.last(7) {
                key("thinking_signature", out, &mut first)?;
                signature_bytes(file, signature, out)?;
            }
            out.write_all(b"}")?;
        }
        out.write_all(b"]")?;
    }
    out.write_all(b"}")?;
    Ok(())
}

/// `codeium_common.StopReason` in the 1.5.9 descriptors. A model stream the client
/// cancelled is written as 18, not 16; a stream the peer dropped has no value at all.
fn stop_name(reason: u64) -> Option<&'static str> {
    Some(match reason {
        1 => "incomplete",
        2 => "stop_pattern",
        3 => "max_tokens",
        4 => "min_log_prob",
        5 => "max_newlines",
        6 => "exit_scope",
        7 => "nonfinite_logit_or_prob",
        8 => "first_non_whitespace_line",
        9 => "partial",
        10 => "function_call",
        11 => "content_filter",
        12 => "non_insertion",
        13 => "error",
        14 => "improper_format",
        15 => "other",
        16 => "client_canceled",
        17 => "client_tool_parse_error",
        18 => "client_stream_error",
        19 => "client_looping",
        20 => "client_invalid_message_order",
        _ => return None,
    })
}

fn signature_bytes(file: &File, signature: Field, out: &mut impl Write) -> Result<()> {
    out.write_all(b"{\"encoding\":\"base64\",\"data\":\"")?;
    super::media::base64(signature.reader(file)?, out)?;
    out.write_all(b"\"}")?;
    Ok(())
}

fn key(name: &str, out: &mut impl Write, first: &mut bool) -> Result<()> {
    if !*first {
        out.write_all(b",")?;
    }
    *first = false;
    write!(out, "\"{name}\":")?;
    Ok(())
}

fn text(file: &File, message: &Proto, number: u32, name: &str, out: &mut impl Write, first: &mut bool) -> Result<()> {
    if let Some(field) = message.last(number) {
        key(name, out, first)?;
        field.write_json_string(file, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::antigravity::cold::proto::{field as bytes, int as number};
    fn projection(bytes: &[u8]) -> serde_json::Value {
        let source = crate::harness::store::bytes::ByteSource::arena().unwrap();
        source
            .append(|out| {
                out.write_all(bytes)?;
                Ok(())
            })
            .unwrap();
        let file = source.file().unwrap();
        let response = Proto::read(&file, 0, bytes.len() as u64).unwrap();
        let mut out = b"{\"test\":true".to_vec();
        planner(&file, &response, &mut out).unwrap();
        out.push(b'}');
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()["native_planner_details"].clone()
    }
    #[test]
    fn binary_planner_and_tool_signatures_remain_distinct_from_text() {
        let call = [
            bytes(1, b"call-id"),
            bytes(2, b"run_command"),
            bytes(6, b"text-thought-signature"),
            bytes(7, &[0, 255, 128, 1]),
        ]
        .concat();
        let data = [
            bytes(4, b"planner-text-signature"),
            bytes(14, &[255, 0, 128]),
            bytes(7, &call),
        ]
        .concat();
        let projected = projection(&data);
        assert_eq!(
            projected["thinking_signature"],
            serde_json::json!({"encoding":"base64", "data":"/wCA"})
        );
        assert_eq!(
            projected["tool_calls"][0],
            serde_json::json!({
                "id":"call-id", "name":"run_command", "thought_signature":"text-thought-signature",
                "thinking_signature":{"encoding":"base64", "data":"AP+AAQ=="}
            })
        );
        assert!(projected.get("signature").is_none());
    }
    #[test]
    fn stop_reasons_are_named_and_an_absent_one_is_not_invented() {
        // CLI 1.5.9 wrote 2 for a finished reply and 18 for a stream the client cancelled.
        for (reason, name) in [
            (2, "stop_pattern"),
            (18, "client_stream_error"),
            (16, "client_canceled"),
        ] {
            let projected = projection(&[bytes(1, b"reply"), number(12, reason)].concat());
            assert_eq!(projected["stop_reason"], reason);
            assert_eq!(projected["stop"], name);
        }
        let dropped = projection(&bytes(1, b"partial reply"));
        assert!(dropped.get("stop_reason").is_none() && dropped.get("stop").is_none());
        let future = projection(&number(12, 77));
        assert_eq!(future["stop_reason"], 77);
        assert!(future.get("stop").is_none());
    }
    #[test]
    fn planner_message_duration_and_unknown_stop_reason_are_preserved() {
        let duration = [number(1, 2), number(2, 123_456_789)].concat();
        let data = [
            bytes(6, b"message-id"),
            bytes(15, b"provider-id"),
            number(12, 1234),
            bytes(11, &duration),
        ]
        .concat();
        assert_eq!(
            projection(&data),
            serde_json::json!({
                "message_id":"message-id", "provider_message_id":"provider-id", "stop_reason":1234,
                "thinking_duration":{"seconds":2,"nanos":123456789}
            })
        );
        let signed = [number(1, (-2i64) as u64), number(2, (-123i64) as u64)].concat();
        assert_eq!(
            projection(&bytes(11, &signed))["thinking_duration"],
            serde_json::json!({"seconds":-2,"nanos":-123})
        );
        assert_eq!(
            projection(&bytes(11, &[]))["thinking_duration"],
            serde_json::json!({"seconds":0,"nanos":0})
        );
    }
    #[test]
    fn empty_binary_signature_preserves_presence() {
        assert_eq!(
            projection(&bytes(14, &[]))["thinking_signature"],
            serde_json::json!({"encoding":"base64", "data":""})
        );
    }
}
