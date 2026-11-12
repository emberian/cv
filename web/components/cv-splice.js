// <cv-splice> — the splice composer, `cv splice` in the browser. ✨ The headline.
//
// A three-pane workspace:
//   1. Source picker  — choose any loaded session; its transcript renders with
//      "＋ splice" buttons on each message (pickMode on <cv-transcript>).
//   2. Composition lane — the messages you've collected, in order. Reorder
//      (▲ ▼ or drag), remove (✕), and "branch from here" (a new branch with everything up to it).
//   3. Live preview     — the composed transcript rendered as it will export.
//
// Beyond rearranging, the composer can generate: with an OpenRouter API key
// it GENERATES a continuation from the current composition and appends the
// model's reply as a new assistant message. "Branch from here" + generate spawns
// sibling BRANCHES (named variants) you can switch between — grow a transcript
// into alternate futures and compare them in the Compare view.
//
// Export: download the active branch as an OpenSession JSON document (or .md) —
// pure client-side, no wasm.
//
// API: splice.sessions = Session[]   (setter)
import "./cv-transcript.js";
import "./cv-harness-badge.js";
import "./cv-session-picker.js";
import {
  sessionKey, sessionByKey, esc, sessionLabel, ROLE_LABELS, toOpenSession, toMarkdown,
  downloadFile, slug, randomId,
} from "./util.js";
import {
  generate, getKey, setKey, getModel, setModel, hasKey,
  MODEL_PRESETS, DEFAULT_MODEL,
} from "../openrouter.js";
import { hydrateSession, needsFullSession } from "./hydrate.js";

