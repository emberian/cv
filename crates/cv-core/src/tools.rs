//! Cross-agent tool analytics: which tools an agent used, which agents used a tool, and aggregate
//! tool usage across a workflow or the whole sub-agent forest.
//!
//! The per-agent event extraction ([`crate::events`]) already classifies every `tool_use` into a
//! normalized row; this module is the *query/aggregation* surface over it. A [`ToolHistogram`] is
//! the per-(agent-or-session) tally — raw tool name → count, with the file_edit/file_read/command
//! split — and [`ForestTools`] holds one per agent across a forest so you can ask both directions:
//! "which tools did agent X use" (one histogram) and "which agents used tool T" (a scan across
//! them). A flat [`ToolEvent`] timeline preserves call order for `--timeline`.
//!
//! Histograms fold each message's event rows immediately, retaining only per-tool counts. Tool
//! results remain lazy on disk. Timelines retain the call rows their output requires.

use crate::events::{self, Event};
use crate::ir::{Block, Message, SessionRef};
use crate::stream::{Demand, Flow, MessageSink};
use std::collections::BTreeMap;

pub mod classify;

/// A per-tool tally with the kind breakdown the event classifier assigns.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct ToolCount {
    /// Total invocations of this tool.
    pub calls: usize,
    /// How many were classified `file_edit`.
    pub edits: usize,
    /// How many were classified `file_read`.
    pub reads: usize,
    /// How many were classified `command`.
    pub commands: usize,
    /// How many of this tool's *results* came back flagged as errors.
    pub errors: usize,
}

/// A tool-usage histogram for one agent (or session): raw tool name → [`ToolCount`].
///
/// `BTreeMap` so iteration / serialization is deterministic (alphabetical); callers wanting a
/// frequency ranking use [`ranked`](ToolHistogram::ranked).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolHistogram {
    pub tools: BTreeMap<String, ToolCount>,
    /// Total tool calls across all tools (sum of `tools[*].calls`).
    pub total_calls: usize,
    /// Total error results observed (sum of `tools[*].errors` plus errors whose tool was unknown).
    pub total_errors: usize,
}

impl ToolHistogram {
    /// Fold a slice of classified [`Event`]s into a histogram. A tool's `calls` counts its
    /// `tool_use` blocks ([`Event::call`]), as `cv doctor` does; the kind counts count rows, so a
    /// three-file patch through the shell is one call, one command and three edits. `error` rows
    /// increment the error count for their tool (or the unknown-tool error total).
    pub fn from_events(evs: &[Event]) -> Self {
        let mut h = ToolHistogram::default();
        for e in evs {
            h.note(e);
        }
        h
    }

    fn note(&mut self, e: &Event) {
        match e.kind {
            "error" => {
                self.total_errors += 1;
                if let Some(t) = &e.tool {
                    self.tools.entry(t.clone()).or_default().errors += 1;
                }
            }
            kind => {
                let Some(name) = &e.tool else { return };
                let c = self.tools.entry(name.clone()).or_default();
                if e.call {
                    c.calls += 1;
                    self.total_calls += 1;
                }
                match kind {
                    "file_edit" => c.edits += 1,
                    "file_read" => c.reads += 1,
                    "command" => c.commands += 1,
                    _ => {}
                }
            }
        }
    }

    fn note_message(
        &mut self,
        m: &Message,
        idx: usize,
        cwd: Option<&std::path::Path>,
        scratch: &mut Vec<Event>,
        tool: Option<&str>,
        extractor: &mut events::EventExtractor,
    ) -> anyhow::Result<()> {
        extractor.extract(m, idx, cwd, tool, None, scratch)?;
        for event in scratch.iter() {
            self.note(event);
        }
        events::release_scratch(scratch);
        Ok(())
    }

