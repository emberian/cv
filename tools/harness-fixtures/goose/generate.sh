#!/usr/bin/env bash
# Regenerate tests/fixtures/goose/sessions-v16-1.51.0.db — written by goose itself, not by cv.
#
# Starts the stub OpenAI server next to this script, points a freshly built goose at a throwaway
# GOOSE_PATH_ROOT, and drives four sessions that between them exercise everything cv's goose adapter
# reads: a completed turn (usage + model in metadata_json), a turn against a dead endpoint, a recipe
# whose success check can never pass (retry exhaustion), and an import of cv's own Claude fixture
# (thinking, a tool call/result pair, an image). Then it VACUUMs the store into the fixture path.
#
# No model call, no network, and nothing outside $WORK and the fixture file is touched.
#
#   tools/harness-fixtures/goose/generate.sh [--keep]
#
# Env: GOOSE_SRC (default ~/pug/goose), GOOSE_BIN (default $GOOSE_SRC/target/debug/goose),
#      PORT (default 18080), CV_REPO (default: the repo this script lives in).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CV_REPO="${CV_REPO:-$(cd "$HERE/../../.." && pwd)}"
GOOSE_SRC="${GOOSE_SRC:-$HOME/pug/goose}"
GOOSE_BIN="${GOOSE_BIN:-$GOOSE_SRC/target/debug/goose}"
PORT="${PORT:-18080}"
OUT="$CV_REPO/crates/cv-core/tests/fixtures/goose/sessions-v16-1.51.0.db"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

if [ ! -x "$GOOSE_BIN" ]; then
  echo "no goose binary at $GOOSE_BIN" >&2
  echo "build it:  cd $GOOSE_SRC && cargo build -p goose-cli" >&2
  exit 1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-goose-fixture.XXXXXX")"
cleanup() { [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null; [ "$KEEP" = 1 ] || rm -rf "$WORK"; return 0; }
trap cleanup EXIT
echo "work dir: $WORK"

python3 "$HERE/stub_openai.py" "$PORT" >"$WORK/stub.log" 2>&1 &
STUB_PID=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 && break
  sleep 0.1
done

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

# 1. a normal completed turn: metadata_json gets usage + model, the session gets a name.
run run -t "Say pong." --name normal-turn

# 2. a turn whose provider is unreachable: goose persists the failure as a message, not a hole.
# In a subshell — a `VAR=x func` prefix in bash persists after the function returns, which would
# quietly point the rest of the run at the dead port.
( export OPENAI_HOST="http://127.0.0.1:1"; run run -t "Say pong." --name dead-endpoint )

# 3. a recipe whose success check never passes: retry exhaustion, persisted as plain text.
run run --recipe "$HERE/retry-fail.yaml" --name retry-fail

# 4. goose's own importer reading a Claude Code transcript (thinking, Edit call/result, image).
run session import "$CV_REPO/crates/cv-core/tests/fixtures/claude/rich_blocks.jsonl"

DB="$GOOSE_PATH_ROOT/data/sessions/sessions.db"
[ -f "$DB" ] || { echo "goose wrote no store at $DB — see $WORK/stub.log" >&2; exit 1; }
# VACUUM INTO checkpoints the WAL and produces one file with no -wal/-shm siblings.
sqlite3 "$DB" "VACUUM INTO '$WORK/fixture.db'"
sqlite3 "$WORK/fixture.db" "SELECT 'schema ' || MAX(version) FROM schema_version;
  SELECT 'sessions ' || COUNT(*) FROM sessions; SELECT 'messages ' || COUNT(*) FROM messages;"
mv "$WORK/fixture.db" "$OUT"
echo "wrote $OUT"
echo "now:  cargo nextest run -p clustervision-core -E 'test(/goose/)'"
