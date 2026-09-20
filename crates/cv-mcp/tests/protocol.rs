//! Protocol-level integration tests for the cv-mcp stdio JSON-RPC server: spawn the real binary,
//! run the MCP handshake, list tools, and call them against a temp-home fixture corpus.
//!
//! Hermetic: a fake `$HOME` carries the claude fixtures and `$CLUSTERVISION_HOME` the index/board
//! state; both are passed only to the child process's environment. `$CV_BIN` points at a **stub
//! `cv`** written into the same temp tree, so the CLI-generated half of the toolset is exercised
//! end-to-end (schema dump → tool registration → argv → exit code) without depending on a built
//! `clustervision` binary being present. What the real commands *do* is the CLI crate's business;
//! what this crate owes is the translation.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A running cv-mcp server over stdio, with a line reader on a side thread so a hung server
/// fails the test with a timeout instead of wedging the run.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    base: PathBuf,
}

impl Server {
    fn spawn(tag: &str) -> Server {
        let base = std::env::temp_dir().join(format!(
            "cv-mcp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&cv_home).unwrap();

        // Fixture corpus: two tiny claude sessions.
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done — the migration is fixed"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "how many quokkas in the census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen quokkas"}]}}),
        ];
        for (name, lines) in [("alphasess", &alpha[..]), ("betasess", &beta[..])] {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(proj.join(format!("{name}.jsonl")), body).unwrap();
        }

        let cv_bin = write_stub_cv(&base);

        let mut child = Command::new(env!("CARGO_BIN_EXE_cv-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("HOME", &home)
            .env("CV_BIN", &cv_bin)
            .env("CLUSTERVISION_HOME", &cv_home)
            .env_remove("CV_ENDPOINT") // hermetic: ambient identity must not leak into the tests
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .spawn()
            .expect("cv-mcp should spawn");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Server {
            child,
            stdin,
            lines: rx,
            base,
        }
    }

    /// Send one raw line to the server.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to cv-mcp stdin");
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, v: &Value) {
        self.send_raw(&v.to_string());
    }

    /// Next response line as JSON (10s timeout → test failure, not a hang).
    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for a cv-mcp response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON response {line:?}: {e}"))
    }

    /// Round-trip a request and assert the JSON-RPC envelope (version + echoed id).
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let resp = self.recv();
        assert_eq!(resp["jsonrpc"], "2.0", "envelope: {resp}");
        assert_eq!(resp["id"], json!(id), "id must echo: {resp}");
        resp
    }

    /// Call a tool and return `(text, is_error)` from the MCP tool result.
    fn call_tool(&mut self, id: u64, name: &str, args: Value) -> (String, bool) {
        let resp = self.request(id, "tools/call", json!({"name": name, "arguments": args}));
        let result = &resp["result"];
        assert!(result.is_object(), "tools/call must return a result: {resp}");
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("tool result must carry content: {resp}"));
        assert_eq!(content[0]["type"], "text", "{resp}");
        let text = content[0]["text"].as_str().unwrap_or_default().to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }
}

