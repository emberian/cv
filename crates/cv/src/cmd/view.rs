//! `cv show` (and its `--tree` / `--kind` views) / `cv export` — reading sessions,
//! plus the streaming renderer (`stream_session_render`) and its header/message helpers.

use super::frames::Frames;
use crate::util::{
    continue_hint, home_rel, parse_harness, resolve, resolve_found, short_id, split_harness_id, usage, KindSelector,
    StreamWindow, Window, WindowArgs,
};
use anyhow::{bail, Context, Result};
use cv_core::ir::{truncate, Block, Harness, Message, MessageKind, Session, SessionRef};
use cv_core::render::turn_label;
use cv_core::Adapter;
use std::io::Write;
use std::path::PathBuf;

/// The views `cv show` offers instead of the plain transcript (each flag documented on `Cmd::Show`).
pub(crate) struct ShowView {
    pub subagents: bool,
    pub agent: Option<String>,
    pub pre_compaction: Option<usize>,
    pub tree: bool,
    pub kind: Option<String>,
    pub summaries: bool,
}

/// What `--kind` selects: each family has one compact view.
pub(super) enum KindView {
    /// `prompt` / `answer`: what the person said (`session_reports::print_prompts`).
    Said { prompts: bool, answers: bool },
    /// `compaction_boundary` / `compaction_summary`: the compaction seams, each boundary paired
    /// with its summary (`compaction::print_compactions`).
    Compactions,
    /// `plan`, `schedule`, `background_job`, … (or `affordances`, all of them): the affordance rows
    /// (`cv_core::affordance`, `docs/design/AFFORDANCES.md`).
    Affordances(std::collections::HashSet<cv_core::affordance::Kind>),
}

pub(super) fn parse_kinds(spec: &str) -> Result<KindView> {
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Family {
        Said,
        Compactions,
        Affordances,
    }
    const SAID: [&str; 2] = ["prompt", "answer"];
    const SEAMS: [&str; 2] = ["compaction_boundary", "compaction_summary"];
    let kinds: Vec<&str> = spec.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    let words: Vec<&str> = cv_core::affordance::Kind::ALL.iter().map(|k| k.as_str()).collect();
    let accepted = format!(
        "`--kind` takes `prompt,answer` (what the person said), \
         `compaction_boundary,compaction_summary` (the compaction seams), or affordances: {} (`affordances` is all of them)",
        words.join(",")
    );
    let family = |k: &str| {
        if SAID.contains(&k) {
            Some(Family::Said)
        } else if SEAMS.contains(&k) {
            Some(Family::Compactions)
        } else if k == "affordances" || cv_core::affordance::Kind::parse(k).is_some() {
            Some(Family::Affordances)
        } else {
            None
        }
    };
    if let Some(k) = kinds.iter().find(|k| family(k).is_none()) {
        return usage(format!("no `--kind` view for {k:?} — {accepted}"));
    }
    let families: std::collections::BTreeSet<Family> = kinds.iter().filter_map(|k| family(k)).collect();
    match families.iter().copied().collect::<Vec<_>>().as_slice() {
        [] => usage(format!("`--kind` is empty — {accepted}")),
        [Family::Said] => Ok(KindView::Said {
            prompts: kinds.contains(&"prompt"),
            answers: kinds.contains(&"answer"),
        }),
        [Family::Compactions] => Ok(KindView::Compactions),
        [Family::Affordances] => Ok(KindView::Affordances(
            cv_core::affordance::parse_kinds(&kinds).expect("every word checked above"),
        )),
        _ => usage(format!("`--kind {spec}` mixes two views — {accepted}")),
    }
}

/// Slice projected `--kind` rows by `selector` (`--first`, `--last`, `--range`, `--around`) and
/// trim at `max_bytes` AFTER projection. The first in-window row always fits whole.
pub(crate) fn slice_and_budget<T>(
    rows: Vec<T>,
    selector: Option<KindSelector>,
    max_bytes: Option<usize>,
    overhead: usize,
    row_index: impl Fn(&T) -> Option<usize>,
    measure: impl Fn(&T) -> Result<usize>,
) -> Result<(Vec<T>, Option<usize>)> {
    let (start, end) = match selector {
        Some(KindSelector::Rows(w)) => w.bounds(|| Ok(rows.len()))?,
        Some(KindSelector::AroundMessage { msg_index, context }) => {
            let tx_len = rows.iter().position(|r| row_index(r).is_none()).unwrap_or(rows.len());
            let tx = &rows[..tx_len];
            let first = tx.partition_point(|r| row_index(r).is_some_and(|i| i < msg_index));
            let after = tx.partition_point(|r| row_index(r).is_some_and(|i| i <= msg_index));
            (
                first.saturating_sub(context),
                Some(after.saturating_add(context).min(tx_len)),
            )
        }
        None => (0, None),
    };
    let start = start.min(rows.len());
    let end = end.map_or(rows.len(), |e| e.min(rows.len())).max(start);
    let mut kept = Vec::new();
    let mut used = overhead;
    let mut next = None;
    for (offset, row) in rows.into_iter().skip(start).take(end - start).enumerate() {
        let size = if max_bytes.is_some() { measure(&row)? } else { 0 };
        if !kept.is_empty() && max_bytes.is_some_and(|budget| used.saturating_add(size) > budget) {
            next = Some(start + offset);
            break;
        }
        used = used.saturating_add(size);
        kept.push(row);
    }
    Ok((kept, next))
}

fn format_affordance_row(a: &cv_core::affordance::Affordance) -> String {
    use cv_core::sanitize::sanitize_line;
    let at = match (a.index, a.source) {
        (Some(i), _) => format!("[{i}]"),
        (None, cv_core::affordance::Source::Store) => "store".to_string(),
        (None, _) => "session".to_string(),
    };
    let ts = a
        .timestamp
        .map(|t| crate::util::fmt_local(t, "%Y-%m-%d %H:%M"))
        .unwrap_or_else(|| "----------------".into());
    let tool = a.tool.as_deref().map(|t| format!("  {t}")).unwrap_or_default();
    let mut out = format!(
        "{at} {ts}  {}/{}{}  {}\n",
        a.kind.as_str(),
        a.action,
        sanitize_line(&tool),
        sanitize_line(&a.summary)
    );
    for it in &a.items {
        let st = it.status.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "      [{}] {}\n",
            sanitize_line(st),
            sanitize_line(&truncate(&it.content, 160))
        ));
    }
    out
}

/// Post-filter windowing for `--kind` views (`pre_compaction` bounds raw message indices before
/// projection; `selector` and `max_bytes` slice projected rows afterward).
#[derive(Clone, Copy)]
pub(crate) struct KindWindow {
    pub pre_compaction: Option<(usize, Option<usize>)>,
    pub selector: Option<KindSelector>,
    pub max_bytes: Option<usize>,
}

/// `show --kind plan,schedule,…`: the affordance rows of one session, transcript rows in message
/// order then the side store's, sliced by `selector` and `max_bytes` after projection.
fn print_affordances(
    r: &SessionRef,
    adapter: &dyn Adapter,
    kinds: &std::collections::HashSet<cv_core::affordance::Kind>,
    win: KindWindow,
    json: bool,
) -> Result<()> {
    let mut rows = cv_core::affordance::read(adapter, r)?;
    rows.retain(|a| {
        kinds.contains(&a.kind)
            && match (a.index, win.pre_compaction) {
                (Some(i), Some((start, end))) => i >= start && end.is_none_or(|e| i < e),
                _ => true,
            }
    });
    let overhead = if json { 5 } else { 100 };
    let (rows, next) = slice_and_budget(
        rows,
        win.selector,
        win.max_bytes,
        overhead,
        |row| row.index,
        |row| {
            if json {
                let pretty = serde_json::to_string_pretty(row)?;
                Ok(pretty.len() + 2 * pretty.lines().count() + 2)
            } else {
                Ok(format_affordance_row(row).len())
            }
        },
    )?;
    if json {
        if let Some(next) = next {
            eprintln!("{}", continue_hint(next));
        }
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("# affordances of {} — {} row(s)\n", short_id(&r.id), rows.len());
    for a in &rows {
        print!("{}", format_affordance_row(a));
    }
    if let Some(next) = next {
        println!("{}", continue_hint(next));
    }
    Ok(())
}

pub(crate) fn cmd_show(
    id: &str,
    harness: Option<String>,
    json: bool,
    window: &WindowArgs,
    view: ShowView,
) -> Result<()> {
    let ShowView {
        subagents,
        agent,
        pre_compaction,
        tree,
        kind,
        summaries,
    } = view;
    // Parse the selector (and `--kind`) before any I/O so a bad value errors immediately.
    let selector = window.selects()?;
    let kind = kind.as_deref().map(parse_kinds).transpose()?;
    if summaries && !matches!(kind, Some(KindView::Compactions)) {
        return usage("`--summaries` goes with `--kind compaction_boundary,compaction_summary`");
    }

    if subagents {
        let (r, _adapter) = resolve(id, parse_harness(&harness)?)?;
        return show_subagents(&r, json);
    }

    let (r, adapter, sub) = resolve_show_target(id, parse_harness(&harness)?, agent.as_deref())?;

    if let Some(kind) = kind {
        return match kind {
            KindView::Said { prompts, answers } => {
                let win = KindWindow {
                    pre_compaction: pre_compaction_span(&r, pre_compaction)?,
                    selector: window.kind_selector_in(adapter.as_ref(), &r)?,
                    max_bytes: window.max_bytes,
                };
                crate::cmd::session_reports::print_prompts(&r, adapter.as_ref(), win, prompts, answers, json)
            }
            KindView::Affordances(kinds) => {
                let win = KindWindow {
                    pre_compaction: pre_compaction_span(&r, pre_compaction)?,
                    selector: window.kind_selector_in(adapter.as_ref(), &r)?,
                    max_bytes: window.max_bytes,
                };
                print_affordances(&r, adapter.as_ref(), &kinds, win, json)
            }
            KindView::Compactions => {
                if selector || window.max_bytes.is_some() || pre_compaction.is_some() {
                    return usage("the compaction view covers the whole session; drop the window flags");
                }
                crate::cmd::compaction::print_compactions(&r, summaries, json)
            }
        };
    }
    if tree && !json {
        if selector || window.max_bytes.is_some() {
            return usage("`--tree` renders the whole session; drop the window flags (or add `--json` for the IR)");
        }
        return print_tree(&r, adapter.as_ref());
    }

    let range = show_range(&r, adapter.as_ref(), window, pre_compaction)?;
    if json {
        return print_session_json(adapter.as_ref(), &r, range, window.max_bytes, false, None).map(|_| ());
    }
    if let Some(sub) = &sub {
        print_subagent_banner(sub);
    }
    render_text(adapter.as_ref(), &r, range, window.max_bytes)
}

fn pre_compaction_span(r: &SessionRef, pre_compaction: Option<usize>) -> Result<Option<(usize, Option<usize>)>> {
    let Some(n) = pre_compaction else {
        return Ok(None);
    };
    if n == 0 {
        return usage("--pre-compaction is 1-based; use 1 for the first boundary");
    }
    let comps = cv_core::compaction::detect(r, false)?;
    if comps.is_empty() {
        bail!("{} never compacted — nothing pre-compaction to show", short_id(&r.id));
    }
    let idx = n.saturating_sub(1);
    let (start, end) = cv_core::compaction::pre_compaction_span(&comps, idx).with_context(|| {
        format!(
            "{} compacted {} time(s); no compaction #{n} (use 1..={})",
            short_id(&r.id),
            comps.len(),
            comps.len(),
        )
    })?;
    eprintln!(
        "✦ pre-compaction #{n} of {}: messages {start}..{end} (the span before boundary @msg {})",
        comps.len(),
        comps[idx].boundary_msg_idx,
    );
    Ok(Some((start, Some(end))))
}

/// The window `show` reads: the selector's, or — under `--pre-compaction <N>` — the span before
/// the Nth (1-based) compaction boundary, the context the continued agent lost, so it is
/// retrieved by message range without the caller computing offsets by hand. Left unresolved:
/// `--last N` is resolved by whichever pass reads the session, not by a counting pass first.
fn show_range(
    r: &SessionRef,
    adapter: &dyn Adapter,
    window: &WindowArgs,
    pre_compaction: Option<usize>,
) -> Result<Option<Window>> {
    if let Some((start, end)) = pre_compaction_span(r, pre_compaction)? {
        return Ok(Some(Window::Range(start, end)));
    }
    window.window_in(adapter, r)
}

/// The CLI's window over `r`: the selector's bounds or the last `n`, cut at the byte budget.
fn window_sink(r: &SessionRef, window: Option<Window>, max_bytes: Option<usize>) -> Result<cv_core::api::WindowSink> {
    use cv_core::api::WindowSink;
    let sink = match window.map_or(StreamWindow::Range(0, None), Window::streaming) {
        StreamWindow::Tail(n) => WindowSink::new(&r.path, 0, None).tail(n),
        StreamWindow::Range(start, end) => WindowSink::new(&r.path, start, end),
    };
    Ok(sink.budget(max_bytes))
}

/// Collect a full-fidelity JSON window, materialized, while reading through EOF for final
/// session metadata: redaction's cross-field rules need the decoded tree.
fn session_json_window(
    adapter: &dyn Adapter,
    r: &SessionRef,
    window: Option<Window>,
    max_bytes: Option<usize>,
) -> Result<(Session, Option<usize>)> {
    let mut sink = window_sink(r, window, max_bytes)?.keeping();
    let mut session = cv_core::stream::stream_counted(
        adapter,
        r,
        &cv_core::Demand::RICH.with_spans(),
        &mut sink,
        cv_core::stream::Pass::RECORD,
    )?
    .session;
    sink.finish()?;
    session.messages = sink.take_kept();
    Ok((session, sink.next))
}

/// Stage selected JSON frames while the adapter runs to EOF. Frames spill rather than retaining
/// decoded payloads; the final Session envelope also spills before stdout is touched.
fn staged_session_json(
    adapter: &dyn Adapter,
    r: &SessionRef,
    window: Option<Window>,
    max_bytes: Option<usize>,
    document: bool,
) -> Result<(cv_core::bytes::ByteSource, Option<usize>)> {
    staged_session_json_inner(adapter, r, window, max_bytes, document, false)
}

fn staged_session_json_inner(
    adapter: &dyn Adapter,
    r: &SessionRef,
    window: Option<Window>,
    max_bytes: Option<usize>,
    document: bool,
    checked: bool,
) -> Result<(cv_core::bytes::ByteSource, Option<usize>)> {
    let mut sink = window_sink(r, window, max_bytes)?;
    let pass = cv_core::stream::Pass {
        guard: checked,
        ..cv_core::stream::Pass::RECORD
    };
    let read = cv_core::stream::stream_counted(adapter, r, &cv_core::Demand::RICH.with_spans(), &mut sink, pass)?;
    let frames = sink.finish()?;
    let mut output = std::io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::default());
    super::session_json::write_document(&mut output, &read.session, &frames, sink.selected, document)?;
    output.write_all(b"\n")?;
    if let Some(guard) = read.guard {
        guard.verify(r)?;
    }
    Ok((
        output.into_inner().map_err(|error| error.into_error())?.finish()?,
        sink.next,
    ))
}

