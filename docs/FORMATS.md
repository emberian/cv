# Harness session formats (reverse-engineered)

This is the ground-truth catalog of how each harness stores sessions on disk, reverse-engineered from a
real machine (2026-05-29; Claude Code and Codex re-verified 2026-09-19 against Claude Code 2.1.278 and
Codex 0.154 / `132c2be23`). It drives the adapters in `cv-core`. Keep it accurate; it is the spec.

The unifying insight: **all harnesses encode the working directory (cwd) into where/how they store a
session.** That cwd-coupling is exactly why sessions are "dir-jailed" and hard to find. clustervision
decouples them via a unified IR.

---

## Claude Code — `~/.claude/`

- **Transcripts:** `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` (one session per file, JSONL).
- **cwd encoding (dir name):** leading `-`, then `/` → `-`, and `.` → `-`. **Lossy / not reversible**
  (original `-` and `.` collide). → *Do not decode the dir name; read `cwd` from inside the transcript.*
- **Per-line `type` values:** `user`, `assistant`, `attachment`, `system` (subtyped — see below), and the
  session-level bookkeeping records: `ai-title` (rewritten on every prompt), `custom-title` (`/rename`;
  outranks `ai-title`), `tag`, `agent-name` (derived session name), `mode`, `permission-mode`,
  `atis-latch`, `last-prompt` (`{leafUuid, lastPrompt, explicit?, rewound?}` — the loader's leaf pointer),
  `queue-operation` (`{operation: enqueue|dequeue|remove|popAll, reason?, content}`), `cost-state`
  (cumulative `totalCostUSD`/durations/`modelUsage` per model), `continued-in` (`{continuedInSessionId}` —
  the conversation moved to another session id), `relocated` (`{relocatedCwd}`, forks), `pr-link`
  (`{prNumber, prUrl, prRepository}`), `frame-link`, `content-replacement` (`{replacements}` applied at
  load — forks/microcompact), `artifact-comment-monitor`, `artifact-autoreact-ledger`,
  `file-history-snapshot`. **`summary` records are no longer written** (gone since ~2.1.25x; still read).
  Most records since ~2.1.25x carry BOTH `sessionId` and snake-case `session_id`, plus `slug`,
  `entrypoint`, `userType`, `version`, `gitBranch`; user turns add `promptId`, `origin{kind:human|…}`,
  `promptSource`, `turnOrigin`, `permissionMode`; assistant turns add `requestId`, `effort`,
  `apiBlockIndex` (one line per streamed content block, sharing `message.id`).
- **Attachments ARE prompt content (since ~2.1.23x).** The `<system-reminder>` text Claude Code appends to
  the prompt no longer lives inline in user content; each is an `attachment` record `{attachment:{type,…},
  rendered?: [{content: "<system-reminder>…</system-reminder>"}], renderedInHumanTurn?}` — `rendered[]`
  is byte-for-byte what the model saw; an attachment WITHOUT `rendered` is bookkeeping only. Kinds seen:
  `hook_success` (`{hookName, hookEvent, toolUseID, stdout, stderr, exitCode, command}`),
  `total_tokens_reminder`, `batching_reminder_sent`, `bash_output_audience_note`, `queued_command`
  (task notifications / queued prompts — the biggest by bytes), `silent_turn_reminder`,
  `edited_text_file` (`{filename, snippet}`), `task_reminder`, `deferred_tools_delta`,
  `agent_listing_delta`, `mcp_instructions_delta`, `skill_listing`, `command_permissions`,
  `environment`, `date`/`date_change`, `model`, `instructions` (CLAUDE.md files), `session_context`,
  `remote_session_change`, `prompt_snapshot` (the whole system prompt + tool schemas; not re-sent),
  `auto_mode`, `goal_status`, `task_status`, `file` (an `@`-mentioned file), `compact_file_reference`,
  `thinking_drop` (`{blockHashes[], newlyDropped{blockCount,turnCount,reason}, thinkingBlocksSent}` —
  Claude Code dropped those thinking blocks from the REQUEST; the transcript keeps them and the loader
  re-applies the drop by hash). cv parses rendered attachments as System turns (`extra.attachmentType`).
- **Synthetic assistant notices:** `assistant` lines with `message.model == "<synthetic>"` are Claude
  Code's own client-side rows, never sent to the API: `isApiErrorMessage:true` + `error`
  (`invalid_request`/`rate_limit`/…) for "Prompt is too long" (with `errorDetails` ONLY when it came from
  the API — the client-side context gate emits it with none), and `isApiErrorMessage:false` for "No
  response requested." (resume preamble). Not turns.
- **`usage.iterations[]`** (Fable-era): per-request entries `{type: message|fallback_message|
  advisor_message|compaction, input_tokens, output_tokens, cache_read_input_tokens,
  cache_creation_input_tokens}`. **Claude Code ≥ 2.1.277 reads the context size from the LAST
  non-advisor/compaction iteration** (falling back to the top-level counters), and its turn gate refuses
  client-side with a synthesized "Prompt is too long" when that + a byte estimate of everything after the
  record ≥ context window − min(max_output, 20k) − 3k. `cv prune --revive` pins both.
- **Persisted tool outputs:** a too-large `tool_result` is replaced by the stub
  `<persisted-output>\nOutput too large (NNKB). Full output saved to: <session>/tool-results/<id>.txt\n\n
  Preview (first 2KB):\n…</persisted-output>` — the model saw only the stub; cv keeps it as content and
  records the path in `details.persistedOutput`.
- **Threading:** every `user`/`assistant`/`attachment` line has `uuid` + `parentUuid` (null at root) →
  a linked list / DAG. `last-prompt.leafUuid` points at the tail.
- **Message line fields:** `message.role`, `message.content` (string for simple user msgs; array of blocks
  `{type: text|thinking|redacted_thinking|tool_use|image|document}` for assistant; `tool_result` blocks
  for tool returns). Also `cwd`, `gitBranch`, `version`, `sessionId`, `timestamp` (ISO-8601), `model`
  (assistant), `usage` (tokens), `requestId`, `message.{id,stop_reason}`.
- **Tool results:** carried on a `user` line as `content[].type=="tool_result"` (`tool_use_id`, `content`,
  `is_error`) plus a richer `toolUseResult` sidecar object (`structuredPatch`/`oldTodos`/`newTodos`/
  `stdout`/`stderr`/file contents…). A user line whose content is *only* tool_results is a Tool turn.
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
- **Compaction** (`compact_boundary`): `compactMetadata = {trigger: manual|auto, preTokens, durationMs,
  preservedSegment:{headUuid,anchorUuid,tailUuid}, preservedMessages:{…}}`. The boundary is immediately
  followed by a `user` message `isCompactSummary:true` whose `parentUuid == boundary.uuid` and whose body
  is the **generated summary that seeds the next context window** (the lost pre-compaction context). A
  session compacts repeatedly (the reference transcript: 11×).
- **Subagents (two tiers):**
  - *Directly-spawned* (`Agent`/`Task` tool): `<sessionId>/subagents/agent-<agentId>.jsonl` +
    `agent-<agentId>.meta.json` = `{agentType, description, toolUseId}`. **`toolUseId` links the child
    back to the exact `Agent`/`Task` tool_use in the parent transcript.** The child's records carry
    `isSidechain:true` + `agentId`; its *return value* is the last assistant text turn (surfaced in the
    parent's tool_result).
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
- **cv surfacing:** `cv tree <id>` appends the sub-agent forest (direct + per-workflow, with journaled
  outcomes); `cv show <id> --subagents` lists every agent with its return; `cv show <id> --agent <aid>`
  renders one sub-agent transcript; `cv events <id> --subagents` extracts the whole forest's tool activity,
  attributed per agent. **`cv workflow <id> [<runId>]`** renders a run's phase tree → agents → outcomes (+
  `--script`); **`cv tools <id>`** is the cross-agent tool-analytics surface (aggregate / `--agent` /
  `--tool` / `--workflow` / `--across` / `--timeline`); **`cv compaction <id>`** lists every boundary +
  summary (`--summaries` for full text); **`cv show <id> --pre-compaction <N>`** reads the lost pre-span.

## Codex CLI — `~/.codex/`

Ground truth: `codex-rs/history/src/{lib,rollout_payload}.rs` (RolloutItem/RolloutLine),
`codex-rs/rollout/src/policy.rs` (what is persisted), `codex-rs/protocol/src/{protocol,models,items}.rs`.
Re-verified 2026-09-19 at `132c2be23` (CLI 0.154).

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
  `context_window`. **Fork/subagent rollouts embed the parent's prefix** (ordinals below
  `subagent_history_start_ordinal`, with a second `session_meta` = the parent's on line 1) stamped with
  the FORK time; the real per-item time is `payload.internal_chat_message_metadata_passthrough
  .create_time` (epoch seconds). The FIRST `session_meta` is canonical.
- **`turn_context` payload:** REQUIRED `cwd`, `approval_policy` (`untrusted|on-request|never`),
  `sandbox_policy` (`{type: danger-full-access|read-only|workspace-write|external-sandbox, …}`),
  `model`, `summary` (`auto|concise|detailed|none`); optional `turn_id`, `root_turn_id`, `effort`,
  `personality`, `collaboration_mode {mode, settings{model, reasoning_effort}}`, `permission_profile`,
  `current_date`, `timezone`, `workspace_roots`. A record missing a required field is skipped on resume.
- **`response_item.payload.type`:** `message` (`role`, `content[]` of `input_text|output_text|
  input_image`, `id: msg_…`, `phase`, `internal_chat_message_metadata_passthrough{turn_id,
  create_time}`), `agent_message` (inter-agent swarm channel: `{id: amsg_…, author, recipient,
  content:[{type:input_text,text}|{type:encrypted_content,…}]}`, always paired with a following top-level
  `inter_agent_communication_metadata {trigger_turn}` line), `reasoning` (`{id: rs_…, summary:[…],
  encrypted_content}` — `content` omitted whenever it holds `reasoning_text`), `function_call`
  (`{id: fc_…, call_id, name, namespace?, arguments (JSON string), encrypted_function_args?}`),
  `function_call_output` (`{call_id, name?, namespace?, output}` where **`output` is a string or a
  content-item array — never an object; no persisted error bit**), `custom_tool_call(_output)` (`input`
  is a freeform string), `local_shell_call`, `web_search_call` (`action: search|open_page|find_in_page`),
  `tool_search_call/_output`, `image_generation_call`, `compaction`/`context_compaction`,
  `configuration_update`.
- **Other top-level types:** `compacted` (`{message (now usually ""), replacement_history[], window_number,
  window_id, first/previous_window_id, compaction_response_id, latest_token_usage_record,
  retained_context}`), `token_usage_record` (per response: `{thread_id, turn_id, session_id,
  root_turn_id, response_id, usage, turn_token_usage, thread_token_usage}`; `TokenUsage = {input_tokens,
  cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`),
  `inter_agent_communication_metadata`, `world_state` (embeds AGENTS.md text), `security_risk_score`,
  `retained_context`, `realtime_item`.
- **`event_msg.payload.type` (both modes):** `token_count {info{last_token_usage, total_token_usage},
  rate_limits}`, `task_started {turn_id, model_context_window}`, `task_complete {turn_id,
  last_agent_message, duration_ms}`, `thread_settings_applied {thread_settings{model, model_provider_id,
  reasoning_effort, personality, approval_policy, cwd}}` (the reliable model-change signal),
  `turn_aborted {turn_id, reason}`, `thread_goal_updated`, `thread_rolled_back {num_turns}`,
  `item_completed` (paginated: all items; legacy: only FunctionCallOutput/Plan/Sleep/completed
  SubAgentActivity). `view_image_tool_call` is never persisted (images arrive as `item_completed
  ImageView{path:"file:///…"}`).
- **`item_completed.item` (`type`, PascalCase):** `UserMessage`, `AgentMessage{content:[{type:Text}],
  phase}`, `Reasoning{summary_text, raw_content}`, `CommandExecution{command[], cwd:"file:///…",
  parsed_cmd, source, status, stdout?, stderr?, aggregated_output?, exit_code?, duration}`,
  `FileChange{changes{path:{type:add|update|delete, content|unified_diff}}, status}`, `ImageView{path}`,
  `ImageGeneration{saved_path}`, `SubAgentActivity{kind: started|interacted|interrupted|completed,
  agent_thread_id, agent_path}`, `CollabAgentToolCall{tool, status, sender_thread_id,
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
  `reasoning{text,encrypted,id}`, `model_id`, `model_fingerprint`.
- **summary.json:** `{info{id,cwd}, created_at, updated_at, num_messages, current_model_id, git_root_dir,
  git_remotes[], head_commit, head_branch, agent_name, ...}`.
- **Indexes (sqlite):** `~/.grok/sessions/session_search.sqlite` (FTS5: `session_docs` + `session_docs_fts`
  over title+content), `~/.grok/worktrees.db` (`worktrees(id,path,session_id,...)`), and per-cwd
  `prompt_history.jsonl` (`{timestamp,session_id,prompt,is_bash}`).

## OpenCode — `~/.local/share/opencode/` (NOT `~/.opencode`, which is plugins/cache)

Re-verified 2026-09-19 at `fee476bb` (1.18.31). Ground truth: `packages/core/src/session/sql.ts` (tables),
`packages/schema/src/v1/session.ts` (Info/Part shapes), `packages/core/src/database/database.ts` (db path).

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
  attachments?: FilePart[]} | error`.
- **The JSON tree is dead:** `storage/{session,message,part}/**.json` was only the input of a one-shot importer,
  deleted 2026-06-02 (`ca2acc4f`); anything left on disk is a pre-2026-01 leftover. Read it only as a fallback
  when no db exists. Also `~/.local/state/opencode/` and `prompt-history.jsonl`. `opencode export [sid]` writes
  `{info, messages:[{info, parts}]}` (an importable interchange form).

## Gemini / Antigravity — `~/.gemini/`

Re-verified 2026-09-19 at `cfbcaa8` (0.46.0): **the record format is unchanged** (`chatRecordingTypes.ts` and
`logger.ts` have no diff since 2026-05).

- **Antigravity transcripts:** `~/.gemini/antigravity/conversations/<uuid>.pb` — **protobuf, opaque**
  (no .proto on disk). Best-effort only.
- **Readable fallback:** `~/.gemini/tmp/<hash>/logs.json` — array of `{sessionId, messageId, type, message,
  timestamp}` (user messages reliably; assistant inconsistently).
- **Sidecars:** `~/.gemini/antigravity/brain/<conv-id>/.system_generated/logs/overview.txt` (summary),
  `knowledge/`, `context_state/`, `implicit/`.
- Note: open-source `gemini-cli` (cloned at `~/pug/gemini-cli`) uses JSON checkpoints, a *different* format
  from the closed Antigravity IDE — consult its `packages/cli/src/utils/sessions.ts` for that variant.
- Also handled now: the gemini-cli JSON **chat recordings** (`~/.gemini/tmp/<projectHash>/chats/session-*.json`
  legacy object + modern append-only `.jsonl` with `$set`/`$rewindTo`) and `checkpoint-*.json` — far richer
  than logs.json (real assistant turns, thoughts, tool calls). Qwen Code reuses this format under `~/.qwen/`.
- **Second storage root:** under macOS Seatbelt (`SANDBOX=sandbox-exec`) the runtime dir is `~/.cache/.gemini`,
  so recordings land in `~/.cache/.gemini/tmp/<project>/chats/…` (and `history/`) — scan both roots, dedupe by
  `sessionId`. `chats/` may hold `<file>.unreadable-<epochms>` and `<file>.tmp-<pid>` siblings (a corrupt
  recording, a rewrite in progress) — skip them. Empty, non-resumable recordings are deleted by the CLI.
- **cwd:** `tmp/<projectIdentifier>` is a short id; the real path lives in `~/.gemini/projects.json`
  (`{"projects": {"/abs/path": "<id>"}}`) or `~/.gemini/history/<id>/.project_root`; `directories[]` inside a
  recording is often absent.

## Hermes (Nous) — `~/.hermes/state.db` (SQLite, `SCHEMA_VERSION` 30 as of 2026-09-19; `$HERMES_HOME` overrides; per-profile `profiles/<name>/state.db`)

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
- **Lineage:** compression chains link via `parent_session_id` (parent `end_reason='compression'`); branch /
  reset / delegate children are marked in `model_config` JSON (`_branched_from`, `_reset_from`,
  `_delegate_from`) — an explicit branch is its own conversation and delegate children are sub-agent runs;
  Hermes's own listing hides `archived=1 OR hidden=1`, compression continuations and delegate children.

## OpenClaw — `$OPENCLAW_STATE_DIR` or `~/.openclaw/agents/<agentId>/`

Re-verified 2026-09-19 at `0e9181234a`. Ground truth: `src/state/openclaw-agent-schema.sql`,
`src/config/sessions/*`, `src/agents/sessions/session-manager-types.ts`.

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

## Cursor IDE — `~/Library/Application Support/Cursor/User/` (mac; `%APPDATA%/Cursor/User` Win; `$XDG_CONFIG_HOME/Cursor/User` Linux)

- VS Code-derived; SQLite `state.vscdb` (`ItemTable` + `cursorDiskKV`; JSON values, TEXT or BLOB). Open READ-ONLY.
- **Global content:** `globalStorage/state.vscdb` → `cursorDiskKV`: `composerData:<id>` (one thread; metadata +
  either inline `conversation[]` (older `_v`) or `fullConversationHeadersOnly[]` pointers (newer)), and
  `bubbleId:<composerId>:<bubbleId>` (one message: `type` 1=user/2=assistant, `text`, `richText` Lexical,
  `thinking{text}`, `toolFormerData{name,rawArgs,params,result,error,status}`, `tokenCount`).
- **cwd:** per-workspace `workspaceStorage/<hash>/state.vscdb` `ItemTable.composer.composerData.allComposers[]`
  links composerIds → workspace; sibling `workspace.json` `folder:"file://…"` is the cwd. Legacy: in-workspace
  `workbench.panel.aichat.view.aichat.chatdata`. Closed-source & churny (`_v` 1..=10+) — skip the unknown.

## Desktop apps (detected, not parseable)

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
  message:{type, payload}}`) enriches tool results + `StatusUpdate.token_usage{input_other, …}`;
  `context_N.jsonl` are compaction segments.
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
  <uuid>.txt` (outputs > 50 000 chars), `media/`, `logs/`.

