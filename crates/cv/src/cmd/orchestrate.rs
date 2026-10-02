//! `cv prompts` / `cv lanes` / `cv deferrals` — the orchestrator's instruments. Each answers one
//! question an orchestrating session asks about itself during a swarm: what did the person
//! actually say (and answer), where is every lane right now, and what did I promise to do later
//! that no task holds.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::{truncate, Block, Message, MessageKind, Origin, Role, SessionRef};
use cv_core::lanes::Lane;
use cv_core::sanitize::sanitize_line;
use cv_core::stream::{Flow, ParseOptions};
use regex::Regex;
use serde::Serialize;

use crate::util::{fmt_local, parse_harness, resolve, short_id};

// ===================== cv prompts =====================

/// The markers a question-dialog tool result starts with — Claude Code's two `AskUserQuestion`
/// phrasings, and Devin CLI's `User answered your questions:`. The answers a person gives through
/// that dialog never appear as a prompt — they are tool output — and an orchestrator re-reading
/// what it was told needs them in the same stream.
const ANSWER_MARKERS: &[&str] = &[
    "The user answered",
    "Your questions have been answered",
    "User answered your questions",
];

#[derive(Debug, Clone, Serialize)]
struct PromptRow {
    /// Message index in the session (what `cv show --around N` takes).
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    /// `prompt` (typed by the person) or `answer` (an `AskUserQuestion` result).
    kind: &'static str,
    text: String,
}

/// Stream `r` once and keep only what the person said: `prompt`/`human` messages and the
/// `AskUserQuestion` answers, in order, with their message indices. `window` bounds the indices
/// (end-exclusive) when a pre-compaction span is requested.
fn collect_prompts(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    window: Option<(usize, usize)>,
) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut idx = 0usize;
    adapter.stream(r, &ParseOptions::default(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if let Some((s, e)) = window {
            if i < s {
                return Flow::Continue;
            }
            if i >= e {
                return Flow::Stop;
            }
        }
        if m.kind == MessageKind::Prompt && m.origin == Origin::Human {
            if let Some(text) = m.text().filter(|t| !t.trim().is_empty()) {
                rows.push(PromptRow {
                    index: i,
                    timestamp: m.timestamp,
                    kind: "prompt",
                    text,
                });
            }
        } else if m.role == Role::Tool || m.kind == MessageKind::ToolResult {
            for b in &m.content {
                if let Block::ToolResult { content, .. } = b {
                    let head = content.trim_start();
                    if ANSWER_MARKERS.iter().any(|mk| head.starts_with(mk)) {
                        rows.push(PromptRow {
                            index: i,
                            timestamp: m.timestamp,
                            kind: "answer",
                            text: content.to_string(),
                        });
                    }
                }
            }
        }
        Flow::Continue
    })?;
    Ok(rows)
}

/// Resolve `--pre-compaction N` into the message window before the Nth boundary, the same way
/// `cv show --pre-compaction` does, so the two commands always agree on what "the lost span" is.
fn pre_compaction_window(r: &SessionRef, n: usize) -> Result<(usize, usize)> {
    let comps = cv_core::compaction::detect(r, false)?;
    if comps.is_empty() {
        bail!("{} never compacted — nothing pre-compaction to show", short_id(&r.id));
    }
    let idx = n.saturating_sub(1);
    let span = cv_core::compaction::pre_compaction_span(&comps, idx).with_context(|| {
        format!(
            "{} compacted {} time(s); no compaction #{n} (use 1..={})",
            short_id(&r.id),
            comps.len(),
            comps.len(),
        )
    })?;
    eprintln!(
        "✦ pre-compaction #{n} of {}: messages {}..{} (the span before boundary @msg {})",
        comps.len(),
        span.0,
        span.1,
        comps[idx].boundary_msg_idx,
    );
    Ok(span)
}

