//! `cv task serve` — a local web inbox served by cv itself: one inline HTML page (no external
//! JS/CSS, no build step) over a handful of JSON routes that record **the same events the CLI
//! records**, so the page and the shell are one store. Every write is stamped `web:<who>`.
//!
//! Loopback by default. `--bind 0.0.0.0:<port>` is an explicit choice to let any device on the LAN
//! act as `web:<who>` (a phone in Safari). The `Host` header must be a loopback name, the bound
//! host, or an IP literal — a DNS name is refused, which defeats DNS rebinding (a hostile page can
//! point its own domain at this machine but cannot make the browser send an IP-literal `Host`).
//! No CORS headers are ever emitted: only the served page is same-origin.
//!
//! Routes:
//! - `GET /`                              the page
//! - `GET /api/inbox?who=`                the inbox page (`task_ops::inbox_page`, `all=1` lifts the window)
//! - `GET /api/task/<id>`                 one item (`events=1` adds the raw history)
//! - `POST /api/task/<id>/resolve`        `{choice}` | `{accept_default: true}`, `{note}` optional
//! - `POST /api/task/<id>/done`           `{observed}` optional
//! - `POST /api/task/<id>/note`           `{text}`
//! - `POST /api/task/<id>/discuss`        `{text}` optional → a `NEEDS DISCUSSION:` note + the `discuss` tag
//! - `POST /api/task/<id>/claim`          claim for `who`
//! - `POST /api/task/<id>/release`
//! - `POST /api/task/<id>/reopen`         a closed task comes back as a NEW task (the record stays)
//! - `GET /api/events?since=&kind=&by=&not_by=&assignee=&task=`  JSON lines — the same query as
//!   `cv task events`, so a poller and the page see one feed.
//! - `GET /api/lanes?session=<id>`        the sub-agent forest of a session (`cv lanes`), running
//!   first — the orchestrator's live lane table beside the inbox.
//! - `GET /api/backlog?repo=&state=&tag=&all=1` every open task (`cv task list` with no window);
//!   `all=1` includes terminal ones.
//!
//! Every POST takes `who` in the body (else the server's `--assignee`).

use std::io::Read;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::Utc;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, TaskEventKind, TaskStore};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use super::task_ops::{self, DecisionSpec, EventFilter, Since};

const PAGE: &str = include_str!("task_inbox.html");

struct Ctx {
    bind_host: String,
    default_who: Option<String>,
}

pub(crate) fn run(bind: &str, default_who: Option<String>, open: bool) -> Result<()> {
    let server = Server::http(bind).map_err(|e| anyhow::anyhow!("cannot bind {bind}: {e}"))?;
    let addr = server
        .server_addr()
        .to_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|| bind.to_string());
    let bind_host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind).to_string();
    let display_host = if bind_host == "0.0.0.0" || bind_host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        bind_host.clone()
    };
    let port = addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("").to_string();
    let url = match &default_who {
        Some(w) => format!("http://{display_host}:{port}/?who={}", urlencoding::encode(w)),
        None => format!("http://{display_host}:{port}/"),
    };
    // The banner carries the REAL bound address (`--bind 127.0.0.1:0` picks a free port), so a
    // test or script can read it instead of guessing.
    eprintln!(
        "cv task serve listening on http://{addr}/ — {}{}",
        match &default_who {
            Some(w) => format!("inbox for {}", sanitize_line(w)),
            None => "pass ?who=<name> (or --assignee)".to_string(),
        },
        if bind_host == "0.0.0.0" {
            " · LAN-reachable: anyone who can reach this port acts as web:<who>"
        } else {
            ""
        }
    );
    eprintln!("open {url}");
    if open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        if let Err(e) = std::process::Command::new(opener).arg(&url).spawn() {
            eprintln!("could not open a browser ({e}); open {url} yourself");
        }
    }
    let server = Arc::new(server);
    let ctx = Arc::new(Ctx {
        bind_host,
        default_who,
    });
    let mut workers = Vec::new();
    for _ in 0..2 {
        let server = Arc::clone(&server);
        let ctx = Arc::clone(&ctx);
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(request, &ctx)));
                if outcome.is_err() {
                    eprintln!("cv task serve: a request handler panicked; still serving");
                }
            }
        }));
    }
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"X-Invalid"[..], &b"1"[..]).unwrap())
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut r = Response::from_data(data).with_status_code(status);
    r.add_header(header("Content-Type", "application/json; charset=utf-8"));
    r.add_header(header("Cache-Control", "no-store"));
    r
}

fn text_response(status: u16, content_type: &str, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = Response::from_data(body.into_bytes()).with_status_code(status);
    r.add_header(header("Content-Type", content_type));
    r.add_header(header("Cache-Control", "no-store"));
    r
}

