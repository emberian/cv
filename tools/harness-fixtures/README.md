# Real-writer fixture generators

Every fixture in `crates/cv-core/tests/fixtures/` that a **harness wrote itself** is regenerated
from here, one command each. That is the point: a fixture cv hand-rolled only proves cv agrees with
cv. A fixture the harness wrote proves the adapter reads what the harness writes — and when the
harness changes, regenerating it is how you find out.

| dir | fixture it produces | what it needs |
|---|---|---|
| [`goose/`](goose/) | `tests/fixtures/goose/sessions-v16-1.51.0.db` | a built `goose` from `~/pug/goose`, python3, sqlite3, curl |
| [`openclaw/`](openclaw/) | `tests/fixtures/openclaw/openclaw-agent.sqlite` | a `~/pug/openclaw` checkout with deps installed, Node ≥ 22.5 |
| [`codex/`](codex/) | no fixture — a **smoke test** that Codex accepts a cv-emitted rollout | an installed `codex`, python3 |
| [`hermes/`](hermes/) | `tests/fixtures/hermes/state-v30.db` | a `~/pug/hermes-agent` checkout, python3 |

All four were run end to end on 2026-09-19 against the pinned checkouts. Each one checks its own
prerequisites first and names the missing one; none of them fails obscurely when a checkout, a
binary or an env var is absent.

None of these needs a paid model call: Goose talks to a stub OpenAI server in this directory, Codex
is pointed at a provider whose base URL is a closed port, and OpenClaw and Hermes are driven through
their own store APIs with no model in the loop.

**They all write to a throwaway home** — `GOOSE_PATH_ROOT`, `OPENCLAW_STATE_DIR`, `CODEX_HOME`,
`HERMES_HOME` — and each refuses to start when the variable is unset or points at the real one.
Hermes needs that guard the most: with `HERMES_HOME` unset Hermes does *not* refuse, it warns and
falls back to `~/.hermes`. Keep that habit in anything you add here.

**Every generator can write somewhere other than the committed fixture** (`--out` for goose, `OUT`
for hermes, `OPENCLAW_OUT` for openclaw). Regenerate to a scratch path and diff before replacing
anything — see each README's "what a regenerated fixture does not match" section, because three of
the four have a real answer to that question.

**Fixture ids are not reproducible.** Goose and Hermes both derive session ids from the clock, so a
regenerated store renumbers everything and the `by_id("20260919_…")` / `const A/R/B/…` assertions in
`crates/cv-core/src/harness/{goose,hermes}.rs` have to be updated to the ids the run prints. That is
not optional: the tests fail on ids before they get to anything interesting.

After regenerating, run the harness's tests (`cargo nextest run -p clustervision-core -E
'test(/<harness>/)'`) and `cargo run -p clustervision -- formats census --harness <h>`: a fixture
that brings new record types with it is exactly the drift the manifests exist to catch.

Related: [`../harness-drift.sh`](../harness-drift.sh) (upstream source vs manifest),
`cv formats check` (adapter source vs manifest), `cv formats census` (real data vs manifest).
