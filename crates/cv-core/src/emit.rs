//! port-a-sesh: emit an IR [`Session`] back into a harness's native on-disk format.
//!
//! This is the engine behind `cv convert` (cross-harness) and `cv port` (rehome a session to another
//! cwd / move local↔remote). Conversion is `parse(source) -> IR -> emit(target)`.
//!
//! Correctness bar: a session emitted for harness H must round-trip back through that harness's
//! parser (`emit(s, H) |> parse == s`, modulo fields H cannot represent). See `docs/FORMATS.md` for
//! each target's on-disk schema.
//!
//! NOTE: This module is owned by the port-a-sesh work. The free function [`emit`] is the public entry
//! point; `cv-core::lib` re-exports it and the `cv` CLI calls it.

use crate::harness::EmitResult;
use crate::ir::{Block, GitInfo, Harness, Role, Session, SessionRef};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Knobs for emitting/porting a session.
#[derive(Debug, Clone, Default)]
pub struct EmitOptions {
    /// Override the recorded working directory (rehome the session to a new project).
    pub new_cwd: Option<PathBuf>,
    /// Force a specific new session id (otherwise a fresh one is generated).
    pub new_id: Option<String>,
}

/// An emitter: writes one IR session into one harness's native on-disk format.
type EmitFn = fn(&Session, &Path, &EmitOptions) -> Result<EmitResult>;

/// The ONE emit registry: which harnesses can be written, and by what. [`emit`] dispatches through
/// it and [`supported_targets`] is derived from it, so the dispatcher and the CLI's `--to` list can
/// never drift apart. Adding a target = adding an arm here (nothing else to keep in sync).
fn emitter_for(target: Harness) -> Option<EmitFn> {
    Some(match target {
        Harness::Claude => emit_claude,
        Harness::Codex => emit_codex,
        Harness::Grok => emit_grok,
        Harness::OpenCode => emit_opencode,
        Harness::OpenClaw => emit_openclaw,
        Harness::Gemini => emit_gemini,
        #[cfg(feature = "sqlite")]
        Harness::Hermes => emit_hermes,
        // Emitters living in their own adapter modules.
        Harness::Kimi => crate::harness::kimi::emit,
        Harness::LmStudio => crate::harness::lmstudio::emit,
        Harness::Cline => crate::harness::cline::emit,
        Harness::Roo => crate::harness::roo::emit,
        Harness::Continue => crate::harness::continuedev::emit,
        // Qwen Code stores the same ConversationRecord shape Gemini does, so it reuses that emitter.
        Harness::Qwen => emit_gemini,
        // Parse-only harnesses (Cursor, Goose, desktop apps, …) aren't conversion targets.
        _ => return None,
    })
}

/// Emit `session` into `target`'s native format under `out_dir` (the target harness's storage root,
/// or any directory for a dry run). Returns where it was written + how to resume it.
pub fn emit(session: &Session, target: Harness, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    #[cfg(not(feature = "sqlite"))]
    if target == Harness::Hermes {
        anyhow::bail!("emit to hermes requires the `sqlite` feature");
    }
    let Some(f) = emitter_for(target) else {
        anyhow::bail!("emit to {target} is not supported yet");
    };
    // Guard: emitters read content via `msg.text()`/`Deref`, which PANICS on an unresolved lazy
    // [`Span`](crate::lazy::Span) — and a span that reached `serde_json::to_value` would serialize
    // as its `{source,off,len}` struct, shipping garbage into the target file. A lazily-parsed
    // session must be resolved before it hits an emitter; do it here (on a clone, since `emit`
    // takes `&Session`) so no caller can forget.
    if has_unresolved_spans(session) {
        let mut owned = session.clone();
        owned.materialize();
        return f(&owned, out_dir, opts);
    }
    f(session, out_dir, opts)
}

/// Whether any message content field is still a lazy [`Span`](crate::lazy::Span) (see [`emit`]).
fn has_unresolved_spans(session: &Session) -> bool {
    session.messages.iter().any(|m| {
        m.content.iter().any(|b| match b {
            Block::Text { text } | Block::Thinking { text, .. } => text.is_span(),
            Block::ToolResult { content, .. } => content.is_span(),
            _ => false,
        })
    })
}

/// Emit `session` into `target`, then re-parse the written output with `target`'s own adapter and
/// diff it against the input IR. Returns the [`EmitResult`] plus a list of human-readable
/// lossy-conversion warnings (empty when the round-trip is clean). The CLI prints these so a user
/// porting a session knows what (if anything) didn't survive.
///
/// This never changes what `emit` writes — it's a read-back verification pass on top of it.
pub fn emit_verified(
    session: &Session,
    target: Harness,
    out_dir: &Path,
    opts: &EmitOptions,
) -> Result<(EmitResult, Vec<String>)> {
    let result = emit(session, target, out_dir, opts)?;
    let warnings = match reparse_emitted(target, &result) {
        Ok(reparsed) => diff_lossy(session, &reparsed),
        Err(e) => vec![format!("could not verify round-trip ({e:#})")],
    };
    Ok((result, warnings))
}

/// Re-parse a just-emitted session back into the IR using `target`'s adapter, so we can diff it
/// against the source. Mirrors how each adapter's `parse` keys off a [`SessionRef`].
fn reparse_emitted(target: Harness, result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let sref = |id: String, path: PathBuf| SessionRef {
        id,
        harness: target,
        path,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };
    match target {
        Harness::Claude => {
            let id = result.new_id.clone();
            crate::harness::claude::Claude::new().parse(&sref(id, result.path.clone()))
        }
        Harness::Codex => crate::harness::codex::Codex::new().parse(&sref(String::new(), result.path.clone())),
        Harness::Grok => crate::harness::grok::Grok::new().parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::OpenClaw => {
            crate::harness::openclaw::OpenClaw::new().parse(&sref(result.new_id.clone(), result.path.clone()))
        }
        Harness::Gemini => {
            crate::harness::gemini::Gemini::new().parse(&sref(result.new_id.clone(), result.path.clone()))
        }
        Harness::OpenCode => reparse_opencode(result),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => reparse_hermes(result),
        // The emitters whose output is parseable directly from the EmitResult path.
        Harness::Kimi => crate::harness::kimi::Kimi::new().parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::LmStudio => {
            crate::harness::lmstudio::LmStudio::new().parse(&sref(result.new_id.clone(), result.path.clone()))
        }
        Harness::Cline => crate::harness::cline::Cline::new().parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Roo => crate::harness::roo::Roo::new().parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Continue => {
            crate::harness::continuedev::Continue::new().parse(&sref(result.new_id.clone(), result.path.clone()))
        }
        // Qwen reuses Gemini's emitter (same record shape) but its own parser.
        Harness::Qwen => crate::harness::qwen::Qwen::new().parse(&sref(result.new_id.clone(), result.path.clone())),
        other => anyhow::bail!("re-parse for {other} not supported"),
    }
}

/// OpenCode resolves message/part dirs relative to a storage root (normally
/// `$HOME/.local/share/opencode/storage`); the emit target dir IS that root, so re-parse with an
/// adapter rooted there explicitly. (No `HOME` mutation: `set_var` is process-global and races any
/// concurrent `getenv` — e.g. rayon discovery threads.)
fn reparse_opencode(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    // result.path is .../storage/session/<projectID>/<id>.json — walk up to the storage root.
    let storage = result
        .path
        .ancestors()
        .find(|p| p.file_name().map(|n| n == "storage").unwrap_or(false))
        .context("locating opencode storage root for re-parse")?;
    let oc = crate::harness::opencode::OpenCode::with_root(storage.to_path_buf());
    oc.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| oc.parse(&r).ok())
        })
        .context("re-discovering emitted opencode session")
}

/// Hermes re-parses straight from the emitted `state.db` (no `HERMES_HOME` mutation — see
/// [`reparse_opencode`] for why env writes are off-limits here).
#[cfg(feature = "sqlite")]
fn reparse_hermes(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let h = crate::harness::hermes::Hermes::with_db(result.path.clone());
    h.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| h.parse(&r).ok())
        })
        .context("re-discovering emitted hermes session")
}

/// Count notable content features of a session, for before/after lossiness comparison.
struct ContentStats {
    tool_uses: usize,
    tool_results: usize,
    images: usize,
    /// Thinking blocks carrying real summary/raw text (not just an encrypted blob).
    thinking_with_text: usize,
    /// Thinking blocks carrying only an encrypted blob (text empty).
    thinking_encrypted_only: usize,
    system_turns: usize,
}

fn content_stats(session: &Session) -> ContentStats {
    let mut s = ContentStats {
        tool_uses: 0,
        tool_results: 0,
        images: 0,
        thinking_with_text: 0,
        thinking_encrypted_only: 0,
        system_turns: 0,
    };
    for m in &session.messages {
        // Only content-bearing system turns count: a format-complete *carrier* (an empty-content
        // System message holding a verbatim meta record) is replayed byte-for-byte by the emitter
        // but re-parses (under the lean full() pass) into either nothing or a rendered system turn
        // — counting carriers would flag that as loss when nothing was lost.
        if m.role == Role::System && !m.content.is_empty() {
            s.system_turns += 1;
        }
        for b in &m.content {
            match b {
                Block::ToolUse { .. } => s.tool_uses += 1,
                Block::ToolResult { .. } => s.tool_results += 1,
                Block::Image { .. } => s.images += 1,
                Block::Thinking { text, .. } => {
                    if text.trim().is_empty() {
                        s.thinking_encrypted_only += 1;
                    } else {
                        s.thinking_with_text += 1;
                    }
                }
                _ => {}
            }
        }
    }
    s
}

/// Human-readable lossy-conversion warnings: what the source IR had that didn't survive the
/// emit→re-parse round-trip. An empty Vec means a clean round-trip (modulo fields the target can't
/// represent, which we don't flag because they're inherent to the format).
fn diff_lossy(input: &Session, reparsed: &Session) -> Vec<String> {
    let before = content_stats(input);
    let after = content_stats(reparsed);
    let mut warnings = Vec::new();

    if after.tool_uses < before.tool_uses {
        let n = before.tool_uses - after.tool_uses;
        warnings.push(format!("dropped {n} tool call{}", if n == 1 { "" } else { "s" }));
    }
    if after.tool_results < before.tool_results {
        let n = before.tool_results - after.tool_results;
        warnings.push(format!("dropped {n} tool result{}", if n == 1 { "" } else { "s" }));
    }
    if after.images < before.images {
        let n = before.images - after.images;
        warnings.push(format!(
            "{n} image{} dropped or reduced to a placeholder",
            if n == 1 { "" } else { "s" }
        ));
    }
    // Reasoning text that didn't come back, or came back only as an encrypted/summary blob.
    if after.thinking_with_text < before.thinking_with_text {
        let lost = before.thinking_with_text - after.thinking_with_text;
        if after.thinking_encrypted_only > before.thinking_encrypted_only {
            warnings.push(format!(
                "{lost} reasoning block{} preserved as encrypted/summary only (raw text not stored)",
                if lost == 1 { "" } else { "s" }
            ));
        } else {
            warnings.push(format!(
                "dropped {lost} reasoning block{}",
                if lost == 1 { "" } else { "s" }
            ));
        }
    }
    if after.system_turns < before.system_turns {
        let n = before.system_turns - after.system_turns;
        warnings.push(format!(
            "dropped {n} system turn{} (target has no standalone system message)",
            if n == 1 { "" } else { "s" }
        ));
    }
    warnings
}

/// Which targets [`emit`] can currently write — derived from [`emitter_for`] (the one registry),
/// so this list is correct by construction.
pub fn supported_targets() -> &'static [Harness] {
    static TARGETS: std::sync::OnceLock<Vec<Harness>> = std::sync::OnceLock::new();
    TARGETS.get_or_init(|| {
        Harness::ALL
            .iter()
            .copied()
            .filter(|&h| emitter_for(h).is_some())
            .collect()
    })
}

/// Effective cwd after applying any rehome override.
fn effective_cwd(session: &Session, opts: &EmitOptions) -> Option<PathBuf> {
    opts.new_cwd.clone().or_else(|| session.cwd.clone())
}

/// Branch name (if any) for git-flavored metadata fields.
fn branch_of(session: &Session) -> Option<String> {
    session.git.as_ref().and_then(|g| g.branch.clone())
}

// ------------------------------------------------------------------------------------------------
// Claude
// ------------------------------------------------------------------------------------------------

