//! `cv doctor` — diagnose why a session's context window keeps filling (and compacting).
//!
//! The analysis engine lives in [`cv_core::doctor`]; this module
//! resolves the target session(s) and renders stored-payload estimates, recorded prompt usage,
//! and compaction evidence. First-prompt usage is only a heuristic overhead proxy. With no <id>,
//! analyzes the most recent session(s) for the current directory.

use anyhow::{Context, Result};
use cv_core::doctor::Report;
use cv_core::SessionRef;

fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn bar(frac: f64) -> String {
    "█".repeat((frac * 24.0).round() as usize)
}

pub(crate) fn cmd_doctor(id: Option<String>, harness: Option<String>, recent: usize, json: bool) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;

    // Resolve the target session(s). An explicit id arrives with its adapter (resolve found it);
    // the recent listing carries refs the loop below still has to load.
    let targets: Vec<(SessionRef, Option<Box<dyn cv_core::Adapter>>)> = if let Some(id) = &id {
        let (r, adapter) = crate::util::resolve(id, want)?;
        vec![(r, Some(adapter))]
    } else {
        let cwd = std::env::current_dir().ok();
        let mut refs: Vec<SessionRef> = cv_core::sessions()
            .into_iter()
            .filter(|r| want.is_none_or(|h| r.harness == h))
            .filter(|r| cwd.as_ref().is_none_or(|c| r.cwd.as_deref() == Some(c.as_path())))
            .collect();
        refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        refs.truncate(recent.max(1));
        if refs.is_empty() {
            // An empty listing may be a store cv could not open: name it with the refusal.
            let unreadable = skipped_lines(&cv_core::catalog::skipped_sources());
            anyhow::bail!(
                "no sessions found for {} — pass an explicit <id>, or run from a project dir{}",
                cwd.map(|c| c.display().to_string()).unwrap_or_default(),
                unreadable.iter().map(|l| format!("\n{l}")).collect::<String>()
            );
        }
        refs.into_iter().map(|r| (r, None)).collect()
    };
    let n_targets = targets.len();

    let mut rep = Report::default();
    let mut label = String::new();
    for (r, found) in targets {
        let (rr, adapter) = match found {
            Some(adapter) => (r, adapter),
            None => {
                cv_core::find(&r.id, Some(r.harness))?.with_context(|| format!("could not load session {}", r.id))?
            }
        };
        // Count decoded text in bounded chunks and request only attribution metadata facts.
        let mut pass = rep.session(Some(rr.path.clone()));
        cv_core::stream::stream_counted(
            adapter.as_ref(),
            &rr,
            &cv_core::Demand::FACTS.with_spans(),
            &mut pass,
            cv_core::stream::Pass::RECORD,
        )?;
        pass.finish()?;
        if label.is_empty() {
            label = crate::util::short_id(&rr.id);
        }
    }

    if json {
        let mut v = rep.to_json();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("hooks".into(), crate::cmd::hooks::doctor_json());
            let skipped = cv_core::catalog::skipped_sources();
            if !skipped.is_empty() {
                obj.insert("skipped_sources".into(), skipped_json(&skipped));
            }
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    print_report(&rep, &label, n_targets);
    Ok(())
}

