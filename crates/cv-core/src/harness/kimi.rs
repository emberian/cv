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
//!   (timestamps + token usage), optional `state.json` (`custom_title`), and rotated-out segments
//!   `context_1.jsonl…context_N.jsonl`. `/compact`, `/clear` and a rewind each rotate the live file
//!   out; the session is ALL segments, oldest first, then the live `context.jsonl` (the head a new
//!   segment repeats — the system prompt, or a rewind's copied prefix — is read once). A segment
//!   that opens with the compaction summary gets a `CompactionBoundary`; any other rotation gets a
//!   synthetic `Branch` (`extra["kimi"]["segment"]`). The count stays in
//!   `extra["kimi"]["compaction_segments"]`.
//! - **Sub-agents:** `<session>/subagents/<agent_id>/` holds each `Agent`-tool child in the same
//!   format (`context.jsonl`, `wire.jsonl`) plus `meta.json` (`subagent_type`, `description`, …);
//!   [`subagent_refs`] lists them, parsed with `lineage.parent` = the session and `agent_path` =
//!   the agent id. Also in a session dir, read by nothing here: `tasks/<id>/` (background shells:
//!   `spec.json`, `runtime.json`, `output.log`, …) and `notifications/<id>/` (`event.json`,
//!   `delivery.json`); `state.json` also keeps `todos` (read as the session's `plan` state,
//!   [`Adapter::store_affordances`]), `plan_mode`, `approval`.
//! - **Legacy (flat form):** `sessions/<hash>/<uuid>.jsonl` — the transcript only, no sidecar/dir.
//!   (kimi-cli migrates these to the dir form on next open; we still read them in place.)
//!
//! ## `context.jsonl` records (one JSON object per line) → kind / origin
//! Ground truth is kimi-cli's `soul/context.py` (the writer) and `soul/kimisoul.py` (the loop).
//! - `{"role":"_system_prompt","content":str}` → `SystemPrompt`/Harness, and `Session::system_prompt`
//!   (`Context.write_system_prompt`, the first record of every segment).
//! - `{"role":"_checkpoint","id":N}` → a per-step rewind target (`KimiSoul._checkpoint()` runs at the
//!   top of EVERY step, `kimisoul.py:1036`; `Context.revert_to(id)` truncates back to it). It marks
//!   nothing the reader needs, so it is a `Carrier` under [`Demand::COMPLETE`] and dropped
//!   otherwise — NOT a `Branch` and NOT a `CompactionBoundary`: the segment rotation
//!   (`context.jsonl` → `context_N.jsonl`) is done by `revert_to` (rewind) and `clear` (compaction,
//!   `kimisoul.py:1577`), never by a checkpoint.
//! - `{"role":"_usage","token_count":N}` → not a message: the request input-token count kimi-cli
//!   logs after a step (`update_token_count(usage.input)`, `kimisoul.py:1267`). It lands on the
//!   preceding message as `extra["kimi"]["token_count"]`, and — on a `Reply` the wire sidecar gave
//!   no usage for — as `usage.input_tokens`.
//! - `{"role":"user","content": str | [Part]}` → `Prompt`/Human, unless kimi-cli's own markers say
//!   the harness wrote it: a first text part starting `<system>Previous context has been compacted`
//!   (`soul/compaction.py::COMPACTION_OUTPUT_PREFIX`) is the `CompactionSummary` that seeds the
//!   rotated segment — preceded by a synthetic `CompactionBoundary`, since the boundary itself is
//!   the file rotation; `<notification …>` (`notifications/llm.py`) is `InjectedContext`/Scheduler;
//!   `<system>…</system>` / `<system-reminder>` (`soul/message.py::system[_reminder]`) is
//!   `InjectedContext`/Harness. `content` is a BARE STRING for a single text part, else a list.
//! - `{"role":"assistant","content":[Part],"tool_calls":[…]}` → `Reply`/Model.
//! - `{"role":"tool","content": str | [Part],"tool_call_id":id}` → `ToolResult`/Harness (one
//!   ToolResult block, `tool_name` resolved from the call). The first text part is often a
//!   `<system>summary</system>` line.
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

use super::store::document::Fields;
use super::store::jsonl;
use super::wire::openai;
use super::Adapter;
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::lazy::{Text, INLINE_MAX};
use crate::source::{Dependencies, Probe};
use crate::stream::{Checkpoint, Demand, Flow, MessageSink, Start, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;

use std::path::{Path, PathBuf};

mod record;

pub struct Kimi {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
    /// The share root itself (`~/.kimi`), for locating `kimi.json` / `config.toml`.
    root: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Kimi = Kimi {
    sessions: None,
    root: None,
};

impl Kimi {
    pub fn detect() -> Self {
        let root = crate::env::var_os("KIMI_SHARE_DIR")
            .map(PathBuf::from)
            .or_else(|| crate::env::home_dir().map(|h| h.join(".kimi")))
            .filter(|p| p.exists());
        match root {
            Some(r) => Kimi::with_home(&r),
            None => Kimi {
                sessions: None,
                root: None,
            },
        }
    }

    /// An adapter over an explicit kimi-cli share dir (the dir holding `sessions/` and
    /// `kimi.json`), instead of `$KIMI_SHARE_DIR` / `~/.kimi` — for the harness corpus and other
    /// tests, without mutating the process environment.
    pub fn with_home(share_dir: &Path) -> Self {
        let root = Some(share_dir.to_path_buf()).filter(|p| p.exists());
        let sessions = root.as_ref().map(|r| r.join("sessions")).filter(|p| p.exists());
        Kimi { sessions, root }
    }
}

impl Adapter for Kimi {
    fn harness(&self) -> Harness {
        Harness::Kimi
    }

    fn has_subagent_forest(&self) -> bool {
        true
    }

    fn record_forest_dependencies(&self, r: &SessionRef, guard: &mut crate::source::ReadSet) {
        guard.record(&r.path);
        let sub_dir = r.path.join("subagents");
        guard.record(&sub_dir);
        if let Ok(entries) = fs::read_dir(&sub_dir) {
            for entry in entries.flatten() {
                guard.record(&entry.path());
            }
        }
    }

    /// The `subagents/<agent_id>/` transcripts beside the session ([`subagent_refs`]).
    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(super::ref_forest(subagent_refs(r)?, workflow, select))
    }

    /// A `<notification>` turn: a background task finished.
    fn record_affordance(
        &self,
        _m: &Message,
        first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        Ok(first
            .starts_with("<notification")?
            .then_some(crate::affordance::RecordHit {
                kind: crate::affordance::Kind::BackgroundJob,
                action: "complete",
                tool: "notification",
            }))
    }

    fn dependencies(&self, r: &SessionRef, probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if let Some(root) = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
            .and_then(Path::parent)
        {
            deps.files.push(root.join("config.toml"));
        }
        deps.files.push(r.path.join("meta.json"));
        if probe.exists(&r.path.join("context.jsonl"))? {
            // Includes rotated segments and their membership; no subagent/background recursion.
            deps.directories.push(r.path.clone());
        } else {
            deps.files.push(r.path.join(format!("{}.jsonl", r.id)));
        }
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        // The share root (`~/.kimi`), NOT `<root>/sessions`: `kimi.json` (the cwd registry) lives
        // at this root beside `sessions/`, so the root is what a consumer needs.
        // (Returning `sessions` here produced `~/.kimi/sessions/sessions/...`, which kimi-cli never
        // discovers.) Fall back to `<sessions>/..` if only the sessions dir was detected.
        self.root
            .clone()
            .or_else(|| self.sessions.as_ref().and_then(|s| s.parent().map(Path::to_path_buf)))
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let _cache_scope = crate::discover_cache::read::Context::current().enter();
        let Some(sessions) = &self.sessions else {
            return Ok(Default::default());
        };
        // hash (dir basename, possibly "<kaos>_<md5>") -> cwd, from kimi.json.
        let hash_to_cwd = self
            .root
            .as_ref()
            .map(|r| load_work_dirs(&r.join("kimi.json")))
            .unwrap_or_default();
        // An unreadable dir, and a transcript that fails to read, are skipped and reported.
        let mut out = crate::Discovered::default();
        let Some(hash_dirs) = out.keep(sessions, fs::read_dir(sessions).map_err(Into::into)) else {
            return Ok(out);
        };
        for hd in hash_dirs {
            let Some(hd) = out.keep(sessions, hd.map_err(Into::into)) else {
                continue;
            };
            let hash_path = hd.path();
            let Some(basename) = hash_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !hash_path.is_dir() {
                continue;
            }
            let cwd = hash_to_cwd.get(basename).cloned();
            let Some(entries) = out.keep(&hash_path, fs::read_dir(&hash_path).map_err(Into::into)) else {
                continue;
            };
            for e in entries {
                let Some(e) = out.keep(&hash_path, e.map_err(Into::into)) else {
                    continue;
                };
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                let (id, transcript, dir): (String, PathBuf, PathBuf) = if p.is_dir() {
                    // Modern: <uuid>/context.jsonl
                    if !is_modern(&p) {
                        continue;
                    }
                    (name.to_string(), p.join("context.jsonl"), p.clone())
                } else if name.ends_with(".jsonl") {
                    // Legacy: <uuid>.jsonl (flat). source dir is the hash dir.
                    let id = name.trim_end_matches(".jsonl").to_string();
                    (id, p.clone(), hash_path.clone())
                } else {
                    continue;
                };
                // `kimi.json` supplies only the cwd, applied after the cache: in the key, every
                // rewrite of the registry rescanned every session of the store.
                let mut reads = crate::source::ReadSet::default();
                reads.record(&dir.join("state.json"));
                if is_modern_transcript(&transcript) {
                    // Capture membership before enumerating rotations, and each existing file
                    // before the scan reads it. Missing wire/state files are dependencies too.
                    reads.record(&dir);
                    reads.record(&dir.join("wire.jsonl"));
                    for segment in transcript_segments(&dir, &transcript) {
                        reads.record(&segment);
                    }
                }
                let hit = crate::discover_cache::cached_scan_with_reads(&transcript, reads, || {
                    scan(&id, &transcript, &dir, None)
                })
                .map(|r| {
                    r.map(|mut r| {
                        r.cwd.clone_from(&cwd);
                        r
                    })
                });
                if let Some(found) = crate::Skipped::listed(&transcript, hit) {
                    out.push(found);
                }
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    /// The rotated segments then the live `context.jsonl`, each through the record codec; a
    /// checkpoint resumes the live file only. Its own segment loop ([`visit_segments`]), not
    /// [`jsonl::drive`]: a checkpoint is offered mid-record, at the start of the line whose
    /// message flushes the held one, and the callbacks go through the same guard per segment.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        match from {
            Start::Whole => self.whole(r, opts, sink).map(Some),
            Start::Checkpoint(cp) => self.resumed(r, opts, sink, cp),
            Start::Prefix(_) => Ok(None),
        }
    }

    /// The todo list `SetTodoList` keeps in the session dir's `state.json` (`todos: [{title,
    /// status}]`). A legacy flat session (no `context.jsonl` in its dir) has no state file.
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        if !is_modern(&r.path) {
            return Ok(Vec::new());
        }
        let Some(todos) =
            crate::affordance::side_json(&r.path.join("state.json"))?.and_then(|v| v.get("todos").cloned())
        else {
            return Ok(Vec::new());
        };
        let items = crate::affordance::items_of(&todos);
        Ok(
            crate::affordance::plan_state(crate::affordance::Kind::Plan, "state.json", items)
                .into_iter()
                .collect(),
        )
    }

    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        // Native layout is fixed: sessions/<hash>/<id> (or its subagent dir) is modern;
        // sessions/<hash> is the legacy flat store. No mutable exists() decision selects a mark.
        let modern = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
            .and_then(|root| r.path.strip_prefix(root).ok())
            .is_some_and(|p| p.components().count() > 1)
            || r.path.file_name().and_then(|p| p.to_str()) == r.id.rsplit('/').next();
        if modern {
            r.path.join("context.jsonl")
        } else {
            r.path.join(format!("{}.jsonl", r.id))
        }
    }
}

