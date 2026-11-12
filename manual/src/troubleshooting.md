# Troubleshooting & FAQ

### The app shows the sample data, not my real sessions

Run the **desktop app** (the browser build only takes zip drops) and check nothing else holds
its port.

### "Showing 0 sessions" / nothing found

cv reads harnesses at their standard locations ([Harnesses](harnesses.md)). An empty `cv ls`
means none were found under `$HOME`.

### First `cv ls` is slow, then fast

The first run reads everything (about 3 minutes for 78 GB); later runs take under 0.1 s. Codex,
Grok, Kimi, Qwen, Hermes, Goose, Zed, Continue and OpenCode counts show a dash until a full read
or `cv index`.
See [Architecture](architecture.md).

### `cv port` warns about lost content

An **expected** loss is one the target format cannot hold (LM Studio stores no tool calls, so
they become text); only an **unexpected** one fails `cv port --strict`. See [Cross-harness conversion](conversion.md).

### Sub-agents

Claude Code sub-agents nest under their parent in the app's transcript view, labeled by task
prompt, never in the main list. See
[The app](viewing-and-sharing.md).

### Still stuck?

Open an issue: <https://github.com/emberian/cv/issues>. Weird old transcripts especially
welcome. 🔮
