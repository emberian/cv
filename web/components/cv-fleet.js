// <cv-fleet> — the Fleet Dashboard. 📡
//
// Connects to a running `cv serve` HTTP API and live-displays the agent fleet
// by polling every ~2s. CORS is enabled server-side, so plain cross-origin
// `fetch` works from this static page.
//
// Endpoints (base URL default http://localhost:7777):
//   GET /api/health                 → { ok, version?, ... }
//   GET /api/sessions?limit=N       → Session[] (IR) — recent activity feed
//   GET /api/lanes?session=ID       → { counts, lanes } — one session's sub-agents, most urgent first
//   GET /api/stream?follow=1&lanes=ID → SSE `change` events carrying compact lane rows
//
// Renders the recent-sessions activity feed and the lanes of one session (the
// most recent, or the one clicked) with an attention filter, auto-refreshing
// with a pause toggle. Lanes arrive over SSE when the browser and server allow
// it, else by polling. If the API is unreachable, a friendly "start `cv serve`" hint (and a
// static sessions .json loader) is shown instead. (The board, claims and
// presence panels went with the coordination board in 0.15.0; the task log is
// the fleet's record — `cv task events`.)
//
// PERFORMANCE: the static shell (toolbar + section scaffolding) is built ONCE in
// connectedCallback. Each poll then patches only the sections whose data changed,
// diffing against the last-rendered payload (`this._last`) and skipping DOM writes
// when a section is unchanged.
import "./cv-harness-badge.js";
import { msgCount, esc, fmtTime, sessionLabel, shortPath } from "./util.js";
import { authHeaders, getAuthToken, setAuthToken } from "./hydrate.js";

const BASE_LS = "cv-fleet-base";
const DEFAULT_BASE = "http://localhost:7777";
const POLL_MS = 2000;
const SESSION_LIMIT = 25;
const LANES_LS = "cv-fleet-lanes";
// Operator urgency, most urgent first — the order `cv_core::lanes::AttentionState` sorts by.
export const ATTENTION = [
  ["needs_you", "Needs you"],
  ["working", "Working"],
  ["stale", "Stale"],
  ["idle", "Idle"],
  ["done", "Done"],
];

