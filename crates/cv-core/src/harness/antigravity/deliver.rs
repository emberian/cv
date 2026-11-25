//! Live seed delivery to a running Antigravity language server
//! (`POST /product.v1.ConversationService/CreateConversation`, Connect HTTP/1.1 JSON).
//!
//! Guarantees at-most-one network send per deterministic `cvseed-<16hex>` tag unless explicitly
//! forced: an intent row is reserved in `state.db` (`deliveries`) under `BEGIN IMMEDIATE` before
//! opening the socket, and any post-write drop (`Uncertain`) runs read-only reconciliation over
//! local Antigravity stores before refusing duplicate transmission.

use super::{Adapter as _, Antigravity};
use crate::ir::Role;
use crate::state::StateStore;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{OptionalExtension as _, TransactionBehavior};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Seed {
    pub source_selector: String,
    pub boundary_ordinal: Option<usize>,
    pub model: String,
    pub title: String,
    pub project_id: String,
    pub tags: Vec<String>,
    pub body: String,
    pub tag: String,
}

impl Seed {
    pub fn new(
        source_selector: impl Into<String>,
        boundary_ordinal: Option<usize>,
        model: impl Into<String>,
        title: impl Into<String>,
        project_id: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let source_selector = source_selector.into();
        let model = model.into();
        let title = title.into();
        let project_id = project_id.into();
        let body = body.into();
        let tag = Self::compute_tag(&source_selector, boundary_ordinal, &body, &model);
        let tags = vec!["cv".into(), "successor".into(), tag.clone()];
        Self {
            source_selector,
            boundary_ordinal,
            model,
            title,
            project_id,
            tags,
            body,
            tag,
        }
    }

    /// `cvseed-` plus 16 lowercase hex chars of BLAKE3 over `(source_selector, boundary_ordinal, blake3(body), model)`.
    pub fn compute_tag(source_selector: &str, boundary_ordinal: Option<usize>, body: &str, model: &str) -> String {
        let body_digest = blake3::hash(body.as_bytes());
        let mut h = blake3::Hasher::new();
        h.update(b"cv-seed-tag-v1\0");
        h.update(source_selector.as_bytes());
        h.update(b"\0");
        if let Some(ord) = boundary_ordinal {
            h.update(ord.to_string().as_bytes());
        }
        h.update(b"\0");
        h.update(body_digest.as_bytes());
        h.update(b"\0");
        h.update(model.as_bytes());
        let hex = h.finalize().to_hex();
        format!("cvseed-{}", &hex[..16])
    }

    /// Seed body guaranteed to end with `self.tag` on its final line so read-only reconciliation
    /// can locate a created conversation from local Antigravity storage after a dropped HTTP reply.
    pub fn framed_body(&self) -> String {
        let trimmed = self.body.trim_end();
        if trimmed.lines().last().is_some_and(|line| line.trim() == self.tag) {
            return trimmed.to_owned();
        }
        format!("{trimmed}\n\n{}", self.tag)
    }

    /// Request identity digest independent of ephemeral local language-server ports so retrying
    /// after an Antigravity LS restart on a new loopback port reconciles cleanly.
    pub fn request_digest(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"cv-seed-delivery-v2\0");
        h.update(self.project_id.as_bytes());
        h.update(b"\0");
        h.update(self.model.as_bytes());
        h.update(b"\0");
        h.update(self.framed_body().as_bytes());
        h.finalize().to_hex().to_string()
    }
}

pub type Delivery = crate::harness::SeedDelivery;

fn parse_loopback_target(raw_addr: &str) -> Result<(String, Vec<SocketAddr>)> {
    let trimmed = raw_addr.trim();
    ensure!(!trimmed.is_empty(), "empty Antigravity language server address");
    if trimmed.starts_with("https://") {
        bail!("Antigravity local language server expects plain loopback HTTP, got https://");
    }
    let host_port = trimmed.strip_prefix("http://").unwrap_or(trimmed);
    ensure!(
        !host_port.is_empty() && !host_port.contains(['/', '?', '#', '@', '\r', '\n', ' ']),
        "invalid Antigravity language server authority {raw_addr:?}"
    );
    let addrs: Vec<SocketAddr> = host_port
        .to_socket_addrs()
        .with_context(|| format!("resolving Antigravity language server {host_port:?}"))?
        .collect();
    ensure!(
        !addrs.is_empty(),
        "Antigravity language server {host_port:?} resolved to zero addresses"
    );
    ensure!(
        addrs.iter().all(|a| a.ip().is_loopback()),
        "refusing non-loopback Antigravity language server address {host_port:?}"
    );
    Ok((host_port.to_owned(), addrs))
}

