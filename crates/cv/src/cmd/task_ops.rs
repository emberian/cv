//! The composite task operations and read views the CLI verbs and `cv serve`'s inbox share, so the
//! page and the shell are one store: posing a decision (`opened` + `posed`, one unit), resolving
//! one, the event feed (`cv task events` ≡ `GET /api/events`), and the inbox page
//! (`cv task inbox` ≡ `GET /api/inbox`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, InboxReason, ReplayOutcome, TaskEvent, TaskEventKind, TaskReadModel, TaskStore};

use crate::util::fmt_local;

// ───────────────────────────── pose / resolve ─────────────────────────────

/// Everything a decision is opened with.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecisionSpec {
    pub title: String,
    pub body: String,
    pub for_who: String,
    pub default_choice: String,
    /// Alternatives; the default is prepended and duplicates dropped.
    pub options: Vec<String>,
    pub deadline: Option<DateTime<Utc>>,
    pub repo: Option<PathBuf>,
    pub issue: Option<String>,
    pub tags: Vec<String>,
    /// The note event this decision was posed from (an old log's `cv task split` wrote these).
    pub source: Option<String>,
    /// Tasks (full ids) this decision blocks.
    pub blocks: Vec<String>,
    /// Resolve it on its default at once, by the poser, with a veto window: a `resolved`
    /// (`provisional: true`) lands in the same unit as the pose.
    pub provisional: bool,
}

/// The option list a spec poses: the default first, then the alternatives, deduplicated.
pub(crate) fn option_list(default_choice: &str, options: &[String]) -> Vec<String> {
    let mut out = vec![default_choice.trim().to_string()];
    for o in options {
        let o = o.trim();
        if !o.is_empty() && !out.iter().any(|have| have == o) {
            out.push(o.to_string());
        }
    }
    out
}

/// The events that pose a decision, in append order: `opened` (assigned to the decider) and
/// `posed` (the options, the default, the deadline) — one creating event per decision, plus a
/// `tagged` only for the caller's own tags, one `blocked_by` per task it blocks, and the
/// provisional `resolved` when `spec.provisional`. Built, not written: [`pose`] appends them as
/// ONE unit.
pub(crate) fn pose_events(from: &str, spec: DecisionSpec) -> Result<Vec<TaskEvent>> {
    if spec.default_choice.trim().is_empty() {
        bail!("a decision needs a --default (the option that stands if nobody speaks)");
    }
    if spec.for_who.trim().is_empty() {
        bail!("a decision needs --for <who> (whose inbox it lands in)");
    }
    let options = option_list(&spec.default_choice, &spec.options);
    let default_choice = spec.default_choice.trim().to_string();
    let opened = task::new_event(
        None,
        from,
        TaskEventKind::Opened {
            title: spec.title,
            body: spec.body,
            repo: spec.repo,
            issue: spec.issue,
            channel: None,
            assignee: Some(spec.for_who.trim().to_string()),
        },
    );
    let id = opened.task_id.clone();
    let mut candidates = vec![
        opened,
        task::new_event(
            Some(&id),
            from,
            TaskEventKind::Posed {
                options,
                default_choice: default_choice.clone(),
                deadline: spec.deadline,
                source: spec.source,
            },
        ),
    ];
    let mut tags: Vec<String> = Vec::new();
    for t in &spec.tags {
        if !tags.iter().any(|have| have == t) {
            tags.push(t.clone());
        }
    }
    if !tags.is_empty() {
        candidates.push(task::new_event(Some(&id), from, TaskEventKind::Tagged { tags }));
    }
    for other in &spec.blocks {
        candidates.push(task::new_event(
            Some(other),
            from,
            TaskEventKind::BlockedBy { task: id.clone() },
        ));
    }
    if spec.provisional {
        candidates.push(task::new_event(
            Some(&id),
            from,
            TaskEventKind::Resolved {
                choice: default_choice,
                note: None,
                provisional: true,
            },
        ));
    }
    Ok(candidates)
}

/// Pose a decision with the whole candidate batch validated before writing; a rejected event
/// leaves no half-posed decision. I/O failure may leave a completed prefix. Returns append-order
/// events and advisory warnings.
pub(crate) fn pose(store: &TaskStore, from: &str, spec: DecisionSpec) -> Result<(Vec<TaskEvent>, Vec<String>)> {
    let out = task::append_events(store, pose_events(from, spec)?, Vec::new())?;
    let mut warnings = out.replay_warnings;
    warnings.extend(out.warnings);
    Ok((out.events, warnings))
}

/// What a resolution says: the literal choice, or the posed default.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Choice(String),
    AcceptDefault,
    /// Confirm a provisional resolution: the decider answers with the choice made for them.
    Confirm,
}

