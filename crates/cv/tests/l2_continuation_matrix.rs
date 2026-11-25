//! L2 continuation: a session `cv port` wrote into a harness's store is continued by that
//! harness itself, against a loopback provider that records what the harness sent.
//!
//! Always on: the `[load]` table of each of the five manifests, and the loopback provider on all
//! four wires, with requests built and read back through `cv_core::harness::wire`.
//!
//! Real binaries: `CV_L2=1` (or `CV_L2=claude,codex`) runs `cv port` -> the harness's own resume
//! command from `[load]` -> the loopback. Each harness runs in a scratch `$HOME` with every store
//! variable from `store_env` inside it, a dummy key, and on macOS under `sandbox-exec` with writes
//! confined to the scratch directory and network to localhost. A harness not on `PATH`, or one
//! `cv port` cannot write, is skipped with the reason on stderr.

use cv_core::formats::{self, Load, LoadVars, Protocol};
use cv_core::harness::wire::{anthropic, openai};
use cv_core::ir::{Block, Harness};
use cv_testkit::loopback::{self, Loopback, Recorded, Wire};
use cv_testkit::{claude, http, World};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SOURCE_ID: &str = "6c1f2a3b-4d5e-4f60-8a71-92b3c4d5e6f7";
const FIRST: &str = "Run the l2demo tests and report the count.";
const COMMAND: &str = "cargo test -p l2demo";
const OUTPUT: &str = "test result: ok. 3 passed; 0 failed";
const SAID: &str = "All 3 l2demo tests pass.";
const PROMPT: &str = "cv-l2 continuation probe: answer with one word.";
const SENTINEL: &str = "CV-L2-LOOPBACK-SENTINEL";
const CALL_ID: &str = "toolu_01L2demoCall";

/// The five harnesses and the provider wire each continues on.
const MATRIX: &[(Harness, Protocol)] = &[
    (Harness::Claude, Protocol::AnthropicMessages),
    (Harness::Codex, Protocol::OpenaiResponses),
    (Harness::Gemini, Protocol::GeminiGenerateContent),
    (Harness::OpenCode, Protocol::AnthropicMessages),
    (Harness::Antigravity, Protocol::AntigravityGateway),
];

fn wire_of(protocol: Protocol) -> Wire {
    match protocol {
        Protocol::AnthropicMessages => Wire::AnthropicMessages,
        Protocol::OpenaiResponses => Wire::OpenaiResponses,
        Protocol::OpenaiChat => Wire::OpenaiChat,
        Protocol::GeminiGenerateContent | Protocol::AntigravityGateway => Wire::Gemini,
    }
}

fn load(h: Harness) -> Load {
    formats::manifest(h)
        .unwrap_or_else(|e| panic!("{h}: {e:#}"))
        .load
        .unwrap_or_else(|| panic!("formats/{h}.toml has no [load]"))
}

// ───────────────────────────── what a harness sent ─────────────────────────────

/// A request body read back into one shape for every wire.
#[derive(Debug, Default)]
struct Sent {
    /// Every text the model would see: system, user and assistant.
    texts: Vec<String>,
    /// `(call id, tool name, input)`.
    calls: Vec<(String, String, Value)>,
    /// `(call id, output text)`.
    results: Vec<(String, String)>,
    /// The text of the last user turn that is not only tool results.
    last_user: String,
}

impl Sent {
    fn read(wire: Wire, body: &Value) -> Sent {
        let mut sent = Sent::default();
        match wire {
            Wire::AnthropicMessages => sent.anthropic(body),
            Wire::OpenaiResponses => sent.responses(body),
            Wire::OpenaiChat => sent.chat(body),
            Wire::Gemini => sent.gemini(body),
        }
        sent
    }

    fn user_turn(&mut self, text: String) {
        if !text.is_empty() {
            self.last_user = text.clone();
            self.texts.push(text);
        }
    }

