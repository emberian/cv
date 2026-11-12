//! `cv watch` / `cv archive` — durable session custody over multi-record Zstandard-Seekable
//! `packs/` and `StateStore` (`state.db`).
//!
//! Layout under `$CLUSTERVISION_HOME` (default `~/.clustervision`):
//! ```text
//!   packs/<pack_id>.pack.zst       sealed multi-record seekable zstd frames + XXH64 seek table
//!   packs/<pack_id>.pack.idx.json  companion FrameIndex + RecordIndex + CodecCheckpointIndex
//!   state.db                       transactional capture_inbox -> pack_manifests custody ledger
//! ```
//!
//! Each sync pass or watch poll batches every new/changed session into **one** sealed `.pack.zst`
//! archive (instead of creating one inode per session), verifies publish-before-reference by
//! sealing `.pack.zst` + `.pack.idx.json` on disk *before* committing the `pack_manifests` range
//! that drains `capture_inbox`, and deduplicates unchanged sessions by
//! `(origin_producer, harness, session_id)` + source stat + canonical IR BLAKE3 digest.
//!
//! `cv archive ls` and `cv archive path` open `state.db` strictly read-only (`list_pack_manifests`)
//! and succeed under `--frozen` without mutating disk even on a cold/absent `$CLUSTERVISION_HOME`.

use crate::util::{fmt_local, parse_harness, poll_interval, short_id};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::packs::{
    pack_idx_path, pack_zst_path, CodecCheckpoint, PackBuilder, PackManifest as PackFileManifest, PayloadAddr,
};
use cv_core::publish::Claim;
use cv_core::state::{AppendRequest, PackManifest as StatePackManifest, StateStore};
use cv_core::{Harness, SessionRef};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-session custody metadata persisted inside `pack_manifests.index_meta["sessions"]` and
/// backed by the companion `.pack.idx.json` `RecordIndex`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchivedSessionEntry {
    pub producer_id: String,
    pub harness: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub archived_at: DateTime<Utc>,
    /// Hex BLAKE3 digest of the serialized Session IR JSON stored in `addr`.
    pub content_blake3: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mtime_ns: Option<i128>,
    pub addr: PayloadAddr,
}

/// Summary of one batched `sync_refs` pass.
#[derive(Debug, Clone, Default)]
pub struct SyncSummary {
    pub archived: usize,
    pub skipped: usize,
    pub failed: Vec<(String, String)>,
    pub pack_id: Option<String>,
    pub archived_entries: Vec<ArchivedSessionEntry>,
}

/// Handle to `$CLUSTERVISION_HOME` (`packs/` + `state.db`).
pub struct Archive {
    pub home: PathBuf,
}

pub type ArchiveSessionKey = (String, String, String);
pub type ArchiveSessionMap = BTreeMap<ArchiveSessionKey, ArchivedSessionEntry>;

