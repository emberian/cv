//! A **query calculus** for selecting sessions across the corpus — one boolean DSL that every
//! corpus-spanning command (`ls`, `dataset`, `stats`, …) accepts via `-q`, instead of
//! each growing its own ad-hoc flags.
//!
//! The whole language is driven by one table — [`FIELDS`] — which is the single source of truth for
//! parsing, evaluation routing, the human reference (`cv schema`), and the machine-readable schema
//! (`cv schema --json`). Add a row there and the field is parseable, evaluable, *and* documented; the
//! three can't drift.
//!
//! # Grammar
//!
//! ```text
//! query   := or
//! or      := and ( ("OR" | "|") and )*
//! and     := unary ( ("AND")? unary )*        # juxtaposition = AND
//! unary   := ("NOT" | "-") unary | primary
//! primary := "(" or ")" | term
//! term    := field op value | WORD            # a bare WORD matches title/cwd/id
//! op      := ":" | "=" | ">" | ">=" | "<" | "<="
//! value   := WORD | "quoted" | a,b,c (OR-list) | lo..hi (range, for num/date)
//! ```
//!
//! Evaluation is **three-valued** ([`Tri`]) so the catalog-cheap prefilter can prune without parsing:
//! a leaf whose data isn't available yet (model needs a parse; `text:`/`tool:` need the index/events
//! catalog) evaluates to [`Tri::Unknown`], and the prefilter keeps a session unless the expression is
//! *definitely* false. A second pass with full [`Facts`] then decides for real.

use crate::ir::{Harness, Session, SessionRef};
use chrono::{DateTime, NaiveDate, Utc};

// ---------------------------------------------------------------------------
// Field registry — the single source of truth.
// ---------------------------------------------------------------------------

/// Every queryable field. Adding a variant + a [`FIELDS`] row makes it parse, evaluate, and document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    Harness,
    Model,
    Cwd,
    Title,
    Id,
    Msgs,
    Created,
    Updated,
    Git,
    Text,
    Tool,
    Touched,
    Has,
    // Forest-tier: reach into the sub-agent forest / workflow runs / compaction seams.
    Subtool,
    Agent,
    Agents,
    Workflow,
    Workflows,
    Compactions,
}

/// What kind of value a field takes — drives value parsing and which operators are legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Case-insensitive substring (`:`) or exact (`=`) string.
    Str,
    /// One of the [`Harness`] names/aliases.
    HarnessEnum,
    /// An unsigned count; supports the comparison operators and ranges.
    Num,
    /// A `YYYY-MM-DD` date or RFC 3339 instant; supports comparisons and ranges.
    Date,
    /// A path substring.
    Path,
    /// An enum of capability flags (see [`HAS_FLAGS`]) — `has:subagents`, `has:errors`, …
    Flag,
}

/// Where a field's answer comes from — determines the evaluation phase. `Catalog` is free (on the
/// [`SessionRef`]); `Parse` needs the transcript; `Index`/`Events` need an external resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    Catalog,
    Parse,
    Index,
    Events,
    /// Walks the sub-agent forest / workflow runs / compaction seams — the priciest tier (reads many
    /// sidecar files), so always pair a forest term with a cheap one.
    Forest,
    /// Answered from the catalog's per-session affordance counts (`has:plan`, `has:schedule`, …),
    /// which any full read fills (`cv index` brings the fleet to counted). A session whose counts
    /// are missing or stale is parsed once, and counted from then on.
    Counted,
}

/// One row of the language: a field, how to spell it, what it takes, and what it costs to answer.
pub struct FieldDef {
    pub id: FieldId,
    pub name: &'static str,
    /// Other spellings. One carrying an implied operator (sugar) pairs with it: `before` means
    /// `updated<`.
    pub aliases: &'static [(&'static str, Option<Op>)],
    pub kind: Kind,
    pub cost: Cost,
    pub desc: &'static str,
    pub example: &'static str,
}

/// The capability flags accepted by `has:` (and surfaced in the reference), each with the tier its
/// answer comes from. The `Cost::Counted` rows are the affordance vocabulary
/// (`crate::affordance::Kind`, `docs/design/AFFORDANCES.md`) and must stay in step with it.
pub const HAS_FLAGS: &[(&str, &str, Cost)] = &[
    (
        "subagents",
        "spawned at least one sub-agent (its forest, as `agents` counts)",
        Cost::Counted,
    ),
    ("errors", "has at least one error event", Cost::Events),
    ("tools", "used at least one tool", Cost::Events),
    ("images", "contains an image block", Cost::Events),
    ("compacted", "context was compacted at least once", Cost::Counted),
    ("workflows", "ran at least one Workflow-tool run", Cost::Events),
    (
        "plan",
        "wrote or holds a todo list / plan, or switched plan mode",
        Cost::Counted,
    ),
    ("goal", "set or holds a standing goal", Cost::Counted),
    (
        "schedule",
        "created, holds or was fired by a cron job, wakeup or loop",
        Cost::Counted,
    ),
    (
        "background_job",
        "started, polled or was notified of a background job",
        Cost::Counted,
    ),
    ("subagent_spawn", "spawned a sub-agent (any harness)", Cost::Counted),
    ("subagent_return", "received a sub-agent's return", Cost::Counted),
    ("skill", "loaded or managed a skill", Cost::Counted),
    ("hook", "a user hook's output reached the model", Cost::Counted),
    ("question", "asked the person a question", Cost::Counted),
    (
        "approval",
        "asked for, was denied or recorded a permission / approval mode",
        Cost::Counted,
    ),
    (
        "checkpoint",
        "left or restored a restorable point (file snapshot, shadow commit, saved chat)",
        Cost::Counted,
    ),
    ("branch", "rewound, undid, reset or cleared", Cost::Counted),
    ("fork", "was forked from another session", Cost::Counted),
    ("model_change", "switched model mid-session", Cost::Counted),
];

