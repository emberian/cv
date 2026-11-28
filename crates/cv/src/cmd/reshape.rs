//! `cv prune` / `cv rewind` / `cv splice` / `cv dataset` — reshaping sessions into new ones.

use crate::cmd::port::emit_session;
use crate::util::{dry_run_nulls, parse_harness, parse_range, report, short_id, usage};
use anyhow::{Context, Result};
use cv_core::harness::claude::carry::{carry_agents, Carried, CarryAgents};
use cv_core::harness::claude::skills::SkillsPolicy;
use cv_core::ir::{human_bytes, Harness, Session};
use cv_core::EmitOptions;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

// ---------- carrying agents (prune, rewind, splice) ----------

/// `--carry-agents` copies a session's own `subagents/`; a sub-agent transcript has none.
fn carry_source(mode: Option<CarryAgents>, r: &cv_core::ir::SessionRef) -> Result<()> {
    if mode.is_some() && cv_core::rewind::is_subagent(r) {
        return usage(format!(
            "--carry-agents carries a top-level session's sub-agents; {} is itself a sub-agent",
            short_id(&r.id)
        ));
    }
    Ok(())
}

/// `--carry-agents`, after the new session is written: the report line, and the JSON's uniform
/// `carried_agents` object (`dir: null` on an unpinned dry run).
fn carry(
    mode: CarryAgents,
    sources: &[PathBuf],
    new_path: &std::path::Path,
    new_id: &str,
    (dry_run, unnamed): (bool, bool),
    h: &mut String,
) -> Result<serde_json::Value> {
    let carried = carry_agents(sources, new_path, new_id, mode, &[], dry_run).with_context(|| {
        if dry_run {
            "--carry-agents".to_string()
        } else {
            format!("the new session {new_id} is written; carrying its agents failed")
        }
    })?;
    let mut v = serde_json::to_value(&carried)?;
    if unnamed {
        v["dir"] = serde_json::Value::Null;
    }
    if carried.is_empty() {
        let _ = writeln!(h, "  agents:      the source has none to carry");
    } else if unnamed {
        let line = carried_line(&carried);
        let did = line.split(" → ").next().unwrap_or_default();
        let _ = writeln!(h, "  agents:      {did} → the new session's subagents/");
        refused_lines(h, &carried);
    } else {
        let _ = writeln!(h, "  agents:      {}", carried_line(&carried));
        refused_lines(h, &carried);
    }
    Ok(v)
}

/// What a carry did, in one line: what happened first, each clause only when it did, then where.
pub(crate) fn carried_line(c: &Carried) -> String {
    let clauses = [
        (c.metas, "spawn record(s) copied"),
        (c.transcripts, "transcript(s) copied"),
        (
            c.joined,
            "transcript(s) joined (the source's records first, then what the session already held)",
        ),
        (c.unfinished, "transcript(s) carried up to an unfinished last record"),
        (c.refused.len(), "transcript(s) refused"),
        (c.already, "file(s) already there, left as they are"),
    ];
    let done: Vec<String> = clauses
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    let mut line = if done.is_empty() {
        "nothing to carry".to_string()
    } else {
        done.join(", ")
    };
    let _ = write!(line, " → {}", c.dir.display());
    if c.duplicates > 0 {
        let _ = write!(line, " ({} file(s) another source already supplied)", c.duplicates);
    }
    line
}

/// One line per transcript a carry refused, under [`carried_line`]: a warning, the session is
/// written either way.
pub(crate) fn refused_lines(h: &mut String, c: &Carried) {
    const SHOWN: usize = 8;
    for r in c.refused.iter().take(SHOWN) {
        let _ = writeln!(h, "  ⚠ agent {} not carried: {}", r.agent, r.why);
    }
    if c.refused.len() > SHOWN {
        let _ = writeln!(h, "  ⚠ … and {} more (--json lists them)", c.refused.len() - SHOWN);
    }
}

// ---------- prune ----------

/// `cv prune <id>` — compact a Claude session into a new resumable one (see [`cv_core::prune`]).
/// Stashed originals come back with `cv cat <new-id> <tool_use_id>`.
/// Resolve the caller-supplied `--declassify` term list from a CSV flag and/or a file (one term per
/// line, `#` comments + blanks ignored). Terms are lowercased + de-duplicated. cv ships NO built-in
/// list — the terms are always external (config/data), so cv's own source carries no domain shitlist.
pub(crate) fn resolve_declassify_tokens(csv: Option<String>, file: Option<PathBuf>) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |t: &str| {
        let t = t.trim().to_ascii_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    };
    if let Some(csv) = csv {
        for t in csv.split(',') {
            push(t);
        }
    }
    if let Some(path) = file {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --declassify-tokens-file {}", path.display()))?;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("");
            push(line);
        }
    }
    Ok(out)
}

pub(crate) struct PruneArgs<'a> {
    pub agent: Option<String>,
    pub id: Option<&'a str>,
    pub harness: Option<String>,
    pub min_size: usize,
    pub keep_last: usize,
    pub to: Option<String>,
    pub force: bool,
    pub out: Option<PathBuf>,
    pub drop: bool,
    pub thinking: bool,
    pub copy_resources: bool,
    pub carry_agents: Option<CarryAgents>,
    pub revive: bool,
    pub revive_estimate: bool,
    pub window: Option<u64>,
    pub keep: Option<String>,
    pub replace_summary: Option<PathBuf>,
    pub title: Option<String>,
    pub declassify: bool,
    pub declassify_tokens: Option<String>,
    pub declassify_tokens_file: Option<PathBuf>,
    pub skills_policy: SkillsPolicy,
    pub plan: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
}

/// Read a `--plan` file; an unreadable or malformed plan is a usage error (exit 2).
fn read_plan(path: &std::path::Path) -> Result<cv_core::prune::Plan> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return usage(format!("--plan {}: {e}", path.display())),
    };
    cv_core::prune::Plan::parse(&text).or_else(|e| usage(format!("--plan {}: {e}", path.display())))
}

