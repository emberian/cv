//! Pi coding-agent adapter — `~/.pi/agent/sessions/` (or `$PI_CODING_AGENT_DIR/sessions`), and any
//! explicit pool RUNDIR passed in.
//!
//! Pi 1.0.2 writes a **version-3 tree JSONL**: one JSON object per line, `id`/`parentId` threading
//! the entries into a tree, the last entry the default leaf. The entry kinds are few:
//!
//! - `session` (always first): `{type, version, id, timestamp, cwd, parentSession?, model?}`.
//! - `message`: `{type, id, parentId, timestamp, message:{role, content, …}}`. The `message` object
//!   is the same shape OpenClaw persists — `role` ∈ `user | assistant | toolResult | system`, and
//!   `content` a list of `text` / `thinking` / `toolCall` / `image` blocks — so the block codec is
//!   [`wire::anthropic`](super::wire::anthropic)'s `OPENCLAW` quirks, not a second parser.
//! - `compaction`: `{type, id, parentId, timestamp, summary, firstKeptEntryId, tokensBefore,
//!   fromHook, systemMessage}`. The `systemMessage` is a **full** system prompt (sections +
//!   `toolsAdded`), and it REPLACES the retained pre-compaction system messages rather than
//!   supplementing them.
//! - `model_change` / `thinking_level_change`: `{type, id, parentId, timestamp, modelId, provider,
//!   thinkingLevel}`.
//! - `session_info`: `{type, id, parentId, timestamp, name}` — names the session (`Session::title`).
//! - `context_edit`: `{type, id, parentId, timestamp, targetId, replacement}` — branch-local edit
//!   (`null` drops `targetId`, an object swaps its `content`).
//!
//! ## What this adapter does NOT do
//!
//! It does not reuse OpenClaw's *store discovery* (Pi has no `sessions.json` index and no sqlite
//! store) or its *v4 leaf controls* (Pi's leaf is simply the last entry). It does reuse the wire
//! block and assistant-metadata codecs.
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

pub mod branch;
pub(crate) mod record;

use super::{parse_ts, ts_from_value, Adapter, Discovered, ResumeEnv};
use crate::ir::*;
use crate::json::JsonSource;
use crate::stream::{Demand, MessageSink};
use anyhow::{Context, Result};
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

fn is_jsonl_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
        && !super::is_cv_sidecar(path)
}

impl Adapter for Pi {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    /// A Pi selection is a native branch of its own file.
    fn writes_selection(&self) -> bool {
        true
    }

    /// Written beside `source` unless `out` says otherwise.
    fn write_selection(
        &self,
        source: &Path,
        selected: &Session,
        out: Option<&Path>,
        new_id: &str,
        sidecar: &[crate::distill::Elided],
    ) -> Result<(PathBuf, Option<PathBuf>, Vec<String>)> {
        let dir = out.unwrap_or_else(|| source.parent().unwrap_or(Path::new(".")));
        branch::write_with_sidecar(source, selected, dir, new_id, sidecar)
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    /// `pi --session <id>` reads `<agent_dir>/sessions/**/<file>.jsonl`, where `<agent_dir>` is
    /// `$PI_CODING_AGENT_DIR` when non-empty and `~/.pi/agent` otherwise.
    fn resume_env(&self, r: &SessionRef) -> Option<std::result::Result<ResumeEnv, String>> {
        const NO_STORE: &str = "no native store can be selected for this source; use the original harness/profile or port it into a supported native store";
        let Ok(canonical) = r.path.canonicalize() else {
            return Some(Err(format!("{} cannot be read", r.path.display())));
        };
        if !is_jsonl_file(&canonical) {
            return Some(Err(NO_STORE.into()));
        }
        let holds = |dir: &Path| {
            dir.join("sessions").canonicalize().is_ok_and(|sessions| {
                canonical
                    .strip_prefix(&sessions)
                    .is_ok_and(|rel| (1..=3).contains(&rel.components().count()))
            })
        };
        let home_default = crate::env::home_dir().map(|home| home.join(".pi").join("agent"));
        let ambient = crate::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        let mut env = ResumeEnv::default();
        if ambient.as_deref().or(home_default.as_deref()).is_some_and(holds) {
            return Some(Ok(env));
        }
        if ambient.is_some() && home_default.as_deref().is_some_and(holds) {
            env.unset.push("PI_CODING_AGENT_DIR");
            return Some(Ok(env));
        }
        let spelled = std::path::absolute(&r.path).ok();
        let dir = [spelled.as_deref(), Some(canonical.as_path())]
            .into_iter()
            .flatten()
            .find_map(|path| {
                path.ancestors().skip(1).take(3).find_map(|anc| {
                    let is_sessions = anc
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n == "sessions");
                    is_sessions.then(|| anc.parent()).flatten()
                })
            })
            .filter(|dir| holds(dir));
        Some(match dir {
            Some(dir) => {
                env.set
                    .insert("PI_CODING_AGENT_DIR".into(), dir.to_string_lossy().into_owned());
                Ok(env)
            }
            None => Err(NO_STORE.into()),
        })
    }

