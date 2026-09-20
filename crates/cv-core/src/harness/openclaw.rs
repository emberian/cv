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
//! lean passes and carried, tagged `extra["openclaw"]["inactive_branch"]`, under
//! [`ParseOptions::complete`].
//!
//! Canonical entry types beyond `message` (`session-manager-types.ts:28-95`): `compaction`
//! (`summary, firstKeptEntryId, tokensBefore, details?, fromHook?`), `reset` (`reason:
//! new|reset|idle|daily|cron-stale, firstKeptEntryId?`), `branch_summary` (`fromId, summary`),
//! `custom_message` (`customType, content, display, details?` — in model context), `custom`
//! (`customType, data?` — extension state, NOT in context), `session_info` (`name` → the title),
//! `model_change` (`provider, modelId`), `thinking_level_change`, `label` (`targetId, label`).
//!
//! ## Kinds (`docs/INTERFACE-V2.md` §4)
//!
//! Entries → messages: `message` by role — `user` → `Prompt`/Human, `assistant` → `Reply`/Model
//! (or `Error`/Harness when it carries `errorMessage` / `stopReason: "error"`, the error under
//! `extra["openclaw"]["error"]`), `toolResult` → `ToolResult`/Harness, `system` → `SystemPrompt`
//! (also `Session::system_prompt`), and the custom roles `bashExecution` → `InjectedContext`
//! (in model context), `branchSummary`/`custom` → `Notice`, `compactionSummary` →
//! `CompactionBoundary` + `CompactionSummary`; `compaction` → a `CompactionBoundary` turn (the
//! entry's id and fields) followed by a `CompactionSummary` turn holding `summary`; `reset` →
//! `Branch`; `model_change` / `thinking_level_change` → `ModelChange` (`Message::model` = the new
//! model); `branch_summary` / `custom_message` → `Notice`. `session_info` and `label` are session
//! facts (`Session::title` + `extra["openclaw"]["session_name"]`, `extra["openclaw"]["labels"]`)
//! and surface only under [`ParseOptions::complete`], as `Notice` turns; `custom`, `leaf`,
//! `thinking_level_change`-like bookkeeping and unknown types surface only under `complete`, as
//! `Carrier` turns. Every carried record is the top-level `_record`; every harness fact is in
//! `extra["openclaw"]` (`record_type` = the entry type, `message_role` = a custom message role,
//! `session_version` on the first turn, `inactive_branch: true` off the visible path, the
//! assistant's `api`/`provider`/`response_model`/`response_id`/`stop_reason`/`diagnostics`/
//! `usage_raw`, and each control entry's own fields snake_cased). The header's `parentSession` is
//! `Lineage::forked_from`; `usage.cost.total` is `Usage::cost_usd`; `Message::model` is set only
//! when it differs from `Session::model` (the first non-synthetic one seen).

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
        // `OPENCLAW_STATE_DIR` overrides the state dir wholesale (`src/config/state-dir.ts`
        // `resolveStateDir`); otherwise `~/.openclaw` (and the elide-home variant).
        let override_dir = std::env::var_os("OPENCLAW_STATE_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        for base in [
            override_dir,
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
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
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
                s.lineage.forked_from = Some(parent.to_string());
            }
            continue;
        }
        // Off the visible branch: gone for the lean passes, carried (tagged) under `complete`.
        if !row.active && !opts.complete {
            continue;
        }
        let msgs = match ty {
            "message" => parse_entry(&v),
            "compaction" | "reset" | "branch_summary" | "custom_message" | "model_change" | "thinking_level_change" => {
                parse_control_entry(&v)
            }
            // Session-level facts with a first-class home; the record itself only rides under
            // `complete`, as the notice the user saw.
            "session_info" => {
                let name = v.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty());
                if let Some(name) = name {
                    // Discovery already applied OpenClaw's own precedence (store label /
                    // display_name first, then this name — what its session list shows), so a
                    // titled ref keeps its title and `cv ls` and `cv show` agree; the name is
                    // still first-class in the bag. A bare JSONL parse has no ref title.
                    if s.title.is_none() {
                        s.title = Some(crate::ir::truncate(name, 80));
                    }
                    s.harness_extra_mut(Harness::OpenClaw)
                        .insert("session_name".into(), Value::from(name));
                }
                notice_if_complete(&v, opts, format!("[session named: {}]", name.unwrap_or("")))
            }
            "label" => {
                let target = v.get("targetId").and_then(Value::as_str);
                let label = v.get("label").and_then(Value::as_str);
                if let (Some(target), Some(label)) = (target, label) {
                    let labels = s
                        .harness_extra_mut(Harness::OpenClaw)
                        .entry("labels")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Some(map) = labels.as_object_mut() {
                        map.insert(target.to_string(), Value::from(label));
                    }
                }
                notice_if_complete(
                    &v,
                    opts,
                    format!("[label {}: {}]", target.unwrap_or(""), label.unwrap_or("")),
                )
            }
            // `custom` (extension state, not in context), `leaf` navigation controls and anything
            // newer: bookkeeping, carried only under `complete`.
            _ => carrier_if_complete(&v, opts),
        };
        for (i, mut m) in msgs.into_iter().enumerate() {
            // The branch selection's normalized parent applies to the entry's own turn; a second
            // turn split off the same entry (a compaction summary) already points at the first.
            if i == 0 {
                if let Some(parent) = &row.parent {
                    m.parent_id = parent.clone();
                }
            }
            if !row.active {
                m.harness_extra_mut(Harness::OpenClaw)
                    .insert("inactive_branch".into(), Value::Bool(true));
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
            // IR diet: a turn names its model only when it differs from the session's. A
            // `ModelChange` always names the model it switched to.
            if m.kind != MessageKind::ModelChange && m.model.is_some() && m.model == s.model {
                m.model = None;
            }
            if m.kind == MessageKind::SystemPrompt && s.system_prompt.is_none() {
                s.system_prompt = m.text();
            }
            // Stash the transcript schema version on the first message so a downstream consumer
            // can tell v1/v2 (linear) from v3/v4 (parent-linked) sessions.
            if let Some(ver) = header_version.take() {
                m.harness_extra_mut(Harness::OpenClaw)
                    .insert("session_version".into(), Value::from(ver));
            }
            // Hand session metadata to the sink before the first message (header consumers).
            if !meta_sent {
                sink.meta(&s);
                meta_sent = true;
            }
            if sink.message(m) == Flow::Stop {
                return s;
            }
        }
    }
    if !meta_sent {
        sink.meta(&s);
    }
    s
}

