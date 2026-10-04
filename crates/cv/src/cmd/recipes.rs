//! `cv recipes` — the agent quickstart: the ten things agents do with cv, each as one command line
//! plus the JSON keys it returns. Printed verbatim; `cv --help` points here.

use anyhow::Result;

pub(crate) const RECIPES: &str = "\
cv recipes — the agent quickstart
=================================

Ids: every command takes `harness:id` or a unique id prefix. An ambiguous prefix lists the
candidates as `harness:full-id` lines and exits 2. `--json` is machine output (snake_case keys,
RFC 3339 timestamps); `cv schema --json` publishes every shape below.

Windows (show, export): --first N · --last N · --range A..B (0-based, end-exclusive; A.. / ..B)
· --around N [--context K] · --max-bytes N (prints `… continue with --range <next>..`).
One selector at a time. Piped `cv show` with no selector over 200 KB prints the first and last
20 messages with a hint between them — pick a window instead of paging the whole thing.

 1. Find my own session by cwd
    cv ls --cwd \"$PWD\" --json --limit 5
    → [{id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes}]
      (newest first; add --enrich for display_title + git)

 2. Read the last N turns of a session
    cv show <id> --last 40                       # rendered transcript
    cv show <id> --json --last 40                # the IR
    → {id, harness, cwd, title, model, system_prompt, lineage, messages: [{role, kind, origin,
       timestamp, content: [{type: text|thinking|tool_use|tool_result|image|file, …}], usage}]}

 3. Read a sub-agent's return
    cv show <id> --subagents --json
    → [{agent_id, agent_type, description, tool_use_id, workflow, result_status, result_summary,
        return, session: {id, harness, path, …}}]
    cv show <agent-id>                           # the sub-agent's own transcript (parent resolved fleet-wide)

 4. Fleet search
    cv search \"<words>\" --json --limit 10   # add --semantic for meaning, --harness <h> to narrow
    → [{id, harness, path, cwd, title, created_at, updated_at, message_count, size_bytes,
        score, snippet, agent_id, parent_id, workflow}]   (run `cv index` once for instant hits)

 5. Get a workflow lane's prompt (and what it did)
    cv workflow <session> <run|name> --json
    → {run_id, name, status, summary, agent_count, total_tokens, phases: [{index, title,
       agents: [{index, label, agent_id, state, model, tokens, tool_calls, error, …}]}]}
    cv workflow <session> <run> --revive         # every unfinished lane's FULL prompt + what it landed
    cv workflow <session> <run> --results        # every lane's full journaled return

 6. Doctor a session (why does the context keep filling?)
    cv doctor <id> --json
    → {sessions, total, tool_results, tool_results_mcp, tool_results_builtin, by_tool: [{tool, calls,
       result_tokens, mcp}], thinking, messages, system_reminders, by_attachment, fixed_overhead_est,
       compactions, auto_compactions, peak_context, …}

 7. Prune and resume a maxed-out session
    cv prune <id> --drop-thinking --json         # snips old payloads + thinking; revives the resume gate
    → {source_id, new_id, harness, before_bytes, after_bytes, snipped_payloads, image_blocks,
       tokens_freed, dropped_turns, window_real_tokens, revived, warnings, new_path, sidecar_path,
       copied_resources, dry_run, note}
    cv resume <new_id>                           # prints `claude --resume <new_id>` (add --launch to run it)

 8. Port a session to another harness or directory
    cv port <id> --harness codex                 # same cwd, runs in Codex
    cv port <id> --cwd ~/new/home                # same harness, new home (carries CLAUDE.md/AGENTS.md)
    cv port <id> --harness codex --out /tmp/dry  # dry run: write under --out, not the real store
    cv port <id> --harness codex --strict        # fail if a loss the target COULD have carried happens
    cv port <id> --harness codex --thinking text # keep every turn: reasoning the target can't hold
                                                 # becomes text (a placeholder for signed blobs)
    → prints `✦ wrote <path> (<new_id>)` and the resume incantation; `⚠ lossy:` lines name what
      the target format cannot carry

 9. Fetch one tool call's full output
    cv cat <session> <tool_use_id>               # inline, prune sidecar, or persisted-output file
    cv cat <session> <tool_use_id> --input       # the call's arguments as JSON
    → raw text on stdout (exit 2 if the id is unknown); ids come from `cv show`/`cv tools --timeline`

10. List what a session touched
    cv events <id> --json                        # add --subagents for the whole forest
    → [{msg_idx, ts, kind: file_edit|file_read|command|tool|error, tool, target, detail,
        agent_id, parent_id, workflow}]
    cv touched <path> --json                     # every session that read/edited a file
    → [{harness, session_id, title, edits, reads, last_ts, agent_id, parent_id, workflow}]

11. Re-read what the person actually said (and answered)
    cv prompts <id>                              # every human prompt + AskUserQuestion answer, with msg indices
    cv prompts <id> --pre-compaction --json      # only the span a compaction discarded
    → [{index, timestamp, kind: prompt|answer, text}]

12. Where is every lane right now?
    cv lanes <id>                                # one row per sub-agent: status, model, duration, tokens, last line
    cv lanes <id> --stranded                     # lanes that stopped saying \"waiting on …\" — each with its resume hint
    cv lanes <id> --running --json
    → [{agent_id, description, model, started_at, duration_ms, tokens: {total, …}, tool_calls, status,
        status_source, last_text, last_tool, stranded}]

13. What did I promise to do later, and does a task hold it?
    cv deferrals <id> --open-tasks               # exit 1 while any deferral is UNMATCHED — gate a closeout on it
    cv deferrals <id> --since <msg> --json
    → [{index, timestamp, phrase, context, matched?: {task_id, title, state, shared}}]

14. Hand a lane's context on without its transcript (distill)
    cv distill <agent-id> --pack lane.md         # brief + messages + own words + facts + ledger + last 12 calls
    cv distill <agent-id> --agent-of <root-id>   # a new sub-agent of <root>: SendMessage to the printed id resumes it
    cv distill <agent-id> --session              # a new session: claude -p --resume <new-id> \"<next quest>\"
    cv distill <agent-id> --with <other-agent>   # inject another lane's findings into this pack
    cv fork <id> --at N --agent-of <root-id>     # branch a lane's verbatim context at message N
    → stderr: context before → after; --json: {stats: {recorded_context_tokens, distilled_est_tokens, …},
      emitted: {session_id, agent_id, path, sidecar_path, resume}}; elided outputs: cv cat <new> <tool_use_id>

More: `cv schema` (the -q query calculus), `cv schema --commands --json` (every command + flag),
`cv <command> --help`.
";

pub(crate) fn cmd_recipes() -> Result<()> {
    print!("{RECIPES}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ten_recipes_and_no_old_names() {
        let r = super::RECIPES;
        for n in 1..=14 {
            assert!(r.contains(&format!("{n:>2}. ")), "recipe {n} missing");
        }
        for old in ["cv convert", "cv query", "cv recall", "--retrieve", "--to-dir", "--to "] {
            assert!(!r.contains(old), "recipes mention the removed {old:?}");
        }
    }
}
