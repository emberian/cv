//! `cv events` / `cv touched` — the extracted-event catalog (`cv blame` lives in `blame.rs`).

use crate::util::{parse_harness, resolve, short_id};
use anyhow::{Context, Result};
use cv_core::ir::truncate;

// ---------- events / touched ----------

pub(crate) fn cmd_events(
    id: &str,
    harness: Option<String>,
    kind: Option<String>,
    subagents: bool,
    json: bool,
) -> Result<()> {
    use cv_core::events;
    let want = parse_harness(&harness)?;
    let (r, _adapter) = resolve(id, want)?;

    // Ensure this one session's events are current (cheap: a single streamed pass); a session
    // already cataloged at this mtime is a no-op.
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        events::ingest_ref(&r)?;
    }

    let rows = events::events_for(r.harness.as_str(), &r.id, kind.as_deref());
    if json {
        // One array: the session's own events, then (with --subagents) each sub-agent's, tagged.
        let mut out: Vec<serde_json::Value> = rows
            .iter()
            .map(|e| {
                event_json(
                    e.msg_idx,
                    e.ts,
                    &e.kind,
                    e.tool.as_deref(),
                    e.target.as_deref(),
                    e.detail.as_deref(),
                    e.agent_id.as_deref(),
                    e.parent_id.as_deref(),
                    e.workflow.as_deref(),
                )
            })
            .collect();
        if subagents {
            out.extend(subagent_events(&r, kind.as_deref())?.into_iter().flat_map(|(s, evs)| {
                let agent_id = s.agent_id().to_string();
                let parent = r.id.clone();
                evs.into_iter().map(move |e| {
                    event_json(
                        e.msg_idx as i64,
                        e.ts,
                        e.kind,
                        e.tool.as_deref(),
                        e.target.as_deref(),
                        e.detail.as_deref(),
                        Some(&agent_id),
                        Some(&parent),
                        s.workflow.as_deref(),
                    )
                })
            }));
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() && !subagents {
        match &kind {
            Some(k) => println!("no {k:?} events in {} (try without --kind)", short_id(&r.id)),
            None => println!("no tool events in {} (a chat-only session?)", short_id(&r.id)),
        }
        return Ok(());
    }

    println!(
        "{} event(s) in {}:{}\n",
        rows.len(),
        r.harness.as_str(),
        short_id(&r.id)
    );
    for e in &rows {
        print_event_row(
            e.msg_idx,
            e.ts,
            &e.kind,
            e.tool.as_deref(),
            e.target.as_deref(),
            e.detail.as_deref(),
        );
    }

    // `--subagents`: descend into the forest and attribute each agent's events to it. Sub-agents
    // aren't in the catalog (there can be thousands), so they're streamed on the spot through an
    // `EventSink` — large tool-result content stays on disk (lazy), only the small rows accumulate.
    if subagents {
        print_subagent_events(&r, kind.as_deref())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn event_json(
    msg_idx: i64,
    ts: Option<i64>,
    kind: &str,
    tool: Option<&str>,
    target: Option<&str>,
    detail: Option<&str>,
    agent_id: Option<&str>,
    parent_id: Option<&str>,
    workflow: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "msg_idx": msg_idx,
        "ts": ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.to_rfc3339()),
        "kind": kind,
        "tool": tool,
        "target": target,
        "detail": detail,
        "agent_id": agent_id,
        "parent_id": parent_id,
        "workflow": workflow,
    })
}

/// Stream every sub-agent of `r` through an [`EventSink`] and collect its events (filtered by
/// `kind`), skipping agents with none. Lazy parse → result bodies never materialize.
fn subagent_events(
    r: &cv_core::SessionRef,
    kind: Option<&str>,
) -> Result<Vec<(cv_core::SubagentInfo, Vec<cv_core::events::Event>)>> {
    use cv_core::events::EventSink;
    use cv_core::ParseOptions;
    let subs = cv_core::subagent_tree_of(r);
    if subs.is_empty() {
        return Ok(Vec::new());
    }
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;
    let mut out = Vec::new();
    for s in subs {
        let mut sink = EventSink::new(s.session.cwd.clone());
        // Best-effort: a single unreadable sub-agent transcript shouldn't abort the whole forest.
        if adapter.stream(&s.session, &ParseOptions::lazy(), &mut sink).is_err() {
            continue;
        }
        let evs: Vec<_> = sink
            .into_events()
            .into_iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        if !evs.is_empty() {
            out.push((s, evs));
        }
    }
    Ok(out)
}

