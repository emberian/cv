//! A session's message tree and the sub-agent forest under it, as the projections every
//! front-end shows: `cv show --tree`, `cv show --subagents`, and `/api/session/…/subagents`.

use crate::ir::{truncate, Block, Lineage, Message, MessageKind, Role, Session, SessionRef};
use crate::resolve::short_id;
use crate::sanitize::sanitize_line;
use crate::{Adapter, AgentRef, Flow, Resolver};
use anyhow::Result;
use serde_json::Value;
use std::io::Write;

/// A message's plain-text blocks joined by newlines; `None` when that is blank.
pub fn reply_text(m: &Message, resolver: &Resolver) -> Result<Option<String>> {
    let mut out = String::new();
    for block in &m.content {
        if let Block::Text { text, .. } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.try_resolve(resolver)?);
        }
    }
    Ok((!out.trim().is_empty()).then_some(out))
}

/// A sub-agent's final return value on any harness: the last non-blank assistant text turn of
/// its transcript, streamed lazily.
pub fn final_return(r: &SessionRef) -> Result<Option<String>> {
    let Some(adapter) = crate::harness::for_harness(r.harness) else {
        return Ok(None);
    };
    struct LastAssistantText {
        resolver: Resolver,
        last: Option<String>,
        error: Option<anyhow::Error>,
    }
    impl crate::MessageSink for LastAssistantText {
        fn tier(&self) -> crate::stream::Tier {
            crate::stream::Tier::Facts
        }
        fn meta(&mut self, session: &Session) {
            self.resolver = session.resolver();
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.error.take()
        }
        fn message(&mut self, m: Message) -> Flow {
            if m.role != Role::Assistant {
                return Flow::Continue;
            }
            match reply_text(&m, &self.resolver) {
                Ok(Some(text)) => self.last = Some(text),
                Ok(None) => {}
                Err(error) => {
                    self.error = Some(error);
                    return Flow::Stop;
                }
            }
            Flow::Continue
        }
    }
    let mut sink = LastAssistantText {
        resolver: Resolver::new(Some(r.path.clone())),
        last: None,
        error: None,
    };
    adapter.try_stream(r, &crate::Demand::LEAN.with_spans(), &mut sink)?;
    Ok(sink.last)
}

/// What an agent came back with when no workflow journal recorded it: a directly spawned agent
/// reports through its transcript's last text turn.
pub fn unjournaled_return(s: &AgentRef) -> Result<Option<String>> {
    match s.result_summary {
        Some(_) => Ok(None),
        None => final_return(&s.session),
    }
}

/// The forest `r` spawned as JSON: each [`AgentRef`] with its bare `agent_id` and, where no
/// journal summarized it, the `return` its transcript ends on. Raw, unsanitized text.
pub fn agents_json(r: &SessionRef) -> Result<Vec<Value>> {
    crate::subagent_tree_of(r)?
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s)?;
            v["agent_id"] = s.agent_id().into();
            if let Some(ret) = unjournaled_return(s)? {
                v["return"] = ret.into();
            }
            Ok(v)
        })
        .collect()
}

/// The `--subagents` view: every direct and workflow sub-agent with its type, journaled outcome
/// and final return value.
pub fn write_agents(r: &SessionRef, out: &mut impl Write) -> Result<()> {
    let subs = crate::subagent_tree_of(r)?;
    if subs.is_empty() {
        writeln!(out, "no sub-agents spawned by {}", short_id(&r.id))?;
        return Ok(());
    }
    writeln!(out, "# sub-agents of {}\n", short_id(&r.id))?;
    for s in &subs {
        let wf = s.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        writeln!(out, "• {}{wf}", agent_line(s))?;
        if let Some(ret) = unjournaled_return(s)? {
            writeln!(out, "  ↩ {}", truncate(&sanitize_line(&ret), 400))?;
        }
    }
    Ok(())
}