    /// Build from already-parsed messages, dropping each message's extracted event rows after
    /// tallying them. The caller's messages are borrowed; no transcript or event list is cloned.
    pub fn from_messages<'a>(
        msgs: impl IntoIterator<Item = &'a Message>,
        cwd: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let mut h = Self::default();
        let mut scratch = Vec::new();
        let mut extractor = events::EventExtractor::default();
        for (i, m) in msgs.into_iter().enumerate() {
            h.note_message(m, i, cwd, &mut scratch, None, &mut extractor)?;
        }
        Ok(h)
    }

    /// Stream one transcript into its histogram, retaining only distinct tool counts. Unreadable
    /// transcripts return an error, including after a partial read; no partial histogram escapes.
    pub fn from_stream(adapter: &dyn crate::Adapter, r: &SessionRef) -> anyhow::Result<Self> {
        Self::from_stream_selected(adapter, r, None)
    }
    fn from_stream_selected(adapter: &dyn crate::Adapter, r: &SessionRef, tool: Option<&str>) -> anyhow::Result<Self> {
        let mut sink = HistogramSink {
            state: SinkState::new(r.cwd.clone()),
            msg_idx: 0,
            histogram: Self::default(),
            extractor: events::EventExtractor::default(),
            tool: tool.map(str::to_owned),
            scratch: Vec::new(),
        };
        adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut sink)?;
        Ok(sink.histogram)
    }

    /// Tools ranked by call count (descending; ties broken alphabetically) — the natural display
    /// order for a histogram.
    pub fn ranked(&self) -> Vec<(&str, &ToolCount)> {
        let mut v: Vec<(&str, &ToolCount)> = self.tools.iter().map(|(k, c)| (k.as_str(), c)).collect();
        v.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// Number of distinct tools used.
    pub fn distinct(&self) -> usize {
        self.tools.len()
    }

    /// Whether tool `name` was used at all.
    pub fn used(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|c| c.calls > 0)
    }

    /// Merge another histogram into this one (for aggregating across agents).
    pub fn merge(&mut self, other: &ToolHistogram) {
        for (name, oc) in &other.tools {
            let c = self.tools.entry(name.clone()).or_default();
            c.calls += oc.calls;
            c.edits += oc.edits;
            c.reads += oc.reads;
            c.commands += oc.commands;
            c.errors += oc.errors;
        }
        self.total_calls += other.total_calls;
        self.total_errors += other.total_errors;
    }
}

/// What the tool sinks share: EventSink's cwd rule (a discovery cwd wins; otherwise the first
/// pushed cwd wins) and the one-error latch that stops the pass.
struct SinkState {
    cwd: Option<std::path::PathBuf>,
    fail: Option<anyhow::Error>,
}

impl SinkState {
    fn new(cwd: Option<std::path::PathBuf>) -> Self {
        SinkState { cwd, fail: None }
    }

    fn meta(&mut self, session: &crate::Session) {
        if self.cwd.is_none() {
            self.cwd = session.cwd.clone();
        }
    }

    fn stop(&mut self, error: anyhow::Error) -> Flow {
        self.fail = Some(error);
        Flow::Stop
    }

    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }
}

struct HistogramSink {
    state: SinkState,
    extractor: events::EventExtractor,
    msg_idx: usize,
    histogram: ToolHistogram,
    tool: Option<String>,
    scratch: Vec<Event>,
}

impl MessageSink for HistogramSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.state.take_error()
    }

    fn meta(&mut self, session: &crate::Session) {
        self.state.meta(session)
    }

    fn message(&mut self, message: Message) -> Flow {
        if let Err(error) = self.histogram.note_message(
            &message,
            self.msg_idx,
            self.state.cwd.as_deref(),
            &mut self.scratch,
            self.tool.as_deref(),
            &mut self.extractor,
        ) {
            return self.state.stop(error);
        }
        self.msg_idx += 1;
        Flow::Continue
    }
}

/// One tool call on a timeline: which agent, which tool, when, what kind/target. The ordered form
/// used by `cv tools --timeline`: one per `tool_use` block, so the timeline has as many entries as
/// [`ToolHistogram::total_calls`] counts. A call that touched several targets (a multi-file patch)
/// keeps its first row's `kind`/`target` and the rest in `more_targets`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolEvent {
    /// Identifier of the agent that made the call (`<orchestrator>` for the parent session itself,
    /// else the agent's short id).
    pub agent: String,
    /// Index of the message within that agent's transcript.
    pub msg_idx: usize,
    /// Unix seconds of the call's message, when timestamped.
    pub ts: Option<i64>,
    /// The call's `tool_use` id: what `cv cat <session> <id>` takes.
    pub tool_use_id: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The targets of the call's further rows (the second and later files of a patch).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub more_targets: Vec<String>,
}

/// A sink that classifies each `tool_use` block on its own, so every row is paired with the id of
/// the call it came from. (The rows themselves are [`events::extract`]'s; results are skipped,
/// since the timeline is calls.)
struct CallSink {
    state: SinkState,
    msg_idx: usize,
    rows: Vec<(String, Event)>,
    tool: Option<String>,
}

