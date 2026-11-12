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
//! 2026.10 (`OPENCLAW_AGENT_SCHEMA_VERSION = 24`) compresses: an event of at least 1 KiB is a zstd
//! frame in `transcript_events.event_zstd` with `event_json` NULL ([`EventReader`] decodes both).
//! Sub-agents (`agent:<id>:subagent:<uuid>` keys) name their spawner's key in `spawnedBy` /
//! `session_windows.spawned_by`; [`KeyFacts`] turns that into `Lineage::parent`.
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
//! `selectSessionTranscriptActiveEntries`), not file order. [`select_navigation`] ports that scan; it only
//! runs when a transcript holds a `leaf` control (OpenClaw's own rule for legacy flat readers,
//! `selectSessionTranscriptLeafControlledPath`). Entries off the active branch are dropped by the
//! lean passes and carried, tagged `extra["openclaw"]["inactive_branch"]`, under
//! [`Demand::COMPLETE`].
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
//! and surface only under [`Demand::COMPLETE`], as `Notice` turns; `custom`, `leaf`,
//! `thinking_level_change`-like bookkeeping and unknown types surface only under `complete`, as
//! `Carrier` turns. Every carried record is the top-level `_record`; every harness fact is in
//! `extra["openclaw"]` (`record_type` = the entry type, `message_role` = a custom message role,
//! `session_version` on the first turn, `inactive_branch: true` off the visible path, the
//! assistant's `api`/`provider`/`responseModel`/`responseId`/`stopReason`/`diagnostics`/
//! `usage_raw`, and each control entry's own fields under their native camelCase keys). The
//! header's `parentSession` is `Lineage::forked_from`; `usage.cost.total` is `Usage::cost_usd`;
//! `Message::model` is each record's own recorded model and `Session::model` the first
//! non-synthetic one seen.

use crate::json::{ReadInput, Validation};
mod discovery;
mod emit;
mod record;
mod registry;
pub use emit::emit;

#[cfg(feature = "sqlite")]
use super::store::sqlite::open_ro;
#[cfg(feature = "sqlite")]
use super::ts_from_epoch;
use super::wire::anthropic;
use super::{parse_ts, ts_from_value, Adapter};
use crate::ir::*;
use crate::source::{sqlite_sidecars, Dependencies, Probe};
use crate::stream::{Demand, Flow, MessageSink, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenClaw {
    roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
/// OpenClaw 10.1's deferred-tool wrapper: `tool_call{id, args}`.
pub(crate) const DEFERRED_WRAPPER: crate::affordance::DeferredWrapper = crate::affordance::DeferredWrapper {
    name_key: "id",
    args_key: "args",
};

pub(crate) static ROOTLESS: OpenClaw = OpenClaw { roots: Vec::new() };

impl OpenClaw {
    pub fn detect() -> Self {
        let mut roots = Vec::new();
        // `OPENCLAW_STATE_DIR` overrides the state dir wholesale (`src/config/state-dir.ts`
        // `resolveStateDir`); otherwise `~/.openclaw` (and the elide-home variant).
        let override_dir = crate::env::var_os("OPENCLAW_STATE_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        for base in [
            override_dir,
            crate::env::home_dir().map(|h| h.join(".openclaw")),
            crate::env::home_dir().map(|h| h.join("elide-home").join(".openclaw")),
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

    /// An adapter rooted at one OpenClaw state dir (what `OPENCLAW_STATE_DIR` names), reading its
    /// `agents/` tree only; never consults the environment.
    pub fn with_state_dir(state_dir: &Path) -> Self {
        OpenClaw {
            roots: vec![state_dir.join("agents")],
        }
    }
}

impl Adapter for OpenClaw {
    fn harness(&self) -> Harness {
        Harness::OpenClaw
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        let Some(state) = r.path.ancestors().nth(4) else {
            return Some(());
        };
        let ext = r.path.extension().and_then(|e| e.to_str());
        if ext.is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3")) {
            let db = state.join("state/openclaw.sqlite");
            deps.files.extend(sqlite_sidecars(&db));
            deps.files.push(db);
        } else {
            if let Some(dir) = r.path.parent() {
                deps.files.push(dir.join("sessions.json"));
            }
            deps.files.push(state.join("cron/jobs.json"));
            deps.files.push(state.join("subagents/runs.json"));
        }
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        // An unreadable store, dir or transcript is skipped and reported.
        let mut out = crate::Discovered::default();
        // Sessions the sqlite store owns; a legacy JSONL twin (pre-migration copy) is skipped.
        let mut seen: HashSet<String> = HashSet::new();
        let mut indexes = discovery::Indexes::default();
        for root in &self.roots {
            // 1. The sqlite store (OpenClaw ≥ 2026-07-11): agents/<agentId>/agent/openclaw-agent*.sqlite
            for db in sqlite_stores(root) {
                for r in out.keep(&db, discover_store(&db)).into_iter().flatten() {
                    seen.insert(r.id.clone());
                    out.refs.push(r);
                }
            }
            // 2. Legacy JSONL transcripts at agents/<agentId>/sessions/<sid>.jsonl.
            for entry in WalkDir::new(root) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        out.skipped.extend(crate::Skipped::walk(&e));
                        continue;
                    }
                };
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
                let index = indexes.get(path.parent().unwrap());
                let scanned = discovery::scan(path, index).map(|r| (!seen.contains(&r.id)).then_some(r));
                if let Some(found) = crate::Skipped::listed(path, scanned) {
                    out.push(found);
                }
            }
        }
        indexes.check()?;
        Ok(out)
    }

    /// Every window's certificate in an agent's `.sqlite` store ([`sqlite_rows`]); every JSONL
    /// session's in the registries beside its transcript ([`registry::jsonl_rows`]).
    #[cfg(feature = "sqlite")]
    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        if !is_sqlite_store_path(store) {
            return registry::jsonl_rows(store);
        }
        open_ro(store).ok()?.read(|conn, _| sqlite_rows(conn)).ok()
    }

    #[cfg(not(feature = "sqlite"))]
    fn rows(&self, store: &Path) -> Option<std::collections::HashMap<String, crate::source::Rows>> {
        (!is_sqlite_store_path(store))
            .then(|| registry::jsonl_rows(store))
            .flatten()
    }

    /// A JSONL session reads its own entries of `sessions.json`, `cron/jobs.json` and
    /// `subagents/runs.json`, which every session of the agent (and the gateway) rewrites: it is
    /// certified by them, its store the directory of its transcript.
    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        if is_sqlite_store_path(&r.path) {
            return Some(r.path.clone());
        }
        let dir = r.path.parent()?;
        (dir.file_name()? == "sessions" && r.path.ancestors().nth(4).is_some()).then(|| dir.to_path_buf())
    }

    fn rows_files(&self, store: &Path) -> Vec<PathBuf> {
        if is_sqlite_store_path(store) {
            return vec![store.to_path_buf()];
        }
        let mut files = vec![store.join("sessions.json")];
        if let Some(state) = store.ancestors().nth(3) {
            files.push(state.join("cron/jobs.json"));
            files.push(state.join("subagents/runs.json"));
        }
        files
    }

    /// An agent's `.sqlite` store is written in place; a JSONL transcript is a file like any other.
    fn watch_policy(&self, r: &SessionRef) -> super::WatchPolicy {
        if is_sqlite_store_path(&r.path) {
            super::WatchPolicy::Database
        } else {
            super::WatchPolicy::Tree
        }
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            denials: &[],
            edit_guard: None,
            lane_stops: None,
            // OpenClaw 10.1 routes lazily loaded tools through `tool_call{id, args}` and its reader
            // leaves the wrapper in place.
            deferred_wrapper: Some(DEFERRED_WRAPPER),
            rich_spans: false,
        }
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::OpenClaw, sink)
                .unwrapping(self.capabilities().deferred_wrapper);
            let sink: &mut dyn MessageSink = &mut spawns;
            if is_sqlite_store_path(&r.path) {
                return stream_sqlite(r, opts, sink);
            }
            with_jsonl_source(&r.path, |source| {
                let mut reads = crate::source::ReadSet::default();
                let facts = jsonl_key_facts_with_reads(&r.path, &r.id, &mut |path| sink.dependency(path), &mut reads)?;
                // Retained bodies depend on the inventory and sibling transcripts consulted
                // for lineage, not only on their own unchanged bytes. The facts were read from
                // the first capture's bytes: the guarded capture must still hold them.
                let guarded = crate::json::JsonSource::capture_with_reads(&r.path, &reads)?;
                anyhow::ensure!(source.admits(&guarded), "JSON source changed: {}", r.path.display());
                stream_jsonl(r, &facts, opts, sink, &guarded)
            })
        })
    }

    /// The cron and sub-agent registries in the state dir that name this session's key
    /// ([`registry`]).
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        registry::affordances(r)
    }
}

/// Capture before registry/branch selection. A completed read was checked at its EOF
/// ([`stream_jsonl`]); a failed one checks here, so a changed source outranks the error it caused.
fn with_jsonl_source<T>(path: &Path, read: impl FnOnce(&crate::json::JsonSource) -> Result<T>) -> Result<T> {
    let source = crate::json::JsonSource::capture(path)?;
    let result = read(&source);
    if result.is_err() {
        source.check()?;
    }
    result
}

/// Is this entry an accepted `leaf` navigation control?
fn is_leaf_entry(view: crate::json::JsonView<'_>) -> Result<bool> {
    let ty = super::store::document::Fields::parse(view, &["type"])?
        .map(|f| f.string("type"))
        .transpose()?
        .flatten();
    Ok(ty.as_deref() == Some("leaf") && is_leaf_control(&record::navigation(view)?))
}

/// A JSONL transcript through the record codec. A transcript with an accepted `leaf` control is a
/// tree: the visible branch is selected over every entry's navigation fields (see
/// [`select_navigation`]) and only the selected records are decoded, by their source ranges.
/// Otherwise every line streams in file order, a rejected line costing only itself.
fn stream_jsonl(
    r: &SessionRef,
    facts: &KeyFacts,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    source: &crate::json::JsonSource,
) -> Result<Session> {
    use super::store::jsonl::Read;
    let read = Read::with(source.clone()).with_context(|| format!("reading {}", r.path.display()))?;
    crate::stream::guarded(sink, &read, |sink| stream_lines(r, facts, opts, sink, &read))
}

/// [`stream_jsonl`] over the opened source, its callbacks guarded.
fn stream_lines(
    r: &SessionRef,
    facts: &KeyFacts,
    opts: &Demand,
    sink: &mut dyn MessageSink,
    read: &super::store::jsonl::Read,
) -> Result<Session> {
    use super::store::jsonl::ranges;
    let source = read.source();
    let mut tree = false;
    read.lines(0, |line| {
        tree = line.view.map(is_leaf_entry).transpose()?.unwrap_or(false);
        Ok(if tree { Flow::Stop } else { Flow::Continue })
    })?;
    let mut fold = Fold::new(r, facts, opts);
    if tree {
        let mut descriptors = Vec::new();
        let mut records = Vec::new();
        read.lines(0, |line| {
            if let Some(view) = line.view {
                descriptors.push(record::navigation(view)?);
                records.push(view.source_range()?.context("a transcript line has a source range")?);
            } else if !line.blank {
                anyhow::ensure!(
                    ranges::valid_utf8(&mut ranges::reader(
                        read,
                        ranges::Range {
                            offset: line.at,
                            len: line.end - line.at,
                        }
                    )?)?,
                    "stream did not contain valid UTF-8"
                );
            }
            Ok(Flow::Continue)
        })?;
        let navigation = select_navigation(&descriptors, opts.tier == Tier::Complete);
        drop(descriptors);
        for row in navigation {
            let (offset, len) = records[row.index];
            let record = source.read(ReadInput::Range(offset, len), Validation::Value, |view| {
                record::record(view, opts)
            })?;
            if fold.push(record, row.active, row.parent, sink)? == Flow::Stop {
                break;
            }
        }
    } else {
        read.lines(0, |line| match line.view {
            Some(view) => fold.push(record::record(view, opts)?, true, None, sink),
            None => Ok(Flow::Continue),
        })?;
    }
    let session = fold.finish(sink);
    read.check()?;
    Ok(session)
}

