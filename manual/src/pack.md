# `cv pack` — the context compiler

Never explain your codebase to an agent twice. `cv pack` takes the task you're
*about* to start, recalls the most relevant material from every session you've
ever run — across every harness — and compiles it into a context bundle you can
hand straight to a fresh agent. 🎒

```sh
cv pack "tantivy chunked indexing"                      # CLAUDE.md-style bundle → stdout
cv pack "tantivy chunked indexing" --out CONTEXT.md     # …or to a file
cv pack "fix the parser" --format prompt                # shaped as a system prompt
cv pack "fix the parser" --format session --harness claude  # a synthetic resumable session
cv pack "migrate the daemon" --limit 3                  # draw from at most 3 past sessions
```

Select sources directly when you already know which conversations you need:

```sh
cv pack --session claude:SESSION --session codex:SESSION
cv pack "unfinished UI work" --session /path/to/transcript.jsonl
```

`--session` accepts an ID, `harness:ID`, or transcript path; prefix a non-Claude path with
its harness, for example `codex:/path/to/rollout.jsonl`. It bypasses corpus
search and does not need an index. An optional task focuses excerpts within those sources;
without one, the pack includes their first and recent requests, worker replies, referenced files,
and commands, with message indexes and source links. All explicit sources are included
(`--limit` only limits search recall). Repeating one source includes it once; different paths
with matching native IDs remain separate. A missing or unreadable selected source fails the
command before an output file is written.

## The pipeline

1. **Recall.** The task is run through `cv_search::Searcher` against the full-text
   index + live unindexed suffix, ranked by BM25 and deduped by session,
   capped at `--limit` sessions. Honest fallback: no index → a live suffix read
   with a stderr note (`cv index` makes packs instant).
2. **Distill.** Each recalled session contributes a *span*, not its whole
   transcript: a ~3-message excerpt window around the best match (streamed
   under lazy spans — huge sessions are never materialized; each message is
   shown up to `--excerpt-bytes` (default 1600), a longer one cut there with an
   open code fence closed and `…` appended), plus the event catalog's record of what that
   session actually **did** — which files it edited, which commands it ran.
   The digest is **purely extractive**: every line comes from a real transcript
   or the catalog, so nothing is hallucinated, zero network calls are made, and
   zero generative models are invoked.
3. **Emit** in the requested `--format`:
   - `md` (default) — a CLAUDE.md-style bundle: the task, one section per
     source (title, harness, date, cwd → key excerpt, files touched,
     commands run, a `cv show --range A..B` pointer back into the transcript), and
     a closing **"Files that keep appearing"** rollup ranked across sources.
   - `prompt` — the same content shaped as a second-person system prompt
     ("Prior context from earlier sessions: …").
   - `session` — a synthetic, resumable session in `--harness <h>`: a user
     turn framing the task with the bundle, an assistant acknowledgment, then
     emitted through the same machinery as [`cv port --harness`](conversion.md) — it
     prints the written path and the harness's resume incantation.

A pack is a bounded bundle by design, and its header says so: a `Bounds:` line
names each bound (excerpt bytes, 8 files per source, 12 rollup files) and how
many items each cut; task-focused packs keep distinct commands whole. Selected packs without a
task use the overview bounds: five requests, eight file events, eight commands, and twelve workers
per source, with request, command, and worker text capped by `--excerpt-bytes`. They report omitted
items per source; file events are a recent sample, not an exhaustive file-change inventory.

## Flags

| flag | meaning |
|---|---|
| `--format md\|prompt\|session` | output shape (default `md`) |
| `--harness <h>` | target harness, for (and only for) `--format session` |
| `--session SESSION` | explicitly select a source; repeat for more; bypasses search |
| `--limit N` | max sessions to recall by task (default 8; does not trim explicit selections) |
| `--out <path>` | write the bundle to a file instead of stdout |
| `--excerpt-bytes N` | bytes of one message an excerpt shows (default 1600) |