impl Answer {
    pub fn from_flags(choice: Option<String>, accept_default: bool, confirm: bool) -> Result<Answer> {
        if usize::from(choice.is_some()) + usize::from(accept_default) + usize::from(confirm) > 1 {
            bail!("pass only one of --choice, --accept-default, or --confirm");
        }
        match (choice, accept_default, confirm) {
            (Some(c), _, _) => Ok(Answer::Choice(c.trim().to_string())),
            (None, true, _) => Ok(Answer::AcceptDefault),
            (None, false, true) => Ok(Answer::Confirm),
            (None, false, false) => bail!("pass --choice \"<option>\", --accept-default, or --confirm"),
        }
    }
}

/// Resolve a decision (full task id) with `answer`. `from` is the resolver and is recorded.
/// `provisional`: resolve it on its default on the decider's behalf, with a veto window. The
/// rules — a posed decision, a known option, provisional only on the default — are the reducer's;
/// this only turns `--accept-default` / `--confirm` into the literal choice they stand for.
pub(crate) fn resolve(
    store: &TaskStore,
    model: &TaskReadModel,
    from: &str,
    id: &str,
    answer: Answer,
    note: Option<String>,
    provisional: bool,
) -> Result<task::AppendOutcome> {
    let t = model.tasks.get(id).with_context(|| format!("no task {id}"))?;
    let short = super::task::prefix(id, 13);
    let decision = || {
        t.decision.as_ref().with_context(|| {
            format!("task {short} is not a decision (nothing was posed) — finish it with `cv task done`")
        })
    };
    let choice = match answer {
        Answer::Choice(c) => c,
        Answer::AcceptDefault => decision()?.default_choice.clone(),
        Answer::Confirm => match decision()?.resolution.as_ref().filter(|r| r.provisional) {
            Some(r) => r.choice.clone(),
            None => bail!(
                "task {short} has no provisional resolution to confirm — answer it with --choice or --accept-default"
            ),
        },
    };
    let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    task::append(
        store,
        Some(id),
        from,
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        },
        Vec::new(),
    )
}

// ───────────────────────────── the event feed ─────────────────────────────

/// A `--since` argument, resolved: an instant, or an event id (exclusive cursor).
#[derive(Clone, Debug, Default)]
pub(crate) struct Since {
    pub ts: Option<DateTime<Utc>>,
    pub id: Option<String>,
}

impl Since {
    pub fn parse(s: Option<&str>, now: DateTime<Utc>) -> Result<Since> {
        let Some(s) = s else {
            return Ok(Since::default());
        };
        let s = s.trim();
        // An event/task id (or a ≥13-hex prefix of one) is a cursor: everything after it.
        let hex_only: String = s.chars().filter(|c| *c != '-').collect();
        let looks_like_id = hex_only.len() >= 13 && hex_only.chars().all(|c| c.is_ascii_hexdigit());
        if looks_like_id && task::uuid_v7_timestamp(s).is_some() {
            return Ok(Since {
                ts: None,
                id: Some(s.to_string()),
            });
        }
        let ts = task::parse_since(s, now).map_err(crate::util::UsageError)?;
        Ok(Since { ts: Some(ts), id: None })
    }

    fn start(&self, events: &[TaskEvent]) -> Result<usize> {
        let Some(cursor) = &self.id else { return Ok(0) };
        let needle: String = cursor
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let mut found: Option<(usize, &str)> = None;
        for (position, event) in events.iter().enumerate() {
            let matches = event
                .id
                .chars()
                .filter(|c| *c != '-')
                .map(|c| c.to_ascii_lowercase())
                .take(needle.len())
                .eq(needle.chars());
            if !matches {
                continue;
            }
            if let Some((_, previous)) = found {
                if previous == event.id.as_str() {
                    bail!("event cursor {cursor:?} occurs more than once in the log; use a timestamp bound instead");
                }
                bail!("event cursor {cursor:?} is ambiguous; pass a full event id");
            }
            found = Some((position, event.id.as_str()));
        }
        found
            .map(|(position, _)| position + 1)
            .with_context(|| format!("no task event matches cursor {cursor:?}"))
    }

