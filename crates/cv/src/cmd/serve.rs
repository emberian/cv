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

impl ServeOpts {
    /// Whether Fleet Hub mode (`/api/hub/*`) is enabled via `$CV_SERVE_HUB`.
    pub(crate) fn hub_enabled(&self) -> bool {
        cv_core::env::var("CV_SERVE_HUB").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }
}

/// Fleet Hub routes answered when `hub` mode is enabled (`--hub` / `CV_SERVE_HUB=1`).
#[allow(dead_code)]
pub(crate) const HUB_ROUTES: &[&str] = &[
    "POST /api/hub/enroll",
    "POST /api/hub/upload_pack",
    "PUT /api/hub/packs/{pack_id}",
    "POST /api/hub/ingest_batch",
    "GET /api/hub/status",
    "GET /api/hub/snapshot",
    "GET /api/hub/sessions",
    "GET /api/hub/session/{producer}/{id}",
];

/// What one request handler needs to know beyond the request itself.
struct Ctx {
    /// The configured bind host, accepted in `Host`/`Origin` checks alongside the loopback names.
    bind_host: String,
    web_root: Option<PathBuf>,
    /// When set, every `/api/*` request must carry `Authorization: Bearer <token>` (or `?token=`
    /// on browser `EventSource` / download GETs that cannot attach custom headers).
    token: Option<String>,
    default_who: Option<String>,
    /// Whether Fleet Hub endpoints (`/api/hub/*`) are enabled on this server instance.
    hub: bool,
    searcher: cv_search::Searcher,
    stream: StreamHub,
}

fn validate_bind_exposure(host: &str, port: u16, token: Option<&str>, insecure_expose: bool, hub: bool) -> Result<()> {
    if !is_loopback_host(host) {
        if hub && token.is_none() {
            anyhow::bail!(
                "refusing to bind Fleet Hub mode on non-loopback host {host:?} without auth: \
                 hub mode accepts replicated fleet writes and NEVER allows unauthenticated network \
                 access (even with --insecure-expose). Set a bearer token (--token or $CV_SERVE_TOKEN)."
            );
        }
        if token.is_none() && !insecure_expose {
            anyhow::bail!(
                "refusing to bind non-loopback host {host:?} without auth: the API serves your full \
                 transcript corpus (which can contain secrets) to anyone who can reach the port.\n\
                 Set a token (--token or $CV_SERVE_TOKEN), or pass --insecure-expose to serve it open."
            );
        }
        eprintln!("cv serve: WARNING: binding non-loopback host {host:?} — the API is reachable from the network.");
        if token.is_none() {
            eprintln!(
                "cv serve: WARNING: --insecure-expose without a token: anyone who can reach {host}:{port} can read \
                 every transcript and act in the inbox as web:<who>."
            );
        }
    }
    Ok(())
}

/// Gatekeep the exposure, then serve until the process is killed: a non-loopback bind serves the
/// whole transcript corpus (secrets included) and the inbox's writes to anyone who can reach the
/// port, so it demands a bearer token or an explicit `--insecure-expose`, and warns either way.
pub(crate) fn cmd_serve(opts: ServeOpts) -> Result<()> {
    let hub = opts.hub_enabled();
    cmd_serve_with_hub(opts, hub)
}

/// Like [`cmd_serve`], with an explicit Fleet Hub enablement flag (for `--hub` CLI wiring).
pub(crate) fn cmd_serve_with_hub(opts: ServeOpts, hub: bool) -> Result<()> {
    let token = opts
        .token
        .or_else(|| cv_core::env::var("CV_SERVE_TOKEN").ok())
        .filter(|t| !t.is_empty());
    validate_bind_exposure(&opts.host, opts.port, token.as_deref(), opts.insecure_expose, hub)?;
    run(&opts.host, opts.port, opts.web, token, opts.who, opts.open, hub)
}

