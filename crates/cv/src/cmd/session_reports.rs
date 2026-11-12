//! `cv show --kind prompt,answer` and `cv lanes` — the orchestrator's instruments. Each answers
//! one question an orchestrating session asks about itself during a swarm: what did the person
//! actually say (and answer), and where is every lane right now.

use anyhow::Result;
use chrono::{DateTime, Utc};
use cv_core::ir::{truncate, Block, Message, MessageKind, Origin, Role, SessionRef};
use cv_core::lanes::Lane;
use cv_core::sanitize::sanitize_line;
use cv_core::stream::{Demand, Flow};
use serde::Serialize;

use crate::util::{fmt_local, home_rel, parse_harness, resolve, short_id};

// ===================== cv show --kind prompt,answer =====================

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
/// (`[start, end)`, `None` end = through the last) when a window or pre-compaction span is given.
fn collect_prompts(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    window: Option<(usize, Option<usize>)>,
    prompts: bool,
    answers: bool,
) -> Result<Vec<PromptRow>> {
    let mut rows = Vec::new();
    let mut idx = 0usize;
    let resolver = cv_core::Resolver::new(Some(r.path.clone()));
    let mut error = None;
    adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut |m: Message| {
        let i = idx;
        idx += 1;
        if let Some((s, e)) = window {
            if i < s {
                return Flow::Continue;
            }
            if e.is_some_and(|e| i >= e) {
                return Flow::Stop;
            }
        }
        let selected = (|| -> Result<()> {
            if prompts && m.kind == MessageKind::Prompt && m.origin == Origin::Human {
                let mut text = String::new();
                for block in &m.content {
                    if let Block::Text { text: body } = block {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        body.try_for_each_chunk(&resolver, 64 * 1024, |part| {
                            text.push_str(part);
                            Ok(())
                        })?;
                    }
                }
                if !text.trim().is_empty() {
                    rows.push(PromptRow {
                        index: i,
                        timestamp: m.timestamp,
                        kind: "prompt",
                        text,
                    });
                }
            } else if answers && (m.role == Role::Tool || m.kind == MessageKind::ToolResult) {
                for block in &m.content {
                    if let Block::ToolResult { content, .. } = block {
                        if is_question_answer(content, &resolver)? {
                            rows.push(PromptRow {
                                index: i,
                                timestamp: m.timestamp,
                                kind: "answer",
                                text: content.try_resolve(&resolver)?.into_owned(),
                            });
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(failure) = selected {
            error = Some(failure);
            return Flow::Stop;
        }
        Flow::Continue
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(rows)
}

fn is_question_answer(content: &cv_core::Text, resolver: &cv_core::Resolver) -> Result<bool> {
    let mut head = String::new();
    let mut started = false;
    let mut answer = false;
    let result = content.try_for_each_chunk(resolver, 4096, |part| {
        for c in part.chars() {
            if !started && c.is_whitespace() {
                continue;
            }
            started = true;
            head.push(c);
            if ANSWER_MARKERS.iter().any(|marker| head.starts_with(marker)) {
                answer = true;
                return Err(cv_core::stream::Stop.into());
            }
            if !ANSWER_MARKERS.iter().any(|marker| marker.starts_with(&head)) {
                return Err(cv_core::stream::Stop.into());
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        if !error.is::<cv_core::stream::Stop>() {
            return Err(error);
        }
    }
    Ok(answer)
}

/// What the person said in `r`, within `window`: typed prompts and/or `AskUserQuestion` answers,
/// as text (each printed whole) or as one JSON array of `index, timestamp, kind, text` rows.
pub(crate) fn print_prompts(
    r: &SessionRef,
    adapter: &dyn cv_core::harness::Adapter,
    window: Option<(usize, Option<usize>)>,
    prompts: bool,
    answers: bool,
    json: bool,
) -> Result<()> {
    let rows = collect_prompts(r, adapter, window, prompts, answers)?;
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
    /// Not finished for real: running, stopped, killed, failed, or stranded — [`Lane::is_done`]'s
    /// complement. Across recent sessions this is the rescue list for `cv adopt`.
    Unfinished,
}

/// Which forests `cv lanes` reads.
pub(crate) enum LaneScope {
    /// One session's sub-agents.
    Session { id: String, harness: Option<String> },
    /// The sub-agents of the `n` most recently active Claude sessions whose recorded cwd is `dir`
    /// — a project's recent swarms in one table (each row carries its `session_id`).
    Recent { n: usize, dir: std::path::PathBuf },
}

fn lane_passes(l: &Lane, f: LaneFilter) -> bool {
    match f {
        LaneFilter::All => true,
        LaneFilter::Running => l.is_running(),
        LaneFilter::Done => l.is_done(),
        LaneFilter::Stranded => l.stranded,
        LaneFilter::Unfinished => !l.is_done(),
    }
}

/// Two directories are the same when their canonical paths agree; a path that no longer exists
/// (a session's recorded cwd from a deleted checkout) compares as spelled.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
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
    scope: LaneScope,
    filter: LaneFilter,
    since: Option<String>,
    with_tasks: bool,
    json: bool,
) -> Result<()> {
    let multi = matches!(scope, LaneScope::Recent { .. });
    let (title, empty_note, sessions): (String, String, Vec<SessionRef>) = match scope {
        LaneScope::Session { id, harness } => {
            let want = parse_harness(&harness)?;
            let (r, _adapter) = resolve(&id, want)?;
            (
                format!("lanes of {}", short_id(&r.id)),
                format!("no sub-agents spawned by {}", short_id(&r.id)),
                vec![r],
            )
        }
        LaneScope::Recent { n, dir } => {
            let mut refs: Vec<SessionRef> = cv_core::sessions()
                .into_iter()
                .filter(|r| {
                    cv_core::harness::rootless(r.harness).is_some_and(|a| a.capabilities().lane_stops.is_some())
                })
                .filter(|r| r.cwd.as_deref().is_some_and(|c| same_dir(c, &dir)))
                .collect();
            refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
            refs.truncate(n);
            let ids: Vec<String> = refs.iter().map(|r| short_id(&r.id)).collect();
            (
                format!(
                    "lanes of the {} most recent session(s) in {} ({})",
                    refs.len(),
                    home_rel(&dir),
                    ids.join(" ")
                ),
                format!("no sessions with lanes in {}", home_rel(&dir)),
                refs,
            )
        }
    };
    let since = match since {
        Some(s) => {
            let d = cv_core::task::parse_duration(&s)
                .ok_or_else(|| anyhow::anyhow!("--since takes a duration: 30m, 2h, 1d, 1w (got {s:?})"))?;
            Some(
                chrono::Utc::now()
                    .checked_sub_signed(d)
                    .ok_or_else(|| anyhow::anyhow!("--since duration is outside the supported date range: {s:?}"))?,
            )
        }
        None => None,
    };
    // Which forest each lane came from, keyed by its transcript path: a `Lane` carries only its
    // own `agent-…` ref, and an adopted agent can sit in two sessions at once.
    let mut parent_of: std::collections::HashMap<std::path::PathBuf, String> = std::collections::HashMap::new();
    let mut all: Vec<Lane> = Vec::new();
    for r in &sessions {
        for l in cv_core::lanes::lanes_of(r)? {
            if since.is_none_or(|t| l.active_since(t)) {
                parent_of.insert(l.path.clone(), r.id.clone());
                all.push(l);
            }
        }
    }
    if with_tasks && all.iter().any(|lane| lane_passes(lane, filter)) {
        let outcome = cv_core::task::replay()?;
        for w in &outcome.warnings {
            eprintln!("⚠ {}", sanitize_line(w));
        }
        cv_core::lanes::attach_tasks(&mut all, &outcome.model);
    }
    let lanes: Vec<&Lane> = all.iter().filter(|lane| lane_passes(lane, filter)).collect();

    if json {
        if multi {
            // `--recent` rows carry the forest they came from as `parent_id`; one session's rows
            // keep the plain `lane` shape.
            let rows: Vec<serde_json::Value> = lanes
                .iter()
                .map(|l| {
                    let mut v = serde_json::to_value(l).unwrap_or_default();
                    v["parent_id"] = serde_json::json!(parent_of.get(&l.path));
                    v
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&lanes)?);
        }
        return Ok(());
    }
    if all.is_empty() {
        println!("{empty_note}");
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
        LaneFilter::Unfinished => format!(" · showing {} unfinished", lanes.len()),
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
        "# {title} — {} sub-agents: {running} running · {done} completed{lost} · {other} other{why} · {stranded} STRANDED{scope}\n",
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
            truncate(l.status.as_str(), 10)
        }
    };
    let sw = lanes
        .iter()
        .map(|l| status_of(l).chars().count())
        .max()
        .unwrap_or(0)
        .max(10);
    // Across sessions every row says which forest it belongs to.
    let session_col = |l: &Lane| {
        if multi {
            format!(
                "{:<9} ",
                parent_of.get(&l.path).map(|p| short_id(p)).unwrap_or_default()
            )
        } else {
            String::new()
        }
    };
    let indent = if multi { "                    " } else { "          " };
    println!(
        "{}AGENT     {:<sw$} MODEL          STARTED         DUR  TOKENS CALLS  DESCRIPTION",
        if multi { "SESSION   " } else { "" },
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
            "{}{:<9} {:<sw$} {:<14} {:<11} {:>7} {:>7} {:>5}  {}{}",
            session_col(l),
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
            println!("{indent}{}", truncate(&sanitize_line(&d), 140));
        }
        if l.stranded {
            println!("{indent}→ resume: SendMessage to {}", l.agent_id);
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
    // `cv adopt` moves lanes between sessions of a harness with derived copies (Claude's).
    let adoptable = sessions.iter().any(|r| cv_core::harness::has_derived_copies(r.harness));
    if multi && filter == LaneFilter::Unfinished && !lanes.is_empty() && adoptable {
        println!("\n→ rescue into a live session: cv adopt <agent-id>… --into <live-session>  (the transcript moves; see `cv adopt --help`)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_selectors_only_resolve_selected_bodies_and_keep_window_indices() -> Result<()> {
        struct Fixture(Vec<Message>);
        impl cv_core::Adapter for Fixture {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<cv_core::Discovered> {
                Ok(Default::default())
            }
            fn stream(
                &self,
                r: &SessionRef,
                opts: &Demand,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<cv_core::Session> {
                assert!(opts.spans);
                for m in &self.0 {
                    if sink.try_message(m.clone())? == Flow::Stop {
                        break;
                    }
                }
                Ok(cv_core::Session::new(&r.id, r.harness))
            }
        }
        let world = crate::test_world("prompt-selected-demands");
        let prompt_path = world.base.join("prompt.txt");
        let answer_path = world.base.join("answer.txt");
        let prompt = "typed🌙\n".repeat(1000);
        let answer = format!("{}The user answered with a complete answer", "\u{2003}".repeat(3000));
        // A span covers a JSON string's body.
        let (prompt_json, answer_json) = (serde_json::to_string(&prompt)?, serde_json::to_string(&answer)?);
        std::fs::write(&prompt_path, &prompt_json)?;
        std::fs::write(&answer_path, &answer_json)?;
        let span = |path: &std::path::Path, len: usize| {
            cv_core::Text::Span(cv_core::Span {
                source: Some(path.into()),
                offset: 1,
                len: len as u64,
                escaped: true,
            })
        };
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: span(&prompt_path, prompt_json.len() - 2),
        });
        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "question".into(),
            content: span(&answer_path, answer_json.len() - 2),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        let mut irrelevant = Message::new(Role::Assistant);
        irrelevant.content.push(Block::Text {
            text: span(&world.base.join("not-demanded"), 8000000),
        });
        let fixture = Fixture(vec![irrelevant, user, tool]);
        let r = SessionRef::new("selection", cv_core::Harness::Claude, world.base.join("source"));
        let both = collect_prompts(&r, &fixture, None, true, true)?;
        assert_eq!(both.len(), 2);
        assert_eq!((both[0].index, both[0].text.as_str()), (1, prompt.as_str()));
        assert_eq!((both[1].index, both[1].text.as_str()), (2, answer.as_str()));
        let window = collect_prompts(&r, &fixture, Some((2, Some(3))), true, true)?;
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].index, 2);
        std::fs::remove_file(&answer_path)?;
        assert_eq!(collect_prompts(&r, &fixture, None, true, false)?[0].text, prompt);
        assert!(collect_prompts(&r, &fixture, None, false, true).is_err());
        Ok(())
    }

    #[test]
    fn answer_marker_projection_matches_trim_start_at_partial_and_unicode_boundaries() -> Result<()> {
        for text in [
            "",
            "The user",
            "The user answered",
            "Your questions have been answered:\nreply",
            "\u{2003}\tUser answered your questions:reply",
            "ordinary output",
            "The user answeréd",
        ] {
            assert_eq!(
                is_question_answer(&text.into(), &cv_core::Resolver::new(None))?,
                ANSWER_MARKERS
                    .iter()
                    .any(|marker| text.trim_start().starts_with(marker))
            );
        }
        Ok(())
    }
}
