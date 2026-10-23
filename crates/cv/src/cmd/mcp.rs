//! # `cv mcp` — clustervision as an MCP server
//!
//! A stdio [Model Context Protocol](https://modelcontextprotocol.io) server that lets a *running*
//! coding agent (Claude Code, Codex, Gemini, …) search and read the sessions of **other** agents —
//! within the same project, back through time, and across harnesses. "Let agents read other agents'
//! minds." It is a thin front-end over the [`cv_core`] library.
//!
//! ## Registering with a host
//!
//! Point your agent at `cv mcp`. For Claude Code:
//!
//! ```text
//! claude mcp add clustervision -- /absolute/path/to/cv mcp
//! ```
//!
//! It speaks line-delimited JSON-RPC 2.0 over stdin/stdout. **stdout is the protocol channel**, so
//! all diagnostics go to stderr.
//!
//! ## Tools exposed
//!
//! Two families, and the split is the whole design (docs/INTERFACE-V2.md §6):
//!
//! 1. **Generated from the CLI** — one tool per `cv` command in the Read / Reshape / Export /
//!    System groups (`ls`, `show`, `cat`, `search`, `events`, `tools`, `workflow`, `prune`,
//!    `export`, `pack`, `doctor`, …) and per `cv task` subcommand (`task_open`, `task_claim`, …),
//!    built at startup from the in-process clap tree (the same `commands_json`
//!    `cv schema --commands --json` prints), so tool names, flags, harness lists
//!    and output shapes cannot drift from the binary. A call runs this executable as
//!    `cv <command> <args…> --json`. See [`super::mcp_tools`].
//! 2. **Hand-written, MCP-only** — the two tools with no CLI equivalent, because they block or
//!    hold a cursor:
//!    - `await_omen(regex, …)` — block until another agent's session emits a matching message.
//!    - `observe_stream(cwd_contains?, harness?, since_cursor?, max_messages?, char_cap?)` — the
//!      non-blocking sibling: drains the *newly-appended* messages since an opaque cursor and
//!      returns immediately with a fresh cursor, so a senior agent can poll a junior's activity on
//!      its own cadence. Everything appended by default; the caller's bounds are stated in the
//!      output. Read-only.
//!
//! `show` returns what `cv show` does: the whole transcript unless the caller windows it.

use super::mcp_tools::CliTools;
use anyhow::Context as _;
use cv_core::scan::{ceil_char, floor_char};
use cv_core::watch::{Cursor, Filter, Poll, Readers, Watcher};
use cv_core::Harness;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The CLI-generated tools, built once at startup from the clap tree.
static CLI: OnceLock<Option<CliTools>> = OnceLock::new();

fn cli() -> Option<&'static CliTools> {
    CLI.get().and_then(Option::as_ref)
}

/// A request stops on either explicit cancellation or connection shutdown.
#[derive(Clone, Copy)]
pub(crate) struct Cancellation<'a> {
    connection: &'a AtomicBool,
    request: Option<&'a AtomicBool>,
}
impl<'a> Cancellation<'a> {
    pub(crate) fn connection(connection: &'a AtomicBool) -> Self {
        Self {
            connection,
            request: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn request(connection: &'a AtomicBool, request: &'a AtomicBool) -> Self {
        Self {
            connection,
            request: Some(request),
        }
    }
    pub(crate) fn is_cancelled(self) -> bool {
        self.connection.load(Ordering::Relaxed) || self.request.is_some_and(|request| request.load(Ordering::Relaxed))
    }
    pub(crate) fn check(self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.connection.load(Ordering::Relaxed), "MCP client disconnected");
        anyhow::ensure!(
            !self.request.is_some_and(|request| request.load(Ordering::Relaxed)),
            "MCP request cancelled"
        );
        Ok(())
    }
}
#[derive(Clone, PartialEq, Eq, Hash)]
enum RequestKey {
    String(String),
    Number(String),
}
impl RequestKey {
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::String(value) => Some(Self::String(value.clone())),
            Value::Number(value) => Some(Self::Number(
                value
                    .as_i64()
                    .map(|value| value.to_string())
                    .or_else(|| value.as_u64().map(|value| value.to_string()))
                    .unwrap_or_else(|| value.as_f64().unwrap().to_string()),
            )),
            _ => None,
        }
    }
}
type Requests = Arc<Mutex<std::collections::HashMap<RequestKey, Arc<AtomicBool>>>>;
struct RequestRegistration {
    requests: Requests,
    key: Option<RequestKey>,
    signal: Arc<AtomicBool>,
}
impl RequestRegistration {
    fn register(requests: &Requests, id: Option<&Value>) -> Result<Self, ()> {
        let key = id.and_then(RequestKey::of);
        let signal = Arc::new(AtomicBool::new(false));
        if let Some(key) = &key {
            let mut entries = requests.lock().unwrap();
            if entries.contains_key(key) {
                return Err(());
            }
            entries.insert(key.clone(), signal.clone());
        }
        Ok(Self {
            requests: requests.clone(),
            key,
            signal,
        })
    }
}
impl Drop for RequestRegistration {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            self.requests.lock().unwrap().remove(key);
        }
    }
}

const PROTOCOL_VERSION: &str = "2025-06-18";

/// A request handler's stack. Tool bodies parse whole transcripts; give them main's room.
const HANDLER_STACK: usize = 8 * 1024 * 1024;

// Permits cover execution AND completed response ownership. Blocking polls cannot occupy
// ordinary tool slots. Control requests run on the reader and backpressure on the response queue.
const ORDINARY_CALLS: usize = 4;
const BLOCKING_POLLS: usize = 2;
const RESPONSE_QUEUE: usize = 8;

