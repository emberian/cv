# Compatibility promise

Stable across minor releases from 0.18:

- Session-row field names and JSON types from `ls --json`, including the common row keys in
  `cv schema --json`.
- Message and block field names, types, discriminant values and minimal-message defaults;
  optional fields may be absent when the source records nothing.
- CLI command and argument names, types, enumerated values, requiredness and defaults, as
  `cv schema --commands --json` lists them.
- HTTP method/path patterns of the read, search, export and task routers.
- Exit 0 on success, 1 on operational failure, 2 on a caller mistake.

New fields, arguments, routes and enum values may appear: ignore unknown fields and
handle unknown enum values. Not covered: help text, human output layout, diagnostic
wording, harness extras, tool input objects, native transcript formats, HTTP bodies beyond the
session/message JSON, every query parameter, and each command's specialized output.

## What the gate checks

CI runs `tools/check-compatibility.py --baseline <released cv> --candidate target/debug/cv`
(the last release's sha256-pinned Linux binary; separate synthetic homes), comparing the command tree, `schema --json` field lists (checked against serialized output),
`ls`/`show` JSON of a Claude and a Codex fixture, exits 1 and 2, and every HTTP
route in the candidate's `schema --json` `routes`: a status the baseline answered must hold.
Additions pass; removed or changed shapes and defaults fail. Route removal is caught once 0.18.0 is
the baseline (older ones list no routes).

A reviewed removal is an `--accept <contract path>` on the CI step until the baseline moves.
0.19 removes `mcp` (`tools`), `scry`, `deferrals`, `search --semantic`, `index --semantic`,
`splice --generate`/`--gen-model`, `pack --context-bytes`, and pre-0.18 CLI/HTTP tombstones
(`adopt`'s legacy `--list`/`--orphans` flags, `rewind --from-compaction`, `dataset --out`, and 410
board/task-shim routes), and widens `show --around` and `export --around` from integer to `N|ID`
string selectors. `crates/cv/tests/announce.rs` tests pipe and
terminal behavior; `python3 -I tools/test-compatibility.py` runs the comparator's mutation tests.

0.20 makes the positional search query conditional: ordinary `cv search` still requires it,
while `cv search --batch` reads queries from stdin. The command schema reports the positional
argument as optional, so CI accepts `commands.search.args.query.required`; existing invocations
retain their meaning. Batch answers have their own documented, opt-in JSON shape.

0.20 also makes the positional pack task conditional: `cv pack TASK` retains corpus recall,
while `cv pack --session SOURCE` produces a bounded overview without a task. CI accepts
`commands.pack.args.task.required`; existing task-based invocations retain their meaning.

0.20 also makes the positional prune session conditional: `cv prune ID` retains session selection,
while `cv prune --agent AGENT` selects a sub-agent directly. CI accepts
`commands.prune.args.id.required`; existing session invocations retain their meaning.

## HTTP transport in 0.19.2

HTTP parsing and response framing now use Hyper. Requests without a `Host` header receive
`400` instead of `403`. For requests carrying both `Content-Length` and chunked
`Transfer-Encoding`, Hyper uses chunked framing and closes the connection after the response;
a pipelined request on that connection is not dispatched. Clients must treat header names
case-insensitively. API routes, authorization checks, and JSON contracts remain unchanged.
