//! Coherent Fleet Hub: privacy-scrubbing replication shipper, pack-verifying receiver,
//! gap-free `(producer, epoch)` origin frontiers, loud conflict quarantine, and atomic
//! [`SnapshotPublicationManifest`] pinned fleet queries.

pub mod receiver;
pub mod shipper;
pub mod snapshot;

use super::{EventId, Producer, StateError, StateStore};
use crate::packs::PayloadAddr;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use receiver::{
    EnrollRequest, EnrollResponse, HubCustodyWatermark, IngestBatchRequest, IngestBatchResponse, IngestEventRecord,
    MirroredPackRecord, UploadPackResponse,
};
pub use shipper::{
    capture_discovered_sessions, capture_session_to_store, resolve_replication_profile, seal_local_inbox_to_pack,
    HttpClient, RawHttpResponse, ShipConfig, ShipReport, Shipper, TransportDeliveryAck,
};
pub use snapshot::{
    FrontierStatus, HubSessionRow, HubSessionsPage, HubSnapshotResponse, HubStatusResponse, HydratedHubSession,
    QuarantineRecord, QuarantineSummary, SessionRevisionEntry, SnapshotPublicationManifest,
};

/// Relative subdirectory under `$CLUSTERVISION_HOME` holding mirrored hub packs.
pub const HUB_PACKS_REL_DIR: &str = "hub/packs";

/// Content-Type for streaming binary pack uploads (`[idx_bytes][zst_bytes]`).
pub const PACK_STREAM_CONTENT_TYPE: &str = "application/vnd.cv.pack+octet-stream";

/// Detailed quarantine payload carried by [`HubError::EventConflict`].
#[derive(Debug, Clone)]
pub struct EventConflictDetail {
    pub event_id: EventId,
    pub existing_digest: String,
    pub conflicting_digest: String,
    pub origin_frontier: BTreeMap<(String, String), u64>,
    pub quarantine_count: u64,
}

impl std::fmt::Display for EventConflictDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "conflicting digest for event {}/{}/#{}: committed `{}` != incoming `{}` (quarantined)",
            self.event_id.producer_id,
            self.event_id.epoch,
            self.event_id.seq,
            self.existing_digest,
            self.conflicting_digest
        )
    }
}

/// Typed errors produced by Fleet Hub receiver, snapshotter, and shipper operations.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error(transparent)]
    State(#[from] StateError),

    #[error("invalid hub request: {0}")]
    InvalidArgument(String),

    #[error("{0}")]
    NotFound(String),

    #[error("missing or unverified pack dependency `{pack_id}`: {detail}")]
    MissingPackDependency { pack_id: String, detail: String },

    #[error("conflicting digest for pack `{pack_id}`: committed `{existing_digest}` != incoming `{incoming_digest}`")]
    PackConflict {
        pack_id: String,
        existing_digest: String,
        incoming_digest: String,
    },

    #[error("{0}")]
    EventConflict(Box<EventConflictDetail>),

    #[error("hub pack verification failed: {0}")]
    Pack(String),

    #[error("hub HTTP transport error (status={status:?}): {message}")]
    Transport { status: Option<u16>, message: String },

    #[error("hub I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("hub JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        Self::State(StateError::Sqlite(e))
    }
}

/// Handle to a Fleet Hub's authoritative `state.db` and mirrored `hub/packs/` directory.
#[derive(Debug, Clone)]
pub struct HubStore {
    store: StateStore,
    packs_dir: PathBuf,
}

impl HubStore {
    /// Open a handle to `$CLUSTERVISION_HOME/state.db` + `$CLUSTERVISION_HOME/hub/packs`.
    pub fn default_hub() -> Result<Self, HubError> {
        let home = crate::env::Env::current().cv_home().ok_or(StateError::NoHome)?;
        Ok(Self::at_home(home))
    }

    /// Open a handle rooted at an explicit `cv_home` directory.
    pub fn at_home(cv_home: impl AsRef<Path>) -> Self {
        let home = cv_home.as_ref();
        Self {
            store: StateStore::at_dir(home),
            packs_dir: home.join("hub").join("packs"),
        }
    }