// ---- injected styles (swarm-safe: never touch styles.css) -----------------
// These AUGMENT the global .fleet-* rules already in styles.css with the app's
// amethyst polish — glows, gradient cards, a live pulse, harness-tinted accents.
// All motion is gated behind prefers-reduced-motion: no-preference.
const FLEET_STYLES = `
  cv-fleet .fleet-card {
    background: linear-gradient(160deg, color-mix(in srgb, var(--bg-elev) 92%, var(--accent) 8%), var(--bg-elev));
    border: 1px solid var(--border);
    box-shadow: 0 1px 0 color-mix(in srgb, var(--accent) 10%, transparent) inset, 0 6px 18px -14px var(--accent);
    transition: box-shadow 0.2s ease, border-color 0.2s ease;
  }
  cv-fleet .fleet-card:hover {
    border-color: color-mix(in srgb, var(--accent) 35%, var(--border));
    box-shadow: 0 1px 0 color-mix(in srgb, var(--accent) 14%, transparent) inset, 0 10px 24px -14px var(--accent);
  }
  cv-fleet .fleet-card h3 {
    display: flex; align-items: center; gap: 8px;
  }
  cv-fleet .fleet-card h3::before {
    content: ""; display: inline-block; width: 6px; height: 6px; border-radius: 50%;
    background: var(--accent); box-shadow: 0 0 8px -1px var(--accent);
  }

  /* toolbar polish */
  cv-fleet .fleet-toolbar {
    background: color-mix(in srgb, var(--bg-elev) 85%, transparent);
    backdrop-filter: blur(6px);
    border-radius: 10px;
  }
  cv-fleet .fleet-token {
    width: 120px; font: inherit; font-family: var(--mono); font-size: 12.5px;
    padding: 5px 9px; border-radius: 7px; border: 1px solid var(--border);
    background: var(--bg); color: var(--fg);
  }
  cv-fleet .fleet-base:focus, cv-fleet .fleet-token:focus {
    outline: none; border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
  }

  /* live connection dot: a soft ping ring when connected */
  cv-fleet .fleet-dot { position: relative; }
  cv-fleet .fleet-dot-ok::after {
    content: ""; position: absolute; inset: -4px; border-radius: 50%;
    border: 2px solid var(--ok); opacity: 0;
  }

  cv-fleet .fleet-lanes { grid-column: 1 / -1; }
  cv-fleet .fleet-attn-bar { display: flex; flex-wrap: wrap; gap: 6px; margin: 0 0 8px; }
  cv-fleet .fleet-attn-bar .mini-btn[aria-pressed="true"] { border-color: var(--accent); color: var(--accent); }
  cv-fleet .fleet-lane-feed { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; max-height: 360px; overflow: auto; }
  cv-fleet .fleet-lane { display: grid; grid-template-columns: 92px 1fr; gap: 4px 10px; padding: 6px 8px; border-radius: 8px; border: 1px solid var(--border); background: var(--bg-sunk); font-size: 13px; }
  cv-fleet .fleet-attn { font-family: var(--mono); font-size: 11.5px; font-weight: 650; }
  cv-fleet .fleet-attn-needs_you { color: var(--error); }
  cv-fleet .fleet-attn-working { color: var(--ok); }
  cv-fleet .fleet-attn-stale { color: var(--warn); }
  cv-fleet .fleet-attn-idle, cv-fleet .fleet-attn-done { color: var(--fg-muted); }
  cv-fleet .fleet-lane-meta { grid-column: 2; font-size: 12px; }
  cv-fleet .fleet-diverge { grid-column: 2; font-size: 12px; color: var(--warn); }
  cv-fleet .fleet-session[data-session] { cursor: pointer; }
  cv-fleet .fleet-session.on { border-color: var(--accent); }
  cv-fleet .fleet-session { transition: border-color 0.2s ease, box-shadow 0.2s ease; }
  cv-fleet .fleet-session:hover {
    border-color: color-mix(in srgb, var(--accent) 35%, var(--border));
    box-shadow: 0 4px 16px -10px var(--accent);
  }

  @media (prefers-reduced-motion: no-preference) {
    cv-fleet .fleet-dot-ok::after { animation: cv-fleet-ping 2.4s ease-out infinite; }
  }
  @keyframes cv-fleet-ping {
    0%   { transform: scale(0.6); opacity: 0.7; }
    80%  { transform: scale(1.6); opacity: 0; }
    100% { transform: scale(1.6); opacity: 0; }
  }
`;

function injectStyles() {
  if (document.getElementById("cv-fleet-styles")) return;
  const el = document.createElement("style");
  el.id = "cv-fleet-styles";
  el.textContent = FLEET_STYLES;
  document.head.appendChild(el);
}

class CvFleet extends HTMLElement {
  constructor() {
    super();
    this._base = this._read(BASE_LS) || DEFAULT_BASE;
    this._originTokens = new Map();
    this._paused = false;
    this._timer = null;
    this._inflight = false;

    // Last-known data + connection state.
    this._state = "idle"; // idle | ok | error
    this._health = null;
    this._err = "";
    this._sessions = [];
    this._lastUpdate = 0;
    this._staticMode = false; // loaded a sessions .json instead of live polling

    // Lanes of one session: chosen by click (remembered), else the most recent.
    this._lanesSession = this._read(LANES_LS);
    this._lanes = [];
    this._laneCounts = null;
    this._lanesErr = "";
    this._attn = "all";       // attention filter: "all" or an ATTENTION id
    this._es = null;          // EventSource on /api/stream, when available
    this._opening = null;     // the pending reader-cookie exchange that precedes the stream
    this._live = false;       // the stream is open: lanes arrive pushed, not polled
    this._lanesInflight = false;

    // Shell built? + per-section signatures of what's currently in the DOM, so
    // each poll can diff and skip untouched sections.
    this._shell = false;
    this._mode = null; // "dash" | "offline" — which content view is mounted
    this._last = {};   // section → signature string of last-rendered data
  }

  connectedCallback() {
    injectStyles();
    this.render();
    if (!this._paused) this._start();
  }
  disconnectedCallback() { this._stop(); }

  _read(k) { try { return localStorage.getItem(k) || ""; } catch { return ""; } }
  _write(k, v) { try { localStorage.setItem(k, v); } catch { /* ignore */ } }

  // ---- polling lifecycle -------------------------------------------------

