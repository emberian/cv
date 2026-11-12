// <cv-app> — top-level shell: header, multi-source dropzone/upload, a view
// switcher (tabs), and the active view. Sessions from every dropped source are
// merged into one pool; each view reads from that pool.
//
// Views:
//   sessions   — the classic two-pane list + transcript
//   timeline   — chronological cross-harness feed (<cv-timeline>)
//   compare    — side-by-side message diff (<cv-compare>)
//   stats      — dashboard (<cv-stats>)
//   forest     — session-structure explorer: agent forest, workflow lanes, tools, compaction (<cv-forest>)
//   splice     — splice composer (<cv-splice>)
//   fleet      — live `cv serve` dashboard (<cv-fleet>)
import "./cv-session-list.js";
import "./cv-transcript.js";
import "./cv-projects.js";
import "./cv-timeline.js";
import "./cv-compare.js";
import "./cv-stats.js";
import "./cv-forest.js";
import "./cv-splice.js";
import "./cv-fleet.js";
import { esc, normalizeSession, normalizeSessions, randomId, sessionKey } from "./util.js";
import { isTauri, listen, canInvokeNative } from "../tauri.js";
import {
  apiGet, hydrateSession, getMessages, getSessionHead, searchSessions, probeSearch,
  authHeaders, getAuthToken, setAuthToken, registerWasmStore, PAGE, CVD_BASE,
} from "./hydrate.js";

// The read API (the native `api` command in the desktop app, a local `cv serve` in a browser)
// exposes the machine's real local sessions. The main viewer prefers these over the bundled
// sample; `CVD_BASE` (from hydrate.js) resolves to the page's own origin when the dashboard is
// served by `cv serve --web`, else
// localhost:7777 — the same base every data fetch uses.

const VIEWS = [
  ["sessions", "Sessions", "🗂"],
  ["projects", "Projects", "◈"],
  ["timeline", "Timeline", "📈"],
  ["compare", "Compare", "🔍"],
  ["stats", "Stats", "📊"],
  ["forest", "Structure", "🌳"],
  ["splice", "Splice", "✨"],
  ["fleet", "Fleet", "📡"],
];

