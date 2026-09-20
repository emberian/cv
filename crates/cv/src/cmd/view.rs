//! `cv show` / `cv export` / `cv tree` / `cv diff` / `cv redact` — reading sessions,
//! plus the streaming renderer (`stream_session_render`) and its header/message helpers.

use crate::util::{
    clamp, continue_hint, count_messages, home_rel, parse_harness, resolve, resolve_found, short_id, split_harness_id,
    usage, WindowArgs,
};
use anyhow::{bail, Context, Result};
use cv_core::ir::{truncate, Block, Harness, Message, MessageKind, Role, Session, SessionRef};
use cv_core::Adapter;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// A piped `cv show` with no selector prints at most this much before switching to head + tail:
/// agents read cv through pipes constantly, and a full transcript in a tool result is the
/// context-window failure mode this exists to prevent.
const PIPE_GUARD_BYTES: usize = 200 * 1024;
/// How many messages each end of the head + tail view shows.
const PIPE_GUARD_EDGE: usize = 20;

pub(crate) fn cmd_show(
    id: &str,
    harness: Option<String>,
    json: bool,
    window: &WindowArgs,
    subagents: bool,
    agent: Option<String>,
    pre_compaction: Option<usize>,
) -> Result<()> {
    let (want, id) = split_harness_id(id, parse_harness(&harness)?);
    // Parse the selector before any I/O so a bad `--range` errors immediately.
    let selector = window.window()?;

    // An `agent-…` shaped id goes straight to the sub-agent view: cv_core::find_cheap can now
    // resolve these too (its fleet-wide fallback), but this path keeps the provenance banner and
    // lists candidates on an ambiguous prefix instead of erroring.
    if id.starts_with("agent-") && show_agent_fleetwide(id, json, window)? {
        return Ok(());
    }
    // find_cheap first: don't pay a full fleet re-discovery before trying the id as a sub-agent
    // id — sub-agents aren't in the main pool, but `cv show <agent-id>` should still just work
    // (harvest reports hand out bare agent ids all the time). Only when the id is neither a
    // cataloged session nor an agent does the full-scan `find` escalation run (a session in the
    // discovery probe's blind spots).
    let found = match cv_core::find_cheap(id, want) {
        Ok(Some(hit)) => Ok(Some(hit)),
        Ok(None) => {
            if show_agent_fleetwide(id, json, window)? {
                return Ok(());
            }
            cv_core::find(id, want)
        }
        Err(e) => Err(e),
    };
    let (r, adapter) = match found {
        Ok(Some(hit)) => hit,
        Ok(None) => return usage(format!("no session (and no sub-agent) matching {id:?}")),
        Err(e) => return resolve_found(Err(e), id, want).map(|_| ()),
    };

    // `--agent <id>`: render one specific sub-agent's transcript (resolved through this parent,
    // since sub-agents aren't in the main pool). `--subagents`: list the whole forest with results.
    if let Some(agent_id) = &agent {
        return show_one_subagent(&r, adapter.as_ref(), agent_id, json, window);
    }
    if subagents {
        return show_subagents(&r, json);
    }

    let mut range = window.bounds(|| count_messages(adapter.as_ref(), &r))?;

    // `--pre-compaction <N>`: resolve the Nth (1-based) compaction's pre-span into a window. This
    // reads the context the continued agent lost — the whole point is to retrieve it by message
    // range without the user computing offsets by hand.
    if let Some(n) = pre_compaction {
        let comps = cv_core::compaction::detect(&r, false)?;
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
        range = Some((start, Some(end)));
    }

    if json {
        return print_session_json(adapter.as_ref(), &r, range, window.max_bytes);
    }
    // The pipe guard applies only to a selector-less, budget-less render (an explicit window or
    // budget is the caller saying what they want).
    let guard = selector.is_none() && pre_compaction.is_none() && window.max_bytes.is_none();
    render_text(adapter.as_ref(), &r, range, window.max_bytes, guard)
}

