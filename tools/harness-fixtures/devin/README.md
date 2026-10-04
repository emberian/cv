# Devin CLI fixture — hand-written, redacted

Produces `crates/cv-core/tests/fixtures/devin/sessions-v17-3000.11.3.db`. Unlike the goose fixture
there is no real writer available: the Devin CLI is closed source, so `generate.py` (python3 +
sqlite3 stdlib, no deps) creates the schema **verbatim as the live `sessions.db` spells it**
(refinery schema 17, all 17 migration rows in `refinery_schema_history`, every table including the
ignored `prompt_history` / `rendered_commits` / `app_state` / `tool_call_state` / `subagent_heads`)
and fills it with a synthetic two-session dataset.

## The one command

```sh
tools/harness-fixtures/devin/generate.py            # or: python3 tools/harness-fixtures/devin/generate.py
```

`--out PATH` writes elsewhere (e.g. to diff a regeneration against the committed fixture).

## What it produces

| session | why it is there |
|---|---|
| `fixture-alpha` | the full shape: an abandoned 3-node pre-prefix chain (nodes 0-2), the live chain rooted at node 3 — a 7-node `is_system_prefix` run, `<system_info>` + `<rules>` copies carrying `compact/prior_node_ids`, a typed user prompt, an assistant turn with `thinking` + text + two parallel `exec` `tool_calls`, one `success:true` tool result (with `chisel/terminal_output`) and one `success:false`, then a `finish_reason:"stop"` reply. `main_chain_id` points at the tail so the chain walk drops the superseded copies. Plus a `sidekick` sub-agent chain (nodes 17-24, a second root: own prefix, `agent-ext/rules-loaded`, `agent-ext/skills-loaded`, a `subagent/handoff` handoff prompt with `chisel/fusion_lead_model_uid`, one exec turn) referenced by `subagent_heads`, and a `ghost` head pointing at a nonexistent node (dangling → skipped + counted). |
| `fixture-beta` | `model = ''` (session model falls back to the last assistant's `metadata.generation_model`), `main_chain_id = NULL` (head falls back to max `node_id`, flagged `main_chain_fallback`), `hidden = 1`, and one corrupt `chat_message` mid-chain (skipped and counted as `cv.skipped_lines`). |

All texts are placeholders ("fixture", "You are Devin (fixture)"); nothing from a real
transcript is embedded.
