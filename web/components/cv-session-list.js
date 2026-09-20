// <cv-session-list> — sortable/filterable list of sessions, rendered as a WINDOW.
//
// API:
//   list.sessions = Session[]          (setter; triggers re-render)
//   list.searchProvider = async (q, { harness, semantic, signal }) => rows | null
//   list.searchMode = "server" | "local"
//   emits "select" CustomEvent with detail = { session } when a row is chosen
//
// Two things make this component more than a `.map()`:
//
//  1. It is windowed. An archive of 6,873 sessions put 54,255 nodes on the page to show ten rows,
//     and every keystroke rebuilt all of them. Only the rows near the viewport exist now; the
//     <ul> is stretched to the height the whole result set would occupy and each live row is
//     placed at its own offset, so the scrollbar still measures the corpus, not the window.
//
//  2. It searches two different ways and says which. With a daemon, `/api/search` reads every
//     message in the archive. Without one (the static demo, an older cvd), all this page has is
//     the metadata stubs it downloaded, so it filters those — which cannot match a message it
//     never had. The placeholder names whichever is true.
import {
  esc, fmtTime, sortTime, searchableText, sessionLabel, shortPath, msgCount, harnessBadge,
  truncate, normalizeSession, HARNESS_LABELS,
} from "./util.js";

/** Rows rendered beyond the visible band, so a fast scroll finds content already painted. */
const OVERSCAN = 6;
/** How many hits to ask the daemon for. The list is windowed, so this is about the honesty of
 *  "top N", not about what the DOM can take. */
const SEARCH_LIMIT = 60;

