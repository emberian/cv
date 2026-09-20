#!/usr/bin/env bash
# Regenerate tests/fixtures/goose/sessions-v16-1.51.0.db — written by goose itself, not by cv.
#
# Starts the stub OpenAI server next to this script, points a freshly built goose at a throwaway
# GOOSE_PATH_ROOT, and drives four sessions that between them exercise everything cv's goose adapter
# reads: a completed turn (usage + model in messages.metadata_json), a turn against a dead endpoint,
# a recipe whose success check can never pass (retry exhaustion), and an import of cv's own Claude
# fixture (thinking, a tool call/result pair, an image). Then it VACUUMs the store into the fixture.
#
# No model call, no network, and nothing outside $WORK and the output file is touched.
#
#   tools/harness-fixtures/goose/generate.sh [--keep] [--out PATH]
#
#   --keep        leave the throwaway home and the stub log in the temp dir
#   --out PATH    write the fixture here instead of the committed one (also: OUT=PATH)
#
# Env: GOOSE_SRC (default ~/pug/goose), GOOSE_BIN (default $GOOSE_SRC/target/debug/goose),
#      PORT (default 18080), CV_REPO (default: the repo this script lives in), OUT.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CV_REPO="${CV_REPO:-$(cd "$HERE/../../.." && pwd)}"
GOOSE_SRC="${GOOSE_SRC:-$HOME/pug/goose}"
GOOSE_BIN="${GOOSE_BIN:-$GOOSE_SRC/target/debug/goose}"
PORT="${PORT:-18080}"
OUT="${OUT:-$CV_REPO/crates/cv-core/tests/fixtures/goose/sessions-v16-1.51.0.db}"
CLAUDE_FIXTURE="$CV_REPO/crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl"
KEEP=0

usage() { sed -n '2,19p' "${BASH_SOURCE[0]}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --out) [ $# -ge 2 ] || { echo "--out needs a path" >&2; exit 2; }; OUT="$2"; shift ;;
    --out=*) OUT="${1#--out=}" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# ── prerequisites ─────────────────────────────────────────────────────────────────────────────
fail=0
for t in python3 sqlite3 curl; do
  command -v "$t" >/dev/null || { echo "missing prerequisite: $t (not on PATH)" >&2; fail=1; }
done
if [ ! -x "$GOOSE_BIN" ]; then
  echo "no goose binary at $GOOSE_BIN" >&2
  if [ -d "$GOOSE_SRC" ]; then echo "build it:  cd $GOOSE_SRC && cargo build -p goose-cli" >&2
  else echo "no checkout at $GOOSE_SRC either — clone it or set GOOSE_SRC/GOOSE_BIN" >&2; fi
  fail=1
fi
[ -f "$CLAUDE_FIXTURE" ] || { echo "no Claude transcript to import at $CLAUDE_FIXTURE (set CV_REPO)" >&2; fail=1; }
# A stub on an occupied port answers with someone else's server, and goose's turn silently records
# whatever that is. Refuse instead.
if curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$PORT/" 2>/dev/null; then
  echo "something is already listening on 127.0.0.1:$PORT — set PORT to a free one" >&2
  fail=1
