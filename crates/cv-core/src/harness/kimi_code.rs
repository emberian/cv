//! Kimi Code adapter — `~/.kimi-code/sessions/wd_<slug>_<sha256(cwd)[:12]>/session_<uuid>/`.
//!
//! Kimi Code is Moonshot's successor to kimi-cli (the [`kimi`](crate::harness::kimi) adapter's
//! `~/.kimi` store, frozen since the 2026-06 migration). The two share nothing but the word "wire":
//! this store is a per-agent **flat** record log, not `context.jsonl` + a `{message:{type,payload}}`
//! sidecar. Ground truth is the installed bundle
//! (`…/@moonshot-ai/kimi-code/dist/main.mjs`: `FileSystemAgentRecordPersistence(join(agentDir,
//! "wire.jsonl"))`, `join(homeDir, "session_index.jsonl")`, `agent.scope("tool-results")`, root
//! `process.env.KIMI_CODE_HOME ?? ~/.kimi-code`) and the real files on this machine (protocol 1.4/1.5).
//!
//! ## Layout
//! - `<root>/session_index.jsonl` — append-only `{sessionId, sessionDir, workDir}` (+ tombstones
//!   `{sessionId, deleted:true}`). Used for tombstones and as a cwd fallback; the walk is the source
//!   of truth because the index lags.
//! - `<root>/workspaces.json` — `{workspaces: {"wd_<slug>_<hash>": {root, name, …}}}`; second cwd
//!   fallback keyed by the workspace dir name.
//! - `<session dir>/state.json` (v2) — `{id: "session_<uuid>", cwd, archived, agents: {main: {type:
//!   "main"}, "agent-N": {type: "sub", parentAgentId, labels}}, title, titleKind ("replaceable" for
//!   the auto title from the first prompt, "custom" after a rename), isCustomTitle, lastPrompt,
//!   createdAt, updatedAt (ms), lastTurnReason}`. **cwd is stored** — no hash inversion needed.
//! - `<session dir>/agents/<agentId>/wire.jsonl` — the transcript of one agent (`main` is the
//!   session; `agent-N` are `Agent`-tool sub-agents). Sidecars: `tool-results/<Tool>-<callId>-<uuid>.txt`
//!   (outputs > ~50k chars, referenced from the truncated result as `output_path: …`), `tasks/<id>/
//!   output.log` (background commands), `media/`, `blobs/`.
//!
//! ## `wire.jsonl` records (`{"type": …, …, "time": <ms>}`, header `{"type":"metadata",
//! "protocol_version", "created_at"}`) → kind / origin
//! - `profile.bind{modelAlias, profileName, thinkingEffort, systemPrompt}` / `config.update{…}` →
//!   the model, plus a `SystemPrompt`/Harness turn holding the system prompt (deduplicated: a re-bind
//!   with the same prompt is bookkeeping) — also `Session::system_prompt`.
//! - `context.append_message{message:{role:"user", content:[Part], origin:{kind}}}` → `origin.kind`
//!   `user` is a typed `Prompt`/Human; `injection` / `system_trigger` are `InjectedContext`/Harness
//!   (steers, reminders); `background_task` / `task` are `InjectedContext`/Scheduler (task
//!   notifications). The raw kind rides in `extra["kimi-code"]["origin_kind"]`.
//!   (`turn.prompt` / `turn.steer` duplicate this text at the UI layer and are not turns.)
//! - `context.append_loop_event{event}` — one LLM step, in this order: `step.begin{uuid, turnId,
//!   step}`, `content.part{stepUuid, part:{type:"think"|"text"}}`…, `tool.call{toolCallId, name,
//!   args:<object>}`…, `tool.result{toolCallId, result:{output, isError?, note?, truncated?}}`…,
//!   `step.end{uuid, usage{inputOther, output, inputCacheRead, inputCacheCreation}, finishReason,
//!   messageId}`. Tool results land BEFORE `step.end` (3614/3614 on this machine), so a step is
//!   buffered whole and flushed at `step.end` (or at the next `step.begin` / EOF for a cancelled
//!   step): one Assistant turn (thinking + text + tool_use blocks, `usage` from `step.end`) followed
//!   by one Tool turn per result. A `think` part with empty text and no encrypted blob (kosong ≥
//!   0.55 writes those) is skipped.
//!   Each step is one `Reply`/Model turn followed by `ToolResult`/Harness turns.
//! - `context.apply_compaction{summary, compactedCount}` → a `CompactionBoundary` turn
//!   (`extra["kimi-code"]["compactedCount"]`) followed by a `CompactionSummary` turn whose text is
//!   the summary that seeds the next window (both origin Harness).
//! - `task.terminated{info{taskId, description, status, exitCode, command}, outputTail}` → a
//!   `Notice`/Harness (background command finished); `turn.cancel` → a `Notice`/Human.
//! - `context.undo{count}` (`/undo`) and `context.clear` → a `Branch`/Human: the undone turns stay
//!   in the log but leave the model's context. (2.x pairs the undo with `agent.switched` and
//!   `context.undone`, both carried.)
//! - 2.x: `subagent.spawned{subagentId, subagentName, parentToolCallId, description, swarmIndex?}`
//!   → `SubagentSpawn`/Harness, `subagent.completed{resultSummary, usage}` / `subagent.failed` →
//!   `SubagentReturn`/Subagent, queued behind the spawning step's reply (the child's usage stays in
//!   the bag; its own transcript counts it). `state.json.forkedFrom` (`kimi fork`) →
//!   `lineage.forked_from`; the fork's wire marks the cut with a carried `forked` record.
//! - 2.x also writes UI/queue twins of the conversation records, all carried: `agent.message.appended`
//!   (a second copy of every message), `agent.turn.started/ended`, `prompt.completed/aborted/steered`,
//!   `turn.step.retrying/interrupted`, `file_history.*` (edit snapshots under `file-history/`).
//! - Everything else (`usage.record`, `llm.request`, `llm.tools_snapshot`, `turn.*`,
//!   `permission.*`, `tools.*`, `token_counting.*`, `task.started`, `plan_mode.*`,
//!   `swarm_mode.*`, `full_compaction.*`, …) is bookkeeping: dropped by the lean passes, carried
//!   verbatim under [`Demand::COMPLETE`] as `Carrier` turns (`_record` = the record,
//!   `extra["kimi-code"]["record_type"]` = its `type`) like the Claude adapter's carriers.
//!
//! Every harness fact lives in `extra["kimi-code"]` (message: `event`, `origin_kind`,
//! `finishReason`, `compactedCount`, `record_type`, and under `Demand::extra` the step
//! telemetry `stepUuid`/`turnId`/`step`/`traceId`/`llm*`; session: `native_id`, `archived`,
//! `lastTurnReason`, `titleKind`, `isCustomTitle`, `lastPrompt`, `agents`, `protocol_version`, and
//! on a sub-agent `agent_id`/`parentAgentId`/`labels`).
//!
//! ### Parts
//! `text{text}` → Text; `think{think, encrypted?}` → Thinking; `image_url{imageUrl:{url}}` (camelCase
//! here, `image_url` in kimi-cli — both accepted) → Image with the `blobref:` url as `data_ref`.
//! A tool result `output` is a string or a list of parts (images come back as parts).
//!
//! ## Ids
//! The cv session id is the bare uuid (`session_` stripped) so `cv show <prefix>` works like every
//! other harness; the native id (`state.json.id`) is kept in `extra["kimi-code"]["native_id"]`.
//! Resume: `kimi --session <id>` (`-S`).
//!
//! ## Sub-agents
//! Like the Claude adapter, [`Adapter::discover`] lists only top-level sessions (the `main` agent's
//! transcript); the other agents are summarized in `extra["kimi-code"]["agents"]` (`{"agent-N":
//! {type, parentAgentId, labels}}`) and the parent's `Agent` tool result names the child (`agent_id:
//! agent-N`). [`subagent_refs`] produces a [`SessionRef`] per sub-agent (id `<uuid>/agent-N`, path
//! `…/agents/agent-N`); parsing one yields its own [`Session`] with `lineage.parent` = the session
//! uuid and `lineage.agent_path` = `agent-N`.

use super::store::{document::Fields, jsonl};
use super::wire::openai;
use super::Adapter;
use crate::ir::CARRIER_KEY;
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::json::{ReadInput, Validation};
use crate::lazy::Text;
use crate::source::{Dependencies, Probe};
use crate::stream::{Demand, Flow, MessageSink, Start, Tier};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct KimiCode {
    /// `$KIMI_CODE_HOME` or `~/.kimi-code`, if it exists.
    root: Option<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: KimiCode = KimiCode { root: None };

impl KimiCode {
    pub fn detect() -> Self {
        let root = crate::env::var_os("KIMI_CODE_HOME")
            .map(PathBuf::from)
            .or_else(|| crate::env::home_dir().map(|h| h.join(".kimi-code")))
            .filter(|p| p.exists());
        KimiCode { root }
    }

    /// An adapter over an explicit Kimi Code home (the dir holding `sessions/`), instead of
    /// `$KIMI_CODE_HOME` / `~/.kimi-code` — for the harness corpus and other tests, without
    /// mutating the process environment.
    pub fn with_home(home: &Path) -> Self {
        KimiCode {
            root: Some(home.to_path_buf()).filter(|p| p.exists()),
        }
    }
}

impl Adapter for KimiCode {
    /// The `origin_kind` the reader keeps: a finished task, a skill activation, a hook's result.
    fn record_affordance(
        &self,
        m: &Message,
        _first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        use crate::affordance::{Kind, RecordHit};
        let origin = m
            .harness_extra(Harness::KimiCode)
            .and_then(|bag| bag.get("origin_kind"))
            .and_then(Value::as_str);
        Ok(match origin.unwrap_or("") {
            "task" | "background_task" => Some(RecordHit {
                kind: Kind::BackgroundJob,
                action: "complete",
                tool: "task",
            }),
            "skill_activation" => Some(RecordHit {
                kind: Kind::Skill,
                action: "load",
                tool: "skill_activation",
            }),
            "hook_result" => Some(RecordHit {
                kind: Kind::Hook,
                action: "output",
                tool: "hook_result",
            }),
            _ => None,
        })
    }

    /// The session's todo state (`tools.update_store`), even when it is empty but present.
    fn session_affordances(&self, s: &Session) -> Vec<crate::affordance::Affordance> {
        use crate::affordance::{item_of, items_summary, session_row, Item, Kind};
        let Some(bag) = s.harness_extra(Harness::KimiCode) else {
            return Vec::new();
        };
        let items: Vec<Item> = bag
            .get("todo")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(item_of)
            .collect();
        if items.is_empty() && bag.get("todo_present").and_then(Value::as_bool) != Some(true) {
            return Vec::new();
        }
        let summary = if items.is_empty() {
            "todo state".into()
        } else {
            items_summary(&items)
        };
        vec![session_row(Kind::Plan, "state", "tools.update_store", summary, items)]
    }

    fn harness(&self) -> Harness {
        Harness::KimiCode
    }

    fn has_subagent_forest(&self) -> bool {
        true
    }

    fn record_forest_dependencies(&self, r: &SessionRef, guard: &mut crate::source::ReadSet) {
        let sdir = r
            .path
            .ancestors()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("session_"))
            })
            .unwrap_or(&r.path);
        guard.record(&sdir.join("state.json"));
        let agents_dir = sdir.join("agents");
        guard.record(&agents_dir);
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                guard.record(&entry.path());
            }
        }
    }

    /// The `agents/agent-N/` transcripts beside the session ([`subagent_refs`]).
    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        Ok(super::ref_forest(subagent_refs(r)?, workflow, select))
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        let (session, agent) = split_agent_path(&r.path);
        deps.files.push(session.join("state.json"));
        deps.files.push(session.join("agents").join(agent).join("wire.jsonl"));
        Some(())
    }

    /// A sub-agent reads its own slice of the session's `state.json`, which the main agent
    /// rewrites every turn: it is certified by that slice ([`state_rows`]).
    fn rows_store(&self, r: &SessionRef) -> Option<PathBuf> {
        let (session, agent) = split_agent_path(&r.path);
        (agent != "main").then(|| session.join("state.json"))
    }

    fn rows(&self, store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
        state_rows(store)
    }

    /// A sub-agent's certificate is keyed by its agent id.
    fn rows_key<'a>(&self, r: &'a SessionRef) -> &'a str {
        split_agent_path(&r.path).1
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let Some(root) = &self.root else {
            return Ok(Default::default());
        };
        // An unreadable index, dir or wire log is skipped and reported; a torn index keeps the
        // tombstones read before the tear.
        let mut out = crate::Discovered::default();
        let (index, index_read) = read_session_index(&root.join("session_index.jsonl"));
        out.keep(&root.join("session_index.jsonl"), index_read);
        let workspaces = read_workspaces(&root.join("workspaces.json"));

        // Walk `sessions/wd_*/session_*/` — the index is append-only and can lag a live session, so
        // the directory tree is the source of truth; the index only contributes tombstones + cwd.
        let mut dirs: Vec<(PathBuf, String, Option<PathBuf>)> = Vec::new();
        let sessions = root.join("sessions");
        let Some(wds) = out.keep(&sessions, fs::read_dir(&sessions).map_err(Into::into)) else {
            return Ok(out);
        };
        for wd in wds {
            let Some(wd) = out.keep(&sessions, wd.map_err(Into::into)) else {
                continue;
            };
            let wd_path = wd.path();
            if !wd_path.is_dir() {
                continue;
            }
            let wd_name = wd.file_name().to_string_lossy().into_owned();
            let Some(sessions) = out.keep(&wd_path, fs::read_dir(&wd_path).map_err(Into::into)) else {
                continue;
            };
            for sd in sessions {
                let Some(sd) = out.keep(&wd_path, sd.map_err(Into::into)) else {
                    continue;
                };
                let sdir = sd.path();
                let name = sd.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_prefix("session_") else {
                    continue;
                };
                if !sdir.is_dir() || index.deleted.contains(&name) {
                    continue;
                }
                if !main_wire(&sdir).exists() && !sdir.join("state.json").exists() {
                    continue;
                }
                let fallback_cwd = index
                    .cwd
                    .get(&name)
                    .cloned()
                    .or_else(|| workspaces.get(&wd_name).cloned());
                dirs.push((sdir, id.to_string(), fallback_cwd));
            }
        }
        // The index and `workspaces.json` supply only the cwd fallback, applied after the cache:
        // in the key, the index's one new line per new session rescanned every session.
        let scanned = crate::par_discovery_filter_map(dirs, |(sdir, id, fallback_cwd)| {
            let mut reads = crate::source::ReadSet::default();
            reads.record(&sdir.join("state.json"));
            let key = main_wire(&sdir);
            reads.record(&key);
            let key = if key.exists() { key } else { sdir.join("state.json") };
            let r = crate::discover_cache::cached_scan_with_reads(&key, reads, || scan(&sdir, &id, None)).map(|r| {
                r.map(|mut r| {
                    r.cwd = r.cwd.or(fallback_cwd);
                    r
                })
            });
            crate::Skipped::listed(&key, r)
        });
        scanned.into_iter().for_each(|found| out.push(found));
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        use anyhow::Context;
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        let (session, agent) = split_agent_path(&r.path);
        agent_wire(session, agent)
    }

    /// An agent's wire log through the driver, after the header its state file gives. Catalog
    /// fallbacks and static sidecar metadata are refreshed rather than kept in a checkpoint; the
    /// caller verifies those dependencies before accepting a suffix.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let ctx = match from {
                Start::Checkpoint(cp) => match jsonl::restore::<ResumeState>(cp) {
                    Some(state) if state.wire == self.resume_source(r) => state.ctx.restore(opts),
                    _ => return Ok(None),
                },
                _ => Ctx::new(opts),
            };
            let ctx = Ctx {
                sidecars: Some(split_agent_path(&r.path).0.join("agents")),
                ..ctx
            };
            let (s, wire) = session_header(r);
            if let Start::Checkpoint(cp) = from {
                if fs::metadata(&wire)?.len() < cp.offset {
                    return Ok(None);
                }
            }
            sink.meta(&s);
            if !wire.is_file() {
                return Ok(Some(s));
            }
            let input = jsonl::Read::open(&wire)?;
            let mut codec = Wire {
                s,
                ctx,
                wire: &wire,
                input: &input,
            };
            codec.ctx.init_model(codec.s.model.as_deref());
            jsonl::drive(&input, from, codec, sink)
        })
    }
}