/// Parse an in-memory OpenClaw JSONL transcript (v1–v4, including `leaf` navigation trees),
/// used by [`crate::ingest`] (wasm-safe).
pub(crate) fn parse_str(id: &str, text: &str) -> Result<Session> {
    use super::store::jsonl;
    use crate::harness::store::document::range::RangeSource;
    let opts = Demand::RICH;
    let r = SessionRef::new(id, Harness::OpenClaw, std::path::PathBuf::new());
    let facts = KeyFacts::default();
    let mut collect = crate::stream::CollectSink::default();
    let mut tree = false;
    jsonl::slice(text.as_bytes(), |line| {
        if let Some(view) = line.view {
            if is_leaf_entry(view)? {
                tree = true;
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    })?;
    let mut fold = Fold::new(&r, &facts, &opts);
    {
        let mut spawns =
            crate::affordance::AffordanceSink::new(Harness::OpenClaw, &mut collect).unwrapping(Some(DEFERRED_WRAPPER));
        if tree {
            let mut descriptors = Vec::new();
            let mut line_slices: Vec<(usize, usize)> = Vec::new();
            jsonl::slice(text.as_bytes(), |line| {
                if let Some(view) = line.view {
                    descriptors.push(record::navigation(view)?);
                    line_slices.push((line.at as usize, line.end as usize));
                }
                Ok(Flow::Continue)
            })?;
            let nav = select_navigation(&descriptors, false);
            for row in nav {
                let (start, end) = line_slices[row.index];
                let slice = &text[start..end];
                let src = RangeSource::text(None, slice, 0, false);
                if let Some(val) = src.accept(src.range(), Validation::Value)? {
                    let rec = record::record(crate::json::JsonView::of(&src, val), &opts)?;
                    if fold.push(rec, row.active, row.parent, &mut spawns)? == Flow::Stop {
                        break;
                    }
                }
            }
        } else {
            jsonl::slice(text.as_bytes(), |line| {
                if let Some(view) = line.view {
                    let _ = fold.push(record::record(view, &opts)?, true, None, &mut spawns)?;
                }
                Ok(Flow::Continue)
            })?;
        }
        let mut session = fold.finish(&mut spawns);
        drop(spawns);
        session.messages = collect.messages;
        session.source_path = None;
        Ok(session)
    }
}

/// Folds decoded records into the session in emission order and hands their messages to the sink.
/// The session's `messages` stay empty.
struct Fold {
    s: Session,
    prompt_origin: Option<Origin>,
    header_version: Option<i64>,
    meta_sent: bool,
    /// Without spans, a complete record keeps its owned value only (as a materialized read does).
    owned: Option<crate::lazy::Resolver>,
}

impl Fold {
    fn new(r: &SessionRef, facts: &KeyFacts, opts: &Demand) -> Self {
        let mut s = Session {
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            source_path: Some(r.path.clone()),
            ..Session::new(r.id.clone(), Harness::OpenClaw)
        };
        s.lineage.parent = facts.parent.clone();
        for (k, v) in [("session_key", &facts.session_key), ("spawned_by", &facts.spawned_by)] {
            if let Some(v) = v {
                s.harness_extra_mut(Harness::OpenClaw)
                    .insert(k.into(), Value::from(v.as_str()));
            }
        }
        Fold {
            s,
            prompt_origin: facts.prompt_origin(),
            header_version: None,
            meta_sent: false,
            owned: (opts.tier == Tier::Complete && !opts.spans)
                .then(|| crate::lazy::Resolver::new(Some(r.path.clone()))),
        }
    }

    /// One record, with the branch selection's verdict: `active` (always true without `leaf`
    /// controls) and the normalized parent (`Some(_)` when the tree rewrote it).
    fn push(
        &mut self,
        record: record::Record,
        active: bool,
        parent: Option<Option<String>>,
        sink: &mut dyn MessageSink,
    ) -> Result<Flow> {
        let s = &mut self.s;
        if let Some(header) = record.header {
            self.header_version = header.version;
            if let Some(v) = header.record {
                s.harness_extra_mut(Harness::OpenClaw).insert("header_record".into(), v);
            }
            if s.cwd.is_none() {
                s.cwd = header.cwd.map(PathBuf::from);
            }
            if s.created_at.is_none() {
                s.created_at = header.created_at;
            }
            // v4 fork lineage: the transcript this one was branched from.
            if let Some(parent) = header.forked_from {
                s.lineage.forked_from = Some(parent);
            }
            return Ok(Flow::Continue);
        }
        if let Some(name) = record.session_name {
            // Discovery already applied OpenClaw's own precedence (store label / display_name
            // first, then this name — what its session list shows), so a titled ref keeps its
            // title and `cv ls` and `cv show` agree; the name is still first-class in the bag.
            if s.title.is_none() {
                s.title = Some(crate::ir::truncate(&name, crate::projection::Head::TITLE));
            }
            s.harness_extra_mut(Harness::OpenClaw)
                .insert("session_name".into(), Value::from(name));
        }
        if let Some((target, label)) = record.label {
            let labels = s
                .harness_extra_mut(Harness::OpenClaw)
                .entry("labels")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(map) = labels.as_object_mut() {
                map.insert(target, Value::from(label));
            }
        }
        let mut msgs = record.messages;
        for (i, m) in msgs.iter_mut().enumerate() {
            // A cron run's or a sub-agent's prompts were written by the scheduler / the parent
            // agent, not typed: the session key says which.
            if let Some(o) = self.prompt_origin {
                if m.kind == MessageKind::Prompt && m.origin == Origin::Human {
                    m.origin = o;
                }
            }
            // The branch selection's normalized parent applies to the entry's own turn; a second
            // turn split off the same entry (a compaction summary) already points at the first.
            if i == 0 {
                if let Some(parent) = &parent {
                    m.parent_id = parent.clone();
                }
            }
            if !active {
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
            if m.kind == MessageKind::SystemPrompt && s.system_prompt.is_none() {
                s.system_prompt = m.text();
            }
            // Stash the transcript schema version on the first message so a downstream consumer
            // can tell v1/v2 (linear) from v3/v4 (parent-linked) sessions.
            if let Some(ver) = self.header_version.take() {
                m.harness_extra_mut(Harness::OpenClaw)
                    .insert("session_version".into(), Value::from(ver));
            }
        }
        if let Some(native) = record.native {
            CompleteRecord::attach_replay(&mut msgs, Harness::OpenClaw, native, serde_json::Map::new());
        }
        for mut m in msgs {
            if let Some(resolver) = &self.owned {
                m.materialize(resolver)?;
            }
            // Hand session metadata to the sink before the first message (header consumers).
            if !self.meta_sent {
                sink.meta(s);
                self.meta_sent = true;
            }
            if sink.message(m) == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(self, sink: &mut dyn MessageSink) -> Session {
        if !self.meta_sent {
            sink.meta(&self.s);
        }
        self.s
    }
}

/// What the store's index says about a session beyond its transcript: its session key (`sessions.json`
/// map key / `session_windows.session_key`), the key of the session that spawned it
/// (`spawnedBy` / `session_windows.spawned_by`), and that spawner's session id when it can be pinned.
#[derive(Default)]
struct KeyFacts {
    session_key: Option<String>,
    spawned_by: Option<String>,
    parent: Option<String>,
}

impl KeyFacts {
    /// `agent:<id>:cron:<job>[:run:<id>]` is an isolated cron run; `agent:<id>:subagent:<uuid>` a
    /// sub-agent (`sessions_spawn`). Their prompts come from the scheduler / the parent agent.
    fn prompt_origin(&self) -> Option<Origin> {
        let key = self.session_key.as_deref()?;
        let mut parts = key.split(':');
        if parts.next() != Some("agent") {
            return None;
        }
        match parts.nth(1)? {
            "cron" => Some(Origin::Scheduler),
            "subagent" => Some(Origin::Subagent),
            _ => None,
        }
    }
}

// Enough room for the childSessionKey field plus quotes, colon and JSON-escaped delimiters.
const CHILD_KEY_LOOKBEHIND: usize = 24;

/// Whether `text` records `key` as a spawned child ([`ChildMarker`]'s policy over one string).
#[cfg(test)]
fn names_child(text: &str, key: &str) -> bool {
    text.match_indices(key).any(|(i, _)| {
        let start = text.floor_char_boundary(i.saturating_sub(CHILD_KEY_LOOKBEHIND));
        text[start..i].contains("childSessionKey")
    })
}

/// Whether bytes fed in any chunking record `key` as a spawned child: a `childSessionKey` field
/// (plain, or JSON-escaped inside a tool result's text) within 24 bytes before an occurrence of
/// the key. A bare mention, such as a `sessions_list` result, does not count. A marker and its key
/// may cross any chunk boundary; only a key-sized window is retained.
struct ChildMarker<'a> {
    key: &'a str,
    window: Vec<u8>,
    found: bool,
}
impl<'a> ChildMarker<'a> {
    fn new(key: &'a str) -> Self {
        ChildMarker {
            key,
            window: Vec::new(),
            found: false,
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(64 * 1024) {
            if self.found {
                return;
            }
            self.window.extend_from_slice(chunk);
            self.found = memchr::memmem::find_iter(&self.window, self.key.as_bytes()).any(|at| {
                memchr::memmem::find(
                    &self.window[at.saturating_sub(CHILD_KEY_LOOKBEHIND)..at],
                    b"childSessionKey",
                )
                .is_some()
            });
            let keep = self.key.len().saturating_add(CHILD_KEY_LOOKBEHIND);
            let start = self.window.len().saturating_sub(keep);
            self.window.copy_within(start.., 0);
            self.window.truncate(self.window.len() - start);
        }
    }
}
impl std::io::Write for ChildMarker<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.feed(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`ChildMarker`] over a candidate transcript, with whole-file UTF-8 rejection.
fn names_child_file(path: &Path, key: &str) -> Result<bool> {
    use super::store::jsonl::ranges;
    use std::io::Read;
    struct Scan<'a, R> {
        reader: R,
        marker: ChildMarker<'a>,
    }
    impl<R: Read> Read for Scan<'_, R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let length = self.reader.read(bytes)?;
            self.marker.feed(&bytes[..length]);
            Ok(length)
        }
    }
    let file = fs::File::open(path)?;
    let range = ranges::Range {
        offset: 0,
        len: file.metadata()?.len(),
    };
    let scanner = Scan {
        reader: ranges::reader(&file, range)?,
        marker: ChildMarker::new(key),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, scanner);
    // Keep consuming after a match: invalid UTF-8 anywhere made the original read_to_string
    // reject this candidate, including in a suffix after a perfectly valid spawn result.
    let valid = ranges::valid_utf8(&mut reader)?;
    Ok(valid && reader.into_inner().marker.found)
}

/// [`KeyFacts`] from the `sessions.json` beside a JSONL transcript. A key names a *current* session
/// only (a reset re-points it), so the spawner is pinned by content: the session whose transcript
/// records this one's key as `childSessionKey` (the `sessions_spawn` result), trying the
/// spawner key's current session first.
#[derive(Default)]
struct JsonKeys {
    sessions: HashMap<String, (String, Option<String>)>,
    preferred: HashMap<String, String>,
}
#[cfg(test)]
thread_local! { static JSON_KEY_READS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)}; }
impl JsonKeys {
    fn read(path: &Path) -> Self {
        use super::store::document::Fields;
        #[cfg(test)]
        JSON_KEY_READS.with(|count| count.set(count.get() + 1));
        crate::json::JsonSource::capture(path)
            .and_then(|source| {
                source.read(ReadInput::Document, Validation::Value, |view| {
                    let mut index = Self::default();
                    view.for_each_entry(|key, entry| {
                        let Some(fields) = Fields::parse(entry, &["sessionId", "spawnedBy"])? else {
                            return Ok(());
                        };
                        if let Some(id) = fields.string("sessionId")? {
                            index.preferred.insert(key.to_owned(), id.clone());
                            index
                                .sessions
                                .entry(id)
                                .or_insert((key.to_owned(), fields.string("spawnedBy")?));
                        }
                        Ok(())
                    })?;
                    Ok(index)
                })
            })
            .unwrap_or_default()
    }
    fn retained_bytes(&self) -> usize {
        self.sessions.capacity() * std::mem::size_of::<(String, (String, Option<String>))>()
            + self.preferred.capacity() * std::mem::size_of::<(String, String)>()
            + self
                .sessions
                .iter()
                .map(|(id, (key, parent))| id.capacity() + key.capacity() + parent.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self
                .preferred
                .iter()
                .map(|(key, id)| key.capacity() + id.capacity())
                .sum::<usize>()
    }
}
#[cfg(test)]
fn jsonl_key_facts(path: &Path, id: &str, dependency: &mut dyn FnMut(&Path)) -> Result<KeyFacts> {
    jsonl_key_facts_with_reads(path, id, dependency, &mut crate::source::ReadSet::default())
}
fn jsonl_key_facts_with_reads(
    path: &Path,
    session_id: &str,
    dependency: &mut dyn FnMut(&Path),
    reads: &mut crate::source::ReadSet,
) -> Result<KeyFacts> {
    let Some(dir) = path.parent() else {
        return Ok(KeyFacts::default());
    };
    let index_path = dir.join("sessions.json");
    let mut baseline = crate::source::ReadSet::default();
    baseline.record(&index_path);
    dependency(&index_path);
    let (index, original) = crate::source::inventory("openclaw-json-keys", &index_path, &baseline, || {
        let index = JsonKeys::read(&index_path);
        let bytes = index.retained_bytes();
        Ok((index, baseline.clone(), bytes))
    })?;
    reads.merge(&original);
    for path in original.paths().filter(|path| *path != index_path) {
        dependency(path);
    }
    let Some((key, spawned_by)) = index.sessions.get(session_id) else {
        return Ok(KeyFacts::default());
    };
    let spawned_by = spawned_by.clone();
    let parent = spawned_by.as_ref().and_then(|by| {
        let first = index
            .preferred
            .get(by)
            .map(String::as_str)
            .map(|sid| dir.join(format!("{sid}.jsonl")));
        // The preferred current window wins regardless of directory membership. Avoid
        // enumerating/sorting every historical transcript when it already records this child.
        if let Some(first) = first.as_ref().filter(|p| p.as_path() != path) {
            reads.record(first);
            dependency(first);
            if names_child_file(first, key).unwrap_or(false) {
                return Some(session_id_from_filename(first));
            }
        }
        reads.record(dir);
        dependency(dir);
        let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.as_path() != path && Some(p) != first.as_ref())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_primary_transcript_name)
            })
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .find(|p| {
                reads.record(p);
                dependency(p);
                names_child_file(p, key).unwrap_or(false)
            })
            .map(|p| session_id_from_filename(&p))
    });
    Ok(KeyFacts {
        session_key: Some(key.clone()),
        spawned_by,
        parent,
    })
}

