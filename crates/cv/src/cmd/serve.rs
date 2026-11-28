//! `cv serve` — one local HTTP server: the read API at `/api/*` (`cv_core::api::route_json`, plus
//! the two tantivy routes `/api/search` and `/api/stats`), the task inbox page at `/inbox` with
//! the routes its buttons use (`serve_tasks`), and, with `--web <dir>`, the browser dashboard from
//! `/`. Without `--web`, `/` is the inbox. Axum/Hyper handles HTTP and SSE; bounded streaming bridges keep synchronous
//! storage operations on blocking workers.
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
    /// Mint a bound per-producer capability token (`cvp1.<id>.<secret>`) and exit.
    pub mint_producer: Option<String>,
}

impl ServeOpts {
    /// Whether Fleet Hub mode (`/api/hub/*`) is enabled via `$CV_SERVE_HUB`.
    pub(crate) fn hub_enabled(&self) -> bool {
        cv_core::env::var("CV_SERVE_HUB").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }
}

/// Fleet Hub control-plane routes answered when `hub` mode is enabled (`--hub` / `CV_SERVE_HUB=1`).
/// All session listing, hydration, search, events, and touched queries route through the one
/// unified read API (`/api/{sessions,session,search,touched,...}`).
#[allow(dead_code)]
pub(crate) const HUB_ROUTES: &[&str] = &[
    "POST /api/hub/producers",
    "POST /api/hub/enroll",
    "POST /api/hub/upload_pack",
    "PUT /api/hub/packs/{pack_id}",
    "POST /api/hub/ingest_batch",
    "GET /api/hub/status",
    "GET /api/hub/quarantine",
    "POST /api/hub/quarantine",
    "GET /api/hub/snapshot",
];

type ResidentPreparedIndex = std::sync::RwLock<Option<(FileStamp, std::sync::Arc<cv_search::PreparedIndex>)>>;

/// What one request handler needs to know beyond the request itself.
struct Ctx {
    /// The configured bind host, accepted in `Host`/`Origin` checks alongside the loopback names.
    bind_host: String,
    /// Exact TCP port this server instance bound (`80` in socketless unit tests).
    bound_port: u16,
    web_root: Option<PathBuf>,
    auth: Credentials,
    default_who: Option<String>,
    /// Whether Fleet Hub endpoints (`/api/hub/*`) are enabled on this server instance.
    hub: bool,
    searcher: cv_search::Searcher,
    prepared_index: ResidentPreparedIndex,
    stream: std::sync::Arc<StreamHub>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HubWriteGrant {
    AdminBootstrap,
    BoundProducer(String),
}

#[derive(Clone, Copy)]
struct Cors<'a> {
    origin: Option<&'a str>,
    allow_credentials: bool,
}

impl<'a> Cors<'a> {
    const NONE: Self = Self {
        origin: None,
        allow_credentials: false,
    };
}

struct Credentials {
    reader: Option<String>,
    operator_token: Option<String>,
    producer_bootstrap: Option<String>,
    producer_map: std::collections::BTreeMap<String, String>,
    task_writer: Option<String>,
    cookie: String,
    tier: Option<cv_core::disclose::DisclosureTier>,
    scope: Option<PathBuf>,
}