    fn admits_time(&self, ev: &TaskEvent) -> bool {
        self.id.is_some() || self.ts.is_none_or(|ts| ev.ts >= ts)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EventFilter {
    pub since: Since,
    /// Wire tags (already canonicalized by [`kind_tag`]).
    pub kinds: Vec<String>,
    pub by: Option<String>,
    pub not_by: Option<String>,
    pub assignee: Option<String>,
    /// Full task id.
    pub task: Option<String>,
}

/// Every wire tag an event can carry: the kinds cv writes, then the retired ones a log may hold.
const LIVE_KIND_TAGS: [&str; 12] = [
    "opened",
    "claimed",
    "released",
    "noted",
    "done",
    "abandoned",
    "superseded",
    "tagged",
    "blocked_by",
    "posed",
    "resolved",
    "status_set",
];

/// The wire tags of a comma-separated `--kind` list (`kind_tag` on each). An unknown spelling is a
/// usage error naming the known tags, never an empty result.
pub(crate) fn kind_tags(list: &str) -> Result<Vec<String>> {
    list.split(',')
        .map(|spelling| {
            let tag = kind_tag(spelling);
            if LIVE_KIND_TAGS.contains(&tag.as_str()) || cv_core::task::RETIRED_TAGS.contains(&tag.as_str()) {
                Ok(tag)
            } else {
                crate::util::usage(format!(
                    "unknown event kind {spelling:?} (known: {}; `note`, `resolve`, `open`, `claim`, … also spell)",
                    LIVE_KIND_TAGS.join(", ")
                ))
            }
        })
        .collect()
}

/// The wire tag for a kind spelling a person would type: `note`→`noted`, `resolve`→`resolved`,
/// `open`→`opened`, `claim`→`claimed`, … Any other spelling is returned lowercased and trimmed.
fn kind_tag(s: &str) -> String {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "note" | "notes" => "noted",
        "resolve" | "resolution" | "decided" | "decide" => "resolved",
        "open" => "opened",
        "claim" => "claimed",
        "release" => "released",
        "abandon" => "abandoned",
        "supersede" => "superseded",
        "tag" => "tagged",
        "block" | "blocked" => "blocked_by",
        "pose" | "decision" => "posed",
        "status" | "status-set" => "status_set",
        other => other,
    }
    .to_string()
}

/// The events matching `f`, log order.
pub(crate) fn select_events<'a>(outcome: &'a ReplayOutcome, f: &EventFilter) -> Result<Vec<&'a TaskEvent>> {
    let start = f.since.start(&outcome.events)?;
    Ok(outcome
        .events
        .iter()
        .skip(start)
        .filter(|ev| f.since.admits_time(ev))
        .filter(|ev| f.kinds.is_empty() || f.kinds.iter().any(|k| k == ev.kind.tag()))
        .filter(|ev| f.by.as_deref().is_none_or(|b| task::same_actor(&ev.by, b)))
        .filter(|ev| !f.not_by.as_deref().is_some_and(|b| task::same_actor(&ev.by, b)))
        .filter(|ev| f.task.as_deref().is_none_or(|t| ev.task_id == t))
        .filter(|ev| {
            f.assignee.as_deref().is_none_or(|a| {
                outcome
                    .model
                    .tasks
                    .get(&ev.task_id)
                    .is_some_and(|t| t.assignee.as_deref().is_some_and(|have| task::same_actor(have, a)))
            })
        })
        .collect())
}

/// One event as a JSON line: the event's own wire fields, plus the task's `title` and current
/// effective `task_state` so a poller can act without a second lookup.
pub(crate) fn event_json(ev: &TaskEvent, model: &TaskReadModel) -> serde_json::Value {
    let mut v = serde_json::to_value(ev).expect("TaskEvent serializes");
    if let Some(obj) = v.as_object_mut() {
        if let Some(t) = model.tasks.get(&ev.task_id) {
            obj.insert("title".into(), serde_json::Value::String(t.title.clone()));
            obj.insert(
                "task_state".into(),
                serde_json::Value::String(task::effective_display(t)),
            );
            if let Some(a) = &t.assignee {
                obj.insert("assignee".into(), serde_json::Value::String(a.clone()));
            }
        }
        // The telling detail the text feed prints, shaped alike but at this feed's own widths,
        // so a page need not know every kind.
        obj.insert("detail".into(), serde_json::Value::String(event_detail(ev)));
    }
    v
}

