# DISTILL — a transcript as a reshapeable lane context

Status: built on branch `distill` (`cv distill`, `cv fork`), measured on three real lanes on
2026-10-04. Not released.

## The problem

A swarm root runs many sub-agent lanes. A long lane reaches 500K–900K tokens of context, and every
tool round re-reads all of it. At that point there are two moves today, and both are bad:

- **keep going**: each round is slow and expensive, and the lane is near its ceiling;
- **hand off to a fresh successor** through a prose brief the root or the lane writes. The brief is
  lossy, and the successor relearns what the brief didn't say.

The transcript itself is the best record of what the lane knows. It is just too big, and most of
its bulk is tool output that has already been acted on. The idea is to **reshape the transcript
into a new, much smaller context**, then either resume it as a live worker or hand it to a fresh
one.

## How Claude Code stores a sub-agent (2.1.289, read from disk)

```
~/.claude/projects/<cwd-slug>/<root-session-id>.jsonl            the root's transcript
~/.claude/projects/<cwd-slug>/<root-session-id>/subagents/
    agent-<agentId>.jsonl          one per sub-agent; agentId = "a" + 16 hex
    agent-<agentId>.meta.json      {agentType, description, toolUseId, spawnDepth, requestShape, model}
~/.claude/projects/<cwd-slug>/<root-session-id>/tool-results/<id>.txt   oversized outputs
```

- Every line of an `agent-*.jsonl` carries `isSidechain: true`, `agentId`, and the **root's**
  `sessionId`. Records thread by `uuid`/`parentUuid`.
- Most lines of a busy lane are disk-only bookkeeping that the model never sees: about two thirds of
  the lines (65–66%) are `attachment` records, mostly `hook_success`. Mid-run messages from the root or peer
  lanes arrive as `attachment.type = "queued_command"` (an `<agent-message from=…>` wrapped in a
  system reminder), and a coordinator's direct prompts arrive as plain user strings.
- **Thinking is signature-only.** None of the three lanes measured (45, 111 and 172 thinking blocks)
  has a single byte of thinking text; each block is an opaque signature (188 KB in the smallest
  lane). The model can read it back on resume; nothing else can, including cv. So **a lane's reasons
  survive any reshaping only where the lane wrote them down**: in its text, its `SendMessage`s,
  its notes and commit messages.
- Context composition of the largest lane (SCHOLAR-OS, 870K recorded): 1.5 MB tool results, 0.46 MB
  thinking signatures, 0.37 MB tool calls, 28 KB of assistant text.

## Can a reshaped transcript be resumed as a live worker? (measured)

Tested with throwaway haiku/sonnet roots in a scratch project (never the live swarm).

### (a) As a sub-agent of the root, via SendMessage — **works**

| test | what | result |
|---|---|---|
| T1 | a headless root spawns a sub-agent told the word PINEAPPLE | agent `a2d344dae921e7de1` |
| T2 | a **new process** resumes the root, `SendMessage(to=<that id>)` | "Resuming agent a2d344d", it answers PINEAPPLE |
| T3 | plant `agent-a0d15711ab0c0ffee.jsonl` + meta (a copy with MANGO) in the root's `subagents/`, never spawned by it | resumes, answers **MANGO** |
| T3b | planted with **no** `meta.json` | resumes, answers KIWI |
| T3c | the root itself `cp`s a new agent file in **mid-run**, then sends to it | resumes, answers PAPAYA |
| T4 | `cv distill <UPGRADE lane> --agent-of <scratch root>`, root sends the 10-question probe | resumes **on opus** (model from the copied meta), loads **38.1K** tokens (the lane was at 249K), answers 9/10 |

The harness does not keep an in-memory registry of what it spawned. `SendMessage(to=<agentId>)`
resolves `<root-session-id>/subagents/agent-<agentId>.jsonl` on disk, even for a file planted
while the root is running. Constraints we hit:

- The agent belongs to **one root session id**. A root resumed with `--fork-session` runs under a
  new id, looks in a different `subagents/` dir, and reports "No transcript found for agent ID".
- The transcript must be **one unbroken `parentUuid` chain** (see the trap below).
- `meta.json` is optional for resume, but it carries `model` and the label. Distill inherits the
  source's meta and sets `description` to `distilled: <label>`.

This is the most useful path for a swarm. The root reshapes a heavy lane into a new agent id, sends
it the next quest, and the reply comes back as an ordinary task notification. There is no new
process to manage and no result file.

### (b) As a headless worker, `claude -p --resume <new-id> "<next quest>"` — **works**