/// The language. One row per field; everything else is generated from this.
#[rustfmt::skip]
pub const FIELDS: &[FieldDef] = &[
    FieldDef { id: FieldId::Harness, name: "harness", aliases: &[("h", None)], kind: Kind::HarnessEnum, cost: Cost::Catalog,
        desc: "Which harness produced the session.", example: "harness:claude" },
    FieldDef { id: FieldId::Model, name: "model", aliases: &[("m", None)], kind: Kind::Str, cost: Cost::Counted,
        desc: "Any turn's model id (substring; counted in catalog once parsed).", example: "model:fable" },
    FieldDef { id: FieldId::Cwd, name: "cwd", aliases: &[("dir", None)], kind: Kind::Path, cost: Cost::Catalog,
        desc: "Working directory (substring).", example: "cwd:/pug/clustervision" },
    FieldDef { id: FieldId::Title, name: "title", aliases: &[], kind: Kind::Str, cost: Cost::Catalog,
        desc: "Session title (substring).", example: "title:\"fix the bug\"" },
    FieldDef { id: FieldId::Id, name: "id", aliases: &[], kind: Kind::Str, cost: Cost::Catalog,
        desc: "Session id (substring/prefix).", example: "id:83e3" },
    FieldDef { id: FieldId::Msgs, name: "msgs", aliases: &[("messages", None)], kind: Kind::Num, cost: Cost::Catalog,
        desc: "User+assistant message count.", example: "msgs>=50" },
    FieldDef { id: FieldId::Created, name: "created", aliases: &[], kind: Kind::Date, cost: Cost::Catalog,
        desc: "Creation time (date or RFC3339).", example: "created>2026-01-01" },
    FieldDef { id: FieldId::Updated, name: "updated", aliases: &[("before", Some(Op::Lt)), ("after", Some(Op::Ge)), ("until", Some(Op::Lt)), ("since", Some(Op::Ge))], kind: Kind::Date, cost: Cost::Catalog,
        desc: "Last-updated time. (`before:`/`until:` are sugar for `updated<`; `after:`/`since:` for `updated>=`.)", example: "updated:2026-01-01..2026-04-01" },
    FieldDef { id: FieldId::Git, name: "git", aliases: &[("branch", None)], kind: Kind::Str, cost: Cost::Counted,
        desc: "Git branch or repo (substring; counted in catalog once parsed).", example: "git:main" },
    FieldDef { id: FieldId::Text, name: "text", aliases: &[("content", None)], kind: Kind::Str, cost: Cost::Index,
        desc: "Full-text match over session content (tantivy). Needs `cv index`.", example: "text:\"SandboxDenied\"" },
    FieldDef { id: FieldId::Tool, name: "tool", aliases: &[], kind: Kind::Str, cost: Cost::Events,
        desc: "Used a tool by this name (events catalog).", example: "tool:Bash" },
    FieldDef { id: FieldId::Touched, name: "touched", aliases: &[("file", None)], kind: Kind::Path, cost: Cost::Events,
        desc: "Read or edited a file at this path (events catalog).", example: "touched:src/ir.rs" },
    FieldDef { id: FieldId::Has, name: "has", aliases: &[], kind: Kind::Flag, cost: Cost::Events,
        desc: "A capability flag (errors, tools, images, workflows; events tier) or a counted fact/affordance (subagents, compacted, plan, schedule, background_job, …; counted tier: catalog-fast once `cv index` or any full read has counted the session, a parse otherwise).", example: "has:plan" },
    FieldDef { id: FieldId::Subtool, name: "subtool", aliases: &[], kind: Kind::Str, cost: Cost::Forest,
        desc: "A tool used by a *sub-agent* (not the orchestrator). Walks the forest.", example: "subtool:Bash" },
    FieldDef { id: FieldId::Agent, name: "agent", aliases: &[("subagent", None)], kind: Kind::Str, cost: Cost::Forest,
        desc: "Spawned a sub-agent of this type (substring): Explore, general-purpose, …", example: "agent:Explore" },
    FieldDef { id: FieldId::Agents, name: "agents", aliases: &[("subagents", None)], kind: Kind::Num, cost: Cost::Counted,
        desc: "Sub-agent forest size (counted in catalog once parsed).", example: "agents>=5" },
    FieldDef { id: FieldId::Workflow, name: "workflow", aliases: &[("wf", None)], kind: Kind::Str, cost: Cost::Forest,
        desc: "Ran a `Workflow` matching this name/run-id (substring).", example: "workflow:census" },
    FieldDef { id: FieldId::Workflows, name: "workflows", aliases: &[], kind: Kind::Num, cost: Cost::Forest,
        desc: "Number of `Workflow`-tool runs.", example: "workflows>=1" },
    FieldDef { id: FieldId::Compactions, name: "compactions", aliases: &[("compacts", None)], kind: Kind::Num, cost: Cost::Counted,
        desc: "Number of context-compaction boundaries (counted in catalog once parsed).", example: "compactions>=2" },
];

fn field_for(key: &str) -> Option<&'static FieldDef> {
    let k = key.to_ascii_lowercase();
    FIELDS
        .iter()
        .find(|f| f.name == k || f.aliases.iter().any(|(a, _)| *a == k))
}

/// The one-word cost label shown in the reference + machine schema.
fn cost_label(c: Cost) -> &'static str {
    match c {
        Cost::Catalog => "catalog",
        Cost::Parse => "parse",
        Cost::Index => "index",
        Cost::Events => "events",
        Cost::Forest => "forest",
        Cost::Counted => "counted",
    }
}

// ---------------------------------------------------------------------------
// Operators & values.
// ---------------------------------------------------------------------------

/// A comparison operator. `Contains` is the default `:`; `Eq` is `=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Contains,
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Contains => ":",
            Op::Eq => "=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
        }
    }
    fn cmp_ok<T: PartialOrd>(self, lhs: &T, rhs: &T) -> bool {
        match self {
            Op::Gt => lhs > rhs,
            Op::Ge => lhs >= rhs,
            Op::Lt => lhs < rhs,
            Op::Le => lhs <= rhs,
            Op::Eq | Op::Contains => lhs == rhs,
        }
    }
}

