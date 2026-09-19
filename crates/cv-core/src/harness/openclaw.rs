//! OpenClaw adapter — `~/.openclaw/agents/<agentId>/sessions/`.
//!
//! See `docs/FORMATS.md`. A `sessions.json` index maps session keys → metadata; each session is a
//! `<sessionId>.jsonl` (or `<sessionId>-topic-<topicId>.jsonl`) transcript.
//!
//! ## On-disk shape (reverse-engineered from the OpenClaw TS source)
//!
//! Source of truth lives in `openclaw/packages/agent-core/src/llm.ts` (the LLM message union),
//! `.../harness/messages.ts` (the custom-message declaration-merge union), and
//! `openclaw/src/config/sessions/transcript*.ts` (read/write/append).
//!
//! - **Header line** (always first): `{type:"session", version:N, id, timestamp(ISO), cwd}`.
//!   `CURRENT_SESSION_VERSION = 3`. Older transcripts: **v1/v2 are "linear"** (entries have no
//!   `parentId`); on first append OpenClaw migrates them in place to **v3 "parent-linked"** by
//!   assigning each entry a `parentId` pointing at the prior entry's `id` (root = `null`). So on
//!   disk you'll see a mix: v3 with `parentId` threading, and legacy v1/v2 without it.
//! - **Message line**: `{type:"message", id, parentId, timestamp(ISO), message:{…}}`. `id` is an
//!   8-hex-char slice of a UUID (or full UUID on collision). `message.timestamp` is epoch-ms.
//! - **`message.role`** is the AgentMessage union discriminator:
//!   - `user`     — content: `string | (TextContent|ImageContent)[]`.
//!   - `assistant`— content: `(TextContent|ThinkingContent|ToolCall)[]`; plus `api`, `provider`,
//!     `model`, `responseModel?`, `responseId?`, `stopReason`, `errorMessage?`, `diagnostics?`,
//!     `usage{input,output,cacheRead,cacheWrite,totalTokens,cost{…}}`.
//!   - `toolResult` — `{toolCallId, toolName, content:(TextContent|ImageContent)[], isError,
//!     details?}`.
//!   - **Custom (declaration-merged) roles** persisted alongside normal history:
//!     `bashExecution`, `branchSummary`, `compactionSummary`, `custom` (with `customType`, e.g.
//!     `openclaw.cache-ttl`). These are NOT LLM messages; OpenClaw flattens them to user text only
//!     when building the model context.
//! - **Content blocks**: `text{text, textSignature?}`, `thinking{thinking, thinkingSignature?,
//!   redacted?}`, `toolCall{id,name,arguments,thoughtSignature?,executionMode?}`,
//!   `image{data, mimeType}`.
//! - **Secret redaction** is applied at write time (`redactTranscriptMessage`): secrets become
//!   `prefix…suffix` / `***` / `…redacted…` (PEM). There is no fixed sentinel string, so we cannot
//!   reliably detect/flag redactions — they just look like truncated values.
//! - **ACP-bridged sessions** (OpenClaw fronting an external agent like Claude Code / Codex via the
//!   ACP control plane): the rich turn (tool calls, thinking, real usage) lives in the *external*
//!   agent's own store, NOT here. OpenClaw only persists a `user` prompt and a single text
//!   `assistant` reply tagged `provider:"openclaw", model:"acp-runtime"`. Embedded-CLI turns use
//!   `api:"cli"` with the real provider/model. Delivery mirrors use
//!   `provider:"openclaw", model:"delivery-mirror"` (or `gateway-injected`) — transcript-only echoes.
//!
//! ## SQLite store (OpenClaw ≥ 2026-07-11, commit `0a8e3604ba` "flip sessions and transcripts to
//! sqlite storage")
//!
//! Live sessions and transcripts now live in `<stateDir>/agents/<agentId>/agent/openclaw-agent.sqlite`
//! (`src/state/openclaw-agent-db.paths.ts:39`; shared stores use `openclaw-agent.<x>.sqlite`), schema
//! `src/state/openclaw-agent-schema.sql` (all tables `STRICT`, `OPENCLAW_AGENT_SCHEMA_VERSION = 21`):
//! - `transcript_events(session_id, seq, event_json, created_at)` — `event_json` is
//!   `JSON.stringify(event)` of EXACTLY the objects that used to be JSONL lines (header + entries;
//!   `session-accessor.sqlite-transcript-store.ts:207-211`), read back `ORDER BY seq ASC`
//!   (`session-accessor.sqlite-read.ts:220-223`). So the sqlite path is the JSONL parser fed rows.
//! - `session_windows(session_id PK, session_key, previous_session_id, reason, created_at, updated_at,
//!   transcript_updated_at, model_provider, model, display_name, parent_session_key, spawned_by, …)` —
//!   one row per transcript (a reset/rollover/fork opens a new window under the same key).
//! - `session_nodes(session_key PK, current_session_id, entry_json, label, display_name,
//!   parent_session_key, fork_source_session_id, archived_at, …)` — `entry_json` is the old
//!   `sessions.json` entry (`{sessionId, updatedAt, cwd?, label?, spawnedCwd?, spawnedWorkspaceDir?, …}`).
//!
//! `sessions.json` is a legacy discovery target only. JSONL files on disk are now: pre-July
//! transcripts (still read), and archives that are NOT sessions — `<sid>.checkpoint.<uuid>.jsonl`
//! (compaction checkpoints), `*.trajectory.jsonl`, `<sid>.jsonl.<reset|deleted|bak>.<ts>[.zst]`,
//! `*.migrated`, `*.pre-doctor-*.bak` and cold-tier `<sha256>.jsonl.zst` (`artifacts.ts`,
//! `session-cold-storage-worker.ts:241`). Discovery applies OpenClaw's own
//! `isPrimarySessionTranscriptFileName` rule and skips `.zst` (archives of reset/deleted history,
//! not live sessions). A session present in both stores is taken from sqlite.
//!
//! ## Transcript tree (v4, `transcript-tree.ts`)
//!
//! `CURRENT_SESSION_VERSION = 4` (`version.ts`; v3 stays readable; the header may carry
//! `parentSession`). Entries form a tree: `{type:"leaf", id, parentId, targetId, appendParentId?,
//! appendMode?}` is a navigation control selecting `targetId` as the active leaf (rewind/branch);
//! any canonical entry may carry `appendMode:"side"` (written past the visible leaf without moving
//! it). Readers must replay the *visible path* (`transcript-visible-events.ts`,
//! `selectSessionTranscriptActiveEntries`), not file order. [`select_rows`] ports that scan; it only
//! runs when a transcript holds a `leaf` control (OpenClaw's own rule for legacy flat readers,
//! `selectSessionTranscriptLeafControlledPath`). Entries off the active branch are dropped by the
//! lean passes and carried, tagged `openclaw_inactive_branch`, under [`ParseOptions::complete`].
//!
//! Canonical entry types beyond `message` (`session-manager-types.ts:28-95`): `compaction`
//! (`summary, firstKeptEntryId, tokensBefore, details?, fromHook?`), `reset` (`reason:
//! new|reset|idle|daily|cron-stale, firstKeptEntryId?`), `branch_summary` (`fromId, summary`),
//! `custom_message` (`customType, content, display, details?` — in model context), `custom`
//! (`customType, data?` — extension state, NOT in context), `session_info` (`name` → the title),
//! `model_change` (`provider, modelId`), `thinking_level_change`, `label` (`targetId, label`).