impl CallSink {
    fn new(cwd: Option<std::path::PathBuf>) -> Self {
        CallSink {
            state: SinkState::new(cwd),
            msg_idx: 0,
            rows: Vec::new(),
            tool: None,
        }
    }
}

impl MessageSink for CallSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.state.take_error()
    }

    fn meta(&mut self, s: &crate::ir::Session) {
        self.state.meta(s)
    }

    fn message(&mut self, m: Message) -> Flow {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                if self.tool.as_deref().is_some_and(|tool| tool != name) {
                    continue;
                }
                let mut rows = Vec::new();
                if let Err(error) = events::extract_call_into(
                    name,
                    input,
                    self.msg_idx,
                    m.timestamp.map(|t| t.timestamp()),
                    self.state.cwd.as_deref(),
                    true,
                    &mut rows,
                ) {
                    return self.state.stop(error);
                }
                self.rows.extend(rows.into_iter().map(|e| (id.clone(), e)));
            }
        }
        self.msg_idx += 1;
        Flow::Continue
    }
}

/// One agent's classified rows as timeline entries, one per call: a row that continues a call
/// ([`Event::call`] false) folds into the entry before it. Error rows are results, not calls.
fn timeline_calls(agent: &str, rows: Vec<(String, Event)>, out: &mut Vec<ToolEvent>) {
    let first = out.len();
    for (id, e) in rows {
        if e.kind == "error" {
            continue;
        }
        if !e.call {
            if let Some(prev) = out[first..].last_mut().filter(|p| p.tool_use_id == id) {
                prev.more_targets.extend(e.target);
                continue;
            }
        }
        out.push(ToolEvent {
            agent: agent.to_string(),
            msg_idx: e.msg_idx,
            ts: e.ts,
            tool_use_id: id,
            kind: e.kind,
            tool: e.tool,
            target: e.target,
            more_targets: Vec::new(),
        });
    }
}

/// Per-agent tool histograms across a whole sub-agent forest (plus the orchestrator), each tagged
/// with the agent's identity/role so both query directions are answerable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForestTools {
    /// `(agent-label, agent-type, workflow-or-None, histogram)`, one per agent (orchestrator first).
    pub agents: Vec<AgentTools>,
}

/// One agent's identity + its tool histogram within a forest aggregation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentTools {
    /// The agent's id (`<orchestrator>` for the parent, else the bare agentId).
    pub agent: String,
    /// The agent's type (`general-purpose`/`workflow-subagent`/…); `orchestrator` for the parent.
    pub agent_type: String,
    /// The workflow run id this agent belonged to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub histogram: ToolHistogram,
}

impl ForestTools {
    /// The aggregate histogram across every agent (orchestrator + forest) — "what did this whole
    /// session, including all its sub-agents, do with tools".
    pub fn aggregate(&self) -> ToolHistogram {
        let mut h = ToolHistogram::default();
        for a in &self.agents {
            h.merge(&a.histogram);
        }
        h
    }

    /// Agents that used tool `name`, with that tool's count for each — the "which agents used T"
    /// direction, ranked by count.
    pub fn agents_using<'a>(&'a self, name: &str) -> Vec<(&'a AgentTools, &'a ToolCount)> {
        let mut v: Vec<(&AgentTools, &ToolCount)> = self
            .agents
            .iter()
            .filter_map(|a| a.histogram.tools.get(name).map(|c| (a, c)))
            .filter(|(_, c)| c.calls > 0)
            .collect();
        v.sort_by(|x, y| y.1.calls.cmp(&x.1.calls).then_with(|| x.0.agent.cmp(&y.0.agent)));
        v
    }

    /// One agent's histogram by its full id (orchestrator via `<orchestrator>`), or `None`. A
    /// user-typed selector goes through [`resolve_agent`] first.
    pub fn agent(&self, id: &str) -> Option<&AgentTools> {
        self.agents.iter().find(|a| a.agent == id)
    }
}

/// The sentinel agent id for the parent session's own tool calls in a forest aggregation.
pub const ORCHESTRATOR: &str = "<orchestrator>";

/// Build the full per-agent tool analytics for a session: the orchestrator's own tools plus one
/// histogram per sub-agent in the forest (each streamed lazily — result bodies stay on disk).
///
/// This is the cross-agent surface: with it you can ask "which tools did agent X use"
/// ([`ForestTools::agent`]), "which agents used tool T" ([`ForestTools::agents_using`]), and the
/// aggregate across the whole forest ([`ForestTools::aggregate`]). A failed read in any agent returns an error; no partial forest is published.
pub fn forest_tools(r: &SessionRef) -> anyhow::Result<ForestTools> {
    forest_tools_selected(r, None, None, None)
}

