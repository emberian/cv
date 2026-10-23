//! `cv serve` — one local HTTP server: the read API at `/api/*` (`cv_core::api::route_json`, plus
//! the two tantivy routes `/api/search` and `/api/stats`), the task inbox page at `/inbox` with
//! the routes its buttons use (`serve_tasks`), and, with `--web <dir>`, the browser dashboard from
//! `/`. Without `--web`, `/` is the inbox. A small *synchronous* server ([`crate::http`]), one
//! thread per connection: the workload is a handful of pollers, and simplicity wins.
//!
//! The transcript corpus is sensitive (it can contain secrets) and the inbox writes identity-bearing
//! events, so both are locked to local callers: the `Host` header must name this machine (defeats
//! DNS rebinding), and CORS is echoed only for an allow-list of local origins (the served pages,
//! `localhost`/`127.0.0.1` dev servers, and the Tauri app) — never `*`. A write (`POST`) must be
//! `application/json` (so a cross-site form cannot send one without a preflight this server refuses)
//! and, when it carries an `Origin`, an allowed one. An optional bearer token (`--token` /
//! `$CV_SERVE_TOKEN`) gates every `/api/*` request; a non-loopback bind demands one (or an explicit
//! `--insecure-expose`). `OPTIONS` preflight is answered with a 204.

use crate::http::{Request, Response};
use anyhow::Result;
use cv_core::Harness;
use cv_search::{Note, Source};
use serde_json::{json, Value};
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use super::serve_tasks;

/// Everything `cv serve` was asked for.
pub(crate) struct ServeOpts {
    pub host: String,
    pub port: u16,
    pub web: Option<PathBuf>,
    pub token: Option<String>,
    pub insecure_expose: bool,
    /// Whose inbox `/inbox` opens on, and who a write is by when the request names nobody.
    pub who: Option<String>,
    /// Open the page in the default browser once the server is up.
    pub open: bool,
}

/// What one request handler needs to know beyond the request itself.
struct Ctx {
    /// The configured bind host, accepted in `Host`/`Origin` checks alongside the loopback names.
    bind_host: String,
    web_root: Option<PathBuf>,
    /// When set, every `/api/*` request must carry `Authorization: Bearer <token>`.
    token: Option<String>,
    default_who: Option<String>,
    searcher: cv_search::Searcher,
}

/// Gatekeep the exposure, then serve until the process is killed: a non-loopback bind serves the
/// whole transcript corpus (secrets included) and the inbox's writes to anyone who can reach the
/// port, so it demands a bearer token or an explicit `--insecure-expose`, and warns either way.
pub(crate) fn cmd_serve(opts: ServeOpts) -> Result<()> {
    if cv_core::env::var_os("CVD_TOKEN").is_some() && cv_core::env::var_os("CV_SERVE_TOKEN").is_none() {
        return crate::util::usage(
            "$CVD_TOKEN was renamed $CV_SERVE_TOKEN in 0.15.0 with `cvd serve` → `cv serve` \
             (or pass --token)",
        );
    }
    let token = opts
        .token
        .or_else(|| cv_core::env::var("CV_SERVE_TOKEN").ok())
        .filter(|t| !t.is_empty());
    let host = opts.host.as_str();
    if !is_loopback_host(host) {
        if token.is_none() && !opts.insecure_expose {
            anyhow::bail!(
                "refusing to bind non-loopback host {host:?} without auth: the API serves your full \
                 transcript corpus (which can contain secrets) to anyone who can reach the port.\n\
                 Set a token (--token or $CV_SERVE_TOKEN), or pass --insecure-expose to serve it open."
            );
        }
        eprintln!("cv serve: WARNING: binding non-loopback host {host:?} — the API is reachable from the network.");
        if token.is_none() {
            eprintln!(
                "cv serve: WARNING: --insecure-expose without a token: anyone who can reach {host}:{} can read \
                 every transcript and act in the inbox as web:<who>.",
                opts.port
            );
        }
    }
    run(&opts.host, opts.port, opts.web, token, opts.who, opts.open)
}

