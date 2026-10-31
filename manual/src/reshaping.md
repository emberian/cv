# Reshaping a lane: `distill`, `splice`, `rewind`, `adopt`

A swarm root runs many sub-agent lanes. A long lane reaches 500K–900K tokens of context, every
tool round re-reads all of it, and the two usual moves are both bad: keep going (slow, expensive,
near the ceiling) or hand off to a fresh successor through a prose brief (lossy; the successor
relearns what the brief did not say). The transcript itself is the best record of what the lane
knows; it is only too big, and most of its bulk is tool output that has already been acted on.

The Reshape verbs produce a **new** session (or sub-agent) from an existing one; the source is never
touched. Four of them are about lanes:

| you want | verb |
|---|---|
| the same lane, its old tool output moved to a sidecar, resumable | [`cv distill`](cli.md#cv-distill) |
| the lane exactly as it stood at message N, to run variants | [`cv splice <id>:..N --session`](cli.md#cv-splice) |
| the agent as of a past message or the commit it made, as a top-level session | [`cv rewind`](cli.md#cv-rewind) |
| sub-agents of a dead root moved under a live one so it can `SendMessage` them | [`cv adopt`](cli.md#cv-adopt) |

`cv prune` is the sibling for one long session, a main thread or a sub-agent transcript (by id or
path): same transcript, bulky old tool payloads lifted into a sidecar, written as a standalone
session under a new id. It never plants the result under a live parent; `cv adopt` moves child
transcripts between parents.

`cv prune --plan <plan.json>` takes the choice of payloads from a judge (a rewound copy of the
session, say) instead of `--min-size`/`--keep-last`. Each listed `tool_use_id` is `keep`, `precis`
(the marker, then the précis) or `excerpt` (the précis, then literal passages of the payload in
their order, between `[…]` lines); unlisted results stay verbatim, and `cv cat <new-id> <id>`
returns every original. `attachments.drop_kinds` removes attachment records of those types,
stashed by record uuid, their children relinked. An unknown id or a passage that is not in its
payload refuses the whole plan (exit 2) before anything is written.

## How Claude Code stores a sub-agent

```
~/.claude/projects/<cwd-slug>/<root-session-id>.jsonl            the root's transcript
~/.claude/projects/<cwd-slug>/<root-session-id>/subagents/
    agent-<agentId>.jsonl          one per sub-agent; agentId = "a" + 16 hex
    agent-<agentId>.meta.json      {agentType, description, toolUseId, spawnDepth, requestShape, model}
```

Every line of an `agent-*.jsonl` carries `isSidechain: true`, `agentId` and the **root's**
`sessionId`; records thread by `uuid`/`parentUuid`. The harness keeps no in-memory registry of what
it spawned: `SendMessage(to=<agentId>)` resolves `<root>/subagents/agent-<agentId>.jsonl` on disk,
even for a file planted while the root is running. That is what makes `--agent-of` and `adopt`
work, and it has two consequences:

- an agent belongs to **one root session id**; a root resumed with `--fork-session` runs under a new
  id and looks in a different `subagents/` dir;
- **thinking is almost all signature-only** in these files: across 60 recent sub-agent transcripts,
  5,496 of 5,585 thinking blocks carried no text. A lane's reasons survive any reshaping only where
  the lane wrote them down: in its text, its `SendMessage`s, its notes and commit messages. A lane
  prompt line such as "state each decision and its reason in your text or STATUS file" is what
  makes a distill (or any handoff) keep the *why*.

## What `cv distill` keeps and drops

Deterministic, rule-based, no model calls, over the IR (any harness parses; emission is Claude).

Kept **verbatim**:

1. **The brief**: the first human prompt.
2. **The timeline**, in order: every coordinator prompt, every peer message (`queued_command`
   attachments), sub-agent returns, compaction summaries, harness errors, and everything the agent
   said or wrote itself — text blocks, outbound `SendMessage`s, notes written to prose files
   (`Write`, or a shell heredoc to a non-code path such as `STATUS.txt`), the sub-agents it spawned.
   A file whose path appears in the brief or in a message to the agent is an instruction by
   reference, so its latest read is kept too (capped at `--note-max`).
3. **The last N tool calls** (`--keep-last`, default 12) as real turns: results capped at
   `--tail-result-max` (head + tail, with a `cv cat` pointer for the rest), thinking dropped unless
   `--keep-tail-thinking`, recorded `usage` stripped so the resume gate never reads the source's
   near-limit size, and a trailing call that never got its result removed.

Kept as an **index**, mined from tool inputs and outputs: commits made, branch tips, repository
heads (the first line of the newest `git log` per `host:dir`), branches, hosts reached over
ssh/scp/rsync, the most-referenced paths, build/test verdicts, and every tool error (its
error-shaped line). The **ledger** is one line per tool call, `[index] label → outcome`, where the
label is the Bash `description` when there is one; consecutive repeats collapse to `×N`
(`--no-ledger` omits it).

**Dropped**: every tool output outside the tail goes to a sidecar `<new>.flat.jsonl` in prune's
format, so `cv cat <new> <tool_use_id>` returns it. Harness reminders, hook records and thinking are
dropped. Token estimates use cv's shared estimator; the pack itself is priced at the tool-input rate
(2.1 bytes per token), since resumed distilled packs measured 2.2–2.4.

What a distilled lane cannot answer, by design: a detail that lived only in a file the lane *read*
(the read is elided; a resumed lane has tools and `cv cat`, so it can re-read), and a "why" the
lane never wrote down (it was in signature-only thinking).

## The dangling-parent trap

Claude Code loads a session by walking `parentUuid` back from the newest record and **stops at the
first missing link without an error**. A lane's turns often point at records the IR does not carry,
such as `hook_success` attachments, so a naïvely emitted session resumes with only its last one or
two records: the pack is never seen, nothing errors, and the agent answers from whatever the tail
happened to hold. The Claude emitter therefore owns the rule, for every verb that writes a
transcript (`distill`, `splice`, `port`, `pack --format session`): a parent that names a record not
in the file is re-linked to the record before it, a call whose result is not in the file is dropped,
and the records carry one identity (a main-thread session, or a sub-agent of the named root).
`emit_verified` re-reads the file and asserts all three; `emit::tests::reshape_verbs_emit_resumable`
and `tests/cli.rs::distill_emits_a_whole_resumable_session_and_a_sub_agent` walk the chain the way
Claude does.

## Three ways to run the result

- **As a sub-agent of a root** — `cv distill <lane> --agent-of <root-session-id>` writes a new
  `agent-*.jsonl` (+ meta, `description: distilled: <label>`, the source's model unless
  `--model <id>` names another; `inherit` is the root's current model) into the root's
  `subagents/`; the root sends the next quest with `SendMessage` to the printed id and the reply
  comes back as an ordinary task notification. No new process, no result file. Only the root should
  plant into its own `subagents/`: the file is a new lane in that root's world.
- **As a headless worker** — `cv distill <lane> --session [--cwd DIR]` writes a new main-thread
  session; `claude -p --resume <id> "<next quest>"` loads it. Add `--fork-session` to question one
  distilled session repeatedly without growing it.
- **As a pack** — `cv distill <lane> --pack lane.md` for a consultant or reviewer that needs to know
  what a lane did, not to *be* it. `--with <other>` (repeatable) appends another lane's findings —
  its own words and its facts, not its brief, ledger or tail — for merge-context.

`cv distill <id> --upto N` is a distilled fork: the lane as it stood at message N. `cv splice
<id>:..N (--session | --agent-of <root>)` is the verbatim prefix `[0, N)` instead; a call whose
result falls after the cut is dropped by the emitter.
