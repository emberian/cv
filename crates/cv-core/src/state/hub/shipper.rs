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
use super::{HubError, PACK_STREAM_CONTENT_TYPE};
use crate::config::Config;
use crate::ir::{Harness, Session};
use crate::packs::{pack_idx_path, pack_zst_path, PackBuilder, PayloadAddr, SealedPack};
use crate::publish::Claim;
use crate::redact::{scrub_text_with_profile, ScrubProfile};
use crate::state::{
    tables::{CustodyAck, PackManifest as LocalStatePackManifest},
    AppendRequest, CaptureInboxRow, DurableReceipt, EventId, StateStore,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Resolve the [`ScrubProfile`] for replication target `target` (`[replication.<target>]`),
/// falling back to `[privacy]` when no target-specific profile is configured.
pub fn resolve_replication_profile(cfg: &Config, target: &str) -> ScrubProfile {
    cfg.replication
        .get(target)
        .cloned()
        .unwrap_or_else(|| cfg.privacy.clone())
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

/// Minimal zero-dependency `std::net::TcpStream` HTTP/1.1 client for hub replication and tests.
#[derive(Debug, Clone)]
pub struct HttpClient {
    host: String,
    port: u16,
    base_path: String,
    bearer_token: Option<String>,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(base_url: &str, bearer_token: Option<String>) -> Result<Self, HubError> {
        let rest = base_url
            .trim()
            .strip_prefix("http://")
            .ok_or_else(|| HubError::InvalidArgument(format!("hub URL must start with `http://`: {base_url:?}")))?;
        let (authority, path_part) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        let (host, port) = parse_authority(authority)?;
        let base_path = path_part.trim_end_matches('/').to_string();
        Ok(Self {
            host,
            port,
            base_path,
            bearer_token: bearer_token.filter(|t| !t.is_empty()),
            timeout: Duration::from_secs(15),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn base_url(&self) -> String {
        let host_fmt = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("http://{}:{}{}", host_fmt, self.port, self.base_path)
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
        let rel = if path.starts_with('/') {
            format!("{}{}", self.base_path, path)
        } else {
            format!("{}/{}", self.base_path, path)
        };
        let addr_spec = if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let sock_addr = addr_spec.to_socket_addrs()?.next().ok_or_else(|| HubError::Transport {
            status: None,
            message: format!("could not resolve hub address `{addr_spec}`"),
        })?;

        let mut stream = TcpStream::connect_timeout(&sock_addr, self.timeout)?;
        let _ = stream.set_nodelay(true);
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let mut head = String::with_capacity(256);
        head.push_str(&format!("{method} {rel} HTTP/1.1\r\n"));
        head.push_str(&format!("Host: {addr_spec}\r\n"));
        head.push_str("Connection: close\r\n");
        if let Some(ref token) = self.bearer_token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        for (k, v) in extra_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() || matches!(method, "POST" | "PUT") {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes())?;
        if !body.is_empty() {
            stream.write_all(body)?;
        }
        stream.flush()?;

        let resp = read_http_response(stream)?;
        let ack = TransportDeliveryAck {
            endpoint: format!("{method} {rel}"),
            status: resp.status,
            response_bytes: resp.body.len(),
            received_at: Utc::now().timestamp_millis(),
        };
        Ok((resp, ack))
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16), HubError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .ok_or_else(|| HubError::InvalidArgument(format!("unclosed IPv6 bracket in authority {authority:?}")))?;
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .ok_or_else(|| HubError::InvalidArgument(format!("invalid port in authority {authority:?}")))?
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port_str)) if !host.is_empty() => {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| HubError::InvalidArgument(format!("invalid port in authority {authority:?}")))?;
            Ok((host.to_string(), port))
        }
        _ if !authority.is_empty() => Ok((authority.to_string(), 80)),
        _ => Err(HubError::InvalidArgument(format!(
            "empty host in authority {authority:?}"
        ))),
    }
}

fn read_http_response(stream: TcpStream) -> Result<RawHttpResponse, HubError> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let n = reader.read_line(&mut status_line)?;
    if n == 0 {
        return Err(HubError::Transport {
            status: None,
            message: "empty HTTP response from hub".into(),
        });
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HubError::Transport {
            status: None,
            message: format!("malformed HTTP status line: {status_line:?}"),
        })?;

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let name = k.trim().to_string();
            let val = v.trim().to_string();
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = val.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("Transfer-Encoding") && val.to_ascii_lowercase().contains("chunked") {
                is_chunked = true;
            }
            headers.push((name, val));
        }
    }

    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if is_chunked {
        read_chunked_body(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        buf
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        buf
    };

    Ok(RawHttpResponse { status, headers, body })
}

