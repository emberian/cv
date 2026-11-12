# Harnesses & where they store sessions

cv reads **21 harnesses**, including the ChatGPT and Claude.ai account exports (`chatgpt-export`,
`claude-export`; register with `cv config --add-export`). Each ties a session to its working
directory; cv's one IR unties it.

Below: where each keeps sessions, whether cv **parses** it, and whether it can **emit** it (a
[conversion](conversion.md) target). The reverse-engineered ground truth is
[`docs/FORMATS.md`](https://github.com/emberian/cv/blob/main/docs/FORMATS.md).

## The master table

`<AppSupport>` is `~/Library/Application Support` (macOS), `~/.config` (Linux), `%APPDATA%` (Windows).

| Harness | On-disk location | Parse | Emit | Notes |
|---|---|:--:|:--:|---|
| **Claude Code** | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | ✓ | ✓ | Threaded by `uuid`/`parentUuid`; sub-agents in `<sid>/subagents/agent-*.jsonl`. Dir names are lossy, so `cwd` is read from inside. Extra roots: [below](#claude-code-sessions-outside-claude). |
| **Codex CLI** | `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<ULID>.jsonl` | ✓ | ✓ | Also `~/.codex/archived_sessions/`; SQLite index `state_5.sqlite`; 2025 legacy form is one `{session,items[]}` JSON. |
| **Grok CLI** | `~/.grok/sessions/<percent-encoded-cwd>/<sid>/` | ✓ | ✓ | A directory per session (`chat_history.jsonl` + sidecars); sub-agents carry `session_kind:"subagent"`. |
| **OpenCode** | `~/.local/share/opencode/storage/` | ✓ | ✓ | `session/**/ses_*.json`, `message/<sid>/*.json`, `part/<msgid>/*.json`; `subtask` parts spawn sub-sessions. Two storage generations. |
| **OpenClaw** | `~/.openclaw/agents/<agentId>/sessions/<sid>.jsonl` | ✓ | ✓ | `sessions.json` index + JSONL; header `version` v1/v2 linear, v3 parent-linked (migrated in place). ACP-bridged sessions are text-only. |
| **Gemini / Antigravity** | `~/.gemini/tmp/<projectHash>/chats/session-*.json\|jsonl` | ✓ | ✓ | Chat recordings, `checkpoint-*.json`, `logs.json` fallback; sub-agents at `chats/<parentId>/<id>.jsonl`. Antigravity `*.pb` is not parsed. |
| **Hermes (Nous)** | `~/.hermes/state.db` (SQLite) | ✓ | ✓ | `$HERMES_HOME` overrides. No cwd stored. Compression chains linked by `parent_session_id`. |
| **Kimi CLI** | `~/.kimi/sessions/<md5(cwd)>/<uuid>/context.jsonl` | ✓ | — | Frozen since 2026-06 (kimi-cli migrates it to `~/.kimi-code`). `$KIMI_SHARE_DIR` overrides; cwd from `kimi.json` `work_dirs[]`; legacy `<hash>/<uuid>.jsonl`; `wire.jsonl` enriches. |
| **Kimi Code** | `~/.kimi-code/sessions/wd_<slug>_<sha256(cwd)[:12]>/session_<uuid>/` | ✓ | — | `$KIMI_CODE_HOME` overrides. `state.json` holds cwd, title, agent map; `agents/<agentId>/wire.jsonl` per agent; large outputs in `tool-results/`. |
| **LM Studio** | `~/.lmstudio/conversations/<ms-epoch>.conversation.json` | ✓ | ✓ | One JSON per chat; no cwd, no tool calls; the selected regeneration is read. |
| **Cline** | `<AppSupport>/<Editor>/User/globalStorage/saoudrizwan.claude-dev/tasks/<taskId>/` | ✓ | ✓ | Also `~/.cline/tasks/`. `api_conversation_history.json` is an Anthropic Messages array; cwd from the first message's `<environment_details>`. |
| **Roo Code** | `<AppSupport>/<Editor>/User/globalStorage/rooveterinaryinc.roo-cline\|roo-code/tasks/<taskId>/` | ✓ | ✓ | Cline's layout and parser/emitter; also `~/.roo/tasks/`. |
| **Continue (continue.dev)** | `~/.continue/sessions/<sid>.json` | ✓ | ✓ | `$CONTINUE_GLOBAL_DIR` overrides; `sessions.json` index; OpenAI-shaped messages; cwd is `workspaceDirectory`. |
| **Qwen Code** | `~/.qwen/projects/<cwd>/chats/<id>.jsonl` (legacy `~/.qwen/tmp/<hash>/chats/session-*.json\|jsonl`) | ✓ | ✓ | Claude-shaped records with Gemini parts; sub-agents in `subagents/<parent>/agent-<id>.jsonl`; legacy files go to the Gemini parser. |
| **Cursor IDE** | `<AppSupport>/Cursor/User/globalStorage/state.vscdb` (SQLite) | ✓ | — | `composerData:`/`bubbleId:` rows; per-workspace DBs link composers to cwd; schema `_v` 1..=10+. |
| **Goose (Block)** | `<AppSupport>/Block.block.goose/sessions/sessions.db` (SQLite) | ✓ | — | `$GOOSE_PATH_ROOT`/`$XDG_DATA_HOME` override; also legacy `<name>.jsonl`; MCP-style tool content. |
| **Zed (agent panel)** | `<data_dir>/Zed/threads/threads.db` (SQLite) | ✓ | — | One row per thread; `data` is zstd-compressed JSON in three generations; cwd and git from the project snapshot; sub-agents by `parent_id`. |
| **Devin CLI** (Cognition) | `~/.local/share/devin/cli/sessions.db` (SQLite) | ✓ | — | `message_nodes` is a forest; the transcript is the `main_chain_id`→root walk. Thinking, parallel calls and `sealed` signatures kept. |
| **ChatGPT export** | a registered `conversations.json` | ✓ | — | One file, many conversations, each a `mapping` DAG walked root→`current_node`; deduped by id. |
| **OpenSession** | `*.opensession.json` (or `*.json` with an `open_session` key) in a registered dir | ✓ | — | Written by `cv export --format json`; 0.3 only, 0.2 refused. See [OpenSession](conversion.md#the-opensession-standard). |
| **Claude.ai export** | a registered `conversations.json` | ✓ | — | Linear `chat_messages[]` (text, thinking, tool_use, tool_result); deduped by id. |

Emit targets (12): Claude Code, Codex, Grok, OpenCode, OpenClaw, Gemini, Hermes, LM Studio, Cline,
Roo, Continue, Qwen (`supported_targets()` in `emit.rs`). The other nine are parse-only.

Claude Code 2.1.304 is the verified writer: its request-ledger records, fork sub-agents, retries and
`--max-turns` are in the committed corpus. Unknown records are carried by complete reads, never
counted as turns; `cv doctor` compares the writer versions it sees with this.

## Claude Code sessions outside `~/.claude`

cv reads Claude sessions from, in order:

1. `$CLAUDE_CONFIG_DIR/projects`, when set; `cv port --harness claude` without `--out` writes here.
2. `~/.claude/projects`.
3. Each entry of `CLUSTERVISION_CLAUDE_ROOTS`, separated like `PATH`.
4. Each line of `$CLUSTERVISION_HOME/claude-roots` (default `~/.clustervision/claude-roots`); blank
   and `#` lines are ignored.

An entry names a config dir (containing `projects/`) or a `projects/` dir, may start with `~`, and
may contain path segments that are exactly `*`, matching each entry of the directory before it.
These lines read every agent with its own config dir under `~/.agents/`, and every instance of a
multi-instance agent:

```text
~/.agents/*/claude
~/.agents/*/instances/*/claude
```

`*` is expanded on every discovery, so new agents appear without a restart. Missing entries are
ignored, a directory named twice (e.g. through a symlink) is read once, and a config dir is skipped
until its `projects/` appears.

Prefer the file: every cv process (CLI, daemon) shares one catalog and reads it, while
an environment variable reaches only the processes that have it.

When two roots hold the same session id, `cv ls` lists both and `cv show <id>` opens the most
recently updated.

## Sub-agents

cv attaches child agents to their parent as a tree ([viewing](viewing-and-sharing.md)):

- **Claude Code**: `<sid>/subagents/agent-<id>.jsonl`, each with a `.meta.json` (`agentType`,
  `description`).
- **Kimi**: `<session>/subagents/<agent_id>/context.jsonl`; **Kimi Code**:
  `<session>/agents/agent-N/wire.jsonl`; both listed as `<parent>/<agent>`. **Hermes**: delegate runs
  marked `model_config._delegate_from`.
- **Grok**: `session_kind: "subagent"` (primaries carry `agent_name`), kept in message `extra`.
- **Gemini**: `chats/<parentId>/<id>.jsonl`; **Qwen**: `subagents/<parentId>/agent-<id>.jsonl`.
- **OpenCode**: `subtask` parts (own prompt, agent and model), beside `agent` mention parts.
- **Zed**: its own `threads.db` row with `parent_id` (0.3.0 blobs add
  `subagent_context{parent_thread_id, depth}`), kept as `Session::lineage.parent`, `depth` in
  `extra["zed"]`.

## The SQLite stores

Hermes (`state.db`), Cursor (`state.vscdb`), Goose (`sessions.db`), Zed (`threads.db`) and Devin
(`sessions.db`) keep everything in SQLite; OpenCode 2.x and newer OpenClaw stores use it beside
files. All open read-only (a WAL database through a private snapshot, so cv never writes a
`-wal`/`-shm`), and each adapter probes `PRAGMA table_info` and selects only existing columns, so an
older schema degrades instead of failing. Codex and Grok SQLite files are only indexes.

## The two desktop apps are not harnesses

The Claude desktop app stores no transcripts locally (its Claude Code sessions are ordinary `claude`
sessions); the ChatGPT desktop app encrypts its history with a key cv cannot read. `claude-app` and
`chatgpt-app` exit 2 with a pointer. Details: `docs/FORMATS.md`, "Desktop apps".

## Bring your own harness

A new harness is one module implementing `Adapter` (`discover`, `parse`, optionally `emit`); see
[Adding a harness](adding-a-harness.md).
