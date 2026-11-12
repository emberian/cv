//! Emit an IR [`Session`] back into a harness's native on-disk format.
//!
//! This is the engine behind `cv port`: cross-harness conversion and rehoming a session to
//! another cwd or machine. Conversion is `parse(source) -> IR -> emit(target)`.
//!
//! Correctness bar: a session emitted for harness H must round-trip back through that harness's
//! parser (`emit(s, H) |> parse == s`, modulo fields H cannot represent). See `docs/FORMATS.md` for
//! each target's on-disk schema.
//!
//! The free function [`emit`] is the public entry point, re-exported by the crate root.

#[cfg(test)]
use crate::harness::claude::verify_claude_resumable;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Message, MessageKind, Role, Session, SessionRef};
use crate::stream::Demand;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) mod fidelity;
mod prepare;
pub(crate) mod thinking;
use fidelity::{diff_features, reparse_features, source_features, target_holds, Field};
#[cfg(test)]
use fidelity::{diff_fidelity, reparse_emitted};
pub use fidelity::{Divergence, FidelityReport};

/// The root transcript a Claude sub-agent emission lives beside, and the agent id it is written as.
#[derive(Debug, Clone)]
pub struct SidechainOf {
    /// The root session's id: every emitted record carries its `sessionId`.
    pub root: String,
    /// The emitted records' `agentId`, and the file's `agent-<id>.jsonl` name.
    pub agent: String,
}

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
    /// What to do with [`Block::Thinking`] on the way out. See [`ThinkingMode`].
    pub thinking: ThinkingMode,
    /// Claude only: write the session as a **sub-agent transcript** of a root instead of a
    /// main-thread session. Every record carries the root's `sessionId`, `isSidechain: true` and
    /// `agentId`; no `ai-title` is written; the file is
    /// `<out_dir>/agent-<agent id>.jsonl` (the caller passes the root's `subagents/` dir as
    /// `out_dir`, since a sub-agent lives beside its root, not under a project dir of its own) and
    /// `EmitResult::new_id` is the agent id, the handle `SendMessage` takes. `new_id` is ignored.
    /// With `None`, the emitter writes a main-thread session: `isSidechain` false where present,
    /// no `agentId` — a span lifted from a sub-agent transcript joins the main thread.
    pub sidechain_of: Option<SidechainOf>,
    /// Replace a session file that already holds the emitted name, by atomic rename. Without it a
    /// taken name (a pinned `new_id` emitted twice) is refused with [`crate::publish::Exists`].
    pub force: bool,
    /// The demand the caller read `session` at. [`emit_report`] re-reads the target's output at the
    /// same one, so both sides of the diff hold the same records.
    pub parsed_at: ParsedAt,
}

/// The demand a session handed to [`emit`] was read at, declared by whoever read it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParsedAt {
    /// [`Demand::RICH`]: turns only.
    #[default]
    Full,
    /// [`Demand::COMPLETE`]: turns plus every carrier and complete record.
    Complete,
}

impl ParsedAt {
    /// What [`parse_for_emit`] reads: a same-harness rehome format-complete, so carriers replay
    /// verbatim; a cross-harness port at full fidelity, since no other emitter replays them.
    pub fn for_port(source: Harness, target: Harness) -> Self {
        if source == target {
            ParsedAt::Complete
        } else {
            ParsedAt::Full
        }
    }

    /// The demand tier this declaration names.
    fn demand(self) -> Demand {
        match self {
            ParsedAt::Full => Demand::RICH,
            ParsedAt::Complete => Demand::COMPLETE,
        }
    }
}

impl EmitOptions {
    /// How an emitter claims the session files it names.
    pub(crate) fn claim(&self) -> crate::publish::Claim {
        if self.force {
            crate::publish::Claim::Replace
        } else {
            crate::publish::Claim::New
        }
    }
}

/// What [`emit`] does with the model's reasoning.
///
/// Measured on one real 900-message session: of 204 thinking blocks only 27 carried text and 177
/// were **signature-only** — an opaque provider-signed blob with no plaintext at all. So "carry the
/// reasoning as text" is impossible for the majority of them; the only question a mode can answer
/// is whether the *turn* survives. An assistant turn whose sole content is such a block is
/// unrepresentable in a store that cannot hold the signature, and vanishes (the fidelity report
/// names it `unrepresentable_turns`), taking the conversation's user/assistant alternation with it.
///
/// Spelled `native` / `text` / `drop` / `portable` on the wire ([`ThinkingMode::parse`], [`std::str::FromStr`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Structured thinking where the target holds it, dropped where it does not, reported as an
    /// expected loss. The library default; for the CLI's port/rewind default see [`Self::replay_default`].
    #[default]
    Native,
    /// Claude replay: demote plaintext thinking to text, remove reasoning that has no plaintext
    /// (signature-only, redacted), and repair illegal tool IDs. Other targets use the text policy.
    Portable,
    /// Never lose the turn. Where the target cannot hold thinking structurally, a block with text
    /// becomes a [`Block::Text`]; a signature-only / encrypted / redacted blob becomes a short
    /// placeholder naming what it was and how big it was, so the turn (and the alternation around
    /// it) survives. The lost *signature* is still reported — this mode keeps the conversation, it
    /// does not make a lossy port look clean.
    Text,
    /// Never emit thinking at all, even where the target could hold it: for sharing a session
    /// without your reasoning. Every thinking loss is then an expected one (you asked for it).
    Drop,
}

impl ThinkingMode {
    /// Every mode, in CLI/`possible_values` order.
    pub const ALL: [ThinkingMode; 4] = [
        ThinkingMode::Native,
        ThinkingMode::Text,
        ThinkingMode::Drop,
        ThinkingMode::Portable,
    ];

    /// The wire spelling (`--thinking <mode>`, JSON, `possible_values`).
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Native => "native",
            ThinkingMode::Portable => "portable",
            ThinkingMode::Text => "text",
            ThinkingMode::Drop => "drop",
        }
    }

    /// The `cv rewind` / `cv port` default for a session: [`Self::Native`] when an adapter claims
    /// every recorded model's thinking as natively replayable
    /// ([`crate::harness::Adapter::native_thinking`]: Claude's, for `claude-*`), [`Self::Portable`]
    /// otherwise.
    pub fn replay_default<'a>(models: impl IntoIterator<Item = &'a str>) -> Self {
        let native = |model: &str| {
            Harness::ALL
                .iter()
                .filter_map(|h| crate::harness::rootless(*h))
                .any(|adapter| adapter.native_thinking(model))
        };
        let foreign = models.into_iter().any(|m| !native(m));
        if foreign {
            ThinkingMode::Portable
        } else {
            ThinkingMode::Native
        }
    }

    /// Parse a wire spelling, case-insensitively. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        ThinkingMode::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

impl std::str::FromStr for ThinkingMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        ThinkingMode::parse(s).ok_or_else(|| {
            let all = ThinkingMode::ALL.map(|m| m.as_str()).join(", ");
            anyhow::anyhow!("unknown thinking mode {s:?} (expected one of: {all})")
        })
    }
}

impl std::fmt::Display for ThinkingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An emitter: writes one IR session into one harness's native on-disk format.
/// Registered writers must accept guarded lazy fields and apply thinking policy per block.
type EmitFn = fn(&Session, &Path, &EmitOptions) -> Result<EmitResult>;

/// A check an emitter runs on what it wrote (the written path, the sub-agent id it wrote as).
/// The ONE emit registry: which harnesses can be written, and by what. [`emit`] dispatches through
/// it and [`supported_targets`] is derived from it, so the dispatcher and the CLI's `--to` list can
/// never drift apart. Adding a target = adding an arm here (nothing else to keep in sync). A
/// harness with no emitter in this build answers why, as the tail of `emit to <target> …`.
fn emitter_for(target: Harness) -> Result<EmitFn, &'static str> {
    Ok(match target {
        Harness::Claude => crate::harness::claude::emit,
        Harness::Codex => crate::harness::codex::emit,
        Harness::Grok => crate::harness::grok::emit,
        Harness::OpenCode => crate::harness::opencode::emit,
        Harness::OpenClaw => crate::harness::openclaw::emit,
        Harness::Gemini => crate::harness::gemini::emit,
        #[cfg(feature = "sqlite")]
        Harness::Hermes => crate::harness::hermes::emit,
        Harness::LmStudio => crate::harness::lmstudio::emit,
        Harness::Cline => crate::harness::cline::emit,
        Harness::Roo => crate::harness::roo::emit,
        Harness::Continue => crate::harness::continuedev::emit,
        // Qwen Code 0.25's session JSONL (`~/.qwen/projects/<cwd>/chats/<id>.jsonl`), not the
        // gemini-cli recording it no longer reads.
        Harness::Qwen => crate::harness::qwen::emit,
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes => return Err("requires the `sqlite` feature"),
        // Parse-only harnesses (Cursor, Goose, desktop apps, …) aren't conversion targets. Kimi
        // (`~/.kimi`) is not one either: kimi-cli froze that store in 2026-06 and migrates it to
        // `~/.kimi-code` on open, so a session written there was a port to nowhere.
        Harness::Cursor
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => return Err("is not supported yet"),
    })
}

/// Emit `session` into `target`'s native format under `out_dir` (the target harness's storage root,
/// or any directory for a dry run). Returns where it was written + how to resume it.
pub fn emit(session: &Session, target: Harness, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let f = registered_emitter(target)?;
    // Every registered writer consumes guarded fields and projects thinking per block.
    // Keep that contract at the writer boundary so direct and dispatched emission agree.
    f(session, out_dir, opts)
}

/// The target's registered writer, with the registry's refusal spelling.
fn registered_emitter(target: Harness) -> Result<EmitFn> {
    emitter_for(target).map_err(|why| anyhow::anyhow!("emit to {target} {why}"))
}

