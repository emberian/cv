// Generate a REAL OpenClaw sqlite transcript store (for cv's adapter tests) through OpenClaw's own
// store code — not by cv guessing at a schema.
//
//   cd ~/pug/openclaw           # tsx resolves OpenClaw's tsconfig paths against the cwd
//   OPENCLAW_STATE_DIR=$(mktemp -d) \
//     node --import ./scripts/tsx.mjs ~/dev/cv/tools/harness-fixtures/openclaw/generate-openclaw-agent-sqlite.mts
//
// The script stays in the cv repo: it imports OpenClaw's modules by absolute path out of
// $OPENCLAW_SRC, so nothing is ever copied into the checkout.
//
// Env:
//   OPENCLAW_STATE_DIR  (required)  throwaway state root; refuses $HOME or ~/.openclaw
//   OPENCLAW_SRC        (default ~/pug/openclaw)               the checkout to import from
//   OPENCLAW_LEGACY_DIR (default ~/.openclaw/agents/main/sessions)  a v3 JSONL store to migrate;
//                       read-only, and skipped with a message when it holds no .jsonl
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const home = process.env.HOME ?? "";

function die(msg: string): never {
  console.error(`generate-openclaw-agent-sqlite: ${msg}`);
  process.exit(1);
}

// ── prerequisites ─────────────────────────────────────────────────────────────────────────────
const stateDirRaw = process.env.OPENCLAW_STATE_DIR ?? "";
if (!stateDirRaw.trim()) {
  die("OPENCLAW_STATE_DIR is unset. Set it to a throwaway dir: OPENCLAW_STATE_DIR=$(mktemp -d)");
}
const stateDir = path.resolve(stateDirRaw);
if (stateDir === path.join(home, ".openclaw") || stateDir === home) {
  die(`refusing to write into ${stateDir} — OPENCLAW_STATE_DIR must be a throwaway dir`);
}

const src = path.resolve(process.env.OPENCLAW_SRC ?? path.join(home, "pug", "openclaw"));
if (!fs.existsSync(path.join(src, "package.json"))) {
  die(`no OpenClaw checkout at ${src} (set OPENCLAW_SRC). Clone it and install deps first.`);
}
if (!fs.existsSync(path.join(src, "node_modules"))) {
  die(`${src} has no node_modules — install its deps first (pnpm install / bun install).`);
}
// tsx resolves OpenClaw's tsconfig `paths` against process.cwd(); from anywhere else the first
// bare import inside OpenClaw's own source dies with ERR_MODULE_NOT_FOUND, which reads like a
// broken checkout rather than a wrong cwd. Say the real thing instead.
if (path.resolve(process.cwd()) !== src) {
  die(`run this from the OpenClaw checkout root (cd ${src}) — tsx resolves its tsconfig paths against the cwd`);
}

