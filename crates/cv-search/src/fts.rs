//! Full-text search via tantivy.
//!
//! Schema fields:
//! - `id`         STRING | STORED  — exact session id (the delete key on reindex)
//! - `harness`    STRING | STORED  — fielded filter, e.g. `harness:claude`
//! - `cwd`        TEXT   | STORED  — tokenized so path fragments are searchable
//! - `title`      TEXT   | STORED
//! - `body`       TEXT             — the big searchable blob; **indexed but not stored**
//! - `path`       STRING | STORED  — source file, so a hit can be snippeted by re-reading just
//!   that one session live (no full stored body — keeps the index lean)
//! - `preview`    STORED (only)    — ~240-char whitespace-collapsed head of the body, written on
//!   the session's **first** chunk doc only (~240 bytes/session): the snippet fallback when the
//!   source file has moved/changed
//! - `created_at`/`updated_at` i64 INDEXED | STORED — returned on hits + future sort/filter
//! - `mtime`      i64 STORED       — file mtime, still used for non-append transcript stores
//! - `size`       i64 STORED       — file size: the primary incremental-index key for append-only
//!   session logs, which can skip on unchanged size even when mtime was spuriously bumped (touch,
//!   rsync, restore); non-append stores require both size and mtime to match.
//!
//! Indexing is **incremental** by default: only sessions whose file signature changed (plus new ones)
//! are (re)written, and sessions whose files vanished are deleted. A full rebuild is `rebuild=true`.
//! Snippets are generated **live** from the top hits (re-reading the session, capped) rather than
//! from a stored copy of every body — which is what keeps the on-disk index small.

use crate::Hit;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

struct Fields {
    id: Field,
    harness: Field,
    cwd: Field,
    title: Field,
    body: Field,
    path: Field,
    preview: Field,
    created_at: Field,
    updated_at: Field,
    mtime: Field,
    size: Field,
    /// Sub-agent provenance, written only on folded-in sub-agent docs (`cv index --subagents`):
    /// `parent_id` is fielded (`parent_id:<top-level id>` filters to one parent's whole forest),
    /// `agent_id`/`workflow` are stored-only attribution carried onto the hit. NULL/absent on
    /// top-level docs, which keeps those docs byte-for-byte as before.
    parent_id: Field,
    agent_id: Field,
    workflow: Field,
}

fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("id", STRING | STORED);
    b.add_text_field("harness", STRING | STORED);
    b.add_text_field("cwd", TEXT | STORED);
    b.add_text_field("title", TEXT | STORED);
    b.add_text_field("body", TEXT);
    b.add_text_field("path", STRING | STORED);
    b.add_text_field("preview", STORED); // stored-only: never searched, just the snippet fallback
    b.add_i64_field("created_at", INDEXED | STORED);
    b.add_i64_field("updated_at", INDEXED | STORED);
    b.add_i64_field("mtime", STORED);
    b.add_i64_field("size", STORED); // primary incremental key for append-only logs
    b.add_text_field("parent_id", STRING | STORED); // fielded: `parent_id:<id>` scopes to a forest
    b.add_text_field("agent_id", STRING | STORED); // stored attribution
    b.add_text_field("workflow", STRING | STORED); // stored attribution
    b.build()
}

fn fields_of(schema: &Schema) -> Result<Fields> {
    let get = |name: &str| schema.get_field(name);
    Ok(Fields {
        id: get("id")?,
        harness: get("harness")?,
        cwd: get("cwd")?,
        title: get("title")?,
        body: get("body")?,
        path: get("path")?,
        preview: get("preview")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
        mtime: get("mtime")?,
        size: get("size")?,
        parent_id: get("parent_id")?,
        agent_id: get("agent_id")?,
        workflow: get("workflow")?,
    })
}

/// Whether an on-disk index carries every field of the current schema. A miss means it was built
/// by an older `cv` (pre-`path`/`mtime`/`preview`, pre-subagent-provenance, or pre-`size`) and
/// must be rebuilt fresh — one-time on upgrade, from the indexing path only.
fn schema_current(schema: &Schema) -> bool {
    ["path", "mtime", "preview", "parent_id", "agent_id", "workflow", "size"]
        .iter()
        .all(|f| schema.get_field(f).is_ok())
}