impl Credentials {
    fn load(cli_token: Option<String>) -> Result<Self> {
        let configured = |name| cv_core::env::var(name).ok().filter(|s| !s.is_empty());
        let env_read_token = configured("CV_SERVE_READ_TOKEN");
        let env_serve_token = configured("CV_SERVE_TOKEN");
        let from_operator_flag = cli_token.is_some() || (env_read_token.is_none() && env_serve_token.is_some());
        let reader = cli_token.or(env_read_token).or(env_serve_token);
        let producer_raw = configured("CV_HUB_PRODUCER_TOKEN");
        let explicit_task_writer = configured("CV_TASK_WRITE_TOKEN");

        let mut values = std::collections::HashSet::new();
        for value in [&reader, &producer_raw, &explicit_task_writer].into_iter().flatten() {
            anyhow::ensure!(
                !value.is_empty() && values.insert(value.as_str()),
                "each serve capability needs a distinct nonempty credential"
            );
        }

        let mut producer_map = std::collections::BTreeMap::new();
        let mut producer_bootstrap = None;
        if let Some(raw) = producer_raw {
            if !raw.starts_with("cvp1.") && raw.contains('=') {
                let mut seen_secrets = std::collections::HashSet::new();
                for entry in raw.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        continue;
                    }
                    let Some((prod_id, secret)) = entry.split_once('=') else {
                        anyhow::bail!("invalid CV_HUB_PRODUCER_TOKEN map entry {entry:?}; expected producer_id=token");
                    };
                    let prod_id = prod_id.trim();
                    let secret = secret.trim();
                    anyhow::ensure!(
                        !prod_id.is_empty() && !secret.is_empty() && !prod_id.eq_ignore_ascii_case("local"),
                        "invalid CV_HUB_PRODUCER_TOKEN map entry {entry:?}"
                    );
                    anyhow::ensure!(
                        seen_secrets.insert(secret.to_string()),
                        "each producer in CV_HUB_PRODUCER_TOKEN map needs a distinct nonempty credential"
                    );
                    if let Some(ref r) = reader {
                        anyhow::ensure!(
                            r != secret,
                            "each serve capability needs a distinct nonempty credential"
                        );
                    }
                    if let Some(ref w) = explicit_task_writer {
                        anyhow::ensure!(
                            w != secret,
                            "each serve capability needs a distinct nonempty credential"
                        );
                    }
                    producer_map.insert(prod_id.to_string(), secret.to_string());
                }
                anyhow::ensure!(
                    !producer_map.is_empty(),
                    "CV_HUB_PRODUCER_TOKEN map must define at least one producer_id=token pair"
                );
            } else {
                producer_bootstrap = Some(raw);
            }
        }

        let tier = configured("CV_SERVE_READ_TIER").map(|s| s.parse()).transpose()?;
        let scope = configured("CV_SERVE_READ_SCOPE").map(PathBuf::from);
        anyhow::ensure!(
            scope
                .as_ref()
                .is_none_or(|p| p.is_absolute() && !p.components().any(|c| c == std::path::Component::ParentDir)),
            "reader scope must be an absolute directory"
        );

        let unrestricted = tier.is_none() && scope.is_none();
        let operator_token = if from_operator_flag && unrestricted {
            reader.clone()
        } else {
            None
        };
        let task_writer = explicit_task_writer.or_else(|| operator_token.clone());

        Ok(Self {
            reader,
            operator_token,
            producer_bootstrap,
            producer_map,
            task_writer,
            tier,
            scope,
            cookie: uuid::Uuid::new_v4().to_string(),
        })
    }

    fn configured(&self) -> bool {
        self.reader.is_some()
            || self.producer_bootstrap.is_some()
            || !self.producer_map.is_empty()
            || self.task_writer.is_some()
    }

    fn representative_token(&self, hub: bool) -> Option<&str> {
        if let Some(t) = self
            .reader
            .as_deref()
            .or(self.producer_bootstrap.as_deref())
            .or(self.task_writer.as_deref())
            .or_else(|| self.producer_map.values().next().map(String::as_str))
        {
            return Some(t);
        }
        if hub
            && cv_core::state::hub::HubStore::default_hub()
                .ok()
                .and_then(|h| h.has_active_producer_credentials().ok())
                .unwrap_or(false)
        {
            return Some("cvp1-configured");
        }
        None
    }

    fn verify_hub_bearer(&self, bearer: &str) -> Option<HubWriteGrant> {
        if bearer.starts_with("cvp1.") {
            return cv_core::state::hub::HubStore::default_hub()
                .ok()
                .and_then(|h| h.verify_producer_token(bearer).ok().flatten())
                .map(HubWriteGrant::BoundProducer);
        }
        if !self.producer_map.is_empty() {
            for (prod_id, secret) in &self.producer_map {
                if token_matches(bearer, secret) {
                    return Some(HubWriteGrant::BoundProducer(prod_id.clone()));
                }
            }
            return None;
        }
        if let Some(ref boot) = self.producer_bootstrap {
            return token_matches(bearer, boot).then_some(HubWriteGrant::AdminBootstrap);
        }
        if let Some(ref op) = self.operator_token {
            return token_matches(bearer, op).then_some(HubWriteGrant::AdminBootstrap);
        }
        None
    }

    fn any_bearer_matches(&self, bearer: &str, hub: bool) -> bool {
        if self.reader.as_deref().is_some_and(|s| token_matches(bearer, s)) {
            return true;
        }
        if self.task_writer.as_deref().is_some_and(|s| token_matches(bearer, s)) {
            return true;
        }
        if self
            .producer_bootstrap
            .as_deref()
            .is_some_and(|s| token_matches(bearer, s))
        {
            return true;
        }
        if self.producer_map.values().any(|s| token_matches(bearer, s)) {
            return true;
        }
        hub && bearer.starts_with("cvp1.") && self.verify_hub_bearer(bearer).is_some()
    }

    fn restricted(&self) -> bool {
        self.tier.is_some() || self.scope.is_some()
    }

    fn in_scope(&self, cwd: Option<&Path>) -> bool {
        self.scope.as_ref().is_none_or(|scope| {
            cwd.is_some_and(|p| p.starts_with(scope) && !p.components().any(|c| c == std::path::Component::ParentDir))
        })
    }

    fn projected_read(&self, parts: &[&str], q: &cv_core::api::Query) -> Result<(u16, Value)> {
        use cv_core::disclose::{disclose, DisclosureTier};
        let tier = self.tier.unwrap_or(DisclosureTier::Full);
        match parts {
            ["api", "health"] => Ok(cv_core::api::route_json(&["api", "health"], "")),
            ["api", "sessions"] => {
                let tier = if tier == DisclosureTier::Presence {
                    tier
                } else {
                    DisclosureTier::Activity
                };
                let mut rows = Vec::new();
                let mut references = cv_core::sessions();
                references.sort_by_cached_key(cv_core::SessionRef::session_selector);
                for r in references.into_iter().filter(|r| self.in_scope(r.cwd.as_deref())) {
                    let mut header = cv_core::Session::new(r.session_selector(), r.harness);
                    header.created_at = r.created_at;
                    header.updated_at = r.updated_at;
                    rows.push(disclose(tier, &header)?);
                }
                Ok((200, serde_json::to_value(rows)?))
            }
            ["api", "session", harness, id] => {
                let missing = || (404, json!({"error":"session not found"}));
                let Some(harness) = cv_core::Harness::parse(harness) else {
                    return Ok(missing());
                };
                let Some((r, adapter)) = cv_core::find(id, Some(harness))? else {
                    return Ok(missing());
                };
                if !self.in_scope(r.cwd.as_deref()) {
                    return Ok(missing());
                }
                if matches!(tier, DisclosureTier::Presence | DisclosureTier::Activity) {
                    let mut session = cv_core::Session::new(r.session_selector(), r.harness);
                    session.created_at = r.created_at;
                    session.updated_at = r.updated_at;
                    return Ok((200, serde_json::to_value(disclose(tier, &session)?)?));
                }
                let mut session = adapter.parse(&r)?;
                session.id = r.session_selector();
                if !self.in_scope(session.cwd.as_deref()) {
                    return Ok(missing());
                }
                Ok((200, serde_json::to_value(disclose(tier, &session)?)?))
            }
            ["api", "session", harness, id, "messages"]
                if matches!(tier, DisclosureTier::Conversation | DisclosureTier::Full) =>
            {
                let missing = || (404, json!({"error":"session not found"}));
                let Some(harness) = cv_core::Harness::parse(harness) else {
                    return Ok(missing());
                };
                let Some((r, adapter)) = cv_core::find(id, Some(harness))? else {
                    return Ok(missing());
                };
                if !self.in_scope(r.cwd.as_deref()) {
                    return Ok(missing());
                }
                let mut session = adapter.parse(&r)?;
                session.id = r.session_selector();
                if !self.in_scope(session.cwd.as_deref()) {
                    return Ok(missing());
                }
                let projected = serde_json::to_value(disclose(tier, &session)?)?;
                let msgs = projected["messages"].as_array().cloned().unwrap_or_default();
                let total = msgs.len();
                let start = match q.unsigned("start") {
                    Ok(v) => v.unwrap_or(0).min(total),
                    Err(e) => return Ok((400, json!({"error": e}))),
                };
                let end = match q.unsigned("end") {
                    Ok(v) => v.unwrap_or(total).min(total),
                    Err(e) => return Ok((400, json!({"error": e}))),
                };
                let slice = if start <= end { &msgs[start..end] } else { &[][..] };
                Ok((
                    200,
                    json!({
                        "id": session.id,
                        "harness": harness.as_str(),
                        "total": total,
                        "start": start,
                        "end": end.max(start),
                        "messages": slice,
                    }),
                ))
            }
            _ => Ok((403, json!({"error":"route outside reader grant"}))),
        }
    }
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
    if let Some(producer_id) = opts.mint_producer.as_deref() {
        anyhow::ensure!(hub, "--mint-producer requires --hub");
        let hub_store = cv_core::state::hub::HubStore::default_hub()?;
        let token = hub_store.issue_producer_token(producer_id)?;
        println!("{token}");
        return Ok(());
    }
    let auth = Credentials::load(opts.token)?;
    validate_bind_exposure(
        &opts.host,
        opts.port,
        auth.representative_token(hub),
        opts.insecure_expose,
        hub,
    )?;
    run(&opts.host, opts.port, opts.web, auth, opts.who, opts.open, hub)
}

/// Run the HTTP server until the process is killed. Never returns under normal operation.
fn run(
    host: &str,
    port: u16,
    web_root: Option<PathBuf>,
    auth: Credentials,
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

    let _recorder = super::start_recorder()?;
    let mut limits = crate::http::Limits::default();
    if hub {
        limits.request = std::time::Duration::from_secs(120);
    }
    let ctx = Ctx {
        bind_host: host.to_string(),
        bound_port: port,
        web_root,
        auth,
        default_who: who,
        hub,
        searcher: cv_search::Searcher,
        prepared_index: std::sync::RwLock::new(None),
        stream: std::sync::Arc::new(StreamHub::new()),
    };
    crate::http::serve(listener, limits, move |request| handle(request, &ctx))
}

/// Whether `request` originates from the exact `(host, bound_port)` authority this server bound,
/// with `Sec-Fetch-Site: same-origin` whenever the browser attached `Sec-Fetch-Site`.
///
/// RFC 6265 cookies ignore port on `localhost`, so cross-port callers (`http://localhost:5173`,
/// `Sec-Fetch-Site: same-site`/`cross-site`) must authenticate via `Authorization: Bearer` and are
/// never granted `Cookie: cv_reader` or `Access-Control-Allow-Credentials: true`.
fn is_same_origin_authority(
    raw_host: Option<&str>,
    raw_origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    bind_host: &str,
    bound_port: u16,
) -> bool {
    let Some(host_hdr) = raw_host else {
        return false;
    };
    if !host_allowed(Some(host_hdr), bind_host) {
        return false;
    }
    let Some((req_host, req_port)) = authority_host_port(host_hdr, 80) else {
        return false;
    };
    if req_port != bound_port {
        return false;
    }
    if let Some(sfs) = sec_fetch_site {
        if !sfs.trim().eq_ignore_ascii_case("same-origin") {
            return false;
        }
    }
    if let Some(orig) = raw_origin {
        let Some((default_port, authority)) = orig
            .strip_prefix("http://")
            .map(|r| (80u16, r))
            .or_else(|| orig.strip_prefix("https://").map(|r| (443u16, r)))
        else {
            return false;
        };
        let Some((orig_host, orig_port)) = authority_host_port(authority, default_port) else {
            return false;
        };
        if !orig_host.eq_ignore_ascii_case(req_host) || orig_port != bound_port {
            return false;
        }
    }
    true
}

