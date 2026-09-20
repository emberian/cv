//! Qwen Code CLI adapter — Qwen Code is a fork of gemini-cli and uses the **same** on-disk
//! session/checkpoint format, just rooted at `~/.qwen` instead of `~/.gemini`:
//!
//! - `~/.qwen/tmp/<projectHash>/logs.json` — readable fallback array of
//!   `{sessionId, messageId, type, message, timestamp}` entries (many sessions per file).
//! - `~/.qwen/tmp/<projectHash>/chats/session-<ts>-<id>.json` (legacy whole-file `ConversationRecord`)
//!   or `.jsonl` (modern append-only: metadata line + message records + `$set`/`$rewindTo` controls),
//!   plus subagent recordings under `chats/<parentId>/<id>.jsonl`.
//! - `~/.qwen/tmp/<projectHash>/checkpoint-<tag>.json` — `{history: Content[]}` or a bare `Content[]`.
//!
//! Because the format is identical, this adapter delegates all parsing to
//! [`crate::harness::gemini::parse_all_str`] (and streaming to
//! [`crate::harness::gemini::stream_for`]) and then rewrites the resulting sessions'
//! `harness` to [`Harness::Qwen`]. Missing dirs degrade gracefully to empty results.

use super::Adapter;
use crate::ir::*;
use crate::stream::{MessageSink, ParseOptions};
use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Qwen {
    root: Option<PathBuf>,
}

impl Qwen {
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".qwen").join("tmp"))
            .filter(|p| p.exists());
        Qwen { root }
    }
}

impl Default for Qwen {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Qwen {
    fn harness(&self) -> Harness {
        Harness::Qwen
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        // Same per-file scan Gemini uses (shared machinery, re-tagged Qwen) — crucially including
        // the filename-derived `checkpoint-<tag>` ids, so discover ids match what `parse`/`stream`
        // produce. (The old path scanned via the filename-blind pure parser, which gave every
        // checkpoint the same generic id "checkpoint".)
        let paths: Vec<_> = WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| is_session_file(p))
            .collect();
        Ok(crate::par_flat_map(paths, |path| {
            crate::discover_cache::cached_scan_many(&path, || {
                crate::harness::gemini::scan_session_file(&path, Harness::Qwen)
            })
        }))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // The whole-session parse is "stream into a CollectSink" — the shared Gemini machinery
        // (see `stream`) handles every shape, so `parse` and `collect(stream)` cannot diverge.
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // The on-disk format is identical to Gemini's, so the streaming machinery (mmap +
        // RawValue for legacy whole-file JSON, line streaming for modern JSONL recordings) is
        // shared; it re-tags the resulting session as Qwen — the same retag `parse_qwen_str`
        // applies on the materializing path.
        crate::harness::gemini::stream_for(Harness::Qwen, r, opts, sink)
    }
}

