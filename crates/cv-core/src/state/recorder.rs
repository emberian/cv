//! Unified Transactional Recorder: idempotent `(op_id, req_digest)` binding + bounded capture inbox
//! over both a restrictive `0600` Unix domain socket batching owner and a `< 2ms` serverless
//! `BEGIN IMMEDIATE` fallback coordinated by an OS advisory ownership lock (`state.owner.lock`).

use super::{
    set_restrictive_file_mode, tables::current_process_identity, DurableReceipt, EventId, StateError, StateStore,
};
use fs4::fs_std::FileExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::mpsc,
    thread::JoinHandle,
};

/// Single logical append request submitted to the transactional recorder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendRequest {
    pub op_id: String,
    pub req_digest: String,
    pub producer_id: Option<String>,
    pub envelope: Value,
    pub payload_ref: Option<String>,
}

impl AppendRequest {
    pub fn new(op_id: impl Into<String>, req_digest: impl Into<String>, envelope: Value) -> Self {
        Self {
            op_id: op_id.into(),
            req_digest: req_digest.into(),
            producer_id: None,
            envelope,
            payload_ref: None,
        }
    }

    pub fn with_producer(mut self, producer_id: impl Into<String>) -> Self {
        self.producer_id = Some(producer_id.into());
        self
    }

    pub fn with_payload_ref(mut self, payload_ref: impl Into<String>) -> Self {
        self.payload_ref = Some(payload_ref.into());
        self
    }
}

/// Deterministic fault-injection modes for exercising owner crash handoff and retry idempotency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaultMode {
    #[default]
    None,
    /// Roll back inside the batch loop, drop client connections without replying, and release the
    /// ownership lock while leaving the stale socket path behind.
    CrashBeforeCommit,
    /// Commit `BEGIN IMMEDIATE` durably to WAL, then drop client connections BEFORE sending replies
    /// and release the ownership lock while leaving the stale socket path behind.
    CrashAfterCommitBeforeReply,
}

#[cfg(unix)]
#[derive(Debug, Serialize, Deserialize)]
enum WireResponse {
    Committed(DurableReceipt),
    Conflict {
        op_id: String,
        existing_digest: String,
        incoming_digest: String,
    },
    InvalidArgument(String),
    Error(String),
}

impl StateStore {
    /// Commit `envelope` under `(op_id, req_digest)` via the active Unix-socket batching owner when
    /// available, or fall back seamlessly to a direct `< 2ms` serverless `BEGIN IMMEDIATE`
    /// transaction under the OS ownership lock.
    ///
    /// - Retrying with the same `(op_id, req_digest)` returns the original [`DurableReceipt`] with
    ///   `replayed == true` and writes nothing new.
    /// - Reusing `op_id` with a different `req_digest` fails with [`StateError::OperationConflict`].
    pub fn append(&self, op_id: &str, req_digest: &str, envelope: &Value) -> Result<DurableReceipt, StateError> {
        self.append_request(&AppendRequest::new(op_id, req_digest, envelope.clone()))
    }

