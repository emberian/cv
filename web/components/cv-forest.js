// <cv-forest> — the Session Structure explorer. 🌳
//
// A claude-code session is not a flat transcript: it fans out into a *forest* of sub-agents (two
// tiers — directly-spawned Agent/Task agents, and Workflow agents grouped by run), each with a
// journaled outcome; its tool use has shape (which agent reached for what); and its history is
// punctuated by compaction boundaries that are the seams you navigate to recover pre-compaction
// context. This view makes all of that legible and explorable.
//
// API:  forest.sessions = Session[]     (the pool; the view lets you pick one root session)
//       forest.session  = Session       (optional: pin a specific root)
//
// Data sources (all via hydrate.js → cvd / native):
//   getSubagentTree(session)   → the forest (direct + workflow agents, enrichment preserved)
//   getMessages(session,…)     → the root transcript (for tool + compaction analysis)
//   getSubagent(…)             → one agent's full transcript (drill-down)
//   getWorkflowScript(…)       → a workflow's driving script
//
// Sub-views (a strip of pills): Overview · Forest · Workflows · Tools · Compaction.
//
// SWARM-SAFE: all styling is injected from here into a single <style id="cv-forest-styles"> — this
// file never touches styles.css. Motion is gated behind prefers-reduced-motion.
import "./cv-harness-badge.js";
import "./cv-transcript.js";
import {
  esc, fmtTime, sessionLabel, shortPath, truncate, msgCount, sumTokens, harnessExtra,
} from "./util.js";
import {
  getSubagentTree, getSubagent, getMessages, getWorkflowScript, getCompactions,
  hydrateSession, isStub, PAGE,
} from "./hydrate.js";

// ─────────────────────────────────────────────────────────────────────────────
// Status vocabulary → semantic class. Workflow journals use a wild, per-workflow
// vocabulary (done / GREEN / welded / blocked / partial / YELLOW / proven / a
// plain-string return / nothing). We fold them onto five buckets so the whole
// forest reads at a glance: good · warn · bad · open · neutral.
// ─────────────────────────────────────────────────────────────────────────────
const STATUS_GOOD = /^(done|green|welded|proven|landed|connected|closed|complete(d)?|success|ok|merged|shipped|proven-meaningful)$/i;
const STATUS_WARN = /^(partial|yellow|proven-but-projection|assumed.*|carried|deferred|wip)$/i;
const STATUS_BAD = /^(blocked|failed?|error|red|abandoned|killed|timeout)$/i;
const STATUS_OPEN = /^(open.*|pending|running|started|in-progress)$/i;

/** Bucket a raw journaled status string into a semantic class (good|warn|bad|open|neutral). */
function statusClass(status) {
  if (!status) return "neutral";
  const s = String(status).trim();
  if (STATUS_GOOD.test(s)) return "good";
  if (STATUS_WARN.test(s)) return "warn";
  if (STATUS_BAD.test(s)) return "bad";
  if (STATUS_OPEN.test(s)) return "open";
  return "neutral";
}

/** Short glyph for a status bucket. */
function statusGlyph(klass) {
  return { good: "✓", warn: "◐", bad: "✕", open: "○", neutral: "·" }[klass] || "·";
}

