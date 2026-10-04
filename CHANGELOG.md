# Changelog

## Unreleased

- **`cv rewind`** — an agent as of a past moment, resumable. `cv rewind <id> --at <MSG_IDX|SHA>`
  copies the source's raw records from the last compaction before the cut (moved back to the
  boundary's preserved segment; `--full` from the top) through the cut into a new session id,
  `parentUuid` chains intact, plus a `<new-id>.rewind.json` provenance sidecar (source bytes +
  sha256, cut, start, omitted tail, cv version). A sha resolves to the tool result whose `git
  commit`/`git push` output names it; open parallel calls at the cut run on to their results.
  Sub-agent transcripts (`agent-<id>`, `--agent`, or a path) come out as standalone top-level
  sessions in the parent's project dir. The resume line uses the launch dir Claude files the
  session under. Streams: a 0.9 GB session rewinds in ~13 s at ~37 MB RSS.
- **`cv blame` exact evidence** — a candidate session whose own tool output shows the commit
  being made (or pushed) ranks above every time-window match as `exact: commit created here`,
  with a `cv rewind <id> --at <sha>` hint. Read from the transcripts (byte prefilter, then the
  stream), so it needs no fresh index; sub-agent edit rows now bring their parent session along
  as a candidate.
- **`cv splice` of a sub-agent span** joins the new main thread: the `isSidechain`/`agentId`
  markers that made Claude skip the emitted records are dropped.
- **Seekable real Claude transcripts** — rendered-attachment messages were never stamped with
  their byte offset under `lazy_offsets`, so any session with one (current Claude Code writes
  them constantly) recorded no seek row and `cv show --range` fell back to a full stream.
- `cv_core::digest::Sha256` — the vendored SHA-256, now incremental.
- **`cv distill` — a transcript as a reshapeable lane context** (`docs/design/DISTILL.md`). It
  compresses a long session, typically a sub-agent lane at 500K–900K tokens, into a pack. The pack
  holds verbatim: the brief, every message to the agent, everything the agent said, sent or wrote
  down, and the documents its instructions named that it read. It also holds an index of commits,
  branch tips, repository heads, hosts, paths, build verdicts and tool errors, a one-line ledger
  per tool call, and the last N tool calls as real turns. It is deterministic, with no model calls.
  Elided outputs go to a prune-format sidecar that `cv cat <new> <tool_use_id>` reads. Three
  emissions: the markdown pack (stdout or `--pack`); `--session`, a new resumable session
  (`claude -p --resume <id>`); and `--agent-of <root>`, a new sub-agent transcript in the root's
  `subagents/`, which the root resumes with `SendMessage` to the printed id. That last route was
  measured to work for a transcript the root never spawned, even one planted mid-run. `--upto N`
  distills the session as of message N; `--with <other>` injects another lane's findings.
  Measured on three real lanes, the distilled context loads 5.4–6.7× fewer tokens than the source
  and answers 28 of 30 probe questions, against 29.5 for the full transcript.
- **`cv fork <id> --at N (--session | --agent-of <root>)`** branches a session's verbatim context at
  message N, to run variants from one point.
- **The name `distill` returns with a new meaning.** The 0.11 stub that pointed `cv distill` at
  `cv pack` is gone. `pack` still builds context from the corpus; `distill` reshapes one session.
- **Reshaped sessions thread linearly.** Claude Code resumes by walking `parentUuid` back from the
  newest record and silently stops at the first missing link. A lane's turns often point at hook
  attachments the IR does not carry, so a session emitted with source parents resumed with only
  its last record or two. `distill` and `fork` clear parents and let the emitter chain records in
  order. (`port`/`splice`/`loom` keep source parents and may share this; not yet checked.)

- **Devin CLI (Cognition) parses** — `~/.local/share/devin/cli/sessions.db` (refinery schema 17, cli
  3000.11.3), read-only and PRAGMA-probed like the other SQLite stores. `message_nodes` is a
  *forest* — every context rebuild rewrites the transcript as a fresh chain of copies — so the
  parse is the `main_chain_id`→root walk (NULL falls back to the max `node_id`, flagged
  `extra["devin"]["main_chain_fallback"]`), and superseded branches are dropped with
  `node_count`/`chain_len` kept in `extra["devin"]`. `is_system_prefix` nodes become
  `Session::system_prompt`; thinking blocks (with `sealed.v1` signatures), parallel tool calls,
  per-call metrics → `Usage`, and `generation_model` (which also rescues the session model when
  `sessions.model` is `''`) all survive. `cv resume` prints `devin --resume <id>`.
  `subagent_heads` rows are **child sessions** — `<session_id>/<agent_id>`
  (`atom-telephone/sidekick`) walking the same forest from `chain_node_id`, with
  `lineage.parent`/`agent_path`, `[<agent_id>]` titles, the lead's handoff prompt as
  `Prompt`/`Subagent` (`lead_model` from `chisel/fusion_lead_model_uid`), and `rules_loaded`/
  `skills_loaded` extras; dangling heads are skipped and counted
  (`dangling_subagent_heads`). `cv lanes`/`cv tools`/`cv show` all see them.
  Compaction pairs (`summarized_from` = old head, an assistant summary + a system wrapper
  pointing at `summaries/<agent>/history_*.md`) synthesize a `CompactionBoundary` carrying
  `source`/`history_path`/`edited_files`/`todo_list`, so `cv compaction` detects Devin
  compactions; `chisel/tool_failure` marks failed tool results,
  `chisel/user_question_answers` surfaces in `cv prompts`, and the parent's
  `subagent/*` completion rows fill `extra["devin"]["subagent"]` +
  `lineage.spawned_by_tool_use`.

## 0.13.0 — decisions are a kind; a human can drain the inbox

Written from the first day a human was on the other end of `cv task`: an orchestrator had filed
~15 "DECIDE (…, default stands if silent): …" items as free-text notes on one assigned task, and
the person asked "how do I access this inbox?" and "is there a web interface where I can see these
and drain them?".

- **Decisions.** Two new event kinds, `posed {options, default, deadline?, source?}` and
  `resolved {choice, note?}`, and a terminal `TaskState::Resolved`. A task with a `posed` event is
  of kind `decision`: `done` is refused on it ("answer it with resolve"), a second `posed` is
  refused (amend by posing anew), the choice must be one of the posed options, and `resolved` is
  identity-bearing — who decided is the fact, so it is never a shared sink. `cv task decide --for
  <who> --default … [--option …]… [--by <when>]` poses one (`opened` + `tagged decision` +
  `posed`); `cv task resolve <id> --accept-default | --choice "…" [--note …]` answers it; with no
  identity the error prints the exact command with the decision's owner filled in.
- **`cv task split <id>`** turns every leading-`DECIDE` note on a task into its own decision:
  title = the parenthetical label + the question's first clause (parentheticals dropped), default
  = the `Default =` / `Default if silent:` / `Recommend:` clause (several are joined; none means
  "as proposed"), options = the `alternative =` clauses; each is assigned like the parent, carries
  the note as its body and `source`, and blocks the parent. The notes stay; `show` points each at
  its decision; a second run creates nothing; mid-text `DECIDE`s are counted, not split.
- **`cv task inbox <who>`** now leads with decisions, each with `⇒ default: … · alt: … · by …` on
  the next line, then assigned actions, claimed work, reviews, unlanded. `--md` renders the whole
  inbox as a Markdown page (every body, the first line of every note, a resolve command per
  decision). `--unread` keeps items whose last event is not `<who>`'s (`web:<who>` counts as
  `<who>`). `--json` carries the decision facet.
- **`cv task serve [--bind 127.0.0.1:7777] [--assignee <who>] [--open]`**: a web inbox served by
  cv itself — one inline page, no external JS/CSS, theme-aware, phone-width — with the decisions'
  options as buttons (Accept default / each alternative / Needs discussion), Done/Claim/Release on
  actions, notes with a Save box, Open/Resolved/All filters and a counter line. Every button
  records the same event the CLI would, as `web:<who>`, through `/api/task/<id>/{resolve,done,
  note,discuss,claim,release,reopen}`; `/api/inbox?who=` and `/api/events?since=` are the CLI's
  own queries. Loopback only unless `--bind 0.0.0.0:…` is passed; a DNS-named `Host` is refused
  (rebinding), an IP literal is accepted (a phone on the LAN). Uses `tiny_http`, already in the
  workspace through cvd. "Reopen" on a closed item opens a NEW task (terminal stays terminal).