impl Kimi {
    fn whole(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Kimi, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let (transcript, wire) = locate_transcript(&r.path, &r.id);
            let segments = transcript_segments(&r.path, &transcript);
            note_config_dependency(self, sink);
            let (s, enrich) = session_header(self, r, opts, &segments, wire.as_deref());
            sink.meta(&s);
            let primary = self.resume_source(r);
            // Offer checkpoints only when layout discovery and resume_source agree on the
            // final segment; a byte offset in another segment cannot resume this stream.
            let capture = segments.last() == Some(&primary);
            stream_context(
                s,
                &segments,
                &enrich,
                opts,
                ContextState::default(),
                ContextStart {
                    at: None,
                    capture,
                    skipped: 0,
                },
                sink,
            )
        })
    }

    fn resumed(
        &self,
        r: &SessionRef,
        opts: &Demand,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let Some((state, skipped)) = jsonl::restore_at::<ContextCheckpoint>(from, *opts) else {
                return Ok(None);
            };
            let primary = self.resume_source(r);
            if state.primary != primary {
                return Ok(None);
            }
            let (transcript, wire) = locate_transcript(&r.path, &r.id);
            let segments = transcript_segments(&r.path, &transcript);
            if transcript != primary || state.segments != segments {
                return Ok(None);
            }
            if fs::metadata(&primary)?.len() < from.offset {
                return Ok(None);
            }
            note_config_dependency(self, sink);
            let (s, enrich) = session_header(self, r, opts, &segments, wire.as_deref());
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Kimi, sink);
            spawns.meta(&s);
            Ok(Some(stream_context(
                s,
                &segments,
                &enrich,
                opts,
                state.ctx,
                ContextStart {
                    at: Some(from.offset),
                    capture: true,
                    skipped,
                },
                &mut spawns,
            )?))
        })
    }
}

fn note_config_dependency(adapter: &Kimi, sink: &mut dyn MessageSink) {
    if let Some(root) = &adapter.root {
        sink.dependency(&root.join("config.toml"));
    }
}

fn session_header(
    adapter: &Kimi,
    r: &SessionRef,
    opts: &Demand,
    segments: &[PathBuf],
    wire: Option<&Path>,
) -> (Session, WireEnrich) {
    let mut s = Session {
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: adapter
            .root
            .as_ref()
            .and_then(|root| default_model_from_config(&root.join("config.toml"))),
        source_path: Some(r.path.clone()),
        lineage: subagent_lineage(&r.path),
        ..Session::new(r.id.clone(), Harness::Kimi)
    };
    if let Some(meta) = subagent_meta(&r.path) {
        s.harness_extra_mut(Harness::Kimi).insert("subagent".into(), meta);
    }

    // Sidecar enrichment from wire.jsonl (tolerant: absent/empty/malformed ⇒ empty). The
    // per-message `tool_results` map (keyed by tool_call_id) is a per-line enrichment; the
    // positional passes (StatusUpdate usage/message_id zipped onto assistant turns, the first
    // wire timestamp stamped onto the first user turn) only ever walk *forward* with O(1)
    // state, so they run inline here too — `parse()` is exactly `collect(stream())`.
    let enrich = wire
        .as_ref()
        .map(|p| read_wire_enrichment(p, opts.spans))
        .unwrap_or_default();

    // Rotated segments (context_1.jsonl…) are read first, in order; their count stays as
    // bookkeeping.
    let seg_count = segments.len().saturating_sub(1) as u64;
    if seg_count > 0 {
        s.harness_extra_mut(Harness::Kimi)
            .insert("compaction_segments".into(), Value::Number(seg_count.into()));
    }
    // The system prompt is the first `context.jsonl` record; peek so the session-level copy is
    // known before the body streams.
    s.system_prompt = peek_system_prompt(&segments[0]);

    if let Some(mode) = approval_mode(&r.path) {
        crate::affordance::note_mode(s.harness_extra_mut(Harness::Kimi), &mode);
    }

    (s, enrich)
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ContextState {
    call_names: BTreeMap<String, String>,
    status_index: usize,
    first_user_seen: bool,
    checkpoints: u64,
    rotated: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct ContextCheckpoint<P = PathBuf, V = Vec<PathBuf>, C = ContextState> {
    primary: P,
    segments: V,
    ctx: C,
}

struct ContextStart {
    at: Option<u64>,
    capture: bool,
    /// Rejected lines before `at` (a checkpoint's count).
    skipped: u64,
}

fn stream_context(
    mut s: Session,
    segments: &[PathBuf],
    enrich: &WireEnrich,
    opts: &Demand,
    mut ctx: ContextState,
    start: ContextStart,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let mut pending: Option<(Message, JsonSource)> = None;
    let mut stopped = false;
    let prior_skipped = start.skipped;
    let mut accept = |view: JsonView<'_>,
                      input: &jsonl::Read,
                      starts: Option<&str>,
                      position: Option<u64>,
                      skipped: u64,
                      sink: &mut dyn MessageSink|
     -> Result<Flow> {
        if let Some(name) = starts {
            ctx.rotated = Some(name.into());
        }
        let Some(record) = record::Record::of(view)? else {
            return Ok(Flow::Continue);
        };
        let role = record.role.as_deref().unwrap_or_default();
        let checkpoint_record = u64::from(role == "_checkpoint");
        ctx.checkpoints += checkpoint_record;
        if role == "_usage" {
            if let Some((p, _)) = pending.as_mut() {
                record::fold_usage(p, view)?;
            }
            return Ok(Flow::Continue);
        }
        // Interpret without applying the current record's call-name/positional state. The anchor
        // below replays this record and must retain only the immutable prefix's state.
        let message = record::message(
            &record,
            opts.spans.then_some(input.source()),
            enrich,
            opts.tier == Tier::Complete,
            &ctx.call_names,
        )?;
        let Some(mut m) = message else {
            return Ok(Flow::Continue);
        };
        if let Some((prev, source)) = pending.take() {
            // The pending message may come from an older rotated segment: its own source is
            // rechecked after a slow callback; `visit_segments` owns the current descriptor.
            let started = std::time::Instant::now();
            let flow = sink.message(prev);
            source.check_after(started)?;
            if flow == Flow::Stop {
                stopped = true;
                return Ok(Flow::Stop);
            }
            if position.is_some_and(|offset| start.capture && sink.wants_checkpoint(offset)) {
                // The current checkpoint carrier is not part of the prefix yet.
                ctx.checkpoints -= checkpoint_record;
                let mut cp = Checkpoint::new(position.unwrap_or_default());
                let state = ContextCheckpoint {
                    primary: segments.last().unwrap(),
                    segments,
                    ctx: &ctx,
                };
                jsonl::stamp(&mut cp, *opts, prior_skipped + skipped, &state);
                sink.checkpoint(&mut cp);
                ctx.checkpoints += checkpoint_record;
            }
        }
        note_call_names(&m, &mut ctx.call_names);
        match m.role {
            Role::Assistant => {
                if let Some(st) = enrich.statuses.get(ctx.status_index) {
                    if m.usage.is_none() {
                        m.usage = st.usage.clone();
                    }
                    if m.id.is_none() {
                        m.id = st.message_id.clone();
                    }
                }
                ctx.status_index += 1;
            }
            Role::User if !ctx.first_user_seen => {
                ctx.first_user_seen = true;
                if m.timestamp.is_none() {
                    m.timestamp = enrich.first_timestamp;
                }
            }
            _ => {}
        }
        if m.kind == MessageKind::CompactionSummary {
            let flow = sink.message(compaction_boundary(m.timestamp));
            if flow == Flow::Stop {
                stopped = true;
                return Ok(flow);
            }
        }
        if let Some(segment) = ctx.rotated.take() {
            if m.kind != MessageKind::CompactionSummary {
                let flow = sink.message(rotation_branch(m.timestamp, segment));
                if flow == Flow::Stop {
                    stopped = true;
                    return Ok(flow);
                }
            }
        }
        pending = Some((m, input.source().clone()));
        Ok(Flow::Continue)
    };
    let skipped = visit_segments(segments, start.at, sink, &mut accept)?;
    if !stopped {
        if let Some((last, source)) = pending {
            sink.message(last);
            source.check()?;
        }
    }
    if let Some(input) = &enrich.file {
        input.check()?;
    }
    if ctx.checkpoints > 0 {
        crate::affordance::note_checkpoints(s.harness_extra_mut(Harness::Kimi), ctx.checkpoints, []);
    }
    super::Unparsed(prior_skipped + skipped).note(&mut s);
    Ok(s)
}

/// The approval mode `state.json` records (`approval{yolo, afk, auto_approve_actions}`): `yolo`
/// and `afk` approve everything, otherwise kimi-cli asks, except for the listed actions.
fn approval_mode(dir: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join("state.json")).ok()?).ok()?;
    let a = v.get("approval")?;
    let on = |k: &str| a.get(k).and_then(Value::as_bool) == Some(true);
    let auto: Vec<&str> = a
        .get("auto_approve_actions")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Some(match () {
        _ if on("yolo") => "yolo".to_string(),
        _ if on("afk") => "afk".to_string(),
        _ if !auto.is_empty() => format!("ask (auto: {})", auto.join(", ")),
        _ => "ask".to_string(),
    })
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

/// The top-level model follows TOML string/comment/table rules. The workspace already includes
/// the parser; typed decoding leaves unrelated configuration out of the adapter's state.
fn default_model_from_config(config_toml: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Config {
        default_model: Option<String>,
    }
    let text = fs::read_to_string(config_toml).ok()?;
    toml::from_str::<Config>(&text)
        .ok()?
        .default_model
        .filter(|model| !model.is_empty())
}

/// Cheap scan of one transcript into a `SessionRef`. `dir` is the session's source dir (the
/// `<uuid>/` dir for modern, the hash dir for legacy). `Ok(None)` when the transcript is not a
/// file; `Err` when it cannot be read.
#[cfg(test)]
static SCANNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn scan(id: &str, transcript: &Path, dir: &Path, cwd: Option<PathBuf>) -> Result<Option<SessionRef>> {
    #[cfg(test)]
    SCANNED.lock().unwrap().push(transcript.to_path_buf());
    let meta = fs::metadata(transcript)?;
    if !meta.is_file() {
        return Ok(None);
    }
    let Some(primary) = crate::source::FileId::of(&meta) else {
        return Ok(None);
    };
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = read_custom_title(dir);
    if title.is_none() {
        let segments = transcript_segments(dir, transcript);
        visit_segments(
            &segments,
            None,
            &mut crate::stream::Discard,
            &mut |view, _, _, _, _, _| {
                if let Some(record) = record::Record::of(view)? {
                    title = record.title()?;
                }
                Ok(if title.is_some() { Flow::Stop } else { Flow::Continue })
            },
        )?;
    } else {
        // A summary title avoids body reads, while the primary path must remain present.
        if crate::source::FileId::of(&fs::metadata(transcript)?) != Some(primary) {
            return Ok(None);
        }
    }

    // wire gives nicer first/last timestamps when present.
    let wire = wire_path_for(dir, transcript);
    let (created, updated) = wire
        .as_ref()
        .and_then(|w| wire_time_bounds(w))
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((mtime, mtime));

    Ok(Some(SessionRef {
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count: None,
        ..SessionRef::new(id.to_string(), Harness::Kimi, dir.to_path_buf())
    }))
}

/// `state.json.custom_title`, when present and non-empty.
fn read_custom_title(dir: &Path) -> Option<String> {
    let path = dir.join("state.json");
    let source = crate::json::JsonSource::capture(&path).ok()?;
    source
        .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
            super::wire::text_title(
                view.get("custom_title")?
                    .filter(|value| value.is_string().unwrap_or(false)),
            )
        })
        .ok()
        .flatten()
}

