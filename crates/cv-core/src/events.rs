//! The event catalog: what a session **did**, not just what was said.
//!
//! The FTS index answers "which session mentioned X"; this module answers "which session *touched*
//! file X / ran command Y / hit error Z". Every [`Block::ToolUse`]/[`Block::ToolResult`] streaming
//! past is distilled into a tiny normalized [`Event`] row (kind + tool + target), persisted into the
//! same SQLite catalog as [`crate::catalog`]'s `sessions` table. That makes `cv touched <path>`
//! an indexed lookup instead of a corpus re-parse, and is the substrate for `cv blame`
//! (line of code → the session whose reasoning wrote it).
//!
//! Tool names differ wildly across harnesses (claude `Edit`/`Read`/`Bash`, codex
//! `shell`/`apply_patch`/`local_shell`, gemini `run_shell_command`/`write_file`/`replace`, …), so
//! classification is heuristic: a file-path key in the input plus a read/write-flavored name, a
//! command key, or a recognizable `apply_patch` body whose per-file headers are mined for targets.
//! Unrecognized tools degrade to `kind='tool'` rather than being dropped.
//!
//! Memory discipline (the project's core value): the sink owns only the small classified rows.
//! Tool arguments are projected from owned or validated source-backed JSON; only selected paths,
//! command prefixes and actual patch target lines are retained. Tool result content is never
//! materialized — under [`Demand::LEAN.with_spans()`](crate::stream::Demand::with_spans) a large result
//! is an unresolved [`Span`](crate::lazy::Span) and the error `detail` is simply skipped for it.
//! Like [`crate::catalog`], the persistence side is best-effort: SQLite errors degrade to empty
//! results / no-ops with a named diagnostic, never silently failing the caller.

use crate::ir::{Block, Message, Session, SessionRef};
use crate::json::{Json, JsonView};
use crate::stream::{Checkpoint, Flow, MessageSink};
use anyhow::Result;
#[cfg(test)]
use serde_json::Value;
use std::path::{Path, PathBuf};

mod spool;
pub use spool::{EventSpool, SpoolingSink};

/// Controls catalog ingestion over the discovered corpus.
#[derive(Debug, Clone, Copy, Default)]
pub struct IngestOptions {
    pub force: bool,
    pub subagents: bool,
}

/// One normalized thing a session did, extracted from a message's tool blocks.
///
/// `kind` is one of: `file_edit`, `file_read`, `command`, `tool` (other/unknown), `error`
/// (a tool result flagged as failed). `target` is an absolute file path for file ops (relative
/// paths resolved against the session cwd), the command head for commands, `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Event {
    /// Index of the message (in transcript stream order) this event came from.
    pub msg_idx: usize,
    /// Unix seconds of the message timestamp, when the harness recorded one.
    pub ts: Option<i64>,
    pub kind: &'static str,
    /// The raw tool name as the harness recorded it (`None` for an error result whose tool is unknown).
    pub tool: Option<String>,
    pub target: Option<String>,
    /// An error result's whole text (trimmed), resolved from the source when it is lazy.
    pub detail: Option<String>,
    /// The first row of its `tool_use` block. One call can yield several rows (a patch sent through
    /// the shell is a `command` row plus a `file_edit` per file); counting calls counts these.
    pub call: bool,
}

/// A [`MessageSink`] that classifies each message's tool blocks into [`Event`]s as it streams past.
/// Holds only the (small) accumulated rows — message content is never retained.
pub struct EventSink {
    cwd: Option<PathBuf>,
    msg_idx: usize,
    events: Vec<Event>,
    dependencies: crate::source::ReadSet,
    capture_events: bool,
    extractor: EventExtractor,
    /// Resolves a lazy error result for its `detail`: the session's source, from
    /// [`with_source`](Self::with_source) or the streamed meta session.
    resolver: crate::lazy::Resolver,
    latch: crate::stream::Latch,
}

impl EventSink {
    /// `cwd` is the session working directory relative file paths resolve against (usually the
    /// [`SessionRef`]'s); when `None`, the adapter's `meta` cwd is used if it arrives.
    pub fn new(cwd: Option<PathBuf>) -> Self {
        EventSink {
            cwd,
            msg_idx: 0,
            events: Vec::new(),
            dependencies: Default::default(),
            capture_events: true,
            extractor: EventExtractor::default(),
            resolver: crate::lazy::Resolver::new(None),
            latch: Default::default(),
        }
    }

    /// Resolve lazy error results against `source` (the session's file) until a streamed meta
    /// session names its own, parse-time stamped, source.
    pub fn with_source(mut self, source: PathBuf) -> Self {
        self.resolver = crate::lazy::Resolver::new(Some(source));
        self
    }

    /// Observe a borrowed message when another consumer owns its delivery.
    pub fn observe_message(&mut self, m: &Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        if let Err(error) = self.extractor.extract(
            m,
            self.msg_idx,
            self.cwd.as_deref(),
            None,
            Some(&self.resolver),
            &mut self.events,
        ) {
            self.latch.fail(error);
            return Flow::Stop;
        }
        if !self.capture_events {
            release_scratch(&mut self.events);
        }
        self.msg_idx += 1;
        Flow::Continue
    }

    /// Keep checkpoint/call identity state and dependencies without retaining event rows.
    /// Useful when events are fresh but another consumer needs a new shared resume mark.
    pub fn without_events(mut self) -> Self {
        self.capture_events = false;
        self
    }

    /// Auxiliary adapter reads captured before their metadata or content was consulted.
    pub fn dependencies(&self) -> &crate::source::ReadSet {
        &self.dependencies
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn into_events(mut self) -> Result<Vec<Event>> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        Ok(self.events)
    }

    /// The sink a checkpoint kept (see [`MessageSink::checkpoint`]), continuing with the message
    /// index and cwd it had there: its events start at that index, for [`record_from`]. `None`
    /// when the checkpoint was taken without one.
    pub fn resume(cp: &Checkpoint) -> Option<EventSink> {
        let st: EventState = cp.get(EVENTS_LAYER)?;
        Some(EventSink {
            cwd: st.cwd,
            msg_idx: st.msg_idx,
            events: Vec::new(),
            dependencies: st.dependencies,
            capture_events: true,
            extractor: EventExtractor::resume(st.extractor).ok()?,
            resolver: crate::lazy::Resolver::new(None),
            latch: Default::default(),
        })
    }
}

/// The [`Checkpoint`] layer an [`EventSink`] keeps its state under.
const EVENTS_LAYER: &str = "events";

/// What an [`EventSink`] holds at a checkpoint: the rows so far are in the catalog already.
#[derive(serde::Serialize, serde::Deserialize)]
struct EventState {
    extractor: ExtractorState,
    msg_idx: usize,
    cwd: Option<PathBuf>,
    dependencies: crate::source::ReadSet,
}

impl MessageSink for EventSink {
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.dependencies.record(path);
        }
    }
    fn append_only_dependency(&mut self, path: &std::path::Path) {
        if !self.latch.failed() {
            self.dependencies.record_append_only(path);
        }
    }
    fn meta(&mut self, s: &Session) {
        if self.latch.failed() {
            return;
        }
        if self.cwd.is_none() {
            self.cwd = s.cwd.clone();
        }
        if s.source_path.is_some() {
            self.resolver = s.resolver();
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        self.observe_message(&m)
    }

    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("event consumer")
    }

    fn wants_checkpoint(&mut self, _offset: u64) -> bool {
        !self.latch.failed() && self.extractor.can_checkpoint()
    }

    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if self.latch.failed() {
            return;
        }
        let Some(extractor) = self.extractor.checkpoint() else {
            return;
        };
        cp.put(
            EVENTS_LAYER,
            &EventState {
                extractor,
                msg_idx: self.msg_idx,
                cwd: self.cwd.clone(),
                dependencies: self.dependencies.clone(),
            },
        );
    }
}

/// Classify one message's tool blocks into events. `msg_idx` is its index in the transcript;
/// relative file targets resolve against `cwd`. The standalone form of what [`EventSink`] does
/// per streamed message. `resolver` reads a lazy error result whole for its `detail`.
pub fn extract(
    m: &Message,
    msg_idx: usize,
    cwd: Option<&Path>,
    resolver: &crate::lazy::Resolver,
) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    EventExtractor::default().extract(m, msg_idx, cwd, None, Some(resolver), &mut out)?;
    Ok(out)
}

/// Reuse ordinary calls' row storage without retaining a wide patch's buffer between messages.
pub(crate) fn release_scratch(events: &mut Vec<Event>) {
    events.clear();
    if events.capacity() > 64 {
        *events = Vec::new();
    }
}

/// Correlate native completed file changes by exact call ID and resolved target.
/// Call markers and target pairs share one bounded set; completions use the other.
/// Code-mode nested calls have their own native IDs and no visible ToolUse counterpart.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ExtractorState {
    inferred_keys: std::collections::BTreeSet<String>,
    #[serde(default)]
    completed: std::collections::BTreeSet<String>,
}

pub(crate) struct EventExtractor {
    inferred_edits: crate::spill::ExactStrings,
    completed: crate::spill::ExactStrings,
    pending: usize,
}

impl Default for EventExtractor {
    fn default() -> Self {
        Self {
            inferred_edits: crate::spill::ExactStrings::with_memory_limit(64 * 1024),
            completed: crate::spill::ExactStrings::with_memory_limit(64 * 1024),
            pending: 0,
        }
    }
}

impl EventExtractor {
    fn resume(state: ExtractorState) -> Result<Self> {
        anyhow::ensure!(
            state.completed.is_subset(&state.inferred_keys) && state.completed.iter().all(|key| key.starts_with('c')),
            "invalid inferred call checkpoint"
        );
        let mut extractor = Self {
            pending: state.inferred_keys.iter().filter(|key| key.starts_with('c')).count() - state.completed.len(),
            ..Self::default()
        };
        for id in state.inferred_keys {
            extractor.inferred_edits.insert(id)?;
        }
        for id in state.completed {
            extractor.completed.insert(id)?;
        }
        Ok(extractor)
    }

    // Spill files belong to this read operation. Decline the shared checkpoint until
    // identities fit in its owned state again; append recovery replays the prior prefix.
    fn can_checkpoint(&self) -> bool {
        self.inferred_edits.memory_values().is_some() && self.completed.memory_values().is_some()
    }

    fn checkpoint(&self) -> Option<ExtractorState> {
        Some(ExtractorState {
            inferred_keys: self.inferred_edits.memory_values()?.iter().cloned().collect(),
            completed: self.completed.memory_values()?.iter().cloned().collect(),
        })
    }