/// Shared header for whole and suffix reads. No message payload is retained here.
fn session_header(r: &SessionRef) -> (Session, PathBuf) {
    // The ref points at the session dir (the `main` agent) or, from [`subagent_refs`], at one
    // sub-agent's dir (`…/agents/agent-N`); the state file is the session's either way.
    let (session_dir, agent_id) = split_agent_path(&r.path);
    let state = read_state(session_dir);
    let wire = agent_wire(session_dir, agent_id);
    let header = peek_header(&wire);

    let mut s = Session {
        system_prompt: header.system_prompt,
        cwd: r
            .cwd
            .clone()
            .or_else(|| state.get("cwd").and_then(Value::as_str).map(PathBuf::from)),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        // The model is bound by the 2nd record (`profile.bind`); peek at the head so the sink's
        // header knows it before the body streams.
        model: header.model,
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::KimiCode)
    };
    if agent_id == "main" {
        // Session facts live in the harness bag, `extra["kimi-code"]`, never at the top level.
        let bag = s.harness_extra_mut(Harness::KimiCode);
        if let Some(native) = state.get("id").and_then(Value::as_str) {
            // The id as Kimi Code writes it (`session_<uuid>`); cv keys the session by the bare uuid.
            bag.insert("native_id".into(), Value::String(native.to_string()));
        }
        for key in [
            "archived",
            "lastTurnReason",
            "titleKind",
            "isCustomTitle",
            "lastPrompt",
            "forkedFrom",
        ] {
            if let Some(v) = state.get(key).filter(|v| !v.is_null()) {
                bag.insert(key.to_string(), v.clone());
            }
        }
        // Sub-agents (`Agent` tool): everything in `state.json.agents` but `main`.
        if let Some(agents) = state.get("agents").and_then(Value::as_object) {
            let subs: Map<String, Value> = agents
                .iter()
                .filter(|(k, _)| k.as_str() != "main")
                .map(|(k, v)| {
                    let mut a = Map::new();
                    for key in ["type", "parentAgentId", "labels"] {
                        if let Some(x) = v.get(key) {
                            a.insert(key.to_string(), x.clone());
                        }
                    }
                    (k.clone(), Value::Object(a))
                })
                .collect();
            if !subs.is_empty() {
                bag.insert("agents".into(), Value::Object(subs));
            }
        }
        // `kimi fork` (2.x): `state.json.forkedFrom` names the source (`session_<uuid>`).
        s.lineage.forked_from = state
            .get("forkedFrom")
            .and_then(Value::as_str)
            .map(|f| f.strip_prefix("session_").unwrap_or(f).to_string());
    } else {
        // A sub-agent's own session: the parent is the session it lives in (its uuid), the
        // nickname is the agent dir. `parentAgentId` says which agent spawned it (`main`, or
        // another sub-agent for nested spawns); the `Agent` tool call that did so is not
        // recorded on the child's side, so `spawned_by_tool_use` stays unknown.
        s.lineage.parent = Some(session_uuid(session_dir).to_string());
        s.lineage.agent_path = Some(agent_id.to_string());
        let bag = s.harness_extra_mut(Harness::KimiCode);
        bag.insert("agent_id".into(), Value::String(agent_id.to_string()));
        if let Some(a) = state.pointer(&format!("/agents/{agent_id}")) {
            for key in ["parentAgentId", "labels"] {
                if let Some(x) = a.get(key).filter(|x| !x.is_null()) {
                    bag.insert(key.to_string(), x.clone());
                }
            }
        }
    }

    (s, wire)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState<P = PathBuf, C = SavedCtx> {
    wire: P,
    ctx: C,
}

#[derive(serde::Deserialize)]
struct SavedCtx {
    model_bindings: ModelBindings,
    call_names: BTreeMap<String, String>,
    seen_prompts: std::collections::BTreeSet<u64>,
    protocol_version: Option<String>,
    #[serde(default)]
    native_subagents: bool,
    facts: Map<String, Value>,
}
impl SavedCtx {
    fn restore(self, opts: &Demand) -> Ctx<'_> {
        let mut facts = self.facts;
        if opts.tier == Tier::Facts {
            facts.remove("todo");
        }
        Ctx {
            opts,
            step: None,
            model_bindings: self.model_bindings,
            call_names: self.call_names,
            seen_prompts: self.seen_prompts,
            protocol_version: self.protocol_version,
            native_subagents: self.native_subagents,
            facts,
            stopped: false,
            sidecars: None,
        }
    }
}

/// The wire head shared by metadata, header peeking and message projection.
struct WireRecord<'v> {
    fields: Fields<'v>,
    ty: String,
}
impl<'v> WireRecord<'v> {
    fn of(view: JsonView<'v>) -> Result<Option<Self>> {
        let Some(fields) = Fields::parse(
            view,
            &["type", "time", "event", "message", "modelAlias", "kind", "systemPrompt"],
        )?
        else {
            return Ok(None);
        };
        let ty = fields.string("type")?.unwrap_or_default();
        Ok(Some(Self { fields, ty }))
    }
    fn timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self.fields.value("time")?.as_ref().and_then(super::ts_from_value))
    }
    fn message(&self) -> Result<Option<MessageHead<'v>>> {
        self.fields
            .get("message")
            .map(MessageHead::of)
            .transpose()
            .map(Option::flatten)
    }
}
struct MessageHead<'v> {
    fields: Fields<'v>,
    role: Option<String>,
    origin: Option<String>,
}
impl<'v> MessageHead<'v> {
    fn of(view: JsonView<'v>) -> Result<Option<Self>> {
        let Some(fields) = Fields::parse(view, &["role", "origin", "content"])? else {
            return Ok(None);
        };
        let role = fields.string("role")?;
        let origin = fields
            .get("origin")
            .map(|v| v.get("kind"))
            .transpose()?
            .flatten()
            .filter(|v| v.is_string().unwrap_or(false))
            .map(|v| -> Result<String> { Ok(v.resolve()?.as_str().unwrap_or_default().to_owned()) })
            .transpose()?;
        Ok(Some(Self { fields, role, origin }))
    }
}

/// The Kimi Code record codec over one agent's wire log. A pending step is amended by later
/// parts, results and usage (its EOF flush is provisional), so the codec is settled only when
/// no step is held; a checkpoint keeps the log it was taken on.
struct Wire<'r> {
    s: Session,
    ctx: Ctx<'r>,
    wire: &'r Path,
    input: &'r jsonl::Read,
}

impl jsonl::Codec for Wire<'_> {
    fn record(&mut self, view: JsonView<'_>, _: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        self.ctx.record(view, Some(self.input), sink)
    }

    fn settled(&self) -> bool {
        self.ctx.step.is_none()
    }

    fn demand(&self) -> Demand {
        *self.ctx.opts
    }

    fn session(&mut self) -> &mut Session {
        &mut self.s
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        ResumeState {
            wire: self.wire,
            ctx: &self.ctx,
        }
    }

    fn finish(mut self, _: bool, sink: &mut dyn MessageSink) -> Result<Session> {
        if !self.ctx.stopped {
            self.ctx.flush_step(sink);
        }
        let s = &mut self.s;
        s.model = self.ctx.model_bindings.first.take().or(s.model.take());
        s.harness_extra_mut(Harness::KimiCode).append(&mut self.ctx.facts);
        if let Some(pv) = self.ctx.protocol_version.take() {
            s.harness_extra_mut(Harness::KimiCode)
                .insert("protocol_version".into(), Value::String(pv));
        }
        Ok(self.s)
    }
}

/// The session's own transcript: the `main` agent's wire log.
fn main_wire(session_dir: &Path) -> PathBuf {
    agent_wire(session_dir, "main")
}

/// One agent's wire log under a session dir.
fn agent_wire(session_dir: &Path, agent_id: &str) -> PathBuf {
    session_dir.join("agents").join(agent_id).join("wire.jsonl")
}

/// A [`SessionRef::path`] is the session dir (`session_<uuid>`) or, for a sub-agent ref, one agent's
/// dir under it (`session_<uuid>/agents/agent-N`): `(session dir, agent id)`.
fn split_agent_path(path: &Path) -> (&Path, &str) {
    let is_agents_dir = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some("agents");
    match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        (Some(agents), Some(agent)) if is_agents_dir(agents) && agent.starts_with("agent-") => {
            (agents.parent().unwrap_or(path), agent)
        }
        _ => (path, "main"),
    }
}

/// The bare uuid a session dir is keyed by (`session_` stripped).
fn session_uuid(session_dir: &Path) -> &str {
    let name = session_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.strip_prefix("session_").unwrap_or(name)
}

/// One [`SessionRef`] per sub-agent transcript of `main` (a top-level ref from
/// [`Adapter::discover`]): `agents/agent-N/wire.jsonl`, in agent order. Ids are `<uuid>/agent-N`;
/// parsing one gives a session whose `lineage.parent` is the uuid and `lineage.agent_path` the
/// agent id. Sub-agents are not listed by `discover` (like the Claude adapter's `subagents/`).
pub fn subagent_refs(main: &SessionRef) -> Result<Vec<SessionRef>> {
    use anyhow::Context;
    let _cache_scope = crate::discover_cache::read::Context::current().enter();
    let mut out = Vec::new();
    for e in super::side_entries(&main.path.join("agents"))? {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix("agent-").and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let dir = e.path();
        let wire = dir.join("wire.jsonl");
        if !wire
            .try_exists()
            .with_context(|| format!("checking {}", wire.display()))?
        {
            continue;
        }
        let reference = crate::discover_cache::cached_scan_checked(&wire, || {
            let w = scan_wire(&wire)?;
            Ok(Some(SessionRef {
                title: w.title,
                created_at: w.first_ts.or_else(|| file_mtime(&wire)),
                updated_at: w.last_ts.or_else(|| file_mtime(&wire)),
                message_count: None,
                ..SessionRef::new(&name, Harness::KimiCode, &dir)
            }))
        });
        let Some(mut reference) = (match reference {
            Ok(reference) => reference,
            Err(error) if super::is_not_found(&error) => None,
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        reference.id = format!("{}/{name}", main.id);
        reference.cwd.clone_from(&main.cwd);
        out.push((n, reference));
    }
    out.sort_by_key(|(n, _)| *n);
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

/// Each sub-agent's certificate in the session `state.json` at `store`, by agent id: the slice a
/// sub-agent's read takes from it ([`session_header`]), the session's `cwd` (its fallback) and
/// its own `agents.<id>` entry (`parentAgentId`, `labels`), digested as JSON. Every `agent-N` the
/// session holds is certified, listed in the state or not.
fn state_rows(store: &Path) -> Option<HashMap<String, crate::source::Rows>> {
    use crate::source::Rows;
    let session_dir = store.parent()?;
    let state = read_state(session_dir);
    let mut agents: std::collections::BTreeSet<String> = state
        .get("agents")
        .and_then(Value::as_object)
        .map(|agents| agents.keys().filter(|k| k.starts_with("agent-")).cloned().collect())
        .unwrap_or_default();
    if let Ok(entries) = fs::read_dir(session_dir.join("agents")) {
        agents.extend(
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| name.starts_with("agent-")),
        );
    }
    let slice = |agent: &str| serde_json::json!([state.get("cwd"), state.pointer(&format!("/agents/{agent}"))]);
    Some(
        agents
            .into_iter()
            .map(|agent| {
                let digest = Rows::fold(Rows::SEED, &serde_json::to_vec(&slice(&agent)).unwrap_or_default());
                (
                    agent,
                    Rows {
                        count: 1,
                        high: 0,
                        digest,
                    },
                )
            })
            .collect(),
    )
}

fn read_state(session_dir: &Path) -> Value {
    fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Only listing fields are demanded; lastPrompt and other state bodies remain validated ranges.
fn read_discovery_state(session_dir: &Path) -> Value {
    let read = || -> Result<Value> {
        let source = crate::json::JsonSource::capture(&session_dir.join("state.json"))?;
        source.read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
            let fields = super::store::document::Fields::parse(view, &["cwd", "title", "createdAt", "updatedAt"])?;
            let Some(fields) = fields else { return Ok(Value::Null) };
            let mut state = serde_json::Map::new();
            for key in ["cwd", "title", "createdAt", "updatedAt"] {
                if let Some(value) = fields.get(key) {
                    if value.is_string()? || (matches!(key, "createdAt" | "updatedAt") && value.as_number()?.is_some())
                    {
                        state.insert(key.into(), value.resolve()?.into_owned());
                    }
                }
            }
            Ok(Value::Object(state))
        })
    };
    read().unwrap_or(Value::Null)
}

#[derive(Default)]
struct SessionIndex {
    /// Native ids (`session_<uuid>`) tombstoned with `deleted: true`.
    deleted: HashSet<String>,
    /// Native id → `workDir`.
    cwd: HashMap<String, PathBuf>,
}

/// `session_index.jsonl`: an append log, later lines win (a tombstone after an entry deletes it; an
/// entry after a tombstone — a re-created id — revives it).
/// What it read before any failure is kept: a torn tail loses none of the tombstones before it.
fn read_session_index(path: &Path) -> (SessionIndex, Result<()>) {
    let mut idx = SessionIndex::default();
    let read = jsonl::for_each_record(path, |view| {
        let Some(record) = Fields::parse(view, &["sessionId", "deleted", "workDir"])? else {
            return Ok(());
        };
        let Some(id) = record.string("sessionId")? else {
            return Ok(());
        };
        if record.get("deleted").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
            idx.deleted.insert(id.clone());
            idx.cwd.remove(&id);
        } else {
            idx.deleted.remove(&id);
            if let Some(wd) = record.string("workDir")? {
                idx.cwd.insert(id, PathBuf::from(wd));
            }
        }
        Ok(())
    });
    (idx, read.map(drop))
}

/// `workspaces.json`: workspace dir name (`wd_<slug>_<hash>`) → its root path.
fn read_workspaces(path: &Path) -> HashMap<String, PathBuf> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    v.get("workspaces")
        .and_then(Value::as_object)
        .map(|ws| {
            ws.iter()
                .filter_map(|(k, w)| Some((k.clone(), PathBuf::from(w.get("root")?.as_str()?))))
                .collect()
        })
        .unwrap_or_default()
}

/// Native state metadata wins; read wire only for missing titles/timestamps. Exact turn counts
/// are deferred to complete reads or indexing.
#[cfg(test)]
static SCANNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn scan(session_dir: &Path, id: &str, fallback_cwd: Option<PathBuf>) -> Result<Option<SessionRef>> {
    #[cfg(test)]
    SCANNED.lock().unwrap().push(session_dir.to_path_buf());
    let state = read_discovery_state(session_dir);
    let wire = main_wire(session_dir);
    if !wire.exists() && state.is_null() {
        return Ok(None);
    }
    let cwd = state
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or(fallback_cwd);
    let title_needed = state
        .get("title")
        .and_then(Value::as_str)
        .is_none_or(|t| t.trim().is_empty());
    let state_created = state.get("createdAt").and_then(super::ts_from_value);
    let state_updated = state.get("updatedAt").and_then(super::ts_from_value);
    let w = scan_wire_with_title(&wire, title_needed, state_created.is_none() || state_updated.is_none())?;
    let title = state
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or(w.title);

    let created = state_created.or(w.first_ts).or_else(|| file_mtime(&wire));
    let updated = state_updated.or(w.last_ts).or_else(|| file_mtime(&wire));

    Ok(Some(SessionRef {
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count: None,
        ..SessionRef::new(id.to_string(), Harness::KimiCode, session_dir.to_path_buf())
    }))
}

/// What one pass over a wire log yields for a listing.
#[derive(Default)]
struct WireScan {
    /// The first prompt the USER typed (not an injected notice), truncated.
    title: Option<String>,
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
}

