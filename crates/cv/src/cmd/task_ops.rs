//! The composite task operations and read views the CLI verbs and `cv task serve` share, so the
//! page and the shell are one store: posing a decision (`opened` + `tagged` + `posed`), resolving
//! one, splitting `DECIDE` notes into decisions, the event feed (`cv task events` ≡
//! `GET /api/events`), and the inbox page (`cv task inbox` ≡ `GET /api/inbox`).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{
    self, InboxReason, ReplayOutcome, TaskEvent, TaskEventKind, TaskProjection, TaskReadModel, TaskStore,
};
use serde::Serialize;

use crate::util::fmt_local;

// ───────────────────────────── pose / resolve / split ─────────────────────────────

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
    pub channel: String,
    pub tags: Vec<String>,
    /// The note event this decision was split from.
    pub source: Option<String>,
    /// Tasks (full ids) this decision blocks.
    pub blocks: Vec<String>,
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

/// Pose a decision: three events (`opened`, `tagged decision`, `posed`) plus one `blocked_by` per
/// task it blocks. Returns the events in append order and the advisory warnings.
pub(crate) fn pose(store: &TaskStore, from: &str, spec: DecisionSpec) -> Result<(Vec<TaskEvent>, Vec<String>)> {
    if spec.default_choice.trim().is_empty() {
        bail!("a decision needs a --default (the option that stands if nobody speaks)");
    }
    if spec.for_who.trim().is_empty() {
        bail!("a decision needs --for <who> (whose inbox it lands in)");
    }
    let options = option_list(&spec.default_choice, &spec.options);
    let mut tags = vec![task::DECISION_TAG.to_string()];
    for t in &spec.tags {
        if !tags.iter().any(|have| have == t) {
            tags.push(t.clone());
        }
    }
    let channel = if spec.channel.is_empty() {
        "tasks".to_string()
    } else {
        spec.channel.clone()
    };
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    let mut push = |task_id: Option<&str>, kind: TaskEventKind| -> Result<String> {
        let out = task::append_and_notify(store, task_id, from, kind, Vec::new())?;
        warnings.extend(out.replay_warnings);
        warnings.extend(out.warnings);
        let id = out.event.task_id.clone();
        events.push(out.event);
        Ok(id)
    };
    let id = push(
        None,
        TaskEventKind::Opened {
            title: spec.title.clone(),
            body: spec.body.clone(),
            repo: spec.repo.clone(),
            issue: spec.issue.clone(),
            channel,
            assignee: Some(spec.for_who.trim().to_string()),
        },
    )?;
    push(Some(&id), TaskEventKind::Tagged { tags })?;
    push(
        Some(&id),
        TaskEventKind::Posed {
            options,
            default_choice: spec.default_choice.trim().to_string(),
            deadline: spec.deadline,
            source: spec.source.clone(),
        },
    )?;
    for other in &spec.blocks {
        push(Some(other), TaskEventKind::BlockedBy { task: id.clone() })?;
    }
    Ok((events, warnings))
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
        match (choice, accept_default, confirm) {
            (Some(c), _, _) => Ok(Answer::Choice(c.trim().to_string())),
            (None, true, _) => Ok(Answer::AcceptDefault),
            (None, false, true) => Ok(Answer::Confirm),
            (None, false, false) => bail!("pass --choice \"<option>\", --accept-default, or --confirm"),
        }
    }
}