/// `show --json` / `export --format json`: the whole IR (incl. `extra`), windowed, and — under
/// `--max-bytes` — cut at the budget with the continuation hint on **stderr** (stdout stays JSON).
fn print_session_json(
    adapter: &dyn Adapter,
    r: &SessionRef,
    range: Option<(usize, Option<usize>)>,
    max_bytes: Option<usize>,
) -> Result<()> {
    let mut session = adapter.parse(r)?;
    let start = range.map(|(s, _)| s).unwrap_or(0);
    if let Some(rg) = range {
        let (s, e) = clamp(rg, session.messages.len());
        session.messages = session.messages.drain(s..e).collect();
    }
    if let Some(budget) = max_bytes {
        let mut bytes = 0usize;
        let mut keep = 0usize;
        for (i, m) in session.messages.iter().enumerate() {
            let n = serde_json::to_vec(m)?.len();
            if keep > 0 && bytes + n > budget {
                break;
            }
            bytes += n;
            keep = i + 1;
        }
        if keep < session.messages.len() {
            eprintln!("{}", continue_hint(start + keep));
            session.messages.truncate(keep);
        }
    }
    println!("{}", serde_json::to_string_pretty(&session)?);
    Ok(())
}

/// The rendered-transcript path of `cv show`, with the pipe guard: a selector-less render to a
/// non-terminal stdout that would exceed [`PIPE_GUARD_BYTES`] becomes the first and last
/// [`PIPE_GUARD_EDGE`] messages with a hint line between them.
fn render_text(
    adapter: &dyn Adapter,
    r: &SessionRef,
    range: Option<(usize, Option<usize>)>,
    max_bytes: Option<usize>,
    guard: bool,
) -> Result<()> {
    let stdout = std::io::stdout();
    if guard && !stdout.is_terminal() {
        // Render into memory under the guard's budget; if it all fits, that IS the output.
        let mut buf = Vec::new();
        let probe = stream_session_render(
            adapter,
            r,
            &mut buf,
            show_header,
            show_message,
            RenderOpts {
                range: None,
                max_bytes: Some(PIPE_GUARD_BYTES),
            },
        )?;
        let mut out = std::io::BufWriter::new(stdout.lock());
        if probe.next.is_none() {
            out.write_all(&buf)?;
            out.flush()?;
            return Ok(());
        }
        let total = count_messages(adapter, r)?;
        if total <= 2 * PIPE_GUARD_EDGE {
            // Too few messages to elide any — the size is in the messages themselves.
            stream_session_render(adapter, r, &mut out, show_header, show_message, RenderOpts::default())?;
            out.flush()?;
            return Ok(());
        }
        stream_session_render(
            adapter,
            r,
            &mut out,
            show_header,
            show_message,
            RenderOpts {
                range: Some((0, Some(PIPE_GUARD_EDGE))),
                max_bytes: None,
            },
        )?;
        writeln!(
            out,
            "… {} messages omitted ({total} total; the full render exceeds {} KB on a pipe) — read them with \
             --first N, --last N, --range A..B, or --around N; --max-bytes N caps output\n",
            total - 2 * PIPE_GUARD_EDGE,
            PIPE_GUARD_BYTES / 1024,
        )?;
        stream_session_render(
            adapter,
            r,
            &mut out,
            |_| String::new(),
            show_message,
            RenderOpts {
                range: Some((total - PIPE_GUARD_EDGE, None)),
                max_bytes: None,
            },
        )?;
        out.flush()?;
        return Ok(());
    }

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

/// `cv show <agent-id>` with no parent given: find which session(s) spawned the agent (filename
/// scan across the fleet) and render it through the one parent — or list the candidates when the
/// prefix is ambiguous. Returns `false` when nothing agent-shaped matched (the caller has one
/// more reading of the id to try).
fn show_agent_fleetwide(agent_id: &str, json: bool, window: &WindowArgs) -> Result<bool> {
    let parents = cv_core::find_subagent_parents(agent_id);
    match parents.as_slice() {
        [] => Ok(false),
        [parent] => {
            let adapter = cv_core::harness::for_harness(parent.harness)
                .with_context(|| format!("no adapter for {}", parent.harness))?;
            eprintln!(
                "✦ sub-agent of session {} ({})",
                short_id(&parent.id),
                parent.title.as_deref().unwrap_or("untitled"),
            );
            show_one_subagent(parent, adapter.as_ref(), agent_id, json, window)?;
            Ok(true)
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
/// [`SubagentInfo`] forest (each annotated with its return) for machine consumption.
fn show_subagents(r: &SessionRef, json: bool) -> Result<()> {
    let subs = cv_core::subagent_tree_of(r);

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
                        if let Some(ret) = cv_core::harness::claude::subagent_return(&s.session.path) {
                            obj.insert("return".into(), serde_json::Value::String(ret));
                        }
                    }
                }
                v
            })
            .collect();
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
        let status = s
            .result_status
            .as_deref()
            .map(|st| format!(" [{st}]"))
            .unwrap_or_default();
        println!(
            "── {}  {}{}{} · {} msg ──",
            short_id(s.agent_id()),
            s.agent_type.as_deref().unwrap_or("agent"),
            status,
            wf,
            s.session.message_count,
        );
        // Task descriptions and returns are transcript-derived (untrusted) — sanitize at the
        // terminal seam (G5); the `--json` path stays raw.
        if let Some(d) = &s.description {
            println!("  task: {}", truncate(&cv_core::sanitize::sanitize_line(d), 200));
        }
        // The real return value: the journaled summary (workflow) or the agent's last text turn.
        let ret = s
            .result_summary
            .clone()
            .or_else(|| cv_core::harness::claude::subagent_return(&s.session.path));
        if let Some(ret) = ret {
            println!("  ↩ {}", truncate(&cv_core::sanitize::sanitize_line(&ret), 400));
        }
        println!();
    }
    Ok(())
}