pub(crate) fn cmd_prune(args: PruneArgs<'_>) -> Result<()> {
    let PruneArgs {
        agent,
        id,
        harness,
        min_size,
        keep_last,
        to,
        force,
        out,
        drop,
        thinking,
        copy_resources,
        carry_agents,
        revive,
        revive_estimate,
        window,
        keep,
        replace_summary,
        title,
        declassify,
        declassify_tokens,
        declassify_tokens_file,
        skills_policy,
        plan,
        dry_run,
        json,
    } = args;
    let plan = plan.as_deref().map(read_plan).transpose()?;
    if let Some(id) = to.as_deref() {
        if let Err(e) = cv_core::harness::validate_session_id(id) {
            return usage(e.to_string());
        }
    }
    let (keep_native_steps, keep_spec) = match keep.as_deref() {
        Some(s) => match s.strip_prefix("steps:").or_else(|| s.strip_prefix("step:")) {
            Some(rest) => (true, Some(rest)),
            None => (false, Some(s)),
        },
        None => (false, None),
    };
    let keep_range = keep_spec.map(parse_range).transpose()?;
    let summary_text = match replace_summary.as_deref() {
        Some(path) => Some(
            fs::read_to_string(path)
                .map_err(|e| crate::util::UsageError(format!("--replace-summary {}: {e}", path.display())))?,
        ),
        None => None,
    };
    let declassify_tokens = resolve_declassify_tokens(declassify_tokens, declassify_tokens_file)?;
    if declassify && declassify_tokens.is_empty() {
        eprintln!(
            "cv prune --declassify: no terms supplied (--declassify-tokens / \
             --declassify-tokens-file) — nothing will be snipped. cv ships no built-in list."
        );
    }
    let want = parse_harness(&harness)?;
    let r = match (id, agent.as_deref()) {
        (Some(id), agent) => {
            let (parent, _) = crate::cmd::distill::resolve_source(id, want)?;
            match agent {
                Some(agent) => crate::cmd::view::resolve_subagent(&parent, agent)?.session,
                None => parent,
            }
        }
        (None, Some(agent)) => {
            let spec = format!("agent-{}", agent.strip_prefix("agent-").unwrap_or(agent));
            crate::cmd::distill::resolve_source(&spec, want)?.0
        }
        (None, None) => return usage("prune requires a source session or --agent"),
    };
    carry_source(carry_agents, &r)?;

    if !cv_core::harness::has_derived_copies(r.harness) {
        if cv_core::harness::has_cold_rewind(r.harness) {
            let claude_only = [
                (plan.is_some(), "--plan"),
                (declassify, "--declassify"),
                (carry_agents.is_some(), "--carry-agents"),
                (skills_policy != SkillsPolicy::Keep, "--skills"),
                (revive_estimate, "--revive-estimate"),
            ];
            if let Some((_, flag)) = claude_only.iter().find(|(set, _)| *set) {
                return usage(format!(
                    "{flag} prunes Claude Code records; {} sessions prune native SQLite steps",
                    r.harness
                ));
            }
            return prune_cold_antigravity(
                &r,
                ColdAntigravityPruneArgs {
                    min_size,
                    keep_last,
                    drop,
                    thinking,
                    copy_resources,
                    window,
                    keep_range,
                    keep_native_steps,
                    replace_summary: summary_text,
                    title,
                    to,
                    force,
                    out,
                    dry_run,
                    json,
                },
            );
        }
        let native_only = [
            (plan.is_some(), "--plan"),
            (window.is_some(), "--window"),
            (keep_range.is_some(), "--keep"),
            (replace_summary.is_some(), "--replace-summary"),
            (title.is_some(), "--title"),
            (declassify, "--declassify"),
            (thinking, "--drop-thinking"),
            (copy_resources, "--copy-resources"),
            (carry_agents.is_some(), "--carry-agents"),
            (skills_policy != SkillsPolicy::Keep, "--skills"),
            (revive_estimate, "--revive-estimate"),
        ];
        if let Some((_, flag)) = native_only.iter().find(|(set, _)| *set) {
            return usage(format!(
                "{flag} prunes native records; {} sessions prune by selection",
                r.harness
            ));
        }
        return prune_ir(
            &r,
            IrPruneArgs {
                min_size,
                keep_last,
                drop,
                to,
                out,
                dry_run,
                json,
            },
        );
    }
    if keep_native_steps {
        return usage("--keep steps:A..B addresses native Antigravity SQLite step indices; pass --keep A..B");
    }
    if replace_summary.is_some() || title.is_some() {
        return usage("--replace-summary and --title apply to native Antigravity checkpoint sessions");
    }
    if out.is_some() {
        return usage("--out writes a pruned session of another harness; a Claude prune writes beside its source");
    }

    let pinned_id = to.is_some(); // --to: the new id is caller-chosen, so a dry run can still report it
    let opts = cv_core::prune::PruneOptions {
        min_size,
        keep_last,
        drop,
        thinking,
        new_id: to,
        copy_resources,
        revive,
        revive_estimate,
        window,
        keep_range,
        declassify,
        declassify_tokens, // caller-supplied (external); cv ships no built-in list. min_hits from Default (2).
        skills_policy,
        plan,
        dry_run,
        force,
        ..Default::default()
    };
    let res = cv_core::prune::prune_session(r.harness, &r.path, &opts).map_err(|e| match e
        .downcast_ref::<cv_core::prune::PlanRefused>()
    {
        Some(refused) => crate::util::UsageError(refused.to_string()).into(),
        None => e,
    })?;

    let mut v = serde_json::to_value(&res)?;
    v["harness"] = r.harness.as_str().into();
    // Existing JSON names describe a metadata rewrite: recorded_tokens_after is the estimate
    // written into usage, not new provider-measured prompt usage or verified resume acceptance.
    v["revived"] = match (res.revive_tokens, res.revive_old_tokens) {
        (Some(estimated), Some(recorded)) => serde_json::json!({
            "recorded_tokens_before": recorded,
            "recorded_tokens_after": estimated,
            "usage_records_rewritten": res.usage_rewritten,
        }),
        _ => serde_json::Value::Bool(false),
    };
    dry_run_nulls(
        &mut v,
        pinned_id,
        &["new_path", "sidecar_path", "copied_resources"],
        &["new_id"],
    );

    let mut h = String::new();
    let pct = if res.original_size > 0 {
        100.0 * res.bytes_saved() as f64 / res.original_size as f64
    } else {
        0.0
    };
    let tag = if dry_run { " (dry run — nothing written)" } else { "" };
    let _ = writeln!(
        h,
        "✦ pruned {} → {}{tag}",
        short_id(&res.source_id),
        short_id(&res.new_id)
    );
    let _ = writeln!(
        h,
        "  {} payload(s) snipped ({} image(s)) · {} → {} on disk (−{:.0}%) · ~{} token equivalents removed (byte estimate)",
        res.pruned_count,
        res.image_blocks,
        human_bytes(res.original_size),
        human_bytes(res.new_size),
        pct,
        res.est_context_tokens_saved,
    );
    if dry_run || opts.plan.is_some() {
        let _ = writeln!(
            h,
            "  cache break-even n* = {} ({})",
            res.cache_break_even.n_star, res.cache_break_even.assumptions
        );
        match (res.invalidated_suffix_tokens, res.cache_break_even_turns) {
            (Some(_), Some(_)) if res.cache_cold => {
                let _ = writeln!(
                    h,
                    "  cache rebuild: the cache is cold (idle over an hour), so it pays back at once"
                );
            }
            (Some(suffix), Some(n)) => {
                let _ = writeln!(
                    h,
                    "  cache rebuild: ~{}k cached tokens invalidated from the first pruned record on · break-even after {n} turn(s)",
                    suffix / 1000
                );
            }
            (Some(_), None) => {
                let _ = writeln!(h, "  cache rebuild: nothing saved, so it never pays back");
            }
            _ => {}
        }
    }
    if res.dropped_turns > 0 {
        let sized = match res.window_real_tokens {
            Some(real) => format!(
                " · kept ~{}k tokens by recorded-usage deltas (new prompt size and system overhead unmeasured)",
                real / 1000
            ),
            None => " (sized by byte estimate — no usage records)".to_string(),
        };
        let _ = writeln!(
            h,
            "  sliding window: dropped {} older turn(s){} — the source keeps the full history",
            res.dropped_turns, sized
        );
    }
    if revive {
        match (res.revive_tokens, res.revive_old_tokens) {
            (Some(estimated), Some(recorded)) => {
                let _ = writeln!(
                    h,
                    "  revived: {} usage record(s) recorded above the post-prune estimate were rewritten to it: \
                     highest recorded {}k → estimated {}k (bytes and recorded deltas, with margins; not a measurement)",
                    res.usage_rewritten,
                    recorded / 1000,
                    estimated / 1000,
                );
            }
            _ => {
                if let Some(overhead) = res.inherited_usage_warning {
                    let _ = writeln!(
                        h,
                        "  ⚠ recorded prompt size implies ~{}k fixed-overhead tokens above visible turns \
                         (likely inherited from a fold/splice without a compaction boundary); \
                         pass --revive-estimate to rewrite from visible content",
                        overhead / 1000,
                    );
                } else {
                    let _ = writeln!(h, "  revive: no recorded usage required an estimated-window rewrite");
                }
            }
        }
    }
    // The number the harness acts on is neither of those two unless it is the last record's.
    if let (Some(before), Some(after)) = (res.last_request_tokens, res.last_request_tokens_after) {
        let now = if after == before {
            "unchanged".to_string()
        } else {
            format!("→ {}k (the estimate) — resume acceptance unverified", after / 1000)
        };
        let _ = writeln!(
            h,
            "  resume reads the last request's recorded size (input + cache read + cache creation): {}k, {now}",
            before / 1000,
        );
    }
    if let Some(p) = &res.plan {
        let _ = writeln!(
            h,
            "  plan: {} kept · {} pointer(s) · {} précis · {} excerpt(s) · dropped {} result(s), {} record(s), \
             {} attachment(s) by kind · {} payload bytes removed",
            p.keep,
            p.pointer,
            p.precis,
            p.excerpt,
            p.results_dropped,
            p.records_dropped,
            p.attachments_dropped,
            human_bytes(p.bytes_saved),
        );
        // The one thing a selection can get wrong and still be a valid session.
        const SHOWN: usize = 8;
        for u in p.rests_on_nothing.iter().take(SHOWN) {
            let _ = writeln!(
                h,
                "  ⚠ reply {} rests on nothing kept: its turn's {} tool result(s) were all dropped or pointered",
                u.reply, u.results
            );
        }
        if p.rests_on_nothing.len() > SHOWN {
            let _ = writeln!(
                h,
                "  ⚠ … and {} more replies that rest on nothing kept (--json lists them)",
                p.rests_on_nothing.len() - SHOWN
            );
        }
    }
    skills_line(&mut h, &res.skills);
    for w in res.warnings.iter().chain(&res.cache_warning) {
        let _ = writeln!(h, "  ⚠ {w}");
    }
    if !dry_run {
        let _ = writeln!(h, "  new session: {}", res.new_path.display());
        if let Some(sc) = &res.sidecar_path {
            let _ = writeln!(
                h,
                "  sidecar:     {} (fetch an original: cv cat {} <tool_use_id>)",
                sc.display(),
                short_id(&res.new_id)
            );
        }
        if let Some(rc) = &res.copied_resources {
            let _ = writeln!(h, "  resources:   copied subagents/workflows → {}", rc.display());
        }
    }
    if let Some(mode) = carry_agents {
        let sources = std::slice::from_ref(&r.path);
        let dry = (dry_run, dry_run && !pinned_id);
        v["carried_agents"] = carry(mode, sources, &res.new_path, &res.new_id, dry, &mut h)?;
    }
    if !dry_run {
        let _ = writeln!(h, "  resume with: claude --resume {}", res.new_id);
    }
    report(json, &h, &v)
}