## Qwen Code — `~/.qwen/`

- A **gemini-cli fork**: byte-identical format under `~/.qwen/tmp/<projectHash>/{logs.json, chats/session-*.json|jsonl,
  checkpoint-*.json}`. The adapter delegates to the Gemini parser and re-tags `Harness::Qwen`. (Path delta only.)

## LM Studio — `~/.lmstudio/conversations/<ms-epoch>.conversation.json`

- Plaintext JSON, one file per chat (filename stem = id = `createdAt` ms). No cwd (chat app). `messages[]` =
  `{versions:[…], currentlySelected}` (every regenerated variant kept; read the selected one). `singleStep` (user)
  content parts `text`/`file`; `multiStep` (assistant) `steps[]` of `contentBlock` (`style.type=="thinking"` →
  reasoning; `genInfo` → model + tokens). No first-class tool calls (gpt-oss inline `<|channel|>` markers kept as
  text). Attachments are references; bytes under `~/.lmstudio/.internal/files/`.

## Cline — VS Code extension (`<globalStorage>/saoudrizwan.claude-dev/`)

- Tasks at `<globalStorage>/saoudrizwan.claude-dev/tasks/<taskId>/` (also `~/.cline/tasks/`): `api_conversation_history.json`
  (a JSON **array of raw Anthropic Messages-API objects** — maps ~1:1 onto the Claude block model), `ui_messages.json`
  (UI events, timestamp enrichment), `task_metadata.json`. globalStorage roots: `<Editor>/User/globalStorage/` for
  `<Editor>` ∈ {Code, Code - Insiders, Cursor, VSCodium}. `<taskId>` ms-epoch → `created_at`. cwd from the first user
  msg's `<environment_details>`. A `user` turn of only `tool_result` blocks → a Tool turn.