/// The one answer to "is this session dir modern (dir form)": its live `context.jsonl` is a file.
/// Legacy flat session dirs answer false. (`resume_source` asks the layout by ancestry instead,
/// deliberately, so a resume path never depends on a mutable `exists()`.)
fn is_modern(dir: &Path) -> bool {
    dir.join("context.jsonl").is_file()
}

/// The wire sidecar of a modern session dir, when present.
fn wire_sidecar(dir: &Path) -> Option<PathBuf> {
    let wire = dir.join("wire.jsonl");
    wire.exists().then_some(wire)
}

/// The store-layout rule, name form: the MODERN transcript is the live `context.jsonl` in its
/// session dir; a LEGACY flat session is `<id>.jsonl` directly in the hash dir. This predicate
/// needs no `exists()` probe, so every caller that already holds the transcript path shares it
/// (`locate_transcript` is the exists-based form, for callers that must find the file).
fn is_modern_transcript(transcript: &Path) -> bool {
    transcript.file_name().and_then(|n| n.to_str()) == Some("context.jsonl")
}

/// Given a `SessionRef.path` and id, find the transcript + optional wire sidecar.
fn locate_transcript(dir: &Path, id: &str) -> (PathBuf, Option<PathBuf>) {
    if is_modern(dir) {
        return (dir.join("context.jsonl"), wire_sidecar(dir));
    }
    // Legacy flat: dir is the hash dir, transcript is <id>.jsonl, no wire.
    let flat = dir.join(format!("{id}.jsonl"));
    (flat, None)
}

/// Locate the wire sidecar for a transcript, if it lives next to it (modern only).
fn wire_path_for(dir: &Path, transcript: &Path) -> Option<PathBuf> {
    if is_modern_transcript(transcript) {
        return wire_sidecar(dir);
    }
    None
}

/// The transcript's segments, oldest first: the rotated-out `context_1.jsonl…context_N.jsonl`
/// (kimi-cli's `next_available_rotation`, numbered upward) and then the live `context.jsonl`. A
/// legacy flat transcript has no segments of its own.
fn transcript_segments(dir: &Path, transcript: &Path) -> Vec<PathBuf> {
    let mut rotated: Vec<(u64, PathBuf)> = Vec::new();
    if is_modern_transcript(transcript) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = e.file_name();
                let Some(n) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("context_"))
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .and_then(|n| n.parse::<u64>().ok())
                else {
                    continue;
                };
                rotated.push((n, e.path()));
            }
        }
    }
    rotated.sort();
    let mut v: Vec<PathBuf> = rotated.into_iter().map(|(_, p)| p).collect();
    v.push(transcript.to_path_buf());
    v
}

