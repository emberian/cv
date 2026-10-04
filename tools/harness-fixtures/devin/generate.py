#!/usr/bin/env python3
# generate.py — write crates/cv-core/tests/fixtures/devin/sessions-v17-3000.11.3.db
#
# A REDACTED, synthetic Devin CLI store: schema 17 exactly as the live `sessions.db` spells it
# (verified against the real DB's sqlite_master on 2026-10-04), populated with made-up sessions
# that mirror the live shapes — a message_nodes forest with a superseded duplicate chain, a
# 7-node is_system_prefix run, thinking + parallel exec tool calls, a failed tool result, and a
# session with model=''/main_chain_id=NULL/hidden=1. Nothing here came out of a real transcript;
# all message texts are placeholders.
#
# Usage: python3 generate.py [--out PATH]

import argparse
import json
import os
import sqlite3
import sys

MIGRATIONS = [
    "initial_schema", "add_thinking_column", "add_prompt_history", "add_metadata_column",
    "message_forest", "add_node_metadata", "add_shell_context", "add_session_cogs",
    "add_rendered_commits", "add_workspace_dirs", "add_prompt_history_is_shell", "add_app_state",
    "rename_permission_mode_to_agent_mode", "tool_call_state", "add_hidden_column",
    "add_session_json_metadata", "subagent_heads",
]

SCHEMA = """
CREATE TABLE refinery_schema_history(
             version int4 PRIMARY KEY,
             name VARCHAR(255),
             applied_on VARCHAR(255),
             checksum VARCHAR(255));
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  working_directory TEXT NOT NULL,
  backend_type TEXT NOT NULL,
  model TEXT NOT NULL,
  agent_mode TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL,
  title TEXT, main_chain_id INTEGER, shell_last_seen_index INTEGER DEFAULT 0, cogs_json TEXT,
  workspace_dirs TEXT, hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT);
CREATE TABLE prompt_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  content TEXT NOT NULL,
  timestamp INTEGER NOT NULL,
  session_id TEXT NOT NULL,
  is_shell INTEGER NOT NULL DEFAULT 0);
CREATE TABLE message_nodes (
  row_id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  node_id INTEGER NOT NULL,
  parent_node_id INTEGER,
  chat_message TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  metadata TEXT,
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE(session_id, node_id));
CREATE TABLE rendered_commits (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  sequence_number INTEGER NOT NULL,
  rendered_html TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE(session_id, sequence_number));
CREATE TABLE app_state (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
CREATE TABLE tool_call_state (
    session_id    TEXT    NOT NULL,
    tool_call_id  TEXT    NOT NULL,
    tool_call_json     TEXT,
    tool_call_update_json TEXT,
    PRIMARY KEY (session_id, tool_call_id),
    FOREIGN KEY (session_id) REFERENCES sessions(id));
CREATE TABLE subagent_heads (
    session_id    TEXT    NOT NULL,
    agent_id      TEXT    NOT NULL,
    chain_node_id INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (session_id, agent_id),
    FOREIGN KEY (session_id) REFERENCES sessions(id));
"""

T0 = 1_759_600_000  # 2026-10-04-ish, arbitrary but modern


def msg(message_id, role, content, **kw):
    m = {"message_id": message_id, "role": role, "content": content}
    m.update(kw)
    return m


def node_meta(prior=None, prefix=None, summarized_from=None, ntp=None):
    d = {"summarized_from": summarized_from, "num_tokens_preceding": ntp,
         "is_system_prefix": prefix}
    if prior is not None:
        d["extensions"] = {"compact/prior_node_ids": prior}
    return json.dumps(d)


def insert_node(c, sid, nid, parent, chat, meta=None, ts=None):
    c.execute(
        "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message,"
        " created_at, metadata) VALUES (?,?,?,?,?,?)",
        (sid, nid, parent, chat if isinstance(chat, str) else json.dumps(chat),
         ts or (T0 + nid), meta if meta is not None else "null"),
    )


