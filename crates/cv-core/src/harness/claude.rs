//! Claude Code adapter — `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`, plus the same
//! layout under any extra roots (`$CLAUDE_CONFIG_DIR`, `$CLUSTERVISION_CLAUDE_ROOTS`, the
//! `claude-roots` file; see [`Claude::detect`]).
//!
//! See `docs/FORMATS.md`. Key points: one session per `.jsonl` file; each line is a typed record;
//! `cwd` is read from inside the transcript (the dir-name encoding is lossy), and the conversation is
//! threaded via `uuid`/`parentUuid`.

use super::{parse_ts, Adapter};
use crate::json::{ReadInput, Validation};
use crate::source::{Dependencies, Probe};

/// `cv adopt`: move a sub-agent stranded by a dead session into a live one — Claude-only byte
/// surgery on this store's layout, so it lives beside the adapter that knows it.
pub mod adopt;
pub(crate) mod emit;
mod fork;
mod hook;
mod metadata;
mod notification;
pub(crate) mod record;
pub(crate) mod replay;
/// The sidecar tree beside a transcript (`subagents/`, `workflows/`, `journal.jsonl`): every path
/// spelled once, and the one journal parser.
pub mod sidecar;
pub mod skills;
use super::store::jsonl;
use super::wire::anthropic;
use crate::ir::*;
use crate::ir::{MessageKind, Origin};
use crate::json::JsonSource;
use crate::lanes::{ApiErrorNotice, SubagentEnd, TaskNotice};
use crate::lazy::Text;
use crate::stream::{Checkpoint, CollectSink, Demand, Flow, MessageSink, Start, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
pub(crate) use emit::verify_derived;
pub use emit::{emit, verify_claude_resumable};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Claude {
    /// Root entries in priority order, before `*` expansion and the existence check. They are
    /// resolved by [`Claude::roots`] on every call, so a root that appears later (a new agent seat
    /// under a `*` entry) is read without rebuilding the adapter.
    entries: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Claude = Claude { entries: Vec::new() };

/// The newest `compact_boundary` record in the last `tail` bytes of a transcript: its
/// `compactMetadata` `(trigger, preTokens)`. The tail is read through the one line reader from
/// `len - tail` (the cut line is rejected, never guessed at); `None` when the file cannot be
/// opened or the tail holds no boundary.
pub fn last_compact_boundary(path: &Path, tail: u64) -> Result<Option<(Option<String>, Option<u64>)>> {
    let Ok(read) = jsonl::Read::open(path) else {
        return Ok(None);
    };
    let len = read.source().len();
    let mut last = None;
    read.snapshot(len.saturating_sub(tail), len, |line| {
        let Some(head) = line
            .view
            .map(|view| record::Head::parse(view, record::Demand::Compact))
            .transpose()?
            .flatten()
        else {
            return Ok(Flow::Continue);
        };
        if head.top.string("subtype")?.as_deref() == Some("compact_boundary") {
            let meta = head
                .top
                .get("compactMetadata")
                .map(|m| super::store::document::Fields::parse(m, &["trigger", "preTokens"]))
                .transpose()?
                .flatten();
            let pre = meta
                .as_ref()
                .and_then(|m| m.get("preTokens"))
                .map(|v| v.as_number())
                .transpose()?
                .flatten()
                .and_then(|n| n.as_u64());
            last = Some((meta.as_ref().map(|m| m.string("trigger")).transpose()?.flatten(), pre));
        }
        Ok(Flow::Continue)
    })?;
    Ok(last)
}

/// What a derived Claude copy rewrites: `sessionId` (and the `session_id` some hook records
/// carry), `agentId`/`isSidechain` on a sub-agent line made standalone, `parentUuid` at a re-root.
pub(crate) const IDENTITY: super::Identity = super::Identity {
    session_keys: &["sessionId", "session_id"],
    agent_key: "agentId",
    sidechain_key: "isSidechain",
    parent_key: "parentUuid",
};

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

/// The roots `claude-roots` lists, read once per file state: `detect` runs once per session in
/// per-session loops (a source's primary path, a search's staleness), and read the file each time.
fn configured_roots(path: &Path) -> Option<std::sync::Arc<[PathBuf]>> {
    use crate::source::FileId;
    use std::sync::{Arc, Mutex, PoisonError};
    type Memo = std::collections::HashMap<PathBuf, (FileId, Arc<[PathBuf]>)>;
    static MEMO: Mutex<Option<Memo>> = Mutex::new(None);
    let before = FileId::at(path)?;
    if let Some((_, roots)) = MEMO
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|memo| memo.get(path))
        .filter(|(seen, _)| *seen == before)
    {
        return Some(Arc::clone(roots));
    }
    let text = fs::read_to_string(path).ok()?;
    let roots: Arc<[PathBuf]> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(PathBuf::from)
        .collect();
    // A write during the read stores nothing: the next call reads the file as it then stands.
    if FileId::at(path) == Some(before) {
        MEMO.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(Default::default)
            .insert(path.to_path_buf(), (before, Arc::clone(&roots)));
    }
    Some(roots)
}

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
        if let Some(listed) = cv_home.and_then(|d| configured_roots(&d.join("claude-roots"))) {
            entries.extend(listed.iter().cloned());
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

    /// A skill a slash command loaded and a `<task-notification>` a stopped task queued: both
    /// injected context Claude Code writes as user turns.
    fn record_affordance(
        &self,
        m: &Message,
        first: &crate::affordance::FirstText<'_>,
    ) -> Result<Option<crate::affordance::RecordHit>> {
        use crate::affordance::{Kind, RecordHit};
        if m.kind != MessageKind::InjectedContext {
            return Ok(None);
        }
        if m.origin == Origin::Harness
            && m.harness_extra(Harness::Claude)
                .is_none_or(|bag| !bag.contains_key("sourceToolUseID"))
            && first.starts_with("Base directory for this skill:")?
        {
            return Ok(Some(RecordHit {
                kind: Kind::Skill,
                action: "load",
                tool: "slash command",
            }));
        }
        Ok(first.starts_with("<task-notification>")?.then_some(RecordHit {
            kind: Kind::BackgroundJob,
            action: "complete",
            tool: "task-notification",
        }))
    }

    /// `<agent>.meta.json`'s `description`, as the reader copies it into the bag.
    fn agent_description<'s>(&self, s: &'s Session) -> Option<&'s str> {
        s.harness_extra(Harness::Claude)
            .and_then(|bag| bag.get("agent_description"))
            .and_then(Value::as_str)
    }

    fn subagent_end(&self, transcript: &std::path::Path) -> Result<Option<SubagentEnd>> {
        Ok(Some(subagent_end(transcript)?))
    }

    fn task_notices(&self, parent: &SessionRef) -> Result<std::collections::HashMap<String, TaskNotice>> {
        task_notices(&parent.path)
    }

    /// Claude Code writes one JSONL line per content block of a streamed response, each
    /// repeating the response's `usage`, and a resumed session copies its predecessor's history
    /// into the new file, so the same `message.id` recurs within and across transcripts.
    fn usage_key(&self, m: &Message) -> Option<String> {
        m.harness_extra(Harness::Claude)
            .and_then(|b| b.get("message.id"))
            .and_then(Value::as_str)
            .map(|id| format!("claude:{id}"))
    }

    fn capabilities(&self) -> super::Capabilities {
        use crate::affordance::Denial;
        super::Capabilities {
            denials: &[
                Denial {
                    prefix: "Permission to use ",
                    also: Some(" denied"),
                },
                Denial {
                    prefix: "The user doesn't want to proceed with this tool use",
                    also: None,
                },
            ],
            edit_guard: Some(crate::hooks::EditGuard::Ask),
            // `SubagentStart`/`SubagentStop` rows name the parent session.
            lane_stops: Some(crate::lanes::StopScope::Parent),
            deferred_wrapper: None,
            // Native spans retain the same full-extra metadata.
            rich_spans: true,
        }
    }

    fn hook_event(&self, payload: &serde_json::Value) -> Result<Option<crate::hooks::Event>> {
        hook::hook_event(payload)
    }

    fn agent_transcript(&self, parent: &SessionRef, agent_session: &str, workflow: Option<&str>) -> Option<PathBuf> {
        hook::agent_transcript(&parent.path, agent_session, workflow)
    }

    fn last_compact_boundary(&self, transcript: &Path, tail: u64) -> Result<Option<(Option<String>, Option<u64>)>> {
        last_compact_boundary(transcript, tail)
    }

    fn identity(&self) -> Option<super::Identity> {
        Some(IDENTITY)
    }

    /// A `.jsonl` transcript only ever grows by appended lines.
    fn append_only(&self, r: &SessionRef) -> bool {
        r.path.extension().is_some_and(|e| e == "jsonl")
    }

    /// The `subagents/` sidecar: directly spawned agents and, a tier deeper, workflow agents.
    fn forest(
        &self,
        r: &SessionRef,
        demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        subagent_tree_filtered(&r.path, demand, workflow, &select)
    }

    /// The `workflows/` state files and the agent transcripts their runs left.
    fn workflows(&self) -> Option<&'static dyn super::Workflows> {
        Some(&super::claude_workflow::RUNS)
    }

    /// Anthropic's models are named `claude-*`; Claude Code's `<synthetic>` records name no model
    /// and carry no foreign thinking.
    fn native_thinking(&self, model: &str) -> bool {
        model == "<synthetic>" || model.starts_with("claude-")
    }

    /// Claude Code records an attachment's type beside the injected turn (`hook_success`, …).
    fn attachment_kind<'m>(&self, m: &'m crate::ir::Message) -> Option<&'m str> {
        m.extra.get("claude")?.get("attachment_type")?.as_str()
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if r.path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("agent-"))
        {
            deps.files.push(r.path.with_extension("meta.json"));
        }
        deps.directories.push(r.path.with_extension("").join("tool-results"));
        // Every agent transcript lives under `<session>/subagents/`, a workflow agent a tier deeper
        // in `subagents/workflows/<run>/`; the nearest `workflows` ancestor is that tier, not the
        // session's own `workflows/` state dir.
        if let Some(session) = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "subagents"))
            .and_then(Path::parent)
        {
            // By identity, not entry by entry: an agent reads a few of its parent's spilled outputs
            // and those are in its read set; declaring the whole directory file by file made one
            // agent row carry thousands of entries it never read.
            deps.files.push(session.join("tool-results"));
        }
        if let Some(config) = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "projects"))
            .and_then(Path::parent)
        {
            deps.directories.push(config.join("tasks").join(&r.id));
        }
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.resolve_roots()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        // Session files sit at projects/<encoded>/<sid>.jsonl (depth 2) in every root. Subagent
        // transcripts live deeper (…/<sid>/subagents/…), which max_depth(2) naturally excludes.
        // Collect paths (cheap), then scan (read + parse) them in parallel. The same session id in
        // two roots (a transcript copied into another config dir to resume there) is listed twice,
        // once per file; `crate::find` resolves such an id to the most recently updated copy.
        // An unreadable directory, and a transcript whose scan fails, are skipped and reported.
        let mut walk_errors = Vec::new();
        let paths: Vec<_> = self
            .resolve_roots()
            .iter()
            .flat_map(|root| WalkDir::new(root).min_depth(2).max_depth(2))
            .filter_map(|e| e.map_err(|e| walk_errors.extend(crate::Skipped::walk(&e))).ok())
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
        let mut found: crate::Discovered =
            crate::par_discovery_filter_map(paths, |path| crate::Skipped::listed(&path, cached_metadata(&path)))
                .into_iter()
                .collect();
        found.skipped.extend(walk_errors);
        Ok(found)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    /// A checkpoint keeps the session accumulated so far (the spawn tracker holds nothing at a
    /// line boundary: Claude's spawns all return later, as their own records). A prefix read is
    /// the session as it was, without the other files a fork source is found in.
    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let session = match from {
                Start::Checkpoint(cp) => match jsonl::restore(cp) {
                    Some(session) => session,
                    None => return Ok(None),
                },
                _ => new_session_for(&r.id, Some(r.path.clone()), opts),
            };
            let read = jsonl::Read::open(&r.path)?;
            let codec = Transcript { session, opts };
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Claude, sink);
            let Some(s) = jsonl::drive(&read, from, codec, &mut spawns)? else {
                return Ok(None);
            };
            Ok(Some(match from {
                Start::Prefix(_) => s,
                _ => with_fork_source(s, &r.path, sink),
            }))
        })
    }

    /// The task list (`TaskCreate`/`TaskUpdate`) Claude Code keeps beside the transcripts:
    /// `<config>/tasks/<session id>/<n>.json`.
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        Ok(task_list(&r.path, &r.id)?.into_iter().collect())
    }

    fn copied_before(&self, r: &SessionRef, sink: &mut dyn MessageSink) -> Option<chrono::DateTime<chrono::Utc>> {
        fork::copied_before(&r.path, sink)
    }
}

