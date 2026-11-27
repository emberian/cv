//! `cv dataset --format cycle`: one flat JSONL record per human-turn cycle instead of one per session.
//!
//! A cycle is a typed prompt H after the session's first one, with the turn it reacts to and the
//! turn it starts: `report` is the last assistant text before H (the end-of-turn report the person
//! read), `plan` the first assistant text after H (the revised plan), and the tool calls of the span
//! before H (since the previous typed prompt) and after H (until the next one). This is the unit for
//! learning from how people steer agents (pushback, approvals, new asks), which a whole-session
//! record hides: there the person's turns and the harness's injected context both render as `user`.
//!
//! Human-turn attestation and wrapper rules belong to [`crate::harness::Adapter::human_turn`].
//! Closed cycles stream directly into [`ByteStage`](crate::harness::store::bytes::ByteStage), tool
//! commands scan [`JsonView`](crate::json::JsonView) via [`crate::tools::classify::visit_tool_ops`]
//! without materializing `serde_json::Value`, and `--active-branch` stages turns through
//! [`crate::spill::Ordered`].

use super::{select_trajectory, DatasetOptions, InputError, StagedRecord};
use crate::harness::store::bytes::{ByteSource, ByteStage};
use crate::ir::{Block, Harness, Message, MessageKind};
use crate::spill::Record;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::OnceLock;

/// A turn relayed from another agent by a coordinator carries a `[from <seat>]` mark.
fn is_relayed(text: &str) -> bool {
    text.trim_start().starts_with("[from ")
}

/// `(commit, push)`: does this command line run `git commit` / `git push` (global options such as
/// `-C dir` or `-c k=v` between `git` and the verb allowed)?
fn git_verbs(cmd: &str) -> (bool, bool) {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\bgit(?:\s+(?:-[Cc]\s+\S+|--?[\w-]+(?:=\S+)?))*\s+(commit|push)\b").expect("git verb regex")
    });
    let (mut commit, mut push) = (false, false);
    for c in re.captures_iter(cmd) {
        match &c[1] {
            "commit" => commit = true,
            _ => push = true,
        }
    }
    (commit, push)
}

/// The assistant activity between two typed prompts.
#[derive(Default)]
struct Span {
    tools: BTreeMap<String, u64>,
    calls: u64,
    commit: bool,
    push: bool,
    first_text: Option<(String, Option<String>)>,
    last_text: Option<(String, Option<String>)>,
}

/// A cycle whose prompt is known and whose following span is still open.
struct Pending {
    index: usize,
    prompt_id: Option<String>,
    timestamp: Option<String>,
    prompt: String,
    queued: bool,
    proof: &'static str,
    relayed: bool,
    report: Option<(String, Option<String>)>,
    before: Span,
}

#[derive(Clone, Copy, Serialize)]
pub struct CycleHeader<'a> {
    pub harness: &'a str,
    pub session: &'a str,
    pub cwd: Option<&'a str>,
}

#[derive(Serialize)]
struct FullCycleRow<'a> {
    harness: &'a str,
    session: &'a str,
    cwd: Option<&'a str>,
    #[serde(flatten)]
    body: CycleBody<'a>,
}

#[derive(Serialize)]
struct CycleBody<'a> {
    index: usize,
    prompt_id: Option<&'a str>,
    timestamp: Option<&'a str>,
    report: Option<&'a str>,
    report_at: Option<&'a str>,
    prompt: &'a str,
    queued: bool,
    proof: &'static str,
    relayed: bool,
    plan: Option<&'a str>,
    plan_at: Option<&'a str>,
    tools_before: &'a BTreeMap<String, u64>,
    tool_calls_before: u64,
    tools_after: &'a BTreeMap<String, u64>,
    tool_calls_after: u64,
    commit_after: bool,
    push_after: bool,
}

/// Cuts a session's messages (in order) into cycles, streaming each closed cycle straight into `W`.
pub struct Cutter<'a, W: Write> {
    harness: Harness,
    header: Option<CycleHeader<'a>>,
    limit: Option<usize>,
    last_human: Option<String>,
    last_queued: bool,
    resolver: &'a crate::lazy::Resolver,
    redact: Option<&'a crate::redact::RedactOptions>,
    prompts: usize,
    span: Span,
    pending: Option<Pending>,
    emitted: usize,
    out: W,
}

impl<'a, W: Write> Cutter<'a, W> {
    pub fn new(
        harness: Harness,
        header: Option<CycleHeader<'a>>,
        limit: Option<usize>,
        resolver: &'a crate::lazy::Resolver,
        redact: Option<&'a crate::redact::RedactOptions>,
        out: W,
    ) -> Self {
        Self {
            harness,
            header,
            limit,
            last_human: None,
            last_queued: false,
            resolver,
            redact,
            prompts: 0,
            span: Span::default(),
            pending: None,
            emitted: 0,
            out,
        }
    }

