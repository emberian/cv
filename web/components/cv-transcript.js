// <cv-transcript> — renders a single Session as role-labeled turns.
//
// API: transcript.session = Session    (setter; triggers render)
//
// Renders text blocks, collapsible thinking, tool_use as a labeled JSON code
// block, tool_result (with error styling), and images as labeled placeholders.
import "./cv-harness-badge.js";
import {
  esc, pretty, fmtTime, sessionLabel, shortPath, sumTokens, msgCount,
  toOpenSession, toMarkdown, downloadFile, slug, ROLE_LABELS, HARNESS_LABELS,
  MESSAGE_KINDS, ORIGIN_LABELS, STRUCTURAL_KINDS, LINEAGE_LABELS,
  messageKindLabel, messageKindGlyph, harnessExtra, fmtCost, fmtTokens,
} from "./util.js";
import { canInvokeNative, invoke } from "../tauri.js";
import { renderMarkdown, renderCodeBlock } from "../markdown.js";
import { getSubagents, getSubagent, getEvents, hydrateSession, getAuthToken, PAGE, CVD_BASE } from "./hydrate.js";

/** Collapse whitespace and cut to `max` — for one-line peeks and rule details. */
function truncateLine(s, max) {
  const t = String(s ?? "").replace(/\s+/g, " ").trim();
  return t.length <= max ? t : t.slice(0, max - 1) + "…";
}

// What the reader chose to hide, remembered across sessions and reloads. Per-viewer convenience
// only — storage can be unavailable, and the transcript is correct either way.
const HIDE_LS = "cv-transcript-hide";

/** The filters, in the order they read. Each is `[id, label, dot color]`; the id names both the
 *  `hide-<id>` class on `.turns` and the CSS rule in styles.css that does the hiding. The dot
 *  takes the same color the thing has in the transcript, so the strip reads as a legend. */
const FILTERS = [
  ["injected", "injected context", "var(--fg-muted)"],
  ["records", "empty records", "var(--border-strong)"],
  ["thinking", "thinking", "var(--accent)"],
  ["tools", "tool calls", "var(--warn)"],
];

function loadHidden() {
  try {
    const raw = localStorage.getItem(HIDE_LS);
    return new Set(raw ? JSON.parse(raw) : []);
  } catch { return new Set(); }
}
function saveHidden(set) {
  try { localStorage.setItem(HIDE_LS, JSON.stringify([...set])); } catch { /* storage may be unavailable */ }
}

/** Is this structural message really just a marker? A rule shows one truncated line, so it is only
 *  safe for a message whose whole content IS that line. Every `subagent_spawn`, `subagent_return`,
 *  `model_change` and `compaction_boundary` in ember's corpus carries ≤ 74 characters of text and
 *  nothing else — but a harness that starts attaching a sub-agent's final report to its return
 *  must get a full turn, not a silently-clipped rule. */
function isMarker(m) {
  const blocks = m.content || [];
  if (blocks.some((b) => b.type !== "text")) return false;
  const chars = blocks.reduce((n, b) => n + (b.text?.length || 0), 0);
  return chars <= 200;
}

class CvTranscript extends HTMLElement {
  constructor() {
    super();
    this._session = null;
    // When true, render a "+" affordance on each message so a host (the splice composer)
    // can collect messages. Hidden by default.
    this._pickMode = false;
    // What the reader wants out of the way. Hiding is pure CSS on `.turns`, so toggling never
    // re-renders — which matters when a session has 1,000+ turns already in the DOM.
    this._hide = loadHidden();
  }

  set session(s) {
    this._session = s || null;
    this.render();
  }
  get session() { return this._session; }

  set pickMode(v) { this._pickMode = !!v; this.render(); }

  connectedCallback() { if (!this.childElementCount) this.render(); }

  render() {
    this._generation = (this._generation || 0) + 1;
    this._lazyBodies = new Map();
    this._lazySerial = 0;
    this._io?.disconnect();
    const s = this._session;
    if (!s) {
      this.innerHTML = `<div class="transcript-empty muted">
        <p>Select a session to view its transcript.</p>
      </div>`;
      return;
    }

    // `content-visibility:auto` on `.turn` (CSS) makes the browser skip layout/paint for off-screen
    // turns and remember each turn's real height once measured — native, correct virtualization. (The
    // old hand-rolled sliding window mapped scroll through *estimated* heights, so one tall Gemini
    // message threw off the math and left blank gaps on scroll.)
    //
    // Building the HTML is the only O(n) cost left (markdown per block), so for huge sessions (20k+
    // messages exist) we render the first screenful synchronously, then append the rest in
    // rAF-scheduled chunks — instant open, no freeze, and scrolling works as chunks fill in.
    // `_pump` renders from `_cursor` up to `_limit` (a window over `s.messages`), so both paged
    // loads (cv serve's windowed endpoint via cv-app) and big in-memory sessions (dropped .zip/.json)
    // go through the same "load more" footer instead of rendering 30k messages up front.
    this.innerHTML = `${this._headerHtml(s)}${this._filterHtml()}<div class="turns ${this._filterClasses()}"></div>`;
    this._wireHeader(s);
    this._wireBlocks(this);
    this._wireFilters();
    this._turns = this.querySelector(".turns");
    this._cursor = 0;
    this._limit = PAGE;
    this._pumping = false;
    this._moreInFlight = false;
    this._pageError = null;
    this._io?.disconnect();
    this._io = null;
    this._pump();

    // A loading stub paints only the header. The hydrated session will replace this DOM;
    // starting its enrichment now duplicates both reads and competes with the first page.
    if (!s._stub) {
      this._mountSubagents(s);
      this._mountEvents(s);
    }
  }

