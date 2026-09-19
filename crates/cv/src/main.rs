//! `cv` — the clustervision CLI.
//!
//! This file holds only the clap surface (`Cli`/`Cmd`) and the dispatch into the per-command
//! modules under `cmd/`; shared helpers live in `util.rs` and `cv blame` in `blame.rs`.

mod blame;
mod cmd;
mod util;

// Crate-root re-exports for `blame.rs`, which renders conversation windows via the same
// streaming renderer `cv show` uses.
pub(crate) use cmd::view::{show_header, show_message, stream_session_render};
pub(crate) use util::short_id;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cmd::live::BoardCmd;
use cmd::task::TaskCmd;
use cmd::{
    browse, compose, config, convert, doctor, live, pack, provenance, query, search, share, task, view, workflow,
};
use std::path::PathBuf;

/// Parse a `cv prune --range` spec. Same grammar as `cv show --range` (`650-`, `650-900`,
/// `-900`, bare `650` = that single turn) so range syntax is uniform across the CLI.
fn parse_keep_range(s: &str) -> Result<(usize, Option<usize>)> {
    util::parse_msg_range(s)
}

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

/// Footer for `cv --help`. The auto `Commands:` list above stays tight (the common path); every
/// other command is still here, grouped, and still invokable via `cv <command>` / `cv <command>
/// --help`. `recall` is intentionally absent from the visible list — see its steer below.
const MORE_COMMANDS: &str = "\
The list above is the common path. Everything below still works — run `cv <command> --help`.

  Read a session     export · tree · tools · workflow · compaction · doctor · diff
  Find across fleet  timeline · stats
  Build context      distill   (for context, prefer `pack`; `recall` is deprecated for agents)
  Reshape & branch   convert · port · resume · prune · splice · loom · dataset · redact · share
  Live               scry · board
  System             config · query";

/// Version with the embedded build commit (set by build.rs; "unknown" outside git), so the
/// binary in your PATH is checkable against source.
const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CV_BUILD_SHA"), ")");

