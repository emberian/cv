# OpenClaw fixture — written by OpenClaw's own store code

Produces `crates/cv-core/tests/fixtures/openclaw/openclaw-agent.sqlite`: the sqlite transcript store
OpenClaw switched to on 2026-07-11, written by calling OpenClaw's own session accessor — not by cv
guessing at a schema. That switch is why the adapter needed rewriting at all; a hand-built fixture
would have baked in the same guess twice.

## Prerequisites

- An OpenClaw checkout at `~/pug/openclaw` (override with `OPENCLAW_SRC`) with its dependencies
  installed — that tree uses pnpm. The fixture in the repo came from `0e9181234a` (2026-09-19).
- `scripts/tsx.mjs` in that checkout (OpenClaw ships it; it is the TS loader the command below
  uses), and Node ≥ 22.5 for `node:sqlite`, which the trim step uses.
- Optionally a legacy v3 JSONL store to migrate. The default is
  `~/.openclaw/agents/main/sessions/`, read-only; override with `OPENCLAW_LEGACY_DIR`. Without one
  the run still succeeds, it just says so and leaves the fixture without the migrated session.

The script refuses with a one-line reason if any of these is missing — a missing checkout, missing
`node_modules`, an unset `OPENCLAW_STATE_DIR`, or the wrong cwd.

## The one command

```sh
cd ~/pug/openclaw          # tsx resolves OpenClaw's tsconfig paths against the cwd
OPENCLAW_STATE_DIR=$(mktemp -d) OPENCLAW_OUT=/tmp/openclaw-agent.sqlite \
  node --import ./scripts/tsx.mjs ~/dev/cv/tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts
```

Nothing is copied into the checkout: the script lives here and imports OpenClaw's modules by
absolute path out of `$OPENCLAW_SRC`. It must still be run *from* the checkout root — tsx resolves
OpenClaw's tsconfig `paths` against `process.cwd()`, and from anywhere else the first bare import
inside OpenClaw's own source dies with `ERR_MODULE_NOT_FOUND`.

Then `cp /tmp/openclaw-agent.sqlite crates/cv-core/tests/fixtures/openclaw/openclaw-agent.sqlite`.

## What it produces

A store with the three shapes the adapter has to get right:

- a **live session** driven through `SessionManager` (`session_windows` + `session_nodes` +
  `transcript_events` rows, written by OpenClaw's own append path);
- a **fork** off it, so the branch/leaf logic has a real second window to resolve;
- the **legacy v3 JSONL** migrated in the way a migration lands it — `upsertSessionEntryCore` for
  the index entry plus `replaceTranscriptEventsSync` for the lines.

Verified 2026-09-19: 3 `session_windows`, 2 `session_nodes`, 46 `transcript_events`, 46
`transcript_event_identities` — the same counts as the committed fixture, and `.schema` identical
to it.

For each session it also dumps `openclaw-stored-<id>.json` and `openclaw-visible-<id>.json` (the
output of OpenClaw's own `selectVisibleTranscriptEvents`) into `$OPENCLAW_STATE_DIR`. Those two
files are the point of the exercise: they are what the adapter's visibility rules are checked
against.

### `OPENCLAW_OUT` and the trim

A store OpenClaw just created carries ~55 tables — memory index, FTS shadows, boards, auth — and is
~670 KB. cv's adapter reads five: `schema_meta`, `session_windows`, `session_nodes`,
`transcript_events`, `transcript_event_identities`. With `OPENCLAW_OUT` set the script `VACUUM
INTO`s a copy (a plain file copy would lose everything still sitting in the `-wal`), drops
everything else including the explicit indexes, VACUUMs again, and prints what it dropped. That
copy is the ~82 KB fixture. Without `OPENCLAW_OUT` you get only the raw store and the path to it.

### What is not reproducible

The fork's session id is a fresh UUIDv7 on every run, so the fixture's ids change. `cvfix-main-0001`
and the migrated session's id are stable.

## Safety

The script refuses to run unless `OPENCLAW_STATE_DIR` is set to something that is neither `$HOME`
nor `~/.openclaw`. It **reads** the legacy JSONL dir and writes only under the state dir you gave
it and `$OPENCLAW_OUT`.

---

Moved here from `crates/cv-core/tests/fixtures/openclaw/` on 2026-09-19 — every real-writer
generator lives under `tools/harness-fixtures/` now. The module doc of
`crates/cv-core/src/harness/openclaw.rs` still says the script sits "next to" the fixture; it sits
here.
