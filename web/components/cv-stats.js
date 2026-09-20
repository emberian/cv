// <cv-stats> — a small, tasteful stats dashboard over the loaded session pool.
//
// API: stats.sessions = Session[]   (setter)
//
// Totals, per-harness counts, message counts, top cwds, date range, token
// sums. Charts are hand-rolled CSS bars + a tiny inline SVG activity sparkline.
// No chart library.
import {
  esc, fmtTime, sortTime, shortPath, sumTokens, msgCount, HARNESS_LABELS,
  messageKindLabel, ORIGIN_LABELS, fmtCost,
} from "./util.js";

// Semantic, not decorative: conversation reads in the transcript's own rail colors, machinery in
// muted, and the things you would want to notice (errors, compaction) in the warning palette.
const KIND_COLOR = {
  prompt: "var(--accent)",
  reply: "var(--h-codex)",
  tool_result: "var(--warn)",
  error: "var(--error)",
  compaction_boundary: "var(--warn)",
  compaction_summary: "var(--warn)",
  system_prompt: "var(--h-hermes)",
  subagent_spawn: "var(--h-hermes)",
  subagent_return: "var(--h-hermes)",
  model_change: "var(--h-gemini)",
};
const ORIGIN_COLOR = {
  human: "var(--accent)",
  model: "var(--h-codex)",
  hook: "var(--warn)",
  scheduler: "var(--warn)",
  subagent: "var(--h-hermes)",
};

