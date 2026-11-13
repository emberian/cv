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
use crate::redact::{CompiledProfile, PrincipalPolicy, ScrubProfile, ScrubStats};
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

const MAX_HUB_HEADER_BYTES: usize = 64 * 1024;
const MAX_HUB_HEADERS: usize = 128;
const MAX_HUB_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

fn read_bounded_http_line(reader: &mut impl BufRead, max_bytes: usize, label: &str) -> Result<String, HubError> {
    let mut buf = Vec::new();
    reader.take((max_bytes + 1) as u64).read_until(b'\n', &mut buf)?;
    if buf.len() > max_bytes {
        return Err(HubError::Transport {
            status: None,
            message: format!("HTTP {label} exceeds {max_bytes} bytes"),
        });
    }
    String::from_utf8(buf).map_err(|e| HubError::Transport {
        status: None,
        message: format!("HTTP {label} is not valid UTF-8: {e}"),
    })
}

fn read_exact_bounded(
    reader: &mut impl BufRead,
    out: &mut Vec<u8>,
    len: usize,
    status: Option<u16>,
) -> Result<(), HubError> {
    out.reserve(len.min(64 * 1024));
    let before = out.len();
    reader.take(len as u64).read_to_end(out)?;
    if out.len() - before != len {
        return Err(HubError::Transport {
            status,
            message: format!(
                "truncated HTTP body from hub: expected {len} bytes, got {}",
                out.len() - before
            ),
        });
    }
    Ok(())
}

fn read_http_response(stream: TcpStream) -> Result<RawHttpResponse, HubError> {
    read_http_response_from(BufReader::new(stream))
}

