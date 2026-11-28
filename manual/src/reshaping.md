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

## Why rewinding and folding beat telephone

Multi-session agent workflows fail at both extremes: **endless telephone** (paraphrasing a living
session into prose handoff briefs or recursive LLM summaries) and **unpruned continuation**
(letting a single window bloat into context inertia until auto-compaction wipes its reasoning).
Controlled harness ablations and multi-month production audits establish six rules behind `cv`'s
Reshape verbs:

1. **Telephone attenuation vs. the whole-history fork trap.**
   On a 24-question blind continuation test, the full transcript scored **24/24**, deterministic
   `cv distill` scored **21.5/24**, the worker's `STATUS` file **11.5/24**, and a coordinator's
   prose resume summary **0.5/24**. Across 156 consecutive seat successions, coordinator briefs
   alone carried a median **6%** of the distinctive facts the successor went on to use, and brief +
   hand-written continuation file carried **37–43%** (while misreporting live processes and
   stranding messages to retired IDs); `cv distill` carried **73%** at ~69K tokens and selective
   `cv prune` carried **95–99%**. The opposite overcorrection—forking an orchestrator's entire
   700K coordination history into every helper—burns quota on irrelevant chatter. Resume veterans
   directly (`SendMessage` or `cv adopt`), or plant focused spans via `distill --agent-of` and
   `splice --agent-of` so successors wake in **first-person motion** (`--keep-last 12`) rather than
   reading a third-person obituary.
2. **Unpruned inertia AND untyped LLM compaction both degrade reasoning.**
   Even before hitting the context ceiling, unpruned long sessions suffer context inertia, a **75%**
   collapse in per-turn thinking tokens (`arXiv:2610.20563`), **98%** late-run zero-tool dawdling
   (`arXiv:2610.07625`), and **2.2×** cost with **64.6% → 35.5%** pass-rate erosion
   (`arXiv:2603.24755`). Yet untyped LLM compaction is worse: monolithic block rewrites suffer
   one-step context collapse (`arXiv:2510.04618`), recursive first-person summaries drive **7×**
   stylistic attractor convergence (`arXiv:2606.30571`) and elongate SWE runs **+13–15%** by
   smoothing away sharp error traces (`arXiv:2508.21433`), and plaintext-flattening compactors
   launder forged `[User]:` lines inside tool outputs into authoritative instructions **87–100%**
   of the time (`arXiv:2610.29869`, vs. **0%** when role boundaries and line-leading markers are
   fenced). Folding mechanically around **50–60%** of the window keeps sessions in their peak
   reasoning band with zero compactions.
3. **Rewinding beats tail repair—and `blame → rewind` creates instant keeper reviewers.**
   End-of-trace self-correction conditions on every wrong localization and bloated early draft
   already in context (`arXiv:2505.06120`: sharded multi-turn corrections lose **39%** accuracy from
   early-draft anchoring; consolidating prompts cleanly recovers **95%**). Mid-trajectory checkpoint
   forking flips **18–49%** of failed multi-agent runs where tail retry flips **0%**
   (`arXiv:2503.02068`, `arXiv:2512.06749`). `cv rewind <id> --at <msg|sha>` and
   `cv splice <id>:..N --session` cut back to a clean prefix at the same prompt-cache cost as an
   inline correction—and pairing `cv blame <path> -L lo,hi` with `cv rewind <id> --at <sha>` forks
   the original authoring session *at the moment it minted a commit*, giving an instant reviewer
   with 100% of that module's design context and rejected alternatives loaded.
4. **Escalation and downshift have opposite optimal handoff shapes (`arXiv:2608.24358`, `arXiv:2606.02875`).**
   Across 58,000 runs, handing a weaker model's raw trajectory—or its eroded dirty branch—to a
   stronger model (`LC → HC`) recovers under half the quality gain at **2.2×** per-step cost (worse
   than starting `HC` fresh from clean `base` + deterministic facts) and risks wrong-hypothesis
   inheritance (`arXiv:2606.02875`). Conversely, dropping a stronger model's trajectory when
   downshifting to a fast worker (`HC → LC`) forces **1.6–2.0×** rework steps.
   `cv distill --handoff escalate` drops predecessor narrative, consolidates user prompts + verified
   build/test verdicts (especially `post_failure_edit`, where handoff debt peaks), and quarantines
   predecessor notes as `[UNVERIFIED]`; `--handoff downshift` preserves the verified checklist and
   verbatim tail.