/// `show --json` / `export --format json`: the whole IR (incl. `extra`), windowed, and — under
/// `--max-bytes` — cut at the budget with the continuation hint on **stderr** (stdout stays JSON).
///
/// `as_document` wraps the same bytes as an **OpenSession** document (`docs/OPENSESSION.md`):
/// the IR's own serialization with `"open_session": "0.3"` as the first key. That marker belongs
/// to a *document*, not to a `Session` — `cv show --json` and `cv serve` serialize
/// the raw IR and their key sets are pinned — so it is added here, at the one boundary where cv
/// hands a session to somebody else, and nowhere else.
fn print_session_json(
    adapter: &dyn Adapter,
    r: &SessionRef,
    window: Option<Window>,
    max_bytes: Option<usize>,
    as_document: bool,
    redact: Option<&cv_core::redact::RedactOptions>,
) -> Result<cv_core::redact::RedactStats> {
    if redact.is_none() {
        let (output, next) = staged_session_json(adapter, r, window, max_bytes, as_document)?;
        if let Some(next) = next {
            eprintln!("{}", continue_hint(next));
        }
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        output.copy_to(&mut out)?;
        out.flush()?;
        return Ok(cv_core::redact::RedactStats::default());
    }
    // Redaction's cross-field rules require the materialized selected tree.
    let (mut session, next) = session_json_window(adapter, r, window, max_bytes)?;
    let mut stats = cv_core::redact::RedactStats::default();
    if let Some(next) = next {
        eprintln!("{}", continue_hint(next));
    }
    if let Some(opts) = redact {
        let (clean, st) = cv_core::redact::redact_with(&session, opts)?;
        session = clean;
        stats = st;
    }
    if as_document {
        println!(
            "{}",
            serde_json::to_string_pretty(&cv_core::harness::opensession::document(&session))?
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&session)?);
    }
    Ok(stats)
}

/// The rendered-transcript path of `cv show`: the window and byte budget the caller asked for,
/// else the whole transcript, on a terminal and a pipe alike.
fn render_text(adapter: &dyn Adapter, r: &SessionRef, range: Option<Window>, max_bytes: Option<usize>) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let outcome = stream_session_render(
        adapter,
        r,
        &mut out,
        show_header,
        show_message,
        RenderOpts { range, max_bytes },
    )?;
    if let Some(next) = outcome.next {
        writeln!(out, "{}", continue_hint(next))?;
    }
    out.flush()?;
    Ok(())
}

fn agent_parents_for_harness(parents: Vec<SessionRef>, want: Option<Harness>) -> Vec<SessionRef> {
    parents
        .into_iter()
        .filter(|parent| want.is_none_or(|h| parent.harness == h))
        .collect()
}

/// Resolve `cv show <id> [--agent <sub>]` across top-level sessions and sub-agents alike so
/// transcript, `--json`, `--kind`, and `--tree` views share one addressing path.
fn resolve_show_target(
    id: &str,
    want: Option<Harness>,
    agent: Option<&str>,
) -> Result<(SessionRef, Box<dyn Adapter>, Option<cv_core::AgentRef>)> {
    if let Some(agent_id) = agent {
        let (parent, adapter) = resolve(id, want)?;
        let sub = resolve_subagent(&parent, agent_id)?;
        return Ok((sub.session.clone(), adapter, Some(sub)));
    }
    let (want, id) = split_harness_id(id, want);
    if id.starts_with("agent-") {
        if let Some((sub, adapter)) = resolve_agent_fleetwide(id, want)? {
            return Ok((sub.session.clone(), adapter, Some(sub)));
        }
    }
    let found = match cv_core::find_cheap(id, want) {
        Ok(Some(hit)) => Ok(Some(hit)),
        Ok(None) => {
            if let Some((sub, adapter)) = resolve_agent_fleetwide(id, want)? {
                return Ok((sub.session.clone(), adapter, Some(sub)));
            }
            if let Some((parent_spec, agent_spec)) = id.split_once('/') {
                let (parent, adapter) = resolve(parent_spec, want)?;
                let sub = resolve_subagent(&parent, agent_spec)?;
                return Ok((sub.session.clone(), adapter, Some(sub)));
            }
            cv_core::find(id, want)
        }
        Err(e) => Err(e),
    };
    match found {
        Ok(Some((r, adapter))) => Ok((r, adapter, None)),
        Ok(None) => usage(format!("no session (and no sub-agent) matching {id:?}")),
        Err(e) => resolve_found(Err(e), id, want).map(|(r, a)| (r, a, None)),
    }
}

pub(crate) struct FleetwideSubagent {
    pub parent: SessionRef,
    pub adapter: Box<dyn Adapter>,
    pub sub: cv_core::AgentRef,
}

/// `cv show <agent-id>` with no parent given: find which session(s) spawned the agent (filename
/// scan across the fleet) and resolve it through the one parent — or list the candidates when the
/// prefix is ambiguous. Returns `Ok(None)` when nothing agent-shaped matched.
pub(crate) fn resolve_agent_fleetwide(
    agent_id: &str,
    want: Option<Harness>,
) -> Result<Option<(cv_core::AgentRef, Box<dyn Adapter>)>> {
    let Some(hit) = resolve_agent_parent_fleetwide(agent_id, want)? else {
        return Ok(None);
    };
    eprintln!(
        "✦ sub-agent of session {} ({})",
        short_id(&hit.parent.id),
        hit.parent.title.as_deref().unwrap_or("untitled"),
    );
    Ok(Some((hit.sub, hit.adapter)))
}

pub(crate) fn resolve_agent_parent_fleetwide(
    agent_id: &str,
    want: Option<Harness>,
) -> Result<Option<FleetwideSubagent>> {
    let parents = agent_parents_for_harness(cv_core::find_subagent_parents(agent_id), want);
    // Copies of one agent across sessions (`cv adopt`) read through the newest parent's copy.
    let parents = if parents.len() > 1 && cv_core::one_agent_across(&parents, agent_id)? {
        eprintln!(
            "✦ {} sessions hold a copy of this agent (adopted); reading the newest — `cv show <session> --agent {agent_id}` for another",
            parents.len()
        );
        parents[..1].to_vec()
    } else {
        parents
    };
    match parents.as_slice() {
        [] => Ok(None),
        [parent] => {
            let adapter = cv_core::harness::for_harness(parent.harness)
                .with_context(|| format!("no adapter for {}", parent.harness))?;
            let sub = resolve_subagent(parent, agent_id)?;
            Ok(Some(FleetwideSubagent {
                parent: parent.clone(),
                adapter,
                sub,
            }))
        }
        many => {
            // Ambiguous: every candidate as a pasteable `harness:id` line, exit 2.
            let mut lines: Vec<String> = many
                .iter()
                .map(|p| {
                    format!(
                        "{}:{}   # cv show {} --agent {agent_id} ({})",
                        p.harness.as_str(),
                        p.id,
                        short_id(&p.id),
                        p.title.as_deref().unwrap_or("untitled")
                    )
                })
                .collect();
            lines.sort();
            usage(format!(
                "ambiguous sub-agent id {agent_id:?} — {} sessions spawned a match:\n{}",
                many.len(),
                lines.join("\n")
            ))
        }
    }
}

/// List the sub-agent forest a session spawned (the `--subagents` view): every direct/`Workflow`
/// sub-agent with its type, journaled outcome, and final return value. `json` emits the structured
/// [`AgentRef`] forest (each annotated with its return) for machine consumption.
fn show_subagents(r: &SessionRef, json: bool) -> Result<()> {
    let subs = cv_core::subagent_tree_of(r)?;

    if json {
        // Enrich each with its final return value (direct agents report via the parent tool_result;
        // workflow agents via the journal summary already on the struct).
        let enriched: Vec<serde_json::Value> = subs
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    // Surface the bare agent id (the journal/transcript key) so consumers don't have
                    // to know the `agent-` stripping convention.
                    obj.insert("agent_id".into(), serde_json::Value::String(s.agent_id().to_string()));
                    // Direct agents have no journaled summary — attach their final return value.
                    if s.result_summary.is_none() {
                        if let Some(ret) = cv_core::harness::claude::subagent_return(&s.session.path)? {
                            obj.insert("return".into(), serde_json::Value::String(ret));
                        }
                    }
                }
                Ok(v)
            })
            .collect::<Result<_>>()?;
        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }

    if subs.is_empty() {
        println!("no sub-agents spawned by {}", short_id(&r.id));
        return Ok(());
    }

    println!("# sub-agents of {}\n", short_id(&r.id));
    for s in &subs {
        let wf = s.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        println!("• {}{wf}", subagent_line(s));
        // A direct agent has no journaled summary (the line showed its task): add its real return,
        // the last text turn of its transcript. Transcript-derived, so sanitized at the seam (G5);
        // the `--json` path stays raw.
        if s.result_summary.is_none() {
            if let Some(ret) = cv_core::harness::claude::subagent_return(&s.session.path)? {
                println!("  ↩ {}", truncate(&cv_core::sanitize::sanitize_line(&ret), 400));
            }
        }
    }
    Ok(())
}

/// One sub-agent of `parent` by its `agent-…` id, bare agentId, or a unique prefix of either — the
/// resolution `cv show --agent` and `cv rewind --agent` share. No match / several → exit 2.
pub(crate) fn resolve_subagent(parent: &SessionRef, agent_id: &str) -> Result<cv_core::AgentRef> {
    // A member id is the agent's own (Claude's `agent-<id>`) or `<parent>/<agent>` (Kimi, Kimi
    // Code): either spelling's agent part is what a caller types.
    let subs = cv_core::subagent_tree_filtered_of(parent, cv_core::SubagentDemand::Rich, None, |id, _| {
        [id, id.rsplit('/').next().unwrap_or(id)].into_iter().any(|id| {
            id.starts_with(agent_id) || id.strip_prefix("agent-").is_some_and(|bare| bare.starts_with(agent_id))
        })
    })?;
    let matches: Vec<&cv_core::AgentRef> = subs.iter().collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => usage(format!(
            "no sub-agent matching {agent_id:?} under {} (try `cv show {} --subagents`)",
            short_id(&parent.id),
            short_id(&parent.id),
        )),
        many => {
            let mut lines: Vec<String> = many
                .iter()
                .map(|s| format!("{}:{}", s.session.harness.as_str(), s.session.id))
                .collect();
            lines.sort();
            usage(format!(
                "ambiguous sub-agent id {agent_id:?} — {} candidates under {}:\n{}",
                many.len(),
                short_id(&parent.id),
                lines.join("\n")
            ))
        }
    }
}

