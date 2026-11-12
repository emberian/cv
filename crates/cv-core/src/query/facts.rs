//! The real [`ExtFacts`] resolver: answers the query calculus's external predicates against one
//! session, and filters a ref list with the full two-phase evaluation.
//!
//! Every fact source here is cv-core's own — the events extractor, the sub-agent forest, workflow
//! state, the compaction scan — with one exception: `text:` needs a full-text index, which lives
//! in cv-search (and cv-search depends on cv-core, so the dependency cannot point that way). So
//! `text:` arrives pre-resolved as a [`TextSets`], built by whichever door is asking: `cv -q` and
//! `cv serve`'s `/api/stats?q=` both hand in the tantivy index, and a caller with no index hands in
//! nothing and `text:` matches nothing. That keeps one implementation of the calculus behind both
//! doors, which is the point — a `-q` that means something different on the daemon than on the
//! CLI is a silent wrong answer, not a missing feature.

use crate::events::{self, Event};
use crate::ir::{Block, Message, Session, SessionRef};
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
    pub fn resolve(query: &SessionQuery, mut search: impl FnMut(&str) -> HashSet<String>) -> TextSets {
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
    tree: OnceCell<Vec<crate::AgentRef>>,
    workflows: OnceCell<Vec<crate::Workflow>>,
    compactions: OnceCell<usize>,
    affordances: OnceCell<HashSet<crate::affordance::Kind>>,
    reduced: Option<&'a ReducedFacts>,
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
            affordances: OnceCell::new(),
            reduced: None,
        }
    }

    fn events(&self) -> anyhow::Result<&[Event]> {
        if self.events.get().is_none() {
            let mut rows = Vec::new();
            let mut extractor = events::EventExtractor::default();
            for (i, message) in self.session.messages.iter().enumerate() {
                extractor.extract(message, i, self.session.cwd.as_deref(), None, None, &mut rows)?;
            }
            let _ = self.events.set(rows);
        }
        Ok(self.events.get().expect("events initialized"))
    }

    fn forest(&self) -> anyhow::Result<&crate::tools::ForestTools> {
        if self.forest.get().is_none() {
            let _ = self.forest.set(crate::tools::forest_tools(self.r)?);
        }
        Ok(self.forest.get().expect("forest initialized"))
    }

    /// The sub-agent forest as lightweight infos (type + ref + journaled outcome).
    fn tree(&self) -> anyhow::Result<&[crate::AgentRef]> {
        if self.tree.get().is_none() {
            let _ = self.tree.set(crate::subagent_tree_of(self.r)?);
        }
        Ok(self.tree.get().expect("tree initialized"))
    }

    fn workflows(&self) -> &[crate::Workflow] {
        self.workflows.get_or_init(|| crate::workflows_of(self.r))
    }

    /// The affordance kinds this session shows, transcript and side store.
    fn affordance_kinds(&self) -> anyhow::Result<&HashSet<crate::affordance::Kind>> {
        if self.affordances.get().is_none() {
            let mut rows = crate::affordance::of_session(self.session)?;
            if let Some(a) = crate::harness::for_harness(self.r.harness) {
                rows.extend(a.store_affordances(self.r)?);
            }
            let _ = self.affordances.set(rows.into_iter().map(|a| a.kind).collect());
        }
        Ok(self.affordances.get().expect("affordances initialized"))
    }

    fn compaction_count(&self) -> anyhow::Result<usize> {
        // Preserve the detector's extra-enabled record acceptance policy.
        if self.compactions.get().is_none() {
            let _ = self.compactions.set(crate::compaction::detect(self.r, false)?.len());
        }
        Ok(*self.compactions.get().expect("compactions initialized"))
    }
}

impl ExtFacts for SessionFacts<'_> {
    fn model(&self, needle: &str, exact: bool) -> anyhow::Result<Tri> {
        Ok(self
            .reduced
            .and_then(|f| f.models.get(needle))
            .map_or(Tri::Unknown, |m| Tri::of(if exact { m.exact } else { m.contains })))
    }
    fn exact(&self, field: FieldId, needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(match field {
            FieldId::Tool => match self.reduced {
                Some(facts) => facts.exact_tools.contains(needle),
                None => self
                    .events()?
                    .iter()
                    .any(|e| e.tool.as_deref().is_some_and(|value| value.to_lowercase() == needle)),
            },
            FieldId::Touched => match self.reduced {
                Some(facts) => facts.exact_targets.contains(needle),
                None => self.events()?.iter().any(|e| {
                    matches!(e.kind, "file_edit" | "file_read")
                        && e.target.as_deref().is_some_and(|value| value.to_lowercase() == needle)
                }),
            },
            FieldId::Subtool => self.forest()?.agents.iter().any(|agent| {
                agent.agent != crate::tools::ORCHESTRATOR
                    && agent.histogram.tools.keys().any(|value| value.to_lowercase() == needle)
            }),
            FieldId::Agent => self.tree()?.iter().any(|info| {
                info.agent_type
                    .as_deref()
                    .is_some_and(|value| value.to_lowercase() == needle)
            }),
            FieldId::Workflow => self.workflows().iter().any(|workflow| {
                workflow.run_id.to_lowercase() == needle
                    || workflow
                        .name
                        .as_deref()
                        .is_some_and(|value| value.to_lowercase() == needle)
            }),
            _ => return Ok(Tri::Unknown),
        }))
    }
    fn text(&self, needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(self.text_sets.contains(needle, &self.session.id)))
    }
    fn tool(&self, name: &str) -> anyhow::Result<Tri> {
        if let Some(facts) = self.reduced {
            return Ok(Tri::of(facts.tools.contains(name)));
        }
        Ok(Tri::of(self.events()?.iter().any(|e| {
            e.tool.as_deref().is_some_and(|t| t.to_lowercase().contains(name))
        })))
    }
    fn touched(&self, path: &str) -> anyhow::Result<Tri> {
        if let Some(facts) = self.reduced {
            return Ok(Tri::of(facts.targets.contains(path)));
        }
        Ok(Tri::of(self.events()?.iter().any(|e| {
            matches!(e.kind, "file_edit" | "file_read")
                && e.target.as_deref().is_some_and(|t| t.to_lowercase().contains(path))
        })))
    }
    fn has(&self, flag: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(match flag {
            "errors" => match self.reduced {
                Some(f) => f.errors,
                None => self.events()?.iter().any(|e| e.kind == "error"),
            },
            "tools" => match self.reduced {
                Some(f) => f.has_tools,
                None => self.events()?.iter().any(|e| e.tool.is_some()),
            },
            "images" => self.reduced.map_or_else(
                || {
                    self.session
                        .messages
                        .iter()
                        .any(|m| m.content.iter().any(|b| matches!(b, Block::Image { .. })))
                },
                |f| f.images,
            ),
            // The forest `agents` counts, asked for membership only unless already read.
            "subagents" => match self.tree.get() {
                Some(tree) => !tree.is_empty(),
                None => !crate::subagent_tree_filtered_of(self.r, crate::SubagentDemand::Sessions, None, |_, _| true)?
                    .is_empty(),
            },
            "compacted" => self.compaction_count()? > 0,
            "workflows" => !self.workflows().is_empty(),
            other => match crate::affordance::Kind::parse(other) {
                Some(k) => match self.reduced {
                    Some(f) => f.counts.as_ref().is_some_and(|c| c.get(&k).is_some_and(|n| *n > 0)),
                    None => self.affordance_kinds()?.contains(&k),
                },
                None => false,
            },
        }))
    }
    fn subtool(&self, name: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(self.forest()?.agents.iter().any(|a| {
            a.agent != crate::tools::ORCHESTRATOR && a.histogram.tools.keys().any(|t| t.to_lowercase().contains(name))
        })))
    }
    fn agent_type(&self, needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(self.tree()?.iter().any(|info| {
            info.agent_type
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(needle))
        })))
    }
    fn workflow(&self, needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(self.workflows().iter().any(|w| {
            w.run_id.to_lowercase().contains(needle)
                || w.name.as_deref().is_some_and(|n| n.to_lowercase().contains(needle))
        })))
    }
    fn count(&self, field: FieldId) -> anyhow::Result<Option<u64>> {
        Ok(match field {
            FieldId::Agents => Some(self.tree()?.len() as u64),
            FieldId::Workflows => Some(self.workflows().len() as u64),
            FieldId::Compactions => Some(self.compaction_count()? as u64),
            _ => None,
        })
    }
}