/// A parsed right-hand value, already shaped to the field's [`Kind`].
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    /// Lowercased strings; >1 means an OR-list (`a,b,c`).
    Strs(Vec<String>),
    Num(u64),
    NumRange(u64, u64),
    Date(DateTime<Utc>),
    DateRange(DateTime<Utc>, DateTime<Utc>),
}

// ---------------------------------------------------------------------------
// AST.
// ---------------------------------------------------------------------------

/// One predicate: `field op value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub field: FieldId,
    pub op: Op,
    pub val: Val,
}

/// A boolean expression over [`Term`]s.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    All, // empty query — matches everything
    Term(Term),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

// ---------------------------------------------------------------------------
// Three-valued logic.
// ---------------------------------------------------------------------------

/// Kleene three-valued result: `Unknown` means "can't tell yet at this phase".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
    /// Lift a definite boolean into a [`Tri`].
    pub fn of(b: bool) -> Tri {
        if b {
            Tri::True
        } else {
            Tri::False
        }
    }
}

// ---------------------------------------------------------------------------
// Facts the evaluator consults.
// ---------------------------------------------------------------------------

/// Externally-resolved predicates (full-text & events) — implemented by the CLI, which has the
/// tantivy index and events catalog. cv-core stays free of those deps. The default impl
/// ([`NoExt`]) answers `Unknown`, so a ref-only prefilter never wrongly excludes on these.
pub trait ExtFacts {
    /// A model predicate resolved without retaining messages. `Unknown` falls back to the
    /// provided Session's model and turn models, preserving existing resolver implementations.
    fn model(&self, _needle: &str, _exact: bool) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// A git branch/remote predicate resolved from catalog facts. `Unknown` falls back to `Session::git`.
    fn git(&self, _needle: &str, _exact: bool) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    fn text(&self, _needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// Exact string predicates for event/forest fields. Existing substring resolvers remain
    /// unchanged; implementations that cannot decide exactness must retain Unknown.
    fn exact(&self, _field: FieldId, _needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    fn tool(&self, _name: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    fn touched(&self, _path: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    fn has(&self, _flag: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// A tool used by a sub-agent (not the orchestrator) — `subtool:`.
    fn subtool(&self, _name: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// Spawned a sub-agent whose type matches — `agent:`.
    fn agent_type(&self, _needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// Ran a `Workflow` matching this name/run-id — `workflow:`.
    fn workflow(&self, _needle: &str) -> anyhow::Result<Tri> {
        Ok(Tri::Unknown)
    }
    /// A forest-derived count for a numeric forest field (`agents`/`workflows`/`compactions`).
    /// `None` ⇒ couldn't compute (treated as `Unknown`).
    fn count(&self, _field: FieldId) -> anyhow::Result<Option<u64>> {
        Ok(None)
    }
}

/// The do-nothing resolver (everything `Unknown`) — used in the cheap prefilter phase.
pub struct NoExt;
impl ExtFacts for NoExt {}

/// Everything the evaluator can look at for one session: the catalog row, the parsed session (when
/// available), and the external resolver.
pub struct Facts<'a> {
    pub r: &'a SessionRef,
    pub session: Option<&'a Session>,
    pub ext: &'a dyn ExtFacts,
}

impl<'a> Facts<'a> {
    /// A cheap, ref-only fact set: no parse, external predicates `Unknown`.
    pub fn from_ref(r: &'a SessionRef) -> Facts<'a> {
        Facts {
            r,
            session: None,
            ext: &NoExt,
        }
    }
}

// ---------------------------------------------------------------------------
// The query.
// ---------------------------------------------------------------------------

/// A parsed query: a boolean [`Expr`] plus the set of [`Cost`]s its leaves touch (so a caller knows
/// whether it must parse / hit the index / read events).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionQuery {
    pub expr: Expr,
    costs: Vec<Cost>,
}

impl SessionQuery {
    /// Parse a query string. Errors are human-readable and point back at `cv schema`.
    pub fn parse(input: &str) -> Result<SessionQuery, String> {
        let toks = lex(input)?;
        let mut p = Parser { toks, i: 0 };
        let expr = if p.toks.is_empty() {
            Expr::All
        } else {
            let e = p.parse_or(0)?;
            if p.i < p.toks.len() {
                return Err(format!(
                    "unexpected {:?} (unbalanced parens?) — see `cv schema`",
                    p.toks[p.i]
                ));
            }
            e
        };
        let mut costs = Vec::new();
        collect_costs(&expr, &mut costs);
        Ok(SessionQuery { expr, costs })
    }

    /// Whether the query matches everything (empty).
    pub fn is_empty(&self) -> bool {
        matches!(self.expr, Expr::All)
    }

    /// Whether honoring this query needs the parsed transcript (a `model:`/`git:` leaf).
    pub fn needs_parse(&self) -> bool {
        self.costs.contains(&Cost::Parse)
    }

    /// Whether it touches the full-text index (`text:`).
    pub fn needs_index(&self) -> bool {
        self.costs.contains(&Cost::Index)
    }

    /// Whether it reads the events catalog (`tool:`/`touched:`/`has:`).
    pub fn needs_events(&self) -> bool {
        self.costs.contains(&Cost::Events)
    }

    /// Whether it has an affordance `has:` term, answered from the catalog's counts where a full
    /// read recorded them (see [`Cost::Counted`]).
    pub fn needs_counted(&self) -> bool {
        self.costs.contains(&Cost::Counted)
    }

    /// The three-valued evaluation against whatever facts are given (`Unknown` where they cannot
    /// decide) — what a sweep uses to keep a definite `True` without parsing.
    pub fn eval(&self, facts: &Facts) -> anyhow::Result<Tri> {
        eval(&self.expr, facts)
    }

    /// Whether it walks the sub-agent forest / workflows / compaction (`subtool:`/`agent:`/`agents`/
    /// `workflow(s)`/`compactions`) — the priciest tier.
    pub fn needs_forest(&self) -> bool {
        self.costs.contains(&Cost::Forest)
    }

    /// Every value-needle used with a given field, anywhere in the expression — so a caller can
    /// pre-resolve external predicates (e.g. run one full-text search per distinct `text:` needle).
    pub fn needles(&self, field: FieldId) -> Vec<String> {
        let mut out = Vec::new();
        collect_needles(&self.expr, field, None, &mut out);
        out
    }

    pub(crate) fn exact_needles(&self, field: FieldId) -> Vec<String> {
        let mut out = Vec::new();
        collect_needles(&self.expr, field, Some(Op::Eq), &mut out);
        out
    }

    pub(crate) fn mentions_field(&self, field: FieldId) -> bool {
        fn walk(e: &Expr, field: FieldId) -> bool {
            match e {
                Expr::All => false,
                Expr::Term(t) => t.field == field,
                Expr::Not(b) => walk(b, field),
                Expr::And(v) | Expr::Or(v) => v.iter().any(|x| walk(x, field)),
            }
        }
        walk(&self.expr, field)
    }

    /// Cheap catalog-only prefilter: evaluate against the ref alone (parse/index/events leaves are
    /// `Unknown`). Returns `false` only when the query is *definitely* false — safe to prune.
    pub fn prefilter(&self, r: &SessionRef) -> bool {
        eval(&self.expr, &Facts::from_ref(r)).expect("catalog facts are infallible") != Tri::False
    }

    /// Full evaluation with whatever facts are available. `Unknown` is treated as no-match (the
    /// final decision must be definite).
    pub fn matches(&self, facts: &Facts) -> anyhow::Result<bool> {
        Ok(eval(&self.expr, facts)? == Tri::True)
    }

    /// Convenience: full match against a parsed [`Session`] (external predicates `Unknown`). Use
    /// [`matches`](Self::matches) with a real [`ExtFacts`] when the query has `text:`/`tool:` leaves.
    pub fn matches_session(&self, s: &Session) -> bool {
        let facts = Facts {
            r: &session_as_ref(s),
            session: Some(s),
            ext: &NoExt,
        };
        eval(&self.expr, &facts).expect("session metadata facts are infallible") == Tri::True
    }
}

fn collect_needles(e: &Expr, field: FieldId, op: Option<Op>, out: &mut Vec<String>) {
    match e {
        Expr::All => {}
        Expr::Term(t) if t.field == field && op.is_none_or(|op| op == t.op) => {
            if let Val::Strs(v) = &t.val {
                for s in v {
                    if !out.contains(s) {
                        out.push(s.clone());
                    }
                }
            }
        }
        Expr::Term(_) => {}
        Expr::Not(b) => collect_needles(b, field, op, out),
        Expr::And(v) | Expr::Or(v) => v.iter().for_each(|e| collect_needles(e, field, op, out)),
    }
}

fn collect_costs(e: &Expr, out: &mut Vec<Cost>) {
    match e {
        Expr::All => {}
        Expr::Term(t) => {
            // `has:` is two tiers (the tier column on [`HAS_FLAGS`]): an affordance (`has:plan`)
            // is counted in the catalog, the capability flags (`has:errors`) come from events —
            // a term needs every one of its flags' tiers.
            let tiers: Vec<Cost> = match (&t.field, &t.val) {
                (FieldId::Has, Val::Strs(v)) => v
                    .iter()
                    .map(|flag| {
                        HAS_FLAGS
                            .iter()
                            .find(|(n, _, _)| n == flag)
                            .map(|(_, _, tier)| *tier)
                            .unwrap_or(Cost::Events)
                    })
                    .collect(),
                _ => vec![FIELDS
                    .iter()
                    .find(|f| f.id == t.field)
                    .map(|f| f.cost)
                    .unwrap_or(Cost::Catalog)],
            };
            for tier in tiers {
                if !out.contains(&tier) {
                    out.push(tier);
                }
            }
        }
        Expr::Not(b) => collect_costs(b, out),
        Expr::And(v) | Expr::Or(v) => v.iter().for_each(|e| collect_costs(e, out)),
    }
}

// ---------------------------------------------------------------------------
// Evaluation.
// ---------------------------------------------------------------------------

fn eval(e: &Expr, f: &Facts) -> anyhow::Result<Tri> {
    Ok(match e {
        Expr::All => Tri::True,
        Expr::Term(t) => eval_term(t, f)?,
        Expr::Not(b) => eval(b, f)?.not(),
        Expr::And(v) => {
            let mut acc = Tri::True;
            for e in v {
                match eval(e, f)? {
                    Tri::False => return Ok(Tri::False),
                    Tri::Unknown => acc = Tri::Unknown,
                    Tri::True => {}
                }
            }
            acc
        }
        Expr::Or(v) => {
            let mut acc = Tri::False;
            for e in v {
                match eval(e, f)? {
                    Tri::True => return Ok(Tri::True),
                    Tri::Unknown => acc = Tri::Unknown,
                    Tri::False => {}
                }
            }
            acc
        }
    })
}

fn eval_term(t: &Term, f: &Facts) -> anyhow::Result<Tri> {
    Ok(match t.field {
        FieldId::Harness => match &t.val {
            Val::Strs(names) => Tri::of(names.iter().any(|n| Harness::parse(n) == Some(f.r.harness))),
            _ => Tri::False,
        },
        FieldId::Cwd => str_match(
            t.op,
            &t.val,
            f.r.cwd.as_ref().map(|p| p.to_string_lossy().to_lowercase()),
        ),
        FieldId::Title => str_match(t.op, &t.val, f.r.title.as_deref().map(str::to_lowercase)),
        FieldId::Id => str_match(t.op, &t.val, Some(f.r.id.to_lowercase())),
        // An uncounted listing row cannot answer `msgs:`; the parse phase counts and decides.
        FieldId::Msgs => match f.r.message_count {
            Some(n) => num_match(t.op, &t.val, n as u64),
            None => Tri::Unknown,
        },
        // Creation is never answered from the update time: a ref without it leaves the parse to
        // decide, and a parsed session without it does not match.
        FieldId::Created => match (f.r.created_at, f.session) {
            (Some(at), _) => date_match(t.op, &t.val, Some(at)),
            (None, Some(s)) => date_match(t.op, &t.val, s.created_at),
            (None, None) => Tri::Unknown,
        },
        FieldId::Updated => date_match(t.op, &t.val, f.r.updated_at.or(f.r.created_at)),
        // Parse-required.
        FieldId::Model => or_over(&t.val, |needle| {
            let resolved = f.ext.model(needle, t.op == Op::Eq)?;
            if resolved != Tri::Unknown {
                return Ok(resolved);
            }
            Ok(match f.session {
                None => Tri::Unknown,
                Some(s) => {
                    let hit = |m: Option<&str>| {
                        m.is_some_and(|m| {
                            let model = m.to_lowercase();
                            if t.op == Op::Eq {
                                model == needle
                            } else {
                                model.contains(needle)
                            }
                        })
                    };
                    Tri::of(hit(s.model.as_deref()) || s.messages.iter().any(|msg| hit(msg.model.as_deref())))
                }
            })
        })?,
        FieldId::Git => or_over(&t.val, |needle| {
            let resolved = f.ext.git(needle, t.op == Op::Eq)?;
            if resolved != Tri::Unknown {
                return Ok(resolved);
            }
            Ok(match f.session {
                None => Tri::Unknown,
                Some(s) if t.op == Op::Eq => Tri::of(s.git.as_ref().is_some_and(|g| {
                    [g.branch.as_deref(), g.remote.as_deref()]
                        .into_iter()
                        .flatten()
                        .any(|value| value.to_lowercase() == needle)
                })),
                Some(s) => Tri::of(s.git.as_ref().is_some_and(|g| {
                    format!(
                        "{} {}",
                        g.branch.as_deref().unwrap_or_default(),
                        g.remote.as_deref().unwrap_or_default()
                    )
                    .to_lowercase()
                    .contains(needle)
                })),
            })
        })?,
        // External (index/events).
        FieldId::Text => or_over(&t.val, |n| f.ext.text(n))?,
        FieldId::Tool => or_over(&t.val, |n| {
            if t.op == Op::Eq {
                f.ext.exact(t.field, n)
            } else {
                f.ext.tool(n)
            }
        })?,
        FieldId::Touched => or_over(&t.val, |n| {
            if t.op == Op::Eq {
                f.ext.exact(t.field, n)
            } else {
                f.ext.touched(n)
            }
        })?,
        FieldId::Has => or_over(&t.val, |n| f.ext.has(n))?,
        // Forest (sub-agents / workflows / compaction).
        FieldId::Subtool => or_over(&t.val, |n| {
            if t.op == Op::Eq {
                f.ext.exact(t.field, n)
            } else {
                f.ext.subtool(n)
            }
        })?,
        FieldId::Agent => or_over(&t.val, |n| {
            if t.op == Op::Eq {
                f.ext.exact(t.field, n)
            } else {
                f.ext.agent_type(n)
            }
        })?,
        FieldId::Workflow => or_over(&t.val, |n| {
            if t.op == Op::Eq {
                f.ext.exact(t.field, n)
            } else {
                f.ext.workflow(n)
            }
        })?,
        FieldId::Agents | FieldId::Workflows | FieldId::Compactions => match f.ext.count(t.field)? {
            Some(n) => num_match(t.op, &t.val, n),
            None => Tri::Unknown,
        },
    })
}

/// OR a predicate over each string in the value (so `tool:a,b` is "a OR b"), with Kleene semantics.
fn or_over(val: &Val, mut g: impl FnMut(&str) -> anyhow::Result<Tri>) -> anyhow::Result<Tri> {
    let mut acc = Tri::False;
    for n in as_strs(val) {
        match g(n)? {
            Tri::True => return Ok(Tri::True),
            Tri::Unknown => acc = Tri::Unknown,
            Tri::False => {}
        }
    }
    Ok(acc)
}

fn as_strs(val: &Val) -> &[String] {
    match val {
        Val::Strs(v) => v,
        _ => &[],
    }
}

/// Does `val` (under `op`) match a single lowercased haystack? Handles substring (`:`) and exact
/// (`=`); OR-lists match if any element does.
fn val_hit(val: &Val, op: Op, h: &str) -> bool {
    match val {
        Val::Strs(strs) => match op {
            Op::Eq => strs.iter().any(|n| h == n),
            _ => strs.iter().any(|n| h.contains(n)),
        },
        _ => false,
    }
}

/// `:` contains, `=` exact — against an optional lowercased haystack.
fn str_match(op: Op, val: &Val, hay: Option<String>) -> Tri {
    match hay {
        None => Tri::False,
        Some(h) => Tri::of(val_hit(val, op, &h)),
    }
}

fn num_match(op: Op, val: &Val, n: u64) -> Tri {
    match val {
        Val::Num(rhs) => Tri::of(op.cmp_ok(&n, rhs)),
        Val::NumRange(lo, hi) => Tri::of(n >= *lo && n <= *hi),
        _ => Tri::False,
    }
}

fn date_match(op: Op, val: &Val, when: Option<DateTime<Utc>>) -> Tri {
    let Some(w) = when else { return Tri::False };
    match val {
        Val::Date(rhs) => Tri::of(op.cmp_ok(&w, rhs)),
        Val::DateRange(lo, hi) => Tri::of(w >= *lo && w <= *hi),
        _ => Tri::False,
    }
}

/// A throwaway ref view of a parsed session so `matches_session` can reuse the catalog-field logic.
fn session_as_ref(s: &Session) -> SessionRef {
    SessionRef {
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at, // `SessionRef.message_count` is contractually user+assistant turns only (see ir.rs), so
        // `msgs:` must count the same way here as the catalog prefilter does.
        message_count: Some(
            s.messages
                .iter()
                .filter(|m| matches!(m.role, crate::ir::Role::User | crate::ir::Role::Assistant))
                .count(),
        ),
        ..SessionRef::new(s.id.clone(), s.harness, s.source_path.clone().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Lexer.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(Word), // a bare/quoted chunk (may contain a field:op:value)
    And,
    Or,
    Not,
    LParen,
    RParen,
}

/// A lexed word plus where its quoting started, so the parser can tell `title:"a, b"` (a quoted
/// value: commas are literal, not an OR-list) and `"http://x"` (a fully-quoted needle: never a
/// field term) apart from bare text. Quoting info would otherwise be lost before value parsing.
#[derive(Debug, Clone, PartialEq)]
struct Word {
    text: String,
    /// Byte offset in `text` where the first quoted character landed, if any part was quoted.
    quoted_from: Option<usize>,
}

/// Tokenize: split on whitespace honoring quotes; recognize `(`/`)` and the keywords AND/OR/NOT and
/// the `|` / `-` sigils. A leading `-` on a word becomes NOT.
fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            out.push(Tok::LParen);
            i += 1;
            continue;
        }
        if c == ')' {
            out.push(Tok::RParen);
            i += 1;
            continue;
        }
        if c == '|' {
            out.push(Tok::Or);
            i += 1;
            continue;
        }
        // A leading '-' (not inside a word) means NOT, unless it's a digit-y range like nothing here.
        if c == '-' && (i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == '(') {
            out.push(Tok::Not);
            i += 1;
            continue;
        }
        // Read a word, honoring quotes and stopping at whitespace/parens. Remember where quoting
        // began so the parser can treat quoted content literally (see [`Word`]).
        let mut w = String::new();
        let mut quoted_from: Option<usize> = None;
        let mut in_quote = false;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                if !in_quote && quoted_from.is_none() {
                    quoted_from = Some(w.len());
                }
                in_quote = !in_quote;
                i += 1;
                continue;
            }
            if !in_quote && (c.is_whitespace() || c == '(' || c == ')' || c == '|') {
                break;
            }
            w.push(c);
            i += 1;
        }
        if in_quote {
            return Err("unterminated quote — see `cv schema`".to_string());
        }
        // Keywords only when unquoted: `"and"` is a needle, AND is an operator.
        match (w.as_str(), quoted_from) {
            ("AND" | "and", None) => out.push(Tok::And),
            ("OR" | "or", None) => out.push(Tok::Or),
            ("NOT" | "not", None) => out.push(Tok::Not),
            _ => out.push(Tok::Word(Word { text: w, quoted_from })),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser (recursive descent).
// ---------------------------------------------------------------------------

// Bound parser recursion and the expression tree traversed by evaluation and drop.
const MAX_QUERY_DEPTH: usize = 64;

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }

    fn parse_or(&mut self, depth: usize) -> Result<Expr, String> {
        let mut terms = vec![self.parse_and(depth)?];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.i += 1;
            terms.push(self.parse_and(depth)?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Expr::Or(terms)
        })
    }

    fn parse_and(&mut self, depth: usize) -> Result<Expr, String> {
        let mut terms = vec![self.parse_unary(depth)?];
        loop {
            match self.peek() {
                // Explicit AND, or juxtaposition (anything that can start a unary) = implicit AND.
                Some(Tok::And) => {
                    self.i += 1;
                    terms.push(self.parse_unary(depth)?);
                }
                Some(Tok::Word(_)) | Some(Tok::Not) | Some(Tok::LParen) => {
                    terms.push(self.parse_unary(depth)?);
                }
                _ => break,
            }
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Expr::And(terms)
        })
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Expr, String> {
        if depth > MAX_QUERY_DEPTH {
            return Err(format!(
                "query nesting exceeds {MAX_QUERY_DEPTH} levels — see `cv schema`"
            ));
        }
        if matches!(self.peek(), Some(Tok::Not)) {
            self.i += 1;
            return Ok(Expr::Not(Box::new(self.parse_unary(depth + 1)?)));
        }
        self.parse_primary(depth)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::LParen) => {
                self.i += 1;
                let e = self.parse_or(depth + 1)?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.i += 1;
                        Ok(e)
                    }
                    _ => Err("missing ')' — see `cv schema`".to_string()),
                }
            }
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.i += 1;
                parse_term(&w)
            }
            other => Err(format!("expected a term, found {other:?} — see `cv schema`")),
        }
    }
}

