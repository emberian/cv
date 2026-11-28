//! Best-effort hook transport. Only the resident recorder opens SQLite or decodes
//! lane transcripts. A hook sends one observation as one datagram on a Unix socket: the kernel
//! keeps each whole and apart from every other sender's, up to [`MAX_FRAME`], and a hook that
//! finds no receiver or a full queue drops it and returns.
use super::StateStore;
use crate::hooks::valid_id;
use crate::ir::Harness;
use crate::lanes::LaneEvent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Serialize, Deserialize)]
struct Observation {
    hook: String,
    harness: Harness,
    session: String,
    actor: String,
    ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    baggage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<(Offer, bool)>,
}

// The mailbox has no free-text fields. Construct the command again at delivery.
#[derive(Serialize, Deserialize)]
struct Offer {
    pointer: Pointer,
    revision: String,
    expires: i64,
}
#[derive(Serialize, Deserialize)]
enum Pointer {
    Inbox { count: u32, source: crate::source::FileId },
    Compact(u32),
    Adopt { count: u32, from: String },
    /// Directives in the newest compaction summary that repeat a tool result and no human prompt.
    Laundering(u32),
}
impl Pointer {
    fn kind(&self) -> &'static str {
        match self {
            Self::Compact(_) => "compaction",
            Self::Inbox { .. } => "decisions",
            Self::Adopt { .. } => "adopt",
            Self::Laundering(_) => "laundering",
        }
    }
    fn command(&self, harness: Harness, session: &str) -> Option<String> {
        match self {
            Self::Compact(n) if *n > 0 && valid_id(session) => {
                Some(format!("cv show {harness}:{session} --pre-compaction {n}"))
            }
            Self::Inbox { count, .. } if *count > 0 => Some("cv task inbox".into()),
            Self::Adopt { count, from } if *count > 0 && valid_id(from) && valid_id(session) => {
                Some(format!("cv adopt --join --from {from} --into {session}"))
            }
            Self::Laundering(count) if *count > 0 && valid_id(session) => {
                Some(format!("cv doctor {harness}:{session}"))
            }
            _ => None,
        }
    }
}

fn mailbox(store: &StateStore, harness: Harness, session: &str, actor: &str) -> PathBuf {
    let key = serde_json::to_vec(&(harness, session, actor)).expect("strings");
    store
        .path()
        .with_file_name("hook-offers")
        .join(blake3::hash(&key).to_hex().as_str())
}

fn take_offer(path: &Path) -> Option<Offer> {
    let claim = path.with_extension(std::process::id().to_string());
    std::fs::rename(path, &claim).ok()?;
    let result = (|| {
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&claim)
            .ok()?;
        if !f.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        f.take(513).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 512 {
            return None;
        }
        let offer: Offer = serde_json::from_slice(&bytes).ok()?;
        valid_id(&offer.revision).then_some(offer)
    })();
    let _ = std::fs::remove_file(claim);
    result
}
const OFFER_MS: i64 = 60_000;

