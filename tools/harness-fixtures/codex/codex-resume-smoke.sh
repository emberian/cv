#!/usr/bin/env bash
# codex-resume-smoke.sh — does the real Codex CLI accept a rollout cv wrote?
#
# This is the check that made `cv port --harness codex` believable. `emit_verified` only proves cv
# can re-read its own output; this proves CODEX can. It ports a session to a rollout, drops it in a
# throwaway CODEX_HOME, and runs
#
#     codex exec --skip-git-repo-check resume <id>
#
# with OPENAI_BASE_URL pointed at a dead port. A PASS is Codex printing the session header (id,
# model, provider), replaying the prompt, and then failing at the MODEL CALL — because everything
# before the model call is exactly the decoding we are testing: the file name, `session_meta`,
# `turn_context`, and every message line. A rollout Codex cannot decode fails earlier and
# differently ("failed to parse", "no such thread", an empty header).
#
# Verified this way on 2026-09-19 with Codex 0.155.1: header printed, prompt replayed, 401 at the
# model call with a fake key, and the thread row got `model_provider: openai`,
# `history_mode: legacy`.
#
#   tools/harness-fixtures/codex/codex-resume-smoke.sh [<session-id-or-prefix>] [--harness <h>]
#
# With no argument it ports the most recent Claude session. Env: CV (default: `cargo run -q -p
# clustervision --`), CODEX_BIN (default `codex`).

set -euo pipefail

SPEC="${1:-}"
[ "${SPEC:-}" = "--harness" ] && SPEC=""
shift || true
CV="${CV:-cargo run -q -p clustervision --}"
CODEX_BIN="${CODEX_BIN:-codex}"

command -v "$CODEX_BIN" >/dev/null || { echo "no codex on PATH (set CODEX_BIN)" >&2; exit 1; }
echo "codex: $("$CODEX_BIN" --version 2>&1 | head -1)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-codex-smoke.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# A throwaway home. Codex writes rollouts, sqlite projections and locks under here and nowhere else,
# so your real ~/.codex is neither read nor written.
export CODEX_HOME="$WORK/codex-home"
mkdir -p "$CODEX_HOME/sessions"
# A base URL nothing is listening on, and a key that is not one: the run MUST die at the model call
# and must not be able to reach a real provider even if a config leaks through.
export OPENAI_BASE_URL="http://127.0.0.1:1/v1"
export OPENAI_API_KEY="sk-not-a-real-key"

if [ -z "$SPEC" ]; then
  SPEC="$($CV ls --harness claude --limit 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')"
  echo "no session given; using the most recent Claude session $SPEC"
fi

# No --out: cv resolves Codex's storage root as Codex does ($CODEX_HOME/sessions), which is the
# throwaway one above — so this exercises the $CODEX_HOME support as well as the emitter.
echo "--- cv port $SPEC --harness codex   (into \$CODEX_HOME/sessions)"
$CV port "$SPEC" --harness codex "$@"
ROLLOUT="$(find "$CODEX_HOME/sessions" -name 'rollout-*.jsonl' | head -1)"
[ -n "$ROLLOUT" ] || { echo "cv port wrote no rollout under $CODEX_HOME/sessions" >&2; exit 1; }
echo "rollout: $ROLLOUT ($(wc -l <"$ROLLOUT" | tr -d ' ') lines)"

# Codex names a rollout rollout-<ISO8601>-<uuid>.jsonl; the uuid is the thread id it resumes by.
ID="$(basename "$ROLLOUT" .jsonl | sed 's/^rollout-[0-9T:-]*-//')"
echo "--- codex exec --skip-git-repo-check resume $ID"
set +e
"$CODEX_BIN" exec --skip-git-repo-check resume "$ID" </dev/null >"$WORK/out.txt" 2>&1
rc=$?
set -e
sed 's/^/    /' "$WORK/out.txt"

if grep -qiE 'connection refused|401|unauthorized|error sending request|stream error' "$WORK/out.txt" &&
   grep -qF "$ID" "$WORK/out.txt"; then
  echo "PASS — Codex decoded the rollout and got as far as the model call (exit $rc)"
else
  echo "FAIL — Codex did not get to the model call; it did not accept the rollout (exit $rc)" >&2
  exit 1
fi