/// A record visitor of [`visit_segments`]: the record, its segment's source, the rotated-in
/// segment's name, a checkpoint anchor, the rejected lines before it, and the guarded sink.
type SegmentRecord<'a> =
    dyn FnMut(JsonView<'_>, &jsonl::Read, Option<&str>, Option<u64>, u64, &mut dyn MessageSink) -> Result<Flow> + 'a;

/// Every record of every segment, in order: the live one from `at` alone when resuming. kimi-cli
/// rotates the live file on `/clear`, on a compaction (`Context.clear`) and on a rewind
/// (`Context.revert_to`, which copies the kept prefix into the new file), and every new file
/// starts with the system prompt again; so the leading lines a segment shares with the one before
/// it are the same records and are not repeated. `accept` gets each record with its segment's
/// source, the segment's name at its first new record (never for the first segment), the start of
/// a finished live-file line (a checkpoint anchor; a partial last line is replayed from an older
/// one) and the rejected lines before it. Returns the count of rejected lines.
fn visit_segments(
    segments: &[PathBuf],
    at: Option<u64>,
    sink: &mut dyn MessageSink,
    accept: &mut SegmentRecord<'_>,
) -> Result<u64> {
    let mut skipped = 0;
    let mut prev = Vec::new();
    for (index, path) in segments.iter().enumerate() {
        let live = index + 1 == segments.len();
        if at.is_some() && !live {
            continue;
        }
        let input = match jsonl::Read::open(path) {
            Ok(input) => input,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        let source = input.source();
        let from = if live { at.unwrap_or(0) } else { 0 };
        anyhow::ensure!(from <= source.len(), "context offset beyond source");
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let mut cur = Vec::new();
        let mut line_index = 0;
        let mut shared = at.is_none() && index > 0;
        let mut announce = shared;
        let mut start = from;
        let mut stopped = false;
        // Each segment's callbacks are guarded against its own descriptor ([`crate::stream::Guard`]).
        crate::stream::guarded(sink, &input, |guard| {
            input.lines(from, |line| {
                let line_start = std::mem::replace(&mut start, line.end);
                // Older segments need fingerprints for their successor; the live one only until its
                // copied prefix diverges.
                if !live || shared {
                    let Some(hash) = fingerprint(&input, &line)? else {
                        skipped += 1;
                        return Ok(Flow::Continue);
                    };
                    if !live {
                        cur.push(hash);
                    }
                    let matches = shared && prev.get(line_index) == Some(&hash);
                    line_index += 1;
                    if matches {
                        return Ok(Flow::Continue);
                    }
                    shared = false;
                }
                let Some(view) = line.view else {
                    skipped += u64::from(!line.blank);
                    return Ok(Flow::Continue);
                };
                let starts = announce.then_some(name);
                announce = false;
                let position = (live && line.complete).then_some(line_start);
                let flow = accept(view, &input, starts, position, skipped, guard)?;
                stopped = flow == Flow::Stop || guard.failed();
                Ok(if stopped { Flow::Stop } else { flow })
            })
        })?;
        if stopped {
            break;
        }
        prev = cur;
    }
    Ok(skipped)
}

/// What a segment's line is matched against its predecessor's by: a hash of its ASCII-trimmed
/// bytes, as `str::hash` writes them. `None` for a non-UTF-8 line, which takes no place in the
/// shared prefix. Only rotated segments and a live file's copied prefix are fingerprinted.
fn fingerprint(file: &impl jsonl::ranges::At, line: &jsonl::Line<'_>) -> Result<Option<u64>> {
    use std::hash::Hasher;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    match line.view {
        Some(view) => {
            let (mut offset, len) = view.source_range()?.context("JSONL record has a source")?;
            let end = offset + len;
            let mut bytes = vec![0; len.min(64 * 1024) as usize];
            while offset < end {
                let n = (end - offset).min(bytes.len() as u64) as usize;
                jsonl::ranges::read_exact_at(file, &mut bytes[..n], offset)?;
                hash.write(&bytes[..n]);
                offset += n as u64;
            }
        }
        None if line.blank => {}
        None => {
            let range = jsonl::ranges::Range {
                offset: line.at,
                len: line.end - line.at,
            };
            if !jsonl::ranges::valid_utf8(&mut jsonl::ranges::reader(file, range)?)? {
                return Ok(None);
            }
            // Match trim_ascii_end without copying a giant rejected record.
            let mut end = line.end;
            let mut bytes = vec![0; range.len.min(64 * 1024) as usize];
            while end > line.at {
                let n = (end - line.at).min(bytes.len() as u64) as usize;
                jsonl::ranges::read_exact_at(file, &mut bytes[..n], end - n as u64)?;
                let trimmed = bytes[..n].trim_ascii_end().len();
                end -= (n - trimmed) as u64;
                if trimmed != 0 {
                    break;
                }
            }
            let mut at = line.at;
            while at < end {
                let n = (end - at).min(bytes.len() as u64) as usize;
                jsonl::ranges::read_exact_at(file, &mut bytes[..n], at)?;
                hash.write(&bytes[..n]);
                at += n as u64;
            }
        }
    }
    hash.write_u8(0xff);
    Ok(Some(hash.finish()))
}

/// The lineage of a sub-agent transcript (`<session>/subagents/<agent_id>`, kimi-cli's
/// `SubagentStore`): the owning session and the agent id. Empty for a session's own dir.
fn subagent_lineage(path: &Path) -> crate::ir::Lineage {
    let mut l = crate::ir::Lineage::default();
    let parent = path.parent();
    if parent.and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("subagents") {
        l.parent = parent
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
        l.agent_path = path.file_name().and_then(|n| n.to_str()).map(str::to_string);
    }
    l
}

/// A sub-agent's `meta.json` (`{agent_id, subagent_type, status, description, launch_spec, …}`).
fn subagent_meta(path: &Path) -> Option<Value> {
    subagent_lineage(path).parent?;
    serde_json::from_str(&fs::read_to_string(path.join("meta.json")).ok()?).ok()
}

/// One [`SessionRef`] per sub-agent of a discovered session (`<session>/subagents/<agent_id>/`
/// with a `context.jsonl`). Discovery lists only top-level sessions; parsing one of these yields a
/// [`Session`] whose `lineage.parent` is the session id and `lineage.agent_path` the agent id.
pub fn subagent_refs(main: &SessionRef) -> Result<Vec<SessionRef>> {
    let _cache_scope = crate::discover_cache::read::Context::current().enter();
    let mut out = Vec::new();
    for e in super::side_entries(&main.path.join("subagents"))? {
        let p = e.path();
        let Some(agent) = p.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let transcript = p.join("context.jsonl");
        let mut reads = crate::source::ReadSet::default();
        reads.record(&p);
        for path in [p.join("state.json"), p.join("meta.json"), p.join("wire.jsonl")] {
            reads.record(&path);
        }
        for segment in transcript_segments(&p, &transcript) {
            reads.record(&segment);
        }
        let scanned = crate::discover_cache::cached_scan_with_reads(&transcript, reads, || {
            let Some(mut r) = scan(&agent, &transcript, &p, None)? else {
                return Ok(None);
            };
            if let Some(d) =
                subagent_meta(&p).and_then(|m| m.get("description").and_then(Value::as_str).map(str::to_string))
            {
                r.title = Some(crate::ir::truncate(&d, crate::projection::Head::TITLE));
            }
            Ok(Some(r))
        });
        let Some(mut r) = (match scanned {
            Ok(r) => r,
            Err(error) if super::is_not_found(&error) => None,
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        // Parent display identity/cwd are supplied by the caller, not by these cached bytes.
        r.id = format!("{}/{agent}", main.id);
        r.cwd.clone_from(&main.cwd);
        out.push(r);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// The synthesized marker for a segment that a `/clear` or a rewind rotated in (a compaction's
/// rotation gets [`compaction_boundary`] from its summary instead).
fn rotation_branch(ts: Option<DateTime<Utc>>, segment: String) -> Message {
    let mut m = Message::synthesized(MessageKind::Branch, Origin::Harness);
    m.timestamp = ts;
    m.harness_extra_mut(Harness::Kimi)
        .insert("segment".into(), Value::String(segment));
    m
}

// ---------------------------------------------------------------------------
// context.jsonl → IR
// ---------------------------------------------------------------------------

/// The system prompt from the head of `context.jsonl` (kimi-cli writes it as the first record,
/// `{"role":"_system_prompt", …}`; `soul/context.py:99`). Bounded to the first few lines.
fn peek_system_prompt(transcript: &Path) -> Option<String> {
    let input = jsonl::Read::open(transcript).ok()?;
    let mut prompt = None;
    let mut seen = 0usize;
    input
        .lines(0, |line| {
            let Some(view) = line.view else {
                return Ok(Flow::Continue);
            };
            seen += 1;
            let Some(record) = record::Record::of(view)? else {
                return Ok(if seen >= 4 { Flow::Stop } else { Flow::Continue });
            };
            if record.role.as_deref() == Some("_system_prompt") {
                let text = record::join(record.fields.get("content"), None)?;
                if !text.is_empty() {
                    prompt = text.inline_str().map(str::to_owned);
                }
                return Ok(Flow::Stop);
            }
            Ok(if seen >= 4 { Flow::Stop } else { Flow::Continue })
        })
        .ok()?;
    prompt
}

fn note_call_names(message: &Message, names: &mut BTreeMap<String, String>) {
    for block in &message.content {
        if let Block::ToolUse { id, name, .. } = block {
            if !id.is_empty() && !name.is_empty() {
                names.insert(id.clone(), name.clone());
            }
        }
    }
}

/// The synthetic boundary emitted before a `CompactionSummary`: kimi-cli's compaction rotates the
/// transcript (`Context.clear`, `kimisoul.py:1577`) and leaves no record of its own, so the
/// summary is the only trace — and the compaction detector pairs a summary with the boundary
/// before it.
fn compaction_boundary(ts: Option<DateTime<Utc>>) -> Message {
    let mut m = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
    m.timestamp = ts;
    m
}

// ---------------------------------------------------------------------------
// wire.jsonl sidecar enrichment
// ---------------------------------------------------------------------------

/// Only navigation survives the wire pass: no output or extras body is retained.
#[derive(Default)]
struct WireEnrich {
    tool_results: HashMap<String, WireToolResult>,
    statuses: Vec<WireStatus>,
    first_timestamp: Option<DateTime<Utc>>,
    file: Option<jsonl::Read>,
    spans: bool,
}

#[derive(Clone, Copy)]
struct WireRange {
    offset: u64,
    len: usize,
}

struct WireToolResult {
    is_error: bool,
    output: Option<WireRange>,
    lazy_output: Option<Text>,
    extras: Option<WireRange>,
}

impl WireEnrich {
    fn read(&self, range: WireRange) -> Option<Vec<u8>> {
        // Keep the original descriptor open: replacing the path during a pass cannot redirect
        // full-mode enrichment to an unrelated file. This reader is used sequentially.
        let file = self.file.as_ref()?;
        let mut bytes = vec![0; range.len];
        jsonl::ranges::read_exact_at(file, &mut bytes, range.offset).ok()?;
        Some(bytes)
    }

    fn output(&self, result: &WireToolResult) -> Text {
        if let Some(text) = &result.lazy_output {
            return text.clone();
        }
        let Some(range) = result.output else {
            return String::new().into();
        };
        self.read(range)
            .and_then(|b| serde_json::from_slice::<String>(&b).ok())
            .unwrap_or_default()
            .into()
    }

    fn extras(&self, result: &WireToolResult) -> Option<Value> {
        self.read(result.extras?).and_then(|b| serde_json::from_slice(&b).ok())
    }
}

#[derive(Default)]
struct WireStatus {
    usage: Option<Usage>,
    message_id: Option<String>,
}

/// Index ToolResult fields by byte range, decoding only usage/identity metadata. A later result
/// for the same call replaces its descriptor, including missing/non-string output and null extras.
/// The scanned descriptor stays open for full-mode reads. An unreadable or changed wire file
/// enriches nothing.
fn read_wire_enrichment(wire: &Path, spans: bool) -> WireEnrich {
    let mut e = WireEnrich {
        spans,
        ..WireEnrich::default()
    };
    match read_wire(wire, &mut e) {
        Ok(file) => {
            e.file = Some(file);
            e
        }
        Err(_) => WireEnrich::default(),
    }
}

fn read_wire(wire: &Path, e: &mut WireEnrich) -> Result<jsonl::Read> {
    let input = jsonl::Read::open(wire)?;
    input.lines(0, |line| {
        if let Some(view) = line.view {
            wire_record(e, view)?;
        }
        Ok(Flow::Continue)
    })?;
    Ok(input)
}

fn wire_record(e: &mut WireEnrich, view: JsonView<'_>) -> Result<()> {
    let Some(v) = Fields::parse(view, &["type", "message", "timestamp"])? else {
        return Ok(());
    };
    if v.string("type")?.as_deref() == Some("metadata") {
        return Ok(());
    }
    let Some(msg) = v.get("message") else { return Ok(()) };
    let Some(msg) = Fields::parse(msg, &["type", "payload"])? else {
        return Ok(());
    };
    let mty = msg.string("type")?.unwrap_or_default();
    if let Some(payload) = msg.get("payload") {
        let p = Fields::parse(payload, &["tool_call_id", "return_value", "token_usage", "message_id"])?;
        let p_string = |key| p.as_ref().map(|p| p.string(key)).transpose().map(Option::flatten);
        match mty.as_str() {
            "ToolResult" => {
                let id = p_string("tool_call_id")?.unwrap_or_default();
                if !id.is_empty() {
                    let rv = match p.as_ref().and_then(|p| p.get("return_value")) {
                        Some(rv) => Fields::parse(rv, &["output", "message", "extras", "is_error"])?,
                        None => None,
                    };
                    let get = |key| rv.as_ref().and_then(|rv| rv.get(key));
                    let string = |key| get(key).filter(|v| v.is_string().unwrap_or(false));
                    let range = |v: JsonView<'_>| -> Result<WireRange> {
                        let (offset, len) = v.source_range()?.context("wire value has a source")?;
                        Ok(WireRange {
                            offset,
                            len: len as usize,
                        })
                    };
                    let output_view = string("output").or_else(|| string("message"));
                    let output = output_view.map(range).transpose()?;
                    // Wire output is auxiliary: the session's primary stamp cannot protect
                    // its offsets. Retain the wire's captured identity with deferred text.
                    let lazy_output = output_view
                        .filter(|_| e.spans && output.is_some_and(|r| r.len.saturating_sub(2) > INLINE_MAX))
                        .map(JsonView::source_text)
                        .transpose()?
                        .flatten();
                    let extras = get("extras")
                        .filter(|v| !v.is_null().unwrap_or(false))
                        .map(range)
                        .transpose()?;
                    let is_error = get("is_error").map(|v| v.as_bool()).transpose()?.flatten() == Some(true);
                    e.tool_results.insert(
                        id,
                        WireToolResult {
                            is_error,
                            output,
                            lazy_output,
                            extras,
                        },
                    );
                }
            }
            "StatusUpdate" => {
                let usage = p
                    .as_ref()
                    .and_then(|p| p.get("token_usage"))
                    .map(wire_usage)
                    .transpose()?;
                let message_id = p_string("message_id")?;
                e.statuses.push(WireStatus { usage, message_id });
            }
            _ => {}
        }
    }
    if matches!(mty.as_str(), "TurnBegin" | "ContentPart" | "ToolCall") && e.first_timestamp.is_none() {
        e.first_timestamp = wire_timestamp(v.get("timestamp"))?;
    }
    Ok(())
}

fn wire_usage(view: JsonView<'_>) -> Result<Usage> {
    let tu = Fields::parse(
        view,
        &["input_other", "output", "input_cache_read", "input_cache_creation"],
    )?;
    let g = |k: &str| -> Result<Option<u64>> {
        Ok(tu
            .as_ref()
            .and_then(|tu| tu.get(k))
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64()))
    };
    Ok(Usage {
        input_tokens: g("input_other")?,
        output_tokens: g("output")?,
        cache_read_tokens: g("input_cache_read")?,
        cache_creation_tokens: g("input_cache_creation")?,
        ..Usage::default()
    })
}

fn wire_timestamp(view: Option<JsonView<'_>>) -> Result<Option<DateTime<Utc>>> {
    Ok(view
        .map(|v| v.as_number())
        .transpose()?
        .flatten()
        .and_then(|n| n.as_f64())
        .map(epoch_to_dt))
}

/// First and last event timestamps in a wire file (epoch floats), for created/updated bounds.
fn wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut first = None;
    let mut last = None;
    jsonl::for_each_record(wire, |view| {
        if let Some(ts) = wire_timestamp(view.get("timestamp")?)? {
            first.get_or_insert(ts);
            last = Some(ts);
        }
        Ok(())
    })
    .ok()?;
    Some((first?, last?))
}

fn epoch_to_dt(secs: f64) -> DateTime<Utc> {
    let nanos = (secs * 1e9).round() as i64;
    DateTime::<Utc>::from_timestamp_nanos(nanos)
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

    for chunk in msg.as_chunks::<64>().0 {
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
    use crate::harness::Parse;
    use serde_json::json;

    /// `kimi.json` is the cwd registry kimi-cli rewrites as work dirs change; a rewrite rescans
    /// no session, and every session takes the cwd the registry now names.
    #[test]
    fn a_registry_rewrite_rescans_no_session() -> Result<()> {
        let world = crate::test_world("kimi_registry_rewrite");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let root = world.home.join(".kimi");
        let registry = |cwd: &str, extra: &str| {
            fs::write(
                root.join("kimi.json"),
                format!("{{\"work_dirs\":[{{\"path\":\"{cwd}\"}}{extra}]}}"),
            )
        };
        let hash = md5_hex(b"/work");
        for i in 0..4 {
            let dir = root
                .join("sessions")
                .join(&hash)
                .join(format!("0000000{i}-0000-4000-8000-000000000000"));
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join("context.jsonl"),
                "{\"role\":\"user\",\"content\":\"hi\"}\n{\"role\":\"assistant\",\"content\":\"hello\"}\n",
            )?;
        }
        registry("/work", "")?;
        let scanned = || {
            let mut all = SCANNED.lock().unwrap();
            let mine = all.iter().filter(|p| p.starts_with(&root)).count();
            all.retain(|p| !p.starts_with(&root));
            mine
        };
        let adapter = Kimi::with_home(&root);
        let first = adapter.discover()?;
        assert_eq!((first.len(), scanned()), (4, 4));
        assert!(first.iter().all(|r| r.cwd.as_deref() == Some(Path::new("/work"))));
        registry("/work", ",{\"path\":\"/elsewhere\"}")?;
        let again = adapter.discover()?;
        assert_eq!((again.len(), scanned()), (4, 0), "a registry rewrite rescans nothing");
        assert!(again.iter().all(|r| r.cwd.as_deref() == Some(Path::new("/work"))));
        registry("/moved", "")?;
        assert!(
            adapter.discover()?.iter().all(|r| r.cwd.is_none()),
            "the registry's current cwd"
        );
        assert_eq!(scanned(), 0);
        Ok(())
    }

    #[test]
    fn custom_title_skips_context_reads_and_fallback_stops_at_first_title() -> Result<()> {
        let world = crate::test_world("kimi-title-demand");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        let live = dir.join("context.jsonl");
        fs::write(
            &live,
            format!(
                "{{\"role\":\"assistant\",\"content\":\"{}\"}}\n{{\"role\":\"user\",\"content\":\"late prompt\"}}\n",
                "x".repeat(1 << 20)
            ),
        )?;
        fs::write(dir.join("state.json"), r#"{"custom_title":"native title"}"#)?;
        fs::write(dir.join("wire.jsonl"), "{\"timestamp\":9}\n{\"timestamp\":2}\n")?;
        jsonl::OPENED.with(|paths| paths.borrow_mut().clear());
        let native = scan("session", &live, &dir, None).unwrap().unwrap();
        assert_eq!(native.title.as_deref(), Some("native title"));
        assert_eq!(
            (native.created_at, native.updated_at),
            (Some(epoch_to_dt(9.0)), Some(epoch_to_dt(2.0)))
        );
        assert!(!jsonl::OPENED.with(|paths| paths.borrow().contains(&live)));
        assert_eq!(native.message_count, None);
        fs::write(dir.join("state.json"), "{}")?;
        assert_eq!(
            scan("session", &live, &dir, None).unwrap().unwrap().title.as_deref(),
            Some("late prompt")
        );
        fs::write(dir.join("state.json"), r#"{"custom_title":"native title"}"#)?;
        fs::remove_file(&live)?;
        assert!(
            !matches!(scan("session", &live, &dir, None), Ok(Some(_))),
            "a summary cannot resurrect an absent primary"
        );
        Ok(())
    }

    #[test]
    fn fingerprints_keep_rejected_utf8_and_ascii_trimming_semantics() -> Result<()> {
        use std::hash::Hasher;
        let world = crate::test_world("kimi-prefix-fingerprint");
        let path = world.base.join("context.jsonl");
        let mut raw = format!("  {{bad:{}\t\r\n", "λ".repeat(100_000)).into_bytes();
        raw.extend_from_slice(b"\xff\n   {\"role\":\"user\",\"content\":\"valid\"}\r\n");
        fs::write(&path, &raw)?;
        let input = jsonl::Read::open(&path)?;
        input.lines(0, |line| {
            let bytes = raw[line.at as usize..line.end as usize].trim_ascii_end();
            let expected = std::str::from_utf8(bytes).ok().map(|_| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                hash.write(bytes);
                hash.write_u8(0xff);
                hash.finish()
            });
            assert_eq!(fingerprint(&input, &line)?, expected);
            Ok(Flow::Continue)
        })?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn callbacks_cannot_resume_mapped_context_after_truncation() -> Result<()> {
        if !crate::harness::store::jsonl::tests::in_child(
            "harness::kimi::tests::callbacks_cannot_resume_mapped_context_after_truncation",
        ) {
            return Ok(());
        }
        struct Shrink {
            path: PathBuf,
            stage: u8,
            hit: bool,
        }
        impl Shrink {
            fn at(&mut self, stage: u8) {
                if self.stage == stage && !self.hit {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&self.path)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                    self.hit = true;
                    // A callback that changed the file is a slow one by construction; the guard
                    // checks the backing after a slow callback, before the mapping is touched.
                    std::thread::sleep(crate::harness::store::jsonl::Read::SLOW_CALLBACK * 10);
                }
            }
        }
        impl MessageSink for Shrink {
            fn message(&mut self, _: Message) -> Flow {
                self.at(0);
                Flow::Continue
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                self.at(1);
                self.stage != 1
            }
            fn checkpoint(&mut self, _: &mut Checkpoint) {
                self.at(2);
            }
        }
        let world = crate::test_world("kimi-callback-source");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        let path = dir.join("context.jsonl");
        let reference = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&world.base);
        for stage in 0..3 {
            fs::write(&path, "{\"role\":\"user\",\"content\":\"owned prompt\"}\n".repeat(1024))?;
            let mut sink = Shrink {
                path: path.clone(),
                stage,
                hit: false,
            };
            assert!(adapter.stream(&reference, &Demand::RICH, &mut sink).is_err());
            assert!(sink.hit, "callback stage {stage}");
        }
        Ok(())
    }

    #[test]
    fn final_pending_message_checks_its_older_segment_and_captured_wire() -> Result<()> {
        let world = crate::test_world("kimi-final-source");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        let old = dir.join("context_1.jsonl");
        let live = dir.join("context.jsonl");
        let wire = dir.join("wire.jsonl");
        let reference = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&world.base);
        for target in [&old, &wire] {
            fs::write(&old, "{\"role\":\"user\",\"content\":\"older pending prompt\"}\n")?;
            fs::write(&live, "")?;
            fs::write(
                &wire,
                "{\"timestamp\":9,\"message\":{\"type\":\"TurnBegin\",\"payload\":{}}}\n",
            )?;
            let mut kept = None;
            let result = adapter.stream(&reference, &Demand::RICH, &mut |m: Message| {
                kept = Some(m);
                fs::remove_file(target).unwrap();
                Flow::Continue
            });
            assert!(result.is_err(), "final callback invalidates {}", target.display());
            assert_eq!(kept.unwrap().text().as_deref(), Some("older pending prompt"));
        }
        // Optional malformed/absent wire remains tolerated before any enrichment is admitted.
        fs::write(&wire, "invalid-json\n")?;
        adapter.stream(&reference, &Demand::RICH, &mut |_: Message| Flow::Continue)?;
        fs::remove_file(&wire)?;
        adapter.stream(&reference, &Demand::RICH, &mut |_: Message| Flow::Continue)?;
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn deferred_counts_publish_for_directory_refs_and_invalidate_on_growth() -> Result<()> {
        let world = crate::test_world("kimi-count-publication");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir)?;
        let path = dir.join("context.jsonl");
        let first = "{\"role\":\"user\",\"content\":\"question\"}\n";
        fs::write(&path, first)?;
        let adapter = Kimi::with_home(&home);
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let r = crate::sessions_for(Some(Harness::Kimi)).remove(0);
        assert_eq!(r.message_count, None);
        let mut sink = crate::stream::CollectSink::default();
        crate::stream::stream_counted(
            &adapter,
            &r,
            &Demand::FACTS.with_spans(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(crate::sessions_for(Some(Harness::Kimi))[0].message_count, Some(1));
        fs::write(&path, format!("{first}{{\"role\":\"user\",\"content\":\"next\"}}\n"))?;
        assert_eq!(crate::sessions_for(Some(Harness::Kimi))[0].message_count, None);
        Ok(())
    }

    #[test]
    fn discovery_refreshes_titles_registry_rotations_and_wire_without_live_append() -> Result<()> {
        let world = crate::test_world("kimi_aux_discovery");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let home = world.home.join(".kimi");
        let cwd = "/synthetic/project";
        let dir = home.join("sessions").join(md5_hex(cwd.as_bytes())).join("session");
        fs::create_dir_all(&dir)?;
        let registry = home.join("kimi.json");
        fs::write(&registry, serde_json::json!({"work_dirs":[{"path":cwd}]}).to_string())?;
        let live = dir.join("context.jsonl");
        let body =
            "{\"role\":\"user\",\"content\":\"prompt fallback\"}\n{\"role\":\"assistant\",\"content\":\"answer\"}\n";
        fs::write(&live, body)?;
        let adapter = Kimi::with_home(&home);
        let first = adapter.discover()?.remove(0);
        assert_eq!(first.title.as_deref(), Some("prompt fallback"));
        assert_eq!(first.message_count, None);
        assert_eq!(first.cwd.as_deref(), Some(Path::new(cwd)));
        fs::write(
            dir.join("state.json"),
            "{\"custom_title\":\"renamed without a new turn\"}",
        )?;
        assert_eq!(
            adapter.discover()?[0].title.as_deref(),
            Some("renamed without a new turn")
        );
        fs::write(dir.join("state.json"), "{}")?;
        assert_eq!(adapter.discover()?[0].title.as_deref(), Some("prompt fallback"));
        fs::write(
            &registry,
            serde_json::json!({"work_dirs":[{"path":cwd,"kaos":"remote"}]}).to_string(),
        )?;
        assert_eq!(adapter.discover()?[0].cwd, None);
        let rotated = dir.join("context_1.jsonl");
        fs::write(&rotated, "{\"role\":\"user\",\"content\":\"earlier\"}\n")?;
        assert_eq!(adapter.discover()?[0].message_count, None);
        fs::write(
            &rotated,
            "{\"role\":\"user\",\"content\":\"earlier\"}\n{\"role\":\"assistant\",\"content\":\"prior reply\"}\n",
        )?;
        assert_eq!(adapter.discover()?[0].message_count, None);
        fs::write(
            dir.join("wire.jsonl"),
            "{\"timestamp\":1700000000}\n{\"timestamp\":1700000001}\n",
        )?;
        let last = adapter.discover()?.remove(0);
        assert_eq!(last.created_at, Some(epoch_to_dt(1700000000.0)));
        assert_eq!(last.updated_at, Some(epoch_to_dt(1700000001.0)));
        assert_eq!(
            fs::read_to_string(live)?,
            body,
            "refresh never needs a transcript append"
        );
        Ok(())
    }

    #[test]
    fn checked_discovery_kimi_matches_owned_segments_titles_and_wire_bounds() -> Result<()> {
        let world = crate::test_world("kimi-discovery-selected");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        let live = dir.join("context.jsonl");
        let body = "body λ\n".repeat(512 * 1024);
        let prompt = serde_json::json!({"role":"user","content":[{"text":""},{"text":body}]}).to_string();
        let prefix = format!(" {prompt}\r\n\ninvalid-json\n");
        let mut rotated = prefix.as_bytes().to_vec();
        rotated.extend_from_slice(b"\xff\n{\"role\":\"assistant\",\"content\":\"old\"}\n");
        fs::write(dir.join("context_1.jsonl"), rotated)?;
        let mut current = prefix.as_bytes().to_vec();
        current.extend_from_slice(b"\xff\n{\"ignored\":1e999}\n{\"role\":\"assistant\",\"content\":\"new\"}\n{\"role\":\"user\",\"content\":\"partial\"}");
        current.extend_from_slice(b"\n{\"role\":\"assistant\",\"role\":\"tool\",\"content\":\"duplicate role\"}\n");
        fs::write(&live, current)?;
        fs::write(
            dir.join("wire.jsonl"),
            format!(
                "{{\"timestamp\":1,\"ignored\":{}}}\n{{\"timestamp\":9}}\n",
                serde_json::to_string(&body)?
            ),
        )?;
        for title in [
            Value::Null,
            serde_json::json!(" custom λ "),
            serde_json::json!("\u{2003}".repeat(8192)),
            serde_json::json!([{"text":"invalid title type"}]),
        ] {
            fs::write(
                dir.join("state.json"),
                serde_json::to_vec(&serde_json::json!({"custom_title":title,"unused":body}))?,
            )?;
            let actual = scan("session", &live, &dir, Some("/p".into())).unwrap().unwrap();
            // The shared prefix (prompt, blank, invalid) is read once; the rejected lines of the
            // live file count, the rotated `\xff` too.
            assert_eq!(actual.message_count, None, "{title}");
            let expected = read_custom_title(&dir).or(crate::harness::wire::text_title(Some(
                (&serde_json::json!([{"text":""},{"text":body}])).into(),
            ))?);
            assert_eq!(actual.title, expected, "{title}");
            assert_eq!(actual.created_at, Some(epoch_to_dt(1.0)));
            assert_eq!(actual.updated_at, Some(epoch_to_dt(9.0)));
        }
        // A non-UTF-8 wire line is one rejected record.
        let mut file = fs::OpenOptions::new().append(true).open(dir.join("wire.jsonl"))?;
        use std::io::Write;
        file.write_all(b"\xff")?;
        assert_eq!(
            wire_time_bounds(&dir.join("wire.jsonl")),
            Some((epoch_to_dt(1.0), epoch_to_dt(9.0)))
        );
        Ok(())
    }

    #[derive(Default)]
    pub(super) struct ResumeKeep {
        pub(super) messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for ResumeKeep {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.push(message);
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), cp.clone()));
        }
    }
    /// A whole stream of `r`, materialized.
    fn whole(adapter: &Kimi, r: &SessionRef, opts: &Demand) -> Session {
        let mut kept = ResumeKeep::default();
        let mut s = adapter.try_stream(r, opts, &mut kept).unwrap();
        s.messages = kept.messages;
        s.materialize().unwrap();
        s
    }

    /// The record codec over an owned value, no spans, noting the record's calls.
    fn context_message(
        v: &Value,
        enrich: &WireEnrich,
        complete: bool,
        names: &mut BTreeMap<String, String>,
    ) -> Option<Message> {
        let record = record::Record::of(v.into()).unwrap()?;
        let m = record::message(&record, None, enrich, complete, names).unwrap()?;
        note_call_names(&m, names);
        Some(m)
    }
    fn materialized(messages: &[Message], r: &SessionRef) -> Value {
        let resolver = crate::Resolver::new(Some(r.path.clone()));
        let mut messages = messages.to_vec();
        for message in &mut messages {
            message.materialize(&resolver).unwrap();
        }
        serde_json::to_value(messages).unwrap()
    }

    #[test]
    fn growing_call_name_checkpoints_preserve_late_and_duplicate_result_suffixes() -> Result<()> {
        let world = crate::test_world("kimi-growing-call-state");
        let dir = world.home.join("session");
        fs::create_dir_all(&dir)?;
        let names = (0..96)
            .map(|index| format!("tool-{index}-{}", "n".repeat(1024)))
            .collect::<Vec<_>>();
        let mut records = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                json!({
                    "role":"assistant","tool_calls":[{"id":format!("call-{index}"),"type":"function",
                        "function":{"name":name,"arguments":"{}"}}]
                })
            })
            .collect::<Vec<_>>();
        records.push(json!({"role":"user","content":"all calls have been recorded"}));
        for index in [0, 48, 95, 0, 999] {
            records.push(json!({"role":"tool","tool_call_id":format!("call-{index}"),"content":"late result"}));
        }
        records.push(json!({"role":"user","content":"done"}));
        fs::write(
            dir.join("context.jsonl"),
            records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
        )?;
        let adapter = Kimi {
            root: None,
            sessions: None,
        };
        let reference = SessionRef::new("session", Harness::Kimi, &dir);
        for options in [Demand::RICH, Demand::LEAN.with_spans()] {
            let mut full = ResumeKeep::default();
            adapter.try_stream(&reference, &options, &mut full)?;
            let last = full.checkpoints.last().unwrap();
            assert_eq!(
                jsonl::restore::<ContextCheckpoint>(&last.1)
                    .unwrap()
                    .ctx
                    .call_names
                    .len(),
                96
            );
            assert!(serde_json::to_vec(&last.1)?.len() > 64 * 1024);
            let result_names = full
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    Block::ToolResult { tool_name, .. } => Some(tool_name.as_deref()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                result_names,
                vec![
                    Some(names[0].as_str()),
                    Some(names[48].as_str()),
                    Some(names[95].as_str()),
                    Some(names[0].as_str()),
                    None
                ]
            );
            for index in [0, 48, 95] {
                let (count, checkpoint) = &full.checkpoints[index];
                let mut suffix = ResumeKeep::default();
                assert!(adapter
                    .transcript(
                        &reference,
                        &options,
                        crate::stream::Start::Checkpoint(checkpoint),
                        &mut suffix
                    )?
                    .is_some());
                assert_eq!(
                    materialized(&suffix.messages, &reference),
                    materialized(&full.messages[*count..], &reference)
                );
            }
            let mut amortized = ResumeKeep::default();
            crate::stream::stream_counted(
                &adapter,
                &reference,
                &options,
                &mut amortized,
                crate::stream::Pass {
                    every: Some(1024),
                    amortized: true,
                    ..Default::default()
                },
            )?;
            assert_eq!(
                materialized(&amortized.messages, &reference),
                materialized(&full.messages, &reference)
            );
            assert!(amortized.checkpoints.len() < full.checkpoints.len() / 2);
            let snapshot_bytes = amortized
                .checkpoints
                .iter()
                .map(|(_, cp)| serde_json::to_vec(cp).unwrap().len())
                .sum::<usize>();
            assert!(snapshot_bytes as u64 <= fs::metadata(dir.join("context.jsonl"))?.len() * 4);
            for (count, checkpoint) in &amortized.checkpoints {
                let mut suffix = ResumeKeep::default();
                assert!(crate::stream::stream_counted(
                    &adapter,
                    &reference,
                    &options,
                    &mut suffix,
                    crate::stream::Pass {
                        from: Some(checkpoint),
                        every: Some(1024),
                        amortized: true,
                        ..Default::default()
                    }
                )
                .is_ok());
                assert_eq!(
                    materialized(&suffix.messages, &reference),
                    materialized(&full.messages[*count..], &reference)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn resume_corpus_replays_usage_rotation_prefixes_and_exact_suffixes() {
        let world = crate::test_world("kimi_resume_corpus");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi");
        let mut files = 0;
        let mut resumes = 0;
        for file in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if file.file_name() != "context.jsonl" {
                continue;
            }
            let dir = file.path().parent().unwrap();
            let id = dir.file_name().unwrap().to_str().unwrap();
            let r = SessionRef::new(id, Harness::Kimi, dir);
            let home = dir
                .ancestors()
                .find(|p| p.file_name().is_some_and(|n| n == ".kimi"))
                .unwrap();
            let adapter = Kimi::with_home(home);
            assert_eq!(adapter.resume_source(&r), file.path());
            for opts in [
                Demand::LEAN.with_spans(),
                Demand::RICH.with_spans(),
                Demand::RICH,
                Demand::COMPLETE,
            ] {
                let mut full = ResumeKeep::default();
                let shell = adapter.try_stream(&r, &opts, &mut full).unwrap();
                let stride = (full.checkpoints.len() / 12).max(1);
                for (i, (count, cp)) in full.checkpoints.iter().enumerate().step_by(stride) {
                    let mut rest = ResumeKeep::default();
                    let suffix = adapter
                        .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut rest)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(suffix).unwrap(),
                        serde_json::to_value(&shell).unwrap(),
                        "{} @{} metadata",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        materialized(&rest.messages, &r),
                        materialized(&full.messages[*count..], &r),
                        "{} @{} suffix",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        rest.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
                        full.checkpoints[i + 1..].iter().map(|(_, cp)| cp).collect::<Vec<_>>()
                    );
                    resumes += 1;
                }
            }
            files += 1;
        }
        assert!(
            files >= 4 && resumes >= 20,
            "native files={files} suffix resumes={resumes}"
        );
        drop(world);
        eprintln!("kimi native files={files} exact live-file suffix resumes={resumes}");
    }

    #[test]
    fn resume_anchors_replay_large_pending_messages_and_usage_without_payload_state() {
        use std::io::Write as _;
        let world = crate::test_world("kimi_pending_anchor");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(home.join("config.toml"), "default_model = \"kimi-test\"\n").unwrap();
        let line = "{\"role\":\"user\",\"content\":\"hello\"}\n";
        let giant = serde_json::json!({"role":"assistant","content":"pending-giant-reply".repeat(100_000),"tool_calls":[{"id":"call","type":"function","function":{"name":"Read","arguments":"{}"}}]});
        let body = format!(
            "{}bad json\n{}\n{{\"role\":\"_usage\",\"token_count\":77}}",
            line.repeat(20_000),
            giant
        );
        fs::write(dir.join("context.jsonl"), &body).unwrap();
        let mut r = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&home);
        let opts = Demand::LEAN.with_spans();
        let mut first = ResumeKeep::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        assert_eq!(cp.offset as usize, line.len() * 20_000 + "bad json\n".len());
        let state = serde_json::to_string(cp).unwrap();
        assert!(state.len() < 1024 && !state.contains("pending-giant-reply"));
        assert_eq!(
            first.messages.last().unwrap().usage.as_ref().unwrap().input_tokens,
            Some(77)
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(adapter.resume_source(&r))
            .unwrap();
        writeln!(file).unwrap();
        file.write_all(line.repeat(200).as_bytes()).unwrap();
        drop(file);
        r.title = Some("new catalog title".into());
        let expected = whole(&adapter, &r, &opts);
        let mut suffix = ResumeKeep::default();
        let mut shell = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        assert_eq!(suffix.messages.len(), 201);
        assert_eq!(shell.title, r.title);
        assert_eq!(shell.model.as_deref(), Some("kimi-test"));
        shell.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        shell.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // Adding a rotation changes the dedup/branch context and declines before callbacks.
        fs::write(dir.join("context_1.jsonl"), line).unwrap();
        let mut none = ResumeKeep::default();
        assert!(adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut none)
            .unwrap()
            .is_none());
        assert!(none.messages.is_empty());
        eprintln!(
            "kimi stable_prefix=20000 appended_callbacks=201 whole_callbacks=20201 checkpoint_bytes={}",
            state.len()
        );
    }

    #[test]
    fn resume_rotated_prefix_branch_and_partial_eof_match_old_acceptance() {
        use std::io::Write as _;
        let world = crate::test_world("kimi_rotated_anchor");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir).unwrap();
        let old = concat!(
            "{\"role\":\"_system_prompt\",\"content\":\"system\"}\n",
            "{\"role\":\"user\",\"content\":\"old\"}\n",
            "{\"role\":\"_usage\",\"token_count\":1}\n"
        );
        fs::write(dir.join("context_1.jsonl"), old).unwrap();
        let body = format!("{old}{{\"role\":\"user\",\"content\":\"new\"}}\r\n{{\"role\":\"_checkpoint\",\"id\":\"point\"}}\n{{\"role\":\"assistant\",\"content\":\"answer\"}}\n{{\"role\":\"_usage\",\"token_count\":9");
        fs::write(dir.join("context.jsonl"), body).unwrap();
        let r = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&home);
        for opts in [Demand::LEAN.with_spans(), Demand::COMPLETE] {
            let mut first = ResumeKeep::default();
            let mut full = adapter.try_stream(&r, &opts, &mut first).unwrap();
            // The shared prefix is read once: system prompt, old turn, new turn, answer, and a
            // branch marker for the rotation (plus the checkpoint carrier in complete).
            assert_eq!(first.messages.len(), if opts.tier == Tier::Complete { 6 } else { 5 });
            for (count, cp) in &first.checkpoints {
                let mut suffix = ResumeKeep::default();
                let shell = adapter
                    .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
                    .unwrap()
                    .unwrap();
                full.messages.clear();
                assert_eq!(
                    serde_json::to_value(shell).unwrap(),
                    serde_json::to_value(&full).unwrap()
                );
                assert_eq!(
                    materialized(&suffix.messages, &r),
                    materialized(&first.messages[*count..], &r)
                );
            }
        }
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(adapter.resume_source(&r))
                .unwrap(),
            "}}"
        )
        .unwrap();
        let mut bytes = fs::read(adapter.resume_source(&r)).unwrap();
        bytes.extend_from_slice(b"\xff\n{\"role\":\"user\",\"content\":\"tail\"}\n");
        fs::write(adapter.resume_source(&r), bytes).unwrap();
        let opts = Demand::COMPLETE;
        let after = whole(&adapter, &r, &opts);
        assert_eq!(after.messages.last().unwrap().text().as_deref(), Some("tail"));
        assert_eq!(after.cv_extra().unwrap()["skipped_lines"], 1);
        // Unsupported non-native layout still reads faithfully but never offers a wrong-file mark.
        let flat = home.join("sessions/legacy");
        fs::create_dir_all(&flat).unwrap();
        fs::write(
            flat.join("id.jsonl"),
            "{\"role\":\"user\",\"content\":\"one\"}\n{\"role\":\"user\",\"content\":\"two\"}\n",
        )
        .unwrap();
        let legacy = SessionRef::new("id", Harness::Kimi, &flat);
        let mut kept = ResumeKeep::default();
        adapter.try_stream(&legacy, &opts, &mut kept).unwrap();
        assert!(!kept.checkpoints.is_empty());
        assert_eq!(adapter.resume_source(&legacy), flat.join("id.jsonl"));
    }

    #[test]
    fn wire_output_replacement_before_first_demand_is_rejected() {
        let world = cv_testkit::World::new("kimi-wire-replaced");
        let path = world.base.join("wire.jsonl");
        let wire = |letter: &str| {
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"call", "return_value":{"output":letter.repeat(20_000)}
            }}})
            .to_string()
        };
        fs::write(&path, wire("a")).unwrap();
        let e = read_wire_enrichment(&path, true);
        let text = e.output(&e.tool_results["call"]);
        let replacement = world.base.join("replacement");
        fs::write(&replacement, wire("b")).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(text.try_resolve(&crate::Resolver::new(None)).is_err());
    }

    #[test]
    fn plain_wire_output_retains_checked_source() {
        let world = cv_testkit::World::new("kimi-wire-plain");
        let path = world.base.join("wire.jsonl");
        let output = "plain 雪".repeat(2000);
        fs::write(
            &path,
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"plain", "return_value":{"output":output}
            }}})
            .to_string(),
        )
        .unwrap();
        let e = read_wire_enrichment(&path, true);
        let text = e.output(&e.tool_results["plain"]);
        assert!(matches!(&text, Text::Source(source) if source.path() == path));
        let resolver = crate::lazy::Resolver::new(None);
        let resolved = text.try_resolve(&resolver).unwrap();
        assert_eq!(resolved.as_ref(), output);
    }

    #[test]
    fn wire_ranges_match_value_enrichment_oracle() {
        let world = cv_testkit::World::new("kimi-wire-ranges");
        let path = world.base.join("wire.jsonl");
        let giant = "雪\n\"\\".repeat(3000);
        let mut lines = vec![
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"giant", "return_value":{"output":giant, "extras":{"rich": [1, true]}}
            }}}).to_string(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"dup","return_value":{"output":"old","extras":{"old":1}}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"dup","return_value":{"output":"first","outp\u0075t":9,"message":"fallback","is_error":true,"extras":null}}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"bad","return_value":{"output":"must not win"}},"ignored":"\ud800"}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"bad-number","return_value":{"output":"must not win"}},"ignored":1e400}}"#.into(),
            r#"{"type":"metadata","message":{"type":"ToolResult","payload":{"tool_call_id":"header","return_value":{"output":"ignored"}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"empty","return_value":{"output":null,"message":false,"extras":42}}}}"#.into(),
            r#"{"timestamp":1700000000,"message":{"type":"TurnBegin","payload":{}}}"#.into(),
            r#"{"timestamp":1700000001,"message":{"type":"StatusUpdate","payload":{"message_id":"reply","token_usage":null}}}"#.into(),
            r#"{"message":{"type":"StatusUpdate","payload":false}}"#.into(),
        ];
        lines.push(format!(r#"{{"message":{{"type":"ToolResult","payload":{{"tool_call_id":"too-deep","return_value":{{"output":"ignored"}}}},"ignored":{}0{}}}}}"#, "[".repeat(128), "]".repeat(128)));
        let text = format!(" \n{}\ninvalid\n", lines.join("\n"));
        fs::write(&path, &text).unwrap();
        let mut expected = HashMap::new();
        super::super::for_each_json_line_str(&text, |v| {
            if v.get("type").and_then(Value::as_str) != Some("metadata")
                && v.pointer("/message/type").and_then(Value::as_str) == Some("ToolResult")
            {
                let p = &v["message"]["payload"];
                let id = p.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if !id.is_empty() {
                    let rv = &p["return_value"];
                    let output = rv
                        .get("output")
                        .and_then(Value::as_str)
                        .or_else(|| rv.get("message").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    expected.insert(
                        id.to_string(),
                        (
                            output,
                            rv.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            rv.get("extras").filter(|x| !x.is_null()).cloned(),
                        ),
                    );
                }
            }
            Flow::Continue
        });
        for spans in [false, true] {
            let e = read_wire_enrichment(&path, spans);
            assert_eq!(e.tool_results.len(), expected.len());
            let resolver = crate::lazy::Resolver::new(None);
            for (id, (output, is_error, extras)) in &expected {
                let result = &e.tool_results[id];
                let actual = e.output(result);
                assert_eq!(actual.try_resolve(&resolver).unwrap().as_ref(), output, "{id}");
                assert_eq!(result.is_error, *is_error);
                assert_eq!(e.extras(result), *extras);
                assert_eq!(matches!(actual, Text::Source(_)), spans && id == "giant");
                if let Text::Source(source) = actual {
                    assert_eq!(source.path(), path);
                }
            }
            assert_eq!(e.statuses.len(), 2);
            assert_eq!(e.statuses[0].message_id.as_deref(), Some("reply"));
            assert!(
                e.statuses[0].usage.is_some(),
                "present null token_usage is default Usage"
            );
            assert!(e.statuses[1].usage.is_none());
            assert_eq!(e.first_timestamp, Some(epoch_to_dt(1700000000.)));
        }
        assert_eq!(
            wire_time_bounds(&path),
            Some((epoch_to_dt(1700000000.), epoch_to_dt(1700000001.)))
        );
        // A non-UTF-8 line is one rejected record; the enrichment around it stands.
        let mut invalid = text.into_bytes();
        invalid.extend_from_slice(b"\xff\n");
        fs::write(&path, invalid).unwrap();
        assert_eq!(read_wire_enrichment(&path, false).tool_results.len(), expected.len());
        assert!(wire_time_bounds(&path).is_some());
    }

    #[test]
    fn wire_spans_preserve_all_stream_modes() {
        let world = crate::test_world("kimi-wire-modes");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("context.jsonl"),
            r#"{"role":"_checkpoint","id":0}
{"role":"user","content":"question"}
{"role":"assistant","content":"answer"}
{"role":"tool","tool_call_id":"selected","content":"fallback"}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("wire.jsonl"),
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"selected","return_value":{"output":"雪\n\"\\".repeat(3000),"extras":{"k":"v"}}
            }}})
            .to_string(),
        )
        .unwrap();
        let adapter = Kimi::with_home(&world.base);
        let r = SessionRef::new("session", Harness::Kimi, dir);
        let mut lean = None;
        for opts in [
            Demand::RICH,
            Demand::LEAN,
            Demand::LEAN.with_spans(),
            Demand::RICH.with_spans(),
            Demand::COMPLETE,
            Demand {
                spans: true,
                ..Demand::COMPLETE
            },
        ] {
            let mut sink = crate::stream::CollectSink::default();
            let mut session = adapter.stream(&r, &opts, &mut sink).unwrap();
            session.messages = sink.messages;
            assert_eq!(session.messages.len(), if opts.tier == Tier::Complete { 4 } else { 3 });
            let tool = session.messages.last().unwrap();
            let Block::ToolResult { content, .. } = &tool.content[0] else {
                panic!("tool result")
            };
            assert_eq!(content.is_lazy(), opts.spans);
            session.materialize().unwrap();
            session.messages.retain(|m| m.kind != MessageKind::Carrier);
            let value = serde_json::to_value(session).unwrap();
            if let Some(expected) = &lean {
                assert_eq!(&value, expected);
            } else {
                lean = Some(value);
            }
        }
    }

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
        let (_world, dir) = write_temp("kimi.json", json);
        let map = load_work_dirs(&dir.join("kimi.json"));
        assert_eq!(
            map.get("a917b40bc491ae8a7f6f508be5446785").unwrap(),
            &PathBuf::from("/Users/ember/elide/breadstuffs")
        );
        // non-local KAOS → "<kaos>_<md5>"
        let h = md5_hex(b"/remote/proj");
        assert_eq!(map.get(&format!("box_{h}")).unwrap(), &PathBuf::from("/remote/proj"));
    }

    #[test]
    fn model_config_uses_top_level_toml_semantics_in_session_headers() -> Result<()> {
        let world = crate::test_world("kimi_toml_model");
        let home = world.home.join(".kimi");
        let session = home.join("sessions/hash/session");
        fs::create_dir_all(&session)?;
        fs::write(
            session.join("context.jsonl"),
            "{\"role\":\"user\",\"content\":\"prompt\"}\n",
        )?;
        let config = home.join("config.toml");
        let adapter = Kimi::with_home(&home);
        let reference = SessionRef::new("session", Harness::Kimi, &session);
        for (text, expected) in [
            (
                "default_model_provider = 'unused'\ndefault_model = 'actual' # comment\n",
                Some("actual"),
            ),
            ("\"default_model\" = \"escaped\\u002fmodel\"\n", Some("escaped/model")),
            ("[profiles.demo]\ndefault_model = 'nested'\n", None),
            ("default_model = ''\n", None),
            ("default_model = 42\n", None),
            ("default_model = 'before'\ninvalid TOML\n", None),
        ] {
            fs::write(&config, text)?;
            assert_eq!(default_model_from_config(&config).as_deref(), expected);
            let mut sink = |_: Message| Flow::Continue;
            assert_eq!(
                adapter.stream(&reference, &Demand::RICH, &mut sink)?.model.as_deref(),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn config_default_model() {
        let toml = "theme = \"dark\"\ndefault_model = \"kimi-code/kimi-for-coding\"\nfoo = 1\n";
        let (_world, dir) = write_temp("config.toml", toml);
        assert_eq!(
            default_model_from_config(&dir.join("config.toml")).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
    }

    #[test]
    fn user_bare_string_and_list() {
        let bare: Value = serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        let m = context_message(&bare, &WireEnrich::default(), false, &mut BTreeMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let list: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA","id":null}}]}"#,
        )
        .unwrap();
        let m = context_message(&list, &WireEnrich::default(), false, &mut BTreeMap::new()).unwrap();
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
        let mut call_names = BTreeMap::new();
        let m = context_message(&v, &WireEnrich::default(), false, &mut call_names).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!((m.kind, m.origin), (MessageKind::Reply, Origin::Model));
        assert_eq!(call_names.get("tool_1").map(String::as_str), Some("Shell"));
        match &m.content[0] {
            Block::Thinking {
                text,
                encrypted,
                signature,
                ..
            } => {
                assert_eq!(text.inline_str(), Some("reasoning"));
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(signature.as_deref(), Some("BLOB"));
            }
            o => panic!("expected thinking, got {o:?}"),
        }
        assert!(matches!(&m.content[1], Block::Text { text , .. } if text.matches_str("Let me look.") == Some(true)));
        match &m.content[2] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Shell");
                assert_eq!(input.resolve().unwrap()["command"], "cat .gitmodules");
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
        let mut call_names = BTreeMap::from([("tool_1".to_string(), "Shell".to_string())]);
        let m = context_message(&v, &WireEnrich::default(), false, &mut call_names).unwrap();
        assert_eq!(m.role, Role::Tool);
        assert_eq!((m.kind, m.origin), (MessageKind::ToolResult, Origin::Harness));
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                status,
                tool_name,
                ..
            } => {
                assert_eq!(tool_use_id, "tool_1");
                assert!(content.contains("Command executed successfully"));
                assert!(!is_error);
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(tool_name.as_deref(), Some("Shell"), "resolved from the call");
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        // With wire enrichment, output/is_error come from return_value.
        let temp = cv_testkit::World::new("kimi-wire-enrich");
        let wire = temp.base.join("wire.jsonl");
        fs::write(&wire, r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"tool_1","return_value":{"is_error":true,"output":"boom","extras":{"k":"v"}}}}}"#).unwrap();
        let enrich = read_wire_enrichment(&wire, false);
        let m = context_message(&v, &enrich, false, &mut BTreeMap::new()).unwrap();
        match &m.content[0] {
            Block::ToolResult {
                content,
                is_error,
                status,
                details,
                ..
            } => {
                assert_eq!(content.inline_str(), Some("boom"));
                assert!(is_error);
                assert_eq!(status.as_deref(), Some("error"));
                assert_eq!(details.as_ref().unwrap()["k"], "v");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn internal_roles_skipped() {
        let cp: Value = serde_json::from_str(r#"{"role":"_checkpoint","id":3}"#).unwrap();
        assert!(context_message(&cp, &WireEnrich::default(), false, &mut BTreeMap::new()).is_none());
        // … but under `complete` a checkpoint rides along as a carrier naming its record type
        let c = context_message(&cp, &WireEnrich::default(), true, &mut BTreeMap::new()).unwrap();
        assert_eq!((c.kind, c.origin), (MessageKind::Carrier, Origin::Harness));
        assert_eq!(c.extra[crate::ir::CARRIER_KEY], cp);
        assert_eq!(c.extra["kimi"]["record_type"], "_checkpoint");
        // `_usage` is never a message, even under `complete`: it folds onto the message before it
        let us: Value = serde_json::from_str(r#"{"role":"_usage","token_count":7291}"#).unwrap();
        for complete in [false, true] {
            assert!(context_message(&us, &WireEnrich::default(), complete, &mut BTreeMap::new()).is_none());
        }
        let sp: Value = serde_json::from_str(r#"{"role":"_system_prompt","content":"You are Kimi."}"#).unwrap();
        let m = context_message(&sp, &WireEnrich::default(), false, &mut BTreeMap::new()).unwrap();
        assert_eq!(m.role, Role::System);
        assert_eq!(m.kind, MessageKind::SystemPrompt);
        assert_eq!(m.origin, Origin::Harness);
        assert_eq!(m.text().as_deref(), Some("You are Kimi."));
    }

    #[test]
    fn user_markers_set_kind_and_origin() {
        let cases = [
            (
                r#"{"role":"user","content":"fix the build"}"#,
                MessageKind::Prompt,
                Origin::Human,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"<current_focus>x</current_focus>"}]}"#,
                MessageKind::CompactionSummary,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system-reminder>\nremember\n</system-reminder>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system>CHECKPOINT 3</system>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<notification id=\"n1\" category=\"task\" type=\"task.completed\">\nTitle: done\n</notification>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Scheduler,
            ),
            // a typed prompt that merely mentions a tag mid-text stays a prompt
            (
                r#"{"role":"user","content":"why does <system> appear in the log?"}"#,
                MessageKind::Prompt,
                Origin::Human,
            ),
        ];
        for (line, kind, origin) in cases {
            let v: Value = serde_json::from_str(line).unwrap();
            let m = context_message(&v, &WireEnrich::default(), false, &mut BTreeMap::new()).unwrap();
            assert_eq!(m.role, Role::User, "{line}");
            assert_eq!((m.kind, m.origin), (kind, origin), "{line}");
        }
    }

    /// A rotated (post-compaction) legacy segment, as `Context.clear` + `write_system_prompt` +
    /// `checkpoint` + `append_message(summary)` + `update_token_count` lay it down: the summary user
    /// record gets a synthetic boundary before it; `_usage` folds onto the message before it (as
    /// `usage.input_tokens` on a Reply, as `token_count` on anything).
    #[test]
    fn usage_folds_onto_preceding_message_and_compaction_pairs() {
        let id = "33333333-3333-3333-3333-333333333333";
        let lines = [
            r#"{"role":"_system_prompt","content":"You are Kimi."}"#,
            r#"{"role":"_checkpoint","id":0}"#,
            r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"summary body"}]}"#,
            r#"{"role":"_usage","token_count":900}"#,
            r#"{"role":"_checkpoint","id":1}"#,
            r#"{"role":"user","content":"and now?"}"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"Next."}]}"#,
            r#"{"role":"_usage","token_count":1234}"#,
        ];
        let (_world, dir) = write_temp(&format!("{id}.jsonl"), &(lines.join("\n") + "\n"));
        let r = SessionRef::new(id, Harness::Kimi, dir.clone());
        let adapter = Kimi {
            sessions: None,
            root: None,
        };
        let s = adapter.parse(&r).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Kimi."));
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::SystemPrompt,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Prompt,
                MessageKind::Reply,
            ],
            "{kinds:?}"
        );
        assert!(s.messages[1].is_synthesized() && s.messages[1].content.is_empty());
        assert_eq!(s.messages[2].extra["kimi"]["token_count"], 900);
        assert!(s.messages[2].usage.is_none(), "token_count is usage only on a Reply");
        let reply = &s.messages[4];
        assert_eq!(reply.extra["kimi"]["token_count"], 1234);
        assert_eq!(reply.usage.as_ref().and_then(|u| u.input_tokens), Some(1234));
        // the compaction detector pairs the synthetic boundary with the summary
        let comps = crate::compaction::detect_in_session(&s, true).unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(2));
        assert!(comps[0].summary.as_deref().unwrap_or("").contains("summary body"));
        // every fact is in the harness bag (cv's own on the turn it synthesized); nothing flat
        for m in &s.messages {
            assert!(
                m.extra.keys().all(|k| k == "kimi"
                    || k == crate::ir::CARRIER_KEY
                    || (k == crate::ir::CV_NAMESPACE && m.is_synthesized())),
                "{:?}",
                m.extra
            );
        }
        // complete: the checkpoints ride as carriers, `_usage` still folds (the emitter re-creates it)
        let c = crate::stream::collect_with(&adapter, &r, &Demand::COMPLETE).unwrap();
        let record_types: Vec<&str> = c
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Carrier)
            .filter_map(|m| m.extra["kimi"]["record_type"].as_str())
            .collect();
        assert_eq!(record_types, ["_checkpoint", "_checkpoint"]);
    }

    /// The committed modern fixture: hash dir `aaaaaaaa…`, session uuid `11111111-…`.
    const FIXTURE_MODERN_ID: &str = "11111111-1111-1111-1111-111111111111";
    fn fixture_modern_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join(FIXTURE_MODERN_ID)
    }

    #[test]
    fn stream_matches_parse_on_modern_fixture() {
        // parse() is collect(stream()); the streamed messages must carry the same wire enrichment
        // (StatusUpdate usage/message_id on assistant turns, first wire timestamp on the first user
        // turn) that the full parse reports. This was a real divergence: the positional passes used
        // to run only in parse(), so streaming consumers saw assistant turns without usage.
        let dir = fixture_modern_dir();
        let r = SessionRef {
            cwd: Some(PathBuf::from("/proj")),
            ..SessionRef::new(FIXTURE_MODERN_ID, Harness::Kimi, dir.clone())
        };
        let adapter = Kimi {
            sessions: None,
            root: None,
        };
        let parsed = adapter.parse(&r).unwrap();

        let mut sink = crate::stream::CollectSink::default();
        let mut streamed = adapter.stream(&r, &crate::stream::Demand::RICH, &mut sink).unwrap();
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
        let dir = fixture_modern_dir();
        let r = SessionRef {
            cwd: Some(PathBuf::from("/proj")),
            ..SessionRef::new(FIXTURE_MODERN_ID, Harness::Kimi, dir.clone())
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
        let id = "22222222-2222-2222-2222-222222222222";
        let r = SessionRef::new(id, Harness::Kimi, dir.clone());
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
    }

    /// Write `name`→`contents` into a fresh temp dir, return the dir.
    /// `/compact` and `/clear` rotate `context.jsonl` into `context_N.jsonl`; the whole history is
    /// read, oldest segment first, the repeated head (system prompt, checkpoint 0) once, with a
    /// compaction boundary before a summary and a branch before a cleared segment.
    #[test]
    fn rotated_segments_read_in_order_with_their_boundaries() {
        let sp = r#"{"role":"_system_prompt","content":"You are Kimi."}"#;
        let c0 = r#"{"role":"_checkpoint","id":0}"#;
        let (_world, dir) = write_temp(
            "context_1.jsonl",
            &[
                sp,
                c0,
                r#"{"role":"user","content":"first"}"#,
                r#"{"role":"assistant","content":"one"}"#,
            ]
            .join("\n"),
        );
        fs::write(
            dir.join("context_2.jsonl"),
            [
                sp,
                c0,
                r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"we said one"}]}"#,
                r#"{"role":"user","content":"second"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            dir.join("context.jsonl"),
            [sp, c0, r#"{"role":"user","content":"third"}"#].join("\n"),
        )
        .unwrap();
        let id = "s1";
        let r = scan(id, &dir.join("context.jsonl"), &dir, None).unwrap().unwrap();
        assert_eq!(
            r.title.as_deref(),
            Some("first"),
            "the title is the first prompt of the first segment"
        );
        let k = Kimi {
            sessions: None,
            root: None,
        };
        let s = k.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::SystemPrompt,
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Prompt,
                MessageKind::Branch,
                MessageKind::Prompt,
            ]
        );
        assert_eq!(
            s.messages[6]
                .harness_extra(Harness::Kimi)
                .and_then(|x| x.get("segment")),
            Some(&Value::String("context.jsonl".into()))
        );
    }

    /// kimi-cli's `Agent` tool keeps each sub-agent under `<session>/subagents/<agent_id>/` in the
    /// session's own format; it parses as its own session under the parent.
    #[test]
    fn subagent_discovery_reuses_histories_and_refreshes_auxiliary_and_parent_metadata() -> Result<()> {
        let world = crate::test_world("kimi-subagent-cache");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let parent = world.home.join("parent");
        let mut main = SessionRef::new("parent-id", Harness::Kimi, &parent);
        main.cwd = Some(PathBuf::from("/first"));
        let body = "{\"role\":\"user\",\"content\":\"child task\"}\n".repeat(128);
        for index in 0..12 {
            let child = parent.join("subagents").join(format!("agent-{index:02}"));
            fs::create_dir_all(&child)?;
            fs::write(child.join("context.jsonl"), &body)?;
        }
        let first = subagent_refs(&main).unwrap();
        assert_eq!(first.len(), 12);
        for reference in &first {
            assert_eq!(reference.message_count, None);
            assert_eq!(reference.cwd, main.cwd);
            let hit = crate::discover_cache::cached_scan(&reference.path.join("context.jsonl"), || {
                panic!("unchanged child context must be cached")
            })
            .unwrap();
            assert_eq!(hit.message_count, reference.message_count);
        }
        main.id = "new-parent".into();
        main.cwd = Some(PathBuf::from("/second"));
        let unchanged = subagent_refs(&main).unwrap();
        assert!(unchanged
            .iter()
            .all(|reference| reference.id.starts_with("new-parent/") && reference.cwd == main.cwd));
        let child = &unchanged[0].path;
        fs::write(child.join("meta.json"), r#"{"description":"new task description"}"#)?;
        assert_eq!(
            subagent_refs(&main).unwrap()[0].title.as_deref(),
            Some("new task description")
        );
        fs::write(
            child.join("context_1.jsonl"),
            "{\"role\":\"assistant\",\"content\":\"older\"}\n",
        )?;
        assert_eq!(subagent_refs(&main).unwrap()[0].message_count, None);
        fs::write(
            child.join("wire.jsonl"),
            "{\"timestamp\":1700000000}\n{\"timestamp\":1700000001}\n",
        )?;
        let refreshed = subagent_refs(&main).unwrap();
        assert_eq!(refreshed[0].created_at, Some(epoch_to_dt(1700000000.0)));
        assert_eq!(refreshed[0].updated_at, Some(epoch_to_dt(1700000001.0)));
        assert_eq!(fs::read_to_string(child.join("context.jsonl"))?, body);
        Ok(())
    }

    #[test]
    fn subagent_dirs_parse_with_lineage() {
        let (_world, dir) = write_temp("context.jsonl", r#"{"role":"user","content":"parent"}"#);
        let sub = dir.join("subagents").join("a1b2");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("context.jsonl"), r#"{"role":"user","content":"child task"}"#).unwrap();
        fs::write(
            sub.join("meta.json"),
            r#"{"agent_id":"a1b2","subagent_type":"coder","description":"do it"}"#,
        )
        .unwrap();
        let main = scan("p1", &dir.join("context.jsonl"), &dir, None).unwrap().unwrap();
        let refs = subagent_refs(&main).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "p1/a1b2");
        assert_eq!(refs[0].title.as_deref(), Some("do it"));
        // The forest every consumer asks (`has:subagents`, `agents>=1`, `--agent`) holds them.
        let forest = crate::subagent_tree_of(&main).unwrap();
        assert_eq!(
            forest.iter().map(|a| a.session.id.as_str()).collect::<Vec<_>>(),
            ["p1/a1b2"]
        );
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&refs[0])
        .unwrap();
        assert_eq!(s.lineage.agent_path.as_deref(), Some("a1b2"));
        assert_eq!(s.lineage.parent.as_deref(), dir.file_name().and_then(|n| n.to_str()));
        assert_eq!(
            s.harness_extra(Harness::Kimi)
                .and_then(|x| x.get("subagent").and_then(|a| a.get("subagent_type"))),
            Some(&Value::String("coder".into()))
        );
    }

    /// Write `name`→`contents` into a fresh hermetic world dir, return the dir. The caller keeps
    /// the world scoped; dropping it removes the dir.
    fn write_temp(name: &str, contents: &str) -> (cv_testkit::InProcess<crate::env::Isolated>, PathBuf) {
        let world = crate::test_world("kimi-write-temp");
        fs::write(world.base.join(name), contents).unwrap();
        let dir = world.base.clone();
        (world, dir)
    }
}