/// The forest under a session's tree: directly spawned sub-agents, then each workflow's agents
/// together. Reads only the metadata and journal sidecars, no transcript bodies.
fn write_forest(r: &SessionRef, out: &mut impl Write) -> Result<()> {
    let subs = crate::subagent_tree_of(r)?;
    if subs.is_empty() {
        return Ok(());
    }
    let (flows, direct): (Vec<&AgentRef>, Vec<&AgentRef>) = subs.iter().partition(|s| s.workflow.is_some());
    let mut by_wf: std::collections::BTreeMap<&str, Vec<&AgentRef>> = Default::default();
    for s in &flows {
        by_wf.entry(s.workflow.as_deref().unwrap_or("")).or_default().push(s);
    }
    writeln!(
        out,
        "\n## sub-agents ({} direct, {} workflow agent(s) across {} workflow(s))",
        direct.len(),
        flows.len(),
        by_wf.len()
    )?;
    if !direct.is_empty() {
        writeln!(out)?;
    }
    for s in &direct {
        writeln!(out, "• {}", agent_line(s))?;
    }
    for (wf, agents) in &by_wf {
        writeln!(out, "\n  ⟐ workflow {wf}  ({} agent(s))", agents.len())?;
        for s in agents {
            writeln!(out, "    • {}", agent_line(s))?;
        }
    }
    Ok(())
}

/// One line describing a sub-agent: short id, type, the journaled status, message count, and the
/// journaled summary or, failing that, the task description. Transcript-derived, so sanitized.
pub fn agent_line(s: &AgentRef) -> String {
    let status = s
        .result_status
        .as_deref()
        .map(|st| format!(" [{st}]"))
        .unwrap_or_default();
    let blurb = s
        .result_summary
        .as_deref()
        .or(s.description.as_deref())
        .or(s.session.title.as_deref())
        .map(|t| truncate(&sanitize_line(t), 88))
        .unwrap_or_default();
    format!(
        "{}  {}{status}  {} msg  {blurb}",
        short_id(s.agent_id()),
        s.agent_type.as_deref().unwrap_or("agent"),
        s.session.message_count.map_or("–".to_string(), |n| n.to_string())
    )
}

/// One message of a session's tree: its place in the DAG and its one-line preview.
pub struct MessageRow {
    pub id: Option<String>,
    pub parent: Option<String>,
    pub line: String,
}

/// Read a session as tree rows in one guarded pass, with its metadata and first prompt title.
pub fn read_messages(r: &SessionRef, adapter: &dyn Adapter) -> Result<(Session, Vec<MessageRow>, Option<String>)> {
    struct TreeSink {
        rows: Vec<MessageRow>,
        resolver: Resolver,
        first_prompt: Option<String>,
        error: Option<anyhow::Error>,
    }
    impl crate::MessageSink for TreeSink {
        fn meta(&mut self, session: &Session) {
            self.resolver = session.resolver();
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.error.take()
        }
        fn message(&mut self, m: Message) -> Flow {
            let result = (|| -> Result<()> {
                if self.first_prompt.is_none() {
                    self.first_prompt = m.prompt_title(&self.resolver)?;
                }
                let preview = crate::render::message_text_preview(&m, &self.resolver, 80)?;
                let line = message_line(&m, preview);
                self.rows.push(MessageRow {
                    id: m.id,
                    parent: m.parent_id,
                    line,
                });
                Ok(())
            })();
            if let Err(error) = result {
                self.error = Some(error);
                return Flow::Stop;
            }
            Flow::Continue
        }
    }
    let mut sink = TreeSink {
        rows: Vec::new(),
        resolver: Resolver::new(Some(r.path.clone())),
        first_prompt: None,
        error: None,
    };
    let pass = crate::stream::Pass {
        guard: true,
        ..crate::stream::Pass::RECORD
    };
    let read = crate::stream::stream_counted(adapter, r, &crate::Demand::FACTS.with_spans(), &mut sink, pass)?;
    let session = read.session;
    if let Some(error) = sink.error {
        return Err(error);
    }
    read.guard.expect("guarded pass").verify(r)?;
    Ok((session, sink.rows, sink.first_prompt))
}

/// Where a session came from, as one line; `None` when it records no lineage.
pub fn lineage_line(lineage: &Lineage) -> Option<String> {
    let parts: Vec<String> = [
        ("parent", &lineage.parent, true),
        ("forked from", &lineage.forked_from, true),
        ("continues", &lineage.continues, true),
        ("continued in", &lineage.continued_in, true),
        ("agent", &lineage.agent_path, false),
        ("tool", &lineage.spawned_by_tool_use, true),
    ]
    .into_iter()
    .filter_map(|(label, value, id)| {
        let value = value.as_deref()?;
        Some(format!(
            "{label} {}",
            if id {
                short_id(value)
            } else {
                sanitize_line(value).into_owned()
            }
        ))
    })
    .collect();
    (!parts.is_empty()).then(|| format!("lineage: {}", parts.join(" · ")))
}

