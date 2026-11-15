# Antigravity real-writer corpus

`generate.py` drives the installed **1.5.9** CLI against a deterministic local
Gemini SSE endpoint. The mock supplies model replies and tool calls; Antigravity
executes the tools and writes its own SQLite records. It uses no account or model
credentials and makes no paid calls.

```sh
python3 -I tools/harness-corpus/antigravity/generate.py \
  --bin /absolute/path/to/agy --scenario SCENARIO \
  --out crates/cv-core/tests/corpus/antigravity/NEW-VERSION
```

The output must not exist. A new fixture records the executable version and
SHA-256, local mock-call count, selection, and scrub procedure. Generate its
`summary.json` with the existing corpus golden test; do not overwrite a committed
version.

Each scenario validates what the writer recorded before anything is staged; a
capture that did not reach its scripted state is never published.

| `--scenario` | Driver | What the writer is made to record |
|---|---|---|
| `native-background-media` (default) | `--print` | A command backgrounded after 500 ms, its completion message, an image tool result. Native store only. |
| `permissions` | ACP, then `--print` | One approved and one denied command; then a gated command in print mode, which cannot ask. Native store only. |
| `interruption` | ACP | A running tool cancelled and retried; an upstream 503, a dropped stream and a cancelled stream, then a normal turn. |
| `background-tasks` | `--print` | Three background commands that finish out of launch order; one killed, one polled. |
| `swarm` | ACP | A coordinator, two workers and a helper one worker adds; board posts, a parked worker, a kill. |
| `subagents` | `--print` | Two `invoke_subagent` children, a parent message to one, their idle returns. |
| `prompt-media` | ACP | An inline prompt image, linked files, an audio block. Native store only; the writer's stored upload is omitted. |
| `compaction-resume` | `--print` ×4 | A turn, `/compact` on resume, the exported logs removed, two more resumes. |

`--print` runs one prompt. The CLI's ACP stdio server (`agy acp`) is the only
non-interactive channel that answers a permission request, cancels a turn, sends
prompt media and keeps a session alive while a swarm wakes it. The mock answers the
title-generation request itself and counts only scripted agent replies against the
scenario's budget; members of a swarm and children are routed by a marker in their
own brief.

Where a scenario is about how the exported log and the store disagree, the fixture
keeps the writer's `transcript_full.jsonl`, and for swarms and children its
`swarm/` and `subagents/` registries. Otherwise the log is omitted so the reader
exercises native SQLite fallback.

Every invocation uses a fresh HOME, app-data, XDG and temporary directory. A macOS
`sandbox-exec` profile denies writes outside that directory (except PTY devices)
and denies non-loopback network, including descendants. There is no unsandboxed
fallback. The CLI's auto-update is disabled, its environment contains no inherited
credentials, and a timeout stops only the owned process group.

The capture uses SQLite's backup API. It retains actual `steps` and
`trajectory_meta` rows, empties unrelated request metadata tables, vacuums to
4 KiB SQLite pages, and applies
only equal-width scratch-path substitutions to native byte cells and to the kept
JSON files. Padding the placeholder path preserves nested protobuf length prefixes.
Of `trajectory_metadata_blob` it keeps whole top-level lineage fields (workspace,
created, trajectory, parent, root, agent type and role) and drops the agent
configuration beside them, which holds system prompts; a top-level field has no
enclosing length prefix, so the kept bytes stay valid. It refuses residual
real paths or dummy credentials. Do **not** run the generic byte-replacing
`scrub.py` over native protobuf: variable-length substitutions corrupt its wire
format. The corpus privacy check and native parse must both pass before committing.

Not reachable with this CLI, mock and sandbox, each with what was observed:

- **Audio or video prompt media.** The ACP server advertises `audio: false`; it
  accepts an audio block and stores no media for it. `--print` takes text only.
- **A renamed swarm member.** The `swarm` tool has `status`, `models`, `add`,
  `model`, `kill` and `title`; `title` names the swarm, nothing renames a member.
- **Native `CLEARED` rows.** `/compact` writes a CHECKPOINT step and leaves every
  earlier step `DONE` in a short session; no trigger for clearing was found.
- **A worker captured mid-turn.** Members are parked or killed when the scenario
  ends, so `wake_mode` for a member that is working is not in a fixture.
- **An approval answered in print mode.** Without `--dangerously-skip-permissions`
  the writer cancels the gated step itself.

A captured ACP session must be closed only after the writer has stored its last
revision: closing straight after a denial captured the step as `CANCELED` with no
answer (the scenario now waits for `ERROR`). The OS sandbox and local gateway
behavior are verified for macOS CLI 1.5.9 only.