    /// Commit an [`AppendRequest`] (including optional `payload_ref` and explicit `producer_id`).
    pub fn append_request(&self, req: &AppendRequest) -> Result<DurableReceipt, StateError> {
        if crate::env::frozen() {
            return Err(StateError::Frozen);
        }
        validate_append_request(req)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut sleep_us = 200u64;

        loop {
            #[cfg(unix)]
            {
                let sock = self.socket_path();
                if sock.exists() {
                    match try_ipc_append(&sock, req) {
                        IpcAttempt::Done(res) => return res,
                        IpcAttempt::TransportFailed => {
                            // Owner may have crashed before/after commit or just shut down; fall
                            // through to ownership lock + recheck with the exact same `op_id`.
                        }
                    }
                }
            }

            if let Some(lock_file) = self.try_acquire_owner_lock()? {
                #[cfg(unix)]
                {
                    let sock = self.socket_path();
                    if sock.exists() {
                        // Recheck socket availability under `owner.lock` before declaring the
                        // socket stale and falling back to direct `BEGIN IMMEDIATE`.
                        match try_ipc_append(&sock, req) {
                            IpcAttempt::Done(res) => {
                                let _ = FileExt::unlock(&lock_file);
                                drop(lock_file);
                                return res;
                            }
                            IpcAttempt::TransportFailed => {
                                let _ = std::fs::remove_file(&sock);
                            }
                        }
                    }
                }
                let result = self.serverless_commit(req);
                let _ = FileExt::unlock(&lock_file);
                drop(lock_file);
                return result;
            }

            if Instant::now() >= deadline {
                return Err(StateError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out coordinating recorder ownership on {}",
                        self.owner_lock_path().display()
                    ),
                )));
            }

            std::thread::sleep(Duration::from_micros(sleep_us));
            sleep_us = (sleep_us * 2).min(4_000);
        }
    }

    pub(crate) fn try_acquire_owner_lock(&self) -> Result<Option<File>, StateError> {
        let lock_path = self.owner_lock_path();
        if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        set_restrictive_file_mode(&lock_path);
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => Ok(Some(file)),
            Ok(false) => Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(StateError::Io(err)),
        }
    }

    fn serverless_commit(&self, req: &AppendRequest) -> Result<DurableReceipt, StateError> {
        let mut conn = self.open_writable()?;
        let producer = req
            .producer_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.default_producer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut serverless_epoch: Option<String> = None;
        let receipt = commit_append_in_tx(&tx, producer, &mut serverless_epoch, "serverless", req)?;
        if let Some(ref ep) = serverless_epoch {
            let now_ms = chrono::Utc::now().timestamp_millis();
            tx.execute(
                "UPDATE writer_epochs SET closed_at = ?1 WHERE epoch = ?2",
                rusqlite::params![now_ms, ep],
            )?;
        }
        tx.commit()?;
        Ok(receipt)
    }
}

pub(crate) fn validate_append_request(req: &AppendRequest) -> Result<(), StateError> {
    if req.op_id.trim().is_empty() {
        return Err(StateError::InvalidArgument("op_id must not be empty".into()));
    }
    if req.req_digest.trim().is_empty() {
        return Err(StateError::InvalidArgument("req_digest must not be empty".into()));
    }
    Ok(())
}

