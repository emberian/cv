// Small shared helpers used by the web components. No dependencies.

/** Escape text for safe insertion into HTML. */
export function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Session ids are unique within a harness, not across the merged archive. */
export function sessionKey(session) {
  return JSON.stringify([String(session?.harness || "").toLowerCase(), String(session?.id || "")]);
}

/** Identity lookup; a plain id remains supported for older component callers. */
export function sessionByKey(sessions, key) {
  return sessions.find((s) => sessionKey(s) === key)
    || sessions.find((s) => (s.id || "") === key) || null;
}

/** Pretty-print a JSON value, falling back to String() on cycles. */
export function pretty(value) {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** Format an ISO timestamp into a compact local string, or "" if absent/invalid. */
export function fmtTime(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit",
  });
}

/** Short relative-ish label for the most recent activity timestamp. */
export function sortTime(session) {
  const t = session.updated_at || session.created_at || null;
  const d = t ? new Date(t).getTime() : 0;
  return Number.isNaN(d) ? 0 : d;
}

/** All searchable text for a session (title + cwd + every block's text). */
export function searchableText(session) {
  const parts = [];
  if (session.title) parts.push(session.title);
  if (session.cwd) parts.push(session.cwd);
  if (session.model) parts.push(session.model);
  for (const m of session.messages || []) {
    for (const b of m.content || []) {
      switch (b.type) {
        case "text":
        case "thinking":
          if (b.text) parts.push(b.text);
          break;
        case "tool_use":
          if (b.name) parts.push(b.name);
          // Only index string args, not a full JSON.stringify of the input — serializing every
          // tool input across every message was a per-keystroke hot spot for big sessions.
          if (typeof b.input === "string") parts.push(b.input);
          break;
        case "tool_result":
          if (b.content) parts.push(b.content);
          if (b.tool_name) parts.push(b.tool_name);
          break;
        case "file":
          if (b.path) parts.push(b.path);
          break;
      }
    }
  }
  return parts.join("\n").toLowerCase();
}

/** First non-empty *human prompt* text — used as a preview/fallback title.
 *  IR v2 distinguishes a typed prompt from the other things that ride a `user` turn (a
 *  compaction summary is `role: user` too, and using one as a title produced 4 kB "titles"),
 *  so prefer `kind === "prompt"` and only fall back to the role when a source predates kinds. */
export function firstUserText(session) {
  const msgs = session.messages || [];
  for (const m of msgs) {
    if (m.kind !== "prompt") continue;
    for (const b of m.content || []) {
      if (b.type === "text" && b.text && b.text.trim()) return b.text.trim();
    }
  }
  for (const m of msgs) {
    if (m.role !== "user") continue;
    for (const b of m.content || []) {
      if (b.type === "text" && b.text && b.text.trim()) return b.text.trim();
    }
  }
  return null;
}

/** Human label for a session listing. */
export function sessionLabel(session) {
  const raw = session.display_title || session.title || firstUserText(session) || "(untitled)";
  return truncate(raw.replace(/\s+/g, " ").trim(), 80);
}

export function truncate(s, max) {
  s = String(s ?? "");
  if (s.length <= max) return s;
  return s.slice(0, Math.max(0, max - 1)) + "…";
}