/// Parse one word into an [`Expr`]: a `field<op>value` term, or a bare needle that matches
/// title/cwd/id (an OR over the three).
fn parse_term(w: &Word) -> Result<Expr, String> {
    // A word that *starts* quoted is a phrase, never a field term: `"http://x"`, `"key: value"`.
    if w.quoted_from == Some(0) {
        return Ok(bare(&w.text));
    }
    if let Some((field, op, value)) = split_op(&w.text)? {
        // Was (any of) the value quoted? Quoted values are literal: exempt from comma-OR-splitting.
        let vstart = w.text.len() - value.len();
        let value_quoted = w.quoted_from.is_some_and(|q| q >= vstart);
        // A known field → a real term. A retired field names its replacement. Anything else (a
        // bare `http://…`, `key:value` prose) falls back to a plain needle.
        if let Some(def) = field_for(field) {
            if def.id == FieldId::Text && op != Op::Contains {
                return Err("`text:` uses full-text search and only takes `:`; use `text:\"quoted phrase\"` for a phrase — see `cv schema`".into());
            }
            if !matches!(def.kind, Kind::Num | Kind::Date) && !matches!(op, Op::Contains | Op::Eq) {
                return Err(format!(
                    "operator {} isn't valid on {} (string field) — see `cv schema`",
                    op.as_str(),
                    def.name
                ));
            }
            // Sugar aliases (the `Option<Op>` on the row) map onto an updated comparison. They
            // only take `:` — an explicit comparison on sugar (`before>=x`) is contradictory, so
            // it teaches instead of being silently overridden.
            let op = match def
                .aliases
                .iter()
                .find(|(a, _)| *a == field.to_ascii_lowercase())
                .and_then(|(_, sugar)| *sugar)
            {
                Some(sugar) => {
                    if op != Op::Contains {
                        return Err(format!(
                            "{field}: is sugar for `updated{}` and only takes `:` — write `updated{}` for an explicit comparison — see `cv schema`",
                            sugar.as_str(),
                            op.as_str()
                        ));
                    }
                    sugar
                }
                None => op,
            };
            let val = parse_value(def, op, value, value_quoted)?;
            return Ok(Expr::Term(Term { field: def.id, op, val }));
        }
        if matches!(field.to_ascii_lowercase().as_str(), "thread" | "path") {
            return Err(
                "`thread:` message paths were removed in 0.15.0 — select the session (`tool:`, `text:`, `has:`) and read the turns with `cv show` — see `cv schema`"
                    .to_string(),
            );
        }
        // Not a known field: fall through to a plain needle, so bare URLs (`http://…`) and
        // `key:value`-shaped prose still search title/cwd/id.
    }
    Ok(bare(&w.text))
}