/// Open the index at `dir` for **indexing**: created when absent, and an existing index with a
/// stale schema (see [`schema_current`]) is transparently rebuilt fresh so the binary self-heals
/// on upgrade — the caller's sweep repopulates it. Any *other* open failure (corruption, a
/// transient IO/lock error) propagates: only a positively-identified schema mismatch may delete;
/// read-only callers use [`open_existing`], which never deletes anything.
fn open_or_create(dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating tantivy dir {}", dir.display()))?;
    // No `meta.json` → nothing here is an index (a fresh dir, or stray leftovers from a failed
    // create): safe to start clean.
    if !dir.join("meta.json").exists() {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
        let index = Index::create_in_dir(dir, build_schema())
            .with_context(|| format!("creating tantivy index in {}", dir.display()))?;
        let fields = fields_of(&index.schema())?;
        return Ok((index, fields));
    }
    let index = Index::open_in_dir(dir).with_context(|| {
        format!(
            "opening tantivy index at {} (if it is corrupt, delete the directory and re-run `cv index`)",
            dir.display()
        )
    })?;
    if schema_current(&index.schema()) {
        let fields = fields_of(&index.schema())?;
        return Ok((index, fields));
    }
    // Positively identified as an older-schema index → rebuild fresh.
    std::fs::remove_dir_all(dir).with_context(|| format!("clearing stale-schema index at {}", dir.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
    let index = Index::create_in_dir(dir, build_schema())
        .with_context(|| format!("creating tantivy index in {}", dir.display()))?;
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

/// Open the index at `dir` for **querying**: never creates and never deletes — a transient open
/// failure during a search must not wipe an index that took an hour to build. A stale-schema index
/// is an error pointing at `cv index` (whose path is the one allowed to rebuild).
fn open_existing(dir: &Path) -> Result<(Index, Fields)> {
    let index = Index::open_in_dir(dir).with_context(|| format!("opening tantivy index at {}", dir.display()))?;
    if !schema_current(&index.schema()) {
        anyhow::bail!(
            "full-text index at {} was built by an older cv — run `cv index` to rebuild it",
            dir.display()
        );
    }
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

const COMMIT_EVERY: usize = 256;

/// Max searchable bytes per tantivy document. A session's body is flushed into **multiple** docs of
/// at most this size (all sharing the session id), so the index never holds a whole large session's
/// body at once — search dedups hits back to one row per id. 4 MB keeps per-doc memory bounded while
/// staying well above almost every individual message.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Discover + parse every session and bring the index at `dir` up to date.
///
/// Incremental by default: a session whose freshness signature already matches the index is
/// **skipped** (no re-parse cost beyond the metadata scan, no rewrite); changed sessions are
/// replaced; new ones added; and sessions whose source files no longer exist are deleted. Pass
/// `rebuild = true` to clear and rebuild from scratch. Returns the number of sessions discovered
/// (not just the changed ones).
///
/// **Chunked ingestion:** each session streams message-by-message into a bounded body buffer that
/// flushes a tantivy document every [`CHUNK_BYTES`], so a large session is indexed as several small
/// docs (same id) and the indexer never holds its whole body. Commits periodically so segments flush
/// rather than accumulating in the writer's heap.
///
/// **Event ride-along:** the same single adapter pass also feeds the event catalog
/// ([`cv_core::events`] — file edits/reads, commands, errors → `cv events` / `cv touched`) via a
/// [`cv_core::TeeSink`]. Events keep their own `(mtime, size)` skip-table (`event_sync`), so an FTS-only
/// `--rebuild` doesn't force an event re-ingest and a tantivy-fresh session whose events are
/// missing gets an events-only catch-up pass; when both are stale the session is read exactly once.
/// Event persistence is best-effort (sqlite errors never fail indexing).
///
/// **Sub-agent forest (`subagents`):** after each top-level claude session, also walk its
/// `cv_core::subagent_tree_of` forest and index/ingest every Task/Workflow agent transcript, tagged
/// with provenance (parent id / agent id / workflow run). Off by default — the forest can be
/// hundreds of transcripts (~900 MB) — and folded-in sub-agent docs carry the provenance fields so
/// a hit knows which agent of which workflow of which parent it came from.
pub fn index_all(dir: &Path, rebuild: bool, subagents: bool) -> Result<usize> {
    use cv_core::ParseOptions;

    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(50_000_000).context("creating tantivy index writer")?;

    if rebuild {
        writer.delete_all_documents().context("clearing index")?;
        writer.commit().context("commit after clear")?;
    }

    // id → (mtime, size) for every live doc — the incremental skip-set — plus the set of ids that
    // are folded-in sub-agent docs (they carry `parent_id`), which the reap below must leave alone
    // on a plain (no --subagents) refresh. Append-only transcript logs use size as the primary
    // freshness signal; non-append stores still use mtime to catch same-size rewrites.
    let (existing, folded): (HashMap<String, (i64, i64)>, HashSet<String>) = if rebuild {
        Default::default()
    } else {
        read_indexed_sigs(&index, &f).unwrap_or_default()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut since_commit = 0usize;
    let mut changed = 0usize;
    let mut total = 0usize;

    // One query for the whole event/offsets skip-tables — the per-session checks would open (and
    // run DDL on) a fresh sqlite connection per call, which a ~2,400-session sweep pays ~2,400×.
    let event_sync = cv_core::events::SyncTable::load();
    let offset_sync = cv_core::offsets::SyncTable::load();

    for r in cv_core::discover_all() {
        total += 1;
        seen.insert(r.id.clone());
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        let fts_fresh = fts_is_fresh(&r, existing.get(&r.id), mtime, size);
        let events_stale = event_sync.needs_ingest(&r, mtime, size);
        // Message byte offsets (seekable `cv show --range` — see [`cv_core::offsets`]) ride the
        // same pass, for the harnesses whose adapters can stamp them.
        let offsets_stale = cv_core::offsets::supported(r.harness) && offset_sync.needs_record(&r, mtime, size);
        // Unchanged on all axes → skip entirely (no re-parse beyond the metadata scan).
        if fts_fresh && !events_stale && !offsets_stale {
            continue;
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let mut events = events_stale.then(|| cv_core::events::EventSink::new(r.cwd.clone()));
        let mut offsets = offsets_stale.then(cv_core::offsets::OffsetSink::new);
        // The recording pass needs per-message offset stamps; otherwise plain lazy. Identical for
        // every other consumer on the tee (the stamp is one small `extra` entry nobody reads).
        let opts = if offsets.is_some() {
            ParseOptions::lazy_offsets()
        } else {
            ParseOptions::lazy()
        };

        // FTS docs current but events/offsets missing/stale (e.g. first run after upgrading, or
        // after a catalog wipe): a catalog-only pass that never touches tantivy.
        if fts_fresh {
            let res = match (events.as_mut(), offsets.as_mut()) {
                (Some(es), Some(os)) => {
                    let mut tee = cv_core::TeeSink::new(es, os);
                    adapter.stream(&r, &opts, &mut tee)
                }
                (Some(es), None) => adapter.stream(&r, &opts, es),
                (None, Some(os)) => adapter.stream(&r, &opts, os),
                (None, None) => unreachable!("skip above covers fresh+fresh"),
            };
            match res {
                Ok(_) => {
                    if let Some(es) = &events {
                        cv_core::events::record(&r, es.events(), mtime, size);
                    }
                    if let Some(os) = &offsets {
                        cv_core::offsets::record(&r, os, mtime, size);
                    }
                }
                Err(e) => eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness),
            }
            continue;
        }

        if existing.contains_key(&r.id) {
            // Changed: drop *all* of this session's docs (delete by the shared id term) before re-add.
            writer.delete_term(Term::from_field_text(f.id, &r.id));
        }
        let docs = match index_session_clean(
            &mut writer,
            &f,
            adapter.as_ref(),
            &r,
            mtime,
            size,
            events.as_mut(),
            offsets.as_mut(),
            cv_core::events::Provenance::default(),
        ) {
            Ok(docs) => docs,
            Err(e) => {
                // `index_session_clean` already deleted any partial chunk docs, so the session is
                // simply absent from the index (and its skip-set entry) until a later run parses
                // it whole — never frozen in as a truncated body stamped fresh.
                eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness);
                continue;
            }
        };
        // Only stamp events/offsets after a *complete* pass — a parse error above leaves the sync
        // rows untouched so the session is retried next run.
        if let Some(es) = events {
            cv_core::events::record(&r, es.events(), mtime, size);
        }
        if let Some(os) = offsets {
            cv_core::offsets::record(&r, &os, mtime, size);
        }
        changed += 1;
        since_commit += docs;
        if since_commit >= COMMIT_EVERY {
            writer.commit().context("interim commit")?;
            since_commit = 0;
        }
    }

    // Sub-agent forest fold (opt-in): after the top-level sweep, walk every claude session's
    // `subagent_tree_of` and index/ingest each agent transcript tagged with provenance. Done as a
    // second pass (a cheap metadata re-discovery) so the top-level loop's skip logic stays intact;
    // its `seen` set is extended here so a previously-folded agent still on disk isn't reaped.
    if subagents {
        for r in cv_core::discover_all() {
            for sub in cv_core::subagent_tree_of(&r) {
                seen.insert(sub.session.id.clone());
                match index_one_subagent(&mut writer, &f, &r, &sub, &existing, &event_sync, &offset_sync) {
                    Ok(0) => {}
                    Ok(docs) => {
                        changed += 1;
                        since_commit += docs;
                        if since_commit >= COMMIT_EVERY {
                            writer.commit().context("interim commit")?;
                            since_commit = 0;
                        }
                    }
                    Err(e) => eprintln!(
                        "cv-search: parse failed for {} ({}): {e:#}",
                        sub.session.id, sub.session.harness
                    ),
                }
            }
        }
    }

    // Reap sessions that disappeared from disk since the last index (incremental only). A plain
    // refresh never reaps the folded sub-agent forest — see [`reap_missing`].
    let removed = if rebuild {
        0
    } else {
        reap_missing(&mut writer, &f, &existing, &seen, &folded, subagents)
    };

    writer.commit().context("committing index")?;
    if !rebuild {
        eprintln!(
            "  ↳ index: {changed} changed/new, {removed} removed, {} unchanged",
            total.saturating_sub(changed)
        );
    }
    Ok(total)
}

/// Stream one session into bounded tantivy docs through [`ChunkSink`] — teeing the same pass into
/// `events` and/or `offsets` when given, so the transcript is read once for all consumers.
/// `lazy()` keeps large content as spans: the chunk sink chunk-resolves them and the other sinks
/// never read them at all; with an offsets sink the pass runs under `lazy_offsets()` so the
/// adapter stamps each message's byte offset for it to harvest. Returns the docs written.
#[allow(clippy::too_many_arguments)]
fn index_session(
    writer: &mut IndexWriter,
    f: &Fields,
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    mtime: i64,
    size: i64,
    events: Option<&mut cv_core::events::EventSink>,
    offsets: Option<&mut cv_core::offsets::OffsetSink>,
    prov: cv_core::events::Provenance,
) -> Result<usize> {
    use cv_core::ParseOptions;
    let opts = if offsets.is_some() {
        ParseOptions::lazy_offsets()
    } else {
        ParseOptions::lazy()
    };
    let mut sink = ChunkSink::new(writer, f, r, mtime, size, prov);
    match (events, offsets) {
        (Some(es), Some(os)) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, es);
            let mut tee = cv_core::TeeSink::new(&mut tee, os);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (Some(es), None) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, es);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (None, Some(os)) => {
            let mut tee = cv_core::TeeSink::new(&mut sink, os);
            adapter.stream(r, &opts, &mut tee)?;
        }
        (None, None) => {
            adapter.stream(r, &opts, &mut sink)?;
        }
    }
    sink.finish()?;
    Ok(sink.docs)
}

/// [`index_session`], but guaranteeing the writer holds **no** docs for the session on error.
///
/// A large session flushes chunk docs into the writer *while* streaming, so a mid-stream parse
/// error would otherwise leave a truncated head behind — and since every chunk doc is stamped with
/// the full current `(mtime, size)`, [`fts_is_fresh`] would then skip the session forever, freezing
/// the truncation in. Deleting by the shared id term on the error path removes those partial docs
/// (tantivy applies operations in opstamp order, so this cannot touch docs added by a later pass);
/// the id drops out of the skip-set entirely and is retried on the next run.
#[allow(clippy::too_many_arguments)]
fn index_session_clean(
    writer: &mut IndexWriter,
    f: &Fields,
    adapter: &dyn cv_core::Adapter,
    r: &cv_core::SessionRef,
    mtime: i64,
    size: i64,
    events: Option<&mut cv_core::events::EventSink>,
    offsets: Option<&mut cv_core::offsets::OffsetSink>,
    prov: cv_core::events::Provenance,
) -> Result<usize> {
    match index_session(writer, f, adapter, r, mtime, size, events, offsets, prov) {
        Ok(docs) => Ok(docs),
        Err(e) => {
            writer.delete_term(Term::from_field_text(f.id, &r.id));
            Err(e)
        }
    }
}

/// Delete every previously-indexed id that this sweep did not see on disk — with one carve-out:
/// when `subagents` is false, ids belonging to the **folded sub-agent forest** (`folded`, the docs
/// tagged with a `parent_id`) are kept. A plain refresh never walks the forest, so an unseen agent
/// transcript is merely *unvisited*, not vanished — reaping it would silently unfold a forest the
/// user deliberately folded in with `cv index --subagents`. Only a `--subagents` sweep (which does
/// walk the forest and extends `seen` with every live agent) may decide an agent is gone. Returns
/// the number of sessions reaped.
fn reap_missing(
    writer: &mut IndexWriter,
    f: &Fields,
    existing: &HashMap<String, (i64, i64)>,
    seen: &HashSet<String>,
    folded: &HashSet<String>,
    subagents: bool,
) -> usize {
    let mut removed = 0usize;
    for id in existing.keys() {
        if seen.contains(id) || (!subagents && folded.contains(id)) {
            continue;
        }
        writer.delete_term(Term::from_field_text(f.id, id));
        removed += 1;
    }
    removed
}

/// Index/ingest one sub-agent transcript `sub` (a child of top-level `parent`) into the forest fold,
/// tagged with provenance (parent id / agent id / workflow). Mirrors the top-level loop's per-axis
/// freshness skip: returns `Ok(0)` when this agent is unchanged on all axes (or has no adapter), so
/// the caller doesn't count it as changed. Events ride into the catalog with the same provenance.
///
/// Memory discipline is identical to the top-level path: lazy parse → large tool payloads stay on
/// disk; only chunked docs and the small event rows touch memory.
#[allow(clippy::too_many_arguments)]
fn index_one_subagent(
    writer: &mut IndexWriter,
    f: &Fields,
    parent: &cv_core::SessionRef,
    sub: &cv_core::SubagentInfo,
    existing: &HashMap<String, (i64, i64)>,
    event_sync: &cv_core::events::SyncTable,
    offset_sync: &cv_core::offsets::SyncTable,
) -> Result<usize> {
    let sr = &sub.session;
    let (mtime, size) = cv_core::offsets::file_sig(&sr.path);
    let fts_fresh = fts_is_fresh(sr, existing.get(&sr.id), mtime, size);
    let events_stale = event_sync.needs_ingest(sr, mtime, size);
    let offsets_stale = cv_core::offsets::supported(sr.harness) && offset_sync.needs_record(sr, mtime, size);
    if fts_fresh && !events_stale && !offsets_stale {
        return Ok(0);
    }
    let Some(adapter) = cv_core::harness::for_harness(sr.harness) else {
        return Ok(0);
    };
    let prov = cv_core::events::Provenance {
        parent_id: Some(parent.id.clone()),
        agent_id: Some(sub.agent_id().to_string()),
        workflow: sub.workflow.clone(),
    };
    let mut events = events_stale.then(|| cv_core::events::EventSink::new(sr.cwd.clone()));
    let mut offsets = offsets_stale.then(cv_core::offsets::OffsetSink::new);
    // FTS-fresh but catalog/offsets stale: a catalog-only pass that never touches tantivy (keeps the
    // doc set byte-stable), mirroring the top-level loop's fast path. This runs BEFORE the
    // delete-by-id below — mirroring the top-level loop's order — so a fresh agent's docs are never
    // deleted with nothing re-added in their place.
    if fts_fresh {
        let opts = if offsets.is_some() {
            cv_core::ParseOptions::lazy_offsets()
        } else {
            cv_core::ParseOptions::lazy()
        };
        match (events.as_mut(), offsets.as_mut()) {
            (Some(es), Some(os)) => {
                let mut tee = cv_core::TeeSink::new(es, os);
                adapter.stream(sr, &opts, &mut tee)?;
            }
            (Some(es), None) => {
                adapter.stream(sr, &opts, es)?;
            }
            (None, Some(os)) => {
                adapter.stream(sr, &opts, os)?;
            }
            (None, None) => return Ok(0),
        }
        if let Some(es) = &events {
            cv_core::events::record_with(sr, es.events(), mtime, size, &prov);
        }
        if let Some(os) = &offsets {
            cv_core::offsets::record(sr, os, mtime, size);
        }
        return Ok(0);
    }
    if existing.contains_key(&sr.id) {
        // Changed agent: drop its prior docs before re-adding (same delete-by-id as top-level).
        writer.delete_term(Term::from_field_text(f.id, &sr.id));
    }
    let docs = index_session_clean(
        writer,
        f,
        adapter.as_ref(),
        sr,
        mtime,
        size,
        events.as_mut(),
        offsets.as_mut(),
        prov.clone(),
    )?;
    if let Some(es) = events {
        cv_core::events::record_with(sr, es.events(), mtime, size, &prov);
    }
    if let Some(os) = offsets {
        cv_core::offsets::record(sr, &os, mtime, size);
    }
    Ok(docs)
}

/// Streams one session's messages into bounded-size tantivy docs (all sharing the session id), so a
/// large session never materializes its whole body in the index.
struct ChunkSink<'w> {
    writer: &'w mut IndexWriter,
    f: &'w Fields,
    id: String,
    harness: String,
    cwd: Option<String>,
    path: String,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    mtime: i64,
    size: i64,
    disc_title: Option<String>,
    meta_title: Option<String>,
    meta_received: bool,
    first_user: Option<String>,
    /// Sub-agent attribution stamped on every doc of a folded-in forest transcript; empty
    /// (all-`None`) for a top-level session, leaving its docs unchanged.
    prov: cv_core::events::Provenance,
    resolver: cv_core::Resolver,
    buf: String,
    /// Whitespace-collapsed head of the body, capped at [`PREVIEW_CHARS`]; stored on the first doc.
    preview: String,
    docs: usize,
    err: Option<anyhow::Error>,
}

/// Length cap (chars) of the stored head-of-body preview — the snippet fallback when a hit's
/// source file is gone at query time. ~240 bytes/session keeps the index lean.
const PREVIEW_CHARS: usize = 240;

impl<'w> ChunkSink<'w> {
    fn new(
        writer: &'w mut IndexWriter,
        f: &'w Fields,
        r: &cv_core::SessionRef,
        mtime: i64,
        size: i64,
        prov: cv_core::events::Provenance,
    ) -> Self {
        ChunkSink {
            writer,
            f,
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            path: r.path.display().to_string(),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            mtime,
            size,
            disc_title: r.title.clone(),
            meta_title: None,
            meta_received: false,
            first_user: None,
            prov,
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            buf: String::new(),
            preview: String::new(),
            docs: 0,
            err: None,
        }
    }

    /// Session label for display: the parsed title when the adapter delivered metadata (authoritative,
    /// e.g. codex's None → first-user fallback), else the discovery-time title (claude's ai-title).
    fn title(&self) -> String {
        let primary = if self.meta_received {
            self.meta_title.as_deref()
        } else {
            self.disc_title.as_deref()
        };
        cv_core::label_from(primary, self.first_user.as_deref())
    }

    /// Flush the current buffer as one document. `force` emits even an empty buffer (so a session with
    /// no body still gets a doc — needed for incremental mtime tracking).
    fn flush(&mut self, force: bool) {
        if self.err.is_some() || (self.buf.is_empty() && !force) {
            return;
        }
        let mut doc = TantivyDocument::default();
        doc.add_text(self.f.id, &self.id);
        doc.add_text(self.f.harness, &self.harness);
        if let Some(cwd) = &self.cwd {
            doc.add_text(self.f.cwd, cwd);
        }
        doc.add_text(self.f.title, self.title());
        doc.add_text(self.f.body, &self.buf);
        doc.add_text(self.f.path, &self.path);
        // The preview rides only the session's first doc — one copy per session, not per chunk.
        if self.docs == 0 && !self.preview.is_empty() {
            doc.add_text(self.f.preview, &self.preview);
        }
        if let Some(t) = self.created_at {
            doc.add_i64(self.f.created_at, t);
        }
        if let Some(t) = self.updated_at {
            doc.add_i64(self.f.updated_at, t);
        }
        doc.add_i64(self.f.mtime, self.mtime);
        // `size` is the primary incremental freshness key for append-only transcript logs.
        doc.add_i64(self.f.size, self.size);
        // Provenance rides every doc of a folded-in sub-agent; top-level docs add nothing here, so
        // they stay byte-for-byte as before.
        if let Some(p) = &self.prov.parent_id {
            doc.add_text(self.f.parent_id, p);
        }
        if let Some(a) = &self.prov.agent_id {
            doc.add_text(self.f.agent_id, a);
        }
        if let Some(w) = &self.prov.workflow {
            doc.add_text(self.f.workflow, w);
        }
        if let Err(e) = self.writer.add_document(doc) {
            self.err = Some(e.into());
        }
        self.buf.clear();
        self.docs += 1;
    }

    /// Flush the trailing buffer (always emits ≥1 doc for the session), propagating any deferred error.
    fn finish(&mut self) -> Result<()> {
        self.flush(self.docs == 0);
        if let Some(e) = self.err.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl<'w> ChunkSink<'w> {
    /// Append `s` to the buffer, flushing a doc whenever it reaches a chunk.
    fn push_chunk(&mut self, s: &str) {
        self.extend_preview(s);
        self.buf.push_str(s);
        if self.buf.len() >= CHUNK_BYTES {
            self.flush(false);
        }
    }

    /// Grow the stored preview from the head of the body: words joined by single spaces (newlines
    /// and runs of whitespace collapse away), truncated at [`PREVIEW_CHARS`]. No-op once full, so
    /// the cost is bounded no matter how large the session is.
    fn extend_preview(&mut self, s: &str) {
        let mut n = self.preview.chars().count();
        if n >= PREVIEW_CHARS {
            return;
        }
        for word in s.split_whitespace() {
            if n >= PREVIEW_CHARS {
                break;
            }
            if !self.preview.is_empty() {
                self.preview.push(' ');
                n += 1;
            }
            for c in word.chars() {
                if n >= PREVIEW_CHARS {
                    break;
                }
                self.preview.push(c);
                n += 1;
            }
        }
    }

    /// Append a content `Text`: chunk-resolved from the source for a span (never owned whole), or the
    /// inline string. `r` is the session resolver (moved out of `self` to avoid a borrow conflict with
    /// the chunk callback, which mutates `self`).
    fn append_text(&mut self, r: &cv_core::Resolver, text: &cv_core::Text) {
        if let Some(sp) = text.as_span() {
            r.for_each_chunk(sp, CHUNK_BYTES, |s| self.push_chunk(s));
        } else if let Some(s) = text.inline_str() {
            self.push_chunk(s);
        }
    }

    /// Append one block's searchable text — identical projection to
    /// [`cv_core::stream::append_searchable`], but chunk-resolving span content.
    fn append_block(&mut self, r: &cv_core::Resolver, b: &cv_core::Block) {
        use cv_core::Block;
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                self.append_text(r, text);
                self.push_chunk("\n");
            }
            Block::ToolUse { name, input, .. } => {
                self.push_chunk(name);
                self.push_chunk(" ");
                self.push_chunk(&input.to_string());
                self.push_chunk("\n");
            }
            Block::ToolResult { content, .. } => {
                self.append_text(r, content);
                self.push_chunk("\n");
                // A persisted-output stub (Claude Code parked the real output in
                // `<session>/tool-results/<id>.txt`): index the file's head too, so the text the tool
                // produced is findable even though the transcript holds only the pointer.
                if let Some(p) = b.persisted_output_path() {
                    if let Some(t) = cv_core::lazy::read_head(std::path::Path::new(p), PERSISTED_INDEX_CAP) {
                        for piece in t.as_bytes().chunks(CHUNK_BYTES) {
                            self.push_chunk(&String::from_utf8_lossy(piece));
                        }
                        self.push_chunk("\n");
                    }
                }
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    self.push_chunk(p);
                    self.push_chunk("\n");
                }
            }
            Block::Image { .. } => {}
        }
    }
}

