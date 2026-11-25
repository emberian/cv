//! A loopback model provider: one `127.0.0.1:0` listener that answers the Anthropic Messages,
//! OpenAI Responses, OpenAI Chat Completions and Gemini `generateContent` wires (and Antigravity's
//! gateway, which carries Gemini bodies) with one deterministic assistant reply, and records every
//! request it received so a test can check what a harness sent when it continued a session.
//!
//! Each reply is plain text ending the turn, so a harness appends one assistant message and exits.
//! Requests are HTTP/1.1 with `Content-Length` or chunked bodies, served one thread per
//! connection with keep-alive. A compressed or non-JSON body is recorded with `body: None`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A request body larger than this is refused (the harness's whole conversation fits easily).
const BODY_CAP: usize = 64 * 1024 * 1024;

/// The wire a recorded request was answered on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    AnthropicMessages,
    OpenaiResponses,
    OpenaiChat,
    /// Gemini `contents` bodies, from `generateContent`, `streamGenerateContent` or a gateway.
    Gemini,
}

/// One request as received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// The request target, query included.
    pub target: String,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    /// The body parsed as JSON; `None` when absent, compressed or not JSON.
    pub body: Option<Value>,
    pub body_len: usize,
    /// The conversation wire it was answered on; `None` for metadata requests and 404s.
    pub wire: Option<Wire>,
    /// Whether the reply was streamed (SSE or a Gemini JSON array).
    pub streamed: bool,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

struct State {
    sentinel: String,
    requests: Mutex<Vec<Recorded>>,
    changed: Condvar,
    replies: AtomicU64,
}

/// The running provider; stops and joins its listener on drop.
pub struct Loopback {
    addr: SocketAddr,
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Loopback {
    /// Listen on an ephemeral loopback port; every conversation request is answered with
    /// `sentinel` as the assistant's whole reply.
    pub fn start(sentinel: &str) -> Loopback {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("loopback address");
        let state = Arc::new(State {
            sentinel: sentinel.to_owned(),
            requests: Mutex::new(Vec::new()),
            changed: Condvar::new(),
            replies: AtomicU64::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (state, stop) = (state.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let _ = serve(stream, &state);
                    });
                }
            })
        };
        Loopback {
            addr,
            state,
            stop,
            worker: Some(worker),
        }
    }

    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn addr(&self) -> String {
        self.addr.to_string()
    }

    pub fn sentinel(&self) -> &str {
        &self.state.sentinel
    }

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    /// The requests answered on a conversation wire.
    pub fn conversations(&self) -> Vec<Recorded> {
        self.requests().into_iter().filter(|r| r.wire.is_some()).collect()
    }

    /// Wait until `done` holds for the recorded requests or `timeout` passes; returns `done`'s
    /// last answer.
    pub fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&[Recorded]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut requests = self.state.requests.lock().unwrap();
        loop {
            if done(&requests) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            requests = self.state.changed.wait_timeout(requests, left).unwrap().0;
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the worker sees `stop`.
        let _ = TcpStream::connect(self.addr);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn serve(stream: TcpStream, state: &State) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    while let Some(request) = read_request(&mut reader, &mut writer)? {
        let close = request
            .headers
            .iter()
            .any(|(k, v)| k == "connection" && v.eq_ignore_ascii_case("close"));
        let (recorded, status, content_type, body) = answer(request, state);
        {
            let mut requests = state.requests.lock().unwrap();
            requests.push(recorded);
            state.changed.notify_all();
        }
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n{}\r\n",
            body.len(),
            if close { "Connection: close\r\n" } else { "" },
        );
        writer.write_all(head.as_bytes())?;
        writer.write_all(&body)?;
        writer.flush()?;
        if close {
            break;
        }
    }
    Ok(())
}

