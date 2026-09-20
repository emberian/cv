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
import { renderMarkdown, renderCodeBlock } from "../markdown.js";
import { getSubagents, getSubagent, getEvents, PAGE } from "./hydrate.js";

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
    // When true, render a "+" affordance on each message so a host (the loom)
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
    // loads (cvd's windowed endpoint via cv-app) and big in-memory sessions (dropped .zip/.json)
    // go through the same "load more" footer instead of rendering 30k messages up front.
    this.innerHTML = `${this._headerHtml(s)}${this._filterHtml()}<div class="turns ${this._filterClasses()}"></div>`;
    this._wireHeader(s);
    this._wireFilters();
    this._turns = this.querySelector(".turns");
    this._cursor = 0;
    this._limit = PAGE;
    this._pumping = false;
    this._moreInFlight = false;
    this._io?.disconnect();
    this._io = null;
    this._pump();

    this._mountSubagents(s);
    this._mountEvents(s);
  }

  /** Render messages from `_cursor` up to `_limit` in rAF-scheduled chunks (first chunk synchronously). */
  _pump() {
    const s = this._session;
    if (!s || this._pumping) return;
    this._pumping = true;
    const CHUNK = 250;
    const step = () => {
      if (this._session !== s) { this._pumping = false; return; } // session changed — abandon
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
  // Two sources of "more messages": (a) the session came from cvd's windowed `/messages`
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
          if (entries.some((e) => e.isIntersecting)) this._requestMore();
        });
        this._io.observe(pager);
      }
    }
    const shown = this._cursor;
    const loaded = (s.messages || []).length;
    const total = s.message_count && s.message_count > shown ? s.message_count
      : loaded > shown ? loaded : null;
    pager.querySelector(".load-more-note").textContent =
      `showing ${shown}${total ? ` of ${total}` : ""} messages`;
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
    this._moreInFlight = true;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = true; btn.textContent = "Loading…"; }
    this.dispatchEvent(new CustomEvent("load-more", {
      detail: { session: this._session }, bubbles: true,
    }));
  }

  /** Host callback once a "load-more" fetch settled (messages appended and/or `_paged` updated). */
  notifyAppended() {
    this._moreInFlight = false;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = false; btn.textContent = "Load more messages"; }
    this._limit = Math.max(this._limit, this._cursor + PAGE);
    this._pump();
    this._renderPager();
  }

  // ---- events panel --------------------------------------------------------
  // What the session DID: file edits/commands/errors from the event catalog (cvd's `/events`
  // endpoint / the desktop's `local_events`). Collapsible, below the header, mirroring the
  // sub-agents panel. Sessions without events (or an older cvd) simply get no panel.
  async _mountEvents(session) {
    if (!session || session._isSubagent) return;
    const events = await getEvents(session);
    if (this._session !== session || !events.length) return;
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
        <span class="sub-fork" aria-hidden="true">⚡</span>
        <b>${events.length}</b> event${events.length === 1 ? "" : "s"}${summary ? ` — ${esc(summary)}` : ""}
        <span class="sub-caret" aria-hidden="true">▸</span>
      </button>
      <div class="sub-list ev-list" hidden>${events.map((e) => this._eventRowHtml(e)).join("")}</div>`;
    // Below the sub-agents panel when present, else right under the header.
    const anchor = this.querySelector(".sub-panel") || this.querySelector(".transcript-header");
    if (anchor) anchor.insertAdjacentElement("afterend", panel);
    else this.insertAdjacentElement("afterbegin", panel);

    const list = panel.querySelector(".ev-list");
    panel.querySelector(".sub-panel-head").addEventListener("click", (e) => {
      const open = list.hasAttribute("hidden");
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
    const when = e.ts ? fmtTime(e.ts * 1000) : "";
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
    const subs = await getSubagents(session);
    if (this._session !== session || !subs.length) return; // changed / none
    this._subs = subs;
    const panel = document.createElement("div");
    panel.className = "sub-panel";
    panel.innerHTML = `
      <button type="button" class="sub-panel-head" aria-expanded="false">
        <span class="sub-fork" aria-hidden="true">⑂</span>
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
      const row = e.target.closest(".sub-row");
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
    try {
      const full = await getSubagent(stub._parentHarness, stub._parentId, stub.id);
      full._isSubagent = true; // so the nested transcript doesn't re-query sub-agents
      body.innerHTML = "";
      const nested = document.createElement("cv-transcript");
      body.appendChild(nested);
      nested.session = full;
      // Relabel the row with the task prompt (its first user message) now that we have it.
      const label = sessionLabel(full);
      if (label && label !== "(untitled)") row.querySelector(".sub-row-title").textContent = label;
    } catch (e) {
      body.innerHTML = `<div class="muted">Couldn't load this sub-agent (${esc(String(e?.message || e))}).</div>`;
    }
  }

  // Header-level controls (export buttons) — wired once per render.
  _wireHeader(s) {
    this.querySelector("[data-export-json]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.opensession.json`,
        JSON.stringify(toOpenSession(s), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.md`, toMarkdown(s), "text/markdown");
    });
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
        await navigator.clipboard.writeText(sp.querySelector("pre")?.textContent || "");
        btn.textContent = "copied";
        setTimeout(() => { btn.textContent = "copy"; }, 1200);
      } catch { /* clipboard may be unavailable */ }
    });
  }

  // Per-message controls (pick + copy). Re-run for any freshly-mounted window
  // of messages (virtualization), scoped to `root` to avoid double-binding.
  _wireBlocks(root) {
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
      btn.setAttribute("data-wired", "1");
      btn.addEventListener("click", async (e) => {
        // Some copy buttons live inside a <summary> — don't toggle the <details>.
        e.preventDefault();
        e.stopPropagation();
        const pre = btn.closest(".block")?.querySelector("pre");
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
    const out = { injected: 0, thinking: 0, tools: 0 };
    for (const m of this._session?.messages || []) {
      if (m.kind === "injected_context") out.injected++;
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
            <button type="button" class="mini-btn" data-export-md title="Download as Markdown">⬇ .md</button>
            <button type="button" class="mini-btn" data-export-json title="Download as OpenSession JSON">⬇ .json</button>
          </div>
        </div>
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
        <pre class="sp-body"><code>${esc(text)}</code></pre>
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
    const blocks = (m.content || []).map((b) => this._blockHtml(b)).join("")
      || '<div class="muted block-empty">(empty)</div>';
    const pick = this._pickMode
      ? `<button type="button" class="pick-btn" data-pick="${idx}" title="Add this message to the loom">＋ loom</button>`
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

    if (CvTranscript.QUIET_KINDS.has(kind)) {
      // One line until you want it. On a Claude session this is ~40% of all turns.
      return `
      <article class="${cls} turn-quiet" data-kind="${esc(kind)}">
        <details>
          <summary class="turn-head">${head}<span class="turn-peek muted">${esc(this._peek(m))}</span>${pick}</summary>
          <div class="turn-body">${blocks}</div>
        </details>
      </article>`;
    }

    return `
      <article class="${cls}" data-kind="${esc(kind)}">
        <div class="turn-head">${head}${pick}</div>
        <div class="turn-body">${blocks}</div>
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

  /** A one-line peek at a folded turn, so the fold still says what is inside it. For Claude's
   *  injected context the harness names the attachment; that name beats the first 80 characters
   *  of `<system-reminder>` boilerplate every time. */
  _peek(m) {
    const e = harnessExtra(m, this._session?.harness) || {};
    const named = e.attachment_type || e.attachmentType || e.record_type || e.display_kind;
    if (named) return String(named).replace(/_/g, " ");
    for (const b of m.content || []) {
      if (b.type === "text" && b.text?.trim()) {
        return truncateLine(b.text.replace(/<\/?system-reminder>/g, "").trim(), 110);
      }
      if (b.type === "tool_result") return truncateLine(String(b.content ?? ""), 110);
    }
    return "";
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
        const body = b.text
          ? this._renderText(b.text)
          : b.redacted ? '<span class="opaque-blob">[redacted reasoning]</span>'
          : b.encrypted ? '<span class="opaque-blob">[encrypted reasoning blob]</span>'
          : "";
        return `
          <details class="block thinking">
            <summary>💭 Thinking${tag}</summary>
            <div class="thinking-body">${body}${b.signature ? `<div class="sig muted" title="signature">sig: ${esc(String(b.signature).slice(0, 24))}…</div>` : ""}</div>
          </details>`;
      }

      case "tool_use": {
        const input = b.input == null ? "" : pretty(b.input);
        const big = input.length > 600;
        const pre = renderCodeBlock(input, "json");
        const label = `
          <div class="block-label">
            <span class="tool-glyph">⚙</span> tool_use
            ${b.namespace ? `<span class="tool-ns" title="tool namespace">${esc(b.namespace)}</span>` : ""}
            <span class="tool-name">${esc(b.name || "?")}</span>
            <button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-use"><summary class="tool-summary">⚙ <span class="tool-name">${esc(b.name || "?")}</span> <span class="muted">tool_use · ${input.length} chars</span><button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button></summary>${pre}</details>`
          : `<div class="block tool-use">${label}${pre}</div>`;
      }

      case "tool_result": {
        const err = b.is_error ? " is-error" : "";
        const content = String(b.content ?? "");
        const status = b.status ? `<span class="tool-status">${esc(b.status)}</span>` : "";
        const tname = b.tool_name ? `<span class="tool-name">${esc(b.tool_name)}</span>` : "";
        const details = this._detailsHtml(b.details);
        const big = content.length > 600;
        const pre = `<pre class="tool-out"><code>${esc(content)}</code></pre>`;
        const label = `
          <div class="block-label">
            <span class="tool-glyph">${b.is_error ? "✖" : "↳"}</span>
            tool_result${b.is_error ? " (error)" : ""} ${tname} ${status}
            <button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-result${err}"><summary class="tool-summary">${b.is_error ? "✖" : "↳"} <span class="muted">tool_result${b.is_error ? " (error)" : ""} · ${content.length} chars</span> ${status}<button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button></summary>${pre}${details}</details>`
          : `<div class="block tool-result${err}">${label}${pre}${details}</div>`;
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
