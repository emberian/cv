//! Grok CLI adapter — xAI "Grok Build" CLI, `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/`.
//!
//! Each session is a *directory*. The files we read:
//! - `summary.json` — metadata (`info{id,cwd}`, timestamps, model, git, `session_kind`,
//!   `chat_format_version`, `generated_title`/`session_summary`).
//! - `chat_history.jsonl` — the canonical transcript. Per line `type` ∈
//!   `system | user | assistant | tool_result | reasoning`:
//!     * `system`/`user`: `content` is a string (system) or `[{type:text,text}]` (user). User lines
//!       may carry `synthetic_reason` (e.g. `project_instructions`) for injected context.
//!     * `assistant`: `content` (string), optional `reasoning{text,encrypted,id}`, `model_id`,
//!       `model_fingerprint`, and **`tool_calls[]`** (`{id,name,arguments(JSON string)}`).
//!     * `tool_result`: `{tool_call_id, content}` — the output fed back to the model.
//!     * `reasoning` (1.0.5x): `{id, summary[{type:summary_text,text}], encrypted_content?}`, written
//!       before the assistant line it belongs to; it becomes that message's leading `Thinking` block.
//! - `updates.jsonl` — an ACP-style `session/update` event stream (plus `_x.ai/session/update`).
//!   We mine `tool_call`/`tool_call_update` entries (keyed by `toolCallId`) to *enrich* tool calls
//!   with a human `title`, a `kind`, file `locations`, and a `completed`/`failed` status that lets us
//!   set `ToolResult.is_error` correctly (chat_history alone doesn't mark errors). Its
//!   `hook_execution` updates are the session's hook runs (`hook`/`run` affordance rows).
//! - `events.jsonl` — timing / permission lifecycle (`tool_started`, `permission_*`,
//!   `tool_completed{outcome,duration_ms}`, `turn_*`). Not needed for content; we don't ingest it.
//!
//! Historical / variant handling (sniffed by content, never by a version field):
//! - Tool data is read from `chat_history.jsonl` (canonical, always present). `updates.jsonl` is
//!   strictly additive enrichment — if it's empty or absent we still get full tool fidelity.
//! - Sessions with no tool activity (`events.jsonl` empty, short `chat_history`) parse fine.
//! - `session_kind: "subagent"` (under `.grok/worktrees/.../subagent-<id>/`) vs primary sessions
//!   are both handled: the raw `session_kind`/`agent_name` ride in `extra["grok"]`, and a subagent's
//!   `agent_name` nickname also becomes [`Lineage::agent_path`]. 1.0.5x links them: a fork's
//!   `summary.json` has `parent_session_id` (→ `lineage.forked_from`), and a parent lists each child
//!   under `subagents/<child id>/meta.json` (→ the child's `lineage.parent`).

use crate::json::{ReadInput, Validation};
pub(crate) mod emit;
mod record;
pub use emit::emit;

use super::store::document::{self, Fields};
use super::store::jsonl;
use super::wire::openai;
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::lazy::Text;
use crate::source::{Dependencies, Probe};
use crate::stream::{Demand, Flow, MessageSink, Start};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Grok {
    root: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Grok = Grok { root: None };

impl Grok {
    pub fn detect() -> Self {
        let root = crate::env::home_dir().map(|h| h.join(".grok").join("sessions"));
        Grok {
            root: root.filter(|p| p.exists()),
        }
    }

    /// An adapter over an explicit Grok home (the dir holding `sessions/`, what `$GROK_HOME` names),
    /// instead of `~/.grok` — for the harness corpus and other tests, without touching the environment.
    pub fn with_home(grok_home: &Path) -> Self {
        let root = grok_home.join("sessions");
        Grok {
            root: root.exists().then_some(root),
        }
    }
}

impl Adapter for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn dependencies(&self, r: &SessionRef, probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        deps.files.extend(
            [
                "summary.json",
                "chat_history.jsonl",
                "system_prompt.txt",
                "plan.json",
                "rewind_points.jsonl",
            ]
            .map(|name| r.path.join(name)),
        );
        // Every turn appends to `updates.jsonl`: its growth resumes the mark, and the resumed read
        // declines when the growth revises a tool the prefix emitted ([`HistoryCheckpoint`]).
        deps.resumable.push(r.path.join("updates.jsonl"));
        // Only subagents consult sibling parent metadata. Ordinary sessions never scan a cwd
        // group's siblings merely to establish freshness. Retain the before-read metadata:
        // interpreting kind must not bless a later version of the summary.
        let summary_path = r.path.join("summary.json");
        probe.exists(&summary_path)?;
        let note = |error: &anyhow::Error| {
            crate::env::warn_once(
                "Grok dependencies",
                format_args!("{}: {error:#}", summary_path.display()),
            );
        };
        const SUBAGENT_KIND: &str = "subagent";
        let subagent = JsonSource::capture(&summary_path)
            .inspect_err(note)
            .ok()?
            .read(ReadInput::Document, Validation::Value, |view| {
                let kind = view
                    .get("session_kind")?
                    .map(|kind| crate::projection::string_prefix(kind, SUBAGENT_KIND.len() + 1))
                    .transpose()?
                    .flatten();
                Ok(kind.as_deref() == Some(SUBAGENT_KIND))
            })
            .inspect_err(note)
            .ok()?;
        if subagent {
            // Its parent's record of it ([`parents`]), once a sibling holds one: a new session in
            // the group changes nothing it reads. Until then, the group and every sibling's
            // would-be record.
            let group = r.path.parent()?;
            let parents = parents(group, &r.id);
            if parents.is_empty() {
                deps.files.push(group.to_path_buf());
                for sibling in probe.list(group)?.iter() {
                    deps.files.push(sibling.join("subagents").join(&r.id).join("meta.json"));
                }
            }
            deps.files.extend(
                parents
                    .iter()
                    .map(|sibling| sibling.join("subagents").join(&r.id).join("meta.json")),
            );
        }
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(root) = &self.root else {
            return Ok(Default::default());
        };
        // An unreadable dir, and a summary whose scan fails, are skipped and reported.
        let mut out = crate::Discovered::default();
        for entry in WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    out.skipped.extend(crate::Skipped::walk(&e));
                    continue;
                }
            };
            if entry.file_name() != "summary.json" {
                continue;
            }
            let dir = entry.path().parent().unwrap();
            let mut reads = crate::source::ReadSet::default();
            reads.record(entry.path());
            let reference = SessionRef::new("", Harness::Grok, dir);
            reads.record(&self.resume_source(&reference));
            // Key by the public ref path, so the existing full-read count publication updates
            // this same cache entry. Summary and transcript identities guard its contents.
            let scanned = crate::discover_cache::cached_scan_with_reads(dir, reads, || scan(entry.path()));
            if let Some(found) = crate::Skipped::listed(entry.path(), scanned) {
                out.push(found);
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        r.path.join("chat_history.jsonl")
    }

    /// `chat_history.jsonl` through the driver, after the header `summary.json` gives. A
    /// checkpoint names its transcript: one of another resumes nothing.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let wire = self.resume_source(r);
            let mut tools = Tools::default();
            let mut mark = None;
            if let Start::Checkpoint(cp) = from {
                match jsonl::restore::<HistoryCheckpoint>(cp) {
                    Some(state) if state.wire == wire => {
                        mark = state.updates;
                        tools = state.tools;
                    }
                    _ => return Ok(None),
                }
            }
            let (s, updates) = session_header(r, mark);
            if mark.is_some() && !tools.unrevised(&updates) {
                return Ok(None);
            }
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Grok, sink);
            spawns.meta(&s);
            let read = jsonl::Read::open(&wire).with_context(|| format!("reading {}", wire.display()))?;
            let codec = History {
                s,
                enrich: &updates.tools,
                updates: updates.settled,
                tools,
                spans: opts.spans.then_some(read.source()),
                wire: &wire,
                pending: None,
                resolver: crate::Resolver::new(Some(wire.clone())),
            };
            jsonl::drive(&read, from, codec, &mut spawns)
        })
    }

    /// The todo list `todo_write` keeps in the session directory (`plan.json` `{todos: {id: …}}`),
    /// then the per-prompt restore points of `rewind_points.jsonl`, then every hook run
    /// `updates.jsonl` recorded ([`hook_runs`]).
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        let plan = crate::affordance::side_json(&r.path.join("plan.json"))?.and_then(|v| {
            let items = crate::affordance::items_of(&v);
            crate::affordance::plan_state(crate::affordance::Kind::Plan, "plan.json", items)
        });
        Ok(plan
            .into_iter()
            .chain(rewind_points(&r.path)?)
            .chain(hook_runs(&r.path)?)
            .collect())
    }
}

