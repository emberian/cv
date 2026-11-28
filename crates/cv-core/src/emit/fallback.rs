//! Canonical degraded-media and tool-fallback formatters shared between native emitters and
//! [`crate::emit::fidelity`], so emitter output and round-trip fidelity projection cannot drift.

pub(crate) const FILE_PREFIX: &str = "[file: ";
pub(crate) const TOOL_CALL_PREFIX: &str = "[tool call: ";
pub(crate) const TOOL_CALL_SEP: &str = " ";
pub(crate) const TOOL_RESULT_PREFIX: &str = "[tool result: ";
pub(crate) const BRACKET_SUFFIX: &str = "]";

/// Split a `data:[<mime>];base64,<payload>` URI into `(mime, base64_payload)`.
pub(crate) fn parse_base64_data_uri(data_ref: Option<&str>) -> Option<(&str, &str)> {
    data_ref?.strip_prefix("data:")?.split_once(";base64,")
}

/// Stand-in text written by GenAI (`Gemini`, `Qwen`) emitters when an image is not an inline
/// `data:...;base64,...` payload.
pub(crate) fn genai_degraded_image_text(media_type: Option<&str>, data_ref: Option<&str>) -> String {
    match data_ref
        .filter(|s| !s.is_empty())
        .or_else(|| media_type.filter(|s| !s.is_empty()))
    {
        Some(label) => format!("[image: {label}]"),
        None => "[image]".to_owned(),
    }
}

/// Label inside `[file: <label>]` when a target degrades [`crate::ir::Block::File`] to text.
pub(crate) fn degraded_file_label<'a>(path: Option<&'a str>, source: Option<&'a str>) -> &'a str {
    path.or(source).unwrap_or("?")
}

/// Stand-in `[file: <label>]` text written when a target cannot hold a structured file block on
/// that turn.
pub(crate) fn degraded_file_text(path: Option<&str>, source: Option<&str>) -> String {
    format!("{FILE_PREFIX}{}{BRACKET_SUFFIX}", degraded_file_label(path, source))
}

/// Stand-in text written by OpenCode when a non-assistant image or file attachment cannot be
/// stored natively.
pub(crate) fn opencode_unavailable_attachment_text(kind: &str, reference: Option<&str>) -> String {
    format!(
        "[{kind} attachment unavailable: {}]",
        reference.unwrap_or("no embedded data")
    )
}

/// Stand-in `[tool call: <name> <canonical_json>]` text written when a target renders tool calls
/// as plain text (`LmStudio`).
pub(crate) fn format_tool_call_text(name: &str, canonical_json: &str) -> String {
    format!("{TOOL_CALL_PREFIX}{name}{TOOL_CALL_SEP}{canonical_json}{BRACKET_SUFFIX}")
}