/// Harnesses rewound and pruned by IR selection and verified emission: every target with an
/// emitter that is not Claude (record derivation) or native Antigravity (cold rewind), and those
/// whose adapter writes a selection itself (Pi's native branch, an OpenSession document).
fn ir_reshapeable(h: Harness) -> bool {
    !cv_core::harness::has_derived_copies(h)
        && !cv_core::harness::has_cold_rewind(h)
        && (cv_core::harness::writes_selection(h) || cv_core::emit::supported_targets().contains(&h))
}

/// Where a selected session is written: `--out`, else the harness's own store.
fn ir_out_dir(h: Harness, out: Option<PathBuf>) -> Result<PathBuf> {
    match out {
        Some(dir) => Ok(dir),
        None => cv_core::harness::for_harness(h)
            .and_then(|a| a.root())
            .with_context(|| format!("{h} doesn't appear installed; pass --out <dir> to write somewhere")),
    }
}

/// Emit an IR selection of `source` under `new_id` to its own harness with read-back
/// verification. Returns the written path and the conversion losses the target's reader reported.
fn emit_ir(
    session: &Session,
    source: &std::path::Path,
    target: Harness,
    out: Option<PathBuf>,
    new_id: &str,
    sidecar: &[cv_core::distill::Elided],
) -> Result<(PathBuf, Option<PathBuf>, Vec<String>)> {
    cv_core::harness::validate_session_id(new_id)?;
    if let Some(adapter) = cv_core::harness::rootless(target).filter(|a| a.writes_selection()) {
        return adapter.write_selection(source, session, out.as_deref(), new_id, sidecar);
    }
    let out_dir = ir_out_dir(target, out)?;
    let opts = EmitOptions {
        new_id: Some(new_id.to_string()),
        thinking: cv_core::emit::ThinkingMode::Native,
        ..Default::default()
    };
    let (res, sc, lost) = cv_core::emit::emit_verified_with_sidecar(session, target, &out_dir, &opts, sidecar)?;
    Ok((res.path, sc, lost))
}

/// The `<new_id>.flat.jsonl` sidecar beside an IR-emitted session: where `cv cat` looks for
/// a pruned output when the transcript's own stem names no sidecar.
pub(crate) fn ir_sidecar_path(written: &std::path::Path, new_id: &str) -> PathBuf {
    written
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join(format!("{new_id}.flat.jsonl"))
}

struct ColdAntigravityPruneArgs {
    min_size: usize,
    keep_last: usize,
    drop: bool,
    thinking: bool,
    copy_resources: bool,
    window: Option<u64>,
    keep_range: Option<(usize, Option<usize>)>,
    keep_native_steps: bool,
    replace_summary: Option<String>,
    title: Option<String>,
    to: Option<String>,
    force: bool,
    out: Option<PathBuf>,
    dry_run: bool,
    json: bool,
}

fn prune_cold_antigravity(r: &cv_core::ir::SessionRef, args: ColdAntigravityPruneArgs) -> Result<()> {
    use cv_core::harness::antigravity::cold::prune::{prune, ColdPruneOptions, KeepSelector};
    let pinned = args.to.is_some();
    let keep = args.keep_range.map(|(s, e)| {
        if args.keep_native_steps {
            KeepSelector::Steps(s as u64, e.map(|x| x as u64))
        } else {
            KeepSelector::Messages(s, e)
        }
    });
    let opts = ColdPruneOptions {
        min_size: args.min_size,
        keep_last: args.keep_last,
        drop: args.drop,
        thinking: args.thinking,
        copy_resources: args.copy_resources,
        window: args.window,
        keep,
        replace_summary: args.replace_summary,
        title: args.title,
        new_id: args.to,
        out_dir: args.out,
        force: args.force,
        dry_run: args.dry_run,
    };
    let res = prune(r, &opts)?;
    if !args.dry_run {
        cv_core::lanes::record_lineage_edge_best_effort(
            &res.source_id,
            &res.new_id,
            "pruned_from",
            Some(r.harness.as_str()),
            None,
            serde_json::json!({
                "route": "native_sqlite",
                "pruned_count": res.pruned_count,
            }),
        );
    }
    let pct = if res.before_bytes > 0 {
        100.0 * res.bytes_saved() as f64 / res.before_bytes as f64
    } else {
        0.0
    };
    let mut h = String::new();
    let tag = if args.dry_run {
        " (dry run — nothing written)"
    } else {
        ""
    };
    let _ = writeln!(
        h,
        "✦ pruned {} → {}{tag}",
        short_id(&res.source_id),
        short_id(&res.new_id)
    );
    let _ = writeln!(
        h,
        "  {} native step(s) written ({} selected, {} dropped, {} compaction wall(s) removed) · {} payload(s) snipped · {} → {} (−{:.0}%) · ~{} token equivalents removed",
        res.steps_written,
        res.selected_steps.len(),
        res.dropped_steps,
        res.checkpoints_dropped,
        res.pruned_count,
        human_bytes(res.before_bytes),
        human_bytes(res.after_bytes),
        pct,
        res.tokens_freed,
    );
    if res.checkpoint_bytes_saved > 0 || res.replaced_summary {
        let summary_note = if res.replaced_summary {
            "replaced checkpoint summary + "
        } else {
            ""
        };
        let _ = writeln!(
            h,
            "  checkpoint:  {summary_note}stripped {} sludge section(s) (−{})",
            res.checkpoint_sections_stripped,
            human_bytes(res.checkpoint_bytes_saved)
        );
    }
    if res.thinking_steps_stripped > 0 {
        let _ = writeln!(
            h,
            "  thinking:    stripped {} planner step(s) (−{})",
            res.thinking_steps_stripped,
            human_bytes(res.thinking_bytes_saved)
        );
    }
    for w in &res.warnings {
        let _ = writeln!(h, "  ⚠ {w}");
    }
    let resume = crate::cmd::port::resume_lines(r.harness, &res.new_id, res.cwd.as_deref());
    if !args.dry_run {
        if let Some(p) = &res.new_path {
            let _ = writeln!(h, "  new session: {}", p.display());
        }
        if let Some(sc) = &res.sidecar_path {
            let _ = writeln!(
                h,
                "  sidecar:     {} (fetch an original: cv cat {} <tool_use_id>)",
                sc.display(),
                short_id(&res.new_id)
            );
        }
        for line in &resume {
            let _ = writeln!(h, "  resume with: {line}");
        }
    }
    let mut v = serde_json::to_value(&res)?;
    v["harness"] = r.harness.as_str().into();
    v["route"] = "native_sqlite".into();
    v["resume"] = serde_json::json!(resume);
    dry_run_nulls(&mut v, pinned, &["new_path", "sidecar_path"], &["new_id", "resume"]);
    report(args.json, &h, &v)
}