fn read_chunked_body(reader: &mut impl BufRead) -> Result<Vec<u8>, HubError> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex, 16).map_err(|_| HubError::Transport {
            status: None,
            message: format!("malformed chunk size line: {size_line:?}"),
        })?;
        if chunk_len == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let prev = out.len();
        out.resize(prev + chunk_len, 0);
        reader.read_exact(&mut out[prev..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(out)
}

/// Configuration for a [`Shipper`] targeting one Fleet Hub.
#[derive(Debug, Clone)]
pub struct ShipConfig {
    /// Logical replication target key (resolves `[replication.<target_name>]` in `config.toml`,
    /// and serves as `custody_acks.target_hub`).
    pub target_name: String,
    /// Hub base URL (`http://host:port`).
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

/// Seal all currently unpacked rows in `store.capture_inbox` into a single consolidated local
/// Zstd-Seekable pack under `local_packs_dir` and atomically transfer ownership via
/// [`StateStore::publish_pack_range`].
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
    let min_pos = rows.first().expect("non-empty").local_commit_pos;
    let max_pos = rows.last().expect("non-empty").local_commit_pos;
    let pack_id = match explicit_pack_id {
        Some(id) => id.to_string(),
        None => format!(
            "inbox-{min_pos}-{max_pos}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
    };

    let mut builder = PackBuilder::create(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
    let mut entries = Vec::with_capacity(rows.len());

    for row in &rows {
        let new_addr = if let Some(ref pref) = row.payload_ref {
            let old_addr: PayloadAddr = serde_json::from_str(pref)?;
            let old_pack =
                SealedPack::open(local_packs_dir, &old_addr.pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
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
        });
    }

    let _sealed_manifest = builder.seal(Claim::New).map_err(|e| HubError::Pack(format!("{e:#}")))?;
    let zst_path = pack_zst_path(local_packs_dir, &pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
    let zst_bytes = std::fs::read(&zst_path)?;
    let pack_digest = blake3::hash(&zst_bytes).to_hex().to_string();

    let state_manifest = LocalStatePackManifest {
        pack_id: pack_id.clone(),
        min_commit_pos: min_pos,
        max_commit_pos: max_pos,
        record_count: rows.len() as u64,
        byte_len: zst_bytes.len() as u64,
        digest: pack_digest,
        pack_path: zst_path.to_string_lossy().into_owned(),
        index_meta: json!({ "commits": entries }),
        published_at: Utc::now().timestamp_millis(),
    };
    store.publish_pack_range(&state_manifest)?;
    Ok(Some(state_manifest))
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
        let host_label = if profile.hostnames && !self.cfg.host_label.is_empty() {
            scrub_text_with_profile(&self.cfg.host_label, &profile).unwrap_or_else(|_| "[REDACTED:host]".into())
        } else {
            self.cfg.host_label.clone()
        };
        let req = EnrollRequest {
            producer_id: self.cfg.producer_id.clone(),
            host_label,
            metadata: self.cfg.producer_metadata.clone(),
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

        let profile = self.resolved_profile();
        let compiled = profile
            .compile()
            .map_err(|e| HubError::InvalidArgument(format!("invalid scrub profile: {e:#}")))?;

        let existing_watermarks: BTreeMap<(String, String), u64> = if self.cfg.force_reship {
            BTreeMap::new()
        } else {
            store
                .custody_acks_for(&self.cfg.target_name)?
                .into_iter()
                .map(|ack| ((ack.producer_id, ack.epoch), ack.contiguous_seq))
                .collect()
        };

        let all_local = collect_local_commits(store)?;
        report.events_examined = all_local.len();

        let pending: Vec<SealedCommitEntry> = all_local
            .into_iter()
            .filter(|c| {
                let acked = existing_watermarks
                    .get(&(self.cfg.producer_id.clone(), c.epoch.clone()))
                    .or_else(|| existing_watermarks.get(&(c.producer_id.clone(), c.epoch.clone())))
                    .copied()
                    .unwrap_or(0);
                c.seq > acked
            })
            .collect();

        if pending.is_empty() {
            return Ok(report);
        }

        // Scrub & filter every pending commit BEFORE building outbound packs or wire envelopes.
        struct PreparedItem {
            commit: SealedCommitEntry,
            scrubbed_envelope: Value,
            scrubbed_payload: Option<Vec<u8>>,
        }

        let mut prepared: Vec<PreparedItem> = Vec::with_capacity(pending.len());
        let mut has_any_payload = false;

        for commit in pending {
            let loaded_session = load_commit_session(&commit, local_packs_dir)?;
            let candidate_session = match loaded_session {
                Some(ref s) => s.clone(),
                None => stub_session_from_envelope(&commit.envelope),
            };

            if !profile.matches_session(&candidate_session) {
                report.sessions_filtered_out += 1;
                prepared.push(PreparedItem {
                    commit,
                    scrubbed_envelope: json!({ "kind": "filtered_out" }),
                    scrubbed_payload: None,
                });
                continue;
            }

            report.sessions_admitted += 1;
            let (scrubbed_session, stats) = compiled
                .scrub_session(&candidate_session)
                .map_err(|e| HubError::Pack(format!("scrubbing session failed: {e:#}")))?;
            report.redactions_applied += stats.total();

            let scrubbed_envelope = build_scrubbed_envelope(&commit.envelope, &scrubbed_session, &profile)?;
            let scrubbed_payload = if loaded_session.is_some() {
                let bytes = serde_json::to_vec(&scrubbed_session)?;
                has_any_payload = true;
                Some(bytes)
            } else {
                None
            };

            prepared.push(PreparedItem {
                commit,
                scrubbed_envelope,
                scrubbed_payload,
            });
        }

        // Stage & upload deterministic, content-addressed outbound Zstd-Seekable packs FIRST
        // (`publish-before-reference`!). Using a per-payload content-addressed `out_pack_id`
        // ensures `PayloadAddr` is a pure function of `scrubbed_payload`, invariant across
        // retries, `force_reship`, and batch boundaries.
        let outbound_dir = local_packs_dir.join(".ship-outbound");
        std::fs::create_dir_all(&outbound_dir)?;

        let mut item_addrs: Vec<Option<PayloadAddr>> = vec![None; prepared.len()];
        let mut uploaded_packs: BTreeMap<String, (PayloadAddr, String)> = BTreeMap::new();

        if has_any_payload {
            for (idx, item) in prepared.iter().enumerate() {
                let Some(ref bytes) = item.scrubbed_payload else {
                    continue;
                };
                let payload_hex = blake3::hash(bytes).to_hex();
                let out_pack_id = format!("ship-{}", &payload_hex.as_str()[..24]);

                if let Some((existing_addr, _)) = uploaded_packs.get(&out_pack_id) {
                    item_addrs[idx] = Some(existing_addr.clone());
                    continue;
                }

                let out_zst =
                    pack_zst_path(&outbound_dir, &out_pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                if out_zst.is_file() {
                    let _ = std::fs::remove_file(&out_zst);
                }
                let out_idx =
                    pack_idx_path(&outbound_dir, &out_pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                if out_idx.is_file() {
                    let _ = std::fs::remove_file(&out_idx);
                }

                let mut builder =
                    PackBuilder::create(&outbound_dir, &out_pack_id).map_err(|e| HubError::Pack(format!("{e:#}")))?;
                let addr = builder
                    .append_record("payload:0", bytes)
                    .map_err(|e| HubError::Pack(format!("{e:#}")))?;
                builder.seal(Claim::New).map_err(|e| HubError::Pack(format!("{e:#}")))?;

                let (upload_resp, upload_ack) = self.upload_pack_files(&outbound_dir, &out_pack_id)?;
                report.transport_acks.push(upload_ack);

                if !upload_resp.durable_fsynced {
                    return Err(HubError::Transport {
                        status: Some(200),
                        message: format!("hub did not confirm durable fsync for pack `{out_pack_id}`"),
                    });
                }
                report.uploaded_pack_id = Some(upload_resp.pack.pack_id.clone());
                report.uploaded_pack_digest = Some(upload_resp.pack.blake3_digest.clone());
                item_addrs[idx] = Some(addr.clone());
                uploaded_packs.insert(upload_resp.pack.pack_id, (addr, upload_resp.pack.blake3_digest));
            }
        }

        // Build `IngestBatchRequest` referencing the already-published outbound pack addresses.
        let mut wire_events = Vec::with_capacity(prepared.len());
        for (item, addr) in prepared.iter().zip(item_addrs) {
            let event_id = EventId::new(self.cfg.producer_id.clone(), item.commit.epoch.clone(), item.commit.seq);
            let op_id = format!(
                "ship:{}:{}:{}:{}",
                self.cfg.target_name, self.cfg.producer_id, item.commit.epoch, item.commit.seq
            );
            let rec = IngestEventRecord::new(event_id, op_id, item.scrubbed_envelope.clone(), addr)?;
            wire_events.push(rec);
        }

        let batch_req = IngestBatchRequest { events: wire_events };
        let (ingest_resp, ingest_ack) = self.send_ingest_batch(&batch_req)?;
        report.transport_acks.push(ingest_ack);
        report.events_shipped = batch_req.events.len();

        if let Some(ref snap) = ingest_resp.snapshot {
            report.hub_snapshot_id = Some(snap.publication_id.clone());
            report.hub_generation = Some(snap.hub_generation);
        }

        // Stage-2 Durable Hub Custody Verification:
        // Record `StateStore::record_custody_ack` ONLY if:
        // 1. `ingest_resp.durable_fsynced == true`, AND
        // 2. For every outbound pack referenced, the hub echoed its exact verified BLAKE3 digest
        //    in `ingest_resp.verified_pack_digests`!
        if !ingest_resp.durable_fsynced {
            return Ok(report);
        }
        for (up_id, (_, up_digest)) in &uploaded_packs {
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

        // Map hub `(producer_id, epoch, contiguous_seq)` back to the highest local `local_commit_pos`
        // covered by `seq <= contiguous_seq` so `StateStore::is_pack_evictable` unlocks ONLY when
        // the hub's gap-free contiguous frontier covers the sealed local pack!
        let all_commits_now = collect_local_commits(store)?;
        for hub_wm in &ingest_resp.custody_acks {
            if hub_wm.contiguous_seq == 0 {
                continue;
            }
            let max_commit_pos = all_commits_now
                .iter()
                .filter(|c| c.epoch == hub_wm.epoch && c.seq <= hub_wm.contiguous_seq)
                .map(|c| c.local_commit_pos)
                .max();
            let ack = CustodyAck {
                target_hub: self.cfg.target_name.clone(),
                producer_id: self.cfg.producer_id.clone(),
                epoch: hub_wm.epoch.clone(),
                contiguous_seq: hub_wm.contiguous_seq,
                max_commit_pos,
                pack_id: None,
                acked_at: hub_wm.acked_at,
            };
            store.record_custody_ack(&ack)?;
            report.durable_custody_acks.push(ack);
        }

        Ok(report)
    }
}

fn collect_local_commits(store: &StateStore) -> Result<Vec<SealedCommitEntry>, HubError> {
    let mut out = Vec::new();
    for pm in store.list_pack_manifests()? {
        if let Some(arr) = pm.index_meta.get("commits").and_then(Value::as_array) {
            for val in arr {
                let entry: SealedCommitEntry = serde_json::from_value(val.clone())?;
                out.push(entry);
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
        });
    }
    out.sort_by_key(|c| c.local_commit_pos);
    Ok(out)
}

fn load_commit_session(commit: &SealedCommitEntry, local_packs_dir: &Path) -> Result<Option<Session>, HubError> {
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

fn stub_session_from_envelope(envelope: &Value) -> Session {
    let harness_str = envelope.get("harness").and_then(Value::as_str).unwrap_or("claude");
    let harness = Harness::parse(harness_str).unwrap_or(Harness::Claude);
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
    profile: &ScrubProfile,
) -> Result<Value, HubError> {
    let kind = raw_envelope
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("session_upsert");
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

    if let Some(meta) = raw_envelope.get("metadata") {
        let meta_str = serde_json::to_string(meta)?;
        let clean_str = scrub_text_with_profile(&meta_str, profile)
            .map_err(|e| HubError::Pack(format!("scrubbing metadata failed: {e:#}")))?;
        if let Ok(clean_val) = serde_json::from_str::<Value>(&clean_str) {
            if let Some(obj) = out.as_object_mut() {
                obj.insert("metadata".into(), clean_val);
            }
        }
    }

    Ok(out)
}
