# Harness session formats (reverse-engineered)

This is the ground-truth catalog of how each harness stores sessions on disk, reverse-engineered from a
real machine (2026-05-29; re-verified 2026-09-19 against upstream checkouts under `~/pug` for Codex
`132c2be23` (0.154), OpenCode `fee476bb` (1.18.31), gemini-cli `cfbcaa8`, Goose `2090ad1c`
(1.51.0), OpenClaw `0e9181234a`, Hermes `6d8a8bebf7` (schema v30), kimi-cli `86f1364`, and against the
shipped bundles for Claude Code 2.1.278 (re-checked against 2.1.296 by the corpus) and Kimi Code 0.39.1; kimi-cli 1.48.0/1.51.0 and Kimi Code
0.39.1/2.1.1 re-read 2026-10-11 from their corpora in `crates/cv-core/tests/corpus/`). It drives the adapters in `cv-core`.
Keep it accurate; it is the spec.

The unifying insight: **all harnesses encode the working directory (cwd) into where/how they store a
session.** That cwd-coupling is exactly why sessions are "dir-jailed" and hard to find. clustervision
decouples them via a unified IR.

## This document and `formats/*.toml` — which one to edit

Since 0.11.0 there are **two** sources of truth and they do not overlap. Each adapter has a
machine-readable manifest at `formats/<harness>.toml` holding `[upstream] repo/commit/date`, the
`store = [...]` paths, and a `[types]` table naming every persisted record/part/column with cv's
status for it (`handled` = its own match arm, `generic` = covered by a blanket rule, `carried` =
verbatim only under `ParseOptions::complete`, `ignored` = known and deliberately skipped).

- **The manifest is authoritative for the machine-checkable vocabulary**: which record types exist,
  what cv does with each, and which upstream commit that was pinned against. It is enforced in both
  directions — `crates/cv-core/tests/formats_manifest.rs` fails when a
  `handled` type is no longer in the adapter, or when the adapter matches a literal the manifest
  does not name.
- **This prose is authoritative for everything a table cannot hold**: where the store lives and how
  it is discovered, how records thread into a conversation, what a field *means*, which fields are
  load-bearing, and the traps.

So: a new record type, or a change in how cv treats one, is a **manifest** edit (plus the adapter).
A change in how the format *works* is an edit **here**. When the two disagree the manifest wins on
vocabulary and this document wins on meaning — and one of them is then wrong and should be fixed.

The per-harness type lists below are therefore **illustrative, not exhaustive**, and are not the
checked list. For the current picture run the `formats_manifest` test (drift against the pinned
manifests) and `cv formats [--harness h] [--recent N]` (what recent real sessions on this machine
actually contain: message kinds, block types, and record vocabulary the adapter did not interpret,
with anything the manifest does not name marked `NEW`). `tools/harness-drift.sh` refreshes the
`~/pug` checkouts and greps their persistence enums against the manifests.

## IR names this document uses (cv 0.11.0)

The contract is `docs/INTERFACE-V2.md` §4; this is only the vocabulary needed to read the notes
below. A `Message` has a `role` (who speaks), a `kind` (`prompt`, `reply`, `tool_result`,
`injected_context`, `system_prompt`, `notice`, `compaction_boundary`, `compaction_summary`,
`model_change`, `error`, `subagent_spawn`, `subagent_return`, `branch`, `carrier`) and an `origin`
(`human`, `model`, `harness`, `hook`, `scheduler`, `subagent`, `import`, `unknown`). A `Block` is
tagged by `type` (`text`, `thinking`, `tool_use`, `tool_result`, `image`, `file`); `tool_use` carries
an optional `namespace`, `tool_result` an optional `details` object. Session-level facts that used to
live in `extra` are now first-class: `Session::system_prompt` and `Session::lineage`
(`forked_from`, `parent`, `spawned_by_tool_use`, `continued_in`, `continues`, `agent_path`).
`Usage` carries `reasoning_tokens` and `cost_usd` alongside the four token counters.

**Harness-specific facts are nested under the harness's own key** — `extra["claude"]["attachment_type"]`,
`extra["zed"]["thread_version"]` — never flat. The one non-harness namespace is `extra["cv"]`, which
holds cv's own parse diagnostics (`extra["cv"]["skipped_lines"]`, written by `harness::note_skipped_lines`
when a transcript had unreadable lines; `Harness::parse("cv")` is `None`, so the two cannot collide).
Exactly two flat message-level keys survive, both cv's own streaming bookkeeping: `_record` (the
verbatim carrier record under `ParseOptions::complete`) and `cv_byte_offset`.

---

## Claude Code — `~/.claude/`

- **Transcripts:** `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` (one session per file, JSONL).
- **Other roots:** when `CLAUDE_CONFIG_DIR` is set, Claude Code uses that directory instead of
  `~/.claude`, with the same layout below it. cv reads `$CLAUDE_CONFIG_DIR/projects`,
  `~/.claude/projects`, each entry of `$CLUSTERVISION_CLAUDE_ROOTS`, and each line of
  `$CLUSTERVISION_HOME/claude-roots` (an entry may hold `*` segments); see the manual's
  harnesses chapter.
- **cwd encoding (dir name):** leading `-`, then `/` → `-`, and `.` → `-`. **Lossy / not reversible**
  (original `-` and `.` collide). → *Do not decode the dir name; read `cwd` from inside the transcript.*
