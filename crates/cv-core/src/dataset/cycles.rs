//! `cv dataset --unit cycle`: one flat JSONL record per human-turn cycle instead of one per session.
//!
//! A cycle is a typed prompt H after the session's first one, with the turn it reacts to and the
//! turn it starts: `report` is the last assistant text before H (the end-of-turn report the person
//! read), `plan` the first assistant text after H (the revised plan), and the tool calls of the span
//! before H (since the previous typed prompt) and after H (until the next one). This is the unit for
//! learning from how people steer agents (pushback, approvals, new asks), which a whole-session
//! record hides: there the person's turns and the harness's injected context both render as `user`.
//!
//! Typed means [`MessageKind::Prompt`] — the adapters already separate injected context, hook
//! output, notices and compaction summaries — minus shell passthrough (`!cmd` in Claude Code
//! records `<bash-input>`/`<bash-stdout>` as prompts: the person typed a command, not a message to
//! the agent). Records are staged per session and published only when the query accepts it.

use super::{select_trajectory, DatasetOptions, InputError, StagedRecord};
use crate::ir::{Block, Message, MessageKind};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::OnceLock;

/// `promptSource` values that say a person typed the turn (Claude Code).
const PERSON_SOURCES: [&str; 3] = ["typed", "queued", "suggestion_accepted"];
/// The framing Claude Code puts around a queued message when it renders it as a reminder.
const QUEUED_HEAD: &str = "The user sent the following message:";
const QUEUED_TAIL: &str = "Please address this message and continue with your tasks.";

/// A turn the person sent: its text, whether it was queued mid-turn, and how it is attested —
/// `record` when the record itself says a person sent it (`promptSource` typed/queued/
/// suggestion_accepted, `origin.kind`/`turnOrigin` human, a queued command in `prompt` mode),
/// `adapter` when only the adapter's kind and origin say so (older records, other harnesses).
struct HumanTurn {
    text: String,
    queued: bool,
    proof: &'static str,
}

fn str_at<'v>(v: Option<&'v serde_json::Value>, path: &[&str]) -> Option<&'v str> {
    let mut v = v?;
    for k in path {
        v = v.get(*k)?;
    }
    v.as_str()
}

/// Prompt prefixes that are shell passthrough, not a message to the agent.
const SHELL_PASSTHROUGH: [&str; 3] = ["<bash-input>", "<bash-stdout>", "<bash-stderr>"];

/// A turn relayed from another agent by a coordinator carries a `[from <seat>]` mark.
fn is_relayed(text: &Option<String>) -> bool {
    text.as_deref().is_some_and(|t| t.trim_start().starts_with("[from "))
}

/// Whether a prompt's text is typed for the agent (not shell passthrough).
pub fn is_typed_prompt_text(text: &str) -> bool {
    let t = text.trim_start();
    !SHELL_PASSTHROUGH.iter().any(|p| t.starts_with(p))
}

/// The shell command line a tool call carries, when it is one: Claude `Bash` `command`, Codex
/// `exec_command`/`shell` `cmd`/`command` (a string or an argv array).
fn shell_command(input: &serde_json::Value) -> Option<String> {
    let obj = input.as_object()?;
    for key in ["command", "cmd"] {
        match obj.get(key) {
            Some(serde_json::Value::String(s)) => return Some(s.clone()),
            Some(serde_json::Value::Array(a)) => {
                let parts: Vec<&str> = a.iter().filter_map(serde_json::Value::as_str).collect();
                if !parts.is_empty() {
                    return Some(parts.join(" "));
                }
            }
            _ => {}
        }
    }
    None
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

/// Cuts a session's messages (in order) into cycles; [`Cutter::finish`] returns the records.
pub struct Cutter<'a> {
    harness: crate::ir::Harness,
    last_human: Option<String>,
    last_queued: bool,
    resolver: &'a crate::lazy::Resolver,
    redact: Option<&'a crate::redact::RedactOptions>,
    prompts: usize,
    span: Span,
    pending: Option<Pending>,
    out: Vec<serde_json::Value>,
}

impl<'a> Cutter<'a> {
    pub fn new(
        harness: crate::ir::Harness,
        resolver: &'a crate::lazy::Resolver,
        redact: Option<&'a crate::redact::RedactOptions>,
    ) -> Self {
        Self {
            harness,
            last_human: None,
            last_queued: false,
            resolver,
            redact,
            prompts: 0,
            span: Span::default(),
            pending: None,
            out: Vec::new(),
        }
    }

