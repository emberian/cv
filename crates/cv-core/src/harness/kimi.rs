//! Kimi CLI (MoonshotAI) adapter — `~/.kimi/sessions/<md5(cwd)>/<session-uuid>/context.jsonl`.
//!
//! ## Storage layout
//! Root is `$KIMI_SHARE_DIR` or `~/.kimi`. Each work directory maps to a project dir whose name is
//! **`md5(cwd_utf8).hexdigest()`** (lowercase hex), or `"<kaos>_<md5>"` when the KAOS isn't `local`.
//! The cwd is NOT stored in the transcript; we recover it from `~/.kimi/kimi.json`'s `work_dirs[]`
//! (`{path, kaos, last_session_id}`) by matching `md5(path) == <hash>`.
//!
//! Two on-disk shapes (sniffed, never via a version field):
//! - **Modern (dir form):** `sessions/<hash>/<uuid>/context.jsonl` plus a `wire.jsonl` sidecar
//!   (timestamps + token usage), optional `state.json` (`custom_title`), and archived pre-compaction
//!   segments `context_1.jsonl…context_N.jsonl` (we read the live `context.jsonl`, recording the
//!   count of segments in `session.extra`).
//! - **Legacy (flat form):** `sessions/<hash>/<uuid>.jsonl` — the transcript only, no sidecar/dir.
//!   (kimi-cli migrates these to the dir form on next open; we still read them in place.)
//!
//! ## `context.jsonl` records (one JSON object per line)
//! - `{"role":"_system_prompt","content":str}` → System message.
//! - `{"role":"_checkpoint","id":N}` → skipped (UI bookmark).
//! - `{"role":"_usage","token_count":N}` → skipped (running total; real usage comes from wire).
//! - `{"role":"user","content": str | [Part]}` → User. `content` is a BARE STRING for a single text
//!   part, else a list of Parts.
//! - `{"role":"assistant","content":[Part],"tool_calls":[…]}` → Assistant.
//! - `{"role":"tool","content": str | [Part],"tool_call_id":id}` → Tool (one ToolResult). The first
//!   text part is often a `<system>summary</system>` line.
//!
//! ### Part (`type`-tagged)
//! - `text{text}` → Text.
//! - `think{think,encrypted}` → Thinking{ text: think, encrypted }.
//! - `image_url{image_url:{url,id}}` → Image (data_ref = url).
//! - `audio_url{…}` / `video_url{…}` → File (source = url). (Media URLs nest under a key named after
//!   the type.)
//!
//! ### tool_calls[]
//! `{type:"function", id, function:{name, arguments}}` → ToolUse{id,name, input}. `arguments` is a
//! JSON-encoded *string*; we parse it (null/empty ⇒ `{}`).
//!
//! ## wire.jsonl sidecar (optional enrichment, like grok's `updates.jsonl`)
//! `{"type":"metadata","protocol_version":"1.x"}` header, then per-event lines
//! `{"timestamp": epoch_float, "message": {"type": …, "payload": …}}`. We mine:
//! - `ToolResult.payload.return_value{is_error,output,message,display,extras}` → richer ToolResult.
//! - `StatusUpdate.payload.token_usage{input_other,output,input_cache_read,input_cache_creation}`
//!   and `message_id` (chatcmpl-…) → attached to the most recent assistant message.
//! - `TurnBegin`/event timestamps → first/last session timestamps when no file mtime is better.
//!
//! We tolerate protocol_version drift (1.1–1.9) and unknown message types: anything we don't
//! recognize is ignored.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub struct Kimi {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
    /// The share root itself (`~/.kimi`), for locating `kimi.json` / `config.toml`.
    root: Option<PathBuf>,
}

impl Kimi {
    pub fn new() -> Self {
        let root = std::env::var_os("KIMI_SHARE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".kimi")))
            .filter(|p| p.exists());
        let sessions = root.as_ref().map(|r| r.join("sessions")).filter(|p| p.exists());
        Kimi { sessions, root }
    }
}

impl Default for Kimi {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Kimi {
    fn harness(&self) -> Harness {
        Harness::Kimi
    }

    fn storage_root(&self) -> Option<PathBuf> {
        // The share root (`~/.kimi`), NOT `<root>/sessions`. `emit()` joins `sessions/<hash>/<id>`
        // itself and also writes `kimi.json` at this root, so it must receive the share root.
        // (Returning `sessions` here produced `~/.kimi/sessions/sessions/...`, which kimi-cli never
        // discovers.) Fall back to `<sessions>/..` if only the sessions dir was detected.
        self.root
            .clone()
            .or_else(|| self.sessions.as_ref().and_then(|s| s.parent().map(Path::to_path_buf)))
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };
        // hash (dir basename, possibly "<kaos>_<md5>") -> cwd, from kimi.json.
        let hash_to_cwd = self
            .root
            .as_ref()
            .map(|r| load_work_dirs(&r.join("kimi.json")))
            .unwrap_or_default();
        let default_model = self
            .root
            .as_ref()
            .and_then(|r| default_model_from_config(&r.join("config.toml")));