/// Handle one request: any error becomes a JSON error response.
fn handle(request: &mut Request<'_>, ctx: &Ctx) {
    let raw_host = header_value(request, "Host");
    // DNS-rebinding guard: a hostile page can point its own domain at 127.0.0.1 and fetch us
    // same-origin, but it can't forge the Host header — reject anything that isn't our own name.
    // Non-browser API/Hub callers presenting a valid `Authorization: Bearer <token>` header (never
    // `?token=`, HTML pages, or `OPTIONS` preflights) may use reverse-proxy or container aliases.
    let authorization = header_value(request, "Authorization");
    let bearer_token = authorization.as_deref().and_then(|v| v.strip_prefix("Bearer "));
    let host_ok = host_allowed(raw_host.as_deref(), &ctx.bind_host)
        || (request.method() != "OPTIONS"
            && is_api_request_url(request.url())
            && raw_host.as_deref().and_then(host_name).is_some()
            && bearer_token.is_some_and(|b| ctx.auth.any_bearer_matches(b, ctx.hub)));
    if !host_ok {
        request.respond(json_response(403, &json!({"error": "forbidden host"}), Cors::NONE, &[]));
        return;
    }
    // Echoed on API responses only when the Origin is on the local allow-list (or same-origin on
    // an explicit non-loopback bind); never `*`. `Access-Control-Allow-Credentials` is restricted
    // strictly to exact same-origin `(host, bound_port)` + `Sec-Fetch-Site: same-origin`.
    let raw_origin = header_value(request, "Origin");
    let sec_fetch_site = header_value(request, "Sec-Fetch-Site");
    let same_origin = is_same_origin_authority(
        raw_host.as_deref(),
        raw_origin.as_deref(),
        sec_fetch_site.as_deref(),
        &ctx.bind_host,
        ctx.bound_port,
    );
    let origin = allowed_origin(raw_origin.as_deref(), raw_host.as_deref(), &ctx.bind_host);
    let cors = Cors {
        origin: origin.as_deref(),
        allow_credentials: origin.is_some() && same_origin,
    };

    let method = request.method().to_string();
    let raw_url = request.url().to_string();

    // CORS preflight: a 204, CORS-tagged only for allowed origins. Unauthenticated by design —
    // browsers strip credentials from preflights, and it discloses nothing.
    if method == "OPTIONS" {
        request.respond(no_content(cors));
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
                [
                    "api",
                    "hub",
                    "producers" | "enroll" | "upload_pack" | "ingest_batch" | "quarantine"
                ]
            ))
            || (put && matches!(parts.as_slice(), ["api", "hub", "packs", _])));

    // GET reads anything; POST is an inbox write (`/api/task/<id>/<verb>`) or (when hub mode is
    // enabled) a Fleet Hub replication write (`/api/hub/*`, plus `PUT /api/hub/packs/<id>`).
    if !(method == "GET" || is_task_post || is_hub_write) {
        request.respond(json_response(405, &json!({"error": "method not allowed"}), cors, &[]));
        return;
    }

    if !is_api {
        // The inbox page: at `/inbox` always, and at `/` when no dashboard is being hosted.
        let page = matches!(parts.as_slice(), ["inbox"] | ["inbox.html"])
            || (ctx.web_root.is_none() && matches!(parts.as_slice(), [] | ["index.html"]));
        if page {
            request.respond(inbox_response());
            return;
        }
        if let Some(root) = &ctx.web_root {
            request.respond(static_file(root, &segments));
            return;
        }
    }

    let q = cv_core::api::Query::parse(query);
    if q.raw("token").is_some() {
        request.respond(json_response(
            401,
            &json!({"error":"URL credentials are unsupported"}),
            cors,
            &[],
        ));
        return;
    }
    let auth = &ctx.auth;
    let configured = auth.configured()
        || (is_hub_write
            && cv_core::state::hub::HubStore::default_hub()
                .ok()
                .and_then(|h| h.has_active_producer_credentials().ok())
                .unwrap_or(false));
    let mut hub_grant: Option<HubWriteGrant> = None;
    let header_ok = if is_hub_write {
        if let Some(provided) = bearer_token {
            hub_grant = auth.verify_hub_bearer(provided);
            hub_grant.is_some()
        } else {
            false
        }
    } else if is_task_post {
        auth.task_writer
            .as_ref()
            .is_some_and(|secret| bearer_token.is_some_and(|provided| token_matches(provided, secret)))
    } else {
        auth.reader
            .as_ref()
            .is_some_and(|secret| bearer_token.is_some_and(|provided| token_matches(provided, secret)))
    };
    let cookie_ok = !is_hub_write
        && !is_task_post
        && auth.reader.is_some()
        && authorization.is_none()
        && same_origin
        && header_value(request, "Cookie").is_some_and(|cookies| {
            cookies.split(';').any(|pair| {
                pair.trim()
                    .strip_prefix("cv_reader=")
                    .is_some_and(|provided| token_matches(provided, &auth.cookie))
            })
        });
    let local = !configured && authorization.is_none() && is_loopback_host(&ctx.bind_host);
    if local && is_hub_write {
        hub_grant = Some(HubWriteGrant::AdminBootstrap);
    }
    if !(header_ok || cookie_ok || local || (!configured && authorization.is_none() && !is_hub_write && !is_task_post))
    {
        request.respond(json_response(
            401,
            &json!({"error":"missing or invalid credential for this route"}),
            cors,
            &[],
        ));
        return;
    }
    if parts.as_slice() == ["api", "auth"] && method == "GET" {
        if !header_ok || !same_origin {
            request.respond(json_response(
                401,
                &json!({"error":"same-origin reader bearer required"}),
                Cors::NONE,
                &[],
            ));
        } else {
            let mut response = Response::bytes(200, br#"{"ok":true}"#.to_vec());
            cors_headers(&mut response, cors);
            response.add_header("Content-Type", "application/json");
            response.add_header("Cache-Control", "no-store");
            response.add_header(
                "Set-Cookie",
                &format!("cv_reader={}; Path=/api/; HttpOnly; SameSite=Strict", auth.cookie),
            );
            request.respond(response);
        }
        return;
    }
    if !is_hub_write && !is_task_post && auth.restricted() {
        if parts.as_slice() == ["api", "stream"] {
            if q.raw("lanes").is_some() || q.raw("files").is_some() {
                let mut response = Response::bytes(
                    403,
                    serde_json::to_vec(&json!({"error":"route outside reader grant"})).unwrap_or_default(),
                );
                response.add_header("Content-Type", "application/json");
                response.add_header("Cache-Control", "no-store");
                cors_headers(&mut response, cors);
                request.respond(response);
            } else {
                serve_stream_with_options(request, &q, cors, &ctx.stream, true);
            }
            return;
        }
        let (status, body) = auth
            .projected_read(&parts, &q)
            .unwrap_or_else(|_| (500, json!({"error":"session read failed"})));
        // Snapshot identifiers reflect above-tier activity. Omit them from a projected read.
        let mut response = Response::bytes(status, serde_json::to_vec(&body).unwrap_or_default());
        response.add_header("Content-Type", "application/json");
        response.add_header("Cache-Control", "no-store");
        cors_headers(&mut response, cors);
        request.respond(response);
        return;
    }

    if is_hub_write {
        if raw_origin.is_some() && origin.is_none() {
            request.respond(json_response(403, &json!({"error": "forbidden origin"}), cors, &[]));
            return;
        }
        let grant = hub_grant.unwrap_or(HubWriteGrant::AdminBootstrap);
        let resp = handle_hub_write(request, &parts, cors, ctx, &grant);
        request.respond(resp);
        return;
    }

    if matches!(parts.as_slice(), ["api", "hub", ..]) {
        if !ctx.hub {
            request.respond(json_response(404, &json!({"error": "not found"}), cors, &[]));
            return;
        }
        let (status, body, extra) = handle_hub_get(&parts, &q);
        request.respond(json_response(status, &body, cors, &extra));
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
                cors,
                &[],
            ));
            return;
        }
        // Capped: a note is a paragraph, not an upload.
        let mut buf = String::new();
        const BODY_LIMIT: u64 = 1 << 20;
        if request.by_ref().take(BODY_LIMIT + 1).read_to_string(&mut buf).is_err() {
            request.respond(json_response(400, &json!({"error": "unreadable body"}), cors, &[]));
            return;
        }
        if buf.len() as u64 > BODY_LIMIT {
            request.respond(json_response(
                413,
                &json!({"error": "body exceeds 1 MiB limit"}),
                cors,
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
                        cors,
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
            request.respond(json_response(status, &v, cors, &[("Cache-Control", "no-store".into())]));
        }
        Some(serve_tasks::Answer::Ndjson(lines)) => {
            let mut resp = text_response(200, "application/x-ndjson; charset=utf-8", lines.into_bytes());
            cors_headers(&mut resp, cors);
            resp.add_header("Cache-Control", "no-store");
            stamp_latest_snapshot(&mut resp, false);
            request.respond(resp);
        }
        None if post => {
            request.respond(json_response(404, &json!({"error": "no such route"}), cors, &[]));
        }
        None => {
            if parts.as_slice() == ["api", "stream"] {
                serve_stream_with_options(request, &q, cors, &ctx.stream, false);
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
                            cors_headers(&mut response, cors);
                            stamp_latest_snapshot(&mut response, false);
                            request.respond(response);
                        }
                        Err(error) => {
                            request.respond(json_response(500, &json!({"error":error.to_string()}), cors, &[]));
                        }
                    },
                    Err((status, error)) => {
                        request.respond(json_response(status, &error, cors, &[]));
                    }
                }
                return;
            }
            match cv_core::api::route_staged(&parts, query) {
                Ok(Some(stage)) => {
                    match stage.body.reader() {
                        Ok(reader) => {
                            let response = staged_response(&stage, reader, cors);
                            request.respond(response);
                        }
                        Err(error) => {
                            request.respond(json_response(
                                500,
                                &json!({"error":format!("staged response failed: {error}")}),
                                cors,
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
                        cors,
                        &[],
                    ));
                    return;
                }
                Ok(None) => {}
            }
            let (status, body, mut extra) = route(false, &parts, query, &q, &ctx.searcher, &ctx.prepared_index);
            if status < 400 && !extra.iter().any(|(k, _)| *k == "X-Cv-Snapshot") {
                let as_of_spec = q.get("as_of").or_else(|| q.get("snapshot"));
                if let Ok(hub) = cv_core::state::hub::HubStore::default_hub() {
                    if let Ok(Some(gen_rec)) = hub.resolve_generation(as_of_spec.as_deref()) {
                        extra.push(("X-Cv-Snapshot", header_safe(&gen_rec.publication_id)));
                        extra.push(("X-Cv-Hub-Generation", gen_rec.generation.to_string()));
                    }
                }
            }
            request.respond(json_response(status, &body, cors, &extra));
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
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)))
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

