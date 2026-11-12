//! The fidelity verifier behind `port --strict`: re-parse what an emitter wrote with the target's
//! own reader and classify every difference from the source as **expected** (a field the target
//! cannot hold — [`holds_fields`], the one capability table, and [`policy`]) or **unexpected** (one it
//! could have kept). `emit_verified` fails on an unexpected loss only, so a strict port is a checked claim.

use super::*;
mod accumulator;
use crate::harness::grok::emit::grok_model_id;
pub(super) use accumulator::source_features;
use accumulator::Accumulator;
#[cfg(feature = "sqlite")]
use anyhow::Context;

/// Re-parse a just-emitted session back into the IR using `target`'s adapter, so we can diff it
/// against the source. Mirrors how each adapter's `parse` keys off a [`SessionRef`].
#[cfg(test)]
pub(super) fn reparse_emitted(target: Harness, result: &EmitResult, opts: &Demand) -> Result<Session> {
    let opts = reparse_options(target, opts);
    reparse_with(target, result, &|adapter, reference| {
        crate::stream::collect_with(adapter, reference, &opts)
    })
}

fn reparse_with(
    target: Harness,
    result: &EmitResult,
    collect: &impl Fn(&dyn crate::harness::Adapter, &SessionRef) -> Result<Session>,
) -> Result<Session> {
    let sref = |id: String, path: PathBuf| SessionRef {
        id,
        harness: target,
        path,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: None,
    };
    match target {
        Harness::Claude => {
            let id = result.new_id.clone();
            collect(
                &crate::harness::claude::Claude::detect(),
                &sref(id, result.path.clone()),
            )
        }
        Harness::Codex => collect(
            // The rollout's own `session_meta` names the session; the ref id is only Codex's
            // fallback for a file without one, which a just-emitted rollout never is.
            &crate::harness::codex::Codex::detect(),
            &sref(String::new(), result.path.clone()),
        ),
        Harness::Grok => {
            // Grok's `parse` takes the title from the `SessionRef` (that is where discovery puts
            // the `summary.json` `session_summary`/`generated_title`); re-parsing from a bare ref
            // would lose it, so read the summary title into the ref exactly as discovery does.
            let title = fs::read_to_string(result.path.join("summary.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| {
                    v.get("session_summary")
                        .or_else(|| v.get("generated_title"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
            let mut r = sref(result.new_id.clone(), result.path.clone());
            r.title = title;
            collect(&crate::harness::grok::Grok::detect(), &r)
        }
        Harness::OpenClaw => collect(
            &crate::harness::openclaw::OpenClaw::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Gemini => collect(
            &crate::harness::gemini::Gemini::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::OpenCode => reparse_opencode(result, collect),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => reparse_hermes(result, collect),
        // The emitters whose output is parseable directly from the EmitResult path.
        Harness::LmStudio => collect(
            &crate::harness::lmstudio::LmStudio::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Cline => collect(
            &crate::harness::cline::Cline::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Roo => collect(
            &crate::harness::roo::Roo::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Continue => collect(
            &crate::harness::continuedev::Continue::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Qwen => collect(
            &crate::harness::qwen::Qwen::detect(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes => anyhow::bail!("re-parse for {target} not supported"),
        Harness::Cursor
        | Harness::Antigravity
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => anyhow::bail!("re-parse for {target} not supported"),
    }
}

/// OpenCode's live store is `opencode.db` (SQLite); the emitter writes the session there and
/// `result.path` IS that db file, so re-parse by pointing an adapter straight at it (no `HOME`
/// mutation: `set_var` is process-global and races any concurrent `getenv` — e.g. rayon discovery
/// threads). This fixes the old "could not locate storage root" failure of the dead JSON-tree path.
#[cfg(feature = "sqlite")]
fn reparse_opencode(
    result: &EmitResult,
    collect: &impl Fn(&dyn crate::harness::Adapter, &SessionRef) -> Result<Session>,
) -> Result<Session> {
    use crate::harness::Adapter;
    let oc = crate::harness::opencode::OpenCode::with_db(result.path.clone());
    oc.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| collect(&oc, &r).ok())
        })
        .context("re-discovering emitted opencode session")
}

#[cfg(not(feature = "sqlite"))]
fn reparse_opencode(
    _result: &EmitResult,
    _collect: &impl Fn(&dyn crate::harness::Adapter, &SessionRef) -> Result<Session>,
) -> Result<Session> {
    anyhow::bail!("opencode re-parse requires the `sqlite` feature")
}

/// Hermes re-parses straight from the emitted `state.db` (no `HERMES_HOME` mutation — see
/// [`reparse_opencode`] for why env writes are off-limits here).
#[cfg(feature = "sqlite")]
fn reparse_hermes(
    result: &EmitResult,
    collect: &impl Fn(&dyn crate::harness::Adapter, &SessionRef) -> Result<Session>,
) -> Result<Session> {
    use crate::harness::Adapter;
    let h = crate::harness::hermes::Hermes::with_db(result.path.clone());
    h.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| collect(&h, &r).ok())
        })
        .context("re-discovering emitted hermes session")
}

// ------------------------------------------------------------------------------------------------
// Fidelity verifier v2
// ------------------------------------------------------------------------------------------------

/// Two representations of one thing disagree, and a judge has ruled whether that is expected
/// (`docs/INTERFACE-V2.md` §8). The emit verifier reports one per field that changed across the
/// emit→re-parse round-trip (`a` the source's human value, `b` what the target's reader returned;
/// `expected` when the target cannot carry the field, else an `--strict` failure). The format
/// manifest check reports one per manifest entry its adapter source contradicts (`a` the
/// [`FindingKind`](crate::formats::FindingKind) tag, `b` the detail). `where` is the harness.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Divergence {
    pub what: String,
    pub a: String,
    pub b: String,
    pub expected: bool,
    #[serde(rename = "where")]
    pub at: String,
}

impl Divergence {
    /// A one-line `<what>: <a> → <b>` description (used in `--strict` errors and the `⚠ lost`
    /// lines). Values are [`brief`]ed: a real session's system prompt is thousands of lines, and
    /// dumping it into the terminal buried every other delta. The struct keeps the full values.
    pub fn describe(&self) -> String {
        format!("{}: {} → {}", self.what, brief(&self.a), brief(&self.b))
    }
}

/// A value shortened for one-line display: its first non-blank line, capped, plus the full
/// character count when anything was cut. Short single-line values pass through untouched.
pub(super) fn brief(s: &str) -> String {
    const MAX: usize = 72;
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let head: String = first.chars().take(MAX).collect();
    if head.len() < s.len() {
        format!("{head}… ({} chars)", s.chars().count())
    } else {
        head
    }
}

/// The structured result of the v2 fidelity check: losses and declared target defaults,
/// each classified expected/unexpected. An empty `deltas` is a fully clean round-trip.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FidelityReport {
    pub deltas: Vec<Divergence>,
}

impl FidelityReport {
    /// Any loss the target could have avoided — what `port --strict` fails on.
    #[cfg(test)]
    pub fn has_unexpected(&self) -> bool {
        self.deltas.iter().any(|d| !d.expected)
    }

    /// Render every delta as a human `⚠ lost` line (expected ones tagged so the reader knows the
    /// format simply cannot hold them). Both kinds are shown: the point is a complete picture.
    pub fn lost_lines(&self) -> Vec<String> {
        self.deltas
            .iter()
            .map(|d| {
                if d.expected {
                    format!("{} (expected: target format policy)", d.describe())
                } else {
                    d.describe()
                }
            })
            .collect()
    }
}

/// Per-message + per-session features extracted for a before/after comparison.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Features {
    role_counts: std::collections::BTreeMap<&'static str, usize>,
    kind_counts: std::collections::BTreeMap<&'static str, usize>,
    // Block-type counts (thinking is broken out into its three sub-features below).
    text: usize,
    tool_use: usize,
    tool_result: usize,
    image: usize,
    file: usize,
    thinking_text: usize,
    thinking_sig: usize,
    thinking_enc: usize,
    results_with_name: usize,
    results_with_details: usize,
    results_is_error: usize,
    usage: usize,
    cost: usize,
    // Cache-write tokens on a target with no native field for them (Continue carries usage/cost
    // without one); a target that holds the field reports them through its usage count.
    unheld_cache_creation: usize,
    // Timestamps and ids are counted separately for TOOL turns, because a store that keeps a tool
    // result inside its originating call's record gives that turn no record of its own to carry
    // them on — see [`Field::IdToolTurn`].
    timestamps: usize,
    timestamps_tool: usize,
    ids: usize,
    ids_tool: usize,
    model_eff: usize,
    dropped: usize,
}

/// Whether `target`’s format can hold this block at all (the block-level half of
/// [`holds_fields`]), under the requested [`ThinkingMode`]. A thinking block survives if
/// [`thinking_fate`] writes anything for it — structurally, as text, or as a placeholder.
pub(super) fn block_survives(target: Harness, thinking: ThinkingMode, b: &Block) -> bool {
    match b {
        Block::Text { .. } => true,
        Block::Thinking {
            text,
            signature,
            encrypted,
            redacted,
        } => {
            thinking_fate(
                target,
                thinking,
                text,
                signature.as_deref(),
                encrypted.as_deref(),
                *redacted,
            ) != ThinkingFate::Drop
        }
        Block::ToolUse { .. } => target_holds(target, Field::ToolUse),
        Block::ToolResult { .. } => target_holds(target, Field::ToolResult),
        Block::Image { .. } => target_holds(target, Field::Image),
        Block::File { .. } => target_holds(target, Field::File),
    }
}

impl Features {
    fn note(
        &mut self,
        target: Harness,
        thinking: ThinkingMode,
        m: &Message,
        session_model: bool,
        orphan_call: &impl Fn(&Block) -> bool,
    ) {
        let f = self;
        // Notes are presence facts (inline markers, `accumulator::project`), never source text.
        let unrepresentable = unrepresentable_checked(target, thinking, m, &crate::lazy::Resolver::new(None))
            .expect("noted facts are inline");
        if !m.is_carrier() && unrepresentable {
            f.dropped += 1;
        }
        // Two kinds of message are not *turns* for this comparison and are skipped whole — counting
        // their timestamps/ids/model would have reported losses that no emitter could have avoided:
        //
        //  * a format-complete **carrier** (empty-content System message holding a verbatim meta
        //    record) is bookkeeping the emitter replays byte-for-byte;
        //  * a turn the target cannot represent AT ALL ([`unrepresentable_checked`]), or one whose only
        //    content is unanswered calls the Claude emitter drops.
        if m.is_carrier() || unrepresentable || (!m.content.is_empty() && m.content.iter().all(&orphan_call)) {
            return;
        }
        // A store whose own reader re-tags a turn on the way back in is not LOSING it, and the
        // verifier must not cry loss over a transformation the format defines. Model the known one
        // here so both sides agree; anything else still shows up.
        let (role, kind) = if write_rules(target).reroles_client_commands && is_client_command(m) {
            ("system", MessageKind::InjectedContext.as_str())
        } else {
            (role_str(m.role), m.kind.as_str())
        };
        *f.role_counts.entry(role).or_default() += 1;
        *f.kind_counts.entry(kind).or_default() += 1;
        // A Tool turn's timestamp/id are counted apart from every other turn's: two of our targets
        // have no record to hang them on, and blanket-excusing ids or timestamps would have hidden
        // a real loss on a normal turn.
        let tool_turn = m.role == Role::Tool;
        if m.timestamp.is_some() {
            if tool_turn {
                f.timestamps_tool += 1;
            } else {
                f.timestamps += 1;
            }
        }
        if m.id.is_some() {
            if tool_turn {
                f.ids_tool += 1;
            } else {
                f.ids += 1;
            }
        }
        // Usage is a property of a MODEL turn, and that is the only place a store keeps it (Codex
        // attaches `token_count` to the assistant message it trails; OpenCode to the assistant
        // part). Claude also writes a `usage` block on its synthetic client-side error notices,
        // which parse as System turns — counting those made every real port report 13 lost usages
        // that no target has a slot for.
        if let (Role::Assistant, Some(usage)) = (m.role, &m.usage) {
            f.usage += 1;
            if !target_holds(target, Field::UsageCacheCreation) && usage.cache_creation_tokens.is_some() {
                f.unheld_cache_creation += 1;
            }
            if usage.cost_usd.is_some() {
                f.cost += 1;
            }
        }
        // Effective model: a turn's own model, else the session default (a target that records the
        // model once per session still carries it, so raw `Message::model` would flag non-losses).
        if m.model.is_some() || session_model {
            f.model_eff += 1;
        }
        for b in m.content.iter().filter(|b| !orphan_call(b)) {
            match b {
                Block::Text { .. } => f.text += 1,
                Block::ToolUse { .. } => f.tool_use += 1,
                Block::Image { .. } => f.image += 1,
                Block::File { .. } => f.file += 1,
                Block::ToolResult {
                    tool_name,
                    is_error,
                    details,
                    ..
                } => {
                    f.tool_result += 1;
                    if tool_name.as_deref().is_some_and(|n| !n.is_empty()) {
                        f.results_with_name += 1;
                    }
                    if *is_error {
                        f.results_is_error += 1;
                    }
                    if details.is_some() {
                        f.results_with_details += 1;
                    }
                }
                Block::Thinking {
                    text,
                    signature,
                    encrypted,
                    ..
                } => {
                    if !text.trim().is_empty() {
                        f.thinking_text += 1;
                    }
                    if signature.is_some() {
                        f.thinking_sig += 1;
                    }
                    if encrypted.is_some() {
                        f.thinking_enc += 1;
                    }
                }
            }
        }
    }
}

pub(super) fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// A fact the verifier compares across the round-trip, and the vocabulary of [`holds_fields`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    /// Text is universal; every conversational store keeps it.
    Text,
    ThinkingText,
    ThinkingSignature,
    ThinkingEncrypted,
    Image,
    File,
    ToolUse,
    ToolResult,
    /// A standalone tool name on a *result*, only where the format has a slot for it.
    ToolName,
    /// A tool result's error flag. Hermes has no slot for it. Its `messages` table (schema v30,
    /// `hermes_state_common.py` `SCHEMA_SQL`) is
    ///   id, session_id, role, content, tool_call_id, tool_calls, tool_name,
    ///   effect_disposition, timestamp, token_count, finish_reason, reasoning*,
    ///   codex_*_items, platform_message_id, observed, _compressed_summary, active,
    ///   compacted, api_content, display_kind, display_metadata, display_identity,
    ///   display_order
    /// — no error/status column. The near misses are not it: `effect_disposition` is the
    /// *side-effect* classification (`"none"` when a call was blocked, `"unknown"` on timeout;
    /// `agent/tool_executor.py`, consumed by `agent/replay_cleanup.py`), and `finish_reason` is
    /// the assistant turn's stop reason. Hermes's own writer proves it: `append_message` /
    /// `make_tool_result_message` (`agent/tool_dispatch_helpers.py`) take no error argument —
    /// a failure reaches the store only as the text of `content`. So the read side has nothing
    /// to read: `hermes.rs` builds every tool row's block with `is_error: false`.
    IsError,
    /// Structured `details`: Claude's `toolUseResult`, OpenCode's tool `state`.
    Details,
    Usage,
    Cost,
    /// Continue carries usage/cost but has no native cache-write field.
    UsageCacheCreation,
    /// Grok's `chat_history.jsonl` turns carry no per-message timestamp; LM Studio none either.
    Timestamp,
    /// A TOOL turn's own timestamp; see [`Field::IdToolTurn`].
    TimestampToolTurn,
    /// Per-message ids: kept by the stores that thread by a stable id.
    Id,
    /// A TOOL turn's own id, a narrower question than [`Field::Id`]. Where the store keeps a tool
    /// result INSIDE its originating call's record ([`folds_tool_result`]) the tool turn has no
    /// record of its own, so there is nowhere for an id or a timestamp to live — the reader
    /// rebuilds the turn out of the call. (Gemini's reader does recover a timestamp from the
    /// enclosing record, so in practice only the id is ever reported there; the classification is
    /// about what the STORE can hold.) Everywhere else the ordinary id/timestamp answer stands, so
    /// a real id loss on a tool turn stays unexpected.
    IdToolTurn,
    /// Per-message model, effective (a turn's own, else the session default): a [`policy`].
    Model,
    Title,
    Cwd,
    SessionModel,
    /// A session model the target must *replace* to stay runnable there: a [`policy`].
    SessionModelForeign,
    SystemPrompt,
    Principal,
    /// Lineage pointers: a [`policy`].
    Lineage,
    /// The target's reader re-reads its own output at the source's demand, format-complete
    /// included. OpenCode and Hermes do not: their verifier read is `Demand::RICH` (the
    /// database branches historically called `parse()`), whatever the source was read at.
    CompleteReparse,
}

impl Field {
    fn is_thinking(self) -> bool {
        matches!(
            self,
            Field::ThinkingText | Field::ThinkingSignature | Field::ThinkingEncrypted
        )
    }
}

/// What each emit target's native format can carry, grounded in its read side (`docs/FORMATS.md`,
/// the `harness/*.rs` parsers): everything a harness's arm omits is reported as an **expected** loss
/// (never fatal under `--strict`); everything it lists but lost is **unexpected**. Anthropic
/// signature-only thinking survives only into Anthropic-shaped stores, OpenAI encrypted reasoning
/// only into OpenAI-shaped ones; cost only into OpenCode or Continue. The arms are the table: the
/// match is exhaustive, so a new harness has no row until it is given one. OpenSession is no emit
/// target; it holds the one field only it has. The other read-only harnesses hold nothing.
#[rustfmt::skip]
pub(crate) fn holds_fields(h: Harness) -> &'static [Field] {
    use Field::*;
    match h {
        Harness::Claude => &[Text, ThinkingText, ThinkingSignature, Image, File, ToolUse, ToolResult, IsError, Details, Usage, UsageCacheCreation, Timestamp, TimestampToolTurn, Id, IdToolTurn, Title, Cwd, SessionModel, SystemPrompt, CompleteReparse],
        Harness::Codex => &[Text, ThinkingText, ThinkingEncrypted, Image, ToolUse, ToolResult, ToolName, IsError, Usage, UsageCacheCreation, Timestamp, TimestampToolTurn, Cwd, SessionModel, SystemPrompt, CompleteReparse],
        Harness::Grok => &[Text, ThinkingText, ThinkingEncrypted, ToolUse, ToolResult, IsError, UsageCacheCreation, Title, Cwd, SessionModel, CompleteReparse],
        Harness::OpenCode => &[Text, ThinkingText, ThinkingSignature, Image, File, ToolUse, ToolResult, ToolName, IsError, Details, Usage, Cost, UsageCacheCreation, Timestamp, Id, Title, Cwd, SessionModel],
        Harness::OpenClaw => &[Text, ThinkingText, ThinkingSignature, Image, ToolUse, ToolResult, ToolName, IsError, Usage, UsageCacheCreation, Timestamp, TimestampToolTurn, Id, IdToolTurn, Title, Cwd, SessionModel, CompleteReparse],
        Harness::Gemini => &[Text, ThinkingText, Image, File, ToolUse, ToolResult, ToolName, IsError, Usage, UsageCacheCreation, Timestamp, Id, Title, Cwd, SessionModel, CompleteReparse],
        Harness::Hermes => &[Text, ThinkingText, ThinkingEncrypted, Image, ToolUse, ToolResult, ToolName, Usage, UsageCacheCreation, Timestamp, TimestampToolTurn, Title, Cwd, SessionModel, SystemPrompt],
        Harness::LmStudio => &[Text, Usage, UsageCacheCreation, Title, SessionModel, CompleteReparse],
        Harness::Cline => &[Text, ThinkingText, Image, ToolUse, ToolResult, IsError, UsageCacheCreation, Timestamp, TimestampToolTurn, Id, IdToolTurn, Title, Cwd, SessionModel, CompleteReparse],
        Harness::Roo => &[Text, ThinkingText, Image, ToolUse, ToolResult, IsError, UsageCacheCreation, Timestamp, TimestampToolTurn, Id, IdToolTurn, Title, Cwd, SessionModel, CompleteReparse],
        Harness::Continue => &[Text, Image, ToolUse, ToolResult, IsError, Usage, Cost, Timestamp, TimestampToolTurn, Title, Cwd, SessionModel, CompleteReparse],
        Harness::Qwen => &[Text, ThinkingText, Image, File, ToolUse, ToolResult, ToolName, IsError, Usage, UsageCacheCreation, Timestamp, TimestampToolTurn, Id, IdToolTurn, Title, Cwd, SessionModel, CompleteReparse],
        Harness::OpenSession => &[Principal],
        Harness::Cursor
        | Harness::Antigravity
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport => &[],
    }
}

/// The fields whose classification is a decision about every target, not a fact about one store.
/// `None` defers to [`holds_fields`].
fn policy(field: Field) -> Option<bool> {
    match field {
        // The effective per-message model: a target that records the model once per session still
        // carries it (`Features::note`), so a drop is always one the emitter could have avoided.
        Field::Model => Some(true),
        // Grok resumes a session against `summary.json`'s `current_model_id`, so a foreign id
        // (`claude-fable-5-1`, `gpt-5.5`) would make it replay the history against a backend that
        // cannot serve it; [`grok_model_id`] pins those to `grok-build` on purpose. The verifier
        // reports the substitution, never as a loss the emitter could have avoided. A Grok model
        // (or a Grok→Grok rehome) is carried verbatim and still checked as `SessionModel`.
        Field::SessionModelForeign => Some(false),
        // Lineage reconstruction on a standalone emit needs a target the pointer resolves in; any
        // lineage change is expected (a fresh single-session port has nothing to point at).
        Field::Lineage => Some(false),
        _ => None,
    }
}

/// Whether `t`'s native format can carry `field`: [`policy`], else its [`holds_fields`] row.
pub(super) fn target_holds(t: Harness, field: Field) -> bool {
    policy(field).unwrap_or_else(|| holds_fields(t).contains(&field))
}

/// Whether `target`'s store keeps a tool result **inside its originating call's record** rather
/// than in a record of its own — OpenCode folds it into the assistant message's `tool` part
/// (`state.output`/`state.error`, `harness/opencode.rs` `tool_blocks`), Gemini into the same
/// `toolCalls[]` entry as the call (`toolCalls[].result`, `harness/gemini.rs`). Both readers
/// synthesize the IR's Tool turn as they parse the call, so it never had a row/object of its own.
/// Every other target writes a standalone record for the result (Claude a `user` line with a
/// `tool_result` block, Codex a `function_call_output` item, OpenClaw a `toolResult` message,
/// Hermes a `tool` row, Grok a `tool_result` line). Standalone framing permits a result-specific
/// id and timestamp; which fields survive depends on the target.
///
/// These emitters skip standalone [`Role::Tool`] records. The field table separately states
/// which ids and timestamps their readers recover from the enclosing call record.
#[cfg(test)]
pub(super) fn folds_tool_result(t: Harness) -> bool {
    matches!(t, Harness::OpenCode | Harness::Gemini)
}

/// What a target's writer (and its reader, on the way back) does beyond the fields it holds
/// ([`holds_fields`]): the rules the thinking projection and the verifier model. One row per
/// harness, exhaustive like [`holds_fields`], so a new harness states its rules or has none.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WriteRules {
    /// [`ThinkingMode::Portable`] is this target's replay policy (Claude's): plaintext thinking
    /// becomes text and reasoning without plaintext goes, so no thinking field is held under it.
    /// Elsewhere Portable is the text policy.
    pub portable_replay: bool,
    /// The writer drops a tool call the transcript never answered, and a turn that held nothing
    /// else (`claude::emit`, rule 1): the format's resumability rule, not an avoidable loss, so
    /// the verifier keeps call ids to tell which calls were answered.
    pub answered_calls_only: bool,
    /// The reader applies gemini-cli's `isIgnoredUserContent` rule (`utils/sessionUtils.ts`): a
    /// user turn beginning `/` or `?` is a command typed AT the client, never a prompt sent to the
    /// model, so reading one back yields a System / `injected_context` turn rather than the
    /// `prompt` that went in. The turn and its text survive intact; only the tagging changes, so
    /// this is a known transformation to model, not a loss to report. Qwen shares Gemini's parser.
    pub reroles_client_commands: bool,
    /// The writer pins a foreign session model to its own id ([`Field::SessionModelForeign`] in
    /// [`policy`]), as this function computes it.
    pub pinned_model: Option<fn(&Session) -> &str>,
}

