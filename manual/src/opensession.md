# The OpenSession standard

After reverse-engineering seventeen different transcript formats, one thing was obvious: they're all
*almost the same thing* underneath. **OpenSession** is the format they should have agreed on — a
small, honest, harness-neutral interchange format, written down as a proposal for harness authors.
clustervision's internal [IR](architecture.md#the-unified-ir) is its reference implementation: the
same spine, the same blocks, the same heresy about `cwd`.

The full spec lives at **[docs/OPENSESSION.md](https://github.com/emberian/cv/blob/main/docs/OPENSESSION.md)**. The essentials:

## The shape

```jsonc
{
  "openSession": "0.2",
  "id": "…",
  "harness": "claude",            // origin hint, not identity
  "title": "…",
  "cwd": "/Users/you/project",    // metadata, NOT identity (see below)
  "model": "claude-opus-4-…",
  "messages": [
    {
      "role": "user",            // user · assistant · system · tool
      "timestamp": "2026-…Z",
      "content": [               // an ordered list of typed blocks
        { "kind": "text", "text": "…" },
        { "kind": "thinking", "text": "…", "signature": "…", "redacted": false },
        { "kind": "toolUse", "id": "…", "name": "run_shell", "input": { … } },
        { "kind": "toolResult", "toolUseId": "…", "content": "…", "isError": false },
        { "kind": "file", "mime": "…", "path": "…" },
        { "kind": "image", "mediaType": "image/png", "dataRef": "…" }
      ]
    }
  ]
}
```

> **Spelling note.** The OpenSession document above is camelCase, as published. cv's *own*
> machine output is not: since 0.11.0 every `cv --json` payload and every MCP payload is
> **snake_case** (`tool_use_id`, `media_type`, `message_count`), a message carries `kind` and
> `origin`, and a block is tagged `type` rather than `kind`. If you are consuming `cv`, follow
> [`cv schema --json`](cli.md#cv-schema), not this page.

## The one heresy: *cwd is metadata, not identity*

Most harnesses key a session to the exact directory it ran in, so moving a project loses the thread.
OpenSession records `cwd` as a plain field. A session is its **messages** — it can be read, ported,
and resumed anywhere. This is what makes [porting](cli.md) and [cross-harness conversion](conversion.md)
possible at all.

## Why you'd care

If you ship a harness: emit OpenSession and your users' sessions become portable *by construction* —
every other tool can read, search, and convert them. 🤝
