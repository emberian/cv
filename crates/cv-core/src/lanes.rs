//! The sub-agent forest as **lanes**: one row per sub-agent with what an orchestrator asks of it —
//! who it is, what it ran on, how long, how much, where it is now, and whether it is parked.
//!
//! [`crate::subagent_tree_of`] lists the forest; this module reads each transcript once (a
//! streamed pass, content inline, usage deduplicated by API `message.id` exactly as
//! `cv stats --tokens` does) and pairs it with two harness-side signals: the parent transcript's
//! `<task-notification>` records (the status Claude Code itself reported for the child) and the
//! child's own `SubagentStop` hook attachment (it stopped; a `SendMessage` resume appends turns
//! after it). The strand class — an agent that stopped with a final text saying it is *waiting*
//! for something that will never wake it — is flagged from those two facts and the text.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::harness::claude::{subagent_end, task_notices, ApiErrorNotice, SubagentEnd, TaskNotice};
use crate::ir::{Block, Message, MessageKind, Role, SessionRef, Usage};
use crate::stream::{Flow, ParseOptions};
use crate::SubagentInfo;

/// Token totals for one lane, following [`Usage`]'s disjoint convention.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct LaneTokens {
    pub calls: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// `input + cache_read + cache_write + output`.
    pub total: u64,
}

impl LaneTokens {
    fn add(&mut self, u: &Usage) {
        self.calls += 1;
        self.input += u.input_tokens.unwrap_or(0);
        self.cache_read += u.cache_read_tokens.unwrap_or(0);
        self.cache_write += u.cache_creation_tokens.unwrap_or(0);
        self.output += u.output_tokens.unwrap_or(0);
        self.total = self.input + self.cache_read + self.cache_write + self.output;
    }
}

/// Where a lane's `status` string was read from, so a reader knows how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    /// A `Workflow` run's `journal.jsonl` result record (`done` / `partial` / `failed` / …).
    Journal,
    /// The parent transcript's last `<task-notification>` for this agent (`completed` /
    /// `failed` / `killed` / `stopped`).
    TaskNotification,
    /// The child's own `SubagentStop` hook attachment after its last turn (`stopped`).
    SubagentStop,
    /// Nothing says it stopped: the transcript ends mid-work (`running`) — or, with no stop
    /// recorded anywhere, it ends in a final report that has sat untouched for
    /// [`RETURNED_QUIET_SECS`] (`returned`: the lane finished and the harness lost the
    /// notification, typically across a restart).
    Transcript,
}

/// How long a transcript must be quiet after a text-only final turn before `returned` is
/// inferred — long enough that a tool call still being streamed out never reads as a return.
pub const RETURNED_QUIET_SECS: i64 = 600;

/// One sub-agent, summarized.
#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    /// The bare `agentId` (the `agent-` prefix stripped) — what `SendMessage` and the journal use.
    pub agent_id: String,
    /// The transcript's session id (`agent-<agent_id>`), as `cv show` resolves it.
    pub session_id: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// The last conversational record (`user`/`assistant`) in the transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub messages: usize,
    pub tool_calls: u64,
    pub tokens: LaneTokens,
    /// `running` · `completed` · `stopped` · `failed` · `killed` · `returned` (finished, no stop
    /// recorded — a lost notification), or a journaled workflow status.
    pub status: String,
    pub status_source: StatusSource,
    /// The last assistant text turn — the return value for a finished agent, the last narration
    /// for a running one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_text: Option<String>,
    /// The last tool call (`Bash · cargo build …`): where a running or dead agent was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// Parked on a promise nothing will keep: stopped, and its last text says it is waiting.
    pub stranded: bool,
    /// Why a `failed` / `killed` lane died, read from its transcript's last API-error notice:
    /// `rate-limited` (an API 429 / session or usage limit — resume after the reset),
    /// `context` (prompt too long — relaunch from its clone, it cannot be resumed), or `stopped`
    /// (no error; the harness's own stop hook fired). `None` = undetermined: plain `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_cause: Option<String>,
    /// For `rate-limited`: when the quota resets (`quotaLimits.resetsAt`), when the record says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// For `rate-limited` / `context`: the notice's own text (`You've hit your session limit ·
    /// resets 9pm (America/New_York)`, `Prompt is too long`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// The task-store endpoint this lane acts as (`lane:<name>`): the first `CV_ENDPOINT=…` its
    /// own tool calls exported, else (after [`attach_tasks`]) a `lane:<slug>` assignee matching
    /// the description's leading token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Where `endpoint` came from: `transcript` (exact) or `description` (a guess by name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_source: Option<EndpointSource>,
    /// The tasks the endpoint holds (assignee), newest activity first — present only when joined
    /// ([`attach_tasks`]; `cv lanes --tasks`, the serve page's Lanes pane).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<LaneTask>>,
}