5. **Meno's paradox breaks bare pointers and voluntary self-pruning (`arXiv:2609.20804`, `arXiv:2509.23586`).**
   Tool observations and opaque thinking signatures exceed **80%** of long-session tokens
   (`arXiv:2508.21433`), yet agents voluntarily invoke self-pruning tools **0%** of the time and
   ignore bare `[recall_event(41)]` stubs in **56.3%** of settings—an agent does not know what it
   forgot to query. Surfacing *semantic* one-line receipts (command, exit code, touched symbols,
   error line) beside `cv cat <orig-session> <tool_use_id>` (`arXiv:2408.09559`) and freezing prefix
   spans into dyadic `--bricks` cuts observation cost **>50%** while keeping provider prompt caches
   hot and original session IDs stable across fold-of-fold (`eclo`).
6. **Live `/resume`, tool-schema cache hashes, and harness chatter have four mechanical traps.**
   - *Nested usage gate*: Claude Code's auto-compact check reads `usage.iterations[-1]` on
     `/resume`; leaving pre-fold counts intact triggers immediate compaction on arrival (`cv prune`
     and `cv distill` rewrite or strip both top-level and nested usage).
   - *In-place `/resume` vs. process relaunch*: `/resume <new-id>` inside the **same** terminal pane
     keeps the OS process and running background shells/sub-agents alive; spawning a new process
     orphans them unless the old session stays as a quiet keeper (`cv adopt --join --from <old>
     --into <new>` reconciles mid-switch helper turns; `--carry-agents` preserves
     `subagents/*.meta.json` so helpers never silently reset their `model` or `agentType`).
   - *Tool-schema prompt-cache trap*: prompt caches hash tool definitions ahead of messages, so
     non-destructive fork probes (`--fork-session`) should keep tool schemas identical (enforcing
     read-only via permissions/hooks rather than `--tools ""`, which forces a full cache rewrite).
   - *Orientation records & signature-only thinking*: harness attachment/singleton records orient
     the model and link `parentUuid` chains (dropping them wholesale causes silent prefix
     truncation), while ~98% of sub-agent thinking blocks store opaque signatures with no text—so
     reasons survive reshaping only when stated in replies, notes, or commit messages.

`cv prune` is the sibling for one long session, a main thread or a sub-agent transcript (by id or
path): same transcript, bulky old tool payloads lifted into a sidecar, written as a standalone
session under a new id. It never plants the result under a live parent; `cv adopt` moves child
transcripts between parents.

`cv prune --plan <plan.json>` carries a selection made by whoever holds the context (the agent
itself, or a judge on a rewound copy) instead of `--min-size`/`--keep-last`. `results` names tool
results by `tool_use_id` and `messages` names records by `uuid` (a prompt, a reply, an
attachment); each is `keep`, `pointer` (the marker alone), `precis` (the marker, then the précis),
`excerpt` (the précis, then what is kept verbatim in payload order: literal passages between `[…]`
lines, and 1-based `lines` such as `"3-10"` or `"40-"` under a line naming them) or `drop`.
Everything unlisted stays verbatim, and `cv cat <new-id> <id>` returns every original: a dropped
record by its uuid, a dropped result by its id with its call as `<id>#call`, a replaced message by
its uuid (`<uuid>#b<n>` per content block, `<uuid>#r<n>` per rendered item of an attachment).

```json
{"results":  {"toolu_01": {"action": "pointer"},
              "toolu_02": {"action": "excerpt", "lines": ["1-12", "240-"]},
              "toolu_03": {"action": "drop"}},
 "messages": {"<uuid of a message with a pasted log>": {"action": "excerpt", "lines": ["1-2", "400-"]},
              "<uuid of a stale attachment>": {"action": "drop"}},
 "attachments": {"drop_kinds": ["hook_success"]}}
```

A dropped result takes its call with it and a dropped record takes the calls and results it holds
with their other halves, so the result is always a session Claude Code resumes; a record left
with no content goes too, its children relinked. A transcript does not mark where a block pasted
into a person's message begins, so the plan keeps their own lines with an excerpt. An attachment
takes `pointer`, `precis` or `drop`: its `rendered` text, which is what a resume sends, goes
behind the marker and the record stays what it was. An unknown id, a passage or line that is not in
its payload, or a result planned one way inside a record the plan drops refuses the whole plan
(exit 2) before anything is written. One thing is checked for the caller and only warned about: a
kept reply whose turn's tool results were all dropped or pointered "rests on nothing kept"
(`plan.rests_on_nothing` in `--json`).

