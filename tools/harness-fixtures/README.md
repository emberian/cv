# Real-writer fixture generators

Every fixture in `crates/cv-core/tests/fixtures/` that a **harness wrote itself** is regenerated
from here, one command each. That is the point: a fixture cv hand-rolled only proves cv agrees with
cv. A fixture the harness wrote proves the adapter reads what the harness writes — and when the
harness changes, regenerating it is how you find out.

| dir | fixture it produces | what it needs |
|---|---|---|
| [`goose/`](goose/) | `tests/fixtures/goose/sessions-v16-1.51.0.db` | a built `goose` from `~/pug/goose`, python3 |
| [`openclaw/`](openclaw/) | `tests/fixtures/openclaw/openclaw-agent.sqlite` | a `~/pug/openclaw` checkout with deps installed |
| [`codex/`](codex/) | no fixture — a **smoke test** that Codex accepts a cv-emitted rollout | an installed `codex` |
| [`hermes/`](hermes/) | `tests/fixtures/hermes/state-v30.db` | a `~/pug/hermes-agent` checkout, python3 |

None of these needs a paid model call: Goose talks to a stub OpenAI server in this directory, Codex
is pointed at a dead base URL on purpose, and OpenClaw and Hermes are driven through their own store
APIs with no model in the loop.

**They all write to a throwaway home.** `GOOSE_PATH_ROOT`, `OPENCLAW_STATE_DIR`, `CODEX_HOME`,
`HERMES_HOME` — never your real one. The OpenClaw generator refuses to run if `OPENCLAW_STATE_DIR`
is unset or points at `~/.openclaw`; keep that habit in anything you add here.

After regenerating, run the harness's tests (`cargo nextest run -p clustervision-core -E
'test(/<harness>/)'`) and `cargo run -p clustervision -- formats census --harness <h>`: a fixture
that brings new record types with it is exactly the drift the manifests exist to catch.

Related: [`../harness-drift.sh`](../harness-drift.sh) (upstream source vs manifest),
`cv formats check` (adapter source vs manifest), `cv formats census` (real data vs manifest).
