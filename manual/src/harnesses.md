# Harnesses & where they store sessions

clustervision currently understands **20 harnesses** 🔮 (incl. the ChatGPT & Claude.ai account data
exports — `chatgpt-export`/`claude-export`, register via `cv config --add-export`). Every one of them couples a session to a
working directory — they encode the `cwd` into the path, the filename, a hash, or a DB column — which
is exactly why your sessions feel "dir-jailed" and impossible to find later. clustervision reads them
all into one unified IR so that coupling stops mattering.

This chapter is the field guide: for each harness, *where* it keeps sessions on disk, whether we can
**parse** it (read it into the IR) and whether we can **emit** it (write the IR back out, making it a
[conversion](conversion.md) target).

The ground-truth catalog these adapters are built from lives in
[`docs/FORMATS.md`](https://github.com/emberian/cv/blob/main/docs/FORMATS.md), reverse-engineered
from a real machine. The table below is the centerpiece; per-harness notes follow.

## The master table

All paths are the real ones the adapters look at. `~` is your home dir; `<AppSupport>` is
`~/Library/Application Support` on macOS, `~/.config` on Linux, `%APPDATA%` on Windows.

| Harness | On-disk location | Parse | Emit | Notes |
|---|---|:--:|:--:|---|
| **Claude Code** | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | ✓ | ✓ | One JSONL per session; threaded via `uuid`/`parentUuid`. **Sub-agents** under `<sid>/subagents/agent-*.jsonl`. Dir-name encoding is lossy — `cwd` is read from inside. |
| **Codex CLI** | `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<ULID>.jsonl` | ✓ | ✓ | Also `~/.codex/archived_sessions/`. **SQLite index** `state_5.sqlite`. 2025 **legacy** variant = single `{session,items[]}` JSON. |
| **Grok CLI** | `~/.grok/sessions/<percent-encoded-cwd>/<sid>/` | ✓ | ✓ | A *directory* per session (`chat_history.jsonl` + sidecars). **Sub-agents** flagged by `session_kind:"subagent"`. cwd is reversible (`%2F`). |
| **OpenCode** | `~/.local/share/opencode/storage/` | ✓ | ✓ | `session/**/ses_*.json` + `message/<sid>/*.json` + `part/<msgid>/*.json`. **Subtask** parts spawn sub-sessions. Two storage generations (inline-summary vs parts). |
| **OpenClaw** | `~/.openclaw/agents/<agentId>/sessions/<sid>.jsonl` | ✓ | ✓ | `sessions.json` index + JSONL transcripts. Header `version` v1/v2 linear → **v3 parent-linked** (migrated in place). ACP-bridged sessions are text-only echoes. |
| **Gemini / Antigravity** | `~/.gemini/tmp/<projectHash>/chats/session-*.json\|jsonl` | ✓ | ✓ | Multi-format, best-effort. Rich chat recordings + `checkpoint-*.json` + `logs.json` fallback. **Sub-agents** nested at `chats/<parentId>/<id>.jsonl`. Antigravity `*.pb` protobufs are opaque (not parsed). |
| **Hermes (Nous)** | `~/.hermes/state.db` (**SQLite**) | ✓ | ✓ | One DB, all sessions; `$HERMES_HOME` overrides. cwd is *not* persisted. Schema drifts live (probe `PRAGMA table_info`). Compression chains linked by `parent_session_id`. |
| **Kimi CLI** | `~/.kimi/sessions/<md5(cwd)>/<uuid>/context.jsonl` | ✓ | ✓ | `$KIMI_SHARE_DIR` overrides. Project dir = `md5(cwd)`; cwd recovered from `kimi.json` `work_dirs[]`. **Legacy** flat form `<hash>/<uuid>.jsonl`. `wire.jsonl` sidecar enriches. |
| **LM Studio** | `~/.lmstudio/conversations/<ms-epoch>.conversation.json` | ✓ | ✓ | Plaintext JSON, one file per chat. No cwd (chat app). Keeps every regenerated variant; we read the selected one. No first-class tool calls. |
| **Cline** | `<AppSupport>/<Editor>/User/globalStorage/saoudrizwan.claude-dev/tasks/<taskId>/` | ✓ | ✓ | VS Code extension; `api_conversation_history.json` is a raw Anthropic-Messages array (≈1:1 with Claude). Also `~/.cline/tasks/`. cwd from first user msg's `<environment_details>`. |
| **Roo Code** | `<AppSupport>/<Editor>/User/globalStorage/rooveterinaryinc.roo-cline\|roo-code/tasks/<taskId>/` | ✓ | ✓ | A **Cline fork** — identical per-task layout, different namespace. Also `~/.roo/tasks/`. Reuses the Cline parser/emitter, tagged `Harness::Roo`. |
| **Continue (continue.dev)** | `~/.continue/sessions/<sid>.json` | ✓ | ✓ | `$CONTINUE_GLOBAL_DIR` overrides. `sessions.json` index + per-session file. OpenAI-shaped messages; cwd = `workspaceDirectory`. |
| **Qwen Code** | `~/.qwen/tmp/<projectHash>/chats/session-*.json\|jsonl` | ✓ | ✓ | A **gemini-cli fork** — byte-identical format, just rooted at `~/.qwen`. Delegates to the Gemini parser, re-tagged `Harness::Qwen`. **Sub-agents** dir-nested like Gemini. |
| **Cursor IDE** | `<AppSupport>/Cursor/User/globalStorage/state.vscdb` (**SQLite**) | ✓ | — | Closed-source, opened read-only. Global DB holds `composerData:`/`bubbleId:` rows; per-workspace DB links composers → cwd. Schema churns (`_v` 1..=10+). |
| **Goose (Block)** | `<AppSupport>/Block.block.goose/sessions/sessions.db` (**SQLite**) | ✓ | — | `$GOOSE_PATH_ROOT`/`$XDG_DATA_HOME` override. Modern `sessions.db` + **legacy** per-session `<name>.jsonl`. MCP-style tool content; read-only, PRAGMA-probed. |
| **Zed (agent panel)** | `<data_dir>/Zed/threads/threads.db` (**SQLite**) | ✓ | — | One DB, one row per thread; `data` BLOB = **zstd-compressed JSON** (pure-Rust decode). Three blob generations (versionless / agent1 0.1–0.2 / agent2 0.3 tagged-enum). cwd + git from the project snapshot; **subagents** linked by `parent_id`. |
| **Claude app** | `<AppSupport>/Claude/` | ✓\* | — | **Detected, not readable.** Transcripts are **server-side** (claude.ai); local store holds only auth/UI state. `claude-code-sessions/*.json` are stubs pointing at `~/.claude/projects/` (handled by Claude Code). |
| **ChatGPT app** | `<AppSupport>/com.openai.chat/conversations-v3-<acct>/<uuid>.data` | ✓\* | — | **Detected, not readable.** History *is* local but **encrypted at rest** with an app-held key (not in a readable Keychain). We count conversations but can't decrypt. |
| **ChatGPT export** | a `conversations.json` you register with `cv config --add-export <path>` | ✓ | — | The "Export data" archive. One file = many conversations; each is a `mapping` DAG (we walk root→`current_node`). Opt-in (no fixed home); deduped by id. |
| **Claude.ai export** | a `conversations.json` you register with `cv config --add-export <path>` | ✓ | — | The claude.ai "Export data" archive. One file = many conversations; linear `chat_messages[]` (text/thinking/tool_use/tool_result). Opt-in; deduped by id. |

`✓` = supported · `—` = not supported · `✓\*` = installation is detected (`storage_root`) but
`discover()` returns empty, for the reasons in the notes.

**13 harnesses are emit-capable** (conversion targets): Claude Code, Codex, Grok, OpenCode, OpenClaw,
Gemini, Hermes, Kimi, LM Studio, Cline, Roo, Continue, Qwen.

**7 are parse-only**: Cursor, Goose, and Zed (closed-source / read-only stores we deliberately don't
write back to), the two desktop apps below, and the two account-data **exports** (`chatgpt-export` /
`claude-export`, registered via `cv config --add-export`).

## Sub-agents

Several harnesses spawn child agents, and clustervision surfaces those as nested sessions you can browse
as a tree — see [sub-agent trees in the app](app.md).

- **Claude Code** writes Task sub-agents to a sibling `<sid>/subagents/agent-<id>.jsonl` (each with a
  `.meta.json` carrying `agentType` and `description`). The discovery walk is depth-limited so these
  don't get mistaken for top-level sessions; they're attached to their parent instead.
- **Grok** tags subagent sessions with `session_kind: "subagent"` (primary sessions instead carry an
  `agent_name`); the kind is preserved in the message `extra`.
- **Gemini and Qwen** dir-nest subagent recordings under `chats/<parentId>/<id>.jsonl`.
- **OpenCode** models them as `subtask` parts — a spawned sub-session with its own prompt, agent, and
  model — alongside `agent` mention parts.
- **Zed** stores a subagent thread as its own row in `threads.db` with a `parent_id` column (and, in
  the 0.3.0 blob, a `subagent_context{parent_thread_id, depth}`); we keep the link in
  `Session::lineage.parent` (with `depth` in `extra["zed"]`).

## The SQLite quartet

Four harnesses keep everything in a SQLite database rather than per-session files: **Hermes**
(`state.db`), **Cursor** (`state.vscdb`), **Goose** (`sessions.db`), and **Zed** (`threads.db`,
whose rows are additionally zstd-compressed JSON blobs). All four are opened
**read-only**, and because their schemas accrete columns across versions without a clean version gate,
each adapter probes `PRAGMA table_info` and selects only the columns that actually exist — so an older
database degrades gracefully instead of failing the whole import. (Codex and Grok also ship SQLite, but
only as *indexes* alongside their JSONL transcripts, not as the primary store.)

## The two desktop apps — detected, not readable

These are the only harnesses we can spot but can't read, and it's worth knowing *why*:

- **Claude app** keeps no transcripts on disk at all. The Electron wrapper around claude.ai stores only
  auth/settings/UI state locally; the actual chat history lives **server-side**. The one local artifact
  is a set of metadata stubs that point (via `cliSessionId`) at `~/.claude/projects/` transcripts — and
  those are already handled by the Claude Code adapter, so re-reading them here would just duplicate.
- **ChatGPT app** *does* store offline history locally — one `.data` file per conversation — but every
  file is **encrypted at rest** with a key the app holds in its own protected keychain access group.
  There's no plaintext index and no readable Keychain item, so we can detect the install and count
  conversations but can't decrypt a single one.

Both still report a `storage_root()` so the install is detectable; their `discover()` simply returns
empty.

## Bring your own harness

Don't see your agent here? Adding one is a single new module implementing the `Adapter` trait —
`discover` + `parse`, and an optional `emit` to make it a conversion target. The full walkthrough is in
[Adding a harness](adding-a-harness.md).