        let mut out = Vec::new();
        let Ok(hash_dirs) = fs::read_dir(sessions) else {
            return Ok(out);
        };
        for hd in hash_dirs.filter_map(|e| e.ok()) {
            let hash_path = hd.path();
            let Some(basename) = hash_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !hash_path.is_dir() {
                continue;
            }
            let cwd = hash_to_cwd.get(basename).cloned();
            let Ok(entries) = fs::read_dir(&hash_path) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                let (id, transcript, dir): (String, PathBuf, PathBuf) = if p.is_dir() {
                    // Modern: <uuid>/context.jsonl
                    let ctx = p.join("context.jsonl");
                    if !ctx.exists() {
                        continue;
                    }
                    (name.to_string(), ctx, p.clone())
                } else if name.ends_with(".jsonl") {
                    // Legacy: <uuid>.jsonl (flat). source dir is the hash dir.
                    let id = name.trim_end_matches(".jsonl").to_string();
                    (id, p.clone(), hash_path.clone())
                } else {
                    continue;
                };
                if let Some(r) = scan(&id, &transcript, &dir, cwd.clone(), default_model.clone()) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // r.path is the session dir (modern) or the hash dir (legacy). Locate the transcript.
        let (transcript, wire) = locate_transcript(&r.path, &r.id);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Kimi,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            model: self
                .root
                .as_ref()
                .and_then(|root| default_model_from_config(&root.join("config.toml"))),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // Sidecar enrichment from wire.jsonl (tolerant: absent/empty/malformed ⇒ empty). The
        // per-message `tool_results` map (keyed by tool_call_id) is a per-line enrichment; the
        // positional passes (StatusUpdate usage/message_id zipped onto assistant turns, the first
        // wire timestamp stamped onto the first user turn) only ever walk *forward* with O(1)
        // state, so they run inline here too — `parse()` is exactly `collect(stream())`.
        let enrich = wire.as_ref().map(read_wire_enrichment).unwrap_or_default();

        // Note archived compaction segments (context_1.jsonl…), if any, for fidelity bookkeeping.
        let seg_count = count_segments(&r.path);
        if seg_count > 0 {
            s.extra
                .insert("compaction_segments".into(), Value::Number(seg_count.into()));
        }

        // Session metadata is known up front; hand it to the sink before the body.
        sink.meta(&s);

        // Stream the transcript line-by-line (a single session can be large; `read_to_string` would
        // resident-spike the whole file). Each record becomes at most one Message, emitted to `sink`
        // and dropped before the next line, so peak memory is O(largest message).
        let Ok(file) = fs::File::open(&transcript) else {
            return Ok(s);
        };
        // Positional wire enrichment, streamed: one StatusUpdate per assistant step (in wire
        // order) and the first wire timestamp for the first user turn. Forward-only, O(1) state.
        let mut statuses = enrich.statuses.iter();
        let first_wire_ts = enrich.msg_timestamps.first().copied();
        let mut first_user_seen = false;
        let skipped = super::for_each_json_line(BufReader::new(file), |v| match context_message(&v, &enrich) {
            Some(mut m) => {
                match m.role {
                    Role::Assistant => {
                        if let Some(st) = statuses.next() {
                            if m.usage.is_none() {
                                m.usage = st.usage.clone();
                            }
                            if m.id.is_none() {
                                m.id = st.message_id.clone();
                            }
                        }
                    }
                    Role::User if !first_user_seen => {
                        first_user_seen = true;
                        if m.timestamp.is_none() {
                            m.timestamp = first_wire_ts;
                        }
                    }
                    _ => {}
                }
                sink.message(m)
            }
            None => Flow::Continue,
        });
        super::note_skipped_lines(&mut s, skipped);

        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// IR → Kimi native format (emitter; faithful inverse of `parse`)
// ---------------------------------------------------------------------------

/// Emit `session` into Kimi's native modern (dir-form) layout under `out_dir`:
///   `out_dir/sessions/<hash>/<id>/context.jsonl`  (the transcript)
///   `out_dir/sessions/<hash>/<id>/wire.jsonl`     (timestamp + token-usage sidecar)
///
/// `<hash>` is `md5(cwd_utf8)` (the local-KAOS project-dir name kimi-cli uses); when there is no
/// cwd we fall back to `md5("")`. `<id>` is `opts.new_id` or a fresh uuid.
///
/// The transcript records are the exact shapes [`context_message`] parses back:
/// - `{"role":"_system_prompt","content": <text>}` for System turns,
/// - `{"role":"user","content": <bare string | [Part]>}` for User turns,
/// - `{"role":"assistant","content":[Part],"tool_calls":[…]}` for Assistant turns
///   (`tool_calls[].function.arguments` is a JSON-encoded *string*),
/// - `{"role":"tool","content": <text>,"tool_call_id": <id>}` for Tool turns.
///
/// Parts are `type`-tagged (`text`/`think`/`image_url`/`audio_url`/`video_url`). The `wire.jsonl`
/// sidecar carries a `metadata` header, one `StatusUpdate` per assistant turn (token usage +
/// `message_id`), one `ToolResult` per tool turn, and per-turn timestamps.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let hash = md5_hex(cwd_str.as_bytes());

    let session_dir = out_dir.join("sessions").join(&hash).join(&new_id);
    fs::create_dir_all(&session_dir).map_err(|e| anyhow::anyhow!("creating {}: {e}", session_dir.display()))?;

    // Base timestamp for any turn lacking its own.
    let base_ts = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);