/// A System turn for a non-`message` entry: id / parent / timestamp from the entry, and — in the
/// harness bag — `record_type` plus every field the entry carries besides the structural ones and
/// the `summary`/`content` that become the turn's text, snake_cased.
fn entry_message(v: &Value, ty: &str, kind: MessageKind, origin: Origin) -> Message {
    let mut m = Message::of_kind(Role::System, kind, origin);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    let bag = m.harness_extra_mut(Harness::OpenClaw);
    bag.insert("record_type".into(), Value::from(ty));
    if let Some(obj) = v.as_object() {
        for (key, val) in obj {
            if matches!(
                key.as_str(),
                "type" | "id" | "parentId" | "timestamp" | "summary" | "content"
            ) || val.is_null()
            {
                continue;
            }
            bag.insert(snake(key), val.clone());
        }
    }
    m
}

/// A non-`message` canonical entry that carries context or a boundary the reader should see, as
/// the kinds vocabulary has it: `compaction` → a `CompactionBoundary` (the entry's own fields:
/// `first_kept_entry_id`, `tokens_before`, `from_hook`, `details`) followed by the
/// `CompactionSummary` that replaced the compacted span; `reset` → `Branch`; `model_change` /
/// `thinking_level_change` → `ModelChange`; `branch_summary` / `custom_message` (extension text
/// that IS in model context) → `Notice`. All origin Harness.
fn parse_control_entry(v: &Value) -> Vec<Message> {
    let Some(ty) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let kind = match ty {
        "compaction" => MessageKind::CompactionBoundary,
        "reset" => MessageKind::Branch,
        "model_change" | "thinking_level_change" => MessageKind::ModelChange,
        "branch_summary" | "custom_message" => MessageKind::Notice,
        _ => return Vec::new(),
    };
    let mut m = entry_message(v, ty, kind, Origin::Harness);
    let str_of = |k: &str| v.get(k).and_then(Value::as_str);
    let text = match ty {
        "compaction" => "[conversation compacted]".to_string(),
        "branch_summary" => str_of("summary").unwrap_or("").to_string(),
        "reset" => format!("[session reset: {}]", str_of("reason").unwrap_or("reset")),
        "model_change" => {
            let model = str_of("modelId").unwrap_or("");
            m.model = (!model.is_empty()).then(|| model.to_string());
            match str_of("provider") {
                Some(p) if !p.is_empty() => format!("[model: {p}/{model}]"),
                _ => format!("[model: {model}]"),
            }
        }
        "thinking_level_change" => match str_of("thinkingLevel") {
            Some(level) => format!("[thinking level: {level}]"),
            None => "[thinking level changed]".to_string(),
        },
        _ => coerce_content_text(v.get("content")),
    };
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }
    let mut out = vec![m];
    if ty == "compaction" {
        let summary = str_of("summary").unwrap_or("");
        if !summary.trim().is_empty() {
            let mut sm = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
            sm.parent_id = v.get("id").and_then(Value::as_str).map(str::to_string);
            sm.timestamp = entry_timestamp(v);
            sm.content.push(Block::Text {
                text: summary.to_string().into(),
            });
            sm.harness_extra_mut(Harness::OpenClaw)
                .insert("record_type".into(), Value::from(ty));
            out.push(sm);
        }
    }
    out
}

