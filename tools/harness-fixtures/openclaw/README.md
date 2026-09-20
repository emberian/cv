# OpenClaw fixture — written by OpenClaw's own store code

Produces `crates/cv-core/tests/fixtures/openclaw/openclaw-agent.sqlite`: the sqlite transcript store
OpenClaw switched to on 2026-07-11, written by calling OpenClaw's own session accessor — not by cv
guessing at a schema. That switch is why the adapter needed rewriting at all; a hand-built fixture
would have baked in the same guess twice.

## Prerequisites

- An OpenClaw checkout at `~/pug/openclaw` with its dependencies installed (`bun install` or
  `npm install` — whatever that tree uses). The fixture in the repo came from `0e9181234a`
  (2026-09-19).
- `scripts/tsx.mjs` in that checkout (OpenClaw ships it; it is the TS loader the command below
  uses).
- The generator imports `../src/config/sessions/session-accessor.js`,
  `../src/config/sessions/transcript-visible-events.js`,
  `../src/config/sessions/legacy-sqlite-marker.js` and `../src/agents/sessions/session-manager.js`,
  so it must be run **from inside the OpenClaw checkout**, with the relative paths intact.

## The one command

```sh
cp tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts \
   ~/pug/openclaw/scripts/cv-openclaw-fixture.mts
cd ~/pug/openclaw && OPENCLAW_STATE_DIR=$(mktemp -d) \
   node --import ./scripts/tsx.mjs scripts/cv-openclaw-fixture.mts
```

It prints the state dir it used; the store is at
`<state>/agents/main/agent/openclaw-agent.sqlite`. Copy that over
`crates/cv-core/tests/fixtures/openclaw/openclaw-agent.sqlite`.

## What it produces

A store with the three shapes the adapter has to get right:

- a **live session** driven through `SessionManager` (`session_windows` + `session_nodes` +
  `transcript_events` rows, written by OpenClaw's own append path);
- a **fork** off it, so the branch/leaf logic has a real second window to resolve;
- the **legacy v3 JSONL** migrated in the way a migration lands it — `upsertSessionEntryCore` for
  the index entry plus `replaceTranscriptEventsSync` for the lines.

For each it also dumps `openclaw-stored-<id>.json` and `openclaw-visible-<id>.json` (the output of
OpenClaw's own `selectVisibleTranscriptEvents`) into the state dir. Those two files are the point of
the exercise: they are what the adapter's visibility rules are checked against.

## Safety

The script refuses to run unless `OPENCLAW_STATE_DIR` is set to something that is neither `$HOME`
nor `~/.openclaw`. It **reads** `~/.openclaw/agents/main/sessions/*.jsonl` (to migrate a real legacy
transcript) and writes only under the state dir you gave it.

---

Moved here from `crates/cv-core/tests/fixtures/openclaw/` on 2026-09-19 — every real-writer
generator lives under `tools/harness-fixtures/` now. The module doc of
`crates/cv-core/src/harness/openclaw.rs` still says the script sits "next to" the fixture; it sits
here.