fn read_http_response_from(mut reader: impl BufRead) -> Result<RawHttpResponse, HubError> {
    let status_line = read_bounded_http_line(&mut reader, 8 * 1024, "status line")?;
    if status_line.is_empty() {
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

    let mut total_header_bytes = status_line.len();
    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    loop {
        let line = read_bounded_http_line(&mut reader, MAX_HUB_HEADER_BYTES, "header line")?;
        if line.is_empty() {
            break;
        }
        total_header_bytes = total_header_bytes.saturating_add(line.len());
        if total_header_bytes > MAX_HUB_HEADER_BYTES {
            return Err(HubError::Transport {
                status: Some(status),
                message: format!("HTTP headers exceed {MAX_HUB_HEADER_BYTES} bytes"),
            });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if headers.len() >= MAX_HUB_HEADERS {
            return Err(HubError::Transport {
                status: Some(status),
                message: format!("HTTP response exceeds {MAX_HUB_HEADERS} headers"),
            });
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let name = k.trim().to_string();
            let val = v.trim().to_string();
            if name.eq_ignore_ascii_case("Content-Length") {
                let parsed = val.parse::<u64>().map_err(|_| HubError::Transport {
                    status: Some(status),
                    message: format!("malformed Content-Length header: {val:?}"),
                })?;
                if parsed > MAX_HUB_RESPONSE_BYTES as u64 {
                    return Err(HubError::Transport {
                        status: Some(status),
                        message: format!(
                            "HTTP response Content-Length {parsed} exceeds max allowed {MAX_HUB_RESPONSE_BYTES}"
                        ),
                    });
                }
                content_length = Some(parsed as usize);
            } else if name.eq_ignore_ascii_case("Transfer-Encoding") && val.to_ascii_lowercase().contains("chunked") {
                is_chunked = true;
            }
            headers.push((name, val));
        }
    }

    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if is_chunked {
        read_chunked_body(&mut reader, Some(status))?
    } else if let Some(len) = content_length {
        let mut buf = Vec::new();
        read_exact_bounded(&mut reader, &mut buf, len, Some(status))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .by_ref()
            .take((MAX_HUB_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut buf)?;
        if buf.len() > MAX_HUB_RESPONSE_BYTES {
            return Err(HubError::Transport {
                status: Some(status),
                message: format!("HTTP response body exceeds max allowed {MAX_HUB_RESPONSE_BYTES} bytes"),
            });
        }
        buf
    };

    Ok(RawHttpResponse { status, headers, body })
}

fn read_chunked_body(reader: &mut impl BufRead, status: Option<u16>) -> Result<Vec<u8>, HubError> {
    let mut out = Vec::new();
    loop {
        let size_line = read_bounded_http_line(reader, 4096, "chunk size line")?;
        if size_line.is_empty() {
            return Err(HubError::Transport {
                status,
                message: "unexpected EOF while reading chunked HTTP body".into(),
            });
        }
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex, 16).map_err(|_| HubError::Transport {
            status,
            message: format!("malformed chunk size line: {size_line:?}"),
        })?;
        if chunk_len == 0 {
            loop {
                let trailer = read_bounded_http_line(reader, 8 * 1024, "chunk trailer")?;
                if trailer.is_empty() || trailer.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            break;
        }
        let total = out
            .len()
            .checked_add(chunk_len)
            .filter(|&n| n <= MAX_HUB_RESPONSE_BYTES)
            .ok_or_else(|| HubError::Transport {
                status,
                message: format!("chunked HTTP body exceeds max allowed {MAX_HUB_RESPONSE_BYTES} bytes"),
            })?;
        let _ = total;
        read_exact_bounded(reader, &mut out, chunk_len, status)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        if crlf != *b"\r\n" {
            return Err(HubError::Transport {
                status,
                message: "malformed chunk terminator in HTTP body".into(),
            });
        }
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

        let existing_watermarks: BTreeMap<(String, String), u64> = if self.cfg.force_reship {
            BTreeMap::new()
        } else {
            store
                .custody_acks_for(&self.cfg.target_name)?
                .into_iter()
                .map(|ack| ((ack.producer_id, ack.epoch), ack.contiguous_seq))
                .collect()
        };

        let all_local = collect_local_commits(store, local_packs_dir)?;
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
            let is_session = loaded_session.is_some() || is_session_upsert_envelope(&commit.envelope);

            if !is_session {
                if !non_session_envelope_matches_filter(&commit.envelope, &profile) {
                    report.sessions_filtered_out += 1;
                    prepared.push(PreparedItem {
                        commit,
                        scrubbed_envelope: json!({ "kind": "filtered_out" }),
                        scrubbed_payload: None,
                    });
                    continue;
                }
                let env_cwd = commit.envelope.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                let raw_repo_root = profile.repo_root.as_deref().or(env_cwd.as_deref());
                let mut scrubbed_envelope = commit.envelope.clone();
                let mut env_stats = ScrubStats::default();
                scrub_json_value_in_place(
                    &mut scrubbed_envelope,
                    &compiled,
                    &profile,
                    raw_repo_root,
                    &mut env_stats,
                );
                report.redactions_applied += env_stats.total();
                prepared.push(PreparedItem {
                    commit,
                    scrubbed_envelope,
                    scrubbed_payload: None,
                });
                continue;
            }

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
            let raw_repo_root = profile.repo_root.as_deref().or(candidate_session.cwd.as_deref());
            let (scrubbed_session, mut stats) = compiled
                .scrub_session(&candidate_session)
                .map_err(|e| HubError::Pack(format!("scrubbing session failed: {e:#}")))?;

            let scrubbed_envelope = build_scrubbed_envelope(
                &commit.envelope,
                &scrubbed_session,
                &compiled,
                &profile,
                raw_repo_root,
                &mut stats,
            );
            report.redactions_applied += stats.total();
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
        let mut item_addrs: Vec<Option<PayloadAddr>> = vec![None; prepared.len()];
        let mut uploaded_packs: BTreeMap<String, (PayloadAddr, String)> = BTreeMap::new();

        if has_any_payload {
            let outbound_dir = local_packs_dir.join(".ship-outbound");
            std::fs::create_dir_all(&outbound_dir)?;
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

                let upload_res = self.upload_pack_files(&outbound_dir, &out_pack_id);
                let _ = std::fs::remove_file(&out_zst);
                let _ = std::fs::remove_file(&out_idx);
                let (upload_resp, upload_ack) = match upload_res {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = std::fs::remove_dir(&outbound_dir);
                        return Err(e);
                    }
                };
                report.transport_acks.push(upload_ack);

                if !upload_resp.durable_fsynced {
                    let _ = std::fs::remove_dir(&outbound_dir);
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
            let _ = std::fs::remove_dir(&outbound_dir);
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
            return Err(HubError::Transport {
                status: Some(200),
                message: "hub did not confirm durable fsync for ingest batch".into(),
            });
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
        let all_commits_now = collect_local_commits(store, local_packs_dir)?;
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
    *val = Value::String(format!("sha256:{}", &hex.as_str()[..24]));
}

fn scrub_json_value_in_place(
    val: &mut Value,
    compiled: &CompiledProfile,
    profile: &ScrubProfile,
    repo_root: Option<&Path>,
    stats: &mut ScrubStats,
) {
    match val {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(clean) = compiled.scrub_text_cow(s, repo_root, stats) {
                *s = clean;
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_json_value_in_place(item, compiled, profile, repo_root, stats);
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
                if k == "kind" || k == "harness" {
                    continue;
                }
                if k == "principal" && !matches!(profile.principal, PrincipalPolicy::Keep) {
                    continue;
                }
                scrub_json_value_in_place(v, compiled, profile, repo_root, stats);
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

    if let Some(meta) = raw_envelope.get("metadata") {
        let mut clean_meta = meta.clone();
        scrub_json_value_in_place(&mut clean_meta, compiled, profile, repo_root, stats);
        if let Some(obj) = out.as_object_mut() {
            obj.insert("metadata".into(), clean_meta);
        }
    }

    out
}

#[cfg(test)]
mod astra_tests {
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
    fn shipper_rejects_unfsynced_ingest_and_oversized_http_responses() {
        // 1. Oversized Content-Length and chunked bodies fail fast without unbounded allocation.
        let huge_cl = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            MAX_HUB_RESPONSE_BYTES + 1,
            "{}"
        );
        assert!(matches!(
            read_http_response_from(std::io::Cursor::new(huge_cl.as_bytes())),
            Err(HubError::Transport { status: Some(200), .. })
        ));
        let overflow_chunk = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n";
        assert!(matches!(
            read_http_response_from(std::io::Cursor::new(&overflow_chunk[..])),
            Err(HubError::Transport { status: Some(200), .. })
        ));

        // 2. `durable_fsynced: false` from `/api/hub/ingest_batch` returns `Err(HubError::Transport)`.
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
                        accepted_count: 1,
                        replayed_count: 0,
                        durable_fsynced: false,
                        verified_pack_digests: BTreeMap::new(),
                        custody_acks: Vec::new(),
                        origin_frontier: BTreeMap::new(),
                        snapshot: None,
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