/// Full evaluation of `query` against a parsed session + its ref, using a [`SessionFacts`] resolver.
pub fn matches_full(
    query: &SessionQuery,
    r: &SessionRef,
    session: &Session,
    text_sets: &TextSets,
) -> anyhow::Result<bool> {
    let facts = SessionFacts::new(r, session, text_sets);
    query.matches(&Facts {
        r,
        session: Some(session),
        ext: &facts,
    })
}

/// The transcript facts the query actually consults. Repeated calls and rich payloads contribute
/// no retained messages or event rows. Model/tool and matched-path state scale with query needles.
/// Unmatched relative paths use bounded private staging until EOF because the whole-session
/// evaluator resolves every target against the final (possibly late) cwd.
#[derive(Default)]
struct ReducedFacts {
    models: HashMap<String, ModelMatch>,
    tools: HashSet<String>,
    exact_tools: HashSet<String>,
    targets: HashSet<String>,
    exact_targets: HashSet<String>,
    errors: bool,
    has_tools: bool,
    images: bool,
    counts: Option<crate::affordance::Counts>,
}

#[derive(Default)]
struct ModelMatch {
    contains: bool,
    exact: bool,
}

impl ReducedFacts {
    fn note_model(&mut self, model: &str) {
        if !self.models.is_empty() {
            let model = model.to_lowercase();
            for (needle, matched) in &mut self.models {
                matched.contains |= model.contains(needle.as_str());
                matched.exact |= model == *needle;
            }
        }
    }
}

struct FactSink {
    facts: ReducedFacts,
    tools: Vec<String>,
    exact_tools: Vec<String>,
    targets: Vec<String>,
    exact_targets: Vec<String>,
    pending_targets: crate::spill::Ordered<String>,
    events: bool,
    /// The same extractor [`SessionFacts::events`] runs over a collected session, fed one streamed
    /// message at a time: one extraction path, so a native edit and its visible call count once.
    extractor: events::EventExtractor,
    index: usize,
    rows: Vec<events::Event>,
    fail: Option<anyhow::Error>,
}

impl FactSink {
    fn new(query: &SessionQuery) -> Self {
        let tools = query.needles(FieldId::Tool);
        let targets = query.needles(FieldId::Touched);
        let flags = query.needles(FieldId::Has);
        Self {
            facts: ReducedFacts {
                models: query
                    .needles(FieldId::Model)
                    .into_iter()
                    .map(|n| (n, ModelMatch::default()))
                    .collect(),
                ..Default::default()
            },
            events: !tools.is_empty()
                || !targets.is_empty()
                || flags.iter().any(|f| matches!(f.as_str(), "errors" | "tools")),
            tools,
            exact_tools: query.exact_needles(FieldId::Tool),
            targets,
            exact_targets: query.exact_needles(FieldId::Touched),
            pending_targets: Default::default(),
            extractor: Default::default(),
            index: 0,
            rows: Vec::new(),
            fail: None,
        }
    }

    fn targets_settled(&self) -> bool {
        self.facts.targets.len() == self.targets.len()
            && self
                .exact_targets
                .iter()
                .all(|needle| self.facts.exact_targets.contains(needle))
    }

    fn note_target(&mut self, target: String) -> anyhow::Result<()> {
        if self.targets_settled() {
            return Ok(());
        }
        // Contains matches survive a later cwd prefix, but an exact relative-path match
        // depends on the final cwd. Absolute paths are already final.
        let lowered = target.to_lowercase();
        let absolute = std::path::Path::new(&target).is_absolute();
        for needle in &self.targets {
            if lowered.contains(needle) {
                self.facts.targets.insert(needle.clone());
            }
        }
        if absolute {
            for needle in &self.exact_targets {
                if lowered == *needle {
                    self.facts.exact_targets.insert(needle.clone());
                }
            }
        }
        if self.targets_settled() {
            self.pending_targets = Default::default();
        } else if !absolute {
            self.pending_targets.push(target)?;
        }
        Ok(())
    }

    fn finish(mut self, session: &Session) -> anyhow::Result<ReducedFacts> {
        for row in self.pending_targets.rows()? {
            let p = row?;
            let path = std::path::Path::new(&p);
            let resolved = match session.cwd.as_deref() {
                Some(cwd) => cwd.join(path).to_string_lossy().to_lowercase(),
                None => p.to_lowercase(),
            };
            for needle in &self.targets {
                if resolved.contains(needle) {
                    self.facts.targets.insert(needle.clone());
                }
            }
            for needle in &self.exact_targets {
                if resolved == *needle {
                    self.facts.exact_targets.insert(needle.clone());
                }
            }
        }
        if let Some(model) = session.model.as_deref() {
            self.facts.note_model(model);
        }
        Ok(self.facts)
    }
}

impl FactSink {
    fn observe(&mut self, m: &Message) -> anyhow::Result<()> {
        if let Some(model) = m.model.as_deref() {
            self.facts.note_model(model);
        }
        self.facts.images |= m.content.iter().any(|b| matches!(b, Block::Image { .. }));
        if self.events {
            let mut rows = std::mem::take(&mut self.rows);
            self.extractor.extract(m, self.index, None, None, None, &mut rows)?;
            self.index += 1;
            for e in rows.drain(..) {
                self.facts.errors |= e.kind == "error";
                self.facts.has_tools |= e.tool.is_some();
                if !self.tools.is_empty() {
                    if let Some(tool) = e.tool {
                        let tool = tool.to_lowercase();
                        for needle in &self.tools {
                            if tool.contains(needle) {
                                self.facts.tools.insert(needle.clone());
                            }
                        }
                        for needle in &self.exact_tools {
                            if tool == *needle {
                                self.facts.exact_tools.insert(needle.clone());
                            }
                        }
                    }
                }
                if !self.targets.is_empty() && matches!(e.kind, "file_edit" | "file_read") {
                    if let Some(target) = e.target {
                        self.note_target(target)?;
                    }
                }
            }
            events::release_scratch(&mut rows);
            self.rows = rows;
        }
        Ok(())
    }
}

