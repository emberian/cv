//! `cv adopt` — rescue sub-agents stranded by a dead Claude Code session into a live one, so the
//! live session can `SendMessage` them and they resume with their whole transcript. The mechanism
//! and its limits live in [`cv_core::harness::claude::adopt`]; this module resolves ids, prints, and gates writes.
//!
//! Human report on stderr; `--json` puts one JSON value on stdout. Finding the agents is
//! `cv lanes`'s job (`cv lanes <session>`, `cv lanes --recent N --unfinished`).

use crate::util::{fmt_local, home_rel, report, short_id, usage};
use anyhow::{Context, Result};
use cv_core::harness::claude::adopt::{self, AgentFile, Plan, SessionFile};
use cv_core::harness::claude::sidecar;
use cv_core::ir::human_bytes;
use serde_json::json;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Every Claude root cv reads (the store plus `CLUSTERVISION_CLAUDE_ROOTS` / `claude-roots`
/// entries): the fleet lookup spans all of them; named here only in a miss's message.
fn roots() -> Result<Vec<PathBuf>> {
    let roots = cv_core::harness::for_harness(cv_core::harness::derived_host())
        .map(|a| a.roots())
        .unwrap_or_default();
    if roots.is_empty() {
        anyhow::bail!("Claude Code's store (~/.claude/projects) was not found");
    }
    Ok(roots)
}

/// A session spec → its file, with a miss or an ambiguity as a usage error (exit 2).
fn session(spec: &str) -> Result<SessionFile> {
    adopt::find_session(spec).or_else(|e| usage(e.to_string()))
}

fn when(t: Option<std::time::SystemTime>) -> String {
    t.map(|t| fmt_local(chrono::DateTime::<chrono::Utc>::from(t), "%m-%d %H:%M"))
        .unwrap_or_else(|| "?".into())
}

fn short_model(m: Option<&str>) -> String {
    m.map(|m| m.strip_prefix("claude-").unwrap_or(m).to_string())
        .unwrap_or_else(|| "?".into())
}

pub(crate) struct AdoptArgs {
    pub agents: Vec<String>,
    pub into: Option<String>,
    pub from: Option<String>,
    pub dry_run: bool,
    pub force: bool,
    /// Written to each adopted agent's meta `model` (see `Cmd::Adopt`).
    pub model: Option<String>,
    pub json: bool,
}

/// Set `model` in a sub-agent's meta sidecar, creating the sidecar when the source had none.
/// `pickedEffortModel` (the model Claude Code picked at launch) is dropped: beside the new `model`
/// it would name a second, stale one.
fn set_meta_model(path: &std::path::Path, model: &str) -> Result<()> {
    let mut meta = sidecar::agent_meta(path);
    meta.insert("model".into(), json!(model));
    meta.remove("pickedEffortModel");
    let meta = serde_json::Value::Object(meta);
    // The meta adopt itself just published (or none): replaced whole.
    cv_core::publish::publish_reader(
        path,
        "meta",
        meta.to_string().as_bytes(),
        cv_core::publish::Claim::Replace,
    )
    .with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn cmd_adopt(a: AdoptArgs) -> Result<()> {
    let roots = roots()?;
    run(&roots, &a)
}

fn run(roots: &[PathBuf], a: &AdoptArgs) -> Result<()> {
    let from = a.from.as_deref().map(session).transpose()?;
    let into_given = a.into.as_deref().map(session).transpose()?;

    // Every copy of each named agent (newest first).
    let mut found: Vec<Vec<AgentFile>> = Vec::new();
    for spec in &a.agents {
        let hits = adopt::find_agent(spec, from.as_ref()).or_else(|e| usage(e.to_string()))?;
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
            let s = adopt::newest_session(slug, &[first.session_id.as_str()])
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

    // An agent already in the live session and nowhere else has nothing to copy; with `--model`
    // it is retargeted in place (its meta rewritten, the transcript untouched).
    let mut plans: Vec<Plan> = Vec::new();
    let mut retargets: Vec<AgentFile> = Vec::new();
    for (spec, hits) in a.agents.iter().zip(found) {
        let (mut here, elsewhere): (Vec<AgentFile>, Vec<AgentFile>) =
            hits.into_iter().partition(|h| h.session_id == into.session_id);
        let mut elsewhere = elsewhere.into_iter();
        let Some(chosen) = elsewhere.next() else {
            if a.model.is_some() && !here.is_empty() {
                retargets.push(here.swap_remove(0));
                continue;
            }
            return usage(format!(
                "{spec} is already in {} and nowhere else{} — nothing to adopt \
                 (`--model <ID>` retargets its model in place)",
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

    // Repeated prefixes/full ids can resolve to the same agent. Reject the entire request before
    // the first copy rather than discovering the collision after one adoption has been published.
    let mut destinations = std::collections::HashSet::new();
    let picked = plans.iter().map(|p| (&p.dest, &p.agent.agent_id));
    for (dest, id) in picked.chain(retargets.iter().map(|g| (&g.path, &g.agent_id))) {
        if !destinations.insert(dest) {
            return usage(format!("agent {id} was selected more than once — nothing was written"));
        }
    }

    // All-or-nothing: refuse before writing anything.
    let blocked: Vec<&Plan> = plans.iter().filter(|p| p.exists).collect();
    if !blocked.is_empty() && !a.force && !a.dry_run {
        let names: Vec<String> = blocked.iter().map(|p| p.dest.display().to_string()).collect();
        return usage(format!(
            "refusing to overwrite (pass --force to replace; nothing was written):\n{}",
            names.join("\n")
        ));
    }

    let mut done = Vec::new();
    let mut h = String::new();
    for p in &plans {
        let g = &p.agent;
        // A dry run still restamps in memory, so its line counts are the real ones.
        let r = adopt::execute(p, !a.dry_run, a.force || a.dry_run)?;
        let meta_dest = p
            .dest_meta
            .clone()
            .unwrap_or_else(|| p.dest.with_extension("meta.json"));
        if !a.dry_run {
            if let Some(m) = &a.model {
                set_meta_model(&meta_dest, m)?;
            }
            let agent_session_id = format!("agent-{}", g.agent_id);
            let host_harness = cv_core::harness::derived_host().as_str();
            cv_core::lanes::record_lineage_edge_best_effort(
                &g.session_id,
                &agent_session_id,
                "parent_subagent",
                Some(host_harness),
                None,
                json!({
                    "agent_id": g.agent_id,
                }),
            );
            cv_core::lanes::record_lineage_edge_best_effort(
                &agent_session_id,
                &into.session_id,
                "adopted_into",
                Some(host_harness),
                None,
                json!({
                    "from_session_id": g.session_id,
                    "agent_id": g.agent_id,
                    "model": a.model.as_deref().or(g.model.as_deref()),
                }),
            );
        }
        let verb = if a.dry_run { "would adopt" } else { "adopted" };
        let _ = writeln!(
            h,
            "✦ {verb} {} — {} ({}, {})",
            g.agent_id,
            g.description.as_deref().unwrap_or("(no description)"),
            short_model(g.model.as_deref()),
            human_bytes(g.bytes)
        );
        let _ = writeln!(h, "  from {} ({})", g.session_id, home_rel(&g.project_dir));
        if !p.other_copies.is_empty() {
            let _ = writeln!(
                h,
                "  (older copies also in: {} — took the newest)",
                p.other_copies
                    .iter()
                    .map(|s| short_id(s))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if into.project_dir.file_name() != g.project_dir.file_name() {
            let _ = writeln!(
                h,
                "  ⚠ crossing projects: the agent's cwd stays {} as recorded in its transcript",
                home_rel(&g.project_dir)
            );
        }
        let _ = writeln!(
            h,
            "  ↳ {} → {}  (sessionId restamped on {} of {} lines)",
            home_rel(&g.path),
            home_rel(&p.dest),
            r.stamped,
            r.lines
        );
        match (&g.meta_path, &p.dest_meta) {
            (Some(src), Some(dest)) => {
                let _ = writeln!(h, "  ↳ {} → {}", home_rel(src), home_rel(dest));
            }
            _ if a.model.is_some() => {
                let _ = writeln!(h, "  ↳ {} (new: the source had none)", home_rel(&meta_dest));
            }
            _ => {
                let _ = writeln!(
                    h,
                    "  ⚠ no .meta.json in the source — the agent's type/model/description are not carried"
                );
            }
        }
        if let Some(m) = &a.model {
            let _ = writeln!(h, "  ↳ model: {m}");
        }
        if p.exists {
            let _ = writeln!(
                h,
                "  {} {} already exists",
                if a.force { "↳ replacing:" } else { "✗ would refuse:" },
                home_rel(&p.dest)
            );
        }
        let _ = writeln!(
            h,
            "  ↳ resume, from session {}: SendMessage {{\"to\": \"{}\", \"message\": \"<next instruction>\"}}",
            short_id(&into.session_id),
            g.agent_id
        );
        done.push(json!({"plan": p, "result": r, "written": !a.dry_run}));
    }
    let mut retargeted = Vec::new();
    for g in &retargets {
        let model = a.model.as_deref().expect("a retarget is planned only with --model");
        let meta = g
            .meta_path
            .clone()
            .unwrap_or_else(|| g.path.with_extension("meta.json"));
        if !a.dry_run {
            set_meta_model(&meta, model)?;
        }
        let _ = writeln!(
            h,
            "✦ {} {} in place: model {} → {model}  ({})",
            if a.dry_run { "would retarget" } else { "retargeted" },
            g.agent_id,
            g.model.as_deref().unwrap_or("(unset)"),
            home_rel(&meta)
        );
        retargeted.push(json!({
            "agent_id": g.agent_id,
            "meta": meta,
            "old_model": g.model,
            "model": model,
            "written": !a.dry_run,
        }));
    }
    let source_sessions: std::collections::BTreeSet<&str> = plans.iter().map(|p| p.agent.session_id.as_str()).collect();
    for source in &source_sessions {
        let _ = writeln!(
            h,
            "  ⚑ keep {source}'s directory: these agents' persisted outputs stay in its tool-results/ \
             (named by absolute path), and what `cv prune` snipped from it stays in its sidecar — \
             `cv cat {source} <tool_use_id>`; an agent's own calls: `cv cat agent-<id> <tool_use_id>`"
        );
    }
    if a.dry_run {
        let _ = writeln!(h, "  (dry run: nothing was written)");
    }
    report(
        a.json,
        &h,
        &json!({"into": into.session_id, "dry_run": a.dry_run, "agents": done, "retargeted": retargeted}),
    )
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn requested_model_is_written_even_when_existing_metadata_is_not_an_object() {
        let world = cv_testkit::World::new("adopt-meta-model");
        let path = world.base.join("agent.meta.json");
        for original in ["null", "[]", "\"not metadata\"", "42", "not json"] {
            std::fs::write(&path, original).unwrap();
            set_meta_model(&path, "requested-model").unwrap();
            let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(updated["model"], "requested-model");
            assert_eq!(updated["agentType"], "general-purpose");
        }
        std::fs::write(&path, r#"{"model":"old","description":"keep","extra":{"a":1}}"#).unwrap();
        set_meta_model(&path, "requested-model").unwrap();
        let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            updated,
            json!({"agentType":"general-purpose","model":"requested-model","description":"keep","extra":{"a":1}}),
            "a sidecar naming no type gets the default; every other key is the source's"
        );
        std::fs::write(&path, r#"{"agentType":"Explore","model":"old"}"#).unwrap();
        set_meta_model(&path, "requested-model").unwrap();
        let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            updated,
            json!({"agentType":"Explore","model":"requested-model"}),
            "never overwritten"
        );
    }
}
