//! The read API every door maps onto: a decoded path plus a raw query string in, a `(status,
//! json)` pair out. `cv serve` puts it behind HTTP (adding the Host/CORS/token model and the two
//! tantivy routes it alone can answer); the desktop app calls it in-process. Neither has a second
//! implementation of a noun, so a consumer cannot tell which door a row came through
//! (docs/INTERFACE-V2.md §3) — the rows here are the same functions `cv <cmd> --json` prints with.
//!
//! No socket or headers. Bounded source-verified read state reuses unchanged transcripts. Every error is a JSON
//! `{"error": …}` with the status that names it (400 caller mistake, 404 absent, 500 broke).

use crate::ir::{Harness, SessionRef};
use crate::Session;
use serde_json::{json, Value};

mod read;
mod selection;
mod window;
pub use selection::{message_index, parse_around, Around, MessageWindow};
pub use window::WindowSink;

/// A router's arms spelled once: the `match` that answers and the method/path patterns
/// `cv schema --json` lists under `routes`, so the compatibility gate probes the code's own table.
/// Each arm names its method; a binding or `_` prints as `{}`, a rest pattern as `*`.
#[macro_export]
macro_rules! routes {
    (@seg $l:literal) => { $l };
    (@seg ..) => { "*" };
    (@seg $i:tt) => { "{}" };
    (@post GET) => { false };
    (@post POST) => { true };
    ($vis:vis const $inventory:ident;
     $(#[$meta:meta])* $fvis:vis fn $name:ident($post:ident, $parts:ident $(, $arg:ident: $ty:ty)*) -> $ret:ty {
        $( $( $method:ident [$($seg:tt),*] )|+ => $body:expr, )*
        _ => $fallback:expr $(,)?
    }) => {
        $vis const $inventory: &[&str] =
            &[$( $( concat!(stringify!($method), " ", $("/", $crate::routes!(@seg $seg)),*) ),+ ),*];
        $(#[$meta])*
        $fvis fn $name($post: bool, $parts: &[&str] $(, $arg: $ty)*) -> $ret {
            match ($post, $parts) {
                $( $( ($crate::routes!(@post $method), [$($seg),*]) )|+ => $body, )*
                _ => $fallback,
            }
        }
    };
}

/// Route a decoded path (`["api", "session", "claude", "abc"]`) plus a raw query string.
/// Never panics; an unknown path is a 404.
pub fn route_json(parts: &[&str], query: &str) -> (u16, Value) {
    let _reads = crate::source::read_scope();
    read(false, parts, query)
}

routes! {
    pub const ROUTES;
    fn read(_post, parts, query: &str) -> (u16, Value) {
        GET ["api", "blame"] => blame(query),

        GET ["api", "health"] => ok(json!({
            "ok": true,
            "harnesses": Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        })),

        GET ["api", "sessions"] => sessions(query),

        GET ["api", "places"] => places(query),

        GET ["api", "session", harness, id] => session(harness, id, query),

        GET ["api", "session", harness, id, "head"] => session_head(harness, id),

        GET ["api", "session", harness, id, "overview"] => session_overview(harness, id),

        GET ["api", "session", harness, id, "messages"] => messages(harness, id, query),

        GET ["api", "session", harness, id, "events"] => session_events(harness, id, query),

        GET ["api", "session", harness, id, "compactions"] => compactions(harness, id),

        GET ["api", "session", harness, id, "context"] => context(harness, id, query),

        GET ["api", "session", harness, id, "files"] => files(harness, id, query),

        GET ["api", "session", harness, id, "prompts"] => prompts(harness, id, query),

        GET ["api", "touched"] => touched(query),

        GET ["api", "session", harness, id, "subagents"] => subagents(harness, id, query),

        GET ["api", "session", harness, parent, "subagent", agent] => subagent(harness, parent, agent),

        GET ["api", "session", harness, id, "workflow", wf, "script"] => workflow_script(harness, id, wf),

        GET ["api", "tasks"] => tasks(query),

        GET ["api", "tasks", "inbox", who] => tasks_inbox(who, query),

        GET ["api", "task", id] => task_one(id, Query::parse(query).flag("events")),

        GET ["api", "task", id, "events"] => task_events(id),

        _ => err(404, "not found"),
    }
}

/// A completed staged response. Its payload stays operation-owned until HTTP publication.
pub struct StagedJson {
    pub status: u16,
    pub body: crate::bytes::ByteSource,
    pub bytes: u64,
}

/// Bounded backend for the HTTP message-window route. Other routes keep `route_json`.
/// Source and serialization failures are converted into a staged error before publication.
pub fn route_staged(parts: &[&str], query: &str) -> anyhow::Result<Option<StagedJson>> {
    let _reads = crate::source::read_scope();
    let ["api", "session", harness, id, "messages"] = parts else {
        return Ok(None);
    };
    let result = message_window(harness, id, query);
    let mut output = crate::bytes::ByteStage::default();
    let mut counted = crate::projection::Counted::new(&mut output);
    let position = counted.position.clone();
    let status = match result {
        Ok((r, mut sink)) => {
            let frames = sink.finish()?;
            let envelope = window_envelope(&r, &sink, Value::Array(Vec::new()));
            let framing = crate::projection::Framing {
                total: Some(sink.selected),
                ..Default::default()
            };
            crate::projection::write_frames(&mut counted, &envelope, "messages", &frames, framing)?;
            anyhow::ensure!(
                sink.reads.fresh(&mut crate::source::Probe::default()),
                "API window source changed while staging response"
            );
            200
        }
        Err((status, error)) => {
            serde_json::to_writer(&mut counted, &error)?;
            status
        }
    };
    let bytes = position.get();
    Ok(Some(StagedJson {
        status,
        body: output.finish()?,
        bytes,
    }))
}

// --- the task log -------------------------------------------------------------------------------

/// Replay the task log; on success hand the outcome to `f`. Log warnings ride every response so
/// a corrupted coordination log is visible from any consumer.
fn with_tasks(f: impl FnOnce(&crate::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match crate::task::replay() {
        Ok(outcome) => f(&outcome),
        Err(e) => err(500, &e.to_string()),
    }
}

/// [`with_tasks`] for a route that reads only the model: the log's stored fold and the events
/// appended since ([`crate::task::fold`]), with the same warnings.
fn with_folded(f: impl FnOnce(&crate::task::Folded) -> (u16, Value)) -> (u16, Value) {
    match crate::task::fold() {
        Ok(folded) => f(&folded),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/tasks?state=&assignee=&repo=&tag=&all=&detail=` — task projections, oldest first;
/// `detail=1` adds the backlog columns (`opened_by`, `issue`, `body`, `notes`, `decision`, `blocked`).
fn tasks(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let detail = params.flag("detail");
    with_folded(|outcome| {
        let filter = crate::task::TaskFilter {
            state: params.get("state"),
            assignee: params.get("assignee"),
            repo: params.get("repo").map(std::path::PathBuf::from),
            include_terminal: params.flag("all"),
            tag: params.get("tag"),
            // The wire sees the whole store: scoping is the shell's default.
            ..Default::default()
        };
        // An unknown state string is a clear 400 (naming the vocabulary), not a silent [].
        let tasks: Vec<crate::task::TaskRow> = match crate::task::list(&outcome.model, &filter) {
            Ok(tasks) => tasks
                .into_iter()
                .map(|t| {
                    if detail {
                        crate::task::TaskRow::detailed(t, &outcome.model)
                    } else {
                        crate::task::TaskRow::full(t)
                    }
                })
                .collect(),
            Err(e) => return err(400, &e),
        };
        ok(json!({ "tasks": tasks, "warnings": outcome.warnings }))
    })
}

/// `GET /api/task/{id}[?events=1]` — one task's full projection, plus its raw history on request.
fn task_one(id: &str, with_events: bool) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let t = &outcome.model.tasks[&id];
        let mut v = json!({
            "task": t,
            "effective_state": crate::task::effective_display(t),
            "warnings": outcome.warnings,
        });
        if with_events {
            let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
            v["events"] = json!(history);
        }
        ok(v)
    })
}

/// `GET /api/task/{id}/events` — the raw durable history of one task.
fn task_events(id: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
        ok(json!({ "events": history, "warnings": outcome.warnings }))
    })
}

/// `GET /api/tasks/inbox/{who}?since=&all=` — what needs this endpoint; the whole store unless
/// `since` windows it.
fn tasks_inbox(who: &str, query: &str) -> (u16, Value) {
    let now = chrono::Utc::now();
    let window = match inbox_window(&Query::parse(query), None, now) {
        Ok(window) => window,
        Err(e) => return err(400, &e),
    };
    with_tasks(|outcome| {
        let entries = crate::task::inbox_page(outcome, who, None, window, false, now).items;
        ok(json!({ "inbox": entries, "warnings": outcome.warnings }))
    })
}

/// The inbox window a query asks for: `all` lifts it, `since` (`2h`, `3d`, an instant) sets it,
/// else `default` back from `now`. Every inbox route windows through this one reading.
pub fn inbox_window(
    params: &Query,
    default: Option<chrono::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    if params.flag("all") {
        return Ok(None);
    }
    match params.get("since") {
        Some(since) => crate::task::parse_since(&since, now).map(Some),
        None => Ok(default.map(|back| now - back)),
    }
}

// --- sessions -----------------------------------------------------------------------------------

pub use crate::query::glob_match;

pub(crate) fn parse_live_ref(r: &SessionRef) -> Option<Session> {
    if let Ok(Some((found, adapter))) = crate::find(&r.id, Some(r.harness)) {
        return adapter.parse(&found).ok();
    }
    None
}

#[cfg(feature = "sqlite")]
pub fn hub_head_to_ref(
    packs_dir: &std::path::Path,
    entry: &crate::state::hub::SessionRevisionEntry,
) -> Option<SessionRef> {
    if let Ok(Some(backing)) = crate::custody::backing_from_hub_head(packs_dir, entry.clone()) {
        return Some(backing.reference);
    }
    let harness = Harness::parse(&entry.harness)?;
    Some(SessionRef {
        id: entry.selector_string(),
        harness,
        path: std::path::PathBuf::new(),
        cwd: entry.cwd.as_deref().map(std::path::PathBuf::from),
        title: entry.title.clone(),
        created_at: entry.created_at.as_deref().and_then(|t| t.parse().ok()),
        updated_at: entry.updated_at.as_deref().and_then(|t| t.parse().ok()),
        message_count: usize::try_from(entry.message_count).ok(),
    })
}

#[cfg(feature = "sqlite")]
fn api_authority_error(err: crate::custody::AuthorityEvalError) -> (u16, Value) {
    use crate::custody::AuthorityEvalError;
    use crate::state::hub::HubError;
    match err {
        AuthorityEvalError::BadDuration(msg) => self::err(400, &msg),
        AuthorityEvalError::GenerationNotFound(spec) => self::err(404, &format!("snapshot `{spec}` not found")),
        AuthorityEvalError::Staleness(v) => (
            503,
            json!({
                "error": v.message,
                "authority": v.authority,
            }),
        ),
        AuthorityEvalError::Hub(HubError::InvalidArgument(msg)) => self::err(400, &msg),
        AuthorityEvalError::Hub(HubError::NotFound(msg)) => self::err(404, &msg),
        AuthorityEvalError::Hub(other) => self::err(503, &other.to_string()),
    }
}

#[cfg(feature = "sqlite")]
fn evaluate_api_query_authority(
    params: &Query,
    has_negation: bool,
    indexed_only: bool,
) -> Result<(Option<u64>, crate::state::hub::AuthorityEnvelope), (u16, Value)> {
    let place = params.get("place").or_else(|| params.get("producer"));
    let as_of = params.get("as_of").or_else(|| params.get("snapshot"));
    let max_staleness = params.get("max_staleness");
    let cwd = params.get("cwd");
    let q_str = params.get("q");
    crate::custody::evaluate_authority(
        place.as_deref(),
        as_of.as_deref(),
        max_staleness.as_deref(),
        cwd.as_deref(),
        q_str.as_deref(),
        has_negation,
        indexed_only,
    )
    .map_err(api_authority_error)
}

#[cfg(feature = "sqlite")]
fn has_authority_params(params: &Query) -> bool {
    params.get("place").or_else(|| params.get("producer")).is_some()
        || params.get("as_of").or_else(|| params.get("snapshot")).is_some()
        || params.get("max_staleness").is_some()
        || params.flag("authority")
}

#[cfg(feature = "sqlite")]
fn scoped_view_from_params(params: &Query) -> Result<Option<crate::custody::ScopedView>, (u16, Value)> {
    if !has_authority_params(params) {
        return Ok(None);
    }
    let (as_of_gen, env) = evaluate_api_query_authority(params, false, false)?;
    let hub = crate::state::hub::HubStore::default_hub().map_err(|e| err(503, &e.to_string()))?;
    Ok(Some(crate::custody::ScopedView::new(hub, env, as_of_gen.is_some())))
}

#[cfg(feature = "sqlite")]
fn scoped_resolve_reply(error: crate::custody::ScopedResolveError) -> (u16, Value) {
    match error {
        crate::custody::ScopedResolveError::NotFound(_) => err(404, "session not found"),
        crate::custody::ScopedResolveError::Ambiguous(spec) => err(400, &format!("ambiguous session id {spec:?}")),
        crate::custody::ScopedResolveError::Other(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/places?place=&as_of=&max_staleness=` — enrolled places, watermarks, and authority status.
fn places(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    #[cfg(feature = "sqlite")]
    {
        match evaluate_api_query_authority(&params, false, false) {
            Ok((_, env)) => ok(json!({
                "places": env.places,
                "authority": env,
            })),
            Err(reply) => reply,
        }
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = params;
        ok(json!({
            "places": [],
            "authority": {
                "generation": 0,
                "indexed_generation": 0,
                "status": "partial",
                "places": [],
                "omissions": [],
            }
        }))
    }
}

/// `GET /api/sessions?harness=&cwd=&place=&as_of=&max_staleness=&authority=1&q=&offset=&limit=` —
/// the §3 session rows, filtered, newest activity first.
fn sessions(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let harness = match params.get("harness") {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            // Validate up front so a typo is a clear 400 rather than silently empty.
            None => return err(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };
    let cwd = params.get("cwd");
    let place = params.get("place").or_else(|| params.get("producer"));
    #[cfg(not(feature = "sqlite"))]
    let as_of = params.get("as_of").or_else(|| params.get("snapshot"));
    let parsed_q = match params.get("q") {
        Some(raw) if !raw.trim().is_empty() => match crate::query::SessionQuery::parse(&raw) {
            Ok(sq) => Some(sq),
            Err(e) => return err(400, &e),
        },
        _ => None,
    };
    let offset = match params.unsigned("offset") {
        Ok(v) => v.unwrap_or(0),
        Err(error) => return err(400, &error),
    };
    let limit = match params.unsigned("limit") {
        Ok(limit) => limit,
        Err(error) => return err(400, &error),
    };

    #[cfg(feature = "sqlite")]
    let (resolved_as_of_gen, scope) = if has_authority_params(&params) {
        let has_neg = parsed_q.as_ref().is_some_and(crate::query::SessionQuery::has_negation);
        match evaluate_api_query_authority(&params, has_neg, false) {
            Ok((g, env)) => (g, Some(env)),
            Err(reply) => return reply,
        }
    } else {
        (None, None)
    };
    #[cfg(not(feature = "sqlite"))]
    let resolved_as_of_gen: Option<u64> = if as_of.is_some() {
        return err(400, "as_of requires sqlite support");
    } else {
        None
    };

    #[allow(unused_mut)]
    let mut selector_places: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let mut refs: Vec<SessionRef> = if let Some(gen) = resolved_as_of_gen {
        #[cfg(feature = "sqlite")]
        {
            let Ok(hub) = crate::state::hub::HubStore::default_hub() else {
                return err(404, "snapshot not found");
            };
            let heads = match hub.heads_at_generation(Some(gen)) {
                Ok(h) => h,
                Err(e) => return err(500, &e.to_string()),
            };
            let mut refs = Vec::new();
            for entry in &heads {
                if let Some(q) = &parsed_q {
                    match entry.matches_query(hub.packs_dir(), q) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => return err(500, &e.to_string()),
                    }
                }
                selector_places.insert(entry.selector_string(), entry.producer_id.clone());
                refs.extend(hub_head_to_ref(hub.packs_dir(), entry));
            }
            refs
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = gen;
            unreachable!()
        }
    } else {
        // The fast catalog read (~ms when warm); transparently escalates to a full scan when cold.
        let mut live = crate::sessions();
        #[cfg(feature = "sqlite")]
        if let Ok(hub) = crate::state::hub::HubStore::default_hub() {
            if let Ok(heads) = hub.heads_at_generation(None) {
                if !heads.is_empty() {
                    let local_prods = hub.local_shipped_producers().unwrap_or_default();
                    let live_triples: std::collections::HashSet<_> = live
                        .iter()
                        .filter(|r| !crate::custody::is_pack_ref(r))
                        .map(|r| (crate::SessionKey::local(r).profile, r.harness, r.id.clone()))
                        .collect();
                    let seen: std::collections::HashSet<String> =
                        live.iter().map(SessionRef::session_selector).collect();
                    for entry in &heads {
                        let sel = entry.selector_string();
                        selector_places.insert(sel.clone(), entry.producer_id.clone());
                        if !seen.contains(&sel) {
                            let is_shadow = local_prods.contains(&entry.producer_id)
                                && entry
                                    .session_key_struct()
                                    .is_some_and(|k| live_triples.contains(&(k.profile, k.harness, k.native_id)));
                            if !is_shadow {
                                if let Some(r) = hub_head_to_ref(hub.packs_dir(), entry) {
                                    live.push(r);
                                }
                            }
                        }
                    }
                }
            }
        }
        live
    };

    if let Some(h) = harness {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .is_some_and(|c| c.to_string_lossy().contains(needle.as_str()))
        });
    }
    if let Some(ref _pattern) = place {
        refs.retain(|r| {
            let p = selector_places.get(&r.id).map_or("local", String::as_str);
            #[cfg(feature = "sqlite")]
            {
                scope.as_ref().is_some_and(|s| s.includes(p))
            }
            #[cfg(not(feature = "sqlite"))]
            {
                glob_match(_pattern, p)
            }
        });
    }
    if let Some(sq) = parsed_q.as_ref().filter(|_| resolved_as_of_gen.is_none()) {
        refs.retain(|r| {
            if !sq.prefilter(r) {
                return false;
            }
            if sq.eval(&crate::query::Facts::from_ref(r)).ok() == Some(crate::query::Tri::True) {
                return true;
            }
            let hydrated = parse_live_ref(r);
            crate::query::matches_session_or_ref(sq, r, hydrated.as_ref())
        });
    }
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });

    let sliced: Vec<&SessionRef> = match limit {
        Some(n) => refs.iter().skip(offset).take(n).collect(),
        None => refs.iter().skip(offset).collect(),
    };
    const METADATA_BATCH_SIZE: usize = 256;
    let mut probe = crate::source::Probe::default();
    let rows: Vec<Value> = sliced
        .into_iter()
        .enumerate()
        .map(|(idx, r)| {
            if idx > 0 && idx % METADATA_BATCH_SIZE == 0 {
                probe = crate::source::Probe::default();
            }
            let (size, store) = if r.path.as_os_str().is_empty() {
                (None, None)
            } else {
                crate::rows::ref_sizes(&mut probe, r)
            };
            crate::rows::session_row_with_store(r, size, store)
        })
        .collect();

    #[cfg(feature = "sqlite")]
    if let Some(env) = scope.filter(|_| params.flag("authority")) {
        return ok(json!({ "sessions": rows, "authority": env }));
    }
    ok(Value::Array(rows))
}