/// Listing metadata only; timestamps retain their exact extrema when the native state lacks them.
fn scan_wire(wire: &Path) -> Result<WireScan> {
    scan_wire_with_title(wire, true, true)
}
fn scan_wire_with_title(wire: &Path, title_needed: bool, times_needed: bool) -> Result<WireScan> {
    let mut w = WireScan::default();
    if (!title_needed && !times_needed) || !wire.exists() {
        return Ok(w);
    }
    let input = jsonl::Read::open(wire)?;
    input.lines(0, |line| {
        let Some(view) = line.view else {
            return Ok(Flow::Continue);
        };
        let Some(record) = WireRecord::of(view)? else {
            return Ok(Flow::Continue);
        };
        if times_needed {
            if let Some(ts) = record.timestamp()? {
                w.first_ts = Some(w.first_ts.map_or(ts, |f| f.min(ts)));
                w.last_ts = Some(w.last_ts.map_or(ts, |l| l.max(ts)));
            }
        }
        if title_needed && w.title.is_none() && record.ty == "context.append_message" {
            if let Some(message) = record.message()? {
                if message.role.as_deref() == Some("user") && message.origin.as_deref().unwrap_or("user") == "user" {
                    w.title = super::wire::text_title(message.fields.get("content"))?;
                }
            }
        }
        Ok(if !times_needed && w.title.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        })
    })?;
    Ok(w)
}

use super::file_mtime;

fn loop_request(ty: &str, kind: Option<&str>) -> bool {
    ty == "llm.request" && kind.is_none_or(|kind| kind == "loop")
}

#[derive(Default, Debug, PartialEq, Eq)]
struct HeaderFields {
    model: Option<String>,
    system_prompt: Option<String>,
}

/// The first sixteen accepted records bind both header fields in one checked pass. Decode each
/// requested field only on relevant record kinds; unrelated body/telemetry fields remain views.
fn peek_header(wire: &Path) -> HeaderFields {
    let read = || -> Result<HeaderFields> {
        let input = jsonl::Read::open(wire)?;
        let mut header = HeaderFields::default();
        let mut seen = 0;
        input.lines(0, |line| {
            let Some(view) = line.view else {
                return Ok(Flow::Continue);
            };
            seen += 1;
            if let Some(head) = WireRecord::of(view)? {
                let record = &head.fields;
                let ty = &head.ty;
                if header.model.is_none()
                    && (matches!(ty.as_str(), "profile.bind" | "config.update" | "usage.record")
                        || loop_request(ty, record.string("kind")?.as_deref()))
                {
                    header.model = record.string("modelAlias")?;
                }
                if header.system_prompt.is_none() && matches!(ty.as_str(), "profile.bind" | "config.update") {
                    header.system_prompt = record.string("systemPrompt")?.filter(|prompt| !prompt.is_empty());
                }
            }
            Ok(
                if seen >= 16 || (header.model.is_some() && header.system_prompt.is_some()) {
                    Flow::Stop
                } else {
                    Flow::Continue
                },
            )
        })?;
        Ok(header)
    };
    read().unwrap_or_default()
}

/// One buffered LLM step: the Assistant turn under construction plus the Tool turns that already
/// answered its calls (results precede `step.end` on the wire).
struct StepBuf {
    uuid: String,
    msg: Message,
    results: Vec<Message>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ModelBindings {
    first: Option<String>,
    current: Option<String>,
}

#[derive(serde::Serialize)]
struct Ctx<'a> {
    #[serde(skip)]
    opts: &'a Demand,
    #[serde(skip)]
    step: Option<StepBuf>,
    model_bindings: ModelBindings,
    /// `toolCallId` → tool name, so a result can name its tool (`Block::ToolResult::tool_name`).
    call_names: BTreeMap<String, String>,
    /// Hashes of system prompts already surfaced (a re-bind with the same prompt is bookkeeping).
    /// Ordered so later checkpoints remain identical after restoring this state.
    seen_prompts: std::collections::BTreeSet<u64>,
    protocol_version: Option<String>,
    native_subagents: bool,
    /// Session-level affordance facts read off bookkeeping records: the approval modes
    /// (`permission.set_mode`) and the file-history checkpoints (`file_history.checkpoint`).
    facts: Map<String, Value>,
    #[serde(skip)]
    stopped: bool,
    /// The session's `agents` directory, which an `output_path:` line is resolved inside;
    /// `None` reads no file.
    #[serde(skip)]
    sidecars: Option<PathBuf>,
}

impl<'a> Ctx<'a> {
    fn new(opts: &'a Demand) -> Self {
        Self {
            opts,
            step: None,
            model_bindings: ModelBindings::default(),
            call_names: BTreeMap::new(),
            seen_prompts: std::collections::BTreeSet::new(),
            protocol_version: None,
            native_subagents: false,
            facts: Map::new(),
            stopped: false,
            sidecars: None,
        }
    }
    fn init_model(&mut self, initial: Option<&str>) {
        if self.model_bindings.first.is_none() {
            self.model_bindings.first = initial.map(str::to_owned);
        }
        if self.model_bindings.current.is_none() {
            self.model_bindings.current.clone_from(&self.model_bindings.first);
        }
    }

    fn bind_model(&mut self, model: Option<String>, request: bool) {
        if let Some(model) = model {
            if self.model_bindings.first.is_none() {
                self.model_bindings.first = Some(model.clone());
            }
            self.model_bindings.current = Some(model);
            if request {
                let reply_model = self.reply_model();
                if let Some(step) = self.step.as_mut() {
                    step.msg.model = reply_model;
                }
            }
        }
    }

    /// The model a reply ran under: the binding in force (`profile.bind`, `config.update`, the
    /// step's `llm.request`), stated on every reply whether or not it is still the session's.
    fn reply_model(&self) -> Option<String> {
        self.model_bindings.current.clone()
    }

