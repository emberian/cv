//! Configurable privacy-scrubbing replication shipper (`cv_core::state::hub::shipper`).
//!
//! Reads committed local `StateStore` `capture_inbox` rows and sealed `pack_manifests`, passes
//! every session and event envelope through the target's resolved [`ScrubProfile`] (`[replication.<target>]`
//! falling back to `[privacy]`), re-packs scrubbed payloads into fresh Zstd-Seekable packs with
//! fresh BLAKE3 digests, enforces `publish-before-reference` (`upload_pack` BEFORE `ingest_batch`),
//! and distinguishes transport delivery acks ([`TransportDeliveryAck`]) from durable hub custody
//! watermarks ([`CustodyAck`]).

use super::receiver::{
    EnrollRequest, EnrollResponse, IngestBatchRequest, IngestBatchResponse, IngestEventRecord, UploadPackResponse,
};
use super::snapshot::ProducerLifecycle;
use super::{HubError, HubStore, PACK_STREAM_CONTENT_TYPE};
use crate::config::Config;
use crate::ir::{Harness, Session};
use crate::packs::{pack_idx_path, pack_zst_path, PackBuilder, PayloadAddr, SealedPack};
use crate::publish::Claim;
use crate::redact::{CompiledProfile, PrincipalPolicy, ScrubProfile, ScrubStats};
use crate::state::{
    tables::{CustodyAck, CustodyCommitRow, PackManifest as LocalStatePackManifest},
    AppendRequest, CaptureInboxRow, DurableReceipt, EventId, StateStore,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Maximum uncompressed payload bytes coalesced into a single outbound `.pack.zst` (8 MiB).
const MAX_OUTBOUND_PACK_BYTES: usize = 8 * 1024 * 1024;
/// Maximum commits processed per chunked sync iteration (`<= 8 MiB` JSON batch).
const MAX_COMMITS_PER_BATCH: usize = 2048;
/// Magic header for `cv --frozen archive sync --export-delta` binary stdio stream.
const DELTA_STREAM_MAGIC: &[u8; 8] = b"CVDLT001";

struct TempStageDir(PathBuf);

impl TempStageDir {
    fn create_in(parent: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        let dir = parent.join(format!(".ship-stage-{}", uuid::Uuid::new_v4().simple()));
        #[cfg(unix)]
        crate::state::ensure_private_dir_0700(&dir).map_err(std::io::Error::other)?;
        #[cfg(not(unix))]
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempStageDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn normalize_hub_url_key(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// Resolve the [`ScrubProfile`] for replication target `target` (`[replication.<target>]` by name
/// or matching `url`), inheriting unset fields from `[privacy]` and falling back to `[privacy]`
/// when no target-specific profile matches.
pub fn resolve_replication_profile(cfg: &Config, target: &str) -> ScrubProfile {
    let trimmed = target.trim();
    let matched = cfg.replication.get_key_value(trimmed).or_else(|| {
        if trimmed.is_empty() {
            return None;
        }
        let norm_target = normalize_hub_url_key(trimmed);
        cfg.replication.iter().find(|(_, prof)| {
            prof.url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty() && normalize_hub_url_key(u) == norm_target)
        })
    });

    match matched {
        Some((name, prof)) => prof.inherit_from(&cfg.privacy, cfg.replication_overrides.get(name)),
        None => cfg.privacy.clone(),
    }
}

/// Stage-1 transport delivery acknowledgement (HTTP response received over TCP; NOT yet a durable
/// hub custody watermark unless verified by stage-2 hub custody checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportDeliveryAck {
    pub endpoint: String,
    pub status: u16,
    pub response_bytes: usize,
    pub received_at: i64,
}

/// Raw HTTP/1.1 response returned by [`HttpClient`].
#[derive(Debug, Clone)]
pub struct RawHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawHttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, HubError> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// Synchronous HTTP/HTTPS transport. TLS verifies certificates against Mozilla roots.
/// Redirects and environment proxies are disabled: credentials and replication evidence go
/// only to the explicitly configured hub. Protocol framing belongs to ureq.
#[derive(Debug, Clone)]
pub struct HttpClient {
    base_url: String,
    bearer_token: Option<String>,
    agent: ureq::Agent,
}

const MAX_HUB_HEADER_BYTES: usize = 64 * 1024;
const MAX_HUB_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
// Bulk packs may take longer than a socket's idle budget on a healthy slower link.
// Keep the former 15s no-progress bound, plus a five-minute total operation bound.
const HUB_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const HUB_TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

fn hub_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_per_read(Some(HUB_IDLE_TIMEOUT))
        .timeout_per_write(Some(HUB_IDLE_TIMEOUT))
        .timeout_resolve(Some(HUB_IDLE_TIMEOUT))
        .timeout_connect(Some(HUB_IDLE_TIMEOUT))
        .timeout_send_request(Some(HUB_IDLE_TIMEOUT))
        .timeout_recv_response(Some(HUB_IDLE_TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .max_response_header_size(MAX_HUB_HEADER_BYTES)
        .build()
        .into()
}

impl HttpClient {
    pub fn new(base_url: &str, bearer_token: Option<String>) -> Result<Self, HubError> {
        let uri: ureq::http::Uri = base_url
            .trim()
            .parse()
            .map_err(|_| HubError::InvalidArgument("invalid hub URL".into()))?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.host().is_none_or(str::is_empty)
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || uri.query().is_some()
            || base_url.contains('#')
        {
            return Err(HubError::InvalidArgument(
                "hub URL must use http:// or https:// with a host and optional path, without credentials, query, or fragment".into(),
            ));
        }
        Ok(Self {
            base_url: uri.to_string().trim_end_matches('/').to_string(),
            bearer_token: bearer_token.filter(|t| !t.is_empty()),
            agent: hub_agent(HUB_TOTAL_TIMEOUT),
        })
    }

    /// Total deadline including DNS, TLS, upload, response headers, and body consumption.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.agent = hub_agent(timeout);
        self
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn get(&self, path: &str) -> Result<(RawHttpResponse, TransportDeliveryAck), HubError> {
        self.send("GET", path, None, &[], &[])
    }

    pub fn post_json<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(RawHttpResponse, TransportDeliveryAck), HubError> {
        let payload = serde_json::to_vec(body)?;
        self.send("POST", path, Some("application/json"), &[], &payload)
    }

    pub fn send(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(RawHttpResponse, TransportDeliveryAck), HubError> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let mut request = ureq::http::Request::builder().method(method).uri(&url);
        if let Some(token) = &self.bearer_token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(ct) = content_type {
            request = request.header("Content-Type", ct);
        }
        for (key, value) in extra_headers {
            request = request.header(*key, *value);
        }
        let request = request
            .body(body)
            .map_err(|_| HubError::InvalidArgument("invalid hub request".into()))?;
        let endpoint = format!(
            "{method} {}",
            request.uri().path_and_query().map_or("/", |p| p.as_str())
        );
        let mut response = self.agent.run(request).map_err(|e| HubError::Transport {
            status: None,
            message: e.to_string(),
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
            .collect();
        // Reject an oversized declared body before reading; also cap decoded chunked/EOF bodies.
        if response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > MAX_HUB_RESPONSE_BYTES as u64)
        {
            return Err(HubError::Transport {
                status: Some(status),
                message: "hub response exceeds 64 MiB".into(),
            });
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_HUB_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|e| HubError::Transport {
                status: Some(status),
                message: e.to_string(),
            })?;
        let ack = TransportDeliveryAck {
            endpoint,
            status,
            response_bytes: body.len(),
            received_at: Utc::now().timestamp_millis(),
        };
        Ok((RawHttpResponse { status, headers, body }, ack))
    }
}

/// Configuration for a [`Shipper`] targeting one Fleet Hub.
#[derive(Debug, Clone)]
pub struct ShipConfig {
    /// Logical replication target key (resolves `[replication.<target_name>]` in `config.toml`,
    /// and serves as `custody_acks.target_hub`).
    pub target_name: String,
    /// Hub base URL (`http://host:port` or verified `https://host`).
    pub hub_url: String,
    /// Optional `Authorization: Bearer <token>` credential.
    pub bearer_token: Option<String>,
    /// Producer identity to enroll (`producers.producer_id`).
    pub producer_id: String,
    /// Human-readable host label enrolled on the hub.
    pub host_label: String,
    /// Producer metadata enrolled on the hub.
    pub producer_metadata: Value,
    /// Explicit privacy/scrubbing profile (when `None`, resolved via [`resolve_replication_profile`]
    /// from [`crate::config::load`]).
    pub profile: Option<ScrubProfile>,
    /// When `true`, re-ships events even if `custody_acks` already covers them (for replay/idempotency tests).
    pub force_reship: bool,
}

impl ShipConfig {
    pub fn new(target_name: impl Into<String>, hub_url: impl Into<String>, producer_id: impl Into<String>) -> Self {
        Self {
            target_name: target_name.into(),
            hub_url: hub_url.into(),
            bearer_token: None,
            producer_id: producer_id.into(),
            host_label: String::new(),
            producer_metadata: json!({}),
            profile: None,
            force_reship: false,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn with_host_label(mut self, label: impl Into<String>) -> Self {
        self.host_label = label.into();
        self
    }

    pub fn with_profile(mut self, profile: ScrubProfile) -> Self {
        self.profile = Some(profile);
        self
    }
}

/// Summary of a completed [`Shipper::sync`] run, separating Stage-1 [`TransportDeliveryAck`]s from
/// Stage-2 durable [`CustodyAck`]s recorded in local `state.db`.
#[derive(Debug, Clone, Default)]
pub struct ShipReport {
    pub events_examined: usize,
    pub events_shipped: usize,
    pub sessions_admitted: usize,
    pub sessions_filtered_out: usize,
    pub redactions_applied: usize,
    pub uploaded_pack_id: Option<String>,
    pub uploaded_pack_digest: Option<String>,
    pub transport_acks: Vec<TransportDeliveryAck>,
    pub durable_custody_acks: Vec<CustodyAck>,
    pub hub_snapshot_id: Option<String>,
    pub hub_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SealedCommitEntry {
    local_commit_pos: i64,
    producer_id: String,
    epoch: String,
    seq: u64,
    op_id: String,
    envelope: Value,
    payload_addr: Option<PayloadAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delta_extents: Option<Vec<PayloadAddr>>,
}

impl SealedCommitEntry {
    fn from_commit_row(r: CustodyCommitRow) -> Self {
        Self {
            local_commit_pos: r.local_commit_pos,
            producer_id: r.producer_id,
            epoch: r.epoch,
            seq: r.seq,
            op_id: r.op_id,
            envelope: r.envelope,
            payload_addr: r.payload_addr,
            delta_extents: r.delta_extents,
        }
    }
}

fn session_envelope(session: &Session) -> Value {
    let msg_count = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, crate::ir::Role::User | crate::ir::Role::Assistant))
        .count() as u64;
    let mut env = json!({
        "kind": "session_upsert",
        "session_id": session.id,
        "harness": session.harness.as_str(),
        "title": session.title,
        "cwd": session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "model": session.model,
        "created_at": session.created_at.map(|t| t.to_rfc3339()),
        "updated_at": session.updated_at.map(|t| t.to_rfc3339()),
        "message_count": msg_count,
    });
    if let Some(path) = &session.source_path {
        let reference = crate::SessionRef::new(&session.id, session.harness, path);
        env["profile"] = json!(crate::SessionKey::local(&reference).profile);
    }
    if let Some(cv_meta) = session.extra.get("cv") {
        if let Some(obj) = env.as_object_mut() {
            obj.insert("metadata".into(), json!({ "cv": cv_meta }));
        }
    }
    env
}

/// Capture one parsed [`Session`] into local `store` (`capture_inbox`) and (when
/// `include_pack_payload` is `true`) a local sealed Zstd-Seekable pack under `local_packs_dir`.
pub fn capture_session_to_store(
    store: &StateStore,
    local_packs_dir: &Path,
    session: &Session,
    include_pack_payload: bool,
) -> Result<DurableReceipt, HubError> {
    let mut owned = session.clone();
    owned
        .materialize()
        .map_err(|e| HubError::Pack(format!("materializing session `{}`: {e:#}", session.id)))?;

    let payload_ref = if include_pack_payload {
        std::fs::create_dir_all(local_packs_dir)?;
        let session_bytes = serde_json::to_vec(&owned)?;
        let content_hex = blake3::hash(&session_bytes).to_hex();
        let pack_id = format!("local-{}", &content_hex.as_str()[..24]);
        let zst_path = pack_zst_path(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let anchor = format!("session:{}:{}", owned.harness.as_str(), owned.id);
        let addr = if zst_path.is_file() {
            let existing = SealedPack::open(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            if let Some(span) = existing.record_index().records().first() {
                span.clone().into_addr(pack_id)
            } else {
                return Err(HubError::Pack("empty local session pack".into()));
            }
        } else {
            let mut builder =
                PackBuilder::create(local_packs_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            let addr = builder
                .append_record(anchor, &session_bytes)
                .map_err(|e| HubError::Pack(format!("{e:#}")))?;
            builder.seal(Claim::New).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            addr
        };
        Some(serde_json::to_string(&addr)?)
    } else {
        None
    };

    let envelope = session_envelope(&owned);
    let canonical = super::canonical_json_bytes(&json!({
        "envelope": &envelope,
        "payload_ref": &payload_ref,
    }))?;
    let req_digest = blake3::hash(&canonical).to_hex().to_string();
    let op_id = format!("capture:{}:{}:{}", owned.harness.as_str(), owned.id, &req_digest[..16]);

    let mut req = AppendRequest::new(op_id, req_digest, envelope);
    if let Some(pref) = payload_ref {
        req = req.with_payload_ref(pref);
    }
    Ok(store.append_request(&req)?)
}

/// Discover all sessions visible in the current environment and capture each into `store` +
/// `local_packs_dir`.
pub fn capture_discovered_sessions(
    store: &StateStore,
    local_packs_dir: &Path,
) -> Result<Vec<DurableReceipt>, HubError> {
    let discovered = crate::discover_all();
    let mut receipts = Vec::with_capacity(discovered.len());
    for sref in discovered {
        let Some(adapter) = crate::harness::for_harness(sref.harness) else {
            continue;
        };
        let session = adapter
            .parse(&sref)
            .map_err(|e| HubError::Pack(format!("parsing discovered session `{}`: {e:#}", sref.id)))?;
        receipts.push(capture_session_to_store(store, local_packs_dir, &session, true)?);
    }
    Ok(receipts)
}

/// Seal all currently unpacked rows in `store.capture_inbox` into consolidated local
/// Zstd-Seekable pack(s) under `local_packs_dir` (one pack per maximal contiguous
/// `local_commit_pos` sub-run so straddling an already-published pack never fails) and atomically
/// transfer ownership via [`StateStore::publish_pack_range`].
pub fn seal_local_inbox_to_pack(
    store: &StateStore,
    local_packs_dir: &Path,
    explicit_pack_id: Option<&str>,
) -> Result<Option<LocalStatePackManifest>, HubError> {
    let rows = store.read_inbox_range(0, i64::MAX)?;
    if rows.is_empty() {
        return Ok(None);
    }

    std::fs::create_dir_all(local_packs_dir)?;

    let mut runs: Vec<Vec<CaptureInboxRow>> = Vec::new();
    for row in rows {
        if let Some(last_run) = runs.last_mut() {
            if let Some(prev) = last_run.last() {
                if row.local_commit_pos == prev.local_commit_pos + 1 {
                    last_run.push(row);
                    continue;
                }
            }
        }
        runs.push(vec![row]);
    }

    let mut last_manifest = None;
    for (run_idx, run) in runs.into_iter().enumerate() {
        let min_pos = run.first().expect("non-empty").local_commit_pos;
        let max_pos = run.last().expect("non-empty").local_commit_pos;
        let pack_id = match explicit_pack_id {
            Some(id) => resolve_explicit_run_pack_id(store, id, run_idx, min_pos, max_pos)?,
            None => format!(
                "inbox-{min_pos}-{max_pos}-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ),
        };

        let mut builder =
            PackBuilder::create(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let mut entries = Vec::with_capacity(run.len());

        for row in &run {
            let new_addr = if let Some(ref pref) = row.payload_ref {
                let old_addr: PayloadAddr = serde_json::from_str(pref)?;
                let old_pack = SealedPack::open(local_packs_dir, &old_addr.pack_id)
                    .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                let bytes = old_pack
                    .read_payload(&old_addr)
                    .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                let anchor = format!("commit:{}", row.local_commit_pos);
                Some(
                    builder
                        .append_record(anchor, &bytes)
                        .map_err(|e| HubError::Pack(format!("{e:#}")))?,
                )
            } else if let Some(inline_sess) = row.envelope.get("session") {
                let bytes = serde_json::to_vec(inline_sess)?;
                let anchor = format!("commit:{}", row.local_commit_pos);
                Some(
                    builder
                        .append_record(anchor, &bytes)
                        .map_err(|e| HubError::Pack(format!("{e:#}")))?,
                )
            } else {
                None
            };

            entries.push(SealedCommitEntry {
                local_commit_pos: row.local_commit_pos,
                producer_id: row.producer_id.clone(),
                epoch: row.epoch.clone(),
                seq: row.seq,
                op_id: row.op_id.clone(),
                envelope: row.envelope.clone(),
                payload_addr: new_addr,
                delta_extents: None,
            });
        }

        // Envelopes are retained evidence, not SQLite index metadata. One addressed record
        // preserves all commit entries; SQLite holds only its small verified locator.
        let commits_addr = builder
            .append_record("inbox-commits", &serde_json::to_vec(&entries)?)
            .map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let seal_claim = if store.get_pack_manifest(&pack_id)?.is_none() {
            Claim::Replace
        } else {
            Claim::New
        };
        let _sealed_manifest = builder.seal(seal_claim).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let zst_path = pack_zst_path(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let zst_bytes = std::fs::read(&zst_path)?;
        let pack_digest = blake3::hash(&zst_bytes).to_hex().to_string();

        let state_manifest = LocalStatePackManifest {
            pack_id,
            min_commit_pos: min_pos,
            max_commit_pos: max_pos,
            record_count: run.len() as u64,
            byte_len: zst_bytes.len() as u64,
            digest: pack_digest,
            pack_path: zst_path.to_string_lossy().into_owned(),
            index_meta: json!({ "commits_addr": commits_addr }),
            published_at: Utc::now().timestamp_millis(),
        };
        store.publish_pack_range(&state_manifest)?;
        last_manifest = Some(state_manifest);
    }

    Ok(last_manifest)
}

fn resolve_explicit_run_pack_id(
    store: &StateStore,
    base_id: &str,
    start_idx: usize,
    min_pos: i64,
    max_pos: i64,
) -> Result<String, HubError> {
    let mut idx = start_idx;
    loop {
        let candidate = if idx == 0 {
            base_id.to_string()
        } else {
            format!("{base_id}--r{idx}")
        };
        match store.get_pack_manifest(&candidate)? {
            None => return Ok(candidate),
            Some(existing) if existing.min_commit_pos == min_pos && existing.max_commit_pos == max_pos => {
                return Ok(candidate);
            }
            Some(_) => {
                idx += 1;
            }
        }
    }
}

/// Replication shipper client driving enrollment, privacy scrubbing, pack upload, batch ingestion,
/// and two-stage custody acknowledgement accounting.
#[derive(Debug, Clone)]
pub struct Shipper {
    cfg: ShipConfig,
    client: HttpClient,
}

impl Shipper {
    pub fn new(cfg: ShipConfig) -> Result<Self, HubError> {
        let client = HttpClient::new(&cfg.hub_url, cfg.bearer_token.clone())?;
        Ok(Self { cfg, client })
    }

    pub fn client(&self) -> &HttpClient {
        &self.client
    }

    pub fn config(&self) -> &ShipConfig {
        &self.cfg
    }

    /// Resolve the active [`ScrubProfile`] for this shipper's target.
    pub fn resolved_profile(&self) -> ScrubProfile {
        if let Some(ref explicit) = self.cfg.profile {
            return explicit.clone();
        }
        let loaded = crate::config::load();
        resolve_replication_profile(&loaded, &self.cfg.target_name)
    }

    /// Enroll this producer on the remote hub (`POST /api/hub/enroll`).
    pub fn enroll(&self) -> Result<(EnrollResponse, TransportDeliveryAck), HubError> {
        let profile = self.resolved_profile();
        let compiled = profile
            .compile()
            .map_err(|e| HubError::InvalidArgument(format!("invalid scrub profile: {e:#}")))?;
        let mut enroll_stats = ScrubStats::default();
        let repo_root = profile.repo_root.as_deref();
        let host_label = if !self.cfg.host_label.is_empty() {
            compiled
                .scrub_text_cow(&self.cfg.host_label, repo_root, &mut enroll_stats)
                .into_owned()
        } else {
            String::new()
        };
        let mut metadata = self.cfg.producer_metadata.clone();
        scrub_json_value_in_place(&mut metadata, &compiled, &profile, repo_root, &mut enroll_stats);
        attach_policy_metadata(&mut metadata, &self.cfg.producer_id, &profile);
        let req = EnrollRequest {
            producer_id: self.cfg.producer_id.clone(),
            host_label,
            metadata,
        };
        let (resp, ack) = self.client.post_json("/api/hub/enroll", &req)?;
        if resp.status != 200 {
            return Err(HubError::Transport {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).into_owned(),
            });
        }
        Ok((resp.json()?, ack))
    }

    /// Upload a sealed `.pack.idx.json` + `.pack.zst` to the hub (`POST /api/hub/upload_pack`)
    /// using the streaming binary wire format.
    pub fn upload_pack_files(
        &self,
        pack_dir: &Path,
        pack_id: &str,
    ) -> Result<(UploadPackResponse, TransportDeliveryAck), HubError> {
        let idx_path = pack_idx_path(pack_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let zst_path = pack_zst_path(pack_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
        let idx_bytes = std::fs::read(&idx_path)?;
        let zst_bytes = std::fs::read(&zst_path)?;
        let zst_digest = blake3::hash(&zst_bytes).to_hex().to_string();

        let mut payload = Vec::with_capacity(idx_bytes.len() + zst_bytes.len());
        payload.extend_from_slice(&idx_bytes);
        payload.extend_from_slice(&zst_bytes);

        let idx_len_str = idx_bytes.len().to_string();
        let headers = [
            ("X-Cv-Pack-Id", pack_id),
            ("X-Cv-Pack-Digest", zst_digest.as_str()),
            ("X-Cv-Pack-Idx-Len", idx_len_str.as_str()),
            ("X-Cv-Producer-Id", self.cfg.producer_id.as_str()),
        ];

        let (resp, ack) = self.client.send(
            "POST",
            "/api/hub/upload_pack",
            Some(PACK_STREAM_CONTENT_TYPE),
            &headers,
            &payload,
        )?;
        if resp.status != 200 {
            return Err(HubError::Transport {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).into_owned(),
            });
        }
        Ok((resp.json()?, ack))
    }

    /// Send a raw [`IngestBatchRequest`] to `POST /api/hub/ingest_batch`, returning the HTTP status
    /// in [`HubError::Transport`] on `409 Conflict` / `424 Failed Dependency` so callers and fault
    /// tests can inspect the structured JSON error body.
    pub fn send_ingest_batch(
        &self,
        batch: &IngestBatchRequest,
    ) -> Result<(IngestBatchResponse, TransportDeliveryAck), HubError> {
        let (resp, ack) = self.client.post_json("/api/hub/ingest_batch", batch)?;
        if resp.status != 200 {
            return Err(HubError::Transport {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).into_owned(),
            });
        }
        Ok((resp.json()?, ack))
    }

    /// Synchronize unsent committed local state (`pack_manifests` + `capture_inbox`) to the target
    /// hub through the `[replication.<target>]` privacy filter & scrubbing pipeline.
    ///
    /// Enforces `publish-before-reference` (uploads scrubbed outbound pack BEFORE sending
    /// `ingest_batch`) and **Two-Stage Ack Accounting** (`StateStore::record_custody_ack` is called
    /// ONLY after the hub verifies pack BLAKE3 digests + fsyncs the batch + returns durable
    /// `custody_acks`).
    pub fn sync(&self, store: &StateStore, local_packs_dir: &Path) -> Result<ShipReport, HubError> {
        let mut report = ShipReport::default();

        let (_, enroll_ack) = self.enroll()?;
        report.transport_acks.push(enroll_ack);

        // Seal any unpacked `capture_inbox` rows (e.g. hook receipts / lane events) into local
        // pack custody so `capture_inbox` drains to 0 once synced and `cv watch --to` remains idle.
        if !crate::env::frozen() && store.inbox_count()? > 0 {
            let _ = seal_local_inbox_to_pack(store, local_packs_dir, None)?;
        }

        let profile = self.resolved_profile();
        let compiled = profile
            .compile()
            .map_err(|e| HubError::InvalidArgument(format!("invalid scrub profile: {e:#}")))?;

        let mut existing_watermarks: BTreeMap<String, u64> = if self.cfg.force_reship {
            BTreeMap::new()
        } else {
            store
                .ship_cursors_for(&self.cfg.target_name)?
                .into_iter()
                .map(|cur| (cur.epoch, cur.contiguous_seq))
                .collect()
        };

        loop {
            let pending_rows: Vec<SealedCommitEntry> =
                match store.pending_custody_commits(&existing_watermarks, MAX_COMMITS_PER_BATCH)? {
                    Some(rows) => rows.into_iter().map(SealedCommitEntry::from_commit_row).collect(),
                    None => collect_local_commits(store, local_packs_dir)?
                        .into_iter()
                        .filter(|c| c.seq > existing_watermarks.get(&c.epoch).copied().unwrap_or(0))
                        .take(MAX_COMMITS_PER_BATCH)
                        .collect(),
                };

            if pending_rows.is_empty() {
                break;
            }
            report.events_examined += pending_rows.len();
            let fetched_len = pending_rows.len();

            let stage_dir = TempStageDir::create_in(local_packs_dir)?;
            let prepared_batch = prepare_and_pack_commits(
                &pending_rows,
                local_packs_dir,
                stage_dir.path(),
                &self.cfg.producer_id,
                &self.cfg.target_name,
                &profile,
                &compiled,
            )?;
            report.sessions_admitted += prepared_batch.sessions_admitted;
            report.sessions_filtered_out += prepared_batch.sessions_filtered_out;
            report.redactions_applied += prepared_batch.redactions_applied;

            let mut uploaded_packs: BTreeMap<String, String> = BTreeMap::new();
            for pack_id in &prepared_batch.pack_ids {
                let (upload_resp, upload_ack) = self.upload_pack_files(stage_dir.path(), pack_id)?;
                report.transport_acks.push(upload_ack);
                if !upload_resp.durable_fsynced {
                    return Err(HubError::Transport {
                        status: Some(200),
                        message: format!("hub did not confirm durable fsync for pack `{pack_id}`"),
                    });
                }
                report.uploaded_pack_id = Some(upload_resp.pack.pack_id.clone());
                report.uploaded_pack_digest = Some(upload_resp.pack.blake3_digest.clone());
                uploaded_packs.insert(upload_resp.pack.pack_id, upload_resp.pack.blake3_digest);
            }

            // Chunk wire events so no single HTTP POST exceeds 8 MiB JSON.
            let mut chunk_events = Vec::new();
            let mut chunk_bytes = 2usize;
            for ev in prepared_batch.wire_events {
                let ev_len = serde_json::to_vec(&ev)?.len() + 1;
                if !chunk_events.is_empty() && chunk_bytes + ev_len > MAX_OUTBOUND_PACK_BYTES {
                    self.submit_and_verify_chunk(
                        store,
                        chunk_events,
                        &uploaded_packs,
                        &mut existing_watermarks,
                        &mut report,
                    )?;
                    chunk_events = Vec::new();
                    chunk_bytes = 2;
                }
                chunk_bytes += ev_len;
                chunk_events.push(ev);
            }
            if !chunk_events.is_empty() {
                self.submit_and_verify_chunk(
                    store,
                    chunk_events,
                    &uploaded_packs,
                    &mut existing_watermarks,
                    &mut report,
                )?;
            }

            if self.cfg.force_reship || fetched_len < MAX_COMMITS_PER_BATCH {
                break;
            }
        }

        Ok(report)
    }

    fn submit_and_verify_chunk(
        &self,
        store: &StateStore,
        events: Vec<IngestEventRecord>,
        uploaded_packs: &BTreeMap<String, String>,
        existing_watermarks: &mut BTreeMap<String, u64>,
        report: &mut ShipReport,
    ) -> Result<(), HubError> {
        let referenced_in_chunk: BTreeSet<String> = events
            .iter()
            .flat_map(|ev| {
                let mut ids = Vec::new();
                if let Some(ref addr) = ev.payload_addr {
                    ids.push(addr.pack_id.clone());
                }
                if let Some(arr) = ev.envelope.get("delta_extents").and_then(Value::as_array) {
                    for v in arr {
                        if let Some(pid) = v.get("pack_id").and_then(Value::as_str) {
                            ids.push(pid.to_string());
                        }
                    }
                }
                ids
            })
            .collect();

        let batch_req = IngestBatchRequest { events };
        let (ingest_resp, ingest_ack) = self.send_ingest_batch(&batch_req)?;
        report.transport_acks.push(ingest_ack);
        report.events_shipped += batch_req.events.len();
        report.hub_snapshot_id = ingest_resp.publication_id.clone();
        report.hub_generation = ingest_resp.hub_generation;

        if !ingest_resp.durable_fsynced {
            return Err(HubError::Transport {
                status: Some(200),
                message: "hub did not confirm durable fsync for ingest batch".into(),
            });
        }
        for up_id in &referenced_in_chunk {
            if let Some(up_digest) = uploaded_packs.get(up_id) {
                let verified = ingest_resp.verified_pack_digests.get(up_id);
                if verified != Some(up_digest) {
                    return Err(HubError::Transport {
                        status: Some(200),
                        message: format!(
                            "hub custody verification failed for pack `{up_id}`: expected digest {up_digest}, got {verified:?}"
                        ),
                    });
                }
            }
        }

        for hub_wm in &ingest_resp.custody_acks {
            if hub_wm.contiguous_seq == 0 {
                continue;
            }
            existing_watermarks.insert(hub_wm.epoch.clone(), hub_wm.contiguous_seq);
            let max_commit_pos = store.max_commit_pos_at_or_below(&hub_wm.epoch, hub_wm.contiguous_seq)?;
            let ack = CustodyAck {
                target_hub: self.cfg.target_name.clone(),
                producer_id: self.cfg.producer_id.clone(),
                epoch: hub_wm.epoch.clone(),
                contiguous_seq: hub_wm.contiguous_seq,
                max_commit_pos,
                pack_id: None,
                acked_at: hub_wm.acked_at,
            };
            if !crate::env::frozen() {
                store.record_custody_ack(&ack)?;
            }
            report.durable_custody_acks.push(ack);
        }
        Ok(())
    }
}

fn collect_local_commits(store: &StateStore, local_packs_dir: &Path) -> Result<Vec<SealedCommitEntry>, HubError> {
    let mut out = Vec::new();
    for pm in store.list_pack_manifests()? {
        let mut handled_manifest = false;
        if let Some(value) = pm.index_meta.get("commits_addr") {
            let addr: PayloadAddr = serde_json::from_value(value.clone())?;
            let raw_parent = Path::new(&pm.pack_path).parent();
            let dir = match raw_parent {
                Some(p) if !p.as_os_str().is_empty() && p.is_absolute() && p.exists() => p,
                _ => local_packs_dir,
            };
            let pack = SealedPack::open(dir, &addr.pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            let bytes = pack.read_payload(&addr).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            out.extend(serde_json::from_slice::<Vec<SealedCommitEntry>>(&bytes)?);
            handled_manifest = true;
        }
        if let Some(arr) = pm.index_meta.get("commits").and_then(Value::as_array) {
            for val in arr {
                let entry: SealedCommitEntry = serde_json::from_value(val.clone())?;
                out.push(entry);
            }
            handled_manifest = true;
        }
        if !handled_manifest {
            // Legacy pre-`f4ff1ce0` `archive` pack manifests persisted `index_meta.sessions` and
            // durable `(op_id, producer_id, epoch, seq, local_commit_pos)` rows in `operation_bindings`
            // without `commits_addr` or `commits`. Reconstruct their commit entries so `(epoch, seq)`
            // stays contiguous (`1..=K`) and historical archived sessions ship cleanly.
            let binding_by_pos: BTreeMap<i64, (String, DurableReceipt)> = store
                .list_operation_bindings_range(pm.min_commit_pos, pm.max_commit_pos)?
                .into_iter()
                .map(|(op_id, rec)| (rec.local_commit_pos, (op_id, rec)))
                .collect();
            let phys_pack_id = pm
                .index_meta
                .get("physical_pack_id")
                .and_then(Value::as_str)
                .or_else(|| pm.pack_id.split_once("--r").map(|(base, _)| base))
                .unwrap_or(&pm.pack_id);
            if let Some(sessions) = pm.index_meta.get("sessions").and_then(Value::as_array) {
                for (idx, sess_val) in sessions.iter().enumerate() {
                    let pos = pm.min_commit_pos + idx as i64;
                    let (op_id, producer_id, epoch, seq) = match binding_by_pos.get(&pos) {
                        Some((op_id, rec)) => (
                            op_id.clone(),
                            rec.event_id.producer_id.clone(),
                            rec.event_id.epoch.clone(),
                            rec.event_id.seq,
                        ),
                        None => {
                            let prod = sess_val
                                .get("producer_id")
                                .and_then(Value::as_str)
                                .unwrap_or_else(|| store.default_producer())
                                .to_string();
                            let harness = sess_val.get("harness").and_then(Value::as_str).unwrap_or("unknown");
                            let sid = sess_val
                                .get("id")
                                .or_else(|| sess_val.get("session_id"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            (
                                format!("archive:{}:{prod}:{harness}:{sid}", pm.pack_id),
                                prod,
                                format!("legacy-{}", pm.pack_id),
                                (idx + 1) as u64,
                            )
                        }
                    };
                    let mut payload_addr = sess_val
                        .get("addr")
                        .and_then(|a| serde_json::from_value::<PayloadAddr>(a.clone()).ok());
                    if let Some(ref mut addr) = payload_addr {
                        if addr.pack_id.is_empty() {
                            addr.pack_id = phys_pack_id.to_string();
                        }
                    }
                    let envelope = json!({
                        "kind": "session_archive",
                        "producer_id": producer_id,
                        "harness": sess_val.get("harness").cloned().unwrap_or(Value::Null),
                        "session_id": sess_val.get("id").or_else(|| sess_val.get("session_id")).cloned().unwrap_or(Value::Null),
                        "title": sess_val.get("title").cloned().unwrap_or(Value::Null),
                        "cwd": sess_val.get("cwd").cloned().unwrap_or(Value::Null),
                        "created_at": sess_val.get("created_at").cloned().unwrap_or(Value::Null),
                        "updated_at": sess_val.get("updated_at").cloned().unwrap_or(Value::Null),
                        "message_count": sess_val.get("message_count").cloned().unwrap_or(json!(0)),
                        "content_blake3": sess_val.get("content_blake3").cloned().unwrap_or(Value::Null),
                        "pack_id": payload_addr.as_ref().map(|a| a.pack_id.as_str()).unwrap_or(phys_pack_id),
                        "decoded_offset": payload_addr.as_ref().map(|a| a.decoded_offset).unwrap_or(0),
                        "length": payload_addr.as_ref().map(|a| a.length).unwrap_or(0),
                    });
                    out.push(SealedCommitEntry {
                        local_commit_pos: pos,
                        producer_id,
                        epoch,
                        seq,
                        op_id,
                        envelope,
                        payload_addr,
                        delta_extents: None,
                    });
                }
            } else {
                for (pos, (op_id, rec)) in binding_by_pos {
                    out.push(SealedCommitEntry {
                        local_commit_pos: pos,
                        producer_id: rec.event_id.producer_id,
                        epoch: rec.event_id.epoch,
                        seq: rec.event_id.seq,
                        op_id,
                        envelope: json!({ "kind": "noop" }),
                        payload_addr: None,
                        delta_extents: None,
                    });
                }
            }
        }
    }
    let inbox: Vec<CaptureInboxRow> = store.read_inbox_range(0, i64::MAX)?;
    for row in inbox {
        let payload_addr = match row.payload_ref {
            Some(ref s) => serde_json::from_str::<PayloadAddr>(s).ok(),
            None => None,
        };
        out.push(SealedCommitEntry {
            local_commit_pos: row.local_commit_pos,
            producer_id: row.producer_id,
            epoch: row.epoch,
            seq: row.seq,
            op_id: row.op_id,
            envelope: row.envelope,
            payload_addr,
            delta_extents: None,
        });
    }
    out.sort_by_key(|c| c.local_commit_pos);
    Ok(out)
}

fn load_commit_session(commit: &SealedCommitEntry, local_packs_dir: &Path) -> Result<Option<Session>, HubError> {
    if let Some(custody) = commit.envelope.get("custody") {
        let custody: crate::custody::CustodyKind = serde_json::from_value(custody.clone())?;
        let text = |k: &str| commit.envelope[k].as_str().unwrap_or_default().to_owned();
        let harness = crate::Harness::parse(&text("harness"))
            .ok_or_else(|| HubError::Pack(format!("unknown archived harness in `{}`", commit.op_id)))?;
        let key = crate::SessionKey {
            producer: text("producer_id"),
            profile: text("profile").into(),
            harness,
            native_id: text("session_id"),
        };
        let mut session = crate::custody::collect_native(local_packs_dir, key, custody)
            .map_err(|e| HubError::Pack(format!("reading native custody of `{}`: {e:#}", commit.op_id)))?;
        session.source_path = None;
        return Ok(Some(session));
    }
    if let Some(ref addr) = commit.payload_addr {
        let pack = SealedPack::open(local_packs_dir, &addr.pack_id)
            .map_err(|e| HubError::Pack(format!("opening local pack `{}`: {e:#}", addr.pack_id)))?;
        let bytes = pack
            .read_payload(addr)
            .map_err(|e| HubError::Pack(format!("reading local pack `{}`: {e:#}", addr.pack_id)))?;
        let session: Session = serde_json::from_slice(&bytes)?;
        return Ok(Some(session));
    }
    if let Some(inline) = commit.envelope.get("session") {
        let session: Session = serde_json::from_value(inline.clone())?;
        return Ok(Some(session));
    }
    Ok(None)
}

fn is_session_upsert_envelope(envelope: &Value) -> bool {
    match envelope.get("kind").and_then(Value::as_str) {
        Some("session_upsert" | "session_archive") => envelope
            .get("session_id")
            .or_else(|| envelope.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        None => envelope.get("session").is_some_and(Value::is_object),
        _ => false,
    }
}

fn path_matches_filter_pattern(cwd: &Path, pattern: &Path) -> bool {
    let pat_str = pattern.to_string_lossy();
    if let Some(prefix) = pat_str.strip_suffix("/*").or_else(|| pat_str.strip_suffix("/**")) {
        return cwd.starts_with(Path::new(prefix));
    }
    cwd.starts_with(pattern)
}

fn non_session_envelope_matches_filter(envelope: &Value, profile: &ScrubProfile) -> bool {
    let is_delete = envelope.get("kind").and_then(Value::as_str) == Some("session_delete");
    let env_cwd = envelope.get("cwd").and_then(Value::as_str).map(Path::new);

    if let Some(cwd) = env_cwd {
        if profile
            .filter
            .deny_cwds
            .iter()
            .any(|deny| path_matches_filter_pattern(cwd, deny))
        {
            return false;
        }
        if !profile.filter.allow_cwds.is_empty()
            && !profile
                .filter
                .allow_cwds
                .iter()
                .any(|allow| path_matches_filter_pattern(cwd, allow))
        {
            return false;
        }
    } else if !profile.filter.allow_cwds.is_empty() && !is_delete {
        return false;
    }

    if !profile.filter.harnesses.is_empty() {
        if let Some(h_str) = envelope.get("harness").and_then(Value::as_str) {
            let actual = match Harness::parse(h_str) {
                Some(h) => h.as_str(),
                None => h_str,
            };
            let accepted = profile.filter.harnesses.iter().any(|want| {
                let trimmed = want.trim();
                trimmed.eq_ignore_ascii_case(actual) || Harness::parse(trimmed).is_some_and(|h| h.as_str() == actual)
            });
            if !accepted {
                return false;
            }
        }
    }

    if !profile.filter.tags.is_empty() && !is_delete {
        let mut have_tags: Vec<&str> = Vec::new();
        for candidate in [
            envelope.get("tags"),
            envelope.pointer("/metadata/cv/tags"),
            envelope.pointer("/metadata/tags"),
        ]
        .into_iter()
        .flatten()
        {
            match candidate {
                Value::String(s) => have_tags.extend(s.split(',').map(str::trim).filter(|t| !t.is_empty())),
                Value::Array(items) => have_tags.extend(items.iter().filter_map(Value::as_str)),
                _ => {}
            }
        }
        if let Some(single) = envelope
            .get("tag")
            .or_else(|| envelope.pointer("/metadata/cv/tag"))
            .and_then(Value::as_str)
        {
            have_tags.push(single);
        }
        if !profile
            .filter
            .tags
            .iter()
            .any(|want| have_tags.iter().any(|have| have.eq_ignore_ascii_case(want.trim())))
        {
            return false;
        }
    }

    true
}

fn hash_principal_json_value(val: &mut Value, salt: Option<&str>) {
    let mut hasher = blake3::Hasher::new();
    if let Some(s) = salt {
        hasher.update(s.as_bytes());
        hasher.update(b":");
    }
    match &*val {
        Value::String(s) => {
            hasher.update(s.as_bytes());
        }
        other => {
            if let Ok(bytes) = super::canonical_json_bytes(other) {
                hasher.update(&bytes);
            }
        }
    }
    let hex = hasher.finalize().to_hex();
    *val = Value::String(format!("blake3:{}", hex.as_str()));
}

fn scrub_json_envelope_in_place(
    val: &mut Value,
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
) {
    scrub_json_children(val, compiled, profile, repo_root, stats, true);
    if profile.classes.assignments {
        stats.secrets.assignments += crate::redact::secrets::fields(val, "");
    }
}

fn scrub_json_value_in_place(
    val: &mut Value,
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
) {
    scrub_json_children(val, compiled, profile, repo_root, stats, false);
    if profile.classes.assignments {
        stats.secrets.assignments += crate::redact::secrets::fields(val, "");
    }
}

fn scrub_json_children(
    val: &mut Value,
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
    exempt_root_tags: bool,
) {
    match val {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(clean) = compiled.scrub_text_cow(s, repo_root, stats) {
                *s = clean;
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_json_children(item, compiled, profile, repo_root, stats, false);
            }
        }
        Value::Object(map) => {
            crate::redact::scrub_object_keys(map, |key| compiled.scrub_text_cow(key, repo_root, stats).into_owned());
            match &profile.principal {
                PrincipalPolicy::Drop => {
                    if map.remove("principal").is_some() {
                        stats.principal_transformed = true;
                    }
                }
                PrincipalPolicy::Hash { salt } => {
                    if let Some(pval) = map.get_mut("principal") {
                        hash_principal_json_value(pval, salt.as_deref());
                        stats.principal_transformed = true;
                    }
                }
                PrincipalPolicy::Keep => {}
            }
            for (k, v) in map.iter_mut() {
                if exempt_root_tags && (k == "kind" || k == "harness") {
                    continue;
                }
                if k == "principal" && !matches!(profile.principal, PrincipalPolicy::Keep) {
                    continue;
                }
                scrub_json_children(v, compiled, profile, repo_root, stats, false);
            }
        }
        _ => {}
    }
}

fn stub_session_from_envelope(envelope: &Value) -> Session {
    let default_harness = Harness::ALL[0];
    let harness = envelope
        .get("harness")
        .and_then(Value::as_str)
        .and_then(Harness::parse)
        .unwrap_or(default_harness);
    let id = envelope
        .get("session_id")
        .or_else(|| envelope.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let cwd = envelope.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    let title = envelope.get("title").and_then(Value::as_str).map(str::to_string);
    let model = envelope.get("model").and_then(Value::as_str).map(str::to_string);
    let created_at = envelope
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let updated_at = envelope
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    let mut extra = serde_json::Map::new();
    if let Some(meta) = envelope.get("metadata").and_then(Value::as_object) {
        if let Some(cv) = meta.get("cv") {
            extra.insert("cv".into(), cv.clone());
        }
    }

    Session {
        principal: None,
        id,
        harness,
        cwd,
        title,
        created_at,
        updated_at,
        model,
        git: None,
        system_prompt: None,
        lineage: Default::default(),
        messages: Vec::new(),
        source_path: None,
        source_stamp: None,
        extra,
    }
}

fn build_scrubbed_envelope(
    raw_envelope: &Value,
    scrubbed_session: &Session,
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
) -> Value {
    let kind = match raw_envelope.get("kind").and_then(Value::as_str) {
        Some("session_archive") | None => "session_upsert",
        Some(other) => other,
    };
    let msg_count = raw_envelope
        .get("message_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            scrubbed_session
                .messages
                .iter()
                .filter(|m| matches!(m.role, crate::ir::Role::User | crate::ir::Role::Assistant))
                .count() as u64
        });

    let mut out = json!({
        "kind": kind,
        "session_id": scrubbed_session.id,
        "harness": scrubbed_session.harness.as_str(),
        "title": scrubbed_session.title,
        "cwd": scrubbed_session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "model": scrubbed_session.model,
        "created_at": scrubbed_session.created_at.map(|t| t.to_rfc3339()),
        "updated_at": scrubbed_session.updated_at.map(|t| t.to_rfc3339()),
        "message_count": msg_count,
    });

    if let Some(local_profile) = raw_envelope["profile"].as_str() {
        out["profile"] = json!(crate::blake3_hex(local_profile.as_bytes()));
    }
    if let Some(meta) = raw_envelope.get("metadata") {
        let mut clean_meta = meta.clone();
        scrub_json_value_in_place(&mut clean_meta, compiled, profile, repo_root, stats);
        if let Some(obj) = out.as_object_mut() {
            obj.insert("metadata".into(), clean_meta);
        }
    }

    out
}

pub(crate) fn hash_cwd_filter_prefix(producer_id: &str, raw_prefix: &str) -> String {
    let norm = raw_prefix
        .trim_end_matches("/**")
        .trim_end_matches("/*")
        .trim_end_matches('/');
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cv-cwd-filter-v1:");
    hasher.update(producer_id.as_bytes());
    hasher.update(b":");
    hasher.update(norm.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn attach_policy_metadata(metadata: &mut Value, producer_id: &str, profile: &ScrubProfile) {
    if !metadata.is_object() {
        *metadata = json!({});
    }
    let mut classes = Vec::new();
    if profile.classes.api_keys {
        classes.push("api_keys");
    }
    if profile.classes.private_keys {
        classes.push("private_keys");
    }
    if profile.classes.jwts {
        classes.push("jwts");
    }
    if profile.classes.emails {
        classes.push("emails");
    }
    if profile.classes.blobs {
        classes.push("blobs");
    }
    if profile.classes.assignments {
        classes.push("assignments");
    }
    let scrub_ruleset = if classes.is_empty() && !profile.rewrite_home && !profile.rewrite_repo {
        "none".to_string()
    } else {
        let doc = json!({
            "classes": &classes,
            "rewrite_home": profile.rewrite_home,
            "rewrite_repo": profile.rewrite_repo,
            "private_ips": profile.private_ips,
            "hostnames": profile.hostnames,
            "ssh_principals": profile.ssh_principals,
        });
        let hex = blake3::hash(&super::canonical_json_bytes(&doc).unwrap_or_default()).to_hex();
        format!("scrub:{}", &hex.as_str()[..16])
    };
    let allow_cwd_hashes: Vec<String> = profile
        .filter
        .allow_cwds
        .iter()
        .map(|p| hash_cwd_filter_prefix(producer_id, &p.to_string_lossy()))
        .collect();
    let deny_cwd_hashes: Vec<String> = profile
        .filter
        .deny_cwds
        .iter()
        .map(|p| hash_cwd_filter_prefix(producer_id, &p.to_string_lossy()))
        .collect();
    let raw_filter_doc = json!({
        "allow_cwds": profile.filter.allow_cwds,
        "deny_cwds": profile.filter.deny_cwds,
        "harnesses": profile.filter.harnesses,
        "tags": profile.filter.tags,
    });
    let is_unfiltered = profile.filter.allow_cwds.is_empty()
        && profile.filter.deny_cwds.is_empty()
        && profile.filter.harnesses.is_empty()
        && profile.filter.tags.is_empty();
    let filter_digest = if is_unfiltered {
        "unfiltered".to_string()
    } else {
        let hex = blake3::hash(&super::canonical_json_bytes(&raw_filter_doc).unwrap_or_default()).to_hex();
        format!("blake3:{}", hex.as_str())
    };
    let filter_policy = json!({
        "restrictive": !is_unfiltered,
        "has_allow_cwds": !allow_cwd_hashes.is_empty(),
        "allow_cwd_hashes": allow_cwd_hashes,
        "deny_cwd_hashes": deny_cwd_hashes,
        "harnesses": profile.filter.harnesses,
    });
    let obj = metadata.as_object_mut().expect("object");
    obj.entry("scrub_ruleset".to_string())
        .or_insert_with(|| json!(scrub_ruleset));
    obj.entry("scrubbed_classes".to_string())
        .or_insert_with(|| json!(classes));
    obj.entry("filter_digest".to_string())
        .or_insert_with(|| json!(filter_digest));
    obj.entry("filter_policy".to_string()).or_insert(filter_policy);
}

fn scrub_jsonl_slice(
    raw: &[u8],
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for line in raw.split_inclusive(|&b| b == b'\n') {
        let has_nl = line.ends_with(b"\n");
        let body = if has_nl { &line[..line.len() - 1] } else { line };
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if body.is_empty() {
            if has_nl {
                out.push(b'\n');
            }
            continue;
        }
        if let Ok(mut val) = serde_json::from_slice::<Value>(body) {
            scrub_json_value_in_place(&mut val, compiled, profile, repo_root, stats);
            if let Ok(encoded) = serde_json::to_vec(&val) {
                out.extend_from_slice(&encoded);
                if has_nl {
                    out.push(b'\n');
                }
                continue;
            }
        }
        let lossy = String::from_utf8_lossy(body);
        let clean = compiled.scrub_text_cow(&lossy, repo_root, stats);
        out.extend_from_slice(clean.as_bytes());
        if has_nl {
            out.push(b'\n');
        }
    }
    out
}

enum OutboundRecordKind {
    None,
    Projection {
        anchor: String,
        bytes: Vec<u8>,
    },
    InitialNativeExtent {
        anchor: String,
        bytes: Vec<u8>,
        file_name: String,
        layout: Vec<String>,
    },
    DeltaExtent {
        anchor: String,
        bytes: Vec<u8>,
    },
}

struct PreparedBatch {
    pack_ids: Vec<String>,
    wire_events: Vec<IngestEventRecord>,
    sessions_admitted: usize,
    sessions_filtered_out: usize,
    redactions_applied: usize,
}

fn outbound_session_anchor(producer_id: &str, raw_envelope: &Value, harness: Harness, native_id: &str) -> String {
    let outbound_profile = raw_envelope
        .get("profile")
        .and_then(Value::as_str)
        .map(|p| PathBuf::from(crate::blake3_hex(p.as_bytes())))
        .unwrap_or_default();
    let key = crate::SessionKey {
        producer: producer_id.to_string(),
        profile: outbound_profile,
        harness,
        native_id: native_id.to_string(),
    };
    crate::custody::archive_anchor(&key)
}

fn prepare_and_pack_commits(
    pending: &[SealedCommitEntry],
    local_packs_dir: &Path,
    stage_dir: &Path,
    producer_id: &str,
    target_name: &str,
    profile: &ScrubProfile,
    compiled: &CompiledProfile,
) -> Result<PreparedBatch, HubError> {
    struct StagedEvent {
        epoch: String,
        seq: u64,
        scrubbed_envelope: Value,
        record: OutboundRecordKind,
    }

    let mut staged_events: Vec<StagedEvent> = Vec::with_capacity(pending.len());
    let mut sessions_admitted = 0usize;
    let mut sessions_filtered_out = 0usize;
    let mut redactions_applied = 0usize;

    for commit in pending {
        let explicit_kind = commit.envelope.get("kind").and_then(Value::as_str);

        // Fast-path O(delta) shipping for appended JSONL extents (`session_delta` or single-extent native JSONL)!
        let delta_addrs: Option<Vec<PayloadAddr>> = commit.delta_extents.clone().or_else(|| {
            commit
                .envelope
                .get("delta_extents")
                .and_then(|v| serde_json::from_value::<Vec<PayloadAddr>>(v.clone()).ok())
        });
        if explicit_kind == Some("session_delta") && delta_addrs.as_ref().is_some_and(|d| !d.is_empty()) {
            let stub = stub_session_from_envelope(&commit.envelope);
            if !profile.matches_session(&stub) {
                sessions_filtered_out += 1;
                staged_events.push(StagedEvent {
                    epoch: commit.epoch.clone(),
                    seq: commit.seq,
                    scrubbed_envelope: json!({ "kind": "filtered_out" }),
                    record: OutboundRecordKind::None,
                });
                continue;
            }
            sessions_admitted += 1;
            let raw_repo_root = profile.repo_root.as_deref().or(stub.cwd.as_deref());
            let (scrubbed_stub, mut stats) = compiled
                .scrub_session(&stub)
                .map_err(|e| HubError::Pack(format!("scrubbing delta stub failed: {e:#}")))?;

            let mut combined_raw = Vec::new();
            for ext in delta_addrs.as_ref().expect("non-empty") {
                let pack = SealedPack::open(local_packs_dir, &ext.pack_id)
                    .map_err(|e| HubError::Pack(format!("opening local delta pack `{}`: {e:#}", ext.pack_id)))?;
                let slice = pack
                    .read_payload(ext)
                    .map_err(|e| HubError::Pack(format!("reading local delta pack `{}`: {e:#}", ext.pack_id)))?;
                combined_raw.extend_from_slice(&slice);
            }
            let scrubbed_slice = scrub_jsonl_slice(&combined_raw, compiled, profile, raw_repo_root, &mut stats);
            let mut scrubbed_env = build_scrubbed_envelope(
                &commit.envelope,
                &scrubbed_stub,
                compiled,
                profile,
                raw_repo_root,
                &mut stats,
            );
            scrubbed_env["kind"] = json!("session_delta");
            let file_name = commit
                .envelope
                .get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("session.jsonl")
                .to_string();
            let layout: Vec<String> = commit
                .envelope
                .get("layout")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            scrubbed_env["file_name"] = json!(file_name);
            if !layout.is_empty() {
                scrubbed_env["layout"] = json!(layout);
            }
            redactions_applied += stats.total();
            let anchor = outbound_session_anchor(producer_id, &commit.envelope, stub.harness, &stub.id);
            staged_events.push(StagedEvent {
                epoch: commit.epoch.clone(),
                seq: commit.seq,
                scrubbed_envelope: scrubbed_env,
                record: OutboundRecordKind::DeltaExtent {
                    anchor,
                    bytes: scrubbed_slice,
                },
            });
            continue;
        }

        // Fast-path O(initial_extent) native JSONL shipping when filter.tags is empty and principal is not embedded in Session IR:
        if let Some(custody_val) = commit.envelope.get("custody") {
            if let Ok(crate::custody::CustodyKind::NativeExtents {
                extents,
                file_name,
                layout,
                ..
            }) = serde_json::from_value::<crate::custody::CustodyKind>(custody_val.clone())
            {
                if extents.len() == 1 && file_name.ends_with(".jsonl") && profile.filter.tags.is_empty() {
                    let stub = stub_session_from_envelope(&commit.envelope);
                    if !profile.matches_session(&stub) {
                        sessions_filtered_out += 1;
                        staged_events.push(StagedEvent {
                            epoch: commit.epoch.clone(),
                            seq: commit.seq,
                            scrubbed_envelope: json!({ "kind": "filtered_out" }),
                            record: OutboundRecordKind::None,
                        });
                        continue;
                    }
                    sessions_admitted += 1;
                    let raw_repo_root = profile.repo_root.as_deref().or(stub.cwd.as_deref());
                    let (scrubbed_stub, mut stats) = compiled
                        .scrub_session(&stub)
                        .map_err(|e| HubError::Pack(format!("scrubbing native stub failed: {e:#}")))?;
                    let ext = &extents[0];
                    let pack = SealedPack::open(local_packs_dir, &ext.pack_id)
                        .map_err(|e| HubError::Pack(format!("opening local pack `{}`: {e:#}", ext.pack_id)))?;
                    let raw_bytes = pack
                        .read_payload(ext)
                        .map_err(|e| HubError::Pack(format!("reading local pack `{}`: {e:#}", ext.pack_id)))?;
                    let scrubbed_bytes = scrub_jsonl_slice(&raw_bytes, compiled, profile, raw_repo_root, &mut stats);
                    let scrubbed_env = build_scrubbed_envelope(
                        &commit.envelope,
                        &scrubbed_stub,
                        compiled,
                        profile,
                        raw_repo_root,
                        &mut stats,
                    );
                    redactions_applied += stats.total();
                    let anchor = outbound_session_anchor(producer_id, &commit.envelope, stub.harness, &stub.id);
                    staged_events.push(StagedEvent {
                        epoch: commit.epoch.clone(),
                        seq: commit.seq,
                        scrubbed_envelope: scrubbed_env,
                        record: OutboundRecordKind::InitialNativeExtent {
                            anchor,
                            bytes: scrubbed_bytes,
                            file_name,
                            layout,
                        },
                    });
                    continue;
                }
            }
        }

        let loaded_session = load_commit_session(commit, local_packs_dir)?;
        let is_session = loaded_session.is_some() || is_session_upsert_envelope(&commit.envelope);

        if !is_session {
            if !non_session_envelope_matches_filter(&commit.envelope, profile) {
                sessions_filtered_out += 1;
                staged_events.push(StagedEvent {
                    epoch: commit.epoch.clone(),
                    seq: commit.seq,
                    scrubbed_envelope: json!({ "kind": "filtered_out" }),
                    record: OutboundRecordKind::None,
                });
                continue;
            }
            let env_cwd = commit.envelope.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            let raw_repo_root = profile.repo_root.as_deref().or(env_cwd.as_deref());
            let mut scrubbed_envelope = commit.envelope.clone();
            if let Some(p) = commit.envelope["profile"].as_str() {
                scrubbed_envelope["profile"] = json!(crate::blake3_hex(p.as_bytes()));
            }
            let mut env_stats = ScrubStats::default();
            scrub_json_envelope_in_place(&mut scrubbed_envelope, compiled, profile, raw_repo_root, &mut env_stats);
            redactions_applied += env_stats.total();
            staged_events.push(StagedEvent {
                epoch: commit.epoch.clone(),
                seq: commit.seq,
                scrubbed_envelope,
                record: OutboundRecordKind::None,
            });
            continue;
        }

        let candidate_session = match loaded_session {
            Some(ref s) => s.clone(),
            None => stub_session_from_envelope(&commit.envelope),
        };
        if !profile.matches_session(&candidate_session) {
            sessions_filtered_out += 1;
            staged_events.push(StagedEvent {
                epoch: commit.epoch.clone(),
                seq: commit.seq,
                scrubbed_envelope: json!({ "kind": "filtered_out" }),
                record: OutboundRecordKind::None,
            });
            continue;
        }

        sessions_admitted += 1;
        let raw_repo_root = profile.repo_root.as_deref().or(candidate_session.cwd.as_deref());
        let (scrubbed_session, mut stats) = compiled
            .scrub_session(&candidate_session)
            .map_err(|e| HubError::Pack(format!("scrubbing session failed: {e:#}")))?;
        let scrubbed_envelope = build_scrubbed_envelope(
            &commit.envelope,
            &scrubbed_session,
            compiled,
            profile,
            raw_repo_root,
            &mut stats,
        );
        redactions_applied += stats.total();
        let record = if loaded_session.is_some() {
            let bytes = serde_json::to_vec(&scrubbed_session)?;
            let anchor = format!("payload:{}:{}", commit.epoch, commit.seq);
            OutboundRecordKind::Projection { anchor, bytes }
        } else {
            OutboundRecordKind::None
        };
        staged_events.push(StagedEvent {
            epoch: commit.epoch.clone(),
            seq: commit.seq,
            scrubbed_envelope,
            record,
        });
    }

    // Coalesce outbound payload records into multi-record Zstd-Seekable packs (`<= 8 MiB` per pack,
    // flushing whenever an anchor repeats inside the same pack so `RecordIndex` anchors stay unique),
    // naming each pack `ship-<64hex_blake3>`!
    struct PackGroup {
        indices: Vec<usize>,
        anchors_and_bytes: Vec<(String, Vec<u8>)>,
        seen_anchors: BTreeSet<String>,
        total_bytes: usize,
    }
    let mut groups: Vec<PackGroup> = Vec::new();
    for (idx, item) in staged_events.iter().enumerate() {
        let (anchor, bytes) = match &item.record {
            OutboundRecordKind::None => continue,
            OutboundRecordKind::Projection { anchor, bytes }
            | OutboundRecordKind::InitialNativeExtent { anchor, bytes, .. }
            | OutboundRecordKind::DeltaExtent { anchor, bytes, .. } => (anchor.clone(), bytes.clone()),
        };
        let needs_new_group = match groups.last() {
            None => true,
            Some(g) => {
                g.seen_anchors.contains(&anchor)
                    || (!g.indices.is_empty() && g.total_bytes + bytes.len() > MAX_OUTBOUND_PACK_BYTES)
            }
        };
        if needs_new_group {
            groups.push(PackGroup {
                indices: Vec::new(),
                anchors_and_bytes: Vec::new(),
                seen_anchors: BTreeSet::new(),
                total_bytes: 0,
            });
        }
        let g = groups.last_mut().expect("just pushed");
        g.total_bytes += bytes.len();
        g.seen_anchors.insert(anchor.clone());
        g.indices.push(idx);
        g.anchors_and_bytes.push((anchor, bytes));
    }

    let mut resolved_addrs: Vec<Option<PayloadAddr>> = vec![None; staged_events.len()];
    let mut pack_ids: Vec<String> = Vec::new();
    let mut seen_pack_ids: BTreeSet<String> = BTreeSet::new();

    for group in groups {
        let mut group_hasher = blake3::Hasher::new();
        for (anchor, bytes) in &group.anchors_and_bytes {
            group_hasher.update(&(anchor.len() as u64).to_le_bytes());
            group_hasher.update(anchor.as_bytes());
            group_hasher.update(&(bytes.len() as u64).to_le_bytes());
            group_hasher.update(bytes);
        }
        let pack_hex = group_hasher.finalize().to_hex();
        let out_pack_id = format!("ship-{}", pack_hex.as_str());

        if seen_pack_ids.insert(out_pack_id.clone()) {
            let mut builder =
                PackBuilder::create(stage_dir, &out_pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            let mut group_addrs = Vec::with_capacity(group.indices.len());
            for (anchor, bytes) in &group.anchors_and_bytes {
                let addr = builder
                    .append_record(anchor, bytes)
                    .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                group_addrs.push(addr);
            }
            builder.seal(Claim::New).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            for (ev_idx, addr) in group.indices.into_iter().zip(group_addrs) {
                resolved_addrs[ev_idx] = Some(addr);
            }
            pack_ids.push(out_pack_id);
        } else {
            let sealed = SealedPack::open(stage_dir, &out_pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
            for (ev_idx, (anchor, _)) in group.indices.into_iter().zip(group.anchors_and_bytes) {
                let span = sealed
                    .record_index()
                    .get(&anchor)
                    .ok_or_else(|| HubError::Pack(format!("missing record `{anchor}` in `{out_pack_id}`")))?;
                resolved_addrs[ev_idx] = Some(span.clone().into_addr(&out_pack_id));
            }
        }
    }

    let mut wire_events = Vec::with_capacity(staged_events.len());
    for (mut item, addr) in staged_events.into_iter().zip(resolved_addrs) {
        let wire_addr = match (&item.record, addr) {
            (OutboundRecordKind::InitialNativeExtent { file_name, layout, .. }, Some(a)) => {
                let custody = crate::custody::CustodyKind::NativeExtents {
                    extents: vec![a.clone()],
                    total_bytes: a.length,
                    prefix_blake3: a.digest_hex(),
                    file_name: file_name.clone(),
                    layout: layout.clone(),
                };
                item.scrubbed_envelope["custody"] = serde_json::to_value(&custody)?;
                Some(a)
            }
            (OutboundRecordKind::DeltaExtent { .. }, Some(a)) => {
                item.scrubbed_envelope["delta_extents"] = json!([a.clone()]);
                Some(a)
            }
            (_, other) => other,
        };
        let event_id = EventId::new(producer_id.to_string(), item.epoch.clone(), item.seq);
        let op_id = format!("ship:{target_name}:{producer_id}:{}:{}", item.epoch, item.seq);
        wire_events.push(IngestEventRecord::new(
            event_id,
            op_id,
            item.scrubbed_envelope,
            wire_addr,
        )?);
    }

    Ok(PreparedBatch {
        pack_ids,
        wire_events,
        sessions_admitted,
        sessions_filtered_out,
        redactions_applied,
    })
}

// ───────────────────── Spool & Agentless SSH `--frozen` Delta Topologies ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpoolBatchManifest {
    pub producer_id: String,
    pub host_label: String,
    pub producer_metadata: Value,
    pub pack_ids: Vec<String>,
    pub batch: IngestBatchRequest,
}

#[derive(Debug, Clone, Default)]
pub struct SpoolExportReport {
    pub events_spooled: usize,
    pub packs_written: usize,
    pub sealed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SpoolIngestReport {
    pub batches_ingested: usize,
    pub events_accepted: usize,
    pub events_replayed: usize,
    pub hub_generation: Option<u64>,
    pub publication_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeltaExportReport {
    pub events_exported: usize,
    pub packs_exported: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DeltaIngestReport {
    pub events_accepted: usize,
    pub events_replayed: usize,
    pub hub_generation: Option<u64>,
    pub publication_id: Option<String>,
}

/// Format an `epoch -> contiguous_seq` watermark map into a shell-safe ASCII token (`"0"` if empty).
pub fn format_watermark(watermarks: &BTreeMap<String, u64>) -> String {
    if watermarks.is_empty() {
        return "0".to_string();
    }
    let parts: Vec<String> = watermarks
        .iter()
        .filter(|(_, &seq)| seq > 0)
        .map(|(epoch, seq)| {
            let mut hex = String::with_capacity(epoch.len() * 2);
            for b in epoch.as_bytes() {
                hex.push_str(&format!("{b:02x}"));
            }
            format!("{hex}:{seq}")
        })
        .collect();
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(",")
    }
}

/// Parse a watermark token produced by [`format_watermark`].
pub fn parse_watermark(raw: &str) -> Result<BTreeMap<String, u64>, HubError> {
    let s = raw.trim();
    if s.is_empty() || s == "0" {
        return Ok(BTreeMap::new());
    }
    let mut out = BTreeMap::new();
    for item in s.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((hex_ep, seq_str)) = item.split_once(':') else {
            return Err(HubError::InvalidArgument(format!("invalid watermark segment {item:?}")));
        };
        if !hex_ep.len().is_multiple_of(2) {
            return Err(HubError::InvalidArgument(format!(
                "invalid epoch hex in watermark {item:?}"
            )));
        }
        let mut bytes = Vec::with_capacity(hex_ep.len() / 2);
        for i in (0..hex_ep.len()).step_by(2) {
            let b = u8::from_str_radix(&hex_ep[i..i + 2], 16)
                .map_err(|_| HubError::InvalidArgument(format!("invalid hex in watermark {item:?}")))?;
            bytes.push(b);
        }
        let epoch =
            String::from_utf8(bytes).map_err(|_| HubError::InvalidArgument("non-UTF8 epoch in watermark".into()))?;
        let seq: u64 = seq_str
            .parse()
            .map_err(|_| HubError::InvalidArgument(format!("invalid seq in watermark {item:?}")))?;
        out.insert(epoch, seq);
    }
    Ok(out)
}

/// Credential-free sandbox spool export (`cv archive sync --spool <DIR> [--seal]`): scrubs unsent
/// local commits and writes sealed `.pack.{zst,idx.json}` + `batches/batch-*.json` into `spool_dir`
/// without any hub URL or bearer token.
pub fn export_to_spool(
    store: &StateStore,
    local_packs_dir: &Path,
    spool_dir: &Path,
    producer_id: &str,
    host_label: &str,
    profile: &ScrubProfile,
    seal: bool,
) -> Result<SpoolExportReport, HubError> {
    if !crate::env::frozen() && store.inbox_count()? > 0 {
        let _ = seal_local_inbox_to_pack(store, local_packs_dir, None)?;
    }
    let compiled = profile
        .compile()
        .map_err(|e| HubError::InvalidArgument(format!("invalid scrub profile: {e:#}")))?;

    let spool_packs_dir = spool_dir.join("packs");
    let spool_batches_dir = spool_dir.join("batches");
    std::fs::create_dir_all(&spool_packs_dir)?;
    std::fs::create_dir_all(&spool_batches_dir)?;

    let target_key = format!("spool:{}", spool_dir.display());
    let mut watermarks: BTreeMap<String, u64> = store
        .ship_cursors_for(&target_key)?
        .into_iter()
        .map(|c| (c.epoch, c.contiguous_seq))
        .collect();

    let mut report = SpoolExportReport {
        sealed: seal,
        ..Default::default()
    };
    let mut latest_epoch = String::new();
    let mut latest_seq = 0u64;
    for (ep, &sq) in &watermarks {
        if (ep.as_str(), sq) >= (latest_epoch.as_str(), latest_seq) {
            latest_epoch = ep.clone();
            latest_seq = sq;
        }
    }

    let pending_rows: Vec<SealedCommitEntry> = match store.pending_custody_commits(&watermarks, usize::MAX)? {
        Some(rows) => rows.into_iter().map(SealedCommitEntry::from_commit_row).collect(),
        None => collect_local_commits(store, local_packs_dir)?
            .into_iter()
            .filter(|c| c.seq > watermarks.get(&c.epoch).copied().unwrap_or(0))
            .collect(),
    };

    let mut enroll_stats = ScrubStats::default();
    let clean_host = if host_label.is_empty() {
        String::new()
    } else {
        compiled
            .scrub_text_cow(host_label, profile.repo_root.as_deref(), &mut enroll_stats)
            .into_owned()
    };

    if !pending_rows.is_empty() || seal {
        let prepared = if pending_rows.is_empty() {
            PreparedBatch {
                pack_ids: Vec::new(),
                wire_events: Vec::new(),
                sessions_admitted: 0,
                sessions_filtered_out: 0,
                redactions_applied: 0,
            }
        } else {
            prepare_and_pack_commits(
                &pending_rows,
                local_packs_dir,
                &spool_packs_dir,
                producer_id,
                "spool",
                profile,
                &compiled,
            )?
        };

        for ev in &prepared.wire_events {
            let entry = watermarks.entry(ev.event_id.epoch.clone()).or_insert(0);
            if ev.event_id.seq > *entry {
                *entry = ev.event_id.seq;
            }
            if (ev.event_id.epoch.as_str(), ev.event_id.seq) >= (latest_epoch.as_str(), latest_seq) {
                latest_epoch = ev.event_id.epoch.clone();
                latest_seq = ev.event_id.seq;
            }
        }

        let now_ms = Utc::now().timestamp_millis();
        let mut prod_meta = json!({});
        attach_policy_metadata(&mut prod_meta, producer_id, profile);
        if seal {
            prod_meta["lifecycle"] = serde_json::to_value(ProducerLifecycle::Sealed {
                sealed_at_ms: now_ms,
                final_epoch: latest_epoch.clone(),
                final_seq: latest_seq,
            })?;
        }

        let batch_id = format!("batch-{now_ms}-{}", &uuid::Uuid::now_v7().simple().to_string()[..12]);
        let manifest = SpoolBatchManifest {
            producer_id: producer_id.to_string(),
            host_label: clean_host,
            producer_metadata: prod_meta,
            pack_ids: prepared.pack_ids.clone(),
            batch: IngestBatchRequest {
                events: prepared.wire_events,
            },
        };
        let tmp_path = spool_batches_dir.join(format!(".{batch_id}.tmp"));
        let final_path = spool_batches_dir.join(format!("{batch_id}.json"));
        std::fs::write(&tmp_path, serde_json::to_vec_pretty(&manifest)?)?;
        crate::state::set_restrictive_file_mode(&tmp_path);
        std::fs::rename(&tmp_path, &final_path)?;

        if !crate::env::frozen() {
            for (ep, &sq) in &watermarks {
                if sq > 0 {
                    store.advance_ship_cursor(&target_key, producer_id, ep, sq, now_ms)?;
                }
            }
        }
        report.events_spooled = manifest.batch.events.len();
        report.packs_written = prepared.pack_ids.len();
    }

    Ok(report)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeltaStreamPackDesc {
    pack_id: String,
    idx_len: u64,
    zst_len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeltaStreamHeader {
    producer_id: String,
    host_label: String,
    producer_metadata: Value,
    packs: Vec<DeltaStreamPackDesc>,
    batch: IngestBatchRequest,
}

/// Export scrubbed commits strictly after `watermark_str` to `writer` (`CVDLT001` binary framing).
/// 100% read-only on `store` and `local_packs_dir` — safe under `cv --frozen`!
pub fn export_delta_stream<W: Write>(
    store: &StateStore,
    local_packs_dir: &Path,
    producer_id: &str,
    host_label: &str,
    profile: &ScrubProfile,
    watermark_str: &str,
    mut writer: W,
) -> Result<DeltaExportReport, HubError> {
    let watermarks = parse_watermark(watermark_str)?;
    let compiled = profile
        .compile()
        .map_err(|e| HubError::InvalidArgument(format!("invalid scrub profile: {e:#}")))?;

    let pending_rows: Vec<SealedCommitEntry> = match store.pending_custody_commits(&watermarks, usize::MAX)? {
        Some(rows) => rows.into_iter().map(SealedCommitEntry::from_commit_row).collect(),
        None => collect_local_commits(store, local_packs_dir)?
            .into_iter()
            .filter(|c| c.seq > watermarks.get(&c.epoch).copied().unwrap_or(0))
            .collect(),
    };

    let temp_parent = std::env::temp_dir();
    let stage_dir = TempStageDir::create_in(&temp_parent)?;
    let prepared = if pending_rows.is_empty() {
        PreparedBatch {
            pack_ids: Vec::new(),
            wire_events: Vec::new(),
            sessions_admitted: 0,
            sessions_filtered_out: 0,
            redactions_applied: 0,
        }
    } else {
        prepare_and_pack_commits(
            &pending_rows,
            local_packs_dir,
            stage_dir.path(),
            producer_id,
            "pull",
            profile,
            &compiled,
        )?
    };

    let mut enroll_stats = ScrubStats::default();
    let clean_host = if host_label.is_empty() {
        String::new()
    } else {
        compiled
            .scrub_text_cow(host_label, profile.repo_root.as_deref(), &mut enroll_stats)
            .into_owned()
    };
    let mut prod_meta = json!({});
    attach_policy_metadata(&mut prod_meta, producer_id, profile);

    let mut pack_blobs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(prepared.pack_ids.len());
    let mut pack_descs: Vec<DeltaStreamPackDesc> = Vec::with_capacity(prepared.pack_ids.len());
    for pack_id in &prepared.pack_ids {
        let idx_bytes =
            std::fs::read(pack_idx_path(stage_dir.path(), pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?)?;
        let zst_bytes =
            std::fs::read(pack_zst_path(stage_dir.path(), pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?)?;
        pack_descs.push(DeltaStreamPackDesc {
            pack_id: pack_id.clone(),
            idx_len: idx_bytes.len() as u64,
            zst_len: zst_bytes.len() as u64,
        });
        pack_blobs.push((idx_bytes, zst_bytes));
    }

    let events_exported = prepared.wire_events.len();
    let packs_exported = pack_descs.len();
    let header = DeltaStreamHeader {
        producer_id: producer_id.to_string(),
        host_label: clean_host,
        producer_metadata: prod_meta,
        packs: pack_descs,
        batch: IngestBatchRequest {
            events: prepared.wire_events,
        },
    };
    let header_bytes = serde_json::to_vec(&header)?;
    writer.write_all(DELTA_STREAM_MAGIC)?;
    writer.write_all(&(header_bytes.len() as u64).to_le_bytes())?;
    writer.write_all(&header_bytes)?;
    for (idx_bytes, zst_bytes) in pack_blobs {
        writer.write_all(&idx_bytes)?;
        writer.write_all(&zst_bytes)?;
    }
    writer.flush()?;

    Ok(DeltaExportReport {
        events_exported,
        packs_exported,
    })
}

impl HubStore {
    /// Idempotently ingest a sandbox spool directory written by [`export_to_spool`].
    pub fn ingest_spool_dir(&self, spool_dir: &Path) -> Result<SpoolIngestReport, HubError> {
        let batches_dir = spool_dir.join("batches");
        let packs_dir = spool_dir.join("packs");
        if !batches_dir.is_dir() {
            return Err(HubError::InvalidArgument(format!(
                "spool directory `{}` has no `batches/` subdirectory",
                spool_dir.display()
            )));
        }
        let mut batch_files: Vec<PathBuf> = std::fs::read_dir(&batches_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.') && n.ends_with(".json"))
            })
            .collect();
        batch_files.sort();

        let mut report = SpoolIngestReport::default();
        for batch_path in batch_files {
            let raw = std::fs::read(&batch_path)?;
            let manifest: SpoolBatchManifest = serde_json::from_slice(&raw)?;
            self.enroll_producer(&manifest.producer_id, &manifest.host_label, &manifest.producer_metadata)?;
            for pack_id in &manifest.pack_ids {
                let idx_bytes =
                    std::fs::read(pack_idx_path(&packs_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?)?;
                let zst_bytes =
                    std::fs::read(pack_zst_path(&packs_dir, pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?)?;
                self.upload_pack_bytes(pack_id, Some(&manifest.producer_id), None, &idx_bytes, &zst_bytes)?;
            }
            if !manifest.batch.events.is_empty() {
                let resp = self.ingest_batch(&manifest.batch)?;
                report.events_accepted += resp.accepted_count;
                report.events_replayed += resp.replayed_count;
                report.hub_generation = resp.hub_generation;
                report.publication_id = resp.publication_id;
            } else {
                let latest = self.latest_generation_fast()?;
                report.hub_generation = latest.as_ref().map(|(g, _)| *g);
                report.publication_id = latest.map(|(_, pid)| pid);
            }
            report.batches_ingested += 1;
        }
        Ok(report)
    }

    /// Build the current pull watermark token for `producer_or_host` from `hub_frontiers`.
    pub fn pull_watermark_for(&self, producer_or_host: &str) -> Result<String, HubError> {
        let status = self.hub_status()?;
        let matched_producers: BTreeSet<String> = status
            .producers
            .iter()
            .filter(|p| {
                p.producer_id == producer_or_host
                    || p.host_label == producer_or_host
                    || p.metadata.get("ssh_target").and_then(Value::as_str) == Some(producer_or_host)
            })
            .map(|p| p.producer_id.clone())
            .collect();
        let mut wm = BTreeMap::new();
        for f in &status.frontiers {
            if f.producer_id == producer_or_host || matched_producers.contains(&f.producer_id) {
                wm.insert(f.epoch.clone(), f.contiguous_seq);
            }
        }
        Ok(format_watermark(&wm))
    }

    /// Ingest a `CVDLT001` binary delta stream produced by [`export_delta_stream`].
    pub fn ingest_delta_stream<R: Read>(
        &self,
        mut reader: R,
        ssh_target_hint: Option<&str>,
    ) -> Result<DeltaIngestReport, HubError> {
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != DELTA_STREAM_MAGIC {
            return Err(HubError::InvalidArgument(
                "remote delta stream missing CVDLT001 magic header".into(),
            ));
        }
        let mut len_buf = [0u8; 8];
        reader.read_exact(&mut len_buf)?;
        let header_len = u64::from_le_bytes(len_buf);
        if header_len > 64 * 1024 * 1024 {
            return Err(HubError::InvalidArgument("delta stream header exceeds 64 MiB".into()));
        }
        let mut header_bytes = vec![0u8; header_len as usize];
        reader.read_exact(&mut header_bytes)?;
        let mut header: DeltaStreamHeader = serde_json::from_slice(&header_bytes)?;
        if let Some(hint) = ssh_target_hint {
            if !header.producer_metadata.is_object() {
                header.producer_metadata = json!({});
            }
            header.producer_metadata["ssh_target"] = json!(hint);
        }
        self.enroll_producer(&header.producer_id, &header.host_label, &header.producer_metadata)?;

        for desc in &header.packs {
            if desc.idx_len > 64 * 1024 * 1024 || desc.zst_len > 512 * 1024 * 1024 {
                return Err(HubError::Pack(format!(
                    "oversized pack `{}` in delta stream",
                    desc.pack_id
                )));
            }
            let mut idx_bytes = vec![0u8; desc.idx_len as usize];
            reader.read_exact(&mut idx_bytes)?;
            let mut zst_bytes = vec![0u8; desc.zst_len as usize];
            reader.read_exact(&mut zst_bytes)?;
            self.upload_pack_bytes(&desc.pack_id, Some(&header.producer_id), None, &idx_bytes, &zst_bytes)?;
        }

        if header.batch.events.is_empty() {
            let latest = self.latest_generation_fast()?;
            return Ok(DeltaIngestReport {
                events_accepted: 0,
                events_replayed: 0,
                hub_generation: latest.as_ref().map(|(g, _)| *g),
                publication_id: latest.map(|(_, pid)| pid),
            });
        }
        let resp = self.ingest_batch(&header.batch)?;
        Ok(DeltaIngestReport {
            events_accepted: resp.accepted_count,
            events_replayed: resp.replayed_count,
            hub_generation: resp.hub_generation,
            publication_id: resp.publication_id,
        })
    }
}

#[cfg(test)]
mod astra_tests {
    #[test]
    fn https_transport_verifies_certificate_and_hostname() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = cert.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key.into())
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                let session = rustls::ServerConnection::new(std::sync::Arc::new(config.clone())).unwrap();
                let mut stream = rustls::StreamOwned::new(session, socket);
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                if request.ends_with(b"\r\n\r\n") {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                        .unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        // Default trust must reject this locally generated certificate.
        let mut client = HttpClient::new(&format!("https://localhost:{port}"), None).unwrap();
        assert!(client.get("/").is_err());
        // Trust only the test CA, with hostname verification still enabled.
        client.agent = ureq::Agent::config_builder()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(3)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&[
                        ureq::tls::Certificate::from_der(der.as_ref()).to_owned(),
                    ]))
                    .build(),
            )
            .build()
            .into();
        assert_eq!(client.get("/").unwrap().0.body, b"{}");
        client.base_url = format!("https://127.0.0.1:{port}");
        assert!(client.get("/").is_err());
        server.join().unwrap();
    }

    fn http_reply(reply: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let _ = socket.write_all(&reply);
            String::from_utf8(request).unwrap()
        });
        (url, thread)
    }

    #[test]
    fn http_transport_preserves_status_prefix_auth_and_chunking() {
        let (url, thread) = http_reply(
            b"HTTP/1.1 409 Conflict\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
                .to_vec(),
        );
        let client = HttpClient::new(&format!("{url}/prefix/"), Some("secret".into())).unwrap();
        let (response, ack) = client.get("/api/hub/status").unwrap();
        assert_eq!(response.status, 409);
        assert_eq!(response.body, b"{}");
        assert_eq!(ack.endpoint, "GET /prefix/api/hub/status");
        let request = thread.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /prefix/api/hub/status http/1.1"));
        assert!(request.contains("authorization: bearer secret\r\n"));
    }

    #[test]
    fn http_transport_does_not_follow_redirects() {
        let (url, thread) = http_reply(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        assert_eq!(
            HttpClient::new(&url, Some("secret".into()))
                .unwrap()
                .get("/")
                .unwrap()
                .0
                .status,
            302
        );
        thread.join().unwrap();
    }

    #[test]
    fn http_transport_rejects_oversized_and_truncated_bodies() {
        for reply in [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_HUB_RESPONSE_BYTES + 1
            )
            .into_bytes(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n".to_vec(),
            format!(
                "HTTP/1.1 200 OK\r\nX-Huge: {}\r\n\r\n",
                "x".repeat(MAX_HUB_HEADER_BYTES)
            )
            .into_bytes(),
        ] {
            let (url, thread) = http_reply(reply);
            assert!(HttpClient::new(&url, None).unwrap().get("/").is_err());
            thread.join().unwrap();
        }
    }

    #[test]
    fn http_transport_deadline_includes_response_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let _ = socket.write_all(b"{}");
        });
        assert!(HttpClient::new(&url, None)
            .unwrap()
            .with_timeout(Duration::from_millis(100))
            .get("/")
            .is_err());
        server.join().unwrap();
    }

    #[test]
    fn http_transport_validates_hub_urls() {
        for url in [
            "ftp://example.com",
            "http://",
            "http://user:secret@example.com",
            "http://example.com?q=1",
            "https://example.com/#fragment",
        ] {
            assert!(HttpClient::new(url, None).is_err(), "{url}");
        }
        for url in [
            "https://example.com",
            "https://[::1]:8443/hub",
            "http://localhost:1234/",
        ] {
            assert!(HttpClient::new(url, None).is_ok(), "{url}");
        }
    }

    use super::*;
    #[test]
    fn sealed_inbox_keeps_envelopes_in_pack_and_reads_them_back() {
        let world = cv_testkit::World::new("inbox-pack-envelope");
        let store = StateStore::at_dir(&world.cv_home);
        let packs_dir = world.cv_home.join("packs");
        let body = "retained evidence ".repeat(10000);
        store.append("op", "digest", &json!({"body": body})).unwrap();
        let before = collect_local_commits(&store, &packs_dir).unwrap();
        let manifest = seal_local_inbox_to_pack(&store, &packs_dir, None).unwrap().unwrap();
        assert!(serde_json::to_vec(&manifest.index_meta).unwrap().len() < 1024);
        assert_eq!(store.inbox_count().unwrap(), 0);
        let after = collect_local_commits(&store, &packs_dir).unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }

    #[test]
    fn seal_local_inbox_splits_around_published_pack_ranges() {
        let world = cv_testkit::World::new("inbox-straddle-seal");
        let store = StateStore::at_dir(&world.cv_home);
        let packs_dir = world.cv_home.join("packs");

        let r1 = store
            .append("op-1", "dig-1", &json!({"kind": "receipt", "n": 1}))
            .unwrap();
        let r2 = store
            .append("op-2", "dig-2", &json!({"kind": "receipt", "n": 2}))
            .unwrap();
        let r3 = store
            .append("op-3", "dig-3", &json!({"kind": "receipt", "n": 3}))
            .unwrap();
        assert_eq!(
            (r1.local_commit_pos, r2.local_commit_pos, r3.local_commit_pos),
            (1, 2, 3)
        );

        // Publish only `[2..=2]` first (simulating `archive sync` interleaved between hook rows 1 and 3).
        store
            .publish_pack_range(&LocalStatePackManifest {
                pack_id: "mid-pack".into(),
                min_commit_pos: 2,
                max_commit_pos: 2,
                record_count: 1,
                byte_len: 16,
                digest: "blake3:mid".into(),
                pack_path: "packs/mid-pack.pack.zst".into(),
                index_meta: json!({}),
                published_at: 100,
            })
            .unwrap();
        assert_eq!(store.inbox_count().unwrap(), 2);

        // Sealing the remaining inbox rows `1` and `3` must split into `[1..=1]` and `[3..=3]`.
        let last = seal_local_inbox_to_pack(&store, &packs_dir, Some("straddle"))
            .unwrap()
            .unwrap();
        assert_eq!(last.pack_id, "straddle--r1");
        assert_eq!(store.inbox_count().unwrap(), 0);

        let all = collect_local_commits(&store, &packs_dir).unwrap();
        let positions: Vec<i64> = all.iter().map(|c| c.local_commit_pos).collect();
        assert_eq!(positions, vec![1, 2, 3]);
    }

    #[test]
    fn seal_local_inbox_recovers_after_partial_multi_run_failure_with_same_explicit_pack_id() {
        let world = cv_testkit::World::new("inbox-partial-retry");
        let store = StateStore::at_dir(&world.cv_home);
        let packs_dir = world.cv_home.join("packs");

        store
            .append("op-1", "dig-1", &json!({"kind": "receipt", "n": 1}))
            .unwrap();
        store
            .append("op-2", "dig-2", &json!({"kind": "receipt", "n": 2}))
            .unwrap();
        let missing_ref = serde_json::to_string(&PayloadAddr {
            pack_id: "temporarily-missing".into(),
            decoded_offset: 0,
            length: 4,
            blake3_digest: *blake3::hash(b"null").as_bytes(),
        })
        .unwrap();
        store
            .append_request(&crate::state::AppendRequest {
                op_id: "op-3".into(),
                req_digest: "dig-3".into(),
                producer_id: None,
                envelope: json!({"kind": "session_upsert", "session_id": "s3", "harness": "claude"}),
                payload_ref: Some(missing_ref.clone()),
            })
            .unwrap();

        // Publish `[2..=2]` so inbox splits into `[1..=1]` (run 0) and `[3..=3]` (run 1).
        store
            .publish_pack_range(&LocalStatePackManifest {
                pack_id: "mid-pack".into(),
                min_commit_pos: 2,
                max_commit_pos: 2,
                record_count: 1,
                byte_len: 16,
                digest: "blake3:mid".into(),
                pack_path: "packs/mid-pack.pack.zst".into(),
                index_meta: json!({}),
                published_at: 100,
            })
            .unwrap();

        // First attempt seals run 0 as `"batch"` and fails on run 1 because `"temporarily-missing"` does not exist yet.
        assert!(seal_local_inbox_to_pack(&store, &packs_dir, Some("batch")).is_err());
        assert_eq!(store.inbox_count().unwrap(), 1);
        assert!(store.get_pack_manifest("batch").unwrap().is_some());

        // Restore the missing payload pack and retry with the exact same explicit pack ID `"batch"`.
        let mut restore = PackBuilder::create(&packs_dir, "temporarily-missing").unwrap();
        let restored_addr = restore.append_record("s3", b"null").unwrap();
        assert_eq!(serde_json::to_string(&restored_addr).unwrap(), missing_ref);
        restore.seal(Claim::New).unwrap();

        let recovered = seal_local_inbox_to_pack(&store, &packs_dir, Some("batch"))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.pack_id, "batch--r1");
        assert_eq!(store.inbox_count().unwrap(), 0);
    }

    #[test]
    fn replication_profile_inherits_privacy_and_matches_url_while_respecting_explicit_false() {
        let raw = r#"
[privacy]
private_ips = true
hostnames = true
rewrite_home = true
principal = "drop"

[privacy.classes]
emails = false
jwts = false

[privacy.filter]
deny_cwds = ["/secret"]

[replication.team-hub]
url = "http://127.0.0.1:9123/"
token = "hub-tok"
producer_id = "box-1"
private_ips = false
principal = "keep"

[replication.team-hub.classes]
jwts = true

[replication.team-hub.filter]
harnesses = ["claude"]
"#;
        let cfg: Config = toml::from_str(raw).unwrap();

        for selector in ["team-hub", "http://127.0.0.1:9123", "http://127.0.0.1:9123/"] {
            let resolved = resolve_replication_profile(&cfg, selector);
            assert_eq!(resolved.token.as_deref(), Some("hub-tok"), "{selector}");
            assert_eq!(resolved.producer_id.as_deref(), Some("box-1"), "{selector}");
            // Inherited from `[privacy]`:
            assert!(resolved.hostnames, "{selector}");
            assert!(resolved.rewrite_home, "{selector}");
            assert!(!resolved.classes.emails, "{selector}");
            assert_eq!(resolved.filter.deny_cwds, vec![PathBuf::from("/secret")], "{selector}");
            // Explicit overrides in `[replication.team-hub]` win over `[privacy]`:
            assert!(!resolved.private_ips, "{selector}");
            assert_eq!(resolved.principal, PrincipalPolicy::Keep, "{selector}");
            assert!(resolved.classes.jwts, "{selector}");
            assert_eq!(resolved.filter.harnesses, vec!["claude"], "{selector}");
        }
    }

    #[test]
    fn shipper_rejects_unfsynced_ingest() {
        // `durable_fsynced: false` from `/api/hub/ingest_batch` returns `Err(HubError::Transport)`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                let header_end = loop {
                    let n = std::io::Read::read(&mut sock, &mut buf).unwrap();
                    if n == 0 {
                        break req.len();
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let content_len = String::from_utf8_lossy(&req[..header_end])
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while req.len() < header_end + content_len {
                    let n = std::io::Read::read(&mut sock, &mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let body = if i == 0 {
                    serde_json::to_vec(&json!({
                        "producer": {
                            "producer_id": "prod-1",
                            "host_label": "box",
                            "metadata": {},
                            "enrolled_at": 1,
                            "updated_at": 1
                        }
                    }))
                    .unwrap()
                } else {
                    serde_json::to_vec(&IngestBatchResponse {
                        hub_generation: None,
                        publication_id: None,
                        accepted_count: 1,
                        replayed_count: 0,
                        durable_fsynced: false,
                        verified_pack_digests: BTreeMap::new(),
                        custody_acks: Vec::new(),
                    })
                    .unwrap()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                sock.write_all(head.as_bytes()).unwrap();
                sock.write_all(&body).unwrap();
                sock.flush().unwrap();
                let _ = sock.shutdown(std::net::Shutdown::Write);
            }
        });

        let world = cv_testkit::World::new("shipper-unfsynced-ingest");
        let store = StateStore::at_dir(&world.cv_home);
        let packs_dir = world.cv_home.join("packs");
        store
            .append(
                "op:1",
                "dig-1",
                &json!({"kind": "session_upsert", "session_id": "s1", "harness": "claude"}),
            )
            .unwrap();
        let cfg = ShipConfig::new("hub", format!("http://{addr}"), store.default_producer());
        let shipper = Shipper::new(cfg).unwrap();
        let err = shipper.sync(&store, &packs_dir).unwrap_err();
        srv.join().unwrap();
        assert!(
            matches!(err, HubError::Transport { status: Some(200), .. }),
            "expected Transport error when durable_fsynced is false, got {err:?}"
        );
        assert!(store.custody_acks_for("hub").unwrap().is_empty());
    }
}