    // ── context.jsonl (the transcript) ──
    let mut ctx_lines: Vec<Value> = Vec::new();
    // ── wire.jsonl (sidecar) ── header first, then per-turn enrichment events.
    let mut wire_lines: Vec<Value> = Vec::new();
    wire_lines.push(serde_json::json!({
        "type": "metadata",
        "protocol_version": "1.9",
    }));

    for msg in &session.messages {
        let ts = msg.timestamp.unwrap_or(base_ts);
        let epoch = dt_to_epoch(ts);
        match msg.role {
            Role::System => {
                let text = coerce_message_text(msg);
                ctx_lines.push(serde_json::json!({
                    "role": "_system_prompt",
                    "content": text,
                }));
                wire_lines.push(wire_event(epoch, "TurnBegin", serde_json::json!({})));
            }
            Role::User => {
                ctx_lines.push(serde_json::json!({
                    "role": "user",
                    "content": user_content(&msg.content),
                }));
                wire_lines.push(wire_event(epoch, "TurnBegin", serde_json::json!({})));
            }
            Role::Assistant => {
                let mut rec = serde_json::Map::new();
                rec.insert("role".into(), Value::String("assistant".into()));
                rec.insert("content".into(), Value::Array(content_parts(&msg.content)));
                let calls = tool_calls(&msg.content);
                if !calls.is_empty() {
                    rec.insert("tool_calls".into(), Value::Array(calls));
                }
                ctx_lines.push(Value::Object(rec));

                // StatusUpdate: usage + message_id ride here (attach_status_updates zips these
                // positionally onto assistant turns).
                let mut payload = serde_json::Map::new();
                if let Some(u) = &msg.usage {
                    payload.insert("token_usage".into(), usage_to_wire(u));
                }
                if let Some(id) = &msg.id {
                    payload.insert("message_id".into(), Value::String(id.clone()));
                }
                wire_lines.push(wire_event(epoch, "ContentPart", serde_json::json!({})));
                wire_lines.push(wire_event(epoch, "StatusUpdate", Value::Object(payload)));
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        details,
                        ..
                    } = b
                    {
                        ctx_lines.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                        // Richer ToolResult in wire (return_value{is_error,output,extras}).
                        let mut rv = serde_json::Map::new();
                        rv.insert("is_error".into(), Value::Bool(*is_error));
                        rv.insert("output".into(), Value::String(content.to_string()));
                        if let Some(d) = details {
                            rv.insert("extras".into(), d.clone());
                        }
                        let payload = serde_json::json!({
                            "tool_call_id": tool_use_id,
                            "return_value": Value::Object(rv),
                        });
                        wire_lines.push(wire_event(epoch, "ToolCall", serde_json::json!({})));
                        wire_lines.push(wire_event(epoch, "ToolResult", payload));
                    }
                }
            }
        }
    }

    let ctx_path = session_dir.join("context.jsonl");
    write_jsonl(&ctx_path, &ctx_lines)?;
    let wire_path = session_dir.join("wire.jsonl");
    write_jsonl(&wire_path, &wire_lines)?;

    // Register the work_dir in `kimi.json`. kimi-cli's `Session.find` returns None (→ silently
    // starts a fresh, empty session) unless the cwd is present in `work_dirs[]`, matched on exact
    // `path` + `kaos`. Local dirs use `kaos: "local"` and a session dir hash of md5(path) — which is
    // exactly what we wrote above. Without this, the ported session exists on disk but is invisible
    // to resume. Best-effort: never fatal.
    if !cwd_str.is_empty() {
        if let Err(e) = register_work_dir(out_dir, &cwd_str, &new_id) {
            eprintln!("cv: kimi work_dir registration skipped ({e}); resume may not find the session");
        }
    }

    let resume = match &cwd {
        Some(c) => format!("kimi --resume {new_id}  (run from {})", c.display()),
        None => format!("kimi --resume {new_id}"),
    };
    Ok(crate::harness::EmitResult {
        path: session_dir,
        new_id,
        resume_hint: Some(resume),
    })
}

/// Upsert a `work_dirs[]` entry in `<root>/kimi.json` so kimi-cli's `Session.find` can locate the
/// ported session for this cwd. Local dirs are keyed by `kaos: "local"`; we match/update on `path`
/// and refresh `last_session_id` (so `--continue` also reaches it). Creates kimi.json if absent.
fn register_work_dir(root: &Path, cwd: &str, session_id: &str) -> Result<()> {
    let kimi_json = root.join("kimi.json");
    let mut doc: Value = match fs::read_to_string(&kimi_json) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    if !doc.is_object() {
        doc = serde_json::json!({});
    }
    let obj = doc.as_object_mut().expect("doc is object");
    let dirs = obj.entry("work_dirs").or_insert_with(|| Value::Array(Vec::new()));
    let arr = match dirs.as_array_mut() {
        Some(a) => a,
        None => {
            *dirs = Value::Array(Vec::new());
            dirs.as_array_mut().unwrap()
        }
    };
    // Update existing local entry for this path, else append.
    let existing = arr.iter_mut().find(|e| {
        e.get("path").and_then(Value::as_str) == Some(cwd) && e.get("kaos").and_then(Value::as_str) == Some("local")
    });
    match existing {
        Some(e) => {
            if let Some(m) = e.as_object_mut() {
                m.insert("last_session_id".into(), Value::String(session_id.to_string()));
            }
        }
        None => arr.push(serde_json::json!({
            "path": cwd,
            "kaos": "local",
            "last_session_id": session_id,
        })),
    }
    fs::write(&kimi_json, serde_json::to_string_pretty(&doc)?)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", kimi_json.display()))?;
    Ok(())
}

