//! Read immutable local and replicated session projections through their committed manifests.
use crate::packs::{pack_idx_path, pack_zst_path, PackManifest as PackFileManifest, PayloadAddr, SealedPack};
use crate::{Adapter, Demand, Discovered, Harness, MessageSink, Session, SessionKey, SessionRef};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-session custody metadata persisted inside `pack_manifests.index_meta["sessions"]` and
/// backed by the companion `.pack.idx.json` `RecordIndex`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchivedSessionEntry {
    pub producer_id: String,
    #[serde(default)]
    pub profile: PathBuf,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<crate::source::Baseline>,
    pub addr: PayloadAddr,
}

fn verify_archive_span(index: &crate::packs::RecordIndex, key: &SessionKey, addr: &PayloadAddr) -> Result<()> {
    let anchor = if key.profile.as_os_str().is_empty() {
        format!("{}:{}:{}", key.producer, key.harness, key.native_id)
    } else {
        key.selector()
    };
    // Packs written before keyed anchors name records differently; accept the record at the
    // entry's exact address, which the span check below still pins to the same digest.
    let span = index
        .get(&anchor)
        .or_else(|| index.record_at_offset(addr.decoded_offset))
        .with_context(|| format!("missing archive record {anchor} in {}", addr.pack_id))?;
    anyhow::ensure!(
        span.blake3_digest == addr.blake3_digest
            && span.decoded_offset == addr.decoded_offset
            && span.length == addr.length,
        "pack {} record span mismatch for {anchor}",
        addr.pack_id
    );
    Ok(())
}

pub type ArchiveSessionMap = BTreeMap<String, ArchivedSessionEntry>;
impl ArchivedSessionEntry {
    fn key(&self) -> Result<SessionKey> {
        Ok(SessionKey {
            producer: self.producer_id.clone(),
            profile: self.profile.clone(),
            harness: Harness::parse(&self.harness).context("unknown archived harness")?,
            native_id: self.id.clone(),
        })
    }
}

/// Read-only map of session selectors to their latest archived revision across
/// all published `pack_manifests` (ascending commit order so newer packs supersede older ones).
/// Also validates any present `.pack.idx.json` record spans. Safe under `--frozen`.
pub fn archive_entries(home: &Path) -> Result<(ArchiveSessionMap, usize)> {
    committed_entries(home, true)
}

fn committed_entries(home: &Path, verify_indexes: bool) -> Result<(ArchiveSessionMap, usize)> {
    let manifests = crate::list_pack_manifests_frozen_safe(home).context("reading pack manifests from state.db")?;
    let packs_dir = home.join("packs");
    let mut distinct_packs = std::collections::BTreeSet::new();
    let mut idx_cache: std::collections::HashMap<String, Option<PackFileManifest>> = std::collections::HashMap::new();
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
            for mut entry in entries {
                if entry.addr.pack_id.is_empty() {
                    entry.addr.pack_id = phys_pack_id.clone();
                }
                let target_pack_id = &entry.addr.pack_id;
                anyhow::ensure!(
                    entry.content_blake3 == entry.addr.digest_hex(),
                    "archive digest mismatch for {}",
                    entry.id
                );
                if verify_indexes && !idx_cache.contains_key(target_pack_id) {
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
                    verify_archive_span(&idx.record_index, &entry.key()?, &entry.addr)?;
                }
                let key = entry.key()?.selector();
                by_key.insert(key, entry);
            }
        }
    }
    Ok((by_key, distinct_packs.len()))
}

pub fn read_session_bytes(home: &Path, harness: &str, id: &str) -> Result<Option<Vec<u8>>> {
    let entries = archive_entries(home)?.0;
    let mut matches = entries
        .iter()
        .filter(|(key, e)| e.harness == harness && (e.id == id || key.as_str() == id));
    let Some((_, entry)) = matches.next() else {
        return Ok(None);
    };
    anyhow::ensure!(matches.next().is_none(), "ambiguous archived session {id}");
    Ok(Some(
        SealedPack::open(&home.join("packs"), &entry.addr.pack_id)?.read_payload(&entry.addr)?,
    ))
}