/// How a lane's task-store endpoint was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    /// The lane's own tool call exported `CV_ENDPOINT=…` (first occurrence) — exact.
    Transcript,
    /// No export seen; the description's leading token matched a `lane:<slug>` assignee.
    Description,
}

/// One task a lane's endpoint holds, as the lane table shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneTask {
    pub id: String,
    pub title: String,
    pub state: String,
    /// The first line of the task's last note, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_note: Option<String>,
    pub last_ts: DateTime<Utc>,
}

/// The first `CV_ENDPOINT=<value>` in a tool call's input (`export CV_ENDPOINT=lane:x; …`,
/// `CV_ENDPOINT="lane:x" cv task …`).
pub fn endpoint_in(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"CV_ENDPOINT=["']?([A-Za-z0-9_.@/-]+:[A-Za-z0-9_.@/:-]+)"#).expect("endpoint regex")
    });
    re.captures(text).map(|c| c[1].to_string())
}

/// The description's leading token as a lane slug: `FIX-KICK: a kick ends authority…` →
/// `fix-kick`. `None` for a description with no plausible token.
pub fn description_slug(description: &str) -> Option<String> {
    let token: String = description
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    (token.len() >= 2).then(|| token.to_lowercase())
}

/// Join each lane to the task store: resolve its endpoint (the transcript's export, else a
/// `lane:<slug>` assignee matching the description's leading token, case-insensitively) and list
/// the tasks that endpoint holds, newest activity first.
pub fn attach_tasks(lanes: &mut [Lane], model: &crate::task::TaskReadModel) {
    let assignees: std::collections::BTreeSet<&str> =
        model.tasks.values().filter_map(|t| t.assignee.as_deref()).collect();
    for lane in lanes.iter_mut() {
        if lane.endpoint.is_none() {
            if let Some(slug) = lane.description.as_deref().and_then(description_slug) {
                let want = format!("lane:{slug}");
                if let Some(hit) = assignees.iter().find(|a| a.to_lowercase() == want) {
                    lane.endpoint = Some(hit.to_string());
                    lane.endpoint_source = Some(EndpointSource::Description);
                }
            }
        }
        let Some(endpoint) = lane.endpoint.as_deref() else {
            lane.tasks = Some(Vec::new());
            continue;
        };
        let mut rows: Vec<LaneTask> = model
            .tasks
            .values()
            .filter(|t| {
                t.assignee
                    .as_deref()
                    .is_some_and(|a| crate::task::same_actor(a, endpoint))
            })
            .map(|t| LaneTask {
                id: t.task_id.clone(),
                title: t.title.clone(),
                state: crate::task::effective_display(t),
                last_note: t.notes.last().map(|n| {
                    n.text
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("")
                        .to_string()
                }),
                last_ts: t.last_ts,
            })
            .collect();
        rows.sort_by_key(|t| std::cmp::Reverse(t.last_ts));
        lane.tasks = Some(rows);
    }
}

impl Lane {
    /// Finished for real: a terminal status AND not stranded. A stranded lane is `completed` as
    /// far as the harness knows — that is exactly the trap — so it never counts as done here.
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "done" | "returned") && !self.stranded
    }

    /// Finished with no stop recorded anywhere: the harness lost this lane's notification.
    pub fn lost_notification(&self) -> bool {
        self.status == "returned"
    }

    /// Any activity (start or last turn) at or after `since`.
    pub fn active_since(&self, since: DateTime<Utc>) -> bool {
        self.started_at.is_some_and(|t| t >= since) || self.last_turn_at.is_some_and(|t| t >= since)
    }

    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
}