/** Message count that works for hydrated sessions and metadata-only stubs (`/api/sessions`). */
export function msgCount(session) {
  // Always a non-negative integer: rows come from other machines too (a fleet peer's
  // `/api/sessions`), and callers interpolate the count into HTML.
  const n = Number(session.messages?.length || session.message_count || 0);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

/** Harness pill as a plain HTML string — same markup/styling as <cv-harness-badge> but with NO
 *  custom-element upgrade, so lists of thousands of rows render fast. */
export function harnessBadge(harness) {
  const h = (harness || "").toLowerCase();
  const label = HARNESS_LABELS[h] || h || "?";
  return `<span class="cv-badge" data-harness="${esc(h)}" title="harness: ${esc(label)}">${esc(label)}</span>`;
}

/** A nicer display label for a filesystem path: keep the tail. */
export function shortPath(p, segs = 3) {
  if (!p) return "";
  const parts = String(p).split("/").filter(Boolean);
  if (parts.length <= segs) return p;
  return "…/" + parts.slice(-segs).join("/");
}

export const ROLE_LABELS = {
  system: "System",
  user: "User",
  assistant: "Assistant",
  tool: "Tool",
};

/** IR v2 `Message::kind` — WHAT a message is. The label is what the transcript prints on the turn;
 *  the glyph is the one-character tell that lets you skim a 1,000-turn session. */
export const MESSAGE_KINDS = {
  prompt:              { label: "Prompt",            glyph: "▸" },
  reply:               { label: "Reply",             glyph: "✦" },
  tool_result:         { label: "Tool result",       glyph: "↳" },
  injected_context:    { label: "Injected context",  glyph: "⟨⟩" },
  system_prompt:       { label: "System prompt",     glyph: "§" },
  notice:              { label: "Notice",            glyph: "ⓘ" },
  compaction_boundary: { label: "Compaction",        glyph: "✂" },
  compaction_summary:  { label: "Compaction summary", glyph: "≡" },
  model_change:        { label: "Model change",      glyph: "⇄" },
  error:               { label: "Error",             glyph: "✖" },
  subagent_spawn:      { label: "Sub-agent spawned", glyph: "⑂" },
  subagent_return:     { label: "Sub-agent returned", glyph: "⑃" },
  branch:              { label: "Branch",            glyph: "⑂" },
  carrier:             { label: "Carrier record",    glyph: "▪" },
};

/** IR v2 `Message::origin` — WHERE a message came from. */
export const ORIGIN_LABELS = {
  human: "you", model: "model", harness: "harness", hook: "hook",
  scheduler: "scheduler", subagent: "sub-agent", import: "imported", unknown: "unknown",
};

/** The kinds that are structural punctuation rather than conversation: they get a rule across the
 *  transcript instead of a turn card. */
export const STRUCTURAL_KINDS = new Set([
  "compaction_boundary", "model_change", "branch", "subagent_spawn", "subagent_return",
]);

/** Label for a message kind, honest about kinds this build has never heard of. */
export function messageKindLabel(kind) {
  if (!kind) return "";
  return MESSAGE_KINDS[kind]?.label || String(kind).replace(/_/g, " ");
}
export function messageKindGlyph(kind) {
  return MESSAGE_KINDS[kind]?.glyph || "•";
}

/** IR v2 `Session::lineage` — the pointers that make a session navigable. */
export const LINEAGE_LABELS = {
  forked_from: "forked from",
  parent: "parent session",
  continued_in: "continued in",
  continues: "continues",
  spawned_by_tool_use: "spawned by tool call",
  agent_path: "agent",
};

export const HARNESS_LABELS = {
  claude: "Claude",
  codex: "Codex",
  grok: "Grok",
  opencode: "OpenCode",
  gemini: "Gemini",
  antigravity: "Antigravity",
  hermes: "Hermes",
  openclaw: "OpenClaw",
  cursor: "Cursor",
  kimi: "Kimi",
  "kimi-code": "Kimi Code",
  qwen: "Qwen",
  lmstudio: "LM Studio",
  cline: "Cline",
  roo: "Roo",
  continue: "Continue",
  goose: "Goose",
  zed: "Zed",
  devin: "Devin",
  "chatgpt-export": "ChatGPT Export",
  "claude-export": "Claude Export",
  opensession: "OpenSession",
};

const HARNESS_SLUG_RE = /^[a-z0-9_-]+$/;

/** Validate a harness slug so untrusted JSON cannot smuggle HTML/CSS syntax via `s.harness`. */
export function sanitizeHarness(value) {
  const h = String(value || "opensession").trim().toLowerCase();
  return HARNESS_SLUG_RE.test(h) ? h : "opensession";
}

// ---------------------------------------------------------------------------
// Normalization — one chokepoint. Every session in the pool passes through here,
// whichever door it came in by: `cv serve`'s HTTP API, the desktop's native `api` command, the
// wasm ingest, or a dropped .json. Every one of them speaks the IR (OpenSession 0.3 *is* the IR):
// a BLOCK is tagged `type`; a MESSAGE has its own `kind` (prompt · reply · tool_result ·
// injected_context · system_prompt · notice · compaction_boundary · compaction_summary ·
// model_change · error · subagent_spawn · subagent_return · branch · carrier) and an `origin`
// (human · model · harness · hook · scheduler · subagent · import); a session carries
// `system_prompt` and `lineage`; keys are snake_case. There is no second vocabulary to fold in:
// an OpenSession 0.2 document (camelCase, blocks tagged `kind`) is refused with the same pointer
// the CLI gives, never half-read.
// ---------------------------------------------------------------------------

/** A usage object's numeric fields (`input_tokens`, `output_tokens`, `cache_read_tokens`,
 *  `cache_creation_tokens`, `reasoning_tokens`, `cost_usd`, …), or undefined when it has none. */
function normUsage(u) {
  if (!u || typeof u !== "object") return undefined;
  const out = {};
  for (const [k, v] of Object.entries(u)) if (typeof v === "number") out[k] = v;
  return Object.keys(out).length ? out : undefined;
}

/** Normalize one content block onto IR v2's `type` tag. */
function normBlock(b) {
  if (!b || typeof b !== "object") return { type: "text", text: String(b ?? "") };
  const type = b.type;

  switch (type) {
    case "text":
      return { type: "text", text: b.text ?? "" };
    case "thinking":
      return {
        type: "thinking",
        text: b.text ?? "",
        signature: b.signature,
        encrypted: b.encrypted,
        redacted: b.redacted,
      };
    case "tool_use":
      return {
        type: "tool_use",
        id: b.id,
        name: b.name,
        input: b.input,
        namespace: b.namespace,
      };
    case "tool_result":
      return {
        type: "tool_result",
        tool_use_id: b.tool_use_id,
        content: b.content,
        is_error: b.is_error ?? false,
        tool_name: b.tool_name,
        status: b.status,
        details: b.details,
      };
    case "file":
      return {
        type: "file",
        mime: b.mime,
        path: b.path,
        source: b.source,
      };
    case "image":
      return {
        type: "image",
        media_type: b.media_type,
        data_ref: b.data_ref,
      };
    default:
      // A block type this build has never heard of. Keep every field so the renderer can show
      // the raw record instead of dropping the turn on the floor.
      return { ...b, type: type ?? "unknown" };
  }
}

/** Only for a message the UI builds itself without a kind. The contract spells these defaults
 *  out: User→Prompt, Assistant→Reply, Tool→ToolResult, System→Notice. */
const KIND_BY_ROLE = { user: "prompt", assistant: "reply", tool: "tool_result", system: "notice" };
const ORIGIN_BY_KIND = { prompt: "human", reply: "model", tool_result: "harness" };

/** Normalize one message: role, IR v2 `kind` + `origin`, usage, blocks. */
function normMessage(m) {
  if (!m || typeof m !== "object") return { role: "user", kind: "prompt", origin: "human", content: [] };
  const content = Array.isArray(m.content) ? m.content.map(normBlock)
    : m.content != null ? [normBlock({ type: "text", text: String(m.content) })]
    : [];
  const role = (m.role || "user").toLowerCase();
  const kind = m.kind || KIND_BY_ROLE[role] || "notice";
  const origin = m.origin || ORIGIN_BY_KIND[kind] || "harness";
  return {
    id: m.id,
    parent_id: m.parent_id,
    role,
    kind,
    origin,
    timestamp: m.timestamp,
    model: m.model,
    usage: normUsage(m.usage),
    content,
    // `extra` is nested by harness in IR v2 (`extra.claude.attachment_type`); pass it through
    // verbatim and let readers reach in via `harnessExtra(m, harness)`.
    extra: m.extra,
  };
}

/** Normalize `Session::lineage`. Returns undefined when the session has no pointers at all, so
 *  `if (s.lineage)` stays a useful question. */
function normLineage(l) {
  if (!l || typeof l !== "object") return undefined;
  const out = {};
  for (const k of ["forked_from", "parent", "spawned_by_tool_use", "continued_in", "continues", "agent_path"]) {
    if (l[k] != null && l[k] !== "") out[k] = l[k];
  }
  return Object.keys(out).length ? out : undefined;
}

/** A message's harness-specific bag. IR v2 nests `extra` under the harness name — never flat —
 *  with exactly two flat exceptions (`_record`, `cv_byte_offset`) that are cv's own bookkeeping. */
export function harnessExtra(m, harness) {
  const e = m?.extra;
  if (!e || typeof e !== "object") return null;
  const h = (harness || "").toLowerCase();
  const own = h && e[h];
  if (own && typeof own === "object") return own;
  // `cv` is the one non-harness namespace the contract allows.
  return null;
}

/**
 * Normalize a single session object from any shape into IR v2 as the components read it.
 * Idempotent: re-normalizing an already-normalized session is a no-op-ish.
 */
export function normalizeSession(s) {
  if (!s || typeof s !== "object") return null;
  refuseOldOpenSession(s);
  return {
    id: s.id ?? randomId(),
    harness: sanitizeHarness(s.harness),
    cwd: s.cwd,
    path: s.path,
    title: s.title,
    display_title: s.display_title,
    created_at: s.created_at,
    updated_at: s.updated_at,
    model: s.model,
    git: s.git,
    // The system prompt the harness sent, when the store keeps it. NOT a message.
    system_prompt: s.system_prompt,
    // Where this session came from and where it went.
    lineage: normLineage(s.lineage),
    extra: s.extra,
    source_path: s.source_path,
    size_bytes: s.size_bytes,
    // Metadata-only "stub" sessions (from `/api/sessions`) carry a count but no messages yet;
    // keep it so the list can show the real length before the transcript is hydrated.
    message_count: s.message_count,
    messages: Array.isArray(s.messages) ? s.messages.map(normMessage) : [],
  };
}

/** An OpenSession document other than 0.3 (a 0.2 `openSession` marker, camelCase keys) is an
 *  error naming the cure, exactly as `cv` refuses it — never a session read half-right. */
function refuseOldOpenSession(s) {
  const found = s.open_session ?? s.openSession;
  if (found != null && found !== OPEN_SESSION_VERSION) {
    throw new Error(
      `OpenSession ${found} document; this reads ${OPEN_SESSION_VERSION} only — re-export the session ` +
      "with `cv export --format json`, or apply the renames in docs/OPENSESSION.md \"0.2 → 0.3\"",
    );
  }
}

/**
 * Accept anything that might be a list of sessions: an array of sessions, a
 * single session object, or an OpenSession document. Returns Session[].
 */
export function normalizeSessions(data) {
  if (Array.isArray(data)) return data.map(normalizeSession).filter(Boolean);
  if (data && typeof data === "object") {
    // A single OpenSession doc, or a wrapper { sessions: [...] }.
    if (Array.isArray(data.sessions)) return data.sessions.map(normalizeSession).filter(Boolean);
    if (data.open_session || data.openSession || data.messages || data.harness) { // refused if not 0.3
      const one = normalizeSession(data);
      return one ? [one] : [];
    }
  }
  return [];
}

/** A loose uuid-ish id (no crypto dependency required, but use it when present). */
export function randomId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  const hex = (n) => Array.from({ length: n }, () => Math.floor(Math.random() * 16).toString(16)).join("");
  return `${hex(8)}-${hex(4)}-4${hex(3)}-${hex(4)}-${hex(12)}`;
}