/// Core single-operation state transition inside an already-open `BEGIN IMMEDIATE` transaction
/// (or per-item `SAVEPOINT`).
///
/// Shared identically between [`RecorderServer`] batch commits and serverless fallback commits.
pub(crate) fn commit_append_in_tx(
    tx: &Connection,
    default_producer_id: &str,
    epoch_slot: &mut Option<String>,
    mode: &str,
    req: &AppendRequest,
) -> Result<DurableReceipt, StateError> {
    validate_append_request(req)?;

    let existing: Option<(String, String, String, i64, i64)> = tx
        .query_row(
            "SELECT request_digest, producer_id, epoch, seq, local_commit_pos
             FROM operation_bindings
             WHERE op_id = ?1",
            [&req.op_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    if let Some((existing_digest, prod, epoch, seq, commit_pos)) = existing {
        if existing_digest == req.req_digest {
            return Ok(DurableReceipt {
                event_id: EventId::new(prod, epoch, seq as u64),
                local_commit_pos: commit_pos,
                replayed: true,
            });
        }
        return Err(StateError::OperationConflict {
            op_id: req.op_id.clone(),
            existing_digest,
            incoming_digest: req.req_digest.clone(),
        });
    }

    let producer_id = req
        .producer_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default_producer_id);
    if producer_id.trim().is_empty() {
        return Err(StateError::InvalidArgument("producer_id must not be empty".into()));
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    tx.execute(
        "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
         VALUES (?1, ?2, '{}', ?3, ?3)
         ON CONFLICT(producer_id) DO NOTHING",
        rusqlite::params![producer_id, default_host_label(), now_ms],
    )?;

    let epoch = match epoch_slot {
        Some(ep) => ep.clone(),
        None => {
            let fresh = uuid::Uuid::new_v4().to_string();
            let (pid, proc_start) = current_process_identity();
            tx.execute(
                "INSERT INTO writer_epochs(epoch, producer_id, mode, pid, proc_start_time, activated_at, closed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                rusqlite::params![fresh, producer_id, mode, pid, proc_start, now_ms],
            )?;
            *epoch_slot = Some(fresh.clone());
            fresh
        }
    };

    // If a per-request `producer_id` differs from the server's default producer, ensure that
    // `(producer_id, epoch)` exists in `writer_epochs` or track sequence cleanly in `epoch_counters`.
    let seq: i64 = tx.query_row(
        "INSERT INTO epoch_counters(producer_id, epoch, last_seq)
         VALUES (?1, ?2, 1)
         ON CONFLICT(producer_id, epoch) DO UPDATE SET last_seq = last_seq + 1
         RETURNING last_seq",
        rusqlite::params![producer_id, epoch],
        |r| r.get(0),
    )?;

    let envelope_json = serde_json::to_string(&req.envelope)?;
    tx.execute(
        "INSERT INTO capture_inbox(producer_id, epoch, seq, op_id, envelope_json, payload_ref, committed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            producer_id,
            epoch,
            seq,
            req.op_id,
            envelope_json,
            req.payload_ref,
            now_ms
        ],
    )?;
    let local_commit_pos = tx.last_insert_rowid();

    tx.execute(
        "INSERT INTO operation_bindings(op_id, request_digest, producer_id, epoch, seq, local_commit_pos, committed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            req.op_id,
            req.req_digest,
            producer_id,
            epoch,
            seq,
            local_commit_pos,
            now_ms
        ],
    )?;

    Ok(DurableReceipt {
        event_id: EventId::new(producer_id, epoch, seq as u64),
        local_commit_pos,
        replayed: false,
    })
}