/// `rewind_points.jsonl`: one line per prompt, `{prompt_index, created_at, file_snapshots,
/// after_snapshots}` — the files as they were before (and after) that prompt, which `/rewind`
/// puts back. Each line is a checkpoint.
fn rewind_points(dir: &Path) -> Result<Vec<crate::affordance::Affordance>> {
    use crate::affordance::{Affordance, Kind};
    reduce_jsonl(&dir.join("rewind_points.jsonl"), |out: &mut Vec<Affordance>, v| {
        let Some(v) = Fields::parse(v, &["created_at", "file_snapshots", "prompt_index"])? else {
            return Ok(());
        };
        let ts = v.string("created_at")?.as_deref().and_then(parse_ts);
        let mut files = Vec::new();
        if let Some(snapshots) = v.get("file_snapshots") {
            // Original key order; a repeated key keeps its first place.
            let mut seen = std::collections::HashSet::new();
            snapshots.for_each_entry(|key, _| {
                if seen.insert(key.to_owned()) {
                    files.push(key.to_owned());
                }
                Ok(())
            })?;
        }
        let mut summary = match number(v.get("prompt_index"))?.and_then(|n| n.as_u64()) {
            Some(i) => format!("prompt {i}"),
            None => "prompt".into(),
        };
        if !files.is_empty() {
            summary.push_str(&format!(" · {}", files.join(", ")));
        }
        out.push(Affordance::store_event(
            Kind::Checkpoint,
            "create",
            ts,
            "rewind_points.jsonl",
            &summary,
        ));
        Ok(())
    })
}

/// Reduce a session-dir JSONL sidecar to `T`, one line at a time: a missing file is the empty
/// reduction, any other read error propagates.
fn reduce_jsonl<T: Default>(path: &Path, mut step: impl FnMut(&mut T, JsonView<'_>) -> Result<()>) -> Result<T> {
    let mut out = T::default();
    let read = jsonl::for_each_record(path, |v| step(&mut out, v));
    if let Err(error) = read {
        if !super::is_not_found(&error) {
            return Err(error);
        }
    }
    Ok(out)
}

/// A number field, or None for any other value.
fn number(v: Option<JsonView<'_>>) -> Result<Option<serde_json::Number>> {
    Ok(v.map(JsonView::as_number).transpose()?.flatten())
}

/// A string field of an object view, or None.
fn string(v: Option<JsonView<'_>>, key: &str) -> Result<Option<String>> {
    let Some(field) = v.map(|v| v.get(key)).transpose()?.flatten() else {
        return Ok(None);
    };
    Ok(field
        .is_string()?
        .then(|| field.resolve())
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

/// The `hook_execution` updates, reduced to metadata. Unknown update/run payloads are validated
/// at the line boundary but never materialized.
fn hook_runs(dir: &Path) -> Result<Vec<crate::affordance::Affordance>> {
    use crate::affordance::{Affordance, Kind};
    reduce_jsonl(&dir.join("updates.jsonl"), |out: &mut Vec<Affordance>, v| {
        let Some(params) = v.get("params")? else { return Ok(()) };
        let Some(u) = params.get("update")? else { return Ok(()) };
        if string(Some(u), "sessionUpdate")?.as_deref() != Some("hook_execution") {
            return Ok(());
        }
        let event = string(Some(u), "event_name")?.unwrap_or_else(|| "hook".into());
        let millis = number(
            params
                .get("_meta")?
                .map(|v| v.get("agentTimestampMs"))
                .transpose()?
                .flatten(),
        )?
        .and_then(|n| n.as_i64());
        let timestamp = millis.and_then(chrono::DateTime::from_timestamp_millis);
        let timestamp = if timestamp.is_some() {
            timestamp
        } else {
            number(v.get("timestamp")?)?
                .and_then(|n| n.as_i64())
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        };
        if let Some(runs) = u.get("runs")?.map(|v| v.array()).transpose()?.flatten() {
            for run in runs {
                let run = run?;
                let name = string(Some(run), "name")?.unwrap_or_default();
                let status = run.get("status")?;
                let label = string(status, "status")?.unwrap_or_default();
                let mut summary = format!("{event}: {name} · {label}");
                if let Some(ms) =
                    number(status.map(|v| v.get("elapsed_ms")).transpose()?.flatten())?.and_then(|n| n.as_u64())
                {
                    summary.push_str(&format!(" · {ms} ms"));
                }
                out.push(Affordance::store_event(
                    Kind::Hook,
                    "run",
                    timestamp,
                    "updates.jsonl",
                    &summary,
                ));
            }
        }
        Ok(())
    })
}

/// Static metadata/enrichment shared by whole and suffix passes. Mutable sidecars are verified
/// by the consumer's source baseline; an enrichment edit can revise an earlier visible tool.
fn session_header(r: &SessionRef, mark: Option<u64>) -> (Session, Updates) {
    // r.path is the session directory. `summary.json` (small) carries all session-level
    // metadata; the transcript is `chat_history.jsonl`, one record per line — streamed.
    let dir = &r.path;
    let summary = read_summary(&dir.join("summary.json")).unwrap_or(Value::Null);

    let mut s = Session {
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
        source_path: Some(dir.clone()),
        ..Session::new(r.id.clone(), Harness::Grok)
    };

    // Sidecar enrichment: tool_call / tool_call_update entries from the ACP update stream.
    let enrich = read_updates(dir, mark);

    // Subagent / agent-name facts + lineage from summary.json (and a sub-agent's parent from the
    // parent's `subagents/<id>/meta.json`).
    grok_session_facts(&mut s, &summary);
    grok_lineage(&mut s, &summary, dir);

    // Grok persists the system prompt it sent as `system_prompt.txt` — byte-identical to the
    // leading `system` transcript record. It is the session-level fact, never a message.
    if let Ok(sp) = fs::read_to_string(dir.join("system_prompt.txt")) {
        if !sp.trim().is_empty() {
            s.system_prompt = Some(sp);
        }
    }

    (s, enrich)
}

/// A mark on `chat_history.jsonl` and on the `updates.jsonl` the read was enriched from. The
/// updates file is a resumable dependency ([`crate::source::Dependencies::resumable`]): it grows
/// every turn, and a resumed read declines when its growth since `updates` revises a tool the
/// prefix emitted ([`Tools::unrevised`]). Marks without `updates` (taken while it was an exact
/// dependency) resume as before.
#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryCheckpoint<P = PathBuf, T = Tools> {
    wire: P,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updates: Option<u64>,
    #[serde(default)]
    tools: T,
}

/// The prefix's tool calls whose rendering growth of `updates.jsonl` could still revise: `open`,
/// emitted as a call whose result the prefix does not hold (only the call's title, kind and
/// locations were rendered); `unenriched`, emitted with no update at all. Every other tool the
/// prefix emitted had an update before the mark.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Tools {
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    open: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    unenriched: std::collections::BTreeSet<String>,
}

impl Tools {
    /// Note the tools of a message the read emits, enriched from `enrich`.
    fn emitted(&mut self, m: &Message, enrich: &HashMap<String, ToolEnrich>) {
        for block in &m.content {
            let (id, call) = match block {
                Block::ToolUse { id, .. } => (id, true),
                Block::ToolResult { tool_use_id, .. } => (tool_use_id, false),
                _ => continue,
            };
            if call {
                self.open.insert(id.clone());
            } else {
                self.open.remove(id);
            }
            if !enrich.contains_key(id) {
                self.unenriched.insert(id.clone());
            }
        }
    }

    /// Whether every tool the prefix emitted renders as it did: no update since the mark changed
    /// a tool the prefix holds (by its call alone, only what the call rendered). A tool first
    /// updated after the mark that the prefix never emitted is new; one updated before it may
    /// be in the prefix, and its change declines.
    fn unrevised(&self, updates: &Updates) -> bool {
        let Some(then) = &updates.at_mark else {
            return false;
        };
        fn call(e: Option<&ToolEnrich>) -> (Option<&String>, Option<&String>, &[String]) {
            e.map_or((None, None, &[]), |e| (e.title.as_ref(), e.kind.as_ref(), &e.locations))
        }
        let changed = updates
            .tools
            .keys()
            .chain(then.keys())
            .filter(|id| updates.tools.get(*id) != then.get(*id));
        for id in changed {
            let (old, new) = (then.get(id), updates.tools.get(id));
            if self.open.contains(id) {
                if call(old) != call(new) {
                    return false;
                }
            } else if old.is_some() || self.unenriched.contains(id) {
                return false;
            }
        }
        true
    }
}

/// The Grok record codec over `chat_history.jsonl`. A standalone `reasoning` record is held and
/// joins the next assistant turn, so the codec is settled only when none is held; a checkpoint
/// keeps the transcript it was taken on.
struct History<'r> {
    s: Session,
    enrich: &'r HashMap<String, ToolEnrich>,
    /// Where `updates.jsonl`'s complete lines ended when `enrich` was read.
    updates: u64,
    tools: Tools,
    spans: Option<&'r JsonSource>,
    wire: &'r Path,
    pending: Option<Message>,
    resolver: crate::Resolver,
}

