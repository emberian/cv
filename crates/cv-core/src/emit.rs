//! port-a-sesh: emit an IR [`Session`] back into a harness's native on-disk format.
//!
//! This is the engine behind `cv convert` (cross-harness) and `cv port` (rehome a session to another
//! cwd / move local↔remote). Conversion is `parse(source) -> IR -> emit(target)`.
//!
//! Correctness bar: a session emitted for harness H must round-trip back through that harness's
//! parser (`emit(s, H) |> parse == s`, modulo fields H cannot represent). See `docs/FORMATS.md` for
//! each target's on-disk schema.
//!
//! NOTE: This module is owned by the port-a-sesh work. The free function [`emit`] is the public entry
//! point; `cv-core::lib` re-exports it and the `cv` CLI calls it.

use crate::harness::EmitResult;
use crate::ir::{Block, GitInfo, Harness, Message, MessageKind, Role, Session, SessionRef};
use crate::stream::ParseOptions;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Knobs for emitting/porting a session.
#[derive(Debug, Clone, Default)]
pub struct EmitOptions {
    /// Override the recorded working directory (rehome the session to a new project).
    pub new_cwd: Option<PathBuf>,
    /// Force a specific new session id (otherwise a fresh one is generated).
    pub new_id: Option<String>,
    /// Fail the emit if the fidelity verifier finds any **unexpected** loss (a field the target
    /// format *can* hold that didn't survive the round-trip). Expected losses — fields the format
    /// inherently cannot represent — never fail; they are reported under `⚠ lost`. The CLI wires
    /// this to `port --strict`. Only [`emit_verified`] consults it (plain [`emit`] never verifies).
    pub strict: bool,
}

/// An emitter: writes one IR session into one harness's native on-disk format.
type EmitFn = fn(&Session, &Path, &EmitOptions) -> Result<EmitResult>;

/// The ONE emit registry: which harnesses can be written, and by what. [`emit`] dispatches through
/// it and [`supported_targets`] is derived from it, so the dispatcher and the CLI's `--to` list can
/// never drift apart. Adding a target = adding an arm here (nothing else to keep in sync).
fn emitter_for(target: Harness) -> Option<EmitFn> {
    Some(match target {
        Harness::Claude => emit_claude,
        Harness::Codex => emit_codex,
        Harness::Grok => emit_grok,
        Harness::OpenCode => emit_opencode,
        Harness::OpenClaw => emit_openclaw,
        Harness::Gemini => emit_gemini,
        #[cfg(feature = "sqlite")]
        Harness::Hermes => emit_hermes,
        // Emitters living in their own adapter modules.
        Harness::Kimi => crate::harness::kimi::emit,
        Harness::LmStudio => crate::harness::lmstudio::emit,
        Harness::Cline => crate::harness::cline::emit,
        Harness::Roo => crate::harness::roo::emit,
        Harness::Continue => crate::harness::continuedev::emit,
        // Qwen Code stores the same ConversationRecord shape Gemini does, so it reuses that emitter.
        Harness::Qwen => emit_gemini,
        // Parse-only harnesses (Cursor, Goose, desktop apps, …) aren't conversion targets.
        _ => return None,
    })
}

/// Emit `session` into `target`'s native format under `out_dir` (the target harness's storage root,
/// or any directory for a dry run). Returns where it was written + how to resume it.
pub fn emit(session: &Session, target: Harness, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    #[cfg(not(feature = "sqlite"))]
    if target == Harness::Hermes {
        anyhow::bail!("emit to hermes requires the `sqlite` feature");
    }
    let Some(f) = emitter_for(target) else {
        anyhow::bail!("emit to {target} is not supported yet");
    };
    // Guard: emitters read content via `msg.text()`/`Deref`, which PANICS on an unresolved lazy
    // [`Span`](crate::lazy::Span) — and a span that reached `serde_json::to_value` would serialize
    // as its `{source,off,len}` struct, shipping garbage into the target file. A lazily-parsed
    // session must be resolved before it hits an emitter; do it here (on a clone, since `emit`
    // takes `&Session`) so no caller can forget.
    if has_unresolved_spans(session) {
        let mut owned = session.clone();
        owned.materialize();
        return f(&owned, out_dir, opts);
    }
    f(session, out_dir, opts)
}

/// Whether any message content field is still a lazy [`Span`](crate::lazy::Span) (see [`emit`]).
fn has_unresolved_spans(session: &Session) -> bool {
    session.messages.iter().any(|m| {
        m.content.iter().any(|b| match b {
            Block::Text { text } | Block::Thinking { text, .. } => text.is_span(),
            Block::ToolResult { content, .. } => content.is_span(),
            _ => false,
        })
    })
}

/// Emit `session` into `target`, then re-parse the written output with `target`'s own adapter and
/// diff it against the input IR. Returns the [`EmitResult`] plus a list of human-readable
/// lossy-conversion warnings (empty when the round-trip is clean). The CLI prints these so a user
/// porting a session knows what (if anything) didn't survive.
///
/// This never changes what `emit` writes — it's a read-back verification pass on top of it.
///
/// With [`EmitOptions::strict`] a single **unexpected** delta (a field the target could hold but
/// lost) fails the emit; expected losses never do. The returned `Vec<String>` is the human `⚠ lost`
/// rendering (both expected and unexpected losses, so the user sees the full picture). Callers that
/// want the structured deltas call [`emit_report`].
pub fn emit_verified(
    session: &Session,
    target: Harness,
    out_dir: &Path,
    opts: &EmitOptions,
) -> Result<(EmitResult, Vec<String>)> {
    let (result, report) = emit_report(session, target, out_dir, opts)?;
    if opts.strict {
        let unexpected: Vec<&Delta> = report.deltas.iter().filter(|d| !d.expected).collect();
        if !unexpected.is_empty() {
            let joined = unexpected.iter().map(|d| d.describe()).collect::<Vec<_>>().join("; ");
            anyhow::bail!(
                "--strict: {} unexpected fidelity loss(es) porting to {target}: {joined}",
                unexpected.len()
            );
        }
    }
    Ok((result, report.lost_lines()))
}

/// Like [`emit_verified`] but returns the structured [`FidelityReport`] instead of rendered strings,
/// and never fails on a delta (the caller decides what `strict` means). This is the field-by-field
/// diff between the source IR and what the target's own adapter reads back — the v2 verifier.
pub fn emit_report(
    session: &Session,
    target: Harness,
    out_dir: &Path,
    opts: &EmitOptions,
) -> Result<(EmitResult, FidelityReport)> {
    let result = emit(session, target, out_dir, opts)?;
    // Re-parse at the fidelity the SOURCE was parsed at. `cv port` parses a same-harness rehome
    // format-complete (`port::parse_for_emit`), and re-parsing that plainly compared a complete
    // session against a lean one: impossible deltas such as `per-message model: 2106 → 836` on an
    // 836-message session, because the source's carriers (and the complete-mode roles of synthetic
    // notices) had no counterpart. Carriers are the tell — only a complete parse produces them —
    // and their presence is exactly what has to match on both sides.
    let reparse_opts = if session.messages.iter().any(is_carrier) {
        ParseOptions::complete()
    } else {
        ParseOptions::full()
    };
    let report = match reparse_emitted(target, &result, &reparse_opts) {
        Ok(reparsed) => diff_fidelity(target, session, &reparsed),
        Err(e) => FidelityReport {
            deltas: vec![Delta {
                field: "round_trip".into(),
                before: "parseable".into(),
                after: format!("re-parse failed ({e:#})"),
                expected: false,
            }],
        },
    };
    Ok((result, report))
}

/// Re-parse a just-emitted session back into the IR using `target`'s adapter, so we can diff it
/// against the source. Mirrors how each adapter's `parse` keys off a [`SessionRef`].
fn reparse_emitted(target: Harness, result: &EmitResult, opts: &ParseOptions) -> Result<Session> {
    use crate::harness::Adapter;
    // Adapters that stream honour `opts` (complete ⇒ carriers + exhaustive `extra`); the rest fall
    // through `Adapter::stream`’s default bridge to `parse`, which ignores it.
    let collect = |a: &dyn Adapter, r: &SessionRef| crate::stream::collect_with(a, r, opts);
    let sref = |id: String, path: PathBuf| SessionRef {
        id,
        harness: target,
        path,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };
    match target {
        Harness::Claude => {
            let id = result.new_id.clone();
            collect(&crate::harness::claude::Claude::new(), &sref(id, result.path.clone()))
        }
        Harness::Codex => collect(
            &crate::harness::codex::Codex::new(),
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
            collect(&crate::harness::grok::Grok::new(), &r)
        }
        Harness::OpenClaw => collect(
            &crate::harness::openclaw::OpenClaw::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Gemini => collect(
            &crate::harness::gemini::Gemini::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::OpenCode => reparse_opencode(result),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => reparse_hermes(result),
        // The emitters whose output is parseable directly from the EmitResult path.
        Harness::Kimi => collect(
            &crate::harness::kimi::Kimi::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::LmStudio => collect(
            &crate::harness::lmstudio::LmStudio::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Cline => collect(
            &crate::harness::cline::Cline::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Roo => collect(
            &crate::harness::roo::Roo::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        Harness::Continue => collect(
            &crate::harness::continuedev::Continue::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        // Qwen reuses Gemini's emitter (same record shape) but its own parser.
        Harness::Qwen => collect(
            &crate::harness::qwen::Qwen::new(),
            &sref(result.new_id.clone(), result.path.clone()),
        ),
        other => anyhow::bail!("re-parse for {other} not supported"),
    }
}

/// OpenCode's live store is `opencode.db` (SQLite); the emitter writes the session there and
/// `result.path` IS that db file, so re-parse by pointing an adapter straight at it (no `HOME`
/// mutation: `set_var` is process-global and races any concurrent `getenv` — e.g. rayon discovery
/// threads). This fixes the old "could not locate storage root" failure of the dead JSON-tree path.
#[cfg(feature = "sqlite")]
fn reparse_opencode(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let oc = crate::harness::opencode::OpenCode::with_db(result.path.clone());
    oc.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| oc.parse(&r).ok())
        })
        .context("re-discovering emitted opencode session")
}

#[cfg(not(feature = "sqlite"))]
fn reparse_opencode(_result: &EmitResult) -> Result<Session> {
    anyhow::bail!("opencode re-parse requires the `sqlite` feature")
}

/// Hermes re-parses straight from the emitted `state.db` (no `HERMES_HOME` mutation — see
/// [`reparse_opencode`] for why env writes are off-limits here).
#[cfg(feature = "sqlite")]
fn reparse_hermes(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let h = crate::harness::hermes::Hermes::with_db(result.path.clone());
    h.discover()
        .ok()
        .and_then(|refs| {
            refs.into_iter()
                .find(|r| r.id == result.new_id)
                .and_then(|r| h.parse(&r).ok())
        })
        .context("re-discovering emitted hermes session")
}

// ------------------------------------------------------------------------------------------------
// Fidelity verifier v2
// ------------------------------------------------------------------------------------------------

/// One field that changed across the emit→re-parse round-trip. `before`/`after` are human strings
/// (counts, option values). `expected` is true when the target format inherently cannot carry the
/// field (a known, non-fatal loss); false when it could have and didn't (an `--strict` failure).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Delta {
    pub field: String,
    pub before: String,
    pub after: String,
    pub expected: bool,
}

impl Delta {
    /// A one-line `<field>: <before> → <after>` description (used in `--strict` errors and the
    /// `⚠ lost` lines). Values are [`brief`]ed: a real session's system prompt is thousands of
    /// lines, and dumping it into the terminal buried every other delta. The struct keeps the full
    /// values for `--json` consumers.
    pub fn describe(&self) -> String {
        format!("{}: {} → {}", self.field, brief(&self.before), brief(&self.after))
    }
}

/// A value shortened for one-line display: its first non-blank line, capped, plus the full
/// character count when anything was cut. Short single-line values pass through untouched.
fn brief(s: &str) -> String {
    const MAX: usize = 72;
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let head: String = first.chars().take(MAX).collect();
    if head.len() < s.len() {
        format!("{head}… ({} chars)", s.chars().count())
    } else {
        head
    }
}

/// The structured result of the v2 fidelity check: every field that did not survive the round-trip,
/// each classified expected/unexpected. An empty `deltas` is a fully clean round-trip.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FidelityReport {
    pub deltas: Vec<Delta>,
}

impl FidelityReport {
    /// Any loss the target could have avoided — what `port --strict` fails on.
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
                    format!("{} (expected: this format cannot carry it)", d.describe())
                } else {
                    d.describe()
                }
            })
            .collect()
    }
}

/// Per-message + per-session features extracted for a before/after comparison.
#[derive(Default)]
struct Features {
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
    timestamps: usize,
    ids: usize,
    model_eff: usize,
}

/// A verbatim meta record carried under `ParseOptions::complete` — never a conversational turn.
fn is_carrier(m: &Message) -> bool {
    m.kind == MessageKind::Carrier || m.extra.contains_key(crate::harness::claude::CARRIER_KEY)
}

