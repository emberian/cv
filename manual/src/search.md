# Search (full-text & semantic)

clustervision indexes every session you've ever run — across every harness — so you
can find that one conversation again. There are two ways to look: **full-text**
search (you remember a word that was said) and **semantic** search (you only
remember what it was *about*). 🔮

- **Full-text** is a real inverted index ([tantivy], Lucene-style): tokenization,
  BM25 ranking, fielded filters, phrase and boolean operators, highlighted
  snippets. Fast and exact — it finds documents that contain your words.
- **Semantic** is meaning-based: every session is embedded into a vector, your
  query is embedded too, and results are ranked by cosine similarity. It finds
  the *right* session even when it shares no words with your query.

Both are exposed through the everyday [`cv`](cli.md) CLI; there's also a small
standalone `cv-search` binary that talks to the same on-disk indexes.

## Building the index

Search runs against a prebuilt index, so build it once (and refresh it whenever
you've accumulated new sessions):

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

`cv index` discovers every session from every supported harness and indexes the
ones that changed — it is **incremental** by default, re-reading only new or
modified sessions and reaping vanished ones, so routine refreshes are cheap. Pass
`--rebuild` to clear and rebuild from scratch. Add `--semantic` to *also* compute
embeddings; that step downloads a small embedding model (~30 MB) the first time
it runs, then caches it. `--subagents` folds the sub-agent/workflow forest in too
(off by default — it can add hundreds of MB).

The equivalent low-level commands on the standalone binary are:

```sh
cv-search index    # full-text only
cv-search embed    # semantic embeddings only (requires the `semantic` build feature)
```

### What gets indexed

Each session becomes one document built from its **entire textual content**, not
just metadata: the title, every message's text and thinking blocks, tool-call
names and their JSON inputs, tool results, and referenced file paths — all
concatenated into one searchable blob. Alongside that body, each document stores
the session `id`, its `harness`, the working directory (`cwd`, tokenized so path
fragments are searchable), the `title`, and created/updated timestamps.

### Where the index lives

Both indexes live under `$CLUSTERVISION_HOME` (default `~/.clustervision`):

| Index            | Path                                  |
| ---------------- | ------------------------------------- |
| Full-text        | `~/.clustervision/tantivy/`            |
| Semantic vectors | `~/.clustervision/embeddings.bin`      |

Set `CLUSTERVISION_HOME` to relocate them.

## Full-text search

```sh
cv search "tantivy bm25"
cv search "kubernetes" --harness claude --limit 50
```

```text
claude    a1f3c2d9  2026-05-21  Rust tantivy index
          … We built a full text search engine with <b>tantivy</b> and BM25 scoring.
```

Bare terms search across the title, body, and cwd. The query supports tantivy's
full syntax:

- **Fielded filters** — `harness:claude foo` restricts to a harness.
- **Phrases** — `"formal verification"` matches the exact phrase.
- **Booleans** — `lean AND proof`, `rust OR zig`.

Terms are **conjunctive by default**: every term must match (better precision
than OR). Results are ranked by BM25, with a highlighted snippet drawn from the
matching region of the body. `--limit` caps the number of rows shown (default
**20**); `--harness` filters by harness after ranking.

The standalone binary exposes the same query engine:

```sh
cv-search text "harness:codex pandas" --limit 5
```

Here `--limit` defaults to **10**.

### How `cv search` resolves

`cv search` prefers the tantivy index when it exists — and when present it's
*authoritative*: an empty result means "no match", not "go scan everything."
Only when no index is built does it scan sessions live (slow — that's what the
index is for). When you see `(no index yet — scanning live; run cv index for
instant search)`, build the index.

> Older releases also kept a SQLite full-text index as a middle tier; it's
> retired (tantivy is canonical). If a stale `index.sqlite` lingers in your
> clustervision home, `cv search` prints a note that it's safe to delete.

## Semantic / meaning search

Keyword search only finds documents that contain your words. Semantic search
finds documents that *mean* what you asked, even with **zero word overlap**.

> 🔮 Suppose you once spent an afternoon with Lean proving a theorem, but the
> word "formalizing" never appears in that transcript. A full-text search for
> `"formalizing proofs"` finds nothing. A *semantic* search for the same phrase
> ranks that Lean session right at the top — because "formalizing proofs" and
> "proving theorems in Lean" live near each other in meaning-space.

Two front doors, both backed by the embeddings store:

```sh
cv search "formalizing proofs" --semantic   # semantic mode of the normal search
cv-search semantic "formalizing proofs" -k 5
```

`cv search --semantic` embeds your query, ranks every stored session vector by
cosine similarity, and prints the same row format as full-text search. It needs
embeddings to exist — run `cv index --semantic` first.

> **No silent degradation.** `cv search --semantic` does **not** quietly fall
> back to keyword ranking when the embeddings store is missing: it errors and
> tells you to run `cv index --semantic`. A result set that says "semantic" is
> always actually semantic.

### From a session row to the material you wanted

`cv search` answers *which* sessions are relevant. When what you actually want is
the **material** — the relevant past spans, compiled into something you can hand
a fresh agent — that's [`cv pack`](pack.md), which runs the same fused full-text
+ semantic ranking and then excerpts each hit:

```sh
cv pack "how did we handle retry backoff"        # a CLAUDE.md-style context bundle
cv pack "auth token refresh" --limit 3
```

`pack` is where the old `cv recall` and `cv distill` went in 0.11.0: one verb for
"build context from the corpus." It degrades honestly too — no index means a live
scan with a stderr note, and no embeddings means full-text-only ranking.

## How semantic embeddings work

Semantic search uses [model2vec] *static* embeddings (model
`minishlab/potion-base-8M`, ~30 MB, 256-dim). "Static" means there's no
transformer forward pass and no ONNX runtime: text is tokenized, per-token
vectors are looked up in a distilled table and mean-pooled. It's tiny and
CPU-fast, so clustervision just embeds every session up front and does a
brute-force in-memory cosine scan at query time — no approximate-nearest-neighbor
index needed at the ~1k-session scale.

The model is fetched from the HuggingFace Hub on first use and cached under
`~/.cache/huggingface`. To pin a local copy and run fully offline, point
`CV_SEARCH_MODEL` at a model directory:

```sh
export CV_SEARCH_MODEL=/path/to/potion-base-8M
cv index --semantic
```

## Quick reference

| Command                          | What it does                                        | Limit flag (default) |
| -------------------------------- | --------------------------------------------------- | -------------------- |
| `cv index`                       | Build/refresh the full-text index                   | —                    |
| `cv index --semantic`            | Also build semantic embeddings                      | —                    |
| `cv search <query>`              | Full-text search (BM25)                             | `--limit` (20)       |
| `cv search <query> --semantic`   | Semantic search (no keyword fallback)               | `--limit` (20)       |
| `cv pack <task>`                 | Fused ranking → relevant spans, as a context bundle | `--limit` (8)        |
| `cv-search text <query>`         | Full-text search (standalone binary)                | `--limit` (10)       |
| `cv-search semantic <query>`     | Semantic search (standalone binary)                 | `-k` (10)            |
| `cv-search index` / `embed`      | Build full-text / embeddings (standalone)           | —                    |

See also: [the CLI](cli.md) for the full command surface, and
[MCP](mcp.md) for exposing search to a running agent.

[tantivy]: https://github.com/quickwit-oss/tantivy
[model2vec]: https://github.com/MinishLab/model2vec
