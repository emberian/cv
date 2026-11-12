//! In-memory ingestion of an uploaded harness file tree — no filesystem access, so it works on
//! wasm32 (the browser app). Given `(relative_path, bytes)` pairs, sniff each file's harness by
//! content and parse it into IR [`Session`]s.
//!
//! OWNED BY the wasm-ingest work. `cv-core::lib` declares this module.

use crate::harness::store::document::range::{RangeSource, Validation};
use crate::harness::{antigravity, claude, codex, gemini};
use crate::ir::Session;
use crate::json::JsonView;

#[cfg(feature = "zip-ingest")]
pub mod archive;

/// Strict borrowed shape inspection: ignored uploaded bodies never become a second Value tree.
fn inspect<T>(text: &str, f: impl FnOnce(JsonView<'_>) -> T) -> Option<T> {
    let source = RangeSource::text(None, text, 0, false);
    let value = source.accept(source.range(), Validation::Value).ok()??;
    Some(f(JsonView::of(&source, value)))
}
/// The fields of an object view; a nonobject (or a read failure) has none.
struct Fields<'a>(JsonView<'a>);
fn fields(view: JsonView<'_>) -> Fields<'_> {
    Fields(view)
}
impl<'a> Fields<'a> {
    fn get(&self, key: &str) -> Option<JsonView<'a>> {
        self.0.get(key).ok().flatten()
    }
    fn string(&self, key: &str) -> Option<String> {
        let value = self.get(key)?;
        value
            .is_string()
            .ok()?
            .then(|| value.resolve().ok())??
            .as_str()
            .map(str::to_owned)
    }
}
/// An array view's items, in order; a nonarray has none.
fn items<'a>(view: JsonView<'a>) -> impl Iterator<Item = JsonView<'a>> {
    view.array().ok().flatten().into_iter().flatten().filter_map(Result::ok)
}

/// Which single-file harness a given file looks like.
enum Sniff {
    Codex { is_jsonl: bool },
    Claude,
    Gemini,
    Antigravity,
    Unknown,
}

/// The file stem (basename without extension), used as a fallback session id.
fn file_stem(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem,
        _ => base,
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Sniff the harness from filename + content. More robust than relying on the path layout, since an
/// uploaded zip may have been re-rooted or renamed.
fn sniff(name: &str, text: &str) -> Sniff {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    let is_jsonl = lower.ends_with(".jsonl");
    if let Some(first) = first_non_empty_line(text) {
        if first.starts_with('[') {
            if inspect(text, |raw| {
                items(raw).take(8).any(|item| {
                    let item = fields(item);
                    (item.get("sessionId").is_some()
                        && item.get("messageId").is_some()
                        && item.get("message").is_some())
                        || (item.get("role").is_some() && item.get("parts").is_some())
                })
            })
            .unwrap_or(false)
            {
                return Sniff::Gemini;
            }
        } else if first.starts_with('{') {
            if inspect(text, |raw| {
                let record = fields(raw);
                (record.get("messages").is_some()
                    && (record.get("sessionId").is_some() || record.get("projectHash").is_some()))
                    || record.get("history").into_iter().flat_map(items).take(8).any(|item| {
                        let item = fields(item);
                        item.get("role").is_some() && item.get("parts").is_some()
                    })
            })
            .unwrap_or(false)
            {
                return Sniff::Gemini;
            }
            if is_jsonl {
                if inspect(first, |raw| {
                    let v = fields(raw);
                    v.get("projectHash").is_some() || v.get("messages").is_some()
                })
                .unwrap_or(false)
                {
                    return Sniff::Gemini;
                }
                if text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(8)
                    .any(|line| {
                        inspect(line, |raw| {
                            let v = fields(raw);
                            v.get("$set").is_some()
                                || v.get("$rewindTo").is_some()
                                || v.get("projectHash").is_some()
                                || v.get("thoughts").is_some()
                                || v.get("toolCalls").is_some()
                                || (v.get("parts").is_some() && v.get("parentUuid").is_none())
                        })
                        .unwrap_or(false)
                    })
                {
                    return Sniff::Gemini;
                }
            }
        }
    }
    if lower.starts_with("rollout-") {
        return Sniff::Codex { is_jsonl };
    }
    let in_chunks = name.split(['/', '\\']).any(|seg| seg == "chunks");
    if !in_chunks {
        if let Some(first) = first_non_empty_line(text) {
            if first.starts_with('{') && inspect(first, antigravity::is_step_record_view).unwrap_or(false) {
                return Sniff::Antigravity;
            }
        }
    }
    if let Some(first) = first_non_empty_line(text) {
        if first.contains("\"session_meta\"") {
            return Sniff::Codex { is_jsonl };
        }
        if inspect(first, |raw| {
            let v = fields(raw);
            v.get("type").is_none() && v.get("id").is_some() && v.get("timestamp").is_some()
        })
        .unwrap_or(false)
            && (text.contains("\"response_item\"") || text.contains("\"event_msg\""))
        {
            return Sniff::Codex { is_jsonl };
        }
    }
    let mut inspected = 0;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()).take(64) {
        if inspected >= 8 {
            break;
        }
        if let Some((ty, has_thread)) = inspect(line, |raw| {
            let r = fields(raw);
            (
                r.string("type").unwrap_or_default(),
                r.get("parentUuid").is_some() || r.get("sessionId").is_some(),
            )
        }) {
            // A record the Claude codec knows is no turn says nothing either way.
            if crate::harness::claude::record::META_TYPES.contains(&ty.as_str()) {
                continue;
            }
            if has_thread && matches!(ty.as_str(), "user" | "assistant") {
                return Sniff::Claude;
            }
        }
        inspected += 1;
    }
    Sniff::Unknown
}