/// Whether `target`’s format can hold this block at all (the block-level half of
/// [`target_holds`]). A thinking block survives if ANY of its three payloads does.
fn block_survives(target: Harness, b: &Block) -> bool {
    match b {
        Block::Text { .. } => true,
        Block::Thinking {
            text,
            signature,
            encrypted,
            ..
        } => {
            (!text.trim().is_empty() && target_holds(target, "thinking_text"))
                || (signature.is_some() && target_holds(target, "thinking_signature"))
                || (encrypted.is_some() && target_holds(target, "thinking_encrypted"))
        }
        Block::ToolUse { .. } => target_holds(target, "tool_use"),
        Block::ToolResult { .. } => target_holds(target, "tool_result"),
        Block::Image { .. } => target_holds(target, "image"),
        Block::File { .. } => target_holds(target, "file"),
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
fn unrepresentable(target: Harness, m: &Message) -> bool {
    !m.content.is_empty() && !m.content.iter().any(|b| block_survives(target, b))
}

fn features(target: Harness, session: &Session) -> Features {
    let mut f = Features::default();
    for m in &session.messages {
        // Two kinds of message are not *turns* for this comparison and are skipped whole — counting
        // their timestamps/ids/model would have reported losses that no emitter could have avoided:
        //
        //  * a format-complete **carrier** (empty-content System message holding a verbatim meta
        //    record) is bookkeeping the emitter replays byte-for-byte;
        //  * a turn the target cannot represent AT ALL ([`unrepresentable`]).
        if is_carrier(m) || unrepresentable(target, m) {
            continue;
        }
        *f.role_counts.entry(role_str(m.role)).or_default() += 1;
        *f.kind_counts.entry(m.kind.as_str()).or_default() += 1;
        if m.timestamp.is_some() {
            f.timestamps += 1;
        }
        if m.id.is_some() {
            f.ids += 1;
        }
        // Usage is a property of a MODEL turn, and that is the only place a store keeps it (Codex
        // attaches `token_count` to the assistant message it trails; OpenCode to the assistant
        // part). Claude also writes a `usage` block on its synthetic client-side error notices,
        // which parse as System turns — counting those made every real port report 13 lost usages
        // that no target has a slot for.
        if m.usage.is_some() && m.role == Role::Assistant {
            f.usage += 1;
        }
        if m.usage.as_ref().and_then(|u| u.cost_usd).is_some() && m.role == Role::Assistant {
            f.cost += 1;
        }
        // Effective model: a turn's own model, else the session default (the "model only when it
        // differs" IR rule means comparing raw `Message::model` would flag non-losses).
        if m.model.is_some() || session.model.is_some() {
            f.model_eff += 1;
        }
        for b in &m.content {
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
    f
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Whether `target`'s native format can carry `field`. Everything it cannot is reported as an
/// **expected** loss (never fatal under `--strict`); everything it can but didn't is **unexpected**.
/// Grounded in each adapter's read side (`docs/FORMATS.md`, the `harness/*.rs` parsers):
/// Anthropic signature-only thinking survives only into Anthropic-shaped stores; OpenAI encrypted
/// reasoning only into OpenAI-shaped ones; per-message ids not into Codex/Hermes; a standalone tool
/// name on a *result* only where the format has a slot for it; structured `details` only into
/// Claude's `toolUseResult` / OpenCode's tool `state`; cost only into OpenCode.
fn target_holds(t: Harness, field: &str) -> bool {
    use Harness::*;
    match field {
        // Text is universal; every conversational store keeps it.
        "text" => true,
        "thinking_text" => !matches!(t, LmStudio | Continue),
        "thinking_signature" => matches!(t, Claude | OpenCode | OpenClaw),
        "thinking_encrypted" => matches!(t, Codex | Grok | Hermes),
        "image" => !matches!(t, Grok | LmStudio),
        "file" => matches!(t, Claude | OpenCode | Gemini),
        "tool_use" | "tool_result" => !matches!(t, LmStudio),
        "tool_name" => matches!(t, OpenClaw | Hermes | Codex | Gemini | OpenCode),
        "is_error" => !matches!(t, LmStudio),
        "details" => matches!(t, Claude | OpenCode),
        "usage" => !matches!(t, Grok | Cline | Roo | Continue),
        "cost" => matches!(t, OpenCode),
        // Grok's `chat_history.jsonl` turns carry no per-message timestamp; LM Studio none either.
        "timestamp" => !matches!(t, LmStudio | Grok),
        // Per-message ids: kept by the stores that thread by a stable id.
        "id" => matches!(t, Claude | OpenCode | OpenClaw | Gemini | Kimi | Cline | Roo),
        "model" => true,
        "title" => !matches!(t, Codex),
        "cwd" => !matches!(t, LmStudio),
        "session_model" => true,
        "system_prompt" => matches!(t, Claude | Codex | Hermes | Kimi),
        // Lineage reconstruction on a standalone emit needs a target the pointer resolves in; treat
        // any lineage change as expected (a fresh single-session port has nothing to point at).
        "lineage" => false,
        _ => true,
    }
}

/// The v2 diff: compare the source IR against what the target's own adapter read back, field by
/// field, classifying each surviving delta. Only *losses* (count decreases, dropped/altered session
/// fields) are reported — the re-parse legitimately adds records (compaction notes, synthesized
/// ids/timestamps), and an addition is never a fidelity loss.
fn diff_fidelity(target: Harness, input: &Session, reparsed: &Session) -> FidelityReport {
    let before = features(target, input);
    let after = features(target, reparsed);
    // Turns the target cannot represent at all: reported once, as their own expected loss, instead
    // of as a phantom deficit spread over role/kind/timestamp/id counts.
    let dropped = input
        .messages
        .iter()
        .filter(|m| !is_carrier(m) && unrepresentable(target, m))
        .count();
    let mut deltas = Vec::new();

    // A count loss (after < before) for `field`, classified via the target's capability table.
    fn count_delta(deltas: &mut Vec<Delta>, target: Harness, field: &str, label: &str, b: usize, a: usize) {
        if a < b {
            deltas.push(Delta {
                field: label.to_string(),
                before: b.to_string(),
                after: a.to_string(),
                expected: !target_holds(target, field),
            });
        }
    }
    // A dropped-or-changed session-level option field.
    fn opt_delta(
        deltas: &mut Vec<Delta>,
        target: Harness,
        field: &str,
        label: &str,
        b: Option<String>,
        a: Option<String>,
    ) {
        if b.is_some() && a != b {
            deltas.push(Delta {
                field: label.to_string(),
                before: b.unwrap_or_default(),
                after: a.unwrap_or_else(|| "(none)".into()),
                expected: !target_holds(target, field),
            });
        }
    }

    if dropped > 0 {
        deltas.push(Delta {
            field: "unrepresentable_turns".into(),
            before: dropped.to_string(),
            after: "0".into(),
            expected: true,
        });
    }
    // Role / kind census. A lost *conversational* turn (user/assistant/tool) is always unexpected;
    // a lost system turn is format-dependent (Claude has no standalone system turn), so expected.
    for (role, &b) in &before.role_counts {
        let a = after.role_counts.get(role).copied().unwrap_or(0);
        if a < b {
            deltas.push(Delta {
                field: format!("role:{role}"),
                before: b.to_string(),
                after: a.to_string(),
                expected: *role == "system",
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
            deltas.push(Delta {
                field: format!("kind:{kind}"),
                before: b.to_string(),
                after: a.to_string(),
                expected: false,
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
        deltas.push(Delta {
            field: "kind:meta".into(),
            before: meta_before.to_string(),
            after: meta_after.to_string(),
            expected: true,
        });
    }

    count_delta(&mut deltas, target, "text", "block:text", before.text, after.text);
    count_delta(
        &mut deltas,
        target,
        "tool_use",
        "block:tool_use",
        before.tool_use,
        after.tool_use,
    );
    count_delta(
        &mut deltas,
        target,
        "tool_result",
        "block:tool_result",
        before.tool_result,
        after.tool_result,
    );
    count_delta(&mut deltas, target, "image", "block:image", before.image, after.image);
    count_delta(&mut deltas, target, "file", "block:file", before.file, after.file);
    count_delta(
        &mut deltas,
        target,
        "thinking_text",
        "thinking(text)",
        before.thinking_text,
        after.thinking_text,
    );
    count_delta(
        &mut deltas,
        target,
        "thinking_signature",
        "thinking(signature)",
        before.thinking_sig,
        after.thinking_sig,
    );
    count_delta(
        &mut deltas,
        target,
        "thinking_encrypted",
        "thinking(encrypted)",
        before.thinking_enc,
        after.thinking_enc,
    );
    count_delta(
        &mut deltas,
        target,
        "tool_name",
        "tool_name on result",
        before.results_with_name,
        after.results_with_name,
    );
    count_delta(
        &mut deltas,
        target,
        "is_error",
        "tool result is_error",
        before.results_is_error,
        after.results_is_error,
    );
    count_delta(
        &mut deltas,
        target,
        "details",
        "tool result details",
        before.results_with_details,
        after.results_with_details,
    );
    count_delta(&mut deltas, target, "usage", "usage", before.usage, after.usage);
    count_delta(&mut deltas, target, "cost", "usage cost", before.cost, after.cost);
    count_delta(
        &mut deltas,
        target,
        "timestamp",
        "timestamps",
        before.timestamps,
        after.timestamps,
    );
    count_delta(&mut deltas, target, "id", "message ids", before.ids, after.ids);
    count_delta(
        &mut deltas,
        target,
        "model",
        "per-message model",
        before.model_eff,
        after.model_eff,
    );

    // Session-level fields: report a drop OR a change in value (not just absence).
    opt_delta(
        &mut deltas,
        target,
        "title",
        "session title",
        input.title.clone(),
        reparsed.title.clone(),
    );
    opt_delta(
        &mut deltas,
        target,
        "cwd",
        "session cwd",
        input.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        reparsed.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
    );
    opt_delta(
        &mut deltas,
        target,
        "session_model",
        "session model",
        input.model.clone(),
        reparsed.model.clone(),
    );
    opt_delta(
        &mut deltas,
        target,
        "system_prompt",
        "system prompt",
        input.system_prompt.clone(),
        reparsed.system_prompt.clone(),
    );
    if !input.lineage.is_empty() && input.lineage != reparsed.lineage {
        deltas.push(Delta {
            field: "lineage".into(),
            before: "present".into(),
            after: if reparsed.lineage.is_empty() {
                "(none)".into()
            } else {
                "changed".into()
            },
            expected: !target_holds(target, "lineage"),
        });
    }

    FidelityReport { deltas }
}

/// Which targets [`emit`] can currently write — derived from [`emitter_for`] (the one registry),
/// so this list is correct by construction.
pub fn supported_targets() -> &'static [Harness] {
    static TARGETS: std::sync::OnceLock<Vec<Harness>> = std::sync::OnceLock::new();
    TARGETS.get_or_init(|| {
        Harness::ALL
            .iter()
            .copied()
            .filter(|&h| emitter_for(h).is_some())
            .collect()
    })
}

/// Effective cwd after applying any rehome override.
fn effective_cwd(session: &Session, opts: &EmitOptions) -> Option<PathBuf> {
    opts.new_cwd.clone().or_else(|| session.cwd.clone())
}

/// Branch name (if any) for git-flavored metadata fields.
fn branch_of(session: &Session) -> Option<String> {
    session.git.as_ref().and_then(|g| g.branch.clone())
}

// ------------------------------------------------------------------------------------------------
// Claude
// ------------------------------------------------------------------------------------------------

/// Resolve symlinks in `cwd` the way Claude Code does when it looks for sessions to `--resume`.
///
/// Claude derives a session's project dir from the *realpath* of the directory it's launched in, so
/// a session we write must be keyed off the resolved path or `claude --resume` reports "No
/// conversation found". This bites on macOS, where `/tmp` and `/var` are symlinks into `/private`:
/// porting a session whose cwd is `/tmp/foo` would write `-tmp-foo` while Claude, launched from
/// `/tmp/foo`, looks under `-private-tmp-foo`. Canonicalizing here keeps the two in lockstep and
/// also matches what Claude records in its own transcripts (it runs in the resolved dir).
///
/// Falls back to the path as-given when it can't be resolved — e.g. the target dir doesn't exist
/// yet — which preserves the prior behavior for those cases.
fn realpath_for_claude(cwd: &Path) -> PathBuf {
    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Encode a cwd into Claude's project-dir name: leading `-`, then every `/` and `.` becomes `-`.
fn claude_encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    // Strip a single leading '/' so the mandatory leading '-' isn't doubled.
    let s = s.strip_prefix('/').unwrap_or(&s);
    let mut out = String::with_capacity(s.len() + 1);
    out.push('-');
    for ch in s.chars() {
        match ch {
            '/' | '.' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

fn emit_claude(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    // Resolve symlinks so the project-dir name matches the realpath Claude resolves at resume time.
    let cwd = effective_cwd(session, opts).map(|p| realpath_for_claude(&p));
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir_name = cwd
        .as_ref()
        .map(|p| claude_encode_cwd(p))
        .unwrap_or_else(|| "-".to_string());
    let branch = branch_of(session);

    let proj_dir = out_dir.join(&dir_name);
    fs::create_dir_all(&proj_dir).with_context(|| format!("creating {}", proj_dir.display()))?;
    let file_path = proj_dir.join(format!("{new_id}.jsonl"));

    let mut lines: Vec<Value> = Vec::new();
    let ts0 = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // ai-title line (so the title round-trips) — unless a format-complete carrier already replays the
    // original ai-title record verbatim (emitting both would duplicate it).
    let has_title_carrier = session
        .messages
        .iter()
        .any(|m| carrier_record_of(m).is_some_and(|r| r.get("type").and_then(Value::as_str) == Some("ai-title")));
    if let Some(title) = &session.title {
        if !has_title_carrier {
            lines.push(json!({
                "type": "ai-title",
                "aiTitle": title,
                "sessionId": new_id,
                "cwd": cwd_str,
                "timestamp": ts0,
            }));
        }
    }

    // Session-level system prompt → a `prompt_snapshot` attachment (Claude's own slot for it), so a
    // cross-harness port keeps the instructions the model ran under. A Claude→Claude session already
    // carries this (its own `prompt_snapshot` attachment rides through as a carrier), so emit it only
    // for a foreign source to avoid a duplicate.
    if session.harness != Harness::Claude {
        if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            lines.push(json!({
                "type": "attachment",
                "sessionId": new_id,
                "cwd": cwd_str,
                "timestamp": ts0,
                "attachment": { "type": "prompt_snapshot", "systemPrompt": [sp] },
            }));
        }
    }

    // Thread the conversation by uuid / parentUuid.
    let mut parent: Option<String> = None;
    // Non-carrier System turns can't be replayed (see below) and are dropped — record each dropped
    // record's uuid → its own (surviving) parent, so a child that pointed at a dropped uuid is
    // re-linked over the gap instead of left with a dangling parentUuid. `resolve_parent` follows
    // chains of consecutively-dropped ancestors (bounded; parent links form a DAG).
    let mut dropped: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    fn resolve_parent(dropped: &std::collections::HashMap<String, Option<String>>, pid: String) -> Option<String> {
        let mut p = Some(pid);
        for _ in 0..=dropped.len() {
            match p.as_ref().and_then(|id| dropped.get(id)) {
                Some(next) => p = next.clone(),
                None => break,
            }
        }
        p
    }
    for msg in &session.messages {
        // Format-complete carrier (a meta/system record carried verbatim under
        // `ParseOptions::complete`): replay it byte-for-byte and skip envelope reconstruction. These
        // are non-conversational, so they don't participate in uuid/parentUuid threading. The one
        // exception to "verbatim": a record's `sessionId`/`cwd` are the session's *identity* — a
        // ported session must carry the NEW ones (Claude keys resume off them), so rewrite them to
        // match the envelope. On a pure round-trip (same id, same cwd) this is byte-identical.
        if let Some(record) = carrier_record_of(msg) {
            let mut record = record.clone();
            if let Some(obj) = record.as_object_mut() {
                if obj.contains_key("sessionId") {
                    obj.insert("sessionId".into(), json!(new_id));
                }
                if obj.contains_key("cwd") {
                    obj.insert("cwd".into(), json!(cwd_str));
                }
            }
            lines.push(record);
            continue;
        }
        // Cross-harness kind-aware records: a compaction pair, or injected context, carried into
        // Claude's own record shapes (a Claude→Claude session replays these verbatim as carriers
        // above, so this runs only when the source is another harness). Each threads normally so a
        // child's parentUuid resolves. See `docs/INTERFACE-V2.md` §4.
        if session.harness != Harness::Claude
            && matches!(
                msg.kind,
                MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
            )
        {
            let uuid = msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
            let parent_uuid = match msg.parent_id.clone() {
                Some(pid) => resolve_parent(&dropped, pid),
                None => parent.clone(),
            };
            let ts = msg
                .timestamp
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
                .unwrap_or_else(|| ts0.clone());
            let text = msg.text().unwrap_or_default();
            let rec = match msg.kind {
                MessageKind::CompactionBoundary => json!({
                    "type": "system", "subtype": "compact_boundary", "level": "info",
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": new_id, "cwd": cwd_str,
                    "timestamp": ts, "content": text,
                    "compactMetadata": { "trigger": "manual", "preTokens": 0 },
                }),
                MessageKind::CompactionSummary => json!({
                    "type": "user", "isCompactSummary": true,
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": new_id, "cwd": cwd_str,
                    "timestamp": ts, "message": { "role": "user", "content": text },
                }),
                // InjectedContext → an `attachment` whose `rendered[].content` is the injected text
                // (exactly what Claude's parser reads back as InjectedContext).
                _ => json!({
                    "type": "attachment",
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": new_id, "cwd": cwd_str,
                    "timestamp": ts,
                    "attachment": { "type": "injected_context" },
                    "rendered": [{ "content": text }],
                }),
            };
            lines.push(rec);
            parent = Some(uuid);
            continue;
        }
        // Claude transcripts have no standalone system turn; skip to avoid polluting user text. (A
        // *carrier* System message was already handled above; this is a synthetic/rendered one —
        // under `ParseOptions::full`, `system` records parse into these.) Its uuid leaves the file,
        // so remember where its children should re-link to.
        if msg.role == Role::System {
            if let Some(id) = &msg.id {
                let p = match msg.parent_id.clone() {
                    Some(pid) => resolve_parent(&dropped, pid),
                    None => parent.clone(),
                };
                dropped.insert(id.clone(), p);
            }
            continue;
        }
        // Preserve the source uuid/parentUuid/timestamp when the message carries them (a
        // format-complete round-trip), else generate fresh threading (cross-harness convert/port).
        // A source parentUuid pointing at a dropped system record re-links to its survivor.
        let uuid = msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
        let parent_uuid = match msg.parent_id.clone() {
            Some(pid) => resolve_parent(&dropped, pid),
            None => parent.clone(),
        };
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts0.clone());

        // Common envelope fields present on every threaded line.
        let mut line = Map::new();
        line.insert("uuid".into(), json!(uuid));
        line.insert("parentUuid".into(), json!(parent_uuid));
        line.insert("sessionId".into(), json!(new_id));
        line.insert("cwd".into(), json!(cwd_str));
        line.insert("timestamp".into(), json!(ts));
        if let Some(b) = &branch {
            line.insert("gitBranch".into(), json!(b));
        }

        match msg.role {
            Role::User => {
                line.insert("type".into(), json!("user"));
                // Text-only user turn → Claude's native bare-string content. Anything richer
                // (images, document/File attachments, mixed content) → the array-of-blocks form
                // Claude also writes; flattening to `msg.text()` dropped every non-text block.
                let content = if msg.content.iter().all(|b| matches!(b, Block::Text { .. })) {
                    json!(msg.text().unwrap_or_default())
                } else {
                    Value::Array(claude_user_blocks(&msg.content))
                };
                line.insert("message".into(), json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                line.insert("type".into(), json!("assistant"));
                let blocks = claude_assistant_blocks(&msg.content);
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    message.insert("model".into(), json!(model));
                }
                // Reconstruct the API `usage` block from the first-classed [`Usage`] (the parser
                // pulls it out of `message.usage`), so a complete round-trip keeps the token counts.
                // Same-harness only, so cross-harness convert output is unchanged.
                if session.harness == Harness::Claude {
                    if let Some(usage) = claude_usage(msg.usage.as_ref()) {
                        message.insert("usage".into(), usage);
                    }
                }
                message.insert("content".into(), Value::Array(blocks));
                line.insert("message".into(), Value::Object(message));
            }
            Role::Tool => {
                // Tool results ride on a `user` line whose content is tool_result blocks.
                let blocks = claude_tool_result_blocks(&msg.content);
                line.insert("type".into(), json!("user"));
                line.insert(
                    "message".into(),
                    json!({ "role": "user", "content": Value::Array(blocks) }),
                );
            }
            Role::System => unreachable!("system turns are carried/skipped above"),
        }

        // A tool result's structured `details` → Claude's `toolUseResult` sidecar: the slot Claude
        // itself writes it in and reads it back from. INTERFACE-V2 §4 puts that sidecar on the block,
        // so `details` is the source of truth for EVERY source harness — a Claude→Claude replay and
        // a cross-harness port alike (foreign details survive rather than vanishing). The
        // complete-mode bag copy applied below is only the fallback, for a message whose block-level
        // details were rewritten or dropped between parse and emit.
        if let Some(tur) = claude_tool_use_result(&msg.content) {
            line.insert(crate::harness::claude::TOOL_USE_RESULT_KEY.into(), tur);
        }

        // Format-complete / same-harness port: fold every captured replay field back onto the
        // record — top-level keys directly, `message.<k>` keys inside the `message` object — so the
        // emitted line carries the same key set + values as the source. (`requestId`, `version`,
        // `userType`, `message.id`, `message.stop_reason`, … all ride here; `toolUseResult` came
        // from the block's `details` above and is not overwritten by the bag's copy of it.)
        // These live in the Claude fact bag (`extra["claude"]`, IR-v2 nesting), so read from there;
        // internal sidecar keys (lazy offset stamp, the carrier key) are never emitted. Only for a
        // Claude→Claude session: a foreign source's bag holds *that* harness's fields, which aren't
        // valid Claude record keys, so cross-harness convert keeps its prior lean output.
        if session.harness == Harness::Claude {
            if let Some(bag) = msg.harness_extra(Harness::Claude) {
                apply_claude_extra(&mut line, bag);
            }
        }

        lines.push(Value::Object(line));
        parent = Some(uuid);
    }

    write_jsonl(&file_path, &lines)?;

    let resume = match &cwd {
        Some(c) => format!("claude --resume {new_id}  (run from {})", c.display()),
        None => format!("claude --resume {new_id}"),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}

/// The verbatim meta record a format-complete carrier message stashed (an object under
/// `extra["_record"]`), or `None` for an ordinary message. See
/// [`claude::carrier_record`](crate::harness::claude::CARRIER_KEY).
fn carrier_record_of(m: &crate::ir::Message) -> Option<&Value> {
    m.extra
        .get(crate::harness::claude::CARRIER_KEY)
        .filter(|v| v.is_object())
}

/// Internal `extra` keys the IR adds for its own bookkeeping — never part of the source record, so
/// they must NOT be replayed into an emitted Claude line.
fn is_internal_extra_key(k: &str) -> bool {
    k == crate::harness::claude::CARRIER_KEY || k == crate::offsets::OFFSET_KEY
}

/// Fold a message's captured `extra` back onto an emitted Claude record (format-complete replay): a
/// `message.<k>` key is routed inside the `message` object; any other key is a top-level field. The
/// captured value is the source-of-truth for that key (it IS the original record's value), so it
/// overrides the envelope reconstruction — e.g. the original `gitBranch`/`version` win over the
/// emit-time params. Two exceptions: `sessionId` and `cwd` are the session's *identity*, and the
/// envelope (which carries the possibly-rehomed values from `EmitOptions::new_id`/`new_cwd`) must
/// stay authoritative — replaying the captured ones would stamp a ported session with its OLD
/// id/cwd, breaking `--new-cwd` porting and `claude --resume` (which keys off both). On a pure
/// round-trip the envelope equals the captured values anyway, so nothing changes there. The
/// IR-first-classed keys (uuid/parentUuid/timestamp/type and `message.content`) are never present
/// in `extra` (the parser excludes them), so they're untouched and the [`Message`]'s own fields
/// remain authoritative.
fn apply_claude_extra(line: &mut Map<String, Value>, extra: &Map<String, Value>) {
    use crate::harness::claude::MESSAGE_EXTRA_PREFIX;
    for (k, val) in extra {
        if is_internal_extra_key(k) || k == "sessionId" || k == "cwd" {
            continue;
        }
        // `toolUseResult` was already rebuilt from the block's `details` (the IR's home for it);
        // the bag's verbatim complete-mode copy is only the fallback, so it must not clobber it.
        if k == crate::harness::claude::TOOL_USE_RESULT_KEY && line.contains_key(k) {
            continue;
        }
        if let Some(sub) = k.strip_prefix(MESSAGE_EXTRA_PREFIX) {
            let msg = line
                .entry("message".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(mobj) = msg.as_object_mut() {
                mobj.insert(sub.to_string(), val.clone());
            }
        } else {
            line.insert(k.clone(), val.clone());
        }
    }
}

/// Reconstruct Claude's `message.usage` object from the IR [`Usage`], reversing
/// [`claude::parse_usage`]'s field renames (`cache_read_input_tokens` ↔ `cache_read_tokens`,
/// `cache_creation_input_tokens` ↔ `cache_creation_tokens`). Emits only the fields actually present,
/// so a source that carried just `input_tokens`/`output_tokens` round-trips with the same key set.
/// `None` when there's no usage at all (an assistant turn the source had no `usage` block on).
fn claude_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    if let Some(x) = u.input_tokens {
        m.insert("input_tokens".into(), json!(x));
    }
    if let Some(x) = u.output_tokens {
        m.insert("output_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_read_tokens {
        m.insert("cache_read_input_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_creation_tokens {
        m.insert("cache_creation_input_tokens".into(), json!(x));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

fn claude_assistant_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("signature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input, .. } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            // Tool results don't belong on an assistant line; skip (emitted on the Tool turn).
            Block::ToolResult { .. } => {}
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::Image { media_type, data_ref } => out.push(claude_image_block(media_type, data_ref)),
        }
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

/// Reconstruct an Anthropic `image` block's `source` from our `data_ref` so the reference survives
/// a round-trip (the parser keeps a ref, not the bytes — dropping it would lose even the
/// `file:`/url pointer). Mirrors the three shapes claude.rs parses.
fn claude_image_block(media_type: &Option<String>, data_ref: &Option<String>) -> Value {
    let mut source = serde_json::Map::new();
    if let Some(mt) = media_type {
        source.insert("media_type".into(), json!(mt));
    }
    match data_ref.as_deref() {
        Some(r) if r.starts_with("file:") => {
            source.insert("type".into(), json!("file"));
            source.insert("file_id".into(), json!(&r["file:".len()..]));
        }
        Some("base64:inline") => {
            // Bytes were intentionally not retained in the IR; mark the kind.
            source.insert("type".into(), json!("base64"));
        }
        Some(r) => {
            source.insert("type".into(), json!("url"));
            source.insert("url".into(), json!(r));
        }
        None => {}
    }
    json!({ "type": "image", "source": Value::Object(source) })
}

/// Map user-turn IR blocks → Claude's array-of-blocks user content. Text/Image round-trip
/// natively; a [`Block::File`] becomes the `document` attachment shape the parser reads back into
/// a File; a stray ToolResult (mixed user content) is kept as a `tool_result` block. Thinking/
/// ToolUse don't occur on user turns, but map to their native shapes rather than being dropped.
fn claude_user_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data_ref } => out.push(claude_image_block(media_type, data_ref)),
            Block::File { mime, path, source } => {
                // Inverse of claude.rs's `document` parsing: title ← path, source ← url/file_id.
                let mut src = serde_json::Map::new();
                if let Some(mt) = mime {
                    src.insert("media_type".into(), json!(mt));
                }
                match source.as_deref() {
                    Some(r) if r.starts_with("file:") => {
                        src.insert("type".into(), json!("file"));
                        src.insert("file_id".into(), json!(&r["file:".len()..]));
                    }
                    Some(r) => {
                        src.insert("type".into(), json!("url"));
                        src.insert("url".into(), json!(r));
                    }
                    None => {}
                }
                let mut doc = Map::new();
                doc.insert("type".into(), json!("document"));
                if let Some(p) = path {
                    doc.insert("title".into(), json!(p));
                }
                doc.insert("source".into(), Value::Object(src));
                out.push(Value::Object(doc));
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })),
            Block::Thinking { .. } | Block::ToolUse { .. } => {
                out.extend(claude_assistant_blocks(std::slice::from_ref(b)));
            }
        }
    }
    out
}

/// A record's `toolUseResult` sidecar, rebuilt from the message's tool-result `details`
/// (INTERFACE-V2 §4: a tool result's structured extras live on the block, so `details` is where
/// both a Claude replay and a cross-harness port read them from).
///
/// Two adjustments reverse what `claude::attach_tool_use_result` did on the way in: a sidecar that
/// was not an object is parked under `details["toolUseResult"]` and
/// comes back out verbatim, and cv's derived `persistedOutput` pointer is stripped (it is
/// re-derived from the transcript stub on every parse and was never a Claude wire field — replaying
/// it would invent a key the source record never had). `None` when the block carried nothing but
/// that pointer.
fn claude_tool_use_result(content: &[Block]) -> Option<Value> {
    let details = content.iter().find_map(|b| match b {
        Block::ToolResult { details: Some(d), .. } => Some(d),
        _ => None,
    })?;
    let Value::Object(map) = details else {
        return Some(details.clone());
    };
    if let Some(parked) = map.get(crate::harness::claude::TOOL_USE_RESULT_KEY) {
        return Some(parked.clone());
    }
    let mut out = map.clone();
    out.remove(crate::harness::claude::PERSISTED_OUTPUT_KEY);
    (!out.is_empty()).then(|| Value::Object(out))
}

fn claude_tool_result_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } = b
        {
            out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Codex
// ------------------------------------------------------------------------------------------------

fn emit_codex(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    // `session_meta.cwd` (and `turn_context.cwd`) are REQUIRED by Codex's decoder (`SessionMeta.cwd:
    // PathBuf`, no default): a meta line without one is undecodable, the thread id can't be read,
    // and resume fails with "failed to parse thread ID from rollout file". A session with no known
    // cwd (some harnesses never record one) lands in the home dir rather than nowhere.
    let cwd = effective_cwd(session, opts)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd_str = cwd.to_string_lossy().into_owned();

    let now = session.created_at.unwrap_or_else(Utc::now);
    // Path: out_dir/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl — in LOCAL time and with no
    // zone suffix, exactly as `RolloutFileName::render` writes it (`OffsetDateTime::now_local()`):
    // Codex's `RolloutFileName::parse` requires a 19-char timestamp with byte 19 == `-` before the
    // uuid, so a `…T11-04-49Z-<uuid>` name (what we used to write) is invisible to every
    // filename-based lookup (thread-id resolution, migration, `builder_from_items`) and the thread
    // only resumed while its sqlite row happened to exist.
    let local = now.with_timezone(&chrono::Local);
    let date_dir = out_dir
        .join(local.format("%Y").to_string())
        .join(local.format("%m").to_string())
        .join(local.format("%d").to_string());
    fs::create_dir_all(&date_dir).with_context(|| format!("creating {}", date_dir.display()))?;
    let stamp = local.format("%Y-%m-%dT%H-%M-%S").to_string();
    let file_path = date_dir.join(format!("rollout-{stamp}-{new_id}.jsonl"));

    let ts_str = now.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();

    // session_meta first line.
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    // `session_id` is the root-thread id (Codex backfills it from `id` on read; explicit is clearer).
    meta.insert("session_id".into(), json!(new_id));
    meta.insert("timestamp".into(), json!(ts_str));
    meta.insert("cwd".into(), json!(cwd_str));
    meta.insert("source".into(), json!("cli"));
    meta.insert("thread_source".into(), json!("user"));
    // We write the pre-paginated layout (`response_item` message + `event_msg` user/agent twins);
    // say so — Codex ≥ 0.147 keys its persistence policy and ordinal handling off `history_mode`,
    // defaulting to `legacy` when absent, and the legacy→paginated migration canonicalizes it.
    meta.insert("history_mode".into(), json!("legacy"));
    meta.insert("originator".into(), json!("clustervision"));
    meta.insert("cli_version".into(), json!(env!("CARGO_PKG_VERSION")));
    // Codex persists this field into `state_5.sqlite.threads.model_provider` when it first
    // discovers the rollout. Omitting it creates an empty provider entry which the TUI then
    // cannot resume (`Model provider `` not found`), even though `turn_context.model` is valid.
    // The Codex target uses its standard OpenAI provider; the model id remains independently
    // carried by `turn_context` below.
    meta.insert("model_provider".into(), json!("openai"));
    // Session-level system prompt → Codex's own `base_instructions {text}` slot (the parser reads it
    // back into `Session::system_prompt`), so a ported session keeps the instructions it ran under.
    if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
        meta.insert("base_instructions".into(), json!({ "text": sp }));
    }
    if let Some(g) = &session.git {
        meta.insert("git".into(), codex_git(g));
    }
    lines.push(json!({
        "type": "session_meta",
        "timestamp": ts_str,
        "payload": Value::Object(meta),
    }));

    // The model rides in a `turn_context` record (per-turn config: model/cwd/approval/sandbox) —
    // that's the ONLY place Codex's own parser (and ours, `apply_turn_context`) reads a model id
    // from. (`session_meta.model_provider` is the *provider* name, not a model id; stuffing the
    // model there was silently dropped on re-parse.)
    // `TurnContextItem` requires `cwd`, `approval_policy`, `sandbox_policy`, `model` and `summary`
    // (no serde defaults); a `{cwd, model}` record fails `decode_rollout_line` and is silently
    // skipped on resume, so the model never actually came back. `on-request` + `workspace-write`
    // are Codex's own defaults (both decode with no further fields).
    if let Some(model) = &session.model {
        lines.push(json!({
            "type": "turn_context",
            "timestamp": ts_str,
            "payload": {
                "turn_id": Uuid::now_v7().to_string(),
                "cwd": cwd_str,
                "approval_policy": "on-request",
                "sandbox_policy": { "type": "workspace-write" },
                "model": model,
                "summary": "auto",
            },
        }));
    }

    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts_str.clone());

        // A compaction pair → Codex's own top-level `compacted` record (the parser reads it back as
        // a CompactionBoundary, plus a CompactionSummary when `message` is non-empty). The boundary
        // carries no text; the summary carries the seed text — both become `compacted` records so the
        // boundary survives even without a summary.
        if matches!(
            msg.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary
        ) {
            let text = if msg.kind == MessageKind::CompactionSummary {
                msg.text().unwrap_or_default()
            } else {
                String::new()
            };
            lines.push(json!({
                "type": "compacted",
                "timestamp": ts,
                "payload": { "message": text },
            }));
            continue;
        }

        match msg.role {
            Role::System => {
                let text = msg.text().unwrap_or_default();
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
            }
            Role::User => {
                let text = msg.text().unwrap_or_default();
                // The `response_item` message is what Codex feeds back into the model on resume;
                // the `event_msg` is only the UI transcript event. Emit BOTH (matching real
                // rollouts) — without the response_item the resumed model has no memory of the
                // user's turns. User content uses `input_text`.
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
                lines.push(codex_event_msg(&ts, json!({ "type": "user_message", "message": text })));
            }
            Role::Assistant => {
                let first_record = lines.len();
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match b {
                        Block::Text { text } => {
                            // Same as user turns: the response_item carries the assistant text
                            // into the resumed model context; the event_msg is UI-only. Assistant
                            // content uses `output_text`.
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": text }],
                                }),
                            ));
                            lines.push(codex_event_msg(
                                &ts,
                                json!({ "type": "agent_message", "message": text }),
                            ));
                        }
                        Block::Thinking { text, encrypted, .. } => {
                            let mut p = Map::new();
                            p.insert("type".into(), json!("reasoning"));
                            // `reasoning.content` is an output-only/raw-reasoning surface for the
                            // current Codex Responses wire. Replaying even one `reasoning_text`
                            // entry makes the next request fail with `array_above_max_length`
                            // (the input schema accepts zero raw content entries). Preserve the
                            // useful text through the replayable summary and the opaque state
                            // through `encrypted_content`, matching native Codex rollouts.
                            // The distinct user-visible summary a Codex source recorded, when it
                            // had one (IR v2: harness facts live in the harness bag, never flat).
                            let summary = msg
                                .harness_extra(Harness::Codex)
                                .and_then(|b| b.get("reasoning_summary"))
                                .and_then(Value::as_str)
                                .unwrap_or(text);
                            let summary_items = if summary.is_empty() {
                                Vec::new()
                            } else {
                                vec![json!({ "type": "summary_text", "text": summary })]
                            };
                            p.insert("summary".into(), Value::Array(summary_items));
                            if let Some(enc) = encrypted {
                                p.insert("encrypted_content".into(), json!(enc));
                            }
                            lines.push(codex_response_item(&ts, Value::Object(p)));
                        }
                        Block::ToolUse { id, name, input, .. } => {
                            let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args,
                                }),
                            ));
                        }
                        Block::File { path, source, .. } => {
                            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                            lines.push(codex_event_msg(
                                &ts,
                                json!({
                                    "type": "agent_message",
                                    "message": format!("[file: {label}]"),
                                }),
                            ));
                        }
                        Block::ToolResult { .. } | Block::Image { .. } => {}
                    }
                }
                // Per-turn token usage → Codex's own `token_count` event, the ONLY record its parser
                // (and ours, `codex::apply_token_count`) reads usage from: it attaches
                // `info.last_token_usage` to the assistant message the event trails. Without it a
                // ported session read back `usage: 393 → 0`. Only when the turn actually wrote
                // records Codex reads back: trailing nothing (or only an empty `reasoning` item,
                // which its reader drops) the event would synthesize a bare usage-only turn.
                if lines.len() > first_record && !unrepresentable(Harness::Codex, msg) {
                    if let Some(info) = codex_token_usage(msg.usage.as_ref()) {
                        lines.push(codex_event_msg(
                            &ts,
                            json!({ "type": "token_count", "info": { "last_token_usage": info } }),
                        ));
                    }
                }
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = b
                    {
                        // `FunctionCallOutputPayload` deserializes ONLY a string or a content-item
                        // array (and never serializes a `success` flag) — Codex itself has no
                        // persisted error bit. The object form `{content, success:false}` we used
                        // to write is undecodable: the line was dropped on resume, orphaning its
                        // `function_call`. Keep the error visible in the text instead; cv's own
                        // Codex parser recognizes the `[error] ` prefix and restores `is_error`.
                        let output = if *is_error {
                            json!(format!("[error] {content}"))
                        } else {
                            json!(content)
                        };
                        lines.push(codex_response_item(
                            &ts,
                            json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }),
                        ));
                    }
                }
            }
        }
    }

    write_jsonl(&file_path, &lines)?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("codex resume {new_id}")),
    })
}

