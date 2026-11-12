# cv 0.19 design record (2026-10-28)

The handoff from ember's session to akapug's fleet, made the day 0.18.0 shipped. Read in order:

1. `INJECTION-SURFACES.md`: what Claude Code and Hermes expose (hooks, OTel, plugin, store) per
   lifecycle moment, the drift rates per release, what breaks with 50 Hermes writers, what only a
   launcher adds. Ground truth from the installed binaries and the Hermes source.
2. `DESIGN.md`: the proposed 0.19 (reader → recorder + optional launcher; identity at birth; a
   per-host ledger mirrored to a hub; precompact as the first public consumer of checkpoints;
   `host` in the IR; the drift method; what the state of the art says will bite; measurements;
   build order; open decisions). Every claim cites a paper, a system's docs, a file:line, a binary
   offset or a Scry row.
3. `REVIEW-ASTRA.md`: an independent architecture review (gpt-6-astra) of DESIGN.md: the strongest
   objection per section with a concrete failure, five absent contracts, a counter-design, the
   three decisions it would make differently, a one-week vertical slice with falsifiers, and the
   verdict "ship with 7 changes". The handoff adopts those seven.

These three documents form the 2026-10-28 architecture record; where `REVIEW-ASTRA.md` amends
`DESIGN.md` (separated producer/epoch/run identity, single-writer durable ledger, hub ingestion
offsets, large-payload separation, per-session SQLite revision safety, and verified precompact
context-generation handoff), `REVIEW-ASTRA.md` governs.