/// Accept `Host` when it is a loopback name, the bound host, or an IP literal; refuse any other
/// DNS name (the rebinding vector).
fn host_allowed(host: Option<&str>, bind_host: &str) -> bool {
    let Some(h) = host else { return false };
    let name = h.trim_start_matches('[');
    let name = match name.rsplit_once(':') {
        // `1.2.3.4:7777` / `[::1]:7777` / a bare IPv6 with many colons
        Some((n, port)) if port.chars().all(|c| c.is_ascii_digit()) => n.trim_end_matches(']'),
        _ => name.trim_end_matches(']'),
    };
    name == "localhost"
        || name == bind_host
        || name.parse::<std::net::IpAddr>().is_ok()
        || (name.ends_with(".localhost"))
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

struct Query(Vec<(String, String)>);

impl Query {
    fn parse(q: &str) -> Query {
        Query(
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    let dec = |s: &str| {
                        urlencoding::decode(&s.replace('+', " "))
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| s.to_string())
                    };
                    (dec(k), dec(v))
                })
                .collect(),
        )
    }
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }
}

fn handle(mut request: Request, ctx: &Ctx) {
    if !host_allowed(header_value(&request, "Host").as_deref(), &ctx.bind_host) {
        let _ = request.respond(json_response(403, &json!({"error": "forbidden host"})));
        return;
    }
    let method = request.method().clone();
    let raw = request.url().to_string();
    let (path, query) = raw.split_once('?').unwrap_or((raw.as_str(), ""));
    let segments: Vec<String> = path
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            urlencoding::decode(s)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| s.to_string())
        })
        .collect();
    let q = Query::parse(query);

    // Body (POST only), capped: a note is a paragraph, not an upload.
    let mut body = Value::Null;
    if method == Method::Post {
        let mut buf = String::new();
        if request
            .as_reader()
            .take(1 << 20)
            .read_to_string(&mut buf)
            .is_err()
        {
            let _ = request.respond(json_response(400, &json!({"error": "unreadable body"})));
            return;
        }
        if !buf.trim().is_empty() {
            match serde_json::from_str::<Value>(&buf) {
                Ok(v) => body = v,
                Err(e) => {
                    let _ = request.respond(json_response(400, &json!({"error": format!("body is not JSON: {e}")})));
                    return;
                }
            }
        }
    }

    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    let response = match (method, parts.as_slice()) {
        (Method::Get, []) | (Method::Get, ["index.html"]) => text_response(200, "text/html; charset=utf-8", PAGE.to_string()),
        (Method::Get, ["api", "inbox"]) => {
            let (status, v) = api_inbox(ctx, &q);
            json_response(status, &v)
        }
        (Method::Get, ["api", "task", id]) => {
            let (status, v) = api_task(id, q.get("events").is_some());
            json_response(status, &v)
        }
        (Method::Get, ["api", "events"]) => match api_events(&q) {
            Ok(lines) => text_response(200, "application/x-ndjson; charset=utf-8", lines),
            Err(e) => json_response(400, &json!({"error": e.to_string()})),
        },
        (Method::Get, ["api", "lanes"]) => {
            let (status, v) = api_lanes(&q);
            json_response(status, &v)
        }
        (Method::Get, ["api", "backlog"]) => {
            let (status, v) = api_backlog(&q);
            json_response(status, &v)
        }
        (Method::Post, ["api", "task", id, verb]) => {
            let who = body
                .get("who")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| q.get("who"))
                .or_else(|| ctx.default_who.clone());
            let (status, v) = match who {
                Some(who) => api_act(id, verb, &who, &body),
                None => (400, json!({"error": "who? pass `who` in the body or start with --assignee"})),
            };
            json_response(status, &v)
        }
        (_, p) if p.first() == Some(&"api") => json_response(404, &json!({"error": "no such route"})),
        _ => text_response(404, "text/plain; charset=utf-8", "not found\n".into()),
    };
    let _ = request.respond(response);
}