    fn scrub(&self, s: String) -> String {
        match self.redact {
            Some(opts) => {
                let mut stats = crate::redact::RedactStats::default();
                crate::redact::scrub_cow(&s, opts, &mut stats).into_owned()
            }
            None => s,
        }
    }

    fn raw_text(&self, m: &Message) -> Result<String> {
        let mut s = String::new();
        for b in &m.content {
            if let Block::Text { text, .. } = b {
                let t = text.try_resolve(self.resolver)?;
                if t.trim().is_empty() {
                    continue;
                }
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&t);
            }
        }
        Ok(s)
    }

    /// Assistant prose for `report` / `plan`, skipping empty blocks and `[PRUNED id=…]` markers.
    fn assistant_text(&self, m: &Message) -> Result<Option<String>> {
        let mut s = String::new();
        for b in &m.content {
            if let Block::Text { text, .. } = b {
                if text
                    .inline_str()
                    .is_some_and(|inline| inline.trim_start().starts_with(crate::prune::MARKER_PREFIX))
                {
                    continue;
                }
                let t = text.try_resolve(self.resolver)?;
                if t.trim().is_empty() || t.trim_start().starts_with(crate::prune::MARKER_PREFIX) {
                    continue;
                }
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&t);
            }
        }
        if s.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(self.scrub(s)))
    }

    pub fn message(&mut self, m: &Message) -> Result<crate::Flow> {
        if self.limit == Some(0) {
            return Ok(crate::Flow::Stop);
        }
        let at = m.timestamp.map(|t| t.to_rfc3339());
        if matches!(m.kind, MessageKind::Prompt | MessageKind::InjectedContext) {
            let raw = self.raw_text(m)?;
            if let Some(turn) = crate::harness::human_turn(self.harness, m, &raw) {
                // A queued message the harness also wrote again as a prompt is one turn.
                if (turn.queued || self.last_queued) && self.last_human.as_deref() == Some(turn.text) {
                    return Ok(crate::Flow::Continue);
                }
                let text = turn.text.to_owned();
                let relayed = is_relayed(&text);
                self.last_human = Some(text.clone());
                self.last_queued = turn.queued;
                self.prompts += 1;
                let span = std::mem::take(&mut self.span);
                if let Some(p) = self.pending.take() {
                    self.close(p, &span)?;
                    if self.limit.is_some_and(|l| self.emitted >= l) {
                        return Ok(crate::Flow::Stop);
                    }
                }
                if self.prompts > 1 {
                    let report = span.last_text.clone();
                    self.pending = Some(Pending {
                        index: self.prompts - 1,
                        prompt_id: m.id.clone(),
                        timestamp: at,
                        prompt: self.scrub(text),
                        queued: turn.queued,
                        proof: turn.proof.as_str(),
                        relayed,
                        report,
                        before: span,
                    });
                }
                return Ok(crate::Flow::Continue);
            }
        }
        match m.kind {
            MessageKind::Reply | MessageKind::SubagentSpawn => {
                for b in &m.content {
                    if let Block::ToolUse { name, input, .. } = b {
                        *self.span.tools.entry(name.clone()).or_default() += 1;
                        self.span.calls += 1;
                        input.with_view(|view| {
                            crate::tools::classify::visit_tool_ops(
                                Some(self.harness),
                                name,
                                view,
                                crate::tools::classify::ClassifyOpts {
                                    commands: true,
                                    command_limit: Some(64 * 1024),
                                    summaries: false,
                                },
                                |sem| {
                                    if let crate::tools::classify::ToolOp::Command { cmd: Some(cmd), .. } = sem.op {
                                        let (c, p) = git_verbs(&cmd);
                                        self.span.commit |= c;
                                        self.span.push |= p;
                                    }
                                },
                            )
                        })?;
                    }
                }
                if m.content.iter().any(|b| matches!(b, Block::Text { .. })) {
                    if let Some(text) = self.assistant_text(m)? {
                        if self.span.first_text.is_none() {
                            self.span.first_text = Some((text.clone(), at.clone()));
                        }
                        self.span.last_text = Some((text, at));
                    }
                }
            }
            _ => {}
        }
        Ok(crate::Flow::Continue)
    }

    fn close(&mut self, p: Pending, after: &Span) -> Result<()> {
        if self.limit.is_some_and(|l| self.emitted >= l) {
            return Ok(());
        }
        let (report, report_at) = p
            .report
            .as_ref()
            .map_or((None, None), |(t, a)| (Some(t.as_str()), a.as_deref()));
        let (plan, plan_at) = after
            .first_text
            .as_ref()
            .map_or((None, None), |(t, a)| (Some(t.as_str()), a.as_deref()));
        let body = CycleBody {
            index: p.index,
            prompt_id: p.prompt_id.as_deref(),
            timestamp: p.timestamp.as_deref(),
            report,
            report_at,
            prompt: &p.prompt,
            queued: p.queued,
            proof: p.proof,
            relayed: p.relayed,
            plan,
            plan_at,
            tools_before: &p.before.tools,
            tool_calls_before: p.before.calls,
            tools_after: &after.tools,
            tool_calls_after: after.calls,
            commit_after: after.commit,
            push_after: after.push,
        };
        if let Some(h) = self.header {
            serde_json::to_writer(
                &mut self.out,
                &FullCycleRow {
                    harness: h.harness,
                    session: h.session,
                    cwd: h.cwd,
                    body,
                },
            )?;
        } else {
            serde_json::to_writer(&mut self.out, &body)?;
        }
        self.out.write_all(b"\n")?;
        self.emitted += 1;
        Ok(())
    }

    /// Close any open trailing cycle at EOF and return `(writer, emitted_count)`.
    pub fn finish(mut self) -> Result<(W, usize)> {
        let span = std::mem::take(&mut self.span);
        if let Some(p) = self.pending.take() {
            self.close(p, &span)?;
        }
        self.out.flush()?;
        Ok((self.out, self.emitted))
    }
}