/// How much of a persisted tool output (a sidecar file) to index per stub. A multi-MB dump beyond
/// this is still on disk (`cv show` prints the path); the index just carries its head.
const PERSISTED_INDEX_CAP: usize = 1024 * 1024;

impl cv_core::MessageSink for ChunkSink<'_> {
    fn meta(&mut self, s: &cv_core::Session) {
        self.meta_title = s.title.clone();
        self.meta_received = true;
        if self.cwd.is_none() {
            self.cwd = s.cwd.as_ref().map(|p| p.display().to_string());
        }
    }
    fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
        use cv_core::Block;
        if self.err.is_some() {
            return cv_core::Flow::Stop;
        }
        // Move the resolver out so the chunk callbacks (which mutate `self`) don't conflict with it.
        let r = std::mem::replace(&mut self.resolver, cv_core::Resolver::new(None));
        // First user text → the title fallback. Only its first 72 chars are ever read
        // (`label_from` truncates), so a span resolves just a 512-byte head, never the whole field.
        if self.first_user.is_none() && m.role == cv_core::Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    let s = match text.as_span() {
                        Some(sp) => r.resolve_prefix(sp, 512),
                        None => text.resolve(&r),
                    };
                    let t = s.trim();
                    if !t.is_empty() {
                        self.first_user = Some(t.to_string());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            self.append_block(&r, b);
        }
        self.resolver = r;
        cv_core::Flow::Continue
    }
}