/// A `--fork-session` copy names no source; the records it shares with one do ([`fork`]). Runs
/// after every stream, whole or resumed, since it reads other files.
fn with_fork_source(mut s: Session, path: &Path, sink: &mut dyn MessageSink) -> Session {
    if s.lineage.parent.is_none() && s.lineage.forked_from.is_none() {
        s.lineage.forked_from = fork::source_tracked(path, sink).map(|(id, _)| id);
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
    let mut session = stream_str(id, text, source_path, &Demand::RICH, &mut sink);
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
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut session = new_session_for(id, source_path, opts);
    let mut spawns = crate::affordance::AffordanceSink::new(Harness::Claude, sink);
    // Pure-string path (parse_str, tests): no file offsets, so all content stays inline.
    let mut skipped = super::Unparsed::default();
    let result = jsonl::slice(text.as_bytes(), |line| match line.view {
        Some(view) => record::record(&mut session, view, None, opts, &mut spawns),
        None => {
            skipped.0 += u64::from(!line.blank);
            Ok(Flow::Continue)
        }
    });
    skipped.note(&mut session);
    super::note_read_error(&mut session, result);
    if let Some(error) = spawns.take_error() {
        super::note_read_error(&mut session, Err(error));
    }
    session
}

/// The Claude record codec over one captured transcript: each record accumulates into the session
/// (a checkpoint's, or a fresh shell at 0). The shared driver owns source identity checks.
struct Transcript<'r> {
    session: Session,
    opts: &'r Demand,
}

impl jsonl::Codec for Transcript<'_> {
    fn record(&mut self, view: crate::json::JsonView<'_>, at: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        record::record(&mut self.session, view, Some(at), self.opts, sink)
    }

    fn demand(&self) -> Demand {
        *self.opts
    }

    fn session(&mut self) -> &mut Session {
        &mut self.session
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        &self.session
    }

    fn finish(self, _: bool, _: &mut dyn MessageSink) -> Result<Session> {
        Ok(self.session)
    }
}