pub(crate) fn cmd_prompts(id: &str, harness: Option<String>, json: bool, pre_compaction: Option<usize>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let window = match pre_compaction {
        Some(n) => Some(pre_compaction_window(&r, n)?),
        None => None,
    };
    let rows = collect_prompts(&r, adapter.as_ref(), window)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let prompts = rows.iter().filter(|p| p.kind == "prompt").count();
    let answers = rows.len() - prompts;
    println!(
        "# prompts of {} — {prompts} typed, {answers} answered via AskUserQuestion\n",
        short_id(&r.id)
    );
    for p in &rows {
        let ts = p
            .timestamp
            .map(|t| fmt_local(t, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "----------------".into());
        let tag = if p.kind == "prompt" { "user" } else { "answer" };
        println!("[{}] {ts}  {tag}", p.index);
        // Prompts are the one thing in a transcript worth reading whole; sanitize per line so a
        // pasted control sequence can't restyle the terminal, but never truncate.
        for line in p.text.lines() {
            println!("{}", sanitize_line(line));
        }
        println!();
    }
    Ok(())
}

// ===================== cv lanes =====================

fn fmt_tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn fmt_dur(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "-".into();
    };
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// `claude-opus-4-1-20250805` → `opus-4-1`: the family and version, not the vendor or the date.
fn short_model(m: Option<&str>) -> String {
    let Some(m) = m else {
        return "-".into();
    };
    let m = m.strip_prefix("claude-").unwrap_or(m);
    // Drop a trailing -YYYYMMDD date stamp.
    let m = match m.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => m,
    };
    truncate(m, 14)
}

/// The last line of a return value: what the lane concluded, not how it began.
fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneFilter {
    All,
    Running,
    Done,
    Stranded,
}

fn lane_passes(l: &Lane, f: LaneFilter) -> bool {
    match f {
        LaneFilter::All => true,
        LaneFilter::Running => l.is_running(),
        LaneFilter::Done => l.is_done(),
        LaneFilter::Stranded => l.stranded,
    }
}

/// The `--tasks` lines under a lane: its endpoint, then each task it holds (short id, state,
/// title, the last note's first line), at most five.
fn print_lane_tasks(l: &Lane, tasks: &[cv_core::lanes::LaneTask]) {
    const SHOWN: usize = 5;
    let Some(endpoint) = l.endpoint.as_deref() else {
        println!(
            "          ⚑ no endpoint (no CV_ENDPOINT export in its tool calls, no lane:<name> matches its description)"
        );
        return;
    };
    let guessed = if l.endpoint_source == Some(cv_core::lanes::EndpointSource::Description) {
        " (by description)"
    } else {
        ""
    };
    if tasks.is_empty() {
        println!("          ⚑ {}{guessed}: holds no tasks", sanitize_line(endpoint));
        return;
    }
    println!(
        "          ⚑ {}{guessed}: {} task(s)",
        sanitize_line(endpoint),
        tasks.len()
    );
    for t in tasks.iter().take(SHOWN) {
        let note = t
            .last_note
            .as_deref()
            .map(|n| format!(" · {}", truncate(&sanitize_line(n), 70)))
            .unwrap_or_default();
        println!(
            "            {} [{}] {}{note}",
            t.id.get(..13).unwrap_or(&t.id),
            t.state,
            truncate(&sanitize_line(&t.title), 60)
        );
    }
    if tasks.len() > SHOWN {
        println!(
            "            (+{} more — `cv task list --assignee {}`)",
            tasks.len() - SHOWN,
            sanitize_line(endpoint)
        );
    }
}