/// The command tree the stub `cv` reports: a faithful (abridged) copy of the real
/// `cv schema --commands --json` shape, including `show`'s window flags, a help-less `--harness`,
/// a command with no `--json` form (`cat`), two positionals in order, and a `Fleet & live` entry
/// that must NOT become a tool.
const STUB_COMMANDS: &str = r#"[
  {"name":"ls","group":"Read","about":"List discovered sessions across all harnesses","args":[
    {"name":"harness","kind":"option","value_type":"string","help":"Only this harness","required":false},
    {"name":"cwd","kind":"option","value_type":"string","help":"Only sessions whose cwd contains this substring","required":false},
    {"name":"limit","kind":"option","value_type":"integer","help":"Max rows to show","required":false,"default":"40"},
    {"name":"sort_by","kind":"option","value_type":"string","help":"Sort key","required":false,"possible_values":["updated","created","messages"],"default":"updated"},
    {"name":"json","kind":"flag","value_type":"boolean","help":"Emit the rows as JSON","required":false}]},
  {"name":"show","group":"Read","about":"Print a single session","args":[
    {"name":"id","kind":"positional","value_type":"string","help":"Session id","required":true},
    {"name":"harness","kind":"option","value_type":"string","help":"","required":false},
    {"name":"json","kind":"flag","value_type":"boolean","help":"Emit the raw unified IR as JSON","required":false},
    {"name":"first","kind":"option","value_type":"integer","help":"The first N messages","required":false},
    {"name":"last","kind":"option","value_type":"integer","help":"The last N messages","required":false},
    {"name":"range","kind":"option","value_type":"string","help":"Messages A..B","required":false},
    {"name":"around","kind":"option","value_type":"integer","help":"Message N with context either side","required":false},
    {"name":"context","kind":"option","value_type":"integer","help":"How many either side","required":false,"default":"5"},
    {"name":"max_bytes","kind":"option","value_type":"integer","help":"Stop after N bytes","required":false},
    {"name":"subagents","kind":"flag","value_type":"boolean","help":"List the sub-agent forest","required":false}]},
  {"name":"cat","group":"Read","about":"Print one tool call's full output","args":[
    {"name":"session","kind":"positional","value_type":"string","help":"The pruned session id","required":true},
    {"name":"tool_use_id","kind":"positional","value_type":"string","help":"The tool_use id","required":true},
    {"name":"input","kind":"flag","value_type":"boolean","help":"Print the call's arguments instead","required":false}]},
  {"name":"search","group":"Read","about":"Search across session transcripts","args":[
    {"name":"query","kind":"positional","value_type":"string","help":"Text to search for","required":true},
    {"name":"limit","kind":"option","value_type":"integer","help":"Max results","required":false,"default":"20"},
    {"name":"json","kind":"flag","value_type":"boolean","help":"Emit rows as JSON","required":false}]},
  {"name":"task","group":"Fleet & live","about":"Durable fleet tasks","args":[]},
  {"name":"task open","group":"Fleet & live","about":"Open a task","args":[
    {"name":"title","kind":"positional","value_type":"string","help":"Title","required":true}]}
]"#;

/// Write a stub `cv` that answers the schema dump from a file, echoes its argv for every other
/// call, and fails loudly on a session id of `nosuch` (the non-zero-exit path).
fn write_stub_cv(base: &Path) -> PathBuf {
    let bin_dir = base.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("commands.json"), STUB_COMMANDS).unwrap();
    let cv = bin_dir.join("cv");
    fs::write(
        &cv,
        "#!/bin/sh\n\
         if [ \"$1\" = schema ]; then cat \"$(dirname \"$0\")/commands.json\"; exit 0; fi\n\
         case \"$*\" in *nosuch*) echo 'cv: no session found for id \"nosuch\"' >&2; exit 2;; esac\n\
         printf 'ARGV'\n\
         for a in \"$@\"; do printf ' %s' \"$a\"; done\n\
         echo\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cv, fs::Permissions::from_mode(0o755)).unwrap();
    }
    cv
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        fs::remove_dir_all(&self.base).ok();
    }
}

#[test]
fn handshake_and_tool_schemas() {
    let mut s = Server::spawn("handshake");

    // initialize: protocol version, server info, capabilities.
    let resp = s.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = &resp["result"];
    assert!(r["protocolVersion"].is_string(), "{resp}");
    assert_eq!(r["serverInfo"]["name"], "clustervision", "{resp}");
    assert!(r["capabilities"]["tools"].is_object(), "{resp}");

    // The initialized notification must produce no response — the next reply must be for id 2.
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let resp = s.request(2, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");

    // tools/list: every tool is named, described, and carries a valid object schema whose
    // `required` names actually exist in `properties`.
    let resp = s.request(3, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(tools.len() >= 5, "expected the full toolset, got {}", tools.len());
    let mut names = std::collections::HashSet::new();
    for t in tools {
        let name = t["name"].as_str().unwrap_or_else(|| panic!("unnamed tool: {t}"));
        assert!(names.insert(name.to_string()), "duplicate tool name {name}");
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{name} needs a description"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: inputSchema must be type:object");
        let props = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name}: properties must be an object"));
        for (pname, p) in props {
            assert!(p["type"].is_string(), "{name}.{pname}: every property needs a type");
            assert!(
                !p["description"].as_str().unwrap_or("").is_empty(),
                "{name}.{pname}: every property needs a description"
            );
        }
        if let Some(req) = schema["required"].as_array() {
            for r in req {
                let r = r.as_str().expect("required entries are strings");
                assert!(props.contains_key(r), "{name}: required {r:?} missing from properties");
            }
        }
    }
    // Generated from the stub CLI's Read group…
    for expected in ["ls", "show", "cat", "search"] {
        assert!(
            names.contains(expected),
            "missing CLI-generated tool {expected}: {names:?}"
        );
    }
    // …alongside the hand-written MCP-only tools, which have no CLI equivalent.
    for expected in ["observe_stream", "await_omen", "board_post", "board_claim", "task_open"] {
        assert!(names.contains(expected), "missing MCP-only tool {expected}");
    }
    // `Fleet & live` commands are NOT generated — their MCP tools are the hand-written ones.
    assert!(!names.contains("task"), "`task` is a dispatcher, not a tool: {names:?}");
    let task_open = tools.iter().find(|t| t["name"] == "task_open").unwrap();
    assert!(
        task_open["description"].as_str().unwrap().contains("dispatch object"),
        "task_open must be the hand-written tool, not a generated `cv task open`: {task_open}"
    );
    // The 0.11.0 removals stay removed: their replacements are show/search/ls/pack/prune/cat.
    for gone in [
        "list_sessions",
        "search_sessions",
        "read_session",
        "project_sessions",
        "recall",
        "prune_session",
        "prune_retrieve",
    ] {
        assert!(
            !names.contains(gone),
            "{gone} was removed in 0.11.0 but is still advertised"
        );
    }

    // `show` advertises its MCP window defaults, and `--json` is never the caller's to set.
    let show = tools.iter().find(|t| t["name"] == "show").unwrap();
    let desc = show["description"].as_str().unwrap();
    assert!(desc.contains("LAST 50") && desc.contains("200000"), "{desc}");
    let props = &show["inputSchema"]["properties"];
    assert!(props.get("json").is_none(), "json must not be a property: {props}");
    assert_eq!(props["last"]["type"], "integer", "{props}");
    assert_eq!(show["inputSchema"]["required"], json!(["id"]), "{show}");
}