impl crate::MessageSink for FactSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.fail.take()
    }

    fn message(&mut self, m: Message) -> crate::Flow {
        match self.observe(&m) {
            Ok(()) => crate::Flow::Continue,
            Err(error) => {
                self.fail = Some(error);
                crate::Flow::Stop
            }
        }
    }
}

/// Query facts accumulated alongside another streaming consumer. Retains predicate state rather
/// than transcript messages. The same query used at construction is evaluated against the final
/// metadata shell; counted affordance facts may come from the enclosing counted stream.
pub struct QueryReducer<'a> {
    query: &'a SessionQuery,
    sink: FactSink,
}

impl<'a> QueryReducer<'a> {
    pub fn new(query: &'a SessionQuery) -> Self {
        Self {
            query,
            sink: FactSink::new(query),
        }
    }

    /// Inspect a borrowed message before its consumer drops it. Projection failures abort the
    /// pass, rather than becoming false query matches.
    pub fn observe(&mut self, message: &Message) -> anyhow::Result<()> {
        self.sink.observe(message)
    }

    /// Evaluate after successful EOF using final model/cwd/git and optional counted affordances.
    /// Forest/workflow/compaction predicates retain their established on-demand reads.
    pub fn matches(
        mut self,
        r: &SessionRef,
        session: &Session,
        text_sets: &TextSets,
        counts: Option<crate::affordance::Counts>,
    ) -> anyhow::Result<bool> {
        self.sink.facts.counts = counts;
        let reduced = self.sink.finish(session)?;
        let facts = SessionFacts {
            reduced: Some(&reduced),
            ..SessionFacts::new(r, session, text_sets)
        };
        self.query.matches(&Facts {
            r,
            session: Some(session),
            ext: &facts,
        })
    }
}

/// Evaluate a query in one lazy, counted pass, retaining only metadata and distinct predicate
/// facts. The parsed-session convenience [`matches_full`] remains available to callers that
/// already own a transcript. Forest/workflow/compaction walks remain memoized and evaluated on
/// demand; compaction detection keeps its separate extra-enabled pass for validation compatibility.
pub fn matches_streamed(
    query: &SessionQuery,
    adapter: &dyn crate::harness::Adapter,
    r: &SessionRef,
    text_sets: &TextSets,
) -> anyhow::Result<bool> {
    matches_streamed_with_probe(query, adapter, r, text_sets, &mut crate::source::Probe::default())
}

struct StreamedMatch {
    decision: anyhow::Result<bool>,
    source: crate::source::Baseline,
    counted: Option<crate::stream::Counted>,
}

fn matches_streamed_with_probe(
    query: &SessionQuery,
    adapter: &dyn crate::harness::Adapter,
    r: &SessionRef,
    text_sets: &TextSets,
    probe: &mut crate::source::Probe,
) -> anyhow::Result<bool> {
    let matched = reduce_streamed(query, adapter, r, text_sets, probe)?;
    if let Some(counted) = matched.counted {
        counted.record_with_source(r, &matched.source);
    }
    matched.decision
}

/// Reduce independently; publication belongs to the caller's serial stage.
fn reduce_streamed(
    query: &SessionQuery,
    adapter: &dyn crate::harness::Adapter,
    r: &SessionRef,
    text_sets: &TextSets,
    probe: &mut crate::source::Probe,
) -> anyhow::Result<StreamedMatch> {
    let mut sink = FactSink::new(query);
    let affordances = query
        .needles(FieldId::Has)
        .iter()
        .any(|flag| crate::affordance::Kind::parse(flag).is_some());
    let source = probe.snapshot(r);
    let read = crate::stream::stream_counted(
        adapter,
        r,
        &crate::Demand::LEAN.with_spans(),
        &mut sink,
        Default::default(),
    )?;
    let (session, mut counted) = (read.session, read.counted);
    if affordances {
        sink.facts.counts = counted.as_ref().map(|c| c.counts.clone());
    }
    let touches_subagents =
        query.mentions_field(FieldId::Agents) || query.needles(FieldId::Has).iter().any(|f| f == "subagents");
    if touches_subagents {
        if let Some(ref mut c) = counted {
            if let Some(ref mut f) = c.facts {
                crate::catalog::populate_subagent_facts(r, f);
            }
        }
    }
    let decision = (|| {
        let reduced = sink.finish(&session)?;
        let facts = SessionFacts {
            reduced: Some(&reduced),
            ..SessionFacts::new(r, &session, text_sets)
        };
        query.matches(&Facts {
            r,
            session: Some(&session),
            ext: &facts,
        })
    })();
    Ok(StreamedMatch {
        decision,
        source,
        counted,
    })
}

/// Facts available before parsing: indexed text membership and fresh catalog counts + session facts.
/// Other external predicates stay `Unknown`, preserving the query's three-valued evaluation.
struct RefFacts<'a> {
    id: &'a str,
    text_sets: &'a TextSets,
    counted: Option<&'a crate::catalog::Counted>,
    subagents_fresh: bool,
}

impl ExtFacts for RefFacts<'_> {
    fn model(&self, needle: &str, exact: bool) -> anyhow::Result<Tri> {
        let Some(facts) = self.counted.and_then(|c| c.facts.as_ref()) else {
            return Ok(Tri::Unknown);
        };
        Ok(Tri::of(facts.models.iter().any(|m| {
            if exact {
                m == needle
            } else {
                m.contains(needle)
            }
        })))
    }

    fn git(&self, needle: &str, exact: bool) -> anyhow::Result<Tri> {
        let Some(facts) = self.counted.and_then(|c| c.facts.as_ref()) else {
            return Ok(Tri::Unknown);
        };
        if exact {
            return Ok(Tri::of(
                [facts.git_branch.as_deref(), facts.git_remote.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|value| value.to_lowercase() == needle),
            ));
        }
        if facts.git_branch.is_none() && facts.git_remote.is_none() {
            return Ok(Tri::False);
        }
        let hay = format!(
            "{} {}",
            facts.git_branch.as_deref().unwrap_or_default(),
            facts.git_remote.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        Ok(Tri::of(hay.contains(needle)))
    }

    fn text(&self, needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::of(self.text_sets.contains(needle, self.id)))
    }

    fn has(&self, flag: &str) -> anyhow::Result<Tri> {
        match flag {
            "compacted" => Ok(self
                .counted
                .and_then(|c| c.facts.as_ref())
                .map_or(Tri::Unknown, |f| Tri::of(f.compactions > 0))),
            "subagents" => Ok(self
                .counted
                .and_then(|c| c.facts.as_ref())
                .filter(|_| self.subagents_fresh)
                .and_then(|f| f.subagent_count)
                .map_or(Tri::Unknown, |n| Tri::of(n > 0))),
            _ => Ok(match (crate::affordance::Kind::parse(flag), self.counted) {
                (Some(k), Some(counted)) => Tri::of(counted.counts.get(&k).is_some_and(|n| *n > 0)),
                _ => Tri::Unknown,
            }),
        }
    }

    fn count(&self, field: FieldId) -> anyhow::Result<Option<u64>> {
        Ok(match field {
            FieldId::Compactions => self
                .counted
                .and_then(|c| c.facts.as_ref())
                .map(|f| u64::from(f.compactions)),
            FieldId::Agents => self
                .counted
                .and_then(|c| c.facts.as_ref())
                .filter(|_| self.subagents_fresh)
                .and_then(|f| f.subagent_count)
                .map(u64::from),
            _ => None,
        })
    }
}