fn print_report(rep: &Report, label: &str, n_targets: usize) {
    let scope = if rep.sessions > 1 {
        format!("{} sessions (this dir)", rep.sessions)
    } else {
        format!("session {label}")
    };
    println!("# compaction doctor — {scope}\n");

    // The cybernetic layer: are the hooks that make compaction visible from inside the session
    // installed, and which compaction knobs are in force (settings + environment).
    for line in crate::cmd::hooks::doctor_lines() {
        println!("{line}");
    }

    println!("{}", writer_versions_line(cv_core::catalog::writer_versions()));
    for line in skipped_lines(&cv_core::catalog::skipped_sources()) {
        println!("{line}");
    }

    // Compaction summary.
    if rep.compactions == 0 {
        println!(
            "Compaction:  never compacted across {} message(s) — healthy ✅",
            rep.messages
        );
    } else {
        let avg = rep.pre_tokens.iter().sum::<u64>() / rep.pre_tokens.len().max(1) as u64;
        let worst = rep.pre_tokens.iter().copied().max().unwrap_or(0);
        println!(
            "Compaction:  {} time(s) ({} auto) · avg pre-compaction {} · worst {}",
            rep.compactions,
            rep.auto_compactions,
            fmt_tok(avg),
            fmt_tok(worst)
        );
    }

    // Recorded prompt usage and byte-based attribution are different sources of evidence.
    if rep.startup_ctx > 0 {
        println!(
            "First prompt: {} recorded (includes conversation; heuristic proxy for fixed overhead)",
            fmt_tok(rep.startup_ctx)
        );
    }
    if rep.peak_ctx > 0 {
        println!("Peak prompt:  {} recorded in transcript", fmt_tok(rep.peak_ctx));
    }

    let total = rep.conv_total().max(1);
    println!(
        "\nTranscript content by source ({} estimated token equivalents):\n\
         (an estimate: bytes at calibrated per-kind rates, ~2.2–2.5 bytes/token, images ~3k; totals include history across compactions)",
        fmt_tok(rep.conv_total())
    );
    if rep.thinking > 0 {
        println!("(thinking is mostly signature bytes; requests carry them, and most models count them)");
    }
    let mut rows: Vec<(&str, u64)> = vec![
        ("tool results", rep.tool_results),
        ("thinking payload", rep.thinking),
        ("assistant text", rep.assistant_text),
        ("user text", rep.user_text),
        ("tool-call args", rep.tool_call_args),
        ("images", rep.images),
        ("system msgs", rep.system_text),
        ("system reminders", rep.attachments),
    ];
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (name, v) in rows {
        if v == 0 {
            continue;
        }
        let frac = v as f64 / total as f64;
        println!(
            "  {:<15} {:>4.0}%  {:<24} {}",
            name,
            frac * 100.0,
            bar(frac),
            fmt_tok(v)
        );
    }

    // Top tools by result tokens (the usual variable bloat).
    let mut tools: Vec<(&String, &cv_core::doctor::ToolStat)> = rep.by_tool.iter().collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.1.result_tokens));
    let shown: Vec<_> = tools.iter().filter(|(_, s)| s.result_tokens > 0).take(8).collect();
    if !shown.is_empty() {
        println!("\nLargest stored tool-result payloads (estimated):");
        for (name, s) in &shown {
            let tag = if s.is_mcp { " [MCP]" } else { "" };
            println!(
                "  {:<28}{:<6} {:>7}  ({} call{})",
                name,
                tag,
                fmt_tok(s.result_tokens),
                s.calls,
                if s.calls == 1 { "" } else { "s" }
            );
        }
        let mcp = rep.mcp_tool_results();
        let builtin = rep.tool_results.saturating_sub(mcp);
        if mcp > 0 {
            let share = 100.0 * mcp as f64 / rep.tool_results.max(1) as f64;
            println!(
                "\nMCP vs builtin tool results:  MCP {} ({:.0}%) · builtin {}",
                fmt_tok(mcp),
                share,
                fmt_tok(builtin)
            );
        }
    }

    // System reminders by kind — the per-turn context Claude Code itself appends (hook output,
    // edited-file notices, queued task notifications, CLAUDE.md `instructions`, …). Before these
    // were itemized they were misread as part of the fixed overhead.
    if rep.attachments > 0 {
        let mut kinds: Vec<(&String, &u64)> = rep.by_attachment.iter().collect();
        kinds.sort_by_key(|k| std::cmp::Reverse(*k.1));
        println!("\nSystem reminders (attachments) by kind:");
        for (kind, v) in kinds.iter().take(6) {
            println!("  {:<28} {:>7}", kind, fmt_tok(**v));
        }
    }

    println!("\nVerdict: {}", verdict(rep));
    if n_targets == 1 && rep.sessions == 1 {
        println!("  (analyze your whole project: `cv doctor --recent 20`)");
    }
}

/// The sources the last discovery found but could not open: the store has bytes, the listing has
/// zero. Catalog knowledge only, as [`writer_versions_line`]; nothing when there are none.
fn skipped_lines(skipped: &[(cv_core::Harness, cv_core::Skipped)]) -> Vec<String> {
    if skipped.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Unreadable sources: {} (the store has bytes, the listing has zero)",
        skipped.len()
    )];
    lines.extend(
        skipped
            .iter()
            .map(|(h, s)| format!("  {h} {} — {}", s.path.display(), s.why)),
    );
    lines
}

fn skipped_json(skipped: &[(cv_core::Harness, cv_core::Skipped)]) -> serde_json::Value {
    skipped
        .iter()
        .map(|(h, s)| serde_json::json!({"harness": h.as_str(), "path": s.path, "why": s.why}))
        .collect()
}

/// Render only catalog knowledge. This helper never resolves or probes source paths.
fn writer_versions_line(observations: Vec<cv_core::writer_version::Observed>) -> String {
    let rows: Vec<_> = observations
        .into_iter()
        .map(|o| {
            let verified = o.verified;
            let status = match o
                .newest
                .as_deref()
                .and_then(|v| cv_core::writer_version::compare(v, verified))
            {
                Some(std::cmp::Ordering::Greater) => " · newer than verified",
                _ => "",
            };
            format!(
                "{} observed {} / verified {}{} ({} unknown of {})",
                o.harness,
                o.newest.as_deref().unwrap_or("unknown"),
                verified,
                status,
                o.unknown,
                o.sessions
            )
        })
        .collect();
    format!("Writer versions: {}", rows.join("; "))
}

