# 🧬 OpenSession — an interchange format for agent sessions

*Status: draft 0.3 · a clustervision proposal · feedback very welcome*

Every coding-agent harness records its conversations, and **every single one invented its own format**:
Claude threads by UUID, Codex emits an event stream (twice — once for the UI, once for the model),
Grok splits a session across four files in a percent-encoded directory, OpenCode stores one JSON file
per message *and* per content part, Gemini/Antigravity uses opaque protobuf, Hermes uses SQLite, OpenClaw
uses yet another JSONL dialect. We've now parsed all of them (see [`FORMATS.md`](FORMATS.md)) and the
striking thing is **how similar they actually are underneath**. They all encode the same handful of ideas.

OpenSession writes those ideas down. It is the format the harnesses would have agreed on if they'd talked
first. clustervision's in-memory IR is the reference implementation, and `cv export --format json` emits it
verbatim — that is a claim you can check, and 0.3 exists because for one release it was false. The spec had
drifted into camelCase with `kind`-tagged blocks while the implementation moved to snake_case and `type`,
so the two described different formats while asserting they were one. The spec follows the implementation
now, because an interchange format nobody can round-trip is a wish, not a standard.

## Design principles (the lessons, earned the hard way)

1. **A session is an ordered list of messages, plus provenance.** That's the whole spine. Don't overthink it.
2. **The working directory is just metadata, never identity.** The original sin of every harness is coupling
   a session to its cwd (in the *filename*, no less) so you can only resume it from there. OpenSession records
   `cwd` as a plain field you can change freely. Portability is the default.
3. **Content is a list of typed blocks**, because a single assistant turn legitimately contains reasoning,
   prose, and several tool calls. A flat string can't represent that without lying.
4. **Four roles, normalized:** `system`, `user`, `assistant`, `tool`. Several harnesses smuggle tool results
   into a `user` turn — OpenSession promotes them to `tool` so conversions don't misattribute them.
5. **Who spoke, what the turn *is*, and where it came from are three questions.** The role answers the first.
   0.3 adds `kind` and `origin` for the others, because collapsing them loses real information: a system
   reminder the harness injected, a notice it printed for the human, and a compaction boundary are all
   `role: "system"` and are not remotely the same thing.
6. **Reasoning is first-class but may be opaque.** Codex and Grok ship *encrypted* reasoning blobs, and most
   Anthropic reasoning is a signature with no plaintext at all. Model them honestly: a `thinking` block
   carries plaintext when available and an opaque `signature`/`encrypted` payload when not. Never pretend
   opaque data is readable — and never pretend a turn survived a conversion when only its shell did.
7. **Tool calls and their results are linked by id**, not by adjacency — interleaving and parallel calls are real.
8. **Lossy-but-honest beats lossless-but-brittle.** Keep an `extra` bag for harness-specific fields so
   round-trips preserve what they can, but the core stays small enough that *every* harness can fill it.