class CvStats extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
  }

  set sessions(arr) { this._sessions = Array.isArray(arr) ? arr : []; this.render(); }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  _compute() {
    const sessions = this._sessions;
    // `messages` uses msgCount so it counts metadata-only stubs (which carry message_count) too.
    // Role/block breakdowns and token sums need the actual message tree, so they're tallied only over
    // *hydrated* sessions and disclosed as such — never silently zero.
    let messages = 0, hydrated = 0, tokIn = 0, tokOut = 0, tokCache = 0, tokReason = 0;
    let cost = 0, costSeen = false;
    const perHarness = new Map();
    const perRole = new Map();
    // IR v2: what a message IS and where it came FROM. Far more telling than the four roles —
    // on a real Claude session `system` alone covers injected context, notices and API errors.
    const perKind = new Map();
    const perOrigin = new Map();
    const cwds = new Map();
    const projects = new Map();
    const times = [];
    const blockKinds = new Map();

    for (const s of sessions) {
      const h = (s.harness || "?").toLowerCase();
      perHarness.set(h, (perHarness.get(h) || 0) + 1);
      if (s.cwd) {
        cwds.set(s.cwd, (cwds.get(s.cwd) || 0) + 1);
        projects.set(s.cwd, true);
      }
      const t = sortTime(s);
      if (t > 0) times.push(t);
      messages += msgCount(s);
      const msgs = s.messages || [];
      if (msgs.length) {
        hydrated++;
        const tk = sumTokens(s);
        tokIn += tk.input; tokOut += tk.output; tokCache += tk.cacheRead; tokReason += tk.reasoning;
        if (tk.cost != null) { cost += tk.cost; costSeen = true; }
        for (const m of msgs) {
          const r = (m.role || "?").toLowerCase();
          perRole.set(r, (perRole.get(r) || 0) + 1);
          if (m.kind) perKind.set(m.kind, (perKind.get(m.kind) || 0) + 1);
          if (m.origin) perOrigin.set(m.origin, (perOrigin.get(m.origin) || 0) + 1);
          for (const b of m.content || []) {
            const k = b?.type || "?";
            blockKinds.set(k, (blockKinds.get(k) || 0) + 1);
          }
        }
      }
    }

    times.sort((a, b) => a - b);
    return {
      sessions: sessions.length, messages, hydrated, tokIn, tokOut, tokCache, tokReason,
      cost: costSeen ? cost : null,
      perHarness, perRole, perKind, perOrigin, blockKinds, projects: projects.size,
      cwds: [...cwds.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6),
      range: times.length ? [times[0], times[times.length - 1]] : null,
      times,
    };
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>No sessions loaded.</p></div>`;
      return;
    }
    const c = this._compute();

    const cards = [
      ["Sessions", c.sessions.toLocaleString()],
      ["Messages", c.messages.toLocaleString()],
      ["Projects", c.projects.toLocaleString()],
      ["Harnesses", c.perHarness.size],
      ["Input tokens", c.tokIn ? c.tokIn.toLocaleString() : "—"],
      ["Output tokens", c.tokOut ? c.tokOut.toLocaleString() : "—"],
      // Only the harnesses that record a cost contribute one, so the tile stays a dash for a
      // Claude-and-Codex corpus rather than claiming a spend of zero.
      ...(c.cost != null ? [["Reported cost", fmtCost(c.cost)]] : []),
      ...(c.tokReason ? [["Reasoning tokens", c.tokReason.toLocaleString()]] : []),
    ].map(([k, v]) => `<div class="stat-card"><div class="stat-num">${esc(String(v))}</div><div class="stat-label muted">${esc(k)}</div></div>`).join("");

    // Honest disclosure: message-level charts only reflect sessions whose transcript is loaded.
    const unhydrated = c.sessions - c.hydrated;
    const note = unhydrated > 0
      ? `<div class="stat-note muted">Role, block &amp; token breakdowns reflect the ${c.hydrated.toLocaleString()} opened session${c.hydrated === 1 ? "" : "s"} — open more to enrich them. (${unhydrated.toLocaleString()} not yet loaded.)</div>`
      : "";

    const needsHydration = (map, label) =>
      map.size ? this._barsHtml(map, ...label) : `<p class="muted">${c.hydrated ? "none" : "open a session to populate"}</p>`;

    this.innerHTML = `
      <div class="view-head"><h2>📊 Stats</h2>
        <span class="muted">${c.range ? `${esc(fmtTime(new Date(c.range[0]).toISOString()))} → ${esc(fmtTime(new Date(c.range[1]).toISOString()))}` : "no timestamps"}</span>
      </div>
      <div class="stat-grid">${cards}</div>
      ${note}
      <div class="stat-cols">
        <section class="stat-block">
          <h3>Sessions per harness</h3>
          ${this._barsHtml(c.perHarness, (k) => HARNESS_LABELS[k] || k, (k) => `var(--h-${k}, var(--accent))`)}
        </section>
        <section class="stat-block">
          <h3>Messages by kind</h3>
          ${needsHydration(c.perKind, [(k) => messageKindLabel(k), (k) => KIND_COLOR[k] || "var(--accent)"])}
        </section>
        <section class="stat-block">
          <h3>Where they came from</h3>
          ${needsHydration(c.perOrigin, [(k) => ORIGIN_LABELS[k] || k, (k) => ORIGIN_COLOR[k] || "var(--fg-muted)"])}
        </section>
        <section class="stat-block">
          <h3>Block types</h3>
          ${needsHydration(c.blockKinds, [(k) => k, () => "var(--h-codex)"])}
        </section>
        <section class="stat-block">
          <h3>Messages by role</h3>
          ${needsHydration(c.perRole, [(k) => k, () => "var(--fg-muted)"])}
        </section>
        <section class="stat-block">
          <h3>Top working directories</h3>
          ${c.cwds.length ? `<ul class="cwd-list">${c.cwds.map(([p, n]) => `<li><span class="cwd-path" title="${esc(p)}">${esc(shortPath(p, 3))}</span><span class="cwd-count muted">${n}</span></li>`).join("")}</ul>` : '<p class="muted">none recorded</p>'}
        </section>
      </div>
      <section class="stat-block">
        <h3>Activity over time</h3>
        ${this._sparkHtml(c.times)}
      </section>
    `;
  }

  _barsHtml(map, labelFn, colorFn) {
    const entries = [...map.entries()].sort((a, b) => b[1] - a[1]);
    if (!entries.length) return '<p class="muted">none</p>';
    const max = Math.max(...entries.map((e) => e[1]));
    return `<div class="bars">${entries.map(([k, v]) => `
      <div class="bar-row">
        <span class="bar-label">${esc(labelFn(k))}</span>
        <span class="bar-track"><span class="bar-fill" style="width:${(v / max * 100).toFixed(1)}%; background:${colorFn(k)}"></span></span>
        <span class="bar-val muted">${v}</span>
      </div>`).join("")}</div>`;
  }

  // A tiny inline SVG: histogram of session activity over the loaded date range.
  _sparkHtml(times) {
    if (times.length < 2) return '<p class="muted">not enough dated sessions to chart</p>';
    const min = times[0], max = times[times.length - 1];
    const span = Math.max(1, max - min);
    const BUCKETS = 24;
    const buckets = new Array(BUCKETS).fill(0);
    for (const t of times) {
      const i = Math.min(BUCKETS - 1, Math.floor((t - min) / span * BUCKETS));
      buckets[i]++;
    }
    const peak = Math.max(...buckets, 1);
    const W = 600, H = 80, bw = W / BUCKETS;
    const rects = buckets.map((v, i) => {
      const h = v / peak * (H - 8);
      return `<rect x="${(i * bw + 1).toFixed(1)}" y="${(H - h).toFixed(1)}" width="${(bw - 2).toFixed(1)}" height="${h.toFixed(1)}" rx="2" fill="var(--accent)" opacity="${v ? 0.85 : 0.15}"><title>${v} session${v === 1 ? "" : "s"}</title></rect>`;
    }).join("");
    return `<svg class="spark" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Activity histogram">${rects}</svg>`;
  }
}

customElements.define("cv-stats", CvStats);
