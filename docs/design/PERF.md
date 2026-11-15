# Performance: measured behavior and regression checks

The goal, in ember's words: "We can't be blowing up to GBs of heap. We need to find a way to have
this being hella fast." Stream, never materialize a session or store you do not need whole; never
re-read bytes already seen; use all cores; keep heap bounded by work in flight, not file size;
every index or mark over a file verifies itself on use. The datapath design and open gaps are in
[`DATAPATHS.md`](DATAPATHS.md); the dated logs and per-change evidence files are in git history.
Per-path costs and the no-mode-switch rule for thresholds: [`COMPLEXITY.md`](COMPLEXITY.md).

## Regression checks

CI checks the corpus counting pass against a plain read of the same bytes in the same run
(`tests/perf_budget.rs`). This ratio partly controls runner speed; passing it is a regression
check, not evidence of a small speedup or a throughput promise for real sessions.

The generated-input heap gates (`tools/check-heap-budget.py` and
`tools/check-operation-heap.py`) compare requested live Rust allocations across input sizes,
including 16/64 MiB giant fields, native mmap and positioned reads. These checks remain useful
regression detectors; their bounds do not describe native SQLite allocation or whole-process RSS.

This document records measurements rather than latency, throughput, or fleet-size targets.
First-run discovery, unchanged indexing, appends and full recounts need separate measurements:
they do different work, and a result on one fleet is not a promise for another.

An API window publication validates one baseline with the captured read set attached; it does
not separately revalidate that same set. Cache admission and publication remain separate
checks because a source can change between them. The number of file identity checks therefore
scales with distinct dependencies and publication boundaries, not message count.

## How to measure without lying

- Copy the catalog and index into a scratch `CLUSTERVISION_HOME`; transcripts are read-only. For
  before/after on the real fleet, compare against a frozen APFS clone (`cp -c -R`), deleted after.
- Prefer work counts (instructions, traversed/decoded bytes, allocation requests, peak live heap,
  staged bytes) over wall time on the shared host; when timing, interleave runs and state the load.
  RSS counts file-backed mmap pages; report `footprint` for mmap-heavy harnesses.
- Output must be byte-identical: hash every message in lazy, bulk and full mode before and after,
  run the corpus golden test, and compare `cv show --json` on a sample.

## Measured

Every number below carries its date and conditions; a wall time measured on a loaded host is
reported with the load. The 0.17 release tree (c7aa0c22, 2026-10-16)
was not timed: the host sat at load 100-175 all day, so only load-independent facts about it are
recorded (counts, instructions retired, memory, sizes) under "0.17 facts" below.