/// Provenance banner printed above a plain text sub-agent transcript.
fn print_subagent_banner(sub: &cv_core::AgentRef) {
    if let Some(d) = &sub.description {
        println!("# sub-agent: {}", truncate(d, 120));
    }
    let wf = sub
        .workflow
        .as_deref()
        .map(|w| format!(" · workflow {w}"))
        .unwrap_or_default();
    let status = sub
        .result_status
        .as_deref()
        .map(|st| format!(" · {st}"))
        .unwrap_or_default();
    println!(
        "{} · {}{}{}\n",
        sub.agent_type.as_deref().unwrap_or("agent"),
        sub.agent_id(),
        status,
        wf,
    );
}

#[derive(Clone, Copy)]
pub(crate) enum ExportFormat {
    Markdown,
    OpenSession,
}

/// Complete export staged privately before HTTP download publication. Uses the native
/// emitters without CLI window or pipe budgets; requested payloads remain message-sized.
pub(crate) fn stage_export(
    adapter: &dyn Adapter,
    r: &SessionRef,
    format: ExportFormat,
) -> Result<(cv_core::bytes::ByteSource, u64)> {
    let _reads = cv_core::source::read_scope();
    if matches!(format, ExportFormat::OpenSession) {
        let (body, next) = staged_session_json_inner(adapter, r, None, None, true, true)?;
        anyhow::ensure!(next.is_none(), "full export unexpectedly truncated");
        let bytes = body.byte_len()?;
        return Ok((body, bytes));
    }
    let mut output = std::io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::default());
    let resolver = cv_core::Resolver::new(Some(r.path.clone()));
    let outcome = stream_session_render_inner(
        adapter,
        r,
        &mut output,
        md_header,
        |m| md_message(m, &resolver),
        RenderOpts::default(),
        Mode::CheckedMarkdown,
    )?;
    anyhow::ensure!(outcome.next.is_none(), "full export unexpectedly truncated");
    let stage = output.into_inner().map_err(|error| error.into_error())?;
    let bytes = stage.len();
    Ok((stage.finish()?, bytes))
}

pub(crate) fn cmd_export(
    id: &str,
    format: &str,
    harness: Option<String>,
    window: &WindowArgs,
    redact: bool,
) -> Result<()> {
    use cv_core::redact::{redact_message, redact_text, RedactOptions, RedactStats};
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let range = window.window_in(adapter.as_ref(), &r)?;
    // `--redact`: the same scrubber `cv share` runs (API keys, private keys, JWTs, emails,
    // assignments, high-entropy blobs), applied per block before anything is rendered; the
    // per-class counts go to stderr so stdout stays the document.
    let resolver = cv_core::Resolver::new(Some(r.path.clone()));
    let opts = RedactOptions::default();
    let stats = std::cell::RefCell::new(RedactStats::default());
    match format {
        "md" | "markdown" => {
            if !redact && range.is_none() && window.max_bytes.is_none() {
                // The same checked complete document serves native save and HTTP download.
                // Finish every source demand before stdout can receive even a header prefix.
                let (body, _) = stage_export(adapter.as_ref(), &r, ExportFormat::Markdown)?;
                let mut out = std::io::BufWriter::new(std::io::stdout().lock());
                body.copy_to(&mut out)?;
                out.flush()?;
                return Ok(());
            }
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            let outcome = stream_session_render(
                adapter.as_ref(),
                &r,
                &mut out,
                |h| {
                    let head = md_header(h);
                    if redact {
                        redact_text(&head)
                    } else {
                        head
                    }
                },
                |m| {
                    if !redact {
                        return md_message(m, &resolver);
                    }
                    let mut m = m.clone();
                    redact_message(&mut m, &opts, &mut stats.borrow_mut(), &resolver)?;
                    md_message(&m, &resolver)
                },
                RenderOpts {
                    range,
                    max_bytes: window.max_bytes,
                },
            )?;
            if let Some(next) = outcome.next {
                writeln!(out, "{}", continue_hint(next))?;
            }
            out.flush()?;
        }
        // `--format json` IS OpenSession 0.3: the IR, plus the version marker.
        "json" => {
            if !redact && range.is_none() && window.max_bytes.is_none() {
                // Checked and complete, like the Markdown document above.
                let (body, _) = stage_export(adapter.as_ref(), &r, ExportFormat::OpenSession)?;
                let mut out = std::io::BufWriter::new(std::io::stdout().lock());
                body.copy_to(&mut out)?;
                out.flush()?;
                return Ok(());
            }
            let st = print_session_json(
                adapter.as_ref(),
                &r,
                range,
                window.max_bytes,
                true,
                redact.then_some(&opts),
            )?;
            *stats.borrow_mut() = st;
        }
        other => bail!("unknown format {other:?} (use md or json; `cv share` for html)"),
    }
    if redact {
        eprintln!("{}", redaction_summary(&stats.borrow()));
    }
    Ok(())
}

/// The one-line per-class tally `export --redact` and `share` print to stderr.
pub(crate) fn redaction_summary(st: &cv_core::redact::RedactStats) -> String {
    format!(
        "✦ redacted {} item(s): {} api_key, {} private_key, {} jwt, {} email, {} blob, {} assignment",
        st.total(),
        st.api_keys,
        st.private_keys,
        st.jwts,
        st.emails,
        st.blobs,
        st.assignments,
    )
}

// ---------- show --tree ----------

struct TreeRow {
    id: Option<String>,
    parent: Option<String>,
    line: String,
}

fn read_tree(r: &SessionRef, adapter: &dyn Adapter) -> Result<(Session, Vec<TreeRow>, Option<String>)> {
    struct TreeSink {
        rows: Vec<TreeRow>,
        resolver: cv_core::Resolver,
        first_prompt: Option<String>,
        error: Option<anyhow::Error>,
    }
    impl cv_core::MessageSink for TreeSink {
        fn meta(&mut self, session: &Session) {
            self.resolver = session.resolver();
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.error.take()
        }
        fn message(&mut self, m: Message) -> cv_core::Flow {
            let result = (|| -> Result<()> {
                if self.first_prompt.is_none() {
                    self.first_prompt = m.prompt_title(&self.resolver)?;
                }
                let preview = cv_core::render::message_text_preview(&m, &self.resolver, 80)?;
                let line = tree_line_with_preview(&m, preview);
                self.rows.push(TreeRow {
                    id: m.id,
                    parent: m.parent_id,
                    line,
                });
                Ok(())
            })();
            if let Err(error) = result {
                self.error = Some(error);
                return cv_core::Flow::Stop;
            }
            cv_core::Flow::Continue
        }
    }
    let mut sink = TreeSink {
        rows: Vec::new(),
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        first_prompt: None,
        error: None,
    };
    let pass = cv_core::stream::Pass {
        guard: true,
        ..cv_core::stream::Pass::RECORD
    };
    let read = cv_core::stream::stream_counted(adapter, r, &cv_core::Demand::FACTS.with_spans(), &mut sink, pass)?;
    let session = read.session;
    if let Some(error) = sink.error {
        return Err(error);
    }
    read.guard.expect("guarded pass").verify(r)?;
    Ok((session, sink.rows, sink.first_prompt))
}

fn print_tree(r: &SessionRef, adapter: &dyn Adapter) -> Result<()> {
    let (session, rows, first_prompt) = read_tree(r, adapter)?;
    println!(
        "# {}",
        cv_core::label_from(session.title.as_deref(), first_prompt.as_deref())
    );
    println!("{} · {} · {} msg", session.harness, session.id, rows.len());
    println!();
    if rows.iter().any(|row| row.parent.is_some()) {
        walk_tree(&rows, |index, depth| {
            println!("{}• {}", "  ".repeat(depth), rows[index].line)
        });
    } else {
        for (i, row) in rows.iter().enumerate() {
            println!("{:>4}. {}", i + 1, row.line);
        }
    }
    render_subagent_forest(r)?;
    Ok(())
}

/// Print the sub-agent forest under a session: directly-spawned sub-agents, then each workflow's
/// agents grouped together with the journaled `status: summary` outcome. Reads only the cheap
/// metadata/journal sidecars (no transcript bodies).
fn render_subagent_forest(r: &SessionRef) -> Result<()> {
    use std::collections::BTreeMap;
    let subs = cv_core::subagent_tree_of(r)?;
    if subs.is_empty() {
        return Ok(());
    }

    let direct: Vec<&cv_core::AgentRef> = subs.iter().filter(|s| s.workflow.is_none()).collect();
    // Workflow agents grouped by run id (BTreeMap → stable run order).
    let mut by_wf: BTreeMap<&str, Vec<&cv_core::AgentRef>> = BTreeMap::new();
    for s in subs.iter().filter(|s| s.workflow.is_some()) {
        by_wf.entry(s.workflow.as_deref().unwrap_or("")).or_default().push(s);
    }

    println!();
    println!(
        "## sub-agents ({} direct, {} workflow agent(s) across {} workflow(s))",
        direct.len(),
        subs.len() - direct.len(),
        by_wf.len()
    );

    if !direct.is_empty() {
        println!();
        for s in &direct {
            println!("• {}", subagent_line(s));
        }
    }
    for (wf, agents) in &by_wf {
        println!("\n  ⟐ workflow {wf}  ({} agent(s))", agents.len());
        for s in agents {
            println!("    • {}", subagent_line(s));
        }
    }
    Ok(())
}

/// One line describing a sub-agent — `cv show --tree`'s forest and `cv show --subagents` share it: short
/// id, type, the journaled status (workflow agents), message count, and the journaled summary or,
/// failing that, the task description — capped and sanitized (both are transcript-derived).
fn subagent_line(s: &cv_core::AgentRef) -> String {
    let id = short_id(s.agent_id());
    let kind = s.agent_type.as_deref().unwrap_or("agent");
    let status = s
        .result_status
        .as_deref()
        .map(|st| format!(" [{st}]"))
        .unwrap_or_default();
    // Prefer the journaled summary (the real outcome); fall back to the task description.
    let blurb = s
        .result_summary
        .as_deref()
        .or(s.description.as_deref())
        .map(|t| truncate(&cv_core::sanitize::sanitize_line(t), 88))
        .unwrap_or_default();
    format!(
        "{id}  {kind}{status}  {} msg  {blurb}",
        s.session.message_count.map_or("–".to_string(), |n| n.to_string())
    )
}

/// Render messages as a DAG by `parent_id`, indented by the forks above each row: an only child
/// keeps its parent's depth and each child of a fork sits one deeper, so a straight chain is flat
/// and the output stays linear in the session. Roots (no/unknown parent) sit at depth 0.
fn walk_tree(rows: &[TreeRow], mut visit: impl FnMut(usize, usize)) {
    let mut by_id = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(id) = &row.id {
            by_id.insert(id.as_str(), i);
        }
    }
    let mut children = vec![Vec::new(); rows.len()];
    let mut roots = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let parent = row
            .parent
            .as_deref()
            .and_then(|id| by_id.get(id).copied())
            .filter(|&p| p != i);
        match parent {
            Some(parent) => children[parent].push(i),
            None => roots.push(i),
        }
    }
    let mut visited = vec![false; rows.len()];
    let mut stack = Vec::new();
    // Preserve ordinary root order; then cover disconnected parent cycles in transcript order.
    for root in roots.into_iter().chain(0..rows.len()) {
        if visited[root] {
            continue;
        }
        stack.push((root, 0));
        while let Some((index, depth)) = stack.pop() {
            if std::mem::replace(&mut visited[index], true) {
                continue;
            }
            visit(index, depth);
            let below = depth + usize::from(children[index].len() > 1);
            stack.extend(children[index].iter().rev().map(|&child| (child, below)));
        }
    }
}