use super::{parse_ts, ts_from_value, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
#[cfg(feature = "sqlite")]
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenClaw {
    roots: Vec<PathBuf>,
}

impl OpenClaw {
    pub fn new() -> Self {
        let mut roots = Vec::new();
        for base in [
            dirs::home_dir().map(|h| h.join(".openclaw")),
            dirs::home_dir().map(|h| h.join("elide-home").join(".openclaw")),
        ]
        .into_iter()
        .flatten()
        {
            let agents = base.join("agents");
            if agents.exists() {
                roots.push(agents);
            }
        }
        OpenClaw { roots }
    }
}

impl Default for OpenClaw {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for OpenClaw {
    fn harness(&self) -> Harness {
        Harness::OpenClaw
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        // Sessions the sqlite store owns; a legacy JSONL twin (pre-migration copy) is skipped.
        let mut seen: HashSet<String> = HashSet::new();
        for root in &self.roots {
            // 1. The sqlite store (OpenClaw ≥ 2026-07-11): agents/<agentId>/agent/openclaw-agent*.sqlite
            for db in sqlite_stores(root) {
                match discover_sqlite(&db) {
                    Ok(refs) => {
                        for r in refs {
                            seen.insert(r.id.clone());
                            out.push(r);
                        }
                    }
                    Err(e) => eprintln!("cv: skipping {}: {e:#}", db.display()),
                }
            }
            // 2. Legacy JSONL transcripts at agents/<agentId>/sessions/<sid>.jsonl.
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !is_primary_transcript_name(name) {
                    continue;
                }
                if path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) != Some("sessions") {
                    continue;
                }
                let index = load_index(path.parent().unwrap());
                match scan(path, &index) {
                    Ok(r) => {
                        if !seen.contains(&r.id) {
                            out.push(r);
                        }
                    }
                    Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        if is_sqlite_store_path(&r.path) {
            return stream_sqlite(r, opts, sink);
        }
        // A transcript with `leaf` controls is a tree: the visible branch must be selected over the
        // whole file (see [`select_rows`]), so load it. Everything else streams one record at a time,
        // peak memory O(largest line) — OpenClaw's own rule for flat readers.
        if file_has_leaf_controls(&r.path)? {
            let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
            let entries: Vec<Value> = text.lines().filter_map(parse_line).collect();
            return Ok(stream_rows(
                rows_from_values(entries, opts.complete).into_iter(),
                r,
                opts,
                sink,
            ));
        }
        // `filter_map`, not `map_while`: a single undecodable line (stray non-UTF8 bytes) must be
        // skipped like any other corrupt record — `map_while` would silently TRUNCATE the whole
        // rest of the transcript at it. The lint's run-forever concern doesn't apply: on a regular
        // file an invalid-UTF8 `Err` still consumes that line's bytes, so the iterator advances to
        // EOF (same tolerance as `for_each_json_line`).
        #[allow(clippy::lines_filter_map_ok)]
        let lines = {
            let file = fs::File::open(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
            BufReader::new(file).lines().filter_map(Result::ok)
        };
        let rows = lines.filter_map(|l| parse_line(&l)).map(Row::active);
        Ok(stream_rows(rows, r, opts, sink))
    }
}

/// One parsed JSONL line / `transcript_events` row (whitespace-trimmed, non-JSON skipped).
fn parse_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(line).ok()
}

/// Does this JSONL transcript hold a `leaf` navigation control? A cheap substring pass (OpenClaw
/// writes `JSON.stringify` output — no spaces around the colon) so flat transcripts keep streaming.
fn file_has_leaf_controls(path: &Path) -> Result<bool> {
    let file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    #[allow(clippy::lines_filter_map_ok)]
    let found = BufReader::new(file)
        .lines()
        .filter_map(Result::ok)
        .any(|l| l.contains("\"type\":\"leaf\""));
    Ok(found)
}

/// A transcript entry on its way to the sink, with the branch selection's verdict.
struct Row {
    v: Value,
    /// On the visible branch (always true when no `leaf` control exists).
    active: bool,
    /// Parent id after active-branch normalization (`Some(_)` when the tree rewrote it).
    parent: Option<Option<String>>,
}

impl Row {
    fn active(v: Value) -> Self {
        Row {
            v,
            active: true,
            parent: None,
        }
    }
}

/// Rows for a fully-loaded transcript: the branch-selected view when a `leaf` control exists, else
/// every entry in file order.
fn rows_from_values(entries: Vec<Value>, complete: bool) -> Vec<Row> {
    if entries.iter().any(is_leaf_control) {
        select_rows(entries, complete)
    } else {
        entries.into_iter().map(Row::active).collect()
    }
}

/// Core parse, split out so tests can drive it from a fixture string. Full fidelity (collects).
/// Test-only: production reads go through [`Adapter::stream`]/[`Adapter::parse`] → [`stream_rows`].
#[cfg(test)]
fn parse_text(text: &str, r: &SessionRef) -> Session {
    parse_text_with(text, r, &ParseOptions::full())
}

#[cfg(test)]
fn parse_text_with(text: &str, r: &SessionRef, opts: &ParseOptions) -> Session {
    let mut sink = crate::stream::CollectSink::default();
    let entries: Vec<Value> = text.lines().filter_map(parse_line).collect();
    let mut s = stream_rows(rows_from_values(entries, opts.complete).into_iter(), r, opts, &mut sink);
    s.messages = sink.messages;
    s
}

/// Streaming core shared by the on-disk [`Adapter::stream`] (JSONL and sqlite) and the string-driven
/// `parse_text`: fold each record's metadata into the session and emit any message to `sink`.
/// Returns the session with empty `messages`.
fn stream_rows<I: Iterator<Item = Row>>(
    rows: I,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut s = Session {
        id: r.id.clone(),
        harness: Harness::OpenClaw,
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
    };
    let mut header_version: Option<i64> = None;
    let mut meta_sent = false;

    for row in rows {
        let v = row.v;
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "session" {
            header_version = v.get("version").and_then(Value::as_i64);
            if s.cwd.is_none() {
                s.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            if s.created_at.is_none() {
                s.created_at = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            }
            // v4 fork lineage: the transcript this one was branched from.
            if let Some(parent) = v.get("parentSession").and_then(Value::as_str) {
                s.extra.insert("openclaw_parent_session".into(), Value::from(parent));
            }
            continue;
        }
        // Off the visible branch: gone for the lean passes, carried (tagged) under `complete`.
        if !row.active && !opts.complete {
            continue;
        }
        let msg = match ty {
            "message" => parse_entry(&v),
            "compaction" | "reset" | "branch_summary" | "custom_message" => parse_control_entry(&v),
            // Session-level facts with a first-class home; the record itself only rides under
            // `complete`.
            "session_info" => {
                if let Some(name) = v.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()) {
                    s.title = Some(crate::ir::truncate(name, 80));
                    s.extra.insert("openclaw_session_name".into(), Value::from(name));
                }
                carrier_if_complete(&v, opts)
            }
            "model_change" => {
                if s.model.is_none() {
                    if let Some(model) = v.get("modelId").and_then(Value::as_str) {
                        s.model = Some(model.to_string());
                    }
                }
                carrier_if_complete(&v, opts)
            }
            "label" => {
                if let (Some(target), Some(label)) = (
                    v.get("targetId").and_then(Value::as_str),
                    v.get("label").and_then(Value::as_str),
                ) {
                    let labels = s
                        .extra
                        .entry("openclaw_labels")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Some(map) = labels.as_object_mut() {
                        map.insert(target.to_string(), Value::from(label));
                    }
                }
                carrier_if_complete(&v, opts)
            }
            // `custom` (extension state, not in context), `thinking_level_change`, `leaf`
            // navigation controls and anything newer: bookkeeping, carried only under `complete`.
            _ => carrier_if_complete(&v, opts),
        };
        let Some(mut m) = msg else {
            continue;
        };
        if let Some(parent) = &row.parent {
            m.parent_id = parent.clone();
        }
        if !row.active {
            m.extra.insert("openclaw_inactive_branch".into(), Value::Bool(true));
        }
        // The session model is the first real model we see; ignore the synthetic openclaw
        // transcript-only providers (delivery mirrors / acp-runtime).
        if s.model.is_none() {
            if let Some(model) = m.model.as_deref() {
                if !is_synthetic_openclaw_model(model) {
                    s.model = Some(model.to_string());
                }
            }
        }
        // Stash the transcript schema version on the first message so a downstream consumer can
        // tell v1/v2 (linear) from v3/v4 (parent-linked) sessions.
        if let Some(ver) = header_version.take() {
            m.extra.insert("openclaw_session_version".into(), Value::from(ver));
        }
        // Hand session metadata to the sink before the first message (header consumers).
        if !meta_sent {
            sink.meta(&s);
            meta_sent = true;
        }
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    if !meta_sent {
        sink.meta(&s);
    }
    s
}

/// A non-`message` canonical entry that carries context or a boundary the reader should see:
/// `compaction` (the summary that replaced the compacted span), `reset` (a session boundary),
/// `branch_summary`, `custom_message` (extension text that IS in model context). System turns with
/// the entry's fields in `extra` (snake_case) and `openclaw_entry_type` naming the kind.
fn parse_control_entry(v: &Value) -> Option<Message> {
    let ty = v.get("type").and_then(Value::as_str)?;
    let mut m = Message::new(Role::System);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    m.extra.insert("openclaw_entry_type".into(), Value::from(ty));
    let text = match ty {
        "compaction" | "branch_summary" => v.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
        "reset" => format!(
            "[session reset: {}]",
            v.get("reason").and_then(Value::as_str).unwrap_or("reset")
        ),
        _ => coerce_content_text(v.get("content")),
    };
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }
    for key in [
        "firstKeptEntryId",
        "tokensBefore",
        "fromHook",
        "details",
        "reason",
        "fromId",
        "customType",
        "display",
        "appendMode",
    ] {
        if let Some(val) = v.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
    Some(m)
}

/// Under [`ParseOptions::complete`], a bookkeeping entry (`session_info`, `model_change`, `label`,
/// `custom`, `thinking_level_change`, `leaf`, …) rides along verbatim as an empty System message
/// carrying the raw record in `extra["openclaw_entry"]`, so nothing is lost; the lean passes drop it.
fn carrier_if_complete(v: &Value, opts: &ParseOptions) -> Option<Message> {
    if !opts.complete {
        return None;
    }
    let mut m = Message::new(Role::System);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    if let Some(ty) = v.get("type").and_then(Value::as_str) {
        m.extra.insert("openclaw_entry_type".into(), Value::from(ty));
    }
    m.extra.insert("openclaw_entry".into(), v.clone());
    Some(m)
}

/// Models OpenClaw writes for transcript-only / bridged turns that should not be reported as the
/// session's "real" model.
fn is_synthetic_openclaw_model(model: &str) -> bool {
    matches!(model, "delivery-mirror" | "gateway-injected" | "acp-runtime")
}

/// OpenClaw's `isPrimarySessionTranscriptFileName` (`artifacts.ts`): a live session transcript is a
/// `.jsonl` that is not a trajectory runtime log (`*.trajectory.jsonl`), not a compaction checkpoint
/// (`<sid>.checkpoint.<uuid>.jsonl`, or the pre-2026-07 `<sid>-compaction-<id>.jsonl`), and not an
/// archive (`<sid>.jsonl.<reset|deleted|bak>.<ts>[.<gen>][.zst]`, `sessions.json.bak.N`,
/// `*.migrated[.N]`, `*.pre-doctor-*.bak` — none of which end in `.jsonl`, but `.zst` cold archives
/// and staging `.tmp` files sit beside them and are skipped by the extension check too).
fn is_primary_transcript_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    if name == "sessions.json" || stem.ends_with(".trajectory") || stem.contains("-compaction-") {
        return false;
    }
    // `<sid>.checkpoint.<uuid>.jsonl`
    if let Some((_, tail)) = stem.rsplit_once(".checkpoint.") {
        if is_uuid_like(tail) {
            return false;
        }
    }
    true
}

fn is_uuid_like(s: &str) -> bool {
    s.len() == 36
        && s.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
}

/// Is `path` an `openclaw-agent*.sqlite` store (a [`SessionRef::path`] discovery assigned)?
fn is_sqlite_store_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("sqlite")
}

/// `agents/<agentId>/agent/openclaw-agent*.sqlite` stores under an `agents` root.
fn sqlite_stores(agents_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(agents) = fs::read_dir(agents_root) else {
        return out;
    };
    for agent in agents.flatten() {
        let dir = agent.path().join("agent");
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("openclaw-agent") && name.ends_with(".sqlite") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Load `sessions.json` (sessionId → metadata) from a sessions dir, if present.
fn load_index(sessions_dir: &Path) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Ok(text) = fs::read_to_string(sessions_dir.join("sessions.json")) {
        if let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(&text) {
            for (_key, entry) in entries {
                if let Some(sid) = entry.get("sessionId").and_then(Value::as_str) {
                    map.insert(sid.to_string(), entry);
                }
            }
        }
    }
    map
}

fn session_id_from_filename(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // strip a `-topic-<id>` suffix if present
    match stem.split_once("-topic-") {
        Some((sid, _)) => sid.to_string(),
        None => stem.to_string(),
    }
}

/// Cheap metadata folded from a transcript's entries (JSONL lines or `transcript_events` rows), for
/// discovery — shared by the JSONL and sqlite scans.
#[derive(Default)]
struct ScanAcc {
    id: String,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    first_user: Option<String>,
    session_name: Option<String>,
    message_count: usize,
}

impl ScanAcc {
    fn feed(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if self.id.is_empty() {
                    self.id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                }
                if self.cwd.is_none() {
                    self.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                }
                if self.created_at.is_none() {
                    self.created_at = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
                }
            }
            Some("message") => {
                self.message_count += 1;
                if let Some(ts) = entry_timestamp(v) {
                    self.updated_at = Some(ts);
                }
                if self.first_user.is_none() && v.pointer("/message/role").and_then(Value::as_str) == Some("user") {
                    let t = coerce_content_text(v.pointer("/message/content"));
                    if !t.trim().is_empty() {
                        self.first_user = Some(crate::ir::truncate(&t, 80));
                    }
                }
            }
            // The user-visible session name (`/name`, UI rename): the natural title.
            Some("session_info") => {
                if let Some(name) = v.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()) {
                    self.session_name = Some(crate::ir::truncate(name, 80));
                }
            }
            _ => {}
        }
    }
}