fn with_replay(f: impl FnOnce(cv_core::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match task::replay() {
        Ok(o) => f(o),
        Err(e) => (500, json!({"error": e.to_string()})),
    }
}

fn api_inbox(ctx: &Ctx, q: &Query) -> (u16, Value) {
    let Some(who) = q.get("who").or_else(|| ctx.default_who.clone()) else {
        return (400, json!({"error": "who? pass ?who=<name> or start with --assignee"}));
    };
    let now = Utc::now();
    let window = if q.get("all").is_some() {
        None
    } else {
        match q.get("since") {
            Some(s) => match task::parse_since(&s, now) {
                Ok(t) => Some(t),
                Err(e) => return (400, json!({"error": e})),
            },
            None => Some(now - chrono::Duration::days(14)),
        }
    };
    with_replay(|outcome| {
        let caller = task::default_endpoint();
        let page = task_ops::inbox_page(&outcome, &who, caller.as_deref(), window, false, now);
        let mut v = serde_json::to_value(&page).expect("page serializes");
        if let Some(o) = v.as_object_mut() {
            o.insert("warnings".into(), json!(outcome.warnings));
        }
        (200, v)
    })
}

fn api_task(id: &str, events: bool) -> (u16, Value) {
    with_replay(|outcome| {
        let id = match task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return (404, json!({"error": e})),
        };
        let t = &outcome.model.tasks[&id];
        let who = t.assignee.clone().unwrap_or_default();
        let page = task_ops::inbox_page(&outcome, &who, None, None, false, Utc::now());
        let item = page
            .items
            .iter()
            .chain(page.closed.iter())
            .find(|i| i.id == id)
            .cloned();
        let mut v = json!({
            "task": t,
            "effective_state": task::effective_display(t),
            "item": item,
            "warnings": outcome.warnings,
        });
        if events {
            let history: Vec<&task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
            v["events"] = json!(history);
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
    let mut lanes = cv_core::lanes::lanes_of(&r);
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
        }),
    )
}

/// `cv task list` with no time window: the whole open backlog (or everything with `all=1`),
/// one compact row per task, newest activity first.
fn api_backlog(q: &Query) -> (u16, Value) {
    with_replay(|outcome| {
        let filter = task::TaskFilter {
            state: q.get("state"),
            assignee: q.get("assignee"),
            repo: q.get("repo").map(std::path::PathBuf::from),
            include_terminal: q.get("all").is_some(),
            tag: q.get("tag"),
            touched_since: None,
            or_involving: None,
            decisions: None,
        };
        let mut tasks = match task::list(&outcome.model, &filter) {
            Ok(t) => t,
            Err(e) => return (400, json!({"error": e})),
        };
        tasks.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
        let now = Utc::now();
        let rows: Vec<Value> = tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.task_id,
                    "short": &t.task_id[..t.task_id.len().min(13)],
                    "title": t.title,
                    "state": task::effective_display(t),
                    "assignee": t.assignee,
                    "opened_by": t.opened_by,
                    "repo": t.repo,
                    "issue": t.issue,
                    "tags": t.tags,
                    "blocked": task::is_blocked(&outcome.model, t),
                    "decision": t.decision.is_some(),
                    "notes": t.notes.len(),
                    "last_ts": t.last_ts,
                    "age": task::age_short(t.last_ts, now),
                    "body": t.body,
                })
            })
            .collect();
        let mut repos: Vec<String> = tasks
            .iter()
            .filter_map(|t| t.repo.as_ref().map(|p| p.display().to_string()))
            .collect();
        repos.sort();
        repos.dedup();
        (200, json!({"count": rows.len(), "repos": repos, "tasks": rows, "warnings": outcome.warnings}))
    })
}

