//! Claude Code adapter — `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`, plus the same
//! layout under any extra roots (`$CLAUDE_CONFIG_DIR`, `$CLUSTERVISION_CLAUDE_ROOTS`, the
//! `claude-roots` file; see [`Claude::new`]).
//!
//! See `docs/FORMATS.md`. Key points: one session per `.jsonl` file; each line is a typed record;
//! `cwd` is read from inside the transcript (the dir-name encoding is lossy), and the conversation is
//! threaded via `uuid`/`parentUuid`.

use super::{parse_ts, Adapter};

/// `cv adopt`: move a sub-agent stranded by a dead session into a live one — Claude-only byte
/// surgery on this store's layout, so it lives beside the adapter that knows it.
pub mod adopt;
pub(crate) mod emit;
mod fork;
mod metadata;
mod record;
pub(crate) mod replay;
/// The sidecar tree beside a transcript (`subagents/`, `workflows/`, `journal.jsonl`): every path
/// spelled once, and the one journal parser.
pub mod sidecar;
pub mod skills;
use super::wire::anthropic;
use crate::ir::*;
use crate::ir::{MessageKind, Origin};
use crate::json::{Json, JsonSource};
use crate::lazy::Text;
use crate::stream::{Checkpoint, CollectSink, Flow, MessageSink, ParseOptions, ADAPTER_LAYER};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
pub use emit::{emit, verify_claude_resumable};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Claude {
    /// Root entries in priority order, before `*` expansion and the existence check. They are
    /// resolved by [`Claude::roots`] on every call, so a root that appears later (a new agent seat
    /// under a `*` entry) is read without rebuilding the adapter.
    entries: Vec<PathBuf>,
}

/// Files Claude Code keeps at the top of a config dir. A directory that holds one of them but no
/// `projects/` is a config dir with no sessions yet, not a projects dir: reading it as one would
/// list its other `.jsonl` files (e.g. `transcripts/*.jsonl`) as sessions.
const CONFIG_MARKERS: [&str; 5] = [
    "settings.json",
    ".claude.json",
    ".credentials.json",
    "history.jsonl",
    "CLAUDE.md",
];

impl Claude {
    /// Collect the root entries, in priority order:
    /// 1. `$CLAUDE_CONFIG_DIR/projects` — Claude Code's own variable for moving its config dir, so
    ///    this is where the Claude Code of this environment reads and writes (and it is the
    ///    the first of [`roots`](Adapter::roots), the default conversion target, when it exists);
    /// 2. `~/.claude/projects` — Claude Code's default;
    /// 3. each entry of `$CLUSTERVISION_CLAUDE_ROOTS` (a path list, split like `$PATH`);
    /// 4. each non-blank line of `$CLUSTERVISION_HOME/claude-roots` (default
    ///    `~/.clustervision/claude-roots`) that does not start with `#` — the machine-wide list,
    ///    which every cv process (CLI, MCP servers, daemon) reads alike.
    ///
    /// An entry from 3 or 4 may name a Claude config dir (one that holds `projects/`) or a projects
    /// dir itself, may start with `~`, and may hold path segments that are exactly `*`; each stands
    /// for every entry of the directory before it (`~/.agents/*/claude`, `~/.agents/*/instances/*/claude`).
    pub fn detect() -> Self {
        let home = crate::env::home_dir();
        let mut entries: Vec<PathBuf> = Vec::new();
        entries.extend(
            crate::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(|d| PathBuf::from(d).join("projects")),
        );
        entries.extend(home.as_ref().map(|h| h.join(".claude").join("projects")));
        if let Some(list) = crate::env::var_os("CLUSTERVISION_CLAUDE_ROOTS") {
            entries.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
        }
        let cv_home = crate::env::var_os("CLUSTERVISION_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".clustervision")));
        if let Some(text) = cv_home.and_then(|d| fs::read_to_string(d.join("claude-roots")).ok()) {
            entries.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(PathBuf::from),
            );
        }
        let entries = entries
            .into_iter()
            .map(|e| match (e.strip_prefix("~"), &home) {
                (Ok(rest), Some(h)) => h.join(rest),
                _ => e,
            })
            .collect();
        Claude { entries }
    }

    /// An adapter over one explicit Claude config dir (the dir holding `projects/`) or projects
    /// dir, instead of the environment and `~/.claude` — for the harness corpus and other tests,
    /// without mutating the process environment.
    pub fn with_config_dir(dir: &Path) -> Self {
        Claude {
            entries: vec![dir.to_path_buf()],
        }
    }

    /// The projects dirs this adapter reads, in priority order: each entry with its `*` expanded
    /// and a config dir mapped to its `projects/`; missing dirs dropped; and one directory reached
    /// several ways (a symlink, or a config dir and its `projects/` both listed) kept once, under
    /// the first spelling seen.
    fn resolve_roots(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        self.entries
            .iter()
            .flat_map(|e| expand_star(e))
            .filter_map(projects_dir)
            .filter(|p| fs::canonicalize(p).is_ok_and(|c| seen.insert(c)))
            .collect()
    }
}

/// `entry` with each path segment that is exactly `*` replaced by each entry of the directory
/// before it (names starting with `.` skipped, like a shell glob), sorted so the root order is
/// stable. An entry with no `*` is returned as is; a `*` under a missing directory matches nothing.
fn expand_star(entry: &Path) -> Vec<PathBuf> {
    let mut head = PathBuf::new();
    let mut rest = entry.components();
    while let Some(c) = rest.next() {
        if c.as_os_str() != "*" {
            head.push(c);
            continue;
        }
        let tail = rest.as_path();
        let Ok(dir) = fs::read_dir(&head) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = dir
            .flatten()
            .filter(|d| !d.file_name().to_string_lossy().starts_with('.'))
            .map(|d| {
                if tail.as_os_str().is_empty() {
                    d.path()
                } else {
                    d.path().join(tail)
                }
            })
            .collect();
        out.sort();
        // A later `*` in the tail (`agents/*/instances/*/claude`) expands the same way.
        return out.iter().flat_map(|p| expand_star(p)).collect();
    }
    vec![entry.to_path_buf()]
}

/// The projects dir `entry` names: its `projects/` when it has one (a Claude config dir), else the
/// entry itself — unless that is missing, or is a config dir with no sessions yet
/// ([`CONFIG_MARKERS`]).
fn projects_dir(entry: PathBuf) -> Option<PathBuf> {
    let projects = entry.join("projects");
    if projects.is_dir() {
        return Some(projects);
    }
    (entry.is_dir() && !CONFIG_MARKERS.iter().any(|m| entry.join(m).exists())).then_some(entry)
}

impl Adapter for Claude {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.resolve_roots()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // Session files sit at projects/<encoded>/<sid>.jsonl (depth 2) in every root. Subagent
        // transcripts live deeper (…/<sid>/subagents/…), which max_depth(2) naturally excludes.
        // Collect paths (cheap), then scan (read + parse) them in parallel. The same session id in
        // two roots (a transcript copied into another config dir to resume there) is listed twice,
        // once per file; `crate::find` resolves such an id to the most recently updated copy.
        let paths: Vec<_> = self
            .resolve_roots()
            .iter()
            .flat_map(|root| WalkDir::new(root).min_depth(2).max_depth(2))
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            // `cv prune` writes a `<id>.flat.jsonl` sidecar next to the session it creates; it has the
            // `.jsonl` extension but is NOT a session (it holds stashed tool payloads), so exclude it.
            .filter(|p| {
                !p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".flat.jsonl"))
            })
            .collect();
        Ok(crate::par_discovery_filter_map(paths, |path| {
            cached_metadata(&path, true)
        }))
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let session = new_session_for(&r.id, Some(r.path.clone()), opts);
            let s = read(session, &r.path, 0, None, opts, sink, &mut || Ok(()))?;
            Ok(with_fork_source(s, &r.path, sink))
        })
    }

    fn resume_format(&self) -> Option<u32> {
        Some(RESUME_FORMAT)
    }

    /// The state at a line boundary is the session accumulated so far (the spawn tracker holds
    /// nothing there: Claude's spawns all return later, as their own records).
    fn resume(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let Some(session) = from.get::<Session>(ADAPTER_LAYER) else {
                return Ok(None);
            };
            let s = read(session, &r.path, from.offset, None, opts, sink, &mut || Ok(()))?;
            Ok(Some(with_fork_source(s, &r.path, sink)))
        })
    }

    /// The task list (`TaskCreate`/`TaskUpdate`) Claude Code keeps beside the transcripts:
    /// `<config>/tasks/<session id>/<n>.json`.
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        task_list(&r.path, &r.id).into_iter().collect()
    }

    fn copied_before(&self, r: &SessionRef, sink: &mut dyn MessageSink) -> Option<chrono::DateTime<chrono::Utc>> {
        fork::copied_before(&r.path, sink)
    }
}

/// What a checkpoint's adapter state means: bump when the session the stream accumulates, or what
/// a record parses to, changes.
const RESUME_FORMAT: u32 = 1;

/// A `--fork-session` copy names no source; the records it shares with one do ([`fork`]). Runs
/// after every stream, whole or resumed, since it reads other files.
fn with_fork_source(mut s: Session, path: &Path, sink: &mut dyn MessageSink) -> Session {
    if s.lineage.parent.is_none() && s.lineage.forked_from.is_none() {
        s.lineage.forked_from = fork::source_tracked(path, sink);
    }
    s
}

/// Fully parse a Claude `.jsonl` transcript from its text contents into a [`Session`] (full
/// fidelity, including `extra` sidecars).
///
/// Pure (no filesystem). `id` is the session id (usually the file stem); `source_path` is recorded
/// for provenance when known. This is the whole-`Session` convenience over [`stream_str`].
pub fn parse_str(id: &str, text: &str, source_path: Option<PathBuf>) -> Session {
    let mut sink = CollectSink::default();
    let mut session = stream_str(id, text, source_path, &ParseOptions::full(), &mut sink);
    session.messages = sink.messages;
    session
}

/// Stream a transcript's text into `sink`, returning the [`Session`] metadata with empty
/// `messages`. The pure (no-filesystem) core that both [`parse_str`] and the on-disk
/// [`Adapter::stream`] delegate to.
pub fn stream_str(
    id: &str,
    text: &str,
    source_path: Option<PathBuf>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut session = new_session_for(id, source_path, opts);
    let mut spawns = crate::affordance::AffordanceSink::new(Harness::Claude, sink);
    // Pure-string path (parse_str, tests): no file offsets, so all content stays inline.
    super::for_each_json_line_str(text, |v| {
        record::ingest(&mut session, (&v).into(), None, opts, &mut spawns, &mut false).unwrap_or(Flow::Stop)
    });
    session
}

/// How many messages a full parse of the transcript's first `len` bytes yields — the count a
/// parse would have returned when the file was `len` bytes long, since a Claude transcript only
/// grows by appending. `check` runs at every line, including lines that emit no message.
pub(crate) fn count_prefix(r: &SessionRef, len: u64) -> Result<usize> {
    count_prefix_checked(r, len, &mut || Ok(()))
}

pub(crate) fn count_prefix_checked(r: &SessionRef, len: u64, check: &mut dyn FnMut() -> Result<()>) -> Result<usize> {
    let mut n = 0usize;
    let mut count = |_: Message| {
        n += 1;
        Flow::Continue
    };
    let opts = ParseOptions::full();
    read(
        new_session_for(&r.id, Some(r.path.clone()), &opts),
        &r.path,
        0,
        Some(len),
        &opts,
        &mut count,
        check,
    )?;
    Ok(n)
}

/// Stream the transcript's first `len` bytes: the session as it was at that length.
pub(crate) fn stream_prefix(
    r: &SessionRef,
    len: u64,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    crate::stream::checked(sink, |sink| {
        read(
            new_session_for(&r.id, Some(r.path.clone()), opts),
            &r.path,
            0,
            Some(len),
            opts,
            sink,
            &mut || Ok(()),
        )
    })
}

/// The one Claude read: lines `from..to` (EOF by default) of the captured transcript, each through
/// the record codec, accumulating into `session` (a checkpoint's state, or a fresh shell at 0).
/// After every finished line the sink may take a [`Checkpoint`] holding the session so far; a
/// partial last line is read but re-read whole next time. A message holding a source descriptor
/// makes the reader recheck the source's identity before going on, and once more at the end.
fn read(
    mut session: Session,
    path: &Path,
    from: u64,
    to: Option<u64>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Session> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    source.check_metadata(file.metadata()?)?;
    let to = to.map_or(source.len(), |to| to.min(source.len()));
    let mut spawns = crate::affordance::AffordanceSink::new(Harness::Claude, sink);
    let mut published = false;
    crate::stream::checked(&mut spawns, |sink| {
        crate::harness::store::jsonl::for_each_line(&source, &file, from, to, |line| {
            check()?;
            if let Some(view) = line.view {
                let mut sourced = false;
                let flow = record::ingest(&mut session, view, Some(line.at), opts, sink, &mut sourced)?;
                if sourced {
                    published = true;
                    source.check()?;
                }
                if flow == Flow::Stop {
                    return Ok(Flow::Stop);
                }
            }
            // Only after a line its writer finished: a partial last line is re-read whole next time.
            if line.complete && sink.wants_checkpoint(line.end) {
                let mut cp = Checkpoint::new(line.end);
                cp.put(ADAPTER_LAYER, &session);
                sink.checkpoint(&mut cp);
            }
            Ok(Flow::Continue)
        })
    })?;
    if published {
        source.check_metadata(file.metadata()?)?;
        source.check()?;
    }
    Ok(session)
}