pub struct Backing {
    pub reference: SessionRef,
    key: SessionKey,
    directory: PathBuf,
    addr: PayloadAddr,
    source: &'static str,
}
impl Adapter for Backing {
    fn harness(&self) -> Harness {
        self.key.harness
    }
    fn roots(&self) -> Vec<PathBuf> {
        vec![]
    }
    fn discover(&self) -> Result<Discovered> {
        Ok(Default::default())
    }
    fn stream(&self, _: &SessionRef, _: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        let pack = SealedPack::open(&self.directory, &self.addr.pack_id)?;
        if self.source == "custody_pack" {
            verify_archive_span(pack.record_index(), &self.key, &self.addr)?;
        }
        let bytes = pack.read_payload(&self.addr)?;
        let mut session = crate::state::hub::snapshot::decode_packed_session(&bytes)?;
        anyhow::ensure!(
            session.id == self.key.native_id && session.harness == self.key.harness,
            "pack record identity mismatch"
        );
        session.source_path = Some(self.reference.path.clone());
        let mut coverage = crate::coverage::ReadCoverage::of_session(&session).unwrap_or_default();
        coverage.sources.insert(self.source.to_string());
        coverage.attach(&mut session);
        crate::stream::replay(session, sink)
    }
}

pub(crate) fn or_archived(
    live: Option<crate::FoundSession>,
    mut rows: Vec<Backing>,
    id: &str,
) -> Result<Option<crate::FoundSession>> {
    if live.is_some() {
        return Ok(live);
    }
    if rows.iter().any(|b| b.reference.id == id || b.key.native_id == id) {
        rows.retain(|b| b.reference.id == id || b.key.native_id == id);
    }
    anyhow::ensure!(
        rows.len() <= 1,
        "ambiguous session {id}; select one of {}",
        rows.iter()
            .map(|b| b.reference.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(rows
        .pop()
        .map(|b| (b.reference.clone(), Box::new(b) as Box<dyn Adapter>)))
}

pub fn inventory() -> Result<Vec<Backing>> {
    let Some(home) = crate::env::Env::current().cv_home() else {
        return Ok(vec![]);
    };
    if !home.join("state.db").try_exists()? {
        return Ok(vec![]);
    }
    let mut rows = BTreeMap::new();
    let hub = crate::state::hub::HubStore::at_home(&home);
    if let Some(manifest) = hub.get_snapshot(None)? {
        for entry in manifest.session_revisions.into_values() {
            let Some(addr) = entry.payload_addr else { continue };
            let Some(harness) = Harness::parse(&entry.harness) else {
                continue;
            };
            let key = SessionKey {
                producer: entry.producer_id,
                profile: entry
                    .metadata
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
                harness,
                native_id: entry
                    .metadata
                    .get("native_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or(entry.session_id),
            };
            let selector = key.selector();
            let directory = hub.packs_dir().to_owned();
            let reference = SessionRef {
                id: selector.clone(),
                harness,
                path: pack_zst_path(&directory, &addr.pack_id)?,
                cwd: entry.cwd.map(Into::into),
                title: entry.title,
                created_at: entry.created_at.and_then(|t| t.parse().ok()),
                updated_at: entry.updated_at.and_then(|t| t.parse().ok()),
                message_count: usize::try_from(entry.message_count).ok(),
            };
            rows.insert(
                selector,
                Backing {
                    reference,
                    key,
                    directory,
                    addr,
                    source: "hub_snapshot",
                },
            );
        }
    }
    for (selector, e) in committed_entries(&home, false)?.0 {
        let key = e.key()?;
        let directory = home.join("packs");
        let reference = SessionRef {
            id: selector.clone(),
            harness: key.harness,
            path: pack_zst_path(&directory, &e.addr.pack_id)?,
            cwd: e.cwd.map(Into::into),
            title: e.title,
            created_at: e.created_at,
            updated_at: e.updated_at,
            message_count: Some(e.message_count),
        };
        rows.insert(
            selector,
            Backing {
                reference,
                key,
                directory,
                addr: e.addr,
                source: "custody_pack",
            },
        );
    }
    Ok(rows.into_values().collect())
}

pub fn backing_for(r: &SessionRef) -> Result<Option<Box<dyn Adapter>>> {
    if !is_pack_ref(r) {
        return Ok(None);
    }
    let backing = inventory()?
        .into_iter()
        .find(|b| b.reference.id == r.id && b.reference.path == r.path)
        .context("custody manifest no longer contains the selected revision")?;
    Ok(Some(Box::new(backing)))
}

pub fn is_pack_ref(r: &SessionRef) -> bool {
    SessionKey::split_selector(&r.id).is_some() && r.path.to_string_lossy().ends_with(".pack.zst")
}