#[cfg(test)]
mod writer_version_tests {
    use super::*;
    #[test]
    fn catalog_knowledge_and_unknown_coverage() {
        let line = writer_versions_line(vec![
            cv_core::writer_version::Observed {
                harness: "claude",
                verified: "2.1.304",
                newest: Some("2.10.0".into()),
                sessions: 5,
                unknown: 2,
            },
            cv_core::writer_version::Observed {
                harness: "codex",
                verified: "0.163.0",
                newest: None,
                sessions: 0,
                unknown: 0,
            },
        ]);
        assert!(line.contains("newer than verified"));
        assert!(line.contains("2 unknown of 5"));
        assert!(line.contains("codex observed unknown"));
    }
}

fn verdict(rep: &Report) -> String {
    let total = rep.conv_total().max(1);
    let mcp = rep.mcp_tool_results();
    let mut parts: Vec<String> = Vec::new();

    // Tool output is a possible lever when it dominates estimated transcript content.
    let tool_activity = rep.tool_results + rep.tool_call_args;
    let tool_results_is_top = [
        rep.thinking,
        rep.assistant_text,
        rep.user_text,
        rep.tool_call_args,
        rep.images,
        rep.system_text,
        rep.attachments,
    ]
    .into_iter()
    .all(|tokens| rep.tool_results >= tokens);
    if rep.tool_results > 0 && (tool_results_is_top || tool_activity * 2 >= total) {
        let mut t: Vec<_> = rep.by_tool.iter().filter(|(_, s)| s.result_tokens > 0).collect();
        t.sort_by_key(|e| std::cmp::Reverse(e.1.result_tokens));
        let top: Vec<String> = t.iter().take(2).map(|(n, _)| (*n).clone()).collect();
        let share = if tool_results_is_top {
            format!(
                "tool output is the largest estimated transcript source ({:.0}%, {:.0}% with tool-call args)",
                100.0 * rep.tool_results as f64 / total as f64,
                100.0 * tool_activity as f64 / total as f64
            )
        } else {
            format!(
                "tool calls and output account for {:.0}% of estimated transcript content ({:.0}% output)",
                100.0 * tool_activity as f64 / total as f64,
                100.0 * rep.tool_results as f64 / total as f64
            )
        };
        parts.push(format!(
            "{share}, led by {} — trim verbose output, scope reads/greps, or `cv prune` old tool payloads",
            top.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" + ")
        ));
    }
    if mcp > 0 && mcp * 2 >= rep.tool_results {
        parts.push(format!(
            "MCP tools are {:.0}% of estimated tool output — review verbose results and unused tool schemas",
            100.0 * mcp as f64 / rep.tool_results.max(1) as f64
        ));
    }
    // A large first prompt merits inspection, but cannot isolate fixed system overhead.
    if rep.startup_ctx >= 50_000 {
        parts.push(format!(
            "first recorded prompt is {} including conversation — inspect initial history, CLAUDE.md and MCP schemas",
            fmt_tok(rep.startup_ctx)
        ));
    }
    if rep.thinking * 4 >= total && rep.thinking > 0 {
        parts.push(
            "stored thinking payloads are a large estimated share; opaque bytes do not establish reasoning-token cost"
                .into(),
        );
    }
    // System reminders (the attachments Claude Code appends per turn) at ≥ 10% are a real, and
    // steerable, share: background-task notifications, edited-file notices and hook stdout.
    if rep.attachments * 10 >= total && rep.attachments > 0 {
        let mut kinds: Vec<(&String, &u64)> = rep.by_attachment.iter().collect();
        kinds.sort_by_key(|k| std::cmp::Reverse(*k.1));
        let top: Vec<String> = kinds.iter().take(2).map(|(k, _)| format!("`{k}`")).collect();
        parts.push(format!(
            "system reminders are {:.0}% of estimated transcript content ({}) — quiet hook stdout, fewer concurrent \
             background tasks, or `cv prune` old turns",
            100.0 * rep.attachments as f64 / total as f64,
            top.join(" + ")
        ));
    }
    if parts.is_empty() {
        if rep.compactions == 0 {
            return "the session has no recorded compactions; this report identifies no dominant tool, thinking or reminder payload.".into();
        }
        return "this report identifies no dominant tool, thinking or reminder payload; recorded compactions do not isolate a single cause."
            .into();
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verdict_distinguishes_tool_output_from_other_dominant_sources() {
        for report in [
            Report {
                tool_results: 100,
                system_text: 900,
                ..Default::default()
            },
            Report {
                tool_results: 100,
                images: 900,
                ..Default::default()
            },
            Report {
                tool_results: 100,
                attachments: 900,
                ..Default::default()
            },
        ] {
            let text = verdict(&report);
            assert!(!text.contains("tool output is the largest"), "{text}");
            assert!(!text.contains("balanced"), "{text}");
        }
        let report = Report {
            tool_results: 100,
            tool_call_args: 900,
            ..Default::default()
        };
        let text = verdict(&report);
        assert!(text.contains("tool calls and output account for 100%"), "{text}");
        assert!(!text.contains("tool output is the largest"), "{text}");
        let report = Report {
            tool_results: 900,
            assistant_text: 100,
            ..Default::default()
        };
        assert!(verdict(&report).contains("tool output is the largest"));
    }
}