/// Whether the `Host` header names this machine.
///
/// On loopback binds, only loopback names (`127.0.0.1`, `::1`, `localhost`, or `bind_host`) pass.
/// On non-loopback binds (`0.0.0.0`, `::`, LAN IP), literal IP addresses (immune to DNS rebinding),
/// local mDNS/Tailscale/LAN names (`.local`, `.ts.net`, `.lan`, ".internal", `.home.arpa`), the
/// explicit `bind_host`, and this machine's exact OS hostname or bare short hostname pass, while
/// arbitrary external domains (including `<short>.<attacker-domain>`) are rejected `403` to prevent
/// LAN DNS rebinding.
fn host_allowed(host_header: Option<&str>, bind_host: &str) -> bool {
    let Some(name) = host_header.and_then(host_name) else {
        return false;
    };
    if is_local_name(name, bind_host) {
        return true;
    }
    if is_loopback_host(bind_host) {
        return false;
    }
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    if [".local", ".ts.net", ".lan", ".internal", ".home.arpa"]
        .iter()
        .any(|sfx| lower.ends_with(sfx))
    {
        return true;
    }
    let os_host_raw = cv_core::state::default_host_label();
    let os_host = os_host_raw.trim_end_matches('.');
    if !os_host.is_empty() && !os_host.eq_ignore_ascii_case("localhost") {
        if name.eq_ignore_ascii_case(os_host) {
            return true;
        }
        if let Some(short) = os_host.split('.').next().filter(|s| !s.is_empty()) {
            if !short.eq_ignore_ascii_case("localhost") && name.eq_ignore_ascii_case(short) {
                return true;
            }
        }
    }
    false
}

fn is_api_request_url(raw_url: &str) -> bool {
    let path = raw_url
        .split_once('?')
        .map_or(raw_url, |(p, _)| p)
        .trim_end_matches('/');
    path == "/api" || path.starts_with("/api/")
}

#[allow(dead_code)]
fn host_allowed_for_request(
    host_header: Option<&str>,
    bind_host: &str,
    method: &str,
    raw_url: &str,
    auth_header: Option<&str>,
    expected_token: Option<&str>,
) -> bool {
    if host_allowed(host_header, bind_host) {
        return true;
    }
    if method == "OPTIONS" || !is_api_request_url(raw_url) {
        return false;
    }
    if host_header.and_then(host_name).is_none() {
        return false;
    }
    let Some(expected) = expected_token else {
        return false;
    };
    auth_header
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|provided| token_matches(provided, expected))
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
    fn route(_post, parts, query: &str, params: &Query, searcher: &cv_search::Searcher, prepared_index: &ResidentPreparedIndex) -> Reply {
        // Routes that carry extra response headers answer here; everything else is header-free.
        GET ["api", "search"] => search(params, searcher, prepared_index),
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
fn obtain_prepared_index(
    cache: &ResidentPreparedIndex,
    dir: &Path,
) -> Result<std::sync::Arc<cv_search::PreparedIndex>> {
    let stamp = file_stamp(&dir.join("meta.json"));
    if let Ok(guard) = cache.read() {
        if let Some((cached_stamp, ref reader)) = *guard {
            if cached_stamp == stamp {
                return Ok(reader.clone());
            }
        }
    }
    let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_stamp, ref reader)) = *guard {
        if cached_stamp == stamp {
            return Ok(reader.clone());
        }
    }
    let opened = std::sync::Arc::new(cv_search::PreparedIndex::open(dir)?);
    *guard = Some((stamp, opened.clone()));
    Ok(opened)
}

fn search(params: &Query, searcher: &cv_search::Searcher, prepared_index: &ResidentPreparedIndex) -> Reply {
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
    let exact = flag("exact");
    if scan && exact {
        return bad(400, "scan=1 and exact=1 conflict; exact=1 uses the trigram index");
    }
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
    let index_only = flag("index_only") || cv_core::env::frozen() || params.get("as_of").is_some();
    let tantivy_dir = cv_search::default_tantivy_dir();
    let result = if exact {
        searcher.search_exact(&q, filter, false, limit, live_bytes)
    } else if !scan && index_only && tantivy_dir.join("meta.json").is_file() {
        obtain_prepared_index(prepared_index, &tantivy_dir)
            .and_then(|reader| reader.search(&q, limit, filter, None))
            .map(|ans| cv_search::Found {
                hits: ans.hits,
                source: Source::Index,
                notes: Vec::new(),
                total: ans.total,
            })
    } else {
        searcher.search(&q, filter, limit, source, live_bytes)
    };
    let found = match result {
        Ok(found) => found,
        Err(e) if scan => return bad(500, &format!("live scan failed: {e:#}")),
        Err(e) if cv_search::is_invalid_text_query(&e) => return bad(400, &format!("invalid search query: {e:#}")),
        Err(e) if e.is::<cv_search::BadLiveBytes>() => return bad(500, &format!("{e:#}")),
        // The index is authoritative: a missing or unusable index is an error naming the cause and
        // the fix, never a silent scan of every transcript.
        Err(e) if exact => return bad(503, &format!("{e:#}")),
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
fn inbox_response() -> Response<'static> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let html = serve_tasks::PAGE.replacen("\n<script>\n", &format!("\n<script nonce=\"{nonce}\">\n"), 1);
    let csp = format!("frame-ancestors 'none'; script-src 'self' 'nonce-{nonce}'; object-src 'none'; base-uri 'self'");
    let mut resp = Response::bytes(200, html.into_bytes());
    resp.add_header("Content-Type", "text/html; charset=utf-8");
    resp.add_header("X-Content-Type-Options", "nosniff");
    resp.add_header("X-Frame-Options", "DENY");
    resp.add_header("Content-Security-Policy", &csp);
    resp
}

fn text_response(status: u16, content_type: &str, body: Vec<u8>) -> Response<'static> {
    let mut resp = Response::bytes(status, body);
    resp.add_header("Content-Type", content_type);
    resp.add_header("X-Content-Type-Options", "nosniff");
    if content_type.starts_with("text/html") {
        resp.add_header("X-Frame-Options", "DENY");
        resp.add_header(
            "Content-Security-Policy",
            "frame-ancestors 'none'; script-src 'self' 'wasm-unsafe-eval'; object-src 'none'; base-uri 'self'",
        );
    }
    resp
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

fn latest_hub_generation_header() -> Option<(u64, String)> {
    cv_core::state::hub::HubStore::default_hub()
        .ok()
        .and_then(|hub| hub.latest_generation_fast().ok().flatten())
}

fn stamp_latest_snapshot(response: &mut Response<'_>, already_stamped: bool) {
    if !already_stamped {
        if let Some((gen, pub_id)) = latest_hub_generation_header() {
            response.add_header("X-Cv-Snapshot", &header_safe(&pub_id));
            response.add_header("X-Cv-Hub-Generation", &gen.to_string());
        }
    }
}

/// Build a JSON response, CORS-tagged for `origin` when one was allowed, plus any route-specific
/// headers (see [`EXPOSED_HEADERS`] — a header a cross-origin dashboard cannot read is not a
/// channel).
fn json_response(status: u16, body: &Value, cors: Cors<'_>, extra: &[(&'static str, String)]) -> Response<'static> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut resp = Response::bytes(status, data);
    cors_headers(&mut resp, cors);
    let mut has_snapshot_hdr = false;
    for (field, value) in extra {
        if field.eq_ignore_ascii_case("X-Cv-Snapshot") {
            has_snapshot_hdr = true;
        }
        resp.add_header(field, value);
    }
    if status < 400 {
        stamp_latest_snapshot(&mut resp, has_snapshot_hdr);
    }
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
    cors: Cors<'_>,
) -> std::result::Result<Value, Response<'static>> {
    if media_type_of(request) != "application/json" {
        return Err(json_response(
            403,
            &json!({"error": "a write must be application/json from a local origin"}),
            cors,
            &[],
        ));
    }
    let mut buf = String::new();
    if request.by_ref().take(limit + 1).read_to_string(&mut buf).is_err() {
        return Err(json_response(400, &json!({"error": "unreadable body"}), cors, &[]));
    }
    if buf.len() as u64 > limit {
        return Err(json_response(
            413,
            &json!({"error": format!("body exceeds {limit_label} limit")}),
            cors,
            &[],
        ));
    }
    if buf.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str::<Value>(&buf)
        .map_err(|e| json_response(400, &json!({"error": format!("body is not JSON: {e}")}), cors, &[]))
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
        HubError::Forbidden(msg) => (403, json!({"error": msg}), Vec::new()),
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

