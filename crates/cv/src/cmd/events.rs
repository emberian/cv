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
    wide: bool,
    authority: crate::AuthorityArgs,
) -> Result<()> {
    use cv_core::events;
    let want = parse_harness(&harness)?;
    let auth_envelope = crate::cmd::browse::evaluate_cli_authority(&authority, None, None, false, false)?;
    let scoped_view = match auth_envelope {
        Some(env) => {
            let hub = cv_core::state::hub::HubStore::default_hub()?;
            Some(cv_core::custody::ScopedView::new(hub, env, authority.as_of.is_some()))
        }
        None => None,
    };

    let mut scoped_children: Vec<cv_core::custody::ScopedSource> = Vec::new();
    let (r, rows): (cv_core::SessionRef, Vec<events::EventRow>) = if let Some(ref view) = scoped_view {
        let src = match view.resolve(id, want) {
            Ok(s) => s,
            Err(cv_core::custody::ScopedResolveError::NotFound(_)) => {
                return match authority.as_of {
                    Some(gen) => crate::util::usage(format!("session {id:?} not found at generation {gen}")),
                    None => crate::util::usage("session not found in selected places"),
                };
            }
            Err(cv_core::custody::ScopedResolveError::Ambiguous(_)) => {
                return crate::util::usage(format!("ambiguous session id {id:?}"));
            }
            Err(cv_core::custody::ScopedResolveError::Other(e)) => return Err(e),
        };
        if view.pinned || src.entry.is_some() {
            if src.r.path.as_os_str().is_empty() {
                return match authority.as_of {
                    Some(gen) => {
                        crate::util::usage(format!("session {id:?} has no mirrored payload at generation {gen}"))
                    }
                    None => crate::util::usage(format!("session {id:?} has no mirrored payload")),
                };
            }
            if subagents {
                if let Some(ref entry) = src.entry {
                    view.omit(
                        &entry.producer_id,
                        "worker_inventory",
                        "Only collected sub-agent transcripts carry event evidence.",
                    );
                }
                scoped_children = view.children(&src)?;
            }
            let mut out_rows = view.event_rows(&src)?;
            if let Some(ref k) = kind {
                out_rows.retain(|e| &e.kind == k);
            }
            (src.r, out_rows)
        } else {
            let rows = events::read_ref(&src.r, kind.as_deref())?;
            (src.r, rows)
        }
    } else {
        let (r, _adapter) = resolve(id, want)?;
        let rows = events::read_ref(&r, kind.as_deref())?;
        (r, rows)
    };

    let use_local_subagents = subagents && authority.as_of.is_none() && !cv_core::custody::is_pack_ref(&r);

    if json {
        // One array: the session's own events, then (with --subagents) each sub-agent's, tagged.
        // Every row is `rows::event_row` — the same function `/api/session/{h}/{id}/events` prints
        // with, so the two doors cannot drift.
        let mut out: Vec<serde_json::Value> = rows.iter().map(cv_core::rows::event_row).collect();
        if use_local_subagents {
            out.extend(
                subagent_events(cv_core::subagent_tree_of(&r)?, r.harness, kind.as_deref())?
                    .into_iter()
                    .flat_map(|(s, evs)| {
                        let agent_id = s.agent_id().to_string();
                        let parent = r.id.clone();
                        let harness = r.harness.as_str().to_string();
                        let baggage = cv_core::hooks::Baggage::for_session(&harness, &s.session.id);
                        evs.into_iter().map(move |e| {
                            cv_core::rows::event_row(&events::EventRow {
                                baggage: baggage.clone(),
                                harness: harness.clone(),
                                session_id: parent.clone(),
                                msg_idx: e.msg_idx as i64,
                                ts: e.ts,
                                kind: e.kind.to_string(),
                                tool: e.tool,
                                target: e.target,
                                detail: e.detail,
                                parent_id: Some(parent.clone()),
                                agent_id: Some(agent_id.clone()),
                                workflow: s.workflow.clone(),
                            })
                        })
                    }),
            );
        } else if subagents {
            if let Some(ref view) = scoped_view {
                for child in &scoped_children {
                    if child.r.path.as_os_str().is_empty() {
                        continue;
                    }
                    let agent_id = cv_core::custody::ScopedView::agent_id(child).to_owned();
                    let workflow = child
                        .entry
                        .as_ref()
                        .and_then(|e| e.metadata["workflow"].as_str().map(str::to_owned));
                    for e in view.events(child)? {
                        if kind.as_deref().is_some_and(|k| e.kind != k) {
                            continue;
                        }
                        out.push(cv_core::rows::event_row(&events::EventRow {
                            baggage: None,
                            harness: r.harness.as_str().to_string(),
                            session_id: r.id.clone(),
                            msg_idx: e.msg_idx as i64,
                            ts: e.ts,
                            kind: e.kind.to_string(),
                            tool: e.tool,
                            target: e.target,
                            detail: e.detail,
                            parent_id: Some(r.id.clone()),
                            agent_id: Some(agent_id.clone()),
                            workflow: workflow.clone(),
                        }));
                    }
                }
            }
        }
        if let Some(view) = scoped_view {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "events": out,
                    "authority": *view.envelope.borrow(),
                }))?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
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
            wide,
        );
    }

    // `--subagents`: descend into the forest and attribute each agent's events to it. Sub-agents
    // aren't in the catalog (there can be thousands), so they're streamed on the spot through an
    // `EventSink` — large tool-result content stays on disk (lazy), only the small rows accumulate.
    if use_local_subagents {
        print_subagent_events(&r, kind.as_deref(), wide)?;
    } else if subagents {
        if let Some(ref view) = scoped_view {
            let mut total = 0usize;
            for child in &scoped_children {
                if child.r.path.as_os_str().is_empty() {
                    continue;
                }
                let evs: Vec<_> = view
                    .events(child)?
                    .into_iter()
                    .filter(|e| kind.as_deref().is_none_or(|k| e.kind == k))
                    .collect();
                if evs.is_empty() {
                    continue;
                }
                total += evs.len();
                let meta = child.entry.as_ref().map(|e| &e.metadata);
                let wf = meta
                    .and_then(|m| m["workflow"].as_str())
                    .map(|w| format!(" ⟐{w}"))
                    .unwrap_or_default();
                let agent_type = meta.and_then(|m| m["agent_type"].as_str()).unwrap_or("agent");
                let desc = meta
                    .and_then(|m| m["description"].as_str())
                    .or(child.r.title.as_deref())
                    .map(|d| format!("  — {}", truncate(d, 60)))
                    .unwrap_or_default();
                println!(
                    "\n┌─ sub-agent {}  {}{wf}  ({} event(s)){desc}",
                    short_id(cv_core::custody::ScopedView::agent_id(child)),
                    agent_type,
                    evs.len(),
                );
                for e in &evs {
                    print!("│ ");
                    print_event_row(
                        e.msg_idx as i64,
                        e.ts,
                        e.kind,
                        e.tool.as_deref(),
                        e.target.as_deref(),
                        e.detail.as_deref(),
                        wide,
                    );
                }
            }
            if total > 0 {
                println!("\n{total} event(s) across {} sub-agent(s)", scoped_children.len());
            }
        }
    }
    Ok(())
}