**Other harnesses.** Claude Code sessions are rewound and pruned record by record. Codex, Gemini,
OpenCode, Qwen, Devin, Pi and OpenSession sessions (any harness with a verified emitter, and OpenSession
documents) are rewound and pruned by selection: cv reads the session into the IR, selects the
prefix through `--at` (a message index or a commit sha, as for Claude) or replaces each tool output
over `--min-size` before the last `--keep-last` messages with a `[PRUNED …]` marker, and emits the
result under a fresh id to the same harness (`--out <dir>`, else its store), reading it back to
report what the target did not keep. Pi has no emitter: its selection is written as a native
branch of the source file, beside it as `<created>_<new-id>.jsonl` (or under `--out`), holding the
header under the new id and every entry on the chain from the root to the cut, bytes unchanged
except each pruned result's text; abandoned sibling branches are left out. A prune writes the originals to `<new_id>.flat.jsonl` beside
the new session, so `cv cat <new-id> <tool_use_id>` answers every marker. Flags that edit Claude
records (`--plan`, `--window`, `--range`, `--declassify`, `--drop-thinking`, `--carry-agents`,
`--skills`, `--uncompact`) refuse there with exit 2. Pass `--harness` with a transcript path whose
format cv would otherwise read as Claude's.

What a session wrote and read on disk is already a mechanical listing, paths from its tool calls:
`cv events <id> --kind file_edit` and `--kind file_read` (`--json`, `--subagents`), one streamed
pass. It is worth pasting into the précis of whatever a plan drops.

`--carry-agents[=meta|full]` on `prune`, `rewind` and `splice --session` takes the session's
sub-agents along under the new session id (unlike `prune --copy-resources`, which copies
`subagents/` and `workflows/` verbatim for `cv`'s forest views when resuming in a new process).
Claude Code re-enters a background agent from the current session's `subagents/` folder, and a
derived session has a new id: without its `agent-<id>.meta.json` the agent comes back on the
parent's model as a general-purpose agent, and without its transcript it has no brief. `meta` copies
the spawn records; `full` (the default) also copies each transcript with the session id restamped.
A value needs `=` (`--carry-agents=meta`), so the bare flag never takes the session id after it.
A transcript goes the way an adoption does: the source (including any `/resume`-split prefix) is
checked while it is read, the staged copy is checked against the resume rules, and it is published
with its meta as one set. A source whose last record is unfinished (its writer is mid-append) is
carried up to that record, and the report says so. A transcript whose content is refused (a whole
line that is not a record, or a copy the resume check will not pass) is skipped and named under the
line; its spawn record still goes and every other agent is carried. The session is written, so
`prune`, `rewind` and `splice` exit 0 with the warning; `cv adopt --join` exits 1.

An agent that is mid-turn when its parent moves keeps running and writes its next records into
the new session's folder: a file that begins mid-conversation, with no meta. So a transcript the
new session already holds is joined, never skipped or replaced: the source's records first, then
the records only the new file holds, and a missing meta is copied. `cv adopt --join --from <old>
--into <new>` does the same after the fact, for every agent of the old session or the ids/prefixes
named, and a repeat changes nothing.

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

Deterministic, rule-based, no model calls, over the IR (any harness parses; `--session=<harness>`
emits to any harness cv writes).

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

**Distilling a distilled session.** The pack is one opening turn whose origin is `import`, never
a person's, and it carries a seed record: the source, how many human instructions the pack holds,
a digest of them, and the pack's length. A later distill reads the brief and the human prompts
back out of the pack only as far as that record vouches for them. Claude transcripts keep the
record beside the turn (`origin.cvSeed`); where it does not check out, because a `· user` entry
was written into the stored pack or an instruction was edited, every entry keeps its words and is
carried as a message to the agent instead of a human prompt. A format with no room for the record
falls back to the pack's own headings, which non-human text cannot forge.

### Who the pack is for: `--handoff`, `--state-header`, `--bricks`

`--handoff peer|escalate|downshift` writes the pack for its successor. Every mode opens with the
**continuation-state badge** (`--state-header` adds it alone), read from the last build/test
command the facts index recognizes and the file edits around it: `post_failure_edit` (it failed,
then edits nothing re-checked), `failing_validation` (it failed, nothing edited since),
`verified_clean` (it passed, nothing edited since) or `in_progress` (no validation, or edits after
the last pass).

- `peer`: the default pack plus the badge.
- `escalate`, for a stronger model after repeated failures: the agent's own narrative leaves the
  timeline, which keeps every prompt and message to it in order (the user specification) and the
  decisions recorded. Commits, branches, files edited, verdicts, errors, unresolved calls and the
  ledger stay. `## 4c. Reads behind later edits and errors` keeps the headings or definition lines
  of the newest read of each file the lane later edited or named in an error, with its `cv cat`
  pointer. Its last message, its grounding and the notes it wrote move under
  `## 5b. [UNVERIFIED PREDECESSOR NOTES — VERIFY AGAINST SOURCE & TESTS]`. `--keep-last`
  defaults to 0.
- `downshift`, for a cheaper model: the specification, a `## 4b. Verified progress` checklist (an
  edit is `[x]` once a later validation passed) and a 6-call tail; no ledger.

`--bricks` renders the turns before the tail as extractive **prefix bricks**: frozen 16-turn
epochs, then the rest in power-of-two spans aligned like a binary counter (24 turns are
`[0..16) [16..24)`, 25 add `[24..25)`, 32 are `[0..16) [16..32)`; epochs never merge). Each
brick names its `cv show <id> --range A..B` span and holds that span's human instructions
verbatim, the files it touched, each read with its `cv cat` pointer and up to 8 distinctive names
from the elided outputs (headings in prose, definitions in code), and each command's
`exit 0`/`FAILED` with its `cv cat` pointer. The bricks follow a source line without counts, the
continuation-state badge comes after them, and the resume preamble drops its counts, so the bytes
through every sealed brick stay identical as the session grows and a provider's prompt cache
keeps serving them.

What a distilled lane cannot answer, by design: a detail that lived only in the body of a file the
lane *read* (the read is elided beyond the names above; a resumed lane has tools and `cv cat`, so
it can re-read), and a "why" the
lane never wrote down (it was in signature-only thinking).

## Budgets and unfinished work

`cv distill ID --max-bytes 200000` refuses output over an exact UTF-8 byte limit, and
`--max-estimated-tokens 80000` output over cv's token estimate. Both measure what the target
loads, whole, after every `--with` finding:

- a standalone pack: the Markdown pack;
- `--session` and `--agent-of`: the content of the session about to be emitted, which is the
  preamble, the pack and the tail's real turns. The transcript file is larger (record envelopes,
  JSON escaping) and the sidecar of elided outputs is never loaded; the report gives both sizes
  on disk.

