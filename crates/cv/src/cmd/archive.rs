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
use cv_core::custody::SourceCheckpoint;
use cv_core::packs::{pack_zst_path, CodecCheckpoint, PackBuilder, PackManifest as PackFileManifest, PayloadAddr};
use cv_core::publish::Claim;
use cv_core::state::{AppendRequest, PackManifest as StatePackManifest, StateStore};
use cv_core::{Harness, SessionRef};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Summary of one batched `sync_refs` pass.
#[derive(Debug, Clone, Default)]
pub struct SyncSummary {
    pub archived: usize,
    pub skipped: usize,
    pub failed: Vec<(String, String)>,
    pub pack_id: Option<String>,
    pub archived_entries: Vec<ArchivedSessionEntry>,
    pub verified_baselines: Vec<(String, String, cv_core::source::Baseline)>,
}

/// Handle to `$CLUSTERVISION_HOME` (`packs/` + `state.db`).
pub struct Archive {
    pub home: PathBuf,
}

use cv_core::custody::{ArchiveSessionMap, ArchivedSessionEntry};

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

    pub fn entries_by_key(&self) -> Result<(ArchiveSessionMap, usize)> {
        cv_core::custody::archive_entries(&self.home)
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

    /// Batch-archive `refs`: unchanged sessions are skipped; all new or modified sessions in this
    /// pass are packed together into **one** sealed `.pack.zst` + `.pack.idx.json`, recorded in
    /// `capture_inbox`, and published atomically into `pack_manifests` (`publish-before-reference`).
    ///
    /// A single append-only native file ([`native_file`]) is held as its own bytes: when the
    /// archived prefix still matches ([`SourceCheckpoint`]), the pass reads and packs only the
    /// complete lines appended since; a shrunk or rewritten file starts a fresh chain. Every
    /// other store is projected to Session IR JSON, as before.
    pub fn sync_refs(&self, refs: &[SessionRef]) -> Result<SyncSummary> {
        if cv_core::env::frozen() {
            bail!("--frozen forbids writing archive packs");
        }

        let store = self.state_store();
        let producer_id = store.default_producer().to_string();
        let packs_dir = self.packs_dir();
        let known = cv_core::custody::head_entries(&self.home)?;

        enum Content {
            Projection(Vec<u8>),
            Native {
                path: PathBuf,
                from: u64,
                to: u64,
                prior: Vec<PayloadAddr>,
                checkpoint: SourceCheckpoint,
                file_name: String,
                layout: Vec<String>,
                document: bool,
                file_id: Option<cv_core::source::FileId>,
            },
        }
        struct StagedSession {
            key: cv_core::SessionKey,
            harness: String,
            id: String,
            title: Option<String>,
            cwd: Option<String>,
            created_at: Option<DateTime<Utc>>,
            updated_at: Option<DateTime<Utc>>,
            message_count: usize,
            source_len: Option<u64>,
            source_mtime_ns: Option<i128>,
            baseline: Option<cv_core::source::Baseline>,
            content: Content,
        }

        let mut summary = SyncSummary::default();
        let mut staged: Vec<StagedSession> = Vec::new();
        // Deduplicate duplicate refs within the same input slice as well.
        let mut seen_in_pass = std::collections::HashSet::new();

        for chunk in refs.chunks(PROBE_BATCH) {
            let mut probe = cv_core::source::Probe::default();
            for r in chunk {
                let harness_str = r.harness.as_str().to_string();
                let key = cv_core::SessionKey::local(r);
                if !seen_in_pass.insert(key.clone()) {
                    summary.skipped += 1;
                    continue;
                }

                let primary = probe.primary_path(r);
                let (source_len, source_mtime_ns) = source_stat_ref(r, &primary);
                let prev = known.get(&key.selector());

                if let Some(prev) = prev {
                    let unchanged = if let Some(ref prev_base) = prev.baseline {
                        let now = probe.snapshot_from(r, &primary);
                        prev_base.fresh(&now, &mut probe) || prev_base.rows_fresh(r, &now, &mut probe)
                    } else {
                        is_legacy_entry_unchanged(r, &primary, source_len, source_mtime_ns, prev)
                    };
                    if unchanged {
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
                let native = native_file(r, adapter.as_ref(), &mut probe);
                let producer_key = cv_core::SessionKey {
                    producer: producer_id.clone(),
                    ..key.clone()
                };
                // A native file's delta is measured before any parse: an unchanged chain (a touch,
                // a companion's stat) costs bounded reads (full BLAKE3 for documents or same-size
                // touches, two boundary windows for appends), never a session parse.
                let delta = match &native {
                    Some(native) => match native_delta(&packs_dir, &producer_key, native, prev) {
                        Ok(Some(delta)) => Some(delta),
                        Ok(None) => {
                            summary.skipped += 1;
                            continue;
                        }
                        Err(e) => {
                            summary
                                .failed
                                .push((format!("{}:{}", r.harness, r.id), format!("{e:#}")));
                            continue;
                        }
                    },
                    None => None,
                };
                #[cfg(test)]
                run_test_hook(&AFTER_NATIVE_DELTA, &r.path);

                let rendered = (|| -> Result<Rendered> {
                    let pass = cv_core::stream::Pass {
                        guard: true,
                        ..cv_core::stream::Pass::RECORD
                    };
                    let (read, turns, payload) = if native.is_some() {
                        // Native custody keeps the bytes; the read only certifies and describes them.
                        let read = cv_core::stream::stream_counted(
                            adapter.as_ref(),
                            r,
                            &cv_core::Demand::LEAN.with_spans(),
                            &mut cv_core::stream::Discard,
                            pass,
                        )?;
                        let turns = read.counted.as_ref().map_or(0, |c| c.turns);
                        (read, turns, None)
                    } else {
                        let mut sink = cv_core::api::WindowSink::new(&r.path, 0, None);
                        let read = cv_core::stream::stream_counted(
                            adapter.as_ref(),
                            r,
                            &cv_core::Demand::COMPLETE.with_spans(),
                            &mut sink,
                            pass,
                        )?;
                        let frames = sink.finish()?;
                        let total = sink.selected;
                        let turns = total - sink.carriers;
                        let mut buf = Vec::new();
                        super::session_json::write_session(&mut buf, &read.session, &frames, None, None, total)?;
                        buf.push(b'\n');
                        (read, turns, Some(buf))
                    };
                    let baseline = match read.guard {
                        Some(guard) => Some(guard.verify_baseline(r)?),
                        None => None,
                    };
                    Ok((read.session, turns, payload, baseline))
                })();

                let (session_meta, message_count, payload, baseline) = match rendered {
                    Ok(v) => v,
                    Err(e) => {
                        summary.failed.push((format!("{}:{}", r.harness, r.id), e.to_string()));
                        continue;
                    }
                };

                let content = match (payload, native, delta) {
                    (Some(payload), _, _) => {
                        if prev.is_some_and(|p| p.content_blake3 == cv_core::blake3_hex(&payload)) {
                            if let Some(base) = baseline {
                                summary
                                    .verified_baselines
                                    .push((harness_str, r.session_selector(), base));
                            }
                            summary.skipped += 1;
                            continue;
                        }
                        Content::Projection(payload)
                    }
                    (
                        None,
                        Some(NativeFile {
                            path,
                            file_name,
                            layout,
                            document,
                        }),
                        Some(Delta {
                            from,
                            to,
                            prior,
                            checkpoint,
                            file_id,
                        }),
                    ) => Content::Native {
                        path,
                        from,
                        to,
                        prior,
                        checkpoint,
                        file_name,
                        layout,
                        document,
                        file_id,
                    },
                    _ => unreachable!("a projection read renders its payload; a native one has its delta"),
                };

                staged.push(StagedSession {
                    key,
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
                    baseline,
                    content,
                });
            }
        }

        if staged.is_empty() {
            return Ok(summary);
        }

        let pack_id = format!("pack-{}", uuid::Uuid::now_v7().simple());
        let archived_at = Utc::now();
        #[cfg(test)]
        for item in &staged {
            if let Content::Native { path, .. } = &item.content {
                run_test_hook(&BEFORE_PACK_STAGE, path);
            }
        }

        // 1. Build and atomically seal `<packs>/<pack_id>.pack.{zst,idx.json}` FIRST
        //    (`publish-before-reference`: immutable pack bytes land before SQLite references them).
        let mut builder = PackBuilder::create(&packs_dir, &pack_id)?;
        let mut entries = Vec::with_capacity(staged.len());
        let mut written = Vec::with_capacity(staged.len());

        for item in staged {
            let anchor = cv_core::custody::archive_anchor(&cv_core::SessionKey {
                producer: producer_id.clone(),
                ..item.key.clone()
            });
            let (addr, custody, entry_baseline, entry_source_len, entry_source_mtime_ns) = match &item.content {
                Content::Projection(payload) => (
                    builder.append_record(&anchor, payload)?,
                    cv_core::custody::CustodyKind::PortableProjection,
                    item.baseline.clone(),
                    item.source_len,
                    item.source_mtime_ns,
                ),
                Content::Native {
                    path,
                    from,
                    to,
                    prior,
                    checkpoint,
                    file_name,
                    layout,
                    document,
                    file_id: delta_file_id,
                } => {
                    let fail_who = format!("{}:{}", item.harness, item.id);
                    let file = match std::fs::File::open(path) {
                        Ok(f) => f,
                        Err(e) => {
                            summary
                                .failed
                                .push((fail_who, format!("opening {}: {e}", path.display())));
                            continue;
                        }
                    };
                    let file_meta = match file.metadata() {
                        Ok(m) => m,
                        Err(e) => {
                            summary.failed.push((fail_who, format!("stat {}: {e}", path.display())));
                            continue;
                        }
                    };
                    let now_file_id = cv_core::source::FileId::of(&file_meta);
                    let same_delta_inode = match (delta_file_id, now_file_id) {
                        (Some(then), Some(now)) => (then.dev, then.ino) == (now.dev, now.ino),
                        (None, None) => true,
                        _ => false,
                    };
                    if !file_meta.is_file() || file_meta.len() < *to || !same_delta_inode {
                        summary
                            .failed
                            .push((fail_who, "source rewritten during archive".into()));
                        continue;
                    }
                    // The prefix the delta extends must still be the archived one, and the
                    // captured range must still end where it was measured — checked BEFORE
                    // appending to `builder` so a failed check never leaves an orphan span in `.pack.zst`.
                    let still = |at: u64, expect: &str| -> bool {
                        if *document {
                            let mut f = &file;
                            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).is_ok()
                                && cv_core::blake3_hex_reader(f).is_ok_and(|(len, hex)| len == at && hex == expect)
                        } else {
                            SourceCheckpoint::read(&file, at).is_ok_and(|c| c.boundary_blake3 == expect)
                        }
                    };
                    let prefix_ok = match prior.is_empty() {
                        true => true,
                        false => known
                            .get(&item.key.selector())
                            .and_then(|p| match &p.custody {
                                cv_core::custody::CustodyKind::NativeExtents { prefix_blake3, .. } => {
                                    Some(still(*from, prefix_blake3))
                                }
                                _ => None,
                            })
                            .unwrap_or(false),
                    };
                    if !prefix_ok || !still(*to, &checkpoint.boundary_blake3) {
                        summary
                            .failed
                            .push((fail_who, "source rewritten during archive".into()));
                        continue;
                    }
                    let mut reader = &file;
                    if let Err(e) = std::io::Seek::seek(&mut reader, std::io::SeekFrom::Start(*from)) {
                        summary
                            .failed
                            .push((fail_who, format!("seeking {}: {e}", path.display())));
                        continue;
                    }
                    let addr = builder.append_reader(&anchor, &mut reader, to - from)?;
                    let post_ok = if *document {
                        addr.length == *to
                            && addr.digest_hex() == checkpoint.boundary_blake3
                            && still(*to, &checkpoint.boundary_blake3)
                    } else {
                        still(*to, &checkpoint.boundary_blake3)
                    };
                    if !post_ok {
                        summary
                            .failed
                            .push((fail_who, "source rewritten during archive".into()));
                        continue;
                    }
                    builder.record_checkpoint(CodecCheckpoint {
                        anchor: anchor.clone(),
                        decoded_offset: addr.decoded_offset + addr.length,
                        codec_version: 1,
                        settled: true,
                        dependencies: Vec::new(),
                        continuation_state: json!({
                            "source_offset": to,
                            "boundary_blake3": checkpoint.boundary_blake3,
                        }),
                    })?;
                    // Only stamp the full `Baseline` as fresh when the live file at pack time has
                    // no bytes past `*to` AND `stream_counted`'s guarded baseline also ended at `*to`.
                    // If a writer appended lines between `native_delta` and `stream_counted` (or
                    // after `stream_counted`, or left an incomplete trailing line), keep `(dev, ino)`
                    // continuity with `stamp: None` so the next `sync_refs` pass is guaranteed not
                    // to skip `[to..new_len)` and chains only the missing delta.
                    let post_len = file.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
                    let all_complete_captured = post_len == *to
                        && item.baseline.as_ref().and_then(|b| b.primary).is_some_and(|id| {
                            id.len == *to && now_file_id.is_some_and(|now| (now.dev, now.ino) == (id.dev, id.ino))
                        });
                    let (entry_baseline, entry_source_len, entry_source_mtime_ns) = if all_complete_captured {
                        (item.baseline.clone(), item.source_len, item.source_mtime_ns)
                    } else {
                        let partial_base = now_file_id.map(|id| cv_core::source::Baseline {
                            primary: Some(cv_core::source::FileId {
                                len: *to,
                                mtime_ns: 0,
                                ctime_ns: 0,
                                dev: id.dev,
                                ino: id.ino,
                            }),
                            stamp: None,
                            rows: None,
                        });
                        (partial_base, None, None)
                    };
                    let mut extents = prior.clone();
                    extents.push(addr.clone());
                    (
                        addr,
                        cv_core::custody::CustodyKind::NativeExtents {
                            extents,
                            total_bytes: *to,
                            prefix_blake3: checkpoint.boundary_blake3.clone(),
                            file_name: file_name.clone(),
                            layout: layout.clone(),
                        },
                        entry_baseline,
                        entry_source_len,
                        entry_source_mtime_ns,
                    )
                }
            };
            entries.push(ArchivedSessionEntry {
                producer_id: producer_id.clone(),
                profile: item.key.profile.clone(),
                harness: item.harness.clone(),
                id: item.id.clone(),
                title: item.title.clone(),
                cwd: item.cwd.clone(),
                created_at: item.created_at,
                updated_at: item.updated_at,
                message_count: item.message_count,
                archived_at,
                content_blake3: addr.digest_hex(),
                source_len: entry_source_len,
                source_mtime_ns: entry_source_mtime_ns,
                baseline: entry_baseline,
                addr,
                custody,
            });
            written.push(item.key);
        }
        if entries.is_empty() {
            return Ok(summary);
        }

        let pack_file_manifest = builder.seal(Claim::New)?;
        let zst_path = pack_zst_path(&packs_dir, &pack_id)?;
        let (byte_len, pack_blake3) = cv_core::blake3_hex_reader(
            std::fs::File::open(&zst_path).with_context(|| format!("reading sealed pack {}", zst_path.display()))?,
        )?;
        let rel_pack_path = format!("packs/{pack_id}.pack.zst");

        // 2. Append capture envelopes to `StateStore` (`capture_inbox`) with pack-scoped `op_id`s
        //    (preventing retry wedges if an earlier pack failed before publishing) and record each
        //    archived session's exact `local_commit_pos` + `(epoch, seq)` for `Shipper::sync`.
        let mut committed = Vec::with_capacity(entries.len());
        for (key, entry) in written.iter().zip(entries.iter()) {
            let op_id = format!("archive:{pack_id}:{}", key.selector());
            let mut envelope = json!({
                "kind": "session_archive",
                "producer_id": producer_id,
                "harness": entry.harness,
                "session_id": entry.id,
                "profile": key.profile,
                "title": entry.title,
                "cwd": entry.cwd,
                "created_at": entry.created_at.map(|t| t.to_rfc3339()),
                "updated_at": entry.updated_at.map(|t| t.to_rfc3339()),
                "message_count": entry.message_count,
                "content_blake3": entry.content_blake3,
                "pack_id": pack_id,
                "decoded_offset": entry.addr.decoded_offset,
                "length": entry.addr.length,
            });
            if !entry.custody.is_projection() {
                envelope["custody"] = serde_json::to_value(&entry.custody)?;
            }
            let payload_ref_json = serde_json::to_string(&entry.addr)?;
            let req = AppendRequest::new(op_id.clone(), entry.content_blake3.clone(), envelope.clone())
                .with_producer(producer_id.clone())
                .with_payload_ref(payload_ref_json);
            let receipt = store
                .append_request(&req)
                .context("appending archive capture envelope to StateStore")?;
            committed.push(CommittedArchiveRecord {
                local_commit_pos: receipt.local_commit_pos,
                epoch: receipt.event_id.epoch,
                seq: receipt.event_id.seq,
                op_id,
                envelope,
                entry: entry.clone(),
            });
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

/// A session held natively: one file, none of whose declared companions
/// ([`cv_core::Adapter::dependencies`]: Claude's tasks, spilled tool results, an agent's meta)
/// exists. Anything else is projected, since a chain of the file alone would drop what the read
/// also consults.
struct NativeFile {
    path: PathBuf,
    file_name: String,
    layout: Vec<String>,
    /// A whole JSON document (OpenSession), rewritten whole: held through its last byte, and
    /// recaptured whole when it changes. Otherwise append-only JSONL, held through its last LF.
    document: bool,
}

/// [`NativeFile`] of `r`, when its harness writes one self-contained file: append-only JSONL for
/// Claude, Codex and Qwen session transcripts, a whole document for OpenSession. A JSONL file
/// without one finished line yet has nothing native to hold; it is projected as it reads.
fn native_file(
    r: &SessionRef,
    adapter: &dyn cv_core::Adapter,
    probe: &mut cv_core::source::Probe,
) -> Option<NativeFile> {
    let path = &r.path;
    let ext = path.extension().and_then(|e| e.to_str());
    // The adapter names the native form ([`cv_core::Adapter::native_custody`]).
    let form = adapter.native_custody().filter(|f| ext == Some(f.extension()))?;
    let document = form == cv_core::harness::NativeCustody::Document;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let mut deps = cv_core::source::Dependencies::default();
    adapter.dependencies(r, probe, &mut deps)?;
    let companions = [deps.files, deps.directories, deps.resumable, deps.growing];
    if companions.iter().flatten().any(|p| p != path && p.exists()) {
        return None;
    }
    if !document && last_complete_line_end(&std::fs::File::open(path).ok()?).ok()? == 0 {
        return None;
    }
    Some(NativeFile {
        path: path.clone(),
        file_name: path.file_name()?.to_str()?.to_owned(),
        layout: cv_core::custody::NativeName::layout_of(path),
        document,
    })
}

/// One session's read for archiving: its metadata, message count, IR payload when projected,
/// and the verified baseline.
type Rendered = (
    cv_core::Session,
    usize,
    Option<Vec<u8>>,
    Option<cv_core::source::Baseline>,
);

/// The bytes `[from, to)` a native capture appends to the chain `prior`, and the checkpoint of
/// the prefix through `to` (full-file BLAKE3 when `native.document == true`).
struct Delta {
    from: u64,
    to: u64,
    prior: Vec<PayloadAddr>,
    checkpoint: SourceCheckpoint,
    file_id: Option<cv_core::source::FileId>,
}

/// The byte range a native capture must add: `None` when the archived chain already holds the
/// exact bytes on disk. Documents (`.opensession.json`) are hashed whole via `blake3_hex_reader`
/// and recaptured whole whenever changed. Append-only JSONL requires `(dev, ino)` continuity and
/// strict growth (`*total_bytes < to`) to extend `extents`; a same-length stat bump (`touch`) on
/// the same inode skips only when the full `[0..to)` BLAKE3 matches the archived chain, while
/// any inode change (atomic temp-file rename), rewrite, or truncation starts a fresh chain at 0.
fn native_delta(
    packs_dir: &Path,
    key: &cv_core::SessionKey,
    native: &NativeFile,
    prev: Option<&ArchivedSessionEntry>,
) -> Result<Option<Delta>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let path = &native.path;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = file.metadata()?;
    let file_id = cv_core::source::FileId::of(&meta);
    if native.document {
        let (to, doc_blake3) = cv_core::blake3_hex_reader(&file)?;
        anyhow::ensure!(to > 0, "empty native document {}", path.display());
        if let Some(prev) = prev {
            if let cv_core::custody::CustodyKind::NativeExtents {
                extents, total_bytes, ..
            } = &prev.custody
            {
                if !extents.is_empty() && *total_bytes == to && prev.content_blake3 == doc_blake3 {
                    return Ok(None);
                }
            }
        }
        return Ok(Some(Delta {
            from: 0,
            to,
            prior: Vec::new(),
            checkpoint: SourceCheckpoint {
                offset: to,
                boundary_blake3: doc_blake3,
            },
            file_id,
        }));
    }

    let to = last_complete_line_end(&file)?;
    anyhow::ensure!(to > 0, "no complete line in {}", path.display());
    let same_inode = prev
        .and_then(|p| p.baseline.as_ref())
        .and_then(|b| b.primary)
        .zip(file_id)
        .is_some_and(|(old, now)| (old.dev, old.ino) == (now.dev, now.ino));
    let (from, prior) = match prev.map(|p| &p.custody) {
        Some(cv_core::custody::CustodyKind::NativeExtents {
            extents,
            total_bytes,
            prefix_blake3,
            ..
        }) if same_inode => {
            if *total_bytes < to && SourceCheckpoint::read(&file, *total_bytes)?.boundary_blake3 == *prefix_blake3 {
                (*total_bytes, extents.clone())
            } else if *total_bytes == to && !extents.is_empty() {
                let mut f = &file;
                f.seek(SeekFrom::Start(0))?;
                let (_, live_blake3) = cv_core::blake3_hex_reader(f.take(to))?;
                let archived_matches = if extents.len() == 1 {
                    extents[0].digest_hex() == live_blake3
                } else {
                    cv_core::custody::ExtentChain::open(packs_dir, key, extents)
                        .ok()
                        .and_then(|c| {
                            cv_core::blake3_hex_reader(cv_core::custody::ExtentChainReader::new(std::sync::Arc::new(c)))
                                .ok()
                        })
                        .is_some_and(|(len, hex)| len == to && hex == live_blake3)
                };
                if archived_matches {
                    return Ok(None);
                }
                (0, Vec::new())
            } else {
                (0, Vec::new())
            }
        }
        _ => (0, Vec::new()),
    };
    Ok(Some(Delta {
        from,
        to,
        prior,
        checkpoint: SourceCheckpoint::read(&file, to)?,
        file_id,
    }))
}

/// The offset just past the file's last LF: lines a writer has not finished wait for a later pass.
fn last_complete_line_end(file: &std::fs::File) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut end = file.metadata()?.len();
    let mut buf = vec![0u8; 64 * 1024];
    let mut f = file;
    while end > 0 {
        let len = end.min(buf.len() as u64);
        f.seek(SeekFrom::Start(end - len))?;
        f.read_exact(&mut buf[..len as usize])?;
        if let Some(i) = buf[..len as usize].iter().rposition(|&b| b == b'\n') {
            return Ok(end - len + i as u64 + 1);
        }
        end -= len;
    }
    Ok(0)
}

struct CommittedArchiveRecord {
    local_commit_pos: i64,
    epoch: String,
    seq: u64,
    op_id: String,
    envelope: serde_json::Value,
    entry: ArchivedSessionEntry,
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
    mut committed: Vec<CommittedArchiveRecord>,
) -> Result<()> {
    if committed.is_empty() {
        return Ok(());
    }
    committed.sort_by_key(|c| c.local_commit_pos);
    committed.dedup_by_key(|c| c.local_commit_pos);

    let mut runs: Vec<Vec<CommittedArchiveRecord>> = Vec::new();
    for item in committed {
        if let Some(last_run) = runs.last_mut() {
            if let Some(prev) = last_run.last() {
                if item.local_commit_pos == prev.local_commit_pos + 1 {
                    last_run.push(item);
                    continue;
                }
            }
        }
        runs.push(vec![item]);
    }

    for (run_idx, run) in runs.into_iter().enumerate() {
        let min_pos = run.first().expect("non-empty run").local_commit_pos;
        let max_pos = run.last().expect("non-empty run").local_commit_pos;
        let record_count = run.len() as u64;
        debug_assert_eq!((max_pos - min_pos + 1) as u64, record_count);
        let mut run_entries = Vec::with_capacity(run.len());
        let mut run_commits = Vec::with_capacity(run.len());
        for c in run {
            run_commits.push(json!({
                "local_commit_pos": c.local_commit_pos,
                "producer_id": c.entry.producer_id,
                "epoch": c.epoch,
                "seq": c.seq,
                "op_id": c.op_id,
                "envelope": c.envelope,
                "payload_addr": c.entry.addr,
            }));
            run_entries.push(c.entry);
        }
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
                "commits": run_commits,
            }),
            published_at: published_at_ms,
        };
        store
            .publish_pack_range(&state_manifest)
            .context("publishing pack manifest range in StateStore")?;
    }
    Ok(())
}