    /// The Kimi Code record codec: one wire record view in, its messages to `sink`. Every byte
    /// source feeds this one function; `spans` (the captured source, when the demand keeps large
    /// strings in it) chooses only where text, arguments and complete envelopes live. The step
    /// under construction keeps its payloads' source lifetime while it waits for usage/end. Small
    /// bookkeeping records are resolved and handled whole by [`Self::bookkeeping`].
    fn record(&mut self, view: JsonView<'_>, input: Option<&jsonl::Read>, sink: &mut dyn MessageSink) -> Result<Flow> {
        let spans = input.filter(|_| self.opts.spans).map(jsonl::Read::source);
        let Some(head) = WireRecord::of(view)? else {
            return Ok(if self.opts.tier == Tier::Complete {
                self.bookkeeping(view.resolve()?.as_ref(), sink)
            } else {
                Flow::Continue
            });
        };
        let record = &head.fields;
        let ty = &head.ty;
        if ty == "tools.update_store" {
            if let Some(fields) = Fields::parse(view, &["key", "value"])? {
                if fields.string("key")?.as_deref() == Some("todo") {
                    if let Some(values) = fields.get("value").map(JsonView::array).transpose()?.flatten() {
                        if self.opts.tier == Tier::Facts {
                            let mut present = false;
                            for value in values {
                                if crate::affordance::item_present(value?)? {
                                    present = true;
                                    break;
                                }
                            }
                            self.facts.remove("todo");
                            self.facts.insert("todo_present".into(), Value::Bool(present));
                        } else {
                            let items: Vec<crate::affordance::Item> = values
                                .map(|value| crate::affordance::item_of_view(value?))
                                .filter_map(|item| item.transpose())
                                .collect::<Result<_>>()?;
                            self.facts.insert("todo_present".into(), Value::Bool(!items.is_empty()));
                            self.facts.insert("todo".into(), serde_json::to_value(items)?);
                        }
                    }
                }
            }
            if self.opts.tier != Tier::Complete {
                return Ok(Flow::Continue);
            }
        }
        if self.opts.tier != Tier::Complete && matches!(ty.as_str(), "llm.request" | "usage.record") {
            let loop_request = loop_request(ty, record.string("kind")?.as_deref());
            if ty == "usage.record" || loop_request {
                self.bind_model(record.string("modelAlias")?, loop_request);
            }
            return Ok(Flow::Continue);
        }
        let mut text = |v: JsonView<'_>| {
            if spans.is_some() {
                v.source_text()
            } else {
                v.text(false)
            }
        };
        let ts = head.timestamp()?;
        if ty == "context.append_message" {
            let message = head.message()?;
            let blocks = openai::blocks_from_view(
                message.as_ref().and_then(|m| m.fields.get("content")),
                &openai::KIMI_CODE,
                &mut text,
            )?;
            let role = message.as_ref().and_then(|m| m.role.as_deref());
            let origin = message.as_ref().and_then(|m| m.origin.as_deref());
            let raw = match (self.opts.tier == Tier::Complete, spans) {
                (false, _) => None,
                (true, Some(_)) => Some(view.to_json()?),
                (true, None) => Some(crate::Json::Owned(view.resolve()?.into_owned())),
            };
            let flow = self.append_message(role, origin.unwrap_or("user"), ts, sink, blocks, raw);
            return Ok(if flow == Flow::Stop { self.stop() } else { flow });
        }
        if ty != "context.append_loop_event" {
            // Bookkeeping has no semantic output in lazy passes and need not become a Value.
            if self.opts.tier != Tier::Complete
                && !matches!(
                    ty.as_str(),
                    "permission.set_mode"
                        | "file_history.checkpoint"
                        | "metadata"
                        | "profile.bind"
                        | "config.update"
                        | "context.apply_compaction"
                        | "task.terminated"
                        | "subagent.spawned"
                        | "subagent.completed"
                        | "subagent.failed"
                        | "context.undo"
                        | "context.clear"
                        | "turn.cancel"
                )
            {
                return Ok(Flow::Continue);
            }
            return Ok(self.bookkeeping(view.resolve()?.as_ref(), sink));
        }
        let Some(event) = record.get("event") else {
            return Ok(Flow::Continue);
        };
        let Some(fields) = Fields::parse(
            event,
            &[
                "type",
                "stepUuid",
                "part",
                "toolCallId",
                "name",
                "args",
                "result",
                "traceId",
                "turnId",
                "step",
            ],
        )?
        else {
            return Ok(if self.opts.tier == Tier::Complete {
                self.carry(view.resolve()?.as_ref(), ts, sink)
            } else {
                Flow::Continue
            });
        };
        let event_type = fields.string("type")?.unwrap_or_default();
        let flow = match event_type.as_str() {
            "content.part" | "tool.call" => {
                let uuid = fields.string("stepUuid")?.unwrap_or_default();
                let metadata = Value::Object(fields.project(&["stepUuid", "turnId", "step"])?);
                // The record's `part` is still read from the mapping after a flushing callback: a
                // slow one's backing check (the driver's guard) stops the read here.
                let flow = self.ensure_step(&uuid, &metadata, ts, sink);
                if flow == Flow::Stop {
                    return Ok(self.stop());
                }
                if event_type == "content.part" {
                    if let Some(part) = fields.get("part") {
                        if let Some(block) = openai::part_from_view(part, &openai::KIMI_CODE, &mut text)? {
                            self.step
                                .as_mut()
                                .expect("ensure_step installed the current step")
                                .msg
                                .content
                                .push(block);
                        }
                    }
                } else {
                    let id = fields.string("toolCallId")?.unwrap_or_default();
                    let name = fields.string("name")?.unwrap_or_default();
                    self.call_names.insert(id.clone(), name.clone());
                    let input = fields
                        .get("args")
                        .map(|v| v.argument(spans.is_some()))
                        .transpose()?
                        .unwrap_or_else(|| Value::Null.into());
                    self.step
                        .as_mut()
                        .expect("ensure_step installed the current step")
                        .msg
                        .content
                        .push(Block::ToolUse {
                            id,
                            name,
                            input,
                            namespace: None,
                            extensions: Default::default(),
                        });
                }
                Flow::Continue
            }
            "tool.result" => {
                let message = self.tool_result(event, &fields, ts, spans)?;
                if let Some(step) = &mut self.step {
                    step.results.push(message);
                    Flow::Continue
                } else {
                    sink.message(message)
                }
            }
            "step.begin" | "step.end" => {
                const KEYS: &[&str] = &[
                    "type",
                    "uuid",
                    "turnId",
                    "step",
                    "messageId",
                    "finishReason",
                    "llmFirstTokenLatencyMs",
                    "llmStreamDurationMs",
                    "traceId",
                    "rawFinishReason",
                ];
                let metadata = Fields::parse(event, KEYS)?.unwrap();
                let mut metadata = metadata.project(KEYS)?;
                if let Some(usage) = event.get("usage")? {
                    const TOKENS: &[&str] = &["inputOther", "output", "inputCacheRead", "inputCacheCreation"];
                    let usage = Fields::parse(usage, TOKENS)?
                        .map(|fields| fields.project(TOKENS))
                        .transpose()?
                        .map(Value::Object)
                        .unwrap_or(Value::Null);
                    metadata.insert("usage".into(), usage);
                }
                self.step_event(&Value::Object(metadata), ts, sink)
            }
            _ if self.opts.tier == Tier::Complete => self.carry(view.resolve()?.as_ref(), ts, sink),
            _ => Flow::Continue,
        };
        Ok(if flow == Flow::Stop { self.stop() } else { flow })
    }

    /// A `tool.result` loop event as its Tool turn. The `<system>…</system>` note, the truncation
    /// flag, and — when the full output went to a sidecar (`tool-results/…txt`,
    /// `tasks/<id>/output.log`) — its path ride in `details`, under the same
    /// `details.persistedOutput` key the Claude adapter uses so `cv show` points at it and the
    /// indexer reads it. A part-list output is flattened (images as `[image]`); under spans into
    /// the source's decoded backing, never owned.
    fn tool_result(
        &self,
        event: JsonView<'_>,
        fields: &Fields<'_>,
        ts: Option<DateTime<Utc>>,
        spans: Option<&JsonSource>,
    ) -> Result<Message> {
        let id = fields.string("toolCallId")?.unwrap_or_default();
        let result = fields
            .get("result")
            .map(|v| Fields::parse(v, &["output", "isError", "note", "truncated"]))
            .transpose()?
            .flatten();
        let output = result.as_ref().and_then(|r| r.get("output"));
        let (content, path) = match output {
            Some(output) if output.is_string()? => {
                let text = match spans {
                    Some(_) => output.source_text()?,
                    None => output.text(false)?,
                };
                (text.unwrap(), output_path_view(output)?)
            }
            Some(output) if output.array()?.is_some() => {
                let parts = || -> Result<Vec<Option<JsonView<'_>>>> {
                    // `None` is an image part's `[image]` marker.
                    let mut out = Vec::new();
                    for part in output.array()?.unwrap() {
                        let Some(fields) = Fields::parse(part?, &["type", "text"])? else {
                            continue;
                        };
                        if fields.string("type")?.is_some_and(|ty| super::wire::is_image_part(&ty)) {
                            out.push(None);
                        } else if let Some(text) = fields.get("text").filter(|v| v.is_string().unwrap_or(false)) {
                            out.push(Some(text));
                        }
                    }
                    Ok(out)
                };
                match spans {
                    None => {
                        let mut joined = Vec::new();
                        for part in parts()? {
                            joined.push(match part {
                                Some(text) => text.resolve()?.as_str().unwrap_or_default().to_owned(),
                                None => "[image]".to_owned(),
                            });
                        }
                        let joined = joined.join("\n");
                        let path = output_path(&joined);
                        (Text::Inline(joined), path)
                    }
                    // One arena per wire capture keeps deferred descriptor registries bounded.
                    Some(source) => {
                        let content = super::store::text::append(source, &source.decoded_backing()?, |writer| {
                            for (i, part) in parts()?.into_iter().enumerate() {
                                if i != 0 {
                                    writer.push("\n")?;
                                }
                                match part {
                                    Some(text) => {
                                        writer.push_string(text)?;
                                    }
                                    None => writer.push("[image]")?,
                                }
                            }
                            Ok(())
                        })?;
                        let path = match &content {
                            Text::Inline(text) => output_path(text),
                            Text::Source(text) => text.range.with_view(output_path_view)?,
                            Text::Span(_) => unreachable!("generated text has no external span"),
                        };
                        (content, path)
                    }
                }
            }
            _ => (Text::default(), None),
        };
        let mut details = Map::new();
        if let Some(result) = &result {
            if let Some(note) = result.value("note")?.filter(|v| !v.is_null()) {
                details.insert("note".into(), note);
            }
            if result.value("truncated")? == Some(Value::Bool(true)) {
                details.insert("truncated".into(), Value::Bool(true));
            }
        }
        if let Some(claimed) = path {
            // `<session>/agents/<agent>/tool-results/<file>`, resolved inside this session only.
            let resolved = self.sidecars.as_deref().and_then(|agents| {
                let path = crate::source::contained_sidecar(agents, &claimed)?;
                let inside: Vec<_> = path.strip_prefix(agents).ok()?.components().collect();
                (inside.len() == 3 && inside[1].as_os_str() == "tool-results").then_some(path)
            });
            details.insert(
                "persistedOutput".into(),
                Block::persisted_output(resolved, &claimed, None),
            );
        }
        let mut message = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        if !self.native_subagents
            && matches!(
                self.call_names.get(&id).map(String::as_str),
                Some("Agent" | "AgentSwarm")
            )
        {
            let resolver = crate::lazy::Resolver::new(None);
            let head = content.try_resolve_prefix(&resolver, crate::ir::TITLE_HEAD)?;
            let header = head.split("\n\n").next().unwrap_or("");
            if header.starts_with("agent_id:")
                && header
                    .lines()
                    .any(|line| matches!(line, "status: completed" | "status: failed"))
            {
                message.kind = MessageKind::SubagentReturn;
                message.origin = Origin::Subagent;
            }
        }
        message.timestamp = ts;
        message.content.push(Block::ToolResult {
            tool_use_id: id.clone(),
            content,
            is_error: result
                .as_ref()
                .map(|r| r.value("isError"))
                .transpose()?
                .flatten()
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tool_name: self.call_names.get(&id).cloned(),
            status: None,
            details: (!details.is_empty()).then(|| Value::Object(details)),
            extensions: Default::default(),
        });
        if self.opts.tier >= Tier::Facts {
            if let Some(trace) = fields.value("traceId")? {
                message
                    .harness_extra_mut(Harness::KimiCode)
                    .insert("traceId".into(), trace);
            }
        }
        if self.opts.tier == Tier::Complete {
            let time = ts.map(|t| t.timestamp_millis());
            match spans {
                Some(source) => {
                    let arena = source.decoded_backing()?;
                    let (offset, len) = arena.append(|writer| {
                        use crate::harness::wire::write::ObjectWriter;
                        let mut out = ObjectWriter::new(writer)?;
                        out.field("type", "context.append_loop_event")?;
                        out.field_with("event", |mut writer| event.write_canonical(&mut writer))?;
                        out.field("time", time)?;
                        out.finish()
                    })?;
                    let raw = source.read(ReadInput::Backing(arena, offset, len), Validation::Syntax, |view| {
                        view.to_json()
                    })?;
                    CompleteRecord::attach(std::slice::from_mut(&mut message), Harness::KimiCode, raw, Map::new());
                }
                None => {
                    message.extra.insert(
                        CARRIER_KEY.into(),
                        serde_json::json!({"type": "context.append_loop_event", "event": event.resolve()?, "time": time}),
                    );
                }
            }
        }
        Ok(message)
    }

    /// A small bookkeeping record, whole: model bindings, session facts, notices, compaction and
    /// branch markers, sub-agent lives, and carriers for everything else.
    fn bookkeeping(&mut self, v: &Value, sink: &mut dyn MessageSink) -> Flow {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = v.get("time").and_then(super::ts_from_value);
        let loop_request = loop_request(ty, v.get("kind").and_then(Value::as_str));
        if loop_request || matches!(ty, "profile.bind" | "config.update" | "usage.record") {
            self.bind_model(
                v.get("modelAlias").and_then(Value::as_str).map(str::to_owned),
                loop_request,
            );
        }
        match ty {
            "permission.set_mode" => {
                let mode = v.get("mode").and_then(Value::as_str).unwrap_or("");
                crate::affordance::note_mode(&mut self.facts, mode);
            }
            "file_history.checkpoint" => {
                let files = v.get("entries").and_then(Value::as_object);
                let files = files.into_iter().flat_map(|e| e.keys().map(String::as_str));
                // One point per turn: a `start` phase, should one be written, opens the `end`'s.
                let points = u64::from(v.get("phase").and_then(Value::as_str) != Some("start"));
                crate::affordance::note_checkpoints(&mut self.facts, points, files);
            }
            _ => {}
        }
        let flow = match ty {
            "metadata" => {
                if let Some(pv) = v.get("protocol_version").and_then(Value::as_str) {
                    self.protocol_version = Some(pv.to_string());
                }
                self.carry(v, ts, sink)
            }
            "profile.bind" | "config.update" => self.system_prompt(v, ty, ts, sink),
            "context.apply_compaction" => {
                let f = self.flush_step(sink);
                if f == Flow::Stop {
                    return self.stop();
                }
                // Two turns, as the kinds vocabulary has it: the boundary (where the harness cut
                // the context; the record carries no text of its own, so neither does the turn),
                // then the summary that seeds the next window, when there is one.
                let mut boundary = note(String::new(), ts, MessageKind::CompactionBoundary, Origin::Harness, ty);
                if let Some(n) = v.get("compactedCount") {
                    boundary
                        .harness_extra_mut(Harness::KimiCode)
                        .insert("compactedCount".into(), n.clone());
                }
                if sink.message(boundary) == Flow::Stop {
                    return self.stop();
                }
                let summary = v.get("summary").and_then(Value::as_str).unwrap_or("");
                if summary.trim().is_empty() {
                    Flow::Continue
                } else {
                    sink.message(note(
                        summary.to_string(),
                        ts,
                        MessageKind::CompactionSummary,
                        Origin::Harness,
                        ty,
                    ))
                }
            }
            "task.terminated" => {
                let info = v.get("info").cloned().unwrap_or(Value::Null);
                let desc = info
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("background task");
                let status = info.get("status").and_then(Value::as_str).unwrap_or("ended");
                let exit = info
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .map(|c| format!(", exit {c}"))
                    .unwrap_or_default();
                let mut m = note(
                    format!("⏱ background task {status}: {desc}{exit}"),
                    ts,
                    MessageKind::Notice,
                    Origin::Harness,
                    ty,
                );
                if self.opts.tier >= Tier::Facts {
                    let bag = m.harness_extra_mut(Harness::KimiCode);
                    bag.insert("task".into(), info);
                    if let Some(tail) = v.get("outputTail") {
                        bag.insert("outputTail".into(), tail.clone());
                    }
                }
                sink.message(m)
            }
            "subagent.spawned" | "subagent.completed" | "subagent.failed" => {
                self.native_subagents = true;
                // Kimi Code 2.x records a sub-agent's life on the parent's wire (0.x left only the
                // `Agent` call and its result). The records land inside the spawning step, so they
                // queue behind its tool results; the child's own transcript is `agents/<id>/`.
                let id = v.get("subagentId").and_then(Value::as_str).unwrap_or("?");
                let (kind, origin, text) = match ty {
                    "subagent.spawned" => {
                        let name = v.get("subagentName").and_then(Value::as_str).unwrap_or("agent");
                        let desc = v.get("description").and_then(Value::as_str).unwrap_or("");
                        (
                            MessageKind::SubagentSpawn,
                            Origin::Harness,
                            format!("[sub-agent {id} ({name}) spawned] {desc}"),
                        )
                    }
                    "subagent.completed" => {
                        let summary = v.get("resultSummary").and_then(Value::as_str).unwrap_or("");
                        (
                            MessageKind::SubagentReturn,
                            Origin::Subagent,
                            format!("[sub-agent {id} returned] {summary}"),
                        )
                    }
                    _ => (
                        MessageKind::SubagentReturn,
                        Origin::Subagent,
                        format!("[sub-agent {id} failed]"),
                    ),
                };
                let mut m = note(text, ts, kind, origin, ty);
                let bag = m.harness_extra_mut(Harness::KimiCode);
                // The child's `usage` stays in the bag: its own transcript already counts it.
                for key in [
                    "usage",
                    "subagentId",
                    "parentToolCallId",
                    "parentAgentId",
                    "swarmIndex",
                    "runInBackground",
                ] {
                    if let Some(x) = v.get(key).filter(|x| !x.is_null()) {
                        bag.insert(key.to_string(), x.clone());
                    }
                }
                match self.step.as_mut() {
                    Some(step) => {
                        step.results.push(m);
                        Flow::Continue
                    }
                    None => sink.message(m),
                }
            }
            "context.undo" | "context.clear" => {
                let f = self.flush_step(sink);
                if f == Flow::Stop {
                    return self.stop();
                }
                // `/undo` drops the last `count` turns from the model's context (they stay in the
                // log); `context.clear` empties it. What follows does not continue what precedes.
                let count = v.get("count").and_then(Value::as_u64);
                let text = match (ty, count) {
                    ("context.undo", Some(n)) => format!("[undo: the last {n} turn(s) left the context]"),
                    ("context.undo", None) => "[undo]".to_string(),
                    _ => "[context cleared]".to_string(),
                };
                let mut m = note(text, ts, MessageKind::Branch, Origin::Human, ty);
                if let Some(n) = count {
                    m.harness_extra_mut(Harness::KimiCode).insert("count".into(), n.into());
                }
                sink.message(m)
            }
            "turn.cancel" => {
                let f = self.flush_step(sink);
                if f == Flow::Stop {
                    return self.stop();
                }
                // The person cancelled the turn: a notice, of human origin.
                sink.message(note(
                    "[turn cancelled]".into(),
                    ts,
                    MessageKind::Notice,
                    Origin::Human,
                    ty,
                ))
            }
            _ => self.carry(v, ts, sink),
        };
        if flow == Flow::Stop {
            self.stop()
        } else {
            flow
        }
    }

    fn stop(&mut self) -> Flow {
        self.stopped = true;
        Flow::Stop
    }

    /// Bookkeeping record: carried verbatim under `complete`, dropped otherwise.
    fn carry(&mut self, v: &Value, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        if self.opts.tier != Tier::Complete {
            return Flow::Continue;
        }
        let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
        m.timestamp = ts;
        m.extra.insert(CARRIER_KEY.into(), v.clone());
        if let Some(ty) = v.get("type").and_then(Value::as_str) {
            m.harness_extra_mut(Harness::KimiCode)
                .insert("record_type".into(), Value::String(ty.to_string()));
        }
        sink.message(m)
    }

    fn system_prompt(&mut self, v: &Value, ty: &str, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        let Some(prompt) = v.get("systemPrompt").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
            return self.carry(v, ts, sink);
        };
        let hash = crate::source::digest(prompt.as_bytes());
        if !self.seen_prompts.insert(hash) {
            return self.carry(v, ts, sink);
        }
        let mut m = Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness);
        if self.opts.tier == Tier::Complete {
            m.extra.insert(CARRIER_KEY.into(), v.clone());
        }
        m.timestamp = ts;
        m.content.push(Block::from_text(Text::from(prompt)));
        let bag = m.harness_extra_mut(Harness::KimiCode);
        bag.insert("event".into(), Value::String(ty.to_string()));
        if self.opts.tier >= Tier::Facts {
            for key in ["modelAlias", "profileName", "thinkingEffort"] {
                if let Some(x) = v.get(key) {
                    bag.insert(key.to_string(), x.clone());
                }
            }
        }
        sink.message(m)
    }

    /// A conversation message: its role, `origin.kind` (who put the text there: `user` is a typed
    /// prompt; `injection` / `system_trigger` are harness steers; `background_task` / `task` are
    /// automation notices), blocks, and under `complete` its record.
    fn append_message(
        &mut self,
        role: Option<&str>,
        origin_kind: &str,
        ts: Option<DateTime<Utc>>,
        sink: &mut dyn MessageSink,
        blocks: Vec<Block>,
        record: Option<crate::Json>,
    ) -> Flow {
        let f = self.flush_step(sink);
        if f == Flow::Stop {
            return Flow::Stop;
        }
        let role = match role {
            Some("assistant") => Role::Assistant,
            Some("tool") => Role::Tool,
            Some("system") => Role::System,
            _ => Role::User,
        };
        let (kind, origin) = match role {
            Role::User => match origin_kind {
                "user" => (MessageKind::Prompt, Origin::Human),
                "background_task" | "task" => (MessageKind::InjectedContext, Origin::Scheduler),
                _ => (MessageKind::InjectedContext, Origin::Harness),
            },
            Role::Assistant => (MessageKind::Reply, Origin::Model),
            Role::Tool => (MessageKind::ToolResult, Origin::Harness),
            Role::System => (MessageKind::InjectedContext, Origin::Harness),
        };
        let mut m = Message::of_kind(role, kind, origin);
        if role == Role::Assistant {
            m.model = self.reply_model();
        }
        m.timestamp = ts;
        m.content = blocks;
        m.harness_extra_mut(Harness::KimiCode)
            .insert("origin_kind".into(), Value::String(origin_kind.to_string()));
        match record {
            Some(crate::Json::Owned(record)) => {
                m.extra.insert(CARRIER_KEY.into(), record);
            }
            Some(record) => CompleteRecord::attach(std::slice::from_mut(&mut m), Harness::KimiCode, record, Map::new()),
            None => {}
        }
        sink.message(m)
    }

    /// A `step.begin` / `step.end` loop event, its metadata projected (`e`).
    fn step_event(&mut self, e: &Value, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        let ety = e.get("type").and_then(Value::as_str).unwrap_or("");
        match ety {
            "step.begin" => {
                let f = self.flush_step(sink);
                if f == Flow::Stop {
                    return Flow::Stop;
                }
                let uuid = e.get("uuid").and_then(Value::as_str).unwrap_or("").to_string();
                self.open_step(uuid, e, ts);
                Flow::Continue
            }
            "step.end" => {
                let uuid = e.get("uuid").and_then(Value::as_str).unwrap_or("");
                let f = self.ensure_step(uuid, e, ts, sink);
                if f == Flow::Stop {
                    return Flow::Stop;
                }
                if let Some(step) = self.step.as_mut() {
                    if let Some(u) = e.get("usage") {
                        step.msg.usage = Some(usage_from(u));
                    }
                    if let Some(mid) = e.get("messageId").and_then(Value::as_str) {
                        step.msg.id = Some(mid.to_string());
                    }
                    let bag = step.msg.harness_extra_mut(Harness::KimiCode);
                    if let Some(fr) = e.get("finishReason") {
                        bag.insert("finishReason".into(), fr.clone());
                    }
                    if self.opts.tier >= Tier::Facts {
                        for key in [
                            "llmFirstTokenLatencyMs",
                            "llmStreamDurationMs",
                            "traceId",
                            "rawFinishReason",
                        ] {
                            if let Some(x) = e.get(key) {
                                bag.insert(key.to_string(), x.clone());
                            }
                        }
                    }
                }
                self.flush_step(sink)
            }
            _ => Flow::Continue,
        }
    }

    fn open_step(&mut self, uuid: String, e: &Value, ts: Option<DateTime<Utc>>) {
        let mut msg = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
        msg.model = self.reply_model();
        msg.id = Some(uuid.clone());
        msg.timestamp = ts;
        if self.opts.tier >= Tier::Facts {
            let bag = msg.harness_extra_mut(Harness::KimiCode);
            bag.insert("stepUuid".into(), Value::String(uuid.clone()));
            for key in ["turnId", "step"] {
                if let Some(x) = e.get(key) {
                    bag.insert(key.to_string(), x.clone());
                }
            }
        }
        self.step = Some(StepBuf {
            uuid,
            msg,
            results: Vec::new(),
        });
    }

    /// Make sure the step `uuid` is the open one: a part/call/end for a different step means the
    /// previous step never got its `step.end` (cancelled turn) — flush it and open the new one.
    fn ensure_step(&mut self, uuid: &str, e: &Value, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        if self.step.as_ref().is_some_and(|s| s.uuid == uuid) {
            return Flow::Continue;
        }
        let f = self.flush_step(sink);
        if f == Flow::Stop {
            return Flow::Stop;
        }
        self.open_step(uuid.to_string(), e, ts);
        Flow::Continue
    }

    /// Emit the buffered step: the Assistant turn, then its Tool turns (in wire order).
    fn flush_step(&mut self, sink: &mut dyn MessageSink) -> Flow {
        let Some(mut step) = self.step.take() else {
            return Flow::Continue;
        };
        if self.native_subagents {
            // Native markers queued in this step are authoritative, including when a tool result
            // arrived before its marker. Do not emit a second inferred return for the same run.
            for result in &mut step.results {
                if result.role == Role::Tool && result.kind == MessageKind::SubagentReturn {
                    result.kind = MessageKind::ToolResult;
                    result.origin = Origin::Harness;
                }
            }
        } else if step.msg.content.iter().any(
            |block| matches!(block, Block::ToolUse { name, .. } if matches!(name.as_str(), "Agent" | "AgentSwarm")),
        ) {
            step.msg.kind = MessageKind::SubagentSpawn;
        }
        if (!step.msg.content.is_empty() || step.msg.usage.is_some()) && sink.message(step.msg) == Flow::Stop {
            return Flow::Stop;
        }
        for r in step.results {
            if sink.message(r) == Flow::Stop {
                return Flow::Stop;
            }
        }
        Flow::Continue
    }
}

