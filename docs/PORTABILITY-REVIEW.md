# Port-a-sesh correctness review — live round-trip (2026-05-31)

> **Historical.** The method still stands; the commands do not. `cv convert <id> --to <h>` became
> `cv port <id> --harness <h>` in 0.11.0, and the fidelity check it describes has been replaced by
> the per-field verifier behind `cv port --strict`. See [`INTERFACE-V2.md`](INTERFACE-V2.md).

Goal: verify that `cv convert` produces sessions that the **target harness's own binary**
can `--resume` and that **Basically Work** (the prior conversation is in the model's
context). Method: plant an unguessable passphrase ("VELVET-OTTER-7731") + a number (8842)
in a source session, convert, resume the target CLI headlessly, and ask it to recall them.
A correct port recalls; a broken one answers UNKNOWN.

Harnesses tested live (real installed binaries): **claude-code 2.1.158**, **codex-cli 0.131.0**,
**grok (Grok Build) 0.1.219**. Priority directed pairs: claude↔codex, grok↔codex.

## Result matrix (after fixes)

| Direction            | Discovery | Context recall | Notes |
|----------------------|-----------|----------------|-------|
| claude → codex       | ✅        | ✅ `CODE / NUMBER` | needed fix #1 |
| codex → claude       | ✅        | ✅ (transcript-confirmed) | symlink caveat below |
| codex → grok         | ✅        | ✅                 | needed fix #2 |
| grok → codex         | ✅        | ✅                 | relies on fix #1 |
| codex → grok (real, w/ reasoning) | ✅ | ✅       | needed fix #3 |
| claude → grok        | ✅        | ✅                 | confirms claude↔grok |
| claude → hermes      | ✅ (loads)| n/a (see #4/#5)    | needed fix #4; model caveat |
| hermes → codex       | ✅        | ✅                 | hermes parser is correct |

All four priority directed pairs **failed before** these fixes and **pass after**.
Second round added claude→grok, and the hermes pair (sqlite state.db).

## Bugs found & fixed (all live-validated)

### Fix #1 — codex emit dropped the model-visible conversation  *(crates/cv-core/src/emit.rs, emit_codex)*
Codex stores two parallel streams: `response_item` records (what's replayed into the model
on resume) and `event_msg` records (UI transcript only). Our emitter wrote **user and
assistant text only as `event_msg`** — so a resumed codex session reconstructed a context
with *zero* user prompts and *zero* assistant replies. Proven: pre-fix probe → `UNKNOWN`;
after emitting `response_item/message` (user→`input_text`, assistant→`output_text`) →
recalls the passphrase. The codex *parser* already dedups the dual representation
(`has_events`), so round-trip message counts are unaffected (18/18 emit tests still pass).

### Fix #2 — grok emit produced an undiscoverable session  *(emit_grok summary.json)*
`grok --resume` returned **"Session does not exist"** for our output. Root cause: our
`summary.json` omitted fields the loader requires (notably `chat_format_version`; real
summaries also carry `num_messages`, `next_trace_turn`, `grok_home`, `last_active_at`,
`agent_name`). Isolation test: a byte-complete real session relocated to a fresh id under a
new cwd resumed fine ("ALIVE"), so discovery is by-scan — the blocker was the malformed
summary. After writing the full field set, grok discovers and resumes. (Discovery is by
URL-encoded cwd dir; see caveat.)

### Fix #3 — cross-provider encrypted reasoning broke grok resume  *(emit_grok reasoning)*
Grok rejected ported history with *"Could not decrypt the provided encrypted_content … This
session's conversation history is incompatible with the current model."* OpenAI's
`encrypted_content` reasoning blobs are provider/account-bound and meaningless to xAI's
backend. Fix: only carry the `encrypted` blob for a **Grok→Grok** port (`session.harness ==
Harness::Grok`); otherwise emit the plain reasoning text and drop the blob. Also: default
`current_model_id` to `grok-build` unless the source model is itself a grok model (a foreign
model id would replay history against an incompatible backend). Validated on a **real** codex
session containing reasoning → grok resumes and quotes the first message verbatim, no error.

### Fix #4 — hermes emit couldn't write into an existing state.db  *(emit_hermes)*
`cv convert --to hermes` (no `--out`) failed with *"creating /Users/.../state.db: File exists"*
whenever hermes was actually installed. Cause: the hermes adapter's `storage_root()` returns the
**state.db file path**, but `emit_hermes` treated `out_dir` as a directory and called
`create_dir_all` on it. Fix: detect whether `out_dir` is the db file (is_file / `.db` ext) and use
it directly, creating only its parent. After this, claude/codex→hermes emit the session + messages
+ FTS rows correctly and hermes discovers and loads them (verified: a 4-message port appears in
`sessions`/`messages`, and hermes→codex round-trips the needle).

### Fix #5 — hermes resume hint printed the wrong flag  *(emit_hermes resume_hint)*
Hint said `hermes --session <id>`; the actual resume flag is `hermes --resume <id>`. Corrected.

## Caveats / not-yet-closed (honest list)

- **hermes resume needs a usable model.** Hermes reads the session row's `model` and sends it as
  `modelId`; a model-less source (no `session.model`) → empty `modelId` → API validation error, and
  hermes does NOT fall back to a default (tested both `''` and `NULL`). Real ports carry the source
  model (e.g. codex→hermes writes `gpt-5.2-codex`), so this only bites model-less sources — but that
  ported model must be one the user's hermes is authed/configured for, or they must pass `-m`. Worth
  emitting a default or warning when `session.model` is None.

- **Symlink/realpath jail (claude target). FIXED.** Claude resolves cwd via realpath; macOS `/tmp`
  → `/private/tmp`. Porting to a symlinked path used to write `~/.claude/projects/-tmp-…` while
  claude looks under `-private-tmp-…` → "No conversation found". `emit_claude` now canonicalizes the
  effective cwd (`realpath_for_claude`, falling back to the raw path when it can't be resolved) before
  deriving the project-dir name, so the dir matches the realpath Claude discovers. Verified: porting
  to `/tmp/cv-portcheck` writes `-private-tmp-cv-portcheck`. Covers `/var`, `/home` symlinks too.
- **Not tested live:** claude↔grok direct, all hermes pairs, and full real claude→codex
  sessions containing tool calls (only synthetic text + one real reasoning session covered).
  emit_codex already routes tool_use→`function_call` / tool_result→`function_call_output` as
  response_items, so those *should* be fine, but it's unverified end-to-end.
- **Claude usage limit** was hit mid-review; codex→claude recall was confirmed from the
  appended transcript rather than a fresh probe.

## Test methodology (reusable)
1. Synthetic source with planted needle (or a real session with a known first message).
2. `cv convert <id> --from X --to Y --cwd <non-symlink sandbox>`.
3. `codex exec --sandbox read-only resume <id> "<probe>"` / `grok -p "<probe>" -r <id>` /
   `claude -p "<probe>" --resume <id>` — run from the sandbox cwd.
4. Compare recall vs. a no-resume control.

---

# Round 2 — source-backed harnesses (opencode / kimi / gemini / openclaw)

We have full source for these in `~/pug`, so each was audited against its real loader code
AND live-tested. Pattern that recurred: **the transcript bytes are usually right; the
address (where/how the file is registered) is wrong.**

| Direction       | Status | Notes |
|-----------------|--------|-------|
| claude → kimi   | ✅ FIXED + live | recalled needle after fix (committed 00ff132) |
| hermes → codex  | ✅ (round 1) | |
| claude → gemini | ⚠️ shape-correct, wrong dir | recall works once placed in `tmp/<slug>/chats/` |
| claude → opencode | ❌ broken | binary moved to SQLite; legacy JSON ignored |
| (openclaw)      | ⚠️ loads, lossy | source-verified; resume hint wrong |

### Kimi — FIXED (commit 00ff132, `harness/kimi.rs`)
Two placement bugs, both live-reproduced then fixed: (1) `storage_root()` returned
`~/.kimi/sessions` while `emit()` re-joined `sessions/…` → doubled path `…/sessions/sessions/…`
that kimi-cli never scans; (2) the cwd was never registered in `~/.kimi/kimi.json` `work_dirs[]`,
so `Session.find` returned None and resume silently started an empty session. After fixing both,
the real `kimi --resume` recalled the planted passphrase. Transcript schema was already correct.

### OpenCode — BROKEN on current installs (spawned as a follow-up task)
The installed opencode binary migrated to **SQLite** (`~/.local/share/opencode/opencode.db`) and
no longer reads the file-based `storage/{session,message,part}/*.json` layout our `emit_opencode`
writes (JSON→DB migration is one-shot, gated on the db not existing). Live: `opencode run --session
<id>` → "Error: Session not found"; the row is absent from `opencode.db`. Fix options: emit into
`opencode.db` (needs a `project` row keyed by the worktree's first-commit git hash, not our
`prj_<fnv>` hash), or gate `cv` to refuse opencode emit until then. `can_emit()` is already `false`
in the adapter. (emit.rs-resident — held; see below.)

### Gemini — shape correct, output dir wrong (emit.rs-resident — held)
`emit_gemini` writes the right legacy whole-file JSON, but to `~/.gemini/tmp/chats/` instead of
`~/.gemini/tmp/<projectIdentifier>/chats/`. The installed gemini resumes only via `--resume`
scoped to the project-slug dir (the `--session-file` flag the source suggests is NOT in this
build). The slug is the cwd basename, registered in `~/.gemini/projects.json`. Proven: copying the
emitted file into `~/.gemini/tmp/proj/chats/` made `gemini --resume <uuid>` recall the needle. Fix:
write into `tmp/<slug>/chats/` and register the cwd→slug in `projects.json` (the kimi pattern).

### OpenClaw — loads but lossy (emit.rs-resident — held)
Source-verified (no real on-disk data to live-test): the threaded transcript loads and replays
user/assistant text, BUT (1) the resume hint `openclaw --session <id>` is wrong — the headless
command is `openclaw agent --session-id <id> --message "…" --local`; (2) IR System turns are emitted
as `role:"system"`, which `buildSessionContext` silently drops (it only handles user/assistant/
toolResult/custom) → context loss for sessions with system/compaction/custom turns; (3) assistant
messages omit `provider`/`api`/`stopReason`/`usage` (degraded model resolution); (4) tool results can
carry an empty `toolCallId` → orphan results some providers reject.

## Holds (shared-file coordination)
The gemini, openclaw, and opencode-gate fixes all live in `emit.rs`, which currently carries the
sibling `realpath_for_claude` work uncommitted in the working tree. Held to avoid committing that
work from under it. Kimi shipped because it lives in its own file (`harness/kimi.rs`).
