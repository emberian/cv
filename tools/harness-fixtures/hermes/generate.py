"""Populate an isolated Hermes state.db through Hermes's OWN store API (no raw SQL), then dump Hermes's
own views for comparison with cv. Run from ~/pug/hermes-agent with HERMES_HOME set."""
import json, os, sys, time
sys.path.insert(0, os.getcwd())
import hermes_state
from hermes_state_ids import new_session_id
from agent.context_compressor import SUMMARY_PREFIX, _SUMMARY_END_MARKER
from hermes_cli.foreign_sessions import import_foreign_session

OUT = os.environ["GEN_OUT"]
db = hermes_state.SessionDB()
print("db:", db.db_path)
MODEL = "anthropic/claude-sonnet-4.5"
SYS_PROMPT = "You are Hermes, a helpful coding agent. Working dir: /tmp/proj."
T0 = 1789900000.0
ids = {}

# ── A: a session that is compacted IN PLACE (archive_and_compact), then continues ────────────
A = new_session_id(); ids["A_compacted"] = A
db.create_session(A, source="cli", model=MODEL, cwd="/tmp/proj",
                  model_config={"max_iterations": 40, "reasoning_config": {"effort": "medium"}},
                  system_prompt=SYS_PROMPT)
db.set_session_title(A, "Fix the flaky test")
r_u1 = db.append_message(A, "user", "fix the flaky test in tests/sched.py", timestamp=T0 + 10)
tc = [{"id": "call_a1", "type": "function", "function": {"name": "read_file", "arguments": json.dumps({"path": "tests/sched.py"})}}]
# deliberately NON-monotonic: the assistant row's timestamp is older than the user prompt's
r_a1 = db.append_message(A, "assistant", "Let me look at the test.", tool_calls=tc, timestamp=T0 + 9)
r_t1 = db.append_message(A, "tool", "def test_sched(): ... sleep(0.01) ...", tool_name="read_file", tool_call_id="call_a1", timestamp=T0 + 11)
r_a2 = db.append_message(A, "assistant", "The test races on a 10ms sleep; I will replace it with a condition wait.", timestamp=T0 + 12)
r_u2 = db.append_message(A, "user", "ok do it", timestamp=T0 + 13)
r_a3 = db.append_message(A, "assistant", "Done — tests/sched.py now waits on the event.", timestamp=T0 + 14)
# In-place compaction exactly as agent/micro_compaction._sync_micro_compact_to_db does it:
# [summary marker] + carried tail rows; tail_count = len - 1. Summary rows use the real prefix and are hidden.
summary = f"{SUMMARY_PREFIX}The user asked to fix a flaky test in tests/sched.py; the 10ms sleep was replaced with an event wait.\n{_SUMMARY_END_MARKER}"
compacted = [
    {"role": "user", "content": summary, "_compressed_summary": True, "display_kind": "hidden", "timestamp": T0 + 15},
    {"role": "user", "content": "ok do it", "timestamp": T0 + 13},
    {"role": "assistant", "content": "Done — tests/sched.py now waits on the event.", "timestamp": T0 + 14},
]
n_active = db.archive_and_compact(A, compacted, tail_count=2)
db.update_system_prompt(A, SYS_PROMPT + "\nContext was compacted once.")
print("A active after compaction:", n_active)
r_u3 = db.append_message(A, "user", "now run the suite", timestamp=T0 + 20)
r_a4 = db.append_message(A, "assistant", "All 42 tests pass.", timestamp=T0 + 21)
# a steer row and a hidden notification row, as the gateway writes them
db.append_message(A, "user", "(steer) keep answers short", display_kind="steer", timestamp=T0 + 22)
db.append_message(A, "user", "[diagnostic] provider latency 900ms", display_kind="hidden",
                  display_metadata={"notification_category": "diagnostic"}, timestamp=T0 + 23)
db.append_delegation_delivery(A, "delegate 7f3c finished: report written to REPORT.md",
                              {"delegation_id": "dlg-7f3c", "presentation_suppressed": False})

# ── R: root with a branch child (B), a reset child (S) and a delegate child (D) ────────────────
R = new_session_id(); ids["R_root"] = R
db.create_session(R, source="cli", model=MODEL, cwd="/tmp/proj2", model_config={"max_iterations": 40})
db.set_session_title(R, "Design the cache")
db.append_message(R, "user", "design a cache for the parser", timestamp=T0 + 100)
db.append_message(R, "assistant", "Two options: LRU by path, or content-hash keyed.", timestamp=T0 + 101)
# /branch (hermes_cli/cli_commands_mixin._handle_branch_command): child BEFORE ending the parent,
# marker in model_config, parent ended 'branched', history copied in one batch.
B = new_session_id(); ids["B_branch"] = B
db.create_session(B, source="cli", model=MODEL, parent_session_id=R,
                  model_config={"max_iterations": 40, "_branched_from": R})