struct IrPruneArgs {
    min_size: usize,
    keep_last: usize,
    drop: bool,
    to: Option<String>,
    out: Option<PathBuf>,
    dry_run: bool,
    json: bool,
}

fn prune_ir(r: &cv_core::ir::SessionRef, args: IrPruneArgs) -> Result<()> {
    let IrPruneArgs {
        min_size,
        keep_last,
        drop,
        to,
        out,
        dry_run,
        json,
    } = args;
    if !ir_reshapeable(r.harness) {
        return usage(format!("cv prune has no verified emitter for {} sessions", r.harness));
    }
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let (session, total, grown) = cv_core::emit::prepare_replayable(adapter.as_ref(), r, 0, None)?;
    if total == 0 {
        return usage(format!(
            "no messages read from {} as a {} session",
            r.path.display(),
            r.harness
        ));
    }
    let pinned = to.is_some();
    let new_id = to.unwrap_or_else(|| cv_core::emit::fresh_session_id(r.harness));
    if let Err(e) = cv_core::harness::validate_session_id(&new_id) {
        return usage(e.to_string());
    }
    let opts = cv_core::prune::PruneOptions {
        min_size,
        keep_last,
        drop,
        ..Default::default()
    };
    let pruned = cv_core::prune::prune_ir(session, &opts, &new_id)?;
    let mut h = String::new();
    let tag = if dry_run { " (dry run — nothing written)" } else { "" };
    let _ = writeln!(h, "✦ pruned {} → {}{tag}", short_id(&r.id), short_id(&new_id));
    let _ = writeln!(
        h,
        "  {} tool output(s) replaced by markers · −{} · ~{} token equivalents removed (byte estimate) · by IR selection",
        pruned.pruned_count,
        human_bytes(pruned.bytes_saved),
        pruned.tokens_saved
    );
    if let Some(grown) = grown {
        let _ = writeln!(h, "  read {grown}");
    }
    let mut v = serde_json::json!({
        "harness": r.harness.as_str(),
        "route": "ir",
        "source_id": r.id,
        "new_id": new_id,
        "snipped_payloads": pruned.pruned_count,
        "bytes_saved": pruned.bytes_saved,
        "tokens_freed": pruned.tokens_saved,
        "dry_run": dry_run,
        "new_path": null,
        "sidecar_path": null,
        "lost": [],
    });
    if !dry_run {
        let (path, sidecar, lost) = emit_ir(&pruned.session, &r.path, r.harness, out, &new_id, &pruned.stashed)?;
        cv_core::lanes::record_lineage_edge_best_effort(
            cv_core::lineage::bare_session_id(&r.id),
            &new_id,
            "pruned_from",
            Some(r.harness.as_str()),
            None,
            serde_json::json!({
                "route": "ir",
                "pruned_count": pruned.pruned_count,
            }),
        );
        let _ = writeln!(h, "  new session: {}", path.display());
        if let Some(sc) = &sidecar {
            let _ = writeln!(
                h,
                "  sidecar:     {} (fetch an original: cv cat {} <tool_use_id>)",
                sc.display(),
                short_id(&new_id)
            );
        }
        for line in &lost {
            let _ = writeln!(h, "  ⚠ lost {line}");
        }
        let resume = crate::cmd::port::resume_lines(r.harness, &new_id, pruned.session.cwd.as_deref());
        for line in &resume {
            let _ = writeln!(h, "  resume with: {line}");
        }
        v["new_path"] = path.display().to_string().into();
        v["sidecar_path"] = sidecar.map(|p| p.display().to_string()).into();
        v["lost"] = serde_json::json!(lost);
        v["resume"] = serde_json::json!(resume);
    } else if !pinned {
        v["new_id"] = serde_json::Value::Null;
    }
    report(json, &h, &v)
}

/// `cv rewind` of a harness rewound by IR selection: the prefix through the cut, emitted under
/// a fresh id to the source's harness (or `--out`), verified by reading it back.
fn rewind_ir(
    r: &cv_core::ir::SessionRef,
    cut: &cv_core::rewind::CutAt,
    out: Option<PathBuf>,
    to: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    // One read: the replayable window `[0..end)` is the whole read truncated.
    let (mut session, total, grown) = cv_core::emit::prepare_replayable(adapter.as_ref(), r, 0, None)?;
    if total == 0 {
        return usage(format!(
            "no messages read from {} as a {} session",
            r.path.display(),
            r.harness
        ));
    }
    let (end, evidence) =
        cv_core::rewind::ir_cut_end(r, total, cut).map_err(|e| crate::util::UsageError(format!("{e:#}")))?;
    session.messages.truncate(end);
    let source_id = cv_core::lineage::bare_session_id(&r.id).to_owned();
    session.lineage.forked_from = Some(source_id.clone());
    cv_core::lineage::stamp_derived(&mut session, &source_id, "rewind");
    let pinned = to.is_some();
    let new_id = to.unwrap_or_else(|| cv_core::emit::fresh_session_id(r.harness));
    if let Err(e) = cv_core::harness::validate_session_id(&new_id) {
        return usage(e.to_string());
    }
    let mut h = String::new();
    let tag = if dry_run { " (dry run — nothing written)" } else { "" };
    let _ = writeln!(h, "✦ rewound {} → {}{tag}", short_id(&r.id), short_id(&new_id));
    let why = match &evidence {
        Some(e) => format!(" · {} — `{}`", e.kind.label(), cv_core::ir::truncate(&e.command, 80)),
        None => String::new(),
    };
    let _ = writeln!(
        h,
        "  cut:     msg {} of {total}{why} · {} message(s) kept · by IR selection",
        end.saturating_sub(1),
        session.messages.len()
    );
    if let Some(grown) = grown {
        let _ = writeln!(h, "  read {grown}");
    }
    let mut v = serde_json::json!({
        "harness": r.harness.as_str(),
        "route": "ir",
        "source_id": r.id,
        "source_path": r.path,
        "new_id": new_id,
        "cut_msg_idx": end.saturating_sub(1),
        "messages": session.messages.len(),
        "evidence": evidence,
        "dry_run": dry_run,
        "new_path": null,
        "lost": [],
    });
    if !dry_run {
        let (path, _, lost) = emit_ir(&session, &r.path, r.harness, out, &new_id, &[])?;
        cv_core::lanes::record_lineage_edge_best_effort(
            &source_id,
            &new_id,
            "rewound_from",
            Some(r.harness.as_str()),
            None,
            serde_json::json!({
                "route": "ir",
                "cut_msg_idx": end.saturating_sub(1),
            }),
        );
        let _ = writeln!(h, "  new session: {}", path.display());
        for line in &lost {
            let _ = writeln!(h, "  ⚠ lost {line}");
        }
        let resume = crate::cmd::port::resume_lines(r.harness, &new_id, session.cwd.as_deref());
        for line in &resume {
            let _ = writeln!(h, "  resume with: {line}");
        }
        v["new_path"] = path.display().to_string().into();
        v["lost"] = serde_json::json!(lost);
        v["resume"] = serde_json::json!(resume);
    } else if !pinned {
        v["new_id"] = serde_json::Value::Null;
    }
    report(json, &h, &v)
}