class CvApp extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._sources = [];        // [{ name, count }]
    this._wasmState = "loading";
    this._view = "sessions";
    this._isSample = true;     // true until the user loads their own data
    this._daemon = false;      // a server answered: the archive is already here
    this._searchMode = "local";
    this._authNeeded = false;
    this._authBarOpen = false;
    this._onAuthRequired = () => this._promptAuth();
  }

  connectedCallback() {
    this.render();
    window.addEventListener("cv:auth-required", this._onAuthRequired);
    this._init();
    this._wireKeyboard();
    this._wireTauri();
  }

  disconnectedCallback() {
    if (this._onKeydown) document.removeEventListener("keydown", this._onKeydown);
    window.removeEventListener("cv:auth-required", this._onAuthRequired);
    this._unlistenTauri?.();
  }

  async _init() {
    const main = await import("../main.js");
    this._ingestZip = main.ingestZip;
    this._ingestZipStateful = main.ingestZipStateful;
    this._loadSample = main.loadSample;

    // Load sessions FIRST, before the wasm gate — wasm is only needed for .zip ingest, and a stalled
    // wasm import under the tauri:// scheme must never block showing the user's real sessions.
    // First choice: the machine's real local sessions (native command in the desktop; a local `cv serve`
    // over HTTP in a browser). Only if that's unreachable/empty do we fall back to the sample.
    const loadedLocal = await this._tryLoadLocal();

    // Now settle the wasm status (for the ingest/status UI), independently of session loading.
    this._wasmState = await main.wasmReady();
    if (typeof main.getWasmStore === "function") {
      registerWasmStore(await main.getWasmStore());
    }
    this._updateStatus();
    if (loadedLocal) {
      // Ask once whether this daemon can search message text, so the search box promises the
      // right thing from the first keystroke rather than discovering it on the first miss.
      probeSearch().then((mode) => {
        this._searchMode = mode;
        if (this._list) this._list.searchMode = mode;
      });
      return;
    }

    try {
      const sample = await main.loadSample();
      if (!this._sessions.length) {
        this._sessions = normalizeSessions(sample);
        this._sources = [{ name: "sample", count: this._sessions.length }];
        this._isSample = true;
        this._refreshViews();
        this._setStatus(isTauri()
          ? "Showing the bundled sample — couldn't reach a local cv serve. Use File → Open, or drop .zip / .json files, to load your own."
          : "Showing the bundled sample dataset — drop one or more .zip / .json files to load your own.");
      }
    } catch (e) {
      console.warn("[clustervision] failed to load sample:", e);
    }
  }

  // ---- Local sessions via the read API ------------------------------------------------
  // Load the real on-disk sessions as lightweight *stubs* (metadata, no messages). The full
  // transcript is fetched lazily in `_showSession` when a row is opened, so startup is fast even
  // with thousands of sessions. Returns true if it populated the pool.
  async _tryLoadLocal() {
    try {
      // Desktop: the native `api` command (no HTTP). Browser: a user may be running `cv serve`;
      // try it, but never block startup on it. No limit: stubs are tiny, the list filters them.
      const refs = canInvokeNative()
        ? await apiGet("/api/sessions")
        : await this._fetchJsonWithTimeout(`${CVD_BASE}/api/sessions`, 4000);
      if (!refs) return false;
      if (!Array.isArray(refs) || !refs.length) return false;
      const sessions = normalizeSessions(refs).map((s) => ((s._stub = true), (s._remote = true), s));
      if (!sessions.length) return false;
      this._sessions = sessions;
      this._sources = [{ name: "local (cv serve)", count: sessions.length }];
      this._isSample = false;
      this._daemon = true;
      // The archive is already here, so the dropzone stops being the way in and folds to one
      // line. It still takes a drop; it just stops charging every view 110px for the offer.
      this._setDropzoneOpen(this._storedDropzoneOpen(), { silent: true });
      this._renderSources?.();
      this._refreshViews();
      this._setStatus(
        `${sessions.length.toLocaleString()} sessions from this machine — pick one to read it.`,
        "ok"
      );
      return true;
    } catch (e) {
      // cv serve not running (normal for the static web deploy) — caller falls back to the sample.
      console.info("[clustervision] no local cv serve, using fallback:", e?.message ?? e);
      return false;
    }
  }

  /** Whether a session is a not-yet-hydrated stub (metadata only, no messages loaded). */
  _needsHydration(s) {
    return !!(s && s._stub && !(s.messages && s.messages.length));
  }

  /** Fetch and swap in the transcript for a stub. Preferred: the *windowed* endpoint — load
   *  just the first PAGE messages so a 30k-message session opens instantly; the transcript's
   *  "load more" pages in the rest (`_loadMoreMessages`). Falls back to the classic whole-session
   *  fetch when the windowed path isn't available (older server). Returns the hydrated session. */
  async _hydrate(stub) {
    this._hydrations ||= new WeakMap();
    const pending = this._hydrations.get(stub);
    if (pending) return pending;
    const task = this._hydrateWindow(stub);
    this._hydrations.set(stub, task);
    try { return await task; }
    finally { this._hydrations.delete(stub); }
  }

  async _hydrateWindow(stub) {
    // Head scans through EOF, independently of the first useful message window.
    // Settle failure immediately so it cannot reject unhandled while the window is pending.
    const windowReady = getMessages(stub, 0, PAGE);
    const headReady = getSessionHead(stub).catch(() => null);
    const win = await windowReady;
    if (win) {
      const present = (o) => Object.fromEntries(Object.entries(o || {}).filter(([, v]) => v != null));
      const windowMeta = present(win.session);
      const full = normalizeSession({ ...stub, ...windowMeta, messages: win.messages });
      full._stub = false;
      full._wasm = !!stub._wasm;
      full._remote = !stub._wasm;
      full._totalKnown = !!win.total_known;
      if (win.total_known) full.message_count = win.total;
      if (win.has_more) full._paged = { next: win.end };
      const i = this._sessions.indexOf(stub);
      if (i >= 0) this._sessions[i] = full;
      headReady.then((head) => {
        if (!head) return;
        const current = this._sessions.find((x) => x.id === full.id && x.harness === full.harness);
        if (current && current !== full) return; // a newer pool entry owns this identity
        // Normalize metadata without copying or replacing already-loaded messages/pages.
        const meta = normalizeSession({ ...full, ...present(head), ...windowMeta, messages: [] });
        for (const [key, value] of Object.entries(meta)) {
          if (key !== "messages" && key !== "message_count") full[key] = value;
        }
        if (!full._totalKnown) {
          if (Number.isFinite(head.total)) full.message_count = head.total;
          else if (meta.message_count != null) full.message_count = meta.message_count;
        }
        if (this._transcript?.session === full) this._transcript.notifyMetadata();
      }).catch((error) => {
        console.warn("[clustervision] late session metadata failed:", error);
      });
      return full;
    }

    // Whole-session path (older server, or anything the windowed endpoint couldn't serve).
    const full = await hydrateSession(stub);
    const i = this._sessions.indexOf(stub);
    if (i >= 0) this._sessions[i] = full; // cache so we don't refetch
    return full;
  }

  /** "load more" from the transcript: fetch the next window and append it in place. */
  async _loadMoreMessages() {
    const s = this._transcript?.session;
    if (!s || !s._paged) return;
    // A page belongs to the cached session, not the current transcript DOM. Returning to
    // a session while its previous page is pending must share that request.
    this._pageLoads ||= new WeakMap();
    let task = this._pageLoads.get(s);
    if (!task) {
      const start = s._paged.next;
      task = (async () => {
        const win = await getMessages(s, start, start + PAGE);
        if (!win) throw new Error("Couldn't load more messages. Try again.");
        // A successful empty terminal page is valid; a non-advancing continuation is not.
        if (win.start !== start || !Number.isInteger(win.end) || win.end < start
          || win.end - start !== win.messages.length || (win.has_more && win.end === start)) {
          throw new Error("The message page was incomplete. Try again.");
        }
        const norm = normalizeSession({ id: s.id, harness: s.harness, messages: win.messages });
        s.messages.push(...norm.messages);
        if (win.total_known) { s.message_count = win.total; s._totalKnown = true; }
        s._paged = win.has_more ? { next: win.end } : null;
      })();
      this._pageLoads.set(s, task);
    }
    let error = null;
    try { await task; }
    catch (e) { error = e?.message || "Couldn't load more messages. Try again."; }
    finally {
      if (this._pageLoads.get(s) === task) this._pageLoads.delete(s);
    }
    if (this._transcript?.session === s) this._transcript.notifyAppended(error);
  }

  /** GET JSON with a hard timeout; returns null on any failure/timeout (never throws, never hangs). */
  async _fetchJsonWithTimeout(url, ms) {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), ms);
    try {
      const resp = await fetch(url, {
        headers: authHeaders({ Accept: "application/json" }),
        signal: ctrl.signal,
      });
      if (resp.status === 401) {
        this._promptAuth();
        return null;
      }
      return resp.ok ? await resp.json() : null;
    } catch {
      return null;
    } finally {
      clearTimeout(timer);
    }
  }

  _promptAuth() {
    this._authNeeded = true;
    this._authBarOpen = true;
    this._renderAuthBar();
  }

  _renderAuthBar() {
    const bar = this.querySelector(".auth-bar");
    if (!bar) return;
    const open = !!(this._authBarOpen || this._authNeeded);
    bar.hidden = !open;
    const note = bar.querySelector(".auth-note");
    if (note) {
      note.textContent = this._authNeeded
        ? "cv serve requires a bearer token (401 Unauthorized):"
        : "Bearer token for cv serve --token (sessionStorage only):";
    }
    const input = bar.querySelector(".auth-token-input");
    if (input && open && document.activeElement !== input) {
      input.value = getAuthToken();
    }
    this._syncChromeHeight();
  }

  /** Show a session in the transcript, hydrating from the read API first if it's only a stub.
   *
   *  Holding `j` asks for a new session every few milliseconds, and the fetches come back in
   *  whatever order they finish — so each one checks that it is still the session the user is
   *  on before it paints anything. Without that, a slow early transcript lands on top of the
   *  row the cursor has already moved to, and the selection jumps backwards. */
  async _showSession(session) {
    if (!session) return;
    const token = (this._showToken = (this._showToken || 0) + 1);
    let full = session;
    if (this._needsHydration(session)) {
      this._transcript.session = session; // render header immediately
      this._setStatus(`Loading transcript for ${session.title || session.id}…`);
      try {
        full = await this._hydrate(session);
      } catch (e) {
        if (token !== this._showToken) return;
        console.error("[clustervision] hydrate failed:", e);
        this._setStatus(`Couldn't load that transcript from the read API (${e?.message ?? e}).`, "error");
        return;
      }
      if (token !== this._showToken) return;   // the cursor has moved on
      this._setStatus("", "ok");
    }
    this._transcript.session = full;
    this._list.selectedSession = full;
  }

  // ---- Tauri: native File→Open pushes sessions via the cv://open-sessions event
  async _wireTauri() {
    if (!isTauri()) return;
    this._unlistenTauri = await listen("cv://open-sessions", (payload) => {
      try {
        const sessions = normalizeSessions(payload);
        if (!sessions.length) { this._setStatus("File → Open: no sessions in that payload.", "warn"); return; }
        if (this._isSample) { this._sessions = []; this._sources = []; this._isSample = false; }
        this._sessions = this._mergePool(this._sessions, sessions);
        this._sources.push({ name: "File → Open", count: sessions.length });
        this._renderSources();
        this._refreshViews();
        if (this._list && this._sessions[0] && !this._list.selectedId) {
          this._transcript.session = this._sessions[0];
          this._list.selectedSession = this._sessions[0];
        }
        this._setStatus(`Loaded ${sessions.length} session${sessions.length === 1 ? "" : "s"} from File → Open.`, "ok");
      } catch (err) {
        console.error("[clustervision] cv://open-sessions failed:", err);
        this._setStatus("Could not load the opened sessions.", "error");
      }
    });
  }

  render() {
    this.innerHTML = `
      <header class="app-header">
        <div class="brand">
          <span class="logo" aria-hidden="true">🔮</span>
          <div class="brand-text">
            <h1>clustervision</h1>
            <p class="tagline">Browse &amp; splice agent sessions — entirely in your browser.</p>
          </div>
        </div>
        <div class="header-actions">
          <button type="button" class="icon-btn token-btn" title="Bearer token for cv serve --token" aria-label="Bearer token">🔑</button>
          <button type="button" class="icon-btn help-btn" title="Keyboard shortcuts (?)" aria-label="Keyboard shortcuts">?</button>
          <button type="button" class="icon-btn theme-toggle" title="Toggle theme — dark / light / auto (t)" aria-label="Toggle theme">◐</button>
          <a class="repo-link" href="https://emberian.github.io/clustervision/manual/" target="_blank" rel="noopener" title="User manual">manual</a>
          <a class="repo-link" href="https://github.com/emberian/cv" target="_blank" rel="noopener" title="Project repository">source</a>
        </div>
      </header>

      <form class="auth-bar" hidden>
        <span class="auth-note">Bearer token for cv serve --token:</span>
        <input type="password" class="auth-token-input" placeholder="paste bearer token…" aria-label="Bearer token" autocomplete="off" />
        <button type="submit" class="mini-btn">Unlock</button>
        <button type="button" class="mini-btn" data-auth-clear>Clear</button>
        <button type="button" class="mini-btn" data-auth-close aria-label="Dismiss">✕</button>
      </form>

      <div class="dropzone" tabindex="0" role="button" aria-label="Upload or drop .zip / .json session files">
        <input type="file" class="file-input" accept=".zip,.json,application/zip,application/json" multiple hidden />
        <button type="button" class="dz-collapse" aria-label="Collapse the file drop area" title="Collapse">✕</button>
        <div class="dz-inner">
          <span class="dz-glyph" aria-hidden="true">📦</span>
          <div class="dz-text">
            <strong>Drop <code>.zip</code> or <code>.json</code> files here</strong>
            <span class="muted">multiple at once — harness <code>.zip</code>s and OpenSession <code>.json</code> files both load right here. Everything merges into one pool.</span>
          </div>
        </div>
        <div class="dz-sources" aria-live="polite"></div>
        <div class="dz-status muted" aria-live="polite"></div>
        <button type="button" class="dz-expand" aria-expanded="true">choose files…</button>
      </div>

      <nav class="view-tabs" role="tablist" aria-label="Views">
        ${VIEWS.map(([id, label, glyph]) => `
          <button type="button" class="view-tab${id === this._view ? " on" : ""}" role="tab"
            aria-selected="${id === this._view}" data-view="${id}">
            <span class="vt-glyph" aria-hidden="true">${glyph}</span>${esc(label)}
          </button>`).join("")}
      </nav>

      <main class="view-host"></main>

      <footer class="app-footer muted">
        <span>All parsing happens locally; nothing is uploaded. ·
        <a class="repo-link" href="https://emberian.github.io/clustervision/manual/opensession.html" target="_blank" rel="noopener">About OpenSession</a></span>
      </footer>
    `;

    this._dropzone = this.querySelector(".dropzone");
    this._fileInput = this.querySelector(".file-input");
    this._status = this.querySelector(".dz-status");
    this._sourcesEl = this.querySelector(".dz-sources");
    this._host = this.querySelector(".view-host");

    this.querySelectorAll(".view-tab").forEach((tab) =>
      tab.addEventListener("click", () => this._setView(tab.dataset.view)));

    this._wireDropzone();
    this._wireTheme();
    this._wireAuthBar();
    this.querySelector(".help-btn")?.addEventListener("click", () => this._toggleHelp());
    this._renderView();
    this._renderSources();
    this._syncChromeHeight();
    window.addEventListener("resize", () => this._syncChromeHeight());
  }

  // ---- dropzone: prominent on the demo, one line next to a daemon ---------

  /** The panes below are sized as `100vh - chrome`. Measure the chrome instead of guessing it,
   *  so folding the dropzone gives the list and transcript the space back. */
  _syncChromeHeight() {
    if (!this._host) return;
    const top = Math.round(this._host.getBoundingClientRect().top + (window.scrollY || 0));
    const footer = this.querySelector(".app-footer")?.offsetHeight || 0;
    const px = `${top + footer + 12}px`;
    if (px !== this._chromeH) { this._chromeH = px; this.style.setProperty("--chrome-h", px); }
  }

  /** Whether the user last left the fold open. Per-viewer convenience only, so a blocked or
   *  empty store just means "closed". */
  _storedDropzoneOpen() {
    try { return localStorage.getItem("cv-dropzone") === "open"; } catch { return false; }
  }

  /** Fold the dropzone to a single line (or unfold it). Only ever called once a daemon has
   *  answered — on the static demo the dropzone IS the way in and stays prominent. */
  _setDropzoneOpen(open, opts = {}) {
    if (!this._dropzone) return;
    this._dzOpen = !!open;
    this._dropzone.classList.toggle("compact", !open);
    this._dropzone.setAttribute("role", open ? "button" : "group");
    this._dropzone.tabIndex = open ? 0 : -1;
    const expand = this.querySelector(".dz-expand");
    if (expand) {
      expand.setAttribute("aria-expanded", String(!!open));
      expand.textContent = open ? "done" : "+ add .zip / .json";
    }
    if (!opts.silent) {
      try { localStorage.setItem("cv-dropzone", open ? "open" : "closed"); } catch { /* fine */ }
    }
    this._updateStatus();
    this._syncChromeHeight();
  }

  // ---- view switching ----------------------------------------------------

  _setView(view) {
    if (!VIEWS.some((v) => v[0] === view)) return;
    this._view = view;
    this.querySelectorAll(".view-tab").forEach((t) => {
      const on = t.dataset.view === view;
      t.classList.toggle("on", on);
      t.setAttribute("aria-selected", String(on));
    });
    this._renderView();
  }

  _renderView() {
    const host = this._host;
    host.className = "view-host view-" + this._view;
    switch (this._view) {
      case "sessions": host.innerHTML = ""; host.appendChild(this._sessionsView()); break;
      case "projects": host.innerHTML = `<cv-projects></cv-projects>`; break;
      case "timeline": host.innerHTML = `<cv-timeline></cv-timeline>`; break;
      case "compare": host.innerHTML = `<cv-compare></cv-compare>`; break;
      case "stats": host.innerHTML = `<cv-stats></cv-stats>`; break;
      case "forest": host.innerHTML = `<cv-forest></cv-forest>`; break;
      case "splice": host.innerHTML = `<cv-splice></cv-splice>`; break;
      case "fleet": host.innerHTML = `<cv-fleet></cv-fleet>`; break;
    }
    this._refreshViews();
  }

  // The classic two-pane sessions view, built once and cached.
  _sessionsView() {
    if (!this._sessionsLayout) {
      const layout = document.createElement("div");
      layout.className = "layout";
      layout.innerHTML = `
        <aside class="pane pane-list" aria-label="Sessions">
          <cv-session-list></cv-session-list>
        </aside>
        <section class="pane pane-transcript" aria-label="Transcript">
          <button type="button" class="back-btn" aria-label="Back to session list">← sessions</button>
          <cv-transcript></cv-transcript>
        </section>`;
      this._sessionsLayout = layout;
      this._list = layout.querySelector("cv-session-list");
      this._transcript = layout.querySelector("cv-transcript");

      this._list.addEventListener("select", (e) => {
        // `select` is also a native event: a text <input> fires one whenever its selection
        // changes, and it bubbles right through here. Only the list's own carries a session.
        if (!e.detail?.session) return;
        this._showSession(e.detail.session);
        layout.classList.add("show-transcript");
        layout.querySelector(".pane-transcript")?.scrollTo?.(0, 0);
      });
      this._transcript.addEventListener("load-more", () => this._loadMoreMessages());
      this._transcript.addEventListener("open-session-id", (e) => this._openById(e.detail));
      layout.querySelector(".back-btn").addEventListener("click", () => layout.classList.remove("show-transcript"));
    }
    return this._sessionsLayout;
  }

  // Push the current pool into whichever view is mounted.
  _refreshViews() {
    if (this._list) {
      if (!this._list._wiredSearch) {
        this._list._wiredSearch = true;
        // The list asks the daemon; `searchSessions` returns null when there is no daemon to ask,
        // which is the list's cue to go back to filtering the stubs it already holds.
        this._list.searchProvider = (q, o) => searchSessions(q, o);
        this._list.searchMode = this._searchMode;
      }
      this._list.sessions = this._sessions;
      if (!this._list.selectedId && this._sessions[0]) {
        // Don't auto-hydrate the first stub on load — it'd fire a fetch for a transcript the user
        // may never open. Just mark it selected; opening a row hydrates via `_showSession`.
        if (this._needsHydration(this._sessions[0])) {
          this._list.selectedSession = this._sessions[0];
        } else {
          this._transcript.session = this._sessions[0];
          this._list.selectedSession = this._sessions[0];
        }
      }
    }
    const pr = this._host?.querySelector("cv-projects"); if (pr) pr.sessions = this._sessions;
    const t = this._host?.querySelector("cv-timeline"); if (t) t.sessions = this._sessions;
    const c = this._host?.querySelector("cv-compare"); if (c) c.sessions = this._sessions;
    const st = this._host?.querySelector("cv-stats"); if (st) st.sessions = this._sessions;
    const fo = this._host?.querySelector("cv-forest"); if (fo) fo.sessions = this._sessions;
    const lo = this._host?.querySelector("cv-splice"); if (lo) lo.sessions = this._sessions;

    // Timeline + Projects → open a session in the sessions view.
    for (const sel of ["cv-timeline", "cv-projects"]) {
      const el = this._host?.querySelector(sel);
      if (el && !el._wired) {
        el._wired = true;
        el.addEventListener("open", (e) => this._openInSessions(e.detail.session));
      }
    }
  }

  /** A lineage chip was clicked: open the session it points at. The pool holds stubs for every
   *  local session, so an id usually resolves there (ids are full, but a harness may record a
   *  prefix, so accept one). Failing that, ask the read API for it directly — a forked-from or
   *  continued-in id can name a session the current filter never listed. */
  async _openById({ id, harness }) {
    if (!id) return;
    const candidates = this._sessions.filter((session) => !harness || session.harness === harness);
    const prefixes = candidates.filter((session) => session.id?.startsWith(id));
    const hit = candidates.find((session) => session.id === id) || (prefixes.length === 1 ? prefixes[0] : null);
    if (hit) { this._openInSessions(hit); return; }
    const token = (this._showToken = (this._showToken || 0) + 1);
    this._setStatus(`Looking up ${id}…`);
    try {
      const stub = normalizeSession({ id, harness, _stub: true });
      stub._stub = true;
      const full = await this._hydrate(stub);
      if (token !== this._showToken) return;
      this._sessions = this._mergePool(this._sessions, [full]);
      this._refreshViews();
      this._openInSessions(full);
      this._setStatus("", "ok");
    } catch {
      if (token !== this._showToken) return;
      this._setStatus(`No session ${id} in this archive — it may live on another machine.`, "warn");
    }
  }

  _openInSessions(session) {
    this._setView("sessions");
    const v = this._sessionsView();
    this._showSession(session);
    v.classList.add("show-transcript");
  }

  // ---- sources / theme / dropzone ---------------------------------------

  _renderSources() {
    if (!this._sourcesEl) return;
    if (!this._sources.length) { this._sourcesEl.innerHTML = ""; return; }
    this._sourcesEl.innerHTML = this._sources.map((s) =>
      `<span class="src-chip" title="${esc(s.name)}">${esc(s.name)} <b>${s.count}</b></span>`).join("");
  }

  _wireAuthBar() {
    const bar = this.querySelector(".auth-bar");
    const input = bar?.querySelector(".auth-token-input");
    this.querySelector(".token-btn")?.addEventListener("click", () => {
      this._authBarOpen = !this._authBarOpen;
      if (!this._authBarOpen) this._authNeeded = false;
      this._renderAuthBar();
      if (this._authBarOpen) input?.focus();
    });
    bar?.addEventListener("submit", async (e) => {
      e.preventDefault();
      setAuthToken(input?.value || "");
      this._authNeeded = false;
      this._authBarOpen = false;
      this._renderAuthBar();
      const loaded = await this._tryLoadLocal();
      if (loaded) {
        probeSearch().then((mode) => {
          this._searchMode = mode;
          if (this._list) this._list.searchMode = mode;
        });
      }
    });
    bar?.querySelector("[data-auth-clear]")?.addEventListener("click", () => {
      setAuthToken("");
      if (input) input.value = "";
      this._authNeeded = false;
      this._authBarOpen = false;
      this._renderAuthBar();
    });
    bar?.querySelector("[data-auth-close]")?.addEventListener("click", () => {
      this._authNeeded = false;
      this._authBarOpen = false;
      this._renderAuthBar();
    });
    this._renderAuthBar();
  }

  _wireTheme() {
    const root = document.documentElement;
    const stored = localStorage.getItem("cv-theme");
    if (stored) root.setAttribute("data-theme", stored);
    this.querySelector(".theme-toggle").addEventListener("click", () => {
      const cur = root.getAttribute("data-theme");
      const next = cur === "dark" ? "light" : cur === "light" ? "auto" : "dark";
      root.setAttribute("data-theme", next);
      localStorage.setItem("cv-theme", next);
    });
  }

  // ---- keyboard shortcuts + help overlay --------------------------------

  _wireKeyboard() {
    this._onKeydown = (e) => {
      // Don't hijack typing in inputs/textareas/contenteditable/selects.
      const t = e.target;
      const typing = t && (t.isContentEditable ||
        /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));

      if (e.key === "Escape") {
        if (this._helpOpen) { this._toggleHelp(false); e.preventDefault(); return; }
        // In the narrow sessions layout, Esc returns to the list.
        const layout = this._sessionsLayout;
        if (this._view === "sessions" && layout?.classList.contains("show-transcript")) {
          layout.classList.remove("show-transcript"); e.preventDefault(); return;
        }
        if (typing) t.blur();
        return;
      }

      if (typing) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;

      // "?" (Shift+/) toggles help.
      if (e.key === "?") { this._toggleHelp(); e.preventDefault(); return; }
      // "/" focuses the session search (switching to sessions first).
      if (e.key === "/") {
        if (this._view !== "sessions") this._setView("sessions");
        const search = this._list?.querySelector?.(".search");
        if (search) { search.focus(); search.select?.(); e.preventDefault(); }
        return;
      }
      // "t" cycles theme.
      if (e.key === "t") { this.querySelector(".theme-toggle")?.click(); e.preventDefault(); return; }
      // Number keys 1..N switch tabs.
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        if (VIEWS[idx]) { this._setView(VIEWS[idx][0]); e.preventDefault(); }
        return;
      }
      // j/k + arrows: move selection within the session list.
      if (this._view === "sessions" && ["j", "k", "ArrowDown", "ArrowUp"].includes(e.key)) {
        const dir = (e.key === "j" || e.key === "ArrowDown") ? 1 : -1;
        if (this._list?.moveSelection?.(dir)) e.preventDefault();
        return;
      }
    };
    document.addEventListener("keydown", this._onKeydown);
  }

  _toggleHelp(force) {
    this._helpOpen = force === undefined ? !this._helpOpen : !!force;
    let overlay = this.querySelector(".help-overlay");
    if (this._helpOpen && !overlay) {
      overlay = document.createElement("div");
      overlay.className = "help-overlay";
      overlay.setAttribute("role", "dialog");
      overlay.setAttribute("aria-modal", "true");
      overlay.setAttribute("aria-label", "Keyboard shortcuts");
      const rows = [
        ["1 – 8", "Switch view (Sessions, Projects, …, Structure, Splice, Fleet)"],
        ["/", "Focus session search"],
        ["j / k  ·  ↓ / ↑", "Move selection in the session list"],
        ["Enter", "Open the focused session"],
        ["t", "Cycle theme (dark → light → auto)"],
        ["Esc", "Close this help · back to the session list"],
        ["?", "Toggle this help"],
      ];
      overlay.innerHTML = `
        <div class="help-card">
          <div class="help-head"><h2>Keyboard shortcuts</h2>
            <button type="button" class="mini-btn help-close" aria-label="Close">esc ✕</button></div>
          <table class="help-table">${rows.map(([k, d]) =>
            `<tr><td class="help-key">${k.split(/\s+/).map((p) => /^[·–]$/.test(p) ? esc(p) : `<kbd>${esc(p)}</kbd>`).join(" ")}</td><td>${esc(d)}</td></tr>`).join("")}</table>
          <p class="help-foot muted">clustervision runs entirely in your browser — nothing is uploaded.</p>
        </div>`;
      overlay.addEventListener("click", (e) => { if (e.target === overlay) this._toggleHelp(false); });
      overlay.querySelector(".help-close").addEventListener("click", () => this._toggleHelp(false));
      this.appendChild(overlay);
      overlay.querySelector(".help-close").focus();
    } else if (!this._helpOpen && overlay) {
      overlay.remove();
    }
  }

  _wireDropzone() {
    const dz = this._dropzone;
    const pick = () => this._fileInput.click();

    this.querySelector(".dz-expand").addEventListener("click", (e) => {
      e.stopPropagation();
      // On the demo this button is the file picker; next to a daemon it is the fold.
      if (!this._daemon) { pick(); return; }
      this._setDropzoneOpen(!this._dzOpen);
    });
    this.querySelector(".dz-collapse").addEventListener("click", (e) => {
      e.stopPropagation();
      this._setDropzoneOpen(false);
    });

    dz.addEventListener("click", (e) => {
      if (e.target.closest("button") || e.target.tagName === "INPUT") return;
      if (dz.classList.contains("compact")) return;   // a one-line bar is not a 30px click target
      pick();
    });
    dz.addEventListener("keydown", (e) => {
      if (dz.classList.contains("compact")) return;
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); }
    });
    this._fileInput.addEventListener("change", () => {
      const files = [...(this._fileInput.files || [])];
      if (files.length) this._handleFiles(files);
      this._fileInput.value = "";
    });

    // Drag targets: the zone itself, and — once it is folded away — the whole app, so a dropped
    // .zip still lands somewhere instead of the browser navigating to it.
    const over = (e) => {
      if (!e.dataTransfer?.types?.includes?.("Files")) return;
      e.preventDefault();
      dz.classList.add("drag");
    };
    ["dragenter", "dragover"].forEach((ev) => {
      dz.addEventListener(ev, over);
      this.addEventListener(ev, over);
    });
    const leave = (e) => {
      if (e.type === "dragleave" && dz.contains(e.relatedTarget)) return;
      dz.classList.remove("drag");
    };
    ["dragleave", "drop"].forEach((ev) => { dz.addEventListener(ev, leave); this.addEventListener(ev, leave); });
    const drop = (e) => {
      const files = [...(e.dataTransfer?.files || [])];
      if (!files.length) return;
      e.preventDefault();
      e.stopPropagation(); // the dropzone is inside the app; ingest this drop once
      this._handleFiles(files);
    };
    dz.addEventListener("drop", drop);
    this.addEventListener("drop", drop);
  }

  async _handleFiles(files) {
    let added = 0, failed = 0, skippedTotal = 0;
    for (const file of files) {
      this._setStatus(`Reading ${file.name}…`);
      this._lastSkipped = [];
      try {
        const got = await this._ingestOne(file);
        skippedTotal += (this._lastSkipped?.length || 0);
        if (got.length) {
          // A failed/empty import must not discard the existing demo pool.
          if (this._isSample) { this._sessions = []; this._sources = []; this._isSample = false; }
          this._sessions = this._mergePool(this._sessions, got);
          this._sources.push({ name: file.name, count: got.length });
          added += got.length;
        } else {
          const skipNote = this._lastSkipped?.length ? ` (${this._lastSkipped.length} skipped)` : "";
          this._setStatus(`No sessions found in ${file.name}${skipNote}.`, "warn");
        }
      } catch (err) {
        console.error(err);
        failed++;
        this._setStatus(err?.message || `Failed to read ${file.name}.`, "error");
      }
    }

    this._renderSources();
    this._refreshViews();
    if (this._list && this._sessions[0] && !this._list.selectedId) {
      if (this._needsHydration(this._sessions[0])) {
        this._showSession(this._sessions[0]);
      } else {
        this._transcript.session = this._sessions[0];
        this._list.selectedSession = this._sessions[0];
      }
    }
    if (added) {
      const skipNote = skippedTotal ? ` (${skippedTotal} skipped)` : "";
      this._setStatus(`Pool now has ${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"} from ${this._sources.length} source${this._sources.length === 1 ? "" : "s"}${skipNote}.`, failed || skippedTotal ? "warn" : "ok");
    }
  }

  async _ingestOne(file) {
    const name = file.name || "file";
    if (/\.json$/i.test(name) || file.type === "application/json") {
      const text = await file.text();
      const data = JSON.parse(text);
      return normalizeSessions(data);
    }
    if (/\.zip$/i.test(name) || file.type === "application/zip") {
      const buf = new Uint8Array(await file.arrayBuffer());
      if (typeof this._ingestZipStateful === "function") {
        const res = await this._ingestZipStateful(buf);
        this._lastSkipped = res.skipped || [];
        const sessions = normalizeSessions(res.sessions);
        if (res.stateful) {
          for (const s of sessions) { s._stub = true; s._wasm = true; }
        }
        return sessions;
      }
      const raw = await this._ingestZip(buf);
      return normalizeSessions(raw);
    }
    throw new Error(`"${name}" isn't a .zip or .json file.`);
  }

  // Merge new sessions by harness + id (we keep the first
  // occurrence to preserve source ordering, but give a fresh id to any collision
  // so nothing is silently dropped). Colliding sessions are cloned rather than
  // mutated — the caller's objects keep their ids, so merging the same array
  // twice can't double-suffix.
  _mergePool(pool, incoming) {
    const out = pool.slice();
    const indices = new Map(pool.map((session, index) => [sessionKey(session), index]));
    for (const session of incoming) {
      const key = sessionKey(session);
      if (session._remote && indices.has(key)) {
        out[indices.get(key)] = session; // refresh source identity; never rename a remote id
        continue;
      }
      let add = session;
      while (add.id && indices.has(sessionKey(add))) add = { ...session, id: `${session.id}~${randomId().slice(0, 8)}` };
      indices.set(sessionKey(add), out.length);
      out.push(add);
    }
    return out;
  }

  _setStatus(msg, kind = "") {
    if (!this._status) return;
    this._status.textContent = msg;
    this._status.className = "dz-status muted" + (kind ? " " + kind : "");
  }

  /** The wasm bundle only matters for `.zip` ingest. Next to a daemon the archive is already
   *  loaded, so a missing bundle is not news — "Demo mode" over 6,873 of the user's own sessions
   *  was simply false. Say it where it actually stops someone: the demo deploy, where dropping a
   *  file is the only way in, and the fold when a user opens it to drop one. */
  _updateStatus() {
    if (this._wasmState !== "missing") return;
    if (this._daemon && !this._dzOpen) return;
    this._setStatus(
      this._daemon
        ? "This build can't unpack .zip archives — OpenSession .json files still load."
        : "Demo mode: .zip ingest isn't available in this build. You can still drop OpenSession .json files and explore the sample.",
      "warn",
    );
  }
}

customElements.define("cv-app", CvApp);
