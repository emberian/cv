// Shared lazy hydration: the session pool is metadata-only stubs (from cvd `/api/sessions` or the
// native `local_sessions` command) until a transcript is actually needed. Any view (transcript,
// compare, loom) calls `hydrateSession(stub)` to pull the full message tree on demand — native
// command in the desktop, HTTP to a local cvd in the browser. Already-full sessions pass through.
import { invoke, canInvokeNative } from "../tauri.js";
import { normalizeSession } from "./util.js";

// Where the cvd JSON API lives. Resolution order:
//   1) an explicit `window.__CVD_BASE__` override (set it before the app loads to point elsewhere);
//   2) the page's own origin when the dashboard is served over http(s) — this is the case when you
//      run `cvd serve --web ./web`, so the API is right here regardless of which port you chose;
//   3) the classic localhost:7777 default (file:// / tauri:// origins, or a static deploy).
export const CVD_BASE = (() => {
  try {
    if (typeof window !== "undefined" && window.__CVD_BASE__) return String(window.__CVD_BASE__).replace(/\/+$/, "");
    const o = typeof location !== "undefined" ? location.origin : "";
    if (/^https?:$/.test(typeof location !== "undefined" ? location.protocol : "")) return o;
  } catch { /* fall through */ }
  return "http://localhost:7777";
})();

/** Whether a session still needs its messages loaded. */
export function isStub(s) {
  return !!(s && s._stub && !(s.messages && s.messages.length));
}

/** Resolve a stub to a full session (with messages). Full sessions are returned unchanged. */
export async function hydrateSession(stub) {
  if (!isStub(stub)) return stub;
  let raw;
  if (canInvokeNative()) {
    raw = JSON.parse(await invoke("local_session", { harness: stub.harness, id: stub.id }));
  } else {
    const url = `${CVD_BASE}/api/session/${encodeURIComponent(stub.harness)}/${encodeURIComponent(stub.id)}`;
    const resp = await fetch(url, { headers: { Accept: "application/json" } });
    if (!resp.ok) throw new Error(`cvd ${resp.status}`);
    raw = await resp.json();
  }
  const full = normalizeSession(raw);
  full._stub = false;
  return full;
}

const enc = encodeURIComponent;

/** Window size for paged transcript loads. */
export const PAGE = 200;

/** Fetch the message window [start, end) of a session without loading the whole transcript
 *  (cvd ≥0.9.12's `/messages` endpoint / the desktop's `local_messages` command). Returns
 *  `{ messages, start, end, has_more, total_known, total, message_count, session }`, or `null`
 *  when the windowed path isn't available (older cvd, no cvd, older app) — callers fall back to
 *  a full `hydrateSession`.
 *
 *  `opts.extra` keeps each message's harness `extra` map (subtype / compactMetadata / the
 *  toolUseResult sidecar) — needed to find compaction boundaries; off by default so ordinary
 *  transcript paging stays lean. */
export async function getMessages(stub, start, end, opts = {}) {
  if (!stub || !stub.harness || !stub.id) return null;
  const wantExtra = !!opts.extra;
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_messages", {
        harness: stub.harness, id: stub.id, start, end, extra: wantExtra,
      }));
    } else {
      const ex = wantExtra ? "&extra=1" : "";
      const url = `${CVD_BASE}/api/session/${enc(stub.harness)}/${enc(stub.id)}/messages?start=${start}&end=${end}${ex}`;
      const resp = await fetch(url, { headers: { Accept: "application/json" } });
      if (!resp.ok) return null; // 404 = older cvd (or unknown session): take the full path
      raw = await resp.json();
    }
    return raw && Array.isArray(raw.messages) ? raw : null;
  } catch {
    return null;
  }
}

/** Everything a session knows about ITSELF, with no messages: the session row plus `model`,
 *  `git`, `system_prompt`, `lineage` and the exact message `total`.
 *
 *  Why this is a separate call and not part of `getMessages`: a windowed read stops streaming
 *  once its window is full, so a session-level fact recorded LATER in the transcript is never
 *  reached — Claude writes its system prompt well after the opening turns, so opening a
 *  transcript at the top could never learn it. One cheap pass that keeps no messages answers it.
 *
 *  `null` on an older cvd or app that has no head endpoint; callers just show less. */
export async function getSessionHead(stub) {
  if (!stub || !stub.harness || !stub.id) return null;
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_session_head", { harness: stub.harness, id: stub.id }));
    } else {
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(stub.harness)}/${enc(stub.id)}/head`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return null;
      raw = await resp.json();
    }
    return raw && typeof raw === "object" ? raw : null;
  } catch {
    return null;
  }
}

/** The session's extracted tool events (file edits/reads, commands, errors), in transcript
 *  order, optionally filtered by kind. Empty on any error or an older cvd — the events panel
 *  simply doesn't appear. */
export async function getEvents(session, kind) {
  if (!session || !session.harness || !session.id) return [];
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_events", { harness: session.harness, id: session.id, kind: kind ?? null }));
    } else {
      const q = kind ? `?kind=${enc(kind)}` : "";
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/events${q}`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return [];
      raw = await resp.json();
    }
    return Array.isArray(raw) ? raw : [];
  } catch {
    return [];
  }
}