/// Resolve a decision (full task id) with `answer`. `from` is the resolver and is recorded;
/// `token` is the TOFU credential for it. `provisional`: resolve it on its default on the
/// decider's behalf, with a veto window (the reducer refuses a provisional non-default choice).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve(
    store: &TaskStore,
    model: &TaskReadModel,
    from: &str,
    token: Option<String>,
    id: &str,
    answer: Answer,
    note: Option<String>,
    provisional: bool,
) -> Result<task::AppendOutcome> {
    let t = model.tasks.get(id).with_context(|| format!("no task {id}"))?;
    let short = &id[..13.min(id.len())];
    let Some(d) = t.decision.as_ref() else {
        bail!("task {short} is not a decision (nothing was posed) — finish it with `cv task done`");
    };
    let choice = match answer {
        Answer::Choice(c) => c,
        Answer::AcceptDefault => d.default_choice.clone(),
        Answer::Confirm => match d.resolution.as_ref().filter(|r| r.provisional) {
            Some(r) => r.choice.clone(),
            None => bail!("task {short} has no provisional resolution to confirm — answer it with --choice or --accept-default"),
        },
    };
    if provisional && choice != d.default_choice {
        bail!(
            "a provisional resolution stands on the default ({:?}); only the decider chooses otherwise",
            d.default_choice
        );
    }
    if !d.options.iter().any(|o| o == &choice) {
        // Be helpful before the reducer refuses: a case-insensitive / prefix match is almost
        // always the intended option; name it instead of guessing.
        let near: Vec<&str> = d
            .options
            .iter()
            .filter(|o| {
                o.eq_ignore_ascii_case(&choice) || o.to_lowercase().starts_with(&choice.to_lowercase())
            })
            .map(String::as_str)
            .collect();
        if let [one] = near.as_slice() {
            bail!("{choice:?} is not an option as written — did you mean --choice {one:?}?");
        }
        bail!(
            "{choice:?} is not one of the posed options:\n{}",
            d.options
                .iter()
                .map(|o| format!("  --choice {o:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    task::append_and_notify(
        &store.clone().with_token(task::token(token)),
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

/// One `DECIDE` note on a task and what it would become.
#[derive(Clone, Debug)]
pub(crate) struct SplitItem {
    pub note_event_id: String,
    pub note_by: String,
    pub note_ts: DateTime<Utc>,
    pub note_text: String,
    pub title: String,
    pub default_choice: String,
    pub options: Vec<String>,
    /// Already split: the decision task that carries this note as its `source`.
    pub existing: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SplitPlan {
    pub parent: String,
    pub items: Vec<SplitItem>,
    /// Notes that mention `DECIDE` after their first sentence — reported, never split.
    pub mid_text: usize,
}

/// What `cv task split <id>` would do, from the model alone.
pub(crate) fn plan_split(model: &TaskReadModel, parent: &str) -> Result<SplitPlan> {
    let t = model.tasks.get(parent).with_context(|| format!("no task {parent}"))?;
    let by_source: HashMap<&str, &str> = model
        .tasks
        .values()
        .filter_map(|d| {
            d.decision
                .as_ref()
                .and_then(|dec| dec.source.as_deref())
                .map(|s| (s, d.task_id.as_str()))
        })
        .collect();
    let mut items = Vec::new();
    let mut mid_text = 0;
    for n in &t.notes {
        if cv_core::task::decide::mentions_decide_mid_text(&n.text) {
            mid_text += 1;
        }
        let Some(parsed) = task::parse_decide_note(&n.text) else {
            continue;
        };
        let (default_choice, options) = task::options_of(&parsed);
        items.push(SplitItem {
            note_event_id: n.event_id.clone(),
            note_by: n.by.clone(),
            note_ts: n.ts,
            note_text: n.text.clone(),
            title: parsed.title,
            default_choice,
            options,
            existing: by_source.get(n.event_id.as_str()).map(|s| s.to_string()),
        });
    }
    Ok(SplitPlan {
        parent: parent.to_string(),
        items,
        mid_text,
    })
}

/// `(note event id, new decision task id)` per decision a split created.
pub(crate) type SplitCreated = Vec<(String, String)>;

/// Execute a plan: one decision per not-yet-split note, assigned like the parent, carrying the
/// parent's repo/issue/channel, with the note as its body and source, blocking the parent.
/// Returns what was created and the advisory warnings.
pub(crate) fn run_split(
    store: &TaskStore,
    model: &TaskReadModel,
    from: &str,
    plan: &SplitPlan,
) -> Result<(SplitCreated, Vec<String>)> {
    let t = &model.tasks[&plan.parent];
    let Some(for_who) = t.assignee.clone() else {
        bail!(
            "task {} has no assignee — split needs to know who owes the decisions (assign it first)",
            &plan.parent[..13]
        );
    };
    let mut created = Vec::new();
    let mut warnings = Vec::new();
    for item in plan.items.iter().filter(|i| i.existing.is_none()) {
        let spec = DecisionSpec {
            title: item.title.clone(),
            body: item.note_text.clone(),
            for_who: for_who.clone(),
            default_choice: item.default_choice.clone(),
            options: item.options.clone(),
            deadline: None,
            repo: t.repo.clone(),
            issue: t.issue.clone(),
            channel: t.channel.clone(),
            tags: vec!["split".into()],
            source: Some(item.note_event_id.clone()),
            blocks: vec![plan.parent.clone()],
        };
        let (events, w) = pose(store, from, spec)?;
        warnings.extend(w);
        created.push((item.note_event_id.clone(), events[0].task_id.clone()));
    }
    Ok((created, warnings))
}

/// `note event id → decision task id` for every decision split from a note (what `show` uses to
/// point a `DECIDE` note at its decision).
pub(crate) fn split_children(model: &TaskReadModel) -> HashMap<String, String> {
    model
        .tasks
        .values()
        .filter_map(|d| {
            d.decision
                .as_ref()
                .and_then(|dec| dec.source.clone())
                .map(|s| (s, d.task_id.clone()))
        })
        .collect()
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
        let ts = task::parse_since(s, now).map_err(|e| anyhow::anyhow!(e))?;
        Ok(Since { ts: Some(ts), id: None })
    }

    fn admits(&self, ev: &TaskEvent) -> bool {
        match (&self.id, self.ts) {
            (Some(id), _) => ev.id.as_str() > id.as_str(),
            (None, Some(ts)) => ev.ts >= ts,
            (None, None) => true,
        }
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

/// The wire tag for a kind spelling a person would type: `note`→`noted`, `resolve`→`resolved`,
/// `open`→`opened`, `claim`→`claimed`, `pass`→`review_passed`, … Unknown spellings pass through
/// (and match nothing, loudly empty rather than wrong).
pub(crate) fn kind_tag(s: &str) -> String {
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
        "propose" | "proposed" | "revision" => "revision_proposed",
        "pass" | "passed" => "review_passed",
        "refute" | "refuted" => "review_refuted",
        "reroute" | "rerouted" => "review_rerouted",
        "land" => "landed",
        other => other,
    }
    .to_string()
}

/// The events matching `f`, log order.
pub(crate) fn select_events<'a>(outcome: &'a ReplayOutcome, f: &EventFilter) -> Vec<&'a TaskEvent> {
    outcome
        .events
        .iter()
        .filter(|ev| f.since.admits(ev))
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
                    .is_some_and(|t| t.assignee.as_deref() == Some(a))
            })
        })
        .collect()
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
        // The same telling detail the text feed prints, so a page need not know every kind.
        obj.insert("detail".into(), serde_json::Value::String(event_detail(ev)));
    }
    v
}