/// Spilled message transport for `--format cycle --active-branch`: preserves timestamps and lean
/// prompt/attachment provenance alongside [`super::deferred::DeferredMessage`]'s detached source
/// descriptors while dropping unused tool-result/thinking/media blocks.
#[derive(Serialize, Deserialize)]
struct CycleHeld {
    deferred: super::deferred::DeferredMessage,
    timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl CycleHeld {
    fn stage(mut message: Message, registry: &mut crate::json::SourceRegistry) -> (Message, Self) {
        let inactive = super::is_inactive_branch(&message);
        let sidechain = super::is_sidechain(&message);
        let mut shell = Message::of_kind(message.role, message.kind, message.origin);
        shell.id = message.id.clone();
        shell.parent_id = message.parent_id.clone();
        if inactive || sidechain {
            let mut cv = serde_json::Map::new();
            if inactive {
                cv.insert("inactive_branch".into(), serde_json::Value::Bool(true));
            }
            if sidechain {
                cv.insert("isSidechain".into(), serde_json::Value::Bool(true));
            }
            shell
                .extra
                .insert(crate::ir::CV_NAMESPACE.into(), serde_json::Value::Object(cv));
        }

        let timestamp = message.timestamp;
        let extra = if matches!(message.kind, MessageKind::Prompt | MessageKind::InjectedContext) {
            message.extra.clone()
        } else {
            serde_json::Map::new()
        };
        message
            .content
            .retain(|b| matches!(b, Block::Text { .. } | Block::ToolUse { .. }));
        let deferred = super::deferred::DeferredMessage::stage(message, registry);
        (
            shell,
            Self {
                deferred,
                timestamp,
                extra,
            },
        )
    }

    fn restore(self, registry: &crate::json::SourceRegistry) -> Result<Message> {
        let mut msg = self.deferred.restore(registry)?;
        msg.timestamp = self.timestamp;
        if !self.extra.is_empty() {
            msg.extra = self.extra;
        }
        Ok(msg)
    }
}

fn map_heap(map: &serde_json::Map<String, serde_json::Value>) -> usize {
    fn val_heap(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::String(s) => s.capacity(),
            serde_json::Value::Array(a) => {
                a.capacity() * std::mem::size_of::<serde_json::Value>() + a.iter().map(val_heap).sum::<usize>()
            }
            serde_json::Value::Object(o) => o.iter().map(|(k, v)| 1024 + k.capacity() + val_heap(v)).sum(),
            _ => 0,
        }
    }
    map.iter().map(|(k, v)| 1024 + k.capacity() + val_heap(v)).sum()
}

impl Record for CycleHeld {
    type ReadState = Vec<u8>;

    fn heap_bytes(&self) -> usize {
        self.deferred.heap_bytes() + map_heap(&self.extra)
    }

    fn write(&self, writer: &mut impl Write) -> Result<()> {
        serde_json::to_writer(&mut *writer, self).context("writing deferred cycle message")?;
        writer.write_all(b"\n")?;
        Ok(())
    }

    fn read(reader: &mut impl BufRead, state: &mut Vec<u8>) -> Result<Self> {
        state.clear();
        let read = reader.read_until(b'\n', state)?;
        anyhow::ensure!(read > 0, "missing deferred cycle record");
        if state.last() == Some(&b'\n') {
            state.pop();
        }
        let row = serde_json::from_slice(state).context("reading deferred cycle message")?;
        state.clear();
        state.shrink_to(64 * 1024);
        Ok(row)
    }
}