/// A resolution failure as a reply: 404 unknown, 400 ambiguous, 500 otherwise.
fn resolve_reply(e: anyhow::Error) -> (u16, Value) {
    err(crate::ResolveError::status(&e), &format!("{e:#}"))
}

/// Look up one session by harness name + id (or unique prefix), with each failure already a reply.
fn lookup(harness: &str, id: &str) -> Result<(SessionRef, Box<dyn crate::Adapter>), (u16, Value)> {
    let h = Harness::parse(harness).ok_or_else(|| err(400, &format!("unknown harness {harness:?}")))?;
    match crate::find(id, Some(h)) {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(err(404, "session not found")),
        Err(e) => Err(resolve_reply(e)),
    }
}

/// `GET /api/session/{harness}/{id}` — the full parsed session (the IR), or 404.
// Both full owned-session doors use the same full-fidelity collector as Adapter::parse,
// but retain its operation receipt until JSON projection finishes and is safe to publish.
fn complete_session_json(adapter: &dyn crate::Adapter, reference: &SessionRef) -> anyhow::Result<Value> {
    let mut sink = crate::stream::CollectSink::default();
    let pass = crate::stream::Pass {
        guard: true,
        ..crate::stream::Pass::RECORD
    };
    let read = crate::stream::stream_counted(adapter, reference, &crate::Demand::RICH, &mut sink, pass)?;
    let mut session = read.session;
    session.messages = sink.messages;
    session.materialize()?;
    let output = serde_json::to_value(&session)?;
    read.guard.expect("guarded pass").verify(reference)?;
    Ok(output)
}

