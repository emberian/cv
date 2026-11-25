//! Bounded orientation over the existing message stream, event classifier and worker forest.
//! Excerpts are pointers into evidence, not a summary or a claim that reported work was integrated.
use crate::events::EventExtractor;
use crate::ir::{Block, Message, MessageKind, Origin, Role, Session, SessionRef};
use crate::stream::{Demand, Flow, MessageSink, Pass, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct OverviewOptions {
    /// First human request plus the most recent requests, without duplicates.
    pub request_limit: usize,
    pub file_limit: usize,
    pub command_limit: usize,
    pub worker_limit: usize,
    pub excerpt_bytes: usize,
}
impl Default for OverviewOptions {
    fn default() -> Self {
        Self {
            request_limit: 5,
            file_limit: 12,
            command_limit: 8,
            worker_limit: 12,
            excerpt_bytes: 1200,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Request {
    pub msg_idx: usize,
    pub ts: Option<DateTime<Utc>>,
    pub text: String,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct File {
    pub msg_idx: usize,
    pub ts: Option<DateTime<Utc>>,
    pub kind: &'static str,
    pub path: String,
    /// Current local metadata only; `None` means unavailable, remote or not checked.
    pub present: Option<bool>,
}
#[derive(Debug, Serialize)]
pub struct Command {
    pub msg_idx: usize,
    pub ts: Option<DateTime<Utc>>,
    pub tool: String,
    /// `None` when a dynamic command cannot be reconstructed from its tool input.
    pub command: Option<String>,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct Worker {
    pub session_id: String,
    pub agent_id: String,
    pub path: PathBuf,
    pub status: crate::lanes::LaneStatus,
    pub status_source: crate::lanes::StatusSource,
    pub description: Option<String>,
    pub last_text: Option<String>,
    pub truncated: bool,
}
#[derive(Debug, Default, Serialize)]
pub struct Omitted {
    pub requests: usize,
    /// File-event occurrences omitted, not a count of unique paths.
    pub files: usize,
    pub commands: usize,
    pub workers: usize,
}
#[derive(Debug, Serialize)]
pub struct Overview {
    pub session: Value,
    pub requests: Vec<Request>,
    pub workers: Vec<Worker>,
    pub files: Vec<File>,
    pub commands: Vec<Command>,
    pub omitted: Omitted,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_coverage: Option<Value>,
}

/// Read the parent once, retaining bounded previews. A failed/changed parent read never returns
/// a partial overview. Worker discovery is independent and failures are disclosed in warnings.
pub fn read(r: &SessionRef, opts: &OverviewOptions) -> Result<Overview> {
    let adapter = crate::harness::rootless(r.harness).context("harness reader unavailable")?;
    let mut sink = Collector::new(r, opts);
    let streamed = crate::stream::stream_counted(
        adapter,
        r,
        &Demand::FACTS.with_spans(),
        &mut sink,
        Pass {
            record: false,
            guard: true,
            ..Pass::RECORD
        },
    )?;
    if let Some(guard) = &streamed.guard {
        guard.verify(r)?;
    }
    let mut reference = r.clone();
    reference.id = streamed.session.id.clone();
    reference.message_count = streamed.counted.as_ref().map(|counts| counts.turns);
    reference.cwd = streamed.session.cwd.clone().or(reference.cwd);
    reference.title = streamed.session.title.clone().or(reference.title);
    reference.created_at = streamed.session.created_at.or(reference.created_at);
    reference.updated_at = streamed.session.updated_at.or(reference.updated_at);
    let mut overview = sink.finish(&reference, &streamed.session);
    for file in &mut overview.files {
        file.present = present(&file.path);
    }
    if opts.worker_limit > 0 {
        match crate::lanes::lanes_of(&reference) {
            Ok(lanes) => {
                overview.omitted.workers = lanes.len().saturating_sub(opts.worker_limit);
                overview.workers = lanes
                    .into_iter()
                    .rev()
                    .take(opts.worker_limit)
                    .map(|lane| {
                        let (description, a) = optional_excerpt(lane.description, opts.excerpt_bytes);
                        let (last_text, b) = optional_excerpt(lane.last_text, opts.excerpt_bytes);
                        Worker {
                            session_id: lane.session_id,
                            agent_id: lane.agent_id,
                            path: lane.path,
                            status: lane.status,
                            status_source: lane.status_source,
                            description,
                            last_text,
                            truncated: a || b,
                        }
                    })
                    .collect();
                overview.workers.reverse();
            }
            Err(error) => overview
                .warnings
                .push(format!("Worker evidence unavailable: {error:#}")),
        }
    } else {
        overview
            .warnings
            .push("Worker discovery disabled by worker_limit=0.".into());
    }
    Ok(overview)
}

/// Orient over an already imported session without filesystem inspection or worker discovery.
/// The imported session owns its messages; this collector retains only bounded excerpts.
pub fn from_session(session: &Session, opts: &OverviewOptions) -> Result<Overview> {
    let r = SessionRef {
        id: session.id.clone(),
        harness: session.harness,
        path: session.source_path.clone().unwrap_or_default(),
        cwd: session.cwd.clone(),
        title: session.title.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        message_count: Some(session.messages.iter().filter(|m| m.is_turn()).count()),
    };
    let mut sink = Collector::new(&r, opts);
    sink.meta(session);
    for message in &session.messages {
        sink.observe(message)?;
    }
    let mut overview = sink.finish(&r, session);
    overview
        .warnings
        .push("Worker discovery and file availability are not checked for an imported session.".into());
    Ok(overview)
}

fn present(path: &str) -> Option<bool> {
    if !std::path::Path::new(path).is_absolute() {
        return None;
    }
    match std::fs::metadata(path) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}
fn excerpt(text: &str, max: usize) -> (String, bool) {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), end < text.len())
}
fn optional_excerpt(text: Option<String>, max: usize) -> (Option<String>, bool) {
    match text {
        Some(text) => {
            let (head, cut) = excerpt(&text, max);
            (Some(head), cut)
        }
        None => (None, false),
    }
}
fn recent<T>(queue: &mut VecDeque<T>, value: T, limit: usize) {
    if limit == 0 {
        return;
    }
    if queue.len() == limit {
        queue.pop_front();
    }
    queue.push_back(value);
}

struct Collector<'a> {
    opts: &'a OverviewOptions,
    cwd: Option<PathBuf>,
    harness: crate::Harness,
    resolver: crate::Resolver,
    index: usize,
    totals: Omitted,
    first: Option<Request>,
    requests: VecDeque<Request>,
    files: VecDeque<File>,
    commands: VecDeque<Command>,
    extractor: EventExtractor,
    scratch: Vec<crate::events::Event>,
    error: Option<anyhow::Error>,
}
impl<'a> Collector<'a> {
    fn new(r: &SessionRef, opts: &'a OverviewOptions) -> Self {
        Self {
            opts,
            cwd: r.cwd.clone(),
            harness: r.harness,
            resolver: crate::Resolver::new(Some(r.path.clone())),
            index: 0,
            totals: Omitted::default(),
            first: None,
            requests: VecDeque::new(),
            files: VecDeque::new(),
            commands: VecDeque::new(),
            extractor: EventExtractor::default(),
            scratch: Vec::new(),
            error: None,
        }
    }
    fn observe(&mut self, m: &Message) -> Result<()> {
        if m.role == Role::User && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
            self.totals.requests += 1;
            if self.opts.request_limit > 0 {
                let mut text = String::new();
                let mut truncated = false;
                for block in &m.content {
                    if let Block::Text { text: source, .. } = block {
                        if !text.is_empty() {
                            if text.len() < self.opts.excerpt_bytes {
                                text.push('\n');
                            } else {
                                truncated = true;
                            }
                        }
                        let left = self.opts.excerpt_bytes.saturating_sub(text.len());
                        let prefix = source.try_resolve_prefix(&self.resolver, left.saturating_add(4) as u64)?;
                        let (head, cut) = excerpt(&prefix, left);
                        text.push_str(&head);
                        truncated |= cut;
                    }
                }
                let request = Request {
                    msg_idx: self.index,
                    ts: m.timestamp,
                    text,
                    truncated,
                };
                if self.first.is_none() {
                    self.first = Some(request);
                } else {
                    recent(&mut self.requests, request, self.opts.request_limit.saturating_sub(1));
                }
            }
        }
        self.extractor
            .extract(m, self.index, self.cwd.as_deref(), None, None, &mut self.scratch)?;
        for event in self.scratch.drain(..) {
            if matches!(event.kind, "file_read" | "file_edit") {
                if let Some(path) = event.target {
                    self.totals.files += 1;
                    recent(
                        &mut self.files,
                        File {
                            msg_idx: event.msg_idx,
                            ts: event.ts.and_then(|t| DateTime::from_timestamp(t, 0)),
                            kind: event.kind,
                            path,
                            present: None,
                        },
                        self.opts.file_limit,
                    );
                }
            }
        }
        crate::events::release_scratch(&mut self.scratch);
        for block in &m.content {
            if let Block::ToolUse { name, input, .. } = block {
                input.with_view(|view| {
                    crate::tools::classify::visit_tool_ops(
                        Some(self.harness),
                        name,
                        view,
                        crate::tools::classify::ClassifyOpts {
                            commands: self.opts.command_limit > 0,
                            command_limit: Some(self.opts.excerpt_bytes.saturating_add(4)),
                            summaries: false,
                        },
                        |sem| {
                            if let crate::tools::classify::ToolOp::Command { cmd, .. } = sem.op {
                                self.totals.commands += 1;
                                let (command, truncated) =
                                    optional_excerpt(cmd.map(|c| c.into_owned()), self.opts.excerpt_bytes);
                                recent(
                                    &mut self.commands,
                                    Command {
                                        msg_idx: self.index,
                                        ts: m.timestamp,
                                        tool: sem.tool.into_owned(),
                                        command,
                                        truncated,
                                    },
                                    self.opts.command_limit,
                                );
                            }
                        },
                    )
                })?;
            }
        }
        self.index += 1;
        Ok(())
    }
    fn finish(self, r: &SessionRef, session: &Session) -> Overview {
        let requests: Vec<_> = self.first.into_iter().chain(self.requests).collect();
        let omitted = Omitted {
            requests: self.totals.requests.saturating_sub(requests.len()),
            files: self.totals.files.saturating_sub(self.files.len()),
            commands: self.totals.commands.saturating_sub(self.commands.len()),
            workers: 0,
        };
        Overview {
            session: crate::rows::session_row(r, None),
            requests,
            workers: Vec::new(),
            files: self.files.into(),
            commands: self.commands.into(),
            omitted,
            warnings: Vec::new(),
            read_coverage: session.cv_extra().and_then(|v| v.get("read_coverage")).cloned(),
        }
    }
}
impl MessageSink for Collector<'_> {
    fn tier(&self) -> Tier {
        Tier::Facts
    }
    fn meta(&mut self, session: &Session) {
        if self.cwd.is_none() {
            self.cwd = session.cwd.clone();
        }
        if session.source_path.is_some() {
            self.resolver = session.resolver();
        }
    }
    fn message(&mut self, message: Message) -> Flow {
        match self.observe(&message) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.error = Some(error);
                Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, Json};

    fn prompt(text: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content.push(Block::from_text(text.into()));
        m
    }
    fn call(name: &str, input: Value) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::ToolUse {
            id: "call".into(),
            name: name.into(),
            input: Json::from(input),
            namespace: None,
            extensions: Default::default(),
        });
        m
    }
    #[test]
    fn retains_first_and_recent_human_requests_with_exact_indices() -> Result<()> {
        let mut session = Session::new("overview", Harness::Claude);
        session.messages.push(prompt("first"));
        let mut injection = prompt("injected instructions");
        injection.kind = MessageKind::InjectedContext;
        session.messages.push(injection);
        let mut delegated = prompt("agent message");
        delegated.origin = Origin::Subagent;
        session.messages.push(delegated);
        for n in 0..7 {
            session.messages.push(prompt(&format!("request {n}")));
        }
        let overview = from_session(&session, &OverviewOptions::default())?;
        assert_eq!(
            overview.requests.iter().map(|r| r.msg_idx).collect::<Vec<_>>(),
            [0, 6, 7, 8, 9]
        );
        assert_eq!(overview.requests[0].text, "first");
        assert_eq!(overview.omitted.requests, 3);
        let opts = OverviewOptions {
            request_limit: 1,
            ..Default::default()
        };
        assert_eq!(from_session(&session, &opts)?.requests.len(), 1);
        Ok(())
    }
    #[test]
    fn bounds_giant_text_commands_and_recent_occurrences() -> Result<()> {
        let mut session = Session::new("bounded", Harness::Claude);
        session.messages.push(prompt(&"🙂".repeat(1_000_000)));
        session
            .messages
            .push(call("Bash", serde_json::json!({"command": "x".repeat(2_000_000)})));
        for i in 0..20 {
            session
                .messages
                .push(call("Read", serde_json::json!({"file_path": format!("/example/{i}")})));
        }
        let opts = OverviewOptions {
            excerpt_bytes: 17,
            file_limit: 2,
            ..Default::default()
        };
        let overview = from_session(&session, &opts)?;
        assert_eq!(overview.requests[0].text.len(), 16);
        assert!(overview.requests[0].truncated);
        assert_eq!(overview.commands[0].msg_idx, 1);
        assert_eq!(overview.commands[0].command.as_ref().unwrap().len(), 17);
        assert!(overview.commands[0].truncated);
        assert_eq!(overview.files.len(), 2);
        assert_eq!(overview.files[0].msg_idx, 20);
        assert_eq!(overview.omitted.files, 18);
        assert!(overview.files.iter().all(|f| f.present.is_none()));
        Ok(())
    }
    #[test]
    fn code_mode_commands_keep_their_source_index() -> Result<()> {
        let mut session = Session::new("commands", Harness::Codex);
        session.messages.push(call(
            "exec",
            serde_json::json!(
                "await tools.exec_command({cmd: 'cargo check'}); await tools.exec_command({cmd: 'git status'});"
            ),
        ));
        let overview = from_session(&session, &OverviewOptions::default())?;
        assert_eq!(overview.commands.len(), 2);
        assert!(overview.commands.iter().all(|c| c.msg_idx == 0));
        assert_eq!(overview.commands[1].command.as_deref(), Some("git status"));
        Ok(())
    }
    #[test]
    fn source_backed_command_is_bounded_and_error_body_is_not_resolved() -> Result<()> {
        let dir = cv_testkit::temp_dir("overview-source-command");
        let path = dir.join("input.json");
        let wire = serde_json::to_vec(&serde_json::json!({"command": "é".repeat(500_000)}))?;
        std::fs::write(&path, &wire)?;
        let mut session = Session::new("source-command", Harness::Claude);
        let mut message = call("Bash", serde_json::json!({}));
        if let Block::ToolUse { input, .. } = &mut message.content[0] {
            *input = Json::source(&path, 0, wire.len() as u64)?;
        }
        session.messages.push(message);
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "call".into(),
            content: crate::Text::Span(crate::Span {
                source: Some(dir.join("missing-result")),
                offset: 0,
                len: 50_000_000,
                escaped: false,
            }),
            is_error: true,
            tool_name: Some("Bash".into()),
            status: None,
            details: None,
            extensions: Default::default(),
        });
        session.messages.push(result);
        let overview = from_session(
            &session,
            &OverviewOptions {
                excerpt_bytes: 7,
                ..Default::default()
            },
        )?;
        assert_eq!(overview.commands[0].command.as_deref(), Some("ééé"));
        assert!(overview.commands[0].truncated);
        Ok(())
    }
    fn source(path: PathBuf) -> SessionRef {
        SessionRef {
            id: "test-overview".into(),
            harness: Harness::Claude,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        }
    }
    #[test]
    fn read_reports_missing_file_and_does_not_read_its_body() -> Result<()> {
        let dir = cv_testkit::temp_dir("overview-read");
        let missing = dir.join("missing.txt");
        let file = dir.join("transcript.jsonl");
        let row = serde_json::json!({"type":"assistant", "uuid":"one", "sessionId":"test-overview",
            "message":{"role":"assistant", "content":[{"type":"tool_use","id":"read", "name":"Read", "input":{"file_path":missing}}]}});
        std::fs::write(&file, format!("{row}\n"))?;
        let overview = read(
            &source(file),
            &OverviewOptions {
                worker_limit: 0,
                ..Default::default()
            },
        )?;
        assert_eq!(overview.files.len(), 1);
        assert_eq!(overview.files[0].present, Some(false));
        assert_eq!(overview.files[0].msg_idx, 0);
        assert_eq!(overview.warnings.len(), 1);
        Ok(())
    }
    #[test]
    fn explicit_path_uses_recorded_session_id_instead_of_filename() -> Result<()> {
        let dir = cv_testkit::temp_dir("overview-recorded-id");
        let file = dir.join("copied-transcript.jsonl");
        let row = serde_json::json!({"type":"session_meta", "payload": {
            "id":"native-codex-id", "cwd":"/example", "timestamp":"2026-11-15T00:00:00Z"
        }});
        std::fs::write(&file, format!("{row}\n"))?;
        let reference = SessionRef::from_path(file, Harness::Codex);
        let overview = read(
            &reference,
            &OverviewOptions {
                worker_limit: 0,
                ..Default::default()
            },
        )?;
        assert_eq!(overview.session["id"], "native-codex-id");
        assert_eq!(overview.session["cwd"], "/example");
        Ok(())
    }
    #[test]
    fn unreadable_source_is_an_error_not_an_empty_overview() {
        let dir = cv_testkit::temp_dir("overview-unreadable");
        assert!(read(&source(dir.join("absent.jsonl")), &OverviewOptions::default()).is_err());
    }
    #[test]
    fn unavailable_lazy_request_fails_the_whole_collection() -> Result<()> {
        let dir = cv_testkit::temp_dir("overview-lazy");
        let path = dir.join("text.json");
        let mut session = Session::new("lazy", Harness::Claude);
        let mut message = prompt("");
        message.content = vec![Block::from_text(crate::Text::Span(crate::Span {
            source: Some(path),
            offset: 0,
            len: 8,
            escaped: false,
        }))];
        session.messages.push(prompt("valid earlier request"));
        session.messages.push(message);
        assert!(from_session(&session, &OverviewOptions::default()).is_err());
        Ok(())
    }

    #[test]
    fn subagent_delegation_prompts_are_excluded_from_human_requests() -> Result<()> {
        let dir = cv_testkit::temp_dir("overview-subagent-delegation");
        let opts = OverviewOptions {
            worker_limit: 0,
            ..Default::default()
        };

        // 1. Claude sub-agent transcript (`isSidechain: true`)
        let claude_dir = dir.join("claude/proj/parent-1/subagents");
        std::fs::create_dir_all(&claude_dir)?;
        let claude_path = claude_dir.join("agent-worker.jsonl");
        std::fs::write(
            &claude_path,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "user",
                    "uuid": "u1",
                    "sessionId": "parent-1",
                    "agentId": "worker",
                    "isSidechain": true,
                    "message": {"role": "user", "content": "Audit exports in crates/cv-core"}
                }),
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "a1",
                    "parentUuid": "u1",
                    "sessionId": "parent-1",
                    "agentId": "worker",
                    "isSidechain": true,
                    "message": {"role": "assistant", "model": "claude-sonnet-4-6", "content": [{"type": "text", "text": "Audited."}]}
                }),
            ),
        )?;
        let claude_ref = SessionRef::from_path(claude_path, Harness::Claude);
        let claude_overview = read(&claude_ref, &opts)?;
        assert!(
            claude_overview.requests.is_empty(),
            "Claude sidechain prompt must not appear in human requests: {:?}",
            claude_overview.requests
        );

        // 2. Codex sub-agent rollouts (both paginated `response_item` and legacy `event_msg`)
        let codex_paginated = dir.join("rollout-codex-child-paginated.jsonl");
        std::fs::write(
            &codex_paginated,
            [
                serde_json::json!({
                    "ordinal": 0,
                    "type": "session_meta",
                    "payload": {
                        "id": "codex-child-1",
                        "parent_thread_id": "codex-parent-1",
                        "subagent_history_start_ordinal": 2,
                        "cwd": "/repo"
                    }
                }),
                serde_json::json!({
                    "ordinal": 1,
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Parent human turn"}]}
                }),
                serde_json::json!({
                    "ordinal": 2,
                    "type": "response_item",
                    "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Delegated sub-agent task"}]}
                }),
                serde_json::json!({
                    "ordinal": 3,
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}
                }),
            ]
            .into_iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
        )?;
        let codex_overview = read(&SessionRef::from_path(codex_paginated, Harness::Codex), &opts)?;
        assert!(
            codex_overview.requests.is_empty(),
            "Codex paginated sub-agent prompt must not appear in human requests: {:?}",
            codex_overview.requests
        );

        let codex_events = dir.join("rollout-codex-child-events.jsonl");
        std::fs::write(
            &codex_events,
            [
                serde_json::json!({
                    "ordinal": 0,
                    "type": "session_meta",
                    "payload": {
                        "id": "codex-child-2",
                        "parent_thread_id": "codex-parent-1",
                        "subagent_history_start_ordinal": 2,
                        "cwd": "/repo"
                    }
                }),
                serde_json::json!({
                    "ordinal": 1,
                    "type": "event_msg",
                    "payload": {"type": "user_message", "message": "Parent human turn"}
                }),
                serde_json::json!({
                    "ordinal": 2,
                    "type": "event_msg",
                    "payload": {"type": "user_message", "message": "Delegated sub-agent event prompt"}
                }),
                serde_json::json!({
                    "ordinal": 3,
                    "type": "event_msg",
                    "payload": {"type": "agent_message", "message": "Done."}
                }),
            ]
            .into_iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
        )?;
        let codex_events_overview = read(&SessionRef::from_path(codex_events, Harness::Codex), &opts)?;
        assert!(
            codex_events_overview.requests.is_empty(),
            "Codex event_msg sub-agent prompt must not appear in human requests: {:?}",
            codex_events_overview.requests
        );

        // 3. Antigravity child transcript with `USER_INPUT` vs top-level parent transcript
        let agy_parent_id = "11111111-2222-4333-8444-555555555555";
        let agy_child_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let agy_root = dir.join("antigravity-cli");
        let parent_logs = agy_root
            .join("brain")
            .join(agy_parent_id)
            .join(".system_generated/logs");
        let parent_subs = agy_root
            .join("brain")
            .join(agy_parent_id)
            .join(".system_generated/subagents");
        let child_logs = agy_root.join("brain").join(agy_child_id).join(".system_generated/logs");
        std::fs::create_dir_all(&parent_logs)?;
        std::fs::create_dir_all(&parent_subs)?;
        std::fs::create_dir_all(&child_logs)?;
        let parent_path = parent_logs.join("transcript_full.jsonl");
        std::fs::write(
            &parent_path,
            format!(
                "{}\n",
                serde_json::json!({
                    "step_index": 0,
                    "source": "USER_EXPLICIT",
                    "type": "USER_INPUT",
                    "status": "DONE",
                    "content": "Human top-level request"
                })
            ),
        )?;
        std::fs::write(
            parent_subs.join(format!("{agy_child_id}.json")),
            serde_json::json!({
                "conversationId": agy_child_id,
                "state": "SUBAGENT_STATE_ALIVE",
                "subagentDescriptor": {"typeName": "worker", "role": "inspect"}
            })
            .to_string(),
        )?;
        let child_path = child_logs.join("transcript_full.jsonl");
        std::fs::write(
            &child_path,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "step_index": 0,
                    "source": "USER_IMPLICIT",
                    "type": "USER_INPUT",
                    "status": "DONE",
                    "content": "Delegated Antigravity prompt"
                }),
                serde_json::json!({
                    "step_index": 1,
                    "source": "MODEL",
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "content": "Done."
                })
            ),
        )?;
        let parent_overview = read(
            &SessionRef::new(agy_parent_id, Harness::Antigravity, parent_path),
            &opts,
        )?;
        assert_eq!(parent_overview.requests.len(), 1);
        assert_eq!(parent_overview.requests[0].text, "Human top-level request");
        let child_overview = read(&SessionRef::new(agy_child_id, Harness::Antigravity, child_path), &opts)?;
        assert!(
            child_overview.requests.is_empty(),
            "Antigravity child USER_INPUT must not appear in human requests: {:?}",
            child_overview.requests
        );
        Ok(())
    }
}
