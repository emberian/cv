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

## The pipeline

1. **Recall.** The task is run against the full-text index *and* the semantic
   embedding store (when it exists); the two rankings are fused by reciprocal
   rank and deduped by session, capped at `--limit` sessions. Honest fallbacks:
   no index → a live scan with a stderr note (`cv index` makes packs instant);
   no embeddings → full-text only (`cv index --semantic` adds semantic recall).
2. **Distill.** Each recalled session contributes a *span*, not its whole
   transcript: a ~3-message excerpt window around the best match (streamed
   under lazy spans — huge sessions are never materialized; code blocks are
   trimmed but kept intact), plus the event catalog's record of what that
   session actually **did** — which files it edited, which commands it ran.
   The digest is **extractive by default**: every line comes from a real
   transcript or the catalog, so nothing is hallucinated, and no network is
   touched and no model is loaded.
3. **Emit** in the requested `--format`:
   - `md` (default) — a CLAUDE.md-style bundle: the task, one section per
     source (title, harness, date, cwd → digest, key excerpt, files touched,
     commands run, a `cv show --range A..B` pointer back into the transcript), and
     a closing **"Files that keep appearing"** rollup ranked across sources.
   - `prompt` — the same content shaped as a second-person system prompt
     ("Prior context from earlier sessions: …").
   - `session` — a synthetic, resumable session in `--harness <h>`: a user
     turn framing the task with the bundle, an assistant acknowledgment, then
     emitted through the same machinery as [`cv port --harness`](conversion.md) — it
     prints the written path and the harness's resume incantation.

## Optional LLM distillation

Set `CV_PACK_LLM=1` (or `CV_PACK_LLM=<model-id>`) to additionally route each
recalled span through the configured LLM provider (`OPENROUTER_API_KEY`,
`ANTHROPIC_API_KEY`, or `LMSTUDIO_API_BASE=local` for free local inference —
the plumbing that used to be `cv distill`, folded into `pack` in 0.11.0) for an abstractive per-span digest on top of
the extractive one. Unset, `cv pack` is fully offline.

## Flags

| flag | meaning |
|---|---|
| `--format md\|prompt\|session` | output shape (default `md`) |
| `--harness <h>` | target harness, for (and only for) `--format session` |
| `--limit N` | max past sessions to draw from (default 8) |
| `--out <path>` | write the bundle to a file instead of stdout |