/// User `content`: a bare string for a single text block (matching how kimi-cli stores single-part
/// user turns, and exactly what `push_parts` round-trips), else a `[Part]` array.
fn user_content(content: &[Block]) -> Value {
    if let [Block::Text { text }] = content {
        return Value::String(text.to_string());
    }
    Value::Array(content_parts(content))
}

/// Map IR blocks → Kimi `type`-tagged Parts (the inverse of `part_to_block`). Tool-use blocks are
/// emitted via `tool_calls[]`, not here, so they're skipped.
fn content_parts(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(serde_json::json!({ "type": "text", "text": text })),
            Block::Thinking { text, encrypted, .. } => {
                let mut p = serde_json::Map::new();
                p.insert("type".into(), Value::String("think".into()));
                p.insert("think".into(), Value::String(text.to_string()));
                if let Some(enc) = encrypted {
                    p.insert("encrypted".into(), Value::String(enc.clone()));
                }
                out.push(Value::Object(p));
            }
            Block::Image { data_ref, .. } => {
                out.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": data_ref, "id": Value::Null },
                }));
            }
            Block::File { source, .. } => {
                // kimi nests audio/video media URLs under a type-named key; default to audio_url.
                out.push(serde_json::json!({
                    "type": "audio_url",
                    "audio_url": { "url": source, "id": Value::Null },
                }));
            }
            // Tool calls go in tool_calls[]; tool results are their own `tool` records.
            Block::ToolUse { .. } | Block::ToolResult { .. } => {}
        }
    }
    out
}

/// Assistant ToolUse blocks → `tool_calls[]` (`arguments` is a JSON-encoded *string*, as the parser
/// expects and `parse_arguments` decodes).
fn tool_calls(content: &[Block]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { id, name, input, .. } => Some(serde_json::json!({
                "type": "function",
                "id": id,
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input)
                        .unwrap_or_else(|_| "{}".to_string()),
                },
            })),
            _ => None,
        })
        .collect()
}

/// Flatten a message's text/thinking blocks to plain text, for `_system_prompt` content.
fn coerce_message_text(msg: &Message) -> String {
    let mut parts = Vec::new();
    for b in &msg.content {
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => parts.push(text.to_string()),
            _ => {}
        }
    }
    parts.join("\n")
}

/// One `wire.jsonl` event line: a top-level `timestamp` (read by `wire_time_bounds` and, for the
/// turn-boundary types `TurnBegin`/`ContentPart`/`ToolCall`, by `attach_timestamps`) wrapping a
/// `{type, payload}` message (mined by `read_wire_enrichment` for `StatusUpdate`/`ToolResult`).
fn wire_event(epoch: f64, mty: &str, payload: Value) -> Value {
    serde_json::json!({
        "timestamp": epoch,
        "message": { "type": mty, "payload": payload },
    })
}

/// IR usage → wire `token_usage` (inverse of `wire_usage`).
fn usage_to_wire(u: &Usage) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(v) = u.input_tokens {
        m.insert("input_other".into(), Value::Number(v.into()));
    }
    if let Some(v) = u.output_tokens {
        m.insert("output".into(), Value::Number(v.into()));
    }
    if let Some(v) = u.cache_read_tokens {
        m.insert("input_cache_read".into(), Value::Number(v.into()));
    }
    if let Some(v) = u.cache_creation_tokens {
        m.insert("input_cache_creation".into(), Value::Number(v.into()));
    }
    Value::Object(m)
}

/// `DateTime<Utc>` → epoch-seconds float (inverse of `epoch_to_dt`).
fn dt_to_epoch(t: DateTime<Utc>) -> f64 {
    t.timestamp_nanos_opt().unwrap_or(0) as f64 / 1e9
}

/// Write one JSON object per line (newline-terminated), like emit.rs's `write_jsonl`.
fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut buf = String::new();
    for v in lines {
        buf.push_str(&serde_json::to_string(v)?);
        buf.push('\n');
    }
    fs::write(path, buf).map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery helpers
// ---------------------------------------------------------------------------

/// Parse `kimi.json` into a map of project-dir basename → cwd. The basename is `md5(path)` for the
/// `local` KAOS, else `"<kaos>_<md5>"`, matching kimi-cli's `WorkDirMeta.sessions_dir`.
fn load_work_dirs(kimi_json: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(kimi_json) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    let Some(dirs) = v.get("work_dirs").and_then(Value::as_array) else {
        return map;
    };
    for wd in dirs {
        let Some(path) = wd.get("path").and_then(Value::as_str) else {
            continue;
        };
        let kaos = wd.get("kaos").and_then(Value::as_str).unwrap_or("local");
        let hash = md5_hex(path.as_bytes());
        let basename = if kaos == "local" {
            hash
        } else {
            format!("{kaos}_{hash}")
        };
        map.insert(basename, PathBuf::from(path));
    }
    map
}