/// One-line preview of a message for the tree: role (with its kind when that says more than the
/// role), markers for tool turns / sub-agent spawns, and a text preview.
fn tree_line_with_preview(m: &Message, preview: Option<String>) -> String {
    let role = turn_label(m);
    let mut tags = Vec::new();
    let has_tool_use = m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
    let has_tool_result = m.content.iter().any(|b| matches!(b, Block::ToolResult { .. }));
    if has_tool_use {
        tags.push("🔧 tool".to_string());
        // Surface a sub-agent spawn if the tool looks like one.
        for b in &m.content {
            if let Block::ToolUse { name, .. } = b {
                let n = name.to_ascii_lowercase();
                if n.contains("task") || n.contains("agent") || n.contains("dispatch") || n.contains("spawn") {
                    tags.push(format!("↳ sub-agent ({name})"));
                }
            }
        }
    }
    if has_tool_result {
        tags.push("↩ result".to_string());
    }
    // The adapter said so: a spawn the tool-name heuristic didn't catch.
    if m.kind == MessageKind::SubagentSpawn && !tags.iter().any(|t| t.starts_with("↳ sub-agent")) {
        tags.push("↳ sub-agent".to_string());
    }

    let preview = preview.unwrap_or_else(|| {
        if has_tool_use {
            m.content
                .iter()
                .find_map(|b| match b {
                    Block::ToolUse { name, .. } => Some(format!("[{name}]")),
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            String::new()
        }
    });

    let tagstr = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    };
    format!("{role}{tagstr}  {preview}")
}

// ---------- rendering helpers ----------

/// Everything a streamed renderer needs for a session's header — derived from the cheap
/// [`SessionRef`] (label/cwd) plus the model captured from the first assistant turn.
pub(crate) struct HeaderInfo {
    harness: Harness,
    id: String,
    cwd: Option<PathBuf>,
    label: String,
    model: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// What to render: an optional message window and an optional byte budget.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RenderOpts {
    /// `Last(n)` is resolved in the render's own pass: it keeps the last `n` messages as they
    /// stream by and renders them at the end, so `--last` reads the session once, not twice.
    pub range: Option<Window>,
    /// Stop before the message that would push the rendered output past this many bytes (the
    /// first in-window message is always rendered, so progress is always possible).
    pub max_bytes: Option<usize>,
}

/// What a render did: how many messages it wrote, and — when the byte budget cut it short — the
/// index of the first message it did NOT write (the `--range <next>..` continuation point).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RenderOutcome {
    pub rendered: usize,
    pub next: Option<usize>,
}

/// How a render delivers: to its writer, or as a complete checked Markdown export.
enum Mode {
    Plain,
    CheckedMarkdown,
}

/// Render a session to `out` by **streaming** — each message is rendered and written as it arrives,
/// then dropped, so a multi-GB transcript renders at O(largest message) instead of materializing the
/// whole `Session`. The header needs the label and model, which come from the first user/assistant
/// turns, so it's held behind the shared holdback rule (`frames::Frames::ready`: model and title
/// known, else a bounded message count) and then flushed ahead of the body — header info lives in
/// the first turns, so the buffer stays tiny.
pub(crate) fn stream_session_render<W: std::io::Write>(
    adapter: &dyn Adapter,
    r: &SessionRef,
    out: &mut W,
    header: impl Fn(&HeaderInfo) -> String,
    render_msg: impl Fn(&Message) -> Result<String>,
    opts: RenderOpts,
) -> Result<RenderOutcome> {
    stream_session_render_inner(adapter, r, out, header, render_msg, opts, Mode::Plain)
}