const mod = (rel: string) => import(pathToFileURL(path.join(src, rel)).href);
const { formatSqliteSessionFileMarker } = await mod("src/config/sessions/legacy-sqlite-marker.ts");
const {
  appendTranscriptMessage,
  loadTranscriptEvents,
  replaceTranscriptEventsSync,
  upsertSessionEntryCore,
} = await mod("src/config/sessions/session-accessor.ts");
const { selectVisibleTranscriptEvents } = await mod("src/config/sessions/transcript-visible-events.ts");
const { SessionManager } = await mod("src/agents/sessions/session-manager.ts");

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

  // session 2: a legacy v3 JSONL, landed the way a migration lands it. Read-only, and optional:
  // without one the store is still valid, it just has no migrated session to check the adapter's
  // legacy path against — so say so rather than throwing ENOENT from readdirSync.
  const legacyDir = path.resolve(
    process.env.OPENCLAW_LEGACY_DIR ?? path.join(home, ".openclaw", "agents", "main", "sessions"),
  );
  let legacyId: string | undefined;
  const legacyFile = fs.existsSync(legacyDir)
    ? fs.readdirSync(legacyDir).find((f) => f.endsWith(".jsonl"))
    : undefined;
  if (!legacyFile) {
    console.warn(
      `no legacy v3 JSONL under ${legacyDir} — skipping the migrated session.\n` +
      `  The fixture will be missing the legacy-migration shape; set OPENCLAW_LEGACY_DIR to a dir that has one.`,
    );
  } else {
    const lines = fs.readFileSync(path.join(legacyDir, legacyFile), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
    legacyId = path.basename(legacyFile, ".jsonl");
    const indexPath = path.join(legacyDir, "sessions.json");
    const index = fs.existsSync(indexPath) ? JSON.parse(fs.readFileSync(indexPath, "utf8")) : {};
    const entry = Object.values(index as Record<string, any>).find((e: any) => e?.sessionId === legacyId) ?? {};
    const legacyKey = `agent:main:legacy-${legacyId}`;
    await upsertSessionEntryCore({ agentId: "main", sessionKey: legacyKey, storePath }, { ...entry, sessionId: legacyId, updatedAt: entry.updatedAt ?? now() } as any);
    replaceTranscriptEventsSync({ agentId: "main", sessionId: legacyId, sessionKey: legacyKey, storePath }, lines as any);
    console.log(`legacy -> ${legacyId} (${lines.length} lines from ${path.join(legacyDir, legacyFile)})`);
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
  const sqlite = path.join(agentDir, "agent", "openclaw-agent.sqlite");
  if (!fs.existsSync(sqlite)) die(`OpenClaw wrote no store at ${sqlite}`);
  console.log(`\nstore: ${sqlite}`);

  const out = process.env.OPENCLAW_OUT;
  if (!out) {
    console.log(`(set OPENCLAW_OUT=<path> to also write the trimmed fixture; the raw store above is ~8x larger)`);
    console.log(`compare:  sqlite3 ${sqlite} .tables`);
  } else {
    console.log(`trimmed fixture -> ${trim(sqlite, path.resolve(out))}`);
  }
}

// A fresh OpenClaw store carries ~55 tables — memory index, FTS shadows, boards, auth — none of
// which cv reads. The committed fixture is the five the adapter touches, which is why it is 80 KB
// and a raw store is 670 KB. Copy, drop the rest, VACUUM: the kept rows are still exactly what
// OpenClaw's own code wrote.
function trim(src: string, out: string): string {
  const KEEP = new Set([
    "schema_meta", "session_windows", "session_nodes", "transcript_events", "transcript_event_identities",
  ]);
  // node:sqlite ships with Node >= 22.5; it is only used here, so require it late and say so clearly.
  let DatabaseSync: any;
  try {
    ({ DatabaseSync } = createRequire(import.meta.url)("node:sqlite"));
  } catch {
    die(`OPENCLAW_OUT needs node:sqlite (Node >= 22.5); this is Node ${process.version}. Trim by hand or unset OPENCLAW_OUT.`);
  }
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.rmSync(out, { force: true });
  // VACUUM INTO, not copyFileSync: the store is in WAL mode, so most of what OpenClaw just wrote is
  // still in the -wal sibling and a plain file copy lands an almost-empty database.
  const live = new DatabaseSync(src);
  live.exec(`VACUUM INTO '${out.replace(/'/g, "''")}'`);
  live.close();
  const db = new DatabaseSync(out);
  // Order matters. Triggers and views first (they reference the tables); then the fts5 VIRTUAL
  // tables, which take their own `_data`/`_idx`/`_docsize`/`_config` shadow tables with them —
  // SQLite refuses `DROP TABLE` on a shadow while its parent still exists. Two passes, because a
  // trigger dropped late can block a table dropped early.
  const rank = (o: { type: string; sql: string | null }) =>
    o.type === "trigger" ? 0 : o.type === "view" ? 1 : /CREATE VIRTUAL TABLE/i.test(o.sql ?? "") ? 2 : 3;
  const dropped: string[] = [];
  let leftover: { type: string; name: string; sql: string | null }[] = [];
  for (const pass of [0, 1]) {
    const objs = (pass === 0
      ? db.prepare("SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'").all()
      : leftover) as { type: string; name: string; sql: string | null }[];
    leftover = [];
    for (const o of [...objs].sort((a, b) => rank(a) - rank(b))) {
      // Explicit indexes go too — at 46 rows they buy nothing and they are most of the file.
      // The implicit `sqlite_autoindex_*` that carry the PRIMARY KEYs are not droppable and stay.
      if (o.type === "index" && o.name.startsWith("sqlite_")) continue;
      if (o.type === "table" && KEEP.has(o.name)) continue;
      try {
        db.exec(`DROP ${o.type.toUpperCase()} IF EXISTS "${o.name}"`);
        dropped.push(o.name);
      } catch (e) {
        if (pass === 0) leftover.push(o);
        else console.warn(`  could not drop ${o.type} ${o.name}: ${(e as Error).message}`);
      }
    }
  }
  db.exec("VACUUM");
  const kept = (db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").all() as { name: string }[]).map((r) => r.name);
  db.close();
  console.log(`  dropped ${dropped.length} objects; kept tables: ${kept.join(" ")}`);
  console.log(`  install:  cp ${out} <cv>/crates/cv-core/tests/fixtures/openclaw/openclaw-agent.sqlite`);
  return out;
}
await main();