/// `default_model` from `config.toml`. We don't pull in a TOML parser for a single scalar: scan for
/// the top-level `default_model = "…"` line.
fn default_model_from_config(config_toml: &Path) -> Option<String> {
    let text = fs::read_to_string(config_toml).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("default_model") {
            let rest = rest.trim_start().strip_prefix('=')?.trim();
            let val = rest.trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Cheap scan of one transcript into a `SessionRef`. `dir` is the session's source dir (the
/// `<uuid>/` dir for modern, the hash dir for legacy). Returns `None` only on a fully unreadable
/// transcript path that doesn't exist.
fn scan(
    id: &str,
    transcript: &Path,
    dir: &Path,
    cwd: Option<PathBuf>,
    _default_model: Option<String>,
) -> Option<SessionRef> {
    let meta = fs::metadata(transcript).ok()?;
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = read_custom_title(dir);
    let mut message_count = 0usize;
    let text = fs::read_to_string(transcript).unwrap_or_default();
    super::for_each_json_line_str(&text, |v| {
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        // SessionRef contract: user + assistant turns only (no tool / _system_prompt records).
        if matches!(role, "user" | "assistant") {
            message_count += 1;
        }
        if title.is_none() && role == "user" {
            let t = coerce_content_text(v.get("content"));
            if !t.trim().is_empty() {
                title = Some(crate::ir::truncate(&t, 80));
            }
        }
        Flow::Continue
    });

    // wire gives nicer first/last timestamps when present.
    let wire = wire_path_for(dir, transcript);
    let (created, updated) = wire
        .as_ref()
        .and_then(|w| wire_time_bounds(w))
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((mtime, mtime));

    Some(SessionRef {
        id: id.to_string(),
        harness: Harness::Kimi,
        path: dir.to_path_buf(),
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count,
    })
}

/// `state.json.custom_title`, when present and non-empty.
fn read_custom_title(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("custom_title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80))
}

/// Given a `SessionRef.path` and id, find the transcript + optional wire sidecar.
fn locate_transcript(dir: &Path, id: &str) -> (PathBuf, Option<PathBuf>) {
    let modern = dir.join("context.jsonl");
    if modern.exists() {
        let wire = dir.join("wire.jsonl");
        return (modern, wire.exists().then_some(wire));
    }
    // Legacy flat: dir is the hash dir, transcript is <id>.jsonl, no wire.
    let flat = dir.join(format!("{id}.jsonl"));
    (flat, None)
}

/// Locate the wire sidecar for a transcript, if it lives next to it (modern only).
fn wire_path_for(dir: &Path, transcript: &Path) -> Option<PathBuf> {
    if transcript.file_name().and_then(|n| n.to_str()) == Some("context.jsonl") {
        let w = dir.join("wire.jsonl");
        return w.exists().then_some(w);
    }
    None
}

/// Count archived pre-compaction segments `context_1.jsonl…` next to a modern transcript.
fn count_segments(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("context_") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .count() as u64
}

// ---------------------------------------------------------------------------
// context.jsonl → IR
// ---------------------------------------------------------------------------

/// Convert one `context.jsonl` record into a `Message`, or `None` to skip it.
fn context_message(v: &Value, enrich: &WireEnrich) -> Option<Message> {
    let role = v.get("role").and_then(Value::as_str)?;
    match role {
        "_checkpoint" | "_usage" => None,
        "_system_prompt" => {
            let text = coerce_content_text(v.get("content"));
            let mut m = Message::new(Role::System);
            m.content.push(Block::Text { text: text.into() });
            Some(m)
        }
        "user" => {
            let mut m = Message::new(Role::User);
            push_parts(&mut m, v.get("content"));
            (!m.content.is_empty()).then_some(m)
        }
        "assistant" => {
            let mut m = Message::new(Role::Assistant);
            push_parts(&mut m, v.get("content"));
            if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let func = c.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input = parse_arguments(func.and_then(|f| f.get("arguments")));
                    m.content.push(Block::ToolUse {
                        id,
                        name,
                        input,
                        namespace: None,
                    });
                }
            }
            (!m.content.is_empty()).then_some(m)
        }
        "tool" => {
            let tool_use_id = v.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
            // Prefer the richer wire return_value if we have it; else the context text parts.
            let we = enrich.tool_results.get(&tool_use_id);
            let (content, is_error, details) = match we {
                Some(tr) => (tr.output.clone(), tr.is_error, tr.extras.clone()),
                None => (coerce_content_text(v.get("content")), false, None),
            };
            let status = Some(if is_error { "error" } else { "completed" }.to_string());
            let mut m = Message::new(Role::Tool);
            m.content.push(Block::ToolResult {
                tool_use_id,
                content: content.into(),
                is_error,
                tool_name: None,
                status,
                details,
            });
            Some(m)
        }
        _ => None,
    }
}

/// Push a record's `content` (bare string or `[Part]`) onto a message as Blocks.
fn push_parts(m: &mut Message, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                m.content.push(Block::Text { text: s.clone().into() });
            }
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(b) = part_to_block(p) {
                    m.content.push(b);
                }
            }
        }
        _ => {}
    }
}