fn api_events(q: &Query) -> Result<String> {
    let outcome = task::replay()?;
    let now = Utc::now();
    let task_id = match q.get("task") {
        Some(p) => Some(task::resolve_id(&outcome.model, &p).map_err(|e| anyhow::anyhow!(e))?.to_string()),
        None => None,
    };
    let f = EventFilter {
        since: Since::parse(q.get("since").as_deref(), now)?,
        kinds: q
            .get("kind")
            .map(|k| k.split(',').map(task_ops::kind_tag).collect())
            .unwrap_or_default(),
        by: q.get("by"),
        not_by: q.get("not_by"),
        assignee: q.get("assignee"),
        task: task_id,
    };
    let mut out = String::new();
    for ev in task_ops::select_events(&outcome, &f) {
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
    let outcome = task::replay()?;
    let id = task::resolve_id(&outcome.model, id)
        .map_err(|e| anyhow::anyhow!(e))?
        .to_string();
    let t = &outcome.model.tasks[&id];
    let by = format!("web:{}", who.trim());
    let store = TaskStore::default_store();
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let appended = match verb {
        "resolve" => {
            let accept = body.get("accept_default").and_then(Value::as_bool).unwrap_or(false);
            let answer = task_ops::Answer::from_flags(text("choice"), accept)?;
            let out = task_ops::resolve(&store, &outcome.model, &by, None, &id, answer, text("note"))?;
            vec![out.event]
        }
        "done" => {
            let out = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Done {
                    observed: text("observed"),
                    check: None,
                },
                Vec::new(),
            )?;
            vec![out.event]
        }
        "note" => {
            let Some(t) = text("text") else {
                bail!("a note needs text")
            };
            let out = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: t,
                    session_ref: None,
                },
                Vec::new(),
            )?;
            vec![out.event]
        }
        "discuss" => {
            let detail = text("text").unwrap_or_else(|| "(no detail)".into());
            let note = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: format!("NEEDS DISCUSSION: {detail}"),
                    session_ref: None,
                },
                Vec::new(),
            )?;
            let tag = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Tagged {
                    tags: vec!["discuss".into()],
                },
                Vec::new(),
            )?;
            vec![note.event, tag.event]
        }
        "claim" => {
            let out = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Claimed {
                    assignee: who.trim().to_string(),
                },
                Vec::new(),
            )?;
            vec![out.event]
        }
        "release" => {
            let out = task::append_and_notify(&store, Some(&id), &by, TaskEventKind::Released {}, Vec::new())?;
            vec![out.event]
        }
        "reopen" => {
            if !t.state.is_terminal() {
                bail!("task is still open");
            }
            // Terminal is terminal: the record of the close stays. The task comes back as a NEW
            // task carrying the same title/body/assignee (and, for a decision, the same options),
            // and the closed one gets a note pointing at it.
            let mut events = Vec::new();
            let new_id = match &t.decision {
                Some(d) => {
                    let (evs, _) = task_ops::pose(
                        &store,
                        &by,
                        DecisionSpec {
                            title: t.title.clone(),
                            body: t.body.clone(),
                            for_who: t.assignee.clone().unwrap_or_else(|| who.trim().to_string()),
                            default_choice: d.default_choice.clone(),
                            options: d.options.clone(),
                            deadline: None,
                            repo: t.repo.clone(),
                            issue: t.issue.clone(),
                            channel: t.channel.clone(),
                            tags: vec!["reopened".into()],
                            source: d.source.clone(),
                            blocks: Vec::new(),
                        },
                    )?;
                    let id = evs[0].task_id.clone();
                    events.extend(evs);
                    id
                }
                None => {
                    let out = task::append_and_notify(
                        &store,
                        None,
                        &by,
                        TaskEventKind::Opened {
                            title: t.title.clone(),
                            body: t.body.clone(),
                            repo: t.repo.clone(),
                            issue: t.issue.clone(),
                            channel: t.channel.clone(),
                            assignee: t.assignee.clone().or_else(|| Some(who.trim().to_string())),
                        },
                        Vec::new(),
                    )?;
                    let id = out.event.task_id.clone();
                    events.push(out.event);
                    let tagged = task::append_and_notify(
                        &store,
                        Some(&id),
                        &by,
                        TaskEventKind::Tagged {
                            tags: vec!["reopened".into()],
                        },
                        Vec::new(),
                    )?;
                    events.push(tagged.event);
                    id
                }
            };
            // Each side points at the other: the new task says where it came from, and the closed
            // one gets a post-close note (it stays closed; a note never changes state).
            let note = task::append_and_notify(
                &store,
                Some(&new_id),
                &by,
                TaskEventKind::Noted {
                    text: format!("reopened from {} ({})", &id[..13], task::effective_display(t)),
                    session_ref: None,
                },
                Vec::new(),
            )?;
            events.push(note.event);
            let back = task::append_and_notify(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: format!("reopened as {}", &new_id[..13]),
                    session_ref: None,
                },
                Vec::new(),
            )?;
            events.push(back.event);
            events
        }
        other => bail!("unknown action {other:?}"),
    };
    let after = task::replay()?;
    let state = after
        .model
        .tasks
        .get(&appended.last().map(|e| e.task_id.clone()).unwrap_or(id.clone()))
        .map(task::effective_display);
    Ok(json!({
        "ok": true,
        "by": by,
        "events": appended,
        "task_state": state,
        "warnings": after.warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_check_admits_loopback_and_ip_literals_and_refuses_dns_names() {
        assert!(host_allowed(Some("127.0.0.1:7777"), "127.0.0.1"));
        assert!(host_allowed(Some("localhost:7777"), "127.0.0.1"));
        assert!(host_allowed(Some("192.168.1.20:7777"), "0.0.0.0"), "a phone on the LAN");
        assert!(host_allowed(Some("[::1]:7777"), "127.0.0.1"));
        assert!(!host_allowed(Some("evil.example:7777"), "0.0.0.0"), "DNS rebinding vector");
        assert!(!host_allowed(None, "127.0.0.1"));
    }

    #[test]
    fn query_decodes_plus_and_percent() {
        let q = Query::parse("who=ember&since=2h&x=a+b%21");
        assert_eq!(q.get("who").as_deref(), Some("ember"));
        assert_eq!(q.get("x").as_deref(), Some("a b!"));
        assert_eq!(q.get("missing"), None);
    }
}