/// The phrases that mark a final text as a parked promise. Each is matched case-insensitively
/// against the tail of the last assistant text. They are the shapes that stranded four lanes on
/// one day (`Waiting on notifications`, `I'll continue when the monitor fires`, `waiting for the
/// … verdict`), generalized just enough to catch their siblings and no further.
pub const STRAND_PATTERNS: &[&str] = &[
    r"waiting (on|for) ([\w'’-]+ ){0,5}(notification|notifications|monitor|verdict|verdicts|build|builds|result|results|lane|lanes|run|job|report|reply|answer)\b",
    r"continue (when|once|after) (the |that |this |it )?\w*( \w+){0,2} ?(fires|lands|arrives|finishes|completes|returns|comes back|is ready|reports)\b",
    r"\b(i.?ll|i will|will) (continue|resume|pick (this|it|that) (back )?up|check back|follow up|report back|proceed) (when|once|after|as soon as)\b",
    r"\b(parked|paused|standing by|on hold) (until|for|pending)\b",
    r"\bawaiting (the |a )?(notification|notifications|verdict|result|results|monitor|build|reply|answer)\b",
    r"\b(when|once|after) the (monitor|notification|watcher|timer) fires\b",
];

fn strand_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        let joined = STRAND_PATTERNS
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|");
        Regex::new(&format!("(?i){joined}")).expect("strand patterns compile")
    })
}

/// Does a final text read as a parked promise? A promise is how a parked text *ends*, so only
/// its last two sentences are consulted — a report that mentions waiting in its middle and then
/// concludes is not stranded — and past-tense narration (`was waiting on … earlier`) is removed
/// before matching, since the regex engine has no lookbehind to exclude it in place.
pub fn text_is_waiting(text: &str) -> bool {
    let sentences: Vec<&str> = text
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = sentences.len().saturating_sub(2);
    let tail = sentences[start..].join(". ");
    static PAST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let past = PAST.get_or_init(|| {
        Regex::new(r"(?i)\b(was|were|had been|have been|has been) (waiting|standing by|parked)\b").unwrap()
    });
    let present = past.replace_all(&tail, "");
    strand_regex().is_match(&present)
}

/// Classify a dead lane's cause from its transcript's last uncleared API-error notice (see
/// [`Lane::failure_cause`]). Only `failed` / `killed` lanes get a cause; everything else is `None`.
pub fn failure_cause(status: &str, end: &SubagentEnd) -> Option<&'static str> {
    if !matches!(status, "failed" | "killed") {
        return None;
    }
    match &end.api_error {
        Some(e) => classify_api_error(e),
        None if end.stopped() == Some(true) => Some("stopped"),
        None => None,
    }
}

/// `rate-limited` / `context` from one API-error notice, else `None`.
pub fn classify_api_error(e: &ApiErrorNotice) -> Option<&'static str> {
    let text = e.text.to_lowercase();
    let error = e.error.as_deref().unwrap_or("");
    if error == "rate_limit"
        || e.status == Some(429)
        || ["session limit", "usage limit", "rate limit", "rate_limit"]
            .iter()
            .any(|p| text.contains(p))
    {
        return Some("rate-limited");
    }
    if [
        "prompt is too long",
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|p| text.contains(p))
    {
        return Some("context");
    }
    None
}

/// One line for a tool call: the tool name plus the argument a reader would look at first.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let arg = pick(&[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "description",
        "prompt",
        "url",
        "skill",
    ])
    .unwrap_or_else(|| match input {
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    });
    let arg = crate::ir::truncate(&arg, 80);
    if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name} · {arg}")
    }
}