/// Title/cwd fields of a `sessions.json` entry / `session_nodes.entry_json` (same shape).
fn entry_title(entry: Option<&Value>) -> Option<String> {
    entry
        .and_then(|e| e.get("label").or_else(|| e.get("subject")))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80))
}

fn entry_cwd(entry: Option<&Value>) -> Option<PathBuf> {
    entry.and_then(|e| {
        ["cwd", "spawnedCwd", "spawnedWorkspaceDir"]
            .iter()
            .find_map(|k| e.get(*k).and_then(Value::as_str))
            .map(PathBuf::from)
    })
}

fn scan(path: &Path, index: &HashMap<String, Value>) -> Result<SessionRef> {
    let text = fs::read_to_string(path)?;
    let mut acc = ScanAcc::default();
    super::for_each_json_line_str(&text, |v| {
        acc.feed(&v);
        Flow::Continue
    });

    let id = if acc.id.is_empty() {
        session_id_from_filename(path)
    } else {
        acc.id
    };
    let entry = index.get(&id);
    let title = entry_title(entry).or(acc.session_name).or(acc.first_user);
    let cwd = acc.cwd.or_else(|| entry_cwd(entry));
    // `updatedAt` in the index is epoch-ms.
    let entry_updated = entry.and_then(|e| e.get("updatedAt")).and_then(ts_from_value);

    Ok(SessionRef {
        id,
        harness: Harness::OpenClaw,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at: acc.created_at,
        updated_at: entry_updated.or(acc.updated_at),
        message_count: acc.message_count,
    })
}

/// Prefer the entry-level ISO `timestamp`, fall back to the inner epoch-ms `message.timestamp`.
fn entry_timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .or_else(|| v.pointer("/message/timestamp").and_then(ts_from_value))
}

fn parse_entry(v: &Value) -> Option<Message> {
    let msg = v.get("message")?;
    let role_str = msg.get("role").and_then(Value::as_str)?;
    let role = match role_str {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        "system" => Role::System,
        // Custom (declaration-merged) message types. Modeled as System turns so they ride along
        // in the IR with their semantics preserved in `extra`/blocks rather than being dropped.
        "bashExecution" | "branchSummary" | "compactionSummary" | "custom" => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    m.model = msg.get("model").and_then(Value::as_str).map(str::to_string);

    match role_str {
        "toolResult" => parse_tool_result(&mut m, msg),
        "assistant" => {
            parse_content_array(&mut m, msg.get("content"));
            capture_assistant_meta(&mut m, msg);
        }
        "user" | "system" => match msg.get("content") {
            Some(Value::String(s)) => m.content.push(Block::Text { text: s.clone().into() }),
            content => parse_content_array(&mut m, content),
        },
        "bashExecution" => parse_bash_execution(&mut m, msg),
        "branchSummary" => parse_branch_summary(&mut m, msg),
        "compactionSummary" => parse_compaction_summary(&mut m, msg),
        "custom" => parse_custom(&mut m, msg),
        _ => {}
    }

    (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
}

fn parse_tool_result(m: &mut Message, msg: &Value) {
    let tool_use_id = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
    m.content.push(Block::ToolResult {
        tool_use_id,
        content: coerce_content_text(msg.get("content")).into(),
        is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
        tool_name: msg.get("toolName").and_then(Value::as_str).map(str::to_string),
        status: None,
        details: msg.get("details").filter(|d| !d.is_null()).cloned(),
    });
    // Image parts in a tool result aren't captured by the text coercion above; surface them.
    if let Some(Value::Array(items)) = msg.get("content") {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(b) = content_block(item) {
                    m.content.push(b);
                }
            }
        }
    }
    if let Some(name) = msg.get("toolName").and_then(Value::as_str) {
        m.extra.insert("tool_name".into(), Value::from(name));
    }
    // `details` is an arbitrary structured payload (e.g. diffs, file lists) — preserve verbatim.
    if let Some(details) = msg.get("details") {
        if !details.is_null() {
            m.extra.insert("tool_details".into(), details.clone());
        }
    }
}