/// Resolve symlinks in `cwd` the way Claude Code does when it looks for sessions to `--resume`.
///
/// Claude derives a session's project dir from the *realpath* of the directory it's launched in, so
/// a session we write must be keyed off the resolved path or `claude --resume` reports "No
/// conversation found". This bites on macOS, where `/tmp` and `/var` are symlinks into `/private`:
/// porting a session whose cwd is `/tmp/foo` would write `-tmp-foo` while Claude, launched from
/// `/tmp/foo`, looks under `-private-tmp-foo`. Canonicalizing here keeps the two in lockstep and
/// also matches what Claude records in its own transcripts (it runs in the resolved dir).
///
/// Falls back to the path as-given when it can't be resolved — e.g. the target dir doesn't exist
/// yet — which preserves the prior behavior for those cases.
fn realpath_for_claude(cwd: &Path) -> PathBuf {
    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Encode a cwd into Claude's project-dir name: leading `-`, then every `/` and `.` becomes `-`.
fn claude_encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    // Strip a single leading '/' so the mandatory leading '-' isn't doubled.
    let s = s.strip_prefix('/').unwrap_or(&s);
    let mut out = String::with_capacity(s.len() + 1);
    out.push('-');
    for ch in s.chars() {
        match ch {
            '/' | '.' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

fn emit_claude(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    // Resolve symlinks so the project-dir name matches the realpath Claude resolves at resume time.
    let cwd = effective_cwd(session, opts).map(|p| realpath_for_claude(&p));
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir_name = cwd
        .as_ref()
        .map(|p| claude_encode_cwd(p))
        .unwrap_or_else(|| "-".to_string());
    let branch = branch_of(session);

    let proj_dir = out_dir.join(&dir_name);
    fs::create_dir_all(&proj_dir).with_context(|| format!("creating {}", proj_dir.display()))?;
    let file_path = proj_dir.join(format!("{new_id}.jsonl"));

    let mut lines: Vec<Value> = Vec::new();
    let ts0 = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // ai-title line (so the title round-trips) — unless a format-complete carrier already replays the
    // original ai-title record verbatim (emitting both would duplicate it).
    let has_title_carrier = session
        .messages
        .iter()
        .any(|m| carrier_record_of(m).is_some_and(|r| r.get("type").and_then(Value::as_str) == Some("ai-title")));
    if let Some(title) = &session.title {
        if !has_title_carrier {
            lines.push(json!({
                "type": "ai-title",
                "aiTitle": title,
                "sessionId": new_id,
                "cwd": cwd_str,
                "timestamp": ts0,
            }));
        }
    }

    // Thread the conversation by uuid / parentUuid.
    let mut parent: Option<String> = None;
    // Non-carrier System turns can't be replayed (see below) and are dropped — record each dropped
    // record's uuid → its own (surviving) parent, so a child that pointed at a dropped uuid is
    // re-linked over the gap instead of left with a dangling parentUuid. `resolve_parent` follows
    // chains of consecutively-dropped ancestors (bounded; parent links form a DAG).
    let mut dropped: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    fn resolve_parent(dropped: &std::collections::HashMap<String, Option<String>>, pid: String) -> Option<String> {
        let mut p = Some(pid);
        for _ in 0..=dropped.len() {
            match p.as_ref().and_then(|id| dropped.get(id)) {
                Some(next) => p = next.clone(),
                None => break,
            }
        }
        p
    }
    for msg in &session.messages {
        // Format-complete carrier (a meta/system record carried verbatim under
        // `ParseOptions::complete`): replay it byte-for-byte and skip envelope reconstruction. These
        // are non-conversational, so they don't participate in uuid/parentUuid threading. The one
        // exception to "verbatim": a record's `sessionId`/`cwd` are the session's *identity* — a
        // ported session must carry the NEW ones (Claude keys resume off them), so rewrite them to
        // match the envelope. On a pure round-trip (same id, same cwd) this is byte-identical.
        if let Some(record) = carrier_record_of(msg) {
            let mut record = record.clone();
            if let Some(obj) = record.as_object_mut() {
                if obj.contains_key("sessionId") {
                    obj.insert("sessionId".into(), json!(new_id));
                }
                if obj.contains_key("cwd") {
                    obj.insert("cwd".into(), json!(cwd_str));
                }
            }
            lines.push(record);
            continue;
        }
        // Claude transcripts have no standalone system turn; skip to avoid polluting user text. (A
        // *carrier* System message was already handled above; this is a synthetic/rendered one —
        // under `ParseOptions::full`, `system` records parse into these.) Its uuid leaves the file,
        // so remember where its children should re-link to.
        if msg.role == Role::System {
            if let Some(id) = &msg.id {
                let p = match msg.parent_id.clone() {
                    Some(pid) => resolve_parent(&dropped, pid),
                    None => parent.clone(),
                };
                dropped.insert(id.clone(), p);
            }
            continue;
        }
        // Preserve the source uuid/parentUuid/timestamp when the message carries them (a
        // format-complete round-trip), else generate fresh threading (cross-harness convert/port).
        // A source parentUuid pointing at a dropped system record re-links to its survivor.
        let uuid = msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
        let parent_uuid = match msg.parent_id.clone() {
            Some(pid) => resolve_parent(&dropped, pid),
            None => parent.clone(),
        };
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts0.clone());

        // Common envelope fields present on every threaded line.
        let mut line = Map::new();
        line.insert("uuid".into(), json!(uuid));
        line.insert("parentUuid".into(), json!(parent_uuid));
        line.insert("sessionId".into(), json!(new_id));
        line.insert("cwd".into(), json!(cwd_str));
        line.insert("timestamp".into(), json!(ts));
        if let Some(b) = &branch {
            line.insert("gitBranch".into(), json!(b));
        }

        match msg.role {
            Role::User => {
                line.insert("type".into(), json!("user"));
                // Text-only user turn → Claude's native bare-string content. Anything richer
                // (images, document/File attachments, mixed content) → the array-of-blocks form
                // Claude also writes; flattening to `msg.text()` dropped every non-text block.
                let content = if msg.content.iter().all(|b| matches!(b, Block::Text { .. })) {
                    json!(msg.text().unwrap_or_default())
                } else {
                    Value::Array(claude_user_blocks(&msg.content))
                };
                line.insert("message".into(), json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                line.insert("type".into(), json!("assistant"));
                let blocks = claude_assistant_blocks(&msg.content);
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    message.insert("model".into(), json!(model));
                }
                // Reconstruct the API `usage` block from the first-classed [`Usage`] (the parser
                // pulls it out of `message.usage`), so a complete round-trip keeps the token counts.
                // Same-harness only, so cross-harness convert output is unchanged.
                if session.harness == Harness::Claude {
                    if let Some(usage) = claude_usage(msg.usage.as_ref()) {
                        message.insert("usage".into(), usage);
                    }
                }
                message.insert("content".into(), Value::Array(blocks));
                line.insert("message".into(), Value::Object(message));
            }
            Role::Tool => {
                // Tool results ride on a `user` line whose content is tool_result blocks.
                let blocks = claude_tool_result_blocks(&msg.content);
                line.insert("type".into(), json!("user"));
                line.insert(
                    "message".into(),
                    json!({ "role": "user", "content": Value::Array(blocks) }),
                );
            }
            Role::System => unreachable!("system turns are carried/skipped above"),
        }

        // Format-complete / same-harness port: fold every captured `extra` field back onto the
        // record — top-level keys directly, `message.<k>` keys inside the `message` object — so the
        // emitted line carries the same key set + values as the source. (`toolUseResult`,
        // `requestId`, `version`, `userType`, `message.id`, `message.stop_reason`, … all ride here.)
        // Internal sidecar keys (lazy offset stamp, the carrier key) are never emitted. Only for a
        // Claude→Claude session: a foreign source's `extra` holds *that* harness's fields, which
        // aren't valid Claude record keys, so cross-harness convert keeps its prior lean output.
        if session.harness == Harness::Claude {
            apply_claude_extra(&mut line, &msg.extra);
        }

        lines.push(Value::Object(line));
        parent = Some(uuid);
    }

    write_jsonl(&file_path, &lines)?;

    let resume = match &cwd {
        Some(c) => format!("claude --resume {new_id}  (run from {})", c.display()),
        None => format!("claude --resume {new_id}"),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}

/// The verbatim meta record a format-complete carrier message stashed (an object under
/// `extra["_record"]`), or `None` for an ordinary message. See
/// [`claude::carrier_record`](crate::harness::claude::CARRIER_KEY).
fn carrier_record_of(m: &crate::ir::Message) -> Option<&Value> {
    m.extra
        .get(crate::harness::claude::CARRIER_KEY)
        .filter(|v| v.is_object())
}

/// Internal `extra` keys the IR adds for its own bookkeeping — never part of the source record, so
/// they must NOT be replayed into an emitted Claude line.
fn is_internal_extra_key(k: &str) -> bool {
    k == crate::harness::claude::CARRIER_KEY || k == crate::offsets::OFFSET_KEY
}

/// Fold a message's captured `extra` back onto an emitted Claude record (format-complete replay): a
/// `message.<k>` key is routed inside the `message` object; any other key is a top-level field. The
/// captured value is the source-of-truth for that key (it IS the original record's value), so it
/// overrides the envelope reconstruction — e.g. the original `gitBranch`/`version` win over the
/// emit-time params. Two exceptions: `sessionId` and `cwd` are the session's *identity*, and the
/// envelope (which carries the possibly-rehomed values from `EmitOptions::new_id`/`new_cwd`) must
/// stay authoritative — replaying the captured ones would stamp a ported session with its OLD
/// id/cwd, breaking `--new-cwd` porting and `claude --resume` (which keys off both). On a pure
/// round-trip the envelope equals the captured values anyway, so nothing changes there. The
/// IR-first-classed keys (uuid/parentUuid/timestamp/type and `message.content`) are never present
/// in `extra` (the parser excludes them), so they're untouched and the [`Message`]'s own fields
/// remain authoritative.
fn apply_claude_extra(line: &mut Map<String, Value>, extra: &Map<String, Value>) {
    use crate::harness::claude::MESSAGE_EXTRA_PREFIX;
    for (k, val) in extra {
        if is_internal_extra_key(k) || k == "sessionId" || k == "cwd" {
            continue;
        }
        if let Some(sub) = k.strip_prefix(MESSAGE_EXTRA_PREFIX) {
            let msg = line
                .entry("message".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(mobj) = msg.as_object_mut() {
                mobj.insert(sub.to_string(), val.clone());
            }
        } else {
            line.insert(k.clone(), val.clone());
        }
    }
}

/// Reconstruct Claude's `message.usage` object from the IR [`Usage`], reversing
/// [`claude::parse_usage`]'s field renames (`cache_read_input_tokens` ↔ `cache_read_tokens`,
/// `cache_creation_input_tokens` ↔ `cache_creation_tokens`). Emits only the fields actually present,
/// so a source that carried just `input_tokens`/`output_tokens` round-trips with the same key set.
/// `None` when there's no usage at all (an assistant turn the source had no `usage` block on).
fn claude_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    if let Some(x) = u.input_tokens {
        m.insert("input_tokens".into(), json!(x));
    }
    if let Some(x) = u.output_tokens {
        m.insert("output_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_read_tokens {
        m.insert("cache_read_input_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_creation_tokens {
        m.insert("cache_creation_input_tokens".into(), json!(x));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

fn claude_assistant_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("signature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            // Tool results don't belong on an assistant line; skip (emitted on the Tool turn).
            Block::ToolResult { .. } => {}
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::Image { media_type, data_ref } => out.push(claude_image_block(media_type, data_ref)),
        }
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

/// Reconstruct an Anthropic `image` block's `source` from our `data_ref` so the reference survives
/// a round-trip (the parser keeps a ref, not the bytes — dropping it would lose even the
/// `file:`/url pointer). Mirrors the three shapes claude.rs parses.
fn claude_image_block(media_type: &Option<String>, data_ref: &Option<String>) -> Value {
    let mut source = serde_json::Map::new();
    if let Some(mt) = media_type {
        source.insert("media_type".into(), json!(mt));
    }
    match data_ref.as_deref() {
        Some(r) if r.starts_with("file:") => {
            source.insert("type".into(), json!("file"));
            source.insert("file_id".into(), json!(&r["file:".len()..]));
        }
        Some("base64:inline") => {
            // Bytes were intentionally not retained in the IR; mark the kind.
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

/// Map user-turn IR blocks → Claude's array-of-blocks user content. Text/Image round-trip
/// natively; a [`Block::File`] becomes the `document` attachment shape the parser reads back into
/// a File; a stray ToolResult (mixed user content) is kept as a `tool_result` block. Thinking/
/// ToolUse don't occur on user turns, but map to their native shapes rather than being dropped.
fn claude_user_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data_ref } => out.push(claude_image_block(media_type, data_ref)),
            Block::File { mime, path, source } => {
                // Inverse of claude.rs's `document` parsing: title ← path, source ← url/file_id.
                let mut src = serde_json::Map::new();
                if let Some(mt) = mime {
                    src.insert("media_type".into(), json!(mt));
                }
                match source.as_deref() {
                    Some(r) if r.starts_with("file:") => {
                        src.insert("type".into(), json!("file"));
                        src.insert("file_id".into(), json!(&r["file:".len()..]));
                    }
                    Some(r) => {
                        src.insert("type".into(), json!("url"));
                        src.insert("url".into(), json!(r));
                    }
                    None => {}
                }
                let mut doc = Map::new();
                doc.insert("type".into(), json!("document"));
                if let Some(p) = path {
                    doc.insert("title".into(), json!(p));
                }
                doc.insert("source".into(), Value::Object(src));
                out.push(Value::Object(doc));
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })),
            Block::Thinking { .. } | Block::ToolUse { .. } => {
                out.extend(claude_assistant_blocks(std::slice::from_ref(b)));
            }
        }
    }
    out
}

fn claude_tool_result_blocks(content: &[Block]) -> Vec<Value> {
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

// ------------------------------------------------------------------------------------------------
// Codex
// ------------------------------------------------------------------------------------------------

fn emit_codex(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    // `session_meta.cwd` (and `turn_context.cwd`) are REQUIRED by Codex's decoder (`SessionMeta.cwd:
    // PathBuf`, no default): a meta line without one is undecodable, the thread id can't be read,
    // and resume fails with "failed to parse thread ID from rollout file". A session with no known
    // cwd (some harnesses never record one) lands in the home dir rather than nowhere.
    let cwd = effective_cwd(session, opts)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd_str = cwd.to_string_lossy().into_owned();

    let now = session.created_at.unwrap_or_else(Utc::now);
    // Path: out_dir/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl — in LOCAL time and with no
    // zone suffix, exactly as `RolloutFileName::render` writes it (`OffsetDateTime::now_local()`):
    // Codex's `RolloutFileName::parse` requires a 19-char timestamp with byte 19 == `-` before the
    // uuid, so a `…T11-04-49Z-<uuid>` name (what we used to write) is invisible to every
    // filename-based lookup (thread-id resolution, migration, `builder_from_items`) and the thread
    // only resumed while its sqlite row happened to exist.
    let local = now.with_timezone(&chrono::Local);
    let date_dir = out_dir
        .join(local.format("%Y").to_string())
        .join(local.format("%m").to_string())
        .join(local.format("%d").to_string());
    fs::create_dir_all(&date_dir).with_context(|| format!("creating {}", date_dir.display()))?;
    let stamp = local.format("%Y-%m-%dT%H-%M-%S").to_string();
    let file_path = date_dir.join(format!("rollout-{stamp}-{new_id}.jsonl"));

    let ts_str = now.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();

    // session_meta first line.
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    // `session_id` is the root-thread id (Codex backfills it from `id` on read; explicit is clearer).
    meta.insert("session_id".into(), json!(new_id));
    meta.insert("timestamp".into(), json!(ts_str));
    meta.insert("cwd".into(), json!(cwd_str));
    meta.insert("source".into(), json!("cli"));
    meta.insert("thread_source".into(), json!("user"));
    // We write the pre-paginated layout (`response_item` message + `event_msg` user/agent twins);
    // say so — Codex ≥ 0.147 keys its persistence policy and ordinal handling off `history_mode`,
    // defaulting to `legacy` when absent, and the legacy→paginated migration canonicalizes it.
    meta.insert("history_mode".into(), json!("legacy"));
    meta.insert("originator".into(), json!("clustervision"));
    meta.insert("cli_version".into(), json!(env!("CARGO_PKG_VERSION")));
    // Codex persists this field into `state_5.sqlite.threads.model_provider` when it first
    // discovers the rollout. Omitting it creates an empty provider entry which the TUI then
    // cannot resume (`Model provider `` not found`), even though `turn_context.model` is valid.
    // The Codex target uses its standard OpenAI provider; the model id remains independently
    // carried by `turn_context` below.
    meta.insert("model_provider".into(), json!("openai"));
    if let Some(g) = &session.git {
        meta.insert("git".into(), codex_git(g));
    }
    lines.push(json!({
        "type": "session_meta",
        "timestamp": ts_str,
        "payload": Value::Object(meta),
    }));

    // The model rides in a `turn_context` record (per-turn config: model/cwd/approval/sandbox) —
    // that's the ONLY place Codex's own parser (and ours, `apply_turn_context`) reads a model id
    // from. (`session_meta.model_provider` is the *provider* name, not a model id; stuffing the
    // model there was silently dropped on re-parse.)
    // `TurnContextItem` requires `cwd`, `approval_policy`, `sandbox_policy`, `model` and `summary`
    // (no serde defaults); a `{cwd, model}` record fails `decode_rollout_line` and is silently
    // skipped on resume, so the model never actually came back. `on-request` + `workspace-write`
    // are Codex's own defaults (both decode with no further fields).
    if let Some(model) = &session.model {
        lines.push(json!({
            "type": "turn_context",
            "timestamp": ts_str,
            "payload": {
                "turn_id": Uuid::now_v7().to_string(),
                "cwd": cwd_str,
                "approval_policy": "on-request",
                "sandbox_policy": { "type": "workspace-write" },
                "model": model,
                "summary": "auto",
            },
        }));
    }

    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts_str.clone());

        match msg.role {
            Role::System => {
                let text = msg.text().unwrap_or_default();
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
            }
            Role::User => {
                let text = msg.text().unwrap_or_default();
                // The `response_item` message is what Codex feeds back into the model on resume;
                // the `event_msg` is only the UI transcript event. Emit BOTH (matching real
                // rollouts) — without the response_item the resumed model has no memory of the
                // user's turns. User content uses `input_text`.
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
                lines.push(codex_event_msg(&ts, json!({ "type": "user_message", "message": text })));
            }
            Role::Assistant => {
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match b {
                        Block::Text { text } => {
                            // Same as user turns: the response_item carries the assistant text
                            // into the resumed model context; the event_msg is UI-only. Assistant
                            // content uses `output_text`.
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": text }],
                                }),
                            ));
                            lines.push(codex_event_msg(
                                &ts,
                                json!({ "type": "agent_message", "message": text }),
                            ));
                        }
                        Block::Thinking { text, encrypted, .. } => {
                            let mut p = Map::new();
                            p.insert("type".into(), json!("reasoning"));
                            // `reasoning.content` is an output-only/raw-reasoning surface for the
                            // current Codex Responses wire. Replaying even one `reasoning_text`
                            // entry makes the next request fail with `array_above_max_length`
                            // (the input schema accepts zero raw content entries). Preserve the
                            // useful text through the replayable summary and the opaque state
                            // through `encrypted_content`, matching native Codex rollouts.
                            let summary = msg
                                .extra
                                .get("reasoning_summary")
                                .and_then(Value::as_str)
                                .unwrap_or(text);
                            let summary_items = if summary.is_empty() {
                                Vec::new()
                            } else {
                                vec![json!({ "type": "summary_text", "text": summary })]
                            };
                            p.insert("summary".into(), Value::Array(summary_items));
                            if let Some(enc) = encrypted {
                                p.insert("encrypted_content".into(), json!(enc));
                            }
                            lines.push(codex_response_item(&ts, Value::Object(p)));
                        }
                        Block::ToolUse { id, name, input } => {
                            let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args,
                                }),
                            ));
                        }
                        Block::File { path, source, .. } => {
                            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                            lines.push(codex_event_msg(
                                &ts,
                                json!({
                                    "type": "agent_message",
                                    "message": format!("[file: {label}]"),
                                }),
                            ));
                        }
                        Block::ToolResult { .. } | Block::Image { .. } => {}
                    }
                }
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = b
                    {
                        // `FunctionCallOutputPayload` deserializes ONLY a string or a content-item
                        // array (and never serializes a `success` flag) — Codex itself has no
                        // persisted error bit. The object form `{content, success:false}` we used
                        // to write is undecodable: the line was dropped on resume, orphaning its
                        // `function_call`. Keep the error visible in the text instead; cv's own
                        // Codex parser recognizes the `[error] ` prefix and restores `is_error`.
                        let output = if *is_error {
                            json!(format!("[error] {content}"))
                        } else {
                            json!(content)
                        };
                        lines.push(codex_response_item(
                            &ts,
                            json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }),
                        ));
                    }
                }
            }
        }
    }

    write_jsonl(&file_path, &lines)?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("codex resume {new_id}")),
    })
}

