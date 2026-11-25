//! Store helpers shared by every adapter: the on-disk plumbing that is the same whichever harness
//! wrote the files. An adapter imports these and keeps only what is genuinely its own — where its
//! files are and what its records mean.

use std::path::PathBuf;

pub(crate) mod bytes;
pub(crate) mod document;
pub(crate) mod json;
pub(crate) mod jsonl;
mod jsonl_write;
pub(crate) mod selection;
#[cfg(test)]
pub(crate) use jsonl_write::publish_stage;
pub(crate) use jsonl_write::JsonlStage;
pub(crate) mod text;

#[cfg(feature = "sqlite")]
pub mod sqlite;
pub(crate) mod zst;

/// The store's standard buffered-I/O chunk: stage chunks, read/write buffers, copy buffers.
const IO_CHUNK: usize = 64 * 1024;
/// The buffer for streaming one stored artifact into another — a copy, not a parse.
const COPY_CHUNK: usize = 256 * 1024;

/// `$XDG_DATA_HOME` when set to an absolute path (the spec says a relative value is ignored), else
/// `~/.local/share`. The base of every XDG-shaped store (opencode, devin, goose, zed); `None` only
/// when there is no home directory at all.
pub(crate) fn xdg_data_dir() -> Option<PathBuf> {
    crate::env::Env::current().data_home()
}
