//! `cv adopt` — rescue sub-agents stranded by a dead Claude Code session into a live one, so the
//! live session can `SendMessage` them and they resume with their whole transcript. The mechanism
//! and its limits live in [`cv_core::adopt`]; this module resolves ids, prints, and gates writes.
//!
//! Human report on stderr; `--json` puts one JSON value on stdout. `--list` and `--orphans` print
//! their table on stdout.

use crate::cmd::compose::human_bytes;
use crate::util::{fmt_local, home_rel, short_id, usage};
use anyhow::{Context, Result};
use cv_core::adopt::{self, AgentFile, Plan, SessionFile};
use cv_core::ir::Harness;
use serde_json::json;
use std::path::PathBuf;

/// Where this environment's Claude Code writes (`$CLAUDE_CONFIG_DIR/projects` or
/// `~/.claude/projects`): the project dir for a cwd is derived here.
fn store() -> Result<PathBuf> {
    cv_core::harness::for_harness(Harness::Claude)
        .and_then(|a| a.storage_root())
        .context("Claude Code's store (~/.claude/projects) was not found")
}

/// Every Claude root cv reads (the store plus `CLUSTERVISION_CLAUDE_ROOTS` / `claude-roots`
/// entries): sessions and agents are looked up across all of them.
fn roots() -> Result<Vec<PathBuf>> {
    let roots = cv_core::harness::for_harness(Harness::Claude)
        .map(|a| a.storage_roots())
        .unwrap_or_default();
    if roots.is_empty() {
        anyhow::bail!("Claude Code's store (~/.claude/projects) was not found");
    }
    Ok(roots)
}

/// A session spec → its file, with a miss or an ambiguity as a usage error (exit 2).
fn session(roots: &[PathBuf], spec: &str) -> Result<SessionFile> {
    adopt::find_session(roots, spec).or_else(|e| usage(e.to_string()))
}

fn when(t: Option<std::time::SystemTime>) -> String {
    t.map(|t| fmt_local(chrono::DateTime::<chrono::Utc>::from(t), "%m-%d %H:%M"))
        .unwrap_or_else(|| "?".into())
}