/// An empty Claude [`Session`] shell, ready to accumulate records. A sub-agent transcript (by its
/// path) starts with its lineage already filled in — see [`apply_subagent_lineage`].
#[cfg(test)]
fn new_session(id: &str, source_path: Option<PathBuf>) -> Session {
    new_session_for(id, source_path, &ParseOptions::full())
}

fn new_session_for(id: &str, source_path: Option<PathBuf>, opts: &ParseOptions) -> Session {
    let mut s = Session {
        source_path,
        ..Session::new(id.to_string(), Harness::Claude)
    };
    if let Some(p) = s.source_path.clone() {
        apply_subagent_lineage(&mut s, &p, !opts.usage_identity || opts.extra || opts.complete);
    }
    s
}

/// A sub-agent transcript lives at `<parent-stem>/subagents/[workflows/<run>/]agent-<id>.jsonl`:
/// the path alone names the parent session, and the `agent-<id>.meta.json` sidecar names the
/// `Agent`/`Task` tool call in the parent that spawned it (plus the agent type and description).
/// Both become first-class [`Lineage`]; the Claude-only facts (agent type, description, workflow
/// run) go to `extra["claude"]`. A top-level session path leaves everything untouched.
fn apply_subagent_lineage(s: &mut Session, path: &std::path::Path, rich: bool) {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    let dir_name = |p: &std::path::Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    // Peel an optional `workflows/<run>/` tier, then require `subagents/` under the parent's dir.
    let (subagents, workflow_run) = match dir.parent().and_then(dir_name).as_deref() {
        Some("workflows") => (dir.parent().and_then(|p| p.parent()), dir_name(dir)),
        _ => (Some(dir), None),
    };
    let Some(subagents) = subagents.filter(|d| dir_name(d).as_deref() == Some("subagents")) else {
        return;
    };
    let Some(parent_stem) = subagents.parent().and_then(dir_name) else {
        return;
    };
    s.lineage.parent = Some(parent_stem);
    // Usage needs transcript/model/response identities, not the auxiliary task description.
    // Keep path-derived parentage so the unrelated fork sibling scan remains ineligible.
    if !rich {
        return;
    }
    let (agent_type, description, tool_use_id) = read_meta(&path.with_extension("meta.json"));
    s.lineage.spawned_by_tool_use = tool_use_id;
    let bag = s.harness_extra_mut(Harness::Claude);
    if let Some(t) = agent_type {
        bag.insert("agent_type".into(), Value::String(t));
    }
    if let Some(d) = description {
        bag.insert("agent_description".into(), Value::String(d));
    }
    if let Some(run) = workflow_run {
        bag.insert("workflow_run".into(), Value::String(run));
    }
}

fn note_conversation_metadata(
    session: &mut Session,
    cwd: Option<&str>,
    branch: Option<&str>,
    timestamp: Option<DateTime<Utc>>,
) {
    if session.cwd.is_none() {
        session.cwd = cwd.map(PathBuf::from);
    }
    if session.git.is_none() {
        session.git = branch.map(|branch| GitInfo {
            branch: Some(branch.to_string()),
            ..Default::default()
        });
    }
    // Real transcripts can contain out-of-order timestamps from queued writes and clock skew.
    if let Some(ts) = timestamp {
        session.created_at = Some(session.created_at.map_or(ts, |c| c.min(ts)));
        session.updated_at = Some(session.updated_at.map_or(ts, |u| u.max(ts)));
    }
}

/// `extra` key under which a [`carrier_record`] stashes the verbatim original meta record.
pub const CARRIER_KEY: &str = "_record";

/// The record field holding a tool result's structured sidecar — and the key the sidecar is parked
/// under inside [`Block::ToolResult::details`] when it is a bare scalar (see
/// [`attach_tool_use_result`]). `emit_claude` writes the field back from those `details`.
pub const TOOL_USE_RESULT_KEY: &str = "toolUseResult";

/// `details` key holding cv's *derived* pointer to a spilled tool output (`persistedOutput.path` /
/// `.size`). Shared across adapters (Kimi Code sets it too) and re-derived from the transcript stub
/// on every parse, so it is never replayed into an emitted Claude record.
pub const PERSISTED_OUTPUT_KEY: &str = "persistedOutput";

/// `message.model` of Claude Code's own client-side assistant notices (API errors such as
/// "Prompt is too long", "No response requested.", …): never model output, never sent to the API.
pub const SYNTHETIC_MODEL: &str = "<synthetic>";

fn user_origin_from(scheduled: bool, is_meta: bool, kind: Option<&str>) -> Origin {
    if scheduled {
        return Origin::Scheduler;
    }
    if is_meta {
        return Origin::Harness;
    }
    match kind {
        None | Some("human") => Origin::Human,
        Some(_) => Origin::Harness,
    }
}

fn user_kind_from_head(origin: Origin, head: &str) -> (MessageKind, Origin) {
    let head = head.trim_start();
    if head.starts_with("[Request interrupted by user") {
        return (MessageKind::InjectedContext, Origin::Harness);
    }
    if origin != Origin::Harness {
        return (MessageKind::Prompt, origin);
    }
    if head.starts_with("<task-notification>") {
        let summary = tag_inner(head, "summary").unwrap_or("").trim_start();
        if summary.starts_with("Agent ")
            || memchr::memchr2_iter(b'w', b'W', summary.as_bytes()).any(|at| {
                summary
                    .as_bytes()
                    .get(at..at + 8)
                    .is_some_and(|word| word.eq_ignore_ascii_case(b"workflow"))
            })
        {
            return (MessageKind::SubagentReturn, Origin::Subagent);
        }
    }
    (MessageKind::InjectedContext, Origin::Harness)
}

/// Recognize Claude Code's persisted-output stub — the note left in a `tool_result` when the real
/// output was too large and went to `<session>/tool-results/<id>.txt` (the model saw only this stub
/// plus a preview). Returns `(path, size hint)`.
fn persisted_output(text: &str) -> Option<(String, Option<String>)> {
    let t = text.trim_start();
    if !t.starts_with("<persisted-output>") {
        return None;
    }
    let after = &t[t.find("Full output saved to:")? + "Full output saved to:".len()..];
    let path = after.lines().next()?.trim();
    if path.is_empty() {
        return None;
    }
    let size = t
        .find("Output too large (")
        .map(|i| &t[i + "Output too large (".len()..])
        .and_then(|rest| rest.split(')').next())
        .map(str::to_string);
    Some((path.to_string(), size))
}

/// Sub-agent transcripts spawned by `parent_path`'s session (Claude Code's Task tool). They live at
/// `<projects>/<encoded>/<sid>/subagents/<agent>.jsonl` — a sibling `subagents/` dir next to the
/// parent's `<sid>.jsonl`. Returned as lightweight refs (newest first), scanned in parallel.
pub fn subagent_refs(parent_path: &std::path::Path) -> Vec<SessionRef> {
    let Some(dir) = sidecar::subagents_dir(parent_path) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                paths.push(p);
            }
        }
    }
    let mut refs = crate::par_filter_map(paths, |p| scan(&p).ok());
    refs.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    refs
}

/// A sub-agent transcript enriched with the metadata Claude Code records *around* it: the
/// `*.meta.json` sidecar (agent type / human description / the spawning tool_use id), the workflow
/// it belongs to (when spawned by a `Workflow`), and — for workflow agents — the structured result
/// the orchestrator journaled (`status` + `summary`, the agent's *real* return value).
///
/// This is the second dimension of a Claude session that flat parsing misses entirely: a deep
/// session fans out into hundreds of these, and the meta/journal sidecars are where their *purpose*
/// and *outcome* live.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentInfo {
    /// The transcript itself (id = `agent-<agentId>`), parseable like any other session.
    pub session: SessionRef,
    /// `agentType` from the sidecar: the subagent kind (`general-purpose`, `Explore`,
    /// `workflow-subagent`, a named custom agent, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// `description` from the sidecar: the human one-line task the parent gave it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `toolUseId` from the sidecar: the `Agent`/`Task` tool_use in the *parent* transcript that
    /// spawned this agent. Links the child back to the exact turn that launched it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The workflow run id (`wf_…`) this agent belongs to, or `None` for a directly-spawned
    /// (`Agent`/`Task`) sub-agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// For workflow agents: the journaled outcome status (`done` / `partial` / `failed` / …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_status: Option<String>,
    /// For workflow agents: the journaled result summary — the agent's structured return value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
}

impl SubagentInfo {
    /// `agentId` (the `agent-<id>` stem with the `agent-` prefix stripped) — the key the workflow
    /// journal and the transcript records use.
    pub fn agent_id(&self) -> &str {
        self.session.id.strip_prefix("agent-").unwrap_or(&self.session.id)
    }
}

/// The full sub-agent forest a Claude session spawned: every directly-spawned (`Agent`/`Task`)
/// sub-agent **and** every `Workflow` sub-agent (which live one level deeper, under
/// `subagents/workflows/<wf>/`), each enriched with its `*.meta.json` sidecar and — for workflow
/// agents — the orchestrator's journaled result.
///
/// Flat [`subagent_refs`] only sees the top-level `subagents/*.jsonl`, so it misses the entire
/// workflow tier (which on a heavy session is the *majority* of the agents) and drops the
/// meta/journal sidecars that carry each agent's purpose and outcome. Newest first.
pub fn subagent_tree(parent_path: &std::path::Path) -> Vec<SubagentInfo> {
    subagent_tree_inner(parent_path, crate::SubagentDemand::Rich, None, &|_, _| true)
}

/// Token aggregation needs the full forest and its metadata order, without task descriptions or
/// journaled return bodies. Unchanged child metadata reuses the versioned discovery scan cache.
pub(crate) fn subagent_session_refs(parent_path: &std::path::Path) -> Vec<SessionRef> {
    subagent_tree_inner(parent_path, crate::SubagentDemand::Sessions, None, &|_, _| true)
        .into_iter()
        .map(|s| s.session)
        .collect()
}

pub(crate) fn subagent_tree_filtered(
    parent_path: &Path,
    demand: crate::SubagentDemand,
    workflow: Option<&str>,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Vec<SubagentInfo> {
    subagent_tree_inner(parent_path, demand, workflow, select)
}

fn subagent_tree_inner(
    parent_path: &Path,
    demand: crate::SubagentDemand,
    workflow: Option<&str>,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Vec<SubagentInfo> {
    let Some(base) = sidecar::subagents_dir(parent_path) else {
        return vec![];
    };

    let mut out: Vec<SubagentInfo> = Vec::new();

    // Tier 1: directly-spawned sub-agents at `subagents/agent-*.jsonl`.
    if workflow.is_none() {
        out.extend(agents_in_dir(&base, None, demand, select));
    }

    // Tier 2: workflow sub-agents at `subagents/workflows/<wf>/agent-*.jsonl`, grouped by run id,
    // each annotated with the result the workflow `journal.jsonl` recorded for its agentId.
    if let Some(rd) = sidecar::workflow_runs_dir(parent_path).and_then(|d| fs::read_dir(d).ok()) {
        let wf_dirs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| workflow.is_none_or(|run| p.file_name().and_then(|name| name.to_str()) == Some(run)))
            .filter(|p| p.is_dir())
            .collect();
        let groups = crate::par_flat_map(wf_dirs, |wf_dir| {
            let run_id = wf_dir.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
            agents_in_dir(&wf_dir, Some(&run_id), demand, select)
        });
        out.extend(groups);
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.session.created_at));
    out
}

/// A run's journal as [`sidecar::journal_results`] reads it: `agentId → full result`.
type JournalResults = std::collections::BTreeMap<String, Value>;

#[cfg(test)]
type ForestReadTrace = (Option<PathBuf>, Vec<(&'static str, PathBuf)>);

#[cfg(test)]
static FOREST_READS: std::sync::Mutex<ForestReadTrace> = std::sync::Mutex::new((None, Vec::new()));
#[cfg(test)]
fn record_forest_read(kind: &'static str, path: &Path) {
    let mut trace = FOREST_READS.lock().unwrap();
    if trace.0.as_ref().is_some_and(|root| path.starts_with(root)) {
        trace.1.push((kind, path.to_path_buf()));
    }
}

/// Scan one directory for `agent-*.jsonl` transcripts, pairing each with its `*.meta.json` sidecar
/// and (for workflow dirs) the journaled result for its agentId. `wf` is `Some(run_id)`
/// for a workflow dir, `None` for the top-level `subagents/` dir.
fn agents_in_dir(
    dir: &Path,
    wf: Option<&str>,
    demand: crate::SubagentDemand,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Vec<SubagentInfo> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            // Only `agent-*.jsonl`; skip `journal.jsonl` and the `.meta.json` sidecars.
            let is_agent = p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"));
            if is_agent
                && p.file_stem()
                    .and_then(|id| id.to_str())
                    .is_some_and(|id| select(id, wf))
            {
                paths.push(p);
            }
        }
    }
    let rich = demand == crate::SubagentDemand::Rich;
    let journal = if rich && wf.is_some() && !paths.is_empty() {
        #[cfg(test)]
        record_forest_read("journal", &dir.join("journal.jsonl"));
        sidecar::journal_results(dir)
    } else {
        JournalResults::new()
    };
    crate::par_discovery_filter_map(paths, |p| {
        #[cfg(test)]
        record_forest_read("transcript", &p);
        let session = if rich {
            scan(&p).ok()?
        } else {
            // Retain the old open-failure exclusion even on a metadata cache hit.
            fs::File::open(&p).ok()?;
            cached_metadata(&p, false)?
        };
        let meta = match demand {
            crate::SubagentDemand::Rich => read_meta(&p.with_extension("meta.json")),
            crate::SubagentDemand::Tools => (read_agent_type(&p.with_extension("meta.json")), None, None),
            crate::SubagentDemand::Sessions => (None, None, None),
        };
        let agent_id = session.id.strip_prefix("agent-").unwrap_or(&session.id).to_string();
        let (status, summary) = wf
            .and_then(|_| journal.get(&agent_id))
            .map(sidecar::result_summary)
            .unwrap_or((None, None));
        Some(SubagentInfo {
            session,
            agent_type: meta.0,
            description: meta.1,
            tool_use_id: meta.2,
            workflow: wf.map(str::to_owned),
            result_status: status,
            result_summary: summary,
        })
    })
}