/// An empty Claude [`Session`] shell, ready to accumulate records. A sub-agent transcript (by its
/// path) starts with its lineage already filled in — see [`apply_subagent_lineage`].
#[cfg(test)]
fn new_session(id: &str, source_path: Option<PathBuf>) -> Session {
    new_session_for(id, source_path, &Demand::RICH)
}

fn new_session_for(id: &str, source_path: Option<PathBuf>, opts: &Demand) -> Session {
    let mut s = Session {
        source_path,
        ..Session::new(id.to_string(), Harness::Claude)
    };
    if let Some(p) = s.source_path.clone() {
        apply_subagent_lineage(&mut s, &p, opts.tier != Tier::Usage);
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
    let (agent_type, description, tool_use_id) = read_meta(&path.with_extension("meta.json")).unwrap_or_else(|error| {
        crate::env::warn_once("agent metadata", format!("{}: {error:#}", path.display()));
        Default::default()
    });
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
        if summary.starts_with("Agent ") || notification::contains_ascii_insensitive(summary.as_bytes(), b"workflow") {
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

/// The full sub-agent forest a Claude session spawned: every directly-spawned (`Agent`/`Task`)
/// sub-agent **and** every `Workflow` sub-agent (which live one level deeper, under
/// `subagents/workflows/<wf>/`), each enriched with its `*.meta.json` sidecar and — for workflow
/// agents — the orchestrator's journaled result.
///
/// The workflow tier is, on a heavy session, the *majority* of the agents, and the meta/journal
/// sidecars carry each agent's purpose and outcome. Newest first.
pub fn subagent_tree(parent_path: &std::path::Path) -> Result<Vec<AgentRef>> {
    subagent_tree_inner(parent_path, crate::SubagentDemand::Rich, None, &|_, _| true)
}

pub(crate) fn subagent_tree_filtered(
    parent_path: &Path,
    demand: crate::SubagentDemand,
    workflow: Option<&str>,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Result<Vec<AgentRef>> {
    subagent_tree_inner(parent_path, demand, workflow, select)
}

fn subagent_tree_inner(
    parent_path: &Path,
    demand: crate::SubagentDemand,
    workflow: Option<&str>,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Result<Vec<AgentRef>> {
    let forest = subagent_forest(parent_path, demand, workflow, select);
    // The agents this read scanned are saved by the read itself, as discovery saves its own:
    // `show --tree`, lanes and `events --subagents` in a later process reuse them instead of
    // rescanning every transcript. A save that fails is said once and retried next time.
    crate::discover_cache::persist(false);
    forest
}

fn subagent_forest(
    parent_path: &Path,
    demand: crate::SubagentDemand,
    workflow: Option<&str>,
    select: &(impl Fn(&str, Option<&str>) -> bool + Sync),
) -> Result<Vec<AgentRef>> {
    let Some(base) = sidecar::subagents_dir(parent_path) else {
        return Ok(vec![]);
    };

    let mut out: Vec<AgentRef> = Vec::new();

    // Tier 1: directly-spawned sub-agents at `subagents/agent-*.jsonl`.
    if workflow.is_none() {
        out.extend(agents_in_dir(&base, None, demand, select)?);
    }

    // Tier 2: workflow sub-agents at `subagents/workflows/<wf>/agent-*.jsonl`, grouped by run id,
    // each annotated with the result the workflow `journal.jsonl` recorded for its agentId.
    if let Some(dir) = sidecar::workflow_runs_dir(parent_path) {
        let rd = super::side_entries(&dir)?;
        let wf_dirs: Vec<PathBuf> = rd
            .into_iter()
            .map(|e| e.path())
            .filter(|p| workflow.is_none_or(|run| p.file_name().and_then(|name| name.to_str()) == Some(run)))
            .filter(|p| p.is_dir())
            .collect();
        let groups = crate::par_flat_map(wf_dirs, |wf_dir| {
            let run_id = wf_dir.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
            vec![agents_in_dir(&wf_dir, Some(&run_id), demand, select)]
        });
        for group in groups {
            out.extend(group?);
        }
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.session.created_at));
    Ok(out)
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
) -> Result<Vec<AgentRef>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    {
        for e in super::side_entries(dir)? {
            let p = e.path();
            // Only `agent-*.jsonl`; skip `journal.jsonl`, the `.meta.json` sidecars and the
            // `.flat.jsonl` payload sidecar `cv distill`/`cv prune` write beside a transcript.
            let is_agent = p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-") && !n.ends_with(".flat.jsonl"));
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
        sidecar::journal_results_checked(dir)?
    } else {
        JournalResults::new()
    };
    crate::par_discovery_filter_map(paths, |p| {
        Some((|| -> Result<Option<AgentRef>> {
            #[cfg(test)]
            record_forest_read("transcript", &p);
            // Every demand reads the agent's metadata through the per-file scan cache: a Rich
            // forest (`show --tree`, `--subagents`, the sessions API, lanes) rescanned every
            // agent transcript on every call. An unopenable transcript fails the forest even on a
            // cache hit.
            fs::File::open(&p)?;
            let session = match cached_metadata(&p)? {
                Some(session) => session,
                None => return Ok(None),
            };
            let meta = match demand {
                crate::SubagentDemand::Rich => read_meta(&p.with_extension("meta.json"))?,
                crate::SubagentDemand::Tools => (read_agent_type(&p.with_extension("meta.json"))?, None, None),
                crate::SubagentDemand::Sessions => (None, None, None),
            };
            let agent_id = session.id.strip_prefix("agent-").unwrap_or(&session.id).to_string();
            let (status, summary) = wf
                .and_then(|_| journal.get(&agent_id))
                .map(sidecar::result_summary)
                .unwrap_or((None, None));
            Ok(Some(AgentRef {
                session,
                agent_type: meta.0,
                description: meta.1,
                tool_use_id: meta.2,
                workflow: wf.map(str::to_owned),
                result_status: status,
                result_summary: summary,
            }))
        })())
    })
    .into_iter()
    .collect::<Result<Vec<_>>>()
    .map(|rows| rows.into_iter().flatten().collect())
}

/// Tool analytics demand the agent type, not the task description or spawning-call metadata.
fn read_agent_type(path: &Path) -> Result<Option<String>> {
    #[cfg(test)]
    record_forest_read("metadata", path);
    let source = match JsonSource::capture(path) {
        Ok(source) => source,
        Err(error) if super::is_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    source.read(ReadInput::Document, Validation::Value, |view| {
        crate::affordance::input::field_string(view, "agentType")
    })
}

/// Missing metadata is optional; corrupt or unreadable metadata is an error.
fn read_meta(path: &Path) -> Result<(Option<String>, Option<String>, Option<String>)> {
    #[cfg(test)]
    record_forest_read("metadata", path);
    let v = sidecar::read(path)?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok((s("agentType"), s("description"), s("toolUseId")))
}

/// The sub-agent's final return value as text: the last assistant text turn in its transcript
/// (what a `general-purpose`/`Explore` agent hands back to its parent, surfaced in the parent's
/// `Agent` tool_result). Reads one record at a time and retains the last substantive assistant text.
/// `None` for an empty transcript or one with no substantive assistant text; failed reads return an error.
pub fn subagent_return(path: &std::path::Path) -> Result<Option<String>> {
    let read = jsonl::Read::open(path)?;
    let mut last = None;
    read.lines(0, |line| {
        let Some(head) = line
            .view
            .map(|view| record::Head::parse(view, record::Demand::Message))
            .transpose()?
            .flatten()
        else {
            return Ok(Flow::Continue);
        };
        if head.ty == "assistant" {
            let items = head.top.get("message").map(|m| m.get("content")).transpose()?.flatten();
            if let Some(items) = items.map(|v| v.array()).transpose()?.flatten() {
                let mut buf = String::new();
                for item in items {
                    let Some(fields) = super::store::document::Fields::parse(item?, &["type", "text"])? else {
                        continue;
                    };
                    if fields.string("type")?.as_deref() == Some("text") {
                        if let Some(text) = fields.string("text")? {
                            if !buf.is_empty() {
                                buf.push('\n');
                            }
                            buf.push_str(&text);
                        }
                    }
                }
                if !buf.trim().is_empty() {
                    last = Some(buf);
                }
            }
        }
        Ok(Flow::Continue)
    })?;
    Ok(last)
}

pub fn subagent_end(path: &std::path::Path) -> Result<SubagentEnd> {
    let mut end = SubagentEnd::default();
    let read = jsonl::Read::open(path)?;
    let result = read.lines(0, |line| {
        let Some(head) = line
            .view
            .map(|view| record::Head::parse(view, record::Demand::Message))
            .transpose()?
            .flatten()
        else {
            return Ok(Flow::Continue);
        };
        let ts = head.top.string("timestamp")?.as_deref().and_then(parse_ts);
        match head.ty.as_str() {
            kind @ ("user" | "assistant") => {
                if let Some(ts) = ts {
                    end.last_turn_at = Some(end.last_turn_at.map_or(ts, |t: DateTime<Utc>| t.max(ts)));
                }
                let message = head
                    .top
                    .get("message")
                    .map(|m| super::store::document::Fields::parse(m, &["content", "model"]))
                    .transpose()?
                    .flatten();
                if head
                    .top
                    .get("isApiErrorMessage")
                    .map(|v| v.as_bool())
                    .transpose()?
                    .flatten()
                    == Some(true)
                {
                    let mut text = String::new();
                    if let Some(content) = message.as_ref().and_then(|m| m.get("content")) {
                        if let Some(blocks) = content.array()? {
                            let mut first = true;
                            for block in blocks {
                                let Some(fields) = super::store::document::Fields::parse(block?, &["text"])? else {
                                    continue;
                                };
                                if let Some(part) = fields.string("text")? {
                                    if !first {
                                        text.push(' ');
                                    }
                                    first = false;
                                    text.push_str(&part);
                                }
                            }
                        } else if content.is_string()? {
                            text = content.resolve()?.as_str().unwrap_or_default().to_string();
                        }
                    }
                    let status = head
                        .top
                        .get("apiErrorStatus")
                        .map(|v| v.as_number())
                        .transpose()?
                        .flatten()
                        .and_then(|n| n.as_u64());
                    let resets_at = line
                        .view
                        .unwrap()
                        .get("quotaLimits")?
                        .map(|v| v.get("resetsAt"))
                        .transpose()?
                        .flatten()
                        .map(|v| v.as_number())
                        .transpose()?
                        .flatten()
                        .and_then(|n| n.as_i64())
                        .and_then(|s| DateTime::from_timestamp(s, 0));
                    end.api_error = Some(ApiErrorNotice {
                        ts,
                        text,
                        error: head.top.string("error")?,
                        status,
                        resets_at,
                    });
                } else if kind == "user"
                    || message
                        .as_ref()
                        .map(|m| m.string("model"))
                        .transpose()?
                        .flatten()
                        .as_deref()
                        != Some(SYNTHETIC_MODEL)
                {
                    end.api_error = None;
                }
            }
            "attachment" => {
                let hook = head
                    .top
                    .get("attachment")
                    .map(|v| v.get("hookEvent"))
                    .transpose()?
                    .flatten();
                if hook
                    .map(|v| crate::projection::string_prefix(v, 13))
                    .transpose()?
                    .flatten()
                    .as_deref()
                    == Some("SubagentStop")
                {
                    if let Some(ts) = ts {
                        end.stopped_at = Some(end.stopped_at.map_or(ts, |t: DateTime<Utc>| t.max(ts)));
                    }
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    });
    result?;
    Ok(end)
}

/// Scan a parent transcript for `<task-notification>` records and reduce them to the last notice
/// per `<task-id>` (the bare agent id for `Agent`-tool children; background shell tasks notify
/// under their own ids and ride along harmlessly). Two record shapes carry one: the
/// `queue-operation`/`enqueue` record Claude Code writes the moment a child stops (top-level
/// `content`), and the `user` turn it becomes when the queue drains into the conversation
/// (`message.content`). Most notifications exist only as the former — on one real session 1,229
/// against 105 — so reading only user turns misses the status of most lanes. Content is read only
/// for records that start with the tag.
pub fn task_notices(parent_path: &std::path::Path) -> Result<std::collections::HashMap<String, TaskNotice>> {
    let mut map = std::collections::HashMap::new();
    let read = jsonl::Read::open(parent_path)?;
    let result = read.lines(0, |line| {
        let Some(head) = line
            .view
            .map(|view| record::Head::parse(view, record::Demand::Message))
            .transpose()?
            .flatten()
        else {
            return Ok(Flow::Continue);
        };
        let content = match head.ty.as_str() {
            "user" => head.top.get("message").map(|m| m.get("content")).transpose()?.flatten(),
            "queue-operation" => head.top.get("content"),
            _ => None,
        };
        let Some(content) = content.filter(|v| v.is_string().unwrap_or(false)) else {
            return Ok(Flow::Continue);
        };
        let body = content.source_text()?.unwrap_or_default();
        let (prefix, _) = super::store::document::text_head(&body, &crate::lazy::Resolver::new(None), 32, true)?;
        if !prefix.starts_with("<task-notification>") {
            return Ok(Flow::Continue);
        }
        let content = content.resolve()?;
        let content = content.as_str().unwrap_or_default();
        if let (Some(id), Some(status)) = (tag_inner(content, "task-id"), tag_inner(content, "status")) {
            map.insert(
                id.trim().to_string(),
                TaskNotice {
                    status: status.trim().to_string(),
                    ts: head.top.string("timestamp")?.as_deref().and_then(parse_ts),
                },
            );
        }
        Ok(Flow::Continue)
    });
    result?;
    Ok(map)
}

/// Cheap metadata-only scan for `discover`.
/// The cached metadata scan of one transcript: `Ok(None)` only when the cache refuses the
/// result, `Err` with the reason a scan failed.
fn cached_metadata(path: &Path) -> Result<Option<SessionRef>> {
    crate::discover_cache::cached_scan_continued(path, metadata::LAYER, |from| {
        let (reference, checkpoint) = scan_at(path, from)?;
        let continuation = checkpoint.and_then(|checkpoint| {
            let head = crate::resume::MarkHead::of(path, &checkpoint, metadata::LAYER)?;
            Some(crate::discover_cache::ScanContinuation {
                head,
                checkpoint,
                dependencies: Default::default(),
            })
        });
        Ok((Some(reference), continuation))
    })
}

#[cfg(test)]
fn scan(path: &Path) -> Result<SessionRef> {
    Ok(scan_at(path, None)?.0)
}

fn scan_at(path: &Path, from: Option<&Checkpoint>) -> Result<(SessionRef, Option<Checkpoint>)> {
    let read = jsonl::Read::open(path)?;
    let source = read.source();
    let (mut scan, offset) = metadata::Scan::restore(path, from);
    anyhow::ensure!(offset <= source.len(), "discovery checkpoint outside source");
    #[cfg(test)]
    metadata::FRAMED.with(|frames| frames.borrow_mut().push((offset, source.len() - offset)));
    #[cfg(test)]
    record_forest_read("scan", path);
    scan.read(&read, offset)?;
    let checkpoint = scan.checkpoint();
    let (session, writer_version) = scan.finish();
    crate::discover_cache::read::Context::current().observe_writer(&session, writer_version.as_deref());
    Ok((session, checkpoint))
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
    "sourceToolUseID",
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
fn task_list(transcript: &Path, id: &str) -> Result<Option<crate::affordance::Affordance>> {
    let Some(config) = transcript
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "projects"))
        .and_then(Path::parent)
    else {
        return Ok(None);
    };
    let dir = config.join("tasks").join(id);
    let items: Vec<_> = crate::affordance::json_files(&dir)?
        .iter()
        .filter_map(|(_, v)| crate::affordance::item_of(v))
        .collect();
    Ok(crate::affordance::plan_state(
        crate::affordance::Kind::Plan,
        "tasks/",
        items,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `claude-roots` is read once per file state, however many sessions ask for an adapter.
    #[test]
    fn claude_roots_are_read_once_per_file_state() {
        let world = crate::test_world("claude-roots-memo");
        let path = world.base.join("claude-roots");
        std::fs::write(&path, "/one\n# note\n/two\n").unwrap();
        let first = configured_roots(&path).unwrap();
        assert_eq!(&*first, [PathBuf::from("/one"), PathBuf::from("/two")]);
        assert!(
            std::sync::Arc::ptr_eq(&first, &configured_roots(&path).unwrap()),
            "unchanged: not reread"
        );
        std::fs::write(&path, "/three\n").unwrap();
        assert_eq!(&*configured_roots(&path).unwrap(), [PathBuf::from("/three")]);
    }
    use crate::lazy::INLINE_MAX;
    use crate::Json;
    use std::sync::LazyLock;

    static NO_BAG: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

    /// A workflow agent at `<session>/subagents/workflows/<run>/agent-*.jsonl` reads its session's
    /// spilled outputs, so `<session>/tool-results` is its dependency, not `subagents/tool-results`.
    #[test]
    fn workflow_agent_depends_on_its_sessions_tool_results() {
        let session = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/corpus/claude/2.1.304/.claude/projects/-home-corpus-project/e55ea121-48f2-441c-a430-fd701a8d9604",
        );
        for agent in [
            session.join("subagents/workflows/wf_5039b6e7-1a7/agent-ac6a8f09d2adefeb4.jsonl"),
            session.join("subagents/agent-direct.jsonl"),
        ] {
            let r = SessionRef::new("agent", Harness::Claude, agent.clone());
            let mut deps = crate::source::Dependencies::default();
            ROOTLESS
                .dependencies(&r, &mut crate::source::Probe::default(), &mut deps)
                .unwrap();
            assert!(
                deps.files.contains(&session.join("tool-results")),
                "{agent:?}: {:?}",
                deps.files
            );
            assert!(
                !deps.files.iter().any(|f| f.ends_with("subagents/tool-results")),
                "{:?}",
                deps.files
            );
        }
    }

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
    fn leftovers_slash_skill_projection_keeps_tool_correlation() -> Result<()> {
        let body = "Base directory for this skill: /skills/test\n\nDo the task";
        let records = [
            serde_json::json!({"type":"user","uuid":"slash","isMeta":true,"message":{"role":"user","content":body}}),
            serde_json::json!({"type":"assistant","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"skill-call","name":"Skill","input":{"skill":"test"}}]}}),
            serde_json::json!({"type":"user","uuid":"tool-body","isMeta":true,"sourceToolUseID":"skill-call","message":{"role":"user","content":body}}),
        ];
        let wire = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        for opts in [Demand::RICH, Demand::FACTS.with_spans(), Demand::COMPLETE] {
            let mut sink = CollectSink::default();
            let mut session = stream_str("skills", &wire, None, &opts, &mut sink);
            session.messages = sink.messages;
            assert_eq!(
                session
                    .messages
                    .iter()
                    .filter(|m| m
                        .harness_extra(Harness::Claude)
                        .is_some_and(|bag| bag.get("sourceToolUseID") == Some(&serde_json::json!("skill-call"))))
                    .count(),
                1
            );
            let rows = crate::affordance::of_session(&session)?;
            assert_eq!(
                rows.iter().filter(|r| r.kind == crate::affordance::Kind::Skill).count(),
                2,
                "slash activation plus the Skill call, without counting its body twice"
            );
        }
        Ok(())
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
        stream_str(sid, &text, None, &Demand::RICH, &mut sink);
        let convo: Vec<_> = sink
            .messages
            .iter()
            .filter(|m| m.extra.contains_key(CARRIER_KEY))
            .collect();
        assert_eq!(sink.messages.len(), 2, "full mode keeps only conversational turns");
        assert_eq!(convo.len(), 0, "no carriers in full mode");

        // Complete: every record present; the 3 meta records become carriers holding the original.
        let mut sink = CollectSink::default();
        let session = stream_str(sid, &text, None, &Demand::COMPLETE, &mut sink);
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
    fn request_ledger_and_unknown_records_are_bookkeeping() {
        // 2.1.300+: every API call leaves an `api-request` (params, blob hashes), its
        // `api-request-shape` (system prompt + tools) and `api-request-blob`s (one message each,
        // content-addressed). A blob has a `message`, but it is a request body, never a turn; the
        // same goes for a record type cv has never met.
        let text = [
            r#"{"type":"user","sessionId":"s1","uuid":"u0","timestamp":"2026-10-19T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"api-request-shape","sessionId":"s1","shapeHash":"h1","timestamp":"2026-10-19T00:00:00Z","version":"2.1.304","shape":{"system":[{"type":"text","text":"sys"}],"tools":[]}}"#,
            r#"{"type":"api-request-blob","sessionId":"s1","hash":"b1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"api-request","sessionId":"s1","id":"r1","timestamp":"2026-10-19T00:00:00Z","version":"2.1.304","querySource":"sdk","shapeHash":"h1","params":{"model":"m","max_tokens":1},"keep":0,"tail":["b1"],"cacheControl":[],"requestDigest":"d"}"#,
            r#"{"type":"isolation-latch","side":"web","sessionId":"s1"}"#,
            r#"{"type":"never-seen-before","sessionId":"s1","message":{"role":"user","content":"not a turn"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a1","parentUuid":"u0","timestamp":"2026-10-19T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ]
        .join("\n");
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &Demand::RICH, &mut sink);
        assert_eq!(sink.messages.len(), 2, "full mode: the two turns only");
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &Demand::COMPLETE, &mut sink);
        let carried: Vec<&str> = sink
            .messages
            .iter()
            .filter_map(|m| m.extra.get(CARRIER_KEY)?.get("type")?.as_str())
            .collect();
        assert_eq!(
            carried,
            [
                "api-request-shape",
                "api-request-blob",
                "api-request",
                "isolation-latch",
                "never-seen-before"
            ]
        );
    }

    #[test]
    fn fork_context_ref_is_fork_lineage() {
        // 2.1.302+: a fork sub-agent's transcript opens with where its context was cut from.
        let text = [
            r#"{"type":"fork-context-ref","agentId":"a1","parentSessionId":"parent","parentLastUuid":"u9","contextLength":10}"#,
            r#"{"type":"assistant","sessionId":"parent","uuid":"a1","parentUuid":null,"isSidechain":true,"agentId":"a1","timestamp":"2026-10-19T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"forked"}]}}"#,
        ]
        .join("\n");
        let s = parse_str("a1", &text, None);
        assert_eq!(s.lineage.forked_from.as_deref(), Some("parent"));
        assert_eq!(
            s.extra["claude"]["fork_context"],
            serde_json::json!({"parent_last_uuid": "u9", "context_length": 10})
        );
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn retried_request_api_error_is_an_error_notice() {
        // 2.1.302+: a retry is an `api_error` with no `content`; the error object and the retry
        // counters say what happened. Older records with `content` still read as before.
        let text = [
            r#"{"type":"system","subtype":"api_error","level":"error","error":{"message":"429 {\"type\":\"error\"}","status":429,"requestId":"req","formatted":"429 rate limit reached"},"retryInMs":547,"retryAttempt":1,"maxRetries":10,"source":"request_retry","uuid":"e1","timestamp":"2026-10-19T00:00:00Z","sessionId":"s1","version":"2.1.302"}"#,
            r#"{"type":"system","subtype":"api_error","level":"error","cause":{"code":"ConnectionRefused"},"retryAttempt":1,"uuid":"e2","timestamp":"2026-10-19T00:00:01Z","content":"Connection refused, retrying"}"#,
            r#"{"type":"system","subtype":"api_error","level":"error","error":"bare string","uuid":"e3","timestamp":"2026-10-19T00:00:02Z"}"#,
            r#"{"type":"system","subtype":"api_error","level":"error","uuid":"e4","timestamp":"2026-10-19T00:00:03Z"}"#,
        ]
        .join("\n");
        let s = parse_str("s1", &text, None);
        let texts: Vec<_> = s
            .messages
            .iter()
            .map(|m| (m.kind, m.text().unwrap_or_default()))
            .collect();
        assert_eq!(
            texts,
            [
                (MessageKind::Error, "429 rate limit reached (retry 1 of 10)".to_string()),
                (MessageKind::Error, "Connection refused, retrying".to_string()),
                (MessageKind::Error, "bare string".to_string()),
            ]
        );
        let bag = cl(&s.messages[0]);
        assert_eq!(bag["retryAttempt"], 1);
        assert_eq!(bag["source"], "request_retry");
        assert_eq!(bag["error"]["status"], 429);
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
        stream_str("s1", &text, None, &Demand::LEAN, &mut sink);
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
        stream_str("s1", &text, None, &Demand::RICH, &mut sink);
        assert_eq!(cl(&sink.messages[1])["attachment"]["filename"], "/x.md");
        assert_eq!(cl(&sink.messages[1])["renderedInHumanTurn"], true);

        // complete: both attachments carried verbatim; the rendered one still carries its text
        let mut sink = CollectSink::default();
        stream_str("s1", &text, None, &Demand::COMPLETE, &mut sink);
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
        let session = stream_str("s1", &text, None, &Demand::RICH, &mut sink);
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
        stream_str("s1", &text, None, &Demand::COMPLETE, &mut sink);
        assert_eq!(sink.messages[1].role, Role::Assistant);
        assert_eq!(
            sink.messages[1].kind,
            MessageKind::Error,
            "the kind is an IR fact even when the wire role is kept"
        );
        assert_eq!(sink.messages[1].model.as_deref(), Some("<synthetic>"));

        // Discovery counts the user turn and the real reply: the API error and the synthetic
        // notice are not turns.
        let dir = std::env::temp_dir().join(format!("cv-claude-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s1.jsonl");
        std::fs::write(&p, format!("{text}\n")).unwrap();
        assert_eq!(scan(&p).unwrap().message_count, Some(2));
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
        let session = stream_str("s1", &text, None, &Demand::RICH, &mut sink);
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
        stream_str("s1", &text, None, &Demand::COMPLETE, &mut sink);
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
        stream_str("s1", &line, None, &Demand::LEAN, &mut sink);
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
        stream_str("s1", &plain, None, &Demand::LEAN, &mut sink);
        let Block::ToolResult { details, .. } = &sink.messages[0].content[0] else {
            panic!()
        };
        assert!(details.is_none());
    }

    /// Every bag key `collect_extra` produces for the kitchen-sink meta record, derived from the
    /// capture lists themselves (plus the `sourceToolUseID` special case) so this test cannot
    /// drift from the codec's lists.
    fn expected_bag_keys() -> Vec<String> {
        let mut keys: Vec<String> = CLAUDE_EXTRA_TOP.iter().map(|k| k.to_string()).collect();
        keys.push("sourceToolUseID".into());
        keys.extend(CLAUDE_EXTRA_MSG.iter().map(|k| format!("{MESSAGE_EXTRA_PREFIX}{k}")));
        keys
    }

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
            "sourceToolAssistantUUID": "u-src", "sourceToolUseID": "toolu_self",
            "attributionAgent": "general-purpose",
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
        for k in expected_bag_keys() {
            assert!(cl(meta).contains_key(&k), "`{k}` must live in extra[\"claude\"]");
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
        let opts = Demand::COMPLETE.with_spans().with_offsets();
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
    fn bodyless_compaction_is_declared_without_transcript_text() {
        let s = parse_str(
            "s",
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b","parentUuid":"p","compactMetadata":{"trigger":"auto","preTokens":42}}"#,
            None,
        );
        let m = &s.messages[0];
        assert!(m.content.is_empty() && m.is_synthesized());
        assert_eq!(m.kind, MessageKind::CompactionBoundary);
        assert_eq!(m.id.as_deref(), Some("b"));
        assert_eq!(m.parent_id.as_deref(), Some("p"));
        assert_eq!(cl(m)["compactMetadata"]["preTokens"], 42);
        assert!(crate::render::turn_label(m).contains("compaction_boundary"));
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
        let end = subagent_end(&agent).unwrap();
        assert_eq!(end.stopped(), Some(true), "{end:?}");
        // A resume appends turns after the stop: no longer stopped.
        let mut resumed = std::fs::read_to_string(&agent).unwrap();
        resumed.push('\n');
        resumed.push_str(r#"{"type":"user","uuid":"u2","timestamp":"2026-10-01T00:05:00Z","message":{"role":"user","content":"poll it in the foreground"}}"#);
        std::fs::write(&agent, resumed).unwrap();
        assert_eq!(subagent_end(&agent).unwrap().stopped(), Some(false));
        // No stop record at all: unknown (a killed harness looks like this too).
        let fresh = dir.join("agent-a2.jsonl");
        std::fs::write(
            &fresh,
            r#"{"type":"user","uuid":"u","timestamp":"2026-10-01T00:00:00Z","message":{"role":"user","content":"go"}}"#,
        )
        .unwrap();
        assert_eq!(subagent_end(&fresh).unwrap().stopped(), None);

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
        let notices = task_notices(&parent).unwrap();
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
        // A distilled agent's payload sidecar sits beside it: not an agent, so `--agent a111` is
        // not ambiguous with it.
        std::fs::write(
            subs.join("agent-a111.flat.jsonl"),
            r#"{"id":"toolu_1","slot":"content","name":"Read","content":"big","size":3,"line_count":1,"kind":"text"}"#,
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
        let tree = subagent_tree(&parent).unwrap();
        assert_eq!(tree.len(), 3, "two tiers: 1 direct + 2 workflow, no `.flat` sidecar");
        // A Rich forest over unchanged transcripts scans none of them again; a change rescans
        // exactly the changed one.
        let scans = |before: usize| -> Vec<PathBuf> {
            FOREST_READS.lock().unwrap().1[before..]
                .iter()
                .filter(|(kind, _)| *kind == "scan")
                .map(|(_, path)| path.clone())
                .collect()
        };
        let before = FOREST_READS.lock().unwrap().1.len();
        let again = subagent_tree(&parent).unwrap();
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&tree).unwrap()
        );
        assert_eq!(
            scans(before),
            Vec::<PathBuf>::new(),
            "an unchanged forest was rescanned"
        );
        let unchanged = std::fs::read(&a1).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&a1)
            .unwrap()
            .write_all(b"{\"type\":\"user\",\"uuid\":\"z\",\"parentUuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"later\"}}\n")
            .unwrap();
        let before = FOREST_READS.lock().unwrap().1.len();
        subagent_tree(&parent).unwrap();
        assert_eq!(scans(before), std::slice::from_ref(&a1), "one change, one rescan");
        std::fs::write(&a1, &unchanged).unwrap();
        // A second process starts with no scan cache of its own: it reads the rows the first
        // one saved, and one change is still one rescan.
        subagent_tree(&parent).unwrap();
        crate::discover_cache::forget_process_cache();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&a1)
            .unwrap()
            .write_all(b"{\"type\":\"user\",\"uuid\":\"z2\",\"parentUuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"later\"}}\n")
            .unwrap();
        let before = FOREST_READS.lock().unwrap().1.len();
        subagent_tree(&parent).unwrap();
        assert_eq!(
            scans(before),
            std::slice::from_ref(&a1),
            "a new process: one change, one rescan"
        );
        std::fs::write(&a1, unchanged).unwrap();

        let before = FOREST_READS.lock().unwrap().1.len();
        let candidates = std::sync::atomic::AtomicUsize::new(0);
        let tools = subagent_tree_filtered(&parent, crate::SubagentDemand::Tools, Some("wf_abc-123"), &|id, run| {
            candidates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            id == "agent-a222" && run == Some("wf_abc-123")
        })
        .unwrap();
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
            .filter(|(kind, path)| *kind != "scan" && path.starts_with(&root))
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
        })
        .unwrap();
        assert_eq!(sessions.len(), 1);
        let reads: Vec<_> = FOREST_READS.lock().unwrap().1[before..]
            .iter()
            .filter(|(kind, path)| *kind != "scan" && path.starts_with(&root))
            .cloned()
            .collect();
        assert_eq!(reads, [("transcript", a1.clone())]);
        let selected_rich =
            subagent_tree_filtered(&parent, crate::SubagentDemand::Rich, None, &|id, _| id == "agent-a222").unwrap();
        assert_eq!(
            serde_json::to_value(&selected_rich[0]).unwrap(),
            serde_json::to_value(original).unwrap()
        );

        *FOREST_READS.lock().unwrap() = (None, Vec::new());
        let refs: Vec<_> = tree.iter().map(|s| &s.session).collect();
        for _ in 0..2 {
            assert_eq!(
                serde_json::to_vec(
                    &subagent_tree_filtered(&parent, crate::SubagentDemand::Sessions, None, &|_, _| true)
                        .unwrap()
                        .into_iter()
                        .map(|s| s.session)
                        .collect::<Vec<_>>()
                )
                .unwrap(),
                serde_json::to_vec(&refs).unwrap(),
                "cold and cached refs preserve both tiers and ordering"
            );
        }

        let mut sink = CollectSink::default();
        let usage = stream_file(
            "a111",
            &subs.join("agent-a111.jsonl"),
            &Demand::USAGE.with_spans(),
            &mut sink,
        );
        assert_eq!(usage.lineage.parent.as_deref(), Some("sess"));
        assert!(usage.lineage.spawned_by_tool_use.is_none());
        assert!(!usage.extra.contains_key("claude"));
        let full = new_session_for("a111", Some(subs.join("agent-a111.jsonl")), &Demand::RICH);
        assert_eq!(full.lineage.spawned_by_tool_use.as_deref(), Some("toolu_77"));

        let direct = tree.iter().find(|s| s.agent_id() == "a111").expect("direct agent");
        assert_eq!(direct.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(direct.description.as_deref(), Some("do task one"));
        assert_eq!(direct.tool_use_id.as_deref(), Some("toolu_77"));
        assert!(direct.workflow.is_none());
        assert_eq!(direct.result_status, None); // direct agents report via parent tool_result
                                                // its return value = the last assistant text turn.
        assert_eq!(
            subagent_return(&direct.session.path).unwrap().as_deref(),
            Some("done one")
        );

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
            Demand::LEAN.with_spans(),
            Demand::RICH.with_spans(),
            Demand::LEAN,
            Demand::RICH,
            Demand::COMPLETE,
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
                        assert!(
                            opts.spans
                                && opts.tier != Tier::Complete
                                && i < inputs.len()
                                && inputs[i].len() > INLINE_MAX
                        );
                        let start = range.offset() as usize;
                        assert_eq!(&text[start..start + range.len() as usize], inputs[i]);
                        assert_eq!(range.path(), path);
                    }
                    Json::Owned(_) => {
                        assert!(
                            !opts.spans
                                || opts.tier == Tier::Complete
                                || i == inputs.len()
                                || inputs[i].len() <= INLINE_MAX
                        );
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
        stream_str(
            "source",
            &text,
            Some(path.clone()),
            &Demand::LEAN.with_spans(),
            &mut sink,
        );
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
        for opts in [Demand::LEAN.with_spans(), Demand::RICH.with_spans()] {
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
        let cp = jsonl::checkpoint::<Transcript>(
            first.len() as u64,
            Demand::LEAN.with_spans(),
            &new_session("source", Some(path.clone())),
        );
        let mut sink = CollectSink::default();
        adapter
            .transcript(
                &reference,
                &Demand::LEAN.with_spans(),
                crate::stream::Start::Checkpoint(&cp),
                &mut sink,
            )
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
        // identity. A slow callback must stop the operation before another callback.
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
                std::thread::sleep(jsonl::Read::SLOW_CALLBACK * 10);
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
            .stream(&reference, &Demand::LEAN.with_spans(), &mut deleting)
            .is_err());
        assert_eq!(deleting.calls, 1);
        assert_eq!(deleting.checkpoints, 0);
        assert!(deleting.captured.unwrap().resolve().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn callback_truncation_is_rejected_before_resuming_mapped_records() {
        if !crate::harness::store::jsonl::tests::in_child(
            "harness::claude::tests::callback_truncation_is_rejected_before_resuming_mapped_records",
        ) {
            return;
        }
        struct Mutate {
            path: PathBuf,
            original: Vec<u8>,
            file: fs::File,
            at: &'static str,
            replace: bool,
            mutate: bool,
            calls: usize,
            checkpoints: usize,
            held: Option<Message>,
        }
        impl Mutate {
            fn change(&mut self, at: &str) {
                if self.mutate && (self.at == at || (self.at == "ignored-wants" && at == "wants")) {
                    if self.replace {
                        fs::rename(&self.path, self.path.with_extension("moved")).unwrap();
                        fs::write(&self.path, &self.original).unwrap();
                    }
                    self.file.set_len(0).unwrap();
                    self.mutate = false;
                    // A callback that changed the file is a slow one by construction; the reader
                    // checks the backing after a slow callback, before touching the mapping again.
                    std::thread::sleep(jsonl::Read::SLOW_CALLBACK * 10);
                }
            }
        }
        impl MessageSink for Mutate {
            fn message(&mut self, message: Message) -> Flow {
                self.calls += 1;
                self.held = Some(message);
                self.change("message");
                Flow::Continue
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                self.change("wants");
                self.at == "checkpoint"
            }
            fn checkpoint(&mut self, _: &mut Checkpoint) {
                self.checkpoints += 1;
                self.change("checkpoint");
            }
        }
        let world = crate::test_world("claude-callback-truncation");
        let path = world.proj.join("source.jsonl");
        for spans in [false, true] {
            let first = serde_json::json!({"type":"user","uuid":"first","message":{"role":"user","content":"owned ".repeat(1000)}});
            let tail = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"tail\"}}\n".repeat(1000);
            let original = format!("{first}\n{tail}").into_bytes();
            for at in ["message", "wants", "checkpoint", "ignored-wants"] {
                let original = if at == "ignored-wants" {
                    [
                        b"{\"type\":\"custom-title\",\"customTitle\":\"metadata first\"}\n".as_slice(),
                        &original,
                    ]
                    .concat()
                } else {
                    original.clone()
                };
                for (mutate, replace) in [(false, false), (true, false), (true, true)] {
                    fs::write(&path, &original).unwrap();
                    let mut sink = Mutate {
                        path: path.clone(),
                        original: original.clone(),
                        file: fs::OpenOptions::new().write(true).open(&path).unwrap(),
                        at,
                        replace,
                        mutate,
                        calls: 0,
                        checkpoints: 0,
                        held: None,
                    };
                    let reference = SessionRef::new("source", Harness::Claude, path.clone());
                    let opts = if spans { Demand::LEAN.with_spans() } else { Demand::RICH };
                    let result = Claude::with_config_dir(&world.proj).stream(&reference, &opts, &mut sink);
                    assert_eq!(result.is_err(), mutate, "spans={spans},at={at},replace={replace}");
                    if mutate {
                        assert_eq!(
                            sink.calls,
                            usize::from(at != "ignored-wants"),
                            "do not resume the mapped framer after a callback shrinks it"
                        );
                        assert_eq!(sink.checkpoints, usize::from(at == "checkpoint"));
                        if at == "ignored-wants" {
                            assert!(
                                sink.held.is_none(),
                                "metadata checkpoint polling precedes every delivered message"
                            );
                            continue;
                        }
                        let Block::Text { text } = &sink.held.as_ref().unwrap().content[0] else {
                            panic!("first text")
                        };
                        assert_eq!(matches!(text, Text::Source(_)), spans);
                        if let Text::Inline(text) = text {
                            assert_eq!(text, &"owned ".repeat(1000));
                        }
                    } else {
                        assert_eq!(sink.calls, 1001);
                        assert_eq!(fs::read(&path).unwrap(), original);
                    }
                }
            }
        }
    }

    #[test]
    fn native_json_inputs_verify_completion_even_after_owned_early_stop() {
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
        fs::write(&path, tiny.clone() + &tiny).unwrap();
        let mut stable_calls = 0;
        adapter
            .stream(&reference, &Demand::LEAN.with_spans(), &mut |_: Message| {
                stable_calls += 1;
                Flow::Stop
            })
            .unwrap();
        assert_eq!(
            stable_calls, 1,
            "intentional stop succeeds while the captured source is unchanged"
        );
        struct RemoveOwned {
            path: PathBuf,
            owned: Option<Message>,
            calls: usize,
            checkpoints: usize,
        }
        impl MessageSink for RemoveOwned {
            fn message(&mut self, message: Message) -> Flow {
                self.calls += 1;
                self.owned = Some(message);
                fs::remove_file(&self.path).unwrap();
                Flow::Stop
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                true
            }
            fn checkpoint(&mut self, _: &mut Checkpoint) {
                self.checkpoints += 1;
            }
        }
        let mut deleting = RemoveOwned {
            path: path.clone(),
            owned: None,
            calls: 0,
            checkpoints: 0,
        };
        assert!(adapter
            .stream(&reference, &Demand::LEAN.with_spans(), &mut deleting)
            .is_err());
        assert_eq!((deleting.calls, deleting.checkpoints), (1, 0));
        let Block::ToolUse {
            input: Json::Owned(input),
            ..
        } = &deleting.owned.as_ref().unwrap().content[0]
        else {
            panic!("tiny tool input should remain owned and usable");
        };
        assert_eq!(input["path"], "x");
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
            .stream(&reference, &Demand::LEAN.with_spans(), &mut remove_later)
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
            message_count: Some(1),
            ..SessionRef::new("s", Harness::Claude, path.clone())
        };
        // Lazy parse → expect a Span; resolving it must equal `big`.
        let mut sink = CollectSink::default();
        let mut s = stream_file(&r.id, &path, &Demand::LEAN.with_spans(), &mut sink);
        s.messages = sink.messages;
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(text.is_lazy(), "large content should be a span");
                let resolver = s.resolver();
                assert_eq!(text.try_resolve(&resolver).unwrap(), big.as_str());
            }
            _ => panic!("expected text block"),
        }
        // And materialize() makes it inline and equal.
        s.materialize().unwrap();
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(!text.is_lazy());
                assert_eq!(text.inline_str(), Some(big.as_str()));
            }
            _ => panic!("expected text block"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stream a transcript file through the one reader.
    fn stream_file(id: &str, path: &Path, opts: &Demand, sink: &mut dyn MessageSink) -> Session {
        // A prefix through EOF: the whole transcript, without the fork-source lookup.
        let r = SessionRef::new(id, Harness::Claude, path);
        ROOTLESS
            .transcript(&r, opts, Start::Prefix(u64::MAX), sink)
            .unwrap()
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
                Demand::LEAN.with_spans(),
                Demand::LEAN.with_spans().with_offsets(),
                Demand::RICH.with_spans(),
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
                        .transcript(&r, &opts, crate::stream::Start::Checkpoint(cp), &mut rest)
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

        let opts = Demand::LEAN.with_spans().with_offsets();
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
        stream_file("s", &path, &Demand::LEAN, &mut bulk);
        assert!(bulk
            .messages
            .iter()
            .all(|m| !m.extra.contains_key(crate::offsets::OFFSET_KEY)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bulk_opts_skip_extra_but_keep_text() {
        // The bulk path (`Demand::LEAN`) must NOT materialize the fat `toolUseResult`
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
        let mut bulk = stream_str("s1", &text, None, &Demand::LEAN, &mut sink);
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
        let searchable = |s: &Session| {
            let mut out = String::new();
            for m in &s.messages {
                m.append_searchable(&mut out).unwrap();
            }
            out
        };
        assert_eq!(
            searchable(&full),
            searchable(&bulk),
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
        let _ = stream_str("s1", &text, None, &Demand::LEAN, &mut sink);
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
    }
}