/// Prepend `{"harness":...,"session":...,"cwd":...,` to each JSON-object line in `bodies`,
/// streaming in bounded buffer slices without re-parsing JSON.
fn prepend_session_header(bodies: ByteSource, header: CycleHeader<'_>) -> Result<ByteSource> {
    let mut prefix = serde_json::to_vec(&header)?;
    anyhow::ensure!(prefix.pop() == Some(b'}'), "header serializes as JSON object");
    prefix.push(b',');

    let mut reader = BufReader::with_capacity(64 * 1024, bodies.reader()?);
    let mut out = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    let mut at_line_start = true;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        if at_line_start {
            anyhow::ensure!(buf[0] == b'{', "cycle body line must open with '{{'");
            out.write_all(&prefix)?;
            reader.consume(1);
            at_line_start = false;
            continue;
        }
        if let Some(nl) = memchr::memchr(b'\n', buf) {
            out.write_all(&buf[..=nl])?;
            reader.consume(nl + 1);
            at_line_start = true;
        } else {
            let len = buf.len();
            out.write_all(buf)?;
            reader.consume(len);
        }
    }
    Ok(out.into_inner().map_err(|e| e.into_error())?.finish()?)
}

/// Write `session`'s cycles directly to `w` (with trailing newlines between records, no trailing
/// newline on the last record when matching [`super::write_record_with_options`], or `false` when
/// no cycles exist).
pub(super) fn write_session_cycles<W: Write + ?Sized>(
    session: &crate::ir::Session,
    w: &mut W,
    redact: Option<&crate::redact::RedactOptions>,
    opts: &DatasetOptions,
) -> std::io::Result<bool> {
    if opts.clean_outcome_only {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--clean-outcome cannot be combined with --format cycle",
        ));
    }
    let select = DatasetOptions {
        clean_outcome_only: false,
        ..opts.clone()
    };
    let selected = select_trajectory(&session.messages, &select).unwrap_or_default();
    let cwd = session.cwd.as_ref().map(|p| p.display().to_string());
    let header = CycleHeader {
        harness: session.harness.as_str(),
        session: &session.id,
        cwd: cwd.as_deref(),
    };
    let resolver = session.resolver();
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut cutter = Cutter::new(session.harness, Some(header), None, &resolver, redact, &mut stage);
        for idx in selected {
            cutter.message(&session.messages[idx]).map_err(std::io::Error::other)?;
        }
        let (_, count) = cutter.finish().map_err(std::io::Error::other)?;
        if count == 0 {
            return Ok(false);
        }
    }
    let bytes = stage
        .into_inner()
        .map_err(|e| e.into_error())?
        .finish()
        .map_err(std::io::Error::other)?;
    let mut buf = Vec::new();
    bytes.copy_to(&mut buf)?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    w.write_all(&buf)?;
    Ok(true)
}

/// Stream one session and stage its cycle records (one JSONL line each, session fields first),
/// or `None` when it has fewer than `min_messages` messages, the query rejects it, or it holds no
/// cycle.
pub fn stage_cycles(
    adapter: &dyn crate::harness::Adapter,
    r: &crate::ir::SessionRef,
    redact: Option<&crate::redact::RedactOptions>,
    min_messages: usize,
    query: Option<&crate::SessionQuery>,
    text_sets: &crate::query::TextSets,
    opts: &DatasetOptions,
) -> Result<Option<(StagedRecord, usize)>> {
    stage_cycles_bounded(adapter, r, redact, min_messages, query, text_sets, opts, None)
}