/// Parse a `(TextContent|ThinkingContent|ToolCall|ImageContent)[]` content array.
fn parse_content_array(m: &mut Message, content: Option<&Value>) {
    if let Some(Value::Array(items)) = content {
        for item in items {
            if let Some(b) = content_block(item) {
                m.content.push(b);
            }
        }
    }
}

/// Capture assistant-level metadata (provider/api/model/usage/stop/diagnostics) into `extra`.
fn capture_assistant_meta(m: &mut Message, msg: &Value) {
    for key in [
        "api",
        "provider",
        "responseModel",
        "responseId",
        "stopReason",
        "errorMessage",
    ] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
    if let Some(diags) = msg.get("diagnostics") {
        if !diags.is_null() {
            m.extra.insert("diagnostics".into(), diags.clone());
        }
    }
    if let Some(usage) = msg.get("usage") {
        m.usage = parse_usage(usage);
        // The full usage object also carries cost breakdowns; keep it verbatim.
        m.extra.insert("usage_raw".into(), usage.clone());
    }
}

fn parse_usage(usage: &Value) -> Option<Usage> {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let u = Usage {
        input_tokens: get("input"),
        output_tokens: get("output"),
        cache_read_tokens: get("cacheRead"),
        cache_creation_tokens: get("cacheWrite"),
    };
    let any = u.input_tokens.is_some()
        || u.output_tokens.is_some()
        || u.cache_read_tokens.is_some()
        || u.cache_creation_tokens.is_some();
    any.then_some(u)
}

fn parse_bash_execution(m: &mut Message, msg: &Value) {
    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
    let output = msg.get("output").and_then(Value::as_str).unwrap_or("");
    let mut text = format!("$ {command}");
    if !output.is_empty() {
        text.push('\n');
        text.push_str(output);
    }
    m.content.push(Block::Text { text: text.into() });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("bashExecution"));
    for key in [
        "exitCode",
        "cancelled",
        "truncated",
        "fullOutputPath",
        "excludeFromContext",
    ] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
}

fn parse_branch_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("branchSummary"));
    if let Some(from_id) = msg.get("fromId").and_then(Value::as_str) {
        m.extra.insert("branch_from_id".into(), Value::from(from_id));
    }
}

fn parse_compaction_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("compactionSummary"));
    for key in ["tokensBefore", "tokensAfter", "firstKeptEntryId"] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
}

fn parse_custom(m: &mut Message, msg: &Value) {
    let text = coerce_content_text(msg.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }
    m.extra.insert("openclaw_message_role".into(), Value::from("custom"));
    if let Some(ct) = msg.get("customType").and_then(Value::as_str) {
        m.extra.insert("custom_type".into(), Value::from(ct));
    }
    if let Some(display) = msg.get("display") {
        if !display.is_null() {
            m.extra.insert("display".into(), display.clone());
        }
    }
    if let Some(details) = msg.get("details") {
        if !details.is_null() {
            m.extra.insert("custom_details".into(), details.clone());
        }
    }
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: item.get("text").and_then(Value::as_str)?.to_string().into(),
        }),
        "thinking" => Some(Block::Thinking {
            text: item
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into(),
            // OpenClaw uses `thinkingSignature`; `redacted` flags a server-redacted reasoning block.
            signature: item
                .get("thinkingSignature")
                .and_then(Value::as_str)
                .map(str::to_string),
            encrypted: item
                .get("redacted")
                .and_then(Value::as_bool)
                .filter(|&r| r)
                .map(|_| "redacted".to_string()),
            redacted: item.get("redacted").and_then(Value::as_bool).unwrap_or(false),
        }),
        "toolCall" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        "image" => Some(Block::Image {
            media_type: item.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: item
                .get("data")
                .and_then(Value::as_str)
                .map(|d| crate::ir::truncate(d, 80)),
        }),
        _ => None,
    }
}

/// Coerce a `content` field (string | array of text/image blocks) to plain text.
fn coerce_content_text(v: Option<&Value>) -> String {
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

/// camelCase → snake_case for `extra` keys (ASCII only; OpenClaw keys are all ASCII).
fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SQLite store (OpenClaw ≥ 2026-07-11)
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
fn open_ro(path: &Path) -> Result<Connection> {
    // Read-only, never a write lock on the agent's live store (WAL readers are fine).
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))
}

/// Epoch-ms INTEGER column → timestamp.
#[cfg(feature = "sqlite")]
fn ms_to_dt(ms: Option<i64>) -> Option<DateTime<Utc>> {
    ms.and_then(DateTime::from_timestamp_millis)
}