/// Tool analytics demand the agent type, not the task description or spawning-call metadata.
fn read_agent_type(path: &Path) -> Option<String> {
    #[cfg(test)]
    record_forest_read("metadata", path);
    let source = JsonSource::capture(path).ok()?;
    source
        .read_document(|view| crate::affordance::input::field_string(view, "agentType"))
        .ok()
        .flatten()
}

/// Read an `agent-*.meta.json` sidecar → `(agentType, description, toolUseId)`. Any read/parse
/// failure (or an absent file) is a clean `(None, None, None)`.
fn read_meta(path: &std::path::Path) -> (Option<String>, Option<String>, Option<String>) {
    #[cfg(test)]
    record_forest_read("metadata", path);
    let Ok(txt) = fs::read_to_string(path) else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return (None, None, None);
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    (s("agentType"), s("description"), s("toolUseId"))
}

/// The sub-agent's final return value as text: the last assistant text turn in its transcript
/// (what a `general-purpose`/`Explore` agent hands back to its parent, surfaced in the parent's
/// `Agent` tool_result). Reads one record at a time and retains the last substantive assistant text.
/// `None` for an empty/unreadable transcript or one with no substantive assistant text.
pub fn subagent_return(path: &std::path::Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut last: Option<String> = None;
    super::for_each_json_line(BufReader::new(file), |v| {
        if v.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(Value::Array(items)) = v.get("message").and_then(|m| m.get("content")) {
                let mut buf = String::new();
                for it in items {
                    if it.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            if !buf.is_empty() {
                                buf.push('\n');
                            }
                            buf.push_str(t);
                        }
                    }
                }
                if !buf.trim().is_empty() {
                    last = Some(buf);
                }
            }
        }
        Flow::Continue
    });
    last
}

/// Where a sub-agent transcript ends: the timestamp of its last conversational record (`user` or
/// `assistant`) and of its last `SubagentStop` hook attachment. Claude Code appends a
/// `SubagentStop` attachment each time the agent stops; a `SendMessage` resume appends new turns
/// after it. So `stopped_at > last_turn_at` means the agent is parked — finished or waiting —
/// and `last_turn_at > stopped_at` (or no stop at all) means it is running, or died without the
/// hook firing (a harness restart). Keeps timing and error facts, discarding ordinary content after each record.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SubagentEnd {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    /// The last API-error notice (`isApiErrorMessage`) with no real turn after it — what killed
    /// the agent, if anything did. A resume clears it (the next real record is a new turn).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_error: Option<ApiErrorNotice>,
}

/// One of Claude Code's synthetic API-error notices, as the transcript records it: the notice
/// text (`You've hit your session limit · resets 9pm (America/New_York)`, `Prompt is too long`),
/// the `error` class (`rate_limit`, `invalid_request`), the HTTP status when present, and the
/// quota's reset instant (`quotaLimits.resetsAt`, epoch seconds) for a rate limit.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ApiErrorNotice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

impl SubagentEnd {
    /// Parked (a `SubagentStop` fired after the last turn)? `None` when the transcript has no
    /// stop record at all — which is *also* what a killed harness leaves behind, so the caller
    /// pairs this with the parent's `<task-notification>` status before saying "running".
    pub fn stopped(&self) -> Option<bool> {
        let stop = self.stopped_at?;
        Some(self.last_turn_at.is_none_or(|t| stop >= t))
    }
}

pub fn subagent_end(path: &std::path::Path) -> SubagentEnd {
    let mut end = SubagentEnd::default();
    let Ok(file) = fs::File::open(path) else {
        return end;
    };
    super::for_each_json_line(BufReader::new(file), |v| {
        let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
        match v.get("type").and_then(Value::as_str) {
            Some(kind @ ("user" | "assistant")) => {
                if let Some(ts) = ts {
                    end.last_turn_at = Some(end.last_turn_at.map_or(ts, |t: DateTime<Utc>| t.max(ts)));
                }
                let msg = v.get("message").unwrap_or(&Value::Null);
                if v.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false) {
                    let text = msg
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .or_else(|| msg.get("content").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_default();
                    end.api_error = Some(ApiErrorNotice {
                        ts,
                        text,
                        error: v.get("error").and_then(Value::as_str).map(str::to_string),
                        status: v.get("apiErrorStatus").and_then(Value::as_u64),
                        resets_at: v
                            .pointer("/quotaLimits/resetsAt")
                            .and_then(Value::as_i64)
                            .and_then(|s| DateTime::from_timestamp(s, 0)),
                    });
                } else if kind == "user" || msg.get("model").and_then(Value::as_str) != Some(SYNTHETIC_MODEL) {
                    // A real turn after the notice: the agent was resumed; the error is history.
                    end.api_error = None;
                }
            }
            Some("attachment")
                if v.pointer("/attachment/hookEvent").and_then(Value::as_str) == Some("SubagentStop") =>
            {
                if let Some(ts) = ts {
                    end.stopped_at = Some(end.stopped_at.map_or(ts, |t: DateTime<Utc>| t.max(ts)));
                }
            }
            _ => {}
        }
        Flow::Continue
    });
    end
}

/// The harness-side status of one sub-agent, as the parent transcript's `<task-notification>`
/// records report it (`completed` / `failed` / `killed` / `stopped`), with the notification's
/// timestamp. A notification fires each time the agent stops, so this is the LAST one per
/// agent id.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskNotice {
    pub status: String,
    pub ts: Option<DateTime<Utc>>,
}

/// Scan a parent transcript for `<task-notification>` records and reduce them to the last notice
/// per `<task-id>` (the bare agent id for `Agent`-tool children; background shell tasks notify
/// under their own ids and ride along harmlessly). Two record shapes carry one: the
/// `queue-operation`/`enqueue` record Claude Code writes the moment a child stops (top-level
/// `content`), and the `user` turn it becomes when the queue drains into the conversation
/// (`message.content`). Most notifications exist only as the former — on one real session 1,229
/// against 105 — so reading only user turns misses the status of most lanes. Content is read only
/// for records that start with the tag.
pub fn task_notices(parent_path: &std::path::Path) -> std::collections::HashMap<String, TaskNotice> {
    let mut map = std::collections::HashMap::new();
    let Ok(file) = fs::File::open(parent_path) else {
        return map;
    };
    super::for_each_json_line(BufReader::new(file), |v| {
        let content = match v.get("type").and_then(Value::as_str) {
            Some("user") => v.pointer("/message/content"),
            Some("queue-operation") => v.get("content"),
            _ => None,
        };
        let Some(Value::String(content)) = content else {
            return Flow::Continue;
        };
        if !content.trim_start().starts_with("<task-notification>") {
            return Flow::Continue;
        }
        if let (Some(id), Some(status)) = (tag_inner(content, "task-id"), tag_inner(content, "status")) {
            map.insert(
                id.trim().to_string(),
                TaskNotice {
                    status: status.trim().to_string(),
                    ts: v.get("timestamp").and_then(Value::as_str).and_then(parse_ts),
                },
            );
        }
        Flow::Continue
    });
    map
}

/// Cheap metadata-only scan for `discover`.
fn cached_metadata(path: &Path, warn: bool) -> Option<SessionRef> {
    crate::discover_cache::cached_scan_continued(path, metadata::CONTINUATION_VERSION, |from| {
        match scan_at(path, from) {
            Ok((reference, checkpoint)) => {
                let continuation = checkpoint.and_then(|checkpoint| {
                    let head = crate::resume::MarkHead::of(path, &checkpoint, metadata::CONTINUATION_VERSION)?;
                    Some(crate::discover_cache::ScanContinuation {
                        head,
                        checkpoint,
                        dependencies: Default::default(),
                    })
                });
                (Some(reference), continuation)
            }
            Err(error) => {
                if warn {
                    eprintln!("cv: skipping {}: {error:#}", path.display());
                }
                (None, None)
            }
        }
    })
}

fn scan(path: &Path) -> Result<SessionRef> {
    Ok(scan_at(path, None)?.0)
}

fn scan_at(path: &Path, from: Option<&Checkpoint>) -> Result<(SessionRef, Option<Checkpoint>)> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    source.check_metadata(file.metadata()?)?;
    let (mut scan, offset) = metadata::Scan::restore(path, from);
    anyhow::ensure!(offset <= source.len(), "discovery checkpoint outside source");
    #[cfg(test)]
    metadata::FRAMED.with(|frames| frames.borrow_mut().push((offset, source.len() - offset)));
    scan.read(&source, &file, offset)?;
    source.check_metadata(file.metadata()?)?;
    let checkpoint = scan.checkpoint();
    Ok((scan.finish(), checkpoint))
}

/// The classification shared by owned and borrowed records. Content storage never chooses kind.
struct ConversationClass {
    synthetic: bool,
    api_error: bool,
    compact_summary: bool,
    sync_agent_return: bool,
    slash_command: bool,
    user: (MessageKind, Origin),
}

fn classify_conversation(role: Role, class: ConversationClass, complete: bool) -> (Role, MessageKind, Origin) {
    // Synthetic client-side notices are not model turns. Complete preserves their wire role for
    // round-trip; kind/origin still identify a harness notice in every fidelity.
    let role = if class.synthetic && !complete {
        Role::System
    } else {
        role
    };
    let (kind, origin) = if class.synthetic {
        (
            if class.api_error {
                MessageKind::Error
            } else {
                MessageKind::Notice
            },
            Origin::Harness,
        )
    } else if class.compact_summary {
        (MessageKind::CompactionSummary, Origin::Harness)
    } else {
        match role {
            Role::Tool if class.sync_agent_return => (MessageKind::SubagentReturn, Origin::Subagent),
            Role::Tool => (MessageKind::ToolResult, Origin::Harness),
            Role::Assistant => (MessageKind::Reply, Origin::Model),
            // New Claude versions write command invocations/output as user records. They are
            // the same harness bookkeeping older versions emitted as system/local_command.
            Role::User if class.slash_command => (MessageKind::Notice, Origin::Harness),
            Role::User => class.user,
            Role::System => (MessageKind::Notice, Origin::Harness),
        }
    };
    (role, kind, origin)
}

/// Distill a `stop_hook_summary` record into a readable one-liner, or `None` when the hook did
/// nothing worth surfacing (no output and no error). The record has no `content`; its activity lives
/// in `hookInfos[]` (each a fired hook, often with a `command` + captured output) and `hookErrors[]`.
fn render_hook_summary(v: &Value) -> Option<String> {
    let count = v.get("hookCount").and_then(Value::as_u64).unwrap_or(0);
    let errors = v.get("hookErrors").and_then(Value::as_array);
    let has_errors = errors.is_some_and(|e| !e.is_empty());
    let has_output = v.get("hasOutput").and_then(Value::as_bool).unwrap_or(false)
        || v.get("hookAdditionalContext")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    if !has_errors && !has_output {
        return None;
    }
    let label = if count == 1 {
        "hook".to_string()
    } else {
        format!("{count} hooks")
    };
    let mut s = format!("⛓ stop {label}");
    if let Some(infos) = v.get("hookInfos").and_then(Value::as_array) {
        // Name each fired hook by the head of its command (the most legible identifier).
        let names: Vec<String> = infos
            .iter()
            .filter_map(|i| i.get("command").and_then(Value::as_str))
            .map(|c| truncate(c.lines().next().unwrap_or(c).trim(), 60))
            .collect();
        if !names.is_empty() {
            s.push_str(&format!(": {}", names.join(" · ")));
        }
    }
    if let Some(ctx) = v
        .get("hookAdditionalContext")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    {
        s.push_str(&format!("\n  ↳ {}", truncate(ctx.trim(), 200)));
    }
    if let Some(errs) = errors.filter(|e| !e.is_empty()) {
        let joined: Vec<String> = errs
            .iter()
            .map(|e| truncate(e.as_str().unwrap_or(&e.to_string()).trim(), 100))
            .collect();
        s.push_str(&format!("\n  ✗ {}", joined.join("; ")));
    }
    Some(s)
}

/// Distill a slash-command `system`/`local_command` body to readable text. Two shapes occur:
/// the *invocation* (`<command-name>/foo</command-name>` + optional `<command-args>`) renders as
/// `/foo args`; the *output* (`<local-command-stdout>…</local-command-stdout>`) renders as the
/// captured stdout. Returns `None` when no recognizable tag is present (caller keeps the raw body).
fn render_slash_command(c: &str) -> Option<String> {
    if let Some(name) = tag_inner(c, "command-name") {
        let name = name.trim();
        let args = tag_inner(c, "command-args").map(|a| a.trim().to_string());
        return Some(match args.as_deref().filter(|a| !a.is_empty()) {
            Some(a) => format!("⌘ {name} {a}"),
            None => format!("⌘ {name}"),
        });
    }
    if let Some(out) = tag_inner(c, "local-command-stdout") {
        let out = out.trim();
        return Some(if out.is_empty() {
            "⌘ (no output)".to_string()
        } else {
            format!("⌘ stdout:\n{out}")
        });
    }
    None
}