/// Like [`stage_cycles`], emitting at most `max_records` cycle rows when `Some`.
#[allow(clippy::too_many_arguments)]
pub fn stage_cycles_bounded(
    adapter: &dyn crate::harness::Adapter,
    r: &crate::ir::SessionRef,
    redact: Option<&crate::redact::RedactOptions>,
    min_messages: usize,
    query: Option<&crate::SessionQuery>,
    text_sets: &crate::query::TextSets,
    opts: &DatasetOptions,
    max_records: Option<usize>,
) -> Result<Option<(StagedRecord, usize)>> {
    anyhow::ensure!(
        !opts.clean_outcome_only,
        "--clean-outcome cannot be combined with --format cycle"
    );
    if max_records == Some(0) {
        return Ok(None);
    }

    struct ActiveHold {
        shells: Vec<Message>,
        rows: crate::spill::Ordered<CycleHeld>,
        sources: crate::json::SourceRegistry,
    }

    struct Sink<'a> {
        cutter: Option<Cutter<'a, BufWriter<ByteStage>>>,
        held: Option<ActiveHold>,
        facts: Option<crate::query::QueryReducer<'a>>,
        messages: usize,
        latch: crate::stream::Latch,
    }

    impl crate::MessageSink for Sink<'_> {
        fn message(&mut self, message: Message) -> crate::Flow {
            self.messages += 1;
            let result = (|| {
                if let Some(facts) = &mut self.facts {
                    facts.observe(&message)?;
                }
                if let Some(hold) = &mut self.held {
                    let (shell, row) = CycleHeld::stage(message, &mut hold.sources);
                    hold.shells.push(shell);
                    hold.rows.push(row)?;
                } else if let Some(cutter) = &mut self.cutter {
                    let _ = cutter.message(&message)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            match result {
                Ok(()) => crate::Flow::Continue,
                Err(error) => {
                    self.latch.fail(error);
                    crate::Flow::Stop
                }
            }
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.latch.take("dataset cycles")
        }
    }

    let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
    // Only allow early `Flow::Stop` when neither `query`, `min_messages`, nor `Pass::RECORD`
    // certification needs EOF — in fact, passing `Pass::RECORD` certifies turn counts on full
    // reads only when the stream reaches EOF, so keep streaming to EOF and let `Cutter` ignore
    // cycles past `max_records` at `close`!
    let mut sink = Sink {
        cutter: (!opts.active_branch_only).then(|| {
            Cutter::new(
                r.harness,
                None,
                max_records,
                &resolver,
                redact,
                BufWriter::with_capacity(64 * 1024, ByteStage::new()),
            )
        }),
        held: opts.active_branch_only.then(|| ActiveHold {
            shells: Vec::new(),
            rows: Default::default(),
            sources: Default::default(),
        }),
        facts: query.map(crate::query::QueryReducer::new),
        messages: 0,
        latch: Default::default(),
    };
    let counted_query = query.is_some_and(crate::SessionQuery::needs_counted);
    let read = crate::stream::stream_counted(
        adapter,
        r,
        &crate::Demand::LEAN.with_spans(),
        &mut sink,
        crate::stream::Pass::RECORD,
    );
    let (session, counted) = match read {
        Ok(done) => (done.session, done.counted),
        Err(error) if sink.latch.failed() => return Err(error),
        Err(error) => return Err(InputError(error).into()),
    };
    let counts = counted.filter(|_| counted_query).map(|c| c.counts);
    if sink.messages < min_messages {
        return Ok(None);
    }
    if let Some(facts) = sink.facts.take() {
        if !facts.matches(r, &session, text_sets, counts)? {
            return Ok(None);
        }
    }

    let cwd = session.cwd.clone().or_else(|| r.cwd.clone());
    let cwd_str = cwd.as_ref().map(|p| p.display().to_string());
    let header = CycleHeader {
        harness: r.harness.as_str(),
        session: &r.id,
        cwd: cwd_str.as_deref(),
    };

    if let Some(hold) = sink.held.take() {
        let select = DatasetOptions {
            clean_outcome_only: false,
            ..opts.clone()
        };
        let selected = select_trajectory(&hold.shells, &select).unwrap_or_default();
        if selected.len() < min_messages {
            return Ok(None);
        }
        let mut cutter = Cutter::new(
            r.harness,
            Some(header),
            max_records,
            &resolver,
            redact,
            BufWriter::with_capacity(64 * 1024, ByteStage::new()),
        );
        if selected.windows(2).all(|w| w[0] < w[1]) {
            let mut want = selected.iter().copied().peekable();
            for (idx, row) in hold.rows.rows()?.enumerate() {
                let row = row?;
                if want.peek() == Some(&idx) {
                    want.next();
                    let msg = row.restore(&hold.sources)?;
                    cutter.message(&msg)?;
                    if want.peek().is_none() {
                        break;
                    }
                }
            }
        } else {
            let mut all = Vec::with_capacity(hold.shells.len());
            for row in hold.rows.rows()? {
                all.push(row?.restore(&hold.sources)?);
            }
            for idx in selected {
                cutter.message(&all[idx])?;
            }
        }
        let (writer, count) = cutter.finish()?;
        if count == 0 {
            return Ok(None);
        }
        let body = writer.into_inner().map_err(|e| e.into_error())?.finish()?;
        return Ok(Some((
            StagedRecord {
                leading: None,
                body,
                skip_header: 0,
            },
            count,
        )));
    }

    let (writer, count) = sink.cutter.expect("streaming cutter").finish()?;
    if count == 0 {
        return Ok(None);
    }
    let bodies = writer.into_inner().map_err(|e| e.into_error())?.finish()?;
    let body = prepend_session_header(bodies, header)?;
    Ok(Some((
        StagedRecord {
            leading: None,
            body,
            skip_header: 0,
        },
        count,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    fn msg(id: &str, kind: MessageKind, blocks: Vec<Block>) -> Message {
        let role = match kind {
            MessageKind::Prompt | MessageKind::InjectedContext => Role::User,
            MessageKind::ToolResult => Role::Tool,
            _ => Role::Assistant,
        };
        let mut m = Message::new(role);
        m.content = blocks;
        m.kind = kind;
        m.id = Some(id.into());
        m
    }

    fn text(t: &str) -> Block {
        Block::from_text(t.into())
    }

    fn call(id: &str, name: &str, command: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({ "command": command }).into(),
            namespace: None,
            extensions: Default::default(),
        }
    }

    fn cut(messages: &[Message], redact: Option<&crate::redact::RedactOptions>) -> Vec<serde_json::Value> {
        let resolver = crate::lazy::Resolver::new(None);
        let mut buf = Vec::new();
        let header = CycleHeader {
            harness: "claude",
            session: "s1",
            cwd: Some("/work"),
        };
        let mut c = Cutter::new(Harness::Claude, Some(header), None, &resolver, redact, &mut buf);
        for m in messages {
            c.message(m).unwrap();
        }
        let (_, count) = c.finish().unwrap();
        let rows: Vec<serde_json::Value> = std::str::from_utf8(&buf)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), count);
        rows
    }

    fn session() -> Vec<Message> {
        vec![
            msg("u1", MessageKind::Prompt, vec![text("Add a widget counter.")]),
            msg("a1", MessageKind::Reply, vec![text("Looking at the frobnicator.")]),
            msg("a2", MessageKind::Reply, vec![call("t1", "Read", "")]),
            msg("a3", MessageKind::Reply, vec![call("t2", "Edit", "")]),
            msg("a4", MessageKind::Reply, vec![text("Done: it counts frobs.")]),
            msg(
                "r1",
                MessageKind::InjectedContext,
                vec![text("<system-reminder>x</system-reminder>")],
            ),
            msg("s1", MessageKind::Prompt, vec![text("<bash-input>ls</bash-input>")]),
            msg(
                "s2",
                MessageKind::Prompt,
                vec![text("<bash-stdout>a.txt</bash-stdout>")],
            ),
            msg("u2", MessageKind::Prompt, vec![text("No: count widgets, not frobs.")]),
            msg("a5", MessageKind::Reply, vec![text("Right, switching to widgets.")]),
            msg(
                "a6",
                MessageKind::Reply,
                vec![call("t3", "Bash", "git -C repo commit -m w && git push")],
            ),
            msg("a7", MessageKind::Reply, vec![text("Committed.")]),
            msg("u3", MessageKind::Prompt, vec![text("looks good")]),
        ]
    }

    #[test]
    fn cuts_one_cycle_per_typed_prompt_after_the_first() {
        let cycles = cut(&session(), None);
        assert_eq!(
            cycles.len(),
            2,
            "u2 and u3; u1 opens the session, shell passthrough is no prompt"
        );
        let c = &cycles[0];
        assert_eq!(c["harness"], "claude");
        assert_eq!(c["session"], "s1");
        assert_eq!(c["cwd"], "/work");
        assert_eq!(c["prompt_id"], "u2");
        assert_eq!(c["report"], "Done: it counts frobs.");
        assert_eq!(c["prompt"], "No: count widgets, not frobs.");
        assert_eq!(c["plan"], "Right, switching to widgets.");
        assert_eq!(c["tools_before"], serde_json::json!({"Edit": 1, "Read": 1}));
        assert_eq!(c["tool_calls_after"], 1);
        assert_eq!(c["commit_after"], true);
        assert_eq!(c["push_after"], true);
        let last = &cycles[1];
        assert_eq!(last["report"], "Committed.");
        assert_eq!(last["plan"], serde_json::Value::Null);
        assert_eq!(last["commit_after"], false);
        assert_eq!(last["index"], 2);
    }

    #[test]
    fn redaction_applies_to_cycle_text() {
        let mut s = session();
        let fake = format!("use {}-ant-api03-{}", "sk", "FAKEfakeFAKEfake0123456789abcdefFAKE");
        s[8] = msg("u2", MessageKind::Prompt, vec![text(&fake)]);
        let cycles = cut(&s, Some(&crate::redact::RedactOptions::default()));
        let p = cycles[0]["prompt"].as_str().unwrap();
        assert!(!p.contains("FAKEfake"), "{p}");
        assert!(p.contains("[REDACTED"), "{p}");
        assert_eq!(cycles[0]["cwd"], "/work", "session cwd stays recorded as documented");
    }

    #[test]
    fn pruned_markers_never_become_report_or_plan() {
        let mut s = session();
        // Replace `a4` (after `Done: it counts frobs.`) with a pruned text stub, and insert a
        // pruned text stub before `a5`'s real plan.
        s.insert(
            5,
            msg(
                "a4_pruned",
                MessageKind::Reply,
                vec![text("[PRUNED id=a4_pruned#b0 4096 bytes -> flat.jsonl]")],
            ),
        );
        s.insert(
            10,
            msg(
                "a5_pruned",
                MessageKind::Reply,
                vec![text("[PRUNED id=a5_pruned#b0 2048 bytes -> flat.jsonl]")],
            ),
        );
        let cycles = cut(&s, None);
        assert_eq!(cycles[0]["report"], "Done: it counts frobs.");
        assert_eq!(cycles[0]["plan"], "Right, switching to widgets.");
    }

    #[test]
    fn queued_mid_turn_and_wrapped_echoes_cut_cleanly_through_real_claude_codec() {
        let jsonl = [
            r#"{"type":"user","sessionId":"s","cwd":"/repo","uuid":"u1","timestamp":"2026-10-01T00:00:00Z","promptSource":"typed","message":{"role":"user","content":"Add widgets"}}"#,
            r#"{"type":"assistant","sessionId":"s","uuid":"a1","parentUuid":"u1","timestamp":"2026-10-01T00:00:01Z","message":{"role":"assistant","model":"claude-fable-5-1","content":[{"type":"text","text":"Checking frobnicator."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#,
            // Unrendered human queued command (`commandMode: "prompt"`) + wrapped `user` echo + unrendered task notification.
            r#"{"type":"attachment","sessionId":"s","uuid":"q1","parentUuid":"a1","timestamp":"2026-10-01T00:00:02Z","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":"stop, use widget_table"}}"#,
            r#"{"type":"user","sessionId":"s","uuid":"qe1","parentUuid":"q1","timestamp":"2026-10-01T00:00:02Z","promptSource":"queued","message":{"role":"user","content":"<system-reminder>\nThe user sent a new message while you were working:\nstop, use widget_table\n\nIMPORTANT: After completing your current task, you MUST address the user's message above. Do not ignore it.\n</system-reminder>"}}"#,
            r#"{"type":"attachment","sessionId":"s","uuid":"qn1","parentUuid":"qe1","timestamp":"2026-10-01T00:00:03Z","attachment":{"type":"queued_command","commandMode":"task-notification","prompt":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
            r#"{"type":"assistant","sessionId":"s","uuid":"a2","parentUuid":"qn1","timestamp":"2026-10-01T00:00:04Z","message":{"role":"assistant","model":"claude-fable-5-1","content":[{"type":"text","text":"Switched to widget_table."}]}}"#,
            // System promptSource rejected; relayed human prompt accepted with `relayed: true`.
            r#"{"type":"user","sessionId":"s","uuid":"usys","parentUuid":"a2","timestamp":"2026-10-01T00:00:05Z","promptSource":"system","message":{"role":"user","content":"internal hook"}}"#,
            r#"{"type":"user","sessionId":"s","uuid":"u2","parentUuid":"usys","timestamp":"2026-10-01T00:00:06Z","message":{"role":"user","content":"[from lead] ship it"}}"#,
        ]
        .join("\n");

        let world = crate::test_world("cycles-claude-codec");
        let path = world.proj.join("s.jsonl");
        std::fs::write(&path, &jsonl).unwrap();
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.proj);
        let r = crate::ir::SessionRef::new("s", Harness::Claude, &path);

        for active_branch_only in [false, true] {
            let (staged, count) = stage_cycles(
                &adapter,
                &r,
                None,
                2,
                None,
                &Default::default(),
                &DatasetOptions {
                    active_branch_only,
                    ..Default::default()
                },
            )
            .unwrap()
            .expect("cycles staged");
            assert_eq!(count, 2, "active_branch_only={active_branch_only}");
            let mut out = Vec::new();
            staged.publish(&mut out).unwrap();
            let rows: Vec<serde_json::Value> = std::str::from_utf8(&out)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["cwd"], "/repo");
            assert_eq!(rows[0]["prompt"], "stop, use widget_table");
            assert_eq!(rows[0]["queued"], true);
            assert_eq!(rows[0]["proof"], "record");
            assert_eq!(rows[0]["report"], "Checking frobnicator.");
            assert_eq!(rows[0]["plan"], "Switched to widget_table.");
            assert_eq!(rows[1]["prompt"], "[from lead] ship it");
            assert_eq!(rows[1]["proof"], "adapter");
            assert_eq!(rows[1]["relayed"], true);
        }
    }

    /// The Claude desktop app records what the person types as `promptSource: "sdk"` with a stated
    /// human origin. Those are typed turns; an `sdk` prompt with no stated human origin is a program.
    #[test]
    fn claude_desktop_sdk_prompts_with_a_stated_human_origin_are_typed_turns() {
        let jsonl = [
            r#"{"type":"user","sessionId":"d","cwd":"/repo","uuid":"u1","timestamp":"2026-11-01T00:00:00Z","entrypoint":"claude-desktop","promptSource":"sdk","turnOrigin":"human","origin":{"kind":"human"},"message":{"role":"user","content":"Add widgets"}}"#,
            r#"{"type":"assistant","sessionId":"d","uuid":"a1","parentUuid":"u1","timestamp":"2026-11-01T00:00:01Z","message":{"role":"assistant","model":"claude-fable-5-2","content":[{"type":"text","text":"Widgets added."}]}}"#,
            // A program driving the SDK: same source, no stated human origin. Not a typed turn.
            r#"{"type":"user","sessionId":"d","uuid":"p1","parentUuid":"a1","timestamp":"2026-11-01T00:00:02Z","promptSource":"sdk","message":{"role":"user","content":"automated follow-up"}}"#,
            r#"{"type":"assistant","sessionId":"d","uuid":"a2","parentUuid":"p1","timestamp":"2026-11-01T00:00:03Z","message":{"role":"assistant","model":"claude-fable-5-2","content":[{"type":"text","text":"Follow-up handled."}]}}"#,
            // The person again, through the desktop app: origin stated one way, then the other.
            r#"{"type":"user","sessionId":"d","uuid":"u2","parentUuid":"a2","timestamp":"2026-11-01T00:00:04Z","entrypoint":"claude-desktop","promptSource":"sdk","origin":{"kind":"human"},"message":{"role":"user","content":"Yes, ship both"}}"#,
            r#"{"type":"assistant","sessionId":"d","uuid":"a3","parentUuid":"u2","timestamp":"2026-11-01T00:00:05Z","message":{"role":"assistant","model":"claude-fable-5-2","content":[{"type":"text","text":"Shipping both."}]}}"#,
            r#"{"type":"user","sessionId":"d","uuid":"u3","parentUuid":"a3","timestamp":"2026-11-01T00:00:06Z","entrypoint":"claude-desktop","promptSource":"sdk","turnOrigin":"human","message":{"role":"user","content":"now undo the second"}}"#,
            r#"{"type":"assistant","sessionId":"d","uuid":"a4","parentUuid":"u3","timestamp":"2026-11-01T00:00:07Z","message":{"role":"assistant","model":"claude-fable-5-2","content":[{"type":"text","text":"Second one undone."}]}}"#,
        ]
        .join("\n");

        let world = crate::test_world("cycles-claude-desktop-sdk");
        let path = world.proj.join("d.jsonl");
        std::fs::write(&path, &jsonl).unwrap();
        let adapter = crate::harness::claude::Claude::with_config_dir(&world.proj);
        let r = crate::ir::SessionRef::new("d", Harness::Claude, &path);

        let (staged, count) = stage_cycles(
            &adapter,
            &r,
            None,
            2,
            None,
            &Default::default(),
            &DatasetOptions::default(),
        )
        .unwrap()
        .expect("cycles staged");
        let mut out = Vec::new();
        staged.publish(&mut out).unwrap();
        let rows: Vec<serde_json::Value> = std::str::from_utf8(&out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // The first typed prompt opens the session; each later one is a cycle.
        assert_eq!(count, 2, "{rows:#?}");
        assert_eq!(rows[0]["prompt"], "Yes, ship both");
        assert_eq!(rows[0]["proof"], "record");
        assert_eq!(rows[0]["queued"], false);
        assert_eq!(rows[0]["report"], "Follow-up handled.");
        assert_eq!(rows[0]["plan"], "Shipping both.");
        assert_eq!(rows[1]["prompt"], "now undo the second");
        assert_eq!(rows[1]["plan"], "Second one undone.");
        assert!(
            rows.iter().all(|row| row["prompt"] != "automated follow-up"),
            "an sdk prompt with no stated human origin must not be a typed turn"
        );
    }

    #[test]
    fn sourced_non_shell_payloads_do_not_materialize_json_values_and_cross_harness_shells_classify() {
        let world = crate::test_world("cycles-jsonview-classify");
        let giant_path = world.base.join("write_input.json");
        let giant_payload = serde_json::json!({
            "file_path": "/repo/big.txt",
            "content": "X".repeat(2 * 1024 * 1024),
        })
        .to_string();
        std::fs::write(&giant_path, &giant_payload).unwrap();

        let write_call = Block::ToolUse {
            id: "w1".into(),
            name: "Write".into(),
            input: crate::Json::source(&giant_path, 0, giant_payload.len() as u64).unwrap(),
            namespace: None,
            extensions: Default::default(),
        };
        let codex_exec = Block::ToolUse {
            id: "e1".into(),
            name: "exec_command".into(),
            input: serde_json::json!({ "cmd": ["git", "-C", "/repo", "commit", "-m", "msg"] }).into(),
            namespace: None,
            extensions: Default::default(),
        };

        let msgs = vec![
            msg("u1", MessageKind::Prompt, vec![text("first")]),
            msg("a1", MessageKind::Reply, vec![text("wrote file"), write_call]),
            msg("u2", MessageKind::Prompt, vec![text("commit it")]),
            msg("a2", MessageKind::Reply, vec![text("committing"), codex_exec]),
        ];
        let resolver = crate::lazy::Resolver::new(None);
        let mut buf = Vec::new();
        let mut c = Cutter::new(Harness::Codex, None, None, &resolver, None, &mut buf);
        for m in &msgs {
            c.message(m).unwrap();
        }
        let (_, count) = c.finish().unwrap();
        assert_eq!(count, 1);
        let row: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(row["tools_before"], serde_json::json!({"Write": 1}));
        assert_eq!(row["commit_after"], true);
        assert_eq!(row["push_after"], false);
    }

    #[test]
    fn git_verbs_need_the_verb_after_git_options() {
        assert_eq!(git_verbs("git -C /r -c user.name=x commit -m m"), (true, false));
        assert_eq!(git_verbs("git push origin HEAD:main"), (false, true));
        assert_eq!(git_verbs("git log --grep commit"), (false, false));
        assert_eq!(git_verbs("echo git; commit"), (false, false));
    }
}