impl jsonl::Codec for History<'_> {
    fn record(&mut self, view: JsonView<'_>, _: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        let (is_reasoning, message) = record::message(view, self.spans, self.enrich, &self.resolver)?;
        let Some(mut m) = message else {
            return Ok(Flow::Continue);
        };
        self.tools.emitted(&m, self.enrich);
        if is_reasoning {
            match self.pending.as_mut() {
                Some(p) => p.content.append(&mut m.content),
                None => self.pending = Some(m),
            }
            return Ok(Flow::Continue);
        }
        if let Some(mut p) = self.pending.take() {
            if m.role == Role::Assistant {
                p.content.append(&mut m.content);
                m.content = p.content;
            } else if sink.message(p) == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        Ok(sink.message(m))
    }

    /// Reasoning at EOF is provisional (AffordanceSink also excludes unresolved synchronous spawns).
    fn settled(&self) -> bool {
        self.pending.is_none()
    }

    /// The state (which transcript) is the same at every demand: a mark resumes at any.
    fn demand(&self) -> Demand {
        Demand::default()
    }

    fn session(&mut self) -> &mut Session {
        &mut self.s
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        HistoryCheckpoint {
            wire: self.wire,
            updates: Some(self.updates),
            tools: &self.tools,
        }
    }

    fn finish(self, stopped: bool, sink: &mut dyn MessageSink) -> Result<Session> {
        if let (Some(p), false) = (self.pending, stopped) {
            sink.message(p);
        }
        Ok(self.s)
    }
}

/// Subagent nickname + Grok-specific session facts from `summary.json`: a subagent's `agent_name`
/// becomes [`Lineage::agent_path`]; the raw `session_kind` / `agent_name` ride in `extra["grok"]`.
fn grok_session_facts(s: &mut Session, summary: &Value) {
    let kind = summary.get("session_kind").and_then(Value::as_str);
    let agent = summary.get("agent_name").and_then(Value::as_str);
    if kind == Some("subagent") {
        if let Some(a) = agent {
            s.lineage.agent_path = Some(a.to_string());
        }
    }
    if kind.is_some() || agent.is_some() {
        let bag = s.harness_extra_mut(Harness::Grok);
        if let Some(k) = kind {
            bag.insert("session_kind".into(), Value::String(k.to_string()));
        }
        if let Some(a) = agent {
            bag.insert("agent_name".into(), Value::String(a.to_string()));
        }
    }
}

/// Who this session came from. A forked or restored session's `summary.json` names its source in
/// `parent_session_id` (1.0.5x, with `forked_at`). A sub-agent's own summary names no parent; the
/// parent records the child under `<parent>/subagents/<child id>/meta.json` (`parent_session_id`),
/// and the parent sits in the same cwd group, so one directory listing finds it.
fn grok_lineage(s: &mut Session, summary: &Value, dir: &Path) {
    let subagent = summary.get("session_kind").and_then(Value::as_str) == Some("subagent");
    let named = summary
        .get("parent_session_id")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    if let (Some(p), false) = (named, subagent) {
        s.lineage.forked_from = Some(p.to_string());
    }
    if !subagent {
        return;
    }
    let Some(group) = dir.parent() else { return };
    for sibling in parents(group, &s.id).iter() {
        let meta = sibling.join("subagents").join(&s.id).join("meta.json");
        let Some(v) = fs::read_to_string(&meta)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        s.lineage.parent = v
            .get("parent_session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| sibling.file_name().and_then(|n| n.to_str()).map(str::to_string));
        s.lineage.spawned_by_tool_use = v.get("tool_call_id").and_then(Value::as_str).map(str::to_string);
        return;
    }
}

/// The sessions of cwd group `group` that record sub-agent `child` (`<sibling>/subagents/<child>/`),
/// in the group's listing order: where its parent's `meta.json` can be. One listing of the group
/// and of each sibling's `subagents/` serves every sub-agent of the group, kept while the group
/// lists the same sessions; a child it does not know lists them again (a parent records a child
/// after the group was listed).
fn parents(group: &Path, child: &str) -> std::sync::Arc<Vec<PathBuf>> {
    type Index = HashMap<String, std::sync::Arc<Vec<PathBuf>>>;
    type Groups = std::sync::Mutex<HashMap<PathBuf, (crate::source::FileId, std::sync::Arc<Index>)>>;
    static GROUPS: std::sync::LazyLock<Groups> = std::sync::LazyLock::new(Default::default);
    let lock = || GROUPS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = crate::source::FileId::at(group) else {
        return Default::default();
    };
    let known = lock()
        .get(group)
        .filter(|(at, _)| *at == state)
        .and_then(|(_, index)| index.get(child).cloned());
    if let Some(parents) = known {
        return parents;
    }
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
    if let Ok(siblings) = fs::read_dir(group) {
        for sibling in siblings.flatten() {
            let Ok(children) = fs::read_dir(sibling.path().join("subagents")) else {
                continue;
            };
            for c in children.flatten() {
                if let Ok(name) = c.file_name().into_string() {
                    index.entry(name).or_default().push(sibling.path());
                }
            }
        }
    }
    let index: Index = index.into_iter().map(|(c, p)| (c, std::sync::Arc::new(p))).collect();
    let parents = index.get(child).cloned().unwrap_or_default();
    // Kept only when the group did not change while it was listed.
    if crate::source::FileId::at(group) == Some(state) {
        lock().insert(group.to_path_buf(), (state, std::sync::Arc::new(index)));
    }
    parents
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
        ..Default::default()
    })
}

/// Enrichment for one tool call, mined from `updates.jsonl`.
#[derive(Default, Clone, PartialEq)]
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

/// `updates.jsonl` read for a pass: its per-`toolCallId` enrichment, where its complete lines
/// end, and (asked with a mark) the enrichment its records before the mark gave.
#[derive(Default)]
struct Updates {
    tools: HashMap<String, ToolEnrich>,
    /// `None` when no mark was asked, or the read failed (no resume is certified then).
    at_mark: Option<HashMap<String, ToolEnrich>>,
    settled: u64,
}