/// What one streamed pass over a transcript yields.
#[derive(Default)]
struct Pass {
    model: Option<String>,
    messages: usize,
    tool_calls: u64,
    tokens: LaneTokens,
    last_text: Option<String>,
    last_tool: Option<String>,
    /// The transcript's last conversational turn is an assistant text with no tool call: the
    /// shape of a final report (a running lane ends in a tool call or a tool result).
    ends_in_report: bool,
    /// The first `CV_ENDPOINT=…` a tool call of this lane exported.
    endpoint: Option<String>,
}

fn pass(r: &SessionRef) -> Option<Pass> {
    let adapter = crate::harness::for_harness(r.harness)?;
    let mut p = Pass::default();
    let mut seen = std::collections::HashSet::new();
    // `extra` on so Claude's `message.id` (the usage dedupe key) is materialized; `spans` off so
    // text blocks are inline and `Message::text` is safe — content is read per message and dropped.
    let opts = ParseOptions {
        extra: true,
        ..ParseOptions::default()
    };
    let session = adapter
        .stream(r, &opts, &mut |m: Message| {
            if m.kind.is_model_visible() && matches!(m.role, Role::User | Role::Assistant) {
                p.messages += 1;
            }
            if m.role == Role::User && matches!(m.kind, MessageKind::Prompt | MessageKind::ToolResult) {
                p.ends_in_report = false;
            }
            if m.role == Role::Assistant {
                let calls_a_tool = m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
                let has_text = m.kind == MessageKind::Reply && m.text().is_some_and(|t| !t.trim().is_empty());
                if calls_a_tool {
                    p.ends_in_report = false;
                } else if has_text {
                    p.ends_in_report = true;
                }
                if p.model.is_none() {
                    p.model = m.model.clone().filter(|s| !s.is_empty());
                }
                if let Some(u) = m.usage.as_ref().filter(|u| u.total_tokens() > 0) {
                    let dup = crate::stats::usage_key(r.harness, &m).is_some_and(|k| !seen.insert(k));
                    if !dup {
                        p.tokens.add(u);
                    }
                }
                if m.kind == MessageKind::Reply {
                    if let Some(t) = m.text().filter(|t| !t.trim().is_empty()) {
                        p.last_text = Some(t);
                    }
                }
                for b in &m.content {
                    if let Block::ToolUse { name, input, .. } = b {
                        p.tool_calls += 1;
                        p.last_tool = Some(tool_summary(name, input));
                        if p.endpoint.is_none() {
                            p.endpoint = match input.get("command").and_then(Value::as_str) {
                                Some(cmd) => endpoint_in(cmd),
                                None => endpoint_in(&input.to_string()),
                            };
                        }
                    }
                }
            }
            Flow::Continue
        })
        .ok()?;
    if p.model.is_none() {
        p.model = session.model.clone();
    }
    Some(p)
}

/// Resolve the status of one lane from its three possible sources, most authoritative first;
/// with none of them, the transcript's own shape decides between `running` and `returned`.
fn status_of(
    sub: &SubagentInfo,
    end: &SubagentEnd,
    notice: Option<&TaskNotice>,
    ends_in_report: bool,
    now: DateTime<Utc>,
) -> (String, StatusSource) {
    if let Some(s) = sub.result_status.as_deref().filter(|s| !s.is_empty()) {
        return (s.to_string(), StatusSource::Journal);
    }
    // A notification that post-dates the last turn describes the current stop; an older one
    // describes a stop the agent has since been resumed from.
    if let Some(n) = notice {
        let current = match (n.ts, end.last_turn_at) {
            (Some(nt), Some(lt)) => nt >= lt,
            (Some(_), None) => true,
            (None, _) => end.stopped() == Some(true),
        };
        if current {
            return (n.status.clone(), StatusSource::TaskNotification);
        }
    }
    if end.stopped() == Some(true) {
        return ("stopped".into(), StatusSource::SubagentStop);
    }
    // No journal, no notification, no stop hook — but the transcript ends in a final report and
    // has been quiet for a while: the lane returned and nobody recorded it (a harness restart
    // drops the pending notification). One "running" row was a lane that had finished 15 h
    // earlier, found this way.
    let quiet = end
        .last_turn_at
        .is_some_and(|t| now.signed_duration_since(t).num_seconds() >= RETURNED_QUIET_SECS);
    if ends_in_report && quiet {
        return ("returned".into(), StatusSource::Transcript);
    }
    ("running".into(), StatusSource::Transcript)
}