/// Run the HTTP server until the process is killed. Never returns under normal operation.
fn run(
    host: &str,
    port: u16,
    web_root: Option<PathBuf>,
    token: Option<String>,
    who: Option<String>,
    open: bool,
) -> Result<()> {
    let listener = TcpListener::bind((host, port)).map_err(|e| anyhow::anyhow!("failed to bind {host}:{port}: {e}"))?;
    // `--port 0` binds an ephemeral port; the banner below must carry the REAL bound port so
    // callers (test harnesses, scripts) can parse the address instead of racing to guess a free
    // one. For a fixed port this is the same number.
    let port = listener.local_addr().map_or(port, |a| a.port());
    // Canonicalize the web root once so per-request path-traversal checks are cheap and robust.
    let web_root = web_root.and_then(|d| match d.canonicalize() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("cv serve: --web {} is unreadable ({e}); serving API only", d.display());
            None
        }
    });
    match &web_root {
        Some(dir) => eprintln!(
            "cv serve on http://{host}:{port} — dashboard at /, inbox at /inbox, API at /api/* (web root {})",
            dir.display()
        ),
        None => eprintln!("cv serve on http://{host}:{port} — inbox at /, API at /api/* (no --web)"),
    }
    let display_host = if is_loopback_host(host) || host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    let inbox = match &who {
        Some(w) => format!("http://{display_host}:{port}/inbox?who={}", urlencoding::encode(w)),
        None => format!("http://{display_host}:{port}/inbox"),
    };
    eprintln!("inbox: {inbox}");
    if open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        if let Err(e) = std::process::Command::new(opener).arg(&inbox).spawn() {
            eprintln!("could not open a browser ({e}); open {inbox} yourself");
        }
    }

    let ctx = Ctx {
        bind_host: host.to_string(),
        web_root,
        token,
        default_who: who,
        searcher: cv_search::Searcher::default(),
    };
    crate::http::serve(listener, crate::http::Limits::default(), move |request| {
        handle(request, &ctx)
    })
}