/// Under [`ParseOptions::complete`], a bookkeeping entry (`custom`, `leaf`, …) rides along verbatim
/// as a `Carrier` turn: the raw record at the top-level `_record`, its type in
/// `extra["openclaw"]["record_type"]`. The lean passes drop it.
fn carrier_if_complete(v: &Value, opts: &ParseOptions) -> Vec<Message> {
    if !opts.complete {
        return Vec::new();
    }
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    m.harness_extra_mut(Harness::OpenClaw)
        .insert("record_type".into(), Value::from(ty));
    m.extra.insert(crate::harness::claude::CARRIER_KEY.into(), v.clone());
    vec![m]
}

/// Under [`ParseOptions::complete`], a session-fact entry the user saw as a notice (`session_info`,
/// `label`) rides along as a `Notice` turn with `text`, the raw record at `_record` and its type in
/// the bag. Lean passes keep only the fact's first-class home.
fn notice_if_complete(v: &Value, opts: &ParseOptions, text: String) -> Vec<Message> {
    let mut out = carrier_if_complete(v, opts);
    if let Some(m) = out.first_mut() {
        m.kind = MessageKind::Notice;
        m.content.push(Block::Text { text: text.into() });
    }
    out
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

/// A `message` entry → its turn(s). The `compactionSummary` custom role (the pre-`compaction`-entry
/// form) yields two: the boundary (the entry's id and token counts) and the summary.
fn parse_entry(v: &Value) -> Vec<Message> {
    let Some(msg) = v.get("message") else {
        return Vec::new();
    };
    let Some(role_str) = msg.get("role").and_then(Value::as_str) else {
        return Vec::new();
    };
    let (role, kind, origin) = match role_str {
        "user" => (Role::User, MessageKind::Prompt, Origin::Human),
        "assistant" => (Role::Assistant, MessageKind::Reply, Origin::Model),
        "toolResult" => (Role::Tool, MessageKind::ToolResult, Origin::Harness),
        "system" => (Role::System, MessageKind::SystemPrompt, Origin::Harness),
        // Custom (declaration-merged) message types, as System turns: a `!cmd` run and its output
        // are flattened into the model's context; the summaries and extension markers are notices
        // (the compaction summary gets its boundary below).
        "bashExecution" => (Role::System, MessageKind::InjectedContext, Origin::Harness),
        "branchSummary" | "custom" => (Role::System, MessageKind::Notice, Origin::Harness),
        "compactionSummary" => (Role::System, MessageKind::CompactionSummary, Origin::Harness),
        _ => return Vec::new(),
    };
    let mut m = Message::of_kind(role, kind, origin);
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

    if m.content.is_empty() && m.extra.is_empty() {
        return Vec::new();
    }
    if role_str != "compactionSummary" {
        return vec![m];
    }
    // The old single-record form: the record IS the compaction. The boundary keeps the entry's id
    // (what a later `firstKeptEntryId` / label points at) and counts; the summary hangs off it.
    let mut boundary = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
    boundary.id = m.id.take();
    boundary.parent_id = m.parent_id.take();
    boundary.timestamp = m.timestamp;
    boundary.extra = std::mem::take(&mut m.extra);
    boundary.content.push(Block::Text {
        text: "[conversation compacted]".into(),
    });
    m.parent_id = boundary.id.clone();
    m.harness_extra_mut(Harness::OpenClaw)
        .insert("message_role".into(), Value::from(role_str));
    vec![boundary, m]
}

fn parse_tool_result(m: &mut Message, msg: &Value) {
    let tool_use_id = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
    // `toolName` and the arbitrary structured `details` payload (diffs, file lists) live on the
    // block, where every harness puts them.
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

/// Capture assistant-level metadata (provider/api/model/usage/stop/diagnostics) into the harness
/// bag, and an API failure into the `Error` kind (`extra["openclaw"]["error"]`).
fn capture_assistant_meta(m: &mut Message, msg: &Value) {
    let failed = msg
        .get("errorMessage")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty())
        || msg.get("stopReason").and_then(Value::as_str) == Some("error");
    if let Some(usage) = msg.get("usage") {
        m.usage = parse_usage(usage);
    }
    let mut bag = serde_json::Map::new();
    for key in [
        "api",
        "provider",
        "responseModel",
        "responseId",
        "stopReason",
        "diagnostics",
    ] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                bag.insert(snake(key), val.clone());
            }
        }
    }
    if let Some(usage) = msg.get("usage") {
        // The full usage object also carries per-bucket cost and `contextUsage`; keep it verbatim.
        bag.insert("usage_raw".into(), usage.clone());
    }
    if failed {
        let mut err = serde_json::Map::new();
        for key in ["errorMessage", "errorCode", "errorType"] {
            if let Some(val) = msg.get(key).filter(|v| !v.is_null()) {
                err.insert(snake(key), val.clone());
            }
        }
        bag.insert("error".into(), Value::Object(err));
        m.kind = MessageKind::Error;
        m.origin = Origin::Harness;
    }
    if !bag.is_empty() {
        m.harness_extra_mut(Harness::OpenClaw).extend(bag);
    }
}

