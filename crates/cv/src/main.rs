//! `cv` — the clustervision CLI.
//!
//! This file holds only the clap surface (`Cli`/`Cmd`), the command groups, and the dispatch into
//! the per-command modules under `cmd/`; shared helpers live in `util.rs` and `cv blame` in `blame.rs`.
//!
//! Interface v2 (0.11.0, `docs/INTERFACE-V2.md`): one verb per act, the same flag means the same
//! thing everywhere, snake_case JSON, no hidden commands. Old names are hidden stubs that error
//! with a pointer and exit 2 — never aliases that work.

mod blame;
mod cmd;
mod util;

// Crate-root re-exports for `blame.rs`, which renders conversation windows via the same
// streaming renderer `cv show` uses.
pub(crate) use cmd::view::{show_header, show_message, stream_session_render, RenderOpts};
pub(crate) use util::short_id;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use cmd::live::BoardCmd;
use cmd::task::TaskCmd;
use cmd::{
    browse, cat, compose, config, doctor, formats, live, pack, port, provenance, recipes, schema, search, share, task,
    view, workflow,
};
use std::path::PathBuf;
use util::{usage, UsageError, WindowArgs};

/// Resolve the caller-supplied `--declassify` term list from a CSV flag and/or a file (one term per
/// line, `#` comments + blanks ignored). Terms are lowercased + de-duplicated. cv ships NO built-in
/// list — the terms are always external (config/data), so cv's own source carries no domain shitlist.
fn resolve_declassify_tokens(csv: Option<String>, file: Option<PathBuf>) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |t: &str| {
        let t = t.trim().to_ascii_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    };
    if let Some(csv) = csv {
        for t in csv.split(',') {
            push(t);
        }
    }
    if let Some(path) = file {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --declassify-tokens-file {}", path.display()))?;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("");
            push(line);
        }
    }
    Ok(out)
}

/// Version with the embedded build commit (set by build.rs; "unknown" outside git), so the
/// binary in your PATH is checkable against source.
const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CV_BUILD_SHA"), ")");

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
            "tree",
            "workflow",
            "compaction",
            "timeline",
            "stats",
            "diff",
            "blame",
            "doctor",
        ],
    ),
    ("Reshape", &["prune", "splice", "loom", "port", "redact", "resume"]),
    ("Export", &["export", "dataset", "pack"]),
    ("Fleet & live", &["task", "board", "scry", "share"]),
    ("System", &["index", "config", "schema", "formats", "recipes"]),
];

