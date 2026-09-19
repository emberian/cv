//! `cv show` / `cv export` / `cv tree` / `cv diff` / `cv redact` — reading sessions,
//! plus the streaming renderer (`stream_session_render`) and its header/message helpers.

use crate::util::{home_rel, parse_harness, parse_msg_range, short_id};
use anyhow::{bail, Context, Result};
use cv_core::ir::{truncate, Block, Harness, Message, Role, Session, SessionRef};
use cv_core::Adapter;
use std::path::PathBuf;

pub(crate) fn cmd_show(
    id: &str,
    harness: Option<String>,
    json: bool,
    range: Option<String>,
    subagents: bool,
    agent: Option<String>,
    pre_compaction: Option<usize>,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    // An `agent-…` shaped id goes straight to the sub-agent view: cv_core::find_cheap can now
    // resolve these too (its fleet-wide fallback), but this path keeps the provenance banner and
    // lists candidates on an ambiguous prefix instead of erroring.
    if id.starts_with("agent-") {
        let parsed_range = range.as_deref().map(parse_msg_range).transpose()?;
        if show_agent_fleetwide(id, json, parsed_range)? {
            return Ok(());
        }
    }
    // find_cheap first: don't pay a full fleet re-discovery before trying the id as a sub-agent
    // id — sub-agents aren't in the main pool, but `cv show <agent-id>` should still just work
    // (harvest reports hand out bare agent ids all the time). Only when the id is neither a
    // cataloged session nor an agent does the full-scan `find` escalation run (a session in the
    // discovery probe's blind spots).
    let found = match cv_core::find_cheap(id, want)? {
        Some(hit) => Some(hit),
        None => {
            let range = range.as_deref().map(parse_msg_range).transpose()?;
            if show_agent_fleetwide(id, json, range)? {
                return Ok(());
            }
            cv_core::find(id, want)?
        }
    };
    let (r, adapter) = found.with_context(|| format!("no session (and no sub-agent) matching {id:?}"))?;
    let mut range = range.as_deref().map(parse_msg_range).transpose()?;

    // `--pre-compaction <N>`: resolve the Nth (1-based) compaction's pre-span into a window. This
    // reads the context the continued agent lost — the whole point is to retrieve it by message
    // range without the user computing offsets by hand.
    if let Some(n) = pre_compaction {
        let comps = cv_core::compaction::detect(&r, false)?;
        if comps.is_empty() {
            anyhow::bail!("{} never compacted — nothing pre-compaction to show", short_id(&r.id));
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
            "✦ pre-compaction #{n} of {}: messages {start}-{end} (the span before boundary @msg {})",
            comps.len(),
            comps[idx].boundary_msg_idx,
        );
        range = Some((start, Some(end)));
    }

    // `--agent <id>`: render one specific sub-agent's transcript (resolved through this parent,
    // since sub-agents aren't in the main pool). `--subagents`: list the whole forest with results.
    if let Some(agent_id) = &agent {
        return show_one_subagent(&r, adapter.as_ref(), agent_id, json, range);
    }
    if subagents {
        return show_subagents(&r, json);
    }

    // JSON wants the whole IR (incl. `extra`), so it materializes; the rendered transcript streams.
    if json {
        let mut session = adapter.parse(&r)?;
        if let Some((start, end)) = range {
            let end = end.unwrap_or(session.messages.len()).min(session.messages.len());
            let start = start.min(end);
            session.messages = session.messages.drain(start..end).collect();
        }
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
    }

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    stream_session_render(adapter.as_ref(), &r, &mut out, show_header, show_message, range)?;
    use std::io::Write;
    out.flush()?;
    Ok(())
}