/// IR [`Usage`](crate::ir::Usage) → Codex's `last_token_usage` block, reversing
/// [`codex::parse_usage`]'s renames (`cache_read_tokens` → `cached_input_tokens`,
/// `cache_creation_tokens` → `cache_write_input_tokens`, `reasoning_tokens` →
/// `reasoning_output_tokens`). Only the fields actually present are written, so a Codex source
/// round-trips with the same key set. `None` when there is nothing Codex's parser would read back
/// (it requires at least one of input / output / cached-input).
fn codex_token_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    for (key, val) in [
        ("input_tokens", u.input_tokens),
        ("cached_input_tokens", u.cache_read_tokens),
        ("cache_write_input_tokens", u.cache_creation_tokens),
        ("output_tokens", u.output_tokens),
        ("reasoning_output_tokens", u.reasoning_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return None;
    }
    // Codex's own totals are input + output (cached input is a *subset* of `input_tokens`, not an
    // addend — 35507 + 55 = 35562 in a real 0.155 record whose cached count was 34432).
    let total: u64 = u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0);
    m.insert("total_tokens".into(), json!(total));
    Some(Value::Object(m))
}

fn codex_event_msg(ts: &str, payload: Value) -> Value {
    json!({ "type": "event_msg", "timestamp": ts, "payload": payload })
}

fn codex_response_item(ts: &str, payload: Value) -> Value {
    json!({ "type": "response_item", "timestamp": ts, "payload": payload })
}

fn codex_git(g: &GitInfo) -> Value {
    let mut m = Map::new();
    if let Some(b) = &g.branch {
        m.insert("branch".into(), json!(b));
    }
    if let Some(c) = &g.commit {
        m.insert("commit_hash".into(), json!(c));
    }
    if let Some(r) = &g.remote {
        m.insert("repository_url".into(), json!(r));
    }
    Value::Object(m)
}

// ------------------------------------------------------------------------------------------------
// Grok (best-effort)
// ------------------------------------------------------------------------------------------------

/// Percent-encode everything except unreserved chars, so `/` → `%2F` (matching Grok's dir layout).
const GROK_ENCODE: &AsciiSet = &CONTROLS.add(b'/').add(b' ').add(b'.').add(b'%').add(b':').add(b'\\');

