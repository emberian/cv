//! `cv blame` — CLI presentation over [`cv_core::blame`].

use anyhow::{Context, Result};
use cv_core::blame::{self, BlameReport, CommitBlame, SessionAttribution, Strength};
use cv_core::ir::{truncate, Harness};

pub fn cmd_blame(file: &str, lines: Option<&str>, show: bool, json: bool) -> Result<()> {
    let report = blame::blame(file, lines)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if !report.in_git_repo {
        return render_event_only(file, &report);
    }

    if report.commits.is_empty() {
        println!("{}", no_commits(&report.file, lines));
        return Ok(());
    }

    if report.cold_catalog {
        eprintln!("(event catalog is empty — run `cv index` first to ingest tool events)");
    }

    println!(
        "{}\n",
        header(&report.file, lines, report.commits.len(), report.total_commits)
    );

    let mut best_overall: Option<(String, String, i64)> = None;
    for c in &report.commits {
        let summary = if c.summary.is_empty() {
            "(no message)"
        } else {
            &c.summary
        };
        println!("◆ {} {}  {summary}", c.short_sha, date(c.time));
        if c.matches.is_empty() {
            println!("  (no agent session found)");
            continue;
        }
        for m in &c.matches {
            print_attribution(m, c);
            best_overall.get_or_insert_with(|| (m.harness.clone(), m.session_id.clone(), m.msg_idx));
        }
    }

    println!(
        "\nprovenance: {} of {} commit(s) matched an agent session",
        report.matched_commits,
        report.commits.len()
    );

    if show {
        match &best_overall {
            None => println!("(--show: no agent session matched any commit)"),
            Some((harness, sid, idx)) => show_best(harness, sid, *idx)?,
        }
    }
    Ok(())
}

fn render_event_only(file: &str, report: &BlameReport) -> Result<()> {
    println!("(not inside a git repository — event-catalog matches only, no commit correlation)\n");
    if report.event_only_sessions.is_empty() {
        if report.cold_catalog {
            println!("no edit events found — run `cv index` first to ingest tool events");
        } else {
            println!("no sessions edited {file:?}");
        }
        return Ok(());
    }
    println!("{} session(s) edited {file:?}:\n", report.event_only_sessions.len());
    for e in &report.event_only_sessions {
        let dt = e.ts.map(date).unwrap_or_else(|| "----------".into());
        println!(
            "  {:8} {}  {}  {}  last edit at msg {}",
            e.harness,
            crate::util::short_id(&e.session_id),
            dt,
            e.title.as_deref().map(|t| truncate(t, 48)).unwrap_or_default(),
            e.msg_idx,
        );
        println!("{}", show_hint(&e.session_id, e.range));
    }
    Ok(())
}

fn header(rel: &str, lines: Option<&str>, shown: usize, total: usize) -> String {
    let more = if total > shown {
        format!(" (of {total}; showing the most recent)")
    } else {
        String::new()
    };
    match lines {
        Some(spec) => format!("✦ blame {rel} -L {spec} — {shown} commit(s) own those lines{more}"),
        None => format!("✦ blame {rel} — {shown} commit(s){more}"),
    }
}

fn no_commits(rel: &str, lines: Option<&str>) -> String {
    match lines {
        Some(spec) => format!(
            "no commit owns {rel} -L {spec} — are those lines committed? (event-catalog view: `cv touched {rel}`)"
        ),
        None => format!("no commits touch {rel} — is it committed? (event-catalog view: `cv touched {rel}`)"),
    }
}