fn session_overview(harness: &str, id: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(found) => found,
        Err(error) => return error,
    };
    match crate::overview::read(&r, &Default::default()).and_then(|overview| Ok(serde_json::to_value(overview)?)) {
        Ok(value) => ok(value),
        Err(error) => err(500, &format!("overview read failed: {error:#}")),
    }
}

#[cfg(feature = "sqlite")]
fn hub_session_json(
    hub: &crate::state::hub::HubStore,
    want_harness: Harness,
    id: &str,
    as_of_spec: Option<&str>,
) -> Option<(u16, Value)> {
    let gen_rec = match hub.resolve_generation(as_of_spec) {
        Ok(Some(g)) => g,
        Ok(None) if as_of_spec.is_none() => return None,
        Ok(None) | Err(crate::state::hub::HubError::NotFound(_)) => {
            return Some(err(404, &format!("snapshot `{}` not found", as_of_spec.unwrap_or(""))))
        }
        Err(e) => return Some(err(400, &e.to_string())),
    };
    let matches = match hub.resolve_session_heads(id, Some(want_harness), Some(gen_rec.generation), None) {
        Ok(m) => m,
        Err(e) => return Some(err(500, &e.to_string())),
    };
    let entry = match matches.as_slice() {
        [one] => one.clone(),
        [] => {
            return if as_of_spec.is_some() {
                Some(err(404, "session not found"))
            } else {
                None
            };
        }
        _ => return Some(err(400, &format!("ambiguous session id {id:?}"))),
    };

    match crate::custody::backing_from_hub_head(hub.packs_dir(), entry.clone()) {
        Ok(Some(backing)) => match complete_session_json(&backing, &backing.reference) {
            Ok(mut val) => {
                val["extra"]["cv"]["payload_unavailable"] = json!(false);
                val["extra"]["cv"]["producer_id"] = json!(entry.producer_id);
                val["extra"]["cv"]["revision"] = json!(entry.revision);
                val["extra"]["cv"]["snapshot_id"] = json!(gen_rec.publication_id);
                val["extra"]["cv"]["hub_generation"] = json!(gen_rec.generation);
                Some(ok(val))
            }
            Err(e) => Some(err(500, &format!("session read failed: {e:#}"))),
        },
        Ok(None) => {
            let native_id = entry
                .metadata
                .get("native_id")
                .and_then(Value::as_str)
                .unwrap_or(&entry.session_id)
                .to_string();
            let mut s = Session::new(native_id, want_harness);
            s.title = entry.title.clone();
            s.cwd = entry.cwd.as_deref().map(std::path::PathBuf::from);
            s.model = entry.model.clone();
            s.created_at = entry
                .created_at
                .as_deref()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            s.updated_at = entry
                .updated_at
                .as_deref()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            let mut val = serde_json::to_value(&s).unwrap_or_else(|_| json!({}));
            val["extra"]["cv"] = json!({
                "payload_unavailable": true,
                "reason": "pack_not_mirrored",
                "producer_id": entry.producer_id,
                "session_id": entry.session_id,
                "revision": entry.revision,
                "snapshot_id": gen_rec.publication_id,
                "hub_generation": gen_rec.generation,
                "message_count": entry.message_count,
                "metadata": entry.metadata,
            });
            Some(ok(val))
        }
        Err(e) => Some(err(500, &format!("session read failed: {e:#}"))),
    }
}