/// Report only an explicitly selected policy; keep preserves existing output.
pub(crate) fn skills_line(h: &mut String, s: &cv_core::harness::claude::skills::SkillsReport) {
    if !s.is_keep() {
        let _ = writeln!(
            h,
            "  skills ({}): dropped {} restored skill(s), {} chars, kept {} · {} record(s) rewritten",
            s.policy, s.dropped, s.dropped_chars, s.kept, s.records
        );
    }
}

// ---------- rewind ----------

/// The `--at` grammar: an all-digit value is a message index (the `cv show --range` counting), any
/// other 7–64 hex digits a commit sha, nothing at all the last message. (A sha whose digits happen to
/// be all decimal reads as an index — pass more of it; a full sha never is.)
pub(crate) fn parse_cut(at: Option<&str>) -> Result<cv_core::rewind::CutAt> {
    use cv_core::rewind::{normalize_sha, CutAt};
    let Some(at) = at.map(str::trim) else {
        return Ok(CutAt::End);
    };
    if !at.is_empty() && at.bytes().all(|b| b.is_ascii_digit()) {
        return match at.parse() {
            Ok(n) => Ok(CutAt::Message(n)),
            Err(_) => usage(format!("--at {at:?}: message index out of range")),
        };
    }
    match normalize_sha(at) {
        Some(sha) => Ok(CutAt::Commit(sha)),
        None => usage(format!(
            "--at {at:?}: expected a message index (as `cv show --range` counts) or a commit sha (7–64 hex digits)"
        )),
    }
}

/// `cv rewind <id>` — derive the agent as of a past message or commit (see [`cv_core::rewind`]).
/// `<id>` is any session id, an `agent-…` sub-agent id, or a transcript path; `--agent` picks a
/// sub-agent under `<id>` the way `cv show --agent` does.
pub(crate) struct RewindArgs<'a> {
    pub recovery: crate::RecoveryArgs,
    pub id: &'a str,
    pub harness: Option<String>,
    pub agent: Option<String>,
    pub at: Option<&'a str>,
    pub out: Option<PathBuf>,
    pub to: Option<String>,
    pub full: bool,
    pub thinking: Option<cv_core::emit::ThinkingMode>,
    pub skills_policy: SkillsPolicy,
    pub carry_agents: Option<CarryAgents>,
    pub dry_run: bool,
    pub json: bool,
    pub generator: &'a str,
}