/// Render one specific sub-agent's transcript (`--agent <id>`), resolved by id-prefix relative to
/// its parent session. Honors `--json` and the window flags exactly as a top-level `cv show` would
/// (`--last` counts the sub-agent's own messages).
fn show_one_subagent(
    parent: &SessionRef,
    adapter: &dyn Adapter,
    agent_id: &str,
    json: bool,
    window: &WindowArgs,
) -> Result<()> {
    let subs = cv_core::subagent_tree_of(parent);
    // Match on the full session id (`agent-…`), the bare agentId, or a prefix of either.
    let matches: Vec<&cv_core::SubagentInfo> = subs
        .iter()
        .filter(|s| {
            s.session.id == agent_id
                || s.agent_id() == agent_id
                || s.session.id.starts_with(agent_id)
                || s.agent_id().starts_with(agent_id)
        })
        .collect();
    let sub = match matches.as_slice() {
        [one] => *one,
        [] => {
            return usage(format!(
                "no sub-agent matching {agent_id:?} under {} ({} sub-agent(s); try `cv show {} --subagents`)",
                short_id(&parent.id),
                subs.len(),
                short_id(&parent.id),
            ))
        }
        many => {
            let mut lines: Vec<String> = many
                .iter()
                .map(|s| format!("{}:{}", s.session.harness.as_str(), s.session.id))
                .collect();
            lines.sort();
            return usage(format!(
                "ambiguous sub-agent id {agent_id:?} — {} candidates under {}:\n{}",
                many.len(),
                short_id(&parent.id),
                lines.join("\n")
            ));
        }
    };
    let range = window.bounds(|| count_messages(adapter, &sub.session))?;

    if json {
        return print_session_json(adapter, &sub.session, range, window.max_bytes);
    }

    // A small provenance banner so the reader knows which agent (and outcome) this is.
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
    render_text(adapter, &sub.session, range, window.max_bytes, false)
}

pub(crate) fn cmd_export(id: &str, format: &str, harness: Option<String>, window: &WindowArgs) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let range = window.bounds(|| count_messages(adapter.as_ref(), &r))?;
    match format {
        "md" | "markdown" => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            let outcome = stream_session_render(
                adapter.as_ref(),
                &r,
                &mut out,
                md_header,
                md_message,
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
        "json" => print_session_json(adapter.as_ref(), &r, range, window.max_bytes)?,
        // HTML is one self-contained document: windowed, but never cut mid-page.
        "html" => {
            if window.max_bytes.is_some() {
                bail!("--max-bytes does not apply to --format html — pick a window (--first/--last/--range/--around) instead");
            }
            let mut session = adapter.parse(&r)?;
            if let Some(rg) = range {
                let (s, e) = clamp(rg, session.messages.len());
                session.messages = session.messages.drain(s..e).collect();
            }
            print!("{}", cv_core::html::to_html(&session));
        }
        other => bail!("unknown format {other:?} (use md, json, or html)"),
    }
    Ok(())
}

pub(crate) fn cmd_redact(id: &str, harness: Option<String>, format: &str, stats: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let session = adapter.parse(&r)?;

    let (redacted, st) = cv_core::redact::redact_with(&session, &Default::default());

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&redacted)?),
        "md" | "markdown" => print!("{}", cv_core::render::to_markdown(&redacted)),
        other => bail!("unknown format {other:?} (use md or json)"),
    }

    if stats {
        eprintln!(
            "✦ redacted {} item(s): {} api_key, {} private_key, {} jwt, {} email, {} blob, {} assignment",
            st.total(),
            st.api_keys,
            st.private_keys,
            st.jwts,
            st.emails,
            st.blobs,
            st.assignments,
        );
    }
    Ok(())
}

// ---------- tree ----------