/// Rewrite every [`Block::Thinking`] in `session` as [`EmitOptions::thinking`] asks, for `target`.
/// A no-op under [`ThinkingMode::Native`] (the emitters' own drop-what-I-cannot-write behaviour is
/// already that mode). [`thinking_fate`] is the single decision function, shared with
/// [`block_survives`] so the verifier's idea of what survives cannot drift from what is written.
#[cfg(test)]
fn apply_thinking_mode(session: &mut Session, target: Harness, mode: ThinkingMode) {
    if mode == ThinkingMode::Native {
        return;
    }
    for msg in &mut session.messages {
        if !msg.content.iter().any(|b| matches!(b, Block::Thinking { .. })) {
            continue;
        }
        let mut out = Vec::with_capacity(msg.content.len());
        for b in std::mem::take(&mut msg.content) {
            let fate = match &b {
                Block::Thinking {
                    text,
                    signature,
                    encrypted,
                    redacted,
                } => thinking_fate(
                    target,
                    mode,
                    text,
                    signature.as_deref(),
                    encrypted.as_deref(),
                    *redacted,
                ),
                _ => {
                    out.push(b);
                    continue;
                }
            };
            match fate {
                ThinkingFate::Keep => out.push(b),
                ThinkingFate::Drop => {}
                ThinkingFate::AsText | ThinkingFate::Placeholder => {
                    let Block::Thinking {
                        text,
                        signature,
                        encrypted,
                        redacted,
                    } = b
                    else {
                        unreachable!("fate computed from a Thinking block")
                    };
                    let text = if fate == ThinkingFate::AsText {
                        text
                    } else {
                        thinking_placeholder(signature.as_deref(), encrypted.as_deref(), redacted).into()
                    };
                    out.push(Block::Text { text });
                }
            }
        }
        msg.content = out;
    }
}

/// What becomes of one thinking block under a [`ThinkingMode`] for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingFate {
    /// Written as a thinking block — the target holds at least one of its payloads.
    Keep,
    /// Written as a [`Block::Text`] carrying the reasoning text verbatim.
    AsText,
    /// Written as a [`Block::Text`] carrying a placeholder: there is no plaintext to carry.
    Placeholder,
    /// Not written at all.
    Drop,
}

/// The one decision: what `mode` does with this thinking block when writing to `target`.
fn thinking_fate(
    target: Harness,
    mode: ThinkingMode,
    text: &str,
    signature: Option<&str>,
    encrypted: Option<&str>,
    redacted: bool,
) -> ThinkingFate {
    thinking_fate_with_presence(target, mode, !text.trim().is_empty(), signature, encrypted, redacted)
}

pub(crate) fn thinking_fate_with_presence(
    target: Harness,
    mode: ThinkingMode,
    has_text: bool,
    signature: Option<&str>,
    encrypted: Option<&str>,
    redacted: bool,
) -> ThinkingFate {
    // Whether the target can hold this block AS a thinking block: any one payload is enough.
    let structural = (has_text && target_holds(target, Field::ThinkingText))
        || (signature.is_some() && target_holds(target, Field::ThinkingSignature))
        || (encrypted.is_some() && target_holds(target, Field::ThinkingEncrypted));
    let mode = if mode == ThinkingMode::Portable {
        if fidelity::write_rules(target).portable_replay {
            return if has_text {
                ThinkingFate::AsText
            } else {
                ThinkingFate::Drop
            };
        }
        ThinkingMode::Text
    } else {
        mode
    };
    match mode {
        ThinkingMode::Portable => unreachable!(),
        ThinkingMode::Drop => ThinkingFate::Drop,
        ThinkingMode::Native if structural => ThinkingFate::Keep,
        ThinkingMode::Native => ThinkingFate::Drop,
        ThinkingMode::Text if structural => ThinkingFate::Keep,
        ThinkingMode::Text if has_text => ThinkingFate::AsText,
        // Signature-only / encrypted / redacted: no plaintext exists, so the turn survives as a
        // named stand-in. An entirely empty thinking block has nothing honest to say, so it goes.
        ThinkingMode::Text if signature.is_some() || encrypted.is_some() || redacted => ThinkingFate::Placeholder,
        ThinkingMode::Text => ThinkingFate::Drop,
    }
}

/// The stand-in text [`ThinkingMode::Text`] writes for a block with no plaintext: what it was and
/// how big it was, in one short line. Never pretends to be the reasoning itself.
pub(crate) fn thinking_placeholder(signature: Option<&str>, encrypted: Option<&str>, redacted: bool) -> String {
    if let Some(e) = encrypted {
        format!("[thinking: encrypted reasoning blob, {} bytes, not portable]", e.len())
    } else if let Some(s) = signature {
        format!(
            "[thinking: signature-only reasoning, {}-byte provider signature, not portable]",
            s.len()
        )
    } else if redacted {
        "[thinking: redacted by the provider]".to_string()
    } else {
        "[thinking: not portable]".to_string()
    }
}

/// Whether any message content field is still a lazy [`Span`](crate::lazy::Span) (see [`emit`]).
fn has_unresolved_spans(session: &Session) -> bool {
    session
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| {
            block.text().is_some_and(crate::Text::is_lazy)
                || matches!(
                    block,
                    Block::ToolUse {
                        input: crate::Json::Source(_),
                        ..
                    }
                )
        })
}

/// Resolve complete values before a full-output consumer touches them. Ordinary owned sessions
/// borrow without a clone; a failed source read returns before native output is created.
#[cfg(test)]
pub(crate) fn materialized_session(session: &Session) -> Result<std::borrow::Cow<'_, Session>> {
    if has_unresolved_spans(session) {
        let mut owned = session.clone();
        owned.materialize()?;
        Ok(std::borrow::Cow::Owned(owned))
    } else {
        Ok(std::borrow::Cow::Borrowed(session))
    }
}

/// Emit `session` into `target`, then re-parse the written output with `target`'s own adapter and
/// diff it against the input IR. Returns the [`EmitResult`] plus a list of human-readable
/// conversion warnings (empty when the round-trip is clean). These report both losses and
/// target-supplied timestamps, so defaults cannot masquerade as source timing.
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
        let unexpected: Vec<&Divergence> = report.deltas.iter().filter(|d| !d.expected).collect();
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
    // Refuse an unregistered target before projecting the source: the projection materializes.
    registered_emitter(target)?;
    let before = source_features(target, opts.thinking, session)?;
    // An emitter that owns a check on its output (Claude's three resumability rules) runs it on
    // the staged file, before publishing.
    let result = emit(session, target, out_dir, opts)?;
    // Re-parse at the fidelity the SOURCE was parsed at, as its reader declared. Re-parsing a
    // complete session plainly compared it against a lean one: impossible deltas such as
    // `per-message model: 2106 → 836` on an 836-message session, because the source's carriers (and
    // the complete-mode roles of synthetic notices) had no counterpart.
    let mut report = match reparse_features(target, opts.thinking, &result, &opts.parsed_at.demand()) {
        Ok((reparsed, after)) => diff_features(target, opts.thinking, session, &reparsed, before, after),
        Err(e) => FidelityReport {
            deltas: vec![Divergence {
                what: "round_trip".into(),
                a: "parseable".into(),
                b: format!("re-parse failed ({e:#})"),
                expected: false,
                at: target.as_str().to_string(),
            }],
        },
    };
    if opts.parsed_at == ParsedAt::Complete && !fidelity::target_holds(target, fidelity::Field::CompleteReparse) {
        report.deltas.push(Divergence {
            what: "verification demand".into(),
            a: "complete".into(),
            b: "rich (target reader policy)".into(),
            expected: true,
            at: target.as_str().into(),
        });
    }
    Ok((result, report))
}

/// Which targets [`emit`] can currently write — derived from [`emitter_for`] (the one registry),
/// so this list is correct by construction.
pub fn supported_targets() -> &'static [Harness] {
    static TARGETS: std::sync::OnceLock<Vec<Harness>> = std::sync::OnceLock::new();
    TARGETS.get_or_init(|| {
        Harness::ALL
            .iter()
            .copied()
            .filter(|&h| emitter_for(h).is_ok())
            .collect()
    })
}

/// Effective cwd after applying any rehome override.
pub(crate) fn effective_cwd(session: &Session, opts: &EmitOptions) -> Option<PathBuf> {
    opts.new_cwd.clone().or_else(|| session.cwd.clone())
}

/// The store's own subdirectory of `out_dir` an emitter writes into, without doubling: when
/// `out_dir` already is that directory (emitting into a discovered root, whose basename is
/// `name`), it stays itself instead of nesting `<name>/<name>`. LM Studio's `conversations` and
/// OpenClaw's `agents` share this one rule.
pub(crate) fn out_dir_within(out_dir: &Path, name: &str) -> PathBuf {
    if out_dir.file_name().is_some_and(|n| n == name) {
        out_dir.to_path_buf()
    } else {
        out_dir.join(name)
    }
}

/// Branch name (if any) for git-flavored metadata fields.
pub(crate) fn branch_of(session: &Session) -> Option<String> {
    session.git.as_ref().and_then(|g| g.branch.clone())
}

/// Every `tool_use` id this session actually calls. A result whose `tool_use_id` is NOT in here is
/// an **orphan**: its call lives outside the transcript, which is the ordinary shape of a resumed or
/// `prune`d session that opens mid-call (a real 1639-message Claude session starts with exactly one
/// such result, its call pruned away). Emitters that store a result *inside* its call's record
/// (OpenCode, Gemini) would otherwise have nothing to fold it onto and would drop it silently.
pub(crate) fn called_tool_ids(session: &Session) -> std::collections::HashSet<&str> {
    session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// Target-required epoch-millis. Callers prefer recorded session time before using the write
/// clock; `emit_report` declares timestamps added by the target in its expected deltas.
pub(crate) fn epoch_ms(t: Option<DateTime<Utc>>) -> i64 {
    t.unwrap_or_else(Utc::now).timestamp_millis()
}

/// A [`Block::ToolResult`] with no call in this session — see [`called_tool_ids`].
pub(crate) fn is_orphan_result(b: &Block, called: &std::collections::HashSet<&str>) -> bool {
    matches!(b, Block::ToolResult { tool_use_id, .. } if !called.contains(tool_use_id.as_str()))
}

/// A short, stable, filesystem-safe hash of a string (FNV-1a, hex). Used for project-dir names.
pub(crate) fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Validate source-backed fields before a native writer creates output. The fidelity projection
/// keeps only scalar facts; it does not materialize bodies or arguments.
pub(crate) fn validate_native_source(session: &Session, target: Harness, thinking: ThinkingMode) -> Result<()> {
    // Complete records can be the only lazy fields (including records foreign to the
    // target). Validate their original provenance even when projection ignores them.
    crate::harness::wire::write::check_source_fields(session)?;
    if has_unresolved_spans(session) {
        source_features(target, thinking, session)?;
    }
    Ok(())
}

/// A prepared read of a live append-only transcript that grew while it was read: the session
/// as it stood at message `messages`, byte offset `read`; the file is `now` bytes long since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Grown {
    pub messages: usize,
    pub read: u64,
    pub now: u64,
}

