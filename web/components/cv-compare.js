// <cv-compare> — pick two sessions and view them side-by-side, with message-
// level alignment and divergence highlighting. Useful for loom branches that
// share a common prefix and then split.
//
// API: compare.sessions = Session[]   (setter)
//
// Alignment: we walk both message lists in parallel. While the messages match
// (same role + same flattened text), they're marked "same". From the first
// mismatch onward, everything is "diverged".
import "./cv-session-picker.js";
import { esc, sessionLabel, fmtTime, harnessBadge, ROLE_LABELS } from "./util.js";
import { hydrateSession, isStub } from "./hydrate.js";

function injectStyles() {
  if (document.getElementById("cv-compare-styles")) return;
  const el = document.createElement("style");
  el.id = "cv-compare-styles";
  el.textContent = `
    .cmp-error {
      margin: 0.75rem 0;
      padding: 0.75rem 1rem;
      border: 1px solid color-mix(in srgb, var(--error) 55%, var(--border));
      border-radius: var(--radius);
      background: color-mix(in srgb, var(--error) 10%, var(--bg-elev));
      color: var(--fg);
    }
    .cmp-error b { color: var(--error); }
    .cmp-error code {
      font-family: var(--mono);
      font-size: 0.85em;
      color: var(--fg-muted);
    }

    /* Shared-prefix rows: subtle amethyst tint, clear ✓ in the gutter. */
    .cmp-row.cmp-same {
      background: color-mix(in srgb, var(--accent) 6%, transparent);
      border-left: 2px solid color-mix(in srgb, var(--accent) 35%, transparent);
    }
    .cmp-row.cmp-same .cmp-gutter {
      color: var(--accent);
    }
    /* Diverged rows: warm contrast against the calm shared prefix. */
    .cmp-row.cmp-diff,
    .cmp-row.cmp-only-a,
    .cmp-row.cmp-only-b {
      background: color-mix(in srgb, var(--warn) 7%, transparent);
      border-left: 2px solid color-mix(in srgb, var(--warn) 45%, transparent);
    }
    .cmp-row.cmp-diff .cmp-gutter,
    .cmp-row.cmp-only-a .cmp-gutter,
    .cmp-row.cmp-only-b .cmp-gutter {
      color: var(--warn);
    }
    .cmp-gutter {
      font-weight: 700;
      user-select: none;
    }

    /* Swap feedback: brief pulse on the pickers. */
    @keyframes cmp-swap-pulse {
      0%   { box-shadow: 0 0 0 0 color-mix(in srgb, var(--accent) 55%, transparent); }
      100% { box-shadow: 0 0 0 8px color-mix(in srgb, var(--accent) 0%, transparent); }
    }
    .cmp-swapped cv-session-picker {
      animation: cmp-swap-pulse 0.5s ease-out;
      border-radius: var(--radius);
    }
    @media (prefers-reduced-motion: reduce) {
      .cmp-swapped cv-session-picker { animation: none; }
    }
  `;
  document.head.appendChild(el);
}

