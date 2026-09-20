# Codex resume smoke test

Not a fixture generator — a **proof**. `emit_verified` re-reads cv's output with cv's own adapter,
which proves cv agrees with cv. This proves the real Codex CLI accepts a rollout cv wrote.

## Prerequisites

- `codex` on `PATH` (set `CODEX_BIN` for a specific build). Verified on 2026-09-19 with **0.155.1**.
- At least one session in your local store to port (any harness; the default picks the most recent
  Claude one).
- `python3`, for reading one id out of `cv ls --json`.

## The one command

```sh
tools/harness-fixtures/codex/codex-resume-smoke.sh              # most recent Claude session
tools/harness-fixtures/codex/codex-resume-smoke.sh 7f3a         # a specific id or prefix
```

## What it produces

No file that survives the run — it prints PASS or FAIL and exits non-zero on FAIL.

It ports the session into a throwaway `CODEX_HOME` (cv resolves Codex's root as Codex does:
`$CODEX_HOME/sessions`, so `--out` is not needed and the `$CODEX_HOME` support gets exercised too),
then runs:

```sh
codex exec --skip-git-repo-check resume <thread-id>
```

with `OPENAI_BASE_URL=http://127.0.0.1:1/v1` and a fake key.

**A PASS is Codex failing at the model call**, and nothing earlier. Reaching the model call means
Codex's decoder accepted the file *name* (`rollout-<19-char-stamp>-<uuid>.jsonl` — Codex's
`RolloutFileName::parse` demands byte 19 be `-`, and a `…Z-<uuid>` name is invisible to every
filename-based lookup), the `session_meta` line (`cwd` is required, no default — without it resume
dies with "failed to parse thread ID from rollout file"), the `turn_context` line, and every message
line. A rollout Codex cannot decode fails earlier and differently: a parse error, "no such thread",
or a header with no id.

On 2026-09-19 the header printed with id, model and provider, the prompt replayed, the run 401'd at
the model call, and Codex's own thread row came back with `model_provider: openai`,
`history_mode: legacy`.

## Safety

Everything is under a `mktemp -d` `CODEX_HOME` that is removed on exit; your real `~/.codex` is
neither read nor written. `OPENAI_BASE_URL` points at a closed port and the key is not a key, so the
run cannot reach a provider even if a config file leaks through.