fn short_model(m: Option<&str>) -> String {
    m.map(|m| m.strip_prefix("claude-").unwrap_or(m).to_string())
        .unwrap_or_else(|| "?".into())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub(crate) struct AdoptArgs {
    pub agents: Vec<String>,
    pub into: Option<String>,
    pub from: Option<String>,
    pub dry_run: bool,
    pub force: bool,
    pub list: Option<String>,
    pub orphans: bool,
    pub recent: usize,
    pub json: bool,
}

pub(crate) fn cmd_adopt(a: AdoptArgs) -> Result<()> {
    let roots = roots()?;
    if let Some(spec) = &a.list {
        return list(&roots, spec, a.json);
    }
    if a.orphans {
        return orphans(&roots, a.into.as_deref(), a.recent, a.json);
    }
    run(&roots, &a)
}

fn list(roots: &[PathBuf], spec: &str, json_out: bool) -> Result<()> {
    let s = session(roots, spec)?;
    let agents = adopt::session_agents(&s);
    let lanes = adopt::lanes_of_session(&s);
    let status = |id: &str| {
        lanes
            .iter()
            .find(|l| l.agent_id == id)
            .map(|l| {
                if l.stranded {
                    "stranded".to_string()
                } else {
                    l.status.clone()
                }
            })
            .unwrap_or_else(|| "?".into())
    };
    if json_out {
        let rows: Vec<_> = agents
            .iter()
            .map(|g| {
                let mut v = serde_json::to_value(g).unwrap_or_default();
                v["has_meta"] = json!(g.meta_path.is_some());
                v["status"] = json!(status(&g.agent_id));
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!(
        "# sub-agents of {} ({}) — {} agents",
        s.session_id,
        home_rel(&s.project_dir),
        agents.len()
    );
    println!();
    println!(
        "{:<18} {:<10} {:<12} {:>9} {:<4} {:<11}  DESCRIPTION",
        "AGENT", "STATUS", "MODEL", "SIZE", "META", "LAST WRITE"
    );
    for g in &agents {
        println!(
            "{:<18} {:<10} {:<12} {:>9} {:<4} {:<11}  {}",
            g.agent_id,
            status(&g.agent_id),
            trunc(&short_model(g.model.as_deref()), 12),
            human_bytes(g.bytes),
            if g.meta_path.is_some() { "yes" } else { "NO" },
            when(g.modified),
            g.description.as_deref().unwrap_or("")
        );
    }
    if !agents.is_empty() {
        println!();
        println!(
            "adopt one into the newest session of this project: cv adopt <agent-id> --from {}",
            short_id(&s.session_id)
        );
    }
    Ok(())
}

fn orphans(roots: &[PathBuf], into: Option<&str>, recent: usize, json_out: bool) -> Result<()> {
    let (project, live) = match into {
        Some(spec) => {
            let s = session(roots, spec)?;
            (s.project_dir.clone(), s.session_id)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let project = adopt::project_dir_for_cwd(&store()?, &cwd);
            let Some(live) = adopt::newest_session(&project, &[]) else {
                return usage(format!(
                    "no Claude sessions for {} ({}) — run from a project dir or pass --into <live-session>",
                    cwd.display(),
                    home_rel(&project)
                ));
            };
            (project, live.session_id)
        }
    };
    let found = adopt::orphans(&project, &live, recent);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"live_session": live, "project_dir": project, "orphans": found}))?
        );
        return Ok(());
    }
    println!(
        "# orphans in {} — unfinished sub-agents of the {recent} most recent sessions other than live {}",
        home_rel(&project),
        short_id(&live)
    );
    println!("# unfinished = no completion recorded (running), stopped/killed/failed, or stranded on a promise");
    println!();
    if found.is_empty() {
        println!("(none)");
        return Ok(());
    }
    println!(
        "{:<8} {:<18} {:<10} {:<12} {:<11}  DESCRIPTION",
        "SESSION", "AGENT", "STATUS", "MODEL", "LAST TURN"
    );
    for o in &found {
        let l = &o.lane;
        println!(
            "{:<8} {:<18} {:<10} {:<12} {:<11}  {}",
            short_id(&o.session_id),
            l.agent_id,
            if l.stranded { "stranded" } else { l.status.as_str() },
            trunc(&short_model(l.model.as_deref()), 12),
            l.last_turn_at
                .map(|t| fmt_local(t, "%m-%d %H:%M"))
                .unwrap_or_else(|| "?".into()),
            l.description.as_deref().unwrap_or("")
        );
    }
    println!();
    println!("adopt into {}: cv adopt <agent-id>… --into {}", short_id(&live), live);
    Ok(())
}

