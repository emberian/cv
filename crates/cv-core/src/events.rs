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
//! `shell`/`apply_patch`/`local_shell`, gemini `run_shell_command`/`write_file`/`replace`,
//! antigravity `edit_file`/`replace_file_content`/`view_file`/`run_command`, pi `edit`/`write`/`read`/`bash`, …), so
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

#[path = "events_workspace.rs"]
pub mod workspace;
use crate::ir::{Block, Message, Session, SessionRef};
use crate::json::{Json, JsonView};
use crate::stream::{Checkpoint, Flow, MessageSink};
use anyhow::Result;
#[cfg(test)]
use serde_json::Value;
use std::path::{Path, PathBuf};

mod spool;
pub use crate::tools::classify::{
    extract_content_hunk_hashes, extract_diff_hunk_hashes, hash_normalized_hunk, hash_normalized_line,
    patch_paths_and_hunks, MAX_HUNK_HASHES_PER_EDIT,
};
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
    /// Normalized 64-bit hunk & line fingerprints extracted from `file_edit` operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunk_hashes: Vec<u64>,
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
    fn tier(&self) -> crate::Tier {
        crate::Tier::Facts
    }
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
                        hunk_hashes: Vec::new(),
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
                // Bounded error head (`<= 1024` UTF-8 bytes): a lazy one reads only a small
                // prefix from the source. A caller that keeps no text (`cv tools` counts rows)
                // passes no resolver and reads nothing.
                let detail = match details {
                    Some(resolver) => extract_capped_detail(content, resolver)?,
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
                    hunk_hashes: Vec::new(),
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
            .map(|(i, (kind, target, hunk_hashes))| Event {
                msg_idx,
                ts,
                kind,
                tool: Some(name.to_owned()),
                target,
                detail: None,
                call: i == 0,
                hunk_hashes,
            }),
    );
    Ok(())
}