struct Admission {
    used: Mutex<usize>,
    available: Condvar,
    limit: usize,
}
impl Admission {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: Mutex::new(0),
            available: Condvar::new(),
            limit,
        })
    }
    fn acquire(self: &Arc<Self>, wait: bool, cancelled: &AtomicBool) -> Option<Permit> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if *used < self.limit {
                *used += 1;
                return Some(Permit(Arc::new(Lease(self.clone()))));
            }
            if !wait {
                return None;
            }
            used = self
                .available
                .wait_timeout(used, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
struct Permit(Arc<Lease>);
impl Clone for Permit {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
struct Lease(Arc<Admission>);
impl Drop for Lease {
    fn drop(&mut self) {
        let mut used = self.0.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= 1;
        self.0.available.notify_one();
    }
}
struct Pending {
    response: Response,
    _permit: Option<Permit>,
}
impl Pending {
    fn control(response: Response) -> Self {
        Self {
            response,
            _permit: None,
        }
    }
}

fn enqueue_tool_response(tx: &mpsc::SyncSender<Pending>, response: Response, permit: Permit, cancelled: &AtomicBool) {
    let mut pending = Pending {
        response,
        _permit: Some(permit),
    };
    loop {
        // Flush pre-disconnect frames, but do not keep a handler blocked behind a dead peer.
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        match tx.try_send(pending) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => return,
            Err(mpsc::TrySendError::Full(frame)) => pending = frame,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Serve MCP over stdio until stdin closes.
pub(crate) fn cmd_mcp(tree: clap::Command) -> anyhow::Result<()> {
    // Unlike a pipeline command, MCP owns children that must be cancelled/reaped when its
    // output peer disappears. Let the writer observe EPIPE instead of terminating on SIGPIPE.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let on_terminal = if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        "; stdin is a terminal: one JSON-RPC request per line, Ctrl-D stops it (a host registers it with \
         `claude mcp add clustervision -- /abs/path/to/cv mcp`)"
    } else {
        ""
    };
    eprintln!("cv mcp: clustervision MCP server starting (stdio JSON-RPC){on_terminal}");
    let generated = match CliTools::from_owned_tree(tree) {
        Ok(c) => {
            eprintln!("cv mcp: generated {} tools from the CLI", c.commands().len());
            Some(c)
        }
        Err(e) => {
            eprintln!("cv mcp: no CLI-generated tools ({e:#}); serving the MCP-only tools");
            None
        }
    };
    let _ = CLI.set(generated);

    // Every response funnels through one writer thread: requests are handled *concurrently* (a
    // parked long-poll like await_omen must not head-of-line-block an `ls` issued after it), but
    // stdout is a shared byte stream, so a single consumer serializes the frames. Responses may
    // complete out of order — JSON-RPC ties a reply to its request by `id`, so that's fine.
    let cancelled = Arc::new(AtomicBool::new(false));
    let writer_cancelled = cancelled.clone();
    let (resp_tx, resp_rx) = mpsc::sync_channel::<Pending>(RESPONSE_QUEUE);
    let requests: Requests = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let ordinary = Admission::new(ORDINARY_CALLS);
    let polls = Admission::new(BLOCKING_POLLS);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut stdout = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
        for msg in resp_rx {
            if let Err(e) = write_message(&mut stdout, &msg.response) {
                eprintln!("cv mcp: stdout write failed: {e:#}");
                break;
            }
            // The frame (and the tool call that built it) is done: hand its freed heap back.
            drop(msg);
            release_freed_heap();
        }
        writer_cancelled.store(true, Ordering::Relaxed);
        let _ = done_tx.send(());
    });

    let mut handlers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut reader = std::io::stdin().lock();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Reuse ordinary frames, but do not retain the largest request for the server's
        // lifetime. The completed frame and its borrowed lossy view have left scope here.
        if buf.capacity() > 64 * 1024 {
            buf = Vec::new();
        } else {
            buf.clear();
        }
        // Read raw frames so malformed UTF8 produces a per-frame JSON parse error rather
        // than terminating the server or silently rewriting tool arguments.
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("cv mcp: stdin read error, shutting down: {e:#}");
                break;
            }
        };
        if n == 0 {
            break; // EOF
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        let req: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                // Parse error: id unknown, reply with null id per JSON-RPC.
                let _ = resp_tx.send(Pending::control(Response::Json(error_response(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                ))));
                continue;
            }
        };
        let (method, params, id) = match request_frame(req) {
            Ok(frame) => frame,
            Err(error) => {
                if resp_tx.send(Pending::control(Response::Json(error))).is_err() {
                    break;
                }
                continue;
            }
        };

        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if method == "notifications/cancelled" {
            if let Some(key) = params
                .as_ref()
                .and_then(|params| params.get("requestId"))
                .and_then(RequestKey::of)
            {
                if let Some(request) = requests.lock().unwrap().get(&key) {
                    request.store(true, Ordering::Relaxed);
                }
            }
            continue;
        }
        // Protocol/control bursts retain their replies, without starting handler threads.
        if method != "tools/call" {
            if let Some(response) = handle(&method, params, id, &cancelled) {
                if resp_tx.send(Pending::control(response)).is_err() {
                    break;
                }
            }
            continue;
        }
        let blocking = params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .and_then(NativeTool::find)
            .is_some_and(NativeTool::blocking);
        let admission = if blocking { &polls } else { &ordinary };
        // Side-effect notifications wait: silently dropping them would change the protocol.
        let Some(permit) = admission.acquire(id.is_none(), &cancelled) else {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            if let Some(id) = id {
                let response = Response::Json(error_response(
                    id,
                    -32000,
                    "MCP tool capacity is busy; retry after an outstanding call completes",
                ));
                if resp_tx.send(Pending::control(response)).is_err() {
                    break;
                }
            }
            continue;
        };
        let registration = match RequestRegistration::register(&requests, id.as_ref()) {
            Ok(registration) => registration,
            Err(()) => {
                let response = Response::Json(error_response(
                    id.unwrap_or(Value::Null),
                    -32600,
                    "request ID is already outstanding",
                ));
                if resp_tx.send(Pending::control(response)).is_err() {
                    break;
                }
                continue;
            }
        };
        // Completed thread handles do not accumulate with request history.
        let mut i = 0;
        while i < handlers.len() {
            if handlers[i].is_finished() {
                let _ = handlers.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
        // A released execution lease may precede the OS's finished-thread flag by a few
        // instructions. Do not accumulate even those epilogues across a sustained burst.
        while handlers.len() >= ORDINARY_CALLS + BLOCKING_POLLS {
            if let Some(i) = handlers.iter().position(std::thread::JoinHandle::is_finished) {
                let _ = handlers.swap_remove(i).join();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let failure_id = id.clone();
        let tx = resp_tx.clone();
        let cancelled = cancelled.clone();
        let env = cv_core::env::Env::scoped();
        let spawned = std::thread::Builder::new().stack_size(HANDLER_STACK).spawn(move || {
            let _env = env.map(cv_core::env::Env::reenter);
            let _execution = permit.clone();
            let panic_id = id.clone();
            let token = Cancellation {
                connection: &cancelled,
                request: Some(&registration.signal),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_cancelled(&method, params, id, token)
            }));
            let response = match result {
                Ok(response) => response,
                Err(_) => panic_id
                    .map(|id| Response::Json(error_response(id, -32603, "internal error: the request panicked"))),
            };
            // Retire the ID before publication so a client can reuse it as soon as
            // it receives the completed response.
            drop(registration);
            if let Some(response) = response {
                enqueue_tool_response(&tx, response, permit, &cancelled);
            }
        });
        if let Ok(handler) = spawned {
            handlers.push(handler);
        } else if let Err(e) = spawned {
            eprintln!("cv mcp: could not start a request thread: {e}");
            if let Some(id) = failure_id {
                let response = Response::Json(error_response(
                    id,
                    -32603,
                    &format!("could not start a request thread: {e}"),
                ));
                if resp_tx.send(Pending::control(response)).is_err() {
                    break;
                }
            }
        }
    }

    // Flush responses already queued (or about to land) before exiting; don't hang around for the
    // full length of an in-flight long-poll — the host that would read its answer is gone.
    drop(resp_tx);
    let finished = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    if !finished {
        cancelled.store(true, Ordering::Relaxed);
    }
    // Child kill/reap and request-owned read scopes must finish before main exits.
    for handler in handlers {
        let _ = handler.join();
    }
    if !finished {
        let _ = done_rx.recv_timeout(Duration::from_secs(1));
    }
    Ok(())
}

/// Validate the protocol envelope before admission or execution, then transfer ownership
/// of the payload rather than cloning large tool argument trees.
fn request_frame(req: Value) -> Result<(String, Option<Value>, Option<Value>), Value> {
    let id = req
        .get("id")
        .filter(|id| id.is_null() || id.is_string() || id.is_number());
    let valid = req.is_object()
        && req.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && req.get("method").is_some_and(Value::is_string)
        && req
            .get("id")
            .is_none_or(|value| value.is_null() || value.is_string() || value.is_number())
        && req
            .get("params")
            .is_none_or(|params| params.is_object() || params.is_array());
    if !valid {
        return Err(error_response(
            id.cloned().unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
        ));
    }
    let Value::Object(mut fields) = req else { unreachable!() };
    let Some(Value::String(method)) = fields.remove("method") else {
        unreachable!()
    };
    Ok((method, fields.remove("params"), fields.remove("id")))
}

fn tool_call_params(params: Option<Value>) -> anyhow::Result<(String, Value)> {
    let Some(Value::Object(mut fields)) = params else {
        anyhow::bail!(InvalidParams("tools/call params must be an object".into()));
    };
    let Some(Value::String(name)) = fields.remove("name") else {
        anyhow::bail!(InvalidParams("tools/call name must be a string".into()));
    };
    let args = fields.remove("arguments").unwrap_or_else(|| json!({}));
    if !args.is_object() {
        anyhow::bail!(InvalidParams("tools/call arguments must be an object".into()));
    }
    Ok((name, args))
}

fn handle(method: &str, params: Option<Value>, id: Option<Value>, cancelled: &AtomicBool) -> Option<Response> {
    handle_cancelled(method, params, id, Cancellation::connection(cancelled))
}

