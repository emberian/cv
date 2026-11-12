// Shared lazy hydration: the session pool is metadata-only stubs (`/api/sessions`) until a
// transcript is actually needed. Any view (transcript, compare, splice) calls `hydrateSession(stub)`
// to pull the full message tree on demand through `apiGet` — the native `api` command in the
// desktop, HTTP to a local `cv serve` in the browser. Already-full sessions pass through.
import { invoke, canInvokeNative } from "../tauri.js";
import { normalizeSession } from "./util.js";

// Where the `cv serve` JSON API lives. Resolution order:
//   1) an explicit `window.__CVD_BASE__` override (set it before the app loads to point elsewhere);
//   2) the page's own origin when the dashboard is served over http(s) — this is the case when you
//      run `cv serve --web ./web`, so the API is right here regardless of which port you chose;
//   3) the classic localhost:7777 default (file:// / tauri:// origins, or a static deploy).
export const CVD_BASE = (() => {
  try {
    if (typeof window !== "undefined" && window.__CVD_BASE__) return String(window.__CVD_BASE__).replace(/\/+$/, "");
    const o = typeof location !== "undefined" ? location.origin : "";
    if (/^https?:$/.test(typeof location !== "undefined" ? location.protocol : "")) return o;
  } catch { /* fall through */ }
  return "http://localhost:7777";
})();

const TOKEN_SS = "cv.token";
let memAuthToken = "";
let wasmStoreRef = null;

function bootstrapUrlToken() {
  if (typeof location === "undefined") return;
  try {
    const sp = new URLSearchParams(location.search || "");
    const hp = new URLSearchParams((location.hash || "").replace(/^#/, ""));
    const tok = hp.get("token") || sp.get("token");
    if (tok) {
      setAuthToken(tok);
      sp.delete("token");
      hp.delete("token");
      if (typeof history !== "undefined" && typeof history.replaceState === "function") {
        const qs = sp.toString();
        const hs = hp.toString();
        history.replaceState(null, "", `${location.pathname}${qs ? `?${qs}` : ""}${hs ? `#${hs}` : ""}`);
      }
    }
  } catch { /* ignore */ }
}

export function getAuthToken() {
  if (memAuthToken) return memAuthToken;
  try {
    const s = sessionStorage.getItem(TOKEN_SS) || "";
    if (s) memAuthToken = s;
    return s;
  } catch {
    return "";
  }
}

export function setAuthToken(tok) {
  memAuthToken = (tok || "").trim();
  try {
    if (memAuthToken) sessionStorage.setItem(TOKEN_SS, memAuthToken);
    else sessionStorage.removeItem(TOKEN_SS);
  } catch { /* sessionStorage may be unavailable */ }
}

export function authHeaders(extra = {}) {
  const tok = getAuthToken();
  return tok ? { ...extra, Authorization: `Bearer ${tok}` } : { ...extra };
}

export function notifyAuthRequired() {
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent("cv:auth-required"));
  }
}

export function registerWasmStore(store) {
  wasmStoreRef = store || null;
}

bootstrapUrlToken();

/** Whether a session still needs its messages loaded. */
export function isStub(s) {
  return !!(s && s._stub && !(s.messages && s.messages.length));
}

const enc = encodeURIComponent;

/** `/api/session/{harness}/{id}` plus an optional tail, encoded. */
function sessionPath(harness, id, tail = "") {
  return `/api/session/${enc(harness)}/${enc(id)}${tail}`;
}

/**
 * GET one route of the read API. In the desktop app it is the native `api` command (the same
 * `route_json` the server runs, in-process: no HTTP, no CORS); in a browser it is `fetch` against a
 * local `cv serve`. Same paths, same JSON, one function per noun on the other side. Throws on a
 * non-2xx answer or an unreachable server; callers decide what absence means.
 * @param {string} path      e.g. `/api/sessions`
 * @param {object} [params]  query parameters (null/undefined values are dropped)
 */
export async function apiGet(path, params = {}) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v != null && v !== false) q.set(k, String(v === true ? 1 : v));
  const query = q.toString();
  if (canInvokeNative()) return JSON.parse(await invoke("api", { path, query }));
  const resp = await fetch(`${CVD_BASE}${path}${query ? `?${query}` : ""}`, {
    headers: authHeaders({ Accept: "application/json" }),
  });
  if (resp.status === 401) notifyAuthRequired();
  if (!resp.ok) {
    const err = new Error(`cv serve ${resp.status}`);
    err.status = resp.status;
    throw err;
  }
  return resp.json();
}

/** Whether a view requiring complete history must fetch, including a browsed page prefix. */
export function needsFullSession(session) { return isStub(session) || !!session?._paged; }