/// One cheap fact table and dependency probe for an entire query selection.
struct RefQuery<'a> {
    query: &'a SessionQuery,
    text_sets: &'a TextSets,
    heavy: bool,
    touches_subagents: bool,
    table: Option<crate::catalog::AffordanceTable>,
    demand_counts: bool,
    /// Opened on the first counted demand. A failed open is not remembered (the catalog's door
    /// warns once): the next demand tries again.
    reader: Option<crate::catalog::Reader>,
    probe: crate::source::Probe,
}

impl<'a> RefQuery<'a> {
    fn new(query: &'a SessionQuery, text_sets: &'a TextSets) -> Self {
        Self::with_counts_mode(query, text_sets, false)
    }

    fn with_counts_mode(query: &'a SessionQuery, text_sets: &'a TextSets, demand_counts: bool) -> Self {
        let touches_subagents =
            query.mentions_field(FieldId::Agents) || query.needles(FieldId::Has).iter().any(|f| f == "subagents");
        Self {
            query,
            text_sets,
            heavy: query.needs_parse() || query.needs_index() || query.needs_events() || query.needs_forest(),
            touches_subagents,
            table: None,
            demand_counts,
            reader: None,
            probe: crate::source::Probe::default(),
        }
    }

    fn cheap(&mut self, r: &SessionRef) -> anyhow::Result<Tri> {
        if !self.query.prefilter(r) {
            return Ok(Tri::False);
        }
        if !self.heavy && !self.query.needs_counted() {
            return Ok(Tri::True);
        }
        // Metadata and indexed text can settle a Boolean branch without opening the catalog.
        let without_counts = self.query.eval(&Facts {
            r,
            session: None,
            ext: &RefFacts {
                id: &r.id,
                text_sets: self.text_sets,
                counted: None,
                subagents_fresh: false,
            },
        })?;
        if !matches!(without_counts, Tri::Unknown) || !self.query.needs_counted() {
            return Ok(without_counts);
        }
        let counted = if self.demand_counts {
            if self.reader.is_none() {
                self.reader = crate::catalog::Reader::open();
            }
            self.reader
                .as_ref()
                .and_then(|reader| reader.counted_with_probe(r, &self.probe.snapshot(r), &mut self.probe))
        } else {
            self.table
                .get_or_insert_with(crate::catalog::AffordanceTable::load)
                .counted_with_probe(r, &self.probe.snapshot(r), &mut self.probe)
        };
        let subagents_fresh = self.touches_subagents
            && counted
                .as_ref()
                .and_then(|c| c.facts.as_ref())
                .is_some_and(|f| f.subagent_count.is_some() && f.subagent_guard.fresh(&mut self.probe));
        self.query.eval(&Facts {
            r,
            session: None,
            ext: &RefFacts {
                id: &r.id,
                text_sets: self.text_sets,
                counted: counted.as_ref(),
                subagents_fresh,
            },
        })
    }

    fn reduce(&mut self, r: &SessionRef) -> anyhow::Result<StreamedMatch> {
        match crate::harness::for_harness(r.harness) {
            Some(adapter) => reduce_streamed(self.query, adapter.as_ref(), r, self.text_sets, &mut self.probe),
            None => Err(anyhow::anyhow!("no adapter for {}", r.harness)),
        }
    }
}

/// Select at most `limit` matches from already-ranked refs; only refs with readable file metadata
/// occupy the window. Unlike [`filter_refs`], this has no
/// exact-total contract: only reads needed to decide the requested window can fail it. An
/// unreadable lower-ranked tail is never evaluated. Stable ties and ranking belong to the caller;
/// selected refs preserve their input order. A read error leaves the caller's list intact.
/// Zero returns an empty window without loading fact tables or demanding any transcript body.
/// Counts and their source provenance are read atomically per demanded record, rather than as one
/// fleet-table snapshot. No catalog read transaction spans body reductions or count publication.
pub fn filter_refs_limited(
    refs: &mut Vec<SessionRef>,
    query: &SessionQuery,
    text_sets: &TextSets,
    limit: usize,
) -> anyhow::Result<()> {
    if limit == 0 {
        refs.clear();
        return Ok(());
    }
    let _reads = crate::source::read_scope();
    let mut plan = RefQuery::with_counts_mode(query, text_sets, true);
    let mut writer: Option<crate::catalog::Writer> = None;
    let mut keep = vec![false; refs.len()];
    let mut selected = 0;
    for (index, r) in refs.iter().enumerate() {
        // An absent ref cannot appear in the window, so its body is not needed.
        if plan.probe.size(&r.path).is_none() {
            continue;
        }
        let matches = match plan.cheap(r)? {
            Tri::True => true,
            Tri::False => false,
            Tri::Unknown => {
                let matched = plan.reduce(r)?;
                if let Some(counted) = matched.counted {
                    if writer.is_none() {
                        writer = crate::catalog::Writer::open();
                    }
                    if let Some(writer) = &mut writer {
                        writer.record_counted(r, &matched.source, counted);
                    }
                }
                matched.decision?
            }
        };
        if matches {
            keep[index] = true;
            selected += 1;
            if selected == limit {
                break;
            }
        }
    }
    let mut decisions = keep.into_iter();
    refs.retain(|_| decisions.next().expect("one decision per ref"));
    Ok(())
}