/// The telling detail of one event (what `event_text` prints after the kind), sanitized.
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
        TaskEventKind::Opened { title, .. } => truncate(&sanitize_line(title), 120),
        TaskEventKind::Claimed { assignee } => format!("by {}", sanitize_line(assignee)),
        TaskEventKind::Done { observed, .. } => observed.as_deref().map(|o| truncate(&sanitize_line(o), 120)).unwrap_or_default(),
        TaskEventKind::Tagged { tags } => tags.iter().map(|t| format!("#{}", sanitize_line(t))).collect::<Vec<_>>().join(" "),
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
        TaskEventKind::Tagged { tags } => format!("#{}", tags.join(" #")),
        TaskEventKind::Posed { default_choice, .. } => format!("default: {}", sanitize_line(default_choice)),
        _ => String::new(),
    };
    format!(
        "{}  {:<16} {}  {:<24} {}{}",
        fmt_local(ev.ts, "%m-%d %H:%M"),
        ev.kind.tag(),
        ev.task_id.get(..plen).unwrap_or(&ev.task_id),
        truncate(&sanitize_line(&ev.by), 24),
        title,
        if detail.is_empty() {
            String::new()
        } else {
            format!(" · {detail}")
        }
    )
}

// ───────────────────────────── the inbox page ─────────────────────────────

