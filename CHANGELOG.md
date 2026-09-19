# Changelog

## Unreleased

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
