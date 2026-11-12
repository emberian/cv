// <cv-fleet> — the Fleet Dashboard. 📡
//
// Connects to a running `cv serve` HTTP API and live-displays the agent fleet
// by polling every ~2s. CORS is enabled server-side, so plain cross-origin
// `fetch` works from this static page.
//
// Endpoints (base URL default http://localhost:7777):
//   GET /api/health                 → { ok, version?, ... }
//   GET /api/sessions?limit=N       → Session[] (IR) — recent activity feed
//
// Renders the recent-sessions activity feed, auto-refreshing with a pause
// toggle. If the API is unreachable, a friendly "start `cv serve`" hint (and a
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
    this._poll();
    this._timer = setInterval(() => { if (!this._paused) this._poll(); }, POLL_MS);
  }
  _stop() {
    if (this._timer) { clearInterval(this._timer); this._timer = null; }
    // Abort any in-flight poll so its late resolution can't clobber the UI
    // after we've torn down / reconnected.
    if (this._ctrl) { try { this._ctrl.abort(); } catch { /* ignore */ } this._ctrl = null; }
    this._inflight = false;
  }

  async _fetchJson(path, signal) {
    const url = this._base.replace(/\/+$/, "") + path;
    const resp = await fetch(url, { signal, headers: authHeaders({ Accept: "application/json" }) });
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
    if (tokEl) tokEl.value = getAuthToken();
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
        <code>/api/health</code> and <code>/api/sessions</code> every ${POLL_MS / 1000}s.</p>
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

  // ---- section HTML builders ---------------------------------------------

  _sessionsHtml() {
    if (!this._sessions.length) return `<p class="muted fleet-empty">No recent sessions reported.</p>`;
    return `<ul class="fleet-session-feed">${this._sessions.map((s) => {
      const h = (s.harness || "").toLowerCase();
      const when = fmtTime(s.updated_at || s.created_at);
      const n = msgCount(s);
      return `
        <li class="fleet-session">
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
    baseEl?.addEventListener("change", () => { this._base = baseEl.value.trim() || DEFAULT_BASE; this._write(BASE_LS, this._base); });
    baseEl?.addEventListener("keydown", (e) => { if (e.key === "Enter") { this._base = baseEl.value.trim() || DEFAULT_BASE; this._write(BASE_LS, this._base); this._start(); } });

    const tokEl = this.querySelector(".fleet-token");
    tokEl?.addEventListener("change", () => { setAuthToken(tokEl.value); });
    tokEl?.addEventListener("keydown", (e) => { if (e.key === "Enter") { setAuthToken(tokEl.value); this._start(); } });

    this.querySelector("[data-fleet-connect]")?.addEventListener("click", () => {
      this._base = (this.querySelector(".fleet-base")?.value || this._base).trim() || DEFAULT_BASE;
      this._write(BASE_LS, this._base);
      if (tokEl) setAuthToken(tokEl.value);
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
    this._lastUpdate = Date.now();
    this.render();
  }
}

customElements.define("cv-fleet", CvFleet);