// ─────────────────────────────────────────────────────────────────────────────
// Injected styles — amethyst, glassy, legible. AUGMENTS the global CSS vars
// (--accent, --bg-elev, --fg, --ok, --warn, --h-*) already defined in styles.css.
// ─────────────────────────────────────────────────────────────────────────────
const FOREST_STYLES = `
  cv-forest { display: block; }
  cv-forest .fr-head { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; margin-bottom:10px; }
  cv-forest .fr-head h2 { margin:0; }
  cv-forest .fr-pick { display:flex; align-items:center; gap:8px; margin-left:auto; }
  cv-forest .fr-pick select {
    background: var(--bg-sunk); color: var(--fg); border:1px solid var(--border);
    border-radius: 8px; padding: 5px 10px; font: inherit; max-width: 46ch;
  }
  cv-forest .fr-pick select:focus { outline:none; border-color:var(--accent);
    box-shadow:0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent); }

  /* sub-view pills */
  cv-forest .fr-tabs { display:flex; gap:6px; flex-wrap:wrap; margin-bottom:14px; }
  cv-forest .fr-tab {
    border:1px solid var(--border); background:var(--bg-elev); color:var(--fg-muted);
    border-radius:999px; padding:5px 13px; font-size:13px; cursor:pointer;
    display:inline-flex; align-items:center; gap:6px; transition:all .15s ease;
  }
  cv-forest .fr-tab:hover { color:var(--fg); border-color:color-mix(in srgb,var(--accent) 35%,var(--border)); }
  cv-forest .fr-tab.on {
    color:var(--fg); border-color:transparent;
    background:linear-gradient(135deg, color-mix(in srgb,var(--accent) 22%,var(--bg-elev)), var(--bg-elev));
    box-shadow:0 0 0 1px color-mix(in srgb,var(--accent) 40%,transparent), 0 4px 14px -8px color-mix(in srgb,var(--accent) 60%,transparent);
  }
  cv-forest .fr-tab .fr-tab-n { font-size:11px; opacity:.7; }

  cv-forest .fr-loading, cv-forest .fr-empty { color:var(--fg-muted); padding:22px 6px; text-align:center; }

  /* ── stat tiles ─────────────────────────────────────────── */
  cv-forest .stat-grid { display:grid; grid-template-columns:repeat(auto-fit,minmax(118px,1fr)); gap:10px; margin-bottom:16px; }
  cv-forest .stat {
    background:linear-gradient(160deg, color-mix(in srgb,var(--accent) 6%,var(--bg-elev)), var(--bg-elev));
    border:1px solid var(--border); border-radius:var(--radius); padding:11px 13px;
    box-shadow:0 8px 24px -18px color-mix(in srgb,var(--accent) 50%,transparent);
  }
  cv-forest .stat .sv { font-size:23px; font-weight:650; line-height:1.05; letter-spacing:-.01em; }
  cv-forest .stat .sl { font-size:11px; color:var(--fg-muted); text-transform:uppercase; letter-spacing:.05em; margin-top:3px; }
  cv-forest .stat .ss { font-size:11px; color:var(--fg-muted); margin-top:2px; }
  cv-forest .stat.good .sv { color:var(--ok); }
  cv-forest .stat.accent .sv { color:var(--accent); }

  cv-forest .ov-card {
    background:var(--bg-elev); border:1px solid var(--border); border-radius:var(--radius);
    padding:13px 15px; margin-bottom:14px;
  }
  cv-forest .ov-card h3 { margin:0 0 8px; font-size:14px; display:flex; align-items:center; gap:8px; }
  cv-forest .ov-meta { display:flex; flex-wrap:wrap; gap:6px 14px; font-size:12.5px; color:var(--fg-muted); }
  cv-forest .ov-meta .k { color:var(--fg); font-weight:600; margin-right:5px; }

  /* ── session timeline strip (overview + compaction) ─────── */
  cv-forest .tl-strip {
    position:relative; height:46px; border-radius:10px; overflow:hidden;
    background:var(--bg-sunk); border:1px solid var(--border); margin:6px 0 4px;
  }
  cv-forest .tl-bar { position:absolute; bottom:0; width:2px; background:color-mix(in srgb,var(--accent) 55%,transparent); border-radius:1px 1px 0 0; }
  cv-forest .tl-bar.tool { background:color-mix(in srgb,var(--h-codex,#6cc) 65%,transparent); }
  cv-forest .tl-compact {
    position:absolute; top:0; bottom:0; width:2px; background:var(--warn);
    box-shadow:0 0 8px -1px var(--warn);
  }
  cv-forest .tl-compact::after {
    content:"✂"; position:absolute; top:-2px; left:50%; transform:translateX(-50%);
    font-size:10px; color:var(--warn);
  }
  cv-forest .tl-legend { display:flex; gap:14px; font-size:11px; color:var(--fg-muted); margin-top:4px; }
  cv-forest .tl-legend i { display:inline-block; width:9px; height:9px; border-radius:2px; margin-right:4px; vertical-align:middle; }

  /* ── forest tree ────────────────────────────────────────── */
  cv-forest .tree { font-size:13px; }
  cv-forest .node { position:relative; }
  cv-forest .node-row {
    display:flex; align-items:center; gap:8px; padding:6px 9px; border-radius:9px;
    border:1px solid transparent; cursor:pointer; transition:background .12s ease, border-color .12s ease;
  }
  cv-forest .node-row:hover { background:color-mix(in srgb,var(--accent) 7%,transparent); border-color:color-mix(in srgb,var(--accent) 20%,var(--border)); }
  cv-forest .node-row.is-root { background:color-mix(in srgb,var(--accent) 10%,var(--bg-elev)); border-color:color-mix(in srgb,var(--accent) 30%,var(--border)); cursor:default; }
  cv-forest .node-caret { width:14px; flex:none; color:var(--fg-muted); transition:transform .12s ease; font-size:11px; }
  cv-forest .node-row.open .node-caret { transform:rotate(90deg); }
  cv-forest .node-glyph { flex:none; width:20px; text-align:center; }
  cv-forest .node-label { font-weight:550; }
  cv-forest .node-sub { color:var(--fg-muted); font-size:12px; }
  cv-forest .node-spacer { flex:1; }
  cv-forest .node-children { margin-left:16px; padding-left:12px; border-left:1px dashed var(--border); }
  cv-forest .node-body { margin:4px 0 8px 26px; }
  cv-forest .node-body cv-transcript { display:block; border:1px solid var(--border); border-radius:10px; overflow:hidden; }

  /* agent-type chip + outcome badge */
  cv-forest .atype { font-size:11px; padding:1px 7px; border-radius:6px; background:var(--bg-sunk); color:var(--fg-muted); border:1px solid var(--border); white-space:nowrap; }
  cv-forest .badge { font-size:11px; padding:1px 8px; border-radius:999px; white-space:nowrap; display:inline-flex; align-items:center; gap:4px; font-weight:600; }
  cv-forest .badge.good { background:color-mix(in srgb,var(--ok) 18%,transparent); color:var(--ok); }
  cv-forest .badge.warn { background:color-mix(in srgb,var(--warn) 18%,transparent); color:var(--warn); }
  cv-forest .badge.bad  { background:color-mix(in srgb,#e0556b 20%,transparent); color:#e0556b; }
  cv-forest .badge.open { background:color-mix(in srgb,var(--accent) 14%,transparent); color:var(--accent); }
  cv-forest .badge.neutral { background:var(--bg-sunk); color:var(--fg-muted); }

  /* workflow group header */
  cv-forest .wf-group { margin:8px 0; border:1px solid var(--border); border-radius:12px; overflow:hidden; background:var(--bg-elev); }
  cv-forest .wf-head { display:flex; align-items:center; gap:10px; padding:9px 12px; cursor:pointer; background:linear-gradient(120deg, color-mix(in srgb,var(--accent) 8%,var(--bg-elev)), var(--bg-elev)); }
  cv-forest .wf-head:hover { background:color-mix(in srgb,var(--accent) 12%,var(--bg-elev)); }
  cv-forest .wf-title { font-weight:650; }
  cv-forest .wf-id { font-size:11px; color:var(--fg-muted); font-family:var(--mono,monospace); }
  cv-forest .wf-mini { display:flex; gap:3px; margin-left:auto; }
  cv-forest .wf-pip { width:9px; height:9px; border-radius:2px; }
  cv-forest .wf-pip.good { background:var(--ok); } cv-forest .wf-pip.warn { background:var(--warn); }
  cv-forest .wf-pip.bad { background:#e0556b; } cv-forest .wf-pip.open { background:var(--accent); }
  cv-forest .wf-pip.neutral { background:color-mix(in srgb,var(--fg-muted) 50%,transparent); }
  cv-forest .wf-body { padding:8px 12px; }

  /* ── workflow phase lanes (Workflows sub-view) ──────────── */
  cv-forest .lane-wrap { margin-bottom:18px; border:1px solid var(--border); border-radius:14px; background:var(--bg-elev); overflow:hidden; }
  cv-forest .lane-head { display:flex; align-items:center; gap:10px; padding:11px 14px; border-bottom:1px solid var(--border); background:linear-gradient(120deg,color-mix(in srgb,var(--accent) 9%,var(--bg-elev)),var(--bg-elev)); }
  cv-forest .lane-head .lh-title { font-weight:700; font-size:15px; }
  cv-forest .lane-head .lh-script { margin-left:auto; }
  cv-forest .lane-cols { display:grid; grid-auto-flow:column; grid-auto-columns:minmax(210px,1fr); gap:12px; padding:12px 14px; overflow-x:auto; }
  cv-forest .lane-col { min-width:200px; }
  cv-forest .lane-col h4 { margin:0 0 8px; font-size:12px; color:var(--fg-muted); text-transform:uppercase; letter-spacing:.05em; display:flex; align-items:center; gap:6px; }
  cv-forest .lane-col h4 .lc-n { background:var(--bg-sunk); border-radius:999px; padding:0 7px; font-size:11px; }
  cv-forest .agent-card {
    background:var(--bg-sunk); border:1px solid var(--border); border-left-width:3px; border-radius:9px;
    padding:8px 10px; margin-bottom:8px; cursor:pointer; transition:transform .1s ease, box-shadow .15s ease;
  }
  cv-forest .agent-card:hover { transform:translateY(-1px); box-shadow:0 6px 18px -10px color-mix(in srgb,var(--accent) 55%,transparent); }
  cv-forest .agent-card.good { border-left-color:var(--ok); } cv-forest .agent-card.warn { border-left-color:var(--warn); }
  cv-forest .agent-card.bad { border-left-color:#e0556b; } cv-forest .agent-card.open { border-left-color:var(--accent); }
  cv-forest .agent-card.neutral { border-left-color:var(--border); }
  cv-forest .agent-card .ac-desc { font-size:12.5px; font-weight:550; line-height:1.3; margin-bottom:4px; }
  cv-forest .agent-card .ac-foot { display:flex; align-items:center; gap:6px; font-size:11px; color:var(--fg-muted); flex-wrap:wrap; }
  cv-forest .agent-card .ac-summary { font-size:11.5px; color:var(--fg-muted); margin-top:5px; line-height:1.35; max-height:3.2em; overflow:hidden; }

  /* script modal */
  cv-forest .scr-overlay { position:fixed; inset:0; background:color-mix(in srgb,#000 55%,transparent); display:flex; align-items:center; justify-content:center; z-index:60; padding:24px; }
  cv-forest .scr-card { background:var(--bg-elev); border:1px solid var(--border); border-radius:14px; max-width:860px; width:100%; max-height:84vh; display:flex; flex-direction:column; box-shadow:0 24px 70px -24px #000; }
  cv-forest .scr-head { display:flex; align-items:center; gap:10px; padding:12px 15px; border-bottom:1px solid var(--border); }
  cv-forest .scr-head b { font-size:14px; } cv-forest .scr-head .scr-name { font-family:var(--mono,monospace); font-size:12px; color:var(--fg-muted); }
  cv-forest .scr-body { overflow:auto; padding:0; }
  cv-forest .scr-body pre { margin:0; padding:14px 16px; font-size:12px; line-height:1.5; white-space:pre; }

  /* ── tools (histogram + heatmap) ────────────────────────── */
  cv-forest .th-bars { display:flex; flex-direction:column; gap:6px; margin-bottom:18px; }
  cv-forest .th-row { display:grid; grid-template-columns:148px 1fr 52px; align-items:center; gap:10px; }
  cv-forest .th-name { font-size:12.5px; font-weight:550; text-align:right; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  cv-forest .th-track { height:18px; background:var(--bg-sunk); border-radius:6px; overflow:hidden; }
  cv-forest .th-fill { height:100%; border-radius:6px; background:linear-gradient(90deg,var(--accent),color-mix(in srgb,var(--accent) 50%,var(--h-grok,#c7f))); }
  cv-forest .th-count { font-size:12px; color:var(--fg-muted); text-align:right; font-variant-numeric:tabular-nums; }

  cv-forest .heat-wrap { overflow-x:auto; border:1px solid var(--border); border-radius:12px; }
  cv-forest table.heat { border-collapse:collapse; font-size:11.5px; }
  cv-forest table.heat th, cv-forest table.heat td { padding:4px 7px; text-align:center; white-space:nowrap; }
  cv-forest table.heat thead th { position:sticky; top:0; background:var(--bg-elev); color:var(--fg-muted); font-weight:600; border-bottom:1px solid var(--border); }
  cv-forest table.heat tbody th { position:sticky; left:0; background:var(--bg-elev); text-align:left; font-weight:550; max-width:200px; overflow:hidden; text-overflow:ellipsis; border-right:1px solid var(--border); }
  cv-forest table.heat td { color:var(--fg); border-radius:3px; }
  cv-forest table.heat td.z { color:transparent; }

  /* ── compaction ─────────────────────────────────────────── */
  cv-forest .cz { display:flex; flex-direction:column; gap:10px; }
  cv-forest .cz-item { display:flex; gap:12px; border:1px solid var(--border); border-radius:12px; padding:11px 13px; background:var(--bg-elev); border-left:3px solid var(--warn); }
  cv-forest .cz-num { font-size:20px; line-height:1; color:var(--warn); }
  cv-forest .cz-main { flex:1; min-width:0; }
  cv-forest .cz-row1 { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; }
  cv-forest .cz-trigger { font-weight:650; }
  cv-forest .cz-stats { display:flex; gap:12px; flex-wrap:wrap; font-size:12px; color:var(--fg-muted); margin-top:5px; }
  cv-forest .cz-stats b { color:var(--fg); }
  cv-forest .cz-shrink { color:var(--ok); font-weight:600; }
  cv-forest .cz-jump { margin-top:6px; display:flex; gap:8px; }

  @media (prefers-reduced-motion: no-preference) {
    cv-forest .stat:hover { transform:translateY(-1px); transition:transform .12s ease; }
  }
`;

