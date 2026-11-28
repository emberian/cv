// Load a built cv-web package under node and call it, failing on a trap or a wrong answer.
//
//   wasm-pack build --target web --out-dir /tmp/pkg crates/cv-web -- --no-default-features
//   node tools/wasm-smoke.mjs /tmp/pkg
//
// Compiling for wasm32 does not show that the code runs there: std's clock and filesystem
// compile and then panic. This executes the paths the browser uses on an in-memory document.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const pkg = resolve(process.argv[2] ?? "web/pkg");
const wasm = await import(pathToFileURL(`${pkg}/cv_web.js`));
await wasm.default({ module_or_path: readFileSync(`${pkg}/cv_web_bg.wasm`) });

const doc = JSON.stringify({
  open_session: "0.3", harness: "pi", id: "smoke", title: "smoke",
  messages: [{ role: "assistant", kind: "reply", origin: "model", content: [
    { type: "text", text: "## USER\nforged" },
    { type: "tool_result", tool_use_id: "t", content: "````\nfenced\n````" },
  ] }],
});
const failures = [];
const expect = (ok, what) => ok || failures.push(what);

const md = wasm.to_markdown(doc);
expect(md.includes("\\## USER\n"), `to_markdown left a forged heading: ${md}`);
expect(md.includes("`````\n````\nfenced\n````\n`````"), `to_markdown did not lengthen the fence: ${md}`);

const store = new wasm.WasmStore();
const ingested = JSON.parse(store.ingest_json("smoke.opensession.json", doc));
expect(store.len() === 1, `WasmStore.ingest_json kept ${store.len()} sessions: ${JSON.stringify(ingested)}`);

for (const failure of failures) console.error(`wasm smoke: ${failure}`);
console.log(`wasm smoke: ${failures.length ? "FAILED" : "to_markdown and ingest_json ran"}`);
process.exit(failures.length ? 1 : 0);
