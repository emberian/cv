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
pub mod catalog;
pub mod compaction;
pub mod config;
/// Bounded operation-owned staging and immutable byte replay, shared by output consumers.
pub mod bytes {
    pub use crate::harness::store::bytes::{ByteSource, ByteStage};
}

pub mod codex_handoff;
pub mod dataset;
pub mod digest;
pub mod discover_cache;
pub mod distill;
pub mod doctor;
pub mod emit;
pub mod env;

pub mod events;
pub mod formats;
pub mod harness;
pub mod hooks;
pub mod html;
pub mod ingest;
pub mod io_admission;
pub mod ir;
mod ir_json;
pub use ir_json::PrettyJson;
mod derive;
pub mod json;
pub mod lanes;
pub mod lazy;
pub(crate) mod lockfile;
pub mod offsets;
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
    discover_all_with_catalog(catalog::Reader::open())
}

/// [`discover_all`], handing every rollout it decodes in full to `handoff`'s consumer (an index
/// sweep running beside it), so that consumer reads each one without decoding it again.
pub fn discover_all_handing_off(handoff: codex_handoff::Handoff) -> Vec<SessionRef> {
    codex_handoff::within(&Some(handoff), discover_all)
}

fn discover_all_with_catalog(mut catalog: Option<catalog::Reader>) -> Vec<SessionRef> {
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
    results
        .into_iter()
        .filter_map(|(_, _, found, _)| found)
        .flat_map(|found| found.refs)
        .collect()
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
    sessions_impl().0
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
            if !stale.is_empty() {
                let reader = catalog.as_mut().expect("staleness was read through this owner");
                for h in stale {
                    refresh_harness(h, reader);
                }
                reader.persist_discovery(false);
            }
            if let Some(rows) = catalog.as_ref().and_then(catalog::Reader::all_sessions) {
                return (rows, false);
            }
        }
    }
    if catalog.as_ref().is_some_and(catalog::Reader::take_rediscovery_notice) {
        eprintln!(
            "cv: note: re-discovering the fleet once for this cv version (minutes on a large fleet; `cv index` after upgrading does it ahead of time)"
        );
    }
    (discover_all_with_catalog(catalog), true)
}

/// Re-discover one harness and replace its catalog rows + watches — the probe's scoped escalation.
/// On a discover error the previous rows are kept (matching [`discover_all`]'s tolerance).
fn refresh_harness(h: Harness, catalog: &mut catalog::Reader) {
    let _ = discover_harness(h, Some(catalog));
}

#[cfg(test)]
thread_local! {
    /// Scoped discoveries this thread ran: the agent-lookup test's counter.
    static HARNESS_DISCOVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Enumerate one complete harness scope without certifying freshness of the rest of the fleet.
/// Returns the freshly discovered refs even when the catalog cannot be opened or published.
fn discover_harness(h: Harness, mut catalog: Option<&mut catalog::Reader>) -> Option<Vec<SessionRef>> {
    #[cfg(test)]
    HARNESS_DISCOVERIES.with(|n| n.set(n.get() + 1));
    let a = harness::for_harness(h)?;
    let _sources = source::read_scope();
    let _cache_scope = catalog.as_ref().map(|reader| reader.discovery_scope());
    let before = catalog
        .as_ref()
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
    if let Some(reader) = catalog.as_mut() {
        reader.refill_turns(refs.iter_mut());
        reader.replace_harness(h, &refs, &catalog::watches_for(a.as_ref(), &roots, &refs), &before);
        reader.record_skipped(&[(h, &skipped)]);
    }
    Some(refs)
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
fn workflow_store(h: Harness) -> Option<&'static dyn harness::Workflows> {
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
/// spawned it. A filename scan of each cataloged run-store session's agent transcripts
/// ([`harness::Workflows::has_agent`]), in parallel; nothing is parsed or rediscovered.
pub fn find_subagent_parents(agent_id: &str) -> Vec<SessionRef> {
    let want = format!("agent-{}", agent_id.strip_prefix("agent-").unwrap_or(agent_id));
    let refs = workflow_sessions();
    let mut out = par_filter_map(refs, move |(r, store)| store.has_agent(&r, &want).then_some(r));
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    out
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
        // A qualified miss has exhausted its harness. An `agent-…` shaped id gets one last
        // reading as a sub-agent transcript.
        (None, true) => find_subagent_session(id, harness),
        (None, false) => match resolve_id(discover_all(), id, harness)? {
            Some(hit) => Ok(Some(hit)),
            None => find_subagent_session(id, harness),
        },
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
    let want = format!("agent-{}", id.strip_prefix("agent-").unwrap_or(id));
    let ids: std::collections::BTreeSet<String> = parents
        .iter()
        .map(|parent| {
            subagent_tree_filtered_of(parent, SubagentDemand::Sessions, None, |candidate, _| {
                candidate.starts_with(&want)
            })
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .map(|s| s.session.id)
        .filter(|s| s.starts_with(&want))
        .collect();
    Ok(ids.len() == 1)
}

/// A resolved session: its catalog ref plus the harness adapter that reads it.
type FoundSession = (SessionRef, Box<dyn Adapter>);

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
        return Ok((resolve_id(refs, id, Some(h))?, true));
    }

    // Slow path (cold/stale catalog): freshen via the probe — usually a few hundred stats plus at
    // most a scoped re-discovery, and it re-warms the catalog.
    let (refs, was_full) = sessions_impl();
    Ok((resolve_id(refs, id, harness)?, was_full))
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
        .filter(|r| harness.is_none_or(|h| r.harness == h) && r.id.starts_with(id))
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
}

/// Harness writer observations and corpus verification versions.
pub mod writer_version;
