//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod affordance;
// The read API every door maps onto (`cv serve`, the desktop app). Native only: it reads the
// catalog and the event store, neither of which exists on wasm32.
#[cfg(feature = "sqlite")]
pub mod api;
#[cfg(feature = "sqlite")]
pub mod blame;
pub mod catalog;
pub mod compaction;
pub mod config;
pub mod context;
pub mod coverage;
pub mod uncompact;
/// Bounded operation-owned staging and immutable byte replay, shared by output consumers.
pub mod bytes {
    pub use crate::harness::store::bytes::{ByteSource, ByteStage};
}

pub mod bundle;
pub mod codex_handoff;
pub mod dataset;
pub mod digest;
pub mod disclose;
pub mod discover_cache;
pub mod distill;
pub mod doctor;
pub mod emit;
pub mod env;

pub mod events;
// Verifies touches against git and the checkout: native only, like `blame`.
#[cfg(feature = "sqlite")]
pub mod files;
pub mod formats;
pub mod harness;
pub mod hooks;
pub mod html;
mod identity;
pub mod ingest;
pub mod io_admission;
pub mod ir;
pub use identity::SessionKey;
#[cfg(feature = "sqlite")]
pub mod custody;
mod ir_json;
pub use ir_json::PrettyJson;
mod derive;
pub mod json;
pub mod lanes;
pub mod lazy;
pub(crate) mod lockfile;
pub mod offsets;
pub mod overview;
pub mod packs;
pub mod prune;
pub mod publish;
pub mod query;
pub mod redact;
pub mod render;
pub mod restamp;
pub mod resume;
pub mod retired;
pub mod rewind;
pub mod rows;
pub mod sanitize;
pub mod scan;
pub mod source;
pub(crate) mod spill;
pub mod splice;
#[cfg(feature = "sqlite")]
pub mod state;
pub mod stats;
pub mod stream;
pub mod task;
pub mod tools;
pub(crate) mod utf8;
pub mod watch;

pub use emit::{emit, EmitOptions, SidechainOf};
pub use harness::{Adapter, Discovered, EmitResult, Skipped};
pub use ir::*;
pub use json::{Json, JsonRange, JsonView};
pub use lazy::{Resolver, Span, Text, INLINE_MAX};
pub use query::SessionQuery;
pub use restamp::{restamp, restamp_line, Restamped};
pub use stream::{collect, CollectSink, Demand, Flow, MessageSink, TeeSink, Tier, TryFnSink};

/// Compute lowercase 64-char hex BLAKE3 digest of `bytes`.
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// [`blake3_hex`] of everything `input` yields, streamed, with its length.
pub fn blake3_hex_reader(mut input: impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut hasher = blake3::Hasher::new();
    let len = std::io::copy(&mut input, &mut hasher)?;
    Ok((len, hasher.finalize().to_hex().to_string()))
}

