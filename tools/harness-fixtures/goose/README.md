# Goose fixture — written by goose 1.51.0 itself

Produces `crates/cv-core/tests/fixtures/goose/sessions-v16-1.51.0.db`: a real Goose sqlite store
(schema 16), written by a real `goose` binary, with no model call and no local Goose install
involved.

## Prerequisites

- A Goose checkout at `~/pug/goose` (override with `GOOSE_SRC`), built:
  `cd ~/pug/goose && cargo build -p goose-cli`. The fixture in the repo came from `2090ad1c`
  (2026-09-19), which reports itself as 1.51.0. `GOOSE_BIN` points at a specific binary.
- `python3` (stdlib only), `sqlite3` and `curl` on `PATH`.
- A free TCP port (default 18080; `PORT` overrides).

The script checks all of these before it starts and names the one that is missing. It also refuses
to run if something is already listening on `$PORT` — a stub that never binds leaves goose talking
to someone else's server, and the fixture comes out looking fine.

## The one command

```sh
tools/harness-fixtures/goose/generate.sh
```

- `--out PATH` (or `OUT=PATH`) writes the fixture somewhere else — use it to diff a regenerated
  store against the committed one instead of replacing it.
- `--keep` leaves the throwaway home and the stub log in the temp dir for inspection.

## What it produces

Four sessions, 13 messages, chosen so every branch of `harness/goose.rs` has something real to read:

| session | why it is there |
|---|---|
| `stubturn` | a completed turn against the stub — `messages.metadata_json` carries `usage` and `inference.requestedModel`, and `--name` becomes the session title (`sessions.name`, `user_set_name = 1`) |
| `deadturn` | provider unreachable. Goose prints `Network error:` and writes **no assistant row at all** — the session keeps only the prompt and the `<turn-context>` row. `harness/goose.rs` asserts that absence |
| `retryfail` | `retry-fail.yaml`, a recipe whose shell success check is `false`; Goose records retry exhaustion as **plain text, not an `error` block** — the thing the adapter would otherwise guess wrong |
| the import | `goose session import` of cv's own `tests/fixtures/claude/rich_blocks.jsonl`: Goose's importer turns a Claude transcript into `thinking` + `toolRequest` / `toolResponse` and an `image` block, and stamps `<turn-context>` as `userVisible: false` |

`stub_openai.py` is a ~50-line OpenAI-compatible chat-completions server that answers everything
with a canned reply plus a `usage` block (including `prompt_tokens_details.cached_tokens`), in SSE
or JSON. It exists so a turn can *complete* — usage and model only land in `metadata_json` when one
does.

The run asserts 4 sessions before writing anything, and says so if the message count is not 13.

## What a regenerated fixture does *not* match

Verified by running this on 2026-09-19 against the pinned checkout and diffing the result against
the committed fixture: identical schema, identical session count, identical per-session block
types, same message count. The one difference is the ids.

**Session ids are `YYYYMMDD_<n>` from today's UTC date** (`crates/goose/src/session/
session_manager.rs`, `%Y%m%d`). `goose run` can pin one with `--session-id`, but that flag is
mutually exclusive with `--name` — which is what puts the title in the fixture — and `goose session
import` has no id flag at all. So every regeneration renumbers all four, and the
`by_id("20260919_…")` assertions in `crates/cv-core/src/harness/goose.rs` (~line 1280) have to be
updated to the ids the run prints at the end.

## Where the fixture goes

`crates/cv-core/tests/fixtures/goose/sessions-v16-1.51.0.db`, read by the `goose` tests in
`crates/cv-core/src/harness/goose.rs`. The generator `VACUUM INTO`s the store so the fixture is one
file with no `-wal`/`-shm` siblings.

## Safety

`GOOSE_PATH_ROOT` relocates Goose's config, state and data roots together, so your real
`~/.config/goose` and `~/Library/Application Support/Block.block.goose` are never read or written.
`GOOSE_DISABLE_KEYRING=1` keeps it out of the system keychain. The only file written outside the
temp dir is the fixture.