fn emit_grok(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc_cwd = utf8_percent_encode(&cwd_str, GROK_ENCODE).to_string();

    let session_dir = out_dir.join(&enc_cwd).join(&new_id);
    fs::create_dir_all(&session_dir).with_context(|| format!("creating {}", session_dir.display()))?;

    let created = session
        .created_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // summary.json
    let mut summary = Map::new();
    summary.insert("info".into(), json!({ "id": new_id, "cwd": cwd_str }));
    summary.insert("created_at".into(), json!(created));
    summary.insert("updated_at".into(), json!(updated));
    summary.insert("last_active_at".into(), json!(updated));
    summary.insert("num_chat_messages".into(), json!(session.messages.len()));
    // Real Grok summaries carry these too; without at least `chat_format_version`
    // the loader rejects the dir with "Session does not exist". (Verified live against
    // grok 0.1.219 — adding the full field set is what makes `grok --resume` discover it.)
    let num_turns = session.messages.iter().filter(|m| matches!(m.role, Role::User)).count();
    summary.insert("num_messages".into(), json!(num_turns));
    summary.insert("next_trace_turn".into(), json!(0));
    summary.insert("chat_format_version".into(), json!(1));
    summary.insert("agent_name".into(), json!("grok-build"));
    if let Some(home) = std::env::var_os("HOME") {
        summary.insert(
            "grok_home".into(),
            json!(format!("{}/.grok", Path::new(&home).to_string_lossy())),
        );
    }
    // Carry the source model verbatim for a Grok→Grok port (it is already a Grok model), and for a
    // cross-harness port only when it *is* a Grok model; otherwise default to `grok-build`. A
    // foreign model id (e.g. `gpt-5.5`) would make Grok replay the history against an incompatible
    // backend, so it is dropped there — but a same-harness rehome must round-trip the model exactly.
    let model_id = session
        .model
        .as_deref()
        .filter(|m| session.harness == Harness::Grok || m.to_ascii_lowercase().contains("grok"))
        .unwrap_or("grok-build");
    summary.insert("current_model_id".into(), json!(model_id));
    if let Some(g) = &session.git {
        if let Some(b) = &g.branch {
            summary.insert("head_branch".into(), json!(b));
        }
        if let Some(c) = &g.commit {
            summary.insert("head_commit".into(), json!(c));
        }
        if let Some(r) = &g.remote {
            summary.insert("git_remotes".into(), json!([r]));
        }
    }
    summary.insert("session_summary".into(), json!(session.title.as_deref().unwrap_or("")));
    let summary_path = session_dir.join("summary.json");
    fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    // chat_history.jsonl, plus an updates.jsonl sidecar carrying each tool call's terminal status.
    // chat_history alone cannot express a failed tool result — the Grok parser derives
    // `ToolResult.is_error`/`status` ONLY from `updates.jsonl` `tool_call_update` entries — so
    // without the sidecar every ported failure came back as a success.
    let mut lines: Vec<Value> = Vec::new();
    let mut updates: Vec<Value> = Vec::new();
    for msg in &session.messages {
        match msg.role {
            Role::System => {
                let text = grok_concat_text(msg);
                lines.push(json!({ "type": "system", "content": text }));
            }
            Role::User => {
                let text = grok_concat_text(msg);
                lines.push(json!({
                    "type": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            Role::Assistant => {
                let mut line = Map::new();
                line.insert("type".into(), json!("assistant"));
                line.insert("content".into(), json!(grok_concat_text(msg)));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    line.insert("model_id".into(), json!(model));
                }
                for b in &msg.content {
                    if let Block::Thinking { text, encrypted, .. } = b {
                        let mut r = Map::new();
                        r.insert("text".into(), json!(text));
                        // `encrypted` reasoning blobs are provider/account-bound. Carrying one
                        // from a foreign harness (e.g. OpenAI's `encrypted_content`) makes Grok's
                        // backend fail with "Could not decrypt the provided encrypted_content".
                        // Only preserve it for a Grok→Grok port.
                        if let Some(enc) = encrypted {
                            if session.harness == Harness::Grok {
                                r.insert("encrypted".into(), json!(enc));
                            }
                        }
                        line.insert("reasoning".into(), Value::Object(r));
                        break;
                    }
                }
                // Emit assistant tool calls. The Grok parser reads `tool_calls[]` with
                // `arguments` as a JSON-encoded *string*, so re-encode the structured input.
                let calls: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input, .. } => Some(json!({
                            "id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    line.insert("tool_calls".into(), Value::Array(calls));
                }
                lines.push(Value::Object(line));
            }
            // Tool turns map to `tool_result` lines (the parser turns these back into Role::Tool).
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        status,
                        ..
                    } = b
                    {
                        lines.push(json!({
                            "type": "tool_result",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                        // ACP-style status entry: the source status verbatim when it has one,
                        // else derived from `is_error` (Grok's own terminal states).
                        let status = status
                            .clone()
                            .unwrap_or_else(|| if *is_error { "failed" } else { "completed" }.into());
                        updates.push(json!({
                            "timestamp": epoch_ms(msg.timestamp.or(session.updated_at)),
                            "method": "session/update",
                            "params": {
                                "sessionId": new_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": tool_use_id,
                                    "status": status,
                                },
                            },
                        }));
                    }
                }
            }
        }
    }
    let chat_path = session_dir.join("chat_history.jsonl");
    write_jsonl(&chat_path, &lines)?;
    if !updates.is_empty() {
        write_jsonl(&session_dir.join("updates.jsonl"), &updates)?;
    }

    Ok(EmitResult {
        path: session_dir,
        new_id: new_id.clone(),
        resume_hint: Some(format!("grok --resume {new_id}")),
    })
}

/// Concatenate a message's text blocks (Grok chat_history carries plain text only).
fn grok_concat_text(msg: &crate::ir::Message) -> String {
    let mut s = String::new();
    for b in &msg.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

// ------------------------------------------------------------------------------------------------
// OpenCode (SQLite `opencode.db`)
// ------------------------------------------------------------------------------------------------

/// Epoch-millis for an optional timestamp, falling back to `now`.
fn epoch_ms(t: Option<DateTime<Utc>>) -> i64 {
    t.unwrap_or_else(Utc::now).timestamp_millis()
}

/// Where the `opencode.db` should live for a given `--out`/storage-root `out_dir`: the db file
/// itself if pointed at one, the data-dir sibling of a `storage/` root (the real install layout —
/// `~/.local/share/opencode/opencode.db` next to `storage/`), else `<out_dir>/opencode.db`.
fn opencode_db_path(out_dir: &Path) -> PathBuf {
    if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else if out_dir.file_name().is_some_and(|n| n == "storage") {
        out_dir
            .parent()
            .map(|p| p.join("opencode.db"))
            .unwrap_or_else(|| out_dir.join("opencode.db"))
    } else {
        out_dir.join("opencode.db")
    }
}

/// Emit into OpenCode's canonical SQLite store `opencode.db` (`session`/`message`/`part` tables;
/// schema per `~/pug/opencode/packages/core/src/session/sql.ts`). The dead JSON tree under
/// `storage/` is no longer read by OpenCode 1.18, so a session emitted there was unresumable — we
/// write the db instead. Each `data` column holds the Info/Part JSON *minus* `id`/`sessionID`/
/// `messageID` (the reader re-hydrates them). Tables are created when the db is absent; a `--out`
/// dir gets a fresh db. Tool results are folded onto the originating assistant's `tool` part
/// (`state.output`/`state.error`), since OpenCode has no standalone tool turn.
fn emit_opencode(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (session, out_dir, opts);
        anyhow::bail!("emit to opencode requires the `sqlite` feature (opencode.db is SQLite)");
    }
    #[cfg(feature = "sqlite")]
    {
        emit_opencode_db(session, out_dir, opts)
    }
}

#[cfg(feature = "sqlite")]
fn emit_opencode_db(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::{params, Connection};

    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| format!("ses_{}", Uuid::now_v7().simple()));
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_id = if cwd_str.is_empty() {
        "prj_global".to_string()
    } else {
        format!("prj_{}", short_hash(&cwd_str))
    };

    let db_path = opencode_db_path(out_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    // FK enforcement stays off (rusqlite's default) so a fresh db needs no `project` row and an
    // append into a real store isn't rejected for the synthetic project_id. Create the reader's
    // schema when absent (`IF NOT EXISTS` no-ops against a real store; our INSERTs name only columns
    // both schemas share, so any extra real columns keep their defaults).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            model TEXT,
            cost REAL NOT NULL DEFAULT 0,
            tokens_input INTEGER NOT NULL DEFAULT 0,
            tokens_output INTEGER NOT NULL DEFAULT 0,
            tokens_reasoning INTEGER NOT NULL DEFAULT 0,
            tokens_cache_read INTEGER NOT NULL DEFAULT 0,
            tokens_cache_write INTEGER NOT NULL DEFAULT 0,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );",
    )
    .context("creating opencode schema")?;

    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));

    // Session row. `model` is `{id, providerID}` (the reader reads `model.id`); `title` is NOT NULL.
    let model_json = session
        .model
        .as_deref()
        .map(|m| json!({ "id": m, "providerID": "clustervision" }).to_string());
    conn.execute(
        "INSERT OR REPLACE INTO session \
         (id, project_id, slug, directory, title, version, model, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new_id,
            project_id,
            short_hash(&new_id),
            cwd_str,
            session.title.clone().unwrap_or_default(),
            env!("CARGO_PKG_VERSION"),
            model_json,
            created,
            updated,
        ],
    )
    .context("inserting opencode session")?;

    // Pre-index tool results by tool_use_id so each folds onto its assistant's `tool` part.
    let mut tool_results: std::collections::HashMap<String, &Block> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult { tool_use_id, .. } = b {
                    tool_results.insert(tool_use_id.clone(), b);
                }
            }
        }
    }

    let base = created;
    let mut seq: u64 = 0;
    for msg in &session.messages {
        // Tool turns are folded into the assistant's tool parts; no standalone message.
        if msg.role == Role::Tool {
            continue;
        }
        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::System => "system",
            _ => "user",
        };
        // Monotonic id + time so the reader's `ORDER BY time_created, id` == emission order.
        let mid = format!("msg_{seq:08}_{}", Uuid::now_v7().simple());
        let mts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(base + seq as i64);

        let mut mdata = Map::new();
        mdata.insert("role".into(), json!(role));
        mdata.insert("time".into(), json!({ "created": mts }));
        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
            mdata.insert("modelID".into(), json!(model));
        }
        // Usage → `tokens` + `cost` (the reader's `parse_usage` reads both off the message record).
        if let Some(u) = &msg.usage {
            let mut tokens = Map::new();
            tokens.insert("input".into(), json!(u.input_tokens.unwrap_or(0)));
            tokens.insert("output".into(), json!(u.output_tokens.unwrap_or(0)));
            tokens.insert("reasoning".into(), json!(u.reasoning_tokens.unwrap_or(0)));
            tokens.insert(
                "cache".into(),
                json!({ "read": u.cache_read_tokens.unwrap_or(0), "write": u.cache_creation_tokens.unwrap_or(0) }),
            );
            mdata.insert("tokens".into(), Value::Object(tokens));
            if let Some(c) = u.cost_usd {
                mdata.insert("cost".into(), json!(c));
            }
        }
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![mid, new_id, mts, mts, Value::Object(mdata).to_string()],
        )
        .context("inserting opencode message")?;

        let mut pidx: u64 = 0;
        let mut write_part = |part: Value| -> Result<()> {
            let pid = format!("prt_{seq:08}_{pidx:04}_{}", Uuid::now_v7().simple());
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![pid, mid, new_id, mts, mts, part.to_string()],
            )
            .context("inserting opencode part")?;
            pidx += 1;
            Ok(())
        };

        for b in &msg.content {
            match b {
                Block::Text { text } => write_part(json!({ "type": "text", "text": text }))?,
                Block::Thinking { text, signature, .. } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("reasoning"));
                    p.insert("text".into(), json!(text));
                    if let Some(sig) = signature {
                        p.insert("metadata".into(), json!({ "anthropic": { "signature": sig } }));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolUse { id, name, input, .. } => {
                    let mut state = Map::new();
                    state.insert("input".into(), input.clone());
                    if let Some(Block::ToolResult {
                        content,
                        is_error,
                        details,
                        ..
                    }) = tool_results.get(id).copied()
                    {
                        if *is_error {
                            state.insert("status".into(), json!("error"));
                            state.insert("error".into(), json!(content));
                        } else {
                            state.insert("status".into(), json!("completed"));
                            state.insert("output".into(), json!(content));
                        }
                        // `details` was read from `state.{title,metadata,time}`; fold it back so an
                        // OpenCode→OpenCode round-trip reproduces the result's structured details.
                        // A FOREIGN details (Claude's `toolUseResult`, Kimi's note/truncation) has
                        // none of those three keys, and dropping it read back as
                        // `tool result details: 194 → 0`. Its keys ride in `metadata`, OpenCode's own
                        // free-form slot for a tool's structured extras — which is exactly where the
                        // adapter reads them back from.
                        if let Some(d) = details {
                            let mut meta = d
                                .get("metadata")
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            match d {
                                Value::Object(d) => {
                                    for (k, v) in d {
                                        match k.as_str() {
                                            "title" | "time" => {
                                                state.insert(k.clone(), v.clone());
                                            }
                                            "metadata" if v.is_object() => {}
                                            _ => {
                                                meta.insert(k.clone(), v.clone());
                                            }
                                        }
                                    }
                                }
                                // A scalar sidecar (no key to hang it on) still belongs in metadata.
                                other => {
                                    meta.insert("details".into(), other.clone());
                                }
                            }
                            if !meta.is_empty() {
                                state.insert("metadata".into(), Value::Object(meta));
                            }
                        }
                    } else {
                        state.insert("status".into(), json!("completed"));
                    }
                    write_part(json!({
                        "type": "tool",
                        "callID": id,
                        "tool": name,
                        "state": Value::Object(state),
                    }))?;
                }
                Block::Image { media_type, data_ref } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = media_type {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(d) = data_ref {
                        p.insert("url".into(), json!(d));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::File { mime, path, source } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = mime {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(url) = source.as_deref().or(path.as_deref()) {
                        p.insert("url".into(), json!(url));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolResult { .. } => {}
            }
        }
        seq += 1;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("opencode --session {new_id}")),
    })
}

/// A short, stable, filesystem-safe hash of a string (FNV-1a, hex). Used for project-dir names.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ------------------------------------------------------------------------------------------------
// OpenClaw
// ------------------------------------------------------------------------------------------------

/// Emit into OpenClaw's `agents/<agentId>/sessions/<sid>.jsonl` transcript (v3 parent-linked) plus
/// a `sessions.json` index entry. Mirrors `harness/openclaw.rs`'s reader: a `{type:session,…}`
/// header line followed by `{type:message,id,parentId,timestamp,message:{role,content,…}}` lines.
fn emit_openclaw(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_id = "main";
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // `out_dir` is already the agents dir: storage_root() returns `~/.openclaw/agents` and discover()
    // walks it for `<agentId>/sessions/<id>.jsonl`. Re-joining "agents" here produced
    // `~/.openclaw/agents/agents/main/sessions/…`, which openclaw never scans (the session was written
    // but undiscoverable). If a caller instead points `--out` at the state dir (`~/.openclaw`), accept
    // that too by appending the missing `agents` segment.
    let agents_root = if out_dir.file_name().is_some_and(|n| n == "agents") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("agents")
    };
    let sessions_dir = agents_root.join(agent_id).join("sessions");
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));

    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();
    // Header line.
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(3));
    header.insert("id".into(), json!(new_id));
    header.insert("timestamp".into(), json!(created_iso));
    if !cwd_str.is_empty() {
        header.insert("cwd".into(), json!(cwd_str));
    }
    lines.push(Value::Object(header));

    // A `session_info` entry carries the session title (OpenClaw's `name`), which the reader reads
    // back as `Session::title` — the `sessions.json`/`session_nodes` index label alone is only a
    // discovery hint, not seen by a direct `parse`. Emit it so the title round-trips.
    if let Some(title) = session.title.as_deref().filter(|t| !t.trim().is_empty()) {
        lines.push(json!({
            "type": "session_info",
            "id": openclaw_short_id(),
            "parentId": Value::Null,
            "timestamp": created_iso,
            "name": title,
        }));
    }

    // Message lines, parent-linked (v3).
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        let entry_id = openclaw_short_id();
        let ts = msg.timestamp.unwrap_or(created);
        let ts_iso = ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        let ts_ms = ts.timestamp_millis();

        // Kind-aware control entries → OpenClaw's own top-level records (the reader parses them as
        // System notes / lineage markers), so a cross-harness compaction, rewind, or model switch is
        // carried rather than flattened into a plain system turn. They still thread by parentId.
        let control: Option<Value> = match msg.kind {
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary => Some(json!({
                "type": "compaction",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "summary": msg.text().unwrap_or_default(),
            })),
            MessageKind::Branch => Some(json!({
                "type": "reset",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "reason": "reset",
            })),
            MessageKind::ModelChange => Some(json!({
                "type": "model_change",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "provider": Value::Null,
                "modelId": msg.model.clone().or_else(|| session.model.clone()),
            })),
            _ => None,
        };
        if let Some(entry) = control {
            lines.push(entry);
            parent = Some(entry_id);
            continue;
        }

        let inner = match msg.role {
            Role::User => json!({
                "role": "user",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::System => json!({
                "role": "system",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), openclaw_content_blocks(&msg.content));
                m.insert("timestamp".into(), json!(ts_ms));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Assistant-level token usage → OpenClaw's own `message.usage` block, which its
                // reader (`openclaw::parse_usage`) reads straight back into [`Usage`]. Without it a
                // ported session read back `usage: 462 → 0`.
                if let Some(u) = openclaw_usage(msg.usage.as_ref()) {
                    m.insert("usage".into(), u);
                }
                Value::Object(m)
            }
            Role::Tool => {
                // OpenClaw models each tool result as its own `toolResult` message.
                let (tool_use_id, content, is_error, tool_name) = openclaw_tool_result(&msg.content);
                let mut m = Map::new();
                m.insert("role".into(), json!("toolResult"));
                m.insert("toolCallId".into(), json!(tool_use_id));
                if let Some(tn) = tool_name {
                    m.insert("toolName".into(), json!(tn));
                }
                m.insert("content".into(), json!([{ "type": "text", "text": content }]));
                m.insert("isError".into(), json!(is_error));
                m.insert("timestamp".into(), json!(ts_ms));
                Value::Object(m)
            }
        };

        lines.push(json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent,
            "timestamp": ts_iso,
            "message": inner,
        }));
        parent = Some(entry_id);
    }

    write_jsonl(&file_path, &lines)?;

    // sessions.json index: merge-or-create.
    let index_path = sessions_dir.join("sessions.json");
    let mut index: Map<String, Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = session.title.as_ref().or(session.first_user_text().as_ref()) {
        entry.insert("label".into(), json!(crate::ir::truncate(t, 80)));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    index.insert(new_id.clone(), Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Object(index))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        // Headless resume is the `agent` subcommand with `--session-id` + a `--message`, run
        // in-process via `--local` (verified against openclaw's CLI registration; there is no
        // `openclaw --session` flag outside the interactive TUI launcher).
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}

