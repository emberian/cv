//! The inbox page: one row shape ([`InboxItem`]) for every surface that shows what needs someone —
//! `cv task inbox` (text, `--md`, `--json`), `cv serve`'s `/inbox` page and its
//! `/api/tasks/inbox/{who}`, and MCP `task_inbox`. Built from the [`super::project::inbox`] projection plus what a reader
//! looks at: the decision's options and deadline, the notes, unread, blocked, age.
//!
//! Not in the purity fence: deadline phrases print the local day.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::model::TaskEvent;
use super::project::{self, InboxReason};
use super::reduce::{Resolution, TaskProjection, TaskReadModel};
use super::store::ReplayOutcome;

/// One inbox item with everything a page or a Markdown export needs — the full projection is
/// flattened into the fields a reader looks at, not embedded.
#[derive(Clone, Debug, Serialize)]
pub struct InboxItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall: Option<super::stall::Stall>,
    pub id: String,
    pub short: String,
    pub title: String,
    /// `decision` or `action`.
    pub kind: &'static str,
    /// `None` for a closed item (the page's Resolved filter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<InboxReason>,
    pub effective_state: String,
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
    /// `None` on brief pages, which never carry bodies (and are never serialized).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub blocked: bool,
    pub last_by: String,
    pub last_ts: DateTime<Utc>,
    pub unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionView>,
    /// The pinned STATUS text, when one is pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `None` on brief pages, which never carry notes (and are never serialized).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<NoteView>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionView {
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
pub struct ResolutionView {
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
    fn of(r: &Resolution, rich: bool) -> ResolutionView {
        ResolutionView {
            choice: r.choice.clone(),
            by: r.by.clone(),
            ts: r.ts,
            note: if rich { r.note.clone() } else { None },
            accepted_default: r.accepted_default,
            provisional: r.provisional,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NoteView {
    pub by: String,
    pub ts: DateTime<Utc>,
    pub text: String,
    /// Appended after the task closed (the page renders "(after close)").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_close: bool,
    /// The decision posed from this note, when one names it as its `source`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_into: Option<String>,
}

/// The whole page for one person.
#[derive(Clone, Debug, Serialize)]
pub struct InboxPage {
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
pub struct InboxCounts {
    pub decisions: usize,
    /// Decisions parked for discussion (open, not owed).
    pub discussing: usize,
    /// Decisions made for `who` provisionally, awaiting a confirm or veto (resolved, not owed).
    pub provisional: usize,
    pub assigned: usize,
    pub claimed: usize,
    /// Unread open items only; unread rows in closed are not included.
    pub unread: usize,
    pub closed: usize,
}

/// `task_id → by of its last event`, from the replayed log.
fn last_authors<'a>(model: &TaskReadModel, events: &'a [TaskEvent]) -> HashMap<&'a str, &'a str> {
    let mut m = HashMap::new();
    for ev in events {
        // Retired events and idempotent re-appends do not touch the projection. The physical
        // final row is therefore not necessarily the event a reader is being asked to notice.
        if model
            .tasks
            .get(&ev.task_id)
            .is_some_and(|task| task.last_event_id == ev.id)
        {
            m.insert(ev.task_id.as_str(), ev.by.as_str());
        }
    }
    m
}

/// Human deadline: `by 10-03 (in 2d)` / `by 10-03 (OVERDUE 1d)`.
pub fn deadline_phrase(deadline: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let day = deadline.with_timezone(&chrono::Local).format("%m-%d").to_string();
    if deadline >= now {
        format!("by {day} (in {})", project::age_short(now, deadline))
    } else {
        format!("by {day} (OVERDUE {})", project::age_short(deadline, now))
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
    rich: bool,
}

fn item_of(ctx: &PageCtx<'_>, t: &TaskProjection, reason: Option<InboxReason>, since: DateTime<Utc>) -> InboxItem {
    let (model, who, plen, children, now) = (ctx.model, ctx.who, ctx.plen, &ctx.children, ctx.now);
    let last_by = ctx.authors.get(t.task_id.as_str()).copied().unwrap_or("");
    InboxItem {
        stall: None,
        id: t.task_id.clone(),
        short: t.task_id.get(..plen).unwrap_or(&t.task_id).to_string(),
        title: t.title.clone(),
        kind: t.kind(),
        reason,
        effective_state: project::effective_display(t),
        since,
        age: project::age_short(since, now),
        stale: now.signed_duration_since(since) > chrono::Duration::hours(24),
        assignee: t.assignee.clone(),
        opened_by: t.opened_by.clone(),
        opened_at: t.opened_at,
        tags: t.tags.clone(),
        repo: t.repo.clone(),
        issue: t.issue.clone(),
        body: ctx.rich.then(|| t.body.clone()),
        blocked: project::is_blocked(model, t),
        last_by: last_by.to_string(),
        last_ts: t.last_ts,
        unread: !project::same_actor(last_by, who),
        decision: t.decision.as_ref().map(|d| DecisionView {
            default_choice: d.default_choice.clone(),
            options: d.options.clone(),
            alternatives: d.alternatives().map(str::to_string).collect(),
            deadline: d.deadline,
            deadline_phrase: d.deadline.map(|dl| deadline_phrase(dl, now)),
            posed_by: d.posed_by.clone(),
            posed_at: d.posed_at,
            source: d.source.clone(),
            resolution: d.resolution.as_ref().map(|r| ResolutionView::of(r, ctx.rich)),
            superseded_provisional: if ctx.rich {
                d.superseded_provisional.as_ref().map(|r| ResolutionView::of(r, true))
            } else {
                None
            },
        }),
        status: if ctx.rich {
            t.status.as_ref().map(|s| s.text.clone())
        } else {
            None
        },
        notes: ctx.rich.then(|| {
            t.notes
                .iter()
                .map(|n| NoteView {
                    by: n.by.clone(),
                    ts: n.ts,
                    text: n.text.clone(),
                    post_close: n.post_close,
                    split_into: children.get(&n.event_id).cloned(),
                })
                .collect()
        }),
    }
}

/// Build the page: the inbox projection for `who`, scoped to `since` (items involving `caller`
/// escape the window), `unread_only` keeping items whose last event is not `who`'s own.
pub fn inbox_page(
    outcome: &ReplayOutcome,
    who: &str,
    caller: Option<&str>,
    since: Option<DateTime<Utc>>,
    unread_only: bool,
    now: DateTime<Utc>,
) -> InboxPage {
    build_page(outcome, who, caller, since, unread_only, now, true)
}

/// The textual inbox summary, using the same rows/order/counts as the full page. Bodies, notes,
/// pinned status and resolution-note history are omitted; JSON/Markdown callers use inbox_page.
pub fn inbox_page_brief(
    outcome: &ReplayOutcome,
    who: &str,
    caller: Option<&str>,
    since: Option<DateTime<Utc>>,
    unread_only: bool,
    now: DateTime<Utc>,
) -> InboxPage {
    build_page(outcome, who, caller, since, unread_only, now, false)
}

fn build_page(
    outcome: &ReplayOutcome,
    who: &str,
    caller: Option<&str>,
    since: Option<DateTime<Utc>>,
    unread_only: bool,
    now: DateTime<Utc>,
    rich: bool,
) -> InboxPage {
    let model = &outcome.model;
    let ctx = PageCtx {
        model,
        who,
        plen: project::unique_prefix_len(model.tasks.keys().map(String::as_str), 8),
        // Only rich rows carry notes, so only they name a split decision.
        children: if rich {
            decisions_from_notes(model)
        } else {
            HashMap::new()
        },
        authors: last_authors(model, &outcome.events),
        now,
        rich,
    };
    let mut items = Vec::new();
    let mut hidden = 0;
    for e in project::inbox(model, who) {
        if !project::in_scope(e.task, since, caller) {
            hidden += 1;
            continue;
        }
        if unread_only && project::same_actor(ctx.authors.get(e.task.task_id.as_str()).copied().unwrap_or(""), who) {
            continue;
        }
        items.push(item_of(&ctx, e.task, Some(e.reason), e.since));
    }
    let mut closed: Vec<InboxItem> = model
        .tasks
        .values()
        .filter(|t| t.state.is_terminal() && t.assignee.as_deref().is_some_and(|have| project::same_actor(have, who)))
        // A provisional resolution awaiting a veto is an open item, not a closed one.
        .filter(|t| !t.decision.as_ref().is_some_and(|d| d.awaiting_veto()))
        .filter(|t| project::in_scope(t, since, caller))
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

/// `note event id → decision task id` for every decision that names a note as its `source` (what
/// `cv task split` wrote until 0.15.0; `show` and the page still point such a note at its
/// decision).
pub fn decisions_from_notes(model: &TaskReadModel) -> HashMap<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_unread_uses_applied_history_and_closed_rows_share_actor_aliases() {
        use crate::task::{TaskEventKind, TaskReducer};
        let ts: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let event = |id: &str, task: &str, by: &str, kind| TaskEvent {
            id: id.into(),
            task_id: task.into(),
            by: by.into(),
            ts,
            kind,
        };
        let opened = event(
            "task",
            "task",
            "orchestrator",
            TaskEventKind::Opened {
                title: "read state".into(),
                body: "large body".repeat(8192),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("web:ember".into()),
            },
        );
        let old_note = event(
            "old",
            "task",
            "orchestrator",
            TaskEventKind::Noted {
                claim: None,
                text: "old unread note".into(),
                session_ref: None,
            },
        );
        let current = event(
            "current",
            "task",
            "web:ember",
            TaskEventKind::Noted {
                claim: None,
                text: "I read it".into(),
                session_ref: None,
            },
        );
        let retired = event(
            "retired",
            "task",
            "cv-verify",
            TaskEventKind::Retired(crate::task::RetiredEvent(
                serde_json::from_value(serde_json::json!({"event":"landed"})).unwrap(),
            )),
        );
        let events = vec![opened, old_note.clone(), current, old_note, retired];
        let mut outcome = ReplayOutcome {
            model: TaskReducer::reduce(&events).unwrap(),
            events,
            warnings: Vec::new(),
            quarantined: 0,
        };
        for who in ["ember", "web:ember"] {
            let page = inbox_page(&outcome, who, None, None, false, ts);
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].last_by, "web:ember");
            assert!(!page.items[0].unread);
            assert_eq!(page.items[0].notes.as_ref().unwrap().len(), 2);
            assert!(inbox_page(&outcome, who, None, None, true, ts).items.is_empty());
        }
        outcome.events.push(event(
            "done",
            "task",
            "web:ember",
            TaskEventKind::Done {
                evidence: Vec::new(),
                disclosure: None,
                observed: None,
                check: None,
            },
        ));
        outcome.model = TaskReducer::reduce(&outcome.events).unwrap();
        for who in ["ember", "web:ember"] {
            let page = inbox_page(&outcome, who, None, None, false, ts);
            assert!(page.items.is_empty());
            assert_eq!(page.closed.len(), 1);
            assert_eq!(page.closed[0].last_by, "web:ember");
            assert!(!page.closed[0].unread);
        }
    }

    #[test]
    fn deadline_phrases_say_in_or_overdue() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        assert!(deadline_phrase(now + chrono::Duration::days(2), now).contains("(in 2d)"));
        assert!(deadline_phrase(now - chrono::Duration::hours(5), now).contains("OVERDUE 5h"));
    }

    /// The one inbox row every surface serves: key names and order are the wire (`preserve_order`),
    /// empty optionals stay off it, and a posed decision carries its options and deadline phrase.
    #[test]
    fn inbox_item_is_the_shipped_wire_shape() {
        use crate::task::model::TaskEventKind;
        use crate::task::reduce::TaskReducer;
        let ts: DateTime<Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
        let ev = |n: u64, kind: TaskEventKind| TaskEvent {
            id: format!("00000000-0000-7000-8000-{n:012}"),
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            ts,
            by: "orchestrator:o".into(),
            kind,
        };
        let events = vec![
            ev(1, crate::task::opened_kind("pick", Some("ember"))),
            ev(
                2,
                TaskEventKind::Posed {
                    options: vec!["a".into(), "b".into()],
                    default_choice: "a".into(),
                    deadline: Some(ts + chrono::Duration::days(2)),
                    source: None,
                },
            ),
        ];
        let model = TaskReducer::reduce(&events).unwrap();
        let outcome = ReplayOutcome {
            model,
            events,
            warnings: Vec::new(),
            quarantined: 0,
        };
        let page = inbox_page(&outcome, "ember", None, None, false, ts + chrono::Duration::hours(1));
        assert_eq!(page.counts.decisions, 1);
        let v = serde_json::to_value(&page.items[0]).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "short",
                "title",
                "kind",
                "reason",
                "effective_state",
                "since",
                "age",
                "stale",
                "assignee",
                "opened_by",
                "opened_at",
                "body",
                "blocked",
                "last_by",
                "last_ts",
                "unread",
                "decision",
                "notes"
            ]
        );
        assert_eq!(v["reason"], "decision_owed");
        assert_eq!(v["effective_state"], "open");
        assert_eq!(v["kind"], "decision");
        assert_eq!(v["age"], "1h");
        assert_eq!(v["decision"]["default"], "a");
        assert_eq!(v["decision"]["alternatives"], serde_json::json!(["b"]));
        assert!(
            v["decision"]["deadline_phrase"].as_str().unwrap().contains("(in 1d)"),
            "{v}"
        );
    }
}