/** Resolve a stub/page prefix to a complete session. Complete local sessions pass through. */
export async function hydrateSession(stub) {
  if (!needsFullSession(stub)) return stub;
  if (stub._wasm && wasmStoreRef?.session) {
    const full = normalizeSession(JSON.parse(wasmStoreRef.session(stub.harness, stub.id)));
    full._stub = false;
    full._wasm = true;
    return full;
  }
  const full = normalizeSession(await apiGet(sessionPath(stub.harness, stub.id)));
  full._stub = false;
  full._remote = true;
  return full;
}

/** Window size for paged transcript loads. */
export const PAGE = 200;

/** Fetch the message window [start, end) of a session without loading the whole transcript.
 *  Returns `{ messages, start, end, has_more, total_known, total, message_count, session }`, or
 *  `null` when the windowed read isn't available — callers fall back to a full `hydrateSession`.
 *
 *  `opts.extra` keeps each message's harness `extra` map (subtype / compactMetadata / the
 *  toolUseResult sidecar) — needed to find compaction boundaries; off by default so ordinary
 *  transcript paging stays lean. */
export async function getMessages(stub, start, end, opts = {}) {
  if (!stub || !stub.harness || !stub.id) return null;
  if (stub._wasm && wasmStoreRef?.messages) {
    try {
      const raw = JSON.parse(wasmStoreRef.messages(stub.harness, stub.id, start, end));
      return raw && Array.isArray(raw.messages) ? raw : null;
    } catch {
      return null;
    }
  }
  try {
    const raw = await apiGet(sessionPath(stub.harness, stub.id, "/messages"), { start, end, extra: !!opts.extra });
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
 *  reached — Claude writes its system prompt well after the opening turns. One cheap pass that
 *  keeps no messages answers it. `null` when unavailable; callers just show less. */
export async function getSessionHead(stub) {
  if (!stub || !stub.harness || !stub.id) return null;
  if (stub._wasm && wasmStoreRef?.session_head) {
    try {
      const raw = JSON.parse(wasmStoreRef.session_head(stub.harness, stub.id));
      return raw && typeof raw === "object" ? raw : null;
    } catch {
      return null;
    }
  }
  try {
    const raw = await apiGet(sessionPath(stub.harness, stub.id, "/head"));
    return raw && typeof raw === "object" ? raw : null;
  } catch {
    return null;
  }
}

/** The session's extracted tool events (file edits/reads, commands, errors), in transcript
 *  order, optionally filtered by kind. Empty on any error — the events panel simply doesn't
 *  appear. Each row's `ts` is an RFC 3339 string. */
export async function getEvents(session, kind) {
  if (!session || !session.harness || !session.id) return [];
  try {
    const raw = await apiGet(sessionPath(session.harness, session.id, "/events"), { kind });
    return Array.isArray(raw) ? raw : [];
  } catch {
    return [];
  }
}

/** Every compaction boundary in a session, as `cv show --kind compaction_boundary --json` rows plus a `headline`:
 *  `{ boundary_msg_idx, summary_msg_idx, trigger, pre_tokens, duration_ms, summary,
 *  pre_compaction_span, headline }`, in the index space `/messages` uses. `null` when unavailable
 *  so the caller can fall back to scanning a window. */
export async function getCompactions(session) {
  if (!session || !session.harness || !session.id) return null;
  try {
    const raw = await apiGet(sessionPath(session.harness, session.id, "/compactions"));
    return Array.isArray(raw) ? raw : null;
  } catch {
    return null;
  }
}

/** Which sessions touched a file (by exact or suffix path match): `{ harness, session_id, title,
 *  edits, reads, last_ts (RFC 3339), agent_id, parent_id, workflow }` rows, or `null` when the
 *  lookup isn't available so the UI can say so instead of showing "no results". */
export async function getTouched(path, editsOnly = false) {
  if (!path) return [];
  try {
    const raw = await apiGet("/api/touched", { path, edits_only: !!editsOnly });
    return Array.isArray(raw) ? raw : null;
  } catch {
    return null;
  }
}

/** The raw `/subagents` rows: the forest a session spawned (directly-spawned `Agent`/`Task`
 *  sub-agents AND the workflow tier), each a session row plus `agent_id`, `agent_type`,
 *  `description`, `tool_use_id`, `workflow`, `result_status`, `result_summary`. */
async function subagentRows(session) {
  if (!session || !session.harness || !session.id) return [];
  try {
    const raw = await apiGet(sessionPath(session.harness, session.id, "/subagents"));
    return Array.isArray(raw) ? raw : [];
  } catch {
    return [];
  }
}

/** The sub-agents a session spawned, as metadata-only stubs. Empty on any error or for harnesses
 *  without sub-agents. */
export async function getSubagents(session) {
  return (await subagentRows(session)).map((s) => {
    const n = normalizeSession(s);
    n._stub = true;
    n._parentId = session.id;
    n._parentHarness = session.harness;
    return n;
  });
}

/** The full sub-agent **forest**, keeping the enrichment fields `getSubagents`' normalize pass
 *  drops — what the forest view groups and labels by. Empty on any error. */
export async function getSubagentTree(session) {
  return (await subagentRows(session)).map((s) => {
    const n = normalizeSession(s);
    n._stub = true;
    n._parentId = session.id;
    n._parentHarness = session.harness;
    n.agent_id = s.agent_id ?? n.id;
    n.agent_type = s.agent_type ?? null;
    n.description = s.description ?? null;
    n.tool_use_id = s.tool_use_id ?? null;
    n.workflow = s.workflow ?? null;
    n.result_status = s.result_status ?? null;
    n.result_summary = s.result_summary ?? null;
    return n;
  });
}

/** The driving script Claude Code recorded for a workflow run: `{ workflow, name, source }`, or
 *  `null` when no script is recorded. */
export async function getWorkflowScript(session, workflow) {
  if (!session || !session.harness || !session.id || !workflow) return null;
  try {
    const raw = await apiGet(sessionPath(session.harness, session.id, `/workflow/${enc(workflow)}/script`));
    return raw && typeof raw.source === "string" ? raw : null;
  } catch {
    return null;
  }
}

/** Full transcript of one sub-agent, loaded relative to its parent. */
export async function getSubagent(parentHarness, parentId, agentId) {
  const full = normalizeSession(await apiGet(sessionPath(parentHarness, parentId, `/subagent/${enc(agentId)}`)));
  full._stub = false;
  return full;
}

// ---------------------------------------------------------------------------
// Corpus-wide reads. These two ask the daemon a question the browser cannot answer for itself:
// the pool the UI holds is session *stubs*, so it can match a title but never a message body,
// and it can count sessions but never messages it has not downloaded. Both return `null` when
// the endpoint isn't there (the static demo, an older server) — every caller keeps a local answer
// for that case and says which one it is showing.
// ---------------------------------------------------------------------------

/** `GET /api/search?q=&limit=&harness=&cwd=&semantic=1` — full-text (or embedding) search over
 *  every session's content, returning the rows `cv search --json` emits: the §3 session row plus
 *  `score`, `snippet`, and the sub-agent provenance `agent_id` / `parent_id` / `workflow`.
 *
 *  `null` means "this deployment cannot search" (404 / unreachable / no server), which is a
 *  different answer from `[]` ("searched, found nothing") — the caller falls back to filtering
 *  the loaded stubs and says so in the placeholder. */
export async function searchSessions(q, opts = {}) {
  const query = String(q ?? "").trim();
  if (!query) return [];
  if (canInvokeNative()) {
    if (opts.signal?.aborted) throw new DOMException("Aborted", "AbortError");
    try {
      const raw = await apiGet("/api/search", {
        q: query,
        limit: opts.limit,
        harness: opts.harness,
        cwd: opts.cwd,
        semantic: opts.semantic ? 1 : undefined,
      });
      if (opts.signal?.aborted) throw new DOMException("Aborted", "AbortError");
      return Array.isArray(raw) ? raw : null;
    } catch (e) {
      if (e?.name === "AbortError") throw e;
      return null;
    }
  }
  const params = new URLSearchParams({ q: query });
  if (opts.limit) params.set("limit", String(opts.limit));
  if (opts.harness) params.set("harness", opts.harness);
  if (opts.cwd) params.set("cwd", opts.cwd);
  if (opts.semantic) params.set("semantic", "1");
  try {
    const resp = await fetch(`${CVD_BASE}/api/search?${params}`, {
      headers: authHeaders({ Accept: "application/json" }),
      signal: opts.signal,
    });
    if (resp.status === 401) notifyAuthRequired();
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
  if (canInvokeNative()) {
    try {
      const raw = await apiGet("/api/search", { q: "cv", limit: 1 });
      return Array.isArray(raw) ? "server" : "local";
    } catch {
      return "local";
    }
  }
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ms);
  try {
    const resp = await fetch(`${CVD_BASE}/api/search?q=cv&limit=1`, {
      headers: authHeaders({ Accept: "application/json" }),
      signal: ctrl.signal,
    });
    if (resp.status === 401) notifyAuthRequired();
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
    const raw = await apiGet("/api/stats", q ? { q } : {});
    return raw && typeof raw === "object" && typeof raw.sessions === "number" ? raw : null;
  } catch {
    return null;
  }
}