fn lane_of(sub: SubagentInfo, notices: &HashMap<String, TaskNotice>) -> Option<Lane> {
    let p = pass(&sub.session)?;
    // `subagent_end` reads Claude's jsonl sidecars; other harnesses' child sessions live inside
    // their own stores (a `.db` path here), where a jsonl scan is meaningless.
    let end = if sub.session.harness == crate::ir::Harness::Claude {
        subagent_end(&sub.session.path)
    } else {
        SubagentEnd::default()
    };
    let agent_id = sub.agent_id().to_string();
    let (status, status_source) = status_of(&sub, &end, notices.get(&agent_id), p.ends_in_report, Utc::now());
    let started_at = sub.session.created_at;
    let last_turn_at = end.last_turn_at.or(sub.session.updated_at);
    let duration_ms = match (started_at, last_turn_at) {
        (Some(s), Some(e)) if e >= s => Some((e - s).num_milliseconds() as u64),
        _ => None,
    };
    // A workflow agent's journaled summary is its real return; a direct agent's is its last text.
    let last_text = sub.result_summary.clone().or(p.last_text);
    let parked = matches!(status.as_str(), "completed" | "stopped" | "returned");
    let stranded = parked && last_text.as_deref().is_some_and(text_is_waiting);
    let cause = failure_cause(&status, &end);
    let notice = end
        .api_error
        .as_ref()
        .filter(|_| matches!(cause, Some("rate-limited" | "context")));
    Some(Lane {
        agent_id,
        session_id: sub.session.id.clone(),
        path: sub.session.path.clone(),
        agent_type: sub.agent_type,
        description: sub.description,
        tool_use_id: sub.tool_use_id,
        workflow: sub.workflow,
        model: p.model,
        started_at,
        last_turn_at,
        duration_ms,
        messages: p.messages,
        tool_calls: p.tool_calls,
        tokens: p.tokens,
        status,
        status_source,
        last_text,
        last_tool: p.last_tool,
        stranded,
        failure_cause: cause.map(str::to_string),
        resets_at: notice.and_then(|n| n.resets_at),
        failure_detail: notice.map(|n| n.text.clone()),
        endpoint_source: p.endpoint.as_ref().map(|_| EndpointSource::Transcript),
        endpoint: p.endpoint,
        tasks: None,
    })
}