/// A bare word → OR over title/cwd/id substring.
fn bare(w: &str) -> Expr {
    let needle = vec![w.to_lowercase()];
    Expr::Or(vec![
        Expr::Term(Term {
            field: FieldId::Title,
            op: Op::Contains,
            val: Val::Strs(needle.clone()),
        }),
        Expr::Term(Term {
            field: FieldId::Cwd,
            op: Op::Contains,
            val: Val::Strs(needle.clone()),
        }),
        Expr::Term(Term {
            field: FieldId::Id,
            op: Op::Contains,
            val: Val::Strs(needle),
        }),
    ])
}

// ---------------------------------------------------------------------------
// Value parsing.
// ---------------------------------------------------------------------------

/// `quoted`: the value was quoted in the source — it's one literal needle, exempt from
/// comma-OR-splitting (`title:"a, b"` is the phrase "a, b", not `a OR b`).
fn parse_value(def: &FieldDef, op: Op, raw: &str, quoted: bool) -> Result<Val, String> {
    /// Split an unquoted value into its comma-OR-list; keep a quoted value whole.
    fn or_list(raw: &str, quoted: bool, lower: bool) -> Vec<String> {
        let norm = |s: &str| if lower { s.to_lowercase() } else { s.to_string() };
        if quoted {
            std::iter::once(norm(raw)).filter(|s| !s.is_empty()).collect()
        } else {
            raw.split(',')
                .map(|s| norm(s.trim()))
                .filter(|s| !s.is_empty())
                .collect()
        }
    }
    match def.kind {
        Kind::Num => {
            if let Some((lo, hi)) = raw.split_once("..") {
                check_range_operator(def, op)?;
                let (lo, hi) = (parse_u64(lo, def)?, parse_u64(hi, def)?);
                if lo > hi {
                    return Err(format!("reversed range for {} — see `cv schema`", def.name));
                }
                Ok(Val::NumRange(lo, hi))
            } else {
                Ok(Val::Num(parse_u64(raw, def)?))
            }
        }
        Kind::Date => {
            if let Some((lo, hi)) = raw.split_once("..") {
                check_range_operator(def, op)?;
                let lo = parse_date(lo)?;
                // A date endpoint includes its entire day; RFC3339 endpoints remain exact.
                let hi = match NaiveDate::parse_from_str(hi.trim(), "%Y-%m-%d") {
                    Ok(day) => day
                        .and_hms_nano_opt(23, 59, 59, 999_999_999)
                        .expect("end of day")
                        .and_utc(),
                    Err(_) => parse_date(hi)?,
                };
                if lo > hi {
                    return Err(format!("reversed range for {} — see `cv schema`", def.name));
                }
                Ok(Val::DateRange(lo, hi))
            } else if op == Op::Contains {
                // A plain `field:DATE` means "any time that day", not "exactly midnight".
                if let Ok(d) = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
                    let lo = d.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
                    let hi = d
                        .and_hms_nano_opt(23, 59, 59, 999_999_999)
                        .expect("end of day")
                        .and_utc();
                    return Ok(Val::DateRange(lo, hi));
                }
                Ok(Val::Date(parse_date(raw)?)) // an RFC3339 instant stays exact
            } else {
                Ok(Val::Date(parse_date(raw)?))
            }
        }
        Kind::HarnessEnum => {
            let names = or_list(raw, quoted, false);
            if names.is_empty() {
                return Err(format!("empty value for {} — see `cv schema`", def.name));
            }
            for n in &names {
                if Harness::parse(n).is_none() {
                    return Err(format!("unknown harness {n:?} — see `cv schema`"));
                }
            }
            Ok(Val::Strs(names))
        }
        Kind::Flag => {
            let flags = or_list(raw, quoted, true);
            if flags.is_empty() {
                return Err(format!("empty value for {} — see `cv schema`", def.name));
            }
            for fl in &flags {
                if !HAS_FLAGS.iter().any(|(n, _, _)| n == fl) {
                    return Err(format!(
                        "unknown has-flag {fl:?} (try {}) — see `cv schema`",
                        HAS_FLAGS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>().join("/")
                    ));
                }
            }
            Ok(Val::Strs(flags))
        }
        Kind::Str | Kind::Path => {
            let strs = or_list(raw, quoted, true);
            if strs.is_empty() {
                return Err(format!("empty value for {} — see `cv schema`", def.name));
            }
            Ok(Val::Strs(strs))
        }
    }
}