/// One `type`-tagged Part → a `Block`.
fn part_to_block(p: &Value) -> Option<Block> {
    let ty = p.get("type").and_then(Value::as_str)?;
    match ty {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            Some(Block::Text { text: text.into() })
        }
        "think" => {
            let text = p.get("think").and_then(Value::as_str).unwrap_or("").to_string();
            let encrypted = p.get("encrypted").and_then(Value::as_str).map(str::to_string);
            Some(Block::Thinking {
                text: text.into(),
                signature: encrypted.clone(),
                encrypted,
                redacted: false,
            })
        }
        "image_url" => {
            let url = media_url(p, "image_url");
            Some(Block::Image {
                media_type: None,
                data_ref: url,
            })
        }
        "audio_url" | "video_url" => {
            let url = media_url(p, ty);
            Some(Block::File {
                mime: None,
                path: None,
                source: url,
            })
        }
        _ => None,
    }
}

/// Media parts nest `{url, id}` under a key named after the type, e.g. `image_url: {url, id}`.
fn media_url(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(|inner| inner.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `arguments` is a JSON-encoded *string*. Parse it; `null`/empty/unparseable ⇒ `{}`.
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if !s.is_empty() => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        Some(Value::Object(_)) => v.cloned().unwrap(),
        _ => Value::Object(Default::default()),
    }
}

/// Flatten a record `content` (string or `[Part]`) to plain text (text parts joined by newline).
fn coerce_content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("think") => p.get("think").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// wire.jsonl sidecar enrichment
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WireEnrich {
    /// tool_call_id → richer result from `ToolResult.return_value`.
    tool_results: HashMap<String, WireToolResult>,
    /// Per assistant step, in wire order.
    statuses: Vec<WireStatus>,
    /// Timestamps of `TurnBegin` and `ContentPart`/`ToolCall` events, in order (best-effort).
    msg_timestamps: Vec<DateTime<Utc>>,
}

struct WireToolResult {
    is_error: bool,
    output: String,
    extras: Option<Value>,
}

#[derive(Default)]
struct WireStatus {
    usage: Option<Usage>,
    message_id: Option<String>,
}

/// Parse `wire.jsonl`. Tolerant of `protocol_version` drift and unknown message types.
fn read_wire_enrichment(wire: &PathBuf) -> WireEnrich {
    let mut e = WireEnrich::default();
    let Ok(text) = fs::read_to_string(wire) else {
        return e;
    };
    super::for_each_json_line_str(&text, |v| {
        // header line: {"type":"metadata","protocol_version":"1.x"}
        if v.get("type").and_then(Value::as_str) == Some("metadata") {
            return Flow::Continue;
        }
        let Some(msg) = v.get("message") else {
            return Flow::Continue;
        };
        let mty = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = msg.get("payload");
        match mty {
            "ToolResult" => {
                if let Some(p) = payload {
                    let id = p.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let rv = p.get("return_value");
                    let is_error = rv
                        .and_then(|r| r.get("is_error"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Prefer `output`, fall back to `message`.
                    let output = rv
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_str)
                        .or_else(|| rv.and_then(|r| r.get("message")).and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let extras = rv.and_then(|r| r.get("extras")).filter(|x| !x.is_null()).cloned();
                    if !id.is_empty() {
                        e.tool_results.insert(
                            id,
                            WireToolResult {
                                is_error,
                                output,
                                extras,
                            },
                        );
                    }
                }
            }
            "StatusUpdate" => {
                if let Some(p) = payload {
                    let usage = p.get("token_usage").map(wire_usage);
                    let message_id = p.get("message_id").and_then(Value::as_str).map(str::to_string);
                    e.statuses.push(WireStatus { usage, message_id });
                }
            }
            _ => {}
        }
        // Record a timestamp for content-bearing turn boundaries.
        if matches!(mty, "TurnBegin" | "ContentPart" | "ToolCall") {
            if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
                e.msg_timestamps.push(ts);
            }
        }
        Flow::Continue
    });
    e
}

fn wire_usage(tu: &Value) -> Usage {
    let g = |k: &str| tu.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: g("input_other"),
        output_tokens: g("output"),
        cache_read_tokens: g("input_cache_read"),
        cache_creation_tokens: g("input_cache_creation"),
        reasoning_tokens: None,
        cost_usd: None,
    }
}

/// First and last event timestamps in a wire file (epoch floats), for created/updated bounds.
fn wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let text = fs::read_to_string(wire).ok()?;
    let mut first = None;
    let mut last = None;
    super::for_each_json_line_str(&text, |v| {
        if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
            if first.is_none() {
                first = Some(ts);
            }
            last = Some(ts);
        }
        Flow::Continue
    });
    Some((first?, last?))
}

fn epoch_to_dt(secs: f64) -> Option<DateTime<Utc>> {
    let nanos = (secs * 1e9).round() as i64;
    Some(DateTime::<Utc>::from_timestamp_nanos(nanos))
}

// ---------------------------------------------------------------------------
// md5 (dependency-free; only used for hashing short cwd strings)
// ---------------------------------------------------------------------------

