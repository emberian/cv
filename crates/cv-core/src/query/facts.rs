//! The real [`ExtFacts`] resolver: answers the query calculus's external predicates against one
//! session, and filters a ref list with the full two-phase evaluation.
//!
//! Every fact source here is cv-core's own — the events extractor, the sub-agent forest, workflow
//! state, the compaction scan — with one exception: `text:` needs a full-text index, which lives
//! in cv-search (and cv-search depends on cv-core, so the dependency cannot point that way). So
//! `text:` arrives pre-resolved as a [`TextSets`], built by whichever door is asking: `cv -q` and
//! `cvd`'s `/api/stats?q=` both hand in the tantivy index, and a caller with no index hands in
//! nothing and `text:` matches nothing. That keeps one implementation of the calculus behind both
//! doors, which is the point — a `-q` that means something different on the daemon than on the
//! CLI is a silent wrong answer, not a missing feature.

use crate::events::{self, Event};
use crate::ir::{Block, Session, SessionRef};
use crate::query::{ExtFacts, Facts, FieldId, SessionQuery, Tri};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

/// Pre-resolved full-text results: each distinct `text:` needle → the set of session ids that match
/// it. Computed once before a scan so per-session evaluation is a set lookup.
#[derive(Debug, Default, Clone)]
pub struct TextSets(HashMap<String, HashSet<String>>);

impl TextSets {
    /// An empty set (no `text:` needles) — for callers with no query.
    pub fn empty() -> TextSets {
        TextSets(HashMap::new())
    }

    /// Resolve every distinct `text:` needle in `query` with `search`, which answers "which session
    /// ids match this needle". A caller with no index passes a closure returning an empty set, and
    /// every `text:` needle then matches nothing (the CLI warns once when it does this).
    pub fn resolve(query: &SessionQuery, search: impl Fn(&str) -> HashSet<String>) -> TextSets {
        let mut map = HashMap::new();
        for n in query.needles(FieldId::Text) {
            let ids = search(&n);
            map.insert(n, ids);
        }
        TextSets(map)
    }

    /// Whether `id` matched `needle` (already lowercased by the parser, matching the map keys).
    pub fn contains(&self, needle: &str, id: &str) -> bool {
        self.0.get(needle).is_some_and(|s| s.contains(id))
    }
}

/// The external-predicate resolver for one parsed session: `text:` from the precomputed
/// [`TextSets`], `tool:`/`touched:`/`has:` from the session's events (extracted in-memory), and the
/// forest-tier predicates (`subtool:`/`agent:`/`agents`/`workflow(s)`/`compactions`) from the
/// sub-agent forest / workflow state / compaction scan. Every expensive walk is memoized so it
/// runs at most once.
pub struct SessionFacts<'a> {
    r: &'a SessionRef,
    session: &'a Session,
    text_sets: &'a TextSets,
    events: OnceCell<Vec<Event>>,
    forest: OnceCell<crate::tools::ForestTools>,
    tree: OnceCell<Vec<crate::SubagentInfo>>,
    workflows: OnceCell<Vec<crate::Workflow>>,
    compactions: OnceCell<usize>,
}

impl<'a> SessionFacts<'a> {
    pub fn new(r: &'a SessionRef, session: &'a Session, text_sets: &'a TextSets) -> Self {
        SessionFacts {
            r,
            session,
            text_sets,
            events: OnceCell::new(),
            forest: OnceCell::new(),
            tree: OnceCell::new(),
            workflows: OnceCell::new(),
            compactions: OnceCell::new(),
        }
    }

    fn events(&self) -> &[Event] {
        self.events.get_or_init(|| {
            let cwd = self.session.cwd.as_deref();
            self.session
                .messages
                .iter()
                .enumerate()
                .flat_map(|(i, m)| events::extract(m, i, cwd))
                .collect()
        })
    }

    /// Per-agent tool histograms across the orchestrator + forest (parses every sub-agent — pricey).
    fn forest(&self) -> &crate::tools::ForestTools {
        self.forest
            .get_or_init(|| crate::tools::forest_tools(self.r).unwrap_or(crate::tools::ForestTools { agents: vec![] }))
    }

    /// The sub-agent forest as lightweight infos (type + ref + journaled outcome).
    fn tree(&self) -> &[crate::SubagentInfo] {
        self.tree.get_or_init(|| crate::subagent_tree_of(self.r))
    }

