//! `cv workflow` / `cv tools` / `cv compaction` — the deep Claude-harness surfaces:
//! first-class workflow runs (phase tree → agents → outcomes + script), cross-agent tool
//! analytics, and compaction-boundary detection/retrieval.

use crate::util::short_id;
use anyhow::{bail, Context, Result};
use cv_core::ir::truncate;
use cv_core::tools::{ForestTools, ToolHistogram};

// ===================== cv workflow =====================

/// `cv workflow <session> [run]`: render one workflow run richly — its phases, the agents grouped
/// under each phase with their outcomes, the run totals, the aggregated result, and (optionally)
/// the driving script. Without a `run`, lists every workflow the session launched. Both arguments
/// accept **names**: `run` matches a workflow name (exact, else unique prefix) as well as a run id,
/// and when `<session>` matches no session id it's resolved as a workflow name across the whole
/// catalog — session titles are auto-generated and rarely mention the workflow you remember.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_workflow(
    id: &str,
    run_id: Option<String>,
    harness: Option<String>,
    json: bool,
    script: bool,
    results: bool,
    follow: bool,
    revive: bool,
    revive_all: bool,
) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    // find_cheap: don't pay a full fleet re-discovery before trying the id as a workflow name —
    // the name path escalates to a full `find` itself once every cheaper reading has missed.
    let Some((r, _adapter)) = cv_core::find_cheap(id, want)? else {
        // Not a session id → maybe it's a workflow name ("the stark-kill session" problem).
        return workflow_by_name_fleetwide(id, want, json, script, results, revive, revive_all);
    };

    // No run id → list the session's workflows (a directory of runs).
    let Some(run_id) = run_id else {
        return list_workflows(&r, json);
    };

    if follow {
        return follow_workflow(&r, &run_id, json, script, results);
    }

    let mut wf = cv_core::workflow_of(&r, &run_id).with_context(|| {
        let runs = cv_core::workflows_of(&r);
        let names: Vec<&str> = runs.iter().filter_map(|w| w.name.as_deref()).collect();
        format!(
            "no workflow {run_id:?} in {} — {} run(s): {}",
            short_id(&r.id),
            runs.len(),
            if names.is_empty() {
                "(unnamed)".into()
            } else {
                names.join(", ")
            },
        )
    })?;
    wf.attach_journal(&r.path);
    emit_workflow(&wf, json, script, results, revive, revive_all, Some(r.path.as_path()))
}