  /** Render messages from `_cursor` up to `_limit` in rAF-scheduled chunks (first chunk synchronously). */
  _pump() {
    const s = this._session;
    if (!s || this._pumping) return;
    this._pumping = true;
    const generation = this._generation;
    const CHUNK = 250;
    const step = () => {
      if (this._generation !== generation) return; // render replaced, even with the same session
      const messages = s.messages || [];
      const target = Math.min(messages.length, this._limit);
      const end = Math.min(target, this._cursor + CHUNK);
      let html = "";
      for (; this._cursor < end; this._cursor++) html += this._messageHtml(messages[this._cursor], this._cursor);
      if (html) {
        // Wire only the freshly-inserted turns — re-scanning the whole subtree
        // every chunk made rendering O(n²) in message count.
        const frag = document.createRange().createContextualFragment(html);
        this._wireBlocks(frag);
        this._turns.appendChild(frag);
      }
      if (this._cursor < Math.min((s.messages || []).length, this._limit)) requestAnimationFrame(step);
      else { this._pumping = false; this._renderPager(); }
    };
    step();
  }

  // ---- paged loading ("load more") ----------------------------------------
  // Two sources of "more messages": (a) the session came from cv serve's windowed `/messages`
  // endpoint and `session._paged` marks that more exist server-side; (b) the session is fully
  // in memory (dropped .zip/.json) but longer than the rendered window `_limit`. Either way we
  // show a "load more" footer that also fires when it scrolls into view. For (a) the host
  // (cv-app) listens for "load-more", fetches the next window, pushes onto `session.messages`,
  // and calls `notifyAppended()`; for (b) we just widen the window and re-pump locally.

  /** Messages already in `session.messages` but beyond the rendered window. */
  _pendingLocal() {
    return (this._session?.messages || []).length - this._cursor;
  }

  _renderPager() {
    const s = this._session;
    let pager = this.querySelector(".load-more");
    if (!s || (!s._paged && this._pendingLocal() <= 0)) {
      pager?.remove();
      this._io?.disconnect();
      this._io = null;
      return;
    }
    if (!pager) {
      pager = document.createElement("div");
      pager.className = "load-more";
      pager.innerHTML = `
        <button type="button" class="mini-btn load-more-btn">Load more messages</button>
        <span class="load-more-note muted"></span>`;
      this.appendChild(pager);
      pager.querySelector(".load-more-btn").addEventListener("click", () => this._requestMore());
      if (typeof IntersectionObserver === "function") {
        this._io = new IntersectionObserver((entries) => {
          if (!this._pageError && entries.some((e) => e.isIntersecting)) this._requestMore();
        });
        this._io.observe(pager);
      }
    }
    const shown = this._cursor;
    const loaded = (s.messages || []).length;
    const total = s.message_count && s.message_count > shown ? s.message_count
      : loaded > shown ? loaded : null;
    pager.querySelector(".load-more-note").textContent =
      this._pageError || `showing ${shown}${total ? ` of ${total}` : ""} messages`;
    const btn = pager.querySelector(".load-more-btn");
    btn.disabled = this._moreInFlight;
    btn.textContent = this._moreInFlight ? "Loading…" : this._pageError ? "Retry loading messages" : "Load more messages";
  }

