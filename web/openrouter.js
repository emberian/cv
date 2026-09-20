// openrouter.js — a tiny, dependency-free client for OpenRouter's
// chat/completions endpoint, plus IR↔chat conversion helpers used by the loom.
//
// The API key lives ONLY in localStorage and is sent ONLY to
// https://openrouter.ai/api/v1/chat/completions (Bearer auth). Nothing else
// ever sees it. There is no build step and no server — this runs in the page.

const ENDPOINT = "https://openrouter.ai/api/v1/chat/completions";
const KEY_LS = "cv-openrouter-key";
const MODEL_LS = "cv-openrouter-model";

/** A few sensible presets; the model field is also free-text. */
export const MODEL_PRESETS = [
  "anthropic/claude-3.5-sonnet",
  "anthropic/claude-3.5-haiku",
  "openai/gpt-4o-mini",
  "openai/gpt-4o",
  "google/gemini-flash-1.5",
  "meta-llama/llama-3.1-70b-instruct",
  "deepseek/deepseek-chat",
];

export const DEFAULT_MODEL = "anthropic/claude-3.5-sonnet";

// ---- credential / preference storage (localStorage only) ------------------

export function getKey() {
  try { return localStorage.getItem(KEY_LS) || ""; } catch { return ""; }
}
export function setKey(k) {
  try {
    if (k) localStorage.setItem(KEY_LS, k);
    else localStorage.removeItem(KEY_LS);
  } catch { /* storage may be unavailable */ }
}
export function hasKey() { return !!getKey(); }

export function getModel() {
  try { return localStorage.getItem(MODEL_LS) || DEFAULT_MODEL; } catch { return DEFAULT_MODEL; }
}
export function setModel(m) {
  try { localStorage.setItem(MODEL_LS, m || DEFAULT_MODEL); } catch { /* ignore */ }
}

// ---- IR → OpenRouter chat conversion --------------------------------------

/** Flatten one IR content block to plain text for the chat API. */
export function blockToText(b) {
  switch (b?.type) {
    case "text":
      return b.text || "";
    case "thinking":
      // Keep reasoning as plain text context; mark it so the model can tell.
      if (b.text) return `(thinking) ${b.text}`;
      if (b.encrypted) return "(thinking: [encrypted])";
      if (b.redacted) return "(thinking: [redacted])";
      return "";
    case "tool_use": {
      let input = "";
      try { input = b.input == null ? "" : JSON.stringify(b.input); } catch { input = String(b.input); }
      return `[tool_use ${b.namespace ? b.namespace + ":" : ""}${b.name || "?"}${input ? " " + input : ""}]`;
    }
    case "tool_result": {
      const body = String(b.content ?? "");
      return `[tool_result${b.is_error ? " (error)" : ""}] ${body}`;
    }
    case "file":
      return `[file ${b.path || b.source || ""}]`;
    case "image":
      return `[image ${b.media_type || ""}]`;
    default:
      return b?.text ? String(b.text) : "";
  }
}

/** Map an IR role to an OpenRouter chat role. `tool` collapses to user context. */
function chatRole(role) {
  const r = (role || "user").toLowerCase();
  if (r === "assistant") return "assistant";
  if (r === "system") return "system";
  if (r === "tool") return "user"; // surface tool output as user-visible context
  return "user";
}

/**
 * Convert an array of IR messages to OpenRouter chat messages.
 * Each IR message's blocks are flattened to a single text content string.
 * Empty messages are dropped. Returns [{ role, content }].
 */
export function messagesToChat(messages) {
  const out = [];
  for (const m of messages || []) {
    const text = (m.content || []).map(blockToText).filter((s) => s && s.trim()).join("\n\n");
    if (!text.trim()) continue;
    out.push({ role: chatRole(m.role), content: text });
  }
  return out;
}

// ---- the request ----------------------------------------------------------

/**
 * Generate a continuation. Sends `messages` (IR) converted to chat format to
 * OpenRouter and resolves to the assistant's reply text.
 *
 * @param {object}  opts
 * @param {object[]} opts.messages       IR messages (the current composition).
 * @param {string}  [opts.model]         model id (defaults to stored/preset).
 * @param {string}  [opts.key]           API key (defaults to stored).
 * @param {AbortSignal} [opts.signal]    cancellation.
 * @param {(chunk:string,full:string)=>void} [opts.onToken]  streaming callback.
 * @returns {Promise<string>} the assistant reply text.
 */
export async function generate({ messages, model, key, signal, onToken } = {}) {
  const apiKey = key ?? getKey();
  if (!apiKey) throw new Error("No OpenRouter API key set. Add one in the loom ⚙ settings.");
  const useModel = model || getModel();
  const chat = messagesToChat(messages);
  if (!chat.length) throw new Error("Nothing to send — the composition has no text content.");

  const stream = typeof onToken === "function";
  const resp = await fetch(ENDPOINT, {
    method: "POST",
    signal,
    headers: {
      "Content-Type": "application/json",
      "Authorization": `Bearer ${apiKey}`,
      // Optional attribution headers OpenRouter recommends; harmless if ignored.
      "HTTP-Referer": location.origin || "https://github.com/emberian/cv",
      "X-Title": "clustervision",
    },
    body: JSON.stringify({ model: useModel, messages: chat, stream }),
  });

  if (!resp.ok) throw await describeError(resp);

  if (stream && resp.body && typeof resp.body.getReader === "function") {
    return await readStream(resp, onToken);
  }

  // Non-streaming fallback.
  const data = await resp.json().catch(() => null);
  const text = data?.choices?.[0]?.message?.content;
  if (typeof text !== "string") {
    throw new Error("OpenRouter returned no message content. Raw: " + truncate(JSON.stringify(data), 300));
  }
  if (onToken) onToken(text, text);
  return text;
}

/** Read an SSE stream of chat.completion.chunk deltas, accumulating content. */
async function readStream(resp, onToken) {
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let full = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    // SSE frames are separated by blank lines; each "data:" line is a JSON chunk.
    let nl;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line || line.startsWith(":")) continue; // keep-alive comment
      if (!line.startsWith("data:")) continue;
      const payload = line.slice(5).trim();
      if (payload === "[DONE]") return full;
      try {
        const obj = JSON.parse(payload);
        const delta = obj?.choices?.[0]?.delta?.content
          ?? obj?.choices?.[0]?.message?.content
          ?? "";
        if (delta) { full += delta; onToken(delta, full); }
      } catch { /* ignore partial/non-JSON frames */ }
    }
  }
  return full;
}

/** Turn a non-OK response into a friendly, specific Error. */
async function describeError(resp) {
  let detail = "";
  try {
    const j = await resp.json();
    detail = j?.error?.message || j?.message || JSON.stringify(j);
  } catch {
    try { detail = await resp.text(); } catch { /* ignore */ }
  }
  detail = truncate(detail, 300);
  switch (resp.status) {
    case 401:
    case 403:
      return new Error(`Auth failed (${resp.status}). Check your OpenRouter API key. ${detail}`);
    case 402:
      return new Error(`Insufficient credits (402). ${detail}`);
    case 429:
      return new Error(`Rate limited (429) — wait a moment and retry. ${detail}`);
    default:
      return new Error(`OpenRouter error ${resp.status}. ${detail}`);
  }
}

function truncate(s, n) {
  s = String(s ?? "");
  return s.length > n ? s.slice(0, n) + "…" : s;
}