/// One [`SessionRef`] per `session_windows` row (one transcript each; a reset/rollover/fork opens a
/// new window under the same `session_key`). Title precedence: the node's `label`, then either
/// `display_name`, then the entry's `label`/`subject`, then a `session_info` name, then the first
/// user prompt. `path` is the store file; `id` is the window's `session_id` — `stream` reopens the
/// store and selects the rows by id.
#[cfg(feature = "sqlite")]
fn discover_sqlite(db: &Path) -> Result<Vec<SessionRef>> {
    let conn = open_ro(db)?;
    let mut windows = conn.prepare(
        "SELECT w.session_id, w.created_at, w.updated_at, w.transcript_updated_at, w.display_name, \
         n.label, n.display_name, n.entry_json \
         FROM session_windows w LEFT JOIN session_nodes n ON n.session_key = w.session_key \
         ORDER BY w.updated_at DESC",
    )?;
    type WindowRow = (
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<WindowRow> = windows
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut events = conn.prepare("SELECT event_json FROM transcript_events WHERE session_id = ?1 ORDER BY seq ASC")?;
    let mut out = Vec::with_capacity(rows.len());
    for (session_id, created, updated, transcript_updated, w_display, n_label, n_display, entry_json) in rows {
        let mut acc = ScanAcc::default();
        let jsons = events.query_map([&session_id], |row| row.get::<_, String>(0))?;
        for json in jsons.flatten() {
            if let Some(v) = parse_line(&json) {
                acc.feed(&v);
            }
        }
        let entry: Option<Value> = entry_json.as_deref().and_then(|e| serde_json::from_str(e).ok());
        let title = n_label
            .or(n_display)
            .or(w_display)
            .filter(|t| !t.trim().is_empty())
            .map(|t| crate::ir::truncate(&t, 80))
            .or_else(|| entry_title(entry.as_ref()))
            .or(acc.session_name)
            .or(acc.first_user);
        let cwd = acc.cwd.or_else(|| entry_cwd(entry.as_ref()));
        out.push(SessionRef {
            id: session_id,
            harness: Harness::OpenClaw,
            path: db.to_path_buf(),
            cwd,
            title,
            created_at: acc.created_at.or_else(|| ms_to_dt(created)),
            updated_at: ms_to_dt(transcript_updated)
                .or_else(|| ms_to_dt(updated))
                .or(acc.updated_at),
            message_count: acc.message_count,
        });
    }
    Ok(out)
}

#[cfg(not(feature = "sqlite"))]
fn discover_sqlite(_db: &Path) -> Result<Vec<SessionRef>> {
    Ok(Vec::new())
}

/// Replay one window's `transcript_events` rows (`ORDER BY seq`) through the same entry parser the
/// JSONL path uses — `event_json` IS the JSONL line.
#[cfg(feature = "sqlite")]
fn stream_sqlite(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    let conn = open_ro(&r.path)?;
    let mut stmt = conn.prepare("SELECT event_json FROM transcript_events WHERE session_id = ?1 ORDER BY seq ASC")?;
    let entries: Vec<Value> = stmt
        .query_map([&r.id], |row| row.get::<_, String>(0))?
        .flatten()
        .filter_map(|j| parse_line(&j))
        .collect();
    Ok(stream_rows(
        rows_from_values(entries, opts.complete).into_iter(),
        r,
        opts,
        sink,
    ))
}

#[cfg(not(feature = "sqlite"))]
fn stream_sqlite(r: &SessionRef, _opts: &ParseOptions, _sink: &mut dyn MessageSink) -> Result<Session> {
    anyhow::bail!(
        "{} is an OpenClaw sqlite store; cv-core was built without the `sqlite` feature",
        r.path.display()
    )
}

// ---------------------------------------------------------------------------
// Transcript tree — a port of OpenClaw's `transcript-tree.ts` (`scanSessionTranscriptNavigation`,
// `selectSessionTranscriptTreePathNodes`, `selectSessionTranscriptActiveEntries`).
// ---------------------------------------------------------------------------

/// `isCanonicalSessionEntryType` (`transcript-tree.ts:32-45`).
fn is_canonical_entry_type(ty: &str) -> bool {
    matches!(
        ty,
        "message"
            | "thinking_level_change"
            | "model_change"
            | "compaction"
            | "reset"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "label"
            | "session_info"
    )
}

fn entry_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn is_canonical_entry(v: &Value) -> bool {
    is_canonical_entry_type(entry_type(v))
}

/// `isSessionTranscriptLeafControl`: a `leaf` row that parses as a tree entry.
fn is_leaf_control(v: &Value) -> bool {
    entry_type(v) == "leaf" && parse_tree_entry(v).is_some()
}

/// A parsed tree row (`SessionTranscriptTreeEntry`). `leaf_id`: `None` = undefined (no leaf
/// update), `Some(None)` = null, `Some(Some(id))` = the new active leaf.
#[derive(Clone, Debug)]
struct TreeEntry {
    id: String,
    parent_id: Option<String>,
    leaf_id: Option<Option<String>>,
    append_parent_id: Option<String>,
    side: bool,
}

/// A JSON field that is `null`, a non-blank string, or absent/invalid: `Some(None)`,
/// `Some(Some(s))`, `None`.
fn nullable_id(v: Option<&Value>) -> Option<Option<String>> {
    match v {
        Some(Value::Null) => Some(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Some(Some(s.clone())),
        _ => None,
    }
}

/// `parseSessionTranscriptTreeEntry` (`transcript-tree.ts:65-110`): rows with a `parentId` key.
fn parse_tree_entry(v: &Value) -> Option<TreeEntry> {
    let obj = v.as_object()?;
    if entry_type(v) == "session" || !obj.contains_key("parentId") {
        return None;
    }
    let id = nullable_id(obj.get("id"))??;
    let parent_id = nullable_id(obj.get("parentId"))?;
    let side = match obj.get("appendMode") {
        None => false,
        Some(Value::String(m)) if m == "side" => true,
        Some(_) => return None,
    };
    if entry_type(v) == "leaf" {
        let target = nullable_id(obj.get("targetId"))?;
        let append_parent_id = match obj.get("appendParentId") {
            None => target.clone(),
            other => nullable_id(other)?,
        };
        return Some(TreeEntry {
            id,
            parent_id: target.clone(),
            leaf_id: Some(target),
            append_parent_id,
            side,
        });
    }
    let leaf_id = (is_canonical_entry(v) && !side).then(|| Some(id.clone()));
    Some(TreeEntry {
        id: id.clone(),
        parent_id,
        leaf_id,
        append_parent_id: Some(id),
        side,
    })
}

/// `parseParentlessCanonicalEntry`: a canonical row written by an older appender (no `parentId`
/// key) continues linearly from the current leaf.
fn parse_parentless_entry(v: &Value, leaf: &Option<String>) -> Option<TreeEntry> {
    let obj = v.as_object()?;
    if !is_canonical_entry(v) || obj.contains_key("parentId") {
        return None;
    }
    let id = nullable_id(obj.get("id"))??;
    let side = obj.get("appendMode").and_then(Value::as_str) == Some("side");
    Some(TreeEntry {
        id: id.clone(),
        parent_id: leaf.clone(),
        leaf_id: (!side).then(|| Some(id.clone())),
        append_parent_id: Some(id),
        side,
    })
}

struct TreeNode {
    entry: TreeEntry,
    is_leaf_control: bool,
    index: usize,
}

/// `resolveCanonicalParentId`: leaf controls are omitted from selected paths, so a parent link
/// that lands on a marker follows through to the marker's own (normalized) parent.
fn resolve_canonical_parent(
    parent: Option<String>,
    nodes: &[TreeNode],
    by_id: &HashMap<String, usize>,
) -> Option<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = parent;
    while let Some(cur) = current {
        if seen.contains(&cur) {
            return Some(cur);
        }
        let Some(&i) = by_id.get(&cur) else {
            return Some(cur);
        };
        if !nodes[i].is_leaf_control {
            return Some(cur);
        }
        seen.insert(cur.clone());
        current = nodes[i].entry.parent_id.clone();
    }
    None
}

/// `selectSessionTranscriptTreePathNodes`: the normalized path from `leaf` to the root, skipping
/// leaf controls; a reachable suffix survives missing ancestors; a cycle yields nothing.
fn select_path(leaf: &Option<String>, nodes: &[TreeNode], by_id: &HashMap<String, usize>) -> Vec<usize> {
    let mut path = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = leaf.clone();
    while let Some(cur) = current {
        if seen.contains(&cur) {
            return Vec::new();
        }
        seen.insert(cur.clone());
        let Some(&i) = by_id.get(&cur) else {
            break;
        };
        if !nodes[i].is_leaf_control {
            path.push(i);
        }
        current = nodes[i].entry.parent_id.clone();
    }
    path.reverse();
    path
}

/// Branch-aware replay of a whole transcript (`scanSessionTranscriptNavigation` +
/// `selectSessionTranscriptActiveEntries`): returns the rows to emit. Lean (`!complete`): the
/// `session` header(s) plus the active entries in OpenClaw's order (a retained pre-reset/compaction
/// prefix, then the visible path), each with its normalized parent. `complete`: every entry in file
/// order, off-branch ones flagged inactive.
fn select_rows(entries: Vec<Value>, complete: bool) -> Vec<Row> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut leaf: Option<String> = None;
    let mut append_parent: Option<String> = None;
    let mut has_explicit_leaf_update = false;
    let mut latest_reset: Option<String> = None;
    let mut reset_descendants: HashSet<String> = HashSet::new();
    let mut invalid_controls: HashSet<String> = HashSet::new();

    for (index, v) in entries.iter().enumerate() {
        let is_ctrl = is_leaf_control(v);
        let mut explicit = parse_tree_entry(v);
        // A leaf control may not jump behind the latest reset boundary.
        if let (Some(_), Some(cur_leaf), Some(e)) = (&latest_reset, &leaf, &explicit) {
            if is_ctrl && e.leaf_id.is_some() {
                let target = e.leaf_id.clone().flatten();
                if target.as_ref().is_none_or(|t| !reset_descendants.contains(t)) {
                    explicit = Some(TreeEntry {
                        parent_id: Some(cur_leaf.clone()),
                        leaf_id: Some(Some(cur_leaf.clone())),
                        append_parent_id: Some(cur_leaf.clone()),
                        ..e.clone()
                    });
                }
            }
        }
        let known = |id: &Option<String>| {
            id.as_ref()
                .is_none_or(|i| by_id.contains_key(i) && !invalid_controls.contains(i))
        };
        let invalid = match &explicit {
            Some(e) if is_ctrl && e.leaf_id.is_some() => {
                !known(&e.leaf_id.clone().flatten()) || !known(&e.append_parent_id)
            }
            _ => false,
        };
        if invalid {
            // A transparent structural marker: descendants repair through its raw parent, the
            // navigation state does not move.
            let e = explicit.expect("invalid implies explicit");
            invalid_controls.insert(e.id.clone());
            let raw_parent = v.get("parentId").and_then(Value::as_str).map(str::to_string);
            let node = TreeNode {
                entry: TreeEntry {
                    parent_id: raw_parent,
                    leaf_id: None,
                    append_parent_id: append_parent.clone(),
                    ..e
                },
                is_leaf_control: true,
                index,
            };
            by_id.insert(node.entry.id.clone(), nodes.len());
            nodes.push(node);
            continue;
        }
        let explicit_present = explicit.is_some();
        let Some(mut te) = explicit.or_else(|| parse_parentless_entry(v, &leaf)) else {
            continue;
        };
        if is_canonical_entry(v) {
            let stale =
                explicit_present && te.parent_id.as_ref().is_some_and(|p| !by_id.contains_key(p)) && leaf.is_some();
            let crosses_reset = latest_reset.is_some()
                && !te.side
                && te.parent_id.as_ref().is_none_or(|p| !reset_descendants.contains(p));
            let reparent_to_leaf = crosses_reset
                || (!te.side && stale)
                || (explicit_present && !te.side && te.parent_id == append_parent && leaf != append_parent);
            let logical = if reparent_to_leaf {
                leaf.clone()
            } else {
                te.parent_id.clone()
            };
            te.parent_id = resolve_canonical_parent(logical, &nodes, &by_id);
        }
        let node = TreeNode {
            entry: te,
            is_leaf_control: is_ctrl,
            index,
        };
        let id = node.entry.id.clone();
        if entry_type(v) == "reset" {
            latest_reset = Some(id.clone());
            reset_descendants.clear();
            reset_descendants.insert(id.clone());
        } else if latest_reset.is_some()
            && node
                .entry
                .parent_id
                .as_ref()
                .is_some_and(|p| reset_descendants.contains(p))
        {
            reset_descendants.insert(id.clone());
        }
        append_parent = node.entry.append_parent_id.clone();
        if let Some(new_leaf) = &node.entry.leaf_id {
            leaf = new_leaf.clone();
            if explicit_present {
                has_explicit_leaf_update = true;
            }
        }
        by_id.insert(id, nodes.len());
        nodes.push(node);
    }

    // `selectSessionTranscriptActiveEntries`
    let active_order: Vec<usize> = if !has_explicit_leaf_update {
        (0..entries.len()).collect()
    } else {
        let path = select_path(&leaf, &nodes, &by_id);
        let mut active: Vec<usize> = path.iter().map(|&n| nodes[n].index).collect();
        // The nearest preceding compaction/reset keeps the context that seeded the visible branch.
        let first = path.first().map(|&n| nodes[n].index).unwrap_or(0);
        let mut prefix: Vec<usize> = Vec::new();
        for i in (0..first).rev() {
            let ty = entry_type(&entries[i]);
            if ty != "compaction" && ty != "reset" {
                continue;
            }
            if ty == "reset" {
                let reset_id = nullable_id(entries[i].get("id")).flatten();
                let kept = nullable_id(entries[i].get("firstKeptEntryId")).flatten();
                if let (Some(rid), Some(kept)) = (reset_id, kept) {
                    let reset_path = select_path(&Some(rid), &nodes, &by_id);
                    if let Some(start) = reset_path.iter().position(|&n| nodes[n].entry.id == kept) {
                        prefix = reset_path[start..].iter().map(|&n| nodes[n].index).collect();
                        break;
                    }
                }
            }
            prefix = vec![i];
            break;
        }
        prefix.append(&mut active);
        prefix
    };
    let normalized: HashMap<usize, Option<String>> = nodes
        .iter()
        .filter(|n| !n.is_leaf_control)
        .map(|n| (n.index, n.entry.parent_id.clone()))
        .collect();
    let active_set: HashSet<usize> = active_order.iter().copied().collect();

    let mut rows: Vec<Row> = Vec::new();
    if complete {
        for (i, v) in entries.into_iter().enumerate() {
            let header = entry_type(&v) == "session";
            rows.push(Row {
                parent: normalized.get(&i).cloned(),
                active: header || active_set.contains(&i),
                v,
            });
        }
    } else {
        let mut taken: Vec<Option<Value>> = entries.into_iter().map(Some).collect();
        // Headers first (never tree nodes), then the active entries in OpenClaw's order.
        for slot in taken.iter_mut() {
            if slot.as_ref().is_some_and(|v| entry_type(v) == "session") {
                rows.push(Row::active(slot.take().unwrap()));
            }
        }
        for i in active_order {
            if let Some(v) = taken[i].take() {
                rows.push(Row {
                    parent: normalized.get(&i).cloned(),
                    active: true,
                    v,
                });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ref(name: &str) -> SessionRef {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/openclaw")
            .join(name);
        SessionRef {
            id: "test-session".into(),
            harness: Harness::OpenClaw,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn parse_fixture(name: &str) -> Session {
        let r = fixture_ref(name);
        let text = fs::read_to_string(&r.path).unwrap_or_else(|e| panic!("reading {}: {e}", r.path.display()));
        parse_text(&text, &r)
    }

    /// The three transcript tables of `src/state/openclaw-agent-schema.sql` (column names verbatim;
    /// cross-table foreign keys to `conversations` dropped).
    #[cfg(feature = "sqlite")]
    const STORE_DDL: &str = "
        CREATE TABLE session_nodes (
          session_key TEXT NOT NULL PRIMARY KEY, current_session_id TEXT NOT NULL,
          entry_json TEXT NOT NULL, legacy_acp_migration_json TEXT,
          entry_valid INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, status TEXT,
          created_at INTEGER, parent_session_key TEXT, spawned_by TEXT, fork_source_session_key TEXT,
          fork_source_session_id TEXT, fork_source_entry_id TEXT, label TEXT, display_name TEXT,
          category TEXT, icon TEXT, pinned_at INTEGER, archived_at INTEGER, last_read_at INTEGER,
          last_interaction_at INTEGER, last_activity_at INTEGER) STRICT;
        CREATE TABLE session_windows (
          session_id TEXT NOT NULL PRIMARY KEY, session_key TEXT NOT NULL, previous_session_id TEXT,
          reason TEXT, session_scope TEXT NOT NULL DEFAULT 'conversation', created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL, transcript_updated_at INTEGER DEFAULT NULL,
          transcript_observed_at INTEGER DEFAULT NULL, started_at INTEGER, ended_at INTEGER, status TEXT,
          chat_type TEXT, channel TEXT, account_id TEXT, primary_conversation_id TEXT,
          model_provider TEXT, model TEXT, agent_harness_id TEXT, parent_session_key TEXT,
          spawned_by TEXT, display_name TEXT,
          FOREIGN KEY (session_key) REFERENCES session_nodes(session_key) ON DELETE CASCADE) STRICT;
        CREATE TABLE transcript_events (
          session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL,
          created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq),
          FOREIGN KEY (session_id) REFERENCES session_windows(session_id) ON DELETE CASCADE) STRICT;
    ";

    /// A v4 transcript with a rewind: `a2` is a sibling answer, the `leaf` control selects `a1`,
    /// the session is then named, compacted, and continued. Visible branch: u1 a1 u2 (si) c1 a3.
    fn branchy_events() -> Vec<String> {
        vec![
            r#"{"type":"session","version":4,"id":"s-sql","timestamp":"2026-09-19T10:00:00.000Z","cwd":"/work/sql","parentSession":"s-parent"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-19T10:00:01.000Z","message":{"role":"user","content":"hello from sqlite"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-19T10:00:02.000Z","message":{"role":"assistant","model":"claude-opus-4","provider":"anthropic","content":[{"type":"text","text":"first answer"}]}}"#,
            r#"{"type":"message","id":"a2","parentId":"u1","timestamp":"2026-09-19T10:00:03.000Z","message":{"role":"assistant","model":"claude-opus-4","provider":"anthropic","content":[{"type":"text","text":"second answer (side branch)"}]}}"#,
            r#"{"type":"leaf","id":"l1","parentId":"a2","targetId":"a1"}"#,
            r#"{"type":"message","id":"u2","parentId":"a1","timestamp":"2026-09-19T10:00:04.000Z","message":{"role":"user","content":"continue"}}"#,
            r#"{"type":"session_info","id":"si","parentId":"u2","timestamp":"2026-09-19T10:00:05.000Z","name":"Named session"}"#,
            r#"{"type":"compaction","id":"c1","parentId":"si","timestamp":"2026-09-19T10:00:06.000Z","summary":"summary of earlier","firstKeptEntryId":"u2","tokensBefore":1234}"#,
            r#"{"type":"message","id":"a3","parentId":"c1","timestamp":"2026-09-19T10:00:07.000Z","message":{"role":"assistant","model":"claude-opus-4","provider":"anthropic","content":[{"type":"text","text":"after compaction"}]}}"#,
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[cfg(feature = "sqlite")]
    fn write_store(agents: &Path, session_id: &str, events: &[String]) -> PathBuf {
        let agent_dir = agents.join("main").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let db = agent_dir.join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(STORE_DDL).unwrap();
        conn.execute(
            "INSERT INTO session_nodes (session_key, current_session_id, entry_json, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "agent:main:main",
                session_id,
                format!(r#"{{"sessionId":"{session_id}","updatedAt":1789810800000,"cwd":"/work/from-entry"}}"#),
                1789810800000i64
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_windows (session_id, session_key, reason, created_at, updated_at, model) VALUES (?1, ?2, 'initial', ?3, ?4, 'claude-opus-4')",
            rusqlite::params![session_id, "agent:main:main", 1789810000000i64, 1789810800000i64],
        )
        .unwrap();
        for (seq, ev) in events.iter().enumerate() {
            conn.execute(
                "INSERT INTO transcript_events (session_id, seq, event_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![session_id, seq as i64, ev, 1789810000000i64 + seq as i64],
            )
            .unwrap();
        }
        db
    }

    fn tmp_agents() -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("cv-openclaw-{}", uuid::Uuid::new_v4()))
            .join("agents");
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_store_is_discovered_and_replays_the_visible_branch() {
        let agents = tmp_agents();
        let db = write_store(&agents, "s-sql", &branchy_events());
        let adapter = OpenClaw {
            roots: vec![agents.clone()],
        };
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1, "one SessionRef per session window");
        let r = &refs[0];
        assert_eq!(r.id, "s-sql");
        assert_eq!(r.path, db);
        assert_eq!(
            r.cwd,
            Some(PathBuf::from("/work/sql")),
            "header cwd wins over entry_json"
        );
        assert_eq!(
            r.title.as_deref(),
            Some("Named session"),
            "session_info names the session"
        );
        assert_eq!(r.message_count, 5, "every `message` row counts, branch or not");
        assert_eq!(r.created_at, parse_ts("2026-09-19T10:00:00.000Z"));
        assert_eq!(
            r.updated_at,
            chrono::DateTime::from_timestamp_millis(1789810800000),
            "window updated_at (epoch ms)"
        );

        // Lean parse: the visible branch only — a2 (the rewound sibling) is gone, the leaf control
        // emits nothing, session_info sets the title, the compaction is a System note.
        let s = adapter.parse(r).unwrap();
        assert_eq!(s.title.as_deref(), Some("Named session"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(s.extra["openclaw_parent_session"], "s-parent");
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "u2", "c1", "a3"]);
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert!(!texts.iter().any(|t| t.contains("side branch")));
        let c1 = &s.messages[3];
        assert_eq!(c1.role, Role::System);
        assert_eq!(c1.extra["openclaw_entry_type"], "compaction");
        assert_eq!(c1.extra["tokens_before"], 1234);
        assert_eq!(c1.extra["first_kept_entry_id"], "u2");
        assert_eq!(c1.text().as_deref(), Some("summary of earlier"));
        // parents on the visible path are kept as written (u2 → a1); the leaf control itself is
        // never a parent of anything on the path.
        assert_eq!(s.messages[2].parent_id.as_deref(), Some("a1"));
        assert_eq!(c1.parent_id.as_deref(), Some("si"));

        // Complete parse: everything rides along; off-branch rows are tagged.
        let mut sink = crate::stream::CollectSink::default();
        adapter.stream(r, &ParseOptions::complete(), &mut sink).unwrap();
        let ids: Vec<&str> = sink.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "a2", "l1", "u2", "si", "c1", "a3"]);
        let inactive: Vec<&str> = sink
            .messages
            .iter()
            .filter(|m| m.extra.get("openclaw_inactive_branch") == Some(&Value::Bool(true)))
            .filter_map(|m| m.id.as_deref())
            .collect();
        assert_eq!(inactive, ["a2", "l1"]);
        let si = sink.messages.iter().find(|m| m.id.as_deref() == Some("si")).unwrap();
        assert_eq!(si.extra["openclaw_entry_type"], "session_info");
        assert_eq!(si.extra["openclaw_entry"]["name"], "Named session");
        assert!(si.content.is_empty());
        fs::remove_dir_all(agents.parent().unwrap()).ok();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn legacy_jsonl_twin_is_deduped_against_the_sqlite_store() {
        let agents = tmp_agents();
        let db = write_store(&agents, "s-sql", &branchy_events());
        let sessions = agents.join("main").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // a pre-migration copy of the same session, plus a genuinely legacy one
        fs::write(sessions.join("s-sql.jsonl"), branchy_events().join("\n") + "\n").unwrap();
        fs::write(
            sessions.join("legacy-1.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"legacy-1","timestamp":"2026-05-01T00:00:00Z","cwd":"/old"}"#,
                "\n",
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-01T00:00:01Z","message":{"role":"user","content":"old prompt"}}"#,
                "\n"
            ),
        )
        .unwrap();
        // archive siblings that must NOT become sessions
        fs::write(sessions.join("legacy-1.trajectory.jsonl"), "{}\n").unwrap();
        fs::write(
            sessions.join("legacy-1.checkpoint.123e4567-e89b-42d3-a456-426614174000.jsonl"),
            "{}\n",
        )
        .unwrap();
        fs::write(sessions.join("legacy-1.jsonl.reset.2026-06-01T00-00-00Z"), "{}\n").unwrap();
        fs::write(sessions.join("deadbeef.jsonl.zst"), b"\x28\xb5\x2f\xfd").unwrap();

        let adapter = OpenClaw {
            roots: vec![agents.clone()],
        };
        let mut refs = adapter.discover().unwrap();
        refs.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["legacy-1", "s-sql"]);
        assert_eq!(refs[1].path, db, "the sqlite copy wins over the JSONL twin");
        assert_eq!(refs[0].cwd, Some(PathBuf::from("/old")));
        fs::remove_dir_all(agents.parent().unwrap()).ok();
    }

    #[test]
    fn primary_transcript_filename_rule() {
        assert!(is_primary_transcript_name("3f28190c-96fa-4d73-b37c-251dcccb3f9b.jsonl"));
        assert!(is_primary_transcript_name("sess-1-topic-foo.jsonl"));
        assert!(!is_primary_transcript_name("sessions.json"));
        assert!(!is_primary_transcript_name("sess-1.trajectory.jsonl"));
        assert!(!is_primary_transcript_name(
            "sess-1.checkpoint.123e4567-e89b-42d3-a456-426614174000.jsonl"
        ));
        assert!(is_primary_transcript_name("sess-1.checkpoint.notauuid.jsonl"));
        assert!(!is_primary_transcript_name("sess-1-compaction-abc.jsonl"));
        assert!(!is_primary_transcript_name("sess-1.jsonl.reset.2026-06-01T00-00-00Z"));
        assert!(!is_primary_transcript_name(
            "sess-1.jsonl.deleted.2026-06-01T00-00-00Z.zst"
        ));
        assert!(!is_primary_transcript_name("deadbeef.jsonl.zst"));
        assert!(!is_primary_transcript_name("sess-1.jsonl.migrated"));
    }

    #[test]
    fn leaf_controls_select_the_visible_branch_in_jsonl_too() {
        // The on-disk path: a `leaf` control makes the adapter load the file and select the branch.
        let dir = std::env::temp_dir().join(format!("cv-openclaw-leaf-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s-sql.jsonl");
        fs::write(&path, branchy_events().join("\n") + "\n").unwrap();
        let r = SessionRef {
            id: "s-sql".into(),
            harness: Harness::OpenClaw,
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = OpenClaw { roots: vec![] }.parse(&r).unwrap();
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "u2", "c1", "a3"]);
        assert_eq!(s.title.as_deref(), Some("Named session"));
        // the same file without the leaf control streams flat (every entry, file order)
        let flat: Vec<String> = branchy_events()
            .into_iter()
            .filter(|l| !l.contains("\"leaf\""))
            .collect();
        fs::write(&path, flat.join("\n") + "\n").unwrap();
        let s = OpenClaw { roots: vec![] }.parse(&r).unwrap();
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "a2", "u2", "c1", "a3"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_and_side_entries_and_bookkeeping_records() {
        // A `reset` boundary is a System note; `model_change` names the model when no assistant
        // turn has; `label` lands in Session.extra; `custom` (extension state) stays out of the
        // lean stream and is carried under `complete`.
        let text = [
            r#"{"type":"session","version":4,"id":"s-r","timestamp":"2026-09-19T11:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"model_change","id":"mc","parentId":null,"timestamp":"2026-09-19T11:00:00.500Z","provider":"anthropic","modelId":"claude-sonnet-5"}"#,
            r#"{"type":"message","id":"u1","parentId":"mc","timestamp":"2026-09-19T11:00:01.000Z","message":{"role":"user","content":"before reset"}}"#,
            r#"{"type":"reset","id":"r1","parentId":"u1","timestamp":"2026-09-19T11:00:02.000Z","reason":"idle"}"#,
            r#"{"type":"message","id":"u2","parentId":"r1","timestamp":"2026-09-19T11:00:03.000Z","message":{"role":"user","content":"after reset"}}"#,
            r#"{"type":"label","id":"lb","parentId":"u2","timestamp":"2026-09-19T11:00:04.000Z","targetId":"u2","label":"pinned prompt"}"#,
            r#"{"type":"custom","id":"cx","parentId":"lb","timestamp":"2026-09-19T11:00:05.000Z","customType":"openclaw.cache-ttl","data":{"ttl":60}}"#,
            r#"{"type":"custom_message","id":"cm","parentId":"cx","timestamp":"2026-09-19T11:00:06.000Z","customType":"ext.note","content":"an extension note","display":true}"#,
        ]
        .join("\n");
        let r = fixture_ref("unused.jsonl");
        let s = parse_text(&text, &r);
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(s.extra["openclaw_labels"]["u2"], "pinned prompt");
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "r1", "u2", "cm"]);
        let reset = &s.messages[1];
        assert_eq!(reset.role, Role::System);
        assert_eq!(reset.text().as_deref(), Some("[session reset: idle]"));
        assert_eq!(reset.extra["reason"], "idle");
        assert_eq!(s.messages[3].extra["custom_type"], "ext.note");
        assert_eq!(s.messages[3].text().as_deref(), Some("an extension note"));

        let c = parse_text_with(&text, &r, &ParseOptions::complete());
        let ids: Vec<&str> = c.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["mc", "u1", "r1", "u2", "lb", "cx", "cm"]);
        let cx = c.messages.iter().find(|m| m.id.as_deref() == Some("cx")).unwrap();
        assert_eq!(cx.extra["openclaw_entry"]["data"]["ttl"], 60);
    }

    #[test]
    fn non_utf8_line_skips_not_truncates() {
        // A stray binary line mid-transcript must cost exactly that line — `map_while(Result::ok)`
        // used to end the whole stream there, silently dropping every later message.
        let dir = std::env::temp_dir().join(format!("cv-openclaw-bin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00Z","cwd":"/w"}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"first"}}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x81]); // undecodable garbage line
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":"second"}}"#,
        );
        bytes.push(b'\n');
        fs::write(&path, &bytes).unwrap();

        let r = SessionRef {
            id: "s1".into(),
            harness: Harness::OpenClaw,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = OpenClaw { roots: vec![] }.parse(&r).unwrap();
        let texts: Vec<_> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["first", "second"],
            "messages after the binary line must survive"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_v3_full_content_union() {
        let s = parse_fixture("v3-full.jsonl");
        // header cwd + created_at pulled from the session line
        assert_eq!(s.cwd, Some(PathBuf::from("/work/proj")));
        assert!(s.created_at.is_some());
        // real model wins over the synthetic delivery-mirror trailer
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));

        // user, assistant, toolResult, + a synthetic delivery-mirror assistant trailer (kept,
        // but its model is not promoted to the session model)
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[3].model.as_deref(), Some("delivery-mirror"));
        let user = &s.messages[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.text().as_deref(), Some("hello"));
        // version stashed on the first message
        assert_eq!(
            user.extra.get("openclaw_session_version").and_then(Value::as_i64),
            Some(3)
        );

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        // thinking (with signature + redacted), text, toolCall
        let kinds: Vec<&str> = asst
            .content
            .iter()
            .map(|b| match b {
                Block::Thinking { .. } => "thinking",
                Block::Text { .. } => "text",
                Block::ToolUse { .. } => "toolcall",
                Block::ToolResult { .. } => "toolresult",
                Block::Image { .. } => "image",
                Block::File { .. } => "file",
            })
            .collect();
        assert_eq!(kinds, ["thinking", "text", "toolcall"]);
        if let Block::Thinking {
            signature, encrypted, ..
        } = &asst.content[0]
        {
            assert_eq!(signature.as_deref(), Some("sig-think"));
            assert_eq!(encrypted.as_deref(), Some("redacted"));
        } else {
            panic!("expected thinking block");
        }
        if let Block::ToolUse { id, name, .. } = &asst.content[2] {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read_file");
        } else {
            panic!("expected toolcall block");
        }
        // usage + raw usage captured
        let u = asst.usage.as_ref().expect("usage");
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(5));
        assert!(asst.extra.contains_key("usage_raw"));
        assert_eq!(asst.extra.get("provider").and_then(Value::as_str), Some("anthropic"));
        assert_eq!(asst.extra.get("stop_reason").and_then(Value::as_str), Some("toolUse"));

        let tr = &s.messages[2];
        assert_eq!(tr.role, Role::Tool);
        match &tr.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                tool_name,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
                assert_eq!(tool_name.as_deref(), Some("read_file"));
            }
            _ => panic!("expected tool result"),
        }
        assert_eq!(tr.extra.get("tool_name").and_then(Value::as_str), Some("read_file"));
        assert!(tr.extra.contains_key("tool_details"));
    }

    #[test]
    fn parses_legacy_v1_linear() {
        let s = parse_fixture("v1-linear.jsonl");
        assert_eq!(s.messages.len(), 2);
        // v1 entries have no parentId
        assert!(s.messages[0].parent_id.is_none());
        assert_eq!(
            s.messages[0]
                .extra
                .get("openclaw_session_version")
                .and_then(Value::as_i64),
            Some(1)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("legacy first"));
    }

    #[test]
    fn parses_custom_message_types() {
        let s = parse_fixture("custom-messages.jsonl");
        // bashExecution, branchSummary, compactionSummary, custom — all System turns
        assert_eq!(s.messages.len(), 4);

        let bash = &s.messages[0];
        assert_eq!(bash.role, Role::System);
        assert_eq!(
            bash.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("bashExecution")
        );
        assert!(bash.text().unwrap().contains("$ ls -la"));
        assert_eq!(bash.extra.get("exit_code").and_then(Value::as_i64), Some(0));

        let branch = &s.messages[1];
        assert_eq!(
            branch.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("branchSummary")
        );
        assert_eq!(
            branch.extra.get("branch_from_id").and_then(Value::as_str),
            Some("abc123")
        );

        let compact = &s.messages[2];
        assert_eq!(
            compact.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("compactionSummary")
        );
        assert_eq!(compact.extra.get("tokens_before").and_then(Value::as_i64), Some(5000));

        let custom = &s.messages[3];
        assert_eq!(
            custom.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("custom")
        );
        assert_eq!(
            custom.extra.get("custom_type").and_then(Value::as_str),
            Some("openclaw.cache-ttl")
        );
    }

    #[test]
    fn parses_topic_and_acp_session() {
        // topic file: id derives from the `<sid>-topic-<id>` filename when no header id given
        assert_eq!(session_id_from_filename(Path::new("sess-1-topic-foo.jsonl")), "sess-1");

        // a real topic transcript parses like any other thread
        let topic = parse_fixture("sess-1-topic-foo.jsonl");
        assert_eq!(topic.messages.len(), 1);
        assert_eq!(topic.messages[0].text().as_deref(), Some("topic thread message"));

        // ACP-bridged session: only user prompt + acp-runtime text reply persisted.
        let s = parse_fixture("acp-session.jsonl");
        assert_eq!(s.messages.len(), 2);
        // model is NOT reported as the synthetic acp-runtime
        assert_eq!(s.model, None);
        let reply = &s.messages[1];
        assert_eq!(reply.role, Role::Assistant);
        assert_eq!(reply.model.as_deref(), Some("acp-runtime"));
        assert_eq!(reply.extra.get("provider").and_then(Value::as_str), Some("openclaw"));
        assert_eq!(reply.text().as_deref(), Some("done bridging"));
        // bridged turns have no captured tool calls / thinking — confirm the gap
        assert!(reply.content.iter().all(|b| matches!(b, Block::Text { .. })));
    }
}