/// A single nonblocking enqueue. No listener, contention and oversize all mean silence.
pub fn observe(hook: &str, harness: Harness, payload: &Value) -> Option<String> {
    if crate::env::frozen() {
        return None;
    }
    let session = payload.get("session_id")?.as_str()?;
    if !valid_id(session) {
        return None;
    }
    let store = StateStore::default_store().ok()?;
    let path = store.hook_socket_path();
    if !std::fs::symlink_metadata(&path).ok()?.file_type().is_socket() {
        return None;
    }
    // Connecting is what tells a live receiver from a socket file a dead one left behind.
    let socket = UnixDatagram::unbound().ok()?;
    socket.connect(&path).ok()?;
    socket.set_nonblocking(true).ok()?;
    buffer(&socket, libc::SO_SNDBUF, MAX_FRAME);
    let actor = crate::task::default_endpoint().unwrap_or_default();
    let ts = chrono::Utc::now().timestamp_millis();
    let emitted = matches!(hook, "start" | "decisions" | "compact")
        .then(|| take_offer(&mailbox(&store, harness, session, &actor)))
        .flatten();
    let hint = emitted.as_ref().and_then(|w| {
        if !(ts..=ts + OFFER_MS).contains(&w.expires) {
            return None;
        }
        if let Pointer::Inbox { source, .. } = &w.pointer {
            if task_source().as_ref() != Some(source) {
                return None;
            }
        }
        let command = w.pointer.command(harness, session)?;
        Some(match w.pointer {
            Pointer::Inbox { count, .. } | Pointer::Adopt { count, .. } => format!("{count} · {command}"),
            Pointer::Compact(_) => command,
            Pointer::Laundering(count) => {
                format!("{count} compaction directive(s) repeat tool output, not a human prompt · {command}")
            }
        })
    });
    let details = match hook {
        "lane" => {
            let mut fields = serde_json::Map::new();
            for k in [
                "hook_event_name",
                "agent_id",
                "agent_type",
                "transcript_path",
                "agent_transcript_path",
                "cwd",
                "pid",
            ] {
                if let Some(v) = payload.get(k) {
                    fields.insert(k.into(), v.clone());
                }
            }
            fields
                .entry("pid")
                .or_insert_with(|| Value::from(unsafe { libc::getppid() }));
            if let Some(line) = payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .and_then(|s| s.lines().find(|s| !s.trim().is_empty()))
            {
                fields.insert(
                    "last_assistant_message".into(),
                    Value::from(crate::ir::truncate(line.trim(), 200)),
                );
            }
            Some(Value::Object(fields))
        }
        "start" => {
            let mut fields = serde_json::Map::new();
            for k in ["transcript_path", "cwd"] {
                if let Some(v) = payload.get(k) {
                    fields.insert(k.into(), v.clone());
                }
            }
            (!fields.is_empty()).then_some(Value::Object(fields))
        }
        "compact" => payload.get("transcript_path").cloned(),
        _ => None,
    };
    let obs = Observation {
        hook: hook.into(),
        harness,
        session: session.into(),
        actor,
        ts,
        baggage: crate::env::var("CV_BAGGAGE")
            .ok()
            .filter(|s| crate::hooks::Baggage::parse(s).is_some()),
        payload: details,
        claim: emitted.map(|offer| (offer, hint.is_some())),
    };
    let bytes = serde_json::to_vec(&obs).ok()?;
    if bytes.len() > MAX_FRAME {
        return None;
    }
    (socket.send(&bytes).ok()? == bytes.len()).then_some(hint).flatten()
}

/// The largest observation, and so the largest datagram.
const MAX_FRAME: usize = 8192;
/// Receive queue: a burst of hooks while the recorder is busy committing.
const QUEUE_BYTES: usize = 256 * 1024;