fn handle_hub_write(
    request: &mut Request<'_>,
    parts: &[&str],
    cors: Cors<'_>,
    ctx: &Ctx,
    grant: &HubWriteGrant,
) -> Response<'static> {
    use cv_core::state::hub::{
        EnrollRequest, EnrollResponse, HubError, HubStore, IngestBatchRequest, QuarantineResolution,
        PACK_STREAM_CONTENT_TYPE,
    };

    let hub = match HubStore::default_hub() {
        Ok(h) => h,
        Err(e) => {
            let (status, body, extra) = hub_error_reply(e);
            return json_response(status, &body, cors, &extra);
        }
    };

    let bound_producer = match grant {
        HubWriteGrant::BoundProducer(id) => Some(id.as_str()),
        HubWriteGrant::AdminBootstrap => None,
    };

    match parts {
        ["api", "hub", "producers"] => {
            if bound_producer.is_some() {
                return json_response(
                    403,
                    &json!({"error": "bound producer credentials cannot mint producer tokens"}),
                    cors,
                    &[],
                );
            }
            let val = match read_json_body(request, HUB_ENROLL_LIMIT, "1 MiB", cors) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let Some(producer_id) = val.get("producer_id").and_then(Value::as_str) else {
                return json_response(400, &json!({"error": "missing producer_id"}), cors, &[]);
            };
            if let Some(host_label) = val.get("host_label").and_then(Value::as_str) {
                let meta = val.get("metadata").cloned().unwrap_or_else(|| json!({}));
                if let Err(e) = hub.enroll_producer(producer_id, host_label, &meta) {
                    let (status, body, extra) = hub_error_reply(e);
                    return json_response(status, &body, cors, &extra);
                }
            }
            let scopes: Vec<String> = val
                .get("scopes")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match hub.issue_producer_token_with_scopes(producer_id, &scopes) {
                Ok((token, meta)) => {
                    ctx.stream.bump(&["hub"]);
                    json_response(
                        200,
                        &json!({
                            "producer_id": meta.producer_id,
                            "token": token,
                            "scopes": meta.scopes,
                            "created_at": meta.created_at,
                        }),
                        cors,
                        &[("Cache-Control", "no-store".into())],
                    )
                }
                Err(e) => {
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, cors, &extra)
                }
            }
        }
        ["api", "hub", "quarantine"] => {
            if bound_producer.is_some() {
                return json_response(
                    403,
                    &json!({"error": "bound producer credentials cannot resolve quarantine entries"}),
                    cors,
                    &[],
                );
            }
            let val = match read_json_body(request, HUB_ENROLL_LIMIT, "1 MiB", cors) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let Some(quarantine_id) = val.get("quarantine_id").and_then(Value::as_i64) else {
                return json_response(400, &json!({"error": "missing integer quarantine_id"}), cors, &[]);
            };
            let Some(res_raw) = val.get("resolution").and_then(Value::as_str) else {
                return json_response(400, &json!({"error": "missing resolution"}), cors, &[]);
            };
            let Some(resolution) = QuarantineResolution::parse(res_raw) else {
                return json_response(
                    400,
                    &json!({"error": "resolution must be accept_existing or accept_incoming"}),
                    cors,
                    &[],
                );
            };
            match hub.resolve_quarantine(quarantine_id, resolution) {
                Ok(resolved) => {
                    ctx.stream.bump(&["hub", "sessions"]);
                    let out = serde_json::to_value(resolved).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                    json_response(200, &out, cors, &[("Cache-Control", "no-store".into())])
                }
                Err(e) => {
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, cors, &extra)
                }
            }
        }
        ["api", "hub", "enroll"] => {
            let val = match read_json_body(request, HUB_ENROLL_LIMIT, "1 MiB", cors) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let req: EnrollRequest = match serde_json::from_value(val) {
                Ok(r) => r,
                Err(e) => {
                    return json_response(
                        400,
                        &json!({"error": format!("invalid enroll payload: {e}")}),
                        cors,
                        &[],
                    );
                }
            };
            if req.producer_id.trim().is_empty() {
                return json_response(400, &json!({"error": "producer_id must not be empty"}), cors, &[]);
            }
            if let Some(bound) = bound_producer {
                if req.producer_id != bound {
                    return json_response(
                        403,
                        &json!({
                            "error": format!("credential bound to producer `{bound}` cannot enroll `{}`", req.producer_id)
                        }),
                        cors,
                        &[],
                    );
                }
            }
            match hub.enroll_producer(&req.producer_id, &req.host_label, &req.metadata) {
                Ok(producer) => {
                    ctx.stream.bump(&["hub"]);
                    let out = serde_json::to_value(EnrollResponse { producer })
                        .unwrap_or_else(|_| json!({"error": "serialize failed"}));
                    json_response(200, &out, cors, &[("Cache-Control", "no-store".into())])
                }
                Err(e) => {
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, cors, &extra)
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
                            cors,
                            &[],
                        );
                    }
                    (Some(u), _) => u.to_string(),
                    (None, Some(h)) if !h.trim().is_empty() => h.trim().to_string(),
                    (None, _) => {
                        return json_response(400, &json!({"error": "missing X-Cv-Pack-Id header"}), cors, &[]);
                    }
                };
                let expected_digest = header_value(request, "X-Cv-Pack-Digest");
                let producer_id = header_value(request, "X-Cv-Producer-Id");
                let Some(idx_len_raw) = header_value(request, "X-Cv-Pack-Idx-Len") else {
                    return json_response(400, &json!({"error": "missing X-Cv-Pack-Idx-Len header"}), cors, &[]);
                };
                let Ok(idx_len) = idx_len_raw.trim().parse::<usize>() else {
                    return json_response(400, &json!({"error": "invalid X-Cv-Pack-Idx-Len header"}), cors, &[]);
                };
                if idx_len > HUB_PACK_IDX_LIMIT {
                    return json_response(413, &json!({"error": "pack index exceeds 16 MiB limit"}), cors, &[]);
                }

                let mut idx_bytes = vec![0u8; idx_len];
                if request.read_exact(&mut idx_bytes).is_err() {
                    return json_response(400, &json!({"error": "truncated pack index stream"}), cors, &[]);
                }

                let mut bounded = BoundedReader {
                    inner: request,
                    limit: HUB_PACK_ZST_LIMIT,
                    read_so_far: 0,
                    exceeded: false,
                };
                let result = hub.upload_pack_stream_for_producer(
                    &pack_id,
                    producer_id.as_deref(),
                    bound_producer,
                    expected_digest.as_deref(),
                    &idx_bytes,
                    &mut bounded,
                );
                if bounded.exceeded {
                    return json_response(413, &json!({"error": "pack body exceeds 256 MiB limit"}), cors, &[]);
                }
                match result {
                    Ok(resp) => {
                        ctx.stream.bump(&["hub"]);
                        let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                        json_response(200, &out, cors, &[("Cache-Control", "no-store".into())])
                    }
                    Err(e) => {
                        let (status, body, extra) = hub_error_reply(e);
                        json_response(status, &body, cors, &extra)
                    }
                }
            } else if ct == "application/json" {
                let val = match read_json_body(request, HUB_PACK_JSON_LIMIT, "64 MiB", cors) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                let body_pack_id = val.get("pack_id").and_then(Value::as_str);
                let pack_id = match (url_pack_id, body_pack_id) {
                    (Some(u), Some(b)) if u != b => {
                        return json_response(
                            400,
                            &json!({"error": format!("URL pack_id {u:?} disagrees with body pack_id {b:?}")}),
                            cors,
                            &[],
                        );
                    }
                    (Some(u), _) => u.to_string(),
                    (None, Some(b)) if !b.trim().is_empty() => b.trim().to_string(),
                    (None, _) => {
                        return json_response(400, &json!({"error": "missing pack_id"}), cors, &[]);
                    }
                };
                let producer_id = val.get("producer_id").and_then(Value::as_str);
                let expected_digest = val.get("blake3_digest").and_then(Value::as_str);
                let Some(idx_val) = val.get("idx") else {
                    return json_response(400, &json!({"error": "missing idx field"}), cors, &[]);
                };
                let idx_bytes = match idx_val {
                    Value::String(s) => s.as_bytes().to_vec(),
                    other => match serde_json::to_vec(other) {
                        Ok(b) => b,
                        Err(e) => return json_response(400, &json!({"error": e.to_string()}), cors, &[]),
                    },
                };
                let Some(hex_str) = val.get("pack_zst_hex").and_then(Value::as_str) else {
                    return json_response(400, &json!({"error": "missing pack_zst_hex field"}), cors, &[]);
                };
                let zst_bytes = match decode_hex(hex_str) {
                    Ok(b) => b,
                    Err(e) => {
                        return json_response(400, &json!({"error": format!("invalid pack_zst_hex: {e}")}), cors, &[])
                    }
                };
                match hub.upload_pack_bytes_for_producer(
                    &pack_id,
                    producer_id,
                    bound_producer,
                    expected_digest,
                    &idx_bytes,
                    &zst_bytes,
                ) {
                    Ok(resp) => {
                        ctx.stream.bump(&["hub"]);
                        let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                        json_response(200, &out, cors, &[("Cache-Control", "no-store".into())])
                    }
                    Err(e) => {
                        let (status, body, extra) = hub_error_reply(e);
                        json_response(status, &body, cors, &extra)
                    }
                }
            } else {
                json_response(
                    403,
                    &json!({
                        "error": "pack upload must be application/vnd.cv.pack+octet-stream, application/octet-stream, or application/json"
                    }),
                    cors,
                    &[],
                )
            }
        }
        ["api", "hub", "ingest_batch"] => {
            let val = match read_json_body(request, HUB_INGEST_BATCH_LIMIT, "32 MiB", cors) {
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
                            cors,
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
                            cors,
                            &[],
                        );
                    }
                }
            };

            match hub.ingest_batch_for_producer(&batch_req, bound_producer) {
                Ok(resp) => {
                    ctx.stream.bump(&["hub", "sessions"]);
                    let mut extra = vec![("Cache-Control", "no-store".to_string())];
                    if let Some(ref pub_id) = resp.publication_id {
                        extra.push(("X-Cv-Snapshot", header_safe(pub_id)));
                    }
                    if let Some(gen) = resp.hub_generation {
                        extra.push(("X-Cv-Hub-Generation", gen.to_string()));
                    }
                    let out = serde_json::to_value(resp).unwrap_or_else(|_| json!({"error": "serialize failed"}));
                    json_response(200, &out, cors, &extra)
                }
                Err(e) => {
                    if matches!(e, HubError::EventConflict(_)) {
                        ctx.stream.bump(&["hub"]);
                    }
                    let (status, body, extra) = hub_error_reply(e);
                    json_response(status, &body, cors, &extra)
                }
            }
        }
        _ => json_response(404, &json!({"error": "not found"}), cors, &[]),
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
        ["api", "hub", "quarantine"] => match hub.quarantine_summary() {
            Ok(summary) => match serde_json::to_value(summary) {
                Ok(val) => (200, val, vec![("Cache-Control", "no-store".to_string())]),
                Err(e) => (500, json!({"error": e.to_string()}), Vec::new()),
            },
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
    cors: Cors<'_>,
) -> Response<'a> {
    let mut response = Response::stream(stage.status, reader, stage.bytes);
    response.add_header("Content-Type", "application/json");
    cors_headers(&mut response, cors);
    stamp_latest_snapshot(&mut response, false);
    response
}

