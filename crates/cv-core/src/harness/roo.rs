//! Roo Code adapter — a Cline fork with the **same** per-task file format.
//!
//! Roo stores tasks under a different VS Code extension namespace
//! (`rooveterinaryinc.roo-cline` and the newer `rooveterinaryinc.roo-code`) plus a plain `~/.roo/`.
//! The on-disk layout inside each `tasks/<taskId>/` is identical to Cline's
//! (`api_conversation_history.json` array of Anthropic messages + `ui_messages.json` +
//! `task_metadata.json`), so all real *parse* logic lives in [`super::cline`] as `pub` helpers and we
//! just point them at Roo's roots and tag the output `Harness::Roo`.
//!
//! # Emit (conversion target)
//!
//! [`emit`] is the faithful inverse of that parser. It writes a `tasks/<taskId>/` dir under
//! `out_dir` containing:
//!
//! - `api_conversation_history.json` — a JSON **array** of Anthropic message objects, the only file
//!   the parser strictly requires. Each IR message maps to one `{role, content}` object whose
//!   `content` is an array of Anthropic content blocks (`text` / `thinking` / `tool_use` /
//!   `tool_result` / `image`), exactly the shapes [`super::cline`]'s `parse_block` reads back.
//! - `task_metadata.json` — a small sidecar carrying a `cwd` hint (the parser falls back to it when
//!   the first user turn lacks an `<environment_details>` cwd line).
//!
//! Roo, like Cline, has **no standalone system turn** in `api_conversation_history.json`; a
//! `Role::System` IR message is emitted as a `system`-role object (which the parser does map back to
//! [`Role::System`]) so it survives the round-trip. A `Role::Tool` turn is written as a `user`-role
//! object whose content is only `tool_result` blocks — the parser reclassifies exactly that shape
//! back to [`Role::Tool`].
//!
//! The cwd is encoded into the first user turn's text as Cline/Roo do natively
//! (`<task>…</task>` + `<environment_details># Current Working Directory (/abs/path) Files`), so the
//! parser recovers both `cwd` and `title` from the transcript itself.

use super::cline::{discover_in, stream_task_dir, task_roots, ROO_EXT_IDS};
use super::Adapter;
use crate::ir::*;
use crate::stream::{MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Roo {
    roots: Vec<PathBuf>,
}

impl Roo {
    pub fn new() -> Self {
        Roo {
            roots: task_roots(ROO_EXT_IDS, ".roo"),
        }
    }
}

impl Default for Roo {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Roo {
    fn harness(&self) -> Harness {
        Harness::Roo
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_in(&self.roots, Harness::Roo))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        stream_task_dir(&r.path, &r.id, Harness::Roo, opts, sink)
    }
}

// ---------------------------------------------------------------------------
// Emit
// ---------------------------------------------------------------------------

/// Emit `session` into Roo's native per-task layout under `out_dir`. Writes
/// `out_dir/tasks/<taskId>/api_conversation_history.json` (+ a `task_metadata.json` cwd hint), the
/// faithful inverse of this module's parser. Honors `opts.new_cwd` (fallback `session.cwd`) and
/// `opts.new_id` (the taskId; fallback a generated millis-epoch / uuid id).
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| generate_task_id(session));
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let task_dir = out_dir.join("tasks").join(&new_id);
    fs::create_dir_all(&task_dir).with_context(|| format!("creating {}", task_dir.display()))?;

    // Build the Anthropic message array.
    let mut messages: Vec<Value> = Vec::new();
    let mut first_user_seen = false;
    for msg in &session.messages {
        let obj = match msg.role {
            Role::User => {
                // The first user turn carries the cwd/title the way Roo natively encodes it, so the
                // parser recovers both straight out of the transcript.
                let inject_env = !first_user_seen;
                first_user_seen = true;
                let blocks = roo_user_blocks(&msg.content, inject_env, session.title.as_deref(), &cwd_str);
                json!({ "role": "user", "content": Value::Array(blocks) })
            }
            Role::Assistant => {
                json!({ "role": "assistant", "content": Value::Array(roo_assistant_blocks(&msg.content)) })
            }
            Role::Tool => {
                // A user turn whose content is *only* tool_result blocks: the parser reclassifies
                // exactly this shape back to Role::Tool.
                json!({ "role": "user", "content": Value::Array(roo_tool_result_blocks(&msg.content)) })
            }
            Role::System => {
                // Roo's history has no native system turn, but the parser maps a "system" role back
                // to Role::System, so emit it to keep the round-trip faithful.
                json!({ "role": "system", "content": Value::Array(roo_assistant_blocks(&msg.content)) })
            }
        };
        messages.push(obj);
    }

    let history_path = task_dir.join("api_conversation_history.json");
    fs::write(&history_path, serde_json::to_string_pretty(&Value::Array(messages))?)
        .with_context(|| format!("writing {}", history_path.display()))?;

    // task_metadata.json — a cwd hint sidecar (the parser falls back to it when the first user turn
    // lacked an <environment_details> cwd line).
    if !cwd_str.is_empty() {
        let mut meta = Map::new();
        meta.insert("cwd".into(), json!(cwd_str));
        let meta_path = task_dir.join("task_metadata.json");
        fs::write(&meta_path, serde_json::to_string_pretty(&Value::Object(meta))?)
            .with_context(|| format!("writing {}", meta_path.display()))?;
    }

    let resume = match &cwd {
        Some(c) => format!("roo: open task {new_id}  (cwd {})", c.display()),
        None => format!("roo: open task {new_id}"),
    };
    // Return the task DIR (not the history file): the adapter's `parse` keys off the task dir and
    // re-appends `api_conversation_history.json` itself.
    let _ = history_path;
    Ok(crate::harness::EmitResult {
        path: task_dir,
        new_id,
        resume_hint: Some(resume),
    })
}