/// Lowercase hex md5 of `data`. A small self-contained RFC 1321 implementation so we don't add a
/// crate dependency just to match kimi-cli's project-dir naming.
fn md5_hex(data: &[u8]) -> String {
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    // padding
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_reference() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        // The real cwd → project-dir hash observed in ~/.kimi.
        assert_eq!(
            md5_hex(b"/Users/ember/elide/breadstuffs"),
            "a917b40bc491ae8a7f6f508be5446785"
        );
    }

    #[test]
    fn work_dirs_map_local_and_kaos() {
        let json = r#"{"work_dirs":[
            {"path":"/Users/ember/elide/breadstuffs","kaos":"local","last_session_id":"x"},
            {"path":"/remote/proj","kaos":"box","last_session_id":null}
        ]}"#;
        let dir = write_temp("kimi.json", json);
        let map = load_work_dirs(&dir.join("kimi.json"));
        assert_eq!(
            map.get("a917b40bc491ae8a7f6f508be5446785").unwrap(),
            &PathBuf::from("/Users/ember/elide/breadstuffs")
        );
        // non-local KAOS → "<kaos>_<md5>"
        let h = md5_hex(b"/remote/proj");
        assert_eq!(map.get(&format!("box_{h}")).unwrap(), &PathBuf::from("/remote/proj"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_default_model() {
        let toml = "theme = \"dark\"\ndefault_model = \"kimi-code/kimi-for-coding\"\nfoo = 1\n";
        let dir = write_temp("config.toml", toml);
        assert_eq!(
            default_model_from_config(&dir.join("config.toml")).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_bare_string_and_list() {
        let bare: Value = serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        let m = context_message(&bare, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let list: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA","id":null}}]}"#,
        )
        .unwrap();
        let m = context_message(&list, &WireEnrich::default()).unwrap();
        assert_eq!(m.text().as_deref(), Some("hi"));
        assert!(matches!(
            m.content[1],
            Block::Image {
                ref data_ref,
                ..
            } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        ));
    }

    #[test]
    fn assistant_think_and_tool_calls() {
        let line = r#"{"role":"assistant","content":[{"type":"think","think":"reasoning","encrypted":"BLOB"},{"type":"text","text":"Let me look."}],"tool_calls":[{"type":"function","id":"tool_1","function":{"name":"Shell","arguments":"{\"command\":\"cat .gitmodules\"}"}}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = context_message(&v, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::Assistant);
        match &m.content[0] {
            Block::Thinking {
                text,
                encrypted,
                signature,
                ..
            } => {
                assert_eq!(text, "reasoning");
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(signature.as_deref(), Some("BLOB"));
            }
            o => panic!("expected thinking, got {o:?}"),
        }
        assert!(matches!(&m.content[1], Block::Text { text } if text == "Let me look."));
        match &m.content[2] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Shell");
                assert_eq!(input["command"], "cat .gitmodules");
            }
            o => panic!("expected tool_use, got {o:?}"),
        }
    }

    #[test]
    fn tool_result_bare_string_and_wire_enrichment() {
        // context.jsonl tool record (bare-string content), no wire → uses context text.
        let line =
            r#"{"role":"tool","content":"<system>Command executed successfully.</system>","tool_call_id":"tool_1"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = context_message(&v, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                status,
                ..
            } => {
                assert_eq!(tool_use_id, "tool_1");
                assert!(content.contains("Command executed successfully"));
                assert!(!is_error);
                assert_eq!(status.as_deref(), Some("completed"));
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        // With wire enrichment, output/is_error come from return_value.
        let mut enrich = WireEnrich::default();
        enrich.tool_results.insert(
            "tool_1".into(),
            WireToolResult {
                is_error: true,
                output: "boom".into(),
                extras: Some(serde_json::json!({"k":"v"})),
            },
        );
        let m = context_message(&v, &enrich).unwrap();
        match &m.content[0] {
            Block::ToolResult {
                content,
                is_error,
                status,
                details,
                ..
            } => {
                assert_eq!(content, "boom");
                assert!(is_error);
                assert_eq!(status.as_deref(), Some("error"));
                assert_eq!(details.as_ref().unwrap()["k"], "v");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_empty_becomes_object() {
        assert_eq!(parse_arguments(None), Value::Object(Default::default()));
        assert_eq!(
            parse_arguments(Some(&Value::String("".into()))),
            Value::Object(Default::default())
        );
        assert_eq!(
            parse_arguments(Some(&Value::String("not json".into()))),
            Value::Object(Default::default())
        );
    }

    #[test]
    fn internal_roles_skipped() {
        for line in [
            r#"{"role":"_checkpoint","id":3}"#,
            r#"{"role":"_usage","token_count":7291}"#,
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(context_message(&v, &WireEnrich::default()).is_none());
        }
        let sp: Value = serde_json::from_str(r#"{"role":"_system_prompt","content":"You are Kimi."}"#).unwrap();
        let m = context_message(&sp, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::System);
        assert_eq!(m.text().as_deref(), Some("You are Kimi."));
    }

    #[test]
    fn stream_matches_parse_on_modern_fixture() {
        // parse() is collect(stream()); the streamed messages must carry the same wire enrichment
        // (StatusUpdate usage/message_id on assistant turns, first wire timestamp on the first user
        // turn) that the full parse reports. This was a real divergence: the positional passes used
        // to run only in parse(), so streaming consumers saw assistant turns without usage.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/11111111-1111-1111-1111-111111111111");
        if !dir.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let adapter = Kimi {
            sessions: None,
            root: None,
        };
        let parsed = adapter.parse(&r).unwrap();

        let mut sink = crate::stream::CollectSink::default();
        let mut streamed = adapter
            .stream(&r, &crate::stream::ParseOptions::full(), &mut sink)
            .unwrap();
        streamed.messages = sink.messages;

        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "kimi parse() and collect(stream()) must agree"
        );
        // And the enrichment is actually present on the streamed side (not just both-empty).
        let asst = streamed.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        assert_eq!(asst.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(asst.id.as_deref(), Some("chatcmpl-abc"));
        let first_user = streamed.messages.iter().find(|m| m.role == Role::User).unwrap();
        assert!(
            first_user.timestamp.is_some(),
            "first user turn gets the first wire timestamp on the streaming path too"
        );
    }

    #[test]
    fn parses_fixture_modern_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/11111111-1111-1111-1111-111111111111");
        if !dir.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        // assistant with a tool_use, and a tool result enriched from wire (is_error=false).
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        let tr = s
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .and_then(|m| m.content.first());
        match tr {
            Some(Block::ToolResult { content, is_error, .. }) => {
                assert!(content.contains("README.md"));
                assert!(!is_error);
            }
            _ => panic!("expected tool result"),
        }
        // assistant message picked up usage + message_id from wire StatusUpdate.
        let asst = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        assert_eq!(asst.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(asst.id.as_deref(), Some("chatcmpl-abc"));
    }

    #[test]
    fn parses_fixture_legacy_flat_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/legacy/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        if !dir.exists() {
            return;
        }
        let id = "22222222-2222-2222-2222-222222222222";
        let r = SessionRef {
            id: id.into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
    }

    /// A unique temp dir for emit round-trip tests (no external tempfile crate).
    fn emit_temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("cv-kimi-emit-{}-{}", std::process::id(), n));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn emit_round_trips_through_parse() {
        // Build a small session: system + user + assistant(think+text+tool_use) + tool result.
        let mut sys = Message::new(Role::System);
        sys.content.push(Block::Text {
            text: "You are Kimi.".into(),
        });

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "list files".into(),
        });

        let mut asst = Message::new(Role::Assistant);
        asst.id = Some("chatcmpl-xyz".into());
        asst.usage = Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(42),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
        });
        asst.content.push(Block::Thinking {
            text: "let me think".into(),
            signature: Some("BLOB".into()),
            encrypted: Some("BLOB".into()),
            redacted: false,
        });
        asst.content.push(Block::Text {
            text: "Running ls.".into(),
        });
        asst.content.push(Block::ToolUse {
            id: "tool_1".into(),
            name: "Shell".into(),
            input: serde_json::json!({ "command": "ls" }),
            namespace: None,
        });

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "tool_1".into(),
            content: "README.md\nsrc".into(),
            is_error: false,
            tool_name: None,
            status: Some("completed".into()),
            details: None,
        });

        let session = Session {
            id: "orig".into(),
            harness: Harness::Kimi,
            cwd: Some(PathBuf::from("/proj")),
            title: Some("demo".into()),
            created_at: Some(DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()),
            updated_at: None,
            model: None,
            git: None,
            messages: vec![sys, user, asst, tool],
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        let out = emit_temp_dir();
        let res = emit(&session, &out, &crate::emit::EmitOptions::default()).unwrap();
        // emit writes the session dir; both transcript + sidecar must exist.
        assert!(res.path.join("context.jsonl").exists());
        assert!(res.path.join("wire.jsonl").exists());

        // Re-parse with THIS adapter (path is the session dir, id is the new id).
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Kimi,
            path: res.path.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();

        // Message count + roles round-trip.
        assert_eq!(parsed.messages.len(), 4);
        let roles: Vec<Role> = parsed.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::System, Role::User, Role::Assistant, Role::Tool]);

        // First text (system prompt) round-trips.
        assert_eq!(parsed.messages[0].text().as_deref(), Some("You are Kimi."));
        assert_eq!(parsed.messages[1].text().as_deref(), Some("list files"));

        // Assistant: thinking + text + tool_use survive; usage + message_id come back via wire.
        let a = &parsed.messages[2];
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { text, .. } if text == "let me think")));
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "Running ls.")));
        match a.content.iter().find(|b| matches!(b, Block::ToolUse { .. })).unwrap() {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Shell");
                assert_eq!(input["command"], "ls");
            }
            _ => unreachable!(),
        }
        assert_eq!(a.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(a.id.as_deref(), Some("chatcmpl-xyz"));

        // Tool result round-trips (content from context, enriched by wire).
        match parsed.messages[3].content.first().unwrap() {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "tool_1");
                assert!(content.contains("README.md"));
                assert!(!is_error);
            }
            _ => unreachable!(),
        }

        let _ = fs::remove_dir_all(&out);
    }

    /// Write `name`→`contents` into a fresh temp dir, return the dir.
    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-kimi-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }
}