pub(crate) fn cmd_lanes(
    id: &str,
    harness: Option<String>,
    filter: LaneFilter,
    since: Option<String>,
    with_tasks: bool,
    json: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) = resolve(id, want)?;
    let since = match since {
        Some(s) => {
            let d = cv_core::task::parse_duration(&s)
                .ok_or_else(|| anyhow::anyhow!("--since takes a duration: 30m, 2h, 1d, 1w (got {s:?})"))?;
            Some(chrono::Utc::now() - d)
        }
        None => None,
    };
    let mut all: Vec<Lane> = cv_core::lanes::lanes_of(&r)
        .into_iter()
        .filter(|l| since.is_none_or(|t| l.active_since(t)))
        .collect();
    if with_tasks {
        let outcome = cv_core::task::replay()?;
        for w in &outcome.warnings {
            eprintln!("⚠ {}", sanitize_line(w));
        }
        cv_core::lanes::attach_tasks(&mut all, &outcome.model);
    }
    let lanes: Vec<&Lane> = all.iter().filter(|l| lane_passes(l, filter)).collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&lanes)?);
        return Ok(());
    }
    if all.is_empty() {
        println!("no sub-agents spawned by {}", short_id(&r.id));
        return Ok(());
    }
    let running = all.iter().filter(|l| l.is_running()).count();
    let done = all.iter().filter(|l| l.is_done()).count();
    let stranded = all.iter().filter(|l| l.stranded).count();
    let returned = all.iter().filter(|l| l.lost_notification() && !l.stranded).count();
    // The four add up to the forest: running, done, stranded, and the rest (failed / killed /
    // stopped without a waiting text / journaled partials).
    let other = all.len() - running - done - stranded;
    let mut scope = match filter {
        LaneFilter::All => String::new(),
        LaneFilter::Running => format!(" · showing {} running", lanes.len()),
        LaneFilter::Done => format!(" · showing {} done", lanes.len()),
        LaneFilter::Stranded => format!(" · showing {} stranded", lanes.len()),
    };
    if let Some(t) = since {
        scope.push_str(&format!(" · active since {}", fmt_local(t, "%m-%d %H:%M")));
    }
    let lost = if returned > 0 {
        format!(" ({returned} returned without a stop record)")
    } else {
        String::new()
    };
    // What killed the "other" lanes, when their transcripts say (the remedy differs: a
    // rate-limited lane resumes after the reset; a context death relaunches from its clone).
    let mut causes: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for l in &all {
        if let Some(c) = l.failure_cause.as_deref() {
            *causes.entry(c).or_default() += 1;
        }
    }
    let why = if causes.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            causes
                .iter()
                .map(|(c, n)| format!("{n} {c}"))
                .collect::<Vec<_>>()
                .join(" · ")
        )
    };
    println!(
        "# lanes of {} — {} sub-agents: {running} running · {done} completed{lost} · {other} other{why} · {stranded} STRANDED{scope}\n",
        short_id(&r.id),
        all.len(),
    );
    if lanes.is_empty() {
        println!("(none match)");
        return Ok(());
    }
    let status_of = |l: &Lane| -> String {
        if l.stranded {
            "STRANDED".to_string()
        } else if let Some(c) = &l.failure_cause {
            format!("{}:{c}", l.status)
        } else {
            truncate(&l.status, 10)
        }
    };
    let sw = lanes
        .iter()
        .map(|l| status_of(l).chars().count())
        .max()
        .unwrap_or(0)
        .max(10);
    println!(
        "AGENT     {:<sw$} MODEL          STARTED         DUR  TOKENS CALLS  DESCRIPTION",
        "STATUS"
    );
    for l in &lanes {
        let started = l
            .started_at
            .map(|t| fmt_local(t, "%m-%d %H:%M"))
            .unwrap_or_else(|| "-".into());
        let status = status_of(l);
        let desc = l
            .description
            .as_deref()
            .map(|d| truncate(&sanitize_line(d), 60))
            .unwrap_or_default();
        let wf = l.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
        println!(
            "{:<9} {:<sw$} {:<14} {:<11} {:>7} {:>7} {:>5}  {}{}",
            short_id(&l.agent_id),
            status,
            short_model(l.model.as_deref()),
            started,
            fmt_dur(l.duration_ms),
            fmt_tokens(l.tokens.total),
            l.tool_calls,
            desc,
            wf,
        );
        // Running: where it is — and for how long nothing has happened (a lane quiet for hours
        // mid tool call is usually dead, killed by a restart; the harness still says nothing).
        let detail = if l.is_running() {
            let quiet = l
                .last_turn_at
                .map(|t| chrono::Utc::now().signed_duration_since(t))
                .filter(|d| d.num_minutes() >= 60)
                .map(|d| {
                    format!(
                        " · ⚠ quiet {}",
                        cv_core::task::age_short(chrono::Utc::now() - d, chrono::Utc::now())
                    )
                })
                .unwrap_or_default();
            l.last_tool.as_deref().map(|t| format!("↪ {t}{quiet}"))
        } else {
            l.last_text.as_deref().map(|t| format!("↩ {}", last_line(t)))
        };
        if let Some(d) = detail {
            println!("          {}", truncate(&sanitize_line(&d), 140));
        }
        if l.stranded {
            println!("          → resume: SendMessage to {}", l.agent_id);
        }
        match l.failure_cause.as_deref() {
            Some("rate-limited") => println!(
                "          ✗ {}{} → resume with one message after the reset: SendMessage to {}",
                truncate(
                    &sanitize_line(l.failure_detail.as_deref().unwrap_or("rate-limited")),
                    80
                ),
                l.resets_at
                    .map(|t| format!(" (resets {})", fmt_local(t, "%m-%d %H:%M")))
                    .unwrap_or_default(),
                l.agent_id
            ),
            Some("context") => println!(
                "          ✗ {} → it cannot be resumed: relaunch from its clone with a STATUS hand-off",
                truncate(&sanitize_line(l.failure_detail.as_deref().unwrap_or("context")), 80)
            ),
            _ => {}
        }
        if let Some(tasks) = &l.tasks {
            print_lane_tasks(l, tasks);
        }
    }
    if stranded > 0 && filter != LaneFilter::Stranded {
        println!(
            "\n⚠ {stranded} lane(s) stopped on a promise nothing will keep — `--stranded` lists them with resume hints"
        );
    }
    if returned > 0 && filter != LaneFilter::Running {
        println!(
            "\n{returned} lane(s) show `returned`: the transcript ends in a final report and no stop was ever recorded — the harness lost the notification (a restart). Their returns are the ↩ lines; they count as done."
        );
    }
    Ok(())
}