  _requestMore() {
    if (this._moreInFlight || !this._session) return;
    if (this._pendingLocal() > 0) {
      // More already in memory — widen the window and render it; no fetch needed.
      this._limit = Math.max(this._limit, this._cursor + PAGE);
      this._pump(); // calls _renderPager() when the window is filled
      return;
    }
    if (!this._session._paged) return;
    this._pageError = null;
    this._moreInFlight = true;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = true; btn.textContent = "Loading…"; }
    this.dispatchEvent(new CustomEvent("load-more", {
      detail: { session: this._session }, bubbles: true,
    }));
  }

  /** Host callback once a "load-more" fetch settled (messages appended and/or `_paged` updated). */
  notifyAppended(error = null) {
    this._moreInFlight = false;
    this._pageError = error;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = false; btn.textContent = "Load more messages"; }
    this._limit = Math.max(this._limit, this._cursor + PAGE);
    this._pump();
    this._renderPager();
  }

  /** Late session facts update chrome without resetting turns, paging or scroll. */
  notifyMetadata() {
    const s = this._session;
    const header = this.querySelector(".transcript-header");
    if (!s || !header) return;
    const promptOpen = !!header.querySelector(".th-sysprompt")?.open;
    header.querySelectorAll("[data-lazy-body]").forEach((body) => this._lazyBodies.delete(body.dataset.lazyBody));
    header.outerHTML = this._headerHtml(s);
    const prompt = this.querySelector(".th-sysprompt");
    if (prompt) prompt.open = promptOpen;
    this._wireHeader(s);
    this._wireBlocks(this.querySelector(".transcript-header"));
    this._renderPager();
  }

  // ---- events panel --------------------------------------------------------
  // What the session DID: file edits/commands/errors from the event catalog (cv serve's `/events`
  // endpoint / the desktop's `local_events`). Collapsible, below the header, mirroring the
  // sub-agents panel. Sessions without events (or an older server) simply get no panel.
  async _mountEvents(session) {
    if (!session || session._isSubagent) return;
    const generation = this._generation;
    const events = await getEvents(session);
    if (this._generation !== generation || this._session !== session || !events.length) return;
    const counts = {};
    for (const e of events) counts[e.kind] = (counts[e.kind] || 0) + 1;
    const summary = [
      counts.file_edit ? `${counts.file_edit} edit${counts.file_edit === 1 ? "" : "s"}` : "",
      counts.command ? `${counts.command} command${counts.command === 1 ? "" : "s"}` : "",
      counts.error ? `${counts.error} error${counts.error === 1 ? "" : "s"}` : "",
      counts.file_read ? `${counts.file_read} read${counts.file_read === 1 ? "" : "s"}` : "",
    ].filter(Boolean).join(" · ");

    const panel = document.createElement("div");
    panel.className = "sub-panel ev-panel";
    panel.innerHTML = `
      <button type="button" class="sub-panel-head" aria-expanded="false">
        <span class="sub-glyph" aria-hidden="true">⚡</span>
        <b>${events.length}</b> event${events.length === 1 ? "" : "s"}${summary ? ` — ${esc(summary)}` : ""}
        <span class="sub-caret" aria-hidden="true">▸</span>
      </button>
      <div class="sub-list ev-list" hidden></div>`;
    // Below the sub-agents panel when present, else right under the header.
    const anchor = this.querySelector(".sub-panel") || this.querySelector(".transcript-header");
    if (anchor) anchor.insertAdjacentElement("afterend", panel);
    else this.insertAdjacentElement("afterbegin", panel);

    const list = panel.querySelector(".ev-list");
    let rendered = false;
    panel.querySelector(".sub-panel-head").addEventListener("click", (e) => {
      const open = list.hasAttribute("hidden");
      if (open && !rendered) {
        list.innerHTML = events.map((event) => this._eventRowHtml(event)).join("");
        rendered = true;
      }
      list.toggleAttribute("hidden", !open);
      e.currentTarget.setAttribute("aria-expanded", String(open));
      e.currentTarget.querySelector(".sub-caret").textContent = open ? "▾" : "▸";
    });
  }

  _eventRowHtml(e) {
    const glyphs = { file_edit: "✎", file_read: "👁", command: "$", error: "✖", tool: "⚙" };
    const glyph = glyphs[e.kind] || "⚙";
    const target = e.target
      ? (e.kind === "file_edit" || e.kind === "file_read" || e.kind === "tool"
        ? shortPath(e.target, 4)
        : e.target)
      : "";
    const when = e.ts ? fmtTime(e.ts) : "";
    return `
      <div class="ev-row ev-${esc(e.kind)}">
        <span class="ev-glyph" aria-hidden="true">${glyph}</span>
        <span class="ev-kind">${esc(e.kind)}</span>
        ${e.tool ? `<span class="ev-tool muted">${esc(e.tool)}</span>` : ""}
        <span class="ev-target" title="${esc(e.target || "")}">${esc(target)}</span>
        ${when ? `<span class="ev-when muted">${esc(when)}</span>` : ""}
        ${e.detail ? `<div class="ev-detail muted">↳ ${esc(e.detail)}</div>` : ""}
      </div>`;
  }

  // ---- sub-agent tree -----------------------------------------------------
  // If this session spawned sub-agents (Claude Code Task), show a collapsible tree below the header;
  // each child lazily loads its full transcript inline (recursively rendered by a nested transcript).
  async _mountSubagents(session) {
    if (!session || session._isSubagent) return; // sub-agents don't spawn their own
    const generation = this._generation;
    const subs = await getSubagents(session);
    if (this._generation !== generation || this._session !== session || !subs.length) return; // changed / none
    this._subs = subs;
    const panel = document.createElement("div");
    panel.className = "sub-panel";
    panel.innerHTML = `
      <button type="button" class="sub-panel-head" aria-expanded="false">
        <span class="sub-glyph" aria-hidden="true">⑂</span>
        <b>${subs.length}</b> sub-agent${subs.length === 1 ? "" : "s"} spawned
        <span class="sub-caret" aria-hidden="true">▸</span>
      </button>
      <div class="sub-list" hidden>${subs.map((s, i) => this._subRowHtml(s, i)).join("")}</div>`;
    const header = this.querySelector(".transcript-header");
    if (header) header.insertAdjacentElement("afterend", panel);
    else this.insertAdjacentElement("afterbegin", panel);

    const list = panel.querySelector(".sub-list");
    panel.querySelector(".sub-panel-head").addEventListener("click", (e) => {
      const open = list.hasAttribute("hidden");
      list.toggleAttribute("hidden", !open);
      e.currentTarget.setAttribute("aria-expanded", String(open));
      e.currentTarget.querySelector(".sub-caret").textContent = open ? "▾" : "▸";
    });
    list.addEventListener("click", (e) => {
      const row = e.target.closest(".sub-row-head")?.closest(".sub-row");
      if (row) this._toggleSub(row);
    });
  }

  _subRowHtml(s, i) {
    const when = fmtTime(s.updated_at || s.created_at);
    const n = msgCount(s);
    return `
      <div class="sub-row" data-idx="${i}">
        <div class="sub-row-head">
          <span class="sub-dot" aria-hidden="true"></span>
          <span class="sub-row-title">${esc(s.id || `sub-agent ${i + 1}`)}</span>
          <span class="sub-row-meta muted">${n} msg${n === 1 ? "" : "s"} · ${esc(when)}</span>
          <span class="sub-row-caret" aria-hidden="true">▸</span>
        </div>
        <div class="sub-row-body" hidden></div>
      </div>`;
  }

  async _toggleSub(row) {
    const body = row.querySelector(".sub-row-body");
    const caret = row.querySelector(".sub-row-caret");
    if (!body.hasAttribute("hidden")) {
      body.setAttribute("hidden", "");
      caret.textContent = "▸";
      row.classList.remove("open");
      return;
    }
    body.removeAttribute("hidden");
    caret.textContent = "▾";
    row.classList.add("open");
    if (body.dataset.loaded) return;
    body.dataset.loaded = "1";
    body.innerHTML = `<div class="muted sub-loading">Loading sub-agent…</div>`;
    const stub = this._subs[+row.dataset.idx];
    const generation = this._generation;
    try {
      const full = await getSubagent(stub._parentHarness, stub._parentId, stub.id);
      if (generation !== this._generation) return;
      full._isSubagent = true; // so the nested transcript doesn't re-query sub-agents
      body.innerHTML = "";
      const nested = document.createElement("cv-transcript");
      body.appendChild(nested);
      nested.session = full;
      // Relabel the row with the task prompt (its first user message) now that we have it.
      const label = sessionLabel(full);
      if (label && label !== "(untitled)") row.querySelector(".sub-row-title").textContent = label;
    } catch (e) {
      if (generation !== this._generation) return;
      delete body.dataset.loaded;
      body.innerHTML = `<div class="muted">Couldn't load this sub-agent (${esc(String(e?.message || e))}).</div><button type="button" class="mini-btn">Retry loading sub-agent</button>`;
      body.querySelector("button").addEventListener("click", () => { body.hidden = true; this._toggleSub(row); });
    }
  }

  // Header-level controls (export buttons) — wired once per render.
  _wireHeader(s) {
    for (const [format, selector] of [["opensession", "[data-export-json]"], ["markdown", "[data-export-md]"]]) {
      this.querySelector(selector)?.addEventListener("click", async (e) => {
        const button = e.currentTarget;
        if (!s._wasm && (s._remote || s._stub || s._paged)) {
          if (!canInvokeNative()) {
            // Let the browser's download manager consume the attachment. No response Blob,
            // full transcript JSON parsing, or mutation of the paged browsing cache.
            const tok = getAuthToken();
            const tokParam = tok ? `&token=${encodeURIComponent(tok)}` : "";
            const link = document.createElement("a");
            link.href = `${CVD_BASE}/api/session/${encodeURIComponent(s.harness)}/${encodeURIComponent(s.id)}/export?format=${format}${tokParam}`;
            link.target = "_blank"; link.rel = "noopener";
            document.body.appendChild(link); link.click(); link.remove();
            return;
          }
          button.disabled = true;
          const note = this.querySelector(".export-note");
          if (note) note.textContent = "";
          try { await invoke("export_session", { harness: s.harness, id: s.id, format }); }
          catch (error) { if (note) note.textContent = `Couldn't export transcript: ${error?.message || error}`; }
          finally { button.disabled = false; }
          return;
        }
        const full = (s._stub || s._paged) ? await hydrateSession(s) : s;
        downloadFile(`${slug(sessionLabel(full))}.${format === "markdown" ? "md" : "opensession.json"}`,
          format === "markdown" ? toMarkdown(full) : JSON.stringify(toOpenSession(full), null, 2),
          format === "markdown" ? "text/markdown" : "application/json");
      });
    }
    // Lineage chips ask the host to open another session; the host knows the pool, we do not.
    this.querySelectorAll("[data-open-session]").forEach((btn) => {
      btn.addEventListener("click", () => {
        this.dispatchEvent(new CustomEvent("open-session-id", {
          detail: { id: btn.dataset.openSession, harness: s.harness, from: s },
          bubbles: true,
        }));
      });
    });
    // The system-prompt fold has its own copy button and no `.block` ancestor for the generic
    // copy handler to find, so wire it here against the <pre> inside the fold.
    const sp = this.querySelector(".th-sysprompt");
    sp?.querySelector("[data-copy]")?.addEventListener("click", async (e) => {
      e.preventDefault(); e.stopPropagation();
      const btn = e.currentTarget;
      try {
        this._materialize(sp.querySelector("[data-lazy-body]"));
        await navigator.clipboard.writeText(sp.querySelector("pre")?.textContent || "");
        btn.textContent = "copied";
        setTimeout(() => { btn.textContent = "copy"; }, 1200);
      } catch { /* clipboard may be unavailable */ }
    });
  }

  // Per-message controls (pick + copy). Re-run for any freshly-mounted window
  // of messages (virtualization), scoped to `root` to avoid double-binding.
  // Closed folds retain source references, not rendered/highlighted HTML or hidden DOM.
  // Materialize exactly once when opened (or copied), including nested fold handlers.
  _deferBody(render) {
    this._lazyBodies ||= new Map();
    const id = String(this._lazySerial = (this._lazySerial || 0) + 1);
    this._lazyBodies.set(id, render);
    return `<div data-lazy-body="${id}"></div>`;
  }

  _materialize(body) {
    if (!body?.hasAttribute("data-lazy-body")) return;
    const id = body.dataset.lazyBody;
    const render = this._lazyBodies.get(id);
    body.removeAttribute("data-lazy-body");
    this._lazyBodies.delete(id);
    if (!render) return;
    body.innerHTML = render();
    this._wireBlocks(body);
  }

  _wireBlocks(root) {
    root.querySelectorAll("[data-lazy-body]").forEach((body) => {
      const fold = body.closest("details");
      if (!fold || fold.dataset.lazyWired) return;
      fold.dataset.lazyWired = "1";
      const generation = this._generation;
      fold.addEventListener("toggle", () => {
        if (this._generation !== generation) return;
        if (fold.open) this._materialize(body);
      });
      if (fold.open) this._materialize(body);
    });
    const s = this._session;
    root.querySelectorAll("[data-pick]:not([data-wired])").forEach((btn) => {
      btn.setAttribute("data-wired", "1");
      btn.addEventListener("click", (e) => {
        // On a folded turn this button lives inside the <summary>; don't toggle the fold.
        e.preventDefault();
        e.stopPropagation();
        const idx = Number(btn.dataset.pick);
        const m = (s?.messages || [])[idx];
        if (m) this.dispatchEvent(new CustomEvent("pick-message", {
          detail: { session: s, message: m, index: idx }, bubbles: true,
        }));
      });
    });
    root.querySelectorAll("[data-copy]:not([data-wired])").forEach((btn) => {
      if (!btn.closest(".block")) return; // system prompt has its header-specific handler
      btn.setAttribute("data-wired", "1");
      btn.addEventListener("click", async (e) => {
        // Some copy buttons live inside a <summary> — don't toggle the <details>.
        e.preventDefault();
        e.stopPropagation();
        const block = btn.closest(".block");
        this._materialize(block?.querySelector("[data-lazy-body]"));
        const pre = block?.querySelector("pre");
        const text = pre ? pre.textContent : "";
        try {
          await navigator.clipboard.writeText(text);
          const old = btn.textContent;
          btn.textContent = "copied";
          setTimeout(() => { btn.textContent = old; }, 1200);
        } catch { /* clipboard may be unavailable */ }
      });
    });
  }

  /** The filter strip. Each toggle hides a class of turn with CSS alone — no re-render. */
  _filterHtml() {
    const counts = this._kindCounts();
    const chips = FILTERS.map(([id, label, color]) => {
      const n = counts[id] || 0;
      if (!n) return "";
      const off = this._hide.has(id);
      return `<button type="button" class="tf-chip${off ? " off" : ""}" data-filter="${id}"
        aria-pressed="${!off}" title="${off ? "Show" : "Hide"} ${esc(label)}">
        <span class="tf-dot" aria-hidden="true" style="background:${color}"></span>${esc(label)} <span class="tf-n">${n.toLocaleString()}</span></button>`;
    }).filter(Boolean).join("");
    if (!chips) return "";
    return `<div class="turn-filters" role="group" aria-label="Hide parts of the transcript">
      <span class="tf-lead muted">showing</span>${chips}</div>`;
  }

  /** How many of each filterable thing the loaded window holds — a filter for something that is
   *  not there is a lie about the session, so an absent count hides the chip. */
  _kindCounts() {
    const out = { injected: 0, records: 0, thinking: 0, tools: 0 };
    for (const m of this._session?.messages || []) {
      if (m.kind === "injected_context") out.injected++;
      if (!(m.content || []).length) out.records++;
      for (const b of m.content || []) {
        if (b.type === "thinking") out.thinking++;
        else if (b.type === "tool_use" || b.type === "tool_result") out.tools++;
      }
    }
    return out;
  }

  _filterClasses() {
    return [...this._hide].map((k) => `hide-${k}`).join(" ");
  }

  _wireFilters() {
    this.querySelectorAll("[data-filter]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const id = btn.dataset.filter;
        if (this._hide.has(id)) this._hide.delete(id); else this._hide.add(id);
        saveHidden(this._hide);
        btn.classList.toggle("off", this._hide.has(id));
        btn.setAttribute("aria-pressed", String(!this._hide.has(id)));
        if (this._turns) this._turns.className = `turns ${this._filterClasses()}`;
      });
    });
  }

  _headerHtml(s) {
    const h = (s.harness || "").toLowerCase();
    const meta = [];
    if (s.model) meta.push(`<span class="kv"><span class="k">model</span>${esc(s.model)}</span>`);
    if (s.cwd) meta.push(`<span class="kv" title="${esc(s.cwd)}"><span class="k">cwd</span>${esc(shortPath(s.cwd, 4))}</span>`);
    if (s.git?.branch) meta.push(`<span class="kv"><span class="k">branch</span>${esc(s.git.branch)}</span>`);
    if (s.git?.commit) meta.push(`<span class="kv"><span class="k">commit</span>${esc(String(s.git.commit).slice(0, 8))}</span>`);
    const when = fmtTime(s.updated_at || s.created_at);
    if (when) meta.push(`<span class="kv"><span class="k">updated</span>${esc(when)}</span>`);
    const tok = sumTokens(s);
    if (tok.total) {
      const extra = [
        tok.reasoning ? `${fmtTokens(tok.reasoning)} reasoning` : "",
        tok.cost != null ? fmtCost(tok.cost) : "",
      ].filter(Boolean);
      meta.push(`<span class="kv" title="total token usage across this session"><span class="k">tokens</span>${tok.input.toLocaleString()}↓ ${tok.output.toLocaleString()}↑${extra.length ? ` · ${esc(extra.join(" · "))}` : ""}</span>`);
    }
    if (s.id) meta.push(`<span class="kv"><span class="k">id</span>${esc(s.id)}</span>`);

    return `
      <header class="transcript-header">
        <div class="th-title">
          <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
          <h2>${esc(sessionLabel(s))}</h2>
          <div class="th-actions">
            <button type="button" class="mini-btn" data-export-md title="Download complete transcript as Markdown">⬇ .md</button>
            <button type="button" class="mini-btn" data-export-json title="Download complete transcript as OpenSession JSON">⬇ .json</button>
          </div>
        </div>
        <div class="export-note muted" role="status"></div>
        <div class="th-meta">${meta.join("")}</div>
        ${this._lineageHtml(s)}
        ${s.source_path ? `<div class="th-source muted" title="${esc(s.source_path)}">${esc(s.source_path)}</div>` : ""}
        ${this._systemPromptHtml(s)}
      </header>`;
  }

  /** `Session::lineage` — where this session came from and where it went, as navigable chips.
   *  Clicking one asks the host to open that session (it may or may not be in the pool). */
  _lineageHtml(s) {
    const l = s.lineage;
    if (!l) return "";
    const chips = Object.entries(LINEAGE_LABELS)
      .filter(([k]) => l[k])
      .map(([k, label]) => {
        const v = String(l[k]);
        // `agent_path` is a nickname, not an id — it has nothing to navigate to.
        const nav = k !== "agent_path" && k !== "spawned_by_tool_use";
        // Ids are long and uuid-shaped more often than not; show enough to recognise one and
        // keep the whole thing in the tooltip. A short id (an agent nickname, a Codex thread
        // label) is shown whole — truncating it would destroy the only information it carries.
        const shown = v.length > 20 ? v.slice(0, 12) + "…" : v;
        const body = `<span class="ln-k">${esc(label)}</span><code>${esc(shown)}</code>`;
        return nav
          ? `<button type="button" class="ln-chip is-nav" data-open-session="${esc(v)}" title="Open ${esc(v)}">${body}</button>`
          : `<span class="ln-chip" title="${esc(v)}">${body}</span>`;
      });
    if (!chips.length) return "";
    return `<div class="th-lineage" aria-label="Session lineage">${chips.join("")}</div>`;
  }

  /** `Session::system_prompt` — what the harness actually sent, which is NOT a message and so
   *  never appeared anywhere in this UI before. Folded, because it is usually thousands of words. */
  _systemPromptHtml(s) {
    const sp = s.system_prompt;
    if (!sp || !String(sp).trim()) return "";
    const text = String(sp);
    const words = text.trim().split(/\s+/).length;
    return `
      <details class="th-sysprompt">
        <summary><span class="sp-glyph" aria-hidden="true">§</span> system prompt <span class="muted">${words.toLocaleString()} words · ${text.length.toLocaleString()} chars</span><button type="button" class="copy-btn" data-copy aria-label="Copy system prompt">copy</button></summary>
        ${this._deferBody(() => `<pre class="sp-body"><code>${esc(text)}</code></pre>`)}
      </details>`;
  }

  // ---- one message --------------------------------------------------------
  // IR v2 gives every message a `kind` (what it IS) and an `origin` (where it came from), so a
  // transcript no longer has to paint a typed prompt, a harness system-reminder, a slash-command
  // notice and an API error as four identical grey "System" turns. Three shapes come out of that:
  //
  //   • a STRUCTURAL kind (compaction, model change, branch, sub-agent spawn/return) is punctuation
  //     between turns, not a turn — it gets a rule across the column;
  //   • a QUIET kind (injected context, system prompt, notice, carrier) is machinery the reader
  //     usually wants out of the way — it gets a one-line fold;
  //   • everything else is conversation and gets a full turn.

  /** Kinds that fold shut by default: real content, but not what you came to read. */
  static QUIET_KINDS = new Set(["injected_context", "system_prompt", "notice", "carrier"]);

  _messageHtml(m, idx) {
    const kind = m.kind || "";
    if (STRUCTURAL_KINDS.has(kind) && isMarker(m)) return this._ruleHtml(m, idx);

    const role = (m.role || "").toLowerCase();
    const known = !!MESSAGE_KINDS[kind];
    const label = kind ? messageKindLabel(kind) : (ROLE_LABELS[role] || role || "?");
    const when = fmtTime(m.timestamp);
    const usage = this._usageHtml(m.usage);
    const model = m.model ? `<span class="turn-model">${esc(m.model)}</span>` : "";
    const origin = this._originHtml(m);
    const renderBlocks = () => (m.content || []).map((b) => this._blockHtml(b)).join("")
      || '<div class="muted block-empty">(empty)</div>';
    const pick = this._pickMode
      ? `<button type="button" class="pick-btn" data-pick="${idx}" title="Add this message to the splice">＋ splice</button>`
      : "";
    const cls = `turn turn-${esc(role)} turn-kind-${esc(kind || "unknown")}${known ? "" : " turn-unknown-kind"}`;
    const head = `
        <span class="turn-glyph" aria-hidden="true">${messageKindGlyph(kind)}</span>
        <span class="turn-role">${esc(label)}</span>
        ${known ? "" : `<span class="turn-unknown-tag" title="this build of the UI does not know this message kind">unknown kind</span>`}
        ${origin}
        ${model}
        ${when ? `<span class="turn-when muted">${esc(when)}</span>` : ""}
        ${usage}`;

    if (!(m.content || []).length) {
      // No content at all. This is a record the harness kept, not something anyone said — 20% of a
      // real Codex session is contentless `reply` rows carrying nothing but `token_count`
      // bookkeeping. Say what it is on one line; do not reclassify it, the adapter owns the kind.
      const named = this._recordName(m);
      return `
      <article class="${cls} turn-empty" data-kind="${esc(kind)}">
        <div class="turn-head">${head}
          <span class="turn-peek muted">${esc(named || "no content")}</span>${pick}</div>
      </article>`;
    }

    if (CvTranscript.QUIET_KINDS.has(kind)) {
      // One line until you want it. On a Claude session this is ~40% of all turns.
      return `
      <article class="${cls} turn-quiet" data-kind="${esc(kind)}">
        <details>
          <summary class="turn-head">${head}<span class="turn-peek muted">${this._peekHtml(m)}</span>${pick}</summary>
          <div class="turn-body">${this._deferBody(renderBlocks)}</div>
        </details>
      </article>`;
    }

    return `
      <article class="${cls}" data-kind="${esc(kind)}">
        <div class="turn-head">${head}${pick}</div>
        <div class="turn-body">${renderBlocks()}</div>
      </article>`;
  }

  /** A structural marker: a rule across the column with what actually happened on it. */
  _ruleHtml(m, idx) {
    const kind = m.kind;
    const e = harnessExtra(m, this._session?.harness) || {};
    const cm = e.compactMetadata || e.compact_metadata || {};
    const text = (m.content || []).map((b) => (b.type === "text" ? b.text : "")).filter(Boolean).join(" ").trim();
    const facts = [];
    if (kind === "compaction_boundary") {
      const trigger = cm.trigger || e.codex_event;
      if (trigger && trigger !== "compacted") facts.push(`${trigger}`);
      const pre = cm.preTokens ?? cm.pre_tokens;
      const post = cm.postTokens ?? cm.post_tokens;
      if (pre != null) facts.push(`${fmtTokens(pre)} → ${post != null ? fmtTokens(post) : "?"} tokens`);
      const dropped = cm.cumulativeDroppedTokens ?? cm.cumulative_dropped_tokens;
      if (dropped != null) facts.push(`${fmtTokens(dropped)} dropped in total`);
      if (e.replacement_history_len != null) facts.push(`${e.replacement_history_len} messages kept`);
    } else if (kind === "model_change") {
      if (m.model) facts.push(m.model);
    } else if (kind === "subagent_spawn" || kind === "subagent_return") {
      const at = e.agent_type || e.agentType;
      if (at) facts.push(at);
      if (e.description) facts.push(String(e.description));
    }
    const detail = facts.length ? facts.join(" · ") : text;
    return `
      <div class="turn-rule turn-rule-${esc(kind)}" data-kind="${esc(kind)}" data-idx="${idx}">
        <span class="tr-glyph" aria-hidden="true">${messageKindGlyph(kind)}</span>
        <span class="tr-label">${esc(messageKindLabel(kind))}</span>
        ${detail ? `<span class="tr-detail muted">${esc(truncateLine(detail, 140))}</span>` : ""}
        ${m.timestamp ? `<span class="tr-when muted">${esc(fmtTime(m.timestamp))}</span>` : ""}
      </div>`;
  }

  /** What the harness itself called this record, when it recorded a name for it —
   *  Claude's `attachment_type`, Codex's `codex_event`, a carrier's `record_type`. Reached
   *  through the harness namespace, never a flat key. */
  _recordName(m) {
    const e = harnessExtra(m, this._session?.harness) || {};
    const named = e.attachment_type || e.attachmentType || e.record_type || e.display_kind || e.codex_event;
    return named ? String(named).replace(/_/g, " ") : "";
  }

  /** A one-line peek at a folded turn, so the fold still says what is inside it: the harness's own
   *  name for the record (Claude `attachment_type`, Codex `codex_event`) as a tag, then the first
   *  line of what it actually says. Either alone is worse — the name alone loses a Codex notice's
   *  text, the text alone loses that a Claude reminder is a `total_tokens_reminder`. */
  _peekHtml(m) {
    const named = this._recordName(m);
    let text = "";
    for (const b of m.content || []) {
      if (b.type === "text" && b.text?.trim()) {
        text = b.text.replace(/<\/?system-reminder>/g, "").trim();
        break;
      }
      if (b.type === "tool_result") { text = String(b.content ?? ""); break; }
    }
    // The name is usually the same words the text opens with; don't print it twice.
    const dupe = named && text && truncateLine(text, 60).toLowerCase().includes(named.toLowerCase());
    const tag = named && !dupe ? `<span class="turn-rec">${esc(named)}</span>` : "";
    const body = text ? esc(truncateLine(text, 110)) : (named && !tag ? esc(named) : "");
    return `${tag}${body}`;
  }

  /** The origin chip, shown only when the origin is NOT the obvious one for the kind — a reply is
   *  from the model and a prompt is from a human, but a prompt from the *scheduler* is news. */
  _originHtml(m) {
    const o = m.origin;
    if (!o || o === "unknown") return "";
    const obvious = (m.kind === "reply" && o === "model")
      || (m.kind === "prompt" && o === "human")
      || (m.kind !== "prompt" && m.kind !== "reply" && o === "harness");
    if (obvious) return "";
    return `<span class="turn-origin" data-origin="${esc(o)}" title="origin: ${esc(o)}">${esc(ORIGIN_LABELS[o] || o)}</span>`;
  }

  _usageHtml(u) {
    if (!u) return "";
    const bits = [];
    if (u.input_tokens != null) bits.push(`${fmtTokens(u.input_tokens)}↓`);
    if (u.output_tokens != null) bits.push(`${fmtTokens(u.output_tokens)}↑`);
    if (u.cache_read_tokens) bits.push(`${fmtTokens(u.cache_read_tokens)} cached`);
    if (u.reasoning_tokens) bits.push(`${fmtTokens(u.reasoning_tokens)} reasoning`);
    if (!bits.length && u.cost_usd == null) return "";
    const cost = u.cost_usd != null ? `<span class="turn-cost" title="provider-reported cost">${esc(fmtCost(u.cost_usd))}</span>` : "";
    const title = [
      u.input_tokens != null ? `input ${u.input_tokens.toLocaleString()}` : "",
      u.output_tokens != null ? `output ${u.output_tokens.toLocaleString()}` : "",
      u.cache_read_tokens ? `cache read ${u.cache_read_tokens.toLocaleString()}` : "",
      u.cache_creation_tokens ? `cache write ${u.cache_creation_tokens.toLocaleString()}` : "",
      u.reasoning_tokens ? `reasoning ${u.reasoning_tokens.toLocaleString()}` : "",
    ].filter(Boolean).join(" · ");
    return `<span class="turn-usage muted" title="${esc(title)}">${esc(bits.join(" · "))}</span>${cost}`;
  }

  _blockHtml(b) {
    switch (b?.type) {
      case "text":
        return `<div class="block block-text">${this._renderText(b.text || "")}</div>`;

      case "thinking": {
        const tag = b.redacted ? " (redacted)" : b.encrypted ? " (encrypted)" : "";
        const body = () => b.text
          ? this._renderText(b.text)
          : b.redacted ? '<span class="opaque-blob">[redacted reasoning]</span>'
          : b.encrypted ? '<span class="opaque-blob">[encrypted reasoning blob]</span>'
          : "";
        return `
          <details class="block thinking">
            <summary>💭 Thinking${tag}</summary>
            <div class="thinking-body">${this._deferBody(body)}${b.signature ? `<div class="sig muted" title="signature">sig: ${esc(String(b.signature).slice(0, 24))}…</div>` : ""}</div>
          </details>`;
      }

      case "tool_use": {
        const input = b.input == null ? "" : pretty(b.input);
        const big = input.length > 600;
        const pre = () => renderCodeBlock(input, "json");
        const label = `
          <div class="block-label">
            <span class="tool-glyph">⚙</span> tool_use
            ${b.namespace ? `<span class="tool-ns" title="tool namespace">${esc(b.namespace)}</span>` : ""}
            <span class="tool-name">${esc(b.name || "?")}</span>
            <button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-use"><summary class="tool-summary">⚙ <span class="tool-name">${esc(b.name || "?")}</span> <span class="muted">tool_use · ${input.length} chars</span><button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button></summary>${this._deferBody(pre)}</details>`
          : `<div class="block tool-use">${label}${pre()}</div>`;
      }

      case "tool_result": {
        const err = b.is_error ? " is-error" : "";
        const content = String(b.content ?? "");
        const status = b.status ? `<span class="tool-status">${esc(b.status)}</span>` : "";
        const tname = b.tool_name ? `<span class="tool-name">${esc(b.tool_name)}</span>` : "";
        const details = () => this._detailsHtml(b.details);
        const big = content.length > 600;
        const pre = () => `<pre class="tool-out"><code>${esc(content)}</code></pre>`;
        const label = `
          <div class="block-label">
            <span class="tool-glyph">${b.is_error ? "✖" : "↳"}</span>
            tool_result${b.is_error ? " (error)" : ""} ${tname} ${status}
            <button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-result${err}"><summary class="tool-summary">${b.is_error ? "✖" : "↳"} <span class="muted">tool_result${b.is_error ? " (error)" : ""} · ${content.length} chars</span> ${status}<button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button></summary>${this._deferBody(() => pre() + details())}</details>`
          : `<div class="block tool-result${err}">${label}${pre()}${details()}</div>`;
      }

      case "file": {
        const path = b.path || b.source || "";
        const mime = b.mime ? esc(b.mime) : "file";
        return `
          <div class="block file">
            <div class="file-card">
              <span class="file-glyph">📄</span>
              <div class="file-meta">
                <div class="file-path" title="${esc(path)}">${esc(shortPath(path, 4) || "(file)")}</div>
                <div class="file-type muted">${mime}${b.source && b.source !== path ? ` · ${esc(b.source)}` : ""}</div>
              </div>
            </div>
          </div>`;
      }

      case "image": {
        const mt = b.media_type ? esc(b.media_type) : "image";
        const ref = b.data_ref ? esc(b.data_ref) : "";
        return `
          <div class="block image">
            <div class="image-placeholder">
              <span class="image-glyph">🖼</span>
              <div class="image-meta">
                <div class="image-type">${mt}</div>
                ${ref ? `<div class="image-ref muted" title="${ref}">${esc(shortPath(b.data_ref, 3))}</div>` : ""}
              </div>
            </div>
          </div>`;
      }

      default: {
        // A block type this build has never heard of. Say so, and show the record — the IR moves
        // faster than this UI, and a silently-dropped block is the failure mode that sent us here.
        const t = b?.type ?? "null";
        return `<div class="block unknown-block muted">
          <span class="ub-tag">unrecognised block</span> <code>${esc(t)}</code>
          ${b && Object.keys(b).length > 1 ? `<details><summary class="muted">raw</summary><pre><code>${esc(pretty(b))}</code></pre></details>` : ""}
        </div>`;
      }
    }
  }

  /** `Block::ToolResult::details` — the structured facts a harness records alongside the text
   *  (Claude's `toolUseResult`, Codex exit codes, Kimi's read notes). Before IR v2 this lived in a
   *  harness bag and never reached the UI. Most of it is a handful of scalars, so pull those out
   *  as chips and keep the raw record one click away; anything unrecognised stays raw JSON. */
  _detailsHtml(d) {
    if (d == null) return "";
    if (typeof d === "string") {
      const t = d.trim();
      if (!t) return "";
      return `<details class="tool-details"><summary class="muted">details · ${t.length.toLocaleString()} chars</summary><pre class="tool-out"><code>${esc(d)}</code></pre></details>`;
    }
    if (typeof d !== "object" || Array.isArray(d)) {
      return `<details class="tool-details"><summary class="muted">details</summary>${renderCodeBlock(pretty(d), "json")}</details>`;
    }
    // Scalar fields read as chips; everything else stays in the raw fold.
    const chips = [];
    const rest = {};
    for (const [k, v] of Object.entries(d)) {
      if (v == null) continue;
      if (typeof v === "number" || typeof v === "boolean") {
        const bad = (k === "exit_code" || k === "exitCode") && v !== 0;
        chips.push(`<span class="td-chip${bad ? " bad" : ""}">${esc(k.replace(/_/g, " "))} <b>${esc(String(v))}</b></span>`);
      } else if (typeof v === "string" && v.length <= 64 && !v.includes("\n")) {
        chips.push(`<span class="td-chip">${esc(k.replace(/_/g, " "))} <b>${esc(v)}</b></span>`);
      } else {
        rest[k] = v;
      }
    }
    const raw = Object.keys(rest).length
      ? `<details class="tool-details"><summary class="muted">details · ${esc(Object.keys(rest).join(", "))}</summary>${renderCodeBlock(pretty(rest), "json")}</details>`
      : "";
    return `${chips.length ? `<div class="td-chips">${chips.join("")}</div>` : ""}${raw}`;
  }

  // Render message prose as (safe) Markdown: headings, lists, blockquotes,
  // emphasis, links, inline + fenced code with a light highlighter. All source
  // text is escaped before any markup is injected (see markdown.js).
  _renderText(text) {
    return `<div class="md">${renderMarkdown(text)}</div>`;
  }
}

customElements.define("cv-transcript", CvTranscript);