/// Every sub-agent of `parent` as a [`Lane`], **launch order** (oldest first — the order the
/// orchestrator issued them, which is how a status table reads). Transcripts that fail to parse
/// are skipped (counted nowhere: one bad file must not hide a fleet).
pub fn lanes_of(parent: &SessionRef) -> Vec<Lane> {
    let subs = crate::subagent_tree_of(parent);
    let notices = task_notices(&parent.path);
    let mut lanes = crate::par_filter_map(subs, |s| lane_of(s, &notices));
    lanes.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.agent_id.cmp(&b.agent_id)));
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_real_strand_phrasings_match_and_a_report_does_not() {
        for t in [
            "Build is mirrored. Waiting on notifications.",
            "the build is still running; I'll continue when the monitor fires",
            "Both lanes are green. I'm now waiting for the integrator's verdict before touching main.",
            "Nothing else to do here until the hbox build finishes — standing by for the notification.",
        ] {
            assert!(text_is_waiting(t), "should read as waiting: {t:?}");
        }
        for t in [
            "sdk-ts is fixed: npm test goes from 97/106 to 110/110. The work is three commits on sdk-ts-repair.",
            "I was waiting on notifications earlier, but the build landed; the lane is complete and the branch is pushed.",
            "K-RAN works: the kernel re-executes a runner's claimed Nock run and admits the writes only when they match.",
        ] {
            assert!(!text_is_waiting(t), "should NOT read as waiting: {t:?}");
        }
    }

    #[test]
    fn only_the_tail_is_consulted() {
        let mut long = String::from("Waiting on notifications. ");
        long.push_str(&"The lane then did a great deal of work. ".repeat(40));
        long.push_str("Done; branch pushed.");
        assert!(!text_is_waiting(&long));
    }

    #[test]
    fn endpoint_and_slug_are_read_from_the_shapes_lanes_write() {
        assert_eq!(
            endpoint_in("export CV_ENDPOINT=lane:cv-edits; cv task list --repo /x").as_deref(),
            Some("lane:cv-edits")
        );
        assert_eq!(
            endpoint_in(r#"CV_ENDPOINT="lane:fix-kick" cv task claim 01a0"#).as_deref(),
            Some("lane:fix-kick")
        );
        assert_eq!(endpoint_in("cv task inbox ember"), None);
        // A format string or a shell variable is not an endpoint: keep looking.
        assert_eq!(
            endpoint_in("export CV_ENDPOINT={owner}; CV_ENDPOINT=$X; CV_ENDPOINT=lane:real").as_deref(),
            Some("lane:real")
        );
        assert_eq!(
            description_slug("FIX-KICK: a kick ends authority").as_deref(),
            Some("fix-kick")
        );
        assert_eq!(description_slug("CV-EDITS").as_deref(), Some("cv-edits"));
        assert_eq!(description_slug(": nothing"), None);
    }

    /// The record shapes of session 0c315aee's 10-01/02 kills (content redacted to the notice):
    /// the session-limit 429s are `rate-limited` with the quota's reset, the integrator's
    /// `Prompt is too long` is `context`, a resumed lane's old error is history, and a lane that
    /// is not dead gets no cause.
    #[test]
    fn failure_cause_reads_the_last_uncleared_api_error() {
        let rate = ApiErrorNotice {
            ts: None,
            text: "You've hit your session limit · resets 9pm (America/New_York)".into(),
            error: Some("rate_limit".into()),
            status: Some(429),
            resets_at: DateTime::from_timestamp(1_790_902_800, 0),
        };
        let ctx = ApiErrorNotice {
            text: "Prompt is too long".into(),
            error: Some("invalid_request".into()),
            ..ApiErrorNotice::default()
        };
        let end = |e: Option<ApiErrorNotice>| SubagentEnd {
            api_error: e,
            ..SubagentEnd::default()
        };
        assert_eq!(failure_cause("failed", &end(Some(rate.clone()))), Some("rate-limited"));
        assert_eq!(failure_cause("killed", &end(Some(ctx.clone()))), Some("context"));
        assert_eq!(failure_cause("completed", &end(Some(rate))), None, "not dead: no cause");
        assert_eq!(
            failure_cause("failed", &end(None)),
            None,
            "nothing says why: plain failed"
        );
        let other = ApiErrorNotice {
            text: "API Error: 500 internal".into(),
            ..ApiErrorNotice::default()
        };
        assert_eq!(failure_cause("failed", &end(Some(other))), None);
        let stopped = SubagentEnd {
            stopped_at: DateTime::from_timestamp(10, 0),
            last_turn_at: DateTime::from_timestamp(5, 0),
            api_error: None,
        };
        assert_eq!(failure_cause("failed", &stopped), Some("stopped"));
        // The usage-limit phrasing without a status is still a rate limit.
        let usage = ApiErrorNotice {
            text: "Claude AI usage limit reached|1759370400".into(),
            ..ApiErrorNotice::default()
        };
        assert_eq!(classify_api_error(&usage), Some("rate-limited"));
    }

    #[test]
    fn tool_summary_picks_the_telling_argument() {
        assert_eq!(
            tool_summary("Bash", &serde_json::json!({"command": "cargo build", "timeout": 5})),
            "Bash · cargo build"
        );
        assert_eq!(
            tool_summary("Read", &serde_json::json!({"file_path": "/x/y.rs"})),
            "Read · /x/y.rs"
        );
        assert_eq!(tool_summary("TaskStop", &serde_json::json!({})), "TaskStop");
    }
}