    /// Construct from an explicit [`StateStore`] and mirrored `packs_dir`.
    pub fn new(store: StateStore, packs_dir: impl Into<PathBuf>) -> Self {
        Self {
            store,
            packs_dir: packs_dir.into(),
        }
    }

    pub fn state_store(&self) -> &StateStore {
        &self.store
    }

    pub fn packs_dir(&self) -> &Path {
        &self.packs_dir
    }

    /// Enroll or update a producer identity in `producers`.
    pub fn enroll_producer(&self, producer_id: &str, host_label: &str, metadata: &Value) -> Result<Producer, HubError> {
        Ok(self.store.enroll_producer(producer_id, host_label, metadata)?)
    }
}

/// Bijective human-readable encoding for `(first, second)` map keys (`"<escaped_first>/<second>"`).
///
/// Only `%` (`%25`) and `/` (`%2F`) in `first` are escaped, so ordinary identifiers serialize as
/// literal `"producer/epoch"` or `"producer/session_id"` while arbitrary strings round-trip losslessly.
pub fn encode_pair_key(first: &str, second: &str) -> String {
    let mut out = String::with_capacity(first.len() + 1 + second.len());
    for ch in first.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            other => out.push(other),
        }
    }
    out.push('/');
    out.push_str(second);
    out
}

/// Inverse of [`encode_pair_key`].
pub fn decode_pair_key(encoded: &str) -> Result<(String, String), String> {
    let Some((raw_first, second)) = encoded.split_once('/') else {
        return Err(format!("missing `/` separator in pair key {encoded:?}"));
    };
    let mut first = String::with_capacity(raw_first.len());
    let bytes = raw_first.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(format!("truncated percent escape in pair key {encoded:?}"));
            }
            match &raw_first[i..i + 3] {
                "%25" => first.push('%'),
                "%2f" | "%2F" => first.push('/'),
                other => return Err(format!("unsupported escape {other:?} in pair key {encoded:?}")),
            }
            i += 3;
        } else {
            let s = &raw_first[i..];
            let ch = s.chars().next().expect("non-empty UTF-8 slice");
            first.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok((first, second.to_string()))
}

/// Produce canonical key-sorted JSON bytes so digest commitments are invariant to map insertion order.
pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    fn canonicalize(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
                for (k, child) in map {
                    sorted.insert(k.clone(), canonicalize(child));
                }
                let mut out = Map::with_capacity(sorted.len());
                for (k, child) in sorted {
                    out.insert(k, child);
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&canonicalize(value))
}

/// Compute a canonical 64-hex BLAKE3 commitment for an ingested event's `(op_id, envelope, payload_addr)`.
pub fn compute_event_digest(
    op_id: &str,
    envelope: &Value,
    payload_addr: Option<&PayloadAddr>,
) -> Result<String, serde_json::Error> {
    let payload_val = match payload_addr {
        Some(addr) => serde_json::to_value(addr)?,
        None => Value::Null,
    };
    let doc = serde_json::json!({
        "op_id": op_id,
        "envelope": envelope,
        "payload_addr": payload_val,
    });
    let bytes = canonical_json_bytes(&doc)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub(crate) mod origin_frontier_serde {
    use super::{decode_pair_key, encode_pair_key};
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        frontier: &BTreeMap<(String, String), u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(frontier.len()))?;
        for ((producer, epoch), seq) in frontier {
            map.serialize_entry(&encode_pair_key(producer, epoch), seq)?;
        }
        map.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<(String, String), u64>, D::Error> {
        let raw: BTreeMap<String, u64> = BTreeMap::deserialize(deserializer)?;
        let mut out = BTreeMap::new();
        for (key, seq) in raw {
            let pair = decode_pair_key(&key).map_err(serde::de::Error::custom)?;
            out.insert(pair, seq);
        }
        Ok(out)
    }
}
