//! Pi coding-agent adapter — `~/.pi/agent/sessions/` (or `$PI_CODING_AGENT_DIR/sessions`), and any
//! explicit pool RUNDIR passed in.
//!
//! Pi 1.0.2 writes a **version-3 tree JSONL**: one JSON object per line, `id`/`parentId` threading
//! the entries into a tree, the last entry the default leaf. The entry kinds are few:
//!
//! - `session` (always first): `{type, version, id, timestamp, cwd}`.
//! - `message`: `{type, id, parentId, timestamp, message:{role, content, …}}`. The `message` object
//!   is the same shape OpenClaw persists — `role` ∈ `user | assistant | toolResult | system`, and
//!   `content` a list of `text` / `thinking` / `toolCall` blocks — so the block codec is
//!   [`wire::anthropic`]'s `OPENCLAW` quirks, not a second parser.
//! - `compaction`: `{type, id, parentId, timestamp, summary, firstKeptEntryId, tokensBefore,
//!   fromHook, systemMessage}`. The `systemMessage` is a **full** system prompt (sections +
//!   `toolsAdded`), and it REPLACES the retained pre-compaction system messages rather than
//!   supplementing them.
//! - `model_change` / `thinking_level_change`: `{type, id, parentId, timestamp, modelId|provider,
//!   thinkingLevel}`.
//!
//! ## What this adapter does NOT do
//!
//! It does not reuse OpenClaw's *store discovery* (Pi has no `sessions.json` index and no sqlite
//! store) or its *v4 leaf controls* (Pi's leaf is simply the last entry). It does reuse the wire
//! block codec, which is the part that would otherwise be a second 1–3k-line parser.
//!
//! ## Reader semantics: history is not model context
//!
//! The tree is the history; the model context is a projection of it. Following Pi's own
//! `buildSessionProjection`: start at the last entry, walk its `parentId` chain to the root, then
//! apply the latest `compaction` — its `firstKeptEntryId` marks the oldest entry the compaction
//! kept, and its `systemMessage` is the system prompt from that point on. A branch-local
//! `context_edit` (when present) applies after the compaction. This adapter emits the entries in
//! that order and marks the compaction boundary, so a consumer can see which entries the
//! compaction kept without re-deriving it.

pub(crate) mod record;

use super::{parse_ts, Adapter, Discovered};
use crate::ir::*;
use crate::json::JsonSource;
use crate::stream::{Demand, MessageSink};
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Pi {
    roots: Vec<PathBuf>,
}

impl Pi {
    pub fn detect() -> Self {
        // `PI_CODING_AGENT_DIR` REPLACES the agent dir wholesale (native config.js:453-458 returns
        // only the override), so when it is set the default home is not consulted at all. Combining
        // the two would read sessions from a directory the user pointed away from.
        let base = match crate::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => Some(PathBuf::from(dir)),
            None => crate::env::home_dir().map(|h| h.join(".pi").join("agent")),
        };
        let mut roots = Vec::new();
        if let Some(base) = base {
            let sessions = base.join("sessions");
            if sessions.exists() {
                roots.push(sessions);
            }
        }
        Pi { roots }
    }

    /// An adapter over explicit session roots — a pool RUNDIR's `session/` directory, or any
    /// directory holding `*.jsonl` Pi sessions. Never consults the environment.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Pi { roots }
    }

    /// An adapter over `agent_dir/sessions`, the native layout.
    pub fn with_root(agent_dir: &Path) -> Self {
        let sessions = agent_dir.join("sessions");
        Pi {
            roots: sessions.exists().then_some(sessions).into_iter().collect(),
        }
    }
}

/// The adapter with no roots: it discovers nothing, and answers the facts that do not depend on
/// where the store lives.
pub(crate) static ROOTLESS: Pi = Pi { roots: Vec::new() };

impl Adapter for Pi {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<Discovered> {
        let mut out = Discovered::default();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in &self.roots {
            // A root may be a session directory or a pool RUNDIR whose sessions sit under
            // `session/`. Both are walked; a file already seen is not added twice.
            for entry in WalkDir::new(root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if !seen.insert(path.to_path_buf()) {
                    continue;
                }
                if let Some(r) = out.keep(path, record::discover_one(path)) {
                    out.refs.push(r);
                }
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let source = JsonSource::capture(&r.path)?;
            let result = record::stream(&r.path, r, opts, sink, &source);
            source.check()?;
            result
        })
    }
}
