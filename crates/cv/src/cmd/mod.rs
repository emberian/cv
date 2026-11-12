//! One module per command family — see `main.rs` for the clap definitions and dispatch.

pub(crate) mod adopt;
pub(crate) mod archive;
pub(crate) mod blame;
pub(crate) mod browse;
pub(crate) mod cat;
pub(crate) mod compaction;
pub(crate) mod config;
pub(crate) mod distill;
pub(crate) mod doctor;
pub(crate) mod events;
pub(crate) mod formats;
pub(crate) mod hooks;
pub(crate) mod pack;
pub(crate) mod port;
pub(crate) mod query;
pub(crate) mod recipes;
pub(crate) mod reshape;
pub(crate) mod retire;
pub(crate) mod schema;
pub(crate) mod search;
pub(crate) mod serve;
pub(crate) mod serve_tasks;
pub(crate) mod session_json;
pub(crate) mod session_reports;
pub(crate) mod share;
pub(crate) mod task;
pub(crate) mod task_ops;
pub(crate) mod tools;
pub(crate) mod view;
pub(crate) mod workflow;

mod frames;

/// A resident reader also batches local capture. Another resident process may already own it;
/// ownership contention is normal, while real startup failures remain visible.
pub(crate) fn start_recorder() -> anyhow::Result<Option<cv_core::state::RecorderServer>> {
    if cv_core::env::frozen() || !cfg!(unix) {
        return Ok(None);
    }
    let store = cv_core::state::StateStore::default_store()?;
    let producer = store.default_producer().to_owned();
    match cv_core::state::RecorderServer::start(store, producer) {
        Ok(owner) => Ok(Some(owner)),
        Err(cv_core::state::StateError::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(e.into()),
    }
}