fn session(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let as_of = params.get("as_of").or_else(|| params.get("snapshot"));
    let Some(want_harness) = Harness::parse(harness) else {
        return err(400, &format!("unknown harness {harness:?}"));
    };
    #[cfg(feature = "sqlite")]
    if as_of.is_some() {
        let Ok(hub) = crate::state::hub::HubStore::default_hub() else {
            return err(404, "session not found");
        };
        return hub_session_json(&hub, want_harness, id, as_of.as_deref())
            .unwrap_or_else(|| err(404, "session not found"));
    }
    #[cfg(not(feature = "sqlite"))]
    if as_of.is_some() {
        return err(400, "as_of requires sqlite support");
    }

    match lookup(harness, id) {
        Ok((r, adapter)) => match complete_session_json(adapter.as_ref(), &r) {
            Ok(value) => ok(value),
            Err(error) => err(500, &format!("session read failed: {error:#}")),
        },
        Err((404, _)) => {
            #[cfg(feature = "sqlite")]
            if let Ok(hub) = crate::state::hub::HubStore::default_hub() {
                if let Some(reply) = hub_session_json(&hub, want_harness, id, None) {
                    return reply;
                }
            }
            err(404, "session not found")
        }
        Err(other) => other,
    }
}

/// `GET /api/session/{harness}/{id}/head` — everything a session knows about ITSELF, and no
/// messages: the §3 row plus `model`, `git`, `system_prompt`, `lineage`, `extra` and the exact
/// message `total`.
///
/// Why this exists rather than folding into the windowed read: a window stops streaming once it
/// is full, so a session-level fact recorded LATER in the transcript — Claude writes its
/// `prompt_snapshot` well after the opening turns — is simply not reached, and asking the window
/// to keep scanning would make every transcript open pay for the whole file. One cheap pass that
/// keeps no messages answers it instead.
fn session_head(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let head = match read::head(read::shared(), adapter.as_ref(), &r) {
        Ok(head) => head,
        Err(e) => return err(500, &format!("stream failed: {e:#}")),
    };
    let mut probe = crate::source::Probe::default();
    let (size, store) = crate::rows::ref_sizes(&mut probe, &r);
    let mut row = crate::rows::session_row_with_store(&r, size, store);
    let map = row.as_object_mut().expect("session_row is an object");
    let Value::Object(head) = head else {
        unreachable!("head metadata is an object")
    };
    for (key, value) in head {
        map.insert(key, value);
    }
    ok(row)
}

/// The `session` object a windowed read returns. Whatever a session knows about itself has to
/// arrive here or it never reaches a reader: the windowed path is the one every transcript open
/// uses.
fn session_meta_json(s: &Session) -> Value {
    let mut meta = json!({
        "title": s.title,
        "model": s.model,
        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
        "system_prompt": s.system_prompt,
        "lineage": s.lineage,
        "git": s.git,
    });
    if let Some(principal) = &s.principal {
        meta["principal"] = json!(principal);
    }
    update_read_coverage(&mut meta, s);
    meta
}

// Read coverage is session metadata, including for lean windows that omit message extras.
// Unlike the initial title/model, evidence may grow after the first metadata callback.
fn update_read_coverage(meta: &mut Value, s: &Session) {
    if let Some(coverage) = s.cv_extra().and_then(|extra| extra.get("read_coverage")) {
        meta["extra"]["cv"]["read_coverage"] = coverage.clone();
    }
}

/// `GET /api/session/{harness}/{id}/messages?start=&end=&around=&context=&extra=` — the window
/// `[start, end)` (or `around` index/message-id ± `context`, default 5) of the session's messages
/// as IR JSON, without parsing the rest of a huge transcript.
///
/// A lazy stream that skips the messages before `start` without materializing them and **stops
/// at `end`** (the `cv show --range` pattern), so the tail is never read.
/// The reply carries the window's actual indices, `total_known` (+ `total` when the stream
/// reached EOF so the count is exact), and `has_more`.
///
/// `extra=1` additionally keeps each message's harness `extra` map (e.g. `compactMetadata` on a
/// compaction boundary) and the structured tool-result `details` sidecar — the structure view
/// pages through with this on to find the seams.
fn message_window(harness: &str, id: &str, query: &str) -> Result<(SessionRef, WindowSink), (u16, Value)> {
    let params = Query::parse(query);
    // Window bounds must parse cleanly — a typo'd index is a clear 400, not a silent full read.
    let idx_param = |key: &str| params.unsigned(key).map_err(|error| err(400, &error));
    let raw_start = idx_param("start")?;
    let raw_end = idx_param("end")?;
    let context = idx_param("context")?;
    let around = match params.raw("around") {
        Some(spec) => Some(parse_around(spec).map_err(|error| err(400, &format!("around: {error}")))?),
        None => None,
    };
    if context.is_some() && around.is_none() {
        return Err(err(400, "context requires around"));
    }
    if around.is_some() && (raw_start.is_some() || raw_end.is_some()) {
        return Err(err(400, "around cannot be combined with start or end"));
    }
    let mut start = raw_start.unwrap_or(0);
    let mut end = raw_end;
    if end.is_some_and(|e| e < start) {
        return Err(err(400, "end must be >= start"));
    }
    let want_extra = params.flag("extra");

    let (r, adapter) = lookup(harness, id)?;
    if let Some(around) = around {
        let center = match around {
            Around::Index(n) => n,
            Around::Id(msg_id) => match message_index(adapter.as_ref(), &r, &msg_id) {
                Ok(Some(at)) => at,
                Ok(None) => return Err(err(400, &format!("no message {msg_id:?} in session {}", r.id))),
                Err(e) => return Err(err(500, &format!("stream failed: {e:#}"))),
            },
        };
        let ctx = context.unwrap_or(5);
        start = center.saturating_sub(ctx);
        end = Some(center.saturating_add(ctx).saturating_add(1));
    }

    let (_, sink) = match read::window(read::shared(), adapter.as_ref(), &r, start, end, want_extra) {
        Ok(value) => value,
        Err(e) => return Err(err(500, &format!("stream failed: {e:#}"))),
    };
    Ok((r, sink))
}

fn window_envelope(r: &SessionRef, sink: &WindowSink, messages: Value) -> Value {
    let total_known = !sink.more;
    json!({
        "harness": r.harness.as_str(), "id": r.id, "start": sink.start,
        "end": sink.start + sink.selected, "has_more": sink.more, "total_known": total_known,
        "total": total_known.then_some(sink.idx), "message_count": r.message_count,
        "session": sink.meta, "messages": messages,
    })
}

fn messages(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let (r, mut sink) = match message_window(harness, id, query) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let messages = match sink.values() {
        Ok(values) => Value::Array(values),
        Err(e) => return err(500, &format!("stream failed: {e:#}")),
    };
    ok(window_envelope(&r, &sink, messages))
}

#[cfg(feature = "sqlite")]
pub fn session_in_places(r: &SessionRef, scope: &crate::state::hub::AuthorityEnvelope) -> anyhow::Result<bool> {
    let place = if crate::custody::is_pack_ref(r) {
        crate::state::hub::HubStore::default_hub()?
            .session_head_for(&r.id, None)?
            .map(|e| e.producer_id)
    } else {
        None
    };
    Ok(scope.includes(place.as_deref().unwrap_or("local")))
}
/// `GET /api/session/{harness}/{id}/events?kind=&place=&as_of=&max_staleness=&authority=1` — the session's
/// extracted tool events (file edits/reads, commands, errors) from the event catalog, in transcript
/// order, as `cv events --json` rows. A stale or missing catalog row is (re)ingested on the spot.
fn session_events(harness: &str, id: &str, query: &str) -> (u16, Value) {
    use crate::events;
    let params = Query::parse(query);
    let kind = params.get("kind");

    #[cfg(not(feature = "sqlite"))]
    if params.get("as_of").or_else(|| params.get("snapshot")).is_some() {
        return err(400, "as_of requires sqlite support");
    }

    #[cfg(feature = "sqlite")]
    if let Some(view) = match scoped_view_from_params(&params) {
        Ok(v) => v,
        Err(reply) => return reply,
    } {
        let Some(want_harness) = Harness::parse(harness) else {
            return err(400, &format!("unknown harness {harness:?}"));
        };
        let src = match view.resolve(id, Some(want_harness)) {
            Ok(s) => s,
            Err(e) => return scoped_resolve_reply(e),
        };
        let rows = if view.pinned || src.entry.is_some() {
            if src.r.path.as_os_str().is_empty() {
                return err(404, "session payload not mirrored");
            }
            match view.event_rows(&src) {
                Ok(mut rows) => {
                    if let Some(ref k) = kind {
                        rows.retain(|r| &r.kind == k);
                    }
                    rows
                }
                Err(e) => return err(500, &format!("event extraction failed: {e:#}")),
            }
        } else {
            match events::read_ref(&src.r, kind.as_deref()) {
                Ok(rows) => rows,
                Err(e) => return err(500, &format!("event catalog: {e:#}")),
            }
        };
        let out: Vec<Value> = rows.iter().map(crate::rows::event_row).collect();
        if params.flag("authority") {
            return ok(json!({ "events": out, "authority": *view.envelope.borrow() }));
        }
        return ok(json!(out));
    }

    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let rows = match events::read_ref(&r, kind.as_deref()) {
        Ok(rows) => rows,
        Err(e) => return err(500, &format!("event catalog: {e:#}")),
    };
    let out: Vec<Value> = rows.iter().map(crate::rows::event_row).collect();
    ok(json!(out))
}