/// A 204 No Content, CORS-tagged for `origin` when one was allowed — for `OPTIONS` preflight.
fn no_content(cors: Cors<'_>) -> Response<'static> {
    let mut resp = Response::empty(204);
    cors_headers(&mut resp, cors);
    resp
}

/// Response headers a cross-origin caller is allowed to READ. A browser hides every non-safelisted
/// response header from `fetch` unless it is named here, so `/api/search`'s "this answer is
/// degraded" signal and hit count would be invisible to a dashboard on a dev server without it.
const EXPOSED_HEADERS: &str =
    "X-Cv-Search-Source, X-Cv-Search-Note, X-Cv-Search-Total, Content-Disposition, X-Cv-Snapshot, X-Cv-Hub-Generation";

/// CORS headers echoing the one allowed `origin` — an unlisted origin gets none (the browser then
/// refuses to share the response with it). `Access-Control-Allow-Credentials` is emitted ONLY when
/// `cors.allow_credentials` holds (exact `(host, bound_port)` + `Sec-Fetch-Site: same-origin`).
fn cors_headers(response: &mut Response<'_>, cors: Cors<'_>) {
    response.add_header("Vary", "Origin");
    if let Some(o) = cors.origin {
        response.add_header("Access-Control-Allow-Origin", o);
        if cors.allow_credentials {
            response.add_header("Access-Control-Allow-Credentials", "true");
        }
        response.add_header("Access-Control-Allow-Methods", "GET, POST, PUT, OPTIONS");
        response.add_header("Access-Control-Allow-Headers", "Authorization, Content-Type");
        response.add_header("Access-Control-Expose-Headers", EXPOSED_HEADERS);
    }
}

// --- live SSE change stream (`/api/stream`) --------------------------------

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
    wake: tokio::sync::Notify,
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
            wake: tokio::sync::Notify::new(),
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
        self.wake.notify_waiters();
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
            // A transcript that moved can change its lanes and its file timeline.
            kinds.extend(["sessions", "lanes", "files"]);
        }
        if next.watch != guard.stamp.watch {
            guard.stamp.watch = next.watch;
            kinds.push("watch");
        }
        if next.hub_state != guard.stamp.hub_state {
            let wal_len = next.hub_state.1.len;
            guard.stamp.hub_state = next.hub_state;
            kinds.push("hub");
            // Hook lane runs and stops live in state.db: the lane witnesses may have moved.
            if !kinds.contains(&"lanes") {
                kinds.push("lanes");
            }
            if wal_len >= (16 << 20) {
                if let Some(home) = cv_core::env::Env::current().cv_home() {
                    let _ = cv_core::state::StateStore::at_dir(home).checkpoint_wal(false);
                }
            }
        }
        if !kinds.is_empty() {
            guard.record(kinds);
            let out = guard.cursor;
            drop(guard);
            self.wake.notify_waiters();
            return out;
        }
        guard.cursor
    }

    /// The current offset and every kind that changed after `seen`.
    fn changes_after(&self, seen: u64) -> (u64, Vec<&'static str>) {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (guard.cursor, guard.kinds_after(seen))
    }
}

/// What a follower asked to have pushed with each change, beyond the kinds: `lanes=<session>`
/// (compact lane rows, most urgent first) and `files=<harness>/<id>` (the timeline's size and
/// latest touch). Absent, change events carry only the cursor and kinds.
#[derive(Clone, Default)]
struct StreamFollow {
    lanes: Option<String>,
    files: Option<(String, String)>,
}

