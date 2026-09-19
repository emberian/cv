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
//! Memory discipline: everything is built from the small [`crate::events::Event`] rows — tool
//! *results* are never materialized (large outputs stay lazy on disk). One streamed pass per agent.

use crate::events::{self, Event, EventSink};
use crate::ir::{Message, SessionRef};
use crate::stream::ParseOptions;
use std::collections::BTreeMap;

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
    /// Fold a slice of classified [`Event`]s into a histogram. `tool_use`-derived rows
    /// (`file_edit`/`file_read`/`command`/`tool`) increment their tool's tally; `error` rows
    /// increment the error count for their tool (or the unknown-tool error total).
    pub fn from_events(evs: &[Event]) -> Self {
        let mut h = ToolHistogram::default();
        for e in evs {
            match e.kind {
                "error" => {
                    h.total_errors += 1;
                    if let Some(t) = &e.tool {
                        h.tools.entry(t.clone()).or_default().errors += 1;
                    }
                }
                kind => {
                    let Some(name) = &e.tool else { continue };
                    let c = h.tools.entry(name.clone()).or_default();
                    c.calls += 1;
                    h.total_calls += 1;
                    match kind {
                        "file_edit" => c.edits += 1,
                        "file_read" => c.reads += 1,
                        "command" => c.commands += 1,
                        _ => {}
                    }
                }
            }
        }
        h
    }

    /// Build directly from a session's already-parsed messages (the whole-session convenience over
    /// a streamed [`EventSink`]). Used for the orchestrator's own tools and in tests.
    pub fn from_messages<'a>(msgs: impl IntoIterator<Item = &'a Message>, cwd: Option<&std::path::Path>) -> Self {
        let mut evs = Vec::new();
        for (i, m) in msgs.into_iter().enumerate() {
            evs.extend(events::extract(m, i, cwd));
        }
        Self::from_events(&evs)
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

/// One tool call on a timeline: which agent, which tool, when, what kind/target. The ordered form
/// used by `cv tools --timeline`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolEvent {
    /// Identifier of the agent that made the call (`<orchestrator>` for the parent session itself,
    /// else the agent's short id).
    pub agent: String,
    /// Index of the message within that agent's transcript.
    pub msg_idx: usize,
    /// Unix seconds of the call's message, when timestamped.
    pub ts: Option<i64>,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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

    /// One agent's histogram by id (orchestrator via `<orchestrator>`), or `None`.
    pub fn agent(&self, id: &str) -> Option<&AgentTools> {
        self.agents.iter().find(|a| a.agent == id || a.agent.starts_with(id))
    }

    /// Restrict to one workflow's agents (a new view sharing the same histograms).
    pub fn for_workflow(&self, run_id: &str) -> ForestTools {
        ForestTools {
            agents: self
                .agents
                .iter()
                .filter(|a| a.workflow.as_deref() == Some(run_id))
                .cloned()
                .collect(),
        }
    }
}

/// The sentinel agent id for the parent session's own tool calls in a forest aggregation.
pub const ORCHESTRATOR: &str = "<orchestrator>";

/// Build the full per-agent tool analytics for a session: the orchestrator's own tools plus one
/// histogram per sub-agent in the forest (each streamed lazily — result bodies stay on disk).
///
/// This is the cross-agent surface: with it you can ask "which tools did agent X use"
/// ([`ForestTools::agent`]), "which agents used tool T" ([`ForestTools::agents_using`]), and the
/// aggregate across the whole forest ([`ForestTools::aggregate`]). Best-effort per agent: an
/// unreadable transcript contributes an empty histogram rather than aborting.
pub fn forest_tools(r: &SessionRef) -> anyhow::Result<ForestTools> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;

    let mut agents = Vec::new();

    // The orchestrator (the parent session) itself.
    {
        let mut sink = EventSink::new(r.cwd.clone());
        adapter.stream(r, &ParseOptions::lazy(), &mut sink)?;
        agents.push(AgentTools {
            agent: ORCHESTRATOR.to_string(),
            agent_type: "orchestrator".to_string(),
            workflow: None,
            histogram: ToolHistogram::from_events(sink.events()),
        });
    }

    // Every sub-agent in the forest (direct + workflow), each its own histogram.
    for s in crate::subagent_tree_of(r) {
        let mut sink = EventSink::new(s.session.cwd.clone());
        let hist = if adapter.stream(&s.session, &ParseOptions::lazy(), &mut sink).is_ok() {
            ToolHistogram::from_events(sink.events())
        } else {
            ToolHistogram::default()
        };
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
pub fn forest_timeline(r: &SessionRef) -> anyhow::Result<Vec<ToolEvent>> {
    let adapter =
        crate::harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for {}", r.harness))?;
    let mut out: Vec<ToolEvent> = Vec::new();

    let mut collect = |agent: String, evs: Vec<Event>| {
        for e in evs {
            if e.kind == "error" {
                continue; // the timeline is *calls*, not result-errors
            }
            out.push(ToolEvent {
                agent: agent.clone(),
                msg_idx: e.msg_idx,
                ts: e.ts,
                kind: e.kind,
                tool: e.tool,
                target: e.target,
            });
        }
    };

    let mut sink = EventSink::new(r.cwd.clone());
    adapter.stream(r, &ParseOptions::lazy(), &mut sink)?;
    collect(ORCHESTRATOR.to_string(), sink.into_events());

    for s in crate::subagent_tree_of(r) {
        let mut sink = EventSink::new(s.session.cwd.clone());
        if adapter.stream(&s.session, &ParseOptions::lazy(), &mut sink).is_ok() {
            collect(s.agent_id().to_string(), sink.into_events());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Message, Role};
    use serde_json::json;

    fn asst(blocks: Vec<Block>) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content = blocks;
        m
    }
    fn tool_use(name: &str, input: serde_json::Value) -> Block {
        Block::ToolUse {
            id: "t".into(),
            name: name.into(),
            input,
            namespace: None,
        }
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
        let h = ToolHistogram::from_messages(msgs.iter(), Some(std::path::Path::new("/repo")));
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
        let h = ToolHistogram::from_messages(std::slice::from_ref(&m).iter(), None);
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
            ),
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
            ),
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

        // Workflow restriction.
        let wf = ft.for_workflow("wf_1");
        assert_eq!(wf.agents.len(), 1);
        assert_eq!(wf.agents[0].agent, "agentA");
    }
}