fn check_range_operator(def: &FieldDef, op: Op) -> Result<(), String> {
    if matches!(op, Op::Contains | Op::Eq) {
        return Ok(());
    }
    Err(format!(
        "{} ranges take `:` or `=`; use two comparisons for explicit bounds — see `cv schema`",
        def.name
    ))
}

fn parse_u64(s: &str, def: &FieldDef) -> Result<u64, String> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| format!("{} wants a number, got {s:?} — see `cv schema`", def.name))
}

fn parse_date(val: &str) -> Result<DateTime<Utc>, String> {
    let val = val.trim();
    if let Ok(d) = NaiveDate::parse_from_str(val, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    }
    DateTime::parse_from_rfc3339(val)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| format!("date {val:?} isn't YYYY-MM-DD or RFC3339 — see `cv schema`"))
}

/// Split a `field<op>value` word into (field, op, value). Returns None if there's no operator.
fn split_op(w: &str) -> Result<Option<(&str, Op, &str)>, String> {
    // Scan for the first operator char outside of any value. Field names are alphanumeric plus
    // `_`, so the operator is the first char outside that set.
    let bytes = w.as_bytes();
    let mut k = 0;
    while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
        k += 1;
    }
    if k == 0 || k == bytes.len() {
        return Ok(None);
    }
    let field = &w[..k];
    // The retired regex operator: on a real field it names its replacement; `a~b` prose that never
    // was a field term stays the needle it always was.
    if w[k..].starts_with('~') {
        return if field_for(field).is_some() {
            Err(
                "the `~` regex operator was removed in 0.15.0 — `:` is substring, `=` exact — see `cv schema`"
                    .to_string(),
            )
        } else {
            Ok(None)
        };
    }
    // Longest operators first so `>=` isn't read as `>`. `strip_prefix` returns the value tail,
    // which is exactly `vstart`.
    let Some((op, vstart)) = [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
        (":", Op::Contains),
    ]
    .into_iter()
    .find_map(|(pat, op)| w[k..].strip_prefix(pat).map(|r| (op, r))) else {
        return Ok(None);
    };
    Ok(Some((field, op, vstart)))
}