## Roo Code — a Cline fork (`<globalStorage>/rooveterinaryinc.roo-cline|roo-code/`)

- Identical per-task layout/schema to Cline, different extension namespace (+ `~/.roo/tasks/`). Reuses the Cline parser,
  tagged `Harness::Roo`.

## Continue (continue.dev) — `~/.continue/sessions/` (`$CONTINUE_GLOBAL_DIR` overrides)

- Index `sessions.json` (`[{sessionId, title, dateCreated, workspaceDirectory}]`) + per-session `<id>.json`
  (`{sessionId, title, workspaceDirectory, history:[item]}`). Each item's `message` is OpenAI-shaped (role
  user/assistant/system/tool; content string or `[{type:text}/{type:imageUrl}]`; assistant `toolCalls[]`;
  `role:"tool"` + `toolCallId`). `contextItems[]` → File refs. cwd = `workspaceDirectory`.

## Goose (Block) — modern SQLite + legacy `.jsonl`

Re-verified 2026-09-19 at `2090ad1c` (1.51.0). Ground truth: `crates/goose/src/session/session_manager.rs`
(schema + writers), `crates/goose-provider-types/src/conversation/message.rs` (content blocks).

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
  primary worktree). `parent_id` links a **subagent** thread to its parent (we surface it as
  `extra.parent_thread_id`; the hierarchy itself is flattened — each thread is its own session).
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
    `{"Compaction":{"Summary":"…"}}`. Tool-result `content` is `{"Text":…}`/`{"Image":…}`/plain string.
- **cwd:** `initial_project_snapshot.worktree_snapshots[0].worktree_path` (fallback: `folder_paths`
  column). The same snapshot's `git_state{remote_url, head_sha, current_branch, diff}` → `GitInfo`.
- **No per-message timestamps anywhere** — only thread-level created/updated.
- **Lossy notes:** extra worktrees of a multi-root workspace, `request_token_usage`, mention bodies
  (URI kept as a File block), and `creases` semantics aren't modeled; legacy `context` and 0.3.0
  `reasoning_details` ride in `extra`. We decode blobs with pure-Rust `ruzstd` (no C cross-compile cost).

## Account data exports — registered in `config.toml` (opt-in)

The archives you download via "Export data". One file (`conversations.json`, often split
`conversations-000.json … -NNN.json`) holds **many** conversations, so one file → many `SessionRef`s.
Distinct from the `claude`/`claude-app`/`chatgpt-app` harnesses. Account exports have no fixed home, so
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

Gap clustervision fills: one **Rust** IR across **all of them** (10 harnesses) with **index + search + port +
convert + live-follow + an MCP server + a coordination board**.