#[cfg(all(feature = "sqlite", test))]
thread_local! {
    static SQL_PARENT_ROWS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
    static SQL_PARENT_FULLSCANS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
}

#[cfg(feature = "sqlite")]
struct WindowGroups(super::store::sqlite::Selection);
#[cfg(feature = "sqlite")]
impl WindowGroups {
    fn load(conn: &rusqlite::Connection) -> Result<(Self, usize)> {
        use rusqlite::types::ValueRef;
        let selection = super::store::sqlite::Selection::build(
            "CREATE TABLE windows(ordinal INTEGER PRIMARY KEY,key BLOB,session_id);
             CREATE INDEX windows_by_key ON windows(key,ordinal);",
            |index| {
                let mut insert = index.prepare("INSERT INTO windows(key,session_id) VALUES (?1,?2)")?;
                // Only the original plain BINARY/TEXT scan uses this index. Preserve its
                // physical order and native ID types, including malformed UTF-8 text.
                let mut stmt = conn.prepare("SELECT session_key,session_id FROM session_windows NOT INDEXED")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let Ok(key) = row.get_ref(0)?.as_str() else { continue };
                    let value = row.get_ref(1)?;
                    if matches!(value, ValueRef::Null) {
                        continue;
                    }
                    insert.execute(rusqlite::params![
                        key.as_bytes(),
                        rusqlite::types::ToSqlOutput::Borrowed(value)
                    ])?;
                    #[cfg(test)]
                    SQL_PARENT_ROWS.with(|count| count.set(count.get() + 1));
                }
                drop(rows);
                #[cfg(test)]
                SQL_PARENT_FULLSCANS.with(|count| {
                    count.set(count.get() + stmt.get_status(rusqlite::StatementStatus::FullscanStep) as usize)
                });
                Ok(())
            },
        )?;
        Ok((Self(selection), super::store::sqlite::Selection::RETAINED_BYTES))
    }
    fn candidates(&self, key: &str, current: &str) -> Result<Vec<String>> {
        self.0.with(|index| {
            // No affinity on the copied value: comparing TEXT current preserves the original
            // TEXT-affinity table's stored values, and get(String) preserves selected errors.
            let mut stmt =
                index.prepare("SELECT session_id FROM windows WHERE key=?1 AND session_id!=?2 ORDER BY ordinal")?;
            let candidates = stmt
                .query_map(rusqlite::params![key.as_bytes(), current], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(candidates)
        })
    }
}
#[cfg(feature = "sqlite")]
fn plain_parent_scan(conn: &rusqlite::Connection, key: &str, current: &str) -> Result<bool> {
    let plain = conn
        .prepare("EXPLAIN QUERY PLAN SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?
        .query_map([key, current], |row| row.get::<_, String>(3))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|detail| detail == "SCAN session_windows");
    if !plain {
        return Ok(false);
    }
    super::store::sqlite::binary_text_columns(conn, "session_windows", &["session_key", "session_id"])
}

#[cfg(feature = "sqlite")]
fn sqlite_parent_candidates(
    conn: &rusqlite::Connection,
    by: &str,
    session_id: &str,
    path: &Path,
    reads: &crate::source::ReadSet,
) -> Result<std::sync::Arc<Vec<String>>> {
    let load = || -> Result<Vec<String>> {
        let mut stmt =
            conn.prepare("SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?;
        let candidates = stmt
            .query_map([by, session_id], |row| {
                #[cfg(test)]
                SQL_PARENT_ROWS.with(|count| count.set(count.get() + 1));
                row.get(0)
            })?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        #[cfg(test)]
        SQL_PARENT_FULLSCANS
            .with(|count| count.set(count.get() + stmt.get_status(rusqlite::StatementStatus::FullscanStep) as usize));
        Ok(candidates)
    };
    // Query plans can vary by parameter with SQLite statistics. Check the actual key's
    // plan rather than applying one plain scan's traversal order to an indexed key.
    let candidates = {
        let mut encoded_plan_key = String::with_capacity(by.len() * 2);
        use std::fmt::Write as _;
        for byte in by.as_bytes() {
            write!(&mut encoded_plan_key, "{byte:02x}")?;
        }
        encoded_plan_key.push(':');
        for byte in session_id.as_bytes() {
            write!(&mut encoded_plan_key, "{byte:02x}")?;
        }
        let (plain, _) =
            crate::source::inventory("openclaw-parent-plan", &path.join(&encoded_plan_key), reads, || {
                Ok((
                    plain_parent_scan(conn, by, session_id)?,
                    reads.clone(),
                    std::mem::size_of::<bool>(),
                ))
            })?;
        // The operation already memoizes complete query results. Only a second cache
        // miss needs a whole-store index; a single-window scope keeps native startup cost.
        crate::source::inventory("openclaw-parent-windows", &path.join(&encoded_plan_key), reads, || {
            let candidates = if *plain && crate::source::inventory_scoped() {
                let (queried, _) = crate::source::inventory("openclaw-window-queried", path, reads, || {
                    Ok((
                        std::sync::atomic::AtomicBool::new(false),
                        reads.clone(),
                        std::mem::size_of::<std::sync::atomic::AtomicBool>(),
                    ))
                })?;
                if queried.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let (groups, _) = crate::source::inventory("openclaw-window-groups", path, reads, || {
                        let (groups, bytes) = WindowGroups::load(conn)?;
                        Ok((groups, reads.clone(), bytes))
                    })?;
                    groups.candidates(by, session_id)?
                } else {
                    load()?
                }
            } else {
                // Both SQL parameters determine membership under custom collations.
                load()?
            };
            let bytes = candidates.capacity() * std::mem::size_of::<String>()
                + candidates.iter().map(String::capacity).sum::<usize>();
            Ok((candidates, reads.clone(), bytes))
        })?
        .0
    };
    Ok(candidates)
}

/// Every window's [`Rows`](crate::source::Rows) certificate: what a read of it consults in its
/// store besides event bodies. Per `transcript_events` row, in rowid order, its rowid, `seq` (the
/// read's order) and the byte lengths of `event_json` and `event_zstd` (record headers, no body);
/// its whole `session_windows` row (key, `spawned_by`); and its spawner lookup
/// ([`sqlite_key_facts`]): the other windows under its `spawned_by` key, in the order the read
/// meets them, and, when there are several (the read then searches their events for this
/// window's key), each one's event certificate. The state database's registries are a separate
/// file dependency, outside this certificate.
#[cfg(feature = "sqlite")]
fn sqlite_rows(conn: &rusqlite::Connection) -> Result<std::collections::HashMap<String, crate::source::Rows>> {
    let _snapshot = super::store::sqlite::snapshot(conn)?;
    let mut events = super::store::sqlite::Certificates::default();
    let zstd = if super::store::sqlite::columns(conn, Events::TABLE).contains("event_zstd") {
        "octet_length(event_zstd)"
    } else {
        "NULL"
    };
    events.rows(
        conn,
        &format!(
            "SELECT session_id, rowid, seq, octet_length(event_json), {zstd} FROM transcript_events \
             ORDER BY session_id, rowid"
        ),
    )?;
    let mut certificates = super::store::sqlite::Certificates(events.0.clone());
    certificates.digest(conn, "SELECT session_id, * FROM session_windows ORDER BY session_id")?;
    let cols = super::store::sqlite::columns(conn, "session_windows");
    if cols.contains("session_key") && cols.contains("spawned_by") {
        let spawned: Vec<(String, String)> = conn
            .prepare("SELECT session_id, spawned_by FROM session_windows WHERE spawned_by IS NOT NULL")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // Each spawner key's windows, once: a query per child scanned every window (children x
        // windows). Exact under plain text comparison; other collations keep the query.
        let grouped =
            super::store::sqlite::binary_text_columns(conn, "session_windows", &["session_key", "session_id"])?;
        let mut windows: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        if grouped {
            let mut stmt = conn.prepare("SELECT session_key, session_id FROM session_windows")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let (Ok(key), Ok(id)) = (row.get_ref(0)?.as_str(), row.get_ref(1)?.as_str()) else {
                    continue;
                };
                windows.entry(key.to_owned()).or_default().push(id.to_owned());
            }
        }
        let mut candidates =
            conn.prepare("SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?;
        for (id, by) in spawned {
            let found: Vec<String> = if grouped {
                windows
                    .get(&by)
                    .into_iter()
                    .flatten()
                    .filter(|window| **window != id)
                    .cloned()
                    .collect()
            } else {
                candidates
                    .query_map([&by, &id], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for candidate in &found {
                certificates.fold(&id, candidate.as_bytes());
                if found.len() > 1 {
                    certificates.fold_from(&id, events.0.get(candidate).copied());
                }
            }
        }
    }
    Ok(certificates.0)
}

/// [`KeyFacts`] from `session_windows`. The spawner is the window under `spawned_by` — when that key
/// has several windows (resets, rollovers), the one whose transcript records this session's key as `childSessionKey`.
#[cfg(feature = "sqlite")]
fn sqlite_key_facts(
    conn: &rusqlite::Connection,
    r: &SessionRef,
    reads: &crate::source::ReadSet,
    events: &Events,
) -> Result<KeyFacts> {
    let (session_id, path) = (r.id.as_str(), r.path.as_path());
    let cols = super::store::sqlite::columns(conn, "session_windows");
    if !cols.contains("session_key") {
        return Ok(KeyFacts::default());
    }
    let spawned_col = if cols.contains("spawned_by") {
        "spawned_by"
    } else {
        "NULL"
    };
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            &format!("SELECT session_key, {spawned_col} FROM session_windows WHERE session_id = ?1"),
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let Some((key, spawned_by)) = row else {
        return Ok(KeyFacts::default());
    };
    let mut parent = None;
    if let Some(by) = &spawned_by {
        let candidates = sqlite_parent_candidates(conn, by, session_id, path, reads)?;
        parent = if candidates.len() == 1 {
            candidates.first().cloned()
        } else {
            let mut found = None;
            for c in candidates.iter() {
                // Keep reading after finding the child so a later SQL error still fails the
                // lookup, as it did when this collected the candidate's complete transcript.
                let mut names = false;
                events.each(conn, c, |cell| {
                    if let (false, Some(cell)) = (names, cell) {
                        names = events
                            .cells
                            .read(cell, |view| {
                                // The record's compact serialization, as a parsed event prints.
                                let mut marker = ChildMarker::new(&key);
                                view.write_canonical(&mut marker)?;
                                Ok(marker.found)
                            })?
                            .unwrap_or(false);
                    }
                    Ok(Flow::Continue)
                })?;
                if names {
                    found = Some(c.clone());
                    break;
                }
            }
            found
        };
    }
    Ok(KeyFacts {
        session_key: Some(key),
        spawned_by,
        parent,
    })
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

/// Is `path` a sqlite store? The check is the extension only: every `path` this adapter asks
/// about is one discovery assigned, and its sqlite stores are all `openclaw-agent*.sqlite`.
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

// ---------------------------------------------------------------------------
// SQLite store (OpenClaw ≥ 2026-07-11)
// ---------------------------------------------------------------------------

/// One [`SessionRef`] per `session_windows` row (one transcript each; a reset/rollover/fork opens a
/// new window under the same `session_key`). Title precedence: the node's `label`, then either
/// `display_name`, then the entry's `label`/`subject`, then a `session_info` name, then the first
/// user prompt. `path` is the store file; `id` is the window's `session_id` — `stream` reopens the
/// store and selects the rows by id.
#[cfg(feature = "sqlite")]
fn discover_sqlite(db: &Path) -> Result<Vec<SessionRef>> {
    #[cfg(test)]
    SQLITE_DISCOVERED.lock().unwrap().push(db.to_path_buf());
    let conn = open_ro(db)?;
    conn.read(|conn, reads| {
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
        let rows = windows.query_map([], |row| -> rusqlite::Result<WindowRow> {
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
        })?;
        let events = Events::new(conn, db, reads)?;
        let mut out = Vec::new();
        for row in rows {
            let (session_id, created, updated, transcript_updated, w_display, n_label, n_display, entry_json) = row?;
            let mut acc = ScanAcc::default();
            events.each(conn, &session_id, |cell| {
                if let Some(cell) = cell {
                    events.cells.read(cell, |view| discovery::feed(view, &mut acc))?;
                }
                Ok(Flow::Continue)
            })?;
            // `entry_json` is the old `sessions.json` entry, read by the same entry decoder.
            let entry = entry_json
                .as_deref()
                .and_then(|e| serde_json::from_str::<Value>(e).ok());
            let entry = entry
                .as_ref()
                .map(|e| discovery::entry(crate::json::JsonView::from(e)))
                .transpose()?
                .unwrap_or_default();
            let title = n_label
                .or(n_display)
                .or(w_display)
                .filter(|t| !t.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, crate::projection::Head::TITLE))
                .or(entry.title)
                .or(acc.session_name)
                .or(acc.first_user);
            let cwd = acc.cwd.or(entry.cwd);
            out.push(SessionRef {
                cwd,
                title,
                created_at: acc.created_at.or_else(|| created.and_then(ts_from_epoch)),
                updated_at: transcript_updated
                    .and_then(ts_from_epoch)
                    .or_else(|| updated.and_then(ts_from_epoch))
                    .or(acc.updated_at),
                message_count: Some(acc.message_count),
                ..SessionRef::new(session_id, Harness::OpenClaw, db.to_path_buf())
            });
        }
        Ok(out)
    })
}

/// One window's `transcript_events` cells in `seq` order, each copied once into one [`Cells`]
/// arena. Since OpenClaw added payload compression (`transcript-payload.ts`, after `0e9181234a`),
/// an event of at least 1 KiB is stored as a zstd frame of its UTF-8 JSON in `event_zstd` with
/// `event_json` NULL (plus a `navigation_json` projection cv does not need); smaller ones stay in
/// `event_json`. A row that decodes to nothing is counted (`extra["cv"]["skipped_lines"]`), never
/// silently dropped.
#[cfg(feature = "sqlite")]
struct Events {
    cells: super::store::sqlite::Cells,
    sql: String,
}

#[cfg(feature = "sqlite")]
impl Events {
    const TABLE: &'static str = "transcript_events";

    fn new(conn: &rusqlite::Connection, path: &Path, reads: &crate::source::ReadSet) -> Result<Self> {
        let cells = super::store::sqlite::Cells::new(conn, path, reads, Self::TABLE)?;
        let compressed = if super::store::sqlite::columns(conn, Self::TABLE).contains("event_zstd") {
            cells.select("event_zstd")
        } else {
            "NULL, NULL".into()
        };
        let sql = format!(
            "SELECT {}, {compressed} FROM transcript_events WHERE session_id = ?1 ORDER BY seq ASC",
            cells.select("event_json")
        );
        Ok(Events { cells, sql })
    }

    /// Visit each row's event cell: a TEXT `event_json`, else a valid `event_zstd` frame decoded,
    /// else None.
    fn each(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
        mut visit: impl FnMut(Option<super::store::sqlite::Cell>) -> Result<Flow>,
    ) -> Result<()> {
        let mut statement = conn.prepare_cached(&self.sql)?;
        let mut rows = statement.query([session_id])?;
        while let Some(row) = rows.next()? {
            let cell = match self.cells.cell(conn, Self::TABLE, "event_json", row, 0)? {
                Some(cell) => Some(cell),
                None => {
                    let mut invalid = false;
                    let cell = self.cells.blob(conn, Self::TABLE, "event_zstd", row, 2, |input, out| {
                        unzstd(input, out, &mut invalid)
                    })?;
                    cell.filter(|_| !invalid)
                }
            };
            if visit(cell)? == Flow::Stop {
                break;
            }
        }
        Ok(())
    }
}

/// One zstd frame from `input` to `out`. A codec rejection sets `invalid` (the row is skipped); a
/// failure to read the cell or write the arena is an error.
#[cfg(feature = "sqlite")]
fn unzstd(input: &mut dyn std::io::BufRead, out: &mut dyn std::io::Write, invalid: &mut bool) -> Result<()> {
    use std::{cell::RefCell, io::Read};
    struct Checked<'a, 'b> {
        inner: &'a mut dyn std::io::BufRead,
        failure: &'b RefCell<Option<std::io::Error>>,
    }
    impl Read for Checked<'_, '_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(bytes).inspect_err(|error| {
                *self.failure.borrow_mut() = Some(std::io::Error::new(error.kind(), error.to_string()));
            })
        }
    }
    let failure = RefCell::<Option<std::io::Error>>::new(None);
    let source_failed = || -> Result<()> {
        match failure.borrow_mut().take() {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    };
    let Ok(mut decoder) = super::store::zst::decoder(Checked {
        inner: input,
        failure: &failure,
    }) else {
        *invalid = true;
        return source_failed();
    };
    let mut chunk = [0; 64 * 1024];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => out.write_all(&chunk[..n])?,
            Err(_) => {
                *invalid = true;
                return source_failed();
            }
        }
    }
}