/// Parse `updates.jsonl` into per-`toolCallId` enrichment. Tolerant: missing/empty/malformed file
/// yields an empty map and zero behavior change.
#[cfg(test)]
fn read_update_enrichment(dir: &Path) -> HashMap<String, ToolEnrich> {
    read_updates(dir, None).tools
}

/// [`Updates`] of `dir`, the enrichment as of `mark` too when given.
fn read_updates(dir: &Path, mark: Option<u64>) -> Updates {
    let path = dir.join("updates.jsonl");
    let mut out = Updates::default();
    let read = (|| -> Result<()> {
        let read = jsonl::Read::open(&path)?;
        let mut step = enrichment_step();
        read.lines(0, |line| {
            if out.at_mark.is_none() && mark.is_some_and(|mark| line.at >= mark) {
                out.at_mark = Some(out.tools.clone());
            }
            if line.complete {
                out.settled = line.end;
            }
            if let Some(view) = line.view {
                step(&mut out.tools, view)?;
            }
            Ok(Flow::Continue)
        })
    })();
    match read {
        Ok(()) => {
            if mark.is_some() && out.at_mark.is_none() {
                out.at_mark = Some(out.tools.clone());
            }
        }
        // A missing file is the empty enrichment, as of any mark; any other failure is the
        // empty enrichment too, but certifies no resume.
        Err(error) => {
            let missing = super::is_not_found(&error);
            out = Updates::default();
            if missing && mark.is_some() {
                out.at_mark = Some(HashMap::new());
            }
        }
    }
    out
}

/// The per-record fold of [`read_updates`].
fn enrichment_step() -> impl FnMut(&mut HashMap<String, ToolEnrich>, JsonView<'_>) -> Result<()> {
    // Preserve first-seen order in the public vector without repeatedly scanning its prefix.
    // Membership belongs to this one read, not the retained enrichment or replay state.
    let mut seen_locations: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    move |map: &mut HashMap<String, ToolEnrich>, v| {
        let Some(u) = v.get("params")?.map(|p| p.get("update")).transpose()?.flatten() else {
            return Ok(());
        };
        let Some(u) = Fields::parse(
            u,
            &["sessionUpdate", "toolCallId", "title", "kind", "locations", "status"],
        )?
        else {
            return Ok(());
        };
        let su = u.string("sessionUpdate")?;
        if !matches!(su.as_deref(), Some("tool_call") | Some("tool_call_update")) {
            return Ok(());
        }
        let Some(id) = u.string("toolCallId")? else {
            return Ok(());
        };
        let seen = seen_locations.entry(id.clone()).or_default();
        let e = map.entry(id).or_default();
        if let Some(title) = u.string("title")? {
            if su.as_deref() == Some("tool_call_update") || e.title.is_none() {
                e.title = Some(title)
            }
        }
        if let Some(kind) = u.string("kind")? {
            e.kind = Some(kind)
        }
        if let Some(locations) = u.get("locations").map(|v| v.array()).transpose()?.flatten() {
            for location in locations {
                if let Some(path) = string(Some(location?), "path")? {
                    if seen.insert(path.clone()) {
                        e.locations.push(path)
                    }
                }
            }
        }
        if let Some(status) = u.string("status")? {
            e.status = Some(status)
        }
        Ok(())
    }
}

