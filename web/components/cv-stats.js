// <cv-stats> — the stats dashboard, in two clearly separated halves.
//
// API: stats.sessions = Session[]   (setter)
//
// The split is the point. Some numbers are true of the whole archive (how many sessions, how
// many messages, which harnesses, which directories); others can only be true of the transcripts
// this page has actually downloaded (tokens, cost, message kinds, block types). Mixing them in
// one grid produced a "Messages: 1,684" tile beside a "Sessions: 6,873" tile, which is two
// different questions answered as if they were one.
//
// So: the CORPUS half comes from `/api/stats` when a daemon is there (and from the stub pool
// when it isn't, which is still archive-wide because the pool is every session's metadata); the
// OPENED half is headed with exactly how many transcripts it covers. Charts are hand-rolled CSS
// bars + a tiny inline SVG activity histogram. No chart library.
import {
  esc, fmtTime, sortTime, shortPath, sumTokens, msgCount, HARNESS_LABELS,
  messageKindLabel, ORIGIN_LABELS, fmtCost,
} from "./util.js";
import { getCorpusStats } from "./hydrate.js";

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
    this._corpus = null;     // `/api/stats` answered
    this._asked = false;
  }

  set sessions(arr) { this._sessions = Array.isArray(arr) ? arr : []; this.render(); this._askCorpus(); }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); this._askCorpus(); }

  /** Ask the daemon for the numbers no browser-side tally can reach — chiefly the real message
   *  total, which is 3.9 million here and would need every transcript downloaded to compute.
   *  Once; a null answer (static demo, older cvd) just leaves the pool's own figures in place.
   *
   *  `/api/stats` also takes `q=`, the session-filter expression of `cv stats --query`. That is a
   *  different language from the search box's free text, so nothing forwards one as the other. */
  async _askCorpus() {
    if (this._asked || !this._sessions.length) return;
    this._asked = true;
    const corpus = await getCorpusStats();
    if (corpus) { this._corpus = corpus; this.render(); }
  }

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

  /** What the whole archive is, preferring the daemon's answer over anything tallied here. */
  _archive(c) {
    const k = this._corpus;
    if (!k) {
      return {
        source: `tallied from the ${c.sessions.toLocaleString()} session${c.sessions === 1 ? "" : "s"} loaded in this page`,
        sessions: c.sessions,
        messages: c.messages,
        perHarness: c.perHarness,
        cwds: c.cwds,
        projects: c.projects,
        range: c.range,
      };
    }
    const perHarness = new Map(Object.entries(k.by_harness || {})
      .map(([h, n]) => [String(h).toLowerCase(), n])
      .sort((a, b) => b[1] - a[1]));
    const at = (t) => (t ? new Date(t).getTime() : 0);
    return {
      source: "every session on this machine · cv stats",
      sessions: k.sessions,
      messages: k.messages,
      perHarness,
      cwds: (k.top_cwds || []).slice(0, 6).map((r) => [r.cwd, r.sessions]),
      projects: null,             // /api/stats reports the top directories, not a distinct count
      range: k.earliest_created ? [at(k.earliest_created), at(k.latest_updated)] : c.range,
    };
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>No sessions loaded.</p></div>`;
      return;
    }
    const c = this._compute();
    const a = this._archive(c);

    const tiles = (rows) => rows.map(([k, v, title]) =>
      `<div class="stat-card"${title ? ` title="${esc(title)}"` : ""}>
         <div class="stat-num">${esc(String(v))}</div>
         <div class="stat-label muted">${esc(k)}</div>
       </div>`).join("");

    const archiveTiles = tiles([
      ["Sessions", a.sessions.toLocaleString()],
      ["Messages", a.messages.toLocaleString()],
      ["Harnesses", a.perHarness.size],
      ...(a.projects != null ? [["Directories", a.projects.toLocaleString()]] : []),
    ]);

    // The second half is about downloaded transcripts and says so in its own heading, so its
    // tiles never have to caveat themselves one by one.
    const openTiles = tiles([
      ["Messages read", c.hydrated ? this._readCount(c).toLocaleString() : "—"],
      ["Input tokens", c.tokIn ? c.tokIn.toLocaleString() : "—"],
      ["Output tokens", c.tokOut ? c.tokOut.toLocaleString() : "—"],
      ...(c.tokCache ? [["Cache reads", c.tokCache.toLocaleString()]] : []),
      ...(c.tokReason ? [["Reasoning tokens", c.tokReason.toLocaleString()]] : []),
      // Only the harnesses that record a cost contribute one, so the tile stays absent for a
      // Claude-and-Codex corpus rather than claiming a spend of zero.
      ...(c.cost != null ? [["Reported cost", fmtCost(c.cost)]] : []),
    ]);

    const span = a.range
      ? `${esc(fmtTime(new Date(a.range[0]).toISOString()))} → ${esc(fmtTime(new Date(a.range[1]).toISOString()))}`
      : "no timestamps";

    const unopened = a.sessions - c.hydrated;
    const openedHead = c.hydrated
      ? `${c.hydrated.toLocaleString()} of ${a.sessions.toLocaleString()} transcript${a.sessions === 1 ? "" : "s"} downloaded — these panels describe only those`
      : `nothing downloaded yet — open a session and these fill in`;

    const bars = (map, label) =>
      map.size ? this._barsHtml(map, ...label) : `<p class="muted">${c.hydrated ? "none" : "open a session to populate"}</p>`;

    this.innerHTML = `
      <div class="view-head"><h2>📊 Stats</h2><span class="muted">${span}</span></div>

      <section class="stat-section">
        <div class="stat-section-head">
          <h3>The archive</h3>
          <span class="stat-source muted">${esc(a.source)}</span>
        </div>
        <div class="stat-grid">${archiveTiles}</div>
        <div class="stat-cols">
          <section class="stat-block">
            <h3>Sessions per harness</h3>
            ${this._barsHtml(a.perHarness, (k) => HARNESS_LABELS[k] || k, (k) => `var(--h-${k}, var(--accent))`)}
          </section>
          <section class="stat-block">
            <h3>Top working directories</h3>
            ${a.cwds.length ? `<ul class="cwd-list">${a.cwds.map(([p, n]) => `<li><span class="cwd-path" title="${esc(p)}">${esc(shortPath(p, 3))}</span><span class="cwd-count muted">${n.toLocaleString()}</span></li>`).join("")}</ul>` : '<p class="muted">none recorded</p>'}
          </section>
        </div>
        <section class="stat-block">
          <h3>Activity over time</h3>
          ${this._sparkHtml(c.times)}
          <p class="stat-source muted">one bar per period, over the ${c.times.length.toLocaleString()} dated session${c.times.length === 1 ? "" : "s"} in this list</p>
        </section>
      </section>

      <section class="stat-section">
        <div class="stat-section-head">
          <h3>Opened transcripts</h3>
          <span class="stat-source muted">${esc(openedHead)}</span>
        </div>
        <div class="stat-grid">${openTiles}</div>
        <div class="stat-cols">
          <section class="stat-block">
            <h3>Messages by kind</h3>
            ${bars(c.perKind, [(k) => messageKindLabel(k), (k) => KIND_COLOR[k] || "var(--accent)"])}
          </section>
          <section class="stat-block">
            <h3>Where they came from</h3>
            ${bars(c.perOrigin, [(k) => ORIGIN_LABELS[k] || k, (k) => ORIGIN_COLOR[k] || "var(--fg-muted)"])}
          </section>
          <section class="stat-block">
            <h3>Block types</h3>
            ${bars(c.blockKinds, [(k) => k, () => "var(--h-codex)"])}
          </section>
          <section class="stat-block">
            <h3>Messages by role</h3>
            ${bars(c.perRole, [(k) => k, () => "var(--fg-muted)"])}
          </section>
        </div>
        ${unopened > 0 ? `<p class="stat-note muted">${unopened.toLocaleString()} session${unopened === 1 ? "" : "s"} in the archive have not been opened here, so none of their messages, tokens or cost are counted above.</p>` : ""}
      </section>
    `;
  }

  /** Messages actually downloaded — the denominator the second half is really about. */
  _readCount(c) {
    let n = 0;
    for (const s of this._sessions) n += s.messages?.length || 0;
    return n;
  }

  _barsHtml(map, labelFn, colorFn) {
    const entries = [...map.entries()].sort((a, b) => b[1] - a[1]);
    if (!entries.length) return '<p class="muted">none</p>';
    const max = Math.max(...entries.map((e) => e[1]));
    return `<div class="bars">${entries.map(([k, v]) => `
      <div class="bar-row">
        <span class="bar-label">${esc(labelFn(k))}</span>
        <span class="bar-track"><span class="bar-fill" style="width:${(v / max * 100).toFixed(1)}%; background:${colorFn(k)}"></span></span>
        <span class="bar-val muted">${v.toLocaleString()}</span>
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