/// The telling detail of one event, shaped like the one `event_text` prints after the kind but
/// at this feed's own widths, sanitized.
pub(crate) fn event_detail(ev: &TaskEvent) -> String {
    match &ev.kind {
        TaskEventKind::Noted { text, .. } => truncate(&sanitize_line(text), 160),
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        } => {
            let p = if *provisional { " (provisional — veto?)" } else { "" };
            match note {
                Some(n) => format!("→ {}{p} ({})", sanitize_line(choice), truncate(&sanitize_line(n), 80)),
                None => format!("→ {}{p}", sanitize_line(choice)),
            }
        }
        TaskEventKind::Posed { default_choice, .. } => format!("default: {}", sanitize_line(default_choice)),
        TaskEventKind::StatusSet { text } => format!("STATUS: {}", first_line(text, 150)),
        TaskEventKind::Opened { title, .. } => truncate(&sanitize_line(title), 120),
        TaskEventKind::Claimed { assignee } => format!("by {}", sanitize_line(assignee)),
        TaskEventKind::Done { observed, .. } => observed
            .as_deref()
            .map(|o| truncate(&sanitize_line(o), 120))
            .unwrap_or_default(),
        TaskEventKind::Tagged { tags } => tags
            .iter()
            .map(|t| format!("#{}", sanitize_line(t)))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// One event as a human line: time, kind, task, actor, and the telling detail.
pub(crate) fn event_text(ev: &TaskEvent, model: &TaskReadModel, plen: usize) -> String {
    let title = model
        .tasks
        .get(&ev.task_id)
        .map(|t| truncate(&sanitize_line(&t.title), 60))
        .unwrap_or_default();
    let detail = match &ev.kind {
        TaskEventKind::Noted { text, .. } => truncate(&sanitize_line(text), 100),
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        } => {
            let p = if *provisional { " (provisional)" } else { "" };
            match note {
                Some(n) => format!("→ {}{p} ({})", sanitize_line(choice), truncate(&sanitize_line(n), 60)),
                None => format!("→ {}{p}", sanitize_line(choice)),
            }
        }
        TaskEventKind::Done { observed, .. } => observed
            .as_deref()
            .map(|o| sanitize_line(o).to_string())
            .unwrap_or_default(),
        TaskEventKind::Abandoned { reason } => sanitize_line(reason).to_string(),
        TaskEventKind::Claimed { assignee } => sanitize_line(assignee).to_string(),
        TaskEventKind::Tagged { tags } => format!("#{}", sanitize_line(&tags.join(" #"))),
        TaskEventKind::Posed { default_choice, .. } => format!("default: {}", sanitize_line(default_choice)),
        TaskEventKind::StatusSet { text } => format!("STATUS: {}", first_line(text, 100)),
        _ => String::new(),
    };
    format!(
        "{}  {:<16} {}  {:<24} {}{}",
        fmt_local(ev.ts, "%m-%d %H:%M"),
        ev.kind.tag(),
        sanitize_line(ev.task_id.get(..plen).unwrap_or(&ev.task_id)),
        truncate(&sanitize_line(&ev.by), 24),
        title,
        if detail.is_empty() {
            String::new()
        } else {
            format!(" · {detail}")
        }
    )
}

// ───────────────────────────── the inbox, rendered ─────────────────────────────

/// The inbox's groups, in the order they are printed: a decision owed is the slowest blocker a
/// fleet has, so it comes first; then the actions handed to you; then the work you hold.
pub(crate) const INBOX_GROUPS: &[(InboxReason, &str)] = &[
    (InboxReason::DecisionOwed, "decisions owed"),
    (
        InboxReason::Discussing,
        "in discussion (parked, still yours to resolve)",
    ),
    (InboxReason::Provisional, "made for you (veto?)"),
    (InboxReason::AssignedOpen, "assigned actions"),
    (InboxReason::ClaimedByYou, "claimed work"),
];

/// The first non-empty line of a text, trimmed and capped.
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let l = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    truncate(&sanitize_line(l), max)
}

/// `cv task inbox <who>` as text.
pub(crate) fn render_inbox_text(page: &task::InboxPage) -> String {
    let mut out = String::new();
    if page.items.is_empty() {
        out.push_str(&format!("(inbox empty for {})\n", sanitize_line(&page.who)));
    }
    for (reason, label) in INBOX_GROUPS {
        let group: Vec<&task::InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("{label} ({}):\n", group.len()));
        for i in group {
            out.push_str(&format!(
                "{} {}  {:>4}  {:10} {}\n",
                if i.stale { "⏰" } else { "  " },
                sanitize_line(&i.short),
                i.age,
                i.effective_state,
                sanitize_line(&i.title)
            ));
            if let Some(d) = &i.decision {
                if let Some(r) = d.resolution.as_ref().filter(|r| r.provisional) {
                    let mut line = format!("⇒ made: {} — by {}", sanitize_line(&r.choice), sanitize_line(&r.by));
                    if !d.alternatives.is_empty() {
                        line.push_str(&format!(
                            " · veto to: {}",
                            d.alternatives
                                .iter()
                                .map(|a| sanitize_line(a))
                                .collect::<Vec<_>>()
                                .join(" / ")
                        ));
                    }
                    out.push_str(&format!(
                        "{:width$}{}\n",
                        "",
                        truncate(&line, 200),
                        width = i.short.len() + 3
                    ));
                    continue;
                }
                let mut line = format!("⇒ default: {}", sanitize_line(&d.default_choice));
                if !d.alternatives.is_empty() {
                    line.push_str(&format!(
                        " · alt: {}",
                        d.alternatives
                            .iter()
                            .map(|a| sanitize_line(a))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ));
                }
                if let Some(p) = &d.deadline_phrase {
                    line.push_str(&format!(" · {p}"));
                }
                out.push_str(&format!(
                    "{:width$}{}\n",
                    "",
                    truncate(&line, 200),
                    width = i.short.len() + 3
                ));
            }
        }
    }
    if page.hidden > 0 {
        out.push_str(&format!(
            "({} older item(s) hidden — `--all`, or `--since 90d`)\n",
            page.hidden
        ));
    }
    if page.counts.decisions > 0 {
        out.push_str(&format!(
            "resolve: cv task resolve <id> --accept-default --from {who} · or --choice \"<option>\" · note: cv task note <id> -\n",
            who = sanitize_line(&page.who)
        ));
    }
    if page.counts.provisional > 0 {
        out.push_str(&format!(
            "made for you: cv task resolve <id> --confirm --from {who} · or veto with --choice \"<option>\"\n",
            who = sanitize_line(&page.who)
        ));
    }
    out
}