    fn note_inferred(&mut self, id: &str) -> Result<()> {
        if self.inferred_edits.insert(format!("c{id}"))? {
            self.pending += 1;
        }
        Ok(())
    }

    // Length-prefix the ID so arbitrary ID/path bytes cannot alias another pair or a call key.
    fn target_key(id: &str, target: &str) -> String {
        format!("p{}:{id}{target}", id.len())
    }

    fn note_target(&mut self, id: &str, target: &str) -> Result<()> {
        self.note_inferred(id)?;
        self.inferred_edits.insert(Self::target_key(id, target))?;
        Ok(())
    }

    fn note_exec_target(&mut self, target: &str) -> Result<()> {
        if Path::new(target).is_absolute() {
            self.inferred_edits.insert(format!("ea{target}"))?;
        } else {
            let rel = target.trim_start_matches("./");
            if !rel.is_empty() {
                self.inferred_edits.insert(format!("er/{rel}"))?;
            }
        }
        Ok(())
    }

    fn has_inferred_edit(&mut self, id: &str, path: &str) -> Result<bool> {
        if self.inferred_edits.contains(&Self::target_key(id, path))? {
            return Ok(true);
        }
        if id.starts_with("exec-") {
            if self.inferred_edits.contains(&format!("ea{path}"))? {
                return Ok(true);
            }
            for (idx, b) in path.bytes().enumerate() {
                if b == b'/' && self.inferred_edits.contains(&format!("er{}", &path[idx..]))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn complete(&mut self, id: &str) -> Result<()> {
        if self.inferred_edits.contains(&format!("c{id}"))? && self.completed.insert(format!("c{id}"))? {
            self.pending = self.pending.saturating_sub(1);
            if self.pending == 0 {
                self.inferred_edits.clear();
                self.completed.clear();
            }
        }
        Ok(())
    }

    /// Classify `m` into `out`. With `details`, rows carry their text whole: a command's string
    /// and an error's (resolved) text. Without, a caller that only counts or matches tools and
    /// files reads and assembles neither.
    pub(crate) fn extract(
        &mut self,
        m: &Message,
        msg_idx: usize,
        cwd: Option<&Path>,
        tool: Option<&str>,
        details: Option<&crate::lazy::Resolver>,
        out: &mut Vec<Event>,
    ) -> Result<()> {
        extract_message(m, msg_idx, cwd, tool, details, out, self)
    }
}

/// A file edit a harness record states outside its tool blocks
/// ([`crate::harness::Adapter::native_edit`]): one `file_edit` event per path, under `tool`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEdit<'m> {
    /// The edit's own id, which a tool call that already produced its events carries too.
    pub id: Option<&'m str>,
    /// The tool the edit is attributed to.
    pub tool: &'static str,
    /// Every path the edit touches (sources and move destinations), as the record spells them.
    pub paths: Vec<&'m str>,
}

fn extract_message(
    m: &Message,
    msg_idx: usize,
    cwd: Option<&Path>,
    tool: Option<&str>,
    details: Option<&crate::lazy::Resolver>,
    out: &mut Vec<Event>,
    extractor: &mut EventExtractor,
) -> Result<()> {
    let mut staged = Vec::new();
    let ts = m.timestamp.map(|t| t.timestamp());
    // A file edit the harness record states outside its tool blocks, unless a call already did.
    if let Some(edit) = crate::harness::native_edit(m) {
        if let Some(id) = edit.id {
            let paths: std::collections::BTreeSet<_> = edit.paths.iter().map(|path| absolutize(path, cwd)).collect();
            for path in paths {
                if !extractor.has_inferred_edit(id, &path)? {
                    staged.push(Event {
                        msg_idx,
                        ts,
                        kind: "file_edit",
                        tool: Some(edit.tool.into()),
                        target: Some(path),
                        detail: None,
                        call: false,
                    });
                }
            }
        }
    }
    for b in &m.content {
        match b {
            Block::ToolUse { id, name, input, .. } => {
                let before = staged.len();
                extract_call_into(name, input, msg_idx, ts, cwd, details.is_some(), &mut staged)?;
                for event in staged[before..].iter().filter(|event| event.kind == "file_edit") {
                    if let Some(target) = &event.target {
                        extractor.note_target(id, target)?;
                        if name == "exec" {
                            extractor.note_exec_target(target)?;
                        }
                    }
                }
            }
            Block::ToolResult {
                content,
                is_error: true,
                tool_name,
                ..
            } => {
                // The whole error text: a lazy one is read from the source. A caller that keeps
                // no text (`cv tools` counts rows) passes no resolver and reads nothing.
                let detail = match details {
                    Some(resolver) => Some(content.try_resolve(resolver)?)
                        .map(|text| text.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                    None => None,
                };
                staged.push(Event {
                    msg_idx,
                    ts,
                    kind: "error",
                    tool: tool_name.clone(),
                    target: None,
                    detail,
                    call: false,
                });
            }
            _ => {}
        }
    }
    for block in &m.content {
        if let Block::ToolResult { tool_use_id, .. } = block {
            extractor.complete(tool_use_id)?;
        }
    }
    // Correlation always sees every call. Select only the canonical attributed rows.
    out.extend(
        staged
            .into_iter()
            .filter(|event| tool.is_none_or(|name| event.tool.as_deref() == Some(name))),
    );
    Ok(())
}

/// Classify a borrowed call without cloning its possibly owned, oversized arguments. A `command`
/// row's target is the whole command only when `commands`; otherwise it is `None` and the command
/// is never assembled.
pub(crate) fn extract_call_into(
    name: &str,
    input: &crate::Json,
    msg_idx: usize,
    ts: Option<i64>,
    cwd: Option<&Path>,
    commands: bool,
    out: &mut Vec<Event>,
) -> Result<()> {
    out.extend(
        input
            .with_view(|view| classify_tool_view(name, view, cwd, commands))?
            .into_iter()
            .enumerate()
            .map(|(i, (kind, target))| Event {
                msg_idx,
                ts,
                kind,
                tool: Some(name.to_owned()),
                target,
                detail: None,
                call: i == 0,
            }),
    );
    Ok(())
}

/// Cross-harness classification of one tool invocation into `(kind, target)` rows via
/// [`crate::tools::classify::visit_tool_ops`].
fn classify_tool_view(
    name: &str,
    input: JsonView<'_>,
    cwd: Option<&Path>,
    commands: bool,
) -> Result<Vec<(&'static str, Option<String>)>> {
    use crate::tools::classify::{ClassifyOpts, ToolOp};
    let opts = if commands {
        ClassifyOpts::EVENTS
    } else {
        ClassifyOpts::COUNTS
    };
    let mut rows = Vec::new();
    let mut fallback_path = None;
    crate::tools::classify::visit_tool_ops(None, name, input, opts, |sem| match sem.op {
        ToolOp::Command { cmd, .. } => rows.push(("command", cmd.map(std::borrow::Cow::into_owned))),
        ToolOp::FileEdit { path, .. } => rows.push(("file_edit", Some(absolutize(&path, cwd)))),
        ToolOp::FileRead { path } | ToolOp::Search { path: Some(path), .. } => {
            rows.push(("file_read", Some(absolutize(&path, cwd))));
        }
        ToolOp::Other { path: Some(path), .. } if fallback_path.is_none() => {
            fallback_path = Some(absolutize(&path, cwd));
        }
        _ => {}
    })?;
    if rows.is_empty() {
        rows.push(("tool", fallback_path));
    }
    Ok(rows)
}

/// Full command projection: the string a `command` event's target holds.
pub(crate) fn command_of_json(input: &Json) -> Result<Option<String>> {
    input.with_view(crate::tools::classify::command_string)
}

/// Lexically absolutize `p` against the session cwd. Already-absolute paths pass through; with no
/// cwd the path is stored as given (still suffix-matchable by `cv touched`).
fn absolutize(p: &str, cwd: Option<&Path>) -> String {
    if Path::new(p).is_absolute() {
        return p.to_string();
    }
    let trimmed = p.trim_start_matches("./");
    match cwd {
        Some(c) => c.join(trimmed).to_string_lossy().into_owned(),
        None => trimmed.to_string(),
    }
}

/// Provenance for a sub-agent event source: which parent session, which workflow run (if any), and
/// which agent inside it the events came from. A top-level session has none of these (`None`).
///
/// Carried on the `event_sessions` row so a stored sub-agent's events can be attributed back to the
/// orchestrator that spawned it (`cv touched` / `cv events` can then say *which* agent of *which*
/// workflow of *which* parent edited a file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The top-level session id this sub-agent was (transitively) spawned by.
    pub parent_id: Option<String>,
    /// The sub-agent's own agent id (`agentId`, the `agent-` prefix stripped).
    pub agent_id: Option<String>,
    /// The workflow run id (`wf_…`) this agent belonged to, or `None` for a directly-spawned agent.
    pub workflow: Option<String>,
}

impl Provenance {
    /// True when this carries no sub-agent attribution — i.e. an ordinary top-level session.
    pub fn is_empty(&self) -> bool {
        self.parent_id.is_none() && self.agent_id.is_none() && self.workflow.is_none()
    }
}

/// Read current events, refreshing stale rows on use. Frozen commands stream stale sources
/// directly so the answer stays current without publishing either events or certified counts.
pub fn read_ref(r: &crate::SessionRef, kind: Option<&str>) -> Result<Vec<EventRow>> {
    if needs_ingest(r) {
        if crate::env::frozen() {
            let adapter = crate::harness::for_harness(r.harness)
                .ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
            let mut sink = EventSink::new(r.cwd.clone()).with_source(r.path.clone());
            adapter.try_stream(r, &crate::Demand::LEAN.with_spans(), &mut sink)?;
            let mut events = sink.into_events()?;
            // Match catalog order.
            events.sort_by_key(|e| (e.msg_idx, e.kind));
            return Ok(events
                .into_iter()
                .filter(|e| kind.is_none_or(|k| e.kind == k))
                .map(|e| EventRow {
                    harness: r.harness.as_str().into(),
                    session_id: r.id.clone(),
                    msg_idx: e.msg_idx as i64,
                    ts: e.ts,
                    kind: e.kind.into(),
                    tool: e.tool,
                    target: e.target,
                    detail: e.detail,
                    parent_id: None,
                    agent_id: None,
                    workflow: None,
                })
                .collect());
        }
        ingest_ref(r)?;
    }
    events_for(r.harness.as_str(), &r.id, kind)
}

/// One persisted event row, as read back from the catalog.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub harness: String,
    pub session_id: String,
    pub msg_idx: i64,
    pub ts: Option<i64>,
    pub kind: String,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub detail: Option<String>,
    /// Sub-agent attribution (all `None` for a top-level session).
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub workflow: Option<String>,
}

/// One session in a `cv touched <path>` result: who touched the file, how, and when last.
#[derive(Debug, Clone)]
pub struct TouchedSession {
    pub harness: String,
    pub session_id: String,
    /// Title from the `sessions` catalog table, when that row exists.
    pub title: Option<String>,
    pub edits: i64,
    pub reads: i64,
    /// Latest event timestamp on the file in this session (unix secs).
    pub last_ts: Option<i64>,
    /// Sub-agent attribution (all `None` for a top-level session): the parent session, the
    /// workflow run, and the agent inside it whose events touched the file.
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub workflow: Option<String>,
}

/// One `file_edit` event joined with its session's catalog metadata — the raw material for
/// `cv blame`'s commit↔session correlation (the timestamps and cwd are what the scoring runs on).
#[derive(Debug, Clone)]
pub struct EditEvent {
    pub harness: String,
    pub session_id: String,
    pub msg_idx: i64,
    /// Unix seconds of the *message* that made the edit (`None` when the harness records none).
    pub ts: Option<i64>,
    /// The target path exactly as the event recorded it (may be from another checkout).
    pub target: Option<String>,
    /// Title from the `sessions` catalog table, when that row exists.
    pub title: Option<String>,
    /// Session working directory from the `sessions` catalog (string form).
    pub cwd: Option<String>,
    /// Session created/updated unix seconds — the weak-match window when `ts` is `None`.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// For a sub-agent transcript folded in by `cv index --subagents`: the session that spawned it
    /// (often the one that committed what the sub-agent edited).
    pub parent_id: Option<String>,
}

#[cfg(feature = "sqlite")]
mod db {
    use super::*;
    use anyhow::{Context as _, Result};
    use rusqlite::{params, Connection};