  _start() {
    this._stop();
    this._staticMode = false;
    this._poll().then(() => { this._openStream(); if (!this._live) this._pollLanes(); });
    this._timer = setInterval(() => {
      if (this._paused) return;
      this._poll();
      if (!this._live) this._pollLanes();
    }, POLL_MS);
  }
  _stop() {
    if (this._timer) { clearInterval(this._timer); this._timer = null; }
    this._closeStream();
    // Abort any in-flight poll so its late resolution can't clobber the UI
    // after we've torn down / reconnected.
    if (this._ctrl) { try { this._ctrl.abort(); } catch { /* ignore */ } this._ctrl = null; }
    this._inflight = false;
  }

  _endpoint(path) {
    const base = (this._base || DEFAULT_BASE).trim().replace(/\/+$/, "");
    return new URL(base + path, document.baseURI);
  }

  _targetOrigin() {
    try {
      const url = this._endpoint("/api/health");
      return ["http:", "https:"].includes(url.protocol) ? url.origin : "";
    } catch {
      return "";
    }
  }

  _isSameOrigin() {
    if (typeof location === "undefined" || !location.origin || location.origin === "null") return false;
    return this._targetOrigin() === location.origin;
  }

  _headersForBase(extra = {}) {
    if (this._isSameOrigin()) return authHeaders(extra);
    const origin = this._targetOrigin();
    const scoped = origin ? (this._originTokens.get(origin) || "") : "";
    return scoped ? { ...extra, Authorization: `Bearer ${scoped}` } : { ...extra };
  }

  _tokenForCurrentBase() {
    if (this._isSameOrigin()) return getAuthToken();
    const origin = this._targetOrigin();
    return origin ? (this._originTokens.get(origin) || "") : "";
  }

  _setTokenForCurrentBase(tok) {
    const trimmed = (tok || "").trim();
    if (this._isSameOrigin()) {
      setAuthToken(trimmed);
      return;
    }
    const origin = this._targetOrigin();
    if (!origin) return;
    if (trimmed) this._originTokens.set(origin, trimmed);
    else this._originTokens.delete(origin);
  }

  async _fetchJson(path, signal) {
    const url = this._endpoint(path).href;
    const resp = await fetch(url, { signal, headers: this._headersForBase({ Accept: "application/json" }) });
    if (resp.status === 401) throw new Error(`${path} → HTTP 401 (enter bearer token in toolbar)`);
    if (!resp.ok) throw new Error(`${path} → HTTP ${resp.status}`);
    return resp.json();
  }

  async _poll() {
    if (this._inflight || this._staticMode) return;
    this._inflight = true;
    const ctrl = new AbortController();
    this._ctrl = ctrl;
    const to = setTimeout(() => ctrl.abort(), POLL_MS + 1500);
    try {
      const health = await this._fetchJson("/api/health", ctrl.signal);
      const sessions = await this._fetchJson(`/api/sessions?limit=${SESSION_LIMIT}`, ctrl.signal);
      if (this._ctrl !== ctrl) return;
      if (ctrl.signal.aborted) throw new Error("request timed out");
      if (!health || typeof health !== "object" || !Array.isArray(sessions?.sessions ?? sessions)) {
        throw new Error("Fleet API returned an invalid response");
      }
      this._health = health;
      this._state = "ok";
      this._err = "";
      this._sessions = this._asSessions(sessions);
      this._lastUpdate = Date.now();
    } catch (err) {
      // If this poll was superseded (reconnect/teardown aborted it), drop it
      // silently — its late result must not clobber the current view.
      if (this._ctrl !== ctrl) { clearTimeout(to); return; }
      this._state = "error";
      this._err = err?.name === "AbortError" ? "request timed out" : (err?.message || String(err));
    } finally {
      clearTimeout(to);
      // Only the current poll clears the inflight flag and repaints.
      if (this._ctrl === ctrl) {
        this._inflight = false;
        this._ctrl = null;
        this.render();
      }
    }
  }

  // ---- lanes: SSE first, polling as the fallback --------------------------

  /** The session whose lanes are shown: the clicked one, else the most recent. */
  _laneTarget() {
    return this._lanesSession || this._sessions[0]?.id || "";
  }

  _streamUrl() {
    const target = this._laneTarget();
    return this._endpoint("/api/stream?follow=1").href
      + (target ? `&lanes=${encodeURIComponent(target)}` : "");
  }