/// Whether a file under `~/.qwen/tmp` is one of the shapes we parse: `logs.json`, a chat recording
/// (`chats/…json|jsonl`), or a `checkpoint-*.json`.
fn is_session_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if name == "logs.json" {
        return true;
    }
    if name.starts_with("checkpoint-") && name.ends_with(".json") {
        return true;
    }
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    in_chats && matches!(path.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Parse Qwen session text by delegating to the (identical) Gemini parser, then re-tag every
    /// resulting session as [`Harness::Qwen`] — the pure-text mirror of what the adapter's on-disk
    /// paths do, used here to cross-check them.
    fn parse_qwen_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
        let mut sessions = crate::harness::gemini::parse_all_str_for(Harness::Qwen, text, source_path);
        for s in &mut sessions {
            s.harness = Harness::Qwen;
        }
        sessions
    }

    fn fixture(name: &str) -> String {
        // Reuse the Gemini fixtures — Qwen shares the on-disk format.
        let path = format!("{}/tests/fixtures/gemini/{}", env!("CARGO_MANIFEST_DIR"), name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn parses_logs_json_as_qwen() {
        let text = fixture("logs.json");
        let mut sessions = parse_qwen_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.harness == Harness::Qwen));
        assert_eq!(sessions[0].id, "sess-a");
    }

    #[test]
    fn parses_legacy_chat_recording_as_qwen() {
        let text = fixture("session_legacy.json");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.harness, Harness::Qwen);
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        // The shared machinery types every turn; Qwen-specific facts say "qwen", never "gemini".
        let kinds: Vec<(Role, MessageKind, Origin)> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
            ]
        );
        let info = &s.messages[4];
        assert_eq!(info.harness_extra(Harness::Qwen).unwrap()["record_type"], "info");
        assert!(info.harness_extra(Harness::Gemini).is_none());
        for m in &s.messages {
            for k in m.extra.keys() {
                assert_eq!(k, "qwen", "flat or misfiled extra key {k:?}");
            }
        }
        assert!(s.messages.iter().all(|m| m.model.is_none()), "one model throughout");
    }

    #[test]
    fn injected_context_and_rewinds_as_qwen() {
        let text = concat!(
            r#"{"sessionId":"q1","projectHash":"h","startTime":"2026-03-01T00:00:00.000Z","lastUpdated":"2026-03-01T00:00:04.000Z","kind":"main"}"#,
            "\n",
            r#"{"id":"u1","timestamp":"2026-03-01T00:00:01.000Z","type":"user","content":"<session_context>\nhi\n</session_context>"}"#,
            "\n",
            r#"{"id":"u2","timestamp":"2026-03-01T00:00:02.000Z","type":"user","content":"<hook_context>\nok\n</hook_context>"}"#,
            "\n",
            r#"{"id":"u3","timestamp":"2026-03-01T00:00:03.000Z","type":"user","content":"do it"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:04.000Z","type":"gemini","model":"qwen3-coder","content":"done"}"#,
            "\n",
            r#"{"$rewindTo":"g1"}"#,
            "\n",
            r#"{"id":"g1","timestamp":"2026-03-01T00:00:05.000Z","type":"gemini","model":"qwen3-coder","content":"done, properly"}"#,
            "\n",
        );
        let s = &parse_qwen_str(text, None)[0];
        assert_eq!(s.harness, Harness::Qwen);
        let kinds: Vec<(MessageKind, Origin)> = s.messages.iter().map(|m| (m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (MessageKind::InjectedContext, Origin::Harness),
                (MessageKind::InjectedContext, Origin::Hook),
                (MessageKind::Prompt, Origin::Human),
                (MessageKind::Branch, Origin::Harness),
                (MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[3].harness_extra(Harness::Qwen).unwrap()["rewind_to"], "g1");
        assert_eq!(s.harness_extra(Harness::Qwen).unwrap()["kind"], "main");
        assert_eq!(s.title.as_deref(), Some("do it"));
    }

    #[test]
    fn parses_modern_jsonl_as_qwen() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Qwen);
        assert_eq!(sessions[0].id, "11112222-3333-4444-5555-666677778888");
    }

    #[test]
    fn streams_modern_jsonl_as_qwen() {
        // The native streaming path (shared Gemini machinery) must re-tag the session as Qwen and
        // emit the same messages the pure parser materializes.
        let text = fixture("session_modern.jsonl");
        let dir = std::env::temp_dir().join(format!("cv-qwen-stream-{}", std::process::id()));
        // `chats/` in the path is how the shared machinery recognizes a chat recording.
        let chats = dir.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-test.jsonl");
        fs::write(&path, &text).unwrap();

        let r = SessionRef {
            id: "11112222-3333-4444-5555-666677778888".into(),
            harness: Harness::Qwen,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let mut sink = crate::stream::CollectSink::default();
        let s = Qwen::new()
            .stream(&r, &crate::stream::ParseOptions::full(), &mut sink)
            .expect("stream");
        assert_eq!(s.harness, Harness::Qwen);

        let parsed = &parse_qwen_str(&text, None)[0];
        assert_eq!(sink.messages.len(), parsed.messages.len());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_qwen_str("not json", None).is_empty());
        assert!(parse_qwen_str("{}", None).is_empty());
    }

    #[test]
    fn checkpoint_ids_are_filename_derived_and_distinct() {
        // Two checkpoint files must discover under DISTINCT filename-derived ids, and parse must
        // return that same id. (The old discover/parse went through the filename-blind pure parser,
        // which gave every checkpoint the generic id "checkpoint" — colliding across files and
        // disagreeing with the streaming path's "checkpoint-<tag>".)
        let text = fixture("checkpoint.json");
        let dir = std::env::temp_dir().join(format!("cv-qwen-ckpt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("checkpoint-alpha.json");
        let p2 = dir.join("checkpoint-beta%20tag.json");
        fs::write(&p1, &text).unwrap();
        fs::write(&p2, &text).unwrap();

        let refs1 = crate::harness::gemini::scan_session_file(&p1, Harness::Qwen);
        let refs2 = crate::harness::gemini::scan_session_file(&p2, Harness::Qwen);
        assert_eq!(refs1.len(), 1);
        assert_eq!(refs2.len(), 1);
        assert_eq!(refs1[0].id, "checkpoint-alpha");
        assert_eq!(refs2[0].id, "checkpoint-beta tag", "percent-decoded tag");
        assert!(refs1.iter().all(|r| r.harness == Harness::Qwen));

        // parse() (= collect(stream)) agrees with the discovered id and re-tags as Qwen.
        let s = Qwen { root: None }.parse(&refs1[0]).expect("parse checkpoint");
        assert_eq!(s.id, "checkpoint-alpha");
        assert_eq!(s.harness, Harness::Qwen);
        assert!(!s.messages.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_session_file_detection() {
        assert!(is_session_file(Path::new("/x/tmp/h/logs.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/checkpoint-foo.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.jsonl")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/parent/child.jsonl")));
        assert!(!is_session_file(Path::new("/x/tmp/h/other.json")));
        assert!(!is_session_file(Path::new("/x/tmp/h/notes.txt")));
    }
}