class CvSessionList extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._rows = [];
    this._rowsById = new Map();
    this._query = "";
    this._harnessFilter = new Set(); // empty = all
    this._semantic = false;
    this._sort = "recent";
    this._sortTouched = false;   // until the user picks one, the sort follows the mode
    this._selectedId = null;
    this._focusedId = null;
    this._searchCache = new WeakMap();
    this._searchMode = "local";
    this._searchProvider = null;
    this._start = -1; this._end = -1; this._extra = null;
    this._result = { kind: "all", n: 0, truncated: false };
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    this._searchCache = new WeakMap();
    // Drop any harness filters that no longer apply.
    const present = new Set(this._sessions.map((s) => (s.harness || "").toLowerCase()));
    for (const h of [...this._harnessFilter]) if (!present.has(h)) this._harnessFilter.delete(h);
    this.render();
  }
  get sessions() { return this._sessions; }

  /** Async full-text search, when the deployment has one. `null` from the provider means the
   *  endpoint isn't there and the component should go back to filtering what it holds. */
  set searchProvider(fn) { this._searchProvider = typeof fn === "function" ? fn : null; }

  /** "server" once a daemon has answered a search probe; "local" otherwise. Changing it only
   *  re-labels the controls — results are recomputed on the next query. */
  set searchMode(mode) {
    const next = mode === "server" && this._searchProvider ? "server" : "local";
    if (next === this._searchMode) return;
    this._searchMode = next;
    if (this.isConnected) this.render();
  }
  get searchMode() { return this._searchMode; }

  set selectedId(id) {
    this._selectedId = id;
    this._layout({ force: true });
  }
  get selectedId() { return this._selectedId; }

  connectedCallback() {
    this._clip = undefined;
    this.render();
    this._wireViewport();
  }

  disconnectedCallback() {
    this._unwireViewport();
    this._abort?.abort();
    clearTimeout(this._searchTimer);
  }

  // ---- filtering -----------------------------------------------------------

  _search(session) {
    let v = this._searchCache.get(session);
    if (v === undefined) {
      v = searchableText(session);
      this._searchCache.set(session, v);
    }
    return v;
  }

  _cmp() {
    return {
      recent: (a, b) => sortTime(b) - sortTime(a),
      oldest: (a, b) => sortTime(a) - sortTime(b),
      title: (a, b) => sessionLabel(a).localeCompare(sessionLabel(b)),
      messages: (a, b) => (msgCount(b) - msgCount(a)),
      relevance: (a, b) => (b.score ?? 0) - (a.score ?? 0),
    }[this._sort] || ((a, b) => 0);
  }

  /** The pool, narrowed by the harness chips and (in local mode) the query. */
  _filteredLocal() {
    const q = this._query.trim().toLowerCase();
    const rows = this._sessions.filter((s) => {
      const h = (s.harness || "").toLowerCase();
      if (this._harnessFilter.size && !this._harnessFilter.has(h)) return false;
      if (q && !this._search(s).includes(q)) return false;
      return true;
    });
    return rows.slice().sort(this._cmp());
  }

  /** Recompute the result set. In server mode with a query this goes to the daemon; everything
   *  else is answered from the pool, synchronously. */
  /** A ranked search should open on its best hit; a browse should open on the newest session.
   *  Follow whichever is happening until the user states a preference. */
  _autoSort() {
    if (this._sortTouched) return;
    const want = this._searchMode === "server" && this._query.trim() ? "relevance" : "recent";
    if (this._sort === want) return;
    this._sort = want;
    const sel = this.querySelector("select");
    if (sel && [...sel.options].some((o) => o.value === want)) sel.value = want;
  }

  _refresh() {
    this._autoSort();
    const q = this._query.trim();
    if (this._searchMode === "server" && this._searchProvider && q) {
      this._pending = this._serverSearch(q);
      return;
    }
    this._pending = null;
    this._abort?.abort();
    this._abort = null;
    const rows = this._filteredLocal();
    this._applyResults(rows, {
      kind: q ? "local-filter" : "all",
      n: rows.length,
      truncated: false,
    });
  }

  /** Resolves once the current query has produced its result set — immediately for a local
   *  filter, and when the daemon answers for a search. */
  whenSettled() { return this._pending || Promise.resolve(); }

  /** Changing the sort reorders what is already here. Only a local filter has to be recomputed
   *  (its result set is the pool), and a server search must never be re-issued for it — the hits
   *  are in hand, and the daemon would just return the same sixty. */
  _resort() {
    const kind = this._result.kind;
    if (kind === "search" || kind === "semantic") {
      this._rows = this._rows.slice().sort(this._cmp());
      this._rowsById = new Map(this._rows.map((r) => [r.id || "", r]));
      this._layout({ force: true });
      return;
    }
    this._refresh();
  }

  async _serverSearch(q) {
    this._abort?.abort();
    const ctrl = new AbortController();
    this._abort = ctrl;
    const seq = (this._seq = (this._seq || 0) + 1);
    this._setCount(`searching ${this._sessions.length.toLocaleString()} sessions for “${esc(q)}”…`, true);
    const only = this._harnessFilter.size === 1 ? [...this._harnessFilter][0] : null;
    let raw;
    try {
      raw = await this._searchProvider(q, {
        harness: only, semantic: this._semantic, limit: SEARCH_LIMIT, signal: ctrl.signal,
      });
    } catch (e) {
      if (e?.name === "AbortError") return;          // superseded by a later keystroke
      raw = null;
    }
    if (seq !== this._seq) return;
    if (raw == null) {
      // The endpoint went away (or never existed). Say so once, then behave like the demo.
      this._searchMode = "local";
      this.render();
      return;
    }
    const rows = raw.map((r) => this._asRow(r)).filter((r) => {
      if (!this._harnessFilter.size) return true;
      return this._harnessFilter.has((r.harness || "").toLowerCase());
    });
    if (this._sort !== "relevance") rows.sort(this._cmp());
    this._applyResults(rows, {
      kind: this._semantic ? "semantic" : "search",
      n: rows.length,
      truncated: raw.length >= SEARCH_LIMIT,
    });
  }

  /** A `/api/search` row → a pool-shaped stub that `cv-app` can hydrate on click, keeping the
   *  three search-only facts (`score`, `snippet`, sub-agent provenance) the normalizer drops. */
  _asRow(r) {
    const known = this._sessions.find((s) => s.id === r.id && s.harness === r.harness);
    const base = known || normalizeSession(r);
    const row = known ? { ...known } : base;
    if (!known) row._stub = true;
    row.score = r.score ?? null;
    row.snippet = r.snippet ?? null;
    row.agent_id = r.agent_id ?? null;
    row.parent_id = r.parent_id ?? null;
    row.workflow = r.workflow ?? null;
    // A search hit may know a count the stub does not, and vice versa — keep whichever exists.
    if (row.message_count == null && r.message_count != null) row.message_count = r.message_count;
    return row;
  }

  _applyResults(rows, result) {
    const changed = result.kind !== this._result.kind || rows.length !== this._rows.length;
    this._rows = rows;
    this._rowsById = new Map(rows.map((r) => [r.id || "", r]));
    this._result = result;
    this._hasSnippets = rows.some((r) => r.snippet);
    this._ul?.classList.toggle("has-snippets", !!this._hasSnippets);
    this._strideCache = null;
    this._renderCount();
    // A new result set starts at the top; the old scroll offset pointed into a different list.
    if (changed) {
      const clip = this._clipParent();
      if (clip) clip.scrollTop = 0;
    }
    this._layout({ force: true });
  }

  // ---- rendering -----------------------------------------------------------

  render() {
    const present = [...new Set(this._sessions.map((s) => (s.harness || "").toLowerCase()))]
      .filter(Boolean)
      .sort();

    const harnessChips = present.map((h) => {
      const on = this._harnessFilter.has(h);
      return `<button type="button" class="chip${on ? " on" : ""}" data-harness="${esc(h)}"
        aria-pressed="${on}">${esc(HARNESS_LABELS[h] || h)}</button>`;
    }).join("");

    const server = this._searchMode === "server";
    const semChip = server
      ? `<button type="button" class="chip${this._semantic ? " on" : ""}" data-semantic
          aria-pressed="${this._semantic}"
          title="Search by meaning instead of by word — needs cv index --semantic">≈ meaning</button>`
      : "";

    const sortOpts = [
      ...(server ? [["relevance", "Best match"]] : []),
      ["recent", "Most recent"],
      ["oldest", "Oldest"],
      ["title", "Title A–Z"],
      ["messages", "Most messages"],
    ];
    if (!sortOpts.some(([v]) => v === this._sort)) this._sort = sortOpts[0][0];

    this.innerHTML = `
      <div class="list-controls">
        <input type="search" class="search" placeholder="${esc(this._placeholder())}"
          aria-label="Search sessions" value="${esc(this._query)}" />
        <div class="control-row">
          <div class="chips" role="group" aria-label="Filter by harness">${harnessChips || '<span class="muted">no sessions</span>'}${semChip}</div>
          <label class="sort">
            <span class="muted">Sort</span>
            <select aria-label="Sort sessions">
              ${sortOpts.map(([v, l]) => `<option value="${v}">${esc(l)}</option>`).join("")}
            </select>
          </label>
        </div>
        <div class="list-count muted" aria-live="polite"></div>
      </div>
      <ul class="session-rows" role="listbox" aria-label="Sessions"></ul>
    `;

    this._ul = this.querySelector(".session-rows");
    this._countEl = this.querySelector(".list-count");
    this.querySelector("select").value = this._sort;

    // Wire events.
    const search = this.querySelector(".search");
    // Debounce: a keystroke either scans every stub or opens a request, and neither should
    // happen once per character.
    search.addEventListener("input", () => {
      this._query = search.value;
      clearTimeout(this._searchTimer);
      // A local keystroke costs a scan; a server keystroke costs a request. Wait longer before
      // spending the second one.
      const wait = this._searchMode === "server" && search.value.trim() ? 260 : 140;
      this._searchTimer = setTimeout(() => this._refresh(), wait);
    });
    this.querySelector("select").addEventListener("change", (e) => {
      this._sort = e.target.value;
      this._sortTouched = true;
      this._resort();
    });
    this.querySelectorAll(".chip").forEach((btn) => {
      btn.addEventListener("click", () => {
        if (btn.hasAttribute("data-semantic")) {
          this._semantic = !this._semantic;
          btn.classList.toggle("on", this._semantic);
          btn.setAttribute("aria-pressed", String(this._semantic));
          this._refresh();
          return;
        }
        const h = btn.dataset.harness;
        const on = this._harnessFilter.has(h);
        if (on) this._harnessFilter.delete(h);
        else this._harnessFilter.add(h);
        // Toggle just this chip + re-filter the rows — no full component rebuild.
        btn.classList.toggle("on", !on);
        btn.setAttribute("aria-pressed", String(!on));
        this._refresh();
      });
    });
    this._wireRows();
    this._start = -1; this._end = -1; this._extra = null;
    this._strideCache = null;
    this._refresh();
  }

  /** What the search box can actually do here, in words. */
  _placeholder() {
    const n = this._sessions.length;
    const many = n ? n.toLocaleString() : "";
    if (this._searchMode === "server") {
      return n ? `Search all ${many} sessions — message text, titles, paths` : "Search every session";
    }
    // Local: the filter only sees what this page downloaded.
    const hydrated = this._sessions.some((s) => s.messages && s.messages.length);
    if (hydrated) return n ? `Filter ${many} loaded sessions — messages, titles, paths` : "Filter sessions";
    return n ? `Filter ${many} sessions by title, path or harness` : "Filter sessions";
  }

  _renderCount() {
    const total = this._sessions.length;
    const n = this._result.n;
    const q = this._query.trim();
    let html;
    if (this._result.kind === "all") {
      html = `<b>${total.toLocaleString()}</b> session${total === 1 ? "" : "s"}`;
    } else if (this._result.kind === "local-filter") {
      html = `<b>${n.toLocaleString()}</b> of ${total.toLocaleString()} — titles &amp; paths only`;
    } else {
      const how = this._result.kind === "semantic" ? "by meaning" : "in message text";
      html = this._result.truncated
        ? `top <b>${n.toLocaleString()}</b> matches ${how} · <mark>${esc(truncate(q, 28))}</mark>`
        : `<b>${n.toLocaleString()}</b> match${n === 1 ? "" : "es"} ${how} · <mark>${esc(truncate(q, 28))}</mark>`;
    }
    this._setCount(html, false);
  }

  _setCount(html, busy) {
    if (!this._countEl) return;
    this._countEl.innerHTML = html;
    this._countEl.classList.toggle("list-busy", !!busy);
  }

  // ---- the window ----------------------------------------------------------

  /** Nearest scrolling ancestor, or null when the document itself scrolls. Cached per mount. */
  _clipParent() {
    if (this._clip !== undefined) return this._clip;
    let el = this.parentElement;
    while (el && el !== document.body && el !== document.documentElement) {
      const o = getComputedStyle(el).overflowY;
      if (o === "auto" || o === "scroll" || o === "overlay") { this._clip = el; return el; }
      el = el.parentElement;
    }
    this._clip = null;
    return null;
  }

  /** One row's pitch, read back from the same CSS custom property that sizes the row box. */
  _stride() {
    if (this._strideCache) return this._strideCache;
    const v = parseFloat(getComputedStyle(this._ul).getPropertyValue("--cv-row-stride"));
    this._strideCache = Number.isFinite(v) && v > 8 ? v : 68;
    return this._strideCache;
  }

  _wireViewport() {
    this._onViewport = () => {
      if (this._raf) return;
      this._raf = requestAnimationFrame(() => { this._raf = 0; this._layout(); });
    };
    // Capture, so a scroll inside ANY ancestor (the list pane, or the document at phone width)
    // reaches us — scroll events do not bubble.
    window.addEventListener("scroll", this._onViewport, { passive: true, capture: true });
    window.addEventListener("resize", this._onViewport);
    if (typeof ResizeObserver !== "undefined") {
      this._ro = new ResizeObserver(() => { this._strideCache = null; this._onViewport(); });
      this._ro.observe(this);
      const clip = this._clipParent();
      if (clip) this._ro.observe(clip);
    }
  }

  _unwireViewport() {
    if (this._onViewport) {
      window.removeEventListener("scroll", this._onViewport, { capture: true });
      window.removeEventListener("resize", this._onViewport);
    }
    this._ro?.disconnect();
    this._ro = null;
    if (this._raf) cancelAnimationFrame(this._raf);
    this._raf = 0;
  }

  /** Place the <ul> at full corpus height and realize only the rows the viewport can reach. */
  _layout(opts = {}) {
    const ul = this._ul;
    if (!ul || !ul.isConnected) return;
    const rows = this._rows;
    const total = rows.length;

    if (!total) {
      if (this._start !== -2 || opts.force) {
        ul.style.height = "";
        ul.innerHTML = `<li class="empty muted">${this._query.trim()
          ? "Nothing matches that." : "No sessions loaded."}</li>`;
        this._start = -2; this._end = -2; this._extra = null;
      }
      return;
    }

    const stride = this._stride();
    ul.style.height = `${total * stride - 6}px`;
    // The sticky controls sit over the top of the scroll box; keyboard scrolling must clear them.
    const sticky = this.querySelector(".list-controls")?.offsetHeight || 0;
    ul.style.setProperty("--cv-sticky-h", `${sticky}px`);

    const ulTop = ul.getBoundingClientRect().top;
    let top = 0;
    let bottom = window.innerHeight || document.documentElement.clientHeight || 800;
    const clip = this._clipParent();
    if (clip) {
      const r = clip.getBoundingClientRect();
      top = Math.max(top, r.top);
      bottom = Math.min(bottom, r.bottom);
    }
    let start = Math.floor((top - ulTop) / stride) - OVERSCAN;
    let end = Math.ceil((bottom - ulTop) / stride) + OVERSCAN;
    start = Math.max(0, Math.min(start, total));
    end = Math.max(start, Math.min(end, total));

    // Keep the selected row realized wherever it is, so focus and the j/k cursor always have a
    // node to land on even when the user has scrolled a thousand rows away from it.
    let extra = null;
    if (this._selectedId != null) {
      const i = rows.findIndex((r) => (r.id || "") === this._selectedId);
      if (i >= 0 && (i < start || i >= end)) extra = i;
    }

    if (!opts.force && start === this._start && end === this._end && extra === this._extra) return;
    this._start = start; this._end = end; this._extra = extra;

    let html = "";
    for (let i = start; i < end; i++) html += this._rowHtml(rows[i], i, stride);
    if (extra != null) html += this._rowHtml(rows[extra], extra, stride);
    ul.innerHTML = html;

    // Replacing the window's innerHTML drops whatever had focus onto <body>. Put it back, so
    // Enter still opens the row a user is sitting on when a scroll re-renders underneath them.
    if (this._focusedId != null && document.activeElement === document.body) {
      this._liFor(this._focusedId)?.focus({ preventScroll: true });
    }
  }

  _rowHtml(s, i, stride) {
    const id = s.id || "";
    const sel = id && id === this._selectedId ? " selected" : "";
    const when = fmtTime(s.updated_at || s.created_at);
    const count = msgCount(s);
    const cwd = s.cwd ? `<span class="row-cwd" title="${esc(s.cwd)}">${esc(shortPath(s.cwd))}</span>` : "";
    const agent = s.agent_id ? `<span class="row-agent" title="sub-agent of ${esc(s.parent_id || "?")}">sub-agent</span>` : "";
    const snippet = s.snippet
      ? `<div class="row-snippet">${this._markup(s.snippet)}</div>` : "";
    return `
      <li class="session-row${sel}" role="option" aria-selected="${!!sel}" aria-setsize="${this._rows.length}"
          aria-posinset="${i + 1}" data-id="${esc(id)}" tabindex="0" style="top:${i * stride}px">
        <div class="row-top">
          ${harnessBadge((s.harness || "").toLowerCase())}
          <span class="row-title">${esc(sessionLabel(s))}</span>
        </div>
        <div class="row-meta muted">
          ${cwd}
          ${when ? `<span class="row-when">${esc(when)}</span>` : ""}
          <span class="row-count">${count ? `${count} msg${count === 1 ? "" : "s"}` : "—"}</span>
          ${agent}
        </div>
        ${snippet}
      </li>`;
  }

  /** Escape the daemon's snippet, then mark the query's own words inside it. */
  _markup(text) {
    const safe = esc(String(text));
    const terms = this._query.trim().split(/\s+/).filter((t) => t.length > 1).slice(0, 6);
    if (!terms.length) return safe;
    const re = new RegExp(`(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "gi");
    return safe.replace(re, "<mark>$1</mark>");
  }

  _liFor(id) {
    return id == null ? null : this._ul?.querySelector(`.session-row[data-id="${CSS.escape(id)}"]`);
  }

  _wireRows() {
    // Event delegation: one listener on the <ul>, not two per row — and the listener survives
    // every window re-render, because only the <ul>'s children are replaced.
    const ul = this._ul;
    if (!ul || ul._delegated) return;
    ul._delegated = true;
    const rowId = (e) => e.target.closest(".session-row")?.dataset.id;
    ul.addEventListener("click", (e) => {
      const id = rowId(e);
      if (id) this._selectById(id);
    });
    ul.addEventListener("focusin", (e) => {
      const id = e.target.closest(".session-row")?.dataset.id;
      if (id) this._focusedId = id;
    });
    ul.addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const id = rowId(e);
      if (id) { e.preventDefault(); this._selectById(id); }
    });
  }

  _selectById(id) {
    const session = this._rowsById.get(id) || this._sessions.find((s) => (s.id || "") === id);
    if (!session) return;
    this._selectedId = id;
    this._updateSelection();
    this.dispatchEvent(new CustomEvent("select", { detail: { session }, bubbles: true }));
  }

  /**
   * Move the selection by `dir` (+1 down / -1 up) through the *result set* — not through the
   * rows that happen to be realized — scroll it into view, focus it, and open it. Returns true
   * if it moved. Used by the app-level j/k + arrow shortcuts.
   */
  moveSelection(dir) {
    const rows = this._rows;
    if (!rows.length) return false;
    let idx = this._selectedId == null ? -1 : rows.findIndex((r) => (r.id || "") === this._selectedId);
    if (idx < 0) idx = dir > 0 ? -1 : rows.length; // wrap to an edge on first press
    const next = Math.min(rows.length - 1, Math.max(0, idx + dir));
    if (next === idx) return false;

    const row = rows[next];
    this._selectedId = row.id || "";
    this._focusedId = this._selectedId;
    this._layout({ force: true });                 // realize the target wherever it is
    this._liFor(this._selectedId)?.scrollIntoView({ block: "nearest" });
    this._layout();                                // the scroll moved the window
    this._liFor(this._selectedId)?.focus({ preventScroll: true });
    this._updateSelection();
    this.dispatchEvent(new CustomEvent("select", { detail: { session: row }, bubbles: true }));
    return true;
  }

  _updateSelection() {
    // Touch only the realized rows — there are at most a few dozen.
    for (const li of this._ul?.querySelectorAll(".session-row.selected") || []) {
      if (li.dataset.id !== this._selectedId) {
        li.classList.remove("selected");
        li.setAttribute("aria-selected", "false");
      }
    }
    const next = this._liFor(this._selectedId);
    if (next) {
      next.classList.add("selected");
      next.setAttribute("aria-selected", "true");
    } else {
      this._layout({ force: true });  // it scrolled out of the window; bring it back
    }
  }
}

customElements.define("cv-session-list", CvSessionList);