/// Extract the inner text of the first `<tag>…</tag>` pair in `s`, if present.
fn tag_inner<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(&s[start..end])
}

const CLAUDE_EXTRA_TOP: &[&str] = &[
    "logicalParentUuid",
    "isSidechain",
    "isMeta",
    "isCompactSummary",
    "isApiErrorMessage",
    "agentId",
    "slug",
    "version",
    "requestId",
    "parentToolUseID",
    "toolUseID",
    "sourceToolAssistantUUID",
    "attributionAgent",
    "attributionMcpServer",
    "attributionMcpTool",
    "attributionSkill",
    "userType",
];
const CLAUDE_EXTRA_MSG: &[&str] = &["id", "stop_reason", "stop_sequence", "stop_details"];

fn keep_extra_top(key: &str, val: &Value) -> bool {
    !matches!((key, val), ("userType", Value::String(s)) if s == "external")
        && !matches!((key, val), ("isSidechain", Value::Bool(false)))
}

/// Top-level record fields the IR first-classes (so they must NOT be duplicated into `extra` —
/// the emitter reconstructs them from the [`Message`]'s own fields).
const CLAUDE_FIRSTCLASS_TOP: &[&str] = &["type", "uuid", "parentUuid", "timestamp", "message"];
/// `message.*` fields the IR first-classes (reconstructed from `content`/`usage`/`model`/role).
const CLAUDE_FIRSTCLASS_MSG: &[&str] = &["content", "usage", "role", "model"];
/// Prefix under which a captured `message.<key>` sub-field is stashed in `extra`, so the emitter can
/// route it back inside the `message` object on replay.
pub const MESSAGE_EXTRA_PREFIX: &str = "message.";

fn attach_tool_use_result(blocks: &mut [Block], tur: Value) {
    let Some(Block::ToolResult { details, .. }) = blocks.iter_mut().find(|b| matches!(b, Block::ToolResult { .. }))
    else {
        return;
    };
    *details = Some(match (tur, details.take()) {
        (Value::Object(mut sidecar), Some(Value::Object(derived))) => {
            sidecar.extend(derived);
            Value::Object(sidecar)
        }
        (sidecar, Some(Value::Object(mut derived))) => {
            derived.insert(TOOL_USE_RESULT_KEY.into(), sidecar);
            Value::Object(derived)
        }
        (sidecar, _) => sidecar,
    });
}

#[cfg(test)]
impl Claude {
    /// Test-only: point the adapter at an explicit projects root.
    fn for_root(root: std::path::PathBuf) -> Self {
        Claude { entries: vec![root] }
    }
}

