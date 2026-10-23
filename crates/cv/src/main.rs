//! `cv` — the clustervision CLI.
//!
//! This file holds only the clap surface (`Cli`/`Cmd`), the command groups, and the dispatch into
//! the per-command modules under `cmd/`; shared helpers live in `util.rs`.
//!
//! Interface v2 (0.11.0, `docs/INTERFACE-V2.md`): one verb per act, the same flag means the same
//! thing everywhere, snake_case JSON, no hidden commands. A name removed in the current release is
//! a hidden stub that errors with a pointer and exits 2 — never an alias that works — and the
//! stub itself goes at the next release (the 0.11 tombstones left at 0.15).

#[cfg(all(test, feature = "probe-heap"))]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;

mod cmd;
mod http;
mod llm;
mod util;

/// A hermetic world whose environment this thread's library calls read (see `cv_core::env`).
#[cfg(test)]
fn test_world(tag: &str) -> cv_testkit::InProcess<cv_core::env::Isolated> {
    cv_testkit::World::in_process(tag, cv_core::env::Env::enter_isolated)
}

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use cmd::hooks::{HookName, HooksCmd};
use cmd::task::TaskCmd;
use cmd::{
    archive, blame, browse, cat, config, doctor, events, formats, hooks, live, pack, port, recipes, reshape, schema,
    search, serve, session_reports, share, task, tools, view, workflow,
};
use std::path::PathBuf;
use util::{usage, UsageError, WindowArgs};

/// Version with the embedded build commit (set by build.rs; "unknown" outside git), so the
/// binary in your PATH is checkable against source.
const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CV_BUILD_SHA"), ")");

/// The clap tree is well-formed (debug asserts: conflicting ids, bad defaults, …).
/// Building the clap tree is deep: `Cli::command()` for every command with its args is a
/// large stack frame in a debug build, and the default 2 MB test-thread stack overflows on
/// it (the binary's main thread has 8 MB and is fine). Run the tree-building tests on a
/// thread sized like main.
#[cfg(test)]
pub(crate) fn on_main_sized_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test thread panicked");
}

/// The command groups `cv --help` prints and `cv schema --commands` reports. clap renders
/// subcommands as one flat list (its `next_help_heading` tags *arguments*), so the grouping lives
/// here; `groups_cover_every_visible_command` keeps this table and the `Cmd` enum in lockstep.
pub(crate) const GROUPS: &[(&str, &[&str])] = &[
    (
        "Read",
        &[
            "ls",
            "show",
            "cat",
            "search",
            "events",
            "touched",
            "tools",
            "workflow",
            "lanes",
            "deferrals",
            "stats",
            "blame",
            "doctor",
        ],
    ),
    (
        "Reshape",
        &["prune", "rewind", "distill", "adopt", "splice", "port", "resume"],
    ),
    ("Export", &["export", "dataset", "pack"]),
    (
        "Fleet & live",
        &["task", "scry", "watch", "archive", "serve", "mcp", "share"],
    ),
    (
        "System",
        &["index", "config", "schema", "formats", "recipes", "hooks", "hook"],
    ),
];

/// The group a visible command belongs to (`None` for hidden stubs and unknown names).
pub(crate) fn group_of(name: &str) -> Option<&'static str> {
    GROUPS.iter().find(|(_, cmds)| cmds.contains(&name)).map(|(g, _)| *g)
}

/// `--skills` values. `--skills-policy` and `drop-all` are earlier spellings callers may
/// still be written against; each parses to its canonical name (`PossibleValuesParser`
/// matches an alias but returns the spelling given, so the mapping lives here, not in
/// the alias).
fn skills_values() -> impl clap::builder::TypedValueParser<Value = String> {
    use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
    const VALUES: [(&str, Option<&str>); 3] = [("keep", None), ("drop", Some("drop-all")), ("drop-stale", None)];
    PossibleValuesParser::new(VALUES.map(|(name, old)| PossibleValue::new(name).aliases(old))).map(|given: String| {
        VALUES
            .iter()
            .find(|(_, old)| *old == Some(given.as_str()))
            .map_or(given.clone(), |(name, _)| name.to_string())
    })
}

const RECIPES_FOOTER: &str = "run `cv recipes` for the agent quickstart";

/// The grouped command listing that replaces clap's flat `Commands:` block — every visible
/// subcommand under its group, each with clap's own `about` line, then the recipes pointer.
fn grouped_help(cmd: &clap::Command) -> String {
    let width = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().len())
        .max()
        .unwrap_or(8);
    let mut out = String::from("Commands:\n");
    for (group, names) in GROUPS {
        out.push_str(&format!("  {group}\n"));
        for name in names.iter() {
            let Some(sub) = cmd.get_subcommands().find(|s| s.get_name() == *name) else {
                continue;
            };
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            out.push_str(&format!("    {name:<width$}  {about}\n"));
        }
    }
    out.push('\n');
    out.push_str(RECIPES_FOOTER);
    out
}