/// Pull a session id out of a Codex transcript's meta/header if present, else fall back to stem.
fn codex_id(name: &str, text: &str) -> String {
    if let Some(first) = first_non_empty_line(text) {
        if let Some(id) = inspect(first, |raw| {
            let record = fields(raw);
            record
                .get("payload")
                .map(fields)
                .and_then(|p| p.string("id"))
                .or_else(|| {
                    if record.get("type").is_none() {
                        record.string("id")
                    } else {
                        None
                    }
                })
        })
        .flatten()
        {
            return id;
        }
    }
    file_stem(name).to_owned()
}

/// Recover `<uuid>` from `…/brain/<uuid>/…` when present, else fall back to filename stem.
fn antigravity_id_and_rank(name: &str) -> (String, u8) {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let base = parts.last().copied().unwrap_or(name);
    let rank = match base {
        "transcript_full.jsonl" => 3,
        "transcript.jsonl" => 2,
        "overview.txt" => 1,
        _ => 0,
    };
    if let Some(pos) = parts.iter().position(|&seg| seg == "brain") {
        if let Some(&uuid) = parts.get(pos + 1) {
            if !uuid.is_empty() && rank > 0 {
                return (uuid.to_owned(), rank);
            }
        }
    }
    (file_stem(name).to_owned(), rank)
}