#[derive(Parser)]
#[command(
    name = "cv",
    version = BUILD_VERSION,
    about = "clustervision — search, read, and port AI agent sessions across harnesses",
    after_help = MORE_COMMANDS
)]
struct Cli {
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
        /// `after:2026-06-01 widget` (implicit AND; see `cv_core::query`). A `model:` term forces a
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
        /// Emit the rows as one JSON array instead of the table — the same rows the table would
        /// show (all filters, sort, and --limit respected), with OpenSession-aligned camelCase
        /// fields. Nothing else goes to stdout, so the output pipes cleanly.
        #[arg(long)]
        json: bool,
        /// (--json only) Enrich each row with transcript-derived `git` (branch/commit/remote, as
        /// `cv show --json` emits it) and `displayTitle` (the title with a first-real-user-text
        /// fallback). Costs one lazy transcript parse per emitted row — O(--limit), not the whole
        /// fleet — so plain `ls --json` stays catalog-cheap; opt in when you need these fields.
        #[arg(long)]
        enrich: bool,
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
        /// order (--harness/--limit/--semantic respected), with camelCase fields and the FULL
        /// session id (the table truncates ids to 8 chars for display). Nothing else goes to
        /// stdout, so the output pipes cleanly.
        #[arg(long)]
        json: bool,
    },
    /// Print a single session (by id or id-prefix).
    Show {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Emit the raw unified IR as JSON instead of a rendered transcript.
        #[arg(long)]
        json: bool,
        /// Only render messages in this 0-based, end-exclusive window: `<start>-<end>`,
        /// `<start>-` (through the last), or `-<end>` (from the first). Messages outside the
        /// window are never resolved — large content stays on disk, so a windowed view of a
        /// huge session reads only the bytes it shows.
        // allow_hyphen_values: the documented `-<end>` form starts with a dash, which clap
        // would otherwise reject as an unknown flag (`cv show id --range -5`).
        #[arg(long, allow_hyphen_values = true)]
        range: Option<String>,
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
        /// the Nth (1-based). Combine with nothing else — it sets the range for you.
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "1")]
        pre_compaction: Option<usize>,
    },
    /// Export a session to markdown, JSON, or self-contained HTML (stdout).
    #[command(hide = true)]
    Export {
        id: String,
        /// Output format: `md` (default), `json`, or `html`.
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Export the corpus as a fine-tuning dataset (JSONL, one session per line).
    /// `chatml`/`sharegpt` import directly into Unsloth Studio / TRL / HF — no adapter.
    #[command(hide = true)]
    Dataset {
        /// `chatml` (default) → {"messages":[…]}, or `sharegpt` → {"conversations":[…]}.
        #[arg(long, default_value = "chatml")]
        format: String,
        /// Only this harness (claude, codex, hermes, …); omit for all.
        #[arg(long)]
        harness: Option<String>,
        /// Query calculus selecting which sessions to include, e.g. `model:fable`,
        /// `harness:claude msgs>=20`, `after:2026-01-01 cwd:/pug` (implicit AND; see `cv_core::query`).
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
    /// Convert a session into another harness's native format (cross-harness port).
    #[command(hide = true)]
    Convert {
        id: String,
        /// Target harness (claude, codex, grok, …).
        #[arg(long)]
        to: String,
        /// Source harness hint (otherwise auto-detected by id).
        #[arg(long)]
        from: Option<String>,
        /// Write under this directory instead of the target's real storage root (safe dry run).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rehome the converted session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Rehome a session to a different working directory (and optionally another harness).
    #[command(hide = true)]
    Port {
        id: String,
        /// Target harness (defaults to the source harness).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        from: Option<String>,
        /// New working directory for the ported session.
        #[arg(long = "to-dir")]
        to_dir: Option<PathBuf>,
        /// Write under this directory instead of the target's real storage root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Don't copy project context files (CLAUDE.md, MEMORY.md, AGENTS.md, …) to the new cwd.
        #[arg(long = "no-context")]
        no_context: bool,
    },
    /// Follow live agent activity across harnesses (tail -f for sessions).
    #[command(hide = true)]
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
    },
    /// Redact a session and emit a single self-contained HTML artifact anyone can open.
    #[command(hide = true)]
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
    /// Compile a context bundle for a new task from your whole session corpus.
    Pack {
        /// What you're about to work on — recall finds the relevant past spans.
        task: String,
        /// Output shape: `md` (CLAUDE.md-style digest, default), `prompt` (system prompt),
        /// or `session` (a synthetic resumable session — see `--to`).
        #[arg(long, default_value = "md")]
        format: String,
        /// Target harness when `--format session`.
        #[arg(long)]
        to: Option<String>,
        /// Max past sessions to draw from.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// Fleet analytics over all discovered sessions.
    #[command(hide = true)]
    Stats {
        /// Restrict the analytics to sessions matching this query (see `cv query`).
        #[arg(long, short = 'q')]
        query: Option<String>,
    },
    /// Custom, lossless compaction of a Claude session into a NEW resumable session: snip bulky old
    /// tool payloads (large reads/logs/screenshots) into a sidecar, leaving a small `[PRUNED]` marker
    /// — your prompts and flow stay verbatim, the recent turns stay sharp. Resume with
    /// `claude --resume <new-id>`. Use `--retrieve <tool_use_id>` to fetch a stashed original back.
    #[command(hide = true)]
    Prune {
        /// Source session id (or, with `--retrieve`, the *pruned* session to read from).
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Fetch a stashed original by its `tool_use_id` from `<id>`'s sidecar (prints it; no prune).
        #[arg(long, value_name = "TOOL_USE_ID")]
        retrieve: Option<String>,
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
        /// Keep an explicit message subrange instead: `START-END` turn indices (END optional —
        /// `650-`, `650-900`, or `-900`), dropping everything outside it. Like --window but by index.
        #[arg(long, value_name = "RANGE")]
        range: Option<String>,
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
        /// Also emit the prune report as ONE JSON object on stdout (camelCase fields, FULL ids).
        /// The human report stays on stderr, so stdout carries only the JSON. Dry-run honest:
        /// nothing was written, so `newPath`/`sidecarPath` are null and `newId` is null unless
        /// `--to` pinned it (see `note`). Ignored by `--retrieve`, which prints the payload itself.
        #[arg(long)]
        json: bool,
    },
    /// View the user config (`~/.config/clustervision/config.toml`) and manage the export-source index.
    /// With no flag, prints the config path + registered export sources. Account data exports
    /// (ChatGPT/Claude.ai `conversations.json`) have no fixed home, so register the dirs/files you want
    /// the `chatgpt-export`/`claude-export` harnesses to discover.
    #[command(hide = true)]
    Config {
        /// Register an export source (a dir to scan, or a `conversations.json` file).
        #[arg(long, value_name = "PATH")]
        add_export: Option<PathBuf>,
        /// Unregister a previously-added export source.
        #[arg(long, value_name = "PATH")]
        rm_export: Option<PathBuf>,
    },
    /// The query-calculus reference: every field, operator, and example. `--json` emits the machine
    /// schema. The `-q` flag on ls/dataset/timeline/stats speaks this language.
    #[command(hide = true)]
    Query {
        /// Emit the machine-readable schema (fields, types, operators) as JSON instead of the
        /// human reference.
        #[arg(long)]
        json: bool,
    },
    /// Print (or with --launch, run) the resume incantation for a session in its native harness.
    #[command(hide = true)]
    Resume {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Actually spawn the harness (cd to the session's cwd) instead of just printing.
        #[arg(long)]
        launch: bool,
    },
    /// Render a session's message threading (DAG if parent_ids exist, else a numbered list).
    #[command(hide = true)]
    Tree {
        id: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// A `Workflow`-tool run, first-class: its phase tree, the agents under each phase with their
    /// outcomes, run totals, and the driving script. Without `<run_id>`, lists the session's runs.
    #[command(hide = true)]
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
    /// Cross-agent tool analytics: per-agent histograms, which-agent-used-what, aggregate usage,
    /// and a tool-call timeline — across the orchestrator and its whole sub-agent forest.
    #[command(hide = true)]
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
    /// Compaction boundaries in a session: every `/compact` (or auto-compaction), its trigger,
    /// pre-compaction context size, and the summary that seeded the next window. `--summaries`
    /// prints each full summary; `cv show <id> --pre-compaction` reads the lost pre-span.
    #[command(hide = true)]
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
    /// Diagnose why a session's context window keeps filling and compacting: attribute context
    /// pressure by source (tool results split MCP/builtin, thinking, messages), size the fixed
    /// system+tools overhead from token usage, and pair it with compaction frequency. With no
    /// <id>, analyzes the most recent session(s) for the current directory.
    #[command(hide = true)]
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
    /// Post to / read from the agent coordination board.
    #[command(hide = true)]
    Board {
        #[command(subcommand)]
        action: BoardCmd,
    },
    /// Fleet task substrate: dispatch, review, and git-observed landing.
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Unified chronological feed across all harnesses (oldest → newest).
    #[command(hide = true)]
    Timeline {
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Restrict the feed to sessions matching this query (see `cv query`).
        #[arg(long, short = 'q')]
        query: Option<String>,
        #[arg(long, default_value_t = 60)]
        limit: usize,
    },
    /// Compare two sessions message-by-message (great for loom branches).
    #[command(hide = true)]
    Diff {
        a: String,
        b: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Compose a new session from spans of existing ones (`<id>:<start>-<end>`).
    #[command(hide = true)]
    Splice {
        /// One or more specs: `<id>:<start>-<end>`, `<id>:<start>-`, or `<id>` (whole session).
        #[arg(required = true)]
        specs: Vec<String>,
        /// Target harness for the composed session (defaults to the first spec's harness).
        #[arg(long)]
        to: Option<String>,
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
    },
    /// Distill a session into durable memory (decisions, gotchas, where things live) via an LLM.
    #[command(hide = true)]
    Distill {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Model id (provider-specific; defaults to the provider's cheap/fast model).
        #[arg(long)]
        model: Option<String>,
        /// Frame the distillation as whole-project memory (may span more than one session).
        #[arg(long)]
        project: bool,
        /// Write the digest to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// With --out, append under a dated header instead of overwriting (build a MEMORY.md).
        #[arg(long)]
        append: bool,
    },
    /// DEPRECATED for agents: returns raw matching spans, not answers — agents reach for this
    /// expecting a synthesized result it can't give. Use `cv search` to find content, or `cv pack`
    /// to assemble task context. (Still functions: top-K semantic spans for a query.)
    #[command(hide = true)]
    Recall {
        query: String,
        #[arg(short = 'k', default_value_t = 5)]
        k: usize,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Scrub secrets/PII from a session and export it (safe to share).
    #[command(hide = true)]
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
    /// Loom graft: take base[..N], then graft other[M..] into one new branched session.
    #[command(hide = true)]
    Loom {
        base: String,
        #[arg(long)]
        at: usize,
        #[arg(long)]
        graft: String,
        #[arg(long)]
        from: usize,
        /// Target harness for the grafted session (defaults to the base's harness).
        #[arg(long)]
        to: Option<String>,
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
    },
}

fn main() -> Result<()> {
    // Die quietly on a closed pipe (`cv task list | head`) instead of panicking:
    // Rust ignores SIGPIPE by default, which turns EPIPE into a write panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
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
            range,
            subagents,
            agent,
            pre_compaction,
        } => view::cmd_show(&id, harness, json, range, subagents, agent, pre_compaction),
        Cmd::Export { id, format, harness } => view::cmd_export(&id, &format, harness),
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
        Cmd::Convert { id, to, from, out, cwd } => convert::cmd_convert(&id, &to, from, out, cwd),
        Cmd::Port {
            id,
            to,
            from,
            to_dir,
            out,
            no_context,
        } => convert::cmd_port(&id, to, from, to_dir, out, no_context),
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
        } => provenance::cmd_events(&id, harness, kind, subagents),
        Cmd::Touched { path, edits_only } => provenance::cmd_touched(&path, edits_only),
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
            to,
            limit,
            out,
        } => pack::cmd_pack(&task, &format, to, limit, out),
        Cmd::Stats { query } => browse::cmd_stats(query),
        Cmd::Prune {
            id,
            harness,
            retrieve,
            min_size,
            keep_last,
            to,
            drop,
            thinking,
            copy_resources,
            no_revive,
            window,
            range,
            declassify,
            declassify_tokens,
            declassify_tokens_file,
            dry_run,
            json,
        } => {
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
                retrieve,
                min_size,
                keep_last,
                to,
                drop,
                thinking,
                copy_resources,
                !no_revive,
                window,
                range.map(|s| parse_keep_range(&s)).transpose()?,
                declassify,
                tokens,
                dry_run,
                json,
            )
        }
        Cmd::Config { add_export, rm_export } => config::cmd_config(add_export, rm_export),
        Cmd::Query { json } => query::cmd_query(json),
        Cmd::Resume { id, harness, launch } => convert::cmd_resume(&id, harness, launch),
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
        } => browse::cmd_timeline(harness, cwd, query, limit),
        Cmd::Diff { a, b, harness } => view::cmd_diff(&a, &b, harness),
        Cmd::Splice {
            specs,
            to,
            out,
            export,
            cwd,
            generate,
            gen_model,
        } => compose::cmd_splice(&specs, to, out, export, cwd, generate, gen_model),
        Cmd::Distill {
            id,
            harness,
            model,
            project,
            out,
            append,
        } => compose::cmd_distill(&id, harness, model, project, out, append),
        Cmd::Recall { query, k, harness } => search::cmd_recall(&query, k, harness),
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
            to,
            out,
            export,
            cwd,
            generate,
            gen_model,
        } => compose::cmd_loom(&base, at, &graft, from, to, out, export, cwd, generate, gen_model),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_declassify_tokens;

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
}