/// Stream each selected sub-agent through an [`EventSink`] and collect its events (filtered by
/// `kind`), skipping agents with none. Lazy parse → result bodies never materialize.
fn subagent_events(
    subs: Vec<cv_core::AgentRef>,
    harness: cv_core::Harness,
    kind: Option<&str>,
) -> Result<Vec<(cv_core::AgentRef, Vec<cv_core::events::Event>)>> {
    use cv_core::events::EventSink;
    use cv_core::Demand;
    if subs.is_empty() {
        return Ok(Vec::new());
    }
    let adapter = cv_core::harness::for_harness(harness).with_context(|| format!("no adapter for {harness}"))?;
    let mut out = Vec::new();
    for s in subs {
        let mut sink = EventSink::new(s.session.cwd.clone()).with_source(s.session.path.clone());
        adapter
            .try_stream(&s.session, &Demand::LEAN.with_spans(), &mut sink)
            .with_context(|| {
                format!(
                    "reading events for subagent {} ({})",
                    s.session.id,
                    s.session.path.display()
                )
            })?;
        let evs: Vec<_> = sink
            .into_events()?
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
fn print_subagent_events(r: &cv_core::SessionRef, kind: Option<&str>, wide: bool) -> Result<()> {
    let subs = cv_core::subagent_tree_of(r)?;
    let n_agents = subs.len();
    let per_agent = subagent_events(subs, r.harness, kind)?;
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
                wide,
            );
        }
    }
    if total > 0 {
        println!("\n{total} event(s) across {n_agents} sub-agent(s)");
    }
    Ok(())
}