/// Read-only reconciliation: scan local Antigravity stores for a conversation created or modified
/// at or after `reserved_at_ms` (with 5s clock-granularity slack) whose first user message ends
/// with or contains `tag`.
pub fn reconcile_local_seed(tag: &str, reserved_at_ms: i64) -> Result<Option<String>> {
    let agy = Antigravity::detect();
    let refs = agy.discover()?;
    let floor_ms = reserved_at_ms.saturating_sub(5_000);
    for r in refs {
        let candidate_recent = r
            .created_at
            .or(r.updated_at)
            .map(|dt| dt.timestamp_millis() >= floor_ms)
            .or_else(|| {
                std::fs::metadata(&r.path)
                    .ok()?
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| (d.as_millis() as i64) >= floor_ms)
            })
            .unwrap_or(true);
        if !candidate_recent {
            continue;
        }
        let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
        let mut matched = false;
        let streamed = agy.try_stream(
            &r,
            &crate::stream::Demand::LEAN.with_spans(),
            &mut crate::stream::TryFnSink::new(|m: crate::ir::Message| {
                if m.role != Role::User {
                    return Ok(crate::stream::Flow::Continue);
                }
                for b in &m.content {
                    if let Some(t) = b.text() {
                        if let Ok(resolved) = t.try_resolve(&resolver) {
                            if resolved.contains(tag) {
                                matched = true;
                                break;
                            }
                        }
                    }
                }
                Ok(crate::stream::Flow::Stop)
            }),
        );
        if let Ok(session) = streamed {
            if matched {
                return Ok(Some(session.id));
            }
        }
    }
    Ok(None)
}

const DELIVERIES_DDL: &str = "
CREATE TABLE IF NOT EXISTS deliveries (
    tag             TEXT PRIMARY KEY,
    digest          TEXT NOT NULL,
    target_ls       TEXT NOT NULL,
    project_id      TEXT NOT NULL DEFAULT '',
    model           TEXT NOT NULL DEFAULT '',
    title           TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL,
    http_status     INTEGER,
    conversation_id TEXT,
    reserved_at     INTEGER NOT NULL,
    observed_at     INTEGER
) WITHOUT ROWID;";

fn ensure_deliveries_table(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(DELIVERIES_DDL)?;
    Ok(())
}

enum ReserveOutcome {
    Proceed,
    Existing(Delivery),
}

struct DeliveryRow {
    state: String,
    http_status: Option<i64>,
    conv_id: Option<String>,
    reserved_at: i64,
    prev_digest: String,
}