#[cfg(test)]
type ArchiveTestHook = std::cell::RefCell<Option<Box<dyn FnMut(&Path)>>>;

#[cfg(test)]
thread_local! {
    static AFTER_NATIVE_DELTA: ArchiveTestHook = std::cell::RefCell::new(None);
    static BEFORE_PACK_STAGE: ArchiveTestHook = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_test_hook(slot: &'static std::thread::LocalKey<ArchiveTestHook>, path: &Path) {
    slot.with(|cell| {
        if let Some(hook) = cell.borrow_mut().as_mut() {
            hook(path);
        }
    });
}

#[cfg(test)]
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

const PROBE_BATCH: usize = 256;

fn is_non_sqlite_path(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !matches!(ext.as_str(), "db" | "sqlite" | "sqlite3" | "vscdb")
}

/// Conservative fallback used only for pre-0.19.1 `pack_manifests` rows recorded before
/// `ArchivedSessionEntry.baseline` existed; newly archived entries carry a full `Baseline`.
fn is_legacy_entry_unchanged(
    r: &SessionRef,
    primary: &Path,
    source_len: Option<u64>,
    source_mtime_ns: Option<i128>,
    prev: &ArchivedSessionEntry,
) -> bool {
    let (self_contained_file_kind, has_companions) = match r.harness {
        Harness::Codex | Harness::Qwen | Harness::Continue | Harness::LmStudio | Harness::Pi | Harness::OpenSession => {
            (true, false)
        }
        Harness::Claude
        | Harness::Antigravity
        | Harness::OpenCode
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Grok
        | Harness::Cline
        | Harness::Roo
        | Harness::Gemini => (false, true),
        Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport => (false, false),
    };
    let self_contained_file = self_contained_file_kind
        && is_non_sqlite_path(&r.path)
        && std::fs::metadata(&r.path).is_ok_and(|m| m.is_file());
    let sqlite_primary = !is_non_sqlite_path(primary);
    let stat_unchanged = self_contained_file
        && source_len.is_some()
        && source_mtime_ns.is_some()
        && prev.source_len == source_len
        && prev.source_mtime_ns == source_mtime_ns;
    let meta_unchanged = !has_companions
        && r.updated_at.is_some()
        && prev.updated_at == r.updated_at
        && r.message_count.is_some_and(|mc| mc == prev.message_count)
        && (sqlite_primary
            || (source_len.is_some() && prev.source_len == source_len && prev.source_mtime_ns == source_mtime_ns));
    stat_unchanged || meta_unchanged
}

fn source_stat_ref(r: &SessionRef, primary: &Path) -> (Option<u64>, Option<i128>) {
    let (len, mtime_ns) = source_stat(primary);
    if len.is_some() {
        (len, mtime_ns)
    } else {
        source_stat(&r.path)
    }
}

type WatchBaselineMap = std::collections::HashMap<(Harness, String, PathBuf), cv_core::source::Baseline>;

fn watch_baseline(r: &SessionRef, probe: &mut cv_core::source::Probe) -> cv_core::source::Baseline {
    let mut b = probe.snapshot(r).with_reads(&cv_core::source::ReadSet::default());
    if b.rows.is_none() {
        b.rows = probe.rows(r);
    }
    b
}

fn seed_watch_baselines(
    refs: &[SessionRef],
    known: Option<&ArchiveSessionMap>,
    producer_id: Option<&str>,
) -> WatchBaselineMap {
    let mut seen = WatchBaselineMap::with_capacity(refs.len());
    for chunk in refs.chunks(PROBE_BATCH) {
        let mut probe = cv_core::source::Probe::default();
        for r in chunk {
            let key = (r.harness, r.id.clone(), r.path.clone());
            if let (Some(known), Some(_)) = (known, producer_id) {
                let akey = r.session_selector();
                if let Some(base) = known.get(&akey).and_then(|e| e.baseline.as_ref()) {
                    let now = probe.snapshot(r);
                    if base.fresh(&now, &mut probe) || base.rows_fresh(r, &now, &mut probe) {
                        seen.insert(key, base.clone());
                        continue;
                    }
                }
            }
            let base = watch_baseline(r, &mut probe);
            if base.primary.is_some() {
                seen.insert(key, base);
            }
        }
    }
    seen
}

fn filter_watch_candidates(refs: Vec<SessionRef>, seen: &mut WatchBaselineMap) -> Vec<SessionRef> {
    let mut candidates = Vec::new();
    for chunk in refs.chunks(PROBE_BATCH) {
        let mut probe = cv_core::source::Probe::default();
        for r in chunk {
            let key = (r.harness, r.id.clone(), r.path.clone());
            let now = probe.snapshot(r);
            if let Some(prev) = seen.get(&key) {
                if prev.fresh(&now, &mut probe) || prev.rows_fresh(r, &now, &mut probe) {
                    continue;
                }
            }
            let mut next = now.with_reads(&cv_core::source::ReadSet::default());
            next.rows = probe.rows(r);
            if next.primary.is_some() {
                seen.insert(key, next);
            } else {
                seen.remove(&key);
            }
            candidates.push(r.clone());
        }
    }
    candidates
}

fn evict_failed_watch_baselines(refs: &[SessionRef], failed: &[(String, String)], seen: &mut WatchBaselineMap) {
    if failed.is_empty() {
        return;
    }
    let failed_ids: std::collections::HashSet<&str> = failed.iter().map(|(who, _)| who.as_str()).collect();
    for r in refs {
        let tag = format!("{}:{}", r.harness, r.id);
        if failed_ids.contains(tag.as_str()) {
            seen.remove(&(r.harness, r.id.clone(), r.path.clone()));
        }
    }
}

fn reconcile_watch_baselines(candidates: &[SessionRef], summary: &SyncSummary, seen: &mut WatchBaselineMap) {
    for (harness, id, base) in &summary.verified_baselines {
        for c in candidates {
            if c.harness.as_str() == harness && &c.session_selector() == id {
                seen.insert((c.harness, c.id.clone(), c.path.clone()), base.clone());
            }
        }
    }
    for entry in &summary.archived_entries {
        for c in candidates {
            if c.harness.as_str() == entry.harness
                && c.id == entry.id
                && cv_core::SessionKey::local(c).profile == entry.profile
            {
                let key = (c.harness, c.id.clone(), c.path.clone());
                match &entry.baseline {
                    Some(base) if base.stamp.is_some() => {
                        seen.insert(key, base.clone());
                    }
                    _ => {
                        seen.remove(&key);
                    }
                }
            }
        }
    }
    evict_failed_watch_baselines(candidates, &summary.failed, seen);
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
    // Custody-backed refs are this archive's own packs (or hub mirrors): never re-archive them.
    cv_core::discover_all()
        .into_iter()
        .filter(|r| !cv_core::custody::is_pack_ref(r))
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

fn resolve_shipper(
    archive: &Archive,
    target: &str,
    cli_token: Option<&str>,
) -> Result<(String, cv_core::state::hub::Shipper)> {
    let target = target.trim();
    if target.is_empty() {
        bail!("--to target must not be empty (pass an `https://host` or `http://host:port` hub URL or a `[replication.<name>]` profile)");
    }
    let cfg = cv_core::config::load();
    let (target_name, hub_url, profile) = if target.starts_with("http://") || target.starts_with("https://") {
        let prof = cv_core::state::hub::resolve_replication_profile(&cfg, target);
        (target.to_string(), target.to_string(), prof)
    } else {
        if !cfg.replication.contains_key(target) {
            bail!(
                "unknown replication target `{target}`: pass an `https://host` or `http://host:port` URL or configure `[replication.{target}]` with `url = \"https://...\"` in config.toml"
            );
        }
        let prof = cv_core::state::hub::resolve_replication_profile(&cfg, target);
        let Some(url) = prof.url.clone().filter(|u| !u.trim().is_empty()) else {
            bail!("replication profile `[replication.{target}]` has no `url = \"https://...\"` in config.toml");
        };
        (target.to_string(), url, prof)
    };

    let bearer_token = cli_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            cv_core::env::var("CV_HUB_PRODUCER_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| profile.token.clone().filter(|s| !s.trim().is_empty()));

    let store = archive.state_store();
    let producer_id = profile
        .producer_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            cv_core::env::var("CV_PRODUCER_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .map(Ok)
        .unwrap_or_else(|| store.ensure_ship_producer_id())
        .context("resolving replication producer_id")?;

    let mut ship_cfg = cv_core::state::hub::ShipConfig::new(target_name.clone(), hub_url, producer_id)
        .with_host_label(cv_core::state::default_host_label())
        .with_profile(profile);
    if let Some(tok) = bearer_token {
        ship_cfg = ship_cfg.with_token(tok);
    }
    let shipper = cv_core::state::hub::Shipper::new(ship_cfg).context("initializing hub replication shipper")?;
    Ok((target_name, shipper))
}

fn format_ship_report(target_name: &str, report: &cv_core::state::hub::ShipReport) -> String {
    if report.events_shipped == 0 {
        return format!("  ↳ replicated to {target_name}: already up to date");
    }
    let snap_note = report
        .hub_snapshot_id
        .as_deref()
        .map(|id| format!(" · snapshot {}", truncate_str(id, 20)))
        .unwrap_or_default();
    format!(
        "  ↳ replicated to {target_name}: {} event(s) ({} session(s), {} filtered, {} redacted){snap_note}",
        report.events_shipped, report.sessions_admitted, report.sessions_filtered_out, report.redactions_applied
    )
}

pub fn cmd_sync(archive: &Archive, to: Option<String>, token: Option<String>) -> Result<()> {
    // Validate `--to` upfront before spending time syncing locally if the target profile is missing.
    let resolved_shipper = match to.as_deref() {
        Some(target) => Some(resolve_shipper(archive, target, token.as_deref())?),
        None => None,
    };

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

    if let Some((target_name, shipper)) = resolved_shipper {
        let store = archive.state_store();
        let report = shipper
            .sync(&store, &archive.packs_dir())
            .with_context(|| format!("shipping archive custody to `{target_name}`"))?;
        println!("{}", format_ship_report(&target_name, &report));
    }
    Ok(())
}

pub fn cmd_watch(
    archive: &Archive,
    interval_secs: f64,
    harness_filter: Option<String>,
    cwd_filter: Option<String>,
    to: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let want = parse_harness(&harness_filter)?;
    let poll = poll_interval(interval_secs)?;
    let resolved_shipper = match to.as_deref() {
        Some(target) => Some(resolve_shipper(archive, target, token.as_deref())?),
        None => None,
    };

    // Announce immediately to stderr BEFORE initial discovery/sync (`docs/INTERFACE-V2.md` §2:
    // daemons announce on stderr within 1s so piped callers know what is running).
    eprintln!(
        "cv watch: watching (interval {}s{}{}{}) -> {}",
        poll.as_secs_f64(),
        want.map(|h| format!(" · harness={h}")).unwrap_or_default(),
        cwd_filter
            .as_deref()
            .map(|c| format!(" · cwd~{c:?}"))
            .unwrap_or_default(),
        resolved_shipper
            .as_ref()
            .map(|(name, _)| format!(" · to={name}"))
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

    let mut ship_retry_pending = false;
    if let Some((ref target_name, ref shipper)) = resolved_shipper {
        let store = archive.state_store();
        match shipper.sync(&store, &archive.packs_dir()) {
            Ok(report) => {
                if report.events_shipped > 0 {
                    eprintln!("{}", format_ship_report(target_name, &report));
                }
            }
            Err(e) => {
                ship_retry_pending = true;
                eprintln!("  ! replicate {target_name}: {e:#}");
            }
        }
    }

    // Track full `cv_core::source::Baseline` (primary identity + declared/dynamic dependencies +
    // shared-store `Rows` certificate) per `(harness, session_id, path)` so idle polls perform
    // zero session parses across all 22 harnesses while catching companion, subagent, WAL, and
    // shared multi-session edits.
    let initial_known = archive.entries_by_key().ok().map(|(m, _)| m);
    let watch_producer = archive.state_store().default_producer().to_string();
    let mut seen_baseline = seed_watch_baselines(&initial_refs, initial_known.as_ref(), Some(&watch_producer));
    evict_failed_watch_baselines(&initial_refs, &init.failed, &mut seen_baseline);

    loop {
        std::thread::sleep(poll);
        let refs = discover_filtered(want, cwd_filter.as_deref());
        let candidates = filter_watch_candidates(refs, &mut seen_baseline);

        let mut synced_any = false;
        if !candidates.is_empty() {
            let batch = archive.sync_refs(&candidates)?;
            for (who, why) in &batch.failed {
                eprintln!("  ! {who}: {why}");
            }
            reconcile_watch_baselines(&candidates, &batch, &mut seen_baseline);
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
            synced_any = batch.archived > 0;
        }

        if let Some((ref target_name, ref shipper)) = resolved_shipper {
            let store = archive.state_store();
            let has_inbox = store.inbox_count().unwrap_or(0) > 0;
            if synced_any || ship_retry_pending || has_inbox {
                match shipper.sync(&store, &archive.packs_dir()) {
                    Ok(report) => {
                        ship_retry_pending = false;
                        if report.events_shipped > 0 {
                            println!("{}", format_ship_report(target_name, &report));
                        }
                    }
                    Err(e) => {
                        ship_retry_pending = true;
                        eprintln!("  ! replicate {target_name}: {e:#}");
                    }
                }
            }
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
    use std::time::{Duration, SystemTime};

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
            "identical native bytes must deduplicate even if mtime moved"
        );
        assert_eq!(third.skipped, 2);
        assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

        // 4. Hydrate payload from sealed pack (verifying BLAKE3 + XXH64) and check content.
        let alpha_bytes = cv_core::custody::read_session_bytes(&archive.home, "claude", "sess-alpha")
            .unwrap()
            .expect("alpha present");
        // A single-file Claude session is held as its own bytes.
        assert_eq!(alpha_bytes, std::fs::read(&a_path).unwrap());

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
            profile: Default::default(),
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
            baseline: None,
            addr: addr_1,
            custody: Default::default(),
        };
        let entry_2 = ArchivedSessionEntry {
            id: "s-two".into(),
            title: Some("Two".into()),
            content_blake3: cv_core::blake3_hex(payload_2),
            addr: addr_2,
            custody: Default::default(),
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
            vec![
                CommittedArchiveRecord {
                    local_commit_pos: r1.local_commit_pos,
                    epoch: r1.event_id.epoch,
                    seq: r1.event_id.seq,
                    op_id: format!("archive:{pack_id}:{producer}:claude:s-one"),
                    envelope: json!({"kind": "session_archive", "session_id": "s-one"}),
                    entry: entry_1,
                },
                CommittedArchiveRecord {
                    local_commit_pos: r2.local_commit_pos,
                    epoch: r2.event_id.epoch,
                    seq: r2.event_id.seq,
                    op_id: format!("archive:{pack_id}:{producer}:claude:s-two"),
                    envelope: json!({"kind": "session_archive", "session_id": "s-two"}),
                    entry: entry_2,
                },
            ],
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
        assert!(cv_core::custody::read_session_bytes(&archive.home, "claude", "s-one")
            .unwrap()
            .is_some());
        assert!(cv_core::custody::read_session_bytes(&archive.home, "claude", "s-two")
            .unwrap()
            .is_some());
    }

    #[test]
    fn watch_candidates_and_sync_refs_track_directory_sessions_and_multi_session_exports() {
        use cv_core::Adapter as _;
        let world = cv_testkit::World::new("archive-watch-dir-and-multi");
        let archive = Archive::resolve(Some(world.cv_home.join("archive"))).unwrap();

        // 1. Directory-backed `kimi-code` session (`r.path` is `.../session_kc1`, transcript is `agents/main/wire.jsonl`).
        let kc_home = world.base.join("kimi-code-home");
        let sess_dir = kc_home.join("sessions/wd1/session_kc1");
        let wire_path = sess_dir.join("agents/main/wire.jsonl");
        let state_path = sess_dir.join("state.json");
        std::fs::create_dir_all(wire_path.parent().unwrap()).unwrap();
        std::fs::write(
            &wire_path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.0"}"#,
                "\n",
                r#"{"timestamp":1710000001.0,"message":{"type":"TurnBegin","payload":{"user_input":"first prompt"}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let kc = cv_core::harness::kimi_code::KimiCode::with_home(&kc_home);
        let kc_refs = kc.discover().unwrap().refs;
        assert_eq!(kc_refs.len(), 1);
        assert!(kc_refs[0].path.is_dir());

        assert_eq!(archive.sync_refs(&kc_refs).unwrap().archived, 1);
        let known = archive.entries_by_key().unwrap().0;
        let prod = archive.state_store().default_producer().to_string();
        let mut seen = seed_watch_baselines(&kc_refs, Some(&known), Some(&prod));

        // Idle poll on directory-backed `kimi-code` emits ZERO candidates (zero session parses!).
        assert!(
            filter_watch_candidates(kc.discover().unwrap().refs, &mut seen).is_empty(),
            "idle kimi-code poll must emit zero watch candidates"
        );
        // Idle `sync_refs` ALSO skips via `prev.baseline.fresh` with zero new packs.
        let idle_sync = archive.sync_refs(&kc_refs).unwrap();
        assert_eq!(idle_sync.archived, 0);
        assert_eq!(idle_sync.skipped, 1);

        // Creating/editing companion `state.json` (without touching `wire.jsonl`!) must surface in watch candidates.
        std::fs::write(&state_path, r#"{"title":"Updated via state.json"}"#).unwrap();
        let kc_state_cands = filter_watch_candidates(kc.discover().unwrap().refs, &mut seen);
        assert_eq!(
            kc_state_cands.len(),
            1,
            "companion state.json mutation must surface in watch candidates"
        );
        assert!(
            filter_watch_candidates(kc.discover().unwrap().refs, &mut seen).is_empty(),
            "subsequent idle poll after state.json update must be quiet again"
        );

        // Appending to nested `agents/main/wire.jsonl` does NOT touch `session_kc1`'s directory mtime,
        // yet both `filter_watch_candidates` and `Archive::sync_refs` must detect and archive the growth!
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&wire_path).unwrap();
            writeln!(
                f,
                r#"{{"timestamp":1710000002.0,"message":{{"type":"ContentPart","payload":{{"type":"text","text":"second reply"}}}}}}"#
            )
            .unwrap();
        }
        let kc_refs_after = kc.discover().unwrap().refs;
        let kc_cands = filter_watch_candidates(kc_refs_after, &mut seen);
        assert_eq!(
            kc_cands.len(),
            1,
            "nested wire.jsonl append must surface in watch candidates"
        );
        assert_eq!(
            archive.sync_refs(&kc_cands).unwrap().archived,
            1,
            "sync_refs must re-archive grown nested wire.jsonl"
        );

        // 2. Multi-session `chatgpt-export` (`conversations.json` shared by two sessions `cg-1` and `cg-2`).
        let exp_file = world.base.join("conversations.json");
        std::fs::write(&exp_file, b"[]").unwrap();
        let make_export_refs = |path: &Path| {
            vec![
                SessionRef {
                    id: "cg-1".into(),
                    harness: Harness::ChatGptExport,
                    path: path.to_path_buf(),
                    cwd: None,
                    title: Some("One".into()),
                    created_at: None,
                    updated_at: None,
                    message_count: Some(1),
                },
                SessionRef {
                    id: "cg-2".into(),
                    harness: Harness::ChatGptExport,
                    path: path.to_path_buf(),
                    cwd: None,
                    title: Some("Two".into()),
                    created_at: None,
                    updated_at: None,
                    message_count: Some(1),
                },
            ]
        };
        let mut exp_seen = seed_watch_baselines(&make_export_refs(&exp_file), None, None);
        assert!(
            filter_watch_candidates(make_export_refs(&exp_file), &mut exp_seen).is_empty(),
            "idle multi-session export must emit zero watch candidates"
        );
        std::fs::write(&exp_file, b"[{}, {}]").unwrap();
        let exp_cands = filter_watch_candidates(make_export_refs(&exp_file), &mut exp_seen);
        assert_eq!(
            exp_cands.len(),
            2,
            "updating shared conversations.json must emit ALL sibling sessions, not just the first"
        );
        assert!(
            filter_watch_candidates(make_export_refs(&exp_file), &mut exp_seen).is_empty(),
            "subsequent idle poll on updated conversations.json must emit zero candidates"
        );

        // 3. Prove `Archive::sync_refs` and `filter_watch_candidates` skip idle Claude sessions
        //    strictly from `Baseline` without invoking `stream_counted`, while catching companion
        //    task directory edits (`<claude_config>/tasks/<sid>/1.json`).
        let claude_cfg = world.base.join("claude-cfg");
        let claude_proj = claude_cfg.join("projects/proj-a");
        std::fs::create_dir_all(&claude_proj).unwrap();
        // Deliberately write truncated/unparseable bytes that fail `stream_counted` if parsed,
        // and seed an `ArchivedSessionEntry` with its current `Baseline`.
        let unparsed_claude = claude_proj.join("sess-no-parse.jsonl");
        std::fs::write(&unparsed_claude, b"\xff\xfe\xfd").unwrap();
        let claude_ref = SessionRef::new("sess-no-parse", Harness::Claude, &unparsed_claude);
        let mut claude_seen = seed_watch_baselines(std::slice::from_ref(&claude_ref), None, None);
        assert!(
            filter_watch_candidates(vec![claude_ref.clone()], &mut claude_seen).is_empty(),
            "idle Claude session must emit zero watch candidates"
        );

        let mut probe = cv_core::source::Probe::default();
        let captured_base = watch_baseline(&claude_ref, &mut probe);
        let store = archive.state_store();
        let pack_id = "pack-claude-baseline-proof";
        let mut builder = PackBuilder::create(&archive.packs_dir(), pack_id).unwrap();
        let dummy_ir = br#"{"id":"sess-no-parse","harness":"claude","messages":[]}"#;
        let addr = builder.append_record(claude_ref.session_selector(), dummy_ir).unwrap();
        let pfm = builder.seal(Claim::New).unwrap();
        let zst = std::fs::read(pack_zst_path(&archive.packs_dir(), pack_id).unwrap()).unwrap();
        let append = store
            .append_request(&AppendRequest::new(
                format!("archive:{pack_id}:{prod}:claude:sess-no-parse"),
                cv_core::blake3_hex(dummy_ir),
                json!({"kind": "session_archive", "session_id": "sess-no-parse"}),
            ))
            .unwrap();
        publish_contiguous_archive_runs(
            &store,
            pack_id,
            &format!("packs/{pack_id}.pack.zst"),
            zst.len() as u64,
            &cv_core::blake3_hex(&zst),
            &pfm,
            Utc::now().timestamp_millis(),
            vec![CommittedArchiveRecord {
                local_commit_pos: append.local_commit_pos,
                epoch: append.event_id.epoch,
                seq: append.event_id.seq,
                op_id: format!("archive:{pack_id}:{prod}:claude:sess-no-parse"),
                envelope: json!({"kind": "session_archive", "session_id": "sess-no-parse"}),
                entry: ArchivedSessionEntry {
                    profile: cv_core::SessionKey::local(&claude_ref).profile,
                    producer_id: prod.clone(),
                    harness: "claude".into(),
                    id: "sess-no-parse".into(),
                    title: None,
                    cwd: None,
                    created_at: None,
                    updated_at: None,
                    message_count: 0,
                    archived_at: Utc::now(),
                    content_blake3: cv_core::blake3_hex(dummy_ir),
                    source_len: Some(3),
                    source_mtime_ns: None,
                    baseline: Some(captured_base),
                    addr,
                    custody: Default::default(),
                },
            }],
        )
        .unwrap();

        // Because `Baseline::fresh` holds, `sync_refs` skips `sess-no-parse` WITHOUT opening or
        // parsing `\xff\xfe\xfd`!
        let skipped_proof = archive.sync_refs(std::slice::from_ref(&claude_ref)).unwrap();
        assert_eq!(skipped_proof.skipped, 1);
        assert_eq!(skipped_proof.archived, 0);
        assert!(
            skipped_proof.failed.is_empty(),
            "unchanged Claude baseline must skip before stream_counted: {:?}",
            skipped_proof.failed
        );

        // Creating a companion task file `<claude_cfg>/tasks/sess-no-parse/1.json` invalidates
        // `Baseline::fresh` on both `filter_watch_candidates` and `Archive::sync_refs`!
        let task_file = claude_cfg.join("tasks/sess-no-parse/1.json");
        std::fs::create_dir_all(task_file.parent().unwrap()).unwrap();
        std::fs::write(&task_file, r#"{"id":"1","subject":"Check companion"}"#).unwrap();
        assert_eq!(
            filter_watch_candidates(vec![claude_ref.clone()], &mut claude_seen).len(),
            1,
            "Claude companion tasks/<sid> edit must surface in watch candidates"
        );
        let invalidated = archive.sync_refs(std::slice::from_ref(&claude_ref)).unwrap();
        assert_eq!(
            invalidated.skipped, 0,
            "Claude companion tasks/<sid> edit must invalidate sync_refs baseline fast-path"
        );
        reconcile_watch_baselines(std::slice::from_ref(&claude_ref), &invalidated, &mut claude_seen);
        assert!(
            filter_watch_candidates(vec![claude_ref.clone()], &mut claude_seen).is_empty(),
            "reconciled baseline after companion edit must stay quiet on idle poll"
        );

        // 4. Prove a failed `sync_refs` read (e.g. corrupt zstd rollout) is evicted from
        //    `seen_baseline` by `reconcile_watch_baselines` so the next poll retries it instead of
        //    permanently dropping the session.
        let bad_zst = world.base.join("corrupt-rollout.jsonl.zst");
        std::fs::write(&bad_zst, b"not-valid-zstd-frame").unwrap();
        let bad_ref = SessionRef::new("codex-corrupt", Harness::Codex, &bad_zst);
        let mut fail_seen = WatchBaselineMap::new();
        let first_cands = filter_watch_candidates(vec![bad_ref.clone()], &mut fail_seen);
        assert_eq!(first_cands.len(), 1);
        let failed_batch = archive.sync_refs(&first_cands).unwrap();
        assert_eq!(failed_batch.failed.len(), 1);
        reconcile_watch_baselines(&first_cands, &failed_batch, &mut fail_seen);
        assert_eq!(
            filter_watch_candidates(vec![bad_ref.clone()], &mut fail_seen).len(),
            1,
            "failed sync_refs session must be evicted from seen_baseline and retried on the next poll"
        );
    }

    /// #tooth Concurrent JSONL growth between `native_delta` and `stream_counted` never stamps
    /// `baseline.primary.len = len_t2` over a `[0..to_t1)` pack: the next sync captures `[to_t1..len_t2)`
    /// as a delta extent instead of skipping it forever.
    #[test]
    fn tooth_concurrent_append_between_native_delta_and_baseline_is_captured_on_next_sync() {
        let world = crate::test_world("archive-concurrent-append-gap");
        let path = world.write_session(
            "sess-gap",
            &[json!({
                "type": "user", "uuid": "u1", "sessionId": "sess-gap",
                "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": "first turn before append"}
            })],
        );
        let r = SessionRef::new("sess-gap", Harness::Claude, &path);
        let archive = Archive {
            home: world.cv_home.clone(),
        };
        let late_line = format!(
            "{}\n",
            json!({
                "type": "assistant", "uuid": "a1", "sessionId": "sess-gap",
                "timestamp": "2026-06-01T10:00:05Z",
                "message": {"role": "assistant", "content": "appended-between-delta-and-stream"}
            })
        );
        let target_path = path.clone();
        let line_copy = late_line.clone();
        AFTER_NATIVE_DELTA.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move |p| {
                if p == target_path {
                    use std::io::Write as _;
                    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
                    f.write_all(line_copy.as_bytes()).unwrap();
                }
            }));
        });
        let pass1 = archive.sync_refs(std::slice::from_ref(&r)).unwrap();
        AFTER_NATIVE_DELTA.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(pass1.archived, 1);

        // Pass 2 runs with NO further writes to `path`: it MUST NOT skip `[to_t1..len_t2)`!
        let pass2 = archive.sync_refs(std::slice::from_ref(&r)).unwrap();
        assert_eq!(
            pass2.archived, 1,
            "second sync must capture lines appended between native_delta and baseline: {pass2:?}"
        );
        assert_eq!(pass2.archived_entries[0].message_count, 2);
        match &pass2.archived_entries[0].custody {
            cv_core::custody::CustodyKind::NativeExtents { extents, .. } => {
                assert_eq!(extents.len(), 2, "second sync chains only the missing delta extent");
                assert_eq!(extents[1].length, late_line.len() as u64);
            }
            other => panic!("expected NativeExtents, got {other:?}"),
        }

        // Pass 3 is now quiescent and skips cleanly.
        let pass3 = archive.sync_refs(std::slice::from_ref(&r)).unwrap();
        assert_eq!((pass3.archived, pass3.skipped), (0, 1));

        let expected = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let recovered = cv_core::custody::read_session_bytes(&archive.home, "claude", "sess-gap")
            .unwrap()
            .unwrap();
        assert_eq!(recovered, expected);
    }

    /// #tooth Vanished or truncated live files at pack time record per-session failures without
    /// aborting sibling sessions in the batch or leaving orphan spans in `.pack.zst`, and native
    /// `message_count` counts turns (`Message::is_turn`), excluding tool-result-only user records.
    #[test]
    fn tooth_vanished_or_truncated_native_file_isolates_failure_without_orphan_spans_and_counts_turns() {
        let world = crate::test_world("archive-vanish-and-turn-count");
        let vanish_path = world.write_session(
            "sess-vanish",
            &[json!({
                "type": "user", "uuid": "v1", "sessionId": "sess-vanish",
                "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": "will vanish before pack"}
            })],
        );
        let trunc_path = world.write_session(
            "sess-trunc",
            &[json!({
                "type": "user", "uuid": "t1", "sessionId": "sess-trunc",
                "timestamp": "2026-06-01T10:00:00Z",
                "message": {"role": "user", "content": "will truncate before pack"}
            })],
        );
        let ok_path = world.write_session(
            "sess-turns",
            &[
                json!({
                    "type": "user", "uuid": "u1", "sessionId": "sess-turns",
                    "timestamp": "2026-06-01T10:00:00Z",
                    "message": {"role": "user", "content": "run a check"}
                }),
                json!({
                    "type": "assistant", "uuid": "a1", "sessionId": "sess-turns",
                    "timestamp": "2026-06-01T10:00:01Z",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "true"}}
                    ]}
                }),
                json!({
                    "type": "user", "uuid": "u2", "sessionId": "sess-turns",
                    "timestamp": "2026-06-01T10:00:02Z",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "tu1", "content": "ok"}
                    ]}
                }),
                json!({
                    "type": "assistant", "uuid": "a2", "sessionId": "sess-turns",
                    "timestamp": "2026-06-01T10:00:03Z",
                    "message": {"role": "assistant", "content": "all green"}
                }),
            ],
        );
        let refs = vec![
            SessionRef::new("sess-vanish", Harness::Claude, &vanish_path),
            SessionRef::new("sess-trunc", Harness::Claude, &trunc_path),
            SessionRef::new("sess-turns", Harness::Claude, &ok_path),
        ];
        let v_target = vanish_path.clone();
        let t_target = trunc_path.clone();
        BEFORE_PACK_STAGE.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move |p| {
                if p == v_target {
                    let _ = std::fs::remove_file(p);
                } else if p == t_target {
                    std::fs::write(p, b"short\n").unwrap();
                }
            }));
        });
        let archive = Archive {
            home: world.cv_home.clone(),
        };
        let summary = archive.sync_refs(&refs).unwrap();
        BEFORE_PACK_STAGE.with(|cell| *cell.borrow_mut() = None);

        assert_eq!(summary.archived, 1, "healthy sibling must still archive: {summary:?}");
        assert_eq!(
            summary.failed.len(),
            2,
            "vanished and truncated files fail per-session: {summary:?}"
        );
        // Tool-use + tool-result exchange has 3 turns (u1 prompt, a1 tool call reply, a2 reply) —
        // u2 is a tool-result-only user record, NOT a turn (`DATAPATHS.md` counting rule).
        let expected_turns = cv_core::stream::stream_counted(
            cv_core::harness::for_harness(Harness::Claude).unwrap().as_ref(),
            &refs[2],
            &cv_core::Demand::LEAN.with_spans(),
            &mut cv_core::stream::Discard,
            cv_core::stream::Pass::default(),
        )
        .unwrap()
        .counted
        .unwrap()
        .turns;
        assert_eq!(summary.archived_entries[0].message_count, expected_turns);

        // Verify zero orphan spans in the sealed `.pack.idx.json`: `total_decoded_len` equals the
        // single committed session's length!
        let pack_id = summary.pack_id.unwrap();
        let idx_raw = std::fs::read(cv_core::packs::pack_idx_path(&archive.packs_dir(), &pack_id).unwrap()).unwrap();
        let pfm = PackFileManifest::from_json_slice(&idx_raw).unwrap();
        assert_eq!(
            pfm.total_decoded_len,
            std::fs::metadata(&ok_path).unwrap().len(),
            "failed pre-append checks must leave zero orphan bytes in .pack.zst"
        );
    }
}