pub(crate) fn write_rules(h: Harness) -> WriteRules {
    let none = WriteRules::default();
    match h {
        Harness::Claude => WriteRules {
            portable_replay: true,
            answered_calls_only: true,
            ..none
        },
        Harness::Gemini | Harness::Qwen => WriteRules {
            reroles_client_commands: true,
            ..none
        },
        Harness::Grok => WriteRules {
            pinned_model: Some(grok_model_id),
            ..none
        },
        Harness::Codex
        | Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Hermes
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => none,
    }
}

/// A human-typed client command (`/compact`, `?help`) rather than a prompt for the model.
pub(super) fn is_client_command(m: &Message) -> bool {
    m.kind == MessageKind::Prompt
        && m.text()
            .map(|t| {
                let t = t.trim_start();
                t.starts_with('/') || t.starts_with('?')
            })
            .unwrap_or(false)
}

/// [`target_holds`] under the requested [`ThinkingMode`]: `drop` is the user saying "lose the
/// reasoning", so a thinking loss there is expected no matter what the target could have held.
/// (`text` needs no override — the text lands in a [`Block::Text`] and the structured payloads are
/// still classified by the target's own capability, which is what makes the report stay honest.)
pub(crate) fn holds(target: Harness, thinking: ThinkingMode, field: Field) -> bool {
    if (thinking == ThinkingMode::Drop || (thinking == ThinkingMode::Portable && write_rules(target).portable_replay))
        && field.is_thinking()
    {
        return false;
    }
    target_holds(target, field)
}

