//! `cv task` — the fleet task substrate: open/claim/note/done dispatch objects and the decisions
//! posed on them. Completion is self-reported and labeled so; cv never claims to have observed
//! what it has not.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, TaskEvent, TaskEventKind, TaskReadModel, TaskRow, TaskStore};

use super::task_ops::{self, DecisionSpec, EventFilter, Since};
use crate::util::{fmt_local, usage};

/// A text argument that may come from a file: `--body-file`/`--file` (`-` reads stdin). Bodies
/// were 500-char shell strings before this existed; a brief is a document.
fn read_text_arg(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            eprintln!("cv task: reading the text from the terminal; end it with Ctrl-D");
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the text from stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// `a,b, c` → `["a", "b", "c"]`: trimmed, empties dropped, duplicates collapsed in order.
fn parse_tags(csv: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in csv.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|have| have == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// The id-prefix length that keeps every task in `model` distinguishable (never below 8). Task
/// ids are UUID v7: a batch opened in one second shares its first eight hex digits, so the usual
/// 8-char prefix was ambiguous for every task opened that way — `cv task show <prefix>` refused
/// what `cv task list` had just printed.
pub(crate) fn prefix_len(model: &TaskReadModel) -> usize {
    task::unique_prefix_len(model.tasks.keys().map(String::as_str), 8)
}

pub(crate) fn prefix(id: &str, len: usize) -> &str {
    id.get(..len).unwrap_or(id)
}

/// A text argument where `-` means stdin (`--body -`, `--note -`, a bare `-` note).
fn text_or_stdin(text: String) -> Result<String> {
    if text == "-" {
        read_text_arg(Path::new("-"))
    } else {
        Ok(text)
    }
}

/// `--issue` absolutized at open time, because the record outlives the shell it was typed in —
/// a relative `--issue` made `sweep` report "issue path no longer exists" the next day. A
/// relative path is taken against the current directory; when it is not there but is under the
/// task's `--repo`, the repo wins (that is where it was meant). Nothing found: joined to the cwd
/// lexically. Handles (`#42`) and URLs pass through untouched.
fn absolutize_issue(issue: Option<String>, repo: Option<&Path>) -> Option<String> {
    let issue = issue?;
    let Some(p) = issue_path(&issue) else {
        return Some(issue);
    };
    let p = Path::new(p);
    if p.is_absolute() {
        return Some(issue);
    }
    let cwd = std::env::current_dir().ok();
    let in_cwd = cwd.as_ref().map(|d| d.join(p));
    let in_repo = repo.map(|r| r.join(p));
    let chosen = match (&in_cwd, &in_repo) {
        (Some(c), _) if c.exists() => c.clone(),
        (_, Some(r)) if r.exists() => r.clone(),
        (Some(c), _) => c.clone(),
        (None, Some(r)) => r.clone(),
        (None, None) => p.to_path_buf(),
    };
    let abs = chosen.canonicalize().unwrap_or(chosen);
    Some(abs.display().to_string())
}

/// The caller's own endpoint for scoping (`$CV_ENDPOINT`), and the default 14-day window.
fn scope_window(all: bool, since: Option<&str>, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    if all {
        return Ok(None);
    }
    match since {
        Some(s) => Ok(Some(task::parse_since(s, now).map_err(|e| anyhow::anyhow!(e))?)),
        None => Ok(Some(now - chrono::Duration::days(14))),
    }
}

/// Identity resolution (G4) for chat-grade verbs: explicit `--from`, else `CV_ENDPOINT`, else
/// the literal `"cv"` (a human opening a task from a shell owes no ceremony). Shared logic lives
/// in [`task::actor`]; only the CLI's default sink is chosen here.
fn from_or_cv(explicit: Option<String>) -> String {
    task::actor(explicit, "cv")
}

/// Identity resolution for identity-BEARING verbs (claim/release/status):
/// [`task::require_actor`], with the error naming this surface's `--from` flag.
fn require_from(explicit: Option<String>) -> Result<String> {
    task::require_actor(explicit, "--from")
}

/// `--token` was the TOFU credential of the retired trust layer. The flag is kept hidden so the
/// old spelling errors with a pointer (exit 2) instead of being silently accepted.
fn refuse_token(token: Option<String>) -> Result<()> {
    if token.is_some() {
        return usage(
            "`--token` / `CV_TOKEN` were removed in 0.15.0 — identity is `--from` / `CV_ENDPOINT`, \
             recorded and labeled, never authenticated",
        );
    }
    Ok(())
}

/// `--channel` named the board channel task events were mirrored to; the board is gone.
fn refuse_channel(channel: Option<String>) -> Result<()> {
    if channel.is_some() {
        return usage(
            "`--channel` was removed in 0.15.0 with the coordination board — task events are mirrored \
             nowhere; follow them with `cv task events --since <id>`",
        );
    }
    Ok(())
}

/// The pointer every retired land-facet verb prints (exit 2).
fn removed_land_verb(verb: &str) -> Result<()> {
    usage(format!(
        "`cv task {verb}` was removed in 0.15.0 with the land facet — completion is self-reported and \
         labeled: `cv task done <id> --observed <evidence>`, evidence in `cv task note`, candidates \
         from `cv task sweep --repo <path>`"
    ))
}

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// Open a new task. Prints its id.
    Open {
        title: String,
        /// The body; `-` reads it from stdin (the natural form for a heredoc — backticks inside a
        /// double-quoted shell string are command-substituted by zsh).
        #[arg(long, default_value = "", conflicts_with = "body_file")]
        body: String,
        /// Read the body from a file (`-` for stdin) — a brief is a document, not a shell string.
        #[arg(long = "body-file", value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Repository this task's work happens in (`list --repo` filters on it; `sweep` reads it).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// External issue/work handle, free-form. A relative path is made absolute at open time
        /// (the record outlives the shell it was typed in).
        #[arg(long)]
        issue: Option<String>,
        // Removed in 0.15.0 with the coordination board — hidden so the old flag errors with a
        // pointer (exit 2).
        #[arg(long, hide = true)]
        channel: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        /// Comma-separated tags (`deploy,lean`); `list --tag` filters on them. `decision` is refused
        /// here: a decision is POSED with `cv task decide … --default …`, so it has options.
        #[arg(long, value_name = "A,B")]
        tags: Option<String>,
        /// This task waits on another (id or unique prefix). Repeatable. Blocked-ness is computed
        /// from the blocker's live state — it clears by itself when the blocker finishes.
        #[arg(long = "blocked-by", value_name = "ID")]
        blocked_by: Vec<String>,
        /// Another task waits on this one (writes a `blocked_by` on THAT task). Repeatable.
        #[arg(long = "blocks", value_name = "ID")]
        blocks: Vec<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// List tasks: non-terminal, touched in the last 14 days or involving you ($CV_ENDPOINT).
    /// The hidden count is the last line; `--all` lifts both the window and the terminal filter.
    List {
        /// Filter by state (open|claimed|resolved|done|abandoned|superseded).
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Only tasks carrying this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Only decisions (posed questions) / only actions (work).
        #[arg(long, group = "kind")]
        decisions: bool,
        #[arg(long, group = "kind")]
        actions: bool,
        /// Everything: every project, every age, terminal tasks included.
        #[arg(long)]
        all: bool,
        /// Widen or narrow the window: tasks touched since (`30d`, `2h`, `2026-09-01`, an event id).
        #[arg(long, value_name = "WHEN", conflicts_with = "all")]
        since: Option<String>,
        #[arg(long)]
        json: bool,
        /// Tab-separated rows for scripts: full id, state, assignee, repo basename, age, title,
        /// blocked_by — no alignment, no truncation.
        #[arg(long, conflicts_with = "json")]
        tsv: bool,
        // Removed in 0.15.0 — hidden so the old flag errors with a pointer (exit 2).
        #[arg(long, hide = true)]
        wide: bool,
    },
    /// Show one task (id may be a unique prefix).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
        /// Also print the raw event history.
        #[arg(long)]
        events: bool,
        /// One line per note (author, time, first 150 chars) and the body's first line — a
        /// 26-note task on one screen. On a task with a pinned STATUS: only the title and the
        /// STATUS (the relay hand-off view).
        #[arg(long)]
        brief: bool,
        /// Print only the pinned STATUS text (exit 1 when none is pinned).
        #[arg(long, conflicts_with_all = ["json", "brief", "events"])]
        status: bool,
        /// Only the last N notes.
        #[arg(long = "notes-last", value_name = "N")]
        notes_last: Option<usize>,
        /// Only notes matching this regex (case-insensitive).
        #[arg(long = "notes-grep", value_name = "PATTERN")]
        notes_grep: Option<String>,
    },
    /// Claim an open task (first writer wins).
    Claim {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        // Removed in 0.15.0 — hidden so the old flag errors with a pointer (exit 2).
        #[arg(long, hide = true)]
        token: Option<String>,
    },
    /// Release a claim back to open.
    Release {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        #[arg(long, hide = true)]
        token: Option<String>,
    },
    /// Record a progress note. `cv task note <id> -` reads the note from stdin.
    Note {
        id: String,
        /// The note (`-` = stdin). Omit it and pass `--file` to read the note from a file.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        text: Option<String>,
        /// Read the note from a file (`-` for stdin).
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Complete a task. Self-reported and labeled so: `--observed` is free text cv records, never
    /// a claim cv checked. Refused on a decision (answer it with `resolve`).
    Done {
        id: String,
        /// Pointer to observable evidence (URL, path, session id).
        #[arg(long)]
        observed: Option<String>,
        // Removed in 0.15.0 — hidden so the old flags error with a pointer (exit 2).
        #[arg(long = "check-cmd", hide = true)]
        check_cmd: Option<String>,
        #[arg(long = "check-file", hide = true)]
        check_file: Option<PathBuf>,
        #[arg(long = "check-http", hide = true)]
        check_http: Option<String>,
        /// A closing note (`-` = stdin), validated with the `done` before appending: the note,
        /// then the done.
        #[arg(long, conflicts_with = "note_file")]
        note: Option<String>,
        /// The closing note from a file (`-` for stdin).
        #[arg(long = "note-file", value_name = "PATH")]
        note_file: Option<PathBuf>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Pin the task's current STATUS (replaces the previous one; history stays in the events):
    /// the relay hand-off — tip, what is merged, what runs, what is next. `show` prints it first.
    /// By the task's assignee or opener; refused on a terminal task.
    Status {
        id: String,
        /// The status text (`-` = stdin). Omit it and pass `--file`.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        text: Option<String>,
        /// Read the status from a file (`-` for stdin).
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Acting endpoint recorded in `by` (must be the assignee or the opener). Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        #[arg(long, hide = true)]
        token: Option<String>,
    },
    /// Kill a task.
    Abandon {
        id: String,
        #[arg(long, default_value = "no reason given")]
        reason: String,
        /// A closing note (`-` = stdin), appended with the abandon as ONE unit.
        #[arg(long, conflicts_with = "note_file")]
        note: Option<String>,
        /// The closing note from a file (`-` for stdin).
        #[arg(long = "note-file", value_name = "PATH")]
        note_file: Option<PathBuf>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Mark a task superseded by another.
    Supersede {
        id: String,
        #[arg(long = "by")]
        by_task: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Add tags to a task (comma-separated; additive, never removes).
    Tag {
        id: String,
        #[arg(value_name = "A,B")]
        tags: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record that a task waits on another: `cv task block <id> --by <blocker>`.
    Block {
        id: String,
        /// The task that must finish first (id or unique prefix).
        #[arg(long = "by", value_name = "ID")]
        by_task: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Pose a decision for someone: a task of kind `decision` with a default that stands if they
    /// stay silent, the alternatives, and an optional deadline. Prints its id.
    Decide {
        title: String,
        /// Who owes the decision (their `cv task inbox` lists it first).
        #[arg(long = "for", value_name = "WHO")]
        for_who: String,
        /// The option that stands if nobody speaks (always one of the options).
        #[arg(long, value_name = "OPTION")]
        default: String,
        /// Another admissible option. Repeatable.
        #[arg(long = "option", value_name = "OPTION")]
        options: Vec<String>,
        /// When the default stands: a duration (`3d`, `48h`), a date (`2026-10-03`) or a datetime.
        #[arg(long = "by", value_name = "WHEN")]
        by: Option<String>,
        /// The question in full (`-` = stdin).
        #[arg(long, default_value = "", conflicts_with = "body_file")]
        body: String,
        #[arg(long = "body-file", value_name = "PATH")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        issue: Option<String>,
        #[arg(long, hide = true)]
        channel: Option<String>,
        /// Extra tags (`decision` is always added).
        #[arg(long, value_name = "A,B")]
        tags: Option<String>,
        /// This decision blocks another task (writes a `blocked_by` on THAT task). Repeatable.
        #[arg(long = "blocks", value_name = "ID")]
        blocks: Vec<String>,
        /// Resolve it on its default NOW, on the decider's behalf (by you, the poser), with a veto
        /// window: work proceeds, and their inbox shows it under "made for you (veto?)" until they
        /// `--confirm` or choose otherwise. Validated in the same batch as the pose.
        #[arg(long)]
        provisional: bool,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        #[arg(long, hide = true)]
        token: Option<String>,
    },
    /// Answer a decision: `--accept-default`, or `--choice "<option>"` (one of the posed options).
    /// Records WHO resolved it: pass `--from <you>` or set `CV_ENDPOINT` once. On a decision made
    /// for you provisionally, `--confirm` keeps its choice and `--choice` vetoes it.
    Resolve {
        id: String,
        #[arg(long, value_name = "OPTION", group = "answer", required = true)]
        choice: Option<String>,
        #[arg(long = "accept-default", group = "answer")]
        accept_default: bool,
        /// Confirm a provisional resolution (the decider answers with the choice made for them).
        #[arg(long, group = "answer")]
        confirm: bool,
        /// Resolve on the default on the decider's behalf, with a veto window (the poser's verb;
        /// only `--accept-default` is admissible).
        #[arg(long, conflicts_with = "confirm")]
        provisional: bool,
        /// Why (`-` = stdin). Recorded IN the resolution event, so it lands with it.
        #[arg(long, conflicts_with = "note_file")]
        note: Option<String>,
        /// The resolution's note from a file (`-` for stdin).
        #[arg(long = "note-file", value_name = "PATH")]
        note_file: Option<PathBuf>,
        /// The resolver, recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
        #[arg(long, hide = true)]
        token: Option<String>,
    },
    /// New task events since a point in time, one JSON line each (the poll surface: hand the
    /// last event id back as the next `--since`). `--text` renders one human line per event.
    Events {
        /// A duration (`2h`), a date, a datetime, an RFC 3339 timestamp, or an event id (exclusive).
        #[arg(long, value_name = "WHEN")]
        since: Option<String>,
        /// Only these kinds (comma-separated: `resolved,done,noted`; `note`/`resolve` spellings ok).
        #[arg(long, value_name = "K,K")]
        kind: Option<String>,
        /// Only events by this endpoint.
        #[arg(long)]
        by: Option<String>,
        /// Hide events by this endpoint (`web:<x>` counts as `<x>`).
        #[arg(long = "not-by", value_name = "WHO")]
        not_by: Option<String>,
        /// Only events on tasks assigned to this endpoint.
        #[arg(long)]
        assignee: Option<String>,
        /// Only events on this task (id or prefix).
        #[arg(long)]
        task: Option<String>,
        /// One human line per event instead of JSON lines.
        #[arg(long)]
        text: bool,
    },
    // Removed in 0.15.0: the inbox is a page on `cv serve` (hidden stub: pointer, exit 2).
    #[command(hide = true)]
    Serve {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    /// Candidates for closing, from what git and the filesystem say — never closes anything.
    /// Lists non-terminal tasks whose `--issue` path no longer exists, or whose title/body names a
    /// branch now merged into the repo's main (`git branch --merged`).
    Sweep {
        /// The repository to observe (branches are read from it; a task's own `--repo` must
        /// match, or be unset).
        #[arg(long)]
        repo: PathBuf,
        /// The branch merged work lands on (default: `main`, else `master` if that is what exists).
        #[arg(long)]
        main: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// What needs `<who>`: decisions owed (default + options on one line each), decisions made
    /// for them provisionally, then assigned actions and claimed work. Same 14-day window as `list`.
    Inbox {
        /// Endpoint to compute the inbox for. Default: $CV_ENDPOINT, else "cv".
        who: Option<String>,
        #[arg(long)]
        json: bool,
        /// The whole inbox as a Markdown page: each item's body and the first line of each note.
        #[arg(long, conflicts_with = "json")]
        md: bool,
        /// Only items whose last event is NOT by `<who>` (news for them).
        #[arg(long)]
        unread: bool,
        /// Every age (no 14-day window).
        #[arg(long)]
        all: bool,
        /// Items touched since (`30d`, `2026-09-01`, …).
        #[arg(long, value_name = "WHEN", conflicts_with = "all")]
        since: Option<String>,
    },
    // ───────────── Removed in 0.15.0 (hidden stubs: pointer, exit 2) ─────────────
    /// `split` turned `DECIDE:` notes into decisions; `decide` poses one directly.
    #[command(hide = true)]
    Split {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    /// `watch` was `events --assignee <who> --not-by $CV_ENDPOINT` under a second name.
    #[command(hide = true)]
    Watch {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Propose {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Reroute {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Pass {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Refute {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Verify {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Debt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Stats {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
}

/// Replay the default store, print any log warnings loudly, return the outcome.
fn replay_loud() -> Result<cv_core::task::ReplayOutcome> {
    let outcome = task::replay()?;
    for w in &outcome.warnings {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    Ok(outcome)
}

/// Append an agent event through the shared core path, print a one-line confirmation.
fn append_and_report(task_id: Option<&str>, from: &str, kind: TaskEventKind) -> Result<()> {
    append_event(task_id, from, kind).map(|_| ())
}

/// [`append_and_report`], returning the event's task id (what `open` needs to attach its tags
/// and relations to the task it just created).
fn append_event(task_id: Option<&str>, from: &str, kind: TaskEventKind) -> Result<String> {
    let store = TaskStore::default_store();
    let report = task::append(&store, task_id, from, kind, Vec::new())?;
    for w in report.replay_warnings.iter().chain(&report.warnings) {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    let state = report.effective_state.unwrap_or_else(|| "?".into());
    println!(
        "✦ {} {} → {}",
        report.event.kind.tag(),
        sanitize_line(prefix(&report.event.task_id, 13)),
        state
    );
    if matches!(report.event.kind, TaskEventKind::Opened { .. }) {
        println!("{}", sanitize_line(&report.event.task_id));
    }
    Ok(report.event.task_id)
}

/// `--note TEXT` (`-` = stdin) or `--note-file PATH`, whichever was given.
fn note_arg(note: Option<String>, note_file: Option<PathBuf>) -> Result<Option<String>> {
    let text = match (note, note_file) {
        (Some(n), _) => text_or_stdin(n)?,
        (None, Some(f)) => read_text_arg(&f)?,
        (None, None) => return Ok(None),
    };
    Ok(Some(text).filter(|t| !t.trim().is_empty()))
}

/// Append several events after validating the whole batch ([`task::append_events`]). One
/// confirmation line per event, each with its task's state at the batch's commit boundary.
fn append_batch(events: Vec<TaskEvent>) -> Result<()> {
    let store = TaskStore::default_store();
    let report = task::append_events(&store, events, Vec::new())?;
    for w in report.replay_warnings.iter().chain(&report.warnings) {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    for ev in &report.events {
        let state = report
            .effective_states
            .get(&ev.task_id)
            .map(String::as_str)
            .unwrap_or("?");
        println!(
            "✦ {} {} → {}",
            ev.kind.tag(),
            sanitize_line(prefix(&ev.task_id, 13)),
            state
        );
    }
    Ok(())
}

/// Close with an optional note first. Validate both candidates before writing either, so a
/// refused close leaves the log unchanged and evidence precedes completion when appended.
fn close_with_note(id: &str, from: &str, note: Option<String>, close: TaskEventKind) -> Result<()> {
    let Some(text) = note else {
        return append_and_report(Some(id), from, close);
    };
    let note = task::new_event(
        Some(id),
        from,
        TaskEventKind::Noted {
            text,
            session_ref: None,
        },
    );
    append_batch(vec![note, task::new_event(Some(id), from, close)])
}

fn resolve<'m>(model: &'m cv_core::task::TaskReadModel, prefix: &str) -> Result<&'m str> {
    task::resolve_id(model, prefix).map_err(|e| anyhow::anyhow!(e))
}

/// One `cv task list` row: id, age since last event (G8), state, assignee, title — rendered from
/// the shared [`TaskRow`] shape (built via [`TaskRow::full`], whose `last_ts` anchors the age).
/// All free-text fields are terminal-sanitized (G5) — the log is forever, so every reader strips.
fn task_row(r: &TaskRow, now: DateTime<Utc>, plen: usize, blocked: bool) -> String {
    let assignee = r.assignee.as_deref().unwrap_or("-");
    format!(
        "{:<plen$}  {:>4}  {:16} {:20} {}{}",
        sanitize_line(prefix(&r.id, plen)),
        task::age_short(r.last_ts.unwrap_or(now), now),
        r.effective_state,
        sanitize_line(assignee),
        if blocked { "⊘ " } else { "" },
        sanitize_line(&r.title)
    )
}

/// `--tsv`: full id, state, assignee, repo basename, age, title, blocked_by — one tab between
/// fields, tabs/newlines inside a field replaced, nothing aligned or cut. For `cut`/`awk`/`sort`.
fn task_row_tsv(r: &TaskRow, now: DateTime<Utc>) -> String {
    let cell = |s: &str| sanitize_line(s).replace(['\t', '\n'], " ");
    let repo = r
        .repo
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    [
        cell(&r.id),
        r.effective_state.clone(),
        cell(r.assignee.as_deref().unwrap_or("")),
        cell(&repo),
        task::age_short(r.last_ts.unwrap_or(now), now),
        cell(&r.title),
        cell(&r.blocked_by.join(",")),
    ]
    .join("\t")
}

pub(crate) fn cmd_task(action: TaskCmd) -> Result<()> {
    match action {
        TaskCmd::Open {
            title,
            body,
            body_file,
            repo,
            issue,
            channel,
            assignee,
            tags,
            blocked_by,
            blocks,
            from,
        } => {
            refuse_channel(channel)?;
            let repo = match repo {
                Some(r) => Some(
                    r.canonicalize()
                        .with_context(|| format!("repo {} not found", r.display()))?,
                ),
                None => None,
            };
            let issue = absolutize_issue(issue, repo.as_deref());
            let body = match &body_file {
                Some(p) => read_text_arg(p)?,
                None => text_or_stdin(body)?,
            };
            let tags = tags.as_deref().map(parse_tags).unwrap_or_default();
            // Relations name other tasks: resolve every prefix BEFORE the open, so a typo refuses
            // the whole command instead of leaving a task opened with half its relations.
            let (blocked_by, blocks) = if blocked_by.is_empty() && blocks.is_empty() {
                (blocked_by, blocks)
            } else {
                let outcome = replay_loud()?;
                let res = |ids: Vec<String>| -> Result<Vec<String>> {
                    ids.iter()
                        .map(|p| resolve(&outcome.model, p).map(str::to_string))
                        .collect()
                };
                (res(blocked_by)?, res(blocks)?)
            };
            let from = from_or_cv(from);
            // Validate the task, tags and relations together before writing: a refused tag
            // (a bare `decision`) or relation leaves no half-opened task.
            let opened = task::new_event(
                None,
                &from,
                TaskEventKind::Opened {
                    title,
                    body,
                    repo,
                    issue,
                    channel: None,
                    assignee,
                },
            );
            let id = opened.task_id.clone();
            let mut batch = vec![opened];
            if !tags.is_empty() {
                batch.push(task::new_event(Some(&id), &from, TaskEventKind::Tagged { tags }));
            }
            for b in blocked_by {
                batch.push(task::new_event(Some(&id), &from, TaskEventKind::BlockedBy { task: b }));
            }
            for other in blocks {
                batch.push(task::new_event(
                    Some(&other),
                    &from,
                    TaskEventKind::BlockedBy { task: id.clone() },
                ));
            }
            append_batch(batch)?;
            println!("{id}");
            Ok(())
        }
        TaskCmd::List {
            state,
            assignee,
            repo,
            tag,
            decisions,
            actions,
            all,
            since,
            json,
            tsv,
            wide,
        } => {
            if wide {
                return usage(
                    "`cv task list --wide` was removed in 0.15.0 — `cv task show <id> --brief` has the tags, \
                     blockers and body line per task; `list --tsv` has the blockers for scripts",
                );
            }
            let outcome = replay_loud()?;
            let now = Utc::now();
            let touched_since = scope_window(all, since.as_deref(), now)?;
            let caller = task::default_endpoint();
            let filter = task::TaskFilter {
                state,
                assignee,
                repo,
                include_terminal: all,
                tag,
                touched_since,
                or_involving: caller,
                decisions: if decisions {
                    Some(true)
                } else if actions {
                    Some(false)
                } else {
                    None
                },
            };
            let tasks = task::list(&outcome.model, &filter).map_err(|e| anyhow::anyhow!(e))?;
            // What the window hid: the same filter with no window, minus what it kept.
            let hidden = if touched_since.is_some() {
                let unscoped = task::TaskFilter {
                    touched_since: None,
                    or_involving: None,
                    ..filter.clone()
                };
                task::list(&outcome.model, &unscoped)
                    .map(|v| v.len().saturating_sub(tasks.len()))
                    .unwrap_or(0)
            } else {
                0
            };
            if json {
                // The full projections, unchanged wire shape (`show --json` sibling).
                println!("{}", serde_json::to_string_pretty(&tasks)?);
                return Ok(());
            }
            let now = Utc::now();
            if tsv {
                for t in &tasks {
                    println!("{}", task_row_tsv(&TaskRow::full(t), now));
                }
                return Ok(());
            }
            let plen = prefix_len(&outcome.model);
            for t in &tasks {
                println!(
                    "{}",
                    task_row(&TaskRow::full(t), now, plen, task::is_blocked(&outcome.model, t))
                );
            }
            if tasks.is_empty() {
                println!("(no matching tasks)");
            } else if tasks.iter().any(|t| task::is_blocked(&outcome.model, t)) {
                println!("⊘ = blocked by a task that has not finished");
            }
            if hidden > 0 {
                println!("({hidden} older task(s) hidden — `--all`, or `--since 90d`)");
            }
            Ok(())
        }
        TaskCmd::Show {
            id,
            json,
            events,
            brief,
            status,
            notes_last,
            notes_grep,
        } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            if status {
                let Some(st) = &t.status else {
                    bail!(
                        "no STATUS pinned on {} — `cv task status {} --file F` pins one",
                        sanitize_line(prefix(&id, 13)),
                        sanitize_line(prefix(&id, 13))
                    );
                };
                for line in st.text.lines() {
                    println!("{}", sanitize_line(line));
                }
                return Ok(());
            }
            let grep = match &notes_grep {
                Some(p) => Some(regex::RegexBuilder::new(p).case_insensitive(true).build()?),
                None => None,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(t)?);
            } else {
                print!(
                    "{}",
                    render_show(&outcome.model, t, brief, grep.as_ref(), notes_last, Utc::now())
                );
            }
            if events {
                let history: Vec<_> = outcome.events.iter().filter(|e| e.task_id == id).collect();
                println!("{}", serde_json::to_string_pretty(&history)?);
            }
            Ok(())
        }
        TaskCmd::Claim { id, from, token } => {
            refuse_token(token)?;
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Claimed { assignee: from.clone() })
        }
        TaskCmd::Release { id, from, token } => {
            refuse_token(token)?;
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Released {})
        }
        TaskCmd::Note {
            id,
            text,
            file,
            session_ref,
            from,
        } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let text = match (text, file) {
                (Some(t), _) => text_or_stdin(t)?,
                (None, Some(f)) => read_text_arg(&f)?,
                (None, None) => unreachable!("clap requires text or --file"),
            };
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Noted { text, session_ref })
        }
        TaskCmd::Done {
            id,
            observed,
            check_cmd,
            check_file,
            check_http,
            note,
            note_file,
            from,
        } => {
            if check_cmd.is_some() || check_file.is_some() || check_http.is_some() {
                return usage(
                    "`--check-cmd` / `--check-file` / `--check-http` were removed in 0.15.0 — run the \
                     check yourself and put its result in `--observed`; completion is self-reported",
                );
            }
            let note = note_arg(note, note_file)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            close_with_note(
                &id,
                &from_or_cv(from),
                note,
                TaskEventKind::Done { observed, check: None },
            )
        }
        TaskCmd::Status {
            id,
            text,
            file,
            from,
            token,
        } => {
            refuse_token(token)?;
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let text = match (text, file) {
                (Some(t), _) => text_or_stdin(t)?,
                (None, Some(f)) => read_text_arg(&f)?,
                (None, None) => unreachable!("clap requires text or --file"),
            };
            let text = text.trim_end().to_string();
            append_and_report(Some(&id), &from, TaskEventKind::StatusSet { text })
        }
        TaskCmd::Abandon {
            id,
            reason,
            note,
            note_file,
            from,
        } => {
            let note = note_arg(note, note_file)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            close_with_note(&id, &from_or_cv(from), note, TaskEventKind::Abandoned { reason })
        }
        TaskCmd::Supersede { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Superseded { by_task })
        }
        TaskCmd::Tag { id, tags, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let tags = parse_tags(&tags);
            if tags.is_empty() {
                bail!("no tags given (comma-separated, e.g. `deploy,lean`)");
            }
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Tagged { tags })
        }
        TaskCmd::Block { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::BlockedBy { task: by_task })
        }
        TaskCmd::Sweep { repo, main, json } => cmd_sweep(&repo, main, json),
        TaskCmd::Inbox {
            who,
            json,
            md,
            unread,
            all,
            since,
        } => {
            // Bare `cv task inbox` means "my inbox": the spawner-set CV_ENDPOINT, else "cv".
            let who = from_or_cv(who);
            let outcome = replay_loud()?;
            let now = Utc::now();
            let window = scope_window(all, since.as_deref(), now)?;
            let caller = task::default_endpoint();
            let page = if json || md {
                task::inbox_page(&outcome, &who, caller.as_deref(), window, unread, now)
            } else {
                task::inbox_page_brief(&outcome, &who, caller.as_deref(), window, unread, now)
            };
            if json {
                // The page the text and Markdown render from — the shape `cv serve`'s `/api/inbox`
                // serves, and whose `items` are the MCP and `/api/tasks/inbox` rows.
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else if md {
                print!("{}", task_ops::render_inbox_md(&page));
            } else {
                print!("{}", task_ops::render_inbox_text(&page));
            }
            Ok(())
        }
        TaskCmd::Decide {
            title,
            for_who,
            default,
            options,
            by,
            body,
            body_file,
            repo,
            issue,
            channel,
            tags,
            blocks,
            provisional,
            from,
            token,
        } => {
            refuse_token(token)?;
            refuse_channel(channel)?;
            let repo = match repo {
                Some(r) => Some(
                    r.canonicalize()
                        .with_context(|| format!("repo {} not found", r.display()))?,
                ),
                None => None,
            };
            let body = match &body_file {
                Some(p) => read_text_arg(p)?,
                None => text_or_stdin(body)?,
            };
            let now = Utc::now();
            let deadline = match by {
                Some(b) => Some(task::parse_deadline(&b, now).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            };
            let blocks = if blocks.is_empty() {
                blocks
            } else {
                let outcome = replay_loud()?;
                blocks
                    .iter()
                    .map(|p| resolve(&outcome.model, p).map(str::to_string))
                    .collect::<Result<Vec<_>>>()?
            };
            let issue = absolutize_issue(issue, repo.as_deref());
            let spec = DecisionSpec {
                title,
                body,
                for_who,
                default_choice: default,
                options,
                deadline,
                repo,
                issue,
                tags: tags.as_deref().map(parse_tags).unwrap_or_default(),
                source: None,
                blocks,
                provisional,
            };
            // Validate the pose and (with --provisional) resolution before writing; candidate
            // refusal leaves no posed-but-unresolved decision owed.
            let store = TaskStore::default_store();
            let from = from_or_cv(from);
            let (events, warnings) = task_ops::pose(&store, &from, spec)?;
            for w in &warnings {
                eprintln!("⚠ {}", sanitize_line(w));
            }
            let id = events[0].task_id.clone();
            let for_who = match &events[0].kind {
                TaskEventKind::Opened { assignee, .. } => assignee.clone().unwrap_or_default(),
                _ => String::new(),
            };
            println!(
                "✦ decision {} posed for {}",
                sanitize_line(prefix(&id, 13)),
                sanitize_line(&for_who)
            );
            if events
                .iter()
                .any(|e| matches!(&e.kind, TaskEventKind::Resolved { provisional: true, .. }))
            {
                println!(
                    "✦ resolved {} provisionally on the default (by {}) — {} can --confirm or veto",
                    sanitize_line(prefix(&id, 13)),
                    sanitize_line(&from),
                    sanitize_line(&for_who)
                );
            }
            println!("{id}");
            Ok(())
        }
        TaskCmd::Resolve {
            id,
            choice,
            accept_default,
            confirm,
            provisional,
            note,
            note_file,
            from,
            token,
        } => {
            refuse_token(token)?;
            let note = note_arg(note, note_file)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            // The resolver is the fact that matters; never guessed. The error names the cure
            // with the decision's own assignee filled in.
            let from = match from.or_else(task::default_endpoint) {
                Some(f) => f,
                None => bail!(
                    "a resolution records WHO decided: this decision is for {owner} — if that is you,\n  \
                     cv task resolve {short} {answer} --from {owner}\n  (or `export CV_ENDPOINT={owner}` once)",
                    owner = sanitize_line(t.assignee.as_deref().unwrap_or("<you>")),
                    short = sanitize_line(prefix(&id, 13)),
                    answer = match &choice {
                        Some(c) => format!("--choice {c:?}"),
                        None if confirm => "--confirm".into(),
                        None => "--accept-default".into(),
                    }
                ),
            };
            let store = TaskStore::default_store();
            let answer = task_ops::Answer::from_flags(choice, accept_default, confirm)?;
            let report = task_ops::resolve(&store, &outcome.model, &from, &id, answer, note, provisional)?;
            for w in report.replay_warnings.iter().chain(&report.warnings) {
                eprintln!("⚠ {}", sanitize_line(w));
            }
            if let TaskEventKind::Resolved {
                choice, provisional, ..
            } = &report.event.kind
            {
                println!(
                    "✦ resolved {} → {} (by {}){}",
                    sanitize_line(prefix(&id, 13)),
                    sanitize_line(choice),
                    sanitize_line(&from),
                    if *provisional {
                        " — provisional: the decider can --confirm or veto"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        TaskCmd::Split { .. } => usage(
            "`cv task split` was removed in 0.15.0 — it mined `DECIDE:` notes for questions; pose one directly: \
             `cv task decide \"<title>\" --for <who> --default \"<option>\" [--option …] [--blocks <id>]`",
        ),
        TaskCmd::Events {
            since,
            kind,
            by,
            not_by,
            assignee,
            task: task_id,
            text,
        } => {
            let outcome = replay_loud()?;
            let now = Utc::now();
            let task_id = match task_id {
                Some(p) => Some(resolve(&outcome.model, &p)?.to_string()),
                None => None,
            };
            let f = EventFilter {
                since: Since::parse(since.as_deref(), now)?,
                kinds: kind
                    .as_deref()
                    .map(|k| k.split(',').map(task_ops::kind_tag).collect())
                    .unwrap_or_default(),
                by,
                not_by,
                assignee,
                task: task_id,
            };
            print_events(&outcome, &f, text)
        }
        TaskCmd::Watch { .. } => usage(
            "`cv task watch` was removed in 0.15.0 — it was an alias: \
             `cv task events --assignee <who> --not-by $CV_ENDPOINT [--since <when>] [--text]`",
        ),
        TaskCmd::Serve { .. } => usage(
            "`cv task serve` was removed in 0.15.0 — `cv serve` carries the inbox at /inbox (and at / without \
             --web): `--who` for `--assignee`, `--host`/`--port` for `--bind`, `--open` as before",
        ),
        TaskCmd::Propose { .. } => removed_land_verb("propose"),
        TaskCmd::Reroute { .. } => removed_land_verb("reroute"),
        TaskCmd::Pass { .. } => removed_land_verb("pass"),
        TaskCmd::Refute { .. } => removed_land_verb("refute"),
        TaskCmd::Verify { .. } => removed_land_verb("verify"),
        TaskCmd::Debt { .. } => usage(
            "`cv task debt` was removed in 0.15.0 with the land facet — `cv task list --all` is the ledger; \
             `cv task sweep --repo <path>` is what git can say about it",
        ),
        TaskCmd::Stats { .. } => usage(
            "`cv task stats` was removed in 0.15.0 with the land facet — `cv task events --json` is the raw material",
        ),
    }
}

/// `cv task show <id>` as text: the pinned STATUS first (with `brief`, only it and the title), then
/// the completion label, the fields, relations, the decision facet, and the notes `grep` and
/// `notes_last` keep. Every free-text field came from the durable log — sanitized at render (G5).
fn render_show(
    model: &TaskReadModel,
    t: &cv_core::task::TaskProjection,
    brief: bool,
    grep: Option<&regex::Regex>,
    notes_last: Option<usize>,
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    // Every free-text field below came from the durable log — sanitize at render
    // (G5): titles, notes, endpoints, and branch names are all untrusted.
    out.push_str(&format!(
        "task {}  [{}]  {}\n",
        sanitize_line(&t.task_id),
        task::effective_display(t),
        t.kind()
    ));
    if let Some(st) = &t.status {
        // The pinned STATUS leads: it is what a successor reads first.
        out.push_str(&format!(
            "  STATUS ({}, {}{}):\n",
            sanitize_line(&st.by),
            fmt_local(st.ts, "%Y-%m-%d %H:%M"),
            if st.revisions > 1 {
                format!(", revision {}", st.revisions)
            } else {
                String::new()
            }
        ));
        for line in st.text.lines() {
            out.push_str(&format!("    {}\n", sanitize_line(line)));
        }
        if brief {
            out.push_str(&format!("  title:    {}\n", sanitize_line(&t.title)));
            return out;
        }
    }
    if t.state == cv_core::task::TaskState::Done {
        // A completion is what the agent said, labeled so: cv never claims to have
        // observed what it has not.
        out.push_str(&format!(
            "  done:     self-reported {} ago{}\n",
            task::age_short(t.last_ts, now),
            t.done_observed
                .as_deref()
                .map(|o| format!(" ({})", sanitize_line(o)))
                .unwrap_or_default()
        ));
    }
    out.push_str(&format!("  title:    {}\n", sanitize_line(&t.title)));
    if !t.body.is_empty() {
        if brief {
            out.push_str(&format!("  body:     {}\n", task_ops::first_line(&t.body, 160)));
        } else {
            out.push_str(&format!("  body:     {}\n", sanitize_line(&t.body)));
        }
    }
    if let Some(repo) = &t.repo {
        out.push_str(&format!("  repo:     {}\n", sanitize_line(&repo.to_string_lossy())));
    }
    if let Some(issue) = &t.issue {
        out.push_str(&format!("  issue:    {}\n", sanitize_line(issue)));
    }
    out.push_str(&format!(
        "  assignee: {}\n",
        sanitize_line(t.assignee.as_deref().unwrap_or("-"))
    ));
    if !t.tags.is_empty() {
        out.push_str(&format!("  tags:     {}\n", sanitize_line(&t.tags.join(", "))));
    }
    let plen = prefix_len(model);
    for b in &t.blocked_by {
        match model.tasks.get(b) {
            Some(bt) => out.push_str(&format!(
                "  blocked by: {} [{}] {}\n",
                sanitize_line(prefix(b, plen)),
                task::effective_display(bt),
                sanitize_line(&bt.title)
            )),
            None => out.push_str(&format!(
                "  blocked by: {} [unknown task]\n",
                sanitize_line(prefix(b, plen))
            )),
        }
    }
    if task::is_blocked(model, t) {
        out.push_str("  ⊘ BLOCKED — a blocker above has not finished\n");
    }
    for other in task::blocks(model, &t.task_id) {
        out.push_str(&format!(
            "  blocks:   {} [{}] {}\n",
            sanitize_line(prefix(&other.task_id, plen)),
            task::effective_display(other),
            sanitize_line(&other.title)
        ));
    }
    out.push_str(&format!(
        "  opened:   {} by {}\n",
        fmt_local(t.opened_at, "%Y-%m-%d %H:%M"),
        sanitize_line(&t.opened_by)
    ));
    if let Some(d) = &t.decision {
        let deadline = d
            .deadline
            .map(|dl| format!(" · {}", task::deadline_phrase(dl, now)))
            .unwrap_or_default();
        out.push_str(&format!(
            "  decision: posed {} by {}{deadline}\n",
            fmt_local(d.posed_at, "%Y-%m-%d %H:%M"),
            sanitize_line(&d.posed_by)
        ));
        out.push_str(&format!("    default:  {}\n", sanitize_line(&d.default_choice)));
        for a in d.alternatives() {
            out.push_str(&format!("    option:   {}\n", sanitize_line(a)));
        }
        if let Some(p) = &d.superseded_provisional {
            out.push_str(&format!(
                "    provisionally: {} — by {}, {} (then answered by the decider, below)\n",
                sanitize_line(&p.choice),
                sanitize_line(&p.by),
                fmt_local(p.ts, "%Y-%m-%d %H:%M"),
            ));
        }
        match &d.resolution {
            Some(r) => {
                out.push_str(&format!(
                    "    resolved: {} — by {}, {}{}{}{}\n",
                    sanitize_line(&r.choice),
                    sanitize_line(&r.by),
                    fmt_local(r.ts, "%Y-%m-%d %H:%M"),
                    if r.accepted_default { " (the default)" } else { "" },
                    if r.provisional { " PROVISIONAL" } else { "" },
                    r.note
                        .as_deref()
                        .map(|n| format!(" · {}", sanitize_line(n)))
                        .unwrap_or_default()
                ));
                if r.provisional {
                    out.push_str(&format!(
                        "    veto?:    cv task resolve {} --confirm --from {owner} · or --choice \"<option>\"\n",
                        sanitize_line(prefix(&t.task_id, plen)),
                        owner = sanitize_line(t.assignee.as_deref().unwrap_or("<you>"))
                    ));
                }
            }
            None if !t.state.is_terminal() => out.push_str(&format!(
                "    resolve:  cv task resolve {} --accept-default --from {}\n",
                sanitize_line(prefix(&t.task_id, plen)),
                sanitize_line(t.assignee.as_deref().unwrap_or("<you>"))
            )),
            None => {}
        }
    }
    let children = task::decisions_from_notes(model);
    let mut shown: Vec<&cv_core::task::Note> = t
        .notes
        .iter()
        .rev()
        .filter(|n| grep.is_none_or(|re| re.is_match(&n.text)))
        .take(notes_last.unwrap_or(usize::MAX))
        .collect();
    shown.reverse();
    if !t.notes.is_empty() && (brief || shown.len() != t.notes.len()) {
        out.push_str(&format!(
            "  notes:    {} of {} (newest last){}\n",
            shown.len(),
            t.notes.len(),
            if brief {
                " — one line each; drop --brief for the full text"
            } else {
                ""
            }
        ));
    }
    for note in shown {
        let split = children
            .get(&note.event_id)
            .map(|c| format!("  → split into {}", sanitize_line(prefix(c, plen))))
            .unwrap_or_default();
        let late = if note.post_close { " (after close)" } else { "" };
        if brief {
            out.push_str(&format!(
                "    {} {:<24} {}{late}{split}\n",
                fmt_local(note.ts, "%m-%d %H:%M"),
                truncate(&sanitize_line(&note.by), 24),
                task_ops::first_line(&note.text, 150)
            ));
        } else {
            out.push_str(&format!(
                "  note ({}, {}){late}: {}{split}\n",
                sanitize_line(&note.by),
                fmt_local(note.ts, "%Y-%m-%d %H:%M"),
                sanitize_line(&note.text)
            ));
        }
    }
    out
}

/// `cv task events`: one line per event, JSON by default (the poll surface),
/// `--text` for people. The next cursor (the last event id) goes to stderr so stdout stays pure.
fn print_events(outcome: &cv_core::task::ReplayOutcome, f: &EventFilter, text: bool) -> Result<()> {
    let events = task_ops::select_events(outcome, f)?;
    let plen = prefix_len(&outcome.model);
    for ev in &events {
        if text {
            println!("{}", task_ops::event_text(ev, &outcome.model, plen));
        } else {
            println!("{}", serde_json::to_string(&task_ops::event_json(ev, &outcome.model))?);
        }
    }
    match events.last() {
        Some(last) => eprintln!(
            "# {} event(s) · next: --since {}",
            events.len(),
            sanitize_line(&last.id)
        ),
        None => eprintln!("# no events"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::task::{TaskProjection, TaskState};

    fn proj(title: &str, last_ts: &str) -> TaskProjection {
        TaskProjection {
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            title: title.into(),
            body: String::new(),
            repo: None,
            issue: None,
            state: TaskState::Open,
            assignee: Some("agent:a".into()),
            opened_by: "h".into(),
            opened_at: "2026-07-10T00:00:00Z".parse().unwrap(),
            last_event_id: "e".into(),
            last_ts: last_ts.parse().unwrap(),
            notes: Vec::new(),
            superseded_by: None,
            abandoned_reason: None,
            done_observed: None,
            tags: Vec::new(),
            blocked_by: Vec::new(),
            decision: None,
            status: None,
        }
    }

    /// `show`'s text, pinned whole for a resolved decision whose provisional answer the decider
    /// vetoed, with a note after the close; and `--brief` with a STATUS stops after the title.
    #[test]
    fn show_renders_a_vetoed_decision_and_brief_stops_at_the_status() {
        use cv_core::task::{Decision, Note, PinnedStatus, Resolution};
        let at = |s: &str| -> DateTime<Utc> { s.parse().unwrap() };
        let loc = |s: &str| fmt_local(at(s), "%Y-%m-%d %H:%M");
        let res = |choice: &str, by: &str, ts: &str, provisional: bool| Resolution {
            choice: choice.into(),
            by: by.into(),
            ts: at(ts),
            event_id: "r".into(),
            note: (!provisional).then(|| "my data".into()),
            accepted_default: provisional,
            provisional,
        };
        let mut t = proj("build base: /tank or \x1b[31mNVMe\x1b[0m", "2026-07-16T12:33:00Z");
        t.state = TaskState::Resolved;
        t.assignee = Some("ember".into());
        t.decision = Some(Decision {
            options: vec!["keep /tank".into(), "move to NVMe".into()],
            default_choice: "keep /tank".into(),
            deadline: None,
            source: None,
            posed_at: at("2026-07-16T12:28:00Z"),
            posed_by: "orchestrator:demo".into(),
            resolution: Some(res("move to NVMe", "web:ember", "2026-07-16T12:33:00Z", false)),
            superseded_provisional: Some(res("keep /tank", "orchestrator:demo", "2026-07-16T12:31:00Z", true)),
        });
        t.notes.push(Note {
            event_id: "n".into(),
            by: "orchestrator:demo".into(),
            ts: at("2026-07-16T12:34:00Z"),
            text: "measured".into(),
            session_ref: None,
            post_close: true,
        });
        let mut model = TaskReadModel::default();
        model.tasks.insert(t.task_id.clone(), t.clone());
        let now = at("2026-07-17T00:00:00Z");
        let got = render_show(&model, &t, false, None, None, now);
        let want = format!(
            "task 00000000-0000-7000-8000-000000000001  [resolved]  decision\n\
             \x20 title:    build base: /tank or NVMe\n\
             \x20 assignee: ember\n\
             \x20 opened:   {opened} by h\n\
             \x20 decision: posed {posed} by orchestrator:demo\n\
             \x20   default:  keep /tank\n\
             \x20   option:   move to NVMe\n\
             \x20   provisionally: keep /tank — by orchestrator:demo, {prov} (then answered by the decider, below)\n\
             \x20   resolved: move to NVMe — by web:ember, {res} · my data\n\
             \x20 note (orchestrator:demo, {note}) (after close): measured\n",
            opened = loc("2026-07-10T00:00:00Z"),
            posed = loc("2026-07-16T12:28:00Z"),
            prov = loc("2026-07-16T12:31:00Z"),
            res = loc("2026-07-16T12:33:00Z"),
            note = loc("2026-07-16T12:34:00Z"),
        );
        assert_eq!(got, want);

        t.status = Some(PinnedStatus {
            text: "gen 2: /tank is fast enough".into(),
            by: "orchestrator:demo".into(),
            ts: at("2026-07-16T12:30:00Z"),
            event_id: "s".into(),
            revisions: 2,
        });
        let got = render_show(&model, &t, true, None, None, now);
        assert!(got.ends_with("  title:    build base: /tank or NVMe\n"), "{got}");
        assert!(
            got.contains("STATUS (orchestrator:demo, ")
                && got.contains(", revision 2):\n    gen 2: /tank is fast enough\n"),
            "{got}"
        );
        assert!(!got.contains("decision: posed"), "brief stops at the STATUS:\n{got}");
    }

    #[test]
    fn show_note_tail_applies_grep_before_limit_and_preserves_chronological_order() {
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let mut task = proj("notes", "2026-07-16T00:00:00Z");
        for index in 0..10 {
            task.notes.push(cv_core::task::Note {
                event_id: format!("note-{index}"),
                by: "author".into(),
                ts: now,
                text: format!("{} {index}", if index % 2 == 0 { "keep" } else { "skip" }),
                session_ref: None,
                post_close: false,
            });
        }
        let model = TaskReadModel::default();
        let grep = regex::Regex::new("keep").unwrap();
        let output = render_show(&model, &task, false, Some(&grep), Some(2), now);
        assert!(output.contains("2 of 10"));
        assert!(output.find("keep 6").unwrap() < output.find("keep 8").unwrap());
        assert!(!output.contains("keep 4") && !output.contains("skip 9"));
        let empty = render_show(&model, &task, false, Some(&grep), Some(0), now);
        assert!(empty.contains("0 of 10") && !empty.contains("keep"));
    }

    #[test]
    fn sweep_observes_local_branch_names_without_inventing_namespace_aliases() {
        let world = cv_testkit::World::new("task-sweep-branches");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&world.base)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        git(&["branch", "feature/compiler"]);
        git(&["update-ref", "refs/remotes/origin/feature/remote-only", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let branches = merged_branches(&world.base, "main").unwrap();
        assert!(branches.contains("feature/compiler"));
        assert!(!branches.contains("compiler"));
        assert!(branches.contains("feature/remote-only") && branches.contains("origin/feature/remote-only"));
        assert!(!branches.contains("main") && !branches.contains("origin/main"));
        assert_eq!(
            branch_tokens_in(["feature/compiler_", "_remote-only end"]),
            branch_tokens("feature/compiler_\n_remote-only end")
        );
    }

    #[test]
    fn task_id_path_and_blocker_rendering_strip_controls_without_changing_json() {
        let mut task = proj("title", "2026-07-13T00:00:00Z");
        task.task_id = "\x1b[31mta\nsk\x1b[0m".into();
        task.repo = Some(PathBuf::from("/tmp/repo\x1b[31mcolor\x1b[0m"));
        task.blocked_by = vec!["bad\tblock\x1b[31m".into()];
        let row = TaskRow::full(&task);
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let rendered = task_row(&row, now, row.id.len(), true);
        assert_eq!(rendered.lines().count(), 1);
        assert!(!rendered.contains('\x1b'));
        let tsv = task_row_tsv(&row, now);
        assert_eq!(tsv.lines().count(), 1);
        let cells: Vec<_> = tsv.split('\t').collect();
        assert_eq!(cells.len(), 7);
        assert_eq!(cells[0], "ta sk");
        assert_eq!(cells[6], "bad block");
        let mut model = TaskReadModel::default();
        model.tasks.insert(task.task_id.clone(), task.clone());
        let shown = render_show(&model, &task, false, None, None, now);
        assert!(shown.starts_with("task ta sk  [open]"), "{shown}");
        assert!(shown.contains("/tmp/repocolor") && !shown.contains('\x1b'));
        assert_eq!(
            serde_json::to_value(&row).unwrap()["id"],
            task.task_id,
            "JSON preserves original identifiers"
        );
    }

    #[test]
    fn task_row_shows_age_and_strips_ansi() {
        let t = proj("evil\u{1b}]0;pwn\u{7}title\u{1b}[31m!", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let row = task_row(&TaskRow::full(&t), now, 8, false);
        assert!(row.contains("  3d  "), "age column from last_ts: {row}");
        assert!(row.contains("eviltitle!"), "payload stripped: {row}");
        assert!(!row.contains('\u{1b}'), "no ESC survives: {row}");
    }

    #[test]
    fn blocked_rows_carry_the_marker_and_tsv_is_one_line_per_task() {
        let mut t = proj("tab\tin\ntitle", "2026-07-13T00:00:00Z");
        t.blocked_by = vec!["00000000-0000-7000-8000-000000000009".into()];
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let row = task_row(&TaskRow::full(&t), now, 13, true);
        assert!(row.contains("⊘ tab"), "{row}");
        assert!(
            row.starts_with("00000000-0000  "),
            "prefix is sized by the caller: {row}"
        );
        let tsv = task_row_tsv(&TaskRow::full(&t), now);
        assert_eq!(tsv.lines().count(), 1, "{tsv:?}");
        let cells: Vec<&str> = tsv.split('\t').collect();
        assert_eq!(cells.len(), 7, "{cells:?}");
        assert_eq!(cells[0], t.task_id);
        assert_eq!(cells[5], "tab in title");
        assert_eq!(cells[6], "00000000-0000-7000-8000-000000000009");
    }

    #[test]
    fn tags_parse_trimmed_and_deduplicated() {
        assert_eq!(parse_tags(" a, b ,,a,c "), vec!["a", "b", "c"]);
        assert!(parse_tags(", ,").is_empty());
    }

    #[test]
    fn sweep_helpers_tell_branches_from_words_and_paths_from_handles() {
        assert!(branch_like("sdk-ts-repair"));
        assert!(branch_like("k-ran"));
        assert!(branch_like("feature/x"));
        assert!(branch_like("p3b1"));
        assert!(!branch_like("fix"));
        assert!(!branch_like("final"));
        let toks = branch_tokens("one commit on branch `sdk-ts-repair` (see docs/plan.md).");
        assert!(toks.contains("sdk-ts-repair"), "{toks:?}");
        assert!(toks.contains("docs/plan.md"), "{toks:?}");
        assert_eq!(
            issue_path("claudesplosion/planning/SESSION-STATE.md"),
            Some("claudesplosion/planning/SESSION-STATE.md")
        );
        assert_eq!(issue_path("#42"), None);
        assert_eq!(issue_path("https://github.com/x/y/issues/4"), None);
        assert_eq!(issue_path("memory/x.md"), Some("memory/x.md"));
    }

    #[test]
    fn explicit_from_beats_everything_and_missing_identity_names_the_cure() {
        assert_eq!(require_from(Some("agent:me".into())).unwrap(), "agent:me");
        assert_eq!(from_or_cv(Some("agent:me".into())), "agent:me");
        // The unset-env cases are exercised end-to-end in tests/cli.rs (spawned process with a
        // controlled environment — no process-global set_var races here).
    }
}

// ===================== cv task sweep =====================

/// What `git` in `repo` says has landed: every local branch merged into `main`, plus every
/// remote-tracking branch merged into it with its remote prefix stripped — the names a task's
/// body would mention. `main` itself is excluded.
fn merged_branches(repo: &Path, main: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for extra in [&[][..], &["-r"][..]] {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .arg("branch")
            .args(extra)
            .arg("--format=%(refname:short)")
            .arg("--merged")
            .arg(main);
        let o = cmd
            .output()
            .with_context(|| format!("running git branch --merged in {}", repo.display()))?;
        if !o.status.success() {
            bail!(
                "git branch --merged {main} failed in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let name = line.trim();
            if name.is_empty() || name.ends_with("/HEAD") || name == main {
                continue;
            }
            if extra.is_empty() {
                // Local branch namespaces are part of their names: feature/compiler does not
                // establish that an unrelated branch called compiler was merged.
                out.insert(name.to_string());
            } else {
                let short = name.split_once('/').map(|(_, rest)| rest).unwrap_or(name);
                if short != main {
                    out.insert(name.to_string());
                    out.insert(short.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Does `repo` have a ref named `name`?
fn has_ref(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", name])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A branch name worth matching against prose: it must look like a branch, not a word — a
/// separator or a digit, or eight characters — so a branch called `fix` does not sweep every task
/// whose body says "fix".
fn branch_like(name: &str) -> bool {
    name.contains(['-', '/', '_', '.']) || name.chars().any(|c| c.is_ascii_digit()) || name.len() >= 8
}

/// The branch-shaped tokens of a text (letters, digits and `-_./`), deduplicated.
#[cfg(test)]
fn branch_tokens(text: &str) -> BTreeSet<String> {
    branch_tokens_in([text])
}

fn branch_tokens_in<'a>(texts: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    texts
        .into_iter()
        .flat_map(|text| text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))))
        .map(|t| t.trim_matches(|c| matches!(c, '.' | '/')))
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

/// Does `issue` read as a path (as opposed to `#42` or a URL)? A slash or a file extension and no
/// whitespace.
fn issue_path(issue: &str) -> Option<&str> {
    let i = issue.trim();
    let looks = !i.contains(char::is_whitespace)
        && !i.contains("://")
        && (i.contains('/') || Path::new(i).extension().is_some());
    looks.then_some(i)
}

#[derive(serde::Serialize)]
struct SweepRow<'a> {
    task_id: &'a str,
    title: &'a str,
    state: String,
    reasons: Vec<String>,
}

/// `cv task sweep --repo <path>`: the tasks the world says are probably done. Observed from git
/// and the filesystem; prints candidates and never closes one (a human or the agent that owns
/// the task does that, with `done`/`abandon`, having read why).
fn cmd_sweep(repo: &Path, main: Option<String>, json: bool) -> Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("repo {} not found", repo.display()))?;
    let main = match main {
        Some(m) => m,
        None if has_ref(&repo, "main") => "main".into(),
        None if has_ref(&repo, "master") => "master".into(),
        None => bail!(
            "{} has neither `main` nor `master`; pass --main <branch>",
            repo.display()
        ),
    };
    let merged = merged_branches(&repo, &main)?;
    let outcome = replay_loud()?;
    let mut rows: Vec<SweepRow<'_>> = Vec::new();
    for t in outcome.model.tasks.values() {
        if t.state.is_terminal() || t.repo.as_deref().is_some_and(|r| r != repo) {
            continue;
        }
        let mut reasons = Vec::new();
        if let Some(p) = t.issue.as_deref().and_then(issue_path) {
            // A relative issue path was typed from *somewhere*: the task's repo, the swept repo,
            // the directory above it (`~/dev/<other-repo>/…` is the common spelling), or the
            // current directory. It is missing only when none of them has it.
            let p = Path::new(p);
            let bases: Vec<PathBuf> = if p.is_absolute() {
                vec![PathBuf::new()]
            } else {
                let mut b: Vec<PathBuf> = Vec::new();
                b.extend(t.repo.clone());
                b.push(repo.clone());
                b.extend(repo.parent().map(Path::to_path_buf));
                b.extend(std::env::current_dir().ok());
                b.dedup();
                b
            };
            if !bases.iter().any(|b| b.join(p).exists()) {
                let tried: Vec<String> = bases.iter().map(|b| b.join(p).display().to_string()).collect();
                reasons.push(format!("issue path no longer exists ({})", tried.join(", ")));
            }
        }
        let mentioned = branch_tokens_in([t.title.as_str(), t.body.as_str()]);
        for b in mentioned
            .iter()
            .filter(|b| branch_like(b) && merged.contains(b.as_str()))
        {
            reasons.push(format!("names branch `{b}`, merged into {main}"));
        }
        if !reasons.is_empty() {
            rows.push(SweepRow {
                task_id: &t.task_id,
                title: &t.title,
                state: task::effective_display(t),
                reasons,
            });
        }
    }
    rows.sort_by(|a, b| a.task_id.cmp(b.task_id));
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "nothing to sweep: no open task names a branch merged into {main} or a missing issue path ({} merged branch(es) observed)",
            merged.len()
        );
        return Ok(());
    }
    let plen = prefix_len(&outcome.model);
    println!(
        "# probably done — {} candidate(s) in {} (observed: {} branch(es) merged into {main}); nothing was closed\n",
        rows.len(),
        sanitize_line(&repo.to_string_lossy()),
        merged.len()
    );
    for r in &rows {
        println!(
            "{:<plen$}  {:16} {}",
            sanitize_line(prefix(r.task_id, plen)),
            r.state,
            sanitize_line(r.title)
        );
        for reason in &r.reasons {
            println!("{:plen$}  ↳ {}", "", sanitize_line(reason));
        }
    }
    println!("\nclose what is really done: `cv task done <id> --observed <evidence>` · drop the rest: `cv task abandon <id> --reason …`");
    Ok(())
}