impl std::fmt::Display for Grown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "as of message {} / byte offset {}; the source has grown since ({} bytes now)",
            self.messages, self.read, self.now
        )
    }
}

/// Prepare a full-fidelity message window, retaining the source guards used by native emission.
/// The stream continues through EOF for final metadata and the original message count. Indices
/// match ordinary full parsing, rather than format-complete carrier parsing. A live source that
/// grew meanwhile yields the prefix read, and [`Grown`] says where it stopped.
pub fn prepare_selected(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    start: usize,
    end: Option<usize>,
) -> Result<(Session, usize, Option<Grown>)> {
    let mut options = Demand::RICH;
    options.spans = true;
    prepare::selected(adapter, reference, &options, start, end)
}

/// Parse at native emission fidelity. Registered writers accept guarded bodies, including
/// same-target complete records. Adapters retain owned fallbacks for unsupported source shapes.
/// Public full and complete parsing still own bodies by default; this preparation explicitly
/// opts into checked sources.
pub fn parse_for_emit(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    target: Harness,
) -> Result<Session> {
    Ok(prepare_for_emit(adapter, reference, target)?.0)
}

/// [`parse_for_emit`], with where a checked read of a live source stopped ([`Grown`]).
pub fn prepare_for_emit(
    adapter: &dyn crate::harness::Adapter,
    reference: &SessionRef,
    target: Harness,
) -> Result<(Session, Option<Grown>)> {
    let mut options = ParsedAt::for_port(reference.harness, target).demand();
    if emitter_for(target).is_ok() {
        options.spans = true;
        return prepare::parse(adapter, reference, &options);
    }
    let mut session = crate::stream::collect_with(adapter, reference, &options)?;
    session.materialize()?;
    Ok((session, None))
}

#[cfg(test)]
pub(crate) mod frozen;
#[cfg(test)]
pub(crate) mod grok_qwen_tests;

#[cfg(test)]
mod tests {
    use super::fidelity::*;
    use super::*;
    use crate::harness::claude::emit::claude_encode_cwd;
    use crate::harness::{claude::Claude, codex::Codex, gemini::Gemini, grok::Grok, openclaw::OpenClaw, Parse};
    #[cfg(feature = "sqlite")]
    use crate::harness::{opencode::OpenCode, Adapter};
    use crate::ir::*;
    use anyhow::Context as _;
    use serde_json::json;