fn codex_event_msg(ts: &str, payload: Value) -> Value {
    json!({ "type": "event_msg", "timestamp": ts, "payload": payload })
}

fn codex_response_item(ts: &str, payload: Value) -> Value {
    json!({ "type": "response_item", "timestamp": ts, "payload": payload })
}

fn codex_git(g: &GitInfo) -> Value {
    let mut m = Map::new();
    if let Some(b) = &g.branch {
        m.insert("branch".into(), json!(b));
    }
    if let Some(c) = &g.commit {
        m.insert("commit_hash".into(), json!(c));
    }
    if let Some(r) = &g.remote {
        m.insert("repository_url".into(), json!(r));
    }
    Value::Object(m)
}

// ------------------------------------------------------------------------------------------------
// Grok (best-effort)
// ------------------------------------------------------------------------------------------------

/// Percent-encode everything except unreserved chars, so `/` → `%2F` (matching Grok's dir layout).
const GROK_ENCODE: &AsciiSet = &CONTROLS.add(b'/').add(b' ').add(b'.').add(b'%').add(b':').add(b'\\');

fn emit_grok(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc_cwd = utf8_percent_encode(&cwd_str, GROK_ENCODE).to_string();

    let session_dir = out_dir.join(&enc_cwd).join(&new_id);
    fs::create_dir_all(&session_dir).with_context(|| format!("creating {}", session_dir.display()))?;

    let created = session
        .created_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // summary.json
    let mut summary = Map::new();
    summary.insert("info".into(), json!({ "id": new_id, "cwd": cwd_str }));
    summary.insert("created_at".into(), json!(created));
    summary.insert("updated_at".into(), json!(updated));
    summary.insert("last_active_at".into(), json!(updated));
    summary.insert("num_chat_messages".into(), json!(session.messages.len()));
    // Real Grok summaries carry these too; without at least `chat_format_version`
    // the loader rejects the dir with "Session does not exist". (Verified live against
    // grok 0.1.219 — adding the full field set is what makes `grok --resume` discover it.)
    let num_turns = session.messages.iter().filter(|m| matches!(m.role, Role::User)).count();
    summary.insert("num_messages".into(), json!(num_turns));
    summary.insert("next_trace_turn".into(), json!(0));
    summary.insert("chat_format_version".into(), json!(1));
    summary.insert("agent_name".into(), json!("grok-build"));
    if let Some(home) = std::env::var_os("HOME") {
        summary.insert(
            "grok_home".into(),
            json!(format!("{}/.grok", Path::new(&home).to_string_lossy())),
        );
    }
    // Carry the source model only if it's actually a Grok model; otherwise default to
    // `grok-build`. A foreign model id (e.g. `gpt-5.5`) would make Grok try to replay the
    // history against an incompatible backend.
    let model_id = session
        .model
        .as_deref()
        .filter(|m| m.to_ascii_lowercase().contains("grok"))
        .unwrap_or("grok-build");
    summary.insert("current_model_id".into(), json!(model_id));
    if let Some(g) = &session.git {
        if let Some(b) = &g.branch {
            summary.insert("head_branch".into(), json!(b));
        }
        if let Some(c) = &g.commit {
            summary.insert("head_commit".into(), json!(c));
        }
        if let Some(r) = &g.remote {
            summary.insert("git_remotes".into(), json!([r]));
        }
    }
    summary.insert("session_summary".into(), json!(session.title.as_deref().unwrap_or("")));
    let summary_path = session_dir.join("summary.json");
    fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    // chat_history.jsonl, plus an updates.jsonl sidecar carrying each tool call's terminal status.
    // chat_history alone cannot express a failed tool result — the Grok parser derives
    // `ToolResult.is_error`/`status` ONLY from `updates.jsonl` `tool_call_update` entries — so
    // without the sidecar every ported failure came back as a success.
    let mut lines: Vec<Value> = Vec::new();
    let mut updates: Vec<Value> = Vec::new();
    for msg in &session.messages {
        match msg.role {
            Role::System => {
                let text = grok_concat_text(msg);
                lines.push(json!({ "type": "system", "content": text }));
            }
            Role::User => {
                let text = grok_concat_text(msg);
                lines.push(json!({
                    "type": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            Role::Assistant => {
                let mut line = Map::new();
                line.insert("type".into(), json!("assistant"));
                line.insert("content".into(), json!(grok_concat_text(msg)));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    line.insert("model_id".into(), json!(model));
                }
                for b in &msg.content {
                    if let Block::Thinking { text, encrypted, .. } = b {
                        let mut r = Map::new();
                        r.insert("text".into(), json!(text));
                        // `encrypted` reasoning blobs are provider/account-bound. Carrying one
                        // from a foreign harness (e.g. OpenAI's `encrypted_content`) makes Grok's
                        // backend fail with "Could not decrypt the provided encrypted_content".
                        // Only preserve it for a Grok→Grok port.
                        if let Some(enc) = encrypted {
                            if session.harness == Harness::Grok {
                                r.insert("encrypted".into(), json!(enc));
                            }
                        }
                        line.insert("reasoning".into(), Value::Object(r));
                        break;
                    }
                }
                // Emit assistant tool calls. The Grok parser reads `tool_calls[]` with
                // `arguments` as a JSON-encoded *string*, so re-encode the structured input.
                let calls: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    line.insert("tool_calls".into(), Value::Array(calls));
                }
                lines.push(Value::Object(line));
            }
            // Tool turns map to `tool_result` lines (the parser turns these back into Role::Tool).
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        status,
                        ..
                    } = b
                    {
                        lines.push(json!({
                            "type": "tool_result",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                        // ACP-style status entry: the source status verbatim when it has one,
                        // else derived from `is_error` (Grok's own terminal states).
                        let status = status
                            .clone()
                            .unwrap_or_else(|| if *is_error { "failed" } else { "completed" }.into());
                        updates.push(json!({
                            "timestamp": epoch_ms(msg.timestamp.or(session.updated_at)),
                            "method": "session/update",
                            "params": {
                                "sessionId": new_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": tool_use_id,
                                    "status": status,
                                },
                            },
                        }));
                    }
                }
            }
        }
    }
    let chat_path = session_dir.join("chat_history.jsonl");
    write_jsonl(&chat_path, &lines)?;
    if !updates.is_empty() {
        write_jsonl(&session_dir.join("updates.jsonl"), &updates)?;
    }

    Ok(EmitResult {
        path: session_dir,
        new_id: new_id.clone(),
        resume_hint: Some(format!("grok --resume {new_id}")),
    })
}

/// Concatenate a message's text blocks (Grok chat_history carries plain text only).
fn grok_concat_text(msg: &crate::ir::Message) -> String {
    let mut s = String::new();
    for b in &msg.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

// ------------------------------------------------------------------------------------------------
// OpenCode (parts generation)
// ------------------------------------------------------------------------------------------------

/// Epoch-millis for an optional timestamp, falling back to `now`.
fn epoch_ms(t: Option<DateTime<Utc>>) -> i64 {
    t.unwrap_or_else(Utc::now).timestamp_millis()
}

/// Emit into OpenCode's newer "parts" storage generation:
///   out_dir/session/<projectID>/ses_<id>.json
///   out_dir/message/<sid>/msg_<mid>.json
///   out_dir/part/<mid>/prt_<pid>.json
///
/// The OpenCode reader (see `harness/opencode.rs`) keys parts by *messageID*, orders messages by
/// `time.created` and parts by filename, and splits tool parts into a trailing Tool-role IR
/// message. We honour all of that. Tool results are folded back onto the originating assistant
/// message's `tool` part (matched by `tool_use_id`), since OpenCode has no standalone tool turn.
fn emit_opencode(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    // OpenCode migrated to a SQLite store (`opencode.db`, sibling of this `storage/` dir). Once that
    // DB exists the binary no longer reads the file-based `storage/{session,message,part}/*.json`
    // layout we write here — the JSON→DB import is one-shot, gated on the DB's absence — so a session
    // emitted into `storage/` is invisible to resume (`opencode run --session <id>` → "Session not
    // found", verified live). Refuse rather than silently produce an unresumable session. Emitting
    // INTO opencode.db is tracked as a follow-up. If the DB is absent (fresh install), the legacy JSON
    // is still imported on next launch, so we proceed normally.
    if out_dir
        .parent()
        .map(|p| p.join("opencode.db"))
        .is_some_and(|db| db.exists())
    {
        anyhow::bail!(
            "opencode has migrated to SQLite (opencode.db); it no longer reads the file-based \
             storage/ layout, so a session converted here cannot be resumed. Emitting into \
             opencode.db isn't implemented yet — see docs/PORTABILITY-REVIEW.md."
        );
    }
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| format!("ses_{}", Uuid::now_v7().simple()));
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // OpenCode shards sessions under a per-project directory. We don't reproduce its exact project
    // hashing; a deterministic dir keyed off the cwd is enough for the reader (it WalkDirs).
    let project_id = if cwd_str.is_empty() {
        "global".to_string()
    } else {
        format!("prj_{}", short_hash(&cwd_str))
    };

    let session_dir = out_dir.join("session").join(&project_id);
    let msg_dir = out_dir.join("message").join(&new_id);
    fs::create_dir_all(&session_dir).with_context(|| format!("creating {}", session_dir.display()))?;
    fs::create_dir_all(&msg_dir).with_context(|| format!("creating {}", msg_dir.display()))?;

    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));

    // ── session metadata file ──
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    if !cwd_str.is_empty() {
        meta.insert("directory".into(), json!(cwd_str));
    }
    if let Some(t) = &session.title {
        meta.insert("title".into(), json!(t));
    }
    meta.insert("time".into(), json!({ "created": created, "updated": updated }));
    let ses_path = session_dir.join(format!("{new_id}.json"));
    fs::write(&ses_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("writing {}", ses_path.display()))?;

    // Pre-index tool results (from Role::Tool turns) by tool_use_id so we can attach them to the
    // matching `tool` part on the assistant message that produced the call.
    let mut tool_results: std::collections::HashMap<String, &Block> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult { tool_use_id, .. } = b {
                    tool_results.insert(tool_use_id.clone(), b);
                }
            }
        }
    }

    let mut seq: u64 = 0;
    for msg in &session.messages {
        // Tool turns are folded into the assistant's tool parts; don't emit standalone messages.
        if msg.role == Role::Tool {
            continue;
        }
        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::System => "system",
            _ => "user",
        };
        let mid = format!("msg_{}", Uuid::now_v7().simple());
        let mts = epoch_ms(msg.timestamp.or(session.created_at));

        let mut mrec = Map::new();
        mrec.insert("id".into(), json!(mid));
        mrec.insert("sessionID".into(), json!(new_id));
        mrec.insert("role".into(), json!(role));
        mrec.insert("time".into(), json!({ "created": mts }));
        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
            mrec.insert("modelID".into(), json!(model));
        }
        let mrec_path = msg_dir.join(format!("{mid}.json"));
        fs::write(&mrec_path, serde_json::to_string_pretty(&Value::Object(mrec))?)
            .with_context(|| format!("writing {}", mrec_path.display()))?;

        let part_dir = out_dir.join("part").join(&mid);
        fs::create_dir_all(&part_dir).with_context(|| format!("creating {}", part_dir.display()))?;

        let mut pidx: u64 = 0;
        let mut write_part = |part: Value| -> Result<()> {
            // Zero-padded prefix keeps the reader's filename sort == emission order.
            let pid = format!("prt_{seq:08}_{pidx:04}_{}", Uuid::now_v7().simple());
            let p = part_dir.join(format!("{pid}.json"));
            fs::write(&p, serde_json::to_string_pretty(&part)?).with_context(|| format!("writing {}", p.display()))?;
            pidx += 1;
            Ok(())
        };

        for b in &msg.content {
            match b {
                Block::Text { text } => {
                    write_part(json!({ "type": "text", "text": text }))?;
                }
                Block::Thinking { text, signature, .. } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("reasoning"));
                    p.insert("text".into(), json!(text));
                    if let Some(sig) = signature {
                        p.insert("metadata".into(), json!({ "anthropic": { "signature": sig } }));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolUse { id, name, input } => {
                    let mut state = Map::new();
                    state.insert("input".into(), input.clone());
                    // Attach the paired result, if we have one.
                    if let Some(Block::ToolResult { content, is_error, .. }) = tool_results.get(id).copied() {
                        if *is_error {
                            state.insert("status".into(), json!("error"));
                            state.insert("error".into(), json!(content));
                        } else {
                            state.insert("status".into(), json!("completed"));
                            state.insert("output".into(), json!(content));
                        }
                    } else {
                        state.insert("status".into(), json!("completed"));
                    }
                    write_part(json!({
                        "type": "tool",
                        "callID": id,
                        "tool": name,
                        "state": Value::Object(state),
                    }))?;
                }
                Block::Image { media_type, data_ref } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = media_type {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(d) = data_ref {
                        p.insert("url".into(), json!(d));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::File { mime, path, source } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = mime {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(url) = source.as_deref().or(path.as_deref()) {
                        p.insert("url".into(), json!(url));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolResult { .. } => {}
            }
        }
        seq += 1;
    }

    let resume = format!("opencode (session {new_id})");
    Ok(EmitResult {
        path: ses_path,
        new_id,
        resume_hint: Some(resume),
    })
}

/// A short, stable, filesystem-safe hash of a string (FNV-1a, hex). Used for project-dir names.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ------------------------------------------------------------------------------------------------
// OpenClaw
// ------------------------------------------------------------------------------------------------

/// Emit into OpenClaw's `agents/<agentId>/sessions/<sid>.jsonl` transcript (v3 parent-linked) plus
/// a `sessions.json` index entry. Mirrors `harness/openclaw.rs`'s reader: a `{type:session,…}`
/// header line followed by `{type:message,id,parentId,timestamp,message:{role,content,…}}` lines.
fn emit_openclaw(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_id = "main";
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // `out_dir` is already the agents dir: storage_root() returns `~/.openclaw/agents` and discover()
    // walks it for `<agentId>/sessions/<id>.jsonl`. Re-joining "agents" here produced
    // `~/.openclaw/agents/agents/main/sessions/…`, which openclaw never scans (the session was written
    // but undiscoverable). If a caller instead points `--out` at the state dir (`~/.openclaw`), accept
    // that too by appending the missing `agents` segment.
    let agents_root = if out_dir.file_name().is_some_and(|n| n == "agents") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("agents")
    };
    let sessions_dir = agents_root.join(agent_id).join("sessions");
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));

    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();
    // Header line.
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(3));
    header.insert("id".into(), json!(new_id));
    header.insert("timestamp".into(), json!(created_iso));
    if !cwd_str.is_empty() {
        header.insert("cwd".into(), json!(cwd_str));
    }
    lines.push(Value::Object(header));

    // Message lines, parent-linked (v3).
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        let entry_id = openclaw_short_id();
        let ts = msg.timestamp.unwrap_or(created);
        let ts_iso = ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        let ts_ms = ts.timestamp_millis();

        let inner = match msg.role {
            Role::User => json!({
                "role": "user",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::System => json!({
                "role": "system",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), openclaw_content_blocks(&msg.content));
                m.insert("timestamp".into(), json!(ts_ms));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                Value::Object(m)
            }
            Role::Tool => {
                // OpenClaw models each tool result as its own `toolResult` message.
                let (tool_use_id, content, is_error, tool_name) = openclaw_tool_result(&msg.content);
                let mut m = Map::new();
                m.insert("role".into(), json!("toolResult"));
                m.insert("toolCallId".into(), json!(tool_use_id));
                if let Some(tn) = tool_name {
                    m.insert("toolName".into(), json!(tn));
                }
                m.insert("content".into(), json!([{ "type": "text", "text": content }]));
                m.insert("isError".into(), json!(is_error));
                m.insert("timestamp".into(), json!(ts_ms));
                Value::Object(m)
            }
        };

        lines.push(json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent,
            "timestamp": ts_iso,
            "message": inner,
        }));
        parent = Some(entry_id);
    }

    write_jsonl(&file_path, &lines)?;

    // sessions.json index: merge-or-create.
    let index_path = sessions_dir.join("sessions.json");
    let mut index: Map<String, Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = session.title.as_ref().or(session.first_user_text().as_ref()) {
        entry.insert("label".into(), json!(crate::ir::truncate(t, 80)));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    index.insert(new_id.clone(), Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Object(index))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        // Headless resume is the `agent` subcommand with `--session-id` + a `--message`, run
        // in-process via `--local` (verified against openclaw's CLI registration; there is no
        // `openclaw --session` flag outside the interactive TUI launcher).
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}