// ---------------------------------------------------------------------------
// Export — OpenSession JSON + Markdown, fully client-side.
// ---------------------------------------------------------------------------

/** Convert an internal session (or composed list of messages) to an OpenSession doc. */
export function toOpenSession(session) {
  // OpenSession 0.3 IS the IR (docs/OPENSESSION.md): same keys, same snake_case, blocks tagged
  // `type`. So this is a near-identity plus the version marker — the whole camelCase translation
  // layer 0.2 needed is gone, and with it the `messageKind` alias that only existed because 0.2
  // spent the word `kind` on blocks and had nothing left for messages.
  const blockOut = (b) => clean({ ...b });
  const msgOut = (m) => clean({
    id: m.id,
    parent_id: m.parent_id,
    role: m.role,
    kind: m.kind,
    origin: m.origin,
    timestamp: m.timestamp,
    model: m.model,
    usage: m.usage && clean({ ...m.usage }),
    content: (m.content || []).map(blockOut),
    extra: m.extra,
  });
  return clean({
    open_session: OPEN_SESSION_VERSION,
    harness: session.harness || "opensession",
    id: session.id || randomId(),
    cwd: session.cwd,
    title: session.title,
    model: session.model,
    created_at: session.created_at,
    updated_at: session.updated_at,
    git: session.git,
    system_prompt: session.system_prompt,
    source_path: session.source_path,
    lineage: session.lineage,
    messages: (session.messages || []).map(msgOut),
    extra: session.extra,
  });
}