/// The group a visible command belongs to (`None` for hidden stubs and unknown names).
pub(crate) fn group_of(name: &str) -> Option<&'static str> {
    GROUPS.iter().find(|(_, cmds)| cmds.contains(&name)).map(|(g, _)| *g)
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
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    // ───────────────────────────── Read ─────────────────────────────
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
        /// fleet — so plain `ls --json` stays catalog-cheap; opt in when you need these fields.
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
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Use semantic (embedding) search instead of full-text. Requires `cv index --semantic`
        /// to have been run; downloads a small embedding model on first use.
        #[arg(long)]
        semantic: bool,
        /// Emit the hits as one JSON array instead of the table — the same hits in the same
        /// order (--harness/--limit/--semantic respected): a session row (`id, harness, path, cwd,
        /// title, created_at, updated_at, message_count, size_bytes`) plus `score`, `snippet`, and
        /// the sub-agent provenance `agent_id`/`parent_id`/`workflow`. Nothing else goes to stdout.
        #[arg(long)]
        json: bool,
    },
    /// List what a session DID: its extracted events (file edits/reads, commands, errors).
    ///
    /// Events are ingested during `cv index`; a session that isn't cataloged yet (or whose file
    /// changed since) is ingested on the spot — one streamed pass, large content stays on disk.
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
        /// Emit the events as one JSON array (snake_case keys) instead of the table.
        #[arg(long)]
        json: bool,
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
        /// One agent's histogram ("which tools did agent X use"). `<orchestrator>` for the parent.
        #[arg(long)]
        agent: Option<String>,
        /// Which agents used this tool, across the forest (ranked by count).
        #[arg(long)]
        tool: Option<String>,
        /// Restrict the analytics to one workflow run's agents.
        #[arg(long)]
        workflow: Option<String>,
        /// Per-agent breakdown (one row per agent) instead of the aggregate histogram.
        #[arg(long)]
        across: bool,
        /// The time-ordered tool-call timeline (optionally narrowed with `--agent`).
        #[arg(long)]
        timeline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Render a session's message threading (DAG if parent_ids exist, else a numbered list).
    Tree {
        id: String,
        #[arg(long)]
        harness: Option<String>,
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
        /// Salvage a dead or interrupted run: emit a ready-to-paste standalone `Agent` prompt per
        /// lane — the FULL original task plus the work that lane already landed (files written,
        /// commands run, its own last note) — so lanes come back resumed rather than restarted.
        #[arg(long)]
        revive: bool,
        /// With `--revive`, include lanes that completed successfully too (default: only the ones
        /// that did not finish).
        #[arg(long)]
        revive_all: bool,
    },
    /// Compaction boundaries in a session: every `/compact` (or auto-compaction), its trigger,
    /// pre-compaction context size, and the summary that seeded the next window. `--summaries`
    /// prints each full summary; `cv show <id> --pre-compaction` reads the lost pre-span.
    Compaction {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Print each compaction's full summary text (not just the head).
        #[arg(long)]
        summaries: bool,
        #[arg(long)]
        json: bool,
    },
    /// Unified chronological feed across all harnesses (oldest → newest).
    Timeline {
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Restrict the feed to sessions matching this query (see `cv schema`).
        #[arg(long, short = 'q')]
        query: Option<String>,
        #[arg(long, default_value_t = 60)]
        limit: usize,
        /// Emit the feed as one JSON array of session rows (same shape as `ls --json`), oldest first.
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
    },
    /// Compare two sessions message-by-message (great for loom branches).
    Diff {
        a: String,
        b: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Code provenance: which agent session wrote this code, and what was it thinking?
    ///
    /// Correlates the file's git history with the event catalog's file_edit events — an agent
    /// edit shortly before a commit is strong evidence that session authored it. Each matched
    /// commit gets its best sessions plus a `cv show --range` hint into the conversation around
    /// the edit. Run `cv index` first to ingest events.
    Blame {
        file: String,
        /// Only these lines: `<line>` or `<line>,<endline>` (via `git blame -L`).
        #[arg(short = 'L', value_name = "LINE[,ENDLINE]")]
        lines: Option<String>,
        /// Also print the conversation window around the single best-matched edit.
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

    // ───────────────────────────── Reshape ─────────────────────────────
    /// Custom, lossless compaction of a Claude session into a NEW resumable session: snip bulky old
    /// tool payloads (large reads/logs/screenshots) into a sidecar, leaving a small `[PRUNED]` marker
    /// — your prompts and flow stay verbatim, the recent turns stay sharp. Resume with
    /// `claude --resume <new-id>`; fetch a stashed original back with `cv cat <new-id> <tool_use_id>`.
    Prune {
        /// Source session id.
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Snip a tool payload only if it's larger than this many bytes.
        #[arg(long, default_value_t = 2048)]
        min_size: usize,
        /// Keep the last N conversational turns' payloads verbatim (recent context stays sharp).
        #[arg(long, default_value_t = 25)]
        keep_last: usize,
        /// New session id (default: a fresh UUID).
        #[arg(long)]
        to: Option<String>,
        /// Hard-drop payloads (no sidecar, irreversible) instead of stashing them for retrieval.
        #[arg(long)]
        drop: bool,
        /// Also flatten the OLDEST assistant reasoning (thinking blocks) — recent thinking (within
        /// --keep-last) stays verbatim. On a long session the chain-of-thought dominates the loaded
        /// context; flattening the old reasoning is the biggest lever for shrinking it. Lossless.
        #[arg(long)]
        thinking: bool,
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
        /// Report what would be pruned without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also emit the prune report as ONE JSON object on stdout (snake_case keys, FULL ids).
        /// The human report stays on stderr, so stdout carries only the JSON. Dry-run honest:
        /// nothing was written, so `new_path`/`sidecar_path` are null and `new_id` is null unless
        /// `--to` pinned it (see `note`).
        #[arg(long)]
        json: bool,
        // Removed in 0.11.0 — kept hidden so the old spelling errors with a pointer (exit 2).
        #[arg(long, hide = true, value_name = "TOOL_USE_ID")]
        retrieve: Option<String>,
        #[arg(long, hide = true, value_name = "RANGE")]
        range: Option<String>,
    },
    /// Compose a new session from spans of existing ones (`<id>:A..B`).
    Splice {
        /// One or more specs: `<id>:A..B`, `<id>:A..` (through the last), `<id>:..B`, or `<id>`
        /// (the whole session). Indices are 0-based, end-exclusive; `<id>` may be `harness:id`.
        #[arg(required = true)]
        specs: Vec<String>,
        /// Target harness for the composed session (defaults to the first spec's harness).
        #[arg(long)]
        harness: Option<String>,
        /// Write under this directory instead of the target's real storage root.
        #[arg(long)]
        out: Option<PathBuf>,
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
        #[arg(long, hide = true)]
        to: Option<String>,
    },
    /// Loom graft: take base[..N], then graft other[M..] into one new branched session.
    Loom {
        base: String,
        /// Keep `base[..N]`.
        #[arg(long, value_name = "N")]
        at: usize,
        /// The session to graft from.
        #[arg(long)]
        graft: String,
        /// Start grafting at `graft[M..]`.
        #[arg(long, value_name = "M")]
        from: usize,
        /// Target harness for the grafted session (defaults to the base's harness).
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the grafted session instead of emitting it: md or json.
        #[arg(long)]
        export: Option<String>,
        /// Rehome the grafted session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Generate a continuation of the grafted branch via an LLM and append it.
        #[arg(long)]
        generate: bool,
        /// Model for --generate.
        #[arg(long = "gen-model")]
        gen_model: Option<String>,
        #[arg(long, hide = true)]
        to: Option<String>,
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
        /// format inherently cannot hold are still only reported under `⚠ lost`.
        #[arg(long)]
        strict: bool,
        // Renamed in 0.11.0 — hidden so the old spellings error with a pointer (exit 2).
        #[arg(long, hide = true)]
        to: Option<String>,
        #[arg(long = "to-dir", hide = true)]
        to_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        from: Option<String>,
    },
    /// Scrub secrets/PII from a session and export it (safe to share).
    Redact {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long, default_value = "md")]
        format: String,
        /// Also print redaction counts per class to stderr.
        #[arg(long)]
        stats: bool,
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

    // ───────────────────────────── Export ─────────────────────────────
    /// Export a session to markdown, JSON, or self-contained HTML (stdout).
    Export {
        id: String,
        /// Output format: `md` (default), `json`, or `html`.
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        window: WindowArgs,
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
        /// Also emit each Claude session's sub-agent forest (directly- and `Workflow`-spawned
        /// transcripts in sidecar files). These are first-class training data — and a parent on one
        /// model often spawns sub-agents on another, so `model:` queries need this to catch them.
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
        out: Option<PathBuf>,
    },
    /// Compile a context bundle for a new task from your whole session corpus.
    Pack {
        /// What you're about to work on — recall finds the relevant past spans.
        task: String,
        /// Output shape: `md` (CLAUDE.md-style digest, default), `prompt` (system prompt),
        /// or `session` (a synthetic resumable session — see `--harness`).
        #[arg(long, default_value = "md")]
        format: String,
        /// Target harness when `--format session`.
        #[arg(long)]
        harness: Option<String>,
        /// Max past sessions to draw from.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, hide = true)]
        to: Option<String>,
    },

    // ───────────────────────────── Fleet & live ─────────────────────────────
    /// Fleet task substrate: dispatch, review, and git-observed landing.
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Post to / read from the agent coordination board.
    Board {
        #[command(subcommand)]
        action: BoardCmd,
    },
    /// Follow live agent activity across harnesses (tail -f for sessions).
    Scry {
        #[arg(long)]
        harness: Option<String>,
        /// Only follow sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Also emit the sessions that already exist at startup (default: only new activity).
        #[arg(long)]
        existing: bool,
    },
    /// Redact a session and emit a single self-contained HTML artifact anyone can open.
    Share {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Write here instead of `./<id>.html`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Skip the redaction pass (default is to scrub secrets/PII before sharing).
        #[arg(long)]
        no_redact: bool,
    },

    // ───────────────────────────── System ─────────────────────────────
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
    /// Audit cv against the harnesses themselves: what recent sessions actually contain
    /// (`census`), and whether each adapter still matches its pinned manifest (`check`).
    Formats {
        #[command(subcommand)]
        action: cmd::formats::FormatsCmd,
    },
    /// The agent quickstart: the ten things agents do with cv, each as one command line with the
    /// JSON keys it returns.
    Recipes,

    // ───────────────────────────── Removed in 0.11.0 (hidden stubs) ─────────────────────────────
    #[command(hide = true)]
    Convert {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Query {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Recall {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Distill {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
}

/// The clap tree with the grouped listing attached (`after_help` is computed from the tree itself,
/// so it can't drift from the commands).
fn build_cli() -> clap::Command {
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
        // Caller mistakes (unknown/ambiguous id, removed command/flag, bad window) exit 2 with the
        // bare message; everything else is a real failure: the anyhow chain, exit 1.
        if let Some(u) = e.downcast_ref::<UsageError>() {
            eprintln!("{u}");
            std::process::exit(2);
        }
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let matches = build_cli().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    match cli.cmd {
        Cmd::Ls {
            harness,
            cwd,
            query,
            limit,
            sort_by,
            fresh,
            json,
            enrich,
        } => browse::cmd_ls(harness, cwd, query, limit, &sort_by, fresh, json, enrich),
        Cmd::Search {
            query,
            harness,
            limit,
            semantic,
            json,
        } => search::cmd_search(&query, harness, limit, semantic, json),
        Cmd::Show {
            id,
            harness,
            json,
            window,
            subagents,
            agent,
            pre_compaction,
        } => view::cmd_show(&id, harness, json, &window, subagents, agent, pre_compaction),
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
        } => view::cmd_export(&id, &format, harness, &window),
        Cmd::Dataset {
            format,
            harness,
            query,
            subagents,
            limit,
            min_messages,
            redact,
            redact_only,
            out,
        } => compose::cmd_dataset(
            &format,
            harness,
            query,
            subagents,
            limit,
            min_messages,
            redact,
            redact_only,
            out,
        ),
        Cmd::Port {
            id,
            harness,
            cwd,
            out,
            no_context,
            strict,
            to,
            to_dir,
            from,
        } => {
            if to.is_some() {
                return usage("`cv port --to <harness>` was renamed in 0.11.0 — use `--harness <harness>`");
            }
            if to_dir.is_some() {
                return usage("`cv port --to-dir <dir>` was renamed in 0.11.0 — use `--cwd <dir>`");
            }
            if from.is_some() {
                return usage(
                    "`cv port --from <harness>` was removed in 0.11.0 — the source harness rides on the id: \
                     `cv port <harness>:<id>`",
                );
            }
            port::cmd_port(&id, harness, cwd, out, no_context, strict)
        }
        Cmd::Scry {
            harness,
            cwd,
            interval,
            existing,
        } => live::cmd_scry(harness, cwd, interval, existing),
        Cmd::Index {
            semantic,
            rebuild,
            subagents,
        } => search::cmd_index(semantic, rebuild, subagents),
        Cmd::Events {
            id,
            harness,
            kind,
            subagents,
            json,
        } => provenance::cmd_events(&id, harness, kind, subagents, json),
        Cmd::Touched {
            path,
            edits_only,
            json,
        } => provenance::cmd_touched(&path, edits_only, json),
        Cmd::Blame { file, lines, show } => blame::cmd_blame(&file, lines.as_deref(), show),
        Cmd::Share {
            id,
            harness,
            out,
            no_redact,
        } => share::cmd_share(&id, harness, out, no_redact),
        Cmd::Pack {
            task,
            format,
            harness,
            limit,
            out,
            to,
        } => {
            if to.is_some() {
                return usage("`cv pack --to <harness>` was renamed in 0.11.0 — use `--harness <harness>`");
            }
            pack::cmd_pack(&task, &format, harness, limit, out)
        }
        Cmd::Stats { query, json } => browse::cmd_stats(query, json),
        Cmd::Prune {
            id,
            harness,
            min_size,
            keep_last,
            to,
            drop,
            thinking,
            copy_resources,
            no_revive,
            window,
            keep,
            declassify,
            declassify_tokens,
            declassify_tokens_file,
            dry_run,
            json,
            retrieve,
            range,
        } => {
            if retrieve.is_some() {
                return usage(
                    "`cv prune --retrieve <tool_use_id>` was removed in 0.11.0 — use `cv cat <session> <tool_use_id>`",
                );
            }
            if range.is_some() {
                return usage(
                    "`cv prune --range A-B` was renamed in 0.11.0 — use `--keep A..B` (the turn indices to KEEP; \
                     0-based, end-exclusive, `A..` through the last)",
                );
            }
            let keep_range = keep.as_deref().map(util::parse_range).transpose()?;
            let tokens = resolve_declassify_tokens(declassify_tokens, declassify_tokens_file)?;
            if declassify && tokens.is_empty() {
                eprintln!(
                    "cv prune --declassify: no terms supplied (--declassify-tokens / \
                     --declassify-tokens-file) — nothing will be snipped. cv ships no built-in list."
                );
            }
            compose::cmd_prune(
                &id,
                harness,
                min_size,
                keep_last,
                to,
                drop,
                thinking,
                copy_resources,
                !no_revive,
                window,
                keep_range,
                declassify,
                tokens,
                dry_run,
                json,
            )
        }
        Cmd::Config { add_export, rm_export } => config::cmd_config(add_export, rm_export),
        Cmd::Schema { json, commands } => schema::cmd_schema(&build_cli(), json, commands),
        Cmd::Formats { action } => formats::cmd_formats(action),
        Cmd::Recipes => recipes::cmd_recipes(),
        Cmd::Resume { id, harness, launch } => port::cmd_resume(&id, harness, launch),
        Cmd::Tree { id, harness } => view::cmd_tree(&id, harness),
        Cmd::Workflow {
            id,
            run_id,
            harness,
            json,
            script,
            results,
            follow,
            revive,
            revive_all,
        } => workflow::cmd_workflow(&id, run_id, harness, json, script, results, follow, revive, revive_all),
        Cmd::Tools {
            id,
            harness,
            agent,
            tool,
            workflow: wf,
            across,
            timeline,
            json,
        } => workflow::cmd_tools(&id, harness, agent, tool, wf, across, timeline, json),
        Cmd::Compaction {
            id,
            harness,
            summaries,
            json,
        } => workflow::cmd_compaction(&id, harness, summaries, json),
        Cmd::Doctor {
            id,
            harness,
            recent,
            json,
        } => doctor::cmd_doctor(id, harness, recent, json),
        Cmd::Board { action } => live::cmd_board(action),
        Cmd::Task { action } => task::cmd_task(action),
        Cmd::Timeline {
            harness,
            cwd,
            query,
            limit,
            json,
        } => browse::cmd_timeline(harness, cwd, query, limit, json),
        Cmd::Diff { a, b, harness } => view::cmd_diff(&a, &b, harness),
        Cmd::Splice {
            specs,
            harness,
            out,
            export,
            cwd,
            generate,
            gen_model,
            to,
        } => {
            if to.is_some() {
                return usage("`cv splice --to <harness>` was renamed in 0.11.0 — use `--harness <harness>`");
            }
            compose::cmd_splice(&specs, harness, out, export, cwd, generate, gen_model)
        }
        Cmd::Redact {
            id,
            harness,
            format,
            stats,
        } => view::cmd_redact(&id, harness, &format, stats),
        Cmd::Loom {
            base,
            at,
            graft,
            from,
            harness,
            out,
            export,
            cwd,
            generate,
            gen_model,
            to,
        } => {
            if to.is_some() {
                return usage("`cv loom --to <harness>` was renamed in 0.11.0 — use `--harness <harness>`");
            }
            compose::cmd_loom(&base, at, &graft, from, harness, out, export, cwd, generate, gen_model)
        }
        // Removed commands: one line each, pointing at the replacement; exit 2.
        Cmd::Convert { .. } => usage(
            "`cv convert` was removed in 0.11.0 — use `cv port <id> --harness <harness>` \
             (`--cwd <dir>` to rehome, `--out <dir>` for a dry run)",
        ),
        Cmd::Query { .. } => usage("`cv query` was renamed in 0.11.0 — use `cv schema` (`cv schema --json` for the machine schema)"),
        Cmd::Recall { .. } => usage(
            "`cv recall` was removed in 0.11.0 — use `cv search <query>` to find content, or `cv pack <task>` to build context",
        ),
        Cmd::Distill { .. } => {
            usage("`cv distill` was removed in 0.11.0 — use `cv pack <task>` (the one build-context-from-the-corpus verb)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The clap tree is well-formed (debug asserts: conflicting ids, bad defaults, …).
    #[test]
    fn clap_tree_is_valid() {
        build_cli().debug_assert();
    }

    /// Every visible command sits in exactly one group, and every grouped name is a real
    /// command — the table and the enum cannot drift apart.
    #[test]
    fn groups_cover_every_visible_command() {
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
        // Old names are hidden stubs, never visible commands.
        for old in ["convert", "query", "recall", "distill"] {
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
        let help = build_cli().render_long_help().to_string();
        for (group, _) in GROUPS {
            assert!(
                help.contains(&format!("  {group}\n")),
                "group {group:?} missing:\n{help}"
            );
        }
        assert!(help.trim_end().ends_with(RECIPES_FOOTER), "footer:\n{help}");
        assert!(
            !help.contains("convert") && !help.contains("distill"),
            "stubs leak:\n{help}"
        );
        // The flat clap list is gone — commands appear under their group only.
        assert_eq!(help.matches("\n    ls ").count(), 1, "{help}");
    }
}