fn reserve_delivery(
    store: &StateStore,
    target_ls: &str,
    seed: &Seed,
    digest: &str,
    now_ms: i64,
    force_retry: bool,
) -> Result<ReserveOutcome> {
    let mut conn = store.open_writable()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_deliveries_table(&tx)?;
    let existing: Option<DeliveryRow> = tx
        .query_row(
            "SELECT state, http_status, conversation_id, reserved_at, digest FROM deliveries WHERE tag = ?1",
            [&seed.tag],
            |r| {
                Ok(DeliveryRow {
                    state: r.get(0)?,
                    http_status: r.get(1)?,
                    conv_id: r.get(2)?,
                    reserved_at: r.get(3)?,
                    prev_digest: r.get(4)?,
                })
            },
        )
        .optional()?;

    match existing {
        None => {
            tx.execute(
                "INSERT INTO deliveries(tag, digest, target_ls, project_id, model, title, state, reserved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'reserved', ?7)",
                rusqlite::params![
                    seed.tag,
                    digest,
                    target_ls,
                    seed.project_id,
                    seed.model,
                    seed.title,
                    now_ms,
                ],
            )?;
            tx.commit()?;
            Ok(ReserveOutcome::Proceed)
        }
        Some(DeliveryRow {
            state,
            http_status,
            conv_id,
            reserved_at,
            prev_digest,
        }) => {
            ensure!(
                prev_digest == digest || state == "created" || force_retry,
                "delivery tag {} already bound to a different request digest",
                seed.tag
            );
            if state == "created" {
                let cid = conv_id.unwrap_or_default();
                tx.commit()?;
                return Ok(ReserveOutcome::Existing(Delivery::Created {
                    conversation_id: cid,
                    tag: seed.tag.clone(),
                    reconciled: true,
                }));
            }
            if state == "refused" && !force_retry {
                let status = http_status.and_then(|s| u16::try_from(s).ok()).unwrap_or(400);
                tx.commit()?;
                return Ok(ReserveOutcome::Existing(Delivery::Refused {
                    status,
                    tag: seed.tag.clone(),
                    detail: "previously refused for this seed tag (pass --force to retry)".into(),
                }));
            }
            // State is `reserved` or `uncertain` (or `refused` with `force_retry`):
            // check read-only local reconciliation before sending anything over the wire.
            drop(tx);
            drop(conn);
            if let Ok(Some(found_id)) = reconcile_local_seed(&seed.tag, reserved_at) {
                let mut wconn = store.open_writable()?;
                let wtx = wconn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                wtx.execute(
                    "UPDATE deliveries SET state = 'created', conversation_id = ?2, observed_at = ?3 WHERE tag = ?1",
                    rusqlite::params![seed.tag, found_id, now_ms],
                )?;
                wtx.commit()?;
                return Ok(ReserveOutcome::Existing(Delivery::Created {
                    conversation_id: found_id,
                    tag: seed.tag.clone(),
                    reconciled: true,
                }));
            }
            let mut wconn = store.open_writable()?;
            let wtx = wconn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if force_retry {
                wtx.execute(
                    "UPDATE deliveries SET digest = ?2, target_ls = ?3, state = 'reserved', http_status = NULL,
                     reserved_at = ?4, observed_at = NULL WHERE tag = ?1",
                    rusqlite::params![seed.tag, digest, target_ls, now_ms],
                )?;
                wtx.commit()?;
                Ok(ReserveOutcome::Proceed)
            } else {
                wtx.execute(
                    "UPDATE deliveries SET state = 'uncertain', observed_at = COALESCE(observed_at, ?2) WHERE tag = ?1 AND state = 'reserved'",
                    rusqlite::params![seed.tag, now_ms],
                )?;
                wtx.commit()?;
                Ok(ReserveOutcome::Existing(Delivery::Uncertain {
                    tag: seed.tag.clone(),
                    detail: "prior delivery attempt did not settle and no matching local conversation was found; pass --force to retry".into(),
                }))
            }
        }
    }
}

fn mark_created(store: &StateStore, tag: &str, status: u16, conv_id: &str, now_ms: i64) -> Result<()> {
    let mut conn = store.open_writable()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE deliveries SET state = 'created', http_status = ?2, conversation_id = ?3, observed_at = ?4 WHERE tag = ?1",
        rusqlite::params![tag, i64::from(status), conv_id, now_ms],
    )?;
    tx.commit()?;
    Ok(())
}

fn mark_refused(store: &StateStore, tag: &str, status: u16, now_ms: i64) -> Result<()> {
    let mut conn = store.open_writable()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE deliveries SET state = 'refused', http_status = ?2, observed_at = ?3 WHERE tag = ?1 AND state != 'created'",
        rusqlite::params![tag, i64::from(status), now_ms],
    )?;
    tx.commit()?;
    Ok(())
}

fn mark_uncertain(store: &StateStore, tag: &str, status: Option<u16>, now_ms: i64) -> Result<()> {
    let mut conn = store.open_writable()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE deliveries SET state = 'uncertain', http_status = COALESCE(?2, http_status), observed_at = ?3
         WHERE tag = ?1 AND state != 'created'",
        rusqlite::params![tag, status.map(i64::from), now_ms],
    )?;
    tx.commit()?;
    Ok(())
}

fn clear_unwritten_reservation(store: &StateStore, tag: &str) -> Result<()> {
    let mut conn = store.open_writable()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM deliveries WHERE tag = ?1 AND state = 'reserved'", [tag])?;
    tx.commit()?;
    Ok(())
}