/// The v2 diff: compare the source IR against what the target's own adapter read back, field by
/// field, classifying each surviving delta. Losses and target-added timestamps are reported;
/// added timestamps are declared target policy, not evidence of source timing.
#[cfg(test)]
pub(super) fn diff_fidelity(
    target: Harness,
    thinking: ThinkingMode,
    input: &Session,
    reparsed: &Session,
) -> FidelityReport {
    let before = source_features(target, thinking, input).expect("source fidelity facts");
    let after = source_features(target, thinking, reparsed).expect("reparsed fidelity facts");
    diff_features(target, thinking, input, reparsed, before, after)
}

pub(super) fn diff_features(
    target: Harness,
    thinking: ThinkingMode,
    input: &Session,
    reparsed: &Session,
    before: Features,
    after: Features,
) -> FidelityReport {
    let dropped = before.dropped;
    let mut deltas = Vec::new();

    // A count loss (after < before) for `field`, classified via the target's capability table.
    let at = || target.as_str().to_string();
    let count_delta = |deltas: &mut Vec<Divergence>, field: Field, label: &str, b: usize, a: usize| {
        if a < b {
            deltas.push(Divergence {
                what: label.to_string(),
                a: b.to_string(),
                b: a.to_string(),
                expected: !holds(target, thinking, field),
                at: at(),
            });
        }
    };
    // A dropped-or-changed session-level option field.
    fn opt_delta(
        deltas: &mut Vec<Divergence>,
        target: Harness,
        field: Field,
        label: &str,
        b: Option<String>,
        a: Option<String>,
    ) {
        if b.is_some() && a != b {
            deltas.push(Divergence {
                what: label.to_string(),
                a: b.unwrap_or_default(),
                b: a.unwrap_or_else(|| "(none)".into()),
                expected: !target_holds(target, field),
                at: target.as_str().to_string(),
            });
        }
    }

    if dropped > 0 {
        deltas.push(Divergence {
            what: "unrepresentable_turns".into(),
            a: dropped.to_string(),
            b: "0".into(),
            expected: true,
            at: at(),
        });
    }
    // Role / kind census. A lost *conversational* turn (user/assistant/tool) is always unexpected;
    // a lost system turn is format-dependent (Claude has no standalone system turn), so expected.
    for (role, &b) in &before.role_counts {
        let a = after.role_counts.get(role).copied().unwrap_or(0);
        if a < b {
            deltas.push(Divergence {
                what: format!("role:{role}"),
                a: b.to_string(),
                b: a.to_string(),
                expected: *role == "system",
                at: at(),
            });
        }
    }
    // Kinds: the three conversational kinds are tracked precisely (a dropped one is unexpected). The
    // meta/system kinds (notice, injected_context, compaction_*, error, model_change, …) are one
    // bucket: a target routinely *re-tags* a meta turn (a Notice becomes a Codex `developer`
    // message → InjectedContext) without losing it, and only a net *drop* of meta turns is a loss —
    // always an expected, format-dependent one.
    let is_conversational = |k: &str| matches!(k, "prompt" | "reply" | "tool_result");
    for kind in ["prompt", "reply", "tool_result"] {
        let b = before.kind_counts.get(kind).copied().unwrap_or(0);
        let a = after.kind_counts.get(kind).copied().unwrap_or(0);
        if a < b {
            deltas.push(Divergence {
                what: format!("kind:{kind}"),
                a: b.to_string(),
                b: a.to_string(),
                expected: false,
                at: at(),
            });
        }
    }
    let meta_before: usize = before
        .kind_counts
        .iter()
        .filter(|(k, _)| !is_conversational(k))
        .map(|(_, n)| *n)
        .sum();
    let meta_after: usize = after
        .kind_counts
        .iter()
        .filter(|(k, _)| !is_conversational(k))
        .map(|(_, n)| *n)
        .sum();
    if meta_after < meta_before {
        deltas.push(Divergence {
            what: "kind:meta".into(),
            a: meta_before.to_string(),
            b: meta_after.to_string(),
            expected: true,
            at: at(),
        });
    }

    count_delta(&mut deltas, Field::Text, "block:text", before.text, after.text);
    count_delta(
        &mut deltas,
        Field::ToolUse,
        "block:tool_use",
        before.tool_use,
        after.tool_use,
    );
    count_delta(
        &mut deltas,
        Field::ToolResult,
        "block:tool_result",
        before.tool_result,
        after.tool_result,
    );
    count_delta(&mut deltas, Field::Image, "block:image", before.image, after.image);
    count_delta(&mut deltas, Field::File, "block:file", before.file, after.file);
    count_delta(
        &mut deltas,
        Field::ThinkingText,
        "thinking(text)",
        before.thinking_text,
        after.thinking_text,
    );
    count_delta(
        &mut deltas,
        Field::ThinkingSignature,
        "thinking(signature)",
        before.thinking_sig,
        after.thinking_sig,
    );
    count_delta(
        &mut deltas,
        Field::ThinkingEncrypted,
        "thinking(encrypted)",
        before.thinking_enc,
        after.thinking_enc,
    );
    count_delta(
        &mut deltas,
        Field::ToolName,
        "tool_name on result",
        before.results_with_name,
        after.results_with_name,
    );
    count_delta(
        &mut deltas,
        Field::IsError,
        "tool result is_error",
        before.results_is_error,
        after.results_is_error,
    );
    count_delta(
        &mut deltas,
        Field::Details,
        "tool result details",
        before.results_with_details,
        after.results_with_details,
    );
    count_delta(&mut deltas, Field::Usage, "usage", before.usage, after.usage);
    count_delta(&mut deltas, Field::Cost, "usage cost", before.cost, after.cost);
    count_delta(
        &mut deltas,
        Field::UsageCacheCreation,
        "usage cache creation tokens",
        before.unheld_cache_creation,
        after.unheld_cache_creation,
    );
    count_delta(
        &mut deltas,
        Field::Timestamp,
        "timestamps",
        before.timestamps,
        after.timestamps,
    );
    count_delta(
        &mut deltas,
        Field::TimestampToolTurn,
        "timestamps (tool turns)",
        before.timestamps_tool,
        after.timestamps_tool,
    );
    // Some target stores require timestamps. Declare their defaulting instead of silently
    // presenting a newly written timestamp as source evidence.
    for (label, before, after) in [
        ("timestamps supplied by target", before.timestamps, after.timestamps),
        (
            "tool timestamps supplied by target",
            before.timestamps_tool,
            after.timestamps_tool,
        ),
    ] {
        if after > before {
            deltas.push(Divergence {
                what: label.into(),
                a: before.to_string(),
                b: after.to_string(),
                expected: true,
                at: at(),
            });
        }
    }
    for (label, before, after) in [
        (
            "session created_at supplied by target",
            input.created_at,
            reparsed.created_at,
        ),
        (
            "session updated_at supplied by target",
            input.updated_at,
            reparsed.updated_at,
        ),
    ] {
        if before.is_none() {
            if let Some(after) = after {
                deltas.push(Divergence {
                    what: label.into(),
                    a: "(unknown)".into(),
                    b: after.to_rfc3339(),
                    expected: true,
                    at: at(),
                });
            }
        }
    }
    count_delta(&mut deltas, Field::Id, "message ids", before.ids, after.ids);
    count_delta(
        &mut deltas,
        Field::IdToolTurn,
        "message ids (tool turns)",
        before.ids_tool,
        after.ids_tool,
    );
    count_delta(
        &mut deltas,
        Field::Model,
        "per-message model",
        before.model_eff,
        after.model_eff,
    );

    // Session-level fields: report a drop OR a change in value (not just absence).
    opt_delta(
        &mut deltas,
        target,
        Field::Title,
        "session title",
        input.title.clone(),
        reparsed.title.clone(),
    );
    opt_delta(
        &mut deltas,
        target,
        Field::Cwd,
        "session cwd",
        input.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        reparsed.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
    );
    // The session model. A target that deliberately *pins* its own model rather than carrying a
    // foreign one (Grok — see [`Field::SessionModelForeign`] in [`policy`]) is classified under that
    // key, so the substitution is reported but not counted against the emitter.
    let model_pinned = write_rules(target)
        .pinned_model
        .is_some_and(|pinned| input.model.as_deref().is_some_and(|m| pinned(input) != m));
    opt_delta(
        &mut deltas,
        target,
        if model_pinned {
            Field::SessionModelForeign
        } else {
            Field::SessionModel
        },
        if model_pinned {
            "session model (foreign)"
        } else {
            "session model"
        },
        input.model.clone(),
        reparsed.model.clone(),
    );
    opt_delta(
        &mut deltas,
        target,
        Field::SystemPrompt,
        "system prompt",
        input.system_prompt.clone(),
        reparsed.system_prompt.clone(),
    );
    opt_delta(
        &mut deltas,
        target,
        Field::Principal,
        "session principal",
        input.principal.as_ref().map(|p| serde_json::json!(p).to_string()),
        reparsed.principal.as_ref().map(|p| serde_json::json!(p).to_string()),
    );
    if !input.lineage.is_empty() && input.lineage != reparsed.lineage {
        deltas.push(Divergence {
            what: "lineage".into(),
            a: "present".into(),
            b: if reparsed.lineage.is_empty() {
                "(none)".into()
            } else {
                "changed".into()
            },
            expected: !target_holds(target, Field::Lineage),
            at: at(),
        });
    }

    FidelityReport { deltas }
}