fn stream_session_render_inner<W: std::io::Write>(
    adapter: &dyn Adapter,
    r: &SessionRef,
    out: &mut W,
    header: impl Fn(&HeaderInfo) -> String,
    render_msg: impl Fn(&Message) -> Result<String>,
    opts: RenderOpts,
    mode: Mode,
) -> Result<RenderOutcome> {
    use cv_core::{Demand, Flow, MessageSink};
    let checked = matches!(mode, Mode::CheckedMarkdown);
    anyhow::ensure!(
        !checked || (opts.range.is_none() && opts.max_bytes.is_none()),
        "checked Markdown export requires a complete render"
    );

    /// The header fields a `meta` call delivers.
    struct Meta {
        title: Option<String>,
        model: Option<String>,
        cwd: Option<PathBuf>,
        created_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    /// `--last n` in one pass: the last `n` messages, still lazy, plus what the render would have
    /// learned on the way — each `meta` with the message count it arrived after, and the first
    /// message model — so [`Sink::finish_tail`] replays the window exactly as a second pass that
    /// started at `total - n` would have.
    struct Tail {
        metas: Vec<(usize, Meta)>,
        first_model: Option<(usize, String)>,
    }

    struct Sink<'w, W, H, R> {
        frames: Frames<'w, W>,
        harness: Harness,
        id: String,
        cwd: Option<PathBuf>,
        title: Option<String>,
        model: Option<String>,
        created_at: Option<chrono::DateTime<chrono::Utc>>,
        first_prompt: Option<String>,
        header: H,
        render_msg: R,
        // Complete Markdown downloads hold the same header window in bounded private staging.
        checked_markdown: bool,
        checked_buf: Option<std::io::BufWriter<cv_core::bytes::ByteStage>>,
        resolver: cv_core::Resolver,
        // Windowed view: 0-based message index we're at, plus the [start, end) bounds. Messages
        // outside the window are never materialized — their on-disk content is never touched.
        window: cv_core::api::MessageWindow<Message>,
        // Byte budget: rendered bytes so far (header + messages) and where the cut landed.
        max_bytes: Option<usize>,
        bytes: usize,
        rendered: usize,
        next: Option<usize>,
        tail: Option<Tail>,
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> Result<String>> Sink<'_, W, H, R> {
        fn apply_meta(&mut self, s: Meta) {
            self.title = s.title;
            if self.model.is_none() {
                self.model = s.model;
            }
            if self.cwd.is_none() {
                self.cwd = s.cwd;
            }
            if self.created_at.is_none() {
                self.created_at = s.created_at;
            }
        }
        /// Render the kept tail as the second pass would have: everything before the window
        /// (metas, the first message model) applied in stream order, then the window's messages
        /// with each meta delivered before the message it preceded.
        fn finish_tail(&mut self) {
            let Some(t) = self.tail.take() else { return };
            let messages = self.window.finish();
            let start = self.window.start;
            self.window.idx = start;
            let mut metas = t.metas.into_iter().peekable();
            let mut pre_model = t.first_model.filter(|(i, _)| *i < start);
            while let Some((at, m)) = metas.next_if(|(at, _)| *at <= start) {
                if let Some((_, md)) = pre_model.take_if(|(i, _)| *i < at) {
                    self.model.get_or_insert(md);
                }
                self.apply_meta(m);
            }
            if let Some((_, md)) = pre_model {
                self.model.get_or_insert(md);
            }
            for (k, (_, m)) in messages.into_iter().enumerate() {
                while let Some((_, s)) = metas.next_if(|(at, _)| *at <= start + k) {
                    self.apply_meta(s);
                }
                if self.message(m) == Flow::Stop {
                    break;
                }
            }
            for (_, s) in metas {
                self.apply_meta(s);
            }
        }
        fn header_now(&self) -> String {
            (self.header)(&HeaderInfo {
                harness: self.harness,
                id: self.id.clone(),
                cwd: self.cwd.clone(),
                label: cv_core::label_from(self.title.as_deref(), self.first_prompt.as_deref()),
                model: self.model.clone(),
                created_at: self.created_at,
            })
        }
        fn flush_header(&mut self) {
            if self.frames.printed {
                return;
            }
            let head = self.header_now();
            self.bytes += head.len();
            self.frames.start(&head);
            if let Some(mut buffered) = self.checked_buf.take() {
                if self.frames.result.is_ok() {
                    self.frames.result = buffered.flush().and_then(|()| {
                        buffered
                            .into_inner()
                            .map_err(|e| e.into_error())?
                            .finish()?
                            .copy_to(self.frames.out)
                            .map(|_| ())
                    });
                }
            }
            self.frames.printed = true;
        }
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> Result<String>> MessageSink
        for Sink<'_, W, H, R>
    {
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.frames.take_error()
        }
        fn meta(&mut self, s: &Session) {
            // Authoritative *parsed* session metadata, delivered before the body by the bridge
            // `stream` and by adapters that call `sink.meta` (e.g. codex). The parsed title overrides
            // the discovery-time `SessionRef` title so the header matches a full parse — some
            // adapters' discovery title differs from the parsed one (codex's discovery title is the
            // first user record, which the parse skips). Model/cwd fill in if discovery lacked them.
            let meta = Meta {
                title: s.title.clone(),
                model: s.model.clone(),
                cwd: s.cwd.clone(),
                created_at: s.created_at,
            };
            match self.tail.as_mut() {
                Some(t) => t.metas.push((self.window.idx, meta)),
                None => self.apply_meta(meta),
            }
        }
        fn message(&mut self, m: Message) -> Flow {
            if let Some(t) = self.tail.as_mut() {
                if t.first_model.is_none() {
                    if let Some(md) = &m.model {
                        t.first_model = Some((self.window.idx, md.clone()));
                    }
                }
                self.window.push(m);
                return Flow::Continue;
            }
            if self.frames.result.is_err() || self.next.is_some() {
                return Flow::Stop;
            }
            let idx = self.window.idx;

            // Past the window's end: nothing left to render — stop early so we never read the
            // rest of the file (the whole point of a windowed show on a huge session).
            if let Some(end) = self.window.end {
                if idx >= end {
                    return Flow::Stop;
                }
            }

            // Model is a small already-parsed field — pick it up even from out-of-window messages
            // so the header stays accurate, without touching any content bytes.
            if self.model.is_none() {
                if let Some(md) = &m.model {
                    self.model = Some(md.clone());
                }
            }

            // Before the window: skip entirely. We do NOT materialize, so no span content is read.
            let Some((_, m)) = self.window.push(m) else {
                return Flow::Continue;
            };

            if self.checked_markdown {
                let result = (|| -> Result<()> {
                    if self.first_prompt.is_none() {
                        self.first_prompt = m.prompt_title(&self.resolver)?;
                    }
                    if self.frames.printed {
                        cv_core::render::write_md_message(&m, self.frames.out, &self.resolver)?;
                    } else {
                        cv_core::render::write_md_message(
                            &m,
                            self.checked_buf.as_mut().context("missing Markdown holdback")?,
                            &self.resolver,
                        )?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    self.frames.result = Err(std::io::Error::other(error));
                    return Flow::Stop;
                }
                self.rendered += 1;
                let title_known = self.title.is_some() || self.first_prompt.is_some();
                if !self.frames.printed && Frames::<W>::ready(self.model.is_some(), title_known, self.rendered) {
                    self.flush_header();
                }
                return if self.frames.result.is_err() {
                    Flow::Stop
                } else {
                    Flow::Continue
                };
            }

            // In-window: resolve this message's lazy content spans (peak = one message) and render.
            let mut m = m;
            if let Err(error) = m.try_materialize_text(&self.resolver) {
                self.frames.result = Err(std::io::Error::other(error));
                return Flow::Stop;
            }
            if self.first_prompt.is_none() {
                match m.prompt_title(&self.resolver) {
                    Ok(title) => self.first_prompt = title,
                    Err(error) => {
                        self.frames.result = Err(std::io::Error::other(error));
                        return Flow::Stop;
                    }
                }
            }
            let rendered = match (self.render_msg)(&m) {
                Ok(rendered) => rendered,
                Err(error) => {
                    self.frames.result = Err(std::io::Error::other(error));
                    return Flow::Stop;
                }
            };
            drop(m);
            // The byte budget: stop BEFORE the message that would overflow it — unless it is the
            // first in-window message, which always renders so a window never comes back empty.
            if let Some(budget) = self.max_bytes {
                if self.rendered > 0 && self.bytes + rendered.len() > budget {
                    self.next = Some(idx);
                    return Flow::Stop;
                }
            }
            self.bytes += rendered.len();
            self.rendered += 1;
            self.frames.push(rendered);
            let title_known = self.title.is_some() || self.first_prompt.is_some();
            if !self.frames.printed && Frames::<W>::ready(self.model.is_some(), title_known, self.rendered) {
                self.flush_header();
            }
            if self.frames.result.is_err() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    let (start, end, tail) = match opts.range.map_or(StreamWindow::Range(0, None), Window::streaming) {
        StreamWindow::Tail(n) => (0, None, Some(n)),
        StreamWindow::Range(start, end) => (start, end, None),
    };
    let mut sink = Sink {
        frames: Frames::new(out),
        harness: r.harness,
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        model: None,
        created_at: r.created_at,
        first_prompt: None,
        header,
        render_msg,
        checked_markdown: checked,
        checked_buf: checked.then(|| std::io::BufWriter::with_capacity(64 * 1024, cv_core::bytes::ByteStage::new())),
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        window: match tail {
            Some(n) => cv_core::api::MessageWindow::new(start, end).tail(n),
            None => cv_core::api::MessageWindow::new(start, end),
        },
        max_bytes: opts.max_bytes,
        bytes: 0,
        rendered: 0,
        next: None,
        tail: tail.map(|_| Tail {
            metas: Vec::new(),
            first_model: None,
        }),
    };
    // Windowed: lazy spans, so out-of-window giant fields arrive as 16-byte handles and only
    // in-window messages materialize (the sink already resolves per message). A full show reads
    // every byte regardless — spans would only add resolve overhead there (floor = C).
    let parse_opts = if checked || opts.range.is_some() {
        Demand::LEAN.with_spans()
    } else {
        Demand::LEAN
    };
    let pass = cv_core::stream::Pass {
        guard: checked,
        ..cv_core::stream::Pass::RECORD
    };
    let guard = cv_core::stream::stream_counted(adapter, r, &parse_opts, &mut sink, pass)?.guard;
    sink.finish_tail();
    sink.flush_header(); // short sessions (no assistant turn / < HOLDBACK msgs) flush here
    sink.frames.result?;
    if let Some(guard) = guard {
        guard.verify(r)?;
    }
    Ok(RenderOutcome {
        rendered: sink.rendered,
        next: sink.next,
    })
}

/// Header for `cv show` (mirrors the old eager header exactly).
pub(crate) fn show_header(h: &HeaderInfo) -> String {
    format!(
        "# {}\n{} · {} · {}{}\n\n",
        h.label,
        h.harness,
        h.id,
        h.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into()),
        h.model.as_ref().map(|m| format!(" · {m}")).unwrap_or_default(),
    )
}

/// One rendered `cv show` message block (the String form of the old `print_message`).
pub(crate) fn show_message(m: &Message) -> Result<String> {
    let mut s = format!("── {} ──\n", turn_label(m));
    for b in &m.content {
        match b {
            Block::Text { text } => {
                s.push_str(text);
                s.push('\n');
            }
            Block::Thinking { text, .. } => s.push_str(&format!("[thinking] {}\n", truncate(text, 200))),
            Block::ToolUse { id, name, input, .. } => s.push_str(&format!(
                "[tool_use {name} {id}] {}\n",
                input.with_view(|view| cv_core::projection::Head::of_json(view, 200))?
            )),
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                details,
                ..
            } => {
                s.push_str(&format!(
                    "[tool_result{} {tool_use_id}] {}\n",
                    if *is_error { " error" } else { "" },
                    truncate(content, 200)
                ));
                if let Some(p) = persisted_path(details.as_ref()) {
                    s.push_str(&format!(
                        "  ↳ full output on disk: {p} (cv cat <session> {tool_use_id})\n"
                    ));
                }
            }
            Block::File { path, source, .. } => s.push_str(&format!(
                "[file: {}]\n",
                path.as_deref().or(source.as_deref()).unwrap_or("?")
            )),
            Block::Image { .. } => s.push_str("[image]\n"),
        }
    }
    s.push('\n');
    Ok(s)
}

/// Where a persisted (too-large) tool output lives on disk, when the adapter recorded it — the
/// transcript holds only the stub the model saw.
fn persisted_path(details: Option<&serde_json::Value>) -> Option<&str> {
    details?.pointer("/persistedOutput/path")?.as_str()
}

/// Header for `cv export md`: [`cv_core::render::md_header`] from the streamed header facts.
fn md_header(h: &HeaderInfo) -> String {
    cv_core::render::md_header(
        &h.label,
        h.harness,
        &h.id,
        h.cwd.as_deref(),
        h.model.as_deref(),
        h.created_at,
    )
}

/// One `cv export md` section: [`cv_core::render::write_md_message`] as a `String`.
fn md_message(m: &Message, resolver: &cv_core::Resolver) -> Result<String> {
    let mut out = Vec::new();
    cv_core::render::write_md_message(m, &mut out, resolver)?;
    Ok(String::from_utf8(out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::Role;

    #[test]
    fn fleetwide_agent_selection_honors_harness_and_zero_compaction_is_usage_error() {
        let parents = vec![
            SessionRef::new("claude-parent", Harness::Claude, "unused"),
            SessionRef::new("codex-parent", Harness::Codex, "unused"),
        ];
        assert_eq!(agent_parents_for_harness(parents.clone(), None).len(), 2);
        let selected = agent_parents_for_harness(parents, Some(Harness::Codex));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "codex-parent");
        let adapter = cv_core::harness::for_harness(Harness::Codex).unwrap();
        let error = show_range(&selected[0], adapter.as_ref(), &WindowArgs::default(), Some(0)).unwrap_err();
        assert!(error.downcast_ref::<crate::util::UsageError>().is_some());
    }

    #[test]
    fn lazy_tree_rows_preserve_owned_previews_and_first_prompt_label() -> Result<()> {
        let world = crate::test_world("tree-row-projection");
        let text = "🌙\\\"\n".repeat(30000);
        let path = world.write_session("tree-preview", &[
            serde_json::json!({"type":"user","uuid":"empty","message":{"role":"user","content":""}}),
            serde_json::json!({"type":"user","uuid":"blank","message":{"role":"user","content":" \n\t"}}),
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":text}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"role":"assistant","content":[{"type":"text","text":text},{"type":"tool_use","id":"call","name":"Agent","input":{"prompt":text}}]}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":text}]}}),
        ]);
        let r = SessionRef::new("tree-preview", Harness::Claude, path);
        let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
        let owned = adapter.parse(&r)?;
        let (session, rows, first_prompt) = read_tree(&r, adapter.as_ref())?;
        assert_eq!(rows.len(), owned.messages.len());
        assert_eq!(
            cv_core::label_from(session.title.as_deref(), first_prompt.as_deref()),
            owned.label()
        );
        for (row, message) in rows.iter().zip(&owned.messages) {
            assert_eq!(row.id, message.id);
            assert_eq!(row.parent, message.parent_id);
            assert_eq!(
                row.line,
                tree_line_with_preview(message, message.text().map(|text| truncate(&text, 80)))
            );
            assert!(row.line.len() < 512);
        }
        Ok(())
    }

    #[test]
    fn iterative_tree_walk_preserves_roots_and_covers_cycles_and_deep_chains() {
        let row = |id: &str, parent: Option<&str>| TreeRow {
            id: Some(id.into()),
            parent: parent.map(str::to_owned),
            line: String::new(),
        };
        let rows = vec![
            row("root", None),
            row("a", Some("root")),
            row("b", Some("root")),
            row("c", Some("a")),
            row("self", Some("self")),
            row("missing", Some("absent")),
            row("cycle1", Some("cycle2")),
            row("cycle2", Some("cycle1")),
            row("tail", Some("cycle2")),
        ];
        let mut order = Vec::new();
        walk_tree(&rows, |index, depth| order.push((index, depth)));
        // `root` forks (a, b sit one deeper); `a`'s only child keeps its depth; `cycle2` forks
        // into `cycle1` (already visited) and `tail`.
        assert_eq!(
            order,
            [(0, 0), (1, 1), (3, 1), (2, 1), (4, 0), (5, 0), (6, 0), (7, 0), (8, 1)]
        );
        let rows = (0..5000)
            .map(|i| TreeRow {
                id: Some(i.to_string()),
                parent: (i > 0).then(|| (i - 1).to_string()),
                line: String::new(),
            })
            .collect::<Vec<_>>();
        let mut count = 0;
        // A straight chain is flat: no row is indented, so the output is linear in the chain.
        walk_tree(&rows, |index, depth| {
            assert_eq!(index, count);
            assert_eq!(depth, 0);
            count += 1;
        });
        assert_eq!(count, rows.len());
    }

    #[test]
    fn checked_markdown_download_keeps_lazy_leaves_and_native_holdback_output() -> Result<()> {
        let world = crate::test_world("download-markdown-holdback");
        let text = " \n🌙\\\"".repeat(16384);
        let mut records = Vec::new();
        for i in 0..25 {
            records.push(
                serde_json::json!({"type":"user", "uuid":format!("u{i}"), "message":{"role":"user", "content":text}}),
            );
        }
        records.push(serde_json::json!({"type":"assistant", "uuid":"a", "message":{"role":"assistant", "model":"late-model", "content":[{"type":"thinking", "thinking":text}, {"type":"text", "text":"final"}]}}));
        let path = world.write_session("holdback", &records);
        let reference = SessionRef::new("holdback", Harness::Claude, path);
        let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
        let mut native = Vec::new();
        stream_session_render(
            adapter.as_ref(),
            &reference,
            &mut native,
            md_header,
            |m| md_message(m, &cv_core::Resolver::new(Some(reference.path.clone()))),
            RenderOpts::default(),
        )?;
        struct Lazy;
        impl Adapter for Lazy {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                assert!(opts.spans, "complete Markdown download eagerly decoded leaves");
                cv_core::harness::for_harness(Harness::Claude)
                    .unwrap()
                    .try_stream(r, opts, sink)
            }
        }
        let (body, bytes) = stage_export(&Lazy, &reference, ExportFormat::Markdown)?;
        let mut checked = Vec::new();
        body.copy_to(&mut checked)?;
        assert_eq!(checked, native);
        assert_eq!(bytes, checked.len() as u64);
        Ok(())
    }

    #[test]
    fn staged_full_download_rejects_changed_dynamic_dependencies() -> Result<()> {
        let world = crate::test_world("download-source-guard");
        let path = world.write_session(
            "download-guard",
            &[serde_json::json!({"type":"user", "uuid":"u", "message":{"role":"user", "content":"complete"}})],
        );
        let auxiliary = world.base.join("auxiliary");
        struct ChangesDependency(PathBuf);
        impl Adapter for ChangesDependency {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                sink.dependency(&self.0);
                let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
                let session = adapter.try_stream(r, opts, sink)?;
                std::fs::write(&self.0, "changed after demanded read")?;
                Ok(session)
            }
        }
        let r = SessionRef::new("download-guard", Harness::Claude, path);
        for format in [ExportFormat::Markdown, ExportFormat::OpenSession] {
            std::fs::write(&auxiliary, "before")?;
            let mut published = Vec::new();
            let result = stage_export(&ChangesDependency(auxiliary.clone()), &r, format).and_then(|(body, _)| {
                body.copy_to(&mut published)?;
                Ok(())
            });
            let error = result.expect_err("changed export was published");
            assert!(error.to_string().contains("source changed"), "{error:#}");
            assert!(published.is_empty(), "failed source guard published output");
        }
        Ok(())
    }

    #[test]
    fn selected_extra_demand_preserves_owned_windows_acceptance_and_eof() -> Result<()> {
        let world = crate::test_world("window-extra-demand");
        let path = world.base.join("source.jsonl");
        let huge = "escaped λ\n\"".repeat(10000);
        let rows = [
            serde_json::json!({"type":"user","uuid":"u","message":{"content":"first"}}),
            serde_json::json!({"type":"assistant","uuid":"a","message":{"model":"model","id":"response","content":[{"type":"tool_use","id":"call","name":"Task","input":{"description":"task"}}]}}),
            serde_json::json!({"type":"user","uuid":"r","toolUseResult":{"agentId":"agent","status":"finished","stdout":huge,"restorable":["file.rs"]},"message":{"content":[{"type":"tool_result","tool_use_id":"call","content":"tiny result"}]}}),
            serde_json::json!({"type":"user","uuid":"last","gitBranch":"late-branch","message":{"content":"last"}}),
            serde_json::json!({"type":"custom-title","customTitle":"EOF title"}),
        ];
        let mut wire = rows.iter().map(|row| format!("{row}\n")).collect::<String>();
        // Full-extra Value acceptance drops this row. A lean/Facts parse alone would accept it
        // and shift every following message index, so it must remain validated even unselected.
        wire.insert_str(
            wire.find(&rows[3].to_string()).unwrap(),
            r#"{"type":"user","uuid":"bad","toolUseResult":{"ignored":"\ud800"},"message":{"content":"bad"}}
"#,
        );
        std::fs::write(&path, wire)?;
        let r = SessionRef::new("source", Harness::Claude, &path);
        let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
        struct FactsCollect(cv_core::stream::CollectSink);
        impl cv_core::MessageSink for FactsCollect {
            fn tier(&self) -> cv_core::stream::Tier {
                cv_core::stream::Tier::Facts
            }
            fn message(&mut self, m: Message) -> cv_core::Flow {
                self.0.message(m)
            }
        }
        let mut rich = cv_core::stream::CollectSink::default();
        let expected_counts = cv_core::stream::stream_counted(
            adapter.as_ref(),
            &r,
            &cv_core::Demand::RICH.with_spans(),
            &mut rich,
            Default::default(),
        )?
        .counted;
        let mut facts = FactsCollect(Default::default());
        let actual_counts = cv_core::stream::stream_counted(
            adapter.as_ref(),
            &r,
            &cv_core::Demand::RICH.with_spans(),
            &mut facts,
            Default::default(),
        )?
        .counted;
        assert_eq!(rich.messages.len(), facts.0.messages.len());
        let expected_counts = expected_counts.unwrap();
        let actual_counts = actual_counts.unwrap();
        assert_eq!(actual_counts.turns, expected_counts.turns);
        assert_eq!(actual_counts.counts, expected_counts.counts);
        assert_eq!(
            actual_counts.counts.get(&cv_core::affordance::Kind::Checkpoint),
            Some(&1)
        );
        let mut complete = FactsCollect(Default::default());
        let mut complete_full = cv_core::stream::CollectSink::default();
        adapter.try_stream(&r, &cv_core::Demand::COMPLETE, &mut complete)?;
        adapter.try_stream(&r, &cv_core::Demand::COMPLETE, &mut complete_full)?;
        assert_eq!(
            serde_json::to_value(&complete.0.messages)?,
            serde_json::to_value(&complete_full.messages)?
        );
        for (window, budget) in [
            (Some(Window::First(1)), None),
            (Some(Window::Range(2, Some(3))), None),
            (Some(Window::Range(3, Some(4))), None),
            (Some(Window::Last(1)), None),
            (Some(Window::First(0)), None),
            (None, Some(1)),
        ] {
            let (expected, next) = full_json_window(adapter.as_ref(), &r, window, budget);
            let expected = serde_json::to_vec_pretty(&expected)?;
            let (actual, actual_next) = staged_session_json(adapter.as_ref(), &r, window, budget, false)?;
            let mut bytes = Vec::new();
            actual.copy_to(&mut bytes)?;
            bytes.pop(); // CLI's trailing newline is outside the Session JSON oracle.
            assert_eq!(bytes, expected, "{window:?}/{budget:?}");
            assert_eq!(actual_next, next);
        }
        Ok(())
    }

    #[test]
    fn cursor_rich_lazy_windows_preserve_owned_fields_acceptance_counts_and_eof() -> Result<()> {
        let world = crate::test_world("cursor-rich-window");
        let path = world.base.join("state.vscdb");
        let root = serde_json::json!({"name":"composer title","createdAt":1,"lastUpdatedAt":2,"unifiedMode":"agent",
            "subagentInfo":{"parentComposerId":"parent","toolCallId":"spawn","subagentTypeName":"researcher"},
            "modelConfig":{"modelName":"fallback"},"fullConversationHeadersOnly":[{"bubbleId":"u"},{"bubbleId":"a"},{"bubbleId":"bad"},{"bubbleId":"missing"},{"bubbleId":"last"}]});
        let user = serde_json::json!({"bubbleId":"u","type":1,"text":"first","unifiedMode":99});
        let assistant = serde_json::json!({"bubbleId":"a","type":2,"thinking":{"text":"thought","signature":"signed"},
            "text":"","richText":serde_json::json!({"root":{"children":[{"text":"paragraph","children":[{"text":"body"}]}]}}).to_string(),
            "modelInfo":{"modelName":"assistant-model"},"tokenCount":{"inputTokens":5,"outputTokens":3},"unifiedMode":1,
            "toolFormerData":{"name":"run_terminal_cmd","toolCallId":"call","rawArgs":r#"{"command":"true"}"#,"status":"error",
                              "result":{"restorable":["file.rs"],"future":"escaped λ\n\"".repeat(10000)},"restorable":["ignored-not-details"]}});
        let last = serde_json::json!({"bubbleId":"last","type":2,"text":"last","modelInfo":{"modelName":"last-model"},"unifiedMode":5});
        let db = rusqlite::Connection::open(&path)?;
        db.execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")?;
        for (key, value) in [
            ("composerData:window-extra", root.to_string()),
            ("bubbleId:window-extra:u", user.to_string()),
            ("bubbleId:window-extra:a", assistant.to_string()),
            ("bubbleId:window-extra:last", last.to_string()),
            (
                "bubbleId:window-extra:bad",
                r#"{"type":2,"text":"ignored","providerOptions":{"future":"\ud800"}}"#.into(),
            ),
        ] {
            db.execute("INSERT INTO cursorDiskKV VALUES(?1,?2)", rusqlite::params![key, value])?;
        }
        drop(db);
        let r = SessionRef::new("window-extra", Harness::Cursor, &path);
        let adapter = cv_core::harness::for_harness(Harness::Cursor).unwrap();
        let mut rich = cv_core::stream::CollectSink::default();
        let mut lazy = cv_core::stream::CollectSink::default();
        let old = cv_core::stream::stream_counted(
            adapter.as_ref(),
            &r,
            &cv_core::Demand::RICH,
            &mut rich,
            Default::default(),
        )?
        .counted;
        let new = cv_core::stream::stream_counted(
            adapter.as_ref(),
            &r,
            &cv_core::Demand::RICH.with_spans(),
            &mut lazy,
            Default::default(),
        )?
        .counted;
        let (old, new) = (old.unwrap(), new.unwrap());
        assert_eq!(old.turns, new.turns);
        assert_eq!(old.counts, new.counts);
        assert_eq!(
            new.counts.get(&cv_core::affordance::Kind::Checkpoint),
            None,
            "Cursor does not emit restorable details from toolFormerData/result objects"
        );
        for (window, budget) in [
            (Some(Window::First(1)), None),
            (Some(Window::Range(1, Some(2))), None),
            (Some(Window::Range(2, Some(3))), None),
            (Some(Window::Last(1)), None),
            (Some(Window::First(0)), None),
            (None, Some(1)),
        ] {
            let (expected, next) = full_json_window(adapter.as_ref(), &r, window, budget);
            let expected = serde_json::to_vec_pretty(&expected)?;
            let (actual, actual_next) = staged_session_json(adapter.as_ref(), &r, window, budget, false)?;
            let mut bytes = Vec::new();
            actual.copy_to(&mut bytes)?;
            bytes.pop();
            assert_eq!(bytes, expected, "{window:?}/{budget:?}");
            assert_eq!(actual_next, next);
        }
        Ok(())
    }

    /// Actual command boundary; fixture/catalog preparation and the independent owned oracle
    /// are outside the allocation interval. Run alone in an explicitly isolated scratch home.
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated whole-command allocation control; requires explicit scratch inputs"]
    fn whole_command_window_extra_control() -> Result<()> {
        let path = PathBuf::from(std::env::var("CV_WINDOW_PROBE_SOURCE")?);
        let range = std::env::var("CV_WINDOW_PROBE_RANGE").unwrap_or_else(|_| "0..1".into());
        let harness = std::env::var("CV_WINDOW_PROBE_HARNESS").unwrap_or_else(|_| "claude".into());
        let harness = Harness::parse(&harness).context("unknown window probe harness")?;
        let reference = SessionRef::new("window-extra", harness, &path);
        cv_core::catalog::sync(std::slice::from_ref(&reference));
        let budget = std::env::var("CV_WINDOW_PROBE_BUDGET")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        let args = WindowArgs {
            first: None,
            last: None,
            range: budget.is_none().then_some(range),
            around: None,
            context: 5,
            max_bytes: budget,
        };
        if std::env::var("CV_WINDOW_PROBE_ORACLE").as_deref() == Ok("1") {
            let adapter = cv_core::harness::for_harness(harness).unwrap();
            let (session, next) = full_json_window(adapter.as_ref(), &reference, args.window()?, budget);
            let _ = next;
            println!("{}", serde_json::to_string_pretty(&session)?);
            return Ok(());
        }
        let before = cv_testkit::heap::begin();
        let result = cmd_show(
            "window-extra",
            Some(harness.as_str().into()),
            true,
            &args,
            ShowView {
                subagents: false,
                agent: None,
                pre_compaction: None,
                tree: false,
                kind: None,
                summaries: false,
            },
        );
        let heap = cv_testkit::heap::end(before);
        eprintln!("CV_WINDOW_HEAP {}", serde_json::to_string(&heap)?);
        result
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated allocator measurement; run alone with --test-threads=1"]
    fn staged_json_selected_payload_heap() {
        let world = crate::test_world("staged-json-heap");
        let path = world.base.join("arguments.json");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        file.write_all(b"{\"argument\":\"").unwrap();
        let piece = b"escaped\\n\\t\\\"\\\\";
        for _ in 0..1_000_000 {
            file.write_all(piece).unwrap();
        }
        file.write_all(b"\"}").unwrap();
        file.flush().unwrap();
        drop(file);
        struct Replay {
            path: PathBuf,
            len: u64,
        }
        impl Adapter for Replay {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                vec![]
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                let mut session = Session::new(&r.id, r.harness);
                sink.meta(&session);
                for _ in 0..4 {
                    let mut message = Message::new(Role::Assistant);
                    message.content.push(Block::ToolUse {
                        id: "call".into(),
                        name: "tool".into(),
                        namespace: None,
                        input: cv_core::Json::source(&self.path, 0, self.len)?,
                    });
                    sink.message(message);
                }
                session.title = Some("late metadata".into());
                Ok(session)
            }
        }
        let adapter = Replay {
            len: std::fs::metadata(&path).unwrap().len(),
            path: path.clone(),
        };
        let r = SessionRef::new("heap", Harness::Codex, &path);
        let baseline = cv_testkit::heap::begin();
        let (output, _) = staged_session_json(&adapter, &r, None, Some(usize::MAX), false).unwrap();
        let emitted = output.copy_to(&mut std::io::sink()).unwrap();
        let streamed = cv_testkit::heap::end(baseline);
        drop(output);
        let baseline = cv_testkit::heap::begin();
        let (session, _) = session_json_window(&adapter, &r, None, Some(usize::MAX)).unwrap();
        let output = serde_json::to_string_pretty(&session).unwrap();
        let materialized = cv_testkit::heap::end(baseline);
        eprintln!(
            "staged={} materialized={} output={}",
            streamed.peak_above_baseline_bytes,
            materialized.peak_above_baseline_bytes,
            output.len()
        );
        assert_eq!(emitted, output.len() as u64 + 1);
        assert!(streamed.peak_above_baseline_bytes < 8 * 1024 * 1024);
        assert!(materialized.peak_above_baseline_bytes > 64 * 1024 * 1024);
    }

    #[test]
    fn rendered_source_text_matches_owned_and_returns_source_error_before_rendering() {
        let world = crate::test_world("render-checked-source-text");
        let path = world.base.join("text.json");
        let body = "actual🌙\n".repeat(1024);
        let wire = serde_json::to_vec(&body).unwrap();
        std::fs::write(&path, &wire).unwrap();
        let text = cv_core::Json::source(&path, 0, wire.len() as u64)
            .unwrap()
            .with_view(|view| view.source_text())
            .unwrap()
            .unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Text { text });
        let mut session = Session::new("render-source", Harness::Codex);
        session.messages.push(message);
        struct Replay(Session);
        impl Adapter for Replay {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                _: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                cv_core::stream::replay(self.0.clone(), sink)
            }
        }
        let r = SessionRef::new("render-source", Harness::Codex, &path);
        let mut expected = session.clone();
        expected.materialize().unwrap();
        let expected = show_message(&expected.messages[0]).unwrap();
        let adapter = Replay(session);
        let mut output = Vec::new();
        stream_session_render(
            &adapter,
            &r,
            &mut output,
            |_| String::new(),
            show_message,
            RenderOpts {
                range: None,
                max_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(output, expected.as_bytes());
        std::fs::remove_file(path).unwrap();
        let mut failed_output = Vec::new();
        let result = stream_session_render(
            &adapter,
            &r,
            &mut failed_output,
            |_| String::new(),
            show_message,
            RenderOpts {
                range: None,
                max_bytes: None,
            },
        );
        assert!(result.is_err());
        assert!(failed_output.is_empty());
    }

    #[test]
    fn source_show_preview_matches_owned_unicode_and_fails_without_continuation() {
        let world = crate::test_world("show-json-boundary");
        let path = world.base.join("arguments.json");
        let value = serde_json::json!({"argument": "月🌙é\n\"\\".repeat(100_000), "tail": "HIDDEN_TAIL"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let message = |input| {
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::ToolUse {
                id: "call".into(),
                name: "custom".into(),
                input,
                namespace: None,
            });
            message
        };
        let expected = show_message(&message(value.into())).unwrap();
        assert!(expected.len() < 1024, "show retains a bounded preview");
        assert!(!expected.contains("HIDDEN_TAIL"));
        struct Replay(Session);
        impl Adapter for Replay {
            fn harness(&self) -> Harness {
                Harness::OpenSession
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                _: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                cv_core::stream::replay(self.0.clone(), sink)
            }
        }
        for deleted in [false, true] {
            std::fs::write(&path, &bytes).unwrap();
            let source_message = message(cv_core::Json::source(&path, 0, bytes.len() as u64).unwrap());
            assert_eq!(show_message(&source_message).unwrap(), expected);
            let mut session = Session::new("source-preview", Harness::OpenSession);
            session.messages.push(source_message.clone());
            let adapter = Replay(session);
            let r = SessionRef::new("source-preview", Harness::OpenSession, world.base.join("primary.json"));
            if !deleted {
                for document in [false, true] {
                    let (staged, next) = staged_session_json(&adapter, &r, None, Some(1), document).unwrap();
                    let (materialized, expected_next) = full_json_window(&adapter, &r, None, Some(1));
                    assert_eq!(next, expected_next);
                    let mut actual = Vec::new();
                    staged.copy_to(&mut actual).unwrap();
                    let mut expected = if document {
                        serde_json::to_vec_pretty(&cv_core::harness::opensession::document(&materialized)).unwrap()
                    } else {
                        serde_json::to_vec_pretty(&materialized).unwrap()
                    };
                    expected.push(b'\n');
                    assert_eq!(actual, expected);
                }
            }

            let mut output = Vec::new();
            let outcome = stream_session_render(
                &adapter,
                &r,
                &mut output,
                |_| String::new(),
                show_message,
                RenderOpts {
                    range: Some(Window::Last(1)),
                    max_bytes: Some(1),
                },
            )
            .unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), expected);
            assert_eq!(outcome.next, None);
            if deleted {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"{} ").unwrap();
            }
            assert!(show_message(&source_message).is_err());
            for window in [None, Some(Window::Last(1))] {
                assert!(staged_session_json(&adapter, &r, window, Some(1), false).is_err());
            }
            // The shared counted stream may demand source fields even outside the output window.
            assert_eq!(
                staged_session_json(&adapter, &r, Some(Window::First(0)), None, false).is_err(),
                session_json_window(&adapter, &r, Some(Window::First(0)), None).is_err()
            );

            for range in [None, Some(Window::Last(1))] {
                let mut output = Vec::new();
                assert!(
                    stream_session_render(
                        &adapter,
                        &r,
                        &mut output,
                        |_| String::new(),
                        show_message,
                        RenderOpts {
                            range,
                            max_bytes: Some(1)
                        }
                    )
                    .is_err(),
                    "projection failure must not return a successful continuation"
                );
            }
        }
    }

    /// Independent oracle: the original full parse, then slice, then apply the byte budget.
    fn full_json_window(
        adapter: &dyn Adapter,
        r: &SessionRef,
        window: Option<Window>,
        budget: Option<usize>,
    ) -> (Session, Option<usize>) {
        let mut session = adapter.parse(r).unwrap();
        let range = window.map(|w| w.bounds(|| Ok(session.messages.len())).unwrap());
        let start = range.map(|(start, _)| start).unwrap_or(0);
        if let Some(range) = range {
            // A past-the-end window is empty, never a panic.
            let end = range.1.unwrap_or(session.messages.len()).min(session.messages.len());
            let start = range.0.min(end);
            session.messages = session.messages.drain(start..end).collect();
        }
        let mut next = None;
        if let Some(budget) = budget {
            let mut bytes = 0;
            let mut keep = 0;
            for m in &session.messages {
                let n = serde_json::to_vec(m).unwrap().len();
                if keep > 0 && bytes + n > budget {
                    break;
                }
                bytes += n;
                keep += 1;
            }
            if keep < session.messages.len() {
                next = Some(start + keep);
                session.messages.truncate(keep);
            }
        }
        (session, next)
    }

    fn assert_json_windows(adapter: &dyn Adapter, r: &SessionRef) {
        for window in [
            None,
            Some(Window::First(0)),
            Some(Window::First(1)),
            Some(Window::Last(1)),
            Some(Window::Last(0)),
            Some(Window::Last(3)),
            Some(Window::Last(100)),
            Some(Window::Range(2, Some(4))),
            Some(Window::Range(100, Some(101))),
            Some(Window::Range(3, None)),
            Some(Window::Around { center: 1, context: 2 }),
            Some(Window::Around {
                center: 100,
                context: 2,
            }),
        ] {
            for budget in [None, Some(0), Some(1024), Some(1 << 20)] {
                let (expected, expected_next) = full_json_window(adapter, r, window, budget);
                for document in [false, true] {
                    let (staged, next) = staged_session_json(adapter, r, window, budget, document).unwrap();
                    let mut bytes = Vec::new();
                    staged.copy_to(&mut bytes).unwrap();
                    let mut oracle = if document {
                        serde_json::to_vec_pretty(&cv_core::harness::opensession::document(&expected)).unwrap()
                    } else {
                        serde_json::to_vec_pretty(&expected).unwrap()
                    };
                    oracle.push(b'\n');
                    assert_eq!(next, expected_next, "staged {window:?}, {budget:?}");
                    assert_eq!(bytes, oracle, "staged {window:?}, {budget:?}, document={document}");
                }
                let (actual, next) = session_json_window(adapter, r, window, budget).unwrap();
                assert_eq!(next, expected_next, "{window:?}, {budget:?}");
                assert_eq!(
                    serde_json::to_string_pretty(&actual).unwrap(),
                    serde_json::to_string_pretty(&expected).unwrap(),
                    "{window:?}, {budget:?}"
                );
                assert_eq!(
                    serde_json::to_string_pretty(&cv_core::harness::opensession::document(&actual)).unwrap(),
                    serde_json::to_string_pretty(&cv_core::harness::opensession::document(&expected)).unwrap()
                );
            }
        }
    }

    #[test]
    fn json_windows_match_full_parse_with_late_metadata_and_extras() {
        let world = crate::test_world("json_windows_match_full_parse_with_late_metadata_and_extras");
        let home = world.base.clone();
        let r = claude_fixture(&home);
        let mut f = std::fs::OpenOptions::new().append(true).open(&r.path).unwrap();
        for record in [
            serde_json::json!({"type":"attachment", "attachment":{"type":"prompt_snapshot",
                "systemPrompt":["late system prompt"]}}),
            serde_json::json!({"type":"user", "uuid":"result", "toolUseResult":{"persistedOutput":{"path":"/tmp/output"},
                "details":{"command":"printf \"🌙\"", "exitCode":1}},
                "message":{"role":"user", "content":[{"type":"tool_result","tool_use_id":"call",
                    "is_error":true,"content":"escaped\n\t\"\\🌙".repeat(1200)}]}}),
            serde_json::json!({"type":"user", "isCompactSummary":true,
                "message":{"role":"user","content":"complete summary\n\"🌙\"".repeat(1200)}}),
            serde_json::json!({"type":"ai-title", "aiTitle":"late title"}),
        ] {
            writeln!(f, "{record}").unwrap();
        }
        drop(f);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        assert_json_windows(adapter.as_ref(), &r);
        let (first, _) = session_json_window(adapter.as_ref(), &r, Some(Window::First(1)), None).unwrap();
        assert_eq!(first.title.as_deref(), Some("late title"));
        assert_eq!(first.system_prompt.as_deref(), Some("late system prompt"));
        assert_eq!(first.messages.len(), 1);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn staged_budget_rejects_giant_second_message_with_exact_continuation_and_eof_metadata() {
        let _world = crate::test_world("staged-budget-giant-reject");
        struct Replay;
        impl Adapter for Replay {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                let mut session = Session::new(&r.id, r.harness);
                sink.meta(&session);
                for body in ["tiny".to_owned(), "giant\n🌙".repeat(1024 * 1024)] {
                    let mut message = Message::new(Role::User);
                    message.content.push(Block::Text { text: body.into() });
                    assert_eq!(sink.message(message), cv_core::Flow::Continue);
                }
                session.title = Some("EOF title".into());
                Ok(session)
            }
        }
        let r = SessionRef::new("giant-reject", Harness::Codex, "/nonexistent/giant.jsonl");
        let (expected, expected_next) = full_json_window(&Replay, &r, None, Some(128));
        let (stage, next) = staged_session_json(&Replay, &r, None, Some(128), false).unwrap();
        assert_eq!(next, Some(1));
        assert_eq!(next, expected_next);
        let mut output = Vec::new();
        stage.copy_to(&mut output).unwrap();
        let mut oracle = serde_json::to_vec_pretty(&expected).unwrap();
        oracle.push(b'\n');
        assert_eq!(output, oracle);
    }

    #[test]
    fn json_window_discards_large_messages_but_reads_the_eof_shell() {
        let _world = crate::test_world("json_window_discards_large_messages_but_reads_the_eof_shell");
        struct Large;
        impl Adapter for Large {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                assert!(opts.tier >= cv_core::stream::Tier::Facts);
                let mut s = Session::new(r.id.clone(), r.harness);
                s.title = Some("early title".into());
                sink.meta(&s);
                for i in 0..32 {
                    let mut m = Message::new(Role::Assistant);
                    m.id = Some(format!("message-{i}"));
                    m.harness_extra_mut(r.harness)
                        .insert("full_detail".into(), serde_json::json!(i));
                    m.content.push(Block::Text {
                        text: "large discarded body".repeat(8192).into(),
                    });
                    assert_eq!(sink.message(m), cv_core::Flow::Continue);
                }
                s.title = Some("final title".into());
                s.lineage.forked_from = Some("late parent".into());
                s.system_prompt = Some("late prompt".into());
                sink.meta(&s);
                Ok(s)
            }
        }
        let r = SessionRef::new("json-large", Harness::Codex, "/nonexistent/json-large.jsonl");
        for window in [Window::First(1), Window::Last(1), Window::Range(15, Some(16))] {
            let (session, _) = session_json_window(&Large, &r, Some(window), None).unwrap();
            assert_eq!(session.messages.len(), 1);
            assert_eq!(session.title.as_deref(), Some("final title"));
            assert_eq!(session.lineage.forked_from.as_deref(), Some("late parent"));
            let (expected, _) = full_json_window(&Large, &r, Some(window), None);
            assert_eq!(
                serde_json::to_value(session).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn gemini_json_windows_preserve_lazy_content_and_replayed_metadata() {
        let world = crate::test_world("gemini_json_windows_preserve_lazy_content_and_replayed_metadata");
        let home = world.base.clone();
        std::fs::create_dir_all(home.join("chats")).unwrap();
        let adapter = cv_core::harness::gemini::Gemini::with_home(&home);
        let giant = "escaped\n\t\"\\🌙".repeat(1200);
        let messages = serde_json::json!([
            {"type":"user","id":"u","content":"small first prompt"},
            {"type":"gemini","id":"a","model":"gemini-test","content":[
                {"text":giant,"thought":true},{"text":giant}],
             "tokens":{"input":100,"cached":60,"output":20},
             "toolCalls":[{"id":"c","name":"shell","args":{"command":"ls"},"status":"success",
                 "result":[{"functionResponse":{"response":{"output":giant}}}]}]},
            {"type":"user","id":"t","content":[
                {"functionResponse":{"id":"c","name":"shell","response":{"output":giant}}}]},
            {"type":"warning","id":"w","content":giant},
            {"type":"user","id":"last","content":"small final prompt"}
        ]);
        let metadata = serde_json::json!({"sessionId":"json-window","projectHash":"x","kind":"subagent",
            "startTime":"2026-01-01T00:00:00Z","lastUpdated":"2026-01-01T00:00:10Z",
            "summary":"full summary\n\"🌙\"".repeat(1200),"directories":["/tmp/q\"uote"]});
        for ext in ["json", "jsonl"] {
            let path = home.join("chats").join(format!("session-window.{ext}"));
            if ext == "json" {
                let mut document = metadata.clone();
                document["messages"] = messages.clone();
                std::fs::write(&path, document.to_string()).unwrap();
            } else {
                let mut early = metadata.clone();
                early.as_object_mut().unwrap().remove("summary");
                let mut records = vec![early.to_string()];
                records.extend(messages.as_array().unwrap().iter().map(ToString::to_string));
                // EOF replay must supply the final shell even for an early, empty or budget-cut window.
                records.push(
                    serde_json::json!({"$set":{"summary":metadata["summary"],
                    "lastUpdated":"2026-01-01T00:00:20Z"}})
                    .to_string(),
                );
                std::fs::write(&path, records.join("\n")).unwrap();
            }
            let r = SessionRef::new("json-window", Harness::Gemini, path);
            assert_json_windows(&adapter, &r);
            let (first, _) = session_json_window(&adapter, &r, Some(Window::First(1)), None).unwrap();
            assert_eq!(first.title.as_deref(), metadata["summary"].as_str());
            assert_eq!(first.messages.len(), 1);
        }
    }

    /// `--kind` picks one view family: what the person said, the compaction seams, or the
    /// affordance rows (`affordances` meaning all of them); mixing families or an unknown word is a
    /// usage error naming every accepted word.
    #[test]
    fn kind_words_pick_one_view_family() {
        use cv_core::affordance::Kind;
        assert!(matches!(
            parse_kinds("prompt,answer"),
            Ok(KindView::Said {
                prompts: true,
                answers: true
            })
        ));
        assert!(matches!(parse_kinds("compaction_summary"), Ok(KindView::Compactions)));
        match parse_kinds("plan,schedule") {
            Ok(KindView::Affordances(k)) => assert_eq!(k, [Kind::Plan, Kind::Schedule].into_iter().collect()),
            _ => panic!("plan,schedule is the affordance view"),
        }
        match parse_kinds("affordances") {
            Ok(KindView::Affordances(k)) => assert_eq!(k.len(), Kind::ALL.len()),
            _ => panic!("affordances is every kind"),
        }
        let err = parse_kinds("plan,prompt").err().expect("mixed families").to_string();
        assert!(err.contains("mixes two views"), "{err}");
        let err = parse_kinds("todo").err().expect("unknown word").to_string();
        assert!(err.contains("background_job"), "{err}");
    }

    fn render(r: &SessionRef, range: Option<(usize, Option<usize>)>) -> String {
        let range = range.map(|(a, b)| Window::Range(a, b));
        render_with(r, RenderOpts { range, max_bytes: None }).0
    }

    fn render_with(r: &SessionRef, opts: RenderOpts) -> (String, RenderOutcome) {
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let mut out = Vec::new();
        let outcome = stream_session_render(adapter.as_ref(), r, &mut out, show_header, show_message, opts).unwrap();
        (String::from_utf8(out).unwrap(), outcome)
    }

    /// Render the window `range` of `r` and check its body is each message of a full eager parse,
    /// rendered alone and concatenated. Returns the windowed render's header.
    fn assert_window_matches_full_parse(r: &SessionRef, range: (usize, Option<usize>)) -> String {
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let mut all = cv_core::CollectSink::default();
        adapter.stream(r, &cv_core::Demand::LEAN, &mut all).unwrap();
        let end = range.1.unwrap_or(usize::MAX).min(all.messages.len());
        let want: String = all.messages[range.0..end]
            .iter()
            .map(|m| show_message(m).unwrap())
            .collect();
        let windowed = render(r, Some(range));
        let body_at = windowed.find("── ").unwrap_or(windowed.len());
        assert_eq!(
            &windowed[body_at..],
            want,
            "range {range:?} body must equal the full parse's slice"
        );
        windowed[..body_at].to_string()
    }

    /// A small Claude fixture: title, a user turn, a model-carrying assistant turn, one big user
    /// turn, then eight short follow-ups (11 messages).
    fn claude_fixture(home: &std::path::Path) -> SessionRef {
        let path = home.join("s.jsonl");
        let big = "long \"quoted\" content\n".repeat(400);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"ai-title","aiTitle":"render window"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"u0","cwd":"/w","message":{{"role":"user","content":"q one"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({"type":"assistant","uuid":"a1","message":{"role":"assistant",
                "model":"claude-test","content":[{"type":"text","text":"a one"}]}})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({"type":"user","uuid":"u2","message":{"role":"user","content":big}})
        )
        .unwrap();
        for i in 0..8 {
            writeln!(
                f,
                "{}",
                serde_json::json!({"type":"user","uuid":format!("u{}", 3+i),
                    "message":{"role":"user","content":format!("follow-up {i}")}})
            )
            .unwrap();
        }
        drop(f);
        SessionRef {
            id: "render-window".into(),
            harness: Harness::Claude,
            path,
            cwd: Some("/w".into()),
            title: Some("render window".into()),
            created_at: None,
            updated_at: None,
            message_count: Some(11),
        }
    }

    /// The windowed-render contract: a window renders byte-identically to the same messages of a
    /// full parse, and its header carries the model the skipped prefix provides.
    #[test]
    fn windowed_render_is_byte_identical_to_the_full_parse() {
        let world = crate::test_world("windowed_render_is_byte_identical_to_the_full_parse");
        let home = world.base.clone();
        let r = claude_fixture(&home);
        let full_header = {
            let full = render(&r, None);
            full[..full.find("── ").unwrap()].to_string()
        };
        for range in [(3usize, Some(6usize)), (2, Some(11)), (5, None), (1, Some(2))] {
            let header = assert_window_matches_full_parse(&r, range);
            assert_eq!(header, full_header, "range {range:?} header");
            assert!(header.contains("claude-test"), "header model expected in {range:?}");
        }
    }

    /// `--max-bytes`: the render stops before the message that would overflow the budget and
    /// reports the continuation index; the first in-window message always renders; a budget
    /// that fits everything reports no cut.
    #[test]
    fn byte_budget_cuts_and_reports_the_next_index() {
        let world = crate::test_world("byte_budget_cuts_and_reports_the_next_index");
        let home = world.base.clone();
        let r = claude_fixture(&home);

        // Message 2 is the ~9 KB giant: a 1 KB budget renders messages 0 and 1, then stops.
        let (out, oc) = render_with(
            &r,
            RenderOpts {
                range: None,
                max_bytes: Some(1024),
            },
        );
        assert_eq!(oc.rendered, 2, "{out}");
        assert_eq!(oc.next, Some(2), "{out}");
        assert!(
            out.contains("q one") && out.contains("a one") && !out.contains("follow-up"),
            "{out}"
        );

        // Starting AT the giant: it renders anyway (progress is always possible), then the cut.
        let (out, oc) = render_with(
            &r,
            RenderOpts {
                range: Some(Window::Range(2, None)),
                max_bytes: Some(1024),
            },
        );
        assert_eq!(oc.rendered, 1, "{out}");
        assert_eq!(oc.next, Some(3), "{out}");
        assert!(out.contains("long \"quoted\" content"), "{out}");

        // A generous budget: everything, no cut.
        let (_, oc) = render_with(
            &r,
            RenderOpts {
                range: Some(Window::Range(3, None)),
                max_bytes: Some(1 << 20),
            },
        );
        assert_eq!(oc.rendered, 8);
        assert_eq!(oc.next, None);
    }

    /// `--last n` keeps a ring of the last `n` messages in one pass; it must render exactly what the
    /// counted `total - n ..` range renders, budget cut and continuation index included.
    #[test]
    fn last_n_renders_what_the_counted_range_does() {
        let world = crate::test_world("last_n_renders_what_the_counted_range_does");
        let home = world.base.clone();
        let r = claude_fixture(&home);
        for n in [0usize, 1, 3, 10, 11, 50] {
            for max_bytes in [None, Some(1024)] {
                let start = 11usize.saturating_sub(n);
                let counted = render_with(
                    &r,
                    RenderOpts {
                        range: Some(Window::Range(start, None)),
                        max_bytes,
                    },
                );
                let ring = render_with(
                    &r,
                    RenderOpts {
                        range: Some(Window::Last(n)),
                        max_bytes,
                    },
                );
                assert_eq!(ring, counted, "--last {n} (budget {max_bytes:?})");
            }
        }
    }

    /// A store whose metadata arrives after the body (codex delivers `meta` at EOF when no model is
    /// ever known) and whose model first shows up past `HOLDBACK` messages: the ring replays each
    /// `meta` and the prefix's model where a second pass would have met them, so the header is the
    /// counted render's, byte for byte.
    #[test]
    fn last_n_replays_late_meta_and_models_in_stream_order() {
        struct Late(usize);
        impl Adapter for Late {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &cv_core::Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<Session> {
                let mut s = Session::new(r.id.clone(), Harness::Codex);
                for i in 0..self.0 {
                    if i == 12 {
                        s.title = Some("mid-stream title".into());
                        sink.meta(&s);
                    }
                    let mut m = Message::new(if i % 2 == 0 { Role::User } else { Role::Assistant });
                    if i == 30 || i == 45 {
                        m.model = Some(format!("model-{i}"));
                    }
                    m.content.push(Block::Text {
                        text: format!("turn {i}").into(),
                    });
                    if sink.message(m) == cv_core::Flow::Stop {
                        break;
                    }
                }
                s.title = Some("eof title".into());
                s.cwd = Some("/late".into());
                sink.meta(&s);
                Ok(s)
            }
        }
        let r = SessionRef {
            id: "late".into(),
            harness: Harness::Codex,
            path: "/nonexistent/late.jsonl".into(),
            cwd: None,
            title: Some("discovery title".into()),
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let total = 60usize;
        let a = Late(total);
        let render = |range: Window, max_bytes: Option<usize>| {
            let mut out = Vec::new();
            let oc = stream_session_render(
                &a,
                &r,
                &mut out,
                show_header,
                show_message,
                RenderOpts {
                    range: Some(range),
                    max_bytes,
                },
            )
            .unwrap();
            (String::from_utf8(out).unwrap(), oc)
        };
        for n in [0usize, 5, 20, 29, 30, 31, 40, 48, 60, 100] {
            for max_bytes in [None, Some(200)] {
                let counted = render(Window::Range(total.saturating_sub(n), None), max_bytes);
                assert_eq!(
                    render(Window::Last(n), max_bytes),
                    counted,
                    "--last {n} (budget {max_bytes:?})"
                );
            }
        }
    }

    /// One label spelling and one block text on every door: `cv show`, `cv export --format md`
    /// and `cv share` label a turn with `render::turn_label` and carry its text blocks verbatim.
    #[test]
    fn show_export_and_share_agree_on_labels_and_block_text() {
        let mut sys = Message::new(Role::System);
        sys.kind = MessageKind::InjectedContext;
        sys.harness_extra_mut(Harness::Claude)
            .insert("attachment_type".into(), serde_json::json!("hook_success"));
        sys.content.push(Block::Text {
            text: "reminder: run the gate".into(),
        });
        let mut summary = Message::new(Role::User);
        summary.kind = MessageKind::CompactionSummary;
        summary.content.push(Block::Text {
            text: "earlier we fixed the parser".into(),
        });
        let mut reply = Message::new(Role::Assistant);
        reply.content.push(Block::Text {
            text: "on it, editing ir.rs".into(),
        });
        for (m, label) in [
            (&sys, "system · injected_context/hook_success"),
            (&summary, "user · compaction_summary"),
            (&reply, "assistant"),
        ] {
            assert_eq!(turn_label(m), label);
            let text = m.text().unwrap();
            let shown = show_message(m).unwrap();
            assert!(
                shown.starts_with(&format!("── {label} ──\n")) && shown.contains(&text),
                "{shown}"
            );
            let md = md_message(m, &cv_core::Resolver::new(None)).unwrap();
            assert!(md.starts_with(&format!("## {label}\n\n")) && md.contains(&text), "{md}");
            let mut html = String::new();
            cv_core::html::share_message(m, 0, &mut html, &cv_core::Resolver::new(None)).unwrap();
            assert!(
                html.contains(&format!("<span class=\"role\">{label}</span>")) && html.contains(&text),
                "{html}"
            );
        }
        // A flat legacy key is not an attachment kind: only a nested harness bag counts.
        let mut legacy = Message::new(Role::System);
        legacy.kind = MessageKind::Error;
        legacy.extra.insert("subtype".into(), serde_json::json!("api_error"));
        assert_eq!(turn_label(&legacy), "system · error");
    }

    /// Same contract for codex, whose metadata arrives through `meta()` (the parse-time title —
    /// `None` — overrides the discovery title).
    #[test]
    fn codex_windowed_render_is_byte_identical_to_the_full_parse() {
        let world = crate::test_world("codex_windowed_render_is_byte_identical_to_the_full_parse");
        let home = world.base.clone();

        let path = home.join("rollout-r.jsonl");
        let big = "giant output\n\t\"\\🌙".repeat(1200);
        let mut f = std::fs::File::create(&path).unwrap();
        for l in [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"rollout-r","cwd":"/work","forked_from_id":"parent","subagent_history_start_ordinal":2,"agent_role":"reviewer"}}"#.to_string(),
            serde_json::json!({"ordinal":1,"type":"response_item","payload":{"type":"custom_tool_call_output",
                "call_id":"inherited","output":big}}).to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello there"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item",
                "payload":{"type":"function_call_output","call_id":"c1","name":"shell","namespace":"functions",
                    "internal_chat_message_metadata_passthrough":{"create_time":1767225604.5,"turn_id":"turn1"},
                    "output":format!("[error] {big}")}}).to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item",
                "payload":{"type":"custom_tool_call_output","call_id":"c2","name":"patch","namespace":"functions",
                    "output":big}}).to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:05Z","type":"compacted",
                "payload":{"message":big,"window_number":3,"window_id":"w","compaction_response_id":"cr",
                    "replacement_history":[{"role":"user","content":"preserved count"}]}}).to_string(),
            r#"{"timestamp":"2026-01-01T00:00:06Z","type":"event_msg","payload":{"type":"agent_message","message":"all done"}}"#.to_string(),
        ] {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        let r = SessionRef {
            id: "rollout-r".into(),
            harness: Harness::Codex,
            path: path.clone(),
            cwd: Some("/work".into()),
            title: Some("discovery title".into()),
            created_at: None,
            updated_at: None,
            message_count: Some(3),
        };
        for range in [(2usize, Some(4usize)), (1, None), (3, Some(5))] {
            let header = assert_window_matches_full_parse(&r, range);
            assert!(header.contains("gpt-test"), "header model expected in {range:?}");
            assert!(!header.contains("discovery title"), "parsed title wins in {range:?}");
        }

        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        assert_json_windows(adapter.as_ref(), &r);
    }
}
