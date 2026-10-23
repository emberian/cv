# Search (full-text & semantic)

Two ways to find a session from any harness: **full-text** (you remember a word) and
**semantic** (you remember what it was about). 🔮

- **Full-text** is a [tantivy] inverted index: BM25 ranking, fielded filters, phrases, booleans,
  highlighted snippets.
- **Semantic** embeds every session and your query, and ranks by cosine similarity: it finds a
  session that shares no words with the query.

Both run through `cv index` and `cv search` ([the CLI](cli.md)); the `cv-search` crate is the
library behind them.

## Building the index

The index is a cache; transcripts are the truth. `cv search` reads live whatever the index has
not seen ([below](#how-cv-search-resolves)), so no timer or cron job is needed; `cv index`
pre-pays that read.

```sh
cv index              # build/refresh the full-text index
cv index --semantic   # also build semantic embeddings
```

```text
✦ building full-text index…
indexed 412 session(s) → /Users/you/.clustervision/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 412 session(s) → /Users/you/.clustervision/embeddings.bin
```

`cv index` is incremental: it reads only new or changed sessions and reaps vanished ones.
`--rebuild` starts from scratch. `--semantic` downloads a ~30 MB model on first use.
`--subagents` also indexes the sub-agent and workflow forest (off by default; it can add
hundreds of MB).

A session that cannot be read (changed twice during the pass, or a failed read) stays out until
a later pass; `cv index` warns with the count and exits 0. `--json` lists each with its reason;
`--strict` exits 1. A persisted tool output it cannot open is dropped with a note; the session is
still indexed.

### What gets indexed

All of a session's text: title, message text and thinking, tool names and JSON inputs, tool
results and file paths, ranked as one session; plus `id`, `harness`, `cwd` (tokenized, so path
fragments match), `title` and timestamps.

The text has two parts. The **exchange** is what the person asked, what the model replied and
ran, what came back, and what sub-agents were handed and returned. The **context** is the
injected instruction files (`CLAUDE.md`, `AGENTS.md`), system prompts, skill bodies, hook output,
notices and compaction summaries. Queries match the exchange by default; `--kind context` or
`--kind all` match the rest. A turn a fork copied (a Claude `--fork-session` copy) matches only in
its source.

### Where the index lives

Under `$CLUSTERVISION_HOME` (default `~/.clustervision`):

| Index            | Path                                  |
| ---------------- | ------------------------------------- |
| Full-text        | `~/.clustervision/tantivy/`            |
| Semantic vectors | `~/.clustervision/embeddings.bin`      |

## Full-text search

```sh
cv search "tantivy bm25"
cv search "kubernetes" --harness claude --limit 50
```

```text
claude    a1f3c2d9  2026-05-21  Rust tantivy index
          … We built a full text search engine with <b>tantivy</b> and BM25 scoring.
```

Bare terms search the title, the exchange and cwd, and all must match. tantivy syntax works:

- **Fielded filters**: `harness:claude foo`.
- **Phrases**: `"formal verification"`.
- **Booleans**: `lean AND proof`, `rust OR zig`.

`--limit` caps rows (default **20**); `--harness` filters before ranking, in both modes.

### How `cv search` resolves

`cv search` answers from the index **plus the unindexed suffix**: every session grown,
rewritten or new since `cv index` is read live, newest first, and ranked by the same BM25 with
shared corpus statistics. A grown session is read from the index's resume mark (taken every
4 MiB of text); others are read whole. By default every such session is read; `--live-bytes N`
bounds the read to N bytes of source, and a session past it keeps what the index last saw.
Without the flag, `$CV_LIVE_BYTES` (bytes, or `all`) and then `[search] live_bytes` in
[`config.toml`](cli.md#cv-config) set the budget, so every caller on a host (the MCP server
included) shares one bound. The footer reports what was read, the budget and where it was set:

```text
(live: read 3 session(s) the index has not seen live (2.1 MiB, 2 from where the index stopped))
```

With `--live-bytes 268435456`:

```text
(live: read 3 session(s) the index has not seen live (2.1 MiB of the 256.0 MiB --live-bytes budget, 2 from where the index stopped))
(note: the index is ~2h behind for 1 session(s) not read live: 1 over the --live-bytes budget (412.0 MiB) — run `cv index`)
```

With no index, every session is unindexed and read live (the cost of `cv index`, every time).
`--frozen` or `--live-bytes 0` answers from the index alone; with no index, `--frozen` exits
naming `cv index`. An unusable index is always an error. The live read builds its private index
in a temporary directory and removes it, so a read-only home is still searchable. A `-q text:`
predicate resolves the same way (all of it); it exits 2 only when a session without an index
could not be read.

`--scan` matches case-insensitive substrings over each session's whole exchange, streamed,
in discovery order, without operators or ranking, and says so on stderr. Invalid query syntax is
an error, never a fallback to scanning. `--kind` cannot be combined with `--scan`.

## Semantic / meaning search

```sh
cv search "formalizing proofs" --semantic
```

A Lean session that never says "formalizing" still ranks first: the query and "proving theorems
in Lean" are close in meaning. `--semantic` prints the same rows as full-text and needs
`cv index --semantic` first; without embeddings it errors instead of falling back to keywords.

### From a session row to the material you wanted

[`cv pack`](pack.md) (formerly `cv recall`) runs the same fused full-text and semantic ranking and
excerpts each hit into a bundle for a fresh agent:

```sh
cv pack "how did we handle retry backoff"        # a CLAUDE.md-style context bundle
cv pack "auth token refresh" --limit 3
```

Without an index it scans live (noted on stderr); without embeddings it ranks full-text only.

## How semantic embeddings work

[model2vec] static embeddings (`minishlab/potion-base-8M`, ~30 MB, 256 dimensions): token vectors
are looked up and mean-pooled, with no transformer pass or ONNX runtime. Every session is
embedded up front; queries do a brute-force in-memory cosine scan.

A session's vector embeds the head of its exchange: never the context, never turns a fork copied.
The head is what the model reads, 512 tokens cut first to 512 × the vocabulary's median token
length in characters (3,072 for potion-base-8M); `cv index --semantic` prints it.
`--kind` is full-text only.

The model downloads from the HuggingFace Hub on first use into `~/.cache/huggingface`. For
offline use, point `CV_SEARCH_MODEL` at a local copy:

```sh
export CV_SEARCH_MODEL=/path/to/potion-base-8M
cv index --semantic
```

## Quick reference

| Command                          | What it does                                        | Limit flag (default) |
| -------------------------------- | --------------------------------------------------- | -------------------- |
| `cv index`                       | Build/refresh the full-text index                   | —                    |
| `cv index --semantic`            | Also build semantic embeddings                      | —                    |
| `cv search <query>`              | Full-text search (BM25), index plus unindexed suffix | `--limit` (20), `--live-bytes` (all) |
| `cv search <query> --semantic`   | Semantic search (no keyword fallback)               | `--limit` (20)       |
| `cv pack <task>`                 | Fused ranking → relevant spans, as a context bundle | `--limit` (8)        |

See also [MCP](mcp.md) for search inside a running agent.

[tantivy]: https://github.com/quickwit-oss/tantivy
[model2vec]: https://github.com/MinishLab/model2vec