    fn workflows(&self) -> &[crate::Workflow] {
        self.workflows.get_or_init(|| crate::workflows_of(self.r))
    }

    fn compaction_count(&self) -> usize {
        *self
            .compactions
            .get_or_init(|| crate::compaction::detect(self.r, false).map(|c| c.len()).unwrap_or(0))
    }
}

impl ExtFacts for SessionFacts<'_> {
    fn text(&self, needle: &str) -> Tri {
        // The id key is the session id; `needle` is already lowercased by the parser, matching the
        // map keys we inserted under.
        Tri::of(self.text_sets.contains(needle, &self.session.id))
    }

    fn tool(&self, name: &str) -> Tri {
        Tri::of(
            self.events()
                .iter()
                .any(|e| e.tool.as_deref().is_some_and(|t| t.to_lowercase().contains(name))),
        )
    }

    fn touched(&self, path: &str) -> Tri {
        Tri::of(self.events().iter().any(|e| {
            matches!(e.kind, "file_edit" | "file_read")
                && e.target.as_deref().is_some_and(|t| t.to_lowercase().contains(path))
        }))
    }

    fn has(&self, flag: &str) -> Tri {
        Tri::of(match flag {
            "errors" => self.events().iter().any(|e| e.kind == "error"),
            "tools" => self.events().iter().any(|e| e.tool.is_some()),
            "images" => self
                .session
                .messages
                .iter()
                .any(|m| m.content.iter().any(|b| matches!(b, Block::Image { .. }))),
            "subagents" => {
                self.session.harness == crate::ir::Harness::Claude
                    && self
                        .session
                        .source_path
                        .as_deref()
                        .is_some_and(|p| !crate::harness::claude::subagent_refs(p).is_empty())
            }
            "compacted" => self.compaction_count() > 0,
            "workflows" => !self.workflows().is_empty(),
            _ => false,
        })
    }

    fn subtool(&self, name: &str) -> Tri {
        // A tool used by any *sub-agent* (skip the orchestrator sentinel).
        Tri::of(self.forest().agents.iter().any(|a| {
            a.agent != crate::tools::ORCHESTRATOR && a.histogram.tools.keys().any(|t| t.to_lowercase().contains(name))
        }))
    }

    fn agent_type(&self, needle: &str) -> Tri {
        Tri::of(self.tree().iter().any(|info| {
            info.agent_type
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(needle))
        }))
    }

    fn workflow(&self, needle: &str) -> Tri {
        Tri::of(self.workflows().iter().any(|w| {
            w.run_id.to_lowercase().contains(needle)
                || w.name.as_deref().is_some_and(|n| n.to_lowercase().contains(needle))
        }))
    }

    fn count(&self, field: FieldId) -> Option<u64> {
        Some(match field {
            FieldId::Agents => self.tree().len() as u64,
            FieldId::Workflows => self.workflows().len() as u64,
            FieldId::Compactions => self.compaction_count() as u64,
            _ => return None,
        })
    }
}

/// Full evaluation of `query` against a parsed session + its ref, using a [`SessionFacts`] resolver.
pub fn matches_full(query: &SessionQuery, r: &SessionRef, session: &Session, text_sets: &TextSets) -> bool {
    let facts = SessionFacts::new(r, session, text_sets);
    query.matches(&Facts {
        r,
        session: Some(session),
        ext: &facts,
    })
}

/// Apply a parsed query to a ref list in place: prune with the catalog-cheap prefilter, then — if
/// the query has any parse/index/events/forest term — parse each survivor and keep only full
/// matches. `cv ls`/`timeline`/`stats` and `cvd`'s `/api/stats?q=` all go through here, so `-q`
/// means the same thing on every door.
pub fn filter_refs(refs: &mut Vec<SessionRef>, query: &SessionQuery, text_sets: &TextSets) {
    refs.retain(|r| query.prefilter(r));
    if query.needs_parse() || query.needs_index() || query.needs_events() || query.needs_forest() {
        refs.retain(|r| {
            crate::harness::for_harness(r.harness)
                .and_then(|a| crate::stream::collect_with(a.as_ref(), r, &crate::ParseOptions::lazy()).ok())
                .is_some_and(|s| matches_full(query, r, &s, text_sets))
        });
    }
}