/// Ask for a socket buffer of at least `bytes`. The default send buffer caps a datagram's size
/// (2048 bytes on macOS) and the default receive buffer holds few of them; where the system
/// grants less, larger observations are dropped by the send, never cut.
fn buffer(socket: &UnixDatagram, which: libc::c_int, bytes: usize) {
    let size = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            which,
            std::ptr::from_ref(&size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Bound reader attached to the recorder's ownership lifetime.
pub struct HookReceiver {
    socket: UnixDatagram,
    path: PathBuf,
    store: StateStore,
}

impl HookReceiver {
    pub fn bind(store: StateStore) -> Result<Self> {
        let path = store.hook_socket_path();
        let socket = super::recorder::bind_restrictive_0600::<UnixDatagram>(&path)?;
        socket.set_nonblocking(true)?;
        buffer(&socket, libc::SO_RCVBUF, QUEUE_BYTES);
        Ok(Self { socket, path, store })
    }

    pub(super) fn run(mut self, stop: Arc<AtomicBool>) {
        let mut poll = libc::pollfd {
            fd: self.socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while !stop.load(Ordering::Acquire) {
            if unsafe { libc::poll(&mut poll, 1, 50) } <= 0 {
                continue;
            }
            let _ = self.read_one();
        }
    }

    /// Fold one already enqueued observation. A quiet queue returns WouldBlock. A datagram that
    /// is not one observation is dropped whole: it cannot shift where the next one starts.
    pub fn read_one(&mut self) -> Result<()> {
        let mut bytes = [0; MAX_FRAME + 1];
        let n = self.socket.recv(&mut bytes)?;
        anyhow::ensure!(n <= MAX_FRAME, "invalid observation length");
        consume(&self.store, serde_json::from_slice(&bytes[..n])?)
    }
}

impl Drop for HookReceiver {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn consume(store: &StateStore, mut obs: Observation) -> Result<()> {
    let scope = crate::env::Env::reenter(crate::env::Env::current());
    scope.set("CLUSTERVISION_HOME", store.path().parent().expect("state directory"));
    if obs.actor.is_empty() {
        scope.remove("CV_ENDPOINT")
    } else {
        scope.set("CV_ENDPOINT", &obs.actor)
    }
    scope.remove("CV_BAGGAGE");
    if let Some(raw) = &obs.baggage {
        scope.set("CV_BAGGAGE", raw);
        if obs.hook != "lane" {
            super::tables::record_baggage(&store.open_writable()?, obs.harness.as_str(), &obs.session, raw)?;
        }
    }
    let subject = subject(&obs);
    if let Some((claim, delivered)) = &obs.claim {
        store.settle_offer(
            &obs.session,
            claim.pointer.kind(),
            &subject,
            &claim.revision,
            delivered.then_some(obs.ts),
        )?;
    }
    let (pointer, rev) = match obs.hook.as_str() {
        "lane" => {
            if let Some(mut payload) = obs.payload.take() {
                payload["session_id"] = Value::from(obs.session);
                if let Some(event) =
                    crate::harness::rootless(obs.harness).and_then(|a| a.hook_event(&payload).ok().flatten())
                {
                    record_lane(store, &event, &payload, obs.ts)?;
                }
            }
            return Ok(());
        }
        "compact" => {
            let Some(path) = obs.payload.as_ref().and_then(Value::as_str) else {
                return Ok(());
            };
            let r = crate::SessionRef::new(&obs.session, obs.harness, path);
            let boundaries = crate::compaction::detect(&r, false)?;
            let Some(last) = boundaries.last() else { return Ok(()) };
            let rev = last
                .boundary_uuid
                .clone()
                .unwrap_or_else(|| last.boundary_msg_idx.to_string());
            // One mailbox slot: a summary that promoted tool output to an instruction outranks
            // the pointer back to what the compaction left out. An unreadable scan warns of nothing.
            let laundered = crate::harness::for_harness(obs.harness)
                .and_then(|adapter| crate::doctor::newest_compaction_laundering(adapter.as_ref(), &r).ok())
                .map_or(0, |found| found.len());
            match u32::try_from(laundered)? {
                0 => (Pointer::Compact(u32::try_from(boundaries.len())?), rev),
                count => (Pointer::Laundering(count), format!("laundering:{rev}")),
            }
        }
        "start" | "decisions" => {
            if let Some(cand) = inbox_candidate()? {
                cand
            } else if obs.hook == "start" {
                let Some(cand) = adopt_candidate(&obs)? else {
                    return Ok(());
                };
                cand
            } else {
                return Ok(());
            }
        }
        _ => return Ok(()),
    };
    let Some(command) = pointer.command(obs.harness, &obs.session) else {
        return Ok(());
    };
    let offer = Offer {
        pointer,
        revision: blake3::hash(rev.as_bytes()).to_hex().to_string(),
        expires: obs.ts + OFFER_MS,
    };
    let path = mailbox(store, obs.harness, &obs.session, &obs.actor);
    if path.exists() {
        return Ok(());
    }
    super::ensure_private_dir_0700(path.parent().expect("mailbox directory"))?;
    if store.offer_whisper(
        obs.harness.as_str(),
        &obs.session,
        offer.pointer.kind(),
        &subject,
        &offer.revision,
        &command,
    )? {
        if let Err(error) = crate::publish::publish_reader(
            &path,
            "whisper",
            &serde_json::to_vec(&offer)?[..],
            crate::publish::Claim::New,
        ) {
            store.settle_offer(&obs.session, offer.pointer.kind(), &subject, &offer.revision, None)?;
            return Err(error);
        }
    }
    Ok(())
}

fn inbox_candidate() -> Result<Option<(Pointer, String)>> {
    let Some(source) = task_source() else { return Ok(None) };
    let model = crate::task::fold()?.model;
    if task_source() != Some(source) {
        return Ok(None);
    }
    let mut ids: Vec<_> = crate::task::inbox(&model, &crate::task::actor(None, "cv"))
        .into_iter()
        .filter(|e| matches!(e.reason, crate::task::InboxReason::DecisionOwed))
        .map(|e| &e.task.task_id)
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    ids.sort_unstable();
    Ok(Some((
        Pointer::Inbox {
            count: u32::try_from(ids.len())?,
            source,
        },
        blake3::hash(&serde_json::to_vec(&ids)?).to_hex().to_string(),
    )))
}

fn adopt_candidate(obs: &Observation) -> Result<Option<(Pointer, String)>> {
    if obs.session.starts_with("agent-") {
        return Ok(None);
    }
    let Some(details) = obs.payload.as_ref().and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(transcript) = details.get("transcript_path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(cwd) = details.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(adapter) = crate::harness::rootless(obs.harness) else {
        return Ok(None);
    };
    let pending = adapter.pending_adoption(&obs.session, Path::new(transcript), Path::new(cwd))?;
    Ok(pending.map(|p| {
        (
            Pointer::Adopt {
                count: p.count,
                from: p.from,
            },
            p.rev,
        )
    }))
}

fn task_source() -> Option<crate::source::FileId> {
    crate::source::FileId::at(&crate::task::tasks_dir().join("events.jsonl"))
}

fn subject(obs: &Observation) -> String {
    blake3::hash(&serde_json::to_vec(&(obs.harness, &obs.actor)).expect("strings"))
        .to_hex()
        .to_string()
}
fn record_lane(store: &StateStore, ev: &crate::hooks::Event, payload: &Value, ts: i64) -> Result<()> {
    let event = match ev.event.as_deref() {
        Some("SubagentStart") => crate::lanes::LaneEventKind::Start,
        Some("SubagentStop") => crate::lanes::LaneEventKind::Stop,
        _ => return Ok(()),
    };
    let Some(agent) = ev.agent.as_ref() else {
        return Ok(());
    };
    if crate::env::frozen() {
        return Ok(());
    }
    let now = chrono::DateTime::from_timestamp_millis(ts).ok_or_else(|| anyhow::anyhow!("invalid observation time"))?;

    let last_text = agent
        .last_message
        .as_deref()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
        .map(|l| crate::ir::truncate(l.trim(), 200));
    let norm_cwd = ev.normalized_cwd();

    let resolved_cwd = norm_cwd.or_else(|| ev.cwd.clone());
    crate::lanes::append_lane_event(&LaneEvent {
        ts: now,
        event,
        session_id: ev.session.id.clone(),
        agent_id: agent.id.clone(),
        agent_type: agent.kind.clone(),
        transcript_path: ev.transcript().map(Path::to_path_buf),
        agent_transcript_path: agent.transcript.clone(),
        cwd: resolved_cwd,
        last_text,
    })
    .context("appending to the lane ledger")?;

    // append_lane_event owns start/stop coalescing and lineage. Enrich the
    // same run with the spawning process and the exact endpoint from this call.
    let Some(mut run) = store
        .list_lane_runs_for_session(&ev.session.id)?
        .into_iter()
        .rev()
        .find(|r| r.agent_id.as_deref() == Some(agent.id.as_str()))
    else {
        return Ok(());
    };
    run.harness = Some(ev.harness.as_str().into());
    if event == crate::lanes::LaneEventKind::Start {
        run.pid = payload.get("pid").and_then(Value::as_i64);
        run.proc_start_time = run.pid.and_then(crate::state::tables::proc_start_time_of);
        run.boot_id = crate::lanes::current_boot_id();
    }
    store.record_lane_start(&run)?;
    if let Some(endpoint) = crate::task::default_endpoint() {
        store.bind_lane_endpoint(&run.run_id, &endpoint)?;
    }
    Ok(())
}