/// `summary.json`: the small session-level metadata sidecar. Missing or malformed is `Null` for
/// callers that tolerate it; [`scan`] propagates instead.
fn read_summary(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// `summary_path` is `.../<sid>/summary.json`; the session dir is its parent.
fn scan(summary_path: &Path) -> Result<Option<SessionRef>> {
    let dir = summary_path.parent().map(Path::to_path_buf);
    let Some(dir) = dir else { return Ok(None) };
    let summary = read_summary(summary_path)?;

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
        .map(|s| crate::ir::truncate(s, crate::projection::Head::TITLE));
    // Exact turn counts belong to index/full reads. Summary-titled sessions do not open the
    // transcript. Otherwise stream the codec's title demand until the first title or EOF;
    // absence beyond a fixed byte budget is not yet an authorized metadata tradeoff.
    if title.is_none() {
        let read = jsonl::Read::open(&dir.join("chat_history.jsonl"));
        if let Ok(read) = read {
            let mut candidate = None;
            if read
                .lines(0, |line| {
                    if let Some(view) = line.view {
                        candidate = record::discovery_title(view)?;
                    }
                    Ok(if candidate.is_some() {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    })
                })
                .is_ok()
            {
                title = candidate;
            }
        }
    }

    Ok(Some(SessionRef {
        cwd,
        title,
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
        message_count: None,
        ..SessionRef::new(id, Harness::Grok, dir)
    }))
}

#[cfg(test)]
mod tests {
    #[derive(Default)]
    struct ResumeKeep {
        messages: Vec<Message>,
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

    /// The record codec over an owned value, no spans.
    fn chat_message(v: &Value, enrich: &HashMap<String, ToolEnrich>) -> Option<Message> {
        record::message(v.into(), None, enrich, &crate::Resolver::new(None))
            .unwrap()
            .1
    }

    /// A whole stream of `r`, messages collected.
    fn whole(adapter: &Grok, r: &SessionRef, opts: &Demand) -> Session {
        let mut all = ResumeKeep::default();
        let mut s = adapter.try_stream(r, opts, &mut all).unwrap();
        s.messages = all.messages;
        s
    }

    #[test]
    fn dependency_decode_failure_is_named_and_never_certified() {
        let world = crate::test_world("grok-dependency-error");
        fs::write(world.home.join("summary.json"), "{broken").unwrap();
        let r = SessionRef::new("fixture", Harness::Grok, &world.home);
        let mut deps = Dependencies::default();
        assert!(ROOTLESS.dependencies(&r, &mut Probe::default(), &mut deps).is_none());
        assert!(!crate::env::warn_once("Grok dependencies", "already reported"));
    }

    #[test]
    fn enrichment_many_locations_keeps_first_seen_order_per_call() {
        let world = crate::test_world("grok-many-tool-locations");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        let mut rows = Vec::new();
        for i in 0..4096 {
            for id in ["a", "b"] {
                rows.push(serde_json::json!({"params":{"update":{
                    "sessionUpdate":"tool_call_update","toolCallId":id,
                    "title":format!("title-{i}"),"status":"completed",
                    "locations":[{"path":format!("{id}/file-{i}")},{"path":format!("{id}/file-0")},{"path":format!("{id}/file-{i}")}]
                }}}).to_string());
            }
        }
        fs::write(dir.join("updates.jsonl"), rows.join("\n")).unwrap();
        let actual = read_update_enrichment(&dir);
        for id in ["a", "b"] {
            let expected = (0..4096).map(|i| format!("{id}/file-{i}")).collect::<Vec<_>>();
            assert_eq!(actual[id].locations, expected);
            assert_eq!(actual[id].title.as_deref(), Some("title-4095"));
            assert_eq!(actual[id].status.as_deref(), Some("completed"));
        }
    }

    #[test]
    fn checked_resume_retains_reasoning_arguments_results_and_source_identity() {
        let world = crate::test_world("grok_selected_resume");
        let dir = world.home.join(".grok/session");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chat_history.jsonl");
        let payload = "雪\n\"\\".repeat(20_000);
        let body = format!(
            "{}\nbad suffix\n{}\n{}\n{}\n",
            serde_json::json!({"type":"user","content":"start"}),
            serde_json::json!({"type":"reasoning","summary":[{"text":payload}],"encrypted_content":"opaque"}),
            serde_json::json!({"type":"assistant","content":payload,"tool_calls":[{"id":"call","name":"Read","arguments":serde_json::json!({"payload":payload}).to_string()},{"id":"structured","name":"Read","arguments":{"payload":payload}}]}),
            serde_json::json!({"type":"tool_result","tool_call_id":"call","content":payload})
        );
        fs::write(&path, &body).unwrap();
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let r = SessionRef::new("session", Harness::Grok, &dir);
        let mut full = ResumeKeep::default();
        let shell = adapter.stream(&r, &Demand::RICH, &mut full).unwrap();
        let (count, checkpoint) = &full.checkpoints[0];
        let mut resumed = ResumeKeep::default();
        let suffix = adapter
            .transcript(
                &r,
                &Demand::LEAN.with_spans(),
                crate::stream::Start::Checkpoint(checkpoint),
                &mut resumed,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(suffix).unwrap(),
            serde_json::to_value(shell).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&resumed.messages).unwrap(),
            serde_json::to_value(&full.messages[*count..]).unwrap()
        );
        assert!(resumed.messages.iter().flat_map(|m| &m.content).any(|b| matches!(
            b,
            Block::Thinking {
                text: Text::Source(_),
                ..
            }
        )));
        assert_eq!(
            resumed.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
            full.checkpoints.iter().skip(1).map(|(_, cp)| cp).collect::<Vec<_>>()
        );
        assert!(resumed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { id, input: crate::Json::Source(_), .. } if id == "structured")));
        // Truncation, not growth: an append-only log may grow under a retained source.
        fs::write(&path, "").unwrap();
        assert!(serde_json::to_value(&resumed.messages).is_err());
    }

    #[test]
    fn resume_corpus_keeps_grouping_enrichment_and_exact_suffixes() {
        let world = crate::test_world("grok_resume_corpus");
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/grok");
        let mut files = 0;
        let mut resumes = 0;
        for file in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if file.file_name() != "chat_history.jsonl" {
                continue;
            }
            let r = SessionRef::new("corpus", Harness::Grok, file.path().parent().unwrap());
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
                    let s = adapter
                        .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut rest)
                        .unwrap()
                        .unwrap();
                    assert_eq!(serde_json::to_value(s).unwrap(), serde_json::to_value(&shell).unwrap());
                    assert_eq!(
                        serde_json::to_value(&rest.messages).unwrap(),
                        serde_json::to_value(&full.messages[*count..]).unwrap(),
                        "{} @{} grouped suffix",
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
        assert!(files >= 15 && resumes >= 100, "native files={files} resumes={resumes}");
        eprintln!("grok native files={files} exact suffix resumes={resumes}");
    }

    #[test]
    fn resume_replays_eof_reasoning_and_partial_records_and_reads_only_appends() {
        use std::io::Write as _;
        let world = crate::test_world("grok_reasoning_resume");
        let dir = world.home.join(".grok/sessions/group/session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"session","cwd":"/old"},"current_model_id":"grok"}"#,
        )
        .unwrap();
        let line = "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}\n";
        let prefix = format!("{}bad json\n{{\"type\":\"reasoning\",\"summary\":[{{\"text\":\"thinking\"}}]}}\n{{\"type\":\"assistant\",\"model_id\":\"grok\",\"content\":", line.repeat(20_000));
        fs::write(dir.join("chat_history.jsonl"), &prefix).unwrap();
        let mut r = SessionRef::new("session", Harness::Grok, &dir);
        let adapter = Grok::with_home(&world.home.join(".grok"));
        let opts = Demand::LEAN.with_spans();
        let mut first = ResumeKeep::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        assert_eq!(first.messages.len(), count + 1); // Provisional EOF reasoning is replayed.
        assert!(serde_json::to_vec(cp).unwrap().len() < 512);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(adapter.resume_source(&r))
            .unwrap();
        writeln!(file, "\"answer\"}}").unwrap();
        file.write_all(line.repeat(200).as_bytes()).unwrap();
        drop(file);
        r.title = Some("current catalog title".into());
        let expected = whole(&adapter, &r, &opts);
        let mut suffix = ResumeKeep::default();
        let mut shell = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        assert_eq!(suffix.messages.len(), 201);
        assert_eq!(shell.title, r.title);
        shell.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let mut other = r.clone();
        other.path = dir.join("other");
        let mut none = ResumeKeep::default();
        assert!(adapter
            .transcript(&other, &opts, crate::stream::Start::Checkpoint(cp), &mut none)
            .unwrap()
            .is_none());
        assert!(none.messages.is_empty());
        // A UTF-8 failure remains a tolerant skipped line rather than poisoning subsequent records.
        let mut bytes = fs::read(adapter.resume_source(&r)).unwrap();
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(line.as_bytes());
        fs::write(adapter.resume_source(&r), bytes).unwrap();
        let after = whole(&adapter, &r, &opts);
        assert_eq!(after.messages.len(), count + 1 + 200 + 1);
        assert_eq!(after.cv_extra().unwrap()["skipped_lines"], 2);
        eprintln!(
            "grok stable_prefix=20000 appended_callbacks=201 whole_callbacks=20201 checkpoint_bytes={}",
            serde_json::to_vec(cp).unwrap().len()
        );
    }

    use super::*;
    use crate::harness::Parse;
    use crate::stream::Checkpoint;

    #[cfg(unix)]
    #[test]
    fn callback_boundaries_and_held_eof_verify_the_backing() {
        const CHILD: &str = "CV_TEST_GROK_CALLBACK_BACKING";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            // The child must prove it ran: a drifted `--exact` filter runs zero tests and still
            // exits 0.
            let world = cv_testkit::World::new("grok-callback-backing");
            let ran = world.base.join("child-ran");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "harness::grok::tests::callback_boundaries_and_held_eof_verify_the_backing",
                ])
                .env(CHILD, "1")
                .env(format!("{CHILD}_RAN"), &ran);
            // A regressed mapped read may fail in the child, never in the shared runner.
            unsafe {
                command.pre_exec(|| {
                    let limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::setrlimit(libc::RLIMIT_CORE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "Grok callback child {} failed: {status}", child.id());
                    assert!(
                        ran.exists(),
                        "the child body never ran (the exact filter matched nothing)"
                    );
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("Grok callback child {} timed out", child.id());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if let Some(ran) = std::env::var_os(format!("{CHILD}_RAN")) {
            fs::write(ran, b"ran").unwrap();
        }
        eprintln!("GROK_CALLBACK_CHILD pid={}", std::process::id());
        struct Callback {
            wire: PathBuf,
            stage: &'static str,
            action: &'static str,
            stop: bool,
            calls: usize,
            checkpoints: usize,
            changed: bool,
            first: Option<Message>,
        }
        impl Callback {
            fn change(&mut self, stage: &str) {
                if self.stage == stage && !self.changed {
                    match self.action {
                        "truncate" => fs::OpenOptions::new()
                            .write(true)
                            .open(&self.wire)
                            .unwrap()
                            .set_len(0)
                            .unwrap(),
                        "delete" => fs::remove_file(&self.wire).unwrap(),
                        _ => return,
                    }
                    self.changed = true;
                    // A callback that changed the file is a slow one by construction (the stream
                    // module's source-guarantee policy): the reader checks right after it.
                    std::thread::sleep(super::jsonl::Read::SLOW_CALLBACK * 10);
                }
            }
        }
        impl MessageSink for Callback {
            fn message(&mut self, message: Message) -> Flow {
                self.calls += 1;
                if self.first.is_none() {
                    self.first = Some(message);
                }
                self.change("message");
                if self.stop {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                self.change("wants");
                self.stage == "checkpoint"
            }
            fn checkpoint(&mut self, _: &mut Checkpoint) {
                self.checkpoints += 1;
                self.change("checkpoint");
            }
        }
        let world = crate::test_world("grok-callback-boundaries");
        let home = world.home.join(".grok");
        let dir = home.join("sessions/group/scratch");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"scratch","cwd":"/synthetic"}}"#,
        )
        .unwrap();
        let wire = dir.join("chat_history.jsonl");
        let reference = SessionRef::new("scratch", Harness::Grok, &dir);
        let adapter = Grok::with_home(&home);
        let reasoning =
            "{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"held reasoning\"}]}\n";
        let user = serde_json::json!({"type":"user","content":"owned ".repeat(1000)});
        let turns = format!("{user}\n{}", "{\"type\":\"user\",\"content\":\"tail\"}\n".repeat(999));
        // EOF first gives a non-crashing independent refutation on an unguarded baseline.
        for stage in ["eof", "pending", "message", "wants", "checkpoint"] {
            let bytes = match stage {
                "eof" => reasoning.to_owned(),
                "pending" => format!("{reasoning}{turns}"),
                _ => turns.clone(),
            };
            for lazy in [false, true] {
                for action in ["unchanged", "truncate", "delete"] {
                    if action == "delete" && stage != "eof" {
                        continue;
                    }
                    for stop in [false, true] {
                        if stop && matches!(stage, "wants" | "checkpoint") {
                            continue;
                        }
                        fs::write(&wire, &bytes).unwrap();
                        let mut sink = Callback {
                            wire: wire.clone(),
                            stage: if matches!(stage, "eof" | "pending") {
                                "message"
                            } else {
                                stage
                            },
                            action,
                            stop,
                            calls: 0,
                            checkpoints: 0,
                            changed: false,
                            first: None,
                        };
                        let opts = if lazy { Demand::LEAN.with_spans() } else { Demand::RICH };
                        let result = adapter.try_stream(&reference, &opts, &mut sink);
                        assert_eq!(
                            result.is_err(),
                            sink.changed,
                            "stage={stage},lazy={lazy},action={action},stop={stop}"
                        );
                        if sink.changed || stop {
                            assert_eq!(
                                sink.calls, 1,
                                "do not deliver another record after invalidation or Stop"
                            );
                        } else {
                            assert_eq!(
                                sink.calls,
                                match stage {
                                    "eof" => 1,
                                    "pending" => 1001,
                                    _ => 1000,
                                }
                            );
                        }
                        assert_eq!(sink.checkpoints > 0, stage == "checkpoint");
                        if action == "unchanged" {
                            assert_eq!(fs::read(&wire).unwrap(), bytes.as_bytes());
                        }
                        if !lazy && stage == "message" {
                            let Block::Text {
                                text: Text::Inline(text),
                                ..
                            } = &sink.first.as_ref().unwrap().content[0]
                            else {
                                panic!("owned text")
                            };
                            assert_eq!(text, &"owned ".repeat(1000));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cheap_discovery_summary_title_never_opens_the_transcript() -> Result<()> {
        let world = crate::test_world("grok-summary-only");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("summary.json"),
            r#"{"info":{"id":"s","cwd":"/project"},"session_summary":"native title","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z"}"#,
        )?;
        // A titled session is identifiable even while its transcript is absent/unreadable.
        jsonl::OPENED.with(|paths| paths.borrow_mut().clear());
        let r = scan(&dir.join("summary.json"))?.unwrap();
        assert_eq!(r.title.as_deref(), Some("native title"));
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/project")));
        assert_eq!(r.updated_at.unwrap().to_rfc3339(), "2026-01-02T00:00:00+00:00");
        assert_eq!(r.message_count, None);
        assert!(jsonl::OPENED.with(|paths| paths.borrow().is_empty()));
        Ok(())
    }

    #[test]
    fn cheap_discovery_keeps_a_title_after_a_giant_preamble() -> Result<()> {
        let world = crate::test_world("grok-bounded-title");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("summary.json"), r#"{"info":{"id":"s"}}"#)?;
        let history = format!(
            "{{\"type\":\"system\",\"content\":\"{}\"}}\n{{\"type\":\"user\",\"content\":\"late title\"}}\n",
            "x".repeat(1 << 20)
        );
        fs::write(dir.join("chat_history.jsonl"), history)?;
        let r = scan(&dir.join("summary.json"))?.unwrap();
        assert_eq!(
            r.title.as_deref(),
            Some("late title"),
            "missing-summary fallback is unchanged"
        );
        assert_eq!(r.message_count, None);
        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cheap_discovery_keeps_exact_full_read_counts_only_while_sources_are_unchanged() -> Result<()> {
        let world = crate::test_world("grok-count-publication");
        let home = world.base.join("grok");
        let dir = home.join("sessions/s");
        fs::create_dir_all(&dir)?;
        let summary = dir.join("summary.json");
        fs::write(&summary, r#"{"info":{"id":"s"},"session_summary":"native title"}"#)?;
        let history = dir.join("chat_history.jsonl");
        let first = "{\"type\":\"user\",\"content\":\"question\"}\n{\"type\":\"assistant\",\"content\":\"answer\"}\n";
        fs::write(&history, first)?;
        let grok = Grok::with_home(&home);
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let r = grok.discover()?.remove(0);
        assert_eq!(r.message_count, None);
        crate::catalog::sync(std::slice::from_ref(&r));
        crate::discover_cache::persist(false);
        let mut sink = crate::stream::CollectSink::default();
        let done = crate::stream::stream_counted(
            &grok,
            &r,
            &Demand::FACTS.with_spans(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(done.counted.unwrap().turns, 2);
        assert_eq!(grok.discover()?[0].message_count, Some(2));
        let conn = crate::catalog::open_db().unwrap();
        let persisted: String = conn.query_row(
            "SELECT refs FROM scan_cache WHERE path=?1",
            [dir.to_string_lossy().as_ref()],
            |row| row.get(0),
        )?;
        // Raw discovery metadata stays unknown: authenticated counts are refilled by logical
        // session identity, rather than rewriting a cache entry whose key may be a directory.
        assert_eq!(
            serde_json::from_str::<Vec<SessionRef>>(&persisted)?[0].message_count,
            None
        );
        assert_eq!(
            crate::catalog::Reader::open().unwrap().all_sessions().unwrap()[0].message_count,
            Some(2)
        );
        // A new operation also retains the exact count without an in-memory cache mutation.
        drop(_scope);
        let next = crate::discover_cache::read::Context::current();
        let _next_scope = next.enter();
        assert_eq!(grok.discover()?[0].message_count, Some(2));
        fs::write(&history, format!("{first}{{\"type\":\"user\",\"content\":\"next\"}}\n"))?;
        assert_eq!(
            grok.discover()?[0].message_count,
            None,
            "growth invalidates the exact count"
        );
        let grown = grok.discover()?.remove(0);
        crate::catalog::sync(std::slice::from_ref(&grown));
        crate::stream::stream_counted(
            &grok,
            &grown,
            &Demand::FACTS.with_spans(),
            &mut sink,
            crate::stream::Pass::RECORD,
        )?;
        assert_eq!(grok.discover()?[0].message_count, Some(3));
        fs::write(&summary, r#"{"info":{"id":"s"},"session_summary":"renamed"}"#)?;
        let renamed = grok.discover()?.remove(0);
        assert_eq!(renamed.title.as_deref(), Some("renamed"));
        assert_eq!(renamed.message_count, None);
        Ok(())
    }

    #[test]
    fn discovery_title_matches_value_oracle_without_counting() {
        let world = cv_testkit::World::new("grok-raw-title");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        let summary = dir.join("summary.json");
        fs::write(&summary, r#"{"info":{"id":"session"}}"#).unwrap();
        let bodies = [
            serde_json::json!("雪\n\"\\".repeat(3000)),
            serde_json::json!(format!("{}late", " ".repeat(10000))),
            serde_json::json!("\u{2003}".repeat(10000)),
            serde_json::json!([{"text":""},{"ignored":"payload"},{"text":"雪\n".repeat(3000)},{"text":"last"}]),
            serde_json::json!([{"text":""},{"text":""},{"text":"answer"}]),
            serde_json::json!([{"text":""},{"text":""}]),
            serde_json::json!([{"text":false}]),
        ];
        let mut cases: Vec<String> = bodies
            .into_iter()
            .map(|content| serde_json::json!({"type":"user", "content":content}).to_string())
            .collect();
        let mut parts = vec![serde_json::json!({"text":""}); 4096];
        parts.push(serde_json::json!({"text":"late"}));
        cases.push(serde_json::json!({"type":"user","content":parts}).to_string());
        cases.extend([
            r#"{"type":"user","ty\u0070e":"assistant","content":"ignored user title"}"#.to_string(),
            r#"{"type":"user","content":[{"text":"old","te\u0078t":"last"}]}"#.to_string(),
            r#"{"type":"user","content":"must skip","ignored":"\ud800"}"#.to_string(),
            r#"{"type":"user","content":"must skip","ignored":1e400}"#.to_string(),
        ]);
        for first in cases {
            let text = format!("{first}\n{{\"type\":\"user\",\"content\":\"fallback\"}}\n{{\"type\":\"assistant\",\"content\":\"answer\"}}\n");
            fs::write(dir.join("chat_history.jsonl"), &text).unwrap();
            let mut expected_title = None;
            let mut expected_count = 0;
            super::super::for_each_json_line_str(&text, |v| {
                let ty = v.get("type").and_then(Value::as_str);
                expected_count += usize::from(matches!(ty, Some("user" | "assistant")));
                if expected_title.is_none() && ty == Some("user") {
                    let text = super::super::wire::joined_owned(v.get("content"), super::super::wire::TEXT);
                    if !text.trim().is_empty() {
                        expected_title = Some(crate::ir::truncate(&text, crate::projection::Head::TITLE));
                    }
                }
                Flow::Continue
            });
            let actual = scan(&summary).unwrap().unwrap();
            assert_eq!(actual.message_count, None);
            assert!(expected_count > 0);
            assert_eq!(
                actual.title,
                expected_title,
                "title oracle case with {} source bytes",
                first.len()
            );
        }
        // A non-UTF-8 line is one rejected record, as in the stream.
        fs::write(
            dir.join("chat_history.jsonl"),
            b"{\"type\":\"user\",\"content\":\"prefix\"}\n\xff",
        )
        .unwrap();
        let actual = scan(&summary).unwrap().unwrap();
        assert_eq!(actual.message_count, None);
        assert_eq!(actual.title.as_deref(), Some("prefix"));
    }

    #[test]
    fn sidecar_projections_preserve_order_selection_and_strict_acceptance() {
        use crate::affordance::{Affordance, Kind};
        let world = cv_testkit::World::new("grok-raw-sidecars");
        let dir = &world.base;
        let updates = [
            r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"call","title":"bare","locations":[{"path":"b"},{"path":"a"},{"path":"b"}],"kind":"read"}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call","title":"old","ti\u0074le":"rich","status":"completed","locations":[{"path":"a"},{"path":"c"}]}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call","status":"failed"}},"ignored":"\ud800"}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook_execution","event_name":"after","runs":[{"name":"check","status":{"status":"ok","elapsed_ms":3}},{"name":"second","status":null}]},"_meta":{"agentTimestampMs":1700000000123}},"timestamp":1}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook\u005fexecution","runs":[{"name":"escaped"}]}}}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook\u005fexecution","runs":[{"name":"selected","status":{"status":"ok"}}]}},"ignored":"hook_execution"}"#,
            r#"{"params":{"update":{"sessionUpdate":"hook_execution","runs":[{"name":"invalid"}]}},"ignored":1e400}"#,
        ].join("\n");
        fs::write(dir.join("updates.jsonl"), &updates).unwrap();
        let e = read_update_enrichment(dir);
        assert_eq!(e.len(), 1);
        assert_eq!(e["call"].title.as_deref(), Some("rich"));
        assert_eq!(e["call"].kind.as_deref(), Some("read"));
        assert_eq!(e["call"].status.as_deref(), Some("completed"));
        assert_eq!(e["call"].locations, ["b", "a", "c"]);
        let ts = chrono::DateTime::from_timestamp_millis(1700000000123);
        let expected = [
            Affordance::store_event(Kind::Hook, "run", ts, "updates.jsonl", "after: check · ok · 3 ms"),
            Affordance::store_event(Kind::Hook, "run", ts, "updates.jsonl", "after: second · "),
            Affordance::store_event(Kind::Hook, "run", None, "updates.jsonl", "hook: escaped · "),
            Affordance::store_event(Kind::Hook, "run", None, "updates.jsonl", "hook: selected · ok"),
        ];
        assert_eq!(
            serde_json::to_value(hook_runs(dir).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let snapshots = format!(
            r#"{{"prompt_index":2,"file_snapshots":{{"b":{{"body":"{}"}},"a":1,"b":2}},"after_snapshots":{{"ignored":"{}"}}}}"#,
            "x".repeat(65536),
            "x".repeat(65536)
        );
        fs::write(
            dir.join("rewind_points.jsonl"),
            format!("{snapshots}\n{{\"file_snapshots\":null}}\n{{\"file_snapshots\":{{\"bad\":\"\\ud800\"}}}}\n"),
        )
        .unwrap();
        let expected = [
            Affordance::store_event(
                Kind::Checkpoint,
                "create",
                None,
                "rewind_points.jsonl",
                "prompt 2 · b, a",
            ),
            Affordance::store_event(Kind::Checkpoint, "create", None, "rewind_points.jsonl", "prompt"),
        ];
        assert_eq!(
            serde_json::to_value(rewind_points(dir).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // A non-UTF-8 line is one rejected record; the rows around it stand.
        let mut bad = updates.into_bytes();
        bad.extend_from_slice(b"\n\xff");
        fs::write(dir.join("updates.jsonl"), bad).unwrap();
        assert_eq!(hook_runs(dir).unwrap().len(), 4);
        assert_eq!(read_update_enrichment(dir).len(), 1);
        fs::write(dir.join("rewind_points.jsonl"), b"{}\n\xff").unwrap();
        assert_eq!(rewind_points(dir).unwrap().len(), 1);
    }

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
                assert_eq!(input.resolve().unwrap()["target_file"], "a.rs");
                assert_eq!(input.resolve().unwrap()["limit"], 10);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(m.kind, MessageKind::Reply);
        assert_eq!(m.origin, Origin::Model);
        let bag = m.harness_extra(Harness::Grok).expect("grok bag");
        assert_eq!(bag["model_fingerprint"], "fp_x");
        assert!(!m.extra.contains_key("model_fingerprint"), "no flat keys");
        // encrypted reasoning preserved
        match &m.content[0] {
            Block::Thinking { encrypted, text, .. } => {
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(text.inline_str(), Some("thinking"));
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
        assert_eq!(m.kind, MessageKind::ToolResult);
        assert_eq!(m.origin, Origin::Harness);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content.inline_str(), Some("file contents"));
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
        let tc = &m.harness_extra(Harness::Grok).expect("grok bag")["tool_calls"]["call-9"];
        assert_eq!(tc["title"], "Search foo");
        assert_eq!(tc["kind"], "search");
    }

    #[test]
    fn user_synthetic_reason_preserved() {
        let line =
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"synthetic_reason":"project_instructions"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hi"));
        // A `synthetic_reason` user turn is harness-injected context, not a prompt.
        assert_eq!(m.kind, MessageKind::InjectedContext);
        assert_eq!(m.origin, Origin::Harness);
        assert_eq!(
            m.harness_extra(Harness::Grok).unwrap()["synthetic_reason"],
            "project_instructions"
        );
        assert!(!m.extra.contains_key("synthetic_reason"), "no flat keys");
    }

    /// The kind/origin mapping across every Grok record shape, plus the `<user_info>` env block.
    #[test]
    fn kind_and_origin_by_record() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new());

        // A real typed prompt.
        let p = msg(r#"{"type":"user","content":[{"type":"text","text":"do the thing"}],"synthetic_reason":null}"#)
            .unwrap();
        assert_eq!((p.kind, p.origin), (MessageKind::Prompt, Origin::Human));

        // The `<user_info>` environment block is harness-injected context, though unflagged.
        // (The newlines inside the block are JSON `\n` escapes: a literal newline inside a JSON
        // string is a control character and would not parse.)
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":null}"#,
        )
        .unwrap();
        assert!(ui.text().is_some_and(|t| t.contains("\nOS: mac\n")));
        assert_eq!((ui.kind, ui.origin), (MessageKind::InjectedContext, Origin::Harness));

        // A compaction summary seeds the next window.
        let cm = msg(
            r#"{"type":"user","content":[{"type":"text","text":"summary…"}],"synthetic_reason":"compaction_meta"}"#,
        )
        .unwrap();
        assert_eq!((cm.kind, cm.origin), (MessageKind::CompactionSummary, Origin::Harness));

        // The system record is the system prompt.
        let sys = msg(r#"{"type":"system","content":"You are Grok."}"#).unwrap();
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );

        // A model reply.
        let a = msg(r#"{"type":"assistant","content":"hi"}"#).unwrap();
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
    }

    #[test]
    fn windowed_reads_keep_the_model() {
        let _world = crate::test_world("grok-windowed");
        let home = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/grok/1.0.50/.grok");
        crate::harness::assert_windowed_models(&Grok::with_home(&home));
    }

    #[test]
    fn reasoning_records_join_the_next_turn_and_lineage_comes_from_summary_and_meta() {
        let group = tempdir_with("x", "");
        let parent = group.join("p1");
        let child = group.join("c1");
        let fork = group.join("f1");
        for d in [&parent, &child, &fork] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(parent.join("subagents/c1")).unwrap();
        fs::write(
            parent.join("subagents/c1/meta.json"),
            r#"{"parent_session_id":"p1","child_session_id":"c1","tool_call_id":"call_7"}"#,
        )
        .unwrap();
        fs::write(
            child.join("summary.json"),
            r#"{"info":{"id":"c1"},"session_kind":"subagent","agent_name":"general-purpose"}"#,
        )
        .unwrap();
        fs::write(
            fork.join("summary.json"),
            r#"{"info":{"id":"f1"},"parent_session_id":"p1","forked_at":"2026-10-11T21:04:37Z"}"#,
        )
        .unwrap();
        // Shapes from the 1.0.50 corpus: reasoning, then the reply; then trailing reasoning alone.
        let chat = [
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}],"prompt_index":0}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"think"}]}"#,
            r#"{"type":"assistant","content":"hello","model_id":"m"}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"cut off"}]}"#,
        ]
        .join("\n");
        for d in [&child, &fork] {
            fs::write(d.join("chat_history.jsonl"), &chat).unwrap();
        }
        let grok = Grok {
            root: Some(group.clone()),
        };
        let c = grok
            .parse(&scan(&child.join("summary.json")).unwrap().unwrap())
            .unwrap();
        assert_eq!(c.lineage.parent.as_deref(), Some("p1"));
        assert_eq!(c.lineage.spawned_by_tool_use.as_deref(), Some("call_7"));
        assert_eq!(c.lineage.forked_from, None);
        let f = grok.parse(&scan(&fork.join("summary.json")).unwrap().unwrap()).unwrap();
        assert_eq!(f.lineage.forked_from.as_deref(), Some("p1"));
        assert_eq!(f.lineage.parent, None);
        let kinds: Vec<Vec<&str>> = f
            .messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        Block::Thinking { .. } => "thinking",
                        Block::Text { .. } => "text",
                        _ => "other",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(kinds, vec![vec!["text"], vec!["thinking", "text"], vec!["thinking"]]);
        let _ = fs::remove_dir_all(&group);
    }

    /// A sub-agent is certified by its own files and its parent's record of it: a new session in
    /// its cwd group leaves it fresh; a sub-agent whose parent has not recorded it yet notices
    /// the record appearing.
    #[test]
    fn a_new_session_in_the_group_leaves_a_sub_agent_certified() {
        let group = tempdir_with("x", "");
        let session = |id: &str, kind: &str| {
            let dir = group.join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("summary.json"),
                format!(r#"{{"info":{{"id":"{id}"}},"session_kind":"{kind}"}}"#),
            )
            .unwrap();
            fs::write(
                dir.join("chat_history.jsonl"),
                "{\"type\":\"user\",\"content\":\"hi\"}\n",
            )
            .unwrap();
            SessionRef::new(id, Harness::Grok, &dir)
        };
        for i in 0..8 {
            session(&format!("s{i}"), "headless");
        }
        session("p1", "headless");
        let meta = group.join("p1/subagents/c1/meta.json");
        fs::create_dir_all(meta.parent().unwrap()).unwrap();
        fs::write(&meta, r#"{"parent_session_id":"p1"}"#).unwrap();
        let child = session("c1", "subagent");
        let orphan = session("c2", "subagent");
        // A pass's baseline (its inventory and an empty read set) against the store now.
        let snapshot = |r: &SessionRef| Probe::default().snapshot(r).with_reads(&Default::default());
        let (child_then, orphan_then) = (snapshot(&child), snapshot(&orphan));
        session("late", "headless");
        assert!(child_then.fresh(&snapshot(&child), &mut Probe::default()));
        assert!(!orphan_then.fresh(&snapshot(&orphan), &mut Probe::default()));
        let orphan_then = snapshot(&orphan);
        let record = group.join("late/subagents/c2/meta.json");
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, r#"{"parent_session_id":"late"}"#).unwrap();
        assert!(!orphan_then.fresh(&snapshot(&orphan), &mut Probe::default()));
        let grok = Grok {
            root: Some(group.clone()),
        };
        assert_eq!(grok.parse(&orphan).unwrap().lineage.parent.as_deref(), Some("late"));
        let child_then = snapshot(&child);
        fs::write(&meta, r#"{"parent_session_id":"p1","tool_call_id":"call_9"}"#).unwrap();
        assert!(!child_then.fresh(&snapshot(&child), &mut Probe::default()));
        assert_eq!(
            grok.parse(&child).unwrap().lineage.spawned_by_tool_use.as_deref(),
            Some("call_9")
        );
        let _ = fs::remove_dir_all(&group);
    }

    #[test]
    fn compaction_meta_on_the_user_info_block_is_context_not_the_summary() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new()).unwrap();
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(ui.kind, MessageKind::InjectedContext);
        let sum = msg(
            r#"{"type":"user","content":[{"type":"text","text":"This session is being continued"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(sum.kind, MessageKind::CompactionSummary);
    }

    #[test]
    fn empty_updates_is_harmless() {
        assert!(read_update_enrichment(Path::new("/nonexistent/dir/xyz")).is_empty());
    }

    #[test]
    fn parses_fixture_session_v1() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/v1_tools");
        assert!(dir.exists(), "missing committed fixture {}", dir.display());
        let r = scan(&dir.join("summary.json")).unwrap().unwrap();
        let grok = Grok { root: None };
        let s = grok.parse(&r).unwrap();
        // The contract count: user + assistant turns (the fixture's `system` line and two
        // `tool_result` lines are not turns; summary.json's `num_chat_messages` says 5).
        assert_eq!(r.message_count, None, "discovery does not count transcript turns");
        assert_eq!(
            4,
            s.messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count(),
            "a full read still yields the exact user+assistant count"
        );
        // Every Grok fact is nested under `extra["grok"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);
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
        // The system record → SystemPrompt kind; system_prompt.txt (if present) seeds the session.
        assert!(s
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::SystemPrompt && m.role == Role::System));
        // summary.json marks this a subagent session.
        assert_eq!(
            s.harness_extra(Harness::Grok)
                .and_then(|b| b.get("session_kind"))
                .and_then(Value::as_str),
            Some("subagent")
        );
    }
}
