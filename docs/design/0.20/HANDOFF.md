# Trustworthy handoffs for 0.20

Owner: handoff lane. Scope: distill output, completion evidence, and emitted Claude parent chains.

## Decisions and implementation

- Budget task `01a114d0-35f5-7871-9196-9b12085bedb8`: exact `--max-bytes` and honestly named `--max-estimated-tokens`, checked on the aggregate after `--with`, before publication or retirement. Refuse rather than silently truncate. The estimate excludes harness overhead and resume preamble; it is not tokenizer-backed.
- Supersession task `01a114d0-3614-7990-9951-876d176ddbdd`: branch tips and repository heads already select the newest observation per key in `render_handoff` and `render_facts`, and explicitly say “last seen.” Commit/error/gate lists are historical and retain message indices. Do not relabel old commits as current state.
- Background task `01a114d0-3634-7ce2-8f88-458ac05e1bcb`: suppress verdict mining for unfinished results and native asynchronous launch acknowledgements. Claude notifications carry `tool-use-id`; Antigravity launch acknowledgement carries `<session>/task-N`, matched to `completed_task` plus the native “finished with result” envelope or a terminal `manage_task` status response. Mine completion against the initiating command. Codex native cell/PTY headers and structured exec_command envelopes retain job ownership across waits and stdin polls; a finished cell does not settle its still-running nested PTY. Queued Claude notifications count before delivery and duplicate delivery does not duplicate facts.
- In-flight task `01a114d0-3652-7813-a96c-e6fcc5c02bd0`: default resumable distill refuses uncertain source calls or a growing capture before publication. Explicit failure/cancellation settles a call. `--allow-unsettled` is an intentional snapshot with unresolved call IDs carried in its context; plain packs carry the same disclosure.
- Parent task `01a10891-5134-7191-a4c0-eca1bbe18da4`: whole-chain emission and missing-parent validation already exist, including main-thread/sub-agent reshape coverage. Added linear parent-cycle detection across all components, including detached branches. Derived rewrites retain the existing policy permitting defects already in their source.

## Still open

- General background lifecycle correlation for Claude polling-only histories without notifications, and Antigravity kill acknowledgements that lack explicit task/status evidence. Do not infer terminal status from a successful poll call alone. Native Antigravity CLEARED records may have lost arguments/results/job IDs; preserve uncertainty. Codex compound polls with ambiguous ownership and bounded JSON envelopes that hide identity after long output also remain uncertain.
- Semantic supersession of natural-language decisions/open items is not solved by newest-per-key git facts. A structured decision identity/replacement relation would be more reliable than guessing that later prose retracts earlier prose.
- Tokenizer-backed budgets, accounting for target harness system/tool overhead, need an explicit model/tokenizer contract. The new byte ceiling is exact only for the Markdown pack, not emitted JSONL or sidecars.
- `--with` findings are historical context from other lanes; the primary source's unsettled guard is not a fleet-wide lock or liveness check. The command neither cancels jobs nor prevents a source from resuming after capture.

Regression coverage belongs in core distill tests, CLI distill publication tests, and Claude emit verifier tests. Fixtures contain synthetic protocol wrappers only; private transcripts are not committed.