fn handle_cancelled(
    method: &str,
    params: Option<Value>,
    id: Option<Value>,
    cancelled: Cancellation<'_>,
) -> Option<Response> {
    let _reads = cv_core::source::read_scope();
    // A request without an `id` is a JSON-RPC *notification*: the method still runs (a
    // notification tools/call may have side effects worth performing), but per spec the server
    // MUST NOT reply — not even with `id: null`.
    let is_notification = id.is_none();
    let id = id.unwrap_or(Value::Null);

    let response = match method {
        "initialize" => Some(ok_response(id, initialize_result())),
        "ping" => Some(ok_response(id, json!({}))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_list() }))),
        "tools/call" => {
            // A panicking tool costs its own reply, not the server. Parameter validation
            // shares that error path and never launches a command for malformed arguments.
            let result = std::panic::catch_unwind(move || {
                let (name, args) = tool_call_params(params)?;
                call_tool(&name, &args, cancelled)
            });

            match result {
                Ok(Ok(ToolOutput::Text(text))) => Some(ok_response(id, tool_text_result(&text, false))),
                Ok(Ok(ToolOutput::Command(output))) => {
                    return (!is_notification).then_some(Response::Command { id, output });
                }
                // A malformed *argument* (wrong type, negative count, …) is the caller's protocol
                // mistake: JSON-RPC -32602 Invalid params, not a tool-execution failure.
                Ok(Err(e)) if e.downcast_ref::<InvalidParams>().is_some() => {
                    Some(error_response(id, -32602, &format!("{e}")))
                }
                Ok(Err(e)) => {
                    // Tool execution errors are reported via the result's isError flag, not a
                    // protocol-level error, per the MCP spec.
                    Some(ok_response(id, tool_text_result(&format!("error: {e:#}"), true)))
                }
                Err(_) => Some(error_response(id, -32603, "internal error: the tool panicked")),
            }
        }
        _ => Some(error_response(id, -32601, &format!("Method not found: {method}"))),
    };
    if is_notification {
        None
    } else {
        response.map(Response::Json)
    }
}

// ---------------------------------------------------------------------------
// MCP boilerplate
// ---------------------------------------------------------------------------

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "clustervision",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions(),
    })
}

/// The server blurb a host shows the model. It names the tools that actually exist (they are
/// generated from `cv`, so this text is the only hand-written place they could go stale) and
/// enumerates harnesses from `Harness::ALL` rather than a frozen list of five.
fn instructions() -> String {
    let harnesses: Vec<&str> = Harness::ALL.iter().map(|h| h.as_str()).collect();
    format!(
        "clustervision lets you read OTHER agents' sessions across harnesses and time. \
         Start with `ls` (add `cwd` to see what happened — or is happening — in the current project, \
         `query` for the filter calculus) and `search` for a past conversation by content; then `show` \
         a transcript (whole by default — pass `first`, `last`, `range`, `around`+`context` or \
         `max_bytes` to window it) and `cat` one tool \
         call's full output by its tool_use_id. `pack` builds task-relevant context out of the whole \
         corpus; `doctor` explains why a session's context window keeps filling; `workflow`, `show` \
         with `tree` or `subagents` open up sub-agent forests, and `show` with `kind` reads only what \
         the person said (`prompt,answer`) or the compaction seams. `prune`, `rewind`, `distill`, `splice` \
         and `port` produce NEW sessions from existing ones without touching the source \
         (`rewind` = an agent as of a past message or commit, resumable). \
         To coordinate with sibling agents live, use `await_omen`/`observe_stream` (watch another \
         agent's output) and the `task_*` tools. \
         Harnesses cv reads: {}.",
        harnesses.join(", ")
    )
}

/// Every tool: the CLI-generated commands first (they are the ones an agent reaches for), then the
/// hand-written MCP-only tools. A generated name never shadows a hand-written one, and none is
/// dropped for colliding: `no_generated_tool_is_shadowed_by_a_hand_written_one` keeps the two
/// name sets disjoint, so the filter below removes nothing in a released binary.
fn tool_list() -> Value {
    let mut tools = base_tool_list();
    let Value::Array(list) = &mut tools else {
        return tools;
    };
    let hand_written: std::collections::HashSet<String> = list
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    if let Some(cli) = cli() {
        let mut generated: Vec<Value> = cli
            .tool_list()
            .into_iter()
            .filter(|t| !hand_written.contains(t["name"].as_str().unwrap_or("")))
            .collect();
        generated.append(list);
        *list = generated;
    }
    tools
}

#[derive(Clone, Copy)]
enum NativeTool {
    AwaitOmen,
    ObserveStream,
}

impl NativeTool {
    const ALL: [Self; 2] = [Self::AwaitOmen, Self::ObserveStream];

    fn name(self) -> &'static str {
        match self {
            Self::AwaitOmen => "await_omen",
            Self::ObserveStream => "observe_stream",
        }
    }

    fn find(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    fn blocking(self) -> bool {
        matches!(self, Self::AwaitOmen)
    }

    fn call(self, args: &Value, cancelled: Cancellation<'_>) -> anyhow::Result<String> {
        match self {
            Self::AwaitOmen => await_omen(args, cancelled),
            Self::ObserveStream => observe_stream(args, cancelled),
        }
    }

    fn descriptor(self) -> Value {
        match self {
            Self::AwaitOmen => json!(        {
                "name": self.name(),
                "description": "Block until another agent's session emits a message matching a regex, then return it. Watches live for new/appended messages (text, thinking, or tool results) across harnesses. Use to wait on a sibling agent — e.g. await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj'). Returns when matched or when timeout_secs elapses.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "regex": { "type": "string", "description": "Rust regex to match against newly-emitted message text." },
                        "harness": { "type": "string", "description": "Only watch this harness." },
                        "cwd_contains": { "type": "string", "description": "Only watch sessions whose cwd contains this substring." },
                        "timeout_secs": { "type": "number", "description": "Give up after this many seconds (default 120)." },
                        "interval_secs": { "type": "number", "description": "Poll interval (default 2)." }
                    },
                    "required": ["regex"]
                }
            }),
            Self::ObserveStream => json!(        {
                "name": self.name(),
                "description": "The NON-BLOCKING sibling of await_omen: drain the newly-appended messages from another agent's session(s) since an opaque cursor and return IMMEDIATELY with a fresh cursor — so a senior orchestrator can poll a junior agent's live activity on its own cadence (await_omen blocks until ONE regex matches; observe_stream tails the incremental stream of everything). The first call (no cursor) records a baseline and returns no backlog — pass the returned cursor to the next call to get only what was appended since. Read-only (never writes the board). Returns everything appended unless max_messages or char_cap bounds it. Scope it with cwd_contains and/or harness, e.g. observe_stream(cwd_contains='/junior-proj').",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd_contains": { "type": "string", "description": "Only observe sessions whose recorded cwd contains this substring." },
                        "harness": { "type": "string", "description": "Only observe this harness (any name `cv ls --harness` takes)." },
                        "since_cursor": { "type": "string", "description": "Opaque cursor from a previous observe_stream call. Omit on the first call to start from the current tail (baseline; returns no messages)." },
                        "max_messages": { "type": "number", "description": "Max messages to return this call (default: all). Remaining unread messages are reported as more_pending; poll again with the returned cursor to drain them." },
                        "char_cap": { "type": "number", "description": "Cap on total characters of message text returned (default: none). The message that crosses it is cut, ends in …, and is counted in `truncated`; the cursor moves past it." }
                    }
                }
            }),
        }
    }
}

pub(crate) fn base_tool_list() -> Value {
    Value::Array(NativeTool::ALL.into_iter().map(NativeTool::descriptor).collect())
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

/// Return freed heap pages to the OS after a request.
///
/// `cv mcp` lives as long as the host session (days), and the in-process tools (`observe_stream`,
/// `await_omen`, the generated tools' children) parse whole transcripts, so one call can transiently allocate ~1 GB
/// across several blocking-pool threads. glibc frees that memory but keeps it mapped in each
/// thread's arena: without a trim, RSS stays at the high-water mark of the largest call ever made,
/// times however many arenas were touched. `malloc_trim(0)` releases the free pages of every arena
/// (measured: 938 MB → 77 MB after one call mix). The live data is untouched, so behaviour is
/// unchanged; the cost is a walk of the free lists, small next to a tool call. Other allocators
/// (musl, macOS) return memory on their own, so this is a no-op there.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn release_freed_heap() {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::os::raw::c_int;
    }
    // SAFETY: glibc's malloc_trim takes only a padding size, touches no caller memory, and
    // locks each arena while it trims, so it is safe to call from any thread at any time.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn release_freed_heap() {}