// ---------------------------------------------------------------------------
// Reference generation (the LLM-facing minispec) + machine schema.
// ---------------------------------------------------------------------------

/// A compact, complete reference for the query language — printed by `cv schema` and embedded in
/// parse-error help so an agent crawling the CLI discovers the whole calculus from one place.
pub fn reference() -> String {
    let mut s = String::new();
    s.push_str("Session query calculus — one DSL for selecting sessions (used by `-q` on ls/dataset/stats).\n\n");
    s.push_str("GRAMMAR\n");
    s.push_str("  terms combine with implicit AND; also: OR (or |), NOT (or a leading -), and ( ) grouping.\n");
    s.push_str("  unquoted | is always OR, even without spaces; quote literal pipes in URLs or values.\n");
    s.push_str("  a term is  field<op>value  — or a bare word, which matches title/cwd/id.\n");
    s.push_str("  (an unknown `key:value` word is a bare needle too — so URLs just work.)\n");
    s.push_str("  ops: ':' (contains) '=' (exact); for numbers/dates also > >= < <=\n");
    s.push_str("  text: uses full-text search; only ':' is supported (quote a phrase for phrase search).\n");
    s.push_str("  values: word | \"quoted phrase\" | a,b,c (OR-list) | lo..hi (range, numbers & dates)\n");
    s.push_str("  ranges are inclusive and take ':' or '='; a date upper endpoint includes that whole day.\n");
    s.push_str("  quoting is literal: commas inside quotes are text, not an OR-list, and a fully-quoted\n");
    s.push_str("  word is always a needle. `field:YYYY-MM-DD` on a date field means that whole day.\n\n");
    s.push_str("FIELDS\n");
    let w = FIELDS.iter().map(|f| f.name.len()).max().unwrap_or(7);
    for f in FIELDS {
        let cost = cost_label(f.cost);
        s.push_str(&format!(
            "  {:<w$}  [{:<7}] {}\n      e.g. {}\n",
            f.name,
            cost,
            f.desc,
            f.example,
            w = w
        ));
        if !f.aliases.is_empty() {
            let names = f.aliases.iter().map(|(a, _)| *a).collect::<Vec<_>>().join(", ");
            s.push_str(&format!("      aliases: {names}\n"));
        }
    }
    s.push_str("\nhas-flags: ");
    s.push_str(&HAS_FLAGS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>().join(", "));
    s.push_str("\nharnesses: ");
    s.push_str(&Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", "));
    s.push_str("\n\nEXAMPLES\n");
    s.push_str("  harness:claude model:fable                  fable sessions from Claude\n");
    s.push_str("  (model:fable OR model:opus) -title:test     opus/fable, excluding tests\n");
    s.push_str("  msgs>=50 after:2026-01-01 tool:Bash         big recent sessions that ran Bash\n");
    s.push_str("  touched:src/ir.rs has:errors                edited ir.rs and hit an error\n");
    s.push_str("  text:\"SandboxDenied\" harness:codex          full-text within a harness\n");
    s.push_str("\nNOTE: parse=needs the transcript; index=needs `cv index`; events=needs the events catalog;\n");
    s.push_str(
        "      counted=the catalog's affordance counts (`cv index` fills them; an uncounted session is parsed once).\n",
    );
    s
}