function injectStyles() {
  if (document.getElementById("cv-forest-styles")) return;
  const el = document.createElement("style");
  el.id = "cv-forest-styles";
  el.textContent = FOREST_STYLES;
  document.head.appendChild(el);
}

// ─────────────────────────────────────────────────────────────────────────────

class CvForest extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._root = null;          // the chosen root Session (hydrated)
    this._tree = null;          // SubagentInfo[] forest
    this._sub = "overview";     // active sub-view
    this._loading = false;
    this._loadedFor = null;     // id of the session the forest/messages were loaded for
    this._scriptCache = new Map();
  }

  set sessions(list) {
    this._sessions = Array.isArray(list) ? list : [];
    // Pick a default root: an explicit pin, else the most-recently-active session that has a forest
    // (we can't know cheaply, so just the newest). The picker lets the user change it.
    if (!this._rootId && this._sessions.length) {
      this._rootId = this._mostRecent()?.id || this._sessions[0].id;
    }
    this.render();
    this._ensureLoaded();
  }
  get sessions() { return this._sessions; }

  set session(s) {
    if (!s) return;
    this._rootId = s.id;
    if (!this._sessions.some((x) => x.id === s.id)) this._sessions = [s, ...this._sessions];
    this.render();
    this._ensureLoaded();
  }

  connectedCallback() {
    injectStyles();
    this.render();
    this._ensureLoaded();
  }

  _mostRecent() {
    return this._sessions
      .slice()
      .sort((a, b) => (new Date(b.updated_at || b.created_at || 0)) - (new Date(a.updated_at || a.created_at || 0)))[0];
  }

  _rootSession() {
    return this._sessions.find((s) => s.id === this._rootId) || null;
  }

  // ── load the forest + a transcript window + the compaction boundaries ──
  async _ensureLoaded() {
    const stub = this._rootSession();
    if (!stub) return;
    if (this._loadedFor === stub.id && this._tree) return; // already have it
    this._loading = true;
    this._loadedFor = stub.id;
    this._tree = null;
    this._root = null;
    this._compactions = null;
    this.render();

    // Three reads in parallel:
    //  • the forest (direct + workflow agents),
    //  • a lean transcript window (no `extra` → no fat toolUseResult sidecars) that powers the tool
    //    histogram, heatmap and the timeline density — a big-but-bounded sample keeps even a 30k
    //    transcript responsive; `_windowHasMore` tells the UI it's only the start of the session,
    //  • the compaction boundaries, scanned server-side over the WHOLE session (complete coverage,
    //    tiny payload) via the dedicated endpoint.
    const WINDOW = Math.max(PAGE, 2500);
    const [tree, win, compactions] = await Promise.all([
      getSubagentTree(stub).catch(() => []),
      getMessages(stub, 0, WINDOW).catch(() => null),
      getCompactions(stub).catch(() => null),
    ]);
    if (this._loadedFor !== stub.id) return; // raced past — a newer root was chosen

    this._tree = tree || [];
    if (win && Array.isArray(win.messages)) {
      this._root = { ...stub, messages: win.messages, _stub: false,
        _windowTotal: win.total_known ? win.total : null,
        _windowHasMore: !!win.has_more };
    } else {
      // Fall back to a full hydrate (older cvd) — still bounded by the parse.
      try {
        const full = isStub(stub) ? await hydrateSession(stub) : stub;
        if (this._loadedFor === stub.id) this._root = full;
      } catch { /* leave _root null; analyses degrade gracefully */ }
    }
    // The dedicated endpoint gives every boundary; if it's unavailable, derive from the window's
    // messages (only those in the loaded window will be found — flagged as sampled in the UI).
    this._compactions = compactions || null;
    this._loading = false;
    this.render();
  }

  // ── derived models ──────────────────────────────────────────────────────

  /** Split the forest into directly-spawned agents and workflow groups. */
  _partition() {
    const direct = [];
    const wf = new Map(); // run id → { id, agents: [] }
    for (const a of (this._tree || [])) {
      if (a.workflow) {
        if (!wf.has(a.workflow)) wf.set(a.workflow, { id: a.workflow, agents: [] });
        wf.get(a.workflow).agents.push(a);
      } else {
        direct.push(a);
      }
    }
    // Workflows newest-ish first by the latest agent activity within them.
    const groups = [...wf.values()].sort((x, y) => y.agents.length - x.agents.length);
    return { direct, groups };
  }

  /** Count tool_use blocks across the root transcript, by tool name. */
  _toolHistogram(session) {
    const counts = new Map();
    for (const m of (session?.messages || [])) {
      for (const b of (m.content || [])) {
        if (b.type === "tool_use" && b.name) counts.set(b.name, (counts.get(b.name) || 0) + 1);
      }
    }
    return [...counts.entries()].sort((a, b) => b[1] - a[1]);
  }

  /** The compaction boundaries to render, normalized to one shape:
   *  `{ index, summaryIndex, trigger, preTokens, durationMs, summary, preSpan: [start,end] }`.
   *  Prefers the dedicated `/compactions` scan (complete coverage, paired summary text + span);
   *  falls back to scanning the loaded window's messages when the endpoint is unavailable. */
  _resolveCompactions() {
    if (Array.isArray(this._compactions)) {
      return this._compactions.map((c) => ({
        index: c.boundary_msg_idx ?? c.index,
        summaryIndex: c.summary_msg_idx ?? c.summary_index,
        trigger: c.trigger,
        preTokens: c.pre_tokens,
        durationMs: c.duration_ms,
        summary: c.summary,
        headline: c.headline,
        preSpan: Array.isArray(c.pre_compaction_span)
          ? c.pre_compaction_span
          : Array.isArray(c.pre_span)
            ? c.pre_span
            : null,
        _complete: true,
      }));
    }
    // Fallback: scan the loaded window. IR v2 makes this a first-class question — a boundary is
    // `kind: "compaction_boundary"` on every harness — so the old Claude-shaped sniff
    // (`extra.subtype === "compact_boundary"` plus a text match) is gone. The harness metadata
    // that decorates it is nested under the harness key now (`extra.claude.compactMetadata`),
    // never flat.
    const out = [];
    const msgs = this._root?.messages || [];
    const hx = (m) => harnessExtra(m, this._root?.harness) || {};
    msgs.forEach((m, i) => {
      if (m.kind !== "compaction_boundary") return;
      const e = hx(m);
      const cm = e.compactMetadata || e.compact_metadata || {};
      out.push({
        index: i,
        // The summary that seeds the next window is its own message right after the boundary.
        summaryIndex: msgs[i + 1]?.kind === "compaction_summary" ? i + 1 : null,
        trigger: cm.trigger || e.codex_event || null,
        preTokens: cm.preTokens ?? cm.pre_tokens ?? null,
        durationMs: cm.durationMs ?? cm.duration_ms ?? null,
        summary: null, preSpan: null, _complete: false,
      });
    });
    return out;
  }

  // ── render ───────────────────────────────────────────────────────────────

  render() {
    const stub = this._rootSession();
    const { direct, groups } = this._partition();
    const wfAgentCount = groups.reduce((n, g) => n + g.agents.length, 0);
    const tools = this._root ? this._toolHistogram(this._root) : [];
    const toolTotal = tools.reduce((n, [, c]) => n + c, 0);
    const compactions = this._resolveCompactions();

    const TABS = [
      ["overview", "Overview", "◉", null],
      ["forest", "Forest", "🌳", (this._tree || []).length],
      ["workflows", "Workflows", "🧬", groups.length],
      ["tools", "Tools", "🔧", tools.length],
      ["compaction", "Compaction", "✂", compactions.length],
    ];

    this.innerHTML = `
      <div class="fr-head">
        <h2>🌳 Session structure</h2>
        <span class="muted">The agent forest, workflow lanes, tool shape &amp; compaction seams of one session.</span>
        <label class="fr-pick">
          <span class="muted">root</span>
          <select class="fr-root" aria-label="Root session"></select>
        </label>
      </div>

      <div class="fr-tabs">
        ${TABS.map(([id, label, glyph, n]) => `
          <button type="button" class="fr-tab${id === this._sub ? " on" : ""}" data-sub="${id}">
            <span aria-hidden="true">${glyph}</span>${esc(label)}${n != null ? `<span class="fr-tab-n">${n}</span>` : ""}
          </button>`).join("")}
      </div>

      <div class="fr-body"></div>
    `;

    // Root picker (built once per render; selection preserved via value).
    const sel = this.querySelector(".fr-root");
    if (sel) {
      const opts = this._sessions
        .slice()
        .sort((a, b) => (new Date(b.updated_at || b.created_at || 0)) - (new Date(a.updated_at || a.created_at || 0)))
        .slice(0, 200);
      sel.innerHTML = opts.map((s) =>
        `<option value="${esc(s.id)}"${s.id === this._rootId ? " selected" : ""}>${esc(truncate(sessionLabel(s), 70))}</option>`).join("");
      sel.value = this._rootId || "";
      sel.addEventListener("change", () => {
        this._rootId = sel.value;
        this._sub = this._sub; // keep sub-view
        this.render();
        this._ensureLoaded();
      });
    }

    this.querySelectorAll(".fr-tab").forEach((t) =>
      t.addEventListener("click", () => { this._sub = t.dataset.sub; this.render(); }));

    const body = this.querySelector(".fr-body");
    if (!stub) { body.innerHTML = `<div class="fr-empty">No sessions loaded.</div>`; return; }
    if (this._loading && !this._tree) { body.innerHTML = `<div class="fr-loading">Loading the forest for <b>${esc(truncate(sessionLabel(stub), 60))}</b>…</div>`; return; }

    const ctx = { stub, direct, groups, wfAgentCount, tools, toolTotal, compactions };
    switch (this._sub) {
      case "overview": body.innerHTML = this._overviewHtml(ctx); break;
      case "forest": body.innerHTML = this._forestHtml(ctx); this._wireForest(body); break;
      case "workflows": body.innerHTML = this._workflowsHtml(ctx); this._wireWorkflows(body); break;
      case "tools": body.innerHTML = this._toolsHtml(ctx); break;
      case "compaction": body.innerHTML = this._compactionHtml(ctx); this._wireCompaction(body); break;
    }
  }

  // ── Overview ──────────────────────────────────────────────────────────────
  _overviewHtml({ stub, direct, groups, wfAgentCount, tools, toolTotal, compactions }) {
    const s = this._root || stub;
    const tok = sumTokens(s);
    const nMsg = this._root?._windowTotal || msgCount(s);
    const moreNote = this._root?._windowHasMore ? " (sampled)" : "";
    const tiles = [
      ["accent", nMsg.toLocaleString(), "messages", ""],
      ["", (this._tree || []).length, "agents", `${direct.length} direct · ${wfAgentCount} in workflows`],
      ["", groups.length, "workflows", ""],
      ["", tools.length, "tool kinds", toolTotal ? `${toolTotal.toLocaleString()} calls${moreNote}` : ""],
      [compactions.length ? "" : "", compactions.length, "compactions", ""],
      ["good", tok.total ? this._kfmt(tok.total) : "—", "tokens", tok.total ? `${this._kfmt(tok.input)}↓ ${this._kfmt(tok.output)}↑` : ""],
    ];

    const h = (stub.harness || "").toLowerCase();
    const meta = [];
    if (stub.cwd) meta.push(`<span><span class="k">cwd</span>${esc(shortPath(stub.cwd, 4))}</span>`);
    if (s.model) meta.push(`<span><span class="k">model</span>${esc(s.model)}</span>`);
    if (s.git?.branch) meta.push(`<span><span class="k">branch</span>${esc(s.git.branch)}</span>`);
    const when = fmtTime(stub.updated_at || stub.created_at);
    if (when) meta.push(`<span><span class="k">updated</span>${esc(when)}</span>`);
    meta.push(`<span><span class="k">id</span>${esc(stub.id)}</span>`);

    return `
      <div class="stat-grid">
        ${tiles.map(([cls, v, l, ss]) => `
          <div class="stat ${cls}">
            <div class="sv">${esc(String(v))}</div>
            <div class="sl">${esc(l)}</div>
            ${ss ? `<div class="ss">${esc(ss)}</div>` : ""}
          </div>`).join("")}
      </div>

      <div class="ov-card">
        <h3><cv-harness-badge harness="${esc(h)}"></cv-harness-badge> ${esc(sessionLabel(stub))}</h3>
        <div class="ov-meta">${meta.join("")}</div>
      </div>

      ${this._timelineHtml(this._root, compactions, tools.length > 0)}

      <div class="ov-card">
        <h3>What's here</h3>
        <p class="muted" style="margin:0; font-size:13px; line-height:1.5;">
          ${this._summarySentence({ direct, groups, wfAgentCount, tools, toolTotal, compactions })}
        </p>
      </div>
    `;
  }

  _summarySentence({ direct, groups, wfAgentCount, tools, toolTotal, compactions }) {
    const parts = [];
    if (direct.length) parts.push(`<b>${direct.length}</b> directly-spawned sub-agent${direct.length === 1 ? "" : "s"}`);
    if (groups.length) parts.push(`<b>${groups.length}</b> workflow run${groups.length === 1 ? "" : "s"} (<b>${wfAgentCount}</b> agents)`);
    if (toolTotal) parts.push(`<b>${toolTotal.toLocaleString()}</b> tool calls across <b>${tools.length}</b> kinds`);
    if (compactions.length) parts.push(`<b>${compactions.length}</b> compaction boundar${compactions.length === 1 ? "y" : "ies"}`);
    if (!parts.length) return "A flat session — no sub-agents, workflows, or compaction recorded.";
    const last = parts.pop();
    return (parts.length ? parts.join(", ") + " and " : "") + last + ". Switch tabs above to explore each.";
  }

  /** A horizontal density strip: message activity (from the loaded window) + compaction cuts.
   *
   *  The density bars come from the loaded window (the start of the session for big transcripts);
   *  compaction `index`es are full-session indices, so we scale the whole strip by the true session
   *  length (`_windowTotal`) when known. Cuts beyond the sampled window still land in the right
   *  place along the full timeline; the sampled region is shaded so the difference is legible. */
  _timelineHtml(session, compactions, hasTools) {
    const msgs = session?.messages || [];
    if (!msgs.length) return "";
    const windowN = msgs.length;
    // Full session length: the known total, else the furthest compaction, else the window.
    const total = session._windowTotal
      || Math.max(windowN, ...(compactions.map((c) => c.index + 1)), windowN);
    const sampled = windowN < total;

    // Bucket the window's messages into columns spanning the *window's* share of the full timeline.
    const COLS = Math.min(120, windowN);
    const buckets = new Array(COLS).fill(0);
    const toolBuckets = new Array(COLS).fill(0);
    msgs.forEach((m, i) => {
      const c = Math.min(COLS - 1, Math.floor((i / windowN) * COLS));
      buckets[c]++;
      if ((m.content || []).some((b) => b.type === "tool_use")) toolBuckets[c]++;
    });
    const max = Math.max(1, ...buckets);
    const windowFrac = windowN / total; // how much of the full strip the window covers
    const bars = buckets.map((v, c) => {
      const x = ((c / COLS) * windowFrac) * 100;
      const tool = toolBuckets[c] > 0;
      const hgt = Math.max(8, Math.round((v / max) * 100));
      return `<span class="tl-bar${tool ? " tool" : ""}" style="left:${x.toFixed(2)}%;height:${hgt}%;width:${((100 / COLS) * windowFrac).toFixed(2)}%"></span>`;
    }).join("");
    const cuts = compactions.map((c) => {
      const x = (c.index / total) * 100;
      const t = c.trigger ? `${c.trigger} compaction` : "compaction";
      return `<span class="tl-compact" style="left:${Math.min(99.7, x).toFixed(2)}%" title="${esc(t)} at message ${c.index}"></span>`;
    }).join("");
    // A shade marking the sampled region (bars only cover this part on big sessions).
    const shade = sampled
      ? `<span style="position:absolute;left:${(windowFrac * 100).toFixed(2)}%;right:0;top:0;bottom:0;background:repeating-linear-gradient(45deg,transparent,transparent 6px,color-mix(in srgb,var(--fg-muted) 6%,transparent) 6px,color-mix(in srgb,var(--fg-muted) 6%,transparent) 12px)" title="not sampled — density shown for the first ${windowN} messages"></span>`
      : "";
    return `
      <div class="ov-card">
        <h3>Timeline <span class="muted" style="font-weight:400;font-size:12px;">${total.toLocaleString()} messages${sampled ? ` · density sampled from first ${windowN.toLocaleString()}` : ""}</span></h3>
        <div class="tl-strip">${shade}${bars}${cuts}</div>
        <div class="tl-legend">
          <span><i style="background:color-mix(in srgb,var(--accent) 55%,transparent)"></i>messages</span>
          ${hasTools ? `<span><i style="background:color-mix(in srgb,var(--h-codex,#6cc) 65%,transparent)"></i>tool turns</span>` : ""}
          ${compactions.length ? `<span><i style="background:var(--warn)"></i>compaction (${compactions.length})</span>` : ""}
        </div>
      </div>`;
  }

  // ── Forest (tree) ──────────────────────────────────────────────────────────
  _forestHtml({ stub, direct, groups }) {
    if (!this._tree?.length) {
      return `<div class="fr-empty">This session spawned no sub-agents.${this._root ? "" : " (Forest data needs a running <code>cvd serve</code>.)"}</div>`;
    }
    const rootRow = `
      <div class="node-row is-root">
        <span class="node-caret" style="visibility:hidden">▸</span>
        <span class="node-glyph">🔮</span>
        <span class="node-label">${esc(truncate(sessionLabel(stub), 64))}</span>
        <span class="node-spacer"></span>
        <span class="node-sub">${(this._tree || []).length} agents</span>
      </div>`;

    const directHtml = direct.map((a) => this._agentNodeHtml(a)).join("");
    const wfHtml = groups.map((g) => this._wfGroupHtml(g)).join("");

    return `
      <div class="tree">
        ${rootRow}
        <div class="node-children">
          ${direct.length ? `<div class="muted" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin:8px 0 4px;">directly spawned (${direct.length})</div>${directHtml}` : ""}
          ${groups.length ? `<div class="muted" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin:12px 0 4px;">workflows (${groups.length})</div>${wfHtml}` : ""}
        </div>
      </div>`;
  }

  _agentNodeHtml(a) {
    const klass = statusClass(a.result_status);
    const label = a.description || sessionLabel(a) || a.agent_id || a.id;
    const n = msgCount(a);
    const badge = a.result_status
      ? `<span class="badge ${klass}">${statusGlyph(klass)} ${esc(truncate(a.result_status, 22))}</span>`
      : "";
    return `
      <div class="node" data-agent="${esc(a.agent_id || a.id)}">
        <div class="node-row" data-toggle>
          <span class="node-caret">▸</span>
          <span class="node-glyph">${a.workflow ? "⚙" : "⑂"}</span>
          <span class="node-label">${esc(truncate(label, 56))}</span>
          ${a.agent_type ? `<span class="atype">${esc(a.agent_type)}</span>` : ""}
          <span class="node-spacer"></span>
          ${badge}
          <span class="node-sub">${n} msg</span>
        </div>
        <div class="node-body" hidden></div>
      </div>`;
  }

  _wfGroupHtml(g) {
    // Status pips: one per agent, colored by outcome — the whole run's health at a glance.
    const pips = g.agents.map((a) => `<span class="wf-pip ${statusClass(a.result_status)}"></span>`).join("");
    const slug = this._wfSlug(g);
    return `
      <div class="wf-group" data-wf="${esc(g.id)}">
        <div class="wf-head" data-wf-toggle>
          <span class="node-caret">▸</span>
          <span class="wf-title">${esc(slug)}</span>
          <span class="wf-id">${esc(g.id)}</span>
          <span class="wf-mini" title="${g.agents.length} agents">${pips}</span>
        </div>
        <div class="wf-body" hidden>
          ${g.agents.map((a) => this._agentNodeHtml(a)).join("")}
        </div>
      </div>`;
  }

  /** A human-ish workflow name: the spawning agents' shared prefix, else the run id. We don't have
   *  the script name here, so derive from the run id token. */
  _wfSlug(g) {
    return "workflow " + g.id.replace(/^wf_/, "");
  }

  _wireForest(root) {
    // Expand/collapse a workflow group.
    root.querySelectorAll("[data-wf-toggle]").forEach((head) => {
      head.addEventListener("click", () => {
        const body = head.parentElement.querySelector(".wf-body");
        const open = body.hasAttribute("hidden");
        body.toggleAttribute("hidden", !open);
        head.querySelector(".node-caret").style.transform = open ? "rotate(90deg)" : "";
      });
    });
    // Expand/collapse + lazy-load an agent transcript.
    root.querySelectorAll(".node-row[data-toggle]").forEach((row) => {
      row.addEventListener("click", () => this._toggleAgent(row));
    });
  }

  async _toggleAgent(row) {
    const node = row.closest(".node");
    const body = node.querySelector(".node-body");
    const open = body.hasAttribute("hidden");
    row.classList.toggle("open", open);
    body.toggleAttribute("hidden", !open);
    if (!open || body.dataset.loaded) return;
    body.dataset.loaded = "1";
    body.innerHTML = `<div class="fr-loading" style="padding:12px">Loading agent transcript…</div>`;
    const agentId = node.dataset.agent;
    const stub = this._rootSession();
    try {
      const full = await getSubagent(stub.harness, stub.id, agentId);
      full._isSubagent = true;
      body.innerHTML = "";
      const t = document.createElement("cv-transcript");
      body.appendChild(t);
      t.session = full;
    } catch (e) {
      body.innerHTML = `<div class="fr-empty" style="padding:12px">Couldn't load this agent (${esc(String(e?.message || e))}).</div>`;
    }
  }

  // ── Workflows (phase lanes) ─────────────────────────────────────────────────
  _workflowsHtml({ groups }) {
    if (!groups.length) return `<div class="fr-empty">No workflow runs in this session.</div>`;
    return groups.map((g) => this._laneHtml(g)).join("");
  }

  _laneHtml(g) {
    // Group the run's agents into status lanes (good / warn / open / bad / neutral), each a column.
    const lanes = { good: [], warn: [], open: [], bad: [], neutral: [] };
    for (const a of g.agents) lanes[statusClass(a.result_status)].push(a);
    const order = [
      ["good", "done"], ["warn", "partial"], ["open", "open"], ["bad", "blocked"], ["neutral", "no status"],
    ];
    const cols = order
      .filter(([k]) => lanes[k].length)
      .map(([k, label]) => `
        <div class="lane-col">
          <h4>${statusGlyph(k)} ${esc(label)} <span class="lc-n">${lanes[k].length}</span></h4>
          ${lanes[k].map((a) => this._agentCardHtml(a)).join("")}
        </div>`).join("");

    return `
      <div class="lane-wrap" data-wf="${esc(g.id)}">
        <div class="lane-head">
          <span class="lh-title">${esc(this._wfSlug(g))}</span>
          <span class="wf-id">${esc(g.id)} · ${g.agents.length} agents</span>
          <span class="lh-script">
            <button type="button" class="mini-btn" data-script="${esc(g.id)}">⌗ view driving script</button>
          </span>
        </div>
        <div class="lane-cols">${cols}</div>
      </div>`;
  }

  _agentCardHtml(a) {
    const klass = statusClass(a.result_status);
    const label = a.description || sessionLabel(a) || a.agent_id;
    const n = msgCount(a);
    return `
      <div class="agent-card ${klass}" data-agent="${esc(a.agent_id || a.id)}">
        <div class="ac-desc">${esc(truncate(label, 88))}</div>
        <div class="ac-foot">
          ${a.agent_type ? `<span class="atype">${esc(a.agent_type)}</span>` : ""}
          ${a.result_status ? `<span class="badge ${klass}">${statusGlyph(klass)} ${esc(truncate(a.result_status, 18))}</span>` : ""}
          <span>${n} msg</span>
        </div>
        ${a.result_summary ? `<div class="ac-summary">${esc(truncate(a.result_summary, 180))}</div>` : ""}
      </div>`;
  }

  _wireWorkflows(root) {
    // Open an agent's transcript in a drill-down (reuse the forest body pattern via a modal-less
    // inline expander appended after the card grid).
    root.querySelectorAll(".agent-card").forEach((card) => {
      card.addEventListener("click", () => this._openAgentInline(card));
    });
    root.querySelectorAll("[data-script]").forEach((btn) => {
      btn.addEventListener("click", () => this._showScript(btn.dataset.script));
    });
  }

  async _openAgentInline(card) {
    const lane = card.closest(".lane-wrap");
    let panel = lane.querySelector(".lane-drill");
    if (!panel) {
      panel = document.createElement("div");
      panel.className = "lane-drill node-body";
      panel.style.margin = "0 14px 14px";
      lane.appendChild(panel);
    }
    const agentId = card.dataset.agent;
    if (panel.dataset.agent === agentId && panel.style.display !== "none") {
      panel.style.display = "none"; panel.dataset.agent = ""; return; // toggle closed
    }
    panel.style.display = "block";
    panel.dataset.agent = agentId;
    panel.innerHTML = `<div class="fr-loading" style="padding:12px">Loading agent transcript…</div>`;
    const stub = this._rootSession();
    try {
      const full = await getSubagent(stub.harness, stub.id, agentId);
      full._isSubagent = true;
      panel.innerHTML = "";
      const t = document.createElement("cv-transcript");
      panel.appendChild(t);
      t.session = full;
      panel.scrollIntoView({ behavior: "smooth", block: "nearest" });
    } catch (e) {
      panel.innerHTML = `<div class="fr-empty" style="padding:12px">Couldn't load this agent (${esc(String(e?.message || e))}).</div>`;
    }
  }

  async _showScript(wfId) {
    const stub = this._rootSession();
    const overlay = document.createElement("div");
    overlay.className = "scr-overlay";
    overlay.innerHTML = `
      <div class="scr-card">
        <div class="scr-head">
          <b>⌗ driving script</b>
          <span class="scr-name">${esc(wfId)}</span>
          <span style="flex:1"></span>
          <button type="button" class="mini-btn scr-close">esc ✕</button>
        </div>
        <div class="scr-body"><div class="fr-loading" style="padding:20px">Loading script…</div></div>
      </div>`;
    this.appendChild(overlay);
    const close = () => overlay.remove();
    overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
    overlay.querySelector(".scr-close").addEventListener("click", close);
    const onKey = (e) => { if (e.key === "Escape") { close(); document.removeEventListener("keydown", onKey); } };
    document.addEventListener("keydown", onKey);

    let data = this._scriptCache.get(wfId);
    if (data === undefined) {
      data = await getWorkflowScript(stub, wfId);
      this._scriptCache.set(wfId, data);
    }
    const bodyEl = overlay.querySelector(".scr-body");
    if (!bodyEl) return;
    if (!data) {
      bodyEl.innerHTML = `<div class="fr-empty" style="padding:20px">No driving script recorded for this run${this._root ? "" : ", or it needs a running <code>cvd serve</code>"}.</div>`;
    } else {
      const head = overlay.querySelector(".scr-name");
      if (head && data.name) head.textContent = data.name;
      bodyEl.innerHTML = `<pre><code>${esc(data.source)}</code></pre>`;
    }
  }

  // ── Tools ───────────────────────────────────────────────────────────────────
  _toolsHtml({ tools, toolTotal }) {
    if (!this._root) {
      return `<div class="fr-empty">Tool analysis needs the session transcript (a running <code>cvd serve</code> or the desktop app).</div>`;
    }
    if (!tools.length) return `<div class="fr-empty">No tool calls in the sampled transcript.</div>`;
    const max = tools[0][1];
    const sampledNote = this._root._windowHasMore
      ? `<p class="muted" style="font-size:12px;margin:0 0 12px;">Sampled from the first ${this._root.messages.length} messages of the session.</p>` : "";

    const bars = tools.map(([name, count]) => {
      const w = (count / max) * 100;
      return `
        <div class="th-row">
          <div class="th-name" title="${esc(name)}">${esc(name)}</div>
          <div class="th-track"><div class="th-fill" style="width:${w.toFixed(1)}%"></div></div>
          <div class="th-count">${count.toLocaleString()}</div>
        </div>`;
    }).join("");

    return `
      <div class="ov-card">
        <h3>Tool usage <span class="muted" style="font-weight:400;font-size:12px;">${toolTotal.toLocaleString()} calls · ${tools.length} kinds</span></h3>
        ${sampledNote}
        <div class="th-bars">${bars}</div>
      </div>
      ${this._heatmapHtml()}
    `;
  }

  /** Cross-agent heatmap: rows = agents (root + sampled sub-agents we have transcripts for),
   *  cols = tools. We only have the root transcript loaded here without N fetches, so the heatmap
   *  shows the root's per-(coarse-phase) tool intensity — a true agents×tools matrix would require
   *  pulling every sub-agent transcript (hundreds), which we deliberately don't do on this view.
   *  Instead: phase × tool intensity across the root, which reads like a timeline-heatmap. */
  _heatmapHtml() {
    const msgs = this._root?.messages || [];
    if (!msgs.length) return "";
    const tools = this._toolHistogram(this._root).slice(0, 16).map(([n]) => n);
    if (!tools.length) return "";
    const PHASES = Math.min(12, Math.max(2, Math.round(msgs.length / 80)));
    const grid = Array.from({ length: PHASES }, () => Object.fromEntries(tools.map((t) => [t, 0])));
    const n = msgs.length;
    msgs.forEach((m, i) => {
      const p = Math.min(PHASES - 1, Math.floor((i / n) * PHASES));
      for (const b of (m.content || [])) {
        if (b.type === "tool_use" && b.name && grid[p][b.name] != null) grid[p][b.name]++;
      }
    });
    let cellMax = 1;
    for (const ph of grid) for (const t of tools) cellMax = Math.max(cellMax, ph[t]);

    const headCols = tools.map((t) => `<th title="${esc(t)}">${esc(truncate(t, 12))}</th>`).join("");
    const rows = grid.map((ph, i) => {
      const lo = Math.floor((i / PHASES) * n), hi = Math.floor(((i + 1) / PHASES) * n);
      const cells = tools.map((t) => {
        const v = ph[t];
        const a = v ? (0.12 + 0.88 * (v / cellMax)) : 0;
        const bg = v ? `background:color-mix(in srgb, var(--accent) ${(a * 100).toFixed(0)}%, transparent)` : "";
        return `<td class="${v ? "" : "z"}" style="${bg}" title="${esc(t)}: ${v}">${v || "·"}</td>`;
      }).join("");
      return `<tr><th title="messages ${lo}–${hi}">▮ ${lo}–${hi}</th>${cells}</tr>`;
    }).join("");

    return `
      <div class="ov-card">
        <h3>Tool intensity across the session <span class="muted" style="font-weight:400;font-size:12px;">phase × tool</span></h3>
        <div class="heat-wrap">
          <table class="heat">
            <thead><tr><th>phase</th>${headCols}</tr></thead>
            <tbody>${rows}</tbody>
          </table>
        </div>
      </div>`;
  }

  // ── Compaction ──────────────────────────────────────────────────────────────
  _compactionHtml({ compactions }) {
    if (!compactions.length) {
      if (!this._root && this._compactions == null) {
        return `<div class="fr-empty">Compaction data needs a running <code>cvd serve</code> or the desktop app.</div>`;
      }
      return `<div class="fr-empty">No compaction boundaries in this session — the context was never compacted.</div>`;
    }
    const windowN = this._root?.messages?.length || 0;
    const items = compactions.map((c, i) => {
      const trig = c.trigger || "compaction";
      const pre = c.preTokens;
      const dur = c.durationMs ? this._dur(c.durationMs) : null;
      const span = c.preSpan ? `${c.preSpan[0].toLocaleString()}–${c.preSpan[1].toLocaleString()}` : null;
      const inWindow = c.index < windowN; // can we preview the surrounding messages inline?
      const hasSummary = !!(c.summary && c.summary.trim());
      return `
        <div class="cz-item" data-idx="${c.index}">
          <div class="cz-num">${i + 1}</div>
          <div class="cz-main">
            <div class="cz-row1">
              <span class="cz-trigger">✂ ${esc(trig)} compaction</span>
              <span class="muted" style="font-size:12px">at message ${c.index.toLocaleString()}</span>
              ${span ? `<span class="muted" style="font-size:12px">· compacted away msgs ${esc(span)}</span>` : ""}
            </div>
            <div class="cz-stats">
              ${pre != null ? `<span><b>${this._kfmt(pre)}</b> tokens of context rebuilt</span>` : ""}
              ${dur ? `<span>took <b>${esc(dur)}</b></span>` : ""}
              ${c.summaryIndex != null ? `<span>summary seeded at msg ${c.summaryIndex.toLocaleString()}</span>` : ""}
            </div>
            ${hasSummary ? `
            <details class="cz-summary" style="margin-top:7px">
              <summary style="cursor:pointer;font-size:12px;color:var(--accent)">▸ summary that carried the context forward</summary>
              <div class="md" style="font-size:12.5px;line-height:1.5;margin-top:6px;padding:8px 10px;background:var(--bg-sunk);border-radius:8px;border:1px solid var(--border);max-height:340px;overflow:auto;white-space:pre-wrap">${esc(c.summary)}</div>
            </details>` : ""}
            ${inWindow ? `
            <div class="cz-jump">
              <button type="button" class="mini-btn" data-jump-pre="${c.index}" title="The messages just before this compaction (pre-compaction context)">↥ before</button>
              <button type="button" class="mini-btn" data-jump-post="${c.index}" title="The messages just after this compaction (the summary + continuation)">↧ after</button>
            </div>` : ""}
            <div class="cz-detail" hidden></div>
          </div>
        </div>`;
    }).join("");

    const complete = compactions[0]?._complete;
    const note = complete === false
      ? `<span class="muted" style="font-weight:400;font-size:12px;"> · sampled from the loaded window</span>` : "";

    return `
      ${this._timelineHtml(this._root || { messages: [] }, compactions, false)}
      <div class="ov-card">
        <h3>Compaction boundaries <span class="muted" style="font-weight:400;font-size:12px;">${compactions.length} · the seams where pre-compaction context is retrievable</span>${note}</h3>
        <div class="cz">${items}</div>
      </div>`;
  }

  _wireCompaction(root) {
    const showSpan = (btn, around, dir) => {
      const item = btn.closest(".cz-item");
      const detail = item.querySelector(".cz-detail");
      const open = detail.hasAttribute("hidden");
      detail.toggleAttribute("hidden", !open);
      if (!open) return;
      const msgs = this._root?.messages || [];
      const span = dir === "pre"
        ? msgs.slice(Math.max(0, around - 4), around)
        : msgs.slice(around, Math.min(msgs.length, around + 5));
      detail.innerHTML = span.length
        ? span.map((m, k) => {
            const idx = (dir === "pre" ? Math.max(0, around - 4) : around) + k;
            const role = (m.role || "?");
            const text = (m.content || [])
              .map((b) => b.type === "text" ? b.text : b.type === "tool_use" ? `⚙ ${b.name}` : "")
              .filter(Boolean).join(" ").replace(/\s+/g, " ");
            return `<div style="font-size:12px;padding:5px 0;border-top:1px solid var(--border)">
              <span class="muted">#${idx} ${esc(role)}</span> ${esc(truncate(text, 160))}</div>`;
          }).join("")
        : `<div class="muted" style="font-size:12px;padding:6px 0">(no messages in this span in the sampled window)</div>`;
    };
    root.querySelectorAll("[data-jump-pre]").forEach((b) =>
      b.addEventListener("click", () => showSpan(b, +b.dataset.jumpPre, "pre")));
    root.querySelectorAll("[data-jump-post]").forEach((b) =>
      b.addEventListener("click", () => showSpan(b, +b.dataset.jumpPost, "post")));
  }

  // ── small format helpers ─────────────────────────────────────────────────────
  _kfmt(n) {
    if (n == null) return "—";
    if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + "M";
    if (n >= 1e3) return (n / 1e3).toFixed(n >= 1e4 ? 0 : 1) + "k";
    return String(n);
  }
  _dur(ms) {
    const s = Math.round(ms / 1000);
    if (s < 60) return `${s}s`;
    const m = Math.floor(s / 60);
    return `${m}m${s % 60 ? ` ${s % 60}s` : ""}`;
  }
}

customElements.define("cv-forest", CvForest);