#[cfg(not(feature = "sqlite"))]
fn discover_sqlite(_db: &Path) -> Result<Vec<SessionRef>> {
    Ok(Vec::new())
}

/// [`discover_sqlite`] under the scan cache, keyed on the store and its WAL and journal, as every
/// other SQLite adapter's discovery is: uncached, each listing refresh copied a WAL store and
/// decompressed and decoded every window's `transcript_events`.
#[cfg(feature = "sqlite")]
fn discover_store(db: &Path) -> Result<Vec<SessionRef>> {
    super::store::sqlite::cached_discover(db, || discover_sqlite(db))
}

#[cfg(not(feature = "sqlite"))]
fn discover_store(db: &Path) -> Result<Vec<SessionRef>> {
    discover_sqlite(db)
}

#[cfg(all(test, feature = "sqlite"))]
static SQLITE_DISCOVERED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Replay one window's `transcript_events` rows (`ORDER BY seq`) through the record codec the
/// JSONL path uses — `event_json` IS the JSONL line. Branch selection reads the navigation fields
/// of every cell, then decodes only the selected ones.
#[cfg(feature = "sqlite")]
fn stream_sqlite(r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
    super::store::sqlite::read(&r.path, |conn, reads| {
        // Navigation and payload reads must see the same history: a concurrent leaf update cannot
        // change which rows the second pass emits. A deferred read transaction writes no store data.
        let _snapshot = super::store::sqlite::snapshot(conn)?;
        let events = Events::new(conn, &r.path, reads)?;
        let facts = sqlite_key_facts(conn, r, reads, &events)?;
        // The whole transcript's diagnostics, even if the sink stops at its first turn.
        let mut cells = Vec::new();
        let mut skipped = 0;
        let mut tree = false;
        events.each(conn, &r.id, |cell| {
            match cell
                .map(|cell| events.cells.read(cell, is_leaf_entry))
                .transpose()?
                .flatten()
            {
                Some(leaf) => {
                    cells.push(cell.expect("read cell"));
                    tree |= leaf;
                }
                None => skipped += 1,
            }
            Ok(Flow::Continue)
        })?;
        let mut fold = Fold::new(r, &facts, opts);
        let mut emit = |index: usize, active: bool, parent: Option<Option<String>>| -> Result<Flow> {
            Ok(
                match events.cells.read(cells[index], |view| record::record(view, opts))? {
                    Some(record) => fold.push(record, active, parent, sink)?,
                    None => Flow::Continue,
                },
            )
        };
        if tree {
            let mut descriptors = Vec::with_capacity(cells.len());
            for cell in &cells {
                descriptors.push(events.cells.read(*cell, record::navigation)?.unwrap_or(Value::Null));
            }
            for row in select_navigation(&descriptors, opts.tier == Tier::Complete) {
                if emit(row.index, row.active, row.parent)? == Flow::Stop {
                    break;
                }
            }
        } else {
            for index in 0..cells.len() {
                if emit(index, true, None)? == Flow::Stop {
                    break;
                }
            }
        }
        let mut s = fold.finish(sink);
        super::Unparsed(skipped).note(&mut s);
        Ok(s)
    })
}