- **Per-line `type` values:** `user`, `assistant`, `attachment`, `system` (subtyped — see below), and the
  session-level bookkeeping records: `ai-title` (rewritten on every prompt), `custom-title` (`/rename`;
  outranks `ai-title`), `tag`, `agent-name` (derived session name), `mode`, `permission-mode`,
  `atis-latch`, `last-prompt` (`{leafUuid, lastPrompt, explicit?, rewound?}` — the loader's leaf pointer),
  `queue-operation` (`{operation: enqueue|dequeue|remove|popAll, reason?, content}`), `cost-state`
  (cumulative `totalCostUSD`/durations/`modelUsage` per model), `continued-in` (`{continuedInSessionId}` —
  the conversation moved to another session id, and cv's **`Session::lineage.continued_in`**),
  `relocated` (`{relocatedCwd}`, forks), `pr-link`
  (`{prNumber, prUrl, prRepository}`), `frame-link`, `content-replacement` (`{replacements}` applied at
  load — forks/microcompact), `artifact-comment-monitor`, `artifact-autoreact-ledger`,
  `file-history-snapshot`, `file-history-delta`, `worktree-state`, `isolation-latch`, the request ledger
  `api-request` / `api-request-shape` / `api-request-blob` and a fork child's `fork-context-ref` (all
  four 2.1.30x; see the 2.1.304 entry below). **`summary` records are no longer written** (gone since
  ~2.1.25x; still read).
  Most records since ~2.1.25x carry BOTH `sessionId` and snake-case `session_id`, plus `slug`,
  `entrypoint`, `userType`, `version`, `gitBranch`; user turns add `promptId`, `origin{kind:human|…}`,
  `promptSource`, `turnOrigin`, `permissionMode`; assistant turns add `requestId`, `effort`,
  `apiBlockIndex` (one line per streamed content block, sharing `message.id`).
  `formats/claude.toml` `[types]` is the checked list and says which of these cv interprets and which
  it merely carries under `ParseOptions::complete`; `cv formats --harness claude` shows what a
  real store holds today.
- **Attachments ARE prompt content (since ~2.1.23x).** The `<system-reminder>` text Claude Code appends to
  the prompt no longer lives inline in user content; each is an `attachment` record `{attachment:{type,…},
  rendered?: [{content: "<system-reminder>…</system-reminder>"}], renderedInHumanTurn?}` — `rendered[]`
  is byte-for-byte what the model saw; an attachment WITHOUT `rendered` is bookkeeping only. Kinds seen:
  `hook_success` (`{hookName, hookEvent, toolUseID, stdout, stderr, exitCode, command}`),
  `total_tokens_reminder`, `batching_reminder_sent`, `bash_output_audience_note`, `queued_command`
  (task notifications / queued prompts — the biggest by bytes), `silent_turn_reminder`,
  `edited_text_file` (`{filename, snippet}`), `task_reminder`, `deferred_tools_delta`,
  `agent_listing_delta`, `mcp_instructions_delta`, `skill_listing`, `invoked_skills`,
  `command_permissions`,
  `environment`, `date`/`date_change`, `model`, `instructions` (CLAUDE.md files), `session_context`,
  `remote_session_change`, `prompt_snapshot` (the whole system prompt + tool schemas; not re-sent),
  `auto_mode`, `goal_status`, `task_status`, `file` (an `@`-mentioned file), `compact_file_reference`,
  `thinking_drop` (`{blockHashes[], newlyDropped{blockCount,turnCount,reason}, thinkingBlocksSent}` —
  Claude Code dropped those thinking blocks from the REQUEST; the transcript keeps them and the loader
  re-applies the drop by hash). The full, current kind list is `formats/claude.toml` (`attachment.*`).
  cv parses a rendered attachment as a `Role::System` message with `kind: injected_context`;
  `origin` is `hook` for the `hook_*` kinds and `harness` otherwise, and the attachment's own kind rides in
  `extra["claude"]["attachment_type"]`. `prompt_snapshot` is the exception: its
  `attachment.systemPrompt[]` sections are joined into **`Session::system_prompt`** (the latest
  snapshot wins — a resume re-snapshots), and it is a session fact, never a message.
- **`invoked_skills` is the restore set, not a prompt delta.** Its payload is
  `attachment.skills[]`, one `{name, path, content}` per skill the session invoked, with `content` the
  text as it was rendered then (a `Base directory for this skill: …` header, the body, and — for a
  skill invoked with arguments — the invocation's `ARGUMENTS:` tail; a skill over the per-skill cap
  ends with the truncation marker). Claude Code writes ALL of them back as one attachment at every
  compaction, and `--resume` rebuilds the store from the records after the last compact boundary.
  Nothing else forgets an entry, so a skill invoked once returns at every later compaction with its
  old text, even after its file changed or the skill was removed. `path` is `<source>:<name>`, source
  `userSettings` (the config dir), `projectSettings` (the recorded `cwd`'s `.claude`) or
  `bundled`/`plugin:<plugin>`; a user or project skill's file is `skills/<name>/SKILL.md`, else the
  legacy `commands/<name>.md` a slash command became (a nested `a:b` name is `commands/a/b.md`).
  `cv prune`, `cv rewind` and `cv port` (to Claude) take `--skills keep|drop|drop-stale` and rewrite
  those records in the session they write — `drop-stale` drops an entry whose file is gone at every
  root or no longer matches the stored text — never the source.
- **Synthetic assistant notices:** `assistant` lines with `message.model == "<synthetic>"` are Claude
  Code's own client-side rows, never sent to the API: `isApiErrorMessage:true` + `error`
  (`invalid_request`/`rate_limit`/…) for "Prompt is too long" (with `errorDetails` ONLY when it came from
  the API — the client-side context gate emits it with none), and `isApiErrorMessage:false` for "No
  response requested." (resume preamble). Not model turns: cv re-roles them `Role::System` with
  `kind: error` (when `isApiErrorMessage`) or `kind: notice`, `origin: harness`, and keeps
  `isApiErrorMessage`/`error`/`errorDetails`/`apiErrorStatus`/`requestId` in `extra["claude"]`. Under
  `ParseOptions::complete` the record keeps its wire `assistant` shape so it round-trips.
- **`usage.iterations[]`** (Fable-era): per-request entries `{type: message|fallback_message|
  advisor_message|compaction, input_tokens, output_tokens, cache_read_input_tokens,
  cache_creation_input_tokens}`. **Claude Code ≥ 2.1.277 reads the context size from the LAST
  non-advisor/compaction iteration** (falling back to the top-level counters), and its turn gate refuses
  client-side with a synthesized "Prompt is too long" when that + a byte estimate of everything after the
  record ≥ context window − min(max_output, 20k) − 3k. `cv prune` pins both — revive is **on by
  default** since 0.11.0; `--no-revive` preserves the original `usage` records byte-for-byte. A pinned
  record carries the count it replaced as a top-level `_cv_orig_ctx` (cv's own key; a later prune
  prices that record from it, `record::Facts::orig_ctx`).
- **Persisted tool outputs:** a too-large `tool_result` is replaced by the stub
  `<persisted-output>\nOutput too large (NNKB). Full output saved to: <session>/tool-results/<id>.txt\n\n
  Preview (first 2KB):\n…</persisted-output>` — the model saw only the stub; cv keeps it as content and
  records the path in `Block::ToolResult::details` as `persistedOutput: {path, size}`. Kimi Code
  spills the same way under the same `details.persistedOutput.path` key, so `cv cat <session>
  <tool_use_id>` (0.11.0's replacement for `prune --retrieve`) fetches the full body for either.
- **Threading:** every `user`/`assistant`/`attachment` line has `uuid` + `parentUuid` (null at root) →
  a linked list / DAG. `last-prompt.leafUuid` points at the tail.
- **Message line fields:** `message.role`, `message.content` (string for simple user msgs; array of blocks
  `{type: text|thinking|redacted_thinking|tool_use|image|document}` for assistant; `tool_result` blocks
  for tool returns). Also `cwd`, `gitBranch`, `version`, `sessionId`, `timestamp` (ISO-8601), `model`
  (assistant), `usage` (tokens), `requestId`, `message.{id,stop_reason}`.
- **Tool results:** carried on a `user` line as `content[].type=="tool_result"` (`tool_use_id`, `content`,
  `is_error`) plus a richer `toolUseResult` sidecar object (`structuredPatch`/`oldTodos`/`newTodos`/
  `stdout`/`stderr`/file contents…). The sidecar is a shared concept, so it rides on the block as
  **`Block::ToolResult::details`**, not in the harness bag, and only under `ParseOptions::extra` (it
  routinely dwarfs the visible transcript). cv's derived `persistedOutput` pointer is merged into the
  same object and stripped back out on emit; when the sidecar is not an object (Claude writes a bare
  error string for ~28% of them) the two are parked side by side under `details.toolUseResult`. A user
  line whose content is *only* tool_results becomes a `Role::Tool` message with `kind: tool_result`.
- **`system` records** (`subtype`): `compact_boundary` (with `compactMetadata`), `local_command` (slash
  commands — `content` wraps `<command-name>/foo</command-name>` + `<command-args>` for the invocation,
  or `<local-command-stdout>…</local-command-stdout>` for the output), `away_summary`, `api_error`
  (`level:error`), `scheduled_task_fire`/`informational` (small `content` notices), `model_refusal_fallback`
  (`content` + `{originalModel, fallbackModel, apiRefusalCategory, apiRefusalExplanation, trigger,
  retractedMessageUuids}` — a provider safety refusal that re-routed to another model), `stop_hook_summary`
  (no `content`; **the record of a Stop-hook firing** — `{hookCount, hookInfos:[{command,…}], hookErrors[],
  hasOutput, hookAdditionalContext, stopReason, toolUseID}`; cv synthesizes a `⛓ stop hook` line when there
  was output/error, else drops it), `scheduled_task_fire` (`/loop`/cron wakeups: `{taskId, cron, prompt,
  taskKind, cronKind}`, and for folded no-op streaks `{noOpStreak, streakStartedAt, foldedUuids[]}`),
  plus bodyless `turn_duration` (`{durationMs, messageCount, pendingBackgroundAgentCount}`) /
  `agents_killed`. Hook output for PreToolUse/PostToolUse/UserPromptSubmit/SessionStart is NOT a system
  record — it is an `attachment{type:hook_success}` (rendered when the hook printed something).
  The subtype is what fixes the IR `kind`/`origin`: `compact_boundary` → `compaction_boundary`,
  `api_error`/`model_refusal_fallback` → `kind: error`, `stop_hook_summary` → `notice`/`origin: hook`,
  `scheduled_task_fire` → `notice`/`origin: scheduler`, everything else → `notice`/`origin: harness`.
  Adding a subtype is a `formats/claude.toml` edit (`system.*`) as well as an adapter one.
- **Compaction** (`compact_boundary` → `kind: compaction_boundary`): `compactMetadata = {trigger:
  manual|auto, preTokens, durationMs, preservedSegment:{headUuid,anchorUuid,tailUuid},
  preservedMessages:{…}}`. The boundary is immediately
  followed by a `user` message `isCompactSummary:true` whose `parentUuid == boundary.uuid` and whose body
  is the **generated summary that seeds the next context window** (the lost pre-compaction context) —
  `kind: compaction_summary`, `origin: harness`. Shared code (`compaction.rs`, `doctor.rs`) keys off
  those two kinds, not off the Claude subtype, so every harness's compaction is found the same way. A
  session compacts repeatedly (the reference transcript: 11×).
- **Subagents (two tiers):**
  - *Directly-spawned* (`Agent`/`Task` tool): `<sessionId>/subagents/agent-<agentId>.jsonl` +
    `agent-<agentId>.meta.json` = `{agentType, description, toolUseId}`. **`toolUseId` links the child
    back to the exact `Agent`/`Task` tool_use in the parent transcript.** The child's records carry
    `isSidechain:true` + `agentId`; its *return value* is the last assistant text turn (surfaced in the
    parent's tool_result). In the IR the child session gets `lineage.parent` = the parent session id
    (read off the `<sessionId>/subagents/` path) and `lineage.spawned_by_tool_use` = that `toolUseId`;
    `agentType`/`description` stay in `extra["claude"]` as `agent_type`/`agent_description`.
  - *Workflow* (`Workflow` tool): TWO sidecar locations per run.
    - `<sessionId>/subagents/workflows/<wf_runId>/agent-<agentId>.jsonl` (+ meta
      `{agentType:"workflow-subagent"}`) **plus `journal.jsonl`** = the orchestrator's structured log:
      `{type:started|result, agentId, key, result}`. **`result` is the agent's real return value**, in one
      of two shapes: an object `{status, summary, …}` (status vocab is per-workflow:
      `done`/`partial`/`GREEN`/`proven`/`blocked`/`welded`/…) or a plain string (the whole freeform return).
    - `<sessionId>/workflows/wf_<runId>.json` = the **run STATE** (first-class): `{workflowName, status
      (completed|killed), summary, error, defaultModel, agentCount, totalTokens, totalToolCalls, durationMs,
      scriptPath, script (inline), phases:[{title,detail}], workflowProgress:[…]}`. `workflowProgress`
      interleaves `{type:workflow_phase, index, title}` headers with `{type:workflow_agent, index, label,
      phaseIndex, agentId, model, state (done|error|progress|start), tokens, toolCalls, durationMs,
      promptPreview, resultPreview, error, cached}` — the phase tree with every agent's telemetry+outcome
      under its phase. The driving script also lives at `<sessionId>/workflows/scripts/<name>-wf_<runId>.js`.
- **Attribution:** assistant/user turns may carry `attributionAgent` / `attributionMcpServer` /
  `attributionMcpTool` / `attributionSkill` (MCP tool_use `name`s are `mcp__<server>__<tool>`; skill
  invocations use the `Skill` tool). `attachment` records carry side-band UI deltas (`deferred_tools_delta`
  with `addedNames`/`removedNames`, etc.) — non-conversational.
- **Indexes/sidecars:** `~/.claude/history.jsonl` (`{display, pastedContents, timestamp(ms), project,
  sessionId}`), `~/.claude/sessions/<pid>.json` (live registry: `{pid, sessionId, cwd, startedAt, version,
  kind: interactive|…, entrypoint, name, nameSource: derived|…, status: busy|idle, updatedAt,
  messagingSocketPath}` — a session's derived name and liveness), `~/.claude/tasks/<sessionId>/…`,
  `<sessionId>/tool-results/<id>.txt` (persisted large tool-result bodies, see above). Debug logs
  (`~/.claude/debug/<sessionId>.txt`) exist only when run with `--debug`. The installed binary
  (`~/.local/share/claude/versions/<v>`, Bun-compiled Mach-O) has its JS in plaintext — `grep -a -b -o`
  + `dd` reads any behavior; older versions stay in that dir, which dates a change.
- **2.1.296 (corpus `crates/cv-core/tests/corpus/claude/2.1.296/`, generated headless):** sub-agents
  launch async by default (the `Agent` tool_result is "Async agent launched successfully"; the return
  arrives as a `<task-notification>` user turn). Their `meta.json` adds `spawnDepth`, `requestShape`
  (`background`/`foreground`), `requestNonInteractive`, and `workflowPhase` for workflow agents. A
  notification turn is `origin{kind:"task-notification", producer, runId}`, `promptSource:"system"`,
  `turnOrigin:"task_notification"`, and its body adds `<output-file>`, `<result>`, `<usage>`. A
  `ScheduleWakeup` fire is a `user` turn with `isMeta`, `turnOrigin:"scheduled"` and no
  `scheduledTaskId` (cv reads it as `origin: scheduler`). `--fork-session` copies the records under the
  new `sessionId` with no pointer back; the shared `uuid`s are the only link. The copy starts at the
  source's last `compact_boundary` (or its root) and comes after the fork's own head records
  (`custom-title`, `queue-operation` for the new prompt), which are newer than it; cv reads that head as
  a fork and takes `lineage.forked_from` from the sibling transcript holding the copied `uuid`. New bookkeeping records:
  `file-history-delta` and `worktree-state` (with `relocated` on EnterWorktree/ExitWorktree); new
  attachments `plan_mode`, `structured_output`, `hook_additional_context`. Assistant turns add
  `requestedModel`, `thinkingDisplay`, `thinkingDurationMs`, `perTurnEffort`; user turns add
  `turnPosition{promptIndex, turnIndex}`. Outside `projects/`: `tasks/<sessionId>/<n>.json`
  (TaskCreate), `file-history/<sessionId>/<hash>@v<n>` (checkpoint backups), and background-task output
  and pasted images under `$CLAUDE_CODE_TMPDIR` (default `/tmp/claude-<uid>/<encoded-cwd>/<sessionId>/`).
- **2.1.304 (corpus `crates/cv-core/tests/corpus/claude/2.1.304/`, generated headless):** the **request
  ledger** (since 2.1.300, "transcripts record each API request's system prompt, tools and parameters"):
  every API call leaves three bookkeeping records — `api-request` `{id, timestamp, version, querySource,
  shapeHash, params{model, betas, max_tokens, thinking, context_management, output_config}, base?, keep,
  tail[], cacheControl[{path, value}], requestDigest, agentId?}`, `api-request-shape` `{shapeHash,
  shape{system[], tools[]}}` (the system prompt and every tool schema, one record per distinct shape) and
  `api-request-blob` `{hash, message}` (one request message, content-addressed; no `version`). A request's
  messages are the `base` request's first `keep` messages followed by the `tail` blobs, so a transcript
  reproduces every request byte-for-byte; `assistant` turns carry `requestRef` = the `api-request.id`.
  It nearly doubles a transcript and no flag turns it off. cv carries all three under `complete` (a
  blob's `message` is a request body, never a turn) and drops them otherwise. **Fork sub-agents**
  (`subagent_type: fork`, on by default in `-p` since 2.1.302): the child transcript opens with
  `fork-context-ref` `{agentId, parentSessionId, parentLastUuid, contextLength}` — its context is the
  parent up to that uuid — and its meta.json adds `agentType: "fork", isFork, model, finished`; cv sets
  `lineage.forked_from` and `extra.claude.fork_context{parent_last_uuid, context_length}`. **Retried
  requests** (2.1.302+): `system/api_error` with `source: "request_retry"`, no `content`, `error{message,
  status, requestId, formatted, connection, isNetworkDown, rateLimits, noResponse}`, `retryAttempt`,
  `maxRetries`, `retryInMs`, one per attempt (a rate-limited session wrote 1,143); the synthetic assistant
  after the last failure may add `apiErrorIsTransient`. cv renders a retry as `kind: error` from
  `error.formatted` (else `message`) plus `(retry N of M)`. New bookkeeping: `isolation-latch` `{side}`
  (no `version`; interactive sessions), attachment `max_turns_reached` `{maxTurns, turnCount,
  exitCommitted}` (`--max-turns`; never rendered) and `invoked_skills`; new fields:
  `agent_listing_delta.builtInPluginTypes`, `skill_listing.digests`, `deferred_tools_delta.loadedNames`,
  `hook_*.agentId`, `structured_output.{agentId, slug}`, user `mcpMeta`, task notifications
  `joinedNoticeRunIds/joinedNoticeLengths/joinedNoticeOwn`, `turn_duration.pendingWorkflowCount`. A
  third-party server tool (Z.ai's `webReader` behind `WebSearch`) leaves `server_tool_use` and
  `tool_result` blocks inside an `assistant` message. The binary's record roster also names
  `ended-by-model`, `agent-color`, `agent-setting`, `bridge-session`, `history-suppression`,
  `attribution-snapshot`, `dev-mods`, `memory-mode`, `observer-ref` (none observed): every unknown type,
  with or without a `message`, is carried under `complete`. A transcript holding only `ai-title` /
  `agent-name` records has no `version` anywhere; `cv doctor` counts it as an unknown writer version.
- **cv surfacing:** `cv show <id> --tree` appends the sub-agent forest (direct + per-workflow, with journaled
  outcomes); `cv show <id> --subagents` lists every agent with its return; `cv show <id> --agent <aid>`
  renders one sub-agent transcript; `cv events <id> --subagents` extracts the whole forest's tool activity,
  attributed per agent. **`cv workflow <id> [<runId>]`** renders a run's phase tree → agents → outcomes (+
  `--script`); **`cv tools <id>`** is the cross-agent tool-analytics surface (aggregate / `--agent` /
  `--tool` / `--workflow` / `--across` / `--timeline`); **`cv show <id> --kind compaction_boundary`** lists every boundary +
  summary (`--summaries` for full text); **`cv show <id> --pre-compaction <N>`** reads the lost pre-span.

## Codex CLI — `~/.codex/`

Ground truth: `codex-rs/history/src/{lib,rollout_payload}.rs` (RolloutItem/RolloutLine),
`codex-rs/rollout/src/policy.rs` (what is persisted), `codex-rs/protocol/src/{protocol,models,items}.rs`.
Re-verified 2026-09-19 at `132c2be23` (CLI 0.154); 2026-10-11 against the `rust-v0.161.0` and
`rust-v0.162.1` tags, with real rollouts from both in `crates/cv-core/tests/corpus/codex/`.

- **Transcripts:** `~/.codex/sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<thread_uuid>.jsonl` — the
  stamp is **local time, no zone suffix**; `RolloutFileName::parse` requires byte 19 to be `-` and skips
  any other name (so a `…T11-04-49Z-…` name is invisible to filename lookups). Reverted threads:
  `rollout-<ts>-<thread_id>_<rollout_id>.jsonl`. Files > 7 days may be compressed to `.jsonl.zst`
  (reader transparent); `.tmp` are staging files. Archived under `~/.codex/archived_sessions/`. **Legacy
  (2025):** single JSON file `{session, items[]}`.
- **Line envelope:** `{timestamp, ordinal?, type, payload}`. `ordinal` is written for paginated threads
  (the join key for `thread_history_1.sqlite` and for `subagent_history_start_ordinal`).
- **Two history modes** (`session_meta.history_mode`: `legacy` (absent ⇒ legacy, CLI ≤ 0.147) or
  `paginated` (0.147+; every thread since 2026-09)). In **paginated** mode the `event_msg`
  `user_message`/`agent_message`/`agent_reasoning`/`patch_apply_end`/`mcp_tool_call_end`/`web_search_end`/
  `context_compacted` twins are NOT persisted; instead every `TurnItem` arrives as `event_msg
  item_completed {thread_id, turn_id, item, started_at_ms?, completed_at_ms}`. A background
  legacy→paginated migration (`legacy_to_paginated_v1`) rewrites legacy files IN PLACE (bytes/offsets
  change).
- **`session_meta` payload:** `id`, `session_id` (root thread), `parent_thread_id`, `forked_from_id`,
  `forked_from_ordinal_exclusive`, `timestamp`, `cwd` (REQUIRED), `originator`, `cli_version`,
  `source` (string `cli|vscode|exec|mcp` OR object `{subagent:{thread_spawn:{parent_thread_id, depth,
  agent_path, agent_nickname, agent_role}}}` / `{subagent:"review"}` / `{custom:…}`), `thread_source`
  (`user|subagent|guardian_review|memory_consolidation|…`), `agent_nickname`, `agent_path`,
  `agent_role`, `model_provider` (copied into the `threads` row — required for resume), `base_instructions
  {text}`, `history_mode`, `history_base {thread_id, end_ordinal_exclusive, end_byte_offset}`,
  `subagent_history_start_ordinal`, `git {commit_hash, branch, repository_url}`, `memory_mode`,
  `context_window`, `creator_user_id`/`creator_account_id` (0.161+, ChatGPT ids; cv drops them). **Fork/subagent rollouts embed the parent's prefix** (ordinals below
  `subagent_history_start_ordinal`, with a second `session_meta` = the parent's on line 1) stamped with
  the FORK time; the real per-item time is `payload.internal_chat_message_metadata_passthrough
  .create_time` (epoch seconds). The FIRST `session_meta` is canonical.
  In the IR the swarm pointers are first-class **`Session::lineage`** — `parent_thread_id` →
  `lineage.parent`, `forked_from_id` → `lineage.forked_from`, `agent_path` → `lineage.agent_path` —
  and `base_instructions.text` → **`Session::system_prompt`**. The rest (`session_id`,
  `thread_source`, `agent_nickname`, `agent_role`, `source`, `model_provider`, `history_mode`,
  `subagent_history_start_ordinal`, `history_base`, `cli_version`, `originator`) stays in the
  session's `extra["codex"]` bag, never flat. Per-message Codex facts are nested the same way
  (`extra["codex"]["codex_event"]`, read through `harness_extra(Harness::Codex)`); reading such a
  key flat once silently disabled a check that depended on it.
- **`turn_context` payload:** REQUIRED `cwd`, `approval_policy` (`untrusted|on-request|never`),
  `sandbox_policy` (`{type: danger-full-access|read-only|workspace-write|external-sandbox, …}`),
  `model`, `summary` (`auto|concise|detailed|none`; optional from 0.162, still written); optional
  `turn_id`, `root_turn_id`, `effort`, `collaboration_mode {mode, settings{model, reasoning_effort}}`,
  `permission_profile`, and until 0.161 `personality`, `current_date`, `timezone`, `workspace_roots`
  (dropped in 0.162). A record missing a required field is skipped on resume; cv's emitter keeps
  writing `summary` because pre-0.162 Codex requires it.
- **`response_item.payload.type`:** `message` (`role`, `content[]` of `input_text|output_text|
  input_image`, `id: msg_…`, `phase`, `internal_chat_message_metadata_passthrough{turn_id,
  create_time}`), `agent_message` (inter-agent swarm channel: `{id: amsg_…, author, recipient,
  content:[{type:input_text,text}|{type:encrypted_content,…}]}`, always paired with a following top-level
  `inter_agent_communication_metadata {trigger_turn}` line), `reasoning` (`{id: rs_…, summary:[…],
  encrypted_content}` — `content` omitted whenever it holds `reasoning_text`), `function_call`
  (`{id: fc_…, call_id, name, namespace?, arguments (JSON string), encrypted_function_args?}` —
  `namespace` is the IR's `Block::ToolUse::namespace`, e.g. `collaboration` for the swarm tools),
  `function_call_output` (`{call_id, name?, namespace?, output}` where **`output` is a string or a
  content-item array — never an object; no persisted error bit**, so an output's error-ness survives
  a round-trip only through cv's own `"[error] "` string prefix, which `emit_codex` writes and the
  adapter reads back — and only when it is a bare string, never inside a content array),
  `custom_tool_call(_output)` (`input`
  is a freeform string), `local_shell_call`, `web_search_call` (`action: search|open_page|find_in_page`),
  `tool_search_call/_output`, `image_generation_call`, `compaction`/`context_compaction`,
  `configuration_update`, `additional_tools` (0.162+: a responses-lite tool-definition prefix,
  `{id: at_…, role: developer, tools[]}`, carried). `formats/codex.toml` `[types]` carries the full, checked list of
  `response_item.*` / `event_msg.*` / `item.*` names with cv's status for each (it names several the
  prose below does not, and marks the legacy `event_msg` twins `ignored`).
- **Other top-level types:** `compacted` (`{message (now usually ""), replacement_history[], window_number,
  window_id, first/previous_window_id, compaction_response_id, latest_token_usage_record,
  retained_context}`), `token_usage_record` (per response: `{thread_id, turn_id, session_id,
  root_turn_id, response_id, usage, turn_token_usage, thread_token_usage}`; `TokenUsage = {input_tokens,
  cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`
  → `Usage`, with `cached_input_tokens` → `cache_read_tokens`, `cache_write_input_tokens` (0.147+,
  absent on older files) → `cache_creation_tokens` and `reasoning_output_tokens` →
  `Usage::reasoning_tokens`. **Codex stores no cost**, so `Usage::cost_usd` is always null here),
  `inter_agent_communication_metadata`, `world_state` (embeds AGENTS.md text), `security_risk_score`,
  `retained_context`, `realtime_item`.
- **`event_msg.payload.type` (both modes):** `token_count {info{last_token_usage, total_token_usage},
  rate_limits}`, `task_started {turn_id, model_context_window, turn_attribution?, root_turn_id?}` (the last two
  0.162+), `task_complete {turn_id, last_agent_message, duration_ms, root_turn_id?}`, `thread_settings_applied {thread_settings{model, model_provider_id,
  reasoning_effort, personality, approval_policy, cwd}}` (the reliable model-change signal → one
  `kind: model_change` note per switch),
  `turn_aborted {turn_id, reason, error? (0.161+), root_turn_id? (0.162+)}`, `thread_goal_updated`, `thread_rolled_back {num_turns}` (→
  `kind: branch`: what follows does not continue what precedes),
  `item_completed` (paginated: all items; legacy: only FunctionCallOutput/Plan/Sleep/completed
  SubAgentActivity). `view_image_tool_call` is never persisted (images arrive as `item_completed
  ImageView{path:"file:///…"}`).
- **`item_completed.item` (`type`, PascalCase):** `UserMessage`, `AgentMessage{content:[{type:Text}],
  phase}`, `Reasoning{summary_text, raw_content}`, `CommandExecution{command[], cwd:"file:///…",
  parsed_cmd, source, status, aggregated_output?, exit_code?, duration}` (`stdout`/`stderr`/
  `formatted_output` until 0.161; 0.162 middle-truncates `aggregated_output` over 64 KiB),
  `FileChange{changes{path:{type:add|update|delete, content|unified_diff}}, status}`, `ImageView{path}`,
  `ImageGeneration{saved_path}`, `SubAgentActivity{kind: started|interacted|interrupted|completed,
  agent_thread_id, agent_path, model?, reasoning_effort?}` (the last two 0.162+) (`started` → IR `kind: subagent_spawn`, `completed` →
  `subagent_return`, anything else → `notice`), `CollabAgentToolCall{tool, status, sender_thread_id,
  receiver_thread_ids, prompt?, model?}`, `McpToolCall{server, tool, arguments, status, result?, error?}`,
  `WebSearch{query, action, results?}`, `ContextCompaction`, `Plan{text}`, `FunctionCallOutput`,
  `Extension{kind}`.
- **Index (sqlite):** `~/.codex/state_5.sqlite` `threads(id, rollout_path, cwd, source, thread_source,
  model_provider, model, reasoning_effort, history_mode, cli_version, title, preview,
  first_user_message, name, agent_nickname/role/path, archived, git_sha/branch/origin_url, tokens_used,
  is_pinned, project_id, thread_section_id, originator, …)` (54 migrations) + `thread_spawn_edges
  (parent_thread_id, child_thread_id, status)`; rows are seeded from `session_meta`, `model` filled at
  runtime. `thread_history_1.sqlite` (`thread_turns`, `thread_items`, projection state keyed by rollout
  ordinal / byte offset) is a PROJECTION of the rollouts — the JSONL stays authoritative.
  `session_index.jsonl` (`{id, thread_name, updated_at}`) and `history.jsonl` (`{session_id, ts, text}`)
  are unchanged.

## Grok CLI — `~/.grok/`

- **Transcripts:** `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/` — a *directory* per session
  containing `chat_history.jsonl`, `events.jsonl`, `updates.jsonl`, `summary.json`, `system_prompt.txt`.
- **cwd encoding:** percent-encoding of the absolute cwd (`%2F` = `/`). Reversible.
- **session id:** UUIDv7. **chat_history.jsonl line:** `{type: system|user|assistant, content}` where
  user `content` is `[{type:text,text}]`, assistant `content` is a string and carries
  `reasoning{text,encrypted,id}`, `model_id`, `model_fingerprint`. A `system` line is
  `kind: system_prompt`.
- **summary.json:** `{info{id,cwd}, created_at, updated_at, num_messages, current_model_id, git_root_dir,
  git_remotes[], head_commit, head_branch, agent_name, ...}`, plus the sub-agent/lineage facts.
- **`system_prompt.txt`** is byte-identical to the leading `system` transcript record; cv reads it as
  **`Session::system_prompt`** (a session fact, never a message).
- **1.0.50 (corpus `crates/cv-core/tests/corpus/grok/1.0.50/`, headless, BYOK model):** `$GROK_HOME`
  overrides `~/.grok`. Reasoning is its own `chat_history` record `{type: reasoning, id, summary:
  [{type: summary_text, text}], encrypted_content?}` before the assistant line (cv joins it to that
  turn). User lines carry `prompt_index`; `/compact` rewrites `chat_history` in place (the summary and
  the re-sent `<user_info>` block are both `synthetic_reason: compaction_meta`) and leaves
  `compaction/{INDEX.md,segment_NNN.md}`, `compaction_checkpoints/<id>.json`,
  `compaction_requests/<id>.json`. A fork's `summary.json` has `parent_session_id` + `forked_at`; a
  sub-agent is a normal session (`session_kind: subagent`) whose parent keeps
  `subagents/<child>/{meta.json,output.json}` (`parent_session_id`, `subagent_type`, `prompt`,
  `status`, `tool_call_id`, `workflow_run_id`). Workflows (Rhai) leave
  `workflows/wf_<run>/{state.json,journal.jsonl,script.rhai,args.json,scripts/NNNN.rhai}`. Also per
  session: `events.jsonl` (`turn_started`/`phase_changed`/`turn_ended` telemetry), `usage.json` and
  `signals.json` (session token totals), `plan.json` (todo_write), `plan_mode.json`,
  `rewind_points.jsonl`, `prompt_context.json`, `tool_definitions.json`, `resources_state.json`,
  `announcement_state.json`, `terminal/` (background command output). `updates.jsonl` adds
  `hook_execution` (`{event_name, runs: [{name, status: {status, elapsed_ms}}]}`, no output; cv: `hook`/`run`
  rows), `background_tasks`, `task_backgrounded`/`task_completed`,
  `scheduled_task_created`/`deleted`, `subagent_spawned`/`finished`, `workflow_updated`,
  `auto_compact_completed`, `compaction_checkpoint`, `current_mode_update`, `plan`, `turn_completed`.
- **Indexes (sqlite):** `~/.grok/sessions/session_search.sqlite` (FTS5: `session_docs` + `session_docs_fts`
  over title+content), `~/.grok/worktrees.db` (`worktrees(id,path,session_id,...)`), and per-cwd
  `prompt_history.jsonl` (`{timestamp,session_id,prompt,is_bash}`).

## OpenCode — `~/.local/share/opencode/` (NOT `~/.opencode`, which is plugins/cache)

Re-verified 2026-09-19 at `fee476bb` (1.18.31); 2026-10-11 against the `v1.18.20` and `v2.0.25` tags, with
real stores from both (and 2.0.25's migration of the 1.18.20 store) in `crates/cv-core/tests/corpus/opencode/`.
Ground truth: `packages/core/src/session/sql.ts` (tables), `packages/schema/src/v1/session.ts` (1.x
Info/Part shapes), `packages/schema/src/session-message.ts` (2.x message union),
`packages/core/src/database/v1-migration.bun.ts` (the 1.x → 2.x import), `packages/cli/src/database-path.ts`.

- **2.x (npm `@opencode/cli`, not `opencode-ai`) replaced the message model.** Same file, same path rules
  (`OPENCODE_DB`, channel dbs). New tables: `session_v2` (the 1.x `session` columns plus `fork_session_id`,
  `fork_boundary`, `time_idle`, `time_viewed`, `idle_outcome`, `time_suspended`, `resume_attempts`) and
  `session_message(id, session_id, type, seq, time_created, time_updated, data)`: one row per event,
  ordered by `seq`, `data` = the `Session.Message.Info` JSON minus `id`/`type`. `type ∈ {user, synthetic,
  system, skill, shell, assistant, compaction, model-switched, agent-switched, location-switched, idle}`.
  `user` = `{text, files[{data (base64), mime, source{inline|uri}, name}], agents[], skills[]}`;
  `assistant` = `{agent, model{id, providerID, variant?}, content[text | reasoning{text, state} |
  tool{id, name, state{status: streaming|running|completed|error, input, content[text|file]?, error{type,
  message}?, metadata?}, time}], finish, rawFinish, cost, tokens, error{type, message}?, snapshot, time}`;
  `compaction` = one row `{status: running|completed|failed, reason: auto|manual, summary, recent}`;
  `shell` = a user `!command` `{shellID, command, status, exit, output{output, cursor, size}}`; `idle`
  ends a turn `{outcome}`. Tool names changed: `bash`→`shell`, `task`→`subagent` (now with
  `background`), `apply_patch`→`patch`; `todowrite`/`todo` are gone; `question`, `websearch` and
  `execute` (code mode) are new. A fresh 2.x db has no `session`/`message`/`part`/`todo` tables; also
  `session_inbox`, `session_pending`, `instruction_{entry,blob,state}`, `worktree`, `kv` (the models.dev
  catalog, ~6 MB). Errors are `{type, message, status?, response?}` with `type` like `provider.invalid-request`, `aborted`,
  `tool.execution`. Usage and cost stay on the assistant row.
- **2.x migrates a 1.x store in place, on first server start** (`V1Migration`, status at
  `GET /api/experimental/migration/v1`, progress in `kv["migration.v1-v2"]`): it empties `event`, then copies
  every `session` into `session_v2` and rebuilds its messages into `session_message` under the same ids. The
  legacy tables are left as they were. The import drops step/snapshot/patch parts (kept as
  `assistant.snapshot`), folds a compaction pair into one `compaction` row, turns synthetic text into
  `synthetic` rows, and appends a `system` row telling the model about renamed tools. cv reads `session_v2`
  first and falls back to `session` for ids it lacks, so a migrated db shows each session once, as 2.x does.
- **1.18 already ships the 2.x tables empty** (`session_message`, `session_input`, `session_context_epoch`,
  with `session_message` keyed to `session`); only a `session_v2` table marks a 2.x store.

- **The canonical store is SQLite:** `opencode.db` in the data dir (`$XDG_DATA_HOME`-aware; `$OPENCODE_DB` may
  point elsewhere — `:memory:`, absolute, or relative to the data dir; non-default channels use
  `opencode-<channel>.db`), WAL mode. Tables `session(id, project_id, parent_id, directory, path, title, version,
  agent, model{id, providerID, variant}, cost, tokens_input/output/reasoning/cache_read/cache_write,
  summary_additions/deletions/files, share_url, revert, time_created, time_updated, time_archived, metadata?
  (json; added 2026-05-30 — probe `PRAGMA table_info`))`, `message(id, session_id, time_created, data)`,
  `part(id, message_id, data)`, `todo`, `session_share`. `data` is the Info/Part JSON MINUS `id`/`sessionID`/
  `messageID` — re-hydrate as `{...data, id, sessionID, messageID}`. Message `data`: `{role, time{created,
  completed}, parentID, modelID, providerID, mode, agent, path{cwd, root}, cost, tokens{input, output, reasoning,
  cache{read, write}}, finish, error?}` with `error.name ∈ {ProviderAuthError, UnknownError,
  MessageOutputLengthError, MessageAbortedError, StructuredOutputError, ContextOverflowError, ContentFilterError,
  APIError}`. Part union unchanged (`text reasoning tool file agent subtask patch snapshot step-start step-finish
  retry compaction`); tool `state` is `pending | running | completed{input, output, title, metadata, time,
  attachments?: FilePart[]} | error`. The message's `tokens{input, output, reasoning, cache{read, write}}`
  and `cost` fill `Usage` whole, including **`reasoning_tokens`** (dropped when 0) and **`cost_usd`** —
  OpenCode is one of the three harnesses that persist a provider cost (with Goose and OpenClaw). A
  `system`-role message is `kind: system_prompt` and the first one also becomes
  `Session::system_prompt`. `formats/opencode.toml` `[types]` is the checked part/table/error vocabulary.
- **The JSON tree is dead:** `storage/{session,message,part}/**.json` was only the input of a one-shot importer,
  deleted 2026-06-02 (`ca2acc4f`); anything left on disk is a pre-2026-01 leftover. Read it only as a fallback
  when no db exists. Also `~/.local/state/opencode/` and `prompt-history.jsonl`. `opencode export [sid]` writes
  `{info, messages:[{info, parts}]}` (an importable interchange form).

## Gemini CLI — `~/.gemini/tmp/<projectHash>/chats/`

Re-verified 2026-09-19 at `cfbcaa8` (the checkout's `package.json` reports
`0.62.0-nightly.20260918`; the newest tag there is `v0.49.0-preview.0`): **the record format is
unchanged** — `packages/core/src/services/chatRecordingTypes.ts` and `packages/core/src/core/logger.ts`
were last touched 2026-05-29, which is also the first commit in that checkout's history, so "unchanged"
here means "unchanged across everything we have", not "unchanged since the file was written".

- **Readable fallback:** `~/.gemini/tmp/<hash>/logs.json` — array of `{sessionId, messageId, type, message,
  timestamp}` (user messages reliably; assistant inconsistently).
- Note: open-source `gemini-cli` (cloned at `~/pug/gemini-cli`) uses JSON checkpoints, a *different* format
  from Google Antigravity (`antigravity`, below) — consult its `packages/cli/src/utils/sessions.ts` for that variant.
- Also handled now: the gemini-cli JSON **chat recordings** (`~/.gemini/tmp/<projectHash>/chats/session-*.json`
  legacy object + modern append-only `.jsonl` with `$set`/`$rewindTo`) and `checkpoint-*.json` — far richer
  than logs.json (real assistant turns, thoughts, tool calls). Qwen Code reuses this format under `~/.qwen/`.
- **Second storage root:** under macOS Seatbelt (`SANDBOX=sandbox-exec`) the runtime dir is `~/.cache/.gemini`,
  so recordings land in `~/.cache/.gemini/tmp/<project>/chats/…` (and `history/`) — scan both roots, dedupe by
  `sessionId`. `chats/` may hold `<file>.unreadable-<epochms>` and `<file>.tmp-<pid>` siblings (a corrupt
  recording, a rewrite in progress) — skip them. Empty, non-resumable recordings are deleted by the CLI.
- **cwd:** `tmp/<projectIdentifier>` is a short id; the real path lives in `~/.gemini/projects.json`
  (`{"projects": {"/abs/path": "<id>"}}`) or `~/.gemini/history/<id>/.project_root`; `directories[]` inside a
  recording is often absent. The runtime root is found by walking a session path
  (`<runtime>/tmp/<id>/chats/<file>`) **right to left** for the component named `tmp` — the innermost
  one is the runtime's own. Left to right breaks on every box whose runtime root itself sits under a
  `tmp` directory (`/tmp` is `std::env::temp_dir()` on Linux), and every cwd comes back `None`.
- **Record vocabulary:** `formats/gemini.toml` `[types]` is the checked list — the record kinds
  (`user`, `gemini`/`model`/`assistant`, `info`, `error`, `warning`, `system`, `rewind`), the
  append-only controls (`$set`, `$rewindTo`) and the parts (`text`, `thought`, `functionCall`,
  `functionResponse`, `inlineData`, `fileData`). Qwen's gemini-cli-shaped `~/.qwen/tmp` store
  delegates to this parser.
- **Re-verified 2026-10-11 against 0.46.0 and 0.63.0** (corpus `tests/corpus/gemini/<v>/`; source diff
  v0.46.0 → cfbcaa8 → v0.63.0: `chatRecordingTypes.ts` unchanged; 0.63.0 adds the
  `session-<ts>-<n>-<id8>.jsonl` same-minute collision name). What real recordings hold that a reader must
  survive, in both versions:
  - every tool response is ALSO a `user` record of bare `functionResponse` parts (the wire turn), beside
    `toolCalls[].result` on the `gemini` record: one result, recorded twice;
  - each resume appends a `$set.messages` re-synced from the API history (`updateMessagesFromHistory`):
    `gemini` entries then carry their thoughts and calls twice (`thought`/`functionCall` parts AND
    `thoughts[]`/`toolCalls[]`), 0.46.0 drops `model` from them, and the tool-response `user` turn can
    take the SAME `id` as the `gemini` turn that made the call (key by type + id, never id alone);
  - a resumed 0.46.0 process leaves a second `session-*.jsonl` with the same `sessionId` holding only the
    `<session_context>` preamble, and it is the newer file (0.63.0 deletes it);
  - a BeforeAgent hook's `additionalContext` is a trailing `<hook_context>` part of the prompt record;
    `displayContent` holds what was typed (`/command`, `@file`) when the model got something else;
  - an `invoke_agent` call's `toolCalls[].agentId` is the child recording `chats/<parent>/<agentId>.jsonl`;
  - compression (automatic; `/compress` is interactive-only) writes no record; `/chat save <tag>` writes
    `checkpoint-<tag>.json` whose first user Content merges the preamble and the prompt (0.63.0);
  - `tmp/<project>/<sessionId>/plans/*.md` (plan mode), `.../tracker/*.json` (task tracker) and
    `tmp/background-processes/background-<pid>.log` are written beside the recordings; cv reads none.

## Google Antigravity — `~/.gemini/{antigravity-cli,antigravity,antigravity-ide}/` (`$JETSKI_APP_DATA_DIR` overrides)

Verified 2026-11-11 against Antigravity CLI 1.5.6 (`agy`, corpus `crates/cv-core/tests/corpus/antigravity/1.5.6/`)
and the Jetski / Cortex store shared by Antigravity CLI (`~/.gemini/antigravity-cli/`), Antigravity 2.0
(`~/.gemini/antigravity/`), and Antigravity IDE (`~/.gemini/antigravity-ide/`). Checked step/source/status
vocabulary: `formats/antigravity.toml`.

- **Transcript discovery & deduplication:** one directory per conversation UUID under `<appDataDir>/brain/<uuid>/`.
  Logs sit in `brain/<uuid>/.system_generated/logs/`: `transcript_full.jsonl` (rank 3, un-truncated) >
  `transcript.jsonl` (rank 2, fields over cap carry `truncated_fields[]`) > `overview.txt` (rank 1, legacy IDE
  NDJSON fallback). `logs/chunks/` holds partial streaming chunks and is skipped. `conversations/<uuid>.pb` and
  `implicit/*.pb` are AEAD-encrypted protobuf trajectories gated on OS Keychain secrets; cv never opens them or
  touches Keychain.
- **Cortex JSONL step records:** `{step_index, source, type, status, created_at, content?, thinking?, tool_calls?,
  input_tokens?, cache_read_tokens?, output_tokens?, error?, media?, truncated_fields?}`. `step_index` is
  monotonically increasing per file with gaps where internal Cortex steps (`EPHEMERAL_MESSAGE`, `MQUERY`,
  `FINISH`) are omitted from JSONL logs.
  - `USER_INPUT` (`USER_EXPLICIT`) → `user`/`prompt`/`human`; `<USER_REQUEST>` wrapper preserved in message body,
    peeled only when synthesizing a fallback title; `media[{mime_type, uri, inline_data?}]` → `Block::Image`.
  - `PLANNER_RESPONSE` (`MODEL`) → `assistant`/`reply`/`model` (`thinking` → `Block::Thinking`, `content` →
    `Block::Text`, `tool_calls[{name, args}]` → `Block::ToolUse` with derived id `cv:antigravity:<step>.<k>`,
    `Usage{input_tokens, cache_read_tokens, output_tokens}`).
  - `GENERIC` (`SYSTEM`) → `tool`/`tool_result`/`harness` paired FIFO against the preceding `PLANNER_RESPONSE`'s
    `tool_calls[]` (reset when a turn is cancelled short); `status ∈ {ERROR, INVALID}` sets `is_error = true`,
    `file:///…/.system_generated/steps/<n>/output.txt` populates `details.persistedOutput.path`, and an
    `invoke_subagent` result JSON `{"conversationId": "<child>"}` links the child session.
  - `SYSTEM_MESSAGE` (`SYSTEM`) → parsed via `[Message] … sender=<id> … content=…`: child `step_index == 0`
    from a parent UUID is `user`/`prompt`/`subagent` (setting `lineage.parent`); parent receipt from a known
    child UUID is `user`/`subagent_return`/`subagent`; `<uuid>/task-*` and timer wakeups are `scheduler`/
    `harness` notices.
  - `CHECKPOINT` (`SYSTEM`) → `compaction_boundary` followed by `compaction_summary` when `content` is non-empty.
  - `ERROR_MESSAGE` → `system`/`error`/`harness`.
- **Read-only sidecars:** `annotations/<uuid>.pbtxt` (`title`), `brain/<uuid>/.system_generated/subagents/<child>.json`
  (`{conversationId, spawnStepIndex, state, subagentDescriptor{typeName, role}, workspaceUris[]}`), and SQLite
  sidecars opened strictly read-only via `store::sqlite`: `conversation_summaries.db` (`conversation_summaries`
  title/preview, `workspace_uris`, `parent_conversation_id`, `agent_name`, `source`, `project_id`) and
  `conversations/<uuid>.db` (`gen_metadata` protobuf `1.19` model slug + packed field `2` step indices;
  `trajectory_metadata_blob` protobuf `1.1` workspace URI, `1.2` git root, `1.4` git branch, `2.1` created epoch).

## Hermes (Nous) — `~/.hermes/state.db` (SQLite, `SCHEMA_VERSION` 31 as of 0.21.6; `$HERMES_HOME` overrides; per-profile `profiles/<name>/state.db`)

Re-verified 2026-10-11 at tag `v0.21.6` (`818c13be1d`, schema 31), also diffed `v2026.8.31`
(`29112bef09`, schema 26, the Homebrew release) against the 2026-09-19 pin `6d8a8bebf7` (schema 30).
Both releases have a real-model corpus in `crates/cv-core/tests/corpus/hermes/` (generator:
`tools/harness-corpus/hermes/`, affordance map: its `AFFORDANCES.md`). The checked
column/`display_kind`/role vocabulary is `formats/hermes.toml`.

- `sessions` + `messages` tables (OpenAI-shaped rows; roles `user|assistant|tool|system`). Multimodal content
  uses a `\x00json:` sentinel prefix. Reasoning spans several columns (`reasoning`, `reasoning_content`,
  `reasoning_details`, `codex_reasoning_items`, `codex_message_items`).
- **Schema drift:** columns are added with `ALTER TABLE ADD COLUMN` against `SCHEMA_SQL`, so probe
  `PRAGMA table_info` and select only what exists. v16 → v30 added to `messages`: `active`, `compacted`,
  `_compressed_summary`, `effect_disposition`, `api_content`, `display_kind` (`hidden | steer | auto_continue |
  model_switch | async_delegation_complete | process_complete | internal_notification`), `display_metadata`,
  `display_identity`, `display_order`; to `sessions`: `cwd`, `git_branch`, `git_repo_root`, `session_key`,
  `display_name`, `origin_json` (foreign imports: `imported_from{tool: claude-code|codex-cli, path,
  foreign_session_id}`), `system_prompt_hash`, `title_source`, `last_activity_at`, `profile_name`,
  `transport_profile`, `pinned`, `hidden`, `tool_names`, …
- **Row visibility is load-bearing:** compaction happens IN PLACE under one session id — old rows get
  `active=0, compacted=1`, the summary row has `_compressed_summary=1` (`display_kind='hidden'`), the carried
  tail is cloned to fresh ids and the originals get `active=0, compacted=0` (rewound rows likewise). Hermes
  shows `active = 1 OR compacted = 1` and sends `active = 1`; read with that filter or every carried message
  appears twice. **Order by `id`**, not `timestamp` (timestamps are not monotonic; tool-call adjacency breaks).
- **System prompt** moved (v25) to `system_prompts(hash, prompt)`; `sessions.system_prompt` is NULL —
  `COALESCE(sp.prompt, s.system_prompt)` via `LEFT JOIN system_prompts sp ON sp.hash = s.system_prompt_hash`.
  That is **`Session::system_prompt`**; a `system` row in `messages` is `kind: system_prompt`.
- **Lineage:** compression chains link via `parent_session_id` (parent `end_reason='compression'`); branch /
  reset / delegate children are marked in `model_config` JSON (`_branched_from`, `_reset_from`,
  `_delegate_from`) — an explicit branch is its own conversation and delegate children are sub-agent runs;
  Hermes's own listing hides `archived=1 OR hidden=1`, compression continuations and delegate children.
  In the IR: `_branched_from` → `lineage.forked_from`, `_delegate_from` → `lineage.parent`, a
  compression rotation → `lineage.continues` / `lineage.continued_in`. `_reset_from` (a `/new` with
  nothing carried over) has no IR field and stays in `extra["hermes"]`, as does an unmarked
  `parent_session_id` whose rows were not merged.
- **Imports:** `sessions.origin_json.imported_from{tool: claude-code|codex-cli, …}` marks a foreign
  transcript Hermes ingested; every message of such a session carries `origin: import`.
- **26 → 30 → 31** (2026.8.31 → pin → 0.21.6). `sessions`: + `compression_recovery_deadline`,
  `transport_profile`, `tool_names` (≤30); + `compression_overload_streak` (28), `created_source`
  (29, the immutable creating source; `source` is retaggable), `auto_archived` (30). `messages`: +
  `display_identity`, `display_order` (≤30); + `message_uid` (one durable id shared by every copy of
  a logical message: compaction generations, rotation children, branch copies), `absorbed_message_uids`
  (a merged user row's absorbed uids), `tool_call_uids` (assistant row: `{provider call id: uid}`)
  and `tool_call_uid` (tool row) — provider call ids repeat, these do not (31). New table
  `conversation_generations` (gateway cache scope); `async_delegations.origin_session_id`. cv carries
  the v29–v31 columns in `extra["hermes"]` (session bag for `created_source` / `auto_archived`).
- **`display_kind = 'failed_turn'`** (0.21): the assistant row Hermes writes to close a turn that got no
  reply ("Your request was not processed…" / "This turn did not complete…"). It is `kind: error`.
  The pin writes the same text untyped; Hermes's reader matches the exact text, and so does cv.
  2026.8.31 writes nothing (the user row is left unanswered).
- **`async_delegation_complete`** arrives as a **`user`**-role row (0.21.6 corpus): still a System
  `subagent_return`.
- Not rows of their own: a `/model` switch is a `[Note: model was just switched …]` prefix in the next
  user row's `api_content`; an image sent from one-shot mode is a `[Image attached at: <path>]` text
  line; an `@file:` reference is expanded inline under `--- Attached Context ---`. 0.21.6 one-shot
  sessions have `source = 'oneshot'` (was `cli`), and 0.21 defers most tools behind
  `tool_search` / `tool_describe` / `tool_call` (a deferred or MCP tool runs as `tool_call{name,
  arguments}`; only the tool row's `tool_name` names the real tool).

## OpenClaw — `$OPENCLAW_STATE_DIR` or `~/.openclaw/agents/<agentId>/`

Re-verified 2026-09-19 at `0e9181234a`; 2026-10-11 against releases 2026.1.29 (`77e703c69b`) and 2026.10.1
(`e178c39e9a`), whose real stores are the corpus in `crates/cv-core/tests/corpus/openclaw/`. Ground truth: `src/state/openclaw-agent-schema.sql`,
`src/state/openclaw-agent-db-contract.ts` (`OPENCLAW_AGENT_SCHEMA_VERSION = 21` — the schema version
is *there*, not in the `.sql`), `src/config/sessions/*`,
`src/agents/sessions/session-manager-types.ts`.

- **The live store is SQLite (since 2026-07-11, `0a8e3604ba`):** `agent/openclaw-agent.sqlite` (also
  `openclaw-agent.<agentId>.sqlite` / `.<n>.sqlite` for shared stores; `PRAGMA user_version` = 21), all tables
  `STRICT`: `transcript_events(session_id, seq, event_json, created_at)` — `event_json` is exactly the object
  that used to be a JSONL line (header + entries), ordered by `seq`; `session_windows(session_id, session_key,
  previous_session_id, reason: initial|reset|rollover|fork|rewind|switch|recovery|compaction, created_at,
  updated_at, status, model_provider, model, parent_session_key, spawned_by, display_name, …)`;
  `session_nodes(session_key, current_session_id, entry_json (the old sessions.json entry), label,
  display_name, parent_session_key, fork_source_session_id, pinned_at, archived_at, last_activity_at, …)`;
  archives `session_transcript_archives` (zstd blobs, reason deleted|reset), `session_transcript_cold_archives`,
  `trajectory_runtime_events`.
- **2026.10 compresses transcript payloads** (`OPENCLAW_AGENT_SCHEMA_VERSION = 24`,
  `src/config/sessions/transcript-payload.ts`): an event of at least 1 KiB is stored as a zstd frame (level 1,
  checksum) of its UTF-8 JSON in `transcript_events.event_zstd`, with `event_json` NULL, its size in
  `event_utf8_bytes`, and a tree-navigation projection in `navigation_json`. Smaller events stay in
  `event_json`. A reader that selects only `event_json` silently loses every large tool result. Also new:
  `session_entry_snapshots` (skillsSnapshot / systemPromptReport, moved out of `entry_json`),
  `session_reactions`, `session_nodes.snapshot_revision`.
- **Session keys** say what a session is: `agent:<id>:main`, `agent:<id>:explicit:<sid>` (2026.10 `--session-id`),
  `agent:<id>:subagent:<uuid>` (a `sessions_spawn` child; its spawner's key in `spawnedBy` /
  `session_windows.spawned_by`), `agent:<id>:cron:<job>[:run:<sid>]` (an isolated cron run). A key names the
  *current* session only: in 2026.1 a reset gives `agent:main:main` a new session id, so the spawner is pinned by
  the transcript whose `sessions_spawn` result records the child as `childSessionKey`. Sub-agent and cron
  registries: 2026.1 `subagents/runs.json`, `cron/jobs.json`, `cron/runs/<job>.jsonl`; 2026.10
  `state/openclaw.sqlite` `subagent_runs`, `cron_jobs`, `cron_run_receipts`. Both key by session key: a run by
  `requesterSessionKey`/`controllerSessionKey`, a job by its owner (2026.10 `owner.sessionKey`; a 2026.1 job
  names none), its run keys, and `sessionTarget: "main"` (cv: `subagent_spawn`/`state`, `schedule`/`state`).
- **User-turn provenance** (`src/sessions/input-provenance.ts`, absent in 2026.1): `message.provenance{kind:
  external_user|inter_session|internal_system, sourceSessionKey?, sourceTool?, sourceRole?, jobId?, runId?}`. A
  sub-agent's announce to its requester is `inter_session` from the sub-agent key; a cron run's prompt is
  `internal_system` with `sourceTool: "cron"`.
- **2026.10 nested tools:** the model sees a small tool surface (`tool_search`, `tool_describe`, `tool_call`, …)
  and reaches the rest through `tool_call`; the tool that actually ran is recorded as a `custom` message,
  `customType: "openclaw.nested-tool.v1"`, `details{toolName, input, result, parentToolCallId}`.
- **JSONL on disk is legacy or archive only:** pre-July `sessions/<sid>.jsonl` (+ `<sid>-topic-<id>.jsonl`,
  `sessions.json` index), cold-tier `<sha256>.jsonl.zst`, reset/delete archives `<sid>.jsonl.<reason>.<ts>[.zst]`,
  compaction checkpoints `<sid>.checkpoint.<uuid>.jsonl`, `*.trajectory.jsonl`, `*.migrated*`, `*.bak` — only the
  first kind is a session.
- **Transcript entries:** header `{type:"session", version:4 (min readable 3), id, timestamp, cwd,
  parentSession?}`, then canonical entries `message | thinking_level_change | model_change{provider, modelId} |
  compaction{summary, firstKeptEntryId, tokensBefore, details?} | reset{reason: new|reset|idle|daily|cron-stale,
  firstKeptEntryId?} | branch_summary | custom | custom_message{customType, content, display} | label{targetId,
  label} | session_info{name}`, plus branch controls `{type:"leaf", id, parentId, targetId, appendParentId?,
  appendMode?:"side"}` (any entry may carry `appendMode:"side"`): readers must follow the visible path, not file
  order. Roles user/assistant/toolResult + custom (bashExecution, branchSummary, compactionSummary, custom);
  blocks text(+textSignature), thinking(+thinkingSignature, redacted), toolCall(+async), image. Assistant
  messages add `responseId`, `turnId`, `endTurn`, `errorCode/errorType`, `usage.contextUsage`; secrets are always
  redacted at write time. ACP-bridged sessions are text-only echoes (`model:"acp-runtime"`).
- **IR mapping:** the entry type fixes the `kind` — `compaction` → `compaction_boundary` (its `summary`
  becomes a following `compaction_summary`), `reset` → `branch`, `model_change`/`thinking_level_change`
  → `model_change`, `branch_summary`/`custom_message` → `notice`; by role, `system` →
  `system_prompt` (and the first one fills `Session::system_prompt`), `bashExecution` →
  `injected_context`, `compactionSummary` → `compaction_summary`, an assistant message with
  `errorCode`/`errorType` → `kind: error`. A fork's `header.parentSession` is
  `Session::lineage.forked_from`; a sub-agent's spawner is `lineage.parent`, and the session key and spawner key
  are `extra.openclaw.{session_key, spawned_by}`. A cron run's prompts are `origin: scheduler`, a sub-agent's
  `origin: subagent`; a user turn's `provenance` (kept in `extra.openclaw.provenance`) maps `inter_session` from a
  sub-agent to `kind: subagent_return`, other `inter_session` to `origin: subagent`, `internal_system` cron to
  `origin: scheduler` and other `internal_system` to `injected_context`. A `nested-tool.v1` record is a `notice`
  with its details in `extra.openclaw.details`. Bag keys copied from a record keep their native camelCase
  (`stopReason`, `customType`, `error.errorCode`); only cv's own keys (`record_type`, `usage_raw`) are snake_case. `usage{input, output, cacheRead, cacheWrite, cost{total}}` fills
  `Usage` including **`cost_usd`**; OpenClaw reports no reasoning-token count.
  `formats/openclaw.toml` `[types]` is the checked entry/role/block vocabulary.

## Cursor IDE — `~/Library/Application Support/Cursor/User/` (mac; `%APPDATA%/Cursor/User` Win; `$XDG_CONFIG_HOME/Cursor/User` Linux)

- VS Code-derived; SQLite `state.vscdb` (`ItemTable` + `cursorDiskKV`; JSON values, TEXT or BLOB). Open READ-ONLY.
- **Global content:** `globalStorage/state.vscdb` → `cursorDiskKV`: `composerData:<id>` (one thread; metadata +
  either inline `conversation[]` (older `_v`) or `fullConversationHeadersOnly[]` pointers (newer)), and
  `bubbleId:<composerId>:<bubbleId>` (one message: `type` 1=user/2=assistant, `text`, `richText` Lexical,
  `thinking{text}`, `toolFormerData{name,rawArgs,params,result,error,status}`, `tokenCount`).
- **cwd:** per-workspace `workspaceStorage/<hash>/state.vscdb` `ItemTable.composer.composerData.allComposers[]`
  links composerIds → workspace; sibling `workspace.json` `folder:"file://…"` is the cwd. Legacy: in-workspace
  `workbench.panel.aichat.view.aichat.chatdata`. Closed-source & churny (`_v` 1..=10+) — skip the unknown.

## Cursor Agent CLI — `~/.cursor/chats/` and `acp-sessions/`

- `CURSOR_CONFIG_DIR` overrides the default; `XDG_CONFIG_HOME/cursor` is the next fallback.
  Both IDE and CLI sessions use `--harness cursor`; CLI IDs are prefixed `cli:`.
- Native 2026.10.01-e373342: SQLite `store.db`, `blobs(id TEXT PRIMARY KEY,data BLOB)` and
  `meta(key TEXT PRIMARY KEY,value TEXT)`. `meta["0"]` is hex-encoded JSON with `agentId`,
  `latestRootBlobId`, `name`, `createdAt`, optional `subagentInfo` and mode/approval metadata.
- The selected protobuf root references JSON model messages (field 1), ordered summary archives
  (field 13; legacy singular field 11), workspace URIs (9), AgentMode (10), and start time (26).
  Archive field 1 references older messages. Old root snapshots and unreferenced blobs are skipped.
  ACP sibling `meta.json` supplies cwd/title fallbacks. Missing references and malformed wire data error.
- JSON messages use `role` and string/part-array `content`: text, reasoning, tool-call (`args`),
  tool-result (`result`), image, file. `providerOptions.cursor.requestContextCompleteness` marks
  injected user context; `isSummary` marks compaction summaries. Tool IDs preserve call/result links.
- CLI AgentMode differs from IDE UnifiedMode: 0 unspecified, 1 agent, 2 ask, 3 plan, 4 debug,
  5 triage, 6 project, 7 multitask, 8 custom. Raw numbers survive unknown values. `subagentInfo`
  populates lineage. Per-message usage/timestamps are not present in the first native JSON capture;
  root context token details are not reported as generation usage.
- Discovery validates JSON with a count/title projection and uses the same semantic reducer as
  streaming. One SQLite snapshot and bounded byte staging replay one reachable message at a time;
  full JSON decoding retains a per-message allocation floor. See the native corpus and generator
  in `tools/harness-corpus/cursor/`; byte replacement alone breaks protobuf length prefixes.

## Desktop apps (not harnesses: nothing on disk cv can read)

Neither app is a `--harness` (they were, as `claude-app`/`chatgpt-app`, until cv removed two adapters whose
`discover()` could only ever return empty). What a look inside found, kept so nobody re-reverse-engineers them:

- **Claude app** — `~/Library/Application Support/Claude/` (Electron). Transcripts are **server-side** (claude.ai);
  local LevelDB/IndexedDB holds only auth/settings/UI state. `claude-code-sessions/*.json` are metadata stubs
  pointing (via `cliSessionId`) at `~/.claude/projects/` transcripts the Claude Code adapter already handles.
- **ChatGPT app** — `~/Library/Application Support/com.openai.chat/` (native AppKit, not Electron). Offline
  history is local as one **encrypted** file per conversation (`conversations-v3-<acct>/<uuid>.data`); the key is
  app-held (not in a readable Keychain item). We detect the install + count convos but cannot decrypt.

## Kimi — `~/.kimi-code` (Kimi Code, current) and `~/.kimi` (kimi-cli, frozen)

- **kimi-cli (`$KIMI_SHARE_DIR` or `~/.kimi`) is deprecated and frozen** — its format is unchanged since 2026-05
  (wire `protocol_version` 1.1–1.10) but nothing new is written once `~/.kimi/.migrated-to-kimi-code` exists.
  Sessions at `sessions/<md5(cwd)>/<uuid>/context.jsonl` (or legacy flat `<uuid>.jsonl`); cwd from
  `~/.kimi/kimi.json` `work_dirs[]`; roles `_system_prompt`/`user`/`assistant`/`tool` (+ `_checkpoint`/`_usage`);
  Parts `text`/`think`/`image_url`; tool calls carry a JSON-string `arguments`; `wire.jsonl` (`{timestamp,
  message:{type, payload}}`) enriches tool results + `StatusUpdate.token_usage{input_other, …}`.
  `/compact`, `/clear` and a rewind each rotate the live file out to `context_N.jsonl` (N counting
  up), so the session is every segment oldest first, then `context.jsonl`; each segment repeats the
  system prompt (a rewind also copies the kept prefix). cv reads them all, once each: a compaction
  segment opens with the `<system>Previous context has been compacted` summary (`compaction_boundary`
  + `compaction_summary`), any other rotation gets a synthetic `branch`. Sub-agents (`Agent` tool) live
  in `<session>/subagents/<agent_id>/` (`context.jsonl`, `wire.jsonl`, `meta.json{subagent_type,
  description, …}`, `prompt.txt`, `output`) and parse as child sessions; `tasks/<id>/` (background
  shells) and `notifications/<id>/` sit beside them, and `state.json` keeps `todos` (`[{title, status}]`, cv:
  `plan`/`state`), `plan_mode`, `approval`. 1.49+ (kosong 0.55) writes `{"type":"think","think":"","encrypted":null}` for every
  step that streamed no reasoning; cv drops those. 1.52.0, the final release, only prints a pointer
  to Kimi Code; 1.51.0 is the last version that writes a store (corpus: 1.48.0, 1.51.0).
- **Kimi Code (`$KIMI_CODE_HOME` or `~/.kimi-code`)** — a different format: `session_index.jsonl`
  (`{sessionId, sessionDir, workDir}` + `{sessionId, deleted:true}` tombstones); workspace dirs
  `sessions/wd_<slug>_<sha256(cwd)[:12]>/session_<uuid>/` with `state.json` v2 `{id, version:2, cwd, archived,
  agents{main, agent-N{type:"sub", parentAgentId}}, title, titleKind, isCustomTitle, lastPrompt, createdAt,
  updatedAt (ms), lastTurnReason}` (**cwd is stored**). The transcript is `agents/<id>/wire.jsonl`: header
  `{type:"metadata", protocol_version:"1.4"|"1.5", created_at:<ms>}` then FLAT records `{type, …, time:<ms>}`:
  `context.append_message` (user), `context.append_loop_event` with `event.type` ∈ `step.begin` /
  `content.part{part:{type: text|think}}` / `tool.call{toolCallId, name, args:<object>}` /
  `tool.result{toolCallId, result:{output, isError?, note?, truncated?}}` / `step.end{finishReason,
  usage{inputOther, output, inputCacheRead, inputCacheCreation}, messageId}` (group by `event.stepUuid`),
  `usage.record`, `llm.request`, `turn.prompt|ended|steer|cancel`, `profile.bind`,
  `context.apply_compaction{summary, compactedCount}`. Sidecars `agents/<id>/tool-results/<Tool>-<callId>-
  <uuid>.txt` (outputs > 50 000 chars, pointed at by `details.persistedOutput.path`), `media/`, `logs/`.
  That record list is **not exhaustive** and it moves fast — `formats/kimi-code.toml` names the
  checked set (including the `permission.*`, `tools.*`, `token_counting.*`, `swarm_mode.*`,
  `plan_mode.*`, `plugin.session_start`, `prompt.accepted`, `staleGuard.recorded` families that a
  real store is full of but which cv only carries), and `cv formats --harness kimi-code`
  shows what is actually on this machine.
- **Kimi Code IR mapping:** `profile.bind` is the system prompt (`kind: system_prompt`, and
  `Session::system_prompt`); `context.apply_compaction` is a `compaction_boundary` +
  `compaction_summary` pair; each sub-agent dir under `agents/` parses as its own `Session` with
  `lineage.parent` = the session uuid and `lineage.agent_path` = `agent-N` (`parentAgentId`/`labels`
  stay in `extra["kimi-code"]`). `context.undo{count}` (`/undo`) and `context.clear` are a `branch`
  of human origin. `/clear` in the TUI starts a NEW session (both 0.39 and 2.1).
- **Kimi Code 2.x (2.0.0, 2026-09-17; corpus 2.1.1)** keeps protocol 1.5 and the record shapes
  above, and adds: `subagent.spawned{subagentId, subagentName, parentToolCallId, description,
  swarmIndex?}` / `subagent.started` / `subagent.completed{resultSummary, usage}` on the parent's wire
  (cv: `subagent_spawn` / `subagent_return`); `kimi fork` (`state.json.forkedFrom` →
  `lineage.forked_from`, a `forked` record at the cut); `/undo` as `agent.switched{branch, reason:
  "undo", base{branch, line}}` + `context.undo` + `context.undone`; a second copy of every message as
  `agent.message.appended{kind, message, meta}`; `agent.turn.started/ended`,
  `prompt.completed/aborted/steered`, `turn.step.retrying/interrupted`; `file_history.tracked/checkpoint`
  with snapshots in `agents/<id>/file-history/`; `notify/state.json` per session and
  `sessions/.index-cache/scan.json`. Opening a 0.39 store with 2.1.1 (corpus
  `0.39.1-then-2.1.1`) rewrites one record in place, `plan.revision{path}` → `{sha256, bytes}`, and
  otherwise only appends.

## Qwen Code — `~/.qwen/`

- **Current (verified on 0.25.1, 2026-10-11; corpus `tests/corpus/qwen/0.25.1/`):** one JSONL per session at
  `~/.qwen/projects/<sanitized cwd>/chats/<sessionId>.jsonl` (`<sanitized cwd>` = every non-alphanumeric byte a
  dash; `$QWEN_HOME` moves `~/.qwen`, `$QWEN_RUNTIME_DIR` the runtime dirs), one `ChatRecord` per line
  (`services/chatRecordingService.ts`): `{uuid, parentUuid, sessionId, timestamp, type: user|assistant|tool_result|system,
  subtype?, provenance, cwd, version, gitBranch, message?: Gemini Content, model?, usageMetadata?, toolCallResult?,
  systemPayload?, forkedFrom?}`. The format is Claude-Code-shaped records carrying Gemini parts; it is NOT the
  gemini-cli recording any more.
  - Sub-agents: `projects/<cwd>/subagents/<parentSessionId>/agent-<agentId>.jsonl` (records with `isSidechain`,
    `agentId`) + `agent-<agentId>.meta.json` (`toolUseId`, `subagentName`, `parentSessionId`).
  - Deferred tools: most tools (agent, web_fetch, cron_*, MCP, goals) are loaded with `tool_search` and invoked as
    `tool_call {name, arguments}`; cv unwraps it to the named tool, namespace `tool_call`.
  - `user` records: `provenance: real_user` is a prompt (a `UserPromptSubmit` hook's context is a trailing
    `<qwen:user-prompt-submit-context>` part); `goal_runtime`, `notification`, `cron`, `agent_*` are injected.
  - `system` subtypes cv reads: `chat_compression` (`{info, compressedHistory}` → boundary + summary),
    `slash_command`, `at_command`, `goal_state`, `rewind`, `session_model`, `custom_title`, `parent_session`,
    `turn_result`. `ui_telemetry` (a third of the bytes), `attribution_snapshot`, `file_history_snapshot` and
    the session-settings echoes are ignored (`formats/qwen.toml`).
  - Beside the sessions: `tmp/<hash>/background-shells/<sessionId>/shell-*.{output,status}`,
    `tmp/<hash>/scheduled_tasks.json` (cron: `[{id, cron, prompt, recurring, createdAt, lastFiredAt?, sessionId?,
    runs?}]`; `<hash>` is the sha256 of the cwd; a job names its session only when one owns it, else cv gives
    it to the session live at `createdAt`), `~/.qwen/memories/` and `projects/<cwd>/memory/` (memory files).
- **Legacy (the fork's original layout):** gemini-cli-shaped `~/.qwen/tmp/<projectHash>/{logs.json,
  chats/session-*.json|jsonl, checkpoint-*.json}`. The adapter delegates these to the Gemini parser and re-tags
  `Harness::Qwen`.

## LM Studio — `~/.lmstudio/conversations/<ms-epoch>.conversation.json`

- Plaintext JSON, one file per chat (filename stem = id = `createdAt` ms). No cwd (chat app). `messages[]` =
  `{versions:[…], currentlySelected}` (every regenerated variant kept; read the selected one). `singleStep` (user)
  content parts `text`/`file`; `multiStep` (assistant) `steps[]` of `contentBlock` (`style.type=="thinking"` →
  reasoning; `genInfo` → model + tokens). No first-class tool calls (gpt-oss inline `<|channel|>` markers kept as
  text). Attachments are references; bytes under `~/.lmstudio/.internal/files/`.
- **Schema snapshot** (no corpus: the desktop app writes conversations, and `lms` has no throwaway-home mode to
  test whether `lms chat` is saved too): `tests/corpus/lmstudio/0.4.21+2/` records the merged JSON shape of the 54 files on the reference machine;
  `tools/harness-corpus/lmstudio/snapshot.sh --check` diffs a later one.

## Cline — VS Code extension (`<globalStorage>/saoudrizwan.claude-dev/`)

- Tasks at `<globalStorage>/saoudrizwan.claude-dev/tasks/<taskId>/` (also `~/.cline/tasks/`): `api_conversation_history.json`
  (a JSON **array of raw Anthropic Messages-API objects** — maps ~1:1 onto the Claude block model), `ui_messages.json`
  (UI events, timestamp enrichment), `task_metadata.json`. globalStorage roots: `<Editor>/User/globalStorage/` for
  `<Editor>` ∈ {Code, Code - Insiders, Cursor, VSCodium}. `<taskId>` ms-epoch → `created_at`. cwd from the first user
  msg's `<environment_details>`. A `user` turn of only `tool_result` blocks → a Tool turn. This is the **3.x
  extension's** store.
- **SDK store** (the npm `cline` CLI 3.x and the 4.x extension, both on `@cline/core`; verified at cli 3.0.70,
  `tests/corpus/cline/3.0.70`): `~/.cline/data/sessions/<sid>/` (`$CLINE_DIR`, `$CLINE_DATA_DIR`,
  `$CLINE_SESSION_DATA_DIR` override) with `<sid>.json` (the session row: cwd, model, provider, started_at,
  status, team_name, `metadata.title`/usage) and `<sid>.messages.json` (`{agent: "lead", sessionId, origin,
  messages[], system_prompt}`; messages are Anthropic-shaped with `id`, `ts` ms, `modelInfo`, `metrics`; a
  `tool_result` names its tool and may hold an array of plain result objects; `metadata.displayOnly` +
  `displayRole: "error"` marks a run error → `kind: error`). Children sit in the same dir:
  `agent_<uuid>.messages.json` (spawn_agent) and `<teammate>__<id>.messages.json` (agent teams), with
  `origin.parentThreadId` → `lineage.parent`; cv's id for them is `<sid>/<stem>`. `db/sessions.db` repeats the
  rows (plus `schedules`, `schedule_executions`, `subagent_spawn_queue`), `db/cron.db` the hub's `cron_specs` +
  `cron_runs`, and `db/teams.db` team state keyed `team_name = <lead session id>`. cv reads an execution's or a
  cron run's `session_id` (that session's `schedule`/`fire`) and `team_events` (the lead's `subagent_spawn`/`state`
  rows); a schedule names no session that made it. `~/.cline/tasks/` is now the SDK's task-SPEC dir, not transcripts.

## Roo Code — a Cline fork (`<globalStorage>/rooveterinaryinc.roo-cline|roo-code/`)

- Identical per-task layout/schema to Cline, different extension namespace (+ `~/.roo/tasks/`). Reuses the Cline parser,
  tagged `Harness::Roo`. The **Roo CLI** (GitHub `cli-v*` releases; verified at 0.1.17, `tests/corpus/roo/0.1.17`)
  runs the extension in a mock VS Code whose globalStorage is `~/.vscode-mock/global-storage/`, so its tasks are
  `~/.vscode-mock/global-storage/tasks/<uuid>/`.
- Roo 3.5x additions: a `history_item.json` per task (its task-history row: `parentTaskId` of a `new_task`
  child → `lineage.parent`, `rootTaskId`, `mode`, totals → `extra["roo"]`), `tasks/_index.json`, assistant
  `{type: "reasoning", text, summary}` blocks (→ thinking), and a non-destructive condense: the summary turn
  is `isSummary: true` + `condenseId` (→ `kind: compaction_summary`, preceded by a synthetic
  `compaction_boundary`), while every turn it replaced stays with `condenseParent` (kept, flagged
  `condensed_into`). `checkpoints/` per task is a shadow git repo (not read). Usage is only per task
  (`history_item.json`) and per request in `ui_messages.json` `api_req_started`; none is per message.

## Continue (continue.dev) — `~/.continue/sessions/` (`$CONTINUE_GLOBAL_DIR` overrides)

- Index `sessions.json` (`[{sessionId, title, dateCreated, workspaceDirectory}]`) + per-session `<id>.json`
  (`{sessionId, title, workspaceDirectory, history:[item]}`). Each item's `message` is OpenAI-shaped (role
  user/assistant/system/tool; content string or `[{type:text}/{type:imageUrl}]`; assistant `toolCalls[]`;
  `role:"tool"` + `toolCallId`). `contextItems[]` → File refs. cwd = `workspaceDirectory`.
- The **cn CLI** (npm `@continuedev/cli`; verified at 1.5.47, `tests/corpus/continue/1.5.47`) writes the same store
  but no `role:"tool"` items: an assistant item carries `toolCallStates[]` (`{toolCallId, toolCall, status:
  done|errored|…, parsedArgs, output:[{content, name, description}]}`), which cv emits as the Tool turn after it,
  and `message.usage` (OpenAI names + `model` + `cost_cents`) → `Usage`/model. A headless `/compact` is not saved.

## Goose (Block) — modern SQLite + legacy `.jsonl`

Re-verified 2026-09-19 at `2090ad1c` (1.51.0), and 2026-10-11 against the release tags `v1.51.0`
(`1a4249ac`) and `v1.54.0` (`cd643d21`), both schema 16, whose real stores are the corpus in
`crates/cv-core/tests/corpus/goose/` (affordance list: `tools/harness-corpus/goose/AFFORDANCES.md`).
Ground truth: `crates/goose/src/session/session_manager.rs` (schema + writers),
`crates/goose-provider-types/src/conversation/message.rs` (content blocks). Unreleased on main
(`3bd85200`): schema 17 adds `sessions.system_prompt_override`, `system_prompt_extras_json`,
`container_id`.

- Data dir: Linux `~/.local/share/goose/sessions/`; macOS `~/Library/Application Support/Block.block.goose/sessions/`;
  Windows `%APPDATA%\Block\goose\data\sessions\` (one `Block`; `$GOOSE_PATH_ROOT` (absolute only) /
  `$XDG_DATA_HOME` override). Modern: `sessions.db`, schema version in a `schema_version` table (`SELECT
  MAX(version)`; 16 today): `sessions(id, name (the title — `description` is never written), working_dir→cwd,
  created_at/updated_at 'YYYY-MM-DD HH:MM:SS', provider_name + model_config_json → model, session_type,
  parent_session_id (v15), cache_read/write_tokens + accumulated_* (v14), …)`, `messages(message_id, session_id,
  role[user|assistant], content_json, created_timestamp secs, tokens (never written), metadata_json)`,
  `usage_ledger(session_id, created_timestamp, model, input/output/total_tokens, cache_read/write_tokens, cost,
  cost_source, is_compaction)` (v15). Open READ-ONLY, PRAGMA-probe columns.
- `content_json` = array of `MessageContentBlock` (`type` camelCase): `text`, `image`, `document{data, mimeType,
  name?}` (2026-09), `toolRequest`, `toolResponse` (`toolResult` = `{status:"success", value}` |
  `{status:"error", error}`; `value` is an rmcp `CallToolResult{content[], structuredContent?, _meta?}` with
  snake_case blocks `text|image|audio|resource|resource_link`, or — legacy — a bare content array),
  `toolConfirmationRequest`, `actionRequired`, `thinking`, `redactedThinking`, `systemNotification`,
  `error{kind: authentication|contextLengthExceeded|creditsExhausted|other, message}` (2026-08);
  `frontendToolRequest` was removed 2026-08 (old rows may carry it). Tool results ride on `user` msgs → Tool turn.
- `metadata_json` (written on every insert): `{userVisible, agentVisible, inference{provider, requestedModel,
  resolvedModel, providerSessionId}, outputTokenLimitReached, steer, turnContext, usage{inputTokens,
  outputTokens, totalTokens, cacheReadTokens, cacheWriteTokens, cost, costSource, elapsedMs,
  timeToFirstTokenMs, isCompaction}, operations}` — the only per-message usage; Goose hides rows with
  `userVisible = 0`. `created_timestamp` is seconds (values > 10_000_000_000 are milliseconds). Legacy:
  per-session `<name>.jsonl` (header line + one message per line), unchanged.
- **Compaction, as written** (both releases): every earlier row's `agentVisible` is flipped to
  `false` in place, then three agent-only rows follow: the summary (user), a continuation note
  (assistant), and a copy of the prompt (user). None of them carries `usage`: `isCompaction` is set
  only on the `usage_ledger` row, and the "Exceeded auto-compact threshold" notice is not stored. A
  tool-pair summary is also an agent-only user row, but it hides only its own pair.
- **Sub-agents:** summon's `delegate` makes a child session (`session_type = sub_agent`,
  `parent_session_id`), and the parent's `toolResponse.value._meta.subagent_session_id` names it.
  A `--fork` copy stores no link to its source, but its messages keep their `message_id` and
  `created_timestamp`, so its first message is older than the session: cv names the earliest-created other
  session holding that `message_id` as `lineage.forked_from`. MCP tools are named `<extension>__<tool>`.
- **IR mapping:** `usage` → `Usage`, with `cost` → **`Usage::cost_usd`** (Goose reports no
  reasoning-token count). `userVisible = 0` is not a drop: the row becomes a `Role::System` turn —
  `kind: compaction_summary` for the agent-only user row with nothing agent-visible before it (or
  a `usage.isCompaction` row; cv synthesizes the paired `compaction_boundary` in front of it), else
  `kind: injected_context` — so turn counts and `cv show` stay honest. A `toolResponse`'s
  `structuredContent` and `_meta` ride verbatim in `ToolResult::details`. A row holding an `error` block is `kind: error`; a row holding only control
  blocks (`systemNotification`, `actionRequired`, `toolConfirmationRequest`) is `kind: notice`.
  `sessions.parent_session_id` (v15) → **`Session::lineage.parent`**. The checked block/action/column
  vocabulary, including the `session_type` values, is `formats/goose.toml`.

## Zed — `<data_dir>/threads/threads.db` (SQLite + zstd blobs)

- **Location:** macOS `~/Library/Application Support/Zed/threads/threads.db`; Linux
  `$XDG_DATA_HOME/zed/threads/threads.db` (default `~/.local/share/zed/…`; flatpak
  `~/.var/app/dev.zed.Zed/data/zed/…`); Windows `%LOCALAPPDATA%\Zed\threads\threads.db`. A sibling
  `threads-db.0.mdb/` LMDB dir is a dead pre-SQLite store (heed era) — empty on the reference machine, ignored.
- **Schema:** one table, grown by un-gated `ALTER TABLE`s (probe `PRAGMA table_info`; old DBs lack the tail):
  `threads(id TEXT PK, summary TEXT, updated_at TEXT, data_type TEXT, data BLOB` + later
  `parent_id, worktree_branch, folder_paths, folder_paths_order, created_at)`. Timestamps are RFC3339
  with offset. `folder_paths` = workspace folders, **lexicographically sorted and `\n`-joined**;
  `folder_paths_order` = `,`-joined indices restoring the user's original order (first ordered path =
  primary worktree). `parent_id` links a **subagent** thread to its parent, and we surface it as
  first-class **`Session::lineage.parent`** — the `parent_id` column wins over the 0.3.0 blob's
  `subagent_context.parent_thread_id` when both name one, and the blob's `depth` rides alongside as
  `extra["zed"]["subagent_depth"]`. The hierarchy itself is flattened: each thread is its own session.
- **Blob:** `data_type` `"zstd"` → one zstd frame (level 3, written by `zstd::encode_all`) of JSON;
  `"json"` → raw JSON (the code supports it; never observed). No message table — counting messages
  requires decoding the blob.
- **Blob JSON, three generations** (sniff by `version` + message shape; serde structs in
  zed `crates/agent/src/{db,legacy_thread,thread}.rs`):
  - **versionless** (oldest): `{summary, updated_at, messages:[{id:int, role, text, tool_uses, tool_results}]}` —
    plain `text` instead of segments.
  - **`0.1.0` / `0.2.0`** (agent1 `SerializedThread`): `{version, summary, updated_at,
    messages:[{id:int, role:"user"|"assistant"|"system", segments:[{type:"text"|"thinking"(+signature)
    |"RedactedThinking"(data)}], tool_uses:[{id,name,input}], tool_results:[{tool_use_id,is_error,
    content,output}], context:"…", creases:[…], is_hidden}], initial_project_snapshot,
    cumulative_token_usage, request_token_usage:[…], detailed_summary_state:{Generated:{text}},
    model:{provider,model}, completion_mode, tool_use_limit_reached, profile}`. In 0.1.0 `tool_results`
    rode on the **next user** message; 0.2.0 moved them onto the calling assistant message. Each agentic
    step is its **own assistant message** (a 235-"message" thread can be ~37 user turns). `context` is the
    rendered attached-files preamble; `creases` are editor fold ranges (UI-only).
  - **`0.3.0`** (agent2/ACP `DbThread`, flattened `version` added at save): `{version, title, messages,
    updated_at, detailed_summary, initial_project_snapshot, cumulative_token_usage,
    request_token_usage:{<user-msg-id>:usage}, model, profile, imported, subagent_context:
    {parent_thread_id, depth}, speed, thinking_enabled, thinking_effort, …}`. Messages are an
    **externally tagged enum**: `{"User":{id, content:[{"Text":…}|{"Mention":{uri,content}}|
    {"Image":{source,size}}]}}`, `{"Agent":{content:[{"Text":…}|{"Thinking":{text,signature}}|
    {"RedactedThinking":"…"}|{"ToolUse":{id,name,input,raw_input,…}}], tool_results:{<id>:
    {tool_use_id,tool_name,is_error,content,output}}, reasoning_details}}`, bare `"Resume"`, or
    `{"Compaction":{"Summary":"…"}}` (→ `kind: compaction_summary`; bare `"Resume"` carries no
    content and is dropped). Tool-result `content` is `{"Text":…}`/`{"Image":…}`/plain string.
- **Kind edges:** a legacy `system`-role message is the system prompt (`kind: system_prompt`; Zed's
  is normally app-level and absent), and a user message with `is_hidden: true` is Zed's auto-injected
  "continue where you left off" turn → `kind: injected_context`, `origin: harness`.
- **cwd:** `initial_project_snapshot.worktree_snapshots[0].worktree_path` (fallback: `folder_paths`
  column). The same snapshot's `git_state{remote_url, head_sha, current_branch, diff}` → `GitInfo`.
- **No per-message timestamps anywhere** — only thread-level created/updated.
- **Lossy notes:** extra worktrees of a multi-root workspace, `request_token_usage`, mention bodies
  (URI kept as a File block), and `creases` semantics aren't modeled; legacy `context` and 0.3.0
  `reasoning_details` ride in `extra["zed"]` (per-message), alongside session-level
  `thread_version`/`profile`/`completion_mode`/`detailed_summary`/`cumulative_token_usage`/`imported`.
  We decode blobs with pure-Rust `ruzstd` (no C cross-compile cost).
- **Schema snapshot** (no corpus: Zed's agent panel cannot be driven headless): `tests/corpus/zed/1.16.1/`
  records the DDL and blob shape of the store on the reference machine (`tools/harness-corpus/zed/snapshot.sh
  --check` diffs a later one). Zed 1.16.1 also keeps agent-sidebar tables in the workspace db
  (`db/0-stable/db.sqlite`): `sidebar_threads(thread_id, session_id, agent_id, title, updated_at, created_at,
  folder_paths…, archived, interacted_at, title_override)`, `sidebar_terminal_threads`,
  `thread_archived_worktrees`, `archived_git_worktrees`. `sidebar_threads.agent_id`/`session_id` index
  external (ACP) agents' threads, whose transcripts are not in `threads.db`; cv reads none of these tables.

## Devin CLI — `~/.local/share/devin/cli/sessions.db` (SQLite, refinery schema 17 as of cli 3000.11.3)

Closed-source (github.com/CognitionAI/devin-cli is a release manifest only); reverse-engineered
from a live DB 2026-10-04. The path is literally `~/.local/share/devin/cli/` even on macOS
(`$XDG_DATA_HOME/devin/cli` honoured); `transcripts/` sits beside it observed **empty** (purpose
unknown), alongside `app_state.json`, `session_locks/<id>.lock`, `logs/`, `plugins/` (not read).
WAL-mode and the CLI is typically running — we open READ-ONLY and never checkpoint.

- **Schema** (`MAX(version)` of `refinery_schema_history` = 17; migrations accrete columns, so we
  probe `PRAGMA table_info`): `sessions(id two-word slug, working_directory→cwd, backend_type
  ('windsurf'), model (CAN BE ''), agent_mode ('bypass'|'normal'|'plan'|…), created_at,
  last_activity_at unix seconds, title, main_chain_id, cogs_json, workspace_dirs JSON, hidden,
  metadata)`, `message_nodes(row_id, session_id, node_id, parent_node_id NULL=root,
  chat_message JSON, created_at secs, metadata JSON-or-'null' — UNIQUE(session_id, node_id))`,
  plus ignored `prompt_history`, `rendered_commits`, `app_state`, `tool_call_state` (ACP
  ToolCall/Update JSON — duplicates the assistant message), and `subagent_heads
  (session_id, agent_id, chain_node_id, updated_at — PK (session_id, agent_id), one current
  head per agent)`.
- **The forest:** every context rebuild (compaction, re-prefixing) writes a NEW chain of nodes
  copying earlier messages (same `message_id`), with node `metadata.extensions
  ["compact/prior_node_ids"]` naming the superseded copies. The transcript is the path from
  `sessions.main_chain_id` up `parent_node_id` to a root, reversed; off-chain nodes aren't emitted
  (`extra["devin"]["node_count"]`/`chain_len` show the loss). `main_chain_id` NULL → head = max
  `node_id`, flagged `main_chain_fallback`. Node `metadata`: `{summarized_from,
  num_tokens_preceding, is_system_prefix, extensions}`; `is_system_prefix` marks the system-prompt
  run.
- **chat_message**: `{message_id, role: system|user|assistant|tool, content: string}` + nested
  `metadata`. Assistant: `tool_calls[{id:"exec_N_…#…", name, arguments, index, kind:"function"}]`,
  `thinking{thinking, signature:"sealed.v1…", signature_type}`, `metadata{num_tokens, request_id,
  metrics{input/output/cache_read/cache_creation_tokens, ttft/total_time/tpot_ms, tokens_per_sec},
  finish_reason, extensions["chisel/tool_call_content"]={<id>:{title,status,kind,rawInput,…}},
  generation_model, created_at RFC3339, telemetry}`. Tool: `tool_call_id` + extensions
  `chisel/tool_call_timing`, `chisel/tool_result_meta{success,kind}`, `chisel/terminal_output`
  (exec: `{text,cwd,exit{terminal_id,exit_code}}`; `text` duplicates `content`, dropped),
  `chisel/undo` (`[{kind: restore_file, path, original_content, expected_post_hash} | {kind:
  irreversible, tool_name, description}]`). User: `metadata.is_user_input` marks typed prompts. Non-prefix system rows are
  `<system_info>` (with `affogato/cog-context`) and `<rules type="always-on">` blocks.
- **Sub-agent chains:** each `subagent_heads` row names a chain head in the *same* forest — a
  whole second conversation (verified: a sidekick's own `is_system_prefix` prompt, then its
  handoff prompt and turns) the main-chain walk never emits. Each becomes a child
  `SessionRef` id `<session_id>/<agent_id>` (`atom-telephone/sidekick`), listed right after the
  parent, with cwd/created_at inherited, `title` = `"[<agent_id>] <parent title>"`, `updated_at`
  from the head row, `lineage.parent`/`agent_path` set, and `extra["devin"]["agent_id"]` +
  `chain_head`. A `chain_node_id` that names no live node is skipped and counted under the
  parent's `extra["devin"]["dangling_subagent_heads"]`. A `run_subagent` call (3000.11.3, seen in
  `tests/corpus/devin/3000.11.3`) writes **no** `subagent_heads` row: its result row's
  `metadata.extensions` carry `subagent/agent_id`, `subagent/chain_node_id` (the child chain's
  tail), `subagent/profile_name` and `subagent/model`, and cv takes the chain from there.
- **Compaction:** a compaction writes two consecutive chain nodes sharing `summarized_from` = the
  old window's head — an `assistant` node holding the summary markdown and a `system` wrapper
  ("You are continuing work…") carrying `devin-rs/summary` (`source`), `compact/edited_files`,
  `compact/todo_list`, and `subagent/handoff_history` (a verbatim handoff copy — not carried). The
  wrapper's content embeds `Full conversation history saved at <path>.` →
  `summaries/<agent_id>/history_<hex>.md` beside `sessions.db` (~300 KB of pre-compaction
  markdown; referenced, not read). A synthetic `CompactionBoundary` is emitted before each pair
  (`extra["devin"]["compaction"]` = `{summarized_from, pre_chain_head, source, history_path,
  edited_files, todo_list}` + `compactMetadata` trigger/preTokens), so `cv show --kind compaction_boundary` works.
- **IR mapping:** prefix system → `system_prompt` kind (also joined into `Session::system_prompt`);
  other system and non-input user rows → `injected_context`; `summarized_from` non-null →
  `compaction_summary` regardless of role; a tool row's `chisel/tool_failure` → `is_error` +
  `details.failure_reason`; `chisel/user_question_answers` (dialog answers, "User answered your
  questions:") → `extra`, which `cv show --kind answer` picks up; `subagent/profile_name|model|agent_id|
  chain_node_id` on the parent's completion rows → `extra["devin"]["subagent"]` (the tool row's
  `tool_call_id` is the child's `spawned_by_tool_use`); assistant
  metrics → `Usage`, `generation_model` → `model` (and the session's
  model when `sessions.model` is `''`); tool rows pair `tool_name` from the earlier call and carry
  `{cwd, exit_code, terminal_id, duration_ms, kind}` in `ToolResult.details`, plus `restorable`
  (the `restore_file` paths of `chisel/undo`). A `user` row with
  `extensions["subagent/handoff"]` → `Prompt`/`Origin::Subagent`, and
  `chisel/fusion_lead_model_uid` → `extra["devin"]["lead_model"]`. Sub-agent system rows'
  `agent-ext/rules-loaded` → `extra["devin"]["rules_loaded"]` (whole object) and
  `agent-ext/skills-loaded` → `skills_loaded` reduced to `{name, path}`. `hidden = 1` sessions
  stay discoverable.

## Account data exports — registered in `config.toml` (opt-in)

The archives you download via "Export data". One file (`conversations.json`, often split
`conversations-000.json … -NNN.json`) holds **many** conversations, so one file → many `SessionRef`s.
Distinct from the `claude` harness. Account exports have no fixed home, so
discovery is **opt-in via the config index**: register source dirs/files with `cv config --add-export
<path>` (stored in `$XDG_CONFIG_HOME/clustervision/config.toml` as `exports = [...]`; `$CV_EXPORTS`, a
`:`-separated list, is honored as an ad-hoc union on top). With nothing registered, no scan happens —
these archives are large and enumerating thousands of conversations costs seconds. Conversations are
**deduped by id** (overlapping exports repeat them). Both adapters parse-only (no emit). The two shapes
are sniffed by content (a file's first conversation): `mapping` ⇒ ChatGPT, `chat_messages` ⇒ Claude.

- **`chatgpt-export`** — each conversation: `{id|conversation_id, title, create_time, update_time,
  default_model_slug, current_node, mapping}`. `mapping` is a **DAG**: `node_id → {id, parent,
  children[], message}`; we walk the `root → current_node` chain (the active branch; regenerated/edited
  branches are excluded). A message: `{author:{role}, content:{content_type, parts|text}, recipient,
  create_time}`. `content_type` ∈ `text` (parts=strings) / `code` (`text`) / `multimodal_text` (parts
  incl. `{content_type:image_asset_pointer, asset_pointer:"file-service://…"}` → `Block::Image`) /
  `execution_output` / `tether_*` (browsing) / `user_editable_context` (custom instructions). On an
  assistant turn, `recipient` ≠ `all` (e.g. `python`, `dalle.text2im`, `browser`, `bio`) ⇒ a tool call.
- **`claude-export`** — each conversation: `{uuid, name, summary, created_at, updated_at,
  chat_messages[]}`. `chat_messages` are linear (via `parent_message_uuid`); `sender` ∈ human/assistant;
  `content[]` blocks ∈ `text` / `thinking` / `tool_use{id,name,input}` / `tool_result{tool_use_id,
  content,is_error}`; a flat `text` field is the fallback when there are no structured blocks.
- **Lossy notes (v1):** off-branch ChatGPT regenerations aren't surfaced; tool-call args beyond the
  rendered text aren't structured for ChatGPT (recipient-tool turns become a `ToolUse` carrying the
  text); image bytes are references (`asset_pointer`), not inlined.

---

## Prior art (don't reinvent; do unify)

- `claude-code-transcripts` (Rust, lib.rs) — typed parser, **Claude only**, parse + HTML.
- `agent-transcript-parser` (Python) — Claude↔Codex conversion, "lossless on round-trip". Only those two.
- `trail-cli`, Contextify, Automagik, `claude_codex_bridge` — Python, 2–3 harnesses, mostly read/HTML.

Gap clustervision fills: one **Rust** IR across **all of them** (21 harnesses — `Harness::COUNT`, one
manifest each under `formats/`) with **index + search + port + live-follow + an MCP server**.
