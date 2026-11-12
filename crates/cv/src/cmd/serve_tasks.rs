//! The inbox's routes on `cv serve` (what `cv task serve` was): reads shaped for the one inline
//! page (`serve_tasks_page.html`, served at `/inbox`) and the writes its buttons make, which record
//! **the same events the CLI records**, so the page and the shell are one store. Every write is
//! stamped `web:<who>`. The HTTP model (Host check, CORS allow-list, bearer token, the
//! non-loopback refusal) is `cv serve`'s; this module never sees a socket.
//!
//! Routes:
//! - `GET /api/inbox?who=`                the inbox page (`task::inbox_page`, `all=1` lifts the window)
//! - `POST /api/task/<id>/resolve`        `{choice}` | `{accept_default: true}` | `{confirm: true}` (a
//!   provisional resolution), `{note}` optional
//! - `POST /api/task/<id>/done`           `{observed}` optional
//! - `POST /api/task/<id>/note`           `{text}`
//! - `POST /api/task/<id>/discuss`        `{text}` optional → a `NEEDS DISCUSSION:` note + the `discuss` tag
//! - `POST /api/task/<id>/claim`          claim for `who`
//! - `POST /api/task/<id>/release`
//! - `GET /api/events?since=&kind=&by=&not_by=&assignee=&task=`  JSON lines — the same query as
//!   `cv task events`, so a poller and the page see one feed.
//! - `GET /api/lanes?session=<id>`        the sub-agent forest of a session (`cv lanes --tasks`),
//!   running first, each lane with the `tasks` its endpoint holds.
//!
//! The backlog pane reads the read API's `GET /api/tasks?detail=1`.
//!
//! Every POST takes `who` in the body (else `?who=`, else the server's `--who`).

use anyhow::{bail, Result};
use chrono::Utc;
use cv_core::api::Query;
use cv_core::task::{self, TaskEventKind, TaskStore};
use serde_json::{json, Value};

use super::task_ops::{self, EventFilter, Since};

/// The inbox page: one inline HTML file, no external JS/CSS, no build step.
pub(crate) const PAGE: &str = include_str!("serve_tasks_page.html");

/// A task-route answer: JSON, or the newline-delimited JSON `/api/events` streams.
pub(crate) enum Answer {
    Json(u16, Value),
    Ndjson(String),
}

cv_core::routes! {
    pub(crate) const ROUTES;
    /// The inbox routes, or `None` when the path is not one of them (the read API answers it).
    pub(crate) fn route(post, parts, q: &Query, body: &Value, default_who: Option<&str>) -> Option<Answer> {
        GET ["api", "inbox"] => json(api_inbox(default_who, q)),
        GET ["api", "events"] => Some(match api_events(q) {
            Ok(lines) => Answer::Ndjson(lines),
            Err(e) => Answer::Json(400, json!({"error": e.to_string()})),
        }),
        GET ["api", "lanes"] => json(api_lanes(q)),
        POST ["api", "task", id, verb] => {
            let who = body
                .get("who")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| q.get("who"))
                .or_else(|| default_who.map(str::to_string));
            json(match who {
                Some(who) => api_act(id, verb, &who, body),
                None => (400, json!({"error": "who? pass `who` in the body or start with --who"})),
            })
        },
        _ => None,
    }
}

fn json((status, v): (u16, Value)) -> Option<Answer> {
    Some(Answer::Json(status, v))
}