/** Every compaction boundary in a session (cvd ≥0.9.12's `/compactions` endpoint): an array of
 *  `{ index, ts, trigger, pre_tokens, post_tokens, duration_ms, pre_discovered_tools, metadata }`
 *  in transcript order, where `index` is the message index `/messages` uses. Scans the whole
 *  transcript server-side (complete coverage) but returns only the tiny boundary records. Returns
 *  `null` when unavailable (older cvd / no cvd) so the caller can fall back to scanning a window. */
export async function getCompactions(session) {
  if (!session || !session.harness || !session.id) return null;
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_compactions", { harness: session.harness, id: session.id }));
    } else {
      const resp = await fetch(
        `${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/compactions`,
        { headers: { Accept: "application/json" } },
      );
      if (!resp.ok) return null;
      raw = await resp.json();
    }
    if (Array.isArray(raw)) return raw;
    return raw && Array.isArray(raw.compactions) ? raw.compactions : null;
  } catch {
    return null;
  }
}

/** Which sessions touched a file (by exact or suffix path match). Returns an array of
 *  `{ harness, session_id, title, edits, reads, last_ts }` rows, or `null` when the lookup
 *  isn't available (older cvd / no cvd) so the UI can say so instead of showing "no results". */
export async function getTouched(path, editsOnly = false) {
  if (!path) return [];
  try {
    if (canInvokeNative()) {
      const raw = JSON.parse(await invoke("local_touched", { path, editsOnly: !!editsOnly }));
      return Array.isArray(raw) ? raw : null;
    }
    const resp = await fetch(`${CVD_BASE}/api/touched?path=${enc(path)}${editsOnly ? "&edits_only=true" : ""}`, {
      headers: { Accept: "application/json" },
    });
    if (!resp.ok) return null;
    const raw = await resp.json();
    return Array.isArray(raw) ? raw : null;
  } catch {
    return null;
  }
}

/** The sub-agents a session spawned (Claude Code Task sub-agents), as metadata-only stubs. Empty on
 *  any error or for harnesses without sub-agents. */
export async function getSubagents(session) {
  if (!session || !session.harness || !session.id) return [];
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_subagents", { harness: session.harness, id: session.id }));
    } else {
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/subagents`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return [];
      raw = await resp.json();
    }
    if (!Array.isArray(raw)) return [];
    return raw.map((s) => {
      const n = normalizeSession(s);
      n._stub = true;
      n._parentId = session.id;
      n._parentHarness = session.harness;
      return n;
    });
  } catch {
    return [];
  }
}

/** The full sub-agent **forest** a session spawned: directly-spawned (`Agent`/`Task`) sub-agents
 *  AND workflow sub-agents (one tier deeper), each with its enrichment sidecar fields preserved —
 *  `agent_type`, `description`, `tool_use_id`, `workflow`, `result_status`, `result_summary` —
 *  which `getSubagents`' normalize pass drops. Returns an array of objects shaped like
 *  `{ ...sessionStub, agent_id, agent_type, description, tool_use_id, workflow, result_status,
 *  result_summary, _parentId, _parentHarness }`. Empty on any error / harness without sub-agents.
 *
 *  This shares the same `/subagents` endpoint as `getSubagents`, but keeps the forest metadata that
 *  the forest view groups and labels by. */
export async function getSubagentTree(session) {
  if (!session || !session.harness || !session.id) return [];
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_subagents", { harness: session.harness, id: session.id }));
    } else {
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/subagents`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return [];
      raw = await resp.json();
    }
    if (!Array.isArray(raw)) return [];
    return raw.map((s) => {
      const n = normalizeSession(s);
      n._stub = true;
      n._parentId = session.id;
      n._parentHarness = session.harness;
      // Carry the forest enrichment the normalizer doesn't model.
      n.agent_id = s.agent_id ?? n.id;
      n.agent_type = s.agent_type ?? null;
      n.description = s.description ?? null;
      n.tool_use_id = s.tool_use_id ?? null;
      n.workflow = s.workflow ?? null;
      n.result_status = s.result_status ?? null;
      n.result_summary = s.result_summary ?? null;
      return n;
    });
  } catch {
    return [];
  }
}

/** The driving script Claude Code recorded for a workflow run (cvd ≥0.9.12's
 *  `/workflow/{wf}/script` endpoint). Returns `{ workflow, name, source }`, or `null` when no script
 *  is recorded / the endpoint is unavailable (older cvd, native desktop without the command). */