impl Archive {
    /// Resolve archive home: explicit `--dir` > `$CLUSTERVISION_HOME` > `~/.clustervision`.
    /// Pure path resolution: creates nothing on disk.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = match explicit {
            Some(p) => p,
            None => match cv_core::env::Env::current().cv_home() {
                Some(p) => p,
                None => bail!("cannot locate home directory (set $CLUSTERVISION_HOME or pass --dir)"),
            },
        };
        Ok(Self { home })
    }

    pub fn packs_dir(&self) -> PathBuf {
        self.home.join("packs")
    }

    pub fn state_store(&self) -> StateStore {
        StateStore::at_dir(&self.home)
    }

    /// Read-only deduplicated map of `(producer_id, harness, id) -> ArchivedSessionEntry` across
    /// all published `pack_manifests` (ascending commit order so newer packs supersede older ones).
    /// Also validates any present `.pack.idx.json` record spans. Safe under `--frozen`.
    pub fn entries_by_key(&self) -> Result<(ArchiveSessionMap, usize)> {
        let manifests =
            cv_core::list_pack_manifests_frozen_safe(&self.home).context("reading pack manifests from state.db")?;
        let packs_dir = self.packs_dir();
        let mut distinct_packs = std::collections::BTreeSet::new();
        let mut idx_cache: std::collections::HashMap<String, Option<PackFileManifest>> =
            std::collections::HashMap::new();
        let mut by_key = BTreeMap::new();

        for m in manifests {
            distinct_packs.insert(m.pack_path.clone());
            let phys_pack_id = m
                .index_meta
                .get("physical_pack_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| m.pack_id.split_once("--r").map(|(base, _)| base))
                .unwrap_or(&m.pack_id)
                .to_string();

            if let Some(sessions_val) = m.index_meta.get("sessions") {
                let entries: Vec<ArchivedSessionEntry> = serde_json::from_value(sessions_val.clone())
                    .with_context(|| format!("decoding index_meta.sessions for pack {}", m.pack_id))?;
                for entry in entries {
                    let target_pack_id = if !entry.addr.pack_id.is_empty() {
                        entry.addr.pack_id.as_str()
                    } else {
                        phys_pack_id.as_str()
                    };
                    if !idx_cache.contains_key(target_pack_id) {
                        let idx_file = pack_idx_path(&packs_dir, target_pack_id)?;
                        let loaded = if idx_file.is_file() {
                            let raw =
                                std::fs::read(&idx_file).with_context(|| format!("reading {}", idx_file.display()))?;
                            Some(PackFileManifest::from_json_slice(&raw)?)
                        } else {
                            None
                        };
                        idx_cache.insert(target_pack_id.to_string(), loaded);
                    }
                    if let Some(Some(idx)) = idx_cache.get(target_pack_id) {
                        let anchor = record_anchor(&entry.producer_id, &entry.harness, &entry.id);
                        if let Some(span) = idx.record_index.get(&anchor) {
                            anyhow::ensure!(
                                span.decoded_offset == entry.addr.decoded_offset
                                    && span.length == entry.addr.length
                                    && span.blake3_digest == entry.addr.blake3_digest,
                                "pack {} record span mismatch for {}",
                                target_pack_id,
                                anchor
                            );
                        }
                    }
                    let key = (entry.producer_id.clone(), entry.harness.clone(), entry.id.clone());
                    by_key.insert(key, entry);
                }
            }
        }
        Ok((by_key, distinct_packs.len()))
    }

    /// Deduplicated archived sessions, newest `archived_at` first. Read-only (`--frozen` safe).
    pub fn entries(&self) -> Result<(Vec<ArchivedSessionEntry>, usize)> {
        let (by_key, pack_count) = self.entries_by_key()?;
        let mut rows: Vec<ArchivedSessionEntry> = by_key.into_values().collect();
        rows.sort_by(|a, b| {
            b.archived_at
                .cmp(&a.archived_at)
                .then_with(|| a.harness.cmp(&b.harness))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok((rows, pack_count))
    }

    /// Hydrate the raw Session IR JSON bytes of `(harness, id)` from its sealed `.pack.zst`,
    /// verifying both Zstd-Seekable XXH64 frame checksums and the record's 32-byte BLAKE3 digest.
    #[cfg(test)]
    pub fn read_session_bytes(&self, harness: &str, id: &str) -> Result<Option<Vec<u8>>> {
        use cv_core::packs::SealedPack;
        let (by_key, _) = self.entries_by_key()?;
        let found = by_key
            .values()
            .filter(|e| e.harness == harness && e.id == id)
            .max_by_key(|e| e.archived_at);
        let Some(entry) = found else {
            return Ok(None);
        };
        let pack = SealedPack::open(&self.packs_dir(), &entry.addr.pack_id)?;
        let bytes = pack.read_payload(&entry.addr)?;
        Ok(Some(bytes))
    }

    /// Batch-archive `refs`: unchanged sessions are skipped; all new or modified sessions in this
    /// pass are packed together into **one** sealed `.pack.zst` + `.pack.idx.json`, recorded in
    /// `capture_inbox`, and published atomically into `pack_manifests` (`publish-before-reference`).
    pub fn sync_refs(&self, refs: &[SessionRef]) -> Result<SyncSummary> {
        if cv_core::env::frozen() {
            bail!("--frozen forbids writing archive packs");
        }

        let store = self.state_store();
        let producer_id = store.default_producer().to_string();
        let (known, _) = self.entries_by_key()?;

        struct StagedSession {
            harness: String,
            id: String,
            title: Option<String>,
            cwd: Option<String>,
            created_at: Option<DateTime<Utc>>,
            updated_at: Option<DateTime<Utc>>,
            message_count: usize,
            source_len: Option<u64>,
            source_mtime_ns: Option<i128>,
            payload: Vec<u8>,
            blake3_hex: String,
        }

        let mut summary = SyncSummary::default();
        let mut staged: Vec<StagedSession> = Vec::new();
        // Deduplicate duplicate refs within the same input slice as well.
        let mut seen_in_pass = std::collections::HashSet::new();

        for r in refs {
            let harness_str = r.harness.as_str().to_string();
            let key = (producer_id.clone(), harness_str.clone(), r.id.clone());
            if !seen_in_pass.insert(key.clone()) {
                summary.skipped += 1;
                continue;
            }

            let (source_len, source_mtime_ns) = source_stat(&r.path);
            let single_file_source = is_single_session_file(&r.path);

            if let Some(prev) = known.get(&key) {
                let stat_unchanged = single_file_source
                    && source_len.is_some()
                    && source_mtime_ns.is_some()
                    && prev.source_len == source_len
                    && prev.source_mtime_ns == source_mtime_ns;
                let meta_unchanged = r.updated_at.is_some()
                    && prev.updated_at == r.updated_at
                    && r.message_count.is_some_and(|mc| mc == prev.message_count)
                    && (!single_file_source
                        || (prev.source_len == source_len && prev.source_mtime_ns == source_mtime_ns));
                if stat_unchanged || meta_unchanged {
                    summary.skipped += 1;
                    continue;
                }
            }

            let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
                summary
                    .failed
                    .push((format!("{}:{}", r.harness, r.id), "no adapter registered".into()));
                continue;
            };

            let rendered = (|| -> Result<(cv_core::Session, usize, Vec<u8>)> {
                let mut sink = cv_core::api::WindowSink::new(&r.path, 0, None);
                let pass = cv_core::stream::Pass {
                    guard: true,
                    ..cv_core::stream::Pass::RECORD
                };
                let read = cv_core::stream::stream_counted(
                    adapter.as_ref(),
                    r,
                    &cv_core::Demand::RICH.with_spans(),
                    &mut sink,
                    pass,
                )?;
                let frames = sink.finish()?;
                let total = sink.selected;
                let mut buf = Vec::new();
                super::session_json::write_session(&mut buf, &read.session, &frames, None, None, total)?;
                buf.push(b'\n');
                if let Some(guard) = read.guard {
                    guard.verify(r)?;
                }
                Ok((read.session, total, buf))
            })();

            let (session_meta, message_count, payload) = match rendered {
                Ok(v) => v,
                Err(e) => {
                    summary.failed.push((format!("{}:{}", r.harness, r.id), e.to_string()));
                    continue;
                }
            };
            let blake3_hex = cv_core::blake3_hex(&payload);

            if let Some(prev) = known.get(&key) {
                if prev.content_blake3 == blake3_hex {
                    summary.skipped += 1;
                    continue;
                }
            }

            staged.push(StagedSession {
                harness: harness_str,
                id: r.id.clone(),
                title: session_meta.title.or_else(|| r.title.clone()),
                cwd: session_meta
                    .cwd
                    .or_else(|| r.cwd.clone())
                    .map(|p| p.to_string_lossy().into_owned()),
                created_at: session_meta.created_at.or(r.created_at),
                updated_at: session_meta.updated_at.or(r.updated_at),
                message_count,
                source_len,
                source_mtime_ns,
                payload,
                blake3_hex,
            });
        }

        if staged.is_empty() {
            return Ok(summary);
        }

        let pack_id = format!("pack-{}", uuid::Uuid::now_v7().simple());
        let packs_dir = self.packs_dir();
        let archived_at = Utc::now();

        // 1. Build and atomically seal `<packs>/<pack_id>.pack.{zst,idx.json}` FIRST
        //    (`publish-before-reference`: immutable pack bytes land before SQLite references them).
        let mut builder = PackBuilder::create(&packs_dir, &pack_id)?;
        let mut entries = Vec::with_capacity(staged.len());

        for item in &staged {
            let anchor = record_anchor(&producer_id, &item.harness, &item.id);
            let addr = builder.append_record(&anchor, &item.payload)?;
            builder.record_checkpoint(CodecCheckpoint {
                anchor: anchor.clone(),
                decoded_offset: addr.decoded_offset + addr.length,
                codec_version: 1,
                settled: true,
                dependencies: Vec::new(),
                continuation_state: json!({
                    "message_count": item.message_count,
                    "updated_at": item.updated_at,
                }),
            })?;
            entries.push(ArchivedSessionEntry {
                producer_id: producer_id.clone(),
                harness: item.harness.clone(),
                id: item.id.clone(),
                title: item.title.clone(),
                cwd: item.cwd.clone(),
                created_at: item.created_at,
                updated_at: item.updated_at,
                message_count: item.message_count,
                archived_at,
                content_blake3: item.blake3_hex.clone(),
                source_len: item.source_len,
                source_mtime_ns: item.source_mtime_ns,
                addr,
            });
        }

        let pack_file_manifest = builder.seal(Claim::New)?;
        let zst_path = pack_zst_path(&packs_dir, &pack_id)?;
        let zst_bytes =
            std::fs::read(&zst_path).with_context(|| format!("reading sealed pack {}", zst_path.display()))?;
        let byte_len = zst_bytes.len() as u64;
        let pack_blake3 = cv_core::blake3_hex(&zst_bytes);
        let rel_pack_path = format!("packs/{pack_id}.pack.zst");

        // 2. Append capture envelopes to `StateStore` (`capture_inbox`) with pack-scoped `op_id`s
        //    (preventing retry wedges if an earlier pack failed before publishing) and record each
        //    archived session's exact `local_commit_pos`.
        let mut committed = Vec::with_capacity(entries.len());
        for (item, entry) in staged.iter().zip(entries.iter()) {
            let anchor = record_anchor(&producer_id, &item.harness, &item.id);
            let op_id = format!("archive:{pack_id}:{producer_id}:{}:{}", item.harness, item.id);
            let envelope = json!({
                "kind": "session_archive",
                "producer_id": producer_id,
                "harness": item.harness,
                "session_id": item.id,
                "message_count": item.message_count,
                "content_blake3": item.blake3_hex,
                "pack_id": pack_id,
                "decoded_offset": entry.addr.decoded_offset,
                "length": entry.addr.length,
            });
            let req = AppendRequest::new(op_id, item.blake3_hex.clone(), envelope)
                .with_producer(producer_id.clone())
                .with_payload_ref(format!("{rel_pack_path}#{anchor}"));
            let receipt = store
                .append_request(&req)
                .context("appending archive capture envelope to StateStore")?;
            committed.push((receipt.local_commit_pos, entry.clone()));
        }

        // 3. Publish maximal contiguous `local_commit_pos` sub-runs so any concurrent non-archive
        //    inbox row (`cv hook receipt` / `cv hook lane`) interleaved between archived sessions
        //    is never deleted from `capture_inbox`.
        publish_contiguous_archive_runs(
            &store,
            &pack_id,
            &rel_pack_path,
            byte_len,
            &pack_blake3,
            &pack_file_manifest,
            archived_at.timestamp_millis(),
            committed,
        )?;

        summary.archived = entries.len();
        summary.pack_id = Some(pack_id);
        summary.archived_entries = entries;
        Ok(summary)
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_contiguous_archive_runs(
    store: &StateStore,
    pack_id: &str,
    rel_pack_path: &str,
    byte_len: u64,
    pack_blake3: &str,
    pack_file_manifest: &PackFileManifest,
    published_at_ms: i64,
    mut committed: Vec<(i64, ArchivedSessionEntry)>,
) -> Result<()> {
    if committed.is_empty() {
        return Ok(());
    }
    committed.sort_by_key(|(pos, _)| *pos);
    committed.dedup_by_key(|(pos, _)| *pos);

    let mut runs: Vec<Vec<(i64, ArchivedSessionEntry)>> = Vec::new();
    for (pos, entry) in committed {
        if let Some(last_run) = runs.last_mut() {
            if let Some((prev_pos, _)) = last_run.last() {
                if pos == *prev_pos + 1 {
                    last_run.push((pos, entry));
                    continue;
                }
            }
        }
        runs.push(vec![(pos, entry)]);
    }

    for (run_idx, run) in runs.into_iter().enumerate() {
        let min_pos = run.first().expect("non-empty run").0;
        let max_pos = run.last().expect("non-empty run").0;
        let record_count = run.len() as u64;
        debug_assert_eq!((max_pos - min_pos + 1) as u64, record_count);
        let run_entries: Vec<ArchivedSessionEntry> = run.into_iter().map(|(_, e)| e).collect();
        let manifest_pack_id = if run_idx == 0 {
            pack_id.to_string()
        } else {
            format!("{pack_id}--r{run_idx}")
        };
        let state_manifest = StatePackManifest {
            pack_id: manifest_pack_id,
            min_commit_pos: min_pos,
            max_commit_pos: max_pos,
            record_count,
            byte_len,
            digest: format!("blake3:{pack_blake3}"),
            pack_path: rel_pack_path.to_string(),
            index_meta: json!({
                "physical_pack_id": pack_id,
                "pack_version": pack_file_manifest.version,
                "frame_count": pack_file_manifest.frame_count,
                "total_decoded_len": pack_file_manifest.total_decoded_len,
                "sessions": run_entries,
            }),
            published_at: published_at_ms,
        };
        store
            .publish_pack_range(&state_manifest)
            .context("publishing pack manifest range in StateStore")?;
    }
    Ok(())
}

fn record_anchor(producer_id: &str, harness: &str, id: &str) -> String {
    format!("{producer_id}:{harness}:{id}")
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() && max_chars > 1 {
        let mut trimmed: String = s.chars().take(max_chars - 1).collect();
        trimmed.push('…');
        trimmed
    } else {
        head
    }
}

fn is_single_session_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !matches!(ext.as_str(), "db" | "sqlite" | "sqlite3" | "vscdb")
}

fn source_stat(path: &Path) -> (Option<u64>, Option<i128>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    if !meta.is_file() {
        return (None, None);
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128);
    (Some(meta.len()), mtime_ns)
}

fn discover_filtered(harness: Option<Harness>, cwd_sub: Option<&str>) -> Vec<SessionRef> {
    cv_core::discover_all()
        .into_iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h))
        .filter(|r| match cwd_sub {
            None => true,
            Some(sub) => r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(sub))
                .unwrap_or(false),
        })
        .collect()
}