/// Apply a parsed query to a ref list in place: prune with the catalog-cheap prefilter, then — if
/// the query has any parse/index/events/forest/counted term — evaluate each survivor against the
/// indexed text membership and catalog's affordance counts, keeping a definite match and dropping
/// a definite miss without a parse, and parse only what is still undecided. `cv ls`/`stats` and
/// `cv serve`'s `/api/stats?q=` all go through here, so `-q` means the same thing on every door.
pub fn filter_refs(refs: &mut Vec<SessionRef>, query: &SessionQuery, text_sets: &TextSets) -> anyhow::Result<()> {
    let _reads = crate::source::read_scope();
    let mut plan = RefQuery::new(query, text_sets);
    // Plan cheap decisions serially, sharing dependency probes. Do not mutate the caller's
    // list until every fallible decision succeeds.
    let mut keep = vec![false; refs.len()];
    let mut pending = Vec::new();
    for (index, r) in refs.iter().enumerate() {
        match plan.cheap(r)? {
            Tri::True => keep[index] = true,
            Tri::False => {}
            Tri::Unknown => pending.push(index),
        }
    }
    // Use the existing Rayon helpers, with finite batches rather than another executor or
    // semaphore. As in indexing, primary bytes are conservative source pressure, not a heap
    // estimate (SQLite contains other sessions, mapped JSON may own only descriptors).
    // A source exceeding the 2 GiB guard runs alone. Width is bounded by available cores.
    const SOURCE_BYTES: u64 = 2 << 30;
    let width = std::thread::available_parallelism().map_or(1, usize::from);
    let mut writer = (!pending.is_empty()).then(crate::catalog::Writer::open).flatten();
    let mut next = 0;
    while next < pending.len() {
        let start = next;
        let mut bytes: u64 = 0;
        while next < pending.len() && next - start < width {
            let size = refs[pending[next]].path.metadata().map_or(0, |m| m.len());
            if next > start && size > SOURCE_BYTES.saturating_sub(bytes) {
                break;
            }
            bytes = bytes.saturating_add(size);
            next += 1;
            if bytes >= SOURCE_BYTES {
                break;
            }
        }
        let mut results = crate::par_filter_map(pending[start..next].to_vec(), |index| {
            let r = &refs[index];
            let result = match crate::harness::for_harness(r.harness) {
                Some(adapter) => reduce_streamed(
                    query,
                    adapter.as_ref(),
                    r,
                    text_sets,
                    &mut crate::source::Probe::default(),
                ),
                None => Err(anyhow::anyhow!("no adapter for {}", r.harness)),
            };
            Some((index, result))
        });
        // Parallel helpers need not promise output order. Publish and propagate errors in the
        // original order, preserving deterministic selection and the caller's intact-list rule.
        results.sort_unstable_by_key(|(index, _)| *index);
        for (index, result) in results {
            let matched = result?;
            if let (Some(writer), Some(counted)) = (&mut writer, matched.counted) {
                writer.record_counted(&refs[index], &matched.source, counted);
            }
            keep[index] = matched.decision?;
        }
    }
    let mut decisions = keep.into_iter();
    refs.retain(|_| decisions.next().expect("one decision per ref"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    fn missing_source() -> SessionRef {
        SessionRef {
            id: "indexed-session".into(),
            harness: crate::Harness::Claude,
            path: std::env::temp_dir().join(format!("cv-query-missing-{}.jsonl", uuid::Uuid::new_v4())),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        }
    }

    fn text_sets(query: &SessionQuery, r: &SessionRef) -> TextSets {
        TextSets::resolve(query, |needle| {
            if needle == "hit" {
                HashSet::from([r.id.clone()])
            } else {
                HashSet::new()
            }
        })
    }

    #[test]
    fn counts_follow_boolean_shortcircuits_without_loading_reader_or_table() {
        let r = missing_source();
        for expression in [
            "id:absent AND has:plan",
            "id:indexed-session OR has:plan",
            "text:hit OR has:plan",
            "has:plan OR text:hit",
            "text:miss AND has:plan",
            "has:plan AND text:miss",
            "NOT (text:miss AND has:plan)",
        ] {
            let query = SessionQuery::parse(expression).unwrap();
            let texts = text_sets(&query, &r);
            for demand_counts in [false, true] {
                let mut plan = RefQuery::with_counts_mode(&query, &texts, demand_counts);
                let result = plan.cheap(&r).unwrap();
                assert!(!matches!(result, Tri::Unknown), "{expression}");
                assert!(plan.reader.is_none(), "{expression} should not demand counts");
                assert!(plan.table.is_none(), "{expression} should not load counts");
            }
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn limited_window_publishes_only_demanded_counts_and_reuses_them() {
        let world = crate::test_world("query_window_work");
        let refs: Vec<_> = (0..3)
            .map(|i| {
                let block = if i == 0 {
                    serde_json::json!({"type":"text","text":"no tool"})
                } else {
                    serde_json::json!({"type":"tool_use","id":"call","name":"TodoWrite","input":{"todos":[{"content":"step","status":"pending"}]}})
                };
                let id = format!("candidate-{i}");
                let path = world.write_session(
                    &id,
                    &[serde_json::json!({
                        "type":"assistant","uuid":id,"message":{"role":"assistant","content":[block]}
                    })],
                );
                SessionRef::new(id, crate::Harness::Claude, path)
            })
            .collect();
        let query = SessionQuery::parse("has:plan").unwrap();
        // Publication is a proxy for successful counted reads, not a byte/read counter.
        let fresh = || {
            let table = crate::catalog::AffordanceTable::load();
            refs.iter().map(|r| table.counts(r).is_some()).collect::<Vec<_>>()
        };
        assert_eq!(fresh(), [false, false, false]);
        let mut zero = refs.clone();
        filter_refs_limited(&mut zero, &query, &TextSets::empty(), 0).unwrap();
        assert_eq!(fresh(), [false, false, false]);
        let mut initial = refs.clone();
        filter_refs_limited(&mut initial, &query, &TextSets::empty(), 1).unwrap();
        assert_eq!(initial[0].id, refs[1].id);
        assert_eq!(fresh(), [true, true, false]);
        let mut warm = refs.clone();
        filter_refs_limited(&mut warm, &query, &TextSets::empty(), 1).unwrap();
        assert_eq!(warm[0].id, initial[0].id);
        assert_eq!(fresh(), [true, true, false]);
        // The warm prefix is decided by cheap cached facts; only Unknown invokes reduce.
        let texts = TextSets::empty();
        let mut plan = RefQuery::with_counts_mode(&query, &texts, true);
        assert!(matches!(plan.cheap(&refs[0]).unwrap(), Tri::False));
        assert!(matches!(plan.cheap(&refs[1]).unwrap(), Tri::True));
        assert_eq!(plan.reader.as_ref().unwrap().counts_work(), (2, 2));
        assert!(plan.table.is_none());
        assert!(matches!(plan.cheap(&refs[2]).unwrap(), Tri::Unknown));
        assert_eq!(plan.reader.as_ref().unwrap().counts_work(), (3, 2));
        for source in ["has:plan,errors", "has:errors,plan"] {
            let mixed = SessionQuery::parse(source).unwrap();
            assert!(mixed.needs_counted() && mixed.needs_events());
            let mut plan = RefQuery::with_counts_mode(&mixed, &texts, true);
            assert_eq!(
                plan.cheap(&refs[1]).unwrap(),
                Tri::True,
                "cached plan should settle mixed OR: {source}"
            );
            assert_eq!(
                plan.cheap(&refs[0]).unwrap(),
                Tri::Unknown,
                "absent plan cannot settle errors: {source}"
            );
        }
        let mut full = refs.clone();
        filter_refs(&mut full, &query, &TextSets::empty()).unwrap();
        assert_eq!(
            full.iter().map(|r| &r.id).collect::<Vec<_>>(),
            refs[1..].iter().map(|r| &r.id).collect::<Vec<_>>()
        );
        assert_eq!(fresh(), [true, true, true]);
    }

    #[test]
    fn limited_window_reads_needed_candidates_and_ignores_lower_ranked_errors() {
        let world = crate::test_world("query_window_errors");
        let good = world.write_session(
            "good",
            &[serde_json::json!({
                "type":"assistant","uuid":"good","message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"call","name":"Edit","input":{"file_path":"a.rs"}}
                ]}
            })],
        );
        let bad = world.home.join("unreadable-directory.jsonl");
        std::fs::create_dir(&bad).unwrap();
        let refs = vec![
            SessionRef::new("good", crate::Harness::Claude, good),
            SessionRef::new("bad", crate::Harness::Claude, bad),
        ];
        let query = SessionQuery::parse("tool:edit").unwrap();
        let mut one = refs.clone();
        filter_refs_limited(&mut one, &query, &TextSets::empty(), 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "good");
        let mut two = refs.clone();
        assert!(filter_refs_limited(&mut two, &query, &TextSets::empty(), 2).is_err());
        assert_eq!(
            two.iter().map(|r| &r.id).collect::<Vec<_>>(),
            refs.iter().map(|r| &r.id).collect::<Vec<_>>()
        );
        assert!(filter_refs(&mut refs.clone(), &query, &TextSets::empty()).is_err());
        let mut zero = refs;
        filter_refs_limited(&mut zero, &query, &TextSets::empty(), 0).unwrap();
        assert!(zero.is_empty());
    }

    #[test]
    fn limited_window_skips_absent_refs_and_boolean_shortcircuits() {
        let world = crate::test_world("query_window_presence");
        let missing = missing_source();
        let path = world.home.join("present-unreadable.jsonl");
        std::fs::create_dir(&path).unwrap();
        let present = SessionRef::new("present", crate::Harness::Claude, path);
        let refs = vec![missing.clone(), present.clone()];
        let query = SessionQuery::parse("tool:edit OR text:hit").unwrap();
        let texts = TextSets::resolve(&query, |_| refs.iter().map(|r| r.id.clone()).collect());
        let mut existing = refs.clone();
        filter_refs_limited(&mut existing, &query, &texts, 1).unwrap();
        assert_eq!(existing[0].id, present.id);
    }

    #[test]
    fn parallel_filter_keeps_input_order_and_does_not_prune_on_error() {
        let world = crate::test_world("parallel_query_order");
        let refs: Vec<_> = (0..35)
            .map(|i| {
                let id = format!("ordered-{i}");
                let path = world.write_session(
                    &id,
                    &[serde_json::json!({
                        "type":"assistant","uuid":id,"message":{"role":"assistant","content":[
                            {"type":"tool_use","id":"call","name":if i % 3 == 0 { "Edit" } else { "Read" },
                            "input":{"file_path":"a.rs"}}
                        ]}
                    })],
                );
                SessionRef::new(id, crate::Harness::Claude, path)
            })
            .collect();
        let query = SessionQuery::parse("tool:edit").unwrap();
        let mut actual = refs.clone();
        filter_refs(&mut actual, &query, &TextSets::empty()).unwrap();
        assert_eq!(
            actual.iter().map(|r| &r.id).collect::<Vec<_>>(),
            refs.iter().step_by(3).map(|r| &r.id).collect::<Vec<_>>()
        );
        let mut with_error = refs;
        with_error.insert(17, missing_source());
        let before = with_error.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert!(filter_refs(&mut with_error, &query, &TextSets::empty()).is_err());
        assert_eq!(with_error.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), before);
    }

    #[test]
    fn indexed_text_queries_filter_without_readable_source() {
        let r = missing_source();
        assert!(!r.path.exists());
        for (source, matches) in [
            ("text:hit", true),
            ("text:miss", false),
            ("NOT text:miss", true),
            ("NOT text:hit", false),
            ("text:hit OR tool:bash", true),
            ("tool:bash OR text:hit", true),
            ("text:miss AND tool:bash", false),
            ("tool:bash AND text:miss", false),
            ("NOT (text:miss AND tool:bash)", true),
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = text_sets(&query, &r);
            let mut refs = vec![r.clone()];
            filter_refs(&mut refs, &query, &texts).unwrap();
            assert_eq!(refs.len(), usize::from(matches), "{source}");
        }
    }

    #[test]
    fn indexed_membership_preserves_unknown_boolean_branches() {
        let r = missing_source();
        for (source, expected) in [
            ("text:miss AND tool:bash", Tri::False),
            ("tool:bash AND text:miss", Tri::False),
            ("text:hit AND tool:bash", Tri::Unknown),
            ("text:hit OR tool:bash", Tri::True),
            ("text:miss OR tool:bash", Tri::Unknown),
            ("NOT (text:hit AND tool:bash)", Tri::Unknown),
            ("NOT (text:miss OR tool:bash)", Tri::Unknown),
            ("NOT text:hit OR tool:bash", Tri::Unknown),
            ("NOT text:miss AND tool:bash", Tri::Unknown),
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = text_sets(&query, &r);
            let ext = RefFacts {
                id: &r.id,
                text_sets: &texts,
                counted: None,
                subagents_fresh: false,
            };
            assert_eq!(
                query
                    .eval(&Facts {
                        r: &r,
                        session: None,
                        ext: &ext
                    })
                    .unwrap(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn streamed_facts_match_collection_with_late_metadata_and_boolean_queries() {
        let world = crate::test_world("streamed_query_facts");
        let payload = "rich escaped\n\"🌙\"".repeat(400);
        let path = world.write_session("facts", &[
            serde_json::json!({"type":"assistant","uuid":"early","message":{"role":"assistant", "model":"EarlyModel",
                "content":[{"type":"tool_use","id":"e","name":"Edit","input":{"file_path":"./src/A.rs","rich":payload}},
                    {"type":"tool_use","id":"r","name":"Read","input":{"file_path":"../other.rs"}},
                    {"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/Absolute/Λ.rs"}},
                    {"type":"tool_use","id":"b","name":"Bash","input":{"command":"git status"}}]}}),
            serde_json::json!({"type":"user","uuid":"image","message":{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":payload}},
                {"type":"tool_result","tool_use_id":"e","is_error":true,"content":"permission denied"}]}}),
            serde_json::json!({"type":"system","uuid":"boundary","subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto","preTokens":1000}}),
            serde_json::json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,
                "message":{"role":"user","content":payload}}),
            serde_json::json!({"type":"assistant","uuid":"late","cwd":"/Late/Work","permissionMode":"plan",
                "gitBranch":"FeatureBranch","message":{"role":"assistant","model":"LateModel","content":[
                    {"type":"tool_use","id":"e2","name":"Edit","input":{"file_path":"./src/A.rs","rich":payload}},
                    {"type":"text","text":"done"}]}}),
            serde_json::json!({"type":"ai-title","aiTitle":"final title"}),
        ]);
        let tasks = world.home.join(".claude/tasks/facts");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            tasks.join("1.json"),
            r#"{"id":"1","subject":"side-store plan","status":"pending"}"#,
        )
        .unwrap();
        let mut r = SessionRef::new("facts", crate::Harness::Claude, path);
        r.cwd = Some("/Stale/Discovery".into());
        r.title = Some("discovery title".into());
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = crate::stream::collect_with(adapter.as_ref(), &r, &crate::Demand::LEAN.with_spans()).unwrap();
        assert_eq!(full.cwd.as_deref(), Some(std::path::Path::new("/Late/Work")));
        for required in [
            "model=earlymodel AND model=latemodel",
            "tool:edit AND tool:bash",
            "touched:/late/work/src/a.rs AND has:images AND has:errors",
            "has:plan",
            "compactions=1",
        ] {
            assert!(
                matches_full(&SessionQuery::parse(required).unwrap(), &r, &full, &TextSets::default()).unwrap(),
                "{required}"
            );
        }
        for source in [
            "model:early",
            "model=earlymodel",
            "model=early",
            "model:late",
            "model:absent",
            "model:early AND model:late",
            "NOT model:early",
            "NOT model:absent",
            "git:feature",
            "git:absent",
            "git=featurebranch",
            "git=feature",
            "tool=edit",
            "tool=edi",
            "tool:bash OR tool=edit",
            "tool=edi OR tool=bash",
            "tool:edit",
            "tool:bash",
            "tool:absent",
            "touched:/late/work/src/a.rs",
            "touched=/late/work/./src/a.rs",
            "touched=/late/work/../other.rs",
            "touched=/absolute/λ.rs",
            "touched=src/a.rs",
            "touched:src/a.rs AND NOT touched=src/a.rs",
            "touched:/late/work/../other.rs",
            "touched:/absolute/λ.rs",
            "touched:/stale/discovery",
            "has:errors",
            "has:tools",
            "has:images",
            "has:compacted",
            "has:plan",
            "has:approval",
            "NOT has:goal",
            "compactions=1",
            "compactions>1",
            "agents=0",
            "workflows=0",
            "NOT has:subagents",
            "NOT has:workflows",
            "text:hit AND tool:edit",
            "text:miss OR model:late",
            "NOT (text:hit AND tool:absent)",
            "(model:absent OR touched:/late/work/src/a.rs) AND NOT has:goal",
            "has:plan AND model:early AND NOT (tool:absent OR has:goal)",
            "title:discovery AND model:late",
            "title:final OR model:absent",
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = TextSets::resolve(&query, |n| {
                if n == "hit" {
                    HashSet::from([r.id.clone()])
                } else {
                    HashSet::new()
                }
            });
            let expected = matches_full(&query, &r, &full, &texts).unwrap();
            assert_eq!(
                matches_streamed(&query, adapter.as_ref(), &r, &texts).unwrap(),
                expected,
                "{source}"
            );
            let mut refs = vec![r.clone()];
            filter_refs(&mut refs, &query, &texts).unwrap();
            assert_eq!(!refs.is_empty(), expected, "filter: {source}");
        }
    }

    #[test]
    fn exact_target_facts_wait_for_final_cwd_even_after_contains_matches() {
        let query = SessionQuery::parse("touched:a.rs AND touched=/late/a.rs").unwrap();
        let mut sink = FactSink::new(&query);
        sink.note_target("a.rs".into()).unwrap();
        assert!(sink.facts.targets.contains("a.rs"));
        assert!(sink.facts.exact_targets.is_empty());
        assert_eq!(sink.pending_targets.len(), 1);
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late".into());
        assert!(sink.finish(&shell).unwrap().exact_targets.contains("/late/a.rs"));

        let query = SessionQuery::parse("touched=a.rs").unwrap();
        let mut sink = FactSink::new(&query);
        sink.note_target("a.rs".into()).unwrap();
        assert!(sink.finish(&shell).unwrap().exact_targets.is_empty());
    }

    #[test]
    fn forest_string_equality_preserves_whole_values() {
        let world = crate::test_world("query-exact-forest");
        let parent = world.write_session(
            "forest",
            &[serde_json::json!({"type":"user","message":{"role":"user","content":"parent"}})],
        );
        world.write_agent("forest", "one", &serde_json::json!({"agentType":"Explore-Deep"}), &[
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tool","name":"ReadFile","input":{"file_path":"a.rs"}}]}})
        ]);
        let workflows = parent.with_extension("").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(
            workflows.join("wf_one.json"),
            serde_json::json!({"runId":"wf_one","workflowName":"Audit-Deep"}).to_string(),
        )
        .unwrap();
        let r = SessionRef::new("forest", crate::Harness::Claude, parent);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = adapter.parse(&r).unwrap();
        for (source, expected) in [
            ("agent:explore", true),
            ("agent=explore", false),
            ("agent=EXPLORE-DEEP", true),
            ("subtool:read", true),
            ("subtool=read", false),
            ("subtool=READFILE", true),
            ("workflow:audit", true),
            ("workflow=audit", false),
            ("workflow=AUDIT-DEEP", true),
            ("workflow=wf_one", true),
            ("workflow=wf_", false),
        ] {
            let query = SessionQuery::parse(source).unwrap();
            assert_eq!(
                matches_full(&query, &r, &full, &TextSets::empty()).unwrap(),
                expected,
                "full {source}"
            );
            assert_eq!(
                matches_streamed(&query, adapter.as_ref(), &r, &TextSets::empty()).unwrap(),
                expected,
                "streamed {source}"
            );
        }
    }

    #[test]
    fn fact_reducer_retains_distinct_facts_without_payloads_or_event_rows() {
        let query = SessionQuery::parse("model:early AND tool:edit AND touched:a.rs AND has:images").unwrap();
        let mut sink = FactSink::new(&query);
        let mut m = Message::new(Role::Assistant);
        m.model = Some("EarlyModel".into());
        m.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Edit".into(),
            namespace: None,
            input: serde_json::json!({"file_path":"./a.rs","rich":"discarded rich argument".repeat(8192)}).into(),
        });
        m.content.push(Block::Image {
            media_type: None,
            data_ref: Some("discarded image reference".repeat(8192)),
        });
        for i in 0..1000 {
            m.model = Some(format!("EarlyModel-{i}"));
            if let Block::ToolUse { name, .. } = &mut m.content[0] {
                *name = format!("Edit-{i}");
            }
            assert_eq!(crate::MessageSink::message(&mut sink, m.clone()), crate::Flow::Continue);
        }
        assert_eq!(sink.facts.models.len(), 1);
        assert_eq!(sink.facts.tools.len(), 1);
        assert_eq!(sink.facts.targets.len(), 1);
        assert!(sink.facts.images);
        assert_eq!(sink.pending_targets.len(), 0);
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late".into());
        let facts = sink.finish(&shell).unwrap();
        assert_eq!(facts.targets, HashSet::from(["a.rs".into()]));

        assert!(facts.models["early"].contains);
        assert!(!facts.models["early"].exact);
        assert!(shell.messages.is_empty());
    }

    #[test]
    fn touched_reduction_discards_absolute_paths_and_waits_for_final_cwd() {
        let query = SessionQuery::parse("touched:/late/work/a.rs AND touched:/absolute/wanted.rs").unwrap();
        let mut sink = FactSink::new(&query);
        for i in 0..1000 {
            sink.note_target(format!("/absolute/unmatched-{i}.rs")).unwrap();
        }
        assert_eq!(sink.pending_targets.len(), 0);
        assert!(sink.facts.targets.is_empty());
        sink.note_target("a.rs".into()).unwrap();
        sink.note_target("/absolute/wanted.rs".into()).unwrap();
        assert_eq!(sink.pending_targets.len(), 1);
        assert_eq!(sink.facts.targets, HashSet::from(["/absolute/wanted.rs".into()]));
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late/work".into());
        assert_eq!(
            sink.finish(&shell).unwrap().targets,
            HashSet::from(["/late/work/a.rs".into(), "/absolute/wanted.rs".into()])
        );
    }

    #[test]
    fn touched_relative_history_spills_and_resolves_against_late_cwd() {
        let query = SessionQuery::parse("touched:/late/work/missing.rs").unwrap();
        let mut sink = FactSink::new(&query);
        for i in 0..20_000 {
            sink.note_target(format!("relative-{i}.rs")).unwrap();
        }
        sink.note_target("missing.rs".into()).unwrap();
        assert!(sink.pending_targets.spilled_bytes().unwrap() > 0);
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late/work".into());
        let facts = sink.finish(&shell).unwrap();
        assert_eq!(facts.targets, HashSet::from(["/late/work/missing.rs".into()]));
    }

    #[test]
    fn streamed_compaction_facts_preserve_full_detector_validation() {
        let world = crate::test_world("streamed_compaction_validation");
        let path = world.transcript("invalid-sidecar");
        // A record that is not a serde Value (here an overflowing number in a sidecar nobody
        // reads) is rejected by every demand, so the lazy IR and the detector agree.
        std::fs::write(
            &path,
            r#"{"type":"system","subtype":"compact_boundary","toolUseResult":{"ignored":1e400}}"#,
        )
        .unwrap();
        let r = SessionRef::new("invalid-sidecar", crate::Harness::Claude, path);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = crate::stream::collect_with(adapter.as_ref(), &r, &crate::Demand::LEAN.with_spans()).unwrap();
        let query = SessionQuery::parse("compactions=0").unwrap();
        let texts = TextSets::default();
        assert_eq!(full.messages.len(), 0);
        assert!(matches_full(&query, &r, &full, &texts).unwrap());
        assert!(matches_streamed(&query, adapter.as_ref(), &r, &texts).unwrap());
    }
    #[test]
    fn unreadable_json_facts_and_filter_errors_are_not_negative_matches() {
        let world = crate::test_world("query_json_failures");
        let path = world.home.join("input.json");
        let body = r#"{"file_path":"a.rs","command":"git commit","plan":["step"]}"#;
        std::fs::write(&path, body).unwrap();
        let input = crate::json::Json::source(&path, 0, body.len() as u64).unwrap();
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Edit".into(),
            namespace: None,
            input,
        });
        std::fs::remove_file(&path).unwrap();
        let r = SessionRef::new("facts", crate::Harness::Claude, world.transcript("missing"));
        let mut full = Session::new("facts", crate::Harness::Claude);
        full.messages.push(message.clone());
        for source in ["tool:edit", "NOT tool:edit", "touched:a.rs", "has:plan"] {
            assert!(
                matches_full(&SessionQuery::parse(source).unwrap(), &r, &full, &TextSets::empty()).is_err(),
                "{source}"
            );
        }
        let mut sink = FactSink::new(&SessionQuery::parse("tool:edit").unwrap());
        assert_eq!(crate::MessageSink::message(&mut sink, message), crate::Flow::Stop);
        assert!(crate::MessageSink::take_error(&mut sink).is_some());
        let mut refs = vec![r.clone()];
        assert!(filter_refs(
            &mut refs,
            &SessionQuery::parse("tool:edit").unwrap(),
            &TextSets::empty()
        )
        .is_err());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, r.id);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn counted_session_fields_evaluate_without_parse_and_revalidate_subagent_forest() {
        let world = crate::test_world("counted_query_fields");
        let path = world.write_session(
            "parent",
            &[
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "u1",
                    "gitBranch": "feat/spine",
                    "message": {
                        "role": "assistant",
                        "model": "claude-opus-4-6",
                        "content": [{"type": "text", "text": "step 1"}]
                    }
                }),
                serde_json::json!({
                    "type": "system",
                    "uuid": "b1",
                    "subtype": "compact_boundary",
                    "compactMetadata": {"trigger": "auto", "preTokens": 4000}
                }),
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "u2",
                    "message": {
                        "role": "assistant",
                        "model": "claude-haiku-4-5",
                        "content": [{"type": "text", "text": "step 2"}]
                    }
                }),
            ],
        );
        world.write_agent(
            "parent",
            "sub-1",
            &serde_json::json!({"agentType": "Explore"}),
            &[serde_json::json!({"type": "user", "message": {"role": "user", "content": "hi"}})],
        );

        let r = SessionRef::new("parent", crate::Harness::Claude, path);
        crate::catalog::sync(std::slice::from_ref(&r));

        // First query counts transcript facts without walking the sub-agent forest.
        let mut initial = vec![r.clone()];
        filter_refs(
            &mut initial,
            &SessionQuery::parse("model:haiku").unwrap(),
            &TextSets::empty(),
        )
        .unwrap();
        assert_eq!(initial.len(), 1);

        let indexed = crate::catalog::session_indexed_fields(&r).expect("sessions row indexed");
        assert_eq!(indexed.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(indexed.git_branch.as_deref(), Some("feat/spine"));
        assert_eq!(indexed.compactions, Some(1));
        assert_eq!(
            indexed.subagent_count, None,
            "non-subagent pass must not walk the sub-agent forest"
        );

        // First sub-agent query populates and guards `subagent_count`.
        let mut sub_initial = vec![r.clone()];
        filter_refs(
            &mut sub_initial,
            &SessionQuery::parse("agents=1").unwrap(),
            &TextSets::empty(),
        )
        .unwrap();
        assert_eq!(sub_initial.len(), 1);
        assert_eq!(
            crate::catalog::session_indexed_fields(&r).unwrap().subagent_count,
            Some(1)
        );

        // Every counted field now settles definitely in `RefQuery::cheap` (zero transcript parse or forest walk).
        let texts = TextSets::empty();
        for (expr, want) in [
            ("model:opus", Tri::True),
            ("model:haiku", Tri::True),
            ("model=claude-haiku-4-5", Tri::True),
            ("model=haiku", Tri::False),
            ("model:sonnet", Tri::False),
            ("git:spine", Tri::True),
            ("git=feat/spine", Tri::True),
            ("git=spine", Tri::False),
            ("compactions=1", Tri::True),
            ("compactions>=2", Tri::False),
            ("has:compacted", Tri::True),
            ("agents=1", Tri::True),
            ("agents>=2", Tri::False),
            ("has:subagents", Tri::True),
        ] {
            let query = SessionQuery::parse(expr).unwrap();
            assert!(
                query.needs_counted() && !query.needs_parse() && !query.needs_forest(),
                "{expr}"
            );
            for demand_counts in [false, true] {
                let mut plan = RefQuery::with_counts_mode(&query, &texts, demand_counts);
                assert_eq!(plan.cheap(&r).unwrap(), want, "{expr} (demand_counts={demand_counts})");
            }
        }

        // Spawning a second sub-agent without touching the parent transcript invalidates `subagent_guard`:
        // `cheap` returns `Unknown` for `agents` / `has:subagents` (never stale `1`), and `filter_refs`
        // refreshes the count to `2`!
        world.write_agent(
            "parent",
            "sub-2",
            &serde_json::json!({"agentType": "Explore"}),
            &[serde_json::json!({"type": "user", "message": {"role": "user", "content": "hi 2"}})],
        );
        let q_agents2 = SessionQuery::parse("agents=2").unwrap();
        let mut stale_plan = RefQuery::with_counts_mode(&q_agents2, &texts, true);
        assert_eq!(
            stale_plan.cheap(&r).unwrap(),
            Tri::Unknown,
            "new sub-agent must invalidate subagent_guard without parent transcript edit"
        );
        let mut refreshed = vec![r.clone()];
        filter_refs(&mut refreshed, &q_agents2, &texts).unwrap();
        assert_eq!(refreshed.len(), 1);
        assert_eq!(
            crate::catalog::session_indexed_fields(&r).unwrap().subagent_count,
            Some(2)
        );
    }
}