/// Read only the selected agent/workflow's tool histograms. `agent` is a full agent id, as
/// [`resolve_agent`] returns it.
pub fn forest_tools_selected(
    r: &SessionRef,
    agent: Option<&str>,
    workflow: Option<&str>,
    tool: Option<&str>,
) -> anyhow::Result<ForestTools> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;

    let mut agents = Vec::new();

    // The orchestrator (the parent session) itself.
    if workflow.is_none() && matches_agent(agent, ORCHESTRATOR) {
        let histogram = ToolHistogram::from_stream_selected(adapter.as_ref(), r, tool)?;
        agents.push(AgentTools {
            agent: ORCHESTRATOR.to_string(),
            agent_type: "orchestrator".to_string(),
            workflow: None,
            histogram,
        });
    }

    // Every sub-agent in the forest (direct + workflow), each its own histogram.
    for s in selected_tool_agents(r, agent, workflow, crate::SubagentDemand::Tools)? {
        let hist = ToolHistogram::from_stream_selected(adapter.as_ref(), &s.session, tool)?;
        agents.push(AgentTools {
            agent: s.agent_id().to_string(),
            agent_type: s.agent_type.clone().unwrap_or_else(|| "agent".to_string()),
            workflow: s.workflow.clone(),
            histogram: hist,
        });
    }

    Ok(ForestTools { agents })
}

/// A flat, time-ordered tool-call timeline across the orchestrator + forest — `cv tools --timeline`.
/// Streams each agent, collects its tool rows tagged with the agent id, then sorts by `(ts,
/// agent, msg_idx)` so calls interleave in wall-clock order (untimestamped rows sort last, stably).
/// Selectors compose before reading transcripts or decoding call arguments. Workflow must be a
/// resolved run ID; agent a full agent id, as [`resolve_agent`] returns it.
pub fn forest_timeline_selected(
    r: &SessionRef,
    agent: Option<&str>,
    workflow: Option<&str>,
    tool: Option<&str>,
) -> anyhow::Result<Vec<ToolEvent>> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
    let mut out: Vec<ToolEvent> = Vec::new();

    if workflow.is_none() && matches_agent(agent, ORCHESTRATOR) {
        let mut sink = CallSink::new(r.cwd.clone());
        sink.tool = tool.map(str::to_owned);
        adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut sink)?;
        timeline_calls(ORCHESTRATOR, sink.rows, &mut out);
    }

    for s in selected_tool_agents(r, agent, workflow, crate::SubagentDemand::Sessions)? {
        let mut sink = CallSink::new(s.session.cwd.clone());
        sink.tool = tool.map(str::to_owned);
        adapter.try_stream(&s.session, &Demand::LEAN.with_spans(), &mut sink)?;
        timeline_calls(s.agent_id(), sink.rows, &mut out);
    }

    out.sort_by(|a, b| match (a.ts, b.ts) {
        (Some(x), Some(y)) => x
            .cmp(&y)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.msg_idx.cmp(&b.msg_idx)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.agent.cmp(&b.agent).then_with(|| a.msg_idx.cmp(&b.msg_idx)),
    });
    Ok(out)
}

fn matches_agent(agent: Option<&str>, id: &str) -> bool {
    agent.is_none_or(|key| id == key)
}

/// An `--agent` selector that names no agent, or more than one: the same two answers the session
/// selector gives, and the CLI exits 2 on both.
#[derive(Debug)]
pub enum AgentSelectorError {
    None { key: String },
    Ambiguous { key: String, candidates: Vec<String> },
}