/// `cv task inbox <who> --md`: the whole inbox as one Markdown page — every item's body and the
/// first line of every note, ready to paste anywhere.
pub(crate) fn render_inbox_md(page: &task::InboxPage) -> String {
    let mut out = String::new();
    let who = sanitize_line(&page.who);
    out.push_str(&format!(
        "# Inbox for {who} — {}\n\n",
        fmt_local(page.now, "%Y-%m-%d %H:%M")
    ));
    let c = &page.counts;
    out.push_str(&format!(
        "{} decision(s) owed · {} assigned action(s) · {} claimed · {} unread",
        c.decisions, c.assigned, c.claimed, c.unread
    ));
    if c.provisional > 0 {
        out.push_str(&format!(" · {} made for you (veto?)", c.provisional));
    }
    if page.hidden > 0 {
        out.push_str(&format!(" · {} older hidden (`--all`)", page.hidden));
    }
    out.push_str("\n\n");
    if page.items.is_empty() {
        out.push_str("_Nothing waits on you._\n");
    }
    let mut n = 0;
    for (reason, label) in INBOX_GROUPS {
        let group: Vec<&task::InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("## {} ({})\n\n", capitalize(label), group.len()));
        for i in group {
            n += 1;
            out.push_str(&format!("### {n}. {}\n\n", md_inline(&i.title)));
            let mut facts = vec![
                format!("`{}`", md_inline(&i.short)),
                i.effective_state.clone(),
                format!(
                    "{} {} ago by {}",
                    if i.decision.is_some() { "asked" } else { "waiting" },
                    i.age,
                    md_inline(&i.opened_by)
                ),
            ];
            if i.unread {
                facts.push(format!("**unread** (last: {})", md_inline(&i.last_by)));
            }
            if i.blocked {
                facts.push("blocked".into());
            }
            if let Some(d) = &i.decision {
                if let Some(p) = &d.deadline_phrase {
                    facts.push(p.clone());
                }
            }
            out.push_str(&facts.join(" · "));
            out.push_str("\n\n");
            if let Some(d) = &i.decision {
                out.push_str(&format!("- **default:** {}\n", md_inline(&d.default_choice)));
                for a in &d.alternatives {
                    out.push_str(&format!("- alternative: {}\n", md_inline(a)));
                }
                match d.resolution.as_ref().filter(|r| r.provisional) {
                    Some(r) => out.push_str(&format!(
                        "- **made for you:** {} — by {} · confirm: `cv task resolve {} --confirm --from {who}` · or veto with `--choice`\n",
                        md_inline(&r.choice),
                        md_inline(&r.by),
                        md_inline(&i.short)
                    )),
                    None => out.push_str(&format!(
                        "- resolve: `cv task resolve {} --accept-default --from {who}`\n",
                        md_inline(&i.short)
                    )),
                }
                out.push('\n');
            }
            if let Some(body) = i.body.as_deref().filter(|body| !body.trim().is_empty()) {
                for para in body.trim().split("\n\n") {
                    out.push_str(&md_block(para));
                    out.push_str("\n\n");
                }
            }
            if i.notes.as_ref().is_some_and(|notes| !notes.is_empty()) {
                let notes = i.notes.as_ref().unwrap();
                out.push_str(&format!("notes ({}):\n\n", notes.len()));
                for note in notes {
                    let mut line = format!(
                        "- _{}, {}{}:_ {}",
                        md_inline(&note.by),
                        fmt_local(note.ts, "%m-%d %H:%M"),
                        if note.post_close { " (after close)" } else { "" },
                        md_inline(&first_line(&note.text, 200))
                    );
                    if let Some(child) = &note.split_into {
                        line.push_str(&format!(" → split into `{}`", child.get(..13).unwrap_or(child)));
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Inline Markdown-safe text: terminal-sanitized, pipes/newlines flattened; backticks kept (the
/// titles are full of code spans and a reader wants them rendered).
fn md_inline(s: &str) -> String {
    sanitize_line(s).replace('\n', " ").replace('|', "\\|")
}

/// A body paragraph: sanitized line by line, kept as written.
fn md_block(s: &str) -> String {
    s.lines()
        .map(|l| sanitize_line(l.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_text_sanitizes_record_identifiers_and_tags_while_json_keeps_wire_values() {
        let mut event = task::new_event(
            Some("ta\nsk\x1b[31m"),
            "author",
            TaskEventKind::Tagged {
                tags: vec!["danger\x1b[31mcolor\x1b[0m".into()],
            },
        );
        event.ts = "2026-10-01T12:00:00Z".parse().unwrap();
        let model = TaskReadModel::default();
        let text = event_text(&event, &model, usize::MAX);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("ta sk") && text.contains("#dangercolor") && !text.contains('\x1b'));
        let json = event_json(&event, &model);
        assert_eq!(json["task_id"], event.task_id);
        assert_eq!(json["tags"][0], "danger\x1b[31mcolor\x1b[0m");
    }

    #[test]
    fn inbox_brief_projection_preserves_full_text_without_cloning_rich_payloads() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let body = "large payload 🌙".repeat(8192);
        let opened = task::new_event(
            None,
            "orchestrator",
            TaskEventKind::Opened {
                title: "brief action".into(),
                body: body.clone(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("ember".into()),
            },
        );
        let action_id = opened.task_id.clone();
        let decision = task::new_event(
            None,
            "orchestrator",
            TaskEventKind::Opened {
                title: "brief decision".into(),
                body: body.clone(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("ember".into()),
            },
        );
        let decision_id = decision.task_id.clone();
        let events = vec![
            opened,
            task::new_event(
                Some(&action_id),
                "orchestrator",
                TaskEventKind::Noted {
                    text: body.clone(),
                    session_ref: None,
                },
            ),
            task::new_event(
                Some(&action_id),
                "orchestrator",
                TaskEventKind::StatusSet { text: body.clone() },
            ),
            decision,
            task::new_event(
                Some(&decision_id),
                "orchestrator",
                TaskEventKind::Posed {
                    options: vec!["keep".into(), "change".into()],
                    default_choice: "keep".into(),
                    deadline: None,
                    source: None,
                },
            ),
            task::new_event(
                Some(&decision_id),
                "orchestrator",
                TaskEventKind::Resolved {
                    choice: "keep".into(),
                    note: Some(body.clone()),
                    provisional: true,
                },
            ),
        ];
        let outcome = ReplayOutcome {
            model: task::TaskReducer::reduce(&events).unwrap(),
            events,
            warnings: Vec::new(),
            quarantined: 0,
        };
        for unread in [false, true] {
            let full = task::inbox_page(&outcome, "ember", None, None, unread, now);
            let brief = task::inbox_page_brief(&outcome, "ember", None, None, unread, now);
            assert_eq!(render_inbox_text(&brief), render_inbox_text(&full));
            assert_eq!(
                serde_json::to_value(&brief.counts).unwrap(),
                serde_json::to_value(&full.counts).unwrap()
            );
            assert!(brief
                .items
                .iter()
                .all(|row| row.body.is_none() && row.notes.is_none() && row.status.is_none()));
            assert!(brief
                .items
                .iter()
                .filter_map(|row| row.decision.as_ref())
                .filter_map(|d| d.resolution.as_ref())
                .all(|r| r.note.is_none()));
            assert!(full.items.iter().any(|row| {
                row.body.as_deref() == Some(body.as_str()) && row.status.as_deref() == Some(body.as_str())
            }));
        }
        assert_eq!(outcome.model.tasks[&action_id].body, body);
        assert_eq!(outcome.model.tasks[&action_id].notes[0].text, body);
    }

    #[test]
    fn event_assignee_filters_share_web_actor_aliases() {
        let opened = task::new_event(
            None,
            "orchestrator",
            TaskEventKind::Opened {
                title: "assigned".into(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("web:ember".into()),
            },
        );
        let outcome = ReplayOutcome {
            model: task::TaskReducer::reduce(std::slice::from_ref(&opened)).unwrap(),
            events: vec![opened],
            warnings: Vec::new(),
            quarantined: 0,
        };
        for who in ["ember", "web:ember"] {
            assert_eq!(
                select_events(
                    &outcome,
                    &EventFilter {
                        assignee: Some(who.into()),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                1
            );
        }
        assert!(select_events(
            &outcome,
            &EventFilter {
                assignee: Some("lane:ember".into()),
                ..Default::default()
            }
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn decision_answer_flags_refuse_conflicting_choices() {
        for (choice, accept, confirm) in [
            (Some("keep".into()), true, false),
            (Some("keep".into()), false, true),
            (None, true, true),
            (Some("keep".into()), true, true),
        ] {
            assert!(Answer::from_flags(choice, accept, confirm)
                .unwrap_err()
                .to_string()
                .contains("only one"));
        }
        assert!(
            matches!(Answer::from_flags(Some(" keep ".into()), false, false).unwrap(), Answer::Choice(c) if c == "keep")
        );
        assert!(matches!(
            Answer::from_flags(None, true, false).unwrap(),
            Answer::AcceptDefault
        ));
        assert!(matches!(
            Answer::from_flags(None, false, true).unwrap(),
            Answer::Confirm
        ));
        assert!(Answer::from_flags(None, false, false).is_err());
    }

    #[test]
    fn kind_spellings_map_to_wire_tags() {
        assert_eq!(kind_tag("note"), "noted");
        assert_eq!(kind_tag("Resolve"), "resolved");
        assert_eq!(kind_tag("done"), "done");
        assert_eq!(
            kind_tags("note,Resolve,landed").unwrap(),
            ["noted", "resolved", "landed"]
        );
        let err = kind_tags("note,bogus").unwrap_err();
        assert!(err.downcast_ref::<crate::util::UsageError>().is_some());
        assert!(
            err.to_string().contains("\"bogus\"") && err.to_string().contains("status_set"),
            "{err}"
        );
    }

    #[test]
    fn event_cursors_follow_log_order_exclude_prefix_matches_and_refuse_ambiguous_positions() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let ids = [
            "01a0f000-0000-7000-8000-000000000001",
            "01a0f100-0000-7000-8000-000000000002",
            "01a0e000-0000-7000-8000-000000000003",
        ];
        let mut events = Vec::new();
        for (index, id) in ids.into_iter().enumerate() {
            let kind = if index == 0 {
                TaskEventKind::Opened {
                    title: "polling".into(),
                    body: String::new(),
                    repo: None,
                    issue: None,
                    channel: None,
                    assignee: None,
                }
            } else {
                TaskEventKind::Noted {
                    text: format!("note {index}"),
                    session_ref: None,
                }
            };
            events.push(TaskEvent {
                id: id.into(),
                task_id: ids[0].into(),
                by: "author".into(),
                ts: now + chrono::Duration::seconds(index as i64),
                kind,
            });
        }
        let mut outcome = ReplayOutcome {
            model: task::TaskReducer::reduce(&events).unwrap(),
            events,
            warnings: Vec::new(),
            quarantined: 0,
        };
        let filter = |cursor: &str| EventFilter {
            since: Since::parse(Some(cursor), now).unwrap(),
            ..Default::default()
        };
        for cursor in [
            ids[1].to_string(),
            ids[1].replace('-', "").to_uppercase(),
            "01a0f10000007".into(),
        ] {
            let got = select_events(&outcome, &filter(&cursor)).unwrap();
            assert_eq!(
                got.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
                [ids[2]],
                "{cursor}"
            );
        }
        let got = select_events(
            &outcome,
            &EventFilter {
                since: Since {
                    ts: Some(now + chrono::Duration::seconds(2)),
                    id: None,
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(), [ids[2]]);
        let got = select_events(
            &outcome,
            &EventFilter {
                kinds: vec!["noted".into()],
                ..filter(ids[0])
            },
        )
        .unwrap();
        assert_eq!(got.len(), 2, "cursor resolution precedes kind filtering");
        assert!(select_events(&outcome, &filter("01a0f20000007"))
            .unwrap_err()
            .to_string()
            .contains("no task event"));
        let mut extra = outcome.events[1].clone();
        extra.id = "01a0f000-0000-7000-8000-000000000004".into();
        outcome.events.push(extra);
        assert!(select_events(&outcome, &filter("01a0f00000007"))
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        outcome.events.pop();
        outcome.events.push(outcome.events[1].clone());
        assert!(select_events(&outcome, &filter(ids[1]))
            .unwrap_err()
            .to_string()
            .contains("more than once"));
    }

    #[test]
    fn since_reads_ids_as_exclusive_cursors_and_times_as_inclusive() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let s = Since::parse(Some("01a0f54c-d1dd-72e2-8b19-e99f146df6ff"), now).unwrap();
        assert!(s.id.is_some() && s.ts.is_none());
        let s = Since::parse(Some("2h"), now).unwrap();
        assert_eq!(s.ts, Some(now - chrono::Duration::hours(2)));
        let s = Since::parse(Some("2026-10-01T10:00:00Z"), now).unwrap();
        assert_eq!(s.ts, Some("2026-10-01T10:00:00Z".parse().unwrap()));
        assert!(Since::parse(Some("yesterday-ish"), now).is_err());
    }

    /// The inbox's text, pinned whole: decisions first with the default and the alternative on
    /// one line, a provisional answer in its own group with the veto spelled out, then actions,
    /// then claimed work, then the two copy-paste lines; and the empty inbox names its owner.
    #[test]
    fn inbox_text_groups_decisions_first_and_names_the_resolve_lines() {
        let ev = |n: u8, task: u8, ts: &str, by: &str, kind: serde_json::Value| -> task::TaskEvent {
            let mut v = serde_json::json!({
                "id": format!("01a0f000-0000-7000-8000-0000000000{n:02x}"),
                "task_id": format!("01a0f000-0000-7000-8000-0000000000{task:02x}"),
                "ts": ts, "by": by,
            });
            for (k, x) in kind.as_object().unwrap() {
                v[k] = x.clone();
            }
            serde_json::from_value(v).unwrap()
        };
        let o = "orchestrator:o";
        let events = vec![
            ev(
                1,
                1,
                "2026-10-01T09:00:00Z",
                o,
                serde_json::json!({"event": "opened", "title": "K-PORTAL: who births the guest cell", "assignee": "ember"}),
            ),
            ev(
                2,
                1,
                "2026-10-01T09:00:00Z",
                o,
                serde_json::json!({"event": "posed", "options": ["the concierge births it", "the receiver allocates it"], "default": "the concierge births it", "deadline": "2026-10-04T12:00:00Z"}),
            ),
            ev(
                3,
                3,
                "2026-10-01T10:00:00Z",
                o,
                serde_json::json!({"event": "opened", "title": "build base: /tank or NVMe", "assignee": "ember"}),
            ),
            ev(
                4,
                3,
                "2026-10-01T10:00:00Z",
                o,
                serde_json::json!({"event": "posed", "options": ["keep /tank", "move to NVMe"], "default": "keep /tank"}),
            ),
            ev(
                5,
                3,
                "2026-10-01T10:00:00Z",
                o,
                serde_json::json!({"event": "resolved", "choice": "keep /tank", "provisional": true}),
            ),
            ev(
                6,
                6,
                "2026-10-01T11:00:00Z",
                o,
                serde_json::json!({"event": "opened", "title": "write the docs", "assignee": "ember"}),
            ),
            ev(
                7,
                7,
                "2026-10-01T11:30:00Z",
                "cv",
                serde_json::json!({"event": "opened", "title": "the claimed one"}),
            ),
            ev(
                8,
                7,
                "2026-10-01T11:30:00Z",
                "ember",
                serde_json::json!({"event": "claimed", "assignee": "ember"}),
            ),
        ];
        let mut r = cv_core::task::TaskReducer::new();
        for e in &events {
            r.apply(e).unwrap();
        }
        let outcome = cv_core::task::ReplayOutcome {
            model: r.model().clone(),
            events,
            warnings: Vec::new(),
            quarantined: 0,
        };
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let page = task::inbox_page(&outcome, "ember", None, None, false, now);
        // The deadline renders in local time; the phrase's own unit test pins its shape.
        let by = task::deadline_phrase("2026-10-04T12:00:00Z".parse().unwrap(), now);
        let pad = " ".repeat(39);
        let want = format!(
            "decisions owed (1):\n\
             \x20  01a0f000-0000-7000-8000-000000000001    3h  open       K-PORTAL: who births the guest cell\n\
             {pad}⇒ default: the concierge births it · alt: the receiver allocates it · {by}\n\
             made for you (veto?) (1):\n\
             \x20  01a0f000-0000-7000-8000-000000000003    2h  resolved   build base: /tank or NVMe\n\
             {pad}⇒ made: keep /tank — by orchestrator:o · veto to: move to NVMe\n\
             assigned actions (1):\n\
             \x20  01a0f000-0000-7000-8000-000000000006    1h  open       write the docs\n\
             claimed work (1):\n\
             \x20  01a0f000-0000-7000-8000-000000000007   30m  claimed    the claimed one\n\
             resolve: cv task resolve <id> --accept-default --from ember · or --choice \"<option>\" · note: cv task note <id> -\n\
             made for you: cv task resolve <id> --confirm --from ember · or veto with --choice \"<option>\"\n"
        );
        assert_eq!(render_inbox_text(&page), want);

        let empty = task::inbox_page(&outcome, "nobody", None, None, false, now);
        assert_eq!(render_inbox_text(&empty), "(inbox empty for nobody)\n");
    }

    #[test]
    fn option_lists_put_the_default_first_and_deduplicate() {
        assert_eq!(
            option_list(" keep ", &["delete".into(), "keep".into(), "".into(), "delete".into()]),
            vec!["keep", "delete"]
        );
    }
}