- **The feed.** `cv task events [--since <when|event-id>] [--kind resolved,done,noted] [--by]
  [--not-by] [--assignee] [--task]` prints one JSON line per event (the event's fields plus
  `title`, `task_state`, `assignee`) and the next cursor on stderr; `--text` for people. `cv task
  watch --assignee <who> --since …` is that minus the caller's own events. No push channel: poll.
- **Scope.** Bare `cv task list` / `inbox` show tasks touched in the last 14 days or involving
  `$CV_ENDPOINT` (opener, assignee, note author, reviewer, resolver), and print the hidden count
  on the last line; `--all` lifts it, `--since 90d` widens it. MCP and HTTP still see everything.
- **`cv task show`**: `--brief` (one line per note, the body's first line), `--notes-last N`,
  `--notes-grep PATTERN`; a decision block (default, options, resolution or the resolve command).
- **stdin.** `cv task note <id> -`, `--body -`, `--note -` read stdin (zsh eats backticks inside
  double quotes; a heredoc does not).
- **Paths.** `--issue` is absolutized at open time (cwd, else the task's `--repo`, else cwd
  lexically); handles and URLs pass through. `--repo` was already canonicalized.
- **`cv lanes`**: `--since 2h` keeps lanes active in the window; a lane with no stop recorded
  anywhere whose transcript ends in a text-only final turn and has been quiet ≥ 10 min reads
  `returned` (status source `transcript`) — a lost notification — and counts as done; a running
  lane quiet for an hour or more says so (`⚠ quiet 14h`).
- Board notifications now carry the task's title, so `cv board read tasks` reads as a timeline.
- Golden fixture: additive only (a decision specimen task; every prior task serializes
  byte-identically).

## 0.12.0 — the orchestrator's instruments

Three commands for a session that is running a swarm, written from a day of running one with
twenty lanes and doing each of these by hand (a thirty-line Python over `cv show --json`, three
times). All three are in the Read group, take `harness:id` or a prefix, have `--json`, and reach
MCP through the generated tool list like every other command.

- **`cv prompts <session>`** prints only what the person said: every human-typed prompt and every
  `AskUserQuestion` answer, in order, with message indices and timestamps. `--pre-compaction [N]`
  narrows to the span the Nth compaction discarded, the same window `cv show --pre-compaction`
  reads. Prompts are printed whole. On the session this was built against, 38 lines and 6 answers
  out of 10,297 records.
- **`cv lanes <session>`** is the sub-agent forest as a status table: one row per lane with its
  model, start, duration, tokens (deduplicated by API `message.id`, as `cv stats --tokens`
  counts), tool calls, status and either the last line of its return or — for a running lane —
  its last tool call. `--running` / `--done` / `--stranded` filter. **Stranded** is the class that
  parked four lanes in one day: the harness reports the lane *completed* and its final text says
  it is waiting (`Waiting on notifications`, `I'll continue when the monitor fires`); nothing will
  wake it. Each stranded row carries `→ resume: SendMessage to <agentId>`, and a stranded lane
  never counts as done. Status comes from the most authoritative source and the JSON names it:
  a `Workflow` journal, else the parent's last `<task-notification>` for the agent, else the
  child's `SubagentStop` hook, else `running`.
- **`cv deferrals <session>`** is a linter for promises: every place the assistant put something
  off ("later lane", "follow-up", "not tonight", "queued for", "ember's call", "when X lands",
  "after FINAL", …) with message index and context. `--open-tasks` cross-references every task in
  the store (three or more shared significant words is a MATCH; the words are printed so the
  match can be judged) and **exits 1 while any deferral is UNMATCHED**, so a closeout can gate on
  it. `--since <msg>` restricts to the post-compaction region.

### `cv task` ergonomics

- `open --body-file <path>` and `note --file <path>` (`-` for stdin). Bodies were 500-character
  shell strings.
- `open --tags a,b`, `cv task tag <id> a,b`, `list --tag <t>`. The `decision` tag on an assigned
  task is a decision owed, and `inbox` lists it first.
- `open --blocked-by <id>` / `open --blocks <id>` and `cv task block <id> --by <id>`: relations
  stored as `blocked_by` events on the blocked task. Blocked-ness is computed at read time from the
  blocker's live state — a blocker finishing or being abandoned unblocks with no further event.
  `list` marks blocked rows `⊘`; `show` prints `blocked by:` and `blocks:`. Relations are resolved
  before the open is written, so a typo refuses the command instead of opening a half-related task.
- `inbox <who>` groups: decisions owed · claimed · reviews · unlanded · assigned, unclaimed.
- `list --tsv` (full id, state, assignee, repo basename, age, title, blocked_by) and `list --wide`
  (a second line with tags, repo, blockers, the body's first line).
- **`cv task sweep --repo <path>`** lists the tasks git and the filesystem say are probably done —
  an `--issue` path that no longer exists, a title/body naming a branch now merged into main, a
  proposed revision whose branch is merged — and never closes one.
- **Task ids no longer collide in every list.** UUID v7 ids opened within one second share their
  first eight hex digits; `list`/`inbox`/`debt` printed 8-char prefixes, so a batch of 23 tasks
  rendered as 23 copies of `01a0f52e` and `show` refused every one. Every list now renders the
  shortest prefix that keeps the ids distinct.

### Wire

- Two new task event kinds, `tagged { tags }` and `blocked_by { task }`, and two projection
  fields, `tags` and `blocked_by`, on `TaskProjection` and `TaskRow` (every surface). Both are
  omitted when empty, so a log that never used them serializes byte-for-byte as before; the golden
  fixture gained specimens of both (`GOLDEN_REGEN=1`, snapshot diff reviewed: only the specimen
  task changed). `TaskFilter` gained `tag`; `task_list` (MCP) and `GET /api/tasks` (cvd) accept it.
  `InboxReason` gained `decision_owed`.
- **Claude adapter:** Claude Code ≥ 2.1 writes a slash command's bookkeeping (`<command-name>…`,
  `<local-command-stdout>…`) as `user` records; they now classify as `notice`/`harness`, as the
  older `system`/`local_command` records always did. The typed `/compact` line stays a human
  prompt. Text is untouched.
- `cv schema --json` publishes the `prompt_row`, `lane` and `deferral` shapes. `cv recipes` has
  three more entries.

### Build

- `make install` (= `cargo install --path crates/cv --force --target-dir target`, plus `cv-mcp`
  and `cvd`) and `make version`, so the installed binary and the checkout can be compared in one
  line. The installed `cv` on the machine this was written on was two releases behind the
  checkout; `cv --version` had said so all along.
- The clap-tree unit tests run on a 16 MB thread: `Cli::command()` for the grown enum overflows
  the default 2 MB test-thread stack in a debug build (the binary's main thread is fine).

## 0.11.2 — `cv stats --tokens`

- **`cv stats --tokens`** totals token usage per harness and model over any query slice:
  uncached input, cache writes, cache reads, output, reasoning, provider cost. It widens each Claude
  session to its sub-agent and `Workflow`-agent forest, which the catalog does not list and which
  on an orchestrating session holds most of the spend. It counts a Claude response once even though
  Claude Code writes one line per content block and copies history into resumed sessions (deduped
  by API `message.id`). `--json` adds a `tokens` object; without the flag the payload is unchanged.
- **`Usage` has one meaning across harnesses.** `input_tokens` is now the *uncached* prompt
  everywhere (Anthropic's convention), with `Usage::prompt_tokens()` / `total_tokens()` for the
  sums. Codex and Gemini report cached tokens as a subset of input; their adapters now subtract on
  parse and their emitters add back on emit. Before, a Codex → Claude port wrote cached tokens
  twice into the Claude `usage` block, and `cv doctor` sized a Codex session's peak context at
  roughly double. **Library consumers reading a Codex or Gemini `Usage.input_tokens` see a smaller
  number now; add `cache_read_tokens` for the old one.**
- **Codex: a re-emitted `token_count` no longer counts as a second call.** Codex repeats its last
  snapshot (same running `total_token_usage`), for example after a user message. The adapter paired
  only the first copy with its `token_usage_record`, so each repeat attached stale usage to the next
  reply or to a synthetic carrier. On one 7,700-snapshot rollout that inflated usage from 834.5M to
  1.06B; after the fix cv matches the rollout's own `thread_token_usage` to the token.

## 0.11.1 — packaging fix

`clustervision-core` 0.11.0 could not be published to crates.io. `formats.rs` embeds the 22 format
manifests with `include_str!`, reaching from `crates/cv-core/src/` up to a `formats/` directory at
the workspace root — and `cargo package` only puts files from *inside* the crate into the tarball,
so the packaged crate had no manifests and would not compile. The manifests are cv-core's data, and
`cv formats census` needs them on a machine with no checkout, so they now live in
`crates/cv-core/formats/`.

Nothing about the binaries changes; v0.11.0's release assets are unaffected. This version exists so
that the tag, the release binaries and the published crate are all the same source.

Worth recording: nothing caught this for a whole release, and the only reason CI did not discover
it halfway through a six-crate publish is that `CARGO_REGISTRY_TOKEN` was never set. `cargo package
-p <crate>` belongs in the pre-tag checklist, since it is the only thing that builds a crate from
the tarball it would actually ship rather than from the working tree.

## 0.11.0 — the clean break

This release renames, regroups and restructures on purpose, with **no aliases for old names**
(an old command or flag errors with a pointer to the new one). The contract is
`docs/INTERFACE-V2.md`. Principle: one name, one meaning, no grammar to memorize.

### Breaking changes

- **Commands.** `convert` is folded into `port` (`port <id> --harness <h> [--cwd <dir>] [--out <dir>]`);
  `query` is `schema`; `recall` and `distill` are gone (`pack` is the one "build context from the
  corpus" verb); `prune --retrieve` is `cat <session> <tool_use_id>`; `prune --range` is
  `prune --keep A..B`; `prune --thinking` is `prune --drop-thinking`, because `--thinking` now
  names what an *emit* does with reasoning and one word may not mean two things. Every command is
  visible in `cv --help`, grouped Read / Reshape / Export / Fleet & live / System; `cv recipes` is
  the agent quickstart.
- **Windows.** `--first N`, `--last N`, `--range A..B` (0-based, end-exclusive; `A..`, `..B`),
  `--around N [--context K]`, `--max-bytes N` — the same five on `show`, `export` and the MCP `show`.
  The old `--range -N` (which meant the FIRST N) is gone.
- **JSON is snake_case everywhere.** `ls --json`: `message_count`, `created_at`, `updated_at`,
  `size_bytes`, `display_title`; `search`/`timeline`/`workflow --json` likewise; every session row
  has the same keys. Blocks in `show --json` are tagged `type` (was `kind`); a message has a `kind`.
- **The IR.** `Message` gains `kind` (prompt · reply · tool_result · injected_context · system_prompt
  · notice · compaction_boundary · compaction_summary · model_change · error · subagent_spawn ·
  subagent_return · branch · carrier) and `origin` (human · model · harness · hook · scheduler ·
  subagent · import); `Session` gains `system_prompt` and `lineage` (forked_from, parent,
  spawned_by_tool_use, continued_in, continues, agent_path); `Block::ToolUse` gains `namespace`;
  `Usage` gains `reasoning_tokens` and `cost_usd`. Harness-specific facts live under
  `extra["<harness>"]` (one bag per harness) — never as flat keys. Facts cv produces itself rather
  than reading from a store (parse diagnostics, and the provenance stamped on a session that `loom`
  or `splice` synthesized) live under `extra["cv"]`, so a session's `extra` has no flat keys at
  all.
- **MCP.** Tools are generated from the CLI (`cv schema --commands --json`): same names, flags and
  output as the commands; `show` defaults to the last 50 messages / 200 KB over MCP.
  `read_session`, `search_sessions`, `list_sessions`, `project_sessions`, `recall`,
  `prune_session`, `prune_retrieve` are gone (`show`, `search`, `ls`, `ls --cwd`, `pack`, `prune`,
  `cat`).
- **Downstream consumers** of `cv ls --json` / `cv show --json` must switch to the snake_case keys
  and the `type` block tag. Ours (`sesh`) is updated separately, after this release.

### New

- **`cv cat <session> <tool_use_id>`** prints one tool call's full output wherever it lives:
  inline in the transcript, in a `prune` sidecar, or in a persisted-output file. `--input` prints
  the call's arguments instead. This replaces `prune --retrieve`, which could only reach sidecars.
- **`cv recipes`** is the agent quickstart: the ten things an agent does with cv, each as one
  command line plus the JSON keys it returns. `cv --help` points at it.
- **`cv schema --commands --json`** dumps the whole command tree, every command and flag with its
  help text. The MCP server builds its tool list from it, so the two cannot drift.
- **`cv formats census` and `cv formats check`** audit cv against the harnesses. `census` parses
  recent real sessions in format-complete mode and reports the record vocabulary each adapter did
  NOT interpret, so a harness changing its format shows up on real data the day it lands. `check`
  compares every adapter's match arms against a pinned manifest in `formats/<harness>.toml`, which
  names the upstream commit it was verified against. `tools/harness-drift.sh` re-checks the
  manifests against the upstream checkouts, and `tools/harness-fixtures/` regenerates each
  real-writer fixture. All of it is now run rather than merely shipped, which is how we learned
  that the Codex smoke check had been proving nothing (it exited before the model call) and that
  `OPENAI_BASE_URL` does not redirect Codex 0.155.1, so the "dead endpoint" it relied on was
  letting a real request leave the machine. Containment is a provider override now, the drift
  script is read-only by default, and every generator writes where you tell it instead of over the
  committed fixture.
- **`cv port --strict`** fails the port when the fidelity check finds a loss the target format
  could have carried. Losses the format inherently cannot hold are still only reported.
- **`--thinking <native|text|drop>`**, on every command that writes a session out (`port`,
  `splice`, `loom`, `pack --format session`). Thinking blocks come in two shapes: a few carry
  plaintext, most carry only an opaque provider signature with no text at all (on one real session,
  177 of 204). Nothing but the store that produced it can hold that blob, so the only question a
  mode can answer is whether the *turn* survives. `native`, the default, keeps structured reasoning
  where the target holds it and drops it elsewhere, which can quietly cost a third of the assistant
  turns. `text` never loses a turn: reasoning with text becomes text, a signed blob becomes a short
  placeholder, and the lost signature is still reported so a lossy port does not start looking
  clean. `drop` omits reasoning entirely, for handing someone a session without your chain of
  thought.

### Fidelity: conversions now carry what they always should have

The round-trip verifier used to compare six counts, so it reported "clean" on paths that were
dropping most of the session. A per-field diff of every conversion path found that nearly all of
them silently lost every usage record, every per-message model, signature-bearing thinking, the
tool name on every result, and every structured result payload; `claude → hermes` also lost the
working directory and `claude → codex` the title. All of that is now carried where the target
format can hold it, and the verifier diffs every field it could have carried, classifying each
loss as expected for that format or unexpected. On a real 900-message session `--strict` now passes
into **all seven** emit targets. Every remaining "inherent" verdict is backed by evidence rather
than assumed: Hermes's schema-30 store has no column for a tool result's error flag and its own
writers take no error argument; Grok records the model it will resume against, so a foreign model
id there is a deliberate rewrite; OpenCode and Gemini fold a tool result into its originating
call's record, so a tool turn never had an id of its own to carry, and Gemini's reader applies
gemini-cli's own rule that a `/`-leading turn is a command typed at the client rather than a
prompt, which re-tags the turn without losing it.

Getting there turned up real bugs that no test had caught, because nothing had compared a
conversion field by field on a real session before: an assistant turn whose only content was
provider-signed reasoning vanished, taking a third of the assistant turns with it; a tool result
whose call had been pruned away was dropped by three emitters and made Codex log an orphan on every
resume; a Gemini tool call that completed with empty output lost its whole turn; and usage never
crossed into Claude from another harness at all. The OpenCode emitter writes OpenCode's SQLite
store, which is what OpenCode 1.18 actually reads; it had been writing the superseded JSON tree.

### The release pipeline, the desktop app and the web UI

Three things had been quietly broken for a release or more, all of them invisible because nothing
in CI looked at them. They are fixed, and CI looks at them now.

- **Tagging produced nothing.** `dist-workspace.toml` said `publish-jobs = ["cargo"]`, which
  cargo-dist rejects while parsing the file — it has no built-in crates.io job, only `homebrew`,
  `npm` and custom `./name` jobs. The `plan` step therefore died before a single artifact was
  built, so every tag since just after v0.9.22 shipped neither binaries nor a crates.io upload, and
  the failure read like a TOML error rather than a missing feature. There is now a real
  `./publish-crates` job that publishes the six library crates in dependency order and tolerates a
  re-run. Verified by running `dist plan` and a full local `dist build` for the host target, which
  produce the seven-target manifest and a working packaged binary.
- **The desktop app had not compiled since the 0.10 crate rename.** It declares its own
  `[workspace]`, so `cargo check --workspace` never saw it, and it asked for a package named
  `cv-core` after that package became `clustervision-core`. Behind that one-line break sat five
  semantic bugs: its session rows were not the §3 shape, `local_messages` ignored the `extra`
  argument the UI had always passed (so tool-result `details` could never reach the desktop), the
  sub-agent listing missed the entire workflow tier, and two commands the UI has invoked since
  0.9.12 did not exist and silently returned null. A new `local_session_head` exposes a session's
  `system_prompt` and `lineage`, which is otherwise unreachable for Claude.
- **The web UI read the pre-0.11.0 block tag,** so every transcript, diff and forest screen was
  broken. It reads `type` now, and uses the new vocabulary rather than painting every
  non-assistant turn the same: a prompt, harness-injected context, a notice, an error and a
  compaction boundary are visually distinct, an unknown kind renders honestly instead of
  vanishing, and lineage is navigable. `web/selftest.html` carries 37 rendering invariants, run in
  a real browser, so the next IR change fails loudly.
- **The daemon's rows now match everyone else's.** `/api/sessions` was missing `path` and
  `size_bytes`, and `/api/touched` dropped the `agent_id`/`parent_id`/`workflow` trio it had always
  carried, so a consumer could tell which door a row came through. Both are pinned by tests.
- **`GET /api/session/<harness>/<id>/head`** answers what a session knows about *itself* — the
  session row plus `model`, `git`, `system_prompt`, `lineage` and the exact message `total` — with
  no messages. A windowed read structurally cannot: it stops once its window is full, and Claude
  writes its system prompt well after the opening turns, so opening a transcript at the top could
  never learn it. The desktop app's `local_session_head` is the same thing through the other door.
- **A missing static asset is a 404 again.** The dashboard server answered any missing path with
  the single-page index, so a module the browser could not find came back as HTML under a `.js`
  name and was rejected with "Expected a JavaScript-or-Wasm module script" — which reads like a
  server misconfiguration rather than the truth. Routes still fall back; assets do not.

### OpenSession 0.3 — the spec cv can actually speak

cv publishes [`docs/OPENSESSION.md`](docs/OPENSESSION.md), a proposed interchange format for agent
sessions, and told you "clustervision's in-memory IR is the reference implementation, and
`cv export --format json` emits it". That was false on every axis: the document specified camelCase
with blocks tagged `kind: "toolResult"` while the export emitted snake_case with
`type: "tool_result"` and no version marker at all — and **no Rust code could read or write the
format**. The only implementation was a dozen lines of translation in the browser.

0.3 resolves it in the direction that leaves one vocabulary: the spec adopts the IR. It gains the
concepts IR v2 added — message `kind` and `origin`, session `system_prompt` and `lineage`, tool
`namespace`, reasoning tokens and cost — and promotes system prompts and cost out of the old
"deliberately not in scope" list, since every harness that records them records the same thing.
`cv export --format json` now emits a valid `open_session: "0.3"` document, and a new `opensession`
harness reads one back through the registered export sources, tolerant of the 0.2 spellings. A real
900-message session round-trips with an identical message, kind and block census; the one field
that differs is `source_path`, which honestly names the document the reader actually opened. The
browser's whole camelCase translation layer is gone with it.

### One name per thing, enforced

The release's thesis applied to itself. cv answers the same questions through several doors —
`cv <cmd> --json`, the daemon's HTTP API, the MCP tools, the desktop app — and a consumer should
never have to ask which door a row came through before reading it. Asking each door the same
questions, and sweeping every type that carries both a serde spelling and a canonical accessor,
turned up six places where it mattered:

- **A harness had two spellings.** `Harness` derived `rename_all = "lowercase"`, which spells the
  *variant*, so `KimiCode` serialized as `"kimicode"` while `as_str()`, `cv --harness` and
  `cv ls --json` all said `"kimi-code"`. Filtering on the name returned nothing from
  `cv show --json`. Serde goes through the canonical name now, for all five hyphenated harnesses.
- **An instant had three spellings**: `…Z` from `/api/sessions`, `…+00:00` from `/api/search` and
  from the CLI. Every door uses `to_rfc3339()`.
- **Sub-agent provenance was dropped twice.** `/api/touched` and `/api/session/…/events` both
  omitted the `agent_id`/`parent_id`/`workflow` trio they had always carried, so a caller could not
  tell a top-level run from one lane of a workflow.
- **Compactions were the odd list out.** That route was the only one wrapped in an object rather
  than served bare, and renamed two fields on the way: the CLI's `boundary_msg_idx` was `index`,
  its `pre_compaction_span` was `pre_span`. The daemon serializes the same struct the CLI does now,
  and adds `headline` rather than renaming anything.
- **Session rows were missing two keys** (`path`, `size_bytes`) from the daemon, so its rows were
  not the row §3 describes.
- **Codex session sizes were badly wrong in `cv ls`.** A rollout over 8 MB is sampled rather than
  read, and the estimate extrapolated from the head — exactly where a rollout keeps its largest and
  least representative records. A real 10.9 MB session reported 74 messages against a true 198.
  Extrapolating from every sampled byte gives 189; across three rollouts the error fell from 63%,
  58% and 25% to 4.5%, 19% and 1.6%.

`crates/cvd/tests/parity.rs` is what notices next time: it asks both doors the same questions and
compares key sets, allowing a door to ADD a field but never to rename or drop one, and pins the
timestamp spelling and the rule that a list is a bare array. It was verified the way a guard should
be — rename a field, watch it fail, put it back, watch it pass. The enum sweep found no remaining
type that spells itself two ways.

### Also in this release

- **`cv workflow <session> <run> --revive` salvages a dead run.** A `Workflow` run that dies
  mid-flight (session limit, kill, crash) loses every in-progress lane at once, and the
  orchestrator's state file keeps only a ~400-char preview of each prompt, so re-running the
  script restarts every lane from zero. `--revive` mines each lane's own transcript
  (`<session>/subagents/workflows/<run>/agent-<id>.jsonl`) for the FULL original prompt plus the
  work it already landed — files written or edited, distinct commands run, its last substantive
  note — and prints a ready-to-paste standalone `Agent` prompt per unfinished lane
  (`--revive-all` includes finished ones), so lanes come back resumed rather than restarted and
  without the workflow runtime.

- **`cv prune --revive` works again on Claude Code ≥ 2.1.277 — the pin now lands in
  `usage.iterations` too.** Claude Code's resume gate sizes the loaded context from
  the last real assistant record's `usage`, and since 2.1.277 it prefers the last
  request entry of that record's `iterations` array (skipping `advisor_message` /
  `compaction` iterations) over the top-level counters. Revive pinned only the top
  level, so the stale wall figure survived inside `iterations`; the gate read it,
  and — with the total at or above *context window − max-output reserve (≤ 20k) −
  3k* — refused the (now small) session client-side with a synthesized `Prompt is
  too long` (no `errorDetails`, no request ever sent), on every resume attempt,
  whatever `--window` was. Now `usage_total` reads a record exactly as Claude Code
  does (the last non-auxiliary iteration when well-formed and non-zero, else the
  top level), so `--window` sizing, the honest figure, stale-record detection and
  the `_cv_orig_ctx` stash all agree with the gate, and revive pins every
  non-auxiliary iteration alongside the top level (output counts untouched, caches
  zeroed). Regression test: `revive_pins_usage_iterations_too`.

- **Claude Code fidelity catch-up (2.1.23x → 2.1.278).** The transcript format moved
  under cv; this brings the adapter, prune and doctor back in line with what the
  files actually hold:
  - *System reminders are content, not UI state.* The `<system-reminder>` text Claude
    Code appends to the prompt — hook output, edited-file notices, queued task
    notifications, CLAUDE.md `instructions`, skill/agent listings, … — has lived in
    separate `attachment` records (with `rendered[].content`) since ~2.1.23x, and cv
    dropped them. Rendered attachments now parse as System turns carrying exactly that
    text (`extra.attachmentType` names the kind; the full object under `full`/`complete`;
    large text spans lazily), so `cv show`, `search`, `dataset` and `doctor` see what the
    model saw. `cv doctor` itemizes them as **system reminders** by kind instead of
    misreading them as fixed system-prompt overhead (~90k of a 327k "overhead" in one
    live session). Unrendered attachments stay bookkeeping.
  - *Synthetic notices are not turns.* Claude Code's client-side `assistant` rows with
    `model: "<synthetic>"` ("Prompt is too long", "No response requested.", …) are never
    sent to the API. The lean passes now surface them as System notices
    (`extra.subtype` = `api_error`/`synthetic`, with `error`/`errorDetails`), they no
    longer count toward `cv ls` message counts, `--keep-last`, `--range` indices or
    `--window` sizing, and they never name the session model. `complete` keeps their
    original shape for round-trips.
  - *Titles and bookkeeping records.* `custom-title` (`/rename`) now outranks `ai-title`
    in `cv ls` and `Session.title`; `agent-name`, `tag`, `relocated`, `continued-in`
    (session lineage) and `pr-link` land in `Session.extra`; every other non-conversational
    record (`cost-state`, `atis-latch`, `frame-link`, `content-replacement`, artifact
    watches, and anything future without a `message`) round-trips as a carrier under
    `complete` instead of being silently dropped. `summary` records — which Claude Code no
    longer writes — are still read.
  - *Persisted tool outputs.* A `<persisted-output>` stub (the real output went to
    `<session>/tool-results/<id>.txt`) keeps the stub as content — that is what the model
    saw — and records the path and size in the block's `details.persistedOutput`; `cv show`
    prints the on-disk pointer under the result, and `cv index`/`search` index the file's
    head (up to 1 MiB) and scan it for live snippets, so text the tool produced is findable
    even though the transcript holds only the pointer. `cv doctor`'s verdict names system
    reminders when they are ≥ 10% of context.
  - *prune:* a windowed tail keeps the LAST of each session-level singleton record
    (`custom-title`, `ai-title`, `tag`, `agent-name`, `relocated`, `cost-state`,
    `atis-latch`, `continued-in`, legacy `summary`) instead of only `summary`; both id
    spellings (`sessionId` and `session_id`) are restamped; byte estimates (the honest
    figure and the no-usage fallback) count rendered attachments as prompt text; and
    `--thinking` snips every old thinking block regardless of `--min-size` — Fable-era
    signature-only blocks are ~700 bytes on disk but hundreds of tokens on the wire
    (Claude Code's own `thinking_drop` freed ~105k for 236 of them).

- **Harness catch-up, from source (2026-09-19).** Every harness we have a checkout for
  (`~/pug/*`) was fast-forwarded and its persistence code diffed against what the adapter
  targeted; three of six had moved their live store out from under cv without any error.
  `docs/FORMATS.md` is re-verified throughout.
  - *Codex (0.154).* The parser handles the paginated history mode (`item_completed`
    items as System notes with the full item in `extra`, since the `event_msg`
    user/agent twins are gone), the inter-agent `agent_message` channel, `token_usage_record`
    as the authoritative per-response usage (`cache_write_input_tokens` mapped), fork/
    subagent rollouts (the embedded parent prefix is skipped in lean passes and tagged under
    `complete`; `create_time` fixes fork-stamped timestamps), the grown `session_meta`
    (swarm fields into `Session.extra`), `thread_settings_applied`/effort model tracking,
    `turn_aborted`/`thread_rolled_back` notes, tool `namespace`, and `.jsonl.zst` rollouts.
    The emitter now writes rollouts Codex can resume: local-time file names, a complete
    `turn_context`, string-form error outputs (`[error] ` prefix, restored on parse), a
    guaranteed `cwd`, `history_mode: legacy`, `model_provider`, summary-only reasoning.
  - *OpenCode.* Reads the canonical `opencode.db` (the JSON tree importer was deleted
    2026-06-02); JSON is a fallback deduped by id; session facts, tool attachments and
    `state.{title,metadata,time}` are preserved; the db is file-watched for freshness.
  - *Hermes (schema 30).* In-place compaction visibility (`active`/`compacted`), `ORDER BY
    id`, compaction summaries as boundary pairs, `system_prompts` table, lineage markers,
    listing parity, `cwd`/`git_branch` first-class; old-schema dbs unchanged.
  - *Goose (schema 16).* Titles from `name`; `metadata_json` usage/model; `document` and
    `error` blocks; bare-array and rmcp-3 tool results; millisecond timestamps.
  - *Gemini/Qwen.* Second (sandbox) storage root; cwd from `projects.json`/`.project_root`.
  - *OpenClaw.* Live sessions and transcripts moved to `agents/<id>/agent/openclaw-agent.sqlite`
    on 2026-07-11; discovery now reads `session_windows`/`session_nodes` and replays
    `transcript_events` (each row is the old JSONL line) through the same parser, deduping
    legacy JSONL twins; v4 `leaf`/`appendMode: side` branch controls are followed (a port of
    OpenClaw's tree navigation) so only the visible branch is emitted in lean passes;
    `compaction`/`reset`/`branch_summary`/`custom_message` become System notes,
    `session_info.name` the title, `model_change` the model; checkpoint/trajectory/archive
    siblings are excluded from discovery.
  - *Kimi Code (new harness `kimi-code`).* kimi-cli is deprecated and `~/.kimi` frozen since
    the 2026-06 migration; its successor writes `~/.kimi-code/sessions/wd_*/session_*/agents/
    <id>/wire.jsonl` (flat records, `time` in ms, object `args`, camelCase usage, `cwd` in
    `state.json`) — 34 sessions on this machine were invisible. The new adapter discovers the
    workspace tree (honouring `session_index.jsonl` tombstones), buffers each LLM step into one
    Assistant turn plus Tool turns (usage from `step.end`), maps compaction, task-termination and
    cancel notes, system prompts from `profile.bind`, injected prompts with their `origin`, and
    persisted `tool-results`/`tasks` outputs as on-disk pointers; sub-agents ride in
    `Session.extra.agents`. `cv resume` launches `kimi --session session_<id>`.
  - *Verified against the harnesses themselves, not just their source.* Real stores were
    generated by running each harness (in throwaway homes) and are now fixtures with tests
    pinning what the real writer produced: a Goose 1.51.0 `sessions.db` (stub-provider turns,
    a retry-exhaustion error, a `goose session import` of a Claude transcript), a Hermes
    schema-30 `state.db` written through Hermes's own store API (in-place compaction, branch/
    reset/delegate children, `system_prompts`, a foreign import), an OpenClaw
    `openclaw-agent.sqlite` written by its store modules (branches, fork, legacy import), and
    Gemini 0.46.0 registry files. Codex 0.155.1 itself resumed a cv-emitted rollout (session
    header and history loaded; only the model call failed, by design), its compression worker
    produced `.jsonl.zst` files that cv lists and parses, and `$CODEX_HOME` is honoured.
    Findings folded back: Goose hides `userVisible: false` rows the way Goose does; Hermes lists
    archived/hidden sessions (Hermes keeps them resumable) with the flags in `extra`, titles a
    rotation tip from its root, and surfaces `origin_json.imported_from`.

## 0.10.0 (2026-07-17)

- **`cv ls --json` closes the consumer gaps (#15).** Every row now carries
  `sizeBytes` (the transcript file's length — free from the same `stat` that
  already guards a since-deleted row, so it is always present and needs no extra
  I/O). A new `--enrich` flag (with `--json`) adds two transcript-derived fields:
  `git` — the same `branch`/`commit`/`remote` object `cv show --json` emits, read
  from the transcript's recorded git context (the branch the session *ran on*, a
  historical fact — not the cwd's current branch) — and `displayTitle`, the row's
  `title` with a fallback synthesized from the first real user turn (peeling a
  leading `<system-reminder>` block and skipping the `Caveat:` preamble, bare
  command wrappers, and tool-result turns; explicit `null` only when a session
  has neither an explicit title nor any user prose). `title` itself is left
  untouched, so existing consumers see byte-identical keys. Enrichment costs one
  lazy transcript parse per emitted row — O(`--limit`), not the whole fleet — so
  plain `cv ls --json` stays a catalog-only, milliseconds-warm read. The `ls`
  freshness contract is now documented (manual `cli.md`): brand-new sessions are
  always seen on the next read (a new file bumps a watched dir mtime); the only
  bounded blind spot is an in-place append to an older session outside the
  top-50, which lags at most `CLUSTERVISION_MAX_STALE_SECS` (default 900s); and
  `--fresh` forces a full re-discovery.
- **Every observed fact now carries its source and freshness — `Unknown` is
  first-class.** A land is no longer a bare boolean; it is "observed landed,
  git-verified, as of a pass 4m ago". A new pure `Provenance { observed_at,
  source, freshness }` shape (cv-core `task/provenance.rs`) rides the debt and
  show surfaces: `source` distinguishes `git-verify` from a `self-report`ed
  `Done` (the completion carve-out made structural, never silently equal to a
  verified land), and `freshness` (`Fresh` / `Stale{age}` / `Unknown`) is
  derived purely from the verifier heartbeat — a revision the verifier has never
  checked since it became verifiable reads `Unknown`, not implicitly fine. Human
  output gains `landed · observed 4m ago` / `ready … · NEVER verified`, `cv task
  show` labels the landed line `git-verified` and a `Done` `self-reported`, and
  additive `provenance` keys ride `cv task debt --json`, MCP `task_debt`, and
  `GET /api/tasks/debt`. No new git calls; freshness takes the heartbeat as input.
- **The task substrate (`cv task`, `task_*` MCP tools, `/api/tasks*`).** Durable,
  replayable dispatch objects for agent fleets, designed against the failure mode
  that killed a 200k-line predecessor (mission-control): progress trackers that
  trust agents' own claims. Four laws: (1) landing state is **observed, not
  attested** — only cv's git verifier (ancestry + `git cherry` patch-id
  equivalence + whole-branch range patch-id recomputed from a recorded base) can
  write `merged_local`/`landed`, and agent-facing append paths reject those
  kinds at the store seam; (2) reviewer independence is **read from transcripts**
  (harness family via the catalog), advisory-warn, never a gate; (3) **no
  authority machinery** — landing authority is whoever can push, cv only tracks
  and verifies; (4) **small**. Lifecycle grammar and its 9-test invariant roster
  ported from mission-control's one pure module (`land_request.rs`): reviewer
  binding, refute-is-terminal, `MergedLocal ≠ Landed`, issues-without-state-change,
  merge/land evidence must match reviewed content. Storage is a locked CAS
  append log (`tasks/events.jsonl`, board's flock recipe via the extracted
  `lockfile::FileLock`) that never contains an event replay would refuse;
  interior corruption is loud, torn tails tolerated. Projections: `list`,
  per-agent `inbox`, and `debt` — reviewed-but-unlanded work, the honest number.
  `cvd watch --verify-interval N` runs the verifier as a daemon; board channels
  carry the notification trail.
- **The trust layer watches itself.** `run_verify` writes a heartbeat
  (`tasks/last_verify.json`); every debt surface renders verified-as-of and
  warns NEVER/STALE, and `cvd`'s verify interval defaults on (300s). Replay
  **quarantines** reducer-refused events loudly instead of bricking every
  consumer at once (appends fail closed on degraded reads; log format header
  v1). Landed revisions are **re-observed every pass**: a forged `Landed`
  contradicts observation within one tick and becomes a SUSPECT debt row —
  and suspects persist across partial verifies, so a targeted re-verify
  cannot launder one away. `cv`/`cvd --version` and the cvd startup log embed
  the build commit.
- **The adversary gym, and three closed holes.** `crates/cv-sim` ships a
  deterministic synthetic-fleet generator, a replay-cost bench (the O(n²) append
  curve *measured*, not hand-waved), and an **adversary gym**: attack tests that
  assert each defense fires, alongside honest `pin_*` tests that named the
  sensors' known weaknesses so closing one is a measurable git event. This
  release closes three of them:
  - **Review receipts require engagement, not a quoted sha.** `saw_change` now
    demands structural evidence the reviewer opened the change — a repo
    file-read, or a real `git diff`/`show`/`log` in *command position* — so
    `echo <sha>` no longer passes (it reads `undetermined`). Still a heuristic
    (a pointless repo read passes), but the trivial forgery is dead.
  - **Verifiable completion.** `cv task done --check-cmd/--check-file/--check-http`
    makes cv RUN a completion predicate: a passing check records the `Done` as
    *observed* (provenance `checked`), a failing one refuses it and leaves the
    task open. Law 1 now reaches non-code tasks; a check-less `Done` stays
    self-reported (and is labeled as such).
  - **Per-endpoint identity (TOFU).** An endpoint binds a token on first use
    (`--token` / `$CV_TOKEN`); thereafter an identity-bearing event
    (claim/release/propose/pass/refute) stamped as that endpoint must present
    the matching token or the CAS append is rejected. Unbound endpoints stay
    trusted (the solo/human case) — authentication of the `by` claim, not
    authorization; no seats, no roles, and only the token's hash is stored.

  Receipts and independence remain **advisory, never a gate**; the sensors
  inform judgment, they do not control it. Review **receipts are a heuristic
  signal, not proof**, and law 1 is spelled out as covering **landing** — and
  now, via `--check-*`, verifiable **completion** — with a check-less `Done`
  honestly labeled self-reported. Doc-comments and the manual state all of this
  plainly instead of implying more.
- **Task identity comes from the environment.** `CV_ENDPOINT` is the identity
  convention (the spawner sets it; `--from` overrides); identity-bearing verbs
  (claim/release/propose/pass/refute) refuse to act with neither present
  rather than silently recording a shared sink. Age is the escalation
  mechanism: list/inbox gain age columns (oldest first, ⏰ past 24h) and the
  debt view gains an awaiting-review section anchored on the new
  `proposed_at` — a dead reviewer is now visible, aging state on the owner
  surface. `cv_core::sanitize` strips ESC/OSC/control characters at every
  terminal render seam (tasks, plus `ls`/`timeline`/`search`/`recall`/`show
  --subagents`; JSON transports stay raw — escaping is the transport's job).
- **Below-the-line audit followups.** `propose` warns when another live task
  carries the same branch or worktree (observation, never a block). Verifier
  issue dedup widens to the last 5, so alternating findings stop growing the
  log forever. `cv board unanswered` / `board_unanswered` / `GET
  …/unanswered` surface requests nobody answered, oldest first with age.
  Reviewer-independence checks read the *last assistant model* from
  multi-model harness transcripts and map model-id prefixes to families —
  Cursor-style harnesses no longer force `undetermined`.
- **One shape, one identity across CLI/MCP/HTTP.** `task/views.rs` owns the
  row types (`TaskRow`/`InboxRow`/`DebtRow`/`AwaitingRow`) and
  `DebtReport::compute`; all three front-ends consume them, so the surfaces
  cannot drift. `--state` now rejects unknown vocabulary naming the valid set
  on every surface (was a silent empty result). MCP board identity routes
  through `CV_ENDPOINT` (one-release legacy-owner transition on
  `board_release`). New coverage: MCP task-tool race smoke + `cvd`
  `/api/tasks*` route tests.
- **Agent-id drill-down everywhere.** `find`/`find_cheap` fall back to
  workflow sub-agent resolution after normal lookups miss, so MCP
  `read_session` and every front-end inherit `cv show <agent-id>` behavior.
  `cv workflow --results` falls back to per-agent transcript returns when
  `journal.jsonl` is absent — crashed runs still harvest fully. Test-suite
  flake classes killed: cvd tests bind port 0 and parse the real port; git
  fixtures neutralize global/system gitconfig (and the verify suite runs 5x
  faster).
- **`cv search` shows sub-agent hits as lanes.** A folded-in sub-agent hit
  renders its bare `agentId` (what `cv show <id>` resolves) with a
  `⤷ sub-agent of <parent>` tag (+ `⟐<workflow>` when it belongs to one); the
  empty-result path nudges toward `cv index --subagents` only when the index
  genuinely holds no forest.
- **`cv` dies quietly on closed pipes.** SIGPIPE resets to `SIG_DFL` on unix,
  so `cv task list | head` behaves like every other pipeline citizen instead
  of panicking on broken-pipe writes.
- **`cv ls --json`** — the listing as one JSON array on stdout (same rows,
  filters, sort, and `--limit` as the table; camelCase, OpenSession-aligned
  fields), so downstream tools can consume the catalog instead of scraping
  the table or re-scanning transcript files. (@akapug)
- **`cv search --json`** — the hits as one JSON array on stdout (same hits,
  order, and `--harness`/`--limit`/`--semantic` handling as the table), with
  camelCase fields, FULL session ids (the table truncates to 8 chars), the
  untruncated snippet, the relevance score, and the sub-agent provenance trio
  `agentId`/`parentId`/`workflow` — so downstream tools can consume hits
  instead of regex-scraping the table. (@akapug)
- **`cv prune --json`** — the prune report as one JSON object on stdout
  (camelCase; FULL `sourceId`/`newId`, before/after bytes, snipped-payload
  and freed-token counts, revive detail, `newPath`/`sidecarPath`), for
  downstream resume-optimizers that today regex-scrape the human report off
  stderr. Dry-run honest: nothing is written, so paths — and `newId`, unless
  `--to` pinned it — are explicit nulls plus a `note`. The human report stays
  on stderr (compose-family convention), so stdout is pure JSON. (@akapug)
- **`cv prune --declassify`** — snip conversational message *prose* (user
  prompts + assistant text blocks) whose density of caller-supplied terms is
  high, into the sidecar with a `[PRUNED …]` marker — lossless and
  `--retrieve`-able, like every other prune pass. A message is snipped iff it
  holds ≥ 2 distinct terms; unlike the tool/`--thinking` passes it ignores
  `keep_last` (recency is no shield when the whole loaded context is scored).
  The terms are **always external** — `--declassify-tokens` (CSV) and/or
  `--declassify-tokens-file` (one per line, `#` comments) — cv ships no
  built-in list, so `--declassify` without terms is a warned no-op. Also on
  the `prune_session` MCP tool (`declassify`/`declassify_tokens`). (@akapug)
- **Release mechanics.** `clustervision-core` compiles for
  `wasm32-unknown-unknown` standalone again (target-gated `uuid` `js`
  feature; CI checks it). Internal dependency pins track `0.10` so a
  crates.io build can never silently resolve an old core. `cv-tui` and
  `cv-web` are explicitly unpublished (`cv-tui` still ships as a release
  binary); releases now publish to crates.io via cargo-dist `publish-jobs`,
  so crates.io stops lagging the tag.

## v0.9.22 (2026-07-08)

Crash-forensics release, paid for the hard way: a real power loss killed a
~7-lane swarm mid-run, and reconstructing "which orchestrators resumed, which
dropped, and what died holding what" surfaced three blind spots. All three are
now closed:

- **Local time everywhere.** Every human-facing time cv prints (`ls`,
  `timeline`, `events`, `touched`, `tools --timeline`, `stats`, `search`,
  `blame`, `board`) now renders in the local timezone instead of UTC. The
  transcripts store UTC, but `git log` and human memory are local — silently
  mixing the two skewed the reconstructed outage window by the UTC offset
  (a real 4-hour miss during the recovery).
- **`cv ls` shows each session's `created → last-active` span** (minute
  granularity, same-day sessions compress the right side) instead of a bare
  date. A long-lived orchestrator vs a one-shot — and post-crash, resumed vs
  dropped — is now visible in the listing itself.
- **`cv timeline` marks multi-day sessions** with `⇠ since MM-DD`. Feed rows
  sit at last-activity; without the marker they read as start times.
- **Workflows are addressable by NAME.** `cv workflow <session> <name>`
  resolves a run by its workflow name (exact, else unique prefix; a re-run
  name → its newest run), and `cv workflow <name>` with no session resolves
  the name across the whole catalog — session titles are auto-generated and
  rarely mention the workflow you remember ("the stark-kill session" was
  titled "Review recent work across three projects"). A fleet-wide miss falls
  through to a ghost-launch scan, so a swarm that died before persisting is
  still findable by name. Run-not-found errors now list the session's run
  names.
- **The by-name and by-agent fallbacks are fast.** A cheap run-name index
  (script filenames + a byte-scan for `"workflowName"`) means the fleet-wide
  name search parses only matching state files; and the fallbacks now resolve
  through `find_cheap` (catalog + probe, no full re-discovery) with the full
  fleet scan reserved for the genuinely-unknown-id case. Fleet name hit:
  3.9s → 0.3s; direct agent open: 2.5s → 0.1s; ghost hunt: 10s → 1s.
- **Ghosts carry their harvest map.** A ghost launch is enriched from what
  DID survive the crash: its orphaned script file (written at launch)
  recovers the run id, which keys the `subagents/workflows/<runId>/` debris
  dir — reported as `· run wf_… · DEBRIS: 10 agent transcript(s), 1 journaled
  result(s)`. The power-loss ghosts turned out to be sitting on 16
  transcripts + 4 journaled results nobody could see.
- **`cv workflow --follow` (`-f`) tails a live run**: one line per agent
  state transition as the harness flushes the state file, then the full
  render (honoring `--json`/`--script`/`--results`) at terminal status.
  Waits for a run that hasn't registered yet.
- **Full per-agent lane returns.** `cv workflow <sess> <run> --results` reads
  the run's `journal.jsonl` and prints each agent's **complete** journaled
  return value (the state file keeps only a ~400-char `resultPreview`); the
  run's `--json` now carries them as `journal_result` per agent. This closes
  the long-standing "per-agent returns truncated to ~400-char previews" gap.
- **`cv show <agent-id>` works without knowing the parent.** An id that
  matches no session resolves as a sub-agent id fleet-wide (parallel filename
  scan of `subagents/` sidecars, both tiers); one parent → the agent renders
  directly with a provenance banner, several (fork lineages share sidecars) →
  ready-to-paste disambiguation commands. Closes the "workflow sub-agents
  aren't indexed as sessions" gap.
- **The workflow parser stops dropping load-bearing fields.** Now parsed and
  rendered: the run's aggregated **`result`** (the script's return value —
  the harvest payload, previously discarded entirely), `logs` (the script's
  `log()` narration; tail shown, full in `--json`), `args`, `taskId`,
  `startTime`; per-agent `attempt`, `startedAt`, `lastProgressAt`, and
  `lastToolName`/`lastToolSummary` — for a dead or interrupted agent, exactly
  where it was when it stopped. Also fixed a hot-loop prefilter in the
  transcript launch scan (pending-id set instead of parsing every tool-result
  line).
- **`cv workflow <session>` detects ghost launches**: `Workflow` tool
  invocations visible in the transcript with **no persisted run state** — the
  signature of a crash/power loss/hard kill before the harness wrote
  `workflows/wf_*.json`. (The power loss left two such swarms completely
  invisible to the old list — including the one whose uncommitted debris most
  needed finding.) Errored-at-launch calls and `scriptPath` resumes of
  recorded runs are excluded, so no false positives. New core API:
  `workflow_launches`/`ghost_launches` + `WorkflowLaunch`. The list form's
  `--json` output is now `{"runs": […], "ghost_launches": […]}` (was a bare
  array).

## v0.9.21 (2026-07-02)

Security-advisory dependency bumps on top of the v0.9.20 audit, plus a CI fix.

- **anyhow ≥ 1.0.103** — clears RUSTSEC-2026-0190 (`downcast_mut` unsoundness).
- **memmap2 ≥ 0.9.11** — clears RUSTSEC-2026-0186 (affects the optional `mmap`
  feature of `clustervision-core`).
- CI: the no-default-features wasm-config build referenced the pre-rename
  package name `cv-core`; corrected to `clustervision-core` so the check runs.

(Both advisory fixes were contributed by @akapug. The known follow-up: the
default read path memory-maps live, externally-truncatable transcript files,
which is unsound per memmap2's contract — a snapshot/immutability guard before
mmap is the real fix, tracked separately.)

## v0.9.20 — the takeover audit (2026-07-02)

A fresh set of eyes (Claude Fable 5) read the whole codebase, then a swarm of
agents fixed what the review found — in parallel, in one tree, using cv to read
its own sessions along the way. Everything below shipped with regression tests;
the full workspace suite, clippy, the wasm target, and the real-corpus
invariant tests are green.

### Security

- **`cvd serve` no longer trusts the whole internet.** The wildcard
  `Access-Control-Allow-Origin: *` — which let any website a user visited fetch
  their entire transcript corpus off `127.0.0.1:7777` — is gone. CORS is now an
  allow-list (tauri + same-host origins, echoed with `Vary: Origin`); the
  `Host` header is validated on loopback binds (kills DNS rebinding); binding a
  non-loopback address requires `--token`/`$CVD_TOKEN` or an explicit
  `--insecure-expose`; `/api/*` supports bearer-token auth; workers are
  panic-isolated.
- **Redaction got real teeth.** Truncated PEM bodies (the common
  clipped-tool-output case) are now caught; new token families: Stripe live
  and restricted keys, GitLab personal access tokens, Slack session/app
  tokens, npm, Hugging Face, Groq, xAI, DigitalOcean, and Shopify tokens
  (prefixes spelled out in `redact.rs` — not here, because a changelog that
  lists secret-shaped strings gets flagged as a secret itself, which is
  rather the point of this feature); connection-string passwords
  (passwords embedded in connection-string URLs); case-insensitive `bearer`;
  `Proxy-Authorization`; `git.remote` and session/message `extra` maps are
  scrubbed. Root-cause fix: the keyword-blob scanner only ever matched blobs
  at end-of-input — quoted mid-sentence secrets now redact. Assignment
  matching no longer mangles code like `let token = get_token();`.
- **The web viewer bounds zip extraction** (256 MB/entry, 512 MB total,
  header sizes distrusted) and markdown links reject protocol-relative
  `//evil.com` navigation.
- **`cv distill`/`loom --generate` print an egress notice** naming the
  provider, model, and payload size before any transcript leaves the machine.

### Index integrity

- A parse error mid-session no longer commits truncated index docs stamped
  fresh-forever; the error path deletes the partial docs.
- Plain `cv index` no longer silently deletes the sub-agent forest folded in by
  a previous `cv index --subagents`.
- An FTS-fresh but events-stale sub-agent no longer loses its search docs.
- The event catalog keys freshness on `(mtime, size)` like the FTS index — a
  mass mtime bump no longer triggers a whole-corpus re-parse.
- `cv search` without an index caps its in-memory haystack (256 KB/session)
  instead of materializing multi-GB sessions; with a stale index, "no matches"
  now says how far behind the index is.
- Semantic search validates the stored model id and vector dimensions instead
  of silently ranking garbage after a model switch.
- A failed index open during *search* propagates the error instead of deleting
  and recreating the index directory.

### Prune / resume safety

- `--window N` now honors its contract: the kept tail is the largest one
  **≤ N** real tokens (it previously overshot by up to one turn), with a loud
  warning when a single turn alone exceeds the budget.
- Revive derives its "honest" context figure from recorded usage deltas
  (byte-estimate only as a fallback floor) and never rewrites records below
  what the evidence supports — a revived session can no longer sail through the
  resume gate and blow the real context limit.
- Sub-agent sidechain records no longer poison window sizing, revive
  arithmetic, or re-root selection.
- Sidecar payload ids are unique (no more unretrievable payloads behind
  colliding `unknown` ids); `--retrieve` errors on duplicates instead of
  silently returning the last one.
- Windowing keeps the session title record and records trailing the final
  turn; `--drop` markers no longer advertise a retrieval that can't happen.
- Pruning parses each line once instead of ~6× (roughly half the peak memory
  on the GB-scale sessions prune exists for).

### Conversion fidelity

- **`cv convert`/`cv port` actually verify now**: the previously-dead
  `emit_verified` machinery runs on every conversion and prints what the
  target format loses.
- Same-harness ports parse in format-complete mode: Claude→Claude ports replay
  system records (compact boundaries, slash-commands, hooks) with parent chains
  re-linked — no more dangling `parentUuid`s in a rehomed transcript.
- →Codex: the model now survives (emitted as a real `turn_context` record, not
  a misused `model_provider` field) and `is_error` tool results round-trip
  (object form with `success:false`).
- →Claude: user turns with images/files/mixed content emit faithful array
  content instead of being flattened to text.
- `serde_json/preserve_order` is on workspace-wide: ChatGPT exports missing
  `current_node` and Zed multi-tool-result messages no longer get shuffled
  into key-sorted order.
- One emit registry: `Adapter::emit`/`can_emit` (never called, frequently
  lying) are gone; the dispatch table in `emit.rs` is the single source of
  truth and `supported_targets()` derives from it.
- Emitting a lazily-parsed session materializes spans first instead of
  panicking or serializing span structs into the output.

### Core

- Query calculus: bare URLs are needles (with a did-you-mean guard for real
  typos); quoted phrases with commas stay literal; `until:`/`since:` work;
  `updated:2026-06-01` means the whole day, not exactly-midnight; `msgs`
  counts user+assistant consistently across prefilter and full match.
- Board claims use real advisory file locks (kernel-released on process
  death) — the 20-second lock-steal path that could crown two winners is gone.
- The Claude sniffer tolerates transcripts opening with long runs of meta
  records; discovery timestamps are min/max rather than first/last (clock-skew
  robustness); `find()`'s slow path filters vanished files like the fast path;
  the claude seek path re-checks file size after opening (mirroring codex).
- MCP server: requests dispatch concurrently — a 120s `await_omen` no longer
  blocks every other tool call; `list_sessions`/`search_sessions`/
  `observe_stream`/`project_sessions` use the catalog fast path instead of a
  full fleet scan; malformed numeric args are protocol errors instead of
  silent defaults.

### App / UX

- Transcripts from dropped files render windowed (200 at a time) instead of
  freezing the tab on 30k-message sessions; chunk wiring is O(chunk) not
  O(session); large tool blocks have copy buttons; the activity heatmap clamps
  to 6 years so one 1970-dated session can't explode the DOM.
- `cv prune --range` accepts the same grammar as `cv show --range`;
  `cv ls --harness` help lists the full harness set; prune warnings surface in
  the CLI and MCP results.

## v0.9.18 and earlier

See git tags.