/// The table's target and detail widths; `--wide` prints the cell whole instead.
const TARGET_COLUMN: usize = 90;
const DETAIL_COLUMN: usize = 100;

/// Print one event row in the shared `cv events` layout (index · time · kind · tool · target, then
/// an optional indented detail line). A cell past its column ends in `…` unless `wide`.
fn print_event_row(
    msg_idx: i64,
    ts: Option<i64>,
    kind: &str,
    tool: Option<&str>,
    target: Option<&str>,
    detail: Option<&str>,
    wide: bool,
) {
    let cell = |text: &str, column: usize| {
        if wide {
            text.replace('\n', " ")
        } else {
            truncate(text, column)
        }
    };
    let time = ts
        .and_then(|t| crate::util::fmt_local_ts(t, "%m-%d %H:%M"))
        .unwrap_or_else(|| "-----------".into());
    println!(
        "{:>5}  {:11}  {:9}  {:14}  {}",
        msg_idx,
        time,
        kind,
        tool.unwrap_or("-"),
        target.map(|t| cell(t, TARGET_COLUMN)).unwrap_or_default(),
    );
    if let Some(d) = detail {
        println!("       ↳ {}", cell(d, DETAIL_COLUMN));
    }
}

pub(crate) fn cmd_touched(path: &str, edits_only: bool, json: bool, authority: crate::AuthorityArgs) -> Result<()> {
    let auth_envelope = crate::cmd::browse::evaluate_cli_authority(&authority, None, None, false, false)?;
    let mut rows = if let Some(gen) = authority.as_of {
        cv_core::api::touched_at_generation(gen, path, edits_only, auth_envelope.as_ref().expect("pinned scope"))?
    } else {
        cv_core::events::sessions_touching(path, edits_only)?
    };
    if let Some(scope) = auth_envelope
        .as_ref()
        .filter(|_| authority.place.is_some() && authority.as_of.is_none())
    {
        let hub_opt = cv_core::state::hub::HubStore::default_hub().ok();
        rows.retain(|t| {
            let p = if cv_core::SessionKey::split_selector(&t.session_id).is_none() {
                "local".to_string()
            } else {
                hub_opt
                    .as_ref()
                    .and_then(|h| h.session_head_for(&t.session_id, authority.as_of).ok().flatten())
                    .map(|e| e.producer_id)
                    .unwrap_or_else(|| "local".to_string())
            };
            scope.includes(&p)
        });
    }
    if json {
        // `rows::touched_row`: the same row `/api/touched` serves.
        let out: Vec<serde_json::Value> = rows.iter().map(cv_core::rows::touched_row).collect();
        if let Some(env) = auth_envelope {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "touched": out,
                    "authority": env,
                }))?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unreadable_subagent_events_fail_with_identity() {
        let world = crate::test_world("events-unreadable-agent");
        let agent = cv_core::AgentRef {
            session: cv_core::SessionRef::new("agent-failed", cv_core::Harness::Claude, &world.home),
            agent_type: None,
            description: None,
            tool_use_id: None,
            workflow: None,
            result_status: None,
            result_summary: None,
        };
        let error = subagent_events(vec![agent], cv_core::Harness::Claude, None).unwrap_err();
        assert!(format!("{error:#}").contains("agent-failed"));
    }
}