/// `output_path: <path>`: the line a truncated tool output ends with when the whole output was
/// written to a sidecar file. Scans decoded chunks keeping only a candidate line; the last
/// matching line wins (an empty one means none).
#[derive(Default)]
struct OutputPath {
    prefix: Option<usize>,
    candidate: Option<String>,
    last: Option<String>,
}
impl OutputPath {
    const PREFIX: &'static str = "output_path:";
    fn push(&mut self, chunk: &str) {
        for segment in chunk.split_inclusive('\n') {
            let complete = segment.ends_with('\n');
            let mut text = if complete {
                &segment[..segment.len() - 1]
            } else {
                segment
            };
            if let Some(matched) = self.prefix {
                let n = text.len().min(Self::PREFIX.len() - matched);
                if text.as_bytes()[..n] == Self::PREFIX.as_bytes()[matched..matched + n] {
                    self.prefix = Some(matched + n);
                    text = &text[n..];
                    if matched + n == Self::PREFIX.len() {
                        self.candidate.get_or_insert_with(String::new);
                    }
                } else {
                    self.prefix = None;
                }
            }
            if let Some(line) = &mut self.candidate {
                line.push_str(text);
            }
            if complete {
                if let Some(line) = self.candidate.take() {
                    self.last = Some(line);
                }
                self.prefix = Some(0);
            }
        }
    }
    fn finish(mut self) -> Option<String> {
        if let Some(line) = self.candidate.take() {
            self.last = Some(line);
        }
        self.last
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
    }
}
fn output_path_view(output: JsonView<'_>) -> Result<Option<String>> {
    let mut scan = OutputPath {
        prefix: Some(0),
        ..OutputPath::default()
    };
    output.for_each_str_chunk(|chunk| {
        scan.push(chunk);
        Ok(())
    })?;
    Ok(scan.finish())
}
fn output_path(text: &str) -> Option<String> {
    let mut scan = OutputPath {
        prefix: Some(0),
        ..OutputPath::default()
    };
    scan.push(text);
    scan.finish()
}

/// A harness-side System turn of the given kind and origin, with the wire record type that
/// produced it in `extra["kimi-code"]["event"]`. An empty `text` is no block at all.
fn note(text: String, ts: Option<DateTime<Utc>>, kind: MessageKind, origin: Origin, event: &str) -> Message {
    let mut m = Message::of_kind(Role::System, kind, origin);
    m.timestamp = ts;
    if !text.is_empty() {
        m.content.push(Block::from_text(Text::from(text)));
    }
    m.harness_extra_mut(Harness::KimiCode)
        .insert("event".into(), Value::String(event.to_string()));
    m
}

