// Copy into ~/pug/openclaw/scripts/ (the imports are relative to that repo) and run:
// OPENCLAW_STATE_DIR=<scratch> node --import ./scripts/tsx.mjs scripts/cv-openclaw-fixture.mts
// Generate a REAL OpenClaw sqlite transcript store (for cv's adapter tests) through OpenClaw's own
// store code. Run:  OPENCLAW_STATE_DIR=<scratch> node --import ./scripts/tsx.mjs scripts/cv-openclaw-fixture.mts
import fs from "node:fs";
import path from "node:path";
import { formatSqliteSessionFileMarker } from "../src/config/sessions/legacy-sqlite-marker.js";
import {
  appendTranscriptMessage,
  loadTranscriptEvents,
  replaceTranscriptEventsSync,
  upsertSessionEntryCore,
} from "../src/config/sessions/session-accessor.js";
import { selectVisibleTranscriptEvents } from "../src/config/sessions/transcript-visible-events.js";
import { SessionManager } from "../src/agents/sessions/session-manager.js";

const stateDir = path.resolve(process.env.OPENCLAW_STATE_DIR ?? "");
const home = process.env.HOME ?? "";
if (!process.env.OPENCLAW_STATE_DIR || stateDir === path.join(home, ".openclaw") || stateDir === home) {
  throw new Error("refusing: set OPENCLAW_STATE_DIR to a scratch dir");
}
const agentDir = path.join(stateDir, "agents", "main");
fs.mkdirSync(path.join(agentDir, "sessions"), { recursive: true });
const storePath = path.join(agentDir, "sessions", "sessions.json");
const cwd = "/Users/ember/dev/cv";
let clock = Date.parse("2026-09-19T20:00:00Z");
const now = () => (clock += 1000);