    /// Open the shared catalog db and ensure the event tables exist. `None` on any error.
    ///
    /// Schema notes:
    /// * `event_sessions` is a dedicated intern table rather than a reuse of the `sessions`
    ///   rowid: `sessions` rows are `INSERT OR REPLACE`d on every sync, which deletes+reinserts
    ///   and so *changes* the rowid — events keyed on it would silently orphan. Its PK is also
    ///   `(harness, id, path)`, so one logical session at two paths would have two rowids.
    ///   `UNIQUE(harness, session_id)` here is stable across syncs and tiny (~2.4k rows).
    /// * `event_targets` interns the distinct target strings (~54k of ~145k events); event rows
    ///   carry a nullable integer. Re-ingest deletes a session's event rows but leaves its
    ///   interned targets — orphans are unreferenced cache rows, reclaimed only when a
    ///   catalog of another schema is rebuilt. Suffix matching scans `event_targets` instead of every event row.
    /// * No dedup: [`record`] is DELETE-by-session + batch INSERT in one transaction, and every
    ///   extracted event is a row.
    /// * `idx_events_session` serves [`events_for`] and `record`'s delete; `idx_events_target`
    ///   joins event rows back from a matched target.
    pub(crate) fn open() -> Option<Connection> {
        let conn = crate::catalog::open_db()?;
        if crate::env::frozen() {
            return Some(conn);
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_sessions(
                 id         INTEGER PRIMARY KEY,
                 harness    TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 parent_id  TEXT,
                 agent_id   TEXT,
                 workflow   TEXT,
                 UNIQUE(harness, session_id)
             );
             CREATE TABLE IF NOT EXISTS event_targets(
                 id     INTEGER PRIMARY KEY,
                 target TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS events(
                 session   INTEGER NOT NULL,
                 msg_idx   INTEGER NOT NULL,
                 ts        INTEGER,
                 kind      TEXT NOT NULL,
                 tool      TEXT,
                 target_id INTEGER,
                 detail    TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_events_session ON events(session);
             CREATE INDEX IF NOT EXISTS idx_events_target ON events(target_id);",
        )
        .ok()?;
        Some(conn)
    }

    /// The `events` consumer's record payload: the provenance and cwd its rows were attributed
    /// under. Another provenance's rows are another output.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub(crate) struct Attributed {
        parent_id: Option<String>,
        agent_id: Option<String>,
        workflow: Option<String>,
        cwd: Option<String>,
        // Required: old catalogs/checkpoints lack target-level correlation and must replay.
        target_correlation: bool,
    }

    impl crate::source::Payload for Attributed {
        const TAG: &'static str = "events";
        const BY_ROWS: bool = true;
    }

    fn authority(r: &SessionRef, prov: &Provenance) -> Attributed {
        Attributed {
            parent_id: prov.parent_id.clone(),
            agent_id: prov.agent_id.clone(),
            workflow: prov.workflow.clone(),
            cwd: r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            target_correlation: true,
        }
    }

    /// `r`'s events record, whatever its source now.
    fn attributed_in(conn: &Connection, r: &SessionRef) -> Option<Attributed> {
        Some(crate::source::record_in::<Attributed>(conn, "events", r)?.payload)
    }

    /// Whether `r` needs an event (re)ingest: its source as captured now is not the one its
    /// published baseline recorded, or that baseline belongs to another provenance, or there is
    /// none. `false` when the catalog can't be opened — events degrade silently rather than
    /// re-streaming the corpus into a broken db forever.
    ///
    /// Opens a fresh connection per call — fine for a one-off check (`cv events <id>`), wrong for
    /// a corpus sweep: bulk callers use [`SyncTable`] (one query for the whole table).
    pub fn needs_ingest(r: &SessionRef) -> bool {
        let Some(conn) = open() else {
            return crate::env::frozen();
        };
        let source = crate::source::Probe::default().snapshot(r);
        !crate::source::fresh_in(&conn, "events", r, &source, &authority(r, &Provenance::default()))
    }

    /// Every published event baseline, loaded with **one** connection + query, for bulk passes
    /// that check every discovered session ([`ingest_all`], the FTS indexer's event ride-along).
    /// The per-session [`needs_ingest`] opens a connection — and re-runs the catalog DDL — per
    /// call, which a ~2,400-session sweep pays ~2,400 times.
    ///
    /// Answers exactly like [`needs_ingest`], including the degrade: when the catalog can't be
    /// opened, every check returns `false` (don't re-stream the corpus into a broken db).
    pub struct SyncTable {
        /// `None` when the catalog is unavailable.
        baselines: Option<crate::source::Table<Attributed>>,
    }

    impl SyncTable {
        pub fn load() -> SyncTable {
            SyncTable {
                baselines: open().map(|conn| crate::source::Table::load_in(&conn, "events")),
            }
        }

        /// Whether `r`'s one published event output is stale: provenance, source identity and
        /// dependencies, checked against the loaded table with the operation's shared probe.
        pub fn needs_ingest(
            &self,
            r: &SessionRef,
            source: &crate::source::Baseline,
            provenance: &Provenance,
            probe: &mut crate::source::Probe,
        ) -> bool {
            self.baselines
                .as_ref()
                .is_some_and(|table| table.admit(r, source, probe) != Some(&authority(r, provenance)))
        }

        /// Whether retained prefix events belong to this source copy and expected provenance.
        /// This is separate from unchanged freshness: an ordinary append changes the identity.
        pub fn can_resume(&self, r: &SessionRef, provenance: &Provenance) -> bool {
            self.baselines.as_ref().is_some_and(|table| {
                table
                    .get(r)
                    .is_some_and(|record| record.payload == authority(r, provenance))
            })
        }
    }

    /// Publish a completed ordered batch with bounded row/target retention in one transaction:
    /// delete the prior rows of messages `from` and later, insert, and publish the source
    /// baseline from `source`, captured by the caller **before** streaming, so content appended
    /// mid-parse reads as stale next time. Any spill read, ordering or SQL failure rolls back rows
    /// and freshness together.
    ///
    /// Dedup invariant: the DELETE-by-session + batch-INSERT shape makes cross-session duplicates
    /// impossible by construction, and duplicates within one message — same `(msg_idx, kind,
    /// target)`, `COALESCE(target,'')` semantics — are dropped as they arrive.
    pub fn record_spooled_with_source(
        r: &SessionRef,
        from: usize,
        events: EventSpool,
        source: &crate::source::Baseline,
        prov: &Provenance,
    ) -> Result<()> {
        if crate::env::frozen() {
            return Ok(());
        }
        let mut conn = open().ok_or_else(|| anyhow::anyhow!("event catalog unavailable"))?;
        record_spooled_in(&mut conn, r, from, events, source, prov)
    }

    pub(crate) fn record_spooled_in(
        conn: &mut Connection,
        r: &SessionRef,
        from: usize,
        events: EventSpool,
        source: &crate::source::Baseline,
        prov: &Provenance,
    ) -> Result<()> {
        if crate::env::frozen() {
            return Ok(());
        }
        let rows = events.rows()?;
        record_from_in(conn, r, from, rows, prov, source)
    }

    // Acquire write ownership before reading authority. A deferred WAL transaction can read
    // the old output, lose a race to another publication and fail its read→write upgrade.
    fn publication_transaction<'a>(
        conn: &'a mut Connection,
        r: &SessionRef,
        from: usize,
        prov: &Provenance,
    ) -> Result<rusqlite::Transaction<'a>> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if from > 0 && attributed_in(&tx, r) != Some(authority(r, prov)) {
            anyhow::bail!("event prefix belongs to another or unknown output authority");
        }
        Ok(tx)
    }

    fn record_from_in(
        conn: &mut Connection,
        r: &SessionRef,
        from: usize,
        events: impl IntoIterator<Item = Result<Event>>,
        prov: &Provenance,
        source: &crate::source::Baseline,
    ) -> Result<()> {
        let tx = publication_transaction(conn, r, from, prov)?;
        let harness = r.harness.as_str();
        tx.execute(
            "INSERT OR IGNORE INTO event_sessions(harness, session_id) VALUES(?1,?2)",
            params![harness, r.id],
        )?;
        // Stamp provenance on every record (idempotent re-ingest keeps it current); top-level
        // sessions write NULLs, which leaves the existing top-level rows exactly as before.
        tx.execute(
            "UPDATE event_sessions SET parent_id=?3, agent_id=?4, workflow=?5 WHERE harness=?1 AND session_id=?2",
            params![harness, r.id, prov.parent_id, prov.agent_id, prov.workflow],
        )?;
        let session = tx.query_row(
            "SELECT id FROM event_sessions WHERE harness=?1 AND session_id=?2",
            params![harness, r.id],
            |row| row.get::<_, i64>(0),
        )?;
        tx.execute(
            "DELETE FROM events WHERE session=?1 AND msg_idx>=?2",
            params![session, from as i64],
        )?;
        {
            let mut intern = tx.prepare_cached("INSERT OR IGNORE INTO event_targets(target) VALUES(?1)")?;
            let mut intern_id = tx.prepare_cached("SELECT id FROM event_targets WHERE target=?1")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO events(session,msg_idx,ts,kind,tool,target_id,detail)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
            )?;
            let mut targets: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
            let mut previous = None;
            for e in events {
                let e = e?;
                if e.msg_idx < from || previous.is_some_and(|idx| e.msg_idx < idx) {
                    anyhow::bail!("event batch is not ordered from the resume index");
                }
                previous = Some(e.msg_idx);
                let target_id: Option<i64> = match e.target.as_deref() {
                    None => None,
                    Some(t) => match targets.get(t) {
                        Some(&id) => Some(id),
                        None => {
                            intern.execute(params![t])?;
                            let id = intern_id.query_row(params![t], |row| row.get(0))?;
                            // Repeated targets benefit from interning without retaining every
                            // distinct file in a session-sized map.
                            if targets.len() >= 256 {
                                targets.clear();
                            }
                            targets.insert(t.to_string(), id);
                            Some(id)
                        }
                    },
                };
                insert.execute(params![
                    session,
                    e.msg_idx as i64,
                    e.ts,
                    e.kind,
                    e.tool,
                    target_id,
                    e.detail,
                ])?;
            }
        }
        // One session's rows have one source copy: another path's baseline no longer describes them.
        tx.execute(
            "DELETE FROM source_dependencies WHERE consumer='events' AND harness=?1 AND id=?2",
            params![harness, r.id],
        )?;
        crate::source::publish_in(
            &tx,
            "events",
            r,
            source,
            &authority(r, prov),
            &mut crate::source::Probe::default(),
        )
        .context("event source baseline publication failed")?;
        tx.commit()?;
        Ok(())
    }

    /// (Re)ingest events for one session: stream it (lazily — large content stays on disk) through
    /// a [`SpoolingSink`] and persist the rows. Returns the number of events extracted. The
    /// baseline is captured **before** the streamed pass, so anything appended while we were
    /// reading still shows as stale next time.
    pub fn ingest_ref(r: &SessionRef) -> Result<usize> {
        ingest_subagent(r, &Provenance::default())
    }

    /// (Re)ingest events for one sub-agent transcript, stamping `prov`enance on its catalog row so
    /// the rows are attributable to the orchestrator that spawned it. Same streaming discipline as
    /// [`ingest_ref`] (lazy parse → large content stays on disk); returns the events extracted.
    pub fn ingest_subagent(r: &SessionRef, prov: &Provenance) -> Result<usize> {
        let adapter =
            crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
        let source = crate::source::Probe::default().snapshot(r);
        let mut sink = SpoolingSink::new(r.cwd.clone()).with_source(r.path.clone());
        adapter.try_stream(r, &crate::stream::Demand::LEAN.with_spans(), &mut sink)?;
        let source = source.with_reads(sink.inner().dependencies());
        let events = sink.into_spool()?;
        let len = events.len();
        record_spooled_with_source(r, 0, events, &source, prov)?;
        Ok(len)
    }

    /// (Re)ingest events for every discovered session, skipping unchanged ones (by their
    /// published baseline) unless `force`. The standalone entry point for users who don't run the tantivy
    /// indexer (which otherwise ingests events as a ride-along). Returns sessions (re)ingested.
    ///
    /// With `subagents`, also fold each top-level Claude session's whole sub-agent **forest**
    /// (`crate::subagent_tree_of`) into the catalog: every Task/Workflow agent's transcript is
    /// streamed through the same `EventSink` and stored with provenance (parent / workflow / agent),
    /// so `cv touched`/`cv events` see what the agents did — not just the orchestrator. Off by
    /// default (the forest can be hundreds of transcripts / ~900 MB).
    pub fn ingest_all(options: IngestOptions) -> Result<usize> {
        let IngestOptions { force, subagents } = options;
        let sources = collect_sources(crate::discover_all(), subagents)?;
        let sync = SyncTable::load();
        let mut probe = crate::source::Probe::default();
        let mut n = 0;
        for (r, provenance) in sources {
            let source = probe.snapshot(&r);
            if !force && !sync.needs_ingest(&r, &source, &provenance, &mut probe) {
                continue;
            }
            match ingest_subagent(&r, &provenance) {
                Ok(_) => n += 1,
                Err(error) => eprintln!("cv: event ingest failed for {} ({}): {error:#}", r.id, r.harness),
            }
        }
        Ok(n)
    }

    pub(super) fn collect_sources(refs: Vec<SessionRef>, subagents: bool) -> Result<Vec<(SessionRef, Provenance)>> {
        let mut sources: Vec<_> = refs.into_iter().map(|r| (r, Provenance::default())).collect();
        if subagents {
            let folded: Vec<_> = sources
                .iter()
                .map(|(parent, _)| {
                    Ok(crate::subagent_tree_of(parent)?
                        .into_iter()
                        .map(|sub| {
                            let provenance = Provenance {
                                parent_id: Some(parent.id.clone()),
                                agent_id: Some(sub.agent_id().to_owned()),
                                workflow: sub.workflow.clone(),
                            };
                            (sub.session, provenance)
                        })
                        .collect::<Vec<_>>())
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            sources.extend(folded);
        }
        Ok(preferred_sources(sources))
    }

    /// Events have one output per harness/id, independent of how many source copies discovery
    /// returns. Resolve that output once before publication; direct attribution wins equal copies.
    pub(super) fn preferred_sources(
        sources: impl IntoIterator<Item = (SessionRef, Provenance)>,
    ) -> Vec<(SessionRef, Provenance)> {
        let mut copies: std::collections::HashMap<_, (SessionRef, Provenance)> = std::collections::HashMap::new();
        for (r, provenance) in sources {
            let key = (r.harness, r.id.clone());
            match copies.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((r, provenance));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let (old, old_provenance) = entry.get();
                    let order = (!r.path.exists())
                        .cmp(&(!old.path.exists()))
                        .then_with(|| r.preferred_copy_cmp(old))
                        .then_with(|| {
                            (
                                !provenance.is_empty(),
                                &provenance.parent_id,
                                &provenance.agent_id,
                                &provenance.workflow,
                            )
                                .cmp(&(
                                    !old_provenance.is_empty(),
                                    &old_provenance.parent_id,
                                    &old_provenance.agent_id,
                                    &old_provenance.workflow,
                                ))
                        });
                    if order.is_lt() {
                        entry.insert((r, provenance));
                    }
                }
            }
        }
        let mut refs: Vec<_> = copies.into_values().collect();
        refs.sort_by(|(a, _), (b, _)| a.harness.as_str().cmp(b.harness.as_str()).then_with(|| a.id.cmp(&b.id)));
        refs
    }

    /// All persisted events of one session in transcript order, optionally filtered by kind.
    /// Empty for an un-ingested session; an event catalog that cannot open or answer is an error.
    pub fn events_for(harness: &str, session_id: &str, kind: Option<&str>) -> Result<Vec<EventRow>> {
        let conn = open().context("the event catalog cannot be opened")?;
        let mut stmt = conn.prepare(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, e.kind, e.tool, t.target, e.detail,
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN event_targets t ON t.id = e.target_id
             WHERE es.harness=?1 AND es.session_id=?2 AND (?3 IS NULL OR e.kind=?3)
             ORDER BY e.msg_idx, e.kind",
        )?;
        let rows = stmt.query_map(params![harness, session_id, kind], |row| {
            Ok(EventRow {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                msg_idx: row.get(2)?,
                ts: row.get(3)?,
                kind: row.get(4)?,
                tool: row.get(5)?,
                target: row.get(6)?,
                detail: row.get(7)?,
                parent_id: row.get(8)?,
                agent_id: row.get(9)?,
                workflow: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// THE query: sessions whose file events touched `path`, newest activity first.
    ///
    /// `path` is lexically absolutized against the current directory, and *also* matched by
    /// suffix (`…/<path>`), so `cv touched src/ir.rs` finds `/repo/src/ir.rs` from anywhere.
    /// `edits_only` keeps only sessions with at least one `file_edit`. Titles come from a join
    /// against the `sessions` catalog. Empty on a cold catalog; a catalog error is an error.
    ///
    /// The leading-`%` LIKE can never use an index; it scans `event_targets` (the
    /// distinct targets, ~⅓ of the event rows) and joins matches back through `idx_events_target`
    /// instead of scanning every event row.
    pub fn sessions_touching(path: &str, edits_only: bool) -> Result<Vec<TouchedSession>> {
        let abs = absolutize(path, crate::env::current_dir().as_deref());
        // Escape LIKE metacharacters in the user's path, then match any target ending in /<path>.
        let escaped = path
            .trim_start_matches("./")
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let suffix = if escaped.starts_with('/') {
            format!("%{escaped}")
        } else {
            format!("%/{escaped}")
        };
        touching(&abs, Some(&suffix), edits_only)
    }

    /// Sessions that EDITED exactly `abs` (an absolute path as the event index stores it): the
    /// indexed point lookup behind the `cv hook edit` guard, which must answer in milliseconds on
    /// a store of millions of events. No suffix match, so no scan of `event_targets` — the
    /// `target` UNIQUE index resolves the path and `idx_events_target` the rows. Newest first.
    pub fn sessions_editing(abs: &str) -> Result<Vec<TouchedSession>> {
        touching(abs, None, true)
    }

    fn touching(abs: &str, suffix: Option<&str>, edits_only: bool) -> Result<Vec<TouchedSession>> {
        let conn = open().context("the event catalog cannot be opened")?;
        let having = if edits_only { "HAVING edits > 0" } else { "" };
        // With a suffix: `target = ?1 OR target LIKE ?2` — the IN-subquery (not a join) pins the
        // plan to one scan over the distinct targets, then matching event rows through
        // idx_events_target, never a scan of all events. Without one: a point lookup.
        let target_where = match suffix {
            Some(_) => "target = ?1 OR target LIKE ?2 ESCAPE '\\'",
            None => "target = ?1",
        };
        let sql = format!(
            "SELECT es.harness, es.session_id,
                    SUM(e.kind='file_edit') AS edits,
                    SUM(e.kind='file_read') AS reads,
                    MAX(e.ts) AS last_ts,
                    MAX(s.title) AS title,
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN sessions s ON s.harness = es.harness AND s.id = es.session_id
             WHERE e.kind IN ('file_edit','file_read')
               AND e.target_id IN (SELECT id FROM event_targets WHERE {target_where})
             GROUP BY es.harness, es.session_id
             {having}
             ORDER BY last_ts DESC, es.session_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        // rusqlite refuses a parameter the SQL never references, so bind exactly what it names.
        let map = |row: &rusqlite::Row| {
            Ok(TouchedSession {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                edits: row.get(2)?,
                reads: row.get(3)?,
                last_ts: row.get(4)?,
                title: row.get(5)?,
                parent_id: row.get(6)?,
                agent_id: row.get(7)?,
                workflow: row.get(8)?,
            })
        };
        let rows = match suffix {
            Some(sfx) => stmt.query_map(params![abs, sfx], map)?,
            None => stmt.query_map(params![abs], map)?,
        };
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every `file_edit` event on one file, with session metadata riding along, oldest first.
    ///
    /// Matched two ways, mirroring [`sessions_touching`]: exactly by `abs` (the file's absolute
    /// path in the *current* checkout) and by repo-relative suffix `rel` (target equals `rel` or
    /// ends with `/<rel>`), so edits recorded by a session whose checkout lived at a different
    /// path still surface. The LEFT JOIN is grouped per event row because a session may be
    /// cataloged at more than one path. Empty on a cold catalog; a catalog error is an error.
    pub fn edits_touching_file(abs: &str, rel: &str) -> Result<Vec<EditEvent>> {
        let conn = open().context("the event catalog cannot be opened")?;
        let escaped = rel.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let suffix = format!("%/{escaped}");
        // IN-subquery for the same plan-pinning reason as [`sessions_touching`]; the extra join
        // back to event_targets only resolves the matched rows' target strings by rowid.
        let mut stmt = conn.prepare(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, t.target,
                    MAX(s.title), MAX(s.cwd), MAX(s.created_at), MAX(s.updated_at), MAX(es.parent_id)
             FROM events e
             JOIN event_targets t ON t.id = e.target_id
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN sessions s ON s.harness = es.harness AND s.id = es.session_id
             WHERE e.kind = 'file_edit'
               AND e.target_id IN (SELECT id FROM event_targets
                                   WHERE target = ?1 OR target = ?2 OR target LIKE ?3 ESCAPE '\\')
             GROUP BY es.harness, es.session_id, e.msg_idx, e.ts, t.target
             ORDER BY e.ts, e.msg_idx",
        )?;
        let rows = stmt.query_map(params![abs, rel, suffix], |row| {
            Ok(EditEvent {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                msg_idx: row.get(2)?,
                ts: row.get(3)?,
                target: row.get(4)?,
                title: row.get(5)?,
                cwd: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
                parent_id: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether *any* events have ever been ingested — lets callers distinguish "no sessions
    /// touched this file" from "nobody ran `cv index` yet" and hint accordingly.
    pub fn catalog_has_events() -> bool {
        let Some(conn) = open() else { return false };
        conn.query_row("SELECT 1 FROM events LIMIT 1", [], |_| Ok(())).is_ok()
    }
    #[cfg(test)]
    mod authority_lock_tests {
        use super::*;
        #[test]
        fn publication_owns_writer_before_authority_read_and_rejects_intervening_change() {
            let world = crate::test_world("event-authority-lock");
            let path = world.base.join("source.jsonl");
            std::fs::write(&path, "source").unwrap();
            let r = SessionRef::new("lock", crate::Harness::Claude, path);
            let source = crate::source::Probe::default()
                .snapshot(&r)
                .with_reads(&Default::default());
            record_spooled_with_source(&r, 0, Default::default(), &source, &Provenance::default()).unwrap();
            let mut first = open().unwrap();
            let second = open().unwrap();
            assert_eq!(
                first
                    .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                    .unwrap(),
                "wal"
            );
            second.busy_timeout(std::time::Duration::ZERO).unwrap();
            // This is the former production sequence: the read admits a WAL snapshot, then
            // another connection commits before the initial INSERT. Waiting cannot fix it.
            let deferred = first.transaction().unwrap();
            assert!(attributed_in(&deferred, &r) == Some(authority(&r, &Provenance::default())));
            // Another publication re-attributes the output: its baseline now names that workflow.
            let provenance = Provenance {
                workflow: Some("intervening".into()),
                ..Default::default()
            };
            crate::source::publish_in(
                &second,
                "events",
                &r,
                &source,
                &authority(&r, &provenance),
                &mut Default::default(),
            )
            .unwrap();
            let error = deferred
                .execute(
                    "INSERT OR IGNORE INTO event_sessions(harness,session_id) VALUES('claude','lock')",
                    [],
                )
                .unwrap_err();
            assert!(
                matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_BUSY_SNAPSHOT)
            );
            deferred.rollback().unwrap();
            let before: Option<String> = second
                .query_row(
                    "SELECT workflow FROM event_sessions WHERE session_id='lock'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(publication_transaction(&mut first, &r, 1, &Provenance::default()).is_err());
            assert_eq!(
                second
                    .query_row(
                        "SELECT workflow FROM event_sessions WHERE session_id='lock'",
                        [],
                        |row| row.get::<_, Option<String>>(0)
                    )
                    .unwrap(),
                before
            );
            // Exercise the production transaction owner before its first output mutation.
            let immediate = publication_transaction(&mut first, &r, 1, &provenance).unwrap();
            let error = second
                .execute(
                    "UPDATE event_sessions SET workflow='racing' WHERE session_id='lock'",
                    [],
                )
                .unwrap_err();
            assert!(
                matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy)
            );
            immediate
                .execute(
                    "INSERT OR IGNORE INTO event_sessions(harness,session_id) VALUES('claude','lock')",
                    [],
                )
                .unwrap();
            immediate.commit().unwrap();
            assert_eq!(
                second
                    .query_row(
                        "SELECT workflow FROM event_sessions WHERE session_id='lock'",
                        [],
                        |row| row.get::<_, Option<String>>(0)
                    )
                    .unwrap(),
                before
            );
            let count: i64 = second
                .query_row(
                    "SELECT COUNT(*) FROM source_dependencies WHERE consumer='events' AND id='lock'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "authority rejection preserves the published baseline");
        }
    }
}

#[cfg(feature = "sqlite")]
pub use db::{
    catalog_has_events, edits_touching_file, events_for, ingest_all, ingest_ref, ingest_subagent, needs_ingest,
    record_spooled_with_source, sessions_editing, sessions_touching, SyncTable,
};

/// No-op fallbacks without sqlite: extraction still works (the sink is pure), persistence and
/// queries degrade to nothing — mirroring [`crate::catalog`].
#[cfg(feature = "sqlite")]
pub(crate) use db::{open as writer_connection, record_spooled_in};

#[cfg(not(feature = "sqlite"))]
mod db_stub {
    use super::*;
    use anyhow::Result;

    pub fn needs_ingest(_r: &SessionRef) -> bool {
        crate::env::frozen()
    }
    /// No-op twin of the sqlite [`SyncTable`]: nothing is ever stale.
    pub struct SyncTable;
    impl SyncTable {
        pub fn load() -> SyncTable {
            SyncTable
        }
        pub fn needs_ingest(
            &self,
            _r: &SessionRef,
            _source: &crate::source::Baseline,
            _provenance: &Provenance,
            _probe: &mut crate::source::Probe,
        ) -> bool {
            false
        }
        pub fn can_resume(&self, _r: &SessionRef, _provenance: &Provenance) -> bool {
            false
        }
    }
    pub fn record_spooled_with_source(
        _r: &SessionRef,
        _from: usize,
        _events: EventSpool,
        _source: &crate::source::Baseline,
        _prov: &Provenance,
    ) -> Result<()> {
        Ok(())
    }
    pub fn ingest_ref(_r: &SessionRef) -> Result<usize> {
        Ok(0)
    }
    pub fn ingest_subagent(_r: &SessionRef, _prov: &Provenance) -> Result<usize> {
        Ok(0)
    }
    pub fn ingest_all(_options: IngestOptions) -> Result<usize> {
        Ok(0)
    }
    pub fn events_for(_harness: &str, _session_id: &str, _kind: Option<&str>) -> Result<Vec<EventRow>> {
        Ok(Vec::new())
    }
    pub fn sessions_touching(_path: &str, _edits_only: bool) -> Result<Vec<TouchedSession>> {
        Ok(Vec::new())
    }
    pub fn sessions_editing(_abs: &str) -> Result<Vec<TouchedSession>> {
        Ok(Vec::new())
    }
    pub fn edits_touching_file(_abs: &str, _rel: &str) -> Result<Vec<EditEvent>> {
        Ok(Vec::new())
    }
    pub fn catalog_has_events() -> bool {
        false
    }
}
#[cfg(not(feature = "sqlite"))]
pub use db_stub::{
    catalog_has_events, edits_touching_file, events_for, ingest_all, ingest_ref, ingest_subagent, needs_ingest,
    record_spooled_with_source, sessions_editing, sessions_touching, SyncTable,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role};
    use serde_json::json;

    #[test]
    fn native_codex_patch_facts_deduplicate_by_exact_call_identity_across_checkpoints() {
        // Real Codex 0.163.0 records from the isolated hooks scenario; scrubbed, not authored.
        let nested = crate::harness::codex::parse_str(
            "nested",
            include_str!("../tests/fixtures/codex/code_mode_patch.jsonl"),
            true,
            None,
        )
        .unwrap();
        let direct = crate::harness::codex::parse_str(
            "direct",
            include_str!("../tests/fixtures/codex/direct_patch.jsonl"),
            true,
            None,
        )
        .unwrap();
        for session in [&nested, &direct] {
            assert_eq!(session.messages.len(), 3);
            let mut before = EventSink::new(None);
            before.observe_message(&session.messages[0]);
            let initial = before.events().iter().filter(|e| e.kind == "file_edit").count();
            let mut checkpoint = Checkpoint::new(42);
            before.checkpoint(&mut checkpoint);
            // Cross the actual serialized checkpoint boundary, not a clone of the extractor.
            let bytes = serde_json::to_vec(&checkpoint).unwrap();
            let checkpoint = serde_json::from_slice(&bytes).unwrap();
            let mut after = EventSink::resume(&checkpoint).unwrap();
            for message in &session.messages[1..] {
                after.observe_message(message);
            }
            let edits: Vec<_> = after.events().iter().filter(|e| e.kind == "file_edit").collect();
            assert_eq!(initial + edits.len(), 1);
            let histogram = crate::tools::ToolHistogram::from_messages(&session.messages, None).unwrap();
            assert_eq!(histogram.tools.values().map(|t| t.edits).sum::<usize>(), 1);
        }
        // Parallel completions must not mistake an unrelated outer call for the native child.
        let mut sink = EventSink::new(None);
        sink.observe_message(&direct.messages[0]);
        sink.observe_message(&nested.messages[0]);
        sink.observe_message(&nested.messages[1]);
        sink.observe_message(&direct.messages[1]);
        assert_eq!(sink.events().iter().filter(|e| e.kind == "file_edit").count(), 2);
        assert_eq!(sink.events().iter().filter(|e| e.call).count(), 2);
    }

    #[test]
    fn native_edits_reconcile_resolved_targets_and_command_workdir() {
        let fixture = include_str!("../tests/fixtures/codex/direct_patch.jsonl");
        for shell_cd in [false, true] {
            let mut session = crate::harness::codex::parse_str("direct", fixture, true, None).unwrap();
            if shell_cd {
                let Block::ToolUse { input, .. } = &mut session.messages[0].content[0] else {
                    panic!("fixture must start with the shell call");
                };
                *input = json!({"cmd": "cd /home/corpus/project && apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: notes.txt\n*** End Patch\nPATCH"}).into();
            }
            // The native completion knows an additional path not understood by inference.
            session.messages[1].extra["codex"]["item"]["changes"]["/home/corpus/project/extra.txt"] =
                json!({"type":"update", "unified_diff":"", "move_path":null});
            let mut before = EventSink::new(Some("/different/session".into()));
            before.observe_message(&session.messages[0]);
            let mut cp = Checkpoint::new(42);
            before.checkpoint(&mut cp);
            let cp = serde_json::from_slice(&serde_json::to_vec(&cp).unwrap()).unwrap();
            let mut after = EventSink::resume(&cp).unwrap();
            for message in &session.messages[1..] {
                after.observe_message(message);
            }
            let mut paths: Vec<_> = before
                .events()
                .iter()
                .chain(after.events())
                .filter(|e| e.kind == "file_edit")
                .map(|e| e.target.as_deref().unwrap())
                .collect();
            paths.sort();
            assert_eq!(
                paths,
                ["/home/corpus/project/extra.txt", "/home/corpus/project/notes.txt"]
            );
        }
    }

    #[test]
    fn inferred_call_identity_spills_exactly_and_defers_checkpoints_until_closed() {
        let _world = crate::test_world("events-spilled-calls");
        let mut sink = EventSink::new(None);
        for i in 0..3000 {
            sink.extractor
                .note_target(&format!("pending-{i}"), &format!("/repo/file-{i}.rs"))
                .unwrap();
        }
        assert!(!sink.wants_checkpoint(1));
        let mut checkpoint = Checkpoint::new(1);
        sink.checkpoint(&mut checkpoint);
        assert!(EventSink::resume(&checkpoint).is_none());
        assert!(sink.extractor.inferred_edits.contains("cpending-0").unwrap());
        assert!(sink.extractor.inferred_edits.contains("cpending-2999").unwrap());
        assert!(!sink.extractor.inferred_edits.contains("absent").unwrap());
        let mut session = crate::harness::codex::parse_str(
            "direct",
            include_str!("../tests/fixtures/codex/direct_patch.jsonl"),
            true,
            None,
        )
        .unwrap();
        let native = &mut session.messages[1];
        native.extra["codex"]["item"]["id"] = json!("pending-0");
        native.extra["codex"]["item"]["changes"] = json!({
            "/repo/file-0.rs": {"type":"update"}, "/repo/missed.rs": {"type":"update"}
        });
        sink.observe_message(native);
        assert_eq!(sink.events().len(), 1);
        assert_eq!(sink.events()[0].target.as_deref(), Some("/repo/missed.rs"));
        sink.extractor.complete("pending-0").unwrap();
        sink.extractor.complete("pending-0").unwrap();
        assert_eq!(sink.extractor.pending, 2999);
        for i in (1..3000).rev() {
            sink.extractor.complete(&format!("pending-{i}")).unwrap();
        }
        assert!(sink.wants_checkpoint(2));
        sink.checkpoint(&mut checkpoint);
        assert_eq!(EventSink::resume(&checkpoint).unwrap().extractor.pending, 0);
    }

    #[test]
    fn checkpoint_only_sink_tracks_positions_without_extracting_rows() {
        let mut sink = EventSink::new(Some("/work".into())).without_events();
        let message = || msg(vec![tool_use("Bash", json!({"command":"echo hi"}))]);
        sink.message(message());
        sink.message(message());
        assert!(sink.events().is_empty());
        let mut cp = Checkpoint::new(42);
        sink.checkpoint(&mut cp);
        let mut resumed = EventSink::resume(&cp).unwrap();
        resumed.message(message());
        assert_eq!(resumed.events().len(), 1);
        assert_eq!(resumed.events()[0].msg_idx, 2);
    }

    fn msg(blocks: Vec<Block>) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content = blocks;
        m
    }

    fn tool_use(name: &str, input: Value) -> Block {
        Block::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input: input.into(),
            namespace: None,
        }
    }

    #[test]
    fn source_tool_projection_preserves_precedence_and_checked_failure() {
        let path = std::env::temp_dir().join(format!("cv-event-input-{}", uuid::Uuid::new_v4()));
        let cases = [
            (
                "Bash",
                r#"{"command":null,"cmd":"masked","ignored":"large"}"#,
                vec![("tool", None)],
            ),
            (
                "Edit",
                r#"{"file_path":"","path":"masked","command":["",false,"echo","",null]}"#,
                vec![("command", Some(" echo ".to_string()))],
            ),
            (
                "apply_patch",
                r#"{"input":null,"patch":"*** Update File: first\n*** Move to: second\n"}"#,
                vec![
                    ("file_edit", Some("/w/first".to_string())),
                    ("file_edit", Some("/w/second".to_string())),
                ],
            ),
            (
                "Bash",
                r#"{"command":"masked","command":["apply_patch","*** Begin Patch\n*** Add File: late\n*** End Patch"]}"#,
                vec![
                    (
                        "command",
                        Some("apply_patch *** Begin Patch\n*** Add File: late\n*** End Patch".to_string()),
                    ),
                    ("file_edit", Some("/w/late".to_string())),
                ],
            ),
        ];
        for (name, raw, expected) in cases {
            std::fs::write(&path, raw).unwrap();
            let source = Json::source(&path, 0, raw.len() as u64).unwrap();
            let owned: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                source
                    .with_view(|view| classify_tool_view(name, view, Some(Path::new("/w")), true))
                    .unwrap(),
                expected
            );
            assert_eq!(
                classify_tool_view(name, JsonView::from(&owned), Some(Path::new("/w")), true).unwrap(),
                expected
            );
            let mut message = msg(vec![
                tool_use("Read", json!({"path":"before"})),
                Block::ToolUse {
                    id: "lazy".into(),
                    name: name.into(),
                    namespace: None,
                    input: source,
                },
            ]);
            std::fs::remove_file(&path).unwrap();
            let mut rows = vec![];
            assert!(EventExtractor::default()
                .extract(&message, 0, None, None, None, &mut rows)
                .is_err());
            assert!(rows.is_empty(), "the first valid call must not partially publish");
            let mut sink = EventSink::new(None);
            assert_eq!(sink.message(message.clone()), Flow::Stop);
            assert!(sink.take_error().is_some());
            message.content.clear();
            assert_eq!(sink.message(message), Flow::Stop);
            assert!(!sink.wants_checkpoint(0));
            assert!(
                sink.into_events().is_err(),
                "draining error cannot authorize partial publication"
            );
        }
    }

    #[test]
    fn source_giant_command_and_patch_match_owned_rows() {
        let path = std::env::temp_dir().join(format!("cv-event-giant-{}", uuid::Uuid::new_v4()));
        for name in ["Bash", "apply_patch"] {
            let text = format!(
                "apply_patch *** Begin Patch\n+{}\n*** Add File: late.rs\n*** End Patch",
                "λ\n".repeat(300_000)
            );
            let value = if name == "Bash" {
                json!({"command":text,"irrelevant":"x".repeat(1_000_000)})
            } else {
                json!(text)
            };
            let raw = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &raw).unwrap();
            let source = Json::source(&path, 0, raw.len() as u64).unwrap();
            let expected = classify_tool_view(name, JsonView::from(&value), None, true).unwrap();
            assert_eq!(
                source
                    .with_view(|view| classify_tool_view(name, view, None, true))
                    .unwrap(),
                expected
            );
            assert!(expected
                .iter()
                .any(|(kind, path)| *kind == "file_edit" && path.as_deref() == Some("late.rs")));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn claude_style_tools_classify_and_resolve_relative_paths() {
        let cwd = Path::new("/repo");
        let m = msg(vec![
            tool_use(
                "Edit",
                json!({"file_path": "src/ir.rs", "old_string": "a", "new_string": "b"}),
            ),
            tool_use("Read", json!({"file_path": "/etc/hosts"})),
            tool_use("Bash", json!({"command": "cargo test -p cv-core"})),
            tool_use("Glob", json!({"pattern": "**/*.rs", "path": "crates"})),
        ]);
        let ev = extract(&m, 7, Some(cwd), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev.len(), 4);
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("file_edit", Some("/repo/src/ir.rs"))
        );
        assert_eq!(ev[0].msg_idx, 7);
        assert_eq!((ev[1].kind, ev[1].target.as_deref()), ("file_read", Some("/etc/hosts")));
        assert_eq!(
            (ev[2].kind, ev[2].target.as_deref()),
            ("command", Some("cargo test -p cv-core"))
        );
        assert_eq!(
            (ev[3].kind, ev[3].target.as_deref()),
            ("file_read", Some("/repo/crates"))
        );
        assert_eq!(ev[0].tool.as_deref(), Some("Edit"));
    }

    #[test]
    fn codex_style_shell_and_local_shell_become_commands() {
        // codex `shell`: argv array.
        let m = msg(vec![tool_use("shell", json!({"command": ["bash", "-lc", "ls -la"]}))]);
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("command", Some("bash -lc ls -la"))
        );

        // codex `local_shell`: nested action.command.
        let m = msg(vec![tool_use(
            "local_shell",
            json!({"action": {"type": "exec", "command": ["echo", "hi"]}, "status": "completed"}),
        )]);
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("command", Some("echo hi")));
    }

    #[test]
    fn apply_patch_body_yields_per_file_edits() {
        let patch = "*** Begin Patch\n\
                     *** Update File: crates/cv-core/src/ir.rs\n\
                     @@\n-old\n+new\n\
                     *** Add File: docs/notes.md\n\
                     +hello\n\
                     *** End Patch";
        // Plain-string input (codex custom_tool_call).
        let m = msg(vec![tool_use("apply_patch", json!(patch))]);
        let ev = extract(&m, 0, Some(Path::new("/repo")), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(
            (ev[0].kind, ev[0].target.as_deref()),
            ("file_edit", Some("/repo/crates/cv-core/src/ir.rs"))
        );
        assert_eq!(
            (ev[1].kind, ev[1].target.as_deref()),
            ("file_edit", Some("/repo/docs/notes.md"))
        );

        // The same patch routed through the shell tool: command row + per-file edits.
        let m = msg(vec![tool_use("shell", json!({"command": ["apply_patch", patch]}))]);
        let ev = extract(&m, 0, Some(Path::new("/repo")), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev[0].kind, "command");
        let edits: Vec<_> = ev.iter().filter(|e| e.kind == "file_edit").collect();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].target.as_deref(), Some("/repo/crates/cv-core/src/ir.rs"));
    }

    fn patch_targets(patch: &str) -> Vec<String> {
        crate::tools::classify::patch_rows(crate::json::JsonView::from(&json!(patch))).unwrap()
    }

    #[test]
    fn codex_code_mode_exec_extracts_commands_patches_and_reads() {
        let script = r#"
const patch = [
  "*** Begin Patch",
  "*** Update File: src/main.rs",
  "*** Add File: src/new.rs",
  "*** End Patch",
].join("\n");
await tools.apply_patch(patch);
await tools.exec_command({ cmd: "cargo test --lib", workdir: "/repo" });
await tools.view_image({ path: "docs/arch.png" });
"#;
        let m = msg(vec![tool_use("exec", json!(script))]);
        let ev = extract(&m, 3, Some(Path::new("/repo")), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev.len(), 4);
        assert!(ev[0].call);
        assert!(!ev[1].call && !ev[2].call && !ev[3].call);
        assert_eq!(
            ev.iter()
                .map(|e| (e.kind, e.tool.as_deref().unwrap(), e.target.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                ("file_edit", "exec", "/repo/src/main.rs"),
                ("file_edit", "exec", "/repo/src/new.rs"),
                ("command", "exec", "cargo test --lib"),
                ("file_read", "exec", "/repo/docs/arch.png"),
            ]
        );
    }

    #[test]
    fn unified_diff_headers_need_minus_lines_to_count() {
        // `+++` headers count only alongside `---` ones…
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(patch_targets(diff), vec!["src/lib.rs".to_string()]);
        // …so an *added line* starting with `++ ` can't fake a target.
        let not_a_diff = "*** Begin Patch\n*** Update File: real.rs\n++++ fake.rs\n*** End Patch";
        assert_eq!(patch_targets(not_a_diff), vec!["real.rs".to_string()]);
        assert!(patch_targets("+++ orphan.rs\n").is_empty());
    }

    #[test]
    fn ambiguous_file_tool_and_unknown_tool_degrade_gracefully() {
        // File key but a name that hints neither read nor write → 'tool' with the path target.
        let m = msg(vec![tool_use("Mystery", json!({"path": "x.txt"}))]);
        let ev = extract(&m, 0, Some(Path::new("/w")), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("tool", Some("/w/x.txt")));
        // No file key, no command → bare 'tool' row.
        let m = msg(vec![tool_use("web_search", json!({"query": "rust"}))]);
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("tool", None));
    }

    #[test]
    fn gemini_style_names_classify() {
        let m = msg(vec![
            tool_use("write_file", json!({"file_path": "out.txt", "content": "hi"})),
            tool_use("read_file", json!({"absolute_path": "/g/in.txt"})),
            tool_use("run_shell_command", json!({"command": "make build"})),
            tool_use(
                "replace",
                json!({"file_path": "/g/edit.txt", "old_string": "a", "new_string": "b"}),
            ),
        ]);
        let ev = extract(&m, 0, Some(Path::new("/g")), &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!((ev[0].kind, ev[0].target.as_deref()), ("file_edit", Some("/g/out.txt")));
        assert_eq!((ev[1].kind, ev[1].target.as_deref()), ("file_read", Some("/g/in.txt")));
        assert_eq!((ev[2].kind, ev[2].target.as_deref()), ("command", Some("make build")));
        assert_eq!(
            (ev[3].kind, ev[3].target.as_deref()),
            ("file_edit", Some("/g/edit.txt"))
        );
    }

    #[test]
    fn error_results_carry_their_whole_text_lazy_or_inline() {
        // A failed dump past the inline limit, lazy on disk: its detail is read whole.
        let dump = format!("  {}tail of the failure\n", "boom\n".repeat(20_000));
        let path = std::env::temp_dir().join(format!("cv-event-detail-{}", uuid::Uuid::new_v4()));
        let encoded = serde_json::to_string(&dump).unwrap();
        std::fs::write(&path, &encoded).unwrap();
        let mut m = Message::new(Role::Tool);
        m.content = vec![
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "command failed: no such file".into(),
                is_error: true,
                tool_name: Some("Bash".into()),
                status: None,
                details: None,
            },
            Block::ToolResult {
                tool_use_id: "t2".into(),
                content: crate::lazy::Text::Span(crate::lazy::Span {
                    source: Some(path.clone()),
                    offset: 1,
                    len: encoded.len() as u64 - 2,
                    escaped: true,
                }),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            },
            // Non-error results emit nothing.
            Block::ToolResult {
                tool_use_id: "t3".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ];
        let ev = extract(&m, 3, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "error");
        assert_eq!(ev[0].detail.as_deref(), Some("command failed: no such file"));
        assert_eq!(ev[0].tool.as_deref(), Some("Bash"));
        assert_eq!(ev[1].kind, "error");
        assert_eq!(ev[1].detail.as_deref(), Some(dump.trim()));
        // A source that is gone fails the extraction instead of dropping the detail.
        std::fs::remove_file(&path).unwrap();
        assert!(extract(&m, 3, None, &crate::lazy::Resolver::new(None)).is_err());
    }

    #[test]
    fn long_commands_and_details_are_whole() {
        let long_cmd = format!("cat <<'EOF'\n{}\nEOF", "x".repeat(100_000));
        let m = msg(vec![tool_use("Bash", json!({"command": long_cmd}))]);
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev[0].target.as_deref(), Some(long_cmd.as_str()));

        let mut m = Message::new(Role::Tool);
        m.content = vec![Block::ToolResult {
            tool_use_id: "t".into(),
            content: "e".repeat(1000).into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        }];
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev[0].detail, Some("e".repeat(1000)));
    }

    #[test]
    fn a_caller_keeping_no_text_assembles_no_command() {
        // Query facts and `cv tools` pass no resolver: the command row and its patched files
        // still land, its text does not.
        let command = format!(
            "apply_patch *** Begin Patch\n*** Add File: a.rs\n{}*** End Patch",
            "+x\n".repeat(10_000)
        );
        let m = msg(vec![tool_use("Bash", json!({ "command": command }))]);
        let mut rows = vec![];
        EventExtractor::default()
            .extract(&m, 0, Some(Path::new("/w")), None, None, &mut rows)
            .unwrap();
        let rows: Vec<_> = rows.iter().map(|e| (e.kind, e.target.as_deref())).collect();
        assert_eq!(rows, [("command", None), ("file_edit", Some("/w/a.rs"))]);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn published_event_authority_tracks_copy_provenance_and_cwd_atomically() {
        let world = crate::test_world("events-authority");
        let fixture = |name: &str, target: &str| {
            let path = world.base.join(format!("{name}.jsonl"));
            let line = serde_json::json!({"type":"assistant","uuid":"a","sessionId":"shared",
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Read","input":{"file_path":target}}]}});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            let mut r = SessionRef::new("shared", crate::Harness::Claude, path);
            r.cwd = Some("/first".into());
            r
        };
        let a = fixture("copy-a", "a.rs");
        let b = fixture("copy-b", "b.rs");
        let direct = Provenance::default();
        let folded = Provenance {
            parent_id: Some("parent".into()),
            agent_id: Some("agent".into()),
            workflow: Some("workflow".into()),
        };
        let stale = |r: &SessionRef, prov: &Provenance| {
            let mut probe = crate::source::Probe::default();
            let source = probe.snapshot(r);
            SyncTable::load().needs_ingest(r, &source, prov, &mut probe)
        };
        let rows = || format!("{:?}", events_for("claude", "shared", None).unwrap());
        ingest_ref(&a).unwrap();
        assert!(!stale(&a, &direct));
        let planned = SyncTable::load();
        assert!(planned.can_resume(&a, &direct));
        ingest_ref(&b).unwrap();
        assert!(stale(&a, &direct));
        assert!(!stale(&b, &direct));
        assert!(!needs_ingest(&b));
        assert!(!SyncTable::load().can_resume(&a, &direct));
        assert_eq!(
            events_for("claude", "shared", None).unwrap()[0].target.as_deref(),
            Some("/first/b.rs")
        );
        let before = rows();
        let source = crate::source::Probe::default().snapshot(&a);
        let empty = || SpoolingSink::new(None).into_spool().unwrap();
        assert!(
            record_spooled_with_source(&a, 1, empty(), &source, &direct).is_err(),
            "publication rechecks authority after planning"
        );
        assert_eq!(rows(), before);
        assert!(
            !stale(&b, &direct),
            "failed prefix publication leaves stamps and output together"
        );
        ingest_subagent(&b, &folded).unwrap();
        assert!(stale(&b, &direct));
        assert!(needs_ingest(&b));
        assert!(!stale(&b, &folded));
        assert_eq!(
            events_for("claude", "shared", None).unwrap()[0].parent_id.as_deref(),
            Some("parent")
        );
        let source = crate::source::Probe::default().snapshot(&b);
        assert!(record_spooled_with_source(&b, 1, empty(), &source, &direct).is_err());
        ingest_ref(&b).unwrap();
        assert!(!stale(&b, &direct));
        assert!(stale(&b, &folded));
        assert!(events_for("claude", "shared", None).unwrap()[0].parent_id.is_none());
        let mut changed = b.clone();
        changed.cwd = Some("/second".into());
        assert!(stale(&changed, &direct));
        assert!(!SyncTable::load().can_resume(&changed, &direct));
        ingest_ref(&changed).unwrap();
        assert_eq!(
            events_for("claude", "shared", None).unwrap()[0].target.as_deref(),
            Some("/second/b.rs")
        );
        assert!(!stale(&changed, &direct));
        let conn = crate::catalog::open_db().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM source_dependencies WHERE consumer='events' AND id='shared'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "one session has one published source copy"
        );
        let before = rows();
        let source = crate::source::Probe::default().snapshot(&changed);
        record_spooled_with_source(&changed, 1, empty(), &source, &direct).unwrap();
        assert_eq!(rows(), before, "same-authority prefix retention remains supported");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn event_source_selection_is_newest_qualified_and_direct_on_identical_copy() {
        let world = crate::test_world("events-copy-selection");
        let old_path = world.base.join("older.jsonl");
        let new_path = world.base.join("newer.jsonl");
        std::fs::write(&old_path, []).unwrap();
        std::fs::write(&new_path, []).unwrap();
        let mut older = SessionRef::new("same", crate::Harness::Claude, &old_path);
        older.updated_at = chrono::DateTime::from_timestamp(1, 0);
        let mut newer = older.clone();
        newer.path = new_path.clone();
        newer.updated_at = chrono::DateTime::from_timestamp(2, 0);
        let mut other = older.clone();
        other.harness = crate::Harness::Codex;
        let folded = Provenance {
            parent_id: Some("parent".into()),
            ..Default::default()
        };
        let inputs = vec![
            (older, Provenance::default()),
            (newer.clone(), folded),
            (other, Provenance::default()),
            (newer.clone(), Provenance::default()),
        ];
        let selected = db::preferred_sources(inputs.clone());
        let reverse = db::preferred_sources(inputs.into_iter().rev());
        assert_eq!(format!("{selected:?}"), format!("{reverse:?}"));
        assert_eq!(selected.len(), 2);
        let (r, provenance) = selected
            .iter()
            .find(|(r, _)| r.harness == crate::Harness::Claude)
            .unwrap();
        assert_eq!(r.path, newer.path);
        assert!(provenance.is_empty());
        std::fs::remove_file(&new_path).unwrap();
        let older = SessionRef {
            path: old_path.clone(),
            updated_at: chrono::DateTime::from_timestamp(1, 0),
            ..newer.clone()
        };
        let select = || {
            db::preferred_sources(vec![
                (newer.clone(), Provenance::default()),
                (older.clone(), Provenance::default()),
            ])
        };
        assert_eq!(
            select()[0].0.path,
            old_path,
            "live older copy beats disappeared preferred copy"
        );
        std::fs::remove_file(&old_path).unwrap();
        assert_eq!(
            select()[0].0.path,
            new_path,
            "all-missing preserves newest error source"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn event_forest_selection_keeps_unique_children_of_nonpreferred_parent_copies() {
        let world = crate::test_world("events-forest-selection");
        let mut parents = Vec::new();
        for (name, timestamp) in [("older", 1), ("newer", 2)] {
            let dir = world.base.join(name);
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("parent.jsonl");
            std::fs::write(&path, []).unwrap();
            let mut parent = SessionRef::new("parent", crate::Harness::Claude, &path);
            parent.updated_at = chrono::DateTime::from_timestamp(timestamp, 0);
            let agents = dir.join("parent/subagents");
            std::fs::create_dir_all(&agents).unwrap();
            let line = serde_json::json!({"type":"assistant", "uuid":name, "sessionId":name,
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Read","input":{"file_path":format!("/{name}.rs")}}]}});
            std::fs::write(agents.join(format!("agent-{name}.jsonl")), format!("{line}\n")).unwrap();
            parents.push(parent);
        }
        let selected = db::collect_sources(parents.clone(), true).unwrap();
        assert_eq!(selected.len(), 3, "one preferred parent plus both disjoint children");
        assert_eq!(
            selected.iter().find(|(r, _)| r.id == "parent").unwrap().0.path,
            parents[1].path
        );
        for (r, provenance) in selected.iter().filter(|(_, provenance)| !provenance.is_empty()) {
            ingest_subagent(r, provenance).unwrap();
            let rows = events_for(r.harness.as_str(), &r.id, None).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].parent_id.as_deref(), Some("parent"));
        }
        assert_eq!(
            format!("{selected:?}"),
            format!(
                "{:?}",
                db::collect_sources(parents.into_iter().rev().collect(), true).unwrap()
            )
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn guarded_empty_events_reuse_requires_known_size_and_source_identity() {
        let world = crate::test_world("events-empty");
        let path = world.cv_home.join("empty.jsonl");
        std::fs::write(&path, []).unwrap();
        let r = SessionRef::new("empty", crate::Harness::Claude, &path);
        assert_eq!(ingest_ref(&r).unwrap(), 0);
        assert!(!needs_ingest(&r));
        let source = crate::source::Probe::default().snapshot(&r);
        let mut probe = crate::source::Probe::default();
        assert!(!SyncTable::load().needs_ingest(&r, &source, &Provenance::default(), &mut probe));
        let conn = crate::catalog::open_db().unwrap();
        conn.execute("DELETE FROM source_dependencies WHERE consumer='events'", [])
            .unwrap();
        assert!(needs_ingest(&r), "missing captured authority still misses");
        ingest_ref(&r).unwrap();
        // A real record of the previous writer version — the whole envelope cv wrote, with only
        // the version rewound — not a hand-cut byte offset.
        conn.execute(
            "UPDATE source_dependencies SET record=CAST(json_set(CAST(record AS TEXT),'$.version',?1) AS BLOB) WHERE consumer='events'",
            [crate::source::VERSION - 1],
        )
        .unwrap();
        assert!(needs_ingest(&r), "old source stamps cannot authorize empty reuse");
        ingest_ref(&r).unwrap();
        std::fs::write(&path, "\n").unwrap();
        assert!(needs_ingest(&r), "empty file growth is stale");
        ingest_ref(&r).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(needs_ingest(&r), "missing primary cannot match");
    }

    /// End-to-end against a real temp catalog db: stream a claude JSONL fixture through
    /// `ingest_ref`, then read events back with `events_for` and `sessions_touching`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn ingest_and_query_roundtrip_in_temp_catalog() {
        let world = crate::test_world("events-roundtrip");
        let home = &world.cv_home;

        let sdir = home.join("sessions");
        std::fs::create_dir_all(&sdir).unwrap();
        let path = sdir.join("evt-e2e.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u0", "sessionId": "evt-e2e",
                "message": {"role": "user", "content": "please fix the parser"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "evt-e2e",
                "timestamp": "2026-06-01T12:00:00Z",
                "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "src/parser.rs", "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test"}}
                ]}
            }),
            serde_json::json!({
                "type": "user", "uuid": "u2", "sessionId": "evt-e2e",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2",
                     "content": "error[E0308]: mismatched types", "is_error": true}
                ]}
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let r = SessionRef {
            id: "evt-e2e".into(),
            harness: crate::ir::Harness::Claude,
            path: path.clone(),
            cwd: Some("/repo".into()),
            title: Some("fix the parser".into()),
            created_at: None,
            updated_at: None,
            message_count: Some(2),
        };

        let n = ingest_ref(&r).unwrap();
        assert!(n >= 3, "expected at least edit+command+error events, got {n}");

        // Freshly ingested → no longer needs ingest at the same mtime; a re-ingest is idempotent.
        assert!(!needs_ingest(&r));
        ingest_ref(&r).unwrap();

        let rows = events_for("claude", "evt-e2e", None).unwrap();
        assert_eq!(rows.len(), n, "re-ingest must not duplicate rows");
        let edit = rows.iter().find(|e| e.kind == "file_edit").unwrap();
        assert_eq!(edit.target.as_deref(), Some("/repo/src/parser.rs"));
        assert_eq!(edit.tool.as_deref(), Some("Edit"));
        assert_eq!(edit.ts, Some(1780315200)); // 2026-06-01T12:00:00Z
        let cmd = rows.iter().find(|e| e.kind == "command").unwrap();
        assert_eq!(cmd.target.as_deref(), Some("cargo test"));
        let err = rows.iter().find(|e| e.kind == "error").unwrap();
        assert!(err.detail.as_deref().unwrap().contains("E0308"));

        // Kind filter narrows.
        assert_eq!(events_for("claude", "evt-e2e", Some("command")).unwrap().len(), 1);

        // A same-size rewrite must invalidate events, even for this normally append-only log.
        let content = std::fs::read(&path).unwrap();
        let edited = String::from_utf8(content.clone())
            .unwrap()
            .replace("parser.rs", "reader.rs");
        assert_eq!(edited.len(), content.len());
        let next_mtime = std::fs::metadata(&path).unwrap().modified().unwrap() + std::time::Duration::from_secs(1);
        std::fs::write(&path, edited).unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(next_mtime))
            .unwrap();
        assert!(needs_ingest(&r), "same-size rewrites must force an event re-ingest");
        ingest_ref(&r).unwrap();
        let edited_rows = events_for("claude", "evt-e2e", Some("file_edit")).unwrap();
        assert_eq!(edited_rows.len(), 1);
        assert_eq!(edited_rows[0].target.as_deref(), Some("/repo/src/reader.rs"));
        assert!(!needs_ingest(&r));

        // Restore the original edit for the remaining query checks below.
        std::fs::write(&path, &content).unwrap();
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({"type": "user", "uuid": "u3", "sessionId": "evt-e2e",
                                   "message": {"role": "user", "content": "and one more thing"}})
            )
            .unwrap();
        }
        assert!(needs_ingest(&r), "an append must read stale");
        ingest_ref(&r).unwrap();
        assert!(!needs_ingest(&r));

        // The touched query finds the session by absolute path and by suffix.
        let touched = sessions_touching("/repo/src/parser.rs", false).unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "evt-e2e");
        assert_eq!(touched[0].edits, 1);
        assert_eq!(touched[0].reads, 0);
        let by_suffix = sessions_touching("src/parser.rs", true).unwrap();
        assert_eq!(by_suffix.len(), 1, "suffix match + edits_only should still hit");
        // A file nobody touched stays quiet.
        assert!(sessions_touching("src/nonexistent.rs", false).unwrap().is_empty());
        // The edit guard's exact lookup: the absolute path hits, a suffix never does.
        let exact = sessions_editing("/repo/src/parser.rs").unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].session_id, "evt-e2e");
        assert!(sessions_editing("src/parser.rs").unwrap().is_empty());

        // blame's edit query: abs + repo-relative-suffix matching, session metadata joined in.
        crate::catalog::sync(std::slice::from_ref(&r));
        assert!(catalog_has_events());
        let edits = edits_touching_file("/repo/src/parser.rs", "src/parser.rs").unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].session_id, "evt-e2e");
        assert_eq!(edits[0].msg_idx, 1);
        assert_eq!(edits[0].ts, Some(1780315200));
        assert_eq!(edits[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(edits[0].title.as_deref(), Some("fix the parser"));
        // Suffix-only match from a *different* checkout path still hits.
        let edits = edits_touching_file("/elsewhere/src/parser.rs", "src/parser.rs").unwrap();
        assert_eq!(edits.len(), 1);
        assert!(edits_touching_file("/repo/src/other.rs", "src/other.rs")
            .unwrap()
            .is_empty());
        // A suffix must start at a path-component boundary, and a relative path equals only itself.
        assert!(edits_touching_file("/x/rser.rs", "rser.rs").unwrap().is_empty());
        assert!(edits_touching_file("/x/src/parser.rs.bak", "src/parser.rs.bak")
            .unwrap()
            .is_empty());
    }
}