  _openStream() {
    this._closeStream();
    // A static export has no server; an environment without EventSource polls instead.
    if (this._staticMode || typeof EventSource !== "function" || !this._laneTarget()) return;
    const hdrs = this._headersForBase();
    if (!hdrs.Authorization) { this._attachStream(); return; }
    // EventSource sends no headers, and the server refuses a query-string bearer: exchange the
    // bearer for the HttpOnly reader cookie first. Until then (or if refused) lanes are polled.
    if (!this._isSameOrigin()) return;
    const opening = this._opening = {};
    fetch(this._endpoint("/api/auth").href, { headers: hdrs, credentials: "same-origin" })
      .then((resp) => { if (resp.ok && this._opening === opening && !this._es) this._attachStream(); })
      .catch(() => {});
  }

  _attachStream() {
    let es;
    try { es = new EventSource(this._streamUrl(), { withCredentials: this._isSameOrigin() }); } catch { return; }
    this._es = es;
    es.onopen = () => { if (this._es === es) this._live = true; };
    // EventSource retries on its own; until it reconnects, the poll timer fetches lanes.
    es.onerror = () => { if (this._es === es) this._live = false; };
    es.addEventListener("change", (e) => {
      if (this._es !== es) return;
      this._live = true;
      let data;
      try { data = JSON.parse(e.data); } catch { return; }
      if (data?.lanes) this._applyLanes(data.lanes);
      const kinds = Array.isArray(data?.kinds) ? data.kinds : [];
      if (kinds.includes("sessions") || kinds.includes("reset")) this._poll();
      else this.render();
    });
  }

  _closeStream() {
    this._opening = null;
    if (this._es) { try { this._es.close(); } catch { /* ignore */ } }
    this._es = null;
    this._live = false;
  }

  async _pollLanes() {
    const target = this._laneTarget();
    if (!target || this._lanesInflight || this._staticMode) return;
    this._lanesInflight = true;
    try {
      const body = await this._fetchJson(`/api/lanes?session=${encodeURIComponent(target)}`);
      if (target === this._laneTarget()) this._applyLanes(body);
    } catch (err) {
      if (target === this._laneTarget()) this._lanesErr = err?.message || String(err);
    } finally {
      this._lanesInflight = false;
      this.render();
      if (this._laneTarget() && this._laneTarget() !== target && !this._paused && !this._staticMode) {
        this._pollLanes();
      }
    }
  }

  /** Accept a `/api/lanes` body or the compact `lanes` object an SSE change carries. */
  _applyLanes(body) {
    if (!body || typeof body !== "object" || body.error) {
      this._lanesErr = String(body?.error || "lanes unavailable");
      return;
    }
    this._lanesErr = "";
    this._lanes = Array.isArray(body.lanes) ? body.lanes : [];
    this._laneCounts = body.counts && typeof body.counts === "object" ? body.counts : null;
  }

  _selectLanes(id) {
    if (!id || id === this._laneTarget()) return;
    this._lanesSession = id;
    this._write(LANES_LS, id);
    this._lanes = [];
    this._laneCounts = null;
    this._lanesErr = "";
    this.render();
    if (this._staticMode) return;
    this._openStream();
    this._pollLanes();
  }

  // ---- response normalizers (tolerant of small shape differences) --------

  _asSessions(data) {
    const arr = Array.isArray(data) ? data : Array.isArray(data?.sessions) ? data.sessions : [];
    return arr;
  }

  // ---- rendering ---------------------------------------------------------
  //
  // render() builds the shell once, then patches sections incrementally. It is
  // safe to call on every poll: untouched sections are diffed away.

  render() {
    if (!this._shell) this._buildShell();
    this._patchToolbar();

    const wantMode = (this._state === "error" && !this._staticMode) ? "offline" : "dash";
    const content = this._content;
    if (this._mode !== wantMode) {
      // Switch the mounted content view. This is the only place innerHTML is
      // used for content, and only when the *kind* of view changes.
      this._mode = wantMode;
      this._last = {}; // invalidate section signatures for the new view
      content.innerHTML = wantMode === "offline" ? this._offlineHtml() : this._dashShellHtml();
      this._wireContent();
    }

    if (wantMode === "offline") {
      this._patchOffline();
    } else {
      this._patchSessions();
      this._patchLanes();
    }
  }