/// 8-hex-char id slice, matching OpenClaw's entry-id convention.
fn openclaw_short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Map IR blocks → an OpenClaw content array (`text`/`thinking`/`toolCall`/`image`).
fn openclaw_content_blocks(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("thinkingSignature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input } => out.push(json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": input,
            })),
            Block::Image { media_type, data_ref } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("image"));
                if let Some(mt) = media_type {
                    m.insert("mimeType".into(), json!(mt));
                }
                if let Some(d) = data_ref {
                    m.insert("data".into(), json!(d));
                }
                out.push(Value::Object(m));
            }
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

/// Pull the (id, content, is_error, name) out of a Tool turn's blocks.
fn openclaw_tool_result(content: &[Block]) -> (String, String, bool, Option<String>) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string(), *is_error, tool_name.clone());
        }
    }
    (String::new(), String::new(), false, None)
}

// ------------------------------------------------------------------------------------------------
// Gemini (legacy ConversationRecord)
// ------------------------------------------------------------------------------------------------

/// Emit a gemini-cli legacy chat recording: `out_dir/<sessionId>.json`, a single whole-file
/// `ConversationRecord` `{sessionId, projectHash, startTime, lastUpdated, messages[]}`. The reader
/// (`harness/gemini.rs`) requires the file to live under a `chats/` dir to be picked up by
/// `discover`, but `parse_all_str` / `parse` will read it directly from any path.
fn emit_gemini(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Gemini's `--resume` / `--list-sessions` only scan `<globalTemp>/<projectIdentifier>/chats/`,
    // where `out_dir` is `<globalTemp>` (~/.gemini/tmp) and `<projectIdentifier>` is the cwd basename
    // registered in `<globalTemp>/../projects.json`. Writing to a bare `chats/` (no identifier dir)
    // makes the session invisible to the installed binary. So register the cwd→slug mapping (matching
    // gemini's getShortId) and write into the slug's chats dir. (Verified live: with the file in the
    // right slug dir, `gemini --resume <uuid>` recalls the conversation.)
    let chats_dir = match gemini_register_project(out_dir, &cwd_str) {
        Some(slug) => out_dir.join(slug).join("chats"),
        None => out_dir.join("chats"), // no cwd → fall back to the legacy bare path
    };
    fs::create_dir_all(&chats_dir).with_context(|| format!("creating {}", chats_dir.display()))?;
    let file_path = chats_dir.join(format!("session-{new_id}.json"));

    let start = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let last = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // Index tool results by tool_use_id so they can be folded into the producing assistant's
    // `toolCalls[].result` (the canonical legacy shape the reader pairs from).
    let mut tool_results: std::collections::HashMap<String, (String, bool)> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    tool_results.insert(tool_use_id.clone(), (content.to_string(), *is_error));
                }
            }
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        let ts = msg.timestamp.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
        match msg.role {
            Role::User | Role::System => {
                let mty = if msg.role == Role::System { "info" } else { "user" };
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!(mty));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                messages.push(Value::Object(m));
            }
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!("gemini"));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Thoughts (thinking) ride in `thoughts[]`, the rest in `content`.
                let mut thoughts = Vec::new();
                for b in &msg.content {
                    if let Block::Thinking { text, .. } = b {
                        thoughts.push(json!({ "subject": "", "description": text }));
                    }
                }
                if !thoughts.is_empty() {
                    m.insert("thoughts".into(), Value::Array(thoughts));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                // Tool calls → toolCalls[] (legacy ToolCallRecord), folding in the paired result so
                // the reader emits a proper Tool turn for it.
                let mut calls = Vec::new();
                for b in &msg.content {
                    if let Block::ToolUse { id, name, input } = b {
                        let mut call = Map::new();
                        call.insert("id".into(), json!(id));
                        call.insert("name".into(), json!(name));
                        call.insert("args".into(), input.clone());
                        if let Some((content, is_error)) = tool_results.get(id) {
                            call.insert("status".into(), json!(if *is_error { "error" } else { "success" }));
                            call.insert(
                                "result".into(),
                                json!([{
                                    "functionResponse": {
                                        "id": id,
                                        "name": name,
                                        "response": { "output": content },
                                    }
                                }]),
                            );
                        }
                        calls.push(Value::Object(call));
                    }
                }
                if !calls.is_empty() {
                    m.insert("toolCalls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(m));
            }
            // Tool turns are folded into the assistant's toolCalls[].result above.
            Role::Tool => {}
        }
    }

    let mut record = Map::new();
    record.insert("sessionId".into(), json!(new_id));
    // gemini keys recordings by an opaque projectHash; synthesize a stable one from the cwd.
    // (The resume path doesn't validate this value; discovery is by the slug dir computed above.)
    record.insert("projectHash".into(), json!(short_hash(&cwd_str)));
    record.insert("startTime".into(), json!(start));
    record.insert("lastUpdated".into(), json!(last));
    if let Some(t) = &session.title {
        record.insert("summary".into(), json!(t));
    }
    if let Some(c) = &cwd {
        record.insert("directories".into(), json!([c.to_string_lossy()]));
    }
    record.insert("messages".into(), Value::Array(messages));

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(record))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(match &cwd {
            Some(c) => format!("gemini --resume {new_id}  (run from {})", c.display()),
            None => format!("gemini --resume {new_id}"),
        }),
    })
}