enum Response {
    Json(Value),
    Command {
        id: Value,
        output: super::mcp_tools::CommandOutput,
    },
}
fn write_message(stdout: &mut impl Write, msg: &Response) -> anyhow::Result<()> {
    match msg {
        Response::Json(value) => serde_json::to_writer(&mut *stdout, value)?,
        Response::Command { id, output } => write_command_response(stdout, id, output)?,
    }
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

/// Publish the fixed JSON-RPC envelope through the server's single writer. Staged text is
/// immutable; serde handles the arbitrary request id and the shared writer handles escaping.
fn write_command_response(
    out: &mut impl Write,
    id: &Value,
    output: &super::mcp_tools::CommandOutput,
) -> anyhow::Result<()> {
    use std::io::Read;
    out.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":")?;
    serde_json::to_writer(&mut *out, id)?;
    out.write_all(b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":")?;
    if let Some(prefix) = &output.error_prefix {
        let detail = if output.stderr.trimmed.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        cv_core::projection::write_json_string(
            out,
            std::io::Cursor::new(prefix.as_bytes()).chain(detail.reader(true)?),
        )?;
    } else {
        cv_core::projection::write_json_string(out, output.stdout.reader(false)?)?;
        if output.stderr.len > 0 {
            out.write_all(b"},{\"type\":\"text\",\"text\":")?;
            cv_core::projection::write_json_string(out, output.stderr.reader(false)?)?;
        }
    }
    out.write_all(if output.error_prefix.is_some() {
        b"}],\"isError\":true}}"
    } else {
        b"}],\"isError\":false}}"
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool implementations (synchronous; each call runs on its request's thread)
// ---------------------------------------------------------------------------

enum ToolOutput {
    Text(String),
    Command(super::mcp_tools::CommandOutput),
}
fn call_tool(name: &str, args: &Value, cancelled: Cancellation<'_>) -> anyhow::Result<ToolOutput> {
    if let Some(tool) = NativeTool::find(name) {
        return tool.call(args, cancelled).map(ToolOutput::Text);
    }
    match name {
        // Removed in 0.15.0 (the coordination board): a pointer, never a silent "unknown tool".
        n if n.starts_with("board_") => anyhow::bail!(
            "{n} was removed in 0.15.0 with the coordination board — the task log is the fleet's record: \
             `task_note` to say something, `task_claim` to hold work, `await_omen`/`observe_stream` to wait on a sibling"
        ),
        // Removed in 0.15.0 with the land facet: a pointer, never a silent "unknown tool".
        "task_propose" | "task_pass" | "task_refute" | "task_verify" | "task_debt" | "task_stats" => {
            anyhow::bail!(
                "{name} was removed in 0.15.0 with the task land facet — completion is self-reported \
                 and labeled: `task_done` with `observed`, `task_note` for evidence"
            )
        }
        // Removed in 0.17: it reads Claude Code's hook JSON on stdin, which no MCP call supplies.
        "hook" => anyhow::bail!(
            "hook was removed from the MCP roster in 0.17 — it is Claude Code's hook body (stdin JSON), \
             which a tool call cannot supply; `hooks_install` wires it into Claude Code"
        ),
        // Everything else is a CLI-generated command tool: shell out to `cv`.
        other => match cli() {
            Some(c) if c.find(other).is_some() => c.call_cancelled(other, args, cancelled).map(ToolOutput::Command),
            Some(_) => anyhow::bail!("unknown tool: {other}"),
            None => anyhow::bail!("unknown tool: {other} (the command tools failed to register at startup)"),
        },
    }
}

/// Check cancellation and the same observation deadline during discovery and polling.
fn check_observation(
    cancelled: Cancellation<'_>,
    start: Instant,
    timeout: Duration,
    expired: &std::cell::Cell<bool>,
) -> anyhow::Result<()> {
    cancelled.check()?;
    if start.elapsed() >= timeout {
        expired.set(true);
        anyhow::bail!("observation deadline reached");
    }
    Ok(())
}

/// Block until a newly-emitted message matches `regex` (or `timeout_secs` elapses). The polling loop
/// runs on its request's own thread, so blocking is fine.
fn await_omen(args: &Value, cancelled: Cancellation<'_>) -> anyhow::Result<String> {
    let pattern = arg_str(args, "regex")?.ok_or_else(|| InvalidParams("`regex` is required".into()))?;
    let re = regex::Regex::new(pattern).with_context(|| format!("invalid regex: {pattern:?}"))?;
    let filter = Filter {
        harness: parse_harness(args)?,
        cwd_contains: arg_str(args, "cwd_contains")?.map(str::to_string),
    };
    let timeout = Duration::from_secs(arg_usize(args, "timeout_secs", 120)? as u64);
    let interval = Duration::from_secs(arg_usize(args, "interval_secs", 2)?.max(1) as u64);

    // Only react to activity from now on (not the existing backlog).
    cancelled.check()?;
    let start = Instant::now();
    let expired = std::cell::Cell::new(false);
    let watcher = Watcher::try_with_discovery(filter, false, cv_core::discover_all, || {
        check_observation(cancelled, start, timeout, &expired)
    });
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(_) if expired.get() => return omen_timeout(start),
        Err(error) => return Err(error),
    };
    loop {
        cancelled.check()?;
        if let Some(matched) = await_poll(&mut watcher, &re, cancelled, start, timeout)? {
            return Ok(serde_json::to_string_pretty(&matched)?);
        }
        if start.elapsed() >= timeout {
            return omen_timeout(start);
        }
        let sleep_for = interval.min(timeout.saturating_sub(start.elapsed()));
        let sleep_start = Instant::now();
        while sleep_start.elapsed() < sleep_for {
            cancelled.check()?;
            std::thread::sleep((sleep_for - sleep_start.elapsed().min(sleep_for)).min(Duration::from_millis(100)));
        }
    }
}

fn omen_timeout(start: Instant) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "matched": false, "timed_out": true, "waited_secs": start.elapsed().as_secs(),
    }))?)
}

/// Match directly while a checked watch read delivers individual retained messages. A hit
/// stays private until that read's source acceptance succeeds; failed/cancelled reads publish nothing.
fn await_poll(
    watcher: &mut Watcher,
    re: &regex::Regex,
    cancelled: Cancellation<'_>,
    start: Instant,
    timeout: Duration,
) -> anyhow::Result<Option<Value>> {
    let expired = std::cell::Cell::new(false);
    let check = || check_observation(cancelled, start, timeout, &expired);
    let mut matched = None;
    let result = watcher.try_poll_messages(&check, |kind, reference, message| {
        let resolver = cv_core::Resolver::new(Some(reference.path.clone()));
        let text = cv_core::projection::searchable_text_checked(&message, &resolver, &check)?;
        if let Some(hit) = re.find(&text) {
            check()?;
            let snippet_start = hit.start().saturating_sub(80);
            let snippet_end = hit.end().saturating_add(80).min(text.len());
            matched = Some(json!({
                "matched": true,
                "harness": reference.harness.as_str(), "id": reference.id,
                "cwd": reference.cwd.as_ref().map(|p| p.to_string_lossy()),
                "role": cv_core::render::role_label(message.role),
                "matched_text": &text[floor_char(&text, snippet_start)..ceil_char(&text, snippet_end)],
                "event": match kind { cv_core::watch::EventKind::New => "new_session", _ => "appended" },
            }));
            Ok(cv_core::Flow::Stop)
        } else {
            check()?;
            Ok(cv_core::Flow::Continue)
        }
    });
    match result {
        Err(_) if expired.get() => Ok(None),
        Err(error) => Err(error),
        Ok(_) => Ok(matched),
    }
}

// ---------------------------------------------------------------------------
// observe_stream — the non-blocking tail of await_omen
// ---------------------------------------------------------------------------

/// The reader states behind every cursor this server issued, one per observed session
/// ([`cv_core::watch::Readers`]): a cursor carries positions only.
static READERS: std::sync::LazyLock<Readers> = std::sync::LazyLock::new(Readers::default);

/// Non-blocking incremental tail: the messages appended to matching sessions since `since_cursor`
/// plus a fresh cursor, returned at once. It is one `Watcher` poll with the watcher's state carried
/// by the caller ([`cv_core::watch::Cursor`]) instead of by a process: the first call (no cursor)
/// records a baseline and emits nothing, so a caller follows from "now" rather than dumping
/// history. Read-only. Sessions come from the catalog fast path (`cv_core::sessions`, ~ms) rather
/// than a stat-the-fleet scan (~s), because this is *polled*; the catalog's staleness probe
/// re-stats the most recently updated transcripts, and an actively tailed session is one of them.
fn observe_stream(args: &Value, cancelled: Cancellation<'_>) -> anyhow::Result<String> {
    let check = || {
        cancelled.check()?;
        Ok(())
    };
    check()?;
    let filter = Filter {
        harness: parse_harness(args)?,
        cwd_contains: arg_str(args, "cwd_contains")?.map(str::to_string),
    };
    let max_messages = arg_usize(args, "max_messages", usize::MAX)?.max(1);
    let char_cap = match args.get("char_cap") {
        None | Some(Value::Null) => None,
        Some(_) => Some(arg_usize(args, "char_cap", 0)?.max(1)),
    };
    let prev = arg_str(args, "since_cursor")?.and_then(Cursor::decode);
    let baseline = prev.is_none();
    let mut watcher = match prev {
        Some(c) => Watcher::from_cursor(filter, c, cv_core::sessions, &READERS),
        None => Watcher::try_with_discovery(filter, false, cv_core::sessions, check)?,
    };

    observe_poll_checked(
        &mut watcher,
        baseline,
        max_messages,
        char_cap,
        |message, resolver, room| match room {
            None => Ok((
                cv_core::projection::searchable_text_checked(message, resolver, check)?,
                false,
            )),
            Some(room) => {
                let (mut text, truncated) =
                    cv_core::projection::searchable_prefix_checked(message, resolver, room, check)?;
                if truncated {
                    text.push('…');
                }
                Ok((text, truncated))
            }
        },
        check,
    )
}

