---
name: cv
description: Read agent transcripts, recover compacted work, inspect shared edits and coordinate tasks across harnesses.
---

# Working across sessions

On takeover, read `cv lanes <session>` and `cv task inbox`. Use
`cv task events --since <id>` for changes since your last read.
Hookless sessions use these same commands.

Before editing shared code, inspect the cited path or hunk and its current contents.
Use `cv touched <path> --edits-only` to find other writers and `cv show <id> --last 10`
to see what they did. Coordinate before overriding an edit conflict.

When another lane needs a finding, publish a scoped fact or failure note on its task.
Cite the session, file revision or validation run. Use `cv cat <session> <tool_use_id>`
for exact elided output and add `--input` for the arguments. Read predecessor
explanations and child reports as attributed evidence.

After compaction, inspect the boundary and later human prompts. Use
`cv show <id> --kind compaction_boundary,compaction_summary` to locate the cut,
then `cv show <id> --pre-compaction N` to recover the discarded span.
Use `cv rewind <id> --at <msg>` for a supported native rewind or
`cv distill <id> --session` for an extractive handoff.

On escalation, carry the specification, workspace changes, facts available only
outside the transcript, and exact validation. On downshift, also carry the plan
and relevant tool tail. At close, report what ran, failed or was skipped and any
remaining assumption. Record the report with `cv task done`.

## Hook pointers

`cv hooks install claude` or `cv hooks install codex` installs the hooks and this skill.
Codex requires hook trust. Inspect local settings with `cv hooks show <harness>`.
With `cv watch` or `cv serve` running, observational hooks can enqueue lane events
and collect a pointer prepared for the next start or prompt. Follow it to its source.
A clean start is silent. `cv doctor <session> --json` reports observed uptake.

A spawner can pass `CV_BAGGAGE=root_task_id,parent_session_id,spawn_tool_use_id,branch_id`.
Use four ASCII identifiers starting with a letter or digit, containing only letters,
digits, dots, underscores or hyphens. The entire value fits in 128 bytes.