pub(crate) fn default_host_label() -> String {
    crate::env::var("HOSTNAME")
        .or_else(|_| crate::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(unix)]
enum IpcAttempt {
    Done(Result<DurableReceipt, StateError>),
    TransportFailed,
}

#[cfg(unix)]
fn try_ipc_append(sock_path: &Path, req: &AppendRequest) -> IpcAttempt {
    let Ok(mut stream) = UnixStream::connect(sock_path) else {
        return IpcAttempt::TransportFailed;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut line = match serde_json::to_vec(req) {
        Ok(b) => b,
        Err(e) => return IpcAttempt::Done(Err(StateError::Json(e))),
    };
    line.push(b'\n');
    if stream.write_all(&line).is_err() || stream.flush().is_err() {
        return IpcAttempt::TransportFailed;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    match reader.read_line(&mut reply) {
        Ok(0) | Err(_) => IpcAttempt::TransportFailed,
        Ok(_) => match serde_json::from_str::<WireResponse>(reply.trim()) {
            Ok(WireResponse::Committed(receipt)) => IpcAttempt::Done(Ok(receipt)),
            Ok(WireResponse::Conflict {
                op_id,
                existing_digest,
                incoming_digest,
            }) => IpcAttempt::Done(Err(StateError::OperationConflict {
                op_id,
                existing_digest,
                incoming_digest,
            })),
            Ok(WireResponse::InvalidArgument(msg)) => IpcAttempt::Done(Err(StateError::InvalidArgument(msg))),
            Ok(WireResponse::Error(msg)) => IpcAttempt::Done(Err(StateError::Io(std::io::Error::other(msg)))),
            Err(_) => IpcAttempt::TransportFailed,
        },
    }
}

#[cfg(unix)]
struct BatchWork {
    req: AppendRequest,
    stream: UnixStream,
}

/// Exclusive Unix-domain-socket batching recorder holding `state.owner.lock` while active.
pub struct RecorderServer {
    store: StateStore,
    epoch: String,
    fault_mode: Arc<Mutex<FaultMode>>,
    stop: Arc<AtomicBool>,
    #[cfg(unix)]
    sock_path: PathBuf,
    #[cfg(unix)]
    acceptor_thread: Option<JoinHandle<()>>,
    #[cfg(unix)]
    committer_thread: Option<JoinHandle<()>>,
    #[cfg(not(unix))]
    _owner_lock: Option<File>,
}

impl RecorderServer {
    /// Acquire exclusive ownership on `store`, allocate a fresh 128-bit writer epoch, and start
    /// batching client appends over a restrictive `0600` Unix domain socket (`#[cfg(unix)]`).
    pub fn start(store: StateStore, producer_id: impl Into<String>) -> Result<Self, StateError> {
        if crate::env::frozen() {
            return Err(StateError::Frozen);
        }
        let producer_id = producer_id.into();
        if producer_id.trim().is_empty() {
            return Err(StateError::InvalidArgument("producer_id must not be empty".into()));
        }

        // Wait up to 3s for any in-flight `< 2ms` serverless writer to release `owner.lock`.
        let deadline = Instant::now() + Duration::from_secs(3);
        let owner_lock = loop {
            if let Some(lock) = store.try_acquire_owner_lock()? {
                break lock;
            }
            if Instant::now() >= deadline {
                return Err(StateError::Io(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another recorder already owns {}", store.owner_lock_path().display()),
                )));
            }
            std::thread::sleep(Duration::from_millis(1));
        };

        let conn = store.open_writable()?;
        let epoch = activate_owner_epoch(&conn, &producer_id)?;
        let fault_mode = Arc::new(Mutex::new(FaultMode::None));
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(unix)]
        {
            let sock_path = store.socket_path();
            let listener = bind_restrictive_0600(&sock_path)?;
            listener.set_nonblocking(true)?;

            let (tx, rx) = mpsc::channel::<BatchWork>();
            let stop_accept = Arc::clone(&stop);
            let accept_sock_path = sock_path.clone();

            let acceptor_thread = std::thread::Builder::new()
                .name("cv-state-accept".into())
                .spawn(move || {
                    while !stop_accept.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let tx_conn = tx.clone();
                                // Read client request inline with short timeout; local Unix frames
                                // arrive in microseconds, and malformed/idle peers time out cleanly.
                                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                                let mut line = String::new();
                                if let Ok(dup) = stream.try_clone() {
                                    use std::io::Read as _;
                                    const MAX_IPC_FRAME_BYTES: u64 = 16 * 1024 * 1024;
                                    let mut reader = BufReader::new(dup.take(MAX_IPC_FRAME_BYTES + 1));
                                    if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                                        if line.len() as u64 > MAX_IPC_FRAME_BYTES {
                                            let _ = write_wire_response(
                                                &mut stream,
                                                &WireResponse::InvalidArgument(
                                                    "IPC request exceeds 16 MiB frame limit".into(),
                                                ),
                                            );
                                        } else {
                                            match serde_json::from_str::<AppendRequest>(line.trim()) {
                                                Ok(req) => {
                                                    let _ = tx_conn.send(BatchWork { req, stream });
                                                }
                                                Err(err) => {
                                                    let _ = write_wire_response(
                                                        &mut stream,
                                                        &WireResponse::InvalidArgument(err.to_string()),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_micros(400));
                            }
                            Err(_) => {
                                if stop_accept.load(Ordering::Acquire) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                    }
                    let _ = accept_sock_path;
                })?;

            let stop_commit = Arc::clone(&stop);
            let fault_commit = Arc::clone(&fault_mode);
            let commit_sock_path = sock_path.clone();
            let commit_epoch = epoch.clone();

            let committer_thread = std::thread::Builder::new()
                .name("cv-state-commit".into())
                .spawn(move || {
                    let mut conn = conn;
                    let mut crashed_unclean = false;

                    loop {
                        let first = match rx.recv_timeout(Duration::from_millis(5)) {
                            Ok(item) => item,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if stop_commit.load(Ordering::Acquire) {
                                    // Drain anything queued before exiting cleanly.
                                    match rx.try_recv() {
                                        Ok(item) => item,
                                        Err(_) => break,
                                    }
                                } else {
                                    continue;
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        };

                        let mut batch = Vec::with_capacity(64);
                        batch.push(first);
                        while batch.len() < 128 {
                            match rx.try_recv() {
                                Ok(next) => batch.push(next),
                                Err(_) => break,
                            }
                        }

                        let mode = *fault_commit.lock().unwrap_or_else(|e| e.into_inner());
                        if execute_batch(&mut conn, &producer_id, &commit_epoch, batch, mode, &stop_commit) {
                            crashed_unclean = true;
                            break;
                        }
                    }

                    if !crashed_unclean {
                        let _ = std::fs::remove_file(&commit_sock_path);
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let _ = conn.execute(
                            "UPDATE writer_epochs SET closed_at = ?1 WHERE epoch = ?2",
                            rusqlite::params![now_ms, commit_epoch],
                        );
                    }
                    drop(conn);
                    let _ = FileExt::unlock(&owner_lock);
                    drop(owner_lock);
                })?;

            Ok(Self {
                store,
                epoch,
                fault_mode,
                stop,
                sock_path,
                acceptor_thread: Some(acceptor_thread),
                committer_thread: Some(committer_thread),
            })
        }

        #[cfg(not(unix))]
        {
            let _ = conn;
            Ok(Self {
                store,
                epoch,
                fault_mode,
                stop,
                _owner_lock: Some(owner_lock),
            })
        }
    }

    /// The fresh 128-bit writer epoch allocated when this server acquired exclusive ownership.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Underlying [`StateStore`] handle.
    pub fn store(&self) -> &StateStore {
        &self.store
    }

    /// Bound Unix domain socket path (`#[cfg(unix)]`).
    #[cfg(unix)]
    pub fn socket_path(&self) -> &Path {
        &self.sock_path
    }

    /// Configure deterministic fault injection on the next batch processed by this server.
    pub fn set_fault_mode(&self, mode: FaultMode) {
        *self.fault_mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    /// Gracefully drain queued appends, unlink the socket, close the writer epoch, and release the
    /// ownership lock.
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            if let Some(h) = self.acceptor_thread.take() {
                let _ = h.join();
            }
            if let Some(h) = self.committer_thread.take() {
                let _ = h.join();
            }
        }
        #[cfg(not(unix))]
        if let Some(lock) = self._owner_lock.take() {
            let _ = FileExt::unlock(&lock);
        }
    }
}

impl Drop for RecorderServer {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn activate_owner_epoch(conn: &Connection, producer_id: &str) -> Result<String, StateError> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let epoch = uuid::Uuid::new_v4().to_string();
    let (pid, proc_start) = current_process_identity();
    conn.execute(
        "INSERT INTO producers(producer_id, host_label, metadata_json, enrolled_at, updated_at)
         VALUES (?1, ?2, '{}', ?3, ?3)
         ON CONFLICT(producer_id) DO UPDATE SET updated_at = excluded.updated_at",
        rusqlite::params![producer_id, default_host_label(), now_ms],
    )?;
    conn.execute(
        "INSERT INTO writer_epochs(epoch, producer_id, mode, pid, proc_start_time, activated_at, closed_at)
         VALUES (?1, ?2, 'ipc_owner', ?3, ?4, ?5, NULL)",
        rusqlite::params![epoch, producer_id, pid, proc_start, now_ms],
    )?;
    Ok(epoch)
}

/// Bind `sock_path` with `0600` permissions without exposing a world-accessible socket inode.
#[cfg(unix)]
fn bind_restrictive_0600(sock_path: &Path) -> Result<UnixListener, StateError> {
    use std::os::unix::fs::PermissionsExt;
    let parent = sock_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if sock_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("cv-usock-"))
    {
        super::ensure_private_dir_0700(parent)?;
    } else {
        std::fs::create_dir_all(parent)?;
    }

    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    // Stage inside a private 0700 directory when path length budget allows so the socket inode has
    // mode 0600 BEFORE its directory entry ever appears at `sock_path`.
    let stage_dir = parent.join(format!(".s{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let stage_sock = stage_dir.join("s");
    let use_stage = {
        use std::os::unix::ffi::OsStrExt;
        stage_sock.as_os_str().as_bytes().len() <= 96 && super::ensure_private_dir_0700(&stage_dir).is_ok()
    };

    if use_stage {
        if let Ok(listener) = UnixListener::bind(&stage_sock) {
            std::fs::set_permissions(&stage_sock, std::fs::Permissions::from_mode(0o600))?;
            if std::fs::rename(&stage_sock, sock_path).is_ok() {
                let _ = std::fs::remove_dir(&stage_dir);
                return Ok(listener);
            }
            drop(listener);
            let _ = std::fs::remove_file(&stage_sock);
        }
        let _ = std::fs::remove_dir_all(&stage_dir);
    }

    let listener = UnixListener::bind(sock_path)?;
    std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Execute one coalesced batch transaction. Returns `true` if a simulated crash terminated the owner.
#[cfg(unix)]
fn execute_batch(
    conn: &mut Connection,
    producer_id: &str,
    epoch: &str,
    batch: Vec<BatchWork>,
    fault: FaultMode,
    stop: &AtomicBool,
) -> bool {
    let mut tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(err) => {
            let msg = err.to_string();
            for mut item in batch {
                let _ = write_wire_response(&mut item.stream, &WireResponse::Error(msg.clone()));
            }
            return false;
        }
    };

    let mut epoch_slot = Some(epoch.to_string());
    let mut outcomes = Vec::with_capacity(batch.len());

    for item in batch {
        let resp = match tx.savepoint() {
            Ok(sp) => {
                let mut item_epoch = epoch_slot.clone();
                match commit_append_in_tx(&sp, producer_id, &mut item_epoch, "ipc_owner", &item.req) {
                    Ok(receipt) => match sp.commit() {
                        Ok(()) => {
                            epoch_slot = item_epoch;
                            WireResponse::Committed(receipt)
                        }
                        Err(err) => WireResponse::Error(err.to_string()),
                    },
                    Err(StateError::OperationConflict {
                        op_id,
                        existing_digest,
                        incoming_digest,
                    }) => WireResponse::Conflict {
                        op_id,
                        existing_digest,
                        incoming_digest,
                    },
                    Err(StateError::InvalidArgument(msg)) => WireResponse::InvalidArgument(msg),
                    Err(other) => WireResponse::Error(other.to_string()),
                }
            }
            Err(err) => WireResponse::Error(err.to_string()),
        };
        outcomes.push((item.stream, resp));
    }

    if fault == FaultMode::CrashBeforeCommit {
        stop.store(true, Ordering::Release);
        drop(tx); // Rollback uncommitted statements.
        drop(outcomes); // Close sockets without sending replies.
        return true;
    }

    if let Err(err) = tx.commit() {
        let msg = err.to_string();
        for (mut stream, _) in outcomes {
            let _ = write_wire_response(&mut stream, &WireResponse::Error(msg.clone()));
        }
        return false;
    }

    if fault == FaultMode::CrashAfterCommitBeforeReply {
        stop.store(true, Ordering::Release);
        drop(outcomes); // Close sockets AFTER durable commit without sending replies.
        return true;
    }

    for (mut stream, resp) in outcomes {
        let _ = write_wire_response(&mut stream, &resp);
    }
    false
}

#[cfg(unix)]
fn write_wire_response(stream: &mut UnixStream, resp: &WireResponse) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(resp).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}