/// The generated tools are real subprocess calls: the argv cv-mcp builds is what `cv` receives,
/// and a non-zero exit comes back as an MCP tool error carrying the child's stderr.
#[test]
fn generated_tools_shell_out_to_cv() {
    let mut s = Server::spawn("generated");
    s.request(1, "initialize", json!({}));

    // No window selector → the MCP defaults, and `--json` because `show` has the flag.
    let (text, is_err) = s.call_tool(2, "show", json!({"id": "alphasess"}));
    assert!(!is_err, "{text}");
    assert_eq!(
        text.trim(),
        "ARGV show alphasess --last 50 --max-bytes 200000 --json",
        "{text}"
    );

    // An explicit selector wins; a clap id becomes its kebab-case long flag.
    let (text, _) = s.call_tool(3, "show", json!({"id": "alphasess", "around": 12, "context": 2}));
    assert_eq!(
        text.trim(),
        "ARGV show alphasess --around 12 --context 2 --json",
        "{text}"
    );
    let (text, _) = s.call_tool(4, "show", json!({"id": "alphasess", "max_bytes": 4096}));
    assert_eq!(text.trim(), "ARGV show alphasess --max-bytes 4096 --json", "{text}");

    // A boolean flag is present only when true; positionals lead in declaration order; `cat` has
    // no JSON form, so no `--json` is appended.
    let (text, _) = s.call_tool(
        5,
        "cat",
        json!({"tool_use_id": "toolu_42", "session": "alphasess", "input": true}),
    );
    assert_eq!(text.trim(), "ARGV cat alphasess toolu_42 --input", "{text}");
    let (text, _) = s.call_tool(6, "cat", json!({"session": "alphasess", "tool_use_id": "toolu_42"}));
    assert_eq!(text.trim(), "ARGV cat alphasess toolu_42", "{text}");

    // A non-zero exit becomes an isError tool result carrying stderr.
    let (text, is_err) = s.call_tool(7, "show", json!({"id": "nosuch"}));
    assert!(is_err, "a failing `cv` must be a tool error: {text}");
    assert!(
        text.contains("no session found"),
        "stderr must reach the caller: {text}"
    );

    // A missing required positional and an unknown argument are the caller's protocol mistakes.
    let resp = s.request(8, "tools/call", json!({"name": "cat", "arguments": {"session": "a"}}));
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(
        resp["error"]["message"].as_str().unwrap().contains("tool_use_id"),
        "{resp}"
    );
    let resp = s.request(
        9,
        "tools/call",
        json!({"name": "show", "arguments": {"id": "a", "limit": 5}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    assert!(resp["error"]["message"].as_str().unwrap().contains("limit"), "{resp}");
}

#[test]
fn tool_calls_against_fixture_corpus() {
    let mut s = Server::spawn("tools");
    s.request(1, "initialize", json!({}));

    // board round-trip: post → read.
    let (text, is_err) = s.call_tool(12, "board_post", json!({"channel": "testchan", "body": "hello fleet"}));
    assert!(!is_err, "{text}");
    let posted: Value = serde_json::from_str(&text).unwrap();
    assert!(posted["id"].is_string(), "{text}");
    let (text, _) = s.call_tool(13, "board_read", json!({"channel": "testchan"}));
    let msgs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(msgs.as_array().unwrap().len(), 1, "{text}");
    assert_eq!(msgs[0]["body"], "hello fleet", "{text}");

    // board_claim: granted, then contended for another owner, then released.
    let (text, _) = s.call_tool(
        14,
        "board_claim",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["granted"], true, "{text}");
    let (text, _) = s.call_tool(
        15,
        "board_claim",
        json!({"channel": "testchan", "key": "taskA", "from": "other"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["granted"], false, "{text}");
    let (text, _) = s.call_tool(
        16,
        "board_release",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["released"], true, "{text}");
    // Releasing a key you don't hold reports false (idempotent), never an error.
    let (text, is_err) = s.call_tool(
        17,
        "board_release",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["released"], false, "{text}");
}

/// The task tools drive the durable store end-to-end over the protocol: open → two claimants
/// race (first writer wins; the loser gets a rejection, not a duplicate) → done. Also pins the
/// task row shapes (no timestamps on MCP rows), the inbox view, the identity-bearing-verb
/// refusal, and the `--state` vocabulary rejection.
#[test]
fn task_tools_open_claim_race_done() {
    let mut s = Server::spawn("tasks");
    s.request(1, "initialize", json!({}));

    // Open. The returned event carries the durable task id.
    let (text, is_err) = s.call_tool(
        2,
        "task_open",
        json!({"title": "carve the totem", "from": "agent:alpha"}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("task_open returns JSON");
    let id = v["event"]["task_id"].as_str().expect("task id").to_string();
    assert_eq!(v["effective_state"], "open", "{text}");

    // First claimant wins…
    let (text, is_err) = s.call_tool(3, "task_claim", json!({"id": id, "from": "agent:alpha"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["effective_state"], "claimed", "{text}");

    // …the second gets a rejection from the store's locked CAS, not a duplicate claim.
    let (text, is_err) = s.call_tool(4, "task_claim", json!({"id": id, "from": "agent:beta"}));
    assert!(is_err, "the second claimant must lose: {text}");
    assert!(text.contains("rejected"), "{text}");

    // Identity-bearing verbs refuse to act namelessly (no `from`, no CV_ENDPOINT in the env).
    let (text, is_err) = s.call_tool(5, "task_release", json!({"id": id}));
    assert!(is_err, "{text}");
    assert!(text.contains("CV_ENDPOINT"), "the error must name the cure: {text}");

    // The winner's inbox carries the claim (shared row shape); the loser's stays empty.
    let (text, is_err) = s.call_tool(6, "task_inbox", json!({"who": "agent:alpha"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let inbox = v["inbox"].as_array().expect("inbox array");
    assert_eq!(inbox.len(), 1, "{text}");
    assert_eq!(inbox[0]["id"], json!(id), "{text}");
    assert_eq!(inbox[0]["reason"], "claimed_by_you", "{text}");
    assert_eq!(inbox[0]["effective_state"], "claimed", "{text}");
    assert!(inbox[0].get("since").is_none(), "since stays off the wire: {text}");
    let (text, _) = s.call_tool(7, "task_inbox", json!({"who": "agent:beta"}));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["inbox"].as_array().unwrap().len(), 0, "{text}");

    // An unknown state filter errors naming the vocabulary — never a silent empty list.
    let (text, is_err) = s.call_tool(8, "task_list", json!({"state": "redy"}));
    assert!(is_err, "{text}");
    assert!(
        text.contains("redy") && text.contains("awaiting_review"),
        "the error names the typo and the vocabulary: {text}"
    );

    // Done closes it: hidden from the default list, visible with `all`.
    let (text, is_err) = s.call_tool(9, "task_done", json!({"id": id, "from": "agent:alpha"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["effective_state"], "done", "{text}");
    let (text, _) = s.call_tool(10, "task_list", json!({}));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["tasks"].as_array().unwrap().len(), 0, "terminal task hidden: {text}");
    let (text, _) = s.call_tool(11, "task_list", json!({"all": true}));
    let v: Value = serde_json::from_str(&text).unwrap();
    let rows = v["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{text}");
    assert_eq!(rows[0]["id"], json!(id), "{text}");
    assert_eq!(rows[0]["effective_state"], "done", "{text}");
    assert!(
        rows[0].get("opened_at").is_none(),
        "MCP rows ship no timestamps: {text}"
    );

    // Debt: the shared report envelope, loud about the never-run verifier.
    let (text, is_err) = s.call_tool(12, "task_debt", json!({}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v["debt"].as_array().unwrap().is_empty(), "{text}");
    assert!(v["suspects"].as_array().unwrap().is_empty(), "{text}");
    assert!(
        v["verify_warning"].as_str().unwrap_or_default().contains("NEVER"),
        "{text}"
    );
}

#[test]
fn protocol_error_handling() {
    let mut s = Server::spawn("errors");
    s.request(1, "initialize", json!({}));

    // Unknown method with an id → -32601.
    let resp = s.request(2, "wizardry/cast", json!({}));
    assert_eq!(resp["error"]["code"], -32601, "{resp}");

    // Unparseable line → -32700 with a null id (and the server survives).
    s.send_raw("this is not json {");
    let resp = s.recv();
    assert_eq!(resp["error"]["code"], -32700, "{resp}");
    assert!(resp["id"].is_null(), "{resp}");

    // Unknown tool → a tool-level error result, not a protocol error.
    let (text, is_err) = s.call_tool(3, "tools_that_do_not_exist", json!({}));
    assert!(is_err, "{text}");
    assert!(text.contains("unknown tool"), "{text}");

    // A *notification* (no id) must never get a response — even for a known method. The next
    // line on the wire after the notifications must be the reply to the request that follows.
    s.send(&json!({"jsonrpc": "2.0", "method": "ping"}));
    s.send(&json!({"jsonrpc": "2.0", "method": "tools/list"}));
    let resp = s.request(4, "ping", json!({}));
    assert_eq!(
        resp["id"],
        json!(4),
        "a notification (no id) must not produce a response; got an extra reply: {resp}"
    );

    // Still alive and well after all of that.
    let resp = s.request(5, "tools/list", json!({}));
    assert!(resp["result"]["tools"].is_array(), "{resp}");
}

/// Requests are dispatched concurrently: a parked long-poll (board_await on a channel nobody
/// posts to) must not head-of-line-block a fast request issued after it. Responses come back out
/// of order — the fast one first — and both complete well under the long-poll's timeout budget.
#[test]
fn slow_long_poll_does_not_block_other_requests() {
    let mut s = Server::spawn("concurrent");
    s.request(1, "initialize", json!({}));

    let start = std::time::Instant::now();
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "board_await",
        "arguments": {"channel": "quiet", "regex": "NEVER_MATCHES", "timeout_secs": 6, "interval_secs": 1}}}));
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "board_read", "arguments": {"channel": "quiet"}}}));

    let first = s.recv();
    let elapsed = start.elapsed();
    assert_eq!(first["id"], json!(3), "the fast request must answer first: {first}");
    assert!(
        elapsed < Duration::from_secs(4),
        "board_read took {elapsed:?} — blocked behind the 6s long-poll?"
    );
    assert_eq!(first["result"]["isError"], false, "{first}");

    // The long-poll still completes on its own schedule (timed out, unmatched).
    let second = s.recv();
    assert_eq!(second["id"], json!(2), "{second}");
    let text = second["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("\"timed_out\": true"), "{second}");
}

/// A present-but-malformed numeric argument is the caller's protocol mistake: JSON-RPC `-32602
/// Invalid params`, not a silent fallback to the default (and not an isError tool result). An
/// integral float is tolerated — clients often serialize `1` as `1.0`.
#[test]
fn malformed_numeric_args_are_invalid_params() {
    let mut s = Server::spawn("badparams");
    s.request(1, "initialize", json!({}));

    for (id, bad) in [(2u64, json!("ten")), (3, json!(-3)), (4, json!(1.5))] {
        let resp = s.request(
            id,
            "tools/call",
            json!({"name": "board_read", "arguments": {"channel": "c", "limit": bad}}),
        );
        assert_eq!(resp["error"]["code"], -32602, "limit={bad}: {resp}");
        assert!(
            resp["error"]["message"].as_str().unwrap_or_default().contains("limit"),
            "the error must name the offending argument: {resp}"
        );
        assert!(resp["result"].is_null(), "an error response carries no result: {resp}");
    }

    // Integral float: accepted, behaves like the integer — on the hand-written guard…
    let (text, is_err) = s.call_tool(5, "board_read", json!({"channel": "c", "limit": 1.0}));
    assert!(!is_err, "{text}");
    // …and on the generated path, which renders it for clap without the `.0`.
    let (text, is_err) = s.call_tool(9, "show", json!({"id": "a", "last": 20.0}));
    assert!(!is_err, "{text}");
    assert_eq!(text.trim(), "ARGV show a --last 20 --json", "{text}");

    // Other tools route through the same guard (spot-check a long-poll's timeout).
    let resp = s.request(
        6,
        "tools/call",
        json!({"name": "board_await", "arguments": {"channel": "c", "regex": ".", "timeout_secs": "soon"}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // Still alive afterwards.
    let resp = s.request(7, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");
}

/// `observe_stream` is the non-blocking tail of `await_omen`: bounded, read-only, cursor-driven.
/// Covers the spec's required cases — an absent/empty corpus yields a bounded EMPTY result, the
/// baseline call emits no backlog, the cursor drains only newly-appended messages, max_messages
/// bounds the batch (more_pending), and NO board side-effect is produced.
#[test]
fn observe_stream_bounded_read_only_tail() {
    let mut s = Server::spawn("observe");
    s.request(1, "initialize", json!({}));

    // 1) Empty corpus (a filter that matches NO session) → a bounded, empty, baseline result.
    //    No crash, no error, an empty `messages`, and a usable cursor.
    let (text, is_err) = s.call_tool(2, "observe_stream", json!({"cwd_contains": "/no/such/dir"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("observe_stream returns JSON");
    assert_eq!(v["baseline"], true, "first call is a baseline: {text}");
    assert_eq!(v["count"], 0, "absent corpus → zero messages: {text}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{text}");
    assert_eq!(v["more_pending"], false, "{text}");
    assert!(
        v["cursor"].as_str().unwrap().contains("\"v\":1"),
        "cursor is versioned: {text}"
    );

    // 2) Baseline over the REAL fixture corpus emits no backlog (only future activity is tailed),
    //    and hands back a cursor recording the current tail.
    let (text, is_err) = s.call_tool(3, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], true, "{text}");
    assert_eq!(v["count"], 0, "baseline emits no history: {text}");
    let cursor = v["cursor"].as_str().unwrap().to_string();

    // 3) Replaying that cursor with no new activity drains nothing (and is no longer a baseline).
    let (text, is_err) = s.call_tool(
        4,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], false, "a cursor call is not a baseline: {text}");
    assert_eq!(v["count"], 0, "no new activity → nothing drained: {text}");

    // 4) Append a NEW message to the alpha session on disk; the next cursor call drains exactly it.
    let proj = s.base.join("home/.claude/projects/-work-proj");
    let extra = json!({"type": "assistant", "uuid": "a2",
                       "timestamp": "2026-06-19T12:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": "ZEBRA_TAIL_MARKER appended later"}]}});
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    body.push_str(&format!("{extra}\n"));
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();

    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")),
        "the appended message must be drained: {text}"
    );
    assert!(
        msgs.iter()
            .all(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")
                || !m["text"].as_str().unwrap_or("").contains("zebrafish")),
        "previously-seen messages must NOT re-emit: {text}"
    );

    // 5) max_messages bounds the batch. Append several messages, baseline-reset, then drain with a
    //    cap of 1 and assert more_pending is flagged.
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("burst{i}"),
                       "timestamp": "2026-06-19T13:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("burst message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), &body).unwrap();
    // Fresh baseline so we know exactly what's pending, then append AFTER baselining.
    let (text, _) = s.call_tool(6, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    let base2 = serde_json::from_str::<Value>(&text).unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("after{i}"),
                       "timestamp": "2026-06-19T14:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("after message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();
    let (text, is_err) = s.call_tool(
        7,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": base2, "max_messages": 1}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["count"], 1, "max_messages=1 bounds the batch: {text}");
    assert_eq!(v["more_pending"], true, "the rest must be flagged pending: {text}");

    // 6) READ-ONLY: observe_stream must NEVER write the board. Reading the channel observe_stream
    //    was scoped to ("/work/proj"/"fleet") returns nothing it could have posted. Assert the
    //    board has no observe_stream-authored traffic on a fresh channel.
    let (text, _) = s.call_tool(8, "board_read", json!({"channel": "fleet"}));
    let msgs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        msgs.as_array().unwrap().len(),
        0,
        "observe_stream must not write the board (fleet channel must be empty): {text}"
    );
}