pub(crate) fn cmd_rewind(args: RewindArgs<'_>) -> Result<()> {
    let RewindArgs {
        recovery,
        id,
        harness,
        agent,
        at,
        out,
        to,
        full,
        thinking,
        skills_policy,
        carry_agents,
        dry_run,
        json,
        generator,
    } = args;
    let cut = parse_cut(at)?;
    if let Some(id) = to.as_deref() {
        if let Err(e) = cv_core::harness::validate_session_id(id) {
            return usage(e.to_string());
        }
    }
    let (r, _adapter) = crate::cmd::distill::resolve_source(id, parse_harness(&harness)?)?;
    let r = match &agent {
        Some(a) => crate::cmd::view::resolve_subagent(&r, a)?.session,
        None => r,
    };
    carry_source(carry_agents, &r)?;
    if recovery.pre_compaction.is_some() && !recovery.uncompact {
        return usage("--pre-compaction needs --uncompact on rewind");
    }
    if recovery.uncompact && (at.is_some() || full) {
        return usage("--uncompact selects the rewind cut and conflicts with --at and --full");
    }
    if ir_reshapeable(r.harness) {
        if carry_agents.is_some() || skills_policy != SkillsPolicy::Keep || recovery.uncompact {
            return usage(format!(
                "--carry-agents, --skills and --uncompact rewind Claude Code records; {} sessions rewind by selection",
                r.harness
            ));
        }
        return rewind_ir(&r, &cut, out, to, dry_run, json);
    }
    if !cv_core::harness::has_derived_copies(r.harness) && !cv_core::harness::has_cold_rewind(r.harness) {
        return usage(format!(
            "cv rewind has no verified emitter for {} sessions — `cv splice {}:..N` composes a prefix of any harness",
            r.harness,
            short_id(&r.id)
        ));
    }

    let pinned = to.is_some();
    let opts = cv_core::rewind::RewindOptions {
        at: cut,
        skills_policy,
        full,
        thinking,
        out_dir: out,
        new_id: to,
        dry_run,
        generator: generator.to_string(),
    };
    if cv_core::harness::has_cold_rewind(r.harness) {
        if carry_agents.is_some()
            || skills_policy != SkillsPolicy::Keep
            || thinking.is_some_and(|t| t != cv_core::emit::ThinkingMode::Native)
        {
            return usage("cold Antigravity rewind uses native thinking and carries its own brain closure");
        }
        let result =
            cv_core::harness::antigravity::cold::rewind(&r, &opts, recovery.uncompact, recovery.pre_compaction)?;
        if !dry_run {
            cv_core::lanes::record_lineage_edge_best_effort(
                &result.source_id,
                &result.new_id,
                "rewound_from",
                Some(r.harness.as_str()),
                None,
                serde_json::json!({
                    "route": "native_sqlite",
                    "selected_steps": result.selected_steps.len(),
                }),
            );
        }
        let mut h = format!(
            "  {} native steps · {} generations retained · {} omitted · {} executor snapshots retained ({} omitted)\n",
            result.selected_steps.len(),
            result.generations_kept,
            result.generations_omitted,
            result.executor_rows_kept,
            result.executor_rows_omitted
        );
        if !result.uncaptured.is_empty() {
            let _ = writeln!(
                h,
                "  brain closure: {} captured · {} item(s) omitted ({})",
                human_bytes(result.closure_bytes),
                result.uncaptured.len(),
                result
                    .uncaptured
                    .iter()
                    .take(4)
                    .map(|u| format!("{}: {}", u.path, u.why))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        for w in &result.warnings {
            let _ = writeln!(h, "  ⚠ {w}");
        }
        if let Some(plan) = &result.recovery {
            h.push_str(&plan.report());
        }
        if let Some(path) = &result.new_path {
            let _ = writeln!(
                h,
                "  {} {}",
                if dry_run { "would write" } else { "wrote" },
                path.display()
            );
        }
        let mut v = serde_json::to_value(&result)?;
        dry_run_nulls(&mut v, pinned, &["new_path"], &["new_id"]);
        return report(json, &h, &v);
    }
    let res = if recovery.uncompact {
        cv_core::rewind::rewind_uncompact(&r, &opts, recovery.pre_compaction)?
    } else {
        cv_core::rewind::rewind_session(&r, &opts)?
    };
    let cwd = res.cwd.as_deref().map(std::path::Path::new);
    let resume = crate::cmd::port::resume_lines(r.harness, &res.new_id, cwd);
    // Claude Code finds a session by its project dir; a rewind written anywhere else must be moved
    // there first. (`--out` defaults to it, so this only fires for an explicit elsewhere.)
    let home = cv_core::rewind::project_dir(&r.path);
    let relocate = res.new_path.parent().is_some_and(|d| d != home);

    let mut v = serde_json::to_value(&res)?;
    v["harness"] = r.harness.as_str().into();
    v["resume"] = serde_json::json!(resume);
    dry_run_nulls(&mut v, pinned, &["new_path", "provenance_path"], &["new_id", "resume"]);

    let mut h = String::new();
    if let Some(plan) = &res.recovery {
        h.push_str(&plan.report());
    }
    let tag = if res.dry_run {
        " (dry run — nothing written)"
    } else {
        ""
    };
    let _ = writeln!(
        h,
        "✦ rewound {} → {}{tag}",
        short_id(&res.source_id),
        short_id(&res.new_id)
    );
    let of = match (&res.subagent, &res.source_session_id) {
        (true, Some(p)) => format!(" · sub-agent of {} → emitted as a top-level session", short_id(p)),
        _ => String::new(),
    };
    let _ = writeln!(
        h,
        "  source:  {} ({}, sha256 {}…){of}",
        res.source_path.display(),
        human_bytes(res.source_bytes),
        &res.source_sha256[..12],
    );
    let why = match &res.evidence {
        Some(e) => format!(" · {} — `{}`", e.kind.label(), cv_core::ir::truncate(&e.command, 80)),
        None => String::new(),
    };
    let _ = writeln!(h, "  cut:     msg {} · line {}{why}", res.cut_msg_idx, res.cut_line);
    let from = match (res.start_mode, res.boundary_msg_idx, res.preserved_head) {
        ("full", ..) => "the first record (--full)".to_string(),
        (_, None, _) => "the first record (no compaction before the cut)".to_string(),
        (_, Some(b), true) => format!("the compaction at msg {b}, from its preserved segment"),
        (_, Some(b), false) => format!("the compaction at msg {b}"),
    };
    let _ = writeln!(
        h,
        "  start:   msg {} · line {} · {from}",
        res.start_msg_idx, res.start_line
    );
    let closed = if res.closed_tool_calls > 0 {
        format!(
            " (+{} tool result(s) past the cut, closing its open calls)",
            res.closed_tool_calls
        )
    } else {
        String::new()
    };
    // An `End` rewind copies the whole source, so there is usually nothing to say about a tail.
    let omitted = match res.omitted_lines {
        0 => String::new(),
        n => format!(" · omitted the {n} line(s) after it"),
    };
    let _ = writeln!(
        h,
        "  copied:  lines {}..={} — {} line(s), {}{closed}{omitted}",
        res.start_line,
        res.end_line,
        res.lines_written,
        human_bytes(res.bytes_written),
    );
    let _ = writeln!(h, "  thinking: {}", res.thinking);
    skills_line(&mut h, &res.skills);
    for w in &res.warnings {
        let _ = writeln!(h, "  ⚠ {w}");
    }
    if res.cut_cwd.is_some() && res.cut_cwd != res.cwd {
        let _ = writeln!(
            h,
            "  at the cut the agent was working in {}",
            res.cut_cwd.as_deref().unwrap_or_default()
        );
    }
    if let Some(mode) = carry_agents {
        let sources = std::slice::from_ref(&r.path);
        let dry = (res.dry_run, res.dry_run && !pinned);
        v["carried_agents"] = carry(mode, sources, &res.new_path, &res.new_id, dry, &mut h)?;
    }
    if !res.dry_run {
        let _ = writeln!(h, "  new session: {}", res.new_path.display());
        let _ = writeln!(h, "  provenance:  {}", res.provenance_path.display());
        if relocate {
            let _ = writeln!(h, "  ⚠ claude resumes from {} — copy it there first", home.display());
        }
        let _ = writeln!(h, "  resume with (add --fork-session to keep the rewind reusable):");
        for line in &resume {
            let _ = writeln!(h, "    {line}");
        }
    }
    report(json, &h, &v)
}

// ---------- splice ----------

/// A parsed splice spec: which session, and a `[start, end)` window (end None → through the last).
struct SpliceSpec {
    id: String,
    start: usize,
    end: Option<usize>,
}

/// Parse `<id>:A..B` | `<id>:A..` | `<id>:..B` | `<id>` into a [`SpliceSpec`] — the same `A..B`
/// window grammar as `show --range` (0-based, end-exclusive). `<id>` may carry a `harness:` prefix
/// (`codex:019e…:0..12`), so the range is split off the END.
fn parse_splice_spec(spec: &str) -> Result<SpliceSpec> {
    match spec.rsplit_once(':') {
        Some((id, range)) if range.contains("..") => {
            let (start, end) =
                parse_range(range).map_err(|error| crate::util::UsageError(format!("bad spec {spec:?}: {error}")))?;
            Ok(SpliceSpec {
                id: id.to_string(),
                start,
                end,
            })
        }
        Some((_, range))
            if !range.is_empty() && range.contains('-') && range.chars().all(|c| c.is_ascii_digit() || c == '-') =>
        {
            usage(format!(
                "bad spec {spec:?}: the `<id>:A-B` grammar is gone — use `<id>:A..B` (0-based, end-exclusive), \
                 `<id>:A..`, or `<id>:..B`"
            ))
        }
        _ => Ok(SpliceSpec {
            id: spec.to_string(),
            start: 0,
            end: None,
        }),
    }
}

/// Where a splice goes. `resumable` is `--session` / `--agent-of` (a Claude transcript the harness
/// resumes, shared with `distill`); otherwise `--harness`/`--out` emit to a harness and nothing at
/// all prints a summary (with `--export md|json`, the composed session).
pub(crate) struct SpliceArgs<'a> {
    pub recovery: crate::RecoveryArgs,
    pub specs: &'a [String],
    pub harness: Option<String>,
    pub out: Option<PathBuf>,
    pub export: Option<String>,
    pub cwd: Option<PathBuf>,
    pub thinking: cv_core::emit::ThinkingMode,
    pub resumable: Option<crate::cmd::distill::Target>,
    pub carry_agents: Option<CarryAgents>,
    pub json: bool,
}

pub(crate) fn cmd_splice(args: SpliceArgs<'_>) -> Result<()> {
    let SpliceArgs {
        recovery,
        specs,
        harness,
        out,
        export,
        cwd,
        thinking,
        resumable,
        carry_agents,
        json,
    } = args;
    if let Some(format) = export.as_deref() {
        if !matches!(format, "json" | "md" | "markdown") {
            return usage(format!("unknown export format {format:?} (use md or json)"));
        }
    }
    if recovery.pre_compaction.is_some() && !recovery.uncompact {
        return usage("--pre-compaction needs --uncompact on splice");
    }
    let mut plans = Vec::new();
    let parsed: Vec<SpliceSpec> = specs.iter().map(|s| parse_splice_spec(s)).collect::<Result<_>>()?;
    let explicit_h = match &harness {
        Some(s) => {
            Some(Harness::parse(s).ok_or_else(|| crate::util::UsageError(format!("unknown target harness: {s}")))?)
        }
        None => None,
    };
    let emitting = resumable.is_some() || explicit_h.is_some() || out.is_some();

    let mut composition = cv_core::splice::Composition::default();
    let mut first_path = None;
    let mut agent_sources: Vec<PathBuf> = Vec::new();
    let mut stale_reads = Vec::new();
    for sp in &parsed {
        let (reference, adapter) = crate::cmd::distill::resolve_source(&sp.id, None)?;
        first_path.get_or_insert_with(|| reference.path.clone());
        if cv_core::harness::has_derived_copies(reference.harness)
            && !cv_core::rewind::is_subagent(&reference)
            && !agent_sources.contains(&reference.path)
        {
            agent_sources.push(reference.path.clone());
        }
        if recovery.uncompact && (sp.start != 0 || sp.end.is_some()) {
            return usage("--uncompact and a splice range name different cuts");
        }
        let (mut session, total, grown) = if emitting {
            cv_core::emit::prepare_replayable(adapter.as_ref(), &reference, sp.start, sp.end)?
        } else {
            cv_core::emit::prepare_selected(adapter.as_ref(), &reference, sp.start, sp.end)?
        };
        if let Some(grown) = grown {
            eprintln!("  ↳ {}: read {grown}", short_id(&reference.id));
        }
        if recovery.uncompact {
            let p = cv_core::uncompact::Plan::select(&reference, &session, recovery.pre_compaction)?;
            if emitting {
                p.ensure_closed()?
            }
            eprint!("{}", p.report());
            p.apply(&mut session);
            plans.push(p);
        }
        stale_reads.extend(cv_core::lanes::session_stale_reads(&session, &reference)?);
        composition.push(session, total, sp.start, sp.end);
    }
    let first_h = composition
        .first_harness()
        .context("splice needs at least one session")?;
    let to_h = explicit_h.unwrap_or(first_h);
    let emit_target = if resumable.is_some() {
        Some(cv_core::harness::derived_host())
    } else if explicit_h.is_some() || out.is_some() {
        Some(to_h)
    } else {
        None
    };
    let mut spliced = match emit_target {
        Some(target) => composition.finish_for_emit(None, target),
        None => composition.finish(None, to_h),
    };
    if !plans.is_empty() {
        spliced
            .cv_extra_mut()
            .insert("recovery".into(), serde_json::json!(plans));
    }
    if !stale_reads.is_empty() {
        let values = serde_json::to_value(&stale_reads)?;
        eprintln!("stale_reads: {values}");
        spliced.cv_extra_mut().insert("stale_reads".into(), values);
    }
    if let Some(target) = resumable {
        // `--session` / `--agent-of`: a resumable Claude transcript, the way `distill` writes one.
        // Nothing is elided (every selected message is carried whole), so there is no sidecar.
        let label = format!("splice: {}", specs.join(" "));
        let src = first_path.context("splice needs at least one session")?;
        let e = crate::cmd::distill::emit_to(&spliced, &target, &[], &src, &label)?;
        if let Some(dst_id) = e.session_id.as_deref().or(e.agent_id.as_deref()) {
            record_splice_lineage(&spliced, dst_id, cv_core::harness::derived_host());
        }
        let mut h = String::new();
        let _ = writeln!(
            h,
            "✦ spliced {} ({} messages from {} span(s))",
            short_id(&spliced.id),
            spliced.messages.len(),
            specs.len()
        );
        if let Some(p) = &e.path {
            let _ = writeln!(h, "  ↳ wrote {}", p.display());
        }
        let mut v = serde_json::to_value(&e)?;
        if !plans.is_empty() {
            v["recovery"] = serde_json::json!(plans);
        }
        v["stale_reads"] = serde_json::to_value(&stale_reads)?;
        if let (Some(mode), Some(path), Some(id), None) = (carry_agents, &e.path, &e.session_id, &e.agent_id) {
            v["carried_agents"] = carry(mode, &agent_sources, path, id, (false, false), &mut h)?;
        }
        if let Some(r) = &e.resume {
            let _ = writeln!(h, "  ↳ resume: {r}");
        }
        return report(json, &h, &v);
    }
    if json && recovery.uncompact && !emitting && export.is_none() {
        return report(
            true,
            "",
            &serde_json::json!({
                "session_id": spliced.id, "message_count": spliced.messages.len(), "recovery": plans
            }),
        );
    }
    finish_composed(ComposedArgs {
        session: spliced,
        to_h,
        explicit_to: harness.is_some(),
        out,
        export,
        cwd,
        thinking,
    })
}

fn record_splice_lineage(spliced: &Session, dst_id: &str, to_h: Harness) {
    if cv_core::env::frozen() || dst_id.trim().is_empty() {
        return;
    }
    let Ok(store) = cv_core::state::StateStore::default_store() else {
        return;
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(arr) = spliced
        .cv_extra()
        .and_then(|b| b.get("loom"))
        .and_then(|v| v.as_array())
    {
        for span in arr {
            let Some(src_id) = span.get("source_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                continue;
            };
            if src_id == dst_id || !seen.insert(src_id.to_string()) {
                continue;
            }
            let _ = store.record_lineage_edge(&cv_core::state::LineageEdge {
                src_session_id: src_id.to_string(),
                dst_session_id: dst_id.to_string(),
                edge_kind: "spliced_from".into(),
                harness: Some(to_h.as_str().to_string()),
                tool_use_id: None,
                metadata: span.clone(),
                created_at: now_ms,
            });
        }
    }
}

/// Shared tail for splice: emit to a harness when `--harness`/`--out` is in play, otherwise
/// print a summary and (with `--export md|json`) the composed session to stdout.
pub(crate) struct ComposedArgs {
    pub session: Session,
    pub to_h: Harness,
    pub explicit_to: bool,
    pub out: Option<PathBuf>,
    pub export: Option<String>,
    pub cwd: Option<PathBuf>,
    pub thinking: cv_core::emit::ThinkingMode,
}

fn finish_composed(args: ComposedArgs) -> Result<()> {
    let ComposedArgs {
        mut session,
        to_h,
        explicit_to,
        out,
        export,
        cwd,
        thinking,
    } = args;

    // Emit path: an explicit --harness or an --out directory means "materialize this for a harness".
    if explicit_to || out.is_some() {
        if let Some(dir) = &cwd {
            session.cwd = Some(dir.clone());
        }
        let dst_id = session.id.clone();
        emit_session(
            &session,
            to_h,
            out,
            EmitOptions {
                new_cwd: cwd,
                new_id: Some(dst_id.clone()),
                thinking,
                ..Default::default()
            },
        )?;
        record_splice_lineage(&session, &dst_id, to_h);
        return Ok(());
    }

    // Complete checked demands before publishing either the report or the exported document.
    let staged = match export.as_deref() {
        Some("json" | "md" | "markdown") => {
            use std::io::Write;
            let mut stage = std::io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::new());
            let resolver = session.resolver();
            if export.as_deref() == Some("json") {
                session.write_json_pretty(&mut stage, &resolver)?;
                stage.write_all(b"\n")?;
            } else {
                cv_core::render::write_markdown(&session, &mut stage, &resolver)?;
            }
            Some(stage.into_inner().map_err(|e| e.into_error())?.finish()?)
        }
        _ => None,
    };
    // Otherwise: print a summary (to stderr when `--export` owns stdout), and with `--export`
    // dump the pure composed document to stdout so `cv splice ... --export json` parses cleanly.
    let header = format!(
        "✦ composed {} ({}) · {} msg",
        short_id(&session.id),
        session.harness.as_str(),
        session.messages.len()
    );
    if staged.is_some() {
        eprintln!("{header}");
    } else {
        println!("{header}");
    }
    if let Some(prov) = session.cv_extra().and_then(|b| b.get("loom")) {
        if let Some(arr) = prov.as_array() {
            for p in arr {
                let line = format!(
                    "  ↳ {} {}[{}..{}]",
                    p.get("harness").and_then(|v| v.as_str()).unwrap_or("?"),
                    p.get("source_id")
                        .and_then(|v| v.as_str())
                        .map(short_id)
                        .unwrap_or_default(),
                    p.get("start").and_then(|v| v.as_u64()).unwrap_or(0),
                    p.get("end").and_then(|v| v.as_u64()).unwrap_or(0),
                );
                if staged.is_some() {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
        }
    }
    if let Some(staged) = staged {
        staged.copy_to(&mut std::io::stdout().lock())?;
    } else {
        println!("\n(use --export md|json to print it, or --harness <harness> [--out <dir>] to emit it)");
    }
    Ok(())
}

// ---------- dataset ----------

pub(crate) struct DatasetArgs<'a> {
    pub format: &'a str,
    pub harness: Option<String>,
    pub query: Option<String>,
    pub subagents: bool,
    pub limit: Option<usize>,
    pub min_messages: usize,
    pub redact: bool,
    pub redact_only: Option<String>,
    pub active_branch: bool,
    pub clean_outcome: bool,
    pub file: Option<PathBuf>,
    pub force: bool,
}

pub(crate) fn cmd_dataset(args: DatasetArgs<'_>) -> Result<()> {
    let DatasetArgs {
        format,
        harness,
        query,
        subagents,
        limit,
        min_messages,
        redact,
        redact_only,
        active_branch,
        clean_outcome,
        file,
        force,
    } = args;
    use std::io::Write;
    let out = file;
    let fmt = match cv_core::dataset::Format::parse(format) {
        Some(f) => f,
        None => {
            return usage(format!(
                "unknown format {format:?} (use chatml, sharegpt, openai-tools, or cycle)"
            ))
        }
    };
    if fmt == cv_core::dataset::Format::Cycle && clean_outcome {
        return usage("--clean-outcome cannot be combined with --format cycle");
    }
    let dataset_opts = cv_core::dataset::DatasetOptions {
        active_branch_only: active_branch,
        clean_outcome_only: clean_outcome,
        include_thinking: true,
    };
    if let Some(path) = &out {
        crate::util::user_file_claim(path, force)?;
    }
    let want = parse_harness(&harness)?;
    let query = match query {
        Some(q) => Some(cv_core::SessionQuery::parse(&q).map_err(crate::util::UsageError)?),
        None => None,
    };
    // `--redact-only <classes>` scopes redaction to specific secret classes (and implies it);
    // plain `--redact` scrubs every class.
    let redact_opts = match &redact_only {
        Some(spec) => Some(cv_core::redact::RedactOptions {
            classes: cv_core::redact::RedactClasses::parse_only(spec).map_err(crate::util::UsageError)?,
        }),
        None => redact.then(cv_core::redact::RedactOptions::default),
    };
    // Pre-resolve any `text:` predicates against the full-text index (one search per needle).
    let text_sets = query
        .as_ref()
        .map(crate::cmd::query::text_sets)
        .transpose()?
        .unwrap_or_else(crate::cmd::query::TextSets::empty);
    // File exports stage the whole operation before opening the destination. In particular,
    // --file must never truncate a transcript before discovery or source validation.
    let sources = cv_core::discover_all();
    let mut extra_sources = Vec::new();
    let mut stage = cv_core::bytes::ByteStage::new();
    let mut writer: Box<dyn Write + '_> = match &out {
        Some(_) => Box::new(std::io::BufWriter::new(&mut stage)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    // Each lazy pass stages a record until EOF and filters succeed, retaining no message history.
    let mut emitted = 0usize;
    let mut skipped = 0usize;
    let mut ctx = EmitCtx {
        writer: writer.as_mut(),
        query: query.as_ref(),
        text_sets: &text_sets,
        fmt,
        redact_opts: redact_opts.as_ref(),
        dataset_opts: &dataset_opts,
        min_messages,
        limit,
        emitted: &mut emitted,
        skipped: &mut skipped,
    };

    'outer: for r in &sources {
        if let Some(w) = want {
            if r.harness != w {
                continue;
            }
        }
        if ctx.emit_ref(r)? == DatasetFlow::LimitHit {
            break;
        }
        // `--subagents`: a session fans out into a forest of sub-agent transcripts (Claude's
        // direct and `Workflow`-spawned agents, Codex's children, Kimi's and Hermes's delegates)
        // that `discover_all` never lists. For a distillation corpus those agent transcripts are
        // first-class training data — and a parent on one model often spawns sub-agents on
        // another, so a `model:` query that misses them misses most of the matching turns. Walk
        // the adapter's forest and emit each match too.
        if subagents {
            for info in cv_core::subagent_tree_of(r)? {
                if out.is_some() {
                    extra_sources.push(info.session.path.clone());
                }
                if ctx.emit_ref(&info.session)? == DatasetFlow::LimitHit {
                    break 'outer;
                }
            }
        }
    }
    writer.flush()?;
    drop(writer);
    if let Some(path) = &out {
        publish_dataset(
            stage,
            path,
            force,
            sources
                .iter()
                .map(|r| r.path.as_path())
                .chain(extra_sources.iter().map(|p| p.as_path())),
        )?;
    }
    let dest = out.as_ref().map(|p| format!(" to {}", p.display())).unwrap_or_default();
    let forest = if subagents { " (incl. sub-agent forest)" } else { "" };
    eprintln!("✦ {format}: wrote {emitted} record(s){dest}{forest} ({skipped} skipped)");
    Ok(())
}

/// Publish the dataset whole at `--file` (refused if taken unless `--force`). Naming one of its
/// own source transcripts is refused even with `--force`: the rename would replace it.
fn publish_dataset<'a>(
    stage: cv_core::bytes::ByteStage,
    path: &std::path::Path,
    force: bool,
    inputs: impl Iterator<Item = &'a std::path::Path>,
) -> Result<()> {
    if let Ok(target) = fs::metadata(path) {
        for input in inputs {
            let Ok(source) = fs::metadata(input) else { continue };
            #[cfg(unix)]
            let aliases = {
                use std::os::unix::fs::MetadataExt;
                source.dev() == target.dev() && source.ino() == target.ino()
            };
            #[cfg(not(unix))]
            let aliases = {
                let _ = (&source, &target);
                fs::canonicalize(input).ok() == fs::canonicalize(path).ok()
            };
            anyhow::ensure!(
                !aliases,
                "dataset output aliases source transcript: {}",
                input.display()
            );
        }
    }
    let claim = crate::util::user_file_claim(path, force)?;
    let bytes = stage.finish()?;
    cv_core::publish::publish_reader(path, "out", bytes.reader()?, claim)?;
    Ok(())
}

/// Whether the emit loop should keep going or stop because `--limit` was reached.
#[derive(PartialEq, Eq)]
enum DatasetFlow {
    Continue,
    LimitHit,
}

/// The shared state for emitting one session ref as a dataset record — so the top-level corpus walk
/// and the per-session sub-agent forest walk run identical parse → filter → write logic.
struct EmitCtx<'a> {
    writer: &'a mut dyn std::io::Write,
    query: Option<&'a cv_core::SessionQuery>,
    text_sets: &'a crate::cmd::query::TextSets,
    fmt: cv_core::dataset::Format,
    redact_opts: Option<&'a cv_core::redact::RedactOptions>,
    dataset_opts: &'a cv_core::dataset::DatasetOptions,
    min_messages: usize,
    limit: Option<usize>,
    emitted: &'a mut usize,
    skipped: &'a mut usize,
}