    fn discover(&self) -> Result<Discovered> {
        let mut out = Discovered::default();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in &self.roots {
            // A root may be a session directory or a pool RUNDIR whose sessions sit under
            // `session/`. Both are walked; a file already seen is not added twice.
            for entry in WalkDir::new(root).max_depth(3) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        out.skipped.extend(crate::Skipped::walk(&e));
                        continue;
                    }
                };
                let path = entry.path();
                if !entry.file_type().is_file() || !is_jsonl_file(path) {
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
        use super::store::jsonl::Read;
        crate::stream::checked(sink, |sink| {
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Pi, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let source = JsonSource::capture(&r.path)?;
            let result = (|| {
                let read = Read::with(source.clone()).with_context(|| format!("reading {}", r.path.display()))?;
                crate::stream::guarded(sink, &read, |sink| record::stream(&r.path, r, opts, sink, &read))
            })();
            if result.is_err() {
                source.check()?;
            }
            result
        })
    }
}

/// Parse an in-memory Pi JSONL transcript (including tree projection, compactions and
/// `context_edit`s), used by [`crate::ingest`] (wasm-safe).
pub(crate) fn parse_str(id: &str, text: &str) -> Result<Session> {
    record::parse_str(id, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_testkit::jsonl;
    use serde_json::json;

    #[test]
    fn no_flat_keys_on_lean_or_complete_pi_sessions() {
        let world = crate::test_world("pi-no-flat-keys");
        let dir = world.base.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}not-json-line\n",
                jsonl(&[
                    json!({"type":"session","version":3,"id":"s1","cwd":"/work","timestamp":"2026-11-19T10:00:00Z","parentSession":"p0"}),
                    json!({"type":"session_info","id":"si1","parentId":"s1","name":"My Pi Run"}),
                    json!({"type":"message","id":"sys","parentId":"si1","message":{"role":"system","content":"system instructions"}}),
                    json!({"type":"message","id":"m1","parentId":"sys","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}),
                    json!({"type":"message","id":"dead","parentId":"m1","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}),
                    json!({"type":"compaction","id":"c1","parentId":"m1","summary":"sum","firstKeptEntryId":"m1","tokensBefore":512,"fromHook":true}),
                    json!({"type":"model_change","id":"mc1","parentId":"c1","provider":"anthropic","modelId":"claude-sonnet-4"}),
                    json!({"type":"message","id":"m2","parentId":"mc1","message":{"role":"assistant","model":"claude-sonnet-4","content":[{"type":"text","text":"live"}]}}),
                    json!({"type":"context_edit","id":"e1","parentId":"m2","targetId":"m1","replacement":{"content":[{"type":"text","text":"edited hello"}]}}),
                    json!({"type":"future_unknown_entry","id":"u1","parentId":"e1","payload":{"x":1}}),
                ])
            ),
        )
        .unwrap();

        let adapter = Pi::with_roots(vec![dir]);
        let r = &adapter.discover().unwrap().refs[0];
        for demand in [
            Demand::LEAN,
            Demand::RICH,
            Demand::COMPLETE,
            Demand::LEAN.with_offsets(),
        ] {
            let s = crate::stream::collect_with(&adapter, r, &demand).unwrap();
            crate::harness::assert_no_flat_keys(&s);
            let roundtrip: Session = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
            crate::harness::assert_no_flat_keys(&roundtrip);
        }
    }

    // #tooth `Pi::discover` skips `.flat.jsonl` prune/distill sidecars written beside Pi sessions.
    #[test]
    fn discover_skips_flat_jsonl_sidecars() {
        let world = crate::test_world("pi-skip-sidecar");
        let dir = world.base.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("s1.jsonl"),
            jsonl(&[
                json!({"type":"session","version":3,"id":"s1","cwd":"/work","timestamp":"2026-11-19T10:00:00Z"}),
                json!({"type":"message","id":"m1","parentId":"s1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
            ]),
        )
        .unwrap();
        std::fs::write(
            dir.join("s1.flat.jsonl"),
            format!(
                "{}\n",
                crate::prune::sidecar_line("t1", "read", &serde_json::Value::Null, "stashed")
            ),
        )
        .unwrap();

        let discovered = Pi::with_roots(vec![dir]).discover().unwrap();
        assert_eq!(discovered.refs.len(), 1);
        assert_eq!(discovered.refs[0].id, "s1");
        assert!(discovered.skipped.is_empty(), "{:?}", discovered.skipped);
    }
}