#[cfg(test)]
fn observe_poll(
    watcher: &mut Watcher,
    baseline: bool,
    max_messages: usize,
    char_cap: Option<usize>,
    project: impl FnMut(&cv_core::Message, &cv_core::Resolver, Option<usize>) -> anyhow::Result<(String, bool)>,
) -> anyhow::Result<String> {
    observe_poll_checked(watcher, baseline, max_messages, char_cap, project, || Ok(()))
}

fn observe_poll_checked(
    watcher: &mut Watcher,
    baseline: bool,
    max_messages: usize,
    char_cap: Option<usize>,
    mut project: impl FnMut(&cv_core::Message, &cv_core::Resolver, Option<usize>) -> anyhow::Result<(String, bool)>,
    mut check: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<String> {
    let mut out_msgs: Vec<Value> = Vec::new();
    let mut chars_used = 0usize;
    let mut truncated = 0usize;
    let mut projection_error = None;
    let more_pending = !baseline
        && watcher.try_poll(
            Poll {
                max: max_messages,
                strict: true,
                retained: true,
                checked: Some(&mut check),
            },
            |ev| {
                if projection_error.is_some() {
                    return 0;
                }
                let r = &ev.reference;
                let resolver = cv_core::Resolver::new(Some(r.path.clone()));
                let mut taken = 0;
                for m in &ev.new_messages {
                    let room = char_cap.map(|cap| cap.saturating_sub(chars_used));
                    if room == Some(0) {
                        break;
                    }
                    let text = match project(m, &resolver, room) {
                        Ok((text, cut)) => {
                            truncated += usize::from(cut);
                            text
                        }
                        Err(error) => {
                            projection_error = Some(error);
                            break;
                        }
                    };
                    chars_used += text.len();
                    out_msgs.push(json!({
                        "harness": r.harness.as_str(),
                        "session_id": r.id,
                        "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                        "role": cv_core::render::role_label(m.role),
                        "timestamp": m.timestamp.map(|t| t.to_rfc3339()),
                        "text": text,
                    }));
                    taken += 1;
                }
                taken
            },
        )?;
    if let Some(error) = projection_error {
        return Err(error);
    }

    check()?;
    let mut out = json!({
        "baseline": baseline,
        "messages": out_msgs,
        "count": out_msgs.len(),
        "more_pending": more_pending,
        "rereads": watcher.rereads(),
        "cursor": watcher.to_cursor(&READERS).encode(),
    });
    // A caller's char_cap is stated beside what it cut.
    if let Some(cap) = char_cap {
        out["char_cap"] = cap.into();
        out["truncated"] = truncated.into();
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

/// All matchable text in a message: text + thinking + tool-result content.
#[cfg(test)]
fn message_text(m: &cv_core::Message) -> anyhow::Result<String> {
    let mut output = String::new();
    m.append_searchable(&mut output)?;
    Ok(output)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok((!value.is_empty()).then_some(value.as_str())),
        Some(value) => Err(InvalidParams(format!("`{key}` must be a string, got {value}")).into()),
    }
}

/// A malformed caller-supplied argument. Mapped to JSON-RPC `-32602 Invalid params` at the
/// protocol layer (unlike tool *execution* failures, which surface as `isError` tool results).
#[derive(Debug)]
pub(crate) struct InvalidParams(pub String);

impl std::fmt::Display for InvalidParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid params: {}", self.0)
    }
}

impl std::error::Error for InvalidParams {}

/// Read an optional non-negative-integer argument. Absent (or JSON null) yields `default`; a
/// present-but-malformed value (a string, a negative, a fractional float) is an [`InvalidParams`]
/// error rather than a silent fallback to the default. Integral floats are tolerated — some
/// clients serialize `20` as `20.0`.
fn arg_usize(args: &Value, key: &str, default: usize) -> anyhow::Result<usize> {
    let v = match args.get(key) {
        None | Some(Value::Null) => return Ok(default),
        Some(v) => v,
    };
    if let Some(n) = v.as_u64() {
        return Ok(n as usize);
    }
    if let Some(f) = v.as_f64() {
        if f >= 0.0 && f.fract() == 0.0 && f <= usize::MAX as f64 {
            return Ok(f as usize);
        }
    }
    Err(InvalidParams(format!("`{key}` must be a non-negative integer, got {v}")).into())
}