class CvCompare extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._a = null;
    this._b = null;
    this._hydrated = new Map(); // id -> full session (with messages)
    this._pending = new Set();
    this._errors = new Set(); // ids whose hydration failed
    this._justSwapped = false;
    injectStyles();
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    // Refreshed metadata pool: drop stale full copies / in-flight / error state
    // so updated stubs aren't masked by old hydrated sessions.
    this._hydrated.clear();
    this._pending.clear();
    this._errors.clear();
    // Keep any still-valid prior picks, but DON'T auto-select a pair — auto-aligning two arbitrary
    // (often huge) sessions on every visit was both chuggy and a useless diff. The user picks.
    if (!this._sessions.find((s) => (s.id || "") === this._a)) this._a = null;
    if (!this._sessions.find((s) => (s.id || "") === this._b)) this._b = null;
    this.render();
  }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  // Prefer the hydrated (full) copy if we've fetched it; otherwise the stub from the pool.
  _byId(id) { return this._hydrated.get(id) || this._sessions.find((s) => (s.id || "") === id) || null; }

  // Pull the full transcript for a picked stub, then re-render. No-op if already full/pending.
  _maybeHydrate(id) {
    const s = this._sessions.find((x) => (x.id || "") === id);
    if (!s || this._hydrated.has(id) || this._pending.has(id) || this._errors.has(id) || !isStub(s)) return;
    this._pending.add(id);
    hydrateSession(s)
      .then((full) => { this._hydrated.set(id, full); this._pending.delete(id); this.render(); })
      .catch(() => { this._pending.delete(id); this._errors.add(id); this.render(); });
  }

  // Flatten a message to a comparable signature. Memoized per-message (was a per-render hot spot —
  // it JSON.stringifies tool inputs over both full sessions' shared prefix).
  _sig(m) {
    if (!this._sigCache) this._sigCache = new WeakMap();
    const hit = this._sigCache.get(m);
    if (hit !== undefined) return hit;
    const text = (m.content || []).map((b) => {
      if (b.type === "text" || b.type === "thinking") return b.text || "";
      if (b.type === "tool_use") return `⚙${b.name}:${JSON.stringify(b.input)}`;
      if (b.type === "tool_result") return `↳${b.content || ""}`;
      return b.type;
    }).join("\n");
    const sig = `${m.role} ${text}`;
    this._sigCache.set(m, sig);
    return sig;
  }

  _align(a, b) {
    const am = a?.messages || [], bm = b?.messages || [];
    const n = Math.max(am.length, bm.length);
    const rows = [];
    let diverged = false;
    for (let i = 0; i < n; i++) {
      const ma = am[i], mb = bm[i];
      let state;
      if (!diverged && ma && mb && this._sig(ma) === this._sig(mb)) {
        state = "same";
      } else {
        diverged = true;
        state = (ma && mb) ? "diff" : ma ? "only-a" : "only-b";
      }
      rows.push({ i, ma, mb, state });
    }
    return rows;
  }

  render() {
    if (this._sessions.length < 1) {
      this.innerHTML = `<div class="view-empty muted"><p>Load at least one session to compare.</p></div>`;
      return;
    }
    const a = this._byId(this._a), b = this._byId(this._b);
    const aReady = a && !isStub(a), bReady = b && !isStub(b);
    const aErr = this._a && this._errors.has(this._a);
    const bErr = this._b && this._errors.has(this._b);
    const anyErr = aErr || bErr;

    const rows = (aReady && bReady) ? this._align(a, b) : [];
    const divergeAt = rows.findIndex((r) => r.state !== "same");
    const commonPrefix = divergeAt === -1 ? rows.length : divergeAt;
    let summary;
    if (aReady && bReady) {
      summary = divergeAt === -1 ? "identical message stream" : `${commonPrefix} shared message${commonPrefix === 1 ? "" : "s"}, then diverges`;
    } else if (anyErr) {
      summary = "couldn't load transcript";
    } else if (this._a && this._b) {
      summary = "loading transcripts…";
    } else {
      summary = "pick two sessions";
    }

    this.innerHTML = `
      <div class="view-head"><h2>🔍 Compare</h2>
        <span class="muted">${esc(summary)}</span>
      </div>
      <div class="cmp-pickers">
        <span class="cmp-side-label">A</span>
        <cv-session-picker data-side="a"></cv-session-picker>
        <button type="button" class="mini-btn" data-swap title="Swap A/B">⇄</button>
        <span class="cmp-side-label">B</span>
        <cv-session-picker data-side="b"></cv-session-picker>
      </div>
      ${aErr ? this._errHtml("A", this._a) : ""}
      ${bErr ? this._errHtml("B", this._b) : ""}
      ${aReady && bReady
        ? this._gridShell(a, b)
        : anyErr ? ""
        : `<div class="view-empty muted"><p>${this._a && this._b ? "Loading transcripts…" : "Pick two sessions above to compare them side by side."}</p></div>`}
    `;

    // Brief pulse on the pickers right after a swap (CSS respects reduced motion).
    const pickers = this.querySelector(".cmp-pickers");
    if (this._justSwapped && pickers) {
      this._justSwapped = false;
      pickers.classList.add("cmp-swapped");
      pickers.addEventListener("animationend", () => pickers.classList.remove("cmp-swapped"), { once: true });
      // Fallback in case the animation is suppressed (reduced motion).
      setTimeout(() => pickers.classList.remove("cmp-swapped"), 600);
    }

    // Configure the two pickers.
    this.querySelectorAll("cv-session-picker").forEach((p) => {
      const side = p.dataset.side;
      p.sessions = this._sessions;
      p.value = side === "a" ? this._a : this._b;
      p.placeholder = side === "a" ? "Pick session A…" : "Pick session B…";
      p.addEventListener("pick", (e) => {
        if (side === "a") this._a = e.detail.id; else this._b = e.detail.id;
        this.render();
      });
    });
    this.querySelector("[data-swap]")?.addEventListener("click", () => {
      [this._a, this._b] = [this._b, this._a];
      this._justSwapped = true;
      this.render();
    });

    // Fill the alignment grid in rAF-scheduled chunks so a 6000-row comparison doesn't block.
    if (aReady && bReady) this._fillGrid(rows);

    // Lazily pull full transcripts for whichever sides are still stubs.
    this._maybeHydrate(this._a);
    this._maybeHydrate(this._b);
  }

  // Just the grid frame (sticky heads + an empty rows container); rows stream in via `_fillGrid`.
  _gridShell(a, b) {
    const head = (s) => `<div class="cmp-col-head">${harnessBadge((s.harness || "").toLowerCase())}<span class="cmp-col-title">${esc(sessionLabel(s))}</span></div>`;
    return `
      <div class="cmp-grid">
        <div class="cmp-heads">${head(a)}<div></div>${head(b)}</div>
        <div class="cmp-rows"></div>
      </div>`;
  }

  _rowHtml(r) {
    const cell = (m) => (m ? this._msgHtml(m) : '<div class="cmp-absent muted">— (no message)</div>');
    const glyph = r.state === "same" ? "✓" : r.state === "diff" ? "≠" : "•";
    return `
      <div class="cmp-row cmp-${r.state}">
        <div class="cmp-cell">${cell(r.ma)}</div>
        <div class="cmp-gutter" title="${r.state}">${glyph}</div>
        <div class="cmp-cell">${cell(r.mb)}</div>
      </div>`;
  }

  // Append rows in chunks; a render token aborts a stale fill if the user re-picks mid-stream.
  _fillGrid(rows) {
    const host = this.querySelector(".cmp-rows");
    if (!host) return;
    const token = (this._fillToken = (this._fillToken || 0) + 1);
    const CHUNK = 120;
    let i = 0;
    const step = () => {
      if (token !== this._fillToken || !host.isConnected) return; // superseded / unmounted
      let html = "";
      const end = Math.min(rows.length, i + CHUNK);
      for (; i < end; i++) html += this._rowHtml(rows[i]);
      host.insertAdjacentHTML("beforeend", html);
      if (i < rows.length) requestAnimationFrame(step);
    };
    step();
  }

  _errHtml(side, id) {
    return `<div class="cmp-error">
      <b>⚠ Side ${esc(side)}: couldn't load transcript</b> (cvd unreachable?)
      <div><code>${esc(id || "")}</code></div>
    </div>`;
  }

  _msgHtml(m) {
    const role = (m.role || "?").toLowerCase();
    const label = ROLE_LABELS[role] || role;
    const when = fmtTime(m.timestamp);
    const body = (m.content || []).map((blk) => {
      if (blk.type === "text") return `<div class="cmp-text">${esc(blk.text || "")}</div>`;
      if (blk.type === "thinking") return `<div class="cmp-think muted">💭 ${esc((blk.text || "[opaque]").slice(0, 280))}</div>`;
      if (blk.type === "tool_use") return `<div class="cmp-tool">⚙ <b>${esc(blk.name || "?")}</b></div>`;
      if (blk.type === "tool_result") return `<div class="cmp-tool">↳ <span class="muted">${esc(String(blk.content || "").slice(0, 200))}</span></div>`;
      return `<div class="muted">[${esc(blk.type || "unknown")}]</div>`;
    }).join("");
    return `<div class="cmp-msg cmp-role-${esc(role)}"><div class="cmp-msg-head"><span class="turn-role">${esc(label)}</span>${when ? `<span class="muted">${esc(when)}</span>` : ""}</div>${body}</div>`;
  }
}

customElements.define("cv-compare", CvCompare);
