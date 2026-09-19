//! Grok CLI adapter — xAI "Grok Build" CLI, `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/`.
//!
//! Each session is a *directory*. The files we read:
//! - `summary.json` — metadata (`info{id,cwd}`, timestamps, model, git, `session_kind`,
//!   `chat_format_version`, `generated_title`/`session_summary`).
//! - `chat_history.jsonl` — the canonical transcript. Per line `type` ∈
//!   `system | user | assistant | tool_result`:
//!     * `system`/`user`: `content` is a string (system) or `[{type:text,text}]` (user). User lines
//!       may carry `synthetic_reason` (e.g. `project_instructions`) for injected context.
//!     * `assistant`: `content` (string), optional `reasoning{text,encrypted,id}`, `model_id`,
//!       `model_fingerprint`, and **`tool_calls[]`** (`{id,name,arguments(JSON string)}`).
//!     * `tool_result`: `{tool_call_id, content}` — the output fed back to the model.
//! - `updates.jsonl` — an ACP-style `session/update` event stream (plus `_x.ai/session/update`).
//!   We mine `tool_call`/`tool_call_update` entries (keyed by `toolCallId`) to *enrich* tool calls
//!   with a human `title`, a `kind`, file `locations`, and a `completed`/`failed` status that lets us
//!   set `ToolResult.is_error` correctly (chat_history alone doesn't mark errors).
//! - `events.jsonl` — timing / permission lifecycle (`tool_started`, `permission_*`,
//!   `tool_completed{outcome,duration_ms}`, `turn_*`). Not needed for content; we don't ingest it.
//!
//! Historical / variant handling (sniffed by content, never by a version field):
//! - Tool data is read from `chat_history.jsonl` (canonical, always present). `updates.jsonl` is
//!   strictly additive enrichment — if it's empty or absent we still get full tool fidelity.
//! - Sessions with no tool activity (`events.jsonl` empty, short `chat_history`) parse fine.
//! - `session_kind: "subagent"` (under `.grok/worktrees/.../subagent-<id>/`) vs primary sessions
//!   (no `session_kind`, an `agent_name` instead) are both handled; the kind is stashed in `extra`.

use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Grok {
    root: Option<PathBuf>,
}