    fn anthropic(&mut self, body: &Value) {
        self.texts.extend(parts_text(&body["system"]));
        for m in body["messages"].as_array().into_iter().flatten() {
            let mut user = String::new();
            let items = match &m["content"] {
                Value::String(s) => vec![json!({"type": "text", "text": s})],
                Value::Array(items) => items.clone(),
                _ => Vec::new(),
            };
            for item in &items {
                match item["type"].as_str() {
                    Some("text") if m["role"] == "user" => user.push_str(item["text"].as_str().unwrap_or("")),
                    Some("text") => self.texts.push(item["text"].as_str().unwrap_or("").into()),
                    Some("tool_use") => {
                        self.calls
                            .push((str_of(&item["id"]), str_of(&item["name"]), item["input"].clone()))
                    }
                    Some("tool_result") => self
                        .results
                        .push((str_of(&item["tool_use_id"]), parts_text(&item["content"]).join("\n"))),
                    _ => {}
                }
            }
            self.user_turn(user);
        }
    }

    fn responses(&mut self, body: &Value) {
        self.texts.extend(parts_text(&body["instructions"]));
        for item in body["input"].as_array().into_iter().flatten() {
            match item["type"].as_str().unwrap_or("message") {
                "message" => {
                    let text = parts_text(&item["content"]).join("\n");
                    match item["role"].as_str() {
                        Some("user") => self.user_turn(text),
                        _ => self.texts.push(text),
                    }
                }
                "function_call" => self.calls.push((
                    str_of(&item["call_id"]),
                    str_of(&item["name"]),
                    openai::parse_arguments(item.get("arguments")),
                )),
                "custom_tool_call" => {
                    self.calls
                        .push((str_of(&item["call_id"]), str_of(&item["name"]), item["input"].clone()))
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let output = match &item["output"] {
                        Value::Object(o) => parts_text(o.get("content").unwrap_or(&Value::Null)).join("\n"),
                        other => parts_text(other).join("\n"),
                    };
                    self.results.push((str_of(&item["call_id"]), output));
                }
                _ => {}
            }
        }
    }

    fn chat(&mut self, body: &Value) {
        for m in body["messages"].as_array().into_iter().flatten() {
            let text = parts_text(&m["content"]).join("\n");
            match m["role"].as_str() {
                Some("user") => self.user_turn(text),
                Some("tool") => self.results.push((str_of(&m["tool_call_id"]), text)),
                _ => self.texts.push(text),
            }
            for call in openai::tool_calls(m.get("tool_calls")).expect("tool_calls decode") {
                if let Block::ToolUse { id, name, input, .. } = call {
                    self.calls.push((id, name, input.resolve().unwrap().into_owned()));
                }
            }
        }
    }

    fn gemini(&mut self, body: &Value) {
        self.texts.extend(parts_text(&body["systemInstruction"]["parts"]));
        for c in body["contents"].as_array().into_iter().flatten() {
            let mut user = String::new();
            for part in c["parts"].as_array().into_iter().flatten() {
                if let Some(text) = part["text"].as_str() {
                    if c["role"] == "user" {
                        user.push_str(text);
                    } else {
                        self.texts.push(text.into());
                    }
                }
                let call = &part["functionCall"];
                if call.is_object() {
                    let id = call["id"].as_str().map_or_else(|| str_of(&call["name"]), str::to_owned);
                    self.calls.push((id, str_of(&call["name"]), call["args"].clone()));
                }
                let response = &part["functionResponse"];
                if response.is_object() {
                    let id = response["id"]
                        .as_str()
                        .map_or_else(|| str_of(&response["name"]), str::to_owned);
                    self.results.push((id, response["response"].to_string()));
                }
            }
            self.user_turn(user);
        }
    }