/// `usage{input, output, cacheRead, cacheWrite, totalTokens, cost{…, total}}` → [`Usage`].
fn parse_usage(usage: &Value) -> Option<Usage> {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let u = Usage {
        input_tokens: get("input"),
        output_tokens: get("output"),
        cache_read_tokens: get("cacheRead"),
        cache_creation_tokens: get("cacheWrite"),
        reasoning_tokens: None,
        cost_usd: usage.pointer("/cost/total").and_then(Value::as_f64),
    };
    let any = u.input_tokens.is_some()
        || u.output_tokens.is_some()
        || u.cache_read_tokens.is_some()
        || u.cache_creation_tokens.is_some()
        || u.cost_usd.is_some();
    any.then_some(u)
}

/// The custom message roles keep their role name and fields in the harness bag.
fn custom_role_bag<'a>(
    m: &'a mut Message,
    role: &str,
    msg: &Value,
    keys: &[&str],
) -> &'a mut serde_json::Map<String, Value> {
    let bag = m.harness_extra_mut(Harness::OpenClaw);
    bag.insert("message_role".into(), Value::from(role));
    for key in keys {
        if let Some(val) = msg.get(*key) {
            if !val.is_null() {
                bag.insert(snake(key), val.clone());
            }
        }
    }
    bag
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
    custom_role_bag(
        m,
        "bashExecution",
        msg,
        &[
            "exitCode",
            "cancelled",
            "truncated",
            "fullOutputPath",
            "excludeFromContext",
        ],
    );
}

fn parse_branch_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    let bag = custom_role_bag(m, "branchSummary", msg, &[]);
    if let Some(from_id) = msg.get("fromId").and_then(Value::as_str) {
        bag.insert("branch_from_id".into(), Value::from(from_id));
    }
}

fn parse_compaction_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    custom_role_bag(
        m,
        "compactionSummary",
        msg,
        &["tokensBefore", "tokensAfter", "firstKeptEntryId"],
    );
}