fn with_replay(f: impl FnOnce(cv_core::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match task::replay() {
        Ok(o) => f(o),
        Err(e) => (500, json!({"error": e.to_string()})),
    }
}

fn api_inbox(default_who: Option<&str>, q: &Query) -> (u16, Value) {
    let Some(who) = q.get("who").or_else(|| default_who.map(str::to_string)) else {
        return (400, json!({"error": "who? pass ?who=<name> or start with --who"}));
    };
    let who = who.trim();
    if task::project::canonical_actor(who).trim().is_empty() {
        return (400, json!({"error": "who must not be empty"}));
    }
    let now = Utc::now();
    let window = match cv_core::api::inbox_window(q, Some(chrono::Duration::days(14)), now) {
        Ok(window) => window,
        Err(e) => return (400, json!({"error": e})),
    };
    with_replay(|outcome| {
        let caller = task::default_endpoint();
        let page = task::inbox_page(&outcome, who, caller.as_deref(), window, false, now);
        let mut v = serde_json::to_value(&page).expect("page serializes");
        if let Some(o) = v.as_object_mut() {
            o.insert("warnings".into(), json!(outcome.warnings));
        }
        (200, v)
    })
}

/// `cv lanes <session>` as JSON: every sub-agent, running first, then by start time (newest
/// first). The page polls this beside the inbox so the orchestrator's forest and the human's
/// decisions sit on one screen.
fn api_lanes(q: &Query) -> (u16, Value) {
    let Some(session) = q.get("session") else {
        return (400, json!({"error": "which session? pass ?session=<id prefix>"}));
    };
    let (r, _adapter) = match crate::util::resolve(&session, None) {
        Ok(x) => x,
        Err(e) => return (404, json!({"error": e.to_string()})),
    };
    let mut lanes = match cv_core::lanes::lanes_of(&r) {
        Ok(lanes) => lanes,
        Err(error) => return (500, json!({"error": format!("{error:#}")})),
    };
    // Task enrichment is optional: retain useful lane facts while exposing degraded replay.
    // An empty forest has no task join to perform and must not inspect the task log.
    let mut warnings = Vec::new();
    if !lanes.is_empty() {
        match task::replay() {
            Ok(outcome) => {
                cv_core::lanes::attach_tasks(&mut lanes, &outcome.model);
                warnings = outcome.warnings;
            }
            Err(error) => warnings.push(format!("task enrichment unavailable: {error:#}")),
        }
    }
    lanes.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then_with(|| b.started_at.cmp(&a.started_at))
    });
    let running = lanes.iter().filter(|l| l.is_running()).count();
    let stranded = lanes.iter().filter(|l| l.stranded).count();
    let done = lanes.iter().filter(|l| l.is_done()).count();
    (
        200,
        json!({
            "session": r.id,
            "counts": {"total": lanes.len(), "running": running, "done": done, "stranded": stranded},
            "lanes": lanes,
            "warnings": warnings,
        }),
    )
}

fn api_events(q: &Query) -> Result<String> {
    let outcome = task::replay()?;
    let now = Utc::now();
    let task_id = match q.get("task") {
        Some(p) => Some(
            task::resolve_id(&outcome.model, &p)
                .map_err(|e| anyhow::anyhow!(e))?
                .to_string(),
        ),
        None => None,
    };
    let f = EventFilter {
        since: Since::parse(q.get("since").as_deref(), now)?,
        kinds: q
            .get("kind")
            .map(|k| task_ops::kind_tags(&k))
            .transpose()?
            .unwrap_or_default(),
        by: q.get("by"),
        not_by: q.get("not_by"),
        assignee: q.get("assignee"),
        task: task_id,
    };
    event_lines(&outcome, &f)
}

fn event_lines(outcome: &task::ReplayOutcome, filter: &EventFilter) -> Result<String> {
    // JSONL rows are events, with no warning envelope. Refuse an incomplete feed rather than
    // silently omit quarantined history from a poller's view; page/task reads expose warnings.
    if let Some(first) = outcome.warnings.first() {
        bail!(
            "task event feed unavailable: degraded log ({} warning(s); first: {first})",
            outcome.warnings.len()
        );
    }
    let mut out = String::new();
    for ev in task_ops::select_events(outcome, filter)? {
        out.push_str(&serde_json::to_string(&task_ops::event_json(ev, &outcome.model))?);
        out.push('\n');
    }
    Ok(out)
}

/// One write, as `web:<who>`: the same `TaskEventKind`s the CLI verbs append.
fn api_act(id: &str, verb: &str, who: &str, body: &Value) -> (u16, Value) {
    match act(id, verb, who, body) {
        Ok(v) => (200, v),
        Err(e) => (400, json!({"error": e.to_string()})),
    }
}

fn act(id: &str, verb: &str, who: &str, body: &Value) -> Result<Value> {
    act_in(&TaskStore::default_store(), id, verb, who, body)
}

