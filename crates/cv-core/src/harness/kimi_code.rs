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
//! - Everything else (`usage.record`, `llm.request`, `llm.tools_snapshot`, `turn.*`,
//!   `permission.*`, `tools.*`, `token_counting.*`, `task.started`, `plan_mode.*`,
//!   `swarm_mode.*`, `full_compaction.*`, …) is bookkeeping: dropped by the lean passes, carried
//!   verbatim under [`ParseOptions::complete`] as `Carrier` turns (`_record` = the record,
//!   `extra["kimi-code"]["record_type"]` = its `type`) like the Claude adapter's carriers.
//!
//! Every harness fact lives in `extra["kimi-code"]` (message: `event`, `origin_kind`,
//! `finishReason`, `compactedCount`, `record_type`, and under `ParseOptions::extra` the step
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

use super::Adapter;
use crate::harness::claude::CARRIER_KEY;
use crate::ir::*;
use crate::lazy::Text;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub struct KimiCode {
    /// `$KIMI_CODE_HOME` or `~/.kimi-code`, if it exists.
    root: Option<PathBuf>,
}

impl KimiCode {
    pub fn new() -> Self {
        let root = std::env::var_os("KIMI_CODE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".kimi-code")))
            .filter(|p| p.exists());
        KimiCode { root }
    }

    /// Test-only: point the adapter at an explicit root.
    #[cfg(test)]
    fn for_root(root: PathBuf) -> Self {
        KimiCode { root: Some(root) }
    }
}

impl Default for KimiCode {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for KimiCode {
    fn harness(&self) -> Harness {
        Harness::KimiCode
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let index = read_session_index(&root.join("session_index.jsonl"));
        let workspaces = read_workspaces(&root.join("workspaces.json"));

        // Walk `sessions/wd_*/session_*/` — the index is append-only and can lag a live session, so
        // the directory tree is the source of truth; the index only contributes tombstones + cwd.
        let mut dirs: Vec<(PathBuf, String, Option<PathBuf>)> = Vec::new();
        let Ok(wds) = fs::read_dir(root.join("sessions")) else {
            return Ok(vec![]);
        };
        for wd in wds.filter_map(|e| e.ok()) {
            let wd_path = wd.path();
            if !wd_path.is_dir() {
                continue;
            }
            let wd_name = wd.file_name().to_string_lossy().into_owned();
            let Ok(sessions) = fs::read_dir(&wd_path) else {
                continue;
            };
            for sd in sessions.filter_map(|e| e.ok()) {
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
        Ok(crate::par_flat_map(dirs, |(sdir, id, fallback_cwd)| {
            // Key the freshness check on the transcript (state.json is rewritten too, but the
            // transcript grows on every turn and is what the scan reads).
            let key = main_wire(&sdir);
            let key = if key.exists() { key } else { sdir.join("state.json") };
            crate::discover_cache::cached_scan(&key, || scan(&sdir, &id, fallback_cwd))
                .into_iter()
                .collect()
        }))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // The ref points at the session dir (the `main` agent) or, from [`subagent_refs`], at one
        // sub-agent's dir (`…/agents/agent-N`); the state file is the session's either way.
        let (session_dir, agent_id) = split_agent_path(&r.path);
        let state = read_state(session_dir);
        let wire = agent_wire(session_dir, agent_id);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::KimiCode,
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
            cwd: r
                .cwd
                .clone()
                .or_else(|| state.get("cwd").and_then(Value::as_str).map(PathBuf::from)),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            // The model is bound by the 2nd record (`profile.bind`); peek at the head so the sink's
            // header knows it before the body streams.
            model: peek_model(&wire),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: Map::new(),
        };
        // The system prompt is bound by the head of the wire (`profile.bind`); peek so the
        // session-level copy is known before the body streams (INTERFACE-V2 §4).
        s.system_prompt = peek_system_prompt(&wire);
        if agent_id == "main" {
            // Session facts live in the harness bag, `extra["kimi-code"]`, never at the top level.
            let bag = s.harness_extra_mut(Harness::KimiCode);
            if let Some(native) = state.get("id").and_then(Value::as_str) {
                // The id as Kimi Code writes it (`session_<uuid>`); cv keys the session by the bare uuid.
                bag.insert("native_id".into(), Value::String(native.to_string()));
            }
            for key in ["archived", "lastTurnReason", "titleKind", "isCustomTitle", "lastPrompt"] {
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

        sink.meta(&s);

        let Ok(file) = fs::File::open(&wire) else {
            return Ok(s);
        };
        let mut ctx = Ctx {
            opts,
            step: None,
            call_names: HashMap::new(),
            seen_prompts: HashSet::new(),
            protocol_version: None,
            stopped: false,
        };
        let skipped = super::for_each_json_line(BufReader::new(file), |v| ctx.record(&v, sink));
        if !ctx.stopped {
            ctx.flush_step(sink);
        }
        if let Some(pv) = ctx.protocol_version.take() {
            s.harness_extra_mut(Harness::KimiCode)
                .insert("protocol_version".into(), Value::String(pv));
        }
        super::note_skipped_lines(&mut s, skipped);
        Ok(s)
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
pub fn subagent_refs(main: &SessionRef) -> Vec<SessionRef> {
    let Ok(rd) = fs::read_dir(main.path.join("agents")) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, SessionRef)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u64 = name.strip_prefix("agent-")?.parse().ok()?;
            let dir = e.path();
            let wire = dir.join("wire.jsonl");
            if !wire.exists() {
                return None;
            }
            let w = scan_wire(&wire);
            Some((
                n,
                SessionRef {
                    id: format!("{}/{name}", main.id),
                    harness: Harness::KimiCode,
                    path: dir,
                    cwd: main.cwd.clone(),
                    title: w.title,
                    created_at: w.first_ts.or_else(|| file_mtime(&wire)),
                    updated_at: w.last_ts.or_else(|| file_mtime(&wire)),
                    message_count: w.message_count,
                },
            ))
        })
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out.into_iter().map(|(_, r)| r).collect()
}

fn read_state(session_dir: &Path) -> Value {
    fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
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
fn read_session_index(path: &Path) -> SessionIndex {
    let mut idx = SessionIndex::default();
    let Ok(text) = fs::read_to_string(path) else {
        return idx;
    };
    super::for_each_json_line_str(&text, |v| {
        let Some(id) = v.get("sessionId").and_then(Value::as_str) else {
            return Flow::Continue;
        };
        if v.get("deleted").and_then(Value::as_bool) == Some(true) {
            idx.deleted.insert(id.to_string());
            idx.cwd.remove(id);
        } else {
            idx.deleted.remove(id);
            if let Some(wd) = v.get("workDir").and_then(Value::as_str) {
                idx.cwd.insert(id.to_string(), PathBuf::from(wd));
            }
        }
        Flow::Continue
    });
    idx
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

/// Cheap metadata-only scan for `discover`: state.json for identity/title/times, one pass over the
/// transcript for the turn count (and the title when state.json has none).
fn scan(session_dir: &Path, id: &str, fallback_cwd: Option<PathBuf>) -> Option<SessionRef> {
    let state = read_state(session_dir);
    let wire = main_wire(session_dir);
    if !wire.exists() && state.is_null() {
        return None;
    }
    let cwd = state
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or(fallback_cwd);
    let w = scan_wire(&wire);
    let title = state
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or(w.title);

    let created = state
        .get("createdAt")
        .and_then(super::ts_from_value)
        .or(w.first_ts)
        .or_else(|| file_mtime(&wire));
    let updated = state
        .get("updatedAt")
        .and_then(super::ts_from_value)
        .or(w.last_ts)
        .or_else(|| file_mtime(&wire));

    Some(SessionRef {
        id: id.to_string(),
        harness: Harness::KimiCode,
        path: session_dir.to_path_buf(),
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count: w.message_count,
    })
}

/// What one pass over a wire log yields for a listing.
#[derive(Default)]
struct WireScan {
    /// user + assistant turns (the [`SessionRef::message_count`] contract).
    message_count: usize,
    /// The first prompt the USER typed (not an injected notice), truncated.
    title: Option<String>,
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
}

/// One pass over a wire log: turn count, title fallback and time bounds. A user record is one turn;
/// an assistant turn is one LLM step (`step.end`).
fn scan_wire(wire: &Path) -> WireScan {
    let mut w = WireScan::default();
    let Ok(file) = fs::File::open(wire) else {
        return w;
    };
    super::for_each_json_line(BufReader::new(file), |v| {
        if let Some(ts) = v.get("time").and_then(super::ts_from_value) {
            w.first_ts = Some(w.first_ts.map_or(ts, |f| f.min(ts)));
            w.last_ts = Some(w.last_ts.map_or(ts, |l| l.max(ts)));
        }
        match v.get("type").and_then(Value::as_str) {
            Some("context.append_message") => {
                if v.pointer("/message/role").and_then(Value::as_str) == Some("user") {
                    w.message_count += 1;
                    if w.title.is_none() && origin_kind(v.get("message")) == "user" {
                        let t = parts_text(v.pointer("/message/content"));
                        if !t.trim().is_empty() {
                            w.title = Some(crate::ir::truncate(&t, 80));
                        }
                    }
                }
            }
            Some("context.append_loop_event")
                if v.pointer("/event/type").and_then(Value::as_str) == Some("step.end") =>
            {
                w.message_count += 1;
            }
            _ => {}
        }
        Flow::Continue
    });
    w
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let m = fs::metadata(path).ok()?.modified().ok()?;
    let d = m.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64))
}

/// The model alias from the first `profile.bind` / `config.update` near the head of the transcript
/// (the bind is the 2nd record; scan a handful of lines, never the whole file).
fn peek_model(wire: &Path) -> Option<String> {
    let file = fs::File::open(wire).ok()?;
    let mut model = None;
    let mut seen = 0usize;
    super::for_each_json_line(BufReader::new(file), |v| {
        seen += 1;
        if matches!(
            v.get("type").and_then(Value::as_str),
            Some("profile.bind" | "config.update" | "llm.request" | "usage.record")
        ) {
            if let Some(m) = v.get("modelAlias").and_then(Value::as_str) {
                model = Some(m.to_string());
                return Flow::Stop;
            }
        }
        if seen >= 16 {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    model
}

/// `message.origin.kind` (`user` / `injection` / `system_trigger` / `background_task` / `task`),
/// `"user"` when absent (older records).
/// The system prompt from the head of the wire: the first `profile.bind` / `config.update` that
/// carries a non-empty `systemPrompt` (the same records [`Ctx::system_prompt`] turns into a
/// `SystemPrompt` message). Bounded like [`peek_model`].
fn peek_system_prompt(wire: &Path) -> Option<String> {
    let file = fs::File::open(wire).ok()?;
    let mut prompt = None;
    let mut seen = 0usize;
    super::for_each_json_line(BufReader::new(file), |v| {
        seen += 1;
        if matches!(
            v.get("type").and_then(Value::as_str),
            Some("profile.bind" | "config.update")
        ) {
            if let Some(p) = v.get("systemPrompt").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                prompt = Some(p.to_string());
                return Flow::Stop;
            }
        }
        if seen >= 16 {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    prompt
}

fn origin_kind(message: Option<&Value>) -> &str {
    message
        .and_then(|m| m.pointer("/origin/kind"))
        .and_then(Value::as_str)
        .unwrap_or("user")
}

/// One buffered LLM step: the Assistant turn under construction plus the Tool turns that already
/// answered its calls (results precede `step.end` on the wire).
struct StepBuf {
    uuid: String,
    msg: Message,
    results: Vec<Message>,
}

struct Ctx<'a> {
    opts: &'a ParseOptions,
    step: Option<StepBuf>,
    /// `toolCallId` → tool name, so a result can name its tool (`Block::ToolResult::tool_name`).
    call_names: HashMap<String, String>,
    /// Hashes of system prompts already surfaced (a re-bind with the same prompt is bookkeeping).
    seen_prompts: HashSet<u64>,
    protocol_version: Option<String>,
    stopped: bool,
}

impl Ctx<'_> {
    fn record(&mut self, v: &Value, sink: &mut dyn MessageSink) -> Flow {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = v.get("time").and_then(super::ts_from_value);
        let flow = match ty {
            "metadata" => {
                if let Some(pv) = v.get("protocol_version").and_then(Value::as_str) {
                    self.protocol_version = Some(pv.to_string());
                }
                self.carry(v, ts, sink)
            }
            "profile.bind" | "config.update" => self.system_prompt(v, ty, ts, sink),
            "context.append_message" => self.append_message(v, ts, sink),
            "context.append_loop_event" => self.loop_event(v, ts, sink),
            "context.apply_compaction" => {
                let f = self.flush_step(sink);
                if f == Flow::Stop {
                    return self.stop();
                }
                // Two turns, as the kinds vocabulary has it: the boundary (where the harness cut
                // the context), then the summary that seeds the next window, when there is one.
                let mut boundary = note(
                    "[conversation compacted]".into(),
                    ts,
                    MessageKind::CompactionBoundary,
                    Origin::Harness,
                    ty,
                );
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
                if self.opts.extra {
                    let bag = m.harness_extra_mut(Harness::KimiCode);
                    bag.insert("task".into(), info);
                    if let Some(tail) = v.get("outputTail") {
                        bag.insert("outputTail".into(), tail.clone());
                    }
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
        if !self.opts.complete {
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
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            prompt.hash(&mut h);
            h.finish()
        };
        if !self.seen_prompts.insert(hash) {
            return self.carry(v, ts, sink);
        }
        let mut m = Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness);
        if self.opts.complete {
            m.extra.insert(CARRIER_KEY.into(), v.clone());
        }
        m.timestamp = ts;
        m.content.push(Block::Text {
            text: Text::from(prompt),
        });
        let bag = m.harness_extra_mut(Harness::KimiCode);
        bag.insert("event".into(), Value::String(ty.to_string()));
        if self.opts.extra {
            for key in ["modelAlias", "profileName", "thinkingEffort"] {
                if let Some(x) = v.get(key) {
                    bag.insert(key.to_string(), x.clone());
                }
            }
        }
        sink.message(m)
    }

    fn append_message(&mut self, v: &Value, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        let f = self.flush_step(sink);
        if f == Flow::Stop {
            return Flow::Stop;
        }
        let message = v.get("message");
        let role = match message.and_then(|m| m.get("role")).and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            Some("tool") => Role::Tool,
            Some("system") => Role::System,
            _ => Role::User,
        };
        // `origin.kind` says who put the text there: `user` is a typed prompt; `injection` /
        // `system_trigger` are harness steers; `background_task` / `task` are automation notices.
        let origin_kind = origin_kind(message);
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
        m.timestamp = ts;
        push_parts(&mut m, message.and_then(|m| m.get("content")));
        m.harness_extra_mut(Harness::KimiCode)
            .insert("origin_kind".into(), Value::String(origin_kind.to_string()));
        if self.opts.complete {
            m.extra.insert(CARRIER_KEY.into(), v.clone());
        }
        sink.message(m)
    }

    fn loop_event(&mut self, v: &Value, ts: Option<DateTime<Utc>>, sink: &mut dyn MessageSink) -> Flow {
        let Some(e) = v.get("event") else {
            return Flow::Continue;
        };
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
            "content.part" => {
                let step_uuid = e.get("stepUuid").and_then(Value::as_str).unwrap_or("");
                let f = self.ensure_step(step_uuid, e, ts, sink);
                if f == Flow::Stop {
                    return Flow::Stop;
                }
                if let (Some(step), Some(part)) = (self.step.as_mut(), e.get("part")) {
                    if let Some(b) = part_to_block(part) {
                        step.msg.content.push(b);
                    }
                }
                Flow::Continue
            }
            "tool.call" => {
                let step_uuid = e.get("stepUuid").and_then(Value::as_str).unwrap_or("");
                let f = self.ensure_step(step_uuid, e, ts, sink);
                if f == Flow::Stop {
                    return Flow::Stop;
                }
                let id = e.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                let name = e.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                self.call_names.insert(id.clone(), name.clone());
                if let Some(step) = self.step.as_mut() {
                    step.msg.content.push(Block::ToolUse {
                        id,
                        name,
                        input: e.get("args").cloned().unwrap_or(Value::Null),
                        namespace: None,
                    });
                }
                Flow::Continue
            }
            "tool.result" => {
                let m = self.tool_result(e, ts);
                match self.step.as_mut() {
                    Some(step) => {
                        step.results.push(m);
                        Flow::Continue
                    }
                    None => sink.message(m),
                }
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
                    if self.opts.extra {
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
            _ => self.carry(v, ts, sink),
        }
    }

    fn open_step(&mut self, uuid: String, e: &Value, ts: Option<DateTime<Utc>>) {
        let mut msg = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
        msg.id = Some(uuid.clone());
        msg.timestamp = ts;
        if self.opts.extra {
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
        let Some(step) = self.step.take() else {
            return Flow::Continue;
        };
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

    fn tool_result(&self, e: &Value, ts: Option<DateTime<Utc>>) -> Message {
        let id = e.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
        let result = e.get("result");
        let output = result.and_then(|r| r.get("output"));
        let content = parts_text(output);
        let is_error = result
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Structured extras: the `<system>…</system>` note, the truncation flag, and — when the
        // full output went to a sidecar (`tool-results/…txt`, `tasks/<id>/output.log`) — its path,
        // under the same `details.persistedOutput` key the Claude adapter uses so `cv show`
        // points at it and the indexer reads it.
        let mut details = Map::new();
        if let Some(note) = result.and_then(|r| r.get("note")).filter(|n| !n.is_null()) {
            details.insert("note".into(), note.clone());
        }
        if result.and_then(|r| r.get("truncated")).and_then(Value::as_bool) == Some(true) {
            details.insert("truncated".into(), Value::Bool(true));
        }
        if let Some(path) = output_path(&content) {
            details.insert("persistedOutput".into(), serde_json::json!({ "path": path }));
        }
        let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        m.timestamp = ts;
        m.content.push(Block::ToolResult {
            tool_use_id: id.clone(),
            content: Text::from(content),
            is_error,
            tool_name: self.call_names.get(&id).cloned(),
            status: None,
            details: (!details.is_empty()).then(|| Value::Object(details)),
        });
        if self.opts.extra {
            if let Some(t) = e.get("traceId") {
                m.harness_extra_mut(Harness::KimiCode)
                    .insert("traceId".into(), t.clone());
            }
        }
        if self.opts.complete {
            m.extra.insert(CARRIER_KEY.into(), serde_json::json!({ "type": "context.append_loop_event", "event": e, "time": ts.map(|t| t.timestamp_millis()) }));
        }
        m
    }
}

/// A harness-side System turn of the given kind and origin, with the wire record type that
/// produced it in `extra["kimi-code"]["event"]`.
fn note(text: String, ts: Option<DateTime<Utc>>, kind: MessageKind, origin: Origin, event: &str) -> Message {
    let mut m = Message::of_kind(Role::System, kind, origin);
    m.timestamp = ts;
    m.content.push(Block::Text { text: Text::from(text) });
    m.harness_extra_mut(Harness::KimiCode)
        .insert("event".into(), Value::String(event.to_string()));
    m
}

/// `output_path: <path>` — the line a truncated tool output ends with when the whole output was
/// written to a sidecar file.
fn output_path(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("output_path:"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

fn usage_from(u: &Value) -> Usage {
    let get = |k: &str| u.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: get("inputOther"),
        output_tokens: get("output"),
        cache_read_tokens: get("inputCacheRead"),
        cache_creation_tokens: get("inputCacheCreation"),
        reasoning_tokens: None,
        cost_usd: None,
    }
}

/// Append a message's parts (`[Part]`, or a bare string) as blocks.
fn push_parts(m: &mut Message, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => m.content.push(Block::Text {
            text: Text::from(s.as_str()),
        }),
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

fn part_to_block(p: &Value) -> Option<Block> {
    match p.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: Text::from(p.get("text").and_then(Value::as_str).unwrap_or("")),
        }),
        "think" => {
            let text = p.get("think").and_then(Value::as_str).unwrap_or("");
            let encrypted = p.get("encrypted").and_then(Value::as_str).map(str::to_string);
            // kosong ≥ 0.55 persists `{"think":"","encrypted":null}` for an empty reasoning
            // stream — nothing to show, nothing to re-send.
            if text.is_empty() && encrypted.is_none() {
                return None;
            }
            Some(Block::Thinking {
                text: Text::from(text),
                signature: None,
                encrypted,
                redacted: false,
            })
        }
        "image_url" => {
            let url = p
                .get("imageUrl")
                .or_else(|| p.get("image_url"))
                .and_then(|u| u.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Block::Image {
                media_type: None,
                data_ref: url,
            })
        }
        _ => None,
    }
}

/// Text of a part list (or bare string): text parts joined by newlines, images as `[image]`.
fn parts_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("image_url") => Some("[image]".to_string()),
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "66732eb8-2935-40cf-bcad-c0bcd204e2fb";

    /// A store mirroring a real `~/.kimi-code` (protocol 1.5): one session with a main agent + one
    /// sub-agent, a workspaces map, and an index with a tombstone for a second (deleted) session.
    fn fixture_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cv-kimi-code-{}", uuid::Uuid::new_v4()));
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
        root
    }

    #[test]
    fn sub_agent_wire_parses_as_its_own_session_with_lineage() {
        let root = fixture_root();
        let a = KimiCode::for_root(root.clone());
        let main = a.discover().unwrap().remove(0);
        let subs = subagent_refs(&main);
        assert_eq!(subs.len(), 1, "{subs:?}");
        let sub = &subs[0];
        assert_eq!(sub.id, format!("{SID}/agent-0"));
        assert_eq!(sub.path, main.path.join("agents").join("agent-0"));
        assert_eq!(sub.title.as_deref(), Some("Extract IPA verifier math"));
        assert_eq!(sub.message_count, 2, "the prompt + one step");
        assert_eq!(sub.created_at.map(|t| t.timestamp_millis()), Some(1787509646010));

        let s = a.parse(sub).unwrap();
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
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovers_sessions_from_the_walk_and_honours_tombstones() {
        let root = fixture_root();
        let refs = KimiCode::for_root(root.clone()).discover().unwrap();
        assert_eq!(refs.len(), 1, "the tombstoned session_dead dir is skipped: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.id, SID, "bare uuid, `session_` stripped");
        assert_eq!(r.harness, Harness::KimiCode);
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(r.title.as_deref(), Some("fix the flaky test"));
        assert_eq!(r.message_count, 4, "2 user records + 2 LLM steps");
        assert_eq!(r.created_at.map(|t| t.timestamp_millis()), Some(1787509645914));
        assert_eq!(r.updated_at.map(|t| t.timestamp_millis()), Some(1787509891612));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_steps_into_assistant_and_tool_turns() {
        let root = fixture_root();
        let a = KimiCode::for_root(root.clone());
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
            matches!(&step1.content[1], Block::ToolUse { id, name, input , ..} if id == "tool_A" && name == "Read" && input["path"] == "/Users/u/proj/tests/flaky.rs")
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
        assert_eq!(
            d["persistedOutput"]["path"],
            "/Users/u/.kimi-code/sessions/x/agents/main/tool-results/Bash-tool_B-abc.txt"
        );
        assert_eq!(
            s.messages[3].timestamp.map(|t| t.timestamp_millis()),
            Some(1787509646100)
        );
        // step 2
        assert_eq!(s.messages[5].text().as_deref(), Some("Fixed it."));
        // compaction: the boundary (what the compaction detector keys on) then the summary
        assert_eq!(s.messages[7].text().as_deref(), Some("[conversation compacted]"));
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
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn complete_mode_carries_bookkeeping_records() {
        let root = fixture_root();
        let a = KimiCode::for_root(root.clone());
        let r = a.discover().unwrap().remove(0);
        let s = crate::stream::collect_with(&a, &r, &ParseOptions::complete()).unwrap();
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
        fs::remove_dir_all(&root).ok();
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
