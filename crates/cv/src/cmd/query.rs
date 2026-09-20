//! CLI glue for the query calculus (`cv_core::query`): parse `-q` strings and a [`SessionFacts`]
//! resolver that answers the external predicates (`text:` via the tantivy index,
//! `tool:`/`touched:`/`has:` from the parsed session's events) so the engine can be evaluated
//! against a real session. The reference itself is `cv schema` (`cmd/schema.rs`).

use anyhow::{anyhow, Result};
use cv_core::events::{self, Event};
use cv_core::ir::{Block, Session, SessionRef};
use cv_core::query::{ExtFacts, Facts, FieldId, SessionQuery, Tri};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

/// Parse an optional `-q` string into a query, surfacing parse errors. The core's messages point
/// at the reference by its 0.10 name; the CLI seam re-points them at `cv schema`.
pub(crate) fn build(query: Option<String>) -> Result<Option<SessionQuery>> {
    match query {
        Some(q) => Ok(Some(
            SessionQuery::parse(&q).map_err(|e| anyhow!(e.replace("`cv query`", "`cv schema`")))?,
        )),
        None => Ok(None),
    }
}

/// Pre-resolved full-text results: each distinct `text:` needle → the set of session ids that match
/// it in the tantivy index. Computed once before the scan so per-session evaluation is a set lookup.
pub(crate) struct TextSets(HashMap<String, HashSet<String>>);

impl TextSets {
    /// An empty set (no `text:` needles) — for callers with no query.
    pub(crate) fn empty() -> TextSets {
        TextSets(HashMap::new())
    }

    /// Run one full-text search per distinct `text:` needle in the query. If there's no index, warn
    /// once and treat every `text:` needle as matching nothing.
    pub(crate) fn resolve(query: &SessionQuery) -> TextSets {
        let mut map = HashMap::new();
        let needles = query.needles(FieldId::Text);
        if needles.is_empty() {
            return TextSets(map);
        }
        if !cv_search::default_tantivy_dir().exists() {
            eprintln!("cv query: text: needs a full-text index — run `cv index` (treating text: as no match)");
            for n in needles {
                map.insert(n, HashSet::new());
            }
            return TextSets(map);
        }
        for n in needles {
            // A generous cap: we want the full matching set, not a top-K ranking.
            let ids = cv_search::text_search(None, &n, 100_000)
                .map(|hits| hits.into_iter().map(|h| h.id).collect())
                .unwrap_or_default();
            map.insert(n, ids);
        }
        TextSets(map)
    }
}

/// The external-predicate resolver for one parsed session: `text:` from the precomputed [`TextSets`],
/// `tool:`/`touched:`/`has:` from the session's events (extracted in-memory), and the forest-tier
/// predicates (`subtool:`/`agent:`/`agents`/`workflow(s)`/`compactions`) from the sub-agent forest /
/// workflow state / compaction scan. Every expensive walk is memoized so it runs at most once.
pub(crate) struct SessionFacts<'a> {
    r: &'a SessionRef,
    session: &'a Session,
    text_sets: &'a TextSets,
    events: OnceCell<Vec<Event>>,
    forest: OnceCell<cv_core::tools::ForestTools>,
    tree: OnceCell<Vec<cv_core::SubagentInfo>>,
    workflows: OnceCell<Vec<cv_core::Workflow>>,
    compactions: OnceCell<usize>,
}

impl<'a> SessionFacts<'a> {
    pub(crate) fn new(r: &'a SessionRef, session: &'a Session, text_sets: &'a TextSets) -> Self {
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
    fn forest(&self) -> &cv_core::tools::ForestTools {
        self.forest.get_or_init(|| {
            cv_core::tools::forest_tools(self.r).unwrap_or(cv_core::tools::ForestTools { agents: vec![] })
        })
    }

    /// The sub-agent forest as lightweight infos (type + ref + journaled outcome).
    fn tree(&self) -> &[cv_core::SubagentInfo] {
        self.tree.get_or_init(|| cv_core::subagent_tree_of(self.r))
    }

    fn workflows(&self) -> &[cv_core::Workflow] {
        self.workflows.get_or_init(|| cv_core::workflows_of(self.r))
    }

    fn compaction_count(&self) -> usize {
        *self
            .compactions
            .get_or_init(|| cv_core::compaction::detect(self.r, false).map(|c| c.len()).unwrap_or(0))
    }
}

impl ExtFacts for SessionFacts<'_> {
    fn text(&self, needle: &str) -> Tri {
        // The id key is the session id; `needle` is already lowercased by the parser, matching the
        // map keys we inserted under.
        Tri::of(
            self.text_sets
                .0
                .get(needle)
                .is_some_and(|s| s.contains(&self.session.id)),
        )
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
                self.session.harness == cv_core::ir::Harness::Claude
                    && self
                        .session
                        .source_path
                        .as_deref()
                        .is_some_and(|p| !cv_core::harness::claude::subagent_refs(p).is_empty())
            }
            "compacted" => self.compaction_count() > 0,
            "workflows" => !self.workflows().is_empty(),
            _ => false,
        })
    }

    fn subtool(&self, name: &str) -> Tri {
        // A tool used by any *sub-agent* (skip the orchestrator sentinel).
        Tri::of(self.forest().agents.iter().any(|a| {
            a.agent != cv_core::tools::ORCHESTRATOR && a.histogram.tools.keys().any(|t| t.to_lowercase().contains(name))
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
pub(crate) fn matches_full(query: &SessionQuery, r: &SessionRef, session: &Session, text_sets: &TextSets) -> bool {
    let facts = SessionFacts::new(r, session, text_sets);
    query.matches(&Facts {
        r,
        session: Some(session),
        ext: &facts,
    })
}