  _buildShell() {
    this.innerHTML = `
      <div class="view-head">
        <h2>📡 Fleet</h2>
        <span class="muted">Live view of a running <code>cv serve</code> — recent sessions across every harness.</span>
      </div>
      <div class="fleet-toolbar">
        <span class="fleet-dot" title="connection"></span>
        <label class="fleet-field">
          <span class="muted">base URL</span>
          <input class="fleet-base" type="text" placeholder="${DEFAULT_BASE}" aria-label="cv serve base URL" />
        </label>
        <label class="fleet-field">
          <span class="muted">token</span>
          <input class="fleet-token" type="password" placeholder="optional" aria-label="Bearer token" autocomplete="off" />
        </label>
        <button type="button" class="mini-btn" data-fleet-connect>reconnect</button>
        <button type="button" class="mini-btn" data-fleet-pause aria-pressed="false">⏸ pause</button>
        <button type="button" class="mini-btn" data-fleet-refresh title="Poll now">⟳</button>
        <span class="fleet-stamp muted">—</span>
        <span class="fleet-ver muted" title="server version" hidden></span>
      </div>
      <div class="fleet-content"></div>
    `;
    this._shell = true;
    this._content = this.querySelector(".fleet-content");
    // The base-URL <input> value is owned by the user while focused; set it once.
    const baseEl = this.querySelector(".fleet-base");
    if (baseEl) baseEl.value = this._base;
    const tokEl = this.querySelector(".fleet-token");
    if (tokEl) tokEl.value = this._tokenForCurrentBase();
    this._wireToolbar();
  }

  // Patch the live status bits of the always-present toolbar without rebuilding
  // its inputs (which would steal focus).
  _patchToolbar() {
    const dot = this._staticMode ? "static" : this._state;
    const dotEl = this.querySelector(".fleet-dot");
    if (dotEl) dotEl.className = `fleet-dot fleet-dot-${esc(dot)}`;

    const stamp = this._lastUpdate
      ? `updated ${fmtTime(new Date(this._lastUpdate).toISOString()).replace(/^.*?, /, "")}`
      : "—";
    const stampEl = this.querySelector(".fleet-stamp");
    const stampText = this._staticMode ? "static export" : stamp;
    if (stampEl && stampEl.textContent !== stampText) stampEl.textContent = stampText;

    const verEl = this.querySelector(".fleet-ver");
    if (verEl) {
      const v = this._health?.version;
      if (v) { verEl.textContent = `v${v}`; verEl.hidden = false; }
      else { verEl.hidden = true; }
    }

    const connectEl = this.querySelector("[data-fleet-connect]");
    if (connectEl) connectEl.textContent = this._staticMode ? "go live" : "reconnect";
    const pauseEl = this.querySelector("[data-fleet-pause]");
    if (pauseEl) {
      pauseEl.textContent = this._paused ? "▶ resume" : "⏸ pause";
      pauseEl.setAttribute("aria-pressed", String(this._paused));
    }
  }

  // ---- offline view ------------------------------------------------------

  _offlineHtml() {
    return `
      <div class="fleet-offline">
        <div class="fo-glyph">🛰️</div>
        <h3>Can't reach the fleet API</h3>
        <p class="fo-err muted"></p>
        <p>Start the daemon's HTTP API and make sure CORS is enabled:</p>
        <pre><code>cv serve</code></pre>
        <p class="muted">Then click <strong>reconnect</strong>. The dashboard polls
        <code>/api/health</code> and <code>/api/sessions</code> every ${POLL_MS / 1000}s and follows
        lanes over <code>/api/stream</code>.</p>
        <div class="fo-static">
          <span class="muted">No server handy? Load a static sessions export:</span>
          <input type="file" class="fleet-static-input" accept=".json,application/json" aria-label="Load static sessions JSON" />
        </div>
      </div>`;
  }

  _patchOffline() {
    const sig = `${this._base}${this._err}`;
    if (this._last.offline === sig) return;
    this._last.offline = sig;
    const errEl = this._content.querySelector(".fo-err");
    if (errEl) errEl.innerHTML = `<code>${esc(this._base)}</code> — ${esc(this._err || "no response")}.`;
  }

  // ---- dashboard shell + per-section patches -----------------------------