class CvSplice extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._sourceId = null;
    this._title = "Spliced session";
    this._dragUid = null;
    this._hydratedSrc = new Map(); // id -> full session (with messages)
    this._pendingSrc = new Set();
    this._sourceErrors = new Map();

    // Branch tree. Each branch is a named variant holding its own lane:
    //   { id, name, lane: [{ uid, message, from, harness }], composedId }
    // The active branch is what every pane reads/writes.
    this._branches = [];
    this._activeBranchId = null;
    this._newBranch("main");

    this._showSettings = false;
    this._gen = { busy: false, error: "", streamUid: null };
    this._abort = null;
  }

  // ---- branch model ------------------------------------------------------

  _newBranch(name, lane = [], opts = {}) {
    const b = {
      id: randomId(),
      name: name || `branch ${this._branches.length + 1}`,
      lane,
      composedId: randomId(),
      parent: opts.parent ?? null,   // parent branch id (null for roots)
      branchAt: opts.branchAt ?? 0,      // # of messages shared with the parent
    };
    this._branches.push(b);
    this._activeBranchId = b.id;
    return b;
  }

  _children(id) { return this._branches.filter((b) => b.parent === id); }
  _branch() { return this._branches.find((b) => b.id === this._activeBranchId) || this._branches[0]; }
  get _lane() { return this._branch().lane; }
  set _lane(v) { this._branch().lane = v; }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    this._poolVersion = (this._poolVersion || 0) + 1;
    this._hydratedSrc.clear();
    this._pendingSrc.clear();
    this._sourceErrors.clear();
    if (!sessionByKey(this._sessions, this._sourceId)) {
      this._sourceId = this._sessions[0] ? sessionKey(this._sessions[0]) : null;
    }
    this.render();
  }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  disconnectedCallback() { this._abort?.abort(); }

  // Prefer the hydrated (full) source if we have it; otherwise the stub from the pool.
  _source() {
    return this._hydratedSrc.get(this._sourceId) || sessionByKey(this._sessions, this._sourceId) || null;
  }

  // Pull the full transcript for the current source, then refresh the source pane. No-op if ready.
  _ensureSourceHydrated() {
    const id = this._sourceId;
    const s = sessionByKey(this._sessions, id);
    if (!s || this._hydratedSrc.has(id) || this._pendingSrc.has(id) || this._sourceErrors.has(id) || !needsFullSession(s)) return;
    const version = this._poolVersion;
    this._pendingSrc.add(id);
    this._renderSourceStatus();
    hydrateSession(s)
      .then((full) => {
        if (version !== this._poolVersion) return;
        this._hydratedSrc.set(id, full);
        this._pendingSrc.delete(id);
        const srcT = this.querySelector(".splice-transcript");
        if (srcT && this._sourceId === id) srcT.session = full;
        this._renderSourceStatus();
      })
      .catch((error) => {
        if (version !== this._poolVersion) return;
        this._pendingSrc.delete(id);
        this._sourceErrors.set(id, error?.message || "Read API unavailable");
        this._renderSourceStatus();
      });
  }

  _renderSourceStatus() {
    const status = this.querySelector(".splice-source-status");
    if (!status) return;
    const error = this._sourceErrors.get(this._sourceId);
    status.querySelector("span").textContent = error ? `Couldn't load complete source: ${error}`
      : this._pendingSrc.has(this._sourceId) ? "Loading complete source transcript…" : "";
    status.querySelector("button").hidden = !error;
  }

  _composed() {
    const b = this._branch();
    // An IR session; `toOpenSession` adds the 0.3 marker when it is downloaded.
    return {
      harness: "opensession",
      id: b.composedId,
      title: this._title + (this._branches.length > 1 ? ` — ${b.name}` : ""),
      messages: b.lane.map((e) => e.message),
    };
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>Load sessions, then splice their messages here.</p></div>`;
      return;
    }
    this.innerHTML = `
      <div class="view-head">
        <h2>✨ Splice</h2>
        <span class="muted">Splice messages from any session — or ⚡ generate continuations with OpenRouter — and weave alternate branches.</span>
        <button type="button" class="mini-btn splice-settings-btn" data-settings title="OpenRouter generation settings">⚙ generation</button>
      </div>
      ${this._settingsHtml()}
      ${this._branchBarHtml()}
      <div class="splice-grid">
        <section class="splice-source">
          <div class="splice-pane-head">
            <strong>Source</strong>
            <cv-session-picker class="splice-source-pick" aria-label="Source session"></cv-session-picker>
          </div>
          <div class="splice-source-status muted" role="status"><span></span><button type="button" class="mini-btn" data-source-retry hidden>Retry loading source</button></div>
          <div class="splice-source-body">
            <cv-transcript class="splice-transcript"></cv-transcript>
          </div>
        </section>

        <section class="splice-lane">
          <div class="splice-pane-head">
            <strong>Composition</strong>
            <span class="muted">${this._lane.length} message${this._lane.length === 1 ? "" : "s"}</span>
          </div>
          <div class="splice-title-row">
            <input class="splice-title" type="text" value="${esc(this._title)}" aria-label="Composition title" placeholder="Composition title" />
          </div>
          <ol class="lane-list">${this._laneHtml()}</ol>
          ${this._genBarHtml()}
          <div class="splice-actions">
            <button type="button" class="mini-btn" data-clear ${this._lane.length ? "" : "disabled"}>clear</button>
            <button type="button" class="btn-accent" data-export-json ${this._lane.length ? "" : "disabled"}>⬇ OpenSession .json</button>
            <button type="button" class="mini-btn" data-export-md ${this._lane.length ? "" : "disabled"}>⬇ .md</button>
          </div>
        </section>

        <section class="splice-preview">
          <div class="splice-pane-head"><strong>Live preview</strong></div>
          <div class="splice-preview-body"><cv-transcript class="splice-preview-transcript"></cv-transcript></div>
        </section>
      </div>
    `;

    // Source transcript in pick mode.
    const srcT = this.querySelector(".splice-transcript");
    srcT.pickMode = true;
    srcT.session = this._source();
    srcT.addEventListener("pick-message", (e) => this._addMessage(e.detail.session, e.detail.message));

    // Preview transcript.
    this._renderPreview();

    // Source picker (searchable, not a 2000-option <select>).
    const pick = this.querySelector(".splice-source-pick");
    pick.sessions = this._sessions;
    pick.selection = this._source();
    pick.placeholder = "Pick a source session…";
    pick.addEventListener("pick", (e) => {
      this._sourceId = sessionKey(e.detail.session);
      this.querySelector(".splice-transcript").session = this._source();
      this._ensureSourceHydrated();
      this._renderSourceStatus();
    });
    this.querySelector("[data-source-retry]").addEventListener("click", () => {
      this._sourceErrors.delete(this._sourceId);
      this._ensureSourceHydrated();
    });
    this._renderSourceStatus();
    // Pull the current source's full transcript if it's still a stub.
    this._ensureSourceHydrated();
    this.querySelector(".splice-title").addEventListener("input", (e) => {
      this._title = e.target.value;
      this._renderPreview();
    });
    this.querySelector("[data-clear]")?.addEventListener("click", () => { this._lane = []; this.render(); });
    this.querySelector("[data-export-json]")?.addEventListener("click", () => {
      downloadFile(`${slug(this._composed().title)}.opensession.json`,
        JSON.stringify(toOpenSession(this._composed()), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(this._composed().title)}.md`, toMarkdown(this._composed()), "text/markdown");
    });

    this.querySelector("[data-settings]")?.addEventListener("click", () => {
      this._showSettings = !this._showSettings; this.render();
    });

    this._wireSettings();
    this._wireBranchBar();
    this._wireGenBar();
    this._wireLane();
  }

  _renderPreview() {
    const p = this.querySelector(".splice-preview-transcript");
    if (p) p.session = this._lane.length ? this._composed() : null;
  }

  // ---- settings panel (OpenRouter key + model) ---------------------------

  _settingsHtml() {
    if (!this._showSettings) return "";
    const key = getKey();
    const model = getModel();
    const masked = key ? key.slice(0, 6) + "…" + key.slice(-4) : "";
    const opts = MODEL_PRESETS.map((m) =>
      `<option value="${esc(m)}"${m === model ? " selected" : ""}>${esc(m)}</option>`).join("");
    const inPresets = MODEL_PRESETS.includes(model);
    return `
      <div class="splice-settings">
        <div class="ls-row">
          <label class="ls-label" for="ls-key">OpenRouter API key</label>
          <input id="ls-key" class="ls-key" type="password" autocomplete="off" spellcheck="false"
            placeholder="sk-or-v1-…" value="${esc(key)}" aria-label="OpenRouter API key" />
          <button type="button" class="mini-btn" data-ls-clear ${key ? "" : "disabled"}>forget</button>
        </div>
        <div class="ls-row">
          <label class="ls-label" for="ls-model">Model</label>
          <select id="ls-model-preset" class="ls-model-preset" aria-label="Model preset">
            ${opts}
            <option value="__custom__"${inPresets ? "" : " selected"}>custom…</option>
          </select>
          <input id="ls-model" class="ls-model" type="text" value="${esc(model)}"
            placeholder="provider/model-id" aria-label="Model id" />
        </div>
        <p class="ls-note muted">
          The key is held in <code>sessionStorage</code> for this tab only (never persisted to disk) and is sent
          <strong>only</strong> to <code>openrouter.ai</code> over HTTPS. ${masked ? `Currently: <code>${esc(masked)}</code>.` : "No key set yet."}
        </p>
      </div>`;
  }

  _wireSettings() {
    if (!this._showSettings) return;
    const keyEl = this.querySelector("#ls-key");
    const modelEl = this.querySelector("#ls-model");
    const presetEl = this.querySelector("#ls-model-preset");
    keyEl?.addEventListener("input", () => setKey(keyEl.value.trim()));
    this.querySelector("[data-ls-clear]")?.addEventListener("click", () => {
      setKey(""); this.render();
    });
    presetEl?.addEventListener("change", () => {
      if (presetEl.value !== "__custom__") {
        modelEl.value = presetEl.value;
        setModel(presetEl.value);
      }
      modelEl.focus();
    });
    modelEl?.addEventListener("input", () => {
      setModel(modelEl.value.trim() || DEFAULT_MODEL);
      // Reflect free-text into the preset selector.
      if (presetEl) presetEl.value = MODEL_PRESETS.includes(modelEl.value.trim()) ? modelEl.value.trim() : "__custom__";
    });
  }

  // ---- branch bar --------------------------------------------------------

  // A real branch-tree visualization: roots and their branched descendants,
  // indented by depth, with the branch point (shared message count) shown on each
  // child. The active branch is highlighted; rows are keyboard-navigable.
  _branchBarHtml() {
    const order = this._branchTreeOrder();
    const multi = this._branches.length > 1;
    const rows = order.map(({ b, depth, isLast, ancestorsLast }) => {
      const on = b.id === this._activeBranchId;
      // Build the ASCII-ish guide: vertical bars for open ancestors, an elbow.
      let guide = "";
      for (let d = 0; d < depth; d++) {
        const lastAtThatDepth = ancestorsLast[d];
        guide += `<span class="bt-rail">${d === depth - 1 ? (isLast ? "└" : "├") : (lastAtThatDepth ? " " : "│")}</span>`;
      }
      const branchMark = b.parent ? `<span class="bt-branch muted" title="branched after ${b.branchAt} message${b.branchAt === 1 ? "" : "s"}">⑂${b.branchAt}</span>` : "";
      return `
        <div class="branch-node${on ? " on" : ""}" data-branch="${esc(b.id)}" role="tab"
          aria-selected="${on}" tabindex="${on ? 0 : -1}" title="${esc(b.name)} · ${b.lane.length} msg">
          <span class="bt-guide" aria-hidden="true">${guide}</span>
          <span class="bt-dot"></span>
          <span class="branch-name">${esc(b.name)}</span>
          ${branchMark}
          <span class="branch-count muted">${b.lane.length}</span>
          ${multi ? `<span class="branch-x" data-branch-del="${esc(b.id)}" role="button" tabindex="-1" title="Delete branch (children re-parent)">✕</span>` : ""}
        </div>`;
    }).join("");
    return `
      <div class="branch-tree" role="tablist" aria-label="Branch tree">
        <div class="branch-tree-head">
          <span class="branch-bar-label muted">branch tree</span>
          <button type="button" class="mini-btn branch-add" data-branch-dup title="Duplicate the active branch into a new child">＋ branch</button>
        </div>
        <div class="branch-rows">${rows}</div>
      </div>`;
  }

  // Depth-first pre-order over the parent→child forest, carrying the data the
  // guide rails need (whether each ancestor was the last child of its parent).
  _branchTreeOrder() {
    const out = [];
    const visit = (b, depth, ancestorsLast) => {
      const siblings = this._children(b.parent);
      const isLast = siblings[siblings.length - 1]?.id === b.id;
      out.push({ b, depth, isLast, ancestorsLast });
      const kids = this._children(b.id);
      kids.forEach((k) => visit(k, depth + 1, [...ancestorsLast, isLast]));
    };
    // Roots: parent is null, OR parent no longer exists (defensive).
    const ids = new Set(this._branches.map((b) => b.id));
    const roots = this._branches.filter((b) => !b.parent || !ids.has(b.parent));
    roots.forEach((r) => visit(r, 0, []));
    // Safety: include any branch not reached (shouldn't happen) as a root.
    for (const b of this._branches) if (!out.some((o) => o.b.id === b.id)) out.push({ b, depth: 0, isLast: true, ancestorsLast: [] });
    return out;
  }

  _wireBranchBar() {
    const rows = [...this.querySelectorAll(".branch-node")];
    rows.forEach((b) => {
      b.addEventListener("click", (e) => {
        if (e.target.closest("[data-branch-del]")) return;
        this._activeBranchId = b.dataset.branch; this.render();
      });
      b.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); this._activeBranchId = b.dataset.branch; this.render(); return; }
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          const i = rows.indexOf(b);
          const next = rows[i + (e.key === "ArrowDown" ? 1 : -1)];
          next?.focus();
        }
      });
    });
    this.querySelectorAll("[data-branch-del]").forEach((x) =>
      x.addEventListener("click", (e) => {
        e.stopPropagation();
        this._deleteBranch(x.dataset.branchDel);
      }));
    this.querySelector("[data-branch-dup]")?.addEventListener("click", () => {
      const cur = this._branch();
      const lane = cur.lane.map((e) => ({ ...e, uid: randomId(), message: JSON.parse(JSON.stringify(e.message)) }));
      this._newBranch(`${cur.name}′`, lane, { parent: cur.id, branchAt: cur.lane.length });
      this.render();
    });
  }

  _deleteBranch(id) {
    if (this._branches.length <= 1) return;
    const i = this._branches.findIndex((b) => b.id === id);
    if (i < 0) return;
    const gone = this._branches[i];
    if (gone.id === this._generatingBranch) this._abort?.abort();
    // Re-parent any children onto the deleted branch's parent so the tree stays
    // connected (no orphaned sub-trees).
    for (const b of this._branches) if (b.parent === id) { b.parent = gone.parent; b.branchAt = Math.min(b.branchAt, gone.branchAt); }
    this._branches.splice(i, 1);
    if (this._activeBranchId === id) this._activeBranchId = (gone.parent && this._branches.find((b) => b.id === gone.parent)?.id) || this._branches[Math.max(0, i - 1)].id;
    this.render();
  }

  // ---- generation bar ----------------------------------------------------

  _genBarHtml() {
    const model = getModel();
    const ready = hasKey();
    const busy = this._gen.busy;
    return `
      <div class="splice-gen">
        <button type="button" class="btn-gen" data-generate ${this._lane.length && !busy ? "" : "disabled"}
          title="${ready ? "Send the composition to OpenRouter and append the reply" : "Set an API key in ⚙ generation first"}">
          ${busy ? "⏳ generating…" : "⚡ generate continuation"}
        </button>
        ${busy ? `<button type="button" class="mini-btn" data-gen-stop>stop</button>` : ""}
        <span class="gen-model muted" title="active model">${esc(model)}</span>
        ${!ready ? `<button type="button" class="gen-needkey" data-settings-open>add API key →</button>` : ""}
        ${this._gen.error ? `<div class="gen-error" role="alert">${esc(this._gen.error)}</div>` : ""}
      </div>`;
  }

  _wireGenBar() {
    this.querySelector("[data-generate]")?.addEventListener("click", () => this._generate());
    this.querySelector("[data-gen-stop]")?.addEventListener("click", () => this._abort?.abort());
    this.querySelector("[data-settings-open]")?.addEventListener("click", () => {
      this._showSettings = true; this.render();
    });
  }

  async _generate() {
    if (this._gen.busy || !this._lane.length) return;
    if (!hasKey()) { this._showSettings = true; this._gen.error = "Add an OpenRouter API key first."; this.render(); return; }

    this._gen.busy = true;
    this._gen.error = "";
    this._abort = new AbortController();
    const branch = this._branch();
    this._generatingBranch = branch.id;

    // Append a placeholder assistant message we stream into.
    const placeholder = {
      id: randomId(),
      role: "assistant",
      kind: "reply",
      origin: "model",
      model: getModel(),
      content: [{ type: "text", text: "" }],
      timestamp: new Date().toISOString(),
    };
    const entry = { uid: randomId(), message: placeholder, from: "⚡ generated", harness: "opensession" };
    this._lane.push(entry);
    this._gen.streamUid = entry.uid;
    this.render();

    const setText = (full) => {
      const m = branch.lane.find((e) => e.uid === entry.uid)?.message;
      if (m) {
        m.content = [{ type: "text", text: full }];
        // Update only the preview + this lane row's text, cheaply.
        if (this._activeBranchId !== branch.id) return;
        if (!this._previewFrame) this._previewFrame = requestAnimationFrame(() => {
          this._previewFrame = null;
          if (this._activeBranchId === branch.id) this._renderPreview();
        });
        const row = this.querySelector(`.lane-item[data-uid="${CSS.escape(entry.uid)}"] .lane-preview`);
        if (row) row.textContent = (full || "").replace(/\s+/g, " ").slice(0, 120) || "(generating…)";
      }
    };

    try {
      const messages = branch.lane
        .filter((e) => e.uid !== entry.uid) // send everything *before* the placeholder
        .map((e) => e.message);
      const text = await generate({ messages, signal: this._abort.signal, onToken: (_chunk, full) => setText(full) });
      setText(text);
      if (!text.trim()) {
        // Empty reply — drop the placeholder so we don't leave a blank turn.
        const i = branch.lane.findIndex((e) => e.uid === entry.uid);
        if (i >= 0) branch.lane.splice(i, 1);
        this._gen.error = "Model returned an empty response.";
      }
    } catch (err) {
      // Remove the placeholder on failure (unless we got partial streamed text).
      const cur = branch.lane.find((e) => e.uid === entry.uid)?.message;
      const got = cur?.content?.[0]?.text?.trim();
      if (!got) {
        const i = branch.lane.findIndex((e) => e.uid === entry.uid);
        if (i >= 0) branch.lane.splice(i, 1);
      }
      this._gen.error = err?.name === "AbortError"
        ? "Generation stopped." + (got ? " Kept the partial reply." : "")
        : (err?.message || "Generation failed.");
    } finally {
      if (this._previewFrame) cancelAnimationFrame(this._previewFrame);
      this._previewFrame = null;
      this._generatingBranch = null;
      this._gen.busy = false;
      this._gen.streamUid = null;
      this._abort = null;
      this.render();
      this.querySelector(".lane-list")?.lastElementChild?.scrollIntoView({ block: "nearest" });
    }
  }

  // ---- lane (collection / reorder / branch) --------------------------------

  _addMessage(session, message) {
    // Clone so reordering/removal never mutates the source pool.
    const clone = JSON.parse(JSON.stringify(message));
    this._lane.push({ uid: randomId(), message: clone, from: sessionLabel(session), harness: (session.harness || "").toLowerCase() });
    this.render();
    // Keep the lane scrolled to the newest entry.
    this.querySelector(".lane-list")?.lastElementChild?.scrollIntoView({ block: "nearest" });
  }

  _laneHtml() {
    if (!this._lane.length) {
      return `<li class="lane-empty muted">Empty — click <b>＋ splice</b> on any source message to add it, then <b>⚡ generate</b> a continuation.</li>`;
    }
    return this._lane.map((e, i) => {
      const m = e.message;
      const role = (m.role || "?").toLowerCase();
      const label = ROLE_LABELS[role] || role;
      const gen = e.from === "⚡ generated";
      const preview = this._previewText(m) || (this._gen.streamUid === e.uid ? "(generating…)" : "(empty)");
      return `
        <li class="lane-item lane-role-${esc(role)}${gen ? " lane-gen" : ""}" draggable="true" data-uid="${esc(e.uid)}">
          <div class="lane-grip" title="Drag to reorder">⋮⋮</div>
          <div class="lane-body">
            <div class="lane-head">
              <span class="turn-role">${esc(label)}</span>
              ${gen ? `<span class="lane-genbadge" title="generated by OpenRouter">⚡</span>` : `<cv-harness-badge harness="${esc(e.harness)}"></cv-harness-badge>`}
              <span class="lane-from muted" title="from ${esc(e.from)}">${esc(e.from)}</span>
            </div>
            <div class="lane-preview muted">${esc(preview)}</div>
          </div>
          <div class="lane-ctrls">
            <button type="button" class="lane-btn" data-up="${esc(e.uid)}" title="Move up" ${i === 0 ? "disabled" : ""}>▲</button>
            <button type="button" class="lane-btn" data-down="${esc(e.uid)}" title="Move down" ${i === this._lane.length - 1 ? "disabled" : ""}>▼</button>
            <button type="button" class="lane-btn" data-branch-here="${esc(e.uid)}" title="Branch from here — a new branch keeping up to and including this message">⑂</button>
            <button type="button" class="lane-btn lane-rm" data-rm="${esc(e.uid)}" title="Remove">✕</button>
          </div>
        </li>`;
    }).join("");
  }

  _previewText(m) {
    for (const b of m.content || []) {
      if (b.type === "text" && b.text?.trim()) return b.text.trim().replace(/\s+/g, " ").slice(0, 120);
    }
    for (const b of m.content || []) {
      if (b.type === "tool_use") return `⚙ ${b.name || "tool"}`;
      if (b.type === "tool_result") return `↳ ${String(b.content || "").slice(0, 100)}`;
      if (b.type === "thinking") return `💭 ${(b.text || "[opaque]").slice(0, 100)}`;
    }
    return "";
  }

  _idx(uid) { return this._lane.findIndex((e) => e.uid === uid); }

  _wireLane() {
    const list = this.querySelector(".lane-list");
    if (!list) return;

    list.querySelectorAll("[data-up]").forEach((b) => b.addEventListener("click", () => this._move(b.dataset.up, -1)));
    list.querySelectorAll("[data-down]").forEach((b) => b.addEventListener("click", () => this._move(b.dataset.down, +1)));
    list.querySelectorAll("[data-rm]").forEach((b) => b.addEventListener("click", () => {
      const i = this._idx(b.dataset.rm); if (i >= 0) this._lane.splice(i, 1); this.render();
    }));
    // Branch from here → spawn a SIBLING BRANCH containing the prefix, switch to it.
    list.querySelectorAll("[data-branch-here]").forEach((b) => b.addEventListener("click", () => {
      const i = this._idx(b.dataset.branchHere);
      if (i < 0) return;
      const cur = this._branch();
      const prefix = this._lane.slice(0, i + 1).map((e) => ({ ...e, uid: randomId(), message: JSON.parse(JSON.stringify(e.message)) }));
      const base = cur.name.replace(/′+$/, "");
      this._newBranch(`${base}·branch`, prefix, { parent: cur.id, branchAt: i + 1 });
      this.render();
    }));

    // Drag to reorder.
    list.querySelectorAll(".lane-item").forEach((li) => {
      li.addEventListener("dragstart", (e) => { this._dragUid = li.dataset.uid; li.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; });
      li.addEventListener("dragend", () => { this._dragUid = null; li.classList.remove("dragging"); list.querySelectorAll(".drop-target").forEach((x) => x.classList.remove("drop-target")); });
      li.addEventListener("dragover", (e) => { e.preventDefault(); li.classList.add("drop-target"); });
      li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
      li.addEventListener("drop", (e) => {
        e.preventDefault();
        const from = this._idx(this._dragUid), to = this._idx(li.dataset.uid);
        if (from < 0 || to < 0 || from === to) return;
        const [moved] = this._lane.splice(from, 1);
        this._lane.splice(to, 0, moved);
        this.render();
      });
    });
  }

  _move(uid, delta) {
    const lane = this._lane;
    const i = this._idx(uid), j = i + delta;
    if (i < 0 || j < 0 || j >= lane.length) return;
    [lane[i], lane[j]] = [lane[j], lane[i]];
    this.render();
  }
}

customElements.define("cv-splice", CvSplice);