pub(crate) fn cmd_tree(id: &str, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let session = adapter.parse(&r)?;

    println!("# {}", session.label());
    println!("{} · {} · {} msg", session.harness, session.id, session.messages.len());
    println!();

    // Threaded view only if at least one message carries a parent_id.
    let has_threading = session.messages.iter().any(|m| m.parent_id.is_some());
    if has_threading {
        render_tree_dag(&session);
    } else {
        for (i, m) in session.messages.iter().enumerate() {
            println!("{:>4}. {}", i + 1, tree_line(m));
        }
    }

    // The second dimension: the sub-agent forest this session spawned. Claude sessions fan out
    // into directly-spawned (`Agent`/`Task`) sub-agents and `Workflow` sub-agents (grouped by run
    // id, with the orchestrator's journaled outcome) — invisible to flat message threading.
    render_subagent_forest(&r);
    Ok(())
}

/// Print the sub-agent forest under a session: directly-spawned sub-agents, then each workflow's
/// agents grouped together with the journaled `status: summary` outcome. Reads only the cheap
/// metadata/journal sidecars (no transcript bodies).
fn render_subagent_forest(r: &SessionRef) {
    use std::collections::BTreeMap;
    let subs = cv_core::subagent_tree_of(r);
    if subs.is_empty() {
        return;
    }

    let direct: Vec<&cv_core::SubagentInfo> = subs.iter().filter(|s| s.workflow.is_none()).collect();
    // Workflow agents grouped by run id (BTreeMap → stable run order).
    let mut by_wf: BTreeMap<&str, Vec<&cv_core::SubagentInfo>> = BTreeMap::new();
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
}

/// One line describing a sub-agent for the forest view: its short id, type, the journaled status
/// (workflow agents), and the human task description — capped.
fn subagent_line(s: &cv_core::SubagentInfo) -> String {
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
        .map(|t| truncate(t, 88))
        .unwrap_or_default();
    format!("{id}  {kind}{status}  {} msg  {blurb}", s.session.message_count)
}

/// Render messages as an indented DAG by `parent_id`. Roots (no/unknown parent) sit at depth 0.
fn render_tree_dag(session: &Session) {
    use std::collections::HashMap;
    // children: parent_id -> ordered list of child message indices.
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (i, m) in session.messages.iter().enumerate() {
        if let Some(id) = &m.id {
            by_id.insert(id.as_str(), i);
        }
    }
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (i, m) in session.messages.iter().enumerate() {
        let parent = m
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied())
            .filter(|&p| p != i);
        children.entry(parent).or_default().push(i);
    }

    fn walk(
        node: Option<usize>,
        depth: usize,
        session: &Session,
        children: &std::collections::HashMap<Option<usize>, Vec<usize>>,
    ) {
        if let Some(kids) = children.get(&node) {
            for &c in kids {
                let indent = "  ".repeat(depth);
                println!("{indent}• {}", tree_line(&session.messages[c]));
                walk(Some(c), depth + 1, session, children);
            }
        }
    }
    walk(None, 0, session, &children);
}