fn act_in(store: &TaskStore, id: &str, verb: &str, who: &str, body: &Value) -> Result<Value> {
    let who = task::project::canonical_actor(who.trim()).trim();
    if who.is_empty() {
        bail!("who must not be empty");
    }
    let outcome = store.replay()?;
    let id = task::resolve_id(&outcome.model, id)
        .map_err(|e| anyhow::anyhow!(e))?
        .to_string();
    let by = format!("web:{who}");
    let text = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let (appended, state, warnings) = match verb {
        "resolve" => {
            let accept = body.get("accept_default").and_then(Value::as_bool).unwrap_or(false);
            let confirm = body.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            let answer = task_ops::Answer::from_flags(text("choice"), accept, confirm)?;
            let out = task_ops::resolve(store, &outcome.model, &by, &id, answer, text("note"), false)?;
            (vec![out.event], out.effective_state, out.warnings)
        }
        "done" => {
            let out = task::append(
                store,
                Some(&id),
                &by,
                TaskEventKind::Done {
                    observed: text("observed"),
                    check: None,
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.warnings)
        }
        "note" => {
            let Some(t) = text("text") else {
                bail!("a note needs text")
            };
            let out = task::append(
                store,
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: t,
                    session_ref: None,
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.warnings)
        }
        "discuss" => {
            // One unit: the note and the `discuss` tag land together.
            let detail = text("text").unwrap_or_else(|| "(no detail)".into());
            let out = task::append_events(
                store,
                vec![
                    task::new_event(
                        Some(&id),
                        &by,
                        TaskEventKind::Noted {
                            text: format!("NEEDS DISCUSSION: {detail}"),
                            session_ref: None,
                        },
                    ),
                    task::new_event(
                        Some(&id),
                        &by,
                        TaskEventKind::Tagged {
                            tags: vec![task::DISCUSS_TAG.into()],
                        },
                    ),
                ],
                Vec::new(),
            )?;
            (out.events, out.effective_state, out.replay_warnings)
        }
        "claim" => {
            let out = task::append(
                store,
                Some(&id),
                &by,
                TaskEventKind::Claimed {
                    assignee: who.to_string(),
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.warnings)
        }
        "release" => {
            let out = task::append(store, Some(&id), &by, TaskEventKind::Released {}, Vec::new())?;
            (vec![out.event], out.effective_state, out.warnings)
        }
        other => bail!("unknown action {other:?}"),
    };
    Ok(json!({
        "ok": true,
        "by": by,
        "events": appended,
        "task_state": state,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(store: &TaskStore, id: &str) {
        let mut event = task::new_event(
            None,
            "author",
            TaskEventKind::Opened {
                title: "existing task".into(),
                body: "brief".into(),
                repo: None,
                issue: None,
                channel: None,
                assignee: Some("ember".into()),
            },
        );
        event.id = id.into();
        event.task_id = id.into();
        store.append_agent_event(event).unwrap();
    }

    #[test]
    fn task_http_empty_actor_and_conflicting_answers_leave_log_unchanged() {
        let world = cv_testkit::World::new("task-http-actor");
        let store = TaskStore::at(world.base.join("tasks"));
        open(&store, "task");
        store
            .append_agent_event(task::new_event(
                Some("task"),
                "author",
                TaskEventKind::Posed {
                    options: vec!["keep".into(), "change".into()],
                    default_choice: "keep".into(),
                    deadline: None,
                    source: None,
                },
            ))
            .unwrap();
        let path = store.dir().join("events.jsonl");
        let before = std::fs::read(&path).unwrap();
        for who in ["", "  ", "web:", "web:  "] {
            assert!(act_in(&store, "task", "note", who, &json!({"text":"no anonymous write"})).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        assert!(act_in(
            &store,
            "task",
            "resolve",
            "ember",
            &json!({"choice":"change","accept_default":true})
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        for who in ["", " ", "web:", "web:  "] {
            assert_eq!(api_inbox(Some(who), &Query::parse("")).0, 400);
        }
        // The shared decision resolution formatter must also accept short/non-ASCII IDs.
        assert!(act_in(&store, "task", "resolve", "ember", &json!({"accept_default":true})).is_ok());
    }

    #[test]
    fn task_http_event_feed_refuses_degraded_replay_instead_of_silently_dropping_rows() {
        let world = cv_testkit::World::new("task-http-degraded-feed");
        let store = TaskStore::at(world.base.join("tasks"));
        open(&store, "task");
        let outcome = store.replay().unwrap();
        let lines = event_lines(&outcome, &EventFilter::default()).unwrap();
        assert_eq!(lines.lines().count(), 1);
        assert_eq!(serde_json::from_str::<Value>(lines.trim()).unwrap()["task_id"], "task");
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(store.dir().join("events.jsonl"))
            .unwrap()
            .write_all(b"malformed historical row\n")
            .unwrap();
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 1);
        let error = event_lines(&outcome, &EventFilter::default()).unwrap_err().to_string();
        assert!(error.contains("degraded log") && error.contains("line 3"), "{error}");
    }
}
