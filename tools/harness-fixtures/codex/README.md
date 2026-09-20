# Codex resume smoke test

Not a fixture generator — a **proof**. `emit_verified` re-reads cv's output with cv's own adapter,
which proves cv agrees with cv. This proves the real Codex CLI accepts a rollout cv wrote.

## Prerequisites

- `codex` on `PATH` (set `CODEX_BIN` for a specific build). Verified on 2026-09-19 with **0.155.1**.
- At least one session in your local store to port (any harness; the default picks the most recent
  Claude one).
- `python3`, for reading one id out of `cv ls --json`.
- A `cv`. `CV` defaults to `cargo run -q -p clustervision --`; set `CV=cv` to use an installed
  build and skip the compile.

## The one command

```sh
tools/harness-fixtures/codex/codex-resume-smoke.sh              # most recent Claude session
tools/harness-fixtures/codex/codex-resume-smoke.sh 7f3a         # a specific id or prefix
tools/harness-fixtures/codex/codex-resume-smoke.sh 7f3a -- --limit 50   # extra `cv port` args
```

## What it produces

No file that survives the run — it prints PASS or FAIL and exits non-zero on FAIL.

It ports the session into a throwaway `CODEX_HOME` (cv resolves Codex's root as Codex does:
`$CODEX_HOME/sessions`, so `--out` is not needed and the `$CODEX_HOME` support gets exercised too),
then runs:

```sh
codex exec --skip-git-repo-check -c 'model_providers.cvdead={…base_url="http://127.0.0.1:1/v1"…}' \
  -c model_provider=cvdead resume <thread-id> "reply with the single word pong"
```

**A PASS is Codex failing at the model call**, and nothing earlier. Reaching the model call means
Codex's decoder accepted the file *name* (`rollout-<19-char-stamp>-<uuid>.jsonl` — Codex's
`RolloutFileName::parse` demands byte 19 be `-`, and a `…Z-<uuid>` name is invisible to every
filename-based lookup), the `session_meta` line (`cwd` is required, no default — without it resume
dies with "failed to parse thread ID from rollout file"), the `turn_context` line, and every message
line. A rollout Codex cannot decode fails earlier and differently: a parse error, "no such thread",
or a header with no id.

The check is: the header carries `session id: <the uuid cv chose>` **and** a model-call failure
appears. On 2026-09-19 the header printed with that id, Codex read the recorded model
(`claude-fable-5-1`) back out of the rollout, the prompt replayed, and the run went into
`Reconnecting... waiting for network` against the dead provider.

## Two things that make this easy to get wrong

Both were live bugs in this script, found by running it:

- **The prompt is not optional.** `codex exec resume <id>` with no `PROMPT` argument reads one from
  stdin; with stdin closed it prints `No prompt provided via stdin.` and exits 1 — *before* the
  model call, so nothing about the rollout is ever decoded and the script reports FAIL on a
  perfectly good file.
- **`OPENAI_BASE_URL` does not redirect Codex 0.155.1.** It is read only by the network-proxy
  credential broker (`codex-rs/network-proxy/src/credential_broker/providers/openai.rs`); the model
  client takes its base URL from config. Pointing it at a dead port and calling that containment
  sent a real request to `wss://api.openai.com/v1/responses` with a junk key and collected a 401.
  The redirect has to be a config override: a `model_providers.<name>` whose `base_url` is the dead
  port, selected with `-c model_provider=<name>`.

Codex also retries a dead provider on a widening backoff and never exits, so the script watches its
output and stops it as soon as the PASS markers appear (`TIMEOUT`, default 90s, caps the wait).

## Safety

Everything is under a `mktemp -d` `CODEX_HOME` that is removed on exit; your real `~/.codex` is
neither read nor written. The selected provider's base URL is a closed port and its key is not a
key, so the run cannot reach a provider.