pub fn cmd_sync(archive: &Archive) -> Result<()> {
    let refs = discover_filtered(None, None);
    let total = refs.len();
    let summary = archive.sync_refs(&refs)?;
    for (who, why) in &summary.failed {
        eprintln!("  ! {who}: {why}");
    }
    let failed = summary.failed.len();
    let pack_note = summary
        .pack_id
        .as_deref()
        .map(|id| format!(" · pack {id}"))
        .unwrap_or_default();
    println!(
        "✦ {} archived, {} skipped (unchanged){} of {total} discovered → {}{pack_note}",
        summary.archived,
        summary.skipped,
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        },
        archive.home.display(),
    );
    Ok(())
}

pub fn cmd_watch(
    archive: &Archive,
    interval_secs: f64,
    harness_filter: Option<String>,
    cwd_filter: Option<String>,
) -> Result<()> {
    let want = parse_harness(&harness_filter)?;
    let poll = poll_interval(interval_secs)?;

    // Announce immediately to stderr BEFORE initial discovery/sync (`docs/INTERFACE-V2.md` §2:
    // daemons announce on stderr within 1s so piped callers know what is running).
    eprintln!(
        "cv watch: watching (interval {}s{}{}) -> {}",
        poll.as_secs_f64(),
        want.map(|h| format!(" · harness={h}")).unwrap_or_default(),
        cwd_filter
            .as_deref()
            .map(|c| format!(" · cwd~{c:?}"))
            .unwrap_or_default(),
        archive.home.display(),
    );

    let _recorder = super::start_recorder()?;
    // Initial sweep batches all existing sessions into a single sealed pack.
    let initial_refs = discover_filtered(want, cwd_filter.as_deref());
    let init = archive.sync_refs(&initial_refs)?;
    for (who, why) in &init.failed {
        eprintln!("  ! {who}: {why}");
    }
    eprintln!(
        "  initial sync: {} archived, {} already current",
        init.archived, init.skipped
    );

    // Track last-seen `(len, mtime)` per source file so unchanged files aren't even touched
    // between polls unless discovery surfaces new/updated refs.
    let mut seen_stat: std::collections::HashMap<PathBuf, (Option<u64>, Option<SystemTime>)> =
        std::collections::HashMap::new();
    for r in &initial_refs {
        if let Ok(m) = std::fs::metadata(&r.path) {
            seen_stat.insert(r.path.clone(), (Some(m.len()), m.modified().ok()));
        }
    }

    loop {
        std::thread::sleep(poll);
        let refs = discover_filtered(want, cwd_filter.as_deref());
        let mut candidates = Vec::new();
        for r in refs {
            let cur = std::fs::metadata(&r.path)
                .ok()
                .map(|m| (Some(m.len()), m.modified().ok()));
            if is_single_session_file(&r.path) && cur.is_some() && seen_stat.get(&r.path) == cur.as_ref() {
                continue;
            }
            if let Some(c) = cur {
                seen_stat.insert(r.path.clone(), c);
            }
            candidates.push(r);
        }
        if candidates.is_empty() {
            continue;
        }
        let batch = archive.sync_refs(&candidates)?;
        for (who, why) in &batch.failed {
            eprintln!("  ! {who}: {why}");
        }
        for entry in &batch.archived_entries {
            let now = fmt_local(Utc::now(), "%H:%M:%S");
            let title = entry.title.as_deref().map(|t| truncate_str(t, 44)).unwrap_or_default();
            println!(
                "  [{now}] {:8} {}  ({:>4} msgs)  {title}",
                entry.harness,
                short_id(&entry.id),
                entry.message_count,
            );
        }
    }
}