/// Handle one request: any error becomes a JSON error response.
fn handle(request: &mut Request<'_>, ctx: &Ctx) {
    // DNS-rebinding guard: a hostile page can point its own domain at 127.0.0.1 and fetch us
    // same-origin, but it can't forge the Host header — reject anything that isn't our own name.
    if !host_allowed(header_value(request, "Host").as_deref(), &ctx.bind_host) {
        request.respond(json_response(403, &json!({"error": "forbidden host"}), None, &[]));
        return;
    }
    // Echoed on API responses only when the Origin is on the local allow-list; never `*`.
    let raw_origin = header_value(request, "Origin");
    let origin = allowed_origin(raw_origin.as_deref(), &ctx.bind_host);

    let method = request.method().to_string();
    let raw_url = request.url().to_string();

    // CORS preflight: a 204, CORS-tagged only for allowed origins. Unauthenticated by design —
    // browsers strip credentials from preflights, and it discloses nothing.
    if method == "OPTIONS" {
        request.respond(no_content(origin.as_deref()));
        return;
    }

    // Split off the query string, then normalise the path (strip trailing slash, decode segments).
    let (path, query) = match raw_url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_url.as_str(), ""),
    };
    let segments: Vec<String> = path
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(cv_core::api::decode)
        .collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    let is_api = parts.first() == Some(&"api");
    let post = method == "POST";

    // GET reads anything; POST is only ever an inbox write (`/api/task/<id>/<verb>`).
    if !(method == "GET" || (post && matches!(parts.as_slice(), ["api", "task", _, _]))) {
        request.respond(json_response(
            405,
            &json!({"error": "method not allowed"}),
            origin.as_deref(),
            &[],
        ));
        return;
    }

    if !is_api {
        // The inbox page: at `/inbox` always, and at `/` when no dashboard is being hosted.
        let page = matches!(parts.as_slice(), ["inbox"] | ["inbox.html"])
            || (ctx.web_root.is_none() && matches!(parts.as_slice(), [] | ["index.html"]));
        if page {
            request.respond(text_response(
                200,
                "text/html; charset=utf-8",
                serve_tasks::PAGE.as_bytes().to_vec(),
            ));
            return;
        }
        if let Some(root) = &ctx.web_root {
            request.respond(static_file(root, &segments));
            return;
        }
    }

    if let Some(token) = &ctx.token {
        let authed = header_value(request, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .is_some_and(|t| token_matches(&t, token));
        if !authed {
            request.respond(json_response(
                401,
                &json!({"error": "missing or invalid bearer token"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
    }

    // A write: JSON only (a cross-site form or `text/plain` fetch is a "simple" request that needs
    // no preflight; requiring JSON forces one, which a foreign origin fails), and a foreign Origin
    // is refused outright rather than merely denied the response.
    let mut body = Value::Null;
    if post {
        let json_ct = header_value(request, "Content-Type").is_some_and(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
        if !json_ct || (raw_origin.is_some() && origin.is_none()) {
            request.respond(json_response(
                403,
                &json!({"error": "a write must be application/json from a local origin"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
        // Capped: a note is a paragraph, not an upload.
        let mut buf = String::new();
        const BODY_LIMIT: u64 = 1 << 20;
        if request.by_ref().take(BODY_LIMIT + 1).read_to_string(&mut buf).is_err() {
            request.respond(json_response(
                400,
                &json!({"error": "unreadable body"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
        if buf.len() as u64 > BODY_LIMIT {
            request.respond(json_response(
                413,
                &json!({"error": "body exceeds 1 MiB limit"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
        if !buf.trim().is_empty() {
            match serde_json::from_str::<Value>(&buf) {
                Ok(v) => body = v,
                Err(e) => {
                    request.respond(json_response(
                        400,
                        &json!({"error": format!("body is not JSON: {e}")}),
                        origin.as_deref(),
                        &[],
                    ));
                    return;
                }
            }
        }
    }

    let q = cv_core::api::Query::parse(query);
    match serve_tasks::route(post, &parts, &q, &body, ctx.default_who.as_deref()) {
        Some(serve_tasks::Answer::Json(status, v)) => {
            request.respond(json_response(
                status,
                &v,
                origin.as_deref(),
                &[("Cache-Control", "no-store".into())],
            ));
        }
        Some(serve_tasks::Answer::Ndjson(lines)) => {
            let mut resp = text_response(200, "application/x-ndjson; charset=utf-8", lines.into_bytes());
            cors_headers(&mut resp, origin.as_deref());
            resp.add_header("Cache-Control", "no-store");
            request.respond(resp);
        }
        None if post => {
            request.respond(json_response(
                404,
                &json!({"error": "no such route"}),
                origin.as_deref(),
                &[],
            ));
        }
        None => {
            if let Some(result) = download_export(false, &parts, &q) {
                match result {
                    Ok(download) => match download.body.reader() {
                        Ok(reader) => {
                            let mut response = Response::stream(200, reader, download.bytes);
                            response.add_header("Content-Type", download.content_type);
                            response.add_header(
                                "Content-Disposition",
                                &format!("attachment; filename=\"{}\"", download.filename),
                            );
                            response.add_header("Cache-Control", "no-store");
                            cors_headers(&mut response, origin.as_deref());
                            request.respond(response);
                        }
                        Err(error) => {
                            request.respond(json_response(
                                500,
                                &json!({"error":error.to_string()}),
                                origin.as_deref(),
                                &[],
                            ));
                        }
                    },
                    Err((status, error)) => {
                        request.respond(json_response(status, &error, origin.as_deref(), &[]));
                    }
                }
                return;
            }
            match cv_core::api::route_staged(&parts, query) {
                Ok(Some(stage)) => {
                    match stage.body.reader() {
                        Ok(reader) => {
                            let response = staged_response(&stage, reader, origin.as_deref());
                            request.respond(response);
                        }
                        Err(error) => {
                            request.respond(json_response(
                                500,
                                &json!({"error":format!("staged response failed: {error}")}),
                                origin.as_deref(),
                                &[],
                            ));
                        }
                    }
                    return;
                }
                Err(error) => {
                    request.respond(json_response(
                        500,
                        &json!({"error":format!("staged response failed: {error:#}")}),
                        origin.as_deref(),
                        &[],
                    ));
                    return;
                }
                Ok(None) => {}
            }
            let (status, body, extra) = route(false, &parts, query, &q, &ctx.searcher);
            request.respond(json_response(status, &body, origin.as_deref(), &extra));
        }
    }
}

/// First value of a (case-insensitively named) request header, if present.
fn header_value(request: &Request<'_>, name: &'static str) -> Option<String> {
    request.header(name).map(str::to_string)
}

/// Whether a presented bearer token equals the configured one, in time that depends only on the
/// presented token's length, so response timing cannot be used to guess the secret byte by byte.
fn token_matches(presented: &str, token: &str) -> bool {
    let (presented, token) = (presented.as_bytes(), token.as_bytes());
    let mut diff = u8::from(presented.len() != token.len());
    for (i, byte) in presented.iter().enumerate() {
        diff |= byte ^ token.get(i % token.len().max(1)).copied().unwrap_or(0);
    }
    std::hint::black_box(diff) == 0
}

/// The hostname part of a `Host` header or URL authority: strips a `:port`, unbrackets IPv6.
fn host_name(authority: &str) -> Option<&str> {
    fn valid_port(suffix: &str) -> bool {
        suffix.is_empty()
            || suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok()
            })
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (name, suffix) = rest.split_once(']')?;
        name.parse::<std::net::Ipv6Addr>().ok()?;
        return valid_port(suffix).then_some(name);
    }
    let (name, suffix) = authority
        .find(':')
        .map(|at| authority.split_at(at))
        .unwrap_or((authority, ""));
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        && valid_port(suffix))
    .then_some(name)
}

/// A name this server answers to: the loopback names, or the configured bind host.
fn is_local_name(name: &str, bind_host: &str) -> bool {
    name == "127.0.0.1" || name == "::1" || name.eq_ignore_ascii_case("localhost") || name == bind_host
}

/// Whether a configured bind host is a loopback name/address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Whether the `Host` header names this machine. An explicitly non-loopback bind is an opt-in to
/// remote clients whose Host we can't predict, so the check only bites on loopback binds.
fn host_allowed(host_header: Option<&str>, bind_host: &str) -> bool {
    if !is_loopback_host(bind_host) {
        return true;
    }
    host_header
        .and_then(host_name)
        .is_some_and(|name| is_local_name(name, bind_host))
}

/// The specific `Origin` to echo in `Access-Control-Allow-Origin`, if it's on the allow-list:
/// the Tauri app's origins, or an http(s) origin on a local name (any port — a dev dashboard on
/// e.g. `http://localhost:5173` is same-machine).
fn allowed_origin(origin: Option<&str>, bind_host: &str) -> Option<String> {
    let o = origin?;
    if o == "tauri://localhost" || o == "http://tauri.localhost" {
        return Some(o.to_string());
    }
    let authority = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"))?;
    is_local_name(host_name(authority)?, bind_host).then(|| o.to_string())
}

/// A reply: status, JSON body, and any response headers beyond the standard set. Only `/api/search`
/// uses the third element today (to say which door answered — see [`search`]).
type Reply = (u16, Value, Vec<(&'static str, String)>);

cv_core::routes! {
    pub(crate) const ROUTES;
    /// Route a decoded read path + raw query string. Never panics.
    fn route(_post, parts, query: &str, params: &Query, searcher: &cv_search::Searcher) -> Reply {
        // Routes that carry extra response headers answer here; everything else is header-free.
        GET ["api", "search"] => search(params, searcher),
        GET ["api", "stats"] => {
            let (status, body) = stats(params);
            (status, body, Vec::new())
        },
        // Every other noun is the one read API every door maps onto.
        _ => {
            let (status, body) = cv_core::api::route_json(parts, query);
            (status, body, Vec::new())
        },
    }
}

// --- search & stats: the two things the CLI could do and the dashboard could not ------------

/// `cv search`'s default `--limit`. The doors agree on defaults too, or a caller gets a different
/// number of rows depending on which one it asked.
const SEARCH_LIMIT: usize = 20;

/// `GET /api/stats?q=<query-calculus>` — corpus-wide fleet statistics: exactly what `cv stats
/// --json` prints (`sessions, messages, by_harness, top_cwds, earliest_created, latest_updated`),
/// computed by [`cv_core::stats::CorpusStats`] so the two doors cannot drift.
///
/// Why this exists: the dashboard's Stats view used to accumulate its totals from whichever
/// sessions the user had happened to click, and had to confess it ("…reflect the 1 opened session
/// — 6,872 not yet loaded"). The numbers are catalog-cheap for the whole fleet; there was never a
/// reason to guess at them.
///
/// The optional `q` is cv's query calculus (`cv schema`), the same string `cv stats -q` takes; a
/// parse error is a 400 quoting the core's own message; a `text:` term with no full-text index is
/// a 503 naming `cv index`.
fn stats(params: &Query) -> (u16, Value) {
    let q = match params.get("q") {
        Some(s) => match cv_core::SessionQuery::parse(&s) {
            Ok(q) => Some(q),
            // The core points at the reference by its 0.10 name; re-point it like the CLI does.
            Err(e) => return err(400, &e.replace("`cv query`", "`cv schema`")),
        },
        None => None,
    };
    let mut refs = cv_core::sessions();
    if let Some(q) = &q {
        if let Err(error) =
            crate::cmd::query::text_sets(q).and_then(|texts| cv_core::query::filter_refs(&mut refs, q, &texts))
        {
            // No index for `text:` is the service lacking it (as `/api/search` says), not a fault.
            let status = if error.downcast_ref::<crate::util::UsageError>().is_some() {
                503
            } else {
                500
            };
            return err(status, &format!("{error:#}"));
        }
    }
    ok(cv_core::stats::CorpusStats::compute(&refs).to_json())
}

/// `GET /api/search?q=&limit=&harness=&cwd=&kind=&semantic=1&scan=1` — full-text (or `semantic=1`) search over
/// the whole corpus, as the JSON array of rows `cv search --json` emits: the §3 session row plus
/// `score`, `snippet` and the sub-agent provenance trio, shaped by [`cv_core::rows::SearchRow`].
///
/// Why this exists: the dashboard's search box could only filter the session stubs it had already
/// downloaded, so it could not find a message it had not fetched — while `cv search` answers over
/// the whole archive instantly off the tantivy index `cv index` maintains. Same index, same rows,
/// through the other door.
///
/// **The index answers, or says why not.** A missing or unusable index is a 503 naming the cause
/// and the fix, as `cv search` errors; `scan=1` asks for the live scan instead
/// ([`cv_core::scan::live_search_filtered`]). Which path answered rides on `X-Cv-Search-Source` (`index` |
/// `semantic` | `live`), with a human sentence in `X-Cv-Search-Note` whenever the answer may be
/// partial (a stale index, a live scan, a truncation). Both headers are CORS-exposed. The body
/// stays exactly the CLI's array.
fn search(params: &Query, searcher: &cv_search::Searcher) -> Reply {
    let bad = |status, message: &str| {
        let (status, body) = err(status, message);
        (status, body, Vec::new())
    };
    // An empty query would mean "every session", which is `/api/sessions`' job; searching for
    // nothing is a caller mistake, not an empty result set.
    let Some(q) = params.get("q") else {
        return bad(400, "missing q parameter");
    };
    // Validate harness up front so a typo is a clear 400 rather than silently empty.
    let want = match params.get("harness") {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            None => return bad(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };
    let limit = match params.unsigned("limit") {
        Ok(limit) => limit.unwrap_or(SEARCH_LIMIT),
        Err(error) => return bad(400, &error),
    };
    let cwd = params.get("cwd");
    let flag = |name| {
        params
            .get(name)
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    };
    let (semantic, scan) = (flag("semantic"), flag("scan"));
    let scope = match params.get("kind") {
        None => cv_search::Scope::Exchange,
        Some(kind) => match cv_search::Scope::parse(&kind) {
            Some(scope) => scope,
            None => return bad(400, &format!("unknown kind {kind:?}: exchange, context or all")),
        },
    };
    if semantic && scan {
        return bad(400, "semantic=1 conflicts with scan=1");
    }
    // The CLI declares --kind as conflicting with --scan and --semantic; the HTTP door keeps the
    // same rule, so one request shape means one thing on both.
    if (semantic || scan) && params.get("kind").is_some() {
        return bad(400, "this mode searches the exchange only; kind= needs the index");
    }
    let source = if semantic {
        Source::Semantic
    } else if scan {
        Source::Live
    } else {
        Source::Index
    };
    let filter = cv_search::SearchFilter {
        harness: want.map(|h| h.as_str()),
        cwd: cwd.as_deref(),
        scope,
    };
    let live_bytes = match params.unsigned("live_bytes") {
        Ok(bytes) => bytes.map(|b| b as u64),
        Err(error) => return bad(400, &error),
    };
    let found = match searcher.search(&q, filter, limit, source, live_bytes) {
        Ok(found) => found,
        // A live scan cannot stand in for semantic ranking, so this is an error, exactly as
        // `cv search --semantic` exits non-zero rather than quietly changing mode.
        Err(e) if semantic => {
            return bad(
                500,
                &format!("semantic search failed (run `cv index --semantic` first?): {e:#}"),
            )
        }
        Err(e) if scan => return bad(500, &format!("live scan failed: {e:#}")),
        Err(e) if cv_search::is_invalid_text_query(&e) => return bad(400, &format!("invalid search query: {e:#}")),
        Err(e) if e.is::<cv_search::BadLiveBytes>() => return bad(500, &format!("{e:#}")),
        // The index is authoritative: a missing or unusable index is an error naming the cause and
        // the fix, never a silent scan of every transcript.
        Err(e) => {
            return bad(
                503,
                &format!("{e:#} (or add scan=1 for a slow live scan of every session)"),
            )
        }
    };
    let context = cv_core::rows::SearchRowContext::new();
    let rows: Vec<Value> = found.hits.iter().map(|h| cv_search::row(h, &context)).collect();

    // The note, in this door's words: the scan, its truncation, then whatever the search itself
    // could not promise, joined with `; `.
    let mut note = scan.then(|| "scan=1: scanned every transcript live".to_string());
    if found.hits.len() < found.total {
        let cut = format!("{} of {} matches; raise limit for more", found.hits.len(), found.total);
        note = Some(match note {
            Some(n) => format!("{n}; {cut}"),
            None => cut,
        });
    }
    for n in &found.notes {
        let s = match n {
            Note::Stale(s) => s.clone(),
            Note::Live(s) => s.clone(),
            Note::SnippetsUnavailable(e) => format!("live snippets unavailable ({e}); showing indexed previews"),
            Note::Unscanned(s) => s.clone(),
            Note::NoSubagents => "sub-agent transcripts aren't searched yet — rebuild with `cv index --subagents` to \
                                  include the Agent/Workflow lanes"
                .to_string(),
        };
        note = Some(match note {
            Some(n) => format!("{n}; {s}"),
            None => s,
        });
    }

    let mut headers = vec![
        ("X-Cv-Search-Source", found.source.as_str().to_string()),
        ("X-Cv-Search-Total", found.total.to_string()),
    ];
    if let Some(n) = note {
        headers.push(("X-Cv-Search-Note", header_safe(&n)));
    }
    (200, json!(rows), headers)
}

/// A header-safe rendering of a note: header values are a byte channel with no escaping, so keep
/// them printable ASCII on one line ([`Response::add_header`] drops anything else, and the note with it).
fn header_safe(s: &str) -> String {
    s.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { ' ' })
        .collect::<String>()
        .trim()
        .to_string()
}

// --- small helpers ---------------------------------------------------------

use cv_core::api::Query;

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

/// Serve a static file from the configured web root for a non-`/api` request.
///
/// `segments` is the already-decoded path (so `["web", "components", "cv-forest.js"]`). An empty
/// path maps to `index.html`. The resolved path is canonicalized and must stay inside the (already
/// canonical) root — any traversal (`..`, symlink escape, absolute injection) yields a 403. A
/// missing file falls back to `index.html` (so client-side deep links work), and only a missing
/// index is a 404. The `Content-Type` is inferred from the extension; static assets are not
/// CORS-tagged (same-origin) — only the JSON API is.
fn static_file(root: &Path, segments: &[String]) -> Response<'static> {
    let index = root.join("index.html");
    let resolved = if segments.is_empty() {
        index.clone()
    } else {
        // Reject obviously hostile components before touching the filesystem.
        if segments.iter().any(|s| s == ".." || s.contains('\0')) {
            return text_response(403, "text/plain; charset=utf-8", b"forbidden".to_vec());
        }
        let mut p = root.to_path_buf();
        for s in segments {
            p.push(s);
        }
        p
    };

    // A deep link is a ROUTE and falls back to the SPA index; a missing ASSET is a 404. Falling
    // back for an asset hands the browser `index.html` under the asset's name, and for a module
    // script that surfaces as "Expected a JavaScript-or-Wasm module script but the server
    // responded with a MIME type of text/html" — which reads like a server misconfiguration
    // rather than the truth, that the file is not there. (`/pkg/cv_web.js`, the optional wasm
    // bundle, is not built unless you run wasm-pack.)
    let is_asset = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| !e.eq_ignore_ascii_case("html"));

    let serve_path = match resolved.canonicalize() {
        Ok(c) if c.starts_with(root) && c.is_file() => c,
        _ if is_asset => return text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
        _ => match index.canonicalize() {
            Ok(c) if c.starts_with(root) && c.is_file() => c,
            _ => return text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
        },
    };

    match std::fs::read(&serve_path) {
        Ok(bytes) => text_response(200, content_type_for(&serve_path), bytes),
        Err(_) => text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

/// Guess a `Content-Type` from a file extension — enough for the dashboard's asset set.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A raw-bytes response with a `Content-Type` and no CORS headers (static assets are same-origin).
fn text_response(status: u16, content_type: &str, body: Vec<u8>) -> Response<'static> {
    let mut resp = Response::bytes(status, body);
    resp.add_header("Content-Type", content_type);
    resp
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

/// Build a JSON response, CORS-tagged for `origin` when one was allowed, plus any route-specific
/// headers (see [`EXPOSED_HEADERS`] — a header a cross-origin dashboard cannot read is not a
/// channel).
fn json_response(
    status: u16,
    body: &Value,
    origin: Option<&str>,
    extra: &[(&'static str, String)],
) -> Response<'static> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut resp = Response::bytes(status, data);
    cors_headers(&mut resp, origin);
    for (field, value) in extra {
        resp.add_header(field, value);
    }
    resp.add_header("Content-Type", "application/json");
    resp
}

struct Download {
    body: cv_core::bytes::ByteSource,
    bytes: u64,
    content_type: &'static str,
    filename: String,
}

cv_core::routes! {
    pub(crate) const EXPORT_ROUTES;
    fn download_export(_post, parts, query: &Query) -> Option<Result<Download, (u16, Value)>> {
        GET ["api", "session", harness, id, "export"] => Some(export(harness, id, query)),
        _ => None,
    }
}

fn export(harness: &str, id: &str, query: &Query) -> Result<Download, (u16, Value)> {
    let format = query.get("format").unwrap_or_else(|| "markdown".into());
    let (content_type, extension, format) = match format.as_str() {
        "markdown" => (
            "text/markdown; charset=utf-8",
            "md",
            super::view::ExportFormat::Markdown,
        ),
        "opensession" => (
            "application/json",
            "opensession.json",
            super::view::ExportFormat::OpenSession,
        ),
        _ => return Err(err(400, "format must be markdown or opensession")),
    };
    let harness = Harness::parse(harness).ok_or_else(|| err(400, "unknown harness"))?;
    let (r, adapter) = cv_core::find(id, Some(harness))
        .map_err(|error| err(500, &error.to_string()))?
        .ok_or_else(|| err(404, "session not found"))?;
    let (body, bytes) = super::view::stage_export(adapter.as_ref(), &r, format)
        .map_err(|error| err(500, &format!("export failed: {error:#}")))?;
    let safe_id: String =
        r.id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .take(120)
            .collect();
    Ok(Download {
        body,
        bytes,
        content_type,
        filename: format!("cv-{}-{safe_id}.{extension}", harness.as_str()),
    })
}

fn staged_response<'a>(
    stage: &cv_core::api::StagedJson,
    reader: Box<dyn std::io::Read + 'a>,
    origin: Option<&str>,
) -> Response<'a> {
    let mut response = Response::stream(stage.status, reader, stage.bytes);
    response.add_header("Content-Type", "application/json");
    cors_headers(&mut response, origin);
    response
}

/// A 204 No Content, CORS-tagged for `origin` when one was allowed — for `OPTIONS` preflight.
fn no_content(origin: Option<&str>) -> Response<'static> {
    let mut resp = Response::empty(204);
    cors_headers(&mut resp, origin);
    resp
}

/// Response headers a cross-origin caller is allowed to READ. A browser hides every non-safelisted
/// response header from `fetch` unless it is named here, so `/api/search`'s "this answer is
/// degraded" signal would be invisible to a dashboard on a dev server without it.
const EXPOSED_HEADERS: &str = "X-Cv-Search-Source, X-Cv-Search-Note, Content-Disposition";

/// CORS headers echoing the one allowed `origin` — an unlisted origin gets none (the browser then
/// refuses to share the response with it). `Vary: Origin` keeps caches from mixing the two.
fn cors_headers(response: &mut Response<'_>, origin: Option<&str>) {
    response.add_header("Vary", "Origin");
    if let Some(o) = origin {
        response.add_header("Access-Control-Allow-Origin", o);
        response.add_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS");
        response.add_header("Access-Control-Allow-Headers", "Authorization, Content-Type");
        response.add_header("Access-Control-Expose-Headers", EXPOSED_HEADERS);
    }
}

#[cfg(test)]
mod staged_tests {
    #[cfg(feature = "probe-heap")]
    use super::*;
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated actual HTTP response allocation control"]
    fn staged_http_giant_selected_window_heap() {
        let world = crate::test_world("staged-http-heap");
        for kind in ["text", "arguments"] {
            for mib in [16, 64] {
                let body = "x\n月".repeat(mib * 1024 * 1024 / 5);
                let block = if kind == "text" {
                    json!({"type":"text","text":body})
                } else {
                    json!({"type":"tool_use","id":"c","name":"tool","input":{"payload":body}})
                };
                world.write_session("staged-http",&[json!({"type":"assistant","sessionId":"staged-http","uuid":"a","message":{"role":"assistant","content":[block]}})]);
                let parts = ["api", "session", "claude", "staged-http", "messages"];
                for staged in [false, true] {
                    let baseline = cv_testkit::heap::begin();
                    if staged {
                        let stage = cv_core::api::route_staged(&parts, "start=0&end=1").unwrap().unwrap();
                        let reader = stage.body.reader().unwrap();
                        staged_response(&stage, reader, None)
                            .write_to(std::io::sink(), false, false)
                            .unwrap();
                    } else {
                        let (status, value) = cv_core::api::route_json(&parts, "start=0&end=1");
                        json_response(status, &value, None, &[])
                            .write_to(std::io::sink(), false, false)
                            .unwrap();
                    }
                    let heap = cv_testkit::heap::end(baseline);
                    println!("{}", json!({"kind":kind,"mib":mib,"staged":staged,"heap":heap}));
                    if staged {
                        assert!(heap.peak_above_baseline_bytes < 3 * 1024 * 1024);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod semantic_service_tests {
    use super::*;

    #[test]
    fn search_rejects_cli_conflicts_before_reading_sources() {
        let _world = crate::test_world("search-conflicts");
        for flags in [
            "scan=1&kind=context",
            "scan=1&kind=all",
            "scan=1&kind=exchange",
            "semantic=1&kind=exchange",
            "scan=1&semantic=1",
        ] {
            let (status, body, _) = search(
                &Query::parse(&format!("q=needle&{flags}")),
                &cv_search::Searcher::default(),
            );
            assert_eq!(status, 400, "{flags}: {body}");
        }
    }

    #[test]
    fn token_comparison_needs_every_byte() {
        assert!(token_matches("opensesame", "opensesame"));
        for wrong in ["", "opensesam", "opensesamee", "Opensesame", "opensesamf", "x"] {
            assert!(!token_matches(wrong, "opensesame"), "{wrong:?}");
        }
        assert!(!token_matches("x", ""));
    }

    #[test]
    fn semantic_route_uses_service_owned_state_and_observes_replacement() {
        let dir = std::env::temp_dir().join(format!("cv-serve-semantic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("vectors.bin");
        let metadata = br#"{"model":"unused","projection":1,"records":[]}"#;
        let mut empty = b"CVEMBED".to_vec();
        empty.push(cv_core::source::VERSION as u8);
        empty.extend_from_slice(&0u32.to_le_bytes());
        empty.extend_from_slice(&0u32.to_le_bytes());
        empty.extend_from_slice(&(metadata.len() as u64).to_le_bytes());
        empty.extend_from_slice(metadata);
        std::fs::write(&path, &empty).unwrap();
        let semantic = cv_search::Searcher::from(cv_search::SemanticSearch::new(Some(path.clone())));
        for _ in 0..2 {
            let (status, body, headers) = route(
                false,
                &["api", "search"],
                "q=anything&semantic=1",
                &Query::parse("q=anything&semantic=1"),
                &semantic,
            );
            assert_eq!(status, 200);
            assert_eq!(body, json!([]));
            assert!(headers
                .iter()
                .any(|(name, value)| *name == "X-Cv-Search-Source" && value == "semantic"));
        }
        std::fs::write(&path, b"invalid replacement").unwrap();
        let (status, _, _) = route(
            false,
            &["api", "search"],
            "q=anything&semantic=1",
            &Query::parse("q=anything&semantic=1"),
            &semantic,
        );
        assert_eq!(status, 500);
        std::fs::write(&path, &empty).unwrap();
        let (status, body, _) = route(
            false,
            &["api", "search"],
            "q=anything&semantic=1",
            &Query::parse("q=anything&semantic=1"),
            &semantic,
        );
        assert_eq!(status, 200);
        assert_eq!(body, json!([]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