impl Grok {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".grok").join("sessions"));
        Grok {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for Grok {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_name() != "summary.json" {
                continue;
            }
            match scan(entry.path()) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", entry.path().display()),
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // r.path is the session directory. `summary.json` (small) carries all session-level
        // metadata; the transcript is `chat_history.jsonl`, one record per line — streamed.
        let dir = &r.path;
        let summary: Value = fs::read_to_string(dir.join("summary.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Grok,
            cwd: summary
                .pointer("/info/cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| r.cwd.clone()),
            title: r.title.clone(),
            created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
            updated_at: summary
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(parse_ts)
                .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
            model: summary
                .get("current_model_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            git: grok_git(&summary),
            messages: Vec::new(),
            source_path: Some(dir.clone()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // Sidecar enrichment: tool_call / tool_call_update entries from the ACP update stream.
        let enrich = read_update_enrichment(dir);

        // All session metadata comes from summary.json (already set above), so hand it to the sink
        // before the body — header-rendering consumers (cv show) get the title/model/cwd up front.
        sink.meta(&s);

        let file = fs::File::open(dir.join("chat_history.jsonl"))
            .with_context(|| format!("reading chat_history in {}", dir.display()))?;
        let skipped = super::for_each_json_line(BufReader::new(file), |v| match chat_message(&v, &enrich) {
            Some(m) => sink.message(m),
            None => Flow::Continue,
        });
        super::note_skipped_lines(&mut s, skipped);
        Ok(s)
    }
}

fn grok_git(summary: &Value) -> Option<GitInfo> {
    let branch = summary.get("head_branch").and_then(Value::as_str);
    let commit = summary.get("head_commit").and_then(Value::as_str);
    let remote = summary
        .get("git_remotes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    if branch.is_none() && commit.is_none() && remote.is_none() {
        return None;
    }
    Some(GitInfo {
        branch: branch.map(str::to_string),
        commit: commit.map(str::to_string),
        remote: remote.map(str::to_string),
    })
}

/// Enrichment for one tool call, mined from `updates.jsonl`.
#[derive(Default, Clone)]
struct ToolEnrich {
    /// Human-friendly title, e.g. `List \`.\`` (last `tool_call_update.title` wins).
    title: Option<String>,
    /// ACP `kind`, e.g. `read`/`edit`/`execute`/`search`/`other`.
    kind: Option<String>,
    /// File paths the call touched.
    locations: Vec<String>,
    /// Terminal status: `completed` / `failed` (drives `ToolResult.is_error`).
    status: Option<String>,
}

impl ToolEnrich {
    fn is_error(&self) -> bool {
        matches!(self.status.as_deref(), Some("failed") | Some("error"))
    }
}

/// Parse `updates.jsonl` into per-`toolCallId` enrichment. Tolerant: missing/empty/malformed file
/// yields an empty map and zero behavior change.
fn read_update_enrichment(dir: &Path) -> HashMap<String, ToolEnrich> {
    let mut map: HashMap<String, ToolEnrich> = HashMap::new();
    let Ok(text) = fs::read_to_string(dir.join("updates.jsonl")) else {
        return map;
    };
    super::for_each_json_line_str(&text, |v| {
        let Some(u) = v.pointer("/params/update") else {
            return Flow::Continue;
        };
        let su = u.get("sessionUpdate").and_then(Value::as_str);
        if !matches!(su, Some("tool_call") | Some("tool_call_update")) {
            return Flow::Continue;
        }
        let Some(id) = u.get("toolCallId").and_then(Value::as_str) else {
            return Flow::Continue;
        };
        let e = map.entry(id.to_string()).or_default();
        // Prefer the descriptive title from tool_call_update over the bare tool name in tool_call.
        if let Some(t) = u.get("title").and_then(Value::as_str) {
            if su == Some("tool_call_update") || e.title.is_none() {
                e.title = Some(t.to_string());
            }
        }
        if let Some(k) = u.get("kind").and_then(Value::as_str) {
            e.kind = Some(k.to_string());
        }
        if let Some(locs) = u.get("locations").and_then(Value::as_array) {
            for l in locs {
                if let Some(p) = l.get("path").and_then(Value::as_str) {
                    if !e.locations.iter().any(|x| x == p) {
                        e.locations.push(p.to_string());
                    }
                }
            }
        }
        if let Some(st) = u.get("status").and_then(Value::as_str) {
            e.status = Some(st.to_string());
        }
        Flow::Continue
    });
    map
}

fn chat_message(v: &Value, enrich: &HashMap<String, ToolEnrich>) -> Option<Message> {
    let ty = v.get("type").and_then(Value::as_str)?;

    // tool_result lines become a distinct Tool-role message so conversions re-encode them correctly.
    if ty == "tool_result" {
        let tool_use_id = v
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let content = coerce_content(v.get("content"));
        let e = enrich.get(&tool_use_id);
        let is_error = e.map(|e| e.is_error()).unwrap_or(false);
        let status = e.and_then(|e| e.status.clone());
        let tool_name = e.and_then(|e| e.title.clone());
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id,
            content: content.into(),
            is_error,
            tool_name,
            status,
            details: None,
        });
        return Some(m);
    }

    let role = match ty {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let mut m = Message::new(role);

    // assistant lines may carry reasoning (summarized text + opaque encrypted blob).
    if let Some(reasoning) = v.get("reasoning") {
        let text = reasoning.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let encrypted = reasoning.get("encrypted").and_then(Value::as_str).map(str::to_string);
        if !text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: text.into(),
                signature: None,
                encrypted,
                redacted: false,
            });
        }
    }
    if let Some(model) = v.get("model_id").and_then(Value::as_str) {
        m.model = Some(model.to_string());
    }

    let text = coerce_content(v.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }

    // assistant tool_calls → ToolUse blocks (arguments is a JSON *string*).
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let id = c.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = c.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let input = parse_arguments(c.get("arguments"));
            // Stash sidecar enrichment so nothing is lost (ir.rs has no field for it yet).
            if let Some(e) = enrich.get(&id) {
                let mut info = serde_json::Map::new();
                if let Some(t) = &e.title {
                    info.insert("title".into(), Value::String(t.clone()));
                }
                if let Some(k) = &e.kind {
                    info.insert("kind".into(), Value::String(k.clone()));
                }
                if !e.locations.is_empty() {
                    info.insert(
                        "locations".into(),
                        Value::Array(e.locations.iter().cloned().map(Value::String).collect()),
                    );
                }
                if !info.is_empty() {
                    m.extra
                        .entry("tool_calls")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Some(Value::Object(obj)) = m.extra.get_mut("tool_calls") {
                        obj.insert(id.clone(), Value::Object(info));
                    }
                }
            }
            m.content.push(Block::ToolUse {
                id,
                name,
                input,
                namespace: None,
            });
        }
    }

    // Preserve a couple of high-signal scalars for round-tripping.
    if let Some(fp) = v.get("model_fingerprint").and_then(Value::as_str) {
        m.extra
            .insert("model_fingerprint".into(), Value::String(fp.to_string()));
    }
    if let Some(sr) = v.get("synthetic_reason").and_then(Value::as_str) {
        m.extra.insert("synthetic_reason".into(), Value::String(sr.to_string()));
    }

    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// Tool-call `arguments` is a JSON-encoded *string*. Parse it into structured JSON when possible,