/// Bind `cwd` to a slug dir under gemini's `<globalTemp>` (=`out_dir`), replicating
/// `ProjectRegistry.getShortId`/`claimNewSlug`. Gemini owns a slug dir via a `.project_root` marker
/// file containing the project path; on launch it RE-derives the slug and, if our dir lacks a marker
/// (or the projects.json entry can't be verified), reassigns a fresh slug — so writing only
/// projects.json is not enough. We therefore: pick the cwd basename (suffixing `-2`, `-3`… if a
/// DIFFERENT path already owns that dir's marker), write the `.project_root` marker, and record the
/// projects.json mapping (the fast path; gemini heals from the marker regardless). After this,
/// `gemini --resume`/`--list-sessions` discover the session. Returns the slug, `None` if `cwd` empty.
fn gemini_register_project(global_tmp: &Path, cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    const MARKER: &str = ".project_root";
    // Reuse the slug whose marker already owns this cwd, if any (gemini's findExistingSlugForPath).
    let owned_by = |slug_dir: &Path| -> Option<String> {
        fs::read_to_string(slug_dir.join(MARKER))
            .ok()
            .map(|s| s.trim().to_string())
    };
    if let Ok(entries) = fs::read_dir(global_tmp) {
        for e in entries.flatten() {
            if owned_by(&e.path()).as_deref() == Some(cwd) {
                return e.file_name().to_str().map(str::to_string);
            }
        }
    }
    // Otherwise claim the basename, skipping dirs whose marker owns a different path.
    let base = Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    let mut slug = base.clone();
    let mut n = 2;
    loop {
        let dir = global_tmp.join(&slug);
        match owned_by(&dir) {
            Some(owner) if owner != cwd => {
                slug = format!("{base}-{n}");
                n += 1;
            }
            _ => break, // free, or already ours
        }
    }
    let slug_dir = global_tmp.join(&slug);
    let _ = fs::create_dir_all(&slug_dir);
    let _ = fs::write(slug_dir.join(MARKER), cwd);
    // Record the projects.json fast-path mapping too (best-effort).
    if let Some(parent) = global_tmp.parent() {
        let projects_json = parent.join("projects.json");
        let mut doc: Value = fs::read_to_string(&projects_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let projects = doc
            .as_object_mut()
            .unwrap()
            .entry("projects")
            .or_insert_with(|| json!({}));
        if let Some(m) = projects.as_object_mut() {
            m.insert(cwd.to_string(), json!(slug));
        }
        if let Ok(s) = serde_json::to_string_pretty(&doc) {
            let _ = fs::write(&projects_json, s);
        }
    }
    Some(slug)
}

/// Map IR blocks → a Gemini `Part[]` (`text` / thought `text` / `inlineData`). Tool calls are
/// emitted separately via `toolCalls[]`, so they're skipped here.
fn gemini_parts(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "text": text })),
            Block::Thinking { text, .. } => out.push(json!({ "text": text, "thought": true })),
            Block::Image { media_type, .. } => {
                let mut inline = Map::new();
                if let Some(mt) = media_type {
                    inline.insert("mimeType".into(), json!(mt));
                }
                out.push(json!({ "inlineData": Value::Object(inline) }));
            }
            Block::File { mime, path, source } => {
                let mut fd = Map::new();
                if let Some(mt) = mime {
                    fd.insert("mimeType".into(), json!(mt));
                }
                if let Some(uri) = source.as_deref().or(path.as_deref()) {
                    fd.insert("fileUri".into(), json!(uri));
                }
                out.push(json!({ "fileData": Value::Object(fd) }));
            }
            // Tool use/result handled out of band.
            Block::ToolUse { .. } | Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

// ------------------------------------------------------------------------------------------------
// Hermes (SQLite)
// ------------------------------------------------------------------------------------------------

/// Emit into Hermes's `state.db` SQLite store (creating/appending). Mirrors the schema the Hermes
/// reader (`harness/hermes.rs`) expects: a `sessions` row + OpenAI-shaped `messages` rows
/// (role/content/tool_calls JSON/reasoning/timestamps as REAL unix secs). Multimodal content uses
/// the `\x00json:` sentinel prefix.
#[cfg(feature = "sqlite")]
fn emit_hermes(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::{params, Connection};

    const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    // `out_dir` may be a directory to drop state.db into, OR the state.db file itself — the hermes
    // adapter's storage_root() returns the .db path, so `cv convert --to hermes` (no --out) passes
    // the existing file here. Treating it as a dir made `create_dir_all` fail with "File exists".
    let db_path = if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("state.db")
    };
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    // Create the (subset of) v14 schema if the DB is fresh. `IF NOT EXISTS` makes append safe.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
         );",
    )
    .context("creating hermes schema")?;

    // FTS5 virtual tables + sync triggers, mirroring hermes_state.py's FTS_SQL / FTS_TRIGRAM_SQL.
    // Without these, a port into a *fresh* state.db is invisible to Hermes's search (which queries
    // `messages_fts`). The unicode61 table is part of bundled FTS5 and always available; the trigram
    // tokenizer is too in modern SQLite, but we still guard it so emit succeeds on any build that
    // lacks it. Created BEFORE inserting rows so the AFTER INSERT triggers populate the index.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content);
         CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;",
    )
    .context("creating hermes messages_fts (FTS5)")?;

    // Trigram table for CJK/substring search. Guarded: if the trigram tokenizer isn't compiled in,
    // skip it (and its triggers) rather than failing the whole emit. The unicode61 table above is
    // enough for Hermes to find ASCII content; the trigram index is a CJK nicety.
    let trigram_sql = "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(content, tokenize='trigram');
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;";
    if let Err(e) = conn.execute_batch(trigram_sql) {
        eprintln!("cv: hermes trigram FTS unavailable, skipping ({e}); unicode61 FTS still active");
    }

    let started = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .timestamp_millis() as f64
        / 1000.0;
    let ended = session
        .updated_at
        .or(session.created_at)
        .map(|t| t.timestamp_millis() as f64 / 1000.0);

    // Format-complete: pull the captured per-session columns back out of `Session.extra` (written
    // by the Hermes parser under `SESSION_META_KEY`) so the round-trip reproduces every session-row
    // value. Anything absent falls back to a sensible default (e.g. `source = "clustervision"`).
    use crate::harness::hermes::{RAW_REASONING_CONTENT_KEY, RAW_REASONING_KEY, SESSION_META_KEY};
    let smeta = session.extra.get(SESSION_META_KEY).and_then(Value::as_object);
    let smeta_str =
        |k: &str| -> Option<String> { smeta.and_then(|m| m.get(k)).and_then(Value::as_str).map(str::to_string) };
    let smeta_int = |k: &str| -> i64 { smeta.and_then(|m| m.get(k)).and_then(Value::as_i64).unwrap_or(0) };
    let source = smeta_str("source").unwrap_or_else(|| "clustervision".to_string());
    let user_id = smeta_str("user_id");
    let model_config = smeta_str("model_config");
    let system_prompt = smeta_str("system_prompt");
    let end_reason = smeta_str("end_reason");

    conn.execute(
        "INSERT OR REPLACE INTO sessions \
         (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
          end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
          cache_read_tokens, cache_write_tokens, reasoning_tokens, title) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            new_id,
            source,
            user_id,
            session.model,
            model_config,
            system_prompt,
            started,
            ended,
            end_reason,
            session.messages.len() as i64,
            smeta_int("tool_call_count"),
            smeta_int("input_tokens"),
            smeta_int("output_tokens"),
            smeta_int("cache_read_tokens"),
            smeta_int("cache_write_tokens"),
            smeta_int("reasoning_tokens"),
            session.title,
        ],
    )
    .context("inserting hermes session")?;

    let mut base_ts = started;
    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|| {
                base_ts += 0.001;
                base_ts
            });

        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };

        // Reasoning columns. For a lossless round-trip we prefer the verbatim source columns the
        // parser stashed in `extra` (`hermes_reasoning` / `hermes_reasoning_content` /
        // `reasoning_details` / `codex_*`) over reconstructing from the folded `Thinking.text`
        // projection — that projection merges + dedups across columns and so can't recover the
        // originals. We only synthesize from Thinking blocks when no verbatim column was captured
        // (i.e. a session built in-memory or ported from another harness).
        let extra_str = |k: &str| -> Option<String> { msg.extra.get(k).and_then(Value::as_str).map(str::to_string) };
        let extra_json = |k: &str| -> Option<String> { msg.extra.get(k).map(|v| v.to_string()) };

        let mut reasoning = extra_str(RAW_REASONING_KEY);
        let reasoning_content = extra_str(RAW_REASONING_CONTENT_KEY);
        // `reasoning_details` / codex items were stashed as parsed JSON; re-serialize them verbatim.
        let mut reasoning_details = extra_json("reasoning_details");
        let codex_reasoning_items = extra_json("codex_reasoning_items");
        let codex_message_items = extra_json("codex_message_items");

        // Fallback synthesis only when nothing verbatim was captured: a foreign/in-memory session
        // still gets its Thinking text + encrypted blob persisted (matching the prior behaviour).
        if reasoning.is_none() && reasoning_content.is_none() && reasoning_details.is_none() {
            let mut text = String::new();
            let mut enc: Option<&str> = None;
            for b in &msg.content {
                if let Block::Thinking { text: t, encrypted, .. } = b {
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                    if enc.is_none() {
                        enc = encrypted.as_deref();
                    }
                }
            }
            if !text.is_empty() {
                reasoning = Some(text);
            }
            if let Some(enc) = enc {
                reasoning_details =
                    Some(json!([{ "type": "reasoning.encrypted_content", "encrypted_content": enc }]).to_string());
            }
        }

        let finish_reason = extra_str("finish_reason");
        let platform_message_id = extra_str("platform_message_id");
        let observed: i64 = if msg.extra.get("observed") == Some(&Value::Bool(true)) {
            1
        } else {
            0
        };
        // token_count: the parser routes a row's single combined count to Usage (output for
        // assistant turns, input otherwise); reverse that mapping to recover the column.
        let token_count: Option<i64> = msg.usage.as_ref().and_then(|u| {
            let v = if msg.role == Role::Assistant {
                u.output_tokens
            } else {
                u.input_tokens
            };
            v.map(|n| n as i64)
        });

        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    ..
                } = b
                {
                    // `tool_name` matters: Hermes's own reader (and our adapter) surfaces it, so
                    // dropping it here made every ported tool turn come back nameless. The optional
                    // per-row columns (token_count / finish_reason / platform_message_id / observed)
                    // can also ride a tool row, so write them back for a lossless round-trip.
                    conn.execute(
                        "INSERT INTO messages \
                         (session_id, role, content, tool_call_id, tool_name, timestamp, \
                          token_count, finish_reason, platform_message_id, observed) \
                         VALUES (?1, 'tool', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            new_id,
                            content.to_string(),
                            tool_use_id,
                            tool_name,
                            ts,
                            token_count,
                            finish_reason,
                            platform_message_id,
                            observed,
                        ],
                    )
                    .context("inserting hermes tool message")?;
                }
            }
            continue;
        }

        // Content: plain text, or the multimodal sentinel form if there are images.
        let has_image = msg.content.iter().any(|b| matches!(b, Block::Image { .. }));
        let content: Option<String> = if has_image {
            let mut parts = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => parts.push(json!({ "type": "text", "text": text })),
                    Block::Image { data_ref, .. } => {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": data_ref.clone().unwrap_or_default() },
                        }));
                    }
                    _ => {}
                }
            }
            Some(format!(
                "{MULTIMODAL_SENTINEL}{}",
                serde_json::to_string(&Value::Array(parts))?
            ))
        } else {
            msg.text()
        };

        // Assistant tool calls → OpenAI-shaped tool_calls JSON.
        let tool_calls: Option<String> = {
            let calls: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                        },
                    })),
                    _ => None,
                })
                .collect();
            (!calls.is_empty()).then(|| Value::Array(calls).to_string())
        };

        conn.execute(
            "INSERT INTO messages \
             (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, \
              reasoning, reasoning_content, reasoning_details, codex_reasoning_items, \
              codex_message_items, platform_message_id, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                new_id,
                role,
                content,
                tool_calls,
                ts,
                token_count,
                finish_reason,
                reasoning,
                reasoning_content,
                reasoning_details,
                codex_reasoning_items,
                codex_message_items,
                platform_message_id,
                observed,
            ],
        )
        .context("inserting hermes message")?;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("hermes --resume {new_id}")),
    })
}