fn read_chunked_body(reader: &mut BufReader<TcpStream>, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated chunked HTTP response",
            ));
        }
        let hex = line.trim().split(';').next().unwrap_or("");
        let size = usize::from_str_radix(hex, 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid HTTP chunk size"))?;
        if size == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        if out.len().saturating_add(size) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response exceeds size cap",
            ));
        }
        let start = out.len();
        out.resize(start + size, 0);
        reader.read_exact(&mut out[start..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(out)
}

fn read_http_response(stream: TcpStream) -> std::io::Result<(u16, Vec<u8>)> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let n = reader.read_line(&mut status_line)?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before HTTP status line",
        ));
    }
    let mut parts = status_line.split_whitespace();
    let proto = parts.next().unwrap_or("");
    if !proto.starts_with("HTTP/1.") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected HTTP protocol {proto:?}"),
        ));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid HTTP status code"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF inside HTTP response headers",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, val)) = line.split_once(':') {
            let name = name.trim();
            let val = val.trim();
            if name.eq_ignore_ascii_case("content-length") {
                let len: usize = val
                    .parse()
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid Content-Length"))?;
                if len > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "HTTP Content-Length exceeds cap",
                    ));
                }
                content_length = Some(len);
            } else if name.eq_ignore_ascii_case("transfer-encoding") && val.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }

    let body = if chunked {
        read_chunked_body(&mut reader, MAX_RESPONSE_BYTES)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        buf
    } else {
        let mut buf = Vec::new();
        reader.take(MAX_RESPONSE_BYTES as u64 + 1).read_to_end(&mut buf)?;
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response exceeds size cap",
            ));
        }
        buf
    };
    Ok((status, body))
}