impl EmitCtx<'_> {
    /// Parse one ref and stream it as a record if it passes the filters. Returns [`DatasetFlow::LimitHit`]
    /// once `--limit` records have been emitted.
    fn emit_ref(&mut self, r: &cv_core::ir::SessionRef) -> Result<DatasetFlow> {
        if self.limit.is_some_and(|limit| *self.emitted >= limit) {
            return Ok(DatasetFlow::LimitHit);
        }

        // Catalog-cheap prefilter (everything but `model:`) prunes before we pay to parse.
        if let Some(q) = self.query {
            if !q.prefilter(r) {
                return Ok(DatasetFlow::Continue);
            }
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            return Ok(DatasetFlow::Continue);
        };
        let staged = if self.fmt == cv_core::dataset::Format::Cycle {
            let remaining = self.limit.map(|l| l.saturating_sub(*self.emitted));
            cv_core::dataset::cycles::stage_cycles_bounded(
                adapter.as_ref(),
                r,
                self.redact_opts,
                self.min_messages,
                self.query,
                self.text_sets,
                self.dataset_opts,
                remaining,
            )
        } else {
            cv_core::dataset::stage_record_with_options(
                adapter.as_ref(),
                r,
                self.fmt,
                self.redact_opts,
                self.min_messages,
                self.query,
                self.text_sets,
                self.dataset_opts,
            )
            .map(|record| record.map(|record| (record, 1)))
        };
        let record = match staged {
            Ok(record) => record,
            Err(error) if error.is::<cv_core::dataset::InputError>() => {
                eprintln!("cv dataset: skipping {} ({}): {error:#}", r.id, r.harness);
                return Ok(DatasetFlow::Continue);
            }
            Err(error) => return Err(error),
        };
        if let Some((record, records)) = record {
            record.publish(self.writer)?;
            *self.emitted += records;
            if self.limit.is_some_and(|l| *self.emitted >= l) {
                return Ok(DatasetFlow::LimitHit);
            }
        } else {
            *self.skipped += 1;
        }
        Ok(DatasetFlow::Continue)
    }
}

#[cfg(test)]
mod splice_spec_tests {
    use super::*;
    #[test]
    fn invalid_splice_windows_keep_spec_context_in_usage_errors() {
        for spec in ["alphasess:zz..3", "codex:alphasess:9..3", "alphasess:1-2"] {
            let error = parse_splice_spec(spec).err().expect("invalid range");
            let usage = error
                .downcast_ref::<crate::util::UsageError>()
                .expect("usage exit status");
            assert!(usage.0.contains(&format!("bad spec {spec:?}")), "{usage}");
            if spec.ends_with("1-2") {
                assert!(usage.0.contains("<id>:A..B"));
            }
        }
    }
}