/** The OpenSession version this writes and the only one it reads (see `refuseOldOpenSession`). */
export const OPEN_SESSION_VERSION = "0.3";

/** Drop undefined/null/empty-object fields for tidy JSON. */
function clean(obj) {
  const out = {};
  for (const [k, v] of Object.entries(obj)) {
    if (v == null) continue;
    if (typeof v === "object" && !Array.isArray(v) && Object.keys(v).length === 0) continue;
    out[k] = v;
  }
  return out;
}

/** Render a session as readable Markdown. */
export function toMarkdown(session) {
  const lines = [];
  lines.push(`# ${session.title || sessionLabel(session)}`);
  lines.push("");
  const meta = [];
  if (session.harness) meta.push(`**harness:** ${session.harness}`);
  if (session.model) meta.push(`**model:** ${session.model}`);
  if (session.cwd) meta.push(`**cwd:** \`${session.cwd}\``);
  if (session.git?.branch) meta.push(`**branch:** ${session.git.branch}`);
  if (session.created_at) meta.push(`**created:** ${session.created_at}`);
  if (session.updated_at) meta.push(`**updated:** ${session.updated_at}`);
  if (session.id) meta.push(`**id:** \`${session.id}\``);
  for (const [k, label] of Object.entries(LINEAGE_LABELS)) {
    const v = session.lineage?.[k];
    if (v) meta.push(`**${label}:** \`${v}\``);
  }
  if (meta.length) { lines.push(meta.join("  \n")); lines.push(""); }

  if (session.system_prompt) {
    lines.push("<details><summary>system prompt</summary>");
    lines.push("");
    lines.push("```");
    lines.push(session.system_prompt);
    lines.push("```");
    lines.push("");
    lines.push("</details>");
    lines.push("");
  }

  for (const m of session.messages || []) {
    const role = (m.role || "?").toUpperCase();
    const kind = m.kind && m.kind !== KIND_BY_ROLE[m.role] ? ` · ${messageKindLabel(m.kind)}` : "";
    const origin = m.origin && m.origin !== "model" && m.origin !== "human" ? ` · via ${m.origin}` : "";
    const when = m.timestamp ? ` · ${fmtTime(m.timestamp)}` : "";
    lines.push(`## ${role}${kind}${origin}${when}`);
    lines.push("");
    for (const b of m.content || []) {
      switch (b.type) {
        case "text":
          lines.push(b.text || ""); lines.push(""); break;
        case "thinking":
          lines.push("> 💭 *thinking*");
          lines.push((b.text || (b.encrypted ? "[encrypted reasoning]" : b.redacted ? "[redacted]" : "")).split("\n").map((l) => "> " + l).join("\n"));
          lines.push(""); break;
        case "tool_use":
          lines.push(`**🔧 tool_use → \`${b.namespace ? b.namespace + ":" : ""}${b.name || "?"}\`**`);
          lines.push("```json"); lines.push(pretty(b.input)); lines.push("```"); lines.push(""); break;
        case "tool_result":
          lines.push(`**↳ tool_result${b.is_error ? " (error)" : ""}${b.status ? " · " + b.status : ""}**`);
          lines.push("```"); lines.push(String(b.content ?? "")); lines.push("```"); lines.push(""); break;
        case "file":
          lines.push(`**📄 file:** \`${b.path || b.source || ""}\`${b.mime ? ` (${b.mime})` : ""}`); lines.push(""); break;
        case "image":
          lines.push(`**🖼 image:** ${b.media_type || ""} ${b.data_ref || ""}`.trim()); lines.push(""); break;
        default:
          lines.push(`*[${b.type || "unknown"} block]*`); lines.push(""); break;
      }
    }
  }
  return lines.join("\n");
}