/// Print each sub-agent's events grouped under the agent (with the agent's type/task as a header).
fn print_subagent_events(r: &cv_core::SessionRef, kind: Option<&str>) -> Result<()> {
    let per_agent = subagent_events(r, kind)?;
    let n_agents = cv_core::subagent_tree_of(r).len();
    let mut total = 0usize;
    for (s, evs) in &per_agent {
        total += evs.len();
        let wf = s.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
        println!(
            "\n┌─ sub-agent {}  {}{}  ({} event(s)){}",
            short_id(s.agent_id()),
            s.agent_type.as_deref().unwrap_or("agent"),
            wf,
            evs.len(),
            s.description
                .as_deref()
                .map(|d| format!("  — {}", truncate(d, 60)))
                .unwrap_or_default(),
        );
        for e in evs {
            print!("│ ");
            print_event_row(
                e.msg_idx as i64,
                e.ts,
                e.kind,
                e.tool.as_deref(),
                e.target.as_deref(),
                e.detail.as_deref(),
            );
        }
    }
    if total > 0 {
        println!("\n{total} event(s) across {n_agents} sub-agent(s)");
    }
    Ok(())
}

/// Print one event row in the shared `cv events` layout (index · time · kind · tool · target, then
/// an optional indented detail line).
fn print_event_row(
    msg_idx: i64,
    ts: Option<i64>,
    kind: &str,
    tool: Option<&str>,
    target: Option<&str>,
    detail: Option<&str>,
) {
    let time = ts
        .and_then(|t| crate::util::fmt_local_ts(t, "%m-%d %H:%M"))
        .unwrap_or_else(|| "-----------".into());
    println!(
        "{:>5}  {:11}  {:9}  {:14}  {}",
        msg_idx,
        time,
        kind,
        tool.unwrap_or("-"),
        target.map(|t| truncate(t, 90)).unwrap_or_default(),
    );
    if let Some(d) = detail {
        println!("       ↳ {}", truncate(d, 100));
    }
}

pub(crate) fn cmd_touched(path: &str, edits_only: bool, json: bool) -> Result<()> {
    let rows = cv_core::events::sessions_touching(path, edits_only);
    if json {
        let out: Vec<serde_json::Value> = rows
            .iter()
            .map(|t| {
                serde_json::json!({
                    "harness": t.harness,
                    "session_id": t.session_id,
                    "title": t.title,
                    "edits": t.edits,
                    "reads": t.reads,
                    "last_ts": t.last_ts.and_then(|s| chrono::DateTime::from_timestamp(s, 0)).map(|d| d.to_rfc3339()),
                    "agent_id": t.agent_id,
                    "parent_id": t.parent_id,
                    "workflow": t.workflow,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "no sessions {} {path:?} — events are ingested by `cv index` (run it first?)",
            if edits_only { "edited" } else { "touched" }
        );
        return Ok(());
    }
    println!("{} session(s) touched {path:?}:\n", rows.len());
    for t in &rows {
        let date = t
            .last_ts
            .and_then(|s| crate::util::fmt_local_ts(s, "%Y-%m-%d"))
            .unwrap_or_else(|| "----------".into());
        let counts = match (t.edits, t.reads) {
            (0, n) => format!("{n} read(s)"),
            (n, 0) => format!("{n} edit(s)"),
            (e, r) => format!("{e} edit(s), {r} read(s)"),
        };
        println!(
            "{:8}  {:8}  {:10}  {:22}  {}",
            t.harness,
            short_id(&t.session_id),
            date,
            counts,
            t.title.as_deref().map(|s| truncate(s, 56)).unwrap_or_default(),
        );
        // Folded-in sub-agent rows (`cv index --subagents`) carry attribution: surface which agent
        // of which workflow of which parent the touch came from.
        if t.parent_id.is_some() || t.agent_id.is_some() || t.workflow.is_some() {
            let wf = t.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
            println!(
                "          ↳ sub-agent {} of {}{}",
                t.agent_id.as_deref().map(short_id).unwrap_or_else(|| "?".into()),
                t.parent_id.as_deref().map(short_id).unwrap_or_else(|| "?".into()),
                wf,
            );
        }
    }
    Ok(())
}