/// Projected rows of one session, selected the way every `show --kind` view selects them:
/// `start`/`end`, `first`, and `last` select projected rows; `around` selects by original
/// transcript index/message ID, with `context` rows on either side.
fn kind_rows_from<T>(
    r: &SessionRef,
    adapter: &dyn crate::Adapter,
    params: &Query,
    read: impl FnOnce(&dyn crate::Adapter, &SessionRef) -> anyhow::Result<Vec<T>>,
    index: impl Fn(&T) -> usize,
) -> Result<Vec<T>, (u16, Value)> {
    let number = |key| params.unsigned(key).map_err(|e| err(400, &e));
    let start = number("start")?;
    let end = number("end")?;
    let first = number("first")?;
    let last = number("last")?;
    let nearby = number("context")?;
    let around = params.get("around");
    let families = usize::from(start.is_some() || end.is_some())
        + usize::from(first.is_some())
        + usize::from(last.is_some())
        + usize::from(around.is_some());
    if families > 1 {
        return Err(err(400, "choose start/end, first, last, or around"));
    }
    if nearby.is_some() && around.is_none() {
        return Err(err(400, "context requires around"));
    }
    if end.is_some_and(|end| end < start.unwrap_or(0)) {
        return Err(err(400, "end must be >= start"));
    }
    let around = match around {
        None => None,
        Some(value) => {
            let at = match parse_around(&value).map_err(|e| err(400, &e))? {
                Around::Index(index) => index,
                Around::Id(id) => message_index(adapter, r, &id)
                    .map_err(|e| err(500, &format!("stream failed: {e:#}")))?
                    .ok_or_else(|| err(400, &format!("no message {id:?} in session {}", r.id)))?,
            };
            Some((at, nearby.unwrap_or(5)))
        }
    };
    let rows = read(adapter, r).map_err(|e| err(500, &format!("stream failed: {e:#}")))?;
    Ok(crate::context::window(
        rows,
        crate::context::ContextWindow {
            start: start.unwrap_or(0),
            end: first.or(end),
            last,
            around,
        },
        index,
    ))
}

fn kind_rows<T>(
    harness: &str,
    id: &str,
    query: &str,
    read: impl FnOnce(&dyn crate::Adapter, &SessionRef) -> anyhow::Result<Vec<T>>,
    index: impl Fn(&T) -> usize,
) -> Result<Vec<T>, (u16, Value)> {
    let params = Query::parse(query);
    let (r, adapter) = lookup(harness, id)?;
    kind_rows_from(&r, adapter.as_ref(), &params, read, index)
}

/// Saved usage and compaction rows ([`kind_rows`] selection). Returns the same array as
/// `show --kind context --json`.
fn context(harness: &str, id: &str, query: &str) -> (u16, Value) {
    match kind_rows(harness, id, query, crate::context::read, |row| row.index) {
        Ok(rows) => ok(json!(rows)),
        Err(error) => error,
    }
}

/// Human prompts and question-tool answers ([`kind_rows`] selection, optional `kind=prompt,answer`).
/// Returns the same `prompt_row` array as `show --kind prompt,answer --json`.
fn prompts(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let (want_prompts, want_answers) = match params.raw("kind") {
        None => (true, true),
        Some(raw) => {
            let (mut p, mut a) = (false, false);
            for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match part {
                    "prompt" | "prompts" => p = true,
                    "answer" | "answers" => a = true,
                    other => return err(400, &format!("unknown prompt kind {other:?}: prompt or answer")),
                }
            }
            if !p && !a {
                return err(400, "empty kind parameter: expected prompt or answer");
            }
            (p, a)
        }
    };
    match kind_rows(
        harness,
        id,
        query,
        |adapter, r| crate::prompts::read_filtered(adapter, r, None, want_prompts, want_answers),
        |row| row.index,
    ) {
        Ok(rows) => ok(json!(rows)),
        Err(error) => error,
    }
}

/// The file-touch timeline ([`kind_rows`] selection): the same array as `show --kind files --json`,
/// supporting `?place=&as_of=&max_staleness=&authority=1`.
fn files(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    #[cfg(not(feature = "sqlite"))]
    if params.get("as_of").or_else(|| params.get("snapshot")).is_some() {
        return err(400, "as_of requires sqlite support");
    }
    #[cfg(feature = "sqlite")]
    if let Some(view) = match scoped_view_from_params(&params) {
        Ok(v) => v,
        Err(reply) => return reply,
    } {
        let Some(want_harness) = Harness::parse(harness) else {
            return err(400, &format!("unknown harness {harness:?}"));
        };
        let src = match view.resolve(id, Some(want_harness)) {
            Ok(s) => s,
            Err(e) => return scoped_resolve_reply(e),
        };
        if src.entry.is_some() && src.r.path.as_os_str().is_empty() {
            return err(404, "session payload not mirrored");
        }
        let adapter = match view.adapter(&src) {
            Ok(a) => a,
            Err(e) => return err(500, &format!("session reader unavailable: {e:#}")),
        };
        let checkout = !view.pinned && !crate::custody::is_pack_ref(&src.r);
        return match kind_rows_from(
            &src.r,
            adapter.as_ref(),
            &params,
            |a, r| crate::files::read_with_checkout(a, r, checkout),
            |touch| touch.msg_idx,
        ) {
            Ok(rows) => {
                if params.flag("authority") {
                    ok(json!({ "files": rows, "authority": *view.envelope.borrow() }))
                } else {
                    ok(json!(rows))
                }
            }
            Err(error) => error,
        };
    }
    match kind_rows(harness, id, query, crate::files::read, |touch| touch.msg_idx) {
        Ok(rows) => ok(json!(rows)),
        Err(error) => error,
    }
}

/// `GET /api/session/{harness}/{id}/compactions` — every compaction boundary in the session, in
/// transcript order, as `cv show --kind compaction_boundary --json` rows plus a `headline` for listings.
///
/// Built on [`crate::compaction::detect`], which streams the whole transcript (memory-safe on a
/// multi-GB session) and pairs each `compact_boundary` with the `isCompactSummary` message that
/// seeds the next window, keeping that **summary text** — how a continued agent recovers the
/// context it can no longer see. `pre_compaction_span` is the `[start, boundary)` range that was
/// compacted away, in the same numbering `/messages` uses.
fn compactions(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    match read::compactions(read::shared(), adapter.as_ref(), &r) {
        Ok(value) => ok(value),
        Err(e) => err(500, &format!("compaction scan failed: {e:#}")),
    }
}

