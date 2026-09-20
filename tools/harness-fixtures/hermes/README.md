# Hermes fixture — written through Hermes's own store API

Produces `crates/cv-core/tests/fixtures/hermes/state-v30.db`: a real Hermes `state.db` at schema 30,
built by calling `hermes_state.SessionDB` methods — `archive_and_compact`, `publish_compression_child`,
`promote_to_session_reset`, `import_foreign_session` — with **no raw SQL anywhere**. That is the
whole point. Hermes's schema went 16 → 30 between the two surveys and its compaction moved in-place;
a fixture cv `INSERT`ed by hand would have encoded cv's belief about the new schema, not Hermes's.

## Prerequisites

- A Hermes checkout at `~/pug/hermes-agent` (the fixture in the repo came from `6d8a8bebf7`,
  2026-09-19), with its Python environment importable: `hermes_state`, `hermes_state_ids`,
  `agent.context_compressor`, `hermes_cli.foreign_sessions`.
- A small Claude Code `.jsonl` to import — `crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl`
  does the job.
- `sqlite3` for the final VACUUM.

## The one command

Run it **from the checkout root** (the script does `sys.path.insert(0, os.getcwd())`):

```sh
cd ~/pug/hermes-agent
HERMES_HOME=$(mktemp -d) \
GEN_OUT=/tmp/hermes-views.json \
FOREIGN_CLAUDE_PATH=$HOME/dev/cv/crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl \
  python3 ~/dev/cv/tools/harness-fixtures/hermes/generate.py
```

It prints the db path it opened and the session ids it made. Then:

```sh
sqlite3 "$HERMES_HOME/state.db" "VACUUM INTO '/tmp/state-v30.db'"
cp /tmp/state-v30.db ~/dev/cv/crates/cv-core/tests/fixtures/hermes/state-v30.db
```

## What it produces

Nine sessions, each one a shape the adapter has to resolve:

| id key | what it exercises |
|---|---|
| `A_compacted` | in-place compaction (`archive_and_compact`): a `SUMMARY_PREFIX` summary row plus the carried tail, with the pre-compaction rows left behind as `active=0` — the `active`/`compacted` filter the adapter needs. Also a deliberately **non-monotonic** timestamp (an assistant row older than the prompt it answers), which is why ordering is `ORDER BY id`, a `steer` and a `hidden` display-kind row, and a delegation delivery |
| `R_root` + `B_branch`, `S_reset`, `D_delegate` | the three `model_config` lineage markers `_branched_from` / `_reset_from` / `_delegate_from`, each written the way the command that makes it writes it (child created *before* the parent is ended, history copied in one batch) |
| `C1_rotated_parent` → `C2_rotated_child` | the legacy compression **rotation** chain (`publish_compression_child`): a new session seeded with the summary plus the tail, pointing back at its parent |
| `X_archived`, `Y_hidden` | the listing flags |
| `F_foreign_claude` | `import_foreign_session("claude", …)` — Hermes's own importer, so the imported-origin columns are real |

It also writes `$GEN_OUT` (`hermes-views.json`): Hermes's **own** answers — `list_sessions_rich`,
`get_messages(include_compacted=…/include_inactive=…)`, `get_messages_as_conversation` with and
without ancestors, and the raw session rows. When cv and Hermes disagree about what a session
contains, that file is the arbiter; it is not committed, regenerate it alongside the db.

## Safety

`HERMES_HOME` is the documented redirect (`hermes_constants.get_hermes_home`: context-local
override → `HERMES_HOME` → platform default), and Hermes itself refuses to let tests run against a
non-temporary home. Point it at a `mktemp -d` and your real Hermes state is never opened.