/// 8-hex-char id slice, matching OpenClaw's entry-id convention.
/// IR [`Usage`](crate::ir::Usage) → OpenClaw's `usage{input, output, cacheRead, cacheWrite,
/// totalTokens, cost{total}}`, reversing [`openclaw::parse_usage`]'s names. Only the fields present
/// are written, so an OpenClaw source round-trips with the same key set; `None` when the turn
/// carried no counts and no cost (nothing its reader would accept).
fn openclaw_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    for (key, val) in [
        ("input", u.input_tokens),
        ("output", u.output_tokens),
        ("cacheRead", u.cache_read_tokens),
        ("cacheWrite", u.cache_creation_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if let Some(c) = u.cost_usd {
        m.insert("cost".into(), json!({ "total": c }));
    }
    if m.is_empty() {
        return None;
    }
    m.insert(
        "totalTokens".into(),
        json!(u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0)),
    );
    Some(Value::Object(m))
}

fn openclaw_short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Map IR blocks → an OpenClaw content array (`text`/`thinking`/`toolCall`/`image`).
fn openclaw_content_blocks(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("thinkingSignature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input, .. } => out.push(json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": input,
            })),
            Block::Image { media_type, data_ref } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("image"));
                if let Some(mt) = media_type {
                    m.insert("mimeType".into(), json!(mt));
                }
                if let Some(d) = data_ref {
                    m.insert("data".into(), json!(d));
                }
                out.push(Value::Object(m));
            }
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

/// Pull the (id, content, is_error, name) out of a Tool turn's blocks.
fn openclaw_tool_result(content: &[Block]) -> (String, String, bool, Option<String>) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string(), *is_error, tool_name.clone());
        }
    }
    (String::new(), String::new(), false, None)
}

// ------------------------------------------------------------------------------------------------
// Gemini (legacy ConversationRecord)
// ------------------------------------------------------------------------------------------------

/// Emit a gemini-cli legacy chat recording: `out_dir/<sessionId>.json`, a single whole-file
/// `ConversationRecord` `{sessionId, projectHash, startTime, lastUpdated, messages[]}`. The reader
/// (`harness/gemini.rs`) requires the file to live under a `chats/` dir to be picked up by
/// `discover`, but `parse_all_str` / `parse` will read it directly from any path.
fn emit_gemini(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Gemini's `--resume` / `--list-sessions` only scan `<globalTemp>/<projectIdentifier>/chats/`,
    // where `out_dir` is `<globalTemp>` (~/.gemini/tmp) and `<projectIdentifier>` is the cwd basename
    // registered in `<globalTemp>/../projects.json`. Writing to a bare `chats/` (no identifier dir)
    // makes the session invisible to the installed binary. So register the cwd→slug mapping (matching
    // gemini's getShortId) and write into the slug's chats dir. (Verified live: with the file in the
    // right slug dir, `gemini --resume <uuid>` recalls the conversation.)
    let chats_dir = match gemini_register_project(out_dir, &cwd_str) {
        Some(slug) => out_dir.join(slug).join("chats"),
        None => out_dir.join("chats"), // no cwd → fall back to the legacy bare path
    };
    fs::create_dir_all(&chats_dir).with_context(|| format!("creating {}", chats_dir.display()))?;
    let file_path = chats_dir.join(format!("session-{new_id}.json"));

    let start = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let last = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // Index tool results by tool_use_id so they can be folded into the producing assistant's
    // `toolCalls[].result` (the canonical legacy shape the reader pairs from).
    let mut tool_results: std::collections::HashMap<String, (String, bool)> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    tool_results.insert(tool_use_id.clone(), (content.to_string(), *is_error));
                }
            }
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        let ts = msg.timestamp.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
        match msg.role {
            Role::User | Role::System => {
                let mty = if msg.role == Role::System { "info" } else { "user" };
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!(mty));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                messages.push(Value::Object(m));
            }
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!("gemini"));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Per-message usage → Gemini's `tokens{input, output, cached}` (the parser reads it
                // back into `Usage`), so token counts survive a port into Gemini.
                if let Some(u) = &msg.usage {
                    let mut tokens = Map::new();
                    if let Some(x) = u.input_tokens {
                        tokens.insert("input".into(), json!(x));
                    }
                    if let Some(x) = u.output_tokens {
                        tokens.insert("output".into(), json!(x));
                    }
                    if let Some(x) = u.cache_read_tokens {
                        tokens.insert("cached".into(), json!(x));
                    }
                    if !tokens.is_empty() {
                        m.insert("tokens".into(), Value::Object(tokens));
                    }
                }
                // Thoughts (thinking) ride in `thoughts[]`, the rest in `content`.
                let mut thoughts = Vec::new();
                for b in &msg.content {
                    if let Block::Thinking { text, .. } = b {
                        thoughts.push(json!({ "subject": "", "description": text }));
                    }
                }
                if !thoughts.is_empty() {
                    m.insert("thoughts".into(), Value::Array(thoughts));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                // Tool calls → toolCalls[] (legacy ToolCallRecord), folding in the paired result so
                // the reader emits a proper Tool turn for it.
                let mut calls = Vec::new();
                for b in &msg.content {
                    if let Block::ToolUse { id, name, input, .. } = b {
                        let mut call = Map::new();
                        call.insert("id".into(), json!(id));
                        call.insert("name".into(), json!(name));
                        call.insert("args".into(), input.clone());
                        if let Some((content, is_error)) = tool_results.get(id) {
                            call.insert("status".into(), json!(if *is_error { "error" } else { "success" }));
                            call.insert(
                                "result".into(),
                                json!([{
                                    "functionResponse": {
                                        "id": id,
                                        "name": name,
                                        "response": { "output": content },
                                    }
                                }]),
                            );
                        }
                        calls.push(Value::Object(call));
                    }
                }
                if !calls.is_empty() {
                    m.insert("toolCalls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(m));
            }
            // Tool turns are folded into the assistant's toolCalls[].result above.
            Role::Tool => {}
        }
    }

    let mut record = Map::new();
    record.insert("sessionId".into(), json!(new_id));
    // gemini keys recordings by an opaque projectHash; synthesize a stable one from the cwd.
    // (The resume path doesn't validate this value; discovery is by the slug dir computed above.)
    record.insert("projectHash".into(), json!(short_hash(&cwd_str)));
    record.insert("startTime".into(), json!(start));
    record.insert("lastUpdated".into(), json!(last));
    if let Some(t) = &session.title {
        record.insert("summary".into(), json!(t));
    }
    if let Some(c) = &cwd {
        record.insert("directories".into(), json!([c.to_string_lossy()]));
    }
    record.insert("messages".into(), Value::Array(messages));

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(record))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(match &cwd {
            Some(c) => format!("gemini --resume {new_id}  (run from {})", c.display()),
            None => format!("gemini --resume {new_id}"),
        }),
    })
}

/// Bind `cwd` to a slug dir under gemini's `<globalTemp>` (=`out_dir`), replicating
/// `ProjectRegistry.getShortId`/`claimNewSlug`. Gemini owns a slug dir via a `.project_root` marker
/// file containing the project path; on launch it RE-derives the slug and, if our dir lacks a marker
/// (or the projects.json entry can't be verified), reassigns a fresh slug — so writing only
/// projects.json is not enough. We therefore: pick the cwd basename (suffixing `-2`, `-3`… if a
/// DIFFERENT path already owns that dir's marker), write the `.project_root` marker, and record the
/// projects.json mapping (the fast path; gemini heals from the marker regardless). After this,
/// `gemini --resume`/`--list-sessions` discover the session. Returns the slug, `None` if `cwd` empty.
fn gemini_register_project(global_tmp: &Path, cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    const MARKER: &str = ".project_root";
    // Reuse the slug whose marker already owns this cwd, if any (gemini's findExistingSlugForPath).
    let owned_by = |slug_dir: &Path| -> Option<String> {
        fs::read_to_string(slug_dir.join(MARKER))
            .ok()
            .map(|s| s.trim().to_string())
    };
    if let Ok(entries) = fs::read_dir(global_tmp) {
        for e in entries.flatten() {
            if owned_by(&e.path()).as_deref() == Some(cwd) {
                return e.file_name().to_str().map(str::to_string);
            }
        }
    }
    // Otherwise claim the basename, skipping dirs whose marker owns a different path.
    let base = Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    let mut slug = base.clone();
    let mut n = 2;
    loop {
        let dir = global_tmp.join(&slug);
        match owned_by(&dir) {
            Some(owner) if owner != cwd => {
                slug = format!("{base}-{n}");
                n += 1;
            }
            _ => break, // free, or already ours
        }
    }
    let slug_dir = global_tmp.join(&slug);
    let _ = fs::create_dir_all(&slug_dir);
    let _ = fs::write(slug_dir.join(MARKER), cwd);
    // Record the projects.json fast-path mapping too (best-effort).
    if let Some(parent) = global_tmp.parent() {
        let projects_json = parent.join("projects.json");
        let mut doc: Value = fs::read_to_string(&projects_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let projects = doc
            .as_object_mut()
            .unwrap()
            .entry("projects")
            .or_insert_with(|| json!({}));
        if let Some(m) = projects.as_object_mut() {
            m.insert(cwd.to_string(), json!(slug));
        }
        if let Ok(s) = serde_json::to_string_pretty(&doc) {
            let _ = fs::write(&projects_json, s);
        }
    }
    Some(slug)
}

/// Map IR blocks → a Gemini `Part[]` (`text` / thought `text` / `inlineData`). Tool calls are
/// emitted separately via `toolCalls[]`, so they're skipped here.
fn gemini_parts(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "text": text })),
            Block::Thinking { text, .. } => out.push(json!({ "text": text, "thought": true })),
            Block::Image { media_type, .. } => {
                let mut inline = Map::new();
                if let Some(mt) = media_type {
                    inline.insert("mimeType".into(), json!(mt));
                }
                out.push(json!({ "inlineData": Value::Object(inline) }));
            }
            Block::File { mime, path, source } => {
                let mut fd = Map::new();
                if let Some(mt) = mime {
                    fd.insert("mimeType".into(), json!(mt));
                }
                if let Some(uri) = source.as_deref().or(path.as_deref()) {
                    fd.insert("fileUri".into(), json!(uri));
                }
                out.push(json!({ "fileData": Value::Object(fd) }));
            }
            // Tool use/result handled out of band.
            Block::ToolUse { .. } | Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

// ------------------------------------------------------------------------------------------------
// Hermes (SQLite)
// ------------------------------------------------------------------------------------------------

/// Emit into Hermes's `state.db` SQLite store (creating/appending). Mirrors the schema the Hermes
/// reader (`harness/hermes.rs`) expects: a `sessions` row + OpenAI-shaped `messages` rows
/// (role/content/tool_calls JSON/reasoning/timestamps as REAL unix secs). Multimodal content uses
/// the `\x00json:` sentinel prefix.
#[cfg(feature = "sqlite")]
fn emit_hermes(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::{params, Connection};

    const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    // `out_dir` may be a directory to drop state.db into, OR the state.db file itself — the hermes
    // adapter's storage_root() returns the .db path, so `cv convert --to hermes` (no --out) passes
    // the existing file here. Treating it as a dir made `create_dir_all` fail with "File exists".
    let db_path = if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("state.db")
    };
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    // Create the (subset of) v14 schema if the DB is fresh. `IF NOT EXISTS` makes append safe.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT,
            cwd TEXT,
            git_branch TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
         );",
    )
    .context("creating hermes schema")?;

    // FTS5 virtual tables + sync triggers, mirroring hermes_state.py's FTS_SQL / FTS_TRIGRAM_SQL.
    // Without these, a port into a *fresh* state.db is invisible to Hermes's search (which queries
    // `messages_fts`). The unicode61 table is part of bundled FTS5 and always available; the trigram
    // tokenizer is too in modern SQLite, but we still guard it so emit succeeds on any build that
    // lacks it. Created BEFORE inserting rows so the AFTER INSERT triggers populate the index.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content);
         CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;",
    )
    .context("creating hermes messages_fts (FTS5)")?;

    // Trigram table for CJK/substring search. Guarded: if the trigram tokenizer isn't compiled in,
    // skip it (and its triggers) rather than failing the whole emit. The unicode61 table above is
    // enough for Hermes to find ASCII content; the trigram index is a CJK nicety.
    let trigram_sql = "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(content, tokenize='trigram');
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;";
    if let Err(e) = conn.execute_batch(trigram_sql) {
        eprintln!("cv: hermes trigram FTS unavailable, skipping ({e}); unicode61 FTS still active");
    }

    let started = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .timestamp_millis() as f64
        / 1000.0;
    let ended = session
        .updated_at
        .or(session.created_at)
        .map(|t| t.timestamp_millis() as f64 / 1000.0);

    // Format-complete: pull the captured per-session columns back out of the Hermes fact bag
    // (`extra["hermes"][SESSION_META_KEY]`, IR-v2 nesting) so the round-trip reproduces every
    // session-row value. Anything absent falls back to a sensible default (`source = "clustervision"`).
    use crate::harness::hermes::{RAW_REASONING_CONTENT_KEY, RAW_REASONING_KEY, SESSION_META_KEY};
    let smeta = session
        .harness_extra(Harness::Hermes)
        .and_then(|b| b.get(SESSION_META_KEY))
        .and_then(Value::as_object);
    let smeta_str =
        |k: &str| -> Option<String> { smeta.and_then(|m| m.get(k)).and_then(Value::as_str).map(str::to_string) };
    let smeta_int = |k: &str| -> i64 { smeta.and_then(|m| m.get(k)).and_then(Value::as_i64).unwrap_or(0) };
    let source = smeta_str("source").unwrap_or_else(|| "clustervision".to_string());
    let user_id = smeta_str("user_id");
    let model_config = smeta_str("model_config");
    // System prompt: the captured Hermes column (same-harness replay) else the first-class
    // `Session::system_prompt` (a cross-harness port carries it here into Hermes's own slot).
    let system_prompt = smeta_str("system_prompt").or_else(|| session.system_prompt.clone());
    let end_reason = smeta_str("end_reason");
    // First-class cwd/git → Hermes's own `cwd`/`git_branch` columns (v-recent schema), so a ported
    // session keeps its working dir (the reader probes these columns).
    let cwd = effective_cwd(session, opts).map(|p| p.to_string_lossy().into_owned());
    let git_branch = branch_of(session);

    conn.execute(
        "INSERT OR REPLACE INTO sessions \
         (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
          end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
          cache_read_tokens, cache_write_tokens, reasoning_tokens, title, cwd, git_branch) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            new_id,
            source,
            user_id,
            session.model,
            model_config,
            system_prompt,
            started,
            ended,
            end_reason,
            session.messages.len() as i64,
            smeta_int("tool_call_count"),
            smeta_int("input_tokens"),
            smeta_int("output_tokens"),
            smeta_int("cache_read_tokens"),
            smeta_int("cache_write_tokens"),
            smeta_int("reasoning_tokens"),
            session.title,
            cwd,
            git_branch,
        ],
    )
    .context("inserting hermes session")?;

    let mut base_ts = started;
    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|| {
                base_ts += 0.001;
                base_ts
            });

        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };

        // Reasoning columns. For a lossless round-trip we prefer the verbatim source columns the
        // parser stashed in the Hermes fact bag (`extra["hermes"]`, IR-v2 nesting: `reasoning` /
        // `reasoning_content` / `reasoning_details` / `codex_*`) over reconstructing from the folded
        // `Thinking.text` projection — that projection merges + dedups across columns and so can't
        // recover the originals. We synthesize from Thinking blocks only when no verbatim column was
        // captured (a session built in-memory or ported from another harness).
        let hbag = msg.harness_extra(Harness::Hermes);
        let extra_str =
            |k: &str| -> Option<String> { hbag.and_then(|b| b.get(k)).and_then(Value::as_str).map(str::to_string) };
        let extra_json = |k: &str| -> Option<String> { hbag.and_then(|b| b.get(k)).map(|v| v.to_string()) };

        let mut reasoning = extra_str(RAW_REASONING_KEY);
        let reasoning_content = extra_str(RAW_REASONING_CONTENT_KEY);
        // `reasoning_details` / codex items were stashed as parsed JSON; re-serialize them verbatim.
        let mut reasoning_details = extra_json("reasoning_details");
        let codex_reasoning_items = extra_json("codex_reasoning_items");
        let codex_message_items = extra_json("codex_message_items");

        // Fallback synthesis only when nothing verbatim was captured: a foreign/in-memory session
        // still gets its Thinking text + encrypted blob persisted (matching the prior behaviour).
        if reasoning.is_none() && reasoning_content.is_none() && reasoning_details.is_none() {
            let mut text = String::new();
            let mut enc: Option<&str> = None;
            for b in &msg.content {
                if let Block::Thinking { text: t, encrypted, .. } = b {
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                    if enc.is_none() {
                        enc = encrypted.as_deref();
                    }
                }
            }
            if !text.is_empty() {
                reasoning = Some(text);
            }
            if let Some(enc) = enc {
                reasoning_details =
                    Some(json!([{ "type": "reasoning.encrypted_content", "encrypted_content": enc }]).to_string());
            }
        }

        let finish_reason = extra_str("finish_reason");
        let platform_message_id = extra_str("platform_message_id");
        let observed: i64 = if hbag.and_then(|b| b.get("observed")) == Some(&Value::Bool(true)) {
            1
        } else {
            0
        };
        // token_count: the parser routes a row's single combined count to Usage (output for
        // assistant turns, input otherwise); reverse that mapping to recover the column.
        let token_count: Option<i64> = msg.usage.as_ref().and_then(|u| {
            let v = if msg.role == Role::Assistant {
                u.output_tokens
            } else {
                u.input_tokens
            };
            v.map(|n| n as i64)
        });

        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    ..
                } = b
                {
                    // `tool_name` matters: Hermes's own reader (and our adapter) surfaces it, so
                    // dropping it here made every ported tool turn come back nameless. The optional
                    // per-row columns (token_count / finish_reason / platform_message_id / observed)
                    // can also ride a tool row, so write them back for a lossless round-trip.
                    conn.execute(
                        "INSERT INTO messages \
                         (session_id, role, content, tool_call_id, tool_name, timestamp, \
                          token_count, finish_reason, platform_message_id, observed) \
                         VALUES (?1, 'tool', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            new_id,
                            content.to_string(),
                            tool_use_id,
                            tool_name,
                            ts,
                            token_count,
                            finish_reason,
                            platform_message_id,
                            observed,
                        ],
                    )
                    .context("inserting hermes tool message")?;
                }
            }
            continue;
        }

        // Content: plain text, or the multimodal sentinel form if there are images.
        let has_image = msg.content.iter().any(|b| matches!(b, Block::Image { .. }));
        let content: Option<String> = if has_image {
            let mut parts = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => parts.push(json!({ "type": "text", "text": text })),
                    Block::Image { data_ref, .. } => {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": data_ref.clone().unwrap_or_default() },
                        }));
                    }
                    _ => {}
                }
            }
            Some(format!(
                "{MULTIMODAL_SENTINEL}{}",
                serde_json::to_string(&Value::Array(parts))?
            ))
        } else {
            msg.text()
        };

        // Assistant tool calls → OpenAI-shaped tool_calls JSON.
        let tool_calls: Option<String> = {
            let calls: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input, .. } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                        },
                    })),
                    _ => None,
                })
                .collect();
            (!calls.is_empty()).then(|| Value::Array(calls).to_string())
        };

        conn.execute(
            "INSERT INTO messages \
             (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, \
              reasoning, reasoning_content, reasoning_details, codex_reasoning_items, \
              codex_message_items, platform_message_id, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                new_id,
                role,
                content,
                tool_calls,
                ts,
                token_count,
                finish_reason,
                reasoning,
                reasoning_content,
                reasoning_details,
                codex_reasoning_items,
                codex_message_items,
                platform_message_id,
                observed,
            ],
        )
        .context("inserting hermes message")?;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("hermes --resume {new_id}")),
    })
}