/// Derive historical file activity from the same immutable session revisions as the inventory.
#[cfg(feature = "sqlite")]
pub fn touched_at_generation(
    gen: u64,
    path: &str,
    edits_only: bool,
    scope: &crate::state::hub::AuthorityEnvelope,
) -> anyhow::Result<Vec<crate::events::TouchedSession>> {
    use crate::Adapter as _;
    let hub = crate::state::hub::HubStore::default_hub()?;
    let abs = crate::events::absolutize(path, crate::env::current_dir().as_deref());
    let suffix = format!(
        "/{}",
        path.trim_start_matches("./")
            .trim_start_matches('/')
            .to_ascii_lowercase()
    );
    let mut rows = Vec::new();
    for entry in hub.heads_at_generation(Some(gen))? {
        if !scope.includes(&entry.producer_id) {
            continue;
        }
        let b = crate::custody::backing_from_hub_head(hub.packs_dir(), entry)?
            .ok_or_else(|| anyhow::anyhow!("historical event payload unavailable"))?;
        let r = b.reference.clone();
        let mut sink = crate::events::EventSink::new(r.cwd.clone());
        b.stream(&r, &crate::Demand::LEAN.with_spans(), &mut sink)?;
        let mut row = crate::events::TouchedSession {
            harness: r.harness.to_string(),
            session_id: r.id,
            title: r.title,
            reads: 0,
            edits: 0,
            last_ts: None,
            parent_id: None,
            agent_id: None,
            workflow: None,
        };
        for event in sink.into_events()? {
            if !event
                .target
                .as_deref()
                .is_some_and(|t| t == abs || t.to_ascii_lowercase().ends_with(&suffix))
            {
                continue;
            }
            match event.kind {
                "file_edit" => row.edits += 1,
                "file_read" => row.reads += 1,
                _ => continue,
            }
            row.last_ts = row.last_ts.max(event.ts);
        }
        if row.edits > 0 || (!edits_only && row.reads > 0) {
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| a.session_id.cmp(&b.session_id)));
    Ok(rows)
}

/// `GET /api/touched?path=&edits_only=&place=&as_of=&max_staleness=&authority=1` — sessions whose
/// tool events touched a file (exact or suffix path match), newest activity first, as `cv touched
/// --json` rows. Empty on a cold catalog; a catalog error is a 500, never an empty list.
fn touched(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(path) = params.get("path") else {
        return err(400, "missing path parameter");
    };
    let place = params.get("place").or_else(|| params.get("producer"));
    #[cfg(not(feature = "sqlite"))]
    let as_of = params.get("as_of").or_else(|| params.get("snapshot"));

    #[cfg(feature = "sqlite")]
    let (resolved_as_of, auth_opt) = if has_authority_params(&params) {
        match evaluate_api_query_authority(&params, false, false) {
            Ok((g, env)) => (g, Some(env)),
            Err(reply) => return reply,
        }
    } else {
        (None, None)
    };

    #[cfg(feature = "sqlite")]
    if let Some(gen) = resolved_as_of {
        let scope = auth_opt.as_ref().expect("evaluated for pinned generation");
        let rows = match touched_at_generation(gen, &path, params.flag("edits_only"), scope) {
            Ok(rows) => rows,
            Err(e) => return err(503, &e.to_string()),
        };
        let rows: Vec<_> = rows.iter().map(crate::rows::touched_row).collect();
        return if params.flag("authority") {
            ok(json!({ "touched": rows, "authority": scope }))
        } else {
            ok(json!(rows))
        };
    }
    #[cfg(not(feature = "sqlite"))]
    if as_of.is_some() {
        return err(400, "as_of requires sqlite support");
    }
    let mut rows = match crate::events::sessions_touching(&path, params.flag("edits_only")) {
        Ok(rows) => rows,
        Err(e) => return err(500, &format!("event catalog: {e:#}")),
    };
    if let Some(ref _pattern) = place {
        #[cfg(feature = "sqlite")]
        let hub_opt = crate::state::hub::HubStore::default_hub().ok();
        rows.retain(|t| {
            let p: String = if crate::SessionKey::split_selector(&t.session_id).is_none() {
                "local".into()
            } else {
                #[cfg(feature = "sqlite")]
                {
                    hub_opt
                        .as_ref()
                        .and_then(|h| h.session_head_for(&t.session_id, resolved_as_of).ok().flatten())
                        .map(|e| e.producer_id)
                        .unwrap_or_else(|| "local".into())
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    "local".into()
                }
            };
            #[cfg(feature = "sqlite")]
            {
                auth_opt.as_ref().is_some_and(|s| s.includes(&p))
            }
            #[cfg(not(feature = "sqlite"))]
            {
                glob_match(_pattern, &p)
            }
        });
    }
    let out: Vec<Value> = rows.iter().map(crate::rows::touched_row).collect();
    #[cfg(feature = "sqlite")]
    if let Some(env) = auth_opt {
        if params.flag("authority") {
            return ok(json!({ "touched": out, "authority": env }));
        }
    }
    ok(json!(out))
}

/// `GET /api/blame?file=<path>[&lines=a,b][&place=&as_of=&max_staleness=&authority=1]` —
/// git-to-session code provenance with content-hunk, commit-SHA, and time-window correlation,
/// matching `cv blame --json`.
fn blame(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(file) = params.get("file") else {
        return err(400, "missing file parameter");
    };
    let lines = params.get("lines");
    #[cfg(not(feature = "sqlite"))]
    if params.get("as_of").or_else(|| params.get("snapshot")).is_some() {
        return err(400, "as_of requires sqlite support");
    }
    #[cfg(feature = "sqlite")]
    if let Some(view) = match scoped_view_from_params(&params) {
        Ok(v) => v,
        Err(reply) => return reply,
    } {
        return match crate::blame::blame_scoped(&view, &file, lines.as_deref()) {
            Ok(report) => match serde_json::to_value(report) {
                Ok(v) => {
                    if params.flag("authority") {
                        ok(json!({ "blame": v, "authority": *view.envelope.borrow() }))
                    } else {
                        ok(v)
                    }
                }
                Err(e) => err(500, &format!("blame serialization failed: {e}")),
            },
            Err(e) if e.downcast_ref::<crate::blame::BlameInputError>().is_some() => err(400, &e.to_string()),
            Err(e) => err(500, &format!("blame failed: {e:#}")),
        };
    }
    match crate::blame::blame(&file, lines.as_deref()) {
        Ok(report) => match serde_json::to_value(report) {
            Ok(v) => ok(v),
            Err(e) => err(500, &format!("blame serialization failed: {e}")),
        },
        Err(e) if e.downcast_ref::<crate::blame::BlameInputError>().is_some() => err(400, &e.to_string()),
        Err(e) => err(500, &format!("blame failed: {e:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/subagents` — the sub-agent **forest** a session spawned
/// (directly-spawned `Agent`/`Task` agents and the workflow tier one level deeper), each a §3
/// session row plus the sidecar enrichment the forest view groups by. Empty array if none.
/// `?returns` adds `return`, the last text turn of each agent no workflow journal summarized.
fn subagents(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let forest = match crate::subagent_tree_of(&r) {
        Ok(forest) => forest,
        Err(error) => return err(500, &format!("forest read failed: {error:#}")),
    };
    let returns = Query::parse(query).flag("returns");
    const METADATA_BATCH_SIZE: usize = 256;
    let mut probe = crate::source::Probe::default();
    let out: anyhow::Result<Vec<Value>> = forest
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            if idx > 0 && idx % METADATA_BATCH_SIZE == 0 {
                probe = crate::source::Probe::default();
            }
            let mut row = crate::rows::subagent_row(s, probe.ref_size(&s.session));
            if let Some(ret) = returns
                .then(|| crate::forest::unjournaled_return(s))
                .transpose()?
                .flatten()
            {
                row["return"] = ret.into();
            }
            Ok(row)
        })
        .collect();
    match out {
        Ok(out) => ok(json!(out)),
        Err(error) => err(500, &format!("forest read failed: {error:#}")),
    }
}

/// `GET /api/session/{harness}/{parent}/subagent/{agent}` — the full parsed transcript of one
/// sub-agent (sub-agents aren't in the main pool, so they're loaded relative to their parent);
/// `agent` is any spelling [`crate::resolve::subagent`] takes: the transcript id, the bare
/// `agentId` the journal uses, or a unique prefix of either.
fn subagent(harness: &str, parent: &str, agent: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, parent) {
        Ok(x) => x,
        Err((404, _)) => return err(404, "parent session not found"),
        Err(e) => return e,
    };
    let sub = match crate::resolve::subagent(&r, agent) {
        Ok(sub) => sub,
        Err(e) => return resolve_reply(e),
    };
    match complete_session_json(adapter.as_ref(), &sub.session) {
        Ok(value) => ok(value),
        Err(error) => err(500, &format!("session read failed: {error:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/workflow/{wf}/script` — the driving script Claude Code recorded
/// for a workflow run, as `{ workflow, name, source }`. Scripts live in the session's sidecar tree
/// at `<session>/workflows/scripts/<slug>-<wf>.js`; the run id (`wf_…`) is matched as the filename
/// suffix (the slug prefix varies per workflow). 404 if there's no script for that run.
fn workflow_script(harness: &str, id: &str, wf: &str) -> (u16, Value) {
    // Reject a path-y run id up front — it's only ever a `wf_…` token, never a path component.
    if wf.contains('/') || wf.contains("..") || wf.is_empty() {
        return err(400, "invalid workflow id");
    }
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(scripts_dir) = crate::harness::claude::sidecar::workflows_dir(&r.path).map(|d| d.join("scripts")) else {
        return err(404, "session has no sidecar tree");
    };
    let found = std::fs::read_dir(&scripts_dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("js"))
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == wf || s.ends_with(&format!("-{wf}")))
            })
    });
    let Some(path) = found else {
        return err(404, "no script recorded for that workflow run");
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => ok(json!({
            "workflow": wf,
            "name": path.file_name().and_then(|n| n.to_str()),
            "source": source,
        })),
        Err(e) => err(500, &format!("could not read script: {e}")),
    }
}