/// The `--tree` view: header, lineage, the messages as a DAG (numbered when the store records no
/// parents), then the sub-agent forest.
pub fn write_tree(r: &SessionRef, adapter: &dyn Adapter, out: &mut impl Write) -> Result<()> {
    let (session, rows, first_prompt) = read_messages(r, adapter)?;
    let label = crate::label_from(session.title.as_deref(), first_prompt.as_deref());
    writeln!(out, "# {label}")?;
    writeln!(out, "{} · {} · {} msg", session.harness, session.id, rows.len())?;
    if let Some(lineage) = lineage_line(&session.lineage) {
        writeln!(out, "{lineage}")?;
    }
    writeln!(out)?;
    if rows.iter().any(|row| row.parent.is_some()) {
        let mut result = Ok(());
        walk(&rows, |index, depth| {
            if result.is_ok() {
                result = writeln!(out, "{}• {}", "  ".repeat(depth), rows[index].line);
            }
        });
        result?;
    } else {
        for (i, row) in rows.iter().enumerate() {
            writeln!(out, "{:>4}. {}", i + 1, row.line)?;
        }
    }
    write_forest(r, out)
}

/// Visit messages as a DAG by `parent`, with the depth to indent each: an only child keeps its
/// parent's depth and each child of a fork sits one deeper, so a straight chain is flat and the
/// output stays linear in the session. Roots (no or unknown parent) sit at depth 0.
pub fn walk(rows: &[MessageRow], mut visit: impl FnMut(usize, usize)) {
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
/// role), markers for tool turns and IR-typed sub-agent spawns/returns, and a text preview.
pub fn message_line(m: &Message, preview: Option<String>) -> String {
    let tool = m.content.iter().find_map(|b| match b {
        Block::ToolUse { name, .. } => Some(name.as_str()),
        _ => None,
    });
    let mut tags = Vec::new();
    if tool.is_some() {
        tags.push("🔧 tool".to_string());
    }
    if m.kind == MessageKind::SubagentSpawn {
        tags.push(match tool {
            Some(name) => format!("↳ sub-agent ({name})"),
            None => "↳ sub-agent".to_string(),
        });
    }
    if m.kind == MessageKind::SubagentReturn {
        tags.push("↩ sub-agent".to_string());
    } else if m.content.iter().any(|b| matches!(b, Block::ToolResult { .. })) {
        tags.push("↩ result".to_string());
    }
    let preview = preview
        .or_else(|| tool.map(|name| format!("[{name}]")))
        .unwrap_or_default();
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    };
    format!("{}{tags}  {preview}", crate::render::turn_label(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Harness;

    #[test]
    fn lazy_tree_rows_preserve_owned_previews_and_first_prompt_label() -> Result<()> {
        let world = crate::test_world("tree-preview");
        let text = "🌙".repeat(8192);
        let path = world.write_session("tree-preview", &[
            serde_json::json!({"type":"user","uuid":"empty","message":{"role":"user","content":""}}),
            serde_json::json!({"type":"user","uuid":"blank","message":{"role":"user","content":" \n\t"}}),
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":text}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"role":"assistant","content":[{"type":"text","text":text},{"type":"tool_use","id":"call","name":"Agent","input":{"prompt":text}}]}}),
            serde_json::json!({"type":"user","uuid":"r","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":text}]}}),
        ]);
        let r = SessionRef::new("tree-preview", Harness::Claude, path);
        let adapter = crate::harness::for_harness(Harness::Claude).unwrap();
        let owned = adapter.parse(&r)?;
        let (session, rows, first_prompt) = read_messages(&r, adapter.as_ref())?;
        assert_eq!(rows.len(), owned.messages.len());
        assert_eq!(
            crate::label_from(session.title.as_deref(), first_prompt.as_deref()),
            owned.label()
        );
        for (row, message) in rows.iter().zip(&owned.messages) {
            assert_eq!(row.id, message.id);
            assert_eq!(row.parent, message.parent_id);
            assert_eq!(
                row.line,
                message_line(message, message.text().map(|text| truncate(&text, 80)))
            );
            assert!(row.line.len() < 512);
        }
        Ok(())
    }

    #[test]
    fn iterative_tree_walk_preserves_roots_and_covers_cycles_and_deep_chains() {
        let row = |id: &str, parent: Option<&str>| MessageRow {
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
        walk(&rows, |index, depth| order.push((index, depth)));
        // `root` forks (a, b sit one deeper); `a`'s only child keeps its depth; `cycle2` forks
        // into `cycle1` (already visited) and `tail`.
        assert_eq!(
            order,
            [(0, 0), (1, 1), (3, 1), (2, 1), (4, 0), (5, 0), (6, 0), (7, 0), (8, 1)]
        );
        let rows = (0..5000)
            .map(|i| row(&i.to_string(), (i > 0).then(|| (i - 1).to_string()).as_deref()))
            .collect::<Vec<_>>();
        let mut count = 0;
        // A straight chain is flat: no row is indented, so the output is linear in the chain.
        walk(&rows, |index, depth| {
            assert_eq!(index, count);
            assert_eq!(depth, 0);
            count += 1;
        });
        assert_eq!(count, rows.len());
    }

    #[test]
    fn tree_tags_follow_ir_message_kinds_and_lineage_instead_of_tool_name_substrings() {
        let tool_use = |id: &str, name: &str| Block::ToolUse {
            id: id.into(),
            name: name.into(),
            namespace: None,
            input: serde_json::json!({}).into(),
            extensions: Default::default(),
        };
        let mut ordinary_task_tool = Message::new(Role::Assistant);
        ordinary_task_tool.content.push(tool_use("t1", "TaskCreate"));
        let line = message_line(&ordinary_task_tool, None);
        assert!(
            line.contains("🔧 tool") && !line.contains("↳ sub-agent") && line.ends_with("[TaskCreate]"),
            "non-spawn tool named TaskCreate must not be tagged as a sub-agent spawn: {line}"
        );

        let mut spawn = Message::of_kind(Role::Assistant, MessageKind::SubagentSpawn, crate::ir::Origin::Model);
        spawn.content.push(tool_use("t2", "run_worker"));
        let spawn_line = message_line(&spawn, Some("delegate".into()));
        assert!(spawn_line.contains("↳ sub-agent (run_worker)"), "{spawn_line}");

        let mut ret = Message::of_kind(Role::Tool, MessageKind::SubagentReturn, crate::ir::Origin::Subagent);
        ret.content.push(Block::ToolResult {
            tool_use_id: "t2".into(),
            content: "finished".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
            extensions: Default::default(),
        });
        let ret_line = message_line(&ret, Some("finished".into()));
        assert!(ret_line.contains("↩ sub-agent"), "{ret_line}");

        let lineage = Lineage {
            parent: Some("parent-session-12345678".into()),
            forked_from: Some("fork-origin-87654321".into()),
            agent_path: Some("worker-1".into()),
            ..Default::default()
        };
        let rendered = lineage_line(&lineage).expect("non-empty lineage");
        assert_eq!(
            rendered,
            "lineage: parent parent-s · forked from fork-ori · agent worker-1"
        );
        assert_eq!(lineage_line(&Lineage::default()), None);
    }

    #[test]
    fn final_return_streams_across_non_claude_harnesses() {
        let world = crate::test_world("forest-final-return");
        let path = world.base.join("sub.opensession.json");
        let h = Harness::parse("opensession").unwrap();
        let mut session = Session::new("sub-1", h);
        let turn = |role, text: &str| {
            let mut m = Message::new(role);
            m.content.push(Block::from_text(text.into()));
            m
        };
        session.messages = vec![
            turn(Role::User, "investigate bug"),
            turn(Role::Assistant, "checking files"),
            turn(Role::Assistant, "root cause found in parser.rs"),
            turn(Role::Assistant, " \n"),
        ];
        std::fs::write(
            &path,
            serde_json::to_vec(&crate::harness::opensession::document(&session)).unwrap(),
        )
        .unwrap();
        let r = SessionRef::new("sub-1", h, &path);
        assert_eq!(
            final_return(&r).unwrap().as_deref(),
            Some("root cause found in parser.rs")
        );
    }
}