/// Run the HTTP server until the process is killed. Never returns under normal operation.
fn run(
    host: &str,
    port: u16,
    web_root: Option<PathBuf>,
    token: Option<String>,
    who: Option<String>,
    open: bool,
    hub: bool,
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
    let hub_tag = if hub { ", hub at /api/hub/*" } else { "" };
    match &web_root {
        Some(dir) => eprintln!(
            "cv serve on http://{host}:{port} — dashboard at /, inbox at /inbox, API at /api/*{hub_tag} (web root {})",
            dir.display()
        ),
        None => eprintln!("cv serve on http://{host}:{port} — inbox at /, API at /api/*{hub_tag} (no --web)"),
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

    let mut limits = crate::http::Limits::default();
    if hub {
        limits.request = std::time::Duration::from_secs(120);
    }
    let ctx = Ctx {
        bind_host: host.to_string(),
        web_root,
        token,
        default_who: who,
        hub,
        searcher: cv_search::Searcher,
        stream: StreamHub::new(),
    };
    crate::http::serve(listener, limits, move |request| handle(request, &ctx))
}

/// Handle one request: any error becomes a JSON error response.
fn handle(request: &mut Request<'_>, ctx: &Ctx) {
    let raw_host = header_value(request, "Host");
    // DNS-rebinding guard: a hostile page can point its own domain at 127.0.0.1 and fetch us
    // same-origin, but it can't forge the Host header — reject anything that isn't our own name.
    if !host_allowed(raw_host.as_deref(), &ctx.bind_host) {
        request.respond(json_response(403, &json!({"error": "forbidden host"}), None, &[]));
        return;
    }
    // Echoed on API responses only when the Origin is on the local allow-list (or same-origin on
    // an explicit non-loopback bind); never `*`.
    let raw_origin = header_value(request, "Origin");
    let origin = allowed_origin(raw_origin.as_deref(), raw_host.as_deref(), &ctx.bind_host);

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
    let put = method == "PUT";
    let is_task_post = post && matches!(parts.as_slice(), ["api", "task", _, _]);
    let is_hub_write = ctx.hub
        && ((post
            && matches!(
                parts.as_slice(),
                ["api", "hub", "enroll" | "upload_pack" | "ingest_batch"]
            ))
            || (put && matches!(parts.as_slice(), ["api", "hub", "packs", _])));

    // GET reads anything; POST is an inbox write (`/api/task/<id>/<verb>`) or (when hub mode is
    // enabled) a Fleet Hub replication write (`/api/hub/*`, plus `PUT /api/hub/packs/<id>`).
    if !(method == "GET" || is_task_post || is_hub_write) {
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

    let q = cv_core::api::Query::parse(query);
    if is_hub_write && !is_loopback_host(&ctx.bind_host) && ctx.token.is_none() {
        request.respond(json_response(
            401,
            &json!({"error": "hub network writes require a bearer token"}),
            origin.as_deref(),
            &[],
        ));
        return;
    }
    if let Some(token) = &ctx.token {
        let header_ok = header_value(request, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .is_some_and(|t| token_matches(&t, token));
        let query_ok = method == "GET"
            && (parts.as_slice() == ["api", "stream"]
                || matches!(parts.as_slice(), ["api", "session", _, _, "export"]))
            && q.get("token").is_some_and(|t| token_matches(&t, token));
        if !(header_ok || query_ok) {
            request.respond(json_response(
                401,
                &json!({"error": "missing or invalid bearer token"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
    }

    if is_hub_write {
        if raw_origin.is_some() && origin.is_none() {
            request.respond(json_response(
                403,
                &json!({"error": "forbidden origin"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
        let resp = handle_hub_write(request, &parts, origin.as_deref(), ctx);
        request.respond(resp);
        return;
    }

    if matches!(parts.as_slice(), ["api", "hub", ..]) {
        if !ctx.hub {
            request.respond(json_response(
                404,
                &json!({"error": "not found"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
        let (status, body, extra) = handle_hub_get(&parts, &q);
        request.respond(json_response(status, &body, origin.as_deref(), &extra));
        return;
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

    match serve_tasks::route(post, &parts, &q, &body, ctx.default_who.as_deref()) {
        Some(serve_tasks::Answer::Json(status, v)) => {
            if post && status < 400 {
                ctx.stream.bump(&["tasks"]);
            }
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
            stamp_latest_snapshot(&mut resp, false);
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
            if parts.as_slice() == ["api", "stream"] {
                serve_stream(request, &q, origin.as_deref(), &ctx.stream);
                return;
            }
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
                            stamp_latest_snapshot(&mut response, false);
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

/// Parse a `Host` header or URL authority into `(unbracketed_host, effective_port)`.
fn authority_host_port(authority: &str, default_port: u16) -> Option<(&str, u16)> {
    fn parse_port(suffix: &str, default_port: u16) -> Option<u16> {
        if suffix.is_empty() {
            return Some(default_port);
        }
        let port = suffix.strip_prefix(':')?;
        (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| port.parse::<u16>().ok())
            .flatten()
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (name, suffix) = rest.split_once(']')?;
        name.parse::<std::net::Ipv6Addr>().ok()?;
        return Some((name, parse_port(suffix, default_port)?));
    }
    let (name, suffix) = authority
        .find(':')
        .map(|at| authority.split_at(at))
        .unwrap_or((authority, ""));
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte)))
    .then(|| Some((name, parse_port(suffix, default_port)?)))
    .flatten()
}

/// The hostname part of a `Host` header or URL authority: strips a `:port`, unbrackets IPv6.
fn host_name(authority: &str) -> Option<&str> {
    authority_host_port(authority, 80).map(|(name, _)| name)
}

/// A name this server answers to: the loopback names, or the configured bind host.
fn is_local_name(name: &str, bind_host: &str) -> bool {
    name == "127.0.0.1"
        || name == "::1"
        || name.eq_ignore_ascii_case("localhost")
        || name.eq_ignore_ascii_case(bind_host.trim_matches(['[', ']']))
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
/// the Tauri app's origins, an http(s) origin on a local name (any port — a dev dashboard on
/// e.g. `http://localhost:5173` is same-machine), or — when explicitly bound to a non-loopback
/// interface (`0.0.0.0`, `::`, LAN IP) — a same-origin request whose `Origin` host+port matches
/// the validated `Host` header.
fn allowed_origin(origin: Option<&str>, host_header: Option<&str>, bind_host: &str) -> Option<String> {
    let o = origin?;
    if matches!(
        o,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) {
        return Some(o.to_string());
    }
    let (default_port, authority) = if let Some(rest) = o.strip_prefix("http://") {
        (80u16, rest)
    } else {
        let rest = o.strip_prefix("https://")?;
        (443u16, rest)
    };
    let (origin_host, origin_port) = authority_host_port(authority, default_port)?;
    if is_local_name(origin_host, bind_host) {
        return Some(o.to_string());
    }
    if !is_loopback_host(bind_host) {
        let (req_host, req_port) = authority_host_port(host_header?, default_port)?;
        if origin_host.eq_ignore_ascii_case(req_host) && origin_port == req_port {
            return Some(o.to_string());
        }
    }
    None
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
        GET ["api", "stream"] => {
            let (status, body) = err(500, "stream is handled before buffered routing");
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

/// `GET /api/search?q=&limit=&harness=&cwd=&kind=&scan=1` — full-text search over
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
/// `live`), with a human sentence in `X-Cv-Search-Note` whenever the answer may be
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
    let scan = flag("scan");
    let scope = match params.get("kind") {
        None => cv_search::Scope::Exchange,
        Some(kind) => match cv_search::Scope::parse(&kind) {
            Some(scope) => scope,
            None => return bad(400, &format!("unknown kind {kind:?}: exchange, context or all")),
        },
    };
    // The CLI declares --kind as conflicting with --scan; the HTTP door keeps the
    // same rule, so one request shape means one thing on both.
    if scan && params.get("kind").is_some() {
        return bad(400, "this mode searches the exchange only; kind= needs the index");
    }
    let source = if scan { Source::Live } else { Source::Index };
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
///
/// A page is never framed: the inbox turns decisions into buttons whose same-origin `POST`s pass
/// every Origin check, so a hostile site that could load it in an invisible frame could have the
/// person click an agent's decision through (clickjacking). `X-Frame-Options` covers browsers
/// that predate `frame-ancestors`. `nosniff` keeps a served file from being run as another type.
fn text_response(status: u16, content_type: &str, body: Vec<u8>) -> Response<'static> {
    let mut resp = Response::bytes(status, body);
    resp.add_header("Content-Type", content_type);
    resp.add_header("X-Content-Type-Options", "nosniff");
    if content_type.starts_with("text/html") {
        resp.add_header("X-Frame-Options", "DENY");
        resp.add_header("Content-Security-Policy", "frame-ancestors 'none'");
    }
    resp
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

fn latest_hub_snapshot_id() -> Option<String> {
    cv_core::state::hub::HubStore::default_hub()
        .ok()
        .and_then(|hub| hub.latest_publication_id_fast().ok().flatten())
}

fn stamp_latest_snapshot(response: &mut Response<'_>, already_stamped: bool) {
    if !already_stamped {
        if let Some(pub_id) = latest_hub_snapshot_id() {
            response.add_header("X-Cv-Snapshot", &header_safe(&pub_id));
        }
    }
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
    let mut has_snapshot_hdr = false;
    for (field, value) in extra {
        if field.eq_ignore_ascii_case("X-Cv-Snapshot") {
            has_snapshot_hdr = true;
        }
        resp.add_header(field, value);
    }
    stamp_latest_snapshot(&mut resp, has_snapshot_hdr);
    resp.add_header("Content-Type", "application/json");
    resp
}

// --- Fleet Hub endpoints (`/api/hub/*`) ------------------------------------

const HUB_ENROLL_LIMIT: u64 = 1 << 20;
const HUB_INGEST_BATCH_LIMIT: u64 = 32 << 20;
const HUB_PACK_JSON_LIMIT: u64 = 64 << 20;
const HUB_PACK_IDX_LIMIT: usize = 16 << 20;
const HUB_PACK_ZST_LIMIT: u64 = 256 << 20;

struct BoundedReader<'a, R> {
    inner: &'a mut R,
    limit: u64,
    read_so_far: u64,
    exceeded: bool,
}

impl<R: Read> Read for BoundedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read_so_far = self.read_so_far.saturating_add(n as u64);
        if self.read_so_far > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pack body exceeds 256 MiB limit",
            ));
        }
        Ok(n)
    }
}

fn media_type_of(request: &Request<'_>) -> String {
    header_value(request, "Content-Type")
        .and_then(|ct| ct.split(';').next().map(|s| s.trim().to_ascii_lowercase()))
        .unwrap_or_default()
}

fn read_json_body(
    request: &mut Request<'_>,
    limit: u64,
    limit_label: &str,
    origin: Option<&str>,
) -> std::result::Result<Value, Response<'static>> {
    if media_type_of(request) != "application/json" {
        return Err(json_response(
            403,
            &json!({"error": "a write must be application/json from a local origin"}),
            origin,
            &[],
        ));
    }
    let mut buf = String::new();
    if request.by_ref().take(limit + 1).read_to_string(&mut buf).is_err() {
        return Err(json_response(400, &json!({"error": "unreadable body"}), origin, &[]));
    }
    if buf.len() as u64 > limit {
        return Err(json_response(
            413,
            &json!({"error": format!("body exceeds {limit_label} limit")}),
            origin,
            &[],
        ));
    }
    if buf.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str::<Value>(&buf)
        .map_err(|e| json_response(400, &json!({"error": format!("body is not JSON: {e}")}), origin, &[]))
}

fn decode_hex(hex: &str) -> std::result::Result<Vec<u8>, String> {
    let s = hex.trim();
    if !s.len().is_multiple_of(2) {
        return Err("hex string must have an even number of digits".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let chunk = std::str::from_utf8(&bytes[i..i + 2]).map_err(|e| e.to_string())?;
        let byte = u8::from_str_radix(chunk, 16).map_err(|_| format!("invalid hex byte {chunk:?}"))?;
        out.push(byte);
        i += 2;
    }
    Ok(out)
}

fn hub_error_reply(err: cv_core::state::hub::HubError) -> Reply {
    use cv_core::state::hub::HubError;
    match err {
        HubError::InvalidArgument(msg) => (400, json!({"error": msg}), Vec::new()),
        HubError::NotFound(msg) => (404, json!({"error": msg}), Vec::new()),
        HubError::MissingPackDependency { pack_id, detail } => (
            424,
            json!({
                "error": format!("missing or unverified pack dependency `{pack_id}`: {detail}"),
                "pack_id": pack_id,
                "detail": detail,
            }),
            Vec::new(),
        ),
        HubError::PackConflict {
            pack_id,
            existing_digest,
            incoming_digest,
        } => (
            409,
            json!({
                "error": format!("conflicting digest for pack `{pack_id}`"),
                "pack_id": pack_id,
                "existing_digest": existing_digest,
                "incoming_digest": incoming_digest,
            }),
            Vec::new(),
        ),
        HubError::EventConflict(detail) => {
            let frontier_map: serde_json::Map<String, Value> = detail
                .origin_frontier
                .iter()
                .map(|((p, e), &seq)| (cv_core::state::hub::encode_pair_key(p, e), json!(seq)))
                .collect();
            (
                409,
                json!({
                    "error": detail.to_string(),
                    "conflict": "digest_mismatch",
                    "quarantined": true,
                    "event_id": detail.event_id,
                    "existing_digest": detail.existing_digest,
                    "conflicting_digest": detail.conflicting_digest,
                    "quarantine_count": detail.quarantine_count,
                    "origin_frontier": frontier_map,
                }),
                Vec::new(),
            )
        }
        HubError::Pack(msg) => (400, json!({"error": msg}), Vec::new()),
        HubError::Io(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
            ) =>
        {
            (
                400,
                json!({"error": format!("unreadable request stream: {e}")}),
                Vec::new(),
            )
        }
        other => (500, json!({"error": other.to_string()}), Vec::new()),
    }
}

fn handle_hub_write(request: &mut Request<'_>, parts: &[&str], origin: Option<&str>, ctx: &Ctx) -> Response<'static> {
    use cv_core::state::hub::{
        EnrollRequest, EnrollResponse, HubError, HubStore, IngestBatchRequest, PACK_STREAM_CONTENT_TYPE,
    };

    let hub = match HubStore::default_hub() {
        Ok(h) => h,
        Err(e) => {
            let (status, body, extra) = hub_error_reply(e);
            return json_response(status, &body, origin, &extra);
        }
    };

    match parts {
        ["api", "hub", "enroll"] => {
            let val = match read_json_body(request, HUB_ENROLL_LIMIT, "1 MiB", origin) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let req: EnrollRequest = match serde_json::from_value(val) {
                Ok(r) => r,
                Err(e) => {
                    return json_response(
                        400,
                        &json!({"error": format!("invalid enroll payload: {e}")}),
                        origin,
                        &[],
                    );
                }
            };
            if req.producer_id.trim().is_empty() {
                return json_response(400, &json!({"error": "producer_id must not be empty"}), origin, &[]);
            }
            match hub.enroll_producer(&req.producer_id, &req.host_label, &req.metadata) {
                Ok(producer) => {
                    ctx.stream.bump(&["hub"]);
                    let out = serde_json::to_value(EnrollResponse { producer })
                        .unwrap_or_else(|_| json!({"error": "serialize failed"}));
                    json_response(200, &out, origin, &[("Cache-Control", "no-store".into())])
                }
                Err(e) => {
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, origin, &extra)
                }
            }
        }
        ["api", "hub", "upload_pack"] | ["api", "hub", "packs", _] => {
            let url_pack_id = match parts {
                ["api", "hub", "packs", pid] => Some(*pid),
                _ => None,
            };
            let ct = media_type_of(request);
            if ct == PACK_STREAM_CONTENT_TYPE || ct == "application/octet-stream" {
                let hdr_pack_id = header_value(request, "X-Cv-Pack-Id");
                let pack_id = match (url_pack_id, hdr_pack_id.as_deref()) {
                    (Some(u), Some(h)) if u != h => {
                        return json_response(
                            400,
                            &json!({"error": format!("URL pack_id {u:?} disagrees with X-Cv-Pack-Id {h:?}")}),
                            origin,
                            &[],
                        );
                    }
                    (Some(u), _) => u.to_string(),
                    (None, Some(h)) if !h.trim().is_empty() => h.trim().to_string(),
                    (None, _) => {
                        return json_response(400, &json!({"error": "missing X-Cv-Pack-Id header"}), origin, &[]);
                    }
                };
                let expected_digest = header_value(request, "X-Cv-Pack-Digest");
                let producer_id = header_value(request, "X-Cv-Producer-Id");
                let Some(idx_len_raw) = header_value(request, "X-Cv-Pack-Idx-Len") else {
                    return json_response(400, &json!({"error": "missing X-Cv-Pack-Idx-Len header"}), origin, &[]);
                };
                let Ok(idx_len) = idx_len_raw.trim().parse::<usize>() else {
                    return json_response(400, &json!({"error": "invalid X-Cv-Pack-Idx-Len header"}), origin, &[]);
                };
                if idx_len > HUB_PACK_IDX_LIMIT {
                    return json_response(413, &json!({"error": "pack index exceeds 16 MiB limit"}), origin, &[]);
                }

                let mut idx_bytes = vec![0u8; idx_len];
                if request.read_exact(&mut idx_bytes).is_err() {
                    return json_response(400, &json!({"error": "truncated pack index stream"}), origin, &[]);
                }

                let mut bounded = BoundedReader {
                    inner: request,
                    limit: HUB_PACK_ZST_LIMIT,
                    read_so_far: 0,
                    exceeded: false,
                };
                let result = hub.upload_pack_stream(
                    &pack_id,
                    producer_id.as_deref(),
                    expected_digest.as_deref(),
                    &idx_bytes,
                    &mut bounded,
                );
                if bounded.exceeded {
                    return json_response(413, &json!({"error": "pack body exceeds 256 MiB limit"}), origin, &[]);
                }
                match result {
                    Ok(resp) => {
                        ctx.stream.bump(&["hub"]);
                        let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                        json_response(200, &out, origin, &[("Cache-Control", "no-store".into())])
                    }
                    Err(e) => {
                        let (status, body, extra) = hub_error_reply(e);
                        json_response(status, &body, origin, &extra)
                    }
                }
            } else if ct == "application/json" {
                let val = match read_json_body(request, HUB_PACK_JSON_LIMIT, "64 MiB", origin) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                let body_pack_id = val.get("pack_id").and_then(Value::as_str);
                let pack_id = match (url_pack_id, body_pack_id) {
                    (Some(u), Some(b)) if u != b => {
                        return json_response(
                            400,
                            &json!({"error": format!("URL pack_id {u:?} disagrees with body pack_id {b:?}")}),
                            origin,
                            &[],
                        );
                    }
                    (Some(u), _) => u.to_string(),
                    (None, Some(b)) if !b.trim().is_empty() => b.trim().to_string(),
                    (None, _) => {
                        return json_response(400, &json!({"error": "missing pack_id"}), origin, &[]);
                    }
                };
                let producer_id = val.get("producer_id").and_then(Value::as_str);
                let expected_digest = val.get("blake3_digest").and_then(Value::as_str);
                let Some(idx_val) = val.get("idx") else {
                    return json_response(400, &json!({"error": "missing idx field"}), origin, &[]);
                };
                let idx_bytes = match idx_val {
                    Value::String(s) => s.as_bytes().to_vec(),
                    other => match serde_json::to_vec(other) {
                        Ok(b) => b,
                        Err(e) => return json_response(400, &json!({"error": e.to_string()}), origin, &[]),
                    },
                };
                let Some(hex_str) = val.get("pack_zst_hex").and_then(Value::as_str) else {
                    return json_response(400, &json!({"error": "missing pack_zst_hex field"}), origin, &[]);
                };
                let zst_bytes = match decode_hex(hex_str) {
                    Ok(b) => b,
                    Err(e) => {
                        return json_response(
                            400,
                            &json!({"error": format!("invalid pack_zst_hex: {e}")}),
                            origin,
                            &[],
                        )
                    }
                };
                match hub.upload_pack_bytes(&pack_id, producer_id, expected_digest, &idx_bytes, &zst_bytes) {
                    Ok(resp) => {
                        ctx.stream.bump(&["hub"]);
                        let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                        json_response(200, &out, origin, &[("Cache-Control", "no-store".into())])
                    }
                    Err(e) => {
                        let (status, body, extra) = hub_error_reply(e);
                        json_response(status, &body, origin, &extra)
                    }
                }
            } else {
                json_response(
                    403,
                    &json!({
                        "error": "pack upload must be application/vnd.cv.pack+octet-stream, application/octet-stream, or application/json"
                    }),
                    origin,
                    &[],
                )
            }
        }
        ["api", "hub", "ingest_batch"] => {
            let val = match read_json_body(request, HUB_INGEST_BATCH_LIMIT, "32 MiB", origin) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let batch_req: IngestBatchRequest = if val.is_array() {
                match serde_json::from_value(val) {
                    Ok(events) => IngestBatchRequest { events },
                    Err(e) => {
                        return json_response(
                            400,
                            &json!({"error": format!("invalid ingest_batch events array: {e}")}),
                            origin,
                            &[],
                        );
                    }
                }
            } else {
                match serde_json::from_value(val) {
                    Ok(req) => req,
                    Err(e) => {
                        return json_response(
                            400,
                            &json!({"error": format!("invalid ingest_batch request: {e}")}),
                            origin,
                            &[],
                        );
                    }
                }
            };

            match hub.ingest_batch(&batch_req) {
                Ok(resp) => {
                    ctx.stream.bump(&["hub", "sessions"]);
                    let mut extra = vec![("Cache-Control", "no-store".to_string())];
                    if let Some(ref snap) = resp.snapshot {
                        extra.push(("X-Cv-Snapshot", header_safe(&snap.publication_id)));
                        extra.push(("X-Cv-Hub-Generation", snap.hub_generation.to_string()));
                    }
                    let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                    json_response(200, &out, origin, &extra)
                }
                Err(e) => {
                    if matches!(e, HubError::EventConflict(_)) {
                        ctx.stream.bump(&["hub"]);
                    }
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, origin, &extra)
                }
            }
        }
        _ => json_response(404, &json!({"error": "not found"}), origin, &[]),
    }
}

fn handle_hub_get(parts: &[&str], q: &Query) -> Reply {
    use cv_core::state::hub::HubStore;

    let hub = match HubStore::default_hub() {
        Ok(h) => h,
        Err(e) => return hub_error_reply(e),
    };

    match parts {
        ["api", "hub", "status"] => match hub.hub_status() {
            Ok(status_resp) => {
                let mut extra = vec![("Cache-Control", "no-store".to_string())];
                if let Some(ref snap) = status_resp.latest_snapshot {
                    extra.push(("X-Cv-Snapshot", header_safe(&snap.publication_id)));
                    extra.push(("X-Cv-Hub-Generation", snap.hub_generation.to_string()));
                }
                match serde_json::to_value(status_resp) {
                    Ok(val) => (200, val, extra),
                    Err(e) => (500, json!({"error": e.to_string()}), Vec::new()),
                }
            }
            Err(e) => hub_error_reply(e),
        },
        ["api", "hub", "snapshot"] => {
            let want = q.get("id").or_else(|| q.get("snapshot"));
            match hub.snapshot_response(want.as_deref()) {
                Ok(snap_resp) => {
                    let extra = vec![
                        ("Cache-Control", "no-store".to_string()),
                        ("X-Cv-Snapshot", header_safe(&snap_resp.snapshot.publication_id)),
                        ("X-Cv-Hub-Generation", snap_resp.snapshot.hub_generation.to_string()),
                    ];
                    match serde_json::to_value(snap_resp) {
                        Ok(val) => (200, val, extra),
                        Err(e) => (500, json!({"error": e.to_string()}), Vec::new()),
                    }
                }
                Err(e) => hub_error_reply(e),
            }
        }
        ["api", "hub", "sessions"] => {
            let offset = match q.unsigned("offset") {
                Ok(v) => v.unwrap_or(0),
                Err(e) => return (400, json!({"error": e}), Vec::new()),
            };
            let limit = match q.unsigned("limit") {
                Ok(v) => v,
                Err(e) => return (400, json!({"error": e}), Vec::new()),
            };
            let snapshot = q.get("snapshot");
            let producer = q.get("producer");
            let harness = q.get("harness");
            let cwd = q.get("cwd");
            let query_str = q.get("q");

            match hub.query_sessions(
                snapshot.as_deref(),
                producer.as_deref(),
                harness.as_deref(),
                cwd.as_deref(),
                query_str.as_deref(),
                offset,
                limit,
            ) {
                Ok(page) => {
                    let mut extra = vec![("Cache-Control", "no-store".to_string())];
                    if let Some(ref sid) = page.snapshot_id {
                        extra.push(("X-Cv-Snapshot", header_safe(sid)));
                        extra.push(("X-Cv-Hub-Generation", page.hub_generation.to_string()));
                    }
                    match serde_json::to_value(page) {
                        Ok(val) => (200, val, extra),
                        Err(e) => (500, json!({"error": e.to_string()}), Vec::new()),
                    }
                }
                Err(e) => hub_error_reply(e),
            }
        }
        ["api", "hub", "session", producer_id, session_id] => {
            let snapshot = q.get("snapshot");
            match hub.hydrate_session(producer_id, session_id, snapshot.as_deref()) {
                Ok((manifest, hydrated)) => match hydrated.to_json() {
                    Ok(val) => {
                        let extra = vec![
                            ("Cache-Control", "no-store".to_string()),
                            ("X-Cv-Snapshot", header_safe(&manifest.publication_id)),
                            ("X-Cv-Hub-Generation", manifest.hub_generation.to_string()),
                        ];
                        (200, val, extra)
                    }
                    Err(e) => hub_error_reply(e),
                },
                Err(e) => hub_error_reply(e),
            }
        }
        _ => (404, json!({"error": "not found"}), Vec::new()),
    }
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
    stamp_latest_snapshot(&mut response, false);
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
/// degraded" signal and hit count would be invisible to a dashboard on a dev server without it.
const EXPOSED_HEADERS: &str =
    "X-Cv-Search-Source, X-Cv-Search-Note, X-Cv-Search-Total, Content-Disposition, X-Cv-Snapshot, X-Cv-Hub-Generation";

/// CORS headers echoing the one allowed `origin` — an unlisted origin gets none (the browser then
/// refuses to share the response with it). `Vary: Origin` keeps caches from mixing the two.
fn cors_headers(response: &mut Response<'_>, origin: Option<&str>) {
    response.add_header("Vary", "Origin");
    if let Some(o) = origin {
        response.add_header("Access-Control-Allow-Origin", o);
        response.add_header("Access-Control-Allow-Methods", "GET, POST, PUT, OPTIONS");
        response.add_header("Access-Control-Allow-Headers", "Authorization, Content-Type");
        response.add_header("Access-Control-Expose-Headers", EXPOSED_HEADERS);
    }
}

// --- live SSE change stream (`/api/stream`) --------------------------------

const MAX_SSE_FOLLOWERS: usize = 16;
const SSE_STEP: std::time::Duration = std::time::Duration::from_millis(250);
const SSE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);
const SESSION_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct FileStamp {
    present: bool,
    len: u64,
    modified_ns: u128,
}

fn file_stamp(path: &Path) -> FileStamp {
    let Ok(meta) = std::fs::metadata(path) else {
        return FileStamp::default();
    };
    let modified_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    FileStamp {
        present: true,
        len: meta.len(),
        modified_ns,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DiskStamp {
    tasks_log: FileStamp,
    catalog: (FileStamp, FileStamp),
    watch: (FileStamp, FileStamp),
    hub_state: (FileStamp, FileStamp),
    sessions_digest: u64,
}

impl DiskStamp {
    fn capture(cached_sessions_digest: Option<u64>) -> Self {
        let cv_home = cv_core::env::var_os("CLUSTERVISION_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| cv_core::env::home_dir().map(|h| h.join(".clustervision")));
        let tasks_log = cv_home
            .as_deref()
            .map(|h| file_stamp(&h.join("tasks").join("events.jsonl")))
            .unwrap_or_default();
        let watch = cv_home
            .as_deref()
            .map(|h| (file_stamp(&h.join("watches.json")), file_stamp(&h.join("watch"))))
            .unwrap_or_default();
        let hub_state = cv_home
            .as_deref()
            .map(|h| (file_stamp(&h.join("state.db")), file_stamp(&h.join("state.db-wal"))))
            .unwrap_or_default();
        let catalog = cv_core::env::cv_cache()
            .map(|c| (file_stamp(&c.join("catalog.db")), file_stamp(&c.join("catalog.db-wal"))))
            .unwrap_or_default();
        let sessions_digest = cached_sessions_digest.unwrap_or_else(sessions_fingerprint);
        Self {
            tasks_log,
            catalog,
            watch,
            hub_state,
            sessions_digest,
        }
    }
}

fn sessions_fingerprint() -> u64 {
    use std::hash::{Hash, Hasher};
    let refs = cv_core::sessions();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    refs.len().hash(&mut hasher);
    for r in &refs {
        r.id.hash(&mut hasher);
        r.message_count.hash(&mut hasher);
        if let Some(ts) = r.updated_at {
            ts.timestamp_millis().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// How many past changes the stream remembers, so a follower that missed several learns the union
/// of what changed; one that fell further behind is told to reload everything (`reset`).
const STREAM_HISTORY: usize = 64;

struct StreamState {
    cursor: u64,
    stamp: DiskStamp,
    /// `(offset, kinds)` of the most recent changes, oldest first.
    history: std::collections::VecDeque<(u64, Vec<&'static str>)>,
    last_session_scan: std::time::Instant,
}

impl StreamState {
    fn record(&mut self, kinds: Vec<&'static str>) {
        self.cursor += 1;
        if self.history.len() == STREAM_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back((self.cursor, kinds));
    }

    /// Every kind that changed after offset `seen`, in first-seen order. A follower older than
    /// the remembered history cannot be told exactly what it missed, so it gets `reset`.
    fn kinds_after(&self, seen: u64) -> Vec<&'static str> {
        let oldest = self.history.front().map_or(self.cursor + 1, |(offset, _)| *offset);
        if seen + 1 < oldest {
            return vec!["reset"];
        }
        let mut out: Vec<&'static str> = Vec::new();
        for (_, kinds) in self.history.iter().filter(|(offset, _)| *offset > seen) {
            for kind in kinds {
                if !out.contains(kind) {
                    out.push(kind);
                }
            }
        }
        out
    }
}

/// Where a follower says it is: nowhere yet, at an offset of this server's generation, or at a
/// cursor this process never issued (another run's generation, a bare offset, garbage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamResume {
    Fresh,
    At(u64),
    Foreign,
}

struct StreamHub {
    /// This process's stream generation. Offsets restart with every `cv serve`, so a cursor is
    /// `<gen>:<offset>` and one from another generation is never compared by offset alone.
    gen: String,
    state: std::sync::Mutex<StreamState>,
    wake: std::sync::Condvar,
    followers: std::sync::atomic::AtomicUsize,
}

struct FollowerSlot<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> FollowerSlot<'a> {
    fn acquire(counter: &'a std::sync::atomic::AtomicUsize) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut cur = counter.load(Ordering::Acquire);
        loop {
            if cur >= MAX_SSE_FOLLOWERS {
                return None;
            }
            match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Self(counter)),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for FollowerSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl StreamHub {
    fn new() -> Self {
        let now = std::time::Instant::now();
        let started_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self {
            gen: format!("{:x}", started_ns ^ (u64::from(std::process::id()) << 44)),
            state: std::sync::Mutex::new(StreamState {
                cursor: 1,
                stamp: DiskStamp::capture(None),
                history: std::collections::VecDeque::from([(1, vec!["ready"])]),
                last_session_scan: now,
            }),
            wake: std::sync::Condvar::new(),
            followers: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The wire cursor for `offset`: `<gen>:<offset>`.
    fn cursor_id(&self, offset: u64) -> String {
        format!("{}:{offset}", self.gen)
    }

    /// Read a follower's `after=` / `Last-Event-ID` cursor.
    fn resume_point(&self, raw: Option<&str>, current: u64) -> StreamResume {
        let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return StreamResume::Fresh;
        };
        match raw.split_once(':') {
            Some((gen, offset)) if gen == self.gen => match offset.parse::<u64>() {
                Ok(offset) if offset <= current => StreamResume::At(offset),
                _ => StreamResume::Foreign,
            },
            _ => StreamResume::Foreign,
        }
    }

    fn bump(&self, kinds: &[&'static str]) {
        let cv_home = cv_core::env::var_os("CLUSTERVISION_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| cv_core::env::home_dir().map(|h| h.join(".clustervision")));
        let fresh_tasks = cv_home
            .as_deref()
            .map(|h| file_stamp(&h.join("tasks").join("events.jsonl")))
            .unwrap_or_default();
        let fresh_hub = cv_home
            .as_deref()
            .map(|h| (file_stamp(&h.join("state.db")), file_stamp(&h.join("state.db-wal"))))
            .unwrap_or_default();
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.stamp.tasks_log = fresh_tasks;
        guard.stamp.hub_state = fresh_hub;
        guard.record(kinds.to_vec());
        drop(guard);
        self.wake.notify_all();
    }

    /// Re-stat the watched stores, record a change if any moved, and return the current offset.
    fn refresh(&self) -> u64 {
        let now = std::time::Instant::now();
        let cached_sessions = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(guard.last_session_scan) >= SESSION_SCAN_INTERVAL {
                guard.last_session_scan = now;
                None
            } else {
                Some(guard.stamp.sessions_digest)
            }
        };
        let next = DiskStamp::capture(cached_sessions);
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut kinds = Vec::new();
        if next.tasks_log != guard.stamp.tasks_log {
            guard.stamp.tasks_log = next.tasks_log;
            kinds.push("tasks");
        }
        let sessions_changed = next.catalog != guard.stamp.catalog
            || (cached_sessions.is_none() && next.sessions_digest != guard.stamp.sessions_digest);
        if sessions_changed {
            guard.stamp.catalog = next.catalog;
            if cached_sessions.is_none() {
                guard.stamp.sessions_digest = next.sessions_digest;
            }
            kinds.push("sessions");
        }
        if next.watch != guard.stamp.watch {
            guard.stamp.watch = next.watch;
            kinds.push("watch");
        }
        if next.hub_state != guard.stamp.hub_state {
            guard.stamp.hub_state = next.hub_state;
            kinds.push("hub");
        }
        if !kinds.is_empty() {
            guard.record(kinds);
            let out = guard.cursor;
            drop(guard);
            self.wake.notify_all();
            return out;
        }
        guard.cursor
    }

    /// The current offset and every kind that changed after `seen`.
    fn changes_after(&self, seen: u64) -> (u64, Vec<&'static str>) {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (guard.cursor, guard.kinds_after(seen))
    }

    fn wait_after(&self, seen: u64, step: std::time::Duration) -> (u64, Vec<&'static str>) {
        {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.cursor > seen {
                return (guard.cursor, guard.kinds_after(seen));
            }
            let (guard, _) = self.wake.wait_timeout(guard, step).unwrap_or_else(|e| e.into_inner());
            if guard.cursor > seen {
                return (guard.cursor, guard.kinds_after(seen));
            }
        }
        self.refresh();
        self.changes_after(seen)
    }
}

fn serve_stream(request: &mut Request<'_>, q: &Query, origin: Option<&str>, hub: &StreamHub) {
    let last_event_id = header_value(request, "Last-Event-ID");
    // `after=<gen>:<offset>` (or the same cursor in `Last-Event-ID`, which a reconnecting
    // EventSource sends by itself).
    let after: Option<String> = q.get("after").map(|s| s.to_string()).or_else(|| last_event_id.clone());
    let max_events = q.get("max_events").and_then(|s| s.parse::<usize>().ok());
    let wait_limit = q
        .get("wait_ms")
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    let live = q.flag("follow")
        || last_event_id.is_some()
        || header_value(request, "Accept").is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));

    let _slot = if live {
        match FollowerSlot::acquire(&hub.followers) {
            Some(slot) => Some(slot),
            None => {
                request.respond(json_response(
                    503,
                    &json!({"error": "too many active event streams"}),
                    origin,
                    &[],
                ));
                return;
            }
        }
    } else {
        None
    };

    let mut resp = Response::sse(|sse| {
        let cur = hub.refresh();
        let mut seen = cur;
        let mut emitted = 0usize;
        let change = |sse: &mut crate::http::SseWriter<'_>, offset: u64, kinds: &[&'static str]| {
            let id = hub.cursor_id(offset);
            let payload = json!({"cursor": id, "kinds": kinds}).to_string();
            sse.event(Some(&id), Some("change"), &payload)
        };
        match hub.resume_point(after.as_deref(), cur) {
            StreamResume::Fresh => {
                change(sse, cur, &["ready"])?;
                if !live || max_events == Some(0) {
                    return Ok(());
                }
            }
            // A cursor this process never issued: the follower was watching another run of the
            // server (offsets restart with each), so anything may have changed since. Say so
            // now instead of comparing offsets across generations and going quiet.
            StreamResume::Foreign => {
                change(sse, cur, &["reset"])?;
                emitted += 1;
                if !live || max_events.is_some_and(|m| emitted >= m) {
                    return Ok(());
                }
            }
            StreamResume::At(prev) if cur > prev => {
                let (_, kinds) = hub.changes_after(prev);
                change(sse, cur, &kinds)?;
                emitted += 1;
                if !live || max_events.is_some_and(|m| emitted >= m) {
                    return Ok(());
                }
            }
            StreamResume::At(_) if !live => {
                sse.comment(&format!("cursor {}", hub.cursor_id(cur)))?;
                return Ok(());
            }
            StreamResume::At(_) => {}
        }

        let started = std::time::Instant::now();
        let mut last_write = started;
        loop {
            if sse.peer_closed() {
                break;
            }
            let step = match wait_limit {
                Some(limit) => {
                    let Some(rem) = limit.checked_sub(started.elapsed()) else {
                        break;
                    };
                    if rem.is_zero() {
                        break;
                    }
                    rem.min(SSE_STEP)
                }
                None => SSE_STEP,
            };
            let (next_cur, next_kinds) = hub.wait_after(seen, step);
            if next_cur > seen {
                seen = next_cur;
                change(sse, seen, &next_kinds)?;
                last_write = std::time::Instant::now();
                emitted += 1;
                if max_events.is_some_and(|m| emitted >= m) {
                    break;
                }
            } else if last_write.elapsed() >= SSE_KEEPALIVE {
                sse.comment("keepalive")?;
                last_write = std::time::Instant::now();
            }
        }
        Ok(())
    });
    cors_headers(&mut resp, origin);
    stamp_latest_snapshot(&mut resp, false);
    request.respond(resp);
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
mod search_service_tests {
    use super::*;

    #[test]
    fn search_rejects_cli_conflicts_before_reading_sources() {
        let _world = crate::test_world("search-conflicts");
        for flags in ["scan=1&kind=context", "scan=1&kind=all", "scan=1&kind=exchange"] {
            let (status, body, _) = search(&Query::parse(&format!("q=needle&{flags}")), &cv_search::Searcher);
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
    fn hub_mode_never_allows_unauthenticated_non_loopback_binds() {
        // Non-hub mode still allows explicit `--insecure-expose`, while hub mode refuses
        // unauthenticated non-loopback binds unconditionally.
        assert!(validate_bind_exposure("0.0.0.0", 7777, None, true, false).is_ok());
        let err = validate_bind_exposure("0.0.0.0", 7777, None, true, true).unwrap_err();
        assert!(err.to_string().contains("NEVER allows unauthenticated"), "{err}");
        assert!(validate_bind_exposure("0.0.0.0", 7777, Some("hub-secret"), false, true).is_ok());
        assert!(validate_bind_exposure("127.0.0.1", 7777, None, false, true).is_ok());
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    fn hub_in(home: &Path) -> (cv_core::env::Scope, StreamHub) {
        let scope = cv_core::env::Env::from_vars([
            ("HOME", home.to_str().unwrap()),
            ("CLUSTERVISION_HOME", home.join("cvhome").to_str().unwrap()),
            ("XDG_CACHE_HOME", home.join("cache").to_str().unwrap()),
        ])
        .enter();
        (scope, StreamHub::new())
    }

    #[test]
    fn a_follower_that_missed_several_changes_learns_every_kind() {
        let home = std::env::temp_dir().join(format!("cv-stream-kinds-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let (_scope, hub) = hub_in(&home);
        let seen = hub.changes_after(0).0;
        // Three changes land before the follower wakes; the last one alone says only `hub`.
        hub.bump(&["tasks"]);
        hub.bump(&["sessions"]);
        hub.bump(&["hub", "tasks"]);
        let (cur, kinds) = hub.changes_after(seen);
        assert_eq!(cur, seen + 3);
        assert_eq!(kinds, ["tasks", "sessions", "hub"]);
        assert_eq!(hub.changes_after(seen + 2).1, ["hub", "tasks"]);
        assert!(hub.changes_after(cur).1.is_empty());

        // Further behind than the remembered history: reload everything.
        for _ in 0..STREAM_HISTORY + 5 {
            hub.bump(&["watch"]);
        }
        assert_eq!(hub.changes_after(seen).1, ["reset"]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cursors_are_scoped_to_the_server_generation() {
        let home = std::env::temp_dir().join(format!("cv-stream-gen-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let (_scope, hub) = hub_in(&home);
        hub.bump(&["tasks"]);
        let cur = hub.changes_after(0).0;
        let id = hub.cursor_id(cur);
        assert_eq!(id, format!("{}:{cur}", hub.gen));
        assert_eq!(hub.resume_point(None, cur), StreamResume::Fresh);
        assert_eq!(hub.resume_point(Some(""), cur), StreamResume::Fresh);
        assert_eq!(hub.resume_point(Some(&id), cur), StreamResume::At(cur));
        assert_eq!(hub.resume_point(Some(&hub.cursor_id(1)), cur), StreamResume::At(1));
        // Another run's generation, a bare offset from it, an offset this run never reached, junk.
        for foreign in [
            "feedface:1",
            "57",
            &hub.cursor_id(cur + 40),
            "x:y:z",
            &format!("{}:", hub.gen),
        ] {
            assert_eq!(hub.resume_point(Some(foreign), cur), StreamResume::Foreign, "{foreign}");
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