fn print_attribution(m: &SessionAttribution, c: &CommitBlame) {
    let dt = m.ts.map(date).unwrap_or_else(|| "----------".into());
    let title = m
        .title
        .as_deref()
        .map(|t| format!("\"{}\"  ", truncate(t, 48)))
        .unwrap_or_default();

    if m.strength.is_exact_sha() {
        let how = match m.commit_evidence.as_deref() {
            Some("pushed") => "commit pushed here",
            _ => "commit created here",
        };
        println!(
            "  {:8} {}  {}  {}commit at msg {} ({} · {how})",
            m.harness,
            crate::util::short_id(&m.session_id),
            dt,
            title,
            m.msg_idx,
            m.label,
        );
        println!("    ↳ cv rewind {} --at {}", hint_id(&m.session_id), c.short_sha);
        println!("{}", show_hint(&m.session_id, m.range));
        return;
    }

    let detail = match m.strength {
        Strength::ContentHunk => match m.delta_secs {
            Some(d) => format!("({} · {})", m.label, fmt_delta(d)),
            None => format!("({})", m.label),
        },
        Strength::Timed => match m.delta_secs {
            Some(d) => format!("({} · {})", m.label, fmt_delta(d)),
            None => format!("({})", m.label),
        },
        Strength::SessionSpan => format!("({}; session active at commit time; no event timestamps)", m.label),
        Strength::CommitShaAndContentHunk | Strength::CommitSha => unreachable!(),
    };
    let elsewhere = if m.cwd_in_repo { "" } else { " · other checkout" };
    println!(
        "  {:8} {}  {}  {}edit at msg {} {detail}{elsewhere}",
        m.harness,
        crate::util::short_id(&m.session_id),
        dt,
        title,
        m.msg_idx,
    );
    println!("{}", show_hint(&m.session_id, m.range));
}

fn show_hint(session_id: &str, (s, end): (usize, usize)) -> String {
    format!("    ↳ cv show {} --range {s}..{end}", hint_id(session_id))
}

fn hint_id(id: &str) -> String {
    if id.starts_with("agent-") {
        id.to_string()
    } else {
        crate::util::short_id(id)
    }
}

fn show_best(harness: &str, session_id: &str, msg_idx: i64) -> Result<()> {
    let want = Harness::parse(harness);
    let (r, adapter) = cv_core::find(session_id, want)?
        .with_context(|| format!("session {harness}:{session_id} not found (deleted since indexing?)"))?;
    let (start, end) = blame::range_hint(msg_idx);
    println!(
        "\n── conversation around the edit · {harness} {} · msg {start}..{end} ──\n",
        crate::util::short_id(session_id)
    );
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    crate::cmd::view::stream_session_render(
        adapter.as_ref(),
        &r,
        &mut out,
        crate::cmd::view::show_header,
        crate::cmd::view::show_message,
        crate::cmd::view::RenderOpts {
            range: Some(crate::util::Window::Range(start, Some(end))),
            max_bytes: None,
        },
    )?;
    use std::io::Write;
    out.flush()?;
    Ok(())
}

fn date(ts: i64) -> String {
    crate::util::fmt_local_ts(ts, "%Y-%m-%d").unwrap_or_else(|| "----------".into())
}

pub(crate) fn fmt_delta(delta: i64) -> String {
    let (mag, dir) = if delta >= 0 {
        (delta, "before")
    } else {
        (-delta, "after")
    };
    let (h, m, s) = (mag / 3600, (mag % 3600) / 60, mag % 60);
    let t = if h > 0 {
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    };
    format!("{t} {dir} commit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_blame_reports_truncation_and_its_own_empty_case() {
        assert_eq!(
            header("a.rs", Some("3,9"), 20, 31),
            "✦ blame a.rs -L 3,9 — 20 commit(s) own those lines (of 31; showing the most recent)"
        );
        assert_eq!(
            header("a.rs", Some("3"), 2, 2),
            "✦ blame a.rs -L 3 — 2 commit(s) own those lines"
        );
        assert_eq!(
            header("a.rs", None, 20, 31),
            "✦ blame a.rs — 20 commit(s) (of 31; showing the most recent)"
        );
        assert!(no_commits("a.rs", Some("3")).starts_with("no commit owns a.rs -L 3 — are those lines committed?"));
        assert!(no_commits("a.rs", None).starts_with("no commits touch a.rs — is it committed?"));
    }

    #[test]
    fn every_show_hint_spells_a_sub_agent_in_full() {
        assert_eq!(
            show_hint("blame-e2e-session", blame::range_hint(1)),
            "    ↳ cv show blame-e2 --range 0..4"
        );
        assert_eq!(
            show_hint("agent-a1b2c3d4e5f6", blame::range_hint(142)),
            "    ↳ cv show agent-a1b2c3d4e5f6 --range 139..145"
        );
    }

    #[test]
    fn delta_humanizes() {
        assert_eq!(fmt_delta(8 * 60), "8m before commit");
        assert_eq!(fmt_delta(2 * 3600 + 600), "2h10m before commit");
        assert_eq!(fmt_delta(3 * 3600), "3h before commit");
        assert_eq!(fmt_delta(-30), "30s after commit");
    }
}