fn usage_from(u: &Value) -> Usage {
    let get = |k: &str| u.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: get("inputOther"),
        output_tokens: get("output"),
        cache_read_tokens: get("inputCacheRead"),
        cache_creation_tokens: get("inputCacheCreation"),
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use crate::stream::Checkpoint;
    use serde_json::json;

    #[test]
    fn todo_updates_keep_latest_state_and_facts_presence() -> Result<()> {
        use crate::affordance::{self, Kind};
        for opts in [
            Demand::RICH,
            Demand::LEAN.with_spans(),
            Demand::FACTS.with_spans(),
            Demand::COMPLETE,
        ] {
            let mut ctx = Ctx::new(&opts);
            let mut sink = crate::stream::CollectSink::default();
            for status in ["pending", "done"] {
                let value = serde_json::json!({"type":"tools.update_store","key":"todo","value":[{"title":"read a.txt","status":status}]});
                ctx.record(JsonView::from(&value), None, &mut sink)?;
            }
            let irrelevant =
                serde_json::json!({"type":"tools.update_store","key":"unrelated","value":"x".repeat(128*1024)});
            ctx.record(JsonView::from(&irrelevant), None, &mut sink)?;
            let mut session = Session::new("todo", Harness::KimiCode);
            session.harness_extra_mut(Harness::KimiCode).extend(ctx.facts.clone());
            let rows = affordance::session_rows(&session);
            assert_eq!(rows.len(), 1);
            assert_eq!((rows[0].kind, rows[0].action), (Kind::Plan, "state"));
            if opts.tier == Tier::Facts {
                assert!(
                    !ctx.facts.contains_key("todo"),
                    "reducers retain presence, not arbitrary-size plan text"
                );
            } else {
                assert_eq!(rows[0].items[0].status.as_deref(), Some("done"));
            }
            let counter = affordance::Counter::new(&SessionRef::new("todo", Harness::KimiCode, "unused"));
            assert_eq!(counter.finish(&session, &[])[&Kind::Plan], 1);
            let clear = serde_json::json!({"type":"tools.update_store","key":"todo","value":[]});
            ctx.record(JsonView::from(&clear), None, &mut sink)?;
            session.harness_extra_mut(Harness::KimiCode).extend(ctx.facts);
            assert!(affordance::session_rows(&session).is_empty());
        }
        Ok(())
    }

    #[test]
    fn legacy_agent_calls_infer_spawn_and_completed_returns() -> Result<()> {
        for status in ["completed", "failed", "running"] {
            let opts = Demand::RICH;
            let mut ctx = Ctx::new(&opts);
            let mut sink = crate::stream::CollectSink::default();
            let records = [
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"step","turnId":"0","step":1}}),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"call","stepUuid":"step","toolCallId":"agent-call","name":"Agent","args":{}}}),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"agent-call","result":{"output":format!("agent_id: agent-0\nactual_subagent_type: coder\nstatus: {status}\n\nanswer")}}}),
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"step","turnId":"0","step":1}}),
            ];
            for record in records {
                ctx.record(JsonView::from(&record), None, &mut sink)?;
            }
            assert_eq!(sink.messages[0].kind, MessageKind::SubagentSpawn);
            assert_eq!(
                sink.messages[1].kind,
                if status == "running" {
                    MessageKind::ToolResult
                } else {
                    MessageKind::SubagentReturn
                }
            );
            let mut session = Session::new("agents", Harness::KimiCode);
            session.messages = sink.messages;
            let rows = crate::affordance::of_session(&session)?;
            assert_eq!(
                rows.iter()
                    .filter(|r| r.kind == crate::affordance::Kind::SubagentSpawn)
                    .count(),
                1
            );
            assert_eq!(
                rows.iter()
                    .filter(|r| r.kind == crate::affordance::Kind::SubagentReturn)
                    .count(),
                usize::from(status != "running")
            );
        }
        Ok(())
    }

    #[test]
    fn model_bindings_follow_native_request_order_and_survive_every_suffix() -> Result<()> {
        let world = crate::test_world("kimi_code_model_bindings");
        let home = world.home.join(".kimi-code");
        let dir = home.join("sessions/wd_test/session_models");
        let path = main_wire(&dir);
        fs::create_dir_all(path.parent().unwrap())?;
        let payload = "reply 雪\n".repeat(10_000);
        let mut rows = vec![serde_json::json!({"type":"profile.bind","modelAlias":"first","systemPrompt":"system"})];
        for (step, alias, config) in [
            ("one", "first", true),
            ("two", "second", true),
            ("three", "third", false),
        ] {
            if config {
                rows.push(serde_json::json!({"type":"config.update","modelAlias":alias,"systemPrompt":"system"}));
            }
            rows.push(
                serde_json::json!({"type":"context.append_message","message":{"role":"user","content":"prompt"}}),
            );
            rows.push(
                serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":step}}),
            );
            // Native llm.request follows step.begin, so it must amend the pending reply too.
            rows.push(serde_json::json!({"type":"llm.request","kind":"loop","modelAlias":alias}));
            rows.push(serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":step,"part":{"type":"text","text":payload}}}));
            rows.push(serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":step,"usage":{"inputOther":1,"output":2}}}));
        }
        rows.push(serde_json::json!({"type":"llm.request","kind":"title","modelAlias":"title-only"}));
        rows.push(serde_json::json!({"type":"context.append_message","message":{"role":"assistant","content":"copied third-model reply"}}));
        rows.push(serde_json::json!({"type":"config.update","modelAlias":"first"}));
        rows.push(serde_json::json!({"type":"context.append_message","message":{"role":"assistant","content":"back to first"}}));
        fs::write(
            &path,
            rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
        )?;
        let adapter = KimiCode::with_home(&home);
        let r = SessionRef::new("models", Harness::KimiCode, &dir);
        for opts in [
            Demand::RICH,
            Demand::LEAN.with_spans(),
            Demand {
                spans: true,
                ..Demand::COMPLETE
            },
        ] {
            let mut full = Kept::default();
            let shell = adapter.stream(&r, &opts, &mut full)?;
            assert_eq!(shell.model.as_deref(), Some("first"));
            let models = full
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .map(|m| m.model.as_deref())
                .collect::<Vec<_>>();
            assert_eq!(
                models,
                vec![
                    Some("first"),
                    Some("second"),
                    Some("third"),
                    Some("third"),
                    Some("first")
                ]
            );
            for (count, checkpoint) in &full.checkpoints {
                let mut rest = Kept::default();
                let suffix = adapter
                    .transcript(&r, &opts, crate::stream::Start::Checkpoint(checkpoint), &mut rest)?
                    .unwrap();
                assert_eq!(serde_json::to_value(&suffix)?, serde_json::to_value(&shell)?);
                assert_eq!(
                    serde_json::to_value(&rest.messages)?,
                    serde_json::to_value(&full.messages[*count..])?
                );
            }
            let mut old = full.checkpoints.last().unwrap().1.clone();
            old.state.get_mut(crate::stream::ADAPTER_LAYER).unwrap()["state"]["ctx"]
                .as_object_mut()
                .unwrap()
                .remove("model_bindings");
            assert!(adapter
                .transcript(&r, &opts, crate::stream::Start::Checkpoint(&old), &mut Kept::default())?
                .is_none());
        }
        Ok(())
    }

    #[test]
    fn windowed_reads_keep_the_model() {
        let _world = crate::test_world("kimi_code_windowed");
        let corpus = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi-code");
        for version in ["0.39.1", "0.39.1-then-2.1.1", "2.1.1"] {
            let home = corpus.join(version).join(".kimi-code");
            crate::harness::assert_windowed_models(&KimiCode::with_home(&home));
        }
    }

    #[test]
    fn one_header_pass_preserves_independent_first_fields_and_record_limit() -> Result<()> {
        let world = crate::test_world("kimi_code_header_fields");
        let path = world.home.join("wire.jsonl");
        let ignored = "unused payload ".repeat(20_000);
        let cases = [
            format!("{}\n{}\n",
                serde_json::json!({"type":"llm.request","kind":"title","modelAlias":"title-only"}),
                serde_json::json!({"type":"profile.bind","modelAlias":"main-model","systemPrompt":"prompt"})),
            format!("bad json\n{}\n{}\n{}\n",
                serde_json::json!({"type":"llm.request","modelAlias":"first-model","systemPrompt":ignored,"body":ignored}),
                serde_json::json!({"type":"profile.bind","modelAlias":"later-model","systemPrompt":"first prompt"}),
                serde_json::json!({"type":"config.update","systemPrompt":"later prompt"})),
            format!("{}{}\n{}\n", "42\n".repeat(15),
                serde_json::json!({"type":"profile.bind","modelAlias":"sixteenth","systemPrompt":"last admitted prompt"}),
                serde_json::json!({"type":"profile.bind","modelAlias":"outside-limit","systemPrompt":"outside-limit"})),
            "{\"type\":\"profile.bind\",\"modelAlias\":\"\",\"systemPrompt\":\"\"}\n{\"type\":\"config.update\",\"modelAlias\":\"later\",\"systemPrompt\":\"nonempty\"}\n".into(),
            "{\"type\":\"profile.bind\",\"modelAlias\":42,\"systemPrompt\":null}\n".into(),
        ];
        for body in cases {
            fs::write(&path, &body)?;
            let mut expected = HeaderFields::default();
            let mut seen = 0;
            super::super::for_each_json_line_str(&body, |value| {
                seen += 1;
                let ty = value.get("type").and_then(Value::as_str);
                if expected.model.is_none()
                    && (matches!(ty, Some("profile.bind" | "config.update" | "usage.record"))
                        || (ty == Some("llm.request")
                            && value
                                .get("kind")
                                .and_then(Value::as_str)
                                .is_none_or(|kind| kind == "loop")))
                {
                    expected.model = value.get("modelAlias").and_then(Value::as_str).map(str::to_owned);
                }
                if expected.system_prompt.is_none() && matches!(ty, Some("profile.bind" | "config.update")) {
                    expected.system_prompt = value
                        .get("systemPrompt")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned);
                }
                if seen >= 16 {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            });
            assert_eq!(peek_header(&path), expected);
        }
        Ok(())
    }

    #[test]
    fn discovery_refreshes_state_and_cwd_fallbacks_without_wire_append() -> Result<()> {
        let world = crate::test_world("kimi_code_aux_discovery");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let home = world.home.join(".kimi-code");
        let dir = home.join("sessions/wd_test/session_one");
        let wire = main_wire(&dir);
        fs::create_dir_all(wire.parent().unwrap())?;
        let body =
            "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"prompt fallback\"}}\n";
        fs::write(&wire, body)?;
        let index = home.join("session_index.jsonl");
        fs::write(&index, "{\"sessionId\":\"session_one\",\"workDir\":\"/first-index\"}\n")?;
        fs::write(dir.join("state.json"), "{\"title\":\"first title\"}")?;
        let adapter = KimiCode::with_home(&home);
        let first = adapter.discover()?.remove(0);
        assert_eq!(first.title.as_deref(), Some("first title"));
        assert_eq!(first.cwd.as_deref(), Some(Path::new("/first-index")));
        fs::write(
            dir.join("state.json"),
            "{\"title\":\"renamed without a new turn\",\"updatedAt\":1700000000000}",
        )?;
        let renamed = adapter.discover()?.remove(0);
        assert_eq!(renamed.title.as_deref(), Some("renamed without a new turn"));
        assert_eq!(renamed.updated_at, DateTime::from_timestamp_millis(1700000000000));
        fs::write(dir.join("state.json"), "{}")?;
        fs::write(
            &index,
            "{\"sessionId\":\"session_one\",\"workDir\":\"/changed-index\"}\n",
        )?;
        let fallback = adapter.discover()?.remove(0);
        assert_eq!(fallback.title.as_deref(), Some("prompt fallback"));
        assert_eq!(fallback.cwd.as_deref(), Some(Path::new("/changed-index")));
        fs::write(&index, "")?;
        fs::write(
            home.join("workspaces.json"),
            "{\"workspaces\":{\"wd_test\":{\"root\":\"/workspace\"}}}",
        )?;
        assert_eq!(adapter.discover()?[0].cwd.as_deref(), Some(Path::new("/workspace")));
        fs::write(dir.join("state.json"), "{\"cwd\":\"/state\"}")?;
        assert_eq!(adapter.discover()?[0].cwd.as_deref(), Some(Path::new("/state")));
        assert_eq!(fs::read_to_string(wire)?, body);
        Ok(())
    }

    /// The index grows by one line per new session; that rescans the new session alone, and
    /// every other session keeps its index cwd.
    #[test]
    fn a_new_session_rescans_only_itself() -> Result<()> {
        let world = crate::test_world("kimi_code_new_sibling");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let home = world.home.join(".kimi-code");
        let index = home.join("session_index.jsonl");
        let add = |name: &str| -> Result<()> {
            let wire = main_wire(&home.join("sessions/wd_test").join(name));
            fs::create_dir_all(wire.parent().unwrap())?;
            fs::write(
                &wire,
                "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            )?;
            let mut f = fs::OpenOptions::new().create(true).append(true).open(&index)?;
            std::io::Write::write_all(
                &mut f,
                format!("{{\"sessionId\":\"{name}\",\"workDir\":\"/cwd-{name}\"}}\n").as_bytes(),
            )?;
            Ok(())
        };
        let scanned = || {
            let mut all = SCANNED.lock().unwrap();
            let mine: Vec<PathBuf> = all.iter().filter(|p| p.starts_with(&home)).cloned().collect();
            all.retain(|p| !p.starts_with(&home));
            mine
        };
        for i in 0..5 {
            add(&format!("session_{i}"))?;
        }
        let adapter = KimiCode::with_home(&home);
        assert_eq!(adapter.discover()?.len(), 5);
        assert_eq!(scanned().len(), 5);
        add("session_new")?;
        let refs = adapter.discover()?;
        assert_eq!(scanned(), vec![home.join("sessions/wd_test/session_new")]);
        for r in &refs {
            let name = r.path.file_name().unwrap().to_string_lossy();
            assert_eq!(r.cwd.as_deref(), Some(Path::new(&format!("/cwd-{name}"))));
        }
        Ok(())
    }

    #[test]
    fn native_state_avoids_wire_scan_but_missing_times_keep_exact_extrema() -> Result<()> {
        let world = crate::test_world("kimi-code-native-metadata");
        let dir = world.base.join("session-summary");
        let wire = main_wire(&dir);
        fs::create_dir_all(wire.parent().unwrap())?;
        fs::write(&wire, "{\"type\":\"context.append_message\",\"time\":9,\"message\":{\"role\":\"user\",\"content\":\"wire prompt\"}}\n{\"time\":2}\n{\"time\":15}\n")?;
        fs::write(
            dir.join("state.json"),
            r#"{"title":" native raw title ","createdAt":100,"updatedAt":101}"#,
        )?;
        jsonl::OPENED.with(|paths| paths.borrow_mut().clear());
        let summary = scan(&dir, "summary", None).unwrap().unwrap();
        assert_eq!(summary.title.as_deref(), Some(" native raw title "));
        assert_eq!(summary.created_at, super::super::ts_from_value(&serde_json::json!(100)));
        assert_eq!(summary.updated_at, super::super::ts_from_value(&serde_json::json!(101)));
        assert!(!jsonl::OPENED.with(|paths| paths.borrow().contains(&wire)));
        fs::write(dir.join("state.json"), r#"{"title":"native title","createdAt":100}"#)?;
        let partial = scan(&dir, "summary", None).unwrap().unwrap();
        assert_eq!(partial.created_at, summary.created_at);
        assert_eq!(partial.updated_at, super::super::ts_from_value(&serde_json::json!(15)));
        fs::write(dir.join("state.json"), r#"{"createdAt":100,"updatedAt":101}"#)?;
        assert_eq!(
            scan(&dir, "summary", None).unwrap().unwrap().title.as_deref(),
            Some("wire prompt")
        );
        Ok(())
    }

    #[test]
    fn swapped_step_and_eof_callbacks_recheck_source_before_reuse() -> Result<()> {
        let world = crate::test_world("kimi-code-step-callback");
        let dir = world.base.join("session-step");
        let wire = main_wire(&dir);
        fs::create_dir_all(wire.parent().unwrap())?;
        let begin = "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.begin\",\"uuid\":\"one\"}}\n";
        let part = "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"stepUuid\":\"one\",\"part\":{\"type\":\"text\",\"text\":\"first reply\"}}}\n";
        let swap = "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"stepUuid\":\"two\",\"part\":{\"type\":\"text\",\"text\":\"next reply\"}}}\n";
        let reference = SessionRef::new("step", Harness::KimiCode, &dir);
        let adapter = KimiCode::with_home(&world.base);
        for tail in [String::new(), format!("{swap}{}", "{}\n".repeat(2000))] {
            let raw = format!("{begin}{part}{tail}");
            fs::write(&wire, &raw)?;
            adapter.stream(&reference, &Demand::RICH, &mut |_: Message| Flow::Continue)?;
            let mut calls = 0;
            let result = adapter.stream(&reference, &Demand::RICH, &mut |_: Message| {
                calls += 1;
                fs::OpenOptions::new()
                    .write(true)
                    .open(&wire)
                    .unwrap()
                    .set_len(0)
                    .unwrap();
                // A callback that changed the file is a slow one by construction (the stream
                // module's source-guarantee policy): the reader checks right after it.
                std::thread::sleep(jsonl::Read::SLOW_CALLBACK * 10);
                Flow::Continue
            });
            assert!(
                result.is_err(),
                "{} callback",
                if tail.is_empty() { "EOF" } else { "swapped step" }
            );
            assert_eq!(calls, 1);
        }
        Ok(())
    }

    #[test]
    fn checked_discovery_kimi_code_matches_owned_steps_titles_and_state() -> Result<()> {
        let world = crate::test_world("kimi-code-discovery-selected");
        let dir = world.base.join("session-id");
        fs::create_dir_all(&dir)?;
        let wire = main_wire(&dir);
        fs::create_dir_all(wire.parent().unwrap())?;
        let body = "body λ\n".repeat(512 * 1024);
        let rows = [
            serde_json::json!({"type":"context.append_message","time":2,"message":{"role":"user","origin":{"kind":"injection"},"content":body}}),
            serde_json::json!({"type":"context.append_message","time":1,"message":{"role":"user","content":[{"text":""},{"image":"ignored"},{"text":body}]}}),
            serde_json::json!({"type":"context.append_loop_event","time":7,"event":{"type":"step.begin","ignored":body}}),
            serde_json::json!({"type":"context.append_loop_event","time":9,"event":{"type":"step.end","ignored":body}}),
            serde_json::json!({"type":"context.append_message","time":8,"message":{"role":"assistant","content":body}}),
        ];
        let mut bytes = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>()
            .into_bytes();
        bytes.extend_from_slice(b"invalid-json\n\xff\n{\"ignored\":1e999}\n");
        bytes.extend_from_slice(" {\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"origin\":{\"kind\":null},\"content\":\"partial\"}} ".as_bytes());
        bytes.extend_from_slice(b"\n{\"type\":\"ignored\",\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.begin\",\"type\":\"step.end\"}}\n");
        fs::write(&wire, bytes)?;
        let facts = |w: WireScan| (w.title, w.first_ts, w.last_ts);
        assert_eq!(facts(scan_wire(&wire)?), facts(reducer(&wire)));
        for title in [
            Value::Null,
            serde_json::json!("explicit title λ"),
            serde_json::json!(" "),
            serde_json::json!([{"text":"invalid title type"}]),
        ] {
            let state = serde_json::json!({"cwd":"/p","title":title,"createdAt":3,"updatedAt":11,"lastPrompt":body,"unknown":body});
            fs::write(dir.join("state.json"), serde_json::to_vec(&state)?)?;
            let actual = scan(&dir, "id", None).unwrap().unwrap();
            let old = reducer(&wire);
            assert_eq!(actual.message_count, None);
            assert_eq!(
                actual.title,
                state
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_string)
                    .or(old.title)
            );
            assert_eq!(
                actual.created_at,
                state
                    .get("createdAt")
                    .and_then(super::super::ts_from_value)
                    .or(old.first_ts)
            );
            assert_eq!(
                actual.updated_at,
                state
                    .get("updatedAt")
                    .and_then(super::super::ts_from_value)
                    .or(old.last_ts)
            );
        }
        for entry in walkdir::WalkDir::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi-code"))
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if entry.file_name() != "wire.jsonl" {
                continue;
            }
            assert_eq!(facts(scan_wire(entry.path())?), facts(reducer(entry.path())));
        }
        Ok(())
    }

    #[derive(Default)]
    struct Kept {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for Kept {
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

    /// The simple discovery reducer over owned values: title fallback and time bounds.
    fn reducer(wire: &Path) -> WireScan {
        let mut w = WireScan::default();
        let bytes = fs::read(wire).unwrap();
        for line in bytes.split(|byte| *byte == b'\n') {
            let Ok(line) = std::str::from_utf8(line) else { continue };
            super::super::for_each_json_line_str(line, |v| {
                if let Some(ts) = v.get("time").and_then(super::super::ts_from_value) {
                    w.first_ts = Some(w.first_ts.map_or(ts, |f| f.min(ts)));
                    w.last_ts = Some(w.last_ts.map_or(ts, |l| l.max(ts)));
                }
                if v.get("type").and_then(Value::as_str) == Some("context.append_message")
                    && v.pointer("/message/role").and_then(Value::as_str) == Some("user")
                {
                    let kind = v.pointer("/message/origin/kind").and_then(Value::as_str);
                    if w.title.is_none() && kind.unwrap_or("user") == "user" {
                        let t =
                            super::super::wire::joined_owned(v.pointer("/message/content"), super::super::wire::TEXT);
                        if !t.trim().is_empty() {
                            w.title = Some(crate::ir::truncate(&t, crate::projection::Head::TITLE));
                        }
                    }
                }
                Flow::Continue
            });
        }
        w
    }

    /// The whole session read with every string inline: what a spans read materializes to.
    fn old_wire_oracle(r: &SessionRef, opts: &Demand) -> Session {
        let mut opts = *opts;
        opts.spans = false;
        let mut kept = Kept::default();
        let mut shell = KimiCode { root: None }.try_stream(r, &opts, &mut kept).unwrap();
        shell.messages = kept.messages;
        shell
    }

    #[test]
    fn checked_complete_bodies_keep_raw_envelopes_and_guarded_replay() {
        let world = crate::test_world("kimi_code_checked_complete");
        let dir = world.home.join(".kimi-code/sessions/wd_test/session_checked");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        let path = main_wire(&dir);
        let payload = "é\n\"\\".repeat(20_000);
        let rows = [
            serde_json::json!({"type":"metadata","protocol_version":"1.5"}),
            serde_json::json!({"type":"context.append_message","time":1730000000,"message":{"role":"user","origin":{"kind":"user"},"content":payload},"extra_raw":payload}),
            serde_json::json!({"type":"context.append_loop_event","time":1730000000.25,"event":{"type":"step.begin","uuid":"s","turnId":"t","step":1}}),
            serde_json::json!({"type":"context.append_loop_event","time":1730000001,"event":{"type":"content.part","stepUuid":"s","part":{"type":"think","think":payload,"encrypted":"opaque"}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","stepUuid":"s","toolCallId":"c","name":"Read","args":{"payload":payload}}}),
            serde_json::json!({"type":"context.append_loop_event","time":"2024-10-27T03:33:22Z","event":{"type":"tool.result","toolCallId":"c","result":{"output":[{"text":payload},{"type":"image_url","image_url":{"url":"file:image"}},{"text":"output_path:/tmp/result"}],"isError":true,"note":{"why":"test"},"truncated":true},"traceId":"trace","unused":payload},"outer_is_not_in_synthesized_record":payload}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s","usage":{"inputOther":17,"output":2},"messageId":"native","finishReason":"stop"}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"unknown","future":payload}}),
            serde_json::json!({"type":"unknown","future":payload}),
            serde_json::json!(42),
        ];
        let body = rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(&path, &body).unwrap();
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        let r = SessionRef::new("checked", Harness::KimiCode, &dir);
        let opts = Demand {
            spans: true,
            ..Demand::COMPLETE
        };
        let expected = old_wire_oracle(&r, &opts);
        let mut kept = Kept::default();
        let mut session = adapter.stream(&r, &opts, &mut kept).unwrap();
        let raw_count = kept.messages.iter().filter(|m| m.complete_record.is_some()).count();
        assert_eq!(raw_count, 2, "append + synthesized tool-result records");
        let retained = kept
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::ToolResult)
            .unwrap()
            .clone();
        session.messages = kept.messages;
        assert_eq!(
            serde_json::to_value(&session).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        session.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(&session).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        // Truncation, not growth: an append-only log may grow under a retained source.
        fs::write(&path, "").unwrap();
        assert!(serde_json::to_value(retained).is_err());
    }

    #[test]
    fn growing_call_name_checkpoints_preserve_late_and_duplicate_result_suffixes() -> Result<()> {
        let world = crate::test_world("kimi-code-growing-call-state");
        let dir = world.home.join("session_calls");
        fs::create_dir_all(dir.join("agents/main"))?;
        let names = (0..96)
            .map(|index| format!("tool-{index}-{}", "n".repeat(1024)))
            .collect::<Vec<_>>();
        let event = |event: Value| json!({"type":"context.append_loop_event","event":event});
        let mut records = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let step = format!("step-{index}");
            records.push(event(json!({"type":"step.begin","uuid":step})));
            records.push(event(
                json!({"type":"tool.call","stepUuid":step,"toolCallId":format!("call-{index}"),"name":name,"args":{}}),
            ));
            records.push(event(json!({"type":"step.end","uuid":step})));
        }
        for index in [0, 48, 95, 0, 999] {
            records.push(event(
                json!({"type":"tool.result","toolCallId":format!("call-{index}"),"result":{"output":"late result"}}),
            ));
        }
        fs::write(
            main_wire(&dir),
            records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
        )?;
        let adapter = KimiCode::with_home(&world.home);
        let reference = SessionRef::new("calls", Harness::KimiCode, &dir);
        for options in [Demand::RICH, Demand::LEAN.with_spans()] {
            let expected = old_wire_oracle(&reference, &options);
            let mut full = Kept::default();
            adapter.try_stream(&reference, &options, &mut full)?;
            assert_eq!(
                serde_json::to_value(&full.messages)?,
                serde_json::to_value(expected.messages)?
            );
            let last = full.checkpoints.last().unwrap();
            assert_eq!(jsonl::restore::<ResumeState>(&last.1).unwrap().ctx.call_names.len(), 96);
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
                let mut suffix = Kept::default();
                assert!(adapter
                    .transcript(
                        &reference,
                        &options,
                        crate::stream::Start::Checkpoint(checkpoint),
                        &mut suffix
                    )?
                    .is_some());
                assert_eq!(
                    serde_json::to_value(suffix.messages)?,
                    serde_json::to_value(&full.messages[*count..])?
                );
            }
            let mut amortized = Kept::default();
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
                serde_json::to_value(&amortized.messages)?,
                serde_json::to_value(&full.messages)?
            );
            assert!(amortized.checkpoints.len() < full.checkpoints.len() / 2);
            let snapshot_bytes = amortized
                .checkpoints
                .iter()
                .map(|(_, cp)| serde_json::to_vec(cp).unwrap().len())
                .sum::<usize>();
            assert!(snapshot_bytes as u64 <= fs::metadata(main_wire(&dir))?.len() * 4);
            for (count, checkpoint) in &amortized.checkpoints {
                let mut suffix = Kept::default();
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
                    serde_json::to_value(suffix.messages)?,
                    serde_json::to_value(&full.messages[*count..])?
                );
            }
        }
        Ok(())
    }

    #[test]
    fn resume_checkpoints_match_old_corpus_and_every_selected_suffix() {
        let world = crate::test_world("kimi_code_resume_corpus");
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi-code");
        let mut files = 0;
        let mut resumes = 0;
        for file in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if file.file_name() != "wire.jsonl" {
                continue;
            }
            let agent = file.path().parent().unwrap();
            let session = agent.parent().unwrap().parent().unwrap();
            let path = if agent.file_name().unwrap() == "main" {
                session
            } else {
                agent
            };
            let r = SessionRef::new("corpus", Harness::KimiCode, path);
            for opts in [
                Demand::LEAN.with_spans(),
                Demand::RICH.with_spans(),
                Demand::RICH,
                Demand::COMPLETE,
                Demand {
                    spans: true,
                    ..Demand::COMPLETE
                },
            ] {
                let expected = old_wire_oracle(&r, &opts);
                let mut full = Kept::default();
                let mut shell = adapter.try_stream(&r, &opts, &mut full).unwrap();
                shell.messages = full.messages.clone();
                assert_eq!(
                    serde_json::to_value(&shell).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{} old collector",
                    file.path().display()
                );
                let stride = (full.checkpoints.len() / 12).max(1);
                for (i, (count, cp)) in full.checkpoints.iter().enumerate().step_by(stride) {
                    let mut rest = Kept::default();
                    let suffix = adapter
                        .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut rest)
                        .unwrap()
                        .unwrap();
                    shell.messages.clear();
                    assert_eq!(
                        serde_json::to_value(suffix).unwrap(),
                        serde_json::to_value(&shell).unwrap(),
                        "{} @{} metadata",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        serde_json::to_value(&rest.messages).unwrap(),
                        serde_json::to_value(&full.messages[*count..]).unwrap(),
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
        assert!(files >= 10, "frozen native wire fixtures missing: {files}");
        assert!(resumes >= 100, "only {resumes} resumable corpus boundaries");
        eprintln!("kimi-code native files={files} exact suffix resumes={resumes}");
    }

    fn resume_fixture(world: &cv_testkit::World, body: &[u8]) -> SessionRef {
        let dir = world.home.join(".kimi-code/sessions/wd_test/session_resume");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::write(dir.join("state.json"), r#"{"id":"session_resume","cwd":"/project"}"#).unwrap();
        fs::write(dir.join("agents/main/wire.jsonl"), body).unwrap();
        SessionRef::new("resume", Harness::KimiCode, dir)
    }

    #[test]
    fn lazy_finalized_payloads_resolve_like_original_step_dispatch() {
        let world = crate::test_world("kimi_code_source_payloads");
        let body = "雪\n\"".repeat(6000);
        let output = format!("{body}\noutput_path: first\n{body}\noutput_path: /scratch/result.log\n");
        let records = [
            serde_json::json!({"type":"metadata","protocol_version":"2"}),
            serde_json::json!({"type":"profile.bind","modelAlias":"test","systemPrompt":"fixture"}),
            serde_json::json!({"type":"context.append_message","message":{"role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":body}]}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s","turnId":"turn","step":3,"part":{"type":"think","think":body}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","stepUuid":"s","toolCallId":"c","name":"Read","args":{"payload":body}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c","traceId":"trace","result":{"output":output,"note":"n","truncated":true,"isError":false}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c","result":{"output":[{"type":"text","text":body},{"type":"image_url","image_url":{"url":"blobref:image"}},{"text":"\noutput_path: /scratch/composite.log"}]}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s","messageId":"final","usage":{"inputOther":7,"output":11},"finishReason":"tool_use"}}),
        ];
        let text = records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        let r = resume_fixture(&world, text.as_bytes());
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        for opts in [
            Demand::LEAN.with_spans(),
            Demand::RICH.with_spans(),
            Demand::RICH,
            Demand::COMPLETE,
        ] {
            let expected = old_wire_oracle(&r, &opts);
            let mut actual = crate::stream::collect_with(&adapter, &r, &opts).unwrap();
            if opts.spans {
                assert!(matches!(
                    &actual.messages[2].content[1],
                    Block::ToolUse {
                        input: crate::Json::Source(_),
                        ..
                    }
                ));
            }
            actual.materialize().unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        let mut lazy = crate::stream::collect_with(&adapter, &r, &Demand::LEAN.with_spans()).unwrap();
        fs::write(adapter.resume_source(&r), "{}\n").unwrap();
        assert!(lazy.materialize().is_err());
    }

    #[test]
    fn giant_two_part_results_match_owned_dispatch() {
        let world = crate::test_world("kimi_code_giant_composite");
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        for bytes in [2 * 1024 * 1024, 8 * 1024 * 1024] {
            for escaped in [false, true] {
                let unit = if escaped { "雪\n\"\\" } else { "x" };
                let body = unit.repeat(bytes / unit.len());
                let records = [
                    serde_json::json!({"type":"metadata","protocol_version":"2"}),
                    serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","stepUuid":"s","toolCallId":"c","result":{"output":[{"type":"text","text":body},{"text":format!("{body}\noutput_path: /scratch/final.log")}]}}}),
                    serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s"}}),
                ];
                let wire = records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
                let session = resume_fixture(&world, wire.as_bytes());
                let expected = old_wire_oracle(&session, &Demand::LEAN.with_spans());
                let mut actual = crate::stream::collect_with(&adapter, &session, &Demand::LEAN.with_spans()).unwrap();
                assert!(actual.messages.iter().flat_map(|m| &m.content).any(|block| matches!(
                    block,
                    Block::ToolResult {
                        content: Text::Source(_),
                        ..
                    }
                )));
                actual.materialize().unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
    }

    #[test]
    fn resume_replays_partial_records_and_unfinished_steps_with_exact_diagnostics() {
        use std::io::Write as _;
        let world = crate::test_world("kimi_code_partial_resume");
        let prefix = concat!(
            "{\"type\":\"metadata\",\"protocol_version\":\"2\"}\n",
            "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\r\n",
            "bad json\n",
            "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.begin\",\"uuid\":\"s\"}}\n",
            "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"stepUuid\":\"s\",\"part\":{\"type\":\"text\",\"text\":\"unfinished\"}}}\n",
            "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.end\",\"uuid\":\"s\",\"usage\":{"
        );
        let r = resume_fixture(&world, prefix.as_bytes());
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        let opts = Demand::RICH.with_spans();
        let mut first = Kept::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert!((cp.offset as usize) < prefix.find("step.begin").unwrap());
        // The EOF-flushed provisional reply must not be included in the immutable prefix.
        assert_eq!(*count, 1);
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(adapter.resume_source(&r))
                .unwrap(),
            "\"output\":3}}}}"
        )
        .unwrap();
        let mut suffix = Kept::default();
        let mut shell = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        shell.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(old_wire_oracle(&r, &opts)).unwrap()
        );
        // Wrong option flavor and a different agent's path decline before metadata/callbacks.
        let mut none = Kept::default();
        assert!(adapter
            .transcript(&r, &Demand::COMPLETE, crate::stream::Start::Checkpoint(cp), &mut none)
            .unwrap()
            .is_none());
        let mut other = r.clone();
        other.path = other.path.join("agents/agent-0");
        assert!(adapter
            .transcript(&other, &opts, crate::stream::Start::Checkpoint(cp), &mut none)
            .unwrap()
            .is_none());
        assert!(none.messages.is_empty() && none.checkpoints.is_empty());
        // Invalid UTF-8 is counted per record, as the old tolerant .lines scanner did.
        let mut bytes = fs::read(adapter.resume_source(&r)).unwrap();
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(b"{\"type\":\"context.clear\"}\n");
        fs::write(adapter.resume_source(&r), bytes).unwrap();
        let mut all = Kept::default();
        let mut shell = adapter.try_stream(&r, &opts, &mut all).unwrap();
        shell.messages = all.messages;
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(old_wire_oracle(&r, &opts)).unwrap()
        );
    }

    #[test]
    fn resume_append_reads_only_new_callbacks_and_keeps_catalog_fallbacks_current() {
        use std::io::Write as _;
        let world = crate::test_world("kimi_code_append_read_work");
        let line = "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
        let mut r = resume_fixture(&world, line.repeat(20_000).as_bytes());
        let adapter = KimiCode::with_home(&world.home.join(".kimi-code"));
        let opts = Demand::LEAN.with_spans();
        let mut first = Kept::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        assert!(serde_json::to_vec(cp).unwrap().len() < 1024);
        fs::OpenOptions::new()
            .append(true)
            .open(adapter.resume_source(&r))
            .unwrap()
            .write_all(line.repeat(200).as_bytes())
            .unwrap();
        r.title = Some("new catalog title".into());
        let mut suffix = Kept::default();
        let shell = adapter
            .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut suffix)
            .unwrap()
            .unwrap();
        assert_eq!(suffix.messages.len(), 200);
        assert_eq!(shell.title, r.title);
        assert_eq!(suffix.messages.len() + count, old_wire_oracle(&r, &opts).messages.len());
        eprintln!(
            "kimi-code initial_callbacks=20000 appended_callbacks=200 whole_callbacks=20200 checkpoint_bytes={}",
            serde_json::to_vec(cp).unwrap().len()
        );
    }

    const SID: &str = "66732eb8-2935-40cf-bcad-c0bcd204e2fb";

    /// A store mirroring a real `~/.kimi-code` (protocol 1.5): one session with a main agent + one
    /// sub-agent, a workspaces map, and an index with a tombstone for a second (deleted) session.
    /// Returns the world (keep it scoped; dropping it removes the dir) and the home root.
    fn fixture_root() -> (cv_testkit::World, PathBuf) {
        let world = cv_testkit::World::new("kimi-code-fixture");
        let root = world.home.join(".kimi-code");
        let sdir = root
            .join("sessions")
            .join("wd_proj_fd4cb822f4b3")
            .join(format!("session_{SID}"));
        let main = sdir.join("agents").join("main");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(sdir.join("agents").join("agent-0")).unwrap();
        fs::write(
            root.join("workspaces.json"),
            r#"{"version":1,"workspaces":{"wd_proj_fd4cb822f4b3":{"root":"/Users/u/proj","name":"proj"}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            format!(
                "{{\"sessionId\":\"session_{SID}\",\"sessionDir\":\"{}\",\"workDir\":\"/Users/u/proj\"}}\n\
                 {{\"sessionId\":\"session_dead\",\"sessionDir\":\"x\",\"workDir\":\"/x\"}}\n\
                 {{\"sessionId\":\"session_dead\",\"deleted\":true}}\n",
                sdir.display()
            ),
        )
        .unwrap();
        // The tombstoned session still has a directory on disk — it must not be listed.
        let dead = root.join("sessions").join("wd_proj_fd4cb822f4b3").join("session_dead");
        fs::create_dir_all(dead.join("agents").join("main")).unwrap();
        fs::write(
            dead.join("state.json"),
            r#"{"id":"session_dead","version":2,"cwd":"/x"}"#,
        )
        .unwrap();
        fs::write(
            sdir.join("state.json"),
            format!(
                r#"{{"id":"session_{SID}","version":2,"cwd":"/Users/u/proj","archived":false,
                    "agents":{{"main":{{"type":"main"}},"agent-0":{{"type":"sub","parentAgentId":"main","labels":{{"parentAgentId":"main"}}}}}},
                    "title":"fix the flaky test","titleKind":"replaceable","isCustomTitle":false,
                    "createdAt":1787509645914,"updatedAt":1787509891612,"lastTurnReason":"completed"}}"#
            ),
        )
        .unwrap();
        let lines = [
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1787509645942}"#,
            r#"{"type":"profile.bind","modelAlias":"kimi-code/k3","profileName":"agent","thinkingEffort":"high","systemPrompt":"You are Kimi Code CLI.","time":1787509645950}"#,
            r#"{"type":"permission.set_mode","mode":"yolo","time":1787509645980}"#,
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"fix the flaky test"}],"origin":{"kind":"user"},"time":1787509645990}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"fix the flaky test"}],"toolCalls":[],"origin":{"kind":"user"}},"time":1787509645990}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},"time":1787509646000}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"p0","turnId":"0","step":1,"stepUuid":"s1","part":{"type":"think","think":"","encrypted":null}},"time":1787509646001}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"p1","turnId":"0","step":1,"stepUuid":"s1","part":{"type":"think","think":"Look at the test first."}},"time":1787509646002}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"tool_A","turnId":"0","step":1,"stepUuid":"s1","toolCallId":"tool_A","name":"Read","args":{"path":"/Users/u/proj/tests/flaky.rs"}},"time":1787509646003}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"tool_A","toolCallId":"tool_A","result":{"output":"fn flaky() {}\n","note":"<system>2 lines read.</system>"},"traceId":"t1"},"time":1787509646100}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"tool_B","turnId":"0","step":1,"stepUuid":"s1","toolCallId":"tool_B","name":"Bash","args":{"command":"cargo test"}},"time":1787509646004}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"tool_B","toolCallId":"tool_B","result":{"output":"error: no such command\noutput_size_bytes: 6261\noutput_path: /Users/u/.kimi-code/sessions/x/agents/main/tool-results/Bash-tool_B-abc.txt\nnext_step: Use Read","isError":true,"truncated":true}},"time":1787509646200}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,"usage":{"inputOther":2452,"output":39,"inputCacheRead":17920,"inputCacheCreation":0},"finishReason":"tool_calls","messageId":"chatcmpl-1"},"time":1787509646300}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s2","turnId":"0","step":2},"time":1787509646400}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"p2","turnId":"0","step":2,"stepUuid":"s2","part":{"type":"text","text":"Fixed it."}},"time":1787509646401}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s2","turnId":"0","step":2,"usage":{"inputOther":100,"output":5,"inputCacheRead":0,"inputCacheCreation":0},"finishReason":"end_turn","messageId":"chatcmpl-2"},"time":1787509646500}"#,
            r#"{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":2552,"output":44,"inputCacheRead":17920,"inputCacheCreation":0},"usageScope":"turn","time":1787509646501}"#,
            r#"{"type":"turn.ended","turnId":0,"reason":"completed","durationMs":600,"time":1787509646600}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"<notification>task done</notification>"}],"origin":{"kind":"background_task"}},"time":1787509646700}"#,
            r#"{"type":"context.apply_compaction","summary":"Current Focus:\nthe flaky test","compactedCount":4,"time":1787509646800}"#,
            r#"{"type":"task.terminated","info":{"taskId":"bash-1","description":"long build","status":"completed","exitCode":0},"outputTail":"ok","time":1787509646900}"#,
        ];
        fs::write(main.join("wire.jsonl"), lines.join("\n") + "\n").unwrap();
        // The sub-agent's own wire (as Kimi Code writes one: `config.update` binds cwd/model, a
        // second one binds the profile's system prompt, then the delegated prompt and its step).
        let sub_lines = [
            lines[0],
            r#"{"type":"config.update","cwd":"/Users/u/proj","modelAlias":"kimi-code/k3","thinkingEffort":"high","time":1787509646010}"#,
            r#"{"type":"config.update","profileName":"explore","systemPrompt":"You are an explorer.","time":1787509646011}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"Extract IPA verifier math"}],"origin":{"kind":"user"}},"time":1787509646020}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"x1","turnId":"0","step":1},"time":1787509646030}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","uuid":"px","turnId":"0","step":1,"stepUuid":"x1","part":{"type":"text","text":"It is a Pedersen commitment."}},"time":1787509646031}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","uuid":"x1","turnId":"0","step":1,"usage":{"inputOther":10,"output":6,"inputCacheRead":0,"inputCacheCreation":0},"finishReason":"end_turn","messageId":"chatcmpl-x"},"time":1787509646040}"#,
        ];
        fs::write(
            sdir.join("agents").join("agent-0").join("wire.jsonl"),
            sub_lines.join("\n") + "\n",
        )
        .unwrap();
        (world, root)
    }

    #[test]
    fn projected_inventory_matches_value_tombstone_oracle() {
        let world = cv_testkit::World::new("kimi-code-inventory");
        let path = world.base.join("session_index.jsonl");
        let text = format!(
            "{{\"sessionId\":\"a\",\"workDir\":\"/a\",\"ignored\":\"{}\"}}\n{}",
            "雪\\\"".repeat(10_000),
            [
                r#"{"sessionId":"b","workDir":"/b"}"#,
                r#"{"sessionId":"a","deleted":true}"#,
                r#"{"sessionId":"a","deleted":true,"\u0064eleted":false,"workDir":"/last"}"#,
                r#"{"sessionId":"bad","workDir":"/bad","ignored":"\ud800"}"#,
                r#"{"sessionId":"bad","workDir":"/bad","ignored":1e400}"#,
                r#"{"sessionId":"b","workDir":null}"#,
                r#"{"sessionId":"b","deleted":"true"}"#,
                r#"{"sessionId":12,"workDir":"/wrong"}"#,
                r#"{"sessionId":"z","deleted":true,"sessionId":"last"}"#,
                "[null]",
                "invalid",
            ]
            .join("\n")
        );
        fs::write(&path, &text).unwrap();
        let mut expected = SessionIndex::default();
        super::super::for_each_json_line_str(&text, |value| {
            let Some(id) = value.get("sessionId").and_then(Value::as_str) else {
                return Flow::Continue;
            };
            if value.get("deleted").and_then(Value::as_bool) == Some(true) {
                expected.deleted.insert(id.to_string());
                expected.cwd.remove(id);
            } else {
                expected.deleted.remove(id);
                if let Some(cwd) = value.get("workDir").and_then(Value::as_str) {
                    expected.cwd.insert(id.to_string(), cwd.into());
                }
            }
            Flow::Continue
        });
        let actual = read_session_index(&path).0;
        assert_eq!(actual.deleted, expected.deleted);
        assert_eq!(actual.cwd, expected.cwd);
        assert_eq!(actual.cwd.get("a"), Some(&PathBuf::from("/last")));
        assert_eq!(actual.deleted, HashSet::from(["last".to_string()]));
        // A non-UTF-8 line is one rejected record; the entries around it stand.
        let mut invalid_utf8 = text.into_bytes();
        invalid_utf8.extend_from_slice(b"\n\xff");
        fs::write(&path, invalid_utf8).unwrap();
        let actual = read_session_index(&path).0;
        assert_eq!((actual.deleted, actual.cwd), (expected.deleted, expected.cwd));
    }

    #[test]
    fn subagent_discovery_reuses_histories_and_preserves_numeric_order_and_current_parent() -> Result<()> {
        let world = crate::test_world("kimi-code-subagent-cache");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let parent = world.home.join("session_parent");
        let mut main = SessionRef::new("parent", Harness::KimiCode, &parent);
        main.cwd = Some(PathBuf::from("/first"));
        let body = "{\"type\":\"context.append_message\",\"time\":1700000000000,\"message\":{\"role\":\"user\",\"content\":\"child task\"}}\n".repeat(128);
        for index in (0..12).rev() {
            let child = parent.join("agents").join(format!("agent-{index}"));
            fs::create_dir_all(&child)?;
            fs::write(child.join("wire.jsonl"), &body)?;
        }
        let first = subagent_refs(&main).unwrap();
        assert_eq!(first.len(), 12);
        for (index, reference) in first.iter().enumerate() {
            assert_eq!(reference.id, format!("parent/agent-{index}"));
            assert_eq!(reference.message_count, None);
            assert_eq!(reference.cwd, main.cwd);
            let wire = reference.path.join("wire.jsonl");
            let hit =
                crate::discover_cache::cached_scan(&wire, || panic!("unchanged child wire must be cached")).unwrap();
            assert_eq!(hit.message_count, reference.message_count);
            assert_eq!(hit.title, scan_wire(&wire)?.title);
        }
        main.id = "new-parent".into();
        main.cwd = Some(PathBuf::from("/second"));
        let unchanged = subagent_refs(&main).unwrap();
        assert!(unchanged
            .iter()
            .all(|reference| reference.id.starts_with("new-parent/") && reference.cwd == main.cwd));
        let wire = unchanged[2].path.join("wire.jsonl");
        fs::write(&wire, "{\"type\":\"context.append_message\",\"time\":1800000000000,\"message\":{\"role\":\"user\",\"content\":\"new prompt\"}}\n")?;
        let refreshed = subagent_refs(&main).unwrap();
        assert_eq!(refreshed[2].message_count, None);
        assert_eq!(refreshed[2].title.as_deref(), Some("new prompt"));
        assert_eq!(refreshed[2].created_at.unwrap().timestamp_millis(), 1800000000000);
        fs::remove_file(wire)?;
        assert_eq!(subagent_refs(&main).unwrap().len(), 11);
        Ok(())
    }

    #[test]
    fn sub_agent_wire_parses_as_its_own_session_with_lineage() {
        let (_world, root) = fixture_root();
        let a = KimiCode::with_home(&root);
        let main = a.discover().unwrap().remove(0);
        let subs = subagent_refs(&main).unwrap();
        assert_eq!(subs.len(), 1, "{subs:?}");
        let sub = &subs[0];
        assert_eq!(sub.id, format!("{SID}/agent-0"));
        let forest = crate::subagent_tree_of(&main).unwrap();
        assert_eq!(forest.iter().map(|a| &a.session.id).collect::<Vec<_>>(), [&sub.id]);
        assert_eq!(sub.path, main.path.join("agents").join("agent-0"));
        assert_eq!(sub.title.as_deref(), Some("Extract IPA verifier math"));
        assert_eq!(
            sub.message_count, None,
            "discovery defers the exact prompt + step count"
        );
        assert_eq!(sub.created_at.map(|t| t.timestamp_millis()), Some(1787509646010));

        let s = a.parse(sub).unwrap();

        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.lineage.parent.as_deref(), Some(SID));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("agent-0"));
        assert_eq!(s.system_prompt.as_deref(), Some("You are an explorer."));
        assert_eq!(s.model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        let bag = &s.extra["kimi-code"];
        assert_eq!(bag["agent_id"], "agent-0");
        assert_eq!(bag["parentAgentId"], "main");
        assert!(
            bag.get("agents").is_none(),
            "the agents map belongs to the main session"
        );
        assert!(bag.get("native_id").is_none());
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![MessageKind::SystemPrompt, MessageKind::Prompt, MessageKind::Reply],
            "{kinds:?}"
        );
        assert_eq!(s.messages[0].extra["kimi-code"]["event"], "config.update");
        // the main session knows nothing of the child's lineage
        let m = a.parse(&main).unwrap();
        assert!(m.lineage.is_empty());
    }

    #[test]
    fn discovers_sessions_from_the_walk_and_honours_tombstones() {
        let (_world, root) = fixture_root();
        let refs = KimiCode::with_home(&root).discover().unwrap();
        assert_eq!(refs.len(), 1, "the tombstoned session_dead dir is skipped: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.id, SID, "bare uuid, `session_` stripped");
        assert_eq!(r.harness, Harness::KimiCode);
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(r.title.as_deref(), Some("fix the flaky test"));
        assert_eq!(r.message_count, None, "discovery defers the exact user + step count");
        assert_eq!(r.created_at.map(|t| t.timestamp_millis()), Some(1787509645914));
        assert_eq!(r.updated_at.map(|t| t.timestamp_millis()), Some(1787509891612));
    }

    #[test]
    fn parses_steps_into_assistant_and_tool_turns() {
        let (_world, root) = fixture_root();
        let a = KimiCode::with_home(&root);
        let r = a.discover().unwrap().remove(0);
        let s = a.parse(&r).unwrap();
        assert_eq!(s.harness, Harness::KimiCode);
        assert_eq!(s.model.as_deref(), Some("kimi-code/k3"));
        let bag = &s.extra["kimi-code"];
        assert_eq!(bag["native_id"], format!("session_{SID}"));
        assert_eq!(bag["agents"]["agent-0"]["parentAgentId"], "main");
        assert_eq!(bag["protocol_version"], "1.5");
        assert_eq!(
            s.extra.len(),
            1,
            "session facts live only in the harness bag: {:?}",
            s.extra
        );
        assert_eq!(s.system_prompt.as_deref(), Some("You are Kimi Code CLI."));

        let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::System,    // system prompt (profile.bind)
                Role::User,      // prompt
                Role::Assistant, // step 1: thinking + 2 tool calls, flushed at step.end
                Role::Tool,      // Read result
                Role::Tool,      // Bash result
                Role::Assistant, // step 2: text
                Role::User,      // injected background-task notice
                Role::System,    // compaction boundary
                Role::System,    // compaction summary
                Role::System,    // task terminated
            ],
            "{roles:?}"
        );
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::SystemPrompt,
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::ToolResult,
                MessageKind::ToolResult,
                MessageKind::Reply,
                MessageKind::InjectedContext,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Notice,
            ],
            "{kinds:?}"
        );
        // system prompt
        assert_eq!(s.messages[0].text().as_deref(), Some("You are Kimi Code CLI."));
        assert_eq!(s.messages[0].origin, Origin::Harness);
        assert_eq!(s.messages[0].extra["kimi-code"]["event"], "profile.bind");
        // a typed prompt vs a harness-injected notice: kind + origin first-class, raw kind in the bag
        assert_eq!(s.messages[1].origin, Origin::Human);
        assert_eq!(s.messages[1].extra["kimi-code"]["origin_kind"], "user");
        assert_eq!(s.messages[6].origin, Origin::Scheduler);
        assert_eq!(s.messages[6].extra["kimi-code"]["origin_kind"], "background_task");
        assert_eq!(s.messages[2].origin, Origin::Model);
        assert_eq!(s.messages[3].origin, Origin::Harness);
        // step 1: the empty think part is dropped; the real one + both calls are there, in order
        let step1 = &s.messages[2];
        assert_eq!(step1.id.as_deref(), Some("chatcmpl-1"), "messageId from step.end");
        assert_eq!(step1.content.len(), 3, "{:?}", step1.content);
        assert!(
            matches!(&step1.content[0], Block::Thinking { text, .. } if text.as_ref() as &str == "Look at the test first.")
        );
        assert!(
            matches!(&step1.content[1], Block::ToolUse { id, name, input , ..} if id == "tool_A" && name == "Read" && input.resolve().unwrap()["path"] == "/Users/u/proj/tests/flaky.rs")
        );
        assert!(matches!(&step1.content[2], Block::ToolUse { name, .. } if name == "Bash"));
        let u = step1.usage.as_ref().unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens
            ),
            (Some(2452), Some(39), Some(17920), Some(0))
        );
        assert_eq!(step1.extra["kimi-code"]["finishReason"], "tool_calls");
        assert!(
            step1.extra.keys().all(|k| k == "kimi-code"),
            "message facts live only in the harness bag: {:?}",
            step1.extra
        );
        // tool results: name resolved from the call, is_error, note/truncated/persisted path in details
        let Block::ToolResult {
            tool_use_id,
            tool_name,
            is_error,
            details,
            content,
            ..
        } = &s.messages[3].content[0]
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_use_id, "tool_A");
        assert_eq!(tool_name.as_deref(), Some("Read"));
        assert!(!is_error);
        assert_eq!(content.as_ref() as &str, "fn flaky() {}\n");
        assert_eq!(details.as_ref().unwrap()["note"], "<system>2 lines read.</system>");
        let Block::ToolResult { is_error, details, .. } = &s.messages[4].content[0] else {
            panic!("expected tool result");
        };
        assert!(is_error);
        let d = details.as_ref().unwrap();
        assert_eq!(d["truncated"], true);
        // The wire names the file where the writer's machine had it; it is read beside this store.
        assert_eq!(
            d["persistedOutput"]["path"],
            r.path
                .join("agents/main/tool-results/Bash-tool_B-abc.txt")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            s.messages[3].timestamp.map(|t| t.timestamp_millis()),
            Some(1787509646100)
        );
        // step 2
        assert_eq!(s.messages[5].text().as_deref(), Some("Fixed it."));
        // compaction: the boundary (what the compaction detector keys on) then the summary
        assert!(s.messages[7].content.is_empty() && !s.messages[7].is_synthesized());
        assert_eq!(s.messages[7].extra["kimi-code"]["compactedCount"], 4);
        assert_eq!(s.messages[7].extra["kimi-code"]["event"], "context.apply_compaction");
        assert!(s.messages[8].text().unwrap().contains("Current Focus"));
        assert!(s.messages[9].text().unwrap().contains("long build"));
        assert_eq!(s.messages[9].extra["kimi-code"]["event"], "task.terminated");
        // lean mode carries no bookkeeping, and nothing lives outside the harness bag
        for m in &s.messages {
            assert!(!m.extra.contains_key(CARRIER_KEY));
            assert!(m.extra.keys().all(|k| k == "kimi-code"), "{:?}", m.extra);
        }
        assert!(s.lineage.is_empty(), "a top-level session has no lineage");
    }

    #[test]
    fn complete_mode_carries_bookkeeping_records() {
        let (_world, root) = fixture_root();
        let a = KimiCode::with_home(&root);
        let r = a.discover().unwrap().remove(0);
        let s = crate::stream::collect_with(&a, &r, &Demand::COMPLETE).unwrap();
        let carried: Vec<&str> = s
            .messages
            .iter()
            .filter_map(|m| m.extra.get(CARRIER_KEY)?.get("type")?.as_str())
            .collect();
        for t in [
            "metadata",
            "permission.set_mode",
            "turn.prompt",
            "usage.record",
            "turn.ended",
        ] {
            assert!(carried.contains(&t), "{t} should be carried: {carried:?}");
        }
        // carriers are `Carrier`-kind turns naming their record type
        assert!(s
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Carrier)
            .all(|m| m.extra.contains_key(CARRIER_KEY) && m.extra["kimi-code"]["record_type"].is_string()));
        // the system-prompt turn is both content and a carrier of the bind record
        let bind = s
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::SystemPrompt)
            .expect("system prompt turn");
        assert!(bind.extra.contains_key(CARRIER_KEY) && !bind.content.is_empty());
        // Every record is accounted for: 10 content turns (the step events fold into their assistant
        // turn rather than being carried one by one; the compaction is boundary + summary) + 5
        // bookkeeping carriers.
        assert_eq!(
            s.messages.len(),
            15,
            "{:?}",
            s.messages.iter().map(|m| m.role).collect::<Vec<_>>()
        );
    }

    /// `/undo` (`context.undo{count}`) is a branch: the undone turns stay in the log but leave the
    /// context. A `kimi fork` (2.x) names its source in `state.json.forkedFrom`.
    #[test]
    fn undo_is_a_branch_and_fork_names_its_source() {
        let world = cv_testkit::World::new("kimi-code-undo-fork");
        let root = world.home.join(".kimi-code");
        let sdir = root
            .join("sessions")
            .join("wd_p_000000000000")
            .join(format!("session_{SID}"));
        let main = sdir.join("agents").join("main");
        fs::create_dir_all(&main).unwrap();
        fs::write(
            sdir.join("state.json"),
            r#"{"id":"session_x","version":2,"cwd":"/p","forkedFrom":"session_0e0e0e0e-0000-0000-0000-000000000000"}"#,
        )
        .unwrap();
        let lines = [
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"Say ok."}],"origin":{"kind":"user"}},"time":1}"#,
            r#"{"type":"agent.switched","agentId":"main","branch":"b1","reason":"undo","base":{"branch":"main","line":7},"turns":1,"time":2}"#,
            r#"{"type":"context.undo","agentId":"main","count":1,"time":2}"#,
            r#"{"type":"context.undone","agentId":"main","turns":1,"fromTurnId":0,"time":2}"#,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"Say hi."}],"origin":{"kind":"user"}},"time":3}"#,
        ];
        fs::write(main.join("wire.jsonl"), lines.join("\n") + "\n").unwrap();
        let a = KimiCode::with_home(&root);
        let r = a.discover().unwrap().pop().unwrap();
        let s = a.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, [MessageKind::Prompt, MessageKind::Branch, MessageKind::Prompt]);
        assert_eq!(s.messages[1].origin, Origin::Human);
        assert_eq!(
            s.lineage.forked_from.as_deref(),
            Some("0e0e0e0e-0000-0000-0000-000000000000")
        );
    }

    /// 2.x writes `subagent.spawned` / `subagent.completed` on the parent's wire inside the
    /// spawning step: they follow the step's reply, and the child's usage is not the parent's.
    #[test]
    fn subagent_records_are_spawn_and_return_after_the_step() {
        let world = cv_testkit::World::new("kimi-code-subagent-records");
        let root = world.home.join(".kimi-code");
        let main = root
            .join("sessions")
            .join("wd_p_000000000000")
            .join(format!("session_{SID}"))
            .join("agents")
            .join("main");
        fs::create_dir_all(&main).unwrap();
        let lines = [
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},"time":1}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"t","stepUuid":"s1","toolCallId":"t1","name":"Agent","args":{}},"time":2}"#,
            r#"{"type":"subagent.spawned","subagentId":"agent-0","subagentName":"coder","parentToolCallId":"t1","description":"say ok","time":3}"#,
            r#"{"type":"subagent.completed","subagentId":"agent-0","resultSummary":"ok","usage":{"inputOther":5,"output":1},"time":4}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"t1","result":{"output":"ok"}},"time":5}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,"finishReason":"tool_calls"},"time":6}"#,
        ];
        fs::write(main.join("wire.jsonl"), lines.join("\n") + "\n").unwrap();
        let a = KimiCode::with_home(&root);
        let r = a.discover().unwrap().pop().unwrap();
        let s = a.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::Reply,
                MessageKind::SubagentSpawn,
                MessageKind::SubagentReturn,
                MessageKind::ToolResult
            ]
        );
        assert_eq!(s.messages[2].origin, Origin::Subagent);
        assert!(s.messages[2].usage.is_none());
        assert_eq!(
            s.messages[1]
                .harness_extra(Harness::KimiCode)
                .and_then(|x| x.get("parentToolCallId")),
            Some(&Value::String("t1".into()))
        );
    }

    #[test]
    fn harness_names_parse() {
        for alias in ["kimi-code", "kimicode", "kimi2"] {
            assert_eq!(Harness::parse(alias), Some(Harness::KimiCode));
        }
        assert_eq!(Harness::KimiCode.as_str(), "kimi-code");
        assert_eq!(
            Harness::parse("kimi"),
            Some(Harness::Kimi),
            "legacy store keeps its name"
        );
    }
}
