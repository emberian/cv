#!/usr/bin/env bash
# codex-resume-smoke.sh — does the real Codex CLI accept a rollout cv wrote?
#
# This is the check that made `cv port --harness codex` believable. `emit_verified` only proves cv
# can re-read its own output; this proves CODEX can. It ports a session to a rollout, drops it in a
# throwaway CODEX_HOME, and runs
#
#     codex exec --skip-git-repo-check resume <id> "<prompt>"
#
# against a provider whose base URL is a dead port. A PASS is Codex printing the session header
# (`session id: <id>`, model, provider), replaying the prompt, and then failing at the MODEL CALL —
# because everything before the model call is exactly the decoding we are testing: the file name,
# `session_meta`, `turn_context`, and every message line. A rollout Codex cannot decode fails
# earlier and differently ("failed to parse", "no such thread", an empty header).
#
# Verified this way on 2026-09-19 with Codex 0.155.1: header printed with the id, the prompt
# replayed, then `Reconnecting... waiting for network` against the dead provider.
#
#   tools/harness-fixtures/codex/codex-resume-smoke.sh [<session-id-or-prefix>] [-- <cv port args>]
#
# With no argument it ports the most recent Claude session. Env: CV (default: `cargo run -q -p
# clustervision --`; set CV=cv to use an installed build), CODEX_BIN (default `codex`),
# TIMEOUT (default 90, seconds to wait for Codex to reach the model call).

set -euo pipefail

SPEC=""
PORT_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; PORT_ARGS=("$@"); break ;;
    -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) echo "unknown flag: $1 (extra \`cv port\` args go after \`--\`)" >&2; exit 2 ;;
    *) [ -z "$SPEC" ] || { echo "at most one session id" >&2; exit 2; }; SPEC="$1" ;;
  esac
  shift
done

CV="${CV:-cargo run -q -p clustervision --}"
CODEX_BIN="${CODEX_BIN:-codex}"
TIMEOUT="${TIMEOUT:-90}"

command -v "$CODEX_BIN" >/dev/null || { echo "no codex on PATH (set CODEX_BIN)" >&2; exit 1; }
command -v python3 >/dev/null || { echo "no python3 on PATH (used to read one id out of cv ls --json)" >&2; exit 1; }
echo "codex: $("$CODEX_BIN" --version 2>&1 | head -1)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cv-codex-smoke.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# A throwaway home. Codex writes rollouts, sqlite projections and locks under here and nowhere else,
# so your real ~/.codex is neither read nor written.
export CODEX_HOME="$WORK/codex-home"
mkdir -p "$CODEX_HOME/sessions"

# Codex 0.155.1 does NOT read $OPENAI_BASE_URL for the model call — it is only consulted by the
# network-proxy credential broker (codex-rs/network-proxy/src/credential_broker/providers/
# openai.rs). Setting it and trusting it is how an earlier version of this script sent a real
# request to wss://api.openai.com with a junk key and got a 401 back. The provider's base URL lives
# in config, so override config: a provider whose base_url is a closed port, selected by
# `model_provider`. Nothing can leave the machine.
CFG_PROVIDER='model_providers.cvdead={name="cv dead endpoint",base_url="http://127.0.0.1:1/v1",wire_api="responses",env_key="CV_SMOKE_FAKE_KEY",request_max_retries=0,stream_max_retries=0}'
export CV_SMOKE_FAKE_KEY="sk-not-a-real-key"
# Belt and braces for anything that still reads the ambient OpenAI vars.
export OPENAI_BASE_URL="http://127.0.0.1:1/v1"
export OPENAI_API_KEY="sk-not-a-real-key"

if [ -z "$SPEC" ]; then
  SPEC="$($CV ls --harness claude --limit 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')"
  echo "no session given; using the most recent Claude session $SPEC"
fi

# No --out: cv resolves Codex's storage root as Codex does ($CODEX_HOME/sessions), which is the
# throwaway one above — so this exercises the $CODEX_HOME support as well as the emitter.
echo "--- cv port $SPEC --harness codex   (into \$CODEX_HOME/sessions)"
$CV port "$SPEC" --harness codex ${PORT_ARGS[@]+"${PORT_ARGS[@]}"}
n_rollouts=$(find "$CODEX_HOME/sessions" -name 'rollout-*.jsonl' | wc -l | tr -d ' ')
[ "$n_rollouts" = 1 ] || { echo "expected 1 rollout under \$CODEX_HOME/sessions, found $n_rollouts" >&2; exit 1; }
ROLLOUT="$(find "$CODEX_HOME/sessions" -name 'rollout-*.jsonl')"
echo "rollout: $ROLLOUT ($(wc -l <"$ROLLOUT" | tr -d ' ') lines)"

# Codex names a rollout rollout-<19-char-stamp>-<uuid>.jsonl; the uuid is the thread id it resumes
# by, and it is the last 36 characters of the stem.
STEM="$(basename "$ROLLOUT" .jsonl)"
ID="${STEM: -36}"
case "$ID" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]-*-*-*-*) ;;
  *) echo "cannot read a thread uuid out of $STEM" >&2; exit 1 ;;
esac

# A prompt is required. `codex exec resume <id>` with no PROMPT reads one from stdin, and with
# stdin closed prints "No prompt provided via stdin." and exits 1 — before the model call, so the
# decode is never exercised and the check silently proves nothing.
echo "--- codex exec --skip-git-repo-check resume $ID \"<prompt>\"  (provider: cvdead, ${TIMEOUT}s cap)"
"$CODEX_BIN" exec --skip-git-repo-check -c "$CFG_PROVIDER" -c model_provider=cvdead \
  resume "$ID" "reply with the single word pong" </dev/null >"$WORK/out.txt" 2>&1 &
CODEX_PID=$!

# Codex retries a dead provider on a widening backoff and never gives up, so waiting for it to exit
# would hang. Watch for the two things a PASS needs and stop as soon as both are there.
MODEL_CALL='Reconnecting|connection refused|error sending request|stream error|401|unauthorized|failed to connect'
pass=0
for _ in $(seq 1 "$((TIMEOUT * 2))"); do
  kill -0 "$CODEX_PID" 2>/dev/null || break
  if grep -qF "session id: $ID" "$WORK/out.txt" 2>/dev/null && grep -qiE "$MODEL_CALL" "$WORK/out.txt" 2>/dev/null; then
    pass=1; break
  fi
  sleep 0.5
done
kill "$CODEX_PID" 2>/dev/null || true
wait "$CODEX_PID" 2>/dev/null || true
head -30 "$WORK/out.txt" | sed 's/^/    /'

if [ "$pass" = 1 ]; then
  echo "PASS — Codex decoded the rollout (header carries $ID) and got as far as the model call"
else
  echo "FAIL — Codex did not get to the model call; it did not accept the rollout" >&2
  echo "       (wanted \"session id: $ID\" in the header AND a model-call failure)" >&2
  exit 1
fi
