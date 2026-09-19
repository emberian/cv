//! `cv doctor` — diagnose why a session's context window keeps filling (and compacting).
//!
//! The analysis engine lives in [`cv_core::doctor`] (shared with the `doctor` MCP tool); this module
//! resolves the target session(s) and renders the report. Attributes context pressure to its
//! sources — tool results (split MCP vs builtin, ranked per tool), thinking, messages, images — and
//! sizes the fixed system+tools overhead from token usage. With no <id>, analyzes the most recent
//! session(s) for the current directory.

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

    // Resolve the target session(s).
    let targets: Vec<SessionRef> = if let Some(id) = &id {
        let (r, _ad) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
        vec![r]
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
            anyhow::bail!(
                "no sessions found for {} — pass an explicit <id>, or run from a project dir",
                cwd.map(|c| c.display().to_string()).unwrap_or_default()
            );
        }
        refs
    };

    let mut rep = Report::default();
    let mut label = String::new();
    for r in &targets {
        let (rr, adapter) =
            cv_core::find(&r.id, Some(r.harness))?.with_context(|| format!("could not load session {}", r.id))?;
        let session = adapter.parse(&rr)?;
        if label.is_empty() {
            label = crate::short_id(&rr.id);
        }
        rep.observe(&session);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rep.to_json())?);
        return Ok(());
    }
    print_report(&rep, &label, targets.len());
    Ok(())
}

fn print_report(rep: &Report, label: &str, n_targets: usize) {
    let scope = if rep.sessions > 1 {
        format!("{} sessions (this dir)", rep.sessions)
    } else {
        format!("session {label}")
    };
    println!("# compaction doctor — {scope}\n");

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

    // Fixed overhead + peak, from usage.
    if rep.startup_ctx > 0 {
        println!(
            "Overhead:    ~{} of fixed context every turn before any conversation\n             \
             (base prompt + CLAUDE.md/rules + skills + MCP tool schemas — sized from token usage,\n             \
             not itemizable: the transcript records the conversation, not the system block)",
            fmt_tok(rep.startup_ctx)
        );
    }
    if rep.peak_ctx > 0 {
        println!("Peak window: {} observed", fmt_tok(rep.peak_ctx));
    }

    // Where the conversational growth goes (measured).
    let total = rep.conv_total().max(1);
    println!(
        "\nConversational context by source ({} measured):",
        fmt_tok(rep.conv_total())
    );
    let mut rows: Vec<(&str, u64)> = vec![
        ("tool results", rep.tool_results),
        ("thinking", rep.thinking),
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
        println!("\nTop context consumers (tool results):");
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

fn verdict(rep: &Report) -> String {
    let total = rep.conv_total().max(1);
    let mcp = rep.mcp_tool_results();
    let mut parts: Vec<String> = Vec::new();

    // Tool output is the lever to pull when it's the single largest source (even below 50%) or
    // when tool results + call args together dominate the window.
    let tool_activity = rep.tool_results + rep.tool_call_args;
    let tool_results_is_top =
        rep.tool_results >= rep.thinking && rep.tool_results >= rep.assistant_text && rep.tool_results >= rep.user_text;
    if rep.tool_results > 0 && (tool_results_is_top || tool_activity * 2 >= total) {
        let mut t: Vec<_> = rep.by_tool.iter().filter(|(_, s)| s.result_tokens > 0).collect();
        t.sort_by_key(|e| std::cmp::Reverse(e.1.result_tokens));
        let top: Vec<String> = t.iter().take(2).map(|(n, _)| (*n).clone()).collect();
        parts.push(format!(
            "tool output is the biggest lever ({:.0}% of context, {:.0}% with tool-call args), led by {} — \
             trim verbose output, scope reads/greps, or `cv prune` old tool payloads",
            100.0 * rep.tool_results as f64 / total as f64,
            100.0 * tool_activity as f64 / total as f64,
            top.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" + ")
        ));
    }
    if mcp > 0 && mcp * 2 >= rep.tool_results {
        parts.push(format!(
            "MCP tools are {:.0}% of tool output — disabling unused MCP servers would free real room",
            100.0 * mcp as f64 / rep.tool_results.max(1) as f64
        ));
    }
    // ~25% of a 200k window is a heavy constant tax.
    if rep.startup_ctx >= 50_000 {
        parts.push(format!(
            "fixed overhead is ~{} before any conversation (leaner CLAUDE.md / fewer MCP servers \
             cuts this every turn)",
            fmt_tok(rep.startup_ctx)
        ));
    }
    if rep.thinking * 4 >= total && rep.thinking > 0 {
        parts.push("extended thinking is a large share — lower the thinking budget if you don't need it".into());
    }
    // System reminders (the attachments Claude Code appends per turn) at ≥ 10% are a real, and
    // steerable, share: background-task notifications, edited-file notices and hook stdout.
    if rep.attachments * 10 >= total && rep.attachments > 0 {
        let mut kinds: Vec<(&String, &u64)> = rep.by_attachment.iter().collect();
        kinds.sort_by_key(|k| std::cmp::Reverse(*k.1));
        let top: Vec<String> = kinds.iter().take(2).map(|(k, _)| format!("`{k}`")).collect();
        parts.push(format!(
            "system reminders are {:.0}% of context ({}) — quiet hook stdout, fewer concurrent \
             background tasks, or `cv prune` old turns",
            100.0 * rep.attachments as f64 / total as f64,
            top.join(" + ")
        ));
    }
    if parts.is_empty() {
        if rep.compactions == 0 {
            return "nothing alarming — context use looks balanced and the session hasn't compacted.".into();
        }
        return "context use is fairly balanced; compaction is driven by overall conversation length more than any one source.".into();
    }
    parts.join("; ")
}
