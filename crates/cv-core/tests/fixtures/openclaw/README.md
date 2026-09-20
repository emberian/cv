# OpenClaw fixtures

`openclaw-agent.sqlite` was written by OpenClaw's own store code, not by cv. Its generator moved to

    tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts

with prerequisites, the one command and what it produces in the README next to it. Every
real-writer fixture generator lives under `tools/harness-fixtures/<harness>/` now.

The `.jsonl` files here are legacy v3 transcripts kept as-is (hand-checked samples, not generated).