pub(super) fn reparse_features(
    target: Harness,
    thinking: ThinkingMode,
    result: &EmitResult,
    opts: &Demand,
) -> Result<(Session, Features)> {
    let mut opts = reparse_options(target, opts);
    // Verification retains the same full/complete metadata and roles, but consumes bodies
    // through checked lazy views; complete-mode adapters may still require owned raw carriers.
    opts.spans = true;
    let sink = std::cell::RefCell::new(Accumulator::new(
        target,
        thinking,
        crate::lazy::Resolver::new(Some(result.path.clone())),
    ));
    let metadata = reparse_with(target, result, &|adapter, reference| {
        crate::stream::stream_counted(
            adapter,
            reference,
            &opts,
            &mut *sink.borrow_mut(),
            crate::stream::Pass::RECORD,
        )
        .map(|read| read.session)
    })?;
    let features = sink.into_inner().finish(&metadata);
    Ok((metadata, features))
}

fn reparse_options(target: Harness, opts: &Demand) -> Demand {
    if !target_holds(target, Field::CompleteReparse) {
        Demand::RICH
    } else {
        *opts
    }
}

/// A turn with nothing left to write once the target has dropped the blocks it cannot hold: every
/// block is unrepresentable there, so the emitted record is empty and the target’s own reader drops
/// it. The real case is Claude’s **signature-only thinking** — bound reasoning with no plaintext
/// and no portable blob (109 of 380 assistant turns in one 836-message session): ported to
/// Codex/OpenCode it leaves an empty turn those stores never read back.
///
/// Such turns are excluded from BOTH sides of the diff and reported once, as an expected
/// `unrepresentable_turns` delta — their disappearance is a consequence of a block-level loss the
/// table already calls expected, and counting them as lost turns made `--strict` fail on every real
/// Claude session (drowning the losses a target genuinely could have avoided).
///
/// [`ThinkingMode::Text`] is the answer to this: under it a signature-only block is written as a
/// placeholder, so the turn is representable after all and nothing here fires.
pub(crate) fn unrepresentable_checked(
    target: Harness,
    thinking: ThinkingMode,
    message: &Message,
    resolver: &crate::lazy::Resolver,
) -> Result<bool> {
    if message.content.is_empty() {
        return Ok(false);
    }
    for block in &message.content {
        let survives = match block {
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
            } => {
                thinking_fate(
                    target,
                    thinking,
                    &accumulator::marker(text, resolver)?,
                    signature.as_deref(),
                    encrypted.as_deref(),
                    *redacted,
                ) != ThinkingFate::Drop
            }
            _ => block_survives(target, thinking, block),
        };
        if survives {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    #[test]
    fn target_added_timestamps_are_declared() {
        let mut input = Session::new("source", Harness::Claude);
        let mut message = crate::Message::new(Role::Assistant);
        message.content.push(Block::Text { text: "answer".into() });
        input.messages.push(message);
        let mut output = input.clone();
        let timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        output.created_at = Some(timestamp);
        output.messages[0].timestamp = Some(timestamp);
        let report = diff_fidelity(Harness::Codex, ThinkingMode::Native, &input, &output);
        assert_eq!(report.deltas.len(), 2);
        assert!(report
            .deltas
            .iter()
            .all(|d| d.expected && d.what.contains("supplied by target")));
    }

    #[test]
    fn native_principal_loss_is_reported_and_retained_value_is_not() {
        let mut input = Session::new("opaque", Harness::OpenSession);
        input.principal = Some(crate::Principal::String("opaque-owner".into()));
        let mut output = input.clone();
        output.principal = None;
        for target in Harness::ALL.into_iter().filter(|h| *h != Harness::OpenSession) {
            let report = diff_fidelity(target, ThinkingMode::Native, &input, &output);
            let delta = report
                .deltas
                .iter()
                .find(|delta| delta.what == "session principal")
                .unwrap();
            assert!(delta.expected, "{target} has no principal field");
            assert!(delta.a.contains("opaque-owner"));
        }
        let retained = diff_fidelity(Harness::OpenSession, ThinkingMode::Native, &input, &input);
        assert!(!retained.deltas.iter().any(|delta| delta.what == "session principal"));
        let lost = diff_fidelity(Harness::OpenSession, ThinkingMode::Native, &input, &output);
        assert!(
            !lost
                .deltas
                .iter()
                .find(|delta| delta.what == "session principal")
                .unwrap()
                .expected
        );
    }
}