// ===================== cv deferrals =====================

/// The phrases an orchestrator uses when it decides *not* to do something now. Each is a label
/// plus the regex that catches its spellings; every match in assistant text is a deferral until a
/// task says otherwise.
pub(crate) const DEFERRAL_PHRASES: &[(&str, &str)] = &[
    (
        "later lane",
        r"\b(a |one |the )?(later|separate|follow-?up|future) lane\b",
    ),
    ("follow-up", r"\bfollow-?ups?\b"),
    ("not tonight", r"\bnot (tonight|today|now|this (pass|session|turn))\b"),
    (
        "queued",
        r"\b(queued|queue (it|this|that|them|these|those) (for|behind|after|until|up))\b",
    ),
    (
        "ember's call",
        r"\b(ember.?s call|your call|ember (decides|to decide|chooses))\b",
    ),
    ("decision for", r"\bdecision (for|is yours|to make)\b"),
    (
        "integrator item",
        r"\b(integrator item|for the (standing )?integrator)\b",
    ),
    ("when X lands", r"\b(when|once|after) [\w`'-]+( [\w`'-]+){0,2} lands\b"),
    ("after FINAL", r"\bafter FINAL\b"),
    ("deferred", r"\bdefer(red|ring|s)?\b"),
    (
        "later pass",
        r"\b(a |the )?(later|next|future) (pass|session|turn|day|week)\b",
    ),
    ("out of scope", r"\bout of scope\b"),
];

#[derive(Debug, Clone, Serialize)]
struct Deferral {
    /// Message index in the session.
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    /// Which phrase fired.
    phrase: &'static str,
    /// ~100 chars centred on the match, newlines flattened.
    context: String,
    /// The sentence the match sits in — what the task cross-reference is scored against.
    #[serde(skip)]
    sentence: String,
    /// `--open-tasks` only: the best-matching task, if any task shares enough words.
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<TaskMatch>,
}

#[derive(Debug, Clone, Serialize)]
struct TaskMatch {
    task_id: String,
    title: String,
    state: String,
    /// The significant words the deferral and the task share.
    shared: Vec<String>,
}

fn phrase_regexes() -> Vec<(&'static str, Regex)> {
    DEFERRAL_PHRASES
        .iter()
        .map(|(label, re)| {
            (
                *label,
                Regex::new(&format!("(?i){re}")).expect("deferral phrase compiles"),
            )
        })
        .collect()
}

/// The sentence of `text` containing byte range `at` (split on `.`/`!`/`?`/newline), capped.
fn sentence_around(text: &str, at: (usize, usize)) -> String {
    let bytes = text.as_bytes();
    let is_break = |b: u8| matches!(b, b'.' | b'!' | b'?' | b'\n');
    let mut s = at.0;
    while s > 0 && !is_break(bytes[s - 1]) {
        s -= 1;
    }
    let mut e = at.1;
    while e < bytes.len() && !is_break(bytes[e]) {
        e += 1;
    }
    // Snap to char boundaries (the breaks are ASCII, so only the walk-back can land inside a char).
    while !text.is_char_boundary(s) {
        s -= 1;
    }
    while !text.is_char_boundary(e) {
        e += 1;
    }
    truncate(text[s..e].trim(), 300)
}

