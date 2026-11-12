//! Readers for account exports into cv; the CLI export group writes sessions out.
//!
//! **Account data-export** adapters — the archives you download from ChatGPT ("Export data") and
//! Claude.ai. Both pack *many* conversations into one big JSON file (`conversations.json`, often split
//! `conversations-000.json … -NNN.json`), so unlike the per-session harnesses one file yields many
//! [`SessionRef`]s. Two shapes, sniffed by content (never a filename):
//!
//! - **ChatGPT export** (`Harness::ChatGptExport`): each conversation has a `mapping` — a DAG of
//!   `node_id -> {parent, children[], message}` — with `current_node` as the active leaf. We walk the
//!   root→`current_node` chain (the live branch). A message has `author.role`, `content.{content_type,
//!   parts|text}`, and `recipient` (≠`all` ⇒ a tool call). `default_model_slug` is the model.
//! - **Claude.ai export** (`Harness::ClaudeExport`): each conversation has `chat_messages[]`, linear
//!   via `parent_message_uuid`; `sender` ∈ human/assistant; `content[]` blocks are
//!   text/thinking/tool_use/tool_result.
//!
//! Distinct from `claude` (Claude Code JSONL). Exports have no fixed home, so discovery is
//! **opt-in**: the sources registered with `cv config --add-export <path>`, unioned with
//! `$CV_EXPORTS` (`:`-separated dirs/files), scanned for `conversations*.json`. With nothing
//! registered no scan happens (see [`export_dirs`]).

use super::wire::anthropic;
use super::{ts_from_value, Adapter};
use crate::ir::*;
use crate::stream::{Demand, MessageSink};
use anyhow::{Context, Result};

mod native;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    ChatGpt,
    Claude,
}

impl Kind {
    fn harness(self) -> Harness {
        match self {
            Kind::ChatGpt => Harness::ChatGptExport,
            Kind::Claude => Harness::ClaudeExport,
        }
    }
}

// ── Adapters (one per export shape over the shared engine) ────────────────────

/// One export adapter: the archive's shape ([`Kind`]) and where its files come from — the
/// registered sources (`roots: None`, read at discovery time) or an explicit list.
#[derive(Clone)]
pub struct ExportAdapter {
    kind: Kind,
    roots: Option<Vec<PathBuf>>,
}

/// The registered-sources adapter of each shape ([`super::rootless`]).
pub(crate) static CHATGPT_EXPORT: ExportAdapter = ExportAdapter {
    kind: Kind::ChatGpt,
    roots: None,
};
pub(crate) static CLAUDE_EXPORT: ExportAdapter = ExportAdapter {
    kind: Kind::Claude,
    roots: None,
};

impl ExportAdapter {
    /// Discover only supplied export files/directories, without consulting configuration.
    pub(crate) fn with_roots(kind: Kind, roots: Vec<PathBuf>) -> Self {
        ExportAdapter {
            kind,
            roots: Some(roots),
        }
    }
}

impl Adapter for ExportAdapter {
    fn harness(&self) -> Harness {
        self.kind.harness()
    }
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone().unwrap_or_else(export_dirs)
    }
    fn discover(&self) -> Result<crate::Discovered> {
        Ok(discover_kind_under(self.kind, self.roots()))
    }
    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(self.kind, r, opts, sink))
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Where to look for export archives — the registered sources from the config file
/// (`cv config --add-export <path>`; see [`crate::config`]), unioned with `$CV_EXPORTS` (a
/// platform path-list via [`std::env::split_paths`]). **Opt-in**: with nothing registered and the
/// env unset, export discovery is a no-op, so the default `cv ls`/`discover_all` never pays to scan.
pub(crate) fn export_dirs() -> Vec<PathBuf> {
    let mut dirs = crate::config::load().exports;
    if let Some(spec) = crate::env::var_os("CV_EXPORTS") {
        dirs.extend(std::env::split_paths(&spec).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs.retain(|p| p.exists());
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Parse an in-memory `conversations.json` export array or single conversation object (used by
/// [`crate::ingest`], wasm-safe).
pub(crate) fn parse_all_str(text: &str) -> Result<Vec<Session>> {
    native::parse_all_str(text)
}

/// Every file under `roots` matching `pred` (depth ≤ 2; a dir entry is scanned, a file entry is
/// taken directly), sorted and deduped; a directory that cannot be read is added to `skipped`.
/// Shared with [`opensession`](super::opensession), which discovers `.opensession.json`
/// documents through the same registered sources — they have no fixed home either.
pub(crate) fn files_under(
    roots: Vec<PathBuf>,
    pred: fn(&Path) -> bool,
    skipped: &mut Vec<crate::Skipped>,
) -> Vec<PathBuf> {
    let mut entries = |dir: &Path| -> Vec<PathBuf> {
        let mut found = crate::Discovered::default();
        let listed = found
            .keep(dir, std::fs::read_dir(dir).map_err(Into::into))
            .into_iter()
            .flatten()
            .filter_map(|entry| found.keep(dir, entry.map_err(Into::into)).map(|e| e.path()))
            .collect();
        skipped.append(&mut found.skipped);
        listed
    };
    let mut out = Vec::new();
    for root in roots {
        if root.is_file() {
            if pred(&root) {
                out.push(root);
            }
            continue;
        }
        // root dir + one level of subdirs (an extracted export is usually a subfolder)
        for p in entries(&root) {
            if p.is_file() && pred(&p) {
                out.push(p);
            } else if p.is_dir() {
                for q in entries(&p) {
                    if q.is_file() && pred(&q) {
                        out.push(q);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_conversations_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "conversations.json" || (n.starts_with("conversations") && n.ends_with(".json")))
}

/// [`discover_kind_in`] over every `conversations*.json` under `roots`; an unreadable dir is
/// reported beside the archives' own skips.
fn discover_kind_under(kind: Kind, roots: Vec<PathBuf>) -> crate::Discovered {
    let mut skipped = Vec::new();
    let files = files_under(roots, is_conversations_file, &mut skipped);
    let mut out = discover_kind_in(kind, files);
    skipped.append(&mut out.skipped);
    out.skipped = skipped;
    out
}

fn discover_kind_in(kind: Kind, paths: Vec<PathBuf>) -> crate::Discovered {
    native::discover(kind, paths)
}

#[cfg(test)]
mod tests;