/// otherwise keep the raw string (some tools historically stored a bare string).
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// Grok `content` is a string (system/assistant/tool_result) or `[{type:text,text}]` (user).
fn coerce_content(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `summary_path` is `.../<sid>/summary.json`; the session dir is its parent.
fn scan(summary_path: &Path) -> Result<Option<SessionRef>> {
    let dir = summary_path.parent().map(Path::to_path_buf);
    let Some(dir) = dir else { return Ok(None) };
    let text = fs::read_to_string(summary_path)?;
    let summary: Value = serde_json::from_str(&text)?;

    let id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_default();

    let cwd = summary.pointer("/info/cwd").and_then(Value::as_str).map(PathBuf::from);

    // title: explicit summary/generated title, else first user message from chat_history.
    let mut title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            summary
                .get("generated_title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(|s| crate::ir::truncate(s, 80));
    if title.is_none() {
        if let Ok(chat) = fs::read_to_string(dir.join("chat_history.jsonl")) {
            super::for_each_json_line_str(&chat, |v| {
                if v.get("type").and_then(Value::as_str) == Some("user") {
                    let t = coerce_content(v.get("content"));
                    if !t.trim().is_empty() {
                        title = Some(crate::ir::truncate(&t, 80));
                        return Flow::Stop;
                    }
                }
                Flow::Continue
            });
        }
    }

    let message_count = summary
        .get("num_chat_messages")
        .and_then(Value::as_u64)
        .or_else(|| summary.get("num_messages").and_then(Value::as_u64))
        .unwrap_or(0) as usize;

    Ok(Some(SessionRef {
        id,
        harness: Harness::Grok,
        path: dir,
        cwd,
        title,
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
        message_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrich_from(s: &str) -> HashMap<String, ToolEnrich> {
        let dir = tempdir_with("updates.jsonl", s);
        let e = read_update_enrichment(&dir);
        let _ = fs::remove_dir_all(&dir);
        e
    }

    /// Write `name`->`contents` into a fresh temp dir, return the dir.
    fn tempdir_with(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-grok-test-{}-{}",
            std::process::id(),
            // cheap unique-ish suffix
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }

    #[test]
    fn assistant_tool_calls_become_tooluse_blocks() {
        let line = r#"{"type":"assistant","content":"","model_id":"grok-build","model_fingerprint":"fp_x","reasoning":{"text":"thinking","encrypted":"BLOB","id":"r1"},"tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"a.rs\",\"limit\":10}"}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("assistant message");
        assert_eq!(m.role, Role::Assistant);
        // thinking + tool_use (content empty so no text block)
        assert!(matches!(m.content[0], Block::Thinking { .. }));
        match &m.content[1] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(input["target_file"], "a.rs");
                assert_eq!(input["limit"], 10);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(m.extra["model_fingerprint"], "fp_x");
        // encrypted reasoning preserved
        match &m.content[0] {
            Block::Thinking { encrypted, text, .. } => {
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(text, "thinking");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let line = r#"{"type":"tool_result","tool_call_id":"call-1","content":"file contents"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("tool message");
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn enrichment_marks_failed_results_as_error_and_adds_metadata() {
        let updates = concat!(
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-9","title":"grep","rawInput":{}}}}"#,
            "\n",
            r#"{"timestamp":2,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","kind":"search","title":"Search foo","locations":[{"path":"src/lib.rs"}]}}}"#,
            "\n",
            r#"{"timestamp":3,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","status":"failed"}}}"#,
        );
        let enrich = enrich_from(updates);
        let e = enrich.get("call-9").expect("enrichment present");
        assert_eq!(e.title.as_deref(), Some("Search foo"));
        assert_eq!(e.kind.as_deref(), Some("search"));
        assert_eq!(e.locations, vec!["src/lib.rs".to_string()]);
        assert!(e.is_error());

        // tool_result with this id should be flagged as error and carry metadata via the call.
        let res: Value =
            serde_json::from_str(r#"{"type":"tool_result","tool_call_id":"call-9","content":"boom"}"#).unwrap();
        let m = chat_message(&res, &enrich).unwrap();
        match &m.content[0] {
            Block::ToolResult { is_error, .. } => assert!(*is_error),
            _ => unreachable!(),
        }

        // and the ToolUse extra picks up title/kind/locations.
        let call: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-9","name":"grep","arguments":"{}"}]}"#,
        )
        .unwrap();
        let m = chat_message(&call, &enrich).unwrap();
        let tc = &m.extra["tool_calls"]["call-9"];
        assert_eq!(tc["title"], "Search foo");
        assert_eq!(tc["kind"], "search");
    }

    #[test]
    fn arguments_falls_back_to_raw_string_when_not_json() {
        assert_eq!(
            parse_arguments(Some(&Value::String("not json".into()))),
            Value::String("not json".into())
        );
        assert_eq!(parse_arguments(None), Value::Null);
    }

    #[test]
    fn user_synthetic_reason_preserved() {
        let line =
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"synthetic_reason":"project_instructions"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hi"));
        assert_eq!(m.extra["synthetic_reason"], "project_instructions");
    }

    #[test]
    fn empty_updates_is_harmless() {
        assert!(read_update_enrichment(Path::new("/nonexistent/dir/xyz")).is_empty());
    }

    #[test]
    fn parses_fixture_session_v1() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/v1_tools");
        if !dir.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&dir.join("summary.json")).unwrap().unwrap();
        let grok = Grok { root: None };
        let s = grok.parse(&r).unwrap();
        // roles present: system, user, assistant(with tool_use), tool(result)
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        // call-2 (write_file) is marked `failed` in updates.jsonl -> is_error must be true;
        // call-1 (read_file) is `completed` -> false.
        let mut saw_call2 = false;
        for m in s.messages.iter().filter(|m| m.role == Role::Tool) {
            if let Block::ToolResult {
                tool_use_id, is_error, ..
            } = &m.content[0]
            {
                match tool_use_id.as_str() {
                    "call-1" => assert!(!*is_error),
                    "call-2" => {
                        assert!(*is_error, "fixture marks call-2 failed");
                        saw_call2 = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_call2, "expected call-2 tool result");
        assert_eq!(s.title.as_deref(), Some("Add a greeting function"));
    }
}