    #[test]
    fn native_source_validation_checks_complete_record_without_lazy_blocks() -> Result<()> {
        let world = crate::test_world("emit-complete-source");
        let path = world.base.join("record.json");
        fs::write(&path, br#"{"type":"assistant","unknown":"original"}"#)?;
        let mut session = Session::new("complete-source", Harness::OpenCode);
        session.messages.push(Message::new(Role::Assistant));
        let raw = crate::Json::source(&path, 0, fs::metadata(&path)?.len())?;
        crate::ir::CompleteRecord::attach_replay(&mut session.messages, Harness::OpenCode, raw, Default::default());
        assert!(!has_unresolved_spans(&session));
        validate_native_source(&session, Harness::Codex, ThinkingMode::Drop)?;
        fs::write(&path, br#"{"type":"assistant","unknown":"changed-longer"}"#)?;
        assert!(validate_native_source(&session, Harness::Codex, ThinkingMode::Drop).is_err());
        Ok(())
    }

    /// A scratch dir removed when the test ends — failed or not — by the same [`World`](cv_testkit::World)
    /// facility [`crate::test_world`] sits on. The environment stays untouched: these tests read none.
    struct Scratch(cv_testkit::World);
    impl std::ops::Deref for Scratch {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0.base
        }
    }

    fn temp_dir() -> Scratch {
        Scratch(cv_testkit::World::new("emit"))
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
            input: (serde_json::json!({ "cmd": "ls" })).into(),
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
            ..Session::new("orig-id", harness)
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
    fn codex_source_payloads_match_owned_bytes_and_reject_changed_sources() -> Result<()> {
        let out = temp_dir();
        let source_path = out.join("payload.json");
        fs::write(
            &source_path,
            format!(
                r#"{{"text":{},"input":{{"z":0,"a":[true,null,"\u03bb"],"z":1.25}}}}"#,
                serde_json::to_string(&"a\"\\\nλ".repeat(20_000))?
            ),
        )?;
        let source = crate::Json::source(&source_path, 0, fs::metadata(&source_path)?.len())?;
        let text = source.with_view(|view| Ok(view.get("text")?.unwrap().source_text()?.unwrap()))?;
        let input = source.with_view(|view| view.get("input")?.unwrap().to_json())?;
        let mut session = sample_session(Harness::Claude);
        session.model = None;
        for message in &mut session.messages {
            message.model = None;
            for block in &mut message.content {
                match block {
                    Block::Text { text: value } | Block::Thinking { text: value, .. } => *value = text.clone(),
                    Block::ToolUse { input: value, .. } => *value = input.clone(),
                    Block::ToolResult { content, is_error, .. } => {
                        *content = text.clone();
                        *is_error = true;
                    }
                    _ => {}
                }
            }
        }
        session.messages[1].content.insert(0, Block::Text { text: "".into() });
        session.messages[1].content.push(Block::Text { text: "".into() });
        session.messages[1].content.push(Block::Text { text: text.clone() });
        for role in [Role::User, Role::System] {
            let mut message = Message::new(role);
            message.content.push(Block::Thinking {
                text: "".into(),
                signature: Some("signature-only".into()),
                encrypted: None,
                redacted: false,
            });
            session.messages.push(message);
        }
        let mut compact = Message::new(Role::System);
        compact.kind = MessageKind::CompactionSummary;
        compact.content.push(Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: None,
            redacted: true,
        });
        session.messages.push(compact);
        let owned = materialized_session(&session)?;
        let opts = EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        };
        let mut before = Vec::new();
        for mode in ThinkingMode::ALL {
            let opts = EmitOptions {
                thinking: mode,
                ..opts.clone()
            };
            // Frozen dispatch policy: materialize first, then rewrite the owned IR.
            let mut legacy = owned.as_ref().clone();
            apply_thinking_mode(&mut legacy, Harness::Codex, mode);
            let expected = crate::harness::codex::emit(
                &legacy,
                &out.join(format!("owned-{}", mode.as_str())),
                &EmitOptions {
                    thinking: ThinkingMode::Native,
                    ..opts.clone()
                },
            )?;
            let (actual, actual_report) = emit_report(
                &session,
                Harness::Codex,
                &out.join(format!("lazy-{}", mode.as_str())),
                &opts,
            )?;
            let (_, expected_report) = emit_report(
                &owned,
                Harness::Codex,
                &out.join(format!("report-{}", mode.as_str())),
                &opts,
            )?;
            assert_eq!(fs::read(&actual.path)?, fs::read(&expected.path)?, "mode={mode:?}");
            let direct = crate::harness::codex::emit(&session, &out.join(format!("direct-{}", mode.as_str())), &opts)?;
            assert_eq!(fs::read(&direct.path)?, fs::read(&actual.path)?, "direct mode={mode:?}");
            assert_eq!(
                serde_json::to_value(actual_report)?,
                serde_json::to_value(expected_report)?
            );
            before.push((mode, actual.path.clone(), fs::read(&actual.path)?));
        }
        fs::write(&source_path, b"{}")?;
        for (mode, path, bytes) in before {
            assert!(emit_report(
                &session,
                Harness::Codex,
                &out.join(format!("lazy-{}", mode.as_str())),
                &EmitOptions {
                    thinking: mode,
                    ..opts.clone()
                }
            )
            .is_err());
            assert_eq!(fs::read(path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn claude_checked_writer_matches_legacy_payloads_metadata_and_resumability() -> Result<()> {
        let world = crate::test_world("claude-checked-legacy");
        let text_path = world.base.join("text.json");
        let args_path = world.base.join("args.json");
        fs::write(&text_path, serde_json::to_vec(&"a\"\\\nλ".repeat(5000))?)?;
        fs::write(&args_path, br#"{"first":1.20,"blob":"escaped\nargument","first":2}"#)?;
        let text = crate::Json::source(&text_path, 0, fs::metadata(&text_path)?.len())?
            .with_view(|view| view.source_text()?.context("source text"))?;
        let args = crate::Json::source(&args_path, 0, fs::metadata(&args_path)?.len())?;
        let mut session = Session::new("source", Harness::Claude);
        session.cwd = Some("/synthetic/claude-checked".into());
        session.created_at = Some("2026-10-12T12:00:00Z".parse()?);
        session.title = Some("retained title".into());
        let mut user = Message::new(Role::User);
        user.id = Some("u".into());
        user.content = vec![
            Block::Text {
                text: String::new().into(),
            },
            Block::Text { text: text.clone() },
            Block::Text {
                text: String::new().into(),
            },
        ];
        session.messages.push(user);
        let mut dropped = Message::new(Role::System);
        dropped.id = Some("system".into());
        dropped.parent_id = Some("u".into());
        session.messages.push(dropped);
        let mut assistant = Message::new(Role::Assistant);
        assistant.id = Some("a".into());
        assistant.parent_id = Some("system".into());
        assistant.content = vec![
            Block::Thinking {
                text: text.clone(),
                signature: Some("signature".into()),
                encrypted: None,
                redacted: false,
            },
            Block::Thinking {
                text: String::new().into(),
                signature: None,
                encrypted: None,
                redacted: true,
            },
            Block::ToolUse {
                id: "answered".into(),
                name: "Read".into(),
                input: args,
                namespace: None,
            },
            Block::ToolUse {
                id: "orphan".into(),
                name: "Read".into(),
                input: Value::Null.into(),
                namespace: None,
            },
        ];
        assistant.harness_extra_mut(Harness::Claude).extend([
            ("version".into(), json!("kept")),
            ("message.stop_reason".into(), Value::Null),
            ("unknown".into(), json!({"large":"replay metadata ".repeat(5000)})),
        ]);
        session.messages.push(assistant);
        let mut orphan = Message::new(Role::Assistant);
        orphan.id = Some("gone".into());
        orphan.parent_id = Some("a".into());
        orphan.content.push(Block::ToolUse {
            id: "orphan-only".into(),
            name: "Read".into(),
            input: Value::Null.into(),
            namespace: None,
        });
        session.messages.push(orphan);
        let mut result = Message::new(Role::Tool);
        result.id = Some("result".into());
        result.parent_id = Some("gone".into());
        result.content.push(Block::ToolResult {
            tool_use_id: "answered".into(),
            content: text.clone(),
            tool_name: Some("Read".into()),
            is_error: false,
            status: None,
            details: Some(json!({"kept":"detail","persistedOutput":"derived-only"})),
        });
        session.messages.push(result);
        let mut carrier = Message::new(Role::System);
        carrier.extra.insert(crate::ir::CARRIER_KEY.into(),json!({"type":"system","uuid":"carrier","parentUuid":"missing","sessionId":"old","cwd":"old","isSidechain":true,"agentId":"old","unrecognized":{"must":"survive"}}));
        session.messages.push(carrier);
        let mut raw_carrier = Message::new(Role::System);
        raw_carrier.extra.insert(crate::ir::CARRIER_KEY.into(),json!({"type":"assistant","uuid":"raw-carrier","parentUuid":"carrier","message":{"role":"assistant","content":[{"type":"tool_use","id":"answered"},{"type":"tool_use","id":"raw-orphan"},{"type":"text","text":"verbatim carrier content"}]}}));
        session.messages.push(raw_carrier);
        let mut raw_result = Message::new(Role::System);
        raw_result.extra.insert(crate::ir::CARRIER_KEY.into(),json!({"type":"user","uuid":"raw-result","message":{"content":[{"type":"tool_result","tool_use_id":"carrier-answer","content":"raw result"}]}}));
        session.messages.push(raw_result);
        let mut carrier_call = Message::new(Role::Assistant);
        carrier_call.id = Some("carrier-call".into());
        carrier_call.content.push(Block::ToolUse {
            id: "carrier-answer".into(),
            name: "Read".into(),
            input: Value::Null.into(),
            namespace: None,
        });
        session.messages.push(carrier_call);
        // Duplicate UUID/tool IDs, raw content overrides, and forward parents keep their old rules.
        let mut override_msg = Message::new(Role::Assistant);
        override_msg.id = Some("a".into());
        override_msg.parent_id = Some("future".into());
        override_msg.harness_extra_mut(Harness::Claude).extend([
            ("message".into(),json!({"role":"assistant","opaque":true,"content":[{"type":"tool_use","id":"raw-orphan"},{"type":"text","text":"raw text"}]})),
            ("message.custom".into(),json!("inner kept"))]);
        session.messages.push(override_msg);
        let mut future = Message::new(Role::User);
        future.id = Some("future".into());
        future.content = vec![
            Block::Text { text: "mixed".into() },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("file:img".into()),
            },
            Block::File {
                mime: Some("text/plain".into()),
                path: Some("doc".into()),
                source: Some("https://example.invalid/doc".into()),
            },
        ];
        session.messages.push(future);
        // Portable is a new wire policy; frozen legacy dispatch has no corresponding oracle.
        let mut frozen = crate::emit::frozen::Frozen::new("claude-checked-writer");
        for mode in [ThinkingMode::Native, ThinkingMode::Text, ThinkingMode::Drop] {
            for sidechain in [
                None,
                Some(SidechainOf {
                    root: "root".into(),
                    agent: "agent".into(),
                }),
            ] {
                let opts = EmitOptions {
                    new_id: Some("fixed-id".into()),
                    thinking: mode,
                    sidechain_of: sidechain,
                    ..Default::default()
                };
                let new = emit(
                    &session,
                    Harness::Claude,
                    &world
                        .base
                        .join(format!("new-{}-{}", mode.as_str(), opts.sidechain_of.is_some())),
                    &opts,
                )?;
                verify_claude_resumable(&new.path, opts.sidechain_of.as_ref().map(|of| of.agent.as_str()))?;
                let bytes = fs::read(&new.path)?;
                frozen.insert(
                    format!("{}-{}", mode.as_str(), opts.sidechain_of.is_some()),
                    json!([bytes.len(), crate::digest::sha256_hex(&bytes)]),
                );
                let direct = crate::harness::claude::emit::emit(
                    &session,
                    &world
                        .base
                        .join(format!("direct-{}-{}", mode.as_str(), opts.sidechain_of.is_some())),
                    &opts,
                )?;
                assert_eq!(
                    fs::read(&new.path)?,
                    fs::read(&direct.path)?,
                    "direct callers honor every thinking mode too"
                );
            }
        }
        frozen.check()?;
        Ok(())
    }

    #[test]
    fn claude_checked_writer_type_and_content_only_overrides_match_old_dispatch() -> Result<()> {
        let world = crate::test_world("claude-extra-overrides");
        let mut frozen = crate::emit::frozen::Frozen::new("claude-extra-overrides");
        for (case, role, override_content) in [
            (0, Role::Assistant, false),
            (1, Role::User, false),
            (2, Role::Assistant, true),
            (3, Role::User, true),
        ] {
            let mut session = Session::new("s", Harness::Claude);
            session.created_at = Some("2026-10-12T12:00:00Z".parse()?);
            let mut message = Message::new(role);
            message.id = Some("fixed-message".into());
            message.content = vec![
                Block::ToolUse {
                    id: "orphan".into(),
                    name: "Read".into(),
                    input: Value::Null.into(),
                    namespace: None,
                },
                Block::Text {
                    text: "original text".into(),
                },
            ];
            let bag = message.harness_extra_mut(Harness::Claude);
            bag.insert(
                "type".into(),
                json!(if role == Role::Assistant { "user" } else { "assistant" }),
            );
            if override_content {
                bag.insert(
                    "message.content".into(),
                    json!([{"type":"tool_use","id":"raw-orphan"},{"type":"text","text":"override text"}]),
                );
            }
            for i in 0..128 {
                bag.insert(format!("opaque-{i}"), json!({"kept":i}));
            }
            session.messages.push(message);
            let opts = EmitOptions {
                new_id: Some("fixed-id".into()),
                ..Default::default()
            };
            // An override that leaves a `tool_use` in a record the writer keeps is not resumable:
            // the staged file is refused and nothing is published.
            let out = world.base.join(format!("new-{case}"));
            let verdict = match emit(&session, Harness::Claude, &out, &opts) {
                Ok(new) => {
                    let bytes = fs::read(&new.path)?;
                    json!([bytes.len(), crate::digest::sha256_hex(&bytes)])
                }
                Err(error) => {
                    assert!(!out.join("-").join("fixed-id.jsonl").exists());
                    json!(error.to_string().replace(out.to_str().unwrap(), "<out>"))
                }
            };
            frozen.insert(format!("case-{case}"), verdict);
        }
        frozen.check()?;
        Ok(())
    }

    #[test]
    fn claude_checked_writer_source_failure_preserves_destination_in_every_mode() -> Result<()> {
        let world = crate::test_world("claude-checked-failure");
        let source = world.base.join("body.json");
        fs::write(&source, serde_json::to_vec(&"checked body".repeat(3000))?)?;
        let text = crate::Json::source(&source, 0, fs::metadata(&source)?.len())?
            .with_view(|v| v.source_text()?.context("source text"))?;
        let mut session = Session::new("s", Harness::Codex);
        let mut message = Message::new(Role::Assistant);
        message.id = Some("a".into());
        message.content.push(Block::Thinking {
            text,
            signature: Some("sig".into()),
            encrypted: None,
            redacted: false,
        });
        session.messages.push(message);
        fs::write(&source, serde_json::to_vec(&"changed body".repeat(3000))?)?;
        for mode in ThinkingMode::ALL {
            let dir = world.base.join(mode.as_str());
            fs::create_dir_all(dir.join("-"))?;
            let destination = dir.join("-/fixed.jsonl");
            fs::write(&destination, "existing destination")?;
            let opts = EmitOptions {
                new_id: Some("fixed".into()),
                thinking: mode,
                ..Default::default()
            };
            assert!(emit(&session, Harness::Claude, &dir, &opts).is_err());
            assert_eq!(fs::read(&destination)?, b"existing destination");
            // Direct emitter callers preserve the same guarded boundary as dispatch callers.
            assert!(crate::harness::claude::emit::emit(&session, &dir, &opts).is_err());
            assert_eq!(fs::read(&destination)?, b"existing destination");
        }
        Ok(())
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
            message_count: None,
        };
        let parsed = Claude::detect().parse(&r).unwrap();

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
            message_count: None,
        };
        let parsed = Claude::detect().parse(&r).unwrap();

        let rich = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .find(|m| m.content.len() > 1)
            .expect("mixed-content user turn survives as multiple blocks");
        assert!(
            rich.content
                .iter()
                .any(|b| matches!(b, Block::Text { text } if text.matches_str("look at this") == Some(true))),
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

    /// A sub-agent transcript the reshape verbs cut: source ids and parents, a hook attachment
    /// between two turns (the IR never carries it, so its child's parent dangles), sidechain markers
    /// on every record, and a trailing call with no result.
    fn reshape_source() -> Session {
        use crate::harness::claude::parse_str;
        let side = |mut v: Value| {
            v["isSidechain"] = json!(true);
            v["agentId"] = json!("a0123456789abcdef");
            v["sessionId"] = json!("rootsess");
            v["cwd"] = json!("/work/proj");
            serde_json::to_string(&v).unwrap()
        };
        let text = [
            side(json!({"type":"user","uuid":"u0","parentUuid":null,"timestamp":"2026-01-01T10:00:00Z",
                "message":{"role":"user","content":"You are lane TEST. Build it."}})),
            side(json!({"type":"assistant","uuid":"a1","parentUuid":"u0","timestamp":"2026-01-01T10:01:00Z",
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make"}}]}})),
            side(json!({"type":"user","uuid":"u1","parentUuid":"a1","timestamp":"2026-01-01T10:02:00Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}})),
            side(json!({"type":"attachment","uuid":"h1","parentUuid":"u1","timestamp":"2026-01-01T10:02:30Z",
                "attachment":{"type":"hook_success","hookName":"PostToolUse","content":""}})),
            side(json!({"type":"assistant","uuid":"a2","parentUuid":"h1","timestamp":"2026-01-01T10:03:00Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"Built."},
                    {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git commit -am x"}}]}})),
            side(json!({"type":"user","uuid":"u2","parentUuid":"a2","timestamp":"2026-01-01T10:04:00Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"[main 1234abc] x","is_error":false}]}})),
            side(json!({"type":"assistant","uuid":"a3","parentUuid":"u2","timestamp":"2026-01-01T10:05:00Z",
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"sleep 999"}}]}})),
        ]
        .join("\n");
        parse_str("agent-a0123456789abcdef", &text, None)
    }

    /// Walk `parentUuid` back from the newest record as Claude Code does on resume: how many
    /// records it reaches, out of how many threaded ones.
    fn chain_walk(path: &Path) -> (usize, usize) {
        let lines: Vec<Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let threaded: Vec<&Value> = lines.iter().filter(|v| v.get("uuid").is_some()).collect();
        let by: std::collections::HashMap<&str, &Value> =
            threaded.iter().map(|v| (v["uuid"].as_str().unwrap(), *v)).collect();
        let mut cur = *threaded.last().unwrap();
        let mut n = 1;
        while let Some(p) = cur["parentUuid"].as_str() {
            cur = by[p];
            n += 1;
        }
        (n, threaded.len())
    }

    /// The emitter owns resumability: every reshape verb is a selector, and whatever it selects
    /// comes out as a transcript Claude Code resumes whole — no dangling `parentUuid`, no orphan
    /// `tool_use`, one identity — as a main-thread session and as a sub-agent of a root. Covers
    /// `distill --session/--agent-of` (`resumable_session`), `splice <id>:..N` (a fork, whose cut
    /// strands `t3`), a two-span splice (whose join point dangles), a plain port of the sub-agent
    /// file, and the distill fixture's own lane.
    #[test]
    fn reshape_verbs_emit_resumable() {
        use crate::distill::{prepare_distill, resumable_session, DistillOptions};
        use crate::splice::tests::compose;

        let src = reshape_source();
        // Six turns: the hook attachment is not one (the IR never carries it), so a2's parent `h1`
        // names a record no emit can write — the dangling-parent case every selector used to dodge.
        assert_eq!(src.messages.len(), 6);
        let opts = DistillOptions {
            keep_last: 1,
            ..Default::default()
        };
        let d = prepare_distill(&src, &opts).unwrap().distilled;
        let distilled = resumable_session(&src, &d, &opts).unwrap();
        let fork = compose(&[(&src, 0, Some(2))], None, Harness::Claude);
        let two = compose(&[(&src, 0, Some(3)), (&src, 4, None)], None, Harness::Claude);
        let lane = crate::distill::tests::lane();
        let dl = prepare_distill(&lane, &opts).unwrap().distilled;
        let lane_distilled = resumable_session(&lane, &dl, &opts).unwrap();

        let verbs: [(&str, &Session); 5] = [
            ("distill", &distilled),
            ("fork", &fork),
            ("splice", &two),
            ("port", &src),
            ("distill(lane).unwrap()", &lane_distilled),
        ];
        for (verb, session) in verbs {
            for sidechain in [
                None,
                Some(SidechainOf {
                    root: "rootsess".into(),
                    agent: "afedcba9876543210".into(),
                }),
            ] {
                let out = temp_dir();
                let opts = EmitOptions {
                    sidechain_of: sidechain.clone(),
                    ..Default::default()
                };
                let agent = sidechain.as_ref().map(|of| of.agent.as_str());
                let (res, _) = emit_verified(session, Harness::Claude, &out, &opts)
                    .unwrap_or_else(|e| panic!("{verb} sidechain={agent:?}: {e:#}"));
                verify_claude_resumable(&res.path, agent).unwrap_or_else(|e| panic!("{verb}: {e:#}"));
                let (reached, threaded) = chain_walk(&res.path);
                assert_eq!(
                    reached, threaded,
                    "{verb} sidechain={agent:?}: a dangling parent truncates the resume"
                );
                let text = fs::read_to_string(&res.path).unwrap();
                match agent {
                    None => {
                        assert!(
                            res.path.ends_with(format!("{}.jsonl", res.new_id)),
                            "{verb}: {}",
                            res.path.display()
                        );
                        assert!(!text.contains("\"agentId\""), "{verb}: {text}");
                    }
                    Some(a) => {
                        assert_eq!(res.new_id, a, "{verb}: the handle is the agent id");
                        assert!(
                            res.path.ends_with(format!("agent-{a}.jsonl")),
                            "{verb}: {}",
                            res.path.display()
                        );
                        assert!(
                            !text.contains("ai-title"),
                            "{verb}: a sub-agent carries no title record"
                        );
                        assert!(
                            text.contains("\"sessionId\":\"rootsess\""),
                            "{verb}: the root's id on every record"
                        );
                    }
                }
            }
        }

        // What each cut lost, specifically.
        let emit_main = |s: &Session| {
            let out = temp_dir();
            let res = emit(s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
            fs::read_to_string(&res.path).unwrap()
        };
        let t = emit_main(&src);
        assert!(!t.contains("\"t3\""), "the unanswered trailing call is dropped: {t}");
        assert!(
            !t.contains("\"uuid\":\"a3\""),
            "and the turn that held nothing else with it: {t}"
        );
        assert!(
            t.contains("\"parentUuid\":\"u1\""),
            "a2's parent re-links over the hook attachment: {t}"
        );
        let t = emit_main(&fork);
        assert!(
            t.contains("\"uuid\":\"u0\"") && !t.contains("\"t1\""),
            "fork at 2 strands t1: {t}"
        );
        let t = emit_main(&two);
        assert!(
            t.contains("\"uuid\":\"u2\",\"parentUuid\":\"u1\"") && !t.contains("\"uuid\":\"a3\""),
            "{t}"
        );
        let t = emit_main(&distilled);
        assert!(t.contains("[cv distill]") && !t.contains("\"t3\""), "{t}");
        let t = emit_main(&lane_distilled);
        assert!(!t.contains("\"z1\""), "the lane's unanswered z1 is dropped: {t}");
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
            message_count: None,
        };
        let parsed = Codex::detect().parse(&r).unwrap();

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
            assert_eq!(content.inline_str(), Some("file_a.txt\nfile_b.txt"));
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
            message_count: None,
        };
        let parsed = Codex::detect().parse(&r).unwrap();
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
            message_count: None,
        };
        let parsed = Grok::detect().parse(&r).unwrap();

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
                Block::ToolUse { input, .. } => Some(input.resolve().unwrap().into_owned()),
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
            assert_eq!(content.inline_str(), Some("file_a.txt\nfile_b.txt"));
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
            message_count: None,
        };
        let parsed = Grok::detect().parse(&r).unwrap();
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
            assert_eq!(content.inline_str(), Some("file_a.txt\nfile_b.txt"));
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
            message_count: None,
        };
        let parsed = OpenClaw::detect().parse(&r).unwrap();

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
            message_count: None,
        };
        let parsed = Gemini::detect().parse(&r).unwrap();

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
                if content.matches_str("file_a.txt\nfile_b.txt") == Some(true)))));
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
                if content.matches_str("file_a.txt\nfile_b.txt") == Some(true)))));
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
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("image session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            messages: vec![user, asst],
            ..Session::new("img-id", Harness::Grok)
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
    fn emit_verified_declares_openclaw_timestamp_defaults() {
        // OpenClaw retains content but supplies timestamps the source did not record.
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        // No tool/image/reasoning loss; OpenClaw keeps system turns and the title.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|line| line.contains("timestamps supplied by target") && line.contains("expected")),
            "{warnings:?}"
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
    fn emit_verified_declares_hermes_timestamp_defaults() {
        // Hermes retains content but supplies timestamps the source did not record.
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|line| line.contains("timestamps supplied by target") && line.contains("expected")),
            "{warnings:?}"
        );
    }

    /// Format-complete losslessness: a rich Claude transcript parsed under `Demand::COMPLETE`
    /// and re-emitted reproduces EVERY record with the same top-level key set and the same value
    /// per key (key order may differ). Covers a user turn, an assistant turn (text + tool_use), a
    /// user tool_result turn (with a `toolUseResult` mirror), a `system` record, and three meta
    /// records (mode / queue-operation / ai-title) carried verbatim.
    #[test]
    fn claude_complete_round_trip_is_lossless() {
        use crate::harness::claude::stream_str;
        use crate::ir::CARRIER_KEY;
        use crate::stream::{CollectSink, Demand};

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
        let session = stream_str(sid, &src_text, None, &Demand::COMPLETE, &mut sink);
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
        assert_eq!(CARRIER_KEY, "_record");

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
    }

    // --------------------------------------------------------------------------------------------
    // Verifier v2 + newly-carried-field round trips
    // --------------------------------------------------------------------------------------------

    /// The strict gate and rendering: an expected-only report is never fatal and still lists its
    /// losses; any unexpected delta trips `has_unexpected` and reads back cleanly.
    #[test]
    fn fidelity_report_gate_and_rendering() {
        let expected_only = FidelityReport {
            deltas: vec![Divergence {
                what: "block:image".into(),
                a: "1".into(),
                b: "0".into(),
                expected: true,
                at: "test".into(),
            }],
        };
        assert!(!expected_only.has_unexpected(), "expected losses never fail strict");
        assert_eq!(expected_only.lost_lines().len(), 1);
        assert!(expected_only.lost_lines()[0].contains("expected"));

        let unexpected = FidelityReport {
            deltas: vec![Divergence {
                what: "usage".into(),
                a: "1".into(),
                b: "0".into(),
                expected: false,
                at: "test".into(),
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
        let scratch = temp_dir();
        let (_r, report) = emit_report(&s, Harness::OpenCode, &scratch, &EmitOptions::default()).unwrap();
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
            message_count: None,
        };
        let parsed = Codex::detect().parse(&r).unwrap();
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
            message_count: None,
        };
        let parsed = Claude::detect().parse(&r).unwrap();
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
            message_count: None,
        };
        let parsed = OpenClaw::detect().parse(&r).unwrap();
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
        let scratch = temp_dir();
        let (res, report) = emit_report(&s, Harness::Codex, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "usage"),
            "usage must survive into Codex, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::Codex, &res, &Demand::RICH).unwrap();
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
        let scratch = temp_dir();
        let (res, report) = emit_report(&s, Harness::OpenClaw, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "usage"),
            "usage must survive into OpenClaw, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::OpenClaw, &res, &Demand::RICH).unwrap();
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
        let scratch = temp_dir();
        let (res, report) = emit_report(&s, Harness::OpenCode, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "details"),
            "details must survive into OpenCode, got {:?}",
            report.deltas
        );
        let re = reparse_emitted(Harness::OpenCode, &res, &Demand::RICH).unwrap();
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

        let scratch = temp_dir();
        let (_res, report) = emit_report(&s, Harness::Codex, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            report.deltas.iter().all(|d| d.expected),
            "no unexpected loss from a turn the target cannot represent, got {:?}",
            report.deltas
        );
        let d = report
            .deltas
            .iter()
            .find(|d| d.what == "unrepresentable_turns")
            .expect("the dropped turn is reported");
        assert_eq!((d.a.as_str(), d.b.as_str()), ("1", "0"));

        // Into Claude itself the same turn is perfectly representable, so nothing is reported.
        let scratch = temp_dir();
        let (_res, report) = emit_report(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "unrepresentable_turns"),
            "Claude holds signature-only thinking, got {:?}",
            report.deltas
        );
    }

    /// The capability table for a TOOL turn's own id/timestamp. OpenCode and Gemini store a tool
    /// result inside its originating call's record, so the turn has no record to carry them; every
    /// other target writes the result as its own record and keeps them. The narrow key is the whole
    /// point: `id`/`timestamp` on a NORMAL turn must keep answering for themselves.
    #[test]
    fn tool_turn_id_is_expected_only_where_the_store_folds_the_result() {
        for t in [Harness::OpenCode, Harness::Gemini] {
            assert!(folds_tool_result(t), "{t} folds a result into its call's record");
            assert!(
                !target_holds(t, Field::IdToolTurn),
                "{t}: a tool turn has no id of its own"
            );
            assert!(
                !target_holds(t, Field::TimestampToolTurn),
                "{t}: a tool turn has no timestamp of its own"
            );
            // …and that must NOT have excused the ordinary fields.
            assert!(target_holds(t, Field::Id), "{t} still holds a normal turn's id");
            assert!(
                target_holds(t, Field::Timestamp),
                "{t} still holds a normal turn's timestamp"
            );
        }
        // A target that gives the result its own record answers exactly as it does for any turn.
        for t in [Harness::Claude, Harness::OpenClaw, Harness::Qwen] {
            assert!(!folds_tool_result(t));
            assert!(target_holds(t, Field::IdToolTurn), "{t} keeps a tool turn's id");
            assert!(
                target_holds(t, Field::TimestampToolTurn),
                "{t} keeps a tool turn's timestamp"
            );
        }
        // Codex has no per-message id at all, so a tool turn's is expected there for the older
        // reason — the narrow key never *adds* a capability.
        assert!(!target_holds(Harness::Codex, Field::Id));
        assert!(!target_holds(Harness::Codex, Field::IdToolTurn));
        assert!(target_holds(Harness::Codex, Field::TimestampToolTurn));
    }

    /// Both directions of the new classification, through the real diff: a folding target losing
    /// every TOOL turn's id/timestamp is an expected loss and passes `--strict`; the same target
    /// losing a NORMAL turn's id is still unexpected and still fails it.
    #[test]
    fn a_folded_tool_turns_lost_id_is_expected_a_normal_turns_is_not() {
        let mut src = sample_session(Harness::Claude);
        for (i, m) in src.messages.iter_mut().enumerate() {
            m.id = Some(format!("m{i}"));
            m.timestamp = Some(Utc::now());
        }

        // What a folding store reads back: every turn keeps its id and timestamp except the tool
        // turn, which the reader rebuilt out of the call and has nowhere to read them from.
        let mut folded = src.clone();
        for m in &mut folded.messages {
            if m.role == Role::Tool {
                m.id = None;
                m.timestamp = None;
            }
        }
        let report = diff_fidelity(Harness::OpenCode, ThinkingMode::Native, &src, &folded);
        let d = report
            .deltas
            .iter()
            .find(|d| d.what == "message ids (tool turns)")
            .expect("the tool turn's lost id is reported");
        assert_eq!((d.a.as_str(), d.b.as_str()), ("1", "0"));
        assert!(d.expected, "OpenCode cannot give a tool turn an id");
        assert!(
            report.deltas.iter().any(|d| d.what == "timestamps (tool turns)"),
            "and its timestamp too: {:?}",
            report.deltas
        );
        assert!(!report.has_unexpected(), "so --strict passes: {:?}", report.deltas);

        // The same target losing an ASSISTANT turn's id is a loss it could have avoided.
        let mut normal = src.clone();
        for m in &mut normal.messages {
            if m.role == Role::Assistant {
                m.id = None;
            }
        }
        let report = diff_fidelity(Harness::OpenCode, ThinkingMode::Native, &src, &normal);
        let d = report
            .deltas
            .iter()
            .find(|d| d.what == "message ids")
            .expect("a normal turn's lost id is reported");
        assert!(!d.expected, "a normal turn's id loss stays unexpected");
        assert!(report.has_unexpected(), "and --strict still fails on it");
    }

    /// A session that opens mid-call — a resumed or `prune`d transcript whose first tool result has
    /// no `tool_use` anywhere in it — keeps that result when ported into a store that folds results
    /// into their call's record. Both emitters used to index results by call id and silently drop
    /// the ones that matched nothing, which cost a real 1639-message port one tool result, its
    /// `details` and its whole Tool turn.
    fn session_with_an_orphan_result(harness: Harness) -> Session {
        let mut s = sample_session(harness);
        let mut orphan = Message::new(Role::Tool);
        orphan.timestamp = Some(Utc::now());
        orphan.content.push(Block::ToolResult {
            tool_use_id: "call_from_before_the_prune".into(),
            content: "the output of a call this transcript does not contain".into(),
            is_error: false,
            tool_name: Some("run_shell".into()),
            status: None,
            details: Some(serde_json::json!({ "title": "ls" })),
        });
        s.messages.insert(0, orphan);
        s
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn opencode_keeps_a_tool_result_whose_call_is_not_in_the_session() {
        let s = session_with_an_orphan_result(Harness::OpenCode);
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

        let orphan = parsed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult {
                    tool_use_id, content, ..
                } if tool_use_id == "call_from_before_the_prune" => Some(content.to_string()),
                _ => None,
            })
            .expect("the orphan result survives the port");
        assert!(orphan.contains("does not contain"));
        assert_eq!(
            parsed.messages.iter().filter(|m| m.role == Role::Tool).count(),
            2,
            "both tool turns are there — the paired one and the orphan"
        );

        let scratch = temp_dir();
        let (_r, report) = emit_report(&s, Harness::OpenCode, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.has_unexpected(),
            "opencode orphan round-trip: {:?}",
            report.deltas
        );
    }

    /// Codex pairs a `function_call_output` with the `function_call` above it and logs "Orphan
    /// function call output for call id" for one it cannot pair — which every ported pruned session
    /// produced, because a pruned transcript can open on a result whose call was snipped away.
    /// Emitting a call shell first keeps the pair well-formed AND the output's text in the thread.
    #[test]
    fn codex_pairs_an_orphan_tool_output_with_a_call_shell() {
        let s = session_with_an_orphan_result(Harness::Codex);
        let dir = temp_dir();
        let res = emit(&s, Harness::Codex, &dir, &EmitOptions::default()).unwrap();
        let text = fs::read_to_string(&res.path).unwrap();
        let items: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        let ids = |ty: &str| -> Vec<String> {
            items
                .iter()
                .filter(|i| i["payload"]["type"] == ty)
                .filter_map(|i| i["payload"]["call_id"].as_str().map(str::to_string))
                .collect()
        };
        let calls = ids("function_call");
        let outs = ids("function_call_output");
        for o in &outs {
            assert!(calls.contains(o), "every output is paired; {o} is orphaned:\n{text}");
        }
        assert!(
            calls.contains(&"call_from_before_the_prune".to_string()),
            "the orphan got a call shell:\n{text}"
        );
        // And the shell sits BEFORE its output, which is what Codex's pairing relies on.
        let pos = |ty: &str| {
            items
                .iter()
                .position(|i| i["payload"]["type"] == ty && i["payload"]["call_id"] == "call_from_before_the_prune")
        };
        assert!(pos("function_call") < pos("function_call_output"), "{text}");
    }

    #[test]
    fn gemini_keeps_a_tool_result_whose_call_is_not_in_the_session() {
        let s = session_with_an_orphan_result(Harness::Gemini);
        let dir = temp_dir();
        let res = emit(&s, Harness::Gemini, &dir, &EmitOptions::default()).unwrap();
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Gemini,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: None,
        };
        let parsed = Gemini::detect().parse(&r).unwrap();

        let orphan = parsed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                Block::ToolResult {
                    tool_use_id, content, ..
                } if tool_use_id == "call_from_before_the_prune" => Some(content.to_string()),
                _ => None,
            })
            .expect("the orphan result survives the port");
        assert!(orphan.contains("does not contain"));
        assert_eq!(
            parsed.messages.iter().filter(|m| m.role == Role::Tool).count(),
            2,
            "both tool turns are there — the paired one and the orphan"
        );
    }

    /// The `⚠ lost` rendering never dumps a whole value: a real session's system prompt is
    /// thousands of lines, and printing it buried every other delta in scrollback. The struct keeps
    /// the full value for `--json`.
    #[test]
    fn delta_describe_briefs_long_values() {
        let d = Divergence {
            what: "system prompt".into(),
            a: format!("\n\nYou are an interactive agent{}\nand more\n", "…".repeat(4000)),
            b: "(none)".into(),
            expected: true,
            at: "test".into(),
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
        assert!(d.a.len() > 1000, "the delta itself keeps the full value");
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
        let mut s = stream_str(sid, &text, None, &Demand::COMPLETE, &mut sink);
        s.messages = sink.messages;
        assert!(
            s.messages.iter().any(Message::is_carrier),
            "the `mode` record is carried"
        );

        let scratch = temp_dir();
        let (_res, report) = emit_report(
            &s,
            Harness::Claude,
            &scratch,
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
        let scratch = temp_dir();
        let (res, report) = emit_report(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "details"),
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
        let scratch = temp_dir();
        let (res, _report) = emit_report(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
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

    /// Every emitter claims its session files: a pinned id emitted twice is refused with nothing
    /// written over the first (a symlink or hard link there included); `force` replaces by rename.
    #[test]
    fn a_taken_session_name_is_refused_unless_forced_for_every_emitter() {
        for &target in supported_targets() {
            if matches!(target, Harness::Hermes | Harness::OpenCode) {
                continue; // SQLite rows, not staged files
            }
            // A file, or a session directory of files (Grok, Cline, Roo).
            let snapshot = |path: &Path| -> Vec<(PathBuf, Vec<u8>)> {
                if path.is_file() {
                    return vec![(path.to_owned(), std::fs::read(path).unwrap())];
                }
                let mut files: Vec<_> = std::fs::read_dir(path)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .filter(|p| p.is_file())
                    .map(|p| {
                        let bytes = std::fs::read(&p).unwrap();
                        (p, bytes)
                    })
                    .collect();
                files.sort();
                files
            };
            let out = temp_dir();
            let pinned = EmitOptions {
                new_id: Some("pinned-session".into()),
                ..Default::default()
            };
            let session = sample_session(Harness::Claude);
            let first = emit(&session, target, &out, &pinned).unwrap();
            let before = snapshot(&first.path);
            let error = emit(&session, target, &out, &pinned).unwrap_err();
            assert!(
                error.downcast_ref::<crate::publish::Exists>().is_some(),
                "{target}: {error:#}"
            );
            assert_eq!(snapshot(&first.path), before, "{target}");
            let forced = EmitOptions { force: true, ..pinned };
            assert_eq!(
                emit(&session, target, &out, &forced).unwrap().path,
                first.path,
                "{target}"
            );
        }
    }

    /// `--strict` fails on an *unexpected* loss — one the target format could have carried. The
    /// subject has had to move twice, because each previous one got fixed: tool-result `details`
    /// (now round-trips) and then per-message `usage` (now written for every source harness, see
    /// [`claude_carries_usage_from_a_foreign_source`]). It is now the **text of a foreign System
    /// turn**: Claude's transcript has no standalone system record, so `emit_claude` drops the turn
    /// — but its text is not inherently unportable there (a foreign `InjectedContext` turn is
    /// carried into an `attachment` record a few lines away), so `target_holds(Claude, Field::Text)` is
    /// rightly true and the loss is rightly unexpected. If that gap is ever closed, point this test
    /// at another real one rather than weakening the check.
    #[test]
    fn strict_fails_on_unexpected_loss() {
        let s = sample_session(Harness::OpenCode); // foreign source → Claude cross-harness path
        assert!(
            s.messages.iter().any(|m| m.role == Role::System && m.text().is_some()),
            "the sample carries a System turn with text"
        );
        // Sanity: the report flags an unexpected loss of that text.
        let scratch = temp_dir();
        let (_r, report) = emit_report(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            report
                .deltas
                .iter()
                .any(|d| d.what == "block:text" && !d.expected && d.a == "3" && d.b == "2"),
            "expected an unexpected text loss, got {:?}",
            report.deltas
        );
        // Non-strict: succeeds, lists the loss.
        let scratch = temp_dir();
        let (_res, warnings) = emit_verified(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
        assert!(warnings.iter().any(|w| w.contains("block:text")));
        // Strict: errors.
        let scratch = temp_dir();
        let err = emit_verified(
            &s,
            Harness::Claude,
            &scratch,
            &EmitOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("strict"), "got: {err:#}");
    }

    /// Per-turn token usage survives a port into Claude **from another harness**. Claude keeps the
    /// counts at `message.usage` and `target_holds(Claude, Field::Usage)` says so, but `emit_claude`
    /// rebuilt that block only when replaying a Claude session from its own fact bag — so a real
    /// Codex session ported here read back `usage: 2032 → 0`, an unexpected loss on every port.
    #[test]
    fn claude_carries_usage_from_a_foreign_source() {
        for src in [Harness::Codex, Harness::OpenCode] {
            let mut s = sample_session(src);
            for m in &mut s.messages {
                if m.role == Role::Assistant {
                    m.usage = Some(crate::ir::Usage {
                        input_tokens: Some(12),
                        output_tokens: Some(34),
                        cache_read_tokens: Some(5),
                        cache_creation_tokens: Some(6),
                        reasoning_tokens: Some(7),
                        cost_usd: Some(0.5),
                    });
                }
            }
            let scratch = temp_dir();
            let (res, report) = emit_report(&s, Harness::Claude, &scratch, &EmitOptions::default()).unwrap();
            assert!(
                !report.deltas.iter().any(|d| d.what == "usage"),
                "usage must survive {src} → Claude, got {:?}",
                report.deltas
            );
            // …and it is really Claude's own `message.usage` shape, read back by Claude's parser.
            let re = reparse_emitted(Harness::Claude, &res, &Demand::RICH).unwrap();
            let u = re
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .find_map(|m| m.usage.as_ref())
                .unwrap_or_else(|| panic!("usage on the re-parsed Claude session ({src})"));
            assert_eq!(
                (
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_tokens,
                    u.cache_creation_tokens
                ),
                (Some(12), Some(34), Some(5), Some(6))
            );
            // The raw record carries the Anthropic spelling of the cache fields.
            let raw = fs::read_to_string(&res.path).unwrap();
            assert!(
                raw.contains("\"cache_read_input_tokens\":5") && raw.contains("\"cache_creation_input_tokens\":6"),
                "Anthropic key spellings in the emitted record"
            );
        }
    }

    /// Hermes cannot carry a tool result's error flag, so the loss is expected, not a `--strict`
    /// failure. Evidence (schema v30, `hermes_state_common.py` SCHEMA_SQL): the `messages` table
    /// has no error/status column — `effect_disposition` is the side-effect classification and
    /// `finish_reason` is the assistant stop reason — and Hermes's own writers (`append_message`,
    /// `make_tool_result_message`) take no error argument, so a failure reaches the store only as
    /// the text of `content`. Its reader therefore builds every tool block with `is_error: false`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_tool_result_is_error_is_an_expected_loss() {
        let mut s = sample_session(Harness::Claude);
        for m in &mut s.messages {
            for b in &mut m.content {
                if let Block::ToolResult { is_error, .. } = b {
                    *is_error = true;
                }
            }
        }
        let scratch = temp_dir();
        let (_res, report) = emit_report(&s, Harness::Hermes, &scratch, &EmitOptions::default()).unwrap();
        let d = report
            .deltas
            .iter()
            .find(|d| d.what == "tool result is_error")
            .expect("the flag is reported as lost");
        assert!(d.expected, "Hermes has no column for it: {d:?}");
        // …so `--strict` does not fail on it.
        let scratch = temp_dir();
        emit_verified(
            &s,
            Harness::Hermes,
            &scratch,
            &EmitOptions {
                strict: true,
                ..Default::default()
            },
        )
        .expect("an expected loss never fails --strict");
        // A target that *does* have a slot still has to fill it.
        let scratch = temp_dir();
        let (_res, report) = emit_report(&s, Harness::Grok, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "tool result is_error"),
            "Grok carries it in updates.jsonl, got {:?}",
            report.deltas
        );
    }

    /// Grok *pins* the model it will resume with: `summary.json`'s `current_model_id` is what the
    /// backend is asked for, so a foreign id would replay the history against a model that cannot
    /// serve it. Substituting `grok-build` is the correct behaviour, and the verifier says so —
    /// reported, but as an expected substitution, never a `--strict` failure. A model Grok can run
    /// is still carried verbatim, and losing THAT would still be unexpected.
    #[test]
    fn grok_pins_a_foreign_session_model() {
        let mut s = sample_session(Harness::Claude);
        s.model = Some("claude-fable-5-1".into());
        let scratch = temp_dir();
        let (res, report) = emit_report(&s, Harness::Grok, &scratch, &EmitOptions::default()).unwrap();
        let d = report
            .deltas
            .iter()
            .find(|d| d.what.starts_with("session model"))
            .expect("the substitution is reported");
        assert_eq!(d.what, "session model (foreign)");
        assert_eq!((d.a.as_str(), d.b.as_str()), ("claude-fable-5-1", "grok-build"));
        assert!(d.expected, "a pinned model is not an avoidable loss: {d:?}");
        let summary: Value = serde_json::from_str(&fs::read_to_string(res.path.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["current_model_id"], json!("grok-build"));

        // A Grok model is carried verbatim, and is checked under the plain `session_model` key.
        let mut g = sample_session(Harness::Grok);
        g.model = Some("grok-code-fast-1".into());
        for m in &mut g.messages {
            m.model = g.model.clone();
        }
        let scratch = temp_dir();
        let (res, report) = emit_report(&g, Harness::Grok, &scratch, &EmitOptions::default()).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what.starts_with("session model")),
            "a Grok model round-trips, got {:?}",
            report.deltas
        );
        let summary: Value = serde_json::from_str(&fs::read_to_string(res.path.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["current_model_id"], json!("grok-code-fast-1"));
    }

    #[test]
    fn replay_default_is_native_only_for_all_claude_models() {
        use ThinkingMode::{Native, Portable};
        assert_eq!(
            ThinkingMode::replay_default(["claude-opus-5", "<synthetic>", "claude-fable-5-1"]),
            Native
        );
        assert_eq!(ThinkingMode::replay_default([]), Native);
        assert_eq!(
            ThinkingMode::replay_default(["claude-opus-5", "grok-4.7-build"]),
            Portable
        );
        assert_eq!(ThinkingMode::replay_default(["glm-5.3-flash"]), Portable);
    }

    /// The wire spellings the CLI parses (`--thinking <mode>`), and the default.
    #[test]
    fn thinking_mode_parses_from_a_string() {
        assert_eq!(ThinkingMode::default(), ThinkingMode::Native);
        assert_eq!(EmitOptions::default().thinking, ThinkingMode::Native);
        for m in ThinkingMode::ALL {
            assert_eq!(ThinkingMode::parse(m.as_str()), Some(m));
            assert_eq!(m.as_str().parse::<ThinkingMode>().unwrap(), m);
            assert_eq!(m.to_string(), m.as_str());
        }
        assert_eq!(ThinkingMode::parse("  TEXT "), Some(ThinkingMode::Text));
        assert_eq!(ThinkingMode::parse("summarize"), None);
        let err = "summarize".parse::<ThinkingMode>().unwrap_err().to_string();
        assert!(err.contains("native, text, drop"), "the error lists the modes: {err}");
        assert_eq!(
            ThinkingMode::ALL.map(|m| m.as_str()),
            ["native", "text", "drop", "portable"],
            "the CLI's possible_values order"
        );
    }

    /// A signature-only thinking block against a target that cannot hold signatures (Codex holds
    /// reasoning *text*, never an Anthropic signature), through all three [`ThinkingMode`]s.
    /// Measured on a real session: 177 of 204 thinking blocks are signature-only, so this is the
    /// common case, not the corner.
    #[test]
    fn thinking_modes_against_a_target_without_signatures() {
        // An assistant turn whose ONLY content is a signature-only block, plus one that also has
        // text (so we can see the text survive independently).
        let sig_only = |sig: &str| Block::Thinking {
            text: "".into(),
            signature: Some(sig.to_string()),
            encrypted: None,
            redacted: false,
        };
        let mut base = sample_session(Harness::Claude);
        let mut think = Message::new(Role::Assistant);
        think.timestamp = Some(Utc::now());
        think.id = Some("think-1".into());
        think.content.push(sig_only("SIG-0123456789"));
        base.messages.insert(3, think);
        // The turn that already has text also carries a signature (lost either way).
        for m in &mut base.messages {
            if m.id.as_deref() == Some("think-1") {
                continue;
            }
            if let Some(Block::Thinking { signature, .. }) = m.content.first_mut() {
                *signature = Some("SIG-abcdef".into());
            }
        }
        let opts = |t: ThinkingMode| EmitOptions {
            thinking: t,
            ..Default::default()
        };
        let reparse = |res: &EmitResult| reparse_emitted(Harness::Codex, res, &Demand::RICH).unwrap();
        let thinking_blocks = |s: &Session| -> usize {
            s.messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, Block::Thinking { .. }))
                .count()
        };

        // --- native: today's behaviour. The signature-only turn is unrepresentable and vanishes.
        let scratch = temp_dir();
        let (res, report) = emit_report(&base, Harness::Codex, &scratch, &opts(ThinkingMode::Native)).unwrap();
        assert!(report.deltas.iter().all(|d| d.expected), "{:?}", report.deltas);
        assert!(
            report.deltas.iter().any(|d| d.what == "unrepresentable_turns"),
            "the sig-only turn is dropped, got {:?}",
            report.deltas
        );
        let re = reparse(&res);
        assert!(
            !re.messages
                .iter()
                .any(|m| m.id.as_deref() == Some("think-1") || m.text().is_some_and(|t| t.contains("signature-only"))),
            "native never invents a placeholder"
        );

        // --- text: the turn survives, and the lost signature is still reported.
        let scratch = temp_dir();
        let (res, report) = emit_report(&base, Harness::Codex, &scratch, &opts(ThinkingMode::Text)).unwrap();
        assert!(
            !report.deltas.iter().any(|d| d.what == "unrepresentable_turns"),
            "no turn is lost under --thinking text, got {:?}",
            report.deltas
        );
        let sig = report
            .deltas
            .iter()
            .find(|d| d.what == "thinking(signature)")
            .expect("the signature loss is STILL reported — this mode saves the turn, not the blob");
        assert!(sig.expected, "Codex cannot hold an Anthropic signature: {sig:?}");
        assert!(report.deltas.iter().all(|d| d.expected), "{:?}", report.deltas);
        let re = reparse(&res);
        let placeholder = re
            .messages
            .iter()
            .filter_map(|m| m.text())
            .find(|t| t.contains("signature-only"))
            .expect("the sig-only turn came through as a named placeholder");
        assert!(
            placeholder.contains("14-byte") && placeholder.contains("thinking"),
            "the placeholder names what it was and how big: {placeholder}"
        );
        // The turn with real reasoning text keeps it as reasoning (Codex holds thinking text).
        assert_eq!(thinking_blocks(&re), 1, "text-bearing thinking stays structural");
        assert!(
            re.messages
                .iter()
                .any(|m| m.text().is_some_and(|t| t.contains("I should run ls"))
                    || m.content
                        .iter()
                        .any(|b| matches!(b, Block::Thinking { text, .. } if text.contains("I should run ls")))),
            "the reasoning text survives"
        );

        // --- drop: no thinking at all, even into a target that could hold every payload.
        for target in [Harness::Codex, Harness::Claude] {
            let scratch = temp_dir();
            let (res, report) = emit_report(&base, target, &scratch, &opts(ThinkingMode::Drop)).unwrap();
            let re = reparse_emitted(target, &res, &Demand::RICH).unwrap();
            assert_eq!(thinking_blocks(&re), 0, "--thinking drop emits none into {target}");
            assert!(
                !re.messages
                    .iter()
                    .any(|m| m.text().is_some_and(|t| t.contains("I should run ls"))),
                "the reasoning text is gone from {target} too"
            );
            assert!(
                report.deltas.iter().all(|d| d.expected),
                "a requested drop is an expected loss into {target}, got {:?}",
                report.deltas
            );
            assert!(
                report.deltas.iter().any(|d| d.what.starts_with("thinking(")),
                "…and it is still REPORTED into {target}, got {:?}",
                report.deltas
            );
        }
    }
    #[test]
    fn accumulated_fidelity_matches_materialized_reports() {
        let world = cv_testkit::World::new("fidelity-accumulator");
        let targets = vec![
            Harness::Claude,
            Harness::Codex,
            Harness::Grok,
            Harness::OpenClaw,
            Harness::Gemini,
            Harness::Qwen,
        ];
        #[cfg(feature = "sqlite")]
        let targets = targets.into_iter().chain([Harness::OpenCode, Harness::Hermes]);
        for target in targets {
            for thinking in [ThinkingMode::Native, ThinkingMode::Text, ThinkingMode::Drop] {
                let mut source = sample_session(Harness::Claude);
                source.system_prompt = Some("system instructions".into());
                source.lineage.forked_from = Some("previous-session".into());
                let mut command = Message::new(Role::User);
                command.content = vec![
                    Block::Text { text: "  ".into() },
                    Block::Text {
                        text: " /compact".into(),
                    },
                ];
                source.messages.push(command);
                let mut orphan = Message::new(Role::Assistant);
                orphan.content.push(Block::ToolUse {
                    id: "orphan".into(),
                    name: "unanswered".into(),
                    input: json!({"ignored": "body"}).into(),
                    namespace: None,
                });
                source.messages.push(orphan);
                let mut signed = Message::new(Role::Assistant);
                signed.content.push(Block::Thinking {
                    text: "  ".into(),
                    signature: Some("signature".into()),
                    encrypted: None,
                    redacted: true,
                });
                source.messages.push(signed);
                let opts = EmitOptions {
                    thinking,
                    // Let each native target choose its own valid session identifier.
                    ..Default::default()
                };
                let (result, actual) =
                    emit_report(&source, target, &world.base.join(format!("{target}-{thinking}")), &opts).unwrap();
                let reparsed = reparse_emitted(target, &result, &Demand::RICH).unwrap();
                let expected = diff_fidelity(target, thinking, &source, &reparsed);
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{target}/{thinking}"
                );
            }
        }
    }
    #[test]
    fn claude_portable_projects_raw_extra_and_carrier_content() -> Result<()> {
        let world = crate::test_world("claude-portable-overrides");
        let mut session = Session::new("raw", Harness::Claude);
        let mut call = Message::new(Role::Assistant);
        call.id = Some("call".into());
        call.content.push(Block::Text {
            text: "IR projection".into(),
        });
        call.harness_extra_mut(Harness::Claude).insert(
            "message.content".into(),
            serde_json::json!([
                {"type":"thinking","thinking":"raw reason","signature":"foreign"},
                {"type":"tool_use","id":"a:b","name":"Read","input":{"untouched":"a:b"}},
                {"type":"tool_use","id":"cv_tool_0","name":"Read","input":{}},
                {"type":"tool_use","id":"orphan:drop","name":"Read","input":{}}
            ]),
        );
        session.messages.push(call);
        let mut result = Message::new(Role::System);
        result.extra.insert(
            crate::ir::CARRIER_KEY.into(),
            serde_json::json!({
                "type":"user","uuid":"result","parentUuid":"call","message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"a:b","content":"raw output"},
                    {"type":"tool_result","tool_use_id":"cv_tool_0","content":"ok"},
                    {"type":"redacted_thinking","data":"opaque"}
                ]}
            }),
        );
        session.messages.push(result);
        let result = emit(
            &session,
            Harness::Claude,
            &world.base.join("out"),
            &EmitOptions {
                thinking: ThinkingMode::Portable,
                ..Default::default()
            },
        )?;
        verify_claude_resumable(&result.path, None)?;
        let rows: Vec<Value> = fs::read_to_string(result.path)?
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let call = &rows[0]["message"]["content"];
        let result = &rows[1]["message"]["content"];
        assert_eq!(call[0], serde_json::json!({"type":"text","text":"raw reason"}));
        assert_eq!(
            call.as_array().unwrap().len(),
            3,
            "orphan is still removed before portable projection"
        );
        assert_eq!(call[1]["id"], "cv_tool_1");
        assert_eq!(call[2]["id"], "cv_tool_0");
        assert_eq!(call[1]["id"], result[0]["tool_use_id"]);
        assert_eq!(call[1]["input"]["untouched"], "a:b");
        assert_eq!(
            result.as_array().unwrap().len(),
            2,
            "redacted reasoning is removed, not replaced"
        );
        Ok(())
    }
    #[test]
    fn claude_drop_filters_raw_reasoning_before_survival_and_parent_relinking() -> Result<()> {
        let world = crate::test_world("claude-drop-overrides");
        let mut session = Session::new("raw-drop", Harness::Claude);
        for (id, parent, raw) in [
            (
                "mixed",
                None,
                serde_json::json!([
                    {"type":"thinking","thinking":"never emit this","signature":"foreign"},
                    {"type":"tool_use","id":"a:b","name":"Read","input":{}}
                ]),
            ),
            (
                "thinking-only",
                Some("mixed"),
                serde_json::json!([
                    {"type":"redacted_thinking","data":"opaque"},
                    {"type":"thinking","thinking":"never emit this either","signature":"foreign"}
                ]),
            ),
        ] {
            let mut message = Message::new(Role::Assistant);
            message.id = Some(id.into());
            message.parent_id = parent.map(str::to_owned);
            message
                .harness_extra_mut(Harness::Claude)
                .insert("message.content".into(), raw);
            session.messages.push(message);
        }
        let mut result = Message::new(Role::System);
        result.extra.insert(
            crate::ir::CARRIER_KEY.into(),
            serde_json::json!({
                "type":"user","uuid":"result","parentUuid":"thinking-only","message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"a:b","content":"kept"},
                    {"type":"thinking","thinking":"hidden in carrier","signature":"foreign"}
                ]}
            }),
        );
        session.messages.push(result);
        let result = emit(
            &session,
            Harness::Claude,
            &world.base.join("out"),
            &EmitOptions {
                thinking: ThinkingMode::Drop,
                ..Default::default()
            },
        )?;
        verify_claude_resumable(&result.path, None)?;
        let output = fs::read_to_string(result.path)?;
        assert!(!output.contains("never emit") && !output.contains("hidden in carrier") && !output.contains("foreign"));
        let rows: Vec<Value> = output.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(
            rows.len(),
            2,
            "raw thinking-only assistant is dropped before rethreading"
        );
        assert_eq!(rows[1]["parentUuid"], "mixed");
        assert_eq!(
            rows[0]["message"]["content"][0]["id"],
            rows[1]["message"]["content"][0]["tool_use_id"]
        );
        assert_eq!(rows[1]["message"]["content"].as_array().unwrap().len(), 1);
        Ok(())
    }
}
