// A small embedded sample dataset matching the cv-core IR serde shape (IR v2, cv 0.11+).
// Used so the UI is demoable without uploading a .zip (and as the fallback when the wasm module
// isn't built/present) — and as the corpus `selftest.html` asserts the renderers against, so it
// deliberately covers the whole vocabulary rather than just the happy path.
//
// Session JSON shape (serde of the Rust IR, snake_case):
//   { id, harness, cwd?, title?, created_at?, updated_at?, model?, git?, system_prompt?,
//     lineage?, messages: [Message], source_path?, extra? }
//   Message: { id?, parent_id?, role, kind, origin, timestamp?, model?, content: [Block],
//              usage?, extra? }   -- `extra` is NESTED BY HARNESS: extra.claude.foo
//   Block is tagged by `type`: text | thinking | tool_use | tool_result | image | file
//
// The last session is deliberately in the OpenSession interchange shape (camelCase, blocks tagged
// `kind`) to keep the normalizer's tolerance honest.

export const SAMPLE_SESSIONS = [
  {
    id: "c0ffee-claude-001",
    harness: "claude",
    cwd: "/Users/ember/pug/clustervision",
    title: "Wire up the WASM ingest contract",
    created_at: "2026-05-28T17:12:04Z",
    updated_at: "2026-05-28T17:41:55Z",
    model: "claude-opus-4-8",
    git: { branch: "dev", commit: "a1b2c3d", remote: "git@github.com:ember/clustervision.git" },
    source_path: ".claude/projects/-Users-ember-pug-clustervision/c0ffee.jsonl",
    system_prompt: "You are an interactive agent that helps users with software engineering tasks.\n\nTone: concise, direct. Do not narrate.",
    lineage: { continued_in: "c0ffee-claude-002" },
    messages: [
      {
        id: "m1",
        role: "user",
        kind: "prompt",
        origin: "human",
        timestamp: "2026-05-28T17:12:04Z",
        content: [
          { type: "text", text: "Can you read the IR and tell me what `ingest_zip` should return? I want the web UI to match exactly." }
        ]
      },
      {
        id: "m1r",
        parent_id: "m1",
        role: "system",
        kind: "injected_context",
        origin: "harness",
        timestamp: "2026-05-28T17:12:04Z",
        extra: { claude: { attachment_type: "system_reminder" } },
        content: [
          { type: "text", text: "<system-reminder>\nThe user's CLAUDE.md asks you to prefer reading the contract over guessing.\n</system-reminder>" }
        ]
      },
      {
        id: "m2",
        parent_id: "m1",
        role: "assistant",
        kind: "reply",
        origin: "model",
        timestamp: "2026-05-28T17:12:30Z",
        model: "claude-opus-4-8",
        usage: { input_tokens: 1840, output_tokens: 320, cache_read_tokens: 12000, reasoning_tokens: 210 },
        content: [
          { type: "thinking", text: "The contract says ingest_zip returns a JSON string that is an array of Session objects. Let me confirm the Block tag names from ir.rs: text, thinking, tool_use, tool_result, image, file. A block is tagged `type`; a message has its own `kind`." },
          { type: "text", text: "`ingest_zip(bytes)` returns a JSON **string** — an array of `Session` objects. Each block is tagged by `type`. Let me grep the source to be sure of the field names." },
          { type: "tool_use", id: "t1", name: "Grep", input: { pattern: "tag = \"type\"", path: "crates/cv-core/src/ir.rs" } }
        ]
      },
      {
        id: "m3",
        parent_id: "m2",
        role: "tool",
        kind: "tool_result",
        origin: "harness",
        timestamp: "2026-05-28T17:12:31Z",
        content: [
          {
            type: "tool_result", tool_use_id: "t1", tool_name: "Grep", status: "ok", is_error: false,
            content: "crates/cv-core/src/ir.rs:206:#[serde(tag = \"type\", rename_all = \"snake_case\")]\npub enum Block {\n    Text { text: String },\n    Thinking { text, signature?, encrypted?, redacted? },\n    ToolUse { id, name, input, namespace? },\n    ToolResult { tool_use_id, content, is_error, tool_name?, status?, details? },\n    Image { media_type?, data_ref? },\n    File { mime?, path?, source? },\n}",
            details: { matches: 1, truncated: false, searched_files: 1 }
          }
        ]
      },
      {
        id: "m4",
        parent_id: "m3",
        role: "assistant",
        kind: "reply",
        origin: "model",
        timestamp: "2026-05-28T17:13:10Z",
        model: "claude-opus-4-8",
        usage: { input_tokens: 2100, output_tokens: 410 },
        content: [
          { type: "text", text: "Confirmed. A **block** is tagged `type` (`text`, `thinking`, `tool_use`, `tool_result`, `image`, `file`); a **message** carries its own `kind` and `origin`. Roles and harness names are lowercase. Render `tool_use.input` as pretty JSON, and `tool_result.content` as a preformatted block with an error style when `is_error` is true." }
        ]
      },
      {
        id: "m5",
        parent_id: "m4",
        role: "system",
        kind: "notice",
        origin: "harness",
        timestamp: "2026-05-28T17:20:00Z",
        content: [{ type: "text", text: "/compact — compacting the conversation" }]
      },
      {
        id: "m6",
        parent_id: "m5",
        role: "system",
        kind: "compaction_boundary",
        origin: "harness",
        timestamp: "2026-05-28T17:20:02Z",
        extra: { claude: { subtype: "compact_boundary", compactMetadata: { trigger: "manual", preTokens: 986745, postTokens: 6996 } } },
        content: [{ type: "text", text: "Conversation compacted" }]
      },
      {
        id: "m7",
        parent_id: "m6",
        role: "user",
        kind: "compaction_summary",
        origin: "harness",
        timestamp: "2026-05-28T17:20:03Z",
        content: [{ type: "text", text: "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion.\n\n1. We established the wasm ingest contract.\n2. We confirmed block tags against `ir.rs`." }]
      },
      {
        id: "m8",
        parent_id: "m7",
        role: "system",
        kind: "error",
        origin: "harness",
        timestamp: "2026-05-28T17:30:00Z",
        extra: { claude: { error: "api_error" } },
        content: [{ type: "text", text: "API Error: 500 — upstream connect error. Retrying." }]
      },
      {
        id: "m9",
        parent_id: "m8",
        role: "user",
        kind: "prompt",
        origin: "scheduler",
        timestamp: "2026-05-28T17:35:00Z",
        content: [{ type: "text", text: "[loop] continue where you left off" }]
      },
      {
        // Deliberately a message kind AND a block type this build has never heard of — the UI
        // must say so rather than render an empty turn. selftest.html asserts exactly this.
        id: "m10",
        parent_id: "m9",
        role: "assistant",
        kind: "telepathy",
        origin: "martian",
        timestamp: "2026-05-28T17:41:55Z",
        content: [{ type: "hologram", payload: { frames: 24 } }]
      }
    ]
  },
  {
    id: "feed-codex-7",
    harness: "codex",
    cwd: "/Users/ember/work/payments-svc",
    title: "Fix the off-by-one in the retry backoff",
    created_at: "2026-05-27T09:02:00Z",
    updated_at: "2026-05-27T09:20:18Z",
    model: "gpt-5-codex",
    git: { branch: "main", commit: "9988776" },
    source_path: ".codex/sessions/2026/05/27/feed-codex-7.jsonl",
    lineage: { forked_from: "feed-codex-6", parent: "feed-codex-root", agent_path: "reviewer" },
    messages: [
      {
        role: "system",
        kind: "system_prompt",
        origin: "harness",
        content: [{ type: "text", text: "You are a coding agent operating in the payments-svc repository. Be terse." }]
      },
      {
        role: "user",
        kind: "prompt",
        origin: "human",
        timestamp: "2026-05-27T09:02:00Z",
        content: [{ type: "text", text: "The retry loop fires one extra time past max_attempts. Find and fix it." }]
      },
      {
        role: "system",
        kind: "injected_context",
        origin: "harness",
        timestamp: "2026-05-27T09:02:01Z",
        extra: { codex: { record_type: "environment_context" } },
        content: [{ type: "text", text: "<environment_context>\n  <cwd>/Users/ember/work/payments-svc</cwd>\n  <approval_policy>on-request</approval_policy>\n</environment_context>" }]
      },
      {
        role: "assistant",
        kind: "reply",
        origin: "model",
        timestamp: "2026-05-27T09:03:40Z",
        model: "gpt-5-codex",
        usage: { input_tokens: 5200, output_tokens: 180, reasoning_tokens: 1408, cost_usd: 0.0214 },
        content: [
          { type: "thinking", text: "Classic `<=` vs `<`. Let me search the retry module." },
          { type: "tool_use", id: "s1", name: "shell", input: { command: ["bash", "-lc", "grep -rn 'attempt' src/retry.rs"] } }
        ]
      },
      {
        role: "tool",
        kind: "tool_result",
        origin: "harness",
        timestamp: "2026-05-27T09:03:41Z",
        content: [{ type: "tool_result", tool_use_id: "s1", tool_name: "shell", is_error: false,
          content: "src/retry.rs:42:        while attempt <= max_attempts {",
          details: { exit_code: 0, duration_ms: 34 } }]
      },
      {
        role: "system",
        kind: "model_change",
        origin: "harness",
        timestamp: "2026-05-27T09:04:00Z",
        model: "gpt-5-codex-high",
        extra: { codex: { codex_event: "thread_settings_applied" } },
        content: [{ type: "text", text: "[effort changed: medium → xhigh]" }]
      },
      {
        role: "assistant",
        kind: "reply",
        origin: "model",
        timestamp: "2026-05-27T09:04:20Z",
        model: "gpt-5-codex-high",
        usage: { input_tokens: 5600, output_tokens: 240, reasoning_tokens: 900, cost_usd: 0.0331 },
        content: [
          { type: "text", text: "Found it. `while attempt <= max_attempts` should be `<`. With a 1-based `attempt` starting at 1, `<=` runs `max_attempts + 1` times. Patching now." },
          { type: "tool_use", id: "s2", name: "apply_patch", namespace: "collaboration", input: { patch: "*** Update File: src/retry.rs\n@@\n-        while attempt <= max_attempts {\n+        while attempt < max_attempts {" } }
        ]
      },
      {
        role: "tool",
        kind: "tool_result",
        origin: "harness",
        timestamp: "2026-05-27T09:04:21Z",
        content: [{ type: "tool_result", tool_use_id: "s2", tool_name: "apply_patch", status: "ok", is_error: false, content: "Applied patch to src/retry.rs" }]
      },
      {
        role: "system",
        kind: "compaction_boundary",
        origin: "harness",
        timestamp: "2026-05-27T09:10:00Z",
        extra: { codex: { codex_event: "compacted", replacement_history_len: 5, window_number: 1 } },
        content: [{ type: "text", text: "[history compacted]" }]
      },
      {
        role: "tool",
        kind: "tool_result",
        origin: "subagent",
        timestamp: "2026-05-27T09:12:00Z",
        content: [{ type: "tool_result", tool_use_id: "s3", tool_name: "collaboration:review", is_error: true, status: "error",
          content: "reviewer: the test `retry_stops_at_max` still asserts the old behaviour",
          details: { exit_code: 1, duration_ms: 4120 } }]
      }
    ]
  },
  {
    id: "grok-img-3",
    harness: "grok",
    cwd: "/tmp/scratch",
    title: "Look at this screenshot",
    updated_at: "2026-05-20T14:00:00Z",
    messages: [
      {
        role: "user",
        kind: "prompt",
        origin: "human",
        content: [
          { type: "text", text: "What's wrong with this layout?" },
          { type: "image", media_type: "image/png", data_ref: "attachments/screenshot-1.png" }
        ]
      },
      {
        role: "assistant",
        kind: "reply",
        origin: "model",
        content: [
          { type: "thinking", encrypted: "AQEBxR3k…opaque…", text: "" },
          { type: "text", text: "The sidebar is overflowing its container — the flex child needs `min-width: 0` so it can shrink below its content size." }
        ]
      }
    ]
  },
  {
    id: "claude-loom-branch-a",
    harness: "claude",
    cwd: "/Users/ember/pug/clustervision",
    title: "Wire up the WASM ingest contract (branch A)",
    created_at: "2026-05-28T17:12:04Z",
    updated_at: "2026-05-28T17:30:00Z",
    model: "claude-opus-4-8",
    git: { branch: "loom-a", commit: "a1b2c3d" },
    lineage: { forked_from: "c0ffee-claude-001" },
    messages: [
      { id: "m1", role: "user", kind: "prompt", origin: "human", timestamp: "2026-05-28T17:12:04Z",
        content: [{ type: "text", text: "Can you read the IR and tell me what `ingest_zip` should return? I want the web UI to match exactly." }] },
      { id: "mb2", parent_id: "m1", role: "assistant", kind: "reply", origin: "model", timestamp: "2026-05-28T17:20:00Z", model: "claude-opus-4-8",
        content: [
          { type: "text", text: "Branch A: let's just trust the docs — it returns a JSON string of Session[]. Shipping." },
          { type: "file", mime: "text/x-rust", path: "crates/cv-web/src/lib.rs", source: "diff" }
        ] }
    ]
  },
  {
    // An OpenSession-format (camelCase, blocks tagged `kind`) session, to exercise the JSON loader
    // / normalizer. Dropping a .json shaped like this loads directly — no wasm. None of these
    // messages declare a `kind`, so the normalizer's role defaults have to carry them.
    openSession: "0.2",
    harness: "openSession",
    id: "open-demo-42",
    cwd: "/Users/ember/lab/opensession",
    title: "OpenSession round-trip demo",
    model: "gpt-5-codex",
    createdAt: "2026-05-29T10:00:00Z",
    updatedAt: "2026-05-29T10:08:00Z",
    git: { branch: "main", commit: "deadbeef", remote: "git@github.com:ember/open.git" },
    messages: [
      { id: "o1", role: "user", timestamp: "2026-05-29T10:00:00Z",
        content: [{ kind: "text", text: "Prove the camelCase OpenSession shape loads and renders." }] },
      { id: "o2", parentId: "o1", role: "assistant", timestamp: "2026-05-29T10:01:00Z", model: "gpt-5-codex",
        usage: { inputTokens: 900, outputTokens: 120, cacheReadTokens: 4000, reasoningTokens: 64, costUsd: 0.0071 },
        content: [
          { kind: "thinking", redacted: true },
          { kind: "text", text: "Here's a tool call using the OpenSession `toolUse` kind:" },
          { kind: "toolUse", id: "c1", name: "Bash", input: { command: "cargo test --workspace" } }
        ] },
      { id: "o3", parentId: "o2", role: "tool", timestamp: "2026-05-29T10:02:00Z",
        content: [{ kind: "toolResult", toolUseId: "c1", toolName: "Bash", status: "ok", isError: false,
          content: "running 128 tests\n…\ntest result: ok. 128 passed; 0 failed",
          details: { exitCode: 0, durationMs: 8421 } }] }
    ]
  }
];

export default SAMPLE_SESSIONS;