// --- small helpers ------------------------------------------------------------------------------

/// A trivially-parsed query string: `k=v&k2=v2`, percent-decoded, `+` as space.
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn parse(q: &str) -> Self {
        Query(
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (decode(&k.replace('+', " ")), decode(&v.replace('+', " ")))
                })
                .collect(),
        )
    }

    /// First value for `key` (even if empty), if the key appears at all.
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// First non-empty value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }

    /// A present numeric selector must be nonempty and valid; malformed limits must never
    /// silently expand into an unbounded read. Other string/flag parameters retain their defaults.
    pub fn unsigned(&self, key: &str) -> Result<Option<usize>, String> {
        let Some((_, value)) = self.0.iter().find(|(name, _)| name == key) else {
            return Ok(None);
        };
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(value) = value.parse() {
                return Ok(Some(value));
            }
        }
        Err(format!("{key} must be a non-negative integer, got {value:?}"))
    }

    /// A boolean parameter: present with any value but `0`/`false` (so `?all=1`, `?all=true` and a
    /// bare `?all` all count).
    pub fn flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .any(|(k, v)| k == key && v != "0" && !v.eq_ignore_ascii_case("false"))
    }
}

/// Percent-decode one URL component (a path segment or a query value), lossily for bad UTF-8.
pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Flow, MessageSink};
    use crate::{Demand, Message};

    fn window(start: usize, end: Option<usize>) -> WindowSink {
        WindowSink::page(
            std::path::Path::new(""),
            start,
            end,
            false,
            crate::source::ReadSet::default(),
        )
    }

    #[test]
    fn complete_session_json_retains_full_owned_parity_and_rejects_source_changes() {
        let world = crate::test_world("owned-api-source-guard");
        let path = world.write_session(
            "full",
            &[json!({"type":"user", "uuid":"u", "message":{"role":"user", "content":"owned text"}})],
        );
        let reference = SessionRef::new("full", Harness::Claude, &path);
        let adapter = crate::harness::for_harness(Harness::Claude).unwrap();
        assert_eq!(
            complete_session_json(adapter.as_ref(), &reference).unwrap(),
            serde_json::to_value(adapter.parse(&reference).unwrap()).unwrap()
        );
        struct Changes(std::path::PathBuf);
        impl crate::Adapter for Changes {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<crate::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                reference: &SessionRef,
                _: &crate::Demand,
                sink: &mut dyn crate::MessageSink,
            ) -> anyhow::Result<Session> {
                sink.dependency(&self.0);
                let mut message = Message::new(crate::Role::User);
                message.content.push(crate::Block::from_text("unpublished".into()));
                sink.try_message(message)?;
                std::fs::write(&self.0, "changed while reading")?;
                Ok(Session::new(&reference.id, reference.harness))
            }
        }
        let auxiliary = world.base.join("metadata");
        for dependency in [&path, &auxiliary] {
            std::fs::write(dependency, "before").unwrap();
            let error = complete_session_json(&Changes(dependency.clone()), &reference).unwrap_err();
            assert!(format!("{error:#}").contains("source changed"), "{error:#}");
        }
    }

    #[test]
    fn staged_message_windows_match_value_response_bytes() {
        let world = crate::test_world("api-staged-windows");
        let body = "月🌙é\n\"\\".repeat(4000);
        world.write_session("staged",&[
            json!({"type":"assistant","sessionId":"staged","uuid":"a","message":{"role":"assistant","content":[{"type":"text","text":body},{"type":"tool_use","id":"c","name":"tool","input":{"z":body,"a":2}}]}}),
            json!({"type":"user","sessionId":"staged","uuid":"b","message":{"role":"user","content":"second"}}),
            json!({"type":"assistant","sessionId":"staged","uuid":"1","message":{"role":"assistant","content":"third"}}),
        ]);
        let parts = ["api", "session", "claude", "staged", "messages"];
        for query in [
            "start=0&end=1",
            "start=1&end=2",
            "start=2&end=3",
            "start=1",
            "start=0&end=0",
            "start=9&end=10",
            "extra=1&start=0&end=1",
            "around=1&context=0",
            "around=b&context=0",
            "around=id%3A1&context=0",
            "around=missing&context=0",
            "around=&context=0",
            "context=1",
            "around=1&start=0",
            "start=oops",
            "start=3&end=1",
        ] {
            let (status, expected) = route_json(&parts, query);
            let actual = route_staged(&parts, query).unwrap().unwrap();
            let mut bytes = Vec::new();
            actual.body.copy_to(&mut bytes).unwrap();
            assert_eq!(actual.status, status, "{query}");
            assert_eq!(actual.bytes, bytes.len() as u64);
            assert_eq!(bytes, serde_json::to_vec(&expected).unwrap(), "{query}");
        }
        // Bare `1` is index 1 (message "b"); `id:1` disambiguates message ID "1" (at index 2).
        let (_, by_idx) = route_json(&parts, "around=1&context=0");
        assert_eq!(by_idx["start"], 1);
        assert_eq!(by_idx["messages"][0]["id"], "b");
        let (_, by_id) = route_json(&parts, "around=id%3A1&context=0");
        assert_eq!(by_id["start"], 2);
        assert_eq!(by_id["messages"][0]["id"], "1");
        let (status, missing) = route_json(&parts, "around=missing&context=0");
        assert_eq!(status, 400);
        assert!(missing["error"].as_str().unwrap().contains("missing"), "{missing}");
    }

    #[test]
    fn selected_json_window_reports_read_errors_and_skipped_values_stay_lazy() {
        let world = crate::test_world("api_json_failure");
        let path = world.home.join("input.json");
        let body = r#"{"command":"git status"}"#;
        std::fs::write(&path, body).unwrap();
        let input = crate::json::Json::source(&path, 0, body.len() as u64).unwrap();
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(crate::Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            namespace: None,
            input,
            extensions: Default::default(),
        });
        std::fs::remove_file(path).unwrap();
        let mut selected = window(0, Some(1));
        assert!(crate::stream::checked(&mut selected, |sink| {
            sink.message(message.clone());
            Ok(())
        })
        .is_err());
        assert_eq!(selected.selected, 0);
        let mut before = window(1, Some(2));
        assert_eq!(before.message(message.clone()), Flow::Continue);
        assert!(before.take_error().is_none());
        let mut after = window(0, Some(0));
        assert_eq!(after.message(message), Flow::Stop);
        assert!(after.more);
        assert!(after.take_error().is_none());
    }

    #[test]
    #[ignore = "read-work baseline for pagination design; writes only an isolated scratch fixture"]
    fn pagination_prefix_replay_read_work() {
        use crate::Adapter as _;
        use std::io::Write as _;
        let world = crate::test_world("api_page_read_work");
        let path = world.transcript("pages");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let records = 20_000usize;
        let page = 200usize;
        for i in 0..records {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"m{i}","message":{{"role":"user","content":"hello"}}}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let r = SessionRef::new("pages", crate::Harness::Claude, path);
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let mut callbacks = 0usize;
        for start in (0..records).step_by(page) {
            let mut sink = window(start, Some(start + page));
            adapter.try_stream(&r, &Demand::LEAN.with_spans(), &mut sink).unwrap();
            assert_eq!(sink.selected, page);
            assert_eq!(sink.more, start + page < records);
            callbacks += sink.idx;
        }
        let expected = page * (records / page) * (records / page + 1) / 2 + records / page - 1;
        assert_eq!(callbacks, expected);
        let mut head_callbacks = 0usize;
        adapter
            .try_stream(&r, &Demand::LEAN.with_spans(), &mut |_m| {
                head_callbacks += 1;
                Flow::Continue
            })
            .unwrap();
        assert_eq!(head_callbacks, records);
        eprintln!(
            "pages={} selected={} page_callbacks={} additional_head_callbacks={}",
            records / page,
            records,
            callbacks,
            head_callbacks
        );
    }

    #[test]
    fn query_decodes_plus_percent_and_flags() {
        let q = Query::parse("who=ember&since=2h&x=a+b%21&all=1&off=0&bare");
        assert_eq!(q.get("who").as_deref(), Some("ember"));
        assert_eq!(q.get("x").as_deref(), Some("a b!"));
        assert_eq!(q.get("missing"), None);
        assert!(q.flag("all"));
        assert!(!q.flag("off"));
        assert!(!q.flag("missing"));
        // A bare key is an empty value: not a `get` hit, but a flag.
        assert_eq!(q.get("bare"), None);
        assert!(q.flag("bare"));
    }

    #[test]
    fn decode_handles_segments() {
        assert_eq!(decode("%2Fwork%2Fproj"), "/work/proj");
        assert_eq!(decode("plain"), "plain");
    }

    #[test]
    fn unknown_and_removed_routes_404() {
        for parts in [
            &["api", "board", "fleet"][..],
            &["api", "channels"],
            &["api", "tasks", "debt"],
            &["api", "backlog"],
            &["api", "nope"],
        ] {
            assert_eq!(route_json(parts, "").0, 404, "{parts:?}");
        }
        assert_eq!(route_json(&["api", "sessions"], "harness=warpdrive").0, 400);
    }

    #[test]
    fn ambiguous_session_prefix_returns_400_bad_request() {
        let world = crate::test_world("api-ambiguous-400");
        let proj = world.home.join(".claude/projects/-tmp");
        std::fs::create_dir_all(&proj).unwrap();
        for id in ["ambig-one", "ambig-two"] {
            let line = serde_json::json!({
                "type": "user",
                "sessionId": id,
                "uuid": "u1",
                "message": {"role": "user", "content": "hello"}
            });
            std::fs::write(proj.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
        }
        let (status, body) = route_json(&["api", "session", "claude", "ambig"], "");
        assert_eq!(status, 400, "ambiguous session prefix must return 400: {body}");
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ambiguous session id"));
    }

    #[test]
    fn prompts_route_projects_human_prompts_and_question_answers_and_windows_rows() {
        let world = crate::test_world("api-prompts-route");
        world.write_session(
            "p1",
            &[
                json!({
                    "type": "user",
                    "sessionId": "p1",
                    "uuid": "u0",
                    "timestamp": "2026-10-01T12:00:00Z",
                    "message": {"role": "user", "content": "first human prompt"}
                }),
                json!({
                    "type": "assistant",
                    "sessionId": "p1",
                    "uuid": "a1",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {"questions": []}},
                            {"type": "tool_use", "id": "b1", "name": "Bash", "input": {"command": "pwd"}}
                        ]
                    }
                }),
                json!({
                    "type": "user",
                    "sessionId": "p1",
                    "uuid": "u2",
                    "timestamp": "2026-10-01T12:01:00Z",
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": "q1", "content": "Selected option B"},
                            {"type": "tool_result", "tool_use_id": "b1", "content": "/tmp"}
                        ]
                    }
                }),
                json!({
                    "type": "user",
                    "sessionId": "p1",
                    "uuid": "u3",
                    "timestamp": "2026-10-01T12:02:00Z",
                    "message": {"role": "user", "content": "second human prompt"}
                }),
            ],
        );
        let parts = ["api", "session", "claude", "p1", "prompts"];
        let (st, body) = route_json(&parts, "");
        assert_eq!(st, 200, "{body}");
        let rows = body.as_array().unwrap();
        assert_eq!(rows.len(), 3, "{body}");
        assert_eq!(
            (
                rows[0]["index"].as_u64(),
                rows[0]["kind"].as_str(),
                rows[0]["text"].as_str()
            ),
            (Some(0), Some("prompt"), Some("first human prompt"))
        );
        assert_eq!(
            (
                rows[1]["index"].as_u64(),
                rows[1]["kind"].as_str(),
                rows[1]["text"].as_str()
            ),
            (Some(2), Some("answer"), Some("Selected option B"))
        );
        assert_eq!(
            (
                rows[2]["index"].as_u64(),
                rows[2]["kind"].as_str(),
                rows[2]["text"].as_str()
            ),
            (Some(3), Some("prompt"), Some("second human prompt"))
        );

        let (st_ans, ans) = route_json(&parts, "kind=answer");
        assert_eq!(st_ans, 200);
        assert_eq!(ans.as_array().unwrap().len(), 1);
        assert_eq!(ans[0]["kind"], "answer");

        let (st_win, win) = route_json(&parts, "kind=prompt&start=1");
        assert_eq!(st_win, 200);
        assert_eq!(win.as_array().unwrap().len(), 1);
        assert_eq!(win[0]["index"], 3);

        let (st_around, around) = route_json(&parts, "around=2&context=0");
        assert_eq!(st_around, 200);
        assert_eq!(around.as_array().unwrap().len(), 1);
        assert_eq!(around[0]["kind"], "answer");

        assert_eq!(route_json(&parts, "kind=bogus").0, 400);
        assert_eq!(route_json(&parts, "kind=").0, 400);
    }

    #[test]
    fn subagent_route_takes_a_unique_stem_prefix_and_types_its_refusals() {
        let world = crate::test_world("api-subagent-stem");
        let proj = world.home.join(".claude/projects/-tmp");
        let line = |session: &str, text: &str| {
            let row = serde_json::json!({
                "type": "user", "sessionId": session, "uuid": "u1",
                "message": {"role": "user", "content": text}
            });
            format!("{row}\n")
        };
        let subs = proj.join("parent-1/subagents");
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::write(proj.join("parent-1.jsonl"), line("parent-1", "root")).unwrap();
        for stem in ["ab12cd", "ab99ef", "ff00"] {
            std::fs::write(subs.join(format!("agent-{stem}.jsonl")), line("parent-1", stem)).unwrap();
        }
        let get = |agent: &str| route_json(&["api", "session", "claude", "parent-1", "subagent", agent], "");
        for unique in ["agent-ab12cd", "ab12cd", "ab12", "agent-ff", "f"] {
            let (status, body) = get(unique);
            assert_eq!(status, 200, "{unique}: {body}");
            let want = if unique.contains('f') && !unique.contains("ab") {
                "agent-ff00"
            } else {
                "agent-ab12cd"
            };
            assert_eq!(body["id"], want, "{unique}");
        }
        let (status, body) = get("ab");
        assert_eq!(status, 400, "{body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("claude:agent-ab12cd") && error.contains("claude:agent-ab99ef"),
            "{error}"
        );
        assert_eq!(get("zz").0, 404);
        let forest = |query| route_json(&["api", "session", "claude", "parent-1", "subagents"], query).1;
        assert!(forest("")
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("return").is_none()));
        assert_eq!(forest("returns").as_array().unwrap().len(), 3);
    }
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    #[test]
    fn window_metadata_preserves_read_coverage_without_message_extras() {
        let mut session = Session::new("member", crate::Harness::Antigravity);
        assert!(session_meta_json(&session).get("extra").is_none());
        let evidence =
            json!({"sources":["member_registry"], "limitations":["metadata_only","future_gap"], "future_field":42});
        session.cv_extra_mut().insert("read_coverage".into(), evidence.clone());
        session
            .harness_extra_mut(crate::Harness::Antigravity)
            .insert("large_raw_data".into(), json!({"unrelated":true}));
        assert_eq!(
            session_meta_json(&session)["extra"],
            json!({"cv":{"read_coverage":evidence}})
        );
        let mut initial = session_meta_json(&Session::new("member", crate::Harness::Antigravity));
        update_read_coverage(&mut initial, &session);
        assert_eq!(initial["extra"]["cv"]["read_coverage"], evidence);
    }

    #[test]
    fn window_metadata_preserves_principal_only_when_present() {
        let mut session = Session::new("opaque", crate::Harness::OpenSession);
        assert!(session_meta_json(&session).get("principal").is_none());
        session.principal = Some(crate::Principal::String("opaque-owner".into()));
        assert_eq!(session_meta_json(&session)["principal"], json!("opaque-owner"));
    }
}