    /// The person's turn `m` carries, if any.
    fn human_turn(&self, m: &Message) -> anyhow::Result<Option<HumanTurn>> {
        let bag = m.extra.get("claude");
        match m.kind {
            MessageKind::Prompt if m.origin == crate::ir::Origin::Human => {
                let source = str_at(bag, &["promptSource"]);
                if source.is_some_and(|s| !PERSON_SOURCES.contains(&s)) {
                    return Ok(None);
                }
                let text = self.raw_text(m)?;
                if !is_typed_prompt_text(&text) {
                    return Ok(None);
                }
                let stated = source.is_some()
                    || str_at(bag, &["origin", "kind"]) == Some("human")
                    || str_at(bag, &["turnOrigin"]) == Some("human");
                Ok(Some(HumanTurn {
                    text,
                    queued: source == Some("queued"),
                    proof: if stated { "record" } else { "adapter" },
                }))
            }
            MessageKind::InjectedContext if crate::harness::attachment_kind(m) == Some("queued_command") => {
                let mode = str_at(bag, &["commandMode"]).or(str_at(bag, &["attachment", "commandMode"]));
                let origin = str_at(bag, &["origin", "kind"]).or(str_at(bag, &["attachment", "origin", "kind"]));
                if mode.is_some_and(|m| m != "prompt") || origin.is_some_and(|o| o != "human") {
                    return Ok(None);
                }
                let raw = self.raw_text(m)?;
                let body = crate::harness::delivered_body(self.harness, m, &raw).unwrap_or(&raw);
                let body = body.trim();
                let body = body.strip_prefix(QUEUED_HEAD).unwrap_or(body);
                let body = body.trim().strip_suffix(QUEUED_TAIL).unwrap_or(body).trim();
                if body.is_empty() || body.starts_with("<task-notification>") {
                    return Ok(None);
                }
                Ok(Some(HumanTurn {
                    text: body.to_owned(),
                    queued: true,
                    proof: if mode.is_some() { "record" } else { "adapter" },
                }))
            }
            _ => Ok(None),
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

    fn text_of(&self, m: &Message) -> anyhow::Result<String> {
        Ok(self.scrub(self.raw_text(m)?))
    }

    fn raw_text(&self, m: &Message) -> anyhow::Result<String> {
        let mut s = String::new();
        for b in &m.content {
            if let Block::Text { text } = b {
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

    pub fn message(&mut self, m: &Message) -> anyhow::Result<()> {
        let at = m.timestamp.map(|t| t.to_rfc3339());
        if let Some(turn) = self.human_turn(m)? {
            // A queued message Claude Code also wrote again as a prompt is one turn.
            if (turn.queued || self.last_queued) && self.last_human.as_deref() == Some(turn.text.as_str()) {
                return Ok(());
            }
            self.last_human = Some(turn.text.clone());
            self.last_queued = turn.queued;
            self.prompts += 1;
            let span = std::mem::take(&mut self.span);
            if let Some(p) = self.pending.take() {
                self.close(p, &span);
            }
            if self.prompts > 1 {
                let report = span.last_text.clone();
                self.pending = Some(Pending {
                    index: self.prompts - 1,
                    prompt_id: m.id.clone(),
                    timestamp: at,
                    prompt: self.scrub(turn.text),
                    queued: turn.queued,
                    proof: turn.proof,
                    relayed: is_relayed(&self.last_human),
                    report,
                    before: span,
                });
            }
            return Ok(());
        }
        match m.kind {
            MessageKind::Reply | MessageKind::SubagentSpawn => {
                for b in &m.content {
                    if let Block::ToolUse { name, input, .. } = b {
                        *self.span.tools.entry(name.clone()).or_default() += 1;
                        self.span.calls += 1;
                        if let Some(cmd) = shell_command(input.resolve()?.as_ref()) {
                            let (c, p) = git_verbs(&cmd);
                            self.span.commit |= c;
                            self.span.push |= p;
                        }
                    }
                }
                if m.content.iter().any(|b| matches!(b, Block::Text { .. })) {
                    let text = self.text_of(m)?;
                    if !text.trim().is_empty() {
                        if self.span.first_text.is_none() {
                            self.span.first_text = Some((text.clone(), at.clone()));
                        }
                        self.span.last_text = Some((text, at));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self, p: Pending, after: &Span) {
        let (report, report_at) = p.report.map_or((None, None), |(t, a)| (Some(t), a));
        let (plan, plan_at) = after.first_text.clone().map_or((None, None), |(t, a)| (Some(t), a));
        self.out.push(serde_json::json!({
            "index": p.index,
            "prompt_id": p.prompt_id,
            "timestamp": p.timestamp,
            "report": report,
            "report_at": report_at,
            "prompt": p.prompt,
            "queued": p.queued,
            "proof": p.proof,
            "relayed": p.relayed,
            "plan": plan,
            "plan_at": plan_at,
            "tools_before": p.before.tools,
            "tool_calls_before": p.before.calls,
            "tools_after": after.tools,
            "tool_calls_after": after.calls,
            "commit_after": after.commit,
            "push_after": after.push,
        }));
    }

    /// Close the last cycle at end of input and return every cycle in order.
    pub fn finish(mut self) -> Vec<serde_json::Value> {
        let span = std::mem::take(&mut self.span);
        if let Some(p) = self.pending.take() {
            self.close(p, &span);
        }
        self.out
    }
}

/// Stream one session and stage its cycle records (one JSONL line each, session fields first),
/// or `None` when it has fewer than `min_messages` messages, the query rejects it, or it holds no
/// cycle. `opts.active_branch_only` cuts the active branch only (the session is held as lazy
/// messages for the selection pass); `clean_outcome_only` and `include_thinking` do not apply.
pub fn stage_cycles(
    adapter: &dyn crate::harness::Adapter,
    r: &crate::ir::SessionRef,
    redact: Option<&crate::redact::RedactOptions>,
    min_messages: usize,
    query: Option<&crate::SessionQuery>,
    text_sets: &crate::query::TextSets,
    opts: &DatasetOptions,
) -> anyhow::Result<Option<(StagedRecord, usize)>> {
    struct Sink<'a> {
        cutter: Cutter<'a>,
        held: Option<Vec<Message>>,
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
                match &mut self.held {
                    Some(held) => held.push(message),
                    None => self.cutter.message(&message)?,
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
    let mut sink = Sink {
        cutter: Cutter::new(r.harness, &resolver, redact),
        held: opts.active_branch_only.then(Vec::new),
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
    let mut cutter = sink.cutter;
    if let Some(held) = sink.held.take() {
        let select = DatasetOptions {
            clean_outcome_only: false,
            ..opts.clone()
        };
        for idx in select_trajectory(&held, &select).unwrap_or_default() {
            cutter.message(&held[idx])?;
        }
    }
    let cycles = cutter.finish();
    if cycles.is_empty() {
        return Ok(None);
    }
    let cwd = session.cwd.clone().or_else(|| r.cwd.clone());
    let mut bytes = std::io::BufWriter::new(crate::harness::store::bytes::ByteStage::new());
    for cycle in &cycles {
        let mut rec = serde_json::Map::new();
        rec.insert("harness".into(), r.harness.as_str().into());
        rec.insert("session".into(), r.id.clone().into());
        rec.insert("cwd".into(), cwd.as_ref().map(|p| p.display().to_string()).into());
        if let serde_json::Value::Object(fields) = cycle {
            rec.extend(fields.clone());
        }
        serde_json::to_writer(&mut bytes, &rec)?;
        bytes.write_all(b"\n")?;
    }
    let body = bytes.into_inner().map_err(|error| error.into_error())?.finish()?;
    Ok(Some((
        StagedRecord {
            leading: None,
            body,
            skip_header: 0,
        },
        cycles.len(),
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
        Block::Text { text: t.into() }
    }
    fn call(id: &str, name: &str, command: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({ "command": command }).into(),
            namespace: None,
        }
    }

    fn cut(messages: &[Message], redact: Option<&crate::redact::RedactOptions>) -> Vec<serde_json::Value> {
        let resolver = crate::lazy::Resolver::new(None);
        let mut c = Cutter::new(crate::ir::Harness::Claude, &resolver, redact);
        for m in messages {
            c.message(m).unwrap();
        }
        c.finish()
    }

    /// Invented session: a first ask, a report, an injected reminder, shell passthrough, a
    /// pushback that leads to a commit, then an approval.
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
        assert_eq!(
            last["plan"],
            serde_json::Value::Null,
            "the session ends after the approval"
        );
        assert_eq!(last["commit_after"], false);
        assert_eq!(last["index"], 2);
    }

    #[test]
    fn redaction_applies_to_cycle_text() {
        let mut s = session();
        // Assembled at run time so the source holds no key-shaped literal.
        let fake = format!("use {}-ant-api03-{}", "sk", "FAKEfakeFAKEfake0123456789abcdefFAKE");
        s[8] = msg("u2", MessageKind::Prompt, vec![text(&fake)]);
        let cycles = cut(&s, Some(&crate::redact::RedactOptions::default()));
        let p = cycles[0]["prompt"].as_str().unwrap();
        assert!(!p.contains("FAKEfake"), "{p}");
        assert!(p.contains("[REDACTED"), "{p}");
    }

    fn with_bag(mut m: Message, bag: serde_json::Value) -> Message {
        m.extra.insert("claude".into(), bag);
        m
    }

    #[test]
    fn queued_mid_turn_messages_are_human_turns_and_notifications_are_not() {
        let mut s = session();
        // Mid-turn, between the tool calls and the report: an unrendered queued command (older
        // Claude Code), and a queued task notification that must not count.
        let queued = with_bag(
            msg(
                "q1",
                MessageKind::InjectedContext,
                vec![text("stop, use the widget table")],
            ),
            serde_json::json!({"attachment_type": "queued_command", "commandMode": "prompt"}),
        );
        let notice = with_bag(
            msg(
                "q2",
                MessageKind::InjectedContext,
                vec![text("<task-notification>x</task-notification>")],
            ),
            serde_json::json!({"attachment_type": "queued_command", "commandMode": "task-notification"}),
        );
        s.insert(3, queued);
        s.insert(4, notice);
        let cycles = cut(&s, None);
        assert_eq!(cycles.len(), 3);
        let q = &cycles[0];
        assert_eq!(q["prompt"], "stop, use the widget table");
        assert_eq!(q["queued"], true);
        assert_eq!(q["proof"], "record");
        assert_eq!(
            q["report"], "Looking at the frobnicator.",
            "the text before the delivery point"
        );
        assert_eq!(q["tools_before"], serde_json::json!({"Read": 1}));
        assert_eq!(q["plan"], "Done: it counts frobs.");
        assert_eq!(cycles[1]["report"], "Done: it counts frobs.");
    }

    #[test]
    fn a_rendered_queued_message_is_unwrapped_and_its_echo_is_one_turn() {
        let mut s = session();
        let rendered = with_bag(
            msg(
                "q1",
                MessageKind::InjectedContext,
                vec![text("<system-reminder>\nThe user sent the following message:\nuse widgets\n\nPlease address this message and continue with your tasks.\n</system-reminder>")],
            ),
            serde_json::json!({"attachment_type": "queued_command"}),
        );
        s.insert(3, rendered);
        s.insert(4, msg("e1", MessageKind::Prompt, vec![text("use widgets")]));
        let cycles = cut(&s, None);
        assert_eq!(cycles.len(), 3);
        assert_eq!(cycles[0]["prompt"], "use widgets");
        assert_eq!(cycles[0]["proof"], "adapter", "no commandMode on the record");
    }

    #[test]
    fn prompt_source_grades_or_rejects_a_prompt() {
        let mut s = session();
        s[8] = with_bag(s[8].clone(), serde_json::json!({"promptSource": "typed"}));
        s[12] = with_bag(s[12].clone(), serde_json::json!({"promptSource": "system"}));
        s.push(msg("u4", MessageKind::Prompt, vec![text("[from lead] relay this")]));
        let cycles = cut(&s, None);
        assert_eq!(cycles.len(), 2, "u2 and the relayed u4; u3 is a system prompt");
        assert_eq!(cycles[0]["proof"], "record");
        assert_eq!(cycles[1]["proof"], "adapter");
        assert_eq!(cycles[1]["relayed"], true);
        assert_eq!(cycles[0]["relayed"], false);
    }

    #[test]
    fn git_verbs_need_the_verb_after_git_options() {
        assert_eq!(git_verbs("git -C /r -c user.name=x commit -m m"), (true, false));
        assert_eq!(git_verbs("git push origin HEAD:main"), (false, true));
        assert_eq!(git_verbs("git log --grep commit"), (false, false));
        assert_eq!(git_verbs("echo git; commit"), (false, false));
        assert_eq!(
            shell_command(&serde_json::json!({"cmd": ["git", "commit", "-m", "x"]})).as_deref(),
            Some("git commit -m x")
        );
    }
}