db.end_session(R, "branched")
db.append_messages_batch(B, [
    {"role": "user", "content": "design a cache for the parser", "timestamp": T0 + 100},
    {"role": "assistant", "content": "Two options: LRU by path, or content-hash keyed.", "timestamp": T0 + 101},
])
db.set_session_title(B, "Design the cache (branch)")
db.append_message(B, "user", "go with content-hash", timestamp=T0 + 110)
db.append_message(B, "assistant", "Content-hash keyed cache it is.", timestamp=T0 + 111)
# reset child (gateway/session_recovery.py:433): parent promoted to a reset boundary, child marked _reset_from
db.reopen_session(R)
db.promote_to_session_reset(R, "session_reset")
S = new_session_id(); ids["S_reset"] = S
db.create_session(S, source="cli", model=MODEL, parent_session_id=R, model_config={"_reset_from": R})
db.append_message(S, "user", "fresh start: write the README", timestamp=T0 + 200)
db.append_message(S, "assistant", "README.md written.", timestamp=T0 + 201)
# delegate child (tools/delegate_tool.py:276): the child agent's init model_config carries _delegate_from
D = new_session_id(); ids["D_delegate"] = D
db.create_session(D, source="cli", model=MODEL, parent_session_id=R,
                  model_config={"max_iterations": 20, "_delegate_from": R})
db.append_message(D, "user", "subtask: benchmark the two cache designs", timestamp=T0 + 150)
db.append_message(D, "assistant", "LRU: 1.2ms, content-hash: 0.9ms.", timestamp=T0 + 151)
db.end_session(D, "delegate")

# ── C1 → C2: legacy compression ROTATION chain (publish_compression_child) ────────────────────
C1 = new_session_id(); ids["C1_rotated_parent"] = C1
db.create_session(C1, source="cli", model=MODEL, cwd="/tmp/proj3", model_config={"max_iterations": 40})
db.set_session_title(C1, "Long migration")
for i in range(4):
    db.append_message(C1, "user", f"step {i}: migrate table t{i}", timestamp=T0 + 300 + 2 * i)
    db.append_message(C1, "assistant", f"migrated t{i}", timestamp=T0 + 301 + 2 * i)
C2 = new_session_id(); ids["C2_rotated_child"] = C2
rot_summary = f"{SUMMARY_PREFIX}Tables t0..t3 were migrated.\n{_SUMMARY_END_MARKER}"
db.publish_compression_child(
    parent_session_id=C1, child_session_id=C2, source="cli", model=MODEL,
    model_config={"max_iterations": 40}, system_prompt=SYS_PROMPT, cwd="/tmp/proj3",
    messages=[{"role": "user", "content": rot_summary, "_compressed_summary": True, "display_kind": "hidden", "timestamp": T0 + 320},
              {"role": "user", "content": "step 3: migrate table t3", "timestamp": T0 + 306},
              {"role": "assistant", "content": "migrated t3", "timestamp": T0 + 307}],
    require_compression_lease=False)
db.append_message(C2, "user", "step 4: migrate t4", timestamp=T0 + 330)
db.append_message(C2, "assistant", "migrated t4", timestamp=T0 + 331)

# ── X archived, Y hidden ───────────────────────────────────────────────────────────────────────
X = new_session_id(); ids["X_archived"] = X
db.create_session(X, source="cli", model=MODEL, cwd="/tmp/old")
db.append_message(X, "user", "old stuff", timestamp=T0 + 400)
db.append_message(X, "assistant", "archived reply", timestamp=T0 + 401)
db.set_session_archived(X, True)
Y = new_session_id(); ids["Y_hidden"] = Y
db.create_session(Y, source="telegram", model=MODEL)
db.append_message(Y, "user", "hidden bot chat", timestamp=T0 + 500)
db.append_message(Y, "assistant", "hidden reply", timestamp=T0 + 501)
db.set_session_hidden(Y, True)

# ── F: a foreign import of a real (small) Claude Code transcript ──────────────────────────────
F = import_foreign_session("claude", os.environ["FOREIGN_CLAUDE_PATH"], db); ids["F_foreign_claude"] = F

db.flush_token_counts() if hasattr(db, "flush_token_counts") else None

# ── Hermes's own views, for comparison ─────────────────────────────────────────────────────────
views = {
    "ids": ids,
    "listing": [{k: r.get(k) for k in ("id", "title", "message_count", "parent_session_id", "end_reason", "archived", "hidden", "source", "cwd")}
                for r in db.list_sessions_rich(limit=50)],
    "A_display": [{k: m.get(k) for k in ("id", "role", "content", "tool_call_id", "display_kind", "_compressed_summary", "active", "compacted", "timestamp")}
                  for m in db.get_messages(A, include_compacted=True)],
    "A_model": db.get_messages_as_conversation(A),
    "A_all_rows": [{k: m.get(k) for k in ("id", "role", "content", "display_kind", "_compressed_summary", "active", "compacted")}
                   for m in db.get_messages(A, include_inactive=True)],
    "C2_resume": db.get_messages_as_conversation(C2, include_ancestors=True),
    "R_row": db.get_session(R), "B_row": db.get_session(B), "S_row": db.get_session(S), "D_row": db.get_session(D),
}
with open(OUT, "w") as f:
    json.dump(views, f, indent=1, default=str)
db.close()
print(json.dumps(ids, indent=1))
