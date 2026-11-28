//! Capture changed sessions and publish immutable pack ranges.
use super::{ArchivedSessionEntry, CustodyKind, SourceCheckpoint};
use crate::packs::{pack_zst_path, CodecCheckpoint, PackBuilder, PackManifest as PackFileManifest, PayloadAddr};
use crate::publish::Claim;
use crate::state::{AppendRequest, PackManifest as StatePackManifest, StateStore};
use crate::{Harness, SessionRef};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::{HashMap, HashSet};
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
    verified_baselines: Vec<(String, String, crate::source::Baseline)>,
}

/// Handle to `$CLUSTERVISION_HOME` (`packs/` + `state.db`).
pub struct Archive {
    pub home: PathBuf,
}

impl Archive {
    /// Use the explicit archive home when supplied, otherwise the environment’s configured home.
    /// Pure path resolution: creates nothing on disk.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = explicit
            .or_else(|| crate::env::Env::current().cv_home())
            .context("cannot locate home directory (set $CLUSTERVISION_HOME or pass --dir)")?;
        Ok(Self { home })
    }

    pub fn packs_dir(&self) -> PathBuf {
        self.home.join("packs")
    }

    pub fn state_store(&self) -> StateStore {
        StateStore::at_dir(&self.home)
    }

    /// Deduplicated archived sessions, newest `archived_at` first. Read-only (`--frozen` safe).
    pub fn entries(&self) -> Result<(Vec<ArchivedSessionEntry>, usize)> {
        let (by_key, pack_count) = super::archive_entries(&self.home)?;
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
    /// A single append-only native file keeps its own bytes. When the
    /// archived prefix still matches ([`SourceCheckpoint`]), the pass reads and packs only the
    /// complete lines appended since; a shrunk or rewritten file starts a fresh chain. Every
    /// other store is projected to Session IR JSON, as before.
    pub fn sync_refs(&self, refs: &[SessionRef]) -> Result<SyncSummary> {
        if crate::env::frozen() {
            bail!("--frozen forbids writing archive packs");
        }

        let store = self.state_store();
        let producer_id = store.default_producer().to_string();
        let packs_dir = self.packs_dir();
        let known = super::head_entries(&self.home)?;

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
                file_id: Option<crate::source::FileId>,
            },
        }
        struct StagedSession {
            key: crate::SessionKey,
            harness: String,
            id: String,
            title: Option<String>,
            cwd: Option<String>,
            created_at: Option<DateTime<Utc>>,
            updated_at: Option<DateTime<Utc>>,
            message_count: usize,
            source_len: Option<u64>,
            source_mtime_ns: Option<i128>,
            baseline: Option<crate::source::Baseline>,
            derived_info: Option<(String, String)>,
            content: Content,
        }

        let mut summary = SyncSummary::default();
        let mut staged: Vec<StagedSession> = Vec::new();
        // Deduplicate duplicate refs within the same input slice as well.
        let mut seen_in_pass = std::collections::HashSet::new();

        for chunk in refs.chunks(PROBE_BATCH) {
            let mut probe = crate::source::Probe::default();
            for r in chunk {
                let harness_str = r.harness.as_str().to_string();
                let key = crate::SessionKey::local(r);
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

                let Some(adapter) = crate::harness::for_harness(r.harness) else {
                    summary
                        .failed
                        .push((format!("{}:{}", r.harness, r.id), "no adapter registered".into()));
                    continue;
                };
                let native = native_file(r, adapter.as_ref(), &mut probe);
                let producer_key = crate::SessionKey {
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
                    let pass = crate::stream::Pass {
                        guard: true,
                        ..crate::stream::Pass::RECORD
                    };
                    let (read, turns, payload) = if native.is_some() {
                        // Native custody keeps the bytes; the read only certifies and describes them.
                        let read = crate::stream::stream_counted(
                            adapter.as_ref(),
                            r,
                            &crate::Demand::LEAN.with_spans(),
                            &mut crate::stream::Discard,
                            pass,
                        )?;
                        let turns = read.counted.as_ref().map_or(0, |c| c.turns);
                        (read, turns, None)
                    } else {
                        let mut sink = crate::api::WindowSink::new(&r.path, 0, None);
                        let read = crate::stream::stream_counted(
                            adapter.as_ref(),
                            r,
                            &crate::Demand::COMPLETE.with_spans(),
                            &mut sink,
                            pass,
                        )?;
                        let frames = sink.finish()?;
                        let total = sink.selected;
                        let turns = total - sink.carriers;
                        let mut buf = Vec::new();
                        crate::projection::write_session_document(&mut buf, &read.session, &frames, total, false)?;
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
                        if prev.is_some_and(|p| p.content_blake3 == crate::blake3_hex(&payload)) {
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

                let derived_info = crate::lineage::derived_info_session(&session_meta)
                    .map(|(p, k)| (p.to_owned(), k.to_owned()))
                    .or_else(|| crate::lineage::derived_info_ref(r));
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
                    derived_info,
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
            let is_native_delta_append =
                matches!(&item.content, Content::Native { prior, document, .. } if !prior.is_empty() && !*document);
            let anchor = super::archive_anchor(&crate::SessionKey {
                producer: producer_id.clone(),
                ..item.key.clone()
            });
            let (addr, custody, entry_baseline, entry_source_len, entry_source_mtime_ns) = match &item.content {
                Content::Projection(payload) => (
                    builder.append_record(&anchor, payload)?,
                    CustodyKind::PortableProjection,
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
                    let now_file_id = crate::source::FileId::of(&file_meta);
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
                                && crate::blake3_hex_reader(f).is_ok_and(|(len, hex)| len == at && hex == expect)
                        } else {
                            SourceCheckpoint::read(&file, at).is_ok_and(|c| c.boundary_blake3 == expect)
                        }
                    };
                    let prefix_ok = match prior.is_empty() {
                        true => true,
                        false => known
                            .get(&item.key.selector())
                            .and_then(|p| match &p.custody {
                                CustodyKind::NativeExtents { prefix_blake3, .. } => Some(still(*from, prefix_blake3)),
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
                        let partial_base = now_file_id.map(|id| crate::source::Baseline {
                            primary: Some(crate::source::FileId {
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
                        CustodyKind::NativeExtents {
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
            written.push((item.key, is_native_delta_append, item.derived_info));
        }
        if entries.is_empty() {
            return Ok(summary);
        }

        let pack_file_manifest = builder.seal(Claim::New)?;
        let zst_path = pack_zst_path(&packs_dir, &pack_id)?;
        let (byte_len, pack_blake3) = crate::blake3_hex_reader(
            std::fs::File::open(&zst_path).with_context(|| format!("reading sealed pack {}", zst_path.display()))?,
        )?;
        let rel_pack_path = format!("packs/{pack_id}.pack.zst");

        // 2. Append capture envelopes to `StateStore` (`capture_inbox`) with pack-scoped `op_id`s
        //    (preventing retry wedges if an earlier pack failed before publishing) and record each
        //    archived session's exact `local_commit_pos` + `(epoch, seq)` for `Shipper::sync`.
        //    Appended native extents emit `"kind": "session_delta"` carrying ONLY the newly added
        //    `delta_extents: [entry.addr]` (`O(1)` per commit, avoiding `O(R^2)` extent rewrites!).
        let mut committed = Vec::with_capacity(entries.len());
        for ((key, is_delta_append, derived_info), entry) in written.iter().zip(entries.iter()) {
            let op_id = format!("archive:{pack_id}:{}", key.selector());
            let mut envelope = json!({
                "archive_version": 2,
                "kind": if *is_delta_append { "session_delta" } else { "session_archive" },
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
            if let Some((df, dk)) = derived_info {
                envelope["metadata"] = json!({
                    "derived_from": df,
                    "derived_kind": dk,
                });
            }
            if *is_delta_append {
                if let CustodyKind::NativeExtents {
                    total_bytes,
                    prefix_blake3,
                    file_name,
                    layout,
                    ..
                } = &entry.custody
                {
                    if let Some(CustodyKind::NativeExtents {
                        prefix_blake3: base, ..
                    }) = known.get(&key.selector()).map(|e| &e.custody)
                    {
                        envelope["base_checkpoint"] = json!(base);
                    }
                    envelope["total_bytes"] = json!(total_bytes);
                    envelope["prefix_blake3"] = json!(prefix_blake3);
                    envelope["file_name"] = json!(file_name);
                    if !layout.is_empty() {
                        envelope["layout"] = json!(layout);
                    }
                    envelope["delta_extents"] = json!([entry.addr]);
                }
            } else if !entry.custody.is_projection() {
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
        //    inbox row interleaved between archived sessions
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
/// ([`crate::Adapter::dependencies`]: Claude's tasks, spilled tool results, an agent's meta)
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
fn native_file(r: &SessionRef, adapter: &dyn crate::Adapter, probe: &mut crate::source::Probe) -> Option<NativeFile> {
    let path = &r.path;
    let ext = path.extension().and_then(|e| e.to_str());
    // The adapter names the native form ([`crate::Adapter::native_custody`]).
    let form = adapter.native_custody().filter(|f| ext == Some(f.extension()))?;
    let document = form == crate::harness::NativeCustody::Document;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let mut deps = crate::source::Dependencies::default();
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
        layout: super::NativeName::layout_of(path),
        document,
    })
}

/// One session's read for archiving: its metadata, message count, IR payload when projected,
/// and the verified baseline.
type Rendered = (crate::Session, usize, Option<Vec<u8>>, Option<crate::source::Baseline>);

/// The bytes `[from, to)` a native capture appends to the chain `prior`, and the checkpoint of
/// the prefix through `to` (full-file BLAKE3 when `native.document == true`).
struct Delta {
    from: u64,
    to: u64,
    prior: Vec<PayloadAddr>,
    checkpoint: SourceCheckpoint,
    file_id: Option<crate::source::FileId>,
}

/// The byte range a native capture must add: `None` when the archived chain already holds the
/// exact bytes on disk. Documents (`.opensession.json`) are hashed whole via `blake3_hex_reader`
/// and recaptured whole whenever changed. Append-only JSONL requires `(dev, ino)` continuity and
/// strict growth (`*total_bytes < to`) to extend `extents`; a same-length stat bump (`touch`) on
/// the same inode skips only when the full `[0..to)` BLAKE3 matches the archived chain, while
/// any inode change (atomic temp-file rename), rewrite, or truncation starts a fresh chain at 0.
fn native_delta(
    packs_dir: &Path,
    key: &crate::SessionKey,
    native: &NativeFile,
    prev: Option<&ArchivedSessionEntry>,
) -> Result<Option<Delta>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let path = &native.path;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = file.metadata()?;
    let file_id = crate::source::FileId::of(&meta);
    if native.document {
        let (to, doc_blake3) = crate::blake3_hex_reader(&file)?;
        anyhow::ensure!(to > 0, "empty native document {}", path.display());
        if let Some(prev) = prev {
            if let CustodyKind::NativeExtents {
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
        Some(CustodyKind::NativeExtents {
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
                let (_, live_blake3) = crate::blake3_hex_reader(f.take(to))?;
                let archived_matches = if extents.len() == 1 {
                    extents[0].digest_hex() == live_blake3
                } else {
                    super::ExtentChain::open(packs_dir, key, extents)
                        .ok()
                        .and_then(|c| {
                            crate::blake3_hex_reader(super::ExtentChainReader::new(std::sync::Arc::new(c))).ok()
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
            let mut entry_val = serde_json::to_value(&c.entry)?;
            if let (Some(obj), Some(meta)) = (entry_val.as_object_mut(), c.envelope.get("metadata")) {
                if let Some(df) = meta.get("derived_from").and_then(serde_json::Value::as_str) {
                    obj.insert("derived_from".into(), json!(df));
                }
                if let Some(dk) = meta.get("derived_kind").and_then(serde_json::Value::as_str) {
                    obj.insert("derived_kind".into(), json!(dk));
                }
            }
            run_commits.push(json!({
                "local_commit_pos": c.local_commit_pos,
                "producer_id": c.entry.producer_id,
                "epoch": c.epoch,
                "seq": c.seq,
                "op_id": c.op_id,
                "envelope": c.envelope,
                "payload_addr": c.entry.addr,
            }));
            run_entries.push(entry_val);
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

type WatchBaselineMap = HashMap<(Harness, String, PathBuf), crate::source::Baseline>;

/// Source certificates used to select an archive watch poll.
#[derive(Default)]
pub struct WatchBaselines {
    seen: WatchBaselineMap,
}

impl WatchBaselines {
    /// Seed only from a committed capture or a verified read. A writer can append between the
    /// initial sync and watch startup, so sampling the current bytes here could skip that append.
    pub fn after_sync(archive: &Archive, refs: &[SessionRef], summary: &SyncSummary) -> Result<Self> {
        let known = super::head_entries(&archive.home)?;
        let mut seen = WatchBaselineMap::with_capacity(refs.len());
        for r in refs {
            if let Some(base) = known.get(&r.session_selector()).and_then(|e| e.baseline.as_ref()) {
                seen.insert((r.harness, r.id.clone(), r.path.clone()), base.clone());
            }
        }
        let mut watch = Self { seen };
        watch.reconcile(refs, summary);
        Ok(watch)
    }

    /// Sync changed sessions and retain only certificates consistent with the outcome.
    pub fn poll(&mut self, archive: &Archive, refs: Vec<SessionRef>) -> Result<SyncSummary> {
        let candidates = self.candidates(refs);
        if candidates.is_empty() {
            return Ok(SyncSummary::default());
        }
        match archive.sync_refs(&candidates) {
            Ok(summary) => {
                self.reconcile(&candidates, &summary);
                Ok(summary)
            }
            Err(error) => {
                for r in candidates {
                    self.seen.remove(&(r.harness, r.id, r.path));
                }
                Err(error)
            }
        }
    }

    fn candidates(&mut self, refs: Vec<SessionRef>) -> Vec<SessionRef> {
        let seen = &mut self.seen;
        let mut candidates = Vec::new();
        for chunk in refs.chunks(PROBE_BATCH) {
            let mut probe = crate::source::Probe::default();
            for r in chunk {
                let key = (r.harness, r.id.clone(), r.path.clone());
                let now = probe.snapshot(r);
                if let Some(prev) = seen.get(&key) {
                    if prev.fresh(&now, &mut probe) || prev.rows_fresh(r, &now, &mut probe) {
                        continue;
                    }
                }
                let mut next = now.with_reads(&crate::source::ReadSet::default());
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

    fn reconcile(&mut self, candidates: &[SessionRef], summary: &SyncSummary) {
        let verified: HashMap<_, _> = summary
            .verified_baselines
            .iter()
            .map(|(h, selector, base)| ((h.as_str(), selector.as_str()), base))
            .collect();
        let archived: HashMap<_, _> = summary
            .archived_entries
            .iter()
            .map(|entry| {
                (
                    (entry.harness.as_str(), entry.id.as_str(), entry.profile.as_path()),
                    entry.baseline.as_ref(),
                )
            })
            .collect();
        let failed: HashSet<_> = summary.failed.iter().map(|(who, _)| who.as_str()).collect();
        for r in candidates {
            let key = (r.harness, r.id.clone(), r.path.clone());
            if !failed.is_empty() && failed.contains(format!("{}:{}", r.harness, r.id).as_str()) {
                self.seen.remove(&key);
                continue;
            }
            let local = crate::SessionKey::local(r);
            let identity = (r.harness.as_str(), r.id.as_str(), local.profile.as_path());
            let baseline = archived.get(&identity).copied().or_else(|| {
                verified
                    .get(&(r.harness.as_str(), local.selector().as_str()))
                    .copied()
                    .map(Some)
            });
            match baseline {
                Some(Some(base)) if base.stamp.is_some() => {
                    self.seen.insert(key, base.clone());
                }
                Some(_) => {
                    self.seen.remove(&key);
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