/// Pick a taskId. Roo/Cline taskIds are millisecond-epoch strings (the parser reads `created_at`
/// from them), so prefer a millis stamp from the session's timestamps; fall back to a uuid.
fn generate_task_id(session: &crate::ir::Session) -> String {
    if let Some(t) = session.created_at.or(session.updated_at) {
        let ms = t.timestamp_millis();
        if ms > 0 {
            return ms.to_string();
        }
    }
    uuid::Uuid::new_v4().to_string()
}

/// Map an IR user message's blocks → Anthropic content blocks. When `inject_env` is set, prepend a
/// `<task>` + `<environment_details>` text block (Roo's native cwd/title carrier) if the message
/// doesn't already start with one.
fn roo_user_blocks(content: &[Block], inject_env: bool, title: Option<&str>, cwd_str: &str) -> Vec<Value> {
    let mut out = Vec::new();

    if inject_env && (title.is_some() || !cwd_str.is_empty()) {
        let mut env = String::new();
        if let Some(t) = title {
            if !t.trim().is_empty() {
                env.push_str("<task>\n");
                env.push_str(t);
                env.push_str("\n</task>\n");
            }
        }
        if !cwd_str.is_empty() {
            env.push_str("<environment_details>\n# Current Working Directory (");
            env.push_str(cwd_str);
            env.push_str(") Files\n</environment_details>");
        }
        if !env.is_empty() {
            out.push(json!({ "type": "text", "text": env }));
        }
    }

    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data_ref } => out.push(roo_image_block(media_type, data_ref)),
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            // tool_use/thinking don't belong on a user turn; tool_result rides on a Tool turn.
            _ => {}
        }
    }

    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

/// Map an IR assistant (or system) message's blocks → Anthropic content blocks the parser reads.
fn roo_assistant_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
            } => {
                if *redacted {
                    let mut m = Map::new();
                    m.insert("type".into(), json!("redacted_thinking"));
                    if let Some(enc) = encrypted {
                        m.insert("data".into(), json!(enc));
                    }
                    out.push(Value::Object(m));
                } else {
                    let mut m = Map::new();
                    m.insert("type".into(), json!("thinking"));
                    m.insert("thinking".into(), json!(text));
                    if let Some(sig) = signature {
                        m.insert("signature".into(), json!(sig));
                    }
                    out.push(Value::Object(m));
                }
            }
            Block::ToolUse { id, name, input, .. } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            Block::Image { media_type, data_ref } => out.push(roo_image_block(media_type, data_ref)),
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            // tool_result doesn't belong on an assistant line (emitted on the Tool turn).
            Block::ToolResult { .. } => {}
        }
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

/// Map an IR Tool turn's blocks → an array of Anthropic `tool_result` blocks (and nothing else, so
/// the parser reclassifies the enclosing user turn back to Role::Tool).
fn roo_tool_result_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } = b
        {
            out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }));
        }
    }
    out
}

