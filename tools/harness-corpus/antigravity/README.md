# Antigravity real-writer corpus

`generate.py` drives the installed **1.5.9** CLI against a deterministic local
Gemini SSE endpoint. The mock supplies model replies and tool calls; Antigravity
executes the tools and writes its own SQLite records. It uses no account or model
credentials and makes no paid calls.

```sh
python3 -I tools/harness-corpus/antigravity/generate.py \
  --bin /absolute/path/to/agy \
  --out crates/cv-core/tests/corpus/antigravity/NEW-VERSION
```

The output must not exist. A new fixture records the executable version and
SHA-256, local mock-call count, selection, and scrub procedure. Generate its
`summary.json` with the existing corpus golden test; do not overwrite a committed
version.

The scenario reads a generated one-pixel PNG and launches a one-second command
that backgrounds after 500 ms. The writer must record the asynchronous completion
message and image media before capture succeeds. The fixture deliberately omits
exported JSONL so the reader exercises native SQLite fallback.

Every invocation uses a fresh HOME, app-data, XDG and temporary directory. A macOS
`sandbox-exec` profile denies writes outside that directory (except PTY devices)
and denies non-loopback network, including descendants. There is no unsandboxed
fallback. The CLI's auto-update is disabled, its environment contains no inherited
credentials, and a timeout stops only the owned process group.

The capture uses SQLite's backup API. It retains actual `steps` and
`trajectory_meta` rows, empties unrelated request metadata tables, vacuums to
4 KiB SQLite pages, and applies
only equal-width scratch-path substitutions to native byte cells. Padding the
placeholder path preserves nested protobuf length prefixes. It refuses residual
real paths or dummy credentials. Do **not** run the generic byte-replacing
`scrub.py` over native protobuf: variable-length substitutions corrupt its wire
format. The corpus privacy check and native parse must both pass before committing.

This covers native-only reading, one background task's start/completion, and an
image tool result. It does not cover permission interactions, resume/compaction,
inline prompt media, multiple tasks, or real/nested swarms. The OS sandbox and
local gateway behavior are verified for macOS CLI 1.5.9 only.