// The size-primary policy (append-only JSONL logs skip on unchanged size even when mtimes were
// mass-bumped) is shared with the event catalog and lives in cv-core.
use cv_core::events::size_primary_freshness;

/// Whether an indexed session is FTS-fresh given its stored `(mtime, size)` and the current file
/// signature. For append-only transcript logs, size is primary: unchanged size means no new content
/// even if mtime was spuriously bumped (touch / rsync / restore). For non-append stores, a same-size
/// rewrite is possible, so mtime must also match. A `size` of 0 (unreadable file) is never fresh,
/// mirroring [`cv_core::offsets`]'s `(0, 0)` semantics. An entry is present only when a doc carried a
/// size; a pre-`size` index never reaches here (it's rebuilt by [`open_or_create`]), so an unknown
/// size is correctly treated as not-fresh rather than skipped.
fn fts_is_fresh(r: &cv_core::SessionRef, stored: Option<&(i64, i64)>, mtime: i64, size: i64) -> bool {
    stored.is_some_and(|&(stored_mtime, stored_size)| {
        stored_size == size && size != 0 && (size_primary_freshness(r) || (stored_mtime == mtime && mtime != 0))
    })
}

/// `id → (mtime, size)` for every live doc (the incremental skip-set), plus the set of ids that
/// are folded-in sub-agent docs (those tagged with a `parent_id`), which a plain refresh's reap
/// must leave alone.
type IndexedSigs = (HashMap<String, (i64, i64)>, HashSet<String>);

/// Read the [`IndexedSigs`] off every live document in the index. Stored fields are tiny (no body
/// is stored), so scanning them all is cheap.
fn read_indexed_sigs(index: &Index, f: &Fields) -> Result<IndexedSigs> {
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    let mut out = HashMap::new();
    let mut folded = HashSet::new();
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(0).context("opening store reader")?;
        for doc_id in seg.doc_ids_alive() {
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let id = doc.get_first(f.id).and_then(|v| v.as_str()).map(str::to_string);
            let mt = doc.get_first(f.mtime).and_then(|v| v.as_i64());
            let sz = doc.get_first(f.size).and_then(|v| v.as_i64());
            if let (Some(id), Some(mt), Some(sz)) = (id, mt, sz) {
                if doc.get_first(f.parent_id).is_some() {
                    folded.insert(id.clone());
                }
                out.insert(id, (mt, sz));
            }
        }
    }
    Ok((out, folded))
}

/// The newest file mtime (ns) stamped on any indexed doc — how far the index has caught up. `None`
/// when the index can't be opened or is empty. One stored-field scan, the same cost incremental
/// indexing pays for its skip-set.
pub fn newest_indexed_mtime(dir: &Path) -> Option<i64> {
    let (index, f) = open_existing(dir).ok()?;
    let (sigs, _) = read_indexed_sigs(&index, &f).ok()?;
    sigs.values().map(|&(mt, _)| mt).max()
}

/// Whether the index holds any folded-in sub-agent transcripts (the `cv index --subagents` forest,
/// detected by a stored `parent_id` on a doc). The empty-result path uses this to nudge toward
/// `--subagents` **only** when the forest is genuinely absent — not when a query simply didn't
/// match a corpus that already includes it. An open/read error reads as `true` (assume present) so
/// a transient failure never spams the hint. One stored-field scan — the same cheap pass
/// [`newest_indexed_mtime`] makes (no body is stored).
pub fn has_subagent_docs(dir: &Path) -> bool {
    let Ok((index, f)) = open_existing(dir) else {
        return true;
    };
    match read_indexed_sigs(&index, &f) {
        Ok((_, folded)) => !folded.is_empty(),
        Err(_) => true,
    }
}

/// Index an explicit set of sessions through the real production [`index_session`] path — the same
/// indexer [`index_all`] drives, minus only the global `discover_all()` scan. Full rebuild: clears
/// prior contents first. Used by tests so date-field (and chunk) indexing is exercised against the
/// code that actually ships rather than a parallel per-doc path. `catalog = true` additionally tees
/// each pass into the event catalog and (for supported harnesses) the message-offsets store, like
/// `index_all` does (only set it under an isolated `CLUSTERVISION_HOME` — it writes the shared
/// catalog db).
#[cfg(test)]
pub(crate) fn index_refs(dir: &Path, refs: &[cv_core::SessionRef], catalog: bool) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(50_000_000).context("creating tantivy index writer")?;
    writer.delete_all_documents().context("clearing index")?;
    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        let mut es = catalog.then(|| cv_core::events::EventSink::new(r.cwd.clone()));
        let mut os = (catalog && cv_core::offsets::supported(r.harness)).then(cv_core::offsets::OffsetSink::new);
        index_session(
            &mut writer,
            &f,
            adapter.as_ref(),
            r,
            mtime,
            size,
            es.as_mut(),
            os.as_mut(),
            cv_core::events::Provenance::default(),
        )
        .with_context(|| format!("indexing {}", r.id))?;
        if let Some(es) = es {
            cv_core::events::record(r, es.events(), mtime, size);
        }
        if let Some(os) = os {
            cv_core::offsets::record(r, &os, mtime, size);
        }
    }
    writer.commit().context("committing index")?;
    Ok(refs.len())
}

/// Index an explicit set of sessions through the real **incremental** path — mirroring `index_all`'s
/// per-session freshness skip ([`fts_is_fresh`] over the [`read_indexed_sigs`] skip-set) and its
/// end-of-sweep [`reap_missing`] (as a plain, no-`--subagents` refresh) exactly, minus only the
/// global `discover_all()` scan (which can't be pointed at a temp dir). Unlike [`index_refs`] it
/// does **not** clear first, so a second call with unchanged files exercises the skip. Returns the
/// number of sessions actually (re)indexed — the count the regression test asserts goes to 0 on a
/// pure mtime bump and to 1 on a real append. `catalog` left off here (FTS-only).
#[cfg(test)]
pub(crate) fn index_refs_incremental(dir: &Path, refs: &[cv_core::SessionRef]) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(50_000_000).context("creating tantivy index writer")?;
    let (existing, folded) = read_indexed_sigs(&index, &f).unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut changed = 0usize;
    for r in refs {
        seen.insert(r.id.clone());
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let (mtime, size) = cv_core::offsets::file_sig(&r.path);
        // The exact production freshness gate.
        if fts_is_fresh(r, existing.get(&r.id), mtime, size) {
            continue;
        }
        if existing.contains_key(&r.id) {
            writer.delete_term(Term::from_field_text(f.id, &r.id));
        }
        index_session_clean(
            &mut writer,
            &f,
            adapter.as_ref(),
            r,
            mtime,
            size,
            None,
            None,
            cv_core::events::Provenance::default(),
        )
        .with_context(|| format!("indexing {}", r.id))?;
        changed += 1;
    }
    // The exact production reap, as a plain (no --subagents) refresh: vanished top-level sessions
    // go, the folded sub-agent forest stays.
    reap_missing(&mut writer, &f, &existing, &seen, &folded, false);
    writer.commit().context("committing index")?;
    Ok(changed)
}

/// Index `refs` (top-level) **and** their sub-agent forests, exercising the exact production forest
/// path [`index_one_subagent`] that `index_all(.., subagents=true)` drives — minus only the global
/// `discover_all()` scan (which can't be pointed at a temp dir in a unit test). `catalog = true` tees
/// each pass into the event catalog (set it only under an isolated `CLUSTERVISION_HOME`). Returns the
/// number of sub-agent transcripts folded in.
#[cfg(test)]
pub(crate) fn index_refs_with_subagents(dir: &Path, refs: &[cv_core::SessionRef], catalog: bool) -> Result<usize> {
    // First the top-level pass (clears + indexes the parents).
    index_refs(dir, refs, catalog)?;
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(50_000_000).context("creating tantivy index writer")?;
    let (existing, _folded) = read_indexed_sigs(&index, &f).unwrap_or_default();
    let event_sync = cv_core::events::SyncTable::load();
    let offset_sync = cv_core::offsets::SyncTable::load();
    let mut folded = 0usize;
    for r in refs {
        for sub in cv_core::subagent_tree_of(r) {
            // In a no-catalog test the skip tables are empty, so every agent indexes; with a catalog
            // the freshness skip behaves exactly as `index_all` does.
            let docs = index_one_subagent(&mut writer, &f, r, &sub, &existing, &event_sync, &offset_sync)?;
            if docs > 0 {
                folded += 1;
            }
        }
    }
    writer.commit().context("committing index")?;
    Ok(folded)
}