impl std::fmt::Display for AgentSelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None { key } => write!(f, "no agent matching {key:?}"),
            Self::Ambiguous { key, candidates } => write!(
                f,
                "ambiguous agent id {key:?} matches {} agents: {} — pass a longer id",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for AgentSelectorError {}

/// The one `--agent` grammar: an exact agent id (`<orchestrator>` included), else a unique prefix
/// of an id (a leading `agent-` is optional), among the orchestrator and the forest (the named
/// workflow's agents when `workflow` is set). Returns the full id; a selector that matches none
/// or several is an [`AgentSelectorError`].
pub fn resolve_agent(r: &SessionRef, key: &str, workflow: Option<&str>) -> anyhow::Result<String> {
    let bare = key.strip_prefix("agent-").unwrap_or(key);
    let mut ids: Vec<String> = Vec::new();
    if workflow.is_none() {
        ids.push(ORCHESTRATOR.to_string());
    }
    ids.extend(
        crate::subagent_tree_filtered_of(r, crate::SubagentDemand::Sessions, workflow, |_, run| {
            workflow.is_none_or(|wanted| run == Some(wanted))
        })?
        .iter()
        .map(|s| s.agent_id().to_string()),
    );
    ids.sort();
    ids.dedup();
    if ids.iter().any(|id| id == key) {
        return Ok(key.to_string());
    }
    let mut hits: Vec<String> = ids
        .into_iter()
        .filter(|id| id.starts_with(bare) || id.starts_with(key))
        .collect();
    match hits.len() {
        0 => Err(AgentSelectorError::None { key: key.to_string() }.into()),
        1 => Ok(hits.remove(0)),
        _ => Err(AgentSelectorError::Ambiguous {
            key: key.to_string(),
            candidates: hits,
        }
        .into()),
    }
}

fn selected_tool_agents(
    r: &SessionRef,
    agent: Option<&str>,
    workflow: Option<&str>,
    demand: crate::SubagentDemand,
) -> anyhow::Result<Vec<crate::AgentRef>> {
    if agent == Some(ORCHESTRATOR) && workflow.is_none() {
        return Ok(Vec::new());
    }
    let workflow_member = std::sync::atomic::AtomicBool::new(false);
    let selected = crate::subagent_tree_filtered_of(r, demand, workflow, |id, run| {
        let matches_workflow = workflow.is_none_or(|wanted| run == Some(wanted));
        if matches_workflow {
            workflow_member.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        matches_workflow && matches_agent(agent, id.strip_prefix("agent-").unwrap_or(id))
    })?;
    if let Some(workflow) = workflow {
        let members = workflow_member.load(std::sync::atomic::Ordering::Relaxed);
        workflow_selection(&r.id, workflow, agent, members, !selected.is_empty())?;
    }
    Ok(selected)
}

/// Refuse a workflow selection that found nothing, naming the selector that failed: a workflow
/// with no agents, or an agent (other than the orchestrator, which no workflow contains) that is
/// not one of the workflow's.
fn workflow_selection(
    session: &str,
    workflow: &str,
    agent: Option<&str>,
    members: bool,
    selected: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(members, "no agents found for workflow {workflow:?} in {session}");
    match agent {
        Some(agent) if !selected && agent != ORCHESTRATOR => {
            anyhow::bail!("no agent {agent:?} in workflow {workflow:?} in {session}")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_empty_workflow_selection_names_the_selector_that_failed() {
        use super::workflow_selection as check;
        let message = |r: anyhow::Result<()>| r.unwrap_err().to_string();
        assert_eq!(
            message(check("s", "wf", None, false, false)),
            "no agents found for workflow \"wf\" in s"
        );
        assert_eq!(
            message(check("s", "wf", Some("a"), false, false)),
            "no agents found for workflow \"wf\" in s"
        );
        assert_eq!(
            message(check("s", "wf", Some("a"), true, false)),
            "no agent \"a\" in workflow \"wf\" in s"
        );
        check("s", "wf", Some("a"), true, true).unwrap();
        check("s", "wf", Some(super::ORCHESTRATOR), true, false).unwrap();
        check("s", "wf", None, true, true).unwrap();
    }

    #[test]
    fn selected_tools_preserve_unfiltered_native_edit_attribution() {
        let session = crate::harness::codex::parse_str(
            "direct",
            include_str!("../tests/fixtures/codex/direct_patch.jsonl"),
            true,
            None,
        )
        .unwrap();
        let mut messages = session.messages.clone();
        let mut visible = crate::Message::new(crate::Role::Assistant);
        visible.content.push(crate::Block::ToolUse {
            id: "visible-patch".into(),
            name: "apply_patch".into(),
            namespace: None,
            input: serde_json::json!({"patch":"*** Begin Patch\n*** Add File: /repo/visible.txt\n+ok\n*** End Patch"})
                .into(),
        });
        messages.insert(0, visible);
        let all = ToolHistogram::from_messages(&messages, None).unwrap();
        assert_eq!(all.tools["apply_patch"].calls, 1);
        assert_eq!(all.tools["apply_patch"].edits, 1);
        for tool in ["apply_patch", "exec_command"] {
            let mut selected = ToolHistogram::default();
            let mut extractor = events::EventExtractor::default();
            let mut scratch = Vec::new();
            for (idx, message) in messages.iter().enumerate() {
                selected
                    .note_message(message, idx, None, &mut scratch, Some(tool), &mut extractor)
                    .unwrap();
            }
            assert_eq!(selected.tools.len(), 1);
            assert_eq!(selected.tools[tool], all.tools[tool]);
        }
    }

    use super::*;
    use crate::ir::{Block, Message, Role};
    use serde_json::json;

    fn asst(blocks: Vec<Block>) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content = blocks;
        m
    }
    fn with_id(mut b: Block, call: &str) -> Block {
        if let Block::ToolUse { id, .. } = &mut b {
            *id = call.into();
        }
        b
    }
    fn tool_use(name: &str, input: serde_json::Value) -> Block {
        Block::ToolUse {
            id: "t".into(),
            name: name.into(),
            input: input.into(),
            namespace: None,
        }
    }

    #[test]
    fn histogram_stream_retains_only_distinct_tools_and_keeps_meta_cwd_rules() {
        let mut sink = HistogramSink {
            extractor: events::EventExtractor::default(),
            state: SinkState::new(None),
            msg_idx: 0,
            histogram: ToolHistogram::default(),
            tool: None,
            scratch: Vec::new(),
        };
        let mut shell = crate::Session::new("histogram", crate::Harness::Claude);
        shell.cwd = Some("/first".into());
        sink.meta(&shell);
        shell.cwd = Some("/later".into());
        sink.meta(&shell);
        assert_eq!(sink.state.cwd.as_deref(), Some(std::path::Path::new("/first")));
        for i in 0..2000 {
            let m = asst(vec![tool_use("Edit", json!({"file_path":format!("unique-{i}.rs")}))]);
            sink.message(m);
        }
        assert_eq!(sink.histogram.distinct(), 1);
        assert_eq!(sink.histogram.total_calls, 2000);
        assert_eq!(sink.histogram.tools["Edit"].edits, 2000);
        assert!(sink.scratch.is_empty());
        assert!(sink.scratch.capacity() <= 64);
        let burst = asst(
            (0..200)
                .map(|i| tool_use("Edit", json!({"file_path":format!("burst-{i}.rs")})))
                .collect(),
        );
        sink.message(burst);
        assert_eq!(sink.histogram.total_calls, 2200);
        assert_eq!(sink.scratch.capacity(), 0);
        let mut discovered = HistogramSink {
            extractor: events::EventExtractor::default(),
            state: SinkState::new(Some("/discovery".into())),
            msg_idx: 0,
            histogram: ToolHistogram::default(),
            tool: None,
            scratch: Vec::new(),
        };
        discovered.meta(&shell);
        assert_eq!(
            discovered.state.cwd.as_deref(),
            Some(std::path::Path::new("/discovery"))
        );
    }

    #[test]
    fn histogram_stream_returns_error_instead_of_partial_counts() {
        struct Fails;
        impl crate::Adapter for Fails {
            fn harness(&self) -> crate::Harness {
                crate::Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                vec![]
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                _r: &SessionRef,
                _opts: &Demand,
                sink: &mut dyn MessageSink,
            ) -> anyhow::Result<crate::Session> {
                sink.message(asst(vec![tool_use("Read", json!({"file_path":"a.rs"}))]));
                anyhow::bail!("fixture read error")
            }
        }
        let r = SessionRef::new("failure", crate::Harness::Claude, "unused");
        assert!(ToolHistogram::from_stream(&Fails, &r).is_err());
    }

    #[test]
    fn histogram_tallies_kinds_and_ranks() {
        let msgs = [
            asst(vec![
                tool_use(
                    "Edit",
                    json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"}),
                ),
                tool_use("Read", json!({"file_path": "b.rs"})),
                tool_use("Bash", json!({"command": "ls"})),
            ]),
            asst(vec![
                tool_use("Bash", json!({"command": "cargo test"})),
                tool_use("Bash", json!({"command": "git status"})),
            ]),
        ];
        let h = ToolHistogram::from_messages(msgs.iter(), Some(std::path::Path::new("/repo"))).unwrap();
        assert_eq!(h.total_calls, 5);
        assert_eq!(h.distinct(), 3);
        assert!(h.used("Bash"));
        assert!(!h.used("Grep"));

        let bash = &h.tools["Bash"];
        assert_eq!(bash.calls, 3);
        assert_eq!(bash.commands, 3);
        assert_eq!(h.tools["Edit"].edits, 1);
        assert_eq!(h.tools["Read"].reads, 1);

        // Ranked: Bash (3) first, then Edit/Read (1 each) alphabetically.
        let ranked = h.ranked();
        assert_eq!(ranked[0].0, "Bash");
        assert_eq!(ranked[1].0, "Edit");
        assert_eq!(ranked[2].0, "Read");
    }

    /// A tool's `calls` is its `tool_use` blocks, whatever its rows: a three-file patch through
    /// the shell and through `apply_patch` are one call each, with every file still an edit.
    #[test]
    fn a_multi_file_patch_is_one_call() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: b.rs\n+z\n\
                     *** Delete File: c.rs\n*** End Patch";
        let msgs = [asst(vec![
            tool_use("shell", json!({"command": ["apply_patch", patch]})),
            tool_use("apply_patch", json!({"input": patch})),
            tool_use("shell", json!({"command": "ls"})),
        ])];
        let blocks = msgs[0].content.len();
        let h = ToolHistogram::from_messages(msgs.iter(), Some(std::path::Path::new("/repo"))).unwrap();
        assert_eq!(h.total_calls, blocks);
        let shell = &h.tools["shell"];
        assert_eq!((shell.calls, shell.commands, shell.edits), (2, 2, 3));
        let ap = &h.tools["apply_patch"];
        assert_eq!((ap.calls, ap.edits), (1, 3));
    }

    /// The timeline has one entry per call, the count `cv tools` reports: a three-file patch is one
    /// line carrying its other files, not three. Each entry carries its call's id.
    #[test]
    fn timeline_has_one_entry_per_call() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: b.rs\n+z\n\
                     *** Delete File: c.rs\n*** End Patch";
        let msgs = [
            asst(vec![
                with_id(tool_use("shell", json!({"command": ["apply_patch", patch]})), "c1"),
                with_id(tool_use("apply_patch", json!({"input": patch})), "c2"),
            ]),
            asst(vec![with_id(tool_use("shell", json!({"command": "ls"})), "c3")]),
        ];
        let cwd = std::path::Path::new("/repo");
        let mut sink = CallSink::new(Some(cwd.into()));
        for m in &msgs {
            sink.message(m.clone());
        }
        let h = ToolHistogram::from_messages(msgs.iter(), Some(cwd)).unwrap();
        let mut tl = Vec::new();
        timeline_calls(ORCHESTRATOR, sink.rows, &mut tl);
        assert_eq!(tl.len(), h.total_calls);
        let line = |e: &ToolEvent| {
            (
                e.tool_use_id.clone(),
                e.tool.clone().unwrap(),
                e.kind,
                e.more_targets.len(),
            )
        };
        assert_eq!(
            tl.iter().map(line).collect::<Vec<_>>(),
            [
                ("c1".into(), "shell".into(), "command", 3),
                ("c2".into(), "apply_patch".into(), "file_edit", 2),
                ("c3".into(), "shell".into(), "command", 0)
            ]
        );
    }

    #[test]
    fn selected_timeline_does_not_demand_excluded_call_arguments() {
        let world = crate::test_world("timeline-selected-call");
        let path = world.base.join("arguments.json");
        std::fs::write(&path, b"{\"command\":\"ignored\"}").unwrap();
        let excluded = crate::Json::source(&path, 0, std::fs::metadata(&path).unwrap().len()).unwrap();
        std::fs::remove_file(path).unwrap();
        let mut m = asst(vec![
            Block::ToolUse {
                id: "excluded".into(),
                name: "Bash".into(),
                namespace: None,
                input: excluded,
            },
            with_id(tool_use("Read", json!({"file_path":"selected.rs"})), "selected"),
        ]);
        m.timestamp = Some(chrono::DateTime::from_timestamp(123, 0).unwrap());
        let mut all = CallSink::new(None);
        assert_eq!(all.message(m.clone()), Flow::Stop);
        assert!(all.take_error().is_some());
        let mut histogram = HistogramSink {
            extractor: events::EventExtractor::default(),
            state: SinkState::new(None),
            msg_idx: 0,
            histogram: ToolHistogram::default(),
            tool: Some("Read".into()),
            scratch: Vec::new(),
        };
        // Histograms correlate native completions with every call before selecting rows.
        // Missing excluded arguments cannot silently change canonical edit attribution.
        assert_eq!(histogram.message(m.clone()), Flow::Stop);
        assert!(histogram.take_error().is_some());
        assert!(histogram.histogram.tools.is_empty());
        let mut selected = CallSink::new(None);
        selected.tool = Some("Read".into());
        assert_eq!(selected.message(m), Flow::Continue);
        let mut timeline = Vec::new();
        timeline_calls(ORCHESTRATOR, selected.rows, &mut timeline);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].tool_use_id, "selected");
        assert_eq!(timeline[0].ts, Some(123));
        assert_eq!(timeline[0].msg_idx, 0);
    }

    #[test]
    fn errors_are_tallied_per_tool_and_total() {
        let mut m = Message::new(Role::Tool);
        m.content = vec![
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
                tool_name: Some("Bash".into()),
                status: None,
                details: None,
            },
            Block::ToolResult {
                tool_use_id: "t2".into(),
                content: "boom2".into(),
                is_error: true,
                tool_name: None, // unknown tool → only the total moves
                status: None,
                details: None,
            },
        ];
        let h = ToolHistogram::from_messages(std::slice::from_ref(&m).iter(), None).unwrap();
        assert_eq!(h.total_errors, 2);
        assert_eq!(h.tools["Bash"].errors, 1);
        assert_eq!(h.total_calls, 0);
    }

    #[test]
    fn forest_aggregate_and_both_query_directions() {
        // Two synthetic agents' histograms, merged into a ForestTools by hand.
        let a1 = AgentTools {
            agent: "<orchestrator>".into(),
            agent_type: "orchestrator".into(),
            workflow: None,
            histogram: ToolHistogram::from_messages(
                std::slice::from_ref(&asst(vec![
                    tool_use("Bash", json!({"command": "ls"})),
                    tool_use("Agent", json!({"description": "go"})),
                ]))
                .iter(),
                None,
            )
            .unwrap(),
        };
        let a2 = AgentTools {
            agent: "agentA".into(),
            agent_type: "general-purpose".into(),
            workflow: Some("wf_1".into()),
            histogram: ToolHistogram::from_messages(
                std::slice::from_ref(&asst(vec![
                    tool_use("Bash", json!({"command": "make"})),
                    tool_use("Read", json!({"file_path": "x.rs"})),
                ]))
                .iter(),
                Some(std::path::Path::new("/r")),
            )
            .unwrap(),
        };
        let ft = ForestTools { agents: vec![a1, a2] };

        // Aggregate: Bash used twice across the two agents.
        let agg = ft.aggregate();
        assert_eq!(agg.tools["Bash"].calls, 2);
        assert_eq!(agg.total_calls, 4);

        // "which tools did agent X use"
        assert!(ft.agent("agentA").unwrap().histogram.used("Read"));
        assert!(!ft.agent("<orchestrator>").unwrap().histogram.used("Read"));

        // "which agents used tool T" — both used Bash; only the orchestrator used Agent.
        let bash_users = ft.agents_using("Bash");
        assert_eq!(bash_users.len(), 2);
        let agent_users = ft.agents_using("Agent");
        assert_eq!(agent_users.len(), 1);
        assert_eq!(agent_users[0].0.agent, "<orchestrator>");
    }
    #[test]
    fn histogram_and_timeline_stop_on_unreadable_tool_arguments() {
        let world = crate::test_world("tool_json_failures");
        let path = world.home.join("input.json");
        let body = r#"{"file_path":"a.rs"}"#;
        std::fs::write(&path, body).unwrap();
        let input = crate::json::Json::source(&path, 0, body.len() as u64).unwrap();
        let message = asst(vec![Block::ToolUse {
            id: "call".into(),
            name: "Edit".into(),
            namespace: None,
            input,
        }]);
        std::fs::remove_file(path).unwrap();
        assert!(ToolHistogram::from_messages([&message], None).is_err());
        let mut histogram = HistogramSink {
            extractor: events::EventExtractor::default(),
            state: SinkState::new(None),
            msg_idx: 0,
            histogram: ToolHistogram::default(),
            tool: None,
            scratch: Vec::new(),
        };
        assert_eq!(histogram.message(message.clone()), Flow::Stop);
        assert!(histogram.take_error().is_some());
        let mut timeline = CallSink::new(None);
        assert_eq!(timeline.message(message), Flow::Stop);
        assert!(timeline.take_error().is_some());
        assert!(timeline.rows.is_empty());
    }
}
