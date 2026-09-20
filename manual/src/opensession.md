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
  "open_session": "0.3",
  "id": "…",
  "harness": "claude",            // origin hint, not identity
  "title": "…",
  "cwd": "/Users/you/project",    // metadata, NOT identity (see below)
  "model": "claude-opus-4-…",     // session default; a message names one only when it differs
  "system_prompt": "…",           // what the harness actually sent, when it stores it
  "lineage": { "forked_from": "…", "parent": "…", "continued_in": "…" },
  "messages": [
    {
      "role": "user",            // WHO spoke: user · assistant · system · tool
      "kind": "prompt",          // WHAT the turn is: prompt · reply · tool_result ·
                                 //   injected_context · notice · compaction_boundary · error · …
      "origin": "human",         // WHERE it came from: human · model · harness · hook ·
                                 //   scheduler · subagent · import
      "timestamp": "2026-…Z",
      "content": [               // an ordered list of typed blocks
        { "type": "text", "text": "…" },
        { "type": "thinking", "text": "…", "signature": "…", "redacted": false },
        { "type": "tool_use", "id": "…", "name": "run_shell", "input": { }, "namespace": "…" },
        { "type": "tool_result", "tool_use_id": "…", "content": "…", "is_error": false },
        { "type": "file", "mime": "…", "path": "…" },
        { "type": "image", "media_type": "image/png", "data_ref": "…" }
      ],
      "extra": { "claude": { } } // namespaced passthrough: one bag per harness
    }
  ]
}
```

> **0.3 is the IR.** Until 0.11.0 this page showed a camelCase document with `kind`-tagged blocks
> while cv's own output was snake_case with `type`-tagged ones, and the spec claimed the IR was its
> reference implementation anyway — so the two described different formats while asserting they
> were one. 0.3 resolves it in the direction that leaves one vocabulary: the spec adopts the IR.
> `cv export --format json` emits a valid OpenSession 0.3 document, and what
> [`cv schema --json`](cli.md#cv-schema) publishes is the same shape. A reader can still accept the
> 0.2 spellings, and cv's does.

## The one heresy: *cwd is metadata, not identity*

Most harnesses key a session to the exact directory it ran in, so moving a project loses the thread.
OpenSession records `cwd` as a plain field. A session is its **messages** — it can be read, ported,
and resumed anywhere. This is what makes [porting](cli.md) and [cross-harness conversion](conversion.md)
possible at all.

## Why you'd care

If you ship a harness: emit OpenSession and your users' sessions become portable *by construction* —
every other tool can read, search, and convert them. 🤝