export async function getWorkflowScript(session, workflow) {
  if (!session || !session.harness || !session.id || !workflow) return null;
  try {
    let raw;
    if (canInvokeNative()) {
      // Optional native command; absent on older desktops → caught below.
      raw = JSON.parse(await invoke("local_workflow_script", {
        harness: session.harness, id: session.id, workflow,
      }));
    } else {
      const resp = await fetch(
        `${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/workflow/${enc(workflow)}/script`,
        { headers: { Accept: "application/json" } },
      );
      if (!resp.ok) return null;
      raw = await resp.json();
    }
    return raw && typeof raw.source === "string" ? raw : null;
  } catch {
    return null;
  }
}

/** Full transcript of one sub-agent, loaded relative to its parent. */
export async function getSubagent(parentHarness, parentId, agentId) {
  let raw;
  if (canInvokeNative()) {
    raw = JSON.parse(await invoke("local_subagent", { harness: parentHarness, parent: parentId, agent: agentId }));
  } else {
    const resp = await fetch(`${CVD_BASE}/api/session/${enc(parentHarness)}/${enc(parentId)}/subagent/${enc(agentId)}`, {
      headers: { Accept: "application/json" },
    });
    if (!resp.ok) throw new Error(`cvd ${resp.status}`);
    raw = await resp.json();
  }
  const full = normalizeSession(raw);
  full._stub = false;
  return full;
}

// ---------------------------------------------------------------------------
// Corpus-wide reads. These two ask the daemon a question the browser cannot answer for itself:
// the pool the UI holds is session *stubs*, so it can match a title but never a message body,
// and it can count sessions but never messages it has not downloaded. Both return `null` when
// the endpoint isn't there (the static demo, an older cvd) — every caller keeps a local answer
// for that case and says which one it is showing.
// ---------------------------------------------------------------------------

/** `GET /api/search?q=&limit=&harness=&cwd=&semantic=1` — full-text (or embedding) search over
 *  every session's content, returning the rows `cv search --json` emits: the §3 session row plus
 *  `score`, `snippet`, and the sub-agent provenance `agent_id` / `parent_id` / `workflow`.
 *
 *  `null` means "this deployment cannot search" (404 / unreachable / no cvd), which is a
 *  different answer from `[]` ("searched, found nothing") — the caller falls back to filtering
 *  the loaded stubs and says so in the placeholder. */
export async function searchSessions(q, opts = {}) {
  const query = String(q ?? "").trim();
  if (!query) return [];
  const params = new URLSearchParams({ q: query });
  if (opts.limit) params.set("limit", String(opts.limit));
  if (opts.harness) params.set("harness", opts.harness);
  if (opts.cwd) params.set("cwd", opts.cwd);
  if (opts.semantic) params.set("semantic", "1");
  try {
    const resp = await fetch(`${CVD_BASE}/api/search?${params}`, {
      headers: { Accept: "application/json" },
      signal: opts.signal,
    });
    if (!resp.ok) return null;
    const raw = await resp.json();
    return Array.isArray(raw) ? raw : null;
  } catch (e) {
    if (e?.name === "AbortError") throw e; // a superseded keystroke, not a missing endpoint
    return null;
  }
}

/** One cheap probe for whether this deployment can search at all, so the search box can promise
 *  the right thing from the first keystroke instead of discovering it on the first miss.
 *  Resolves "server" or "local"; never throws, never blocks longer than `ms`. */
export async function probeSearch(ms = 3500) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ms);
  try {
    const resp = await fetch(`${CVD_BASE}/api/search?q=cv&limit=1`, {
      headers: { Accept: "application/json" },
      signal: ctrl.signal,
    });
    return resp.ok ? "server" : "local";
  } catch {
    return "local";
  } finally {
    clearTimeout(timer);
  }
}

/** `GET /api/stats?q=` — corpus-wide analytics, exactly what `cv stats --json` emits:
 *  `{ sessions, messages, by_harness, top_cwds, earliest_created, latest_updated }`.
 *
 *  `q` is the *session-filter* expression `cv stats --query` takes (see `cv schema`) — a
 *  different language from the search box's free text, so the UI does not forward one as the
 *  other. `null` when the endpoint is absent; the caller then computes what the pool can support
 *  and labels it as such. */
export async function getCorpusStats(q) {
  try {
    const qs = q ? `?q=${enc(q)}` : "";
    const resp = await fetch(`${CVD_BASE}/api/stats${qs}`, { headers: { Accept: "application/json" } });
    if (!resp.ok) return null;
    const raw = await resp.json();
    return raw && typeof raw === "object" && typeof raw.sessions === "number" ? raw : null;
  } catch {
    return null;
  }
}