/// Read `pack_manifests` from `<cv_home>/state.db` without creating `-wal`/`-shm` sidecars when
/// `--frozen` is active (delegating to [`harness::store::sqlite::open_ro`]).
#[cfg(feature = "sqlite")]
pub fn list_pack_manifests_frozen_safe(cv_home: &std::path::Path) -> anyhow::Result<Vec<state::PackManifest>> {
    if !env::frozen() {
        return Ok(state::StateStore::at_dir(cv_home).list_pack_manifests()?);
    }
    let db_path = cv_home.join("state.db");
    let meta = match std::fs::metadata(&db_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() == 0 {
        return Ok(Vec::new());
    }
    harness::store::sqlite::read(&db_path, |conn, _| {
        let has_table: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pack_manifests')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !has_table {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT pack_id, min_commit_pos, max_commit_pos, record_count, byte_len,
                    digest, pack_path, index_meta_json, published_at
             FROM pack_manifests
             ORDER BY min_commit_pos ASC, pack_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let meta_str: String = r.get(7)?;
            Ok(state::PackManifest {
                pack_id: r.get(0)?,
                min_commit_pos: r.get(1)?,
                max_commit_pos: r.get(2)?,
                record_count: r.get::<_, i64>(3)? as u64,
                byte_len: r.get::<_, i64>(4)? as u64,
                digest: r.get(5)?,
                pack_path: r.get(6)?,
                index_meta: serde_json::from_str(&meta_str).unwrap_or_else(|_| serde_json::json!({})),
                published_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// Replay the default task store without creating `state.db-wal`/`state.db-shm` or `tasks/state.json`
/// when `--frozen` is active.
#[cfg(feature = "sqlite")]
pub fn replay_tasks_frozen_safe() -> anyhow::Result<task::ReplayOutcome> {
    if !env::frozen() {
        return task::replay();
    }
    use std::io::BufRead;
    let tasks_dir = task::tasks_dir();
    let events_path = tasks_dir.join("events.jsonl");
    let mut raw_events: Vec<(usize, task::TaskEvent)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut line_no = 0usize;

    match std::fs::File::open(&events_path) {
        Ok(file) => {
            let mut reader = std::io::BufReader::new(file);
            let mut raw = Vec::new();
            loop {
                raw.clear();
                let n = reader.read_until(b'\n', &mut raw)?;
                if n == 0 {
                    break;
                }
                line_no += 1;
                let terminated = raw.ends_with(b"\n");
                let Ok(text) = std::str::from_utf8(&raw) else {
                    if !terminated {
                        break;
                    }
                    anyhow::bail!(
                        "reading task log {}: invalid UTF-8 at line {line_no}",
                        events_path.display()
                    );
                };
                let line = text.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<task::TaskEvent>(line) {
                    Ok(ev) => {
                        seen_ids.insert(ev.id.clone());
                        raw_events.push((line_no, ev));
                    }
                    Err(_) => {
                        if let Ok(hdr) = serde_json::from_str::<serde_json::Value>(line) {
                            if hdr.get("format").and_then(|v| v.as_str()) == Some("cv-task-log") {
                                if let Some(v) = hdr.get("v").and_then(|v| v.as_u64()) {
                                    if v > 1 {
                                        anyhow::bail!(
                                            "task log {} declares format v{v} (this cv reads v1) — upgrade before touching it",
                                            events_path.display()
                                        );
                                    }
                                    continue;
                                }
                            }
                        }
                        if !terminated
                            && matches!(serde_json::from_str::<&serde_json::value::RawValue>(line), Err(e) if e.is_eof())
                        {
                            break;
                        }
                        let snippet: String = line.chars().take(80).collect();
                        warnings.push(format!(
                            "task log {}: line {} is not a valid event (log may be corrupted): {}",
                            events_path.display(),
                            line_no,
                            snippet
                        ));
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let db_path = tasks_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty() && tasks_dir.file_name().and_then(|n| n.to_str()) == Some("tasks"))
        .map(|p| p.join("state.db"))
        .unwrap_or_else(|| tasks_dir.join("state.db"));
    if std::fs::metadata(&db_path).is_ok_and(|m| m.is_file() && m.len() > 0) {
        let db_rows: Vec<String> = harness::store::sqlite::read(&db_path, |conn, _| {
            let has_table: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_events')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if !has_table {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare("SELECT raw_json FROM task_events ORDER BY seq ASC")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        for raw_json in db_rows {
            line_no += 1;
            match serde_json::from_str::<task::TaskEvent>(&raw_json) {
                Ok(ev) => {
                    if seen_ids.insert(ev.id.clone()) {
                        raw_events.push((line_no, ev));
                    }
                }
                Err(e) => warnings.push(format!("state.db task_events row {line_no}: {e}")),
            }
        }
    }

    let mut reducer = task::TaskReducer::new();
    let mut events = Vec::with_capacity(raw_events.len());
    let mut quarantined = 0usize;
    for (ln, ev) in raw_events {
        match reducer.apply(&ev) {
            Ok(()) => events.push(ev),
            Err(e) => {
                quarantined += 1;
                warnings.push(format!(
                    "task log {}: line {} quarantined ({})",
                    events_path.display(),
                    ln,
                    e
                ));
            }
        }
    }
    Ok(task::ReplayOutcome {
        model: reducer.into_model(),
        events,
        warnings,
        quarantined,
    })
}

use anyhow::Result;

/// Discover every session from every registered harness on this machine — the **full scan**, and
/// the slow path. Adapters are queried in parallel (each is independent and `Send + Sync`), so the
/// cost is the slowest single adapter rather than the sum. Disable the `parallel` feature (e.g. on
/// wasm32) for an identical sequential pass.
///
/// Side effects: flushes the per-file scan cache, replaces each successfully-scanned harness's
/// rows + freshness watches in the catalog, and stamps the full-sync time — which is what makes
/// [`sessions`] a trustworthy fast read afterwards. Most read-side callers want [`sessions`]
/// instead; call this when you need a guaranteed-fresh scan (e.g. `cv ls --fresh`, indexers).
pub fn discover_all() -> Vec<SessionRef> {
    discover_all_inventory().refs
}

/// [`discover_all`], handing every rollout it decodes in full to `handoff`'s consumer (an index
/// sweep running beside it), so that consumer reads each one without decoding it again.
pub fn discover_all_handing_off(handoff: codex_handoff::Handoff) -> Vec<SessionRef> {
    codex_handoff::within(&Some(handoff), discover_all)
}

/// Full discovery's available sessions and failures. A partial inventory remains useful for
/// refreshing found sessions, but must never authorize deleting absent sessions from derived stores.
pub struct DiscoveryInventory {
    pub refs: Vec<SessionRef>,
    /// Failed adapters plus individually skipped sources. Details are reported during discovery
    /// and, for skipped sources, recorded in the catalog.
    pub errors: usize,
}

pub fn discover_all_inventory() -> DiscoveryInventory {
    discover_all_with_catalog(catalog::Reader::open())
}

/// Full discovery that refuses an incomplete inventory. Whole-store replacement must not
/// mistake an adapter failure or skipped source for an authoritative empty result.
pub fn discover_all_checked() -> Result<Vec<SessionRef>> {
    let inventory = discover_all_inventory();
    anyhow::ensure!(
        inventory.errors == 0,
        "session discovery was incomplete; previous full-text index retained"
    );
    Ok(inventory.refs)
}

/// Full discovery with a completeness result and the decode-once handoff.
pub fn discover_all_handing_off_inventory(handoff: codex_handoff::Handoff) -> DiscoveryInventory {
    codex_handoff::within(&Some(handoff), discover_all_inventory)
}

fn discover_all_with_catalog(mut catalog: Option<catalog::Reader>) -> DiscoveryInventory {
    let _sources = source::read_scope();
    let _cache_scope = catalog.as_ref().map(catalog::Reader::discovery_scope);
    let discovery = discover_cache::read::Context::current();
    let adapters = harness::all();
    let mut known = std::collections::HashMap::<Harness, Vec<SessionRef>>::new();
    for r in catalog
        .as_ref()
        .map(catalog::Reader::primary_sessions)
        .unwrap_or_default()
    {
        known.entry(r.harness).or_default().push(r);
    }

    let jobs: Vec<_> = adapters
        .into_iter()
        .map(|a| {
            let refs = known.remove(&a.harness()).unwrap_or_default();
            (a, refs)
        })
        .collect();
    drop(known);

    // Per-adapter outcome: what it found on success, `None` on error (the catalog keeps that
    // harness's previous rows rather than wiping them over a transient failure). Rootless
    // adapters are a successful empty scan — their catalog rows (root deleted since last sync)
    // must clear.
    fn run(
        a: &dyn Adapter,
        known: Vec<SessionRef>,
    ) -> (
        Harness,
        Vec<std::path::PathBuf>,
        Option<Discovered>,
        catalog::PrimaryReads,
    ) {
        let h = a.harness();
        let roots = a.roots();
        if roots.is_empty() {
            return (h, roots, Some(Discovered::default()), Default::default());
        }
        let before = catalog::capture_primary_reads_from(a, &known);
        drop(known); // Retain source guards, not a second fleet of metadata, during discovery.
        match a.discover() {
            Ok(found) => (h, roots, Some(found), before),
            Err(e) => {
                eprintln!("cv: discover failed for {h}: {e:#}");
                (h, roots, None, before)
            }
        }
    }

    let inventory = source::inventory_context();
    let env = env::Env::scoped();
    let handoff = codex_handoff::current();
    let run_job = |(a, known): (Box<dyn Adapter>, Vec<SessionRef>)| {
        let discover = || codex_handoff::within(&handoff, || discovery.run(|| run(a.as_ref(), known)));
        env::within(&env, || match &inventory {
            Some(context) => context.run(discover),
            None => discover(),
        })
    };

    #[cfg(feature = "parallel")]
    let mut results: Vec<_> = {
        use rayon::prelude::*;
        jobs.into_par_iter().map(run_job).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let mut results: Vec<_> = jobs.into_iter().map(run_job).collect();

    // Persist freshly-scanned metadata (pruning vanished files) so the next discovery reuses it.
    if catalog.is_none() {
        catalog = catalog::Reader::open();
    }
    if let Some(catalog) = &mut catalog {
        catalog.refill_turns(
            results
                .iter_mut()
                .filter_map(|(_, _, found, _)| found.as_mut())
                .flat_map(|found| found.refs.iter_mut()),
        );
        catalog.persist_discovery(true);
    } else {
        discover_cache::persist(true);
    }

    // Publish every successful harness in one SQL snapshot. Enumeration remains fresh;
    // unchanged rows can be retained only after comparing the complete ordered inventory.
    let publications: Vec<_> = results
        .iter()
        .filter_map(|(h, roots, found, before)| {
            found
                .as_ref()
                .zip(harness::rootless(*h))
                .map(|(found, a)| (*h, found.refs.as_slice(), catalog::watches_for(a, roots, found), before))
        })
        .collect();
    let skipped: Vec<_> = results
        .iter()
        .filter_map(|(h, _, found, _)| Some((*h, found.as_ref()?.skipped.as_slice())))
        .collect();
    note_skipped(&skipped);
    if let Some(catalog) = &mut catalog {
        catalog.publish_all(&publications);
        catalog.record_skipped(&skipped);
    }
    drop((publications, skipped));
    #[allow(unused_mut)]
    let mut errors: usize = results
        .iter()
        .map(|(_, _, found, _)| found.as_ref().map_or(1, |f| f.skipped.len()))
        .sum();
    #[allow(unused_mut)]
    let mut refs: Vec<SessionRef> = results
        .into_iter()
        .filter_map(|(_, _, found, _)| found)
        .flat_map(|found| found.refs)
        .collect();
    #[cfg(feature = "sqlite")]
    if let Err(error) = extend_with_custody(&mut refs, None) {
        eprintln!("cv: could not read session custody: {error:#}");
        errors += 1;
    }
    DiscoveryInventory { refs, errors }
}

/// The one stderr line a discovery that could not open some sources prints per process; the
/// sources themselves are in the catalog, where `cv doctor` names them.
fn note_skipped(skipped: &[(Harness, &[Skipped])]) {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let n: usize = skipped.iter().map(|(_, s)| s.len()).sum();
    if n > 0 && !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("cv: {n} source(s) could not be read and are missing from the listing; `cv doctor` names them");
    }
}

/// Every known session, served from the catalog when it's provably fresh — the **fast read path**
/// (~ms against a warm catalog, vs ~seconds for [`discover_all`]'s stat-the-fleet scan). This is
/// what `cv ls`/`stats`, id resolution and `cv adopt` read. Followers continue to call
/// [`discover_all`] each poll, independently of catalog synchronization.
///
/// A never-synced or overaged catalog (default 900 seconds, configurable with
/// `CLUSTERVISION_MAX_STALE_SECS`) runs full discovery. Otherwise [`catalog::probe_stale`]
/// checks watched directories/DB sidecars, every primary source's captured identity, and
/// registered roots. Changed inputs scope discovery to affected harnesses. Quiet-session
/// appends are covered irrespective of their timestamp rank. New uncached primary sources
/// conservatively require one later scoped pass to establish their before-read baseline.
///
/// Directory-watch gaps (a first file in a previously empty deeper directory) remain bounded
/// by the time backstop. Metadata identities do not hash file contents. `cv ls --fresh` or
/// `CLUSTERVISION_MAX_STALE_SECS=0` forces full discovery.
pub fn sessions() -> Vec<SessionRef> {
    with_custody(sessions_impl().0, None)
}

/// Enumerate the requested harness without opening unrelated stores. A qualified scope uses
/// fresh discovery with its per-file metadata cache and publishes only that harness's rows and
/// watches; it never stamps a full-fleet sync. An unqualified scope retains [`sessions`]' cache
/// and freshness policy. Discovery failures remain best effort, returning an empty scope.
pub fn sessions_for(harness: Option<Harness>) -> Vec<SessionRef> {
    let Some(harness) = harness else {
        return sessions();
    };
    let mut reader = catalog::Reader::open();
    let refs = discover_harness(harness, reader.as_mut()).unwrap_or_default();
    if let Some(reader) = &mut reader {
        reader.persist_discovery(false);
    } else {
        discover_cache::persist(false);
    }
    with_custody(refs, Some(harness))
}

/// Like [`sessions_for`], propagating custody-store read errors instead of falling back to live
/// refs alone.
pub fn try_sessions_for(harness: Option<Harness>) -> Result<Vec<SessionRef>> {
    #[allow(unused_mut)]
    let mut refs = match harness {
        None => sessions_impl().0,
        Some(h) => {
            let mut reader = catalog::Reader::open();
            let refs = discover_harness(h, reader.as_mut()).unwrap_or_default();
            if let Some(reader) = &mut reader {
                reader.persist_discovery(false);
            } else {
                discover_cache::persist(false);
            }
            refs
        }
    };
    #[cfg(feature = "sqlite")]
    extend_with_custody(&mut refs, harness)?;
    Ok(refs)
}

#[cfg(feature = "sqlite")]
fn extend_with_custody(refs: &mut Vec<SessionRef>, harness: Option<Harness>) -> Result<()> {
    let backings = custody::inventory()?;
    if !backings.is_empty() {
        refs.retain(|r| r.path.exists());
        let live: std::collections::HashSet<_> = refs.iter().map(SessionRef::session_selector).collect();
        refs.extend(
            backings
                .into_iter()
                .map(|b| b.reference)
                .filter(|r| harness.is_none_or(|h| h == r.harness) && !live.contains(&r.id)),
        );
    }
    Ok(())
}

fn with_custody(mut refs: Vec<SessionRef>, harness: Option<Harness>) -> Vec<SessionRef> {
    #[cfg(feature = "sqlite")]
    if let Err(error) = extend_with_custody(&mut refs, harness) {
        eprintln!("cv: could not read session custody: {error:#}");
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (&mut refs, harness);
    }
    refs
}

/// The catalog exactly as it stands: no freshness probe, no discovery, never a fleet scan. Empty
/// on a cold catalog. For callers that must answer in milliseconds whatever the store's state
/// — the `cv hook` commands Claude Code runs on its own events — and can say "as of the last
/// `cv ls` / `cv archive sync`" honestly. Everything else wants [`sessions`].
pub fn cataloged_sessions() -> Vec<SessionRef> {
    catalog::all_sessions().unwrap_or_default()
}

/// [`sessions`] plus whether the answer came from a full [`discover_all`] (so `find` knows not to
/// escalate to a second full scan).
fn sessions_impl() -> (Vec<SessionRef>, bool) {
    let max_stale = env::var("CLUSTERVISION_MAX_STALE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(900);
    let mut catalog = catalog::Reader::open();
    let fresh_enough = catalog
        .as_ref()
        .and_then(catalog::Reader::last_full_sync)
        .is_some_and(|t| (chrono::Utc::now().timestamp() - t) < max_stale);
    if fresh_enough {
        if let Some(stale) = catalog.as_ref().and_then(catalog::Reader::probe_stale) {
            let mut unpersisted = Vec::new();
            if !stale.is_empty() {
                let reader = catalog.as_mut().expect("staleness was read through this owner");
                for h in stale {
                    if let Some(fresh) = refresh_harness(h, reader) {
                        unpersisted.push((h, fresh));
                    }
                }
                reader.persist_discovery(false);
            }
            if let Some(rows) = catalog.as_ref().and_then(catalog::Reader::all_sessions) {
                return (splice_refreshed_sessions(rows, unpersisted), false);
            }
        }
    }
    if catalog.as_ref().is_some_and(catalog::Reader::take_rediscovery_notice) {
        eprintln!(
            "cv: note: re-discovering the fleet once for this cv version (minutes on a large fleet; `cv index` after upgrading does it ahead of time)"
        );
    }
    (discover_all_with_catalog(catalog).refs, true)
}

/// Re-discover one harness and replace its catalog rows + watches — the probe's scoped escalation.
/// Returns `Some(fresh_refs)` when discovery succeeded without persisting to `catalog.db` (e.g.
/// under `--frozen` or a read-only catalog) so the caller can splice them in memory; on a discover
/// error `None` keeps the previous catalog rows (matching [`discover_all`]'s tolerance).
fn refresh_harness(h: Harness, catalog: &mut catalog::Reader) -> Option<Vec<SessionRef>> {
    let (refs, persisted) = discover_harness_inner(h, Some(catalog))?;
    (!persisted).then_some(refs)
}

/// Replace any stale harnesses' catalog rows in memory when their scoped refresh did not persist
/// to `catalog.db`, preserving discovery's registration order and last-occurrence deduplication.
fn splice_refreshed_sessions(
    mut rows: Vec<SessionRef>,
    unpersisted: Vec<(Harness, Vec<SessionRef>)>,
) -> Vec<SessionRef> {
    if unpersisted.is_empty() {
        return rows;
    }
    let mut replacements = std::collections::BTreeMap::new();
    for (h, refs) in unpersisted {
        let last: std::collections::HashMap<(String, String), usize> = refs
            .iter()
            .enumerate()
            .map(|(i, r)| ((r.id.clone(), r.path.to_string_lossy().into_owned()), i))
            .collect();
        let deduped: Vec<SessionRef> = refs
            .into_iter()
            .enumerate()
            .filter(|(i, r)| last.get(&(r.id.clone(), r.path.to_string_lossy().into_owned())) == Some(i))
            .map(|(_, r)| r)
            .collect();
        replacements.insert(harness::rank(h), deduped);
    }
    rows.retain(|r| !replacements.contains_key(&harness::rank(r.harness)));
    for (rank, fresh) in replacements {
        if fresh.is_empty() {
            continue;
        }
        let at = rows
            .iter()
            .position(|r| harness::rank(r.harness) > rank)
            .unwrap_or(rows.len());
        rows.splice(at..at, fresh);
    }
    rows
}

#[cfg(test)]
thread_local! {
    /// Scoped discoveries this thread ran: the agent-lookup test's counter.
    static HARNESS_DISCOVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// `fs::read_dir` calls made while scanning or verifying sub-agent stems on this thread.
    static SUBAGENT_READ_DIRS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_subagent_read_dir() {
    SUBAGENT_READ_DIRS.with(|n| n.set(n.get() + 1));
}

/// Enumerate one complete harness scope without certifying freshness of the rest of the fleet.
/// Returns the freshly discovered refs even when the catalog cannot be opened or published.
fn discover_harness(h: Harness, catalog: Option<&mut catalog::Reader>) -> Option<Vec<SessionRef>> {
    discover_harness_inner(h, catalog).map(|(refs, _)| refs)
}

fn discover_harness_inner(h: Harness, mut catalog: Option<&mut catalog::Reader>) -> Option<(Vec<SessionRef>, bool)> {
    #[cfg(test)]
    HARNESS_DISCOVERIES.with(|n| n.set(n.get() + 1));
    let a = harness::for_harness(h)?;
    let _sources = source::read_scope();
    let _cache_scope = catalog.as_ref().map(|reader| reader.discovery_scope());
    let writable = !env::frozen();
    let before = catalog
        .as_ref()
        .filter(|_| writable)
        .map(|reader| reader.capture_primary_reads(a.as_ref()))
        .unwrap_or_default();
    let roots = a.roots();
    let Discovered { mut refs, skipped } = if roots.is_empty() {
        Discovered::default()
    } else {
        match a.discover() {
            Ok(found) => found,
            Err(e) => {
                eprintln!("cv: discover failed for {h}: {e:#}");
                return None;
            }
        }
    };
    note_skipped(&[(h, &skipped)]);
    let mut persisted = false;
    if let Some(reader) = catalog.as_mut() {
        reader.refill_turns(refs.iter_mut());
        if writable {
            persisted = reader.replace_harness(h, &refs, &catalog::watches_for(a.as_ref(), &roots, &refs), &before);
            reader.record_skipped(&[(h, &skipped)]);
        }
    }
    Some((refs, persisted))
}

/// Map `f` over `items`, keeping the `Some` results — in parallel when the `parallel` feature is on
/// (used by the file-heavy adapters to scan many transcripts at once), sequentially otherwise. The
/// output order is not guaranteed to match the input.
pub(crate) fn par_filter_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Option<R> + Sync + Send,
{
    let inventory = source::inventory_context();
    let env = env::Env::scoped();
    let f = |item| {
        env::within(&env, || match &inventory {
            Some(context) => context.run(|| f(item)),
            None => f(item),
        })
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        items.into_par_iter().filter_map(f).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        items.into_iter().filter_map(f).collect()
    }
}

/// Discovery-only propagation: standalone batches share one lazy connection, while operations
/// inherit their catalog owner's capability. Ordinary parallel consumers pay no context cost.
pub(crate) fn par_discovery_filter_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Option<R> + Sync + Send,
{
    let context = discover_cache::read::Context::current();
    let handoff = codex_handoff::current();
    par_filter_map(items, |item| {
        codex_handoff::within(&handoff, || context.run(|| f(item)))
    })
}
pub(crate) fn par_discovery_flat_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Vec<R> + Sync + Send,
{
    let context = discover_cache::read::Context::current();
    let handoff = codex_handoff::current();
    par_flat_map(items, |item| {
        codex_handoff::within(&handoff, || context.run(|| f(item)))
    })
}

/// Like [`par_filter_map`] but each input may yield zero, one, or many results (e.g. a Gemini
/// `logs.json` holding several sessions). Output order is not guaranteed.
pub(crate) fn par_flat_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Vec<R> + Sync + Send,
{
    let inventory = source::inventory_context();
    let env = env::Env::scoped();
    let f = |item| {
        env::within(&env, || match &inventory {
            Some(context) => context.run(|| f(item)),
            None => f(item),
        })
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        items.into_par_iter().flat_map_iter(f).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        items.into_iter().flat_map(f).collect()
    }
}

/// Fields demanded from the sub-agent forest's metadata and workflow journals.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubagentDemand {
    /// Session references and workflow identity only.
    Sessions,
    /// Session references, workflow identity and agent type for tool analytics.
    Tools,
    /// Task descriptions, spawning call and journaled outcomes too.
    Rich,
}

/// Select forest members by transcript identity and workflow before reading their bodies or
/// optional metadata. Results preserve the full forest's newest-first ordering.
pub fn subagent_tree_filtered_of(
    r: &SessionRef,
    demand: SubagentDemand,
    workflow: Option<&str>,
    select: impl Fn(&str, Option<&str>) -> bool + Sync,
) -> Result<Vec<AgentRef>> {
    harness::rootless(r.harness).map_or_else(|| Ok(Vec::new()), |a| a.forest(r, demand, workflow, &select))
}

/// The full sub-agent **forest** a session spawned: directly-spawned (`Agent`/`Task`) sub-agents
/// *and* `Workflow` sub-agents (which live a tier deeper), each enriched with its `*.meta.json`
/// sidecar (agent type / task description / spawning tool_use) and, for workflow agents, the
/// orchestrator's journaled result (`status` + `summary`). Newest first. The adapter's
/// [`harness::Adapter::forest`]: empty for a store that records none.
pub fn subagent_tree_of(r: &SessionRef) -> Result<Vec<AgentRef>> {
    subagent_tree_filtered_of(r, SubagentDemand::Rich, None, |_, _| true)
}

/// The run store of `h`'s adapter, when it keeps one ([`harness::Adapter::workflows`]).
pub(crate) fn workflow_store(h: Harness) -> Option<&'static dyn harness::Workflows> {
    harness::rootless(h)?.workflows()
}

/// Every session whose adapter keeps a run store, with that store: the catalog's rows
/// ([`sessions`], freshness-probed), never a rediscovery of the harness per call.
fn workflow_sessions() -> Vec<(SessionRef, &'static dyn harness::Workflows)> {
    sessions()
        .into_iter()
        .filter_map(|r| workflow_store(r.harness).map(|w| (r, w)))
        .collect()
}

pub use harness::claude_workflow::{LaneWork, Workflow, WorkflowAgent, WorkflowLaunch, WorkflowPhase};

/// Every `Workflow`-tool run a session launched, as first-class [`Workflow`] objects (phase tree →
/// agents → outcomes + the driving script), newest first. Read from the session's
/// `workflows/wf_*.json` state files. Empty where the adapter keeps no run store.
pub fn workflows_of(r: &SessionRef) -> Vec<Workflow> {
    workflow_store(r.harness).map_or_else(Vec::new, |w| w.runs(r))
}

/// Find workflow runs by **name** (exact, else prefix) across the whole catalog — so a workflow
/// can be addressed without knowing which session launched it (session titles are auto-generated
/// and rarely mention the workflow's name). Returns `(session, run)` pairs, newest-session first.
/// Cheap: the sidecar `workflows/` dir is only read for sessions that have one.
pub fn find_workflows_by_name(name: &str) -> Vec<(SessionRef, Workflow)> {
    let mut refs = workflow_sessions();
    refs.sort_by_key(|(r, _)| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    let want = name.to_string();
    let per_session: Vec<Vec<(SessionRef, Workflow)>> = par_filter_map(refs, move |(r, store)| {
        // The cheap name index picks candidates; only matching state files get fully parsed.
        let hits: Vec<(SessionRef, Workflow)> = store.named(&r, &want).into_iter().map(|w| (r.clone(), w)).collect();
        (!hits.is_empty()).then_some(hits)
    });
    let all: Vec<(SessionRef, Workflow)> = per_session.into_iter().flatten().collect();
    let (exact, prefix): (Vec<_>, Vec<_>) = all.into_iter().partition(|(_, w)| w.name.as_deref() == Some(name));
    if exact.is_empty() {
        prefix
    } else {
        exact
    }
}

/// Find the parent session(s) of a sub-agent by its `agent-…` id (or bare/prefix form) — so an
/// agent can be opened directly (`cv show <agent-id>`) without first knowing which session
/// spawned it. Served from `catalog.db`'s directory-stat-guarded `subagents` stem index when
/// available, falling back to a parallel filename scan ([`harness::Workflows::has_agent`]) when
/// the catalog cannot be opened or `sqlite` is disabled.
pub fn find_subagent_parents(agent_id: &str) -> Vec<SessionRef> {
    let want = format!("agent-{}", agent_id.strip_prefix("agent-").unwrap_or(agent_id));
    if let Some(parents) = lookup_subagent_parents_indexed(&want) {
        return parents;
    }
    let refs = workflow_sessions();
    let mut out = par_filter_map(refs, move |(r, store)| store.has_agent(&r, &want).then_some(r));
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    out
}

fn lookup_subagent_parents_indexed(want: &str) -> Option<Vec<SessionRef>> {
    let mut reader = catalog::Reader::open()?;
    let wf_harnesses: Vec<(Harness, &'static dyn harness::Workflows)> = Harness::ALL
        .into_iter()
        .filter_map(|h| workflow_store(h).map(|w| (h, w)))
        .collect();
    if wf_harnesses.is_empty() {
        return Some(Vec::new());
    }
    let max_stale = env::var("CLUSTERVISION_MAX_STALE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(900);
    let harness_ids: Vec<Harness> = wf_harnesses.iter().map(|(h, _)| *h).collect();
    let stale = reader.probe_workflow_harnesses(&harness_ids, max_stale)?;
    let mut unpersisted = std::collections::HashMap::new();
    if !stale.is_empty() {
        for h in stale {
            if let Some((fresh_refs, persisted)) = discover_harness_inner(h, Some(&mut reader)) {
                if !persisted {
                    unpersisted.insert(h, fresh_refs);
                }
            }
        }
        reader.persist_discovery(false);
    }
    reader.lookup_subagent_parents(want, &wf_harnesses, &unpersisted)
}

/// Fleet-wide ghost hunt by name-prefix: [`workflow_ghosts_of`] across every session that
/// recorded at least one workflow (the bounded set where the transcript-vs-state cross-check is
/// meaningful), in parallel. For finding a swarm you remember by name that left no run record.
pub fn find_ghost_launches_by_name(name: &str) -> Vec<(SessionRef, WorkflowLaunch)> {
    let refs = workflow_sessions();
    let name = name.to_string();
    let mut out: Vec<(SessionRef, WorkflowLaunch)> = par_filter_map(refs, move |(r, store)| {
        if !store.has_records(&r) {
            return None;
        }
        let hits: Vec<(SessionRef, WorkflowLaunch)> = store
            .ghosts(&r)
            .into_iter()
            .filter(|g| g.name.as_deref().is_some_and(|n| n.starts_with(name.as_str())))
            .map(|g| (r.clone(), g))
            .collect();
        (!hits.is_empty()).then_some(hits)
    })
    .into_iter()
    .flatten()
    .collect();
    out.sort_by_key(|(_, g)| std::cmp::Reverse(g.ts));
    out
}

/// Transcript `Workflow` launches with **no recorded run** — the crash-forensics view: a power
/// loss / hard kill can leave a launch in the transcript whose `workflows/wf_*.json` state file
/// was never persisted (its sub-agent debris, if any, sits under `subagents/workflows/`).
/// Empty where the adapter keeps no run store.
pub fn workflow_ghosts_of(r: &SessionRef) -> Vec<WorkflowLaunch> {
    workflow_store(r.harness).map_or_else(Vec::new, |w| w.ghosts(r))
}

/// Resolve a workflow's ID/name selector without reading its undemanded rich bodies.
pub fn workflow_id_of(r: &SessionRef, key: &str) -> Option<String> {
    workflow_store(r.harness)?.run_id(r, key)
}

/// One workflow run by its run ID or unambiguous name; None when no run resolves.
pub fn workflow_of(r: &SessionRef, run_id: &str) -> Option<Workflow> {
    workflow_store(r.harness)?.run(r, run_id)
}

/// Find a single session by id (optionally constrained to one harness), returning its ref + adapter.
///
/// An exact id match always wins and returns immediately. Otherwise the id is treated as a prefix:
/// a single prefix hit is returned, but *multiple* distinct prefix hits are an error rather than a
/// silent "first one wins" — callers should disambiguate (e.g. by passing a longer id or a harness).
/// When several files carry the same id (one session under two storage roots), the most recently
/// updated one is returned.
pub fn find(id: &str, harness: Option<Harness>) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
    match find_inner(id, harness)? {
        (Some(hit), _) => Ok(Some(hit)),
        // An unqualified probe-path miss escalates to the full scan (a session in one of the probe's documented
        // blind spots) — unless the probe path already was one (cold catalog): a miss is a miss.
        // A qualified miss has exhausted its harness. An `agent-…` shaped id checks the sub-agent
        // index before paying a 23-harness full scan, and once more if the full scan ran.
        (None, true) => find_subagent_session(id, harness),
        (None, false) => {
            if let Some(sub) = find_subagent_session(id, harness)? {
                return Ok(Some(sub));
            }
            match resolve_id(discover_all(), id, harness)? {
                Some(hit) => Ok(Some(hit)),
                None if !env::frozen() => find_subagent_session(id, harness),
                None => Ok(None),
            }
        }
    }
}

/// [`find`] without the final full-fleet escalation: the catalog fast path plus the (cheap)
/// staleness probe, but never a full re-discovery. For callers with their own fallback
/// interpretations of a non-matching id — a workflow name, a sub-agent id — that shouldn't pay a
/// multi-second full scan on every such miss. Escalate yourself via [`find`] once every cheaper
/// interpretation has failed.
pub fn find_cheap(id: &str, harness: Option<Harness>) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
    match find_inner(id, harness)?.0 {
        Some(hit) => Ok(Some(hit)),
        None => find_subagent_session(id, harness),
    }
}

/// Top-level session resolution without sub-agent fallback or full-fleet escalation, returning
/// `(hit, exhausted)` so CLI callers with their own sub-agent / `<parent>/<agent>` interpretations
/// can interleave them once and escalate via [`find_top_level_escalate`] without repeating
/// `sessions_impl()` or `find_subagent_parents()`.
pub fn find_top_level_cheap(id: &str, harness: Option<Harness>) -> Result<(Option<FoundSession>, bool)> {
    find_inner(id, harness)
}

/// Blind-spot full-fleet escalation for top-level sessions after [`find_top_level_cheap`] missed.
/// Does nothing when `exhausted` is already `true` (cold catalog or `--harness`-qualified lookup).
pub fn find_top_level_escalate(id: &str, harness: Option<Harness>, exhausted: bool) -> Result<Option<FoundSession>> {
    if exhausted || harness.is_some() {
        return Ok(None);
    }
    resolve_id(discover_all(), id, harness)
}

/// The sub-agent fallback behind [`find`]/[`find_cheap`]: an `agent-…` id no pool/catalog session
/// carries is resolved as a workflow/`Task` sub-agent transcript (those live in `subagents/`
/// sidecars, never the main pool), so every front-end that resolves session ids — MCP
/// `read_session`, `cv serve` routes, exports — can drill into an agent id directly.
///
/// Cost discipline: gated on the literal `agent-` prefix (and a harness constraint whose adapter
/// keeps no run store),
/// so a normal session-id miss pays **zero** extra work; only agent-shaped ids pay the same
/// parallel filename scan `cv show <agent-id>` already does. Multiple parent sessions — or
/// several agents matching a prefix under the one parent — are an error listing the candidates,
/// mirroring `cv show`'s ambiguity handling rather than a silent "first one wins".
fn find_subagent_session(id: &str, harness: Option<Harness>) -> Result<Option<FoundSession>> {
    if !id.starts_with("agent-") || harness.is_some_and(|h| workflow_store(h).is_none()) {
        return Ok(None);
    }
    let parents = find_subagent_parents(id);
    let parent = match parents.as_slice() {
        [] => return Ok(None),
        [one] => one,
        // Copies of ONE agent under several sessions (`cv adopt` moved it into a live session):
        // the newest parent's copy is the one that continues, so it wins.
        many if one_agent_across(many, id)? => &many[0],
        many => {
            let mut names: Vec<String> = many
                .iter()
                .map(|p| format!("{} ({})", p.id, p.title.as_deref().unwrap_or("untitled")))
                .collect();
            names.sort();
            anyhow::bail!(
                "{} sessions have a sub-agent matching {id:?}: {} — pass a longer agent id",
                names.len(),
                names.join(", ")
            );
        }
    };
    // Within the one parent, mirror `resolve_id`'s contract: exact match wins, a unique prefix
    // resolves, several distinct prefix hits are an ambiguity error.
    let mut hits: Vec<SessionRef> = Vec::new();
    for s in subagent_tree_filtered_of(parent, SubagentDemand::Sessions, None, |candidate, _| {
        candidate.starts_with(id)
    })? {
        if s.session.id == id {
            return Ok(harness::for_harness(s.session.harness).map(|a| (s.session, a)));
        }
        if s.session.id.starts_with(id) {
            hits.push(s.session);
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => {
            let r = hits.pop().unwrap();
            Ok(harness::for_harness(r.harness).map(|a| (r, a)))
        }
        _ => Err(ambiguous(id, hits.iter())),
    }
}

/// Whether every sub-agent matching `id` (`agent-` optional) under `parents` is the same agent:
/// copies of one agent in several sessions, as `cv adopt` leaves them.
pub fn one_agent_across(parents: &[SessionRef], id: &str) -> Result<bool> {
    Ok(shared_agent_copies(parents, id)?.is_some())
}

/// When every sub-agent matching `id` (`agent-` optional) under `parents` has the same agent id,
/// returns the count of distinct tip transcript paths across those parents (`1` for a single
/// `/resume`-split agent, `> 1` for separate adopted copies); `None` otherwise.
pub fn shared_agent_copies(parents: &[SessionRef], id: &str) -> Result<Option<usize>> {
    let want = format!("agent-{}", id.strip_prefix("agent-").unwrap_or(id));
    let mut ids = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for parent in parents {
        for s in subagent_tree_filtered_of(parent, SubagentDemand::Sessions, None, |candidate, _| {
            candidate.starts_with(&want)
        })? {
            if s.session.id.starts_with(&want) {
                ids.insert(s.session.id);
                paths.insert(s.session.path);
            }
        }
    }
    Ok((ids.len() == 1).then_some(paths.len()))
}

/// A resolved session: its catalog ref plus the harness adapter that reads it.
pub type FoundSession = (SessionRef, Box<dyn Adapter>);

/// The shared catalog-then-probe resolution behind [`find`]/[`find_cheap`]. The `bool` reports
/// whether discovery already exhausted the requested scope (fleet or qualified harness).
fn find_inner(id: &str, harness: Option<Harness>) -> Result<(Option<FoundSession>, bool)> {
    // Fast path: the persisted catalog resolves the id without touching the fleet. We trust a row
    // only if its file still exists (a stale row — session deleted/moved — falls through to scan).
    let live: Vec<SessionRef> = catalog::lookup(id, harness)
        .into_iter()
        .filter(|r| r.path.exists())
        .collect();
    if let Some(r) = pick(live, id)? {
        if let Some(a) = harness::for_harness(r.harness) {
            return Ok((Some((r, a)), false));
        }
    }

    // A selector can identify an archived twin even when a live session shares its native ID.
    #[cfg(feature = "sqlite")]
    let archived = custody::inventory()?
        .into_iter()
        .filter(|b| {
            harness.is_none_or(|h| h == b.reference.harness)
                && (b.reference.id == id
                    || SessionKey::split_selector(&b.reference.id).is_some_and(|(native, _)| native.starts_with(id)))
        })
        .collect::<Vec<_>>();
    // A qualified lookup's universe is one harness. Its complete enumeration resolves prefixes
    // without opening unrelated stores; scoped publication must not stamp a full-fleet sync.
    if let Some(h) = harness {
        let mut reader = catalog::Reader::open();
        let refs = discover_harness(h, reader.as_mut()).unwrap_or_default();
        if let Some(reader) = &mut reader {
            reader.persist_discovery(false);
        } else {
            discover_cache::persist(false);
        }
        let live = resolve_id(refs, id, Some(h))?;
        #[cfg(feature = "sqlite")]
        let live = custody::or_archived(live, archived, id)?;
        return Ok((live, true));
    }

    // Slow path (cold/stale catalog): freshen via the probe — usually a few hundred stats plus at
    // most a scoped re-discovery, and it re-warms the catalog.
    let (refs, was_full) = sessions_impl();
    let live = resolve_id(refs, id, harness)?;
    #[cfg(feature = "sqlite")]
    let live = custody::or_archived(live, archived, id)?;
    Ok((live, was_full))
}

/// Match `id` (exact first, then as a prefix) against `refs` — `find`'s matching contract.
/// Multiple distinct prefix hits are an error rather than a silent "first one wins".
fn resolve_id(
    refs: Vec<SessionRef>,
    id: &str,
    harness: Option<Harness>,
) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
    let hits: Vec<SessionRef> = refs
        .into_iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h) && (r.id.starts_with(id) || r.session_selector() == id))
        // Mirror the fast path: never return a session whose file has vanished (deleted between
        // the scan and now, or a stale probe-path catalog row). Stat only the id matches — cheap.
        .filter(|r| r.path.exists())
        .collect();
    Ok(pick(hits, id)?.and_then(|r| harness::for_harness(r.harness).map(|a| (r, a))))
}

/// Choose among the live sessions matching `id` — `find`'s matching contract, shared by the
/// catalog and scan paths. An exact id beats a prefix. Several files can carry one session id (a
/// Claude transcript copied into a second config dir and continued there, both under read roots):
/// they resolve to the most recently updated copy rather than an arbitrary one. Prefix hits naming
/// more than one distinct session are an error rather than a silent "first one wins".
fn pick(mut hits: Vec<SessionRef>, id: &str) -> Result<Option<SessionRef>> {
    hits.sort_by(|a, b| (a.id != id).cmp(&(b.id != id)).then_with(|| a.preferred_copy_cmp(b)));
    if hits.first().is_some_and(|r| r.id == id) {
        return Ok(hits.into_iter().next());
    }
    let mut seen = std::collections::HashSet::new();
    hits.retain(|r| seen.insert((r.harness, r.id.clone())));
    match hits.len() {
        0 | 1 => Ok(hits.pop()),
        _ => Err(ambiguous(id, hits.iter())),
    }
}

/// The "ambiguous prefix" error shared by `find`'s fast and slow paths.
fn ambiguous<'a>(id: &str, hits: impl Iterator<Item = &'a SessionRef>) -> anyhow::Error {
    let mut ids: Vec<String> = hits.map(|r| format!("{}:{}", r.harness.as_str(), r.id)).collect();
    ids.sort();
    anyhow::anyhow!(
        "ambiguous session id {id:?} matches {} sessions: {} — pass a longer id or --harness",
        ids.len(),
        ids.join(", ")
    )
}

/// A hermetic world whose environment this thread's library calls read (see [`env`]).
#[cfg(test)]
pub(crate) fn test_world(tag: &str) -> cv_testkit::InProcess<env::Isolated> {
    cv_testkit::World::in_process(tag, env::Env::enter_isolated)
}

/// Bounded text projections for API and search consumers.
pub mod projection;

#[cfg(test)]
mod copy_authority_tests {
    use super::*;

    #[test]
    fn qualified_cold_lookup_only_discovers_its_harness_and_preserves_resolution() {
        let world = crate::test_world("qualified-discovery");
        let codex = world.home.join(".codex/sessions");
        std::fs::create_dir_all(&codex).unwrap();
        let write = |id: &str| {
            let path = codex.join(format!("rollout-{id}.jsonl"));
            let row = serde_json::json!({"type":"session_meta","payload":{"id":id}});
            std::fs::write(&path, format!("{row}\n")).unwrap();
            path
        };
        let alpha = write("alpha");
        world.write_session("unrelated", &[serde_json::json!({"type":"user","sessionId":"unrelated","message":{"role":"user","content":"other store"}})]);
        let (hit, exhausted) = find_inner("alpha", Some(Harness::Codex)).unwrap();
        assert!(exhausted);
        assert_eq!(hit.unwrap().0.path, alpha);
        #[cfg(feature = "sqlite")]
        {
            let reader = catalog::Reader::open().unwrap();
            assert!(
                reader.last_full_sync().is_none(),
                "scoped enumeration cannot warm the fleet"
            );
            let rows = reader.all_sessions().unwrap();
            assert_eq!(rows.len(), 1, "unrelated Claude store was not discovered");
            assert_eq!(rows[0].harness, Harness::Codex);
        }
        assert_eq!(find("al", Some(Harness::Codex)).unwrap().unwrap().0.id, "alpha");
        write("beta-one");
        write("beta-two");
        assert!(
            find("beta", Some(Harness::Codex)).is_err(),
            "scoped prefix ambiguity is retained"
        );
        assert!(find("missing", Some(Harness::Codex)).unwrap().is_none());
        #[cfg(feature = "sqlite")]
        {
            let reader = catalog::Reader::open().unwrap();
            assert!(reader.last_full_sync().is_none());
            assert!(reader
                .all_sessions()
                .unwrap()
                .iter()
                .all(|r| r.harness == Harness::Codex));
        }
        std::fs::remove_dir_all(&codex).unwrap();
        assert!(find("alpha", Some(Harness::Codex)).unwrap().is_none());
        #[cfg(feature = "sqlite")]
        assert!(
            cataloged_sessions().is_empty(),
            "vanished qualified roots clear only their rows"
        );
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn agent_lookups_read_the_catalog_not_a_rediscovery() {
        let world = crate::test_world("agent-lookup");
        world.write_session(
            "parent",
            &[serde_json::json!({"type":"user","sessionId":"parent","message":{"role":"user","content":"spawn"}})],
        );
        world.write_agent(
            "parent",
            "a1b2c3",
            &serde_json::json!({}),
            &[serde_json::json!({"type":"user","sessionId":"parent","isSidechain":true,"message":{"role":"user","content":"task"}})],
        );
        cv_testkit::settle(&world.home);
        assert_eq!(sessions().len(), 1, "a warm catalog");
        let before = HARNESS_DISCOVERIES.with(std::cell::Cell::get);
        let before_dirs = SUBAGENT_READ_DIRS.with(std::cell::Cell::get);
        for _ in 0..3 {
            let parents = find_subagent_parents("agent-a1b2");
            assert_eq!(parents.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["parent"]);
        }
        assert!(find_subagent_parents("agent-ffff").is_empty());
        assert_eq!(
            HARNESS_DISCOVERIES.with(std::cell::Cell::get),
            before,
            "a lookup by agent id rediscovered the harness"
        );
        assert_eq!(
            SUBAGENT_READ_DIRS.with(std::cell::Cell::get),
            before_dirs,
            "warm sub-agent hit and miss must perform zero read_dir calls"
        );
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn subagent_stem_index_guards_warm_zero_readdirs_frozen_spawns_deletions_and_eviction() {
        let world = crate::test_world("subagent-stem-index");
        let msg =
            |sid: &str| serde_json::json!({"type":"user","sessionId":sid,"message":{"role":"user","content":"hello"}});
        let side_msg = |sid: &str| serde_json::json!({"type":"user","sessionId":sid,"isSidechain":true,"message":{"role":"user","content":"work"}});

        // 16 childless sessions (`<proj>/<sid>` absent -> guarded by `<proj>` FileId),
        // 1 session with `<sid>` dir only (`subagents/` absent -> guarded by `<sid>` FileId),
        // 1 direct sub-agent parent, and 1 workflow sub-agent parent.
        for i in 0..16 {
            let sid = format!("childless-{i:02}");
            world.write_session(&sid, &[msg(&sid)]);
        }
        world.write_session("sid-only", &[msg("sid-only")]);
        std::fs::create_dir_all(world.proj.join("sid-only/workflows")).unwrap();

        world.write_session("direct-parent", &[msg("direct-parent")]);
        let direct_agent_file = world.write_agent(
            "direct-parent",
            "ad1ec70001",
            &serde_json::json!({}),
            &[side_msg("direct-parent")],
        );
        // A `.flat.jsonl` payload sidecar beside the agent must never be indexed as a separate stem.
        std::fs::write(
            world.proj.join("direct-parent/subagents/agent-ad1ec70001.flat.jsonl"),
            "{}\n",
        )
        .unwrap();

        let wf_parent_file = world.write_session("wf-parent", &[msg("wf-parent")]);
        let wf_run_dir = world.proj.join("wf-parent/subagents/workflows/wf_run1");
        std::fs::create_dir_all(&wf_run_dir).unwrap();
        std::fs::write(
            wf_run_dir.join("agent-a0f100002.jsonl"),
            format!("{}\n", side_msg("wf-parent")),
        )
        .unwrap();

        cv_testkit::settle(&world.home);
        assert_eq!(discover_all().len(), 19);

        // Warm hits (direct + workflow, exact + prefix, `agent-` + bare) and warm misses must all
        // perform ZERO `fs::read_dir` calls and ZERO `HARNESS_DISCOVERIES`.
        HARNESS_DISCOVERIES.with(|n| n.set(0));
        SUBAGENT_READ_DIRS.with(|n| n.set(0));

        assert_eq!(
            find_subagent_parents("agent-ad1ec70001")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["direct-parent"]
        );
        assert_eq!(
            find_subagent_parents("ad1ec7")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["direct-parent"]
        );
        assert_eq!(
            find_subagent_parents("agent-a0f100002")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["wf-parent"]
        );
        assert_eq!(
            find_subagent_parents("a0f10")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["wf-parent"]
        );
        assert!(find_subagent_parents("agent-ad1ec70001.flat").is_empty());
        assert!(find_subagent_parents("agent-deadbeef").is_empty());

        assert_eq!(HARNESS_DISCOVERIES.with(std::cell::Cell::get), 0);
        assert_eq!(
            SUBAGENT_READ_DIRS.with(std::cell::Cell::get),
            0,
            "warm sub-agent index hits and misses must not call read_dir"
        );

        // Checkpoint catalog.db and snapshot cv_home before testing `--frozen` live updates.
        if let Some(conn) = catalog::open_db() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        let snapshot_cv_home = || {
            let mut out = std::collections::BTreeMap::new();
            for entry in std::fs::read_dir(&world.cv_home).unwrap() {
                let p = entry.unwrap().path();
                if p.is_file() {
                    out.insert(p.clone(), std::fs::read(&p).unwrap());
                }
            }
            out
        };
        let before_cv_home = snapshot_cv_home();

        // 1) Spawn a sub-agent inside `sid-only` (mutates `<proj>/sid-only`, NOT `<proj>`!),
        // 2) Spawn a workflow sub-agent under `childless-03`, and
        // 3) Create a brand-new parent session `after-warm` with its own sub-agent.
        world.write_agent("sid-only", "a11e0001", &serde_json::json!({}), &[side_msg("sid-only")]);
        let live_wf = world.proj.join("childless-03/subagents/workflows/wf_live");
        std::fs::create_dir_all(&live_wf).unwrap();
        std::fs::write(
            live_wf.join("agent-a11e0002.jsonl"),
            format!("{}\n", side_msg("childless-03")),
        )
        .unwrap();
        world.write_session("after-warm", &[msg("after-warm")]);
        world.write_agent(
            "after-warm",
            "a11e0003",
            &serde_json::json!({}),
            &[side_msg("after-warm")],
        );

        {
            let _frozen = env::Env::from_vars([
                ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
                ("HOME", world.home.as_os_str()),
            ])
            .with_frozen()
            .enter();

            assert_eq!(
                find_subagent_parents("agent-a11e0001")
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["sid-only"]
            );
            assert_eq!(
                find_subagent_parents("a11e0002")
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["childless-03"]
            );
            assert_eq!(
                find_subagent_parents("agent-a11e0003")
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["after-warm"]
            );
        }
        assert_eq!(
            snapshot_cv_home(),
            before_cv_home,
            "--frozen sub-agent lookup must not mutate catalog.db"
        );

        // Delete `direct_agent_file`: both `--frozen` and writable lookups must immediately drop it.
        std::fs::remove_file(&direct_agent_file).unwrap();
        {
            let _frozen = env::Env::from_vars([
                ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
                ("HOME", world.home.as_os_str()),
            ])
            .with_frozen()
            .enter();
            assert!(find_subagent_parents("agent-ad1ec70001").is_empty());
        }
        assert!(find_subagent_parents("agent-ad1ec70001").is_empty());

        // Settle mtimes, refresh once so unsettled `< 2s` guards quiesce, then confirm 0 `read_dir`s again.
        cv_testkit::settle(&world.home);
        assert_eq!(find_subagent_parents("agent-a11e0003").len(), 1);
        SUBAGENT_READ_DIRS.with(|n| n.set(0));
        assert_eq!(find_subagent_parents("agent-a11e0001")[0].id, "sid-only");
        assert_eq!(find_subagent_parents("agent-a11e0002")[0].id, "childless-03");
        assert_eq!(find_subagent_parents("agent-a11e0003")[0].id, "after-warm");
        assert!(find_subagent_parents("agent-ad1ec70001").is_empty());
        assert_eq!(SUBAGENT_READ_DIRS.with(std::cell::Cell::get), 0);

        // Delete `wf-parent` session transcript + sidecar dir: `sessions()` evicts its rows from
        // `subagent_parents` and `subagents`, and `invalidate_harness_in` clears the harness cleanly.
        std::fs::remove_file(&wf_parent_file).unwrap();
        std::fs::remove_dir_all(world.proj.join("wf-parent")).unwrap();
        cv_testkit::settle(&world.home);
        let _ = sessions();
        assert!(find_subagent_parents("agent-a0f100002").is_empty());
        let conn = catalog::open_db().unwrap();
        let leftover_parent: i64 = conn
            .query_row(
                "SELECT count(*) FROM subagent_parents WHERE parent_id='wf-parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let leftover_stem: i64 = conn
            .query_row("SELECT count(*) FROM subagents WHERE parent_id='wf-parent'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!((leftover_parent, leftover_stem), (0, 0));
        drop(conn);

        catalog::with_scoped_versions(&[(Harness::Claude, 9999)], None, || {
            let conn = catalog::open_db().unwrap();
            let p_cnt: i64 = conn
                .query_row(
                    "SELECT count(*) FROM subagent_parents WHERE harness='claude'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let s_cnt: i64 = conn
                .query_row("SELECT count(*) FROM subagents WHERE harness='claude'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!((p_cnt, s_cnt), (0, 0), "scoped codec bump must clear subagent tables");
        });
    }

    #[test]
    fn subagent_lookup_falls_back_cleanly_without_catalog_or_sqlite() {
        let world = crate::test_world("subagent-fallback");
        world.write_session(
            "fallback-parent",
            &[serde_json::json!({"type":"user","sessionId":"fallback-parent","message":{"role":"user","content":"spawn"}})],
        );
        world.write_agent(
            "fallback-parent",
            "afb00123",
            &serde_json::json!({}),
            &[serde_json::json!({"type":"user","sessionId":"fallback-parent","isSidechain":true,"message":{"role":"user","content":"task"}})],
        );
        // Point CLUSTERVISION_CACHE at a regular file so `catalog.db` cannot open even with `sqlite`
        // enabled, exercising the parallel `workflow_sessions()` fallback under both feature sets.
        let blocker = world.base.join("not-a-dir");
        std::fs::write(&blocker, "file").unwrap();
        let _unopenable = env::Env::from_vars([
            ("CLUSTERVISION_CACHE", blocker.as_os_str()),
            ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
            ("HOME", world.home.as_os_str()),
        ])
        .enter();

        assert_eq!(
            find_subagent_parents("afb001")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["fallback-parent"]
        );
        assert_eq!(
            find_cheap("agent-afb001", None).unwrap().unwrap().0.id,
            "agent-afb00123"
        );
        assert!(find_subagent_parents("agent-missing").is_empty());
    }

    #[test]
    fn scoped_sessions_refresh_only_requested_rows_without_certifying_the_fleet() {
        let world = crate::test_world("scoped-session-enumeration");
        let codex = world.home.join(".codex/sessions");
        std::fs::create_dir_all(&codex).unwrap();
        let path = codex.join("rollout-scoped.jsonl");
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"scoped\"}}\n").unwrap();
        world.write_session(
            "unrelated",
            &[serde_json::json!({"type":"user","message":{"role":"user","content":"other store"}})],
        );
        let refs = sessions_for(Some(Harness::Codex));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, path);
        #[cfg(feature = "sqlite")]
        {
            let reader = catalog::Reader::open().unwrap();
            assert!(reader.last_full_sync().is_none());
            assert!(reader
                .all_sessions()
                .unwrap()
                .iter()
                .all(|r| r.harness == Harness::Codex));
        }
        std::fs::remove_file(path).unwrap();
        assert!(sessions_for(Some(Harness::Codex)).is_empty());
        #[cfg(feature = "sqlite")]
        assert!(cataloged_sessions().is_empty());
        assert!(sessions_for(None).iter().any(|r| r.id == "unrelated"));
    }

    #[test]
    fn exact_copy_authority_is_newest_then_deterministic_and_harness_qualified() {
        let world = crate::test_world("copy-authority");
        let make = |name: &str, harness, updated| {
            let path = world.home.join(name);
            std::fs::write(&path, "{}").unwrap();
            SessionRef {
                updated_at: chrono::DateTime::from_timestamp(updated, 0),
                ..SessionRef::new("same", harness, path)
            }
        };
        let old = make("a", Harness::Claude, 1);
        let new = make("z", Harness::Claude, 2);
        for refs in [vec![old.clone(), new.clone()], vec![new.clone(), old.clone()]] {
            assert_eq!(pick(refs, "same").unwrap().unwrap().path, new.path);
        }
        let tie = make("b", Harness::Claude, 2);
        for refs in [vec![tie.clone(), new.clone()], vec![new.clone(), tie.clone()]] {
            assert_eq!(pick(refs, "same").unwrap().unwrap().path, tie.path);
        }
        let foreign = make("foreign", Harness::Codex, 3);
        let resolved = resolve_id(vec![old.clone(), foreign], "same", Some(Harness::Claude))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.0.path, old.path);
        std::fs::remove_file(&new.path).unwrap();
        assert_eq!(
            resolve_id(vec![new, old.clone()], "same", None)
                .unwrap()
                .unwrap()
                .0
                .path,
            old.path
        );
        let prefix = SessionRef::new("same-longer", Harness::Claude, tie.path);
        assert_eq!(pick(vec![prefix, old.clone()], "same").unwrap().unwrap().path, old.path);
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn frozen_sessions_and_lookup_retain_scoped_refresh_in_memory_without_mutating_catalog() {
        use std::io::Write as _;
        let w = crate::test_world("frozen-stale-refresh");
        let existing = w.write_session(
            "existing-sess",
            &[serde_json::json!({
                "type": "user",
                "uuid": "u1",
                "sessionId": "existing-sess",
                "message": {"role": "user", "content": "initial prompt"}
            })],
        );
        cv_testkit::settle(&w.home);
        let initial = discover_all();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].id, "existing-sess");
        assert_eq!(initial[0].message_count, Some(1));

        // Checkpoint catalog.db so no WAL sidecars exist and snapshot every file in cv_home.
        if let Some(conn) = catalog::open_db() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        let snapshot_cv_home = || {
            let mut out = std::collections::BTreeMap::new();
            for entry in std::fs::read_dir(&w.cv_home).unwrap() {
                let p = entry.unwrap().path();
                if p.is_file() {
                    out.insert(p.clone(), (source::FileId::at(&p), std::fs::read(&p).unwrap()));
                }
            }
            out
        };
        let before_cv_home = snapshot_cv_home();

        // Append a turn + title to `existing-sess` and add a brand-new session in Claude AND Codex
        // (a harness that previously had 0 rows in catalog.db) so splicing tests both replacement
        // and registration-order insertion.
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&existing).unwrap(),
            "{}",
            serde_json::json!({
                "type": "user",
                "uuid": "u2",
                "sessionId": "existing-sess",
                "message": {"role": "user", "content": "second prompt"}
            })
        )
        .unwrap();
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&existing).unwrap(),
            "{}",
            serde_json::json!({"type": "ai-title", "aiTitle": "Updated Title"})
        )
        .unwrap();
        w.write_session(
            "added-claude",
            &[serde_json::json!({
                "type": "user",
                "uuid": "n1",
                "sessionId": "added-claude",
                "message": {"role": "user", "content": "fresh under frozen"}
            })],
        );
        let codex_dir = w.home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("rollout-added-codex.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"added-codex\"}}\n",
        )
        .unwrap();

        {
            let _frozen = env::Env::from_vars([
                ("CLUSTERVISION_HOME", w.cv_home.as_os_str()),
                ("HOME", w.home.as_os_str()),
            ])
            .with_frozen()
            .enter();

            let (listed, was_full) = sessions_impl();
            assert!(
                !was_full,
                "warm catalog under --frozen must use scoped refresh, not full scan"
            );
            let ids: Vec<(&str, Harness)> = listed.iter().map(|r| (r.id.as_str(), r.harness)).collect();
            assert!(ids.contains(&("existing-sess", Harness::Claude)), "{ids:?}");
            assert!(ids.contains(&("added-claude", Harness::Claude)), "{ids:?}");
            assert!(ids.contains(&("added-codex", Harness::Codex)), "{ids:?}");
            // Registration order: all Claude rows precede Codex rows.
            let first_codex = listed.iter().position(|r| r.harness == Harness::Codex).unwrap();
            assert!(listed[..first_codex].iter().all(|r| r.harness == Harness::Claude));

            let updated = listed.iter().find(|r| r.id == "existing-sess").unwrap();
            assert_eq!(updated.message_count, Some(2));
            assert_eq!(updated.title.as_deref(), Some("Updated Title"));

            let found_new = find_cheap("added-claude", None)
                .unwrap()
                .expect("new session resolves via find_cheap");
            assert_eq!(found_new.0.id, "added-claude");
            let found_codex = find_cheap("added-codex", None)
                .unwrap()
                .expect("new codex session resolves");
            assert_eq!(found_codex.0.id, "added-codex");
        }

        // catalog.db on disk was NOT mutated at all (and raw `cataloged_sessions` still has only the old row).
        assert_eq!(
            snapshot_cv_home(),
            before_cv_home,
            "--frozen must not mutate catalog.db or create sidecars"
        );
        let persisted_only = cataloged_sessions();
        assert_eq!(persisted_only.len(), 1);
        assert_eq!(persisted_only[0].id, "existing-sess");
        assert_eq!(persisted_only[0].message_count, Some(1));
    }
}

/// Harness writer observations and corpus verification versions.
pub mod writer_version;

/// Bounded, operation-owned staging for atomic publication across consumers.
pub mod staging {
    pub use crate::spill::ordered::{Ordered, Record, Rows};
}
