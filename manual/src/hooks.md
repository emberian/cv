# Inside the harness: hooks

Hooks let cv act inside a running session with no tool call at all: surface compactions with a
way back, brief a new session on this directory's history, ledger lanes, surface owed decisions at
each prompt, and ask before editing a file another live session is editing (Codex: deny).

```sh
cv hooks install claude                 # ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR)
cv hooks install claude --scope project # ./.claude/settings.json
cv hooks install codex                  # ~/.codex/hooks.json (or $CODEX_HOME)
cv hooks install codex --scope project  # ./.codex/hooks.json
cv hooks show claude                    # what install would change; --json for the before/after
cv hooks remove claude                  # exactly cv's entries and the skill, nothing else
```

`install` is idempotent: it merges six entries (`lane` serves both sub-agent events) beside
existing `hooks` and copies the [`cv` skill](#the-skill) to `skills/cv/SKILL.md`. Each runs
`<absolute cv> hook <name> --harness <h>`; a reinstall from a moved binary replaces them. Invalid
JSON settings are left untouched and the command errors. Hooks apply to new sessions; `cv doctor`
shows them. Any `--harness` spelling works (`cc`, `claude-code`).

Verified contracts: Claude Code 2.1.296 ([hooks reference](https://code.claude.com/docs/en/hooks))
and Codex 0.163.0. Other harnesses error.

Codex skips untrusted handlers silently: accept cv's entries in `codex`'s startup hooks review
(automation: `codex exec --dangerously-bypass-hook-trust`); project scope also needs the
directory trusted. A moved binary or edited matcher makes an entry *modified*, untrusted again.
`cv hooks install|show codex` and `cv doctor` report each entry as trusted, untrusted or modified. cv reads Codex's own `[hooks.state]` table in
`~/.codex/config.toml` (key and hash checked against Codex 0.162.1's `hooks/list`).

## The hooks

Each `cv hook <name>` reads hook JSON on stdin, prints one line or nothing, and exits 0 (errors
go to stderr); with a terminal on stdin it exits 2 with usage. None scans the fleet.

| event · matcher | command | prints |
|---|---|---|
| `SessionStart` · `compact` | `cv hook compact` | `additionalContext`: `context compacted (auto, 180,412 tokens) at message N — compaction #k`, then `cv show <id> --pre-compaction k`, `cv show <id> --kind prompt,answer --pre-compaction k` and `cv rewind <id> --at N-1` |
| `SessionStart` · `startup\|resume` | `cv hook start` | `additionalContext`: up to six of this directory's earlier sessions (id, age, messages, title, the last commit sha its transcript's tail shows) and the count of decisions owed |
| `SubagentStart`, `SubagentStop` (`async`) | `cv hook lane` | nothing; records `{ts, event, session_id, agent_id, agent_type, transcript_path, agent_transcript_path, cwd, last_text}` and live OS PID / boot metadata in `$CLUSTERVISION_HOME/state.db` |
| `UserPromptSubmit` | `cv hook decisions` | `additionalContext` only when `cv task inbox` has decisions owed to `$CV_ENDPOINT` (else `cv`): the count and the commands that answer them |
| `PreToolUse` · every tool (opt-in: `cv hooks install --retired`) | `cv hook retired` | `permissionDecision: "deny"` when the calling sub-agent (`agent_id`) is retired in `$CLUSTERVISION_HOME/state.db` (written by `cv distill --agent-of`, listed and reversed by `cv retire`), telling it its successor and to end its turn; the same for a `SendMessage` whose `to` is retired, naming the successor to send to; nothing (allow) otherwise |
| `PreToolUse`, `PostToolUse` · every tool (opt-in: `cv hooks install --receipts`) | `cv hook receipt` | appends a BLAKE3 hash-chained pre/post tool-call receipt to `$CLUSTERVISION_HOME/state.db` (`CV_RECEIPT_STRICT=1` denies a call whose pre-receipt cannot be durably written) |
| `PreToolUse` · `Edit\|Write` | `cv hook edit` | `permissionDecision: "ask"` with the session id and minutes since, when another session edited this exact path and its transcript changed in the last 30 minutes; nothing (allow) otherwise |

- Hooks answer inside the harness's turn, so each reads at most 8 MiB of transcript (streaming
  costs ~4.5 ms/MB against a 100 ms budget) and says in its text what it left unread.
- **compact** gives the exact index and ordinal up to 8 MiB; above, it reads the last 8 MiB for
  trigger and size, says so, and points at `cv show <id> --kind compaction_boundary,compaction_summary`.
  [`cv show --pre-compaction`](cli.md#cv-show) reads the compacted records;
  [`cv rewind --at`](cli.md#cv-rewind) resumes before them.
- **start** reads the catalog without a freshness probe: sessions newer than the last `cv ls` /
  `cv archive sync` are missing. The sha is the last `[branch sha]` line in the transcript's tail,
  the 8 MiB split across the rows; a row with none there says how far back it looked.
- **Codex** uses the same events; the edit matcher is `apply_patch`, and the guard checks every
  added, updated, removed or moved path. Codex rejects `ask`, so a live conflict is `deny`.
  Threads under one root (`parent_thread_id` headers, or the lane ledger for an unindexed child)
  count as one session, as a Claude session's lanes do, so a swarm editing its own files is never
  denied. To override, remove the edit handler from `hooks.json` (`cv hooks remove codex` removes
  all five). `cv rewind` supports only Claude sessions. Codex's forest includes children
  registered by installed hooks; older unregistered children are absent.
- **lane** writes the ledger [`cv lanes`](swarm.md) reads. A `stop` row at or after a lane's last
  turn makes it `stopped` with `status_source: hook_ledger`, before the transcript records it.
- **edit** looks up the exact path in the event index (`cv touched`'s table), so sessions not
  ingested by `cv index` or `cv events <id>` are invisible. It skips the calling session and agent.

Measured at 0.16.1 on a 6,704-session, 37 GB store: each hook takes 10–30 ms; `compact` takes
70–90 ms on a 16.5 MB transcript (hence the 8 MiB limit), 10 ms on the tail path.

## The skill

`crates/cv/skill/SKILL.md`, which `install` copies: when `rewind`, `show --pre-compaction`,
`distill`, `splice`, `adopt` and `lanes` apply, plus the table above. The copy must match the
binary's embedded bytes; `cv hooks show` flags a stale copy and `install` rewrites it.

## The compaction policy

`cv doctor`'s `Policy:` line combines user and project settings (project wins) and the
environment (wins over a settings `env` block): `autoCompactEnabled` (default on),
`idleCompaction` (default off; when enabled server-side, fires at 90% of a 200K-token floor),
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `DISABLE_AUTO_COMPACT`.
No hook can supply or replace the summary (`PreCompact` can only block), so `compact` makes the
loss visible and reversible, not preventable.

## Every other harness

Reading needs no hooks. cv ships hooks only for harnesses checked against their documentation.
