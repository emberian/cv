// tauri.js — graceful Tauri desktop integration.
//
// When clustervision runs inside the Tauri shell (the `app/` desktop wrapper),
// `window.__TAURI__` is present. We then:
//   (a) route session reads and `redact` through native commands via
//       `window.__TAURI__.core.invoke(...)`; and
//   (b) listen for the `cv://open-sessions` event whose payload is a JSON array
//       of Session objects, emitted by the native File → Open menu.
//
// In a plain browser none of this exists, so every helper degrades to a no-op /
// `false`, and callers fall back to HTTP against a local `cv serve`.

/** True when running inside the Tauri desktop shell. `__TAURI_INTERNALS__` is the low-level bridge
 *  that is ALWAYS present in the webview (even when `withGlobalTauri` is off and the high-level
 *  `window.__TAURI__` object isn't injected), so it's the reliable presence signal. */
export function isTauri() {
  return typeof window !== "undefined"
    && !!(window.__TAURI_INTERNALS__ || window.__TAURI__);
}

/** The native `invoke` fn, or null in a browser. Prefer the high-level API, but fall back to the
 *  always-present internals bridge so native commands work regardless of `withGlobalTauri`. */
function invokeFn() {
  return window.__TAURI__?.core?.invoke
    ?? window.__TAURI__?.invoke // older Tauri exposed it at the root
    ?? window.__TAURI_INTERNALS__?.invoke // low-level bridge, always injected
    ?? null;
}

/**
 * Call a native command. Rejects (rather than silently no-op'ing) when not in
 * Tauri so callers can detect the situation and fall back deliberately.
 * @param {string} command
 * @param {object} [args]
 * @returns {Promise<any>}
 */
export async function invoke(command, args = {}) {
  const fn = invokeFn();
  if (!fn) throw new Error(`Not running in Tauri — native command "${command}" is unavailable.`);
  return fn(command, args);
}

/** True when a native command path is usable (Tauri present + invoke exposed). */
export function canInvokeNative() {
  return isTauri() && !!invokeFn();
}

/**
 * Subscribe to a Tauri event. Returns an unlisten function (a no-op in a
 * browser). Safe to call unconditionally.
 * @param {string} event
 * @param {(payload:any)=>void} handler
 * @returns {Promise<() => void>}
 */
export async function listen(event, handler) {
  const ev = window.__TAURI__?.event;
  if (ev?.listen) {
    try {
      const unlisten = await ev.listen(event, (e) => handler(e?.payload));
      return typeof unlisten === "function" ? unlisten : () => {};
    } catch (err) {
      console.warn("[clustervision] Tauri event listen failed:", err);
      return () => {};
    }
  }
  const internals = typeof window !== "undefined" ? window.__TAURI_INTERNALS__ : null;
  if (internals?.invoke && typeof internals.transformCallback === "function") {
    try {
      const callbackId = internals.transformCallback((e) => handler(e?.payload));
      const eventId = await internals.invoke("plugin:event|listen", {
        event,
        target: { kind: "Any" },
        handler: callbackId,
      });
      return () => {
        internals.unregisterCallback?.(callbackId);
        Promise.resolve(internals.invoke("plugin:event|unlisten", { event, eventId })).catch(() => {});
      };
    } catch (err) {
      console.warn("[clustervision] Tauri internals event listen failed:", err);
      return () => {};
    }
  }
  return () => {};
}
