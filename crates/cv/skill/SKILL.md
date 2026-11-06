---
name: cv
description: ClusterVision (`cv`) reads every agent session on this machine, including yours. Use when context was compacted and you need what was lost, when a lane of yours should be rewound, distilled or adopted, when you want to know what another session is doing to the same files, or to install the Claude Code or Codex hooks that surface these moments automatically.
---

# cv — the session store as an instrument

`cv` reads the harnesses' own transcripts; nothing has to be wired for it to see a session.
Every command takes a unique session id prefix or an `agent-<id>`. For Codex, use
`codex:<full-uuid>`: neighboring UUIDv7 sessions often share an eight-character prefix.

## After a compaction

The pre-compaction records are still in your transcript; only your context lost them.

- `cv show <id> --kind compaction_boundary,compaction_summary` — every boundary (trigger, tokens,
  message index).
- `cv show <id> --pre-compaction N` — the span the Nth compaction discarded, verbatim.
- `cv show <id> --kind prompt,answer --pre-compaction N` — only the human prompts of that span
  (cheap first look).
- `cv rewind <id> --at <msg|sha>` (Claude sessions) — a NEW resumable session of the agent as it stood at a
  message or at the commit it made; `claude --resume <new-id> --fork-session`.

## Reshaping a lane (sub-agent)

- `cv lanes <session>` / `cv lanes --recent 5 --unfinished` — every lane with status, tokens,
  last line; `--stranded` lists lanes parked on a promise nothing will keep.
- `cv distill <id> --session` — the same lane at a sixth of the context, resumable; its brief,
  timeline and last N tool calls verbatim, the rest indexed (`cv cat <new> <tool_use_id>`).
- `cv splice <id>:..N --session` (or `--agent-of <root>`) — the lane exactly as it stood at message N, to run variants.
- `cv adopt <agent-id> --into <live-root>` — a dead root's lane moved under a live one so it can
  `SendMessage` it; `cv lanes --recent N --unfinished` is the rescue list. A lane that hit a
  model's usage limit: `cv adopt <agent-id> --into <its-root> --model <ID>` retargets it in place.
- `cv show <agent-id>` / `cv show <session> --subagents` — read a lane, or the whole forest.

## Before editing a file someone else is on

- `cv touched <path> --edits-only` — which sessions edited it (indexed; `cv index` refreshes).
- `cv show <id> --last 10` — what that session did last.

## Pulling prior work into context

- `cv ls --cwd <dir>` — this project's sessions, newest first.
- `cv pack "<task>"` — a capped markdown pack of the sessions relevant to a task.
- `cv search "<words>"` — full text across every harness.
- `cv doctor [<id>]` — why the context keeps filling, by source; the hooks line says whether
  the hooks below are installed and what the compaction policy in force is.

## The hooks

`cv hooks install claude [--scope user|project]` merges five hooks into Claude Code's
settings (`~/.claude/settings.json`, or `<project>/.claude/settings.json`) and copies this skill
beside them; it is idempotent, `cv hooks show claude` prints what it would change, and
`cv hooks remove claude` deletes exactly its entries. Each hook is `cv hook <name>`, reads the
hook JSON on stdin, prints at most a line or two:

For Codex, use the same commands with `codex`: user scope writes `$CODEX_HOME/hooks.json`
(default `~/.codex/hooks.json`), project scope writes `.codex/hooks.json`. Codex skips them until
you accept them in its startup hooks review (`codex exec --dangerously-bypass-hook-trust` for
automation); `cv hooks show codex` says whether each is trusted. The edit matcher
is `apply_patch`; every path in its patch is checked. A live conflict blocks the patch
with `deny` because Codex does not support `ask`; your own sub-agents and parent are not another
session. Coordinate with the other session before editing. To override after coordinating, remove the edit handler from `hooks.json`. Compaction recovery uses `cv show`;
`cv rewind` currently supports Claude sessions.

| event | `cv hook` | what you see |
|---|---|---|
| `SessionStart` (`compact`) | `compact` | "context compacted (auto, N tokens) at message M" + the three commands above |
| `SessionStart` (`startup\|resume`) | `start` | this directory's last sessions: id, age, title, last commit, decisions owed |
| `SubagentStart` / `SubagentStop` | `lane` | nothing — a row in the ledger `cv lanes` reads |
| `UserPromptSubmit` | `decisions` | one line when `cv task inbox` has decisions owed to you |
| `PreToolUse` (`Edit\|Write`) | `edit` | an `ask` when another live session edited the same file |