/// Reconstruct an Anthropic `image` block from the IR's `media_type` + `data_ref`, mirroring the
/// three `source` shapes the parser reads (`url` / `base64` / inline).
fn roo_image_block(media_type: &Option<String>, data_ref: &Option<String>) -> Value {
    let mut source = Map::new();
    if let Some(mt) = media_type {
        source.insert("media_type".into(), json!(mt));
    }
    match data_ref.as_deref() {
        Some("base64:inline") => {
            source.insert("type".into(), json!("base64"));
        }
        Some(r) => {
            source.insert("type".into(), json!("url"));
            source.insert("url".into(), json!(r));
        }
        None => {}
    }
    json!({ "type": "image", "source": Value::Object(source) })
}

#[cfg(test)]
mod tests {
    use super::super::cline::{parse_history_str, parse_task_dir};
    use super::*;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/roo/");
        std::fs::read_to_string(format!("{path}{name}")).unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn roo_reuses_cline_parser_and_tags_harness() {
        let s = parse_history_str(
            "1717111111111",
            &fixture("api_conversation_history.json"),
            Harness::Roo,
            None,
        );
        assert_eq!(s.harness, Harness::Roo);
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(s.title.as_deref(), Some("Add a unit test"));
        // user, assistant, tool turn.
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[2].role, Role::Tool);
    }

    /// Build a small Session, emit it to a unique temp dir, re-parse with this adapter, and assert
    /// message count + roles + tool blocks survive the round-trip.
    #[test]
    fn emit_round_trips_through_parser() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        // ── build a small session ──
        let mut user = Message::new(Role::User);
        user.content = vec![Block::Text {
            text: "Please add a test.".into(),
        }];

        let mut asst = Message::new(Role::Assistant);
        asst.content = vec![
            Block::Thinking {
                text: "I should write a test.".into(),
                signature: Some("SIG==".into()),
                encrypted: None,
                redacted: false,
            },
            Block::Text { text: "On it.".into() },
            Block::ToolUse {
                id: "toolu_42".into(),
                name: "write_to_file".into(),
                input: serde_json::json!({ "path": "src/lib.rs" }),
                namespace: None,
            },
        ];

        let mut tool = Message::new(Role::Tool);
        tool.content = vec![Block::ToolResult {
            tool_use_id: "toolu_42".into(),
            content: "File written.".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        }];

        let session = Session {
            id: "src".into(),
            harness: Harness::Roo,
            cwd: Some(PathBuf::from("/home/dev/app")),
            title: Some("Add a unit test".into()),
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages: vec![user, asst, tool],
            source_path: None,
            extra: Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // ── emit to a unique temp dir (no tempfile crate) ──
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_dir = std::env::temp_dir().join(format!("cv-roo-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&out_dir);

        let result = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).expect("emit roo session");

        // emit returns the task DIR (tasks/<taskId>/), holding api_conversation_history.json.
        let task_dir = result.path.as_path();
        assert_eq!(
            task_dir.file_name().and_then(|s| s.to_str()),
            Some(result.new_id.as_str())
        );
        assert!(task_dir.join("api_conversation_history.json").exists());

        // ── re-parse with this adapter and assert structure survives ──
        let reparsed = parse_task_dir(task_dir, &result.new_id, Harness::Roo).expect("re-parse emitted roo session");

        assert_eq!(reparsed.harness, Harness::Roo);
        assert_eq!(reparsed.messages.len(), 3);
        assert_eq!(reparsed.messages[0].role, Role::User);
        assert_eq!(reparsed.messages[1].role, Role::Assistant);
        // tool_result-only user turn reclassified back to Tool.
        assert_eq!(reparsed.messages[2].role, Role::Tool);

        // cwd + title recovered from the injected <environment_details>/<task> wrappers.
        assert_eq!(reparsed.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(reparsed.title.as_deref(), Some("Add a unit test"));

        // Assistant blocks survived: thinking (with signature), text, tool_use.
        let asst = &reparsed.messages[1];
        assert!(matches!(
            &asst.content[0],
            Block::Thinking { signature: Some(sig), .. } if sig == "SIG=="
        ));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "write_to_file")));

        // Tool result survived with its id + content.
        match &reparsed.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_42");
                assert!(content.contains("File written"));
                assert!(!is_error);
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        let _ = fs::remove_dir_all(&out_dir);
    }
}