// ------------------------------------------------------------------------------------------------
// shared
// ------------------------------------------------------------------------------------------------

fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut buf = String::new();
    for v in lines {
        buf.push_str(&serde_json::to_string(v)?);
        buf.push('\n');
    }
    fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        claude::Claude, codex::Codex, gemini::Gemini, grok::Grok, openclaw::OpenClaw, opencode::OpenCode, Adapter,
    };
    use crate::ir::*;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cv-emit-{}", Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_session(harness: Harness) -> Session {
        let mut sys = Message::new(Role::System);
        sys.content.push(Block::Text {
            text: "you are a helpful assistant".into(),
        });

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "list the files please".into(),
        });

        let mut asst = Message::new(Role::Assistant);
        asst.model = Some("test-model".into());
        asst.content.push(Block::Thinking {
            text: "I should run ls".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        asst.content.push(Block::Text {
            text: "Sure, listing now.".into(),
        });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "run_shell".into(),
            input: serde_json::json!({ "cmd": "ls" }),
            namespace: None,
        });

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "file_a.txt\nfile_b.txt".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });

        Session {
            id: "orig-id".into(),
            harness,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("a test session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: Some(GitInfo {
                branch: Some("main".into()),
                commit: None,
                remote: None,
            }),
            messages: vec![sys, user, asst, tool],
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        }
    }

    fn texts(session: &Session, role: Role) -> Vec<String> {
        session
            .messages
            .iter()
            .filter(|m| m.role == role)
            .filter_map(|m| m.text())
            .collect()
    }

    fn tool_names(session: &Session) -> Vec<String> {
        let mut out = Vec::new();
        for m in &session.messages {
            for b in &m.content {
                if let Block::ToolUse { name, .. } = b {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    #[test]
    fn claude_round_trip() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // Re-parse via the Claude adapter. id is derived from the filename stem.
        let id = res.path.file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(id, res.new_id);
        let r = SessionRef {
            id,
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();

        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // The Tool turn (tool_result) should survive as a Role::Tool message.
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        // Thinking survives.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // gitBranch round-trips.
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
    }

    #[test]
    fn claude_encode_cwd_format() {
        assert_eq!(
            claude_encode_cwd(Path::new("/Users/test/my.project")),
            "-Users-test-my-project"
        );
    }

    /// Porting to a symlinked cwd must key the project dir off the *realpath*, so `claude --resume`
    /// — which resolves symlinks on the launch dir — discovers the session. Guards the macOS
    /// `/tmp` → `/private/tmp` jail. See `realpath_for_claude`.
    #[test]
    #[cfg(unix)]
    fn claude_project_dir_follows_symlinked_cwd() {
        use std::os::unix::fs::symlink;

        let base = temp_dir();
        let real = base.join("real_project");
        fs::create_dir_all(&real).unwrap();
        let link = base.join("link_project");
        symlink(&real, &link).unwrap();

        // Sanity: the link path encodes differently from its realpath, else the test proves nothing.
        let resolved = fs::canonicalize(&link).unwrap();
        assert_ne!(claude_encode_cwd(&link), claude_encode_cwd(&resolved));

        let out = temp_dir();
        let res = emit(
            &sample_session(Harness::Claude),
            Harness::Claude,
            &out,
            &EmitOptions {
                new_cwd: Some(link.clone()),
                new_id: None,
                ..Default::default()
            },
        )
        .unwrap();

        let proj_dir = res.path.parent().unwrap().file_name().unwrap();
        assert_eq!(proj_dir, claude_encode_cwd(&resolved).as_str());
        assert_ne!(proj_dir, claude_encode_cwd(&link).as_str());
    }

    /// User turns with non-text blocks must emit Claude's array-of-blocks content — the old
    /// `msg.text()` flattening dropped user images and document/File attachments entirely.
    #[test]
    fn claude_round_trip_preserves_user_images_and_files() {
        let mut s = sample_session(Harness::Claude);
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "look at this".into(),
        });
        user.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("https://example.com/x.png".into()),
        });
        user.content.push(Block::File {
            mime: Some("application/pdf".into()),
            path: Some("spec.pdf".into()),
            source: Some("file:file_123".into()),
        });
        s.messages.push(user);

        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();

        let rich = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .find(|m| m.content.len() > 1)
            .expect("mixed-content user turn survives as multiple blocks");
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::Text { text } if text == "look at this")),
            "user text block survives"
        );
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::Image { media_type, data_ref }
                if media_type.as_deref() == Some("image/png")
                    && data_ref.as_deref() == Some("https://example.com/x.png"))),
            "user image (media_type + data_ref) survives: {:?}",
            rich.content
        );
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::File { mime, path, source }
                if mime.as_deref() == Some("application/pdf")
                    && path.as_deref() == Some("spec.pdf")
                    && source.as_deref() == Some("file:file_123"))),
            "user document/File attachment survives: {:?}",
            rich.content
        );
    }

    /// Non-carrier System turns (a `full()`-parse of `system` records) are dropped on emit; their
    /// children must be re-linked to the nearest surviving ancestor, never left pointing at a uuid
    /// that isn't in the file. Covers a dropped root, a dropped mid-chain node, and a dropped
    /// *chain* (two consecutive system records).
    #[test]
    fn claude_emit_relinks_parents_over_dropped_system_records() {
        use crate::harness::claude::parse_str;

        let sid = "22222222-2222-4222-8222-222222222222";
        let src_lines = [
            json!({"type":"system","uuid":"s0","parentUuid":null,"sessionId":sid,"subtype":"info",
                "content":"session started","level":"info","timestamp":"2026-06-16T00:00:00.000Z"}),
            json!({"type":"user","uuid":"u1","parentUuid":"s0","sessionId":sid,"cwd":"/work/proj",
                "timestamp":"2026-06-16T00:00:01.000Z","message":{"role":"user","content":"hello"}}),
            json!({"type":"system","uuid":"s2","parentUuid":"u1","sessionId":sid,"subtype":"local_command",
                "content":"<command-name>/foo</command-name>","timestamp":"2026-06-16T00:00:02.000Z"}),
            json!({"type":"system","uuid":"s3","parentUuid":"s2","sessionId":sid,"subtype":"info",
                "content":"notice two","timestamp":"2026-06-16T00:00:03.000Z"}),
            json!({"type":"assistant","uuid":"a4","parentUuid":"s3","sessionId":sid,
                "timestamp":"2026-06-16T00:00:04.000Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}),
        ];
        let text = src_lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let session = parse_str(sid, &text, None);
        // Sanity: the lean full() parse surfaced the system records as content-bearing System turns.
        assert_eq!(
            session.messages.iter().filter(|m| m.role == Role::System).count(),
            3,
            "full() should parse all three system records into System messages"
        );

        let out = temp_dir();
        let res = emit(&session, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        let emitted: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // No parentUuid may dangle: every non-null parent must be a uuid present in the file.
        let uuids: std::collections::HashSet<&str> = emitted
            .iter()
            .filter_map(|v| v.get("uuid").and_then(Value::as_str))
            .collect();
        for v in &emitted {
            if let Some(p) = v.get("parentUuid").and_then(Value::as_str) {
                assert!(uuids.contains(p), "dangling parentUuid {p} in emitted transcript");
            }
        }
        // And the re-links went to the *nearest surviving ancestor*:
        let parent_of = |uuid: &str| -> Option<String> {
            emitted
                .iter()
                .find(|v| v.get("uuid").and_then(Value::as_str) == Some(uuid))
                .and_then(|v| v.get("parentUuid").and_then(Value::as_str))
                .map(str::to_string)
        };
        // u1 pointed at the dropped root s0 → re-linked to null (no surviving ancestor).
        assert_eq!(parent_of("u1"), None, "child of a dropped root gets a null parent");
        // a4 pointed at s3 → s2 → u1: the dropped chain collapses to u1.
        assert_eq!(
            parent_of("a4").as_deref(),
            Some("u1"),
            "child of a dropped system chain re-links to the surviving ancestor"
        );
    }

    #[test]
    fn codex_error_outputs_are_decodable_strings() {
        // `function_call_output.output` is a string or a content-item array — never an object.
        // The old `{content, success:false}` form was undecodable and dropped on resume.
        let mut s = sample_session(Harness::Codex);
        for m in &mut s.messages {
            for b in &mut m.content {
                if let Block::ToolResult { is_error, .. } = b {
                    *is_error = true;
                }
            }
        }
        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        let outputs: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|item| item["payload"]["type"] == "function_call_output")
            .collect();
        assert_eq!(outputs.len(), 1);
        let output = &outputs[0]["payload"]["output"];
        assert!(output.is_string(), "got {output}");
        assert!(output.as_str().unwrap().starts_with("[error] "));
    }

    #[test]
    fn codex_round_trip() {
        let s = sample_session(Harness::Codex);
        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        let first_line = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .next()
            .map(str::to_owned)
            .unwrap();
        let session_meta: Value = serde_json::from_str(&first_line).unwrap();
        assert_eq!(
            session_meta["payload"]["model_provider"], "openai",
            "emitted Codex sessions must be resumable before their model turn-context is read"
        );
        // Codex's decoder rules (rollout_file_name.rs / protocol.rs / models.rs at 132c2be23):
        // file name `rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl`, local time, no zone suffix.
        let name = res.path.file_name().unwrap().to_string_lossy().into_owned();
        let stamp = name.strip_prefix("rollout-").unwrap();
        assert_eq!(stamp.as_bytes()[19], b'-', "byte 19 must be `-`: {name}");
        assert!(
            stamp[..19].chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'T'),
            "{name}"
        );
        assert!(stamp[20..].starts_with(&res.new_id), "{name}");
        // session_meta: `cwd` is required; history_mode says which layout we wrote.
        assert_eq!(session_meta["payload"]["cwd"], "/Users/test/project");
        assert_eq!(session_meta["payload"]["history_mode"], "legacy");
        assert_eq!(session_meta["payload"]["session_id"], res.new_id);
        // turn_context: every required field present, else the line is skipped and the model lost.
        let tc = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|item| item["type"] == "turn_context")
            .expect("turn_context emitted");
        for key in [
            "turn_id",
            "cwd",
            "approval_policy",
            "sandbox_policy",
            "model",
            "summary",
        ] {
            assert!(
                tc["payload"].get(key).is_some(),
                "turn_context.{key} is required by Codex"
            );
        }
        assert_eq!(tc["payload"]["sandbox_policy"]["type"], "workspace-write");
        assert_eq!(tc["payload"]["approval_policy"], "on-request");
        let emitted_reasoning = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|item| item["type"] == "response_item" && item["payload"]["type"] == "reasoning")
            .unwrap();
        assert!(
            emitted_reasoning["payload"].get("content").is_none(),
            "raw reasoning content is not accepted when a Codex rollout is replayed"
        );
        assert_eq!(emitted_reasoning["payload"]["summary"].as_array().unwrap().len(), 1);
        // The model must survive: it's carried by a `turn_context` record (the only place the
        // codex parser reads a model id from; `session_meta.model_provider` is not a model).
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        // event_msg user/assistant text survives.
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // function_call_output round-trips as a Tool message (a success: is_error false).
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, is_error, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
            assert!(!is_error);
        } else {
            panic!("expected tool result");
        }
        // reasoning survives.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // git branch round-trips.
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
        // System turn (developer) survives.
        assert!(parsed.messages.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn codex_round_trip_preserves_tool_failure() {
        // Codex persists no error bit on `function_call_output` (string or content-item array only),
        // so a failed result is emitted as the string "[error] <content>" and cv's Codex parser
        // restores `is_error` from that prefix — the object form ({content, success:false}) we
        // used to write is undecodable by Codex and was dropped on resume.
        let mut s = sample_session(Harness::Codex);
        let mut failed = Message::new(Role::Tool);
        failed.content.push(Block::ToolResult {
            tool_use_id: "call_2".into(),
            content: "command not found: frobnicate".into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        });
        s.messages.push(failed);

        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();
        let mut results: Vec<(String, String, bool)> = Vec::new();
        for m in parsed.messages.iter().filter(|m| m.role == Role::Tool) {
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    results.push((tool_use_id.clone(), content.to_string(), *is_error));
                }
            }
        }
        assert_eq!(
            results,
            vec![
                ("call_1".to_string(), "file_a.txt\nfile_b.txt".to_string(), false),
                ("call_2".to_string(), "command not found: frobnicate".to_string(), true),
            ],
            "is_error must survive a port into codex"
        );
    }

    #[test]
    fn grok_round_trip() {
        let s = sample_session(Harness::Grok);
        let out = temp_dir();
        let res = emit(&s, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.is_dir());
        assert!(res.path.join("summary.json").exists());
        assert!(res.path.join("chat_history.jsonl").exists());

        // r.path is the session directory for Grok.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Grok,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Grok::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(parsed.git.clone().and_then(|g| g.branch), Some("main".to_string()));
        // Thinking/reasoning survives on the assistant turn.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // Tool calls now SURVIVE (previously emit dropped all tool turns).
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // And the assistant tool call's arguments round-trip.
        let tool_use_input = parsed.messages.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                Block::ToolUse { input, .. } => Some(input.clone()),
                _ => None,
            })
        });
        assert_eq!(
            tool_use_input
                .as_ref()
                .and_then(|v| v.get("cmd"))
                .and_then(|v| v.as_str()),
            Some("ls")
        );
        // The tool RESULT survives as a Role::Tool message with its content intact.
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult {
            content, tool_use_id, ..
        } = &tool_results[0].content[0]
        {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
            assert_eq!(tool_use_id, "call_1");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn grok_round_trip_preserves_tool_failure() {
        // chat_history.jsonl can't express a failed tool result; the Grok parser derives
        // is_error/status from updates.jsonl. Emit must therefore write the status sidecar, or
        // every ported failure silently becomes a success.
        let mut s = sample_session(Harness::Grok);
        let mut failed = Message::new(Role::Tool);
        failed.content.push(Block::ToolResult {
            tool_use_id: "call_2".into(),
            content: "command not found: frobnicate".into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        });
        s.messages.push(failed);

        let out = temp_dir();
        let res = emit(&s, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.join("updates.jsonl").exists(), "status sidecar written");

        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Grok,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Grok::new().parse(&r).unwrap();
        let mut results: Vec<(String, bool, Option<String>)> = Vec::new();
        for m in parsed.messages.iter().filter(|m| m.role == Role::Tool) {
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id,
                    is_error,
                    status,
                    ..
                } = b
                {
                    results.push((tool_use_id.clone(), *is_error, status.clone()));
                }
            }
        }
        assert_eq!(
            results,
            vec![
                ("call_1".to_string(), false, Some("completed".to_string())),
                ("call_2".to_string(), true, Some("failed".to_string())),
            ],
            "is_error/status must survive a port into grok"
        );
    }

    #[test]
    fn new_cwd_and_new_id_overrides() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let opts = EmitOptions {
            new_cwd: Some(PathBuf::from("/tmp/rehomed")),
            new_id: Some("forced-id".into()),
            ..Default::default()
        };
        let res = emit(&s, Harness::Claude, &out, &opts).unwrap();
        assert_eq!(res.new_id, "forced-id");
        assert!(res.path.to_string_lossy().contains("-tmp-rehomed"));
        assert!(res.path.ends_with("forced-id.jsonl"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn opencode_round_trip() {
        // The emitter writes `opencode.db`; re-parse by pointing an adapter straight at that db
        // file (no HOME mutation). `out_dir` is a `storage/` root, so the db lands beside it.
        let storage = temp_dir().join(".local/share/opencode/storage");
        fs::create_dir_all(&storage).unwrap();

        let s = sample_session(Harness::OpenCode);
        let res = emit(&s, Harness::OpenCode, &storage, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        assert_eq!(res.path.file_name().unwrap(), "opencode.db");

        let oc = OpenCode::with_db(res.path.clone());
        let refs = oc.discover().unwrap();
        let parsed = refs.iter().find(|r| r.id == res.new_id).map(|r| oc.parse(r).unwrap());

        let parsed = parsed.expect("emitted opencode session should be discoverable");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result split into a Tool turn
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
        } else {
            panic!("expected tool result");
        }
        // reasoning survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[test]
    fn openclaw_round_trip() {
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let res = emit(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        // sessions.json index written
        assert!(res.path.parent().unwrap().join("sessions.json").exists());

        // OpenClaw::parse reads r.path directly; cwd/created come from the header line.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::OpenClaw,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = OpenClaw::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result → toolResult message (Role::Tool)
        let tool_results: Vec<_> = parsed.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_results.len(), 1);
        // thinking survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
        // model promoted
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn gemini_round_trip() {
        let s = sample_session(Harness::Gemini);
        let out = temp_dir();
        let res = emit(&s, Harness::Gemini, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // gemini parse keys off the `chats/` dir in the path; emit places the file there.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Gemini,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Gemini::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        // tool result is retagged to a Tool turn by the reader
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // thinking → thoughts survives
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_round_trip() {
        use crate::harness::hermes::Hermes;
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        assert_eq!(res.path.file_name().unwrap(), "state.db");

        // Point an adapter straight at the emitted DB (no HERMES_HOME mutation).
        let h = Hermes::with_db(res.path.clone());
        let refs = h.discover().unwrap();
        let parsed = refs.iter().find(|r| r.id == res.new_id).map(|r| h.parse(r).unwrap());

        let parsed = parsed.expect("emitted hermes session should be discoverable");
        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result row → Tool turn
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // reasoning → Thinking
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_emit_creates_fts_and_is_searchable() {
        use rusqlite::{Connection, OpenFlags};

        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        let db = res.path.clone();
        assert_eq!(db.file_name().unwrap(), "state.db");

        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

        // The FTS virtual tables must exist (a fresh port is invisible to Hermes search without them).
        let table_exists = |name: &str| -> bool {
            conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .is_ok()
        };
        assert!(table_exists("messages_fts"), "messages_fts must exist");
        // trigram is guarded; assert it exists when the tokenizer is available (it is in bundled FTS5).
        assert!(
            table_exists("messages_fts_trigram"),
            "messages_fts_trigram should exist with bundled FTS5"
        );

        // The triggers must have populated the index: an FTS MATCH finds an emitted message.
        let cnt: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt >= 1, "FTS query for 'files' should match the emitted user message");

        // And the trigram index covers a substring of the assistant text.
        let cnt_t: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'listing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt_t >= 1, "trigram FTS should find 'listing' substring");
    }

    /// A session whose only non-text payload is an image — a lossy target (Grok stores plain text
    /// only) should report the image as dropped.
    fn image_session() -> Session {
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "look at this".into(),
        });
        user.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("data:image/png;base64,AAAA".into()),
        });
        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text {
            text: "nice picture".into(),
        });

        Session {
            id: "img-id".into(),
            harness: Harness::Grok,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("image session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: None,
            messages: vec![user, asst],
            source_path: None,
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        }
    }

    #[test]
    fn emit_verified_reports_lossy_grok() {
        // An image-bearing session ported to Grok (plain-text chat_history) should warn that the
        // image was dropped — the canonical lossy case.
        let img = image_session();
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&img, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("image")),
            "expected an image-dropped warning, got: {warnings:?}"
        );

        // A tools+text+system+reasoning Grok session now round-trips cleanly (post tool-emit fix):
        // Grok keeps system turns, tool calls, tool results, and reasoning.
        let clean = sample_session(Harness::Grok);
        let out2 = temp_dir();
        let (_r2, w2) = emit_verified(&clean, Harness::Grok, &out2, &EmitOptions::default()).unwrap();
        assert!(w2.is_empty(), "expected clean Grok round-trip, got: {w2:?}");
    }

    #[test]
    fn emit_verified_clean_for_openclaw() {
        // OpenClaw represents text, thinking, tool calls/results, system turns — a clean round-trip.
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        // No tool/image/reasoning loss; OpenClaw keeps system turns and the title.
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for OpenClaw, got: {warnings:?}"
        );
    }

    /// Every `messages`-row column we care about for losslessness, as a comparable record.
    #[cfg(feature = "sqlite")]
    #[derive(Debug, PartialEq)]
    struct HermesMsgRow {
        role: String,
        content: Option<String>,
        tool_call_id: Option<String>,
        tool_calls: Option<String>,
        tool_name: Option<String>,
        token_count: Option<i64>,
        finish_reason: Option<String>,
        reasoning: Option<String>,
        reasoning_content: Option<String>,
        reasoning_details: Option<String>,
        codex_reasoning_items: Option<String>,
        codex_message_items: Option<String>,
        platform_message_id: Option<String>,
        observed: Option<i64>,
    }

    /// Dump every message row (ordered) of `session_id` from a Hermes `state.db` into comparable
    /// tuples — the data side of the column set. `timestamp` is excluded (REAL secs, compared
    /// separately at ms granularity); `id`/`session_id` are surrogate keys, not content.
    #[cfg(feature = "sqlite")]
    fn dump_hermes_messages(db: &Path, session_id: &str) -> Vec<HermesMsgRow> {
        use rusqlite::{Connection, OpenFlags};
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT role, content, tool_call_id, tool_calls, tool_name, token_count, \
                 finish_reason, reasoning, reasoning_content, reasoning_details, \
                 codex_reasoning_items, codex_message_items, platform_message_id, observed \
                 FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(HermesMsgRow {
                    role: r.get(0)?,
                    content: r.get(1)?,
                    tool_call_id: r.get(2)?,
                    tool_calls: r.get(3)?,
                    tool_name: r.get(4)?,
                    token_count: r.get(5)?,
                    finish_reason: r.get(6)?,
                    reasoning: r.get(7)?,
                    reasoning_content: r.get(8)?,
                    reasoning_details: r.get(9)?,
                    codex_reasoning_items: r.get(10)?,
                    codex_message_items: r.get(11)?,
                    platform_message_id: r.get(12)?,
                    observed: r.get(13)?,
                })
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Dump the session-row columns we round-trip (everything but the surrogate `id` and the
    /// auto-derived `message_count`). `started_at`/`ended_at` are REAL secs and compared at ms
    /// granularity in the caller, so they're excluded here.
    #[cfg(feature = "sqlite")]
    #[allow(clippy::type_complexity)]
    fn dump_hermes_session(
        db: &Path,
        session_id: &str,
    ) -> (
        String,         // source
        Option<String>, // user_id
        Option<String>, // model
        Option<String>, // model_config
        Option<String>, // system_prompt
        Option<String>, // end_reason
        Option<String>, // title
        // tool_call_count, input/output/cache_read/cache_write/reasoning tokens (grouped to stay
        // within std's 12-arity Debug/PartialEq tuple impls).
        [i64; 6],
    ) {
        use rusqlite::{Connection, OpenFlags};
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.query_row(
            "SELECT source, user_id, model, model_config, system_prompt, end_reason, title, \
             tool_call_count, input_tokens, output_tokens, cache_read_tokens, \
             cache_write_tokens, reasoning_tokens FROM sessions WHERE id = ?1",
            [session_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?],
                ))
            },
        )
        .unwrap()
    }

    /// Format-complete losslessness for the SQLite-backed Hermes adapter: build a `state.db` row set
    /// that exercises the **full column surface** (every per-session column and every per-message
    /// column: tool calls + results, all three reasoning columns, codex items, token_count,
    /// finish_reason, platform_message_id, observed), parse it, emit it to a fresh db, re-parse, and
    /// assert that emit reproduced every session-row and message-row column value. The parse→emit
    /// path is the round-trip under test; we compare the *source* db's rows against the
    /// *re-emitted* db's rows so nothing is asserted only via the lossy IR projection.
    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_complete_round_trip_is_lossless() {
        use crate::harness::hermes::Hermes;
        use crate::harness::Adapter;
        use rusqlite::Connection;

        // --- 1. Build a source state.db exercising the full column set. ---
        let src_home = temp_dir();
        let src_db = src_home.join("state.db");
        {
            let conn = Connection::open(&src_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, source TEXT NOT NULL, user_id TEXT, model TEXT,
                    model_config TEXT, system_prompt TEXT, parent_session_id TEXT,
                    started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
                    message_count INTEGER DEFAULT 0, tool_call_count INTEGER DEFAULT 0,
                    input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                    cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
                    reasoning_tokens INTEGER DEFAULT 0, title TEXT
                 );
                 CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
                    content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
                    timestamp REAL NOT NULL, token_count INTEGER, finish_reason TEXT, reasoning TEXT,
                    reasoning_content TEXT, reasoning_details TEXT, codex_reasoning_items TEXT,
                    codex_message_items TEXT, platform_message_id TEXT, observed INTEGER DEFAULT 0
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions \
                 (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
                  end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
                  cache_read_tokens, cache_write_tokens, reasoning_tokens, title) \
                 VALUES ('src', 'telegram', 'u-42', 'nous/hermes-4', '{\"temp\":0.7}', \
                 'be helpful', 1000.000, 1050.000, 'completed', 3, 1, 11, 22, 3, 4, 5, 'Full session')",
                [],
            )
            .unwrap();
            // user turn with token_count + platform_message_id + observed.
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, timestamp, token_count, platform_message_id, observed) \
                 VALUES ('src','user','hello there',1001.000,7,'pmid-1',1)",
                [],
            )
            .unwrap();
            // assistant turn: tool call + all reasoning columns + finish_reason + token_count + codex.
            let tc = "[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]";
            let rd = "[{\"type\":\"reasoning.summary\",\"summary\":\"sum\"},{\"type\":\"reasoning.encrypted_content\",\"encrypted_content\":\"ENC\"}]";
            let cri = "[{\"type\":\"reasoning\",\"id\":\"rs_a\",\"encrypted_content\":\"CBLOB\"}]";
            let cmi =
                "[{\"type\":\"message\",\"phase\":\"final\",\"content\":[{\"type\":\"output_text\",\"text\":\"D\"}]}]";
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, \
                  reasoning, reasoning_content, reasoning_details, codex_reasoning_items, \
                  codex_message_items) \
                 VALUES ('src','assistant','here you go',?1,1002.000,99,'stop','short sum',\
                 'native pad',?2,?3,?4)",
                rusqlite::params![tc, rd, cri, cmi],
            )
            .unwrap();
            // tool result turn with tool_name.
            conn.execute(
                "INSERT INTO messages \
                 (session_id, role, content, tool_call_id, tool_name, timestamp) \
                 VALUES ('src','tool','{\"ok\":true}','c1','web_search',1003.000)",
                [],
            )
            .unwrap();
        }
        let src_rows = dump_hermes_messages(&src_db, "src");
        let src_session = dump_hermes_session(&src_db, "src");

        // --- 2. Parse the source db (adapter pointed straight at it; no HERMES_HOME mutation). ---
        let h = Hermes::with_db(src_db.clone());
        let sref = h
            .discover()
            .unwrap()
            .into_iter()
            .find(|r| r.id == "src")
            .expect("source session discoverable");
        let parsed = h.parse(&sref).unwrap();

        // --- 3. Emit to a fresh db, preserving the original id so the dumps line up. ---
        let dst_home = temp_dir();
        let opts = EmitOptions {
            new_id: Some("src".into()),
            ..Default::default()
        };
        let res = emit(&parsed, Harness::Hermes, &dst_home, &opts).unwrap();

        // --- 4. Compare the source db rows against the re-emitted db rows, column by column. ---
        // JSON-bearing columns (tool_calls / reasoning_details / codex_*) are compared by *parsed
        // value*, not raw string: the IR stash → re-serialize path canonicalizes JSON whitespace
        // (and, before `preserve_order`, used to re-sort keys), which the losslessness bar
        // explicitly permits ("key ORDER may differ"). Plain-text columns are compared verbatim.
        let dst_rows = dump_hermes_messages(&res.path, "src");
        let norm = |rows: Vec<HermesMsgRow>| -> Vec<HermesMsgRow> {
            rows.into_iter()
                .map(|mut r| {
                    let canon = |s: &Option<String>| -> Option<String> {
                        s.as_deref()
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok().map(|v| v.to_string()))
                    };
                    if let Some(c) = canon(&r.tool_calls) {
                        r.tool_calls = Some(c);
                    }
                    if let Some(c) = canon(&r.reasoning_details) {
                        r.reasoning_details = Some(c);
                    }
                    if let Some(c) = canon(&r.codex_reasoning_items) {
                        r.codex_reasoning_items = Some(c);
                    }
                    if let Some(c) = canon(&r.codex_message_items) {
                        r.codex_message_items = Some(c);
                    }
                    r
                })
                .collect()
        };
        assert_eq!(
            norm(src_rows),
            norm(dst_rows),
            "message-row columns must round-trip losslessly (left=source, right=re-emitted)"
        );
        let dst_session = dump_hermes_session(&res.path, "src");
        assert_eq!(
            src_session, dst_session,
            "session-row columns must round-trip losslessly (left=source, right=re-emitted)"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn emit_verified_clean_for_hermes() {
        // Hermes keeps tools, reasoning, system turns, and title — should round-trip cleanly.
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for Hermes, got: {warnings:?}"
        );
    }

    /// Format-complete losslessness: a rich Claude transcript parsed under `ParseOptions::complete`
    /// and re-emitted reproduces EVERY record with the same top-level key set and the same value
    /// per key (key order may differ). Covers a user turn, an assistant turn (text + tool_use), a
    /// user tool_result turn (with a `toolUseResult` mirror), a `system` record, and three meta
    /// records (mode / queue-operation / ai-title) carried verbatim.
    #[test]
    fn claude_complete_round_trip_is_lossless() {
        use crate::harness::claude::{self, stream_str, CARRIER_KEY};
        use crate::stream::{CollectSink, ParseOptions};

        let sid = "11111111-1111-4111-8111-111111111111";
        // A transcript exercising every covered record shape, with assorted top-level + message-level
        // metadata the lean passes would drop.
        let src_lines = [
            json!({"type":"user","uuid":"u0","parentUuid":null,"sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","userType":"external","isSidechain":false,
                "timestamp":"2026-06-16T00:00:00.000Z","message":{"role":"user","content":"hello there"}}),
            json!({"type":"mode","sessionId":sid,"mode":"plan","timestamp":"2026-06-16T00:00:01.000Z"}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u0","sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","requestId":"req_42","isSidechain":false,
                "timestamp":"2026-06-16T00:00:02.000Z",
                "message":{"id":"msg_xyz","role":"assistant","model":"claude-opus-4",
                    "stop_reason":"tool_use","stop_sequence":null,
                    "usage":{"input_tokens":12,"output_tokens":34,"cache_read_input_tokens":5},
                    "content":[{"type":"text","text":"on it"},
                        {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a.rs"}}]}}),
            json!({"type":"queue-operation","sessionId":sid,"operation":"enqueue","content":"do later",
                "timestamp":"2026-06-16T00:00:03.000Z"}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":sid,"cwd":"/work/proj",
                "gitBranch":"main","version":"0.9.14","userType":"external","isSidechain":false,
                "timestamp":"2026-06-16T00:00:04.000Z",
                "toolUseResult":{"filePath":"a.rs","content":"fn main(){}","numLines":1},
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":"fn main(){}","is_error":false}]}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"s3","parentUuid":"u2",
                "sessionId":sid,"level":"info","timestamp":"2026-06-16T00:00:05.000Z",
                "compactMetadata":{"trigger":"manual","preTokens":1000}}),
            json!({"type":"ai-title","sessionId":sid,"aiTitle":"Reading a file"}),
        ];
        let src_text = src_lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        // Parse format-complete: every record is carried (meta/system as carriers, user/assistant/
        // tool with exhaustive `extra`).
        let mut sink = CollectSink::default();
        let session = stream_str(sid, &src_text, None, &ParseOptions::complete(), &mut sink);
        let mut session = session;
        session.messages = sink.messages;
        assert_eq!(
            session.messages.len(),
            src_lines.len(),
            "every source record must produce a message under complete"
        );

        // Emit back to disk, keeping the original session id + cwd so the envelope matches the source.
        let out = temp_dir();
        let opts = EmitOptions {
            new_id: Some(sid.to_string()),
            new_cwd: Some(PathBuf::from("/work/proj")),
            ..Default::default()
        };
        let res = emit(&session, Harness::Claude, &out, &opts).unwrap();

        // Read the emitted .jsonl back, line by line, as raw JSON.
        let emitted_text = fs::read_to_string(&res.path).unwrap();
        let emitted: Vec<Value> = emitted_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect();

        assert_eq!(
            emitted.len(),
            src_lines.len(),
            "same number of records out as in (no dup ai-title, no dropped record)"
        );

        // Index source records by uuid (else fall back to type for the uuid-less meta lines).
        let key_of = |v: &Value| -> String {
            v.get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| v.get("type").and_then(Value::as_str).unwrap_or("?").to_string())
        };
        let by_key: std::collections::HashMap<String, &Value> = src_lines.iter().map(|v| (key_of(v), v)).collect();

        // Carriers must come back byte-equal (same JSON value) to their source record.
        let carrier_types: Vec<&str> = session
            .messages
            .iter()
            .filter_map(|m| m.extra.get(CARRIER_KEY)?.get("type")?.as_str())
            .collect();
        assert!(
            carrier_types.contains(&"mode")
                && carrier_types.contains(&"queue-operation")
                && carrier_types.contains(&"ai-title")
                && carrier_types.contains(&"system"),
            "meta + system records carried as carriers, got {carrier_types:?}"
        );
        // sanity: CARRIER_KEY constant is the public one.
        assert_eq!(claude::CARRIER_KEY, "_record");

        // For every emitted record, find its source and assert key-set + per-value parity.
        for out_rec in &emitted {
            let k = key_of(out_rec);
            let src = by_key
                .get(&k)
                .unwrap_or_else(|| panic!("no source for emitted record {k}"));
            let src_obj = src.as_object().unwrap();
            let out_obj = out_rec.as_object().unwrap();

            let src_keys: std::collections::BTreeSet<&String> = src_obj.keys().collect();
            let out_keys: std::collections::BTreeSet<&String> = out_obj.keys().collect();
            assert_eq!(
                src_keys, out_keys,
                "record {k}: top-level key set differs\n src={src_keys:?}\n out={out_keys:?}"
            );
            for (key, src_val) in src_obj {
                assert_eq!(
                    out_obj.get(key),
                    Some(src_val),
                    "record {k}: value for `{key}` differs\n src={src_val}\n out={:?}",
                    out_obj.get(key)
                );
            }
        }

        fs::remove_dir_all(&out).ok();
    }

    // --------------------------------------------------------------------------------------------
    // Verifier v2 + newly-carried-field round trips
    // --------------------------------------------------------------------------------------------

    /// The strict gate and rendering: an expected-only report is never fatal and still lists its
    /// losses; any unexpected delta trips `has_unexpected` and reads back cleanly.
    #[test]
    fn fidelity_report_gate_and_rendering() {
        let expected_only = FidelityReport {
            deltas: vec![Delta {
                field: "block:image".into(),
                before: "1".into(),
                after: "0".into(),
                expected: true,
            }],
        };
        assert!(!expected_only.has_unexpected(), "expected losses never fail strict");
        assert_eq!(expected_only.lost_lines().len(), 1);
        assert!(expected_only.lost_lines()[0].contains("expected"));

        let unexpected = FidelityReport {
            deltas: vec![Delta {
                field: "usage".into(),
                before: "1".into(),
                after: "0".into(),
                expected: false,
            }],
        };
        assert!(unexpected.has_unexpected());
        assert!(!unexpected.lost_lines()[0].contains("expected"));
    }

    /// OpenCode (SQLite) carries per-message usage (incl. `cost_usd`) and a tool result's structured
    /// `details` (`state.{title,metadata}`) — all previously dropped on the file-tree path.
    #[cfg(feature = "sqlite")]
    #[test]
    fn opencode_carries_usage_cost_and_details() {
        let mut s = sample_session(Harness::OpenCode);
        // Give the assistant real usage + cost.
        for m in &mut s.messages {
            if m.role == Role::Assistant {
                m.usage = Some(Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    cache_read_tokens: Some(10),
                    cache_creation_tokens: Some(2),
                    reasoning_tokens: Some(5),
                    cost_usd: Some(0.0123),
                });
            }
            // Give the tool result structured details.
            for b in &mut m.content {
                if let Block::ToolResult { details, .. } = b {
                    *details = Some(serde_json::json!({ "title": "ls", "metadata": { "exit": 0 } }));
                }
            }
        }

        let storage = temp_dir().join(".local/share/opencode/storage");
        fs::create_dir_all(&storage).unwrap();
        let res = emit(&s, Harness::OpenCode, &storage, &EmitOptions::default()).unwrap();
        let oc = OpenCode::with_db(res.path.clone());
        let parsed = oc
            .discover()
            .unwrap()
            .into_iter()
            .find(|r| r.id == res.new_id)
            .map(|r| oc.parse(&r).unwrap())
            .expect("discoverable");

        let usage = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.usage.clone())
            .expect("assistant usage survives");
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cost_usd, Some(0.0123));

        let details = parsed.messages.iter().flat_map(|m| &m.content).find_map(|b| match b {
            Block::ToolResult { details, .. } => details.clone(),
            _ => None,
        });
        assert_eq!(
            details.as_ref().and_then(|d| d.get("title")).and_then(|v| v.as_str()),
            Some("ls"),
            "tool result details survive on the block"
        );

        // The verifier sees no unexpected loss for this OpenCode round-trip.
        let (_r, report) = emit_report(&s, Harness::OpenCode, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(!report.has_unexpected(), "opencode round-trip: {:?}", report.deltas);
    }

    /// Codex carries the session system prompt (`base_instructions`) and a compaction pair
    /// (`compacted` records → boundary + summary).
    #[test]
    fn codex_carries_system_prompt_and_compaction() {
        let mut s = sample_session(Harness::Codex);
        s.system_prompt = Some("You are a careful engineer.".into());
        let mut boundary = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
        boundary.content.push(Block::Text { text: "".into() });
        let mut summary = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
        summary.content.push(Block::Text {
            text: "earlier: set up the project".into(),
        });
        s.messages.push(boundary);
        s.messages.push(summary);

        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();
        assert_eq!(parsed.system_prompt.as_deref(), Some("You are a careful engineer."));
        assert!(
            parsed
                .messages
                .iter()
                .any(|m| m.kind == MessageKind::CompactionBoundary),
            "compaction boundary survives into Codex"
        );
        assert!(
            parsed.messages.iter().any(|m| m.kind == MessageKind::CompactionSummary
                && m.text().as_deref() == Some("earlier: set up the project")),
            "compaction summary text survives"
        );
    }

    /// A cross-harness port INTO Claude carries a compaction pair, injected context, and the session
    /// system prompt into Claude's own record shapes (compact_boundary + isCompactSummary,
    /// attachment/rendered, prompt_snapshot).
    #[test]
    fn claude_cross_harness_carries_compaction_injected_and_system_prompt() {
        // Source harness is Codex → the Claude emitter's cross-harness path runs.
        let mut s = sample_session(Harness::Codex);
        s.system_prompt = Some("Follow the house style.".into());
        let mut injected = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness);
        injected.content.push(Block::Text {
            text: "<environment_context>cwd=/x</environment_context>".into(),
        });
        let mut boundary = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
        boundary.content.push(Block::Text {
            text: "[history compacted]".into(),
        });
        let mut summary = Message::of_kind(Role::User, MessageKind::CompactionSummary, Origin::Harness);
        summary.content.push(Block::Text {
            text: "summary seed".into(),
        });
        s.messages.push(injected);
        s.messages.push(boundary);
        s.messages.push(summary);

        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();
        assert_eq!(parsed.system_prompt.as_deref(), Some("Follow the house style."));
        assert!(
            parsed
                .messages
                .iter()
                .any(|m| m.kind == MessageKind::CompactionBoundary),
            "compact_boundary survives into Claude"
        );
        assert!(
            parsed
                .messages
                .iter()
                .any(|m| m.kind == MessageKind::CompactionSummary && m.text().as_deref() == Some("summary seed")),
            "isCompactSummary survives into Claude"
        );
        assert!(
            parsed.messages.iter().any(|m| m.kind == MessageKind::InjectedContext),
            "injected context survives as a Claude attachment"
        );
    }

    /// Hermes carries `Session::cwd` into its own `cwd` column.
    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_carries_cwd() {
        use crate::harness::hermes::Hermes;
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        let h = Hermes::with_db(res.path.clone());
        let parsed = h
            .discover()
            .unwrap()
            .into_iter()
            .find(|r| r.id == res.new_id)
            .map(|r| h.parse(&r).unwrap())
            .expect("discoverable");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
    }

    /// OpenClaw carries the title (`session_info`) and a model-change marker.
    #[test]
    fn openclaw_carries_title_and_model_change() {
        let mut s = sample_session(Harness::OpenClaw);
        let mut mc = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
        mc.model = Some("claude-opus-4".into());
        mc.content.push(Block::Text {
            text: "[model changed]".into(),
        });
        s.messages.push(mc);

        let out = temp_dir();
        let res = emit(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::OpenClaw,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = OpenClaw::new().parse(&r).unwrap();
        assert_eq!(
            parsed.title.as_deref(),
            Some("a test session"),
            "title round-trips via session_info"
        );
        // The model change is written as OpenClaw's own `model_change` entry (the reader folds it
        // into `Session::model`/notes rather than a standalone turn, so assert the emitted record).
        let emitted: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            emitted
                .iter()
                .any(|v| v.get("type").and_then(Value::as_str) == Some("model_change")
                    && v.get("modelId").and_then(Value::as_str) == Some("claude-opus-4")),
            "model_change entry written to the OpenClaw transcript"
        );
    }

    /// `--strict` never fails on an *expected* loss: an image ported into Grok (plain-text chat) is
    /// reported but not fatal.
    #[test]
    fn strict_passes_on_expected_loss() {
        let img = image_session();
        let out = temp_dir();
        let (_res, warnings) = emit_verified(
            &img,
            Harness::Grok,
            &out,
            &EmitOptions {
                strict: true,
                ..Default::default()
            },
        )
        .expect("expected loss (image → Grok) must not fail --strict");
        assert!(warnings.iter().any(|w| w.contains("image")));
    }

    /// Per-turn token usage survives a port into Codex. Codex keeps usage in a `token_count`
    /// event that trails the assistant turn it reports on (`codex::apply_token_count`), and the
    /// emitter wrote none at all, so every real port read back `usage: 393 → 0`.
    #[test]
    fn codex_carries_per_turn_usage() {
        let mut s = sample_session(Harness::Claude);
        for m in &mut s.messages {
            if m.role == Role::Assistant {
                m.usage = Some(crate::ir::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(34),
                    cache_read_tokens: Some(5),
                    ..Default::default()
                });
            }
        }
        let (res, report) = emit_report(&s, Harness::Codex, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.field == "usage"),
            "usage must survive into Codex, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::Codex, &res, &ParseOptions::full()).unwrap();
        let u = re
            .messages
            .iter()
            .find_map(|m| m.usage.as_ref())
            .expect("usage on the re-parsed Codex session");
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (Some(12), Some(34), Some(5))
        );
    }

    /// Assistant token usage survives a port into OpenClaw, whose message format has its own
    /// `usage` block (the emitter wrote none, so every port read back `usage: 462 → 0`).
    #[test]
    fn openclaw_carries_assistant_usage() {
        let mut s = sample_session(Harness::Claude);
        for m in &mut s.messages {
            if m.role == Role::Assistant {
                m.usage = Some(crate::ir::Usage {
                    input_tokens: Some(9),
                    output_tokens: Some(8),
                    cache_read_tokens: Some(7),
                    ..Default::default()
                });
            }
        }
        let (res, report) = emit_report(&s, Harness::OpenClaw, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.field == "usage"),
            "usage must survive into OpenClaw, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::OpenClaw, &res, &ParseOptions::full()).unwrap();
        let u = re
            .messages
            .iter()
            .find_map(|m| m.usage.as_ref())
            .expect("usage on the re-parsed OpenClaw session");
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (Some(9), Some(8), Some(7))
        );
    }

    /// A FOREIGN tool-result `details` survives a port into OpenCode. OpenCode reads `details` from
    /// `state.{title,metadata,time}`; the emitter folded back only those three keys, so a Claude
    /// `toolUseResult` (no such key) read back as `tool result details: 194 → 0`. Foreign keys now
    /// ride in `metadata`, OpenCode's own free-form slot.
    #[cfg(feature = "sqlite")]
    #[test]
    fn opencode_carries_foreign_tool_details() {
        let mut s = sample_session(Harness::Claude);
        for b in s.messages.iter_mut().flat_map(|m| &mut m.content) {
            if let Block::ToolResult { details, .. } = b {
                *details = Some(serde_json::json!({ "filePath": "/w/a.rs", "structuredPatch": [1] }));
            }
        }
        let (res, report) = emit_report(&s, Harness::OpenCode, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.field == "details"),
            "details must survive into OpenCode, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::OpenCode, &res, &ParseOptions::full()).unwrap();
        let d = re
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult { details, .. } => details.clone(),
                _ => None,
            })
            .expect("details on the re-parsed OpenCode tool result");
        assert_eq!(d["metadata"]["filePath"], "/w/a.rs");
    }

    /// A turn whose every block is unrepresentable in the target (Claude's signature-only thinking
    /// — bound reasoning with no plaintext and no portable blob) cannot come back: the emitted
    /// record is empty and the target's reader drops it. That is an EXPECTED loss, reported once
    /// as `unrepresentable_turns`, not a phantom deficit spread over role/kind/timestamp/id counts
    /// (which failed `--strict` on every real Claude session: 133 of 458 assistant turns).
    #[test]
    fn turns_with_only_unrepresentable_content_are_an_expected_loss() {
        let mut s = sample_session(Harness::Claude);
        let mut think = Message::new(Role::Assistant);
        think.timestamp = Some(Utc::now());
        think.id = Some("think-1".into());
        think.usage = Some(crate::ir::Usage {
            input_tokens: Some(7),
            ..Default::default()
        });
        think.content.push(Block::Thinking {
            text: "".into(),
            signature: Some("SIGONLY".into()),
            encrypted: None,
            redacted: false,
        });
        s.messages.insert(3, think);

        let (_res, report) = emit_report(&s, Harness::Codex, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            report.deltas.iter().all(|d| d.expected),
            "no unexpected loss from a turn the target cannot represent, got {:?}",
            report.deltas
        );
        let d = report
            .deltas
            .iter()
            .find(|d| d.field == "unrepresentable_turns")
            .expect("the dropped turn is reported");
        assert_eq!((d.before.as_str(), d.after.as_str()), ("1", "0"));

        // Into Claude itself the same turn is perfectly representable, so nothing is reported.
        let (_res, report) = emit_report(&s, Harness::Claude, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.field == "unrepresentable_turns"),
            "Claude holds signature-only thinking, got {:?}",
            report.deltas
        );
    }

    /// The `⚠ lost` rendering never dumps a whole value: a real session's system prompt is
    /// thousands of lines, and printing it buried every other delta in scrollback. The struct keeps
    /// the full value for `--json`.
    #[test]
    fn delta_describe_briefs_long_values() {
        let d = Delta {
            field: "system prompt".into(),
            before: format!("\n\nYou are an interactive agent{}\nand more\n", "…".repeat(4000)),
            after: "(none)".into(),
            expected: true,
        };
        let line = d.describe();
        assert!(
            line.chars().count() < 120 && !line.contains('\n'),
            "describe must stay one short line: {}",
            line.chars().count()
        );
        assert!(
            line.contains("You are an interactive agent"),
            "keeps the first real line: {line}"
        );
        assert!(line.contains("chars)"), "says how much was cut: {line}");
        assert!(line.ends_with("→ (none)"), "short values pass through: {line}");
        assert!(d.before.len() > 1000, "the delta itself keeps the full value");
    }

    /// Same-harness verification compares like with like. `cv port` parses a rehome
    /// format-complete, so re-parsing the output plainly diffed a complete session against a lean
    /// one and invented deltas (`per-message model: 2106 → 836` against 836 messages).
    #[test]
    fn same_harness_verification_reparses_in_the_source_mode() {
        use crate::harness::claude::stream_str;
        use crate::stream::CollectSink;

        let sid = "22222222-2222-4222-8222-222222222222";
        let text = [
            json!({"type":"user","uuid":"u0","parentUuid":null,"sessionId":sid,"cwd":"/w","version":"2.1.0",
                   "timestamp":"2026-09-19T00:00:00.000Z","message":{"role":"user","content":"hi"}}),
            json!({"type":"mode","sessionId":sid,"mode":"plan","timestamp":"2026-09-19T00:00:01.000Z"}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u0","sessionId":sid,"cwd":"/w","version":"2.1.0",
                   "timestamp":"2026-09-19T00:00:02.000Z",
                   "message":{"id":"msg_1","role":"assistant","model":"claude-opus-4","stop_reason":"end_turn",
                              "usage":{"input_tokens":3,"output_tokens":4},
                              "content":[{"type":"text","text":"hello"}]}}),
        ]
        .map(|v| v.to_string())
        .join("\n");

        let mut sink = CollectSink::default();
        let mut s = stream_str(sid, &text, None, &ParseOptions::complete(), &mut sink);
        s.messages = sink.messages;
        assert!(s.messages.iter().any(is_carrier), "the `mode` record is carried");

        let (_res, report) = emit_report(
            &s,
            Harness::Claude,
            &temp_dir(),
            &EmitOptions {
                new_id: Some(sid.into()),
                new_cwd: Some(PathBuf::from("/w")),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            report.deltas.is_empty(),
            "a complete-mode Claude rehome is lossless, got {:?}",
            report.deltas
        );
    }

    /// The INTERFACE-V2 §4 fidelity gain: a tool result's structured `details` is a SHARED concept,
    /// so it survives a cross-harness port INTO Claude — written as the `toolUseResult` sidecar and
    /// read back onto the block by Claude's own parser. This is the loss the verifier used to
    /// report as unexpected for Claude (the adapter kept the sidecar in `extra["claude"]`, so a
    /// re-parse saw no block-level details at all).
    #[test]
    fn claude_cross_harness_carries_tool_result_details() {
        let mut s = sample_session(Harness::OpenCode);
        for m in &mut s.messages {
            for b in &mut m.content {
                if let Block::ToolResult { details, .. } = b {
                    *details = Some(serde_json::json!({ "title": "ls", "metadata": { "exit": 0 } }));
                }
            }
        }
        let (res, report) = emit_report(&s, Harness::Claude, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.field == "details"),
            "details must survive a port into Claude, got {:?}",
            report.deltas
        );
        let parsed = crate::harness::claude::parse_str(&res.new_id, &fs::read_to_string(&res.path).unwrap(), None);
        let details = parsed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult { details, .. } => details.clone(),
                _ => None,
            })
            .expect("details on the re-parsed Claude tool result");
        assert_eq!(details["title"], "ls");
        assert_eq!(details["metadata"]["exit"], 0);
    }

    /// Same-harness replay reads `toolUseResult` back out of the block's `details` and reproduces
    /// the source record's value exactly — including the two shapes the parser has to reconcile
    /// with cv's derived `persistedOutput` pointer: an object sidecar (pointer merged in) and a
    /// bare string one (pointer parked beside it). Neither may leak that cv-only key into the
    /// emitted record.
    #[test]
    fn claude_replay_rebuilds_tool_use_result_from_details() {
        let stub = "<persisted-output>\nOutput too large (66KB). Full output saved to: /tmp/s/tool-results/b.txt\n\nPreview (first 2KB):\nhi\n</persisted-output>";
        let obj_tur = json!({ "stdout": "hi", "persistedOutputPath": "/tmp/s/tool-results/b.txt" });
        let str_tur = json!("Error: Exit code 64");
        let text = [
            json!({"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2026-09-19T00:00:00Z",
                   "toolUseResult": obj_tur,
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":stub}]}}),
            json!({"type":"user","sessionId":"s1","uuid":"u2","parentUuid":"u1","timestamp":"2026-09-19T00:00:01Z",
                   "toolUseResult": str_tur,
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":stub,"is_error":true}]}}),
        ]
        .map(|v| v.to_string())
        .join("\n");

        let s = crate::harness::claude::parse_str("s1", &text, None);
        let (res, _report) = emit_report(&s, Harness::Claude, &temp_dir(), &EmitOptions::default()).unwrap();
        let emitted: Vec<Value> = fs::read_to_string(&res.path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let sidecars: Vec<&Value> = emitted.iter().filter_map(|v| v.get("toolUseResult")).collect();
        assert_eq!(sidecars, vec![&obj_tur, &str_tur], "both sidecars replay verbatim");
        assert!(
            !emitted
                .iter()
                .any(|v| v["toolUseResult"].get("persistedOutput").is_some()),
            "cv's derived pointer is never written into a Claude record"
        );
    }

    /// `--strict` fails on an *unexpected* loss — one the target format could have carried. Tool
    /// result `details` used to be that loss for Claude (parse kept the sidecar in
    /// `extra["claude"]`, so a re-parse read back no block-level details); it now round-trips, so
    /// the subject here is per-message `usage`: Claude's records carry a `message.usage` block
    /// (`target_holds(Claude, "usage")`), but `emit_claude` only reconstructs one for a
    /// same-harness replay, so a foreign source's token counts are genuinely dropped. If that gap
    /// is ever closed, point this test at another real one rather than weakening the check.
    #[test]
    fn strict_fails_on_unexpected_loss() {
        let mut s = sample_session(Harness::OpenCode); // foreign source → Claude cross-harness path
        for m in &mut s.messages {
            if m.role == Role::Assistant {
                m.usage = Some(crate::ir::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(34),
                    ..Default::default()
                });
            }
        }
        // Sanity: the report flags an unexpected `usage` loss.
        let (_r, report) = emit_report(&s, Harness::Claude, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(
            report.deltas.iter().any(|d| d.field.contains("usage") && !d.expected),
            "expected an unexpected usage loss, got {:?}",
            report.deltas
        );
        // Non-strict: succeeds, lists the loss.
        let (_res, warnings) = emit_verified(&s, Harness::Claude, &temp_dir(), &EmitOptions::default()).unwrap();
        assert!(warnings.iter().any(|w| w.contains("usage")));
        // Strict: errors.
        let err = emit_verified(
            &s,
            Harness::Claude,
            &temp_dir(),
            &EmitOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("strict"), "got: {err:#}");
    }
}