9. **`extra` is namespaced, never flat.** Every key under `extra` is the name of the harness whose fact it is
   (or `cv`, for the reader's own bookkeeping). A flat bag turns into a namespace collision the moment two
   harnesses use the same word, and it makes "whose field is this?" unanswerable.
10. **Be tolerant on the way in.** Real transcripts have corrupt lines, missing fields, and multiple historical
    format versions. A parser that rejects a session because one line is malformed is worse than useless.

## The schema (v0.3)

```jsonc
{
  "open_session": "0.3",           // format version
  "harness": "claude",             // origin: claude|codex|grok|opencode|gemini|hermes|openclaw|...
  "id": "da9174f4-…",              // session id (native if possible)
  "cwd": "/Users/ember/pug/x",     // METADATA, not identity — freely rewritable
  "title": "…",                    // human/AI label (optional)
  "model": "claude-opus-4-8",      // session default; a message names one only when it differs
  "created_at": "2026-05-29T21:…Z",// ISO-8601 (optional)
  "updated_at": "2026-05-29T22:…Z",
  "git": { "branch": "main", "commit": "…", "remote": "…" },   // optional
  "system_prompt": "…",            // what the harness actually sent, when it stores it (optional)
  "lineage": {                     // where this session came from and went (all optional)
    "forked_from": "…",            // forked/branched off this session
    "parent": "…",                 // the session that owns this one as a sub-agent
    "spawned_by_tool_use": "…",    // the tool call in the parent that started it
    "continued_in": "…",           // the session this one continued into
    "continues": "…",              // the inverse pointer, when the store records it
    "agent_path": "…"              // sub-agent path/nickname, when the harness has one
  },
  "source_path": "/…/da9174f4.jsonl", // where the WRITER read it from; a reader may ignore it,
                                   //   and must never treat it as identity (optional)
  "extra": { "claude": { } },      // namespaced passthrough: one bag per harness (optional)
  "messages": [
    {
      "id": "uuid",                // optional
      "parent_id": "uuid|null",    // optional threading (DAG); omit for linear
      "role": "assistant",         // WHO spoke: system|user|assistant|tool
      "kind": "reply",             // WHAT the turn is — see below
      "origin": "model",           // WHERE it came from — see below
      "timestamp": "…",            // optional
      "model": "…",                // optional; set when it differs from the session default
      "usage": { "input_tokens": 0, "output_tokens": 0,
                 "cache_read_tokens": 0, "cache_creation_tokens": 0,
                 "reasoning_tokens": 0, "cost_usd": 0.0 },   // optional
      "content": [                 // ordered, typed blocks
        { "type": "thinking", "text": "…", "signature": "…", "encrypted": "…", "redacted": false },
        { "type": "text", "text": "…" },
        { "type": "tool_use", "id": "call_1", "name": "Bash", "input": { "command": "ls" },
          "namespace": "…" },
        { "type": "tool_result", "tool_use_id": "call_1", "content": "…", "is_error": false,
          "tool_name": "Bash", "status": "completed", "details": { } },
        { "type": "file", "mime": "application/pdf", "path": "spec.pdf", "source": "file:///…" },
        { "type": "image", "media_type": "image/png", "data_ref": "…" }
      ],
      "extra": { "claude": { } }   // namespaced passthrough (optional)
    }
  ]
}
```

### Block types

| `type` | meaning | key fields |
|---|---|---|
| `text` | plain prose | `text` |
| `thinking` | reasoning / chain-of-thought | `text`, `signature?`, `encrypted?` (opaque blob), `redacted?` (provider-redacted flag) |
| `tool_use` | a tool/function invocation | `id`, `name`, `input` (arbitrary JSON), `namespace?` |
| `tool_result` | the result of one | `tool_use_id`, `content`, `is_error`, `tool_name?`, `status?`, `details?` (structured) |
| `file` | a file/dir/resource attachment (never inlined bytes) | `mime?`, `path?`, `source?` (uri/ref) |
| `image` | an image reference (never inlined bytes) | `media_type?`, `data_ref?` |

New block types are additive; consumers MUST ignore types they don't recognize (forward-compatibility).

### Message kinds

`role` says who spoke. `kind` says what the turn *is* — the vocabulary conversions key off, so that a
harness-injected reminder is never mistaken for something a human typed.

| `kind` | meaning |
|---|---|
| `prompt` | a human's typed prompt |
| `reply` | the model's reply: text, thinking, tool calls |
| `tool_result` | tool output fed back to the model |
| `injected_context` | context the HARNESS put into the model's input (system reminders, hook output, environment blocks) |
| `system_prompt` | the system prompt, where the store keeps it as a message rather than a session field |
| `notice` | something the harness showed the human and never sent to the model |
| `compaction_boundary` | the point where the harness compacted the context |
| `compaction_summary` | the summary that seeds the next window |
| `model_change` | the model or effort changed from here on; `model` holds the new one |
| `error` | an error the model never answered (API error, refusal, retry exhaustion) |
| `subagent_spawn` / `subagent_return` | a sub-agent was started here / its result arrived |
| `branch` | a rewind, reset or fork marker: what follows does not continue what precedes |
| `carrier` | a verbatim non-conversational record, carried only by format-complete readers |

Consumers MUST ignore kinds they don't recognize. A reader that only cares about the conversation can keep
`prompt`, `reply`, `tool_result`, `injected_context`, `system_prompt` and `compaction_summary` and drop the
rest; everything else is structure around the conversation rather than part of it.

### Origins

| `origin` | meaning |
|---|---|
| `human` | typed by a person |
| `model` | produced by the model |
| `harness` | produced by the harness itself |
| `hook` | a user-configured hook's output |
| `scheduler` | cron, loop wakeups, automation |
| `subagent` | another agent: inter-agent messages, sub-agent returns |
| `import` | imported from another harness by the harness |
| `unknown` | the store does not say |

## What's deliberately *not* in OpenSession

- **Tool schemas** — huge, harness-specific, and rarely portable. A harness may stash them in `extra`.
- **Project context files** (`CLAUDE.md`, `MEMORY.md`, `AGENTS.md`) — these live *next to* a session, not
  inside it. clustervision's `cv port` carries them alongside; OpenSession may grow an optional `attachments`
  array later.
- **Inlined binary content.** `image` and `file` carry references, never bytes. A transcript that embeds
  megabytes of base64 is one nobody can stream.

0.3 promoted two things that used to be on this list. **System prompts** are now `system_prompt`, because
every harness that stores one stores the same thing and leaving it in `extra` meant no conversion could
carry it. **Cost** is now `usage.cost_usd`, for the same reason.

## Versioning & historical variants

`open_session` is the format version of *this document*. The messy reality is that each *source* harness also
drifts over time (Codex alone has ≥3 on-disk shapes). A faithful importer must recognize those by **content,
not by a version field** — see [`ADDING_HARNESS.md`](../ADDING_HARNESS.md). OpenSession's job is to be the
stable target all those variants converge onto.

### 0.2 → 0.3

0.3 is a **renaming** plus additions. The shape did not change; the spellings did, to match the reference
implementation instead of diverging from it.

- The version key is `open_session`, not `openSession`.
- Every key is snake_case: `parent_id`, `created_at`, `updated_at`, `input_tokens`, `cache_read_tokens`, …
- A content block is tagged **`type`**, not `kind` — a block has a type, a *message* has a kind, and having
  one word mean both was the ambiguity that made this version necessary.
- Block names follow: `tool_use`, `tool_result`; and their fields `tool_use_id`, `is_error`, `tool_name`,
  `media_type`, `data_ref`.
- Added: message `kind` and `origin`; session `system_prompt` and `lineage`; `tool_use.namespace`;
  `usage.reasoning_tokens` and `usage.cost_usd`.
- `extra` is namespaced by harness name rather than flat.

A reader that wants to accept both can switch on the version key, or simply accept either spelling per field
— clustervision's browser reader does the latter, because principle 10 outranks tidiness on the way in.

> Galaxy-brained by pug. If you ship a harness, please consider emitting OpenSession too — then everyone's
> sessions are portable by construction. 🤝