/// Ingest a set of in-memory files (e.g. the contents of an uploaded zip) into sessions.
///
/// Each file's bytes are decoded as UTF-8 (lossy) and its harness sniffed by content. Files that
/// can't be classified are skipped. Works without any filesystem access (wasm-safe).
pub fn ingest_files(files: Vec<(String, Vec<u8>)>) -> anyhow::Result<Vec<Session>> {
    let mut out = Vec::new();
    let mut agy_candidates: std::collections::BTreeMap<String, (u8, usize, String)> = std::collections::BTreeMap::new();
    for (idx, (name, bytes)) in files.into_iter().enumerate() {
        let text = String::from_utf8_lossy(&bytes);
        match sniff(&name, &text) {
            Sniff::Codex { is_jsonl } => {
                let id = codex_id(&name, &text);
                out.push(codex::parse_str(&id, &text, is_jsonl, None)?);
            }
            Sniff::Claude => {
                let id = file_stem(&name).to_string();
                out.push(claude::parse_str(&id, &text, None));
            }
            Sniff::Gemini => {
                out.extend(gemini::parse_all_str(&text, None)?);
            }
            Sniff::Antigravity => {
                let (id, rank) = antigravity_id_and_rank(&name);
                let replace = agy_candidates
                    .get(&id)
                    .is_none_or(|(prev_rank, _, _)| rank > *prev_rank);
                if replace {
                    agy_candidates.insert(id, (rank, idx, text.into_owned()));
                }
            }
            Sniff::Unknown => {}
        }
    }
    let mut agy_ordered: Vec<(usize, String, String)> = agy_candidates
        .into_iter()
        .map(|(id, (_rank, idx, text))| (idx, id, text))
        .collect();
    agy_ordered.sort_by_key(|(idx, _, _)| *idx);
    for (_, id, text) in agy_ordered {
        out.push(antigravity::parse_str(&id, &text, None)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    const CLAUDE_SAMPLE: &str = r#"{"type":"user","sessionId":"abc","parentUuid":null,"uuid":"u1","cwd":"/home/x","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}
{"type":"assistant","sessionId":"abc","parentUuid":"u1","uuid":"u2","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi there"}]}}"#;

    const CODEX_SAMPLE: &str = r#"{"timestamp":"2025-01-02T00:00:00Z","type":"session_meta","payload":{"id":"sess-123","cwd":"/work","timestamp":"2025-01-02T00:00:00Z"}}
{"timestamp":"2025-01-02T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"do a thing"}}
{"timestamp":"2025-01-02T00:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#;

    #[test]
    fn ingests_claude_and_codex() {
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert_eq!(sessions.len(), 2);

        let claude = sessions
            .iter()
            .find(|s| s.harness == Harness::Claude)
            .expect("claude session");
        assert_eq!(claude.id, "abc");
        assert_eq!(claude.messages.len(), 2);

        let codex = sessions
            .iter()
            .find(|s| s.harness == Harness::Codex)
            .expect("codex session");
        assert_eq!(codex.id, "sess-123");
        assert_eq!(codex.messages.len(), 2);
    }

    #[test]
    fn claude_with_many_leading_meta_records_still_sniffs() {
        // Real transcripts can open with >8 summary / file-history-snapshot records; they must not
        // exhaust the sniff window and drop the upload as Unknown.
        let mut lines: Vec<String> = (0..9)
            .map(|i| format!(r#"{{"type":"summary","summary":"topic {i}","leafUuid":"l{i}"}}"#))
            .collect();
        lines.push(r#"{"type":"file-history-snapshot","messageId":"m0","snapshot":{"trackedFileBackups":{}}}"#.into());
        // Every type the codec knows is no turn is skipped, not just the ones sniffing once named.
        lines.extend((0..9).map(|i| format!(r#"{{"type":"custom-title","customTitle":"t{i}"}}"#)));
        lines.push(r#"{"type":"cost-state","totalCostUsd":0}"#.into());
        lines.push(CLAUDE_SAMPLE.to_string());
        let text = lines.join("\n");

        let sessions = ingest_files(vec![("abc.jsonl".to_string(), text.into_bytes())]).unwrap();
        assert_eq!(sessions.len(), 1, "meta-heavy claude transcript should ingest");
        assert_eq!(sessions[0].harness, Harness::Claude);
        assert_eq!(sessions[0].messages.len(), 2);
    }

    #[test]
    fn skips_unclassifiable() {
        let files = vec![("random.txt".to_string(), b"just some text".to_vec())];
        assert!(ingest_files(files).unwrap().is_empty());
    }

    // A legacy whole-file ConversationRecord object (sessionId + projectHash + messages).
    const GEMINI_LEGACY_RECORDING: &str = r#"{"sessionId":"g-legacy-1","projectHash":"deadbeef","startTime":"2025-03-01T00:00:00Z","lastUpdated":"2025-03-01T00:01:00Z","messages":[{"id":"m1","type":"user","timestamp":"2025-03-01T00:00:00Z","content":"hi gemini"},{"id":"m2","type":"gemini","timestamp":"2025-03-01T00:00:30Z","model":"gemini-3-flash","content":[{"text":"hello back"}]}]}"#;

    // A modern append-only .jsonl recording: metadata line + message records + $set + $rewindTo.
    const GEMINI_MODERN_JSONL: &str = r#"{"sessionId":"g-modern-1","projectHash":"cafef00d","startTime":"2025-03-02T00:00:00Z"}
{"id":"u1","type":"user","timestamp":"2025-03-02T00:00:00Z","content":"list files"}
{"id":"a1","type":"gemini","timestamp":"2025-03-02T00:00:10Z","model":"gemini-3","content":[{"text":"sure"}],"toolCalls":[{"id":"t1","name":"list_directory","args":{},"result":"main.py"}]}
{"$set":{"summary":"Listed the project files."}}"#;

    // A checkpoint: {history: Content[]} with {role, parts}.
    const GEMINI_CHECKPOINT: &str =
        r#"{"history":[{"role":"user","parts":[{"text":"what is 2+2"}]},{"role":"model","parts":[{"text":"4"}]}]}"#;

    #[test]
    fn ingests_gemini_legacy_recording() {
        let files = vec![(
            "session-123.json".to_string(),
            GEMINI_LEGACY_RECORDING.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "legacy recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-legacy-1");
    }

    #[test]
    fn ingests_gemini_modern_jsonl() {
        let files = vec![("session-456.jsonl".to_string(), GEMINI_MODERN_JSONL.as_bytes().to_vec())];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "modern jsonl recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-modern-1");
    }

    #[test]
    fn ingests_gemini_checkpoint() {
        let files = vec![(
            "checkpoint-test.json".to_string(),
            GEMINI_CHECKPOINT.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files).unwrap();
        assert!(!sessions.is_empty(), "checkpoint should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
    }

    #[test]
    fn gemini_shapes_dont_regress_claude_codex() {
        // Claude and Codex samples must still classify correctly alongside the gemini widening.
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert!(sessions.iter().any(|s| s.harness == Harness::Claude));
        assert!(sessions.iter().any(|s| s.harness == Harness::Codex));
        assert!(!sessions.iter().any(|s| s.harness == Harness::Gemini));
    }
    #[test]
    fn borrowed_sniff_acceptance_and_shape_windows_are_the_recorded_ones() {
        fn label(value: Sniff) -> u8 {
            match value {
                Sniff::Unknown => 0,
                Sniff::Claude => 1,
                Sniff::Gemini => 2,
                Sniff::Codex { is_jsonl: false } => 3,
                Sniff::Codex { is_jsonl: true } => 4,
                Sniff::Antigravity => 5,
            }
        }
        let body = "large escaped λ\n\"".repeat(5000);
        let cases = [
            CLAUDE_SAMPLE.to_owned(),
            CODEX_SAMPLE.to_owned(),
            GEMINI_LEGACY_RECORDING.to_owned(),
            GEMINI_MODERN_JSONL.to_owned(),
            GEMINI_CHECKPOINT.to_owned(),
            serde_json::json!({"messages":[],"sessionId":"id","ignored":body}).to_string(),
            serde_json::json!([{"role":"user","parts":[{"text":body}]}]).to_string(),
            serde_json::json!({"type":"user","sessionId":null,"message":{"content":body}}).to_string(),
            r#"{"type":"user","sessionId":null,"ignored":"\ud800"}"#.into(),
            r#"{"type":"user","ty\u0070e":"assistant","sessionId":"id","ignored":1e400}"#.into(),
            r#"{"type":"user","sessionId":null,"ignored":1e400,"ignored":0}"#.into(),
            r#"{"id":"bare","timestamp":0}
{"type":"response_item"}"#
                .into(),
            "[]".into(),
            "not JSON".into(),
        ];
        // Recorded from the retired owned-Value sniffer, which the borrowed one matched for every
        // name x case (names outermost, cases in the order above).
        #[rustfmt::skip]
        let recorded: [u8; 42] = [
            1, 3, 2, 0, 2, 2, 2, 1, 0, 0, 0, 3, 0, 0,
            1, 4, 2, 2, 2, 2, 2, 1, 0, 0, 0, 4, 0, 0,
            4, 4, 2, 2, 2, 2, 2, 4, 4, 4, 4, 4, 4, 4,
        ];
        let mut actual = Vec::new();
        for name in ["upload.json", "upload.jsonl", "rollout-renamed.jsonl"] {
            for text in &cases {
                actual.push(label(sniff(name, text)));
            }
        }
        assert_eq!(actual, recorded);
        let snapshots = (0..63)
            .map(|_| serde_json::json!({"type":"file-history-snapshot","ignored":body}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("{snapshots}\n{CLAUDE_SAMPLE}");
        // 63 meta records do not consume the 8-line window; the 64th line is the first conversational one.
        assert_eq!(label(sniff("history.jsonl", &text)), 1);
    }

    #[test]
    fn ingests_antigravity_and_prefers_full_log_over_compact_and_chunks() {
        let full = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-11T10:00:00Z","content":"full prompt"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-11T10:00:01Z","content":"full reply"}"#;
        let compact = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-11T10:00:00Z","content":"trunc","truncated_fields":["content"]}"#;
        let files = vec![
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/transcript.jsonl".into(),
                compact.as_bytes().to_vec(),
            ),
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/transcript_full.jsonl".into(),
                full.as_bytes().to_vec(),
            ),
            (
                "brain/11111111-2222-4333-8444-555555555555/.system_generated/logs/chunks/transcript_full/0.jsonl"
                    .into(),
                full.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Antigravity);
        assert_eq!(sessions[0].id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(sessions[0].messages.len(), 2);
    }
}