/// `cv show <agent-id>` with no parent given: find which session(s) spawned the agent (filename
/// scan across the fleet) and render it through the one parent — or list the candidates when the
/// prefix is ambiguous. Returns `false` when nothing agent-shaped matched (the caller has one
/// more reading of the id to try).
fn show_agent_fleetwide(agent_id: &str, json: bool, range: Option<(usize, Option<usize>)>) -> Result<bool> {
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
            show_one_subagent(parent, adapter.as_ref(), agent_id, json, range)?;
            Ok(true)
        }
        many => {
            println!("# {} session(s) have a sub-agent matching {agent_id:?}:\n", many.len());
            for p in many {
                println!(
                    "cv show {} --agent {agent_id}   # {}",
                    short_id(&p.id),
                    p.title.as_deref().unwrap_or("untitled"),
                );
            }
            Ok(true)
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
                    // Surface the bare agentId (the journal/transcript key) so consumers don't have
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
/// its parent session. Honors `--json` and `--range` exactly as a top-level `cv show` would.
fn show_one_subagent(
    parent: &SessionRef,
    adapter: &dyn Adapter,
    agent_id: &str,
    json: bool,
    range: Option<(usize, Option<usize>)>,
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
        [] => bail!(
            "no sub-agent matching {agent_id:?} under {} ({} sub-agent(s); try `cv show {} --subagents`)",
            short_id(&parent.id),
            subs.len(),
            short_id(&parent.id),
        ),
        many => bail!(
            "{} sub-agents match {agent_id:?} — disambiguate with a longer id ({})",
            many.len(),
            many.iter()
                .map(|s| short_id(s.agent_id()))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    };

    if json {
        let mut session = adapter.parse(&sub.session)?;
        if let Some((start, end)) = range {
            let end = end.unwrap_or(session.messages.len()).min(session.messages.len());
            let start = start.min(end);
            session.messages = session.messages.drain(start..end).collect();
        }
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
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

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    stream_session_render(adapter, &sub.session, &mut out, show_header, show_message, range)?;
    use std::io::Write;
    out.flush()?;
    Ok(())
}

pub(crate) fn cmd_export(id: &str, format: &str, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    match format {
        "md" | "markdown" => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            stream_session_render(adapter.as_ref(), &r, &mut out, md_header, md_message, None)?;
            use std::io::Write;
            out.flush()?;
        }
        // JSON and HTML need the whole session at once (full IR / a single self-contained document).
        "json" => {
            let session = adapter.parse(&r)?;
            println!("{}", serde_json::to_string_pretty(&session)?);
        }
        "html" => {
            let session = adapter.parse(&r)?;
            print!("{}", cv_core::html::to_html(&session));
        }
        other => bail!("unknown format {other:?} (use md, json, or html)"),
    }
    Ok(())
}

pub(crate) fn cmd_redact(id: &str, harness: Option<String>, format: &str, stats: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
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
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
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

/// One-line preview of a message for the tree: role, markers for tool turns / sub-agent spawns,
/// and a text preview.
fn tree_line(m: &Message) -> String {
    let role = cv_core::render::role_label(m.role);
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
    // Sub-agent spawns recorded in `extra` (harness-specific).
    if let Some(sub) = sub_agent_from_extra(m) {
        tags.push(format!("↳ sub-agent ({sub})"));
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

/// Look for a sub-agent spawn recorded in `Message::extra` under common harness keys.
fn sub_agent_from_extra(m: &Message) -> Option<String> {
    for key in ["subagent", "sub_agent", "subAgent", "spawn", "agent", "child_agent"] {
        if let Some(v) = m.extra.get(key) {
            return Some(match v {
                serde_json::Value::String(s) => s.clone(),
                other => truncate(&other.to_string(), 40),
            });
        }
    }
    None
}

// ---------- diff ----------

/// Compare two sessions message-by-message: a shared prefix (`=`) then a divergence marked
/// `<` (only in A) / `>` (only in B). Comparison is on role + `Message::text()`.
pub(crate) fn cmd_diff(a: &str, b: &str, harness: Option<String>) -> Result<()> {
    let default = parse_harness(&harness)?;
    // Each side may carry its own `harness:id` prefix so the two sessions can live in *different*
    // harnesses — applying one `--harness` to both made cross-harness diffs impossible. A side with
    // no recognized prefix falls back to the shared `--harness` (or unconstrained search).
    let (wa, ida) = split_side_harness(a, default);
    let (wb, idb) = split_side_harness(b, default);
    let (ra, aa) = cv_core::find(ida, wa)?.with_context(|| format!("no session matching {a:?}"))?;
    let (rb, ab) = cv_core::find(idb, wb)?.with_context(|| format!("no session matching {b:?}"))?;
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

/// Split an optional `<harness>:<id>` prefix off a diff side. Only treats the part before the first
/// `:` as a harness when it actually names one; otherwise the whole string is the id and `fallback`
/// (the shared `--harness`) applies.
fn split_side_harness(spec: &str, fallback: Option<Harness>) -> (Option<Harness>, &str) {
    if let Some((head, rest)) = spec.split_once(':') {
        if let Some(h) = Harness::parse(head) {
            return (Some(h), rest);
        }
    }
    (fallback, spec)
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
    range: Option<(usize, Option<usize>)>,
) -> Result<()> {
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
            if self.result.is_err() {
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

    let start = range.map(|(s, _)| s).unwrap_or(0);
    let end = range.and_then(|(_, e)| e);
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
    };
    // Windowed: lazy spans, so out-of-window giant fields arrive as 16-byte handles and only
    // in-window messages materialize (the sink already resolves per message). A full show reads
    // every byte regardless — spans would only add resolve overhead there (floor = C).
    let opts = if range.is_some() {
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
        if cv_core::offsets::stream_range(r, start, end, &opts, &mut sink)? {
            sink.flush_header();
            sink.result?;
            return Ok(());
        }
        sink.idx = 0;
    }
    adapter.stream(r, &opts, &mut sink)?;
    sink.flush_header(); // short sessions (no assistant turn / < HOLDBACK msgs) flush here
    sink.result?;
    Ok(())
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
    let tag = match m.role {
        Role::System => system_tag(m),
        Role::User => "user".to_string(),
        Role::Assistant => "assistant".to_string(),
        Role::Tool => "tool".to_string(),
    };
    let mut s = format!("── {tag} ──\n");
    for b in &m.content {
        match b {
            Block::Text { text } => {
                s.push_str(text);
                s.push('\n');
            }
            Block::Thinking { text, .. } => s.push_str(&format!("[thinking] {}\n", truncate(text, 200))),
            Block::ToolUse { name, input, .. } => {
                s.push_str(&format!("[tool_use {name}] {}\n", truncate(&input.to_string(), 200)))
            }
            Block::ToolResult {
                content,
                is_error,
                details,
                ..
            } => {
                s.push_str(&format!(
                    "[tool_result{}] {}\n",
                    if *is_error { " error" } else { "" },
                    truncate(content, 200)
                ));
                if let Some(p) = persisted_path(details.as_ref()) {
                    s.push_str(&format!("  ↳ full output on disk: {p}\n"));
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

/// Label for a System turn: the attachment kind (a Claude Code system reminder — `hook_success`,
/// `edited_text_file`, …) or the record subtype (`api_error`, `compact_boundary`, …) when the
/// adapter recorded one, so a reader can tell a hook's output from a compaction marker.
fn system_tag(m: &Message) -> String {
    let kind = m
        .extra
        .get("attachmentType")
        .or_else(|| m.extra.get("subtype"))
        .and_then(|v| v.as_str());
    match kind {
        Some(k) => format!("system · {k}"),
        None => "system".to_string(),
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
        Role::System => "System",
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::Tool => "Tool",
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
    use std::io::Write;

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
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let mut out = Vec::new();
        stream_session_render(adapter.as_ref(), r, &mut out, show_header, show_message, range).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Whether a windowed render of `r` would take the seek path right now.
    fn seekable(r: &SessionRef, start: usize) -> bool {
        let mut sink = cv_core::CollectSink::default();
        cv_core::offsets::stream_range(r, start, Some(start + 1), &cv_core::ParseOptions::lazy(), &mut sink).unwrap()
    }

    /// THE Phase-2 contract: the same window rendered via the seek path (offsets recorded) and
    /// via the full stream (no offsets) must be **byte-identical** — header (incl. the model the
    /// skipped prefix provides) and body. And a stale recording falls back, output unchanged.
    #[test]
    fn windowed_render_is_byte_identical_via_seek_and_full_stream() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("claude");
        std::env::set_var("CLUSTERVISION_HOME", &home);

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
        let r = SessionRef {
            id: "render-seek".into(),
            harness: Harness::Claude,
            path: path.clone(),
            cwd: Some("/w".into()),
            title: Some("render seek".into()),
            created_at: None,
            updated_at: None,
            message_count: 11,
        };
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
