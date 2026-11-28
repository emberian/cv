//! `cv tools` — cross-agent tool analytics over a session's forest (the orchestrator plus every
//! sub-agent): the aggregate histogram, one agent's, which agents used a tool, the per-agent
//! breakdown, and the time-ordered tool-call timeline.

use crate::util::short_id;
use anyhow::{Context, Result};
use cv_core::ir::truncate;
use cv_core::tools::{ForestTools, ToolHistogram};

/// `cv tools <id>`: cross-agent tool analytics. Default = the aggregate histogram across the whole
/// forest (orchestrator + every sub-agent). Filters:
/// * `--agent <id>` — one agent's histogram ("which tools did agent X use")
/// * `--tool <name>` — which agents used tool T (across the forest)
/// * `--workflow <run>` — restrict to one workflow's agents
/// * `--across` — one row per agent (the per-agent breakdown), instead of the aggregate
/// * `--timeline` — the time-ordered tool-call timeline
pub(crate) struct ToolsArgs<'a> {
    pub id: &'a str,
    pub harness: Option<String>,
    pub agent: Option<String>,
    pub tool: Option<String>,
    pub workflow: Option<String>,
    pub across: bool,
    pub timeline: bool,
    pub json: bool,
}

pub(crate) fn cmd_tools(args: ToolsArgs<'_>) -> Result<()> {
    let ToolsArgs {
        id,
        harness,
        agent,
        tool,
        workflow,
        across,
        timeline,
        json,
    } = args;
    if tool.is_some() && agent.is_some() && !timeline {
        return crate::util::usage(
            "`cv tools --tool` and `--agent` are two different views; pass `--timeline` to narrow by both",
        );
    }
    let want = crate::util::parse_harness(&harness)?;
    let view = super::place::current();
    let source = view.as_ref().map(|v| v.resolve(id, want)).transpose()?;
    let (forest, agent) = if let (Some(v), Some(src)) = (&view, source.filter(|s| s.entry.is_some())) {
        let entry = src.entry.as_ref().unwrap();
        v.omit(
            &entry.producer_id,
            "worker_inventory",
            "Only collected worker transcripts carry tool evidence.",
        );
        let mut inputs = vec![(cv_core::tools::ORCHESTRATOR, "orchestrator", None, src)];
        for s in v.children(src) {
            let m = &s.entry.as_ref().unwrap().metadata;
            inputs.push((
                super::place::View::agent_id(s),
                m["agent_type"].as_str().unwrap_or("agent"),
                m["workflow"].as_str(),
                s,
            ));
        }
        inputs.retain(|(_, _, w, _)| workflow.as_deref().is_none_or(|q| w.is_some_and(|w| w.starts_with(q))));
        if let Some(q) = &agent {
            inputs.retain(|(id, _, _, _)| id.starts_with(q));
            anyhow::ensure!(inputs.len() == 1, "missing or ambiguous collected agent {q:?}");
        }
        let selected = agent.as_ref().and_then(|_| inputs.first().map(|t| t.0.to_owned()));
        let mut hist = Vec::new();
        let mut events = Vec::new();
        for (id, ty, w, s) in inputs {
            let reader = v.adapter(s)?;
            if timeline {
                events.extend(cv_core::tools::timeline_from_reader(
                    reader.as_ref(),
                    &s.r,
                    id,
                    tool.as_deref(),
                )?);
            } else {
                hist.push(cv_core::tools::AgentTools {
                    agent: id.into(),
                    agent_type: ty.into(),
                    workflow: w.map(str::to_owned),
                    histogram: cv_core::tools::ToolHistogram::from_stream_selected(
                        reader.as_ref(),
                        &s.r,
                        tool.as_deref(),
                    )?,
                });
            }
        }
        if timeline {
            events.sort_by_key(|e| (e.ts.unwrap_or(i64::MAX), e.agent.clone(), e.msg_idx));
            return render_timeline(events, selected.as_deref(), json);
        }
        (ForestTools { agents: hist }, selected)
    } else {
        let r = match source {
            Some(s) => s.r.clone(),
            None => crate::util::resolve(id, want)?.0,
        };

        let workflow = if let Some(run) = &workflow {
            // Resolve a run-id *prefix* to the full id (the forest tags agents with the full run id),
            // so `--workflow wf_08eb66fa` matches `wf_08eb66fa-4b1`.
            let full = cv_core::workflow_id_of(&r, run)
                .with_context(|| format!("no workflow {run:?} in {}", short_id(&r.id)))?;
            // The orchestrator isn't part of any one workflow; restrict to the named run's agents.
            Some(full)
        } else {
            None
        };
        // One selector grammar for every view: an exact id or a unique prefix; none or several is a
        // usage error (exit 2), like a session id.
        let agent = agent
            .map(|key| {
                cv_core::tools::resolve_agent(&r, &key, workflow.as_deref()).map_err(|e| {
                    match e.downcast_ref::<cv_core::tools::AgentSelectorError>() {
                        Some(sel) => {
                            crate::util::UsageError(format!("{sel} (try `cv tools {} --across`)", short_id(&r.id)))
                                .into()
                        }
                        None => e,
                    }
                })
            })
            .transpose()?;
        if timeline {
            return tools_timeline(&r, agent.as_deref(), workflow.as_deref(), tool.as_deref(), json);
        }

        (
            cv_core::tools::forest_tools_selected(&r, agent.as_deref(), workflow.as_deref(), tool.as_deref())?,
            agent,
        )
    };
    // `--tool T`: which agents used it.
    if let Some(t) = &tool {
        return tools_which_agents(&forest, t, json);
    }
    // `--agent X`: one agent's histogram.
    if let Some(a) = &agent {
        let at = forest.agent(a).with_context(|| {
            format!(
                "no agent matching {a:?} in {} (try `cv tools {} --across`)",
                short_id(id),
                short_id(id)
            )
        })?;
        if json {
            super::place::print_json("tools", at)?;
            return Ok(());
        }
        println!("# tools used by {} ({})", at.agent, at.agent_type);
        print_histogram(&at.histogram);
        return Ok(());
    }
    // `--across`: per-agent breakdown.
    if across {
        return tools_across(&forest, json);
    }

    // Default: the aggregate across the whole forest.
    let agg = forest.aggregate();
    if json {
        super::place::print_json("tools", &agg)?;
        return Ok(());
    }
    println!(
        "# tool usage across {} ({} agent(s): orchestrator + forest)",
        short_id(id),
        forest.agents.len()
    );
    print_histogram(&agg);
    println!("\n→ `--across` for per-agent · `--agent <id>` · `--tool <name>` · `--timeline`");
    Ok(())
}