impl StreamFollow {
    fn parse(q: &Query) -> Self {
        let files = q.get("files").and_then(|spec| {
            let (h, id) = spec.split_once('/').or_else(|| spec.split_once(':'))?;
            (!h.is_empty() && !id.is_empty()).then(|| (h.to_string(), id.to_string()))
        });
        Self {
            lanes: q.get("lanes").filter(|s| !s.trim().is_empty()),
            files,
        }
    }

    fn wants(&self) -> bool {
        self.lanes.is_some() || self.files.is_some()
    }
}

/// The compact lane rows `/api/stream?lanes=` pushes: the `/api/lanes` counts plus, per lane,
/// only what an attention board draws.
fn compact_lanes(session: &str) -> Value {
    let (status, body) = serve_tasks::lanes_report(session);
    if status != 200 {
        return json!({"error": body["error"]});
    }
    let rows: Vec<Value> = body["lanes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|l| {
            let mut row = serde_json::Map::new();
            for key in [
                "agent_id",
                "session_id",
                "description",
                "status",
                "attention",
                "stranded",
                "pending_ask",
                "witness_divergence",
                "last_turn_at",
                "last_tool",
            ] {
                if let Some(v) = l.get(key).filter(|v| !v.is_null()) {
                    row.insert(key.into(), v.clone());
                }
            }
            Value::Object(row)
        })
        .collect();
    json!({"session": body["session"], "counts": body["counts"], "lanes": rows})
}

/// What `/api/stream?files=` pushes: how many touches, and the latest one.
fn compact_files(harness: &str, id: &str) -> Value {
    let (status, body) = cv_core::api::route_json(&["api", "session", harness, id, "files"], "");
    if status != 200 {
        return json!({"error": body["error"]});
    }
    let rows = body.as_array().map(Vec::as_slice).unwrap_or_default();
    json!({"harness": harness, "id": id, "touches": rows.len(), "last": rows.last()})
}

/// One change event's data: the cursor and kinds, plus whatever the follower asked to have pushed
/// for kinds that touch it (`ready` and `reset` touch everything).
fn change_data(id: &str, kinds: &[&str], follow: &StreamFollow) -> String {
    let mut v = json!({"cursor": id, "kinds": kinds});
    let touches = |k: &str| kinds.iter().any(|x| *x == k || *x == "ready" || *x == "reset");
    if let Some(session) = follow.lanes.as_deref().filter(|_| touches("lanes")) {
        v["lanes"] = compact_lanes(session);
    }
    if let Some((harness, sid)) = follow.files.as_ref().filter(|_| touches("files")) {
        v["files"] = compact_files(harness, sid);
    }
    v.to_string()
}

