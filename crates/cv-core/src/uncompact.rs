//! A recovery cut in the parent's original message coordinates. Only recorded
//! parent deliveries enter the selected history. Child-only evidence travels separately.
use crate::harness::lifecycle::{Target, ToolResult as ResultEvidence};
use crate::ir::{Block, Message, MessageKind, Origin, Session, SessionRef};
use crate::tools::classify::{visit_tool_ops, ClassifyOpts, ToolOp};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize)]
pub struct ExcludedPrompt {
    pub message_index: usize,
    pub record_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoveredChild {
    pub child: String,
    pub launch: String,
    pub status: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub source_id: String,
    pub pre_compaction: usize,
    pub boundary_message: usize,
    pub boundary_record: Option<String>,
    pub messages: BTreeSet<usize>,
    pub grafts: Vec<usize>,
    pub excluded_prompts: Vec<ExcludedPrompt>,
    pub recovered_children: Vec<RecoveredChild>,
    pub open_calls: BTreeSet<String>,
    pub undelivered_jobs: BTreeMap<String, String>,
}

fn text(s: &Session, m: &Message) -> Result<String> {
    let resolver = s.resolver();
    let mut parts = Vec::new();
    for block in &m.content {
        if let Block::Text { text, .. } = block {
            parts.push(text.try_resolve(&resolver)?);
        }
    }
    Ok(parts.join("\n"))
}

impl Plan {
    pub fn select(reference: &SessionRef, session: &Session, ordinal: Option<usize>) -> Result<Self> {
        use MessageKind::*;
        let branch = crate::dataset::select_active_branch_indices(&session.messages, true);
        let boundaries: Vec<_> = branch
            .iter()
            .map(|&i| (i, &session.messages[i]))
            .filter(|(_, m)| m.kind == CompactionBoundary)
            .collect();
        let n = ordinal.unwrap_or(boundaries.len());
        let &(boundary, marker) = n
            .checked_sub(1)
            .and_then(|i| boundaries.get(i))
            .with_context(|| format!("cannot select compaction {n} of {}", boundaries.len()))?;
        let mut plan = Self {
            source_id: session.id.clone(),
            pre_compaction: n,
            boundary_message: boundary,
            boundary_record: marker.id.clone().or_else(|| {
                session
                    .messages
                    .get(boundary + 1)
                    .filter(|m| m.kind == CompactionSummary)
                    .and_then(|m| m.id.clone())
            }),
            messages: Default::default(),
            grafts: Vec::new(),
            excluded_prompts: Vec::new(),
            recovered_children: Vec::new(),
            open_calls: Default::default(),
            undelivered_jobs: Default::default(),
        };
        for (i, m) in session.messages.iter().enumerate().skip(boundary + 1) {
            if m.kind == Prompt && m.origin == Origin::Human {
                plan.excluded_prompts.push(ExcludedPrompt {
                    message_index: i,
                    record_id: m.id.clone(),
                    text: text(session, m)?,
                });
            }
        }
        let adapter = crate::harness::rootless(reference.harness).context("missing lifecycle adapter")?;
        let resolver = session.resolver();
        let mut seen = BTreeSet::new();
        let mut calls = BTreeMap::new();
        let mut pending = BTreeMap::new();
        let mut after_boundary = false;
        let mut parallel = BTreeSet::new();
        let mut spawn_calls = BTreeSet::new();
        let mut delivered = BTreeSet::new();
        let mut active_calls = BTreeSet::new();
        for i in branch {
            let m = &session.messages[i];
            if matches!(m.kind, CompactionBoundary | CompactionSummary) {
                after_boundary = true;
                continue;
            }
            if m.origin == Origin::Hook && m.kind == InjectedContext {
                let attachment = m.harness_extra(session.harness).and_then(|v| v.get("attachment"));
                let event = attachment.and_then(|v| v.get("hookEvent")).and_then(|v| v.as_str());
                let name = attachment.and_then(|v| v.get("hookName")).and_then(|v| v.as_str());
                if event == Some("PostCompact")
                    || name == Some("SessionStart:compact")
                    || (after_boundary && event.is_none() && name.is_none())
                {
                    continue;
                }
            }
            after_boundary = false;
            let prefix = i < boundary;
            // A producer may repeat a preserved segment. Missing record IDs cannot
            // establish identity, even when two turns have identical text.
            if prefix && m.id.as_ref().is_some_and(|id| !seen.insert((m.kind.as_str(), id))) {
                continue;
            }
            let mut keep = prefix;
            if matches!(m.kind, Prompt | Reply) {
                parallel.clear();
            }
            let closing_group = !m.content.is_empty()
                && m.content.iter().all(|b| {
                    matches!(b,
                Block::ToolResult{ tool_use_id,.. } if parallel.contains(tool_use_id))
                });

            for b in &m.content {
                match b {
                    Block::ToolUse { id, name, input, .. } if prefix => {
                        let mut ops = Vec::new();
                        input.with_view(|view| {
                            visit_tool_ops(Some(session.harness), name, view, ClassifyOpts::COUNTS, |op| {
                                ops.push(op.into_owned())
                            })
                        })?;
                        if ops.iter().any(|op| matches!(op.op, ToolOp::SubagentSpawn { .. })) {
                            spawn_calls.insert(id.as_str());
                        }
                        calls.insert(id.as_str(), (name.as_str(), ops));
                        parallel.insert(id.clone());
                        active_calls.insert(id.clone());
                        plan.open_calls.insert(id.clone());
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        tool_name,
                        status,
                        is_error,
                        details,
                        ..
                    } => {
                        let closes = prefix || closing_group;
                        if closes {
                            plan.open_calls.remove(tool_use_id);
                            parallel.remove(tool_use_id);
                            keep = true;
                        }
                        if !closes && pending.is_empty() {
                            continue;
                        }
                        let call = calls.get(tool_use_id.as_str());
                        let observation = adapter.tool_result_lifecycle(&ResultEvidence {
                            message: Some(m),
                            tool: tool_name.as_deref().or_else(|| call.map(|c| c.0)).unwrap_or(""),
                            body: &content.try_resolve(&resolver)?,
                            ops: call.map_or(&[], |c| c.1.as_slice()),
                            status: status.as_deref(),
                            is_error: *is_error,
                            details: details.as_ref(),
                        });
                        for job in observation.started_jobs {
                            if closes {
                                delivered.remove(&job);
                                pending.insert(job, tool_use_id.clone());
                            } else {
                                pending.remove(&job);
                                delivered.insert(job);
                            }
                        }
                        if !closes {
                            continue;
                        }
                        if observation.status.is_terminal() {
                            active_calls.remove(tool_use_id);
                        }
                        for update in observation.updates {
                            if let Target::Job(job) = update.target {
                                if update.status.is_terminal() {
                                    pending.remove(&job);
                                    delivered.insert(job);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if matches!(m.kind, InjectedContext | SubagentReturn | Carrier) {
                for update in adapter.message_lifecycle(m, &text(session, m)?).updates {
                    if !update.status.is_terminal() {
                        continue;
                    }
                    let matched = match update.target {
                        Target::Job(id) => {
                            let matched = pending.remove(&id).is_some();
                            if matched {
                                delivered.insert(id);
                            }
                            matched
                        }
                        Target::Call(id) => active_calls.remove(&id),
                    };
                    keep |= matched;
                }
            }
            if keep {
                plan.messages.insert(i);
                if !prefix {
                    plan.grafts.push(i);
                }
            }
        }
        if !pending.is_empty() || !spawn_calls.is_empty() {
            for child in crate::subagent_tree_of(reference)? {
                if delivered.contains(child.agent_id()) {
                    continue;
                }
                let Some(launch) = pending
                    .get(child.agent_id())
                    .map(String::as_str)
                    .or_else(|| child.tool_use_id.as_deref().filter(|id| spawn_calls.contains(id)))
                else {
                    continue;
                };
                let mut child_reads = crate::source::Probe::default()
                    .snapshot(&child.session)
                    .all_reads()
                    .context("unable to capture child revision")?;
                child_reads.record(&child.session.path);
                let status = child.result_status.as_deref().unwrap_or("");
                let (status, body) = if matches!(
                    status,
                    "done" | "completed" | "failed" | "killed" | "stopped" | "partial"
                ) {
                    (status, child.result_summary.unwrap_or_default())
                } else if status != "unavailable"
                    && adapter
                        .subagent_end(&child.session.path)?
                        .is_some_and(|end| end.stopped() == Some(true) && end.parked_status.is_none())
                {
                    let s = adapter.parse(&child.session)?;
                    let body = s
                        .messages
                        .last()
                        .filter(|m| m.kind == Reply)
                        .map(|m| text(&s, m))
                        .transpose()?
                        .unwrap_or_default();
                    ("stopped", body)
                } else {
                    continue;
                };
                ensure!(
                    child_reads.fresh(&mut Default::default()),
                    "child changed during recovery: {}",
                    child.session.id
                );
                plan.recovered_children.push(RecoveredChild {
                    child: child.session.id,
                    launch: launch.to_owned(),
                    status: status.into(),
                    text: body,
                });
            }
        }
        plan.undelivered_jobs = pending;
        Ok(plan)
    }

    pub fn apply(&self, s: &mut Session) {
        let mut i = 0;
        s.messages.retain(|_| {
            let keep = self.messages.contains(&i);
            i += 1;
            keep
        });
    }

    pub fn report(&self) -> String {
        use std::fmt::Write;
        let mut h = format!(
            "  compaction {} · {} original messages selected · {} late records\n",
            self.pre_compaction,
            self.messages.len(),
            self.grafts.len()
        );
        for p in &self.excluded_prompts {
            let _ = writeln!(
                h,
                "  excluded human prompt at message {} (record {}): {}",
                p.message_index,
                p.record_id.as_deref().unwrap_or("?"),
                crate::ir::truncate(&p.text, 96)
            );
        }
        for child in &self.recovered_children {
            let _ = writeln!(h, "  recovered from child {} ({})", child.child, child.status);
        }
        h
    }

    pub fn ensure_closed(&self) -> Result<()> {
        ensure!(
            self.open_calls.is_empty(),
            "uncompact cannot close these calls before the next turn: {:?}",
            self.open_calls
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Role};
    #[test]
    fn compaction_uses_the_boundary_on_the_retained_branch() -> Result<()> {
        let mut s = Session::new("branches", Harness::Claude);
        let r = SessionRef::new("branches", Harness::Claude, "branches.jsonl");
        let node = |kind, id: &str, parent: Option<&str>| {
            let mut m = Message::synthesized(kind, Origin::Harness);
            m.id = Some(id.to_owned());
            m.parent_id = parent.map(str::to_owned);
            m
        };
        s.messages = vec![
            node(MessageKind::Reply, "root", None),
            node(MessageKind::CompactionBoundary, "fork", Some("root")),
            node(MessageKind::Reply, "main", Some("root")),
            node(MessageKind::CompactionBoundary, "cut", Some("main")),
        ];
        let p = Plan::select(&r, &s, None)?;
        assert_eq!(p.pre_compaction, 1);
        assert_eq!(p.boundary_record.as_deref(), Some("cut"));
        assert_eq!(p.messages, [0, 2].into());
        Ok(())
    }

    #[test]
    fn compaction_closes_parallel_results_only_before_a_new_turn() -> Result<()> {
        // #tooth: an unrelated result cannot hitch a ride in a selected record.
        let mut s = Session::new("calls", Harness::Claude);
        let r = SessionRef::new("calls", Harness::Claude, "calls.jsonl");
        let mut calls = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
        for id in ["left", "right"] {
            calls.content.push(Block::ToolUse {
                id: id.into(),
                name: "Read".into(),
                namespace: None,
                input: serde_json::json!({}).into(),
                extensions: Default::default(),
            });
        }
        let result = |id: &str| {
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content.push(Block::ToolResult {
                tool_use_id: id.into(),
                content: "done".into(),
                is_error: false,
                tool_name: Some("Read".into()),
                status: None,
                details: None,
                extensions: Default::default(),
            });
            m
        };
        s.messages = vec![
            calls,
            result("left"),
            Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness),
            result("right"),
        ];
        let p = Plan::select(&r, &s, None)?;
        p.ensure_closed()?;
        assert_eq!(p.grafts, [3]);
        s.messages[3].content.extend(result("stray").content);
        assert!(Plan::select(&r, &s, None)?.ensure_closed().is_err());
        s.messages[3] = result("right");
        s.messages
            .insert(3, Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human));
        let crossed = Plan::select(&r, &s, None)?;
        assert!(crossed.grafts.is_empty());
        assert!(crossed.ensure_closed().is_err());
        Ok(())
    }
}