fn parse_custom(m: &mut Message, msg: &Value) {
    let text = coerce_content_text(msg.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }
    let bag = custom_role_bag(m, "custom", msg, &["display"]);
    if let Some(ct) = msg.get("customType").and_then(Value::as_str) {
        bag.insert("custom_type".into(), Value::from(ct));
    }
    if let Some(details) = msg.get("details").filter(|d| !d.is_null()) {
        bag.insert("custom_details".into(), details.clone());
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
            namespace: None,
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
        // emits nothing, session_info sets the title, the compaction is a boundary + summary.
        let s = adapter.parse(r).unwrap();
        assert_eq!(s.title.as_deref(), Some("Named session"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(
            s.lineage.forked_from.as_deref(),
            Some("s-parent"),
            "header parentSession"
        );
        assert_eq!(s.extra["openclaw"]["session_name"], "Named session");
        assert_eq!(
            s.extra.len(),
            1,
            "session facts live only in the harness bag: {:?}",
            s.extra
        );
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "u2", "c1", "a3"]);
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::Prompt,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Reply,
            ]
        );
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert!(!texts.iter().any(|t| t.contains("side branch")));
        let c1 = &s.messages[3];
        assert_eq!((c1.role, c1.origin), (Role::System, Origin::Harness));
        assert_eq!(c1.extra["openclaw"]["record_type"], "compaction");
        assert_eq!(c1.extra["openclaw"]["tokens_before"], 1234);
        assert_eq!(c1.extra["openclaw"]["first_kept_entry_id"], "u2");
        let summary = &s.messages[4];
        assert_eq!(summary.text().as_deref(), Some("summary of earlier"));
        assert_eq!(
            summary.parent_id.as_deref(),
            Some("c1"),
            "the summary hangs off its boundary"
        );
        assert!(summary.id.is_none());
        // parents on the visible path are kept as written (u2 → a1); the leaf control itself is
        // never a parent of anything on the path.
        assert_eq!(s.messages[2].parent_id.as_deref(), Some("a1"));
        assert_eq!(c1.parent_id.as_deref(), Some("si"));
        // the session model is on the session, not repeated on every reply (IR diet)
        assert!(s.messages.iter().all(|m| m.model.is_none()), "{:?}", s.messages);
        for m in &s.messages {
            assert!(m.extra.keys().all(|k| k == "openclaw"), "{:?}", m.extra);
        }

        // Complete parse: everything rides along; off-branch rows are tagged.
        let mut sink = crate::stream::CollectSink::default();
        adapter.stream(r, &ParseOptions::complete(), &mut sink).unwrap();
        let ids: Vec<&str> = sink.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["u1", "a1", "a2", "l1", "u2", "si", "c1", "a3"]);
        let inactive: Vec<&str> = sink
            .messages
            .iter()
            // The bag only exists for messages that carry OpenClaw facts, so look it up rather
            // than indexing (indexing panics on the ones that have none).
            .filter(|m| {
                m.harness_extra(Harness::OpenClaw)
                    .and_then(|b| b.get("inactive_branch"))
                    == Some(&Value::Bool(true))
            })
            .filter_map(|m| m.id.as_deref())
            .collect();
        assert_eq!(inactive, ["a2", "l1"]);
        // the rewound reply keeps its kind; the leaf control is a carrier
        let a2 = sink.messages.iter().find(|m| m.id.as_deref() == Some("a2")).unwrap();
        assert_eq!(a2.kind, MessageKind::Reply);
        let l1 = sink.messages.iter().find(|m| m.id.as_deref() == Some("l1")).unwrap();
        assert_eq!(l1.kind, MessageKind::Carrier);
        assert_eq!(l1.extra["openclaw"]["record_type"], "leaf");
        assert_eq!(l1.extra[crate::harness::claude::CARRIER_KEY]["targetId"], "a1");
        // the session name is a notice under complete, carrying its record
        let si = sink.messages.iter().find(|m| m.id.as_deref() == Some("si")).unwrap();
        assert_eq!(si.kind, MessageKind::Notice);
        assert_eq!(si.extra["openclaw"]["record_type"], "session_info");
        assert_eq!(si.extra[crate::harness::claude::CARRIER_KEY]["name"], "Named session");
        assert_eq!(si.text().as_deref(), Some("[session named: Named session]"));
        for m in &sink.messages {
            assert!(
                m.extra
                    .keys()
                    .all(|k| k == "openclaw" || k == crate::harness::claude::CARRIER_KEY),
                "{:?}",
                m.extra
            );
        }
        fs::remove_dir_all(agents.parent().unwrap()).ok();
    }

    /// The store under `tests/fixtures/openclaw/openclaw-agent.sqlite` was written by OpenClaw's OWN
    /// code (`0e9181234a`, 2026-09-19): `tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts` drives
    /// `upsertSessionEntryCore` + `appendTranscriptMessage` + `SessionManager` (`appendSessionInfo`,
    /// `appendModelChange`, `appendLabelChange`, `appendCompaction`, `branch`, `appendLeafControl`
    /// with and without `appendMode: "side"`, `appendResetBoundary`, `createBranchedSession`) and
    /// lands the user's legacy v3 JSONL through `replaceTranscriptEventsSync`; the transcript tables
    /// were then copied verbatim (`.dump`), `PRAGMA user_version` 21. The expected visible path below
    /// is what OpenClaw's `selectVisibleTranscriptEvents` returned for the same rows.
    #[cfg(feature = "sqlite")]
    #[test]
    fn real_openclaw_store_matches_openclaws_own_visible_path() {
        let agents = tmp_agents();
        let agent_dir = agents.join("main").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/openclaw")
            .join("openclaw-agent.sqlite");
        let db = agent_dir.join("openclaw-agent.sqlite");
        fs::copy(&src, &db).unwrap();
        let adapter = OpenClaw {
            roots: vec![agents.clone()],
        };

        let refs = adapter.discover().unwrap();
        let by_id = |id: &str| {
            refs.iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("{id} discovered"))
        };
        assert_eq!(refs.len(), 3, "main + fork + migrated legacy window");
        let main = by_id("cvfix-main-0001");
        assert_eq!(main.path, db);
        assert_eq!(main.cwd, Some(PathBuf::from("/Users/ember/dev/cv")));
        assert_eq!(
            main.title.as_deref(),
            Some("cv fixture"),
            "the store label, as OpenClaw's list shows"
        );
        assert_eq!(main.message_count, 13, "every message row, branches included");
        let fork = by_id("01a0bb4e-2bea-749a-8ec2-c494636244d1");
        assert_eq!(fork.message_count, 12);
        let legacy = by_id("3f28190c-96fa-4d73-b37c-251dcccb3f9b");
        assert_eq!(legacy.cwd, Some(PathBuf::from("/Users/ember/ocfid/proj")));
        assert_eq!(legacy.message_count, 5);

        // OpenClaw's visible path for the main window. `session_info` and `label` are session
        // facts (they land in the session bag above, not as turns); `model_change` IS a turn —
        // `MessageKind::ModelChange`, the point the model switched (`docs/INTERFACE-V2.md` §4).
        // So: 4 turns, model change, compaction, 2 turns, 2 turns, reset, 2 turns.
        let s = adapter.parse(main).unwrap();
        assert_eq!(s.title.as_deref(), Some("cv fixture"), "`cv ls` and `cv show` agree");
        // OpenClaw session facts ride nested under `extra["openclaw"]`, never flat.
        let bag = s.harness_extra(Harness::OpenClaw).expect("openclaw session bag");
        assert_eq!(bag["session_name"], "cv fixture session");
        assert_eq!(bag["labels"]["a2"], "good answer");
        assert_eq!(
            s.extra.keys().collect::<Vec<_>>(),
            ["openclaw"],
            "no flat session keys besides the harness bag"
        );
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4.6"));
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(
            ids,
            [
                "u1",
                "a1",
                "t1",
                "a2",
                "2ebc2e14-15f4-4a7f-96b3-b53b0c85f19f",
                "a8809d72-24a8-48f1-af6e-96d73cdaea5e",
                "4f9b3064-6b25-4ab4-82fc-9267b84f8674",
                "8aa23aa2-040c-404e-872f-73a4225f184b",
                "cdb5934d-842c-4b9d-b31a-50ab73e44a2d",
                "dadcbccc-cecb-4f1a-97c8-b396b5e2cf7f",
                "7320572f-1b0e-4e42-bcaf-205f8b5d81f3",
                "0ca1dba7-abc2-46fa-8acb-d6b142a2373c",
                "9c7327ea-0d4d-4b4a-94d2-cbe37776ee5f",
            ]
        );
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        for gone in [
            "branch: rename README",
            "branch answer: renamed.",
            "side note appended in side mode",
        ] {
            assert!(
                !texts.iter().any(|t| t.contains(gone)),
                "{gone} is off the visible path"
            );
        }
        // Every OpenClaw fact is nested under `extra["openclaw"]` — no flat keys survive.
        let ocbag = |m: &Message| m.harness_extra(Harness::OpenClaw).expect("openclaw bag").clone();
        let switch = &s.messages[4];
        assert_eq!(switch.kind, MessageKind::ModelChange);
        assert_eq!(
            switch.model.as_deref(),
            Some("claude-opus-4.6"),
            "a ModelChange names the model it switched TO, even though the session default is the first one"
        );
        assert_eq!(ocbag(switch)["record_type"], "model_change");
        let compaction = &s.messages[5];
        assert_eq!(compaction.role, Role::System);
        assert_eq!(compaction.kind, MessageKind::CompactionBoundary);
        assert_eq!(ocbag(compaction)["record_type"], "compaction");
        assert_eq!(ocbag(compaction)["tokens_before"], 4321);
        assert_eq!(ocbag(compaction)["first_kept_entry_id"], "u1");
        assert_eq!(compaction.text().as_deref(), Some("[conversation compacted]"));
        // The summary that seeds the next window is its own turn, hanging off the boundary; it is
        // the store's synthetic text, so it carries no entry id (hence the id list skips it).
        let summary = &s.messages[6];
        assert_eq!(summary.kind, MessageKind::CompactionSummary);
        assert!(summary.text().unwrap().starts_with("Summary: the user asked"));
        assert!(summary.id.is_none());
        let reset = &s.messages[11];
        assert_eq!(ocbag(reset)["record_type"], "reset");
        assert_eq!(ocbag(reset)["reason"], "reset");
        crate::harness::assert_no_flat_keys(&s);
        // the assistant turn carries thinking + the tool call; the tool result names its tool
        assert!(s.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. })));
        assert!(s.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "bash")));
        assert_eq!(s.messages[2].role, Role::Tool);

        // complete: the abandoned branch, the side-mode note and the leaf controls ride along, tagged
        let mut sink = crate::stream::CollectSink::default();
        adapter.stream(main, &ParseOptions::complete(), &mut sink).unwrap();
        let inactive: Vec<String> = sink
            .messages
            .iter()
            .filter(|m| {
                m.harness_extra(Harness::OpenClaw)
                    .and_then(|b| b.get("inactive_branch"))
                    == Some(&Value::Bool(true))
            })
            .filter_map(|m| m.text())
            .collect();
        assert!(inactive.iter().any(|t| t.contains("branch: rename README")));
        assert!(inactive.iter().any(|t| t.contains("side note appended in side mode")));
        assert_eq!(
            sink.messages
                .iter()
                .filter(|m| {
                    m.harness_extra(Harness::OpenClaw)
                        .and_then(|b| b.get("record_type"))
                        .and_then(Value::as_str)
                        == Some("leaf")
                })
                .count(),
            3,
            "three leaf controls in the store"
        );

        // the fork: v4 header with `parentSession`, the inherited prefix, then its own turns
        let f = adapter.parse(fork).unwrap();
        // The fork pointer is first-class lineage now, not a bag key.
        assert_eq!(f.lineage.forked_from.as_deref(), Some("cvfix-main-0001"));
        let ftexts: Vec<String> = f.messages.iter().filter_map(|m| m.text()).collect();
        assert!(ftexts.iter().any(|t| t == "post-reset answer"));
        assert_eq!(ftexts.last().map(String::as_str), Some("fork answer"));
        // `createBranchedSession` copies only the visible path, so the fork has no abandoned
        // branch: its 12 message rows plus the model change, the compaction boundary and its
        // summary, and the reset note — all on the path.
        assert_eq!(f.messages.len(), 16);
        assert!(!f.messages.iter().any(|m| m
            .harness_extra(Harness::OpenClaw)
            .is_some_and(|b| b.contains_key("inactive_branch"))));

        // the legacy v3 transcript, as a migration lands it
        let l = adapter.parse(legacy).unwrap();
        assert_eq!(l.messages.len(), 5);
        assert_eq!(l.messages[2].role, Role::Assistant);
        assert_eq!(l.messages[3].role, Role::Tool);
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
        assert_eq!(s.extra["openclaw"]["labels"]["u2"], "pinned prompt");
        let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(
            ids,
            ["mc", "u1", "r1", "u2", "cm"],
            "a model change is a turn the reader sees"
        );
        let mc = &s.messages[0];
        assert_eq!((mc.kind, mc.origin), (MessageKind::ModelChange, Origin::Harness));
        assert_eq!(
            mc.model.as_deref(),
            Some("claude-sonnet-5"),
            "a ModelChange always names the new model"
        );
        assert_eq!(mc.text().as_deref(), Some("[model: anthropic/claude-sonnet-5]"));
        assert_eq!(mc.extra["openclaw"]["record_type"], "model_change");
        assert_eq!(mc.extra["openclaw"]["provider"], "anthropic");
        let reset = &s.messages[2];
        assert_eq!((reset.role, reset.kind), (Role::System, MessageKind::Branch));
        assert_eq!(reset.text().as_deref(), Some("[session reset: idle]"));
        assert_eq!(reset.extra["openclaw"]["reason"], "idle");
        let cm = &s.messages[4];
        assert_eq!(cm.kind, MessageKind::Notice);
        assert_eq!(cm.extra["openclaw"]["custom_type"], "ext.note");
        assert_eq!(cm.extra["openclaw"]["display"], true);
        assert_eq!(cm.text().as_deref(), Some("an extension note"));

        let c = parse_text_with(&text, &r, &ParseOptions::complete());
        let ids: Vec<&str> = c.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["mc", "u1", "r1", "u2", "lb", "cx", "cm"]);
        let cx = c.messages.iter().find(|m| m.id.as_deref() == Some("cx")).unwrap();
        assert_eq!(cx.kind, MessageKind::Carrier);
        assert_eq!(cx.extra["openclaw"]["record_type"], "custom");
        assert_eq!(cx.extra[crate::harness::claude::CARRIER_KEY]["data"]["ttl"], 60);
        let lb = c.messages.iter().find(|m| m.id.as_deref() == Some("lb")).unwrap();
        assert_eq!(lb.kind, MessageKind::Notice);
        assert_eq!(lb.text().as_deref(), Some("[label u2: pinned prompt]"));
        assert_eq!(lb.extra[crate::harness::claude::CARRIER_KEY]["targetId"], "u2");
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
        assert_eq!(s.messages[1].model, None, "the session's own model is not repeated");
        let user = &s.messages[0];
        assert_eq!(user.role, Role::User);
        assert_eq!((user.kind, user.origin), (MessageKind::Prompt, Origin::Human));
        assert_eq!(user.text().as_deref(), Some("hello"));
        // version stashed on the first message
        assert_eq!(user.extra["openclaw"]["session_version"], 3);

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!((asst.kind, asst.origin), (MessageKind::Reply, Origin::Model));
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
        // usage (+ cost) first-class, raw usage in the bag
        let u = asst.usage.as_ref().expect("usage");
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(5));
        assert_eq!(u.cost_usd, Some(0.03));
        let bag = &asst.extra["openclaw"];
        assert!(bag.get("usage_raw").is_some());
        assert_eq!(bag["provider"], "anthropic");
        assert_eq!(bag["stop_reason"], "toolUse");
        assert_eq!(asst.extra.len(), 1, "nothing flat: {:?}", asst.extra);

        let tr = &s.messages[2];
        assert_eq!(tr.role, Role::Tool);
        assert_eq!((tr.kind, tr.origin), (MessageKind::ToolResult, Origin::Harness));
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
        // the tool's name and details live on the block, not in extra
        assert!(tr.extra.is_empty(), "{:?}", tr.extra);
        if let Block::ToolResult { details, .. } = &tr.content[0] {
            assert!(details.is_some());
        }
    }

    #[test]
    fn parses_legacy_v1_linear() {
        let s = parse_fixture("v1-linear.jsonl");
        assert_eq!(s.messages.len(), 2);
        // v1 entries have no parentId
        assert!(s.messages[0].parent_id.is_none());
        assert_eq!(s.messages[0].extra["openclaw"]["session_version"], 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("legacy first"));
    }

    #[test]
    fn parses_custom_message_types() {
        let s = parse_fixture("custom-messages.jsonl");
        // bashExecution, branchSummary, compactionSummary (→ boundary + summary), custom — all
        // System turns
        assert_eq!(s.messages.len(), 5);
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::InjectedContext,
                MessageKind::Notice,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Notice,
            ]
        );
        assert!(s
            .messages
            .iter()
            .all(|m| m.role == Role::System && m.origin == Origin::Harness));

        let bash = &s.messages[0];
        assert_eq!(bash.extra["openclaw"]["message_role"], "bashExecution");
        assert!(bash.text().unwrap().contains("$ ls -la"));
        assert_eq!(bash.extra["openclaw"]["exit_code"], 0);

        let branch = &s.messages[1];
        assert_eq!(branch.extra["openclaw"]["message_role"], "branchSummary");
        assert_eq!(branch.extra["openclaw"]["branch_from_id"], "abc123");

        // the single-record compaction: the boundary keeps the entry id and counts, the summary
        // its text, hanging off the boundary
        let boundary = &s.messages[2];
        assert_eq!(boundary.id.as_deref(), Some("c3"));
        assert_eq!(boundary.parent_id.as_deref(), Some("c2"));
        assert_eq!(boundary.extra["openclaw"]["message_role"], "compactionSummary");
        assert_eq!(boundary.extra["openclaw"]["tokens_before"], 5000);
        let summary = &s.messages[3];
        assert_eq!(summary.text().as_deref(), Some("earlier history compacted"));
        assert_eq!(summary.parent_id.as_deref(), Some("c3"));
        assert_eq!(summary.extra["openclaw"]["message_role"], "compactionSummary");
        let comps = crate::compaction::detect_in_session(&s, true);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(3));

        let custom = &s.messages[4];
        assert_eq!(custom.extra["openclaw"]["message_role"], "custom");
        assert_eq!(custom.extra["openclaw"]["custom_type"], "openclaw.cache-ttl");
        assert_eq!(custom.extra["openclaw"]["custom_details"]["ttlMs"], 3600000);
    }

    #[test]
    fn assistant_api_failure_is_an_error_turn() {
        let text = [
            r#"{"type":"session","version":4,"id":"s-e","timestamp":"2026-09-19T12:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-19T12:00:01.000Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-19T12:00:02.000Z","message":{"role":"assistant","model":"claude-opus-4","provider":"anthropic","stopReason":"error","errorMessage":"overloaded","errorCode":529,"errorType":"api","content":[]}}"#,
            r#"{"type":"message","id":"s1","parentId":"a1","timestamp":"2026-09-19T12:00:03.000Z","message":{"role":"system","content":"You are OpenClaw."}}"#,
        ]
        .join("\n");
        let s = parse_text(&text, &fixture_ref("unused.jsonl"));
        let a1 = &s.messages[1];
        assert_eq!(
            (a1.role, a1.kind, a1.origin),
            (Role::Assistant, MessageKind::Error, Origin::Harness)
        );
        assert_eq!(a1.extra["openclaw"]["error"]["error_message"], "overloaded");
        assert_eq!(a1.extra["openclaw"]["error"]["error_code"], 529);
        assert_eq!(a1.extra["openclaw"]["stop_reason"], "error");
        assert!(
            a1.extra["openclaw"].get("error_message").is_none(),
            "the error is one object"
        );
        // a `system` record is the system prompt, session-level too
        let sys = &s.messages[2];
        assert_eq!((sys.kind, sys.origin), (MessageKind::SystemPrompt, Origin::Harness));
        assert_eq!(s.system_prompt.as_deref(), Some("You are OpenClaw."));
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
        assert_eq!(reply.extra["openclaw"]["provider"], "openclaw");
        assert_eq!(reply.text().as_deref(), Some("done bridging"));
        // bridged turns have no captured tool calls / thinking — confirm the gap
        assert!(reply.content.iter().all(|b| matches!(b, Block::Text { .. })));
    }
}