fn serve_stream_with_options(
    request: &mut Request<'_>,
    q: &Query,
    cors: Cors<'_>,
    hub: &std::sync::Arc<StreamHub>,
    restricted_reader: bool,
) {
    use axum::response::sse::Event;
    let last_event_id = header_value(request, "Last-Event-ID");
    let after = q.get("after").or_else(|| last_event_id.clone());
    let max_events = q.get("max_events").and_then(|s| s.parse::<usize>().ok());
    let wait_limit = q
        .get("wait_ms")
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    let live = q.flag("follow")
        || last_event_id.is_some()
        || header_value(request, "Accept").is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));
    let follow = if restricted_reader {
        StreamFollow::default()
    } else {
        StreamFollow::parse(q)
    };
    let filter_kinds = move |kinds: &[&'static str]| -> Vec<&'static str> {
        if !restricted_reader {
            return kinds.to_vec();
        }
        kinds
            .iter()
            .copied()
            .filter(|k| matches!(*k, "ready" | "reset" | "sessions"))
            .collect()
    };
    let hub = hub.clone();
    let env = cv_core::env::Env::scoped();
    let started = std::time::Instant::now();
    let stream = futures_util::stream::unfold(
        (true, 0u64, 0usize, false, started),
        move |(init, mut seen, mut emitted, mut done, mut last_write)| {
            let hub = hub.clone();
            let env = env.clone();
            let follow = follow.clone();
            let after = after.clone();
            async move {
                if init {
                    let init_hub = hub.clone();
                    let init_env = env.clone();
                    let init_follow = follow.clone();
                    let (first, cur, init_emitted, init_done) = tokio::task::spawn_blocking(move || {
                        let _env = init_env.map(cv_core::env::Env::reenter);
                        let cur = init_hub.refresh();
                        let change = |offset, kinds: &[&'static str]| {
                            let id = init_hub.cursor_id(offset);
                            let allowed = filter_kinds(kinds);
                            Event::default()
                                .id(&id)
                                .event("change")
                                .data(change_data(&id, &allowed, &init_follow))
                        };
                        let (first, init_emitted, init_done) = match init_hub.resume_point(after.as_deref(), cur) {
                            StreamResume::Fresh => (Some(change(cur, &["ready"])), 0, !live || max_events == Some(0)),
                            StreamResume::Foreign => (
                                Some(change(cur, &["reset"])),
                                1,
                                !live || max_events.is_some_and(|n| n <= 1),
                            ),
                            StreamResume::At(prev) if cur > prev => {
                                let allowed = filter_kinds(&init_hub.changes_after(prev).1);
                                if allowed.is_empty() {
                                    if !live {
                                        (
                                            Some(
                                                Event::default().comment(format!("cursor {}", init_hub.cursor_id(cur))),
                                            ),
                                            0,
                                            true,
                                        )
                                    } else {
                                        (None, 0, false)
                                    }
                                } else {
                                    (
                                        Some(change(cur, &allowed)),
                                        1,
                                        !live || max_events.is_some_and(|n| n <= 1),
                                    )
                                }
                            }
                            StreamResume::At(_) if !live => (
                                Some(Event::default().comment(format!("cursor {}", init_hub.cursor_id(cur)))),
                                0,
                                true,
                            ),
                            StreamResume::At(_) => (None, 0, false),
                        };
                        (first, cur, init_emitted, init_done)
                    })
                    .await
                    .ok()?;
                    seen = cur;
                    emitted = init_emitted;
                    done = init_done;
                    if let Some(first) = first {
                        last_write = std::time::Instant::now();
                        return Some((Ok::<_, std::io::Error>(first), (false, seen, emitted, done, last_write)));
                    }
                }
                loop {
                    if done {
                        return None;
                    }
                    let step = match wait_limit {
                        Some(limit) => limit.checked_sub(started.elapsed())?.min(SSE_STEP),
                        None => SSE_STEP,
                    };
                    if step.is_zero() {
                        return None;
                    }
                    tokio::select! { _ = hub.wake.notified() => {}, _ = tokio::time::sleep(step) => {} }
                    // Disk polling is shared/throttled in StreamHub and happens on blocking workers;
                    // idle followers themselves hold no thread or blocking-worker permit.
                    let refresh = hub.clone();
                    let scoped = env.clone();
                    tokio::task::spawn_blocking(move || {
                        let _env = scoped.map(cv_core::env::Env::reenter);
                        refresh.refresh()
                    })
                    .await
                    .ok()?;
                    let (next, raw_kinds) = hub.changes_after(seen);
                    let kinds = filter_kinds(&raw_kinds);
                    let event = if next > seen && !kinds.is_empty() {
                        seen = next;
                        emitted += 1;
                        done = max_events.is_some_and(|n| emitted >= n);
                        let id = hub.cursor_id(seen);
                        // Pushed rows read transcripts and the task log: build them off the runtime.
                        let data = if follow.wants() {
                            let (id, follow) = (id.clone(), follow.clone());
                            let env = env.clone();
                            tokio::task::spawn_blocking(move || {
                                let _env = env.map(cv_core::env::Env::reenter);
                                change_data(&id, &kinds, &follow)
                            })
                            .await
                            .ok()?
                        } else {
                            change_data(&id, &kinds, &follow)
                        };
                        Some(Event::default().id(&id).event("change").data(data))
                    } else {
                        if next > seen {
                            seen = next;
                        }
                        if last_write.elapsed() >= SSE_KEEPALIVE {
                            Some(Event::default().comment("keepalive"))
                        } else {
                            None
                        }
                    };
                    if let Some(event) = event {
                        last_write = std::time::Instant::now();
                        return Some((Ok(event), (false, seen, emitted, done, last_write)));
                    }
                }
            }
        },
    );
    let mut response = Response::events(stream);
    cors_headers(&mut response, cors);
    if !restricted_reader {
        stamp_latest_snapshot(&mut response, false);
    }
    request.respond(response);
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
                        staged_response(&stage, reader, Cors::NONE)
                            .write_body_to(std::io::sink())
                            .unwrap();
                    } else {
                        let (status, value) = cv_core::api::route_json(&parts, "start=0&end=1");
                        json_response(status, &value, Cors::NONE, &[])
                            .write_body_to(std::io::sink())
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

    /// #tooth Authenticate each request at the dispatch boundary, including native browser downloads.
    #[test]
    fn serve_credential_matrix_rejects_url_credentials_and_cross_capability_requests() {
        let world = crate::test_world("serve-capabilities");
        world.scope.set("CV_HUB_PRODUCER_TOKEN", "producer");
        world.scope.set("CV_TASK_WRITE_TOKEN", "writer");
        world.scope.set("CV_SERVE_READ_TIER", "presence");
        let ctx = Ctx {
            bind_host: "127.0.0.1".into(),
            bound_port: 80,
            web_root: None,
            auth: Credentials::load(Some("reader".into())).unwrap(),
            default_who: None,
            hub: true,
            searcher: cv_search::Searcher,
            prepared_index: std::sync::RwLock::new(None),
            stream: std::sync::Arc::new(StreamHub::new()),
        };
        let request = |method: &str, path: &str, headers: &[(&str, &str)]| {
            crate::http::test_request(method, path, headers, |req| handle(req, &ctx))
        };
        for (method, path, credential) in [
            ("GET", "/api/sessions?token=reader", "reader"),
            ("GET", "/api/sessions", "producer"),
            ("GET", "/api/sessions", "writer"),
            ("POST", "/api/hub/enroll", "reader"),
            ("POST", "/api/hub/enroll", "writer"),
            ("POST", "/api/task/x/note", "reader"),
            ("POST", "/api/task/x/note", "producer"),
        ] {
            assert_eq!(
                request(method, path, &[("Authorization", &format!("Bearer {credential}"))]).status(),
                401
            );
        }
        let response = request("GET", "/api/auth", &[("Authorization", "Bearer reader")]);
        assert_eq!(response.status(), 200);
        let mut cookie = response.headers()["Set-Cookie"].to_str().unwrap().split(';');
        let cookie_value = cookie.next().unwrap();
        assert!(cookie.any(|s| s.trim() == "HttpOnly"));
        let cookie_header = [("Cookie", cookie_value)];
        assert_eq!(request("GET", "/api/health", &cookie_header).status(), 200);
        assert_eq!(request("GET", "/api/sessions", &cookie_header).status(), 200);
        assert_eq!(request("GET", "/api/search?q=secret", &cookie_header).status(), 403);
        assert_eq!(request("POST", "/api/task/x/note", &cookie_header).status(), 401);
        // Cross-port localhost Origin or non-same-origin Sec-Fetch-Site must reject Cookie auth.
        assert_eq!(
            request(
                "GET",
                "/api/sessions",
                &[("Cookie", cookie_value), ("Origin", "http://localhost:5173")]
            )
            .status(),
            401
        );
        assert_eq!(
            request(
                "GET",
                "/api/sessions",
                &[("Cookie", cookie_value), ("Sec-Fetch-Site", "same-site")]
            )
            .status(),
            401
        );
    }

    #[test]
    fn search_rejects_cli_conflicts_before_reading_sources() {
        let _world = crate::test_world("search-conflicts");
        let prepared = std::sync::RwLock::new(None);
        for flags in ["scan=1&kind=context", "scan=1&kind=all", "scan=1&kind=exchange"] {
            let (status, body, _) = search(
                &Query::parse(&format!("q=needle&{flags}")),
                &cv_search::Searcher,
                &prepared,
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
    fn hub_mode_never_allows_unauthenticated_non_loopback_binds() {
        // Non-hub mode still allows explicit `--insecure-expose`, while hub mode refuses
        // unauthenticated non-loopback binds unconditionally.
        assert!(validate_bind_exposure("0.0.0.0", 7777, None, true, false).is_ok());
        let err = validate_bind_exposure("0.0.0.0", 7777, None, true, true).unwrap_err();
        assert!(err.to_string().contains("NEVER allows unauthenticated"), "{err}");
        assert!(validate_bind_exposure("0.0.0.0", 7777, Some("hub-secret"), false, true).is_ok());
        assert!(validate_bind_exposure("127.0.0.1", 7777, None, false, true).is_ok());
    }

    #[test]
    fn host_guard_allows_underscore_lan_and_bearer_api_while_blocking_rebinding() {
        // Underscore container/LAN names and private LAN suffixes pass on non-loopback binds.
        assert!(host_allowed(Some("cv_hub.lan:7777"), "0.0.0.0"));
        assert!(host_allowed(Some("node_1.internal:7777"), "0.0.0.0"));
        assert!(host_allowed(Some("nas.home.arpa:7777"), "0.0.0.0"));
        assert!(host_allowed(Some("cv_hub:7777"), "cv_hub"));

        let os_host_raw = cv_core::state::default_host_label();
        let os_host = os_host_raw.trim_end_matches('.');
        if !os_host.is_empty() && !os_host.eq_ignore_ascii_case("localhost") {
            assert!(host_allowed(Some(&format!("{os_host}:7777")), "0.0.0.0"));
        }
        if let Some(short) = os_host
            .split('.')
            .next()
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("localhost"))
        {
            assert!(host_allowed(Some(&format!("{short}:7777")), "0.0.0.0"));
            assert!(host_allowed(Some(&format!("{short}.local:7777")), "0.0.0.0"));
            assert!(
                !host_allowed(Some(&format!("{short}.evil-rebind.example.com:7777")), "0.0.0.0"),
                "must reject <short>.<attacker-domain> DNS-rebinding Host headers"
            );
        }

        // Unauthenticated foreign domains stay 403 on both loopback and 0.0.0.0.
        assert!(!host_allowed(Some("evil.example.com:7777"), "0.0.0.0"));
        assert!(!host_allowed(Some("cv_hub.lan:7777"), "127.0.0.1"));

        // Valid `Authorization: Bearer` header on `/api/*` bypasses Host check for reverse proxies /
        // Docker service names (`cv_hub:7777`, `hub.example.com`), while unauthenticated requests,
        // wrong tokens, HTML pages, OPTIONS preflights, and `?token=` query strings remain blocked.
        assert!(host_allowed_for_request(
            Some("cv_hub:7777"),
            "0.0.0.0",
            "POST",
            "/api/hub/enroll",
            Some("Bearer s3cr3t"),
            Some("s3cr3t"),
        ));
        assert!(host_allowed_for_request(
            Some("hub.proxy.example.com"),
            "127.0.0.1",
            "GET",
            "/api/sessions?limit=5",
            Some("Bearer s3cr3t"),
            Some("s3cr3t"),
        ));
        // Reject unauthenticated, wrong Bearer, query-string `?token=`, HTML page, OPTIONS, bad Host.
        assert!(!host_allowed_for_request(
            Some("evil.example.com"),
            "0.0.0.0",
            "GET",
            "/api/sessions",
            None,
            Some("s3cr3t"),
        ));
        assert!(!host_allowed_for_request(
            Some("evil.example.com"),
            "0.0.0.0",
            "GET",
            "/api/sessions",
            Some("Bearer wrong"),
            Some("s3cr3t"),
        ));
        assert!(!host_allowed_for_request(
            Some("evil.example.com"),
            "0.0.0.0",
            "GET",
            "/api/stream?token=s3cr3t",
            None,
            Some("s3cr3t"),
        ));
        assert!(!host_allowed_for_request(
            Some("evil.example.com"),
            "0.0.0.0",
            "GET",
            "/inbox",
            Some("Bearer s3cr3t"),
            Some("s3cr3t"),
        ));
        assert!(!host_allowed_for_request(
            Some("evil.example.com"),
            "0.0.0.0",
            "OPTIONS",
            "/api/hub/enroll",
            Some("Bearer s3cr3t"),
            Some("s3cr3t"),
        ));
        assert!(!host_allowed_for_request(
            Some("bad/host"),
            "0.0.0.0",
            "GET",
            "/api/sessions",
            Some("Bearer s3cr3t"),
            Some("s3cr3t"),
        ));
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