#[derive(Parser)]
#[command(
    name = "cv",
    version = BUILD_VERSION,
    about = "clustervision — search, read, and port AI agent sessions across harnesses",
    help_template = "{before-help}{about-with-newline}\n{usage-heading} {usage}\n\nOptions:\n{options}\n{after-help}"
)]
struct Cli {
    /// Leave cv's stores untouched (also CV_FROZEN=1). Normally full reads record counts and
    /// affordances, and events refreshes stale sessions for later reads. Frozen reads record
    /// nothing: events streams stale sessions, ls keeps unknown counts as a dash, and scan/usage
    /// caches and skipped sources are not saved. Commands that write stores or sessions exit 2.
    #[arg(long, global = true)]
    frozen: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List discovered sessions across all harnesses.
    Ls {
        /// Only this harness (claude, codex, grok, opencode, gemini, cursor, kimi, kimi-code,
        /// qwen, cline, roo, continue, lmstudio, hermes, goose, zed, openclaw, …).
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Query calculus to filter rows, e.g. `model:fable`, `harness:codex msgs>=50`,
        /// `after:2026-06-01 widget` (implicit AND; see `cv schema`). A `model:` term forces a
        /// parse of each candidate (the catalog doesn't store the model), so pair it with cheap
        /// terms like `harness:`/`cwd:` to keep it fast.
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Max rows to show.
        #[arg(long, default_value_t = 40)]
        limit: usize,
        /// Sort key: updated (default), created, or messages.
        #[arg(long = "sort-by", default_value = "updated", value_parser = ["updated", "created", "messages"])]
        sort_by: String,
        /// Print the shown rows oldest-first (ascending): the same newest `--limit` rows, read as
        /// a feed. `cv ls --sort-by updated --asc` is what `cv timeline` used to print.
        #[arg(long)]
        asc: bool,
        /// Force a full fleet re-discovery instead of trusting the catalog's staleness probe.
        #[arg(long)]
        fresh: bool,
        /// Emit the rows as one JSON array of session rows (`id, harness, path, cwd, title,
        /// created_at, updated_at, message_count, size_bytes`) instead of the table — the same rows
        /// the table would show (all filters, sort, and --limit respected). Nothing else goes to
        /// stdout, so the output pipes cleanly.
        #[arg(long)]
        json: bool,
        /// (--json only) Enrich each row with transcript-derived `git` (branch/commit/remote, as
        /// `cv show --json` emits it) and `display_title` (the title with a first-real-user-text
        /// fallback). Costs one lazy transcript parse per emitted row — O(--limit), not the whole
        /// fleet — so plain `ls --json` stays catalog-cheap; opt in when you need these fields. A
        /// row whose transcript cannot be read carries `enrich_error` instead.
        #[arg(long)]
        enrich: bool,
    },
    /// Print a single session (by `harness:id` or a unique id prefix).
    Show {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Emit the raw unified IR as JSON instead of a rendered transcript.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        window: WindowArgs,
        /// Instead of the transcript, list the sub-agent forest this session spawned — each
        /// direct/`Workflow` sub-agent with its type, journaled outcome (workflow agents), and
        /// final return value. The second dimension of a Claude session.
        #[arg(long)]
        subagents: bool,
        /// Render one specific sub-agent's transcript by its `agent-…` id (or id-prefix), resolved
        /// relative to this parent session. Sub-agents aren't in the main pool, so they're read
        /// through their parent.
        #[arg(long)]
        agent: Option<String>,
        /// Read the pre-compaction span: the messages before a compaction boundary (the context a
        /// continued agent lost). Defaults to the FIRST boundary; pass `--pre-compaction <N>` for
        /// the Nth (1-based). It sets the window for you, so it excludes the window flags.
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "1",
              conflicts_with_all = ["first", "last", "range", "around"])]
        pre_compaction: Option<usize>,
        /// Instead of the transcript, the message threading — a DAG when messages carry
        /// `parent_id`, else a numbered list — then the sub-agent forest, over the whole session.
        /// With `--json`, the IR itself: every message carries the same `parent_id`.
        #[arg(long, conflicts_with_all = ["kind", "subagents", "agent", "pre_compaction"])]
        tree: bool,
        /// Only messages of these kinds, comma-separated, as a compact view. `prompt,answer`: what
        /// the person said — every typed prompt and `AskUserQuestion` answer with its index and
        /// timestamp, printed whole (`--json`: `index, timestamp, kind, text` rows; a window narrows
        /// it). `compaction_boundary,compaction_summary`: every compaction seam — trigger,
        /// pre-compaction size, the summary that seeded the next window (`--json`: one row per
        /// boundary with its `pre_compaction_span`). An affordance — `plan`, `goal`, `schedule`,
        /// `background_job`, `subagent_spawn`, `subagent_return`, `skill`, `hook`, `question`,
        /// `approval`, `checkpoint`, `branch`, `fork`, `model_change`, or `affordances` for all —
        /// lists those rows across harnesses, side stores included (`--json`: `kind, action,
        /// index, timestamp, tool, tool_use_id, summary, items, source` rows).
        #[arg(long, value_name = "KIND,…", conflicts_with_all = ["subagents", "agent"])]
        kind: Option<String>,
        /// With the compaction kinds: print each summary in full, not just its head.
        #[arg(long, requires = "kind")]
        summaries: bool,
    },
    /// Print one tool call's full output, wherever it lives: inline in the transcript, in a
    /// `prune` sidecar, or in a persisted-output file. `--input` prints the call's arguments instead.
    Cat {
        /// The session (`harness:id` or a unique id prefix).
        session: String,
        /// The tool call's id (`toolu_…`, `call_…`, …) — as `cv show`/`cv tools --timeline` print it.
        tool_use_id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Print the tool call's input (its arguments, as JSON) instead of its output.
        #[arg(long)]
        input: bool,
    },
    /// Full-text search across all session content.
    Search {
        query: String,
        #[arg(long)]
        harness: Option<String>,
        /// Hits to show. Every match is still counted: the footer says `N of M matches`.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Use semantic (embedding) search instead of full-text. Requires `cv index --semantic`
        /// to have been run; downloads a small embedding model on first use.
        #[arg(long)]
        semantic: bool,
        /// Scan the transcripts live instead of reading the index: slow, matches only each
        /// session's head, and is never taken unasked. Without it a missing or unusable index is an
        /// error that names its cause.
        #[arg(long, conflicts_with = "semantic")]
        scan: bool,
        /// Which text to match: `exchange` (default: what the person asked, what the model
        /// replied and ran, what came back), `context` (what surrounds it: injected instruction
        /// files, system prompts, skill bodies, hook output, notices, compaction summaries), or
        /// `all`. Context repeats across sessions, so it is matched only when asked for.
        #[arg(long, value_name = "exchange|context|all", conflicts_with_all = ["semantic", "scan"])]
        kind: Option<String>,
        /// The index answers plus a live read of the sessions it has not seen as they are now
        /// (grown, rewritten, new since `cv index`), from where the index stopped: all of them by
        /// default; with this, at most BYTES of source, newest sessions first, and the footer
        /// names what the budget left out. 0 (or global --frozen) answers from the index alone.
        #[arg(long, value_name = "BYTES", conflicts_with_all = ["semantic", "scan"])]
        live_bytes: Option<u64>,
        /// Emit `{"hits": [...], "total": M}` instead of the table — the same hits in the same
        /// order (--harness/--limit/--semantic respected), each a session row (`id, harness, path,
        /// cwd, title, created_at, updated_at, message_count, size_bytes`) plus `score`, `snippet`,
        /// and the sub-agent provenance `agent_id`/`parent_id`/`workflow`; `total` counts every
        /// session that matched. Nothing else goes to stdout.
        #[arg(long)]
        json: bool,
    },
    /// List what a session DID: its extracted events (file edits/reads, commands, errors).
    ///
    /// Events are ingested during `cv index`; a session that isn't cataloged yet (or whose file
    /// changed since) is ingested on the spot — one streamed pass, large content stays on disk.
    /// This read command writes derived events and counts to cv's catalog; it never changes
    /// harness transcripts. Use global --frozen (or CV_FROZEN=1) to stream stale events without
    /// publishing them or updating other cv stores; see --frozen in the global options.
    Events {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Only this kind: file_edit, file_read, command, tool, or error.
        #[arg(long)]
        kind: Option<String>,
        /// Also extract events from every sub-agent this session spawned (the forest), attributed
        /// to each agent — so a session's *full* activity (incl. what its Task/Workflow agents did)
        /// is visible, not just the orchestrator's own tool calls.
        #[arg(long)]
        subagents: bool,
        /// Emit the events as one JSON array (snake_case keys) instead of the table. Every
        /// target and detail is whole: the full command, the full error text.
        #[arg(long)]
        json: bool,
        /// Print each table cell whole (newlines as spaces). Without it the target column is
        /// shortened to 90 characters and the detail line to 100, a cut marked with `…`.
        #[arg(long, conflicts_with = "json")]
        wide: bool,
    },
    /// Sessions that touched a file — every session with a file_edit/file_read event on its path.
    ///
    /// The path is matched absolutely and by suffix, so `cv touched src/ir.rs` finds sessions
    /// that edited `/any/repo/src/ir.rs`. Run `cv index` first to (re)ingest events.
    Touched {
        path: String,
        /// Only sessions that EDITED the file (drop read-only appearances).
        #[arg(long)]
        edits_only: bool,
        /// Emit the rows as one JSON array (snake_case keys) instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Cross-agent tool analytics: per-agent histograms, which-agent-used-what, aggregate usage,
    /// and a tool-call timeline — across the orchestrator and its whole sub-agent forest.
    Tools {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// One agent's histogram ("which tools did agent X use"): its full id, or a unique prefix
        /// (several matches exit 2). `<orchestrator>` for the parent. With `--timeline` it narrows
        /// the timeline instead.
        #[arg(long)]
        agent: Option<String>,
        /// Which agents used this tool, across the forest (ranked by count). Not with `--agent`
        /// unless `--timeline` narrows by both.
        #[arg(long)]
        tool: Option<String>,
        /// Restrict the analytics to one workflow run's agents.
        #[arg(long)]
        workflow: Option<String>,
        /// Per-agent breakdown (one row per agent) instead of the aggregate histogram.
        #[arg(long, conflicts_with_all = ["agent", "tool", "timeline"])]
        across: bool,
        /// The time-ordered tool-call timeline (optionally narrowed with `--agent`).
        #[arg(long)]
        timeline: bool,
        #[arg(long)]
        json: bool,
    },
    /// A `Workflow`-tool run, first-class: its phase tree, the agents under each phase with their
    /// outcomes, run totals, and the driving script. Without `<run_id>`, lists the session's runs.
    Workflow {
        /// The session that launched the workflow — or a workflow NAME, resolved fleet-wide.
        id: String,
        /// The workflow run id (`wf_…`, or a unique prefix) or the workflow's name. Omit to list
        /// all runs.
        run_id: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// Emit the structured workflow (phases → agents → outcomes) as JSON.
        #[arg(long)]
        json: bool,
        /// Also print the driving workflow script (the JS that fanned out the agents).
        #[arg(long)]
        script: bool,
        /// Print each agent's FULL journaled return value (the state file keeps only a ~400-char
        /// preview; the complete results live in the run's journal).
        #[arg(long)]
        results: bool,
        /// Follow a LIVE run: stream agent state transitions as they land in the state file, then
        /// render the full run when it reaches a terminal status.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Follow for this long (30s, 5m, 2h), then render the run as it stands; implies --follow.
        #[arg(id = "for", long = "for", value_name = "DURATION")]
        for_: Option<String>,
        /// Salvage a dead or interrupted run: emit a ready-to-paste standalone `Agent` prompt per
        /// lane — the FULL original task plus the work that lane already landed (files written,
        /// commands run, its own last note) — so lanes come back resumed rather than restarted.
        /// Prints prompts, so it excludes `--json`, `--script`, `--results` and `--follow`/`--for`.
        #[arg(long, conflicts_with_all = ["json", "script", "results", "follow", "for"])]
        revive: bool,
        /// With `--revive`, include lanes that completed successfully too (default: only the ones
        /// that did not finish).
        #[arg(long, requires = "revive")]
        revive_all: bool,
    },
    /// The sub-agent forest as a status table: one row per lane with its model, start, duration,
    /// tokens, tool calls, status and last line — and the strand class (`--stranded`): lanes that
    /// stopped saying they are waiting for something that will never wake them. `--recent N`
    /// reads the forests of a project's N most recent sessions instead of one session's;
    /// with `--unfinished` that is the rescue list for `cv adopt`.
    Lanes {
        /// The session (`harness:id` or a unique prefix). Omit it with `--recent`.
        #[arg(required_unless_present = "recent", conflicts_with = "recent")]
        id: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// Instead of one session: the N most recently active sessions (of a harness whose hooks
        /// record lanes: Claude Code, Codex) whose cwd is `--cwd` (default: the current
        /// directory), each row tagged with its session.
        #[arg(long, value_name = "N")]
        recent: Option<usize>,
        /// With --recent: the project directory the sessions ran in.
        #[arg(long, value_name = "DIR", requires = "recent")]
        cwd: Option<PathBuf>,
        /// Only lanes still running (no stop recorded).
        #[arg(long, group = "lane_filter")]
        running: bool,
        /// Only lanes that completed (harness `completed`, or a journaled `done`).
        #[arg(long, group = "lane_filter")]
        done: bool,
        /// Only stranded lanes: stopped, with a final text that says it is waiting (`Waiting on
        /// notifications`, `I'll continue when the monitor fires`, …). Each row carries the
        /// resume hint.
        #[arg(long, group = "lane_filter")]
        stranded: bool,
        /// Only lanes not finished for real: running, stopped, killed, failed or stranded.
        #[arg(long, group = "lane_filter")]
        unfinished: bool,
        /// Only lanes active in this window: started or last turned within `2h`, `30m`, `1d`, …
        #[arg(long, value_name = "DUR")]
        since: Option<String>,
        /// Join each lane to the task store: the tasks its endpoint (`CV_ENDPOINT=lane:…` from its
        /// own tool calls, else its description's leading token) holds, with the last note.
        #[arg(long)]
        tasks: bool,
        /// Emit the lanes as one JSON array (snake_case; `cv schema` has the shape).
        #[arg(long)]
        json: bool,
    },
    /// A linter for promises: every place the assistant deferred something ("later lane",
    /// "follow-up", "not tonight", "queued", "your call", "when X lands", …) with its message
    /// index and context. The phrase table is `--phrases-file`, else `<config>/deferrals.txt`
    /// if present, else a built-in default. `--open-tasks` cross-references the task store and
    /// marks each MATCHED or UNMATCHED; any UNMATCHED exits 1, so a closeout can gate on it.
    Deferrals {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Only messages from this message index on (e.g. the post-compaction region).
        #[arg(long, value_name = "MSG")]
        at: Option<usize>,
        // Renamed to `--at` in 0.18.0 (`--since` is a duration on `lanes`) — hidden so the old flag errors with a pointer.
        #[arg(long = "since", hide = true)]
        since_removed: Option<String>,
        /// Cross-reference every task in the store (terminal ones included): a deferral MATCHES a
        /// task when they share three or more significant words.
        #[arg(long)]
        open_tasks: bool,
        /// The phrase table: one `label = regex` per line (`#` comments; regexes are
        /// case-insensitive). Default: `~/.config/clustervision/deferrals.txt` if it exists, else
        /// the built-in list — your project's vocabulary lives in the file, not in cv's source.
        #[arg(long, value_name = "PATH")]
        phrases_file: Option<PathBuf>,
        /// Emit the deferrals as one JSON array (`index, timestamp, phrase, context, matched?`).
        #[arg(long)]
        json: bool,
    },
    /// Fleet analytics over all discovered sessions.
    Stats {
        /// Restrict the analytics to sessions matching this query (see `cv schema`).
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Emit the analytics as one JSON object (snake_case keys) instead of the report.
        #[arg(long)]
        json: bool,
        /// Also total token usage per harness and model. Parses every matched transcript and its
        /// sub-agents (slower than the catalog-only default); `--json` adds a `tokens` object.
        #[arg(long)]
        tokens: bool,
    },
    /// Code provenance: which agent session wrote this code, and what was it thinking?
    ///
    /// Correlates the file's git history with the event catalog's file_edit events — an agent
    /// edit shortly before a commit is strong evidence that session authored it. A session whose
    /// own tool output shows the commit being made ranks above all of those as `exact: commit
    /// created here`, with a `cv rewind <id> --at <sha>` hint to resume that agent as of the commit.
    /// Each matched commit gets its best sessions plus a `cv show --range` hint into the
    /// conversation around the edit. Run `cv index` first to ingest events.
    Blame {
        file: String,
        /// Only these lines: `<line>` or `<line>,<endline>` (via `git blame -L`).
        #[arg(short = 'L', value_name = "LINE[,ENDLINE]")]
        lines: Option<String>,
        /// Also print the conversation window around the single best match (the commit itself
        /// for an exact match, else the matched edit).
        #[arg(long)]
        show: bool,
    },
    /// Diagnose why a session's context window keeps filling and compacting: attribute context
    /// pressure by source (tool results split MCP/builtin, thinking, messages), size the fixed
    /// system+tools overhead from token usage, and pair it with compaction frequency. With no
    /// <id>, analyzes the most recent session(s) for the current directory.
    Doctor {
        /// Session id (prefix ok). Omit to analyze recent sessions in the current project.
        id: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// With no <id>: how many recent sessions (for this cwd) to aggregate.
        #[arg(long, default_value_t = 1)]
        recent: usize,
        #[arg(long)]
        json: bool,
    },

    /// Custom, lossless compaction of a Claude session into a NEW resumable session: snip bulky old
    /// tool payloads (large reads/logs/screenshots) into a sidecar, leaving a small `[PRUNED]` marker
    /// — your prompts and flow stay verbatim, the recent turns stay sharp. Resume with
    /// `claude --resume <new-id>`; fetch a stashed original back with `cv cat <new-id> <tool_use_id>`.
    Prune {
        /// Source session id, sub-agent's agent-<id>, or Claude transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Snip a tool payload only if it's larger than this many bytes.
        #[arg(long, default_value_t = 2048)]
        min_size: usize,
        /// Keep the last N conversational turns' payloads verbatim (recent context stays sharp).
        #[arg(long, default_value_t = 25)]
        keep_last: usize,
        /// New session id (default: a fresh UUID). An id whose transcript or sidecar already
        /// exists is refused (exit 2) unless --force.
        #[arg(long)]
        to: Option<String>,
        /// Replace an existing --to session (transcript and sidecar) by atomic rename.
        #[arg(long, requires = "to")]
        force: bool,
        /// Hard-drop payloads (no sidecar, irreversible) instead of stashing them for retrieval.
        #[arg(long)]
        drop: bool,
        /// Also omit the OLDEST assistant reasoning (thinking blocks) — recent thinking (within
        /// --keep-last) stays verbatim. Omitted blocks leave no marker; `cv cat <new-id>
        /// <record-uuid>#think<i>` returns each from the sidecar. Lossless.
        ///
        /// Renamed from `--thinking` in 0.11.0: that spelling now names the emit-side
        /// `--thinking <native|text|drop>`, and one word may not mean two things.
        #[arg(long = "drop-thinking")]
        drop_thinking: bool,
        /// Also copy the session's subagents/workflows dir under the new id (off by default; can be
        /// hundreds of MB for big sessions). Resume doesn't need it — only cv's forest features do.
        #[arg(long)]
        copy_resources: bool,
        /// Preserve the original `usage` records byte-for-byte (opt out of revive). By default,
        /// prune corrects the recorded context size so a maxed-out session will resume: Claude
        /// Code's resume gate reads the last turn's stored `usage` (input+cache) as the current
        /// size *before* it re-sends anything, so a session at the wall refuses to resume even
        /// after pruning shrinks it. Revive rewrites that stale number to the honest post-prune
        /// figure — a no-op when the recorded size is already honest. (Source untouched.)
        #[arg(long)]
        no_revive: bool,
        /// Sliding window: keep only the NEWEST turns totalling ≤ this many REAL tokens, dropping
        /// older turns (lossy — the source keeps the full history). Sized from Claude's OWN recorded
        /// `usage` counts (no byte/tokenizer estimation), so the budget lands true; the resumed
        /// session loads ≈ this budget + ~30k system overhead. Re-roots the kept tail into a
        /// standalone resumable session.
        #[arg(long, value_name = "TOKENS")]
        window: Option<u64>,
        /// Keep only this turn-index window, `A..B` (0-based, end-exclusive; `A..` through the
        /// last), dropping everything outside it. Like --window but by index.
        #[arg(long, value_name = "A..B")]
        keep: Option<String>,
        /// Snip message TEXT (user prompts + assistant replies) whose density of caller-supplied
        /// TERMS is high, into the sidecar with a `[PRUNED …]` marker. Motivation: a safeguard
        /// classifier scores the WHOLE loaded context, so term-dense PROSE in the history can silently
        /// downgrade a resumed seat's model tier — and tool/`--thinking` snipping leaves prose verbatim.
        /// Lossless (retrievable). A message is snipped iff it holds ≥2 distinct terms. cv ships NO
        /// built-in term list (that would trip the very classifier this dodges, and can't adapt);
        /// supply the terms with --declassify-tokens / --declassify-tokens-file, else this is a no-op.
        #[arg(long)]
        declassify: bool,
        /// Comma-separated terms for --declassify (lowercase; case-insensitive substring match).
        #[arg(long, value_name = "T1,T2,…")]
        declassify_tokens: Option<String>,
        /// File of --declassify terms, one per line (`#` comments + blank lines ignored). The external,
        /// version-controllable home for your term list — e.g. tuned from a classifier-trip corpus.
        #[arg(long, value_name = "PATH")]
        declassify_tokens_file: Option<PathBuf>,
        /// Restored Claude skills: keep all, drop all, or drop entries whose files changed or disappeared.
        #[arg(long = "skills", alias = "skills-policy", value_parser = skills_values(), default_value = "keep")]
        skills: String,
        /// Report what would be pruned without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also emit the prune report as ONE JSON object on stdout (snake_case keys, FULL ids).
        /// The human report stays on stderr, so stdout carries only the JSON. Dry-run honest:
        /// nothing was written, so `new_path`/`sidecar_path` are null and `new_id` is null unless
        /// `--to` pinned it (see `note`).
        #[arg(long)]
        json: bool,
    },
    /// Rewind a Claude session to a past moment: a NEW resumable session holding the source's
    /// records up to a message or the tool result that made a commit — the agent as it was when it
    /// landed that code, ready for `claude --resume <new-id> --fork-session`. Starts at the last
    /// compaction before the cut (the context the agent actually had); a sub-agent transcript comes
    /// out as a standalone top-level session. Writes `<new-id>.rewind.json` provenance alongside.
    Rewind {
        /// Source session: an id (prefix, `harness:id`), a sub-agent's `agent-<id>`, or a transcript path.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// The cut, inclusive: a message index (as `cv show --range` counts) or a commit sha (7+ hex
        /// digits) — resolved to the `git commit`/`git push` tool result whose output names it, or an
        /// error if none does. An all-digit value is an index. Omit for the last message.
        #[arg(long, value_name = "MSG_IDX|SHA")]
        at: Option<String>,
        /// Rewind this sub-agent of <id> instead (its `agent-…` id, bare agentId, or a prefix), as
        /// `cv show --agent` resolves it.
        #[arg(long)]
        agent: Option<String>,
        /// Write under this directory instead of the source's project dir (where `claude --resume`
        /// looks; a sub-agent's is its parent's).
        #[arg(long)]
        out: Option<PathBuf>,
        /// New session id (default: a fresh UUID).
        #[arg(long)]
        to: Option<String>,
        // The default start, spelled out; removed in 0.18.0 — hidden so the old flag errors with a pointer.
        #[arg(long, hide = true)]
        from_compaction: bool,
        /// Start at the source's first record instead of the last compaction boundary before the
        /// cut (the default: what a resume loads).
        #[arg(long)]
        full: bool,
        /// Reasoning policy, two of the four `--thinking` values (a rewind writes Claude): `native`
        /// keeps the source's reasoning and identifiers; `portable` demotes plaintext thinking to
        /// text, removes signature-only reasoning and repairs tool IDs. `text` and `drop` are
        /// refused here. Default: native when every model the window records is `claude-*`, else portable.
        #[arg(long, value_parser = ["native", "portable"])]
        thinking: Option<String>,
        /// Restored Claude skills: keep all, drop all, or drop entries whose files changed or disappeared.
        #[arg(long = "skills", alias = "skills-policy", value_parser = skills_values(), default_value = "keep")]
        skills: String,
        /// Report the window that would be written without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also emit the result as ONE JSON object on stdout (snake_case, FULL ids, the `resume`
        /// lines); the human report stays on stderr. Dry-run honest like `prune --json`.
        #[arg(long)]
        json: bool,
    },
    /// Distill a long session (typically a sub-agent lane) into a compact context: the brief and
    /// every message to the agent verbatim, its own words (text, messages sent, notes written)
    /// verbatim, an index of commits/branches/hosts/paths/verdicts/errors, a one-line-per-call
    /// ledger, and its last N tool calls as real turns. Deterministic (no model calls). Elided
    /// outputs go to a sidecar `cv cat` reads. Prints the markdown pack by default; `--session`
    /// emits a resumable session (`claude -p --resume <id>`); `--agent-of <root>` emits a new
    /// sub-agent of that root, which the root resumes with SendMessage to the printed agent id.
    Distill {
        /// Session id, agent id (`agent-…`), `harness:id`, or a path to a transcript file.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Keep the last N tool calls (and the turns around them) verbatim.
        #[arg(long, default_value_t = 12)]
        keep_last: usize,
        /// Byte cap for one tool result inside the verbatim tail (head+tail kept, rest elided).
        #[arg(long, default_value_t = 6000)]
        tail_result_max: usize,
        /// Byte cap for one note the agent wrote, a sub-agent return, or a spawned prompt.
        #[arg(long, default_value_t = 6000)]
        note_max: usize,
        /// Keep the agent's thinking blocks inside the verbatim tail (dropped by default).
        #[arg(long)]
        keep_tail_thinking: bool,
        /// Omit the one-line-per-tool-call ledger.
        #[arg(long)]
        no_ledger: bool,
        /// Distill the session as it stood at message N (exclusive): a distilled fork.
        #[arg(long, value_name = "N")]
        upto: Option<usize>,
        /// Inject another session's findings (its own words and facts) into this pack. Repeatable.
        #[arg(long = "with", value_name = "SESSION")]
        with: Vec<String>,
        /// Write the markdown pack to this file (instead of stdout).
        #[arg(long, value_name = "FILE")]
        pack: Option<PathBuf>,
        /// Replace an existing --pack file by atomic rename (otherwise a taken name exits 2).
        #[arg(long, requires = "pack")]
        force: bool,
        /// Emit a resumable main-thread session (new id) under the Claude store or --out.
        #[arg(long, conflicts_with = "agent_of")]
        session: bool,
        /// Emit a new sub-agent transcript owned by this ROOT session (id or transcript path).
        #[arg(long, value_name = "ROOT")]
        agent_of: Option<String>,
        /// With --agent-of: the model the new sub-agent runs on, written to its meta (`opus`,
        /// `sonnet`, a full model id, or `inherit`: the root's current model, which is also what
        /// a meta without `model` means). Default: the source's meta value.
        #[arg(long, value_name = "ID", requires = "agent_of")]
        model: Option<String>,
        /// With --session: rehome the new session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// With --session: write under this directory instead of the Claude store.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Report as JSON on stdout (stats + what was written).
        #[arg(long)]
        json: bool,
    },
    /// Rescue sub-agents stranded by a dead Claude Code session into a live one, so the live
    /// session can `SendMessage` them and they resume with their whole transcript: copies
    /// `subagents/agent-<id>.jsonl` (with `sessionId` restamped on every line, nothing else
    /// changed) and its `.meta.json` into the live session's `subagents/`. Find the agents with
    /// `cv lanes <session>` (one session) or `cv lanes --recent N --unfinished` (the project's
    /// recent sessions).
    Adopt {
        /// Agent ids (`a5d8…` or `agent-a5d8…`, unique prefix ok).
        #[arg(required_unless_present_any = ["list", "orphans"])]
        agents: Vec<String>,
        /// The live session that adopts them (default: the newest session in the agent's
        /// project other than the one holding it, printed before acting).
        #[arg(long, value_name = "SESSION")]
        into: Option<String>,
        /// The dead session that holds them (default: search every session's `subagents/`;
        /// when several hold a copy, the newest copy is taken).
        #[arg(long, value_name = "SESSION")]
        from: Option<String>,
        /// Print what would be copied; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Replace an agent file already present in the live session.
        #[arg(long)]
        force: bool,
        /// The model the adopted sub-agent runs on when resumed, written to its `agent-<id>.meta.json`
        /// (`opus`, `sonnet`, a full model id, or `inherit`: the root's current model, which is
        /// also what a meta without `model` means). Default: the copied meta's own value.
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        // Removed in 0.15.0 — hidden so the old spellings error with a pointer (exit 2).
        #[arg(long, hide = true, value_name = "SESSION")]
        list: Option<String>,
        #[arg(long, hide = true)]
        orphans: bool,
        #[arg(long)]
        json: bool,
    },
    /// Compose a new session from spans of existing ones (`<id>:A..B`): a verbatim prefix
    /// (`<id>:..N`) forks a lane at message N; two spans graft one session's continuation onto
    /// another's head. `--session` emits a resumable Claude session, `--agent-of <root>` a new
    /// sub-agent of that root (as `distill` does); `--harness`/`--out` emit to any harness; with
    /// neither, a summary prints (`--export md|json` prints the composed session).
    Splice {
        /// One or more specs: `<id>:A..B`, `<id>:A..` (through the last), `<id>:..B`, or `<id>`
        /// (the whole session). Indices are 0-based, end-exclusive; `<id>` may be `harness:id` or a
        /// transcript path.
        #[arg(required = true)]
        specs: Vec<String>,
        /// Emit a resumable main-thread Claude session (new id) under the Claude store or --out.
        #[arg(long, conflicts_with_all = ["agent_of", "harness", "export", "generate"])]
        session: bool,
        /// Emit a new sub-agent transcript owned by this ROOT session (id or transcript path).
        #[arg(long, value_name = "ROOT", conflicts_with_all = ["harness", "export", "generate", "out", "cwd"])]
        agent_of: Option<String>,
        /// Target harness for the composed session (defaults to the first spec's harness).
        #[arg(long)]
        harness: Option<String>,
        /// Write under this directory instead of the target's real storage root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// With --session / --agent-of: report as JSON on stdout (what was written, how to resume).
        #[arg(long)]
        json: bool,
        /// What to do with the model's reasoning on the way out: `native` keeps it where the target
        /// format holds it and drops it elsewhere, `text` keeps the turn by demoting thinking to
        /// text (a placeholder for provider-signed blobs, which have no text), `drop` omits it.
        /// `portable` demotes plaintext Claude reasoning, removes signature-only reasoning and repairs tool IDs;
        /// other targets use the text policy.
        #[arg(long, default_value = "native", value_parser = ["native", "text", "drop", "portable"])]
        thinking: String,
        /// Print the composed session instead of emitting it: md or json.
        #[arg(long)]
        export: Option<String>,
        /// Rehome the composed session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Generate a continuation of the composed session via an LLM and append it (the loom's
        /// generative step). Uses OPENROUTER/ANTHROPIC keys or LMSTUDIO_API_BASE for free local gen.
        #[arg(long)]
        generate: bool,
        /// Model for --generate (provider-specific; defaults to the provider's default).
        #[arg(long = "gen-model")]
        gen_model: Option<String>,
    },
    /// Produce a copy of a session that runs elsewhere: in another harness (`--harness`), from
    /// another working directory (`--cwd`, carrying CLAUDE.md/AGENTS.md/… along), or both. The
    /// source is never touched; the source harness rides on the id (`codex:019e…`).
    Port {
        id: String,
        /// Target harness (claude, codex, grok, …). Defaults to the source harness (a pure rehome).
        #[arg(long)]
        harness: Option<String>,
        /// New working directory for the ported session.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Write under this directory instead of the target's real storage root (safe dry run).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Don't copy project context files (CLAUDE.md, MEMORY.md, AGENTS.md, …) to the new cwd.
        #[arg(long = "no-context")]
        no_context: bool,
        /// Fail if the fidelity check finds a loss the target format could have carried. Losses the
        /// format inherently cannot hold are still only reported under `⚠ lossy:`.
        #[arg(long)]
        strict: bool,
        /// Native keeps reasoning where the target holds it; portable demotes plaintext thinking to
        /// text, removes signature-only reasoning and repairs tool IDs (other targets: the text
        /// policy); text keeps unsupported reasoning as text/placeholders; drop omits reasoning.
        /// Default: native when every recorded model is `claude-*`, else portable.
        #[arg(long, value_parser = ["native", "text", "drop", "portable"])]
        thinking: Option<String>,
        /// Restored Claude skills: keep all, drop all, or drop entries whose files changed or disappeared.
        #[arg(long = "skills", alias = "skills-policy", value_parser = skills_values(), default_value = "keep")]
        skills: String,
    },
    /// Print (or with --launch, run) the resume incantation for a session in its native harness.
    Resume {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Actually spawn the harness (cd to the session's cwd) instead of just printing.
        #[arg(long)]
        launch: bool,
    },

    /// Export a session to markdown or OpenSession JSON (stdout); `cv share` is the HTML artifact.
    Export {
        id: String,
        /// Output format: `md` (default) or `json`.
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        window: WindowArgs,
        /// Scrub secrets/PII first — API keys, private keys, JWTs, emails, `KEY=value` assignments,
        /// high-entropy blobs — with the same pass `cv share` runs; per-class counts go to stderr.
        #[arg(long)]
        redact: bool,
    },
    /// Export the corpus as a fine-tuning dataset (JSONL, one session per line).
    /// `chatml`/`sharegpt` import directly into Unsloth Studio / TRL / HF — no adapter.
    Dataset {
        /// `chatml` (default) → {"messages":[…]}, or `sharegpt` → {"conversations":[…]}.
        #[arg(long, default_value = "chatml")]
        format: String,
        /// Only this harness (claude, codex, hermes, …); omit for all.
        #[arg(long)]
        harness: Option<String>,
        /// Query calculus selecting which sessions to include, e.g. `model:fable`,
        /// `harness:claude msgs>=20`, `after:2026-01-01 cwd:/pug` (implicit AND; see `cv schema`).
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Also emit each session's sub-agent forest (the transcripts its adapter keeps beside it:
        /// Claude's direct and `Workflow` agents, Codex children, Kimi and Hermes delegates). These
        /// are first-class training data — and a parent on one model often spawns sub-agents on
        /// another, so `model:` queries need this to catch them.
        #[arg(long)]
        subagents: bool,
        /// Stop after N emitted records.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip sessions with fewer than this many messages (drops trivial/empty ones).
        #[arg(long, default_value_t = 2)]
        min_messages: usize,
        /// Scrub secrets/PII from every session before emitting (cv_core::redact).
        #[arg(long)]
        redact: bool,
        /// Scrub only these secret classes (comma list: `private_key`, `api_key`, `jwt`, `email`,
        /// `blob`, `assignment`), leaving everything else intact — e.g. `--redact-only private_key`
        /// to strip PEM blocks but keep identities. Implies redaction; overrides `--redact`.
        #[arg(long, value_name = "CLASSES")]
        redact_only: Option<String>,
        /// Write to this file instead of stdout.
        #[arg(long)]
        file: Option<PathBuf>,
        // Renamed to `--file` in 0.18.0 (`--out` is a directory everywhere else) — hidden so the old flag errors with a pointer.
        #[arg(long = "out", hide = true)]
        out_removed: Option<PathBuf>,
        /// Replace an existing output file by atomic rename (otherwise a taken name exits 2).
        #[arg(long)]
        force: bool,
    },
    /// Compile a context bundle for a new task from your whole session corpus.
    Pack {
        /// What you're about to work on — recall finds the relevant past spans.
        task: String,
        /// Output shape: `md` (CLAUDE.md-style digest, default), `prompt` (system prompt),
        /// or `session` (a synthetic resumable session — see `--harness`).
        #[arg(long, default_value = "md", value_parser = ["md", "prompt", "session"])]
        format: String,
        /// Target harness when `--format session`.
        #[arg(long)]
        harness: Option<String>,
        /// Max past sessions to draw from.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Write the md/prompt bundle to this file, or `--format session` under this directory.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace an existing output file by atomic rename (otherwise a taken name exits 2).
        #[arg(long)]
        force: bool,
        /// What to do with the model's reasoning on the way out: `native` keeps it where the target
        /// format holds it and drops it elsewhere, `text` keeps the turn by demoting thinking to
        /// text (a placeholder for provider-signed blobs, which have no text), `drop` omits it.
        /// `portable` demotes plaintext Claude reasoning, removes signature-only reasoning and repairs tool IDs;
        /// other targets use the text policy.
        #[arg(long, default_value = "native", value_parser = ["native", "text", "drop", "portable"])]
        thinking: String,
        /// Return the bundle or written artifact as a JSON result.
        #[arg(long)]
        json: bool,
        /// Bytes of each span's transcript the `CV_PACK_LLM` digest may send. Default: the model's
        /// context window as its provider reports it, less the digest's output and prompt (a token
        /// is at least a byte). The bundle states each digest's input and limit.
        #[arg(long, value_name = "N")]
        context_bytes: Option<usize>,
        /// Bytes of one message an excerpt shows; a longer one is cut there, an open code fence
        /// closed, and marked `…`. The bundle's header states this and every other bound, and
        /// what each cut.
        #[arg(long, value_name = "N", default_value_t = pack::EXCERPT_BYTES)]
        excerpt_bytes: usize,
    },

    /// Fleet task substrate: dispatch, review, and git-observed landing.
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Follow live agent activity across harnesses (tail -f for sessions).
    Scry {
        #[arg(long)]
        harness: Option<String>,
        /// Only follow sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Poll interval in seconds (0.25 minimum; negative is a usage error).
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Also emit the sessions that already exist at startup (default: only new activity).
        #[arg(long)]
        existing: bool,
        /// Keep following when stdout is not a terminal (a pipe gets the current state once without it).
        #[arg(long, short = 'f')]
        follow: bool,
        /// Follow for this long (30s, 5m, 2h), then exit 0; implies --follow.
        #[arg(id = "for", long = "for", value_name = "DURATION")]
        for_: Option<String>,
    },
    /// Follow live activity and archive every session as it changes, forever (archives what
    /// already exists on the first poll). `cv scry` is the same feed without the archive.
    Watch {
        /// Poll interval in seconds (0.25 minimum; negative is a usage error).
        #[arg(long, default_value_t = 3.0)]
        interval: f64,
        /// Only this harness (e.g. claude, codex, gemini).
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Archive home (default: $CLUSTERVISION_HOME or ~/.clustervision).
        #[arg(long, value_name = "DIR")]
        home: Option<PathBuf>,
        // Removed in 0.15.0 with the task verifier — hidden so the old flag errors with a pointer.
        #[arg(long = "verify-interval", hide = true)]
        verify_interval: Option<u64>,
    },
    /// The durable archive: every session snapshotted as IR under one home, for search and
    /// safekeeping across machines (`sync` once, `ls`, `path`; `cv watch` keeps it current).
    Archive {
        /// Archive home (default: $CLUSTERVISION_HOME or ~/.clustervision).
        #[arg(long, global = true, value_name = "DIR")]
        home: Option<PathBuf>,
        #[command(subcommand)]
        action: ArchiveCmd,
    },
    /// One local HTTP server: the read API at /api/* (what the dashboard and app read), the task
    /// inbox at /inbox (decisions as buttons; writes record the same events the CLI does, as
    /// `web:<who>`), and with `--web DIR` the dashboard at /. Loopback only unless `--host` is
    /// non-loopback AND a token (or `--insecure-expose`) is given.
    Serve {
        /// Port to listen on (0 picks a free one; the banner prints the real port).
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Host/interface to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Also host the browser dashboard from this directory (the repo's `web/`).
        #[arg(long, value_name = "DIR")]
        web: Option<PathBuf>,
        /// Require `Authorization: Bearer <token>` on every /api/* request ($CV_SERVE_TOKEN works too).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Allow a non-loopback --host without a token. The transcript corpus can contain
        /// secrets; anyone who can reach the port can read all of it and act in the inbox.
        #[arg(long)]
        insecure_expose: bool,
        /// Whose inbox /inbox opens on, and who a write is by when it names nobody. Default: $CV_ENDPOINT.
        #[arg(long)]
        who: Option<String>,
        /// Open the inbox in the default browser once the server is up.
        #[arg(long)]
        open: bool,
    },
    /// The MCP server (stdio JSON-RPC): every Read/Reshape/Export/System command as a tool, built
    /// from this binary's own clap tree, plus `await_omen`, `observe_stream` and the task tools.
    /// Register it with `claude mcp add clustervision -- /abs/path/to/cv mcp`.
    Mcp,
    /// Redact a session and emit a single self-contained HTML artifact anyone can open.
    Share {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Write here instead of `./<id>.html`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace an existing output file by atomic rename (otherwise a taken name exits 2).
        #[arg(long)]
        force: bool,
        /// Skip the redaction pass (default is to scrub secrets/PII before sharing).
        #[arg(long)]
        no_redact: bool,
    },

    /// Build/refresh the tantivy full-text index that makes `cv search` instant.
    ///
    /// Incremental by default: only changed/new sessions are re-indexed and vanished ones reaped,
    /// so routine refreshes are fast and light. Use `--rebuild` to clear and rebuild from scratch.
    /// Tool-call events (file edits/reads, commands, errors) ride along on the same pass into the
    /// catalog, powering `cv events` and `cv touched`.
    Index {
        /// Also build semantic embeddings (`cv search --semantic`). Downloads a small embedding
        /// model (~30MB) on first use.
        #[arg(long)]
        semantic: bool,
        /// Clear and rebuild the index from scratch instead of incrementally updating it.
        #[arg(long)]
        rebuild: bool,
        /// Also fold every session's sub-agent forest (its Task/Workflow agent transcripts) into
        /// BOTH the full-text index and the event catalog, tagged with provenance (which agent of
        /// which workflow of which parent). Off by default: it can add ~900MB to the index and many
        /// hundreds of transcripts to ingest. Without it, only top-level sessions are indexed.
        #[arg(long)]
        subagents: bool,
        /// Fail (exit 1) when any session cannot be read. By default such sessions are warnings:
        /// the index commits the rest and a later pass retries them.
        #[arg(long)]
        strict: bool,
        /// Print the result as JSON: `{indexed, changed, removed, errors, path, unreadable:
        /// [{id, harness, reason}]}`.
        #[arg(long)]
        json: bool,
    },
    /// View the user config (`~/.config/clustervision/config.toml`) and manage the export-source index.
    /// With no flag, prints the config path + registered export sources. Account data exports
    /// (ChatGPT/Claude.ai `conversations.json`) have no fixed home, so register the dirs/files you want
    /// the `chatgpt-export`/`claude-export` harnesses to discover.
    Config {
        /// Register an export source (a dir to scan, or a `conversations.json` file).
        #[arg(long, value_name = "PATH")]
        add_export: Option<PathBuf>,
        /// Unregister a previously-added export source.
        #[arg(long, value_name = "PATH")]
        rm_export: Option<PathBuf>,
    },
    /// The reference: the `-q` query calculus (every field, operator, example) and, with `--json`,
    /// the machine-readable schema of every shape cv emits (session row, message, block, query).
    /// `--commands --json` dumps the whole command tree for tool generation.
    Schema {
        /// Emit the machine-readable schema as JSON instead of the human reference.
        #[arg(long)]
        json: bool,
        /// The command tree instead of the data schema: every visible command and subcommand with
        /// its group, about text, and args (name, kind, value_type, possible_values, default, help,
        /// required). Human list without `--json`, one JSON array with it.
        #[arg(long)]
        commands: bool,
    },
    /// The format census: what the most recent sessions actually contain, per harness — message
    /// kinds, block types and the record vocabulary the adapter did not interpret. Types the
    /// manifest does not name are marked `NEW`, the drift signal.
    Formats {
        /// Only this harness.
        #[arg(long)]
        harness: Option<String>,
        /// How many recent sessions per harness to parse.
        #[arg(long, default_value_t = 20, value_name = "N")]
        recent: usize,
        #[arg(long)]
        json: bool,
        // The 0.14 subcommands (`census`, `check`) — hidden so they error with a pointer (exit 2).
        #[arg(hide = true)]
        legacy: Option<String>,
    },
    /// The agent quickstart: the things agents do with cv, each as one command line with the JSON
    /// keys it returns.
    Recipes,
    /// Wire cv into Claude Code: install / remove / show the five hooks (`cv hook …`) and the `cv`
    /// skill in its settings. Idempotent and reversible; agents set this up for themselves.
    Hooks {
        #[command(subcommand)]
        action: HooksCmd,
    },
    /// One hook's body, run by the harness: reads the hook JSON on stdin, prints a line or two of
    /// hook JSON (or nothing). `cv hooks install <harness>` writes the entries that call these.
    Hook {
        #[arg(value_enum)]
        name: HookName,
        /// The harness whose payload is on stdin (`claude` or `codex`; default `claude`).
        #[arg(long)]
        harness: Option<String>,
    },

    // ───────────────────────────── Removed (hidden stubs: error with a pointer, exit 2) ─────────────────────────────
    #[command(hide = true)]
    Timeline {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Redact {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Diff {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Fork {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Loom {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Tree {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Compaction {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Prompts {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    // Removed in 0.15.0 (the coordination board) — same hidden-stub rule.
    #[command(hide = true)]
    Board {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ArchiveCmd {
    /// Discover and archive every session once, then exit (idempotent: unchanged ones are skipped).
    Sync,
    /// List what's in the archive, newest activity first.
    Ls,
    /// Print the archive location.
    Path,
}

#[cfg(test)]
thread_local! { static CLI_TREE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// The clap tree with the grouped listing attached (`after_help` is computed from the tree itself,
/// so it can't drift from the commands).
fn build_cli() -> clap::Command {
    #[cfg(test)]
    CLI_TREE_BUILDS.with(|count| count.set(count.get() + 1));
    let cmd = Cli::command();
    let listing = grouped_help(&cmd);
    cmd.after_help(listing)
}

fn main() {
    // Die quietly on a closed pipe (`cv task list | head`) instead of panicking:
    // Rust ignores SIGPIPE by default, which turns EPIPE into a write panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = run() {
        // Caller mistakes (unknown/ambiguous id, removed command/flag, bad window, a taken output
        // name) exit 2 with the bare message; everything else is a real failure: the anyhow
        // chain, exit 1.
        if e.downcast_ref::<UsageError>().is_some() || e.downcast_ref::<cv_core::publish::Exists>().is_some() {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}

/// Only MCP retains this parsed tree for startup introspection. Other commands release it
/// before executing; MCP transfers ownership to extraction, which then releases it.
fn retain_mcp_tree(tree: clap::Command, cli: &Cli) -> Option<clap::Command> {
    matches!(&cli.cmd, Cmd::Mcp).then_some(tree)
}

fn run() -> Result<()> {
    let mut tree = build_cli();
    let matches = tree.get_matches_mut();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // Install before any library read, and enter it so subprocesses (including MCP tools)
    // inherit the flag as CV_FROZEN without mutating the process environment.
    let env = cv_core::env::Env::from_process();
    let env = if cli.frozen { env.with_frozen() } else { env };
    cv_core::env::Env::install(env.clone());
    let _env = env.enter();
    if cv_core::env::frozen() {
        let writes = match &cli.cmd {
            Cmd::Index { .. }
            | Cmd::Prune { .. }
            | Cmd::Rewind { .. }
            | Cmd::Adopt { .. }
            | Cmd::Port { .. }
            | Cmd::Distill { .. }
            | Cmd::Splice { .. }
            | Cmd::Task { .. }
            | Cmd::Hook { .. }
            | Cmd::Watch { .. } => true,
            Cmd::Hooks { action } => !matches!(action, HooksCmd::Show { .. }),
            Cmd::Archive { action, .. } => matches!(action, ArchiveCmd::Sync),
            Cmd::Config { add_export, rm_export } => add_export.is_some() || rm_export.is_some(),
            Cmd::Pack { format, .. } => format == "session",
            Cmd::Resume { launch, .. } => *launch,
            _ => false,
        };
        if writes {
            return usage("--frozen (CV_FROZEN=1) forbids this command: it writes stores or sessions");
        }
    }
    for (old, new) in [
        ("CLAURDVOYANT_HOME", "CLUSTERVISION_HOME"),
        ("CLAURDVOYANT_MAX_STALE_SECS", "CLUSTERVISION_MAX_STALE_SECS"),
    ] {
        if cv_core::env::var_os(old).is_some() {
            return usage(format!("`${old}` was removed in 0.18.0 — use `${new}`; unset `${old}`"));
        }
    }
    let mcp_tree = retain_mcp_tree(tree, &cli);
    // Long-running services scope reuse per request/poll, so cold private snapshots do not
    // survive idle server time. Ordinary commands share one cache across their session reads.
    let _reads = (!matches!(
        &cli.cmd,
        Cmd::Serve { .. }
            | Cmd::Mcp
            | Cmd::Watch { .. }
            | Cmd::Scry { .. }
            | Cmd::Workflow { follow: true, .. }
            | Cmd::Workflow { for_: Some(_), .. }
    ))
    .then(cv_core::source::read_scope);
    match cli.cmd {
        Cmd::Ls {
            harness,
            cwd,
            query,
            limit,
            sort_by,
            asc,
            fresh,
            json,
            enrich,
        } => browse::cmd_ls(browse::LsArgs { harness, cwd, query, limit, sort_by: &sort_by, asc, fresh, json, enrich }),
        Cmd::Search {
            query,
            harness,
            limit,
            semantic,
            scan,
            kind,
            live_bytes,
            json,
        } => search::cmd_search(&query, harness, limit, semantic, scan, kind, live_bytes, json),
        Cmd::Show {
            id,
            harness,
            json,
            window,
            subagents,
            agent,
            pre_compaction,
            tree,
            kind,
            summaries,
        } => view::cmd_show(
            &id,
            harness,
            json,
            &window,
            view::ShowView {
                subagents,
                agent,
                pre_compaction,
                tree,
                kind,
                summaries,
            },
        ),
        Cmd::Cat {
            session,
            tool_use_id,
            harness,
            input,
        } => cat::cmd_cat(&session, &tool_use_id, harness, input),
        Cmd::Export {
            id,
            format,
            harness,
            window,
            redact,
        } => view::cmd_export(&id, &format, harness, &window, redact),
        Cmd::Dataset {
            format,
            harness,
            query,
            subagents,
            limit,
            min_messages,
            redact,
            redact_only,
            file,
            out_removed,
            force,
        } => {
            if out_removed.is_some() {
                return usage(
                    "`cv dataset --out` was renamed to `--file <path>` in 0.18.0 (`--out` is a directory \
                     everywhere else)",
                );
            }
            reshape::cmd_dataset(reshape::DatasetArgs { format: &format, harness, query, subagents, limit, min_messages, redact, redact_only, file, force })
        }
        Cmd::Port {
            id,
            harness,
            cwd,
            out,
            no_context,
            strict,
            thinking,
            skills,
        } => port::cmd_port(&id, harness, cwd, out, no_context, strict, thinking.as_deref().map(util::parse_thinking).transpose()?, skills.parse()?),
        Cmd::Scry {
            harness,
            cwd,
            interval,
            existing,
            follow,
            for_,
        } => live::cmd_scry(live::ScryArgs { harness, cwd, interval, existing, follow, for_ }),
        Cmd::Index {
            semantic,
            rebuild,
            subagents,
            strict,
            json,
        } => search::cmd_index(semantic, rebuild, subagents, strict, json),
        Cmd::Events {
            id,
            harness,
            kind,
            subagents,
            json,
            wide,
        } => events::cmd_events(&id, harness, kind, subagents, json, wide),
        Cmd::Touched {
            path,
            edits_only,
            json,
        } => events::cmd_touched(&path, edits_only, json),
        Cmd::Blame { file, lines, show } => blame::cmd_blame(&file, lines.as_deref(), show),
        Cmd::Share {
            id,
            harness,
            out,
            force,
            no_redact,
        } => share::cmd_share(&id, harness, out, force, no_redact),
        Cmd::Pack {
            task,
            format,
            harness,
            limit,
            out,
            force,
            thinking,
            json,
            context_bytes,
            excerpt_bytes,
        } => pack::cmd_pack(pack::PackArgs {
            task: &task,
            format: &format,
            harness,
            limit,
            out,
            force,
            thinking: util::parse_thinking(&thinking)?,
            json,
            context_bytes,
            excerpt_bytes,
        }),
        Cmd::Stats { query, json, tokens } => browse::cmd_stats(query, json, tokens),
        Cmd::Prune {
            id,
            harness,
            min_size,
            keep_last,
            to,
            force,
            drop,
            drop_thinking,
            copy_resources,
            no_revive,
            window,
            keep,
            declassify,
            declassify_tokens,
            declassify_tokens_file,
            skills,
            dry_run,
            json,
        } => {
            reshape::cmd_prune(reshape::PruneArgs {
                id: &id, harness, min_size, keep_last, to, force, drop, thinking: drop_thinking,
                copy_resources, revive: !no_revive, window, keep, declassify,
                declassify_tokens, declassify_tokens_file, skills_policy: skills.parse()?, dry_run, json,
            })
        }
        Cmd::Rewind {
            id,
            harness,
            at,
            agent,
            out,
            to,
            from_compaction,
            thinking,
            skills,
            full,
            dry_run,
            json,
        } => {
            if from_compaction {
                return usage(
                    "`cv rewind --from-compaction` was removed in 0.18.0 — starting at the last compaction boundary is the default; \
                     pass `--full` for the whole history",
                );
            }
            reshape::cmd_rewind(reshape::RewindArgs { id: &id, harness, agent, at: at.as_deref(), out, to, full, thinking: thinking.as_deref().map(util::parse_thinking).transpose()?, skills_policy: skills.parse()?, dry_run, json, generator: BUILD_VERSION })
        }
        Cmd::Config { add_export, rm_export } => config::cmd_config(add_export, rm_export),
        Cmd::Schema { json, commands } => schema::cmd_schema(&build_cli(), json, commands),
        Cmd::Formats {
            harness,
            recent,
            json,
            legacy,
        } => formats::cmd_formats(harness, recent, json, legacy),
        Cmd::Recipes => recipes::cmd_recipes(),
        Cmd::Hooks { action } => hooks::cmd_hooks(action),
        Cmd::Hook { name, harness } => hooks::cmd_hook(name, harness),
        Cmd::Resume { id, harness, launch } => port::cmd_resume(&id, harness, launch),
        Cmd::Workflow {
            id,
            run_id,
            harness,
            json,
            script,
            results,
            follow,
            for_,
            revive,
            revive_all,
        } => workflow::cmd_workflow(workflow::WorkflowArgs { id: &id, run_id, harness, json, script, results, follow, for_, revive, revive_all }),
        Cmd::Tools {
            id,
            harness,
            agent,
            tool,
            workflow: wf,
            across,
            timeline,
            json,
        } => tools::cmd_tools(tools::ToolsArgs { id: &id, harness, agent, tool, workflow: wf, across, timeline, json }),
        Cmd::Lanes {
            id,
            harness,
            recent,
            cwd,
            running,
            done,
            stranded,
            unfinished,
            since,
            tasks,
            json,
        } => {
            let filter = if running {
                session_reports::LaneFilter::Running
            } else if done {
                session_reports::LaneFilter::Done
            } else if stranded {
                session_reports::LaneFilter::Stranded
            } else if unfinished {
                session_reports::LaneFilter::Unfinished
            } else {
                session_reports::LaneFilter::All
            };
            let scope = match (id, recent) {
                (Some(id), _) => session_reports::LaneScope::Session { id, harness },
                (None, Some(n)) => session_reports::LaneScope::Recent {
                    n,
                    dir: match cwd {
                        Some(d) => d,
                        None => std::env::current_dir().context("reading the current directory")?,
                    },
                },
                (None, None) => unreachable!("clap requires <ID> unless --recent is present"),
            };
            session_reports::cmd_lanes(scope, filter, since, tasks, json)
        }
        Cmd::Deferrals {
            id,
            harness,
            at,
            since_removed,
            open_tasks,
            phrases_file,
            json,
        } => {
            if since_removed.is_some() {
                return usage(
                    "`cv deferrals --since` was renamed to `--at <MSG>` in 0.18.0 (`--since` is a duration \
                     on `cv lanes`)",
                );
            }
            // An unmatched deferral is a finding, not a failure: plain exit 1, no error text.
            if !session_reports::cmd_deferrals(&id, harness, at, open_tasks, phrases_file, json)? {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Doctor {
            id,
            harness,
            recent,
            json,
        } => doctor::cmd_doctor(id, harness, recent, json),
        Cmd::Task { action } => task::cmd_task(action),
        Cmd::Mcp => cmd::mcp::cmd_mcp(mcp_tree.expect("MCP retained its parsed command tree")),
        Cmd::Watch {
            interval,
            harness,
            cwd,
            home,
            verify_interval,
        } => {
            if verify_interval.is_some() {
                return usage(
                    "`--verify-interval` was removed in 0.15.0 — the task git-verifier is gone; \
                     completion is self-reported and labeled so (`cv task done --observed`)",
                );
            }
            archive::cmd_watch(&archive::Archive::resolve(home)?, interval, harness, cwd)
        }
        Cmd::Archive { home, action } => {
            let a = archive::Archive::resolve(home)?;
            match action {
                ArchiveCmd::Sync => archive::cmd_sync(&a),
                ArchiveCmd::Ls => archive::cmd_ls(&a),
                ArchiveCmd::Path => archive::cmd_path(&a),
            }
        }
        Cmd::Serve {
            port,
            host,
            web,
            token,
            insecure_expose,
            who,
            open,
        } => serve::cmd_serve(serve::ServeOpts {
            host,
            port,
            web,
            token,
            insecure_expose,
            who: who.or_else(cv_core::task::default_endpoint),
            open,
        }),
        Cmd::Distill {
            id,
            harness,
            keep_last,
            tail_result_max,
            note_max,
            keep_tail_thinking,
            no_ledger,
            upto,
            with,
            pack,
            force,
            session,
            agent_of,
            model,
            cwd,
            out,
            json,
        } => {
            let opts = cv_core::distill::DistillOptions {
                keep_last,
                tail_result_max,
                note_max,
                keep_tail_thinking,
                ledger: !no_ledger,
                retrieve_ref: None,
            };
            let target = cmd::distill::target(session, agent_of, model, cwd, out)?;
            cmd::distill::cmd_distill(&id, harness, opts, upto, &with, pack, force, target, json)
        }
        Cmd::Adopt {
            agents,
            into,
            from,
            dry_run,
            force,
            model,
            list,
            orphans,
            json,
        } => {
            if let Some(session) = list {
                return usage(format!(
                    "`cv adopt --list <session>` was removed in 0.15.0 — use `cv lanes {session}` (status, model, description per agent)"
                ));
            }
            if orphans {
                return usage(
                    "`cv adopt --orphans` was removed in 0.15.0 — use `cv lanes --recent <N> --unfinished [--cwd <project>]` \
                     (unfinished sub-agents of the project's recent sessions, each row tagged with its session)",
                );
            }
            cmd::adopt::cmd_adopt(cmd::adopt::AdoptArgs {
                agents,
                into,
                from,
                dry_run,
                force,
                model,
                json,
            })
        }
        Cmd::Splice {
            specs,
            session,
            agent_of,
            harness,
            out,
            json,
            export,
            cwd,
            generate,
            gen_model,
            thinking,
        } => {
            // `--session` / `--agent-of` take the distill destinations (`--out`/`--cwd` ride along
            // with `--session`); without them `--out`/`--cwd` keep their harness-emit meaning.
            let resumable = if session || agent_of.is_some() {
                Some(cmd::distill::target(session, agent_of, None, cwd.clone(), out.clone())?)
            } else {
                None
            };
            reshape::cmd_splice(reshape::SpliceArgs { specs: &specs, harness, out, export, cwd, generate, gen_model, thinking: util::parse_thinking(&thinking)?, resumable, json })
        }
        // Removed commands: one line each, pointing at the replacement; exit 2.
        Cmd::Timeline { .. } => usage(
            "`cv timeline` was removed in 0.15.0 — use `cv ls --sort-by updated --asc` (the newest --limit rows, \
             oldest first; `--json` gives the same session rows)",
        ),
        Cmd::Redact { .. } => usage(
            "`cv redact <id> [--format md|json]` was removed in 0.15.0 — use `cv export <id> --redact [--format md|json]` \
             (the per-class counts print to stderr); `cv share <id>` for a redacted HTML artifact",
        ),
        Cmd::Diff { .. } => usage(
            "`cv diff` was removed in 0.15.0 — `cv export <id> --format md` each session and diff the two files",
        ),
        Cmd::Fork { .. } => usage(
            "`cv fork <id> --at N` was removed in 0.15.0 — use `cv splice <id>:..N --session` (or `--agent-of <root>`): \
             the verbatim prefix as a new resumable session",
        ),
        Cmd::Loom { .. } => usage(
            "`cv loom <base> --at N --graft <other> --from M` was removed in 0.15.0 — use `cv splice <base>:..N <other>:M..` \
             (two spans; `--session`, `--harness <h>` or `--export md` as before)",
        ),
        Cmd::Tree { .. } => usage(
            "`cv tree <id>` was removed in 0.15.0 — use `cv show <id> --tree` (the same threading, then the sub-agent forest)",
        ),
        Cmd::Compaction { .. } => usage(
            "`cv compaction <id>` was removed in 0.15.0 — use `cv show <id> --kind compaction_boundary,compaction_summary` \
             (`--summaries` and `--json` as before)",
        ),
        Cmd::Prompts { .. } => usage(
            "`cv prompts <id>` was removed in 0.15.0 — use `cv show <id> --kind prompt,answer` \
             (the same rows; `--json` and `--pre-compaction [N]` as before)",
        ),
        Cmd::Board { .. } => usage(
            "`cv board` was removed in 0.15.0 — the task log is the fleet's record: `cv task note` to say something, \
             `cv task events --since <id>` to follow it, `cv task claim` to hold work",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::reshape::resolve_declassify_tokens;

    #[test]
    fn declassify_tokens_resolve_from_csv_and_file() {
        // CSV alone: trimmed, lowercased, de-duplicated, order-preserving.
        let got = resolve_declassify_tokens(Some("Exploit, credential ,exploit,".into()), None).unwrap();
        assert_eq!(got, vec!["exploit", "credential"]);

        // File alone: one term per line, `#` comments (inline too) and blanks ignored.
        let dir = std::env::temp_dir().join(format!("cv-declass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("terms.txt");
        std::fs::write(&path, "# a comment\nExfil\n\nauth bypass  # trailing note\n exfil \n").unwrap();
        let got = resolve_declassify_tokens(None, Some(path.clone())).unwrap();
        assert_eq!(got, vec!["exfil", "auth bypass"]);

        // CSV + file compose; duplicates across sources collapse; neither source is required.
        let got = resolve_declassify_tokens(Some("exfil,Attacker".into()), Some(path)).unwrap();
        assert_eq!(got, vec!["exfil", "attacker", "auth bypass"]);
        assert!(resolve_declassify_tokens(None, None).unwrap().is_empty());

        // A missing file is a real error naming the path, not a silent empty list.
        let err = resolve_declassify_tokens(None, Some(dir.join("nope.txt"))).unwrap_err();
        assert!(err.to_string().contains("declassify-tokens-file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skills_policy_spelling_parses_to_the_canonical_skills_value() {
        crate::on_main_sized_stack(|| {
            for (argv, want) in [
                (["cv", "rewind", "s", "--skills-policy", "keep"], "keep"),
                (["cv", "prune", "s", "--skills-policy", "drop-all"], "drop"),
                (["cv", "port", "s", "--skills", "drop-stale"], "drop-stale"),
            ] {
                let m = build_cli().try_get_matches_from(argv).unwrap();
                let (_, sub) = m.subcommand().unwrap();
                assert_eq!(
                    sub.get_one::<String>("skills").map(String::as_str),
                    Some(want),
                    "{argv:?}"
                );
            }
            assert!(build_cli()
                .try_get_matches_from(["cv", "rewind", "s", "--skills", "bogus"])
                .is_err());
        });
    }

    #[test]
    fn mcp_reuses_one_parsed_tree_with_exact_schema_and_releases_parser_owners() {
        crate::on_main_sized_stack(|| {
            use std::sync::Arc;
            #[derive(Clone)]
            struct Owner(Arc<()>);
            impl clap::builder::TypedValueParser for Owner {
                type Value = String;
                fn parse_ref(
                    &self,
                    _: &clap::Command,
                    _: Option<&clap::Arg>,
                    value: &std::ffi::OsStr,
                ) -> Result<String, clap::Error> {
                    let _owner = &self.0;
                    Ok(value.to_string_lossy().into_owned())
                }
            }
            // Independent old startup recipe: argument tree consumed, then a fresh tree for MCP.
            CLI_TREE_BUILDS.with(|count| count.set(0));
            let parsed = build_cli().try_get_matches_from(["cv", "mcp"]).unwrap();
            assert!(matches!(Cli::from_arg_matches(&parsed).unwrap().cmd, Cmd::Mcp));
            let fresh = build_cli();
            let expected_schema = cmd::schema::commands_json(&fresh);
            let expected_tools = cmd::mcp_tools::CliTools::from_tree(&fresh).unwrap().tool_list();
            assert_eq!(CLI_TREE_BUILDS.with(|count| count.get()), 2);
            drop(fresh);

            for arguments in [vec!["cv", "mcp"], vec!["cv", "show", "example"]] {
                CLI_TREE_BUILDS.with(|count| count.set(0));
                let owner = Arc::new(());
                let weak = Arc::downgrade(&owner);
                let mut tree = build_cli().arg(
                    clap::Arg::new("tree-owner")
                        .long("tree-owner")
                        .hide(true)
                        .value_parser(clap::builder::ValueParser::new(Owner(owner))),
                );
                let parsed = tree.try_get_matches_from_mut(arguments).unwrap();
                let cli = Cli::from_arg_matches(&parsed).unwrap();
                let retained = retain_mcp_tree(tree, &cli);
                assert_eq!(CLI_TREE_BUILDS.with(|count| count.get()), 1);
                if let Some(tree) = retained {
                    assert!(
                        weak.upgrade().is_some(),
                        "MCP owns the original parsed tree until extraction"
                    );
                    assert_eq!(cmd::schema::commands_json(&tree), expected_schema);
                    let generated = cmd::mcp_tools::CliTools::from_owned_tree(tree).unwrap();
                    assert_eq!(generated.tool_list(), expected_tools);
                    assert!(
                        weak.upgrade().is_none(),
                        "extraction releases original and cloned parser owners"
                    );
                } else {
                    assert!(
                        weak.upgrade().is_none(),
                        "ordinary CLI commands release their tree before execution"
                    );
                }
            }
        });
    }

    #[test]
    fn clap_tree_is_valid() {
        crate::on_main_sized_stack(|| build_cli().debug_assert());
    }

    /// Every visible command sits in exactly one group, and every grouped name is a real
    /// command — the table and the enum cannot drift apart.
    #[test]
    fn groups_cover_every_visible_command() {
        crate::on_main_sized_stack(groups_cover_every_visible_command_body);
    }

    fn groups_cover_every_visible_command_body() {
        let cmd = Cli::command();
        let visible: Vec<&str> = cmd
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| s.get_name())
            .collect();
        for name in &visible {
            let n = GROUPS.iter().filter(|(_, cmds)| cmds.contains(name)).count();
            assert_eq!(
                n, 1,
                "visible command {name:?} must be in exactly one group (found {n})"
            );
        }
        for (group, names) in GROUPS {
            for name in names.iter() {
                assert!(
                    visible.contains(name),
                    "group {group:?} lists {name:?}, which is not a visible command"
                );
            }
        }
        // Names removed in this release are hidden stubs, never visible commands.
        for old in [
            "timeline",
            "redact",
            "diff",
            "board",
            "fork",
            "loom",
            "tree",
            "compaction",
            "prompts",
        ] {
            assert!(!visible.contains(&old), "{old} must be hidden");
            assert!(
                cmd.get_subcommands().any(|s| s.get_name() == old),
                "{old} stub must exist"
            );
        }
    }

    /// `cv --help` lists the groups and ends with the recipes pointer.
    #[test]
    fn help_is_grouped_and_ends_with_recipes() {
        crate::on_main_sized_stack(help_is_grouped_and_ends_with_recipes_body);
    }

    fn help_is_grouped_and_ends_with_recipes_body() {
        let help = build_cli().render_long_help().to_string();
        for (group, _) in GROUPS {
            assert!(
                help.contains(&format!("  {group}\n")),
                "group {group:?} missing:\n{help}"
            );
        }
        assert!(help.trim_end().ends_with(RECIPES_FOOTER), "footer:\n{help}");
        assert!(
            !help.contains("    timeline ") && !help.contains("    redact "),
            "stubs leak:\n{help}"
        );
        // The flat clap list is gone — commands appear under their group only.
        assert_eq!(help.matches("\n    ls ").count(), 1, "{help}");
    }
}