pub fn cmd_ls(archive: &Archive) -> Result<()> {
    let (entries, pack_count) = archive.entries()?;
    if entries.is_empty() {
        println!(
            "archive empty ({}) — run 'cv archive sync' or 'cv watch'",
            archive.home.display()
        );
        return Ok(());
    }
    println!(
        "{:<16} {:<8} {:<10} {:>6}   {:<24} TITLE",
        "ARCHIVED", "HARNESS", "ID", "MSGS", "PACK"
    );
    println!("{}", "─".repeat(96));
    for e in &entries {
        let dt = fmt_local(e.archived_at, "%Y-%m-%d %H:%M");
        let title = e.title.as_deref().map(|t| truncate_str(t, 42)).unwrap_or_default();
        let pack_short = truncate_str(&e.addr.pack_id, 24);
        println!(
            "{:<16} {:<8} {:<10} {:>4} msgs {:<24} {}",
            dt,
            e.harness,
            short_id(&e.id),
            e.message_count,
            pack_short,
            title,
        );
    }
    println!(
        "\n{} session(s) across {} pack(s) in {}",
        entries.len(),
        pack_count,
        archive.packs_dir().display()
    );
    Ok(())
}

pub fn cmd_path(archive: &Archive) -> Result<()> {
    println!("{}", archive.home.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn packs_archive_batches_sessions_deduplicates_verifies_blake3_and_reads_frozen() {
        let world = crate::test_world("archive-packs-unit");
        let a_path = world.write_session(
            "sess-alpha",
            &[
                json!({"type":"user","uuid":"u1","sessionId":"sess-alpha","timestamp":"2026-06-01T10:00:00Z",
                       "message":{"role":"user","content":"alpha prompt"}}),
                json!({"type":"assistant","uuid":"a1","sessionId":"sess-alpha","timestamp":"2026-06-01T10:00:05Z",
                       "message":{"role":"assistant","content":"alpha answer"}}),
            ],
        );
        let b_path = world.write_session(
            "sess-beta",
            &[
                json!({"type":"user","uuid":"u2","sessionId":"sess-beta","timestamp":"2026-06-01T11:00:00Z",
                       "message":{"role":"user","content":"beta prompt"}}),
            ],
        );

        let archive = Archive {
            home: world.cv_home.clone(),
        };
        let refs = vec![
            SessionRef::new("sess-alpha", Harness::Claude, &a_path),
            SessionRef::new("sess-beta", Harness::Claude, &b_path),
        ];

        // 1. First sync packs BOTH sessions into ONE sealed `.pack.zst` and drains `capture_inbox`.
        let first = archive.sync_refs(&refs).unwrap();
        assert_eq!(first.archived, 2);
        assert_eq!(first.skipped, 0);
        assert!(first.pack_id.is_some());
        assert!(!world.cv_home.join("archive").exists(), "legacy archive/ dir is gone");
        assert!(
            !world.cv_home.join("catalog.jsonl").exists(),
            "legacy catalog.jsonl is gone"
        );

        let store = archive.state_store();
        assert_eq!(
            store.inbox_count().unwrap(),
            0,
            "capture_inbox drained into pack_manifests"
        );
        let manifests = store.list_pack_manifests().unwrap();
        assert_eq!(manifests.len(), 1, "single sync pass produces exactly ONE pack");
        assert_eq!(manifests[0].record_count, 2);

        // 2. Second sync with unchanged files writes zero packs and zero inbox rows.
        let second = archive.sync_refs(&refs).unwrap();
        assert_eq!(second.archived, 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

        // 3. Touching mtime without changing content ALSO deduplicates via BLAKE3 content digest!
        let future = SystemTime::now() + Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&a_path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        let third = archive.sync_refs(&refs).unwrap();
        assert_eq!(
            third.archived, 0,
            "identical IR bytes must deduplicate even if mtime moved"
        );
        assert_eq!(third.skipped, 2);
        assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

        // 4. Hydrate payload from sealed pack (verifying BLAKE3 + XXH64) and check content.
        let alpha_bytes = archive
            .read_session_bytes("claude", "sess-alpha")
            .unwrap()
            .expect("alpha present");
        let alpha_ir: serde_json::Value = serde_json::from_slice(&alpha_bytes).unwrap();
        assert_eq!(alpha_ir["id"], "sess-alpha");
        assert_eq!(alpha_ir["messages"].as_array().unwrap().len(), 2);

        // 5. `--frozen` allows `entries()`, `cmd_ls`, and `cmd_path` while refusing `sync_refs`.
        {
            let _frozen = cv_core::env::Env::from_vars([
                ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
                ("CV_FROZEN", std::ffi::OsStr::new("1")),
            ])
            .enter();
            let (rows, packs) = archive.entries().unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(packs, 1);
            cmd_ls(&archive).unwrap();
            cmd_path(&archive).unwrap();
            assert!(archive.sync_refs(&refs).is_err(), "--frozen must refuse archive sync");
        }
    }

    #[test]
    fn interleaved_non_archive_inbox_rows_survive_contiguous_pack_publish_and_retry() {
        let world = crate::test_world("archive-interleaved-inbox");
        let archive = Archive {
            home: world.cv_home.clone(),
        };
        let store = archive.state_store();
        let producer = store.default_producer().to_string();

        // Build one physical sealed pack containing two session records (`s-one` and `s-two`).
        let packs_dir = archive.packs_dir();
        let pack_id = "pack-interleaved-test";
        let mut builder = PackBuilder::create(&packs_dir, pack_id).unwrap();
        let payload_1 = br#"{"id":"s-one","harness":"claude","messages":[]}"#;
        let payload_2 = br#"{"id":"s-two","harness":"claude","messages":[]}"#;
        let addr_1 = builder
            .append_record(record_anchor(&producer, "claude", "s-one"), payload_1)
            .unwrap();
        let addr_2 = builder
            .append_record(record_anchor(&producer, "claude", "s-two"), payload_2)
            .unwrap();
        let pfm = builder.seal(Claim::New).unwrap();
        let zst_bytes = std::fs::read(pack_zst_path(&packs_dir, pack_id).unwrap()).unwrap();
        let rel_pack_path = format!("packs/{pack_id}.pack.zst");
        let pack_blake3 = cv_core::blake3_hex(&zst_bytes);
        let now = Utc::now();

        // Simulate an earlier failed/aborted pack (`pack-old`) that left `s-one` at `pos = 1`,
        // followed by a published pack at `pos = 2`, then our new sync pass appending `s-one`
        // (`pos = 3`), a concurrent `cv hook receipt` interleaving at `pos = 4`, and `s-two`
        // appending at `pos = 5`.
        let stale_req = AppendRequest::new(
            format!("archive:pack-old:{producer}:claude:s-one"),
            cv_core::blake3_hex(payload_1),
            json!({"kind": "session_archive", "session_id": "s-one"}),
        );
        let pos_stale = store.append_request(&stale_req).unwrap().local_commit_pos;
        assert_eq!(pos_stale, 1);

        let mid_req = AppendRequest::new(
            format!("archive:pack-mid:{producer}:claude:s-mid"),
            "deadbeef",
            json!({"kind": "session_archive", "session_id": "s-mid"}),
        );
        let pos_mid = store.append_request(&mid_req).unwrap().local_commit_pos;
        assert_eq!(pos_mid, 2);
        store
            .publish_pack_range(&StatePackManifest {
                pack_id: "pack-mid".into(),
                min_commit_pos: 2,
                max_commit_pos: 2,
                record_count: 1,
                byte_len: 1,
                digest: "blake3:00".into(),
                pack_path: "packs/pack-mid.pack.zst".into(),
                index_meta: json!({"sessions": []}),
                published_at: now.timestamp_millis(),
            })
            .unwrap();

        // Pack-scoped `op_id` gets a fresh `local_commit_pos = 3` (no retry wedge across `pos = 2`!).
        let r1 = store
            .append_request(&AppendRequest::new(
                format!("archive:{pack_id}:{producer}:claude:s-one"),
                cv_core::blake3_hex(payload_1),
                json!({"kind": "session_archive", "session_id": "s-one"}),
            ))
            .unwrap();
        assert_eq!(r1.local_commit_pos, 3);

        // Concurrent `cv hook receipt` lands right in the middle at `pos = 4`!
        let hook_rec = store
            .append(
                "receipt:claude:live-sess:1",
                "hash-1",
                &json!({"kind": "tool_receipt", "session_id": "live-sess", "seq": 1}),
            )
            .unwrap();
        assert_eq!(hook_rec.local_commit_pos, 4);

        let r2 = store
            .append_request(&AppendRequest::new(
                format!("archive:{pack_id}:{producer}:claude:s-two"),
                cv_core::blake3_hex(payload_2),
                json!({"kind": "session_archive", "session_id": "s-two"}),
            ))
            .unwrap();
        assert_eq!(r2.local_commit_pos, 5);

        let entry_1 = ArchivedSessionEntry {
            producer_id: producer.clone(),
            harness: "claude".into(),
            id: "s-one".into(),
            title: Some("One".into()),
            cwd: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
            archived_at: now,
            content_blake3: cv_core::blake3_hex(payload_1),
            source_len: None,
            source_mtime_ns: None,
            addr: addr_1,
        };
        let entry_2 = ArchivedSessionEntry {
            id: "s-two".into(),
            title: Some("Two".into()),
            content_blake3: cv_core::blake3_hex(payload_2),
            addr: addr_2,
            ..entry_1.clone()
        };

        publish_contiguous_archive_runs(
            &store,
            pack_id,
            &rel_pack_path,
            zst_bytes.len() as u64,
            &pack_blake3,
            &pfm,
            now.timestamp_millis(),
            vec![(r1.local_commit_pos, entry_1), (r2.local_commit_pos, entry_2)],
        )
        .unwrap();

        // Interleaved `pos = 4` (`tool_receipt`) and unreferenced `pos = 1` MUST remain in `capture_inbox`,
        // while `pos = 3` and `pos = 5` were cleanly drained into `pack_manifests`!
        let remaining = store.read_inbox_range(1, 10).unwrap();
        let remaining_pos: Vec<i64> = remaining.iter().map(|e| e.local_commit_pos).collect();
        assert_eq!(remaining_pos, vec![1, 4]);
        assert_eq!(remaining[1].op_id, "receipt:claude:live-sess:1");

        // `entries()` reads both sub-runs (`pack_id` and `pack_id--r1`), validates `.pack.idx.json`,
        // and counts distinct physical `pack_path`s (`pack-mid` + `pack-interleaved-test` = 2).
        let (rows, pack_count) = archive.entries().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(pack_count, 2);
        assert!(archive.read_session_bytes("claude", "s-one").unwrap().is_some());
        assert!(archive.read_session_bytes("claude", "s-two").unwrap().is_some());
    }
}