#[cfg(not(feature = "sqlite"))]
fn stream_sqlite(r: &SessionRef, _opts: &Demand, _sink: &mut dyn MessageSink) -> Result<Session> {
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

/// Leaf-control chains are shared by many side entries. Resolve each stable suffix once.
/// A repeated ID or newly filled missing target can change an earlier walk's meaning, so
/// those malformed/forward-reference updates invalidate this operation-local memoization.
#[derive(Default)]
struct CanonicalParents {
    resolved: HashMap<String, Option<String>>,
    missing: HashSet<String>,
    #[cfg(test)]
    visits: usize,
}
impl CanonicalParents {
    fn insert(&mut self, id: String, index: usize, by_id: &mut HashMap<String, usize>) {
        if by_id.insert(id.clone(), index).is_some() || self.missing.contains(&id) {
            self.resolved.clear();
            self.missing.clear();
        }
    }
    fn resolve(
        &mut self,
        parent: Option<String>,
        nodes: &[TreeNode],
        by_id: &HashMap<String, usize>,
    ) -> Option<String> {
        let mut path = Vec::<String>::new();
        let mut seen = HashMap::<String, usize>::new();
        let mut current = parent;
        let result = loop {
            let Some(cur) = current else {
                break None;
            };
            if let Some(result) = self.resolved.get(&cur) {
                break result.clone();
            }
            if let Some(&start) = seen.get(&cur) {
                // Starting on a cycle returns that starting ID. Prefixes return the first
                // cycle entry they encounter; each cached cyclic node therefore keeps itself.
                for id in path.iter().skip(start) {
                    self.resolved.insert(id.clone(), Some(id.clone()));
                }
                path.truncate(start);
                break Some(cur);
            }
            #[cfg(test)]
            {
                self.visits += 1;
            }
            let Some(&index) = by_id.get(&cur) else {
                self.missing.insert(cur.clone());
                break Some(cur);
            };
            if !nodes[index].is_leaf_control {
                break Some(cur);
            }
            seen.insert(cur.clone(), path.len());
            path.push(cur);
            current = nodes[index].entry.parent_id.clone();
        };
        for id in path {
            self.resolved.insert(id, result.clone());
        }
        result
    }
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
fn select_navigation(entries: &[Value], complete: bool) -> Vec<NavigationRow> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut parents = CanonicalParents::default();
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
            parents.insert(node.entry.id.clone(), nodes.len(), &mut by_id);
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
            te.parent_id = parents.resolve(logical, &nodes, &by_id);
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
        parents.insert(id, nodes.len(), &mut by_id);
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

    if complete {
        entries
            .iter()
            .enumerate()
            .map(|(index, v)| NavigationRow {
                index,
                parent: normalized.get(&index).cloned(),
                active: entry_type(v) == "session" || active_set.contains(&index),
            })
            .collect()
    } else {
        let mut seen = HashSet::new();
        let headers = entries
            .iter()
            .enumerate()
            .filter(|(_, v)| entry_type(v) == "session")
            .map(|(i, _)| i);
        headers
            .chain(active_order)
            .filter(|index| seen.insert(*index))
            .map(|index| NavigationRow {
                index,
                parent: normalized.get(&index).cloned(),
                active: true,
            })
            .collect()
    }
}

struct NavigationRow {
    index: usize,
    active: bool,
    parent: Option<Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    /// The 2026.10.1 corpus agent store: an event appended to the window that spawned the
    /// sub-agent moves its certificate alone (the sub-agent's spawner lookup names one candidate,
    /// so it consults that window's id, not its events), and every other window reads exactly as
    /// before.
    #[cfg(feature = "sqlite")]
    #[test]
    fn corpus_rows_certify_each_session_and_move_only_with_its_rows() {
        let world = crate::test_world("openclaw-corpus-rows");
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/openclaw/2026.10.1");
        for entry in WalkDir::new(&corpus) {
            let entry = entry.unwrap();
            let to = world.base.join(entry.path().strip_prefix(&corpus).unwrap());
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&to).unwrap();
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
        let db = world.base.join("agents/main/agent/openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let leaf: String = conn
            .query_row(
                "SELECT p.session_id FROM session_windows c JOIN session_windows p ON p.session_key = c.spawned_by \
                 ORDER BY p.session_id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let adapter = OpenClaw::with_state_dir(&world.base);
        crate::harness::store::sqlite::assert_rows_move_alone(&adapter, &db, &leaf, |c| {
            c.execute(
                "INSERT INTO transcript_events (session_id, seq, event_json, created_at) \
                 SELECT ?1, max(seq) + 1, '{\"type\":\"custom\",\"id\":\"zq\",\"customType\":\"zqcertified\"}', \
                 4102444800000 FROM transcript_events WHERE session_id = ?1",
                [&leaf],
            )
            .unwrap();
        });
    }

    fn fixture_ref(name: &str) -> SessionRef {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/openclaw")
            .join(name);
        SessionRef::new("test-session", Harness::OpenClaw, path)
    }

    /// A fixture's text, without its final newline (the text the old inline `[..].join("\n")` built).
    fn fixture_text(name: &str) -> String {
        let r = fixture_ref(name);
        let text = fs::read_to_string(&r.path).unwrap_or_else(|e| panic!("reading {}: {e}", r.path.display()));
        text.strip_suffix('\n').map(str::to_string).unwrap_or(text)
    }

    fn parse_fixture(name: &str) -> Session {
        let r = fixture_ref(name);
        let text = fs::read_to_string(&r.path).unwrap_or_else(|e| panic!("reading {}: {e}", r.path.display()));
        parse_text(&text, &r)
    }

    /// `text` read as a transcript file through the adapter, as `r` (full fidelity, collected).
    fn parse_text(text: &str, r: &SessionRef) -> Session {
        parse_text_with(text, r, &Demand::RICH)
    }

    fn parse_text_with(text: &str, r: &SessionRef, opts: &Demand) -> Session {
        let dir = std::env::temp_dir().join(format!("cv-openclaw-text-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let mut file_ref = r.clone();
        file_ref.path = dir.join("transcript.jsonl");
        fs::write(&file_ref.path, text).unwrap();
        let mut s = collect(&file_ref, opts).unwrap();
        s.materialize().unwrap();
        s.source_path = Some(r.path.clone());
        fs::remove_dir_all(&dir).unwrap();
        s
    }

    /// A v4 transcript with a rewind: `a2` is a sibling answer, the `leaf` control selects `a1`,
    /// the session is then named, compacted, and continued. Visible branch: u1 a1 u2 (si) c1 a3.
    fn branchy_events() -> Vec<String> {
        fixture_text("branchy-v4.jsonl")
            .split('\n')
            .map(str::to_string)
            .collect()
    }

    #[cfg(feature = "sqlite")]
    fn uncached_parent(parent: Option<String>, nodes: &[TreeNode], by_id: &HashMap<String, usize>) -> Option<String> {
        let mut seen = HashSet::new();
        let mut current = parent;
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                return Some(id);
            }
            let Some(&index) = by_id.get(&id) else {
                return Some(id);
            };
            if !nodes[index].is_leaf_control {
                return Some(id);
            }
            current = nodes[index].entry.parent_id.clone();
        }
        None
    }
    #[cfg(feature = "sqlite")]
    fn parent_node(id: &str, parent: Option<&str>, control: bool) -> TreeNode {
        TreeNode {
            entry: TreeEntry {
                id: id.into(),
                parent_id: parent.map(str::to_owned),
                leaf_id: None,
                append_parent_id: None,
                side: false,
            },
            is_leaf_control: control,
            index: 0,
        }
    }
    #[cfg(feature = "sqlite")]
    #[test]
    fn canonical_parent_shared_chains_have_linear_visits_including_missing_targets_and_cycles() {
        for terminal in [Some("root"), Some("absent"), None, Some("leaf-0")] {
            let mut nodes = vec![parent_node("root", None, false)];
            let mut by_id = HashMap::from([("root".to_owned(), 0)]);
            let mut resolver = CanonicalParents::default();
            for i in 0..4000 {
                let id = format!("leaf-{i}");
                let parent = if i == 0 {
                    terminal.map(str::to_owned)
                } else {
                    Some(format!("leaf-{}", i - 1))
                };
                let node = parent_node(&id, parent.as_deref(), true);
                resolver.insert(id, nodes.len(), &mut by_id);
                nodes.push(node);
            }
            for i in (0..4000).rev() {
                let parent = Some(format!("leaf-{i}"));
                assert_eq!(
                    resolver.resolve(parent.clone(), &nodes, &by_id),
                    uncached_parent(parent, &nodes, &by_id)
                );
            }
            assert!(
                resolver.visits <= 4001,
                "shared suffixes must be walked once: {}",
                resolver.visits
            );
        }
    }
    #[cfg(feature = "sqlite")]
    #[test]
    fn canonical_parent_cache_preserves_forward_targets_duplicate_ids_and_cycle_entry_points() {
        let mut nodes = vec![
            parent_node("a", Some("b"), true),
            parent_node("b", Some("c"), true),
            parent_node("c", Some("b"), true),
        ];
        let mut by_id = HashMap::from([("a".to_owned(), 0), ("b".to_owned(), 1), ("c".to_owned(), 2)]);
        let mut resolver = CanonicalParents::default();
        for id in ["a", "b", "c", "missing"] {
            assert_eq!(
                resolver.resolve(Some(id.into()), &nodes, &by_id),
                uncached_parent(Some(id.into()), &nodes, &by_id)
            );
        }
        for node in [
            parent_node("b", Some("missing"), true),
            parent_node("missing", Some("c"), true),
            parent_node("c", None, false),
        ] {
            resolver.insert(node.entry.id.clone(), nodes.len(), &mut by_id);
            nodes.push(node);
            for id in ["a", "b", "c", "missing"] {
                assert_eq!(
                    resolver.resolve(Some(id.into()), &nodes, &by_id),
                    uncached_parent(Some(id.into()), &nodes, &by_id)
                );
            }
        }
    }

    #[cfg(feature = "sqlite")]
    fn write_store(agents: &Path, session_id: &str, events: &[String]) -> PathBuf {
        let agent_dir = agents.join("main").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let db = agent_dir.join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        // The three transcript tables of OpenClaw's `src/state/openclaw-agent-schema.sql` (column
        // names verbatim; cross-table foreign keys to `conversations` dropped).
        conn.execute_batch(&fixture_text("transcript-tables.sql")).unwrap();
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

    fn collect(r: &SessionRef, opts: &Demand) -> Result<Session> {
        let mut sink = crate::stream::CollectSink::default();
        let mut session = OpenClaw { roots: vec![] }.stream(r, opts, &mut sink)?;
        session.messages = sink.messages;
        Ok(session)
    }

    /// An unchanged store is not decoded again at discovery; a write to it is.
    #[cfg(feature = "sqlite")]
    #[test]
    fn unchanged_sqlite_store_is_not_rediscovered() {
        let _world = crate::test_world("openclaw-sqlite-discovery-cache");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let agents = tmp_agents();
        let events: Vec<String> = fixture_text("v3-full.jsonl").lines().map(str::to_string).collect();
        let db = write_store(&agents, "cached", &events);
        let scans = || {
            let mut all = SQLITE_DISCOVERED.lock().unwrap();
            let mine = all.iter().filter(|p| **p == db).count();
            all.retain(|p| *p != db);
            mine
        };
        let adapter = OpenClaw {
            roots: vec![agents.clone()],
        };
        let first = adapter.discover().unwrap();
        assert_eq!(scans(), 1);
        let again = adapter.discover().unwrap();
        assert_eq!(scans(), 0, "an unchanged store is not decoded again");
        assert_eq!(
            serde_json::to_value(&first.refs).unwrap(),
            serde_json::to_value(&again.refs).unwrap()
        );
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("UPDATE session_nodes SET updated_at = updated_at + 1", [])
            .unwrap();
        adapter.discover().unwrap();
        assert_eq!(scans(), 1, "a write is read");
        let _ = fs::remove_dir_all(agents.parent().unwrap());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_rows_read_like_the_jsonl_lines_they_hold_and_count_late_corruption_on_stop() {
        let agents = tmp_agents();
        let mut events: Vec<String> = fixture_text("v3-full.jsonl").lines().map(str::to_string).collect();
        events.extend(["unreadable".into(), "{\"ignored\":1e1000}".into()]);
        let db = write_store(&agents, "flat", &events);
        let r = discover_sqlite(&db).unwrap().remove(0);
        let jsonl = agents.join("flat.jsonl");
        fs::write(&jsonl, events.join("\n")).unwrap();
        let mut line_ref = r.clone();
        line_ref.path = jsonl;
        for opts in [Demand::RICH, Demand::COMPLETE] {
            let rows = collect(&r, &opts).unwrap();
            let lines = collect(&line_ref, &opts).unwrap();
            assert_eq!(rows.extra["cv"]["skipped_lines"], 2);
            assert_eq!(
                serde_json::to_value(&rows.messages).unwrap(),
                serde_json::to_value(&lines.messages).unwrap()
            );
        }
        struct StopAfterOne(usize);
        impl MessageSink for StopAfterOne {
            fn message(&mut self, _: Message) -> Flow {
                self.0 += 1;
                Flow::Stop
            }
        }
        let mut sink = StopAfterOne(0);
        let s = stream_sqlite(&r, &Demand::LEAN.with_spans(), &mut sink).unwrap();
        assert_eq!(sink.0, 1);
        assert_eq!(s.extra["cv"]["skipped_lines"], 2);
        fs::remove_dir_all(agents.parent().unwrap()).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_cells_keep_long_text_in_the_store_across_branches_and_retain_wal_guards() {
        for branch in [false, true] {
            let agents = tmp_agents();
            let mut events: Vec<String> = fixture_text(if branch { "branchy-v4.jsonl" } else { "v3-full.jsonl" })
                .lines()
                .map(str::to_string)
                .collect();
            for event in &mut events {
                let mut value: Value = serde_json::from_str(event).unwrap();
                if matches!(value["id"].as_str(), Some("u1" | "a2" | "m1" | "m4")) {
                    value["message"]["content"] =
                        serde_json::json!([{"type":"text","text":"selected λ\n".repeat(131072)}]);
                    *event = serde_json::to_string(&value).unwrap();
                }
            }
            events.extend([
                "malformed".into(),
                "{\"ignored\":1e999}".into(),
                "{\"ignored\":\"\\ud800\"}".into(),
                "{\"type\":\"future\",\"payload\":{\"unknown\":[true,42]}}".into(),
            ]);
            let db = write_store(&agents, "checked", &events);
            let writer = rusqlite::Connection::open(&db).unwrap();
            writer.execute_batch("ALTER TABLE transcript_events RENAME TO old_events;
                CREATE TABLE transcript_events (session_id TEXT NOT NULL,seq INTEGER NOT NULL,event_json TEXT,event_zstd BLOB,created_at INTEGER NOT NULL,PRIMARY KEY(session_id,seq)) STRICT;
                INSERT INTO transcript_events (session_id,seq,event_json,created_at) SELECT session_id,seq,event_json,created_at FROM old_events;
                DROP TABLE old_events; PRAGMA journal_mode=WAL;").unwrap();
            // One giant inactive branch is compressed, while the giant user cell stays TEXT.
            for (seq, event) in events.iter().enumerate() {
                if serde_json::from_str::<Value>(event)
                    .ok()
                    .is_some_and(|v| matches!(v["id"].as_str(), Some("a2" | "m4")))
                {
                    let compressed = zstd::encode_all(event.as_bytes(), 1).unwrap();
                    writer.execute("UPDATE transcript_events SET event_json=NULL,event_zstd=?1 WHERE session_id='checked' AND seq=?2", rusqlite::params![compressed, seq as i64]).unwrap();
                }
            }
            writer.execute("INSERT INTO transcript_events (session_id,seq,event_json,event_zstd,created_at) VALUES ('checked',999,NULL,?1,1)", [b"invalid compressed frame".as_slice()]).unwrap();
            let r = discover_sqlite(&db).unwrap().remove(0);
            for complete in [false, true] {
                let options = if complete { Demand::COMPLETE } else { Demand::RICH };
                let inline = collect(&r, &options).unwrap();
                let mut spans = collect(&r, &Demand { spans: true, ..options }).unwrap();
                assert_eq!(inline.extra["cv"]["skipped_lines"], 4);
                assert!(spans.messages.iter().any(|m| m.content.iter().any(|b| matches!(
                    b,
                    Block::Text {
                        text: crate::Text::Source(_)
                    }
                ))));
                spans.materialize().unwrap();
                assert_eq!(
                    serde_json::to_value(inline).unwrap(),
                    serde_json::to_value(spans).unwrap(),
                    "branch={branch}/complete={complete}"
                );
            }
            let mut retained = collect(
                &r,
                &Demand {
                    spans: true,
                    ..Demand::COMPLETE
                },
            )
            .unwrap();
            writer
                .execute(
                    "UPDATE transcript_events SET event_json='{}' WHERE session_id='checked' AND seq=1",
                    [],
                )
                .unwrap();
            assert!(
                retained.materialize().is_err(),
                "retained DB/WAL guard must survive connection release"
            );
            drop(writer);
            fs::remove_dir_all(agents.parent().unwrap()).unwrap();
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn a_without_rowid_store_reads_its_cells_from_the_row() {
        let agents = tmp_agents();
        let mut events = branchy_events();
        events[1] = serde_json::json!({"type":"message","id":"u1","parentId":null,
            "message":{"role":"user","content":"long λ\n".repeat(4096)}})
        .to_string();
        let db = write_store(&agents, "compat", &events);
        let expected = collect(&discover_sqlite(&db).unwrap().remove(0), &Demand::COMPLETE).unwrap();
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("ALTER TABLE transcript_events RENAME TO old_events;
            CREATE TABLE transcript_events (session_id TEXT NOT NULL,seq INTEGER NOT NULL,event_json TEXT,created_at INTEGER NOT NULL,PRIMARY KEY(session_id,seq)) WITHOUT ROWID;
            INSERT INTO transcript_events SELECT session_id,seq,event_json,created_at FROM old_events;
            DROP TABLE old_events;").unwrap();
        drop(writer);
        let r = discover_sqlite(&db).unwrap().remove(0);
        let mut actual = collect(
            &r,
            &Demand {
                spans: true,
                ..Demand::COMPLETE
            },
        )
        .unwrap();
        assert!(actual.messages.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            Block::Text {
                text: crate::Text::Source(_)
            }
        ))));
        actual.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(expected).unwrap(),
            serde_json::to_value(actual).unwrap()
        );
        fs::remove_dir_all(agents.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_source_replaced_during_the_read_fails_it_flat_or_tree() -> Result<()> {
        let world = crate::test_world("openclaw-owned-tree-identity");
        let path = world.base.join("source.jsonl");
        for tree in [false, true] {
            let mut bytes =
                b"{\"type\":\"message\",\"id\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"original\"}}\n"
                    .to_vec();
            if tree {
                bytes.extend_from_slice(b"{\"type\":\"leaf\",\"id\":\"l\",\"parentId\":\"u\",\"targetId\":\"u\"}\n");
            }
            for spans in [false, true] {
                fs::write(&path, &bytes)?;
                // Use a fallible callback so its diagnostic cannot hide the primary-source race.
                struct MutateFallible<'a>(&'a Path);
                impl MessageSink for MutateFallible<'_> {
                    fn message(&mut self, _: Message) -> Flow {
                        unreachable!()
                    }
                    fn try_message(&mut self, _: Message) -> Result<Flow> {
                        fs::write(self.0, b"replacement")?;
                        anyhow::bail!("callback diagnostic")
                    }
                }
                let r = SessionRef::new("identity", Harness::OpenClaw, &path);
                let options = Demand {
                    spans,
                    ..Demand::COMPLETE
                };
                let error = OpenClaw { roots: vec![] }
                    .stream(&r, &options, &mut MutateFallible(&path))
                    .unwrap_err();
                assert!(
                    format!("{error:#}").contains("source changed"),
                    "tree={tree}/spans={spans}: {error:#}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn formatted_leaf_controls_select_the_same_visible_branch() -> Result<()> {
        let world = crate::test_world("openclaw-formatted-leaf");
        let path = world.base.join("tree.jsonl");
        let r = SessionRef::new("formatted", Harness::OpenClaw, &path);
        for leaf in [
            r#"{"type":"leaf","id":"l1","parentId":"a2","targetId":"a1"}"#,
            r#"{ "type" : "leaf", "id" : "l1", "parentId" : "a2", "targetId" : "a1" }"#,
            r#"{"\u0074ype":"l\u0065af","id":"l1","parentId":"a2","targetId":"a1"}"#,
            r#"{"type":"future","type":"leaf","id":"l1","parentId":"a2","targetId":"a1"}"#,
        ] {
            let text = branchy_events()
                .into_iter()
                .map(|line| {
                    if line.contains("\"leaf\"") {
                        leaf.to_string()
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&path, &text)?;
            for spans in [false, true] {
                let options = Demand { spans, ..Demand::RICH };
                let s = collect(&r, &options)?;
                let ids: Vec<&str> = s.messages.iter().filter_map(|m| m.id.as_deref()).collect();
                assert_eq!(ids, ["u1", "a1", "u2", "c1", "a3"], "{leaf}");
            }
        }
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cold_sqlite_stream_preserves_store_and_checks_original_dependencies() {
        let agents = tmp_agents();
        let db = write_store(&agents, "cold", &branchy_events());
        {
            let writer = rusqlite::Connection::open(&db).unwrap();
            writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        }
        let sidecar = |suffix: &str| {
            let mut path = db.as_os_str().to_os_string();
            path.push(suffix);
            PathBuf::from(path)
        };
        let r = discover_sqlite(&db).unwrap().remove(0);
        let mut expected = crate::stream::CollectSink::default();
        stream_sqlite(&r, &Demand::RICH, &mut expected).unwrap();
        assert!(!expected.messages.is_empty());
        assert!(!sidecar("-wal").exists());
        assert!(!sidecar("-shm").exists());
        let mut mutate = |_: Message| {
            fs::write(sidecar("-journal"), b"changed during callback").unwrap();
            Flow::Stop
        };
        let error = stream_sqlite(&r, &Demand::LEAN.with_spans(), &mut mutate).unwrap_err();
        assert!(error.to_string().contains("dependencies changed"), "{error:#}");
        fs::remove_dir_all(agents.parent().unwrap()).unwrap();
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
        assert_eq!(
            r.message_count,
            Some(5),
            "every user/assistant row counts, branch or not"
        );
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
        assert_eq!(c1.extra["openclaw"]["tokensBefore"], 1234);
        assert_eq!(c1.extra["openclaw"]["firstKeptEntryId"], "u2");
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
        // every reply states the model its row names; the first one is also the session's
        assert!(
            s.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .all(|m| m.model.as_deref() == Some("claude-opus-4")),
            "{:?}",
            s.messages
        );
        for m in &s.messages {
            assert!(
                m.extra
                    .keys()
                    .all(|k| k == "openclaw" || (k == crate::ir::CV_NAMESPACE && m.is_synthesized())),
                "{:?}",
                m.extra
            );
        }

        // Complete parse: everything rides along; off-branch rows are tagged.
        let mut sink = crate::stream::CollectSink::default();
        adapter.stream(r, &Demand::COMPLETE, &mut sink).unwrap();
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
        assert_eq!(
            serde_json::to_value(l1).unwrap()["extra"][crate::ir::CARRIER_KEY]["targetId"],
            "a1"
        );
        // the session name is a notice under complete, carrying its record
        let si = sink.messages.iter().find(|m| m.id.as_deref() == Some("si")).unwrap();
        assert_eq!(si.kind, MessageKind::Notice);
        assert_eq!(si.extra["openclaw"]["record_type"], "session_info");
        assert_eq!(
            serde_json::to_value(si).unwrap()["extra"][crate::ir::CARRIER_KEY]["name"],
            "Named session"
        );
        assert_eq!(si.text().as_deref(), Some("[session named: Named session]"));
        for m in &sink.messages {
            assert!(
                m.extra.keys().all(|k| k == "openclaw"
                    || k == crate::ir::CARRIER_KEY
                    || (k == crate::ir::CV_NAMESPACE && m.is_synthesized())),
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
        // user + assistant rows, branches included; each window also holds one `toolResult`
        // row, which is not a turn (`SessionRef::message_count`).
        assert_eq!(
            main.message_count,
            Some(12),
            "6 user + 6 assistant; the toolResult row is not a turn"
        );
        let fork = by_id("01a0bb4e-2bea-749a-8ec2-c494636244d1");
        assert_eq!(fork.message_count, Some(11), "5 user + 6 assistant");
        let legacy = by_id("3f28190c-96fa-4d73-b37c-251dcccb3f9b");
        assert_eq!(legacy.cwd, Some(PathBuf::from("/Users/ember/ocfid/proj")));
        assert_eq!(legacy.message_count, Some(4), "2 user + 2 assistant");

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
        assert_eq!(ocbag(compaction)["tokensBefore"], 4321);
        assert_eq!(ocbag(compaction)["firstKeptEntryId"], "u1");
        assert!(compaction.content.is_empty() && compaction.is_synthesized());
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
        adapter.stream(main, &Demand::COMPLETE, &mut sink).unwrap();
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
        fs::write(sessions.join("legacy-1.jsonl"), fixture_text("legacy-1.jsonl") + "\n").unwrap();
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
        let r = SessionRef::new("s-sql", Harness::OpenClaw, path.clone());
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
        let text = fixture_text("reset-and-side-entries.jsonl");
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
        assert_eq!(cm.extra["openclaw"]["customType"], "ext.note");
        assert_eq!(cm.extra["openclaw"]["display"], true);
        assert_eq!(cm.text().as_deref(), Some("an extension note"));

        let c = parse_text_with(&text, &r, &Demand::COMPLETE);
        let ids: Vec<&str> = c.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["mc", "u1", "r1", "u2", "lb", "cx", "cm"]);
        let cx = c.messages.iter().find(|m| m.id.as_deref() == Some("cx")).unwrap();
        assert_eq!(cx.kind, MessageKind::Carrier);
        assert_eq!(cx.extra["openclaw"]["record_type"], "custom");
        assert_eq!(
            serde_json::to_value(cx).unwrap()["extra"][crate::ir::CARRIER_KEY]["data"]["ttl"],
            60
        );
        let lb = c.messages.iter().find(|m| m.id.as_deref() == Some("lb")).unwrap();
        assert_eq!(lb.kind, MessageKind::Notice);
        assert_eq!(lb.text().as_deref(), Some("[label u2: pinned prompt]"));
        assert_eq!(
            serde_json::to_value(lb).unwrap()["extra"][crate::ir::CARRIER_KEY]["targetId"],
            "u2"
        );
    }

    #[test]
    fn non_utf8_line_skips_not_truncates() {
        // A stray binary line mid-transcript must cost exactly that line — `map_while(Result::ok)`
        // used to end the whole stream there, silently dropping every later message.
        let dir = std::env::temp_dir().join(format!("cv-openclaw-bin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        // Line 3 of the fixture is undecodable garbage (FF FE 80 81).
        fs::copy(fixture_ref("non-utf8-line.jsonl").path, &path).unwrap();

        let r = SessionRef::new("s1", Harness::OpenClaw, path);
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
    fn windowed_reads_keep_the_model() {
        let _world = crate::test_world("openclaw-windowed");
        let state = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/openclaw/2026.1.29");
        crate::harness::assert_windowed_models(&OpenClaw::with_state_dir(&state));
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
        assert_eq!(
            s.messages[1].model.as_deref(),
            Some("claude-opus-4"),
            "the reply states its model"
        );
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
        assert_eq!(bag["stopReason"], "toolUse");
        assert!(
            bag.get("stop_reason").is_none(),
            "native keys keep their camelCase: {bag}"
        );
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
                assert_eq!(content.inline_str(), Some("file contents"));
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
        assert_eq!(bash.extra["openclaw"]["exitCode"], 0);

        let branch = &s.messages[1];
        assert_eq!(branch.extra["openclaw"]["message_role"], "branchSummary");
        assert_eq!(branch.extra["openclaw"]["fromId"], "abc123");

        // the single-record compaction: the boundary keeps the entry id and counts, the summary
        // its text, hanging off the boundary
        let boundary = &s.messages[2];
        assert_eq!(boundary.id.as_deref(), Some("c3"));
        assert_eq!(boundary.parent_id.as_deref(), Some("c2"));
        assert_eq!(boundary.extra["openclaw"]["message_role"], "compactionSummary");
        assert_eq!(boundary.extra["openclaw"]["tokensBefore"], 5000);
        let summary = &s.messages[3];
        assert_eq!(summary.text().as_deref(), Some("earlier history compacted"));
        assert_eq!(summary.parent_id.as_deref(), Some("c3"));
        assert_eq!(summary.extra["openclaw"]["message_role"], "compactionSummary");
        let comps = crate::compaction::detect_in_session(&s, true).unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(3));

        let custom = &s.messages[4];
        assert_eq!(custom.extra["openclaw"]["message_role"], "custom");
        assert_eq!(custom.extra["openclaw"]["customType"], "openclaw.cache-ttl");
        assert_eq!(custom.extra["openclaw"]["details"]["ttlMs"], 3600000);
    }

    #[test]
    fn assistant_api_failure_is_an_error_turn() {
        let text = fixture_text("assistant-api-failure.jsonl");
        let s = parse_text(&text, &fixture_ref("unused.jsonl"));
        let a1 = &s.messages[1];
        assert_eq!(
            (a1.role, a1.kind, a1.origin),
            (Role::Assistant, MessageKind::Error, Origin::Harness)
        );
        assert_eq!(a1.extra["openclaw"]["error"]["errorMessage"], "overloaded");
        assert_eq!(a1.extra["openclaw"]["error"]["errorCode"], 529);
        assert_eq!(a1.extra["openclaw"]["stopReason"], "error");
        assert!(
            a1.extra["openclaw"].get("errorMessage").is_none(),
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

    /// 2026.10: an event of 1 KiB or more is a zstd frame in `event_zstd` with `event_json` NULL. It
    /// reads like any other row; a frame that does not decode is counted, not dropped silently. A
    /// sub-agent window points at its spawner through `spawned_by`, and the spawner's copy of the
    /// announce (`provenance.kind = inter_session` from a sub-agent key) is the return.
    #[cfg(feature = "sqlite")]
    #[test]
    fn compressed_events_and_subagent_windows() {
        let agents = tmp_agents();
        let agent_dir = agents.join("main").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let db = agent_dir.join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(&fixture_text("transcript-tables.sql")).unwrap();
        conn.execute_batch(
            "DROP TABLE transcript_events;
             CREATE TABLE transcript_events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT,
               created_at INTEGER NOT NULL, event_zstd BLOB, event_utf8_bytes INTEGER, navigation_json TEXT,
               PRIMARY KEY (session_id, seq)) STRICT;",
        )
        .unwrap();
        let child_key = "agent:main:subagent:0f38eb76-c49b-46a0-99f1-1c03787525d8";
        for (sid, key, by) in [
            ("parent-1", "agent:main:explicit:parent-1", None),
            ("child-1", child_key, Some("agent:main:explicit:parent-1")),
        ] {
            conn.execute(
                "INSERT INTO session_nodes (session_key, current_session_id, entry_json, updated_at) VALUES (?1, ?2, '{}', 1)",
                rusqlite::params![key, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_windows (session_id, session_key, created_at, updated_at, spawned_by) VALUES (?1, ?2, 1, 1, ?3)",
                rusqlite::params![sid, key, by],
            )
            .unwrap();
        }
        let long = "x".repeat(2000);
        let spawn_result = format!(
            r#"{{"type":"message","id":"p3","parentId":"p2","timestamp":"2026-10-11T20:00:02Z","message":{{"role":"toolResult","toolCallId":"c1","toolName":"sessions_spawn","content":[{{"type":"text","text":"{{\"status\": \"accepted\", \"childSessionKey\": \"{child_key}\"}} {long}"}}],"isError":false}}}}"#
        );
        let announce = format!(
            r#"{{"type":"message","id":"p4","parentId":"p3","timestamp":"2026-10-11T20:00:03Z","message":{{"role":"user","content":"[Subagent Context] settled","provenance":{{"kind":"inter_session","sourceSessionKey":"{child_key}","sourceTool":"subagent_announce"}}}}}}"#
        );
        type Row<'a> = (&'a str, i64, Option<String>, Option<Vec<u8>>);
        let rows: Vec<Row> = vec![
            ("parent-1", 0, Some(r#"{"type":"session","version":4,"id":"parent-1","timestamp":"2026-10-11T20:00:00Z","cwd":"/w"}"#.into()), None),
            ("parent-1", 1, Some(r#"{"type":"message","id":"p2","parentId":null,"timestamp":"2026-10-11T20:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"sessions_spawn","arguments":{}}],"model":"m"}}"#.into()), None),
            ("parent-1", 2, None, Some(zstd::encode_all(spawn_result.as_bytes(), 1).unwrap())),
            ("parent-1", 3, Some(announce), None),
            ("parent-1", 4, None, Some(b"\x28\xb5\x2f\xfd not a frame".to_vec())),
            ("child-1", 0, Some(r#"{"type":"message","id":"k1","parentId":null,"timestamp":"2026-10-11T20:00:01Z","message":{"role":"user","content":"Reply ok."}}"#.into()), None),
        ];
        for (sid, seq, json, zstd) in rows {
            conn.execute(
                "INSERT INTO transcript_events (session_id, seq, event_json, event_zstd, created_at) VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![sid, seq, json, zstd],
            )
            .unwrap();
        }
        drop(conn);
        let a = OpenClaw::with_state_dir(agents.parent().unwrap());
        let refs = a.discover().unwrap();
        let by_id = |id: &str| a.parse(refs.iter().find(|r| r.id == id).unwrap()).unwrap();
        let parent = by_id("parent-1");
        let kinds: Vec<MessageKind> = parent.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::SubagentSpawn,
                MessageKind::ToolResult,
                MessageKind::SubagentReturn
            ]
        );
        assert!(format!("{:?}", parent.messages[1].content).contains(&long));
        assert_eq!(parent.messages[2].origin, Origin::Subagent);
        assert_eq!(parent.cv_extra().unwrap()["skipped_lines"], 1);
        let child = by_id("child-1");
        assert_eq!(child.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(child.extra["openclaw"]["spawned_by"], "agent:main:explicit:parent-1");
        assert_eq!(child.messages[0].origin, Origin::Subagent);
        let _ = fs::remove_dir_all(agents.parent().unwrap());
    }

    #[test]
    fn bounded_parent_scan_matches_lexical_policy_across_boundaries_and_utf8_failures() -> Result<()> {
        let world = crate::test_world("openclaw-parent-scan");
        let path = world.base.join("parent.jsonl");
        let key = "agent:main:subagent:child-λ";
        for marker in [
            format!("\"childSessionKey\":\"{key}\""),
            format!("childSessionKey\\\": \\\"{key}"),
            format!("unrelated {key}"),
            format!("childSessionKey{} {key}", " ".repeat(30)),
        ] {
            for padding in [0, 65510, 65520, 65530, 65540, 65550] {
                let text = format!("{}雪{marker}{}", "x".repeat(padding), "雪".repeat(30000));
                fs::write(&path, &text)?;
                assert_eq!(names_child_file(&path, key)?, names_child(&text, key));
                let mut bytes = text.into_bytes();
                bytes.push(0xff);
                fs::write(&path, bytes)?;
                assert!(!names_child_file(&path, key)?);
            }
        }
        Ok(())
    }

    #[test]
    fn child_bodies_and_completion_guard_parent_lookup_dependencies() -> Result<()> {
        let world = crate::test_world("openclaw-parent-source-guard");
        let dir = world.base.join("agents/main/sessions");
        fs::create_dir_all(&dir)?;
        let path = dir.join("child.jsonl");
        let parent = dir.join("parent.jsonl");
        let key = "agent:main:subagent:child";
        let index = dir.join("sessions.json");
        fs::write(
            &index,
            serde_json::json!({"agent:main:main":{"sessionId":"parent"},
            key:{"sessionId":"child","spawnedBy":"agent:main:main"}})
            .to_string(),
        )?;
        fs::write(&parent, serde_json::json!({"childSessionKey":key}).to_string())?;
        fs::write(
            &path,
            serde_json::json!({"type":"message","id":"c","message":{"role":"user",
            "content":"guarded child λ\n".repeat(10000)}})
            .to_string(),
        )?;
        let adapter = OpenClaw::with_state_dir(&world.base);
        let reference = SessionRef::new("child", Harness::OpenClaw, &path);
        let mut child = crate::stream::collect_with(&adapter, &reference, &Demand::LEAN.with_spans())?;
        assert_eq!(child.lineage.parent.as_deref(), Some("parent"));
        fs::write(&parent, "changed parent")?;
        assert!(child.materialize().is_err());
        fs::write(&parent, serde_json::json!({"childSessionKey":key}).to_string())?;
        struct ChangeParent(PathBuf);
        impl MessageSink for ChangeParent {
            fn message(&mut self, _: Message) -> Flow {
                fs::write(&self.0, "changed again").unwrap();
                Flow::Stop
            }
        }
        assert!(adapter
            .stream(&reference, &Demand::RICH, &mut ChangeParent(parent))
            .is_err());
        Ok(())
    }

    #[test]
    fn jsonl_key_inventory_matches_value_first_key_and_final_duplicate_precedence() -> Result<()> {
        let world = crate::test_world("openclaw-json-key-duplicates");
        let path = world.base.join("sessions.json");
        for text in [
            r#"{"z":{"sessionId":"same","spawnedBy":"first"},"a":{"sessionId":"same","spawnedBy":"second"}}"#,
            r#"{"a":{"sessionId":"same","spawnedBy":"first"},"z":{"sessionId":"same","spawnedBy":"second"}}"#,
            r#"{"first":{"sessionId":"old","spawnedBy":"gone"},"second":{"sessionId":"old","spawnedBy":"winner"},"first":{"sessionId":"new","spawnedBy":"replacement"}}"#,
            r#"{"first":{"sessionId":"old"},"second":{"sessionId":"old","spawnedBy":"winner"},"first":null}"#,
            r#"{"first":{"sessionId":"same","sessionId":"new","spawnedBy":"old","spawnedBy":"winner"},"second":{"sessionId":"same"}}"#,
            r#"{"escaped":{"session\u0049d":"same","spawned\u0042y":"winner"},"later":{"sessionId":"same"}}"#,
        ] {
            fs::write(&path, text)?;
            let actual = JsonKeys::read(&path);
            let Value::Object(owned) = serde_json::from_str(text)? else {
                unreachable!();
            };
            for id in ["same", "old", "new", "missing"] {
                let expected = owned
                    .iter()
                    .find(|(_, v)| v.get("sessionId").and_then(Value::as_str) == Some(id))
                    .map(|(key, v)| {
                        (
                            key.clone(),
                            v.get("spawnedBy").and_then(Value::as_str).map(str::to_owned),
                        )
                    });
                assert_eq!(actual.sessions.get(id), expected.as_ref(), "{text} id={id}");
            }
            let expected: HashMap<String, String> = owned
                .iter()
                .filter_map(|(key, v)| Some((key.clone(), v.get("sessionId")?.as_str()?.to_owned())))
                .collect();
            assert_eq!(actual.preferred, expected, "{text}");
        }
        Ok(())
    }

    #[test]
    fn jsonl_key_inventory_reads_once_per_scope_and_keeps_negative_dependency_guards() -> Result<()> {
        let world = crate::test_world("openclaw-json-key-scope");
        let dir = world.base.join("agents/main/sessions");
        fs::create_dir_all(&dir)?;
        let adapter = OpenClaw::with_state_dir(&world.base);
        let mut index = serde_json::Map::new();
        for i in 0..40 {
            fs::write(
                dir.join(format!("child-{i}.jsonl")),
                serde_json::json!({"type":"message","id":"u",
                "message":{"role":"user","content":"checked λ\n".repeat(1000)}})
                .to_string(),
            )?;
            index.insert(
                format!("agent:main:cron:job-{i}"),
                serde_json::json!({"sessionId":format!("child-{i}")}),
            );
        }
        let read = |i, opts: &Demand| {
            crate::stream::collect_with(
                &adapter,
                &SessionRef::new(
                    format!("child-{i}"),
                    Harness::OpenClaw,
                    dir.join(format!("child-{i}.jsonl")),
                ),
                opts,
            )
        };
        JSON_KEY_READS.with(|count| count.set(0));
        {
            let _scope = crate::source::read_scope();
            let mut absent = read(0, &Demand::LEAN.with_spans())?;
            for i in 1..40 {
                assert_eq!(read(i, &Demand::RICH)?.messages[0].origin, Origin::Human);
            }
            assert_eq!(JSON_KEY_READS.with(|count| count.get()), 1);
            fs::write(dir.join("sessions.json"), Value::Object(index).to_string())?;
            assert_eq!(read(1, &Demand::RICH)?.messages[0].origin, Origin::Scheduler);
            assert_eq!(JSON_KEY_READS.with(|count| count.get()), 2);
            assert!(
                absent.materialize().is_err(),
                "cached absence must remain a source guard"
            );
        }
        JSON_KEY_READS.with(|count| count.set(0));
        for i in 0..3 {
            assert_eq!(read(i, &Demand::RICH)?.messages[0].origin, Origin::Scheduler);
        }
        assert_eq!(
            JSON_KEY_READS.with(|count| count.get()),
            3,
            "standalonecalls must not retain inventory"
        );
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_scoped_parent_fallback_keys_both_parameters_under_nocase() -> Result<()> {
        let world = crate::test_world("openclaw-nocase-parent-scope");
        let path = world.base.join("sessions.db");
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE session_windows(session_id TEXT,session_key TEXT COLLATE NOCASE,spawned_by TEXT);
            INSERT INTO session_windows VALUES ('child-a','PARENT','parent'),('child-b','PARENT','parent');",
        )?;
        let mut reads = crate::source::ReadSet::default();
        reads.record(&path);
        let _scope = crate::source::read_scope();
        for id in ["child-a", "child-b", "child-a", "child-b"] {
            let original: String = conn.query_row(
                "SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2",
                ["parent", id],
                |row| row.get(0),
            )?;
            let reference = SessionRef::new(id, Harness::OpenClaw, &path);
            let events = Events::new(&conn, &path, &reads)?;
            let facts = sqlite_key_facts(&conn, &reference, &reads, &events)?;
            assert_eq!(facts.parent.as_deref(), Some(original.as_str()));
            assert_ne!(
                facts.parent.as_deref(),
                Some(id),
                "the SQL exclusion remains child-specific"
            );
        }
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_window_membership_preserves_sql_order_nulls_and_selected_errors() -> Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE session_windows(session_key TEXT,session_id TEXT);
            INSERT INTO session_windows VALUES ('parent','z'),('parent',NULL),('parent','self'),
                ('parent','a'),('parent','z'),('other',x'01'),('utf8',CAST(x'ff' AS TEXT));",
        )?;
        assert!(plain_parent_scan(&conn, "parent", "self")?);
        let (groups, _) = WindowGroups::load(&conn)?;
        let oracle = conn
            .prepare("SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?
            .query_map(["parent", "self"], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(groups.candidates("parent", "self")?, oracle);
        assert!(groups.candidates("missing", "self")?.is_empty());
        assert!(groups.candidates("other", "self").is_err());
        assert!(groups.candidates("utf8", "self").is_err());
        for key in ["other", "utf8"] {
            let native = conn
                .query_row(
                    "SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2",
                    [key, "self"],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_err();
            let indexed = groups.candidates(key, "self").unwrap_err();
            assert_eq!(
                indexed.downcast_ref::<rusqlite::Error>().unwrap().to_string(),
                native.to_string()
            );
        }
        conn.execute("CREATE INDEX windows_by_key ON session_windows(session_key)", [])?;
        assert!(!plain_parent_scan(&conn, "parent", "self")?);
        for ddl in [
            "CREATE TABLE session_windows(session_key TEXT COLLATE NOCASE,session_id TEXT)",
            "CREATE TABLE session_windows(session_key TEXT,session_id TEXT COLLATE NOCASE)",
            "CREATE TABLE session_windows(session_key INTEGER,session_id TEXT)",
            "CREATE TABLE session_windows(session_key TEXT,session_id INTEGER)",
        ] {
            let custom = rusqlite::Connection::open_in_memory()?;
            custom.execute_batch(ddl)?;
            assert!(!plain_parent_scan(&custom, "parent", "self")?, "{ddl}");
        }
        // Oversized inventories remain indexed instead of falling back to a scan per key.
        let large = rusqlite::Connection::open_in_memory()?;
        large.execute_batch(
            "CREATE TABLE session_windows(session_key TEXT,session_id TEXT);
            INSERT INTO session_windows VALUES ('parent',printf('%.*c',9*1024*1024,'x'));",
        )?;
        let (groups, _) = WindowGroups::load(&large)?;
        assert_eq!(groups.candidates("parent", "self")?[0].len(), 9 * 1024 * 1024);
        assert!(groups.candidates("missing", "self")?.is_empty());
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_large_window_selection_uses_index_and_matches_native_scan() -> Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE session_windows(session_key TEXT,session_id TEXT)")?;
        {
            let mut insert = conn.prepare("INSERT INTO session_windows VALUES (?1,?2)")?;
            for ordinal in 0..25000 {
                insert.execute(rusqlite::params![
                    format!("key-{}", ordinal % 500),
                    format!("{ordinal}:{}", "x".repeat(512))
                ])?;
            }
        }
        let (groups, _) = WindowGroups::load(&conn)?;
        let mut native_steps = 0;
        let mut expected = Vec::new();
        for i in 0..100 {
            let mut stmt =
                conn.prepare("SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?;
            expected.push(
                stmt.query_map([format!("key-{i}"), "self".into()], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
            native_steps += stmt.get_status(rusqlite::StatementStatus::FullscanStep);
        }
        for (i, oracle) in expected.iter().enumerate() {
            assert_eq!(&groups.candidates(&format!("key-{i}"), "self")?, oracle);
        }
        let disk_bytes = groups.0.with(|index| {
            let mut stmt =
                index.prepare("SELECT session_id FROM windows WHERE key=?1 AND session_id!=?2 ORDER BY ordinal")?;
            assert_eq!(
                stmt.query_map(rusqlite::params![b"key-0".as_slice(), "self"], |row| row
                    .get::<_, String>(0))?
                    .count(),
                50
            );
            assert_eq!(stmt.get_status(rusqlite::StatementStatus::FullscanStep), 0);
            Ok(index.query_row(
                "SELECT page_count*page_size FROM pragma_page_count(),pragma_page_size()",
                [],
                |row| row.get::<_, u64>(0),
            )?)
        })?;
        assert!(disk_bytes > 8 * 1024 * 1024);
        assert_eq!(native_steps, 100 * 24999);
        let reads = crate::source::ReadSet::default();
        let selector = Path::new("/synthetic/openclaw-query-promotion");
        let _scope = crate::source::read_scope();
        assert_eq!(
            &*sqlite_parent_candidates(&conn, "key-0", "self", selector, &reads)?,
            &expected[0]
        );
        // A repeated identical query is memoized and must not construct a private index.
        assert_eq!(
            &*sqlite_parent_candidates(&conn, "key-0", "self", selector, &reads)?,
            &expected[0]
        );
        for (i, oracle) in expected.iter().enumerate().skip(1) {
            assert_eq!(
                &*sqlite_parent_candidates(&conn, &format!("key-{i}"), "self", selector, &reads)?,
                oracle
            );
        }
        let tiny = rusqlite::Connection::open_in_memory()?;
        tiny.execute_batch("CREATE TABLE session_windows(session_key TEXT,session_id TEXT); INSERT INTO session_windows VALUES ('key','a'),('key','b')")?;
        let (small, _) = WindowGroups::load(&tiny)?;
        for _ in 0..1000 {
            assert_eq!(small.candidates("key", "self")?, ["a", "b"]);
        }
        for _ in 0..1000 {
            let mut stmt =
                tiny.prepare("SELECT session_id FROM session_windows WHERE session_key=?1 AND session_id!=?2")?;
            assert_eq!(
                stmt.query_map(["key", "self"], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                ["a", "b"]
            );
        }
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_parent_window_scan_promotes_after_a_distinct_query_and_preserves_guards() -> Result<()> {
        let world = crate::test_world("openclaw-sql-parent-scope");
        let dir = world.base.join("agents/main/agent");
        fs::create_dir_all(&dir)?;
        let path = dir.join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(&fixture_text("transcript-tables.sql"))?;
        conn.execute("INSERT INTO session_nodes(session_key,current_session_id,entry_json,updated_at) VALUES ('parent-key','parent','{}',1)",[])?;
        conn.execute("INSERT INTO session_windows(session_id,session_key,created_at,updated_at) VALUES ('parent','parent-key',1,1)",[])?;
        for i in 0..40 {
            let sid = format!("child-{i}");
            conn.execute(
                "INSERT INTO session_nodes(session_key,current_session_id,entry_json,updated_at) VALUES (?1,?2,'{}',1)",
                rusqlite::params![format!("child-key-{i}"), sid],
            )?;
            conn.execute("INSERT INTO session_windows(session_id,session_key,spawned_by,created_at,updated_at) VALUES (?1,?2,'parent-key',1,1)",rusqlite::params![sid,format!("child-key-{i}")])?;
            conn.execute(
                "INSERT INTO transcript_events(session_id,seq,event_json,created_at) VALUES (?1,0,?2,1)",
                rusqlite::params![
                    sid,
                    serde_json::json!({"type":"message","id":"u","message":{"role":"user","content":"prompt"}})
                        .to_string()
                ],
            )?;
        }
        for i in 0..40 {
            let parent = format!("distinct-parent-{i}");
            let child = format!("distinct-child-{i}");
            let key = format!("distinct-key-{i}");
            for (sid, key) in [(&parent, &key), (&child, &format!("distinct-child-key-{i}"))] {
                conn.execute("INSERT INTO session_nodes(session_key,current_session_id,entry_json,updated_at) VALUES (?1,?2,'{}',1)",rusqlite::params![key,sid])?;
                conn.execute(
                    "INSERT INTO session_windows(session_id,session_key,created_at,updated_at) VALUES (?1,?2,1,1)",
                    rusqlite::params![sid, key],
                )?;
            }
            conn.execute(
                "UPDATE session_windows SET spawned_by=?1 WHERE session_id=?2",
                rusqlite::params![key, child],
            )?;
        }
        conn.execute("INSERT INTO session_nodes(session_key,current_session_id,entry_json,updated_at) VALUES ('child-negative','negative','{}',1)",[])?;
        conn.execute("INSERT INTO session_windows(session_id,session_key,spawned_by,created_at,updated_at) VALUES ('negative','child-negative','not-yet',1,1)",[])?;
        drop(conn);
        let adapter = OpenClaw::with_state_dir(&world.base);
        let read = |id: &str| {
            crate::stream::collect_with(&adapter, &SessionRef::new(id, Harness::OpenClaw, &path), &Demand::RICH)
        };
        SQL_PARENT_ROWS.with(|count| count.set(0));
        SQL_PARENT_FULLSCANS.with(|count| count.set(0));
        let one_scan;
        {
            let _scope = crate::source::read_scope();
            for i in 0..40 {
                assert_eq!(read(&format!("child-{i}"))?.lineage.parent.as_deref(), Some("parent"));
                if i == 0 {
                    assert_eq!(
                        SQL_PARENT_ROWS.with(|count| count.get()),
                        1,
                        "one query keeps native startup"
                    );
                    let steps = SQL_PARENT_FULLSCANS.with(|count| count.get());
                    assert_eq!(read("child-0")?.lineage.parent.as_deref(), Some("parent"));
                    assert_eq!(
                        SQL_PARENT_ROWS.with(|count| count.get()),
                        1,
                        "same memoized query does not promote"
                    );
                    assert_eq!(SQL_PARENT_FULLSCANS.with(|count| count.get()), steps);
                }
            }
            for i in 0..40 {
                assert_eq!(
                    read(&format!("distinct-child-{i}"))?.lineage.parent.as_deref(),
                    Some(format!("distinct-parent-{i}").as_str())
                );
            }
            assert_eq!(SQL_PARENT_ROWS.with(|count| count.get()), 123);
            one_scan = SQL_PARENT_FULLSCANS.with(|count| count.get());
            assert_eq!(one_scan, 2 * 121, "first query plus one whole-store inventory scan");
            assert!(read("negative")?.lineage.parent.is_none());
            let conn = rusqlite::Connection::open(&path)?;
            conn.execute("INSERT INTO session_nodes(session_key,current_session_id,entry_json,updated_at) VALUES ('not-yet','new-parent','{}',1)",[])?;
            conn.execute("INSERT INTO session_windows(session_id,session_key,created_at,updated_at) VALUES ('new-parent','not-yet',1,1)",[])?;
            drop(conn);
            assert_eq!(read("negative")?.lineage.parent.as_deref(), Some("new-parent"));
        }
        SQL_PARENT_ROWS.with(|count| count.set(0));
        SQL_PARENT_FULLSCANS.with(|count| count.set(0));
        for i in 0..3 {
            assert_eq!(read(&format!("child-{i}"))?.lineage.parent.as_deref(), Some("parent"));
        }
        assert_eq!(SQL_PARENT_ROWS.with(|count| count.get()), 3);
        assert!(SQL_PARENT_FULLSCANS.with(|count| count.get()) > one_scan);
        Ok(())
    }

    #[test]
    fn preferred_jsonl_parent_avoids_global_candidate_inventory_and_preserves_fallback_order() -> Result<()> {
        let world = crate::test_world("openclaw-parent-preferred-order");
        let dir = &world.base;
        let child = dir.join("child.jsonl");
        let preferred = dir.join("preferred.jsonl");
        let key = "agent:main:subagent:child";
        fs::write(
            dir.join("sessions.json"),
            serde_json::json!({"parent-key":{"sessionId":"preferred"},
            key:{"sessionId":"child","spawnedBy":"parent-key"}})
            .to_string(),
        )?;
        fs::write(&child, "{}")?;
        fs::write(&preferred, serde_json::json!({"childSessionKey":key}).to_string())?;
        for i in 0..300 {
            fs::write(dir.join(format!("unrelated-{i:04}.jsonl")), "{}")?;
        }
        fs::write(
            dir.join("a-earlier.jsonl"),
            serde_json::json!({"childSessionKey":key}).to_string(),
        )?;
        let mut consulted = Vec::new();
        let facts = jsonl_key_facts(&child, "child", &mut |p| consulted.push(p.to_path_buf()))?;
        assert_eq!(facts.parent.as_deref(), Some("preferred"));
        assert_eq!(consulted, vec![dir.join("sessions.json"), preferred.clone()]);
        // The missing/unmatched preferred path stays a dependency; fallback candidates retain
        // their lexicographic precedence and the child's own path is never consulted.
        fs::write(&preferred, "unrelated")?;
        consulted.clear();
        let facts = jsonl_key_facts(&child, "child", &mut |p| consulted.push(p.to_path_buf()))?;
        assert_eq!(facts.parent.as_deref(), Some("a-earlier"));
        assert_eq!(
            consulted,
            vec![
                dir.join("sessions.json"),
                preferred,
                dir.clone(),
                dir.join("a-earlier.jsonl")
            ]
        );
        Ok(())
    }

    /// 2026.1: `sessions.json` names a sub-agent's spawner by KEY, and a reset has since re-pointed
    /// that key at a newer session, so the spawner is the transcript recording the child's key as
    /// `childSessionKey`, not the key's current session. A cron run's key makes its prompt the
    /// scheduler's.
    #[test]
    fn jsonl_subagent_parent_survives_a_reset_and_cron_prompts_are_scheduled() {
        let agents = tmp_agents();
        let dir = agents.join("main").join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let child_key = "agent:main:subagent:40223705-4ebc-4437-855c-e6a384c8c77a";
        let user = |id: &str, text: &str| {
            format!(
                r#"{{"type":"message","id":"{id}","parentId":null,"timestamp":"2026-10-11T20:00:00Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        let spawn = format!(
            r#"{{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-11T20:00:01Z","message":{{"role":"toolResult","toolCallId":"c","toolName":"sessions_spawn","content":[{{"type":"text","text":"{{\n  \"childSessionKey\": \"{child_key}\"\n}}"}}],"isError":false}}}}"#
        );
        fs::write(
            dir.join("old-main.jsonl"),
            format!("{}\n{spawn}\n", user("a", "spawn one")),
        )
        .unwrap();
        // the key's current session only lists the child (a bare mention is not a spawn)
        fs::write(
            dir.join("new-main.jsonl"),
            format!("{}\n", user("n", &format!("list {child_key}"))),
        )
        .unwrap();
        fs::write(dir.join("child.jsonl"), format!("{}\n", user("k", "Reply ok."))).unwrap();
        fs::write(
            dir.join("cronrun.jsonl"),
            format!("{}\n", user("r", "[cron:j1 ping] pong")),
        )
        .unwrap();
        fs::write(
            dir.join("sessions.json"),
            format!(
                r#"{{"agent:main:main":{{"sessionId":"new-main"}},"{child_key}":{{"sessionId":"child","spawnedBy":"agent:main:main"}},"agent:main:cron:j1":{{"sessionId":"cronrun"}}}}"#
            ),
        )
        .unwrap();
        let a = OpenClaw::with_state_dir(agents.parent().unwrap());
        let refs = a.discover().unwrap();
        let by_id = |id: &str| a.parse(refs.iter().find(|r| r.id == id).unwrap()).unwrap();
        let child = by_id("child");
        assert_eq!(child.lineage.parent.as_deref(), Some("old-main"));
        assert_eq!(child.extra["openclaw"]["session_key"], child_key);
        assert_eq!(child.messages[0].origin, Origin::Subagent);
        assert_eq!(by_id("cronrun").messages[0].origin, Origin::Scheduler);
        let main = by_id("new-main");
        assert!(main.lineage.is_empty());
        assert_eq!(main.messages[0].origin, Origin::Human);
        let _ = fs::remove_dir_all(agents.parent().unwrap());
    }
}