/// Run a full-text query, returning up to `limit` hits ranked by BM25, each with a live snippet and
/// its `created_at`/`updated_at` (so callers needn't re-discover the corpus just to date the rows).
/// If the hit's source file can't be re-read (moved/changed since indexing), the snippet falls back
/// to the head-of-body preview stored at index time, suffixed with `(source file moved)` when the
/// file is gone entirely.
///
/// The query supports tantivy's syntax: bare terms search title+body+cwd, and fielded terms like
/// `harness:claude`, phrases `"foo bar"`, and booleans `a AND b` work too.
pub fn text_search(dir: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
    if !dir.join("meta.json").exists() {
        anyhow::bail!("no full-text index at {} — run `index_all` first", dir.display());
    }
    // Read-only open: a transient failure here must surface as an error, never as a wiped index.
    let (index, f) = open_existing(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();

    let mut parser = QueryParser::for_index(&index, vec![f.title, f.body, f.cwd]);
    parser.set_conjunction_by_default(); // all terms must match — better precision than OR
    let parsed = parser
        .parse_query(query)
        .with_context(|| format!("parsing query {query:?}"))?;

    // A session is several docs (body chunks) sharing one id, so over-fetch and dedup to one row per
    // id, keeping the best-scoring doc (TopDocs is score-ordered, so the first id occurrence wins).
    let fetch = (limit.saturating_mul(4)).max(64);
    let top = searcher
        .search(&parsed, &TopDocs::with_limit(fetch).order_by_score())
        .context("running search")?;

    let mut hits = Vec::with_capacity(limit);
    let mut seen: HashSet<String> = HashSet::new();
    for (score, addr) in top {
        if hits.len() >= limit {
            break;
        }
        let doc: TantivyDocument = searcher.doc(addr).context("loading stored doc")?;
        let get_str =
            |field: Field| -> Option<String> { doc.get_first(field).and_then(|v| v.as_str()).map(|s| s.to_string()) };
        let id = get_str(f.id).unwrap_or_default();
        if !seen.insert(id.clone()) {
            continue; // another chunk of a session we already have
        }
        let harness = get_str(f.harness).unwrap_or_default();
        let path = get_str(f.path);
        let snippet = match path
            .as_deref()
            .and_then(|p| live_snippet(p, &harness, query))
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            // The source file is unreadable (moved/changed) or yielded nothing — fall back to the
            // preview stored at index time. It rides the session's *first* chunk doc, which may
            // not be the doc that scored this hit, hence the by-id lookup.
            None => {
                let preview = get_str(f.preview)
                    .filter(|s| !s.is_empty())
                    .or_else(|| stored_preview(&searcher, &f, &id))
                    .unwrap_or_default();
                let gone = path.as_deref().is_none_or(|p| !Path::new(p).exists());
                match (gone, preview.is_empty()) {
                    (true, true) => "(source file moved)".to_string(),
                    (true, false) => format!("{preview} (source file moved)"),
                    (false, _) => preview,
                }
            }
        };

        hits.push(Hit {
            id,
            harness,
            cwd: get_str(f.cwd),
            title: get_str(f.title),
            created_at: doc.get_first(f.created_at).and_then(|v| v.as_i64()),
            updated_at: doc.get_first(f.updated_at).and_then(|v| v.as_i64()),
            score,
            snippet,
            parent_id: get_str(f.parent_id),
            agent_id: get_str(f.agent_id),
            workflow: get_str(f.workflow),
        });
    }
    Ok(hits)
}

