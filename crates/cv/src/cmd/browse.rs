//! `cv ls` / `cv timeline` / `cv stats` — browsing the discovered corpus.
//!
//! These read [`cv_core::sessions`] (the probed catalog — milliseconds warm, transparently a full
//! discovery when cold/stale); `cv ls --fresh` forces [`cv_core::discover_all`]'s full scan.

use crate::util::{dim_cwd, parse_harness, session_row, short_id};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};
use cv_core::sanitize::sanitize_line;

/// Apply a parsed query to a ref list in place. The two-phase filter itself is
/// [`cv_core::query::filter_refs`] — shared with `cvd`'s `/api/stats?q=`, so `ls`/`timeline`/
/// `stats` and the daemon all speak exactly the same `-q`; the CLI only supplies the `text:`
/// resolution, which needs the tantivy index cv-core cannot depend on.
fn apply_query(refs: &mut Vec<SessionRef>, query: &Option<cv_core::SessionQuery>) {
    let Some(q) = query else { return };
    crate::cmd::query::filter_refs(refs, q, &crate::cmd::query::text_sets(q));
}

/// Session rows for `--json` listings: the SAME refs the table would print (the `exists()` guard —
/// a row whose file vanished since the probe is dropped — comes free from the `stat` that yields
/// `size_bytes`), capped at `limit`.
fn json_rows<'a>(refs: impl Iterator<Item = &'a SessionRef>, limit: usize) -> Vec<(&'a SessionRef, serde_json::Value)> {
    refs.filter_map(|r| std::fs::metadata(&r.path).ok().map(|m| (r, m.len())))
        .take(limit)
        .map(|(r, size)| (r, session_row(r, Some(size))))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_ls(
    harness: Option<String>,
    cwd: Option<String>,
    query: Option<String>,
    limit: usize,
    sort_by: &str,
    fresh: bool,  // force a full re-discovery instead of trusting the probed catalog
    json: bool,   // emit the rows as one JSON array of session rows instead of the table
    enrich: bool, // --json only: add transcript-derived git + display_title (one parse per emitted row)
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    let all = if fresh {
        cv_core::discover_all()
    } else {
        cv_core::sessions()
    };
    let discovered = all.len();
    let mut refs: Vec<SessionRef> = all
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(c)),
        })
        .collect();
    apply_query(&mut refs, &query);

    match sort_by {
        "created" => refs.sort_by_key(|r| std::cmp::Reverse(r.created_at.or(r.updated_at))),
        "messages" => refs.sort_by_key(|r| std::cmp::Reverse(r.message_count)),
        // "updated" (the default; clap's value_parser admits nothing else)
        _ => refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at))),
    }

    let total = refs.len();
    if json {
        // Machine-readable listing: the SAME rows the table below would print (same filters, sort,
        // exists() guard, and limit), as one JSON array on stdout — no header/footer, so it pipes
        // cleanly. Transcript-derived text (title) stays raw here — sanitizing is the terminal
        // seam's job (G5); JSON is the machine contract.
        //
        // `--enrich` (git + display_title) is opt-in because it costs one transcript parse per
        // emitted row — O(limit), not O(fleet), but not the catalog-cheap default `ls --json`
        // promises. See the perf note in the flag's help.
        let rows: Vec<serde_json::Value> = json_rows(refs.iter(), limit)
            .into_iter()
            .map(|(r, mut obj)| {
                if enrich {
                    // Same transcript source `cv show --json` reads. A lazy parse leaves giant
                    // content on disk (memory-safe on the multi-MB "single-exchange giants" sesh
                    // warns about) while still carrying the session's git metadata and enough of the
                    // first user turn to synthesize a title.
                    if let Some(session) = cv_core::harness::for_harness(r.harness)
                        .and_then(|a| cv_core::stream::collect_with(a.as_ref(), r, &cv_core::ParseOptions::lazy()).ok())
                    {
                        let map = obj.as_object_mut().expect("json object");
                        // `git`: the same object `cv show --json` emits (branch/commit/remote,
                        // skip-none), omitted entirely when the transcript records no git context.
                        if let Some(git) = &session.git {
                            if let Ok(g) = serde_json::to_value(git) {
                                map.insert("git".into(), g);
                            }
                        }
                        // `display_title`: `title` with a first-real-user-text fallback. Explicit
                        // null (not absent) when a session has neither, so consumers can tell "no
                        // title anywhere" from "not enriched" (the key is absent without --enrich).
                        map.insert("display_title".into(), serde_json::json!(session.synth_title()));
                    }
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if total < discovered {
        println!("{total} session(s) (of {discovered} discovered; filtered)\n");
    } else {
        println!("{total} session(s)\n");
    }
    // The exists() check guards the catalog read path's one residual lie — a session deleted since
    // the probe last looked — and is bounded by `limit`, not the fleet (it stats only listed rows).
    for r in refs.iter().filter(|r| r.path.exists()).take(limit) {
        // Titles/cwds are transcript-derived (untrusted, replayed forever) — sanitize at the
        // terminal seam (G5), exactly like task/board rows.
        println!(
            "{:8}  {:8}  {}  {:>4} msg  {}",
            r.harness.as_str(),
            short_id(&r.id),
            crate::util::fmt_span(r.created_at, r.updated_at),
            r.message_count,
            r.title
                .as_deref()
                .map(|t| truncate(&sanitize_line(t), 60))
                .unwrap_or_else(|| sanitize_line(&dim_cwd(r.cwd.as_deref())).into_owned()),
        );
    }
    if total > limit {
        println!("\n… {} more (use --limit)", total - limit);
    }
    Ok(())
}

// ---------- timeline ----------

/// A unified chronological feed across all harnesses (oldest → newest, like a feed). Grouped by day.
pub(crate) fn cmd_timeline(
    harness: Option<String>,
    cwd: Option<String>,
    query: Option<String>,
    limit: usize,
    json: bool, // the shown window as one JSON array of session rows, oldest first
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    let mut refs: Vec<SessionRef> = cv_core::sessions()
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r.cwd.as_ref().map(|p| p.to_string_lossy().contains(c)).unwrap_or(false),
        })
        .collect();
    apply_query(&mut refs, &query);

    // Sort ascending by updated_at (falling back to created_at), so the feed reads oldest → newest.
    let key = |r: &SessionRef| r.updated_at.or(r.created_at);
    refs.sort_by_key(key);

    let total = refs.len();
    // A feed shows the *most recent* window; keep the last `limit` rows but still oldest → newest.
    let shown = if total > limit {
        &refs[total - limit..]
    } else {
        &refs[..]
    };
    if json {
        let rows: Vec<serde_json::Value> = json_rows(shown.iter(), limit).into_iter().map(|(_, v)| v).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if total > limit {
        println!("… {} older (use --limit)\n", total - limit);
    }

    let mut last_day: Option<String> = None;
    // exists(): same catalog-read guard as `cmd_ls` — O(shown window), not O(fleet).
    for r in shown.iter().filter(|r| r.path.exists()) {
        let when = key(r);
        // The feed instant is the session's LAST activity (its `updated_at`), local time — a
        // long-lived session appears at where it went quiet, with a `⇠ since` start marker.
        let day = when
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d"))
            .unwrap_or_else(|| "----------".into());
        if last_day.as_deref() != Some(day.as_str()) {
            println!("── {day} ──");
            last_day = Some(day.clone());
        }
        let time = when
            .map(|d| crate::util::fmt_local(d, "%H:%M"))
            .unwrap_or_else(|| "--:--".into());
        // Transcript-derived text — sanitize at the terminal seam (G5).
        let title = r
            .title
            .as_deref()
            .map(|t| truncate(&sanitize_line(t), 50))
            .unwrap_or_else(|| sanitize_line(&dim_cwd(r.cwd.as_deref())).into_owned());
        let since = match (r.created_at, when) {
            (Some(c), Some(u))
                if c.with_timezone(&chrono::Local).date_naive() != u.with_timezone(&chrono::Local).date_naive() =>
            {
                format!("  ⇠ since {}", crate::util::fmt_local(c, "%m-%d"))
            }
            _ => String::new(),
        };
        println!(
            "  {}  {:8}  {:8}  {:24}  {}{}",
            time,
            r.harness.as_str(),
            short_id(&r.id),
            truncate(&sanitize_line(&dim_cwd(r.cwd.as_deref())), 24),
            title,
            since,
        );
    }
    println!("\n{total} session(s)");
    Ok(())
}

// ---------- stats ----------

pub(crate) fn cmd_stats(query: Option<String>, json: bool) -> Result<()> {
    let query = crate::cmd::query::build(query)?;
    let mut refs = cv_core::sessions();
    apply_query(&mut refs, &query);
    // The aggregation and the `--json` payload both live in `cv_core::stats`, so `cv stats --json`
    // and `cvd`'s `/api/stats` are the same six numbers computed once — the dashboard used to
    // accumulate its own from whichever sessions the user had happened to open.
    let s = cv_core::stats::CorpusStats::compute(&refs);
    let total = s.sessions;

    if json {
        println!("{}", serde_json::to_string_pretty(&s.to_json())?);
        return Ok(());
    }

    if total == 0 {
        let scope = if query.is_some() {
            " match the query"
        } else {
            " discovered"
        };
        println!("no sessions{scope}.");
        return Ok(());
    }

    println!("✦ clustervision fleet stats\n");
    println!("{total} session(s) · {} message(s)\n", s.messages);

    println!("by harness:");
    for (h, n) in &s.by_harness {
        println!("  {h:12} {n:>5}");
    }

    println!("\ntop cwds:");
    for (c, n) in s.top_cwds.iter().take(cv_core::stats::TOP_CWDS) {
        println!("  {n:>5}  {}", truncate(c, 70));
    }

    println!("\ndate range:");
    println!(
        "  earliest created: {}",
        s.earliest_created
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  latest updated:   {}",
        s.latest_updated
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    Ok(())
}