fn read_request(reader: &mut BufReader<TcpStream>, writer: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        header_bytes += line.len();
        if header_bytes > 256 * 1024 {
            return Err(std::io::Error::other("request header too large"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut body = Vec::new();
    if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| std::io::Error::other("bad chunk size"))?;
            if size == 0 {
                // Trailers end at a blank line.
                loop {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            if body.len() + size > BODY_CAP {
                return Err(std::io::Error::other("request body too large"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            line.clear();
            reader.read_line(&mut line)?;
        }
    } else if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if len > BODY_CAP {
            return Err(std::io::Error::other("request body too large"));
        }
        body.resize(len, 0);
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

/// Route one request: `(record, status line, content type, body)`.
fn answer(request: Request, state: &State) -> (Recorded, &'static str, &'static str, Vec<u8>) {
    let encoded = request
        .headers
        .iter()
        .any(|(k, v)| k == "content-encoding" && !v.eq_ignore_ascii_case("identity"));
    let body: Option<Value> = (!encoded && !request.body.is_empty())
        .then(|| serde_json::from_slice(&request.body).ok())
        .flatten();
    let path = request.target.split('?').next().unwrap_or("").to_owned();
    let query = request.target.split_once('?').map_or("", |(_, q)| q).to_owned();
    let mut recorded = Recorded {
        method: request.method.clone(),
        target: request.target.clone(),
        headers: request.headers,
        body: body.clone(),
        body_len: request.body.len(),
        wire: None,
        streamed: false,
    };
    let field = |key: &str| body.as_ref().and_then(|b| b.get(key));
    let wants_stream = field("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = field("model").and_then(Value::as_str).unwrap_or("loopback").to_owned();
    let n = state.replies.fetch_add(1, Ordering::SeqCst);
    let text = state.sentinel.as_str();

    if request.method == "GET" || request.method == "HEAD" {
        if path.ends_with("/models") {
            let list = json!({"object": "list", "data": [], "models": []});
            return (recorded, "200 OK", "application/json", list.to_string().into_bytes());
        }
        return not_found(recorded);
    }
    if request.method != "POST" {
        return not_found(recorded);
    }
    if path.ends_with("/messages/count_tokens") {
        let count = json!({"input_tokens": 1});
        return (recorded, "200 OK", "application/json", count.to_string().into_bytes());
    }
    if encoded {
        return (
            recorded,
            "415 Unsupported Media Type",
            "application/json",
            error_body("compressed request body"),
        );
    }
    let wire = if path.ends_with("/messages") {
        Wire::AnthropicMessages
    } else if path.ends_with("/responses") {
        Wire::OpenaiResponses
    } else if path.ends_with("/chat/completions") {
        Wire::OpenaiChat
    } else if path.contains(":generateContent")
        || path.contains(":streamGenerateContent")
        || field("contents").is_some()
    {
        Wire::Gemini
    } else {
        return not_found(recorded);
    };
    if body.is_none() {
        return (
            recorded,
            "400 Bad Request",
            "application/json",
            error_body("expected a JSON body"),
        );
    }
    recorded.wire = Some(wire);
    let (streamed, content_type, reply) = match wire {
        Wire::AnthropicMessages => {
            let id = format!("msg_loopback_{n}");
            if wants_stream {
                (true, "text/event-stream", anthropic_sse(&id, &model, text))
            } else {
                let message = json!({
                    "id": id, "type": "message", "role": "assistant", "model": model,
                    "content": [{"type": "text", "text": text}],
                    "stop_reason": "end_turn", "stop_sequence": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                });
                (false, "application/json", message.to_string())
            }
        }
        Wire::OpenaiResponses => {
            let id = format!("resp_loopback_{n}");
            if wants_stream {
                (true, "text/event-stream", responses_sse(&id, &model, text))
            } else {
                (
                    false,
                    "application/json",
                    responses_object(&id, &model, text).to_string(),
                )
            }
        }
        Wire::OpenaiChat => {
            let id = format!("chatcmpl-loopback-{n}");
            if wants_stream {
                (true, "text/event-stream", chat_sse(&id, &model, text))
            } else {
                let completion = json!({
                    "id": id, "object": "chat.completion", "created": 0, "model": model,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                });
                (false, "application/json", completion.to_string())
            }
        }
        Wire::Gemini => {
            let candidate = json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
                "modelVersion": "loopback",
            });
            if path.contains(":generateContent") {
                (false, "application/json", candidate.to_string())
            } else if path.contains("/models/") && !query.split('&').any(|kv| kv == "alt=sse") {
                // The public API streams a JSON array unless asked for SSE.
                (true, "application/json", json!([candidate]).to_string())
            } else {
                // `alt=sse`, and a gateway route (Antigravity's), which streams SSE.
                (true, "text/event-stream", format!("data: {candidate}\n\n"))
            }
        }
    };
    recorded.streamed = streamed;
    (recorded, "200 OK", content_type, reply.into_bytes())
}

fn not_found(recorded: Recorded) -> (Recorded, &'static str, &'static str, Vec<u8>) {
    (
        recorded,
        "404 Not Found",
        "application/json",
        error_body("no such loopback route"),
    )
}

fn error_body(message: &str) -> Vec<u8> {
    json!({"error": {"type": "invalid_request_error", "message": message}})
        .to_string()
        .into_bytes()
}

fn sse(out: &mut String, event: &str, data: &Value) {
    out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
}

fn anthropic_sse(id: &str, model: &str, text: &str) -> String {
    let mut out = String::new();
    sse(
        &mut out,
        "message_start",
        &json!({"type": "message_start", "message": {
            "id": id, "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 1, "output_tokens": 1},
        }}),
    );
    sse(
        &mut out,
        "content_block_start",
        &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    sse(
        &mut out,
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
    );
    sse(
        &mut out,
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    );
    sse(
        &mut out,
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 1}}),
    );
    sse(&mut out, "message_stop", &json!({"type": "message_stop"}));
    out
}

fn responses_item(id: &str, text: &str, status: &str) -> Value {
    let content = if status == "completed" {
        json!([{"type": "output_text", "text": text, "annotations": []}])
    } else {
        json!([])
    };
    json!({"type": "message", "id": format!("msg_{id}"), "role": "assistant", "status": status, "content": content})
}

fn responses_object(id: &str, model: &str, text: &str) -> Value {
    json!({
        "id": id, "object": "response", "created_at": 0, "status": "completed", "model": model,
        "output": [responses_item(id, text, "completed")],
        "usage": {
            "input_tokens": 1, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 2,
        },
    })
}

fn responses_sse(id: &str, model: &str, text: &str) -> String {
    let mut out = String::new();
    let mut started = responses_object(id, model, text);
    started["status"] = json!("in_progress");
    started["output"] = json!([]);
    let item_id = format!("msg_{id}");
    let events = [
        json!({"type": "response.created", "response": started}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": responses_item(id, text, "in_progress")}),
        json!({"type": "response.content_part.added", "item_id": item_id, "output_index": 0, "content_index": 0,
               "part": {"type": "output_text", "text": "", "annotations": []}}),
        json!({"type": "response.output_text.delta", "item_id": item_id, "output_index": 0, "content_index": 0, "delta": text}),
        json!({"type": "response.output_text.done", "item_id": item_id, "output_index": 0, "content_index": 0, "text": text}),
        json!({"type": "response.content_part.done", "item_id": item_id, "output_index": 0, "content_index": 0,
               "part": {"type": "output_text", "text": text, "annotations": []}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": responses_item(id, text, "completed")}),
        json!({"type": "response.completed", "response": responses_object(id, model, text)}),
    ];
    for (seq, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(seq);
        let name = event["type"].as_str().unwrap_or_default().to_owned();
        sse(&mut out, &name, &event);
    }
    out
}

fn chat_sse(id: &str, model: &str, text: &str) -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": id, "object": "chat.completion.chunk", "created": 0, "model": model,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut out = String::new();
    for data in [
        chunk(json!({"role": "assistant", "content": text}), Value::Null),
        chunk(json!({}), json!("stop")),
    ] {
        out.push_str(&format!("data: {data}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// The assistant text a loopback reply carries, read back from any of its four shapes (JSON or
/// streamed): what a client of the wire would display.
pub fn reply_text(body: &str) -> String {
    let mut out = String::new();
    let mut take = |v: &Value| {
        // Anthropic JSON and SSE deltas.
        for c in v["content"].as_array().into_iter().flatten() {
            out.push_str(c["text"].as_str().unwrap_or(""));
        }
        if v["type"] == "content_block_delta" {
            out.push_str(v["delta"]["text"].as_str().unwrap_or(""));
        }
        // Responses JSON, and the streamed deltas only (the done events repeat the text).
        if v["object"] == "response" {
            for item in v["output"].as_array().into_iter().flatten() {
                for c in item["content"].as_array().into_iter().flatten() {
                    out.push_str(c["text"].as_str().unwrap_or(""));
                }
            }
        }
        if v["type"] == "response.output_text.delta" {
            out.push_str(v["delta"].as_str().unwrap_or(""));
        }
        // Chat JSON and chunks.
        for choice in v["choices"].as_array().into_iter().flatten() {
            out.push_str(choice["message"]["content"].as_str().unwrap_or(""));
            out.push_str(choice["delta"]["content"].as_str().unwrap_or(""));
        }
        // Gemini.
        for cand in v["candidates"].as_array().into_iter().flatten() {
            for part in cand["content"]["parts"].as_array().into_iter().flatten() {
                out.push_str(part["text"].as_str().unwrap_or(""));
            }
        }
    };
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => items.iter().for_each(&mut take),
            Ok(v) => take(&v),
            Err(_) => {}
        }
    } else {
        for line in body.lines() {
            if let Some(Ok(v)) = line.strip_prefix("data: ").map(serde_json::from_str::<Value>) {
                take(&v);
            }
        }
    }
    out
}