/// One-line preview of a message for the tree: role (with its kind when that says more than the
/// role), markers for tool turns / sub-agent spawns, and a text preview.
fn tree_line(m: &Message) -> String {
    let role = role_tag(m);
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

    let preview = m.text().map(|t| truncate(&t, 80)).unwrap_or_else(|| {
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

// ---------- diff ----------

/// Compare two sessions message-by-message: a shared prefix (`=`) then a divergence marked
/// `<` (only in A) / `>` (only in B). Comparison is on role + `Message::text()`.
pub(crate) fn cmd_diff(a: &str, b: &str, harness: Option<String>) -> Result<()> {
    let default = parse_harness(&harness)?;
    // Each side may carry its own `harness:id` prefix so the two sessions can live in *different*
    // harnesses; a side with no prefix falls back to the shared `--harness` (or unconstrained).
    let (ra, aa) = resolve(a, default)?;
    let (rb, ab) = resolve(b, default)?;
    let sa = aa.parse(&ra)?;
    let sb = ab.parse(&rb)?;

    println!(
        "A {:8} {:8}  {} msg",
        sa.harness.as_str(),
        short_id(&sa.id),
        sa.messages.len()
    );
    println!(
        "B {:8} {:8}  {} msg",
        sb.harness.as_str(),
        short_id(&sb.id),
        sb.messages.len()
    );
    println!();

    let key = |m: &Message| (m.role, m.text().unwrap_or_default());
    let na = sa.messages.len();
    let nb = sb.messages.len();

    // Shared prefix: matching role+text from the top.
    let mut shared = 0;
    while shared < na && shared < nb && key(&sa.messages[shared]) == key(&sb.messages[shared]) {
        println!("= {}", diff_line(&sa.messages[shared]));
        shared += 1;
    }
    // After divergence, list A's remainder then B's remainder.
    for m in &sa.messages[shared..] {
        println!("< {}", diff_line(m));
    }
    for m in &sb.messages[shared..] {
        println!("> {}", diff_line(m));
    }

    println!(
        "\n{shared} shared, {} only-in-A, {} only-in-B",
        na - shared,
        nb - shared
    );
    Ok(())
}

/// One-line `role: text-preview` for a diff row.
fn diff_line(m: &Message) -> String {
    let role = cv_core::render::role_label(m.role);
    let text = m.text().unwrap_or_default();
    format!("{role:9} {}", truncate(&text, 80))
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
}

/// What to render: an optional `[start, end)` message window and an optional byte budget.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RenderOpts {
    pub range: Option<(usize, Option<usize>)>,
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

/// Render a session to `out` by **streaming** — each message is rendered and written as it arrives,
/// then dropped, so a multi-GB transcript renders at O(largest message) instead of materializing the
/// whole `Session`. The header needs the label and model, which come from the first user/assistant
/// turns, so it's held behind a small bounded buffer (`HOLDBACK` messages) until those are known and
/// then flushed ahead of the body — header info lives in the first turns, so the buffer stays tiny.
pub(crate) fn stream_session_render<W: std::io::Write>(
    adapter: &dyn Adapter,
    r: &SessionRef,
    out: &mut W,
    header: impl Fn(&HeaderInfo) -> String,
    render_msg: impl Fn(&Message) -> String,
    opts: RenderOpts,
) -> Result<RenderOutcome> {
    use cv_core::{Flow, MessageSink, ParseOptions};
    const HOLDBACK: usize = 24;

    struct Sink<'w, W, H, R> {
        out: &'w mut W,
        harness: Harness,
        id: String,
        cwd: Option<PathBuf>,
        title: Option<String>,
        model: Option<String>,
        first_user: Option<String>,
        header: H,
        render_msg: R,
        buf: Vec<String>,
        printed: bool,
        result: std::io::Result<()>,
        resolver: cv_core::Resolver,
        // Windowed view: 0-based message index we're at, plus the [start, end) bounds. Messages
        // outside the window are never materialized — their on-disk content is never touched.
        idx: usize,
        start: usize,
        end: Option<usize>,
        // Byte budget: rendered bytes so far (header + messages) and where the cut landed.
        max_bytes: Option<usize>,
        bytes: usize,
        rendered: usize,
        next: Option<usize>,
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> String> Sink<'_, W, H, R> {
        fn write(&mut self, s: &str) {
            if self.result.is_ok() {
                self.result = self.out.write_all(s.as_bytes());
            }
        }
        fn flush_header(&mut self) {
            if self.printed {
                return;
            }
            let info = HeaderInfo {
                harness: self.harness,
                id: self.id.clone(),
                cwd: self.cwd.clone(),
                label: cv_core::label_from(self.title.as_deref(), self.first_user.as_deref()),
                model: self.model.clone(),
            };
            let head = (self.header)(&info);
            self.bytes += head.len();
            self.write(&head);
            let buffered = std::mem::take(&mut self.buf);
            for s in buffered {
                self.write(&s);
            }
            self.printed = true;
        }
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> String> MessageSink for Sink<'_, W, H, R> {
        fn meta(&mut self, s: &Session) {
            // Authoritative *parsed* session metadata, delivered before the body by the bridge
            // `stream` and by adapters that call `sink.meta` (e.g. codex). The parsed title overrides
            // the discovery-time `SessionRef` title so the header matches a full parse — some
            // adapters' discovery title differs from the parsed one (codex's discovery title is the
            // first user record, which the parse skips). Model/cwd fill in if discovery lacked them.
            self.title = s.title.clone();
            if self.model.is_none() {
                self.model = s.model.clone();
            }
            if self.cwd.is_none() {
                self.cwd = s.cwd.clone();
            }
        }
        fn message(&mut self, m: Message) -> Flow {
            if self.result.is_err() || self.next.is_some() {
                return Flow::Stop;
            }
            let idx = self.idx;
            self.idx += 1;

            // Past the window's end: nothing left to render — stop early so we never read the
            // rest of the file (the whole point of a windowed show on a huge session).
            if let Some(end) = self.end {
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
            if idx < self.start {
                return Flow::Continue;
            }

            // In-window: resolve this message's lazy content spans (peak = one message) and render.
            let mut m = m;
            m.materialize(&self.resolver);
            if self.first_user.is_none() && m.role == Role::User {
                if let Some(t) = m.text() {
                    if !t.trim().is_empty() {
                        self.first_user = Some(t);
                    }
                }
            }
            let rendered = (self.render_msg)(&m);
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
            if self.printed {
                self.write(&rendered);
            } else {
                self.buf.push(rendered);
                let title_known = self.title.is_some() || self.first_user.is_some();
                if (self.model.is_some() && title_known) || self.buf.len() >= HOLDBACK {
                    self.flush_header();
                }
            }
            if self.result.is_err() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    let start = opts.range.map(|(s, _)| s).unwrap_or(0);
    let end = opts.range.and_then(|(_, e)| e);
    let mut sink = Sink {
        out,
        harness: r.harness,
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        model: None,
        first_user: None,
        header,
        render_msg,
        buf: Vec::new(),
        printed: false,
        result: Ok(()),
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        idx: 0,
        start,
        end,
        max_bytes: opts.max_bytes,
        bytes: 0,
        rendered: 0,
        next: None,
    };
    // Windowed: lazy spans, so out-of-window giant fields arrive as 16-byte handles and only
    // in-window messages materialize (the sink already resolves per message). A full show reads
    // every byte regardless — spans would only add resolve overhead there (floor = C).
    let parse_opts = if opts.range.is_some() {
        ParseOptions::lazy()
    } else {
        ParseOptions::bulk()
    };
    // Windowed reads first try the seekable-session store (offsets recorded by `cv index`): jump
    // straight to message `start`'s byte offset and parse only the window, instead of streaming
    // from byte 0 and discarding everything before it. `stream_range` delivers one `meta()` (the
    // recorded metadata snapshot — including the model the skipped prefix would have provided)
    // plus exactly the window's messages, so the sink starts its index at `start`. `false` means
    // no/stale offsets — the sink is untouched; fall through to the full stream below.
    if start > 0 {
        sink.idx = start;
        if cv_core::offsets::stream_range(r, start, end, &parse_opts, &mut sink)? {
            sink.flush_header();
            sink.result?;
            return Ok(RenderOutcome {
                rendered: sink.rendered,
                next: sink.next,
            });
        }
        sink.idx = 0;
    }
    adapter.stream(r, &parse_opts, &mut sink)?;
    sink.flush_header(); // short sessions (no assistant turn / < HOLDBACK msgs) flush here
    sink.result?;
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
pub(crate) fn show_message(m: &Message) -> String {
    let mut s = format!("── {} ──\n", role_tag(m));
    for b in &m.content {
        match b {
            Block::Text { text } => {
                s.push_str(text);
                s.push('\n');
            }
            Block::Thinking { text, .. } => s.push_str(&format!("[thinking] {}\n", truncate(text, 200))),
            Block::ToolUse { id, name, input, .. } => s.push_str(&format!(
                "[tool_use {name} {id}] {}\n",
                truncate(&input.to_string(), 200)
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
    s
}

/// The turn label: the role, plus the message `kind` whenever it says more than the role does. A
/// System turn always carries its kind (`system · injected_context`, `system · error`, …) and,
/// for Claude attachments, the attachment kind from `extra["claude"]["attachment_type"]`
/// (`system · injected_context · hook_success`). Other roles add the kind only when it differs
/// from the role's default (`user · compaction_summary`, `assistant · error`).
fn role_tag(m: &Message) -> String {
    match m.role {
        Role::System => {
            let attachment = m
                .harness_extra(Harness::Claude)
                .and_then(|c| c.get("attachment_type"))
                .and_then(|v| v.as_str());
            match attachment {
                Some(a) => format!("system · {} · {a}", kind_name(m.kind)),
                None => format!("system · {}", kind_name(m.kind)),
            }
        }
        role => {
            let base = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
                _ => "tool",
            };
            if m.kind == MessageKind::for_role(role) {
                base.to_string()
            } else {
                format!("{base} · {}", kind_name(m.kind))
            }
        }
    }
}

/// `MessageKind` as serde spells it (`injected_context`, `compaction_boundary`, …).
fn kind_name(k: MessageKind) -> String {
    match serde_json::to_value(k) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{k:?}").to_ascii_lowercase(),
    }
}

/// Where a persisted (too-large) tool output lives on disk, when the adapter recorded it — the
/// transcript holds only the stub the model saw.
fn persisted_path(details: Option<&serde_json::Value>) -> Option<&str> {
    details?.pointer("/persistedOutput/path")?.as_str()
}

/// Header for `cv export md`.
fn md_header(h: &HeaderInfo) -> String {
    let model = h.model.as_ref().map(|m| format!("- model: {m}\n")).unwrap_or_default();
    format!(
        "# {}\n\n- harness: {}\n- id: {}\n- cwd: {}\n{}\n",
        h.label,
        h.harness,
        h.id,
        h.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into()),
        model,
    )
}

/// One rendered `cv export md` message section.
fn md_message(m: &Message) -> String {
    let who = match m.role {
        Role::System => format!("System · {}", kind_name(m.kind)),
        Role::User => "User".to_string(),
        Role::Assistant => "Assistant".to_string(),
        Role::Tool => "Tool".to_string(),
    };
    let mut out = format!("## {who}\n\n");
    for b in &m.content {
        match b {
            Block::Text { text } => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            Block::Thinking { text, .. } => {
                out.push_str("> 🧠 ");
                out.push_str(&text.replace('\n', "\n> "));
                out.push_str("\n\n");
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(&format!("**🔧 {name}**\n\n```json\n{input}\n```\n\n"));
            }
            Block::ToolResult { content, is_error, .. } => {
                out.push_str(&format!(
                    "**↩ result{}**\n\n```\n{}\n```\n\n",
                    if *is_error { " (error)" } else { "" },
                    truncate(content, 4000)
                ));
            }
            Block::File { path, source, .. } => {
                out.push_str(&format!(
                    "_[file: {}]_\n\n",
                    path.as_deref().or(source.as_deref()).unwrap_or("?")
                ));
            }
            Block::Image { .. } => out.push_str("_[image]_\n\n"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CLUSTERVISION_HOME` is process-global — these are the only env-touching unit tests in
    /// this binary, serialized against each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cv-view-seek-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Record message offsets for `r` the way `cv index`'s ride-along does.
    fn record_offsets(r: &SessionRef) {
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        let mut sink = cv_core::offsets::OffsetSink::new();
        adapter
            .stream(r, &cv_core::ParseOptions::lazy_offsets(), &mut sink)
            .unwrap();
        assert!(sink.seekable());
        cv_core::offsets::record(r, &sink, mtime, size);
    }

    fn render(r: &SessionRef, range: Option<(usize, Option<usize>)>) -> String {
        render_with(r, RenderOpts { range, max_bytes: None }).0
    }

    fn render_with(r: &SessionRef, opts: RenderOpts) -> (String, RenderOutcome) {
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let mut out = Vec::new();
        let outcome = stream_session_render(adapter.as_ref(), r, &mut out, show_header, show_message, opts).unwrap();
        (String::from_utf8(out).unwrap(), outcome)
    }

    /// Whether a windowed render of `r` would take the seek path right now.
    fn seekable(r: &SessionRef, start: usize) -> bool {
        let mut sink = cv_core::CollectSink::default();
        cv_core::offsets::stream_range(r, start, Some(start + 1), &cv_core::ParseOptions::lazy(), &mut sink).unwrap()
    }

    /// A small Claude fixture: title, a user turn, a model-carrying assistant turn, one big user
    /// turn, then eight short follow-ups (11 messages).
    fn claude_fixture(home: &std::path::Path) -> SessionRef {
        let path = home.join("s.jsonl");
        let big = "long \"quoted\" content\n".repeat(400);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"ai-title","aiTitle":"render seek"}}"#).unwrap();
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
            id: "render-seek".into(),
            harness: Harness::Claude,
            path,
            cwd: Some("/w".into()),
            title: Some("render seek".into()),
            created_at: None,
            updated_at: None,
            message_count: 11,
        }
    }

    /// THE Phase-2 contract: the same window rendered via the seek path (offsets recorded) and
    /// via the full stream (no offsets) must be **byte-identical** — header (incl. the model the
    /// skipped prefix provides) and body. And a stale recording falls back, output unchanged.
    #[test]
    fn windowed_render_is_byte_identical_via_seek_and_full_stream() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("claude");
        std::env::set_var("CLUSTERVISION_HOME", &home);
        let r = claude_fixture(&home);
        let path = r.path.clone();
        record_offsets(&r);
        assert!(seekable(&r, 3), "recording must enable the seek path");

        // Window past the model-carrying assistant turn: the header's model must come from the
        // recorded metadata on the seek path. Compare against the full stream with no catalog.
        for range in [(3usize, Some(6usize)), (2, Some(11)), (5, None), (1, Some(2))] {
            let seeked = render(&r, Some((range.0, range.1)));
            std::env::remove_var("CLUSTERVISION_HOME");
            std::env::set_var("CLUSTERVISION_HOME", temp_home("claude-empty"));
            assert!(!seekable(&r, range.0), "empty catalog must fall back");
            let full = render(&r, Some((range.0, range.1)));
            std::env::set_var("CLUSTERVISION_HOME", &home);
            assert_eq!(seeked, full, "range {range:?} render must be byte-identical");
            assert!(seeked.contains("claude-test"), "header model expected in {range:?}");
        }

        // Staleness: appending a message makes the recording stale → fallback, still correct.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(
                f,
                r#"{{"type":"user","uuid":"uz","message":{{"role":"user","content":"appended"}}}}"#
            )
            .unwrap();
        }
        assert!(!seekable(&r, 3), "appended file must read as stale");
        let after = render(&r, Some((9, Some(12))));
        assert!(after.contains("follow-up 7") && after.contains("appended"));

        std::env::remove_var("CLUSTERVISION_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    /// `--max-bytes`: the render stops before the message that would overflow the budget and
    /// reports the continuation index; the first in-window message always renders; a budget
    /// that fits everything reports no cut.
    #[test]
    fn byte_budget_cuts_and_reports_the_next_index() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("budget");
        std::env::set_var("CLUSTERVISION_HOME", temp_home("budget-empty"));
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
                range: Some((2, None)),
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
                range: Some((3, None)),
                max_bytes: Some(1 << 20),
            },
        );
        assert_eq!(oc.rendered, 8);
        assert_eq!(oc.next, None);

        std::env::remove_var("CLUSTERVISION_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    /// System turns are labelled by `kind` (plus Claude's attachment kind when present); other
    /// roles add the kind only when it says more than the role.
    #[test]
    fn turn_labels_come_from_kind_not_flat_extra_keys() {
        let mut m = Message::new(Role::System);
        m.kind = MessageKind::InjectedContext;
        assert_eq!(role_tag(&m), "system · injected_context");
        m.harness_extra_mut(Harness::Claude)
            .insert("attachment_type".into(), serde_json::json!("hook_success"));
        assert_eq!(role_tag(&m), "system · injected_context · hook_success");
        // A flat legacy key is ignored: only the nested harness bag counts.
        let mut legacy = Message::new(Role::System);
        legacy.kind = MessageKind::Error;
        legacy.extra.insert("subtype".into(), serde_json::json!("api_error"));
        assert_eq!(role_tag(&legacy), "system · error");
        // Defaults stay bare; a non-default kind on a user turn is surfaced.
        assert_eq!(role_tag(&Message::new(Role::User)), "user");
        assert_eq!(role_tag(&Message::new(Role::Assistant)), "assistant");
        let mut summary = Message::new(Role::User);
        summary.kind = MessageKind::CompactionSummary;
        assert_eq!(role_tag(&summary), "user · compaction_summary");
        assert!(show_message(&summary).starts_with("── user · compaction_summary ──\n"));
    }

    /// Same contract for codex, whose metadata comes from the recorded `meta()` snapshot (the
    /// parse-time title — `None` — must override the discovery title on both paths).
    #[test]
    fn codex_windowed_render_is_byte_identical_via_seek_and_full_stream() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("codex");
        std::env::set_var("CLUSTERVISION_HOME", &home);

        let path = home.join("rollout-r.jsonl");
        let big = "giant output\n".repeat(500);
        let mut f = std::fs::File::create(&path).unwrap();
        for l in [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"rollout-r","cwd":"/work"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello there"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item",
                "payload":{"type":"function_call_output","call_id":"c1","output":big}}).to_string(),
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
            message_count: 3,
        };
        record_offsets(&r);
        assert!(seekable(&r, 2));

        for range in [(2usize, Some(4usize)), (1, None), (3, Some(5))] {
            let seeked = render(&r, Some((range.0, range.1)));
            std::env::set_var("CLUSTERVISION_HOME", temp_home("codex-empty"));
            let full = render(&r, Some((range.0, range.1)));
            std::env::set_var("CLUSTERVISION_HOME", &home);
            assert_eq!(seeked, full, "range {range:?} render must be byte-identical");
            assert!(seeked.contains("gpt-test"), "header model expected in {range:?}");
        }

        std::env::remove_var("CLUSTERVISION_HOME");
        std::fs::remove_dir_all(&home).ok();
    }
}