  _dashShellHtml() {
    return `
      <div class="fleet-grid">
        <section class="fleet-card fleet-lanes">
          <h3>Lanes <span class="muted fleet-lanes-of"></span></h3>
          <div class="fleet-attn-bar" role="toolbar" aria-label="Filter lanes by attention"></div>
          <div class="fleet-lanes-body"></div>
        </section>
        <section class="fleet-card fleet-sessions">
          <h3>Recent sessions <span class="muted fleet-sessions-n"></span></h3>
          <div class="fleet-sessions-body"></div>
        </section>
      </div>`;
  }

  _patchSessions() {
    const sig = JSON.stringify(this._sessions.map((s) => [
      s.id, s.harness, sessionLabel(s), s.cwd, s.updated_at || s.created_at, msgCount(s),
    ]));
    const countEl = this._content.querySelector(".fleet-sessions-n");
    if (countEl) countEl.textContent = `(${this._sessions.length})`;
    if (this._last.sessions === sig) return;
    this._last.sessions = sig;
    this._content.querySelector(".fleet-sessions-body").innerHTML = this._sessionsHtml();
  }

  _patchLanes() {
    const sig = JSON.stringify([this._laneTarget(), this._attn, this._lanesErr, this._live, this._laneCounts, this._lanes]);
    if (this._last.lanes === sig) return;
    this._last.lanes = sig;
    const of = this._content.querySelector(".fleet-lanes-of");
    if (of) of.textContent = this._laneTarget() ? `of ${this._laneTarget().slice(0, 8)} · ${this._live ? "live" : "polling"}` : "";
    const bar = this._content.querySelector(".fleet-attn-bar");
    if (bar) bar.innerHTML = this._attnBarHtml();
    const body = this._content.querySelector(".fleet-lanes-body");
    if (body) body.innerHTML = this._lanesHtml();
  }

  // ---- section HTML builders ---------------------------------------------

  _attnBarHtml() {
    const counts = this._laneCounts?.attention || {};
    const count = (id) => (Number.isSafeInteger(counts[id]) ? counts[id] : this._lanes.filter((l) => l.attention === id).length);
    const button = (id, label, n) =>
      `<button type="button" class="mini-btn" data-attn="${esc(id)}" aria-pressed="${this._attn === id}">${esc(label)} <span class="muted">${n}</span></button>`;
    return button("all", "All", this._lanes.length) + ATTENTION.map(([id, label]) => button(id, label, count(id))).join("");
  }

  _lanesHtml() {
    if (!this._laneTarget()) return `<p class="muted fleet-empty">No session selected.</p>`;
    if (this._lanesErr) return `<p class="muted fleet-empty">Lanes unavailable: ${esc(this._lanesErr)}</p>`;
    const rank = (a) => { const i = ATTENTION.findIndex(([id]) => id === a); return i < 0 ? ATTENTION.length : i; };
    const shown = this._lanes
      .filter((l) => this._attn === "all" || l.attention === this._attn)
      .slice()
      .sort((a, b) => rank(a.attention) - rank(b.attention));
    if (!shown.length) {
      return `<p class="muted fleet-empty">${this._lanes.length ? "No lanes in this state." : "No sub-agents in this session."}</p>`;
    }
    const label = (id) => ATTENTION.find(([a]) => a === id)?.[1] || id || "—";
    return `<ul class="fleet-lane-feed">${shown.map((l) => `
      <li class="fleet-lane" data-attention="${esc(l.attention || "")}">
        <span class="fleet-attn fleet-attn-${esc(l.attention || "none")}">${esc(label(l.attention))}</span>
        <span class="fs-title" title="${esc(l.description || l.agent_id || "")}">${esc(l.description || l.agent_id || "(lane)")}</span>
        <span class="fleet-lane-meta muted">${esc(l.status || "")}${l.pending_ask ? " · waiting on an answer" : ""}${l.stranded ? " · stranded" : ""}${l.last_tool ? ` · ${esc(l.last_tool)}` : ""}${l.last_turn_at ? ` · ${esc(fmtTime(l.last_turn_at).replace(/^.*?, /, ""))}` : ""}</span>
        ${l.witness_divergence ? `<span class="fleet-diverge" title="hook record and transcript disagree">⚠ ${esc(l.witness_divergence)}</span>` : ""}
      </li>`).join("")}</ul>`;
  }