// ------------------------------------------------------------------------------------------------
// shared
// ------------------------------------------------------------------------------------------------

fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut buf = String::new();
    for v in lines {
        buf.push_str(&serde_json::to_string(v)?);
        buf.push('\n');
    }
    fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        claude::Claude, codex::Codex, gemini::Gemini, grok::Grok, openclaw::OpenClaw, opencode::OpenCode, Adapter,
    };
    use crate::ir::*;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cv-emit-{}", Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_session(harness: Harness) -> Session {
        let mut sys = Message::new(Role::System);
        sys.content.push(Block::Text {
            text: "you are a helpful assistant".into(),
        });

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "list the files please".into(),
        });

        let mut asst = Message::new(Role::Assistant);
        asst.model = Some("test-model".into());
        asst.content.push(Block::Thinking {
            text: "I should run ls".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        asst.content.push(Block::Text {
            text: "Sure, listing now.".into(),
        });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "run_shell".into(),
            input: serde_json::json!({ "cmd": "ls" }),
        });

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "file_a.txt\nfile_b.txt".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });

        Session {
            id: "orig-id".into(),
            harness,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("a test session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: Some(GitInfo {
                branch: Some("main".into()),
                commit: None,
                remote: None,
            }),
            messages: vec![sys, user, asst, tool],
            source_path: None,
            extra: serde_json::Map::new(),
        }
    }

    fn texts(session: &Session, role: Role) -> Vec<String> {
        session
            .messages
            .iter()
            .filter(|m| m.role == role)
            .filter_map(|m| m.text())
            .collect()
    }

    fn tool_names(session: &Session) -> Vec<String> {
        let mut out = Vec::new();
        for m in &session.messages {
            for b in &m.content {
                if let Block::ToolUse { name, .. } = b {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    #[test]
    fn claude_round_trip() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // Re-parse via the Claude adapter. id is derived from the filename stem.
        let id = res.path.file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(id, res.new_id);
        let r = SessionRef {
            id,
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();

        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // The Tool turn (tool_result) should survive as a Role::Tool message.
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        // Thinking survives.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // gitBranch round-trips.
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
    }

    #[test]
    fn claude_encode_cwd_format() {
        assert_eq!(
            claude_encode_cwd(Path::new("/Users/test/my.project")),
            "-Users-test-my-project"
        );
    }

    /// Porting to a symlinked cwd must key the project dir off the *realpath*, so `claude --resume`
    /// — which resolves symlinks on the launch dir — discovers the session. Guards the macOS
    /// `/tmp` → `/private/tmp` jail. See `realpath_for_claude`.
    #[test]
    #[cfg(unix)]
    fn claude_project_dir_follows_symlinked_cwd() {
        use std::os::unix::fs::symlink;

        let base = temp_dir();
        let real = base.join("real_project");
        fs::create_dir_all(&real).unwrap();
        let link = base.join("link_project");
        symlink(&real, &link).unwrap();

        // Sanity: the link path encodes differently from its realpath, else the test proves nothing.
        let resolved = fs::canonicalize(&link).unwrap();
        assert_ne!(claude_encode_cwd(&link), claude_encode_cwd(&resolved));

        let out = temp_dir();
        let res = emit(
            &sample_session(Harness::Claude),
            Harness::Claude,
            &out,
            &EmitOptions {
                new_cwd: Some(link.clone()),
                new_id: None,
            },
        )
        .unwrap();

        let proj_dir = res.path.parent().unwrap().file_name().unwrap();
        assert_eq!(proj_dir, claude_encode_cwd(&resolved).as_str());
        assert_ne!(proj_dir, claude_encode_cwd(&link).as_str());
    }

    /// User turns with non-text blocks must emit Claude's array-of-blocks content — the old
    /// `msg.text()` flattening dropped user images and document/File attachments entirely.
    #[test]
    fn claude_round_trip_preserves_user_images_and_files() {
        let mut s = sample_session(Harness::Claude);
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "look at this".into(),
        });
        user.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("https://example.com/x.png".into()),
        });
        user.content.push(Block::File {
            mime: Some("application/pdf".into()),
            path: Some("spec.pdf".into()),
            source: Some("file:file_123".into()),
        });
        s.messages.push(user);

        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();

        let rich = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .find(|m| m.content.len() > 1)
            .expect("mixed-content user turn survives as multiple blocks");
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::Text { text } if text == "look at this")),
            "user text block survives"
        );
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::Image { media_type, data_ref }
                if media_type.as_deref() == Some("image/png")
                    && data_ref.as_deref() == Some("https://example.com/x.png"))),
            "user image (media_type + data_ref) survives: {:?}",
            rich.content
        );
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::File { mime, path, source }
                if mime.as_deref() == Some("application/pdf")
                    && path.as_deref() == Some("spec.pdf")
                    && source.as_deref() == Some("file:file_123"))),
            "user document/File attachment survives: {:?}",
            rich.content
        );
    }

    /// Non-carrier System turns (a `full()`-parse of `system` records) are dropped on emit; their
    /// children must be re-linked to the nearest surviving ancestor, never left pointing at a uuid
    /// that isn't in the file. Covers a dropped root, a dropped mid-chain node, and a dropped
    /// *chain* (two consecutive system records).
    #[test]
    fn claude_emit_relinks_parents_over_dropped_system_records() {
        use crate::harness::claude::parse_str;

        let sid = "22222222-2222-4222-8222-222222222222";
        let src_lines = [
            json!({"type":"system","uuid":"s0","parentUuid":null,"sessionId":sid,"subtype":"info",
                "content":"session started","level":"info","timestamp":"2026-06-16T00:00:00.000Z"}),
            json!({"type":"user","uuid":"u1","parentUuid":"s0","sessionId":sid,"cwd":"/work/proj",
                "timestamp":"2026-06-16T00:00:01.000Z","message":{"role":"user","content":"hello"}}),
            json!({"type":"system","uuid":"s2","parentUuid":"u1","sessionId":sid,"subtype":"local_command",
                "content":"<command-name>/foo</command-name>","timestamp":"2026-06-16T00:00:02.000Z"}),
            json!({"type":"system","uuid":"s3","parentUuid":"s2","sessionId":sid,"subtype":"info",
                "content":"notice two","timestamp":"2026-06-16T00:00:03.000Z"}),
            json!({"type":"assistant","uuid":"a4","parentUuid":"s3","sessionId":sid,
                "timestamp":"2026-06-16T00:00:04.000Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}),
        ];
        let text = src_lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let session = parse_str(sid, &text, None);
        // Sanity: the lean full() parse surfaced the system records as content-bearing System turns.
        assert_eq!(
            session.messages.iter().filter(|m| m.role == Role::System).count(),
            3,
            "full() should parse all three system records into System messages"
        );

        let out = temp_dir();
        let res = emit(&session, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        let emitted: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // No parentUuid may dangle: every non-null parent must be a uuid present in the file.
        let uuids: std::collections::HashSet<&str> = emitted
            .iter()
            .filter_map(|v| v.get("uuid").and_then(Value::as_str))
            .collect();
        for v in &emitted {
            if let Some(p) = v.get("parentUuid").and_then(Value::as_str) {
                assert!(uuids.contains(p), "dangling parentUuid {p} in emitted transcript");
            }
        }
        // And the re-links went to the *nearest surviving ancestor*:
        let parent_of = |uuid: &str| -> Option<String> {
            emitted
                .iter()
                .find(|v| v.get("uuid").and_then(Value::as_str) == Some(uuid))
                .and_then(|v| v.get("parentUuid").and_then(Value::as_str))
                .map(str::to_string)
        };
        // u1 pointed at the dropped root s0 → re-linked to null (no surviving ancestor).
        assert_eq!(parent_of("u1"), None, "child of a dropped root gets a null parent");
        // a4 pointed at s3 → s2 → u1: the dropped chain collapses to u1.
        assert_eq!(
            parent_of("a4").as_deref(),
            Some("u1"),
            "child of a dropped system chain re-links to the surviving ancestor"
        );
    }

    #[test]
    fn codex_error_outputs_are_decodable_strings() {
        // `function_call_output.output` is a string or a content-item array — never an object.
        // The old `{content, success:false}` form was undecodable and dropped on resume.
        let mut s = sample_session(Harness::Codex);
        for m in &mut s.messages {
            for b in &mut m.content {
                if let Block::ToolResult { is_error, .. } = b {
                    *is_error = true;
                }
            }
        }
        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        let outputs: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|item| item["payload"]["type"] == "function_call_output")
            .collect();
        assert_eq!(outputs.len(), 1);
        let output = &outputs[0]["payload"]["output"];
        assert!(output.is_string(), "got {output}");
        assert!(output.as_str().unwrap().starts_with("[error] "));
    }

    #[test]
    fn codex_round_trip() {
        let s = sample_session(Harness::Codex);
        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        let first_line = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .next()
            .map(str::to_owned)
            .unwrap();
        let session_meta: Value = serde_json::from_str(&first_line).unwrap();
        assert_eq!(
            session_meta["payload"]["model_provider"], "openai",
            "emitted Codex sessions must be resumable before their model turn-context is read"
        );
        // Codex's decoder rules (rollout_file_name.rs / protocol.rs / models.rs at 132c2be23):
        // file name `rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl`, local time, no zone suffix.
        let name = res.path.file_name().unwrap().to_string_lossy().into_owned();
        let stamp = name.strip_prefix("rollout-").unwrap();
        assert_eq!(stamp.as_bytes()[19], b'-', "byte 19 must be `-`: {name}");
        assert!(
            stamp[..19].chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'T'),
            "{name}"
        );
        assert!(stamp[20..].starts_with(&res.new_id), "{name}");
        // session_meta: `cwd` is required; history_mode says which layout we wrote.
        assert_eq!(session_meta["payload"]["cwd"], "/Users/test/project");
        assert_eq!(session_meta["payload"]["history_mode"], "legacy");
        assert_eq!(session_meta["payload"]["session_id"], res.new_id);
        // turn_context: every required field present, else the line is skipped and the model lost.
        let tc = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|item| item["type"] == "turn_context")
            .expect("turn_context emitted");
        for key in [
            "turn_id",
            "cwd",
            "approval_policy",
            "sandbox_policy",
            "model",
            "summary",
        ] {
            assert!(
                tc["payload"].get(key).is_some(),
                "turn_context.{key} is required by Codex"
            );
        }
        assert_eq!(tc["payload"]["sandbox_policy"]["type"], "workspace-write");
        assert_eq!(tc["payload"]["approval_policy"], "on-request");
        let emitted_reasoning = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|item| item["type"] == "response_item" && item["payload"]["type"] == "reasoning")
            .unwrap();
        assert!(
            emitted_reasoning["payload"].get("content").is_none(),
            "raw reasoning content is not accepted when a Codex rollout is replayed"
        );
        assert_eq!(emitted_reasoning["payload"]["summary"].as_array().unwrap().len(), 1);
        // The model must survive: it's carried by a `turn_context` record (the only place the
        // codex parser reads a model id from; `session_meta.model_provider` is not a model).
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        // event_msg user/assistant text survives.
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // function_call_output round-trips as a Tool message (a success: is_error false).
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, is_error, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
            assert!(!is_error);
        } else {
            panic!("expected tool result");
        }
        // reasoning survives.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // git branch round-trips.
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
        // System turn (developer) survives.
        assert!(parsed.messages.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn codex_round_trip_preserves_tool_failure() {
        // Codex persists no error bit on `function_call_output` (string or content-item array only),
        // so a failed result is emitted as the string "[error] <content>" and cv's Codex parser
        // restores `is_error` from that prefix — the object form ({content, success:false}) we
        // used to write is undecodable by Codex and was dropped on resume.
        let mut s = sample_session(Harness::Codex);
        let mut failed = Message::new(Role::Tool);
        failed.content.push(Block::ToolResult {
            tool_use_id: "call_2".into(),
            content: "command not found: frobnicate".into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        });
        s.messages.push(failed);

        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();
        let mut results: Vec<(String, String, bool)> = Vec::new();
        for m in parsed.messages.iter().filter(|m| m.role == Role::Tool) {
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    results.push((tool_use_id.clone(), content.to_string(), *is_error));
                }
            }
        }
        assert_eq!(
            results,
            vec![
                ("call_1".to_string(), "file_a.txt\nfile_b.txt".to_string(), false),
                ("call_2".to_string(), "command not found: frobnicate".to_string(), true),
            ],
            "is_error must survive a port into codex"
        );
    }

    #[test]
    fn grok_round_trip() {
        let s = sample_session(Harness::Grok);
        let out = temp_dir();
        let res = emit(&s, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.is_dir());
        assert!(res.path.join("summary.json").exists());
        assert!(res.path.join("chat_history.jsonl").exists());

        // r.path is the session directory for Grok.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Grok,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Grok::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(parsed.git.clone().and_then(|g| g.branch), Some("main".to_string()));
        // Thinking/reasoning survives on the assistant turn.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // Tool calls now SURVIVE (previously emit dropped all tool turns).
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // And the assistant tool call's arguments round-trip.
        let tool_use_input = parsed.messages.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                Block::ToolUse { input, .. } => Some(input.clone()),
                _ => None,
            })
        });
        assert_eq!(
            tool_use_input
                .as_ref()
                .and_then(|v| v.get("cmd"))
                .and_then(|v| v.as_str()),
            Some("ls")
        );
        // The tool RESULT survives as a Role::Tool message with its content intact.
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult {
            content, tool_use_id, ..
        } = &tool_results[0].content[0]
        {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
            assert_eq!(tool_use_id, "call_1");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn grok_round_trip_preserves_tool_failure() {
        // chat_history.jsonl can't express a failed tool result; the Grok parser derives
        // is_error/status from updates.jsonl. Emit must therefore write the status sidecar, or
        // every ported failure silently becomes a success.
        let mut s = sample_session(Harness::Grok);
        let mut failed = Message::new(Role::Tool);
        failed.content.push(Block::ToolResult {
            tool_use_id: "call_2".into(),
            content: "command not found: frobnicate".into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        });
        s.messages.push(failed);

        let out = temp_dir();
        let res = emit(&s, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.join("updates.jsonl").exists(), "status sidecar written");

        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Grok,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Grok::new().parse(&r).unwrap();
        let mut results: Vec<(String, bool, Option<String>)> = Vec::new();
        for m in parsed.messages.iter().filter(|m| m.role == Role::Tool) {
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    is_error,
                    status,
                    ..
                } = b
                {
                    results.push((tool_use_id.clone(), *is_error, status.clone()));
                }
            }
        }
        assert_eq!(
            results,
            vec![
                ("call_1".to_string(), false, Some("completed".to_string())),
                ("call_2".to_string(), true, Some("failed".to_string())),
            ],
            "is_error/status must survive a port into grok"
        );
    }

    #[test]
    fn new_cwd_and_new_id_overrides() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let opts = EmitOptions {
            new_cwd: Some(PathBuf::from("/tmp/rehomed")),
            new_id: Some("forced-id".into()),
        };
        let res = emit(&s, Harness::Claude, &out, &opts).unwrap();
        assert_eq!(res.new_id, "forced-id");
        assert!(res.path.to_string_lossy().contains("-tmp-rehomed"));
        assert!(res.path.ends_with("forced-id.jsonl"));
    }

    #[test]
    fn opencode_round_trip() {
        // OpenCode's parse() resolves message/part dirs relative to a storage root; point an
        // adapter at the emitted storage dir explicitly (no HOME mutation).
        let storage = temp_dir().join(".local/share/opencode/storage");
        fs::create_dir_all(&storage).unwrap();

        let s = sample_session(Harness::OpenCode);
        let res = emit(&s, Harness::OpenCode, &storage, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        let oc = OpenCode::with_root(storage);
        let refs = oc.discover().unwrap();
        let parsed = refs.iter().find(|r| r.id == res.new_id).map(|r| oc.parse(r).unwrap());

        let parsed = parsed.expect("emitted opencode session should be discoverable");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result split into a Tool turn
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
        } else {
            panic!("expected tool result");
        }
        // reasoning survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[test]
    fn openclaw_round_trip() {
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let res = emit(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        // sessions.json index written
        assert!(res.path.parent().unwrap().join("sessions.json").exists());

        // OpenClaw::parse reads r.path directly; cwd/created come from the header line.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::OpenClaw,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = OpenClaw::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result → toolResult message (Role::Tool)
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        // thinking survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // model promoted
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn gemini_round_trip() {
        let s = sample_session(Harness::Gemini);
        let out = temp_dir();
        let res = emit(&s, Harness::Gemini, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // gemini parse keys off the `chats/` dir in the path; emit places the file there.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Gemini,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Gemini::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        // tool result is retagged to a Tool turn by the reader
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // thinking → thoughts survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_round_trip() {
        use crate::harness::hermes::Hermes;
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        assert_eq!(res.path.file_name().unwrap(), "state.db");

        // Point an adapter straight at the emitted DB (no HERMES_HOME mutation).
        let h = Hermes::with_db(res.path.clone());
        let refs = h.discover().unwrap();
        let parsed = refs.iter().find(|r| r.id == res.new_id).map(|r| h.parse(r).unwrap());

        let parsed = parsed.expect("emitted hermes session should be discoverable");
        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result row → Tool turn
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // reasoning → Thinking
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_emit_creates_fts_and_is_searchable() {
        use rusqlite::{Connection, OpenFlags};

        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        let db = res.path.clone();
        assert_eq!(db.file_name().unwrap(), "state.db");

        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

        // The FTS virtual tables must exist (a fresh port is invisible to Hermes search without them).
        let table_exists = |name: &str| -> bool {
            conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .is_ok()
        };
        assert!(table_exists("messages_fts"), "messages_fts must exist");
        // trigram is guarded; assert it exists when the tokenizer is available (it is in bundled FTS5).
        assert!(
            table_exists("messages_fts_trigram"),
            "messages_fts_trigram should exist with bundled FTS5"
        );

        // The triggers must have populated the index: an FTS MATCH finds an emitted message.
        let cnt: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt >= 1, "FTS query for 'files' should match the emitted user message");

        // And the trigram index covers a substring of the assistant text.
        let cnt_t: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'listing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt_t >= 1, "trigram FTS should find 'listing' substring");
    }

    /// A session whose only non-text payload is an image — a lossy target (Grok stores plain text
    /// only) should report the image as dropped.
    fn image_session() -> Session {
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "look at this".into(),
        });
        user.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("data:image/png;base64,AAAA".into()),
        });
        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text {
            text: "nice picture".into(),
        });

        Session {
            id: "img-id".into(),
            harness: Harness::Grok,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("image session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: None,
            messages: vec![user, asst],
            source_path: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn emit_verified_reports_lossy_grok() {
        // An image-bearing session ported to Grok (plain-text chat_history) should warn that the
        // image was dropped — the canonical lossy case.
        let img = image_session();
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&img, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("image")),
            "expected an image-dropped warning, got: {warnings:?}"
        );

        // A tools+text+system+reasoning Grok session now round-trips cleanly (post tool-emit fix):
        // Grok keeps system turns, tool calls, tool results, and reasoning.
        let clean = sample_session(Harness::Grok);
        let out2 = temp_dir();
        let (_r2, w2) = emit_verified(&clean, Harness::Grok, &out2, &EmitOptions::default()).unwrap();
        assert!(w2.is_empty(), "expected clean Grok round-trip, got: {w2:?}");
    }

    #[test]
    fn emit_verified_clean_for_openclaw() {
        // OpenClaw represents text, thinking, tool calls/results, system turns — a clean round-trip.
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        // No tool/image/reasoning loss; OpenClaw keeps system turns and the title.
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for OpenClaw, got: {warnings:?}"
        );
    }

    /// Every `messages`-row column we care about for losslessness, as a comparable record.
    #[cfg(feature = "sqlite")]
    #[derive(Debug, PartialEq)]
    struct HermesMsgRow {
        role: String,
        content: Option<String>,
        tool_call_id: Option<String>,
        tool_calls: Option<String>,
        tool_name: Option<String>,
        token_count: Option<i64>,
        finish_reason: Option<String>,
        reasoning: Option<String>,
        reasoning_content: Option<String>,
        reasoning_details: Option<String>,
        codex_reasoning_items: Option<String>,
        codex_message_items: Option<String>,
        platform_message_id: Option<String>,
        observed: Option<i64>,
    }

    /// Dump every message row (ordered) of `session_id` from a Hermes `state.db` into comparable
    /// tuples — the data side of the column set. `timestamp` is excluded (REAL secs, compared
    /// separately at ms granularity); `id`/`session_id` are surrogate keys, not content.
    #[cfg(feature = "sqlite")]
    fn dump_hermes_messages(db: &Path, session_id: &str) -> Vec<HermesMsgRow> {
        use rusqlite::{Connection, OpenFlags};
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT role, content, tool_call_id, tool_calls, tool_name, token_count, \
                 finish_reason, reasoning, reasoning_content, reasoning_details, \
                 codex_reasoning_items, codex_message_items, platform_message_id, observed \
                 FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(HermesMsgRow {
                    role: r.get(0)?,
                    content: r.get(1)?,
                    tool_call_id: r.get(2)?,
                    tool_calls: r.get(3)?,
                    tool_name: r.get(4)?,
                    token_count: r.get(5)?,
                    finish_reason: r.get(6)?,
                    reasoning: r.get(7)?,
                    reasoning_content: r.get(8)?,
                    reasoning_details: r.get(9)?,
                    codex_reasoning_items: r.get(10)?,
                    codex_message_items: r.get(11)?,
                    platform_message_id: r.get(12)?,
                    observed: r.get(13)?,
                })
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Dump the session-row columns we round-trip (everything but the surrogate `id` and the
    /// auto-derived `message_count`). `started_at`/`ended_at` are REAL secs and compared at ms
    /// granularity in the caller, so they're excluded here.
    #[cfg(feature = "sqlite")]
    #[allow(clippy::type_complexity)]
    fn dump_hermes_session(
        db: &Path,
        session_id: &str,
    ) -> (
        String,         // source
        Option<String>, // user_id
        Option<String>, // model
        Option<String>, // model_config
        Option<String>, // system_prompt
        Option<String>, // end_reason
        Option<String>, // title
        // tool_call_count, input/output/cache_read/cache_write/reasoning tokens (grouped to stay
        // within std's 12-arity Debug/PartialEq tuple impls).
        [i64; 6],
    ) {
        use rusqlite::{Connection, OpenFlags};
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.query_row(
            "SELECT source, user_id, model, model_config, system_prompt, end_reason, title, \
             tool_call_count, input_tokens, output_tokens, cache_read_tokens, \
             cache_write_tokens, reasoning_tokens FROM sessions WHERE id = ?1",
            [session_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?],
                ))
            },
        )
        .unwrap()
    }

    /// Format-complete losslessness for the SQLite-backed Hermes adapter: build a `state.db` row set
    /// that exercises the **full column surface** (every per-session column and every per-message
    /// column: tool calls + results, all three reasoning columns, codex items, token_count,
    /// finish_reason, platform_message_id, observed), parse it, emit it to a fresh db, re-parse, and
    /// assert that emit reproduced every session-row and message-row column value. The parse→emit
    /// path is the round-trip under test; we compare the *source* db's rows against the
    /// *re-emitted* db's rows so nothing is asserted only via the lossy IR projection.
    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_complete_round_trip_is_lossless() {
        use crate::harness::hermes::Hermes;
        use crate::harness::Adapter;
        use rusqlite::Connection;

        // --- 1. Build a source state.db exercising the full column set. ---
        let src_home = temp_dir();
        let src_db = src_home.join("state.db");
        {
            let conn = Connection::open(&src_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, source TEXT NOT NULL, user_id TEXT, model TEXT,
                    model_config TEXT, system_prompt TEXT, parent_session_id TEXT,
                    started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
                    message_count INTEGER DEFAULT 0, tool_call_count INTEGER DEFAULT 0,
                    input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                    cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
                    reasoning_tokens INTEGER DEFAULT 0, title TEXT
                 );
                 CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
                    content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
                    timestamp REAL NOT NULL, token_count INTEGER, finish_reason TEXT, reasoning TEXT,
                    reasoning_content TEXT, reasoning_details TEXT, codex_reasoning_items TEXT,
                    codex_message_items TEXT, platform_message_id TEXT, observed INTEGER DEFAULT 0
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions \
                 (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
                  end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
                  cache_read_tokens, cache_write_tokens, reasoning_tokens, title) \
                 VALUES ('src', 'telegram', 'u-42', 'nous/hermes-4', '{\"temp\":0.7}', \
                 'be helpful', 1000.000, 1050.000, 'completed', 3, 1, 11, 22, 3, 4, 5, 'Full session')",
                [],
            )
            .unwrap();
            // user turn with token_count + platform_message_id + observed.
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, timestamp, token_count, platform_message_id, observed) \
                 VALUES ('src','user','hello there',1001.000,7,'pmid-1',1)",
                [],
            )
            .unwrap();
            // assistant turn: tool call + all reasoning columns + finish_reason + token_count + codex.
            let tc = "[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]";
            let rd = "[{\"type\":\"reasoning.summary\",\"summary\":\"sum\"},{\"type\":\"reasoning.encrypted_content\",\"encrypted_content\":\"ENC\"}]";
            let cri = "[{\"type\":\"reasoning\",\"id\":\"rs_a\",\"encrypted_content\":\"CBLOB\"}]";
            let cmi =
                "[{\"type\":\"message\",\"phase\":\"final\",\"content\":[{\"type\":\"output_text\",\"text\":\"D\"}]}]";
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, \
                  reasoning, reasoning_content, reasoning_details, codex_reasoning_items, \
                  codex_message_items) \
                 VALUES ('src','assistant','here you go',?1,1002.000,99,'stop','short sum',\
                 'native pad',?2,?3,?4)",
                rusqlite::params![tc, rd, cri, cmi],
            )
            .unwrap();
            // tool result turn with tool_name.
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, tool_call_id, tool_name, timestamp) \
                 VALUES ('src','tool','{\"ok\":true}','c1','web_search',1003.000)",
                [],
            )
            .unwrap();
        }
        let src_rows = dump_hermes_messages(&src_db, "src");
        let src_session = dump_hermes_session(&src_db, "src");

        // --- 2. Parse the source db (adapter pointed straight at it; no HERMES_HOME mutation). ---
        let h = Hermes::with_db(src_db.clone());
        let sref = h
            .discover()
            .unwrap()
            .into_iter()
            .find(|r| r.id == "src")
            .expect("source session discoverable");
        let parsed = h.parse(&sref).unwrap();

        // --- 3. Emit to a fresh db, preserving the original id so the dumps line up. ---
        let dst_home = temp_dir();
        let opts = EmitOptions {
            new_id: Some("src".into()),
            ..Default::default()
        };
        let res = emit(&parsed, Harness::Hermes, &dst_home, &opts).unwrap();

        // --- 4. Compare the source db rows against the re-emitted db rows, column by column. ---
        // JSON-bearing columns (tool_calls / reasoning_details / codex_*) are compared by *parsed
        // value*, not raw string: the IR stash → re-serialize path canonicalizes JSON whitespace
        // (and, before `preserve_order`, used to re-sort keys), which the losslessness bar
        // explicitly permits ("key ORDER may differ"). Plain-text columns are compared verbatim.
        let dst_rows = dump_hermes_messages(&res.path, "src");
        let norm = |rows: Vec<HermesMsgRow>| -> Vec<HermesMsgRow> {
            rows.into_iter()
                .map(|mut r| {
                    let canon = |s: &Option<String>| -> Option<String> {
                        s.as_deref()
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok().map(|v| v.to_string()))
                    };
                    if let Some(c) = canon(&r.tool_calls) {
                        r.tool_calls = Some(c);
                    }
                    if let Some(c) = canon(&r.reasoning_details) {
                        r.reasoning_details = Some(c);
                    }
                    if let Some(c) = canon(&r.codex_reasoning_items) {
                        r.codex_reasoning_items = Some(c);
                    }
                    if let Some(c) = canon(&r.codex_message_items) {
                        r.codex_message_items = Some(c);
                    }
                    r
                })
                .collect()
        };
        assert_eq!(
            norm(src_rows),
            norm(dst_rows),
            "message-row columns must round-trip losslessly (left=source, right=re-emitted)"
        );
        let dst_session = dump_hermes_session(&res.path, "src");
        assert_eq!(
            src_session, dst_session,
            "session-row columns must round-trip losslessly (left=source, right=re-emitted)"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn emit_verified_clean_for_hermes() {
        // Hermes keeps tools, reasoning, system turns, and title — should round-trip cleanly.
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for Hermes, got: {warnings:?}"
        );
    }

    /// Format-complete losslessness: a rich Claude transcript parsed under `ParseOptions::complete`
    /// and re-emitted reproduces EVERY record with the same top-level key set and the same value
    /// per key (key order may differ). Covers a user turn, an assistant turn (text + tool_use), a
    /// user tool_result turn (with a `toolUseResult` mirror), a `system` record, and three meta
    /// records (mode / queue-operation / ai-title) carried verbatim.
    #[test]
    fn claude_complete_round_trip_is_lossless() {
        use crate::harness::claude::{self, stream_str, CARRIER_KEY};
        use crate::stream::{CollectSink, ParseOptions};

        let sid = "11111111-1111-4111-8111-111111111111";
        // A transcript exercising every covered record shape, with assorted top-level + message-level
        // metadata the lean passes would drop.
        let src_lines = [
            json!({"type":"user","uuid":"u0","parentUuid":null,"sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","userType":"external","isSidechain":false,
                "timestamp":"2026-06-16T00:00:00.000Z","message":{"role":"user","content":"hello there"}}),
            json!({"type":"mode","sessionId":sid,"mode":"plan","timestamp":"2026-06-16T00:00:01.000Z"}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u0","sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","requestId":"req_42","isSidechain":false,
                "timestamp":"2026-06-16T00:00:02.000Z",
                "message":{"id":"msg_xyz","role":"assistant","model":"claude-opus-4",
                    "stop_reason":"tool_use","stop_sequence":null,
                    "usage":{"input_tokens":12,"output_tokens":34,"cache_read_input_tokens":5},
                    "content":[{"type":"text","text":"on it"},
                        {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a.rs"}}]}}),
            json!({"type":"queue-operation","sessionId":sid,"operation":"enqueue","content":"do later",
                "timestamp":"2026-06-16T00:00:03.000Z"}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","userType":"external","isSidechain":false,
                "timestamp":"2026-06-16T00:00:04.000Z",
                "toolUseResult":{"filePath":"a.rs","content":"fn main(){}","numLines":1},
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":"fn main(){}","is_error":false}]}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"s3","parentUuid":"u2",
                "sessionId":sid,"level":"info","timestamp":"2026-06-16T00:00:05.000Z",
                "compactMetadata":{"trigger":"manual","preTokens":1000}}),
            json!({"type":"ai-title","sessionId":sid,"aiTitle":"Reading a file"}),
        ];
        let src_text = src_lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        // Parse format-complete: every record is carried (meta/system as carriers, user/assistant/
        // tool with exhaustive `extra`).
        let mut sink = CollectSink::default();
        let session = stream_str(sid, &src_text, None, &ParseOptions::complete(), &mut sink);
        let mut session = session;
        session.messages = sink.messages;
        assert_eq!(
            session.messages.len(),
            src_lines.len(),
            "every source record must produce a message under complete"
        );

        // Emit back to disk, keeping the original session id + cwd so the envelope matches the source.
        let out = temp_dir();
        let opts = EmitOptions {
            new_id: Some(sid.to_string()),
            new_cwd: Some(PathBuf::from("/work/proj")),
        };
        let res = emit(&session, Harness::Claude, &out, &opts).unwrap();

        // Read the emitted .jsonl back, line by line, as raw JSON.
        let emitted_text = fs::read_to_string(&res.path).unwrap();
        let emitted: Vec<Value> = emitted_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect();

        assert_eq!(
            emitted.len(),
            src_lines.len(),
            "same number of records out as in (no dup ai-title, no dropped record)"
        );

        // Index source records by uuid (else fall back to type for the uuid-less meta lines).
        let key_of = |v: &Value| -> String {
            v.get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| v.get("type").and_then(Value::as_str).unwrap_or("?").to_string())
        };
        let by_key: std::collections::HashMap<String, &Value> = src_lines.iter().map(|v| (key_of(v), v)).collect();

        // Carriers must come back byte-equal (same JSON value) to their source record.
        let carrier_types: Vec<&str> = session
            .messages
            .iter()
            .filter_map(|m| m.extra.get(CARRIER_KEY)?.get("type")?.as_str())
            .collect();
        assert!(
            carrier_types.contains(&"mode")
                && carrier_types.contains(&"queue-operation")
                && carrier_types.contains(&"ai-title")
                && carrier_types.contains(&"system"),
            "meta + system records carried as carriers, got {carrier_types:?}"
        );
        // sanity: CARRIER_KEY constant is the public one.
        assert_eq!(claude::CARRIER_KEY, "_record");

        // For every emitted record, find its source and assert key-set + per-value parity.
        for out_rec in &emitted {
            let k = key_of(out_rec);
            let src = by_key
                .get(&k)
                .unwrap_or_else(|| panic!("no source for emitted record {k}"));
            let src_obj = src.as_object().unwrap();
            let out_obj = out_rec.as_object().unwrap();

            let src_keys: std::collections::BTreeSet<&String> = src_obj.keys().collect();
            let out_keys: std::collections::BTreeSet<&String> = out_obj.keys().collect();
            assert_eq!(
                src_keys, out_keys,
                "record {k}: top-level key set differs\n src={src_keys:?}\n out={out_keys:?}"
            );
            for (key, src_val) in src_obj {
                assert_eq!(
                    out_obj.get(key),
                    Some(src_val),
                    "record {k}: value for `{key}` differs\n src={src_val}\n out={:?}",
                    out_obj.get(key)
                );
            }
        }

        fs::remove_dir_all(&out).ok();
    }
}