/// "Which agents used tool T", ranked by count.
fn tools_which_agents(forest: &ForestTools, tool: &str, json: bool) -> Result<()> {
    let users = forest.agents_using(tool);
    if json {
        let rows: Vec<serde_json::Value> = users
            .iter()
            .map(|(a, c)| {
                serde_json::json!({
                    "agent": a.agent, "agent_type": a.agent_type, "workflow": a.workflow,
                    "calls": c.calls, "edits": c.edits, "reads": c.reads, "commands": c.commands, "errors": c.errors,
                })
            })
            .collect();
        super::place::print_json("tools", &rows)?;
        return Ok(());
    }
    if users.is_empty() {
        println!("no agent used {tool:?}");
        return Ok(());
    }
    let total: usize = users.iter().map(|(_, c)| c.calls).sum();
    println!("# {} agent(s) used {tool:?} ({total} call(s) total)\n", users.len());
    for (a, c) in users {
        let wf = a.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        println!(
            "{:>5}  {:9}  {:18}{}",
            c.calls,
            a.agent_type,
            if a.agent == cv_core::tools::ORCHESTRATOR {
                a.agent.clone()
            } else {
                short_id(&a.agent)
            },
            wf,
        );
    }
    Ok(())
}

/// Per-agent breakdown: one block per agent, its histogram beneath.
fn tools_across(forest: &ForestTools, json: bool) -> Result<()> {
    if json {
        super::place::print_json("tools", forest)?;
        return Ok(());
    }
    // Skip agents that used no tools (a chat-only sub-agent) to keep the view dense.
    let active: Vec<&cv_core::tools::AgentTools> =
        forest.agents.iter().filter(|a| a.histogram.total_calls > 0).collect();
    println!("# per-agent tool usage ({} active agent(s))\n", active.len());
    for a in active {
        let wf = a.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        let label = if a.agent == cv_core::tools::ORCHESTRATOR {
            a.agent.clone()
        } else {
            short_id(&a.agent)
        };
        let top: Vec<String> = a
            .histogram
            .ranked()
            .into_iter()
            .take(6)
            .map(|(name, c)| format!("{name}×{}", c.calls))
            .collect();
        println!(
            "{:18} {:11} {:>4} call(s){}  {}",
            label,
            a.agent_type,
            a.histogram.total_calls,
            wf,
            top.join(" "),
        );
    }
    Ok(())
}