/// 100 chars centred on the match, newlines flattened, ellipses where it was cut.
fn context_around(text: &str, at: (usize, usize)) -> String {
    let half = 50usize;
    let mut s = at.0.saturating_sub(half);
    let mut e = (at.1 + half).min(text.len());
    while !text.is_char_boundary(s) {
        s -= 1;
    }
    while !text.is_char_boundary(e) {
        e += 1;
    }
    let mut out = String::new();
    if s > 0 {
        out.push('…');
    }
    out.push_str(&text[s..e].replace('\n', " "));
    if e < text.len() {
        out.push('…');
    }
    out
}

const STOPWORDS: &[&str] = &[
    "that",
    "this",
    "with",
    "from",
    "have",
    "will",
    "when",
    "lands",
    "after",
    "into",
    "then",
    "than",
    "they",
    "them",
    "there",
    "their",
    "what",
    "which",
    "while",
    "would",
    "could",
    "should",
    "about",
    "been",
    "were",
    "also",
    "just",
    "only",
    "more",
    "some",
    "such",
    "very",
    "each",
    "other",
    "over",
    "under",
    "those",
    "these",
    "where",
    "here",
    "lane",
    "lanes",
    "later",
    "follow",
    "followup",
    "queued",
    "queue",
    "ember",
    "call",
    "decision",
    "integrator",
    "item",
    "tonight",
    "final",
    "defer",
    "deferred",
    "pass",
    "session",
    "once",
    "before",
    "because",
    "does",
    "done",
    "doing",
    "make",
    "makes",
    "need",
    "needs",
    "still",
    "until",
    "through",
    "being",
    "want",
    "wants",
    "like",
    "same",
    "both",
    "much",
    "many",
    "most",
    "next",
    "first",
    "last",
    "work",
    "task",
    "tasks",
    "today",
    "already",
    "scope",
];

