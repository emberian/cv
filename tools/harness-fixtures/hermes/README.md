# Hermes fixture — written through Hermes's own store API

Produces `crates/cv-core/tests/fixtures/hermes/state-v30.db`: a real Hermes `state.db` at schema 30,
built by calling `hermes_state.SessionDB` methods — `archive_and_compact`, `publish_compression_child`,
`promote_to_session_reset`, `import_foreign_session` — with **no raw SQL anywhere**. That is the
whole point. Hermes's schema went 16 → 30 between the two surveys and its compaction moved in-place;
a fixture cv `INSERT`ed by hand would have encoded cv's belief about the new schema, not Hermes's.

## Prerequisites

- A Hermes checkout at `~/pug/hermes-agent` (override with `HERMES_SRC`; the fixture in the repo
  came from `6d8a8bebf7`, 2026-09-19), with its Python environment importable: `hermes_state`,
  `hermes_state_ids`, `agent.context_compressor`, `hermes_cli.foreign_sessions`. Verified
  2026-09-19 with the system `python3` (3.13, Homebrew); `~/pug/hermes-agent/.venv/bin/python`
  also works.
- A small Claude Code `.jsonl` to import. Defaults to
  `crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl`; see the caveat below.

Every prerequisite is checked before any session is created, and each failure prints one line
saying which variable to set.

## The one command

Run it from anywhere — it puts `$HERMES_SRC` on `sys.path` itself:

```sh
HERMES_HOME=$(mktemp -d) OUT=/tmp/state-v30.db \
  python3 tools/harness-fixtures/hermes/generate.py
```

It prints the db path it opened, the session ids it made, and VACUUMs the finished store into
`$OUT`. Then `cp /tmp/state-v30.db crates/cv-core/tests/fixtures/hermes/state-v30.db`.

`OUT` refuses to overwrite an existing file (`VACUUM INTO` will not), so it can never land on the
committed fixture by accident. Leave `OUT` unset to keep the store in `$HERMES_HOME` instead.

## What it produces

**Ten** sessions, each one a shape the adapter has to resolve:

| id key | what it exercises |
|---|---|
| `A_compacted` | in-place compaction (`archive_and_compact`): a `SUMMARY_PREFIX` summary row plus the carried tail, with the pre-compaction rows left behind as `active=0` — the `active`/`compacted` filter the adapter needs. Also a deliberately **non-monotonic** timestamp (an assistant row older than the prompt it answers), which is why ordering is `ORDER BY id`, a `steer` and a `hidden` display-kind row, and a delegation delivery |
| `R_root` + `B_branch`, `S_reset`, `D_delegate` | the three `model_config` lineage markers `_branched_from` / `_reset_from` / `_delegate_from`, each written the way the command that makes it writes it (child created *before* the parent is ended, history copied in one batch) |
| `C1_rotated_parent` → `C2_rotated_child` | the legacy compression **rotation** chain (`publish_compression_child`): a new session seeded with the summary plus the tail, pointing back at its parent |
| `X_archived`, `Y_hidden` | the listing flags |
| `F_foreign_claude` | `import_foreign_session("claude", …)` — Hermes's own importer, so the imported-origin columns are real |

It also writes `$GEN_OUT` (`hermes-views.json`, next to `$OUT` by default): Hermes's **own**
answers — `list_sessions_rich`, `get_messages(include_compacted=…/include_inactive=…)`,
`get_messages_as_conversation` with and without ancestors, and the raw session rows. When cv and
Hermes disagree about what a session contains, that file is the arbiter; it is not committed,
regenerate it alongside the db.

## What a regenerated fixture does *not* match

Verified by running this on 2026-09-19 against the pinned checkout and diffing the result against
the committed fixture. Nine of the ten sessions come back identical in shape; the differences are:

- **Session ids change every run.** Hermes derives them from the wall clock
  (`YYYYMMDD_HHMMSS_<hex>`), so the `const A/R/B/S/D/C1/C2/X/Y/F` block at the top of
  `crates/cv-core/src/harness/hermes.rs` (~line 2320) has to be updated to the ids the run prints.
  Running the harness tests before doing that fails, whatever else is right.
- **The imported Claude session differs.** The committed fixture was imported from a transcript
  whose first message is `"Reply with exactly: ONE"`, which `harness/hermes.rs` asserts on — that
  file is not in the repo. The default `rich_blocks.jsonl` imports cleanly but gives a 3-message
  session titled *"Imported from Claude Code: please refactor"*, so that assertion needs updating
  too, or `FOREIGN_CLAUDE_PATH` needs to point at the original.
- **A fresh store has FTS.** Hermes creates `messages_fts*` and `messages_fts_trigram*` whenever the
  interpreter's SQLite has fts5 (it does on this machine). The committed fixture has none of them,
  so it was made on a build without fts5. cv's adapter does not read them; the store is otherwise
  identical.

## Safety

`HERMES_HOME` is the documented redirect (`hermes_constants.get_hermes_home`: context-local
override → `HERMES_HOME` → platform default). When it is **unset Hermes does not refuse** — it logs
a warning and falls back to `~/.hermes`, which on a machine that runs Hermes is your real state. So
this script refuses to start unless `HERMES_HOME` is set and is neither `$HOME` nor `~/.hermes`, and
it re-checks the path `SessionDB` actually opened before writing anything.