/// `<config>/tasks/<session id>/*.json` (`{id, subject, description, status, blocks, blockedBy}`)
/// as the session's plan, found from the transcript's own path (`<config>/projects/<p>/<id>.jsonl`).
fn task_list(transcript: &Path, id: &str) -> Option<crate::affordance::Affordance> {
    let config = transcript
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "projects"))?
        .parent()?;
    let dir = config.join("tasks").join(id);
    let items: Vec<_> = crate::affordance::json_files(&dir)
        .iter()
        .filter_map(|(_, v)| crate::affordance::item_of(v))
        .collect();
    crate::affordance::plan_state(crate::affordance::Kind::Plan, "tasks/", items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lazy::INLINE_MAX;
    use std::sync::LazyLock;

    static NO_BAG: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

    /// The message's `extra["claude"]` bag (empty when it recorded nothing).
    fn cl(m: &Message) -> &Map<String, Value> {
        m.harness_extra(Harness::Claude).unwrap_or(&NO_BAG)
    }

    /// The structured `details` of the message's first tool-result block — where the record's
    /// `toolUseResult` sidecar lives (INTERFACE-V2 §4).
    fn details(m: &Message) -> &Value {
        m.content
            .iter()
            .find_map(|b| match b {
                Block::ToolResult { details, .. } => details.as_ref(),
                _ => None,
            })
            .expect("tool result details")
    }

    #[test]
    fn complete_mode_carries_dropped_meta_records() {
        // A session mixing conversational turns with the meta records the lean passes drop.
        let sid = "s1";
        let text = [
            r#"{"type":"user","sessionId":"s1","uuid":"u0","timestamp":"2026-06-16T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"mode","sessionId":"s1","mode":"plan"}"#,
            r#"{"type":"queue-operation","sessionId":"s1","operation":"enqueue","content":"later","timestamp":"2026-06-16T00:00:01Z"}"#,
            r#"{"type":"ai-title","sessionId":"s1","aiTitle":"My Session"}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a1","parentUuid":"u0","timestamp":"2026-06-16T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ].join("\n");

        // Default (full, not complete): meta records dropped — only the 2 conversational turns.
        let mut sink = CollectSink::default();
        stream_str(sid, &text, None, &ParseOptions::full(), &mut sink);
        let convo: Vec<_> = sink
            .messages
            .iter()
            .filter(|m| m.extra.contains_key(CARRIER_KEY))
            .collect();
        assert_eq!(sink.messages.len(), 2, "full mode keeps only conversational turns");
        assert_eq!(convo.len(), 0, "no carriers in full mode");

        // Complete: every record present; the 3 meta records become carriers holding the original.
        let mut sink = CollectSink::default();
        let session = stream_str(sid, &text, None, &ParseOptions::complete(), &mut sink);
        let carriers: Vec<_> = sink
            .messages
            .iter()
            .filter(|m| m.extra.contains_key(CARRIER_KEY))
            .collect();
        assert_eq!(sink.messages.len(), 5, "complete mode carries every record");
        assert_eq!(carriers.len(), 3, "mode + queue-operation + ai-title carried");
        // carriers stash the verbatim original record
        let types: Vec<&str> = carriers
            .iter()
            .filter_map(|m| m.extra.get(CARRIER_KEY)?.get("type")?.as_str())
            .collect();
        assert!(
            types.contains(&"mode") && types.contains(&"queue-operation") && types.contains(&"ai-title"),
            "got {types:?}"
        );
        // ai-title still sets the session title (IR convenience) AND round-trips as a carrier
        assert_eq!(session.title.as_deref(), Some("My Session"));
        // carriers are empty-content System turns, so message_count (user+assistant) is unaffected
        assert!(carriers.iter().all(|m| m.content.is_empty() && m.role == Role::System));
    }

    #[test]
    fn rendered_attachments_surface_as_system_reminders() {
        // Since ~2.1.23x the system reminders Claude Code appends to the prompt are `attachment`
        // records; `rendered[].content` is exactly what the model saw. Unrendered ones (a hook that
        // printed nothing) are bookkeeping only.
        let text = [
            r#"{"type":"user","sessionId":"s1","uuid":"u0","timestamp":"2026-09-19T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"attachment","sessionId":"s1","uuid":"t1","parentUuid":"u0","timestamp":"2026-09-19T00:00:01Z","attachment":{"type":"hook_success","hookName":"UserPromptSubmit","stdout":"{}\n"}}"#,
            r#"{"type":"attachment","sessionId":"s1","uuid":"t2","parentUuid":"t1","timestamp":"2026-09-19T00:00:02Z","attachment":{"type":"edited_text_file","filename":"/x.md","snippet":"1\tfoo"},"rendered":[{"content":"<system-reminder>\nNote: /x.md was modified.\n</system-reminder>"}],"renderedInHumanTurn":true}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a1","parentUuid":"t2","timestamp":"2026-09-19T00:00:03Z","message":{"role":"assistant","model":"claude-fable-5-1","content":[{"type":"text","text":"ok"}]}}"#,
        ]
        .join("\n");
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &ParseOptions::bulk(), &mut sink);
        assert_eq!(
            sink.messages.len(),
            3,
            "user + rendered attachment + assistant; the silent hook is dropped"
        );
        let att = &sink.messages[1];
        assert_eq!(att.role, Role::System);
        assert_eq!(
            cl(att).get("attachment_type").and_then(Value::as_str),
            Some("edited_text_file")
        );
        assert_eq!(att.id.as_deref(), Some("t2"));
        assert_eq!(att.parent_id.as_deref(), Some("t1"));
        assert!(att.text().unwrap().contains("/x.md was modified"));
        assert!(!cl(att).contains_key("attachment"), "bulk keeps extra lean");

        // full: the attachment object rides along
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &ParseOptions::full(), &mut sink);
        assert_eq!(cl(&sink.messages[1])["attachment"]["filename"], "/x.md");
        assert_eq!(cl(&sink.messages[1])["renderedInHumanTurn"], true);

        // complete: both attachments carried verbatim; the rendered one still carries its text
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &ParseOptions::complete(), &mut sink);
        assert_eq!(sink.messages.len(), 4);
        let carried: Vec<_> = sink
            .messages
            .iter()
            .filter(|m| m.extra.contains_key(CARRIER_KEY))
            .collect();
        assert_eq!(carried.len(), 2);
        assert_eq!(carried[0].extra[CARRIER_KEY]["attachment"]["type"], "hook_success");
        assert!(carried[0].content.is_empty());
        assert_eq!(carried[1].extra[CARRIER_KEY]["attachment"]["type"], "edited_text_file");
        assert!(!carried[1].content.is_empty(), "rendered carrier keeps its text");
    }

    #[test]
    fn synthetic_assistant_notices_are_system_not_turns() {
        // Claude Code's client-side notices ("Prompt is too long", "No response requested.") are
        // `assistant` records with `model: "<synthetic>"`: not model output, never sent to the API.
        let text = [
            r#"{"type":"user","sessionId":"s1","uuid":"u0","timestamp":"2026-09-19T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"e1","parentUuid":"u0","timestamp":"2026-09-19T00:00:01Z","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"Prompt is too long"}],"usage":{"input_tokens":0,"output_tokens":0}},"isApiErrorMessage":true,"error":"invalid_request"}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"n1","parentUuid":"e1","timestamp":"2026-09-19T00:00:02Z","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]},"isApiErrorMessage":false}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a1","parentUuid":"n1","timestamp":"2026-09-19T00:00:03Z","message":{"role":"assistant","model":"claude-fable-5-1","content":[{"type":"text","text":"ok"}]}}"#,
        ]
        .join("\n");
        let mut sink = CollectSink::default();
        let session = stream_str("s1", &text, None, &ParseOptions::full(), &mut sink);
        let e1 = &sink.messages[1];
        assert_eq!(e1.role, Role::System);
        assert_eq!(e1.kind, MessageKind::Error);
        assert_eq!(e1.origin, Origin::Harness);
        assert_eq!(cl(e1).get("error").and_then(Value::as_str), Some("invalid_request"));
        assert_eq!(e1.model, None);
        assert_eq!(e1.text().as_deref(), Some("Prompt is too long"));
        assert_eq!(
            sink.messages[2].kind,
            MessageKind::Notice,
            "\"No response requested.\" is a notice"
        );
        assert_eq!(
            session.model.as_deref(),
            Some("claude-fable-5-1"),
            "a notice never names the session model"
        );

        // complete: the record keeps its own shape for round-trip
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &ParseOptions::complete(), &mut sink);
        assert_eq!(sink.messages[1].role, Role::Assistant);
        assert_eq!(
            sink.messages[1].kind,
            MessageKind::Error,
            "the kind is an IR fact even when the wire role is kept"
        );
        assert_eq!(sink.messages[1].model.as_deref(), Some("<synthetic>"));

        // scan(): message_count skips both notices
        let dir = std::env::temp_dir().join(format!("cv-claude-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s1.jsonl");
        std::fs::write(&p, format!("{text}\n")).unwrap();
        assert_eq!(scan(&p).unwrap().message_count, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn custom_title_outranks_ai_title_and_bookkeeping_records_carry() {
        let text = [
            r#"{"type":"ai-title","aiTitle":"Auto title","sessionId":"s1"}"#,
            r#"{"type":"custom-title","customTitle":"My rename","sessionId":"s1"}"#,
            r#"{"type":"ai-title","aiTitle":"Auto title v2","sessionId":"s1"}"#,
            r#"{"type":"agent-name","agentName":"Debug docker","sessionId":"s1"}"#,
            r#"{"type":"continued-in","timestamp":"2026-09-03T12:45:08.257Z","sessionId":"s1","continuedInSessionId":"s2"}"#,
            r#"{"type":"cost-state","sessionId":"s1","totalCostUSD":1.5}"#,
            r#"{"type":"pr-link","sessionId":"s1","prNumber":1,"prUrl":"https://github.com/o/r/pull/1","timestamp":"2026-09-02T04:04:30.481Z"}"#,
            r#"{"type":"some-future-record","sessionId":"s1","payload":true}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u0","timestamp":"2026-09-19T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
        ]
        .join("\n");
        let mut sink = CollectSink::default();
        let session = stream_str("s1", &text, None, &ParseOptions::full(), &mut sink);
        assert_eq!(
            session.title.as_deref(),
            Some("My rename"),
            "/rename beats a later ai-title"
        );
        let bag = session.harness_extra(Harness::Claude).expect("claude session bag");
        assert_eq!(bag["agent_name"], "Debug docker");
        assert_eq!(bag["pr_links"][0], "https://github.com/o/r/pull/1");
        assert_eq!(
            session.lineage.continued_in.as_deref(),
            Some("s2"),
            "a continuation is lineage, not a bag fact"
        );
        assert!(
            session.extra.keys().all(|k| k == "claude"),
            "session extra is nested by harness: {:?}",
            session.extra.keys().collect::<Vec<_>>()
        );
        assert_eq!(sink.messages.len(), 1, "bookkeeping records are not turns");

        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &ParseOptions::complete(), &mut sink);
        assert_eq!(
            sink.messages.len(),
            9,
            "complete mode carries every record, known or not"
        );
        assert!(sink.messages.iter().any(|m| m
            .extra
            .get(CARRIER_KEY)
            .and_then(|r| r.get("type"))
            .and_then(Value::as_str)
            == Some("some-future-record")));

        // scan() agrees on the title
        let dir = std::env::temp_dir().join(format!("cv-claude-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s1.jsonl");
        std::fs::write(&p, format!("{text}\n")).unwrap();
        assert_eq!(scan(&p).unwrap().title.as_deref(), Some("My rename"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persisted_output_stubs_are_annotated() {
        // A too-large tool output goes to `<session>/tool-results/<id>.txt`; the transcript (and the
        // model) get only this stub. The stub stays the content; the path rides in `details`.
        let stub = "<persisted-output>\nOutput too large (66KB). Full output saved to: /tmp/proj/sess/tool-results/bgz.txt\n\nPreview (first 2KB):\nhello\n</persisted-output>";
        let line = serde_json::json!({"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}})
        .to_string();
        let mut sink = CollectSink::default();
        stream_str("s1", &line, None, &ParseOptions::bulk(), &mut sink);
        let Block::ToolResult { content, details, .. } = &sink.messages[0].content[0] else {
            panic!("expected a tool result");
        };
        assert!(
            content.contains("Preview"),
            "the stub — what the model saw — stays the content"
        );
        let d = details.as_ref().expect("persisted output recorded");
        assert_eq!(d["persistedOutput"]["path"], "/tmp/proj/sess/tool-results/bgz.txt");
        assert_eq!(d["persistedOutput"]["size"], "66KB");
        // an ordinary result carries no details
        let plain = serde_json::json!({"type":"user","sessionId":"s1","uuid":"u2","timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}})
        .to_string();
        let mut sink = CollectSink::default();
        stream_str("s1", &plain, None, &ParseOptions::bulk(), &mut sink);
        let Block::ToolResult { details, .. } = &sink.messages[0].content[0] else {
            panic!()
        };
        assert!(details.is_none());
    }

    /// Every record field the Claude adapter keeps that the IR does not first-class, with the
    /// spelling Claude itself uses. Nesting them under `extra["claude"]` (IR v2) is what keeps the
    /// top level of `extra` free for cv's own bookkeeping.
    const FORMER_FLAT_KEYS: &[&str] = &[
        "logicalParentUuid",
        "isSidechain",
        "isMeta",
        "isCompactSummary",
        "isApiErrorMessage",
        "agentId",
        "slug",
        "version",
        "requestId",
        "parentToolUseID",
        "toolUseID",
        "sourceToolAssistantUUID",
        "attributionAgent",
        "attributionMcpServer",
        "attributionMcpTool",
        "attributionSkill",
        "userType",
        "message.id",
        "message.stop_reason",
        "message.stop_sequence",
        "message.stop_details",
    ];

    /// Two records covering everything `collect_extra` captures: one carrying every non-first-class
    /// field, one carrying a `toolUseResult` sidecar.
    fn kitchen_sink_transcript() -> String {
        let meta = serde_json::json!({
            "type": "user", "sessionId": "s1", "uuid": "u1", "parentUuid": null,
            "timestamp": "2026-09-19T00:00:00Z",
            "logicalParentUuid": "u0", "isSidechain": true, "isMeta": true,
            "isCompactSummary": true, "isApiErrorMessage": false,
            "agentId": "a4a55af", "slug": "magical-wondering-rabbit", "version": "2.1.278",
            "requestId": "req_123", "parentToolUseID": "toolu_parent", "toolUseID": "toolu_self",
            "sourceToolAssistantUUID": "u-src", "attributionAgent": "general-purpose",
            "attributionMcpServer": "scry", "attributionMcpTool": "sql",
            "attributionSkill": "code-review", "userType": "internal",
            "message": {"role": "user", "content": "hi", "id": "msg_abc", "stop_reason": "tool_use",
                        "stop_sequence": "</done>", "stop_details": {"type": "max_tokens"}},
        });
        let result = serde_json::json!({
            "type": "user", "sessionId": "s1", "uuid": "u2", "parentUuid": "u1",
            "timestamp": "2026-09-19T00:00:01Z",
            "toolUseResult": {"filePath": "/w/a.rs", "structuredPatch": [{"oldStart": 1}]},
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
        });
        format!("{meta}\n{result}\n")
    }

    #[test]
    fn tool_use_result_is_block_details_and_every_other_fact_is_nested() {
        let text = kitchen_sink_transcript();
        let s = parse_str("s1", &text, None);

        // 1. The sidecar is block-level `details` (INTERFACE-V2 §4), not a bag key.
        let tool = s.messages.iter().find(|m| m.role == Role::Tool).expect("tool turn");
        assert_eq!(details(tool)["filePath"], "/w/a.rs");
        assert!(details(tool).get("structuredPatch").is_some());
        assert!(
            !cl(tool).contains_key(TOOL_USE_RESULT_KEY),
            "not duplicated into the bag"
        );

        // 2. Every remaining Claude fact is in the bag, with Claude's own spelling.
        let meta = &s.messages[0];
        for k in FORMER_FLAT_KEYS {
            assert!(cl(meta).contains_key(*k), "`{k}` must live in extra[\"claude\"]");
        }
        assert_eq!(cl(meta)["agentId"], "a4a55af");
        assert_eq!(cl(meta)["message.stop_reason"], "tool_use");
        assert_eq!(cl(meta)["message.stop_details"]["type"], "max_tokens");
        assert_eq!(cl(meta)["userType"], "internal");

        // 3. Nothing but the harness bag at the top level of `extra` — and under the strongest
        //    options (complete + carriers + offset stamping) only cv's own two bookkeeping keys
        //    join it.
        for m in &s.messages {
            assert!(
                m.extra.keys().all(|k| k == "claude"),
                "top-level extra must hold only the harness bag, got {:?}",
                m.extra.keys().collect::<Vec<_>>()
            );
        }
        let dir = std::env::temp_dir().join(format!("cv-claude-bag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        // A bookkeeping record so the carrier key is exercised too.
        std::fs::write(&path, format!("{{\"type\":\"mode\",\"sessionId\":\"s1\"}}\n{text}")).unwrap();
        let opts = ParseOptions {
            extra: true,
            complete: true,
            spans: true,
            offsets: true,
            ..Default::default()
        };
        let mut sink = CollectSink::default();
        stream_file("s1", &path, &opts, &mut sink);
        let allowed = ["claude", CARRIER_KEY, crate::offsets::OFFSET_KEY];
        let mut saw_carrier = false;
        let mut saw_offset = false;
        for m in &sink.messages {
            for k in m.extra.keys() {
                assert!(allowed.contains(&k.as_str()), "unexpected top-level extra key `{k}`");
                saw_carrier |= k == CARRIER_KEY;
                saw_offset |= k == crate::offsets::OFFSET_KEY;
            }
        }
        assert!(
            saw_carrier && saw_offset,
            "the two bookkeeping keys must actually be exercised"
        );

        // 4. Complete mode keeps a verbatim bag copy *as well* — the byte-exact replay fallback —
        //    while `details` stays the sidecar's real home.
        let tool = sink
            .messages
            .iter()
            .find(|m| m.content.iter().any(|b| matches!(b, Block::ToolResult { .. })))
            .expect("tool turn");
        assert_eq!(cl(tool)[TOOL_USE_RESULT_KEY]["filePath"], "/w/a.rs");
        assert_eq!(details(tool)["filePath"], "/w/a.rs");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persisted_output_pointer_rides_alongside_the_sidecar() {
        // cv's derived `persistedOutput` pointer and Claude's own sidecar must BOTH survive on the
        // one `details` slot: merged when the sidecar is an object, parked side by side when it is
        // a bare string (Claude writes those for tool errors).
        let stub = "<persisted-output>\nOutput too large (66KB). Full output saved to: /tmp/s/tool-results/b.txt\n\nPreview (first 2KB):\nhi\n</persisted-output>";
        let obj = serde_json::json!({"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2026-09-19T00:00:00Z",
            "toolUseResult":{"stdout":"hi","persistedOutputPath":"/tmp/s/tool-results/b.txt"},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}});
        let scalar = serde_json::json!({"type":"user","sessionId":"s1","uuid":"u2","timestamp":"2026-09-19T00:00:01Z",
            "toolUseResult":"Error: Exit code 64",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":stub,"is_error":true}]}});
        let s = parse_str("s1", &format!("{obj}\n{scalar}\n"), None);

        let d = details(&s.messages[0]);
        assert_eq!(d["stdout"], "hi", "the sidecar survives");
        assert_eq!(d[PERSISTED_OUTPUT_KEY]["path"], "/tmp/s/tool-results/b.txt");
        assert_eq!(d[PERSISTED_OUTPUT_KEY]["size"], "66KB");
        assert_eq!(
            s.messages[0].content[0].persisted_output_path(),
            Some("/tmp/s/tool-results/b.txt")
        );

        let d = details(&s.messages[1]);
        assert_eq!(
            d[TOOL_USE_RESULT_KEY], "Error: Exit code 64",
            "a scalar sidecar is parked, not dropped"
        );
        assert_eq!(d[PERSISTED_OUTPUT_KEY]["path"], "/tmp/s/tool-results/b.txt");
    }

    #[test]
    fn discovery_excludes_prune_flat_sidecar() {
        let root = std::env::temp_dir().join(format!("cv-disc-{}", uuid::Uuid::new_v4()));
        let proj = root.join("-Users-x-proj");
        fs::create_dir_all(&proj).unwrap();
        let sid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        // a real session...
        fs::write(
            proj.join(format!("{sid}.jsonl")),
            serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-16T00:00:00Z",
                "message":{"role":"user","content":"hi"}})
            .to_string()
                + "\n",
        )
        .unwrap();
        // ...and a prune sidecar sitting right next to it (must NOT be discovered as a session).
        fs::write(
            proj.join(format!("{sid}.flat.jsonl")),
            serde_json::json!({"id":"toolu_1","slot":"content","name":"Read","content":"big","size":3,"line_count":1,"kind":"text"}).to_string() + "\n",
        ).unwrap();

        let refs = Claude::for_root(root.clone()).discover().unwrap();
        assert_eq!(refs.len(), 1, "the .flat.jsonl sidecar must be excluded");
        assert_eq!(refs[0].id, sid);
        fs::remove_dir_all(&root).ok();
    }

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude/");
        std::fs::read_to_string(format!("{path}{name}")).unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn rich_blocks_variant() {
        let s = parse_str("s1", &fixture("rich_blocks.jsonl"), None);
        // session-level metadata: ai-title wins over summary; cwd / git / model captured.
        assert_eq!(s.title.as_deref(), Some("Parser refactor session"));
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/work/proj")));
        assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));

        // user, assistant, tool-result turn, image/document turn.
        assert_eq!(s.messages.len(), 4);

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        // thinking (with signature), redacted_thinking (encrypted), text, tool_use all preserved.
        let kinds: Vec<_> = asst.content.iter().collect();
        assert!(matches!(kinds[0], Block::Thinking { signature: Some(sig), .. } if sig == "SIGREDACTED=="));
        assert!(
            matches!(kinds[1], Block::Thinking { text, encrypted: Some(e), signature: None, .. } if text.is_empty() && e == "ENCREDACTED==")
        );
        assert!(matches!(kinds[2], Block::Text { .. }));
        assert!(matches!(kinds[3], Block::ToolUse { name, .. } if name == "Edit"));
        // assistant API metadata + requestId captured in extra.
        assert_eq!(cl(asst).get("message.stop_reason").unwrap(), "tool_use");
        assert_eq!(cl(asst).get("message.id").unwrap(), "msg_abc");
        assert_eq!(cl(asst).get("requestId").unwrap(), "req_123");

        // tool-result user line is reclassified as a Tool turn, and the rich sidecar is preserved
        // — on the BLOCK, as shared `details`, not in the harness bag.
        let tool = &s.messages[2];
        assert_eq!(tool.role, Role::Tool);
        let tur = details(tool);
        assert!(tur.get("structuredPatch").is_some());
        assert_eq!(tur.get("filePath").unwrap(), "/work/proj/a.rs");
        assert!(
            !cl(tool).contains_key(TOOL_USE_RESULT_KEY),
            "the sidecar is block-level `details`; the bag keeps a copy only under `complete`"
        );

        // image + document.
        let media = &s.messages[3];
        assert!(
            matches!(&media.content[0], Block::Image { media_type: Some(m), data_ref: Some(r) } if m == "image/png" && r == "base64:inline")
        );
        assert!(
            matches!(&media.content[1], Block::File { mime: Some(m), path: Some(p), .. } if m == "application/pdf" && p == "spec.pdf")
        );
    }

    #[test]
    fn system_lines_variant() {
        let s = parse_str("s2", &fixture("system_lines.jsonl"), None);
        let systems: Vec<_> = s.messages.iter().filter(|m| m.role == Role::System).collect();
        // compact_boundary, away_summary, local_command, api_error surface; turn_duration +
        // agents_killed (bodyless) are dropped.
        assert_eq!(systems.len(), 4);
        assert!(systems
            .iter()
            .any(|m| m.text().as_deref() == Some("Conversation compacted")
                && m.kind == MessageKind::CompactionBoundary
                && cl(m).get("compactMetadata").is_some()));
        assert!(systems.iter().any(|m| cl(m).get("subtype").unwrap() == "away_summary"));
        assert!(systems
            .iter()
            .any(|m| m.kind == MessageKind::Error && cl(m).get("level").unwrap() == "error"));
        // no bodyless subtypes leaked in.
        assert!(!systems.iter().any(|m| matches!(
            cl(m).get("subtype").and_then(Value::as_str),
            Some("turn_duration" | "agents_killed")
        )));
    }

    #[test]
    fn stop_hook_summary_with_output_surfaces_and_keeps_hook_metadata() {
        // A Stop-hook that fired with output (and a sibling that did nothing): the first surfaces
        // as a readable ⛓ line carrying its hook command + context; the no-op one stays dropped.
        let text = [
            // Hook with output + a command name → surfaced.
            r#"{"type":"system","subtype":"stop_hook_summary","uuid":"h1","timestamp":"2026-06-11T00:00:00Z","hookCount":1,"hasOutput":true,"hookAdditionalContext":"keep working all night","hookInfos":[{"command":"alright well i'm going to bed. so.... best of luck"}],"hookErrors":[],"stopReason":"hook","toolUseID":"tu1"}"#,
            // No output, no error → dropped from the stream (a no-op hook summary).
            r#"{"type":"system","subtype":"stop_hook_summary","uuid":"h2","timestamp":"2026-06-11T00:01:00Z","hookCount":1,"hasOutput":false,"hookInfos":[{"command":"noop"}],"hookErrors":[]}"#,
            // A model_refusal_fallback: has content AND rich metadata that must ride in extra.
            r#"{"type":"system","subtype":"model_refusal_fallback","uuid":"r1","timestamp":"2026-06-11T00:02:00Z","content":"Fable 5's safety measures flagged this message.","originalModel":"claude-fable-5[1m]","fallbackModel":"claude-opus-4-8","trigger":"refusal"}"#,
        ]
        .join("\n");
        let s = parse_str("hooks", &text, None);
        let systems: Vec<_> = s.messages.iter().filter(|m| m.role == Role::System).collect();
        // The hook-with-output and the refusal surface; the no-op hook does not.
        assert_eq!(
            systems.len(),
            2,
            "got {:?}",
            systems.iter().map(|m| m.text()).collect::<Vec<_>>()
        );

        let hook = systems
            .iter()
            .find(|m| cl(m).get("subtype").and_then(Value::as_str) == Some("stop_hook_summary"))
            .unwrap();
        let body = hook.text().unwrap();
        assert!(body.starts_with("⛓ stop hook"), "hook body: {body:?}");
        assert!(body.contains("best of luck"), "hook command surfaced: {body:?}");
        assert!(
            body.contains("keep working all night"),
            "hook context surfaced: {body:?}"
        );
        // Hook metadata preserved in extra.
        assert_eq!(cl(hook).get("hookCount").unwrap(), 1);
        assert!(cl(hook).get("hookInfos").is_some());
        assert_eq!(cl(hook).get("stopReason").unwrap(), "hook");

        // Refusal-fallback metadata preserved.
        let refusal = systems
            .iter()
            .find(|m| cl(m).get("subtype").and_then(Value::as_str) == Some("model_refusal_fallback"))
            .unwrap();
        assert_eq!(cl(refusal).get("originalModel").unwrap(), "claude-fable-5[1m]");
        assert_eq!(cl(refusal).get("fallbackModel").unwrap(), "claude-opus-4-8");
    }

    #[test]
    fn sidechain_subagent_variant() {
        let s = parse_str("sub1", &fixture("sidechain_subagent.jsonl"), None);
        // progress / started / result are non-conversational and skipped.
        assert_eq!(s.messages.len(), 2);
        let first = &s.messages[0];
        // isSidechain / isMeta / agentId / slug preserved for sub-agent reconstruction.
        assert_eq!(cl(first).get("isSidechain").unwrap(), true);
        assert_eq!(cl(first).get("isMeta").unwrap(), true);
        assert_eq!(cl(first).get("agentId").unwrap(), "a4a55af");
        assert_eq!(cl(first).get("slug").unwrap(), "magical-wondering-rabbit");
        // attribution metadata on the assistant turn.
        assert_eq!(cl(&s.messages[1]).get("attributionAgent").unwrap(), "general-purpose");
    }

    #[test]
    fn compact_summary_variant() {
        let s = parse_str("s3", &fixture("compact_summary.jsonl"), None);
        let first = &s.messages[0];
        assert_eq!(cl(first).get("isCompactSummary").unwrap(), true);
        // todo sidecar preserved on the tool turn, as block-level `details`.
        let tool = s.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(details(tool).get("newTodos").is_some());
    }

    #[test]
    fn slash_command_records_render_readably() {
        // The invocation record (`<command-name>` + `<command-args>`) and the output record
        // (`<local-command-stdout>`) both distill to clean text, not raw pseudo-XML.
        let text = [
            r#"{"type":"system","subtype":"local_command","uuid":"c1","timestamp":"2026-06-01T00:00:00Z","content":"<command-name>/usage</command-name>\n  <command-message>usage</command-message>\n  <command-args>--verbose</command-args>"}"#,
            r#"{"type":"system","subtype":"local_command","uuid":"c2","timestamp":"2026-06-01T00:00:01Z","content":"<local-command-stdout>Settings dialog dismissed</local-command-stdout>"}"#,
        ]
        .join("\n");
        let s = parse_str("slash", &text, None);
        let bodies: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert!(bodies.iter().any(|b| b == "⌘ /usage --verbose"), "got {bodies:?}");
        assert!(
            bodies
                .iter()
                .any(|b| b.starts_with("⌘ stdout:") && b.contains("Settings dialog dismissed")),
            "got {bodies:?}"
        );
        // The raw tag text must NOT survive into the rendered body.
        assert!(
            !bodies.iter().any(|b| b.contains("<command-name>")),
            "raw tag leaked: {bodies:?}"
        );
    }

    /// Claude Code ≥ 2.1 writes the slash-command invocation/output records as `user` rows. They
    /// are harness bookkeeping (kind `notice`, origin `harness`); the person's own `/compact` line
    /// just before them stays a human prompt — `cv show --kind prompt` keys on exactly this split.
    #[test]
    fn slash_command_user_rows_are_harness_notices_not_prompts() {
        let text = [
            r#"{"type":"user","uuid":"p1","timestamp":"2026-09-30T18:20:00.612Z","message":{"role":"user","content":"/compact"}}"#,
            r#"{"type":"user","uuid":"c1","parentUuid":"p1","timestamp":"2026-09-30T18:20:00.618Z","message":{"role":"user","content":"<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>"}}"#,
            r#"{"type":"user","uuid":"c2","parentUuid":"c1","timestamp":"2026-09-30T18:22:27.150Z","message":{"role":"user","content":"<local-command-stdout>Compacted</local-command-stdout>"}}"#,
            r#"{"type":"user","uuid":"p2","parentUuid":"c2","timestamp":"2026-09-30T18:38:45.941Z","message":{"role":"user","content":"hi :) we /compacted. <command-name> is a thing I typed"}}"#,
        ]
        .join("\n");
        let s = parse_str("slash-user", &text, None);
        let kinds: Vec<(MessageKind, Origin)> = s.messages.iter().map(|m| (m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (MessageKind::Prompt, Origin::Human),
                (MessageKind::Notice, Origin::Harness),
                (MessageKind::Notice, Origin::Harness),
                (MessageKind::Prompt, Origin::Human),
            ],
            "{kinds:?}"
        );
        // The text itself is untouched (round-trips stay byte-honest); only the classification moves.
        assert!(s.messages[1].text().unwrap().starts_with("<command-name>"));
    }

    #[test]
    fn a_wakeup_turn_is_the_scheduler_and_a_task_notification_is_the_harness() {
        // Shapes from the 2.1.296 corpus: a ScheduleWakeup fire and an agent's task-notification.
        let text = [
            r#"{"type":"user","uuid":"w","timestamp":"2026-10-11T20:23:00Z","isMeta":true,"promptSource":"system","turnOrigin":"scheduled","message":{"role":"user","content":"say awake"}}"#,
            r#"{"type":"user","uuid":"n","parentUuid":"w","timestamp":"2026-10-11T20:24:00Z","origin":{"kind":"task-notification","producer":"session-task"},"promptSource":"system","turnOrigin":"task_notification","message":{"role":"user","content":"<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
        ]
        .join("\n");
        let s = parse_str("wakeup", &text, None);
        let origins: Vec<Origin> = s.messages.iter().map(|m| m.origin).collect();
        assert_eq!(origins, vec![Origin::Scheduler, Origin::Harness]);
    }

    /// The `user` rows Claude Code writes itself are not prompts: an agent's or workflow's
    /// task-notification is the sub-agent's return, a command's is injected context, and so are
    /// the interrupt marker and `isMeta` rows. A wakeup still acts as the prompt. The spawning
    /// `Agent` call is a `SubagentSpawn`; a foreground agent's result is its return.
    #[test]
    fn harness_user_rows_are_typed_and_agent_calls_are_spawns() {
        let note = |id: &str, summary: &str| {
            format!(
                r#"{{"type":"user","uuid":"{id}","origin":{{"kind":"task-notification"}},"message":{{"role":"user","content":"<task-notification>\n<task-id>t</task-id>\n<status>completed</status>\n<summary>{summary}</summary>\n</task-notification>"}}}}"#
            )
        };
        let text = [
            r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{"prompt":"x"}}]}}"#.to_string(),
            r#"{"type":"user","uuid":"r","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]},"toolUseResult":{"agentId":"a1","status":"completed"}}"#.to_string(),
            note("n1", "Agent 'x' finished"),
            note("n2", "Dynamic workflow 'w' completed"),
            note("n3", "Background command 'sleep' completed (exit code 0)"),
            r#"{"type":"user","uuid":"i","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#.to_string(),
            r#"{"type":"user","uuid":"m","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat</local-command-caveat>"}}"#.to_string(),
            r#"{"type":"user","uuid":"p","message":{"role":"user","content":"a real prompt"}}"#.to_string(),
        ]
        .join("\n");
        let s = parse_str("kinds", &text, None);
        let got: Vec<(MessageKind, Origin)> = s.messages.iter().map(|m| (m.kind, m.origin)).collect();
        assert_eq!(
            got,
            [
                (MessageKind::SubagentSpawn, Origin::Model),
                (MessageKind::SubagentReturn, Origin::Subagent),
                (MessageKind::SubagentReturn, Origin::Subagent),
                (MessageKind::SubagentReturn, Origin::Subagent),
                (MessageKind::InjectedContext, Origin::Harness),
                (MessageKind::InjectedContext, Origin::Harness),
                (MessageKind::InjectedContext, Origin::Harness),
                (MessageKind::Prompt, Origin::Human),
            ]
        );
        assert!(s.messages[0].is_turn() && s.messages[0].is_model_visible());
    }

    #[test]
    fn subagent_end_reads_the_stop_hook_and_task_notices_keep_the_last_status() {
        let dir = std::env::temp_dir().join(format!("cv-subend-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent = dir.join("agent-a1.jsonl");
        std::fs::write(
            &agent,
            [
                r#"{"type":"user","uuid":"u","timestamp":"2026-10-01T00:00:00Z","message":{"role":"user","content":"go"}}"#,
                r#"{"type":"assistant","uuid":"a","timestamp":"2026-10-01T00:01:00Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"Waiting on notifications."}]}}"#,
                r#"{"type":"attachment","uuid":"h","timestamp":"2026-10-01T00:01:05Z","attachment":{"type":"hook_success","hookEvent":"SubagentStop"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let end = subagent_end(&agent);
        assert_eq!(end.stopped(), Some(true), "{end:?}");
        // A resume appends turns after the stop: no longer stopped.
        let mut resumed = std::fs::read_to_string(&agent).unwrap();
        resumed.push('\n');
        resumed.push_str(r#"{"type":"user","uuid":"u2","timestamp":"2026-10-01T00:05:00Z","message":{"role":"user","content":"poll it in the foreground"}}"#);
        std::fs::write(&agent, resumed).unwrap();
        assert_eq!(subagent_end(&agent).stopped(), Some(false));
        // No stop record at all: unknown (a killed harness looks like this too).
        let fresh = dir.join("agent-a2.jsonl");
        std::fs::write(
            &fresh,
            r#"{"type":"user","uuid":"u","timestamp":"2026-10-01T00:00:00Z","message":{"role":"user","content":"go"}}"#,
        )
        .unwrap();
        assert_eq!(subagent_end(&fresh).stopped(), None);

        let parent = dir.join("parent.jsonl");
        std::fs::write(
            &parent,
            [
                r#"{"type":"user","uuid":"n1","timestamp":"2026-10-01T00:02:00Z","message":{"role":"user","content":"<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n<summary>Agent finished</summary>\n</task-notification>"}}"#,
                r#"{"type":"user","uuid":"n2","timestamp":"2026-10-01T00:06:00Z","message":{"role":"user","content":"<task-notification>\n<task-id>a1</task-id>\n<status>killed</status>\n</task-notification>"}}"#,
                r#"{"type":"user","uuid":"n3","timestamp":"2026-10-01T00:06:00Z","message":{"role":"user","content":"<task-notification>\n<task-id>bash-7</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
                r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-01T00:08:00Z","content":"<task-notification>\n<task-id>a2</task-id>\n<status>completed</status>\n</task-notification>"}"#,
                r#"{"type":"user","uuid":"p","timestamp":"2026-10-01T00:07:00Z","message":{"role":"user","content":"a human mentioning <task-notification> in passing"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let notices = task_notices(&parent);
        assert_eq!(notices.len(), 3, "{notices:?}");
        assert_eq!(notices["a1"].status, "killed");
        assert_eq!(notices["a2"].status, "completed", "queue-operation records count");
        assert_eq!(
            notices["a1"].ts.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-10-01T00:06:00+00:00")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subagent_tree_discovers_both_tiers_with_meta_and_journal() {
        use std::io::Write;
        let _world = crate::test_world("claude-subagent-ref-projection");
        // Lay out a parent session with one directly-spawned sub-agent and one workflow holding
        // two agents (one journaled done, one a string return), mirroring the real on-disk shape.
        let root = std::env::temp_dir().join(format!("cv-subtree-{}", uuid::Uuid::new_v4()));
        let proj = root.join("projects").join("-enc");
        std::fs::create_dir_all(&proj).unwrap();
        let parent = proj.join("sess.jsonl");
        std::fs::write(
            &parent,
            "{\"type\":\"user\",\"uuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n",
        )
        .unwrap();

        let subs = proj.join("sess").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        // Tier 1: a direct Agent sub-agent + its meta sidecar.
        let a1 = subs.join("agent-a111.jsonl");
        std::fs::write(&a1, "{\"type\":\"user\",\"uuid\":\"x\",\"message\":{\"role\":\"user\",\"content\":\"task one\"}}\n{\"type\":\"assistant\",\"uuid\":\"y\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done one\"}]}}\n").unwrap();
        std::fs::write(
            subs.join("agent-a111.meta.json"),
            r#"{"agentType":"general-purpose","description":"do task one","toolUseId":"toolu_77"}"#,
        )
        .unwrap();

        // Tier 2: a workflow dir with two agents and a journal.
        let wf = subs.join("workflows").join("wf_abc-123");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(
            wf.join("agent-a222.jsonl"),
            "{\"type\":\"user\",\"uuid\":\"p\",\"message\":{\"role\":\"user\",\"content\":\"wtask\"}}\n",
        )
        .unwrap();
        std::fs::write(
            wf.join("agent-a222.meta.json"),
            r#"{"agentType":"workflow-subagent","description":"wf task"}"#,
        )
        .unwrap();
        std::fs::write(
            wf.join("agent-a333.jsonl"),
            "{\"type\":\"user\",\"uuid\":\"q\",\"message\":{\"role\":\"user\",\"content\":\"wtask2\"}}\n",
        )
        .unwrap();
        std::fs::write(wf.join("agent-a333.meta.json"), r#"{"agentType":"workflow-subagent"}"#).unwrap();
        let mut jf = std::fs::File::create(wf.join("journal.jsonl")).unwrap();
        writeln!(
            jf,
            r#"{{"type":"result","agentId":"a222","result":{{"status":"GREEN","summary":"wf agent did it"}}}}"#
        )
        .unwrap();
        writeln!(jf, r#"{{"type":"result","agentId":"a333","result":"string return"}}"#).unwrap();
        drop(jf);

        FOREST_READS.lock().unwrap().0 = Some(root.clone());
        let tree = subagent_tree(&parent);
        assert_eq!(tree.len(), 3, "two tiers: 1 direct + 2 workflow");

        let before = FOREST_READS.lock().unwrap().1.len();
        let candidates = std::sync::atomic::AtomicUsize::new(0);
        let tools = subagent_tree_filtered(&parent, crate::SubagentDemand::Tools, Some("wf_abc-123"), &|id, run| {
            candidates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            id == "agent-a222" && run == Some("wf_abc-123")
        });
        assert_eq!(
            candidates.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the direct-agent directory is outside the selected workflow"
        );
        assert_eq!(tools.len(), 1);
        let original = tree.iter().find(|s| s.agent_id() == "a222").unwrap();
        assert_eq!(
            serde_json::to_value(&tools[0].session).unwrap(),
            serde_json::to_value(&original.session).unwrap()
        );
        assert_eq!(tools[0].agent_type, original.agent_type);
        assert_eq!(tools[0].workflow, original.workflow);
        assert!(tools[0].description.is_none() && tools[0].result_summary.is_none());
        let reads: Vec<_> = FOREST_READS.lock().unwrap().1[before..]
            .iter()
            .filter(|(_, path)| path.starts_with(&root))
            .cloned()
            .collect();
        assert_eq!(
            reads.len(),
            2,
            "one selected transcript and its agent-type metadata only: {reads:?}"
        );
        assert!(reads
            .iter()
            .all(|(kind, path)| *kind != "journal"
                && path.file_name().unwrap().to_string_lossy().starts_with("agent-a222.")));
        let before = FOREST_READS.lock().unwrap().1.len();
        let sessions = subagent_tree_filtered(&parent, crate::SubagentDemand::Sessions, None, &|id, _| {
            id == "agent-a111"
        });
        assert_eq!(sessions.len(), 1);
        let reads: Vec<_> = FOREST_READS.lock().unwrap().1[before..]
            .iter()
            .filter(|(_, path)| path.starts_with(&root))
            .cloned()
            .collect();
        assert_eq!(reads, [("transcript", a1.clone())]);
        let selected_rich =
            subagent_tree_filtered(&parent, crate::SubagentDemand::Rich, None, &|id, _| id == "agent-a222");
        assert_eq!(
            serde_json::to_value(&selected_rich[0]).unwrap(),
            serde_json::to_value(original).unwrap()
        );

        *FOREST_READS.lock().unwrap() = (None, Vec::new());
        let refs: Vec<_> = tree.iter().map(|s| &s.session).collect();
        for _ in 0..2 {
            assert_eq!(
                serde_json::to_vec(&subagent_session_refs(&parent)).unwrap(),
                serde_json::to_vec(&refs).unwrap(),
                "cold and cached refs preserve both tiers and ordering"
            );
        }

        let mut sink = CollectSink::default();
        let usage = stream_file(
            "a111",
            &subs.join("agent-a111.jsonl"),
            &ParseOptions::usage(),
            &mut sink,
        );
        assert_eq!(usage.lineage.parent.as_deref(), Some("sess"));
        assert!(usage.lineage.spawned_by_tool_use.is_none());
        assert!(!usage.extra.contains_key("claude"));
        let path = Some(subs.join("agent-a111.jsonl"));
        let full = new_session_for("a111", path.clone(), &ParseOptions::full());
        let both = new_session_for(
            "a111",
            path,
            &ParseOptions {
                usage_identity: true,
                ..ParseOptions::full()
            },
        );
        assert_eq!(serde_json::to_vec(&full).unwrap(), serde_json::to_vec(&both).unwrap());
        assert_eq!(full.lineage.spawned_by_tool_use.as_deref(), Some("toolu_77"));

        let direct = tree.iter().find(|s| s.agent_id() == "a111").expect("direct agent");
        assert_eq!(direct.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(direct.description.as_deref(), Some("do task one"));
        assert_eq!(direct.tool_use_id.as_deref(), Some("toolu_77"));
        assert!(direct.workflow.is_none());
        assert_eq!(direct.result_status, None); // direct agents report via parent tool_result
                                                // its return value = the last assistant text turn.
        assert_eq!(subagent_return(&direct.session.path).as_deref(), Some("done one"));

        let w1 = tree.iter().find(|s| s.agent_id() == "a222").expect("wf agent 1");
        assert_eq!(w1.workflow.as_deref(), Some("wf_abc-123"));
        assert_eq!(w1.result_status.as_deref(), Some("GREEN"));
        assert_eq!(w1.result_summary.as_deref(), Some("wf agent did it"));

        let w2 = tree.iter().find(|s| s.agent_id() == "a333").expect("wf agent 2");
        assert_eq!(w2.result_status, None);
        assert_eq!(w2.result_summary.as_deref(), Some("string return"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn common_defaults_stay_out_of_extra() {
        // A vanilla user line should not bloat `extra` with userType=external / isSidechain=false.
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"userType":"external","isSidechain":false,"sessionId":"s","timestamp":"2026-05-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#;
        let s = parse_str("s", line, None);
        assert!(s.messages[0].extra.is_empty());
    }

    #[test]
    fn tolerates_corrupt_and_unknown_lines() {
        let text = "not json\n{\"type\":\"x-quota\"}\n{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        let s = parse_str("s", text, None);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn native_json_inputs_preserve_values_modes_and_exact_ranges() {
        let world = crate::test_world("claude-json-inputs");
        let path = world.proj.join("source.jsonl");
        let big = serde_json::to_string(&"a\nβ".repeat(INLINE_MAX)).unwrap();
        let inputs = vec![
            format!("{{\"large\":{big},\"duplicate\":1,\"duplicate\":2}}"),
            format!("[{big},null,true,3]"),
            big,
            r#"{"small":"a\nb"}"#.to_owned(),
            "[null,true,3]".to_owned(),
            r#""encoded JSON stays a string""#.to_owned(),
            "false".to_owned(),
            "1.5".to_owned(),
            "null".to_owned(),
            format!("\"{}\"", "x".repeat(INLINE_MAX - 2)),
            format!("\"{}\"", "x".repeat(INLINE_MAX - 1)),
        ];
        let mut text = String::new();
        for (i, input) in inputs.iter().enumerate() {
            text.push_str(&format!(" \t{{\"type\":\"assistant\",\"uuid\":\"a{i}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t{i}\",\"name\":\"custom_tool\",\"input\": {input} }}]}}}}\r\n"));
        }
        text.push_str("{\"type\":\"assistant\",\"uuid\":\"missing\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"missing\",\"name\":\"custom_tool\"}]}}\n");
        fs::write(&path, &text).unwrap();
        let reference = SessionRef::new("source", Harness::Claude, path.clone());
        let adapter = Claude::with_config_dir(&world.proj);
        for opts in [
            ParseOptions::lazy(),
            ParseOptions::lazy_extra(),
            ParseOptions::bulk(),
            ParseOptions::full(),
            ParseOptions::complete(),
        ] {
            let mut sink = CollectSink::default();
            let mut actual = adapter.stream(&reference, &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(actual.messages.len(), inputs.len() + 1, "{opts:?}");
            for (i, message) in actual.messages.iter().enumerate() {
                let Block::ToolUse { input, .. } = &message.content[0] else {
                    panic!("expected tool")
                };
                match input {
                    Json::Source(range) => {
                        assert!(opts.spans && !opts.complete && i < inputs.len() && inputs[i].len() > INLINE_MAX);
                        let start = range.offset() as usize;
                        assert_eq!(&text[start..start + range.len() as usize], inputs[i]);
                        assert_eq!(range.path(), path);
                    }
                    Json::Owned(_) => {
                        assert!(!opts.spans || opts.complete || i == inputs.len() || inputs[i].len() <= INLINE_MAX);
                    }
                }
                let expected = inputs
                    .get(i)
                    .map_or(Value::Null, |input| serde_json::from_str(input).unwrap());
                assert_eq!(*input.resolve().unwrap(), expected, "{opts:?} input {i}");
            }
        }
        // Strings have no source file: their inputs are always owned.
        let mut sink = CollectSink::default();
        stream_str("source", &text, Some(path.clone()), &ParseOptions::lazy(), &mut sink);
        assert!(sink.messages.iter().flat_map(|m| &m.content).all(|b| !matches!(
            b,
            Block::ToolUse {
                input: Json::Source(_),
                ..
            }
        )));
    }

    #[test]
    fn native_json_inputs_keep_owned_fallback_last_wins_and_invalid_tail() {
        let world = crate::test_world("claude-json-input-fallback");
        let path = world.proj.join("source.jsonl");
        let text = concat!(
            r#"{"type":"assistant","uuid":"duplicate","message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"custom_tool","input":{"old":1},"input":{"winner":2}}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"bad","message":{"role":"assistant","content":[{"type":"tool_use","id":"bad","name":"custom_tool","input":{"bad":"\ud800"}}]}}"#,
            "\n",
        );
        fs::write(&path, text).unwrap();
        let adapter = Claude::with_config_dir(&world.proj);
        let reference = SessionRef::new("source", Harness::Claude, path.clone());
        for opts in [ParseOptions::lazy(), ParseOptions::lazy_extra()] {
            let mut sink = CollectSink::default();
            let mut actual = adapter.stream(&reference, &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(actual.messages.len(), 1, "the record with a lone surrogate is skipped");
            let Block::ToolUse {
                input: Json::Owned(value),
                ..
            } = &actual.messages[0].content[0]
            else {
                panic!("a small input is owned")
            };
            assert_eq!(value, &serde_json::json!({"winner": 2}));
        }
    }

    #[test]
    fn native_json_inputs_reject_changed_source_and_resume_exact_ranges() {
        let world = crate::test_world("claude-json-input-errors");
        let path = world.proj.join("source.jsonl");
        let padding = "x".repeat(INLINE_MAX);
        let line = |id: &str| {
            format!("{{\"type\":\"assistant\",\"uuid\":\"{id}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"custom_tool\",\"input\":{{\"id\":\"{id}\",\"padding\":\"{padding}\"}}}}]}}}}\n")
        };
        let first = line("first");
        let text = format!("{first}{}", line("second"));
        fs::write(&path, &text).unwrap();
        let reference = SessionRef::new("source", Harness::Claude, path.clone());
        let adapter = Claude::with_config_dir(&world.proj);
        let mut cp = Checkpoint::new(first.len() as u64);
        cp.put(ADAPTER_LAYER, &new_session("source", Some(path.clone())));
        let mut sink = CollectSink::default();
        adapter
            .resume(&reference, &ParseOptions::lazy(), &mut sink, &cp)
            .unwrap()
            .unwrap();
        assert_eq!(sink.messages.len(), 1);
        let Block::ToolUse {
            input: Json::Source(range),
            ..
        } = &sink.messages[0].content[0]
        else {
            panic!("expected sourced input")
        };
        assert!(range.offset() >= first.len() as u64);
        assert_eq!(Json::Source(range.clone()).resolve().unwrap()["id"], "second");
        // Deleting the path keeps an existing Unix mmap readable but invalidates its captured
        // identity. The operation must stop before another callback and return the source error.
        struct Deleting {
            path: PathBuf,
            calls: usize,
            checkpoints: usize,
            captured: Option<Json>,
        }
        impl MessageSink for Deleting {
            fn message(&mut self, message: Message) -> Flow {
                self.calls += 1;
                if let Block::ToolUse { input, .. } = &message.content[0] {
                    self.captured = Some(input.clone());
                }
                fs::remove_file(&self.path).unwrap();
                Flow::Continue
            }
            fn wants_checkpoint(&mut self, _offset: u64) -> bool {
                true
            }
            fn checkpoint(&mut self, _checkpoint: &mut Checkpoint) {
                self.checkpoints += 1;
            }
        }
        let mut deleting = Deleting {
            path: path.clone(),
            calls: 0,
            checkpoints: 0,
            captured: None,
        };
        assert!(adapter
            .stream(&reference, &ParseOptions::lazy(), &mut deleting)
            .is_err());
        assert_eq!(deleting.calls, 1);
        assert_eq!(deleting.checkpoints, 0);
        assert!(deleting.captured.unwrap().resolve().is_err());
    }

    #[test]
    fn native_json_inputs_check_completion_only_after_source_publication() {
        let world = crate::test_world("claude-json-input-completion");
        let path = world.proj.join("source.jsonl");
        let record = |input: Value| {
            serde_json::json!({
                "type": "assistant", "uuid": "a", "message": {"role": "assistant",
                "content": [{"type": "tool_use", "id": "t", "name": "custom_tool", "input": input}]}
            })
            .to_string()
                + "\n"
        };
        let tiny = record(serde_json::json!({"path": "x"}));
        let reference = SessionRef::new("source", Harness::Claude, path.clone());
        let adapter = Claude::with_config_dir(&world.proj);
        fs::write(&path, &tiny).unwrap();
        let mut owned = None;
        let mut remove_tiny = |message: Message| {
            owned = Some(message);
            fs::remove_file(&path).unwrap();
            Flow::Stop
        };
        adapter
            .stream(&reference, &ParseOptions::lazy(), &mut remove_tiny)
            .unwrap();
        assert!(matches!(
            &owned.unwrap().content[0],
            Block::ToolUse {
                input: Json::Owned(_),
                ..
            }
        ));
        // A later callback that emits only owned inputs need not stat the source on that record,
        // but completion must reject invalidation of an earlier published Source descriptor.
        let large = record(serde_json::json!({"body": "x".repeat(INLINE_MAX)}));
        fs::write(&path, large + &tiny).unwrap();
        let mut calls = 0;
        let mut remove_later = |_: Message| {
            calls += 1;
            if calls == 2 {
                fs::remove_file(&path).unwrap();
            }
            Flow::Continue
        };
        assert!(adapter
            .stream(&reference, &ParseOptions::lazy(), &mut remove_later)
            .is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    fn lazy_spans_resolve_to_same_text() {
        // A record with content well over INLINE_MAX: the lazy path must produce a Span whose
        // resolved text equals the inline parse, byte-for-byte (incl. escapes).
        use std::io::Write;
        let big = "x\n\"quoted\"\t".repeat(1000); // ~10 KB with escapes (\n, \", \t)
        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "u1",
            "message": { "role": "assistant", "content": [{"type": "text", "text": big}] }
        })
        .to_string();
        let dir = std::env::temp_dir().join(format!("cv-lazy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        drop(f);

        let r = SessionRef {
            message_count: 1,
            ..SessionRef::new("s", Harness::Claude, path.clone())
        };
        // Lazy parse → expect a Span; resolving it must equal `big`.
        let mut sink = CollectSink::default();
        let mut s = stream_file(&r.id, &path, &ParseOptions::lazy(), &mut sink);
        s.messages = sink.messages;
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(text.is_span(), "large content should be a span");
                let resolver = s.resolver();
                assert_eq!(text.try_resolve(&resolver).unwrap(), big.as_str());
            }
            _ => panic!("expected text block"),
        }
        // And materialize() makes it inline and equal.
        s.materialize().unwrap();
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(!text.is_span());
                assert_eq!(text.inline_str(), Some(big.as_str()));
            }
            _ => panic!("expected text block"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stream a transcript file through the one reader.
    fn stream_file(id: &str, path: &Path, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Session {
        read(
            new_session_for(id, Some(path.to_path_buf()), opts),
            path,
            0,
            None,
            opts,
            sink,
            &mut || Ok(()),
        )
        .unwrap()
    }

    #[test]
    fn checkpoints_mark_finished_lines_and_resume_continues_from_them() {
        #[derive(Default)]
        struct Capture {
            messages: Vec<Message>,
            checkpoints: Vec<(Checkpoint, usize)>,
            stop_after: Option<usize>,
        }
        impl MessageSink for Capture {
            fn message(&mut self, message: Message) -> Flow {
                self.messages.push(message);
                if self.stop_after == Some(self.messages.len()) {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                true
            }
            fn checkpoint(&mut self, cp: &mut Checkpoint) {
                self.checkpoints.push((cp.clone(), self.messages.len()));
            }
        }
        fn resolved(mut messages: Vec<Message>, path: &Path) -> Value {
            let resolver = crate::Resolver::new(Some(path.to_path_buf()));
            for m in &mut messages {
                m.materialize(&resolver).unwrap();
            }
            serde_json::to_value(messages).unwrap()
        }
        let dir = cv_testkit::temp_dir("claude-checkpoint-lines");
        let path = dir.join("s.jsonl");
        let body = "wide \"payload\"\n".repeat(1000);
        let first = serde_json::json!({"type":"user","uuid":"u","cwd":"/project","permissionMode":"plan","timestamp":"2026-01-02T00:00:00Z","message":{"content":"question"}});
        let second = serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","timestamp":"2026-01-01T00:00:00Z","message":{"model":"test-model","content":[{"type":"text","text":body},{"type":"tool_use","id":"call","name":"Agent","input":{"prompt":"test"}}]}});
        let tail = serde_json::json!({"type":"user","uuid":"t","parentUuid":"a","toolUseResult":{"agentId":"agent"},"message":{"content":[{"type":"tool_result","tool_use_id":"call","content":"done"}]}});
        let prefix = format!("  {first}\r\n\n{second}\n");
        let ends: Vec<u64> = prefix.match_indices('\n').map(|(i, _)| i as u64 + 1).collect();
        for (tail, tail_messages) in [
            (tail.to_string(), 1),
            (r#"{"type":"assistant","message": {"content":"partial"#.to_owned(), 0),
        ] {
            let text = format!("{prefix}{tail}");
            fs::write(&path, &text).unwrap();
            let r = scan(&path).unwrap();
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::lazy_offsets(),
                ParseOptions::lazy_extra(),
            ] {
                let mut whole = Capture::default();
                let session = Claude::with_config_dir(&dir).stream(&r, &opts, &mut whole).unwrap();
                assert_eq!(whole.messages.len(), 2 + tail_messages);
                // Every finished line, and never the unterminated tail.
                assert_eq!(
                    whole.checkpoints.iter().map(|(cp, _)| cp.offset).collect::<Vec<_>>(),
                    ends
                );
                assert_eq!(session.cwd.as_deref(), Some(Path::new("/project")));
                for (cp, seen) in &whole.checkpoints {
                    let mut rest = Capture::default();
                    let resumed = Claude::with_config_dir(&dir)
                        .resume(&r, &opts, &mut rest, cp)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        resolved(rest.messages, &path),
                        resolved(whole.messages[*seen..].to_vec(), &path)
                    );
                    assert_eq!(
                        serde_json::to_value(&resumed).unwrap(),
                        serde_json::to_value(&session).unwrap()
                    );
                }
                let mut stopped = Capture {
                    stop_after: Some(1),
                    ..Capture::default()
                };
                Claude::with_config_dir(&dir).stream(&r, &opts, &mut stopped).unwrap();
                assert_eq!(stopped.messages.len(), 1);
                assert_eq!(
                    stopped.checkpoints.iter().map(|(cp, _)| cp.offset).collect::<Vec<_>>(),
                    ends[..0]
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// The stamp `cv rewind` cuts at: under `lazy_offsets()` every message is stamped with the
    /// byte offset of the record line that produced it, in file order.
    #[test]
    fn offset_stamps_mark_each_message_record_start() {
        use std::io::Write;
        let big = "wide \"payload\"\n".repeat(600); // > INLINE_MAX, with escapes → a Span
        let lines = [
            serde_json::json!({"type":"ai-title","aiTitle":"stamp test"}),
            serde_json::json!({"type":"user","uuid":"u0","cwd":"/w","gitBranch":"main",
                "message":{"role":"user","content":"first question"}}),
            serde_json::json!({"type":"assistant","uuid":"a1","timestamp":"2026-06-01T00:00:01Z",
                "message":{"role":"assistant","model":"claude-test-1","content":[
                    {"type":"text","text":"answer one"},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            serde_json::json!({"type":"user","uuid":"u2","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":big,"is_error":false}]}}),
            serde_json::json!({"type":"system","subtype":"away_summary","uuid":"s3",
                "content":"came back"}),
            // A model-visible attachment (hook output) is a message too — and must be stamped.
            serde_json::json!({"type":"attachment","uuid":"h3","attachment":{"type":"hook_success"},
                "rendered":[{"content":"hook said hi"}]}),
            serde_json::json!({"type":"user","uuid":"u4","message":{"role":"user","content":"second question"}}),
            serde_json::json!({"type":"assistant","uuid":"a5","message":{"role":"assistant",
                "model":"claude-test-1","content":[{"type":"text","text":"answer two"}]}}),
        ];
        let dir = std::env::temp_dir().join(format!("cv-claude-stamp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);

        let opts = ParseOptions::lazy_offsets();
        let mut full = CollectSink::default();
        stream_file("s", &path, &opts, &mut full);
        let full = full.messages;
        assert_eq!(full.len(), 7); // user, assistant, tool, system, attachment, user, assistant
        let offs: Vec<u64> = full
            .iter()
            .map(|m| {
                m.extra
                    .get(crate::offsets::OFFSET_KEY)
                    .and_then(Value::as_u64)
                    .expect("every message stamped under lazy_offsets")
            })
            .collect();
        assert!(offs.windows(2).all(|w| w[0] < w[1]), "offsets follow file order");

        let bytes = fs::read(&path).unwrap();
        let line_starts: Vec<u64> = std::iter::once(0)
            .chain(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, &b)| b == b'\n')
                    .map(|(i, _)| i as u64 + 1),
            )
            .collect();
        // Line 0 is the title record (no message); messages k sit on lines k+1.
        assert_eq!(
            offs,
            line_starts[1..=full.len()],
            "each stamp is its record's line start"
        );

        // And bulk/full options never stamp (their output stays byte-identical to before).
        let mut bulk = CollectSink::default();
        stream_file("s", &path, &ParseOptions::bulk(), &mut bulk);
        assert!(bulk
            .messages
            .iter()
            .all(|m| !m.extra.contains_key(crate::offsets::OFFSET_KEY)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bulk_opts_skip_extra_but_keep_text() {
        // The bulk path (`ParseOptions::bulk()`) must NOT materialize the fat `toolUseResult`
        // sidecar — that's the whole point of gating `extra` for index/search/dataset — while the
        // full path (`parse`) keeps it. The searchable text must be identical either way.
        let text = fixture("rich_blocks.jsonl");

        let full = parse_str("s1", &text, None);
        let tool_full = full.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(
            details(tool_full).get("structuredPatch").is_some(),
            "full parse must keep the sidecar"
        );

        let mut sink = CollectSink::default();
        let mut bulk = stream_str("s1", &text, None, &ParseOptions::bulk(), &mut sink);
        bulk.messages = sink.messages;
        for m in &bulk.messages {
            assert!(m.extra.is_empty(), "bulk opts must leave every message's extra empty");
            assert!(
                m.content
                    .iter()
                    .all(|b| !matches!(b, Block::ToolResult { details: Some(_), .. })),
                "bulk opts must not materialize the fat sidecar onto the block either"
            );
        }
        assert_eq!(
            full.searchable_text().unwrap(),
            bulk.searchable_text().unwrap(),
            "gating extra must not change searchable text"
        );
    }

    #[test]
    fn stream_can_stop_early() {
        // A sink returning Stop ends the parse without visiting the rest of the transcript.
        let text = fixture("rich_blocks.jsonl");
        let mut seen = 0usize;
        let mut sink = |_m: Message| {
            seen += 1;
            Flow::Stop
        };
        let _ = stream_str("s1", &text, None, &ParseOptions::bulk(), &mut sink);
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
    }
}