`cv distill <lane> --session [--cwd DIR]` writes a new main-thread session (`isSidechain` false,
fresh session id, no `agentId`). `claude -p --resume <id> --output-format json` loads it. Measured
total loads: 44K / 79K / 130K tokens, against 249K / 478K / 870K at the source's last turn. Use
`--fork-session` to question one distilled session repeatedly without growing it. Claude Code also
has `claude --bg --resume <id>` (a background session listed by `claude agents`, addressable by
cross-session `SendMessage`). That route exists but was **not measured** here.

### (c) As a fresh agent whose first message is the pack — **works**, no resume mechanics

`cv distill <lane> --pack lane.md` and paste or read it as the first message. It costs about the same
tokens as (b): the tail is rendered as markdown instead of real turns, so the model reads about
itself rather than being mid-motion.

### Trap found on the way: one dangling parent silently truncates a resume

Claude Code loads a session by walking `parentUuid` back from the newest record, and it **stops at
the first missing link without an error**. A lane's turns often point at records the IR does not
carry, such as `hook_success` attachments. The first emitted sessions therefore resumed with only
their last one or two records: 25K tokens loaded instead of about 44K, the pack never
seen. Nothing errored: the runs answered UNKNOWN to 3, 9 and 8 of their 10 questions, from whatever the last records happened to hold. Fix:
`distill::linearize` clears every parent so the emitter chains records in order (also applied to
`cv fork`). `tests/cli.rs::distill_emits_a_whole_resumable_session_and_a_sub_agent` walks the
chain the way Claude does. Disabling the fix turns that test red ("a dangling parent truncates the
resumed context").

`cv port`/`splice`/`loom` keep source parents too, so they are probably exposed to the same
truncation when a source has hook attachments. Not investigated here.

## `cv distill` — what it keeps and drops

Deterministic, rule-based, no model calls, over the IR (any harness parses; emission is Claude).

Kept **verbatim**:
1. **Brief**: the first human prompt.
2. **Timeline** in order: every coordinator prompt, every peer message (`queued_command`),
   sub-agent returns, compaction summaries, harness errors (usage-limit stops), and everything the
   agent said or wrote itself: text blocks, outbound `SendMessage`s, notes written to prose files
   (`Write`, or a shell heredoc to a non-code path such as STATUS.txt or MERGE-QUEUE.md), and
   sub-agents it spawned.
   **Documents named in its instructions**: a file whose path appears in the brief or a message to
   the agent is an instruction by reference, so its latest read is kept (capped at `--note-max`).
   Example: ROOT's `resume/RETENTION-PAYERS.md`.
3. **Last N tool calls** (`--keep-last`, default 12) as real turns: results capped at
   `--tail-result-max` (head + tail, with a `cv cat` pointer for the rest), thinking dropped unless
   `--keep-tail-thinking`, recorded `usage` stripped (so the resume gate never reads the source's
   near-limit size), and a trailing call that never got its result removed.

Kept as an **index** (mined from tool inputs and outputs):
- commits made (`[branch sha] subject` lines);
- branch tips (`<sha> refs/heads/<b>`);
- **repository heads**: the first line of the newest `git log` per `host:dir` (or `[ref]`). A
  command that runs several `git log`s is skipped, because its output can't be attributed;
- branches, hosts reached over ssh/scp/rsync, the most-referenced paths;
- build/test verdicts (the verdict line of commands that look like builds), and every tool error
  (its error-shaped line, not its first line).

The **ledger**: one line per tool call, `[index] label → outcome`. The label is the Bash
`description` when there is one, and lanes write good ones ("Take ours for surfaces json
(regenerate later)"). Consecutive repeats collapse to `×N`.

**Dropped**: every tool output outside the tail goes to a sidecar `<new>.flat.jsonl` in prune's
format, so `cv cat <new> <tool_use_id>` returns it, and the source transcript is never touched.
Harness reminders and hook records are dropped, and so is thinking.

Token estimates use 2.4 characters per token. That figure is measured: three resumed distilled
sessions, harness base subtracted, ran 2.2–2.4. Prune's 3.5 bytes/token undercounted this
identifier-dense content by 1.5–2.2×.

## Quality check

**Method.** Three paused lanes from session c1e4d723 were copied to scratch: UPGRADE
`a5e2da77f3df2384b`, RETENTION-PAYERS `a04ff45c2ecd2561e` and SCHOLAR-OS `ae5ec47c7b8ba51ba`. For
each copy, a separate opus agent wrote 10 gold questions **from the full transcript, blind to the
distiller**: 2 current state, 2 next step, 2 traps, 2 why, 1 instruction, 1 mid-transcript detail.
Each question has a gold answer and required tokens. The answerer was `claude -p --model
sonnet[1m] --tools ""`, told to use only its context. I graded every answer by hand against the
gold: 1 if correct, ½ if partial or hedged, 0 if wrong or UNKNOWN.

Conditions:
- **full**: the whole rendered transcript in context (the baseline);
- **pack**: the markdown pack (c);
- **resumed**: the `--session` emission, resumed (b);
- **agent**: the `--agent-of` emission resumed through SendMessage (a), UPGRADE only, opus.

Tokens are the measured total prompt of the answering call. Every call in this harness carries a
22.6K base (system prompt, tools, injected context); the source lane's own base is in its 249K
etc. too.

| lane | source at last turn | full | pack v1 | pack v2 | resumed (v1) | agent (v1) |
|---|---|---|---|---|---|---|
| UPGRADE | 249K | 217K → **10** | 44K → **9** | 46K → **9** | 44K → **9** | 38K → **9** |
| RETENTION-PAYERS | 478K | 442K → **9.5** | 78K → **8.5** | 81K → **9.5** | 79K → **8** | — |
| SCHOLAR-OS | 870K | 798K → **10** | 130K → **10** | 148K → **9.5** | 130K → **9.5** | — |
| **total /30** | | **29.5** | **27.5** | **28** | **26.5** | 9/10 |

In other words, the distilled context answers 88–93% of what the full transcript answers, at
roughly a sixth of the load (5.4–6.7× smaller including the base; 7–10× on the conversation alone).
Per-question cost fell from $0.83 / $1.74 / $3.16 (full) to $0.03–$0.49.

v2 = v1 plus two rules added **after reading the questions**: repository heads from `git log`, and
documents named in instructions. Both are generic, but they were motivated by RETENTION-PAYERS
Q1/Q2, which they fixed. The v1 column is the blind result.

**What was missed, and why.**
- *A detail that lived only in a file the lane read* (UPGRADE Q10: which function builds the
  request Facts in `ObjectiveActivity.lean`). Every distilled form misses it, by design, because
  the read output is elided. A resumed lane has tools and `cv cat`, so it can re-read the file; the
  tool-less answerer could not. The full transcript got it.
- *Stale state.* RETENTION-PAYERS's tip moved in a clone the root prepared; v1's facts showed the
  old tip. v2 gets it from the referenced resume note.
- *A "why" the lane never wrote down* (RETENTION-PAYERS Q10, why the build moved boxes). All
  conditions, including the full transcript, got only half. The reason was in thinking, which is
  signature-only.
- *Hedging.* SCHOLAR-OS Q6 (ssh disconnect vs OOM at 16G): the distilled answers mention both and
  lead with the wrong one. Both facts are in the pack, but the conclusion is not stated in one
  place.

## `cv fork` and merge-context

- `cv fork <id> --at N (--session | --agent-of <root>)` emits the **verbatim** prefix `[0, N)` as a
  new resumable session or sub-agent. Calls whose results fall after the cut are dropped, and the
  chain is linearized. Use it to branch a lane at a decision point and run variants.
- `cv distill <id> --upto N` gives a **distilled** fork: the lane as it stood at message N.
- `cv distill <id> --with <other>` (repeatable) is merge-context. It appends another lane's
  **findings** to this pack: its own words (text, messages sent, notes written) and its facts, but
  not its brief, ledger or tail. On UPGRADE with RETENTION-PAYERS's findings, the pack grows from
  46 KB to 72 KB. This was not quality-tested.

## Recommendation for the swarm

1. **Distill instead of re-briefing.** When a lane nears about 400K, or has been paused, the root
   runs `cv distill agent-<id> --agent-of <root-session-id>` and sends the next quest to the printed
   id. The successor is the same lane at a sixth of the context: brief, every message, its own
   words and its last 12 steps verbatim, with everything else one `cv cat` away. No prose summary
   is needed from anyone.
2. **Lanes should write down their reasons.** Thinking is unrecoverable, so a decision lives only
   if it is in a `SendMessage`, a STATUS/notes file, or a commit message. A lane prompt line such as
   "state each decision and its reason in your text or STATUS file" is what makes a distill (or any
   handoff) keep the *why*.
3. **Use `--pack` for consultants and reviewers.** A fresh agent that needs to know what a lane
   did, but not to *be* it, reads the pack (c). `--with` hands it a sibling's findings too.
4. **Don't plant into a live root without the root's say-so.** `--agent-of` writes a new file into
   the root's `subagents/` dir (never modifying an existing one). That is safe, but it is a new
   lane in that root's world, so the root should be the one to run it.

## Not done / next

- An optional `--summarize` step (an LLM pass over the ledger to state conclusions in one place)
  would address the hedging failure. Deliberately not built: extraction stays deterministic.
- Emission is Claude-only. Distilling a Codex or other lane into a Claude worker should work through
  the IR but is untested.
- `claude --bg --resume` plus cross-session `SendMessage` (a background worker the root talks to by
  name) is the remaining resume path to measure.
- `cv port`/`splice`/`loom` likely share the dangling-parent truncation; they should linearize or
  re-link over records they drop.