fn run(roots: &[PathBuf], a: &AdoptArgs) -> Result<()> {
    let from = a.from.as_deref().map(|f| session(roots, f)).transpose()?;
    let into_given = a.into.as_deref().map(|i| session(roots, i)).transpose()?;

    // Every copy of each named agent (newest first).
    let mut found: Vec<Vec<AgentFile>> = Vec::new();
    for spec in &a.agents {
        let hits = adopt::find_agent(roots, spec, from.as_ref()).or_else(|e| usage(e.to_string()))?;
        if hits.is_empty() {
            return usage(match &from {
                Some(f) => format!("no sub-agent {spec:?} in {}/subagents/", f.session_id),
                None => format!(
                    "no sub-agent {spec:?} in any session's subagents/ under {}",
                    roots.iter().map(|r| home_rel(r)).collect::<Vec<_>>().join(", ")
                ),
            });
        }
        found.push(hits);
    }

    // The live session: named, or the newest in the (first) agent's project other than the one
    // that holds the agent — a session that just crashed is often the newest file, and it is the
    // one we are rescuing FROM. The project is matched by slug across every root.
    let into = match into_given {
        Some(s) => s,
        None => {
            let first = &found[0][0];
            let slug = first
                .project_dir
                .file_name()
                .context("the agent's project dir has no name")?;
            let s = adopt::newest_session_across(roots, slug, &[first.session_id.as_str()])
                .context("the agent's project has no other session to adopt into (pass --into)")?;
            eprintln!(
                "✦ into {} — the newest session in {} (last write {}); pass --into to choose another",
                s.session_id,
                home_rel(&s.project_dir),
                when(s.modified)
            );
            s
        }
    };

    let mut plans: Vec<Plan> = Vec::new();
    for (spec, hits) in a.agents.iter().zip(found) {
        let (here, elsewhere): (Vec<AgentFile>, Vec<AgentFile>) =
            hits.into_iter().partition(|h| h.session_id == into.session_id);
        let mut elsewhere = elsewhere.into_iter();
        let Some(chosen) = elsewhere.next() else {
            return usage(format!(
                "{spec} is already in {} and nowhere else{} — nothing to adopt",
                into.session_id,
                if here.is_empty() {
                    ""
                } else {
                    " (it was adopted or spawned there)"
                }
            ));
        };
        let others = elsewhere.map(|h| h.session_id).collect();
        plans.push(adopt::plan(chosen, others, &into));
    }

    // All-or-nothing: refuse before writing anything.
    let blocked: Vec<&Plan> = plans.iter().filter(|p| p.exists).collect();
    if !blocked.is_empty() && !a.force && !a.dry_run {
        let names: Vec<String> = blocked.iter().map(|p| p.dest.display().to_string()).collect();
        anyhow::bail!(
            "refusing to overwrite (pass --force to replace; nothing was written):\n{}",
            names.join("\n")
        );
    }

    let mut done = Vec::new();
    for p in &plans {
        let g = &p.agent;
        // A dry run still restamps in memory, so its line counts are the real ones.
        let r = adopt::execute(p, !a.dry_run, a.force || a.dry_run)?;
        let verb = if a.dry_run { "would adopt" } else { "adopted" };
        eprintln!(
            "✦ {verb} {} — {} ({}, {})",
            g.agent_id,
            g.description.as_deref().unwrap_or("(no description)"),
            short_model(g.model.as_deref()),
            human_bytes(g.bytes)
        );
        eprintln!("  from {} ({})", g.session_id, home_rel(&g.project_dir));
        if !p.other_copies.is_empty() {
            eprintln!(
                "  (older copies also in: {} — took the newest)",
                p.other_copies
                    .iter()
                    .map(|s| short_id(s))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if into.project_dir.file_name() != g.project_dir.file_name() {
            eprintln!(
                "  ⚠ crossing projects: the agent's cwd stays {} as recorded in its transcript",
                home_rel(&g.project_dir)
            );
        }
        eprintln!(
            "  ↳ {} → {}  (sessionId restamped on {} of {} lines)",
            home_rel(&g.path),
            home_rel(&p.dest),
            r.stamped,
            r.lines
        );
        match (&g.meta_path, &p.dest_meta) {
            (Some(src), Some(dest)) => eprintln!("  ↳ {} → {}", home_rel(src), home_rel(dest)),
            _ => eprintln!("  ⚠ no .meta.json in the source — the agent's type/model/description are not carried"),
        }
        if p.exists {
            eprintln!(
                "  {} {} already exists",
                if a.force { "↳ replacing:" } else { "✗ would refuse:" },
                home_rel(&p.dest)
            );
        }
        eprintln!(
            "  ↳ resume, from session {}: SendMessage {{\"to\": \"{}\", \"message\": \"<next instruction>\"}}",
            short_id(&into.session_id),
            g.agent_id
        );
        done.push(json!({"plan": p, "result": r, "written": !a.dry_run}));
    }
    let deads: std::collections::BTreeSet<&str> = plans.iter().map(|p| p.agent.session_id.as_str()).collect();
    for dead in &deads {
        eprintln!(
            "  ⚑ keep {dead}'s directory: these agents' persisted outputs stay in its tool-results/ \
             (named by absolute path), and what `cv prune` snipped from it stays in its sidecar — \
             `cv cat {dead} <tool_use_id>`; an agent's own calls: `cv cat agent-<id> <tool_use_id>`"
        );
    }
    if a.dry_run {
        eprintln!("  (dry run: nothing was written)");
    }
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"into": into.session_id, "dry_run": a.dry_run, "agents": done}))?
        );
    }
    Ok(())
}