/// The machine-readable schema (for `cv schema --json`) — fields, types, operators, costs, examples.
pub fn schema_json() -> serde_json::Value {
    use serde_json::json;
    let fields: Vec<_> = FIELDS
        .iter()
        .map(|f| {
            let kind = match f.kind {
                Kind::Str => "string",
                Kind::HarnessEnum => "harness",
                Kind::Num => "number",
                Kind::Date => "date",
                Kind::Path => "path",
                Kind::Flag => "flag",
            };
            let ops: Vec<&str> = if f.id == FieldId::Text {
                vec![":"]
            } else if matches!(f.kind, Kind::Num | Kind::Date) {
                vec![":", "=", ">", ">=", "<", "<="]
            } else {
                vec![":", "="]
            };
            let cost = cost_label(f.cost);
            json!({
                "name": f.name,
                "aliases": f.aliases.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
                "type": kind, "ops": ops,
                "cost": cost, "desc": f.desc, "example": f.example,
                "values": if matches!(f.kind, Kind::HarnessEnum) {
                    json!(Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>())
                } else if matches!(f.kind, Kind::Flag) {
                    json!(HAS_FLAGS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>())
                } else { serde_json::Value::Null },
            })
        })
        .collect();
    json!({
        "grammar": {
            "combine": ["implicit-AND", "AND", "OR", "|", "NOT", "-", "( )"],
            "term": "field<op>value | bareword(matches title/cwd/id)",
            "value_forms": ["word", "\"quoted\"", "a,b,c (OR-list)", "lo..hi (range)"],
        },
        "fields": fields,
    })
}

pub mod facts;
pub use facts::{
    filter_refs, filter_refs_limited, matches_full, matches_streamed, QueryReducer, SessionFacts, TextSets,
};

#[cfg(test)]
mod tests;