/// `--follow`: poll the run's state file (the harness flushes it as agents progress) and stream
/// agent state TRANSITIONS as feed lines — `12:03:41  ✓ verify:merkle → done (312,004 tok)` —
/// then, when the run reaches a terminal status, emit the full render (honoring
/// `--json`/`--script`/`--results`). Waits for the state file if the run hasn't registered yet.
fn follow_workflow(r: &cv_core::SessionRef, key: &str, json: bool, script: bool, results: bool) -> Result<()> {
    use std::collections::HashMap;
    let mut seen: HashMap<u64, String> = HashMap::new(); // agent index → last printed state
    let mut announced = false;
    loop {
        let Some(wf) = cv_core::workflow_of(r, key) else {
            if !announced {
                eprintln!("… waiting for run {key:?} to register (Ctrl-C to stop)");
                announced = true;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        };
        if !announced {
            eprintln!(
                "✦ following {} ({}) — Ctrl-C to stop",
                wf.name.as_deref().unwrap_or("(unnamed)"),
                wf.run_id,
            );
            announced = true;
        }
        let now = crate::util::fmt_local(chrono::Utc::now(), "%H:%M:%S");
        for a in wf.phases.iter().flat_map(|p| &p.agents).chain(&wf.orphan_agents) {
            let state = a.state.clone().unwrap_or_default();
            if seen.get(&a.index).is_some_and(|s| *s == state) {
                continue;
            }
            seen.insert(a.index, state.clone());
            let glyph = match state.as_str() {
                "done" => "✓",
                "error" => "✗",
                "progress" => "…",
                _ => "▸",
            };
            let tok = a.tokens.map(|t| format!(" ({} tok)", fmt_int(t))).unwrap_or_default();
            println!(
                "{now}  {glyph} {} → {state}{tok}",
                a.label.as_deref().unwrap_or("(agent)")
            );
        }
        let live = matches!(wf.status.as_deref(), None | Some("running") | Some("started"));
        if !live {
            println!("\n── run reached {} ──\n", wf.status.as_deref().unwrap_or("?"));
            let mut wf = wf;
            wf.attach_journal(&r.path);
            return emit_workflow(&wf, json, script, results, false, false, Some(r.path.as_path()));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Shared single-run output: `--json` gets the full structure (journal results included);
/// otherwise the rendered view, with `--results` appending each agent's full journaled return.
fn emit_workflow(
    wf: &cv_core::Workflow,
    json: bool,
    script: bool,
    results: bool,
    revive: bool,
    revive_all: bool,
    session_path: Option<&std::path::Path>,
) -> Result<()> {
    if revive {
        return render_revive(wf, revive_all, session_path);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(wf)?);
        return Ok(());
    }
    render_workflow(wf, script);
    if results {
        render_journal_results(wf);
    } else if wf
        .phases
        .iter()
        .flat_map(|p| &p.agents)
        .chain(&wf.orphan_agents)
        .any(|a| a.journal_result.is_some())
    {
        println!("→ `--results` for each agent's FULL journaled return (previews above are ~400 chars)");
    }
    Ok(())
}

/// The `--results` tail: every agent's FULL journaled return value, pretty-printed.
fn render_journal_results(w: &cv_core::Workflow) {
    let agents: Vec<&cv_core::WorkflowAgent> = w
        .phases
        .iter()
        .flat_map(|p| &p.agents)
        .chain(&w.orphan_agents)
        .filter(|a| a.journal_result.is_some())
        .collect();
    if agents.is_empty() {
        println!("\n(no journaled per-agent results — the run's journal is absent or empty)");
        return;
    }
    println!("\n## full per-agent returns ({} journaled)", agents.len());
    for a in agents {
        println!(
            "\n### {}  {}",
            a.label.as_deref().unwrap_or("(agent)"),
            a.agent_id.as_deref().map(short_id).unwrap_or_default(),
        );
        let r = a.journal_result.as_ref().unwrap();
        println!("{}", serde_json::to_string_pretty(r).unwrap_or_else(|_| r.to_string()));
    }
}

/// Resolve a bare `cv workflow <name>` against every session's workflow runs. One hit renders it;
/// several list themselves with ready-to-paste commands. A miss falls through to a ghost-launch
/// scan — a name with no recorded run anywhere may still be a launch whose state was never
/// persisted (crash/power loss), and that is precisely when someone hunts it by name.
fn workflow_by_name_fleetwide(
    name: &str,
    want: Option<cv_core::ir::Harness>,
    json: bool,
    script: bool,
    results: bool,
    revive: bool,
    revive_all: bool,
) -> Result<()> {
    let hits = cv_core::find_workflows_by_name(name);
    if hits.is_empty() {
        let ghosts = cv_core::find_ghost_launches_by_name(name);
        if ghosts.is_empty() {
            // Very last reading: a session id living in the discovery probe's blind spots (what
            // the full `find` covers and `find_cheap` deliberately skipped).
            if let Some((r, _adapter)) = cv_core::find(name, want)? {
                return list_workflows(&r, json);
            }
        }
        if !ghosts.is_empty() {
            println!(
                "no recorded run named {name:?} — but {} GHOST launch(es) match (state never persisted; crash/kill before write?):\n",
                ghosts.len()
            );
            for (r, g) in &ghosts {
                println!(
                    "   {} — session {} ({})",
                    ghost_line(g),
                    short_id(&r.id),
                    r.title.as_deref().unwrap_or("untitled"),
                );
            }
            println!("\n→ debris dirs live under the session dir's subagents/workflows/<runId>/ (`cv show <agent-id>` opens a transcript)");
            return Ok(());
        }
        bail!("no session and no workflow name matching {name:?} (try `cv ls -q 'workflow:{name}'`)");
    }
    if hits.len() == 1 {
        let (r, wf) = &hits[0];
        let mut wf = wf.clone();
        wf.attach_journal(&r.path);
        if !json {
            println!(
                "(in session {} — {})\n",
                short_id(&r.id),
                r.title.as_deref().unwrap_or("untitled")
            );
        }
        return emit_workflow(&wf, json, script, results, revive, revive_all, Some(r.path.as_path()));
    }
    println!("# {} workflow run(s) matching {name:?}:\n", hits.len());
    for (r, w) in &hits {
        println!(
            "cv workflow {} {}   # {} · {} · {} agent(s)",
            short_id(&r.id),
            w.run_id,
            w.name.as_deref().unwrap_or("(unnamed)"),
            w.status.as_deref().unwrap_or("?"),
            w.agent_count,
        );
    }
    Ok(())
}

/// List every workflow run a session launched, newest first — name, status, phase/agent counts,
/// and the run summary — plus any **ghost launches**: `Workflow` invocations visible in the
/// transcript whose run state was never persisted (a crash / power loss / hard kill before the
/// harness wrote `workflows/wf_*.json`). Without the ghost check, a run that died at launch is
/// simply invisible here — exactly the run whose debris most needs finding.
fn list_workflows(r: &cv_core::SessionRef, json: bool) -> Result<()> {
    let runs = cv_core::workflows_of(r);
    let ghosts = cv_core::workflow_ghosts_of(r);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"runs": runs, "ghost_launches": ghosts}))?
        );
        return Ok(());
    }
    if runs.is_empty() && ghosts.is_empty() {
        println!("no workflows launched by {}", short_id(&r.id));
        return Ok(());
    }
    if runs.is_empty() {
        println!("# 0 recorded workflow(s) in {}", short_id(&r.id));
    } else {
        println!("# {} workflow(s) in {}\n", runs.len(), short_id(&r.id));
    }
    for w in &runs {
        let status = w.status.as_deref().unwrap_or("?");
        let states = w.state_counts();
        let state_str = if states.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = states.iter().map(|(k, n)| format!("{n} {k}")).collect();
            format!("  [{}]", parts.join(", "))
        };
        println!(
            "{}  {}  {} · {} phase(s) · {} agent(s){}",
            w.run_id,
            w.name.as_deref().unwrap_or("(unnamed)"),
            status,
            w.phases.len(),
            w.agent_count,
            state_str,
        );
        if let Some(s) = &w.summary {
            println!("    {}", truncate(s, 120));
        }
    }
    if !ghosts.is_empty() {
        println!(
            "\n⚠ {} launch(es) with NO recorded run — state never persisted (crash/kill before write?):",
            ghosts.len()
        );
        for g in &ghosts {
            println!("   {}", ghost_line(g));
        }
        println!("   → debris dirs live under the session dir's subagents/workflows/<runId>/ (`cv show <agent-id>` opens a transcript)");
    }
    println!("\n→ `cv workflow {} <runId>` for one run's phase tree", short_id(&r.id));
    Ok(())
}