  _sessionsHtml() {
    if (!this._sessions.length) return `<p class="muted fleet-empty">No recent sessions reported.</p>`;
    return `<ul class="fleet-session-feed">${this._sessions.map((s) => {
      const h = (s.harness || "").toLowerCase();
      const when = fmtTime(s.updated_at || s.created_at);
      const n = msgCount(s);
      const on = s.id === this._laneTarget() ? " on" : "";
      return `
        <li class="fleet-session${on}" data-session="${esc(s.id)}" title="Show this session's lanes">
          <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
          <div class="fs-body">
            <div class="fs-title" title="${esc(sessionLabel(s))}">${esc(sessionLabel(s))}</div>
            <div class="fs-meta muted">
              ${s.cwd ? `<span class="fs-cwd" title="${esc(s.cwd)}">${esc(shortPath(s.cwd, 3))}</span>` : ""}
              ${n ? `<span>${n} msg</span>` : ""}
              ${when ? `<span>${esc(when.replace(/^.*?, /, ""))}</span>` : ""}
            </div>
          </div>
        </li>`;
    }).join("")}</ul>`;
  }

  // ---- wiring ------------------------------------------------------------
  //
  // Toolbar listeners are bound once (the toolbar is built once). Content
  // listeners (offline file input) are (re)bound whenever the content view
  // is swapped.

  _wireToolbar() {
    const baseEl = this.querySelector(".fleet-base");
    const tokEl = this.querySelector(".fleet-token");
    const syncBase = () => {
      this._base = baseEl.value.trim() || DEFAULT_BASE;
      this._write(BASE_LS, this._base);
      if (tokEl) tokEl.value = this._tokenForCurrentBase();
    };
    baseEl?.addEventListener("change", syncBase);
    baseEl?.addEventListener("keydown", (e) => { if (e.key === "Enter") { syncBase(); this._start(); } });

    tokEl?.addEventListener("change", () => { this._setTokenForCurrentBase(tokEl.value); });
    tokEl?.addEventListener("keydown", (e) => { if (e.key === "Enter") { this._setTokenForCurrentBase(tokEl.value); this._start(); } });

    this.querySelector("[data-fleet-connect]")?.addEventListener("click", () => {
      const nextBase = (this.querySelector(".fleet-base")?.value || this._base).trim() || DEFAULT_BASE;
      const baseChanged = nextBase !== this._base;
      this._base = nextBase;
      this._write(BASE_LS, this._base);
      if (tokEl) {
        if (baseChanged) tokEl.value = this._tokenForCurrentBase();
        else this._setTokenForCurrentBase(tokEl.value);
      }
      this._paused = false;
      this._start();
    });
    this.querySelector("[data-fleet-pause]")?.addEventListener("click", () => {
      this._paused = !this._paused;
      if (this._paused) this._stop();
      else this._start();
      this.render();
    });
    this.querySelector("[data-fleet-refresh]")?.addEventListener("click", () => {
      if (this._staticMode) this._start(); else this._poll();
    });
  }

  _wireContent() {
    // Delegated, once: section bodies are re-rendered by innerHTML, the content root is not.
    if (!this._contentWired) {
      this._contentWired = true;
      this._content.addEventListener("click", (e) => {
        const attn = e.target.closest?.("[data-attn]");
        if (attn) { this._attn = attn.dataset.attn; this.render(); return; }
        const row = e.target.closest?.("[data-session]");
        if (row) this._selectLanes(row.dataset.session);
      });
    }
    const fileEl = this._content.querySelector(".fleet-static-input");
    fileEl?.addEventListener("change", async () => {
      const f = fileEl.files?.[0];
      if (!f) return;
      try {
        const data = JSON.parse(await f.text());
        this._loadStatic(data);
      } catch (e) {
        this._err = "Could not parse that JSON: " + (e?.message || e);
        this.render();
      }
    });
  }

  // Load a static export: an array of sessions, or an object { sessions? }.
  _loadStatic(data) {
    this._stop();
    this._staticMode = true;
    this._paused = true;
    this._state = "ok";
    this._err = "";
    this._sessions = this._asSessions(Array.isArray(data) ? data : data?.sessions ?? []);
    // A saved `/api/lanes` body (`{ session, counts, lanes }`) may ride along.
    if (Array.isArray(data?.lanes)) {
      if (data.session) this._lanesSession = String(data.session);
      this._applyLanes(data);
    }
    this._lastUpdate = Date.now();
    this.render();
  }
}

customElements.define("cv-fleet", CvFleet);