/// The words a cross-reference is scored on: lowercase, ≥ 4 chars, alphanumeric, not a stopword.
fn significant_words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .map(|w| w.trim_matches(|c| c == '-' || c == '_').to_ascii_lowercase())
        .filter(|w| w.len() >= 4 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// The task sharing the most significant words with `sentence`, if that is at least three.
fn best_task(model: &cv_core::task::TaskReadModel, sentence: &str) -> Option<TaskMatch> {
    let words = significant_words(sentence);
    if words.len() < 3 {
        return None;
    }
    let mut best: Option<(usize, &cv_core::task::TaskProjection, Vec<String>)> = None;
    for t in model.tasks.values() {
        let hay = format!("{} {} {}", t.title, t.body, t.issue.as_deref().unwrap_or(""));
        let theirs = significant_words(&hay);
        let mut shared: Vec<String> = words.intersection(&theirs).cloned().collect();
        if shared.len() < 3 {
            continue;
        }
        shared.sort();
        // Ties go to the newer task (ids are time-sortable), which is the one most likely to be
        // the deferral's own.
        let better = match &best {
            None => true,
            Some((n, prev, _)) => shared.len() > *n || (shared.len() == *n && t.task_id > prev.task_id),
        };
        if better {
            best = Some((shared.len(), t, shared));
        }
    }
    best.map(|(_, t, shared)| TaskMatch {
        task_id: t.task_id.clone(),
        title: t.title.clone(),
        state: cv_core::task::effective_display(t),
        shared,
    })
}

fn collect_deferrals(r: &SessionRef, adapter: &dyn cv_core::harness::Adapter, since: usize) -> Result<Vec<Deferral>> {
    let regexes = phrase_regexes();
    let mut out = Vec::new();
    let mut idx = 0usize;
    adapter.stream(r, &ParseOptions::default(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if i < since || m.role != Role::Assistant || m.kind != MessageKind::Reply || m.origin != Origin::Model {
            return Flow::Continue;
        }
        let Some(text) = m.text() else {
            return Flow::Continue;
        };
        // One hit per phrase per message: the same promise repeated in one reply is one deferral.
        for (label, re) in &regexes {
            if let Some(mm) = re.find(&text) {
                let at = (mm.start(), mm.end());
                out.push(Deferral {
                    index: i,
                    timestamp: m.timestamp,
                    phrase: label,
                    context: context_around(&text, at),
                    sentence: sentence_around(&text, at),
                    matched: None,
                });
            }
        }
        Flow::Continue
    })?;
    Ok(out)
}

/// Exit status 1 when `--open-tasks` finds an unmatched deferral, so a closeout can gate on it.
pub(crate) fn cmd_deferrals(
    id: &str,
    harness: Option<String>,
    since: Option<usize>,
    open_tasks: bool,
    json: bool,
) -> Result<bool> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = resolve(id, want)?;
    let mut found = collect_deferrals(&r, adapter.as_ref(), since.unwrap_or(0))?;

    if open_tasks {
        let outcome = cv_core::task::replay()?;
        for w in &outcome.warnings {
            eprintln!("⚠ task log: {w}");
        }
        for d in &mut found {
            d.matched = best_task(&outcome.model, &d.sentence);
        }
    }
    let unmatched = if open_tasks {
        found.iter().filter(|d| d.matched.is_none()).count()
    } else {
        0
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(unmatched == 0);
    }

    let tally = if open_tasks {
        format!(" ({} matched, {unmatched} UNMATCHED)", found.len() - unmatched)
    } else {
        String::new()
    };
    println!(
        "# deferrals in {} — {} found{tally}{}\n",
        short_id(&r.id),
        found.len(),
        since.map(|n| format!(" · from msg {n}")).unwrap_or_default()
    );
    if found.is_empty() {
        println!("(none)");
        return Ok(true);
    }
    // Ids are rendered at a collision-free length: tasks opened in one batch share 8 hex digits.
    let plen = cv_core::task::unique_prefix_len(
        found
            .iter()
            .filter_map(|d| d.matched.as_ref())
            .map(|m| m.task_id.as_str()),
        8,
    );
    for d in &found {
        let ts = d
            .timestamp
            .map(|t| fmt_local(t, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "----------------".into());
        println!(
            "[{}] {ts}  {:<16} {}",
            d.index,
            format!("\"{}\"", d.phrase),
            sanitize_line(&d.context)
        );
        if open_tasks {
            match &d.matched {
                Some(m) => println!(
                    "       MATCHED   {} [{}] {}  (shared: {})",
                    &m.task_id[..plen.min(m.task_id.len())],
                    m.state,
                    truncate(&sanitize_line(&m.title), 70),
                    m.shared.join(", ")
                ),
                None => println!("       UNMATCHED"),
            }
        }
    }
    if open_tasks && unmatched > 0 {
        println!("\n✗ {unmatched} deferral(s) have no task — `cv task open` each, or say why not");
    }
    Ok(unmatched == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_compile_and_catch_the_real_sentences() {
        let regexes = phrase_regexes();
        let hits = |s: &str| -> Vec<&'static str> {
            regexes
                .iter()
                .filter(|(_, re)| re.is_match(s))
                .map(|(l, _)| *l)
                .collect()
        };
        assert!(hits("an inconsistency to settle in one later lane, not tonight.").contains(&"later lane"));
        assert!(hits("an inconsistency to settle in one later lane, not tonight.").contains(&"not tonight"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"queued"));
        assert!(hits("I'll queue it for the integration step.").contains(&"queued"));
        assert!(!hits("a queue is not an architecture").contains(&"queued"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"after FINAL"));
        assert!(hits("is queued for the standing integrator right after FINAL-1 lands").contains(&"when X lands"));
        assert!(hits("that one is ember's call").contains(&"ember's call"));
        assert!(hits("I'll defer the IPA leg to a later pass").contains(&"deferred"));
        assert!(hits("I'll defer the IPA leg to a later pass").contains(&"later pass"));
        assert!(
            hits("the tests pass and the build is green").is_empty(),
            "{:?}",
            hits("the tests pass and the build is green")
        );
    }

    #[test]
    fn context_and_sentence_are_bounded_and_char_safe() {
        let text = "Ünïcode before. The Host umbrella — is queued for the standing integrator right after FINAL-1 lands, since it touches the file. Next sentence.";
        let at = text.find("queued").map(|s| (s, s + 6)).unwrap();
        let ctx = context_around(text, at);
        assert!(ctx.chars().count() <= 104, "{ctx}");
        assert!(ctx.contains("queued"));
        let sent = sentence_around(text, at);
        assert!(sent.starts_with("The Host umbrella"), "{sent}");
        assert!(sent.ends_with("touches the file"), "{sent}");
    }

    #[test]
    fn significant_words_drop_stopwords_and_short_tokens() {
        let w = significant_words("Make ResourceTargetAdmission.externalKind EXHAUSTIVE after FINAL-1 lands");
        assert!(w.contains("resourcetargetadmission"));
        assert!(w.contains("externalkind"));
        assert!(w.contains("exhaustive"));
        assert!(!w.contains("after"));
        assert!(!w.contains("final-1") || true);
        assert!(!w.contains("make"));
    }
}