function assistant(text: string, extra: unknown[] = []) {
  return {
    role: "assistant" as const,
    content: [...(extra as never[]), { type: "text" as const, text }],
    api: "messages" as const,
    provider: "anthropic" as const,
    model: "claude-sonnet-4.6",
    usage: {
      input: 120, output: 40, cacheRead: 0, cacheWrite: 0, totalTokens: 160,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: "stop" as const,
    timestamp: now(),
  };
}
const user = (text: string) => ({ role: "user" as const, content: text, timestamp: now() });

function summarize(e: any) {
  if (!e || typeof e !== "object") return String(e);
  if (e.type === "message") {
    const m = e.message;
    const c = typeof m.content === "string" ? m.content : m.content.map((b: any) => b.type === "text" ? b.text : `[${b.type}]`).join(" ");
    return `${e.type}:${m.role}:${e.id}<${e.parentId ?? "root"}>${e.appendMode ? ":side" : ""} ${c.slice(0, 40)}`;
  }
  return `${e.type}:${e.id ?? ""}${e.parentId !== undefined ? `<${e.parentId ?? "root"}>` : ""}${e.targetId !== undefined ? `->${e.targetId}` : ""}${e.name ? ` name=${e.name}` : ""}${e.reason ? ` reason=${e.reason}` : ""}${e.version ? ` v${e.version}` : ""}${e.parentSession ? ` parent=${e.parentSession}` : ""}`;
}

async function main() {
  const sessionId = "cvfix-main-0001";
  const sessionKey = "agent:main:cvfix";
  const marker = formatSqliteSessionFileMarker({ agentId: "main", sessionId, storePath });
  const scope = { agentId: "main", sessionId, sessionKey, storePath };
  await upsertSessionEntryCore(
    { agentId: "main", sessionKey, storePath },
    { delivery: { kind: "internal" }, sessionFile: marker, sessionId, updatedAt: now(), cwd, label: "cv fixture" } as any,
  );
  const u1 = await appendTranscriptMessage(scope, { cwd, eventId: "u1", message: user("first question: list the files") });
  const a1 = await appendTranscriptMessage(scope, {
    cwd, eventId: "a1", parentId: u1.messageId,
    message: assistant("Listing now.", [
      { type: "thinking", thinking: "the user wants a directory listing" },
      { type: "toolCall", id: "call-1", name: "bash", arguments: { command: "ls" } },
    ]),
  });
  const t1 = await appendTranscriptMessage(scope, {
    cwd, eventId: "t1", parentId: a1.messageId,
    message: { role: "toolResult", toolCallId: "call-1", toolName: "bash", content: [{ type: "text", text: "Cargo.toml\nREADME.md" }], isError: false, timestamp: now() },
  });
  const a2 = await appendTranscriptMessage(scope, { cwd, eventId: "a2", parentId: t1.messageId, message: assistant("Two files: Cargo.toml and README.md.") });

  const sm = SessionManager.open({ agentId: "main", sessionId, sessionKey, storePath }, cwd);
  sm.appendSessionInfo("cv fixture session");
  sm.appendModelChange("anthropic", "claude-opus-4.6");
  sm.appendLabelChange(a2.messageId, "good answer");
  sm.appendCompaction("Summary: the user asked for a listing; two files exist.", u1.messageId, 4321);
  sm.appendMessage(user("after compaction: which is bigger?"));
  const a3 = sm.appendMessage(assistant("main branch: README.md is bigger."));
  // a side branch off a2 (abandoned), then back to the main tip
  sm.branch(a2.messageId);
  sm.appendMessage(user("branch: rename README"));
  sm.appendMessage(assistant("branch answer: renamed."));
  sm.appendLeafControl({ targetId: a3, appendParentId: a3 });
  sm.appendMessage(user("back on main"));
  const a4 = sm.appendMessage(assistant("main again."));
  // a side-mode append
  sm.appendLeafControl({ targetId: a4, appendParentId: a4, appendMode: "side" });
  sm.appendMessage(user("side note appended in side mode"));
  sm.appendLeafControl({ targetId: a4, appendParentId: a4 });
  sm.appendResetBoundary("reset");
  sm.appendMessage(user("after reset"));
  const a5 = sm.appendMessage(assistant("post-reset answer"));
  const forkId = await sm.createBranchedSession(a5);
  console.log("fork ->", forkId);
  if (forkId) {
    const fsm = SessionManager.open({ agentId: "main", sessionId: forkId, sessionKey, storePath }, cwd);
    fsm.appendMessage(user("question in the fork"));
    fsm.appendMessage(assistant("fork answer"));
  }

  // session 2: the user's legacy v3 JSONL, landed the way a migration lands it
  const legacyDir = path.join(home, ".openclaw", "agents", "main", "sessions");
  const legacyFile = fs.readdirSync(legacyDir).find((f) => f.endsWith(".jsonl"));
  let legacyId: string | undefined;
  if (legacyFile) {
    const lines = fs.readFileSync(path.join(legacyDir, legacyFile), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
    legacyId = path.basename(legacyFile, ".jsonl");
    const index = JSON.parse(fs.readFileSync(path.join(legacyDir, "sessions.json"), "utf8"));
    const entry = Object.values(index as Record<string, any>).find((e: any) => e?.sessionId === legacyId) ?? {};
    const legacyKey = `agent:main:legacy-${legacyId}`;
    await upsertSessionEntryCore({ agentId: "main", sessionKey: legacyKey, storePath }, { ...entry, sessionId: legacyId, updatedAt: entry.updatedAt ?? now() } as any);
    replaceTranscriptEventsSync({ agentId: "main", sessionId: legacyId, sessionKey: legacyKey, storePath }, lines as any);
  }

  for (const [sid, key] of [[sessionId, sessionKey], [forkId, sessionKey], [legacyId, `agent:main:legacy-${legacyId}`]] as const) {
    if (!sid) continue;
    const events = await loadTranscriptEvents({ agentId: "main", sessionId: sid, sessionKey: key, storePath });
    const visible = selectVisibleTranscriptEvents(events as any[]);
    console.log(`\n=== ${sid}: ${events.length} stored, ${visible.length} visible ===`);
    for (const e of visible) console.log("  " + summarize(e));
    fs.writeFileSync(path.join(stateDir, `openclaw-visible-${sid}.json`), JSON.stringify(visible, null, 1));
    fs.writeFileSync(path.join(stateDir, `openclaw-stored-${sid}.json`), JSON.stringify(events, null, 1));
  }
  console.log("\nstore:", path.join(agentDir, "agent"));
}
await main();