    /// The source conversation arrived whole: the first prompt, the call and its paired result,
    /// the agent's reply, the working directory, and the new prompt last.
    /// `seeded`: the target holds the source as one seed prompt (the adapter's `accepts_seed`), so
    /// the call and its result arrive as rendered text rather than as a paired tool exchange.
    fn assert_continues_source(&self, raw: &Value, cwd: &str, what: &str, seeded: bool) {
        let all = self.texts.join("\n");
        assert!(all.contains(FIRST), "{what}: first prompt missing:\n{self:#?}");
        assert!(all.contains(SAID), "{what}: agent reply missing:\n{self:#?}");
        if seeded {
            assert!(
                all.contains(COMMAND) && all.contains(OUTPUT),
                "{what}: the seed lacks the call or its output:\n{self:#?}"
            );
        } else {
            self.assert_paired_call(what);
        }
        assert!(
            self.last_user.contains(PROMPT),
            "{what}: the new prompt is not the last user turn:\n{self:#?}"
        );
        // The harness tells the model where it runs; that is how the cwd travels.
        assert!(
            raw.to_string().contains(&json_escaped(cwd)),
            "{what}: the working directory {cwd} never reached the model"
        );
    }

    fn assert_paired_call(&self, what: &str) {
        let call = self
            .calls
            .iter()
            .find(|(_, _, input)| input.to_string().contains(COMMAND))
            .unwrap_or_else(|| panic!("{what}: no call runs {COMMAND:?}:\n{self:#?}"));
        assert!(
            self.results
                .iter()
                .any(|(id, out)| id == &call.0 && out.contains(OUTPUT)),
            "{what}: call {} has no paired result with the output:\n{self:#?}",
            call.0
        );
    }
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn json_escaped(s: &str) -> String {
    let quoted = Value::String(s.into()).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

/// The texts of a string, a `{text}` part, or an array of either.
fn parts_text(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(parts_text).collect(),
        Value::Object(o) => o
            .get("text")
            .and_then(Value::as_str)
            .map(|t| vec![t.to_owned()])
            .or_else(|| o.get("parts").map(parts_text))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

// ───────────────────────────── always on ─────────────────────────────

#[test]
fn load_tables_name_a_resumable_loopback_invocation() {
    let vars = LoadVars {
        session: "sess-0001",
        prompt: PROMPT,
        base_url: "http://127.0.0.1:9",
        api_key: "loopback-key",
        home: "/scratch/home",
    };
    for &(h, protocol) in MATRIX {
        let load = load(h);
        load.validate().unwrap_or_else(|e| panic!("{h}: {e:#}"));
        assert_eq!(load.protocol, protocol, "{h}");
        assert!(cv_core::emit::supported_targets().contains(&h), "{h}: emit target");
        let argv = load.argv_with(&vars);
        let env = load.env_with(&vars);
        assert!(argv.iter().any(|a| a == "sess-0001"), "{h}: {argv:?}");
        assert!(argv.iter().any(|a| a == PROMPT), "{h}: {argv:?}");
        let everything = format!("{argv:?}{env:?}");
        assert!(everything.contains("http://127.0.0.1:9"), "{h}: provider not addressed");
        assert!(!everything.contains("${"), "{h}: unexpanded placeholder: {everything}");
        for key in load.store_env.keys() {
            let (_, value) = env.iter().find(|(k, _)| k == key).unwrap();
            assert!(
                value.starts_with("/scratch/home"),
                "{h}: {key}={value} escapes the scratch home"
            );
        }
    }
    // A harness that declares [load] is in the matrix, so it gets a real-binary test.
    for (h, m) in formats::manifests() {
        if m.unwrap().load.is_some() {
            assert!(
                MATRIX.iter().any(|(listed, _)| *listed == h),
                "{h} has [load] but no l2 test"
            );
        }
    }
    // JSON inside a value is literal; only `${name}` expands.
    let opencode = load(Harness::OpenCode).env_with(&vars);
    let (_, config) = opencode.iter().find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").unwrap();
    let config: Value = serde_json::from_str(config).expect("OPENCODE_CONFIG_CONTENT is JSON");
    assert_eq!(
        config["provider"]["anthropic"]["options"]["baseURL"],
        "http://127.0.0.1:9/v1"
    );
}

/// The source turns as IR blocks, so a request can be written by the wire codec itself.
fn source_blocks() -> (Block, Block, Block, Block) {
    let call = Block::ToolUse {
        id: CALL_ID.into(),
        name: "Bash".into(),
        input: json!({"command": COMMAND, "description": "Run tests"}).into(),
        namespace: None,
        extensions: Default::default(),
    };
    let result = Block::ToolResult {
        tool_use_id: CALL_ID.into(),
        content: OUTPUT.into(),
        is_error: false,
        tool_name: None,
        status: None,
        details: None,
        extensions: Default::default(),
    };
    (
        Block::from_text(FIRST.into()),
        call,
        result,
        Block::from_text(SAID.into()),
    )
}

/// What each wire's client sends to continue the source conversation with [`PROMPT`].
fn request_for(wire: Wire, cwd: &str, stream: bool) -> Value {
    let (_first, call, result, said) = source_blocks();
    let system = format!("You are a coding agent. Primary working directory: {cwd}");
    match wire {
        Wire::AnthropicMessages => json!({
            "model": "claude-loopback", "stream": stream, "max_tokens": 64,
            "system": [{"type": "text", "text": system}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": FIRST}]},
                {"role": "assistant", "content": anthropic::assistant_blocks(&[call], &anthropic::ANTHROPIC).unwrap()},
                {"role": "user", "content": anthropic::tool_result_blocks(&[result]).unwrap()},
                {"role": "assistant", "content": anthropic::assistant_blocks(&[said], &anthropic::ANTHROPIC).unwrap()},
                {"role": "user", "content": PROMPT},
            ],
        }),
        Wire::OpenaiResponses => {
            let Block::ToolUse { input, .. } = &call else {
                unreachable!()
            };
            json!({
                "model": "gpt-loopback", "stream": stream, "instructions": system,
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": FIRST}]},
                    {"type": "function_call", "call_id": CALL_ID, "name": "shell", "arguments": input.resolve().unwrap().into_owned().to_string()},
                    {"type": "function_call_output", "call_id": CALL_ID, "output": OUTPUT},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": SAID}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": PROMPT}]},
                ],
            })
        }
        Wire::OpenaiChat => {
            let Block::ToolUse { input, .. } = &call else {
                unreachable!()
            };
            json!({
                "model": "gpt-loopback", "stream": stream,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": FIRST},
                    {"role": "assistant", "content": null, "tool_calls": [{"id": CALL_ID, "type": "function",
                        "function": {"name": "Bash", "arguments": input.resolve().unwrap().into_owned().to_string()}}]},
                    {"role": "tool", "tool_call_id": CALL_ID, "content": OUTPUT},
                    {"role": "assistant", "content": SAID},
                    {"role": "user", "content": [{"type": "text", "text": PROMPT}]},
                ],
            })
        }
        Wire::Gemini => json!({
            "systemInstruction": {"parts": [{"text": system}]},
            "contents": [
                {"role": "user", "parts": [{"text": FIRST}]},
                {"role": "model", "parts": [{"functionCall": {"id": CALL_ID, "name": "run_shell_command", "args": {"command": COMMAND}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": CALL_ID, "name": "run_shell_command", "response": {"output": OUTPUT}}}]},
                {"role": "model", "parts": [{"text": SAID}]},
                {"role": "user", "parts": [{"text": PROMPT}]},
            ],
        }),
    }
}

#[test]
fn loopback_answers_every_wire_and_records_the_continued_conversation() {
    let lb = Loopback::start(SENTINEL);
    let cwd = "/scratch/project \"quoted\"";
    let cases: &[(Wire, &str, bool)] = &[
        (Wire::AnthropicMessages, "/v1/messages", false),
        (Wire::AnthropicMessages, "/v1/messages?beta=true", true),
        (Wire::OpenaiResponses, "/v1/responses", false),
        (Wire::OpenaiResponses, "/v1/responses", true),
        (Wire::OpenaiChat, "/v1/chat/completions", false),
        (Wire::OpenaiChat, "/v1/chat/completions", true),
        (Wire::Gemini, "/v1beta/models/gemini-loopback:generateContent", false),
        (
            Wire::Gemini,
            "/v1beta/models/gemini-loopback:streamGenerateContent?alt=sse",
            true,
        ),
        (
            Wire::Gemini,
            "/v1beta/models/gemini-loopback:streamGenerateContent",
            true,
        ),
        // Antigravity's gateway: a Gemini body on the gateway's own path, always streamed.
        (Wire::Gemini, "/v1internal:streamGenerateContent", true),
    ];
    for (i, &(wire, path, stream)) in cases.iter().enumerate() {
        let body = request_for(wire, cwd, stream).to_string();
        let raw = http::raw_request(
            &lb.addr(),
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(&body),
        );
        assert_eq!(http::status(&raw), 200, "{path}: {raw}");
        let (head, reply) = http::parts(&raw);
        assert_eq!(
            head.contains("text/event-stream"),
            stream && !(path.contains("/models/") && path.ends_with(":streamGenerateContent")),
            "{path}: {head}"
        );
        assert_eq!(loopback::reply_text(reply), SENTINEL, "{path}: {reply}");

        let recorded = &lb.conversations()[i];
        assert_eq!(recorded.wire, Some(wire), "{path}");
        assert_eq!(recorded.streamed, stream, "{path}");
        assert_eq!(recorded.target, path);
        let body = recorded.body.as_ref().expect("JSON body recorded");
        Sent::read(wire, body).assert_continues_source(body, cwd, path, false);
    }

    // A tool call the codec wrote is read back by the codec: same id, name and input.
    let anthropic_body = lb.conversations()[0].body.clone().unwrap();
    let (_, call, _, _) = source_blocks();
    let expected = anthropic::assistant_blocks(&[call], &anthropic::ANTHROPIC).unwrap();
    assert_eq!(anthropic_body["messages"][1]["content"], json!(expected));
    let chat_body = lb.conversations()[4].body.clone().unwrap();
    let calls = openai::tool_calls(chat_body["messages"][2].get("tool_calls")).unwrap();
    assert!(
        matches!(&calls[..], [Block::ToolUse { id, input, .. }] if id == CALL_ID && input.resolve().unwrap().into_owned()["command"] == COMMAND),
        "{calls:?}"
    );

    // Metadata routes answer without counting as a conversation; unknown routes are 404s.
    let models = http::raw_request(&lb.addr(), "GET", "/v1/models", &[], None);
    assert_eq!(http::status(&models), 200);
    let count = http::raw_request(&lb.addr(), "POST", "/v1/messages/count_tokens", &[], Some("{}"));
    assert_eq!(http::status(&count), 200);
    let missing = http::raw_request(&lb.addr(), "POST", "/v2/unknown", &[], Some("{}"));
    assert_eq!(http::status(&missing), 404);
    let compressed = http::raw_request(
        &lb.addr(),
        "POST",
        "/v1/responses",
        &[("Content-Encoding", "zstd")],
        Some("\u{28}\u{b5}"),
    );
    assert_eq!(http::status(&compressed), 415);
    assert_eq!(lb.conversations().len(), cases.len());
    let all = lb.requests();
    assert_eq!(all.len(), cases.len() + 4);
    assert!(all
        .last()
        .is_some_and(|r: &Recorded| r.body.is_none() && r.header("content-encoding") == Some("zstd")));
}

#[test]
fn loopback_keeps_a_connection_alive_across_requests() {
    use std::io::{BufRead, BufReader, Read, Write};
    let lb = Loopback::start(SENTINEL);
    let stream = std::net::TcpStream::connect(lb.addr()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    for stream_reply in [false, true] {
        let body = request_for(Wire::AnthropicMessages, "/p", stream_reply).to_string();
        // Chunked, as a streaming client uploads.
        write!(
            writer,
            "POST /v1/messages HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        )
        .unwrap();
        let mut length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
        }
        let mut reply = vec![0; length];
        reader.read_exact(&mut reply).unwrap();
        assert_eq!(loopback::reply_text(std::str::from_utf8(&reply).unwrap()), SENTINEL);
    }
    assert!(lb.wait_until(Duration::from_secs(5), |r| r.len() == 2));
    assert!(lb
        .conversations()
        .iter()
        .all(|r| r.header("transfer-encoding") == Some("chunked")));
}

// ───────────────────────────── real binaries ─────────────────────────────

/// Why `h` does not run here, or `None` when it does.
fn gate(h: Harness, load: &Load) -> Option<String> {
    let selected = std::env::var("CV_L2").unwrap_or_default();
    let names: Vec<&str> = selected.split(',').map(str::trim).collect();
    if !(names.contains(&"1") || names.contains(&"all") || names.contains(&h.as_str())) {
        return Some("set CV_L2=1 (or a list of harnesses) to run real binaries".into());
    }
    if cfg!(not(unix)) {
        return Some("real-binary runs need a unix process group".into());
    }
    if which(&load.binary).is_none() {
        return Some(format!("{} is not on PATH", load.binary));
    }
    if !cv_core::emit::supported_targets().contains(&h) {
        return Some(format!("cv port cannot write {h} yet"));
    }
    None
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(binary))
        .find(|p| p.is_file())
}

/// The source conversation as Claude records, in the world's Claude store, at `cwd`.
fn seed_source(w: &World, cwd: &Path) {
    let at = |uuid: &str, row: Value, parent: Option<&str>| {
        claude::with(
            row,
            json!({"cwd": cwd, "sessionId": SOURCE_ID, "parentUuid": parent, "version": "2.1.330", "uuid": uuid}),
        )
    };
    w.write_session(
        SOURCE_ID,
        &[
            at("a1", claude::user(SOURCE_ID, "a1", "2026-11-20T10:00:00Z", FIRST), None),
            at(
                "a2",
                claude::reply(
                    "a2",
                    "2026-11-20T10:00:05Z",
                    json!([claude::tool_use(
                        CALL_ID,
                        "Bash",
                        json!({"command": COMMAND, "description": "Run tests"})
                    )]),
                    json!({"input_tokens": 10, "output_tokens": 5}),
                ),
                Some("a1"),
            ),
            at(
                "a3",
                claude::tool_result("a3", "2026-11-20T10:00:09Z", CALL_ID, OUTPUT),
                Some("a2"),
            ),
            at(
                "a4",
                claude::reply(
                    "a4",
                    "2026-11-20T10:00:12Z",
                    json!([{"type": "text", "text": SAID}]),
                    json!({"input_tokens": 20, "output_tokens": 6}),
                ),
                Some("a3"),
            ),
        ],
    );
}

/// macOS: writes confined to `root` (and the terminal devices), network to localhost only.
fn sandbox_profile(root: &Path) -> String {
    format!(
        "(version 1)(allow default)(deny file-write*)\
         (allow file-write* (subpath {root:?}) (literal \"/dev/null\") (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\"))\
         (deny network*)(allow network* (remote ip \"localhost:*\") (local ip \"localhost:*\"))(allow network* (remote unix-socket))",
        root = root.to_str().unwrap()
    )
}

#[cfg(unix)]
fn run_bounded(mut cmd: std::process::Command, timeout: Duration) -> (Option<i32>, String, String) {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn harness");
    let pid = child.id();
    let (out_pipe, err_pipe) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let read = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = pipe.read_to_end(&mut s);
            String::from_utf8_lossy(&s).into_owned()
        })
    };
    let (out, err) = (read(Box::new(out_pipe)), read(Box::new(err_pipe)));
    let deadline = std::time::Instant::now() + timeout;
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if std::time::Instant::now() >= deadline {
            // Only the process group this test started.
            if let Ok(pgid) = libc::pid_t::try_from(pid) {
                if pgid > 0 {
                    // SAFETY: `cmd.process_group(0)` placed `child` into its own process group `pgid`.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
            }
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (code, out.join().unwrap(), err.join().unwrap())
}

#[cfg(not(unix))]
fn run_bounded(_: std::process::Command, _: Duration) -> (Option<i32>, String, String) {
    unreachable!("gated off")
}

fn l2(h: Harness) {
    let load = load(h);
    if let Some(why) = gate(h, &load) {
        eprintln!("l2 {h}: skipped: {why}");
        return;
    }
    let w = World::new(&format!("l2-{h}"));
    let base = w.base.canonicalize().unwrap();
    let home = base.join("home");
    let project = base.join("project");
    let tmp = base.join("tmp");
    for dir in [&project, &tmp] {
        std::fs::create_dir_all(dir).unwrap();
    }
    seed_source(&w, &project);

    let lb = Loopback::start(SENTINEL);
    let base_url = lb.base_url();
    let home_str = home.to_str().unwrap();
    // The id is not known before the port; the store env does not depend on it.
    let store_vars = LoadVars {
        session: "",
        prompt: PROMPT,
        base_url: &base_url,
        api_key: "loopback-dummy-key",
        home: home_str,
    };
    let store_env: Vec<(String, String)> = load
        .env_with(&store_vars)
        .into_iter()
        .filter(|(k, _)| load.store_env.contains_key(k))
        .collect();
    for dir in store_env
        .iter()
        .map(|(_, d)| d.clone())
        .chain(load.installed_dirs_with(&store_vars))
    {
        std::fs::create_dir_all(dir).unwrap();
    }
    let env_refs: Vec<(&str, &str)> = store_env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (ok, code, out, err) = w.cv_env(&["port", SOURCE_ID, "--harness", h.as_str(), "--json"], &env_refs);
    assert!(ok, "cv port --harness {h} exited {code}\n{out}\n{err}");
    let ported: Value = serde_json::from_str(out.lines().last().unwrap()).expect("port --json");
    let id = ported["id"].as_str().expect("ported id").to_owned();

    let vars = LoadVars {
        session: &id,
        ..store_vars
    };
    let binary = which(&load.binary).unwrap();
    let mut cmd = if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        let mut c = std::process::Command::new("/usr/bin/sandbox-exec");
        c.args(["-p", &sandbox_profile(&base)]).arg(&binary);
        c
    } else {
        std::process::Command::new(&binary)
    };
    cmd.args(load.argv_with(&vars)).current_dir(&project).env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TMPDIR", &tmp)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1");
    cmd.envs(w.env());
    cmd.envs(load.env_with(&vars));
    let (code, stdout, stderr) = run_bounded(cmd, Duration::from_secs(180));
    let report = || {
        let seen: Vec<String> = lb
            .requests()
            .iter()
            .map(|r| format!("{} {} {:?}", r.method, r.target, r.wire))
            .collect();
        format!("exit {code:?}\nrequests: {seen:#?}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    };
    assert_eq!(code, Some(0), "{h} did not continue {id} cleanly\n{}", report());

    let wire = wire_of(load.protocol);
    let continued = lb
        .conversations()
        .into_iter()
        .filter(|r| r.wire == Some(wire))
        .filter_map(|r| r.body.clone())
        .map(|body| (Sent::read(wire, &body), body))
        .filter(|(sent, _)| sent.last_user.contains(PROMPT))
        .max_by_key(|(sent, _)| sent.calls.len())
        .unwrap_or_else(|| panic!("{h}: no {wire:?} request carried the prompt\n{}", report()));
    let seeded = cv_core::harness::rootless(h).is_some_and(|a| a.accepts_seed());
    continued
        .0
        .assert_continues_source(&continued.1, project.to_str().unwrap(), h.as_str(), seeded);

    // The harness appended its continuation to the store, and cv reads it back.
    let (ok, code, md, err) = w.cv_env(&["export", &format!("{h}:{id}"), "--format", "md"], &env_refs);
    assert!(ok, "cv export {h}:{id} exited {code}\n{err}");
    for needle in [FIRST, OUTPUT, PROMPT, SENTINEL] {
        assert!(md.contains(needle), "{h}: continued session lacks {needle:?}:\n{md}");
    }
    eprintln!("l2 {h}: continued {id} ({} provider requests)", lb.requests().len());
}

#[test]
fn l2_claude_continues_a_ported_session() {
    l2(Harness::Claude);
}

#[test]
fn l2_codex_continues_a_ported_session() {
    l2(Harness::Codex);
}

#[test]
fn l2_gemini_continues_a_ported_session() {
    l2(Harness::Gemini);
}

#[test]
fn l2_opencode_continues_a_ported_session() {
    l2(Harness::OpenCode);
}

#[test]
fn l2_antigravity_continues_a_ported_session() {
    l2(Harness::Antigravity);
}