/// Cross-harness classification of one tool invocation into `(kind, target, hunk_hashes)` rows via
/// [`crate::tools::classify::visit_tool_ops_with_hunks`].
type ClassifiedToolRow = (&'static str, Option<String>, Vec<u64>);

/// Maximum UTF-8 bytes stored in `Event.target` for a `command` event (file paths stay whole).
pub const MAX_COMMAND_TARGET_BYTES: usize = 512;

/// Maximum UTF-8 bytes stored in `Event.detail` for a failed tool result.
pub const MAX_ERROR_DETAIL_BYTES: usize = 1024;

/// Cap a shell/command string at [`MAX_COMMAND_TARGET_BYTES`] UTF-8 bytes while collapsing
/// multiline heredocs and oversized `-c` bodies into `head … [+N bytes]`.
fn cap_command_target(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let newline_at = trimmed.find(['\n', '\r']);
    if newline_at.is_none() && trimmed.len() <= MAX_COMMAND_TARGET_BYTES {
        return trimmed.to_owned();
    }
    let first_line = match newline_at {
        Some(pos) => trimmed[..pos].trim_end(),
        None => trimmed,
    };
    // Reserve worst-case suffix length (` … [+` = 7 UTF-8 bytes + digits + ` bytes]` = 7 bytes).
    let digits_cap = 20usize;
    let suffix_budget = 7 + digits_cap + 7;
    let max_head = MAX_COMMAND_TARGET_BYTES.saturating_sub(suffix_budget);
    let head_end = first_line.floor_char_boundary(max_head);
    let head = first_line[..head_end].trim_end();
    let omitted = trimmed.len().saturating_sub(head.len());
    format!("{head} … [+{omitted} bytes]")
}

fn extract_capped_detail(content: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> Result<Option<String>> {
    let mut head = String::new();
    let result = content.try_for_each_chunk(resolver, 4096, |piece| {
        let slice = if head.is_empty() { piece.trim_start() } else { piece };
        let take = slice.floor_char_boundary(MAX_ERROR_DETAIL_BYTES - head.len());
        head.push_str(&slice[..take]);
        if take < slice.len() || head.len() == MAX_ERROR_DETAIL_BYTES {
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    match result {
        Err(error) if error.is::<crate::stream::Stop>() => {}
        other => other?,
    }
    head.truncate(head.trim_end().len());
    Ok((!head.is_empty()).then_some(head))
}

fn classify_tool_view(
    name: &str,
    input: JsonView<'_>,
    cwd: Option<&Path>,
    commands: bool,
) -> Result<Vec<ClassifiedToolRow>> {
    use crate::tools::classify::{ClassifyOpts, ToolOp};
    let opts = if commands {
        ClassifyOpts::EVENTS
    } else {
        ClassifyOpts::COUNTS
    };
    let mut rows = Vec::new();
    let mut fallback_path = None;
    let mut visit = |sem: crate::tools::classify::ToolSemantic<'_>, hunks: Vec<u64>| match sem.op {
        ToolOp::Command { cmd, .. } => rows.push(("command", cmd.map(|c| cap_command_target(&c)), Vec::new())),
        ToolOp::FileEdit { path, .. } => rows.push(("file_edit", Some(absolutize(&path, cwd)), hunks)),
        ToolOp::FileRead { path } | ToolOp::Search { path: Some(path), .. } => {
            rows.push(("file_read", Some(absolutize(&path, cwd)), Vec::new()));
        }
        ToolOp::Other { path: Some(path), .. } if fallback_path.is_none() => {
            fallback_path = Some(absolutize(&path, cwd));
        }
        _ => {}
    };
    if commands {
        crate::tools::classify::visit_tool_ops_with_hunks(None, name, input, opts, &mut visit)?;
    } else {
        crate::tools::classify::visit_tool_ops(None, name, input, opts, |sem| {
            visit(sem, Vec::new());
        })?;
    }
    if rows.is_empty() {
        rows.push(("tool", fallback_path, Vec::new()));
    }
    Ok(rows)
}

/// Full command projection: the string a `command` event's target holds.
pub(crate) fn command_of_json(input: &Json) -> Result<Option<String>> {
    input.with_view(crate::tools::classify::command_string)
}

/// Absolutize `p` against the session cwd and canonicalize the deepest existing ancestor so OS
/// symlink aliases (such as macOS `/var` <-> `/private/var`) match identically across ingest and
/// hook/query lookups. With no cwd, relative paths are stored as given (still suffix-matchable).
pub fn absolutize(p: &str, cwd: Option<&Path>) -> String {
    crate::tools::classify::absolutize_path(p, cwd)
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
    let mut rows = read_ref_rows(r, kind)?;
    let native_id = crate::SessionKey::split_selector(&r.id).map_or(r.id.as_str(), |(n, _)| n);
    let baggage = crate::hooks::Baggage::for_session(r.harness.as_str(), native_id);
    for row in &mut rows {
        row.baggage.clone_from(&baggage)
    }
    Ok(rows)
}

fn read_ref_rows(r: &crate::SessionRef, kind: Option<&str>) -> Result<Vec<EventRow>> {
    #[cfg(feature = "sqlite")]
    let backing = crate::custody::backing_for(r)?;
    #[cfg(not(feature = "sqlite"))]
    let backing: Option<Box<dyn crate::Adapter>> = None;
    if needs_ingest(r) {
        if crate::env::frozen() {
            let adapter = backing
                .or_else(|| crate::harness::for_harness(r.harness))
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
                    baggage: None,
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
    pub baggage: Option<crate::hooks::Baggage>,
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

/// [`EditEvent`] paired with the normalized 64-bit hunk & line fingerprints extracted from the edit.
#[derive(Debug, Clone)]
pub struct EditHunkEvent {
    pub edit: EditEvent,
    pub hunk_hashes: Vec<u64>,
}

/// Encode `u64` hunk fingerprints as little-endian bytes (`None` when empty so non-edit rows pay zero bytes).
pub fn encode_hunk_hashes(hashes: &[u64]) -> Option<Vec<u8>> {
    if hashes.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(hashes.len() * 8);
    for &h in hashes {
        bytes.extend_from_slice(&h.to_le_bytes());
    }
    Some(bytes)
}

/// Persisted `(msg_idx, target, hunk_hashes)` row for a `file_edit` event of one session.
pub type SessionEditHunkRow = (i64, Option<String>, Vec<u64>);

/// Decode little-endian `u64` hunk fingerprints from a stored SQLite/segment blob.
pub fn decode_hunk_hashes(blob: Option<&[u8]>) -> Vec<u64> {
    let Some(bytes) = blob else {
        return Vec::new();
    };
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .copied()
        .map(u64::from_le_bytes)
        .collect()
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
    ///   `UNIQUE(selector)` here (the profile-qualified [`SessionRef::session_selector`]) is
    ///   stable across syncs and tiny (~2.4k rows), and keeps two profiles' copies of one native
    ///   id apart: indexing one never replaces the other's rows. A catalog keyed the old way, by
    ///   `(harness, session_id)`, drops its event rows and their baselines once, and re-ingests.
    /// * `event_targets` interns the distinct target strings (~54k of ~145k events); event rows
    ///   carry a nullable integer. Re-ingest deletes a session's event rows but leaves its
    ///   interned targets — orphans are unreferenced cache rows, reclaimed only when a
    ///   catalog of another schema is rebuilt. Suffix matching scans `event_targets` instead of every event row.
    /// * No dedup: [`record`] is DELETE-by-session + batch INSERT in one transaction, and every
    ///   extracted event is a row.
    /// * `idx_events_session` serves [`events_for`] and `record`'s delete; `idx_events_target`
    ///   joins event rows back from a matched target.
    pub(crate) fn open() -> Option<Connection> {
        let mut conn = crate::catalog::open_db()?;
        if crate::env::frozen() || conn.is_readonly(rusqlite::MAIN_DB).ok()? {
            return Some(conn);
        }
        if table_has_column(&conn, "event_sessions", "harness") && !has_selector(&conn) {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .ok()?;
            if table_has_column(&tx, "event_sessions", "harness") && !has_selector(&tx) {
                tx.execute_batch("DROP TABLE IF EXISTS events; DROP TABLE event_sessions;")
                    .ok()?;
                if table_has_column(&tx, "source_dependencies", "consumer") {
                    tx.execute("DELETE FROM source_dependencies WHERE consumer='events'", [])
                        .ok()?;
                }
            }
            tx.commit().ok()?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_sessions(
                 id         INTEGER PRIMARY KEY,
                 selector   TEXT NOT NULL UNIQUE,
                 harness    TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 parent_id  TEXT,
                 agent_id   TEXT,
                 workflow   TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_event_sessions_native ON event_sessions(harness, session_id);
             CREATE TABLE IF NOT EXISTS event_targets(
                 id         INTEGER PRIMARY KEY,
                 target     TEXT NOT NULL UNIQUE,
                 rev_target TEXT
             );
             CREATE TABLE IF NOT EXISTS events(
                 session     INTEGER NOT NULL,
                 msg_idx     INTEGER NOT NULL,
                 ts          INTEGER,
                 kind        TEXT NOT NULL,
                 tool        TEXT,
                 target_id   INTEGER,
                 detail      TEXT,
                 hunk_hashes BLOB
             );
             CREATE INDEX IF NOT EXISTS idx_events_session ON events(session);
             CREATE INDEX IF NOT EXISTS idx_events_target ON events(target_id);",
        )
        .ok()?;
        let has_hunk_col = table_has_column(&conn, "events", "hunk_hashes");
        if !has_hunk_col {
            let _ = conn.execute("ALTER TABLE events ADD COLUMN hunk_hashes BLOB", []);
        }
        if let Err(error) = ensure_rev_targets(&conn) {
            crate::env::warn_once("event suffix index", error);
        }
        Some(conn)
    }

    fn reverse_chars(s: &str) -> String {
        s.chars().rev().collect()
    }

    fn ensure_rev_targets(conn: &Connection) -> rusqlite::Result<()> {
        if !table_has_column(conn, "event_targets", "rev_target") {
            let tx = conn.unchecked_transaction()?;
            if !table_has_column(&tx, "event_targets", "rev_target") {
                tx.execute("ALTER TABLE event_targets ADD COLUMN rev_target TEXT", [])?;
            }
            tx.commit()?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_event_targets_suffix ON event_targets(rev_target COLLATE NOCASE);
             DROP INDEX IF EXISTS idx_event_targets_rev;",
        )?;
        loop {
            let tx = conn.unchecked_transaction()?;
            let pending = tx
                .prepare("SELECT id,target FROM event_targets WHERE rev_target IS NULL LIMIT 256")?
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if pending.is_empty() {
                return Ok(());
            }
            let mut update = tx.prepare("UPDATE event_targets SET rev_target=?1 WHERE id=?2")?;
            for (id, target) in pending {
                update.execute(params![reverse_chars(&target), id])?;
            }
            drop(update);
            tx.commit()?;
        }
    }

    fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .is_ok_and(|names| names.iter().any(|n| n == column))
    }

    /// Whether `event_sessions` is keyed by selector (a frozen read of an older catalog is not).
    fn has_selector(conn: &Connection) -> bool {
        table_has_column(conn, "event_sessions", "selector")
    }

    /// The `WHERE` term naming one session by `?2`: a native id (every profile's copy) or a
    /// selector (exactly one), or on an older catalog the native id alone.
    fn session_named(conn: &Connection) -> &'static str {
        match has_selector(conn) {
            true => "es.harness=?1 AND (es.session_id=?2 OR es.selector=?2)",
            false => "es.harness=?1 AND es.session_id=?2",
        }
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
        // Required: pre-0.19 catalogs lack 64-bit edit hunk hashes and replay cleanly on sync.
        #[serde(default)]
        hunk_hashes: bool,
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
            hunk_hashes: true,
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
        let selector = r.session_selector();
        tx.execute(
            "INSERT OR IGNORE INTO event_sessions(selector, harness, session_id) VALUES(?1,?2,?3)",
            params![selector, harness, r.id],
        )?;
        // Stamp provenance on every record (idempotent re-ingest keeps it current); top-level
        // sessions write NULLs, which leaves the existing top-level rows exactly as before.
        tx.execute(
            "UPDATE event_sessions SET parent_id=?2, agent_id=?3, workflow=?4 WHERE selector=?1",
            params![selector, prov.parent_id, prov.agent_id, prov.workflow],
        )?;
        let session = tx.query_row(
            "SELECT id FROM event_sessions WHERE selector=?1",
            params![selector],
            |row| row.get::<_, i64>(0),
        )?;
        tx.execute(
            "DELETE FROM events WHERE session=?1 AND msg_idx>=?2",
            params![session, from as i64],
        )?;
        {
            let mut intern =
                tx.prepare_cached("INSERT OR IGNORE INTO event_targets(target, rev_target) VALUES(?1, ?2)")?;
            let mut intern_id = tx.prepare_cached("SELECT id FROM event_targets WHERE target=?1")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO events(session,msg_idx,ts,kind,tool,target_id,detail,hunk_hashes)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
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
                            intern.execute(params![t, reverse_chars(t)])?;
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
                let hunk_blob = encode_hunk_hashes(&e.hunk_hashes);
                insert.execute(params![
                    session,
                    e.msg_idx as i64,
                    e.ts,
                    e.kind,
                    e.tool,
                    target_id,
                    e.detail,
                    hunk_blob,
                ])?;
            }
        }
        // One session's rows have one source copy: another path of the same selector (the file
        // moved within its profile) no longer describes them. Another profile's copy keeps its own.
        let paths = {
            let mut stmt =
                tx.prepare("SELECT path FROM source_dependencies WHERE consumer='events' AND harness=?1 AND id=?2")?;
            let paths = stmt
                .query_map(params![harness, r.id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            paths
        };
        for path in paths {
            if SessionRef::new(r.id.clone(), r.harness, &path).session_selector() == selector {
                tx.execute(
                    "DELETE FROM source_dependencies WHERE consumer='events' AND harness=?1 AND id=?2 AND path=?3",
                    params![harness, r.id, path],
                )?;
            }
        }
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
        let adapter = crate::custody::backing_for(r)?
            .or_else(|| crate::harness::for_harness(r.harness))
            .ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
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
            let mut visited = std::collections::HashSet::new();
            let mut frontier: Vec<(SessionRef, String, Option<String>)> = sources
                .iter()
                .map(|(parent, _)| {
                    visited.insert((parent.harness, parent.id.clone(), parent.path.clone()));
                    (parent.clone(), parent.id.clone(), None)
                })
                .collect();
            let mut folded = Vec::new();
            while !frontier.is_empty() {
                let mut next_frontier = Vec::new();
                for (parent, root_parent_id, inherited_workflow) in frontier {
                    for sub in crate::subagent_tree_of(&parent)? {
                        let wf = sub.workflow.clone().or_else(|| inherited_workflow.clone());
                        let provenance = Provenance {
                            parent_id: Some(root_parent_id.clone()),
                            agent_id: Some(sub.agent_id().to_owned()),
                            workflow: wf.clone(),
                        };
                        let key = (sub.session.harness, sub.session.id.clone(), sub.session.path.clone());
                        if visited.insert(key) {
                            next_frontier.push((sub.session.clone(), root_parent_id.clone(), wf));
                        }
                        folded.push((sub.session, provenance));
                    }
                }
                frontier = next_frontier;
            }
            sources.extend(folded);
        }
        Ok(preferred_sources(sources))
    }

    /// Events have one output per session selector, independent of how many source copies
    /// discovery returns of it (two profiles' copies of one native id are two selectors, two
    /// outputs). Resolve that output once before publication; direct attribution wins equal copies.
    pub(super) fn preferred_sources(
        sources: impl IntoIterator<Item = (SessionRef, Provenance)>,
    ) -> Vec<(SessionRef, Provenance)> {
        let mut copies: std::collections::HashMap<_, (SessionRef, Provenance)> = std::collections::HashMap::new();
        for (r, provenance) in sources {
            let key = r.session_selector();
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
        let mut stmt = conn.prepare(&format!(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, e.kind, e.tool, t.target, e.detail,
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN event_targets t ON t.id = e.target_id
             WHERE {} AND (?3 IS NULL OR e.kind=?3)
             ORDER BY es.id, e.msg_idx, e.kind",
            session_named(&conn)
        ))?;
        let rows = stmt.query_map(params![harness, session_id, kind], |row| {
            Ok(EventRow {
                baggage: None,
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
    /// The reversed suffix uses the target prefix index. A legacy catalog scans its interned targets.
    pub fn sessions_touching(path: &str, edits_only: bool) -> Result<Vec<TouchedSession>> {
        let abs = absolutize(path, crate::env::current_dir().as_deref());
        let trimmed = path.trim_start_matches("./");
        let suffix = if trimmed.starts_with('/') {
            trimmed.to_owned()
        } else {
            format!("/{trimmed}")
        };
        touching(&abs, Some(&suffix), edits_only)
    }

    /// Sessions that EDITED exactly `abs` (an absolute path as the event index stores it): the
    /// indexed point lookup behind the `cv hook edit` guard, which must answer in milliseconds on
    /// a store of millions of events. No suffix match, so no scan of `event_targets` — the
    /// `target` UNIQUE index resolves the path and `idx_events_target` the rows. Newest first.
    pub fn sessions_editing(abs: &str) -> Result<Vec<TouchedSession>> {
        let abs = absolutize(abs, None);
        touching(&abs, None, true)
    }

    #[derive(Default, Clone)]
    struct CatalogSessionMeta {
        title: Option<String>,
        cwd: Option<String>,
        created_at: Option<i64>,
        updated_at: Option<i64>,
    }

    impl CatalogSessionMeta {
        fn merge(&mut self, other: Self) {
            if let Some(t) = other.title {
                if self.title.as_deref().is_none_or(|cur| t.as_str() > cur) {
                    self.title = Some(t);
                }
            }
            if let Some(c) = other.cwd {
                if self.cwd.as_deref().is_none_or(|cur| c.as_str() > cur) {
                    self.cwd = Some(c);
                }
            }
            if let Some(ts) = other.created_at {
                self.created_at = Some(self.created_at.map_or(ts, |cur| cur.max(ts)));
            }
            if let Some(ts) = other.updated_at {
                self.updated_at = Some(self.updated_at.map_or(ts, |cur| cur.max(ts)));
            }
        }
    }

    struct CatalogMetaLookup {
        by_selector: std::collections::HashMap<(String, String), CatalogSessionMeta>,
        by_native: std::collections::HashMap<(String, String), CatalogSessionMeta>,
    }

    impl CatalogMetaLookup {
        fn load<'a>(conn: &Connection, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self> {
            let mut seen = std::collections::HashSet::new();
            let mut by_selector: std::collections::HashMap<(String, String), CatalogSessionMeta> =
                std::collections::HashMap::new();
            let mut by_native: std::collections::HashMap<(String, String), CatalogSessionMeta> =
                std::collections::HashMap::new();
            if !table_has_column(conn, "sessions", "path") {
                return Ok(Self { by_selector, by_native });
            }
            let mut stmt = conn.prepare_cached(
                "SELECT path, title, cwd, created_at, updated_at
                 FROM sessions WHERE harness = ?1 AND id = ?2",
            )?;
            for (harness, native_id) in pairs {
                if !seen.insert((harness.to_owned(), native_id.to_owned())) {
                    continue;
                }
                let parsed_harness = crate::Harness::parse(harness);
                let rows = stmt.query_map(params![harness, native_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        CatalogSessionMeta {
                            title: r.get(1)?,
                            cwd: r.get(2)?,
                            created_at: r.get(3)?,
                            updated_at: r.get(4)?,
                        },
                    ))
                })?;
                for row in rows {
                    let (path, meta) = row?;
                    if let Some(h) = parsed_harness {
                        let selector = SessionRef::new(native_id, h, Path::new(&path)).session_selector();
                        by_selector
                            .entry((harness.to_owned(), selector))
                            .or_default()
                            .merge(meta.clone());
                    }
                    by_native
                        .entry((harness.to_owned(), native_id.to_owned()))
                        .or_default()
                        .merge(meta);
                }
            }
            Ok(Self { by_selector, by_native })
        }

        fn get(&self, harness: &str, native_id: &str, selector: Option<&str>) -> Option<&CatalogSessionMeta> {
            match selector {
                Some(sel) => self.by_selector.get(&(harness.to_owned(), sel.to_owned())),
                None => self.by_native.get(&(harness.to_owned(), native_id.to_owned())),
            }
        }
    }

    fn escape_like(s: &str) -> String {
        s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    }

    /// Keep the original SQLite LIKE semantics, including ASCII case folding. A legacy
    /// writer can insert a NULL after migration, so include those rows in the same snapshot.
    fn target_query(
        conn: &Connection,
        abs: &str,
        rel: Option<&str>,
        suffix: Option<&str>,
    ) -> (&'static str, Vec<String>) {
        let Some(suffix) = suffix else {
            return ("SELECT id FROM event_targets WHERE target=?1", vec![abs.to_owned()]);
        };
        let mut args = vec![
            abs.to_owned(),
            rel.unwrap_or(abs).to_owned(),
            format!("%{}", escape_like(suffix)),
        ];
        if !table_has_column(conn, "event_targets", "rev_target") {
            return (
                "SELECT id FROM event_targets WHERE target IN (?1,?2) OR target LIKE ?3 ESCAPE '\\'",
                args,
            );
        }
        args.push(format!("{}%", escape_like(&reverse_chars(suffix))));
        (
            "SELECT id FROM event_targets WHERE target IN (?1,?2)
          UNION ALL SELECT id FROM event_targets WHERE rev_target LIKE ?4 ESCAPE '\\'
          UNION ALL SELECT id FROM event_targets WHERE rev_target IS NULL AND target LIKE ?3 ESCAPE '\\'",
            args,
        )
    }

    fn touching(abs: &str, suffix: Option<&str>, edits_only: bool) -> Result<Vec<TouchedSession>> {
        let conn = open().context("the event catalog cannot be opened")?;
        let having = if edits_only { "HAVING edits > 0" } else { "" };
        let (target_subquery, args) = target_query(&conn, abs, None, suffix);
        let selector_expr = if has_selector(&conn) { "es.selector" } else { "NULL" };
        let sql = format!(
            "SELECT es.harness, es.session_id,
                    SUM(e.kind='file_edit') AS edits,
                    SUM(e.kind='file_read') AS reads,
                    MAX(e.ts) AS last_ts,
                    {selector_expr},
                    es.parent_id, es.agent_id, es.workflow
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             WHERE e.kind IN ('file_edit','file_read')
               AND e.target_id IN ({target_subquery})
             GROUP BY es.id
             {having}
             ORDER BY last_ts DESC, es.session_id, es.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row| {
            Ok((
                row.get::<_, Option<String>>(5)?,
                TouchedSession {
                    harness: row.get(0)?,
                    session_id: row.get(1)?,
                    edits: row.get(2)?,
                    reads: row.get(3)?,
                    last_ts: row.get(4)?,
                    title: None,
                    parent_id: row.get(6)?,
                    agent_id: row.get(7)?,
                    workflow: row.get(8)?,
                },
            ))
        };
        let raw: Vec<(Option<String>, TouchedSession)> = stmt
            .query_map(rusqlite::params_from_iter(args), map)?
            .collect::<rusqlite::Result<_>>()?;
        let lookup = CatalogMetaLookup::load(
            &conn,
            raw.iter().map(|(_, s)| (s.harness.as_str(), s.session_id.as_str())),
        )?;
        Ok(raw
            .into_iter()
            .map(|(selector, mut s)| {
                s.title = lookup
                    .get(&s.harness, &s.session_id, selector.as_deref())
                    .and_then(|m| m.title.clone());
                s
            })
            .collect())
    }

    /// Every `file_edit` event on one file, with session metadata riding along, oldest first.
    ///
    /// Matched two ways, mirroring [`sessions_touching`]: exactly by `abs` (the file's absolute
    /// path in the *current* checkout) and by repo-relative suffix `rel` (target equals `rel` or
    /// ends with `/<rel>`), so edits recorded by a session whose checkout lived at a different
    /// path still surface. Grouped per `event_sessions` row so profile twins sharing `(harness, id)`
    /// stay distinct and resolve their own `sessions` metadata by selector.
    pub fn edits_touching_file(abs: &str, rel: &str) -> Result<Vec<EditEvent>> {
        let abs = absolutize(abs, None);
        let conn = open().context("the event catalog cannot be opened")?;
        let (target_subquery, args) = target_query(&conn, &abs, Some(rel), Some(&format!("/{rel}")));
        let selector_expr = if has_selector(&conn) { "es.selector" } else { "NULL" };
        let mut stmt = conn.prepare(&format!(
            "SELECT es.harness, es.session_id, e.msg_idx, e.ts, t.target,
                    MAX(es.parent_id), {selector_expr}
             FROM events e
             JOIN event_targets t ON t.id = e.target_id
             JOIN event_sessions es ON es.id = e.session
             WHERE e.kind = 'file_edit'
               AND e.target_id IN ({target_subquery})
             GROUP BY es.id, e.msg_idx, e.ts, t.target
             ORDER BY e.ts, e.msg_idx, es.id, MIN(e.rowid)"
        ))?;
        let map = |row: &rusqlite::Row| {
            Ok((
                row.get::<_, Option<String>>(6)?,
                EditEvent {
                    harness: row.get(0)?,
                    session_id: row.get(1)?,
                    msg_idx: row.get(2)?,
                    ts: row.get(3)?,
                    target: row.get(4)?,
                    title: None,
                    cwd: None,
                    created_at: None,
                    updated_at: None,
                    parent_id: row.get(5)?,
                },
            ))
        };
        let raw: Vec<(Option<String>, EditEvent)> = stmt
            .query_map(rusqlite::params_from_iter(args), map)?
            .collect::<rusqlite::Result<_>>()?;
        let lookup = CatalogMetaLookup::load(
            &conn,
            raw.iter().map(|(_, e)| (e.harness.as_str(), e.session_id.as_str())),
        )?;
        Ok(raw
            .into_iter()
            .map(|(selector, mut e)| {
                if let Some(m) = lookup.get(&e.harness, &e.session_id, selector.as_deref()) {
                    e.title.clone_from(&m.title);
                    e.cwd.clone_from(&m.cwd);
                    e.created_at = m.created_at;
                    e.updated_at = m.updated_at;
                }
                e
            })
            .collect())
    }

    /// Same file-matching and ordering semantics as [`edits_touching_file`], additionally
    /// returning the persisted 64-bit normalized hunk & line fingerprints for each edit.
    pub fn edits_with_hunks_touching_file(abs: &str, rel: &str) -> Result<Vec<EditHunkEvent>> {
        let abs = absolutize(abs, None);
        let conn = open().context("the event catalog cannot be opened")?;
        let (target_subquery, args) = target_query(&conn, &abs, Some(rel), Some(&format!("/{rel}")));
        let selector_expr = if has_selector(&conn) { "es.selector" } else { "NULL" };
        let mut stmt = conn.prepare(&format!(
            "SELECT es.id, es.harness, es.session_id, e.msg_idx, e.ts, t.target,
                    es.parent_id, e.hunk_hashes, {selector_expr}
             FROM events e
             JOIN event_targets t ON t.id = e.target_id
             JOIN event_sessions es ON es.id = e.session
             WHERE e.kind = 'file_edit'
               AND e.target_id IN ({target_subquery})
             ORDER BY e.ts, e.msg_idx, es.id, e.rowid"
        ))?;
        let map = |row: &rusqlite::Row| {
            let blob: Option<Vec<u8>> = row.get(7)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(8)?,
                EditHunkEvent {
                    edit: EditEvent {
                        harness: row.get(1)?,
                        session_id: row.get(2)?,
                        msg_idx: row.get(3)?,
                        ts: row.get(4)?,
                        target: row.get(5)?,
                        title: None,
                        cwd: None,
                        created_at: None,
                        updated_at: None,
                        parent_id: row.get(6)?,
                    },
                    hunk_hashes: decode_hunk_hashes(blob.as_deref()),
                },
            ))
        };
        let raw_rows: Vec<(i64, Option<String>, EditHunkEvent)> = stmt
            .query_map(rusqlite::params_from_iter(args), map)?
            .collect::<rusqlite::Result<_>>()?;
        let lookup = CatalogMetaLookup::load(
            &conn,
            raw_rows
                .iter()
                .map(|(_, _, item)| (item.edit.harness.as_str(), item.edit.session_id.as_str())),
        )?;
        // Merge rows that share the same `(es.id, msg_idx, ts, target)` key so row cardinality
        // matches [`edits_touching_file`] 1:1 while keeping profile twins apart.
        type EditMergeKey = (i64, i64, Option<i64>, Option<String>);
        let mut merged: Vec<EditHunkEvent> = Vec::new();
        let mut key_to_idx: std::collections::HashMap<EditMergeKey, usize> = std::collections::HashMap::new();
        for (es_id, selector, mut item) in raw_rows {
            if let Some(m) = lookup.get(&item.edit.harness, &item.edit.session_id, selector.as_deref()) {
                item.edit.title.clone_from(&m.title);
                item.edit.cwd.clone_from(&m.cwd);
                item.edit.created_at = m.created_at;
                item.edit.updated_at = m.updated_at;
            }
            let key = (es_id, item.edit.msg_idx, item.edit.ts, item.edit.target.clone());
            if let Some(&idx) = key_to_idx.get(&key) {
                let dst = &mut merged[idx].hunk_hashes;
                for h in item.hunk_hashes {
                    if dst.len() < MAX_HUNK_HASHES_PER_EDIT && !dst.contains(&h) {
                        dst.push(h);
                    }
                }
            } else {
                key_to_idx.insert(key, merged.len());
                merged.push(item);
            }
        }
        Ok(merged)
    }

    /// Persisted `(msg_idx, target, hunk_hashes)` rows for `file_edit` events of one session.
    pub fn edit_hunks_for(harness: &str, session_id: &str) -> Result<Vec<SessionEditHunkRow>> {
        let conn = open().context("the event catalog cannot be opened")?;
        let mut stmt = conn.prepare(&format!(
            "SELECT e.msg_idx, t.target, e.hunk_hashes
             FROM events e
             JOIN event_sessions es ON es.id = e.session
             LEFT JOIN event_targets t ON t.id = e.target_id
             WHERE {} AND e.kind='file_edit'
             ORDER BY es.id, e.msg_idx, e.rowid",
            session_named(&conn)
        ))?;
        let rows = stmt.query_map(params![harness, session_id], |row| {
            let blob: Option<Vec<u8>> = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, decode_hunk_hashes(blob.as_deref())))
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
                    "INSERT OR IGNORE INTO event_sessions(selector,harness,session_id) VALUES('lock','claude','lock')",
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
                    "INSERT OR IGNORE INTO event_sessions(selector,harness,session_id) VALUES('lock','claude','lock')",
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
    catalog_has_events, edit_hunks_for, edits_touching_file, edits_with_hunks_touching_file, events_for, ingest_all,
    ingest_ref, ingest_subagent, needs_ingest, record_spooled_with_source, sessions_editing, sessions_touching,
    SyncTable,
};

/// No-op fallbacks without sqlite: extraction still works (the sink is pure), persistence and
/// queries degrade to nothing — mirroring [`crate::catalog`].
#[cfg(feature = "sqlite")]
pub(crate) use db::{open as writer_connection, record_spooled_in};

/// Read the immutable consumer identity embedded in a Tantivy schema.
#[cfg(feature = "sqlite")]
pub fn fts_schema_consumer(dir: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::read(dir.join("meta.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata: serde_json::Value = serde_json::from_slice(&metadata)?;
    Ok(metadata["schema"].as_array().and_then(|fields| {
        fields.iter().find_map(|field| {
            let id = field["name"].as_str()?.strip_prefix("cv_consumer_")?;
            uuid::Uuid::parse_str(id).ok()?;
            Some(format!("fts:{id}"))
        })
    }))
}

/// Verify the current cache pair when reporting a live watermark. A different or cleared
/// cache has no coverage, even when the durable hub remembers indexing in another cache.
#[cfg(feature = "sqlite")]
pub(crate) fn current_hub_index_cursor() -> Result<u64> {
    use rusqlite::OptionalExtension;
    let Some(cache) = crate::env::cv_cache() else {
        return Ok(0);
    };
    let dir = cache.join("tantivy");
    let db = cache.join("catalog.db");
    if !dir.join("meta.json").is_file() || !db.is_file() {
        return Ok(0);
    }
    let consumer = match fts_schema_consumer(&dir)? {
        Some(c) => c,
        None => match std::fs::read_to_string(dir.join("cv-index-id")) {
            Ok(id) if !id.trim().is_empty() => format!("fts:{}", id.trim()),
            Ok(_) => return Ok(0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        },
    };
    crate::harness::store::sqlite::open_ro(&db)?.read_snapshot(|conn, _| {
        if !conn.table_exists(None, "hub_index_cursor")? {
            return Ok(0);
        }
        Ok(conn
            .query_row(
                "SELECT generation FROM hub_index_cursor WHERE consumer=?1",
                [consumer],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    })
}

/// Per-index catch-up cursor lives with the derived events. Replacing either index or catalog
/// starts at zero. Advance it only after the Tantivy commit and all event publications succeed.
#[cfg(feature = "sqlite")]
pub fn hub_index_cursor(consumer: &str) -> Result<u64> {
    use anyhow::Context as _;
    use rusqlite::OptionalExtension;
    let conn = writer_connection().context("opening event catalog")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_index_cursor(consumer TEXT PRIMARY KEY,generation INTEGER NOT NULL)",
    )?;
    Ok(conn
        .query_row(
            "SELECT generation FROM hub_index_cursor WHERE consumer=?1",
            [consumer],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

#[cfg(feature = "sqlite")]
pub fn finish_hub_index(consumer: &str, generation: u64, removed: &[String]) -> Result<()> {
    use anyhow::Context as _;
    let mut conn = writer_connection().context("opening event catalog")?;
    let tx = conn.transaction()?;
    for selector in removed {
        tx.execute(
            "DELETE FROM events WHERE session IN (SELECT id FROM event_sessions WHERE selector=?1)",
            [selector],
        )?;
        tx.execute("DELETE FROM event_sessions WHERE selector=?1", [selector])?;
        tx.execute(
            "DELETE FROM source_dependencies WHERE id=?1 AND consumer IN ('events',?2)",
            rusqlite::params![selector, consumer],
        )?;
    }
    tx.execute(
        "INSERT INTO hub_index_cursor VALUES(?1,?2) ON CONFLICT(consumer) DO UPDATE SET generation=excluded.generation",
        rusqlite::params![consumer, generation],
    )?;
    tx.commit()?;
    Ok(())
}

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
    pub fn edits_with_hunks_touching_file(_abs: &str, _rel: &str) -> Result<Vec<EditHunkEvent>> {
        Ok(Vec::new())
    }
    pub fn edit_hunks_for(_harness: &str, _session_id: &str) -> Result<Vec<SessionEditHunkRow>> {
        Ok(Vec::new())
    }
    pub fn catalog_has_events() -> bool {
        false
    }
}
#[cfg(not(feature = "sqlite"))]
pub use db_stub::{
    catalog_has_events, edit_hunks_for, edits_touching_file, edits_with_hunks_touching_file, events_for, ingest_all,
    ingest_ref, ingest_subagent, needs_ingest, record_spooled_with_source, sessions_editing, sessions_touching,
    SyncTable,
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
            extensions: Default::default(),
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
                // A command target is stored trimmed.
                vec![("command", Some("echo".to_string()))],
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
                        // A multi-line command keeps its first line and the size of the rest.
                        Some("apply_patch *** Begin Patch … [+33 bytes]".to_string()),
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
                    .unwrap()
                    .into_iter()
                    .map(|(k, t, _)| (k, t))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                classify_tool_view(name, JsonView::from(&owned), Some(Path::new("/w")), true)
                    .unwrap()
                    .into_iter()
                    .map(|(k, t, _)| (k, t))
                    .collect::<Vec<_>>(),
                expected
            );
            let mut message = msg(vec![
                tool_use("Read", json!({"path":"before"})),
                Block::ToolUse {
                    id: "lazy".into(),
                    name: name.into(),
                    namespace: None,
                    input: source,
                    extensions: Default::default(),
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
                .any(|(kind, path, _)| *kind == "file_edit" && path.as_deref() == Some("late.rs")));
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
        let expected_hosts = absolutize("/etc/hosts", None);
        assert_eq!(
            (ev[1].kind, ev[1].target.as_deref()),
            ("file_read", Some(expected_hosts.as_str()))
        );
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
        crate::tools::classify::patch_rows(crate::json::JsonView::from(&json!(patch)), false)
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect()
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
                extensions: Default::default(),
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
                extensions: Default::default(),
            },
            // Non-error results emit nothing.
            Block::ToolResult {
                tool_use_id: "t3".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ];
        let ev = extract(&m, 3, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "error");
        assert_eq!(ev[0].detail.as_deref(), Some("command failed: no such file"));
        assert_eq!(ev[0].tool.as_deref(), Some("Bash"));
        assert_eq!(ev[1].kind, "error");
        assert_eq!(ev[1].kind, "error");
        let capped_detail = ev[1].detail.as_deref().unwrap();
        assert_eq!(capped_detail.len(), MAX_ERROR_DETAIL_BYTES);
        assert!(dump.trim().starts_with(capped_detail));
        // A source that is gone fails the extraction instead of dropping the detail.
        std::fs::remove_file(&path).unwrap();
        assert!(extract(&m, 3, None, &crate::lazy::Resolver::new(None)).is_err());
    }

    #[test]
    fn long_commands_and_details_are_capped_safely_while_file_paths_stay_whole() {
        let long_cmd = format!("cat <<'EOF'\n{}\nEOF", "x".repeat(100_000));
        let giant_inline = format!("python3 -c '{}'", "🦀".repeat(5_000));
        let long_path = format!("/repo/{}/deep/file.rs", "segment_dir_".repeat(60));
        let m = msg(vec![
            tool_use("Bash", json!({"command": long_cmd})),
            tool_use("Bash", json!({"command": giant_inline})),
            tool_use("Read", json!({"file_path": long_path})),
        ]);
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        let heredoc_target = ev[0].target.as_deref().unwrap();
        assert!(heredoc_target.len() <= MAX_COMMAND_TARGET_BYTES, "{heredoc_target}");
        assert!(heredoc_target.starts_with("cat <<'EOF' … [+"), "{heredoc_target}");
        assert!(!heredoc_target.contains('\n'), "{heredoc_target}");

        let inline_target = ev[1].target.as_deref().unwrap();
        assert!(inline_target.len() <= MAX_COMMAND_TARGET_BYTES, "{inline_target}");
        assert!(inline_target.contains(" … [+"), "{inline_target}");

        assert_eq!(
            ev[2].target.as_deref(),
            Some(long_path.as_str()),
            "file paths must stay whole above 512 bytes"
        );

        let mut m = Message::new(Role::Tool);
        m.content = vec![
            Block::ToolResult {
                tool_use_id: "t".into(),
                content: "e".repeat(1000).into(),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
            Block::ToolResult {
                tool_use_id: "t2".into(),
                content: "🦀".repeat(1000).into(),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ];
        let ev = extract(&m, 0, None, &crate::lazy::Resolver::new(None)).unwrap();
        assert_eq!(ev[0].detail, Some("e".repeat(1000)));
        assert_eq!(ev[1].detail.as_ref().unwrap().len(), MAX_ERROR_DETAIL_BYTES);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn rev_target_migrates_legacy_rows_and_uses_covering_btree_index() {
        let _world = crate::test_world("events-rev-target");
        {
            let conn = crate::catalog::open_db().unwrap();
            conn.execute_batch(
                "CREATE TABLE event_sessions(
                     id INTEGER PRIMARY KEY, selector TEXT NOT NULL UNIQUE,
                     harness TEXT NOT NULL, session_id TEXT NOT NULL,
                     parent_id TEXT, agent_id TEXT, workflow TEXT
                 );
                 CREATE TABLE event_targets(
                     id INTEGER PRIMARY KEY,
                     target TEXT NOT NULL UNIQUE
                 );
                 CREATE TABLE events(
                     session INTEGER NOT NULL, msg_idx INTEGER NOT NULL, ts INTEGER,
                     kind TEXT NOT NULL, tool TEXT, target_id INTEGER, detail TEXT, hunk_hashes BLOB
                 );
                 INSERT INTO event_sessions(id, selector, harness, session_id) VALUES(1, 'leg', 'claude', 'leg');
                 INSERT INTO event_targets(id, target) VALUES(1, '/work/%2Fweird_dir/src/ir.rs');
                 WITH RECURSIVE n(i) AS (VALUES(2) UNION ALL SELECT i+1 FROM n WHERE i<300)
                 INSERT INTO event_targets(id,target) SELECT i, '/old/' || i FROM n;
                 INSERT INTO events(session, msg_idx, ts, kind, tool, target_id) VALUES(1, 0, 10, 'file_edit', 'Edit', 1);",
            )
            .unwrap();
        }
        {
            let _frozen = (*crate::env::Env::current()).clone().with_frozen().enter();
            assert_eq!(sessions_touching("src/ir.rs", true).unwrap()[0].edits, 1);
        }
        let touched = sessions_touching("src/ir.rs", true).unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "leg");
        let weird = sessions_touching("%2Fweird_dir/src/ir.rs", true).unwrap();
        assert_eq!(weird.len(), 1);

        // #tooth The indexed suffix keeps LIKE's ASCII case folding and literal metacharacters.
        let folded = "%2fWEIRD_dir/SRC/IR.RS";
        assert_eq!(sessions_touching(folded, true).unwrap().len(), 1);
        assert_eq!(edits_touching_file("", folded).unwrap().len(), 1);
        assert_eq!(edits_with_hunks_touching_file("", folded).unwrap().len(), 1);
        let conn = db::open().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(rev_target) FROM event_targets", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            300
        );
        conn.execute_batch(
            "INSERT INTO event_targets(id,target) VALUES (301,'/other/%2Fweird_dir/src/ir.rs');
             INSERT INTO events(session,msg_idx,kind,target_id) VALUES (1,1,'file_edit',301);",
        )
        .unwrap();
        {
            let _frozen = (*crate::env::Env::current()).clone().with_frozen().enter();
            // #tooth A legacy writer can add a NULL after migration. A read-only query sees both edits once.
            assert_eq!(sessions_touching(folded, true).unwrap()[0].edits, 2);
        }
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM event_targets \
                 WHERE rev_target LIKE 'sr.ri/crs/%' ESCAPE '\\'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("idx_event_targets_suffix") && !joined.contains("SCAN"),
            "expected B-tree index range scan without table SCAN, got: {joined}"
        );
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

    /// #tooth Two profiles' copies of one native id are two event outputs: ingesting the second
    /// leaves the first's rows, a selector reads exactly one copy, the native id reads both, and
    /// a catalog keyed the old way by `(harness, session_id)` is rebuilt once instead of erroring.
    #[cfg(feature = "sqlite")]
    #[test]
    fn profile_twins_with_one_native_id_keep_separate_event_rows() {
        let world = crate::test_world("events-twins");
        {
            // An event catalog from before selectors, with a row and its baseline.
            let conn = crate::catalog::open_db().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS event_sessions(id INTEGER PRIMARY KEY, harness TEXT NOT NULL,
                     session_id TEXT NOT NULL, parent_id TEXT, agent_id TEXT, workflow TEXT,
                     UNIQUE(harness, session_id));
                 INSERT INTO event_sessions(harness, session_id) VALUES('claude', 'legacy');",
            )
            .unwrap();
        }
        let twin = |profile: &str, target: &str| {
            let dir = world.base.join(profile).join("projects/p");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("twin.jsonl");
            let line = serde_json::json!({"type":"assistant","uuid":"a","sessionId":"twin",
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Edit",
                    "input":{"file_path":target,"old_string":"a","new_string":"b"}}]}});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            SessionRef::new("twin", crate::Harness::Claude, path)
        };
        let a = twin(".claude", "/repo/alpha_twin.rs");
        let b = twin(".claude-alt", "/repo/beta_twin.rs");
        assert_ne!(a.session_selector(), b.session_selector());
        let targets = |id: &str| -> Vec<String> {
            events_for("claude", id, Some("file_edit"))
                .unwrap()
                .into_iter()
                .filter_map(|e| e.target)
                .collect()
        };
        ingest_ref(&a).unwrap();
        ingest_ref(&b).unwrap();
        assert_eq!(targets(&a.session_selector()), ["/repo/alpha_twin.rs"]);
        assert_eq!(targets(&b.session_selector()), ["/repo/beta_twin.rs"]);
        assert_eq!(targets("twin").len(), 2, "the native id reads every copy");
        for file in ["/repo/alpha_twin.rs", "/repo/beta_twin.rs"] {
            let touched = sessions_editing(file).unwrap();
            assert_eq!(touched.len(), 1, "{file}: {touched:?}");
            assert_eq!(touched[0].session_id, "twin", "output keeps the native id");
            assert_eq!(touched[0].edits, 1);
        }
        assert!(
            targets("legacy").is_empty(),
            "the old-keyed rows were dropped, not kept"
        );
        let mut probe = crate::source::Probe::default();
        let sync = SyncTable::load();
        for r in [&a, &b] {
            let source = probe.snapshot(r);
            assert!(!sync.needs_ingest(r, &source, &Provenance::default(), &mut probe));
        }
        let sources = db::preferred_sources([(a.clone(), Provenance::default()), (b.clone(), Provenance::default())]);
        assert_eq!(sources.len(), 2, "each twin is its own output");

        // #tooth Profile twins sharing `(harness, id)` keep separate `title`, `cwd`, and
        // timestamps across `sessions_touching`, `edits_touching_file`, and `edits_with_hunks_touching_file`.
        let mut a_cat = a.clone();
        a_cat.title = Some("Alpha Title".into());
        a_cat.cwd = Some("/work/alpha".into());
        a_cat.created_at = chrono::DateTime::from_timestamp(100, 0);
        a_cat.updated_at = chrono::DateTime::from_timestamp(200, 0);
        let mut b_cat = b.clone();
        b_cat.title = Some("Zeta Title".into());
        b_cat.cwd = Some("/work/zeta".into());
        b_cat.created_at = chrono::DateTime::from_timestamp(300, 0);
        b_cat.updated_at = chrono::DateTime::from_timestamp(400, 0);
        crate::catalog::sync(&[a_cat, b_cat]);

        let touched_a = sessions_editing("/repo/alpha_twin.rs").unwrap();
        assert_eq!(touched_a[0].title.as_deref(), Some("Alpha Title"));
        let touched_b = sessions_editing("/repo/beta_twin.rs").unwrap();
        assert_eq!(touched_b[0].title.as_deref(), Some("Zeta Title"));

        let edits_a = edits_touching_file("/repo/alpha_twin.rs", "alpha_twin.rs").unwrap();
        assert_eq!(edits_a.len(), 1);
        assert_eq!(edits_a[0].title.as_deref(), Some("Alpha Title"));
        assert_eq!(edits_a[0].cwd.as_deref(), Some("/work/alpha"));
        assert_eq!(edits_a[0].created_at, Some(100));
        assert_eq!(edits_a[0].updated_at, Some(200));

        let hunks_a = edits_with_hunks_touching_file("/repo/alpha_twin.rs", "alpha_twin.rs").unwrap();
        assert_eq!(hunks_a.len(), 1);
        assert_eq!(hunks_a[0].edit.title.as_deref(), Some("Alpha Title"));
        assert_eq!(hunks_a[0].edit.cwd.as_deref(), Some("/work/alpha"));
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
        // Two copies of one session in one profile (two of its project dirs): one event output.
        for (name, timestamp) in [("older", 1), ("newer", 2)] {
            let dir = world.base.join("profile/projects").join(name);
            std::fs::create_dir_all(&dir).unwrap();
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

        // 64-bit hunk hashes persist across SQLite ingest and match re-indented git diff hunks.
        let hunk_edits = edits_with_hunks_touching_file("/elsewhere/src/parser.rs", "src/parser.rs").unwrap();
        assert_eq!(hunk_edits.len(), 1);
        assert_eq!(hunk_edits[0].edit.session_id, "evt-e2e");
        let per_sess = edit_hunks_for("claude", "evt-e2e").unwrap();
        assert_eq!(per_sess.len(), 1);
        assert_eq!(per_sess[0].2, hunk_edits[0].hunk_hashes);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn persisted_edit_hunk_hashes_match_rebased_diff_and_suffix_replacement() {
        let world = crate::test_world("events-hunk-hashes");
        let path = world.write_session(
            "hunk-sess",
            &[json!({
                "type": "assistant", "uuid": "a1", "sessionId": "hunk-sess",
                "timestamp": "2026-07-01T10:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use", "id": "t1", "name": "Edit",
                        "input": {
                            "file_path": "/repo/src/engine.rs",
                            "old_string": "let cap = 8;",
                            "new_string": "let cap = compute_dynamic_cap(limit);\nassert!(cap >= 16);"
                        }
                    }]
                }
            })],
        );
        let r = SessionRef::new("hunk-sess", crate::Harness::Claude, &path);
        assert_eq!(ingest_ref(&r).unwrap(), 1);

        let stored = edits_with_hunks_touching_file("/other-worktree/src/engine.rs", "src/engine.rs").unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].hunk_hashes.is_empty());

        let rebased_commit_diff = "\
--- a/src/engine.rs
+++ b/src/engine.rs
@@ -88,1 +120,2 @@
-        let cap = 8;
+        let cap = compute_dynamic_cap(limit);
+        assert!(cap >= 16);
";
        let diff_hashes = extract_diff_hunk_hashes(rebased_commit_diff);
        assert_eq!(stored[0].hunk_hashes, diff_hashes);
    }

    #[test]
    fn antigravity_and_pi_tools_extract_events_and_hunk_fingerprints() {
        let cwd = Path::new("/repo");
        let m = msg(vec![
            // Antigravity 1.0.x / 1.5.x reads & searches
            tool_use("read_file", json!({"Path": "src/lib.rs"})),
            tool_use("view_file", json!({"AbsolutePath": "/repo/src/view.rs"})),
            tool_use("view_file", json!({"AbsolutePathUri": "file:///repo/src/uri.rs"})),
            tool_use("view_file_outline", json!({"AbsolutePath": "/repo/src/outline.rs"})),
            tool_use(
                "view_code_item",
                json!({"File": "/repo/src/item.rs", "NodePaths": ["Foo"]}),
            ),
            tool_use("grep_search", json!({"Query": "parse", "SearchPath": "crates"})),
            tool_use("find_by_name", json!({"Pattern": "*.rs", "SearchDirectory": "src"})),
            tool_use("list_dir", json!({"DirectoryPath": "docs"})),
            tool_use("search_web", json!({"query": "rust", "domain": "docs.rs"})),
            tool_use("read_url_content", json!({"Url": "https://docs.rs"})),
            // Antigravity 1.0.x / 1.5.x edits
            tool_use(
                "write_file",
                json!({"Path": "src/a.rs", "Content": "pub fn agy_write() {}\n"}),
            ),
            tool_use(
                "write_to_file",
                json!({"TargetFile": "src/b.rs", "CodeContent": "pub fn agy_write_to() {}\n"}),
            ),
            tool_use(
                "edit_file",
                json!({"Path": "src/c.rs", "Target": "old()", "Replacement": "agy_edit_file();"}),
            ),
            tool_use(
                "replace_file_content",
                json!({
                    "TargetFile": "src/d.rs",
                    "TargetContent": "old()",
                    "ReplacementContent": "agy_replace_content();"
                }),
            ),
            tool_use(
                "multi_replace_file_content",
                json!({
                    "TargetFile": "src/e.rs",
                    "ReplacementChunks": [
                        {"TargetContent": "x", "ReplacementContent": "agy_multi_chunk_one();"},
                        {"TargetContent": "y", "ReplacementContent": "agy_multi_chunk_two();"}
                    ]
                }),
            ),
            // Antigravity commands & background controls
            tool_use("run_command", json!({"CommandLine": "cargo test", "Cwd": "/repo"})),
            tool_use("manage_task", json!({"Action": "status", "TaskId": "s/task-1"})),
            tool_use("command_status", json!({"CommandId": "cmd-1"})),
            tool_use("send_command_input", json!({"CommandId": "cmd-1", "Terminate": true})),
            // Pi v3 tools
            tool_use("bash", json!({"command": "cargo check", "timeout": 10})),
            tool_use("read", json!({"path": "src/pi_read.rs", "offset": 1, "limit": 20})),
            tool_use(
                "write",
                json!({"path": "src/pi_write.rs", "content": "pub fn pi_written() {}\n"}),
            ),
            tool_use(
                "edit",
                json!({"path": "src/pi_edit1.rs", "oldText": "a", "newText": "pi_single_edit_line();"}),
            ),
            tool_use(
                "edit",
                json!({
                    "path": "src/pi_edit2.rs",
                    "edits": [{"oldText": "b", "newText": "pi_multi_edit_line();"}]
                }),
            ),
            tool_use("grep", json!({"pattern": "fn main", "path": "src"})),
            tool_use("find", json!({"pattern": "*.rs", "path": "crates"})),
            tool_use("ls", json!({"path": "tests"})),
        ]);

        let ev = extract(&m, 0, Some(cwd), &crate::lazy::Resolver::new(None)).unwrap();
        let summary: Vec<(&str, Option<&str>, Option<&str>, bool)> = ev
            .iter()
            .map(|e| {
                (
                    e.kind,
                    e.tool.as_deref(),
                    e.target.as_deref(),
                    !e.hunk_hashes.is_empty(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("file_read", Some("read_file"), Some("/repo/src/lib.rs"), false),
                ("file_read", Some("view_file"), Some("/repo/src/view.rs"), false),
                ("file_read", Some("view_file"), Some("/repo/src/uri.rs"), false),
                (
                    "file_read",
                    Some("view_file_outline"),
                    Some("/repo/src/outline.rs"),
                    false
                ),
                ("file_read", Some("view_code_item"), Some("/repo/src/item.rs"), false),
                ("file_read", Some("grep_search"), Some("/repo/crates"), false),
                ("file_read", Some("find_by_name"), Some("/repo/src"), false),
                ("file_read", Some("list_dir"), Some("/repo/docs"), false),
                ("tool", Some("search_web"), None, false),
                ("tool", Some("read_url_content"), None, false),
                ("file_edit", Some("write_file"), Some("/repo/src/a.rs"), true),
                ("file_edit", Some("write_to_file"), Some("/repo/src/b.rs"), true),
                ("file_edit", Some("edit_file"), Some("/repo/src/c.rs"), true),
                ("file_edit", Some("replace_file_content"), Some("/repo/src/d.rs"), true),
                (
                    "file_edit",
                    Some("multi_replace_file_content"),
                    Some("/repo/src/e.rs"),
                    true
                ),
                ("command", Some("run_command"), Some("cargo test"), false),
                ("tool", Some("manage_task"), None, false),
                ("tool", Some("command_status"), None, false),
                ("tool", Some("send_command_input"), None, false),
                ("command", Some("bash"), Some("cargo check"), false),
                ("file_read", Some("read"), Some("/repo/src/pi_read.rs"), false),
                ("file_edit", Some("write"), Some("/repo/src/pi_write.rs"), true),
                ("file_edit", Some("edit"), Some("/repo/src/pi_edit1.rs"), true),
                ("file_edit", Some("edit"), Some("/repo/src/pi_edit2.rs"), true),
                ("file_read", Some("grep"), Some("/repo/src"), false),
                ("file_read", Some("find"), Some("/repo/crates"), false),
                ("file_read", Some("ls"), Some("/repo/tests"), false),
            ]
        );
    }
}