/// Fetch the stored head-of-body preview for session `id`. The preview lives only on the session's
/// **first** chunk doc, which for a multi-chunk session is usually not the doc that produced the
/// hit — so look it up across all of the id's docs. Only runs on the fallback path (live snippet
/// already failed), so the extra term query costs nothing in the common case.
fn stored_preview(searcher: &tantivy::Searcher, f: &Fields, id: &str) -> Option<String> {
    use tantivy::collector::DocSetCollector;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    let q = TermQuery::new(Term::from_field_text(f.id, id), IndexRecordOption::Basic);
    let docs = searcher.search(&q, &DocSetCollector).ok()?;
    for addr in docs {
        let doc: TantivyDocument = match searcher.doc(addr) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Some(p) = doc.get_first(f.preview).and_then(|v| v.as_str()) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Most text we'll scan from a session to find a snippet window. Matches are almost always early;
/// capping keeps snippeting a multi-GB hit bounded (and Phase-2 offsets will make it a seek).
const SNIPPET_SCAN_CAP: usize = 256 * 1024;

/// Generate a snippet for a hit by re-reading **just that one session** (streamed, capped), finding
/// the first query term, and windowing around it — instead of keeping a stored copy of every body.
///
/// Streams under [`ParseOptions::lazy`] and resolves span content **capped at the remaining scan
/// budget**, so a session fronted by a giant record (e.g. a 700 MB tool dump) costs at most
/// [`SNIPPET_SCAN_CAP`] here — the old `bulk()` + materialize path owned the whole record before
/// the cap check could run. Trade-off: a term that only occurs *beyond* the cap inside one giant
/// field now falls back to the head/preview snippet (it already did for terms in later messages).
fn live_snippet(path: &str, harness: &str, query: &str) -> Option<String> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, SessionRef};
    let h = cv_core::Harness::parse(harness)?;
    let adapter = cv_core::harness::for_harness(h)?;
    let sref = SessionRef {
        id: String::new(),
        harness: h,
        path: PathBuf::from(path),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };

    struct Acc {
        buf: String,
        resolver: cv_core::Resolver,
    }
    impl Acc {
        /// Append one content text: inline as-is; a span resolved to at most the remaining budget
        /// (never materialized whole — `resolve_prefix` also keeps a truncated escaped span clean).
        fn push_text(&mut self, text: &cv_core::Text) {
            if let Some(sp) = text.as_span() {
                let remaining = SNIPPET_SCAN_CAP.saturating_sub(self.buf.len()) as u64;
                if remaining == 0 {
                    return;
                }
                self.buf.push_str(&self.resolver.resolve_prefix(sp, remaining));
            } else if let Some(s) = text.inline_str() {
                self.buf.push_str(s);
            }
        }
    }
    impl MessageSink for Acc {
        fn message(&mut self, m: Message) -> Flow {
            // Same projection as `cv_core::stream::append_searchable`, but span-aware.
            use std::fmt::Write as _;
            for b in &m.content {
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        self.push_text(text);
                        self.buf.push('\n');
                    }
                    Block::ToolUse { name, input, .. } => {
                        self.buf.push_str(name);
                        self.buf.push(' ');
                        let _ = write!(self.buf, "{input}");
                        self.buf.push('\n');
                    }
                    Block::ToolResult { content, .. } => {
                        self.push_text(content);
                        self.buf.push('\n');
                        // Persisted output: scan the sidecar file's head for the snippet too.
                        if let Some(p) = b.persisted_output_path() {
                            let remaining = SNIPPET_SCAN_CAP.saturating_sub(self.buf.len());
                            if remaining > 0 {
                                if let Some(t) = cv_core::lazy::read_head(std::path::Path::new(p), remaining) {
                                    self.buf.push_str(&t);
                                    self.buf.push('\n');
                                }
                            }
                        }
                    }
                    Block::File { path, source, .. } => {
                        if let Some(p) = path.as_deref().or(source.as_deref()) {
                            self.buf.push_str(p);
                            self.buf.push('\n');
                        }
                    }
                    Block::Image { .. } => {}
                }
            }
            if self.buf.len() >= SNIPPET_SCAN_CAP {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    let mut acc = Acc {
        buf: String::new(),
        resolver: cv_core::Resolver::new(Some(PathBuf::from(path))),
    };
    adapter.stream(&sref, &ParseOptions::lazy(), &mut acc).ok()?;
    Some(make_snippet(&acc.buf, query))
}

/// Window the body around the first matching query term; fall back to a head if nothing matches.
fn make_snippet(body: &str, query: &str) -> String {
    let lc = body.to_lowercase();
    for term in query_terms(query) {
        if let Some(pos) = lc.find(&term) {
            return window(body, pos, term.len());
        }
    }
    head(body, 200)
}

/// The bare, matchable terms of a tantivy query: drops `field:value` filters and boolean operators,
/// keeps lowercased word tokens. Used only to locate a snippet window, not for retrieval.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.contains(':'))
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn window(body: &str, pos: usize, len: usize) -> String {
    let start = floor_char(body, pos.saturating_sub(40));
    let end = ceil_char(body, (pos + len + 80).min(body.len()));
    let s = &body[start..end];
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        let t: String = s.chars().take(199).collect();
        format!("{t}…")
    } else {
        s
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Remove a single session from the index by id (handy for incremental updates).
#[allow(dead_code)]
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    let (index, f) = open_existing(dir)?;
    let mut writer: IndexWriter = index.writer(15_000_000)?;
    writer.delete_term(Term::from_field_text(f.id, id));
    writer.commit()?;
    Ok(())
}

fn head(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cv-search-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Indexing tees events into the catalog, whose path comes from the process-global
    /// `CLUSTERVISION_HOME`. Tests that index MUST isolate it to a temp dir — otherwise they write to
    /// (and read from) the shared real catalog, racing each other and the other crates' test binaries.
    /// This RAII guard serializes those tests on one lock AND points the catalog at a private temp
    /// home for the test's duration; both are released (and the home removed) on drop.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct IsolatedHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        home: std::path::PathBuf,
    }
    impl IsolatedHome {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let home = tmpdir();
            std::env::set_var("CLUSTERVISION_HOME", &home);
            IsolatedHome { _lock: lock, home }
        }
    }
    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            std::env::remove_var("CLUSTERVISION_HOME");
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    /// Write a minimal one-message Claude transcript so `live_snippet` has a real file to re-read,
    /// and return its path. `body` becomes the single user message's text.
    fn write_claude(dir: &Path, id: &str, body: &str) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": id,
            "sessionId": id,
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p.display().to_string()
    }

    /// A [`cv_core::SessionRef`] pointing at a written Claude transcript, with the dates the index is
    /// expected to surface (`created_at`→1s, `updated_at`→2s, as the hit assertions check). The
    /// production `ChunkSink` reads these straight off the ref — Claude's adapter never emits a
    /// metadata title, so `title` here is the authoritative label.
    fn sref(id: &str, title: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            id: id.into(),
            harness: cv_core::Harness::Claude,
            path: path.into(),
            cwd: Some("/home/u/proj".into()),
            title: Some(title.into()),
            created_at: chrono::DateTime::from_timestamp(1, 0),
            updated_at: chrono::DateTime::from_timestamp(2, 0),
            message_count: 1,
        }
    }

    /// Like [`sref`] but with no discovery-time title — so the indexed label falls back to the first
    /// user message's text (the path Claude sessions without an `ai-title` line take).
    fn sref_untitled(id: &str, path: String) -> cv_core::SessionRef {
        cv_core::SessionRef {
            title: None,
            ..sref(id, "", path)
        }
    }

    /// Write a multi-message Claude transcript (one JSONL record per `(role, content)`), returning its
    /// path. `role` is `"user"` or `"assistant"`; content is a bare string.
    fn write_claude_msgs(dir: &Path, id: &str, msgs: &[(&str, &str)]) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let mut out = String::new();
        for (i, (role, content)) in msgs.iter().enumerate() {
            let line = serde_json::json!({
                "type": role,
                "uuid": format!("{id}-{i}"),
                "sessionId": id,
                "message": { "role": role, "content": content }
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
        std::fs::write(&p, out).unwrap();
        p.display().to_string()
    }

    #[test]
    fn index_and_search_with_live_snippets() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let b1 = "We built a full text search engine with tantivy and BM25 scoring.";
        let b2 = "Loading dataframes and grouping by columns in pandas.";
        let p1 = write_claude(&sdir, "a1", b1);
        let p2 = write_claude(&sdir, "b2", b2);
        let refs = vec![sref("a1", "Rust tantivy index", p1), sref("b2", "Python pandas", p2)];
        let n = index_refs(&dir, &refs, false).unwrap();
        assert_eq!(n, 2);

        // Term unique to the first doc; snippet is generated live from the session file.
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected exactly one match for 'tantivy'");
        assert_eq!(hits[0].id, "a1");
        assert!(
            hits[0].snippet.contains("tantivy"),
            "live snippet should mention the term: {:?}",
            hits[0].snippet
        );
        // Dates come straight off the index now (no corpus re-discovery).
        assert_eq!(hits[0].created_at, Some(1));
        assert_eq!(hits[0].updated_at, Some(2));

        let hits = text_search(&dir, "pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b2");

        // Fielded query restricts by harness.
        let hits = text_search(&dir, "harness:claude pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, "claude");

        // Conjunction-by-default: a term present in neither yields nothing.
        assert!(text_search(&dir, "kubernetes", 10).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn persisted_tool_output_files_are_indexed_and_snippeted() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Claude Code parks a too-large tool output in `<session>/tool-results/<id>.txt` and leaves
        // only a stub (pointer + preview) in the transcript. The marker word exists ONLY in the file.
        let tr = sdir.join("p1").join("tool-results");
        std::fs::create_dir_all(&tr).unwrap();
        let file = tr.join("abc123.txt");
        std::fs::write(
            &file,
            "line one\nthe zanzibar constant appears only on disk\nline three\n",
        )
        .unwrap();
        let stub = format!(
            "<persisted-output>\nOutput too large (66KB). Full output saved to: {}\n\nPreview (first 2KB):\nline one\n</persisted-output>",
            file.display()
        );
        let p = sdir.join("p1.jsonl");
        let lines = [
            serde_json::json!({"type":"user","uuid":"u0","sessionId":"p1","timestamp":"2026-09-19T00:00:00Z",
                "message":{"role":"user","content":"run the harvester"}}),
            serde_json::json!({"type":"assistant","uuid":"a0","parentUuid":"u0","sessionId":"p1","timestamp":"2026-09-19T00:00:01Z",
                "message":{"role":"assistant","model":"m","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"harvest"}}]}}),
            serde_json::json!({"type":"user","uuid":"u1","parentUuid":"a0","sessionId":"p1","timestamp":"2026-09-19T00:00:02Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}}),
        ];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        index_refs(&dir, &[sref("p1", "harvest run", p.display().to_string())], false).unwrap();

        let hits = text_search(&dir, "zanzibar", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "a word that lives only in the persisted file is findable"
        );
        assert_eq!(hits[0].id, "p1");
        assert!(
            hits[0].snippet.contains("zanzibar"),
            "the live snippet scans the file too: {:?}",
            hits[0].snippet
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn incremental_skips_unchanged_and_reaps_missing() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "x", "alpha beta gamma");
        // Seed the index with one session.
        index_refs(&dir, &[sref("x", "first", p.clone())], false).unwrap();
        assert_eq!(text_search(&dir, "alpha", 10).unwrap().len(), 1);

        // A direct full-rebuild replaces contents (old doc gone).
        let p2 = write_claude(&sdir, "y", "delta epsilon");
        index_refs(&dir, &[sref("y", "second", p2)], false).unwrap();
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "delta", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Rewrite a file with its own byte-identical content — bumping mtime (the OS stamps "now" on
    /// write) while leaving size unchanged: the precise "mtime mass-bump, no real change" condition
    /// from the live incident. No new dep needed; the freshness check ignores the mtime value anyway,
    /// so an unchanged size is all that decides the skip.
    fn bump_mtime(path: &str) {
        let content = std::fs::read(path).unwrap();
        let before = std::fs::metadata(path).unwrap().len();
        std::fs::write(path, &content).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            before,
            "bump_mtime must not change size"
        );
    }

    /// Append a line to a session transcript (size grows) — a real content change.
    fn append(path: &str, extra: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "type": "user",
            "uuid": "append",
            "message": { "role": "user", "content": extra }
        });
        let before = std::fs::metadata(path).unwrap().len();
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
        assert!(
            std::fs::metadata(path).unwrap().len() > before,
            "append must grow the file"
        );
    }

    /// Regression for bug-class `fts-incremental-lossy-mtime`: incremental freshness for append-only
    /// transcripts keys on file **size**, not mtime. A pure mtime bump (touch / rsync / restore) with
    /// byte-identical content must **skip** — the pre-fix mtime-only check
    /// re-indexed the whole corpus here, which is what blew past the caller timeout and spiralled.
    /// A real append (size grows) must re-index exactly that one session.
    #[test]
    fn incremental_keys_on_size_not_mtime() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let p1 = write_claude(&sdir, "s1", "alpha session one body");
        let p2 = write_claude(&sdir, "s2", "beta session two body");
        let refs = || vec![sref("s1", "one", p1.clone()), sref("s2", "two", p2.clone())];

        // Initial index: both sessions are new → both indexed.
        assert_eq!(
            index_refs_incremental(&dir, &refs()).unwrap(),
            2,
            "first pass indexes both new sessions"
        );

        // Bump BOTH files' mtime forward while keeping content (and therefore size) identical — the
        // exact "mtimes mass-bumped" condition from the live incident. Size unchanged ⇒ must skip.
        bump_mtime(&p1);
        bump_mtime(&p2);
        let reindexed = index_refs_incremental(&dir, &refs()).unwrap();
        assert_eq!(
            reindexed, 0,
            "pure mtime bump (size unchanged) must re-index NOTHING — got {reindexed} (the lossy-mtime bug)"
        );

        // Append to exactly one session (size grows) → that one re-indexes, the other still skips.
        append(&p2, "gamma appended tail");
        let reindexed = index_refs_incremental(&dir, &refs()).unwrap();
        assert_eq!(
            reindexed, 1,
            "a real append (size grew) must re-index exactly the appended session — got {reindexed}"
        );
        // And the appended content is now searchable.
        let hits = text_search(&dir, "gamma", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s2");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Unit guard on the freshness predicate itself: append-only logs use size as the primary signal,
    /// while rewriteable stores still require mtime. An unknown size (no entry) or a 0 size (unreadable
    /// file) is never fresh.
    #[test]
    fn fts_is_fresh_uses_size_primary_only_for_append_only_sources() {
        let append_only = sref("append", "append", "/tmp/append.jsonl".into());
        let non_append = cv_core::SessionRef {
            id: "cline-task".into(),
            harness: cv_core::Harness::Cline,
            path: "/tmp/task/api_conversation_history.json".into(),
            cwd: Some("/home/u/proj".into()),
            title: Some("task".into()),
            created_at: None,
            updated_at: None,
            message_count: 1,
        };

        assert!(size_primary_freshness(&append_only));
        assert!(!size_primary_freshness(&non_append));

        // Same size, different mtime is fresh only for append-only logs.
        assert!(fts_is_fresh(&append_only, Some(&(111, 4096)), 222, 4096));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 4096)), 111, 8192));
        assert!(!fts_is_fresh(&append_only, Some(&(111, 0)), 111, 0));
        assert!(!fts_is_fresh(&append_only, None, 111, 4096));

        assert!(fts_is_fresh(&non_append, Some(&(111, 4096)), 111, 4096));
        assert!(!fts_is_fresh(&non_append, Some(&(111, 4096)), 222, 4096));
        assert!(!fts_is_fresh(&non_append, Some(&(111, 4096)), 111, 8192));
    }

    /// The core invariant of `ChunkSink`: a body larger than [`CHUNK_BYTES`] is flushed into several
    /// tantivy docs that all share the session id. Search must (a) still find a term living only in a
    /// late chunk, and (b) dedup a term present in *every* chunk back to a single row per id.
    #[test]
    fn oversized_body_is_chunked_yet_dedups_to_one_hit() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // ~5.6 MB of a repeated common token (lands in every chunk) with a unique marker at the very
        // end (lands only in the final chunk) — forcing ≥2 docs for the one session.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "big", &body);
        index_refs(&dir, &[sref("big", "huge session", p)], false).unwrap();

        // Term in every chunk → multiple matching docs, but deduped to exactly one row for the id.
        let hits = text_search(&dir, "padding", 10).unwrap();
        assert_eq!(hits.len(), 1, "chunks of one session must dedup to a single hit");
        assert_eq!(hits[0].id, "big");

        // Term only in the trailing chunk → still indexed and findable (proves later chunks are added).
        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "big");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// A session with no discovery title labels itself from the first user message, and the body
    /// accumulates across roles — so an assistant-only term is searchable too.
    #[test]
    fn title_falls_back_to_first_user_and_body_spans_roles() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude_msgs(
            &sdir,
            "m",
            &[
                ("user", "alpha question about tantivy"),
                ("assistant", "beta answer mentioning bm25"),
            ],
        );
        index_refs(&dir, &[sref_untitled("m", p)], false).unwrap();

        // Assistant-turn term is in the body (roles accumulate into one searchable blob).
        let hits = text_search(&dir, "bm25", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m");
        // No ai-title → label is the first user message's text.
        assert_eq!(hits[0].title.as_deref(), Some("alpha question about tantivy"));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// `live_snippet` must stay O(cap) on a session fronted by one giant field: a term within the
    /// scan cap windows normally, a term hiding *beyond* the cap inside the same giant field falls
    /// back to the head instead of materializing the whole record to find it.
    #[test]
    fn live_snippet_caps_giant_fields() {
        let sdir = tmpdir();
        let mut body = String::from("zqearly marker then padding ");
        body.push_str(&"pad ".repeat(2 * SNIPPET_SCAN_CAP / 4)); // 2× the cap of filler
        body.push_str(" zqlate");
        assert!(body.len() > SNIPPET_SCAN_CAP);
        let p = write_claude(&sdir, "giant", &body);

        let snip = live_snippet(&p, "claude", "zqearly").expect("snippet");
        assert!(snip.contains("zqearly"), "in-cap term should window: {snip:?}");

        let snip = live_snippet(&p, "claude", "zqlate").expect("snippet");
        assert!(
            !snip.contains("zqlate") && snip.starts_with("zqearly"),
            "beyond-cap term must fall back to the head, got: {snip:?}"
        );

        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn make_snippet_windows_match_then_falls_back_to_head() {
        let body = "the quick brown fox jumps over the lazy dog";
        let snip = make_snippet(body, "brown");
        assert!(snip.contains("brown"), "snippet should window the match: {snip:?}");
        // No term matches → fall back to the head of the body.
        assert_eq!(make_snippet("hello world", "kubernetes"), "hello world");
    }

    #[test]
    fn query_terms_drops_fields_and_booleans() {
        let terms = query_terms("harness:claude Tantivy AND bm25:score NOT pandas");
        assert_eq!(terms, vec!["tantivy", "pandas"]);
    }

    /// A query made *entirely* of fielded/boolean tokens has no bare terms to window on — the
    /// snippet must silently fall back to the head of the body, not panic or come back empty.
    #[test]
    fn all_fielded_query_snippets_fall_back_to_head() {
        let _home = IsolatedHome::new();
        assert!(query_terms("harness:claude AND cwd:proj").is_empty());
        assert_eq!(make_snippet("hello world", "harness:claude"), "hello world");
        assert_eq!(
            make_snippet("hello world", "harness:claude AND cwd:proj NOT id:x"),
            "hello world"
        );
        // End-to-end: a purely-fielded query still produces a non-empty (head) snippet on a hit.
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "ff", "fielded only query body text");
        index_refs(&dir, &[sref("ff", "fielded", p)], false).unwrap();
        let hits = text_search(&dir, "harness:claude", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("fielded only query body"),
            "head fallback snippet expected: {:?}",
            hits[0].snippet
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Fix for the blank-snippet hole: when a hit's source file has been deleted since indexing,
    /// the snippet falls back to the head-of-body preview stored on the first chunk doc, with a
    /// marker explaining why there's no live context.
    #[test]
    fn missing_source_falls_back_to_stored_preview_with_marker() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let body = "We built a full text\nsearch engine   with tantivy and BM25 scoring.";
        let p = write_claude(&sdir, "gone", body);
        index_refs(&dir, &[sref("gone", "doomed session", p.clone())], false).unwrap();

        // While the file is present, the snippet is live (no marker).
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("(source file moved)"));

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "hit should survive the file vanishing");
        // Preview is whitespace-collapsed: the newline and space-run become single spaces.
        assert!(
            hits[0]
                .snippet
                .contains("We built a full text search engine with tantivy"),
            "stored preview expected: {:?}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.ends_with("(source file moved)"),
            "marker expected: {:?}",
            hits[0].snippet
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The event ride-along, end to end through the production `index_session` tee: one indexing
    /// pass writes BOTH searchable tantivy docs and queryable event rows in the catalog db
    /// (isolated via `CLUSTERVISION_HOME`).
    #[test]
    fn indexing_tees_events_into_the_catalog() {
        let h = IsolatedHome::new();
        let dir = tmpdir();

        // A claude transcript whose assistant turn edits a file and runs a command.
        let p = h.home.join("tee-evt.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u0", "sessionId": "tee-evt",
                "message": {"role": "user", "content": "rework the indexer"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "tee-evt",
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "on it — touching the indexer now"},
                    {"type": "tool_use", "id": "t1", "name": "Edit",
                     "input": {"file_path": "crates/cv-search/src/fts.rs",
                               "old_string": "a", "new_string": "b"}},
                    {"type": "tool_use", "id": "t2", "name": "Bash",
                     "input": {"command": "cargo test -p cv-search"}}
                ]}
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let mut r = sref("tee-evt", "indexer rework", p.display().to_string());
        r.cwd = Some("/repo".into());

        index_refs(&dir, &[r], true).unwrap();

        // The text landed in tantivy…
        let hits = text_search(&dir, "indexer", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "tee-evt");

        // …and the SAME pass landed events in the catalog.
        let rows = cv_core::events::events_for("claude", "tee-evt", None);
        let edit = rows.iter().find(|e| e.kind == "file_edit").expect("edit event");
        assert_eq!(edit.target.as_deref(), Some("/repo/crates/cv-search/src/fts.rs"));
        let cmd = rows.iter().find(|e| e.kind == "command").expect("command event");
        assert_eq!(cmd.target.as_deref(), Some("cargo test -p cv-search"));

        // The touched query resolves it by path suffix.
        let touched = cv_core::events::sessions_touching("crates/cv-search/src/fts.rs", true);
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].session_id, "tee-evt");
        assert_eq!(touched[0].edits, 1);

        std::fs::remove_dir_all(&dir).ok();
        // `h` drops here: removes CLUSTERVISION_HOME, deletes the temp home, releases the lock.
    }

    /// Write a sub-agent transcript in the on-disk layout `subagent_tree_of` expects:
    /// `<parent_dir>/<parent_stem>/subagents/agent-<agent_id>.jsonl`. `body` is one user message.
    /// Returns the agent transcript path (so a test can prove it exists / mtime it).
    fn write_subagent(parent_path: &str, agent_id: &str, body: &str) -> std::path::PathBuf {
        let pp = Path::new(parent_path);
        let stem = pp.file_stem().unwrap().to_str().unwrap();
        let dir = pp.parent().unwrap().join(stem).join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("agent-{agent_id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": format!("agent-{agent_id}"),
            "sessionId": format!("agent-{agent_id}"),
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p
    }

    /// `cv index --subagents` end to end: a string living ONLY inside a sub-agent transcript is
    /// (a) NOT findable when the forest is not folded, and (b) findable AND provenance-tagged
    /// (parent id + agent id) once it is. Exercises the production forest path
    /// (`index_one_subagent`), not a parallel one.
    #[test]
    fn subagent_forest_is_searchable_and_provenance_tagged_only_when_folded() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Parent transcript + a sub-agent whose body alone holds the unique marker.
        let pp = write_claude(&sdir, "parent1", "orchestrator planning the work");
        write_subagent(&pp, "child9", "deep in the weeds: zqsubmarker lives only here");
        let parent = sref("parent1", "parent session", pp);

        // (a) Top-level only: the sub-agent marker is invisible.
        index_refs(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert!(
            text_search(&dir, "zqsubmarker", 10).unwrap().is_empty(),
            "without --subagents a sub-agent-only term must NOT be findable"
        );
        // The parent itself is still indexed.
        assert_eq!(text_search(&dir, "orchestrator", 10).unwrap().len(), 1);
        // The empty-result nudge fires: a top-level-only index carries no folded forest.
        assert!(
            !has_subagent_docs(&dir),
            "a top-level-only index must report no sub-agent forest (so `cv search` nudges --subagents)"
        );

        // (b) Fold the forest: the marker becomes findable, tagged back to its parent + agent.
        let folded = index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(folded, 1, "exactly one sub-agent transcript folded in");
        assert!(
            has_subagent_docs(&dir),
            "once folded, the index must report a sub-agent forest (so the nudge goes quiet)"
        );
        let hits = text_search(&dir, "zqsubmarker", 10).unwrap();
        assert_eq!(hits.len(), 1, "sub-agent term findable once folded");
        let h = &hits[0];
        assert_eq!(h.id, "agent-child9");
        assert_eq!(h.parent_id.as_deref(), Some("parent1"), "tagged with parent id");
        assert_eq!(h.agent_id.as_deref(), Some("child9"), "tagged with agent id");
        assert_eq!(h.workflow, None, "directly-spawned agent has no workflow");
        // A fielded query can scope to one parent's whole forest.
        let scoped = text_search(&dir, "parent_id:parent1 zqsubmarker", 10).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "agent-child9");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Regression for the truncated-fresh hole (B1): a parse error *after* chunk docs were already
    /// flushed into the writer must not leave those docs behind — they carry the full current
    /// `(mtime, size)` stamp, so a later `fts_is_fresh` would skip the session forever and freeze
    /// the truncated head into the index. [`index_session_clean`] (the path both `index_all` and
    /// the sub-agent fold drive) deletes the partial docs on error, dropping the id from the
    /// skip-set so the session is retried next run.
    #[test]
    fn failed_parse_leaves_no_partial_docs_behind() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();

        /// Streams more than one chunk's worth of body into the sink (forcing ≥1 mid-stream doc
        /// flush), then fails — the shape of a transcript whose tail is malformed after a healthy
        /// multi-chunk head.
        struct FailsAfterAChunk;
        impl cv_core::Adapter for FailsAfterAChunk {
            fn harness(&self) -> cv_core::Harness {
                cv_core::Harness::Claude
            }
            fn storage_root(&self) -> Option<std::path::PathBuf> {
                None
            }
            fn discover(&self) -> Result<Vec<cv_core::SessionRef>> {
                Ok(Vec::new())
            }
            fn parse(&self, _r: &cv_core::SessionRef) -> Result<cv_core::Session> {
                anyhow::bail!("unused: this test streams")
            }
            fn stream(
                &self,
                _r: &cv_core::SessionRef,
                _opts: &cv_core::ParseOptions,
                sink: &mut dyn cv_core::MessageSink,
            ) -> Result<cv_core::Session> {
                let mut m = cv_core::Message::new(cv_core::Role::User);
                let body = "zqpartial ".repeat(CHUNK_BYTES / 8); // > CHUNK_BYTES → mid-stream flush
                m.content.push(cv_core::Block::Text { text: body.into() });
                let _ = sink.message(m);
                anyhow::bail!("malformed line mid-transcript")
            }
        }

        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer: IndexWriter = index.writer(50_000_000).unwrap();
        let r = sref("victim", "doomed", "/nonexistent/victim.jsonl".into());
        let res = index_session_clean(
            &mut writer,
            &f,
            &FailsAfterAChunk,
            &r,
            111,
            4096,
            None,
            None,
            cv_core::events::Provenance::default(),
        );
        assert!(res.is_err(), "the fixture adapter must fail");
        // index_all commits after the error (it continues the sweep); mirror that.
        writer.commit().unwrap();

        // No ghost docs: the flushed chunk must be gone…
        assert!(
            text_search(&dir, "zqpartial", 10).unwrap().is_empty(),
            "partial chunk docs from a failed parse must not be committed (B1)"
        );
        // …and the id is absent from the skip-set, so the next incremental pass retries it
        // instead of skipping a truncation stamped fresh.
        let (sigs, _) = read_indexed_sigs(&index, &f).unwrap();
        assert!(
            !sigs.contains_key("victim"),
            "a failed session must not enter the freshness skip-set (B1)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression (B2): an FTS-fresh sub-agent whose *catalog* is stale (e.g. first run after an
    /// event-schema upgrade, or a catalog wipe) takes the catalog-only pass — which must never
    /// delete its tantivy docs. The pre-fix path issued the delete-by-id BEFORE the fresh check,
    /// so the agent's docs were dropped and never re-added: a window of silently missing results.
    #[test]
    fn fts_fresh_events_stale_subagent_keeps_its_docs() {
        let h = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let pp = write_claude(&sdir, "parent2", "orchestrator planning the work");
        write_subagent(&pp, "kid2", "zqkeepme lives only in the sub-agent");
        let parent = sref("parent2", "parent session", pp);

        // Fold the forest: sub-agent indexed AND its events stamped in the catalog.
        let folded = index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(folded, 1);
        assert_eq!(text_search(&dir, "zqkeepme", 10).unwrap().len(), 1);

        // Wipe the catalog (db + WAL sidecars): the agent is now FTS-fresh but events-stale — the
        // exact combination that routes through the catalog-only pass.
        std::fs::remove_file(h.home.join("catalog.db")).unwrap();
        std::fs::remove_file(h.home.join("catalog.db-wal")).ok();
        std::fs::remove_file(h.home.join("catalog.db-shm")).ok();

        // Drive the production forest path directly (what index_all's --subagents sweep calls).
        let (index, f) = open_or_create(&dir).unwrap();
        let mut writer: IndexWriter = index.writer(50_000_000).unwrap();
        let (existing, _) = read_indexed_sigs(&index, &f).unwrap();
        let event_sync = cv_core::events::SyncTable::load();
        let offset_sync = cv_core::offsets::SyncTable::load();
        let subs = cv_core::subagent_tree_of(&parent);
        assert_eq!(subs.len(), 1, "fixture must discover exactly one sub-agent");
        let docs = index_one_subagent(&mut writer, &f, &parent, &subs[0], &existing, &event_sync, &offset_sync)
            .expect("catalog-only pass");
        assert_eq!(docs, 0, "catalog-only pass must not rewrite tantivy docs");
        writer.commit().unwrap();

        assert_eq!(
            text_search(&dir, "zqkeepme", 10).unwrap().len(),
            1,
            "an FTS-fresh, events-stale sub-agent must keep its docs (B2)"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// Regression (B3): a plain (no `--subagents`) refresh after `cv index --subagents` must NOT
    /// reap the folded forest. Only a --subagents sweep walks the forest, so only it can tell a
    /// vanished agent transcript from a merely-unvisited one.
    #[test]
    fn plain_reindex_keeps_the_folded_subagent_forest() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        let pp = write_claude(&sdir, "parent3", "orchestrator text here");
        write_subagent(&pp, "kid3", "zqforest lives only in the sub-agent");
        let parent = sref("parent3", "parent session", pp);

        index_refs_with_subagents(&dir, std::slice::from_ref(&parent), false).unwrap();
        assert_eq!(text_search(&dir, "zqforest", 10).unwrap().len(), 1);

        // A plain incremental refresh over the same top-level refs: the parent is fresh-skipped
        // and the deliberately-folded forest must survive the reap.
        let changed = index_refs_incremental(&dir, std::slice::from_ref(&parent)).unwrap();
        assert_eq!(changed, 0, "unchanged parent must be skipped");
        assert_eq!(
            text_search(&dir, "zqforest", 10).unwrap().len(),
            1,
            "a plain refresh must not silently reap the folded sub-agent forest (B3)"
        );
        assert_eq!(text_search(&dir, "orchestrator", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    /// The forest fold also tees sub-agent events into the catalog WITH provenance: a file the
    /// sub-agent edited surfaces in `cv touched`, attributed to the agent + its parent.
    #[test]
    fn subagent_events_ride_along_with_provenance() {
        let h = IsolatedHome::new();
        let dir = tmpdir();

        let pp = write_claude_msgs(&h.home, "evt-parent", &[("user", "spawn an agent to touch a file")]);
        // A sub-agent whose assistant turn edits a file.
        let stem = Path::new(&pp).file_stem().unwrap().to_str().unwrap().to_string();
        let sub_dir = Path::new(&pp).parent().unwrap().join(&stem).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_path = sub_dir.join("agent-ed1.jsonl");
        // Absolute target so the catalog query is unambiguous (the agent transcript carries no cwd
        // of its own, so a relative path wouldn't absolutize the same way the query side does).
        let line = serde_json::json!({
            "type": "assistant", "uuid": "agent-ed1", "sessionId": "agent-ed1",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Edit",
                 "input": {"file_path": "/repo/src/only_subagent.rs", "old_string": "a", "new_string": "b"}}
            ]}
        });
        std::fs::write(&sub_path, format!("{line}\n")).unwrap();

        let mut parent = sref("evt-parent", "parent", pp);
        parent.cwd = Some("/repo".into());

        index_refs_with_subagents(&dir, std::slice::from_ref(&parent), true).unwrap();

        // The sub-agent's edit is in the catalog, attributed to it + its parent.
        let touched = cv_core::events::sessions_touching("/repo/src/only_subagent.rs", true);
        assert_eq!(touched.len(), 1, "sub-agent edit must be catalogued");
        let t = &touched[0];
        assert_eq!(t.session_id, "agent-ed1");
        assert_eq!(t.parent_id.as_deref(), Some("evt-parent"));
        assert_eq!(t.agent_id.as_deref(), Some("ed1"));

        // And the per-session events carry it too.
        let rows = cv_core::events::events_for("claude", "agent-ed1", None);
        let edit = rows.iter().find(|e| e.kind == "file_edit").expect("edit event");
        assert_eq!(edit.parent_id.as_deref(), Some("evt-parent"));
        assert_eq!(edit.agent_id.as_deref(), Some("ed1"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The preview rides only the session's *first* chunk doc, but a hit can score on a *later*
    /// chunk — the fallback must still find the preview via the by-id lookup. Also caps length.
    #[test]
    fn preview_fallback_found_even_when_hit_is_a_late_chunk() {
        let _home = IsolatedHome::new();
        let dir = tmpdir();
        let sdir = tmpdir();
        // Marker only in the trailing chunk, so the sole matching doc has no preview field.
        let mut body = "padding ".repeat(700_000);
        assert!(body.len() > CHUNK_BYTES, "fixture must exceed one chunk");
        body.push_str("zqxmarker");
        let p = write_claude(&sdir, "bigone", &body);
        index_refs(&dir, &[sref("bigone", "huge doomed session", p.clone())], false).unwrap();

        std::fs::remove_file(&p).unwrap();

        let hits = text_search(&dir, "zqxmarker", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "bigone");
        assert!(
            hits[0].snippet.starts_with("padding padding"),
            "preview from the first chunk doc expected: {:?}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.ends_with("(source file moved)"));
        // The stored preview itself is capped (marker text rides on top of it).
        let stored = hits[0].snippet.trim_end_matches(" (source file moved)").chars().count();
        assert!(stored <= PREVIEW_CHARS, "preview over cap: {stored} chars");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }
}