/// The time-ordered tool-call timeline across the forest.
fn tools_timeline(
    r: &cv_core::SessionRef,
    agent: Option<&str>,
    workflow: Option<&str>,
    tool: Option<&str>,
    json: bool,
) -> Result<()> {
    let events = cv_core::tools::forest_timeline_selected(r, agent, workflow, tool)?;
    render_timeline(events, agent, json)
}

fn render_timeline(events: Vec<cv_core::tools::ToolEvent>, agent: Option<&str>, json: bool) -> Result<()> {
    if json {
        super::place::print_json("tools", &events)?;
        return Ok(());
    }
    if events.is_empty() {
        println!(
            "no tool calls{}",
            agent.map(|a| format!(" for {a:?}")).unwrap_or_default()
        );
        return Ok(());
    }
    println!("# {} tool call(s) (chronological)\n", events.len());
    for e in &events {
        let time =
            e.ts.and_then(|t| crate::util::fmt_local_ts(t, "%m-%d %H:%M:%S"))
                .unwrap_or_else(|| "--------------".into());
        let who = if e.agent == cv_core::tools::ORCHESTRATOR {
            "orch".to_string()
        } else {
            short_id(&e.agent)
        };
        let mut target = e.target.as_deref().map(|t| truncate(t, 70)).unwrap_or_default();
        if !e.more_targets.is_empty() {
            target.push_str(&format!(" (+{} more)", e.more_targets.len()));
        }
        println!(
            // The id in full: `cv cat` matches it exactly, so a shortened one would not reach it.
            "{}  {:8}  {}  {:9}  {:14}  {}",
            time,
            who,
            e.tool_use_id,
            e.kind,
            e.tool.as_deref().unwrap_or("-"),
            target,
        );
    }
    Ok(())
}

/// Print a [`ToolHistogram`] as a ranked table (tool · calls · kind breakdown · errors).
fn print_histogram(h: &ToolHistogram) {
    if h.tools.is_empty() {
        println!("  (no tool calls)");
        return;
    }
    println!(
        "  {:<22} {:>6}  {:>6} {:>6} {:>6} {:>6}",
        "tool", "calls", "edit", "read", "cmd", "err"
    );
    for (name, c) in h.ranked() {
        println!(
            "  {:<22} {:>6}  {:>6} {:>6} {:>6} {:>6}",
            truncate(name, 22),
            c.calls,
            kindcell(c.edits),
            kindcell(c.reads),
            kindcell(c.commands),
            kindcell(c.errors),
        );
    }
    println!(
        "  {:<22} {:>6}  {:>30}",
        "TOTAL",
        h.total_calls,
        format!("{} distinct · {} error(s)", h.distinct(), h.total_errors),
    );
}

/// A histogram kind-cell: the count, or a blank for zero (keeps the table readable).
fn kindcell(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kindcell_blanks_zero() {
        assert_eq!(kindcell(0), "");
        assert_eq!(kindcell(3), "3");
    }
}