fi
[ "$fail" = 0 ] || exit 1
mkdir -p "$(dirname "$OUT")"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-goose-fixture.XXXXXX")"
cleanup() { [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null; [ "$KEEP" = 1 ] || rm -rf "$WORK"; return 0; }
trap cleanup EXIT
echo "work dir: $WORK"

python3 "$HERE/stub_openai.py" "$PORT" >"$WORK/stub.log" 2>&1 &
STUB_PID=$!
ready=0
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 && { ready=1; break; }
  sleep 0.1
done
# Without this the run continued against nothing and produced a fixture with no completed turn —
# the one thing the fixture exists to carry.
[ "$ready" = 1 ] || { echo "stub_openai.py never answered on 127.0.0.1:$PORT after 5s:" >&2; sed 's/^/    /' "$WORK/stub.log" >&2; exit 1; }

# Isolated home + stub provider. GOOSE_PATH_ROOT relocates config/state/data wholesale, so the real
# ~/.config/goose and ~/.local/share/goose are never read or written.
export GOOSE_PATH_ROOT="$WORK/goose-home"
export GOOSE_DISABLE_KEYRING=1
export GOOSE_PROVIDER=openai
export GOOSE_MODEL=stub-model
export OPENAI_HOST="http://127.0.0.1:$PORT"
export OPENAI_API_KEY=sk-stub-not-a-real-key
mkdir -p "$GOOSE_PATH_ROOT"

run() { echo "--- goose $*"; "$GOOSE_BIN" "$@" </dev/null || echo "    (exit $?; expected for the failure cases)"; }

# The --name values are the fixture's session titles and `harness/goose.rs` asserts on them
# (`title_prefers_the_written_name_column`, and `by_id(…).title == Some("stubturn")`). Renaming one
# here silently breaks that test on the next regeneration.
# 1. a normal completed turn: messages.metadata_json gets usage + model, the session gets a name.
run run -t "Say pong." --name stubturn

# 2. a turn whose provider is unreachable. Goose prints the network error but writes NO assistant
# row — `harness/goose.rs` asserts exactly that ("recorded no assistant row at all").
# In a subshell — a `VAR=x func` prefix in bash persists after the function returns, which would
# quietly point the rest of the run at the dead port.
( export OPENAI_HOST="http://127.0.0.1:1"; run run -t "Say pong." --name deadturn )

# 3. a recipe whose success check never passes: retry exhaustion, persisted as plain text.
run run --recipe "$HERE/retry-fail.yaml" --name retryfail

# 4. goose's own importer reading a Claude Code transcript (thinking, Edit call/result, image).
run session import "$CLAUDE_FIXTURE"

DB="$GOOSE_PATH_ROOT/data/sessions/sessions.db"
[ -f "$DB" ] || { echo "goose wrote no store at $DB — see $WORK/stub.log" >&2; exit 1; }
# VACUUM INTO checkpoints the WAL and produces one file with no -wal/-shm siblings.
sqlite3 "$DB" "VACUUM INTO '$WORK/fixture.db'"
n_sessions=$(sqlite3 "$WORK/fixture.db" "SELECT COUNT(*) FROM sessions;")
n_messages=$(sqlite3 "$WORK/fixture.db" "SELECT COUNT(*) FROM messages;")
echo "schema $(sqlite3 "$WORK/fixture.db" "SELECT MAX(version) FROM schema_version;"), sessions $n_sessions, messages $n_messages"
# The 2026-09-19 fixture: 4 sessions, 13 messages. Fewer means a turn did not land — a stub that
# answered wrong, a goose that rejected the recipe — and the fixture is worth less than it looks.
if [ "$n_sessions" != 4 ]; then
  echo "expected 4 sessions, got $n_sessions — one of the four runs above did not persist" >&2
  sqlite3 "$WORK/fixture.db" "SELECT id, name FROM sessions;" >&2
  exit 1
fi
[ "$n_messages" = 13 ] || echo "note: $n_messages messages (the 2026-09-19 fixture had 13)" >&2

mv "$WORK/fixture.db" "$OUT"
echo "wrote $OUT"
sqlite3 "$OUT" "SELECT '  ' || id || '  ' || name FROM sessions ORDER BY id;"
echo "NOTE: goose derives session ids from today's UTC date (session_manager.rs, \`%Y%m%d\`) and"
echo "      \`goose session import\` takes no id, so every regeneration renumbers them. The"
echo "      \`by_id(\"20260919_…\")\` assertions in crates/cv-core/src/harness/goose.rs must be"
echo "      updated to the ids above."
echo "then: cargo nextest run -p clustervision-core -E 'test(/goose/)'"