fn parse_harness(args: &Value) -> anyhow::Result<Option<Harness>> {
    match arg_str(args, "harness")? {
        None => Ok(None),
        Some(s) => match Harness::parse(s) {
            Some(h) => Ok(Some(h)),
            None => Err(InvalidParams(format!(
                "unknown harness: {s:?} (expected one of {})",
                Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")
            ))
            .into()),
        },
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use cv_core::{Block, Json, Message, Role, SessionRef};

    fn discover() -> Vec<SessionRef> {
        cv_core::harness::for_harness(Harness::Claude)
            .unwrap()
            .discover()
            .unwrap()
            .refs
    }

    // Independent pre-change owned observer: whole searchable strings, then truncation.
    fn owned_observe_oracle(watcher: &mut Watcher, cap: usize) -> anyhow::Result<String> {
        let mut rows = Vec::new();
        let mut used = 0usize;
        let mut cut = 0usize;
        let more = watcher.try_poll(Poll { max: 50, strict: true, ..Poll::default() }, |event| {
            let r=&event.reference; let mut taken=0;
            for m in event.new_messages {
                let full=message_text(&m).unwrap(); let room=cap.saturating_sub(used);
                if room==0 { break; }
                let text=if full.len()>room {cut+=1; format!("{}…",&full[..floor_char(&full,room)])} else {full};
                used+=text.len();
                rows.push(json!({"harness":r.harness.as_str(),"session_id":r.id,"cwd":r.cwd.as_ref().map(|p|p.to_string_lossy().to_string()),"role":cv_core::render::role_label(m.role),"timestamp":m.timestamp.map(|t|t.to_rfc3339()),"text":text}));taken+=1;
            } taken
        })?;
        Ok(serde_json::to_string_pretty(
            &json!({"baseline":false,"messages":rows,"count":rows.len(),"more_pending":more,"rereads":watcher.rereads(),"cursor":watcher.to_cursor(&READERS).encode(),"char_cap":cap,"truncated":cut}),
        )?)
    }

    #[test]
    #[ignore = "isolated clean release CPU control; CV_OBSERVE_CONTROL=old|new"]
    fn observe_tiny_actual_consumer_cpu() {
        let world = crate::test_world("observe-tiny-cpu");
        world.write_session("observe-tiny", &[json!({"type":"assistant", "sessionId":"observe-tiny", "uuid":"a", "message":{"role":"assistant","content":"tiny 月 text"}})]);
        let retained = std::env::var("CV_OBSERVE_CONTROL").unwrap() == "new";
        let start = std::time::Instant::now();
        for _ in 0..2000 {
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            if retained {
                std::hint::black_box(
                    observe_poll(&mut watcher, false, 50, Some(16000), |m, r, cap| {
                        let (mut text, truncated) = cv_core::projection::searchable_prefix(m, r, cap.unwrap())?;
                        if truncated {
                            text.push('…');
                        }
                        Ok((text, truncated))
                    })
                    .unwrap(),
                );
            } else {
                std::hint::black_box(owned_observe_oracle(&mut watcher, 16000).unwrap());
            }
        }
        println!("observe tiny retained={retained} wall={:?}", start.elapsed());
    }

    #[test]
    fn retained_observation_rejects_callback_mutation_before_acceptance() {
        let world = crate::test_world("observe-prefix-mutation");
        let path = world.write_session("observe-mutation", &[json!({"type":"assistant", "sessionId":"observe-mutation", "uuid":"a", "message":{"role":"assistant","content":"hello"}})]);
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        let before = watcher.to_cursor(&READERS).encode();
        let result = watcher.try_poll(
            Poll {
                max: 50,
                strict: true,
                retained: true,
                ..Poll::default()
            },
            |event| {
                assert_eq!(event.new_messages.len(), 1);
                std::fs::write(&path, b"changed").unwrap();
                1
            },
        );
        assert!(result.is_err());
        assert_eq!(watcher.to_cursor(&READERS).encode(), before);
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "isolated allocation evidence; run one test thread with probe-heap"]
    fn observe_giant_actual_consumer_heap() {
        let world = crate::test_world("observe-giant-heap");
        for kind in ["text", "arguments"] {
            for mib in [16, 64] {
                let body = "x\n月".repeat(mib * 1024 * 1024 / 5);
                let block = if kind == "text" {
                    json!({"type":"text","text":body})
                } else {
                    json!({"type":"tool_use","id":"c","name":"tool","input":{"payload":body}})
                };
                world.write_session("observe-giant", &[json!({"type":"assistant", "sessionId":"observe-giant", "uuid":"a", "message":{"role":"assistant","content":[block]}})]);
                let mut oracle = None;
                for retained in [false, true] {
                    let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
                    let baseline = cv_testkit::heap::begin();
                    let output = if retained {
                        observe_poll(&mut watcher, false, 50, Some(16000), |m, r, cap| {
                            let (mut text, truncated) = cv_core::projection::searchable_prefix(m, r, cap.unwrap())?;
                            if truncated {
                                text.push('…');
                            }
                            Ok((text, truncated))
                        })
                        .unwrap()
                    } else {
                        owned_observe_oracle(&mut watcher, 16000).unwrap()
                    };
                    let heap = cv_testkit::heap::end(baseline);
                    println!(
                        "{}",
                        json!({"kind":kind,"mib":mib,"retained":retained,"heap":heap,"output_bytes":output.len()})
                    );
                    if retained {
                        assert!(heap.peak_above_baseline_bytes < 2 * 1024 * 1024);
                    }
                    if retained {
                        assert_eq!(
                            oracle.as_deref(),
                            Some(output.as_str()),
                            "complete giant response oracle"
                        );
                    } else {
                        oracle = Some(output.clone());
                    }
                    drop(output);
                }
            }
        }
    }

    #[test]
    fn observe_retained_prefix_matches_owned_oracle() {
        let world = crate::test_world("observe-prefix-oracle");
        let body = "月🌙é\n\"\\".repeat(4000);
        world.write_session("observe-prefix", &[json!({"type":"assistant", "sessionId":"observe-prefix", "uuid":"a", "message":{"role":"assistant","content":[{"type":"text","text":body},{"type":"tool_use","id":"c","name":"tool","input":{"payload":body}}]}})]);
        for cap in [1usize, 2, 3, 4, 7, 19, 16000, 200000] {
            let mut old = Watcher::with_discovery(Filter::default(), true, discover);
            let oracle = owned_observe_oracle(&mut old, cap).unwrap();
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            let output = observe_poll(&mut watcher, false, 50, Some(cap), |message, resolver, room| {
                let (mut text, truncated) = cv_core::projection::searchable_prefix(message, resolver, room.unwrap())?;
                if truncated {
                    text.push('…');
                }
                Ok((text, truncated))
            })
            .unwrap();
            // Each cursor names its own watcher's reader generations; everything else is equal.
            let without_cursor = |text: &str| {
                let mut v: Value = serde_json::from_str(text).unwrap();
                v.as_object_mut().unwrap().remove("cursor");
                v
            };
            assert_eq!(without_cursor(&output), without_cursor(&oracle), "cap={cap}");
        }
    }

    /// With no char_cap a poll returns every appended message whole and states no cap; with one,
    /// it states the cap and how many messages it cut.
    #[test]
    fn observe_is_whole_unless_capped_and_says_what_a_cap_cut() {
        let world = crate::test_world("observe-whole");
        let body = "word ".repeat(10_000);
        world.write_session("observe-whole", &[json!({"type":"assistant", "sessionId":"observe-whole", "uuid":"a", "message":{"role":"assistant","content":body}})]);
        let poll = |cap: Option<usize>| -> Value {
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            let out = observe_poll_checked(
                &mut watcher,
                false,
                usize::MAX,
                cap,
                |m, r, room| match room {
                    None => Ok((cv_core::projection::searchable_text_checked(m, r, || Ok(()))?, false)),
                    Some(room) => cv_core::projection::searchable_prefix(m, r, room),
                },
                || Ok(()),
            )
            .unwrap();
            serde_json::from_str(&out).unwrap()
        };
        let whole = poll(None);
        assert_eq!(
            whole["messages"][0]["text"].as_str().unwrap().trim_end(),
            body.trim_end()
        );
        assert!(
            whole.get("char_cap").is_none() && whole.get("truncated").is_none(),
            "{whole}"
        );
        let capped = poll(Some(100));
        assert_eq!(
            (capped["char_cap"].as_u64(), capped["truncated"].as_u64()),
            (Some(100), Some(1))
        );
    }

    #[test]
    fn observe_over_many_sessions_rereads_none_of_them() {
        // 0.17 carried reader state for eight sessions in the cursor: past eight, every session
        // that had changed once was read from its start again on every later call.
        let world = crate::test_world("observe-many-sessions");
        let line = |sid: &str, uuid: &str| json!({"type":"user","sessionId":sid,"uuid":uuid,"message":{"role":"user","content":"activity"}});
        let ids: Vec<String> = (0..12).map(|i| format!("observed-{i:02}")).collect();
        for id in &ids {
            world.write_session(id, &[line(id, "a")]);
        }
        let call = |cursor: Option<&str>| -> (Value, usize) {
            let mut watcher = match cursor {
                Some(c) => Watcher::from_cursor(Filter::default(), Cursor::decode(c).unwrap(), discover, &READERS),
                None => Watcher::with_discovery(Filter::default(), false, discover),
            };
            let out = observe_poll(&mut watcher, cursor.is_none(), 500, None, |m, _, _| {
                Ok((message_text(m)?, false))
            })
            .unwrap();
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["rereads"], watcher.rereads(), "the output states its rereads");
            (v, watcher.rereads())
        };
        let (base, _) = call(None);
        for id in &ids {
            world.write_session(id, &[line(id, "a"), line(id, "b")]);
        }
        let (grown, rereads) = call(base["cursor"].as_str());
        assert_eq!(grown["count"], 12);
        assert_eq!(
            rereads, 0,
            "each grown session resumes from its baseline's reader state"
        );
        let mut cursor = grown["cursor"].as_str().unwrap().to_string();
        for _ in 0..3 {
            let (quiet, rereads) = call(Some(&cursor));
            assert_eq!(quiet["count"], 0);
            assert_eq!(rereads, 0, "a session with a reader state is not read from its start");
            cursor = quiet["cursor"].as_str().unwrap().to_string();
        }
        assert_eq!(
            ids.iter()
                .filter(|id| READERS.contains(&format!("claude:{id}")))
                .count(),
            12,
            "one reader state per observed session (READERS is process-wide: \
             a total count races with other tests' sessions)"
        );
        // Carried to a process that never issued it, the cursor reads each changed session
        // from its start once, and says so.
        for id in &ids {
            world.write_session(id, &[line(id, "a"), line(id, "b"), line(id, "c")]);
        }
        let elsewhere = Readers::default();
        let mut watcher = Watcher::from_cursor(
            Filter::default(),
            Cursor::decode(&cursor).unwrap(),
            discover,
            &elsewhere,
        );
        let out: Value = serde_json::from_str(
            &observe_poll(&mut watcher, false, 500, None, |m, _, _| Ok((message_text(m)?, false))).unwrap(),
        )
        .unwrap();
        assert_eq!((out["count"].as_u64(), out["rereads"].as_u64()), (Some(12), Some(12)));
    }

    #[test]
    fn observe_read_failure_returns_error_without_advancing_cursor() {
        let _world = crate::test_world("mcp-unreadable-boundary");
        fn unreadable() -> Vec<SessionRef> {
            vec![SessionRef::new(
                "unreadable",
                Harness::Claude,
                std::env::temp_dir().join(format!("cv-mcp-missing-{}.jsonl", uuid::Uuid::new_v4())),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, unreadable);
        let before = watcher.to_cursor(&READERS).encode();
        assert!(observe_poll(&mut watcher, false, 50, Some(16_000), |m, _, _| Ok((
            message_text(m)?,
            false
        )))
        .is_err());
        assert_eq!(
            watcher.to_cursor(&READERS).encode(),
            before,
            "a demanded read failure leaves cursor position unchanged"
        );
    }

    #[test]
    fn observe_projection_failure_returns_error_without_a_success_cursor() {
        let world = crate::test_world("mcp-json-boundary");
        world.write_session("observe-json", &[json!({"type":"user", "sessionId":"observe-json", "uuid":"u", "message":{"role":"user","content":"new activity"}})]);
        let path = world.base.join("arguments.json");
        let value = json!({"argument":"月🌙é\n\"\\".repeat(300)});
        let bytes = serde_json::to_vec(&value).unwrap();
        let message = |input| {
            let mut message = Message::new(Role::Assistant);
            message.content.push(Block::ToolUse {
                id: "call".into(),
                name: "custom".into(),
                input,
                namespace: None,
            });
            message
        };
        let owned_text = message_text(&message(value.into())).unwrap();
        for deleted in [false, true] {
            std::fs::write(&path, &bytes).unwrap();
            let source = message(Json::source(&path, 0, bytes.len() as u64).unwrap());
            assert_eq!(message_text(&source).unwrap(), owned_text);
            if deleted {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"{} ").unwrap();
            }
            let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
            let mut demanded = false;
            let result = observe_poll(&mut watcher, false, 50, Some(16_000), |_, _, _| {
                demanded = true;
                Ok((message_text(&source)?, false))
            });
            assert!(demanded, "must exercise a demanded callback projection");
            assert!(
                result.is_err(),
                "cannot serialize a success cursor after failed projection"
            );
            let mut recovered = 0;
            watcher.poll(
                Poll {
                    max: 50,
                    ..Poll::default()
                },
                |event| {
                    recovered += event.new_messages.len();
                    event.new_messages.len()
                },
            );
            assert_eq!(recovered, 1, "failed projection must leave offered messages unread");
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::super::mcp_tools::{CapturedText, CommandOutput};
    use super::*;
    #[test]
    fn staged_protocol_matches_old_value_oracle_and_keeps_success_stderr() {
        let body = "λ🌙\"\\\n".repeat(12000).into_bytes();
        for stdout in [b"".as_slice(), b"ok\n", b"\xff\xe2\x82", body.as_slice()] {
            for stderr in [b"".as_slice(), " warning λ\n".as_bytes(), b" \xff\n".as_slice()] {
                for failure in [false, true] {
                    let prefix = failure.then(|| "error: cv show s exited 2: ".to_owned());
                    let output = CommandOutput {
                        stdout: CapturedText::capture(stdout).unwrap(),
                        stderr: CapturedText::capture(stderr).unwrap(),
                        error_prefix: prefix.clone(),
                    };
                    let id = json!({"nested":{"deeper":{"text":"do not replace id"}}});
                    let stdout = String::from_utf8_lossy(stdout);
                    let stderr = String::from_utf8_lossy(stderr);
                    let mut expected = if let Some(prefix) = prefix {
                        let detail = if stderr.trim().is_empty() {
                            stdout.trim()
                        } else {
                            stderr.trim()
                        };
                        tool_text_result(&format!("{prefix}{detail}"), true)
                    } else {
                        tool_text_result(&stdout, false)
                    };
                    if !failure && !stderr.is_empty() {
                        expected["content"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"type":"text","text":stderr}));
                    }
                    let mut expected = serde_json::to_vec(&ok_response(id.clone(), expected)).unwrap();
                    expected.push(b'\n');
                    let mut actual = Vec::new();
                    write_message(&mut actual, &Response::Command { id, output }).unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
    #[test]
    fn staged_writer_failure_and_protocol_classification() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = CommandOutput {
            stdout: CapturedText::capture(b"body".as_slice()).unwrap(),
            stderr: CapturedText::capture(b"".as_slice()).unwrap(),
            error_prefix: None,
        };
        assert!(write_message(&mut Failed, &Response::Command { id: json!(1), output }).is_err());
        let cancelled = AtomicBool::new(false);
        let params = json!({"name":"observe_stream","arguments":{"max_messages":"invalid"}});
        let Some(Response::Json(value)) = handle("tools/call", Some(params), Some(json!(1)), &cancelled) else {
            panic!("error frame");
        };
        assert_eq!(value["error"]["code"], -32602);
        let params = json!({"name":"board_removed","arguments":{}});
        let Some(Response::Json(value)) = handle("tools/call", Some(params.clone()), Some(json!(1)), &cancelled) else {
            panic!("tool error frame");
        };
        assert_eq!(value["result"]["isError"], true);
        assert!(handle("tools/call", Some(params), None, &cancelled).is_none());
    }
}

#[cfg(all(test, feature = "probe-heap"))]
mod transport_heap_probe {
    use super::*;
    #[test]
    #[ignore = "isolated whole generated-tool transport allocation control"]
    fn staged_mcp_whole_transport_heap() {
        struct Count(u64);
        impl Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bin = std::env::var("CV_MCP_PROBE_BIN").expect("actual cv binary");
        let source = std::env::var("CV_MCP_PROBE_SOURCE").expect("scratch source");
        let range = std::env::var("CV_MCP_PROBE_RANGE").unwrap_or_else(|_| "0..1".into());
        let mode = std::env::var("CV_MCP_PROBE_MODE").expect("owned or staged");
        let argv = vec!["show".to_string(), source, "--range".into(), range, "--json".into()];
        let mut sink = Count(0);
        let baseline = cv_testkit::heap::begin();
        if mode == "owned" {
            let child = std::process::Command::new(&bin)
                .args(&argv)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(child.status.success());
            let text = String::from_utf8_lossy(&child.stdout).into_owned();
            let value = ok_response(json!(1), tool_text_result(&text, false));
            let mut frame = serde_json::to_vec(&value).unwrap();
            frame.push(b'\n');
            sink.write_all(&frame).unwrap();
        } else {
            let output =
                super::super::mcp_tools::run_command(std::path::Path::new(&bin), &argv, &AtomicBool::new(false))
                    .unwrap();
            assert!(output.error_prefix.is_none());
            write_message(&mut sink, &Response::Command { id: json!(1), output }).unwrap();
        }
        let heap = cv_testkit::heap::end(baseline);
        eprintln!(
            "MCP_TRANSPORT_HEAP {}",
            json!({"mode":mode,"wire_bytes":sink.0,"heap":heap})
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn permit_covers_queued_response_and_releases_on_disconnect_and_panic() {
        let cancelled = AtomicBool::new(false);
        let pool = Admission::new(1);
        let permit = pool.acquire(false, &cancelled).unwrap();
        let execution = permit.clone();
        drop(permit);
        assert!(
            pool.acquire(false, &cancelled).is_none(),
            "execution still owns the shared lease"
        );
        drop(execution);
        let permit = pool.acquire(false, &cancelled).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Pending {
            response: Response::Json(json!({})),
            _permit: Some(permit),
        })
        .unwrap_or_else(|_| panic!("send failed"));
        assert!(
            pool.acquire(false, &cancelled).is_none(),
            "completed responses retain admission"
        );
        drop(rx);
        assert!(
            pool.acquire(false, &cancelled).is_some(),
            "disconnect drops queued ownership"
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _permit = pool.acquire(false, &cancelled).unwrap();
            panic!("handler failure");
        }));
        assert!(result.is_err());
        assert!(pool.acquire(false, &cancelled).is_some());
        let permit = pool.acquire(false, &cancelled).unwrap();
        assert!(tx
            .send(Pending {
                response: Response::Json(json!({})),
                _permit: Some(permit)
            })
            .is_err());
        assert!(
            pool.acquire(false, &cancelled).is_some(),
            "failed send releases admission"
        );
    }

    #[test]
    fn notification_wait_is_cancelled_and_poll_capacity_is_separate() {
        let ordinary = Admission::new(ORDINARY_CALLS);
        let polls = Admission::new(BLOCKING_POLLS);
        let cancelled = Arc::new(AtomicBool::new(false));
        let held: Vec<_> = (0..BLOCKING_POLLS)
            .map(|_| polls.acquire(false, &cancelled).unwrap())
            .collect();
        assert!(polls.acquire(false, &cancelled).is_none());
        assert!(ordinary.acquire(false, &cancelled).is_some());
        let other = cancelled.clone();
        let pool = polls.clone();
        let waiter = std::thread::spawn(move || pool.acquire(true, &other).is_none());
        cancelled.store(true, Ordering::Relaxed);
        assert!(waiter.join().unwrap());
        drop(held);
        assert_eq!(*polls.used.lock().unwrap(), 0);
    }

    #[test]
    fn completed_after_disconnect_is_dropped_before_publication() {
        let cancelled = AtomicBool::new(false);
        let admission = Admission::new(1);
        let permit = admission.acquire(false, &cancelled).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        cancelled.store(true, Ordering::Relaxed);
        enqueue_tool_response(&tx, Response::Json(json!({"id":1})), permit, &cancelled);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(*admission.used.lock().unwrap(), 0);
    }

    #[test]
    fn publication_wait_stops_on_disconnect_even_with_full_queue() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let admission = Admission::new(1);
        let permit = admission.acquire(false, &cancelled).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Pending::control(Response::Json(json!({"id":0}))))
            .unwrap_or_else(|_| panic!("send failed"));
        let other = cancelled.clone();
        let worker =
            std::thread::spawn(move || enqueue_tool_response(&tx, Response::Json(json!({"id":1})), permit, &other));
        cancelled.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_eq!(*admission.used.lock().unwrap(), 0);
        assert!(matches!(
            rx.try_recv(),
            Ok(Pending {
                response: Response::Json(_),
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn large_tool_arguments_transfer_ownership_from_frame_to_execution() {
        let request = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"show", "arguments":{"id":"x".repeat(2 * 1024 * 1024)}}});
        let original = request["params"]["arguments"]["id"].as_str().unwrap().as_ptr();
        let (method, params, id) = request_frame(request).unwrap();
        assert_eq!(method, "tools/call");
        assert_eq!(id, Some(json!(1)));
        let (name, args) = tool_call_params(params).unwrap();
        assert_eq!(name, "show");
        let transferred = args["id"].as_str().unwrap();
        assert_eq!(transferred.len(), 2 * 1024 * 1024);
        assert_eq!(transferred.as_ptr(), original, "large request payload was cloned");
    }

    #[test]
    fn omen_matches_first_message_and_cursor_keeps_the_unseen_tail() {
        let world = crate::test_world("omen-incremental");
        world.write_session("omen", &[
            json!({"type":"assistant", "uuid":"a", "message":{"role":"assistant", "content":[{"type":"text", "text":"λ BUILD PASSED"}]}}),
            json!({"type":"user", "uuid":"b", "message":{"role":"user", "content":"unseen second"}}),
            json!({"type":"user", "uuid":"c", "message":{"role":"user", "content":"unseen third"}}),
        ]);
        fn discover_omen() -> Vec<cv_core::SessionRef> {
            cv_core::harness::for_harness(cv_core::Harness::Claude)
                .unwrap()
                .discover()
                .unwrap()
                .refs
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover_omen);
        let matched = await_poll(
            &mut watcher,
            &regex::Regex::new("BUILD PASSED").unwrap(),
            Cancellation::connection(&AtomicBool::new(false)),
            Instant::now(),
            Duration::from_secs(30),
        )
        .unwrap()
        .unwrap();
        assert_eq!(matched["matched_text"], "λ BUILD PASSED\n");
        assert_eq!(matched["id"], "omen");
        assert_eq!(matched["role"], "assistant");
        let cursor = Cursor::decode(&watcher.to_cursor(&READERS).encode()).unwrap();
        let mut resumed = Watcher::from_cursor(Filter::default(), cursor, discover_omen, &READERS);
        let mut tail = Vec::new();
        resumed
            .try_poll_messages(
                || Ok(()),
                |_, reference, message| {
                    tail.push(cv_core::projection::searchable_text_checked(
                        &message,
                        &cv_core::Resolver::new(Some(reference.path.clone())),
                        || Ok(()),
                    )?);
                    Ok(cv_core::Flow::Continue)
                },
            )
            .unwrap();
        assert_eq!(tail, ["unseen second\n", "unseen third\n"]);
        assert!(await_poll(
            &mut resumed,
            &regex::Regex::new(".*").unwrap(),
            Cancellation::connection(&AtomicBool::new(true)),
            Instant::now(),
            Duration::from_secs(30)
        )
        .is_err());
        assert!(await_poll(
            &mut resumed,
            &regex::Regex::new(".*").unwrap(),
            Cancellation::connection(&AtomicBool::new(false)),
            Instant::now(),
            Duration::ZERO
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn disconnected_poll_does_not_enter_watcher() {
        let cancelled = AtomicBool::new(true);
        let error = await_omen(&json!({"regex":"never"}), Cancellation::connection(&cancelled)).unwrap_err();
        assert_eq!(error.to_string(), "MCP client disconnected");
    }

    #[test]
    fn disconnected_observer_does_not_enter_watcher() {
        let error = observe_stream(&json!({}), Cancellation::connection(&AtomicBool::new(true))).unwrap_err();
        assert_eq!(error.to_string(), "MCP client disconnected");
    }

    #[test]
    fn notification_wait_resumes_after_response_is_dropped() {
        let pool = Admission::new(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let held = pool.acquire(false, &cancelled).unwrap();
        let other = cancelled.clone();
        let admission = pool.clone();
        let waiter = std::thread::spawn(move || admission.acquire(true, &other));
        drop(held);
        assert!(waiter.join().unwrap().is_some());
        assert_eq!(*pool.used.lock().unwrap(), 0);
    }
}

#[cfg(test)]
mod instructions_tests {
    /// A generated tool whose name a hand-written one already holds is dropped from the roster
    /// (`tool_list`); no command may be lost that way unnoticed, so none may collide at all.
    #[test]
    fn no_generated_tool_is_shadowed_by_a_hand_written_one() {
        crate::on_main_sized_stack(|| {
            let generated = super::CliTools::from_owned_tree(crate::build_cli()).expect("tool tree");
            let hand_written: Vec<&str> = super::NativeTool::ALL.iter().map(|t| t.name()).collect();
            let shadowed: Vec<String> = generated
                .tool_list()
                .iter()
                .filter_map(|t| t["name"].as_str())
                .filter(|name| hand_written.contains(name))
                .map(str::to_string)
                .collect();
            assert!(
                shadowed.is_empty(),
                "generated tools shadowed by hand-written ones: {shadowed:?}"
            );
        });
    }

    /// The blurb is the one hand-written roster of the server's surface: every backticked word must
    /// be a tool, a `prefix*` family of tools, an argument some tool takes, or values of one of
    /// those arguments, all read from the live tool list. A renamed tool or flag fails here. (A
    /// review once read `query` as a removed command; it is `ls`'s filter argument, and passes.)
    #[test]
    fn instructions_name_only_real_tools_and_arguments() {
        crate::on_main_sized_stack(|| {
            // As `serve` does at startup: the generated tools come from the clap tree.
            let generated = super::CliTools::from_owned_tree(crate::build_cli()).expect("tool tree");
            let _ = super::CLI.set(Some(generated));
            let tools = super::tool_list();
            let tools = tools.as_array().expect("tool list is an array");
            let names: std::collections::HashSet<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
            let properties = || {
                tools
                    .iter()
                    .filter_map(|t| t["inputSchema"]["properties"].as_object())
                    .flat_map(|p| p.iter())
            };
            let arguments: std::collections::HashSet<&str> = properties().map(|(k, _)| k.as_str()).collect();
            let values: std::collections::HashSet<String> = properties()
                .flat_map(|(_, p)| {
                    let listed = p["enum"].as_array().or_else(|| p["items"]["enum"].as_array());
                    listed
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_string))
                })
                .collect();
            // `kind` is a free comma list `show` validates itself: its own parser is the oracle.
            assert!(arguments.contains("kind"));
            let kind = |word: &str| crate::cmd::view::parse_kinds(word).is_ok();
            let text = super::instructions();
            for word in text.split('`').skip(1).step_by(2) {
                if let Some(prefix) = word.strip_suffix('*') {
                    assert!(names.iter().any(|n| n.starts_with(prefix)), "no {word} tool");
                } else if !names.contains(word) && !arguments.contains(word) {
                    assert!(
                        word.split(',').all(|v| values.contains(v)) || kind(word),
                        "instructions name {word:?}: not a tool, an argument, or argument values"
                    );
                }
            }
        });
    }
}