/** Trigger a client-side download of a string as a file. */
export function downloadFile(filename, text, mime = "application/octet-stream") {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

/** A filesystem-safe slug from a label. */
export function slug(s, max = 48) {
  return String(s ?? "session")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, max) || "session";
}

/** Total token usage across a session's messages. */
export function sumTokens(session) {
  let input = 0, output = 0, cacheRead = 0, reasoning = 0, cost = 0, costSeen = false;
  for (const m of session.messages || []) {
    const u = m.usage;
    if (!u) continue;
    input += u.input_tokens || 0;
    output += u.output_tokens || 0;
    cacheRead += u.cache_read_tokens || 0;
    reasoning += u.reasoning_tokens || 0;
    if (u.cost_usd != null) { cost += u.cost_usd; costSeen = true; }
  }
  return { input, output, cacheRead, reasoning, cost: costSeen ? cost : null, total: input + output };
}

/** Format a USD cost the way a transcript wants it: never round a fraction of a cent to "$0.00". */
export function fmtCost(usd) {
  if (usd == null || !Number.isFinite(usd)) return "";
  if (usd === 0) return "$0";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  if (usd < 1) return `$${usd.toFixed(3)}`;
  return `$${usd.toFixed(2)}`;
}

/** Compact token count: 1234 → 1.2k. Coerces untrusted input so non-finite strings never pass raw into innerHTML. */
export function fmtTokens(n) {
  if (n == null || n === "" || typeof n === "boolean") return "";
  const v = typeof n === "number" ? n : Number(n);
  if (!Number.isFinite(v)) return "";
  if (v >= 1e6) return (v / 1e6).toFixed(v >= 1e7 ? 0 : 1) + "M";
  if (v >= 1e3) return (v / 1e3).toFixed(v >= 1e4 ? 0 : 1) + "k";
  return String(v);
}