**2026-10-14, 0eea676f -> v17-perf.** Release builds, `/usr/bin/time -l`, instructions retired
(repeat to ~1%; the host's load was 25-200, so wall times are wide). Frozen: APFS clone under a
fake HOME, 5,421 sessions (claude 38 GB; codex 40 GB, 3,923 of 4,483 rollouts `.jsonl.zst`,
~89 GB decoded). Live: the real HOME read-only, ~7,000 sessions, scratch `CLUSTERVISION_HOME`.

| operation | 0eea676f | v17-perf |
|---|---|---|
| index, nothing changed, frozen | 7.5-9.0 G insn, 0.74-3.10 s | 4.3-4.5 G, 0.41-0.80 s |
| index, nothing changed, live | 27-36 G, 2.6-5.2 s | 6.7-12.3 G, 0.99-1.61 s |
| index after ~1 MB appended to each of 3 sessions | 9.3 G, 0.95-0.98 s | 5.9-6.1 G, 0.70-0.74 s |
| full recount, catalog deleted, frozen | 21.6 T, 849 s, RSS 2.9 GB | 12.8 T, 834 s, RSS 2.9 GB, footprint 689 MB |
| ls, first run, frozen | 9,546 G, 257 s | 5,178-5,188 G, 158-186 s |
| ls, warm, frozen / live | 0.50 G, 0.06-0.09 s | 0.50 G, 0.06-0.09 s (when no live session changed) |
| `ls -q has:plan`, frozen | 4.9 G, 0.47-1.43 s | 1.7 G, 0.24-0.30 s |
| `show --last 20` | | claude 3.7 G vs 3.6 G; codex 708 MB 12.6 G vs 12.7 G; codex zst 412 MB 28.0 G vs 27.8 G (session counting pass) |
| counting pass | | best of 3: claude 190 MB at 641 MB/s, codex 708 MB at 719 MB/s, codex zst at 174 MB/s decoded |

A live "nothing changed" index also re-reads whatever ember's running sessions appended, so its
range is the live write rate. The compressed path's floor is ruzstd's decode: 18.1-18.6 G for
the 412 MB rollout with ruzstd 0.8.3 or 0.9.1 (C `zstd -dc`: 4.7 G). A cold index decodes each
rollout once since 0.17: discovery hands its decoded bytes to the sweep running beside it.
A full build from an empty home, frozen: 21.6 T, 737 s -> 12.9 T, 866 s, footprint 565 -> 715 MB.
Live: 13.5 T, 966 s, RSS 3.4 GB, footprint 759 MB. `has:` now pays for the Claude tool-results
inventory (a stat per file), which 0.16.1 did not check.

Not reproducible on the 2026-10-14 fleet, so removed: perf-2's recount 41-44 s / 140 MB, unchanged
index 0.03-0.05 s, first listing 7.5-7.8 s, and the 421.5 s live index of 2026-10-13. All
were measured before most codex rollouts were compressed.

**0.17 facts (c7aa0c22, 2026-10-16, release build, live HOME read-only, fresh scratch
`CLUSTERVISION_HOME`, host load 100-175: no wall times).** The fleet is 7,374 top-level sessions
(1,435 Claude transcripts, 4,872 Codex rollouts of which 4,091 `.jsonl.zst`; 39 GB + 42 GB on
disk). Instructions retired, `/usr/bin/time -l`:

| operation | 0.17 | the 2026-10-14 figure |
|---|---|---|
| `ls`, first run on an empty home | 2.36 T; RSS 850 MB, footprint 101 MB | 5.18 T frozen (5,421 sessions) |
| `ls`, warm | 0.61-0.62 G; footprint 16 MB (the first warm read after discovery: 3.4 G) | 0.50 G |
| `ls -q has:plan`, first ever (builds the counts for every session) | 2.81 T; footprint 140 MB | |
| `ls -q has:plan`, after `cv index` | 2.8-3.2 G; footprint 46 MB | 1.7 G frozen |
| `cv index`, first pass on the discovered home | 3.35 T; RSS 3.6 GB, footprint 891 MB | 12.9 T frozen full build |
| `cv index`, second pass | re-read 4,893 of 7,374 sessions, 1.94 T, footprint 538 MB | |
| `cv index`, third pass | 11 changed (live appends), 7,362 unchanged, 17.6 G, footprint 145 MB | 6.7-12.3 G live |
| `show --last 20`, Claude 190 MB | 3.34-3.38 G | 3.7 G |
| `show --last 20`, Codex 708 MB plain | 10.3-11.7 G | 12.6 G |
| `show --last 20`, Codex 227 MB `.jsonl.zst` | 11.3-11.65 G | (412 MB rollout: 28.0 G) |

The index of that home is 2.4 GB of tantivy segments and a 193 MB `catalog.db` (7,374 session
rows, 6,828 scan-cache rows, 1,513,943 events, 7,374 affordance-count rows, 22,120 source
dependencies, 389 watched paths). The second index pass is a defect, not a cost: 4,893 is the
Codex rollout count, so that snapshot's first-pass Codex baselines did not hold. This is historical
evidence; verify the present tree with repeated unchanged passes, including compressed rollouts. The one-time 0.17 upgrade costs on a home built by 0.16.1 (catalog reread
for the new identity, the index rebuild for the `context` field and the title rule, the semantic
re-embed) were **not measured**: the upgrade run was stopped for load. Expect one full reread of
every session by the catalog and one by the indexer, and one re-embed of every session's exchange.

**0.20 candidate fleet (2026-11-14, 2d8f123e and the search-fleet fixes on it).** Release
builds, `/usr/bin/time -l`, the real HOME read-only, a fresh scratch `CLUSTERVISION_HOME`. The
fleet is 10,185 top-level sessions: 6,192 Codex rollouts (5,673 `.jsonl.zst`), 1,896 Claude,
722 Gemini, 501 Antigravity, 328 opencode, 186 Cursor, 174 Kimi and seven smaller stores.
Sessions were being written throughout, and the host load was 10-45 except where stated.
Physical I/O was not measured: `getrusage` reports zero block operations on this APFS host, and
the page cache could not be dropped, so every number is warm-cache. Instructions retired are
the comparable figure; wall times carry the load.

| operation | 2d8f123e | with the fixes |
|---|---|---|
| `cv index`, first pass on an empty home | 7.50 T, 1,188 s at load 100-160; RSS 4.65 GB, footprint 788 MB | not rerun |
| `cv index`, only live sessions changed | 144-170 G, 15-18 s; 42-62 read, 0-3 resumed | 30-67 G, 3.5-9 s; 24-47 read, 24-35 resumed |
| `cv search` (live), 20 hits | 118 G, 11.5-13.3 s; 51 sessions and 494 MiB read live | 16-34 G, 1.8-3.5 s; 16-33 sessions, 0.26-30 MiB |
| `cv --frozen search`, no hit / 20 hits | 9.2 G, 2.0 s / 16.5-17.4 G, 3.9-4.7 s | 7.8 G, 0.8 s / 11.8 G, 1.2 s (load differs; no change intended) |
| `cv --frozen search --batch`, 20 queries | | 0.99 G, 0.10-0.15 s in all; reader setup 1.8-2.6 ms, evaluation 91-131 ms |
| the same 20 queries as 20 `--frozen search` runs | | 236 G, 24.7 s; identical totals |
| `cv ls`, first run on an empty home | | 933 G, 21.3 s; footprint 181 MB |
| `cv ls`, warm | 3.6-5.4 G, 0.7-1.1 s | 3.0-4.5 G, 0.3-0.4 s (load differs; no change intended) |
| `cv --frozen show --json`, 122 MB native Antigravity store | 21.0 G, 2.1-2.5 s system time | 10.0 G, 0.6-0.7 s; output identical |
| `cv --frozen show <agent-id>` / a session id | 8.0 G, 0.47 s / 0.56 G, 0.05 s | unchanged |

The first pass indexed 10,147 sessions; four Antigravity sessions deleted between discovery and
their read were reported in one warning and gone by the next pass. No path-length error and no
per-session parse line appeared in any pass. The home it built: 3.2 GB of tantivy segments,
2.0 GB of columnar events, and a 2.9 GB `catalog.db` of which resume marks are 1.39 GB (1,971
rows) and event targets with their index 1.3 GB.

What the numbers say:

- **An "unchanged" pass is the live sessions.** Baselines held for every quiet session,
  compressed rollouts included (10,095-10,160 of 10,185 unchanged per pass). Before the fixes
  each live session was read whole every pass: one written during its own pass published no
  baseline, and a Codex rollout read mid-turn left no mark.
- **An ordinary frozen search pays for the fleet, a batch query for the query.** About 7.8 G
  per invocation is fixed: the session rows, every meta document, the index's dependency
  records and one stat per session, all to state how far the index trails. Evaluation is
  5-10 ms. Twenty lookups cost 236 G apart and 1 G in one batch. This is the reproducible
  part of issue 33: repeated lookups belong in `--batch`.
- **A frozen command beside a running writer rediscovers the fleet.** With a non-empty
  `catalog.db-wal` (any other `cv` holding the catalog open), a frozen open declines the
  catalog, so every `--frozen` command rereads the stores: 916-931 G and 21-23 s each here,
  and the search footer calls all 10,185 sessions stale. Reproduce on a copy of a built home:
  hold a SQLite connection open after one write to `catalog.db`, then run
  `cv --frozen search <anything>`. Open: a frozen reader needs a snapshot of a catalog whose
  WAL has frames, as `state.db` already has.
- **Agent ids resolve ten times slower than session ids** (8.0 G against 0.56 G; a miss is
  33.7 G, 2.0 s). The catalog rows are loaded and 849 sidecar directories listed per lookup.

Peak requested Rust heap, before -> after the datapaths pass (2026-10-12..14; not re-measured
here, the CI heap gates that bound them pass; compressed Codex now also gated, 6.4 MB flat):

| operation | before | after |
|---|---:|---:|
| Gemini lazy count, frozen 313.8 MB | 841 MB | 42.7 KB |
| Claude lazy count, generated 100 MB escaped text | 365 MB | 10.9 KB |
| Codex tool-only lazy count, 16 -> 64 MiB payloads | 12.4 -> 49.7 MB | 4.3 KB, flat |
| Continue / LM Studio discovery, 16 -> 64 MiB bodies | 33.6 -> 134.7 MB | 1.2-1.4 KB, flat |
| Claude metadata discovery, 64 MiB of outputs | 49.5-68.4 MB | 96 KB, flat |
| Event extraction + SQL publication, 200,000 events | 58.9 MB | 358 KB, flat |
| Doctor, 64 MiB giant fields (Claude/Codex/Gemini) | 33-67 MB | 4.6-201 KB, flat |
| Portable Claude export, two 67 MB escaped payloads | whole payloads | 1.39 MB native, 1.45 MB no-mmap |

Bounded heap is not free CPU: dense escaped text decodes at 2.1-2.3x the old whole-field CPU,
OpenClaw bounded replay costs about 1.5x CPU, and Zed's decoded spill is about 16% above the
original one-parse reader on escaped input. None of these figures bounds native SQLite allocation
or whole-process RSS.

## Resume marks (2026-10-14)

A resume mark (`resume.rs`) is checked by device and inode, length, and a hash of the first 4 KiB
and the 4 KiB before the mark. An in-place edit outside those samples that is followed by growth
is not detected: index and watch resume past it, and the edited bytes keep the docs, events and
counts they were read with until a whole pass (`cv index --rebuild`, a same-size rewrite, or a new
inode). Rewind's prefix receipt has the same limit, and so does the append admission of a captured
`.jsonl` (`source::FileId::admits`): a stat that changed at the captured length (a writer
mid-`write`) is admitted when the samples match, so a same-length edit outside them is not caught.

Hashing the whole prefix on each resume would close the gap, and was rejected. On the largest
local transcript (189,759,498 bytes), the mark's FNV-1a took 1.03 s and 1.22 s (0.16 to 0.19
GB/s, on a machine shared with builds), and reading the page-cached file took 0.05 s. A resume
otherwise reads only the appended suffix. Watch resumes on every append, and the local Claude
store is 33,168 transcripts holding 36.3 GB. A full check therefore turns every resumed pass back
into a whole read of bytes already seen. Detecting such edits needs a writer or filesystem change
signal, which no harness provides.

## Open measurement gaps

Audited against `git log v0.17.0..HEAD` on 2026-10-21, fleet items again on 2026-11-14:

- **Full recount remains unmeasured on the current fleet.** First listing and repeated
  indexing were measured on 2026-11-14 (above). A recount with the catalog deleted was not.
- **Cold-cache and physical I/O figures do not exist.** Every fleet number above is warm-cache
  instructions and wall time; no byte counter covers index or source reads of an ordinary
  search (`--batch` reports logical work only).
- **Upgrade cost is unknown.** The 0.16.1 upgrade gate (`1ff6669c`) checks migrated output against
  a clean index; it does not measure fleet upgrade time, heap or re-embedding cost.
- **Arbitrary prefix-edit detection remains limited** by the samples described above. A full
  hash contradicts the measured suffix-only work goal; no harness supplies a trusted change
  signal. Native SQLite allocation and whole-process RSS likewise remain outside Rust heap gates.