/// Deliver `seed` to the loopback Antigravity language server at `ls_addr` using `csrf`,
/// journaling reservation and outcome in `store`.
pub fn deliver_live_seed(
    store: &StateStore,
    ls_addr: &str,
    csrf: &str,
    seed: &Seed,
    force_retry: bool,
) -> Result<Delivery> {
    let csrf = csrf.trim();
    ensure!(
        !csrf.is_empty() && !csrf.contains(['\r', '\n']),
        "missing or invalid ANTIGRAVITY_CSRF_TOKEN"
    );
    let (authority, addrs) = parse_loopback_target(ls_addr)?;
    ensure!(
        !crate::env::frozen(),
        "--frozen forbids reserving or delivering a live Antigravity conversation"
    );

    let now_ms = chrono::Utc::now().timestamp_millis();
    let digest = seed.request_digest();
    if let ReserveOutcome::Existing(existing) = reserve_delivery(store, &authority, seed, &digest, now_ms, force_retry)?
    {
        return Ok(existing);
    }

    let mut tags = seed.tags.clone();
    if !tags.iter().any(|t| t == &seed.tag) {
        tags.push(seed.tag.clone());
    }
    let body_json = serde_json::to_vec(&json!({
        "projectId": seed.project_id,
        "title": seed.title,
        "model": seed.model,
        "tags": tags,
        "prompt": [{"text": seed.framed_body()}],
    }))?;

    let mut connect_err = None;
    let mut stream = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => connect_err = Some(e),
        }
    }
    let Some(mut stream) = stream else {
        // Zero bytes left this process: release the reservation cleanly.
        let _ = clear_unwritten_reservation(store, &seed.tag);
        return Err(anyhow::Error::from(connect_err.unwrap()))
            .with_context(|| format!("connecting to Antigravity language server at {authority}"));
    };
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));

    let header = format!(
        "POST /product.v1.ConversationService/CreateConversation HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Content-Type: application/json\r\n\
         Connect-Protocol-Version: 1\r\n\
         x-antigravity-csrf-token: {csrf}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body_json.len()
    );

    let write_res = stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(&body_json))
        .and_then(|()| stream.flush());

    if let Err(err) = write_res {
        let obs_ms = chrono::Utc::now().timestamp_millis();
        mark_uncertain(store, &seed.tag, None, obs_ms)?;
        if let Ok(Some(found_id)) = reconcile_local_seed(&seed.tag, now_ms) {
            mark_created(store, &seed.tag, 200, &found_id, obs_ms)?;
            return Ok(Delivery::Created {
                conversation_id: found_id,
                tag: seed.tag.clone(),
                reconciled: true,
            });
        }
        return Ok(Delivery::Uncertain {
            tag: seed.tag.clone(),
            detail: format!("write to language server interrupted after connect ({err})"),
        });
    }

    match read_http_response(stream) {
        Ok((status, resp_bytes)) if (200..=299).contains(&status) => {
            let obs_ms = chrono::Utc::now().timestamp_millis();
            let parsed: Option<String> = serde_json::from_slice::<Value>(&resp_bytes).ok().and_then(|v| {
                v.get("conversation")
                    .and_then(|c| c.get("id"))
                    .or_else(|| v.get("conversationId"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
            });
            if let Some(conv_id) = parsed {
                mark_created(store, &seed.tag, status, &conv_id, obs_ms)?;
                Ok(Delivery::Created {
                    conversation_id: conv_id,
                    tag: seed.tag.clone(),
                    reconciled: false,
                })
            } else {
                mark_uncertain(store, &seed.tag, Some(status), obs_ms)?;
                if let Ok(Some(found_id)) = reconcile_local_seed(&seed.tag, now_ms) {
                    mark_created(store, &seed.tag, status, &found_id, obs_ms)?;
                    return Ok(Delivery::Created {
                        conversation_id: found_id,
                        tag: seed.tag.clone(),
                        reconciled: true,
                    });
                }
                Ok(Delivery::Uncertain {
                    tag: seed.tag.clone(),
                    detail: format!("HTTP {status} reply lacked conversation.id"),
                })
            }
        }
        Ok((status, resp_bytes)) if (400..=499).contains(&status) => {
            let obs_ms = chrono::Utc::now().timestamp_millis();
            mark_refused(store, &seed.tag, status, obs_ms)?;
            let detail = String::from_utf8_lossy(&resp_bytes).trim().to_owned();
            Ok(Delivery::Refused {
                status,
                tag: seed.tag.clone(),
                detail: if detail.is_empty() {
                    format!("HTTP {status}")
                } else {
                    crate::ir::truncate(&detail, 240)
                },
            })
        }
        Ok((status, resp_bytes)) => {
            let obs_ms = chrono::Utc::now().timestamp_millis();
            mark_uncertain(store, &seed.tag, Some(status), obs_ms)?;
            if let Ok(Some(found_id)) = reconcile_local_seed(&seed.tag, now_ms) {
                mark_created(store, &seed.tag, status, &found_id, obs_ms)?;
                return Ok(Delivery::Created {
                    conversation_id: found_id,
                    tag: seed.tag.clone(),
                    reconciled: true,
                });
            }
            let detail = String::from_utf8_lossy(&resp_bytes).trim().to_owned();
            Ok(Delivery::Uncertain {
                tag: seed.tag.clone(),
                detail: if detail.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}: {}", crate::ir::truncate(&detail, 200))
                },
            })
        }
        Err(err) => {
            let obs_ms = chrono::Utc::now().timestamp_millis();
            mark_uncertain(store, &seed.tag, None, obs_ms)?;
            if let Ok(Some(found_id)) = reconcile_local_seed(&seed.tag, now_ms) {
                mark_created(store, &seed.tag, 200, &found_id, obs_ms)?;
                return Ok(Delivery::Created {
                    conversation_id: found_id,
                    tag: seed.tag.clone(),
                    reconciled: true,
                });
            }
            Ok(Delivery::Uncertain {
                tag: seed.tag.clone(),
                detail: format!("HTTP reply interrupted ({err})"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_request_digest_ignores_ephemeral_ls_port_across_restarts() {
        // #tooth Restarting the local language server on a new ephemeral loopback port must not
        // collide on `request_digest` for an uncertain reservation of the same `Seed`.
        let world = crate::test_world("agy-deliver-port-rotate");
        let store = StateStore::at_dir(&world.cv_home);
        let seed = Seed::new("src-1", Some(1), "model-x", "distilled: x", "proj-1", "pack body");
        let d1 = seed.request_digest();

        // Simulate an interrupted reservation against port 51001, then retry against port 59999.
        let r1 = reserve_delivery(&store, "127.0.0.1:51001", &seed, &d1, 1_700_000_000_000, false).unwrap();
        assert!(matches!(r1, ReserveOutcome::Proceed));
        let r2 = reserve_delivery(&store, "127.0.0.1:59999", &seed, &d1, 1_700_000_001_000, false).unwrap();
        assert!(
            matches!(r2, ReserveOutcome::Existing(Delivery::Uncertain { .. })),
            "new loopback port must settle to Uncertain instead of digest-mismatch error"
        );
    }
}