/// One inbox item with everything a page or a Markdown export needs — the full projection is
/// flattened into the fields a reader looks at, not embedded.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InboxItem {
    pub id: String,
    pub short: String,
    pub title: String,
    /// `decision` or `action`.
    pub kind: &'static str,
    /// `None` for a closed item (the page's Resolved filter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<InboxReason>,
    pub state: String,
    pub since: DateTime<Utc>,
    pub age: String,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub opened_by: String,
    pub opened_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    pub body: String,
    pub blocked: bool,
    pub last_by: String,
    pub last_ts: DateTime<Utc>,
    pub unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionView>,
    pub notes: Vec<NoteView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DecisionView {
    #[serde(rename = "default")]
    pub default_choice: String,
    pub options: Vec<String>,
    pub alternatives: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_phrase: Option<String>,
    pub posed_by: String,
    pub posed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionView>,
    /// The provisional resolution a decider confirmed or vetoed (kept, never rewritten).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_provisional: Option<ResolutionView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResolutionView {
    pub choice: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub accepted_default: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
}

impl ResolutionView {
    fn of(r: &task::Resolution) -> ResolutionView {
        ResolutionView {
            choice: r.choice.clone(),
            by: r.by.clone(),
            ts: r.ts,
            note: r.note.clone(),
            accepted_default: r.accepted_default,
            provisional: r.provisional,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NoteView {
    pub by: String,
    pub ts: DateTime<Utc>,
    pub text: String,
    /// Appended after the task closed (the page renders "(after close)").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_close: bool,
    /// The decision this note was split into, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_into: Option<String>,
}

/// The whole page for one person.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InboxPage {
    pub who: String,
    pub now: DateTime<Utc>,
    /// Open items, stalest first (decisions age from their pose).
    pub items: Vec<InboxItem>,
    /// Closed items assigned to `who` in the window, newest first (resolved / done / abandoned).
    pub closed: Vec<InboxItem>,
    /// Open items outside the window (not involving the caller).
    pub hidden: usize,
    pub counts: InboxCounts,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct InboxCounts {
    pub decisions: usize,
    /// Decisions parked for discussion (open, not owed).
    pub discussing: usize,
    /// Decisions made for `who` provisionally, awaiting a confirm or veto (resolved, not owed).
    pub provisional: usize,
    pub assigned: usize,
    pub claimed: usize,
    pub reviews: usize,
    pub unlanded: usize,
    pub unread: usize,
    pub closed: usize,
}

/// `task_id → by of its last event`, from the replayed log.
fn last_authors(events: &[TaskEvent]) -> HashMap<&str, &str> {
    let mut m = HashMap::new();
    for ev in events {
        m.insert(ev.task_id.as_str(), ev.by.as_str());
    }
    m
}

/// Human deadline: `by 10-03 (in 2d)` / `by 10-03 (OVERDUE 1d)`.
pub(crate) fn deadline_phrase(deadline: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let day = fmt_local(deadline, "%m-%d");
    if deadline >= now {
        format!("by {day} (in {})", task::age_short(now, deadline))
    } else {
        format!("by {day} (OVERDUE {})", task::age_short(deadline, now))
    }
}

/// What every item on one page shares.
struct PageCtx<'a> {
    model: &'a TaskReadModel,
    who: &'a str,
    plen: usize,
    children: HashMap<String, String>,
    authors: HashMap<&'a str, &'a str>,
    now: DateTime<Utc>,
}

fn item_of(ctx: &PageCtx<'_>, t: &TaskProjection, reason: Option<InboxReason>, since: DateTime<Utc>) -> InboxItem {
    let (model, who, plen, children, now) = (ctx.model, ctx.who, ctx.plen, &ctx.children, ctx.now);
    let last_by = ctx.authors.get(t.task_id.as_str()).copied().unwrap_or("");
    InboxItem {
        id: t.task_id.clone(),
        short: t.task_id.get(..plen).unwrap_or(&t.task_id).to_string(),
        title: t.title.clone(),
        kind: t.kind(),
        reason,
        state: task::effective_display(t),
        since,
        age: task::age_short(since, now),
        stale: now.signed_duration_since(since) > chrono::Duration::hours(24),
        assignee: t.assignee.clone(),
        opened_by: t.opened_by.clone(),
        opened_at: t.opened_at,
        tags: t.tags.clone(),
        repo: t.repo.clone(),
        issue: t.issue.clone(),
        body: t.body.clone(),
        blocked: task::is_blocked(model, t),
        last_by: last_by.to_string(),
        last_ts: t.last_ts,
        unread: !task::same_actor(last_by, who),
        decision: t.decision.as_ref().map(|d| DecisionView {
            default_choice: d.default_choice.clone(),
            options: d.options.clone(),
            alternatives: d.alternatives().map(str::to_string).collect(),
            deadline: d.deadline,
            deadline_phrase: d.deadline.map(|dl| deadline_phrase(dl, now)),
            posed_by: d.posed_by.clone(),
            posed_at: d.posed_at,
            source: d.source.clone(),
            resolution: d.resolution.as_ref().map(ResolutionView::of),
            superseded_provisional: d.superseded_provisional.as_ref().map(ResolutionView::of),
        }),
        notes: t
            .notes
            .iter()
            .map(|n| NoteView {
                by: n.by.clone(),
                ts: n.ts,
                text: n.text.clone(),
                post_close: n.post_close,
                split_into: children.get(&n.event_id).cloned(),
            })
            .collect(),
    }
}

/// Build the page: the inbox projection for `who`, scoped to `since` (items involving `caller`
/// escape the window), `unread_only` keeping items whose last event is not `who`'s own.
pub(crate) fn inbox_page(
    outcome: &ReplayOutcome,
    who: &str,
    caller: Option<&str>,
    since: Option<DateTime<Utc>>,
    unread_only: bool,
    now: DateTime<Utc>,
) -> InboxPage {
    let model = &outcome.model;
    let ctx = PageCtx {
        model,
        who,
        plen: task::unique_prefix_len(model.tasks.keys().map(String::as_str), 8),
        children: split_children(model),
        authors: last_authors(&outcome.events),
        now,
    };
    let mut items = Vec::new();
    let mut hidden = 0;
    for e in task::inbox(model, who) {
        if !task::in_scope(e.task, since, caller) {
            hidden += 1;
            continue;
        }
        let item = item_of(&ctx, e.task, Some(e.reason), e.since);
        if unread_only && !item.unread {
            continue;
        }
        items.push(item);
    }
    let mut closed: Vec<InboxItem> = model
        .tasks
        .values()
        .filter(|t| t.state.is_terminal() && t.assignee.as_deref() == Some(who))
        // A provisional resolution awaiting a veto is an open item, not a closed one.
        .filter(|t| !t.decision.as_ref().is_some_and(|d| d.awaiting_veto()))
        .filter(|t| task::in_scope(t, since, caller))
        .map(|t| item_of(&ctx, t, None, t.last_ts))
        .collect();
    closed.sort_by_key(|i| std::cmp::Reverse(i.last_ts));
    let count = |r: InboxReason| items.iter().filter(|i| i.reason == Some(r)).count();
    let counts = InboxCounts {
        decisions: count(InboxReason::DecisionOwed),
        discussing: count(InboxReason::Discussing),
        provisional: count(InboxReason::Provisional),
        assigned: count(InboxReason::AssignedOpen),
        claimed: count(InboxReason::ClaimedByYou),
        reviews: count(InboxReason::AwaitingYourReview),
        unlanded: count(InboxReason::YourUnlandedWork),
        unread: items.iter().filter(|i| i.unread).count(),
        closed: closed.len(),
    };
    InboxPage {
        who: who.to_string(),
        now,
        items,
        closed,
        hidden,
        counts,
    }
}

/// The inbox's groups, in the order they are printed: a decision owed is the slowest blocker a
/// fleet has, so it comes first; then the actions handed to you; then the work you hold; then
/// what others wait on you for.
pub(crate) const INBOX_GROUPS: &[(InboxReason, &str)] = &[
    (InboxReason::DecisionOwed, "decisions owed"),
    (InboxReason::Discussing, "in discussion (parked, still yours to resolve)"),
    (InboxReason::Provisional, "made for you (veto?)"),
    (InboxReason::AssignedOpen, "assigned actions"),
    (InboxReason::ClaimedByYou, "claimed work"),
    (InboxReason::AwaitingYourReview, "reviews"),
    (InboxReason::YourUnlandedWork, "unlanded"),
];

/// The first non-empty line of a text, trimmed and capped.
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let l = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    truncate(&sanitize_line(l), max)
}

/// `cv task inbox <who>` as text.
pub(crate) fn render_inbox_text(page: &InboxPage) -> String {
    let mut out = String::new();
    if page.items.is_empty() {
        out.push_str(&format!("(inbox empty for {})\n", sanitize_line(&page.who)));
    }
    for (reason, label) in INBOX_GROUPS {
        let group: Vec<&InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("{label} ({}):\n", group.len()));
        for i in group {
            out.push_str(&format!(
                "{} {}  {:>4}  {:10} {}\n",
                if i.stale { "⏰" } else { "  " },
                i.short,
                i.age,
                i.state,
                sanitize_line(&i.title)
            ));
            if let Some(d) = &i.decision {
                if let Some(r) = d.resolution.as_ref().filter(|r| r.provisional) {
                    let mut line = format!("⇒ made: {} — by {}", sanitize_line(&r.choice), sanitize_line(&r.by));
                    if !d.alternatives.is_empty() {
                        line.push_str(&format!(
                            " · veto to: {}",
                            d.alternatives.iter().map(|a| sanitize_line(a)).collect::<Vec<_>>().join(" / ")
                        ));
                    }
                    out.push_str(&format!("{:width$}{}\n", "", truncate(&line, 200), width = i.short.len() + 3));
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
                out.push_str(&format!("{:width$}{}\n", "", truncate(&line, 200), width = i.short.len() + 3));
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
pub(crate) fn render_inbox_md(page: &InboxPage) -> String {
    let mut out = String::new();
    let who = sanitize_line(&page.who);
    out.push_str(&format!(
        "# Inbox for {who} — {}\n\n",
        fmt_local(page.now, "%Y-%m-%d %H:%M")
    ));
    let c = &page.counts;
    out.push_str(&format!(
        "{} decision(s) owed · {} assigned action(s) · {} claimed · {} review(s) · {} unlanded · {} unread",
        c.decisions, c.assigned, c.claimed, c.reviews, c.unlanded, c.unread
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
        let group: Vec<&InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("## {} ({})\n\n", capitalize(label), group.len()));
        for i in group {
            n += 1;
            out.push_str(&format!("### {n}. {}\n\n", md_inline(&i.title)));
            let mut facts = vec![
                format!("`{}`", i.short),
                i.state.clone(),
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
                        i.short
                    )),
                    None => out.push_str(&format!(
                        "- resolve: `cv task resolve {} --accept-default --from {who}`\n",
                        i.short
                    )),
                }
                out.push('\n');
            }
            if !i.body.trim().is_empty() {
                for para in i.body.trim().split("\n\n") {
                    out.push_str(&md_block(para));
                    out.push_str("\n\n");
                }
            }
            if !i.notes.is_empty() {
                out.push_str(&format!("notes ({}):\n\n", i.notes.len()));
                for note in &i.notes {
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
    fn kind_spellings_map_to_wire_tags() {
        assert_eq!(kind_tag("note"), "noted");
        assert_eq!(kind_tag("Resolve"), "resolved");
        assert_eq!(kind_tag("done"), "done");
        assert_eq!(kind_tag("pass"), "review_passed");
        assert_eq!(kind_tag("bogus"), "bogus");
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

    #[test]
    fn option_lists_put_the_default_first_and_deduplicate() {
        assert_eq!(
            option_list(" keep ", &["delete".into(), "keep".into(), "".into(), "delete".into()]),
            vec!["keep", "delete"]
        );
    }

    #[test]
    fn deadline_phrases_say_in_or_overdue() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        assert!(deadline_phrase(now + chrono::Duration::days(2), now).contains("(in 2d)"));
        assert!(deadline_phrase(now - chrono::Duration::hours(5), now).contains("OVERDUE 5h"));
    }
}