The report and `--json` (`budget`) name the quantity measured. The token figure is a byte
heuristic, not a tokenizer's count or a guarantee about a context window, and the harness adds
its own system prompt and tool schemas on top. Both limits are checked before publishing a
session, sidecar, pack, lineage edge or source retirement. Nothing is trimmed to meet them.

Resumable `--session` and `--agent-of` outputs refuse when the source grew during capture or a
call has no terminal evidence, and the refusal lists each such call with what is missing. A
missing result establishes uncertainty, not that a job is still running. Explicit failures and
cancellations settle calls. Background work settles only from evidence that names it:

- Claude: a launch (a background command, one moved to the background on timeout or by a
  message, an agent, an agent resumed by `SendMessage`, a monitor, a workflow) stays open until
  a task notification for it, a `TaskOutput` poll whose payload status is terminal, a `TaskStop`
  acknowledgement naming it, or a monitor's expiry notice. A poll that merely returned settles
  nothing.
- Antigravity: a matching task completion or cancellation message, a terminal status poll, or
  a kill acknowledgement naming the task.
- Codex: a cell or PTY session ends from its own result. In a script with several process
  calls, results are attributed only when there is exactly one per call, in order; a finished
  Code Mode wrapper does not settle a process it left running.

A completion is attributed to the command that launched the job, and running output is not a
build verdict.

Use `--allow-unsettled` for an intentional snapshot after inspecting the source. The pack's
hand-off lists every unresolved call (label, id, what is missing), in standalone packs too, and
`--json` carries them as `unsettled`. This does not stop or join jobs, and cannot recover job
identity or completion evidence that the harness has discarded.

## The dangling-parent trap

Claude Code loads a session by walking `parentUuid` back from the newest record and **stops at the
first missing link without an error**. A lane's turns often point at records the IR does not carry,
such as `hook_success` attachments, so a naïvely emitted session resumes with only its last one or
two records: the pack is never seen, nothing errors, and the agent answers from whatever the tail
happened to hold. The Claude emitter therefore owns the rule, for every verb that writes a
transcript (`distill`, `splice`, `port`, `pack --format session`): a parent that names a record not
in the file is re-linked to the record before it, a call whose result is not in the file is dropped,
and the records carry one identity (a main-thread session, or a sub-agent of the named root).
`emit_verified` re-reads the file and asserts all three, also rejecting parent cycles; `emit::tests::reshape_verbs_emit_resumable`
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
  distilled session repeatedly without growing it. `--session=<harness>` hands the lane to another
  harness instead (`codex`, `gemini`, `devin`, any [`cv port`](cli.md#cv-port) target): the pack
  and tail are emitted in its format, verified by reading them back, with the sidecar beside them.
- **As a pack** — `cv distill <lane> --pack lane.md` for a consultant or reviewer that needs to know
  what a lane did, not to *be* it. `--with <other>` (repeatable) appends another lane's findings —
  its own words and its facts, not its brief, ledger or tail — for merge-context.

`cv distill <id> --upto N` is a distilled fork: the lane as it stood at message N. `cv splice
<id>:..N (--session | --agent-of <root>)` is the verbatim prefix `[0, N)` instead; a call whose
result falls after the cut is dropped by the emitter.