def assistant_meta(created, finish, model="swe-2-high", metrics=True, tool_call_content=None):
    m = {
        "num_tokens": 42 if metrics else None,
        "is_user_input": None,
        "request_id": "req-fixture-1",
        "metrics": {
            "ttft_ms": 100, "total_time_ms": 500, "input_tokens": 100, "output_tokens": 42,
            "cache_read_tokens": 200, "cache_creation_tokens": None, "tpot_ms": 1.0,
            "tokens_per_sec": 84.0,
        } if metrics else None,
        "finish_reason": finish,
        "extensions": {},
        "response_dimensions": [],
        "started_generation_at": created,
        "created_at": created,
        "generation_model": model,
        "telemetry": {"source": "assistant", "operation": "inference"},
    }
    if tool_call_content:
        m["extensions"]["chisel/tool_call_content"] = tool_call_content
    return m


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=os.path.join(
        os.path.dirname(__file__), "../../../crates/cv-core/tests/fixtures/devin/sessions-v17-3000.11.3.db"))
    args = ap.parse_args()
    out = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    if os.path.exists(out):
        os.remove(out)
    c = sqlite3.connect(out)
    c.execute("PRAGMA page_size = 1024")  # keep the fixture small (<64 KB)
    c.executescript(SCHEMA)
    for i, name in enumerate(MIGRATIONS, start=1):
        c.execute(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) VALUES (?,?,?,?)",
            (i, name, "2026-01-01T00:00:00Z", str(i)))

    # ── session A: fixture-alpha ────────────────────────────────────────────
    sid = "fixture-alpha"
    c.execute(
        "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,"
        " created_at, last_activity_at, title, main_chain_id, shell_last_seen_index, cogs_json,"
        " workspace_dirs, hidden, metadata) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        (sid, "/work/alpha", "windsurf", "fusion-test-model", "bypass",
         T0, T0 + 100, "Fixture session alpha", 16, 0, None, "[]", 0, None))

    # Abandoned pre-prefix chain (nodes 0-2): the live store's first root before a context
    # rebuild. Node metadata is the literal string 'null' on these.
    sys_info = msg("m-sys-info", "system", "<system_info>fixture environment info</system_info>")
    rules = msg("m-rules", "system", '<rules type="always-on">fixture rules</rules>')
    prompt = msg("m-prompt", "user", "hello from the fixture",
                 metadata={"is_user_input": True, "created_at": "2026-10-04T00:00:02Z",
                           "extensions": {"chisel/client-message-id": "cm-fixture-1"},
                           "telemetry": {"source": "user", "operation": "unknown"}})
    insert_node(c, sid, 0, None, sys_info)
    insert_node(c, sid, 1, 0, rules)
    insert_node(c, sid, 2, 1, prompt)

    # The rebuilt chain, rooted at node 3: a 7-node system prefix, then copies of the first
    # chain's nodes carrying compact/prior_node_ids, then the turn.
    prefixes = [
        "You are Devin, an interactive command line agent from Cognition. (fixture)",
        "Available subagent profiles for the `run_subagent` tool. (fixture)",
        "## Parallel tool calls (fixture)",
        "Fixture rule block A.",
        "Fixture rule block B.",
        "Fixture rule block C.",
        "You are powered by SWE-2 High.",
    ]
    for i, text in enumerate(prefixes):
        insert_node(c, sid, 3 + i, None if i == 0 else 3 + i - 1,
                    msg(f"m-prefix-{i}", "system", text),
                    meta=node_meta(prefix=True))
    insert_node(c, sid, 10, 9, sys_info,
                meta=node_meta(prior=[0]))  # <system_info> copy + cog context
    # Re-insert with the cog-context extension the live store carries.
    si = dict(sys_info)
    si["metadata"] = {"extensions": {"affogato/cog-context": {"key": "workspace/context"}},
                      "created_at": "2026-10-04T00:00:10Z", "telemetry": {"source": "system"}}
    c.execute("UPDATE message_nodes SET chat_message = ? WHERE session_id = ? AND node_id = 10",
              (json.dumps(si), sid))
    insert_node(c, sid, 11, 10, rules, meta=node_meta(prior=[1]))
    insert_node(c, sid, 12, 11, prompt, meta=node_meta(prior=[2]))

    call1 = "exec_0_aaaa#fixture1"
    call2 = "exec_1_bbbb#fixture2"
    asst1 = msg("m-asst-1", "assistant", "I'll check both.",
                tool_calls=[
                    {"id": call1, "name": "exec", "arguments": {"command": "ls /work/alpha"},
                     "index": 0, "kind": "function"},
                    {"id": call2, "name": "exec",
                     "arguments": {"command": "cat /work/alpha/README.md"},
                     "index": 1, "kind": "function"},
                ],
                thinking={"thinking": "fixture reasoning", "signature": "sealed.v1.fixture",
                          "signature_type": "sealed"},
                metadata=assistant_meta(
                    "2026-10-04T00:00:12Z", "tool_calls",
                    tool_call_content={
                        call1: {"toolCallId": call1, "title": "Ran ls", "status": "pending",
                                "kind": "execute", "rawInput": {"command": "ls"},
                                "content": [], "_meta": {}},
                        call2: {"toolCallId": call2, "title": "Ran cat", "status": "pending",
                                "kind": "execute", "rawInput": {"command": "cat"},
                                "content": [], "_meta": {}},
                    }))
    insert_node(c, sid, 13, 12, asst1, meta=node_meta(ntp=1234))

    tool1 = msg("m-tool-1", "tool", "total 1\n-rw-r--r-- 1 u g 5 README.md", tool_call_id=call1,
                metadata={
                    "extensions": {
                        "chisel/tool_call_timing": {"started_at": "2026-10-04T00:00:13Z",
                                                  "finished_at": "2026-10-04T00:00:13Z",
                                                  "duration_ms": 12},
                        "chisel/tool_result_meta": {"success": True, "kind": "execute"},
                        "chisel/terminal_output": {"text": "total 1\n-rw-r--r-- 1 u g 5 README.md",
                                                   "cwd": "/work/alpha",
                                                   "exit": {"terminal_id": "t-fixture",
                                                            "exit_code": 0}},
                        "chisel/undo": [{"kind": "irreversible", "tool_name": "exec",
                                         "description": "ran ls"}],
                    },
                    "created_at": "2026-10-04T00:00:13Z",
                    "telemetry": {"source": "tool_result", "operation": "exec"}})
    tool2 = msg("m-tool-2", "tool", "cat: /work/alpha/README.md: No such file or directory",
                tool_call_id=call2,
                metadata={
                    "extensions": {
                        "chisel/tool_call_timing": {"started_at": "2026-10-04T00:00:14Z",
                                                  "finished_at": "2026-10-04T00:00:14Z",
                                                  "duration_ms": 3},
                        "chisel/tool_result_meta": {"success": False, "kind": "execute"},
                    },
                    "created_at": "2026-10-04T00:00:14Z",
                    "telemetry": {"source": "tool_result", "operation": "exec"}})
    insert_node(c, sid, 14, 13, tool1)
    insert_node(c, sid, 15, 14, tool2)

    asst2 = msg("m-asst-2", "assistant", "The listing worked; the cat failed because the file is absent.",
                metadata=assistant_meta("2026-10-04T00:00:15Z", "stop"))
    insert_node(c, sid, 16, 15, asst2, meta=node_meta(ntp=1400))

    # ── fixture-alpha's sidekick: a second root in the SAME forest, reached via
    # subagent_heads.chain_node_id — own prefix, rules/skills loads, handoff prompt, one turn.
    sub_call = "exec_0_cccc#fixturesub"
    insert_node(c, sid, 17, None,
                msg("s-prefix-0", "system", "You are the Sidekick subagent of Devin. (fixture)"),
                meta=node_meta(prefix=True))
    insert_node(c, sid, 18, 17,
                msg("s-prefix-1", "system", "You are powered by SWE-2 Medium."),
                meta=node_meta(prefix=True))
    rules_loaded = msg("s-rules", "system",
                       '<rules type="always-on">fixture sidekick rules</rules>',
                       metadata={"extensions": {"agent-ext/rules-loaded": {
                           "rule_paths": ["/work/alpha/CLAUDE.md"],
                           "available_rule_paths": ["/work/alpha/CLAUDE.md",
                                                    "/work/alpha/AGENTS.md"],
                           "content_bytes": 24, "is_always_on": True}},
                                 "created_at": "2026-10-04T00:00:16Z",
                                 "telemetry": {"source": "system"}})
    skills_loaded = msg("s-skills", "system",
                        "The following skills are available. (fixture)",
                        metadata={"extensions": {"agent-ext/skills-loaded": {
                            "skills": [{"name": "repo-hygiene",
                                        "description": "audit a repo",
                                        "path": "/skills/repo-hygiene/SKILL.md"}]}},
                                  "created_at": "2026-10-04T00:00:17Z",
                                  "telemetry": {"source": "system"}})
    handoff = msg("s-handoff", "user", "Implement the fixture change, then report.",
                  metadata={"is_user_input": True,
                            "extensions": {"subagent/handoff": True,
                                           "chisel/fusion_lead_model_uid": "claude-fable-5-1-medium"},
                            "created_at": "2026-10-04T00:00:18Z",
                            "telemetry": {"source": "user", "operation": "unknown"}})
    sub_asst = msg("s-asst-1", "assistant", "On it.",
                   tool_calls=[{"id": sub_call, "name": "exec",
                                "arguments": {"command": "ls /work/alpha"}, "index": 0,
                                "kind": "function"}],
                   metadata=assistant_meta("2026-10-04T00:00:19Z", "tool_calls",
                                           model="swe-2-medium"))
    sub_tool = msg("s-tool-1", "tool", "README.md", tool_call_id=sub_call,
                   metadata={"extensions": {
                       "chisel/tool_call_timing": {"duration_ms": 2},
                       "chisel/tool_result_meta": {"success": True, "kind": "execute"}},
                       "created_at": "2026-10-04T00:00:20Z",
                       "telemetry": {"source": "tool_result", "operation": "exec"}})
    sub_done = msg("s-asst-2", "assistant", "Done — reporting back.",
                   metadata=assistant_meta("2026-10-04T00:00:21Z", "stop", model="swe-2-medium"))
    # A compaction pair on the sidekick chain (the live shape: an assistant node carrying the
    # summary markdown, then a system node wrapping it with the "Full conversation history
    # saved at …" line and the sidecar extensions — both marked summarized_from=<old head>).
    sub_summary = msg("s-summary", "assistant",
                      "## Summary of prior work\nImplemented the adapter; tests pending.",
                      metadata=assistant_meta("2026-10-04T00:00:22Z", "stop",
                                              model="swe-2-medium", metrics=False))
    cont = msg("s-continue", "system",
               "You are continuing work from a previous conversation thread. Below is a"
               " summary of the previous conversation thread:\n"
               "Full conversation history saved at"
               " /work/summaries/sidekick/history_0123abcd.md.\n"
               "Summary: … (fixture)",
               metadata={"extensions": {
                   "devin-rs/summary": {"source": "async_file_compactor"},
                   "compact/edited_files": {"paths": ["/work/alpha/src/lib.rs",
                                                    "/work/alpha/README.md"]},
                   "compact/todo_list": {"todos": [{"content": "verify", "status": "pending"}]},
                   "subagent/handoff_history": {"messages": [{"role": "user",
                                                            "message": "(dup of handoff)"}]},
               },
               "created_at": "2026-10-04T00:00:23Z", "telemetry": {"source": "system"}})
    fail_call = "exec_0_dddd#fixturefail"
    sub_asst2 = msg("s-asst-3", "assistant", "Retrying the edit.",
                    tool_calls=[{"id": fail_call, "name": "edit",
                                 "arguments": {"file_path": "/work/alpha/src/lib.rs"},
                                 "index": 0, "kind": "function"}],
                    metadata=assistant_meta("2026-10-04T00:00:24Z", "tool_calls",
                                            model="swe-2-medium"))
    fail_tool = msg("s-tool-2", "tool",
                    "Tool 'edit' validation failed: String not found in file.",
                    tool_call_id=fail_call,
                    metadata={"extensions": {
                        "chisel/tool_failure": {"reason": "ValidationError"},
                        "chisel/tool_call_timing": {"duration_ms": 1}},
                        "created_at": "2026-10-04T00:00:25Z",
                        "telemetry": {"source": "tool_result", "operation": "edit"}})
    sub_done2 = msg("s-asst-4", "assistant", "Adjusted and done — reporting back.",
                    metadata=assistant_meta("2026-10-04T00:00:26Z", "stop", model="swe-2-medium"))
    insert_node(c, sid, 19, 18, rules_loaded)
    insert_node(c, sid, 20, 19, skills_loaded)
    insert_node(c, sid, 21, 20, handoff)
    insert_node(c, sid, 22, 21, sub_asst)
    insert_node(c, sid, 23, 22, sub_tool)
    insert_node(c, sid, 24, 23, sub_done)
    insert_node(c, sid, 25, 24, sub_summary, meta=node_meta(summarized_from=24, ntp=48000))
    insert_node(c, sid, 26, 25, cont, meta=node_meta(summarized_from=24))
    insert_node(c, sid, 27, 26, sub_asst2)
    insert_node(c, sid, 28, 27, fail_tool)
    insert_node(c, sid, 29, 28, sub_done2)
    c.execute("INSERT INTO subagent_heads (session_id, agent_id, chain_node_id, updated_at)"
              " VALUES (?,?,?,?)", (sid, "sidekick", 29, T0 + 200))
    # A stale head left behind after a forest rotation: no node 999 — skipped, counted dangling.
    c.execute("INSERT INTO subagent_heads (session_id, agent_id, chain_node_id, updated_at)"
              " VALUES (?,?,?,?)", (sid, "ghost", 999, T0 + 150))

    # ── session B: fixture-beta — model '', main_chain_id NULL, hidden, corrupt node ──
    sid = "fixture-beta"
    c.execute(
        "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,"
        " created_at, last_activity_at, title, main_chain_id, shell_last_seen_index, cogs_json,"
        " workspace_dirs, hidden, metadata) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        (sid, "/work/beta", "windsurf", "", "normal",
         T0, T0 + 10, "Fixture session beta", None, 0, None, "[]", 1, None))
    insert_node(c, sid, 0, None,
                msg("b-prefix", "system", "You are Devin (fixture beta)."),
                meta=node_meta(prefix=True))
    insert_node(c, sid, 1, 0,
                msg("b-user", "user", "fixture beta prompt",
                    metadata={"is_user_input": True, "created_at": "2026-10-04T00:00:05Z"}))
    insert_node(c, sid, 2, 1, "{not json — corrupt row")  # skipped, counted
    insert_node(c, sid, 3, 2,
                msg("b-asst", "assistant", "fixture beta reply",
                    metadata=assistant_meta("2026-10-04T00:00:06Z", "stop")))

    c.commit()
    c.close()
    print(f"wrote {out} ({os.path.getsize(out)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