/// One ghost launch, with everything the crash left behind: launch time, the run id recovered
/// from its orphaned script file, and the debris counts (agent transcripts + journaled results).
fn ghost_line(g: &cv_core::WorkflowLaunch) -> String {
    let when =
        g.ts.map(|t| crate::util::fmt_local(t, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "(no timestamp)".into());
    let mut line = format!("{}  launched {when}", g.name.as_deref().unwrap_or("(unnamed)"));
    if let Some(rid) = &g.run_id {
        line.push_str(&format!(" · run {rid}"));
    }
    match (g.debris_agents, g.journal_result_count) {
        (Some(0), Some(0)) => line.push_str(" · no debris"),
        (Some(a), Some(j)) => line.push_str(&format!(" · DEBRIS: {a} agent transcript(s), {j} journaled result(s)")),
        _ => {}
    }
    line
}

/// Render one workflow run: header (name/status/totals) → each phase with its agents and outcomes
/// → optionally the driving script.
fn render_workflow(w: &cv_core::Workflow, show_script: bool) {
    println!(
        "# workflow {}  ({})",
        w.name.as_deref().unwrap_or("(unnamed)"),
        w.run_id
    );
    let dur = w
        .duration_ms
        .map(|ms| format!(" · {}", fmt_duration(ms)))
        .unwrap_or_default();
    let started = w
        .started_at
        .map(|t| format!(" · started {}", crate::util::fmt_local(t, "%Y-%m-%d %H:%M")))
        .unwrap_or_default();
    println!(
        "{} · {} agent(s) · {} phase(s) · {} tokens · {} tool calls{}{}",
        w.status.as_deref().unwrap_or("?"),
        w.agent_count,
        w.phases.len(),
        fmt_int(w.total_tokens),
        fmt_int(w.total_tool_calls),
        dur,
        started,
    );
    if let Some(m) = &w.default_model {
        println!("model: {m}");
    }
    if let Some(t) = &w.task_id {
        println!("task: {t}");
    }
    if let Some(a) = &w.args {
        println!("args: {}", truncate(&a.to_string(), 200));
    }
    if let Some(rf) = &w.resume_from {
        println!("resumed from: {rf}");
    }
    if let Some(e) = &w.error {
        println!("⚠ error: {}", truncate(e, 200));
    }
    if let Some(s) = &w.summary {
        println!("\n{}", truncate(s, 600));
    }

    // The phase tree: phases in order, agents under each with their state + outcome.
    for p in &w.phases {
        let title = p.title.as_deref().unwrap_or("(phase)");
        println!(
            "\n## phase {} · {}{}",
            p.index,
            title,
            p.detail
                .as_deref()
                .map(|d| format!("  — {}", truncate(d, 80)))
                .unwrap_or_default()
        );
        if p.agents.is_empty() {
            println!("   (no agents)");
        }
        for a in &p.agents {
            render_workflow_agent(a);
        }
    }
    if !w.orphan_agents.is_empty() {
        println!("\n## (agents with no matching phase)");
        for a in &w.orphan_agents {
            render_workflow_agent(a);
        }
    }

    // The run's own narration: log() lines. Tail-biased — the end is where a run explains how it
    // finished (or what was failing when it stopped).
    if !w.logs.is_empty() {
        const TAIL: usize = 8;
        println!("\n## log ({} line(s))", w.logs.len());
        if w.logs.len() > TAIL {
            println!("   … {} earlier (--json for all)", w.logs.len() - TAIL);
        }
        for l in w.logs.iter().rev().take(TAIL).rev() {
            println!("   {}", truncate(l, 160));
        }
    }

    // The aggregated return value — the harvest payload.
    match &w.result {
        Some(r) => {
            let pretty = serde_json::to_string_pretty(r).unwrap_or_else(|_| r.to_string());
            println!("\n## result");
            if pretty.len() > 2000 {
                println!(
                    "{}\n(truncated — `--json` for the full result)",
                    truncate(&pretty, 2000)
                );
            } else {
                println!("{pretty}");
            }
        }
        None => println!("\n(no result recorded — the run returned nothing or died before finishing)"),
    }

    if show_script {
        match &w.script {
            Some(src) => {
                println!(
                    "\n## script{}",
                    w.script_path
                        .as_ref()
                        .map(|p| format!(" ({})", p.display()))
                        .unwrap_or_default()
                );
                println!("```js\n{src}\n```");
            }
            None => println!("\n(no script recorded)"),
        }
    } else if w.script.is_some() {
        println!("\n→ `--script` to print the driving workflow script");
    }
}

/// One agent line inside a workflow's phase tree: state glyph, label, id, telemetry, and the head
/// of its result.
fn render_workflow_agent(a: &cv_core::WorkflowAgent) {
    let glyph = match a.state.as_deref() {
        Some("done") => "✓",
        Some("error") => "✗",
        Some("progress") => "…",
        Some("start") => "▸",
        _ => "•",
    };
    let id = a.agent_id.as_deref().map(short_id).unwrap_or_else(|| "—".into());
    let label = a.label.as_deref().unwrap_or("(agent)");
    let mut tele = Vec::new();
    if let Some(t) = a.tokens {
        tele.push(format!("{} tok", fmt_int(t)));
    }
    if let Some(tc) = a.tool_calls {
        tele.push(format!("{tc} calls"));
    }
    if let Some(ms) = a.duration_ms {
        tele.push(fmt_duration(ms));
    }
    if a.cached {
        tele.push("cached".into());
    }
    if let Some(n) = a.attempt.filter(|n| *n > 1) {
        tele.push(format!("attempt {n}"));
    }
    let tele = if tele.is_empty() {
        String::new()
    } else {
        format!("  ({})", tele.join(", "))
    };
    println!("   {glyph} {label}  {id}{tele}");
    if let Some(e) = &a.error {
        println!("       ✗ {}", truncate(e, 120));
    }
    // A non-terminal agent (crash/kill/interrupt) — show where it was when last alive.
    if !matches!(a.state.as_deref(), Some("done")) {
        if let Some(tool) = &a.last_tool_name {
            let when = a
                .last_progress_at
                .map(|t| format!(" @ {}", crate::util::fmt_local(t, "%m-%d %H:%M")))
                .unwrap_or_default();
            println!(
                "       ↪ last: {tool} · {}{when}",
                a.last_tool_summary
                    .as_deref()
                    .map(|s| truncate(s, 100))
                    .unwrap_or_default(),
            );
        }
    }
    if let Some(rp) = &a.result_preview {
        println!("       ↩ {}", truncate(rp, 200));
    }
}

// ===================== cv tools =====================

/// `cv tools <id>`: cross-agent tool analytics. Default = the aggregate histogram across the whole
/// forest (orchestrator + every sub-agent). Filters:
/// * `--agent <id>` — one agent's histogram ("which tools did agent X use")
/// * `--tool <name>` — which agents used tool T (across the forest)
/// * `--workflow <run>` — restrict to one workflow's agents
/// * `--across` — one row per agent (the per-agent breakdown), instead of the aggregate
/// * `--timeline` — the time-ordered tool-call timeline
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_tools(
    id: &str,
    harness: Option<String>,
    agent: Option<String>,
    tool: Option<String>,
    workflow: Option<String>,
    across: bool,
    timeline: bool,
    json: bool,
) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let (r, _adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    if timeline {
        return tools_timeline(&r, agent.as_deref(), json);
    }

    let mut forest = cv_core::tools::forest_tools(&r)?;
    if let Some(run) = &workflow {
        // Resolve a run-id *prefix* to the full id (the forest tags agents with the full run id),
        // so `--workflow wf_08eb66fa` matches `wf_08eb66fa-4b1`.
        let full = cv_core::workflow_of(&r, run)
            .map(|w| w.run_id)
            .with_context(|| format!("no workflow {run:?} in {}", short_id(&r.id)))?;
        // The orchestrator isn't part of any one workflow; restrict to the named run's agents.
        forest = forest.for_workflow(&full);
        if forest.agents.is_empty() {
            bail!("no agents found for workflow {full:?} in {}", short_id(&r.id));
        }
    }

    // `--tool T`: which agents used it.
    if let Some(t) = &tool {
        return tools_which_agents(&forest, t, json);
    }
    // `--agent X`: one agent's histogram.
    if let Some(a) = &agent {
        let at = forest.agent(a).with_context(|| {
            format!(
                "no agent matching {a:?} in {} (try `cv tools {} --across`)",
                short_id(&r.id),
                short_id(&r.id)
            )
        })?;
        if json {
            println!("{}", serde_json::to_string_pretty(at)?);
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
        println!("{}", serde_json::to_string_pretty(&agg)?);
        return Ok(());
    }
    println!(
        "# tool usage across {} ({} agent(s): orchestrator + forest)",
        short_id(&r.id),
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
        println!("{}", serde_json::to_string_pretty(&rows)?);
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
        println!("{}", serde_json::to_string_pretty(forest)?);
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
fn tools_timeline(r: &cv_core::SessionRef, agent: Option<&str>, json: bool) -> Result<()> {
    let mut events = cv_core::tools::forest_timeline(r)?;
    if let Some(key) = agent {
        // Match the orchestrator sentinel or a full/prefix agent id.
        events.retain(|e| e.agent == key || e.agent.starts_with(key));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&events)?);
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
        println!(
            "{}  {:8}  {:9}  {:14}  {}",
            time,
            who,
            e.kind,
            e.tool.as_deref().unwrap_or("-"),
            e.target.as_deref().map(|t| truncate(t, 70)).unwrap_or_default(),
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

// ===================== cv compaction =====================

/// `cv compaction <id>`: list every compaction boundary in a session — when it happened, why
/// (trigger), the pre-compaction context size, and the summary that seeded the next window. With
/// `--summaries`, print each summary's full text.
pub(crate) fn cmd_compaction(id: &str, harness: Option<String>, summaries: bool, json: bool) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let (r, _adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    let comps = cv_core::compaction::detect(&r, true)?;

    if json {
        // Attach each boundary's pre-compaction span for machine consumers.
        let rows: Vec<serde_json::Value> = comps
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut v = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    if let Some((s, e)) = cv_core::compaction::pre_compaction_span(&comps, i) {
                        obj.insert("pre_compaction_span".into(), serde_json::json!([s, e]));
                    }
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if comps.is_empty() {
        println!("{} never compacted (0 boundaries)", short_id(&r.id));
        return Ok(());
    }

    println!("# {} compacted {} time(s)\n", short_id(&r.id), comps.len());
    for (i, c) in comps.iter().enumerate() {
        let trig = c.trigger.as_deref().unwrap_or("?");
        let pre = c
            .pre_tokens
            .map(|t| format!("{} tokens", fmt_int(t)))
            .unwrap_or_else(|| "? tokens".into());
        let dur = c
            .duration_ms
            .map(|ms| format!(" · took {}", fmt_duration(ms)))
            .unwrap_or_default();
        let span = cv_core::compaction::pre_compaction_span(&comps, i)
            .map(|(s, e)| format!("  · pre-span msgs {s}-{e}"))
            .unwrap_or_default();
        println!(
            "── #{} · {} · {} (pre) · @msg {}{}{}",
            i + 1,
            trig,
            pre,
            c.boundary_msg_idx,
            dur,
            span,
        );
        match (&c.summary, summaries) {
            (Some(s), true) => println!("\n{s}\n"),
            (Some(s), false) => println!("   summary: {}\n", truncate(s, 200)),
            (None, _) => println!("   (no summary recorded)\n"),
        }
    }
    if !summaries {
        println!(
            "→ `--summaries` for full summary text · `cv show {} --pre-compaction` to read the lost span",
            short_id(&r.id)
        );
    }
    Ok(())
}

// ===================== formatting helpers =====================

/// Group-separate a large integer (`589698` → `589,698`).
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Humanize a millisecond duration (`3144884` → `52m 24s`).
fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

// ===================== cv workflow --revive =====================

/// One lane's salvageable state: the task it was given, and what it managed to do first.
struct Revivable {
    label: String,
    agent_id: Option<String>,
    phase: u64,
    state: String,
    error: Option<String>,
    tokens: u64,
    tool_calls: u64,
    /// The FULL prompt the orchestrator handed the lane (not the ~400-char preview).
    prompt: Option<String>,
    /// Files the lane wrote or edited before it died — the concrete work worth not redoing.
    files: Vec<String>,
    /// Distinct shell commands it ran, deduped and capped.
    commands: Vec<String>,
    /// The lane's last substantive note to itself, which usually says where it had got to.
    last_note: Option<String>,
}

/// `--revive`: turn a dead or interrupted workflow run into per-lane resume prompts.
///
/// A `Workflow` run that dies mid-flight (an API session limit, a kill, a crash) loses every
/// in-progress lane at once, and the orchestrator's state file keeps only a ~400-char preview of
/// each prompt — so the obvious recovery, re-running the script, restarts every lane from zero and
/// throws away whatever they had already done. That is the expensive failure: our own runs have
/// lost multi-million-token waves this way.
///
/// The lanes' transcripts survive on disk regardless. This mines each one for the full original
/// prompt plus the work it actually landed (files written, commands run, its own last note) and
/// emits a ready-to-paste prompt for a standalone `Agent` — so lanes come back individually,
/// resumed rather than restarted, without needing the workflow runtime at all.
fn render_revive(w: &cv_core::Workflow, all: bool, session_path: Option<&std::path::Path>) -> Result<()> {
    let agents: Vec<&cv_core::WorkflowAgent> = w
        .phases
        .iter()
        .flat_map(|p| &p.agents)
        .chain(&w.orphan_agents)
        .filter(|a| all || a.state.as_deref() != Some("done"))
        .collect();

    if agents.is_empty() {
        println!(
            "no revivable lanes in {} (every agent completed; `--revive --all` to include them)",
            w.run_id
        );
        return Ok(());
    }

    println!("# revive {} — {} lane(s)", w.run_id, agents.len());
    if let Some(e) = &w.error {
        println!("run died: {}", truncate(e, 160));
    }
    println!(
        "\nEach block below is a standalone Agent prompt. The lane's own transcript supplied the\n\
         full task; the work log is mined from its tool calls so the revived lane does not redo it.\n"
    );

    for a in agents {
        let rev = mine_lane(a, &w.run_id, session_path);
        print_revive_block(&rev);
    }
    Ok(())
}

/// Pull one lane's full prompt and work log out of its own transcript.
///
/// Resolved by PATH, not by id lookup: a workflow lane's transcript always lives at
/// `<session>/subagents/workflows/<run_id>/agent-<id>.jsonl`, and we already know all three parts.
/// Going through the fleet-wide id resolver instead silently returns nothing for these — which is
/// how the first cut of this command ended up emitting truncated previews and empty work logs.
fn mine_lane(a: &cv_core::WorkflowAgent, run_id: &str, session_path: Option<&std::path::Path>) -> Revivable {
    let mut rev = Revivable {
        label: a.label.clone().unwrap_or_else(|| format!("agent#{}", a.index)),
        agent_id: a.agent_id.clone(),
        phase: a.phase_index,
        state: a.state.clone().unwrap_or_else(|| "unknown".into()),
        error: a.error.clone(),
        tokens: a.tokens.unwrap_or(0),
        tool_calls: a.tool_calls.unwrap_or(0),
        // Fall back to the ~400-char preview only if the transcript is genuinely unreadable.
        prompt: a.prompt_preview.clone(),
        files: Vec::new(),
        commands: Vec::new(),
        last_note: None,
    };

    let (Some(id), Some(sp)) = (a.agent_id.as_deref(), session_path) else {
        return rev;
    };
    let Some(stem) = sp.file_stem().and_then(|s| s.to_str()) else {
        return rev;
    };
    let dir = sp.with_file_name(stem).join("subagents").join("workflows").join(run_id);
    let bare = id.strip_prefix("agent-").unwrap_or(id);
    let path = dir.join(format!("agent-{bare}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return rev;
    };
    mine_transcript(&text, &mut rev);
    rev
}

/// The transcript-mining half of [`mine_lane`], split from path resolution so it can be tested
/// on JSONL text alone: the FULL first user prompt, the files the lane wrote or edited, its distinct
/// Bash commands (first line, capped), and its last substantive note.
fn mine_transcript(text: &str, rev: &mut Revivable) {
    let mut seen_files = std::collections::BTreeSet::new();
    let mut seen_cmds = std::collections::BTreeSet::new();
    let mut got_prompt = false;

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let role = v.pointer("/message/role").and_then(|r| r.as_str()).unwrap_or("");
        let content = match v.pointer("/message/content") {
            Some(c) => c,
            None => continue,
        };

        // The lane's task is the first user message — the FULL text the orchestrator handed it.
        if role == "user" && !got_prompt {
            let full = match content {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(bs) => bs
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .next()
                    .map(str::to_string),
                _ => None,
            };
            if let Some(f) = full.filter(|f| !f.trim().is_empty()) {
                rev.prompt = Some(f);
                got_prompt = true;
            }
        }

        let serde_json::Value::Array(blocks) = content else {
            continue;
        };
        for b in blocks {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let input = b.get("input");
                    match name {
                        "Write" | "Edit" | "NotebookEdit" => {
                            if let Some(f) = input.and_then(|i| i.get("file_path")).and_then(|f| f.as_str()) {
                                if seen_files.insert(f.to_string()) {
                                    rev.files.push(f.to_string());
                                }
                            }
                        }
                        "Bash" => {
                            if let Some(c) = input.and_then(|i| i.get("command")).and_then(|c| c.as_str()) {
                                let head: String = c.split('\n').next().unwrap_or(c).chars().take(70).collect();
                                if seen_cmds.insert(head.clone()) {
                                    rev.commands.push(head);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some("text") if role == "assistant" => {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if t.trim().len() > 80 {
                            rev.last_note = Some(t.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn print_revive_block(rev: &Revivable) {
    println!("\n{}", "=".repeat(78));
    println!("## lane: {}", rev.label);
    print!(
        "state={} phase={} tokens={} tools={}",
        rev.state,
        rev.phase,
        fmt_int(rev.tokens),
        fmt_int(rev.tool_calls)
    );
    if let Some(id) = &rev.agent_id {
        print!(" agent={}", short_id(id));
    }
    println!();
    if let Some(e) = &rev.error {
        println!("died: {}", truncate(e, 200));
    }
    if rev.prompt.is_none() {
        println!("\n(no prompt recoverable — neither transcript nor preview; re-issue by hand)");
        return;
    }

    // The work log only earns its place when the lane actually did something.
    let did_work = !rev.files.is_empty() || !rev.commands.is_empty();
    println!("\n--- 8< --- paste the block below as an Agent prompt --- 8< ---\n");
    println!("{}", rev.prompt.as_deref().unwrap_or_default());

    if did_work {
        println!(
            "\n\n--- RESUMING AN INTERRUPTED RUN ---\n\
             A previous run of this exact lane was cut short ({}) after {} tool calls. Its work is\n\
             still on disk. Do NOT start over — verify what is listed below, then continue from there.",
            rev.error
                .as_deref()
                .map(|e| truncate(e, 90))
                .unwrap_or_else(|| rev.state.clone()),
            fmt_int(rev.tool_calls),
        );
        if !rev.files.is_empty() {
            println!("\nFiles it already wrote or edited ({}):", rev.files.len());
            for f in rev.files.iter().take(40) {
                println!("  {}", f);
            }
            if rev.files.len() > 40 {
                println!("  … and {} more", rev.files.len() - 40);
            }
        }
        if !rev.commands.is_empty() {
            println!("\nCommands it ran ({} distinct, first 15):", rev.commands.len());
            for c in rev.commands.iter().take(15) {
                println!("  $ {}", c);
            }
        }
        if let Some(note) = &rev.last_note {
            println!(
                "\nIts own last note, which usually says where it had got to:\n{}",
                truncate(note, 900)
            );
        }
        println!("\n--- END RESUME CONTEXT ---");
    }
    println!("\n--- >8 --- end prompt --- >8 ---");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_duration_formatting() {
        assert_eq!(fmt_int(589698), "589,698");
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(42), "42");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_duration(3144884), "52m 24s");
        assert_eq!(fmt_duration(5_000), "5s");
        assert_eq!(fmt_duration(7_200_000), "2h 0m");
    }

    #[test]
    fn kindcell_blanks_zero() {
        assert_eq!(kindcell(0), "");
        assert_eq!(kindcell(3), "3");
    }

    #[test]
    fn revive_mines_prompt_files_commands_and_last_note() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"Full lane task: implement the widget end to end."}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Starting; I will scaffold first and then wire the tests, keeping the public surface unchanged for now."},{"type":"tool_use","name":"Write","input":{"file_path":"/repo/src/widget.rs","content":"..."}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test -p widget\necho done"}},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/widget.rs"}},{"type":"tool_use","name":"Bash","input":{"command":"cargo test -p widget"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Scaffold and tests landed in widget.rs; the remaining piece is the CLI flag, which I had not started when interrupted."}]}}"#,
        ]
        .join("\n");
        let mut rev = Revivable {
            label: "widget".into(),
            agent_id: Some("agent-1".into()),
            phase: 0,
            state: "error".into(),
            error: Some("session limit".into()),
            tokens: 0,
            tool_calls: 4,
            prompt: Some("Full lane task: impl…".into()), // the ~400-char preview, to be replaced
            files: vec![],
            commands: vec![],
            last_note: None,
        };
        mine_transcript(&lines, &mut rev);
        assert_eq!(
            rev.prompt.as_deref(),
            Some("Full lane task: implement the widget end to end."),
            "the FULL first user prompt replaces the preview"
        );
        assert_eq!(
            rev.files,
            vec!["/repo/src/widget.rs"],
            "Write + Edit of one file dedupe"
        );
        assert_eq!(rev.commands, vec!["cargo test -p widget"], "first line only, deduped");
        assert!(rev
            .last_note
            .as_deref()
            .is_some_and(|n| n.starts_with("Scaffold and tests landed")));
    }
}
